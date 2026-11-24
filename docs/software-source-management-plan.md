# Software source management and inventory plan

**Status:** First slice implemented — Steam and Microsoft Store collectors,
controller inventory API, and agent reporting are in place. Remaining sources
and policy/action integration are follow-up work.

## 1. Decision

Steam, Microsoft Store, and similar application ecosystems are **software
sources**, not distribution targets. They are first-class inputs to inventory,
reporting, licensing, compliance, and update-state decisions.

The two axes are separate:

| Axis | Meaning | Examples |
|---|---|---|
| Software source | Where installed software comes from and how it updates | Steam, Microsoft Store/MSIX, winget, Chocolatey, apt/dpkg, dnf/rpm, pacman, Flatpak, Snap, Homebrew, Mac App Store, pkgutil, MSI, vendor updaters, osquery |
| Distribution target | How Ozma itself is packaged and delivered | deb/rpm, MSI/MSIX, DMG/pkg, AppImage, Flatpak, Snap, Docker; Steam/Store only for a consumer client if ever required |

Steam and Store applications are usually **external-managed** from Ozma's
point of view. Ozma reports them, correlates them with licenses and policies,
and optionally launches or triggers the vendor updater. It must not claim it can
patch Steam game files or Store package internals in place.

## 2. Current state

- `ozma-node/src/software/` is the primary implementation (Rust, desktop and
  node profiles): `model.rs` defines the normalized record, `native.rs` collects
  dpkg/rpm/snap, Homebrew, and winget, `steam.rs` parses Steam's VDF metadata,
  `microsoft_store.rs` enumerates Appx/MSIX packages, and `report.rs` posts
  snapshots to the controller.
- `agent/backup.py` keeps its backup-scoped `AppInventoryEntry` inventory and
  reinstall path. It is not the fleet software source of record.
- `controller/software_inventory.py` stores per-host snapshots and exposes the
  `/api/v1/software/*` fleet views.
- `controller/license_manager.py` reconciles installed names against licensed
  products by name, not by source-qualified identity.
- `controller/job_queue.py` defines `PACKAGE_INSTALL`/`PACKAGE_REMOVE` with
  `apt`, `dnf`, `winget`, and `brew`, but there is no agent-side executor yet.
- `controller/backup` exposes `/backup/nodes/{node_id}/apps`, which remains the
  backup-specific view.
- `controller/mdm_bridge.py` inventories Google/Intune/Jamf devices but does not
  deploy software.

## 3. Normalized record

The shared model lives in Rust at `ozma-node/src/software/model.rs` and mirrors
the controller JSON shape:

```text
SoftwareRecord:
  name
  source             # steam | microsoft_store | winget | choco | apt | dnf | ...
  source_id          # Steam AppID, PackageFamilyName, winget ID, package name
  version
  latest_version
  install_path
  scope              # system | user | provisioned | unknown
  user_id / user_sid
  channel
  architecture
  managed_by         # ozma | steam | microsoft_store | winget | vendor | mdm
  update_state       # current | outdated | external_managed | unknown | unsupported
  license_state
  identifiers        # purl/cpe/namespaced identifiers
  metadata           # source-specific fields
```

Source adapters return these records. The controller stores them per host and
builds fleet views by source, title, version, and update state.

## 4. Source adapter contract

Each source implements, where applicable:

```text
discover()          -> source roots, accounts, users, scopes
inventory()         -> SoftwareRecord[]
latest()            -> latest known version/build per title
capabilities()      -> can_install | can_update | can_remove | external_only
install/update/remove (optional)
launch (optional)
```

Sources are not required to support mutation. `external_managed` is a valid
outcome and must be reported honestly.

## 5. Steam source

Steam is read from its own metadata. Do not scan game directories.

### Discovery

- Windows: `HKCU\Software\Valve\Steam\SteamPath`, then
  `steamapps/libraryfolders.vdf`.
- Linux: `~/.steam/steam`, `~/.local/share/Steam`, and Flatpak Steam under
  `~/.var/app/com.valvesoftware.Steam/`.
- macOS: `~/Library/Application Support/Steam`.

### Inventory

- Parse `steamapps/libraryfolders.vdf` for every library root.
- Parse each `steamapps/appmanifest_<appid>.acf`:
  `appid`, `name`, `installdir`, `StateFlags`, `SizeOnDisk`, `buildid`,
  `LastUpdated`, `LastPlayed`, `SharedDepots`.
- Optionally parse `userdata/<id>/config/shortcuts.vdf` for non-Steam shortcuts.
- Record `source: steam`, `source_id: <appid>`, `version: <buildid>`,
  `managed_by: steam`, `scope: user`.

### Update state

- Compare installed `buildid` with Steam's latest build.
- Use `steamcmd +app_info_update 1 +app_info_print <appid>` or an approved Steam
  metadata API.
- Mark `update_state: external_managed` unless a future policy explicitly opts
  into SteamCMD-managed dedicated servers.
