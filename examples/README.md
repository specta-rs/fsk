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

On macOS, install and authorize `FSK.app` before running either CLI. Linux
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
