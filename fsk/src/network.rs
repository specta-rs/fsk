//! Network filesystem interface and cache policy.

use crate::{Filesystem, Result};
use std::path::{Path, PathBuf};

/// Returns a directory where materialized [`LocalFile`] paths can live.
///
/// On macOS the backend and the `FSKit` extension run in different processes,
/// and the extension is sandboxed to the `group.dev.specta.fsk` App Group. A
/// local file in the process temporary directory, or anywhere else outside the
/// app group, can't be opened by the extension. This function returns the
/// shared `Caches` directory inside that app group instead.
///
/// On other platforms this returns the process temporary directory.
///
/// The directory is created if needed. Backends should create their own
/// subdirectory so independent mounts don't collide.
///
/// # Errors
///
/// Returns an error if the directory can't be created or the home directory
/// can't be located.
pub fn local_file_directory() -> std::io::Result<PathBuf> {
    imp::local_file_directory()
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    pub(super) fn local_file_directory() -> std::io::Result<PathBuf> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not locate the user home directory",
            )
        })?;
        let path = PathBuf::from(home).join("Library/Group Containers/group.dev.specta.fsk/Caches");
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::path::PathBuf;

    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn local_file_directory() -> std::io::Result<PathBuf> {
        Ok(std::env::temp_dir())
    }
}

/// The kernel caching behavior requested by a network backend.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KernelCachePolicy {
    /// Don't retain file data in the kernel.
    None = 0,
    /// Cache metadata, but route file data back through the backend.
    #[default]
    MetadataOnly = 1,
    /// Permit clean read data to remain cached.
    Read = 2,
    /// Permit read and write caching when the backend maintains coherency.
    ReadWrite = 3,
}

/// Network-specific behavior communicated to the Swift `FSKit` layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetworkConfiguration {
    /// Whether namespace and file data are resolved only when requested.
    pub lazy: bool,
    /// Whether [`NetworkFile::Local`] may be returned by [`NetworkFilesystem::open_file`].
    pub supports_local_files: bool,
    /// The desired kernel caching policy.
    pub kernel_cache_policy: KernelCachePolicy,
}

impl Default for NetworkConfiguration {
    fn default() -> Self {
        Self {
            lazy: true,
            supports_local_files: true,
            kernel_cache_policy: KernelCachePolicy::MetadataOnly,
        }
    }
}

/// A locally materialized file managed by a network backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFile {
    path: PathBuf,
    generation: u64,
}

impl LocalFile {
    /// Creates a local-file lease.
    ///
    /// The path must remain readable until the matching
    /// [`NetworkFilesystem::close_file`] call completes and must be valid UTF-8
    /// for Foundation to open it. On macOS the path must also be inside the
    /// sandboxed extension's App Group; use [`local_file_directory`] to find a
    /// suitable directory.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, generation: u64) -> Self {
        Self {
            path: path.into(),
            generation,
        }
    }

    /// Path to the materialized file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A backend-defined value that changes when the local contents change.
    ///
    /// Pass the value to [`LocalFile::new`]. The adapter forwards it to the
    /// kernel so cached data can be invalidated when the local file changes.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// The data source selected when `FSKit` opens a network file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NetworkFile {
    /// Keep reads lazy and call [`Filesystem::read`].
    Remote,
    /// Let Swift use `pread` against a backend-managed local file.
    ///
    /// The backend must keep the path readable until
    /// [`NetworkFilesystem::close_file`] is called.
    Local(LocalFile),
}

/// Additional lifecycle hooks for lazy network filesystems.
///
/// A network backend implements [`Filesystem`] for namespace and data
/// operations and this trait for the open/close lease that lets already
/// downloaded files be read locally.
///
/// # Lazy versus materialized
///
/// With [`NetworkConfiguration::lazy`] enabled, the adapter resolves metadata
/// and contents only when the kernel asks for them. Returning
/// [`NetworkFile::Local`] from [`open_file`](Self::open_file) hands the kernel a
/// path to a fully materialized file; subsequent reads bypass
/// [`Filesystem::read`]. Return [`NetworkFile::Remote`] to keep reads flowing
/// through the backend.
///
/// Ownership of cached files stays with the backend. The adapter only calls
/// [`close_file`](Self::close_file) when the kernel is done with a lease.
///
/// See the [crate documentation](crate#defining-a-network-filesystem) for an
/// example.
pub trait NetworkFilesystem: Filesystem {
    /// Describes laziness, local-file support, and the desired kernel cache
    /// policy.
    ///
    /// The adapter reads this once when the volume starts. The default enables
    /// lazy lookup, local files, and metadata-only caching.
    fn network_configuration(&self) -> NetworkConfiguration {
        NetworkConfiguration::default()
    }

    /// Selects the data source for an open file.
    ///
    /// Returning [`NetworkFile::Remote`] leaves reads lazy. Returning
    /// [`NetworkFile::Local`] guarantees the path remains valid until
    /// [`close_file`](Self::close_file).
    ///
    /// The default implementation always returns [`NetworkFile::Remote`]. Set
    /// [`NetworkConfiguration::supports_local_files`] to `false` if this backend
    /// never returns a local file.
    ///
    /// # Errors
    ///
    /// Returns a positive Darwin errno if the file can't be opened.
    fn open_file(&self, _inode: u64, _writable: bool) -> Result<NetworkFile> {
        Ok(NetworkFile::Remote)
    }

    /// Releases a file lease after Swift has closed any local descriptor.
    ///
    /// This is a suitable point to update last-access state or schedule cache
    /// cleanup. The default implementation is a no-op.
    ///
    /// # Errors
    ///
    /// Returns a positive Darwin errno if lease cleanup fails.
    fn close_file(&self, _inode: u64, _file: NetworkFile) -> Result<()> {
        Ok(())
    }
}
