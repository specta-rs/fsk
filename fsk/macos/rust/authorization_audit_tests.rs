//! Snapshot-only adversarial regression tests for the macOS authorization module.
use super::*;
use std::{
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Instant,
};

fn audit_app(directory: &Path) -> PathBuf {
    let app = directory.join("FSK.app");
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    fs::create_dir_all(app.join("Contents/Extensions/FSKExtension.appex/Contents")).unwrap();
    fs::write(app.join(APP_EXECUTABLE), b"").unwrap();
    fs::write(
        app.join("Contents/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{APP_BUNDLE_IDENTIFIER}</string>
</dict></plist>
"#
        ),
    )
    .unwrap();
    fs::write(
        app.join(EXTENSION_INFO_PLIST),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>{APP_PROTOCOL_MAJOR_KEY}</key><integer>{PROTOCOL_MAJOR}</integer>
<key>{APP_PROTOCOL_MINOR_KEY}</key><integer>0</integer>
</dict></plist>
"#
        ),
    )
    .unwrap();
    app
}

fn audit_reporting_app(directory: &Path, script: &str) -> PathBuf {
    let app = audit_app(directory);
    let executable = app.join(APP_EXECUTABLE);
    fs::write(&executable, script).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    app
}

fn audit_run_self(name: &str, envs: &[(&str, &std::ffi::OsStr)]) -> std::process::Output {
    let mut command = Command::new(env::current_exe().unwrap());
    command.args(["--exact", name, "--nocapture"]);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn audit_assert_child_success(name: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "child {name} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn audit_replacement_preserves_an_unrelated_directory_that_races_the_swap() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("FSK.app");
    let staging = directory.path().join("staged.app");
    let backup = directory.path().join("backup");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("marker"), b"old app").unwrap();
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();

    let result = replace_app_with(&staging, &destination, &backup, |staging, destination| {
        // Simulate a non-cooperating process replacing the destination after
        // install() checked it but before renamex_np executes.
        if fs::read(destination.join("marker")).ok().as_deref() == Some(b"old app") {
            fs::remove_dir_all(destination)?;
            fs::create_dir(destination)?;
            fs::write(destination.join("important"), b"keep me").unwrap();
        }
        atomic_swap(staging, destination)
    });

    assert_eq!(
        fs::read(destination.join("important")).unwrap_or_default(),
        b"keep me",
        "an unrelated directory was deleted; replacement result: {result:?}"
    );
    assert!(
        result.is_err(),
        "replacement should report the destination race"
    );
}

#[test]
fn audit_install_creates_a_private_parent_under_permissive_umask() {
    const CHILD: &str = "audit-permissions-child";
    if env::var_os("FSK_AUDIT_CHILD").as_deref() == Some(std::ffi::OsStr::new(CHILD)) {
        // SAFETY: this is a single-threaded child test process.
        unsafe { libc::umask(0) };
        let result = install_inner(&mut |_| {});
        assert!(
            result.is_err(),
            "malformed mirror URL should fail the download"
        );
        let parent = PathBuf::from(env::var_os("FSK_AUDIT_PARENT").unwrap());
        let mode = fs::metadata(parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "install parent must be private even with umask 000"
        );
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("new-parent");
    let destination = parent.join("FSK.app");
    let output = audit_run_self(
        "macos_authorization::audit_tests::audit_install_creates_a_private_parent_under_permissive_umask",
        &[
            ("FSK_AUDIT_CHILD", std::ffi::OsStr::new(CHILD)),
            ("FSK_AUDIT_PARENT", parent.as_os_str()),
            ("FSK_APP_PATH", destination.as_os_str()),
            ("FSK_RELEASE_BASE_URL", std::ffi::OsStr::new(":")),
        ],
    );
    audit_assert_child_success(
        "audit_install_creates_a_private_parent_under_permissive_umask",
        &output,
    );
}

#[test]
fn audit_default_status_verifies_the_release_signature_before_execution() {
    const CHILD: &str = "audit-default-status-child";
    if env::var_os("FSK_AUDIT_CHILD").as_deref() == Some(std::ffi::OsStr::new(CHILD)) {
        let marker = PathBuf::from(env::var_os("FSK_AUDIT_EXEC_MARKER").unwrap());
        let status = authorization_status().unwrap();
        assert!(
            !marker.exists(),
            "an unsigned bundle in the default release location was executed"
        );
        assert_eq!(
            status,
            AuthorizationStatus::NotInstalled,
            "an unsigned bundle in the default release location must not be executed and trusted"
        );
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("unsigned-executable-ran");
    let app = app_path_from(None, Some(home.path().as_os_str().into())).unwrap();
    audit_reporting_app(
        app.parent().unwrap(),
        "#!/bin/sh\n: > \"$FSK_AUDIT_EXEC_MARKER\"\nprintf '%s\\n' 'authorized'\n",
    );
    let output = audit_run_self(
        "macos_authorization::audit_tests::audit_default_status_verifies_the_release_signature_before_execution",
        &[
            ("FSK_AUDIT_CHILD", std::ffi::OsStr::new(CHILD)),
            ("HOME", home.path().as_os_str()),
            ("FSK_AUDIT_EXEC_MARKER", marker.as_os_str()),
        ],
    );
    audit_assert_child_success(
        "audit_default_status_verifies_the_release_signature_before_execution",
        &output,
    );
}

#[test]
fn audit_recovery_tries_an_older_valid_backup_when_the_newest_is_corrupt() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path();
    let destination = parent.join("FSK.app");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let valid = parent.join(format!(".FSK.app.backup-{}-1-0", now - 2_000));
    let corrupt = parent.join(format!(".FSK.app.backup-{}-1-0", now - 1_000));
    fs::create_dir(&valid).unwrap();
    fs::write(valid.join("marker"), b"valid previous app").unwrap();
    fs::create_dir(&corrupt).unwrap();
    fs::write(corrupt.join("marker"), b"corrupt newer backup").unwrap();

    recover_interrupted_install_with(&destination, |candidate| {
        if candidate == valid {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidData, "corrupt backup"))
        }
    })
    .unwrap();

    assert_eq!(
        fs::read(destination.join("marker")).unwrap_or_default(),
        b"valid previous app",
        "a corrupt newest backup must not hide an older recoverable app"
    );
}

