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
pub(crate) const VERSION: u16 = 1;
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
        let (request_id, operation, payload) = read_request(&mut stream)?;
        if operation != opcode::HELLO || payload.as_slice() != self.token {
            write_response(&mut stream, request_id, libc::EACCES, &[])?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid FSBind capability",
            ));
        }
        write_response(&mut stream, request_id, 0, &[])?;

        loop {
            let (request_id, operation, payload) = match read_request(&mut stream) {
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
            let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.dispatch(request_id, operation, &payload)
            }))
            .unwrap_or(Err(Error::IO));
            match response {
                Ok(body) => write_response(&mut stream, request_id, 0, &body)?,
                Err(error) => write_response(&mut stream, request_id, error.0.max(1), &[])?,
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
                let count = fs.read(inode, offset, &mut output)?;
                output.truncate(count);
                Ok(output)
            }
            opcode::WRITE => {
                let inode = input.u64()?;
                let offset = input.u64()?;
                let count = fs.write(inode, offset, input.remaining())?;
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
                let count = fs.read_symlink(inode, &mut output)?;
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
        let mut output = Encoder::with_capacity(32 + self.records.len() * 24 + self.names.len());
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

fn read_request(stream: &mut UnixStream) -> io::Result<(u64, u16, Vec<u8>)> {
    let mut header = [0; HEADER_LEN];
    stream.read_exact(&mut header)?;
    let mut input = Decoder::new(&header);
    if input.u32_io()? != MAGIC || input.u16_io()? != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid FSBind frame",
        ));
    }
    let operation = input.u16_io()?;
    let request_id = input.u64_io()?;
    let length = usize::try_from(input.u32_io()?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "oversized frame"))?;
    let _reserved = input.u32_io()?;
    if length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized frame",
        ));
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    Ok((request_id, operation, payload))
}

fn write_response(
    stream: &mut UnixStream,
    request_id: u64,
    status: i32,
    payload: &[u8],
) -> io::Result<()> {
    let mut header = Encoder::with_capacity(HEADER_LEN);
    header.u32(MAGIC);
    header.u16(VERSION);
    header.u16(0);
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
    io::Error::new(io::ErrorKind::InvalidData, "truncated FSBind frame")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileType, ROOT_INODE, test_support::TestFilesystem};
    use std::os::unix::net::UnixStream;

    #[test]
    fn authenticated_connection_serves_metadata() {
        let filesystem = TestFilesystem::default();
        let token = [7; 32];
        let service = Arc::new(Service::local(filesystem, token));
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || service.serve_connection(server).unwrap());

        send_request(&mut client, 1, opcode::HELLO, &token);
        assert_eq!(read_test_response(&mut client).0, 0);
        send_request(&mut client, 2, opcode::GETATTR, &ROOT_INODE.to_le_bytes());
        let (status, body) = read_test_response(&mut client);
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
        send_request(&mut client, 1, opcode::HELLO, &[8; 32]);
        assert_eq!(read_test_response(&mut client).0, libc::EACCES);
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
        send_request(&mut client, 1, opcode::HELLO, &token);
        assert_eq!(read_test_response(&mut client).0, 0);

        let mut request = Encoder::with_capacity(13);
        request.u64(ROOT_INODE);
        request.bytes(b"entry");
        let request = request.finish();
        let iterations = 100_000_u32;
        let start = std::time::Instant::now();
        for index in 0..iterations {
            send_request(&mut client, u64::from(index) + 2, opcode::LOOKUP, &request);
            assert_eq!(read_test_response(&mut client).0, 0);
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

    fn send_request(stream: &mut UnixStream, id: u64, operation: u16, payload: &[u8]) {
        let mut frame = Encoder::with_capacity(HEADER_LEN + payload.len());
        frame.u32(MAGIC);
        frame.u16(VERSION);
        frame.u16(operation);
        frame.u64(id);
        frame.u32(u32::try_from(payload.len()).unwrap());
        frame.u32(0);
        frame.bytes(payload);
        stream.write_all(&frame.finish()).unwrap();
    }

    fn read_test_response(stream: &mut UnixStream) -> (i32, Vec<u8>) {
        let mut header = [0; HEADER_LEN];
        stream.read_exact(&mut header).unwrap();
        let status = i32::from_le_bytes(header[16..20].try_into().unwrap());
        let length = u32::from_le_bytes(header[20..24].try_into().unwrap()) as usize;
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        (status, body)
    }
}
