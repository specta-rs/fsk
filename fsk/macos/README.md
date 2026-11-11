# macOS bridge

`FSK.app` is the install-once companion application that hosts the FSKit
extension. It is a setup utility, not a background service: it opens the File
System Extensions pane, reports whether the extension is enabled, and waits for
the user to enable it.

## Setup flow

1. An embedding application installs `FSK.app` with `fsk::install()`.
   `fsk::authorization_status()` reports `NotInstalled` when the matching app
   is missing, so `install()` should run first. The app goes into a hidden
   per-user directory, `~/Library/Application Support/dev.specta.fsk/FSK.app`,
   and a legacy `/Applications/FSK.app` from an older SDK is removed.
2. The embedding application calls `fsk::launch_app()` when
   `fsk::authorization_status()` reports `NeedsApproval`. A freshly installed
   app is not registered with `FSKit` until it has been launched once.
3. `FSK.app` shows a single permission row. **Open Settings** opens
   **System Settings → General → Login Items & Extensions → File System
   Extensions** directly with the File System Extensions pane identifier. The
   macOS 26 SDK does not expose `FSClient.openFileSystemExtensionsSettings()`,
   so the same deep link is used on macOS 26 and 27.
4. The row updates as the user flips the FSK switch. When the extension is
   enabled, **Done** quits the app.

macOS does not expose an API that enables an FSKit extension. The switch is a
per-user action in System Settings; the app can only make that action obvious
and verify the result.

## Why a hidden directory?

FSKit discovers modules through LaunchServices and PlugInKit when the host app
is launched, not because of where the bundle lives. Xcode runs and tests FSKit
extensions from DerivedData, so the install location does not affect discovery
as long as the app is launched at least once. `launch_app()` does that as part
of the setup flow.

The default destination is
`~/Library/Application Support/dev.specta.fsk/FSK.app`. `~/Library` is hidden in
Finder, so the app never appears in Applications, Launchpad, or Spotlight, and
each user gets their own copy without needing administrator permission.
`FSK.app` also sets `LSUIElement`, so it does not appear in the Dock or App
Switcher; it only shows its setup window when launched.

If a future macOS refuses to register an extension outside Applications, set
`FSK_APP_PATH=/Applications/FSK.app` before calling the lifecycle helpers.

## Build and test

```sh
(cd fsk/macos && xcodegen generate)
./fsk/macos/check.sh
```
