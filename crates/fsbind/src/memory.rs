use crate::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, ROOT_INODE,
    ReadDirectoryResult, Result, SetMetadata, StatFs, now_ns,
};
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

const BLOCK_SIZE: u64 = 4096;
const BLOCK_SIZE_U32: u32 = 4096;
const DEFAULT_CAPACITY: u64 = 1 << 40;

struct Node {
    state: RwLock<NodeState>,
}

struct NodeState {
    metadata: Metadata,
    data: NodeData,
}

enum NodeData {
    File(Vec<u8>),
    Directory(Directory),
    Symlink(Vec<u8>),
}

struct Directory {
    version: u64,
    slots: Vec<Option<OwnedEntry>>,
    by_name: HashMap<Vec<u8>, usize>,
}

#[derive(Clone)]
struct OwnedEntry {
    name: Vec<u8>,
    inode: u64,
    kind: FileType,
}

/// A concurrent in-memory filesystem suitable as a starter backend and test oracle.
///
/// Inode lookup is sharded by [`DashMap`]. File bytes and each directory are guarded
/// independently, while namespace mutations take a short global mutex to make
/// cross-directory rename atomic. Directory slots are stable between mutations, so a
/// cookie is a direct index rather than an O(n) rescan.
pub struct MemoryFileSystem {
    nodes: DashMap<u64, Arc<Node>>,
    next_inode: AtomicU64,
    used_bytes: AtomicU64,
    namespace: Mutex<()>,
    capacity: u64,
}

impl Default for MemoryFileSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryFileSystem {
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    #[must_use]
    pub fn with_capacity(capacity: u64) -> Self {
        let now = now_ns();
        let root = Arc::new(Node {
            state: RwLock::new(NodeState {
                metadata: Metadata {
                    inode: ROOT_INODE,
                    parent: ROOT_INODE,
                    size: 0,
                    allocated_size: 0,
                    generation: 1,
                    created_ns: now,
                    modified_ns: now,
                    accessed_ns: now,
                    mode: 0o755,
                    uid: current_uid(),
                    gid: current_gid(),
                    link_count: 1,
                    kind: FileType::Directory,
                },
                data: NodeData::Directory(Directory {
                    version: 1,
                    slots: Vec::new(),
                    by_name: HashMap::new(),
                }),
            }),
        });
        let nodes = DashMap::new();
        nodes.insert(ROOT_INODE, root);
        Self {
            nodes,
            next_inode: AtomicU64::new(ROOT_INODE + 1),
            used_bytes: AtomicU64::new(0),
            namespace: Mutex::new(()),
            capacity,
        }
    }

    fn node(&self, inode: u64) -> Result<Arc<Node>> {
        self.nodes
            .get(&inode)
            .map(|entry| Arc::clone(entry.value()))
            .ok_or(Error(errno::ENOENT))
    }

    fn validate_name(name: &[u8]) -> Result<()> {
        if name.is_empty()
            || name.len() > 255
            || name == b"."
            || name == b".."
            || name.contains(&0)
            || name.contains(&b'/')
        {
            return Err(Error(errno::EINVAL));
        }
        Ok(())
    }

    fn allocate_node(&self, parent: u64, kind: FileType, mode: u32, bytes: Vec<u8>) -> Arc<Node> {
        let inode = self.next_inode.fetch_add(1, Ordering::Relaxed);
        let now = now_ns();
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let allocated_size = allocated_size(size);
        let data = match kind {
            FileType::File => NodeData::File(bytes),
            FileType::Directory => NodeData::Directory(Directory {
                version: 1,
                slots: Vec::new(),
                by_name: HashMap::new(),
            }),
            FileType::Symlink => NodeData::Symlink(bytes),
        };
        Arc::new(Node {
            state: RwLock::new(NodeState {
                metadata: Metadata {
                    inode,
                    parent,
                    size,
                    allocated_size,
                    generation: 1,
                    created_ns: now,
                    modified_ns: now,
                    accessed_ns: now,
                    mode: mode & 0o7777,
                    uid: current_uid(),
                    gid: current_gid(),
                    link_count: 1,
                    kind,
                },
                data,
            }),
        })
    }

