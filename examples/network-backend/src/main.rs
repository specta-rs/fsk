#![doc(
    html_logo_url = "https://github.com/specta-rs/fsk/raw/main/assets/app-icon-light.svg",
    html_favicon_url = "https://github.com/specta-rs/fsk/raw/main/assets/app-icon-light.svg"
)]

mod fs;

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
    let mut arguments = env::args_os().skip(1);
    let mount_path = arguments.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: fsk-network-backend <mount-path> [remote-content]",
        )
    })?;
    let resource = arguments.next().map_or_else(
        || "Content fetched lazily by the network backend.\n".into(),
        |value| value.to_string_lossy().into_owned(),
    );

    fsk::ensure_authorization()?;
    let filesystem = fs::filesystem(resource.as_bytes())
        .map_err(|error| io::Error::from_raw_os_error(error.0))?;
    let session = MountSession::mount_network(filesystem, &mount_path)?;
    println!(
        "mounted network filesystem at {} — press Ctrl-C to unmount",
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
