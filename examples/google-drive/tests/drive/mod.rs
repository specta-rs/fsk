use super::*;
use std::{
    io::{BufRead as _, BufReader, Write as _},
    net::{TcpListener, TcpStream},
    sync::{Arc, atomic::AtomicBool, mpsc},
    thread,
    time::Duration,
};

fn filesystem() -> GoogleDrive {
    GoogleDrive {
        client: Client::new(),
        token: String::new(),
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
        total_bytes: 0,
        free_bytes: 0,
    }
}

fn add_tree_with_limits(
    filesystem: &mut GoogleDrive,
    root_inode: u64,
    root_id: &str,
    files: &[DriveFile],
    max_nodes: usize,
    max_depth: usize,
) {
    let mut ids = HashMap::from([(root_id.to_owned(), HashSet::from([root_inode]))]);
    let mut ancestry = HashMap::from([(root_inode, Vec::new())]);
    let mut mounted = HashSet::new();
    filesystem.expand_tree_aliases_with(
        &mut ids,
        &mut ancestry,
        files,
        &mut mounted,
        max_nodes,
        max_depth,
    );
}

fn add_tree_with_stats(
    filesystem: &mut GoogleDrive,
    root_inode: u64,
    root_id: &str,
    files: &[DriveFile],
    max_nodes: usize,
    max_depth: usize,
) -> (usize, usize) {
    let mut ids = HashMap::from([(root_id.to_owned(), HashSet::from([root_inode]))]);
    let mut ancestry = HashMap::from([(root_inode, Vec::new())]);
    let mut mounted = HashSet::new();
    filesystem.expand_tree_aliases_with(
        &mut ids,
        &mut ancestry,
        files,
        &mut mounted,
        max_nodes,
        max_depth,
    )
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

fn offline_client() -> Client {
    Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::all("http://127.0.0.1:9").expect("offline proxy"))
        .timeout(std::time::Duration::from_millis(100))
        .build()
        .expect("offline client")
}

fn seed_cache(filesystem: &GoogleDrive, id: &str, version: &str, data: &[u8]) {
    let mut cache = filesystem.cache.lock().expect("cache");
    cache.entries.insert(
        id.to_owned(),
        CachedFile {
            version: version.to_owned(),
            data: data.to_vec(),
        },
    );
    cache.versions.insert(id.to_owned(), version.to_owned());
}

fn cached_bytes(cache: &FileCache, id: &str) -> Option<Vec<u8>> {
    cache.entries.get(id).map(|entry| entry.data.clone())
}

fn cached_version<'a>(cache: &'a FileCache, id: &str) -> Option<&'a str> {
    cache.entries.get(id).map(|entry| entry.version.as_str())
}

struct FakeRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl FakeRequest {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    fn query(&self, name: &str) -> Option<String> {
        self.target.split_once('?')?.1.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
    }
}

struct FakeResponse {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl FakeResponse {
    fn json(body: String) -> Self {
        Self {
            status: "200 OK",
            headers: vec![("Content-Type", "application/json".to_owned())],
            body: body.into_bytes(),
        }
    }

    fn bytes(body: &[u8]) -> Self {
        Self {
            status: "200 OK",
            headers: vec![("Content-Type", "text/plain".to_owned())],
            body: body.to_vec(),
        }
    }

