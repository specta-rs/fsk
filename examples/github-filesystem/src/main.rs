#![doc(
    html_logo_url = "https://github.com/specta-rs/fsk/raw/main/assets/app-icon-light.svg",
    html_favicon_url = "https://github.com/specta-rs/fsk/raw/main/assets/app-icon-light.svg"
)]

mod fs;

use fsk::MountSession;
use std::{
    env, io,
    path::PathBuf,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

fn main() -> io::Result<()> {
    let config = Config::parse()?;
    authorize()?;
    let filesystem = fs::GitHubFilesystem::new(fs::Options {
        api_url: config.api_url,
        token: config.token,
        cache_dir: config.cache_dir,
        max_cache_bytes: config.max_cache_bytes,
        max_idle: config.max_idle,
        download_on_list: config.download_on_list,
    })
    .map_err(to_io)?;
    let session = MountSession::mount_network(filesystem, &config.mount_path)?;
    println!(
        "mounted GitHub at {} — press Ctrl-C to unmount",
        session.mount_path().display()
    );

    let running = Arc::new(AtomicBool::new(true));
    let signal_state = Arc::clone(&running);
    ctrlc::set_handler(move || signal_state.store(false, Ordering::Release))
        .map_err(io::Error::other)?;
    while running.load(Ordering::Acquire) {
        thread::park_timeout(Duration::from_millis(250));
    }
    drop(session);
    Ok(())
}

/// Checks the platform requirement before mounting.
///
/// `FSKit` approval, FUSE, and the Projected File System optional component are
/// host setup that Cargo cannot install. `requirement()` names the next step;
/// `ensure_authorization()` opens the platform setup UI when one exists and
/// returns the actionable error that `main` prints.
fn authorize() -> io::Result<()> {
    match fsk::authorization_status()? {
        fsk::AuthorizationStatus::Authorized => Ok(()),
        status => {
            if let Some(requirement) = status.requirement() {
                eprintln!("setup required: {requirement}");
            }
            fsk::ensure_authorization()
        }
    }
}

struct Config {
    mount_path: PathBuf,
    api_url: String,
    token: String,
    cache_dir: PathBuf,
    max_cache_bytes: u64,
    max_idle: Duration,
    download_on_list: bool,
}

impl Config {
    fn parse() -> io::Result<Self> {
        let mut args = env::args_os().skip(1);
        let mount_path = args.next().map(PathBuf::from).ok_or_else(usage)?;
        let mut api_url = "https://api.github.com".to_owned();
        let mut cache_dir =
            env::var_os("GHFS_CACHE_DIR").map_or_else(default_cache_dir, PathBuf::from);
        let mut max_cache_bytes = env_u64("GHFS_MAX_CACHE_BYTES")?.unwrap_or(1 << 30);
        let mut max_idle_secs = env_u64("GHFS_MAX_IDLE_SECS")?.unwrap_or(7 * 24 * 60 * 60);
        let mut download_on_list = false;
        while let Some(arg) = args.next() {
            let value = arg.to_string_lossy();
            match value.as_ref() {
                "--api-url" => api_url = next_string(&mut args, "--api-url")?,
                "--cache-dir" => cache_dir = PathBuf::from(next_value(&mut args, "--cache-dir")?),
                "--max-cache-bytes" => max_cache_bytes = next_u64(&mut args, "--max-cache-bytes")?,
                "--max-idle-secs" => max_idle_secs = next_u64(&mut args, "--max-idle-secs")?,
                "--download-on-list" => download_on_list = true,
                "--help" | "-h" => return Err(usage()),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown option: {value}"),
                    ));
                }
            }
        }
        let token = env::var("GITHUB_TOKEN")
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "GITHUB_TOKEN is required"))?;
        Ok(Self {
            mount_path,
            api_url,
            token,
            cache_dir,
            max_cache_bytes,
            max_idle: Duration::from_secs(max_idle_secs),
            download_on_list,
        })
    }
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: fsk-github-filesystem <mount-path> [--cache-dir PATH] [--max-cache-bytes BYTES] [--max-idle-secs SECONDS] [--download-on-list] [--api-url URL]",
    )
}

fn next_value(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &str,
) -> io::Result<std::ffi::OsString> {
    args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag} requires a value"),
        )
    })
}

fn next_string(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &str,
) -> io::Result<String> {
    Ok(next_value(args, flag)?.to_string_lossy().into_owned())
}

fn next_u64(args: &mut impl Iterator<Item = std::ffi::OsString>, flag: &str) -> io::Result<u64> {
    next_string(args, flag)?.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag} requires an integer"),
        )
    })
}

fn env_u64(name: &str) -> io::Result<Option<u64>> {
    env::var(name)
        .ok()
        .map(|value| {
            value.parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name} must be an integer"),
                )
            })
        })
        .transpose()
}

fn default_cache_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(root) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(root).join("fsk-github-filesystem");
    }
    #[cfg(target_os = "macos")]
    if let Some(root) = env::var_os("HOME") {
        return PathBuf::from(root)
            .join("Library")
            .join("Caches")
            .join("fsk-github-filesystem");
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(root) = env::var_os("XDG_CACHE_HOME") {
            return PathBuf::from(root).join("fsk-github-filesystem");
        }
        if let Some(root) = env::var_os("HOME") {
            return PathBuf::from(root)
                .join(".cache")
                .join("fsk-github-filesystem");
        }
    }
    env::temp_dir().join("fsk-github-filesystem")
}

