# Ozma Windows GUI configuration preview

This guide covers the current Windows tray and setup flow. The target can
review and approve a pending access request locally, and the controller can
start the approved session. The project owner reports that the existing
Windows build can control itself. This newly added tray/setup path still needs
a native Windows walkthrough; an API Ready state alone does not confirm that
its session carries picture and input.

## Requirements

- A Windows machine with the Rust MSVC toolchain to build the tray shell.
- A reachable Ozma controller with the React UI installed at `/ui/`.
- At least one registered Ozma node to configure as a target.
- The target's `ozma-node` API and `ozma-desktop` tray shell running on that
  same computer for local consent review.
- For controller-to-node session requests, configure matching bearer tokens:
  `OZMA_API_TOKEN` on the node and the registered per-node token or controller
  `OZMA_NODE_API_TOKEN`. Without a token, browser session API requests are
  limited to same-origin loopback access, and controller-proxied requests fail
  closed.

Build the tray shell on Windows:

```powershell
.\ozma-desktop\build-windows.ps1
```

The script stages `ozma-desktop.exe` and `ozma.ico` in
`ozma-desktop\dist`. By default, the shell opens the controller at
`http://127.0.0.1:7380`. Set `OZMA_CONTROLLER_URL` before launching it if the
controller uses another address.
The consent action opens the target-local node at
`http://127.0.0.1:7382/api/v1/local/consent`. Set `OZMA_LOCAL_NODE_URL` before
launching the tray if the node uses another local address or port.

The controller dashboard also has a **Start setup** entry when there are no
saved scenarios, and **Open setup** afterward. It opens the same intent-led
flow as the tray menu.

Build the controller's bundled UI from `controller\ui` before packaging or
running the controller:

```powershell
npm ci
npm run build
```

The UI bundle is written to `controller\static\ui` and is served by the
controller at `/ui/`. The type-check covers the React app entry and its
imported routes; older orphaned UI modules outside that route graph are not
included in the alpha build.

## Configure a setup

1. Launch `ozma-desktop.exe`. Its notification-area icon stays available while
   browser windows open or close.
2. Right-click the icon and choose **Open setup**. Sign in to the controller
   if prompted. **Help and setup guide** opens the authenticated in-app guide
   at `/ui/help`.
3. Choose the outcome and the computer, then use **Check if Ozma can do this**
   to review availability. **Create a custom setup** starts with
   view-only access; choose keyboard/mouse in Guided or a more detailed view
   and the check follows that choice. The signed-in controller is the starting
   point by default. Expert includes an advanced option to check from another
   endpoint; that option changes route checking only, while session access
   requests still originate from the signed-in controller.
4. Choose **Simple**, **Guided**, **Fine-tune**, or **Expert** to show more or
   less detail. This changes what the page shows; it does not change the
   selected goal or grant permissions. Fine-tune and Expert can set maximum
   motion rates of 15, 24, or 30 frames per second and video data ceilings of
   1, 2, or 4 Mbps. These are session ceilings: the target enforces them and
   reports the effective values and any limits. **Restore recommended choices**
   resets supported session options while preserving the goal, endpoints, and
   detail level.
5. Save the setup to the controller if you want to reopen the same intent and
   supported choices later. The most recently saved or loaded profile is
   selected automatically next time this browser opens Setup. A saved profile
   does not request access or start a session.
6. **Request this access** first checks that the target exposes a native
   session connection. If it is unavailable, no consent request is created and
   Setup explains that the target must be made ready. Otherwise, the node
   evaluates the selected session permissions and video quality ceilings, then
   reports requested and effective values. If the target does not confirm the
   requested quality settings, Setup tries to end the new request and asks for an
   updated target node. If cancellation cannot be confirmed, it tells you to
   deny the request from the target tray. The request asks for an activity
   indicator, but this path does not confirm whether the target displays one.
7. On the target computer, right-click the tray icon and choose **Review
   access on this computer**. Review the requester and permissions, then
   approve or deny the request on the target.
8. Return to the setup page. When the request is approved, choose **Refresh
   approval status**, then **Start approved session**. A Ready state confirms
   the session lifecycle transition; it does not by itself confirm that
   picture, audio, or input has connected.
9. When finished, choose **End session** on the setup page. A new control
   session requires a new target approval.

To control this computer from a different Ozma endpoint, start the setup from
that source endpoint and select this computer as the target. The target user
reviews and approves the request from this computer's tray icon.

## Current limits

- The tray menu opens setup, the controller dashboard, authenticated in-app
  Help, target-local access review, and quits the tray process. A background
  worker refreshes controller and local-node health endpoints; the icon
  tooltip and menu report cached HTTP reachability only. They do not report
  active session/media state, switch targets, pause/resume a session, open logs,
  or supervise controller/node processes. The tray icon is restored if
  Explorer restarts.
- The setup page can save and load intent profiles and request a node policy
  evaluation, including the chosen motion-rate and video-data ceilings. Approval is served by
  the node's loopback-only consent page. The
  audit actor is the node process's OS account; this is not person-specific
  when people share that account.
- Resolution, quality-region, and foveation settings are unavailable because
  the selected runtime path does not consume them. The session applies maximum
  frame-rate and video-bitrate ceilings and reports the target's effective
  values. Foveation describes prioritizing image detail near a gaze or focus
  region over the periphery; it is not active in this alpha.
- The tray shell opens the React UI in the default browser, not an embedded
  window. **Quit Ozma** exits the tray shell only; it does not stop the
  controller or node.

Treat this as a configuration and policy-review alpha until the new setup-to-
session journey is run on supported Windows and picture/input are observed
after Ready.
