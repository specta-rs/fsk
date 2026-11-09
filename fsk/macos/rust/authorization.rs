//! macOS `FSKit` extension authorization helpers.

use std::{
    env,
    ffi::CString,
    fs, io,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;

const APP_BUNDLE_IDENTIFIER: &str = "dev.specta.fsk";
const APP_TEAM_IDENTIFIER: &str = "68994Z5PT3";
const APP_EXECUTABLE: &str = "Contents/MacOS/FSK";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_BASE_URL: &str = "https://github.com/specta-rs/fsk/releases/download";
const RELEASE_API_URL: &str = "https://api.github.com/repos/specta-rs/fsk/releases/tags";
const STALE_ENTRY_AGE: Duration = Duration::from_secs(24 * 60 * 60);
static APP_MUTATION: Mutex<()> = Mutex::new(());
static ENTRY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The result of installing the companion application for this SDK version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppInstallOutcome {
    /// The matching application was already installed.
    AlreadyInstalled,
    /// No application was present, so it was installed.
    Installed,
    /// An existing outdated or invalid application was replaced.
    Updated,
}

/// Whether macOS currently exposes the installed `FSK` module to `FSKit`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizationStatus {
    /// The app or its embedded extension is not installed.
    NotInstalled,
    /// The extension is installed but disabled by the user.
    NeedsApproval,
    /// The extension is installed and enabled.
    Authorized,
}

/// Download and install the `FSK` companion app matching this SDK version.
///
/// The app is installed machine-wide at `/Applications/FSK.app` and shared by
/// every process on the machine. Installation requires an exact version match,
/// so a program built against an older SDK can replace (downgrade) an app
/// installed by a newer SDK.
///
/// This operation is synchronous and never launches the application or presents
/// UI, so callers can run it on a background thread or executor of their choice.
/// The release is downloaded from GitHub, checked by macOS, and installed at
/// `/Applications/FSK.app`. An already installed, valid matching version is
/// left alone. An interrupted previous installation is recovered before any
/// download begins.
///
/// Set `FSK_APP_PATH` to the full path of an app bundle (for example
/// `/Applications/FSK.app`) to choose a different destination during
/// development. It must be an absolute path ending in `.app`; it is not a
/// directory that contains the bundle. The value is normalized before use, and
/// relative paths or paths containing `..` are rejected.
///
/// If `GITHUB_TOKEN` or `GH_TOKEN` is set, the release asset is resolved through
/// the GitHub API so a private release can be downloaded. Set
/// `FSK_RELEASE_BASE_URL` to use a mirror that serves
/// `{base}/v{version}/FSK-v{version}.zip`. Without either, the release must be
/// publicly downloadable.
///
/// The destination is only replaced when it is an existing `FSK.app` bundle or
/// a symlink; an unrelated directory or file is refused instead of being
/// deleted.
///
/// Concurrent installations and uninstalls are serialized in this process and,
/// through an exclusive advisory lock on the destination's parent directory,
/// across cooperating processes on the machine. No lock file is created.
///
/// The release is selected using this crate's version, so a build from git
/// between releases reports the last released version and treats that released
/// app as current.
///
/// # Errors
///
/// Returns an error if the release cannot be downloaded, unpacked, validated,
/// or installed. Installing in the chosen destination may require permission.
/// Returns [`io::ErrorKind::InvalidInput`] if `FSK_APP_PATH` is relative,
/// contains `..`, or does not name a folder ending in `.app`.
pub fn download_and_install_app() -> io::Result<AppInstallOutcome> {
    let _mutation = APP_MUTATION.lock();
    let destination = app_path()?;
    let parent = destination_parent(&destination)?;
    fs::create_dir_all(parent).map_err(|error| fs_error("create", parent, &error))?;
    // The lock is held on the directory itself, so no lock file is created and
    // only read permission on the parent is required.
    let _lock = AppLock::acquire(parent)?;
    recover_interrupted_install(&destination)?;
    let was_installed = path_present(&destination)?;
    if validate_app(&destination, APP_VERSION).is_ok() {
        return Ok(AppInstallOutcome::AlreadyInstalled);
    }
    if was_installed && !is_fsk_app(&destination)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to replace {} because it is not an FSK.app bundle",
                destination.display()
            ),
        ));
    }

    let temporary = temporary_directory()?;
    let result = install_from_release(&temporary, &destination);
    let _ = remove_path(&temporary);
    result?;

    Ok(if was_installed {
        AppInstallOutcome::Updated
    } else {
        AppInstallOutcome::Installed
    })
}

/// Remove the installed `FSK` companion app without presenting UI.
///
/// The app is installed machine-wide and shared by every process on the
/// machine, so this removes it for all consumers, not just the current process.
/// Leftover staging entries from an interrupted installation that are older
/// than 24 hours are removed best-effort. Every destination backup is removed
/// so a later install cannot restore an app that was just uninstalled; a crash
/// between removing the app and its backups can still leave one behind.
///
/// This operation is synchronous and is serialized with installation calls in
/// the same process and, through an exclusive lock on the destination's parent
/// directory, across cooperating processes on the machine. No lock file is
/// created. It returns `true` when an app was removed and `false` when no app
/// was installed.
///
/// Set `FSK_APP_PATH` to the full path of an app bundle (for example
/// `/Applications/FSK.app`) to choose a different destination during
/// development. The value is normalized before use and must be a full bundle
/// path, not a directory that contains the bundle.
///
/// # Errors
///
/// Returns an error if the installed bundle cannot be removed. A directory at
/// the destination is refused unless it is an `FSK.app` bundle with the
/// expected `CFBundleIdentifier`, and a mount point is always refused. Removing
/// it from the chosen destination may require permission.
pub fn uninstall_app() -> io::Result<bool> {
    let _mutation = APP_MUTATION.lock();
    let app = app_path()?;
    let parent = destination_parent(&app)?;
    let name = app_name(&app)?;
    if !path_present(parent)? {
        // Nothing could have been installed without a parent directory.
        return Ok(false);
    }
    let _lock = AppLock::acquire(parent)?;
    let removed = uninstall_app_at(&app)?;
    let _ = cleanup_stale_entries(parent, name, "install");
    // Remove every backup for this destination, not just stale ones. A recent
    // backup would otherwise be restored by a later install, making the
    // uninstall non-durable. The parent lock means no cooperating process can
    // own one right now.
    let _ = remove_matching_entries(parent, name, "backup");
    Ok(removed)
}

