//! Snapshot-only Linux callback regressions. These drive fuser over a Unix
//! socketpair, so they exercise real FUSE request/reply encoding without a
//! kernel mount or /dev/fuse.

use super::*;
use crate::{
    KernelCachePolicy, LocalFile, NetworkConfiguration, ROOT_INODE, ReadDirectoryResult,
    raw::RawBuffer,
};
use std::{
    ffi::c_int,
    fs::File,
    io::{ErrorKind, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    sync::Arc,
};

const FUSE_MKNOD: u32 = 8;
const FUSE_OPEN: u32 = 14;
const FUSE_READ: u32 = 15;
const FUSE_WRITE: u32 = 16;
const FUSE_STATFS: u32 = 17;
const FUSE_RELEASE: u32 = 18;
const FUSE_FLUSH: u32 = 25;
const FUSE_INIT: u32 = 26;
const FUSE_OPENDIR: u32 = 27;
const FUSE_READDIR: u32 = 28;
const FUSE_DESTROY: u32 = 38;
const FUSE_ACCESS: u32 = 34;
const FUSE_CREATE: u32 = 35;
const FUSE_RENAME2: u32 = 45;

fn socketpair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as c_int; 2];
    // SAFETY: fds points to two writable descriptors and no other arguments
    // borrow memory from Rust.
    let result = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert_eq!(
        result,
        0,
        "socketpair failed: {}",
        io::Error::last_os_error()
    );
    // SAFETY: socketpair returned two new, owned descriptors.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn read_exact(file: &mut File, output: &mut [u8]) {
    file.read_exact(output).expect("read FUSE reply");
}

fn read_response(file: &mut File) -> Response {
    let mut header = [0_u8; 16];
    read_exact(file, &mut header);
    let len = read_u32(&header, 0) as usize;
    let error = read_i32(&header, 4);
    let unique = read_u64(&header, 8);
    assert!(len >= header.len(), "short FUSE reply: {len}");
    assert!(len <= 16 * 1024 * 1024, "unexpected FUSE reply: {len}");
    let mut data = vec![0_u8; len - header.len()];
    read_exact(file, &mut data);
    Response {
        error,
        unique,
        data,
    }
}

struct Response {
    error: i32,
    unique: u64,
    data: Vec<u8>,
}

impl Response {
    fn success(self) -> Self {
        assert_eq!(self.error, 0, "FUSE reply error: {}", self.error);
        self
    }
}

struct FusePeer {
    kernel: File,
    session: Option<fuser::BackgroundSession>,
    next_unique: u64,
}

impl Drop for FusePeer {
    fn drop(&mut self) {
        // SAFETY: kernel owns this live socket descriptor.
        unsafe {
            libc::shutdown(self.kernel.as_raw_fd(), libc::SHUT_RDWR);
        }
        if let Some(session) = self.session.take() {
            let _ = session.join();
        }
    }
}

impl FusePeer {
    fn new(adapter: FuseAdapter) -> Self {
        let (kernel, daemon) = socketpair();
        let mut kernel = File::from(kernel);
        let timeout = libc::timeval {
            tv_sec: 5,
            tv_usec: 0,
        };
        // SAFETY: the socket and timeval live for the setsockopt call.
        let result = unsafe {
            libc::setsockopt(
                kernel.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const timeout).cast(),
                libc::socklen_t::try_from(size_of_val(&timeout)).unwrap(),
            )
        };
        assert_eq!(
            result,
            0,
            "setsockopt failed: {}",
            io::Error::last_os_error()
        );

        let mut init = Vec::new();
        push_header(&mut init, FUSE_INIT, 1, 0, 1000, 1000, 4242);
        init.extend_from_slice(&7_u32.to_le_bytes()); // major
        init.extend_from_slice(&40_u32.to_le_bytes()); // minor
        init.extend_from_slice(&(128 * 1024_u32).to_le_bytes()); // max_readahead
        init.extend_from_slice(&0_u32.to_le_bytes()); // capabilities
        let init_len = u32::try_from(init.len()).unwrap();
        init[..4].copy_from_slice(&init_len.to_le_bytes());
        kernel.write_all(&init).expect("write FUSE_INIT");

