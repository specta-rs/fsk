# GitHub filesystem example

This example recreates GHFS's filesystem model on fsk across macOS, Linux, and
Windows: the mount is read-only, paths are `/<owner>/<repository>/`, only the
default branch is shown, repository discovery is paginated, and namespace data
is fetched lazily from GitHub's REST API.

Unlike GHFS's whole-repository clone, this example materializes individual files.
That makes the policy explicit and keeps it portable without requiring `git` or
libgit2. The first read is returned from the API and atomically cached; later
opens use fsk's `LocalFile` path and bypass userspace copies. `--download-on-list`
also materializes the files in each directory as it is browsed.

## Run

Create a fine-grained token with read access to the repositories you want to
mount, then install the platform prerequisite described in the repository root.

```sh
GITHUB_TOKEN=github_pat_... \
  cargo run -p fsk-github-filesystem -- /path/to/mount
```

Options:

```text
--cache-dir PATH          cache location (platform cache directory by default)
--max-cache-bytes BYTES   LRU size limit (default: 1073741824)
--max-idle-secs SECONDS   remove files untouched this long (default: 604800)
--download-on-list        download files when their directory is listed
--api-url URL             GitHub API base URL (default: https://api.github.com)
```

`GITHUB_TOKEN` is required. `GHFS_CACHE_DIR`, `GHFS_MAX_CACHE_BYTES`, and
`GHFS_MAX_IDLE_SECS` provide environment equivalents. Cache cleanup runs at
startup and after local-file leases close. Files currently open are never
removed. Offline reads continue to work for already materialized files; uncached
namespace and file requests return an I/O error.

The cache key includes repository, path, and blob SHA. A changed default-branch
blob therefore never aliases stale bytes. Downloads use a temporary file plus an
atomic rename so interrupted requests cannot expose partial contents.

## Tests

```sh
cargo test -p fsk-github-filesystem
```

The end-to-end test starts a local fake GitHub REST server and exercises discovery,
directory traversal, an API read, a cached local reopen, read-only behavior, and
age-based eviction without external credentials.

### Live mount test

`live_mount_reads_repository` mounts the real repository through FUSE on Linux
and reads it through the mounted path. It is ignored by default because it needs
network access and a working FUSE installation:

```sh
GITHUB_TOKEN=... GITHUB_REPOSITORY=specta-rs/fsk \
  cargo test -p fsk-github-filesystem -- --ignored live_mount
```

Any user token with repository read access works. CI uses the workflow's
built-in installation token: the API client falls back to
`/installation/repositories` when `/user/repos` is unavailable. The
`.github/workflows/live-mounts.yml` workflow runs this test on every change and
weekly.
