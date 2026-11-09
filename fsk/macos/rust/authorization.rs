//! macOS `FSKit` extension authorization helpers.

use std::{
    env, fs, io,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;

const APP_BUNDLE_IDENTIFIER: &str = "dev.specta.fsk";
const APP_EXECUTABLE: &str = "Contents/MacOS/FSK";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_BASE_URL: &str = "https://github.com/specta-rs/fsk/releases/download";
const RELEASE_API_URL: &str = "https://api.github.com/repos/specta-rs/fsk/releases/tags";
static APP_MUTATION: Mutex<()> = Mutex::new(());

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
/// This operation is synchronous and never launches the application or presents
/// UI, so callers can run it on a background thread or executor of their choice.
/// The release is downloaded from GitHub, checked by macOS, and installed at
/// `/Applications/FSK.app`. An already installed, valid matching version is
/// left alone. Set `FSK_APP_PATH` to choose a different destination during
/// development.
///
/// If `GITHUB_TOKEN` or `GH_TOKEN` is set, the release asset is resolved through
/// the GitHub API so a private release can be downloaded. Set
/// `FSK_RELEASE_BASE_URL` to use a mirror that serves
/// `{base}/v{version}/FSK-v{version}.zip`. Without either, the release must be
/// publicly downloadable.
///
/// The destination is only replaced when it is an existing `FSK.app` bundle;
/// an unrelated directory or file is refused instead of being deleted.
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
/// contains `..`, or names something that is not an `FSK.app` bundle.
pub fn download_and_install_app() -> io::Result<AppInstallOutcome> {
    let _mutation = APP_MUTATION.lock();
    let destination = app_path()?;
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
    let _ = fs::remove_dir_all(&temporary);
    result?;

    Ok(if was_installed {
        AppInstallOutcome::Updated
    } else {
        AppInstallOutcome::Installed
    })
}

/// Remove the installed `FSK` companion app without presenting UI.
///
/// This operation is synchronous and is serialized with installation calls in
/// the same process. It returns `true` when an app was removed and `false` when
/// no app was installed. Set `FSK_APP_PATH` to choose a different destination
/// during development. The destination must be an `FSK.app` bundle or a
/// symlink; an unrelated directory or file is refused.
///
/// # Errors
///
/// Returns an error if the installed bundle cannot be removed or the
/// destination is not an `FSK.app` bundle. Removing it from the chosen
/// destination may require permission.
pub fn uninstall_app() -> io::Result<bool> {
    let _mutation = APP_MUTATION.lock();
    uninstall_app_at(&app_path()?)
}

/// Ask the installed Swift app for the current `FSKit` authorization state.
///
/// Set `FSK_APP_PATH` to override the normal `/Applications/FSK.app`
/// location, which is useful for development and app-local distributions.
///
/// # Errors
///
/// Returns an error when the app cannot be executed or returns invalid data.
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
/// # Errors
///
/// Returns an error when the app is missing or macOS cannot open it.
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
    fs::create_dir(&extracted)?;
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
    validate_app(&source, APP_VERSION)?;

    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "FSK_APP_PATH must have a parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;
    let invocation = temporary
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("invalid FSK installer temporary path"))?;
    let staging = parent.join(format!(".FSK.app.install-{invocation}"));
    let stage_result = run(
        Command::new("/usr/bin/ditto").arg(&source).arg(&staging),
        "stage FSK.app installation",
    );
    if let Err(error) = stage_result {
        let _ = remove_path(&staging);
        return Err(error);
    }
    // Re-check the staged copy so a swapped or corrupted source cannot reach
    // the destination between validation and replacement.
    if let Err(error) = validate_app(&staging, APP_VERSION) {
        let _ = remove_path(&staging);
        return Err(error);
    }

    let backup = parent.join(format!(".FSK.app.backup-{invocation}"));
    replace_app(&staging, destination, &backup)
}

fn download_release(temporary: &Path, archive: &Path, tag: &str) -> io::Result<()> {
    if let Some(base) = env::var_os("FSK_RELEASE_BASE_URL") {
        let base = base.to_string_lossy();
        let url = format!("{}/{tag}/FSK-{tag}.zip", base.trim_end_matches('/'));
        return run(curl_download(archive).arg(url), "download FSK.app");
    }
    if let Some(token) = github_token() {
        return download_release_with_token(temporary, archive, tag, &token);
    }
    let url = format!("{RELEASE_BASE_URL}/{tag}/FSK-{tag}.zip");
    run(curl_download(archive).arg(url), "download FSK.app").map_err(|error| {
        io::Error::other(format!(
            "{error}. FSK.app releases must be publicly downloadable; for a private repository set GITHUB_TOKEN or FSK_RELEASE_BASE_URL"
        ))
    })
}

fn curl_download(archive: &Path) -> Command {
    let mut command = Command::new("/usr/bin/curl");
    command.args([
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
        .find_map(|key| env::var(key).ok())
        .filter(|token| !token.trim().is_empty())
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

    run(
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
    let destination_present = match path_present(destination) {
        Ok(present) => present,
        Err(error) => {
            let _ = remove_path(staging);
            return Err(error);
        }
    };
    if destination_present {
        if let Err(error) = fs::rename(destination, backup) {
            let _ = remove_path(staging);
            return Err(error);
        }
    }
    if let Err(error) = fs::rename(staging, destination) {
        let _ = remove_path(staging);
        if destination_present {
            return match fs::rename(backup, destination) {
                Ok(()) => Err(error),
                Err(restore_error) => Err(io::Error::other(format!(
                    "could not install FSK.app ({error}) and could not restore the previous app ({restore_error}); the previous app remains at {}",
                    backup.display()
                ))),
            };
        }
        return Err(error);
    }
    // The new app is in place, so a stale backup is not an installation
    // failure. Leave it behind rather than reporting a false error.
    let _ = remove_path(backup);
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
    run(
        Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(app),
        "validate FSK.app with Gatekeeper",
    )
}

/// Check that a destination is an FSK app bundle without requiring it to be
/// current or correctly signed. This gates destructive operations so a
/// misconfigured `FSK_APP_PATH` cannot remove an unrelated directory.
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
    Ok(app_bundle_identifier(app)?.as_deref() == Some(APP_BUNDLE_IDENTIFIER))
}

fn uninstall_app_at(app: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(app) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        fs::remove_file(app)?;
        return Ok(true);
    }
    if !is_fsk_app(app)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to remove {} because it is not an FSK.app bundle",
                app.display()
            ),
        ));
    }
    fs::remove_dir_all(app)?;
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

fn app_path() -> io::Result<PathBuf> {
    let path = env::var_os("FSK_APP_PATH").map_or_else(
        || Path::new("/Applications/FSK.app").to_owned(),
        PathBuf::from,
    );
    normalize_app_path(&path)
}

/// Normalize `FSK_APP_PATH` before any metadata, rename, or removal operation.
/// Trailing separators and `.` components are removed, and `..` is rejected so
/// the resolved path cannot escape the directory the caller chose.
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
    if normalized.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FSK_APP_PATH must name an FSK.app bundle",
        ));
    }
    Ok(normalized)
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
    }

    #[test]
    fn uninstall_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let app = app_fixture(directory.path());

        assert!(uninstall_app_at(&app).unwrap());
        assert!(!uninstall_app_at(&app).unwrap());
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
}
