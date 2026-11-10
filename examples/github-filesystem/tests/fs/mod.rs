use super::*;
use fsk::Filesystem;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

struct FakeGitHub {
    url: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake GitHub");
        listener.set_nonblocking(true).expect("nonblocking");
        let address = listener.local_addr().expect("server address");
        let url = format!("http://{address}");
        let server_url = url.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stop_flag.load(Ordering::Acquire) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                };
                stream.set_nonblocking(false).expect("blocking connection");
                let mut input = [0_u8; 4096];
                let count = stream.read(&mut input).expect("read request");
                let request = String::from_utf8_lossy(&input[..count]);
                let target = request.split_whitespace().nth(1).expect("request target");
                request_count.fetch_add(1, Ordering::Relaxed);
                let (content_type, body) = if target.starts_with("/user/repos") {
                    (
                        "application/json",
                        r#"[{"name":"fsk","owner":{"login":"specta-rs"}}]"#.to_owned(),
                    )
                } else if target == "/repos/specta-rs/fsk/contents" {
                    (
                        "application/json",
                        format!(
                            r#"[{{"name":"README.md","path":"README.md","sha":"abc123","size":12,"type":"file","download_url":"{server_url}/download/readme","content":null}},{{"name":"src","path":"src","sha":"dir1","size":0,"type":"dir","download_url":null,"content":null}}]"#
                        ),
                    )
                } else if target == "/download/readme" {
                    ("application/octet-stream", "hello github".to_owned())
                } else {
                    ("application/json", "{}".to_owned())
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });
        Self {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join fake server");
        }
    }
}

#[test]
fn end_to_end_github_semantics_and_local_reopen() {
    let server = FakeGitHub::start();
    let cache = tempfile::tempdir().expect("cache directory");
    let filesystem = GitHubFilesystem::new(Options {
        api_url: server.url.clone(),
        token: "test-token".to_owned(),
        cache_dir: cache.path().to_owned(),
        max_cache_bytes: 1024,
        max_idle: Duration::from_secs(60),
        download_on_list: false,
    })
    .expect("filesystem");

    let (owner, owner_kind) = filesystem.lookup(ROOT_INODE, b"specta-rs").expect("owner");
    assert_eq!(owner_kind, FileType::Directory);
    let (repo, _) = filesystem.lookup(owner, b"fsk").expect("repository");
    let (readme, readme_kind) = filesystem.lookup(repo, b"README.md").expect("README");
    assert_eq!(readme_kind, FileType::File);
    assert_eq!(filesystem.metadata(readme).expect("metadata").mode, 0o444);
    assert_eq!(
        filesystem.open_file(readme, false).expect("first open"),
        NetworkFile::Remote
    );

    let mut output = [0_u8; 32];
    let count = filesystem
        .read(readme, 0, &mut output)
        .expect("API-backed read");
    assert_eq!(&output[..count], b"hello github");
    let local = filesystem.open_file(readme, false).expect("cached open");
    assert!(matches!(local, NetworkFile::Local(_)));
    filesystem.close_file(readme, local).expect("close lease");
    assert_eq!(
        filesystem.write(readme, 0, b"no"),
        Err(Error::NOT_SUPPORTED)
    );
    assert_eq!(filesystem.open_file(readme, true), Err(Error(EACCES)));
    assert_eq!(server.requests.load(Ordering::Relaxed), 3);
}

#[test]
fn cleanup_evicts_idle_files_but_not_leases() {
    let directory = tempfile::tempdir().expect("cache directory");
    let cache =
        Cache::new(directory.path().to_owned(), 1024, Duration::from_secs(10)).expect("cache");
    let file = FileNode {
        owner: "o".into(),
        repo: "r".into(),
        path: "f".into(),
        sha: "s".into(),
        size: 4,
        download_url: None,
        inline: None,
    };
    let path = cache
        .materialize(&file, || Ok(b"data".to_vec()))
        .expect("materialize");
    let lease = cache.acquire(&file).expect("acquire").expect("local path");
    let old = filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(60));
    filetime::set_file_mtime(&path, old).expect("age cache file");
    cache.cleanup().expect("cleanup while leased");
    assert!(path.exists());
    cache.release(&lease).expect("release");
    assert!(!path.exists());
}
