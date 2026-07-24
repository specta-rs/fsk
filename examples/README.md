# UFSI examples

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
cargo run -p ufsi-memory-backend -- /path/to/mount
```

## `network-backend`

A minimal immutable network filesystem and mount CLI. It demonstrates:

- implementing `NetworkFilesystem: Filesystem`;
- serving remote content lazily;
- returning an explicitly downloaded `LocalFile`;
- receiving close callbacks for activity tracking and eviction;
- keeping network policy independent from the in-memory example.

```sh
cargo run -p ufsi-network-backend -- /path/to/mount \
  "Content returned by origin.txt"
```

On macOS, install and authorize `UFSI.app` before running either CLI. Linux
requires FUSE, and Windows requires the Projected File System optional feature.
