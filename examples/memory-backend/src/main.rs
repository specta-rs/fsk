use fsbind::{AuthorizationStatus, MountSession, authorization_status, open_authorization_app};
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
            "usage: fsbind-memory-backend <mount-path>",
        )
    })?;

    match authorization_status()? {
        AuthorizationStatus::Authorized => {}
        AuthorizationStatus::NeedsApproval => {
            open_authorization_app()?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "enable FSBind in the window that opened, then run this command again",
            ));
        }
        AuthorizationStatus::NotInstalled => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "install FSBind.app in /Applications before mounting",
            ));
        }
    }

    let session = MountSession::mount(fsbind_memory_backend::filesystem(), &mount_path)?;
    println!(
        "mounted at {} — press Ctrl-C to unmount",
        session.mount_path().display()
    );

    let running = Arc::new(AtomicBool::new(true));
    let signal_state = Arc::clone(&running);
    ctrlc::set_handler(move || signal_state.store(false, Ordering::Release))
        .map_err(io::Error::other)?;
    while running.load(Ordering::Acquire) {
        thread::park_timeout(Duration::from_millis(250));
    }

    // MountSession's Drop implementation unmounts and removes its socket.
    drop(session);
    Ok(())
}
