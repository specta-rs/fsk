//! Windows `ProjFS` adapter.

#![allow(clippy::upper_case_acronyms, non_camel_case_types, non_snake_case)]

use crate::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, NetworkFilesystem,
    ROOT_INODE,
    raw::{
        Caller, NativeRequest, Operation, Platform, RawBuffer, RawEvent, RawEventMask, RawOutcome,
    },
};
use parking_lot::{Mutex, RwLock};
use std::{
    collections::HashMap,
    ffi::c_void,
    fs, io,
    mem::size_of,
    path::{Component, Path, PathBuf},
    ptr::{self, NonNull},
    sync::Arc,
};
#[allow(clippy::wildcard_imports)]
use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_ACCESS_DENIED, ERROR_DIR_NOT_EMPTY, ERROR_DIRECTORY, ERROR_DISK_FULL,
            ERROR_FILE_EXISTS, ERROR_FILE_INVALID, ERROR_FILE_NOT_FOUND,
            ERROR_FILE_SYSTEM_VIRTUALIZATION_INVALID_OPERATION, ERROR_HANDLE_EOF,
            ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
            ERROR_IO_DEVICE, ERROR_NOT_ENOUGH_MEMORY, ERROR_NOT_SUPPORTED, ERROR_PATH_NOT_FOUND,
            ERROR_REPARSE_POINT_ENCOUNTERED, ERROR_SHARING_VIOLATION, ERROR_WRITE_PROTECT, FARPROC,
            FreeLibrary, HMODULE,
        },
        System::{
            Com::CoCreateGuid,
            LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
        },
    },
    core::{GUID, HRESULT, PCWSTR},
};

const S_OK: HRESULT = 0;
const FILE_ATTRIBUTE_READ_ONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const WINDOWS_EPOCH_OFFSET_100NS: u64 = 116_444_736_000_000_000;
const MAX_SYMLINK_TARGET_BYTES: usize = 1024 * 1024;

const PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN: i32 = 1;
const PRJ_CB_DATA_FLAG_ENUM_RETURN_SINGLE_ENTRY: i32 = 2;
const PRJ_FLAG_NONE: i32 = 0;
const PRJ_FLAG_USE_NEGATIVE_PATH_CACHE: i32 = 1;
const PRJ_EXT_INFO_TYPE_SYMLINK: i32 = 1;
const PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED: u32 = 2048;
const PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED: u32 = 1024;
const PRJ_NOTIFY_FILE_HANDLE_CLOSED_NO_MODIFICATION: u32 = 512;
const PRJ_NOTIFY_FILE_OPENED: u32 = 2;
const PRJ_NOTIFY_FILE_OVERWRITTEN: u32 = 8;
const PRJ_NOTIFY_FILE_PRE_CONVERT_TO_FULL: u32 = 4096;
const PRJ_NOTIFY_FILE_RENAMED: u32 = 128;
const PRJ_NOTIFY_HARDLINK_CREATED: u32 = 256;
const PRJ_NOTIFY_NEW_FILE_CREATED: u32 = 4;
const PRJ_NOTIFY_PRE_DELETE: u32 = 16;
const PRJ_NOTIFY_PRE_RENAME: u32 = 32;
const PRJ_NOTIFY_PRE_SET_HARDLINK: u32 = 64;
const PRJ_UPDATE_ALLOW_READ_ONLY: i32 = 32;
const PRJ_UPDATE_FAILURE_CAUSE_NONE: i32 = 0;
const PRJ_UPDATE_NONE: i32 = 0;

type PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT = *mut c_void;
type PRJ_DIR_ENTRY_BUFFER_HANDLE = *mut c_void;
type PRJ_CALLBACK_DATA_FLAGS = i32;
type PRJ_NOTIFICATION = i32;
type PRJ_NOTIFY_TYPES = u32;
type PRJ_STARTVIRTUALIZING_FLAGS = i32;
type PRJ_UPDATE_TYPES = i32;
type PRJ_UPDATE_FAILURE_CAUSES = i32;

type PRJ_START_DIRECTORY_ENUMERATION_CB =
    Option<unsafe extern "system" fn(*const PRJ_CALLBACK_DATA, *const GUID) -> HRESULT>;
type PRJ_END_DIRECTORY_ENUMERATION_CB =
    Option<unsafe extern "system" fn(*const PRJ_CALLBACK_DATA, *const GUID) -> HRESULT>;
type PRJ_GET_DIRECTORY_ENUMERATION_CB = Option<
    unsafe extern "system" fn(
        *const PRJ_CALLBACK_DATA,
        *const GUID,
        PCWSTR,
        PRJ_DIR_ENTRY_BUFFER_HANDLE,
    ) -> HRESULT,
>;
type PRJ_GET_PLACEHOLDER_INFO_CB =
    Option<unsafe extern "system" fn(*const PRJ_CALLBACK_DATA) -> HRESULT>;
type PRJ_GET_FILE_DATA_CB =
    Option<unsafe extern "system" fn(*const PRJ_CALLBACK_DATA, u64, u32) -> HRESULT>;
type PRJ_QUERY_FILE_NAME_CB =
    Option<unsafe extern "system" fn(*const PRJ_CALLBACK_DATA) -> HRESULT>;
type PRJ_NOTIFICATION_CB = Option<
    unsafe extern "system" fn(
        *const PRJ_CALLBACK_DATA,
        bool,
        PRJ_NOTIFICATION,
        PCWSTR,
        *mut PRJ_NOTIFICATION_PARAMETERS,
    ) -> HRESULT,
