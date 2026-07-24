# FSBind

FSBind is a working macOS FSKit extension whose filesystem implementation lives
in Rust. It provides:

- a small Rust `Filesystem` trait for downstream backends;
- a `NetworkFilesystem: Filesystem` trait for lazy remote volumes and
  backend-owned downloaded files;
- a stable, versioned binary protocol over authenticated Unix sockets;
- a reusable Swift adapter that can serve any number of Rust processes;
- a concurrent in-memory backend that is useful as a starter implementation;
- a host app and ExtensionKit extension generated with XcodeGen;
- Rust and Swift cross-language tests.

The Swift app is built and approved once. Each Rust CLI then owns its filesystem
implementation and mount lifetime independently.

## Requirements

- macOS 26 or later
- Xcode 26 with the macOS 26 SDK
- Rust 1.85 or later with the target for the build architecture
- XcodeGen 2.45 or later
- an Apple development team that can sign the
  `com.apple.developer.fskit.fsmodule` entitlement

The macOS 26 requirement comes from the path-URL resource used to mount a
resource-independent virtual filesystem. The Rust crate itself has no Apple
framework dependency.

## Build and test

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
xcodegen generate
xcodebuild \
  -project FSBind.xcodeproj \
  -scheme FSBindApp \
  -derivedDataPath DerivedData \
  CODE_SIGNING_ALLOWED=NO \
  test
```

Or run `scripts/check.sh`. Code-signing is intentionally disabled for CI tests;
mounting requires a normally signed build from Xcode.

## Install the bridge and mount the example

1. Open `FSBind.xcodeproj`, select a development team for both the app and
   extension, then build and run `FSBindApp`.
2. In **System Settings > General > Login Items & Extensions > File System
   Extensions**, enable **FSBind virtual file system**.
3. Build and run the Rust example:

```sh
mkdir -p "$HOME/fsbind-mount"
cargo run -p fsbind-memory-backend -- "$HOME/fsbind-mount"
```

In another terminal:

```sh
ls -la "$HOME/fsbind-mount"
cat "$HOME/fsbind-mount/hello.txt"
```

Press Control-C in the first terminal to unmount. `MountSession` owns the
registration: dropping it unmounts the path, stops its IPC service, closes
backend leases, and removes the private socket.

## Create a downstream filesystem

Implement `Filesystem`, then keep a `MountSession` alive in your CLI:

```rust
use fsbind::{Filesystem, MountSession};

struct MyCoolFilesystem;

impl Filesystem for MyCoolFilesystem {
    // Implement the required operations.
    # fn statfs(&self) -> fsbind::Result<fsbind::StatFs> { unimplemented!() }
    # fn metadata(&self, _: u64) -> fsbind::Result<fsbind::Metadata> { unimplemented!() }
    # fn lookup(&self, _: u64, _: &[u8]) -> fsbind::Result<(u64, fsbind::FileType)> { unimplemented!() }
    # fn read_directory(&self, _: u64, _: u64, _: u64, _: &mut dyn fsbind::DirectorySink) -> fsbind::Result<fsbind::ReadDirectoryResult> { unimplemented!() }
    # fn read(&self, _: u64, _: u64, _: &mut [u8]) -> fsbind::Result<usize> { unimplemented!() }
}

let mount = MountSession::mount(MyCoolFilesystem, "/Volumes/MyCoolFilesystem")?;
// Run the CLI. Dropping `mount` unmounts it.
# Ok::<(), std::io::Error>(())
```

For a database, remote service, archive, or content-addressed store, implement
`fsbind::Filesystem` on your own `Send + Sync` type. Read-only backends only
need to implement `statfs`, `metadata`, `lookup`, `read_directory`, and `read`;
mutating methods default to `ENOTSUP`. Names are byte slices rather than UTF-8
strings because FSKit filenames are not guaranteed to be valid UTF-8.

No Rust code is linked into the app or extension. Multiple CLIs can mount
different implementations simultaneously through the same installed bridge.

### Network filesystems

Network backends implement the parent trait and then opt into network behavior:

```rust
use fsbind::{
    Filesystem, LocalFile, Mount, NetworkFile, NetworkFilesystem, Result,
};

