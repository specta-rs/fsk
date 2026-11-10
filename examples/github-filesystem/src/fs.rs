use base64::{Engine as _, engine::general_purpose::STANDARD};
use fsk::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, KernelCachePolicy, LocalFile,
    Metadata, NetworkConfiguration, NetworkFile, NetworkFilesystem, ROOT_INODE,
    ReadDirectoryResult, Result, StatFs, now_ns,
};
use parking_lot::{Mutex, RwLock};
use reqwest::{Url, blocking::Client};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

const EACCES: i32 = 13;
const ENOENT: i32 = 2;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;

pub struct Options {
    pub api_url: String,
    pub token: String,
    pub cache_dir: PathBuf,
    pub max_cache_bytes: u64,
    pub max_idle: Duration,
    pub download_on_list: bool,
}

pub struct GitHubFilesystem {
    api: Api,
    nodes: RwLock<HashMap<u64, Node>>,
    next_inode: AtomicU64,
    load_lock: Mutex<()>,
    cache: Cache,
    download_on_list: bool,
}

#[derive(Clone)]
struct Node {
    inode: u64,
    parent: u64,
    name: Vec<u8>,
    kind: NodeKind,
}

#[derive(Clone)]
enum NodeKind {
    Directory {
        location: DirectoryLocation,
        children: Option<Vec<u64>>,
    },
    File(FileNode),
}

#[derive(Clone)]
enum DirectoryLocation {
    Root,
    Owner,
    Repository {
        owner: String,
        repo: String,
        path: String,
    },
}

#[derive(Clone)]
struct FileNode {
    owner: String,
    repo: String,
    path: String,
    sha: String,
    size: u64,
    download_url: Option<String>,
    inline: Option<Vec<u8>>,
}

impl GitHubFilesystem {
    pub fn new(options: Options) -> Result<Self> {
        let api = Api::new(&options.api_url, &options.token)?;
        let repositories = api.repositories()?;
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT_INODE,
            Node {
                inode: ROOT_INODE,
                parent: ROOT_INODE,
                name: b".".to_vec(),
                kind: NodeKind::Directory {
                    location: DirectoryLocation::Root,
                    children: Some(Vec::new()),
                },
            },
        );
        let filesystem = Self {
            api,
            nodes: RwLock::new(nodes),
            next_inode: AtomicU64::new(ROOT_INODE + 1),
            load_lock: Mutex::new(()),
            cache: Cache::new(options.cache_dir, options.max_cache_bytes, options.max_idle)?,
            download_on_list: options.download_on_list,
        };
        filesystem.install_repositories(repositories);
        Ok(filesystem)
    }

    fn install_repositories(&self, repositories: Vec<Repository>) {
        let mut nodes = self.nodes.write();
        let mut owners = HashMap::<String, u64>::new();
        for repository in repositories {
            let owner_name = repository.owner.login;
            let owner = *owners.entry(owner_name.clone()).or_insert_with(|| {
                let inode = self.allocate_inode();
                nodes.insert(
                    inode,
                    Node {
                        inode,
                        parent: ROOT_INODE,
                        name: owner_name.as_bytes().to_vec(),
                        kind: NodeKind::Directory {
                            location: DirectoryLocation::Owner,
                            children: Some(Vec::new()),
                        },
                    },
                );
                if let NodeKind::Directory {
                    children: Some(children),
                    ..
                } = &mut nodes.get_mut(&ROOT_INODE).expect("root").kind
                {
                    children.push(inode);
                }
                inode
            });
            let inode = self.allocate_inode();
            nodes.insert(
                inode,
                Node {
                    inode,
                    parent: owner,
                    name: repository.name.as_bytes().to_vec(),
                    kind: NodeKind::Directory {
                        location: DirectoryLocation::Repository {
                            owner: owner_name,
                            repo: repository.name,
                            path: String::new(),
                        },
                        children: None,
                    },
                },
            );
            if let NodeKind::Directory {
                children: Some(children),
                ..
            } = &mut nodes.get_mut(&owner).expect("owner").kind
            {
                children.push(inode);
            }
        }
        let names: HashMap<u64, Vec<u8>> = nodes
            .iter()
            .map(|(inode, node)| (*inode, node.name.clone()))
            .collect();
        for node in nodes.values_mut() {
            if let NodeKind::Directory {
                children: Some(children),
                ..
            } = &mut node.kind
            {
                children.sort_by(|left, right| names[left].cmp(&names[right]));
            }
        }
    }

    fn allocate_inode(&self) -> u64 {
        self.next_inode.fetch_add(1, Ordering::Relaxed)
    }

    fn ensure_directory(&self, inode: u64) -> Result<Vec<u64>> {
        if let Some(children) = self.children(inode)? {
            return Ok(children);
        }
        let _loading = self.load_lock.lock();
        if let Some(children) = self.children(inode)? {
            return Ok(children);
        }
        let location = match self.node(inode)?.kind {
            NodeKind::Directory {
                location: DirectoryLocation::Repository { owner, repo, path },
                ..
            } => (owner, repo, path),
            NodeKind::Directory { .. } => return Ok(Vec::new()),
            NodeKind::File(_) => return Err(Error(ENOTDIR)),
        };
        let contents = self.api.contents(&location.0, &location.1, &location.2)?;
        let mut child_ids = Vec::with_capacity(contents.len());
        let mut files = Vec::new();
        let mut nodes = self.nodes.write();
        for content in contents {
            let child = self.allocate_inode();
            let path = content.path;
            let kind = if content.kind == "dir" {
                NodeKind::Directory {
                    location: DirectoryLocation::Repository {
                        owner: location.0.clone(),
                        repo: location.1.clone(),
                        path,
                    },
                    children: None,
                }
            } else {
                let inline = content.payload.as_deref().and_then(decode_content);
                let file = FileNode {
                    owner: location.0.clone(),
                    repo: location.1.clone(),
                    path,
                    sha: content.sha,
                    size: content.size,
                    download_url: content.download_url,
                    inline,
                };
                files.push(file.clone());
                NodeKind::File(file)
            };
            nodes.insert(
                child,
                Node {
                    inode: child,
                    parent: inode,
                    name: content.name.into_bytes(),
                    kind,
                },
            );
            child_ids.push(child);
        }
        child_ids.sort_by(|left, right| {
            nodes
                .get(left)
                .expect("child")
                .name
                .cmp(&nodes.get(right).expect("child").name)
        });
        if let NodeKind::Directory { children, .. } =
            &mut nodes.get_mut(&inode).ok_or(Error(ENOENT))?.kind
        {
            *children = Some(child_ids.clone());
        }
        drop(nodes);
        if self.download_on_list {
            for file in files {
                let _ = self.materialize(&file);
            }
        }
        Ok(child_ids)
    }

    fn children(&self, inode: u64) -> Result<Option<Vec<u64>>> {
        match &self.node(inode)?.kind {
            NodeKind::Directory { children, .. } => Ok(children.clone()),
            NodeKind::File(_) => Err(Error(ENOTDIR)),
        }
    }

    fn node(&self, inode: u64) -> Result<Node> {
        self.nodes.read().get(&inode).cloned().ok_or(Error(ENOENT))
    }

    fn materialize(&self, file: &FileNode) -> Result<PathBuf> {
        self.cache.materialize(file, || {
            if let Some(bytes) = &file.inline {
                return Ok(bytes.clone());
            }
            let url = file.download_url.as_deref().ok_or(Error::IO)?;
            self.api.download(url)
        })
    }
}

