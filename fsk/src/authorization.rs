//! Platform setup required before mounting a filesystem.

use std::io;

/// Whether the current platform is ready to mount a filesystem.
///
/// The platform adapters need some host support that is not part of the Rust
/// process:
///
/// - **macOS** needs the `FSK.app` companion and its `FSKit` extension enabled.
/// - **Linux** needs FUSE available to the mounting user.
/// - **Windows** needs the Projected File System optional component
///   (`Client-ProjFS`).
///
/// [`authorization_status`] performs the platform-specific check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizationStatus {
    /// No companion app with a compatible protocol version is installed.
    ///
    /// Only macOS returns this. [`ensure_authorization`] installs the release
    /// matching this crate and launches it; `install` and `launch_app`
    /// remain available when callers want to control each step.
    NotInstalled,
    /// The platform component or extension needs a user action.
    ///
    /// On macOS, launch `FSK.app` and enable the extension under
    /// **System Settings → General → Login Items & Extensions → File System
    /// Extensions**. On Windows, enable the Projected File System optional
    /// component. On Linux, install FUSE and make `/dev/fuse` available.
    NeedsApproval,
    /// The filesystem can be mounted.
    Authorized,
}

impl AuthorizationStatus {
    /// Returns whether `fsk::install()` should be called.
    ///
    /// This is only `true` on macOS when the companion app is missing or speaks
    /// an incompatible protocol version. Installing does not authorize the
    /// extension, so the status is normally [`NeedsApproval`](Self::NeedsApproval)
    /// afterwards.
    #[must_use]
    pub const fn needs_install(self) -> bool {
        matches!(self, Self::NotInstalled)
    }

    /// The platform-specific action that makes this status [`Authorized`](Self::Authorized).
    ///
    /// Returns `None` when the platform is already ready.
    #[must_use]
    pub const fn requirement(self) -> Option<&'static str> {
        match self {
            Self::Authorized => None,
            Self::NotInstalled => {
                Some("install the FSK companion app with fsk::ensure_authorization()")
            }
            Self::NeedsApproval => Some(APPROVAL_REQUIREMENT),
        }
    }
}

#[cfg(target_os = "macos")]
const APPROVAL_REQUIREMENT: &str =
    "enable FSK in System Settings → General → Login Items & Extensions → File System Extensions";
#[cfg(target_os = "linux")]
const APPROVAL_REQUIREMENT: &str = "FUSE must be available";
#[cfg(target_os = "windows")]
const APPROVAL_REQUIREMENT: &str = concat!(
    "enable the Projected File System optional component (Client-ProjFS) from an ",
    "administrator PowerShell with: ",
    "Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS",
);
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const APPROVAL_REQUIREMENT: &str = "this platform is not supported";

/// Checks whether the current platform is ready to mount a filesystem.
///
/// On macOS this queries the installed `FSK.app`. On Linux it checks that
/// `/dev/fuse` is available. On Windows it checks that `ProjectedFSLib.dll` is
/// installed.
///
/// # Errors
///
/// Returns an error when the platform check itself fails, such as when the
/// macOS companion cannot be executed.
pub fn authorization_status() -> io::Result<AuthorizationStatus> {
    imp::authorization_status()
}

/// Checks the platform requirements, installs the companion app when it is
/// missing, and opens the relevant setup UI when a user action is needed.
///
/// On macOS this installs the `FSK.app` release matching this crate, launches
/// it so its extension registers, and returns an error asking the user to
/// enable the extension. An installed app that already matches the SDK is
/// launched without downloading anything. On Windows this opens the Windows
/// Features dialog when `Client-ProjFS` is missing. Linux has no setup dialog,
/// so the returned error describes the FUSE requirement.
///
/// Use `install` and `launch_app` directly only when the caller needs to
/// control installation separately, such as installing ahead of time or
/// selecting a development build with `FSK_APP_PATH`.
///
/// This blocks while downloading and verifying `FSK.app`, so run it on a
/// background executor in an async application.
///
/// # Errors
///
/// Returns [`io::ErrorKind::PermissionDenied`] with an actionable message when
/// the platform still needs to be configured after the setup UI is opened.
/// Propagates errors from installing `FSK.app`, such as network, verification,
/// or filesystem failures.
pub fn ensure_authorization() -> io::Result<()> {
    match run_setup()? {
        SetupOutcome::Authorized => Ok(()),
        SetupOutcome::NeedsApproval => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            APPROVAL_MESSAGE,
        )),
    }
}

