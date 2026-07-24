//! High-performance virtual and network filesystems for macOS, Linux, and Windows.
//!
//! Backends implement [`Filesystem`] (and optionally [`NetworkFilesystem`]) against one
//! portable data model. Platform adapters use `FSKit` on macOS, `FUSE` on Linux, and `ProjFS`
//! on Windows. Opt-in [`raw`] events expose native requests without adding allocations
//! to the default path.

#[cfg(target_os = "macos")]
mod authorization;
mod core;
#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod ipc;
#[cfg(target_os = "linux")]
mod linux;
mod memory;
#[cfg(target_os = "macos")]
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
pub use ffi::{FsbAttributes, FsbDirEntry, FsbSetAttributes, FsbStatFs};
#[cfg(target_os = "linux")]
pub use linux::{MountOptions, MountSession};
pub use memory::MemoryFileSystem;
#[cfg(target_os = "macos")]
pub use mount::{MountOptions, MountSession};
pub use network::{
    KernelCachePolicy, LocalFile, Mount, NetworkConfiguration, NetworkFile, NetworkFilesystem,
};
#[cfg(target_os = "windows")]
pub use windows::{MountOptions, MountSession};

/// Version of the C ABI implemented by this crate.
pub const ABI_VERSION: u32 = 2;

/// Exports a backend factory using the ABI expected by `FSBindExtension`.
///
/// The factory is called once per mounted volume. Panics never cross the FFI boundary;
/// they are converted to `EIO`.
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! export_filesystem {
    ($factory:path) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn fsb_volume_create(
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

/// Exports a network backend factory using the ABI expected by `FSBindExtension`.
///
/// The factory receives the URL used to mount the volume and returns either a
/// network filesystem or a positive Darwin errno.
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! export_network_filesystem {
    ($factory:path) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn fsb_volume_create(
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
