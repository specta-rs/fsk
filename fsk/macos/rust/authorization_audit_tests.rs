//! Snapshot-only adversarial regression tests for the macOS authorization module.
use super::*;
use std::{
    cell::Cell,
    os::unix::fs::{PermissionsExt, symlink},
    process::{Command, Stdio},
    sync::{Mutex as StdMutex, mpsc},
    thread,
    time::Instant,
};

// /usr/bin/python3 is an Xcode shim on macOS. Serialize fixture startup so two
// parallel audit tests do not race its toolchain lookup.
static HTTPS_SERVER_START: StdMutex<()> = StdMutex::new(());

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
    command.args(["--exact", "--include-ignored", name, "--nocapture"]);
    // Never let an ambient developer override or credential redirect a child
    // fixture outside its temporary bundle/home or to a real release service.
    for key in [
        "FSK_APP_PATH",
        "FSK_RELEASE_BASE_URL",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "FSK_AUDIT_CHILD",
        "FSK_AUDIT_PARENT",
        "FSK_AUDIT_EXEC_MARKER",
        "FSK_AUDIT_HTTPS_BASE",
    ] {
        command.env_remove(key);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn audit_assert_child_success(name: &str, output: &std::process::Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "child {name} failed\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("1 passed"),
        "child {name} did not execute exactly one test\nstdout:\n{stdout}"
    );
}

#[test]
fn audit_replacement_preserves_an_unrelated_directory_that_races_the_swap() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("FSK.app");
    let staging = directory.path().join("staged.app");
    let backup = directory.path().join("backup");
    audit_app(directory.path());
    fs::write(destination.join("marker"), b"old app").unwrap();
    fs::create_dir_all(&staging).unwrap();
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
fn audit_replacement_refuses_an_unrelated_destination_before_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("FSK.app");
    let staging = directory.path().join("staged.app");
    let backup = directory.path().join("backup");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("important"), b"keep me").unwrap();
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();

    assert!(replace_app_with(&staging, &destination, &backup, atomic_swap).is_err());

    assert_eq!(fs::read(destination.join("important")).unwrap(), b"keep me");
    assert!(
        !staging.exists(),
        "the rejected staged app should be cleaned up"
    );
}

#[test]
fn audit_staging_cleanup_preserves_a_replaced_container() {
    let directory = tempfile::tempdir().unwrap();
    let staging = directory.path().join("staging");
    let staged_app = staging.join("FSK.app");
    fs::create_dir_all(&staged_app).unwrap();
    fs::write(staged_app.join("marker"), b"new app").unwrap();
    let container_identity = path_identity(&staging).unwrap();
    let staged_app_identity = path_identity(&staged_app).unwrap();

    fs::remove_dir_all(&staging).unwrap();
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("important"), b"keep me").unwrap();

    clean_staging_container(
        &staging,
        &staged_app,
        container_identity,
        staged_app_identity,
    );

    assert_eq!(
        fs::read(staging.join("important")).unwrap(),
        b"keep me",
        "staging cleanup deleted a replacement container"
    );
}

fn audit_validate_staging_marker(staging: &Path) -> io::Result<()> {
    if fs::read(staging.join("marker"))? == b"new app" {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staging marker changed",
        ))
    }
}

