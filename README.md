# FSK

Build cross-platform virtual file systems with ease

- macOS: FSKit extension with an install-once Swift bridge.
- Linux: multithreaded kernel FUSE adapter.
- Windows: native ProjFS provider for lazy virtual and network trees.
- Rust: `Filesystem` and `NetworkFilesystem: Filesystem` traits.
- Native access: opt-in borrowed FSKit, FUSE, and Windows event contexts.

The common API stays deliberately small. Backends identify items with stable
64-bit inode values, stream directory entries into a caller-owned sink, and
read directly into caller-provided buffers. Names are arbitrary bytes on Unix
and WTF-8 on Windows, preserving Windows UTF-16 names including unpaired
surrogates.

The published `fsk` crate deliberately implements neither `Filesystem` nor
`NetworkFilesystem`. It does not choose an inode index, cache, locking model,
transaction boundary, consistency policy, or storage engine for downstream
users. The workspace's memory and network backends are examples only; copy
their ideas or replace them entirely. The example catalog also includes a
Google Drive backend that mounts `My Drive`, items `Shared with me`, and shared
drives using an OAuth token while honoring each file's write capability.

## Platform support

| Platform | Native layer | Model | Installation |
|---|---|---|---|
| macOS 26+ | FSKit | Full virtual filesystem; lazy network support | Install and authorize `FSK.app` once |
| Linux | FUSE kernel protocol through `fuser` | Full virtual filesystem; lazy network support | FUSE must be available |
| Windows 10 1809+ | ProjFS | Lazy projected tree with Windows-managed local cache | Enable the Projected File System optional component |

ProjFS intentionally merges projected entries with local files. Writes and
hydrated content live in its virtualization root, while providers receive
native notifications through the raw hook. This is a particularly good fit for
source-control, cloud, content-addressed, and other network-backed trees. A
fully synthetic Windows filesystem with provider-owned write semantics would
use WinFsp; FSK exposes the Windows-native callback context in a form that
does not prevent a WinFsp adapter from being added without changing the common
model.

## Implement a filesystem

```rust
use fsk::{Filesystem, MountSession};

struct MyCoolFilesystem;

impl Filesystem for MyCoolFilesystem {
    fn statfs(&self) -> fsk::Result<fsk::StatFs> {
        todo!()
    }

    fn metadata(&self, inode: u64) -> fsk::Result<fsk::Metadata> {
        todo!()
    }

    fn lookup(
        &self,
        parent: u64,
        name: &[u8],
    ) -> fsk::Result<(u64, fsk::FileType)> {
        todo!()
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        verifier: u64,
        sink: &mut dyn fsk::DirectorySink,
    ) -> fsk::Result<fsk::ReadDirectoryResult> {
        todo!()
    }

    fn read(
        &self,
        inode: u64,
        offset: u64,
        output: &mut [u8],
    ) -> fsk::Result<usize> {
        todo!()
    }
}

let mount = MountSession::mount(MyCoolFilesystem, "/path/to/mount")?;
// Keep `mount` alive. Dropping it unregisters/unmounts the filesystem.
# Ok::<(), std::io::Error>(())
```

The required methods are sufficient for a read-only filesystem. Mutation
methods default to `ENOTSUP`. Methods can run concurrently, so implementations
must be `Send + Sync`.

The workspace's memory example is a complete CLI and a deliberately separate
implementation:

```sh
cargo run -p fsk-memory-backend -- /path/to/mount
```

Control-C performs an orderly unmount. The same executable builds against the
native adapter selected by its target OS.

See [`examples/README.md`](examples/README.md) for the complete example catalog.

## Network filesystems and downloaded files

```rust
use fsk::{LocalFile, NetworkFile, NetworkFilesystem};

impl NetworkFilesystem for MyCoolFilesystem {
    fn open_file(
        &self,
        inode: u64,
        _writable: bool,
    ) -> fsk::Result<NetworkFile> {
        if let Some(download) = self.downloaded_file(inode) {
            return Ok(NetworkFile::Local(LocalFile::new(
                download.path,
                download.generation,
            )));
        }
        Ok(NetworkFile::Remote)
    }

    fn close_file(
        &self,
        inode: u64,
        file: NetworkFile,
    ) -> fsk::Result<()> {
        self.download_manager.file_closed(inode, file);
        Ok(())
    }
}

let mount = fsk::MountSession::mount_network(
    MyCoolFilesystem::connect()?,
    "/path/to/mount",
)?;
# Ok::<(), std::io::Error>(())
```