- The agent may offer launch via `steam://run/<appid>`; it must not edit game
  files or force updates.

### Libraries

`vdf` (Valve KeyValues), `steamlocate` (library discovery), optional Steam Web
API client. A small internal VDF parser is acceptable as a fallback.

## 6. Microsoft Store source

### Discovery / inventory

- Windows: `Get-AppxPackage -AllUsers` and
  `Get-AppxProvisionedPackage -Online` in the first implementation, or the
  `windows` crate `Management.Deployment.PackageManager` /
  `ApplicationModel` APIs in a native agent.
- Parse `AppxManifest.xml` for `PackageFamilyName`, `PackageFullName`, `Name`,
  `Publisher`, `Version`, `Architecture`, `InstallLocation`, `IsFramework`,
  `SignatureKind`, and `Status`.
- Distinguish user-installed, provisioned, and framework/system packages.
- Record `source: microsoft_store`, `source_id: <PackageFamilyName>`,
  `scope: user|provisioned`.

### Update state

- Store updates are owned by Microsoft Store, Intune, or
  `winget --source msstore`.
- Record `managed_by: microsoft_store` and
  `update_state: external_managed` unless the package is deployed through Ozma
  or Intune.
- Installation may use MSIX/`Add-AppxPackage`, Intune, or `winget` where policy
  allows. Do not patch package internals.

### Libraries

PowerShell `Get-AppxPackage`/`Get-AppxProvisionedPackage` first; Rust `windows`
crate for native enumeration; `msix-packaging`/MakeAppx for packaging;
`winget` for cross-check and optional update triggering.

## 7. Other sources

- Windows: winget, Chocolatey, MSI/uninstall registry, Store/MSIX.
- macOS: Homebrew, Mac App Store (`mas`), `pkgutil --pkgs`, `/Applications`.
- Linux: apt/dpkg, dnf/rpm/zypper, pacman, Flatpak, Snap, AppImage, Steam.
- Cross-platform: osquery tables as a supplemental source and FleetDM parity.
- Vendor-managed applications: record `managed_by` and `update_state`, and do
  not attempt to replace the vendor updater.

## 8. Controller inventory service

Add a controller-side `SoftwareInventoryManager`:

- `ingest(host_id, records, reported_at)` stores the latest inventory snapshot.
- `host(host_id)` returns records and summary counts.
- `titles(source=None, outdated=None)` returns normalized fleet titles.
- `sources()` returns counts and management/update-state summaries.
- `outdated()` returns records that are not `current`, separated by
  `managed_by`.

API sketch:

```text
POST /api/v1/software/hosts/{host_id}/inventory
GET  /api/v1/software/hosts/{host_id}
GET  /api/v1/software/hosts
GET  /api/v1/software/titles?source=steam
GET  /api/v1/software/outdated
GET  /api/v1/software/sources
```

The existing backup app inventory endpoint remains as the backup-specific view;
the new endpoints are the fleet inventory view.

### Node implementation

`ozma-node/src/software/` collects the sources and `report.rs` pushes one
snapshot per host. The node reports immediately at startup and then hourly when
`--register-url` is configured; a failed report retries after five minutes.
Collection runs on a blocking task so PowerShell, `dpkg`, and `snap` never block
the async runtime. The ingest POST is exempt from controller auth like
registration and heartbeat; the fleet read views stay authenticated.

## 9. Reporting and actions

- Inventory: show source, version, install path, scope, and user.
- Update state: distinguish `current`, `outdated`, `external_managed`, and
  `unknown`; never label an externally managed title as unpatched without
  qualification.
- Compliance: map Steam/Store records to application-control evidence and
  vendor-managed patch status.
- Licensing: reconcile on `(source, source_id)` first, then name.
- Jobs: only package sources with verified mutation support appear as
  install/update targets. Steam and Store default to report/launch.
- Audit: record inventory collection, source discovery, and any external
  launch or update trigger.

## 10. Implementation phases

1. Shared `SoftwareRecord` model and source adapter contract. **Done (Rust).**
2. Steam collector with VDF manifest parsing and tests. **Done (Rust).**
3. Microsoft Store collector with injectable PowerShell runner and tests.
   **Done (Rust).**
4. Controller inventory manager and API with tests. **Done (Python controller).**
5. Integrate collectors into the node inventory path and post snapshots to the
   controller. **Done (Rust `ozma-node`).**
6. Add remaining sources (Flatpak, Chocolatey, Mac App Store, pkgutil, osquery).
7. Extend license, compliance, and job semantics with `managed_by` and
   `update_state`.

## 11. Acceptance criteria for the first slice

- Steam discovery handles multiple libraries and app manifests.
- Steam records include AppID, name, build ID, install path, and user scope.
- Microsoft Store records include PackageFamilyName, version, architecture,
  install location, and user/provisioned scope.
- Controller ingest stores per-host snapshots and exposes source and update
  summaries.
- No code attempts to patch Steam or Store applications in place.
- Tests run without Steam, Windows, or a real Microsoft Store installation.