#[test]
fn audit_install_replacement_revalidates_the_staging_container() {
    let directory = tempfile::tempdir().unwrap();
    let destination = audit_app(directory.path());
    fs::write(destination.join("marker"), b"old app").unwrap();
    let container = directory.path().join("staging");
    let staging = container.join("FSK.app");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();
    let backup = directory.path().join("backup");
    let destination_identity = path_identity(&destination).unwrap();
    let staging_identity = path_identity(&staging).unwrap();
    let container_identity = directory_identity(&container).unwrap();
    let noop = |_: &Path, _: &Path| {};

    let result = replace_app_with_expected_hooks(
        &staging,
        &destination,
        &backup,
        Some(destination_identity),
        StagingExpectation {
            identity: staging_identity,
            container_identity: Some(container_identity),
            validate: audit_validate_staging_marker,
        },
        atomic_swap,
        ReplacementHooks {
            before_mutation: &|staging, _| {
                if fs::read(staging.join("marker")).ok().as_deref() == Some(b"new app") {
                    let container = staging.parent().unwrap();
                    fs::remove_dir_all(container).unwrap();
                    audit_app(container);
                    fs::write(container.join("important"), b"keep me").unwrap();
                }
            },
            after_fallback_move: &noop,
            after_swap: &noop,
        },
    );

    assert!(
        result.is_err(),
        "a replaced staging container must fail install"
    );
    assert_eq!(fs::read(destination.join("marker")).unwrap(), b"old app");
    assert!(
        !backup.exists(),
        "the old app must not be backed up or deleted"
    );
    assert_eq!(
        fs::read(container.join("important")).unwrap(),
        b"keep me",
        "the unrelated FSK.app-like staging container must survive"
    );
}

#[test]
fn audit_slow_download_refuses_a_destination_replaced_during_download() {
    let directory = tempfile::tempdir().unwrap();
    let destination = audit_app(directory.path());
    fs::write(destination.join("marker"), b"old app").unwrap();

    let error = install_from_release_with_download(
        directory.path(),
        &destination,
        |_, _, _| {
            thread::sleep(Duration::from_millis(50));
            fs::remove_dir_all(&destination)?;
            fs::create_dir(&destination)?;
            fs::write(destination.join("important"), b"keep me").unwrap();
            Ok(())
        },
        &mut |_| {},
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("changed during installation"),
        "expected a destination race, got {error}"
    );
    assert_eq!(fs::read(destination.join("important")).unwrap(), b"keep me");
    assert!(!directory.path().join("extracted").exists());
}

#[test]
fn audit_swap_verifies_the_destination_contains_the_exact_staged_app() {
    let directory = tempfile::tempdir().unwrap();
    let destination = audit_app(directory.path());
    fs::write(destination.join("marker"), b"old app").unwrap();
    let staging = directory.path().join("staged.app");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();
    let backup = directory.path().join("backup");
    let destination_identity = path_identity(&destination).unwrap();
    let staging_identity = path_identity(&staging).unwrap();

    let result = replace_app_with_expected(
        &staging,
        &destination,
        &backup,
        Some(destination_identity),
        StagingExpectation::unchecked(staging_identity),
        |staging, destination| {
            if fs::read(staging.join("marker")).ok().as_deref() == Some(b"new app") {
                fs::remove_dir_all(staging)?;
                fs::create_dir(staging)?;
                fs::write(staging.join("important"), b"keep me").unwrap();
            }
            atomic_swap(staging, destination)
        },
    );

    assert!(result.is_err(), "a staged-app race must be reported");
    assert_eq!(fs::read(destination.join("marker")).unwrap(), b"old app");
    assert_eq!(
        fs::read(staging.join("important")).unwrap(),
        b"keep me",
        "the unrelated staging replacement must be preserved"
    );
}

#[test]
fn audit_swap_verifies_both_sides_when_destination_changes_after_exchange() {
    let directory = tempfile::tempdir().unwrap();
    let destination = audit_app(directory.path());
    fs::write(destination.join("marker"), b"old app").unwrap();
    let staging = directory.path().join("staged.app");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();
    let backup = directory.path().join("backup");
    let destination_identity = path_identity(&destination).unwrap();
    let staging_identity = path_identity(&staging).unwrap();

    let result = replace_app_with_expected(
        &staging,
        &destination,
        &backup,
        Some(destination_identity),
        StagingExpectation::unchecked(staging_identity),
        |staging, destination| {
            atomic_swap(staging, destination)?;
            if fs::read(destination.join("marker")).ok().as_deref() == Some(b"new app") {
                fs::remove_dir_all(destination)?;
                fs::create_dir(destination)?;
                fs::write(destination.join("important"), b"keep me").unwrap();
            }
            Ok(())
        },
    );

    assert!(result.is_err(), "a destination-side race must be reported");
    assert_eq!(fs::read(destination.join("marker")).unwrap(), b"old app");
    assert_eq!(fs::read(staging.join("important")).unwrap(), b"keep me");
}

