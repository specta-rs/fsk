//! High-performance virtual and network filesystems for macOS, Linux, and Windows.
//!
//! Backends implement [`Filesystem`] (and optionally [`NetworkFilesystem`]) against one
//! portable data model. Platform adapters use `FSKit` on macOS, `FUSE` on Linux, and `ProjFS`
//! on Windows. Opt-in [`raw`] events expose native requests without adding allocations
//! to the default path.
//!
//! This crate provides no filesystem implementation or storage policy. Inode
//! indexing, caching, synchronization, consistency, and persistence belong to
//! the downstream type implementing [`Filesystem`].

#[cfg(target_os = "macos")]
#[path = "../macos/rust/authorization.rs"]
mod authorization;
mod core;
#[cfg(target_os = "macos")]
#[path = "../macos/rust/ffi.rs"]
mod ffi;
#[cfg(target_os = "macos")]
#[path = "../macos/rust/ipc.rs"]
mod ipc;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
#[path = "../macos/rust/mount.rs"]
mod mount;
mod network;
pub mod raw;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
pub use authorization::{AuthorizationStatus, authorization_status, open_authorization_app};
pub use core::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, ROOT_INODE,
    ReadDirectoryResult, Result, SetMetadata, StatFs, now_ns,
};
#[cfg(target_os = "macos")]
pub use ffi::{FskAttributes, FskDirEntry, FskSetAttributes, FskStatFs};
#[cfg(target_os = "linux")]
pub use linux::{MountOptions, MountSession};
#[cfg(target_os = "macos")]
pub use mount::{MountOptions, MountSession};
pub use network::{
    KernelCachePolicy, LocalFile, Mount, NetworkConfiguration, NetworkFile, NetworkFilesystem,
};
#[cfg(target_os = "windows")]
pub use windows::{MountOptions, MountSession};

/// Version of the C ABI implemented by this crate.
pub const ABI_VERSION: u32 = 2;

#[cfg(all(test, target_os = "macos"))]
mod test_support {
    use crate::{
        DirectorySink, Error, FileType, Filesystem, Metadata, ROOT_INODE, ReadDirectoryResult,
        Result, StatFs,
    };

    #[derive(Default)]
    pub(crate) struct TestFilesystem {
        entry: bool,
    }

    impl TestFilesystem {
        pub(crate) const fn with_entry() -> Self {
            Self { entry: true }
        }
    }

    impl Filesystem for TestFilesystem {
        fn statfs(&self) -> Result<StatFs> {
            Ok(StatFs {
                block_size: 4096,
                io_size: 131_072,
                total_bytes: 4096,
                free_bytes: 4096,
                available_bytes: 4096,
                files: 2,
                free_files: u64::MAX - 2,
            })
        }

        fn metadata(&self, inode: u64) -> Result<Metadata> {
            let kind = match inode {
                ROOT_INODE => FileType::Directory,
                2 if self.entry => FileType::File,
                _ => return Err(Error(2)),
            };
            Ok(Metadata {
                inode,
                parent: ROOT_INODE,
                size: 0,
                allocated_size: 0,
                generation: 1,
                created_ns: 0,
                modified_ns: 0,
                accessed_ns: 0,
                mode: if kind == FileType::Directory {
                    0o755
                } else {
                    0o644
                },
                uid: 0,
                gid: 0,
                link_count: 1,
                kind,
            })
        }

        fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)> {
            if self.entry && parent == ROOT_INODE && name == b"entry" {
                Ok((2, FileType::File))
            } else {
                Err(Error(2))
            }
        }

        fn read_directory(
            &self,
            _inode: u64,
            _cookie: u64,
            _verifier: u64,
            _sink: &mut dyn DirectorySink,
        ) -> Result<ReadDirectoryResult> {
            Ok(ReadDirectoryResult {
                verifier: 1,
                next_cookie: 0,
                eof: true,
            })
        }

        fn read(&self, _inode: u64, _offset: u64, _output: &mut [u8]) -> Result<usize> {
            Ok(0)
        }
    }
}

/// Exports a backend factory using the ABI expected by `FSKExtension`.
///
/// The factory is called once per mounted volume. Panics never cross the FFI boundary;
/// they are converted to `EIO`.
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! export_filesystem {
    ($factory:path) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn fsk_volume_create(
            _resource: *const u8,
            _resource_length: usize,
            error_out: *mut i32,
        ) -> *mut ::core::ffi::c_void {
            if !error_out.is_null() {
                unsafe { *error_out = 0 };
            }
            $crate::__private::create_volume($factory())
        }
    };
}

/// Exports a network backend factory using the ABI expected by `FSKExtension`.
///
/// The factory receives the URL used to mount the volume and returns either a
/// network filesystem or a positive Darwin errno.
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! export_network_filesystem {
    ($factory:path) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn fsk_volume_create(
            resource: *const u8,
            resource_length: usize,
            error_out: *mut i32,
        ) -> *mut ::core::ffi::c_void {
            unsafe {
                $crate::__private::create_network_volume_from_raw(
                    resource,
                    resource_length,
                    error_out,
                    $factory,
                )
            }
        }
    };
}

#[cfg(target_os = "macos")]
#[doc(hidden)]
pub mod __private {
    pub use crate::ffi::{create_network_volume_from_raw, create_volume};
}
