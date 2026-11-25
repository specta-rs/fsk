use fsk::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, KernelCachePolicy, Metadata,
    NetworkConfiguration, NetworkFile, NetworkFilesystem, ROOT_INODE, ReadDirectoryResult, Result,
    SetMetadata, StatFs, current_gid, current_uid, now_ns,
};
use reqwest::blocking::Client;
use serde::{Deserialize, Deserializer, de::Error as _};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Read},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use unicode_normalization::UnicodeNormalization;

const API: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_API: &str = "https://www.googleapis.com/upload/drive/v3";
const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const FOLDER: &str = "application/vnd.google-apps.folder";
const SHORTCUT: &str = "application/vnd.google-apps.shortcut";
const MAX_BUFFERED_FILE_SIZE: u64 = 256 * 1024 * 1024;
// A continuously changing remote must not keep a read or write retrying forever.
const MAX_DOWNLOAD_RETRIES: usize = 3;
// Folder shortcuts and multi-parent folders turn the Drive listing into a DAG
// whose aliases can multiply exponentially. Bound both the depth of a single
// alias path and the total number of aliases one walk may create; realistic
// trees stay well below these limits, while pathological shortcut graphs stop
// quickly instead of exhausting memory. These bounds only apply to repeated
// mounts of a listing entry (aliases); the first mount of every listing entry
// is always created. A shortcut stores both its own id and its target's id on
// the path, so allow enough room for a legitimate deep tree to also carry
// aliases.
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
struct AccessToken {
    access_token: String,
}

