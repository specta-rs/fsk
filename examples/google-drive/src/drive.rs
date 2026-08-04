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
        let mut ids = HashMap::from([(root_id.to_owned(), vec![root_inode])]);
        let mut pending: Vec<_> = files.iter().collect();
        loop {
            let before = pending.len();
            pending.retain(|file| {
                let parents = tree_parents(file, &ids);
                if parents.is_empty() {
                    return true;
                }
                let inodes: Vec<_> = parents
                    .into_iter()
                    .map(|parent| self.add_drive_file(parent, file))
                    .collect();
                register_tree_ids(&mut ids, file, &inodes);
                false
            });
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
    }

    fn add_shared_tree(&mut self, root_inode: u64, roots: &[DriveFile], files: &[DriveFile]) {
        let mut ids: HashMap<String, Vec<u64>> = HashMap::new();
        for file in roots {
            let inode = self.add_drive_file(root_inode, file);
            register_tree_ids(&mut ids, file, &[inode]);
        }
        // Direct shares are root-level aliases, but they can also be children of
        // another shared folder. Process every account file so those aliases are
        // retained at the root and attached beneath each reachable parent.
        let mut pending: Vec<_> = files.iter().collect();
        loop {
            let before = pending.len();
            pending.retain(|file| {
                let parents = tree_parents(file, &ids);
                if parents.is_empty() {
                    return true;
                }
                let inodes: Vec<_> = parents
                    .into_iter()
                    .map(|parent| self.add_drive_file(parent, file))
                    .collect();
                register_tree_ids(&mut ids, file, &inodes);
                false
            });
            if pending.is_empty() || pending.len() == before {
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

fn tree_parents(file: &DriveFile, ids: &HashMap<String, Vec<u64>>) -> Vec<u64> {
    let mut output = Vec::new();
    for parent in file.parents.iter().filter_map(|id| ids.get(id)) {
        for inode in parent {
            if !output.contains(inode) {
                output.push(*inode);
            }
        }
    }
    output
}

fn register_tree_ids(ids: &mut HashMap<String, Vec<u64>>, file: &DriveFile, inodes: &[u64]) {
    let mut keys = vec![file.id.as_str()];
    if let Some(target) = &file.resolved_target {
        keys.push(&target.id);
    }
    for key in keys {
        let values = ids.entry(key.to_owned()).or_default();
        for inode in inodes {
            if !values.contains(inode) {
                values.push(*inode);
            }
        }
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
    while output.ends_with([' ', '.']) {
        output.pop();
    }
    while output.len() > 240 {
        output.pop();
    }
    if output.is_empty() || matches!(output.as_str(), "." | "..") {
        "_".clone_into(&mut output);
    }
    let stem = output.split('.').next().unwrap_or_default();
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if reserved
        .iter()
        .any(|value| stem.eq_ignore_ascii_case(value))
    {
        output.insert(0, '_');
    }
    while output.len() > 240 {
        output.pop();
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
        assert!(portable_name(&format!("CON.{}", "x".repeat(300))).len() <= 240);
        assert!(names_equal("É".as_bytes(), "é".as_bytes()));
        assert!(names_equal("é".as_bytes(), "e\u{301}".as_bytes()));
    }

    #[test]
    fn collision_suffixes_stay_within_portable_name_limit() {
        let mut filesystem = filesystem();
        let long_name = "é".repeat(120);
        let first = filesystem.add_node(ROOT_INODE, &long_name, FileType::File, 0, None, false);
        let second = filesystem.add_node(ROOT_INODE, &long_name, FileType::File, 0, None, false);

        assert!(filesystem.nodes[inode_index(first).unwrap()].name.len() <= 240);
        let second_name = &filesystem.nodes[inode_index(second).unwrap()].name;
        assert!(second_name.len() <= 240);
        assert!(second_name.ends_with(b" (2)"));
        assert!(std::str::from_utf8(second_name).is_ok());
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

        filesystem.add_tree(root, "root-id", &[folder, shortcut, child]);
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
        assert_eq!(stats.free_bytes, 1_000);
        assert_eq!(stats.available_bytes, 1_000);
    }
}
