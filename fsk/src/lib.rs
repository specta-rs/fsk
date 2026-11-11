#![deny(missing_docs)]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/specta-rs/fsk/main/assets/app-icon-light.svg",
    html_favicon_url = "https://raw.githubusercontent.com/specta-rs/fsk/main/assets/app-icon-light.svg"
)]

//! High-performance virtual and network filesystems for macOS, Linux, and Windows.
//!
//! `fsk` connects portable Rust backends to each operating system's native
//! filesystem framework. A backend implements [`Filesystem`] (and optionally
//! [`NetworkFilesystem`]) once, without platform `cfg` code, and is mounted with
//! a platform [`MountSession`].
//!
//! Platform adapters use `FSKit` on macOS, `FUSE` on Linux, and `ProjFS` on
//! Windows. The crate provides no filesystem implementation or storage policy:
//! inode indexing, caching, synchronization, consistency, and persistence belong
//! to the backend.
//!
//! # Defining a filesystem
//!
//! Implement [`Filesystem`] on a `Send + Sync + 'static` type. Five read-only
//! methods are required; every mutation method has a default that returns
//! [`Error::NOT_SUPPORTED`], so a read-only backend only implements what it can
//! serve. The following example exposes a single file and can be mounted:
//!
//! ```no_run
//! use fsk::{
//!     DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata,
//!     ReadDirectoryResult, Result, ROOT_INODE, StatFs,
//! };
//!
//! const HELLO_INODE: u64 = 2;
//! const HELLO: &[u8] = b"hello, filesystem!\n";
//!
//! struct HelloFilesystem;
//!
//! impl Filesystem for HelloFilesystem {
//!     fn statfs(&self) -> Result<StatFs> {
//!         Ok(StatFs {
//!             block_size: 4096,
//!             io_size: 4096,
//!             total_bytes: 4096,
//!             free_bytes: 0,
//!             available_bytes: 0,
//!             files: 2,
//!             free_files: 0,
//!         })
//!     }
//!
//!     fn metadata(&self, inode: u64) -> Result<Metadata> {
//!         let (kind, size) = match inode {
//!             ROOT_INODE => (FileType::Directory, 0),
//!             HELLO_INODE => (FileType::File, HELLO.len() as u64),
//!             _ => return Err(Error(2)), // ENOENT
//!         };
//!         Ok(Metadata {
//!             inode,
//!             parent: ROOT_INODE,
//!             size,
//!             allocated_size: size,
//!             generation: 1,
//!             created_ns: 0,
//!             modified_ns: 0,
//!             accessed_ns: 0,
//!             mode: if kind == FileType::Directory { 0o755 } else { 0o644 },
//!             uid: 0,
//!             gid: 0,
//!             link_count: 1,
//!             kind,
//!         })
//!     }
//!
//!     fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)> {
//!         if parent == ROOT_INODE && name == b"hello.txt" {
//!             Ok((HELLO_INODE, FileType::File))
//!         } else {
//!             Err(Error(2)) // ENOENT
//!         }
//!     }
//!
//!     fn read_directory(
//!         &self,
//!         inode: u64,
//!         cookie: u64,
//!         verifier: u64,
//!         sink: &mut dyn DirectorySink,
//!     ) -> Result<ReadDirectoryResult> {
//!         if inode != ROOT_INODE {
//!             return Err(Error(2)); // ENOENT
//!         }
//!         if verifier != 0 && verifier != 1 {
//!             return Err(Error::STALE);
//!         }
//!         if cookie == 0
//!             && !sink.push(DirectoryEntry {
//!                 name: b"hello.txt",
//!                 inode: HELLO_INODE,
//!                 kind: FileType::File,
//!                 next_cookie: 1,
//!             })
//!         {
//!             return Ok(ReadDirectoryResult {
//!                 verifier: 1,
//!                 next_cookie: 0,
//!                 eof: false,
//!             });
//!         }
//!         Ok(ReadDirectoryResult {
//!             verifier: 1,
//!             next_cookie: 1,
//!             eof: true,
//!         })
//!     }
//!
//!     fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize> {
//!         if inode != HELLO_INODE {
//!             return Err(Error(2)); // ENOENT
//!         }
//!         let start = usize::try_from(offset).unwrap_or(usize::MAX).min(HELLO.len());
//!         let count = output.len().min(HELLO.len() - start);
//!         output[..count].copy_from_slice(&HELLO[start..start + count]);
//!         Ok(count)
//!     }
//! }
//!
//! # fn main() -> std::io::Result<()> {
//! let session = fsk::MountSession::mount(HelloFilesystem, "/tmp/fsk-hello")?;
//! println!("mounted at {}", session.mount_path().display());
//! # Ok(())
//! # }
//! ```
//!
//! ## Inodes and identity
//!
//! Inodes are the backend's primary keys. [`ROOT_INODE`] must identify the
//! root, where [`Metadata::parent`] points back at the root itself. Every other
//! inode must be unique and stable while the kernel can reference it. Inodes are
//! never names: renaming an entry changes its parent directory, not its inode.
//!
//! The kernel may call [`Filesystem::metadata`] and [`Filesystem::read`] for any
//! inode it has previously observed, without a preceding
//! [`Filesystem::lookup`]. Increment [`Metadata::generation`] whenever an inode
//! is reused for a different object, and return [`Error::STALE`] when an inode is
//! no longer valid.
//!
//! [`Metadata::size`] is the logical file length; [`Metadata::allocated_size`]
//! is the space the file consumes and may be smaller for sparse files. Timestamps
//! are nanoseconds since the Unix epoch; [`now_ns`] produces them.
//!
//! ## Directory enumeration
//!
//! [`Filesystem::read_directory`] is pull-based. Pass entries to the
//! [`DirectorySink`] in the order they should appear, set each entry's
//! [`DirectoryEntry::next_cookie`], and stop when [`DirectorySink::push`] returns
//! `false`. The returned [`ReadDirectoryResult`] carries the cursor for the next
//! call and whether the directory is exhausted.
//!
//! Cookies are opaque to the kernel and only compared by the backend. A cookie
//! of `0` starts an enumeration. The [`ReadDirectoryResult::verifier`] changes
//! whenever the directory contents change; if the kernel later supplies a
//! verifier the backend no longer recognizes, return [`Error::STALE`] so the
//! enumeration restarts instead of returning duplicates or skipping entries.
//!
//! ## Names
//!
//! Names are platform-native bytes: arbitrary bytes on Unix and WTF-8 on
//! Windows. They never contain a path separator and do not include a terminating
//! NUL. The kernel handles `.` and `..` before calling the backend, so
//! [`Filesystem::lookup`] normally sees only real entries.
//!
//! ## Errors and concurrency
//!
//! Return a positive POSIX error code in [`Error`]. Use [`Error::IO`] when no
//! more specific portable code applies. Adapters call methods concurrently from
//! multiple worker threads, so a backend must synchronize its own state; a
//! request may block without preventing other requests from being dispatched.
//!
//! Only implement a mutation method if writes are supported. Leaving the default
//! is better than pretending to succeed: the kernel otherwise assumes the
//! operation took effect.
//!
//! # Defining a network filesystem
//!
//! A network backend implements [`Filesystem`] for namespace and data
//! operations and [`NetworkFilesystem`] for lazy-open behavior. Mount it with
//! [`MountSession::mount_network`].
//!
//! [`NetworkConfiguration`] controls whether the adapter asks the backend for
//! entries and data on demand, whether local files are allowed, and how much the
//! kernel may cache. [`NetworkFilesystem::open_file`] chooses between keeping
//! reads lazy ([`NetworkFile::Remote`]) and handing the kernel a fully
//! materialized file ([`NetworkFile::Local`]):
//!
//! ```ignore
//! use fsk::{
//!     LocalFile, NetworkConfiguration, NetworkFile, NetworkFilesystem, Result,
//! };
//!
//! impl NetworkFilesystem for MyBackend {
//!     fn network_configuration(&self) -> NetworkConfiguration {
//!         NetworkConfiguration {
//!             lazy: true,
//!             supports_local_files: true,
//!             kernel_cache_policy: fsk::KernelCachePolicy::MetadataOnly,
//!         }
//!     }
//!
//!     fn open_file(&self, inode: u64, writable: bool) -> Result<NetworkFile> {
//!         if writable {
//!             return Ok(NetworkFile::Remote);
//!         }
//!         match self.cached_download(inode)? {
//!             Some(path) => Ok(NetworkFile::Local(LocalFile::new(
//!                 path,
//!                 self.generation(inode),
//!             ))),
//!             None => Ok(NetworkFile::Remote),
//!         }
//!     }
//!
//!     fn close_file(&self, inode: u64, _file: NetworkFile) -> Result<()> {
//!         self.release_cache_lease(inode);
//!         Ok(())
//!     }
//! }
//! ```
//!
//! A [`LocalFile`] path must stay readable and unchanged until
//! [`NetworkFilesystem::close_file`] returns. The adapter does not own cache
//! eviction; it only tells the backend when the kernel has finished with a
//! lease. Return [`NetworkFile::Remote`] for writes unless the backend can make
//! them durable itself.
//!
//! # Accessing OS-native events
//!
//! Native events are opt-in. By default [`Filesystem::raw_event_mask`] returns
//! [`raw::RawEventMask::NONE`] and the adapter never constructs an event, so
//! there is no cost on hot paths. To observe native requests, select the
//! operations you need and implement [`Filesystem::raw_event`]:
//!
//! ```ignore
//! use fsk::raw::{
//!     NativeRequest, Operation, Platform, RawEvent, RawEventMask, RawOutcome,
//! };
//! use fsk::Filesystem;
//!
//! impl Filesystem for MyBackend {
//!     // ... the portable operations ...
//!
//!     fn raw_event_mask(&self) -> RawEventMask {
//!         RawEventMask::operation(Operation::Write)
//!             .union(RawEventMask::operation(Operation::Notification))
//!     }
//!
//!     fn raw_event(&self, event: RawEvent<'_>) -> RawOutcome {
//!         match (event.platform, event.operation) {
//!             (Platform::Linux, Operation::Write) => {
//!                 // `buffer` is the exact kernel input for this write.
//!                 let bytes = unsafe { event.buffer.as_ref().unwrap().as_bytes() };
//!                 self.observe_write(bytes);
//!             }
//!             (Platform::Windows, Operation::Notification) => {
//!                 if let NativeRequest::Windows { flags, .. } = event.native {
//!                     self.observe_windows_notification(flags);
//!                 }
//!             }
//!             _ => {}
//!         }
//!         RawOutcome::Continue
//!     }
//! }
//! ```
//!
//! [`raw_event`](Filesystem::raw_event) runs before the portable operation. The
//! event borrows platform callback state and buffers that are valid only for the
//! call. Copy anything you need to retain; never store a
//! [`raw::RawEvent`], [`raw::RawBuffer`], or a pointer from
//! [`raw::NativeRequest`].
//!
//! Return [`raw::RawOutcome::Continue`] to let the adapter dispatch normally, or
//! [`raw::RawOutcome::Reject`] to skip the portable operation and fail the
//! request. Rejecting is useful for auditing or filtering requests without
//! duplicating the backend logic.
//!
//! The mask is read once per mounted volume. Changing it after mounting has no
//! effect until the filesystem is mounted again.
//!
//! ## macOS
//!
//! `FSKit` runs in a separate process, so the hook cannot receive Objective-C or
//! `FSKit` pointers. [`raw::NativeRequest::MacOS`] carries the IPC `request_id`,
//! the `item_id` decoded from the payload, a reserved flags field, and the exact
//! IPC request body in its `payload` field. The payload layout
//! is bridge-internal and may change between SDK versions; prefer the portable
//! operations. Buffer data is not attached to macOS events.
//!
//! ## Linux
//!
//! FUSE callbacks provide [`raw::NativeRequest::Linux`] with the fuser request
//! pointer, the FUSE unique ID, the open file handle, and native open flags.
//! Read and write events also carry a [`raw::RawBuffer`]. The request pointer can
//! be used only synchronously inside the hook.
//!
//! ## Windows
//!
//! `ProjFS` callbacks provide [`raw::NativeRequest::Windows`] with the
//! `PRJ_CALLBACK_DATA` pointer, namespace context, command ID, and callback
//! flags. [`raw::Operation::Notification`] events combine the raw
//! `PRJ_NOTIFICATION` value into `flags` with a bitwise OR;
//! [`raw::Operation::Cancel`] reports a cancelled command. Read callbacks attach
//! a writable [`raw::RawBuffer`].
//!
//! # Mounting
//!
//! [`MountSession`] owns the mount and the service that answers the kernel.
//! Dropping it unmounts the volume and releases file leases:
//!
//! ```no_run
//! # use fsk::{Filesystem, MountSession};
//! # fn demo(filesystem: impl Filesystem) -> std::io::Result<()> {
//! let session = MountSession::mount(filesystem, "/tmp/my-volume")?;
//! println!("mounted at {}", session.mount_path().display());
//! // Keep `session` alive while the filesystem should be available.
//! drop(session);
//! # Ok(())
//! # }
//! ```
//!
//! Use [`MountSession::mount_network`] for a [`NetworkFilesystem`]. Every
//! platform also offers `mount_with_options` and `mount_network_with_options`
//! for platform-specific settings such as read-only mounts, worker counts, and
//! cache policy.
//!
//! # Platform setup
//!
//! [`authorization_status`] reports whether the host is ready, and
//! [`ensure_authorization`] performs the same check while opening any setup UI
//! the platform provides.
//!
//! - **macOS** uses an `FSKit` extension hosted by the companion `FSK.app`. The
//!   app must be installed and its extension enabled in **System Settings →
//!   General → Login Items & Extensions → File System Extensions**. The SDK
//!   exposes `install`, `launch_app`, and `uninstall` to automate that setup.
//!   The app is installed per-user under `~/Library/Application Support`;
//!   `FSK_APP_PATH` overrides that location during development.
//! - **Linux** requires FUSE: `/dev/fuse` must exist and the mounting user must
//!   be allowed to use it.
//! - **Windows** requires the Projected File System optional component
//!   (`Client-ProjFS`). [`ensure_authorization`] opens **Windows Features** when
//!   it is missing; it can also be enabled from an administrator shell with
//!   `Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS`.
//!
//! # Examples
//!
//! The repository contains complete backends in `examples/`:
//!
//! - `examples/memory-backend` — a writable in-memory filesystem with
//!   concurrency, directory cookies, and a seeded file.
//! - `examples/network-backend` — a lazy network filesystem with cached local
//!   files.
//! - `examples/github-filesystem` — a GitHub-backed network filesystem.
//! - `examples/google-drive` — a Google Drive network filesystem.
//!
//! This crate provides no storage implementation or policy. Inode
//! indexing, caching, synchronization, consistency, and persistence belong to
//! the downstream type implementing [`Filesystem`].

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
#[path = "../macos/rust/authorization.rs"]
mod macos_authorization;
#[cfg(target_os = "macos")]
#[path = "../macos/rust/mount.rs"]
mod mount;
mod network;
pub mod raw;
#[cfg(target_os = "windows")]
mod windows;

