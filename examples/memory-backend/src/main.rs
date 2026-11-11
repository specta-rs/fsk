#![doc(
    html_logo_url = "https://github.com/specta-rs/fsk/raw/main/assets/app-icon-light.svg",
    html_favicon_url = "https://github.com/specta-rs/fsk/raw/main/assets/app-icon-light.svg"
)]

mod fs;

use fsk::MountSession;
use std::{
    env,
    ffi::OsString,
    fs as std_fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mount_path = mount_path_from_args()?;

    // The session creates the directory if it is missing, so create it here
    // first. That makes an existing directory an error and lets us tell
    // whether cleanup below owns the directory.
    let mount_directory = MountDirectory::create(&mount_path)?;

    authorize()?;

    let session = MountSession::mount(fs::filesystem(), &mount_path)?;
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

    // MountSession's Drop implementation unmounts and removes its socket, then
    // MountDirectory's Drop removes the empty mount directory.
    drop(session);
    drop(mount_directory);
    Ok(())
}

/// Checks the platform requirement before mounting.
///
/// `FSKit` approval, FUSE, and the Projected File System optional component are
/// host setup that Cargo cannot install. `requirement()` names the next step;
/// `ensure_authorization()` opens the platform setup UI when one exists and
/// returns the actionable error that `main` prints.
fn authorize() -> io::Result<()> {
    match fsk::authorization_status()? {
        fsk::AuthorizationStatus::Authorized => Ok(()),
        status => {
            if let Some(requirement) = status.requirement() {
                eprintln!("setup required: {requirement}");
            }
            fsk::ensure_authorization()
        }
    }
}

fn mount_path_from_args() -> io::Result<PathBuf> {
    let mut args = env::args_os();
    let program = args
        .next()
        .unwrap_or_else(|| OsString::from("fsk-memory-backend"));
    match (args.next(), args.next()) {
        (Some(path), None) => Ok(PathBuf::from(path)),
        (None, _) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "missing mount directory\n\nusage: {} <mount-path>\n\n\
                 <mount-path> is created when the mount starts and removed when it stops, \
                 so it must not already exist.",
                Path::new(&program).display()
            ),
        )),
        (Some(_), Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "expected exactly one mount directory\n\nusage: {} <mount-path>",
                Path::new(&program).display()
            ),
        )),
    }
}

/// Owns the mount directory created by this process and removes it on exit.
struct MountDirectory {
    path: PathBuf,
}

impl MountDirectory {
    fn create(path: &Path) -> io::Result<Self> {
        match std_fs::create_dir(path) {
            Ok(()) => Ok(Self {
                path: path.to_owned(),
            }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "mount directory {} already exists; remove it or choose a different path",
                    path.display()
                ),
            )),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!(
                    "could not create mount directory {}: {error}",
                    path.display()
                ),
            )),
        }
    }
}

impl Drop for MountDirectory {
    fn drop(&mut self) {
        if let Err(error) = std_fs::remove_dir(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "warning: could not remove mount directory {}: {error}",
                self.path.display()
            );
        }
    }
}
