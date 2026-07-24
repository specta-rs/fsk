use fsbind::{FileType, Filesystem, MemoryFileSystem, ROOT_INODE};

fn filesystem() -> MemoryFileSystem {
    let filesystem = MemoryFileSystem::new();
    let readme = filesystem
        .create(ROOT_INODE, b"hello.txt", FileType::File, 0o644)
        .expect("create example file");
    filesystem
        .write(
            readme,
            0,
            b"Hello from Rust through a Swift FSKit extension.\n",
        )
        .expect("write example file");
    filesystem
}

fsbind::export_filesystem!(filesystem);
