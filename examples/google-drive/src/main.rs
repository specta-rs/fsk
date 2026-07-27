mod drive;

use fsk::MountSession;
#[cfg(target_os = "macos")]
use fsk::{AuthorizationStatus, authorization_status, open_authorization_app};
use std::{
    env, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

fn main() -> io::Result<()> {
    let mount_path = env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: fsk-google-drive <mount-path>",
        )
    })?;
    let token = env::var("GOOGLE_DRIVE_ACCESS_TOKEN").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "set GOOGLE_DRIVE_ACCESS_TOKEN to an OAuth token with the drive.readonly scope",
        )
    })?;
    check_platform_authorization()?;
    let filesystem = drive::GoogleDrive::connect(token)?;
    let session = MountSession::mount_network(filesystem, &mount_path)?;
    println!(
        "mounted Google Drive at {} — press Ctrl-C to unmount",
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

#[cfg(target_os = "macos")]
fn check_platform_authorization() -> io::Result<()> {
    match authorization_status()? {
        AuthorizationStatus::Authorized => Ok(()),
        AuthorizationStatus::NeedsApproval => {
            open_authorization_app()?;
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