enum SetupOutcome {
    Authorized,
    NeedsApproval,
}

fn run_setup() -> io::Result<SetupOutcome> {
    match authorization_status()? {
        AuthorizationStatus::Authorized => return Ok(SetupOutcome::Authorized),
        AuthorizationStatus::NotInstalled => {
            #[cfg(target_os = "macos")]
            imp::install_app()?;
        }
        AuthorizationStatus::NeedsApproval => {}
    }
    imp::request_approval()?;
    Ok(SetupOutcome::NeedsApproval)
}

#[cfg(target_os = "macos")]
const APPROVAL_MESSAGE: &str = concat!(
    "enable FSK in System Settings → General → Login Items & Extensions → ",
    "File System Extensions, then run this command again"
);
#[cfg(target_os = "linux")]
const APPROVAL_MESSAGE: &str = "FUSE must be available; install fuse3 and make /dev/fuse accessible, then run this command again";
#[cfg(target_os = "windows")]
const APPROVAL_MESSAGE: &str = concat!(
    "Enable the Projected File System optional component (Client-ProjFS) in the window that ",
    "opened, then run this command again. From an administrator PowerShell:\n\n    ",
    "Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS",
);
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const APPROVAL_MESSAGE: &str = "this platform is not supported";

/// How often [`wait_for_authorization`] rechecks while the user is in System
/// Settings.
///
/// A macOS check runs the installed companion app's status command, so this is
/// deliberately slower than a filesystem stat.
const AUTHORIZATION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Runs the setup flow once, then blocks until the platform is ready to mount.
///
/// This is the "wait for the user" counterpart to [`ensure_authorization`]:
///
/// 1. When the host is already ready, it returns immediately.
/// 2. Otherwise it runs the same setup as [`ensure_authorization`] once:
///    install the matching macOS app when needed and open the setup UI.
///    Setup failures are returned immediately.
/// 3. It then polls [`authorization_status`] until the user finishes setup, or
///    until `timeout` elapses.
///
/// A reload of System Settings, a restart of the terminal, or a fresh build are
/// not needed: the first poll after the switch flips succeeds and the caller can
/// mount immediately.
///
/// The call blocks the current thread for as long as the user takes, so run it
/// on a background executor in an async application. `timeout` bounds the wait
/// for CLIs and CI; a value like `Duration::from_secs(600)` is generous enough
/// for a person and short enough not to hang a script. Use `Duration::ZERO` to
/// check once without waiting.
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] when the deadline passes while setup is
/// still required; the message names the outstanding requirement. Propagates
/// installation, setup UI, and status-check errors unchanged.
///
/// # Example
///
/// ```no_run
/// use std::{io, time::Duration};
///
/// fn main() -> io::Result<()> {
///     // Installs FSK.app on macOS if needed, asks the user to enable the
///     // extension, and returns once the platform can mount.
///     fsk::wait_for_authorization(Duration::from_secs(600))?;
///     Ok(())
/// }
/// ```
pub fn wait_for_authorization(timeout: std::time::Duration) -> io::Result<()> {
    wait_for_authorization_with(timeout, run_setup, authorization_status)
}

