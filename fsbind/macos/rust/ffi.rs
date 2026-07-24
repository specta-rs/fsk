//! In-process C ABI used by macOS bridge tests and compatibility clients.

use crate::{
    ABI_VERSION, DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, Mount,
    NetworkFile, NetworkFilesystem, ReadDirectoryResult, SetMetadata, StatFs,
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    ffi::c_void,
    os::unix::ffi::OsStrExt,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr, slice,
    sync::atomic::{AtomicU64, Ordering},
};

const EIO: i32 = 5;
const EINVAL: i32 = 22;
const ERANGE: i32 = 34;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsbAttributes {
    pub inode: u64,
    pub parent: u64,
    pub size: u64,
    pub allocated_size: u64,
    pub generation: u64,
    pub created_ns: u64,
    pub modified_ns: u64,
    pub accessed_ns: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub link_count: u32,
    pub kind: u8,
    pub reserved: [u8; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsbSetAttributes {
    pub valid: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub accessed_ns: u64,
    pub modified_ns: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsbStatFs {
    pub block_size: u32,
    pub io_size: u32,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub files: u64,
    pub free_files: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsbDirEntry {
    pub inode: u64,
    pub next_cookie: u64,
    pub name_offset: u32,
    pub name_length: u16,
    pub kind: u8,
    pub reserved: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsbConfiguration {
    pub kind: u8,
    pub lazy: u8,
    pub supports_local_files: u8,
    pub kernel_cache_policy: u8,
    pub reserved: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsbOpenFile {
    pub lease: u64,
    pub generation: u64,
    pub path_length: usize,
    pub source: u8,
    pub reserved: [u8; 7],
}

enum Backend {
    Local(Box<dyn Filesystem>),
    Network(Box<dyn NetworkFilesystem>),
}

impl Backend {
    fn filesystem(&self) -> &dyn Filesystem {
        match self {
            Self::Local(filesystem) => filesystem.as_ref(),
            Self::Network(filesystem) => filesystem.as_ref(),
        }
    }

    fn network(&self) -> Option<&dyn NetworkFilesystem> {
        match self {
            Self::Local(_) => None,
            Self::Network(filesystem) => Some(filesystem.as_ref()),
        }
    }
}

struct Volume {
    backend: Backend,
    leases: Mutex<HashMap<u64, (u64, NetworkFile)>>,
    next_lease: AtomicU64,
}

impl Volume {
    fn new(backend: Backend) -> Self {
        Self {
            backend,
            leases: Mutex::new(HashMap::new()),
            next_lease: AtomicU64::new(1),
        }
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        let Some(network) = self.backend.network() else {
            return;
        };
        for (_, (inode, file)) in self.leases.get_mut().drain() {
            let _ = network.close_file(inode, file);
        }
    }
}

pub fn create_volume(filesystem: impl Filesystem) -> *mut c_void {
    Box::into_raw(Box::new(Volume::new(Backend::Local(Box::new(filesystem))))).cast()
}

fn create_network_volume(filesystem: impl NetworkFilesystem) -> *mut c_void {
    Box::into_raw(Box::new(Volume {
        backend: Backend::Network(Box::new(filesystem)),
        leases: Mutex::new(HashMap::new()),
        next_lease: AtomicU64::new(1),
    }))
    .cast()
}

/// Builds an exported network volume from the raw mount URL.
///
/// # Safety
///
/// `resource` must reference `resource_length` readable bytes when the length is
/// nonzero. `error_out`, when non-null, must be writable.
pub unsafe fn create_network_volume_from_raw<T>(
    resource: *const u8,
    resource_length: usize,
    error_out: *mut i32,
    factory: impl FnOnce(&Mount) -> Result<T, Error>,
) -> *mut c_void
where
    T: NetworkFilesystem,
{
    let result = catch_unwind(AssertUnwindSafe(|| {
        let resource = unsafe { bytes(resource, resource_length)? };
        factory(&Mount::new(resource.to_vec()))
    }));
    match result {
        Ok(Ok(filesystem)) => {
            if !error_out.is_null() {
                unsafe { ptr::write(error_out, 0) };
            }
            create_network_volume(filesystem)
        }
        Ok(Err(error)) => {
            if !error_out.is_null() {
                unsafe { ptr::write(error_out, error.0.max(1)) };
            }
            ptr::null_mut()
        }
        Err(_) => {
            if !error_out.is_null() {
                unsafe { ptr::write(error_out, EIO) };
            }
            ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn fsb_abi_version() -> u32 {
    ABI_VERSION
}

unsafe fn volume_handle<'a>(pointer: *mut c_void) -> Result<&'a Volume, Error> {
    if pointer.is_null() {
        return Err(Error(EINVAL));
    }
    Ok(unsafe { &*pointer.cast::<Volume>() })
}

unsafe fn volume<'a>(pointer: *mut c_void) -> Result<&'a dyn Filesystem, Error> {
    Ok(unsafe { volume_handle(pointer)? }.backend.filesystem())
}

fn status(function: impl FnOnce() -> Result<(), Error>) -> i32 {
    match catch_unwind(AssertUnwindSafe(function)) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => error.0.max(1),
        Err(_) => EIO,
    }
}

unsafe fn bytes<'a>(pointer: *const u8, length: usize) -> Result<&'a [u8], Error> {
    if length == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() {
        return Err(Error(EINVAL));
    }
    Ok(unsafe { slice::from_raw_parts(pointer, length) })
}

unsafe fn bytes_mut<'a>(pointer: *mut u8, length: usize) -> Result<&'a mut [u8], Error> {
    if length == 0 {
        return Ok(&mut []);
    }
    if pointer.is_null() {
        return Err(Error(EINVAL));
    }
    Ok(unsafe { slice::from_raw_parts_mut(pointer, length) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_volume_destroy(pointer: *mut c_void) {
    if !pointer.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            drop(unsafe { Box::from_raw(pointer.cast::<Volume>()) });
        }));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_configuration(
    pointer: *mut c_void,
    output: *mut FsbConfiguration,
) -> i32 {
    status(|| {
        if output.is_null() {
            return Err(Error(EINVAL));
        }
        let volume = unsafe { volume_handle(pointer)? };
        let configuration = if let Some(network) = volume.backend.network() {
            let configuration = network.network_configuration();
            FsbConfiguration {
                kind: 1,
                lazy: u8::from(configuration.lazy),
                supports_local_files: u8::from(configuration.supports_local_files),
                kernel_cache_policy: configuration.kernel_cache_policy as u8,
                reserved: [0; 4],
            }
        } else {
            FsbConfiguration::default()
        };
        unsafe { ptr::write(output, configuration) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_open_file(
    pointer: *mut c_void,
    inode: u64,
    writable: u8,
    path: *mut u8,
    path_capacity: usize,
    output: *mut FsbOpenFile,
) -> i32 {
    status(|| {
        if output.is_null() {
            return Err(Error(EINVAL));
        }
        let volume = unsafe { volume_handle(pointer)? };
        let Some(network) = volume.backend.network() else {
            unsafe { ptr::write(output, FsbOpenFile::default()) };
            return Ok(());
        };
        let file = network.open_file(inode, writable != 0)?;
        let (source, generation, path_bytes) = match &file {
            NetworkFile::Remote => (0, 0, &[][..]),
            NetworkFile::Local(local) => {
                (1, local.generation(), local.path().as_os_str().as_bytes())
            }
        };
        if path_bytes.len() > path_capacity || (!path_bytes.is_empty() && path.is_null()) {
            let _ = network.close_file(inode, file);
            return Err(Error(ERANGE));
        }
        if !path_bytes.is_empty() {
            unsafe { ptr::copy_nonoverlapping(path_bytes.as_ptr(), path, path_bytes.len()) };
        }
        let path_length = path_bytes.len();
        let lease = volume.next_lease.fetch_add(1, Ordering::Relaxed).max(1);
        volume.leases.lock().insert(lease, (inode, file));
        unsafe {
            ptr::write(
                output,
                FsbOpenFile {
                    lease,
                    generation,
                    path_length,
                    source,
                    reserved: [0; 7],
                },
            );
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_close_file(pointer: *mut c_void, lease: u64) -> i32 {
    status(|| {
        if lease == 0 {
            return Ok(());
        }
        let volume = unsafe { volume_handle(pointer)? };
        let network = volume.backend.network().ok_or(Error(EINVAL))?;
        let (inode, file) = volume.leases.lock().remove(&lease).ok_or(Error(EINVAL))?;
        network.close_file(inode, file)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_statfs(pointer: *mut c_void, output: *mut FsbStatFs) -> i32 {
    status(|| {
        if output.is_null() {
            return Err(Error(EINVAL));
        }
        let value = unsafe { volume(pointer)? }.statfs()?;
        unsafe { ptr::write(output, value.into()) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_getattr(
    pointer: *mut c_void,
    inode: u64,
    output: *mut FsbAttributes,
) -> i32 {
    status(|| {
        if output.is_null() {
            return Err(Error(EINVAL));
        }
        let value = unsafe { volume(pointer)? }.metadata(inode)?;
        unsafe { ptr::write(output, value.into()) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_setattr(
    pointer: *mut c_void,
    inode: u64,
    input: *const FsbSetAttributes,
    output: *mut FsbAttributes,
) -> i32 {
    status(|| {
        if input.is_null() || output.is_null() {
            return Err(Error(EINVAL));
        }
        let update = unsafe { (*input).into() };
        let value = unsafe { volume(pointer)? }.set_metadata(inode, update)?;
        unsafe { ptr::write(output, value.into()) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_lookup(
    pointer: *mut c_void,
    parent: u64,
    name: *const u8,
    name_length: usize,
    inode_out: *mut u64,
    kind_out: *mut u8,
) -> i32 {
    status(|| {
        if inode_out.is_null() || kind_out.is_null() {
            return Err(Error(EINVAL));
        }
        let name = unsafe { bytes(name, name_length)? };
        let (inode, kind) = unsafe { volume(pointer)? }.lookup(parent, name)?;
        unsafe {
            ptr::write(inode_out, inode);
            ptr::write(kind_out, kind as u8);
        }
        Ok(())
    })
}

struct PackedSink<'a> {
    entries: &'a mut [FsbDirEntry],
    names: &'a mut [u8],
    entry_count: usize,
    name_count: usize,
}

impl DirectorySink for PackedSink<'_> {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
        if self.entry_count == self.entries.len()
            || entry.name.len() > u16::MAX as usize
            || self.name_count + entry.name.len() > self.names.len()
        {
            return false;
        }
        let name_offset = self.name_count;
        self.names[name_offset..name_offset + entry.name.len()].copy_from_slice(entry.name);
        self.entries[self.entry_count] = FsbDirEntry {
            inode: entry.inode,
            next_cookie: entry.next_cookie,
            name_offset: u32::try_from(name_offset).unwrap_or(u32::MAX),
            name_length: u16::try_from(entry.name.len()).unwrap_or(u16::MAX),
            kind: entry.kind as u8,
            reserved: 0,
        };
        self.name_count += entry.name.len();
        self.entry_count += 1;
        true
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_readdir(
    pointer: *mut c_void,
    inode: u64,
    cookie: u64,
    verifier: u64,
    entries: *mut FsbDirEntry,
    entry_capacity: usize,
    names: *mut u8,
    name_capacity: usize,
    entry_count_out: *mut usize,
    name_count_out: *mut usize,
    next_cookie_out: *mut u64,
    verifier_out: *mut u64,
    eof_out: *mut u8,
) -> i32 {
    status(|| {
        if entry_count_out.is_null()
            || name_count_out.is_null()
            || next_cookie_out.is_null()
            || verifier_out.is_null()
            || eof_out.is_null()
        {
            return Err(Error(EINVAL));
        }
        let entries = if entry_capacity == 0 {
            &mut []
        } else {
            if entries.is_null() {
                return Err(Error(EINVAL));
            }
            unsafe { slice::from_raw_parts_mut(entries, entry_capacity) }
        };
        let names = unsafe { bytes_mut(names, name_capacity)? };
        let mut sink = PackedSink {
            entries,
            names,
            entry_count: 0,
            name_count: 0,
        };
        let ReadDirectoryResult {
            verifier,
            next_cookie,
            eof,
        } = unsafe { volume(pointer)? }.read_directory(inode, cookie, verifier, &mut sink)?;
        if sink.entry_count == 0 && !eof && (entry_capacity == 0 || name_capacity == 0) {
            return Err(Error(ERANGE));
        }
        unsafe {
            ptr::write(entry_count_out, sink.entry_count);
            ptr::write(name_count_out, sink.name_count);
            ptr::write(next_cookie_out, next_cookie);
            ptr::write(verifier_out, verifier);
            ptr::write(eof_out, u8::from(eof));
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_read(
    pointer: *mut c_void,
    inode: u64,
    offset: u64,
    output: *mut u8,
    length: usize,
    count_out: *mut usize,
) -> i32 {
    status(|| {
        if count_out.is_null() {
            return Err(Error(EINVAL));
        }
        let output = unsafe { bytes_mut(output, length)? };
        let count = unsafe { volume(pointer)? }
            .read(inode, offset, output)
            .and_then(|count| crate::core::checked_io_count(count, output.len()))?;
        unsafe { ptr::write(count_out, count) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_write(
    pointer: *mut c_void,
    inode: u64,
    offset: u64,
    input: *const u8,
    length: usize,
    count_out: *mut usize,
) -> i32 {
    status(|| {
        if count_out.is_null() {
            return Err(Error(EINVAL));
        }
        let input = unsafe { bytes(input, length)? };
        let count = unsafe { volume(pointer)? }
            .write(inode, offset, input)
            .and_then(|count| crate::core::checked_io_count(count, input.len()))?;
        unsafe { ptr::write(count_out, count) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_create(
    pointer: *mut c_void,
    parent: u64,
    name: *const u8,
    name_length: usize,
    kind: u8,
    mode: u32,
    inode_out: *mut u64,
) -> i32 {
    status(|| {
        if inode_out.is_null() {
            return Err(Error(EINVAL));
        }
        let name = unsafe { bytes(name, name_length)? };
        let inode =
            unsafe { volume(pointer)? }.create(parent, name, FileType::try_from(kind)?, mode)?;
        unsafe { ptr::write(inode_out, inode) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_symlink(
    pointer: *mut c_void,
    parent: u64,
    name: *const u8,
    name_length: usize,
    target: *const u8,
    target_length: usize,
    mode: u32,
    inode_out: *mut u64,
) -> i32 {
    status(|| {
        if inode_out.is_null() {
            return Err(Error(EINVAL));
        }
        let name = unsafe { bytes(name, name_length)? };
        let target = unsafe { bytes(target, target_length)? };
        let inode = unsafe { volume(pointer)? }.create_symlink(parent, name, target, mode)?;
        unsafe { ptr::write(inode_out, inode) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_readlink(
    pointer: *mut c_void,
    inode: u64,
    output: *mut u8,
    capacity: usize,
    count_out: *mut usize,
) -> i32 {
    status(|| {
        if count_out.is_null() {
            return Err(Error(EINVAL));
        }
        let output = unsafe { bytes_mut(output, capacity)? };
        let count = unsafe { volume(pointer)? }
            .read_symlink(inode, output)
            .and_then(|count| crate::core::checked_io_count(count, output.len()))?;
        unsafe { ptr::write(count_out, count) };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_remove(
    pointer: *mut c_void,
    parent: u64,
    name: *const u8,
    name_length: usize,
    inode: u64,
) -> i32 {
    status(|| {
        let name = unsafe { bytes(name, name_length)? };
        unsafe { volume(pointer)? }.remove(parent, name, inode)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_rename(
    pointer: *mut c_void,
    source_parent: u64,
    source_name: *const u8,
    source_name_length: usize,
    destination_parent: u64,
    destination_name: *const u8,
    destination_name_length: usize,
    replaced_inode: u64,
) -> i32 {
    status(|| {
        let source_name = unsafe { bytes(source_name, source_name_length)? };
        let destination_name = unsafe { bytes(destination_name, destination_name_length)? };
        unsafe { volume(pointer)? }.rename(
            source_parent,
            source_name,
            destination_parent,
            destination_name,
            (replaced_inode != 0).then_some(replaced_inode),
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsb_sync(pointer: *mut c_void) -> i32 {
    status(|| unsafe { volume(pointer)? }.synchronize())
}

impl From<Metadata> for FsbAttributes {
    fn from(value: Metadata) -> Self {
        Self {
            inode: value.inode,
            parent: value.parent,
            size: value.size,
            allocated_size: value.allocated_size,
            generation: value.generation,
            created_ns: value.created_ns,
            modified_ns: value.modified_ns,
            accessed_ns: value.accessed_ns,
            mode: value.mode,
            uid: value.uid,
            gid: value.gid,
            link_count: value.link_count,
            kind: value.kind as u8,
            reserved: [0; 7],
        }
    }
}

impl From<StatFs> for FsbStatFs {
    fn from(value: StatFs) -> Self {
        Self {
            block_size: value.block_size,
            io_size: value.io_size,
            total_bytes: value.total_bytes,
            free_bytes: value.free_bytes,
            available_bytes: value.available_bytes,
            files: value.files,
            free_files: value.free_files,
        }
    }
}

impl From<FsbSetAttributes> for SetMetadata {
    fn from(value: FsbSetAttributes) -> Self {
        Self {
            size: (value.valid & 1 != 0).then_some(value.size),
            mode: (value.valid & 2 != 0).then_some(value.mode),
            uid: (value.valid & 4 != 0).then_some(value.uid),
            gid: (value.valid & 8 != 0).then_some(value.gid),
            accessed_ns: (value.valid & 16 != 0).then_some(value.accessed_ns),
            modified_ns: (value.valid & 32 != 0).then_some(value.modified_ns),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FileType, KernelCachePolicy, LocalFile, NetworkConfiguration, ROOT_INODE,
        ReadDirectoryResult, test_support::TestFilesystem,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct TestNetworkFilesystem {
        inner: TestFilesystem,
        local_path: std::path::PathBuf,
        closes: Arc<AtomicUsize>,
    }

    impl Filesystem for TestNetworkFilesystem {
        fn statfs(&self) -> crate::Result<StatFs> {
            self.inner.statfs()
        }

        fn metadata(&self, inode: u64) -> crate::Result<Metadata> {
            self.inner.metadata(inode)
        }

        fn lookup(&self, parent: u64, name: &[u8]) -> crate::Result<(u64, FileType)> {
            self.inner.lookup(parent, name)
        }

        fn read_directory(
            &self,
            inode: u64,
            cookie: u64,
            verifier: u64,
            sink: &mut dyn DirectorySink,
        ) -> crate::Result<ReadDirectoryResult> {
            self.inner.read_directory(inode, cookie, verifier, sink)
        }

        fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> crate::Result<usize> {
            self.inner.read(inode, offset, output)
        }
    }

    impl NetworkFilesystem for TestNetworkFilesystem {
        fn network_configuration(&self) -> NetworkConfiguration {
            NetworkConfiguration {
                lazy: true,
                supports_local_files: true,
                kernel_cache_policy: KernelCachePolicy::MetadataOnly,
            }
        }

        fn open_file(&self, _inode: u64, _writable: bool) -> crate::Result<NetworkFile> {
            Ok(NetworkFile::Local(LocalFile::new(
                self.local_path.clone(),
                42,
            )))
        }

        fn close_file(&self, _inode: u64, _file: NetworkFile) -> crate::Result<()> {
            self.closes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn network_configuration_and_local_file_lease_cross_the_abi() {
        let directory = tempfile::tempdir().unwrap();
        let local_path = directory.path().join("downloaded");
        std::fs::write(&local_path, b"cached").unwrap();
        let closes = Arc::new(AtomicUsize::new(0));
        let pointer = create_network_volume(TestNetworkFilesystem {
            inner: TestFilesystem::default(),
            local_path: local_path.clone(),
            closes: Arc::clone(&closes),
        });

        let mut configuration = FsbConfiguration::default();
        assert_eq!(
            unsafe { fsb_configuration(pointer, &raw mut configuration) },
            0
        );
        assert_eq!(configuration.kind, 1);
        assert_eq!(configuration.lazy, 1);
        assert_eq!(
            configuration.kernel_cache_policy,
            KernelCachePolicy::MetadataOnly as u8
        );

        let mut path = [0_u8; 1024];
        let mut opened = FsbOpenFile::default();
        assert_eq!(
            unsafe {
                fsb_open_file(
                    pointer,
                    ROOT_INODE,
                    0,
                    path.as_mut_ptr(),
                    path.len(),
                    &raw mut opened,
                )
            },
            0
        );
        assert_eq!(opened.source, 1);
        assert_eq!(opened.generation, 42);
        assert_eq!(
            &path[..opened.path_length],
            local_path.as_os_str().as_bytes()
        );
        assert_eq!(unsafe { fsb_close_file(pointer, opened.lease) }, 0);
        assert_eq!(closes.load(Ordering::Relaxed), 1);
        unsafe { fsb_volume_destroy(pointer) };
    }
}
