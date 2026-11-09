use fsk::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, KernelCachePolicy, Metadata,
    NetworkConfiguration, NetworkFile, NetworkFilesystem, ROOT_INODE, ReadDirectoryResult, Result,
    SetMetadata, StatFs,
};
use reqwest::blocking::Client;
use serde::{Deserialize, Deserializer, de::Error as _};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Read},
    sync::Mutex,
};
use unicode_normalization::UnicodeNormalization;

const API: &str = "https://www.googleapis.com/drive/v3";
const FOLDER: &str = "application/vnd.google-apps.folder";
const SHORTCUT: &str = "application/vnd.google-apps.shortcut";
const MAX_BUFFERED_FILE_SIZE: u64 = 256 * 1024 * 1024;
// Folder shortcuts and multi-parent folders turn the Drive listing into a DAG
// whose aliases can multiply exponentially. Bound both the depth of a single
// alias path and the total number of aliases one walk may create; realistic
// trees stay well below these limits, while pathological shortcut graphs stop
// quickly instead of exhausting memory. These bounds only apply to repeated
// mounts (aliases); the first mount of every remote file is always created.
// A shortcut stores both its own id and its target's id on the path, so allow
// enough room for a legitimate deep tree to also carry aliases.
const MAX_ALIAS_DEPTH: usize = 256;
const MAX_ALIAS_NODES: usize = 10_000;

fn deserialize_u64_string<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer)?
        .parse()
        .map_err(D::Error::custom)
}

