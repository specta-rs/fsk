//! Authenticated local IPC service used by the `FSKit` extension.

use crate::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, NetworkFile, NetworkFilesystem,
    ReadDirectoryResult, SetMetadata,
    raw::{Caller, NativeRequest, Operation, Platform, RawEvent, RawEventMask, RawOutcome},
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub(crate) const MAGIC: u32 = 0x4653_4249;
/// Incompatible generation of the Rust <-> Swift protocol.
///
/// Peers must agree on the major version exactly. Bump it when an opcode is
/// removed, an existing payload changes meaning, or the size or meaning of a
/// non-reserved frame field changes. Reinterpreting bytes that older peers
/// were required to write as zero is a minor change, not a major one.
pub const PROTOCOL_MAJOR: u16 = 1;
/// Additive revision of the Rust <-> Swift protocol.
///
/// Minor revisions only add opcodes, trailing fields, or meaning to previously
/// reserved bytes. Peers use the lower of the two advertised minor versions, so
/// an old app keeps working with a new SDK and vice versa. Bump it whenever an
/// opcode or field is added.
pub const PROTOCOL_MINOR: u16 = 1;
pub(crate) const HEADER_LEN: usize = 24;
pub(crate) const MAX_FRAME: usize = 16 * 1024 * 1024;

pub(crate) mod opcode {
    pub const HELLO: u16 = 0;
    pub const CONFIGURATION: u16 = 1;
    pub const STATFS: u16 = 2;
    pub const GETATTR: u16 = 3;
    pub const SETATTR: u16 = 4;
    pub const LOOKUP: u16 = 5;
    pub const READDIR: u16 = 6;
    pub const READ: u16 = 7;
    pub const WRITE: u16 = 8;
    pub const CREATE: u16 = 9;
    pub const SYMLINK: u16 = 10;
    pub const READLINK: u16 = 11;
    pub const REMOVE: u16 = 12;
    pub const RENAME: u16 = 13;
    pub const SYNC: u16 = 14;
    pub const OPEN: u16 = 15;
    pub const CLOSE: u16 = 16;
}

pub(crate) enum ServiceBackend {
    Local(Arc<dyn Filesystem>),
    Network(Arc<dyn NetworkFilesystem>),
}

impl ServiceBackend {
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

pub(crate) struct Service {
    backend: ServiceBackend,
    token: [u8; 32],
    raw_mask: RawEventMask,
    leases: Mutex<HashMap<u64, (u64, NetworkFile)>>,
    next_lease: AtomicU64,
}

impl Service {
    pub(crate) fn local(filesystem: impl Filesystem, token: [u8; 32]) -> Self {
        let raw_mask = filesystem.raw_event_mask();
        Self {
            backend: ServiceBackend::Local(Arc::new(filesystem)),
            token,
            raw_mask,
            leases: Mutex::new(HashMap::new()),
            next_lease: AtomicU64::new(1),
        }
    }

    pub(crate) fn network(filesystem: impl NetworkFilesystem, token: [u8; 32]) -> Self {
        let raw_mask = filesystem.raw_event_mask();
        Self {
            backend: ServiceBackend::Network(Arc::new(filesystem)),
            token,
            raw_mask,
            leases: Mutex::new(HashMap::new()),
            next_lease: AtomicU64::new(1),
        }
    }

    pub(crate) fn serve_connection(&self, mut stream: UnixStream) -> io::Result<()> {
        stream.set_nonblocking(false)?;
        let request = Request::read(&mut stream)?;
        if request.major != PROTOCOL_MAJOR {
            write_response(&mut stream, request.id, PROTOCOL_MINOR, libc::EPROTO, &[])?;
            return Err(protocol_mismatch(request.major, request.minor));
        }
        if request.operation != opcode::HELLO || request.payload.as_slice() != self.token {
            write_response(&mut stream, request.id, PROTOCOL_MINOR, libc::EACCES, &[])?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid FSK capability",
            ));
        }
        // The app and the SDK each advertise the newest revision they
        // understand; both then restrict themselves to the lower one.
        let negotiated_minor = request.minor.min(PROTOCOL_MINOR);
        write_response(&mut stream, request.id, negotiated_minor, 0, &[])?;

