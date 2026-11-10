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
    let token = env::var("GOOGLE_DRIVE_ACCESS_TOKEN").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "set GOOGLE_DRIVE_ACCESS_TOKEN to an OAuth token with the drive scope",
        )
    })?;
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