>;
type PRJ_CANCEL_COMMAND_CB = Option<unsafe extern "system" fn(*const PRJ_CALLBACK_DATA)>;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PRJ_CALLBACKS {
    pub StartDirectoryEnumerationCallback: PRJ_START_DIRECTORY_ENUMERATION_CB,
    pub EndDirectoryEnumerationCallback: PRJ_END_DIRECTORY_ENUMERATION_CB,
    pub GetDirectoryEnumerationCallback: PRJ_GET_DIRECTORY_ENUMERATION_CB,
    pub GetPlaceholderInfoCallback: PRJ_GET_PLACEHOLDER_INFO_CB,
    pub GetFileDataCallback: PRJ_GET_FILE_DATA_CB,
    pub QueryFileNameCallback: PRJ_QUERY_FILE_NAME_CB,
    pub NotificationCallback: PRJ_NOTIFICATION_CB,
    pub CancelCommandCallback: PRJ_CANCEL_COMMAND_CB,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PRJ_CALLBACK_DATA {
    pub Size: u32,
    pub Flags: PRJ_CALLBACK_DATA_FLAGS,
    pub NamespaceVirtualizationContext: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    pub CommandId: i32,
    pub FileId: GUID,
    pub DataStreamId: GUID,
    pub FilePathName: PCWSTR,
    pub VersionInfo: *mut PRJ_PLACEHOLDER_VERSION_INFO,
    pub TriggeringProcessId: u32,
    pub TriggeringProcessImageFileName: PCWSTR,
    pub InstanceContext: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PRJ_FILE_BASIC_INFO {
    pub IsDirectory: bool,
    pub FileSize: i64,
    pub CreationTime: i64,
    pub LastAccessTime: i64,
    pub LastWriteTime: i64,
    pub ChangeTime: i64,
    pub FileAttributes: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PRJ_PLACEHOLDER_VERSION_INFO {
    pub ProviderID: [u8; 128],
    pub ContentID: [u8; 128],
}

impl Default for PRJ_PLACEHOLDER_VERSION_INFO {
    fn default() -> Self {
        Self {
            ProviderID: [0; 128],
            ContentID: [0; 128],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PRJ_PLACEHOLDER_INFO {
    pub FileBasicInfo: PRJ_FILE_BASIC_INFO,
    pub EaInformation: PRJ_PLACEHOLDER_EA_INFORMATION,
    pub SecurityInformation: PRJ_PLACEHOLDER_SECURITY_INFORMATION,
    pub StreamsInformation: PRJ_PLACEHOLDER_STREAMS_INFORMATION,
    pub VersionInfo: PRJ_PLACEHOLDER_VERSION_INFO,
    pub VariableData: [u8; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PRJ_PLACEHOLDER_EA_INFORMATION {
    pub EaBufferSize: u32,
    pub OffsetToFirstEa: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PRJ_PLACEHOLDER_SECURITY_INFORMATION {
    pub SecurityBufferSize: u32,
    pub OffsetToSecurityDescriptor: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PRJ_PLACEHOLDER_STREAMS_INFORMATION {
    pub StreamsInfoBufferSize: u32,
    pub OffsetToFirstStreamInfo: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PRJ_EXTENDED_INFO {
    pub InfoType: i32,
    pub NextInfoOffset: u32,
    pub Anonymous: PRJ_EXTENDED_INFO_0,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union PRJ_EXTENDED_INFO_0 {
    pub Symlink: PRJ_EXTENDED_INFO_0_0,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PRJ_EXTENDED_INFO_0_0 {
    pub TargetName: PCWSTR,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PRJ_NOTIFICATION_MAPPING {
    pub NotificationBitMask: PRJ_NOTIFY_TYPES,
    pub NotificationRoot: PCWSTR,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PRJ_NOTIFICATION_PARAMETERS_0 {
    pub NotificationMask: PRJ_NOTIFY_TYPES,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PRJ_NOTIFICATION_PARAMETERS_1 {
    pub NotificationMask: PRJ_NOTIFY_TYPES,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PRJ_NOTIFICATION_PARAMETERS_2 {
    pub IsFileModified: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union PRJ_NOTIFICATION_PARAMETERS {
    pub PostCreate: PRJ_NOTIFICATION_PARAMETERS_0,
    pub FileRenamed: PRJ_NOTIFICATION_PARAMETERS_1,
    pub FileDeletedOnHandleClose: PRJ_NOTIFICATION_PARAMETERS_2,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PRJ_STARTVIRTUALIZING_OPTIONS {
    pub Flags: PRJ_STARTVIRTUALIZING_FLAGS,
    pub PoolThreadCount: u32,
    pub ConcurrentThreadCount: u32,
    pub NotificationMappings: *mut PRJ_NOTIFICATION_MAPPING,
    pub NotificationMappingsCount: u32,
}

type PrjAllocateAlignedBufferFn =
    unsafe extern "system" fn(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, usize) -> *mut c_void;
type PrjDeleteFileFn = unsafe extern "system" fn(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    PCWSTR,
    PRJ_UPDATE_TYPES,
    *mut PRJ_UPDATE_FAILURE_CAUSES,
) -> HRESULT;
type PrjFileNameCompareFn = unsafe extern "system" fn(PCWSTR, PCWSTR) -> i32;
type PrjFileNameMatchFn = unsafe extern "system" fn(PCWSTR, PCWSTR) -> bool;
type PrjFillDirEntryBufferFn = unsafe extern "system" fn(
    PCWSTR,
    *const PRJ_FILE_BASIC_INFO,
    PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT;
type PrjFillDirEntryBuffer2Fn = unsafe extern "system" fn(
    PRJ_DIR_ENTRY_BUFFER_HANDLE,
    PCWSTR,
    *const PRJ_FILE_BASIC_INFO,
    *const PRJ_EXTENDED_INFO,
) -> HRESULT;
type PrjFreeAlignedBufferFn = unsafe extern "system" fn(*const c_void);
type PrjMarkDirectoryAsPlaceholderFn = unsafe extern "system" fn(
    PCWSTR,
    PCWSTR,
    *const PRJ_PLACEHOLDER_VERSION_INFO,
    *const GUID,
) -> HRESULT;
type PrjStartVirtualizingFn = unsafe extern "system" fn(
    PCWSTR,
    *const PRJ_CALLBACKS,
    *const c_void,
    *const PRJ_STARTVIRTUALIZING_OPTIONS,
    *mut PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
) -> HRESULT;
type PrjStopVirtualizingFn = unsafe extern "system" fn(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT);
type PrjWriteFileDataFn = unsafe extern "system" fn(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    *const GUID,
    *const c_void,
    u64,
    u32,
) -> HRESULT;
type PrjWritePlaceholderInfoFn = unsafe extern "system" fn(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    PCWSTR,
    *const PRJ_PLACEHOLDER_INFO,
    u32,
) -> HRESULT;
type PrjWritePlaceholderInfo2Fn = unsafe extern "system" fn(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    PCWSTR,
    *const PRJ_PLACEHOLDER_INFO,
    u32,
    *const PRJ_EXTENDED_INFO,
) -> HRESULT;

struct ModuleHandle(HMODULE);

impl Drop for ModuleHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from LoadLibraryExW and has not been freed.
        unsafe { FreeLibrary(self.0) };
    }
}

struct ProjFS {
    module: HMODULE,
    allocate_aligned_buffer: PrjAllocateAlignedBufferFn,
    delete_file: PrjDeleteFileFn,
    file_name_compare: PrjFileNameCompareFn,
    file_name_match: PrjFileNameMatchFn,
    fill_dir_entry_buffer: PrjFillDirEntryBufferFn,
    fill_dir_entry_buffer2: Option<PrjFillDirEntryBuffer2Fn>,
    free_aligned_buffer: PrjFreeAlignedBufferFn,
    mark_directory_as_placeholder: PrjMarkDirectoryAsPlaceholderFn,
    start_virtualizing: PrjStartVirtualizingFn,
    stop_virtualizing: PrjStopVirtualizingFn,
    write_file_data: PrjWriteFileDataFn,
    write_placeholder_info: PrjWritePlaceholderInfoFn,
    write_placeholder_info2: Option<PrjWritePlaceholderInfo2Fn>,
}

// SAFETY: an HMODULE and its exported function pointers are immutable and may
// be called concurrently. The handle stays loaded while any Arc<ProjFS> exists.
unsafe impl Send for ProjFS {}
unsafe impl Sync for ProjFS {}

impl Drop for ProjFS {
    fn drop(&mut self) {
        // SAFETY: this is the last owner and no API call can still be running.
        unsafe { FreeLibrary(self.module) };
    }
}

impl ProjFS {
    fn load() -> io::Result<Arc<Self>> {
        let name: Vec<u16> = "ProjectedFSLib.dll\0".encode_utf16().collect();
        // SAFETY: name is a terminated system DLL name.
        let module =
            unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "ProjectedFSLib.dll is unavailable; enable the Client-ProjFS Windows feature",
            ));
        }
        let module = ModuleHandle(module);
        let api = Self {
            module: module.0,
            allocate_aligned_buffer: load_symbol(module.0, b"PrjAllocateAlignedBuffer\0")?,
            delete_file: load_symbol(module.0, b"PrjDeleteFile\0")?,
            file_name_compare: load_symbol(module.0, b"PrjFileNameCompare\0")?,
            file_name_match: load_symbol(module.0, b"PrjFileNameMatch\0")?,
            fill_dir_entry_buffer: load_symbol(module.0, b"PrjFillDirEntryBuffer\0")?,
            fill_dir_entry_buffer2: load_optional_symbol(module.0, b"PrjFillDirEntryBuffer2\0"),
            free_aligned_buffer: load_symbol(module.0, b"PrjFreeAlignedBuffer\0")?,
            mark_directory_as_placeholder: load_symbol(
                module.0,
                b"PrjMarkDirectoryAsPlaceholder\0",
            )?,
            start_virtualizing: load_symbol(module.0, b"PrjStartVirtualizing\0")?,
            stop_virtualizing: load_symbol(module.0, b"PrjStopVirtualizing\0")?,
            write_file_data: load_symbol(module.0, b"PrjWriteFileData\0")?,
            write_placeholder_info: load_symbol(module.0, b"PrjWritePlaceholderInfo\0")?,
            write_placeholder_info2: load_optional_symbol(module.0, b"PrjWritePlaceholderInfo2\0"),
        };
        std::mem::forget(module);
        Ok(Arc::new(api))
    }

    fn compare_names(&self, left: &[u16], right: &[u16]) -> i32 {
        let mut left = left.to_vec();
        let mut right = right.to_vec();
        left.push(0);
        right.push(0);
        // SAFETY: both vectors are terminated and live for the synchronous call.
        unsafe { (self.file_name_compare)(left.as_ptr(), right.as_ptr()) }
    }

    fn fill_directory_entry(
        &self,
        name: &[u16],
        info: &PRJ_FILE_BASIC_INFO,
        buffer: PRJ_DIR_ENTRY_BUFFER_HANDLE,
        symlink_target: Option<&[u16]>,
    ) -> HRESULT {
        // SAFETY: all pointers are valid for this synchronous call.
        unsafe {
            if let Some(target) = symlink_target {
                let Some(fill) = self.fill_dir_entry_buffer2 else {
                    return hresult(ERROR_NOT_SUPPORTED);
                };
                let extended = symlink_extended_info(target);
                fill(buffer, name.as_ptr(), info, &raw const extended)
            } else {
                (self.fill_dir_entry_buffer)(name.as_ptr(), info, buffer)
            }
        }
    }

    fn write_placeholder(
        &self,
        context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
        path: PCWSTR,
        info: &PRJ_PLACEHOLDER_INFO,
        symlink_target: Option<&[u16]>,
    ) -> HRESULT {
        let size = u32::try_from(size_of::<PRJ_PLACEHOLDER_INFO>()).unwrap_or(u32::MAX);
        // SAFETY: all pointers are valid for this synchronous call.
        unsafe {
            if let Some(target) = symlink_target {
                let Some(write) = self.write_placeholder_info2 else {
                    return hresult(ERROR_NOT_SUPPORTED);
                };
                let extended = symlink_extended_info(target);
                write(context, path, info, size, &raw const extended)
            } else {
                (self.write_placeholder_info)(context, path, info, size)
            }
        }
    }
}

fn load_symbol<T: Copy>(module: HMODULE, name: &[u8]) -> io::Result<T> {
    // SAFETY: module is loaded and name is a terminated export name.
    let address: FARPROC = unsafe { GetProcAddress(module, name.as_ptr()) };
    let Some(address) = address else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ProjectedFSLib.dll is missing export {}",
                String::from_utf8_lossy(name.strip_suffix(b"\0").unwrap_or(name))
            ),
        ));
    };
    // SAFETY: a non-null FARPROC has the representation of the function-pointer
    // type requested for that export, and T is the matching function pointer.
    let function: T = unsafe { std::mem::transmute_copy(&address) };
    Ok(function)
}

fn load_optional_symbol<T: Copy>(module: HMODULE, name: &[u8]) -> Option<T> {
    // SAFETY: module is loaded and name is a terminated export name.
    let address: FARPROC = unsafe { GetProcAddress(module, name.as_ptr()) };
    address.map(|address| {
        // SAFETY: a non-null FARPROC has the requested function-pointer layout.
        unsafe { std::mem::transmute_copy(&address) }
    })
}

fn symlink_extended_info(target: &[u16]) -> PRJ_EXTENDED_INFO {
    // SAFETY: every field is initialized below before the value is read.
    let mut extended: PRJ_EXTENDED_INFO = unsafe { std::mem::zeroed() };
    extended.InfoType = PRJ_EXT_INFO_TYPE_SYMLINK;
    extended.NextInfoOffset = 0;
    extended.Anonymous.Symlink.TargetName = target.as_ptr();
    extended
}

/// Settings used when creating a `ProjFS` virtualization root on Windows.
#[derive(Clone, Debug)]
pub struct MountOptions {
    /// Present the projection as read-only.
    pub read_only: bool,
    /// Filesystem name used for diagnostics.
    pub filesystem_name: String,
    /// Number of concurrent `ProjFS` callback threads.
    pub worker_threads: usize,
    /// Remember names that were reported as absent to avoid repeated lookups.
    pub use_negative_path_cache: bool,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            filesystem_name: "fsk".into(),
            worker_threads: std::thread::available_parallelism().map_or(4, usize::from),
            use_negative_path_cache: true,
        }
    }
}

/// The result of an eviction request.
///
/// Eviction is best-effort: `ProjFS` only removes items that are still a clean
/// cache of the backend. Items with local changes are retained so no user data
/// is discarded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvictionOutcome {
    /// The cached copy was removed, or the item was already virtual.
    Evicted,
    /// The item has local changes, is a tombstone, is open, or is a non-empty
    /// directory, so it was left in place. An open item may instead fail the
    /// call depending on how its handles were opened; treat either result as
    /// retryable.
    Retained,
}

/// A cloneable handle that evicts hydrated content from another thread.
///
/// [`MountSession`] is created after the backend is mounted, so a backend that
/// needs to schedule eviction keeps a handle instead of a session:
///
/// ```ignore
/// let eviction = Arc::new(Mutex::new(None));
/// let backend = Backend::new(Arc::clone(&eviction));
/// let session = MountSession::mount(backend, path)?;
/// *eviction.lock().unwrap() = Some(session.eviction_handle());
///
/// // From a worker thread, once the file is idle:
/// eviction.lock().unwrap().as_ref().unwrap().evict("dir/file.txt")?;
/// ```
///
/// The handle does not keep the mount alive. Calls made after the session is
/// dropped return [`io::ErrorKind::NotConnected`].
#[derive(Clone)]
pub struct EvictionHandle {
    state: Arc<EvictionState>,
    api: Arc<ProjFS>,
}

impl EvictionHandle {
    /// Evicts a hydrated file from the `ProjFS` cache.
    ///
    /// `relative_path` is interpreted relative to the mount, must not contain a
    /// root, prefix, or `..` component, and no component may end in `.` or a
    /// space (Win32 strips those characters).
    ///
    /// See [`MountSession::evict`] for the behavior and errors.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::NotConnected`] after the mount stops, or another
    /// error when the request fails for a reason unrelated to the item's state.
    pub fn evict(&self, relative_path: impl AsRef<Path>) -> io::Result<EvictionOutcome> {
        self.state
            .evict(relative_path.as_ref(), self.api.delete_file)
    }
}

struct EvictionState {
    context: RwLock<Option<VirtualizationContext>>,
}

impl EvictionState {
    fn evict(
        &self,
        relative_path: &Path,
        delete_file: PrjDeleteFileFn,
    ) -> io::Result<EvictionOutcome> {
        let path = wide_relative_path(relative_path)?;
        let context = self.context.read();
        let Some(context) = context.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the ProjFS mount is no longer active",
            ));
        };
        let mut failure = PRJ_UPDATE_FAILURE_CAUSE_NONE;
        // SAFETY: the read guard keeps `MountSession::drop` from taking the
        // context until this call returns, and `path` is a valid
        // null-terminated relative path.
        let result = unsafe {
            delete_file(
                context.0,
                path.as_ptr(),
                PRJ_UPDATE_NONE | PRJ_UPDATE_ALLOW_READ_ONLY,
                &raw mut failure,
            )
        };
        classify_eviction(result, failure)
    }
}

struct VirtualizationContext(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT);

// SAFETY: ProjFS virtualization contexts are process-wide handles that may be
// used from any thread. `EvictionState` synchronizes access, and the session
// stops virtualization before the handle becomes invalid.
unsafe impl Send for VirtualizationContext {}
// SAFETY: the handle is immutable and shared access only reads the pointer.
unsafe impl Sync for VirtualizationContext {}

/// An active Windows `ProjFS` virtualization root.
///
/// `ProjFS` is lazy by design: projected metadata and contents are materialized
/// into the Windows cache on demand. Local edits remain in that cache and are
/// reported through raw notification events.
///
/// `ProjFS` does not evict hydrated content automatically. Use
/// [`eviction_handle`](Self::eviction_handle) or [`evict`](Self::evict) to
/// return idle files to virtual placeholders.
pub struct MountSession {
    mount_path: PathBuf,
    eviction: EvictionHandle,
    _state: Box<WindowsState>,
}

impl MountSession {
    /// Start a projected filesystem at `mount_path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be prepared or `ProjFS` rejects
    /// the virtualization instance.
    pub fn mount(filesystem: impl Filesystem, mount_path: impl AsRef<Path>) -> io::Result<Self> {
        Self::mount_with_options(filesystem, mount_path, &MountOptions::default())
    }

    /// Start a projected filesystem with explicit Windows settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be prepared or `ProjFS` rejects
    /// the virtualization instance.
    pub fn mount_with_options(
        filesystem: impl Filesystem,
        mount_path: impl AsRef<Path>,
        options: &MountOptions,
    ) -> io::Result<Self> {
        Self::start(Arc::new(filesystem), mount_path.as_ref(), options)
    }

    /// Start a lazy network projection.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be prepared or `ProjFS` rejects
    /// the virtualization instance.
    pub fn mount_network(
        filesystem: impl NetworkFilesystem,
        mount_path: impl AsRef<Path>,
    ) -> io::Result<Self> {
        Self::mount_network_with_options(filesystem, mount_path, &MountOptions::default())
    }

    /// Start a lazy network projection with explicit Windows settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be prepared or `ProjFS` rejects
    /// the virtualization instance.
    pub fn mount_network_with_options(
        filesystem: impl NetworkFilesystem,
        mount_path: impl AsRef<Path>,
        options: &MountOptions,
    ) -> io::Result<Self> {
        Self::start(Arc::new(filesystem), mount_path.as_ref(), options)
    }

    /// Path of the virtualization root.
    #[must_use]
    pub fn mount_path(&self) -> &Path {
        &self.mount_path
    }

    /// Returns a cloneable handle that can evict hydrated files from another
    /// thread.
    ///
    /// The handle does not keep the mount alive. Calls made after the session is
    /// dropped return [`io::ErrorKind::NotConnected`].
    #[must_use]
    pub fn eviction_handle(&self) -> EvictionHandle {
        self.eviction.clone()
    }

    /// Evicts a hydrated file from the `ProjFS` cache.
    ///
    /// `relative_path` is interpreted relative to
    /// [`mount_path`](Self::mount_path), must not contain a root, prefix, or
    /// `..` component, and no component may end in `.` or a space (Win32 strips
    /// those characters). On success the local copy is removed and the item
    /// becomes virtual again; the next open recreates the placeholder and
    /// hydrates it through the backend. A path that is not cached is treated as
    /// already evicted.
    ///
    /// Eviction is best-effort. Items with local changes, tombstones, open
    /// handles, and non-empty directories are left in place and reported as
    /// [`EvictionOutcome::Retained`]; retry after handles close. This never
    /// asks `ProjFS` to discard dirty metadata or dirty file data.
    ///
    /// A clean directory placeholder is also a valid target because this call
    /// passes `PRJ_UPDATE_NONE`, which only permits items still in the
    /// placeholder state. Removing an empty directory's local metadata is
    /// recoverable when the backend lists the directory in its parent
    /// enumeration; a proactively created placeholder that is not enumerated
    /// must be re-created by the backend. A non-empty directory is retained.
    ///
    /// Call this outside a `Filesystem` callback. `ProjFS` may dispatch
    /// callbacks while the eviction is processed, so the backend must not hold a
    /// lock that those callbacks need.
    ///
    /// # Errors
    ///
    /// Returns an error when `relative_path` is invalid, the mount has stopped,
    /// or `ProjFS` rejects the request for a reason unrelated to the item's
    /// state.
    pub fn evict(&self, relative_path: impl AsRef<Path>) -> io::Result<EvictionOutcome> {
        self.eviction.evict(relative_path.as_ref())
    }

    /// Stop the projection.
    ///
    /// Dropping the session has the same effect.
    pub fn unmount(self) {}

    fn start(
        filesystem: Arc<dyn Filesystem>,
        mount_path: &Path,
        options: &MountOptions,
    ) -> io::Result<Self> {
        fs::create_dir_all(mount_path)?;
        let wide_path = wide_path(mount_path);
        let api = ProjFS::load()?;
        let mut instance_id = GUID::default();
        // SAFETY: CoCreateGuid writes exactly one GUID.
        check_hresult(unsafe { CoCreateGuid(&raw mut instance_id) })?;
        // SAFETY: all pointers reference initialized values for this call.
        check_placeholder_mark_hresult(unsafe {
            (api.mark_directory_as_placeholder)(
                wide_path.as_ptr(),
                ptr::null(),
                ptr::null(),
                &raw const instance_id,
            )
        })?;

        let mut state = Box::new(WindowsState::new(filesystem, Arc::clone(&api)));
        let callbacks = PRJ_CALLBACKS {
            StartDirectoryEnumerationCallback: Some(start_directory),
            EndDirectoryEnumerationCallback: Some(end_directory),
            GetDirectoryEnumerationCallback: Some(get_directory),
            GetPlaceholderInfoCallback: Some(get_placeholder),
            GetFileDataCallback: Some(get_file_data),
            QueryFileNameCallback: Some(query_file_name),
            NotificationCallback: Some(notification),
            CancelCommandCallback: Some(cancel_command),
        };
        let root = [0_u16];
        let mut mapping = PRJ_NOTIFICATION_MAPPING {
            NotificationBitMask: PRJ_NOTIFY_FILE_OPENED
                | PRJ_NOTIFY_NEW_FILE_CREATED
                | PRJ_NOTIFY_FILE_OVERWRITTEN
                | PRJ_NOTIFY_PRE_DELETE
                | PRJ_NOTIFY_PRE_RENAME
                | PRJ_NOTIFY_PRE_SET_HARDLINK
                | PRJ_NOTIFY_FILE_RENAMED
                | PRJ_NOTIFY_HARDLINK_CREATED
                | PRJ_NOTIFY_FILE_HANDLE_CLOSED_NO_MODIFICATION
                | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED
                | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED
                | PRJ_NOTIFY_FILE_PRE_CONVERT_TO_FULL,
            NotificationRoot: root.as_ptr(),
        };
        let start_options = PRJ_STARTVIRTUALIZING_OPTIONS {
            Flags: if options.use_negative_path_cache {
                PRJ_FLAG_USE_NEGATIVE_PATH_CACHE
            } else {
                PRJ_FLAG_NONE
            },
            PoolThreadCount: u32::try_from(options.worker_threads.saturating_mul(2))
                .unwrap_or(u32::MAX),
            ConcurrentThreadCount: u32::try_from(options.worker_threads).unwrap_or(u32::MAX),
            NotificationMappings: &raw mut mapping,
            NotificationMappingsCount: 1,
        };
        let mut context = ptr::null_mut();
        // SAFETY: state is boxed and remains stable until after StopVirtualizing.
        check_hresult(unsafe {
            (api.start_virtualizing)(
                wide_path.as_ptr(),
                &raw const callbacks,
                (&raw mut *state).cast(),
                &raw const start_options,
                &raw mut context,
            )
        })?;
        Ok(Self {
            mount_path: mount_path.to_owned(),
            eviction: EvictionHandle {
                state: Arc::new(EvictionState {
                    context: RwLock::new(Some(VirtualizationContext(context))),
                }),
                api,
            },
            _state: state,
        })
    }
}

impl Drop for MountSession {
    fn drop(&mut self) {
        let context = self.eviction.state.context.write().take();
        if let Some(context) = context {
            // SAFETY: taking the context under the write lock prevents new
            // eviction calls from using it, and existing calls hold the read
            // lock until they finish.
            unsafe { (self.eviction.api.stop_virtualizing)(context.0) };
        }
    }
}

struct WindowsState {
    filesystem: Arc<dyn Filesystem>,
    api: Arc<ProjFS>,
    raw_mask: RawEventMask,
    enumerations: Mutex<HashMap<[u8; 16], Arc<Mutex<Enumeration>>>>,
}

impl WindowsState {
    fn new(filesystem: Arc<dyn Filesystem>, api: Arc<ProjFS>) -> Self {
        let raw_mask = filesystem.raw_event_mask();
        Self {
            filesystem,
            api,
            raw_mask,
            enumerations: Mutex::new(HashMap::new()),
        }
    }

    unsafe fn from_callback<'a>(callback: *const PRJ_CALLBACK_DATA) -> &'a Self {
        // SAFETY: every callback is invoked with the stable instance pointer.
        unsafe { &*((*callback).InstanceContext.cast::<Self>()) }
    }

    unsafe fn before(
        &self,
        callback: *const PRJ_CALLBACK_DATA,
        operation: Operation,
        buffer: Option<RawBuffer<'_>>,
        extra_flags: u32,
    ) -> Result<(), HRESULT> {
        if !self.raw_mask.contains(operation) {
            return Ok(());
        }
        // SAFETY: callback is guaranteed non-null by ProjFS.
        let data = unsafe { &*callback };
        let event = RawEvent {
            platform: Platform::Windows,
            operation,
            caller: Caller {
                process_id: data.TriggeringProcessId,
                user_id: 0,
                group_id: 0,
            },
            native: NativeRequest::Windows {
                callback_data: NonNull::from(data).cast::<c_void>(),
                namespace_context: data.NamespaceVirtualizationContext as isize,
                command_id: data.CommandId,
                flags: u32::from_ne_bytes(data.Flags.to_ne_bytes()) | extra_flags,
            },
            buffer,
        };
        match self.filesystem.raw_event(event) {
            RawOutcome::Continue => Ok(()),
            RawOutcome::Reject(error) => Err(error_hresult(error)),
        }
    }

    unsafe fn inode_for_callback(&self, callback: *const PRJ_CALLBACK_DATA) -> crate::Result<u64> {
        // SAFETY: FilePathName is a callback-scoped null-terminated string.
        let path = unsafe { wide_slice((*callback).FilePathName) };
        self.inode_for_path(path)
    }

    fn inode_for_path(&self, path: &[u16]) -> crate::Result<u64> {
        self.inode_for_path_with(path, |left, right| self.api.compare_names(left, right) == 0)
    }

    fn inode_for_path_with(
        &self,
        path: &[u16],
        mut name_equal: impl FnMut(&[u16], &[u16]) -> bool,
    ) -> crate::Result<u64> {
        let mut inode = ROOT_INODE;
        for component in path.split(|unit| *unit == u16::from(b'\\') || *unit == u16::from(b'/')) {
            if component.is_empty() {
                continue;
            }
            let name = encode_wtf8(component);
            inode = match self.filesystem.lookup(inode, &name) {
                Ok((inode, _)) => inode,
                Err(Error::NOT_FOUND) => {
                    self.lookup_enumerated(inode, component, &mut name_equal)?
                }
                Err(error) => return Err(error),
            };
        }
        Ok(inode)
    }

    fn lookup_enumerated(
        &self,
        inode: u64,
        component: &[u16],
        name_equal: &mut dyn FnMut(&[u16], &[u16]) -> bool,
    ) -> crate::Result<u64> {
        struct MatchSink<'a> {
            component: &'a [u16],
            name_equal: &'a mut dyn FnMut(&[u16], &[u16]) -> bool,
            found: Option<u64>,
        }

        impl DirectorySink for MatchSink<'_> {
            fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
                if self.found.is_none()
                    && (self.name_equal)(&decode_wtf8(entry.name), self.component)
                {
                    self.found = Some(entry.inode);
                    false
                } else {
                    true
                }
            }
        }

        let mut sink = MatchSink {
            component,
            name_equal,
            found: None,
        };
        let mut cookie = 0;
        let mut verifier = 0;
        loop {
            let result = self
                .filesystem
                .read_directory(inode, cookie, verifier, &mut sink)?;
            if let Some(found) = sink.found {
                return Ok(found);
            }
            if result.eof {
                return Err(Error::NOT_FOUND);
            }
            if result.next_cookie <= cookie {
                return Err(Error::INVALID);
            }
            cookie = result.next_cookie;
            verifier = result.verifier;
        }
    }
}

#[derive(Default)]
struct Enumeration {
    entries: Vec<WindowsEntry>,
    index: usize,
    // None before the first page; an empty vector means no filter was supplied.
    search_expression: Option<Vec<u16>>,
}

impl Enumeration {
    /// `search_expression` must be null or a valid, terminated UTF-16 string.
    unsafe fn begin_page(&mut self, restart: bool, search_expression: PCWSTR) -> Option<&[u16]> {
        if restart || self.search_expression.is_none() {
            self.index = 0;
            // ProjFS supplies a callback-scoped expression. Capture it only on
            // the first page or on restart, and retain it for continuation pages.
            let expression = if search_expression.is_null() {
                Vec::new()
            } else {
                // SAFETY: the callback guarantees a valid terminated string.
                let mut expression = unsafe { wide_slice(search_expression) }.to_vec();
                expression.push(0);
                expression
            };
            self.search_expression = Some(expression);
        }
        self.search_expression
            .as_deref()
            .filter(|expression| !expression.is_empty())
    }
}

struct WindowsEntry {
    name: Vec<u16>,
    metadata: Metadata,
    symlink_target: Option<Vec<u16>>,
}

struct CollectSink<'a> {
    filesystem: &'a dyn Filesystem,
    entries: Vec<WindowsEntry>,
    error: Option<Error>,
}

fn read_symlink_target(
    filesystem: &dyn Filesystem,
    metadata: Metadata,
) -> crate::Result<Option<Vec<u16>>> {
    if metadata.kind != FileType::Symlink {
        return Ok(None);
    }
    let mut capacity = usize::try_from(metadata.size)
        .unwrap_or(MAX_SYMLINK_TARGET_BYTES)
        .clamp(1, MAX_SYMLINK_TARGET_BYTES);
    loop {
        let mut bytes = vec![0; capacity];
        match filesystem.read_symlink(metadata.inode, &mut bytes) {
            Ok(count) => {
                let count = crate::core::checked_io_count(count, bytes.len())?;
                bytes.truncate(count);
                let mut target = decode_wtf8(&bytes);
                if target.contains(&0) {
                    return Err(Error::INVALID);
                }
                target.push(0);
                return Ok(Some(target));
            }
            Err(Error::RANGE) if capacity < MAX_SYMLINK_TARGET_BYTES => {
                capacity = capacity.saturating_mul(2).min(MAX_SYMLINK_TARGET_BYTES);
            }
            Err(error) => return Err(error),
        }
    }
}

impl DirectorySink for CollectSink<'_> {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
        match self.filesystem.metadata(entry.inode).and_then(|metadata| {
            let symlink_target = read_symlink_target(self.filesystem, metadata)?;
            Ok((metadata, symlink_target))
        }) {
            Ok((metadata, symlink_target)) => {
                let mut name = decode_wtf8(entry.name);
                name.push(0);
                self.entries.push(WindowsEntry {
                    name,
                    metadata,
                    symlink_target,
                });
            }
            Err(error) => {
                self.error = Some(error);
                return false;
            }
        }
        true
    }
}

