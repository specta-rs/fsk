//! macOS mount-session lifecycle.

use crate::{Filesystem, NetworkFilesystem, ipc::Service};
use parking_lot::Mutex;
use std::{
    fs::{self, File},
    io::{self, Read},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);

/// Settings used when registering an `FSK` volume with macOS.
#[derive(Clone, Debug, Default)]
pub struct MountOptions {
    /// Mount the volume read-only.
    pub read_only: bool,
    /// Additional comma-separated options passed to `mount(8)`.
    pub mount_options: Vec<String>,
}

/// An active `FSKit` mount backed by this process.
///
/// Dropping the value attempts to unmount the path and always stops the local
/// IPC service, closes its listener, and removes its private socket. If
/// `diskutil` fails, the path can remain mounted. Keep the value alive for as
/// long as the filesystem should be available.
pub struct MountSession {
    mount_path: PathBuf,
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    state: Mutex<SessionState>,
}

struct SessionState {
    accept_thread: Option<JoinHandle<()>>,
    mounted: bool,
}

impl MountSession {
    /// Start a local filesystem service and mount it at `mount_path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket, mount directory, or `mount(8)` call fails.
    pub fn mount(filesystem: impl Filesystem, mount_path: impl AsRef<Path>) -> io::Result<Self> {
        Self::mount_with_options(filesystem, mount_path, &MountOptions::default())
    }

    /// Start a local filesystem service with explicit mount settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket, mount directory, or `mount(8)` call fails.
    pub fn mount_with_options(
        filesystem: impl Filesystem,
        mount_path: impl AsRef<Path>,
        options: &MountOptions,
    ) -> io::Result<Self> {
        let token = random_token()?;
        Self::start(
            Service::local(filesystem, token),
            token,
            mount_path.as_ref(),
            options,
        )
    }

    /// Start a lazy network filesystem service and mount it at `mount_path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket, mount directory, or `mount(8)` call fails.
    pub fn mount_network(
        filesystem: impl NetworkFilesystem,
        mount_path: impl AsRef<Path>,
    ) -> io::Result<Self> {
        Self::mount_network_with_options(filesystem, mount_path, &MountOptions::default())
    }

    /// Start a lazy network filesystem with explicit mount settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket, mount directory, or `mount(8)` call fails.
    pub fn mount_network_with_options(
        filesystem: impl NetworkFilesystem,
        mount_path: impl AsRef<Path>,
        options: &MountOptions,
    ) -> io::Result<Self> {
        let token = random_token()?;
        Self::start(
            Service::network(filesystem, token),
            token,
            mount_path.as_ref(),
            options,
        )
    }

    /// Path at which this volume is mounted.
    #[must_use]
    pub fn mount_path(&self) -> &Path {
        &self.mount_path
    }

    /// Explicitly unmount. Calling this is optional; [`Drop`] also attempts it.
    ///
    /// A failed explicit unmount leaves the mount and its service alive so the
    /// caller can retry through the same immutable session binding. In contrast,
    /// [`Drop`] always performs best-effort local cleanup: it attempts unmount
    /// again and releases the socket and accept thread even if `diskutil` fails.
    ///
    /// # Errors
    ///
    /// Returns an error if `diskutil` cannot unmount the path.
    pub fn unmount(&self) -> io::Result<()> {
        self.unmount_inner(&mut self.state.lock())
    }

    fn start(
        service: Service,
        token: [u8; 32],
        mount_path: &Path,
        options: &MountOptions,
    ) -> io::Result<Self> {
        let mount_path = prepare_mount_path(mount_path)?;
        let socket_path = socket_path()?;
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let listener = bind_listener(&socket_path)?;
        // Any early return below must not leave the bound socket behind.
        let mut socket_cleanup = SocketCleanup {
            path: socket_path.clone(),
            armed: true,
        };
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let service = Arc::new(service);
        let accept_thread = thread::Builder::new()
            .name("fsk-accept".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let service = Arc::clone(&service);
                            let _ = thread::Builder::new().name("fsk-connection".into()).spawn(
                                move || {
                                    let _ = service.serve_connection(stream);
                                },
                            );
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) if retryable_accept_error(&error) => {
                            // Aborted handshakes and temporary descriptor or
                            // memory pressure must not stop the volume. Back
                            // off so persistent EMFILE does not spin a CPU.
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })?;

