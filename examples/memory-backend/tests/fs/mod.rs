use super::*;
use fsk::DirectorySink;
use std::sync::Arc;

#[derive(Default)]
struct VecSink {
    capacity: usize,
    entries: Vec<(Vec<u8>, u64, FileType, u64)>,
}

impl DirectorySink for VecSink {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
        if self.entries.len() == self.capacity {
            return false;
        }
        self.entries.push((
            entry.name.to_vec(),
            entry.inode,
            entry.kind,
            entry.next_cookie,
        ));
        true
    }
}

#[test]
fn sparse_write_reads_zeroes_and_updates_metadata() {
    let filesystem = MemoryFileSystem::new();
    let inode = filesystem
        .create(ROOT_INODE, b"data", FileType::File, 0o640)
        .unwrap();
    assert_eq!(filesystem.write(inode, 4, b"rust").unwrap(), 4);

    let mut output = [0xff; 8];
    assert_eq!(filesystem.read(inode, 0, &mut output).unwrap(), 8);
    assert_eq!(&output, b"\0\0\0\0rust");
    let metadata = filesystem.metadata(inode).unwrap();
    assert_eq!(metadata.size, 8);
    assert_eq!(metadata.allocated_size, BLOCK_SIZE);
    assert_eq!(metadata.mode, 0o640);
}

#[test]
fn directory_cookies_resume_without_rescanning() {
    let filesystem = MemoryFileSystem::new();
    for name in [b"alpha".as_slice(), b"beta", b"gamma"] {
        filesystem
            .create(ROOT_INODE, name, FileType::File, 0o644)
            .unwrap();
    }

    let mut first = VecSink {
        capacity: 2,
        ..VecSink::default()
    };
    let first_result = filesystem
        .read_directory(ROOT_INODE, 0, 0, &mut first)
        .unwrap();
    assert_eq!(first.entries.len(), 2);
    assert!(!first_result.eof);
    assert_eq!(first_result.next_cookie, 2);

    let mut second = VecSink {
        capacity: 2,
        ..VecSink::default()
    };
    let second_result = filesystem
        .read_directory(
            ROOT_INODE,
            first_result.next_cookie,
            first_result.verifier,
            &mut second,
        )
        .unwrap();
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].0, b"gamma");
    assert!(second_result.eof);
}

#[test]
fn mutation_invalidates_an_active_directory_enumeration() {
    let filesystem = MemoryFileSystem::new();
    filesystem
        .create(ROOT_INODE, b"before", FileType::File, 0o644)
        .unwrap();
    let mut sink = VecSink {
        capacity: 1,
        ..VecSink::default()
    };
    let result = filesystem
        .read_directory(ROOT_INODE, 0, 0, &mut sink)
        .unwrap();
    filesystem
        .create(ROOT_INODE, b"after", FileType::File, 0o644)
        .unwrap();
    let error = filesystem
        .read_directory(ROOT_INODE, result.next_cookie, result.verifier, &mut sink)
        .unwrap_err();
    assert_eq!(error, Error::STALE);
}

#[test]
fn cross_directory_rename_updates_parent_and_lookup_indexes() {
    let filesystem = MemoryFileSystem::new();
    let source = filesystem
        .create(ROOT_INODE, b"source", FileType::Directory, 0o755)
        .unwrap();
    let destination = filesystem
        .create(ROOT_INODE, b"destination", FileType::Directory, 0o755)
        .unwrap();
    let file = filesystem
        .create(source, b"old", FileType::File, 0o644)
        .unwrap();

    filesystem
        .rename(source, b"old", destination, b"new", None)
        .unwrap();
    assert_eq!(
        filesystem.lookup(destination, b"new").unwrap(),
        (file, FileType::File)
    );
    assert_eq!(
        filesystem.lookup(source, b"old").unwrap_err(),
        Error::NOT_FOUND
    );
    assert_eq!(filesystem.metadata(file).unwrap().parent, destination);
}

#[test]
fn nonempty_directory_cannot_be_removed() {
    let filesystem = MemoryFileSystem::new();
    let directory = filesystem
        .create(ROOT_INODE, b"directory", FileType::Directory, 0o755)
        .unwrap();
    filesystem
        .create(directory, b"child", FileType::File, 0o644)
        .unwrap();
    assert_eq!(
        filesystem
            .remove(ROOT_INODE, b"directory", directory)
            .unwrap_err(),
        Error::DIRECTORY_NOT_EMPTY
    );
}

