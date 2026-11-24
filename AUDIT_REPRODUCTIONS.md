# Audit reproductions

This branch is a **test-only reproduction branch**, not a fix PR. It adds regression tests and test-only scheduling hooks to main at defaaf4. The tests are expected to fail until the corresponding bugs are fixed. No production fix from the audit snapshots is included.

Run commands from the repository root. Tests do not mount real volumes or use real GitHub/Google credentials.

## macOS IPC, mount, and authorization

    cargo test -p fsk ipc::audit_tests:: -- --nocapture --test-threads=1
    cargo test -p fsk mount::audit_tests:: -- --nocapture --test-threads=1
    cargo test -p fsk macos_authorization::audit_tests:: -- --nocapture --test-threads=1

The mount tests deny /sbin/mount and diskutil in sandboxed child processes where needed. The failed-unmount control uses a fresh temporary directory that is not a mount point. Authorization fixtures use temporary homes/bundles and loopback HTTPS only.

## Example backends

    cargo test -p fsk-memory-backend audit_ -- --nocapture --test-threads=1
    mkdir -p .audit-home .audit-tmp
    HOME="$PWD/.audit-home" TMPDIR="$PWD/.audit-tmp" cargo test -p fsk-network-backend audit_ -- --nocapture --test-threads=1
    cargo test -p fsk-github-filesystem -- --nocapture
    cargo test -p fsk-google-drive -- --nocapture

The memory race tests use a test-only barrier to force the exact inode-reclamation interleaving. The network tests use an isolated HOME so they do not touch the user's cache.

## Swift FSKit companion

    ./audit/run-tests.sh --no-parallel
    ./audit/run-probes.sh

The scripts compile the actual Swift sources with the installed Xcode toolchain and run the local test/probe binaries under a macOS sandbox. They do not launch or register an extension.

## README and release workflow

    FSK_AUDIT_ROOT="$PWD" ./repros/reproduce_readme_authorization.sh x86_64-unknown-linux-gnu
    FSK_AUDIT_ROOT="$PWD" ./repros/reproduce_readme_authorization.sh x86_64-pc-windows-msvc
    FSK_AUDIT_ROOT="$PWD" ./repros/reproduce_release_version.sh

The first two commands require the corresponding Rust targets. The release test extracts and executes the workflow's actual version-matching shell against a fixture changelog.

## Windows logic and imports

These tests run on the host by extracting the pure production helpers from fsk/src/windows.rs; the import test cross-compiles the real crate to a Windows COFF object.

    FSK_AUDIT_SOURCE_ROOT="$PWD" CARGO_BUILD_RUSTC_WRAPPER='' CARGO_TARGET_DIR="$PWD/repros/windows/target" cargo test --manifest-path repros/windows/logic/Cargo.toml --test windows_contracts
    ./repros/windows/check_no_static_projfs_import.sh

The import test requires x86_64-pc-windows-msvc and strings. No Windows runtime is required.

## Linux FUSE callbacks and authorization

Run on Linux (or in a Linux container with the Rust toolchain):

    cargo test -p fsk linux::tests::audit:: -- --nocapture --test-threads=1
    cargo test -p fsk authorization::imp::tests::authorization_requires_device_open_not_just_metadata -- --exact --nocapture

The FUSE harness drives real fuser request/reply encoding over a Unix socketpair; it does not need /dev/fuse. The authorization regression expects a headless runner without a controlling TTY.