#[test]
fn audit_after_swap_destination_replacement_restores_the_previous_app() {
    let directory = tempfile::tempdir().unwrap();
    let destination = audit_app(directory.path());
    fs::write(destination.join("marker"), b"old app").unwrap();
    let container = directory.path().join("staging");
    let staging = container.join("FSK.app");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();
    let backup = directory.path().join("backup");
    let destination_identity = path_identity(&destination).unwrap();
    let staging_identity = path_identity(&staging).unwrap();
    let container_identity = directory_identity(&container).unwrap();
    let noop = |_: &Path, _: &Path| {};

    let result = replace_app_with_expected_hooks(
        &staging,
        &destination,
        &backup,
        Some(destination_identity),
        StagingExpectation {
            identity: staging_identity,
            container_identity: Some(container_identity),
            validate: audit_validate_staging_marker,
        },
        atomic_swap,
        ReplacementHooks {
            before_mutation: &noop,
            after_fallback_move: &noop,
            after_swap: &|_, destination| {
                fs::remove_dir_all(destination).unwrap();
                fs::create_dir(destination).unwrap();
                fs::write(destination.join("important"), b"keep me").unwrap();
            },
        },
    );

    assert!(
        result.is_err(),
        "the replaced destination must fail acceptance"
    );
    assert_eq!(
        fs::read(destination.join("marker")).unwrap(),
        b"old app",
        "the previous app must be restored out of the install container"
    );
    assert!(
        !staging.exists(),
        "the old app must no longer live in staging"
    );
    let preserved = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".FSK.app.race-"))
        })
        .expect("the unexpected destination should be preserved");
    assert_eq!(fs::read(preserved.join("important")).unwrap(), b"keep me");
}

#[test]
fn audit_failed_swap_back_preserves_a_non_expiring_recovery_path() {
    use std::fs::FileTimes;

    let directory = tempfile::tempdir().unwrap();
    let destination = audit_app(directory.path());
    fs::write(destination.join("marker"), b"old app").unwrap();
    let staging = directory.path().join("staged.app");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();
    let backup = directory.path().join("backup");
    let destination_identity = path_identity(&destination).unwrap();
    let staging_identity = path_identity(&staging).unwrap();
    let swaps = Cell::new(0_usize);
    let noop = |_: &Path, _: &Path| {};

    let result = replace_app_with_expected_hooks(
        &staging,
        &destination,
        &backup,
        Some(destination_identity),
        StagingExpectation {
            identity: staging_identity,
            container_identity: None,
            validate: audit_validate_staging_marker,
        },
        |staging, destination| {
            if swaps.get() == 0 {
                swaps.set(1);
                atomic_swap(staging, destination)
            } else {
                Err(io::Error::from_raw_os_error(libc::ENOTSUP))
            }
        },
        ReplacementHooks {
            before_mutation: &noop,
            after_fallback_move: &noop,
            after_swap: &|_, destination| {
                fs::write(destination.join("marker"), b"invalid").unwrap();
            },
        },
    );

    let error = result.unwrap_err();
    let recovery = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".FSK.app.recovery-"))
        })
        .expect("the previous app should be preserved in a dedicated recovery entry");
    assert!(
        error
            .to_string()
            .contains(recovery.to_string_lossy().as_ref()),
        "the exact recovery path must be reported: {error}"
    );
    assert_eq!(fs::read(recovery.join("marker")).unwrap(), b"old app");
    assert_eq!(fs::read(destination.join("marker")).unwrap(), b"invalid");

    fs::File::open(&recovery)
        .unwrap()
        .set_times(
            FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(48 * 60 * 60)),
        )
        .unwrap();
    for kind in ["install", "backup", "race"] {
        cleanup_stale_entries(directory.path(), "FSK.app", kind).unwrap();
    }
    assert!(
        recovery.exists(),
        "normal stale cleanup must not expire recovery"
    );

    recover_interrupted_install_with(&destination, |_| {
        Err(io::Error::new(io::ErrorKind::InvalidData, "occupied"))
    })
    .unwrap();
    assert!(
        recovery.exists(),
        "an occupied destination must retain recovery"
    );
    assert_eq!(fs::read(destination.join("marker")).unwrap(), b"invalid");

    fs::remove_dir_all(&destination).unwrap();
    let recovery_to_restore = recovery.clone();
    recover_interrupted_install_with(&destination, |candidate| {
        if candidate == recovery_to_restore {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidData, "unrelated"))
        }
    })
    .unwrap();
    assert_eq!(fs::read(destination.join("marker")).unwrap(), b"old app");
    assert!(!recovery.exists(), "restored recovery should be consumed");
}