unsafe extern "system" fn start_directory(
    callback: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    // SAFETY: ProjFS owns both callback pointers for this invocation.
    let state = unsafe { WindowsState::from_callback(callback) };
    if let Err(result) = unsafe { state.before(callback, Operation::ReadDirectory, None, 0) } {
        return result;
    }
    let inode = match unsafe { state.inode_for_callback(callback) } {
        Ok(value) => value,
        Err(error) => return error_hresult(error),
    };
    let mut sink = CollectSink {
        filesystem: state.filesystem.as_ref(),
        entries: Vec::new(),
        error: None,
    };
    let mut cookie = 0;
    let mut verifier = 0;
    loop {
        match state
            .filesystem
            .read_directory(inode, cookie, verifier, &mut sink)
        {
            Ok(result) => {
                if let Some(error) = sink.error {
                    return error_hresult(error);
                }
                // Every non-final page must advance the cursor. Comparing with
                // `<=` also rejects a backend that rewinds, which would otherwise
                // loop here forever while growing `entries` without bound.
                if !result.eof && result.next_cookie <= cookie {
                    return error_hresult(Error::INVALID);
                }
                cookie = result.next_cookie;
                verifier = result.verifier;
                if result.eof {
                    break;
                }
            }
            Err(error) => return error_hresult(error),
        }
    }
    if let Some(error) = sink.error {
        return error_hresult(error);
    }
    sink.entries.sort_unstable_by(|left, right| {
        // SAFETY: collected names are null-terminated UTF-16 strings.
        unsafe { (state.api.file_name_compare)(left.name.as_ptr(), right.name.as_ptr()) }.cmp(&0)
    });
    // SAFETY: enumeration_id references one GUID.
    let key = unsafe { guid_bytes(enumeration_id) };
    state.enumerations.lock().insert(
        key,
        Arc::new(Mutex::new(Enumeration {
            entries: sink.entries,
            ..Enumeration::default()
        })),
    );
    S_OK
}

