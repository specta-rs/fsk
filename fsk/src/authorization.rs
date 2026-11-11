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
    /// No companion app matching this SDK version is installed.
    ///
    /// Only macOS returns this. Call `fsk::install()` to install the matching
    /// release, then launch the app to register its extension.
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
    /// This is only `true` on macOS when the matching companion app is not
    /// installed. Installing does not authorize the extension, so the status is
    /// normally [`NeedsApproval`](Self::NeedsApproval) afterwards.
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
            Self::NotInstalled => Some("install the FSK companion app with fsk::install()"),
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

/// Checks the platform requirements and opens the relevant setup UI when a
/// user action is needed.
///
/// On macOS this opens `FSK.app` when the extension needs approval. On Windows
/// it opens the Windows Features dialog when `Client-ProjFS` is missing. Linux
/// has no setup dialog, so the returned error describes the FUSE requirement.
///
/// # Errors
///
/// Returns [`io::ErrorKind::NotFound`] when the macOS companion is missing, or
/// [`io::ErrorKind::PermissionDenied`] with an actionable message when the
/// platform still needs to be configured. For example:
///
/// - macOS: install `FSK.app` with `fsk::install()` and enable the extension.
/// - Linux: `FUSE must be available`.
/// - Windows: `Enable the Projected File System optional component`.
pub fn ensure_authorization() -> io::Result<()> {
    match authorization_status()? {
        AuthorizationStatus::Authorized => Ok(()),
        AuthorizationStatus::NotInstalled => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "install FSK.app with fsk::install() before mounting",
        )),
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
}
