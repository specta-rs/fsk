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

#[derive(Clone, Copy)]
enum FakeMode {
    Normal,
    Installation,
    Paginated,
}

impl FakeGitHub {
    fn start() -> Self {
        Self::start_configured(FakeMode::Normal, None)
    }

    /// Serves the workflow installation token's endpoints: `/user/repos` is
    /// forbidden and repositories come from `/installation/repositories`.
    fn start_installation() -> Self {
        Self::start_configured(FakeMode::Installation, None)
    }

    fn start_with_repository_contents(contents: String) -> Self {
        Self::start_configured(FakeMode::Normal, Some(contents))
    }

    fn start_paginated() -> Self {
        Self::start_configured(FakeMode::Paginated, None)
    }

    #[allow(clippy::too_many_lines)]
    fn start_configured(mode: FakeMode, repository_contents: Option<String>) -> Self {
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
                let (status, content_type, body) = if target.starts_with("/user/repos") {
                    if matches!(mode, FakeMode::Installation) {
                        (
                            "403 Forbidden",
                            "application/json",
                            r#"{"message":"Resource not accessible by integration"}"#.to_owned(),
                        )
                    } else if matches!(mode, FakeMode::Paginated) {
                        let name = if target.contains("page=2") {
                            "second"
                        } else {
                            "first"
                        };
                        (
                            "200 OK",
                            "application/json",
                            format!(r#"[{{"name":"{name}","owner":{{"login":"owner"}}}}]"#),
                        )
                    } else {
                        (
                            "200 OK",
                            "application/json",
                            r#"[{"name":"fsk","owner":{"login":"specta-rs"}}]"#.to_owned(),
                        )
                    }
                } else if target.starts_with("/installation/repositories") {
                    (
                        "200 OK",
                        "application/json",
                        r#"{"total_count":1,"repositories":[{"name":"fsk","owner":{"login":"specta-rs"}}]}"#
                            .to_owned(),
                    )
                } else if target == "/repos/specta-rs/fsk/contents" {
                    let body = repository_contents.clone().unwrap_or_else(|| {
                        format!(
                            r#"[{{"name":"README.md","path":"README.md","sha":"abc123","size":12,"type":"file","download_url":"{server_url}/download/readme","content":null}},{{"name":"src","path":"src","sha":"dir1","size":0,"type":"dir","download_url":null,"content":null}}]"#
                        )
                    });
                    ("200 OK", "application/json", body)
                } else if target == "/download/readme" {
                    // The branch moved after we indexed the original blob.
                    (
                        "200 OK",
                        "application/octet-stream",
                        "updated readme".to_owned(),
                    )
                } else if target == "/repos/specta-rs/fsk/git/blobs/abc123"
                    && request.lines().any(|line| {
                        line.eq_ignore_ascii_case("accept: application/vnd.github.raw+json")
                    })
                {
                    (
                        "200 OK",
                        "application/octet-stream",
                        "hello github".to_owned(),
                    )
                } else {
                    ("200 OK", "application/json", "{}".to_owned())
                };
                let link_header = if matches!(mode, FakeMode::Paginated)
                    && target.starts_with("/user/repos")
                    && !target.contains("page=2")
                {
                    format!("Link: <{server_url}/user/repos?per_page=100&page=2>; rel=\"next\"\r\n")
                } else {
                    String::new()
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{link_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    assert_eq!(&output[..count], b"hello github", "read the indexed blob");
    let local = filesystem.open_file(readme, false).expect("cached open");
    let NetworkFile::Local(lease) = &local else {
        panic!("expected a cached local file");
    };
    assert_eq!(fs::read(lease.path()).unwrap(), b"hello github");
    filesystem.close_file(readme, local).expect("close lease");
    assert_eq!(
        filesystem.write(readme, 0, b"no"),
        Err(Error::NOT_SUPPORTED)
    );
    assert_eq!(filesystem.open_file(readme, true), Err(Error(EACCES)));
    assert_eq!(server.requests.load(Ordering::Relaxed), 3);
}

#[test]
fn installation_tokens_discover_repositories() {
    let server = FakeGitHub::start_installation();
    let cache = tempfile::tempdir().expect("cache directory");
    let filesystem = GitHubFilesystem::new(Options {
        api_url: server.url.clone(),
        token: "ghs_test".to_owned(),
        cache_dir: cache.path().to_owned(),
        max_cache_bytes: 1024,
        max_idle: Duration::from_secs(60),
        download_on_list: false,
    })
    .expect("filesystem");

    let (owner, kind) = filesystem.lookup(ROOT_INODE, b"specta-rs").expect("owner");
    assert_eq!(kind, FileType::Directory);
    let (repo, kind) = filesystem.lookup(owner, b"fsk").expect("repository");
    assert_eq!(kind, FileType::Directory);
    let (_readme, kind) = filesystem.lookup(repo, b"README.md").expect("README");
    assert_eq!(kind, FileType::File);
    // `/user/repos` was rejected, so discovery used
    // `/installation/repositories` before loading the repository contents.
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

#[test]
fn repository_pagination_follows_link_when_a_page_is_short() {
    let server = FakeGitHub::start_paginated();
    let api = Api::new(&server.url, "test-token").expect("API client");

    let repositories = api.user_repositories().expect("repository pages");
    let names: Vec<_> = repositories
        .into_iter()
        .map(|repository| repository.name)
        .collect();

    assert_eq!(names, ["first", "second"], "the Link next page was skipped");
    assert_eq!(server.requests.load(Ordering::Relaxed), 2);
}

fn filesystem_with_unsupported_git_entries()
-> (FakeGitHub, tempfile::TempDir, GitHubFilesystem, u64) {
    let server = FakeGitHub::start_with_repository_contents(
        r#"[{"name":"link","path":"link","sha":"symlink-sha","size":4,"type":"symlink","target":"README.md"},{"name":"module","path":"module","sha":"commit-sha","size":0,"type":"submodule","submodule_git_url":"https://example.invalid/module.git"}]"#
            .to_owned(),
    );
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
    let (owner, _) = filesystem.lookup(ROOT_INODE, b"specta-rs").expect("owner");
    let (repo, _) = filesystem.lookup(owner, b"fsk").expect("repository");
    (server, cache, filesystem, repo)
}

#[test]
fn symlinks_are_not_exposed_as_regular_files() {
    let (_server, _cache, filesystem, repo) = filesystem_with_unsupported_git_entries();

    assert_eq!(
        filesystem.lookup(repo, b"link"),
        Err(Error(ENOENT)),
        "a symlink must not be presented as a regular file containing link text"
    );
}

#[test]
fn submodules_are_not_exposed_as_regular_files() {
    let (_server, _cache, filesystem, repo) = filesystem_with_unsupported_git_entries();

    assert_eq!(
        filesystem.lookup(repo, b"module"),
        Err(Error(ENOENT)),
        "a submodule commit SHA is not a readable Git blob"
    );
}

#[test]
fn contents_page_at_the_api_limit_is_not_reported_as_complete() {
    let entries = (0..1000)
        .map(|index| {
            format!(
                r#"{{"name":"file-{index:04}","path":"file-{index:04}","sha":"sha-{index}","size":1,"type":"file","content":null}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let server = FakeGitHub::start_with_repository_contents(format!("[{entries}]"));
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
    let (owner, _) = filesystem.lookup(ROOT_INODE, b"specta-rs").expect("owner");
    let (repo, _) = filesystem.lookup(owner, b"fsk").expect("repository");

    assert_eq!(
        filesystem.lookup(repo, b"missing"),
        Err(Error::IO),
        "GitHub's 1,000-entry contents response may be truncated and must not be marked complete"
    );
}

#[test]
fn another_cache_instance_must_not_evict_a_leased_file() {
    let directory = tempfile::tempdir().expect("cache directory");
    let first = Cache::new(directory.path().to_owned(), 1024, Duration::from_secs(10))
        .expect("first cache");
    let leased_file = FileNode {
        owner: "o".into(),
        repo: "r".into(),
        path: "leased".into(),
        sha: "leased-sha".into(),
        size: 4,
        inline: None,
    };
    let other_file = FileNode {
        path: "other".into(),
        sha: "other-sha".into(),
        ..leased_file.clone()
    };
    let leased_path = first
        .materialize(&leased_file, || Ok(b"data".to_vec()))
        .expect("leased materialize");
    first
        .materialize(&other_file, || Ok(b"temp".to_vec()))
        .expect("other materialize");
    let _lease = first.acquire(&leased_file).expect("acquire lease");
    let old = filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(60));
    filetime::set_file_mtime(&leased_path, old).expect("age leased file");

    let _second = Cache::new(directory.path().to_owned(), 1024, Duration::from_secs(10))
        .expect("second cache sharing the directory");

    assert!(
        leased_path.is_file(),
        "a second cache instance evicted a file held by the first instance"
    );
}

#[test]
fn startup_cleanup_removes_stale_interrupted_downloads() {
    let directory = tempfile::tempdir().expect("cache directory");
    Cache::new(directory.path().to_owned(), 1024, Duration::from_secs(10)).expect("initial cache");
    let temporary = directory.path().join(".1234.5678.tmp");
    fs::write(&temporary, b"partial download").expect("write interrupted download");
    let old = filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(60));
    filetime::set_file_mtime(&temporary, old).expect("age temporary file");

    Cache::new(directory.path().to_owned(), 1024, Duration::from_secs(10))
        .expect("restarted cache");

    assert!(!temporary.exists(), "stale .tmp files leak across restarts");
}

/// Mount a real GitHub repository through FUSE and read it.
///
/// This is the live counterpart to the fake-server test above. It is ignored by
/// default because it needs network access, a token, and a working FUSE
/// installation. CI runs it with:
///
/// ```sh
/// GITHUB_TOKEN=... GITHUB_REPOSITORY=specta-rs/fsk \
///   cargo test -p fsk-github-filesystem -- --ignored live_mount
/// ```
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires network access, a GitHub token, and FUSE"]
fn live_mount_reads_repository() {
    let token = std::env::var("GITHUB_TOKEN").expect("GITHUB_TOKEN must be set");
    let repository = std::env::var("GITHUB_REPOSITORY").expect("GITHUB_REPOSITORY must be set");
    let (owner, repo) = repository
        .split_once('/')
        .expect("GITHUB_REPOSITORY must be owner/repo");
    let cache = tempfile::tempdir().expect("cache directory");
    let filesystem = GitHubFilesystem::new(Options {
        api_url: "https://api.github.com".to_owned(),
        token,
        cache_dir: cache.path().to_owned(),
        max_cache_bytes: 64 * 1024 * 1024,
        max_idle: Duration::from_secs(60),
        download_on_list: false,
    })
    .expect("connect to the GitHub API");
    let mount = tempfile::tempdir().expect("mount directory");
    let session = fsk::MountSession::mount_network(filesystem, mount.path()).expect("mount GitHub");

    let repo_path = mount.path().join(owner).join(repo);
    let mut entries: Vec<_> = std::fs::read_dir(&repo_path)
        .expect("list repository root")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect();
    entries.sort();
    for expected in ["Cargo.toml", "LICENSE", "README.md", "examples"] {
        assert!(
            entries
                .iter()
                .any(|entry| entry.to_string_lossy() == expected),
            "{expected} missing from {entries:?}"
        );
    }

    let license = std::fs::read_to_string(repo_path.join("LICENSE")).expect("read LICENSE");
    assert!(
        license.starts_with("MIT License"),
        "unexpected LICENSE: {license:?}"
    );

    let examples = std::fs::read_dir(repo_path.join("examples"))
        .expect("list examples")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect::<Vec<_>>();
    assert!(
        examples
            .iter()
            .any(|entry| entry.to_string_lossy() == "github-filesystem")
    );
    assert!(
        examples
            .iter()
            .any(|entry| entry.to_string_lossy() == "google-drive")
    );

    let missing = std::fs::read(repo_path.join("fsk-live-mount-missing-file"));
    assert_eq!(
        missing.expect_err("missing files must fail").kind(),
        std::io::ErrorKind::NotFound
    );

    drop(session);
}