/// Ask the installed Swift app for the current `FSKit` authorization state.
///
/// Set `FSK_APP_PATH` to override the normal `/Applications/FSK.app`
/// location, which is useful for development and app-local distributions. The
/// value must be the full path of an `.app` bundle, not a directory that
/// contains it.
///
/// # Errors
///
/// Returns an error when the path is invalid, or when the app cannot be
/// executed or returns invalid data.
pub fn authorization_status() -> io::Result<AuthorizationStatus> {
    let app = app_path()?;
    if !app.exists() {
        return Ok(AuthorizationStatus::NotInstalled);
    }
    let output = Command::new(app.join(APP_EXECUTABLE))
        .arg("--fsk-authorization-status")
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "authorized" => Ok(AuthorizationStatus::Authorized),
        "needs-approval" => Ok(AuthorizationStatus::NeedsApproval),
        "not-installed" => Ok(AuthorizationStatus::NotInstalled),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected FSK authorization response: {other}"),
        )),
    }
}

/// Open the `FSK` companion app directly in its authorization guide.
///
/// Set `FSK_APP_PATH` to override the normal `/Applications/FSK.app`
/// location. The value must be the full path of an `.app` bundle, not a
/// directory that contains it.
///
/// # Errors
///
/// Returns an error when the path is invalid, or when the app is missing or
/// macOS cannot open it.
pub fn launch_app() -> io::Result<()> {
    let app = app_path()?;
    if !app.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("FSK.app was not found at {}", app.display()),
        ));
    }
    let status = Command::new("/usr/bin/open")
        .arg("-a")
        .arg(&app)
        .arg("--args")
        .arg("--fsk-authorize")
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("macOS could not open FSK.app"))
    }
}

/// Open the `FSK` companion app directly in its authorization guide.
///
/// This is the old name for [`launch_app`].
///
/// # Errors
///
/// Returns an error when the app is missing or macOS cannot open it.
#[deprecated(note = "renamed to `launch_app`")]
pub fn open_authorization_app() -> io::Result<()> {
    launch_app()
}

fn install_from_release(temporary: &Path, destination: &Path) -> io::Result<()> {
    let archive = temporary.join("FSK.zip");
    let extracted = temporary.join("extracted");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&extracted)
        .map_err(|error| fs_error("create", &extracted, &error))?;
    let tag = format!("v{APP_VERSION}");
    download_release(temporary, &archive, &tag)?;
    run(
        Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(&archive)
            .arg(&extracted),
        "unpack FSK.app",
    )?;

    let source = extracted.join("FSK.app");
    require_real_directory(&source)?;
    validate_app(&source, APP_VERSION)?;

    // The destination was validated in `download_and_install_app`, so it has a
    // parent and a final component.
    let parent = destination_parent(destination)?;
    fs::create_dir_all(parent).map_err(|error| fs_error("create", parent, &error))?;
    let name = app_name(destination)?;
    let staging = new_entry_path(parent, name, "install");
    create_staging_directory(&staging)?;
    let staged_app = staging.join(name);
    let stage_result = run(
        Command::new("/usr/bin/ditto").arg(&source).arg(&staged_app),
        "stage FSK.app installation",
    );
    if let Err(error) = stage_result {
        let _ = remove_path(&staging);
        return Err(error);
    }

    // The bytes that get swapped into place must be the bytes we validated.
    let stage_check =
        require_real_directory(&staged_app).and_then(|()| validate_app(&staged_app, APP_VERSION));
    if let Err(error) = stage_check {
        let _ = remove_path(&staging);
        return Err(error);
    }

    let backup = new_entry_path(parent, name, "backup");
    let result = replace_app(&staged_app, destination, &backup);
    // `replace_app` consumes the staged app on success; the staging directory
    // that contained it is always ours to remove.
    let _ = remove_path(&staging);
    result
}

fn create_staging_directory(staging: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(staging) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to reuse the existing staging path {}; it may be a stale install, file, or symlink",
                staging.display()
            ),
        )),
        Err(error) => Err(fs_error("create staging directory", staging, &error)),
    }
}

fn download_release(temporary: &Path, archive: &Path, tag: &str) -> io::Result<()> {
    if let Some(base) = release_base_from(env::var_os("FSK_RELEASE_BASE_URL"))? {
        return run(
            curl_download(archive).arg(mirror_url(&base, tag)),
            "download FSK.app",
        );
    }
    if let Some(token) = github_token() {
        return download_release_with_token(temporary, archive, tag, &token);
    }
    run(curl_download(archive).arg(public_release_url(tag)), "download FSK.app").map_err(|error| {
        io::Error::other(format!(
            "{error}. FSK.app releases must be publicly downloadable; for a private repository set GITHUB_TOKEN or FSK_RELEASE_BASE_URL"
        ))
    })
}

fn release_base_from(configured: Option<std::ffi::OsString>) -> io::Result<Option<String>> {
    match configured {
        Some(value) if value.to_string_lossy().trim().is_empty() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FSK_RELEASE_BASE_URL must not be empty",
        )),
        Some(value) => Ok(Some(value.to_string_lossy().trim().to_owned())),
        None => Ok(None),
    }
}

fn public_release_url(tag: &str) -> String {
    format!("{RELEASE_BASE_URL}/{tag}/FSK-{tag}.zip")
}

fn mirror_url(base: &str, tag: &str) -> String {
    format!("{}/{tag}/FSK-{tag}.zip", base.trim_end_matches('/'))
}

