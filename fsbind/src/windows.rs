//! Windows `ProjFS` adapter.

use crate::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, NetworkFilesystem,
    ROOT_INODE,
    raw::{
        Caller, NativeRequest, Operation, Platform, RawBuffer, RawEvent, RawEventMask, RawOutcome,
    },
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    ffi::c_void,
    fs, io,
    mem::size_of,
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    sync::Arc,
};
#[allow(clippy::wildcard_imports)]
use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
            ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
        },
        Storage::ProjectedFileSystem::*,
        System::Com::CoCreateGuid,
    },
    core::{GUID, HRESULT, PCWSTR},
};

const S_OK: HRESULT = 0;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const WINDOWS_EPOCH_OFFSET_100NS: u64 = 116_444_736_000_000_000;

#[derive(Clone, Debug)]
pub struct MountOptions {
    pub read_only: bool,
    pub filesystem_name: String,
    pub worker_threads: usize,
    pub use_negative_path_cache: bool,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            filesystem_name: "fsbind".into(),
            worker_threads: std::thread::available_parallelism().map_or(4, usize::from),
            use_negative_path_cache: true,
        }
    }
}

/// An active Windows `ProjFS` virtualization root.
///
/// `ProjFS` is lazy by design: projected metadata and contents are materialized
/// into the Windows cache on demand. Local edits remain in that cache and are
/// reported through raw notification events.
pub struct MountSession {
    mount_path: PathBuf,
    context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
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

    #[must_use]
    pub fn mount_path(&self) -> &Path {
        &self.mount_path
    }

    pub fn unmount(self) {}

    fn start(
        filesystem: Arc<dyn Filesystem>,
        mount_path: &Path,
        options: &MountOptions,
    ) -> io::Result<Self> {
        fs::create_dir_all(mount_path)?;
        let wide_path = wide_path(mount_path);
        let mut instance_id = GUID::default();
        // SAFETY: CoCreateGuid writes exactly one GUID.
        check_hresult(unsafe { CoCreateGuid(&raw mut instance_id) })?;
        // SAFETY: all pointers reference initialized values for this call.
        check_hresult(unsafe {
            PrjMarkDirectoryAsPlaceholder(
                wide_path.as_ptr(),
                ptr::null(),
                ptr::null(),
                &raw const instance_id,
            )
        })?;

        let mut state = Box::new(WindowsState::new(filesystem));
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
            PrjStartVirtualizing(
                wide_path.as_ptr(),
                &raw const callbacks,
                (&raw mut *state).cast(),
                &raw const start_options,
                &raw mut context,
            )
        })?;
        Ok(Self {
            mount_path: mount_path.to_owned(),
            context,
            _state: state,
        })
    }
}

impl Drop for MountSession {
    fn drop(&mut self) {
        if !self.context.is_null() {
            // SAFETY: context was returned by PrjStartVirtualizing and is live.
            unsafe { PrjStopVirtualizing(self.context) };
            self.context = ptr::null_mut();
        }
    }
}

struct WindowsState {
    filesystem: Arc<dyn Filesystem>,
    raw_mask: RawEventMask,
    enumerations: Mutex<HashMap<[u8; 16], Arc<Mutex<Enumeration>>>>,
}

impl WindowsState {
    fn new(filesystem: Arc<dyn Filesystem>) -> Self {
        let raw_mask = filesystem.raw_event_mask();
        Self {
            filesystem,
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
        let mut inode = ROOT_INODE;
        for component in path.split(|unit| *unit == u16::from(b'\\') || *unit == u16::from(b'/')) {
            if component.is_empty() {
                continue;
            }
            let name = encode_wtf8(component);
            inode = self.filesystem.lookup(inode, &name)?.0;
        }
        Ok(inode)
    }
}

#[derive(Default)]
struct Enumeration {
    entries: Vec<WindowsEntry>,
    index: usize,
}

struct WindowsEntry {
    name: Vec<u16>,
    metadata: Metadata,
}

struct CollectSink<'a> {
    filesystem: &'a dyn Filesystem,
    entries: Vec<WindowsEntry>,
    error: Option<Error>,
}

