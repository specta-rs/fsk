//! Portable filesystem traits and data model.

use crate::raw::{RawEvent, RawEventMask, RawOutcome};
use std::time::{SystemTime, UNIX_EPOCH};

/// The inode of the filesystem root.
///
/// Every mount starts at this inode. Backends may choose any other inode values,
/// but they must be unique for as long as the kernel can reference them and must
/// not change while the filesystem is mounted. Platform adapters otherwise treat
/// inode values as opaque identifiers.
pub const ROOT_INODE: u64 = 1;

/// A filesystem error represented by `FSK`'s stable, positive error code.
///
/// The portable codes use Darwin's POSIX values for compatibility with the
/// macOS IPC protocol. Native adapters translate values that differ on their
/// platform. Other positive values pass through on Unix, allowing a
/// target-specific backend to return additional native errors.
///
/// Use [`Error::IO`] when a failure has no more specific code, and
/// [`Error::NOT_SUPPORTED`] for operations a backend does not implement. A
/// negative value is never valid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error(pub i32);

impl Error {
    /// Generic I/O failure (`EIO`).
    pub const IO: Self = Self(5);
    /// Invalid argument (`EINVAL`).
    pub const INVALID: Self = Self(22);
    /// Operation is not supported by this backend (`ENOTSUP`).
    pub const NOT_SUPPORTED: Self = Self(45);
    /// Directory is not empty (`ENOTEMPTY`).
    pub const DIRECTORY_NOT_EMPTY: Self = Self(66);
    /// Cached state is no longer valid and the caller should retry (`ESTALE`).
    pub const STALE: Self = Self(70);
}

/// The result type returned by [`Filesystem`] methods.
pub type Result<T> = std::result::Result<T, Error>;

/// The kind of object referenced by an inode.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileType {
    /// A regular file.
    File = 1,
    /// A directory.
    Directory = 2,
    /// A symbolic link.
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

/// The stable attributes of an inode.
///
/// A backend returns this from [`Filesystem::metadata`] and after metadata or
/// content changes. Timestamps are nanoseconds since the Unix epoch; use
/// [`now_ns`] to produce them. `allocated_size` may differ from `size` for
/// sparse files and is what the volume reports as consumed space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// The unique inode identifier.
    pub inode: u64,
    /// The inode of the containing directory. The root's parent is itself.
    pub parent: u64,
    /// The logical size in bytes.
    pub size: u64,
    /// The space allocated for the file, which may exceed `size` or be smaller
    /// for sparse files.
    pub allocated_size: u64,
    /// A value that changes when the inode is reused for a different object.
    ///
    /// Adapters pass this to the kernel so stale cached entries can be
    /// invalidated. Incrementing it on every mutation is safe.
    pub generation: u64,
    /// Creation time in nanoseconds since the Unix epoch.
    pub created_ns: u64,
    /// Last modification time in nanoseconds since the Unix epoch.
    pub modified_ns: u64,
    /// Last access time in nanoseconds since the Unix epoch.
    pub accessed_ns: u64,
    /// POSIX permission and type bits, for example `0o755`.
    pub mode: u32,
    /// Owning user ID.
    pub uid: u32,
    /// Owning group ID.
    pub gid: u32,
    /// Number of hard links to the object.
    pub link_count: u32,
    /// The kind of object referenced by this inode.
    pub kind: FileType,
}

/// A partial metadata update passed to [`Filesystem::set_metadata`].
///
/// Only `Some` fields are changed; `None` leaves the current value untouched.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SetMetadata {
    /// New logical size in bytes.
    pub size: Option<u64>,
    /// New POSIX permission bits.
    pub mode: Option<u32>,
    /// New owning user ID.
    pub uid: Option<u32>,
    /// New owning group ID.
    pub gid: Option<u32>,
    /// New access time in nanoseconds since the Unix epoch.
    pub accessed_ns: Option<u64>,
    /// New modification time in nanoseconds since the Unix epoch.
    pub modified_ns: Option<u64>,
}

/// One entry produced while enumerating a directory.
#[derive(Clone, Copy, Debug)]
pub struct DirectoryEntry<'a> {
    /// Platform-native name bytes: arbitrary bytes on Unix and WTF-8 on Windows.
    pub name: &'a [u8],
    /// The inode the name refers to.
    pub inode: u64,
    /// The kind of object the name refers to.
    pub kind: FileType,
    /// Opaque cursor at which the next call should resume.
    pub next_cookie: u64,
}