impl Filesystem for GitHubFilesystem {
    fn statfs(&self) -> Result<StatFs> {
        let (used, files) = self.cache.usage();
        Ok(StatFs {
            block_size: 4096,
            io_size: 128 * 1024,
            total_bytes: self.cache.max_bytes,
            free_bytes: self.cache.max_bytes.saturating_sub(used),
            available_bytes: self.cache.max_bytes.saturating_sub(used),
            files,
            free_files: u64::MAX - files,
        })
    }

    fn metadata(&self, inode: u64) -> Result<Metadata> {
        let node = self.node(inode)?;
        let (kind, size, mode) = match node.kind {
            NodeKind::Directory { .. } => (FileType::Directory, 0, 0o555),
            NodeKind::File(file) => (FileType::File, file.size, 0o444),
        };
        Ok(Metadata {
            inode: node.inode,
            parent: node.parent,
            size,
            allocated_size: size,
            generation: 1,
            created_ns: 0,
            modified_ns: 0,
            accessed_ns: 0,
            mode,
            uid: 0,
            gid: 0,
            link_count: 1,
            kind,
        })
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)> {
        let children = self.ensure_directory(parent)?;
        let nodes = self.nodes.read();
        let child = children
            .into_iter()
            .find(|inode| nodes.get(inode).is_some_and(|node| node.name == name))
            .ok_or(Error(ENOENT))?;
        let kind = match nodes.get(&child).expect("child").kind {
            NodeKind::Directory { .. } => FileType::Directory,
            NodeKind::File(_) => FileType::File,
        };
        Ok((child, kind))
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult> {
        let children = self.ensure_directory(inode)?;
        let version = 1;
        if verifier != 0 && verifier != version {
            return Err(Error::STALE);
        }
        let start = usize::try_from(cookie).map_err(|_| Error::INVALID)?;
        if start > children.len() {
            return Err(Error::INVALID);
        }
        let nodes = self.nodes.read();
        for (index, child) in children.iter().enumerate().skip(start) {
            let node = nodes.get(child).expect("child");
            let kind = match node.kind {
                NodeKind::Directory { .. } => FileType::Directory,
                NodeKind::File(_) => FileType::File,
            };
            let next = u64::try_from(index + 1).unwrap_or(u64::MAX);
            if !sink.push(DirectoryEntry {
                name: &node.name,
                inode: *child,
                kind,
                next_cookie: next,
            }) {
                return Ok(ReadDirectoryResult {
                    verifier: version,
                    next_cookie: u64::try_from(index).unwrap_or(u64::MAX),
                    eof: false,
                });
            }
        }
        Ok(ReadDirectoryResult {
            verifier: version,
            next_cookie: u64::try_from(children.len()).unwrap_or(u64::MAX),
            eof: true,
        })
    }

    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize> {
        let file = match self.node(inode)?.kind {
            NodeKind::File(file) => file,
            NodeKind::Directory { .. } => return Err(Error(EISDIR)),
        };
        let path = self.materialize(&file)?;
        let mut input = fs::File::open(path).map_err(io_error)?;
        input.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        input.read(output).map_err(io_error)
    }
}

impl NetworkFilesystem for GitHubFilesystem {
    fn network_configuration(&self) -> NetworkConfiguration {
        NetworkConfiguration {
            lazy: true,
            supports_local_files: true,
            kernel_cache_policy: KernelCachePolicy::MetadataOnly,
        }
    }

