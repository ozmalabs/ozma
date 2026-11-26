# Ozma GUI configuration plan for alpha

Status: **active plan**
Goal: make Ozma understandable and configurable by describing what a person
wants to do, while retaining detailed controls for people who need them.

## Outcome

A new user can install and launch Ozma, choose a goal in ordinary language,
review and apply a setup, and then operate it from the notification area. The
tray stays available while the main configuration window is closed. Variable
aperture lets the user move from a simple outcome choice to progressively more
detailed and precise configuration without changing the underlying intent.
Detailed settings explain the effect and scope of each choice; permissions and
device/policy limits are part of that detail.

The initial implementation targets the existing Windows build and its
controller/self-control workflow. Keep platform-specific shell code behind a
small boundary so Linux tray support can be added without changing the intent
or policy model.

## Current building blocks

- `controller/ui` is the React application. It has dashboard, node, scenario,
  streaming, audio, controls, routing, and settings pages. Its Settings page is
  currently limited to Connect, account, and controller information.
- `controller/routing/intent.py` defines built-in intents. The controller has
  intent listing, feasibility, explanation, and evaluation APIs. The React
  setup page now uses intent listing and feasibility for its preview.
- `ozma-aperture` implements typed values, frames, evaluation, provenance,
  recourse, and receipts. The native node API evaluates session permissions
  and maximum frame-rate and video-bitrate requests against encoder and node
  limits. Resolution, quality regions, and foveation are not yet connected to
  the session media path.
- `agent/tray.py` has a legacy Python tray, but `ozma-agent` is deprecated and
  its Rust implementation is a daemon skeleton. Do not make the new alpha
  depend on that legacy tray.
- `docs/aperture-policy.md` describes the policy model and intended adoption.

## User experience

### Notification-area icon and menu

Provide one Ozma icon while the desktop runtime is active. Its state should
distinguish disconnected, connecting, ready, active, and needs-attention
conditions without relying on colour alone. Right-click opens a native menu:

1. Connection state and active computer/setup.
2. Recent or available targets, with the active target checked; selecting one
   switches control after the relevant authorization check.
3. Pause/resume control, with a clear indication of whether video, audio, and
   input are currently enabled.
4. Open Ozma, open configuration, and open logs/help.
5. Quit Ozma.

Menu actions must reflect runtime state and show a useful error if an action
fails. Closing the window must not quit the tray process. Reopening it must
restore the current setup and state.

### Intent-led first-run setup

Start with outcomes rather than node profiles, codecs, ports, or providers.
Initial goal cards should cover:

- Control another computer from this computer.
- Show or control this computer from another Ozma endpoint.
- View a machine without sending input.
- Choose a custom setup.

Then ask for the target/source, whether input is allowed, whether audio is
needed, and the desired responsiveness or image quality. Show a preview of the
result in plain language before applying it. The routing resolver can propose
the implementation; users should not need to select its pipeline or transport.

When Ozma cannot fulfil a goal, show the reason, usable alternatives, and the
action needed to proceed. Do not silently widen permissions or claim an
unsupported capability is active.

### Variable configuration aperture

Here, **aperture** means how much configuration detail and precision Ozma
exposes at a given moment. It is broader than access permissions. The user
starts with an outcome; opening the aperture reveals progressively finer
choices, with more capability only where the product can explain and apply it:

1. **Outcome:** “Control this computer from my laptop.”
2. **Guided choices:** whether to include picture, sound, microphone, or input;
   a simple preference such as responsive control or sharper picture.
3. **Fine tuning:** supported bounds for resolution, frame rate, bandwidth,
   quality regions, and foveation (for example, where to spend image quality
   around a gaze/focus region versus the periphery).
4. **Expert detail:** exact transports, codecs, providers, policy frames, and
   diagnostic receipts.

The user can adjust the aperture to show more or less detail without changing
the desired outcome. Defaults should keep complexity low; advanced settings
remain accessible and reversible. The interface must distinguish a preference
from an enforced limit and show which settings are unsupported by the selected
path. Foveation and any other fine-tuning option must not be presented as
active until the selected runtime actually consumes it.

Keep two related ideas distinct in the implementation: the **UI aperture** is
the amount of detail exposed, while the `ozma-aperture` policy evaluation is
the requested-versus-effective capability/quality constraint model. Changing
the UI aperture alone must not widen permissions, change quality targets, or
alter the saved intent.