#[test]
fn audit_fallback_validation_failure_after_backup_move_restores_the_previous_app() {
    let directory = tempfile::tempdir().unwrap();
    let destination = audit_app(directory.path());
    fs::write(destination.join("marker"), b"old app").unwrap();
    let staging = directory.path().join("staged.app");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();
    let backup = directory.path().join("backup");
    let destination_identity = path_identity(&destination).unwrap();
    let staging_identity = path_identity(&staging).unwrap();
    let calls = Cell::new(0_usize);
    let noop = |_: &Path, _: &Path| {};

    let result = replace_app_with_expected_hooks(
        &staging,
        &destination,
        &backup,
        Some(destination_identity),
        StagingExpectation {
            identity: staging_identity,
            container_identity: None,
            validate: audit_validate_staging_marker,
        },
        |_, _| Err(io::Error::from_raw_os_error(libc::ENOTSUP)),
        ReplacementHooks {
            before_mutation: &|staging, _| {
                if calls.get() == 2 {
                    fs::write(staging.join("marker"), b"invalid").unwrap();
                }
                calls.set(calls.get() + 1);
            },
            after_fallback_move: &noop,
            after_swap: &noop,
        },
    );

    let error = result.unwrap_err();
    assert!(
        error.to_string().contains("staging marker changed"),
        "expected the original validation error, got {error}"
    );
    assert_eq!(fs::read(destination.join("marker")).unwrap(), b"old app");
    assert!(!backup.exists(), "the restored backup should be consumed");
    assert!(
        !staging.exists(),
        "the owned invalid staged app should be removed"
    );
}

#[test]
fn audit_fallback_final_race_preserves_the_destination_and_restores_the_backup() {
    let directory = tempfile::tempdir().unwrap();
    let destination = audit_app(directory.path());
    fs::write(destination.join("marker"), b"old app").unwrap();
    let staging = directory.path().join("staged.app");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();
    let backup = directory.path().join("backup");
    let destination_identity = path_identity(&destination).unwrap();
    let staging_identity = path_identity(&staging).unwrap();
    let noop = |_: &Path, _: &Path| {};

    let result = replace_app_with_expected_hooks(
        &staging,
        &destination,
        &backup,
        Some(destination_identity),
        StagingExpectation::unchecked(staging_identity),
        |_, _| Err(io::Error::from_raw_os_error(libc::ENOTSUP)),
        ReplacementHooks {
            before_mutation: &noop,
            after_fallback_move: &|_, destination| {
                assert_eq!(fs::read(destination.join("marker")).unwrap(), b"new app");
                fs::remove_dir_all(destination).unwrap();
                fs::create_dir(destination).unwrap();
                fs::write(destination.join("important"), b"keep me").unwrap();
            },
            after_swap: &noop,
        },
    );

    assert!(
        result.is_err(),
        "the final destination race must be reported"
    );
    assert_eq!(
        fs::read(destination.join("marker")).unwrap(),
        b"old app",
        "the previous app was not restored from the backup"
    );
    assert!(!backup.exists(), "the restored backup should be consumed");
    let preserved = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".FSK.app.race-"))
        })
        .expect("the unexpected destination should be preserved");
    assert_eq!(fs::read(preserved.join("important")).unwrap(), b"keep me");
}