        let resource = resource_url(&socket_path, &token);
        let mut command = Command::new("/sbin/mount");
        command.arg("-F").arg("-t").arg("fsk");
        if options.read_only {
            command.arg("-r");
        }
        if !options.mount_options.is_empty() {
            command.arg("-o").arg(options.mount_options.join(","));
        }
        // `?` here would return with the accept thread and its listener still
        // alive, so both failure modes shut the service down first.
        let output = match command.arg(&resource).arg(&mount_path).output() {
            Ok(output) => output,
            Err(error) => {
                stop_accepting(&stop, &socket_path, accept_thread);
                return Err(error);
            }
        };
        if !output.status.success() {
            stop_accepting(&stop, &socket_path, accept_thread);
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(io::Error::other(format!(
                "FSKit mount failed: {message}{}",
                mount_failure_hint(&message)
            )));
        }
        socket_cleanup.armed = false;

        Ok(Self {
            mount_path,
            socket_path,
            stop,
            state: Mutex::new(SessionState {
                accept_thread: Some(accept_thread),
                mounted: true,
            }),
        })
    }

    fn unmount_inner(&self, state: &mut SessionState) -> io::Result<()> {
        // The state lock serializes unmount attempts and teardown. The accept
        // worker only reads the stop flag, so joining it under this lock
        // cannot deadlock on session state.
        let result = if state.mounted {
            match Command::new("/usr/sbin/diskutil")
                .arg("unmount")
                .arg(&self.mount_path)
                .output()
            {
                Ok(output) if output.status.success() => {
                    state.mounted = false;
                    Ok(())
                }
                Ok(output) => Err(io::Error::other(
                    String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                )),
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        };
        if result.is_ok() {
            self.stop_service(state);
        }
        result
    }

    fn stop_service(&self, state: &mut SessionState) {
        if let Some(thread) = state.accept_thread.take() {
            stop_accepting(&self.stop, &self.socket_path, thread);
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl Drop for MountSession {
    fn drop(&mut self) {
        // An explicit unmount failure leaves the service usable for a retry.
        // Drop cannot return that error, so always release local resources.
        let mut state = self.state.lock();
        let _ = self.unmount_inner(&mut state);
        self.stop_service(&mut state);
    }
}

/// Removes a bound socket unless startup completed and a [`MountSession`] now
/// owns the cleanup.
struct SocketCleanup {
    path: PathBuf,
    armed: bool,
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Stops the polling accept loop and waits for it to finish.
fn stop_accepting(stop: &AtomicBool, socket_path: &Path, accept_thread: JoinHandle<()>) {
    stop.store(true, Ordering::Release);
    // The listener is non-blocking, so wake the loop in case it is sleeping.
    let _ = UnixStream::connect(socket_path);
    let _ = accept_thread.join();
}

/// `accept` can fail for one aborted handshake without the listener being
/// broken; those errors must not shut the service down permanently.
fn retryable_accept_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    ) || matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

/// Adds the likely cause to the bare `EPERM` macOS reports for some mounts.
///
/// `mount(8)` only forwards `Operation not permitted`, which gives no hint that
/// the mount directory itself is the problem.
fn mount_failure_hint(stderr: &str) -> &'static str {
    if stderr.contains("Operation not permitted") {
        "\n\nhint: macOS blocks FSKit mounts inside privacy-protected folders such as \
         Desktop, Documents and Downloads. Choose a mount directory outside those folders, \
         for example under your home directory or in /tmp."
    } else {
        ""
    }
}

fn prepare_mount_path(path: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(path)?;
    fs::canonicalize(path)
}

fn random_token() -> io::Result<[u8; 32]> {
    let mut token = [0; 32];
    File::open("/dev/urandom")?.read_exact(&mut token)?;
    Ok(token)
}

fn socket_path() -> io::Result<PathBuf> {
    let id = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
    let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate the user home directory",
        )
    })?);
    if !home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HOME must be an absolute path",
        ));
    }
    Ok(home
        .join("Library/Group Containers/group.dev.specta.fsk/Runtime")
        .join(format!("{}-{id}.sock", std::process::id())))
}

