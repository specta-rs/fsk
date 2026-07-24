use crate::{Filesystem, Result};
use std::path::{Path, PathBuf};

/// The resource supplied by macOS when mounting a network filesystem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mount {
    resource_url: Vec<u8>,
}

impl Mount {
    pub(crate) fn new(resource_url: Vec<u8>) -> Self {
        Self { resource_url }
    }

    /// Returns the URL as its original UTF-8 bytes.
    #[must_use]
    pub fn resource_url_bytes(&self) -> &[u8] {
        &self.resource_url
    }

    /// Returns the URL as UTF-8.
    ///
    /// # Errors
    ///
    /// Returns an error if the resource URL isn't valid UTF-8.
    pub fn resource_url(&self) -> std::result::Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.resource_url)
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
    /// for Foundation to open it.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, generation: u64) -> Self {
        Self {
            path: path.into(),
            generation,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A backend-defined value that changes when the local contents change.
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
    Local(LocalFile),
}

/// Additional lifecycle hooks for lazy network filesystems.
///
/// Namespace operations remain demand-driven methods on the parent
/// [`Filesystem`] trait. The open/close lease allows explicitly downloaded files
/// to use local I/O without giving Swift ownership of cache eviction.
pub trait NetworkFilesystem: Filesystem {
    fn network_configuration(&self) -> NetworkConfiguration {
        NetworkConfiguration::default()
    }

    /// Selects the data source for an open file.
    ///
    /// Returning [`NetworkFile::Remote`] leaves reads lazy. Returning
    /// [`NetworkFile::Local`] guarantees the path remains valid until `close_file`.
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
    /// cleanup.
    ///
    /// # Errors
    ///
    /// Returns a positive Darwin errno if lease cleanup fails.
    fn close_file(&self, _inode: u64, _file: NetworkFile) -> Result<()> {
        Ok(())
    }
}