struct MyCoolFilesystem {
    // remote client, metadata cache, download manager, ...
}

impl Filesystem for MyCoolFilesystem {
    // statfs, metadata, lookup, read_directory and read are resolved lazily.
    // Implement the required methods here.
    # fn statfs(&self) -> Result<fsbind::StatFs> { unimplemented!() }
    # fn metadata(&self, _: u64) -> Result<fsbind::Metadata> { unimplemented!() }
    # fn lookup(&self, _: u64, _: &[u8]) -> Result<(u64, fsbind::FileType)> { unimplemented!() }
    # fn read_directory(&self, _: u64, _: u64, _: u64, _: &mut dyn fsbind::DirectorySink) -> Result<fsbind::ReadDirectoryResult> { unimplemented!() }
    # fn read(&self, _: u64, _: u64, _: &mut [u8]) -> Result<usize> { unimplemented!() }
}

impl NetworkFilesystem for MyCoolFilesystem {
    fn open_file(&self, inode: u64, _writable: bool) -> Result<NetworkFile> {
        if let Some(download) = self.downloaded_file(inode) {
            // The path remains owned by this backend.
            return Ok(NetworkFile::Local(LocalFile::new(
                download.path,
                download.generation,
            )));
        }
        Ok(NetworkFile::Remote)
    }

    fn close_file(&self, inode: u64, file: NetworkFile) -> Result<()> {
        // Swift has closed its descriptor. Update last-access state or schedule
        // removal according to your own cache policy.
        self.download_manager.file_closed(inode, file);
        Ok(())
    }
}

let mount = fsbind::MountSession::mount_network(
    MyCoolFilesystem::connect()?,
    "/Volumes/MyNetworkFilesystem",
)?;
# Ok::<(), std::io::Error>(())
```

`open_file` does not implicitly download anything. A backend returns
`NetworkFile::Local` only when its own download manager has explicitly
materialized that inode; otherwise `Filesystem::read` remains the lazy remote
path. The local file is leased until `close_file`. Swift closes its descriptor
before that callback, so the backend remains responsible for last-access
tracking and eviction.

See [the complete network example](examples/network-backend/src/lib.rs).

### Permission checks

The CLI can ask the Swift app for FSKit's authoritative enabled state and open
the authorization guide when needed:

```rust
match fsbind::authorization_status()? {
    fsbind::AuthorizationStatus::Authorized => {}
    fsbind::AuthorizationStatus::NeedsApproval => {
        fsbind::open_authorization_app()?;
    }
    fsbind::AuthorizationStatus::NotInstalled => {
        eprintln!("Install FSBind.app in /Applications");
    }
}
# Ok::<(), std::io::Error>(())
```

Set `FSBIND_APP_PATH` to an app bundle outside `/Applications` during
development. The app uses `FSClient` rather than guessing from process or
filesystem state, and embeds macOS's own extension authorization control.

## Architecture

```mermaid
flowchart LR
    K["macOS VFS / FSKit"] --> S["Swift FSVolume adapter"]
    S --> P["Pooled authenticated Unix sockets"]
    P --> R["Rust CLI / Filesystem trait object"]
    R --> M["Memory, database, remote, or custom backend"]
