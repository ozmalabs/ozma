# Ozma desktop shell (Windows)

This standalone Windows process owns an Ozma notification-area icon. Right-click
the icon for **Open setup**, **Open Ozma**, **Help and setup guide**, **Review
access on this computer**, or **Quit tray shell**. Setup and the dashboard open
in the default browser and reuse the existing controller UI. Help opens the
authenticated in-app guide. The access action opens the local node URL.

Build on a Windows machine with the Rust MSVC toolchain:

```powershell
.\ozma-desktop\build-windows.ps1
```

The script stages `ozma-desktop.exe` and `ozma.ico` together in `ozma-desktop\dist`.
The shell reads `OZMA_CONTROLLER_URL` (default `http://127.0.0.1:7380`) and opens
`/ui/setup` or `/ui/` on that controller. Set the variable before launch when
the controller is at a different address. Help opens `/ui/help` on the same
controller. Set `OZMA_HELP_URL` to open a different help page. The local access
action reads `OZMA_LOCAL_NODE_URL` (default `http://127.0.0.1:7382`) and opens
`/api/v1/local/consent`; override it when the local node uses another address
or port.

Windows release bundles also include the user journey in `SETUP.md` and the
full license text in `LICENSE.txt` alongside the short `COPYING` notice.

For an MSVC-targeted cross-build from Linux, install `cargo-xwin` and run:

```sh
cargo install cargo-xwin --locked
cargo xwin build --release -p ozma-desktop --target x86_64-pc-windows-msvc
```

This checks the Windows-target link and produces
`target/x86_64-pc-windows-msvc/release/ozma-desktop.exe`. It does not replace
running the shell on Windows to check the notification-area icon and menu.

For a repeatable Windows-API smoke under Wine on Linux, install `cargo-xwin`,
MinGW-w64, Wine, and Xvfb, and initialize a Wine prefix. Then run:

```sh
./ozma-desktop/smoke-wine.sh
```

Set `WINEPREFIX` to choose a different initialized prefix. The script does not
initialize Wine because first-run setup can require interactive host settings.

The smoke starts the Windows executable, injects its right-click callback,
checks that a native popup window appears, posts **Quit tray shell**, and
asserts that the process exits successfully. It does not verify Explorer's
notification-area registration or replace the native Windows walkthrough.

## Current boundary

The shell does not start or supervise the controller or node. It does not yet
show session state, switch the active target, pause media/input, launch the
configuration UI as an embedded window, or configure startup-on-login. Choose
computers from the authenticated setup page. The **Help and setup guide** item
opens the in-app help page protected by the controller's normal sign-in flow.
The shell refreshes the controller's public `/health` route and this
computer's public `/api/v1/health` route in a background worker, then shows the
cached result in the icon tooltip and right-click menu. The menu does not wait
for DNS or HTTP.
It reports only whether each health endpoint returned HTTP 200; this does not
prove a remote node is reachable or that a session has media/input flowing.
HTTPS endpoints show status as unavailable; the shell does not downgrade HTTPS
or handle credentials. The tray icon is restored after Explorer restarts.
**Quit tray shell** removes the notification icon and exits only this shell
process. Left-click opens the controller UI.

**Review access on this computer** opens the node's loopback-only consent page
at the configured local URL. The page shows pending requests and handles
approve/deny locally. The tray does not call node approval routes or store/send
node API tokens. The node API must be running on the target computer.

`ozma.ico` is copied from the existing controller favicon. If it is missing
beside the executable, the shell falls back to the standard Windows
application icon.