Permissions are one detailed-settings dimension: show what Ozma may do, where,
and under what limits. Let the user narrow view, keyboard/mouse control, audio,
microphone, clipboard, file transfer, USB, or power only where the backend
supports and enforces the choice. Show requested versus effective settings;
explain device/policy limits and whether the user can change them, request a
negotiated change, or needs an operator. Keep raw policy documents and receipts
under Expert detail.

## Implementation phases

### Phase 0 — freeze the product boundary and desktop integration

- Confirm the supported Windows build and the exact existing
  controller/self-control journey to carry into alpha.
- Locate the executable/process that owns the desktop session and select the
  tray integration point. Reuse the existing UI where practical; do not revive
  the deprecated agent as the product host.
- Define the tray-to-runtime command/status interface and lifecycle (startup,
  window close, logout, shutdown, failure).
- Identify which permissions and quality dimensions have real enforcement
  today. Mark other settings as unavailable or deferred.

**Exit:** one platform-specific shell design, a list of supported actions, and
an agreed runtime/UI contract.

### Phase 1 — intent-led setup in the existing React UI

- Add a first-run and “Add setup” flow with goal cards and progressive
  questions.
- Connect the UI to existing intent listing, feasibility, and explanation
  APIs. Translate results into outcome-focused summaries and alternatives.
- Preview configuration and required permissions before applying it.
- Save and activate the resulting setup through the existing scenario/profile
  mechanism where suitable; document any persistence/API gap rather than
  introducing a second source of truth.

**Exit:** a user can create and re-open at least the supported self-control and
view-only setups without entering technical pipeline details.

### Phase 2 — configuration aperture and policy integration

- Define a versioned configuration representation that separates user intent,
  preferences, hard requirements, permissions, and expert tuning values.
- Make the UI aperture control progressive disclosure over this same
  representation; changing visible detail must not silently change the
  underlying request.
- Connect `ozma-aperture` to values that the chosen runtime actually enforces.
  Use it for session authorization first, then supported quality dimensions
  such as frame rate. Add resolution, bandwidth, and foveation only with real
  consuming capture/encode/render path.
- Preserve chosen/forced/negotiated provenance and explain why a requested
  value differs from the effective value.
- Fail closed for unsupported values, conflicting policies, stale approvals,
  or invalid receipts; do not show an unsupported advanced control as active.

**Exit:** the user can widen/narrow visible detail while intent remains stable;
supported settings change effective behavior; forced limits cannot be bypassed;
negotiated changes remain pending until required parties accept; the UI
explains each outcome.

### Phase 3 — detailed settings and persistence

- Add per-setup settings for permissions, scope, quality, connection, and
  device-specific options, grouped by outcome and progressively revealed.
- Add an aperture-level control (Simple / Guided / Fine-tune / Expert or an
  equivalent continuous control) that changes visible detail, not the chosen
  intent.
- Include foveation or quality-region controls only when supported by the
  selected endpoint path; make effective behavior observable.
- Use safe defaults and clear reset-to-recommended controls. Keep expert fields
  behind Advanced and label which require restart or re-consent.
- Persist settings against the right user, endpoint, and setup; validate them on
  load and show migration/error states.
- Provide an effective-settings view with the source of each limit and the
  latest redacted decision receipt.

**Exit:** settings survive restart, remain scoped to the selected setup, and
the effective summary matches backend state.

### Phase 4 — native tray shell and action wiring

- Implement the Windows notification-area icon and right-click menu in the
  selected desktop host.
- Open the authenticated in-app help guide from the tray; use the setup page's
  endpoint selectors for choosing computers, and offer runtime actions only
  when a real API can perform them.
- Wire target switching, pause/resume, and log access only after their runtime
  behavior and ownership are defined.
- Reflect connection and authorization state; prevent stale menu state from
  implying an action succeeded.
- Keep tray startup, window lifetime, and runtime shutdown behavior explicit.

**Exit:** the user can complete the core operating loop from the tray and the
window can close/reopen without stopping Ozma.

### Phase 5 — alpha integration and release readiness

- Walk the complete install → first-run intent → permission review → apply →
  tray operation → edit settings → restart/reopen flow on a clean supported
  Windows environment.
- Confirm the existing self-control flow still works and that view-only and
  denied/limited cases are understandable.
- Update user documentation and release notes to describe supported goals,
  platform limits, privacy/permission behavior, and known gaps.
- Capture reproducible build and smoke evidence from the frozen alpha commit.

**Exit:** the alpha can be installed and operated by a user without requiring
them to understand Ozma's internal node, pipeline, codec, or transport model.

## Acceptance criteria

- The notification-area icon exposes the current state and the required
  right-click actions; menu state tracks the runtime.