unsafe extern "system" fn end_directory(
    callback: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    // SAFETY: pointers are callback-scoped and valid.
    let state = unsafe { WindowsState::from_callback(callback) };
    state
        .enumerations
        .lock()
        .remove(&unsafe { guid_bytes(enumeration_id) });
    S_OK
}

unsafe extern "system" fn get_directory(
    callback: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
    search_expression: PCWSTR,
    buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    // SAFETY: pointers are callback-scoped and valid.
    let state = unsafe { WindowsState::from_callback(callback) };
    let key = unsafe { guid_bytes(enumeration_id) };
    let Some(session) = state.enumerations.lock().get(&key).cloned() else {
        return hresult(ERROR_INVALID_PARAMETER);
    };
    let mut session = session.lock();
    // SAFETY: callback references initialized callback data.
    let restart = unsafe { (*callback).Flags } & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN != 0;
    // SAFETY: ProjFS supplies null or a callback-scoped terminated string.
    let search_expression =
        unsafe { session.begin_page(restart, search_expression) }.map(<[u16]>::as_ptr);
    let mut added = false;
    while let Some(entry) = session.entries.get(session.index) {
        if let Some(expression) = search_expression
            // SAFETY: both strings are terminated and the captured expression
            // stays alive in `session` throughout this loop.
            && !unsafe { (state.api.file_name_match)(entry.name.as_ptr(), expression) }
        {
            session.index += 1;
            continue;
        }
        let info = basic_info(entry.metadata);
        // SAFETY: name and info remain alive for this call.
        let result = state.api.fill_directory_entry(
            entry.name.as_slice(),
            &info,
            buffer_handle,
            entry.symlink_target.as_deref(),
        );
        if result == hresult(ERROR_INSUFFICIENT_BUFFER) {
            // ProjFS requires this result when the buffer could not fit the
            // first entry it was offered, so the caller retries with a larger
            // buffer. Returning `S_OK` there would silently report an empty (or
            // truncated) directory.
            return if added { S_OK } else { result };
        }
        if result < 0 {
            return result;
        }
        added = true;
        session.index += 1;
        // SAFETY: callback references initialized callback data.
        if unsafe { (*callback).Flags } & PRJ_CB_DATA_FLAG_ENUM_RETURN_SINGLE_ENTRY != 0 {
            break;
        }
    }
    S_OK
}