impl DirectorySink for CollectSink<'_> {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
        match self.filesystem.metadata(entry.inode) {
            Ok(metadata) => {
                let mut name = decode_wtf8(entry.name);
                name.push(0);
                self.entries.push(WindowsEntry { name, metadata });
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
                if !result.eof && result.next_cookie == cookie {
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
        unsafe { PrjFileNameCompare(left.name.as_ptr(), right.name.as_ptr()) }.cmp(&0)
    });
    // SAFETY: enumeration_id references one GUID.
    let key = unsafe { guid_bytes(enumeration_id) };
    state.enumerations.lock().insert(
        key,
        Arc::new(Mutex::new(Enumeration {
            entries: sink.entries,
            index: 0,
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
    if unsafe { (*callback).Flags } & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN != 0 {
        session.index = 0;
    }
    while let Some(entry) = session.entries.get(session.index) {
        if !search_expression.is_null()
            // SAFETY: both are null-terminated UTF-16 strings.
            && !unsafe { PrjFileNameMatch(entry.name.as_ptr(), search_expression) }
        {
            session.index += 1;
            continue;
        }
        let info = basic_info(entry.metadata);
        // SAFETY: name and info remain alive for this call.
        let result =
            unsafe { PrjFillDirEntryBuffer(entry.name.as_ptr(), &raw const info, buffer_handle) };
        if result == hresult(ERROR_INSUFFICIENT_BUFFER) {
            break;
        }
        if result < 0 {
            return result;
        }
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
    let mut info = PRJ_PLACEHOLDER_INFO {
        FileBasicInfo: basic_info(metadata),
        ..PRJ_PLACEHOLDER_INFO::default()
    };
    info.VersionInfo.ContentID[..8].copy_from_slice(&metadata.generation.to_le_bytes());
    // SAFETY: callback and info are valid for this synchronous call.
    unsafe {
        PrjWritePlaceholderInfo(
            (*callback).NamespaceVirtualizationContext,
            (*callback).FilePathName,
            &raw const info,
            u32::try_from(size_of::<PRJ_PLACEHOLDER_INFO>()).unwrap_or(u32::MAX),
        )
    }
}

unsafe extern "system" fn get_file_data(
    callback: *const PRJ_CALLBACK_DATA,
    offset: u64,
    length: u32,
) -> HRESULT {
    // SAFETY: callback is valid for this invocation.
    let state = unsafe { WindowsState::from_callback(callback) };
    // SAFETY: ProjFS returns a suitably aligned buffer or null.
    let buffer = unsafe {
        PrjAllocateAlignedBuffer((*callback).NamespaceVirtualizationContext, length as usize)
    };
    let Some(pointer) = NonNull::new(buffer.cast::<u8>()) else {
        return hresult(ERROR_INSUFFICIENT_BUFFER);
    };
    // SAFETY: the allocated buffer is writable for length bytes.
    let raw_buffer = unsafe { RawBuffer::new(pointer, length as usize, true) };
    if let Err(result) = unsafe { state.before(callback, Operation::Read, Some(raw_buffer), 0) } {
        // SAFETY: buffer came from PrjAllocateAlignedBuffer.
        unsafe { PrjFreeAlignedBuffer(buffer) };
        return result;
    }
    let inode = match unsafe { state.inode_for_callback(callback) } {
        Ok(value) => value,
        Err(error) => {
            // SAFETY: buffer came from PrjAllocateAlignedBuffer.
            unsafe { PrjFreeAlignedBuffer(buffer) };
            return error_hresult(error);
        }
    };
    // SAFETY: buffer is live and writable until it is freed below.
    let bytes = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length as usize) };
    let count = match state
        .filesystem
        .read(inode, offset, bytes)
        .and_then(|count| crate::core::checked_io_count(count, bytes.len()))
    {
        Ok(value) => value,
        Err(error) => {
            // SAFETY: buffer came from PrjAllocateAlignedBuffer.
            unsafe { PrjFreeAlignedBuffer(buffer) };
            return error_hresult(error);
        }
    };
    // SAFETY: callback GUID and buffer remain valid for the call.
    let result = unsafe {
        PrjWriteFileData(
            (*callback).NamespaceVirtualizationContext,
            &raw const (*callback).DataStreamId,
            buffer,
            offset,
            u32::try_from(count).unwrap_or(u32::MAX),
        )
    };
    // SAFETY: buffer came from PrjAllocateAlignedBuffer.
    unsafe { PrjFreeAlignedBuffer(buffer) };
    result
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
    PRJ_FILE_BASIC_INFO {
        IsDirectory: metadata.kind == FileType::Directory,
        FileSize: i64::try_from(metadata.size).unwrap_or(i64::MAX),
        CreationTime: windows_time(metadata.created_ns),
        LastAccessTime: windows_time(metadata.accessed_ns),
        LastWriteTime: windows_time(metadata.modified_ns),
        ChangeTime: windows_time(metadata.modified_ns),
        FileAttributes: if metadata.kind == FileType::Directory {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        },
    }
}

fn windows_time(nanoseconds: u64) -> i64 {
    let ticks = nanoseconds / 100 + WINDOWS_EPOCH_OFFSET_100NS;
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

fn error_hresult(error: Error) -> HRESULT {
    match error.0 {
        2 => hresult(ERROR_FILE_NOT_FOUND),
        13 => hresult(ERROR_ACCESS_DENIED),
        22 => hresult(ERROR_INVALID_PARAMETER),
        _ => hresult(ERROR_INVALID_FUNCTION),
    }
}

fn hresult(code: u32) -> HRESULT {
    i32::from_ne_bytes((0x8007_0000_u32 | (code & 0xffff)).to_ne_bytes())
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

    #[test]
    fn wtf8_round_trips_unpaired_surrogates() {
        let value = [u16::from(b'a'), 0xd800, 0xd83d, 0xde80];
        assert_eq!(decode_wtf8(&encode_wtf8(&value)), value);
    }
}
