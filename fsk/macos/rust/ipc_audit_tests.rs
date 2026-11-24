//! Snapshot-only deterministic regression tests for the macOS IPC audit.

use super::*;
use crate::{FileType, ReadDirectoryResult, Result, StatFs, test_support::TestFilesystem};
use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

struct LeaseTrackingFilesystem {
    inner: TestFilesystem,
    local_path: PathBuf,
    close_calls: Arc<AtomicU64>,
}

impl Filesystem for LeaseTrackingFilesystem {
    fn statfs(&self) -> Result<StatFs> {
        self.inner.statfs()
    }

    fn metadata(&self, inode: u64) -> Result<crate::Metadata> {
        self.inner.metadata(inode)
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)> {
        self.inner.lookup(parent, name)
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult> {
        self.inner.read_directory(inode, cookie, verifier, sink)
    }

    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize> {
        self.inner.read(inode, offset, output)
    }
}

impl NetworkFilesystem for LeaseTrackingFilesystem {
    fn open_file(&self, _inode: u64, _writable: bool) -> Result<NetworkFile> {
        Ok(NetworkFile::Local(crate::LocalFile::new(
            self.local_path.clone(),
            11,
        )))
    }

    fn close_file(&self, inode: u64, file: NetworkFile) -> Result<()> {
        assert_eq!(inode, 2);
        assert_eq!(
            file,
            NetworkFile::Local(crate::LocalFile::new(self.local_path.clone(), 11))
        );
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn disconnect_releases_local_file_lease() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("materialized.bin");
    std::fs::write(&local_path, b"cached bytes").unwrap();
    let close_calls = Arc::new(AtomicU64::new(0));
    let service = Arc::new(Service::network(
        LeaseTrackingFilesystem {
            inner: TestFilesystem::default(),
            local_path,
            close_calls: Arc::clone(&close_calls),
        },
        [7; 32],
    ));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker_service = Arc::clone(&service);
    let worker = std::thread::spawn(move || worker_service.serve_connection(server));

    send(&mut client, 1, opcode::HELLO, &[7; 32]);
    assert_eq!(response(&mut client).1, 0);

    let mut open = Encoder::with_capacity(9);
    open.u64(2);
    open.u8(0);
    let open = open.finish();
    send(&mut client, 2, opcode::OPEN, &open);
    let (_, status, body) = response(&mut client);
    assert_eq!(status, 0);
    let lease = u64::from_le_bytes(body[..8].try_into().unwrap());
    assert!(service.leases.lock().contains_key(&lease));

    // A concurrent connection's lease must not be swept up by the first
    // connection's disconnect cleanup.
    let (mut client2, server2) = UnixStream::pair().unwrap();
    client2
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker_service2 = Arc::clone(&service);
    let worker2 = std::thread::spawn(move || worker_service2.serve_connection(server2));
    send(&mut client2, 1, opcode::HELLO, &[7; 32]);
    assert_eq!(response(&mut client2).1, 0);
    send(&mut client2, 2, opcode::OPEN, &open);
    let (_, status, body2) = response(&mut client2);
    assert_eq!(status, 0);
    let lease2 = u64::from_le_bytes(body2[..8].try_into().unwrap());
    assert_ne!(lease, lease2);

    drop(client);
    worker.join().unwrap().unwrap();

    assert_eq!(
        close_calls.load(Ordering::SeqCst),
        1,
        "an authenticated disconnect must close the connection's local-file lease"
    );
    let remaining = service.leases.lock().keys().copied().collect::<Vec<_>>();
    assert_eq!(
        remaining,
        vec![lease2],
        "disconnect cleanup must be connection-scoped"
    );

    send(&mut client2, 3, opcode::CLOSE, &lease2.to_le_bytes());
    assert_eq!(response(&mut client2).1, 0);
    drop(client2);
    worker2.join().unwrap().unwrap();
    assert_eq!(close_calls.load(Ordering::SeqCst), 2);
    assert!(service.leases.lock().is_empty());
}

#[test]
fn fragmented_and_batched_frames_preserve_negotiation_and_response_ids() {
    let service = Arc::new(Service::local(TestFilesystem::with_entry(), [7; 32]));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker = std::thread::spawn(move || Arc::clone(&service).serve_connection(server));

    send_fragmented(&mut client, 1, opcode::HELLO, 0, &[7; 32]);
    let (id, minor, status, body) = response_full(&mut client);
    assert_eq!((id, minor, status), (1, 0, 0));
    assert_eq!(body, Vec::<u8>::new());

    let mut metadata = Encoder::with_capacity(8);
    metadata.u64(crate::ROOT_INODE);
    send(&mut client, u64::MAX, opcode::GETATTR, &metadata.finish());
    let (id, status, body) = response(&mut client);
    assert_eq!((id, status), (u64::MAX, 0));
    assert_eq!(
        u64::from_le_bytes(body[..8].try_into().unwrap()),
        crate::ROOT_INODE
    );

    let mut lookup = Encoder::with_capacity(13);
    lookup.u64(crate::ROOT_INODE);
    lookup.bytes(b"entry");
    let lookup = lookup.finish();
    let mut batch = Vec::new();
    batch.extend(frame(7, opcode::LOOKUP, PROTOCOL_MINOR, &lookup));
    batch.extend(frame(8, opcode::LOOKUP, PROTOCOL_MINOR, &lookup));
    client.write_all(&batch).unwrap();
    for id in [7, 8] {
        let (response_id, status, body) = response(&mut client);
        assert_eq!((response_id, status), (id, 0));
        assert_eq!(u64::from_le_bytes(body[..8].try_into().unwrap()), 2);
    }

    drop(client);
    worker.join().unwrap().unwrap();
}

#[test]
fn oversized_frame_header_is_rejected_without_waiting_for_its_body() {
    let (mut client, mut server) = UnixStream::pair().unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut header = Encoder::with_capacity(HEADER_LEN);
    header.u32(MAGIC);
    header.u16(PROTOCOL_MAJOR);
    header.u16(opcode::HELLO);
    header.u64(1);
    header.u32(u32::try_from(MAX_FRAME + 1).unwrap());
    header.u16(PROTOCOL_MINOR);
    header.u16(0);
    client.write_all(&header.finish()).unwrap();

    let error = Request::read(&mut server).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "oversized frame");
}

#[test]
fn truncated_headers_and_payloads_are_reported_as_eof() {
    let complete = frame(1, opcode::HELLO, PROTOCOL_MINOR, &[7; 32]);
    for length in [
        0,
        4,
        HEADER_LEN - 1,
        HEADER_LEN,
        HEADER_LEN + 1,
        complete.len() - 1,
    ] {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(&complete[..length]).unwrap();
        drop(client);
        let error = Request::read(&mut server).err().unwrap();
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "truncation at {length}"
        );
    }
}