        let mut config = fuser::Config::default();
        config.n_threads = Some(1);
        config.clone_fd = false;
        let session = fuser::Session::from_fd(adapter, daemon, fuser::SessionACL::All, config)
            .expect("FUSE handshake");
        let response = read_response(&mut kernel);
        assert_eq!(response.error, 0, "FUSE_INIT error: {}", response.error);
        assert_eq!(response.unique, 1);
        let session = session.spawn().expect("spawn FUSE session");
        Self {
            kernel,
            session: Some(session),
            next_unique: 2,
        }
    }

    fn request(&mut self, opcode: u32, nodeid: u64, body: &[u8]) -> Response {
        let unique = self.next_unique;
        self.next_unique += 1;
        let mut request = Vec::new();
        push_header(&mut request, opcode, unique, nodeid, 1000, 1000, 4242);
        request.extend_from_slice(body);
        let request_len = u32::try_from(request.len()).unwrap();
        request[..4].copy_from_slice(&request_len.to_le_bytes());
        self.kernel.write_all(&request).expect("write FUSE request");
        let response = read_response(&mut self.kernel);
        assert_eq!(response.unique, unique, "reply unique mismatch");
        response
    }

    fn stop(mut self) {
        let response = self.request(FUSE_DESTROY, 0, &[]).success();
        assert_eq!(response.data, Vec::<u8>::new());
        self.session
            .take()
            .expect("FUSE session")
            .join()
            .expect("join FUSE session");
    }
}

fn push_header(
    output: &mut Vec<u8>,
    opcode: u32,
    unique: u64,
    nodeid: u64,
    uid: u32,
    gid: u32,
    pid: u32,
) {
    output.extend_from_slice(&40_u32.to_le_bytes());
    output.extend_from_slice(&opcode.to_le_bytes());
    output.extend_from_slice(&unique.to_le_bytes());
    output.extend_from_slice(&nodeid.to_le_bytes());
    output.extend_from_slice(&uid.to_le_bytes());
    output.extend_from_slice(&gid.to_le_bytes());
    output.extend_from_slice(&pid.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    if let Some(last) = output.len().checked_sub(4) {
        output[last..].copy_from_slice(&0_u32.to_le_bytes());
    }
}

fn named_body(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut body = prefix.to_vec();
    body.extend_from_slice(name);
    body.push(0);
    body
}

fn open_body(flags: i32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body
}

fn create_body(flags: i32, name: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&(0o100_600_u32).to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    named_body(&body, name)
}

fn read_body(fh: u64, offset: u64, size: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&fh.to_le_bytes());
    body.extend_from_slice(&offset.to_le_bytes());
    body.extend_from_slice(&size.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body.extend_from_slice(&0_u64.to_le_bytes());
    body.extend_from_slice(&0_i32.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body
}

fn release_body(fh: u64, flags: i32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&fh.to_le_bytes());
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body.extend_from_slice(&0_u64.to_le_bytes());
    body
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

#[derive(Debug)]
struct RawRecord {
    operation: Operation,
    flags: u32,
    buffer_len: Option<usize>,
    writable: Option<bool>,
}

#[derive(Default)]
struct State {
    created: Vec<(u64, FileType, u32)>,
    opens: Vec<(u64, bool)>,
    closes: Vec<(u64, NetworkFile)>,
    renames: Vec<Option<u64>>,
    raw: Vec<RawRecord>,
    verifiers: Vec<u64>,
    configuration_reads: usize,
    destination_lookup_error: Option<Error>,
    reject_close: bool,
}

struct AuditFilesystem {
    state: Mutex<State>,
    local_path: Option<PathBuf>,
    cache_policy: KernelCachePolicy,
    raw_mask: RawEventMask,
}

impl AuditFilesystem {
    fn new(local_path: Option<PathBuf>, cache_policy: KernelCachePolicy) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            local_path,
            cache_policy,
            raw_mask: RawEventMask::NONE,
        })
    }

    fn with_raw(mut self: Arc<Self>, operation: Operation) -> Arc<Self> {
        let filesystem = Arc::get_mut(&mut self).unwrap();
        filesystem.raw_mask = filesystem
            .raw_mask
            .union(RawEventMask::operation(operation));
        self
    }
}

