use fsk::{
    DirectoryEntry, DirectorySink, Error, FileType, Filesystem, KernelCachePolicy, Metadata,
    NetworkConfiguration, NetworkFile, NetworkFilesystem, ROOT_INODE, ReadDirectoryResult, Result,
    StatFs,
};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::{collections::HashMap, io, sync::Mutex};

const API: &str = "https://www.googleapis.com/drive/v3";
const FOLDER: &str = "application/vnd.google-apps.folder";

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
    #[serde(default)]
    size: u64,
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
}

pub struct GoogleDrive {
    client: Client,
    token: String,
    nodes: Vec<Node>,
    children: HashMap<u64, Vec<u64>>,
    cache: Mutex<HashMap<u64, Vec<u8>>>,
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
            }],
            children: HashMap::new(),
            cache: Mutex::new(HashMap::new()),
        };
        let my_drive = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None);
        let shared =
            filesystem.add_node(ROOT_INODE, "Shared with me", FileType::Directory, 0, None);
        let shared_drives =
            filesystem.add_node(ROOT_INODE, "Shared drives", FileType::Directory, 0, None);

        let account_files = filesystem.list_files("user", None, Some("trashed = false"))?;
        filesystem.add_tree(my_drive, "root", &account_files);
        let shared_files = filesystem.list_files(
            "user",
            None,
            Some("sharedWithMe = true and trashed = false"),
        )?;
        for file in shared_files {
            filesystem.add_drive_file(shared, &file);
        }
        for drive in filesystem.list_drives()? {
            let root =
                filesystem.add_node(shared_drives, &drive.name, FileType::Directory, 0, None);
            let files = filesystem.list_files("drive", Some(&drive.id), Some("trashed = false"))?;
            filesystem.add_tree(root, &drive.id, &files);
        }
        filesystem.hydrate_exports()?;
        Ok(filesystem)
    }

    // The Drive API does not report an exported byte length for native Google
    // documents. Materialize those during indexing so getattr can return the
    // exact size; ordinary files remain lazy.
    fn hydrate_exports(&mut self) -> io::Result<()> {
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
            let data = self
                .download(inode, &remote)
                .map_err(|error| io::Error::from_raw_os_error(error.0))?;
            let index = inode_index(inode).expect("allocated inode fits usize");
            self.nodes[index].size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        }
        Ok(())
    }

    fn request(&self, url: &str) -> reqwest::blocking::RequestBuilder {
        self.client.get(url).bearer_auth(&self.token)
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
                    "nextPageToken,files(id,name,mimeType,parents,size)".to_owned(),
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
        let mut ids = HashMap::from([(root_id.to_owned(), root_inode)]);
        let mut pending: Vec<_> = files.iter().collect();
        loop {
            let before = pending.len();
            pending.retain(|file| {
                let Some(parent) = file.parents.iter().find_map(|id| ids.get(id)).copied() else {
                    return true;
                };
                let inode = self.add_drive_file(parent, file);
                ids.insert(file.id.clone(), inode);
                false
            });
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
    }

    fn add_drive_file(&mut self, parent: u64, file: &DriveFile) -> u64 {
        let kind = if file.mime_type == FOLDER {
            FileType::Directory
        } else {
            FileType::File
        };
        let (name, remote) = if kind == FileType::Directory {
            (file.name.clone(), None)
        } else {
            let (suffix, _) = export_format(&file.mime_type);
            (
                format!("{}{}", file.name, suffix),
                Some(RemoteFile {
                    id: file.id.clone(),
                    mime_type: file.mime_type.clone(),
                }),
            )
        };
        self.add_node(parent, &name, kind, file.size, remote)
    }

    fn add_node(
        &mut self,
        parent: u64,
        name: &str,
        kind: FileType,
        size: u64,
        remote: Option<RemoteFile>,
    ) -> u64 {
        let mut display = name.as_bytes().to_vec();
        if self.children.get(&parent).is_some_and(|children| {
            children.iter().any(|inode| {
                let index = inode_index(*inode).expect("allocated inode fits usize");
                self.nodes[index].name == display
            })
        }) {
            display.extend_from_slice(format!(" ({})", self.nodes.len() + 1).as_bytes());
        }
        let inode = u64::try_from(self.nodes.len() + 1).unwrap_or(u64::MAX);
        self.nodes.push(Node {
            parent,
            name: display,
            kind,
            size,
            remote,
        });
        self.children.entry(parent).or_default().push(inode);
        inode
    }

    fn download(&self, inode: u64, remote: &RemoteFile) -> Result<Vec<u8>> {
        if let Some(data) = self.cache.lock().map_err(|_| Error(5))?.get(&inode) {
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
        let data = request
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::bytes)
            .map_err(|_| Error(5))?
            .to_vec();
        self.cache
            .lock()
            .map_err(|_| Error(5))?
            .insert(inode, data.clone());
        Ok(data)
    }
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
        let used = self.nodes.iter().map(|node| node.size).sum();
        Ok(StatFs {
            block_size: 4096,
            io_size: 131_072,
            total_bytes: used,
            free_bytes: 0,
            available_bytes: 0,
            files: self.nodes.len() as u64,
            free_files: 0,
        })
    }

    fn metadata(&self, inode: u64) -> Result<Metadata> {
        let node = inode_index(inode)
            .and_then(|index| self.nodes.get(index))
            .ok_or(Error(2))?;
        Ok(Metadata {
            inode,
            parent: node.parent,
            size: node.size,
            allocated_size: 0,
            generation: 1,
            created_ns: 0,
            modified_ns: 0,
            accessed_ns: 0,
            mode: if node.kind == FileType::Directory {
                0o555
            } else {
                0o444
            },
            uid: 0,
            gid: 0,
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
        let data = self.download(inode, remote)?;
        let start = usize::try_from(offset).map_err(|_| Error::INVALID)?;
        let Some(input) = data.get(start..) else {
            return Ok(0);
        };
        let count = input.len().min(output.len());
        output[..count].copy_from_slice(&input[..count]);
        Ok(count)
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
    fn open_file(&self, _inode: u64, writable: bool) -> Result<NetworkFile> {
        if writable {
            Err(Error(30))
        } else {
            Ok(NetworkFile::Remote)
        }
    }
}
