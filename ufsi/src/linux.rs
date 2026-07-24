//! Linux `FUSE` adapter.

use crate::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, NetworkFile,
    NetworkFilesystem, SetMetadata,
    raw::{Caller, NativeRequest, Operation, Platform, RawEvent, RawEventMask, RawOutcome},
};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FopenFlags, Generation, INodeNo, MountOption, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use parking_lot::Mutex;
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::{OsStr, c_void},
    fs::File,
    io,
    os::unix::{ffi::OsStrExt, fs::FileExt},
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

thread_local! {
    /// FUSE replies copy synchronously. Reuse one buffer per dispatcher thread
    /// instead of allocating for every read.
    static READ_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug)]
pub struct MountOptions {
    pub read_only: bool,
    pub filesystem_name: String,
    pub worker_threads: usize,
    pub kernel_permissions: bool,
    pub metadata_ttl: Duration,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            filesystem_name: "ufsi".into(),
            worker_threads: std::thread::available_parallelism().map_or(4, usize::from),
            kernel_permissions: true,
            metadata_ttl: Duration::from_secs(1),
        }
    }
}

pub struct MountSession {
    mount_path: PathBuf,
    session: Option<fuser::BackgroundSession>,
}

impl MountSession {
    /// Mount a local filesystem through the Linux kernel FUSE interface.
    ///
    /// # Errors
    ///
    /// Returns an error when the FUSE session cannot be created or mounted.
    pub fn mount(filesystem: impl Filesystem, mount_path: impl AsRef<Path>) -> io::Result<Self> {
        Self::mount_with_options(filesystem, mount_path, &MountOptions::default())
    }

    /// Mount a local filesystem with explicit Linux settings.
    ///
    /// # Errors
    ///
    /// Returns an error when the FUSE session cannot be created or mounted.
    pub fn mount_with_options(
        filesystem: impl Filesystem,
        mount_path: impl AsRef<Path>,
        options: &MountOptions,
    ) -> io::Result<Self> {
        Self::start(
            Backend::Local(Arc::new(filesystem)),
            mount_path.as_ref(),
            options,
        )
    }

    /// Mount a lazy network filesystem through FUSE.
    ///
    /// # Errors
    ///
    /// Returns an error when the FUSE session cannot be created or mounted.
    pub fn mount_network(
        filesystem: impl NetworkFilesystem,
        mount_path: impl AsRef<Path>,
    ) -> io::Result<Self> {
        Self::mount_network_with_options(filesystem, mount_path, &MountOptions::default())
    }

    /// Mount a lazy network filesystem with explicit Linux settings.
    ///
    /// # Errors
    ///
    /// Returns an error when the FUSE session cannot be created or mounted.
    pub fn mount_network_with_options(
        filesystem: impl NetworkFilesystem,
        mount_path: impl AsRef<Path>,
        options: &MountOptions,
    ) -> io::Result<Self> {
        Self::start(
            Backend::Network(Arc::new(filesystem)),
            mount_path.as_ref(),
            options,
        )
    }

    #[must_use]
    pub fn mount_path(&self) -> &Path {
        &self.mount_path
    }

    /// Unmount now. Dropping the session has the same effect.
    pub fn unmount(mut self) {
        self.session.take();
    }

    fn start(backend: Backend, mount_path: &Path, options: &MountOptions) -> io::Result<Self> {
        std::fs::create_dir_all(mount_path)?;
        let adapter = FuseAdapter::new(backend, options.metadata_ttl);
        let mut config = Config::default();
        config.n_threads = Some(options.worker_threads.max(1));
        config.clone_fd = options.worker_threads > 1;
        config
            .mount_options
            .push(MountOption::FSName(options.filesystem_name.clone()));
        config
            .mount_options
            .push(MountOption::Subtype("ufsi".into()));
        config.mount_options.push(if options.read_only {
            MountOption::RO
        } else {
            MountOption::RW
        });
        if options.kernel_permissions {
            config.mount_options.push(MountOption::DefaultPermissions);
        }
        let session = fuser::spawn_mount2(adapter, mount_path, &config)?;
        Ok(Self {
            mount_path: mount_path.to_owned(),
            session: Some(session),
        })
    }
}