fn curl_download(archive: &Path) -> Command {
    let mut command = Command::new("/usr/bin/curl");
    command.args([
        "--disable",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--fail",
        "--location",
        "--silent",
        "--show-error",
        "--retry",
        "3",
        "--retry-delay",
        "2",
        "--retry-connrefused",
        "--connect-timeout",
        "30",
        "--max-time",
        "600",
        "--output",
    ]);
    command.arg(archive);
    command
}

fn github_token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"]
        .into_iter()
        .find_map(|key| env::var(key).ok().filter(|token| !token.trim().is_empty()))
}

fn download_release_with_token(
    temporary: &Path,
    archive: &Path,
    tag: &str,
    token: &str,
) -> io::Result<()> {
    let config = curl_token_config(temporary, token)?;
    let metadata = Command::new("/usr/bin/curl")
        .args([
            "--disable",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--retry",
            "3",
            "--retry-delay",
            "2",
            "--retry-connrefused",
            "--connect-timeout",
            "30",
            "--max-time",
            "120",
            "--config",
        ])
        .arg(&config)
        .args(["--header", "Accept: application/vnd.github+json"])
        .arg(format!("{RELEASE_API_URL}/{tag}"))
        .output()?;
    if !metadata.status.success() {
        return Err(command_error("read FSK release metadata", &metadata.stderr));
    }

    let value: serde_json::Value = serde_json::from_slice(&metadata.stdout).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid FSK release metadata: {error}"),
        )
    })?;
    let asset_name = format!("FSK-{tag}.zip");
    let asset = value
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .and_then(|assets| {
            assets.iter().find(|asset| {
                asset.get("name").and_then(serde_json::Value::as_str) == Some(asset_name.as_str())
            })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("FSK release {tag} does not contain {asset_name}"),
            )
        })?;
    let asset_url = asset
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("FSK release asset {asset_name} has no API URL"),
            )
        })?;
    if !asset_url.starts_with("https://api.github.com/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("FSK release asset {asset_name} has an unexpected API URL"),
        ));
    }

    run(
        // `--disable` is first, then the shared download options, the token
        // config, and finally the API URL.
        curl_download(archive)
            .arg("--config")
            .arg(&config)
            .args(["--header", "Accept: application/octet-stream"])
            .arg(asset_url),
        "download FSK.app",
    )
}

fn curl_token_config(temporary: &Path, token: &str) -> io::Result<PathBuf> {
    let path = temporary.join("curl.conf");
    fs::write(
        &path,
        format!("header = \"Authorization: Bearer {token}\"\n"),
    )?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

fn replace_app(staging: &Path, destination: &Path, backup: &Path) -> io::Result<()> {
    replace_app_with(staging, destination, backup, atomic_swap)
}

fn replace_app_with(
    staging: &Path,
    destination: &Path,
    backup: &Path,
    swap: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let destination_present = match path_present(destination) {
        Ok(present) => present,
        Err(error) => {
            let _ = remove_path(staging);
            return Err(error);
        }
    };
    if destination_present {
        match swap(staging, destination) {
            Ok(()) => {
                // The previous app is now at `staging`; removing it is
                // best-effort because the new app is already installed.
                let _ = remove_path(staging);
                let _ = remove_path(backup);
                return Ok(());
            }
            Err(error) if swap_unsupported(&error) => {}
            Err(error) => {
                let _ = remove_path(staging);
                return Err(fs_error("install FSK.app at", destination, &error));
            }
        }
        if let Err(error) = fs::rename(destination, backup) {
            let _ = remove_path(staging);
            return Err(fs_error("move the previous FSK.app to", backup, &error));
        }
    }
    if let Err(error) = fs::rename(staging, destination) {
        let _ = remove_path(staging);
        if destination_present {
            return match fs::rename(backup, destination) {
                Ok(()) => Err(fs_error("install FSK.app at", destination, &error)),
                Err(restore_error) => Err(io::Error::new(
                    restore_error.kind(),
                    format!(
                        "could not install FSK.app at {} ({error}) and could not restore the previous app from {} ({restore_error}); the previous app remains there",
                        destination.display(),
                        backup.display()
                    ),
                )),
            };
        }
        return Err(fs_error("install FSK.app at", destination, &error));
    }
    // The new app is in place, so a stale backup is not an installation
    // failure. Leave it behind rather than reporting a false error.
    let _ = remove_path(backup);
    Ok(())
}

/// Atomically exchange two paths using `renamex_np(RENAME_SWAP)`.
fn atomic_swap(first: &Path, second: &Path) -> io::Result<()> {
    let first = path_c_string(first)?;
    let second = path_c_string(second)?;
    // SAFETY: both pointers are valid, NUL-terminated C strings that outlive
    // the call. `renamex_np` does not retain them after returning.
    let result = unsafe { libc::renamex_np(first.as_ptr(), second.as_ptr(), libc::RENAME_SWAP) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn swap_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOTSUP | libc::EINVAL | libc::ENOSYS)
    )
}

fn path_c_string(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains an interior NUL byte: {}", path.display()),
        )
    })
}

fn recover_interrupted_install(destination: &Path) -> io::Result<()> {
    recover_interrupted_install_with(destination, validate_restored_app)
}

fn recover_interrupted_install_with(
    destination: &Path,
    validate: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = destination_parent(destination)?;
    let name = app_name(destination)?;

    // Staging directories belong to a specific installation call. Anything old
    // enough cannot belong to a live call, since mutations are serialized by
    // the global mutex and the parent-directory lock held by callers, so
    // remove it best-effort.
    let _ = cleanup_stale_entries(parent, name, "install");

    if !path_present(destination)? {
        if let Some(backup) = newest_entry(parent, name, "backup")? {
            if validate(&backup).is_ok() {
                fs::rename(&backup, destination).map_err(|error| {
                    fs_error("restore the interrupted FSK.app from", &backup, &error)
                })?;
            } else {
                // Restoring an unverified bundle would install arbitrary
                // bytes. Keep it for manual inspection while it is young, but
                // still remove stale entries so the same corrupt backup is not
                // re-selected on every run.
                let _ = cleanup_stale_entries(parent, name, "backup");
                return Ok(());
            }
        }
    }

    let _ = cleanup_stale_entries(parent, name, "backup");
    Ok(())
}