    fn create_with_bytes(
        &self,
        parent: u64,
        name: &[u8],
        kind: FileType,
        mode: u32,
        bytes: Vec<u8>,
    ) -> Result<u64> {
        Self::validate_name(name)?;
        let _namespace = self.namespace.lock();
        let parent_node = self.node(parent)?;
        let mut parent_state = parent_node.state.write();
        let NodeData::Directory(directory) = &mut parent_state.data else {
            return Err(Error(errno::ENOTDIR));
        };
        if directory.by_name.contains_key(name) {
            return Err(Error(errno::EEXIST));
        }

        let node = self.allocate_node(parent, kind, mode, bytes);
        let inode = node.state.read().metadata.inode;
        let slot = directory.slots.len();
        directory.slots.push(Some(OwnedEntry {
            name: name.to_vec(),
            inode,
            kind,
        }));
        directory.by_name.insert(name.to_vec(), slot);
        directory.version = directory.version.wrapping_add(1).max(1);
        parent_state.metadata.modified_ns = now_ns();
        parent_state.metadata.generation = parent_state.metadata.generation.wrapping_add(1);
        self.used_bytes
            .fetch_add(node.state.read().metadata.allocated_size, Ordering::Relaxed);
        self.nodes.insert(inode, node);
        Ok(inode)
    }
}

impl Filesystem for MemoryFileSystem {
    fn statfs(&self) -> Result<StatFs> {
        let used = self.used_bytes.load(Ordering::Relaxed).min(self.capacity);
        let free = self.capacity - used;
        Ok(StatFs {
            block_size: BLOCK_SIZE_U32,
            io_size: 128 * 1024,
            total_bytes: self.capacity,
            free_bytes: free,
            available_bytes: free,
            files: self.nodes.len() as u64,
            free_files: u64::MAX - self.nodes.len() as u64,
        })
    }

    fn metadata(&self, inode: u64) -> Result<Metadata> {
        Ok(self.node(inode)?.state.read().metadata)
    }

