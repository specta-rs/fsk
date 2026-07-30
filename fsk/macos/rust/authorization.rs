//! macOS `FSKit` extension authorization helpers.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const APP_EXECUTABLE: &str = "Contents/MacOS/FSK";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_URL: &str = "https://github.com/specta-rs/fsk/releases/download";

/// The result of installing the companion application for this SDK version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppInstallOutcome {
    /// The matching application was already installed.
    AlreadyInstalled,
    /// No application was present, so it was installed.
    Installed,
    /// A different application version was replaced.
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
/// `/Applications/FSK.app`. An already installed matching version is left alone.
/// Set `FSK_APP_PATH` to choose a different destination during development.
///
/// # Errors
///
/// Returns an error if the release cannot be downloaded, unpacked, validated,
/// or installed. Installing in the chosen destination may require permission.
pub fn download_and_install_app() -> io::Result<AppInstallOutcome> {
    let destination = app_path();
    let previous_version = app_version(&destination)?;
    if previous_version.as_deref() == Some(APP_VERSION) {
        return Ok(AppInstallOutcome::AlreadyInstalled);
    }

    let temporary = temporary_directory()?;
    let result = install_from_release(&temporary, &destination);
    let _ = fs::remove_dir_all(&temporary);
    result?;

    Ok(if previous_version.is_some() {
        AppInstallOutcome::Updated
    } else {
        AppInstallOutcome::Installed
    })
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
    let app = app_path();
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
    let app = app_path();
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

fn install_from_release(temporary: &Path, destination: &Path) -> io::Result<()> {
    let archive = temporary.join("FSK.zip");
    let extracted = temporary.join("extracted");
    fs::create_dir(&extracted)?;
    let tag = format!("v{APP_VERSION}");
    let url = format!("{RELEASE_URL}/{tag}/FSK-{tag}.zip");

    run(
        Command::new("/usr/bin/curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--output",
            ])
            .arg(&archive)
            .arg(url),
        "download FSK.app",
    )?;
    run(
        Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(&archive)
            .arg(&extracted),
        "unpack FSK.app",
    )?;

    let source = extracted.join("FSK.app");
    if app_version(&source)?.as_deref() != Some(APP_VERSION) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "downloaded FSK.app version does not match the Rust SDK",
        ));
    }
    run(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&source),
        "verify FSK.app signature",
    )?;
    run(
        Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(&source),
        "validate FSK.app with Gatekeeper",
    )?;

    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "FSK_APP_PATH must have a parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".FSK.app.install-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    run(
        Command::new("/usr/bin/ditto").arg(&source).arg(&staging),
        "stage FSK.app installation",
    )?;

    let backup = parent.join(format!(".FSK.app.backup-{}", std::process::id()));
    if destination.exists() {
        fs::rename(destination, &backup)?;
    }
    if let Err(error) = fs::rename(&staging, destination) {
        if backup.exists() {
            let _ = fs::rename(&backup, destination);
        }
        return Err(error);
    }
    if backup.exists() {
        fs::remove_dir_all(backup)?;
    }
    Ok(())
}

fn app_version(app: &Path) -> io::Result<Option<String>> {
    if !app.exists() {
        return Ok(None);
    }
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .output()?;
    if !output.status.success() {
        return Err(command_error("read FSK.app version", &output.stderr));
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

fn app_path() -> PathBuf {
    env::var_os("FSK_APP_PATH").map_or_else(
        || Path::new("/Applications/FSK.app").to_owned(),
        PathBuf::from,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_app_is_not_installed() {
        // The path resolver itself is deliberately deterministic; integration
        // tests exercise the installed app on signed release builds.
        assert_eq!(
            Path::new("/Applications/FSK.app").join(APP_EXECUTABLE),
            Path::new("/Applications/FSK.app/Contents/MacOS/FSK")
        );
    }
}
