# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Added

- `fsk::wait_for_authorization(timeout)`: installs and launches the platform
  component when needed, then blocks until the user has granted access. Examples
  and applications no longer need to re-run a command after enabling the
  extension.
- `fsk::install_with_progress(callback)`: reports `Downloading`, `Verifying` and
  `Installing` phases for the macOS companion app.
- `fsk::current_uid()` and `fsk::current_gid()` for metadata ownership.
- `Metadata::file(..)`, `Metadata::directory(..)` and `Metadata::owned_by(..)`
  constructors, plus `StatFs::with_capacity(..)` and `StatFs::default()`.
- Named `Error` constants for `ENOENT`, `EEXIST`, `ENOTDIR`, `EISDIR`, `EFBIG`,
  `ENOSPC` and `ERANGE`.

### Changed

- The macOS `PermissionDenied` message from `ensure_authorization()` now names
  **System Settings → General → Login Items & Extensions → File System
  Extensions** instead of referring to "the window that opened".
- The macOS mount error explains that FSKit cannot mount inside Desktop,
  Documents or Downloads, instead of leaving a bare `Operation not permitted`.
- Migrating a legacy `/Applications/FSK.app` now quits the running legacy app
  before removing its bundle, and the docs state that the new location needs
  approval again.

### Documentation

- Getting started uses the new metadata constructors and owner helpers, waits
  for authorization before mounting, links the testing guide, and warns about
  privacy-protected mount directories.
- Troubleshooting explains that `pluginkit` cannot enable the extension and
  documents the EPERM mount failure.
- `examples/memory-backend` and `examples/google-drive` use the shared owner
  helpers instead of declaring their own libc bindings or errno constants.

## 0.1.0 - 2026-11-12

### Added

- Windows ProjFS hydration eviction API (`MountSession::evict`,
  `MountSession::eviction_handle`).
- Programmatic macOS app lifecycle: `install`, `uninstall`, `launch_app`,
  `authorization_status` and the hidden per-user app install.
- Versioned Rust <-> Swift IPC protocol with a compatibility check, so an
  installed app is reused when its protocol major matches.
- GitHub filesystem and Google Drive examples, including writable Drive mounts.
- User guide under `docs/`, published at <https://specta.dev/docs/fsk>.

### Changed

- The macOS app and extension are installed per user in
  `~/Library/Application Support/dev.specta.fsk/` instead of `/Applications`.
- Removed the dead C ABI and the deprecated authorization aliases.
- macOS mounts canonicalise their path before calling `mount(8)`.
- The macOS release pipeline now notarises, staples and Gatekeeper-checks the
  app, and verifies the IPC protocol metadata in the exported bundle.
- The README and documentation were rewritten around the published crate.

### Fixed

- Linux rename flag validation and Google Drive alias handling.
- Windows setup guidance now includes the `Enable-WindowsOptionalFeature`
  command.
- macOS app installation and uninstall hardening, permission setup flow and
  extension icon.
