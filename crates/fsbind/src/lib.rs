//! A small, allocation-conscious interface for implementing macOS `FSKit` volumes in Rust.
//!
//! Backends implement [`FileSystem`]. The [`export_filesystem!`] macro turns a factory
//! into the stable C ABI consumed by the Swift `FSKit` extension.

mod ffi;
mod memory;

use std::time::{SystemTime, UNIX_EPOCH};

pub use ffi::{FsbAttributes, FsbDirEntry, FsbSetAttributes, FsbStatFs};
pub use memory::MemoryFileSystem;

/// Version of the C ABI implemented by this crate.
pub const ABI_VERSION: u32 = 1;

/// The root inode. `FSKit` treats inode values as opaque, except that zero is invalid.
pub const ROOT_INODE: u64 = 1;

/// An error represented as a positive POSIX errno.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error(pub i32);

impl Error {
    pub const IO: Self = Self(libc_errno::EIO);
    pub const INVALID: Self = Self(libc_errno::EINVAL);
    pub const NOT_SUPPORTED: Self = Self(libc_errno::ENOTSUP);
}

pub type Result<T> = std::result::Result<T, Error>;

/// File kinds supported by the portable bridge.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileType {
    File = 1,
    Directory = 2,
    Symlink = 3,
}

impl TryFrom<u8> for FileType {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::File),
            2 => Ok(Self::Directory),
            3 => Ok(Self::Symlink),
            _ => Err(Error::INVALID),
        }
    }
}

/// Metadata returned without allocating.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    pub inode: u64,
    pub parent: u64,
    pub size: u64,
    pub allocated_size: u64,
    pub generation: u64,
    pub created_ns: u64,
    pub modified_ns: u64,
    pub accessed_ns: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub link_count: u32,
    pub kind: FileType,
}

/// A partial metadata update. `None` leaves the corresponding field unchanged.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SetMetadata {
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub accessed_ns: Option<u64>,
    pub modified_ns: Option<u64>,
}

/// A borrowed entry emitted during directory enumeration.
#[derive(Clone, Copy, Debug)]
pub struct DirectoryEntry<'a> {
    pub name: &'a [u8],
    pub inode: u64,
    pub kind: FileType,
    /// Opaque cursor at which the next call should resume.
    pub next_cookie: u64,
}

/// Receives entries without requiring the backend to allocate a result vector.
pub trait DirectorySink {
    /// Returns `false` when the caller-provided output buffers are full.
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadDirectoryResult {
    pub verifier: u64,
    pub next_cookie: u64,
    pub eof: bool,
}

/// Operations used by the Swift `FSKit` adapter.
///
/// Methods may run concurrently. Implementations must be thread-safe and must return
/// positive POSIX errno values in [`Error`].
#[allow(clippy::missing_errors_doc)]
pub trait FileSystem: Send + Sync + 'static {
    fn statfs(&self) -> Result<StatFs>;
    fn metadata(&self, inode: u64) -> Result<Metadata>;
    fn set_metadata(&self, _inode: u64, _update: SetMetadata) -> Result<Metadata> {
        Err(Error::NOT_SUPPORTED)
    }
    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)>;
    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult>;
    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize>;
    fn write(&self, _inode: u64, _offset: u64, _input: &[u8]) -> Result<usize> {
        Err(Error::NOT_SUPPORTED)
    }
    fn create(&self, _parent: u64, _name: &[u8], _kind: FileType, _mode: u32) -> Result<u64> {
        Err(Error::NOT_SUPPORTED)
    }
    fn create_symlink(
        &self,
        _parent: u64,
        _name: &[u8],
        _target: &[u8],
        _mode: u32,
    ) -> Result<u64> {
        Err(Error::NOT_SUPPORTED)
    }
    fn read_symlink(&self, _inode: u64, _output: &mut [u8]) -> Result<usize> {
        Err(Error::NOT_SUPPORTED)
    }
    fn remove(&self, _parent: u64, _name: &[u8], _inode: u64) -> Result<()> {
        Err(Error::NOT_SUPPORTED)
    }
    fn rename(
        &self,
        _source_parent: u64,
        _source_name: &[u8],
        _destination_parent: u64,
        _destination_name: &[u8],
        _replaced_inode: Option<u64>,
    ) -> Result<()> {
        Err(Error::NOT_SUPPORTED)
    }
    fn synchronize(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatFs {
    pub block_size: u32,
    pub io_size: u32,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub files: u64,
    pub free_files: u64,
}

/// Nanoseconds since the Unix epoch, saturated for clocks before the epoch.
#[must_use]
pub fn now_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

/// Exports a backend factory using the ABI expected by `FSBindExtension`.
///
/// The factory is called once per mounted volume. Panics never cross the FFI boundary;
/// they are converted to `EIO`.
#[macro_export]
macro_rules! export_filesystem {
    ($factory:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn fsb_volume_create() -> *mut ::core::ffi::c_void {
            $crate::__private::create_volume($factory())
        }
    };
}

#[doc(hidden)]
pub mod __private {
    pub use crate::ffi::create_volume;
}

mod libc_errno {
    pub const EIO: i32 = 5;
    pub const EINVAL: i32 = 22;
    pub const ENOTSUP: i32 = 45;
}