struct AuditHttpsServer {
    child: std::process::Child,
    port: u16,
    _directory: tempfile::TempDir,
}

impl Drop for AuditHttpsServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn audit_fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../audit/fixtures")
        .join(name)
        .canonicalize()
        .unwrap()
}

fn audit_start_https_server(mode: &str, payload: Option<&Path>) -> AuditHttpsServer {
    let directory = tempfile::tempdir().unwrap();
    let portfile = directory.path().join("port");
    let mut command = Command::new("/usr/bin/python3");
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../audit/https_fixture.py"))
        .arg(mode)
        .arg(&portfile)
        .arg(payload.unwrap_or_else(|| Path::new("")))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let port = loop {
        if let Ok(port) = fs::read_to_string(&portfile)
            .and_then(|value| value.trim().parse::<u16>().map_err(io::Error::other))
        {
            break port;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("HTTPS fixture exited before publishing its port: {status}");
        }
        assert!(Instant::now() < deadline, "HTTPS fixture did not start");
        thread::sleep(Duration::from_millis(10));
    };
    AuditHttpsServer {
        child,
        port,
        _directory: directory,
    }
}

fn audit_tree_size(path: &Path) -> u64 {
    let metadata = fs::symlink_metadata(path).unwrap();
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return metadata.len();
    }
    let mut total = 0;
    for entry in fs::read_dir(path).unwrap() {
        total += audit_tree_size(&entry.unwrap().path());
    }
    total
}

#[test]
fn audit_download_rejects_an_announced_oversized_archive() {
    const CHILD: &str = "audit-download-limit-child";
    if env::var_os("FSK_AUDIT_CHILD").as_deref() == Some(std::ffi::OsStr::new(CHILD)) {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("FSK.zip");
        let error = download_release(temporary.path(), &archive, "v0.2.0").unwrap_err();
        assert!(
            error.to_string().contains("Maximum file size exceeded"),
            "curl accepted an archive declared above the 256 MiB policy: {error}"
        );
        return;
    }

    let external_base = env::var_os("FSK_AUDIT_HTTPS_BASE");
    let server = external_base
        .is_none()
        .then(|| audit_start_https_server("oversized", None));
    let base = external_base.map_or_else(
        || format!("https://127.0.0.1:{}", server.as_ref().unwrap().port),
        |value| value.to_string_lossy().into_owned(),
    );
    let certificate = audit_fixture_path("cert.pem");
    let output = audit_run_self(
        "macos_authorization::audit_tests::audit_download_rejects_an_announced_oversized_archive",
        &[
            ("FSK_AUDIT_CHILD", std::ffi::OsStr::new(CHILD)),
            ("FSK_RELEASE_BASE_URL", std::ffi::OsStr::new(&base)),
            ("CURL_CA_BUNDLE", certificate.as_os_str()),
            ("NO_PROXY", std::ffi::OsStr::new("*")),
            ("no_proxy", std::ffi::OsStr::new("*")),
        ],
    );
    audit_assert_child_success(
        "audit_download_rejects_an_announced_oversized_archive",
        &output,
    );
}

