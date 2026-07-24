# FSBind

FSBind is a working macOS FSKit extension whose filesystem implementation lives
in Rust. It provides:

- a small Rust `Filesystem` trait for downstream backends;
- a `NetworkFilesystem: Filesystem` trait for lazy remote volumes and
  backend-owned downloaded files;
- a stable, versioned C ABI with opaque volume handles;
- a Swift adapter that maps FSKit operations onto that ABI;
- a concurrent in-memory backend that is useful as a starter implementation;
- a host app and ExtensionKit extension generated with XcodeGen;
- Rust and Swift cross-language tests.

The checked-in example mounts a writable memory filesystem containing
`hello.txt`.

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

## Mount the example

1. Open `FSBind.xcodeproj`, select a development team for both the app and
   extension, then build and run `FSBindApp`.
2. In **System Settings > General > Login Items & Extensions > File System
   Extensions**, enable **FSBind virtual file system**.
3. Create source and mount directories, then mount the filesystem:

```sh
mkdir -p /tmp/fsbind-source "$HOME/fsbind-mount"
mount -t fsbind /tmp/fsbind-source "$HOME/fsbind-mount"
ls -la "$HOME/fsbind-mount"
cat "$HOME/fsbind-mount/hello.txt"
```

The source path is only the resource FSKit uses to identify this mount; the
example backend does not read it. Unmount with:

```sh
diskutil unmount "$HOME/fsbind-mount"
```

## Create a downstream filesystem

The quickest route is to replace the factory in
`examples/memory-backend/src/lib.rs`. The factory returns one independent
backend for each loaded volume:

```rust
use fsbind::{FileType, Filesystem, MemoryFileSystem, ROOT_INODE};

fn filesystem() -> MemoryFileSystem {
    let fs = MemoryFileSystem::new();
    let inode = fs
        .create(ROOT_INODE, b"status", FileType::File, 0o444)
        .unwrap();
    fs.write(inode, 0, b"ready\n").unwrap();
    fs
}

fsbind::export_filesystem!(filesystem);
```

For a database, remote service, archive, or content-addressed store, implement
`fsbind::Filesystem` on your own `Send + Sync` type. Read-only backends only
need to implement `statfs`, `metadata`, `lookup`, `read_directory`, and `read`;
mutating methods default to `ENOTSUP`. Names are byte slices rather than UTF-8
strings because FSKit filenames are not guaranteed to be valid UTF-8.

Build the downstream crate as a `staticlib`, invoke
`fsbind::export_filesystem!(your_factory)`, and change the two library names in
`project.yml` from `fsbind_memory_backend` to the resulting static library.
The generated C symbols stay fixed, so no Swift source changes are needed.

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

fn filesystem(mount: &Mount) -> Result<MyCoolFilesystem> {
    let url = mount
        .resource_url()
        .map_err(|_| fsbind::Error::INVALID)?;
    MyCoolFilesystem::connect(url)
}

fsbind::export_network_filesystem!(filesystem);
```

`open_file` does not implicitly download anything. A backend returns
`NetworkFile::Local` only when its own download manager has explicitly
materialized that inode; otherwise `Filesystem::read` remains the lazy remote
path. The local file is leased until `close_file`. Swift closes its descriptor
before that callback, so the backend remains responsible for last-access
tracking and eviction.

See [the complete network example](examples/network-backend/src/lib.rs).

Run `xcodegen generate` after changing `project.yml`. The build phase maps
Xcode's `arm64` and `x86_64` architectures to their Rust target triples and
places the correct library in an architecture-specific directory.

## Architecture

```mermaid
flowchart LR
    K["macOS VFS / FSKit"] --> S["Swift FSVolume adapter"]
    S --> C["Versioned C ABI"]
    C --> R["Rust Filesystem trait object"]
    R --> M["Memory, database, remote, or custom backend"]
```

The ABI has one opaque handle per volume. Rust panics are caught at every
exported operation and become `EIO`, so unwinding never crosses into Swift.
All other failures use positive Darwin `errno` values.

## Performance design

- File reads write directly into FSKit's `FSMutableFileDataBuffer`.
- File writes borrow Foundation `Data`; neither hot path creates an
  intermediate Swift byte array.
- One `readdir` FFI call fills an array of fixed-size entry records and a packed
  byte arena for names. There is no FFI call or allocation per directory entry.
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
extension. Unit tests therefore validate the Rust core and the actual Swift/C
boundary without attempting to automate System Settings or a privileged mount.

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