    fn open_file(&self, inode: u64, writable: bool) -> Result<NetworkFile> {
        if writable {
            return Err(Error(EACCES));
        }
        let file = match self.node(inode)?.kind {
            NodeKind::File(file) => file,
            NodeKind::Directory { .. } => return Err(Error(EISDIR)),
        };
        if let Some(path) = self.cache.acquire(&file)? {
            Ok(NetworkFile::Local(LocalFile::new(
                path,
                generation(&file.sha),
            )))
        } else {
            Ok(NetworkFile::Remote)
        }
    }

    fn close_file(&self, _inode: u64, file: NetworkFile) -> Result<()> {
        if let NetworkFile::Local(local) = file {
            self.cache.release(local.path())?;
        }
        Ok(())
    }
}

struct Api {
    base: Url,
    client: Client,
    token: String,
}

impl Api {
    fn new(base: &str, token: &str) -> Result<Self> {
        let base = Url::parse(base).map_err(|_| Error::INVALID)?;
        let client = Client::builder()
            .user_agent("fsk-github-filesystem-example")
            .build()
            .map_err(|_| Error::IO)?;
        Ok(Self {
            base,
            client,
            token: token.to_owned(),
        })
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.base.clone();
        {
            let mut path = url.path_segments_mut().map_err(|()| Error::INVALID)?;
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    fn repositories(&self) -> Result<Vec<Repository>> {
        let mut all = Vec::new();
        for page in 1.. {
            let mut url = self.endpoint(&["user", "repos"])?;
            url.query_pairs_mut()
                .append_pair("per_page", "100")
                .append_pair("page", &page.to_string())
                .append_pair("sort", "full_name");
            let batch: Vec<Repository> = self.get(url)?.json().map_err(|_| Error::IO)?;
            let done = batch.len() < 100;
            all.extend(batch);
            if done {
                break;
            }
        }
        Ok(all)
    }

    fn contents(&self, owner: &str, repo: &str, path: &str) -> Result<Vec<Content>> {
        let mut segments = vec!["repos", owner, repo, "contents"];
        segments.extend(path.split('/').filter(|part| !part.is_empty()));
        let response = self.get(self.endpoint(&segments)?)?;
        response.json().map_err(|_| Error::IO)
    }

    fn download(&self, url: &str) -> Result<Vec<u8>> {
        let url = Url::parse(url).map_err(|_| Error::IO)?;
        self.get(url)?
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|_| Error::IO)
    }

    fn get(&self, url: Url) -> Result<reqwest::blocking::Response> {
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .map_err(|_| Error::IO)?;
        if response.status().as_u16() == 404 {
            return Err(Error(ENOENT));
        }
        if !response.status().is_success() {
            return Err(Error::IO);
        }
        Ok(response)
    }
}

#[derive(Deserialize)]
struct Repository {
    name: String,
    owner: Owner,
}
#[derive(Deserialize)]
struct Owner {
    login: String,
}
#[derive(Deserialize)]
struct Content {
    name: String,
    path: String,
    sha: String,
    size: u64,
    #[serde(rename = "type")]
    kind: String,
    download_url: Option<String>,
    #[serde(rename = "content")]
    payload: Option<String>,
}

fn decode_content(value: &str) -> Option<Vec<u8>> {
    STANDARD
        .decode(
            value
                .bytes()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect::<Vec<_>>(),
        )
        .ok()
}

struct Cache {
    root: PathBuf,
    max_bytes: u64,
    max_idle: Duration,
    leases: Mutex<HashMap<PathBuf, usize>>,
    mutation: Mutex<()>,
}

impl Cache {
    fn new(root: PathBuf, max_bytes: u64, max_idle: Duration) -> Result<Self> {
        fs::create_dir_all(&root).map_err(io_error)?;
        let cache = Self {
            root,
            max_bytes,
            max_idle,
            leases: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
        };
        cache.cleanup()?;
        Ok(cache)
    }