        loop {
            let request = match Request::read(&mut stream) {
                Ok(value) => value,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            // `HELLO` agreed the major version; a later frame that changes it
            // is a protocol violation, not a request to dispatch.
            if request.major != PROTOCOL_MAJOR {
                write_response(&mut stream, request.id, PROTOCOL_MINOR, libc::EPROTO, &[])?;
                return Err(protocol_mismatch(request.major, request.minor));
            }
            let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.dispatch(request.id, request.operation, &request.payload)
            }))
            .unwrap_or(Err(Error::IO));
            match response {
                Ok(body) => {
                    write_response(&mut stream, request.id, negotiated_minor, 0, &body)?;
                }
                Err(error) => write_response(
                    &mut stream,
                    request.id,
                    negotiated_minor,
                    error.0.max(1),
                    &[],
                )?,
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn dispatch(&self, request_id: u64, operation: u16, payload: &[u8]) -> crate::Result<Vec<u8>> {
        let fs = self.backend.filesystem();
        if let Some(raw_operation) = raw_operation(operation)
            && self.raw_mask.contains(raw_operation)
        {
            let item_id = payload
                .get(..8)
                .and_then(|value| value.try_into().ok())
                .map_or(0, u64::from_le_bytes);
            let event = RawEvent {
                platform: Platform::MacOS,
                operation: raw_operation,
                caller: Caller::default(),
                native: NativeRequest::MacOS {
                    request_id,
                    item_id,
                    flags: 0,
                    payload,
                },
                buffer: None,
            };
            if let RawOutcome::Reject(error) = fs.raw_event(event) {
                return Err(error);
            }
        }
        let mut input = Decoder::new(payload);
        match operation {
            opcode::CONFIGURATION => {
                let mut output = Encoder::with_capacity(4);
                if let Some(network) = self.backend.network() {
                    let config = network.network_configuration();
                    output.u8(1);
                    output.u8(u8::from(config.lazy));
                    output.u8(u8::from(config.supports_local_files));
                    output.u8(config.kernel_cache_policy as u8);
                } else {
                    output.bytes(&[0; 4]);
                }
                Ok(output.finish())
            }
            opcode::STATFS => {
                let value = fs.statfs()?;
                let mut output = Encoder::with_capacity(52);
                output.u32(value.block_size);
                output.u32(value.io_size);
                output.u64(value.total_bytes);
                output.u64(value.free_bytes);
                output.u64(value.available_bytes);
                output.u64(value.files);
                output.u64(value.free_files);
                Ok(output.finish())
            }
            opcode::GETATTR => Ok(encode_metadata(fs.metadata(input.u64()?)?)),
            opcode::SETATTR => {
                let inode = input.u64()?;
                let valid = input.u32()?;
                let mode = input.u32()?;
                let uid = input.u32()?;
                let gid = input.u32()?;
                let size = input.u64()?;
                let accessed = input.u64()?;
                let modified = input.u64()?;
                Ok(encode_metadata(fs.set_metadata(
                    inode,
                    SetMetadata {
                        size: (valid & 1 != 0).then_some(size),
                        mode: (valid & 2 != 0).then_some(mode),
                        uid: (valid & 4 != 0).then_some(uid),
                        gid: (valid & 8 != 0).then_some(gid),
                        accessed_ns: (valid & 16 != 0).then_some(accessed),
                        modified_ns: (valid & 32 != 0).then_some(modified),
                    },
                )?))
            }
            opcode::LOOKUP => {
                let (inode, kind) = fs.lookup(input.u64()?, input.remaining())?;
                let mut output = Encoder::with_capacity(9);
                output.u64(inode);
                output.u8(kind as u8);
                Ok(output.finish())
            }
            opcode::READDIR => {
                let inode = input.u64()?;
                let cookie = input.u64()?;
                let verifier = input.u64()?;
                let max_entries = usize::try_from(input.u32()?).map_err(|_| Error::INVALID)?;
                let max_names = usize::try_from(input.u32()?).map_err(|_| Error::INVALID)?;
                if max_entries > 65_536 || max_names > MAX_FRAME {
                    return Err(Error::INVALID);
                }
                // The client rejects any response frame above `MAX_FRAME` as a
                // protocol violation, so never build a batch whose packed
                // records and names cannot fit in one.
                let max_names = max_names.min(readdir_name_budget(max_entries));
                let mut sink = PackedDirectorySink::new(max_entries, max_names);
                let result = fs.read_directory(inode, cookie, verifier, &mut sink)?;
                Ok(sink.finish(result))
            }
            opcode::READ => {
                let inode = input.u64()?;
                let offset = input.u64()?;
                let length = usize::try_from(input.u32()?).map_err(|_| Error::INVALID)?;
                if length > MAX_FRAME {
                    return Err(Error::INVALID);
                }
                let mut output = vec![0; length];
                let count = fs
                    .read(inode, offset, &mut output)
                    .and_then(|count| crate::core::checked_io_count(count, output.len()))?;
                output.truncate(count);
                Ok(output)
            }
            opcode::WRITE => {
                let inode = input.u64()?;
                let offset = input.u64()?;
                let input = input.remaining();
                let count = fs
                    .write(inode, offset, input)
                    .and_then(|count| crate::core::checked_io_count(count, input.len()))?;
                let mut output = Encoder::with_capacity(8);
                output.u64(u64::try_from(count).map_err(|_| Error::INVALID)?);
                Ok(output.finish())
            }
            opcode::CREATE => {
                let parent = input.u64()?;
                let kind = FileType::try_from(input.u8()?)?;
                let mode = input.u32()?;
                let inode = fs.create(parent, input.remaining(), kind, mode)?;
                Ok(inode.to_le_bytes().to_vec())
            }
            opcode::SYMLINK => {
                let parent = input.u64()?;
                let mode = input.u32()?;
                let name_len = usize::try_from(input.u32()?).map_err(|_| Error::INVALID)?;
                let name = input.take(name_len)?;
                let inode = fs.create_symlink(parent, name, input.remaining(), mode)?;
                Ok(inode.to_le_bytes().to_vec())
            }
            opcode::READLINK => {
                let inode = input.u64()?;
                let capacity = usize::try_from(input.u32()?).map_err(|_| Error::INVALID)?;
                let mut output = vec![0; capacity.min(MAX_FRAME)];
                let count = fs
                    .read_symlink(inode, &mut output)
                    .and_then(|count| crate::core::checked_io_count(count, output.len()))?;
                output.truncate(count);
                Ok(output)
            }
            opcode::REMOVE => {
                let parent = input.u64()?;
                let inode = input.u64()?;
                fs.remove(parent, input.remaining(), inode)?;
                Ok(Vec::new())
            }
            opcode::RENAME => {
                let source_parent = input.u64()?;
                let destination_parent = input.u64()?;
                let replaced = input.u64()?;
                let source_len = usize::try_from(input.u32()?).map_err(|_| Error::INVALID)?;
                let source = input.take(source_len)?;
                let destination = input.remaining();
                fs.rename(
                    source_parent,
                    source,
                    destination_parent,
                    destination,
                    (replaced != 0).then_some(replaced),
                )?;
                Ok(Vec::new())
            }
            opcode::SYNC => {
                fs.synchronize()?;
                Ok(Vec::new())
            }
            opcode::OPEN => self.open(input.u64()?, input.u8()? != 0),
            opcode::CLOSE => {
                self.close(input.u64()?)?;
                Ok(Vec::new())
            }
            _ => Err(Error::NOT_SUPPORTED),
        }
    }

    fn open(&self, inode: u64, writable: bool) -> crate::Result<Vec<u8>> {
        let Some(network) = self.backend.network() else {
            return Ok(vec![0; 17]);
        };
        let file = network.open_file(inode, writable)?;
        let lease = self.next_lease.fetch_add(1, Ordering::Relaxed).max(1);
        let mut output = Encoder::with_capacity(128);
        output.u64(lease);
        match &file {
            NetworkFile::Remote => {
                output.u8(0);
                output.u64(0);
            }
            NetworkFile::Local(local) => {
                output.u8(1);
                output.u64(local.generation());
                output.bytes(local.path().as_os_str().as_encoded_bytes());
            }
        }
        self.leases.lock().insert(lease, (inode, file));
        Ok(output.finish())
    }

    fn close(&self, lease: u64) -> crate::Result<()> {
        if lease == 0 {
            return Ok(());
        }
        let network = self.backend.network().ok_or(Error::INVALID)?;
        let (inode, file) = self.leases.lock().remove(&lease).ok_or(Error::INVALID)?;
        network.close_file(inode, file)
    }
}