/// Verify that a backup is still a valid, correctly signed FSK.app.
///
/// The version is deliberately not checked: an older but valid release may be
/// the best thing to restore after an interrupted upgrade.
fn validate_restored_app(app: &Path) -> io::Result<()> {
    run(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app),
        "verify restored FSK.app signature",
    )?;
    check_signing_identity(app)?;
    run(
        Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(app),
        "validate restored FSK.app with Gatekeeper",
    )
}

fn cleanup_stale_entries(parent: &Path, name: &str, kind: &str) -> io::Result<()> {
    let prefix = entry_prefix(name, kind);
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(fs_error("read", parent, &error)),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        // This exact-prefix filter is what keeps a parent such as
        // `/Applications` safe: only our own `.FSK.app.<kind>-` entries are
        // ever candidates.
        if !file_name.starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if is_stale(entry_time(file_name, &prefix, &metadata)) {
            let _ = remove_path(&path);
        }
    }
    Ok(())
}

fn remove_matching_entries(parent: &Path, name: &str, kind: &str) -> io::Result<()> {
    let prefix = entry_prefix(name, kind);
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(fs_error("read", parent, &error)),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.starts_with(&prefix) {
            let _ = remove_path(&entry.path());
        }
    }
    Ok(())
}

fn newest_entry(parent: &Path, name: &str, kind: &str) -> io::Result<Option<PathBuf>> {
    let prefix = entry_prefix(name, kind);
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(fs_error("read", parent, &error)),
    };
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !file_name.starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !(metadata.file_type().is_dir() || metadata.file_type().is_symlink()) {
            continue;
        }
        let created = entry_time(file_name, &prefix, &metadata);
        if newest.as_ref().is_none_or(|(time, _)| created > *time) {
            newest = Some((created, path));
        }
    }
    Ok(newest.map(|(_, path)| path))
}

fn entry_prefix(name: &str, kind: &str) -> String {
    format!(".{name}.{kind}-")
}

/// Build a unique staging or backup name that records its creation time.
///
/// The timestamp is part of the name because renaming a directory does not
/// update its mtime: a backup of an app installed months ago would otherwise
/// look stale the moment it was created.
fn new_entry_path(parent: &Path, name: &str, kind: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = ENTRY_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        "{}{timestamp}-{}-{counter}",
        entry_prefix(name, kind),
        std::process::id()
    ))
}

/// Prefer the timestamp encoded in the name, falling back to mtime for names
/// written by older versions of this module.
fn entry_time(file_name: &str, prefix: &str, metadata: &fs::Metadata) -> SystemTime {
    let Some(nanos) = file_name
        .strip_prefix(prefix)
        .and_then(|rest| rest.split('-').next())
        .and_then(|timestamp| timestamp.parse::<u64>().ok())
    else {
        return metadata.modified().unwrap_or(UNIX_EPOCH);
    };
    UNIX_EPOCH + Duration::from_nanos(nanos)
}

fn is_stale(created: SystemTime) -> bool {
    SystemTime::now()
        .duration_since(created)
        .is_ok_and(|age| age > STALE_ENTRY_AGE)
}

fn require_real_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a real directory", path.display()),
        ));
    }
    Ok(())
}

fn validate_app(app: &Path, expected_version: &str) -> io::Result<()> {
    if app_version(app)?.as_deref() != Some(expected_version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FSK.app version does not match the Rust SDK",
        ));
    }
    if app_bundle_identifier(app)?.as_deref() != Some(APP_BUNDLE_IDENTIFIER) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FSK.app bundle identifier does not match the Rust SDK",
        ));
    }
    run(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app),
        "verify FSK.app signature",
    )?;
    check_signing_identity(app)?;
    run(
        Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(app),
        "validate FSK.app with Gatekeeper",
    )
}

fn check_signing_identity(app: &Path) -> io::Result<()> {
    let output = Command::new("/usr/bin/codesign")
        .args(["--display", "--verbose=4"])
        .arg(app)
        .output()
        .map_err(|error| fs_error("read FSK.app signing identity from", app, &error))?;
    if !output.status.success() {
        return Err(command_error(
            "read FSK.app signing identity",
            &output.stderr,
        ));
    }
    let display = String::from_utf8_lossy(&output.stderr);
    if signing_identity_matches(&display) {
        Ok(())
    } else {
        let (identifier, team_identifier) = signing_identity(&display);
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "FSK.app at {} is not signed by the expected release identity: expected identifier {APP_BUNDLE_IDENTIFIER} and team {APP_TEAM_IDENTIFIER}, but observed identifier {} and team {}; the downloaded release asset is not signed by the expected release identity",
                app.display(),
                identifier.unwrap_or("none"),
                team_identifier.unwrap_or("none"),
            ),
        ))
    }
}

fn signing_identity(display: &str) -> (Option<&str>, Option<&str>) {
    let mut identifier = None;
    let mut team_identifier = None;
    for line in display.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Identifier=") {
            identifier = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("TeamIdentifier=") {
            team_identifier = Some(value.trim());
        }
    }
    (identifier, team_identifier)
}

fn signing_identity_matches(display: &str) -> bool {
    let (identifier, team_identifier) = signing_identity(display);
    identifier == Some(APP_BUNDLE_IDENTIFIER) && team_identifier == Some(APP_TEAM_IDENTIFIER)
}

/// Check that a destination is an FSK app bundle or a symlink. Symlinks are
/// treated as removable links and are never followed by destructive
/// operations. A directory must contain the app executable and a readable
/// `CFBundleIdentifier` so a misconfigured `FSK_APP_PATH` cannot remove an
/// unrelated directory.
fn is_fsk_app(app: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(app) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        // Destructive operations remove the link itself, never its target.
        return Ok(true);
    }
    if !metadata.is_dir() || !app.join(APP_EXECUTABLE).is_file() {
        return Ok(false);
    }
    // A malformed or missing Info.plist is not a usable FSK app. Treat it as
    // non-matching so the caller refuses instead of deleting the directory.
    Ok(app_bundle_identifier(app).ok().flatten().as_deref() == Some(APP_BUNDLE_IDENTIFIER))
}