#[test]
fn audit_extraction_rejects_an_archive_above_the_expanded_size_limit() {
    const CHILD: &str = "audit-expansion-limit-child";
    if env::var_os("FSK_AUDIT_CHILD").as_deref() == Some(std::ffi::OsStr::new(CHILD)) {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("destination/FSK.app");
        let result = install_from_release(temporary.path(), &destination, &mut |_| {});
        assert!(result.is_err(), "the fixture is not a signed FSK.app");
        let expanded = audit_tree_size(&temporary.path().join("extracted"));
        assert!(
            expanded <= 256 * 1024 * 1024,
            "ditto expanded {expanded} bytes before signature verification"
        );
        return;
    }

    let archive = audit_fixture_path("expansion-bomb.zip");
    let external_base = env::var_os("FSK_AUDIT_HTTPS_BASE");
    let server = external_base
        .is_none()
        .then(|| audit_start_https_server("file", Some(&archive)));
    let base = external_base.map_or_else(
        || format!("https://127.0.0.1:{}", server.as_ref().unwrap().port),
        |value| value.to_string_lossy().into_owned(),
    );
    let certificate = audit_fixture_path("cert.pem");
    let output = audit_run_self(
        "macos_authorization::audit_tests::audit_extraction_rejects_an_archive_above_the_expanded_size_limit",
        &[
            ("FSK_AUDIT_CHILD", std::ffi::OsStr::new(CHILD)),
            ("FSK_RELEASE_BASE_URL", std::ffi::OsStr::new(&base)),
            ("CURL_CA_BUNDLE", certificate.as_os_str()),
            ("NO_PROXY", std::ffi::OsStr::new("*")),
            ("no_proxy", std::ffi::OsStr::new("*")),
        ],
    );
    audit_assert_child_success(
        "audit_extraction_rejects_an_archive_above_the_expanded_size_limit",
        &output,
    );
}

#[test]
fn audit_uninstall_terminates_a_running_companion_process() {
    let directory = tempfile::tempdir().unwrap();
    let app = audit_app(directory.path());
    let executable = app.join(APP_EXECUTABLE);
    fs::copy(audit_fixture_path("audit-sleeper"), &executable).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let ready = directory.path().join("ready");
    let mut child = Command::new(&executable).arg(&ready).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "fixture process did not become ready"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "fixture process exited before uninstall"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let removed = uninstall_app_at(&app).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let exited = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(25));
    };
    if !exited {
        let _ = child.kill();
    }
    let _ = child.wait();
    assert!(removed, "the bundle should be removed");
    assert!(
        exited,
        "uninstall left the companion process running after deleting its bundle"
    );
}

#[test]
fn audit_authorization_status_has_a_bounded_execution_time() {
    let directory = tempfile::tempdir().unwrap();
    let pidfile = directory.path().join("pid");
    let quoted_pidfile = format!("'{}'", pidfile.display().to_string().replace('\'', "'\''"));
    let app = audit_reporting_app(
        directory.path(),
        &format!("#!/bin/sh\nprintf '%s\\n' \"$$\" > {quoted_pidfile}\nexec /bin/sleep 30\n"),
    );
    let (sender, receiver) = mpsc::channel::<io::Result<AuthorizationStatus>>();
    let status_app = app.clone();
    let worker = thread::spawn(move || {
        let _ = sender.send(authorization_status_at(&status_app));
    });

    let received = receiver.recv_timeout(Duration::from_secs(7));
    if received.is_err() {
        // Current source has no timeout. Stop the fixture so the failing test
        // remains bounded and deterministic.
        let deadline = Instant::now() + Duration::from_secs(1);
        let pid = loop {
            if let Ok(pid) = fs::read_to_string(&pidfile)
                .map_err(io::Error::other)
                .and_then(|value| value.trim().parse::<i32>().map_err(io::Error::other))
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "status fixture did not start");
            thread::sleep(Duration::from_millis(10));
        };
        // SAFETY: pid belongs to the sleep fixture created above.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let _ = worker.join().unwrap();
    assert!(
        received.is_ok(),
        "authorization status exceeded its 7-second test bound"
    );
    let result = received.unwrap();
    assert_eq!(
        result.unwrap_err().kind(),
        io::ErrorKind::TimedOut,
        "a hung companion must produce a timeout, not block wait_for_authorization forever"
    );
}