fn raw_operation(operation: u16) -> Option<Operation> {
    Some(match operation {
        opcode::STATFS => Operation::StatFs,
        opcode::GETATTR => Operation::Metadata,
        opcode::SETATTR => Operation::SetMetadata,
        opcode::LOOKUP => Operation::Lookup,
        opcode::READDIR => Operation::ReadDirectory,
        opcode::READ => Operation::Read,
        opcode::WRITE => Operation::Write,
        opcode::CREATE => Operation::Create,
        opcode::SYMLINK => Operation::Symlink,
        opcode::READLINK => Operation::ReadLink,
        opcode::REMOVE => Operation::Remove,
        opcode::RENAME => Operation::Rename,
        opcode::SYNC => Operation::Synchronize,
        opcode::OPEN => Operation::Open,
        opcode::CLOSE => Operation::Close,
        _ => return None,
    })
}

impl Drop for Service {
    fn drop(&mut self) {
        let Some(network) = self.backend.network() else {
            return;
        };
        for (_, (inode, file)) in self.leases.get_mut().drain() {
            let _ = network.close_file(inode, file);
        }
    }
}

fn encode_metadata(value: crate::Metadata) -> Vec<u8> {
    let mut output = Encoder::with_capacity(96);
    output.u64(value.inode);
    output.u64(value.parent);
    output.u64(value.size);
    output.u64(value.allocated_size);
    output.u64(value.generation);
    output.u64(value.created_ns);
    output.u64(value.modified_ns);
    output.u64(value.accessed_ns);
    output.u32(value.mode);
    output.u32(value.uid);
    output.u32(value.gid);
    output.u32(value.link_count);
    output.u8(value.kind as u8);
    output.finish()
}

