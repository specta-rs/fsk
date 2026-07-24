use std::{
    env, io,
    path::{Path, PathBuf},
    process::Command,
};

const APP_EXECUTABLE: &str = "Contents/MacOS/FSBind";

/// Whether macOS currently exposes the installed `FSBind` module to `FSKit`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizationStatus {
    /// The app or its embedded extension is not installed.
    NotInstalled,
    /// The extension is installed but disabled by the user.
    NeedsApproval,
    /// The extension is installed and enabled.
    Authorized,
}

/// Ask the installed Swift app for the current `FSKit` authorization state.
///
/// Set `FSBIND_APP_PATH` to override the normal `/Applications/FSBind.app`
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
        .arg("--fsbind-authorization-status")
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
            format!("unexpected FSBind authorization response: {other}"),
        )),
    }
}

/// Open the `FSBind` companion app directly in its authorization guide.
///
/// # Errors
///
/// Returns an error when the app is missing or macOS cannot open it.
pub fn open_authorization_app() -> io::Result<()> {
    let app = app_path();
    if !app.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("FSBind.app was not found at {}", app.display()),
        ));
    }
    let status = Command::new("/usr/bin/open")
        .arg("-a")
        .arg(&app)
        .arg("--args")
        .arg("--fsbind-authorize")
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("macOS could not open FSBind.app"))
    }
}

fn app_path() -> PathBuf {
    env::var_os("FSBIND_APP_PATH").map_or_else(
        || Path::new("/Applications/FSBind.app").to_owned(),
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
            Path::new("/Applications/FSBind.app").join(APP_EXECUTABLE),
            Path::new("/Applications/FSBind.app/Contents/MacOS/FSBind")
        );
    }
}