```

Each mount gets a cryptographically random capability and a mode-0600 socket in
the FSBind application-group container. The capability is carried in the
private FSKit resource URL and authenticated once per pooled connection. Rust
panics are caught at the dispatch boundary and become `EIO`.

## Performance design

- File reads write directly into FSKit's `FSMutableFileDataBuffer`.
- File writes borrow Foundation `Data`; neither hot path creates an
  intermediate Swift byte array.
- One `readdir` response fills an array of fixed-size entry records and a packed
  byte arena for names. There is no IPC round-trip or allocation per entry.
- The extension maintains up to one persistent connection per active CPU
  (bounded at 32), so unrelated FSKit callbacks execute concurrently without a
  connect, authentication, or global-request lock on the hot path.
- Frames use fixed little-endian headers and bounded binary payloads; there is
  no JSON, Codable, string conversion, or dynamic symbol lookup.
- Explicitly downloaded network files still use `pread` directly in the
  extension and do not send file contents through IPC.
- The memory backend uses a sharded inode map and per-node reader/writer locks,
  so unrelated reads and writes proceed concurrently.
- Namespace changes take one short mutex, making cross-directory rename atomic
  without serializing ordinary lookup or I/O.
- Directory lookup is hashed. Enumeration cookies are stable slot indices, so
  a resumed enumeration does not scan from the beginning.
- Each directory has a mutation verifier. A changed directory fails an old
  enumeration with `ESTALE` instead of returning an inconsistent snapshot.
- FSKit item objects are cached behind an unfair lock and reclaimed when FSKit
  releases them.
- Release builds use dead stripping and the Rust release profile.

For a quick machine-local baseline:

```sh
cargo run --release -p fsbind --example throughput
```

This is a microbenchmark of the Rust memory backend, not a mounted-filesystem
guarantee. Run Instruments or `fs_usage` against the signed extension for
end-to-end measurements of a real backend.

## Scope

The adapter supports attributes, lookup, batched enumeration, create, remove,
rename, symbolic links, sparse reads/writes, synchronization, and volume
statistics. Hard links and extended attributes deliberately return `ENOTSUP`;
they can be added to the ABI without changing existing records by incrementing
`FSB_ABI_VERSION`.

FSKit currently requires a host app and explicit user approval for the
extension. Unit tests validate the Rust dispatcher, authentication and Swift
codec without attempting to automate System Settings or a privileged mount.

## Signed GitHub releases

Pushing a tag matching `v*` runs
[the release workflow](.github/workflows/release.yml). It builds a universal
Developer ID application, notarizes it, staples the notarization ticket, and
publishes the resulting ZIP to GitHub Releases.

Configure these GitHub Actions secrets:

- `BUILD_CERTIFICATE_BASE64`: base64-encoded Developer ID Application `.p12`
  containing its private key.
- `P12_PASSWORD`: password used when exporting that `.p12`.
- `KEYCHAIN_PASSWORD`: a random password used only for the temporary CI
  keychain.
- `APP_PROVISIONING_PROFILE_BASE64`: base64-encoded Developer ID provisioning
  profile for `com.specta.fsbind`.
- `EXTENSION_PROVISIONING_PROFILE_BASE64`: base64-encoded Developer ID
  provisioning profile for `com.specta.fsbind.extension`, including the FSKit
  module entitlement.
- `APPLE_TEAM_ID`: Apple Developer team identifier.
- `APPLE_ID`: Apple account used by `notarytool`.
- `APPLE_APP_SPECIFIC_PASSWORD`: app-specific password for that Apple account.

The workflow uses GitHub's automatic `GITHUB_TOKEN` to create the release; no
additional GitHub token is required.

Both App IDs and provisioning profiles must include the
`group.com.specta.fsbind` App Group. It is the only directory shared by the
extension and unsandboxed Rust CLIs, and holds short-lived mode-0600 sockets
only.

## Why there is an app

FSKit discovers this code as an ExtensionKit app extension. The `.appex` must
be code-signed, embedded in a containing `.app`, installed, and approved by the
user. A Mach-O command-line executable can't directly contain and register that
extension.

A CLI can still be the primary interface: ship the signed `FSBind.app`
alongside it, make the app UI-less if desired, and have the CLI open or install
that companion. The distributable boundary remains an app bundle because its
nested extension, provisioning profile, entitlements, and signatures must
retain their bundle structure.
