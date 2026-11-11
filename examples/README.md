# FSK examples

These are standalone, binary-only implementations of the `Filesystem` traits.
They are not part of the published `fsk` crate and deliberately make
different storage and consistency choices.

## `memory-backend`

A writable in-memory filesystem and mount CLI. It demonstrates:

- implementing the complete `Filesystem` trait;
- sharded inode lookup and per-node locking;
- atomic cross-directory namespace mutations;
- stable directory cookies and mutation verifiers;
- sparse reads and writes;
- logging namespace changes (`create`, `write`, `rename`, `remove`, …) to the
  console;
- mounting until Ctrl-C and then unmounting cleanly.

```sh
cargo run -p fsk-memory-backend -- /path/to/mount
```

The mount directory is created on startup and removed on exit, so it must not
already exist.

## `network-backend`

A minimal immutable network filesystem and mount CLI. It demonstrates:

- implementing `NetworkFilesystem: Filesystem`;
- serving remote content lazily;
- returning an explicitly downloaded `LocalFile`;
- using `fsk::local_file_directory()` so the sandboxed macOS extension can
  open the materialized file;
- receiving close callbacks for activity tracking and eviction;
- keeping network policy independent from the in-memory example.

```sh
cargo run -p fsk-network-backend -- /path/to/mount \
  "Content returned by origin.txt"
```

On macOS this is a mounted volume. FSKit does not expose iCloud-style
per-file download/evict controls; use a File Provider extension
(`NSFileProviderReplicatedExtension`) when the goal is a folder that looks like
iCloud Drive.

## `google-drive`

A Google Drive mount backed by the Drive API. It exposes `My Drive`,
`Shared with me`, and every shared drive available to the account. File bytes
are fetched lazily on first access. Google Workspace documents are exported to
portable formats while the mount is indexed because Drive does not publish
their exported byte lengths. Ordinary files are writable when Drive reports
`canModifyContent`; files without that capability and exported Google Workspace
documents are exposed read-only through the platform's normal permission bits.
On macOS and Linux, writes are uploaded to Drive. Windows ProjFS keeps edits in
its local projection cache and does not currently provide portable write-back
callbacks, but Drive read-only files still receive the Windows read-only
attribute.

Create an OAuth access token with the Drive scope, then run:

```sh
export GOOGLE_DRIVE_ACCESS_TOKEN="$(gcloud auth application-default print-access-token)"
cargo run -p fsk-google-drive -- /path/to/mount
```

The token must include `https://www.googleapis.com/auth/drive` for writes.
Application-default tokens from `gcloud` only work when the associated OAuth
grant includes that scope; a token from any other OAuth flow works too.
Access tokens are short-lived; restart the example with a refreshed token when
the current token expires.

For unattended use, set the OAuth client credentials and a refresh token
instead. The example exchanges them for an access token at startup:

```sh
GOOGLE_CLIENT_ID=... \
GOOGLE_CLIENT_SECRET=... \
GOOGLE_DRIVE_REFRESH_TOKEN=... \
cargo run -p fsk-google-drive -- /path/to/mount
```

`GOOGLE_DRIVE_ACCESS_TOKEN` takes precedence when both forms are set. CI uses
the refresh-token form so no expiring token has to be stored. To mint a refresh
token, create a **Web application** OAuth client with
`https://developers.google.com/oauthplayground` as an authorised redirect URI,
publish the app to production, then authorise with **Access type: Offline** in
the OAuth Playground. A read-only mount only needs
`https://www.googleapis.com/auth/drive.readonly`. The live mount test expects a
`fsk-ci/hello.txt` fixture (override with `FSK_DRIVE_TEST_PATH`) containing
`hello from fsk ci\n`, and runs as:

```sh
cargo test -p fsk-google-drive -- --ignored live_mount
```

`GOOGLE_DRIVE_ACCESS_TOKEN` can be used instead of the three refresh variables.
The test requires FUSE on Linux and is run by
`.github/workflows/live-mounts.yml`.

For simplicity, this example stages ordinary file contents in memory and limits
individual buffered files to 256 MiB. Uploads use Drive revisions and ETags to
reject saves when another client changed the remote file after it was indexed.

On macOS, install and authorize `FSK.app` before running a CLI. Linux
requires FUSE, and Windows requires the Projected File System optional feature.
The CLIs call `authorization_status()` and print the platform requirement,
including the exact `Enable-WindowsOptionalFeature` command on Windows, before
`ensure_authorization()` opens the platform setup UI when possible.

## `github-filesystem`

A complete, read-only GitHub filesystem inspired by
[GHFS](https://github.com/indragiek/GHFS). It demonstrates repository discovery,
lazy API-backed directory trees, atomic downloads, local-file acceleration, and
automatic least-recently-used cache cleanup.

```sh
GITHUB_TOKEN=github_pat_... cargo run -p fsk-github-filesystem -- /path/to/mount
```

See [`github-filesystem/README.md`](github-filesystem/README.md) for cache and
download controls.