- The app remains running when its main window closes and exits cleanly when
  Quit is selected.
- First-run setup is phrased around user outcomes and produces a reviewable
  configuration before applying it.
- View-only and control-enabled configurations are distinguishable and
  enforced by the session authorization path.
- Users can narrow their chosen aperture; forced limits are explained and
  cannot be relaxed from the user UI; negotiated changes show pending/accepted
  state.
- Requested versus effective settings, limiting reasons, and safe alternatives
  are visible without requiring technical vocabulary.
- The configuration aperture can be opened or narrowed without changing the
  selected intent or silently altering its values.
- Fine tuning (including foveation where implemented) changes a real setting;
  unsupported fine tuning is clearly marked and cannot be applied.
- Unsupported options are explicitly unavailable and never appear enabled.
- Configuration persists across restart and stays associated with the correct
  endpoint/setup.
- The complete alpha journey is demonstrated on the supported Windows build;
  other operating systems are claimed only after equivalent tray and setup
  validation.

## Out of scope for this plan

- Completing all Rust/Python parity or deleting legacy trees.
- Rewriting every existing controller page around intents.
- Exposing every `ozma-aperture` dimension before a product path enforces it.
- Promising Linux/macOS tray support as part of the first Windows alpha UI
  milestone.
- Replacing the routing engine or inventing a new profile store without first
  checking whether scenarios can hold the required settings.

## Initial shell implementation status

The first standalone Windows shell slice is `ozma-desktop`. It adds a native
notification-area icon, a right-click menu, left-click dashboard launch, and
Open setup/Open Ozma/Help actions that launch the existing React UI in the
default browser. Help opens the authenticated `/ui/help` page. The UI URLs are
derived from `OZMA_CONTROLLER_URL` (default
`http://127.0.0.1:7380`); the setup path is `/ui/setup`, matching the React
router's `/ui` basename and `setup` route. `ozma-desktop/build-windows.ps1`
builds and stages the binary and icon on Windows.

The menu also has **Review access on this computer**, which opens
`{OZMA_LOCAL_NODE_URL}/api/v1/local/consent` (default
`http://127.0.0.1:7382/api/v1/local/consent`). The shell does not call approval
APIs or handle node tokens. The node serves the local review page.

The shell does not start or supervise controller/node processes. It does not
switch the active target, pause/resume input or media, provide embedded
settings, or install itself for startup. Computer choices are made in the
authenticated setup page; the tray does not expose a working node-management
page or switch the active target. **Help and setup guide** opens the
authenticated React `/ui/help` page (overridable with `OZMA_HELP_URL`). The
right-click menu probes the controller `/health` and local node `/api/v1/health`
routes over HTTP. This reports only endpoint reachability, not session or
media/input state. Runtime controls remain out until their APIs and lifecycle
ownership are defined.
The currently supported preview steps and limits are documented in
[`gui-configuration-alpha.md`](gui-configuration-alpha.md).

## Target consent and authorization boundary

The node now forces `ConsentRequirement::Always` for any session that requests
remote input, so a controller cannot bypass target consent by sending
`consent: never`. The node serves pending-request metadata and a local review
page only to loopback peers with a loopback Host. Approve/deny POSTs also
require a same-authority plain HTTP Origin; local consent responses strip
development CORS headers, and the page prevents framing. Audit actors come
from the node process OS identity, not a caller-supplied `by`. The older
session decision routes are loopback-gated too. The tray opens the page
without storing or sending the node token.

This establishes a local-machine approval boundary, not a distinct human
identity when people share the node process account. The controller setup page
can refresh approval status and start a session after target approval. A Ready
state still needs an observed media/input connection before the end-to-end
remote-control journey is accepted.

Controller-to-node session calls require a shared node API bearer token. With
`OZMA_API_TOKEN` unset, the node's session routes accept only loopback
connections, and browser requests must be same-origin; the controller proxy
cannot use that local-only mode. The controller must use the matching
registered per-node token or `OZMA_NODE_API_TOKEN`. Health/capability
discovery remains available without a token.

## Progress

- **Phase 0 — partial.** Repository reconnaissance found no suitable existing
  Rust Windows GUI host. `ozma-client` is a headless CLI, `ozma-play` is a
  playback window, and `ozma-agent` is deprecated. The existing Python tray is
  legacy. A standalone `ozma-desktop` shell is now the tray owner and launches
  the current React UI in the default browser. The runtime status/action API,
  target-side consent surface, and process shutdown ownership still need a
  defined contract.
