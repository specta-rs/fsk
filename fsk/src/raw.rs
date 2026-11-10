//! Opt-in borrowed native requests and callback buffers.
//!
//! Most backends never need this module. A backend that wants to observe or
//! intercept kernel-specific requests opts in by returning a non-empty
//! [`RawEventMask`] from [`Filesystem::raw_event_mask`](crate::Filesystem::raw_event_mask)
//! and implementing [`Filesystem::raw_event`](crate::Filesystem::raw_event).
//!
//! See the [crate documentation](crate#accessing-os-native-events) for the
//! platform-specific shapes and lifetime rules.

use crate::Error;
use std::{ffi::c_void, marker::PhantomData, ptr::NonNull};

/// The operating system that produced a [`RawEvent`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    /// `FSKit` on macOS. Requests arrive over the extension's IPC connection,
    /// so native pointers are not available.
    MacOS,
    /// `FUSE` on Linux.
    Linux,
    /// `ProjFS` on Windows.
    Windows,
}

/// The portable operation associated with a native request.
///
/// These mirror the methods on [`Filesystem`](crate::Filesystem). `Open`,
/// `Close`, `Notification`, and `Cancel` describe lifecycle events that have no
/// dedicated portable method.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    /// [`Filesystem::statfs`](crate::Filesystem::statfs).
    StatFs,
    /// [`Filesystem::metadata`](crate::Filesystem::metadata).
    Metadata,
    /// [`Filesystem::set_metadata`](crate::Filesystem::set_metadata).
    SetMetadata,
    /// [`Filesystem::lookup`](crate::Filesystem::lookup).
    Lookup,
    /// [`Filesystem::read_directory`](crate::Filesystem::read_directory).
    ReadDirectory,
    /// [`Filesystem::read`](crate::Filesystem::read).
    Read,
    /// [`Filesystem::write`](crate::Filesystem::write).
    Write,
    /// [`Filesystem::create`](crate::Filesystem::create).
    Create,
    /// [`Filesystem::create_symlink`](crate::Filesystem::create_symlink).
    Symlink,
    /// [`Filesystem::read_symlink`](crate::Filesystem::read_symlink).
    ReadLink,
    /// [`Filesystem::remove`](crate::Filesystem::remove).
    Remove,
    /// [`Filesystem::rename`](crate::Filesystem::rename).
    Rename,
    /// [`Filesystem::synchronize`](crate::Filesystem::synchronize).
    Synchronize,
    /// A file was opened. Native-only.
    Open,
    /// A file was closed or its lease released. Native-only.
    Close,
    /// The platform reported a change to the namespace or file data.
    /// Native-only; inspect [`RawEvent::native`] for the platform-specific code.
    Notification,
    /// The kernel cancelled a pending request. Native-only.
    Cancel,
}

/// Bit mask selecting operations delivered to [`crate::Filesystem::raw_event`].
///
/// Build a mask with [`operation`](Self::operation) and [`union`](Self::union).
/// The adapter caches the mask when the volume is mounted, so changing it later
/// has no effect until the filesystem is mounted again.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RawEventMask(u64);

impl RawEventMask {
    /// No native events. This is the default and keeps the hook off hot paths.
    pub const NONE: Self = Self(0);
    /// Every native event the platform can deliver.
    pub const ALL: Self = Self(u64::MAX);

    /// Returns a mask containing only `operation`.
    #[must_use]
    pub const fn operation(operation: Operation) -> Self {
        Self(1 << operation as u8)
    }

    /// Returns whether `operation` is selected by this mask.
    #[must_use]
    pub const fn contains(self, operation: Operation) -> bool {
        self.0 & (1 << operation as u8) != 0
    }

    /// Returns the union of two masks.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// How [`crate::Filesystem::raw_event`] affects the portable operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawOutcome {
    /// Let the adapter dispatch the portable operation normally.
    Continue,
    /// Skip the portable operation and fail the request with this error.
    Reject(Error),
}

/// A mutable buffer borrowed directly from the platform callback.
///
/// A buffer is provided for data-carrying operations such as
/// [`Operation::Read`] and [`Operation::Write`]. It is valid only for the
/// duration of the [`crate::Filesystem::raw_event`] call. Bounds are already
/// checked by the adapter; use [`len`](Self::len) to avoid reading beyond the
/// native allocation.
pub struct RawBuffer<'a> {
    pointer: NonNull<u8>,
    length: usize,
    writable: bool,
    _lifetime: PhantomData<&'a mut [u8]>,
}