`NetworkFile::Remote` keeps reads lazy through `Filesystem::read`.
`NetworkFile::Local` leases an explicitly downloaded file until `close_file`,
letting the backend track activity and perform its own eviction.

- macOS reads leased local files with `pread`, bypassing IPC.
- Linux reads the local descriptor directly on its FUSE worker.
- Windows ProjFS manages its virtualization root as the local hydration cache.

## Native requests without default overhead

Implementers can inspect platform-specific events without forcing every
filesystem through an allocation-heavy universal request object:

```rust
use fsk::raw::{
    NativeRequest, Operation, RawEvent, RawEventMask, RawOutcome,
};

impl Filesystem for MyCoolFilesystem {
    // common methods...

    fn raw_event_mask(&self) -> RawEventMask {
        RawEventMask::operation(Operation::Open)
            .union(RawEventMask::operation(Operation::Write))
    }

    fn raw_event(&self, event: RawEvent<'_>) -> RawOutcome {
        match event.native {
            NativeRequest::Linux {
                request,
                unique,
                file_handle,
                flags,
            } => {
                // `request` points to the borrowed fuser::Request.
            }
            NativeRequest::Windows {
                callback_data,
                namespace_context,
                command_id,
                flags,
            } => {
                // Live PRJ_CALLBACK_DATA and virtualization context.
            }
            NativeRequest::MacOS {
                request_id,
                item_id,
                flags,
                payload,
            } => {
                // Exact borrowed bridge payload and stable FSKit identifiers.
            }
        }
        RawOutcome::Continue
    }
}
```

The mask is fetched once when mounting. When it is empty, adapters take one
predictable bit test and do not construct an event or call the hook. Buffers
are borrowed in place:

- Linux exposes the live `fuser::Request` and read/write buffer.
- Windows exposes `PRJ_CALLBACK_DATA`, namespace context, command ID, and
  ProjFS-aligned hydration buffer.
- macOS exposes exact bridge payloads, request/item identifiers, and flags.
  FSKit Objective-C pointers cannot be validly exposed to a separate Rust
  process, so the bridge never presents unsafe cross-process pointer values.

Raw pointers and buffers are valid only for the synchronous hook call.

## Architecture

```mermaid
flowchart LR
    C["Filesystem / NetworkFilesystem"] --> A["Platform adapter"]
    A --> L["Linux FUSE"]
    A --> W["Windows ProjFS"]
    A --> M["macOS FSKit IPC"]
    M --> S["Install-once Swift extension"]
```

macOS mounts use a mode-0600 Unix socket in the FSK App Group container and
a random per-mount capability. The extension pools up to one persistent
connection per active CPU. Linux uses multiple FUSE event loops and
`FUSE_DEV_IOC_CLONE` where available. Windows lets ProjFS select or configure
native callback workers.

## Performance

The hot paths avoid serialization frameworks and per-item calls:

- caller-owned read buffers and borrowed write buffers;
- packed directory batches on macOS;
- reusable thread-local FUSE read buffers on Linux;
- ProjFS-aligned buffers written directly with `PrjWriteFileData` on Windows;
- fixed little-endian IPC frames rather than JSON or Codable;
- `writev` for macOS response headers and payloads;
- stable directory cookies and mutation verifiers;
- no raw-event allocation or virtual call unless its operation is enabled.

These figures use a benchmark-only immutable `Filesystem` with no inode index,
cache, locking, consistency policy, or storage engine. They measure trait
dispatch, caller-owned buffers, directory sinks, and raw-hook plumbing rather
than either example implementation. Benchmarks were run on an Apple M1 Pro
using release builds:

| Benchmark | Observed result |
|---|---:|
| Trait-object lookup | 435 million operations/s median |
| 128 KiB caller-buffer reads | 54.8 GiB/s median |
| 10,001-entry directory sink | 466 million entries/s median |
| Raw hooks disabled | Within run-to-run noise of direct dispatch |
| Authenticated macOS IPC lookup | 188 thousand round trips/s median |

Changing macOS response writes from two syscalls to vectored I/O improved the
authenticated lookup result from a 162–169k baseline to 182–196k round trips/s
(roughly 12–15% by median, depending on the run). A round trip is about
5.1–5.5 µs on this machine.

Run the same benchmarks locally:

