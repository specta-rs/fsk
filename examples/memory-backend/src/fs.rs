use dashmap::DashMap;
use fsk::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, Metadata, ROOT_INODE,
    ReadDirectoryResult, Result, SetMetadata, StatFs, now_ns,
};
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
/// cookie is a direct index rather than an O(n) rescan. Changes are printed to stdout
/// so the mount example shows what the kernel is doing.
pub struct MemoryFileSystem {
    nodes: DashMap<u64, Arc<Node>>,
    next_inode: AtomicU64,
    used_bytes: AtomicU64,
    namespace: Mutex<()>,
    capacity: u64,
}

/// Build the seeded filesystem used by the mount example.
#[must_use]
#[allow(dead_code)]
pub fn filesystem() -> MemoryFileSystem {
    let filesystem = MemoryFileSystem::new();
    let readme = filesystem
        .create(ROOT_INODE, b"hello.txt", FileType::File, 0o644)
        .expect("create example file");
    filesystem
        .write(
            readme,
            0,
            b"Hello from Rust through a Swift FSKit extension.\n",
        )
        .expect("write example file");
    filesystem
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

    /// Resolve an inode to a readable path for the change log.
    ///
    /// Uses `try_read` so logging can never block or deadlock behind a mutation
    /// that is already holding a node lock; the inode is shown instead.
    fn path_for(&self, inode: u64) -> String {
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut current = inode;
        for _ in 0..self.nodes.len().saturating_add(1) {
            if current == ROOT_INODE {
                break;
            }
            let Some((parent, name)) = self.parent_and_name(current) else {
                return format!("<inode {inode}>");
            };
            names.push(name);
            current = parent;
        }
        names.reverse();
        let mut path = String::from("/");
        for (index, name) in names.iter().enumerate() {
            if index > 0 {
                path.push('/');
            }
            path.push_str(&String::from_utf8_lossy(name));
        }
        path
    }

    fn parent_and_name(&self, inode: u64) -> Option<(u64, Vec<u8>)> {
        let node = self
            .nodes
            .get(&inode)
            .map(|entry| Arc::clone(entry.value()))?;
        let parent = node.state.try_read()?.metadata.parent;
        if parent == inode {
            return None;
        }
        let parent_node = self
            .nodes
            .get(&parent)
            .map(|entry| Arc::clone(entry.value()))?;
        let parent_state = parent_node.state.try_read()?;
        let NodeData::Directory(directory) = &parent_state.data else {
            return None;
        };
        let entry = directory
            .slots
            .iter()
            .flatten()
            .find(|entry| entry.inode == inode)?;
        Some((parent, entry.name.clone()))
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
        let parent_path = self.path_for(parent);
        let link_target =
            (kind == FileType::Symlink).then(|| String::from_utf8_lossy(&bytes).into_owned());
        let _namespace = self.namespace.lock();
        let parent_node = self.node(parent)?;
        let mut parent_state = parent_node.state.write();
        let NodeData::Directory(directory) = &mut parent_state.data else {
            return Err(Error(errno::ENOTDIR));
        };
        if directory.by_name.contains_key(name) {
            return Err(Error(errno::EEXIST));
        }

        let size = u64::try_from(bytes.len()).map_err(|_| Error(errno::EFBIG))?;
        self.reserve_growth(0, allocated_size(size))?;
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
        self.nodes.insert(inode, node);
        let path = join_path(&parent_path, name);
        match kind {
            FileType::File => println!("[fs] create {path}"),
            FileType::Directory => println!("[fs] mkdir {path}"),
            FileType::Symlink => {
                println!(
                    "[fs] symlink {path} -> {}",
                    link_target.as_deref().unwrap_or_default()
                );
            }
        }
        Ok(inode)
    }

    fn reserve_growth(&self, old: u64, new: u64) -> Result<()> {
        let additional = new.saturating_sub(old);
        if additional == 0 {
            return Ok(());
        }
        let mut current = self.used_bytes.load(Ordering::Relaxed);
        loop {
            let next = current
                .checked_add(additional)
                .filter(|total| *total <= self.capacity)
                .ok_or(Error(errno::ENOSPC))?;
            match self.used_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    fn update_usage(&self, old: u64, new: u64) -> Result<()> {
        if new > old {
            self.reserve_growth(old, new)
        } else {
            self.used_bytes.fetch_sub(old - new, Ordering::Relaxed);
            Ok(())
        }
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
        let path = self.path_for(inode);
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
            let new_allocated = allocated_size(size);
            self.update_usage(old_allocated, new_allocated)?;
            data.resize(new_len, 0);
            state.metadata.size = size;
            state.metadata.allocated_size = new_allocated;
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
        let mut changes = Vec::new();
        if let Some(size) = update.size {
            changes.push(format!("size={size}"));
        }
        if let Some(mode) = update.mode {
            changes.push(format!("mode={:04o}", mode & 0o7777));
        }
        if let Some(uid) = update.uid {
            changes.push(format!("uid={uid}"));
        }
        if let Some(gid) = update.gid {
            changes.push(format!("gid={gid}"));
        }
        if !changes.is_empty() {
            println!("[fs] set {path}: {}", changes.join(", "));
        }
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
        let path = self.path_for(inode);
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
            self.update_usage(old_allocated, allocated_size(end as u64))?;
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(input);
        state.metadata.size = data.len() as u64;
        state.metadata.allocated_size = allocated_size(state.metadata.size);
        state.metadata.modified_ns = now_ns();
        state.metadata.generation = state.metadata.generation.wrapping_add(1);
        if !input.is_empty() {
            println!(
                "[fs] write {path} at offset {offset} ({} bytes)",
                input.len()
            );
        }
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
        let parent_path = self.path_for(parent);
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
        let (removed_kind, removed_size) = {
            let state = removed.state.read();
            (state.metadata.kind, state.metadata.allocated_size)
        };
        self.used_bytes.fetch_sub(removed_size, Ordering::Relaxed);
        let action = if removed_kind == FileType::Directory {
            "rmdir"
        } else {
            "remove"
        };
        println!("[fs] {action} {}", join_path(&parent_path, name));
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
        if source_parent == destination_parent && source_name == destination_name {
            return Ok(());
        }
        if source_entry.kind == FileType::Directory
            && self.is_descendant_of(destination_parent, source_entry.inode)?
        {
            return Err(Error(errno::EINVAL));
        }
        self.validate_replacement(source_entry.kind, replaced_inode)?;

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
        self.log_rename(
            source_parent,
            source_name,
            destination_parent,
            destination_name,
        );
        Ok(())
    }
}

impl MemoryFileSystem {
    fn validate_replacement(
        &self,
        source_kind: FileType,
        replaced_inode: Option<u64>,
    ) -> Result<()> {
        let Some(replaced) = replaced_inode else {
            return Ok(());
        };
        let replacement = self.node(replaced)?;
        let replacement_state = replacement.state.read();
        if source_kind == FileType::Directory
            && replacement_state.metadata.kind != FileType::Directory
        {
            return Err(Error(errno::ENOTDIR));
        }
        if source_kind != FileType::Directory
            && replacement_state.metadata.kind == FileType::Directory
        {
            return Err(Error(errno::EISDIR));
        }
        if let NodeData::Directory(directory) = &replacement_state.data
            && !directory.by_name.is_empty()
        {
            return Err(Error(errno::ENOTEMPTY));
        }
        Ok(())
    }

    fn log_rename(
        &self,
        source_parent: u64,
        source_name: &[u8],
        destination_parent: u64,
        destination_name: &[u8],
    ) {
        println!(
            "[fs] rename {} -> {}",
            join_path(&self.path_for(source_parent), source_name),
            join_path(&self.path_for(destination_parent), destination_name)
        );
    }

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

fn join_path(parent: &str, name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

const fn allocated_size(size: u64) -> u64 {
    size.div_ceil(BLOCK_SIZE).saturating_mul(BLOCK_SIZE)
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

// Tests live under `tests/` to keep the example source focused.
#[cfg(test)]
#[path = "../tests/fs/mod.rs"]
mod tests;