impl Filesystem for AuditFilesystem {
    fn statfs(&self) -> crate::Result<crate::StatFs> {
        Ok(crate::StatFs {
            block_size: 4096,
            io_size: 131_072,
            total_bytes: 8192,
            free_bytes: 4096,
            available_bytes: 4096,
            files: 4,
            free_files: 2,
        })
    }

    fn metadata(&self, inode: u64) -> crate::Result<Metadata> {
        match inode {
            ROOT_INODE => Ok(Metadata::directory(ROOT_INODE, ROOT_INODE, 0o755)),
            2..=99 => {
                let mut value = Metadata::file(inode, ROOT_INODE, 6, 0o644);
                value.generation = 7;
                Ok(value)
            }
            100.. => {
                let mut value = Metadata::file(inode, ROOT_INODE, 6, 0o600);
                value.generation = 7;
                Ok(value)
            }
            _ => Err(Error::NOT_FOUND),
        }
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> crate::Result<(u64, FileType)> {
        if parent != ROOT_INODE {
            return Err(Error::NOT_A_DIRECTORY);
        }
        match name {
            b"file" | b"src" => Ok((2, FileType::File)),
            b"dest" => {
                if let Some(error) = self.state.lock().destination_lookup_error {
                    Err(error)
                } else {
                    Err(Error::NOT_FOUND)
                }
            }
            _ => Err(Error::NOT_FOUND),
        }
    }

    fn create(&self, parent: u64, _name: &[u8], kind: FileType, mode: u32) -> crate::Result<u64> {
        if parent != ROOT_INODE {
            return Err(Error::NOT_A_DIRECTORY);
        }
        let mut state = self.state.lock();
        let inode = 100 + state.created.len() as u64;
        state.created.push((inode, kind, mode));
        Ok(inode)
    }

    fn rename(
        &self,
        _source_parent: u64,
        _source_name: &[u8],
        _destination_parent: u64,
        _destination_name: &[u8],
        replaced_inode: Option<u64>,
    ) -> crate::Result<()> {
        self.state.lock().renames.push(replaced_inode);
        Ok(())
    }

    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> crate::Result<usize> {
        if inode == ROOT_INODE {
            return Err(Error::IS_A_DIRECTORY);
        }
        let input = b"remote";
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(input.len());
        let count = output.len().min(input.len() - start);
        output[..count].copy_from_slice(&input[start..start + count]);
        Ok(count)
    }

    fn write(&self, inode: u64, _offset: u64, input: &[u8]) -> crate::Result<usize> {
        if inode == ROOT_INODE {
            return Err(Error::IS_A_DIRECTORY);
        }
        Ok(input.len())
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> crate::Result<ReadDirectoryResult> {
        if inode != ROOT_INODE {
            return Err(Error::NOT_A_DIRECTORY);
        }
        self.state.lock().verifiers.push(verifier);
        let entries: &[(&[u8], u64)] = &[(b"a", 2), (b"b", 3), (b"c", 4)];
        let start = usize::try_from(cookie).map_err(|_| Error::INVALID)?;
        let mut next = cookie;
        for (index, &(name, child)) in entries.iter().enumerate().skip(start) {
            let next_cookie = index as u64 + 1;
            if !sink.push(DirectoryEntry {
                name,
                inode: child,
                kind: FileType::File,
                next_cookie,
            }) {
                return Ok(ReadDirectoryResult {
                    verifier: 77,
                    next_cookie: next,
                    eof: false,
                });
            }
            next = next_cookie;
        }
        Ok(ReadDirectoryResult {
            verifier: 77,
            next_cookie: next,
            eof: true,
        })
    }

    fn raw_event_mask(&self) -> RawEventMask {
        self.raw_mask
    }

    fn raw_event(&self, event: RawEvent<'_>) -> RawOutcome {
        let (_file_handle, flags) = match event.native {
            NativeRequest::Linux {
                file_handle, flags, ..
            } => (file_handle, flags),
            _ => (0, 0),
        };
        self.state.lock().raw.push(RawRecord {
            operation: event.operation,
            flags,
            buffer_len: event.buffer.as_ref().map(RawBuffer::len),
            writable: event.buffer.as_ref().map(RawBuffer::is_writable),
        });
        if self.state.lock().reject_close && event.operation == Operation::Close {
            RawOutcome::Reject(Error(13))
        } else {
            RawOutcome::Continue
        }
    }
}

impl NetworkFilesystem for AuditFilesystem {
    fn network_configuration(&self) -> NetworkConfiguration {
        self.state.lock().configuration_reads += 1;
        NetworkConfiguration {
            lazy: true,
            supports_local_files: true,
            kernel_cache_policy: self.cache_policy,
        }
    }

    fn open_file(&self, inode: u64, writable: bool) -> crate::Result<NetworkFile> {
        self.state.lock().opens.push((inode, writable));
        match &self.local_path {
            Some(path) => Ok(NetworkFile::Local(LocalFile::new(path, 1))),
            None => Ok(NetworkFile::Remote),
        }
    }

    fn close_file(&self, inode: u64, file: NetworkFile) -> crate::Result<()> {
        self.state.lock().closes.push((inode, file));
        Ok(())
    }
}

fn network_peer(filesystem: Arc<AuditFilesystem>) -> FusePeer {
    FusePeer::new(FuseAdapter::new(
        Backend::Network(filesystem),
        Duration::ZERO,
    ))
}

#[test]
fn network_create_uses_materialized_local_lease_and_closes_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cached");
    std::fs::write(&path, b"cached").unwrap();
    let filesystem = AuditFilesystem::new(Some(path), KernelCachePolicy::MetadataOnly);
    let mut peer = network_peer(filesystem.clone());

    let created = peer
        .request(
            FUSE_CREATE,
            ROOT_INODE,
            &create_body(libc::O_RDONLY, b"new"),
        )
        .success();
    let inode = read_u64(&created.data, 0);
    let fh = read_u64(&created.data, 128);
    let read = peer
        .request(FUSE_READ, inode, &read_body(fh, 0, 6))
        .success();
    assert_eq!(read.data, b"cached", "create must honor the local lease");
    peer.request(FUSE_RELEASE, inode, &release_body(fh, libc::O_RDONLY))
        .success();

    let state = filesystem.state.lock();
    assert_eq!(state.opens, vec![(inode, false)]);
    assert_eq!(state.closes.len(), 1);
    assert_eq!(state.closes[0].0, inode);
    peer.stop();
}

#[test]
fn mknod_rejects_special_file_types_instead_of_creating_regular_files() {
    let filesystem = AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly);
    let mut peer = network_peer(filesystem.clone());
    let mut body = Vec::new();
    body.extend_from_slice(&(libc::S_IFIFO | 0o600).to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    let response = peer.request(FUSE_MKNOD, ROOT_INODE, &named_body(&body, b"pipe"));
    assert_eq!(
        response.error,
        -libc::ENOTSUP,
        "mknod must reject FIFO nodes"
    );
    assert_eq!(
        filesystem.state.lock().created,
        Vec::<(u64, FileType, u32)>::new()
    );
    peer.stop();
}

#[test]
fn rename_propagates_destination_lookup_errors() {
    let filesystem = AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly);
    filesystem.state.lock().destination_lookup_error = Some(Error::IO);
    let mut peer = network_peer(filesystem.clone());
    let mut body = Vec::new();
    body.extend_from_slice(&ROOT_INODE.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    let mut names = b"src\0dest\0".to_vec();
    let response = peer.request(FUSE_RENAME2, ROOT_INODE, &{
        body.append(&mut names);
        body
    });
    assert_eq!(
        response.error,
        -libc::EIO,
        "lookup failure must abort rename"
    );
    assert_eq!(filesystem.state.lock().renames, Vec::<Option<u64>>::new());
    peer.stop();
}

#[test]
fn network_cache_policy_none_enables_direct_io() {
    let filesystem = AuditFilesystem::new(None, KernelCachePolicy::None);
    let mut peer = network_peer(filesystem.clone());
    let response = peer
        .request(FUSE_OPEN, 2, &open_body(libc::O_RDONLY))
        .success();
    let open_flags = read_u32(&response.data, 8);
    assert_eq!(
        open_flags & fuser::FopenFlags::FOPEN_DIRECT_IO.bits(),
        fuser::FopenFlags::FOPEN_DIRECT_IO.bits(),
        "KernelCachePolicy::None must bypass the kernel data cache"
    );
    let created = peer
        .request(
            FUSE_CREATE,
            ROOT_INODE,
            &create_body(libc::O_RDONLY, b"new"),
        )
        .success();
    assert_eq!(
        read_u32(&created.data, 136) & fuser::FopenFlags::FOPEN_DIRECT_IO.bits(),
        fuser::FopenFlags::FOPEN_DIRECT_IO.bits(),
        "create must apply the same cache policy as open"
    );
    {
        let state = filesystem.state.lock();
        assert_eq!(state.configuration_reads, 1);
    }
    peer.stop();
}

#[test]
fn network_metadata_only_policy_enables_direct_io() {
    let filesystem = AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly);
    let mut peer = network_peer(filesystem.clone());
    let response = peer
        .request(FUSE_OPEN, 2, &open_body(libc::O_RDONLY))
        .success();
    assert_eq!(
        read_u32(&response.data, 8) & fuser::FopenFlags::FOPEN_DIRECT_IO.bits(),
        fuser::FopenFlags::FOPEN_DIRECT_IO.bits(),
        "MetadataOnly must route data reads back through the backend"
    );
    peer.stop();
}