enum Backend {
    Local(Arc<dyn Filesystem>),
    Network(Arc<dyn NetworkFilesystem>),
}

impl Backend {
    fn filesystem(&self) -> &dyn Filesystem {
        match self {
            Self::Local(value) => value.as_ref(),
            Self::Network(value) => value.as_ref(),
        }
    }

    fn network(&self) -> Option<&dyn NetworkFilesystem> {
        match self {
            Self::Local(_) => None,
            Self::Network(value) => Some(value.as_ref()),
        }
    }
}

struct OpenFile {
    inode: u64,
    network_file: NetworkFile,
    local: Option<File>,
}

struct FuseAdapter {
    backend: Backend,
    raw_mask: RawEventMask,
    ttl: Duration,
    open_files: Mutex<HashMap<u64, OpenFile>>,
    next_handle: AtomicU64,
}

impl FuseAdapter {
    fn new(backend: Backend, ttl: Duration) -> Self {
        let raw_mask = backend.filesystem().raw_event_mask();
        Self {
            backend,
            raw_mask,
            ttl,
            open_files: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
        }
    }

    #[inline]
    fn before(
        &self,
        request: &Request,
        operation: Operation,
        file_handle: u64,
        flags: u32,
    ) -> Result<(), Error> {
        self.before_with_buffer(request, operation, file_handle, flags, None)
    }

    #[inline]
    fn before_with_buffer(
        &self,
        request: &Request,
        operation: Operation,
        file_handle: u64,
        flags: u32,
        buffer: Option<crate::raw::RawBuffer<'_>>,
    ) -> Result<(), Error> {
        if !self.raw_mask.contains(operation) {
            return Ok(());
        }
        let pointer = NonNull::from(request).cast::<c_void>();
        let event = RawEvent {
            platform: Platform::Linux,
            operation,
            caller: Caller {
                process_id: request.pid(),
                user_id: request.uid(),
                group_id: request.gid(),
            },
            native: NativeRequest::Linux {
                request: pointer,
                unique: request.unique().0,
                file_handle,
                flags,
            },
            buffer,
        };
        match self.backend.filesystem().raw_event(event) {
            RawOutcome::Continue => Ok(()),
            RawOutcome::Reject(error) => Err(error),
        }
    }

    fn attr(&self, inode: u64) -> crate::Result<FileAttr> {
        Ok(to_fuse_attr(self.backend.filesystem().metadata(inode)?))
    }

    fn opened_local(&self, handle: u64) -> Option<File> {
        self.open_files
            .lock()
            .get(&handle)
            .and_then(|open| open.local.as_ref())
            .and_then(|file| file.try_clone().ok())
    }
}

impl Drop for FuseAdapter {
    fn drop(&mut self) {
        let Some(network) = self.backend.network() else {
            return;
        };
        for (_, open) in self.open_files.get_mut().drain() {
            let _ = network.close_file(open.inode, open.network_file);
        }
    }
}