unsafe extern "system" fn get_placeholder(callback: *const PRJ_CALLBACK_DATA) -> HRESULT {
    // SAFETY: callback is valid for this invocation.
    let state = unsafe { WindowsState::from_callback(callback) };
    if let Err(result) = unsafe { state.before(callback, Operation::Metadata, None, 0) } {
        return result;
    }
    let inode = match unsafe { state.inode_for_callback(callback) } {
        Ok(value) => value,
        Err(error) => return error_hresult(error),
    };
    let metadata = match state.filesystem.metadata(inode) {
        Ok(value) => value,
        Err(error) => return error_hresult(error),
    };
    let symlink_target = match read_symlink_target(state.filesystem.as_ref(), metadata) {
        Ok(value) => value,
        Err(error) => return error_hresult(error),
    };
    let mut info = PRJ_PLACEHOLDER_INFO {
        FileBasicInfo: basic_info(metadata),
        ..PRJ_PLACEHOLDER_INFO::default()
    };
    info.VersionInfo.ContentID[..8].copy_from_slice(&metadata.generation.to_le_bytes());
    // SAFETY: callback is valid for this invocation and both fields are read
    // only to pass them to the synchronous ProjFS call below.
    let (context, path) = unsafe {
        (
            (*callback).NamespaceVirtualizationContext,
            (*callback).FilePathName,
        )
    };
    state
        .api
        .write_placeholder(context, path, &info, symlink_target.as_deref())
}