#[test]
fn network_create_local_open_failure_releases_the_lease() {
    let directory = tempfile::tempdir().unwrap();
    let filesystem = AuditFilesystem::new(
        Some(directory.path().join("missing")),
        KernelCachePolicy::MetadataOnly,
    );
    let mut peer = network_peer(filesystem.clone());
    let response = peer.request(
        FUSE_CREATE,
        ROOT_INODE,
        &create_body(libc::O_RDONLY, b"new"),
    );
    assert_eq!(response.error, -libc::ENOENT);
    let state = filesystem.state.lock();
    assert_eq!(state.opens, vec![(100, false)]);
    assert_eq!(state.closes.len(), 1);
    drop(state);
    peer.stop();
}

#[test]
fn create_raw_event_preserves_native_open_flags() {
    let filesystem =
        AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly).with_raw(Operation::Create);
    let mut peer = network_peer(filesystem.clone());
    peer.request(
        FUSE_CREATE,
        ROOT_INODE,
        &create_body(libc::O_WRONLY, b"new"),
    )
    .success();
    let state = filesystem.state.lock();
    assert_eq!(state.raw.len(), 1);
    assert_eq!(state.raw[0].operation, Operation::Create);
    assert_eq!(state.raw[0].flags, libc::O_WRONLY as u32);
    assert_eq!(state.opens, vec![(100, true)]);
    drop(state);
    peer.stop();
}