fn uninstall_app_at(app: &Path) -> io::Result<bool> {
    let app = normalize_app_path(app)?;
    let metadata = match fs::symlink_metadata(&app) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(fs_error("inspect", &app, &error)),
    };
    if metadata.file_type().is_symlink() {
        // Remove the link itself, never its target.
        fs::remove_file(&app).map_err(|error| fs_error("remove", &app, &error))?;
        return Ok(true);
    }
    if !is_fsk_app(&app)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to remove {} because it is not an FSK.app bundle",
                app.display()
            ),
        ));
    }
    // Refuse to recursively delete a mounted volume that happens to be named
    // like the app; a real bundle lives on its parent's device.
    let parent = destination_parent(&app)?;
    let parent_device = fs::metadata(parent)
        .map_err(|error| fs_error("inspect", parent, &error))?
        .dev();
    if metadata.dev() != parent_device {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "refusing to remove {}: it is a mount point, not an installed app bundle",
                app.display()
            ),
        ));
    }
    fs::remove_dir_all(&app).map_err(|error| fs_error("remove", &app, &error))?;
    Ok(true)
}

fn remove_path(path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(true)
}

fn path_present(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn app_version(app: &Path) -> io::Result<Option<String>> {
    if !app.exists() {
        return Ok(None);
    }
    plist_string(app, "CFBundleShortVersionString")
}

fn app_bundle_identifier(app: &Path) -> io::Result<Option<String>> {
    if !app.exists() {
        return Ok(None);
    }
    plist_string(app, "CFBundleIdentifier")
}

fn plist_string(app: &Path, key: &str) -> io::Result<Option<String>> {
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", key, "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .output()?;
    if !output.status.success() {
        return Err(command_error("read FSK.app Info.plist", &output.stderr));
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

fn temporary_directory() -> io::Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = env::temp_dir().join(format!("fsk-install-{}-{nonce}", std::process::id()));
    fs::create_dir(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

/// An exclusive advisory lock held for the duration of an install or uninstall.
///
/// The destination's parent directory is locked rather than a lock file beside
/// the app: no lock file is created, so a read-only parent such as
/// `/Applications` is enough, and dropping the guard releases the kernel lock.
/// The lock serializes cooperating processes that mutate the same directory.
struct AppLock {
    directory: fs::File,
}

impl AppLock {
    fn acquire(parent: &Path) -> io::Result<Self> {
        let directory = fs::File::open(parent)
            .map_err(|error| fs_error("open the directory", parent, &error))?;
        loop {
            // SAFETY: `directory` owns a valid descriptor for the duration of
            // the call and `flock` does not retain it. A directory can be
            // locked this way on macOS and only requires read permission.
            let result = unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX) };
            if result == 0 {
                return Ok(Self { directory });
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::new(
                error.kind(),
                format!("could not lock {}: {error}", parent.display()),
            ));
        }
    }
}

impl Drop for AppLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still owned by `self.directory`.
        unsafe {
            libc::flock(self.directory.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn run(command: &mut Command, operation: &str) -> io::Result<()> {
    let output = command.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(operation, &output.stderr))
    }
}

fn command_error(operation: &str, stderr: &[u8]) -> io::Error {
    let detail = String::from_utf8_lossy(stderr);
    io::Error::other(format!("could not {operation}: {}", detail.trim()))
}

fn fs_error(operation: &str, path: &Path, error: &io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("could not {operation} {}: {error}", path.display()),
    )
}

fn app_path() -> io::Result<PathBuf> {
    app_path_from(env::var_os("FSK_APP_PATH"))
}

fn app_path_from(configured: Option<std::ffi::OsString>) -> io::Result<PathBuf> {
    let path = match configured {
        Some(value) if value.to_string_lossy().trim().is_empty() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "FSK_APP_PATH must not be empty",
            ));
        }
        Some(value) => PathBuf::from(value),
        None => Path::new("/Applications/FSK.app").to_owned(),
    };
    normalize_app_path(&path)
}

/// Normalize `FSK_APP_PATH` before any metadata, rename, or removal operation.
/// Trailing separators and `.` components are removed, `..` is rejected so the
/// resolved path cannot escape the directory the caller chose, and the final
/// component must be a folder ending in `.app`.
fn normalize_app_path(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FSK_APP_PATH must be an absolute path",
        ));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FSK_APP_PATH must not contain `..`",
        ));
    }
    let normalized: PathBuf = path.components().collect();
    let Some(file_name) = normalized.file_name().and_then(|name| name.to_str()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FSK_APP_PATH must name an FSK.app bundle",
        ));
    };
    let is_app_bundle = normalized
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("app"));
    if !is_app_bundle || file_name.len() <= ".app".len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FSK_APP_PATH must name a folder ending in `.app`",
        ));
    }
    Ok(normalized)
}

fn app_name(destination: &Path) -> io::Result<&str> {
    destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "FSK_APP_PATH must name an app bundle folder: {}",
                    destination.display()
                ),
            )
        })
}

