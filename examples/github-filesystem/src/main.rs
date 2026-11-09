mod fs;

use fsk::MountSession;
#[cfg(target_os = "macos")]
use fsk::{AuthorizationStatus, authorization_status, launch_app};
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
    check_platform_authorization()?;
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

#[cfg(target_os = "macos")]
fn check_platform_authorization() -> io::Result<()> {
    match authorization_status()? {
        AuthorizationStatus::Authorized => Ok(()),
        AuthorizationStatus::NeedsApproval => {
            launch_app()?;
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "enable FSK in the window that opened, then run this command again",
            ))
        }
        AuthorizationStatus::NotInstalled => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "install FSK.app in /Applications before mounting",
        )),
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)]
fn check_platform_authorization() -> io::Result<()> {
    Ok(())
}