    fn path(&self, file: &FileNode) -> PathBuf {
        let digest = Sha256::digest(format!(
            "{}/{}/{}:{}",
            file.owner, file.repo, file.path, file.sha
        ));
        self.root.join(format!("{digest:x}.blob"))
    }

    fn materialize(
        &self,
        file: &FileNode,
        fetch: impl FnOnce() -> Result<Vec<u8>>,
    ) -> Result<PathBuf> {
        let mutation_guard = self.mutation.lock();
        let path = self.path(file);
        if path.is_file() {
            touch(&path)?;
            return Ok(path);
        }
        let bytes = fetch()?;
        let temporary = self
            .root
            .join(format!(".{}.{}.tmp", std::process::id(), now_ns()));
        let mut output = fs::File::create(&temporary).map_err(io_error)?;
        output
            .write_all(&bytes)
            .and_then(|()| output.sync_all())
            .map_err(io_error)?;
        fs::rename(&temporary, &path).map_err(io_error)?;
        drop(mutation_guard);
        self.cleanup_except(Some(&path))?;
        Ok(path)
    }

    fn acquire(&self, file: &FileNode) -> Result<Option<PathBuf>> {
        let path = self.path(file);
        if !path.is_file() {
            return Ok(None);
        }
        touch(&path)?;
        *self.leases.lock().entry(path.clone()).or_default() += 1;
        Ok(Some(path))
    }

    fn release(&self, path: &Path) -> Result<()> {
        let mut leases = self.leases.lock();
        if let Some(count) = leases.get_mut(path) {
            *count -= 1;
            if *count == 0 {
                leases.remove(path);
            }
        }
        drop(leases);
        self.cleanup_except(None)
    }

    fn usage(&self) -> (u64, u64) {
        let entries = self.entries();
        (
            entries.iter().map(|entry| entry.size).sum(),
            u64::try_from(entries.len()).unwrap_or(u64::MAX),
        )
    }

    fn cleanup(&self) -> Result<()> {
        self.cleanup_except(None)
    }

    fn cleanup_except(&self, protected: Option<&Path>) -> Result<()> {
        let _mutation = self.mutation.lock();
        let leased: HashSet<PathBuf> = self.leases.lock().keys().cloned().collect();
        let now = SystemTime::now();
        let mut entries = self.entries();
        for entry in &entries {
            if protected != Some(entry.path.as_path())
                && !leased.contains(&entry.path)
                && now.duration_since(entry.accessed).unwrap_or_default() > self.max_idle
            {
                let _ = fs::remove_file(&entry.path);
            }
        }
        entries = self.entries();
        entries.sort_by_key(|entry| entry.accessed);
        let mut used: u64 = entries.iter().map(|entry| entry.size).sum();
        let mut existing = entries.len();
        for entry in entries {
            if used <= self.max_bytes || existing == 1 {
                break;
            }
            if protected == Some(entry.path.as_path()) || leased.contains(&entry.path) {
                continue;
            }
            fs::remove_file(&entry.path).map_err(io_error)?;
            used = used.saturating_sub(entry.size);
            existing -= 1;
        }
        Ok(())
    }

    fn entries(&self) -> Vec<CacheEntry> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(std::ffi::OsStr::to_str) != Some("blob") {
                    return None;
                }
                let metadata = entry.metadata().ok()?;
                Some(CacheEntry {
                    path,
                    size: metadata.len(),
                    accessed: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                })
            })
            .collect()
    }
}

struct CacheEntry {
    path: PathBuf,
    size: u64,
    accessed: SystemTime,
}

fn touch(path: &Path) -> Result<()> {
    filetime::set_file_mtime(path, filetime::FileTime::now()).map_err(io_error)
}
#[allow(clippy::needless_pass_by_value)]
fn io_error(error: std::io::Error) -> Error {
    Error(error.raw_os_error().unwrap_or(5))
}
fn generation(sha: &str) -> u64 {
    let digest = Sha256::digest(sha.as_bytes());
    u64::from_le_bytes(digest[..8].try_into().expect("eight bytes"))
}

// Tests live under `tests/` to keep the example source focused.
#[cfg(test)]
#[path = "../tests/fs/mod.rs"]
mod tests;