unsafe extern "system" fn get_file_data(
    callback: *const PRJ_CALLBACK_DATA,
    offset: u64,
    length: u32,
) -> HRESULT {
    // SAFETY: callback is valid for this invocation.
    let state = unsafe { WindowsState::from_callback(callback) };
    if length == 0 {
        // Nothing to hydrate, and allocating a zero-length aligned buffer can
        // legitimately fail.
        return S_OK;
    }
    // SAFETY: ProjFS returns a suitably aligned buffer or null.
    let buffer = unsafe {
        (state.api.allocate_aligned_buffer)(
            (*callback).NamespaceVirtualizationContext,
            length as usize,
        )
    };
    let Some(pointer) = NonNull::new(buffer.cast::<u8>()) else {
        // `PrjAllocateAlignedBuffer` returns null when it cannot satisfy the
        // request, which is an out-of-memory condition rather than a hint that
        // the caller should retry with a larger buffer.
        return hresult(ERROR_NOT_ENOUGH_MEMORY);
    };
    // SAFETY: the allocated buffer is writable for length bytes.
    let raw_buffer = unsafe { RawBuffer::new(pointer, length as usize, true) };
    if let Err(result) = unsafe { state.before(callback, Operation::Read, Some(raw_buffer), 0) } {
        // SAFETY: buffer came from PrjAllocateAlignedBuffer.
        unsafe { (state.api.free_aligned_buffer)(buffer) };
        return result;
    }
    let inode = match unsafe { state.inode_for_callback(callback) } {
        Ok(value) => value,
        Err(error) => {
            // SAFETY: buffer came from PrjAllocateAlignedBuffer.
            unsafe { (state.api.free_aligned_buffer)(buffer) };
            return error_hresult(error);
        }
    };
    // SAFETY: buffer is live and writable until it is freed below.
    let bytes = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length as usize) };
    let count = match hydrate(offset, bytes, |position, output| {
        state.filesystem.read(inode, position, output)
    }) {
        Ok(value) => value,
        Err(error) => {
            // SAFETY: buffer came from PrjAllocateAlignedBuffer.
            unsafe { (state.api.free_aligned_buffer)(buffer) };
            return error_hresult(error);
        }
    };
    if count != length as usize {
        // SAFETY: buffer came from PrjAllocateAlignedBuffer.
        unsafe { (state.api.free_aligned_buffer)(buffer) };
        // The callback must supply the whole requested range. A short read that
        // cannot be resumed means the backend is shorter than the placeholder.
        return hresult(ERROR_HANDLE_EOF);
    }
    // SAFETY: callback GUID and buffer remain valid for the call.
    let result = unsafe {
        (state.api.write_file_data)(
            (*callback).NamespaceVirtualizationContext,
            &raw const (*callback).DataStreamId,
            buffer,
            offset,
            length,
        )
    };
    // SAFETY: buffer came from PrjAllocateAlignedBuffer.
    unsafe { (state.api.free_aligned_buffer)(buffer) };
    result
}

/// Fills `bytes` with file data starting at `offset`.
///
/// `ProjFS` requires the provider to supply at least the requested range, but
/// [`Filesystem::read`] may legally return a short read, so keep reading until
/// the range is full or the backend reports end of file. Stopping after a short
/// read would leave part of the hydrated range unwritten.
fn hydrate(
    offset: u64,
    bytes: &mut [u8],
    mut read: impl FnMut(u64, &mut [u8]) -> crate::Result<usize>,
) -> crate::Result<usize> {
    let mut filled = 0;
    while filled < bytes.len() {
        let position = offset.saturating_add(filled as u64);
        let count = read(position, &mut bytes[filled..])?;
        if count == 0 {
            break;
        }
        filled += crate::core::checked_io_count(count, bytes.len() - filled)?;
    }
    Ok(filled)
}

unsafe extern "system" fn query_file_name(callback: *const PRJ_CALLBACK_DATA) -> HRESULT {
    // SAFETY: callback is valid for this invocation.
    let state = unsafe { WindowsState::from_callback(callback) };
    if let Err(result) = unsafe { state.before(callback, Operation::Lookup, None, 0) } {
        return result;
    }
    match unsafe { state.inode_for_callback(callback) } {
        Ok(_) => S_OK,
        Err(error) => error_hresult(error),
    }
}

unsafe extern "system" fn notification(
    callback: *const PRJ_CALLBACK_DATA,
    _is_directory: bool,
    notification: PRJ_NOTIFICATION,
    _destination: PCWSTR,
    _parameters: *mut PRJ_NOTIFICATION_PARAMETERS,
) -> HRESULT {
    // SAFETY: callback is valid for this invocation.
    let state = unsafe { WindowsState::from_callback(callback) };
    unsafe {
        state
            .before(
                callback,
                Operation::Notification,
                None,
                u32::from_ne_bytes(notification.to_ne_bytes()),
            )
            .map_or_else(|error| error, |()| S_OK)
    }
}

unsafe extern "system" fn cancel_command(callback: *const PRJ_CALLBACK_DATA) {
    // SAFETY: callback is valid for this invocation.
    let state = unsafe { WindowsState::from_callback(callback) };
    let _ = unsafe { state.before(callback, Operation::Cancel, None, 0) };
}

fn basic_info(metadata: Metadata) -> PRJ_FILE_BASIC_INFO {
    let mut attributes = match metadata.kind {
        FileType::Directory => FILE_ATTRIBUTE_DIRECTORY,
        FileType::File => FILE_ATTRIBUTE_NORMAL,
        FileType::Symlink => FILE_ATTRIBUTE_REPARSE_POINT,
    };
    if metadata.mode & 0o222 == 0 {
        attributes &= !FILE_ATTRIBUTE_NORMAL;
        attributes |= FILE_ATTRIBUTE_READ_ONLY;
    }
    PRJ_FILE_BASIC_INFO {
        IsDirectory: metadata.kind == FileType::Directory,
        FileSize: i64::try_from(metadata.size).unwrap_or(i64::MAX),
        CreationTime: windows_time(metadata.created_ns),
        LastAccessTime: windows_time(metadata.accessed_ns),
        LastWriteTime: windows_time(metadata.modified_ns),
        ChangeTime: windows_time(metadata.modified_ns),
        FileAttributes: attributes,
    }
}