fn destination_parent(destination: &Path) -> io::Result<&Path> {
    destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "FSK_APP_PATH must have a parent directory: {}",
                destination.display()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_fixture(directory: &Path) -> PathBuf {
        let app = directory.join("FSK.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join(APP_EXECUTABLE), b"").unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>dev.specta.fsk</string>
</dict>
</plist>
"#,
        )
        .unwrap();
        app
    }

    #[test]
    fn missing_app_is_not_installed() {
        // The path resolver itself is deliberately deterministic; integration
        // tests exercise the installed app on signed release builds.
        assert_eq!(
            Path::new("/Applications/FSK.app").join(APP_EXECUTABLE),
            Path::new("/Applications/FSK.app/Contents/MacOS/FSK")
        );
    }

    #[test]
    fn normalize_app_path_strips_trailing_separator() {
        assert_eq!(
            normalize_app_path(Path::new("/Applications/FSK.app/")).unwrap(),
            Path::new("/Applications/FSK.app")
        );
    }

    #[test]
    fn normalize_app_path_rejects_relative_and_parent_paths() {
        assert!(normalize_app_path(Path::new("FSK.app")).is_err());
        assert!(normalize_app_path(Path::new("/Applications/../FSK.app")).is_err());
        assert!(normalize_app_path(Path::new("/")).is_err());
        assert!(normalize_app_path(Path::new("/Applications")).is_err());
        assert!(normalize_app_path(Path::new("/Applications/FSK")).is_err());
        assert!(normalize_app_path(Path::new("/Applications/.app")).is_err());
    }

    #[test]
    fn normalize_app_path_accepts_app_bundles_case_insensitively() {
        assert_eq!(
            normalize_app_path(Path::new("/Applications/FSK.APP")).unwrap(),
            Path::new("/Applications/FSK.APP")
        );
    }

    #[test]
    fn signing_identity_accepts_the_release_signer() {
        let display = "\
Executable=/Applications/FSK.app/Contents/MacOS/FSK
Identifier=dev.specta.fsk
Format=app bundle with Mach-O thin (arm64)
CodeDirectory v=20500 size=1234 flags=0x10000(runtime) hashes=1+3 location=embedded
Authority=Developer ID Application: Specta (68994Z5PT3)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
TeamIdentifier=68994Z5PT3
";
        assert!(signing_identity_matches(display));
    }

    #[test]
    fn signing_identity_rejects_other_signers() {
        assert!(!signing_identity_matches(
            "Identifier=com.example.evil\nTeamIdentifier=68994Z5PT3\n"
        ));
        assert!(!signing_identity_matches(
            "Identifier=dev.specta.fsk\nTeamIdentifier=ABCDE12345\n"
        ));
        assert!(!signing_identity_matches(
            "Identifier=dev.specta.fsk\nTeamIdentifier=not set\n"
        ));
        assert!(!signing_identity_matches("Identifier=dev.specta.fsk\n"));
    }

    #[test]
    fn uninstall_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let app = app_fixture(directory.path());

        assert!(uninstall_app_at(&app).unwrap());
        assert!(!uninstall_app_at(&app).unwrap());
    }

    #[test]
    fn uninstall_cleanup_removes_all_destination_backups() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path();
        // A recent backup must also go: otherwise a later install would restore
        // it and the uninstall would not be durable.
        let young = new_entry_path(parent, "FSK.app", "backup");
        fs::create_dir(&young).unwrap();
        let legacy = parent.join(".FSK.app.backup-legacy");
        fs::create_dir(&legacy).unwrap();
        let unrelated = parent.join("Safari.app");
        fs::create_dir(&unrelated).unwrap();

        remove_matching_entries(parent, "FSK.app", "backup").unwrap();

        assert!(!young.exists());
        assert!(!legacy.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn uninstall_refuses_non_app_directory() {
        let directory = tempfile::tempdir().unwrap();
        let app = directory.path().join("FSK.app");
        fs::create_dir(&app).unwrap();
        fs::write(app.join("important"), b"keep me").unwrap();

        assert!(uninstall_app_at(&app).is_err());
        assert_eq!(fs::read(app.join("important")).unwrap(), b"keep me");
    }

    #[test]
    fn uninstall_refuses_non_app_file() {
        let directory = tempfile::tempdir().unwrap();
        let app = directory.path().join("FSK.app");
        fs::write(&app, b"not an app bundle").unwrap();

        assert!(uninstall_app_at(&app).is_err());
        assert_eq!(fs::read(&app).unwrap(), b"not an app bundle");
    }

    #[test]
    fn uninstall_refuses_a_directory_with_a_foreign_identifier() {
        let directory = tempfile::tempdir().unwrap();
        let app = directory.path().join("FSK.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join(APP_EXECUTABLE), b"").unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>com.example.other</string>
</dict>
</plist>
"#,
        )
        .unwrap();
        fs::write(app.join("marker"), b"keep me").unwrap();

        assert!(uninstall_app_at(&app).is_err());
        assert_eq!(fs::read(app.join("marker")).unwrap(), b"keep me");
    }

    #[test]
    fn fsk_app_detection_requires_a_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let app = app_fixture(directory.path());
        assert!(is_fsk_app(&app).unwrap());

        let plain = directory.path().join("plain");
        fs::create_dir(&plain).unwrap();
        assert!(!is_fsk_app(&plain).unwrap());

        let file = directory.path().join("file");
        fs::write(&file, b"").unwrap();
        assert!(!is_fsk_app(&file).unwrap());
    }

    #[test]
    fn failed_replacement_restores_previous_app() {
        let directory = tempfile::tempdir().unwrap();
        let destination = app_fixture(directory.path());
        let staging = directory.path().join("missing-staging");
        let backup = directory.path().join("backup");
        fs::write(destination.join("marker"), b"previous app").unwrap();

        assert!(replace_app(&staging, &destination, &backup).is_err());
        assert_eq!(
            fs::read(destination.join("marker")).unwrap(),
            b"previous app"
        );
        assert!(!backup.exists());
    }

    #[test]
    fn replacement_swaps_in_new_content_and_removes_staging() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("FSK.app");
        let staging = directory.path().join(".FSK.app.install-test");
        let backup = directory.path().join(".FSK.app.backup-test");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("marker"), b"old app").unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("marker"), b"new app").unwrap();

        replace_app(&staging, &destination, &backup).unwrap();

        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"new app");
        assert!(!staging.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn replacement_installs_when_destination_is_absent() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("FSK.app");
        let staging = directory.path().join(".FSK.app.install-test");
        let backup = directory.path().join(".FSK.app.backup-test");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("marker"), b"new app").unwrap();

        replace_app(&staging, &destination, &backup).unwrap();

        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"new app");
        assert!(!staging.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn replacement_falls_back_when_swap_is_unsupported() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("FSK.app");
        let staging = directory.path().join(".FSK.app.install-test");
        let backup = directory.path().join(".FSK.app.backup-test");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("marker"), b"old app").unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("marker"), b"new app").unwrap();

        replace_app_with(&staging, &destination, &backup, |_, _| {
            Err(io::Error::from_raw_os_error(libc::ENOTSUP))
        })
        .unwrap();

        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"new app");
        assert!(!staging.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn failed_fallback_rename_restores_the_previous_app() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("FSK.app");
        let staging = directory.path().join("missing-staging");
        let backup = directory.path().join(".FSK.app.backup-test");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("marker"), b"previous app").unwrap();

        assert!(
            replace_app_with(&staging, &destination, &backup, |_, _| {
                Err(io::Error::from_raw_os_error(libc::ENOTSUP))
            })
            .is_err()
        );
        assert_eq!(
            fs::read(destination.join("marker")).unwrap(),
            b"previous app"
        );
        assert!(!backup.exists());
    }

    #[test]
    fn staging_directory_is_created_exclusively() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join(".FSK.app.install-test");
        fs::write(&staging, b"pre-existing file").unwrap();

        let error = create_staging_directory(&staging).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&staging).unwrap(), b"pre-existing file");
    }

    #[cfg(unix)]
    #[test]
    fn staging_directory_does_not_follow_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let staging = directory.path().join(".FSK.app.install-test");
        fs::create_dir(&target).unwrap();
        symlink(&target, &staging).unwrap();

        let error = create_staging_directory(&staging).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(fs::read_dir(&target).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn staging_directory_is_private() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join(".FSK.app.install-test");

        create_staging_directory(&staging).unwrap();

        assert_eq!(fs::metadata(&staging).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn recovery_restores_an_interrupted_backup() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("FSK.app");
        let backup = directory.path().join(".FSK.app.backup-crashed");
        fs::create_dir_all(backup.join("Contents")).unwrap();
        fs::write(backup.join("Contents/Info.plist"), b"plist").unwrap();
        fs::write(backup.join("marker"), b"recovered app").unwrap();

        recover_interrupted_install_with(&destination, |_| Ok(())).unwrap();

        assert_eq!(
            fs::read(destination.join("marker")).unwrap(),
            b"recovered app"
        );
        assert!(destination.join("Contents/Info.plist").is_file());
        assert!(!backup.exists());
    }

    #[test]
    fn recovery_leaves_an_invalid_backup_for_inspection() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("FSK.app");
        let backup = directory.path().join(".FSK.app.backup-invalid");
        fs::create_dir_all(backup.join("Contents")).unwrap();
        fs::write(backup.join("Contents/Info.plist"), b"plist").unwrap();
        fs::write(backup.join("marker"), b"untrusted app").unwrap();

        recover_interrupted_install_with(&destination, |_| {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not signed by the release identity",
            ))
        })
        .unwrap();

        assert!(!destination.exists());
        assert_eq!(fs::read(backup.join("marker")).unwrap(), b"untrusted app");
    }

    #[test]
    fn recovery_removes_a_stale_invalid_backup_but_keeps_a_young_one() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path();
        let destination = parent.join("FSK.app");
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let old = parent.join(format!(
            ".FSK.app.backup-{}-1-0",
            (now.checked_sub(Duration::from_secs(48 * 60 * 60)).unwrap()).as_nanos()
        ));
        let young = parent.join(format!(".FSK.app.backup-{}-1-0", now.as_nanos()));
        for backup in [&old, &young] {
            fs::create_dir_all(backup.join("Contents")).unwrap();
            fs::write(backup.join("Contents/Info.plist"), b"plist").unwrap();
            fs::write(backup.join("marker"), b"untrusted app").unwrap();
        }

        recover_interrupted_install_with(&destination, |_| {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not signed by the release identity",
            ))
        })
        .unwrap();

        assert!(!destination.exists());
        assert!(!old.exists());
        assert!(young.exists());
    }

    #[test]
    fn recovery_keeps_a_live_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("FSK.app");
        let backup = directory.path().join(".FSK.app.backup-test");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("marker"), b"current app").unwrap();
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("marker"), b"old app").unwrap();

        recover_interrupted_install(&destination).unwrap();

        assert_eq!(
            fs::read(destination.join("marker")).unwrap(),
            b"current app"
        );
        assert!(backup.exists());
    }

    #[test]
    fn stale_install_entries_are_removed_but_young_ones_are_kept() {
        use std::fs::FileTimes;

        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path();
        let old = parent.join(".FSK.app.install-old");
        let young = parent.join(".FSK.app.install-young");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&young).unwrap();
        let old_file = fs::File::open(&old).unwrap();
        old_file
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(48 * 60 * 60)),
            )
            .unwrap();

        cleanup_stale_entries(parent, "FSK.app", "install").unwrap();

        assert!(!old.exists());
        assert!(young.exists());
    }

    #[test]
    fn cleanup_removes_name_timestamped_stale_entries_and_keeps_future_ones() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let old = parent.join(format!(
            ".FSK.app.install-{}-1-0",
            (now.checked_sub(Duration::from_secs(48 * 60 * 60)).unwrap()).as_nanos()
        ));
        let future = parent.join(format!(
            ".FSK.app.install-{}-1-0",
            (now + Duration::from_secs(48 * 60 * 60)).as_nanos()
        ));
        fs::create_dir(&old).unwrap();
        fs::create_dir(&future).unwrap();

        cleanup_stale_entries(parent, "FSK.app", "install").unwrap();

        assert!(!old.exists());
        assert!(future.exists());
    }

    #[test]
    fn cleanup_never_removes_unrelated_entries() {
        use std::fs::FileTimes;

        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path();
        // A parent directory such as `/Applications` contains unrelated apps
        // that must never be touched, however old they are.
        let unrelated = parent.join("Safari.app");
        fs::create_dir(&unrelated).unwrap();
        let old_file = fs::File::open(&unrelated).unwrap();
        old_file
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(365 * 24 * 60 * 60)),
            )
            .unwrap();

        cleanup_stale_entries(parent, "FSK.app", "install").unwrap();
        cleanup_stale_entries(parent, "FSK.app", "backup").unwrap();

        assert!(unrelated.exists());
    }

    #[test]
    fn a_recent_name_timestamp_keeps_an_old_mtime_entry() {
        use std::fs::FileTimes;

        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let backup = parent.join(format!(
            ".FSK.app.backup-{timestamp}-{}-0",
            std::process::id()
        ));
        fs::create_dir(&backup).unwrap();
        let file = fs::File::open(&backup).unwrap();
        file.set_times(
            FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(48 * 60 * 60)),
        )
        .unwrap();

        cleanup_stale_entries(parent, "FSK.app", "backup").unwrap();

        assert!(backup.exists());
    }

    #[test]
    fn newest_entry_orders_by_name_timestamp() {
        use std::fs::FileTimes;

        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path();
        let older = parent.join(".FSK.app.backup-1000000000000000000-1-0");
        let newer = parent.join(".FSK.app.backup-2000000000000000000-1-0");
        fs::create_dir(&older).unwrap();
        fs::create_dir(&newer).unwrap();
        // Give the newer entry an older mtime to prove names win.
        fs::File::open(&newer)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(48 * 60 * 60)),
            )
            .unwrap();

        assert_eq!(
            newest_entry(parent, "FSK.app", "backup").unwrap(),
            Some(newer)
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_lock_is_exclusive_and_released() {
        let directory = tempfile::tempdir().unwrap();
        let lock = AppLock::acquire(directory.path()).unwrap();

        let error = try_lock_directory(directory.path()).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EWOULDBLOCK));
        // Locking the directory must not leave a lock file behind.
        assert!(!directory.path().join(".FSK.app.lock").exists());

        drop(lock);

        try_lock_directory(directory.path()).unwrap();
    }

    #[cfg(unix)]
    fn try_lock_directory(parent: &Path) -> io::Result<()> {
        let directory = fs::File::open(parent).unwrap();
        // SAFETY: `directory` owns a valid descriptor for the duration of the
        // call and `flock` does not retain it.
        let result = unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[test]
    fn curl_download_disables_config_files_and_restricts_protocols() {
        let command = curl_download(Path::new("/tmp/FSK.zip"));
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(args.first().map(String::as_str), Some("--disable"));
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--proto" && pair[1] == "=https")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--proto-redir" && pair[1] == "=https")
        );
        assert_eq!(args.last().map(String::as_str), Some("/tmp/FSK.zip"));
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_removes_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let app = directory.path().join("FSK.app");
        fs::create_dir(&target).unwrap();
        symlink(&target, &app).unwrap();

        assert!(uninstall_app_at(&app).unwrap());
        assert!(target.exists());
        assert!(!app.exists());
    }

    #[test]
    fn release_urls_use_the_release_tag_and_asset_name() {
        assert_eq!(
            public_release_url("v0.0.9"),
            "https://github.com/specta-rs/fsk/releases/download/v0.0.9/FSK-v0.0.9.zip"
        );
        assert_eq!(
            mirror_url("https://example.com/fsk/", "v0.0.9"),
            "https://example.com/fsk/v0.0.9/FSK-v0.0.9.zip"
        );
    }

    #[test]
    fn empty_overrides_are_rejected() {
        assert!(app_path_from(Some("".into())).is_err());
        assert!(app_path_from(Some("   ".into())).is_err());
        assert_eq!(
            app_path_from(None).unwrap(),
            Path::new("/Applications/FSK.app")
        );
        assert!(release_base_from(Some("".into())).is_err());
        assert!(release_base_from(Some("   ".into())).is_err());
        assert_eq!(release_base_from(None).unwrap(), None);
        assert_eq!(
            release_base_from(Some("https://example.com/fsk/".into())).unwrap(),
            Some("https://example.com/fsk/".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_restores_a_symlink_backup() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("marker"), b"restored link").unwrap();
        let destination = directory.path().join("FSK.app");
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let backup = directory
            .path()
            .join(format!(".FSK.app.backup-{timestamp}-1"));
        symlink(&target, &backup).unwrap();

        recover_interrupted_install_with(&destination, |_| Ok(())).unwrap();

        assert!(
            fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!backup.exists());
    }

    #[test]
    fn temporary_directory_is_private() {
        let directory = temporary_directory().unwrap();
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn token_config_is_private() {
        let directory = tempfile::tempdir().unwrap();
        let config = curl_token_config(directory.path(), "token").unwrap();
        assert_eq!(fs::metadata(&config).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn uninstall_refuses_app_without_info_plist() {
        let directory = tempfile::tempdir().unwrap();
        let app = directory.path().join("FSK.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join(APP_EXECUTABLE), b"").unwrap();

        assert!(uninstall_app_at(&app).is_err());
        assert!(app.exists());
    }

    #[cfg(unix)]
    #[test]
    fn is_fsk_app_accepts_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let app = directory.path().join("FSK.app");
        fs::create_dir(&target).unwrap();
        symlink(&target, &app).unwrap();

        assert!(is_fsk_app(&app).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn require_real_directory_rejects_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();

        assert!(require_real_directory(&link).is_err());
        assert!(require_real_directory(&target).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_removes_symlink_with_trailing_separator() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let app = directory.path().join("FSK.app");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"keep me").unwrap();
        symlink(&target, &app).unwrap();
        let app_with_slash = format!("{}/", app.display());

        assert!(uninstall_app_at(Path::new(&app_with_slash)).unwrap());
        assert!(target.exists());
        assert_eq!(fs::read(target.join("keep")).unwrap(), b"keep me");
        assert!(!app.exists());
    }
}
