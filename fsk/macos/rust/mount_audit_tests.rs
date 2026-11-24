//! Snapshot-only deterministic regression tests for the macOS mount audit.

use super::*;
use crate::test_support::TestFilesystem;
use std::{
    fs::File,
    io::ErrorKind,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::Command,
    sync::mpsc,
    thread,
    time::Duration,
};

const CHILD: &str = "FSK_AUDIT_MOUNT_CHILD";
const AUDIT_HOME: &str = "FSK_AUDIT_HOME";
const DENY_MOUNT_AND_DISKUTIL: &str = concat!(
    "(version 1) (allow default) ",
    "(deny process-exec (literal \"/sbin/mount\")) ",
    "(deny process-exec (literal \"/usr/sbin/diskutil\"))"
);

#[test]
fn unmount_spawn_error_still_stops_service_and_removes_socket() {
    if std::env::var_os(CHILD).is_none() {
        run_sandboxed_child(
            "mount::audit_tests::unmount_spawn_error_still_stops_service_and_removes_socket",
        );
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("orphan.sock");
    fs::write(&socket, b"placeholder").unwrap();
    let probe = Command::new("/usr/sbin/diskutil")
        .arg("unmount")
        .arg(directory.path())
        .output()
        .unwrap_err();
    assert_eq!(
        probe.kind(),
        ErrorKind::PermissionDenied,
        "sandbox must deny diskutil"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stop_observer = Arc::clone(&stop);
    let worker_stop = Arc::clone(&stop);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let accept_thread = thread::spawn(move || {
        ready_tx.send(()).unwrap();
        while !worker_stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        done_tx.send(()).unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let session = MountSession {
        mount_path: directory.path().to_path_buf(),
        socket_path: socket.clone(),
        stop,
        accept_thread: Some(accept_thread),
        mounted: true,
    };

    drop(session);
    assert!(
        stop_observer.load(Ordering::Acquire),
        "Drop returned without stopping the accept loop after a diskutil spawn error"
    );
    assert!(
        !socket.exists(),
        "Drop returned without removing the socket after a diskutil spawn error"
    );
    done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
}

#[test]
fn failed_unmount_keeps_the_backend_available_for_a_retry() {
    if std::env::var_os(CHILD).is_none() {
        run_child(
            "mount::audit_tests::failed_unmount_keeps_the_backend_available_for_a_retry",
            None,
        );
        return;
    }

    // diskutil is real here, but the freshly-created temporary directory is
    // deliberately not a mount point. No mounted volume is touched.
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("retry.sock");
    fs::write(&socket, b"placeholder").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_observer = Arc::clone(&stop);
    let worker_stop = Arc::clone(&stop);
    let accept_thread = thread::spawn(move || {
        while !worker_stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
    });
    #[allow(unused_mut)] // Required by the snapshot fix's &mut self receiver.
    let mut session = MountSession {
        mount_path: directory.path().to_path_buf(),
        socket_path: socket.clone(),
        stop,
        accept_thread: Some(accept_thread),
        mounted: true,
    };

    let error = session.unmount().unwrap_err();
    eprintln!(
        "diskutil nonzero status: kind={:?} raw={:?}",
        error.kind(),
        error.raw_os_error()
    );
    assert!(
        !stop_observer.load(Ordering::Acquire),
        "a failed unmount consumed the session and stopped the backend while the mount remained"
    );
    assert!(
        socket.exists(),
        "a failed unmount removed the IPC socket needed for a retry"
    );
}

#[test]
fn stale_socket_from_an_earlier_process_is_recovered() {
    let Some(home) = std::env::var_os(AUDIT_HOME) else {
        // Darwin's sockaddr_un.sun_path is only 104 bytes; /tmp keeps the
        // audit fixture's HOME short enough for the real Runtime path.
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        run_sandboxed_child_with_home(
            "mount::audit_tests::stale_socket_from_an_earlier_process_is_recovered",
            directory.path(),
        );
        return;
    };
    assert!(std::env::var_os(CHILD).is_some());

    let runtime = PathBuf::from(home).join("Library/Group Containers/group.dev.specta.fsk/Runtime");
    fs::create_dir_all(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    // This child has not called socket_path yet, so start() will request ID 1.
    let stale = runtime.join(format!("{}-1.sock", std::process::id()));
    let stale_listener = UnixListener::bind(&stale).unwrap();
    drop(stale_listener);
    assert!(
        stale.exists(),
        "dropping a Unix listener leaves its pathname"
    );

    let directory = tempfile::tempdir().unwrap();
    let Err(error) = MountSession::mount(TestFilesystem::default(), directory.path().join("mount"))
    else {
        panic!("mount unexpectedly succeeded; the sandbox must deny /sbin/mount");
    };
    eprintln!(
        "mount with stale socket: kind={:?} raw={:?}",
        error.kind(),
        error.raw_os_error()
    );
    assert_ne!(
        error.raw_os_error(),
        Some(libc::EADDRINUSE),
        "a stale socket left by a crashed process permanently poisons this socket ID"
    );
}

#[test]
fn relative_home_resource_socket_works_across_working_directories_or_is_rejected() {
    const STAGE: &str = "FSK_AUDIT_RELATIVE_STAGE";
    const ENCODED_SOCKET: &str = "FSK_AUDIT_RELATIVE_SOCKET";
    const EXTENSION_CWD: &str = "FSK_AUDIT_EXTENSION_CWD";
    const TEST: &str = "mount::audit_tests::relative_home_resource_socket_works_across_working_directories_or_is_rejected";

    match std::env::var(STAGE).ok().as_deref() {
        None => {
            let directory = tempfile::tempdir().unwrap();
            let backend_cwd = directory.path().join("backend");
            let extension_cwd = directory.path().join("extension");
            fs::create_dir(&backend_cwd).unwrap();
            fs::create_dir(&extension_cwd).unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .env(STAGE, "backend")
                .env("HOME", "relative-home")
                .env(EXTENSION_CWD, &extension_cwd)
                .current_dir(&backend_cwd)
                .output()
                .unwrap();
            eprintln!(
                "backend stdout:\n{}\nbackend stderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.status.success(),
                "production relative-HOME backend child failed"
            );
        }
        Some("backend") => {
            assert_eq!(std::env::var_os("HOME").unwrap(), "relative-home");
            let socket = match socket_path() {
                Ok(socket) => socket,
                Err(error) => {
                    eprintln!("production rejected relative HOME: kind={:?}", error.kind());
                    assert_eq!(error.kind(), ErrorKind::InvalidInput);
                    return;
                }
            };
            assert!(
                !socket.is_absolute(),
                "production unexpectedly returned {socket:?}"
            );
            fs::create_dir_all(socket.parent().unwrap()).unwrap();
            let _listener = UnixListener::bind(&socket).unwrap();
            let control = UnixStream::connect(&socket).unwrap();
            println!("backend same-cwd control: connected");
            drop(control);
            let resource = resource_url(&socket, &[0xab; 32]);
            let encoded = resource
                .split("socket=")
                .nth(1)
                .unwrap()
                .split("&token=")
                .next()
                .unwrap();
            eprintln!("production resource URL: {resource}");
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .env(STAGE, "connect")
                .env(ENCODED_SOCKET, encoded)
                .current_dir(std::env::var_os(EXTENSION_CWD).unwrap())
                .output()
                .unwrap();
            eprintln!(
                "extension stdout:\n{}\nextension stderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.status.success(),
                "the extension process could not connect to the production resource socket"
            );
        }
        Some("connect") => {
            let relative = percent_decode(&std::env::var(ENCODED_SOCKET).unwrap());
            match UnixStream::connect(&relative) {
                Ok(_) => println!("relative resource socket: connected"),
                Err(error) => {
                    eprintln!(
                        "relative resource socket: kind={:?} raw={:?} path={relative:?}",
                        error.kind(),
                        error.raw_os_error()
                    );
                    std::process::exit(1);
                }
            }
        }
        Some(other) => panic!("unknown relative-HOME audit stage: {other}"),
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = char::from(bytes[index + 1]).to_digit(16).unwrap();
            let low = char::from(bytes[index + 2]).to_digit(16).unwrap();
            output.push(char::from(u8::try_from(high * 16 + low).unwrap()));
            index += 3;
        } else {
            output.push(char::from(bytes[index]));
            index += 1;
        }
    }
    output
}

#[test]
fn descriptor_exhaustion_is_retryable_for_accept() {
    if std::env::var_os(CHILD).is_none() {
        run_child(
            "mount::audit_tests::descriptor_exhaustion_is_retryable_for_accept",
            None,
        );
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let client = UnixStream::connect(&path).unwrap();
    let mut original = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut original) },
        0
    );
    let mut limited = original;
    limited.rlim_cur = limited.rlim_max.min(32);
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limited) },
        0
    );

    let mut held = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => held.push(file),
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                break;
            }
        }
    }
    let error = listener.accept().unwrap_err();
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const original) },
        0
    );
    drop(held);
    drop(client);
    eprintln!(
        "accept under EMFILE: kind={:?} raw={:?}",
        error.kind(),
        error.raw_os_error()
    );
    assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
    assert!(
        retryable_accept_error(&error),
        "EMFILE currently terminates the mount's accept loop permanently"
    );
}

fn run_sandboxed_child(test: &str) {
    run_sandboxed_child_with_home(test, Path::new("/tmp"));
}

fn run_sandboxed_child_with_home(test: &str, home: &Path) {
    let output = Command::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(DENY_MOUNT_AND_DISKUTIL)
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, "1")
        .env(AUDIT_HOME, home)
        .env("HOME", home)
        .output()
        .unwrap();
    eprintln!(
        "child stdout:\n{}\nchild stderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "sandboxed child {test} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_child(test: &str, home: Option<&Path>) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, "1");
    if let Some(home) = home {
        command.env(AUDIT_HOME, home).env("HOME", home);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "child {test} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