#[test]
fn a_server_error_preserves_the_connection_and_request_id() {
    let service = Arc::new(Service::local(TestFilesystem::default(), [7; 32]));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker = std::thread::spawn(move || Arc::clone(&service).serve_connection(server));
    send(&mut client, 1, opcode::HELLO, &[7; 32]);
    assert_eq!(response(&mut client).1, 0);

    let mut missing = Encoder::with_capacity(8);
    missing.u64(999);
    send(&mut client, 44, opcode::GETATTR, &missing.finish());
    let (id, status, body) = response(&mut client);
    assert_eq!((id, status), (44, libc::ENOENT));
    assert_eq!(body, Vec::<u8>::new());

    send(&mut client, 45, opcode::STATFS, &[]);
    let (id, status, body) = response(&mut client);
    assert_eq!((id, status), (45, 0));
    assert_eq!(body.len(), 48);
    drop(client);
    worker.join().unwrap().unwrap();
}

#[test]
fn directory_truncation_resumes_at_the_last_accepted_cookie() {
    struct Entries {
        last_accepted: std::sync::Mutex<u64>,
    }

    impl Filesystem for Entries {
        fn statfs(&self) -> Result<StatFs> {
            unreachable!()
        }

        fn metadata(&self, _: u64) -> Result<crate::Metadata> {
            unreachable!()
        }

        fn lookup(&self, _: u64, _: &[u8]) -> Result<(u64, FileType)> {
            unreachable!()
        }

        fn read_directory(
            &self,
            _: u64,
            cookie: u64,
            _: u64,
            sink: &mut dyn DirectorySink,
        ) -> Result<ReadDirectoryResult> {
            let names = [b"a".as_slice(), b"b", b"c"];
            let start = usize::try_from(cookie).unwrap();
            let mut last = 0;
            let mut eof = true;
            for (index, name) in names.into_iter().enumerate().skip(start) {
                if !sink.push(DirectoryEntry {
                    name,
                    inode: u64::try_from(index + 2).unwrap(),
                    kind: FileType::File,
                    next_cookie: u64::try_from(index + 1).unwrap(),
                }) {
                    eof = false;
                    break;
                }
                last = u64::try_from(index + 1).unwrap();
            }
            *self.last_accepted.lock().unwrap() = last;
            Ok(ReadDirectoryResult {
                verifier: 1,
                next_cookie: last,
                eof,
            })
        }

        fn read(&self, _: u64, _: u64, _: &mut [u8]) -> Result<usize> {
            unreachable!()
        }
    }

    let service = Service::local(
        Entries {
            last_accepted: std::sync::Mutex::new(0),
        },
        [7; 32],
    );
    let mut request = Encoder::with_capacity(32);
    request.u64(crate::ROOT_INODE);
    request.u64(0);
    request.u64(0);
    request.u32(2);
    request.u32(1024);
    let body = service
        .dispatch(1, opcode::READDIR, &request.finish())
        .unwrap();
    let mut decoder = Decoder::new(&body);
    assert_eq!(decoder.u64().unwrap(), 1);
    assert_eq!(decoder.u64().unwrap(), 2);
    assert_eq!(decoder.u8().unwrap(), 0);
    assert_eq!(decoder.u32().unwrap(), 2);
    assert_eq!(decoder.u32().unwrap(), 2);
    let first = decoder.u64().unwrap();
    let first_cookie = decoder.u64().unwrap();
    let first_offset = decoder.u32().unwrap();
    let first_length = decoder.u16().unwrap();
    let _ = decoder.u8().unwrap();
    let second = decoder.u64().unwrap();
    let second_cookie = decoder.u64().unwrap();
    let second_offset = decoder.u32().unwrap();
    let second_length = decoder.u16().unwrap();
    let _ = decoder.u8().unwrap();
    assert_eq!((first, first_cookie, second, second_cookie), (2, 1, 3, 2));
    assert_eq!(decoder.remaining(), b"ab");
    assert_eq!((first_offset, first_length), (0, 1));
    assert_eq!((second_offset, second_length), (1, 1));
}

