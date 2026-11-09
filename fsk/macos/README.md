# macOS bridge

`FSK.app` is the install-once companion application that hosts the FSKit
extension. It is a setup utility, not a background service: it opens the File
System Extensions pane, reports whether the extension is enabled, and waits for
the user to choose **Done** or **Quit**.

## Setup flow

1. An embedding application installs `FSK.app` (or the user moves it to
   `/Applications`).
2. The embedding application calls `fsk::launch_app()` when
   `fsk::authorization_status()` is not `Authorized`.
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

## Why `/Applications`?

FSKit discovers modules through LaunchServices and PlugInKit. `/Applications`
is the reliable, supported location. Apps elsewhere (including
`~/Applications`) can work after they have been launched once, but discovery is
less predictable and System Settings may not list the extension until then.

`FSK.app` sets `LSUIElement`, so it does not appear in the Dock or App Switcher.
It is still a normal application bundle in `/Applications`; it only shows its
setup window when launched. Set `FSK_APP_PATH` while testing an app outside
`/Applications`.

## Build and test

```sh
(cd fsk/macos && xcodegen generate)
./fsk/macos/check.sh
```