- **Phase 1 — partial.** `controller/ui/src/pages/SetupPage.tsx` now has
  outcome cards for control, view-only, full workspace, and custom setup,
  endpoint selection, route feasibility, progressive Simple /
  Guided / Fine-tune / Expert detail, a dashboard first-run entry, and a
  request/review flow for a capture session. The request asks for local target
  consent, requests one default capture track, and compares selected
  permissions with the node response. Before creating a consent request, the
  UI checks the target's native session transport and explains when it is
  unavailable; the create-session API enforces the same check for other clients.
  Session creation remains pending until the target approves it through the
  local tray link. The controller UI can refresh status and start an approved
  session. Reusable session operation and the complete self-control journey
  remain open until a real media/input connection is observed.
  Version-1 setup profiles now persist
  intent, target node, supported permission choices, consent/indicator/session
  duration, and UI aperture level in the existing Scenario store. Profiles
  can be saved, updated, listed, and loaded; loading one does not grant access
  or start a session. Setup restores the last saved/loaded profile when reopened
  in the same browser. Profiles also preserve the selected source endpoint;
  older version-1 profiles default to the controller source when loaded. The
  loader validates version, intent, endpoints, every permission, consent,
  duration, and aperture level before applying the profile. Unsupported or
  corrupt profiles produce an error. The normal setup view names the target
  computer and uses the signed-in controller as the starting point; changing
  the route-check source is now an Expert-only advanced option. The selected
  source affects route checking only; native session access requests originate
  from the authenticated controller identity. “Control another computer”
  retains its own user-facing goal identifier while using the combined
  picture-and-input routing intent required by its capture session. Earlier version-1 profiles
  without a goal identifier continue to load. Custom profiles preserve the
  custom goal separately from the effective route intent, which follows
  whether keyboard/mouse input is selected. A target-side instruction card
  directs incoming requests to the remote source's setup and this computer's
  tray approval page. Failed route checks identify
  unavailable capabilities and offer view-only continuation when the route
  explanation reports a picture path.
- **Phase 2 — first backend slice implemented.** The native node session API
  returns a permission aperture receipt and a separate quality receipt. It
  grants only permissions implied by the `SessionSpec` and resolves a requested
  frame-rate ceiling against the usable H.264 encoder capability. The same
  effective ceiling is applied to outgoing capture frames. Those permission
  scopes are used in the session token and control gate.
  Control sessions are forced to require consent even when a caller asks for
  `never`.
  An authenticated controller proxy now creates/inspects/starts/stops node
  sessions using registered node addresses and tokens, with node/session
  ownership checks. Profile writes require controller write scope and
  permission to use the registered target node. Target approvals use the
  loopback/same-origin node page and process-account audit identity. This is
  per-session least privilege; account/device grants and permissions outside
  `SessionSpec` remain unintegrated. A session Ready state still does not prove
  media or input is connected. Remote session control is fail-closed without a
  matching node/controller bearer token; no-token session APIs are loopback
  only.
- **UI shell — partial.** Authenticated React routes now share `AppLayout`,
  and the setup page is in the normal sidebar. `ozma-desktop` supplies a
  native Windows icon/menu with Open setup, Open Ozma, Help and setup guide,
  Review access on this computer, and Quit actions. Help opens authenticated
  `/ui/help`; computer selection remains within Setup. A background worker
  refreshes controller and local node health endpoints, so menu opening does
  not wait for DNS or HTTP. The cached status appears in the menu and tooltip
  and reports HTTP reachability only; it does not report active session/media
  state, switch the active target, or operate sessions. The icon is re-added
  after an Explorer restart.
- **Phase 3 — partial.** The profile persistence slice covers current intent,
  permission, maximum frame rate and video bitrate, session, and UI aperture
  choices. Fine-tune and Expert can select 15, 24, or 30 FPS and 1, 2, or
  4 Mbps. The node validates those preferences, applies the lower of the FPS
  request and usable H.264 encoder ceiling, and caps bitrate by the request,
  4 Mbps product limit, and configured node budget. It reports requested and
  effective values and the limiting source; bitrate reaches encoder selection
  and the fake fallback. Expert detail shows policy versions
  and redacted receipt references for permission and quality decisions. Setup
  provides a recommended reset for supported permission/session/quality
  settings without changing intent,
  endpoints, or detail level. Resolution, bandwidth, connection, and
  device-specific settings are not modeled or runtime-enforced yet; foveation
  remains explicitly unavailable. Fine-tune and Expert explain that
  foveation allocates image detail around a gaze/focus region and mark it
  unavailable until the capture path consumes and reports it. UI aperture
  changes detail only and remains distinct from policy aperture, which
  evaluates permission scopes plus FPS and bitrate constraints.