fn windows_time(nanoseconds: u64) -> i64 {
    let ticks = nanoseconds / 100 + WINDOWS_EPOCH_OFFSET_100NS;
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

fn error_hresult(error: Error) -> HRESULT {
    // Portable codes use Darwin's POSIX values, so each one needs an explicit
    // Win32 translation. Falling through to `ERROR_INVALID_FUNCTION` would turn
    // every defaulted mutation into "Incorrect function" and hide the reason
    // the provider rejected the callback.
    let code = match error {
        Error::IO => ERROR_IO_DEVICE,
        Error::INVALID => ERROR_INVALID_PARAMETER,
        Error::NOT_SUPPORTED => ERROR_NOT_SUPPORTED,
        Error::DIRECTORY_NOT_EMPTY => ERROR_DIR_NOT_EMPTY,
        Error::STALE => ERROR_FILE_INVALID,
        Error(1 | 13 | 21) => ERROR_ACCESS_DENIED, // EPERM, EACCES, EISDIR
        Error(2) => ERROR_FILE_NOT_FOUND,          // ENOENT
        Error(17) => ERROR_FILE_EXISTS,            // EEXIST
        Error(20) => ERROR_DIRECTORY,              // ENOTDIR
        Error(28) => ERROR_DISK_FULL,              // ENOSPC
        Error(30) => ERROR_WRITE_PROTECT,          // EROFS
        _ => ERROR_INVALID_FUNCTION,
    };
    hresult(code)
}

fn hresult(code: u32) -> HRESULT {
    i32::from_ne_bytes((0x8007_0000_u32 | (code & 0xffff)).to_ne_bytes())
}

fn check_placeholder_mark_hresult(value: HRESULT) -> io::Result<()> {
    if value >= 0 || value == hresult(ERROR_REPARSE_POINT_ENCOUNTERED) {
        Ok(())
    } else {
        check_hresult(value)
    }
}

fn check_hresult(value: HRESULT) -> io::Result<()> {
    if value >= 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Windows filesystem API failed with HRESULT 0x{:08x}",
            u32::from_ne_bytes(value.to_ne_bytes())
        )))
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Encodes a path relative to the virtualization root for a `ProjFS` API.
fn wide_relative_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut wide = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                // Win32 strips trailing dots and spaces from each component, so
                // a name ending in either would address a different entry.
                if name.is_empty()
                    || name
                        .as_encoded_bytes()
                        .last()
                        .is_some_and(|byte| matches!(byte, b'.' | b' '))
                {
                    return Err(invalid_eviction_path(path));
                }
                if !wide.is_empty() {
                    wide.push(u16::from(b'\\'));
                }
                for unit in name.encode_wide() {
                    if unit == 0 {
                        return Err(invalid_eviction_path(path));
                    }
                    wide.push(unit);
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(invalid_eviction_path(path));
            }
        }
    }
    if wide.is_empty() {
        return Err(invalid_eviction_path(path));
    }
    wide.push(0);
    Ok(wide)
}

fn invalid_eviction_path(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "eviction path must be relative to the mount; components may not be a root, prefix, `..`, or end in `.` or a space: {}",
            path.display()
        ),
    )
}

fn classify_eviction(
    result: HRESULT,
    failure: PRJ_UPDATE_FAILURE_CAUSES,
) -> io::Result<EvictionOutcome> {
    if result >= 0
        || result == hresult(ERROR_FILE_NOT_FOUND)
        || result == hresult(ERROR_PATH_NOT_FOUND)
    {
        return Ok(EvictionOutcome::Evicted);
    }
    if result == hresult(ERROR_FILE_SYSTEM_VIRTUALIZATION_INVALID_OPERATION) {
        return Ok(EvictionOutcome::Retained);
    }
    if result == hresult(ERROR_SHARING_VIOLATION) || result == hresult(ERROR_DIR_NOT_EMPTY) {
        return Ok(EvictionOutcome::Retained);
    }
    Err(io::Error::other(format!(
        "ProjFS could not evict the item: HRESULT 0x{:08x} (failure 0x{:08x})",
        u32::from_ne_bytes(result.to_ne_bytes()),
        u32::from_ne_bytes(failure.to_ne_bytes()),
    )))
}

unsafe fn wide_slice<'a>(pointer: PCWSTR) -> &'a [u16] {
    if pointer.is_null() {
        return &[];
    }
    let mut length = 0;
    // SAFETY: ProjFS supplies a valid null-terminated PCWSTR.
    while unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the scan above established the initialized range.
    unsafe { std::slice::from_raw_parts(pointer, length) }
}

unsafe fn guid_bytes(pointer: *const GUID) -> [u8; 16] {
    let mut bytes = [0; 16];
    // SAFETY: GUID has exactly 16 readable bytes.
    unsafe {
        ptr::copy_nonoverlapping(pointer.cast::<u8>(), bytes.as_mut_ptr(), bytes.len());
    }
    bytes
}

/// Encode UTF-16, including unpaired surrogates, as WTF-8.
fn encode_wtf8(units: &[u16]) -> Vec<u8> {
    let mut output = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        let scalar = if (0xd800..=0xdbff).contains(&unit)
            && units
                .get(index + 1)
                .is_some_and(|next| (0xdc00..=0xdfff).contains(next))
        {
            let next = units[index + 1];
            index += 1;
            0x1_0000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(next) - 0xdc00)
        } else {
            u32::from(unit)
        };
        push_utf8(&mut output, scalar);
        index += 1;
    }
    output
}

fn decode_wtf8(bytes: &[u8]) -> Vec<u16> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let first = bytes[index];
        let (mut scalar, count) = if first < 0x80 {
            (u32::from(first), 1)
        } else if first & 0xe0 == 0xc0 {
            (u32::from(first & 0x1f), 2)
        } else if first & 0xf0 == 0xe0 {
            (u32::from(first & 0x0f), 3)
        } else {
            (u32::from(first & 0x07), 4)
        };
        if index + count > bytes.len() {
            scalar = 0xfffd;
        } else {
            for byte in &bytes[index + 1..index + count] {
                scalar = (scalar << 6) | u32::from(byte & 0x3f);
            }
        }
        if scalar <= 0xffff {
            output.push(u16::try_from(scalar).unwrap_or(0xfffd));
        } else {
            let value = scalar - 0x1_0000;
            output.push(0xd800 | u16::try_from(value >> 10).unwrap_or(0));
            output.push(0xdc00 | u16::try_from(value & 0x3ff).unwrap_or(0));
        }
        index += count;
    }
    output
}