struct PackedDirectorySink {
    records: Vec<(u64, u64, u32, u16, u8)>,
    names: Vec<u8>,
    max_entries: usize,
    max_names: usize,
}

/// Response bytes outside the packed names: verifier (8), next cookie (8),
/// end-of-directory flag (1), entry count (4), and name count (4).
const READDIR_FIXED_LEN: usize = 25;
/// inode (8), next cookie (8), name offset (4), name length (2), kind (1).
const READDIR_ENTRY_LEN: usize = 23;

/// Largest names payload a `READDIR` response can carry for `max_entries`
/// records while staying inside [`MAX_FRAME`].
fn readdir_name_budget(max_entries: usize) -> usize {
    MAX_FRAME
        .saturating_sub(READDIR_FIXED_LEN)
        .saturating_sub(max_entries.saturating_mul(READDIR_ENTRY_LEN))
}

impl PackedDirectorySink {
    fn new(max_entries: usize, max_names: usize) -> Self {
        Self {
            records: Vec::with_capacity(max_entries.min(4096)),
            names: Vec::with_capacity(max_names.min(MAX_FRAME)),
            max_entries,
            max_names,
        }
    }

    fn finish(self, result: ReadDirectoryResult) -> Vec<u8> {
        let mut output = Encoder::with_capacity(
            READDIR_FIXED_LEN + self.records.len() * READDIR_ENTRY_LEN + self.names.len(),
        );
        output.u64(result.verifier);
        output.u64(result.next_cookie);
        output.u8(u8::from(result.eof));
        output.u32(u32::try_from(self.records.len()).unwrap_or(u32::MAX));
        output.u32(u32::try_from(self.names.len()).unwrap_or(u32::MAX));
        for (inode, next_cookie, offset, length, kind) in self.records {
            output.u64(inode);
            output.u64(next_cookie);
            output.u32(offset);
            output.u16(length);
            output.u8(kind);
        }
        output.bytes(&self.names);
        output.finish()
    }
}

