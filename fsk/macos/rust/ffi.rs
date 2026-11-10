//! In-process C ABI used by macOS bridge tests and compatibility clients.

use crate::{
    ABI_VERSION, DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, Mount,
    NetworkFile, NetworkFilesystem, ReadDirectoryResult, SetMetadata, StatFs,
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    ffi::c_void,
    os::unix::ffi::OsStrExt,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr, slice,
    sync::atomic::{AtomicU64, Ordering},
};

const EIO: i32 = 5;
const EINVAL: i32 = 22;
const ERANGE: i32 = 34;

/// C ABI representation of [`Metadata`] used by the macOS bridge.
///
/// This is an internal ABI type exposed for bridge tests and compatibility
/// clients. Its layout is versioned by [`ABI_VERSION`](crate::ABI_VERSION).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FskAttributes {
    /// See [`Metadata::inode`].
    pub inode: u64,
    /// See [`Metadata::parent`].
    pub parent: u64,
    /// See [`Metadata::size`].
    pub size: u64,
    /// See [`Metadata::allocated_size`].
    pub allocated_size: u64,
    /// See [`Metadata::generation`].
    pub generation: u64,
    /// See [`Metadata::created_ns`].
    pub created_ns: u64,
    /// See [`Metadata::modified_ns`].
    pub modified_ns: u64,
    /// See [`Metadata::accessed_ns`].
    pub accessed_ns: u64,
    /// See [`Metadata::mode`].
    pub mode: u32,
    /// See [`Metadata::uid`].
    pub uid: u32,
    /// See [`Metadata::gid`].
    pub gid: u32,
    /// See [`Metadata::link_count`].
    pub link_count: u32,
    /// A [`FileType`] discriminant.
    pub kind: u8,
    /// Reserved for future expansion. Must be zero.
    pub reserved: [u8; 7],
}

/// C ABI representation of [`SetMetadata`] used by the macOS bridge.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FskSetAttributes {
    /// Bit mask of the fields that are present.
    pub valid: u32,
    /// New permission bits.
    pub mode: u32,
    /// New owner user ID.
    pub uid: u32,
    /// New owner group ID.
    pub gid: u32,
    /// New logical size.
    pub size: u64,
    /// New access time in nanoseconds since the Unix epoch.
    pub accessed_ns: u64,
    /// New modification time in nanoseconds since the Unix epoch.
    pub modified_ns: u64,
}

/// C ABI representation of [`StatFs`] used by the macOS bridge.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FskStatFs {
    /// See [`StatFs::block_size`].
    pub block_size: u32,
    /// See [`StatFs::io_size`].
    pub io_size: u32,
    /// See [`StatFs::total_bytes`].
    pub total_bytes: u64,
    /// See [`StatFs::free_bytes`].
    pub free_bytes: u64,
    /// See [`StatFs::available_bytes`].
    pub available_bytes: u64,
    /// See [`StatFs::files`].
    pub files: u64,
    /// See [`StatFs::free_files`].
    pub free_files: u64,
}