/// Exchange an OAuth refresh token for an hour-long access token.
///
/// Interactive mounts can use `GOOGLE_DRIVE_ACCESS_TOKEN` directly, but CI
/// cannot keep an expiring token in a secret. Storing the refresh token and the
/// OAuth client credentials lets each run mint a fresh access token instead.
pub fn refresh_access_token(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> io::Result<String> {
    let response = Client::builder()
        .user_agent("fsk-google-drive-example")
        .build()
        .map_err(io::Error::other)?
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .map_err(io::Error::other)?;
    let status = response.status();
    let body = response.text().map_err(io::Error::other)?;
    if !status.is_success() {
        return Err(io::Error::other(format!(
            "OAuth token exchange failed with {status}: {body}"
        )));
    }
    let token: AccessToken = serde_json::from_str(&body).map_err(io::Error::other)?;
    Ok(token.access_token)
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedFile {
    // A version is stored only when metadata fetched before the content request
    // (or an ETag match) verifies that it describes these bytes.
    version: Option<String>,
    etag: Option<String>,
    data: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FileCache {
    entries: HashMap<String, CachedFile>,
    // Listing versions are upload fallbacks only; they never identify cached bytes.
    versions: HashMap<String, String>,
    // Invalidations and uploads advance the epoch so in-flight responses can
    // recognize that their bytes are no longer eligible to install.
    epochs: HashMap<String, u64>,
    // An invalidated epoch waits for matching bytes before mtime advances.
    changed_epochs: HashMap<String, u64>,
}

impl FileCache {
    fn epoch(&self, id: &str) -> u64 {
        self.epochs.get(id).copied().unwrap_or(0)
    }

    fn invalidate(&mut self, id: &str) -> u64 {
        self.entries.remove(id);
        let epoch = self.epochs.entry(id.to_owned()).or_default();
        *epoch += 1;
        let epoch = *epoch;
        self.changed_epochs.insert(id.to_owned(), epoch);
        epoch
    }

    fn install_uploaded(&mut self, id: &str, version: String, data: Vec<u8>) {
        let epoch = self.epochs.entry(id.to_owned()).or_default();
        *epoch += 1;
        self.entries.insert(
            id.to_owned(),
            CachedFile {
                version: Some(version.clone()),
                etag: None,
                data,
            },
        );
        self.versions.insert(id.to_owned(), version);
        self.changed_epochs.remove(id);
    }

    fn install_local(&mut self, id: &str, data: Vec<u8>) {
        let version = self.entries.get(id).and_then(|entry| entry.version.clone());
        self.entries.insert(
            id.to_owned(),
            CachedFile {
                version,
                etag: None,
                data,
            },
        );
    }
}

enum DownloadPlan {
    Retry,
    Cached(Vec<u8>),
    Fetch {
        epoch: u64,
        known_metadata: Option<RemoteMetadata>,
    },
}

struct DownloadedFile {
    data: Vec<u8>,
    etag: Option<String>,
}

#[derive(Clone)]
struct RemoteMetadata {
    version: String,
    etag: Option<String>,
}

impl CachedFile {
    fn matches(&self, current: &RemoteMetadata) -> bool {
        match (&self.etag, &current.etag) {
            (Some(cached), Some(current)) => etag_token(cached)
                .zip(etag_token(current))
                .is_some_and(|(cached, current)| cached == current),
            _ => self.version.as_deref() == Some(current.version.as_str()),
        }
    }

    fn same_identity(&self, other: &Self) -> bool {
        match (&self.etag, &other.etag) {
            (Some(left), Some(right)) => etag_token(left)
                .zip(etag_token(right))
                .is_some_and(|(left, right)| left == right),
            _ => self.version.is_some() && self.version == other.version,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CacheDownload {
    Retry,
    Existing(Vec<u8>),
    Installed { data: Vec<u8>, changed: bool },
}

struct Node {
    parent: u64,
    name: Vec<u8>,
    kind: FileType,
    size: u64,
    remote: Option<RemoteFile>,
    writable: bool,
    generation: AtomicU64,
    modified_ns: AtomicU64,
}

pub struct GoogleDrive {
    client: Client,
    token: String,
    api_url: String,
    upload_url: String,
    nodes: Vec<Node>,
    children: HashMap<u64, Vec<u64>>,
    cache: Mutex<FileCache>,
    sizes: Mutex<HashMap<String, u64>>,
    writes: Mutex<()>,
    dirty: Mutex<HashSet<String>>,
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
            api_url: API.to_owned(),
            upload_url: UPLOAD_API.to_owned(),
            nodes: vec![Node {
                parent: ROOT_INODE,
                name: Vec::new(),
                kind: FileType::Directory,
                size: 0,
                remote: None,
                writable: false,
                generation: AtomicU64::new(1),
                modified_ns: AtomicU64::new(0),
            }],
            children: HashMap::new(),
            cache: Mutex::new(FileCache::default()),
            sizes: Mutex::new(HashMap::new()),
            writes: Mutex::new(()),
            dirty: Mutex::new(HashSet::new()),
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
        // Aliases share one remote id, so download each export once during
        // indexing instead of multiplying requests across the mounted tree.
        let mut exports: HashMap<String, (RemoteFile, Vec<u64>)> = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            let Some(remote) = node.remote.as_ref() else {
                continue;
            };
            if export_format(&remote.mime_type).1.is_none() {
                continue;
            }
            let Ok(inode) = u64::try_from(index + 1) else {
                continue;
            };
            exports
                .entry(remote.id.clone())
                .or_insert_with(|| (remote.clone(), Vec::new()))
                .1
                .push(inode);
        }
        let mut exports: Vec<_> = exports.into_values().collect();
        exports.sort_by(|left, right| left.0.id.cmp(&right.0.id));
        for (remote, inodes) in exports {
            match self.download(&remote) {
                Ok(data) => {
                    let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
                    for inode in inodes {
                        let index = inode_index(inode).expect("allocated inode fits usize");
                        self.nodes[index].size = size;
                    }
                }
                Err(error) => {
                    // Keep the entry so a transient export failure does not
                    // silently remove a document from the mounted namespace. A
                    // later read retries the export and refreshes its size.
                    eprintln!(
                        "drive: could not hydrate export {} for inodes {inodes:?}: {error:?}",
                        remote.id
                    );
                }
            }
        }
    }

    fn request(&self, url: &str) -> reqwest::blocking::RequestBuilder {
        self.client.get(url).bearer_auth(&self.token)
    }

    fn load_quota(&mut self) -> io::Result<()> {
        let about: About = self
            .request(&format!("{}/about", self.api_url))
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
            .request(&format!("{}/files/root", self.api_url))
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
        self.request(&format!("{}/files/{id}", self.api_url))
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
                .request(&format!("{}/files", self.api_url))
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
                .request(&format!("{}/drives", self.api_url))
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
        // Nothing is mounted yet, so every listed file is a first mount.
        let mut mounted = HashSet::new();
        self.expand_tree_aliases(&mut ids, &mut ancestry, files, &mut mounted);
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
        // Direct shares are already mounted, so the walk must treat every later
        // inode for one of these listing ids as an alias.
        let mut mounted: HashSet<String> = HashSet::new();
        for file in roots {
            if !seen_roots.insert(file.id.as_str()) {
                continue;
            }
            let inode = self.add_drive_file(root_inode, file);
            register_tree_ids(&mut ids, file, &[inode]);
            ancestry.insert(inode, tree_ids(file));
            mounted.insert(file.id.clone());
        }
        self.expand_tree_aliases(&mut ids, &mut ancestry, files, &mut mounted);
    }

    // Attach every listed file beneath every known alias of its parents,
    // repeating until a pass adds nothing new so the result does not depend on
    // the order Drive listed the files in. Direct shares are also seeded as
    // aliases here (rather than attached once and forgotten) so a child listed
    // before a second alias of its parent is still replayed under that alias.
    //
    // Aliasing is decided per listing id, not per target id: a mount is an alias
    // once that listing entry already has an inode (`mounted`). A shortcut
    // therefore does not make its target look like an alias, and distinct
    // shortcuts that point at the same target are each their own first mount.
    // `ids` still maps target ids to inodes for parent lookup, and `ancestry`
    // records both the listing id and the target id so a cycle on either is
    // detected.
    //
    // Termination: every successful mount inserts its (listing id, parent)
    // pair into `seen`, so no pair is ever mounted twice, while `ancestry`
    // refuses to revisit a remote id already on the path. Each listing id can
    // be first-mounted at most once and every further mount is an alias, so the
    // walk creates at most one inode per listing entry plus `MAX_ALIAS_NODES`
    // aliases. Since a pass that adds nothing ends the loop, the walk terminates.
    //
    // The caps apply only to aliases: the first mount of every listing entry is
    // never counted or truncated.
    fn expand_tree_aliases(
        &mut self,
        ids: &mut HashMap<String, HashSet<u64>>,
        ancestry: &mut HashMap<u64, Vec<String>>,
        files: &[DriveFile],
        mounted: &mut HashSet<String>,
    ) {
        self.expand_tree_aliases_with(
            ids,
            ancestry,
            files,
            mounted,
            MAX_ALIAS_NODES,
            MAX_ALIAS_DEPTH,
        );
    }

    /// Returns `(nodes created, seen pairs)` so tests can check the walk stays
    /// proportional to the tree it builds instead of the alias product.
    fn expand_tree_aliases_with(
        &mut self,
        ids: &mut HashMap<String, HashSet<u64>>,
        ancestry: &mut HashMap<u64, Vec<String>>,
        files: &[DriveFile],
        mounted: &mut HashSet<String>,
        max_nodes: usize,
        max_depth: usize,
    ) -> (usize, usize) {
        let start_nodes = self.nodes.len();
        let mut seen: HashSet<(&str, u64)> = HashSet::new();
        let mut aliases = 0_usize;
        let mut node_cap_reached = false;
        let mut depth_cap_reached = false;
        loop {
            let mut added = false;
            for file in files {
                // Once the cap is reached, every remaining mount of an already
                // mounted listing entry would be a rejected alias. Skip the
                // file before building its parent list so the `seen` set cannot
                // grow with (files x aliases) rejected pairs.
                if node_cap_reached && mounted.contains(&file.id) {
                    continue;
                }
                for parent in tree_parents(file, ids) {
                    let alias = mounted.contains(&file.id);
                    if alias && (node_cap_reached || aliases >= max_nodes) {
                        if !node_cap_reached {
                            eprintln!(
                                "drive: stopping alias expansion after {max_nodes} aliases; \
                                 some duplicate aliases may be omitted"
                            );
                            node_cap_reached = true;
                        }
                        // The file is already mounted and every remaining
                        // candidate is an alias, so the cap rejects them all.
                        // Stop without recording the pairs.
                        break;
                    }
                    let Some(parent_ancestry) = ancestry.get(&parent) else {
                        continue;
                    };
                    let file_ids = tree_ids(file);
                    // A remote id may appear at most once on a path; seeing one
                    // again means the listing contains a cycle, so do not recurse.
                    // The first mount is still created: a shortcut may target its
                    // own parent, and #7 mounted that shortcut once under the
                    // parent. Only re-mounts under an alias are refused.
                    // Record the rejected pair so later passes do not retry it.
                    if alias && file_ids.iter().any(|id| parent_ancestry.contains(id)) {
                        seen.insert((file.id.as_str(), parent));
                        continue;
                    }
                    // Do not descend past the configured alias depth.
                    if alias && parent_ancestry.len() >= max_depth {
                        if !depth_cap_reached {
                            eprintln!(
                                "drive: stopping alias expansion at depth {max_depth}; \
                                 some duplicate aliases may be omitted"
                            );
                            depth_cap_reached = true;
                        }
                        seen.insert((file.id.as_str(), parent));
                        continue;
                    }
                    if !seen.insert((file.id.as_str(), parent)) {
                        continue;
                    }
                    let mut chain = parent_ancestry.clone();
                    chain.extend(file_ids.iter().cloned());
                    let inode = self.add_drive_file(parent, file);
                    ancestry.insert(inode, chain);
                    register_tree_ids(ids, file, &[inode]);
                    mounted.insert(file.id.clone());
                    if alias {
                        aliases += 1;
                        if aliases >= max_nodes && !node_cap_reached {
                            eprintln!(
                                "drive: stopping alias expansion after {max_nodes} aliases; \
                                 some duplicate aliases may be omitted"
                            );
                            node_cap_reached = true;
                        }
                    }
                    added = true;
                    if !alias && node_cap_reached {
                        // The cap is already reached, so this listing entry only
                        // needs its one first mount. Further parents would only
                        // produce rejected aliases.
                        break;
                    }
                }
            }
            if !added {
                break;
            }
        }
        (self.nodes.len() - start_nodes, seen.len())
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
            self.cache
                .get_mut()
                .expect("cache mutex is not poisoned during indexing")
                .versions
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
            generation: AtomicU64::new(1),
            modified_ns: AtomicU64::new(0),
        });
        self.children.entry(parent).or_default().push(inode);
        inode
    }

    fn download(&self, remote: &RemoteFile) -> Result<Vec<u8>> {
        if self.is_dirty(&remote.id)? {
            return self.cached_download(&remote.id);
        }

        download_with_retries(|| {
            // Revalidation and invalidation must exclude writes. Otherwise a
            // clean read could remove a buffer that a concurrent write made
            // dirty. Content GETs stay outside this lock so downloads can run
            // concurrently.
            let plan = {
                let _write_guard = self.writes.lock().map_err(|_| Error::IO)?;
                if self.is_dirty(&remote.id)? {
                    DownloadPlan::Cached(self.cached_download(&remote.id)?)
                } else {
                    self.revalidate_cached_download(remote)?
                }
            };
            if let Some(data) = self.finish_download(remote, plan)? {
                return Ok(Some(data));
            }
            Ok(None)
        })
    }

    // Mutations already hold `writes`; revalidate without taking it again.
    fn download_for_write(&self, remote: &RemoteFile) -> Result<Vec<u8>> {
        download_with_retries(|| {
            let plan = if self.is_dirty(&remote.id)? {
                DownloadPlan::Cached(self.cached_download(&remote.id)?)
            } else {
                self.revalidate_cached_download(remote)?
            };
            if let Some(data) = self.finish_download(remote, plan)? {
                return Ok(Some(data));
            }
            Ok(None)
        })
    }

    fn is_dirty(&self, id: &str) -> Result<bool> {
        Ok(self.dirty.lock().map_err(|_| Error::IO)?.contains(id))
    }

    fn cached_download(&self, id: &str) -> Result<Vec<u8>> {
        self.cache
            .lock()
            .map_err(|_| Error::IO)?
            .entries
            .get(id)
            .map(|entry| entry.data.clone())
            .ok_or(Error::IO)
    }

    fn revalidate_cached_download(&self, remote: &RemoteFile) -> Result<DownloadPlan> {
        let cached = {
            let cache = self.cache.lock().map_err(|_| Error::IO)?;
            (
                cache.entries.get(&remote.id).cloned(),
                cache.epoch(&remote.id),
            )
        };
        let (cached, epoch) = cached;
        let Some(cached) = cached else {
            return Ok(DownloadPlan::Fetch {
                epoch,
                known_metadata: None,
            });
        };

        // A remote id is immutable, but its bytes are not. Compare the server
        // version with the version that produced the cached bytes.
        let current = self.remote_metadata(&remote.id)?;
        if cached.matches(&current) {
            let mut cache = self.cache.lock().map_err(|_| Error::IO)?;
            let unchanged = cache.epoch(&remote.id) == epoch
                && cache.entries.get(&remote.id).is_some_and(|entry| {
                    entry.version == cached.version && entry.etag == cached.etag
                });
            if !unchanged {
                return Ok(DownloadPlan::Retry);
            }
            let matched_by_etag = cached.etag.is_some() && current.etag.is_some();
            if matched_by_etag {
                let verified_version = Some(current.version.clone());
                if verified_version != cached.version {
                    if let Some(entry) = cache.entries.get_mut(&remote.id) {
                        entry.version.clone_from(&verified_version);
                    }
                    cache
                        .versions
                        .insert(remote.id.clone(), current.version.clone());
                }
            }
            return Ok(DownloadPlan::Cached(cached.data));
        }

        let mut cache = self.cache.lock().map_err(|_| Error::IO)?;
        if cache.epoch(&remote.id) != epoch
            || cache
                .entries
                .get(&remote.id)
                .is_none_or(|entry| !entry.same_identity(&cached))
        {
            return Ok(DownloadPlan::Retry);
        }
        let epoch = cache.invalidate(&remote.id);
        drop(cache);
        self.sizes.lock().map_err(|_| Error::IO)?.remove(&remote.id);
        Ok(DownloadPlan::Fetch {
            epoch,
            known_metadata: Some(current),
        })
    }

    fn finish_download(&self, remote: &RemoteFile, plan: DownloadPlan) -> Result<Option<Vec<u8>>> {
        match plan {
            DownloadPlan::Retry => Ok(None),
            DownloadPlan::Cached(data) => Ok(Some(data)),
            DownloadPlan::Fetch {
                epoch,
                known_metadata,
            } => {
                let Some(cached) = self.fetch_cached_file(remote, known_metadata)? else {
                    return Ok(None);
                };
                match cache_download(&self.cache, &self.dirty, &remote.id, cached, epoch)? {
                    CacheDownload::Retry => Ok(None),
                    CacheDownload::Existing(data) => Ok(Some(data)),
                    CacheDownload::Installed { data, changed } => {
                        if changed {
                            self.note_content_change(&remote.id);
                        }
                        Ok(Some(data))
                    }
                }
            }
        }
    }

    fn fetch_cached_file(
        &self,
        remote: &RemoteFile,
        known_metadata: Option<RemoteMetadata>,
    ) -> Result<Option<CachedFile>> {
        if let Some(metadata) = known_metadata {
            let downloaded = self.download_content(remote, false)?.ok_or(Error::IO)?;
            return Ok(bind_pre_content(metadata, downloaded));
        }

        // Probe the content response first. An ETag identifies these exact
        // bytes without another API call. If it is absent, discard the body and
        // fetch metadata before the content request whose bytes we will cache.
        if let Some(downloaded) = self.download_content(remote, true)? {
            return Ok(Some(CachedFile {
                version: None,
                etag: downloaded.etag,
                data: downloaded.data,
            }));
        }
        let metadata = self.remote_metadata(&remote.id)?;
        let downloaded = self.download_content(remote, false)?.ok_or(Error::IO)?;
        Ok(bind_pre_content(metadata, downloaded))
    }

    fn download_content(
        &self,
        remote: &RemoteFile,
        require_etag: bool,
    ) -> Result<Option<DownloadedFile>> {
        let (_, export_mime) = export_format(&remote.mime_type);
        let mut request = if let Some(mime) = export_mime {
            self.request(&format!("{}/files/{}/export", self.api_url, remote.id))
                .query(&[("mimeType", mime)])
        } else {
            self.request(&format!("{}/files/{}", self.api_url, remote.id))
                .query(&[("alt", "media"), ("supportsAllDrives", "true")])
        };
        request = request.header(reqwest::header::ACCEPT, "*/*");
        let response = request.send().map_err(|error| {
            eprintln!("drive: content request for {} failed: {error}", remote.id);
            Error::IO
        })?;
        let status = response.status();
        if !status.is_success() {
            eprintln!("drive: content request for {} returned {status}", remote.id);
            return Err(Error::IO);
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .and_then(etag_token);
        if require_etag && etag.is_none() {
            return Ok(None);
        }
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
        Ok(Some(DownloadedFile { data, etag }))
    }

    fn note_content_change(&self, remote_id: &str) {
        let modified_ns = now_ns();
        for node in &self.nodes {
            if node
                .remote
                .as_ref()
                .is_some_and(|remote| remote.id == remote_id)
            {
                node.generation.fetch_add(1, Ordering::Relaxed);
                node.modified_ns.store(modified_ns, Ordering::Relaxed);
            }
        }
    }

    fn record_cached_size(&self, id: &str, size: usize) -> Result<()> {
        let _write = self.writes.lock().map_err(|_| Error::IO)?;
        if self.is_dirty(id)? {
            return Ok(());
        }
        let matches = self
            .cache
            .lock()
            .map_err(|_| Error::IO)?
            .entries
            .get(id)
            .is_some_and(|entry| entry.data.len() == size);
        if matches {
            self.sizes
                .lock()
                .map_err(|_| Error::IO)?
                .insert(id.to_owned(), u64::try_from(size).unwrap_or(u64::MAX));
        }
        Ok(())
    }

    fn upload(&self, remote: &RemoteFile, data: &[u8]) -> Result<()> {
        if u64::try_from(data.len()).unwrap_or(u64::MAX) > MAX_BUFFERED_FILE_SIZE {
            return Err(Error(27));
        }
        let current = self.remote_metadata(&remote.id)?;
        let etag = current.etag.ok_or(Error::IO)?;
        let expected_version = {
            let cache = self.cache.lock().map_err(|_| Error::IO)?;
            cache
                .entries
                .get(&remote.id)
                .and_then(|entry| entry.version.clone())
                .or_else(|| cache.versions.get(&remote.id).cloned())
                .unwrap_or_default()
        };
        if !version_matches(&expected_version, &current.version) {
            return Err(Error::STALE);
        }
        let response = self
            .client
            .patch(format!("{}/files/{}", self.upload_url, remote.id))
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
        self.cache.lock().map_err(|_| Error::IO)?.install_uploaded(
            &remote.id,
            updated.version,
            data.to_vec(),
        );
        Ok(())
    }

    fn remote_metadata(&self, id: &str) -> Result<RemoteMetadata> {
        let response = self
            .request(&format!("{}/files/{id}", self.api_url))
            .query(&[("fields", "version"), ("supportsAllDrives", "true")])
            .send()
            .map_err(|error| {
                eprintln!("drive: metadata request for {id} failed: {error}");
                Error::IO
            })?;
        let status = response.status();
        if !status.is_success() {
            eprintln!("drive: metadata request for {id} returned {status}");
            return Err(Error::IO);
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .and_then(etag_token);
        let version: FileVersion = response.json().map_err(|error| {
            eprintln!("drive: metadata response for {id} was invalid: {error}");
            Error::IO
        })?;
        Ok(RemoteMetadata {
            version: version.version,
            etag,
        })
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
            .entries
            .get(&remote.id)
            .map(|entry| entry.data.clone())
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
    // `ids` stores inodes in hash sets, so sort the candidates to make cap and
    // depth truncation deterministic across runs.
    output.sort_unstable();
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

fn download_with_retries<F>(mut attempt: F) -> Result<Vec<u8>>
where
    F: FnMut() -> Result<Option<Vec<u8>>>,
{
    for _ in 0..=MAX_DOWNLOAD_RETRIES {
        if let Some(data) = attempt()? {
            return Ok(data);
        }
    }
    Err(Error::STALE)
}

fn cache_download(
    cache: &Mutex<FileCache>,
    dirty: &Mutex<HashSet<String>>,
    id: &str,
    cached: CachedFile,
    epoch: u64,
) -> Result<CacheDownload> {
    let mut cache = cache.lock().map_err(|_| Error::IO)?;
    if dirty.lock().map_err(|_| Error::IO)?.contains(id) {
        return Ok(CacheDownload::Existing(
            cache
                .entries
                .get(id)
                .map(|entry| entry.data.clone())
                .ok_or(Error::IO)?,
        ));
    }
    if cache.epoch(id) != epoch {
        return Ok(CacheDownload::Retry);
    }
    if let Some(current) = cache.entries.get(id) {
        return if current.same_identity(&cached) {
            Ok(CacheDownload::Existing(current.data.clone()))
        } else {
            Ok(CacheDownload::Retry)
        };
    }
    cache.entries.insert(id.to_owned(), cached.clone());
    // The version and bytes become visible under the same lock. An old
    // response can never publish bytes under a newer version.
    if let Some(version) = &cached.version {
        cache.versions.insert(id.to_owned(), version.clone());
    }
    let changed = cache.changed_epochs.get(id) == Some(&epoch);
    if changed {
        cache.changed_epochs.remove(id);
    }
    Ok(CacheDownload::Installed {
        data: cached.data,
        changed,
    })
}

fn bind_pre_content(metadata: RemoteMetadata, downloaded: DownloadedFile) -> Option<CachedFile> {
    let version = match (&metadata.etag, &downloaded.etag) {
        (Some(pre_content), Some(content)) if etag_token(pre_content) != etag_token(content) => {
            return None;
        }
        (Some(_), Some(_)) | (_, None) => Some(metadata.version),
        (None, Some(_)) => None,
    };
    Some(CachedFile {
        version,
        etag: downloaded.etag.or(metadata.etag),
        data: downloaded.data,
    })
}

fn etag_token(value: &str) -> Option<String> {
    let value = value.trim();
    let value = value.strip_prefix("W/").unwrap_or(value).trim();
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
        .trim();
    (!value.is_empty()).then(|| value.to_owned())
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
            generation: node.generation.load(Ordering::Relaxed),
            created_ns: 0,
            modified_ns: node.modified_ns.load(Ordering::Relaxed),
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
        let parent_node = inode_index(parent)
            .and_then(|index| self.nodes.get(index))
            .ok_or(Error(2))?;
        if parent_node.kind != FileType::Directory {
            return Err(Error(20));
        }
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
        self.record_cached_size(&remote.id, data.len())?;
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
        if input.is_empty() {
            return Ok(0);
        }
        let start = usize::try_from(offset).map_err(|_| Error::INVALID)?;
        let end = start.checked_add(input.len()).ok_or(Error::INVALID)?;
        if u64::try_from(end).unwrap_or(u64::MAX) > MAX_BUFFERED_FILE_SIZE {
            return Err(Error(27));
        }
        let mut data = self.download_for_write(remote)?;
        if data.len() < end {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(input);
        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        self.cache
            .lock()
            .map_err(|_| Error::IO)?
            .install_local(&remote.id, data);
        self.sizes
            .lock()
            .map_err(|_| Error::IO)?
            .insert(remote.id.clone(), size);
        self.dirty
            .lock()
            .map_err(|_| Error::IO)?
            .insert(remote.id.clone());
        self.note_content_change(&remote.id);
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
        let mut data = self.download_for_write(remote)?;
        data.resize(usize::try_from(size).map_err(|_| Error::INVALID)?, 0);
        self.upload(remote, &data)?;
        self.sizes
            .lock()
            .map_err(|_| Error::IO)?
            .insert(remote.id.clone(), size);
        self.dirty.lock().map_err(|_| Error::IO)?.remove(&remote.id);
        self.note_content_change(&remote.id);
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
            kernel_cache_policy: KernelCachePolicy::MetadataOnly,
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

// Tests live under `tests/` to keep the example source focused.
#[cfg(test)]
#[path = "../tests/drive/mod.rs"]
mod tests;
