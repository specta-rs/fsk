use super::*;
use tempfile::TempDir;

fn test_filesystem(resource: &[u8]) -> (ExampleNetworkFilesystem, TempDir) {
    let cache = tempfile::tempdir().unwrap();
    let downloaded = b"This file is read locally.\n".to_vec();
    let downloaded_path = cache.path().join("downloaded.txt");
    std::fs::write(&downloaded_path, &downloaded).unwrap();
    let filesystem = ExampleNetworkFilesystem {
        remote: resource.to_vec(),
        downloaded,
        downloaded_inode: 3,
        downloaded_path,
        cache_generation: std::sync::atomic::AtomicU64::new(1),
        cache_lock: std::sync::Mutex::new(()),
    };
    (filesystem, cache)
}

#[derive(Default)]
struct VecSink {
    capacity: usize,
    entries: Vec<Vec<u8>>,
}

impl DirectorySink for VecSink {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
        if self.entries.len() == self.capacity {
            return false;
        }
        self.entries.push(entry.name.to_vec());
        true
    }
}

#[test]
fn audit_writable_open_does_not_lease_a_read_only_local_file() {
    let (filesystem, _cache) = test_filesystem(b"remote");

    let opened = filesystem.open_file(3, true).unwrap();
    assert!(
        matches!(opened, NetworkFile::Remote),
        "a writable open of the immutable downloaded inode returned a local-file lease"
    );
}

#[test]
fn audit_local_open_never_leases_a_missing_cache_file() {
    let (filesystem, _cache) = test_filesystem(b"remote");
    let NetworkFile::Local(initial) = filesystem.open_file(3, false).unwrap() else {
        panic!("downloaded file should use a local lease");
    };
    std::fs::remove_file(initial.path()).unwrap();

    let NetworkFile::Local(reopened) = filesystem.open_file(3, false).unwrap() else {
        panic!("downloaded file should use a local lease");
    };
    assert_eq!(
        std::fs::read(reopened.path()).unwrap(),
        filesystem.downloaded,
        "a local lease must point at a readable, current cache file"
    );
}

#[test]
fn audit_local_open_detects_and_repairs_stale_cache_bytes() {
    let (filesystem, _cache) = test_filesystem(b"remote");
    let NetworkFile::Local(initial) = filesystem.open_file(3, false).unwrap() else {
        panic!("downloaded file should use a local lease");
    };
    std::fs::write(initial.path(), b"stale cache bytes").unwrap();

    let mut remote = vec![0; filesystem.downloaded.len()];
    assert_eq!(filesystem.read(3, 0, &mut remote), Ok(remote.len()));
    assert_eq!(remote, filesystem.downloaded);

    let NetworkFile::Local(reopened) = filesystem.open_file(3, false).unwrap() else {
        panic!("downloaded file should use a local lease");
    };
    assert_eq!(std::fs::read(reopened.path()).unwrap(), remote);
    assert_ne!(
        reopened.generation(),
        initial.generation(),
        "repairing local contents must invalidate the old local-file generation"
    );
}

#[test]
fn audit_open_rejects_unknown_inodes() {
    let (filesystem, _cache) = test_filesystem(b"remote");

    assert_eq!(filesystem.open_file(999, false), Err(Error::NOT_FOUND));
    assert_eq!(filesystem.open_file(2, false), Ok(NetworkFile::Remote));
}

#[test]
fn audit_open_rejects_the_root_directory() {
    let (filesystem, _cache) = test_filesystem(b"remote");

    assert_eq!(
        filesystem.open_file(ROOT_INODE, false),
        Err(Error::IS_A_DIRECTORY)
    );
}

#[test]
fn audit_lookup_rejects_unknown_parents() {
    let (filesystem, _cache) = test_filesystem(b"remote");

    assert_eq!(filesystem.lookup(999, b"origin.txt"), Err(Error::NOT_FOUND));
    assert_eq!(
        filesystem.lookup(2, b"origin.txt"),
        Err(Error::NOT_A_DIRECTORY)
    );
}

#[test]
fn audit_read_directory_rejects_unknown_inodes() {
    let (filesystem, _cache) = test_filesystem(b"remote");
    let mut sink = VecSink::default();

    assert_eq!(
        filesystem.read_directory(999, 0, 0, &mut sink),
        Err(Error::NOT_FOUND)
    );
    assert_eq!(
        filesystem.read_directory(2, 0, 0, &mut sink),
        Err(Error::NOT_A_DIRECTORY)
    );
}

#[test]
fn audit_remote_reads_and_directory_pages_match_namespace() {
    let (filesystem, _cache) = test_filesystem(b"abcdef");
    assert_eq!(
        filesystem.lookup(ROOT_INODE, b"origin.txt"),
        Ok((2, FileType::File))
    );
    assert_eq!(
        filesystem.lookup(ROOT_INODE, b"downloaded.txt"),
        Ok((3, FileType::File))
    );
    let mut output = [0; 4];
    assert_eq!(filesystem.read(2, 2, &mut output), Ok(4));
    assert_eq!(&output, b"cdef");
    assert_eq!(filesystem.read(2, 99, &mut output), Ok(0));
    assert_eq!(filesystem.open_file(2, false), Ok(NetworkFile::Remote));

    let mut cookie = 0;
    let mut verifier = 0;
    let mut names = Vec::new();
    loop {
        let mut sink = VecSink {
            capacity: 1,
            entries: Vec::new(),
        };
        let result = filesystem
            .read_directory(ROOT_INODE, cookie, verifier, &mut sink)
            .unwrap();
        assert_eq!(result.verifier, 1);
        names.extend(sink.entries);
        cookie = result.next_cookie;
        verifier = result.verifier;
        if result.eof {
            break;
        }
        assert!(result.next_cookie > 0);
    }
    assert_eq!(names, [b"origin.txt".to_vec(), b"downloaded.txt".to_vec()]);
    assert_eq!(filesystem.close_file(2, NetworkFile::Remote), Ok(()));
}