/// C ABI representation of one packed [`DirectoryEntry`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FskDirEntry {
    /// Inode the entry refers to.
    pub inode: u64,
    /// Cursor at which the next entry should resume.
    pub next_cookie: u64,
    /// Offset of the name in the response's name buffer.
    pub name_offset: u32,
    /// Length of the name in bytes.
    pub name_length: u16,
    /// A [`FileType`] discriminant.
    pub kind: u8,
    /// Reserved for future expansion. Must be zero.
    pub reserved: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FskConfiguration {
    pub kind: u8,
    pub lazy: u8,
    pub supports_local_files: u8,
    pub kernel_cache_policy: u8,
    pub reserved: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FskOpenFile {
    pub lease: u64,
    pub generation: u64,
    pub path_length: usize,
    pub source: u8,
    pub reserved: [u8; 7],
}

enum Backend {
    Local(Box<dyn Filesystem>),
    Network(Box<dyn NetworkFilesystem>),
}

impl Backend {
    fn filesystem(&self) -> &dyn Filesystem {
        match self {
            Self::Local(filesystem) => filesystem.as_ref(),
            Self::Network(filesystem) => filesystem.as_ref(),
        }
    }

    fn network(&self) -> Option<&dyn NetworkFilesystem> {
        match self {
            Self::Local(_) => None,
            Self::Network(filesystem) => Some(filesystem.as_ref()),
        }
    }
}

struct Volume {
    backend: Backend,
    leases: Mutex<HashMap<u64, (u64, NetworkFile)>>,
    next_lease: AtomicU64,
}

impl Volume {
    fn new(backend: Backend) -> Self {
        Self {
            backend,
            leases: Mutex::new(HashMap::new()),
            next_lease: AtomicU64::new(1),
        }
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        let Some(network) = self.backend.network() else {
            return;
        };
        for (_, (inode, file)) in self.leases.get_mut().drain() {
            let _ = network.close_file(inode, file);
        }
    }
}

/// Creates an opaque volume handle for a local filesystem.
///
/// The returned pointer is owned by the caller and must be destroyed with
/// `fsk_volume_destroy`.
pub fn create_volume(filesystem: impl Filesystem) -> *mut c_void {
    Box::into_raw(Box::new(Volume::new(Backend::Local(Box::new(filesystem))))).cast()
}

fn create_network_volume(filesystem: impl NetworkFilesystem) -> *mut c_void {
    Box::into_raw(Box::new(Volume {
        backend: Backend::Network(Box::new(filesystem)),
        leases: Mutex::new(HashMap::new()),
        next_lease: AtomicU64::new(1),
    }))
    .cast()
}

/// Builds an exported network volume from the raw mount URL.
///
/// # Safety
///
/// `resource` must reference `resource_length` readable bytes when the length is
/// nonzero. `error_out`, when non-null, must be writable.
pub unsafe fn create_network_volume_from_raw<T>(
    resource: *const u8,
    resource_length: usize,
    error_out: *mut i32,
    factory: impl FnOnce(&Mount) -> Result<T, Error>,
) -> *mut c_void
where
    T: NetworkFilesystem,
{
    let result = catch_unwind(AssertUnwindSafe(|| {
        let resource = unsafe { bytes(resource, resource_length)? };
        factory(&Mount::new(resource.to_vec()))
    }));
    match result {
        Ok(Ok(filesystem)) => {
            if !error_out.is_null() {
                unsafe { ptr::write(error_out, 0) };
            }
            create_network_volume(filesystem)
        }
        Ok(Err(error)) => {
            if !error_out.is_null() {
                unsafe { ptr::write(error_out, error.0.max(1)) };
            }
            ptr::null_mut()
        }
        Err(_) => {
            if !error_out.is_null() {
                unsafe { ptr::write(error_out, EIO) };
            }
            ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn fsk_abi_version() -> u32 {
    ABI_VERSION
}

unsafe fn volume_handle<'a>(pointer: *mut c_void) -> Result<&'a Volume, Error> {
    if pointer.is_null() {
        return Err(Error(EINVAL));
    }
    Ok(unsafe { &*pointer.cast::<Volume>() })
}

unsafe fn volume<'a>(pointer: *mut c_void) -> Result<&'a dyn Filesystem, Error> {
    Ok(unsafe { volume_handle(pointer)? }.backend.filesystem())
}

fn status(function: impl FnOnce() -> Result<(), Error>) -> i32 {
    match catch_unwind(AssertUnwindSafe(function)) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => error.0.max(1),
        Err(_) => EIO,
    }
}

unsafe fn bytes<'a>(pointer: *const u8, length: usize) -> Result<&'a [u8], Error> {
    if length == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() {
        return Err(Error(EINVAL));
    }
    Ok(unsafe { slice::from_raw_parts(pointer, length) })
}

unsafe fn bytes_mut<'a>(pointer: *mut u8, length: usize) -> Result<&'a mut [u8], Error> {
    if length == 0 {
        return Ok(&mut []);
    }
    if pointer.is_null() {
        return Err(Error(EINVAL));
    }
    Ok(unsafe { slice::from_raw_parts_mut(pointer, length) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_volume_destroy(pointer: *mut c_void) {
    if !pointer.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            drop(unsafe { Box::from_raw(pointer.cast::<Volume>()) });
        }));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_configuration(
    pointer: *mut c_void,
    output: *mut FskConfiguration,
) -> i32 {
    status(|| {
        if output.is_null() {
            return Err(Error(EINVAL));
        }
        let volume = unsafe { volume_handle(pointer)? };
        let configuration = if let Some(network) = volume.backend.network() {
            let configuration = network.network_configuration();
            FskConfiguration {
                kind: 1,
                lazy: u8::from(configuration.lazy),
                supports_local_files: u8::from(configuration.supports_local_files),
                kernel_cache_policy: configuration.kernel_cache_policy as u8,
                reserved: [0; 4],
            }
        } else {
            FskConfiguration::default()
        };
        unsafe { ptr::write(output, configuration) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_open_file(
    pointer: *mut c_void,
    inode: u64,
    writable: u8,
    path: *mut u8,
    path_capacity: usize,
    output: *mut FskOpenFile,
) -> i32 {
    status(|| {
        if output.is_null() {
            return Err(Error(EINVAL));
        }
        let volume = unsafe { volume_handle(pointer)? };
        let Some(network) = volume.backend.network() else {
            unsafe { ptr::write(output, FskOpenFile::default()) };
            return Ok(());
        };
        let file = network.open_file(inode, writable != 0)?;
        let (source, generation, path_bytes) = match &file {
            NetworkFile::Remote => (0, 0, &[][..]),
            NetworkFile::Local(local) => {
                (1, local.generation(), local.path().as_os_str().as_bytes())
            }
        };
        if path_bytes.len() > path_capacity || (!path_bytes.is_empty() && path.is_null()) {
            let _ = network.close_file(inode, file);
            return Err(Error(ERANGE));
        }
        if !path_bytes.is_empty() {
            unsafe { ptr::copy_nonoverlapping(path_bytes.as_ptr(), path, path_bytes.len()) };
        }
        let path_length = path_bytes.len();
        let lease = volume.next_lease.fetch_add(1, Ordering::Relaxed).max(1);
        volume.leases.lock().insert(lease, (inode, file));
        unsafe {
            ptr::write(
                output,
                FskOpenFile {
                    lease,
                    generation,
                    path_length,
                    source,
                    reserved: [0; 7],
                },
            );
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_close_file(pointer: *mut c_void, lease: u64) -> i32 {
    status(|| {
        if lease == 0 {
            return Ok(());
        }
        let volume = unsafe { volume_handle(pointer)? };
        let network = volume.backend.network().ok_or(Error(EINVAL))?;
        let (inode, file) = volume.leases.lock().remove(&lease).ok_or(Error(EINVAL))?;
        network.close_file(inode, file)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_statfs(pointer: *mut c_void, output: *mut FskStatFs) -> i32 {
    status(|| {
        if output.is_null() {
            return Err(Error(EINVAL));
        }
        let value = unsafe { volume(pointer)? }.statfs()?;
        unsafe { ptr::write(output, value.into()) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_getattr(
    pointer: *mut c_void,
    inode: u64,
    output: *mut FskAttributes,
) -> i32 {
    status(|| {
        if output.is_null() {
            return Err(Error(EINVAL));
        }
        let value = unsafe { volume(pointer)? }.metadata(inode)?;
        unsafe { ptr::write(output, value.into()) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_setattr(
    pointer: *mut c_void,
    inode: u64,
    input: *const FskSetAttributes,
    output: *mut FskAttributes,
) -> i32 {
    status(|| {
        if input.is_null() || output.is_null() {
            return Err(Error(EINVAL));
        }
        let update = unsafe { (*input).into() };
        let value = unsafe { volume(pointer)? }.set_metadata(inode, update)?;
        unsafe { ptr::write(output, value.into()) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_lookup(
    pointer: *mut c_void,
    parent: u64,
    name: *const u8,
    name_length: usize,
    inode_out: *mut u64,
    kind_out: *mut u8,
) -> i32 {
    status(|| {
        if inode_out.is_null() || kind_out.is_null() {
            return Err(Error(EINVAL));
        }
        let name = unsafe { bytes(name, name_length)? };
        let (inode, kind) = unsafe { volume(pointer)? }.lookup(parent, name)?;
        unsafe {
            ptr::write(inode_out, inode);
            ptr::write(kind_out, kind as u8);
        }
        Ok(())
    })
}

struct PackedSink<'a> {
    entries: &'a mut [FskDirEntry],
    names: &'a mut [u8],
    entry_count: usize,
    name_count: usize,
    /// Set when the caller's buffers refused an entry. A page that refused its
    /// very first entry made no progress and must be reported as `ERANGE`.
    overflowed: bool,
}

impl DirectorySink for PackedSink<'_> {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
        let name_end = self.name_count.saturating_add(entry.name.len());
        let Ok(name_offset) = u32::try_from(self.name_count) else {
            self.overflowed = true;
            return false;
        };
        if self.entry_count == self.entries.len()
            || entry.name.len() > u16::MAX as usize
            || name_end > self.names.len()
        {
            self.overflowed = true;
            return false;
        }
        self.names[self.name_count..name_end].copy_from_slice(entry.name);
        self.entries[self.entry_count] = FskDirEntry {
            inode: entry.inode,
            next_cookie: entry.next_cookie,
            name_offset,
            name_length: u16::try_from(entry.name.len()).unwrap_or(u16::MAX),
            kind: entry.kind as u8,
            reserved: 0,
        };
        self.name_count = name_end;
        self.entry_count += 1;
        true
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_readdir(
    pointer: *mut c_void,
    inode: u64,
    cookie: u64,
    verifier: u64,
    entries: *mut FskDirEntry,
    entry_capacity: usize,
    names: *mut u8,
    name_capacity: usize,
    entry_count_out: *mut usize,
    name_count_out: *mut usize,
    next_cookie_out: *mut u64,
    verifier_out: *mut u64,
    eof_out: *mut u8,
) -> i32 {
    status(|| {
        if entry_count_out.is_null()
            || name_count_out.is_null()
            || next_cookie_out.is_null()
            || verifier_out.is_null()
            || eof_out.is_null()
        {
            return Err(Error(EINVAL));
        }
        let entries = if entry_capacity == 0 {
            &mut []
        } else {
            if entries.is_null() {
                return Err(Error(EINVAL));
            }
            unsafe { slice::from_raw_parts_mut(entries, entry_capacity) }
        };
        let names = unsafe { bytes_mut(names, name_capacity)? };
        let mut sink = PackedSink {
            entries,
            names,
            entry_count: 0,
            name_count: 0,
            overflowed: false,
        };
        let ReadDirectoryResult {
            verifier,
            next_cookie,
            eof,
        } = unsafe { volume(pointer)? }.read_directory(inode, cookie, verifier, &mut sink)?;
        // A page that packed nothing, is not the end, and had an entry refused
        // made no progress: the caller must retry with larger buffers instead
        // of seeing a successful empty page (and looping on the same cookie).
        if sink.entry_count == 0
            && !eof
            && (sink.overflowed || entry_capacity == 0 || name_capacity == 0)
        {
            return Err(Error(ERANGE));
        }
        unsafe {
            ptr::write(entry_count_out, sink.entry_count);
            ptr::write(name_count_out, sink.name_count);
            ptr::write(next_cookie_out, next_cookie);
            ptr::write(verifier_out, verifier);
            ptr::write(eof_out, u8::from(eof));
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_read(
    pointer: *mut c_void,
    inode: u64,
    offset: u64,
    output: *mut u8,
    length: usize,
    count_out: *mut usize,
) -> i32 {
    status(|| {
        if count_out.is_null() {
            return Err(Error(EINVAL));
        }
        let output = unsafe { bytes_mut(output, length)? };
        let count = unsafe { volume(pointer)? }
            .read(inode, offset, output)
            .and_then(|count| crate::core::checked_io_count(count, output.len()))?;
        unsafe { ptr::write(count_out, count) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_write(
    pointer: *mut c_void,
    inode: u64,
    offset: u64,
    input: *const u8,
    length: usize,
    count_out: *mut usize,
) -> i32 {
    status(|| {
        if count_out.is_null() {
            return Err(Error(EINVAL));
        }
        let input = unsafe { bytes(input, length)? };
        let count = unsafe { volume(pointer)? }
            .write(inode, offset, input)
            .and_then(|count| crate::core::checked_io_count(count, input.len()))?;
        unsafe { ptr::write(count_out, count) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_create(
    pointer: *mut c_void,
    parent: u64,
    name: *const u8,
    name_length: usize,
    kind: u8,
    mode: u32,
    inode_out: *mut u64,
) -> i32 {
    status(|| {
        if inode_out.is_null() {
            return Err(Error(EINVAL));
        }
        let name = unsafe { bytes(name, name_length)? };
        let inode =
            unsafe { volume(pointer)? }.create(parent, name, FileType::try_from(kind)?, mode)?;
        unsafe { ptr::write(inode_out, inode) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_symlink(
    pointer: *mut c_void,
    parent: u64,
    name: *const u8,
    name_length: usize,
    target: *const u8,
    target_length: usize,
    mode: u32,
    inode_out: *mut u64,
) -> i32 {
    status(|| {
        if inode_out.is_null() {
            return Err(Error(EINVAL));
        }
        let name = unsafe { bytes(name, name_length)? };
        let target = unsafe { bytes(target, target_length)? };
        let inode = unsafe { volume(pointer)? }.create_symlink(parent, name, target, mode)?;
        unsafe { ptr::write(inode_out, inode) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_readlink(
    pointer: *mut c_void,
    inode: u64,
    output: *mut u8,
    capacity: usize,
    count_out: *mut usize,
) -> i32 {
    status(|| {
        if count_out.is_null() {
            return Err(Error(EINVAL));
        }
        let output = unsafe { bytes_mut(output, capacity)? };
        let count = unsafe { volume(pointer)? }
            .read_symlink(inode, output)
            .and_then(|count| crate::core::checked_io_count(count, output.len()))?;
        unsafe { ptr::write(count_out, count) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_remove(
    pointer: *mut c_void,
    parent: u64,
    name: *const u8,
    name_length: usize,
    inode: u64,
) -> i32 {
    status(|| {
        let name = unsafe { bytes(name, name_length)? };
        unsafe { volume(pointer)? }.remove(parent, name, inode)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_rename(
    pointer: *mut c_void,
    source_parent: u64,
    source_name: *const u8,
    source_name_length: usize,
    destination_parent: u64,
    destination_name: *const u8,
    destination_name_length: usize,
    replaced_inode: u64,
) -> i32 {
    status(|| {
        let source_name = unsafe { bytes(source_name, source_name_length)? };
        let destination_name = unsafe { bytes(destination_name, destination_name_length)? };
        unsafe { volume(pointer)? }.rename(
            source_parent,
            source_name,
            destination_parent,
            destination_name,
            (replaced_inode != 0).then_some(replaced_inode),
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsk_sync(pointer: *mut c_void) -> i32 {
    status(|| unsafe { volume(pointer)? }.synchronize())
}

impl From<Metadata> for FskAttributes {
    fn from(value: Metadata) -> Self {
        Self {
            inode: value.inode,
            parent: value.parent,
            size: value.size,
            allocated_size: value.allocated_size,
            generation: value.generation,
            created_ns: value.created_ns,
            modified_ns: value.modified_ns,
            accessed_ns: value.accessed_ns,
            mode: value.mode,
            uid: value.uid,
            gid: value.gid,
            link_count: value.link_count,
            kind: value.kind as u8,
            reserved: [0; 7],
        }
    }
}

impl From<StatFs> for FskStatFs {
    fn from(value: StatFs) -> Self {
        Self {
            block_size: value.block_size,
            io_size: value.io_size,
            total_bytes: value.total_bytes,
            free_bytes: value.free_bytes,
            available_bytes: value.available_bytes,
            files: value.files,
            free_files: value.free_files,
        }
    }
}

impl From<FskSetAttributes> for SetMetadata {
    fn from(value: FskSetAttributes) -> Self {
        Self {
            size: (value.valid & 1 != 0).then_some(value.size),
            mode: (value.valid & 2 != 0).then_some(value.mode),
            uid: (value.valid & 4 != 0).then_some(value.uid),
            gid: (value.valid & 8 != 0).then_some(value.gid),
            accessed_ns: (value.valid & 16 != 0).then_some(value.accessed_ns),
            modified_ns: (value.valid & 32 != 0).then_some(value.modified_ns),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FileType, KernelCachePolicy, LocalFile, NetworkConfiguration, ROOT_INODE,
        ReadDirectoryResult, test_support::TestFilesystem,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct TestNetworkFilesystem {
        inner: TestFilesystem,
        local_path: std::path::PathBuf,
        closes: Arc<AtomicUsize>,
    }

    impl Filesystem for TestNetworkFilesystem {
        fn statfs(&self) -> crate::Result<StatFs> {
            self.inner.statfs()
        }

        fn metadata(&self, inode: u64) -> crate::Result<Metadata> {
            self.inner.metadata(inode)
        }

        fn lookup(&self, parent: u64, name: &[u8]) -> crate::Result<(u64, FileType)> {
            self.inner.lookup(parent, name)
        }

        fn read_directory(
            &self,
            inode: u64,
            cookie: u64,
            verifier: u64,
            sink: &mut dyn DirectorySink,
        ) -> crate::Result<ReadDirectoryResult> {
            self.inner.read_directory(inode, cookie, verifier, sink)
        }

        fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> crate::Result<usize> {
            self.inner.read(inode, offset, output)
        }
    }

    impl NetworkFilesystem for TestNetworkFilesystem {
        fn network_configuration(&self) -> NetworkConfiguration {
            NetworkConfiguration {
                lazy: true,
                supports_local_files: true,
                kernel_cache_policy: KernelCachePolicy::MetadataOnly,
            }
        }

        fn open_file(&self, _inode: u64, _writable: bool) -> crate::Result<NetworkFile> {
            Ok(NetworkFile::Local(LocalFile::new(
                self.local_path.clone(),
                42,
            )))
        }

        fn close_file(&self, _inode: u64, _file: NetworkFile) -> crate::Result<()> {
            self.closes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn network_configuration_and_local_file_lease_cross_the_abi() {
        let directory = tempfile::tempdir().unwrap();
        let local_path = directory.path().join("downloaded");
        std::fs::write(&local_path, b"cached").unwrap();
        let closes = Arc::new(AtomicUsize::new(0));
        let pointer = create_network_volume(TestNetworkFilesystem {
            inner: TestFilesystem::default(),
            local_path: local_path.clone(),
            closes: Arc::clone(&closes),
        });

        let mut configuration = FskConfiguration::default();
        assert_eq!(
            unsafe { fsk_configuration(pointer, &raw mut configuration) },
            0
        );
        assert_eq!(configuration.kind, 1);
        assert_eq!(configuration.lazy, 1);
        assert_eq!(
            configuration.kernel_cache_policy,
            KernelCachePolicy::MetadataOnly as u8
        );

        let mut path = [0_u8; 1024];
        let mut opened = FskOpenFile::default();
        assert_eq!(
            unsafe {
                fsk_open_file(
                    pointer,
                    ROOT_INODE,
                    0,
                    path.as_mut_ptr(),
                    path.len(),
                    &raw mut opened,
                )
            },
            0
        );
        assert_eq!(opened.source, 1);
        assert_eq!(opened.generation, 42);
        assert_eq!(
            &path[..opened.path_length],
            local_path.as_os_str().as_bytes()
        );
        assert_eq!(unsafe { fsk_close_file(pointer, opened.lease) }, 0);
        assert_eq!(closes.load(Ordering::Relaxed), 1);
        unsafe { fsk_volume_destroy(pointer) };
    }

    /// One directory page containing a single short name, ending after it.
    #[derive(Default)]
    struct OneEntryFilesystem {
        inner: TestFilesystem,
    }

    impl Filesystem for OneEntryFilesystem {
        fn statfs(&self) -> crate::Result<StatFs> {
            self.inner.statfs()
        }

        fn metadata(&self, inode: u64) -> crate::Result<Metadata> {
            self.inner.metadata(inode)
        }

        fn lookup(&self, parent: u64, name: &[u8]) -> crate::Result<(u64, FileType)> {
            self.inner.lookup(parent, name)
        }

        fn read_directory(
            &self,
            _inode: u64,
            cookie: u64,
            _verifier: u64,
            sink: &mut dyn DirectorySink,
        ) -> crate::Result<ReadDirectoryResult> {
            if cookie != 0 {
                return Ok(ReadDirectoryResult {
                    verifier: 1,
                    next_cookie: cookie,
                    eof: true,
                });
            }
            let pushed = sink.push(DirectoryEntry {
                name: b"entry",
                inode: 2,
                kind: FileType::File,
                next_cookie: 1,
            });
            Ok(ReadDirectoryResult {
                verifier: 1,
                next_cookie: u64::from(pushed),
                eof: false,
            })
        }

        fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> crate::Result<usize> {
            self.inner.read(inode, offset, output)
        }
    }

    struct ReadDirCall {
        status: i32,
        entry_count: usize,
        name_count: usize,
        next_cookie: u64,
        eof: u8,
        names: Vec<u8>,
    }

    fn read_dir_with_buffers(entry_capacity: usize, name_capacity: usize) -> ReadDirCall {
        let pointer = create_volume(OneEntryFilesystem::default());
        let mut entries = vec![FskDirEntry::default(); entry_capacity];
        let mut names = vec![0_u8; name_capacity];
        let (mut entry_count, mut name_count) = (usize::MAX, usize::MAX);
        let (mut next_cookie, mut verifier, mut eof) = (u64::MAX, u64::MAX, u8::MAX);
        // SAFETY: every pointer is derived from a live allocation of the stated
        // length for the duration of the call.
        let status = unsafe {
            fsk_readdir(
                pointer,
                ROOT_INODE,
                0,
                1,
                entries.as_mut_ptr(),
                entries.len(),
                names.as_mut_ptr(),
                names.len(),
                &raw mut entry_count,
                &raw mut name_count,
                &raw mut next_cookie,
                &raw mut verifier,
                &raw mut eof,
            )
        };
        // SAFETY: the volume was created above and is not used again.
        unsafe { fsk_volume_destroy(pointer) };
        ReadDirCall {
            status,
            entry_count,
            name_count,
            next_cookie,
            eof,
            names,
        }
    }

    #[test]
    fn readdir_reports_erange_when_the_first_entry_cannot_fit() {
        // Non-zero but too small: the old behaviour returned a successful
        // empty page with `eof = 0`, which makes a caller retry the same
        // cookie forever instead of growing its buffers.
        let call = read_dir_with_buffers(4, 1);
        assert_eq!(call.status, ERANGE);
        assert_eq!(call.next_cookie, u64::MAX);
    }

    #[test]
    fn readdir_packs_an_entry_when_the_buffers_fit() {
        let call = read_dir_with_buffers(4, 32);
        assert_eq!(call.status, 0);
        assert_eq!(call.entry_count, 1);
        assert_eq!(call.name_count, 5);
        assert_eq!(call.next_cookie, 1);
        assert_eq!(call.eof, 0);
        assert_eq!(&call.names[..call.name_count], b"entry");
    }
}
