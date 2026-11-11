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
    /// matching this crate and launches it; [`install`] and [`launch_app`]
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
/// Use [`install`] and [`launch_app`] directly only when the caller needs to
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
    match authorization_status()? {
        AuthorizationStatus::Authorized => Ok(()),
        AuthorizationStatus::NotInstalled => {
            #[cfg(target_os = "macos")]
            imp::install_app()?;
            imp::request_approval()?;
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                APPROVAL_MESSAGE,
            ))
        }
        AuthorizationStatus::NeedsApproval => {
            imp::request_approval()?;
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                APPROVAL_MESSAGE,
            ))
        }
    }
}

#[cfg(target_os = "macos")]
const APPROVAL_MESSAGE: &str = "enable FSK in the window that opened, then run this command again";
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
    use std::{fs, io, os::unix::fs::FileTypeExt};

    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn authorization_status() -> io::Result<AuthorizationStatus> {
        let available =
            fs::metadata("/dev/fuse").is_ok_and(|metadata| metadata.file_type().is_char_device());
        Ok(if available {
            AuthorizationStatus::Authorized
        } else {
            AuthorizationStatus::NeedsApproval
        })
    }

    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn request_approval() -> io::Result<()> {
        // Linux has no standard setup dialog. The error from
        // `ensure_authorization` contains the FUSE requirement instead.
        Ok(())
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
