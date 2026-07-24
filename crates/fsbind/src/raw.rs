use crate::Error;
use std::{ffi::c_void, marker::PhantomData, ptr::NonNull};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    MacOS,
    Linux,
    Windows,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    StatFs,
    Metadata,
    SetMetadata,
    Lookup,
    ReadDirectory,
    Read,
    Write,
    Create,
    Symlink,
    ReadLink,
    Remove,
    Rename,
    Synchronize,
    Open,
    Close,
    Notification,
    Cancel,
}

/// Bit mask selecting operations delivered to [`crate::Filesystem::raw_event`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RawEventMask(u64);

impl RawEventMask {
    pub const NONE: Self = Self(0);
    pub const ALL: Self = Self(u64::MAX);

    #[must_use]
    pub const fn operation(operation: Operation) -> Self {
        Self(1 << operation as u8)
    }

    #[must_use]
    pub const fn contains(self, operation: Operation) -> bool {
        self.0 & (1 << operation as u8) != 0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawOutcome {
    Continue,
    Reject(Error),
}

/// A mutable buffer borrowed directly from the platform callback.
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

    #[must_use]
    pub const fn len(&self) -> usize {
        self.length
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

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
    pub process_id: u32,
    pub user_id: u32,
    pub group_id: u32,
}

pub enum NativeRequest<'a> {
    /// `FSKit` executes out of process, so native Objective-C pointers cannot
    /// safely cross the bridge. The event preserves identifiers, flags, caller
    /// data, and the exact binary request payload.
    MacOS {
        request_id: u64,
        item_id: u64,
        flags: u64,
        payload: &'a [u8],
    },
    /// Borrowed FUSE request. `request` points at fuser's request object and is
    /// usable only synchronously during the hook.
    Linux {
        request: NonNull<c_void>,
        unique: u64,
        file_handle: u64,
        flags: u32,
    },
    /// Borrowed `WinFsp` or `ProjFS` callback state.
    Windows {
        callback_data: NonNull<c_void>,
        namespace_context: isize,
        command_id: i32,
        flags: u32,
    },
}

pub struct RawEvent<'a> {
    pub platform: Platform,
    pub operation: Operation,
    pub caller: Caller,
    pub native: NativeRequest<'a>,
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
