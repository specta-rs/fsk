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
- mounting until Ctrl-C and then unmounting cleanly.

```sh
cargo run -p fsk-memory-backend -- /path/to/mount
```

## `network-backend`

A minimal immutable network filesystem and mount CLI. It demonstrates:

- implementing `NetworkFilesystem: Filesystem`;
- serving remote content lazily;
- returning an explicitly downloaded `LocalFile`;
- receiving close callbacks for activity tracking and eviction;
- keeping network policy independent from the in-memory example.

```sh
cargo run -p fsk-network-backend -- /path/to/mount \
  "Content returned by origin.txt"
```

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

On macOS, install and authorize `FSK.app` before running a CLI. Linux
requires FUSE, and Windows requires the Projected File System optional feature.

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