    fn with_header(mut self, name: &'static str, value: String) -> Self {
        self.headers.push((name, value));
        self
    }
}

struct FakeServer {
    base_url: String,
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeServer {
    fn new<F>(handler: F) -> Self
    where
        F: Fn(FakeRequest) -> FakeResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Drive server");
        let address = listener.local_addr().expect("fake Drive server address");
        listener
            .set_nonblocking(true)
            .expect("make fake Drive listener nonblocking");
        let handler = Arc::new(handler);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let handler = Arc::clone(&handler);
                        thread::spawn(move || {
                            let mut stream = stream;
                            stream
                                .set_nonblocking(false)
                                .expect("make fake Drive connection blocking");
                            if let Ok(request) = read_fake_request(&mut stream) {
                                let response = handler(request);
                                let _ = write_fake_response(&mut stream, &response);
                            }
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::park_timeout(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url: format!("http://{address}"),
            address,
            stop,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_fake_request(stream: &mut TcpStream) -> io::Result<FakeRequest> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    Ok(FakeRequest {
        method,
        target,
        headers,
        body,
    })
}

fn write_fake_response(stream: &mut TcpStream, response: &FakeResponse) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    )?;
    for (name, value) in &response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n")?;
    stream.write_all(&response.body)
}

struct FakeDriveState {
    version: String,
    advance_version_on_metadata: bool,
    metadata_gets: usize,
    content_gets: HashMap<String, usize>,
    upload_starts: usize,
    uploads: Vec<Vec<u8>>,
}

struct FakeDriveServer {
    server: FakeServer,
    state: Arc<Mutex<FakeDriveState>>,
    old_started: mpsc::Receiver<()>,
    release_old: mpsc::Sender<()>,
}

impl FakeDriveServer {
    fn new(block_old_response: bool) -> Self {
        let state = Arc::new(Mutex::new(FakeDriveState {
            version: "1".to_owned(),
            advance_version_on_metadata: false,
            metadata_gets: 0,
            content_gets: HashMap::new(),
            upload_starts: 0,
            uploads: Vec::new(),
        }));
        let (old_started_tx, old_started_rx) = mpsc::channel();
        let (release_old_tx, release_old_rx) = mpsc::channel();
        let release_old = Arc::new(Mutex::new(release_old_rx));
        let handler_state = Arc::clone(&state);
        let handler = move |request: FakeRequest| {
            if request.path() == "/drive/v3/files/id" && request.query("alt").is_none() {
                let mut state = handler_state.lock().expect("fake Drive state");
                state.metadata_gets += 1;
                if state.advance_version_on_metadata {
                    let version = state
                        .version
                        .parse::<u64>()
                        .expect("fake Drive version is numeric")
                        + 1;
                    state.version = version.to_string();
                }
                let version = state.version.clone();
                return FakeResponse::json(serde_json::json!({ "version": version }).to_string())
                    .with_header("ETag", format!("\"{version}\""));
            }
            if request.path() == "/drive/v3/files/id"
                && request.query("alt").as_deref() == Some("media")
            {
                let (version, count) = {
                    let mut state = handler_state.lock().expect("fake Drive state");
                    let version = state.version.clone();
                    let count = state.content_gets.entry(version.clone()).or_default();
                    *count += 1;
                    (version, *count)
                };
                if block_old_response && version == "1" && count == 1 {
                    let _ = old_started_tx.send(());
                    let _ = release_old
                        .lock()
                        .expect("old response release")
                        .recv_timeout(Duration::from_secs(10));
                }
                let body = format!("v{version}");
                return FakeResponse::bytes(body.as_bytes());
            }
            if request.method == "PATCH" && request.path() == "/upload/drive/v3/files/id" {
                handler_state
                    .lock()
                    .expect("fake Drive state")
                    .upload_starts += 1;
                let host = request
                    .headers
                    .get("host")
                    .cloned()
                    .unwrap_or_else(|| "127.0.0.1".to_owned());
                return FakeResponse {
                    status: "200 OK",
                    headers: vec![],
                    body: Vec::new(),
                }
                .with_header("Location", format!("http://{host}/upload/drive/v3/session"));
            }
            if request.method == "PUT" && request.path() == "/upload/drive/v3/session" {
                let mut state = handler_state.lock().expect("fake Drive state");
                state.uploads.push(request.body);
                state.version = "3".to_owned();
                return FakeResponse::json(serde_json::json!({ "version": "3" }).to_string());
            }
            FakeResponse {
                status: "404 Not Found",
                headers: vec![],
                body: b"not found".to_vec(),
            }
        };
        let server = FakeServer::new(handler);
        Self {
            server,
            state,
            old_started: old_started_rx,
            release_old: release_old_tx,
        }
    }

    fn wait_for_old_request(&self) {
        self.old_started
            .recv_timeout(Duration::from_secs(5))
            .expect("old content request should start");
    }

    fn release_old_request(&self) {
        self.release_old
            .send(())
            .expect("release old content request");
    }
}

fn fake_filesystem(server: &FakeDriveServer) -> GoogleDrive {
    let mut filesystem = filesystem();
    filesystem.client = Client::builder()
        .no_proxy()
        .build()
        .expect("fake Drive client");
    filesystem.api_url = format!("{}/drive/v3", server.server.base_url());
    filesystem.upload_url = format!("{}/upload/drive/v3", server.server.base_url());
    filesystem
}

fn add_listed_file(filesystem: &mut GoogleDrive, writable: bool) -> u64 {
    let mut listed = file("text/plain", writable);
    listed.version = "1".to_owned();
    filesystem.add_drive_file(ROOT_INODE, &listed)
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
    let file: DriveFile =
        serde_json::from_str(r#"{"id":"id","name":"file","mimeType":"text/plain","size":"42"}"#)
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
    let my_drive = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
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
    add_tree_with_limits(&mut filesystem, root, "root", &files, 4, 4);

    assert_eq!(filesystem.children[&root].len(), FILES);
    assert_eq!(filesystem.nodes.len(), 2 + FILES);
}

#[test]
fn shortcut_listed_before_its_target_is_not_an_alias() {
    // The shortcut registers its target id as a parent lookup key, but that
    // must not make the target's own first mount look like an alias.
    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut shortcut = file(SHORTCUT, false);
    shortcut.id = "shortcut".to_owned();
    shortcut.name = "shortcut".to_owned();
    shortcut.parents = vec!["root".to_owned()];
    shortcut.resolved_target = Some(ShortcutTarget {
        id: "target".to_owned(),
        mime_type: FOLDER.to_owned(),
        size: 0,
        capabilities: Capabilities::default(),
        version: "1".to_owned(),
    });
    let mut target = file(FOLDER, false);
    target.id = "target".to_owned();
    target.name = "target".to_owned();
    target.parents = vec!["root".to_owned()];

    // A zero alias cap keeps only first mounts, so both entries must exist.
    add_tree_with_limits(
        &mut filesystem,
        root,
        "root",
        &[shortcut, target],
        0,
        MAX_ALIAS_DEPTH,
    );

    assert!(filesystem.lookup(root, b"shortcut").is_ok());
    assert!(filesystem.lookup(root, b"target").is_ok());
    assert_eq!(filesystem.nodes.len(), 1 + 1 + 2);
}

#[test]
fn shortcut_targeting_its_parent_is_mounted_once() {
    // A shortcut whose resolved target is its own parent is still a first
    // mount. It must appear once under the parent, but it must not be
    // re-expanded under its own alias.
    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut folder = file(FOLDER, false);
    folder.id = "folder".to_owned();
    folder.name = "folder".to_owned();
    folder.parents = vec!["root".to_owned()];
    let mut shortcut = file(SHORTCUT, false);
    shortcut.id = "shortcut".to_owned();
    shortcut.name = "shortcut".to_owned();
    shortcut.parents = vec!["folder".to_owned()];
    shortcut.resolved_target = Some(ShortcutTarget {
        id: "folder".to_owned(),
        mime_type: FOLDER.to_owned(),
        size: 0,
        capabilities: Capabilities::default(),
        version: "1".to_owned(),
    });

    filesystem.add_tree(root, "root", &[folder, shortcut]);

    let (folder_inode, _) = filesystem.lookup(root, b"folder").expect("parent folder");
    let (shortcut_inode, _) = filesystem
        .lookup(folder_inode, b"shortcut")
        .expect("shortcut under its target parent");
    assert!(filesystem.lookup(shortcut_inode, b"shortcut").is_err());
    assert_eq!(filesystem.nodes.len(), 1 + 1 + 2);
}

#[test]
fn distinct_shortcuts_are_first_mounts_not_aliases() {
    const SHORTCUTS: usize = 10;
    const CHILDREN: usize = 10;
    const MAX_NODES: usize = 3;

    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut folder = file(FOLDER, false);
    folder.id = "F".to_owned();
    folder.name = "F".to_owned();
    folder.parents = vec!["root".to_owned()];
    let mut files = vec![folder];
    for index in 0..SHORTCUTS {
        let mut shortcut = file(SHORTCUT, false);
        shortcut.id = format!("S{index}");
        shortcut.name.clone_from(&shortcut.id);
        shortcut.parents = vec!["root".to_owned()];
        shortcut.resolved_target = Some(ShortcutTarget {
            id: "F".to_owned(),
            mime_type: FOLDER.to_owned(),
            size: 0,
            capabilities: Capabilities::default(),
            version: "1".to_owned(),
        });
        files.push(shortcut);
    }
    for index in 0..CHILDREN {
        let mut child = file("text/plain", false);
        child.id = format!("C{index}");
        child.name.clone_from(&child.id);
        child.parents = vec!["F".to_owned()];
        files.push(child);
    }

    add_tree_with_limits(
        &mut filesystem,
        root,
        "root",
        &files,
        MAX_NODES,
        MAX_ALIAS_DEPTH,
    );

    let (folder_inode, _) = filesystem.lookup(root, b"F").expect("canonical folder");
    // Every distinct shortcut is its own first mount, even though they all
    // target the same folder.
    for index in 0..SHORTCUTS {
        assert!(
            filesystem
                .lookup(root, format!("S{index}").as_bytes())
                .is_ok(),
            "shortcut S{index} must be mounted"
        );
    }
    // The folder's children are first mounts under the canonical folder...
    for index in 0..CHILDREN {
        assert!(
            filesystem
                .lookup(folder_inode, format!("C{index}").as_bytes())
                .is_ok(),
            "child C{index} must be mounted under F"
        );
    }
    // ...while re-mounting those same children under the shortcuts is
    // bounded by the alias cap.
    let alias_children: usize = (0..SHORTCUTS)
        .map(|index| {
            let (shortcut_inode, _) = filesystem
                .lookup(root, format!("S{index}").as_bytes())
                .expect("shortcut");
            filesystem.children.get(&shortcut_inode).map_or(0, Vec::len)
        })
        .sum();
    assert_eq!(alias_children, MAX_NODES);
    assert_eq!(
        filesystem.nodes.len(),
        1 + 1 + 1 + SHORTCUTS + CHILDREN + MAX_NODES
    );
}

#[test]
fn alias_expansion_stops_at_the_alias_cap() {
    // F and S are distinct listing entries, so both are first mounts. Only
    // the ten children re-mounted beneath S count as aliases.
    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut folder = file(FOLDER, false);
    folder.id = "F".to_owned();
    folder.name = "F".to_owned();
    folder.parents = vec!["root".to_owned()];
    let mut shortcut = file(SHORTCUT, false);
    shortcut.id = "S".to_owned();
    shortcut.name = "S".to_owned();
    shortcut.parents = vec!["root".to_owned()];
    shortcut.resolved_target = Some(ShortcutTarget {
        id: "F".to_owned(),
        mime_type: FOLDER.to_owned(),
        size: 0,
        capabilities: Capabilities::default(),
        version: "1".to_owned(),
    });
    let mut files = vec![folder, shortcut];
    for index in 0..10 {
        let mut child = file("text/plain", false);
        child.id = format!("C{index}");
        child.name.clone_from(&child.id);
        child.parents = vec!["F".to_owned()];
        files.push(child);
    }

    add_tree_with_limits(&mut filesystem, root, "root", &files, 3, MAX_ALIAS_DEPTH);

    let (folder_inode, _) = filesystem.lookup(root, b"F").expect("canonical folder");
    let (shortcut_inode, _) = filesystem.lookup(root, b"S").expect("shortcut");
    // Every child is a first mount under the canonical folder.
    for index in 0..10 {
        assert!(
            filesystem
                .lookup(folder_inode, format!("C{index}").as_bytes())
                .is_ok(),
            "child C{index} must be mounted under F"
        );
    }
    // Only three re-mounts under the shortcut alias are allowed.
    assert_eq!(filesystem.children[&shortcut_inode].len(), 3);
    // Filesystem root, the mounted root, F, S, ten children, three aliases.
    assert_eq!(filesystem.nodes.len(), 1 + 1 + 1 + 1 + 10 + 3);
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

    add_tree_with_limits(&mut filesystem, root, "root", &files, 4, 4);

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
    // first mount per folder and per shortcut, and exactly the alias cap's
    // worth of aliases.
    assert_eq!(filesystem.nodes.len(), 3 + 2 * DEPTH + MAX_ALIAS_NODES);
}

#[test]
fn alias_depth_is_bounded_but_real_folders_are_not() {
    // F0..F10 form one real folder chain, and Sk is a shortcut to Fk listed
    // inside F(k-1). The real chain is a first mount at every level, so the
    // alias depth limit must only truncate repeated alias paths.
    const FOLDERS: usize = 10;
    const MAX_DEPTH: usize = 5;

    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);

    let mut files = Vec::new();
    for index in 0..=FOLDERS {
        let mut folder = file(FOLDER, false);
        folder.id = format!("F{index}");
        folder.name.clone_from(&folder.id);
        folder.parents = vec![if index == 0 {
            "root".to_owned()
        } else {
            format!("F{}", index - 1)
        }];
        files.push(folder);
    }
    for index in 1..=FOLDERS {
        let mut shortcut = file(SHORTCUT, false);
        shortcut.id = format!("S{index}");
        shortcut.name.clone_from(&shortcut.id);
        shortcut.parents = vec![format!("F{}", index - 1)];
        shortcut.resolved_target = Some(ShortcutTarget {
            id: format!("F{index}"),
            mime_type: FOLDER.to_owned(),
            size: 0,
            capabilities: Capabilities::default(),
            version: "1".to_owned(),
        });
        files.push(shortcut);
    }

    add_tree_with_limits(
        &mut filesystem,
        root,
        "root",
        &files,
        MAX_ALIAS_NODES,
        MAX_DEPTH,
    );

    // Every real folder survives along the canonical chain even though the
    // chain is deeper than the alias limit.
    let mut parent = root;
    for index in 0..=FOLDERS {
        let (inode, _) = filesystem
            .lookup(parent, format!("F{index}").as_bytes())
            .expect("canonical folder is a first mount");
        parent = inode;
    }

    // The shortcut chain is an alias path: S1 is a first mount under F0,
    // and S2 is re-mounted beneath it, but the next level is past the depth
    // limit.
    let (f0, _) = filesystem.lookup(root, b"F0").expect("F0");
    let (s1, _) = filesystem.lookup(f0, b"S1").expect("S1 under F0");
    let (s2_alias, _) = filesystem.lookup(s1, b"S2").expect("S2 alias under S1");
    assert!(filesystem.lookup(s2_alias, b"S3").is_err());
    assert!(filesystem.lookup(s2_alias, b"F3").is_err());

    // A deeper alias path can still be created where the parent fits below
    // the limit, but it is truncated one level later.
    let (f2_alias, _) = filesystem.lookup(s1, b"F2").expect("F2 alias under S1");
    let (s3_alias, _) = filesystem
        .lookup(f2_alias, b"S3")
        .expect("S3 alias under the F2 alias");
    assert!(filesystem.lookup(s3_alias, b"S4").is_err());
}

#[test]
fn alias_cap_keeps_the_seen_set_proportional_to_the_tree() {
    // Many aliases of one folder times many children is the case that used
    // to record every (file, alias-parent) pair even after the cap was hit.
    const SHORTCUTS: usize = 50;
    const CHILDREN: usize = 50;
    const MAX_NODES: usize = 5;

    let mut filesystem = filesystem();
    let root = filesystem.add_node(ROOT_INODE, "My Drive", FileType::Directory, 0, None, false);
    let mut folder = file(FOLDER, false);
    folder.id = "F".to_owned();
    folder.name = "F".to_owned();
    folder.parents = vec!["root".to_owned()];
    let mut files = vec![folder];
    for index in 0..SHORTCUTS {
        let mut shortcut = file(SHORTCUT, false);
        shortcut.id = format!("S{index}");
        shortcut.name.clone_from(&shortcut.id);
        shortcut.parents = vec!["root".to_owned()];
        shortcut.resolved_target = Some(ShortcutTarget {
            id: "F".to_owned(),
            mime_type: FOLDER.to_owned(),
            size: 0,
            capabilities: Capabilities::default(),
            version: "1".to_owned(),
        });
        files.push(shortcut);
    }
    for index in 0..CHILDREN {
        let mut child = file("text/plain", false);
        child.id = format!("C{index}");
        child.name.clone_from(&child.id);
        child.parents = vec!["F".to_owned()];
        files.push(child);
    }

    let (nodes_created, seen) = add_tree_with_stats(
        &mut filesystem,
        root,
        "root",
        &files,
        MAX_NODES,
        MAX_ALIAS_DEPTH,
    );

    // One folder, every shortcut, every child, plus only the capped aliases.
    assert_eq!(nodes_created, 1 + SHORTCUTS + CHILDREN + MAX_NODES);
    // Recording every rejected (file, alias-parent) pair would take roughly
    // CHILDREN * SHORTCUTS == 2,500 entries. The early-outs keep `seen` near
    // the number of created nodes instead.
    assert!(
        seen <= nodes_created + files.len(),
        "seen={seen} nodes={nodes_created} files={}",
        files.len()
    );

    let alias_children: usize = (0..SHORTCUTS)
        .map(|index| {
            let (shortcut_inode, _) = filesystem
                .lookup(root, format!("S{index}").as_bytes())
                .expect("shortcut");
            filesystem.children.get(&shortcut_inode).map_or(0, Vec::len)
        })
        .sum();
    assert_eq!(alias_children, MAX_NODES);
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
    seed_cache(&filesystem, "id", "1", b"abc");
    // Model the second write after the first write has already buffered data.
    filesystem
        .dirty
        .lock()
        .expect("dirty")
        .insert("id".to_owned());

    assert_eq!(filesystem.write(inode, 1, b"XY"), Ok(2));
    assert_eq!(
        cached_bytes(&filesystem.cache.lock().expect("cache"), "id"),
        Some(b"aXY".to_vec())
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
fn remote_reads_do_not_request_unsafe_kernel_read_caching() {
    let filesystem = filesystem();

    assert_eq!(
        filesystem.network_configuration().kernel_cache_policy,
        KernelCachePolicy::MetadataOnly
    );
}

#[test]
fn changed_remote_version_does_not_reuse_old_cached_bytes() {
    let server = FakeDriveServer::new(false);
    server.state.lock().expect("fake Drive state").version = "2".to_owned();
    let mut filesystem = fake_filesystem(&server);
    let inode = add_listed_file(&mut filesystem, false);
    assert_eq!(
        filesystem
            .cache
            .lock()
            .expect("cache")
            .versions
            .get("id")
            .map(String::as_str),
        Some("1"),
        "the listing should start with version 1"
    );

    let mut first = [0_u8; 2];
    assert_eq!(filesystem.read(inode, 0, &mut first), Ok(2));
    assert_eq!(&first, b"v2");
    let after_first = filesystem
        .metadata(inode)
        .expect("metadata after first read");
    assert_eq!(
        cached_version(&filesystem.cache.lock().expect("cache"), "id"),
        Some("2")
    );

    let mut second = [0_u8; 2];
    assert_eq!(filesystem.read(inode, 0, &mut second), Ok(2));
    assert_eq!(&second, b"v2");
    let after_second = filesystem
        .metadata(inode)
        .expect("metadata after second read");
    assert_eq!(after_second.modified_ns, after_first.modified_ns);

    let state = server.state.lock().expect("fake Drive state");
    assert_eq!(state.metadata_gets, 3);
    assert_eq!(state.content_gets.get("2"), Some(&1));
}

#[test]
fn continuously_changing_remote_version_returns_stale() {
    let server = FakeDriveServer::new(false);
    {
        let mut state = server.state.lock().expect("fake Drive state");
        state.advance_version_on_metadata = true;
    }
    let mut filesystem = fake_filesystem(&server);
    let inode = add_listed_file(&mut filesystem, false);

    let mut output = [0_u8; 2];
    assert_eq!(filesystem.read(inode, 0, &mut output), Err(Error::STALE));

    let state = server.state.lock().expect("fake Drive state");
    let attempts = MAX_DOWNLOAD_RETRIES + 1;
    assert_eq!(state.metadata_gets, attempts * 2);
    assert_eq!(state.content_gets.values().sum::<usize>(), attempts);
}

#[test]
fn clean_cached_write_revalidates_before_buffering() {
    let server = FakeDriveServer::new(false);
    server.state.lock().expect("fake Drive state").version = "2".to_owned();
    let mut filesystem = fake_filesystem(&server);
    let inode = add_listed_file(&mut filesystem, true);
    let mut output = [0_u8; 2];
    assert_eq!(filesystem.read(inode, 0, &mut output), Ok(2));
    assert_eq!(&output, b"v2");
    let before = filesystem.metadata(inode).expect("metadata before write");

    server.state.lock().expect("fake Drive state").version = "3".to_owned();
    assert_eq!(filesystem.write(inode, 0, b"X"), Ok(1));

    let cache = filesystem.cache.lock().expect("cache");
    assert_eq!(cached_bytes(&cache, "id"), Some(b"X3".to_vec()));
    assert_eq!(cached_version(&cache, "id"), Some("3"));
    drop(cache);
    assert!(filesystem.dirty.lock().expect("dirty").contains("id"));
    let after = filesystem.metadata(inode).expect("metadata after write");
    assert!(after.modified_ns > before.modified_ns);

    let state = server.state.lock().expect("fake Drive state");
    assert_eq!(state.metadata_gets, 4);
    assert_eq!(state.content_gets.get("2"), Some(&1));
    assert_eq!(state.content_gets.get("3"), Some(&1));
}

#[test]
fn stale_in_flight_download_cannot_replace_newer_bytes_or_upload_old_data() {
    let server = FakeDriveServer::new(true);
    let mut filesystem = fake_filesystem(&server);
    let inode = add_listed_file(&mut filesystem, true);
    let filesystem = Arc::new(filesystem);
    let old_filesystem = Arc::clone(&filesystem);
    let old_read = thread::spawn(move || {
        let mut output = [0_u8; 2];
        (old_filesystem.read(inode, 0, &mut output), output)
    });
    server.wait_for_old_request();

    // Let a second v1 request finish so the old request is followed by a clean
    // cache entry that can be invalidated when the server advances to v2.
    let mut output = [0_u8; 2];
    assert_eq!(filesystem.read(inode, 0, &mut output), Ok(2));
    assert_eq!(&output, b"v1");
    server.state.lock().expect("fake Drive state").version = "2".to_owned();

    let mut newer = [0_u8; 2];
    assert_eq!(filesystem.read(inode, 0, &mut newer), Ok(2));
    assert_eq!(&newer, b"v2");
    let modified_ns = filesystem
        .metadata(inode)
        .expect("metadata after v2")
        .modified_ns;

    server.release_old_request();
    let (old_result, old_output) = old_read.join().expect("old read thread");
    assert_eq!(old_result, Ok(2));
    assert_eq!(
        &old_output, b"v2",
        "the old read must not return stale bytes"
    );
    assert_eq!(
        filesystem
            .metadata(inode)
            .expect("metadata after old read")
            .modified_ns,
        modified_ns
    );
    {
        let cache = filesystem.cache.lock().expect("cache");
        assert_eq!(cached_bytes(&cache, "id"), Some(b"v2".to_vec()));
        assert_eq!(cached_version(&cache, "id"), Some("2"));
        assert_eq!(cache.versions.get("id").map(String::as_str), Some("2"));
    }

    let mut repeated = [0_u8; 2];
    assert_eq!(filesystem.read(inode, 0, &mut repeated), Ok(2));
    assert_eq!(&repeated, b"v2");
    assert_eq!(
        filesystem
            .metadata(inode)
            .expect("metadata after repeated read")
            .modified_ns,
        modified_ns
    );
    {
        let state = server.state.lock().expect("fake Drive state");
        assert_eq!(state.content_gets.get("1"), Some(&2));
        assert_eq!(state.content_gets.get("2"), Some(&1));
    }

    assert_eq!(filesystem.write(inode, 0, b"X"), Ok(1));
    assert_eq!(filesystem.synchronize(), Ok(()));
    {
        let state = server.state.lock().expect("fake Drive state");
        assert_eq!(state.upload_starts, 1);
        assert_eq!(state.uploads, vec![b"X2".to_vec()]);
    }
    let cache = filesystem.cache.lock().expect("cache");
    assert_eq!(cached_bytes(&cache, "id"), Some(b"X2".to_vec()));
    assert_eq!(cached_version(&cache, "id"), Some("3"));
}

#[test]
fn transient_export_failure_does_not_silently_remove_document() {
    let mut filesystem = filesystem();
    filesystem.client = offline_client();
    let inode = filesystem.add_node(
        ROOT_INODE,
        "Document.docx",
        FileType::File,
        0,
        Some(RemoteFile {
            id: "doc".to_owned(),
            mime_type: "application/vnd.google-apps.document".to_owned(),
        }),
        false,
    );

    filesystem.hydrate_exports();

    assert_eq!(
        filesystem.lookup(ROOT_INODE, b"Document.docx"),
        Ok((inode, FileType::File)),
        "a transient export error must not make the document disappear"
    );
}

#[test]
fn lookup_below_a_file_returns_not_a_directory() {
    let mut filesystem = filesystem();
    let inode = filesystem.add_node(ROOT_INODE, "file", FileType::File, 0, None, false);

    assert_eq!(filesystem.lookup(inode, b"child"), Err(Error(20)));
}

#[test]
fn zero_length_write_beyond_the_buffer_limit_is_a_no_op() {
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
    seed_cache(&filesystem, "id", "1", b"abc");
    filesystem
        .dirty
        .lock()
        .expect("dirty")
        .insert("id".to_owned());

    assert_eq!(
        filesystem.write(inode, MAX_BUFFERED_FILE_SIZE + 1, &[]),
        Ok(0)
    );
    assert_eq!(
        cached_bytes(&filesystem.cache.lock().expect("cache"), "id"),
        Some(b"abc".to_vec())
    );
}

#[test]
fn writes_update_mtime_for_every_remote_alias() {
    let mut filesystem = filesystem();
    let remote = RemoteFile {
        id: "id".to_owned(),
        mime_type: "text/plain".to_owned(),
    };
    let first = filesystem.add_node(
        ROOT_INODE,
        "first",
        FileType::File,
        3,
        Some(remote.clone()),
        true,
    );
    let second = filesystem.add_node(ROOT_INODE, "second", FileType::File, 3, Some(remote), true);
    seed_cache(&filesystem, "id", "1", b"abc");
    filesystem
        .dirty
        .lock()
        .expect("dirty")
        .insert("id".to_owned());
    let before = filesystem.metadata(first).expect("metadata before");

    assert_eq!(filesystem.write(first, 0, b"z"), Ok(1));

    for inode in [first, second] {
        let after = filesystem.metadata(inode).expect("metadata after write");
        assert!(
            after.modified_ns > before.modified_ns,
            "mtime did not advance for inode {inode}"
        );
    }
}

#[test]
fn completed_download_does_not_replace_concurrent_cached_write() {
    let cache = Mutex::new(FileCache {
        entries: HashMap::from([(
            "id".to_owned(),
            CachedFile {
                version: "1".to_owned(),
                data: b"new dirty data".to_vec(),
            },
        )]),
        ..FileCache::default()
    });
    let dirty = Mutex::new(HashSet::from(["id".to_owned()]));
    let selected = cache_download(
        &cache,
        &dirty,
        "id",
        "2".to_owned(),
        0,
        b"stale download".to_vec(),
    )
    .expect("select cached data");
    assert_eq!(
        selected,
        CacheDownload::Existing(b"new dirty data".to_vec())
    );
    assert_eq!(
        cached_bytes(&cache.lock().expect("cache"), "id"),
        Some(b"new dirty data".to_vec())
    );
}

#[test]
fn clean_old_version_download_cannot_replace_newer_cache() {
    let cache = Mutex::new(FileCache {
        entries: HashMap::from([(
            "id".to_owned(),
            CachedFile {
                version: "2".to_owned(),
                data: b"new".to_vec(),
            },
        )]),
        versions: HashMap::from([("id".to_owned(), "2".to_owned())]),
        ..FileCache::default()
    });
    let dirty = Mutex::new(HashSet::new());

    let selected = cache_download(&cache, &dirty, "id", "1".to_owned(), 0, b"old".to_vec())
        .expect("reject old clean download");

    assert_eq!(selected, CacheDownload::Retry);
    let cache = cache.lock().expect("cache");
    assert_eq!(cached_bytes(&cache, "id"), Some(b"new".to_vec()));
    assert_eq!(cached_version(&cache, "id"), Some("2"));
    assert_eq!(cache.versions.get("id").map(String::as_str), Some("2"));
}

#[test]
fn invalidated_epoch_rejects_completed_download() {
    let cache = Mutex::new(FileCache {
        versions: HashMap::from([("id".to_owned(), "1".to_owned())]),
        epochs: HashMap::from([("id".to_owned(), 1)]),
        ..FileCache::default()
    });
    let dirty = Mutex::new(HashSet::new());

    let selected = cache_download(&cache, &dirty, "id", "1".to_owned(), 0, b"old".to_vec())
        .expect("reject download from an invalidated epoch");

    assert_eq!(selected, CacheDownload::Retry);
    let cache = cache.lock().expect("cache");
    assert_eq!(cached_bytes(&cache, "id"), None);
    assert_eq!(cache.versions.get("id").map(String::as_str), Some("1"));
    assert_eq!(cache.epoch("id"), 1);
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

/// Mount a real Drive account through FUSE and read the CI fixture.
///
/// Ignored by default because it needs network access, Google credentials, and
/// a working FUSE installation. CI runs it with:
///
/// ```sh
/// cargo test -p fsk-google-drive -- --ignored live_mount
/// ```
///
/// Set `FSK_DRIVE_TEST_PATH` to override the fixture path under `My Drive`
/// (default: `fsk-ci/hello.txt`). Its contents must be `hello from fsk ci\n`.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires network access, Google credentials, and FUSE"]
fn live_mount_reads_drive_fixture() {
    let filesystem = GoogleDrive::connect(live_access_token()).expect("connect to Google Drive");
    let mount = tempfile::tempdir().expect("mount directory");
    let session =
        fsk::MountSession::mount_network(filesystem, mount.path()).expect("mount Google Drive");

    let fixture =
        std::env::var("FSK_DRIVE_TEST_PATH").unwrap_or_else(|_| "fsk-ci/hello.txt".to_owned());
    let mut path = mount.path().join("My Drive");
    for segment in fixture.split('/').filter(|segment| !segment.is_empty()) {
        path.push(segment);
    }

    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert_eq!(content, "hello from fsk ci\n", "unexpected fixture content");

    let metadata = std::fs::metadata(&path).expect("fixture metadata");
    assert_eq!(metadata.len(), content.len() as u64);

    let missing = std::fs::read(mount.path().join("My Drive").join("fsk-ci-missing-file"));
    assert_eq!(
        missing.expect_err("missing files must fail").kind(),
        std::io::ErrorKind::NotFound
    );

    drop(session);
}

#[cfg(target_os = "linux")]
fn live_access_token() -> String {
    if let Ok(token) = std::env::var("GOOGLE_DRIVE_ACCESS_TOKEN") {
        return token;
    }
    let client_id = std::env::var("GOOGLE_CLIENT_ID")
        .expect("GOOGLE_CLIENT_ID must be set (or GOOGLE_DRIVE_ACCESS_TOKEN)");
    let client_secret = std::env::var("GOOGLE_CLIENT_SECRET")
        .expect("GOOGLE_CLIENT_SECRET must be set (or GOOGLE_DRIVE_ACCESS_TOKEN)");
    let refresh_token = std::env::var("GOOGLE_DRIVE_REFRESH_TOKEN")
        .expect("GOOGLE_DRIVE_REFRESH_TOKEN must be set (or GOOGLE_DRIVE_ACCESS_TOKEN)");
    refresh_access_token(&client_id, &client_secret, &refresh_token)
        .expect("exchange Google Drive refresh token")
}