fn inode_index(inode: u64) -> Option<usize> {
    usize::try_from(inode).ok()?.checked_sub(1)
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

#[cfg(windows)]
const fn current_uid() -> u32 {
    0
}

#[cfg(windows)]
const fn current_gid() -> u32 {
    0
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveFile {
    id: String,
    name: String,
    mime_type: String,
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_u64_string")]
    size: u64,
    #[serde(default)]
    capabilities: Capabilities,
    shortcut_details: Option<ShortcutDetails>,
    #[serde(skip)]
    resolved_target: Option<ShortcutTarget>,
    #[serde(default)]
    version: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShortcutDetails {
    target_id: String,
}

#[derive(Clone)]
struct ShortcutTarget {
    id: String,
    mime_type: String,
    size: u64,
    capabilities: Capabilities,
    version: String,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    #[serde(default)]
    can_modify_content: bool,
    #[serde(default)]
    can_download: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilePage {
    #[serde(default)]
    files: Vec<DriveFile>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DrivePage {
    #[serde(default)]
    drives: Vec<SharedDrive>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct SharedDrive {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct RootFile {
    id: String,
}

#[derive(Deserialize)]
struct FileVersion {
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct About {
    storage_quota: StorageQuota,
}

#[derive(Deserialize)]
struct StorageQuota {
    limit: Option<String>,
    usage: String,
}

#[derive(Clone)]
struct RemoteFile {
    id: String,
    mime_type: String,
}

struct Node {
    parent: u64,
    name: Vec<u8>,
    kind: FileType,
    size: u64,
    remote: Option<RemoteFile>,
    writable: bool,
}

pub struct GoogleDrive {
    client: Client,
    token: String,
    nodes: Vec<Node>,
    children: HashMap<u64, Vec<u64>>,
    cache: Mutex<HashMap<String, Vec<u8>>>,
    sizes: Mutex<HashMap<String, u64>>,
    writes: Mutex<()>,
    dirty: Mutex<HashSet<String>>,
    versions: Mutex<HashMap<String, String>>,
    total_bytes: u64,
    free_bytes: u64,
}

impl GoogleDrive {
    pub fn connect(token: String) -> io::Result<Self> {
        let mut filesystem = Self {
            client: Client::builder()
                .user_agent("fsk-google-drive-example")
                .build()
                .map_err(io::Error::other)?,
            token,
            nodes: vec![Node {
                parent: ROOT_INODE,
                name: Vec::new(),
                kind: FileType::Directory,
                size: 0,
                remote: None,
                writable: false,
            }],
            children: HashMap::new(),
            cache: Mutex::new(HashMap::new()),
            sizes: Mutex::new(HashMap::new()),
            writes: Mutex::new(()),
            dirty: Mutex::new(HashSet::new()),
            versions: Mutex::new(HashMap::new()),
            total_bytes: u64::MAX,
            free_bytes: u64::MAX,
        };
        filesystem.load_quota()?;
        let my_drive =
            filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
        let shared = filesystem.add_node(
            ROOT_INODE,
            "Shared with me",
            FileType::Directory,
            0,
            None,
            false,
        );
        let shared_drives = filesystem.add_node(
            ROOT_INODE,
            "Shared drives",
            FileType::Directory,
            0,
            None,
            false,
        );

        let root_id = filesystem.resolve_root_id()?;
        let account_files = filesystem.resolve_shortcuts(filesystem.list_files(
            "user",
            None,
            Some("trashed = false"),
        )?);
        filesystem.add_tree(my_drive, &root_id, &account_files);
        let shared_files = filesystem.resolve_shortcuts(filesystem.list_files(
            "user",
            None,
            Some("sharedWithMe = true and trashed = false"),
        )?);
        filesystem.add_shared_tree(shared, &shared_files, &account_files);
        for drive in filesystem.list_drives()? {
            let root = filesystem.add_node(
                shared_drives,
                &drive.name,
                FileType::Directory,
                0,
                None,
                false,
            );
            let files = filesystem.resolve_shortcuts(filesystem.list_files(
                "drive",
                Some(&drive.id),
                Some("trashed = false"),
            )?);
            filesystem.add_tree(root, &drive.id, &files);
        }
        filesystem.hydrate_exports();
        Ok(filesystem)
    }

    // The Drive API does not report an exported byte length for native Google
    // documents. Materialize those during indexing so getattr can return the
    // exact size; ordinary files remain lazy.
    fn hydrate_exports(&mut self) {
        let exports: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                let remote = node.remote.as_ref()?;
                export_format(&remote.mime_type).1?;
                Some((u64::try_from(index + 1).ok()?, remote.clone()))
            })
            .collect();
        for (inode, remote) in exports {
            if let Ok(data) = self.download(&remote) {
                let index = inode_index(inode).expect("allocated inode fits usize");
                self.nodes[index].size = u64::try_from(data.len()).unwrap_or(u64::MAX);
            } else {
                let index = inode_index(inode).expect("allocated inode fits usize");
                let parent = self.nodes[index].parent;
                if let Some(children) = self.children.get_mut(&parent) {
                    children.retain(|child| *child != inode);
                }
            }
        }
    }

    fn request(&self, url: &str) -> reqwest::blocking::RequestBuilder {
        self.client.get(url).bearer_auth(&self.token)
    }

    fn load_quota(&mut self) -> io::Result<()> {
        let about: About = self
            .request(&format!("{API}/about"))
            .query(&[("fields", "storageQuota(limit,usage)")])
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::json)
            .map_err(io::Error::other)?;
        let used = about.storage_quota.usage.parse::<u64>().unwrap_or(0);
        if let Some(limit) = about
            .storage_quota
            .limit
            .and_then(|value| value.parse::<u64>().ok())
        {
            self.total_bytes = limit;
            self.free_bytes = limit.saturating_sub(used);
        }
        Ok(())
    }

    fn resolve_root_id(&self) -> io::Result<String> {
        let root: RootFile = self
            .request(&format!("{API}/files/root"))
            .query(&[("fields", "id")])
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::json)
            .map_err(io::Error::other)?;
        Ok(root.id)
    }

    fn resolve_shortcuts(&self, files: Vec<DriveFile>) -> Vec<DriveFile> {
        files
            .into_iter()
            .filter_map(|mut file| {
                if file.mime_type != SHORTCUT {
                    return supported_file(&file).then_some(file);
                }
                let target_id = file.shortcut_details.as_ref()?.target_id.clone();
                let target = self.get_file(&target_id).ok()?;
                file.resolved_target = Some(ShortcutTarget {
                    id: target.id,
                    mime_type: target.mime_type,
                    size: target.size,
                    capabilities: target.capabilities,
                    version: target.version,
                });
                file.shortcut_details = None;
                supported_file(&file).then_some(file)
            })
            .collect()
    }

    fn get_file(&self, id: &str) -> io::Result<DriveFile> {
        self.request(&format!("{API}/files/{id}"))
            .query(&[
                (
                    "fields",
                    "id,name,mimeType,parents,size,version,capabilities(canModifyContent,canDownload),shortcutDetails(targetId)",
                ),
                ("supportsAllDrives", "true"),
            ])
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::json)
            .map_err(io::Error::other)
    }

    fn list_files(
        &self,
        corpus: &str,
        drive_id: Option<&str>,
        query: Option<&str>,
    ) -> io::Result<Vec<DriveFile>> {
        let mut output = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut params = vec![
                ("corpora", corpus.to_owned()),
                (
                    "fields",
                    "nextPageToken,files(id,name,mimeType,parents,size,version,capabilities(canModifyContent,canDownload),shortcutDetails(targetId))"
                        .to_owned(),
                ),
                ("pageSize", "1000".to_owned()),
                ("supportsAllDrives", "true".to_owned()),
                ("includeItemsFromAllDrives", "true".to_owned()),
            ];
            if let Some(value) = drive_id {
                params.push(("driveId", value.to_owned()));
            }
            if let Some(value) = query {
                params.push(("q", value.to_owned()));
            }
            if let Some(value) = &page_token {
                params.push(("pageToken", value.clone()));
            }
            let page: FilePage = self
                .request(&format!("{API}/files"))
                .query(&params)
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::json)
                .map_err(io::Error::other)?;
            output.extend(page.files);
            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }
        Ok(output)
    }

    fn list_drives(&self) -> io::Result<Vec<SharedDrive>> {
        let mut output = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut params = vec![
                ("pageSize", "100".to_owned()),
                ("fields", "nextPageToken,drives(id,name)".to_owned()),
            ];
            if let Some(value) = &page_token {
                params.push(("pageToken", value.clone()));
            }
            let page: DrivePage = self
                .request(&format!("{API}/drives"))
                .query(&params)
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::json)
                .map_err(io::Error::other)?;
            output.extend(page.drives);
            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }
        Ok(output)
    }

    fn add_tree(&mut self, root_inode: u64, root_id: &str, files: &[DriveFile]) {
        let mut ids = HashMap::from([(root_id.to_owned(), HashSet::from([root_inode]))]);
        let mut ancestry = HashMap::from([(root_inode, Vec::new())]);
        self.expand_tree_aliases(&mut ids, &mut ancestry, files);
    }

    #[cfg(test)]
    fn add_tree_with_limits(
        &mut self,
        root_inode: u64,
        root_id: &str,
        files: &[DriveFile],
        max_nodes: usize,
        max_depth: usize,
    ) {
        let mut ids = HashMap::from([(root_id.to_owned(), HashSet::from([root_inode]))]);
        let mut ancestry = HashMap::from([(root_inode, Vec::new())]);
        self.expand_tree_aliases_with(&mut ids, &mut ancestry, files, max_nodes, max_depth);
    }

    fn add_shared_tree(&mut self, root_inode: u64, roots: &[DriveFile], files: &[DriveFile]) {
        let mut ids: HashMap<String, HashSet<u64>> = HashMap::new();
        // Every inode created below the "Shared with me" root records the remote
        // ids on its path from that root. This lets the walk below stop at a
        // cycle instead of unfolding one forever.
        let mut ancestry: HashMap<u64, Vec<String>> = HashMap::from([(root_inode, Vec::new())]);
        // Pagination overlaps can list the same shared file twice. Attaching
        // both copies would create duplicate root aliases and double every
        // expansion below them, so keep the first alias per remote id.
        let mut seen_roots: HashSet<&str> = HashSet::new();
        for file in roots {
            if !seen_roots.insert(file.id.as_str()) {
                continue;
            }
            let inode = self.add_drive_file(root_inode, file);
            register_tree_ids(&mut ids, file, &[inode]);
            ancestry.insert(inode, tree_ids(file));
        }
        self.expand_tree_aliases(&mut ids, &mut ancestry, files);
    }

    // Attach every listed file beneath every known alias of its parents,
    // repeating until a pass adds nothing new so the result does not depend on
    // the order Drive listed the files in. Direct shares are also seeded as
    // aliases here (rather than attached once and forgotten) so a child listed
    // before a second alias of its parent is still replayed under that alias.
    //
    // Termination: `seen` deduplicates (remote file id, parent inode) pairs and
    // every created inode is one such pair, while `ancestry` refuses to revisit
    // a remote id already on the path. Since that path can only grow and never
    // repeats an id, cycles cannot recurse. `MAX_ALIAS_DEPTH` and
    // `MAX_ALIAS_NODES` additionally stop nested shortcuts and diamond DAGs from
    // multiplying without bound. Keying on the listing id (not the slice index)
    // also keeps a duplicated listing entry from creating a second child.
    //
    // The caps apply only to aliases: a mount is an alias when one of the
    // file's own remote ids is already reachable before the mount, so the first
    // (and only) inode for each listed file is never counted or truncated.
    fn expand_tree_aliases(
        &mut self,
        ids: &mut HashMap<String, HashSet<u64>>,
        ancestry: &mut HashMap<u64, Vec<String>>,
        files: &[DriveFile],
    ) {
        self.expand_tree_aliases_with(ids, ancestry, files, MAX_ALIAS_NODES, MAX_ALIAS_DEPTH);
    }

    fn expand_tree_aliases_with(
        &mut self,
        ids: &mut HashMap<String, HashSet<u64>>,
        ancestry: &mut HashMap<u64, Vec<String>>,
        files: &[DriveFile],
        max_nodes: usize,
        max_depth: usize,
    ) {
        let mut seen: HashSet<(&str, u64)> = HashSet::new();
        let mut aliases = 0_usize;
        let mut node_cap_reached = false;
        let mut depth_cap_reached = false;
        loop {
            let mut added = false;
            for file in files {
                for parent in tree_parents(file, ids) {
                    if !seen.insert((file.id.as_str(), parent)) {
                        continue;
                    }
                    let Some(parent_ancestry) = ancestry.get(&parent) else {
                        continue;
                    };
                    let file_ids = tree_ids(file);
                    // A remote id may appear at most once on a path; seeing one
                    // again means the listing contains a cycle, so do not recurse.
                    if file_ids.iter().any(|id| parent_ancestry.contains(id)) {
                        continue;
                    }
                    // A first mount has no inode for any of the file's remote
                    // ids yet; only repeated mounts are aliases, and only those
                    // are bounded.
                    let alias = file_ids
                        .iter()
                        .any(|id| ids.get(id).is_some_and(|inodes| !inodes.is_empty()));
                    if alias {
                        // Do not descend past the configured alias depth.
                        if parent_ancestry.len() >= max_depth {
                            if !depth_cap_reached {
                                eprintln!(
                                    "drive: stopping alias expansion at depth {max_depth}; \
                                     some duplicate aliases may be omitted"
                                );
                                depth_cap_reached = true;
                            }
                            continue;
                        }
                        if node_cap_reached || aliases >= max_nodes {
                            if !node_cap_reached {
                                eprintln!(
                                    "drive: stopping alias expansion after {max_nodes} aliases; \
                                     some duplicate aliases may be omitted"
                                );
                                node_cap_reached = true;
                            }
                            continue;
                        }
                    }
                    let mut chain = parent_ancestry.clone();
                    chain.extend(file_ids.iter().cloned());
                    let inode = self.add_drive_file(parent, file);
                    ancestry.insert(inode, chain);
                    register_tree_ids(ids, file, &[inode]);
                    if alias {
                        aliases += 1;
                    }
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
    }

    fn add_drive_file(&mut self, parent: u64, file: &DriveFile) -> u64 {
        let (remote_id, mime_type, size, _, version) = effective_file(file);
        let kind = if mime_type == FOLDER {
            FileType::Directory
        } else {
            FileType::File
        };
        let (name, remote) = if kind == FileType::Directory {
            (file.name.clone(), None)
        } else {
            let (suffix, _) = export_format(mime_type);
            (
                format!("{}{}", file.name, suffix),
                Some(RemoteFile {
                    id: remote_id.to_owned(),
                    mime_type: mime_type.to_owned(),
                }),
            )
        };
        let writable = drive_file_is_writable(file);
        if kind == FileType::File {
            self.versions
                .get_mut()
                .expect("version mutex is not poisoned during indexing")
                .entry(remote_id.to_owned())
                .or_insert_with(|| version.to_owned());
        }
        self.add_node(parent, &name, kind, size, remote, writable)
    }

    fn add_node(
        &mut self,
        parent: u64,
        name: &str,
        kind: FileType,
        size: u64,
        remote: Option<RemoteFile>,
        writable: bool,
    ) -> u64 {
        let base = portable_name(name);
        let mut display = base.clone();
        let mut suffix = 2_u64;
        while self.children.get(&parent).is_some_and(|children| {
            children.iter().any(|inode| {
                let index = inode_index(*inode).expect("allocated inode fits usize");
                names_equal(&self.nodes[index].name, &display)
            })
        }) {
            display.clone_from(&base);
            let suffix_bytes = format!(" ({suffix})").into_bytes();
            truncate_utf8(&mut display, 240_usize.saturating_sub(suffix_bytes.len()));
            display.extend_from_slice(&suffix_bytes);
            suffix += 1;
        }
        let inode = u64::try_from(self.nodes.len() + 1).unwrap_or(u64::MAX);
        self.nodes.push(Node {
            parent,
            name: display,
            kind,
            size,
            remote,
            writable,
        });
        self.children.entry(parent).or_default().push(inode);
        inode
    }

    fn download(&self, remote: &RemoteFile) -> Result<Vec<u8>> {
        if let Some(data) = self.cache.lock().map_err(|_| Error(5))?.get(&remote.id) {
            return Ok(data.clone());
        }
        let (_, export_mime) = export_format(&remote.mime_type);
        let mut request = if let Some(mime) = export_mime {
            self.request(&format!("{API}/files/{}/export", remote.id))
                .query(&[("mimeType", mime)])
        } else {
            self.request(&format!("{API}/files/{}", remote.id))
                .query(&[("alt", "media"), ("supportsAllDrives", "true")])
        };
        request = request.header(reqwest::header::ACCEPT, "*/*");
        let response = request
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|_| Error::IO)?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BUFFERED_FILE_SIZE)
        {
            return Err(Error(27));
        }
        let mut data = Vec::new();
        response
            .take(MAX_BUFFERED_FILE_SIZE + 1)
            .read_to_end(&mut data)
            .map_err(|_| Error::IO)?;
        if u64::try_from(data.len()).unwrap_or(u64::MAX) > MAX_BUFFERED_FILE_SIZE {
            return Err(Error(27));
        }
        cache_download(&self.cache, &remote.id, data)
    }

    fn upload(&self, remote: &RemoteFile, data: &[u8]) -> Result<()> {
        if u64::try_from(data.len()).unwrap_or(u64::MAX) > MAX_BUFFERED_FILE_SIZE {
            return Err(Error(27));
        }
        let (current_version, etag) = self.remote_version(&remote.id)?;
        let expected_version = self
            .versions
            .lock()
            .map_err(|_| Error::IO)?
            .get(&remote.id)
            .cloned()
            .unwrap_or_default();
        if !version_matches(&expected_version, &current_version) {
            return Err(Error::STALE);
        }
        let response = self
            .client
            .patch(format!(
                "https://www.googleapis.com/upload/drive/v3/files/{}",
                remote.id
            ))
            .bearer_auth(&self.token)
            .query(&[
                ("uploadType", "resumable"),
                ("supportsAllDrives", "true"),
                ("fields", "version"),
            ])
            .header(reqwest::header::IF_MATCH, etag)
            .header("X-Upload-Content-Type", &remote.mime_type)
            .header("X-Upload-Content-Length", data.len())
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|_| Error::IO)?;
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(Error::IO)?;
        let updated: FileVersion = self
            .client
            .put(location)
            .header(reqwest::header::CONTENT_TYPE, &remote.mime_type)
            .header(reqwest::header::CONTENT_LENGTH, data.len())
            .body(data.to_vec())
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::json)
            .map_err(|_| Error::IO)?;
        self.versions
            .lock()
            .map_err(|_| Error::IO)?
            .insert(remote.id.clone(), updated.version);
        Ok(())
    }

    fn remote_version(&self, id: &str) -> Result<(String, String)> {
        let response = self
            .request(&format!("{API}/files/{id}"))
            .query(&[("fields", "version"), ("supportsAllDrives", "true")])
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|_| Error::IO)?;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .ok_or(Error::IO)?
            .to_owned();
        let version: FileVersion = response.json().map_err(|_| Error::IO)?;
        Ok((version.version, etag))
    }

    fn flush(&self, inode: u64) -> Result<()> {
        let remote = inode_index(inode)
            .and_then(|index| self.nodes.get(index))
            .and_then(|node| node.remote.as_ref())
            .ok_or(Error(2))?;
        self.flush_remote(remote)
    }

    fn flush_remote(&self, remote: &RemoteFile) -> Result<()> {
        let _write = self.writes.lock().map_err(|_| Error::IO)?;
        if !self
            .dirty
            .lock()
            .map_err(|_| Error::IO)?
            .contains(&remote.id)
        {
            return Ok(());
        }
        let data = self
            .cache
            .lock()
            .map_err(|_| Error::IO)?
            .get(&remote.id)
            .cloned()
            .ok_or(Error::IO)?;
        self.upload(remote, &data)?;
        self.dirty.lock().map_err(|_| Error::IO)?.remove(&remote.id);
        Ok(())
    }
}