```sh
cargo bench -p fsk --bench throughput
cargo bench -p fsk --bench raw_overhead
cargo test --release -p fsk \
  ipc::tests::benchmark_authenticated_ipc_lookup_roundtrip \
  -- --ignored --nocapture
```

These isolate backend and dispatch costs. Mounted performance also includes the
host kernel, FUSE/FSKit/ProjFS, cache state, and the downstream backend; use
`fio`, Instruments, `fs_usage`, or platform equivalents for deployment-specific
measurements.

## Building and testing

Rust requires version 1.85 or later.

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings

cargo check --workspace --all-targets \
  --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets \
  --target x86_64-pc-windows-msvc
```

The CI matrix runs native Rust tests on macOS, Ubuntu, and Windows. The macOS
job also generates the Xcode project and runs the Swift bridge tests.

### macOS bridge

macOS additionally requires Xcode 26, XcodeGen, and an Apple team capable of
signing `com.apple.developer.fskit.fsmodule`.

```sh
(cd fsk/macos && xcodegen generate)
./fsk/macos/check.sh
```

The Rust SDK can download and install the matching `FSK.app` release without
opening it. Run the blocking installation call on your background executor,
then launch the app separately when the user is ready to authorize it under
**System Settings → General → Login Items & Extensions → File System Extensions**.
Copying the app into place does not register its extension, so
`authorization_status()` may still report `NotInstalled` until the app has been
launched once:

```rust
let outcome = fsk::download_and_install_app()?;
println!("FSK app installation: {outcome:?}");

match fsk::authorization_status()? {
    fsk::AuthorizationStatus::Authorized => {}
    fsk::AuthorizationStatus::NeedsApproval | fsk::AuthorizationStatus::NotInstalled => {
        fsk::launch_app()?;
    }
}
# Ok::<(), std::io::Error>(())
```

Later, when your application no longer needs the extension:

```rust
let removed = fsk::uninstall_app()?;
println!("FSK app removed: {removed}");
# Ok::<(), std::io::Error>(())
```

The default download uses the public GitHub release for this repository. If the
release is private, set `GITHUB_TOKEN` or `GH_TOKEN` and the SDK resolves the
asset through the GitHub API, or set `FSK_RELEASE_BASE_URL` to a mirror that
serves `{base}/v0.0.9/FSK-v0.0.9.zip`.

The app is installed machine-wide at `/Applications/FSK.app` and shared by every
process and SDK consumer on the machine. Installation requires an exact version
match, so a program built against an older SDK can replace (downgrade) an app
installed by a newer SDK. `uninstall_app()` likewise removes the app for all
consumers, not just the current program. Both operations are serialized across
cooperating processes with an advisory lock on the destination's parent
directory; no lock file is created.

Set `FSK_APP_PATH` to the full path of an `.app` bundle (for example
`/Applications/FSK.app`) while testing an app outside `/Applications`. All four
helpers — install, authorization status, launch, and uninstall — require a full
bundle path; it is not a directory that contains the app. Uninstall removes
directory bundles only when their `CFBundleIdentifier` is `dev.specta.fsk`
(symlinks at the destination are removed without following them, and any other
file or directory is refused), and it always refuses to delete a mount point.

### Windows

Enable **Projected File System** in “Turn Windows features on or off”, or with
an administrator shell:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS
```

ProjFS requires an empty or previously initialized virtualization root.

## Releases

Tags matching `v*` build, sign, notarize, staple, and upload `FSK.app`, then
publish the matching `fsk` crate through crates.io Trusted Publishing.

Configure a crates.io trusted publisher for:

- GitHub owner: `specta-rs`
- repository: `fsk`
- workflow: `release.yml`
- environment: none

The workflow uses GitHub OIDC and does not require a crates.io API-token
secret. The tag must exactly match the crate version, such as `v0.0.1`.

For macOS signing, configure:

- `BUILD_CERTIFICATE_BASE64`
- `P12_PASSWORD`
- `KEYCHAIN_PASSWORD`
- `APP_PROVISIONING_PROFILE_BASE64`
- `EXTENSION_PROVISIONING_PROFILE_BASE64`
- `APPLE_TEAM_ID`
- `APPLE_ID`
- `APPLE_APP_SPECIFIC_PASSWORD`

Both App IDs and profiles must include the
`group.dev.specta.fsk` App Group.

FSKit still requires its `.appex` inside a signed `.app`; a CLI cannot contain
the extension as a raw executable. The companion app is installed once and can
serve any number of independent Rust mount processes.