#[test]
fn raw_close_rejection_still_releases_network_lease() {
    let filesystem =
        AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly).with_raw(Operation::Close);
    filesystem.state.lock().reject_close = true;
    let mut peer = network_peer(filesystem.clone());
    let opened = peer
        .request(FUSE_OPEN, 2, &open_body(libc::O_RDONLY))
        .success();
    let fh = read_u64(&opened.data, 0);
    let response = peer.request(FUSE_RELEASE, 2, &release_body(fh, libc::O_RDONLY));
    assert_eq!(response.error, -libc::EACCES);
    assert_eq!(filesystem.state.lock().closes.len(), 1);
    peer.stop();
}

#[test]
fn readdir_reply_preserves_entry_cookies_across_small_buffers() {
    let filesystem = AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly);
    let mut peer = network_peer(filesystem.clone());
    peer.request(FUSE_OPENDIR, ROOT_INODE, &open_body(libc::O_RDONLY))
        .success();
    let first = peer
        .request(FUSE_READDIR, ROOT_INODE, &read_body(0, 0, 40))
        .success();
    assert_eq!(first.data.len(), 32);
    assert_eq!(read_u64(&first.data, 0), 2);
    assert_eq!(read_u64(&first.data, 8), 1);
    assert_eq!(&first.data[24..25], b"a");

    let second = peer
        .request(FUSE_READDIR, ROOT_INODE, &read_body(0, 1, 128))
        .success();
    let mut offset = 0;
    let mut names = Vec::new();
    let mut cookies = Vec::new();
    while offset < second.data.len() {
        names.push(second.data[offset + 24]);
        cookies.push(read_u64(&second.data, offset + 8));
        let name_len = read_u32(&second.data, offset + 16) as usize;
        offset += (24 + name_len + 7) & !7;
    }
    assert_eq!(names, b"bc");
    assert_eq!(cookies, vec![2, 3]);
    peer.stop();
}