    fn set_metadata(&self, inode: u64, update: SetMetadata) -> Result<Metadata> {
        let node = self.node(inode)?;
        let mut state = node.state.write();
        if let Some(size) = update.size {
            if size > self.capacity {
                return Err(Error(errno::ENOSPC));
            }
            let old_allocated = state.metadata.allocated_size;
            let NodeData::File(data) = &mut state.data else {
                return Err(Error(errno::EISDIR));
            };
            let new_len = usize::try_from(size).map_err(|_| Error(errno::EFBIG))?;
            data.resize(new_len, 0);
            state.metadata.size = size;
            state.metadata.allocated_size = allocated_size(size);
            adjust_atomic(
                &self.used_bytes,
                old_allocated,
                state.metadata.allocated_size,
            );
        }
        if let Some(mode) = update.mode {
            state.metadata.mode = mode & 0o7777;
        }
        if let Some(uid) = update.uid {
            state.metadata.uid = uid;
        }
        if let Some(gid) = update.gid {
            state.metadata.gid = gid;
        }
        if let Some(accessed_ns) = update.accessed_ns {
            state.metadata.accessed_ns = accessed_ns;
        }
        if let Some(modified_ns) = update.modified_ns {
            state.metadata.modified_ns = modified_ns;
        }
        state.metadata.generation = state.metadata.generation.wrapping_add(1);
        Ok(state.metadata)
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)> {
        let node = self.node(parent)?;
        let state = node.state.read();
        let NodeData::Directory(directory) = &state.data else {
            return Err(Error(errno::ENOTDIR));
        };
        let slot = *directory.by_name.get(name).ok_or(Error(errno::ENOENT))?;
        let entry = directory.slots[slot].as_ref().ok_or(Error(errno::ENOENT))?;
        Ok((entry.inode, entry.kind))
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult> {
        let node = self.node(inode)?;
        let state = node.state.read();
        let NodeData::Directory(directory) = &state.data else {
            return Err(Error(errno::ENOTDIR));
        };
        if verifier != 0 && verifier != directory.version {
            return Err(Error(errno::ESTALE));
        }
        let start = usize::try_from(cookie).map_err(|_| Error(errno::EINVAL))?;
        if start > directory.slots.len() {
            return Err(Error(errno::EINVAL));
        }
        let mut next_cookie = cookie;
        for (index, entry) in directory.slots.iter().enumerate().skip(start) {
            next_cookie = u64::try_from(index + 1).unwrap_or(u64::MAX);
            if let Some(entry) = entry
                && !sink.push(DirectoryEntry {
                    name: &entry.name,
                    inode: entry.inode,
                    kind: entry.kind,
                    next_cookie,
                })
            {
                return Ok(ReadDirectoryResult {
                    verifier: directory.version,
                    next_cookie: u64::try_from(index).unwrap_or(u64::MAX),
                    eof: false,
                });
            }
        }
        Ok(ReadDirectoryResult {
            verifier: directory.version,
            next_cookie,
            eof: true,
        })
    }

    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize> {
        let node = self.node(inode)?;
        let state = node.state.read();
        let NodeData::File(data) = &state.data else {
            return Err(Error(errno::EISDIR));
        };
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let count = output.len().min(data.len() - start);
        output[..count].copy_from_slice(&data[start..start + count]);
        Ok(count)
    }

    fn write(&self, inode: u64, offset: u64, input: &[u8]) -> Result<usize> {
        let node = self.node(inode)?;
        let mut state = node.state.write();
        let old_allocated = state.metadata.allocated_size;
        let NodeData::File(data) = &mut state.data else {
            return Err(Error(errno::EISDIR));
        };
        let start = usize::try_from(offset).map_err(|_| Error(errno::EFBIG))?;
        let end = start.checked_add(input.len()).ok_or(Error(errno::EFBIG))?;
        if u64::try_from(end).unwrap_or(u64::MAX) > self.capacity {
            return Err(Error(errno::ENOSPC));
        }
        if end > data.len() {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(input);
        state.metadata.size = data.len() as u64;
        state.metadata.allocated_size = allocated_size(state.metadata.size);
        state.metadata.modified_ns = now_ns();
        state.metadata.generation = state.metadata.generation.wrapping_add(1);
        adjust_atomic(
            &self.used_bytes,
            old_allocated,
            state.metadata.allocated_size,
        );
        Ok(input.len())
    }

    fn create(&self, parent: u64, name: &[u8], kind: FileType, mode: u32) -> Result<u64> {
        if kind == FileType::Symlink {
            return Err(Error(errno::EINVAL));
        }
        self.create_with_bytes(parent, name, kind, mode, Vec::new())
    }

    fn create_symlink(&self, parent: u64, name: &[u8], target: &[u8], mode: u32) -> Result<u64> {
        self.create_with_bytes(parent, name, FileType::Symlink, mode, target.to_vec())
    }

    fn read_symlink(&self, inode: u64, output: &mut [u8]) -> Result<usize> {
        let node = self.node(inode)?;
        let state = node.state.read();
        let NodeData::Symlink(target) = &state.data else {
            return Err(Error(errno::EINVAL));
        };
        if output.len() < target.len() {
            return Err(Error(errno::ERANGE));
        }
        output[..target.len()].copy_from_slice(target);
        Ok(target.len())
    }

    fn remove(&self, parent: u64, name: &[u8], inode: u64) -> Result<()> {
        let _namespace = self.namespace.lock();
        let parent_node = self.node(parent)?;
        let child_node = self.node(inode)?;
        if let NodeData::Directory(child_directory) = &child_node.state.read().data
            && !child_directory.by_name.is_empty()
        {
            return Err(Error(errno::ENOTEMPTY));
        }
        let mut parent_state = parent_node.state.write();
        let NodeData::Directory(directory) = &mut parent_state.data else {
            return Err(Error(errno::ENOTDIR));
        };
        let slot = *directory.by_name.get(name).ok_or(Error(errno::ENOENT))?;
        if directory.slots[slot].as_ref().map(|entry| entry.inode) != Some(inode) {
            return Err(Error(errno::ENOENT));
        }
        directory.by_name.remove(name);
        directory.slots[slot] = None;
        directory.version = directory.version.wrapping_add(1).max(1);
        parent_state.metadata.modified_ns = now_ns();
        let removed = self.nodes.remove(&inode).ok_or(Error(errno::ENOENT))?.1;
        self.used_bytes.fetch_sub(
            removed.state.read().metadata.allocated_size,
            Ordering::Relaxed,
        );
        Ok(())
    }

    fn rename(
        &self,
        source_parent: u64,
        source_name: &[u8],
        destination_parent: u64,
        destination_name: &[u8],
        replaced_inode: Option<u64>,
    ) -> Result<()> {
        Self::validate_name(destination_name)?;
        let _namespace = self.namespace.lock();
        let source_node = self.node(source_parent)?;
        let destination_node = self.node(destination_parent)?;
        let source_entry = {
            let source_state = source_node.state.read();
            let NodeData::Directory(source_directory) = &source_state.data else {
                return Err(Error(errno::ENOTDIR));
            };
            let source_slot = *source_directory
                .by_name
                .get(source_name)
                .ok_or(Error(errno::ENOENT))?;
            source_directory.slots[source_slot]
                .clone()
                .ok_or(Error(errno::ENOENT))?
        };
        if source_entry.kind == FileType::Directory
            && self.is_descendant_of(destination_parent, source_entry.inode)?
        {
            return Err(Error(errno::EINVAL));
        }
        if let Some(replaced) = replaced_inode {
            let replacement = self.node(replaced)?;
            let replacement_state = replacement.state.read();
            if source_entry.kind == FileType::Directory
                && replacement_state.metadata.kind != FileType::Directory
            {
                return Err(Error(errno::ENOTDIR));
            }
            if source_entry.kind != FileType::Directory
                && replacement_state.metadata.kind == FileType::Directory
            {
                return Err(Error(errno::EISDIR));
            }
            if let NodeData::Directory(directory) = &replacement_state.data
                && !directory.by_name.is_empty()
            {
                return Err(Error(errno::ENOTEMPTY));
            }
        }

        if source_parent == destination_parent {
            let mut state = source_node.state.write();
            let NodeData::Directory(directory) = &mut state.data else {
                return Err(Error(errno::ENOTDIR));
            };
            rename_in_same_directory(directory, source_name, destination_name, replaced_inode)?;
            directory.version = directory.version.wrapping_add(1).max(1);
            state.metadata.modified_ns = now_ns();
        } else {
            let (first, second, source_is_first) = if source_parent < destination_parent {
                (&source_node, &destination_node, true)
            } else {
                (&destination_node, &source_node, false)
            };
            let mut first_state = first.state.write();
            let mut second_state = second.state.write();
            let (source_state, destination_state) = if source_is_first {
                (&mut first_state, &mut second_state)
            } else {
                (&mut second_state, &mut first_state)
            };
            let NodeData::Directory(source_directory) = &mut source_state.data else {
                return Err(Error(errno::ENOTDIR));
            };
            let NodeData::Directory(destination_directory) = &mut destination_state.data else {
                return Err(Error(errno::ENOTDIR));
            };
            let source_slot = *source_directory
                .by_name
                .get(source_name)
                .ok_or(Error(errno::ENOENT))?;
            let mut entry = source_directory.slots[source_slot]
                .take()
                .ok_or(Error(errno::ENOENT))?;
            source_directory.by_name.remove(source_name);
            remove_destination(destination_directory, destination_name, replaced_inode)?;
            entry.name = destination_name.to_vec();
            let destination_slot = destination_directory.slots.len();
            destination_directory.slots.push(Some(entry.clone()));
            destination_directory
                .by_name
                .insert(entry.name.clone(), destination_slot);
            source_directory.version = source_directory.version.wrapping_add(1).max(1);
            destination_directory.version = destination_directory.version.wrapping_add(1).max(1);
            source_state.metadata.modified_ns = now_ns();
            destination_state.metadata.modified_ns = now_ns();
            self.node(entry.inode)?.state.write().metadata.parent = destination_parent;
        }
        if let Some(replaced) = replaced_inode {
            if let Some((_, node)) = self.nodes.remove(&replaced) {
                self.used_bytes
                    .fetch_sub(node.state.read().metadata.allocated_size, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

impl MemoryFileSystem {
    fn is_descendant_of(&self, mut candidate: u64, ancestor: u64) -> Result<bool> {
        for _ in 0..self.nodes.len().saturating_add(1) {
            if candidate == ancestor {
                return Ok(true);
            }
            if candidate == ROOT_INODE {
                return Ok(false);
            }
            candidate = self.node(candidate)?.state.read().metadata.parent;
        }
        Err(Error(errno::EINVAL))
    }
}

fn rename_in_same_directory(
    directory: &mut Directory,
    source_name: &[u8],
    destination_name: &[u8],
    replaced_inode: Option<u64>,
) -> Result<()> {
    let source_slot = *directory
        .by_name
        .get(source_name)
        .ok_or(Error(errno::ENOENT))?;
    if source_name == destination_name {
        return Ok(());
    }
    remove_destination(directory, destination_name, replaced_inode)?;
    let entry = directory.slots[source_slot]
        .as_mut()
        .ok_or(Error(errno::ENOENT))?;
    directory.by_name.remove(source_name);
    entry.name = destination_name.to_vec();
    directory
        .by_name
        .insert(destination_name.to_vec(), source_slot);
    Ok(())
}

fn remove_destination(
    directory: &mut Directory,
    name: &[u8],
    expected_inode: Option<u64>,
) -> Result<()> {
    match (directory.by_name.get(name).copied(), expected_inode) {
        (None, None) => Ok(()),
        (Some(slot), Some(expected))
            if directory.slots[slot].as_ref().map(|entry| entry.inode) == Some(expected) =>
        {
            directory.by_name.remove(name);
            directory.slots[slot] = None;
            Ok(())
        }
        (None | Some(_), Some(_)) | (Some(_), None) => Err(Error(errno::EEXIST)),
    }
}

const fn allocated_size(size: u64) -> u64 {
    size.div_ceil(BLOCK_SIZE) * BLOCK_SIZE
}

fn adjust_atomic(value: &AtomicU64, old: u64, new: u64) {
    if new >= old {
        value.fetch_add(new - old, Ordering::Relaxed);
    } else {
        value.fetch_sub(old - new, Ordering::Relaxed);
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "getuid"]
    fn c_getuid() -> u32;
    #[link_name = "getgid"]
    fn c_getgid() -> u32;
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { c_getuid() }
}

#[cfg(unix)]
fn current_gid() -> u32 {
    unsafe { c_getgid() }
}

// Windows has no numeric POSIX UID/GID. Zero is the neutral value consumed by
// the portable model; ProjFS applies Windows ACLs to the virtualization root.
#[cfg(windows)]
const fn current_uid() -> u32 {
    0
}

#[cfg(windows)]
const fn current_gid() -> u32 {
    0
}

mod errno {
    pub const ENOENT: i32 = 2;
    pub const EEXIST: i32 = 17;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EINVAL: i32 = 22;
    pub const EFBIG: i32 = 27;
    pub const ENOSPC: i32 = 28;
    pub const ERANGE: i32 = 34;
    pub const ENOTEMPTY: i32 = 66;
    pub const ESTALE: i32 = 70;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DirectorySink;
    use std::sync::Arc;

    #[derive(Default)]
    struct VecSink {
        capacity: usize,
        entries: Vec<(Vec<u8>, u64, FileType, u64)>,
    }

    impl DirectorySink for VecSink {
        fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
            if self.entries.len() == self.capacity {
                return false;
            }
            self.entries.push((
                entry.name.to_vec(),
                entry.inode,
                entry.kind,
                entry.next_cookie,
            ));
            true
        }
    }

    #[test]
    fn sparse_write_reads_zeroes_and_updates_metadata() {
        let filesystem = MemoryFileSystem::new();
        let inode = filesystem
            .create(ROOT_INODE, b"data", FileType::File, 0o640)
            .unwrap();
        assert_eq!(filesystem.write(inode, 4, b"rust").unwrap(), 4);

        let mut output = [0xff; 8];
        assert_eq!(filesystem.read(inode, 0, &mut output).unwrap(), 8);
        assert_eq!(&output, b"\0\0\0\0rust");
        let metadata = filesystem.metadata(inode).unwrap();
        assert_eq!(metadata.size, 8);
        assert_eq!(metadata.allocated_size, BLOCK_SIZE);
        assert_eq!(metadata.mode, 0o640);
    }

    #[test]
    fn directory_cookies_resume_without_rescanning() {
        let filesystem = MemoryFileSystem::new();
        for name in [b"alpha".as_slice(), b"beta", b"gamma"] {
            filesystem
                .create(ROOT_INODE, name, FileType::File, 0o644)
                .unwrap();
        }

        let mut first = VecSink {
            capacity: 2,
            ..VecSink::default()
        };
        let first_result = filesystem
            .read_directory(ROOT_INODE, 0, 0, &mut first)
            .unwrap();
        assert_eq!(first.entries.len(), 2);
        assert!(!first_result.eof);
        assert_eq!(first_result.next_cookie, 2);

        let mut second = VecSink {
            capacity: 2,
            ..VecSink::default()
        };
        let second_result = filesystem
            .read_directory(
                ROOT_INODE,
                first_result.next_cookie,
                first_result.verifier,
                &mut second,
            )
            .unwrap();
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.entries[0].0, b"gamma");
        assert!(second_result.eof);
    }

    #[test]
    fn mutation_invalidates_an_active_directory_enumeration() {
        let filesystem = MemoryFileSystem::new();
        filesystem
            .create(ROOT_INODE, b"before", FileType::File, 0o644)
            .unwrap();
        let mut sink = VecSink {
            capacity: 1,
            ..VecSink::default()
        };
        let result = filesystem
            .read_directory(ROOT_INODE, 0, 0, &mut sink)
            .unwrap();
        filesystem
            .create(ROOT_INODE, b"after", FileType::File, 0o644)
            .unwrap();
        let error = filesystem
            .read_directory(ROOT_INODE, result.next_cookie, result.verifier, &mut sink)
            .unwrap_err();
        assert_eq!(error, Error(errno::ESTALE));
    }

    #[test]
    fn cross_directory_rename_updates_parent_and_lookup_indexes() {
        let filesystem = MemoryFileSystem::new();
        let source = filesystem
            .create(ROOT_INODE, b"source", FileType::Directory, 0o755)
            .unwrap();
        let destination = filesystem
            .create(ROOT_INODE, b"destination", FileType::Directory, 0o755)
            .unwrap();
        let file = filesystem
            .create(source, b"old", FileType::File, 0o644)
            .unwrap();

        filesystem
            .rename(source, b"old", destination, b"new", None)
            .unwrap();
        assert_eq!(
            filesystem.lookup(destination, b"new").unwrap(),
            (file, FileType::File)
        );
        assert_eq!(
            filesystem.lookup(source, b"old").unwrap_err(),
            Error(errno::ENOENT)
        );
        assert_eq!(filesystem.metadata(file).unwrap().parent, destination);
    }

    #[test]
    fn nonempty_directory_cannot_be_removed() {
        let filesystem = MemoryFileSystem::new();
        let directory = filesystem
            .create(ROOT_INODE, b"directory", FileType::Directory, 0o755)
            .unwrap();
        filesystem
            .create(directory, b"child", FileType::File, 0o644)
            .unwrap();
        assert_eq!(
            filesystem
                .remove(ROOT_INODE, b"directory", directory)
                .unwrap_err(),
            Error(errno::ENOTEMPTY)
        );
    }

    #[test]
    fn directory_cannot_be_moved_inside_itself() {
        let filesystem = MemoryFileSystem::new();
        let parent = filesystem
            .create(ROOT_INODE, b"parent", FileType::Directory, 0o755)
            .unwrap();
        let child = filesystem
            .create(parent, b"child", FileType::Directory, 0o755)
            .unwrap();
        assert_eq!(
            filesystem
                .rename(ROOT_INODE, b"parent", child, b"cycle", None)
                .unwrap_err(),
            Error(errno::EINVAL)
        );
    }

    #[test]
    fn rename_cannot_replace_a_nonempty_directory() {
        let filesystem = MemoryFileSystem::new();
        let source = filesystem
            .create(ROOT_INODE, b"source", FileType::Directory, 0o755)
            .unwrap();
        let destination = filesystem
            .create(ROOT_INODE, b"destination", FileType::Directory, 0o755)
            .unwrap();
        filesystem
            .create(destination, b"child", FileType::File, 0o644)
            .unwrap();
        assert_eq!(
            filesystem
                .rename(
                    ROOT_INODE,
                    b"source",
                    ROOT_INODE,
                    b"destination",
                    Some(destination),
                )
                .unwrap_err(),
            Error(errno::ENOTEMPTY)
        );
        assert_eq!(filesystem.lookup(ROOT_INODE, b"source").unwrap().0, source);
    }

    #[test]
    fn independent_files_support_concurrent_io() {
        let filesystem = Arc::new(MemoryFileSystem::new());
        let inodes: Vec<_> = (0..8)
            .map(|index| {
                filesystem
                    .create(
                        ROOT_INODE,
                        format!("file-{index}").as_bytes(),
                        FileType::File,
                        0o644,
                    )
                    .unwrap()
            })
            .collect();
        let threads: Vec<_> = inodes
            .into_iter()
            .enumerate()
            .map(|(index, inode)| {
                let filesystem = Arc::clone(&filesystem);
                std::thread::spawn(move || {
                    let bytes = vec![u8::try_from(index).unwrap(); 64 * 1024];
                    filesystem.write(inode, 0, &bytes).unwrap();
                    let mut output = vec![0; bytes.len()];
                    filesystem.read(inode, 0, &mut output).unwrap();
                    assert_eq!(output, bytes);
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }
}