#[test]
fn audit_fallback_final_race_without_a_previous_app_leaves_destination_absent() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("FSK.app");
    let staging = directory.path().join("staged.app");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();
    let backup = directory.path().join("backup");
    let staging_identity = path_identity(&staging).unwrap();
    let noop = |_: &Path, _: &Path| {};

    let result = replace_app_with_expected_hooks(
        &staging,
        &destination,
        &backup,
        None,
        StagingExpectation::unchecked(staging_identity),
        atomic_swap,
        ReplacementHooks {
            before_mutation: &noop,
            after_fallback_move: &|_, destination| {
                fs::remove_dir_all(destination).unwrap();
                fs::create_dir(destination).unwrap();
                fs::write(destination.join("important"), b"keep me").unwrap();
            },
            after_swap: &noop,
        },
    );

    assert!(
        result.is_err(),
        "the final destination race must be reported"
    );
    assert!(!destination.exists(), "no previous app exists to restore");
    assert!(!backup.exists());
    let preserved = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".FSK.app.race-"))
        })
        .expect("the unexpected destination should be preserved");
    assert_eq!(fs::read(preserved.join("important")).unwrap(), b"keep me");
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

#[test]
fn audit_recovery_rechecks_backup_identity_after_validation() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path();
    let destination = parent.join("FSK.app");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let valid = parent.join(format!(".FSK.app.backup-{}-1-0", now - 2_000));
    let raced = parent.join(format!(".FSK.app.backup-{}-1-0", now - 1_000));
    fs::create_dir(&valid).unwrap();
    fs::write(valid.join("marker"), b"valid previous app").unwrap();
    fs::create_dir(&raced).unwrap();
    fs::write(raced.join("marker"), b"newer app").unwrap();

    recover_interrupted_install_with(&destination, |candidate| {
        if candidate == raced {
            fs::remove_dir_all(candidate)?;
            fs::create_dir(candidate)?;
            fs::write(candidate.join("important"), b"keep me").unwrap();
        }
        Ok(())
    })
    .unwrap();

    assert_eq!(
        fs::read(destination.join("marker")).unwrap(),
        b"valid previous app",
        "an unrelated replacement backup must not be installed"
    );
    assert_eq!(
        fs::read(raced.join("important")).unwrap(),
        b"keep me",
        "the unrelated replacement backup must be preserved"
    );
    assert!(!valid.exists(), "the valid older backup should be restored");
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
        .join("macos/rust/audit/fixtures")
        .join(name)
        .canonicalize()
        .unwrap()
}

fn audit_start_https_server(mode: &str, payload: Option<&Path>) -> AuditHttpsServer {
    let _startup = HTTPS_SERVER_START
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let portfile = directory.path().join("port");
    let mut command = Command::new("/usr/bin/python3");
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("macos/rust/audit/https_fixture.py"))
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

#[test]
#[ignore = "loopback HTTPS fixture; run explicitly via fsk/macos/check.sh"]
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

    let server = audit_start_https_server("oversized", None);
    let base = format!("https://127.0.0.1:{}", server.port);
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
#[ignore = "loopback HTTPS fixture; run explicitly via fsk/macos/check.sh"]
fn audit_extraction_rejects_an_archive_above_the_expanded_size_limit() {
    const CHILD: &str = "audit-expansion-limit-child";
    if env::var_os("FSK_AUDIT_CHILD").as_deref() == Some(std::ffi::OsStr::new(CHILD)) {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("destination/FSK.app");
        let result = install_from_release(temporary.path(), &destination, &mut |_| {});
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("exceeds extraction limits"),
            "expected the archive expansion limit, got {error}"
        );
        assert!(
            !temporary.path().join("extracted").exists(),
            "a rejected archive must not leave an extracted tree"
        );
        return;
    }

    let archive = audit_fixture_path("expansion-bomb.zip");
    let server = audit_start_https_server("file", Some(&archive));
    let base = format!("https://127.0.0.1:{}", server.port);
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