impl RawBuffer<'_> {
    /// Construct a view over a native callback buffer.
    ///
    /// # Safety
    ///
    /// `pointer` must remain valid for `length` bytes for `'a`, and must be
    /// uniquely writable when `writable` is true.
    #[must_use]
    pub unsafe fn new(pointer: NonNull<u8>, length: usize, writable: bool) -> Self {
        Self {
            pointer,
            length,
            writable,
            _lifetime: PhantomData,
        }
    }

    /// Returns the buffer length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.length
    }

    /// Returns whether the buffer is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Returns whether the backend may write into the buffer.
    ///
    /// Read requests from the kernel are writable; write requests carry
    /// backend-owned input and are read-only.
    #[must_use]
    pub const fn is_writable(&self) -> bool {
        self.writable
    }

    /// Borrow the native bytes.
    ///
    /// # Safety
    ///
    /// The platform may impose additional alignment or initialization rules.
    #[must_use]
    pub unsafe fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
    }

    /// Mutably borrow the native bytes.
    ///
    /// Returns `None` when the buffer is read-only.
    ///
    /// # Safety
    ///
    /// The caller must respect the platform operation's buffer contract.
    pub unsafe fn as_bytes_mut(&mut self) -> Option<&mut [u8]> {
        self.writable
            .then(|| unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.length) })
    }
}

/// Request identity common to FUSE, `FSKit` contexts, and Windows callbacks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Caller {
    /// Process ID that triggered the request, when the platform provides it.
    pub process_id: u32,
    /// User ID that triggered the request, when the platform provides it.
    pub user_id: u32,
    /// Group ID that triggered the request, when the platform provides it.
    pub group_id: u32,
}

/// Platform-specific request state borrowed for one [`RawEvent`].
///
/// Every pointer in this enum is valid only for the duration of the
/// [`crate::Filesystem::raw_event`] call. Copy what you need before returning.
pub enum NativeRequest<'a> {
    /// `FSKit` executes out of process, so native Objective-C pointers cannot
    /// safely cross the bridge. The event preserves identifiers, flags, caller
    /// data, and the exact binary request payload.
    MacOS {
        /// Monotonic identifier for this IPC request.
        request_id: u64,
        /// The item identifier encoded at the start of the payload, or `0`.
        item_id: u64,
        /// Reserved native flags. Currently always `0`.
        flags: u64,
        /// The raw IPC request body. Its layout is an implementation detail of
        /// the Rust/Swift bridge and may change between versions.
        payload: &'a [u8],
    },
    /// Borrowed FUSE request. `request` points at fuser's request object and is
    /// usable only synchronously during the hook.
    Linux {
        /// Opaque pointer to `fuser::Request`.
        request: NonNull<c_void>,
        /// The FUSE unique request identifier.
        unique: u64,
        /// The file handle associated with the operation, or `0`.
        file_handle: u64,
        /// Native open flags associated with the operation, or `0`.
        flags: u32,
    },
    /// Borrowed `WinFsp` or `ProjFS` callback state.
    Windows {
        /// Opaque pointer to the `PRJ_CALLBACK_DATA` for this callback.
        callback_data: NonNull<c_void>,
        /// The `PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT` as an integer.
        namespace_context: isize,
        /// The `CommandId` of the callback.
        command_id: i32,
        /// Callback flags. For notifications, the raw `PRJ_NOTIFICATION` value
        /// is combined with this field using a bitwise OR.
        flags: u32,
    },
}

/// A borrowed native request delivered to [`crate::Filesystem::raw_event`].
pub struct RawEvent<'a> {
    /// The platform adapter that produced the event.
    pub platform: Platform,
    /// The portable operation being performed.
    pub operation: Operation,
    /// Identity of the caller, when available.
    pub caller: Caller,
    /// Platform-specific request state.
    pub native: NativeRequest<'a>,
    /// The data buffer for this operation, if it has one.
    pub buffer: Option<RawBuffer<'a>>,
}

impl std::fmt::Debug for RawEvent<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RawEvent")
            .field("platform", &self.platform)
            .field("operation", &self.operation)
            .field("caller", &self.caller)
            .field("has_buffer", &self.buffer.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_masks_compose_without_allocating() {
        let mask = RawEventMask::operation(Operation::Read)
            .union(RawEventMask::operation(Operation::Write));
        assert!(mask.contains(Operation::Read));
        assert!(mask.contains(Operation::Write));
        assert!(!mask.contains(Operation::Lookup));
        assert_eq!(
            std::mem::size_of::<RawEventMask>(),
            std::mem::size_of::<u64>()
        );
    }
}