/// Receives entries without requiring the backend to allocate a result vector.
///
/// Adapters supply a sink sized to the kernel's output buffer. Push entries in
/// the order the backend wants them returned and stop when [`push`](Self::push)
/// returns `false`; the last successfully pushed entry's `next_cookie` is the
/// resume point for the next [`Filesystem::read_directory`] call.
pub trait DirectorySink {
    /// Returns `false` when the caller-provided output buffers are full; the
    /// backend should stop pushing further entries for this call.
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool;
}

/// The result of one [`Filesystem::read_directory`] call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadDirectoryResult {
    /// A value that changes whenever the directory contents change.
    ///
    /// The kernel passes it back on the next call. A backend may return
    /// [`Error::STALE`] when the value no longer matches, forcing the kernel to
    /// restart the enumeration.
    pub verifier: u64,
    /// The cursor to pass as `cookie` to continue this enumeration.
    pub next_cookie: u64,
    /// Whether the enumeration reached the end of the directory.
    pub eof: bool,
}

/// Cross-platform filesystem operations.
///
/// Implement this trait for a read-only filesystem or override the mutation
/// methods to support writes. The five read-only methods ([`statfs`],
/// [`metadata`], [`lookup`], [`read_directory`], and [`read`]) are required;
/// every other operation defaults to [`Error::NOT_SUPPORTED`].
///
/// See the [crate documentation](crate#defining-a-filesystem) for a complete
/// example and the rules every backend must follow.
///
/// # Concurrency
///
/// Adapters are `Send + Sync` and call methods concurrently from multiple
/// kernel worker threads. A backend must synchronize its own state. Blocking is
/// allowed, but long operations delay other requests handled by the same
/// adapter thread.
///
/// # Identity
///
/// Inodes are the backend's primary keys. The kernel may call [`metadata`] and
/// [`read`] with any inode it has seen, without a preceding [`lookup`]. An inode
/// must remain valid until the backend can no longer receive it, and a reused
/// inode must have a different [`Metadata::generation`].
///
/// [`statfs`]: Self::statfs
/// [`metadata`]: Self::metadata
/// [`lookup`]: Self::lookup
/// [`read_directory`]: Self::read_directory
/// [`read`]: Self::read
#[allow(clippy::missing_errors_doc)]
pub trait Filesystem: Send + Sync + 'static {
    /// Reports total and available space and inode counts for the volume.
    fn statfs(&self) -> Result<StatFs>;

    /// Returns the current attributes of `inode`.
    ///
    /// The kernel may pass any inode it has previously observed. Return
    /// [`Error::STALE`] if the inode is no longer valid so callers can retry
    /// from a fresh lookup.
    fn metadata(&self, inode: u64) -> Result<Metadata>;

    /// Applies a partial metadata update and returns the updated attributes.
    ///
    /// The default implementation rejects the call with
    /// [`Error::NOT_SUPPORTED`]. A backend that supports writes usually
    /// implements this alongside [`write`](Self::write).
    fn set_metadata(&self, _inode: u64, _update: SetMetadata) -> Result<Metadata> {
        Err(Error::NOT_SUPPORTED)
    }

    /// Resolves `name` inside `parent` to an inode and object kind.
    ///
    /// `name` contains platform-native bytes and never includes separators. The
    /// names `.` and `..` are handled by the adapter and are not normally passed
    /// to the backend. Return an appropriate error such as `ENOENT` when the
    /// name does not exist.
    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)>;

    /// Enumerates directory entries starting at `cookie`.
    ///
    /// `cookie` is `0` for the first call and otherwise the `next_cookie` from a
    /// previous [`DirectoryEntry`] or [`ReadDirectoryResult`]. Push entries into
    /// `sink` until it returns `false`, then return the resume cursor and whether
    /// the enumeration is complete. `verifier` is the value from the previous
    /// call (`0` on the first call) and may be used to detect a changed
    /// directory.
    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult>;

    /// Reads at most `output.len()` bytes starting at `offset`.
    ///
    /// Returns the number of bytes initialized in `output`. Returning fewer
    /// bytes than requested is allowed; returning `0` signals end of file.
    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize>;

    /// Writes at most `input.len()` bytes starting at `offset`.
    ///
    /// Returns the number of bytes consumed and updates the inode's size when
    /// the write extends it. The default implementation rejects the call with
    /// [`Error::NOT_SUPPORTED`].
    fn write(&self, _inode: u64, _offset: u64, _input: &[u8]) -> Result<usize> {
        Err(Error::NOT_SUPPORTED)
    }

    /// Creates a file or directory named `name` inside `parent` and returns its
    /// inode.
    ///
    /// Symlinks are created through [`create_symlink`](Self::create_symlink).
    /// The default implementation rejects the call with
    /// [`Error::NOT_SUPPORTED`].
    fn create(&self, _parent: u64, _name: &[u8], _kind: FileType, _mode: u32) -> Result<u64> {
        Err(Error::NOT_SUPPORTED)
    }

    /// Creates a symbolic link named `name` inside `parent` pointing at
    /// `target`, and returns its inode.
    ///
    /// `target` is the raw link text and is not resolved. The default
    /// implementation rejects the call with [`Error::NOT_SUPPORTED`].
    fn create_symlink(
        &self,
        _parent: u64,
        _name: &[u8],
        _target: &[u8],
        _mode: u32,
    ) -> Result<u64> {
        Err(Error::NOT_SUPPORTED)
    }

    /// Reads the target of the symbolic link at `inode` into `output`.
    ///
    /// Returns the number of bytes written and does not append a terminator.
    /// The default implementation rejects the call with
    /// [`Error::NOT_SUPPORTED`].
    fn read_symlink(&self, _inode: u64, _output: &mut [u8]) -> Result<usize> {
        Err(Error::NOT_SUPPORTED)
    }

    /// Removes the entry `name` from `parent`.
    ///
    /// `inode` is the entry's inode as last observed by the caller and can be
    /// used to validate that the entry still refers to the same object. Return
    /// [`Error::DIRECTORY_NOT_EMPTY`] when `name` is a non-empty directory. The
    /// default implementation rejects the call with [`Error::NOT_SUPPORTED`].
    fn remove(&self, _parent: u64, _name: &[u8], _inode: u64) -> Result<()> {
        Err(Error::NOT_SUPPORTED)
    }

    /// Renames `source_name` in `source_parent` to `destination_name` in
    /// `destination_parent`.
    ///
    /// `replaced_inode` is the inode already at the destination when the kernel
    /// expects an atomic replacement, or `None` when the destination must not
    /// exist. Replacing a non-empty directory must fail. The default
    /// implementation rejects the call with [`Error::NOT_SUPPORTED`].
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

    /// Flushes pending state to durable storage.
    ///
    /// The default implementation is a no-op, which is correct for filesystems
    /// whose writes are already durable when the operation returns.
    fn synchronize(&self) -> Result<()> {
        Ok(())
    }

    /// Native events the backend wants to observe.
    ///
    /// Returning an empty mask (the default) removes the hook from hot paths:
    /// adapters cache this value once when the volume is mounted and do not
    /// construct [`RawEvent`] values. Set a bit for every operation whose native
    /// request or callback you need, then implement [`raw_event`](Self::raw_event).
    ///
    /// See the [crate documentation](crate#accessing-os-native-events) for
    /// platform-specific details.
    fn raw_event_mask(&self) -> RawEventMask {
        RawEventMask::NONE
    }

    /// Inspect a borrowed native request before generic dispatch.
    ///
    /// Called only for operations selected by [`raw_event_mask`](Self::raw_event_mask),
    /// after the platform adapter has decoded enough request identity to build
    /// the [`RawEvent`] but before it invokes the portable operation. Return
    /// [`RawOutcome::Reject`] to skip the generic operation and fail the request,
    /// or [`RawOutcome::Continue`] to let the adapter dispatch normally.
    ///
    /// Raw pointers and buffers are valid only for this call. Do not store them,
    /// and do not block for long while holding a borrow of platform callback
    /// state.
    fn raw_event(&self, _event: RawEvent<'_>) -> RawOutcome {
        RawOutcome::Continue
    }
}

/// Filesystem-wide capacity and inode statistics.
///
/// Values are advisory: the kernel and tools use them for reporting and for
/// deciding how much data can be written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatFs {
    /// Preferred block size for I/O, in bytes.
    pub block_size: u32,
    /// Optimal transfer size for I/O, in bytes.
    pub io_size: u32,
    /// Total capacity in bytes.
    pub total_bytes: u64,
    /// Bytes allocated to files, including space not currently writable.
    pub free_bytes: u64,
    /// Bytes available to unprivileged callers.
    pub available_bytes: u64,
    /// Total number of inodes.
    pub files: u64,
    /// Number of unused inodes.
    pub free_files: u64,
}

/// Returns the current time as nanoseconds since the Unix epoch.
///
/// Use this for [`Metadata`] timestamps. The value is clamped to `u64`, so it
/// remains valid after the year 2554.
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