impl DirectorySink for PackedDirectorySink {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
        let Ok(length) = u16::try_from(entry.name.len()) else {
            return false;
        };
        let Ok(offset) = u32::try_from(self.names.len()) else {
            return false;
        };
        if self.records.len() >= self.max_entries
            || self.names.len().saturating_add(entry.name.len()) > self.max_names
        {
            return false;
        }
        self.records.push((
            entry.inode,
            entry.next_cookie,
            offset,
            length,
            entry.kind as u8,
        ));
        self.names.extend_from_slice(entry.name);
        true
    }
}

struct Request {
    major: u16,
    operation: u16,
    minor: u16,
    id: u64,
    payload: Vec<u8>,
}

impl Request {
    fn read(stream: &mut UnixStream) -> io::Result<Self> {
        let mut header = [0; HEADER_LEN];
        stream.read_exact(&mut header)?;
        let mut input = Decoder::new(&header);
        if input.u32_io()? != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid FSK frame",
            ));
        }
        let major = input.u16_io()?;
        let operation = input.u16_io()?;
        let id = input.u64_io()?;
        let length = usize::try_from(input.u32_io()?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "oversized frame"))?;
        let minor = input.u16_io()?;
        let _flags = input.u16_io()?;
        if length > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized frame",
            ));
        }
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload)?;
        Ok(Self {
            major,
            operation,
            minor,
            id,
            payload,
        })
    }
}

fn protocol_mismatch(client_major: u16, client_minor: u16) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "FSK protocol {client_major}.{client_minor} cannot talk to this SDK ({PROTOCOL_MAJOR}.{PROTOCOL_MINOR}); install or update FSK.app"
        ),
    )
}

fn write_response(
    stream: &mut UnixStream,
    request_id: u64,
    minor: u16,
    status: i32,
    payload: &[u8],
) -> io::Result<()> {
    let mut header = Encoder::with_capacity(HEADER_LEN);
    header.u32(MAGIC);
    header.u16(PROTOCOL_MAJOR);
    header.u16(minor);
    header.u64(request_id);
    header.i32(status);
    header.u32(u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "response exceeds protocol limit",
        )
    })?);
    let header = header.finish();
    let slices = [io::IoSlice::new(&header), io::IoSlice::new(payload)];
    let written = stream.write_vectored(&slices)?;
    if written < header.len() {
        stream.write_all(&header[written..])?;
        stream.write_all(payload)
    } else {
        stream.write_all(&payload[written - header.len()..])
    }
}

pub(crate) struct Encoder(Vec<u8>);

impl Encoder {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }
    pub(crate) fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    pub(crate) fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn i32(&mut self, value: i32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }
    pub(crate) fn finish(self) -> Vec<u8> {
        self.0
    }
}