fn to_io(error: fsk::Error) -> io::Error {
    io::Error::from_raw_os_error(error.0)
}
#[cfg(all(target_os = "macos", test))]
#[link(name = "sandbox")]
unsafe extern "C" {
    fn sandbox_init(
        profile: *const std::ffi::c_char,
        flags: u64,
        error: *mut *mut std::ffi::c_char,
    ) -> i32;
    fn sandbox_free_error(error: *mut std::ffi::c_char);
    #[link_name = "open"]
    fn probe_open(path: *const std::ffi::c_char, flags: i32, ...) -> i32;
    #[link_name = "read"]
    fn probe_read(fd: i32, buffer: *mut std::ffi::c_void, count: usize) -> isize;
    #[link_name = "close"]
    fn probe_close(fd: i32) -> i32;
}

#[cfg(all(target_os = "macos", test))]
struct TemporaryHome(Option<std::ffi::OsString>);

#[cfg(all(target_os = "macos", test))]
impl TemporaryHome {
    fn new(path: &std::path::Path) -> Self {
        let previous = env::var_os("HOME");
        unsafe { env::set_var("HOME", path) };
        Self(previous)
    }
}

#[cfg(all(target_os = "macos", test))]
impl Drop for TemporaryHome {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            unsafe { env::set_var("HOME", previous) };
        } else {
            unsafe { env::remove_var("HOME") };
        }
    }
}

#[cfg(all(target_os = "macos", test))]
fn sandbox_read_status(
    path: &std::path::Path,
    allowed_root: &std::path::Path,
    expected: &[u8],
) -> i32 {
    std::process::Command::new(env::current_exe().expect("test executable"))
        .args(["--exact", "sandbox_read_probe"])
        .env("FSK_SANDBOX_READ_PATH", path)
        .env("FSK_SANDBOX_ALLOWED_ROOT", allowed_root)
        .env(
            "FSK_SANDBOX_EXPECTED",
            std::str::from_utf8(expected).expect("UTF-8 probe contents"),
        )
        .status()
        .expect("run sandbox read probe")
        .code()
        .unwrap_or(99)
}

#[cfg(all(target_os = "macos", test))]
#[test]
fn sandbox_read_probe() {
    let Some(path) = env::var_os("FSK_SANDBOX_READ_PATH") else {
        return;
    };
    let allowed_root = env::var_os("FSK_SANDBOX_ALLOWED_ROOT").expect("allowed sandbox root");
    let expected = env::var("FSK_SANDBOX_EXPECTED").expect("expected probe contents");
    let allowed_root =
        std::fs::canonicalize(PathBuf::from(allowed_root)).expect("canonical allowed sandbox root");
    let path = std::fs::canonicalize(PathBuf::from(path)).expect("canonical probe path");
    let path =
        std::ffi::CString::new(path.to_str().expect("UTF-8 probe path")).expect("probe path");
    let expected = expected.into_bytes();
    let mut buffer = [0_u8; 256];
    let root = allowed_root
        .to_str()
        .expect("UTF-8 sandbox root")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let profile = std::ffi::CString::new(format!(
        "(version 1) (deny default) (allow file-read* (subpath \"{root}\"))"
    ))
    .expect("sandbox profile");
    let mut error = std::ptr::null_mut();
    let status = unsafe { sandbox_init(profile.as_ptr(), 0, &raw mut error) };
    if status != 0 {
        let message = if error.is_null() {
            "unknown sandbox_init error".to_owned()
        } else {
            let message = unsafe { std::ffi::CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned();
            unsafe { sandbox_free_error(error) };
            message
        };
        panic!("sandbox_init failed: {message}");
    }

    let fd = unsafe { probe_open(path.as_ptr(), 0) };
    if fd < 0 {
        let errno = std::io::Error::last_os_error().raw_os_error();
        std::process::exit(if errno == Some(1) { 10 } else { 12 });
    }
    let count = unsafe { probe_read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    unsafe { probe_close(fd) };
    if count < 0 {
        std::process::exit(12);
    }
    let count = usize::try_from(count).expect("read count fits usize");
    if buffer[..count] == expected {
        std::process::exit(0);
    }
    std::process::exit(11);
}

#[cfg(all(target_os = "macos", test))]
#[test]
fn default_cache_local_open_is_denied_but_app_group_open_succeeds() {
    let temporary_home = tempfile::tempdir().expect("temporary HOME");
    let _home = TemporaryHome::new(temporary_home.path());
    let legacy_default = temporary_home
        .path()
        .join("Library/Caches/fsk-github-filesystem");
    let app_group_root = temporary_home
        .path()
        .join("Library/Group Containers/group.dev.specta.fsk");
    let app_group_default = app_group_root.join("Caches/fsk-github-filesystem");
    let contents = b"cached bytes";
    for directory in [&legacy_default, &app_group_default] {
        std::fs::create_dir_all(directory).expect("create cache directory");
        std::fs::write(directory.join("file.blob"), contents).expect("write cached file");
    }

    let selected_default = default_cache_dir();
    let legacy_status =
        sandbox_read_status(&legacy_default.join("file.blob"), &app_group_root, contents);
    let app_group_status = sandbox_read_status(
        &app_group_default.join("file.blob"),
        &app_group_root,
        contents,
    );
    println!(
        "legacy default sandbox read exit: {legacy_status}; App Group sandbox read exit: {app_group_status}"
    );

    assert_eq!(legacy_status, 10, "legacy cache open must fail with EPERM");
    assert_eq!(app_group_status, 0, "App Group cache open must succeed");
    assert_eq!(
        selected_default, app_group_default,
        "local-file leases must use the readable App Group container"
    );
}