#[test]
fn directory_cannot_be_moved_inside_itself() {
    let filesystem = MemoryFileSystem::new();
    let parent = filesystem
        .create(ROOT_INODE, b"parent", FileType::Directory, 0o755)
        .unwrap();
    let child = filesystem
        .create(parent, b"child", FileType::Directory, 0o755)
        .unwrap();
    assert_eq!(
        filesystem
            .rename(ROOT_INODE, b"parent", child, b"cycle", None)
            .unwrap_err(),
        Error::INVALID
    );
}

#[test]
fn rename_cannot_replace_a_nonempty_directory() {
    let filesystem = MemoryFileSystem::new();
    let source = filesystem
        .create(ROOT_INODE, b"source", FileType::Directory, 0o755)
        .unwrap();
    let destination = filesystem
        .create(ROOT_INODE, b"destination", FileType::Directory, 0o755)
        .unwrap();
    filesystem
        .create(destination, b"child", FileType::File, 0o644)
        .unwrap();
    assert_eq!(
        filesystem
            .rename(
                ROOT_INODE,
                b"source",
                ROOT_INODE,
                b"destination",
                Some(destination),
            )
            .unwrap_err(),
        Error::DIRECTORY_NOT_EMPTY
    );
    assert_eq!(filesystem.lookup(ROOT_INODE, b"source").unwrap().0, source);
}

#[test]
fn same_name_rename_is_a_noop_even_with_replacement_inode() {
    let filesystem = MemoryFileSystem::new();
    let inode = filesystem
        .create(ROOT_INODE, b"same", FileType::File, 0o644)
        .unwrap();

    filesystem
        .rename(ROOT_INODE, b"same", ROOT_INODE, b"same", Some(inode))
        .unwrap();

    assert_eq!(
        filesystem.lookup(ROOT_INODE, b"same").unwrap(),
        (inode, FileType::File)
    );
    assert_eq!(filesystem.metadata(inode).unwrap().inode, inode);
}

#[test]
fn capacity_is_enforced_across_files_and_failed_growth_is_atomic() {
    let filesystem = MemoryFileSystem::with_capacity(BLOCK_SIZE * 2);
    let first = filesystem
        .create(ROOT_INODE, b"first", FileType::File, 0o644)
        .unwrap();
    let second = filesystem
        .create(ROOT_INODE, b"second", FileType::File, 0o644)
        .unwrap();
    filesystem.write(first, 0, b"a").unwrap();
    filesystem.write(second, 0, b"b").unwrap();

    assert_eq!(
        filesystem.write(first, BLOCK_SIZE, b"x").unwrap_err(),
        Error::NO_SPACE
    );
    assert_eq!(filesystem.metadata(first).unwrap().size, 1);
    assert_eq!(filesystem.statfs().unwrap().free_bytes, 0);

    filesystem
        .set_metadata(
            second,
            SetMetadata {
                size: Some(0),
                ..SetMetadata::default()
            },
        )
        .unwrap();
    filesystem.write(first, BLOCK_SIZE, b"x").unwrap();
    assert_eq!(filesystem.metadata(first).unwrap().size, BLOCK_SIZE + 1);
}

#[test]
fn concurrent_growth_cannot_overcommit_capacity() {
    let filesystem = Arc::new(MemoryFileSystem::with_capacity(BLOCK_SIZE));
    let first = filesystem
        .create(ROOT_INODE, b"first", FileType::File, 0o644)
        .unwrap();
    let second = filesystem
        .create(ROOT_INODE, b"second", FileType::File, 0o644)
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = [first, second]
        .into_iter()
        .map(|inode| {
            let filesystem = Arc::clone(&filesystem);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                filesystem.write(inode, 0, b"x")
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(Error::NO_SPACE))
            .count(),
        1
    );
    assert_eq!(filesystem.statfs().unwrap().free_bytes, 0);
}

#[test]
fn independent_files_support_concurrent_io() {
    let filesystem = Arc::new(MemoryFileSystem::new());
    let inodes: Vec<_> = (0..8)
        .map(|index| {
            filesystem
                .create(
                    ROOT_INODE,
                    format!("file-{index}").as_bytes(),
                    FileType::File,
                    0o644,
                )
                .unwrap()
        })
        .collect();
    let threads: Vec<_> = inodes
        .into_iter()
        .enumerate()
        .map(|(index, inode)| {
            let filesystem = Arc::clone(&filesystem);
            std::thread::spawn(move || {
                let bytes = vec![u8::try_from(index).unwrap(); 64 * 1024];
                filesystem.write(inode, 0, &bytes).unwrap();
                let mut output = vec![0; bytes.len()];
                filesystem.read(inode, 0, &mut output).unwrap();
                assert_eq!(output, bytes);
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}