/// Binds the per-process socket, recovering a pathname left behind by a
/// process that exited without unlinking its listener. A live listener or a
/// non-socket file is never removed.
fn bind_listener(path: &Path) -> io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Err(error) if error.raw_os_error() == Some(libc::EADDRINUSE) => {
            match UnixStream::connect(path) {
                Err(connect_error) if connect_error.raw_os_error() == Some(libc::ECONNREFUSED) => {
                    let metadata = fs::symlink_metadata(path)?;
                    if !metadata.file_type().is_socket() {
                        return Err(error);
                    }
                    fs::remove_file(path)?;
                    UnixListener::bind(path)
                }
                Err(connect_error) if connect_error.kind() == io::ErrorKind::NotFound => {
                    UnixListener::bind(path)
                }
                Ok(_) | Err(_) => Err(error),
            }
        }
        result => result,
    }
}

fn resource_url(socket: &Path, token: &[u8; 32]) -> String {
    let mut token_hex = String::with_capacity(64);
    for byte in token {
        use std::fmt::Write as _;
        let _ = write!(&mut token_hex, "{byte:02x}");
    }
    format!(
        "fsk://mount?socket={}&token={token_hex}",
        percent_encode(socket.as_os_str().as_encoded_bytes())
    )
}

fn percent_encode(value: &[u8]) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'_' | b'.') {
            output.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut output, "%{byte:02X}");
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_contains_encoded_socket_and_secret() {
        let value = resource_url(Path::new("/tmp/a b.sock"), &[0xab; 32]);
        assert!(value.starts_with("fsk://mount?socket=/tmp/a%20b.sock&token="));
        assert!(value.ends_with(&"ab".repeat(32)));
    }

    #[test]
    fn mount_path_is_canonicalized_for_diskutil() {
        let path = Path::new("/tmp");
        let canonical = prepare_mount_path(path).unwrap();
        assert_eq!(canonical, Path::new("/private/tmp"));
    }

    #[test]
    fn aborted_handshakes_do_not_stop_the_accept_loop() {
        for kind in [io::ErrorKind::Interrupted, io::ErrorKind::ConnectionAborted] {
            assert!(retryable_accept_error(&io::Error::from(kind)));
        }
        for kind in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Other,
        ] {
            assert!(!retryable_accept_error(&io::Error::from(kind)));
        }
    }

    #[test]
    fn permission_failures_explain_the_privacy_restriction() {
        let stderr = "mount: Final mount step ended with error: The operation couldn’t be \
                      completed. Operation not permitted";
        assert!(mount_failure_hint(stderr).contains("privacy-protected"));
        assert_eq!(mount_failure_hint("no such file or directory"), "");
    }

    #[test]
    fn unmount_accepts_an_immutable_binding_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("test.sock");
        fs::write(&socket, b"").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let accept_thread = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        });
        let session = MountSession {
            mount_path: directory.path().to_path_buf(),
            socket_path: socket.clone(),
            stop,
            state: Mutex::new(SessionState {
                accept_thread: Some(accept_thread),
                mounted: false,
            }),
        };

        session.unmount().unwrap();
        session.unmount().unwrap();
        assert!(!socket.exists());
    }

    #[test]
    fn socket_cleanup_removes_only_an_armed_socket() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("test.sock");

        fs::write(&socket, b"").unwrap();
        drop(SocketCleanup {
            path: socket.clone(),
            armed: true,
        });
        assert!(!socket.exists());

        fs::write(&socket, b"").unwrap();
        drop(SocketCleanup {
            path: socket.clone(),
            armed: false,
        });
        assert!(socket.exists());
    }
}

#[cfg(test)]
#[path = "mount_audit_tests.rs"]
mod audit_tests;
