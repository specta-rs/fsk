mod memory;

use fsbind::{FileType, Filesystem, ROOT_INODE};
pub use memory::MemoryFileSystem;

/// Build the example volume.
///
/// # Panics
///
/// Panics only if the fixed starter file cannot be created in a fresh volume.
#[must_use]
pub fn filesystem() -> MemoryFileSystem {
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