- **Phase 4 — partial.** The standalone tray host can launch/reopen the
  browser UI, open the authenticated in-app help page, show cached
  controller/local-node health reachability, restore its icon after Explorer
  restarts, and quit its own process. It does not supervise runtime/window
  lifecycle or wire active target switching, pause/resume, logs, or session
  connectivity state.
- **Phase 5 — not complete.** The clean Windows alpha journey remains
  outstanding. The project owner reports that the existing Windows build can
  control itself; the native tray/setup route still has not been exercised on
  a supported Windows host. A linked x64 MSVC release executable has now been produced on
  Linux with `cargo xwin`; it is a PE32+ Windows GUI executable. That release
  binary remained running for 12 seconds under Wine/Xvfb until the smoke
  timeout, with no startup error. The current worktree includes
  `ozma-desktop/smoke-wine.sh` and its Win32 helper. The smoke starts the
  executable, dispatches the right-click callback, verifies a popup window,
  posts the Quit command, and asserts a clean process exit; the script passed
  against the current release binary. This validates the native popup callback
  and quit path under Wine. Wine did not provide a usable Windows notification
  area for checking shell icon registration or real mouse interaction with
  the icon.
  Native Windows menu interaction and the clean setup journey remain
  unverified. The user-reported existing self-control path is distinct from
  proving the new tray/setup flow reaches a session with picture and input.
  The Rust-first release workflow now builds the tray on a Windows 2022 runner
  and packages the executable, icon, user setup guide, developer README, and
  both license files into a Windows x64 zip attached to tagged releases. This
  release workflow is complemented by `.github/workflows/ozma-desktop-windows.yml`,
  which builds and uploads the executable and icon on tray-related pull
  requests and pushes. Neither workflow has yet run on GitHub for the current
  worktree. The MSVC cross-build was rerun against the current worktree with
  `cargo xwin build --release -p ozma-desktop --target
  x86_64-pc-windows-msvc`; the result is a PE32+ x64 Windows GUI executable.
  The current local Windows x64 package is
  `target/ozma-desktop-windows-x64.zip` (SHA-256
  `baefda69508837c07c6f795cd4de6bd6f1647628058f8d4f19cf4ce0976bcb12`) and
  contains the tray executable, icon, setup guide, README, and both license
  files.
  No supported Windows image is available locally. Legacy Windows 98/XP disk
  images exist but are not valid targets for this alpha and were not used.
  Microsoft's Windows download page rejects direct shell requests here, and no
  in-app browser is connected. No unrelated Windows VM or disk was started or
  reused.
- **Aperture clarification.** Aperture means progressive configuration
  complexity and precision, not just permissions. Repository search found no
  foveation implementation yet; Fine-tune and Expert describe the setting and
  show it as unavailable until a real runtime path consumes and reports it.
- Verification so far: `cargo check -p ozma-protocol -p ozma-api -p ozma-node`,
  `npm run build`, Python syntax compilation, workflow YAML parsing, and
  `git diff --check` pass. Full relevant Rust suites pass:
  `cargo test -p ozma-node --lib` (799 passed, 8 ignored) and
  `cargo test -p ozma-api --lib` (11 passed). The two relevant controller test
  files pass in full (165 passed, one upstream Starlette/httpx deprecation
  warning) when pytest temp files are put on the workspace disk; the system
  `/tmp` tmpfs filled during an earlier run. The controller test command is:

  ```sh
  TMPDIR="$PWD/target/controller-test-tmp" pytest -q \
    --basetemp="$PWD/target/controller-pytest-basetemp" \
    tests/unit/controller/test_setup_profiles.py \
    tests/unit/controller/test_node_api_token_proxy.py
  ```

  The repeatable Wine tray smoke
  `./ozma-desktop/smoke-wine.sh` passes: it opens the popup callback and exits
  through Quit. The UI type-check
  covers the app entry and all imported routes; orphaned legacy screens are
  outside the bundle/type-check graph. The MSVC
  release build is reproducible with `cargo install cargo-xwin --locked`
  followed by `cargo xwin build --release -p ozma-desktop --target
  x86_64-pc-windows-msvc`; the artifact is
  `target/x86_64-pc-windows-msvc/release/ozma-desktop.exe`. The working
  tree also contains unrelated in-progress changes;
  preserve them and review changes by file before committing.