fn drive_file_is_writable(file: &DriveFile) -> bool {
    let (_, mime_type, _, capabilities, _) = effective_file(file);
    mime_type != FOLDER
        && capabilities.can_modify_content
        && !mime_type.starts_with("application/vnd.google-apps.")
}

fn supported_file(file: &DriveFile) -> bool {
    let (_, mime_type, _, capabilities, _) = effective_file(file);
    mime_type == FOLDER
        || (capabilities.can_download
            && (!mime_type.starts_with("application/vnd.google-apps.")
                || export_format(mime_type).1.is_some()))
}

fn effective_file(file: &DriveFile) -> (&str, &str, u64, &Capabilities, &str) {
    file.resolved_target.as_ref().map_or(
        (
            file.id.as_str(),
            file.mime_type.as_str(),
            file.size,
            &file.capabilities,
            file.version.as_str(),
        ),
        |target| {
            (
                target.id.as_str(),
                target.mime_type.as_str(),
                target.size,
                &target.capabilities,
                target.version.as_str(),
            )
        },
    )
}

fn tree_parents(file: &DriveFile, ids: &HashMap<String, HashSet<u64>>) -> Vec<u64> {
    let mut output = Vec::new();
    let mut seen = HashSet::new();
    for parent in file.parents.iter().filter_map(|id| ids.get(id)) {
        for inode in parent {
            if seen.insert(*inode) {
                output.push(*inode);
            }
        }
    }
    output
}

