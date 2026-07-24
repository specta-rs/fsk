//! Portable filesystem traits and data model.

use crate::raw::{RawEvent, RawEventMask, RawOutcome};
use std::time::{SystemTime, UNIX_EPOCH};

/// The root inode. Platform adapters treat inode values as opaque identifiers.
pub const ROOT_INODE: u64 = 1;

/// A filesystem error represented by `FSK`'s stable, positive error code.
///
/// The portable codes use Darwin's POSIX values for compatibility with the
/// macOS IPC protocol. Native adapters translate values that differ on their
/// platform. Other positive values pass through on Unix, allowing a
/// target-specific backend to return additional native errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error(pub i32);

impl Error {
    pub const IO: Self = Self(5);
    pub const INVALID: Self = Self(22);
    pub const NOT_SUPPORTED: Self = Self(45);
    pub const DIRECTORY_NOT_EMPTY: Self = Self(66);
    pub const STALE: Self = Self(70);
}

pub type Result<T> = std::result::Result<T, Error>;

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SetMetadata {
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub accessed_ns: Option<u64>,
    pub modified_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct DirectoryEntry<'a> {
    /// Platform-native name bytes: arbitrary bytes on Unix and WTF-8 on Windows.
    pub name: &'a [u8],
    pub inode: u64,
    pub kind: FileType,
    pub next_cookie: u64,
}

pub trait DirectorySink {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadDirectoryResult {
    pub verifier: u64,
    pub next_cookie: u64,
    pub eof: bool,
}

/// Cross-platform filesystem operations.
///
/// The five read-only methods are required. All mutation methods default to
/// `ENOTSUP`. Adapters may call methods concurrently.
#[allow(clippy::missing_errors_doc)]
pub trait Filesystem: Send + Sync + 'static {
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
    /// Reads at most `output.len()` bytes and returns the initialized byte count.
    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize>;
    /// Writes at most `input.len()` bytes and returns the consumed byte count.
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
    /// Reads at most `output.len()` bytes and returns the initialized byte count.
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

    /// Native events the backend wants to observe.
    ///
    /// Returning an empty mask (the default) removes the hook from hot paths:
    /// adapters cache this value once and do not construct [`RawEvent`] values.
    fn raw_event_mask(&self) -> RawEventMask {
        RawEventMask::NONE
    }

    /// Inspect a borrowed native request before generic dispatch.
    ///
    /// Raw pointers and buffers are valid only for this call. `Reject` skips the
    /// generic operation; `Continue` proceeds without changing its fast path.
    fn raw_event(&self, _event: RawEvent<'_>) -> RawOutcome {
        RawOutcome::Continue
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

#[must_use]
pub fn now_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

pub(crate) fn checked_io_count(count: usize, capacity: usize) -> Result<usize> {
    if count <= capacity {
        Ok(count)
    } else {
        Err(Error::INVALID)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_io_counts_beyond_the_supplied_buffer() {
        assert_eq!(checked_io_count(4, 4), Ok(4));
        assert_eq!(checked_io_count(5, 4), Err(Error::INVALID));
    }
}