fn audit_start_companion(directory: &Path) -> (PathBuf, std::process::Child) {
    let app = audit_app(directory);
    let executable = app.join(APP_EXECUTABLE);
    fs::copy(audit_fixture_path("audit-sleeper"), &executable).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let ready = directory.join("ready");
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
    (app, child)
}

fn audit_wait_for_companion_exit(child: &mut std::process::Child) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    child.try_wait().unwrap().is_some()
}

#[test]
fn audit_uninstall_terminates_a_running_companion_process() {
    let directory = tempfile::tempdir().unwrap();
    let (app, mut child) = audit_start_companion(directory.path());

    let removed = uninstall_app_at(&app).unwrap();
    let exited = audit_wait_for_companion_exit(&mut child);
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
fn audit_symlinked_parent_uninstall_terminates_the_running_companion() {
    let directory = tempfile::tempdir().unwrap();
    let real_parent = directory.path().join("real");
    let linked_parent = directory.path().join("linked");
    fs::create_dir(&real_parent).unwrap();
    symlink(&real_parent, &linked_parent).unwrap();
    let (app, mut child) = audit_start_companion(&linked_parent);

    assert!(uninstall_app_at(&app).unwrap());

    assert!(
        audit_wait_for_companion_exit(&mut child),
        "canonical process matching did not stop the companion through a symlinked parent"
    );
    let _ = child.wait();
}

#[test]
fn audit_symlinked_parent_update_terminates_the_running_companion() {
    let directory = tempfile::tempdir().unwrap();
    let real_parent = directory.path().join("real");
    let linked_parent = directory.path().join("linked");
    fs::create_dir(&real_parent).unwrap();
    symlink(&real_parent, &linked_parent).unwrap();
    let (app, mut child) = audit_start_companion(&linked_parent);
    let staging = directory.path().join("staged.app");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("marker"), b"new app").unwrap();
    let backup = directory.path().join("backup");
    let destination_identity = path_identity(&app).unwrap();
    let staging_identity = path_identity(&staging).unwrap();

    replace_app_with_expected(
        &staging,
        &app,
        &backup,
        Some(destination_identity),
        StagingExpectation::unchecked(staging_identity),
        atomic_swap,
    )
    .unwrap();

    assert!(
        audit_wait_for_companion_exit(&mut child),
        "update left the old companion running through a symlinked parent"
    );
    let _ = child.wait();
    assert_eq!(fs::read(app.join("marker")).unwrap(), b"new app");
}

#[test]
fn audit_final_app_symlink_uninstall_never_stops_its_target() {
    let directory = tempfile::tempdir().unwrap();
    let target_parent = directory.path().join("target");
    fs::create_dir(&target_parent).unwrap();
    let (target_app, mut child) = audit_start_companion(&target_parent);
    let linked_app = directory.path().join("FSK.app");
    symlink(&target_app, &linked_app).unwrap();

    assert!(uninstall_app_at(&linked_app).unwrap());

    assert!(
        child.try_wait().unwrap().is_none(),
        "uninstall followed the final app symlink and stopped its target"
    );
    let _ = child.kill();
    let _ = child.wait();
    assert!(target_app.exists());
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

    let status_result = receiver.recv_timeout(Duration::from_secs(7));
    if status_result.is_err() {
        // If the timeout regresses, stop the fixture so the failing test stays
        // bounded and deterministic.
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
    worker.join().unwrap();
    assert!(
        status_result.is_ok(),
        "authorization status exceeded its 7-second test bound"
    );
    let result = status_result.unwrap();
    assert_eq!(
        result.unwrap_err().kind(),
        io::ErrorKind::TimedOut,
        "a hung companion must produce a timeout, not block wait_for_authorization forever"
    );
}

#[test]
fn audit_validation_commands_share_the_bounded_output_helper() {
    let started = Instant::now();
    let mut command = Command::new("/bin/sleep");
    command.arg("30");
    let error = output_with_timeout(
        &mut command,
        Duration::from_millis(50),
        1024,
        "validation fixture",
    )
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("validation fixture"));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "bounded command cleanup blocked after the deadline"
    );
}