pub use authorization::{AuthorizationStatus, authorization_status, ensure_authorization};
#[cfg(target_os = "macos")]
pub use macos_authorization::{AppInstallOutcome, install, launch_app, uninstall};
// Re-exporting the deprecated alias would otherwise trip `-D warnings` here.
pub use core::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, ROOT_INODE,
    ReadDirectoryResult, Result, SetMetadata, StatFs, now_ns,
};
#[cfg(target_os = "macos")]
pub use ffi::{FskAttributes, FskDirEntry, FskSetAttributes, FskStatFs};
#[cfg(target_os = "linux")]
pub use linux::{MountOptions, MountSession};
#[cfg(target_os = "macos")]
#[allow(deprecated)]
pub use macos_authorization::open_authorization_app;
#[cfg(target_os = "macos")]
pub use mount::{MountOptions, MountSession};
pub use network::{
    KernelCachePolicy, LocalFile, Mount, NetworkConfiguration, NetworkFile, NetworkFilesystem,
    local_file_directory,
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
            unsafe { $crate::__private::create_volume_from_factory(error_out, $factory) }
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
    use crate::{Error, Filesystem};
    use std::{
        ffi::c_void,
        panic::{AssertUnwindSafe, catch_unwind},
        ptr,
    };

    pub use crate::ffi::{create_network_volume_from_raw, create_volume};

    /// Runs a backend factory, converting a panic into `EIO` before it can
    /// unwind through the exported C function.
    ///
    /// # Safety
    ///
    /// `error_out`, when non-null, must be writable for one `i32`.
    pub unsafe fn create_volume_from_factory<T: Filesystem>(
        error_out: *mut i32,
        factory: impl FnOnce() -> T,
    ) -> *mut c_void {
        if let Ok(filesystem) = catch_unwind(AssertUnwindSafe(factory)) {
            if !error_out.is_null() {
                unsafe { ptr::write(error_out, 0) };
            }
            create_volume(filesystem)
        } else {
            if !error_out.is_null() {
                unsafe { ptr::write(error_out, Error::IO.0) };
            }
            ptr::null_mut()
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod export_tests {
    use crate::{__private::create_volume_from_factory, Error, test_support::TestFilesystem};

    #[test]
    fn a_panicking_factory_is_reported_as_io() {
        let mut error = 0;
        // SAFETY: `error` is a writable `i32` for the duration of the call.
        let volume = unsafe {
            create_volume_from_factory(&raw mut error, || -> TestFilesystem {
                panic!("factory panic");
            })
        };
        assert!(volume.is_null());
        assert_eq!(error, Error::IO.0);
    }

    #[test]
    fn a_successful_factory_clears_the_error() {
        let mut error = i32::MIN;
        // SAFETY: `error` is a writable `i32` for the duration of the call.
        let volume = unsafe { create_volume_from_factory(&raw mut error, TestFilesystem::default) };
        assert!(!volume.is_null());
        assert_eq!(error, 0);
        // SAFETY: `volume` came from `create_volume_from_factory`.
        unsafe { crate::ffi::fsk_volume_destroy(volume) };
    }
}