#[test]
fn read_and_write_raw_buffers_have_the_documented_direction() {
    let filesystem = AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly)
        .with_raw(Operation::Read)
        .with_raw(Operation::Write);
    let mut peer = network_peer(filesystem.clone());
    let opened = peer
        .request(FUSE_OPEN, 2, &open_body(libc::O_RDWR))
        .success();
    let fh = read_u64(&opened.data, 0);
    peer.request(FUSE_READ, 2, &read_body(fh, 0, 4)).success();
    let mut write = read_body(fh, 0, 3);
    write.extend_from_slice(b"abc");
    peer.request(FUSE_WRITE, 2, &write).success();
    let state = filesystem.state.lock();
    assert_eq!(state.raw[0].buffer_len, Some(4));
    assert_eq!(state.raw[0].writable, Some(true));
    assert_eq!(state.raw[1].buffer_len, Some(3));
    assert_eq!(state.raw[1].writable, Some(false));
    drop(state);
    peer.stop();
}

#[test]
fn default_flush_rejection_is_masked_by_the_kernel_protocol() {
    // This records the current callback behavior. Linux treats ENOSYS from the
    // first FUSE_FLUSH as "no flush supported" and disables later flush calls.
    let filesystem = AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly);
    let mut peer = network_peer(filesystem.clone());
    let opened = peer
        .request(FUSE_OPEN, 2, &open_body(libc::O_RDONLY))
        .success();
    let fh = read_u64(&opened.data, 0);
    let mut body = Vec::new();
    body.extend_from_slice(&fh.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body.extend_from_slice(&0_u64.to_le_bytes());
    assert_eq!(peer.request(FUSE_FLUSH, 2, &body).error, -libc::ENOSYS);
    peer.stop();
}

#[test]
fn access_callback_currently_returns_enosys() {
    // Records the behavior relevant to kernel_permissions = false. The
    // portable trait has no access method, so this is an audit candidate rather
    // than an asserted regression until the intended policy is decided.
    let filesystem = AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly);
    let mut peer = network_peer(filesystem.clone());
    let mut body = Vec::new();
    body.extend_from_slice(&libc::R_OK.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    assert_eq!(peer.request(FUSE_ACCESS, 2, &body).error, -libc::ENOSYS);
    peer.stop();
}

#[test]
fn adapter_drop_closes_outstanding_network_leases() {
    let filesystem = AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly);
    {
        let adapter = FuseAdapter::new(Backend::Network(filesystem.clone()), Duration::ZERO);
        adapter.open_files.lock().insert(
            9,
            OpenFile {
                inode: 2,
                network_file: NetworkFile::Remote,
                local: None,
            },
        );
    }
    assert_eq!(filesystem.state.lock().closes.len(), 1);
}

#[test]
fn statfs_reply_uses_fragment_units_for_all_block_counts() {
    let filesystem = AuditFilesystem::new(None, KernelCachePolicy::MetadataOnly);
    let mut peer = network_peer(filesystem.clone());
    let response = peer.request(FUSE_STATFS, ROOT_INODE, &[]).success();
    assert_eq!(read_u64(&response.data, 0), 2);
    assert_eq!(read_u64(&response.data, 8), 1);
    assert_eq!(read_u64(&response.data, 16), 1);
    assert_eq!(read_u32(&response.data, 40), 4096);
    assert_eq!(read_u32(&response.data, 48), 4096);
    peer.stop();
}

#[test]
fn socket_peer_reports_timeouts_as_test_failures_not_hangs() {
    let (kernel, _daemon) = socketpair();
    let mut kernel = File::from(kernel);
    let timeout = libc::timeval {
        tv_sec: 0,
        tv_usec: 1_000,
    };
    // SAFETY: see FusePeer::new.
    unsafe {
        libc::setsockopt(
            kernel.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const timeout).cast(),
            libc::socklen_t::try_from(size_of_val(&timeout)).unwrap(),
        );
    }
    let mut byte = [0_u8; 1];
    let error = kernel.read(&mut byte).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::WouldBlock);
}