pub(crate) struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    pub(crate) fn take(&mut self, length: usize) -> crate::Result<&'a [u8]> {
        let end = self.offset.checked_add(length).ok_or(Error::INVALID)?;
        let value = self.bytes.get(self.offset..end).ok_or(Error::INVALID)?;
        self.offset = end;
        Ok(value)
    }
    pub(crate) fn remaining(&mut self) -> &'a [u8] {
        let value = &self.bytes[self.offset..];
        self.offset = self.bytes.len();
        value
    }
    pub(crate) fn u8(&mut self) -> crate::Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u16(&mut self) -> crate::Result<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().map_err(|_| Error::INVALID)?,
        ))
    }
    pub(crate) fn u32(&mut self) -> crate::Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| Error::INVALID)?,
        ))
    }
    pub(crate) fn u64(&mut self) -> crate::Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| Error::INVALID)?,
        ))
    }
    fn u16_io(&mut self) -> io::Result<u16> {
        self.u16().map_err(invalid_frame)
    }
    fn u32_io(&mut self) -> io::Result<u32> {
        self.u32().map_err(invalid_frame)
    }
    fn u64_io(&mut self) -> io::Result<u64> {
        self.u64().map_err(invalid_frame)
    }
}

fn invalid_frame(_: Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "truncated FSK frame")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileType, ROOT_INODE, test_support::TestFilesystem};
    use std::{
        os::{fd::AsRawFd, unix::net::UnixStream},
        time::{Duration, Instant},
    };

    #[test]
    fn authenticated_connection_serves_metadata() {
        let filesystem = TestFilesystem::default();
        let token = [7; 32];
        let service = Arc::new(Service::local(filesystem, token));
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || service.serve_connection(server).unwrap());

        send_request(&mut client, 1, opcode::HELLO, PROTOCOL_MINOR, &token);
        assert_eq!(read_test_response(&mut client).1, 0);
        send_request(
            &mut client,
            2,
            opcode::GETATTR,
            PROTOCOL_MINOR,
            &ROOT_INODE.to_le_bytes(),
        );
        let (_, status, body) = read_test_response(&mut client);
        assert_eq!(status, 0);
        assert_eq!(
            u64::from_le_bytes(body[0..8].try_into().unwrap()),
            ROOT_INODE
        );
        assert_eq!(body[80], FileType::Directory as u8);
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn incorrect_capability_is_rejected_before_dispatch() {
        let service = Arc::new(Service::local(TestFilesystem::default(), [7; 32]));
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            assert!(service.serve_connection(server).is_err());
        });
        send_request(&mut client, 1, opcode::HELLO, PROTOCOL_MINOR, &[8; 32]);
        assert_eq!(read_test_response(&mut client).1, libc::EACCES);
        worker.join().unwrap();
    }

    #[test]
    fn hello_negotiates_the_lower_minor_version() {
        for client_minor in [0, PROTOCOL_MINOR, PROTOCOL_MINOR + 7] {
            let service = Arc::new(Service::local(TestFilesystem::default(), [7; 32]));
            let (mut client, server) = UnixStream::pair().unwrap();
            let worker = std::thread::spawn(move || service.serve_connection(server).unwrap());

            send_request(&mut client, 1, opcode::HELLO, client_minor, &[7; 32]);
            let (minor, status, _) = read_test_response(&mut client);
            assert_eq!(status, 0, "client_minor={client_minor}");
            assert_eq!(
                minor,
                client_minor.min(PROTOCOL_MINOR),
                "client_minor={client_minor}"
            );
            drop(client);
            worker.join().unwrap();
        }
    }

    #[test]
    fn a_different_protocol_major_is_rejected() {
        let service = Arc::new(Service::local(TestFilesystem::default(), [7; 32]));
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let error = service.serve_connection(server).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("cannot talk to this SDK"));
        });

        send_request_with_major(
            &mut client,
            1,
            PROTOCOL_MAJOR + 1,
            opcode::HELLO,
            PROTOCOL_MINOR,
            &[7; 32],
        );
        let (minor, status, body) = read_test_response(&mut client);
        assert_eq!(status, libc::EPROTO);
        assert_eq!(minor, PROTOCOL_MINOR);
        assert_eq!(body, Vec::<u8>::new());
        worker.join().unwrap();
    }

    #[test]
    fn a_major_change_after_hello_is_rejected() {
        let service = Arc::new(Service::local(TestFilesystem::default(), [7; 32]));
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let error = service.serve_connection(server).unwrap_err();
            assert!(error.to_string().contains("cannot talk to this SDK"));
        });

        send_request(&mut client, 1, opcode::HELLO, PROTOCOL_MINOR, &[7; 32]);
        assert_eq!(read_test_response(&mut client).1, 0);
        send_request_with_major(
            &mut client,
            2,
            PROTOCOL_MAJOR + 1,
            opcode::GETATTR,
            PROTOCOL_MINOR,
            &ROOT_INODE.to_le_bytes(),
        );
        let (_, status, body) = read_test_response(&mut client);
        assert_eq!(status, libc::EPROTO);
        assert_eq!(body, Vec::<u8>::new());
        worker.join().unwrap();
    }

    #[test]
    fn the_hello_frame_layout_is_stable() {
        assert_eq!(HEADER_LEN, 24);
        // A frame is fixed little-endian: magic, major, opcode, request id,
        // body length, client minor, flags, then the body.
        let mut expected = vec![0x49, 0x42, 0x53, 0x46]; // magic
        expected.extend_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
        expected.extend_from_slice(&[0x00, 0x00]); // HELLO
        expected.extend_from_slice(&1_u64.to_le_bytes()); // request id
        expected.extend_from_slice(&3_u32.to_le_bytes()); // body length
        expected.extend_from_slice(&PROTOCOL_MINOR.to_le_bytes());
        expected.extend_from_slice(&[0x00, 0x00]); // flags
        expected.extend_from_slice(b"abc");
        assert_eq!(
            encode_request(1, PROTOCOL_MAJOR, opcode::HELLO, PROTOCOL_MINOR, b"abc"),
            expected
        );
    }

    #[test]
    fn the_swift_bridge_declares_the_same_protocol_version() {
        // The extension cannot import the Rust constants, so guard the two
        // hand-maintained copies against drift here.
        let macos = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("macos");
        let swift = macos.join("FSKExtension/RustBackend.swift");
        if let Ok(source) = std::fs::read_to_string(&swift) {
            assert!(
                source.contains(&format!("ipcProtocolMajor: UInt16 = {PROTOCOL_MAJOR}")),
                "RustBackend.swift must declare ipcProtocolMajor = {PROTOCOL_MAJOR}"
            );
            assert!(
                source.contains(&format!("ipcProtocolMinor: UInt16 = {PROTOCOL_MINOR}")),
                "RustBackend.swift must declare ipcProtocolMinor = {PROTOCOL_MINOR}"
            );
        }

        let plist = macos.join("FSKExtension/Info.plist");
        if let Ok(source) = std::fs::read_to_string(&plist) {
            assert_eq!(
                plist_integer(&source, "FSKProtocolMajor"),
                Some(PROTOCOL_MAJOR),
                "FSKExtension/Info.plist must declare FSKProtocolMajor = {PROTOCOL_MAJOR}"
            );
            assert_eq!(
                plist_integer(&source, "FSKProtocolMinor"),
                Some(PROTOCOL_MINOR),
                "FSKExtension/Info.plist must declare FSKProtocolMinor = {PROTOCOL_MINOR}"
            );
        }
    }

    #[test]
    fn readdir_batches_stay_within_the_frame_limit() {
        // The client aborts the connection on an oversized response, so the
        // worst case for every legal entry count must fit in one frame.
        for entries in [0, 1, 512, 65_536] {
            let names = readdir_name_budget(entries);
            assert!(
                READDIR_FIXED_LEN + entries * READDIR_ENTRY_LEN + names <= MAX_FRAME,
                "entries={entries}"
            );
        }
        // A normal client request is unaffected.
        assert_eq!(readdir_name_budget(512).min(64 * 1024), 64 * 1024);
    }

    #[test]
    fn nonblocking_server_stream_is_normalized_before_reading() {
        let filesystem = TestFilesystem::default();
        let token = [7; 32];
        let service = Arc::new(Service::local(filesystem, token));
        let (mut client, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let observer = server.try_clone().unwrap();
        let worker = std::thread::spawn(move || service.serve_connection(server).unwrap());

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: `observer` owns this valid descriptor for the duration
            // of the call, and `F_GETFL` takes no variadic argument.
            let flags = unsafe { libc::fcntl(observer.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(flags, -1);
            if flags & libc::O_NONBLOCK == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "serve_connection did not restore blocking mode"
            );
            std::thread::yield_now();
        }
        send_request(&mut client, 1, opcode::HELLO, PROTOCOL_MINOR, &token);
        assert_eq!(read_test_response(&mut client).1, 0);
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    #[ignore = "microbenchmark; run explicitly with --release --ignored --nocapture"]
    fn benchmark_authenticated_ipc_lookup_roundtrip() {
        let filesystem = TestFilesystem::with_entry();
        let token = [7; 32];
        let service = Arc::new(Service::local(filesystem, token));
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || service.serve_connection(server).unwrap());
        send_request(&mut client, 1, opcode::HELLO, PROTOCOL_MINOR, &token);
        assert_eq!(read_test_response(&mut client).1, 0);

        let mut request = Encoder::with_capacity(13);
        request.u64(ROOT_INODE);
        request.bytes(b"entry");
        let request = request.finish();
        let iterations = 100_000_u32;
        let start = std::time::Instant::now();
        for index in 0..iterations {
            send_request(
                &mut client,
                u64::from(index) + 2,
                opcode::LOOKUP,
                PROTOCOL_MINOR,
                &request,
            );
            assert_eq!(read_test_response(&mut client).1, 0);
        }
        let elapsed = start.elapsed();
        eprintln!(
            "authenticated IPC lookup: {:.0} roundtrips/s ({:.1} µs/roundtrip)",
            f64::from(iterations) / elapsed.as_secs_f64(),
            elapsed.as_secs_f64() * 1_000_000.0 / f64::from(iterations)
        );
        drop(client);
        worker.join().unwrap();
    }

    fn send_request(stream: &mut UnixStream, id: u64, operation: u16, minor: u16, payload: &[u8]) {
        send_request_with_major(stream, id, PROTOCOL_MAJOR, operation, minor, payload);
    }

    fn send_request_with_major(
        stream: &mut UnixStream,
        id: u64,
        major: u16,
        operation: u16,
        minor: u16,
        payload: &[u8],
    ) {
        let frame = encode_request(id, major, operation, minor, payload);
        stream.write_all(&frame).unwrap();
    }

    fn encode_request(id: u64, major: u16, operation: u16, minor: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = Encoder::with_capacity(HEADER_LEN + payload.len());
        frame.u32(MAGIC);
        frame.u16(major);
        frame.u16(operation);
        frame.u64(id);
        frame.u32(u32::try_from(payload.len()).unwrap());
        frame.u16(minor);
        frame.u16(0);
        frame.bytes(payload);
        frame.finish()
    }

    fn read_test_response(stream: &mut UnixStream) -> (u16, i32, Vec<u8>) {
        let mut header = [0; HEADER_LEN];
        stream.read_exact(&mut header).unwrap();
        let minor = u16::from_le_bytes(header[6..8].try_into().unwrap());
        let status = i32::from_le_bytes(header[16..20].try_into().unwrap());
        let length = u32::from_le_bytes(header[20..24].try_into().unwrap()) as usize;
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        (minor, status, body)
    }

    fn plist_integer(source: &str, key: &str) -> Option<u16> {
        let rest = source.split(&format!("<key>{key}</key>")).nth(1)?;
        let value = rest.split("<integer>").nth(1)?;
        value.split("</integer>").next()?.trim().parse().ok()
    }
}
