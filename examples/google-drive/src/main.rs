#![doc(
    html_logo_url = "https://github.com/specta-rs/fsk/raw/main/assets/app-icon-light.svg",
    html_favicon_url = "https://github.com/specta-rs/fsk/raw/main/assets/app-icon-light.svg"
)]

mod drive;

use fsk::MountSession;
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
    let token = access_token()?;
    fsk::ensure_authorization()?;
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

fn access_token() -> io::Result<String> {
    if let Ok(token) = env::var("GOOGLE_DRIVE_ACCESS_TOKEN") {
        return Ok(token);
    }
    let client_id = required_env("GOOGLE_CLIENT_ID")?;
    let client_secret = required_env("GOOGLE_CLIENT_SECRET")?;
    let refresh_token = required_env("GOOGLE_DRIVE_REFRESH_TOKEN")?;
    drive::refresh_access_token(&client_id, &client_secret, &refresh_token)
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "set GOOGLE_DRIVE_ACCESS_TOKEN, or set {name} along with \
                 GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET, and \
                 GOOGLE_DRIVE_REFRESH_TOKEN"
            ),
        )
    })
}