fn tree_ids(file: &DriveFile) -> Vec<String> {
    let mut ids = vec![file.id.clone()];
    if let Some(target) = &file.resolved_target {
        ids.push(target.id.clone());
    }
    ids
}

fn register_tree_ids(ids: &mut HashMap<String, HashSet<u64>>, file: &DriveFile, inodes: &[u64]) {
    let mut keys = vec![file.id.as_str()];
    if let Some(target) = &file.resolved_target {
        keys.push(&target.id);
    }
    for key in keys {
        let values = ids.entry(key.to_owned()).or_default();
        values.extend(inodes.iter().copied());
    }
}

fn trim_portable_tail(output: &mut String) {
    while output.ends_with([' ', '.']) {
        output.pop();
    }
}

fn portable_name(name: &str) -> Vec<u8> {
    let mut output: String = name
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            character if character.is_control() => '_',
            _ => character,
        })
        .collect();
    trim_portable_tail(&mut output);
    // `pop` removes a whole char, so truncation never splits UTF-8.
    while output.len() > 240 {
        output.pop();
    }
    // Truncation can expose a trailing space or dot even when the original name
    // did not end in one, so trim again before returning the name.
    trim_portable_tail(&mut output);
    if output.is_empty() || matches!(output.as_str(), "." | "..") {
        "_".clone_into(&mut output);
    }
    let stem = output.split('.').next().unwrap_or_default();
    // Windows also reserves the superscript forms of the numbered devices, so
    // compare a normalised stem against the plain names.
    let normalized_stem: String = stem
        .chars()
        .map(|character| match character {
            '¹' => '1',
            '²' => '2',
            '³' => '3',
            other => other,
        })
        .collect();
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "CLOCK$", "COM1", "COM2", "COM3", "COM4",
        "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
        "LPT7", "LPT8", "LPT9",
    ];
    if reserved
        .iter()
        .any(|value| normalized_stem.eq_ignore_ascii_case(value))
    {
        output.insert(0, '_');
    }
    while output.len() > 240 {
        output.pop();
    }
    // Prepending `_` and re-truncating can expose another trailing space or dot.
    trim_portable_tail(&mut output);
    if output.is_empty() || matches!(output.as_str(), "." | "..") {
        "_".clone_into(&mut output);
    }
    output.into_bytes()
}

fn truncate_utf8(value: &mut Vec<u8>, max_len: usize) {
    let mut len = value.len().min(max_len);
    while len > 0 && std::str::from_utf8(&value[..len]).is_err() {
        len -= 1;
    }
    value.truncate(len);
}

