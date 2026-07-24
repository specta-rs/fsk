# UFSI

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

The published `ufsi` crate deliberately implements neither `Filesystem` nor
`NetworkFilesystem`. It does not choose an inode index, cache, locking model,
transaction boundary, consistency policy, or storage engine for downstream
users. The workspace's memory and network backends are examples only; copy
their ideas or replace them entirely.

## Platform support

| Platform | Native layer | Model | Installation |
|---|---|---|---|
| macOS 26+ | FSKit | Full virtual filesystem; lazy network support | Install and authorize `UFSI.app` once |
| Linux | FUSE kernel protocol through `fuser` | Full virtual filesystem; lazy network support | FUSE must be available |
| Windows 10 1809+ | ProjFS | Lazy projected tree with Windows-managed local cache | Enable the Projected File System optional component |

ProjFS intentionally merges projected entries with local files. Writes and
hydrated content live in its virtualization root, while providers receive
native notifications through the raw hook. This is a particularly good fit for
source-control, cloud, content-addressed, and other network-backed trees. A
fully synthetic Windows filesystem with provider-owned write semantics would
use WinFsp; UFSI exposes the Windows-native callback context in a form that
does not prevent a WinFsp adapter from being added without changing the common
model.

## Implement a filesystem

```rust
use ufsi::{Filesystem, MountSession};

struct MyCoolFilesystem;

impl Filesystem for MyCoolFilesystem {
    fn statfs(&self) -> ufsi::Result<ufsi::StatFs> {
        todo!()
    }

    fn metadata(&self, inode: u64) -> ufsi::Result<ufsi::Metadata> {
        todo!()
    }

    fn lookup(
        &self,
        parent: u64,
        name: &[u8],
    ) -> ufsi::Result<(u64, ufsi::FileType)> {
        todo!()
    }

    fn read_directory(
        &self,
        inode: u64,
        cookie: u64,
        verifier: u64,
        sink: &mut dyn ufsi::DirectorySink,
    ) -> ufsi::Result<ufsi::ReadDirectoryResult> {
        todo!()
    }

    fn read(
        &self,
        inode: u64,
        offset: u64,
        output: &mut [u8],
    ) -> ufsi::Result<usize> {
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
cargo run -p ufsi-memory-backend -- /path/to/mount
```

Control-C performs an orderly unmount. The same executable builds against the
native adapter selected by its target OS.

See [`examples/README.md`](examples/README.md) for the complete example catalog.

## Network filesystems and downloaded files

```rust
use ufsi::{LocalFile, NetworkFile, NetworkFilesystem};

impl NetworkFilesystem for MyCoolFilesystem {
    fn open_file(
        &self,
        inode: u64,
        _writable: bool,
    ) -> ufsi::Result<NetworkFile> {
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
    ) -> ufsi::Result<()> {
        self.download_manager.file_closed(inode, file);
        Ok(())
    }
}

let mount = ufsi::MountSession::mount_network(
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
use ufsi::raw::{
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

macOS mounts use a mode-0600 Unix socket in the UFSI App Group container and
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
cargo bench -p ufsi --bench throughput
cargo bench -p ufsi --bench raw_overhead
cargo test --release -p ufsi \
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
(cd ufsi/macos && xcodegen generate)
./ufsi/macos/check.sh
```

Install `UFSI.app`, then enable it under **System Settings → General → Login
Items & Extensions → File System Extensions**. A CLI can query and open the
authorization guide:

```rust
match ufsi::authorization_status()? {
    ufsi::AuthorizationStatus::Authorized => {}
    ufsi::AuthorizationStatus::NeedsApproval => {
        ufsi::open_authorization_app()?;
    }
    ufsi::AuthorizationStatus::NotInstalled => {
        eprintln!("Install UFSI.app in /Applications");
    }
}
# Ok::<(), std::io::Error>(())
```

Set `UFSI_APP_PATH` while testing an app outside `/Applications`.

### Windows

Enable **Projected File System** in “Turn Windows features on or off”, or with
an administrator shell:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS
```

ProjFS requires an empty or previously initialized virtualization root.

## Releases

Tags matching `v*` build, sign, notarize, staple, and upload `UFSI.app`, then
publish the matching `ufsi` crate through crates.io Trusted Publishing.

Configure a crates.io trusted publisher for:

- GitHub owner: `specta-rs`
- repository: `ufsi`
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
`group.dev.specta.ufsi` App Group.

FSKit still requires its `.appex` inside a signed `.app`; a CLI cannot contain
the extension as a raw executable. The companion app is installed once and can
serve any number of independent Rust mount processes.
