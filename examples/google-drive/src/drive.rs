use fsk::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, KernelCachePolicy, Metadata,
    NetworkConfiguration, NetworkFile, NetworkFilesystem, ROOT_INODE, ReadDirectoryResult, Result,
    SetMetadata, StatFs, current_gid, current_uid,
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
const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const FOLDER: &str = "application/vnd.google-apps.folder";
const SHORTCUT: &str = "application/vnd.google-apps.shortcut";
const MAX_BUFFERED_FILE_SIZE: u64 = 256 * 1024 * 1024;
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

// Tests live under `tests/` to keep the example source focused.
#[cfg(test)]
#[path = "../tests/drive/mod.rs"]
mod tests;
