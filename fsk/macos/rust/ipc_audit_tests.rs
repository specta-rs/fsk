//! Snapshot-only deterministic regression tests for the macOS IPC audit.

use super::*;
use crate::{FileType, ReadDirectoryResult, Result, StatFs, test_support::TestFilesystem};
use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Barrier,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

struct LeaseTrackingFilesystem {
    inner: TestFilesystem,
    local_path: PathBuf,
    close_calls: Arc<AtomicU64>,
    open_entered: Option<Arc<Barrier>>,
    open_release: Option<Arc<Barrier>>,
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
        if let Some(barrier) = &self.open_entered {
            barrier.wait();
        }
        if let Some(barrier) = &self.open_release {
            barrier.wait();
        }
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
fn failed_open_response_releases_the_pending_lease() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("materialized.bin");
    std::fs::write(&local_path, b"cached bytes").unwrap();
    let close_calls = Arc::new(AtomicU64::new(0));
    let open_entered = Arc::new(Barrier::new(2));
    let open_release = Arc::new(Barrier::new(2));
    let service = Arc::new(Service::network(
        LeaseTrackingFilesystem {
            inner: TestFilesystem::default(),
            local_path,
            close_calls: Arc::clone(&close_calls),
            open_entered: Some(Arc::clone(&open_entered)),
            open_release: Some(Arc::clone(&open_release)),
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
    send(&mut client, 2, opcode::OPEN, &open.finish());
    open_entered.wait();
    drop(client);
    open_release.wait();
    worker.join().unwrap().unwrap_err();

    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    assert!(service.leases.lock().is_empty());
    drop(service);
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn unacknowledged_disconnect_releases_only_pending_lease() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("materialized.bin");
    std::fs::write(&local_path, b"cached bytes").unwrap();
    let close_calls = Arc::new(AtomicU64::new(0));
    let service = Arc::new(Service::network(
        LeaseTrackingFilesystem {
            inner: TestFilesystem::default(),
            local_path: local_path.clone(),
            close_calls: Arc::clone(&close_calls),
            open_entered: None,
            open_release: None,
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
    let pending = u64::from_le_bytes(body[..8].try_into().unwrap());
    assert!(
        service
            .leases
            .lock()
            .get(&pending)
            .unwrap()
            .pending_connection
            .is_some()
    );

    let (mut client2, server2) = UnixStream::pair().unwrap();
    client2
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker_service2 = Arc::clone(&service);
    let worker2 = std::thread::spawn(move || worker_service2.serve_connection(server2));
    send(&mut client2, 1, opcode::HELLO, &[7; 32]);
    assert_eq!(response(&mut client2).1, 0);
    send(&mut client2, 2, opcode::OPEN, &open);
    let (_, status, body) = response(&mut client2);
    assert_eq!(status, 0);
    let acknowledged = u64::from_le_bytes(body[..8].try_into().unwrap());
    send(
        &mut client2,
        3,
        opcode::ACK_LEASE,
        &acknowledged.to_le_bytes(),
    );
    assert_eq!(response(&mut client2).1, 0);

    drop(client);
    worker.join().unwrap().unwrap();
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    assert!(!service.leases.lock().contains_key(&pending));
    assert!(service.leases.lock().contains_key(&acknowledged));

    send(&mut client2, 4, opcode::CLOSE, &acknowledged.to_le_bytes());
    assert_eq!(response(&mut client2).1, 0);
    drop(client2);
    worker2.join().unwrap().unwrap();
    assert_eq!(close_calls.load(Ordering::SeqCst), 2);
    assert!(service.leases.lock().is_empty());
}

#[test]
fn close_succeeds_after_pending_cleanup_and_repeated_close() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("materialized.bin");
    std::fs::write(&local_path, b"cached bytes").unwrap();
    let close_calls = Arc::new(AtomicU64::new(0));
    let service = Arc::new(Service::network(
        LeaseTrackingFilesystem {
            inner: TestFilesystem::default(),
            local_path,
            close_calls: Arc::clone(&close_calls),
            open_entered: None,
            open_release: None,
        },
        [7; 32],
    ));
    let (mut creator, creator_server) = UnixStream::pair().unwrap();
    creator
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker_service = Arc::clone(&service);
    let creator_worker =
        std::thread::spawn(move || worker_service.serve_connection(creator_server));
    send(&mut creator, 1, opcode::HELLO, &[7; 32]);
    assert_eq!(response(&mut creator).1, 0);

    let mut open = Encoder::with_capacity(9);
    open.u64(2);
    open.u8(0);
    let open = open.finish();
    send(&mut creator, 2, opcode::OPEN, &open);
    let (_, status, body) = response(&mut creator);
    assert_eq!(status, 0);
    let pending = u64::from_le_bytes(body[..8].try_into().unwrap());

    let (mut closer, closer_server) = UnixStream::pair().unwrap();
    closer
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker_service = Arc::clone(&service);
    let closer_worker = std::thread::spawn(move || worker_service.serve_connection(closer_server));
    send(&mut closer, 1, opcode::HELLO, &[7; 32]);
    assert_eq!(response(&mut closer).1, 0);

    drop(creator);
    creator_worker.join().unwrap().unwrap();
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    assert!(!service.leases.lock().contains_key(&pending));

    send(&mut closer, 2, opcode::CLOSE, &pending.to_le_bytes());
    assert_eq!(response(&mut closer).1, 0);
    send(&mut closer, 3, opcode::CLOSE, &pending.to_le_bytes());
    assert_eq!(response(&mut closer).1, 0);
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);

    send(&mut closer, 4, opcode::OPEN, &open);
    let (_, status, body) = response(&mut closer);
    assert_eq!(status, 0);
    let acknowledged = u64::from_le_bytes(body[..8].try_into().unwrap());
    send(
        &mut closer,
        5,
        opcode::ACK_LEASE,
        &acknowledged.to_le_bytes(),
    );
    assert_eq!(response(&mut closer).1, 0);
    send(&mut closer, 6, opcode::CLOSE, &acknowledged.to_le_bytes());
    assert_eq!(response(&mut closer).1, 0);
    send(&mut closer, 7, opcode::CLOSE, &acknowledged.to_le_bytes());
    assert_eq!(response(&mut closer).1, 0);
    assert_eq!(close_calls.load(Ordering::SeqCst), 2);

    drop(closer);
    closer_worker.join().unwrap().unwrap();
    assert!(service.leases.lock().is_empty());
}

#[test]
fn legacy_peers_acknowledge_open_on_response_write() {
    for minor in [0, LEASE_ACK_MINOR - 1] {
        let directory = tempfile::tempdir().unwrap();
        let local_path = directory.path().join("materialized.bin");
        std::fs::write(&local_path, b"cached bytes").unwrap();
        let close_calls = Arc::new(AtomicU64::new(0));
        let service = Arc::new(Service::network(
            LeaseTrackingFilesystem {
                inner: TestFilesystem::default(),
                local_path,
                close_calls: Arc::clone(&close_calls),
                open_entered: None,
                open_release: None,
            },
            [7; 32],
        ));
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let worker_service = Arc::clone(&service);
        let worker = std::thread::spawn(move || worker_service.serve_connection(server));
        send_with_minor(&mut client, 1, opcode::HELLO, minor, &[7; 32]);
        let (_, negotiated, status, _) = response_full(&mut client);
        assert_eq!(negotiated, minor);
        assert_eq!(status, 0);

        let mut open = Encoder::with_capacity(9);
        open.u64(2);
        open.u8(0);
        send_with_minor(&mut client, 2, opcode::OPEN, minor, &open.finish());
        let (_, status, body) = response(&mut client);
        assert_eq!(status, 0);
        let lease = u64::from_le_bytes(body[..8].try_into().unwrap());
        assert!(
            service
                .leases
                .lock()
                .get(&lease)
                .unwrap()
                .pending_connection
                .is_none()
        );

        drop(client);
        worker.join().unwrap().unwrap();
        assert_eq!(close_calls.load(Ordering::SeqCst), 0);
        assert!(service.leases.lock().contains_key(&lease));

        let (mut client2, server2) = UnixStream::pair().unwrap();
        let worker_service2 = Arc::clone(&service);
        let worker2 = std::thread::spawn(move || worker_service2.serve_connection(server2));
        send_with_minor(&mut client2, 1, opcode::HELLO, minor, &[7; 32]);
        assert_eq!(response(&mut client2).1, 0);
        send_with_minor(&mut client2, 2, opcode::CLOSE, minor, &lease.to_le_bytes());
        assert_eq!(response(&mut client2).1, 0);
        drop(client2);
        worker2.join().unwrap().unwrap();
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
        assert!(service.leases.lock().is_empty());
    }
}

#[test]
fn ack_from_another_connection_is_rejected_without_clearing_pending_lease() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("materialized.bin");
    std::fs::write(&local_path, b"cached bytes").unwrap();
    let close_calls = Arc::new(AtomicU64::new(0));
    let service = Arc::new(Service::network(
        LeaseTrackingFilesystem {
            inner: TestFilesystem::default(),
            local_path,
            close_calls: Arc::clone(&close_calls),
            open_entered: None,
            open_release: None,
        },
        [7; 32],
    ));
    let (mut creator, creator_server) = UnixStream::pair().unwrap();
    creator
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker_service = Arc::clone(&service);
    let creator_worker =
        std::thread::spawn(move || worker_service.serve_connection(creator_server));
    send(&mut creator, 1, opcode::HELLO, &[7; 32]);
    assert_eq!(response(&mut creator).1, 0);

    let mut open = Encoder::with_capacity(9);
    open.u64(2);
    open.u8(0);
    send(&mut creator, 2, opcode::OPEN, &open.finish());
    let (_, status, body) = response(&mut creator);
    assert_eq!(status, 0);
    let lease = u64::from_le_bytes(body[..8].try_into().unwrap());
    assert!(
        service
            .leases
            .lock()
            .get(&lease)
            .unwrap()
            .pending_connection
            .is_some()
    );

    let (mut other, other_server) = UnixStream::pair().unwrap();
    other
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker_service = Arc::clone(&service);
    let other_worker = std::thread::spawn(move || worker_service.serve_connection(other_server));
    send(&mut other, 1, opcode::HELLO, &[7; 32]);
    assert_eq!(response(&mut other).1, 0);

    send(&mut other, 2, opcode::ACK_LEASE, &lease.to_le_bytes());
    assert_eq!(response(&mut other).1, Error::INVALID.0);
    assert_eq!(close_calls.load(Ordering::SeqCst), 0);
    assert!(
        service
            .leases
            .lock()
            .get(&lease)
            .unwrap()
            .pending_connection
            .is_some()
    );

    send(&mut creator, 3, opcode::ACK_LEASE, &lease.to_le_bytes());
    assert_eq!(response(&mut creator).1, 0);
    assert!(
        service
            .leases
            .lock()
            .get(&lease)
            .unwrap()
            .pending_connection
            .is_none()
    );

    drop(creator);
    creator_worker.join().unwrap().unwrap();
    assert_eq!(close_calls.load(Ordering::SeqCst), 0);
    assert!(service.leases.lock().contains_key(&lease));

    send(&mut other, 3, opcode::CLOSE, &lease.to_le_bytes());
    assert_eq!(response(&mut other).1, 0);
    drop(other);
    other_worker.join().unwrap().unwrap();
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    assert!(service.leases.lock().is_empty());
}

#[test]
fn acknowledged_disconnect_preserves_local_file_lease_until_kernel_close() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("materialized.bin");
    std::fs::write(&local_path, b"cached bytes").unwrap();
    let close_calls = Arc::new(AtomicU64::new(0));
    let service = Arc::new(Service::network(
        LeaseTrackingFilesystem {
            inner: TestFilesystem::default(),
            local_path: local_path.clone(),
            close_calls: Arc::clone(&close_calls),
            open_entered: None,
            open_release: None,
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

    // A second pooled connection can hold an independent lease at the same time.
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
    // Each lease must be acknowledged by the connection that created it.
    send(&mut client, 3, opcode::ACK_LEASE, &lease.to_le_bytes());
    assert_eq!(response(&mut client).1, 0);
    send(&mut client2, 3, opcode::ACK_LEASE, &lease2.to_le_bytes());
    assert_eq!(response(&mut client2).1, 0);
    assert!(
        service
            .leases
            .lock()
            .get(&lease)
            .unwrap()
            .pending_connection
            .is_none()
    );

    drop(client);
    worker.join().unwrap().unwrap();

    assert_eq!(
        close_calls.load(Ordering::SeqCst),
        0,
        "an IPC disconnect must not close a lease while the kernel open is alive"
    );
    assert_eq!(std::fs::read(&local_path).unwrap(), b"cached bytes");
    let mut remaining = service.leases.lock().keys().copied().collect::<Vec<_>>();
    remaining.sort_unstable();
    let mut expected = vec![lease, lease2];
    expected.sort_unstable();
    assert_eq!(remaining, expected);

    // A reconnected pooled transport can close the original kernel lease.
    let (mut client3, server3) = UnixStream::pair().unwrap();
    client3
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker_service3 = Arc::clone(&service);
    let worker3 = std::thread::spawn(move || worker_service3.serve_connection(server3));
    send(&mut client3, 1, opcode::HELLO, &[7; 32]);
    assert_eq!(response(&mut client3).1, 0);

    send(&mut client3, 2, opcode::CLOSE, &lease.to_le_bytes());
    assert_eq!(response(&mut client3).1, 0);
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    assert!(!service.leases.lock().contains_key(&lease));
    assert!(service.leases.lock().contains_key(&lease2));

    send(&mut client2, 3, opcode::CLOSE, &lease2.to_le_bytes());
    assert_eq!(response(&mut client2).1, 0);
    drop(client2);
    drop(client3);
    worker2.join().unwrap().unwrap();
    worker3.join().unwrap().unwrap();
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
    send_with_minor(stream, id, operation, PROTOCOL_MINOR, payload);
}

fn send_with_minor(stream: &mut UnixStream, id: u64, operation: u16, minor: u16, payload: &[u8]) {
    stream
        .write_all(&frame(id, operation, minor, payload))
        .unwrap();
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
