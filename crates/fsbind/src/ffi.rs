use crate::{
    ABI_VERSION, DirectoryEntry, DirectorySink, Error, FileSystem, FileType, Metadata,
    ReadDirectoryResult, SetMetadata, StatFs,
};
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr, slice,
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

struct Volume {
    filesystem: Box<dyn FileSystem>,
}

pub fn create_volume(filesystem: impl FileSystem) -> *mut c_void {
    Box::into_raw(Box::new(Volume {
        filesystem: Box::new(filesystem),
    }))
    .cast()
}

#[unsafe(no_mangle)]
pub extern "C" fn fsb_abi_version() -> u32 {
    ABI_VERSION
}

unsafe fn volume<'a>(pointer: *mut c_void) -> Result<&'a dyn FileSystem, Error> {
    if pointer.is_null() {
        return Err(Error(EINVAL));
    }
    let volume = unsafe { &*pointer.cast::<Volume>() };
    Ok(volume.filesystem.as_ref())
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
        let count = unsafe { volume(pointer)? }.read(inode, offset, output)?;
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
        let count = unsafe { volume(pointer)? }.write(inode, offset, input)?;
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
        let count = unsafe { volume(pointer)? }.read_symlink(inode, output)?;
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