impl fuser::Filesystem for FuseAdapter {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if let Err(error) = self.before(req, Operation::Lookup, 0, 0) {
            reply.error(errno(error));
            return;
        }
        let fs = self.backend.filesystem();
        match fs
            .lookup(parent.0, name.as_bytes())
            .and_then(|(inode, _)| self.attr(inode))
        {
            Ok(attr) => reply.entry(&self.ttl, &attr, Generation(attr.ino.0)),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        if let Err(error) = self.before(req, Operation::Metadata, fh.map_or(0, |v| v.0), 0) {
            reply.error(errno(error));
            return;
        }
        match self.attr(ino.0) {
            Ok(attr) => reply.attr(&self.ttl, &attr),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if let Err(error) = self.before(req, Operation::SetMetadata, fh.map_or(0, |v| v.0), 0) {
            reply.error(errno(error));
            return;
        }
        let update = SetMetadata {
            size,
            mode,
            uid,
            gid,
            accessed_ns: atime.map(time_or_now_ns),
            modified_ns: mtime.map(time_or_now_ns),
        };
        match self.backend.filesystem().set_metadata(ino.0, update) {
            Ok(value) => reply.attr(&self.ttl, &to_fuse_attr(value)),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn readlink(&self, req: &Request, ino: INodeNo, reply: ReplyData) {
        if let Err(error) = self.before(req, Operation::ReadLink, 0, 0) {
            reply.error(errno(error));
            return;
        }
        READ_BUFFER.with_borrow_mut(|buffer| {
            buffer.resize(64 * 1024, 0);
            match self
                .backend
                .filesystem()
                .read_symlink(ino.0, buffer)
                .and_then(|count| crate::core::checked_io_count(count, buffer.len()))
            {
                Ok(count) => reply.data(&buffer[..count]),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        self.create_entry(req, parent, name, FileType::Directory, mode, reply);
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        self.create_entry(req, parent, name, FileType::File, mode, reply);
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove_entry(req, parent, name, reply);
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove_entry(req, parent, name, reply);
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        if let Err(error) = self.before(req, Operation::Symlink, 0, 0) {
            reply.error(errno(error));
            return;
        }
        let fs = self.backend.filesystem();
        match fs
            .create_symlink(
                parent.0,
                link_name.as_bytes(),
                target.as_os_str().as_bytes(),
                0o777,
            )
            .and_then(|inode| self.attr(inode))
        {
            Ok(attr) => reply.entry(&self.ttl, &attr, Generation(attr.ino.0)),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if let Err(error) = self.before(req, Operation::Rename, 0, 0) {
            reply.error(errno(error));
            return;
        }
        let fs = self.backend.filesystem();
        let replaced = fs
            .lookup(newparent.0, newname.as_bytes())
            .ok()
            .map(|value| value.0);
        match fs.rename(
            parent.0,
            name.as_bytes(),
            newparent.0,
            newname.as_bytes(),
            replaced,
        ) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if let Err(error) = self.before(req, Operation::Open, 0, raw_flags(flags)) {
            reply.error(errno(error));
            return;
        }
        let Some(network) = self.backend.network() else {
            reply.opened(FileHandle(ino.0), FopenFlags::empty());
            return;
        };
        match network.open_file(ino.0, flags.0 & libc::O_ACCMODE != libc::O_RDONLY) {
            Ok(network_file) => {
                let local = match &network_file {
                    NetworkFile::Remote => None,
                    NetworkFile::Local(file) => match File::open(file.path()) {
                        Ok(value) => Some(value),
                        Err(error) => {
                            let _ = network.close_file(ino.0, network_file);
                            reply.error(Errno::from(error));
                            return;
                        }
                    },
                };
                let handle = self.next_handle.fetch_add(1, Ordering::Relaxed).max(1);
                self.open_files.lock().insert(
                    handle,
                    OpenFile {
                        inode: ino.0,
                        network_file,
                        local,
                    },
                );
                reply.opened(FileHandle(handle), FopenFlags::empty());
            }
            Err(error) => reply.error(errno(error)),
        }
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let local = self.opened_local(fh.0);
        READ_BUFFER.with_borrow_mut(|buffer| {
            buffer.resize(size as usize, 0);
            // SAFETY: the Vec allocation remains stable for the synchronous hook.
            let raw_buffer = unsafe {
                crate::raw::RawBuffer::new(
                    NonNull::new(buffer.as_mut_ptr()).expect("Vec pointer is non-null"),
                    buffer.len(),
                    true,
                )
            };
            if let Err(error) = self.before_with_buffer(
                req,
                Operation::Read,
                fh.0,
                raw_flags(flags),
                Some(raw_buffer),
            ) {
                reply.error(errno(error));
                return;
            }
            let result = if let Some(file) = local {
                file.read_at(buffer, offset)
                    .map_err(|error| Error(error.raw_os_error().unwrap_or(libc::EIO)))
            } else {
                self.backend
                    .filesystem()
                    .read(ino.0, offset, buffer)
                    .and_then(|count| crate::core::checked_io_count(count, buffer.len()))
            };
            match result {
                Ok(count) => reply.data(&buffer[..count]),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        // SAFETY: FUSE owns this immutable request buffer for the callback.
        let raw_buffer = unsafe {
            crate::raw::RawBuffer::new(
                NonNull::new(data.as_ptr().cast_mut()).expect("slice pointer is non-null"),
                data.len(),
                false,
            )
        };
        if let Err(error) = self.before_with_buffer(
            req,
            Operation::Write,
            fh.0,
            raw_flags(flags),
            Some(raw_buffer),
        ) {
            reply.error(errno(error));
            return;
        }
        match self
            .backend
            .filesystem()
            .write(ino.0, offset, data)
            .and_then(|count| crate::core::checked_io_count(count, data.len()))
        {
            Ok(count) => match u32::try_from(count) {
                Ok(count) => reply.written(count),
                Err(_) => reply.error(Errno::EOVERFLOW),
            },
            Err(error) => reply.error(errno(error)),
        }
    }

    fn release(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Err(error) = self.before(req, Operation::Close, fh.0, raw_flags(flags)) {
            reply.error(errno(error));
            return;
        }
        let Some(open) = self.open_files.lock().remove(&fh.0) else {
            reply.ok();
            return;
        };
        match self
            .backend
            .network()
            .expect("network handle without network backend")
            .close_file(open.inode, open.network_file)
        {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn fsync(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.sync(req, fh.0, reply);
    }

    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        if let Err(error) = self.before(req, Operation::ReadDirectory, fh.0, 0) {
            reply.error(errno(error));
            return;
        }
        let mut sink = FuseDirectorySink { reply: Some(reply) };
        let result = self
            .backend
            .filesystem()
            .read_directory(ino.0, offset, 0, &mut sink);
        let reply = sink.reply.take().expect("directory reply missing");
        match result {
            Ok(_) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn fsyncdir(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.sync(req, fh.0, reply);
    }

    fn statfs(&self, req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        if let Err(error) = self.before(req, Operation::StatFs, 0, 0) {
            reply.error(errno(error));
            return;
        }
        match self.backend.filesystem().statfs() {
            Ok(value) => {
                let block_size = u64::from(value.block_size.max(1));
                reply.statfs(
                    value.total_bytes / block_size,
                    value.free_bytes / block_size,
                    value.available_bytes / block_size,
                    value.files,
                    value.free_files,
                    value.block_size,
                    255,
                    value.io_size,
                );
            }
            Err(error) => reply.error(errno(error)),
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if let Err(error) = self.before(req, Operation::Create, 0, 0) {
            reply.error(errno(error));
            return;
        }
        match self
            .backend
            .filesystem()
            .create(parent.0, name.as_bytes(), FileType::File, mode)
            .and_then(|inode| self.attr(inode))
        {
            Ok(attr) => reply.created(
                &self.ttl,
                &attr,
                Generation(attr.ino.0),
                FileHandle(attr.ino.0),
                FopenFlags::empty(),
            ),
            Err(error) => reply.error(errno(error)),
        }
    }
}

impl FuseAdapter {
    fn create_entry(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        kind: FileType,
        mode: u32,
        reply: ReplyEntry,
    ) {
        if let Err(error) = self.before(req, Operation::Create, 0, 0) {
            reply.error(errno(error));
            return;
        }
        match self
            .backend
            .filesystem()
            .create(parent.0, name.as_bytes(), kind, mode)
            .and_then(|inode| self.attr(inode))
        {
            Ok(attr) => reply.entry(&self.ttl, &attr, Generation(attr.ino.0)),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn remove_entry(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if let Err(error) = self.before(req, Operation::Remove, 0, 0) {
            reply.error(errno(error));
            return;
        }
        let fs = self.backend.filesystem();
        match fs
            .lookup(parent.0, name.as_bytes())
            .and_then(|(inode, _)| fs.remove(parent.0, name.as_bytes(), inode))
        {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn sync(&self, req: &Request, file_handle: u64, reply: ReplyEmpty) {
        if let Err(error) = self.before(req, Operation::Synchronize, file_handle, 0) {
            reply.error(errno(error));
            return;
        }
        match self.backend.filesystem().synchronize() {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }
}

struct FuseDirectorySink {
    reply: Option<ReplyDirectory>,
}

impl DirectorySink for FuseDirectorySink {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
        let full = self.reply.as_mut().is_none_or(|reply| {
            reply.add(
                INodeNo(entry.inode),
                entry.next_cookie,
                to_fuse_kind(entry.kind),
                OsStr::from_bytes(entry.name),
            )
        });
        !full
    }
}

fn errno(error: Error) -> Errno {
    let native = match error {
        Error::NOT_SUPPORTED => libc::ENOTSUP,
        Error::DIRECTORY_NOT_EMPTY => libc::ENOTEMPTY,
        Error::STALE => libc::ESTALE,
        Error(value) => value,
    };
    Errno::from_i32(native)
}

fn to_fuse_kind(kind: FileType) -> fuser::FileType {
    match kind {
        FileType::File => fuser::FileType::RegularFile,
        FileType::Directory => fuser::FileType::Directory,
        FileType::Symlink => fuser::FileType::Symlink,
    }
}

fn to_fuse_attr(value: Metadata) -> FileAttr {
    FileAttr {
        ino: INodeNo(value.inode),
        size: value.size,
        blocks: value.allocated_size.div_ceil(512),
        atime: ns_time(value.accessed_ns),
        mtime: ns_time(value.modified_ns),
        ctime: ns_time(value.modified_ns),
        crtime: ns_time(value.created_ns),
        kind: to_fuse_kind(value.kind),
        perm: u16::try_from(value.mode & 0o7777).unwrap_or(0o7777),
        nlink: value.link_count,
        uid: value.uid,
        gid: value.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn ns_time(value: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(value)
}

fn time_or_now_ns(value: TimeOrNow) -> u64 {
    let time = match value {
        TimeOrNow::SpecificTime(value) => value,
        TimeOrNow::Now => SystemTime::now(),
    };
    u64::try_from(
        time.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
}

fn raw_flags(flags: OpenFlags) -> u32 {
    u32::from_ne_bytes(flags.0.to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_preserve_inode_size_and_kind_without_allocation() {
        let metadata = Metadata {
            inode: 42,
            parent: 1,
            size: 4097,
            allocated_size: 4096,
            generation: 2,
            created_ns: 0,
            modified_ns: 0,
            accessed_ns: 0,
            mode: 0o100_640,
            uid: 501,
            gid: 20,
            link_count: 1,
            kind: FileType::File,
        };
        let attr = to_fuse_attr(metadata);
        assert_eq!(attr.ino.0, 42);
        assert_eq!(attr.size, 4097);
        assert_eq!(attr.blocks, 8);
        assert_eq!(attr.kind, fuser::FileType::RegularFile);
        assert_eq!(attr.perm, 0o640);
    }

    #[test]
    fn portable_errors_are_translated_to_linux_errno_values() {
        assert_eq!(errno(Error::NOT_SUPPORTED).code(), libc::ENOTSUP);
        assert_eq!(errno(Error::DIRECTORY_NOT_EMPTY).code(), libc::ENOTEMPTY);
        assert_eq!(errno(Error::STALE).code(), libc::ESTALE);
        assert_eq!(errno(Error(2)).code(), libc::ENOENT);
    }
}