fn push_utf8(output: &mut Vec<u8>, scalar: u32) {
    if scalar <= 0x7f {
        output.push(u8::try_from(scalar).unwrap_or(b'?'));
    } else if scalar <= 0x7ff {
        output.push(0xc0 | u8::try_from(scalar >> 6).unwrap_or(0));
        output.push(0x80 | u8::try_from(scalar & 0x3f).unwrap_or(0));
    } else if scalar <= 0xffff {
        output.push(0xe0 | u8::try_from(scalar >> 12).unwrap_or(0));
        output.push(0x80 | u8::try_from((scalar >> 6) & 0x3f).unwrap_or(0));
        output.push(0x80 | u8::try_from(scalar & 0x3f).unwrap_or(0));
    } else {
        output.push(0xf0 | u8::try_from(scalar >> 18).unwrap_or(0));
        output.push(0x80 | u8::try_from((scalar >> 12) & 0x3f).unwrap_or(0));
        output.push(0x80 | u8::try_from((scalar >> 6) & 0x3f).unwrap_or(0));
        output.push(0x80 | u8::try_from(scalar & 0x3f).unwrap_or(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win32_code(value: HRESULT) -> u32 {
        u32::from_ne_bytes(value.to_ne_bytes()) & 0xffff
    }

    unsafe extern "system" fn unused_delete(
        _context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
        _path: PCWSTR,
        _flags: PRJ_UPDATE_TYPES,
        _failure: *mut PRJ_UPDATE_FAILURE_CAUSES,
    ) -> HRESULT {
        panic!("a stopped mount must not call PrjDeleteFile");
    }

    #[test]
    fn directory_search_expression_persists_across_pages() {
        use windows_sys::w;

        let mut session = Enumeration::default();
        let mut initial: Vec<u16> = "*.txt\0".encode_utf16().collect();
        // SAFETY: all strings passed to these functions are terminated or null.
        unsafe {
            assert!(session.begin_page(false, initial.as_ptr()).is_some());
            initial.fill(0);
            session.index = 2;

            for input in [ptr::null(), w!("*.bin")] {
                let expression = session.begin_page(false, input).unwrap();
                assert_eq!(expression, "*.txt".encode_utf16().collect::<Vec<_>>());
                assert_eq!(session.index, 2);
            }
        }
    }

    #[test]
    fn directory_search_expression_is_recaptured_on_restart() {
        use windows_sys::w;

        let mut session = Enumeration::default();
        // SAFETY: all strings passed to these functions are terminated or null.
        unsafe {
            assert!(session.begin_page(false, ptr::null()).is_none());
            session.index = 2;
            assert!(session.begin_page(false, w!("*.txt")).is_none());
            assert_eq!(session.index, 2);

            assert!(session.begin_page(true, w!("*.txt")).is_some());
            session.index = 1;
            let expression = session.begin_page(true, w!("*.bin")).unwrap();
            assert_eq!(expression, "*.bin".encode_utf16().collect::<Vec<_>>());
            assert_eq!(session.index, 0);

            session.index = 1;
            assert!(session.begin_page(true, ptr::null()).is_none());
            assert_eq!(session.index, 0);
        }
    }

    #[test]
    fn wtf8_round_trips_unpaired_surrogates() {
        let value = [u16::from(b'a'), 0xd800, 0xd83d, 0xde80];
        assert_eq!(decode_wtf8(&encode_wtf8(&value)), value);
    }

    #[test]
    fn portable_errors_keep_their_meaning_as_win32_codes() {
        assert_eq!(win32_code(error_hresult(Error::IO)), ERROR_IO_DEVICE);
        assert_eq!(
            win32_code(error_hresult(Error::INVALID)),
            ERROR_INVALID_PARAMETER
        );
        assert_eq!(
            win32_code(error_hresult(Error::NOT_SUPPORTED)),
            ERROR_NOT_SUPPORTED
        );
        assert_eq!(
            win32_code(error_hresult(Error::DIRECTORY_NOT_EMPTY)),
            ERROR_DIR_NOT_EMPTY
        );
        assert_eq!(win32_code(error_hresult(Error::STALE)), ERROR_FILE_INVALID);
        assert_eq!(win32_code(error_hresult(Error(2))), ERROR_FILE_NOT_FOUND);
        assert_eq!(win32_code(error_hresult(Error(13))), ERROR_ACCESS_DENIED);
        assert_eq!(win32_code(error_hresult(Error(17))), ERROR_FILE_EXISTS);
        assert_eq!(win32_code(error_hresult(Error(20))), ERROR_DIRECTORY);
        assert_eq!(win32_code(error_hresult(Error(28))), ERROR_DISK_FULL);
        assert_eq!(win32_code(error_hresult(Error(30))), ERROR_WRITE_PROTECT);
        // An unknown code must still be a failure rather than a success.
        assert!(error_hresult(Error(9999)) < 0);
    }

    #[test]
    fn placeholder_mark_accepts_an_existing_virtualization_root() {
        assert!(check_placeholder_mark_hresult(hresult(ERROR_REPARSE_POINT_ENCOUNTERED)).is_ok());
        assert!(check_hresult(hresult(ERROR_REPARSE_POINT_ENCOUNTERED)).is_err());
    }

    #[test]
    fn symlink_metadata_is_a_reparse_point() {
        let mut metadata = Metadata::file(2, ROOT_INODE, 4, 0o777);
        metadata.kind = FileType::Symlink;
        let info = basic_info(metadata);
        assert_eq!(info.FileAttributes, FILE_ATTRIBUTE_REPARSE_POINT);
    }

    #[test]
    fn eviction_paths_stay_inside_the_mount() {
        for path in [
            "",
            ".",
            "..",
            "../file",
            "dir/..",
            "dir/../../file",
            "/file",
            r"C:\file",
            "C:file",
            r"\\server\share\file",
            ".. ",
            "file.",
        ] {
            assert!(
                wide_relative_path(Path::new(path)).is_err(),
                "accepted {path:?}"
            );
        }
    }

    #[test]
    fn eviction_paths_are_normalized_utf16() {
        let wide = wide_relative_path(Path::new("dir/./file.txt")).unwrap();
        let expected: Vec<u16> = "dir\\file.txt".encode_utf16().chain(Some(0)).collect();
        assert_eq!(wide, expected);
    }

    #[test]
    fn eviction_reports_a_stopped_mount() {
        let state = EvictionState {
            context: RwLock::new(None),
        };
        let error = state
            .evict(Path::new("file.txt"), unused_delete)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    }

    #[test]
    fn eviction_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EvictionHandle>();
        assert_send_sync::<MountSession>();
    }

    #[test]
    fn eviction_classifies_items_that_must_be_retained() {
        const PRJ_UPDATE_FAILURE_CAUSE_DIRTY_DATA: i32 = 2;
        let invalid = hresult(ERROR_FILE_SYSTEM_VIRTUALIZATION_INVALID_OPERATION);
        assert_eq!(
            classify_eviction(invalid, PRJ_UPDATE_FAILURE_CAUSE_DIRTY_DATA).unwrap(),
            EvictionOutcome::Retained
        );
        assert_eq!(
            classify_eviction(invalid, PRJ_UPDATE_FAILURE_CAUSE_NONE).unwrap(),
            EvictionOutcome::Retained,
            "an invalid-operation result means the item's state prevented eviction"
        );
        assert_eq!(
            classify_eviction(hresult(ERROR_FILE_NOT_FOUND), PRJ_UPDATE_FAILURE_CAUSE_NONE)
                .unwrap(),
            EvictionOutcome::Evicted
        );
        assert_eq!(
            classify_eviction(hresult(ERROR_PATH_NOT_FOUND), PRJ_UPDATE_FAILURE_CAUSE_NONE)
                .unwrap(),
            EvictionOutcome::Evicted
        );
        assert_eq!(
            classify_eviction(
                hresult(ERROR_SHARING_VIOLATION),
                PRJ_UPDATE_FAILURE_CAUSE_NONE
            )
            .unwrap(),
            EvictionOutcome::Retained
        );
        assert_eq!(
            classify_eviction(hresult(ERROR_DIR_NOT_EMPTY), PRJ_UPDATE_FAILURE_CAUSE_NONE).unwrap(),
            EvictionOutcome::Retained
        );
        assert_eq!(
            classify_eviction(S_OK, PRJ_UPDATE_FAILURE_CAUSE_NONE).unwrap(),
            EvictionOutcome::Evicted
        );
        assert!(
            classify_eviction(hresult(ERROR_ACCESS_DENIED), PRJ_UPDATE_FAILURE_CAUSE_NONE).is_err(),
            "an access denial is not a normal item-state outcome"
        );
    }

    #[test]
    fn hydration_fills_the_range_across_short_reads() {
        let source = *b"abcdefghij";
        let mut output = [0_u8; 10];
        let mut calls = 0;
        let count = hydrate(0, &mut output, |position, target| {
            calls += 1;
            let start = usize::try_from(position).unwrap();
            let available = &source[start..];
            let chunk = available.len().min(3).min(target.len());
            target[..chunk].copy_from_slice(&available[..chunk]);
            Ok(chunk)
        })
        .unwrap();
        assert_eq!(count, source.len());
        assert_eq!(output, source);
        assert!(calls > 1, "short reads should be resumed");
    }

    #[test]
    fn hydration_stops_at_end_of_file() {
        let source = *b"abc";
        let mut output = [0_u8; 8];
        let count = hydrate(0, &mut output, |position, target| {
            let start = usize::try_from(position).unwrap();
            let chunk = source.len().saturating_sub(start).min(target.len());
            target[..chunk].copy_from_slice(&source[start..start + chunk]);
            Ok(chunk)
        })
        .unwrap();
        assert_eq!(count, source.len());
        assert_eq!(&output[..count], &source);
    }

    #[test]
    fn hydration_rejects_counts_beyond_the_remaining_range() {
        let mut output = [0_u8; 4];
        assert_eq!(
            hydrate(0, &mut output, |_, target| Ok(target.len() + 1)),
            Err(Error::INVALID)
        );
    }
}