fn send_fragmented(stream: &mut UnixStream, id: u64, operation: u16, minor: u16, payload: &[u8]) {
    for byte in frame(id, operation, minor, payload) {
        stream.write_all(&[byte]).unwrap();
    }
}

fn frame(id: u64, operation: u16, minor: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Encoder::with_capacity(HEADER_LEN + payload.len());
    frame.u32(MAGIC);
    frame.u16(PROTOCOL_MAJOR);
    frame.u16(operation);
    frame.u64(id);
    frame.u32(u32::try_from(payload.len()).unwrap());
    frame.u16(minor);
    frame.u16(0);
    frame.bytes(payload);
    frame.finish()
}

fn send(stream: &mut UnixStream, id: u64, operation: u16, payload: &[u8]) {
    let mut frame = Encoder::with_capacity(HEADER_LEN + payload.len());
    frame.u32(MAGIC);
    frame.u16(PROTOCOL_MAJOR);
    frame.u16(operation);
    frame.u64(id);
    frame.u32(u32::try_from(payload.len()).unwrap());
    frame.u16(PROTOCOL_MINOR);
    frame.u16(0);
    frame.bytes(payload);
    stream.write_all(&frame.finish()).unwrap();
}

fn response(stream: &mut UnixStream) -> (u64, i32, Vec<u8>) {
    let (id, _, status, body) = response_full(stream);
    (id, status, body)
}

fn response_full(stream: &mut UnixStream) -> (u64, u16, i32, Vec<u8>) {
    let mut header = [0; HEADER_LEN];
    stream.read_exact(&mut header).unwrap();
    let id = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let minor = u16::from_le_bytes(header[6..8].try_into().unwrap());
    let status = i32::from_le_bytes(header[16..20].try_into().unwrap());
    let length = u32::from_le_bytes(header[20..24].try_into().unwrap()) as usize;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    (id, minor, status, body)
}