fn wait_for_authorization_with(
    timeout: std::time::Duration,
    setup: impl FnOnce() -> io::Result<SetupOutcome>,
    mut check_status: impl FnMut() -> io::Result<AuthorizationStatus>,
) -> io::Result<()> {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "authorization timeout is too large",
            )
        })?;

    if let SetupOutcome::Authorized = setup()? {
        return Ok(());
    }

    loop {
        match check_status()? {
            AuthorizationStatus::Authorized => return Ok(()),
            status => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        status.requirement().map_or_else(
                            || "timed out waiting for FSK setup".to_owned(),
                            |requirement| format!("timed out waiting for FSK setup: {requirement}"),
                        ),
                    ));
                }
                std::thread::sleep(remaining.min(AUTHORIZATION_POLL_INTERVAL));
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::AuthorizationStatus;
    use std::io;

    pub(super) fn authorization_status() -> io::Result<AuthorizationStatus> {
        crate::macos_authorization::authorization_status()
    }

    pub(super) fn install_app() -> io::Result<()> {
        crate::macos_authorization::install().map(|_| ())
    }

    pub(super) fn request_approval() -> io::Result<()> {
        crate::macos_authorization::launch_app()
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::AuthorizationStatus;
    use std::{fs, io, os::unix::fs::FileTypeExt, path::Path};

    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn authorization_status() -> io::Result<AuthorizationStatus> {
        let available = available_at(Path::new("/dev/fuse"));
        Ok(if available {
            AuthorizationStatus::Authorized
        } else {
            AuthorizationStatus::NeedsApproval
        })
    }

    fn available_at(path: &Path) -> bool {
        fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_char_device())
    }

    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn request_approval() -> io::Result<()> {
        // Linux has no standard setup dialog. The error from
        // `ensure_authorization` contains the FUSE requirement instead.
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn authorization_requires_device_open_not_just_metadata() {
            let path = Path::new("/dev/tty");
            assert!(fs::metadata(path).unwrap().file_type().is_char_device());
            assert!(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .is_err(),
                "this regression expects the headless runner to have no controlling tty"
            );
            assert!(
                !available_at(path),
                "an unopenable character device must not report Authorized"
            );
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::AuthorizationStatus;
    use std::{io, process::Command};
    use windows_sys::Win32::{
        Foundation::FreeLibrary,
        System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
    };

    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn authorization_status() -> io::Result<AuthorizationStatus> {
        Ok(if projfs_available() {
            AuthorizationStatus::Authorized
        } else {
            AuthorizationStatus::NeedsApproval
        })
    }

    pub(super) fn request_approval() -> io::Result<()> {
        Command::new("cmd")
            .args(["/C", "start", "", "optionalfeatures.exe"])
            .spawn()?;
        Ok(())
    }

    fn projfs_available() -> bool {
        // `LoadLibraryExW` with the system32 search path avoids loading a
        // same-named DLL from the current directory.
        let name: Vec<u16> = "ProjectedFSLib.dll\0".encode_utf16().collect();
        let module = unsafe {
            LoadLibraryExW(
                name.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            return false;
        }
        unsafe {
            FreeLibrary(module);
        }
        true
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod imp {
    use super::AuthorizationStatus;
    use std::io;

    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn authorization_status() -> io::Result<AuthorizationStatus> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "fsk does not support this platform",
        ))
    }

    pub(super) fn request_approval() -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_propagates_setup_permission_denied() {
        let error = wait_for_authorization_with(
            std::time::Duration::ZERO,
            || {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cannot create the companion app directory",
                ))
            },
            || Ok(AuthorizationStatus::NotInstalled),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            error.to_string(),
            "cannot create the companion app directory"
        );
    }

    #[test]
    fn wait_polls_when_setup_needs_approval() {
        wait_for_authorization_with(
            std::time::Duration::ZERO,
            || Ok(SetupOutcome::NeedsApproval),
            || Ok(AuthorizationStatus::Authorized),
        )
        .unwrap();
    }

    #[test]
    fn status_reports_the_next_setup_action() {
        assert_eq!(AuthorizationStatus::Authorized.requirement(), None);
        assert!(!AuthorizationStatus::Authorized.needs_install());

        assert!(AuthorizationStatus::NotInstalled.needs_install());
        assert!(AuthorizationStatus::NotInstalled.requirement().is_some());

        assert!(!AuthorizationStatus::NeedsApproval.needs_install());
        assert!(AuthorizationStatus::NeedsApproval.requirement().is_some());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_requirement_includes_the_enable_command() {
        let requirement = AuthorizationStatus::NeedsApproval
            .requirement()
            .expect("needs approval has a requirement");
        assert!(requirement.contains("Client-ProjFS"));
        assert!(
            requirement
                .contains("Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS")
        );
    }
}