fn names_equal(left: &[u8], right: &[u8]) -> bool {
    let normalize = |value: &[u8]| {
        String::from_utf8_lossy(value)
            .nfd()
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    normalize(left) == normalize(right)
}

fn cache_download(
    cache: &Mutex<HashMap<String, Vec<u8>>>,
    id: &str,
    downloaded: Vec<u8>,
) -> Result<Vec<u8>> {
    let mut cache = cache.lock().map_err(|_| Error::IO)?;
    if let Some(current) = cache.get(id) {
        return Ok(current.clone());
    }
    cache.insert(id.to_owned(), downloaded.clone());
    Ok(downloaded)
}

fn version_matches(expected: &str, current: &str) -> bool {
    expected.is_empty() || expected == current
}

fn export_format(mime: &str) -> (&'static str, Option<&'static str>) {
    match mime {
        "application/vnd.google-apps.document" => (
            ".docx",
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        ),
        "application/vnd.google-apps.spreadsheet" => (
            ".xlsx",
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        ),
        "application/vnd.google-apps.presentation" => (
            ".pptx",
            Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        ),
        "application/vnd.google-apps.drawing" => (".pdf", Some("application/pdf")),
        _ => ("", None),
    }
}

impl Filesystem for GoogleDrive {
    fn statfs(&self) -> Result<StatFs> {
        // Drive's About quota is account-wide. Visible shared files may consume
        // another owner's quota, so deriving quota deltas from mounted files can
        // make these values less accurate than the authoritative API snapshot.
        Ok(StatFs {
            block_size: 4096,
            io_size: 131_072,
            total_bytes: self.total_bytes,
            free_bytes: self.free_bytes,
            available_bytes: self.free_bytes,
            files: self.nodes.len() as u64,
            free_files: 0,
        })
    }

    fn metadata(&self, inode: u64) -> Result<Metadata> {
        let node = inode_index(inode)
            .and_then(|index| self.nodes.get(index))
            .ok_or(Error(2))?;
        let sizes = self.sizes.lock().map_err(|_| Error::IO)?;
        let size = node
            .remote
            .as_ref()
            .and_then(|remote| sizes.get(&remote.id))
            .copied()
            .unwrap_or(node.size);
        Ok(Metadata {
            inode,
            parent: node.parent,
            size,
            allocated_size: 0,
            generation: 1,
            created_ns: 0,
            modified_ns: 0,
            accessed_ns: 0,
            mode: if node.kind == FileType::Directory {
                0o555
            } else if node.writable {
                0o644
            } else {
                0o444
            },
            uid: current_uid(),
            gid: current_gid(),
            link_count: 1,
            kind: node.kind,
        })
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, FileType)> {
        self.children
            .get(&parent)
            .and_then(|children| {
                children.iter().find_map(|inode| {
                    let index = inode_index(*inode).expect("allocated inode fits usize");
                    let node = &self.nodes[index];
                    (node.name == name).then_some((*inode, node.kind))
                })
            })
            .ok_or(Error(2))
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        _verifier: u64,
        sink: &mut dyn DirectorySink,
    ) -> Result<ReadDirectoryResult> {
        let node = inode_index(inode)
            .and_then(|index| self.nodes.get(index))
            .ok_or(Error(2))?;
        if node.kind != FileType::Directory {
            return Err(Error(20));
        }
        let children = self.children.get(&inode).map_or(&[][..], Vec::as_slice);
        let start = usize::try_from(cookie).map_err(|_| Error::INVALID)?;
        for (index, child_inode) in children.iter().enumerate().skip(start) {
            let node_index = inode_index(*child_inode).expect("allocated inode fits usize");
            let child = &self.nodes[node_index];
            if !sink.push(DirectoryEntry {
                name: &child.name,
                inode: *child_inode,
                kind: child.kind,
                next_cookie: (index + 1) as u64,
            }) {
                return Ok(ReadDirectoryResult {
                    verifier: 1,
                    next_cookie: index as u64,
                    eof: false,
                });
            }
        }
        Ok(ReadDirectoryResult {
            verifier: 1,
            next_cookie: children.len() as u64,
            eof: true,
        })
    }

    fn read(&self, inode: u64, offset: u64, output: &mut [u8]) -> Result<usize> {
        let node = inode_index(inode)
            .and_then(|index| self.nodes.get(index))
            .ok_or(Error(2))?;
        let remote = node.remote.as_ref().ok_or(Error(21))?;
        let data = self.download(remote)?;
        let start = usize::try_from(offset).map_err(|_| Error::INVALID)?;
        let Some(input) = data.get(start..) else {
            return Ok(0);
        };
        let count = input.len().min(output.len());
        output[..count].copy_from_slice(&input[..count]);
        Ok(count)
    }

    fn write(&self, inode: u64, offset: u64, input: &[u8]) -> Result<usize> {
        let _write = self.writes.lock().map_err(|_| Error::IO)?;
        let node = inode_index(inode)
            .and_then(|index| self.nodes.get(index))
            .ok_or(Error(2))?;
        if !node.writable {
            return Err(Error(30));
        }
        let remote = node.remote.as_ref().ok_or(Error(21))?;
        let start = usize::try_from(offset).map_err(|_| Error::INVALID)?;
        let end = start.checked_add(input.len()).ok_or(Error::INVALID)?;
        if u64::try_from(end).unwrap_or(u64::MAX) > MAX_BUFFERED_FILE_SIZE {
            return Err(Error(27));
        }
        let mut data = self.download(remote)?;
        if data.len() < end {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(input);
        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        self.cache
            .lock()
            .map_err(|_| Error::IO)?
            .insert(remote.id.clone(), data);
        self.sizes
            .lock()
            .map_err(|_| Error::IO)?
            .insert(remote.id.clone(), size);
        self.dirty
            .lock()
            .map_err(|_| Error::IO)?
            .insert(remote.id.clone());
        Ok(input.len())
    }

    fn set_metadata(&self, inode: u64, update: SetMetadata) -> Result<Metadata> {
        let _write = self.writes.lock().map_err(|_| Error::IO)?;
        let Some(size) = update.size else {
            return Err(Error::NOT_SUPPORTED);
        };
        if update.mode.is_some()
            || update.uid.is_some()
            || update.gid.is_some()
            || update.accessed_ns.is_some()
            || update.modified_ns.is_some()
        {
            return Err(Error::NOT_SUPPORTED);
        }
        let node = inode_index(inode)
            .and_then(|index| self.nodes.get(index))
            .ok_or(Error(2))?;
        if !node.writable {
            return Err(Error(30));
        }
        if size > MAX_BUFFERED_FILE_SIZE {
            return Err(Error(27));
        }
        let remote = node.remote.as_ref().ok_or(Error(21))?;
        let mut data = self.download(remote)?;
        data.resize(usize::try_from(size).map_err(|_| Error::INVALID)?, 0);
        self.upload(remote, &data)?;
        self.cache
            .lock()
            .map_err(|_| Error::IO)?
            .insert(remote.id.clone(), data);
        self.sizes
            .lock()
            .map_err(|_| Error::IO)?
            .insert(remote.id.clone(), size);
        self.dirty.lock().map_err(|_| Error::IO)?.remove(&remote.id);
        self.metadata(inode)
    }

    fn synchronize(&self) -> Result<()> {
        let dirty: Vec<_> = self
            .dirty
            .lock()
            .map_err(|_| Error::IO)?
            .iter()
            .cloned()
            .collect();
        for id in dirty {
            let remote = self
                .nodes
                .iter()
                .filter_map(|node| node.remote.as_ref())
                .find(|remote| remote.id == id)
                .ok_or(Error(2))?;
            self.flush_remote(remote)?;
        }
        Ok(())
    }
}

impl NetworkFilesystem for GoogleDrive {
    fn network_configuration(&self) -> NetworkConfiguration {
        NetworkConfiguration {
            lazy: true,
            supports_local_files: false,
            kernel_cache_policy: KernelCachePolicy::Read,
        }
    }
    fn open_file(&self, inode: u64, writable: bool) -> Result<NetworkFile> {
        let node = inode_index(inode)
            .and_then(|index| self.nodes.get(index))
            .ok_or(Error(2))?;
        if writable && !node.writable {
            return Err(Error(30));
        }
        Ok(NetworkFile::Remote)
    }

    fn close_file(&self, inode: u64, _file: NetworkFile) -> Result<()> {
        self.flush(inode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filesystem() -> GoogleDrive {
        GoogleDrive {
            client: Client::new(),
            token: String::new(),
            nodes: vec![Node {
                parent: ROOT_INODE,
                name: Vec::new(),
                kind: FileType::Directory,
                size: 0,
                remote: None,
                writable: false,
            }],
            children: HashMap::new(),
            cache: Mutex::new(HashMap::new()),
            sizes: Mutex::new(HashMap::new()),
            writes: Mutex::new(()),
            dirty: Mutex::new(HashSet::new()),
            versions: Mutex::new(HashMap::new()),
            total_bytes: 0,
            free_bytes: 0,
        }
    }

    fn file(mime_type: &str, can_modify_content: bool) -> DriveFile {
        DriveFile {
            id: "id".to_owned(),
            name: "name".to_owned(),
            mime_type: mime_type.to_owned(),
            parents: Vec::new(),
            size: 0,
            capabilities: Capabilities {
                can_modify_content,
                can_download: true,
            },
            shortcut_details: None,
            resolved_target: None,
            version: "1".to_owned(),
        }
    }

    #[test]
    fn ordinary_files_follow_drive_write_capability() {
        assert!(drive_file_is_writable(&file("text/plain", true)));
        assert!(!drive_file_is_writable(&file("text/plain", false)));
    }

    #[test]
    fn folders_and_exported_workspace_files_stay_read_only() {
        assert!(!drive_file_is_writable(&file(FOLDER, true)));
        assert!(!drive_file_is_writable(&file(
            "application/vnd.google-apps.document",
            true,
        )));
    }

    #[test]
    fn parses_string_encoded_drive_file_size() {
        let file: DriveFile = serde_json::from_str(
            r#"{"id":"id","name":"file","mimeType":"text/plain","size":"42"}"#,
        )
        .expect("Drive API file should deserialize");
        assert_eq!(file.size, 42);
    }

    #[test]
    fn portable_names_avoid_host_special_names_and_characters() {
        assert_eq!(portable_name("a/b:c"), b"a_b_c");
        assert_eq!(portable_name("a\u{1f}b"), b"a_b");
        assert_eq!(portable_name("CON"), b"_CON");
        assert_eq!(portable_name("trailing. "), b"trailing");
        assert!(portable_name(&"x".repeat(300)).len() <= 240);
        let long_con = portable_name(&format!("CON.{}", "x".repeat(300)));
        assert!(long_con.starts_with(b"_CON"));
        assert!(long_con.len() <= 240);
        assert!(!long_con.ends_with(b" ") && !long_con.ends_with(b"."));
        // Prepending `_` makes this 241 bytes, so re-truncation exposes the
        // separator that follows `CON` and the trailing trim must remove it.
        let prefixed = portable_name(&format!("CON.{} x", " ".repeat(234)));
        assert_eq!(prefixed, b"_CON");
        assert!(!prefixed.ends_with(b" ") && !prefixed.ends_with(b"."));
        let truncated_space = portable_name(&format!("{} {}", "x".repeat(239), "y".repeat(50)));
        assert_eq!(truncated_space, "x".repeat(239).into_bytes());
        assert!(!truncated_space.ends_with(b" "));
        // Truncating a multibyte name must not split a character.
        let multibyte = portable_name(&("a".to_owned() + &"€".repeat(80)));
        assert!(multibyte.starts_with(b"a"));
        assert!(std::str::from_utf8(&multibyte).is_ok());
        assert!(multibyte.len() <= 240);
        assert_eq!(multibyte.len(), 1 + 79 * 3);
        assert_eq!(portable_name(&" ".repeat(300)), b"_");
        // Windows reserves the console/clock device aliases and the superscript
        // forms of the numbered ports.
        assert_eq!(portable_name("CONIN$"), b"_CONIN$");
        assert_eq!(portable_name("conout$"), b"_conout$");
        assert_eq!(portable_name("CLOCK$"), b"_CLOCK$");
        assert_eq!(portable_name("COM¹"), b"_COM\xC2\xB9");
        assert_eq!(portable_name("com²"), b"_com\xC2\xB2");
        assert_eq!(portable_name("LPT³"), b"_LPT\xC2\xB3");
        assert_eq!(portable_name("COM10"), b"COM10");
        // These only share a prefix with a reserved device; they must not be
        // rewritten by a loose prefix match.
        assert_eq!(portable_name("COM¹x"), b"COM\xC2\xB9x");
        assert_eq!(portable_name("CLOCK$foo"), b"CLOCK$foo");
        assert!(names_equal("É".as_bytes(), "é".as_bytes()));
        assert!(names_equal("é".as_bytes(), "e\u{301}".as_bytes()));
    }

    #[test]
    fn collision_suffixes_stay_within_portable_name_limit() {
        let mut filesystem = filesystem();
        // A three-byte character means the 236-byte truncation target for a
        // ` (n)` suffix falls inside a character, so byte-level truncation
        // would produce invalid UTF-8.
        let long_name = "€".repeat(80);
        let inodes: Vec<_> = (0..13)
            .map(|_| filesystem.add_node(ROOT_INODE, &long_name, FileType::File, 0, None, false))
            .collect();
        let names: Vec<&[u8]> = inodes
            .iter()
            .map(|inode| {
                filesystem.nodes[inode_index(*inode).unwrap()]
                    .name
                    .as_slice()
            })
            .collect();

        for name in &names {
            assert!(name.len() <= 240);
            assert!(std::str::from_utf8(name).is_ok());
        }
        assert!(names[9].ends_with(b" (10)"));
        assert!(names[10].ends_with(b" (11)"));
        let unique: HashSet<&[u8]> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn trees_use_actual_root_ids_and_include_shared_descendants() {
        let mut filesystem = filesystem();
        let my_drive =
            filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
        let shared = filesystem.add_node(
            ROOT_INODE,
            "Shared with me",
            FileType::Directory,
            0,
            None,
            false,
        );
        let mut folder = file(FOLDER, false);
        folder.id = "folder".to_owned();
        folder.parents = vec!["actual-root-id".to_owned()];
        let mut child = file("text/plain", false);
        child.id = "child".to_owned();
        child.parents = vec!["folder".to_owned()];

        filesystem.add_tree(my_drive, "actual-root-id", &[folder.clone(), child.clone()]);
        let (my_folder, _) = filesystem
            .lookup(my_drive, b"name")
            .expect("top-level folder");
        assert!(filesystem.lookup(my_folder, b"name").is_ok());

        filesystem.add_shared_tree(shared, &[folder], &[child]);
        let (shared_folder, _) = filesystem.lookup(shared, b"name").expect("shared folder");
        assert!(filesystem.lookup(shared_folder, b"name").is_ok());
    }

    #[test]
    fn nested_direct_shares_keep_root_and_parent_aliases() {
        let mut filesystem = filesystem();
        let shared = filesystem.add_node(
            ROOT_INODE,
            "Shared with me",
            FileType::Directory,
            0,
            None,
            false,
        );
        let mut folder = file(FOLDER, false);
        folder.id = "folder".to_owned();
        folder.name = "folder".to_owned();
        let mut child = file("text/plain", false);
        child.id = "child".to_owned();
        child.name = "child".to_owned();
        child.parents = vec!["folder".to_owned()];

        filesystem.add_shared_tree(shared, &[folder.clone(), child.clone()], &[folder, child]);

        let (shared_folder, _) = filesystem.lookup(shared, b"folder").expect("shared folder");
        assert!(filesystem.lookup(shared, b"child").is_ok());
        assert!(filesystem.lookup(shared_folder, b"child").is_ok());
    }

    #[test]
    fn duplicated_direct_shares_create_one_root_alias() {
        let mut filesystem = filesystem();
        let shared = filesystem.add_node(
            ROOT_INODE,
            "Shared with me",
            FileType::Directory,
            0,
            None,
            false,
        );
        let mut folder = file(FOLDER, false);
        folder.id = "folder".to_owned();
        folder.name = "folder".to_owned();
        let mut child = file("text/plain", false);
        child.id = "child".to_owned();
        child.name = "child".to_owned();
        child.parents = vec!["folder".to_owned()];

        // Pagination can overlap, listing the same share more than once.
        let roots = [folder.clone(), folder.clone(), child.clone(), child];
        filesystem.add_shared_tree(shared, &roots, &roots);

        let (folder_inode, _) = filesystem.lookup(shared, b"folder").expect("folder alias");
        assert!(filesystem.lookup(shared, b"folder (2)").is_err());
        assert!(filesystem.lookup(shared, b"child").is_ok());
        assert!(filesystem.lookup(shared, b"child (2)").is_err());
        assert!(filesystem.lookup(folder_inode, b"child").is_ok());
        assert!(filesystem.lookup(folder_inode, b"child (2)").is_err());
    }

    #[test]
    fn first_mounts_are_not_limited_by_the_alias_cap() {
        // A flat My Drive has one first mount per file and no aliases at all.
        const FILES: usize = 50;

        let mut filesystem = filesystem();
        let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
        let files: Vec<_> = (0..FILES)
            .map(|index| {
                let mut entry = file("text/plain", false);
                entry.id = format!("id{index}");
                entry.name = format!("file{index}");
                entry.parents = vec!["root".to_owned()];
                entry
            })
            .collect();

        // The alias cap is far below the number of files, but none of these
        // mounts is an alias, so every file still lands in the tree.
        filesystem.add_tree_with_limits(root, "root", &files, 4, 4);

        assert_eq!(filesystem.children[&root].len(), FILES);
        assert_eq!(filesystem.nodes.len(), 2 + FILES);
    }

    #[test]
    fn alias_expansion_stops_at_the_alias_cap() {
        // One real folder is mounted first, then every shortcut repeats its
        // remote id, so these are the only mounts the alias cap may drop.
        let mut filesystem = filesystem();
        let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
        let mut target = file(FOLDER, false);
        target.id = "target".to_owned();
        target.name = "target".to_owned();
        target.parents = vec!["root".to_owned()];
        let mut files = vec![target];
        for index in 0..10 {
            let mut shortcut = file(SHORTCUT, false);
            shortcut.id = format!("S{index}");
            shortcut.name.clone_from(&shortcut.id);
            shortcut.parents = vec!["root".to_owned()];
            shortcut.resolved_target = Some(ShortcutTarget {
                id: "target".to_owned(),
                mime_type: FOLDER.to_owned(),
                size: 0,
                capabilities: Capabilities::default(),
                version: "1".to_owned(),
            });
            files.push(shortcut);
        }

        filesystem.add_tree_with_limits(root, "root", &files, 3, MAX_ALIAS_DEPTH);

        // Filesystem root, the mounted root, the target, and exactly three
        // duplicate aliases.
        assert_eq!(filesystem.nodes.len(), 6);
        assert_eq!(filesystem.children[&root].len(), 4);
        assert!(filesystem.lookup(root, b"target").is_ok());
        for index in 0..3 {
            assert!(
                filesystem
                    .lookup(root, format!("S{index}").as_bytes())
                    .is_ok()
            );
        }
        assert!(filesystem.lookup(root, b"S3").is_err());
    }

    #[test]
    fn plain_deep_trees_are_not_truncated_by_alias_limits() {
        // The alias limits are far below this chain's depth, but every level is
        // a first mount, so the whole chain must still be created.
        const DEPTH: usize = 70;

        let mut filesystem = filesystem();
        let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
        let mut files = Vec::new();
        for level in 0..DEPTH {
            let mut folder = file(FOLDER, false);
            folder.id = format!("F{level}");
            folder.name.clone_from(&folder.id);
            folder.parents = vec![if level == 0 {
                "root".to_owned()
            } else {
                format!("F{}", level - 1)
            }];
            files.push(folder);
        }

        filesystem.add_tree_with_limits(root, "root", &files, 4, 4);

        assert_eq!(filesystem.nodes.len(), 2 + DEPTH);
        let mut parent = root;
        for level in 0..DEPTH {
            let (inode, _) = filesystem
                .lookup(parent, format!("F{level}").as_bytes())
                .expect("every level is mounted");
            parent = inode;
        }
    }

    #[test]
    fn shortcut_alias_expansion_is_bounded() {
        // Each folder contains both its child folder and a shortcut to that
        // child, so the unbounded walk would create roughly 2^20 aliases.
        const DEPTH: usize = 20;

        let mut filesystem = filesystem();
        let shared = filesystem.add_node(
            ROOT_INODE,
            "Shared with me",
            FileType::Directory,
            0,
            None,
            false,
        );

        let mut root = file(FOLDER, false);
        root.id = "F0".to_owned();
        root.name = "F0".to_owned();
        let mut files = vec![root.clone()];
        for level in 0..DEPTH {
            let child_id = format!("F{}", level + 1);
            let mut child = file(FOLDER, false);
            child.id.clone_from(&child_id);
            child.name.clone_from(&child_id);
            child.parents = vec![format!("F{level}")];
            let mut shortcut = file(SHORTCUT, false);
            shortcut.id = format!("S{}", level + 1);
            shortcut.name.clone_from(&shortcut.id);
            shortcut.parents = vec![format!("F{level}")];
            shortcut.resolved_target = Some(ShortcutTarget {
                id: child_id,
                mime_type: FOLDER.to_owned(),
                size: 0,
                capabilities: Capabilities::default(),
                version: "1".to_owned(),
            });
            files.push(child);
            files.push(shortcut);
        }

        filesystem.add_shared_tree(shared, &[root], &files);

        // Filesystem root, "Shared with me", the directly shared root, one
        // first mount per level, and exactly the alias cap's worth of aliases.
        assert_eq!(filesystem.nodes.len(), 3 + DEPTH + MAX_ALIAS_NODES);
    }

    #[test]
    fn alias_depth_is_bounded() {
        // The real folder chain must stay complete; only a chain of shortcuts
        // to already-mounted folders runs into the alias depth limit.
        const FOLDERS: usize = 10;
        const MAX_DEPTH: usize = 6;

        let mut filesystem = filesystem();
        let shared = filesystem.add_node(
            ROOT_INODE,
            "Shared with me",
            FileType::Directory,
            0,
            None,
            false,
        );

        let mut files = Vec::new();
        for index in 0..FOLDERS {
            let mut folder = file(FOLDER, false);
            folder.id = format!("A{index}");
            folder.name.clone_from(&folder.id);
            folder.parents = vec!["F0".to_owned()];
            files.push(folder);
        }
        for index in 0..FOLDERS {
            let mut shortcut = file(SHORTCUT, false);
            shortcut.id = format!("S{index}");
            shortcut.name.clone_from(&shortcut.id);
            shortcut.parents = vec![if index == 0 {
                "F0".to_owned()
            } else {
                format!("S{}", index - 1)
            }];
            shortcut.resolved_target = Some(ShortcutTarget {
                id: format!("A{index}"),
                mime_type: FOLDER.to_owned(),
                size: 0,
                capabilities: Capabilities::default(),
                version: "1".to_owned(),
            });
            files.push(shortcut);
        }

        // Treat the shared root inode as the remote folder `F0`, then run the
        // walk with a depth limit small enough to truncate aliases only.
        filesystem.add_tree_with_limits(shared, "F0", &files, MAX_ALIAS_NODES, MAX_DEPTH);

        // Every real folder is a first mount and survives.
        for index in 0..FOLDERS {
            assert!(
                filesystem
                    .lookup(shared, format!("A{index}").as_bytes())
                    .is_ok()
            );
        }
        // A shortcut stores its own id and its target's id, so the chain grows
        // by two per level. Three aliases fit below the depth limit; the fourth
        // is truncated.
        let (s0, _) = filesystem.lookup(shared, b"S0").expect("first alias");
        let (s1, _) = filesystem.lookup(s0, b"S1").expect("second alias");
        let (s2, _) = filesystem.lookup(s1, b"S2").expect("third alias");
        assert!(filesystem.lookup(s2, b"S3").is_err());
        assert_eq!(filesystem.nodes.len(), 2 + FOLDERS + 3);
    }

    #[test]
    fn directly_shared_chains_are_complete_regardless_of_listing_order() {
        let mut filesystem = filesystem();
        let shared = filesystem.add_node(
            ROOT_INODE,
            "Shared with me",
            FileType::Directory,
            0,
            None,
            false,
        );
        let mut outer = file(FOLDER, false);
        outer.id = "outer".to_owned();
        outer.name = "outer".to_owned();
        let mut inner = file(FOLDER, false);
        inner.id = "inner".to_owned();
        inner.name = "inner".to_owned();
        inner.parents = vec!["outer".to_owned()];
        let mut leaf = file("text/plain", false);
        leaf.id = "leaf".to_owned();
        leaf.name = "leaf".to_owned();
        leaf.parents = vec!["inner".to_owned()];

        // Drive listing order is unspecified, so descendants may arrive first.
        let listed = [leaf.clone(), inner.clone(), outer.clone()];
        filesystem.add_shared_tree(shared, &listed, &listed);

        // Every directly shared file is still an alias at the root.
        let (outer_inode, _) = filesystem.lookup(shared, b"outer").expect("outer alias");
        let (inner_alias, _) = filesystem.lookup(shared, b"inner").expect("inner alias");
        let (leaf_alias, _) = filesystem.lookup(shared, b"leaf").expect("leaf alias");

        // ...and the full chain is rebuilt beneath the shared ancestors.
        let (inner_inode, _) = filesystem
            .lookup(outer_inode, b"inner")
            .expect("inner under outer");
        let (leaf_inode, _) = filesystem
            .lookup(inner_inode, b"leaf")
            .expect("leaf under outer/inner");
        assert!(filesystem.lookup(inner_alias, b"leaf").is_ok());
        assert_ne!(inner_alias, inner_inode);
        assert_ne!(leaf_alias, leaf_inode);
    }

    #[test]
    fn cyclic_shared_listings_terminate() {
        let mut filesystem = filesystem();
        let shared = filesystem.add_node(
            ROOT_INODE,
            "Shared with me",
            FileType::Directory,
            0,
            None,
            false,
        );
        let mut left = file(FOLDER, false);
        left.id = "left".to_owned();
        left.name = "left".to_owned();
        left.parents = vec!["right".to_owned()];
        let mut right = file(FOLDER, false);
        right.id = "right".to_owned();
        right.name = "right".to_owned();
        right.parents = vec!["left".to_owned()];

        let listed = [left, right];
        filesystem.add_shared_tree(shared, &listed, &listed);

        let (left_inode, _) = filesystem.lookup(shared, b"left").expect("left alias");
        let (right_inode, _) = filesystem.lookup(shared, b"right").expect("right alias");
        assert!(filesystem.lookup(left_inode, b"right").is_ok());
        assert!(filesystem.lookup(right_inode, b"left").is_ok());
    }

    #[test]
    fn folder_shortcuts_preserve_identity_and_duplicate_target_children() {
        let mut filesystem = filesystem();
        let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
        let mut folder = file(FOLDER, false);
        folder.id = "target".to_owned();
        folder.parents = vec!["root-id".to_owned()];
        let mut shortcut = file(SHORTCUT, false);
        shortcut.id = "shortcut".to_owned();
        shortcut.parents = vec!["root-id".to_owned()];
        shortcut.resolved_target = Some(ShortcutTarget {
            id: "target".to_owned(),
            mime_type: FOLDER.to_owned(),
            size: 0,
            capabilities: Capabilities::default(),
            version: "1".to_owned(),
        });
        let mut child = file("text/plain", false);
        child.id = "child".to_owned();
        child.parents = vec!["target".to_owned()];

        // Deliberately list the child before the shortcut: a one-shot attach
        // would leave it missing beneath the shortcut's alias of the target.
        filesystem.add_tree(root, "root-id", &[folder, child, shortcut]);
        let (folder_inode, _) = filesystem.lookup(root, b"name").expect("real folder");
        let (shortcut_inode, _) = filesystem.lookup(root, b"name (2)").expect("shortcut");
        assert!(filesystem.lookup(folder_inode, b"name").is_ok());
        assert!(filesystem.lookup(shortcut_inode, b"name").is_ok());
    }

    #[test]
    fn unsupported_and_download_restricted_files_are_hidden() {
        let mut unsupported = file("application/vnd.google-apps.form", true);
        assert!(!supported_file(&unsupported));
        unsupported.mime_type = "text/plain".to_owned();
        unsupported.capabilities.can_download = false;
        assert!(!supported_file(&unsupported));
    }

    #[test]
    fn writes_buffer_by_remote_id_until_close() {
        let mut filesystem = filesystem();
        let inode = filesystem.add_node(
            ROOT_INODE,
            "file",
            FileType::File,
            3,
            Some(RemoteFile {
                id: "id".to_owned(),
                mime_type: "text/plain".to_owned(),
            }),
            true,
        );
        filesystem
            .cache
            .lock()
            .expect("cache")
            .insert("id".to_owned(), b"abc".to_vec());

        assert_eq!(filesystem.write(inode, 1, b"XY"), Ok(2));
        assert_eq!(
            filesystem.cache.lock().expect("cache").get("id"),
            Some(&b"aXY".to_vec())
        );
        assert!(filesystem.dirty.lock().expect("dirty").contains("id"));

        let cached = filesystem.cache.lock().expect("cache").clone();
        let sizes = filesystem.sizes.lock().expect("sizes").clone();
        let dirty = filesystem.dirty.lock().expect("dirty").clone();
        assert_eq!(
            filesystem.write(inode, MAX_BUFFERED_FILE_SIZE, b"x"),
            Err(Error(27))
        );
        assert_eq!(
            filesystem.set_metadata(
                inode,
                SetMetadata {
                    size: Some(MAX_BUFFERED_FILE_SIZE + 1),
                    ..SetMetadata::default()
                }
            ),
            Err(Error(27))
        );
        assert_eq!(*filesystem.cache.lock().expect("cache"), cached);
        assert_eq!(*filesystem.sizes.lock().expect("sizes"), sizes);
        assert_eq!(*filesystem.dirty.lock().expect("dirty"), dirty);
    }

    #[test]
    fn completed_download_does_not_replace_concurrent_cached_write() {
        let cache = Mutex::new(HashMap::from([(
            "id".to_owned(),
            b"new dirty data".to_vec(),
        )]));
        let selected =
            cache_download(&cache, "id", b"stale download".to_vec()).expect("select cached data");
        assert_eq!(selected, b"new dirty data");
        assert_eq!(
            cache.lock().expect("cache").get("id"),
            Some(&b"new dirty data".to_vec())
        );
    }

    #[test]
    fn stale_remote_versions_are_rejected_and_drive_quota_stays_authoritative() {
        assert!(version_matches("7", "7"));
        assert!(!version_matches("7", "8"));

        let mut filesystem = filesystem();
        filesystem.total_bytes = 2_000;
        filesystem.free_bytes = 1_000;
        filesystem
            .sizes
            .lock()
            .expect("sizes")
            .insert("id".to_owned(), 125);
        let stats = filesystem.statfs().expect("statfs");
        assert_eq!(stats.total_bytes, 2_000);
        assert_eq!(stats.free_bytes, 1_000);
        assert_eq!(stats.available_bytes, 1_000);
    }
}
