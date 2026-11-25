# ozma-usb

Safe USB gadget, virtual-media, and USB/IP control foundations for Ozma nodes.

This crate is the model and controller layer of the Python
`softnode/virtual_usb.py`, `softnode/usb_mass_storage.py`, and
`softnode/virtual_media.py` paths. It builds FAT32 images in memory, adds a
bounded copy-on-write write overlay, models ISO/NBD media, includes a bounded
fixed-newstyle NBD server backed by a regular file, an in-memory image, or a
copy-on-write overlay, serves the USB mass-storage SCSI/BOT data path over that
same overlay, and includes a bounded in-process USB/IP daemon/control plane, a
bounded version-`0x0111` USB/IP control-wire server (`OP_REQ_DEVLIST`/
`OP_REQ_IMPORT`) with an injectable stream transport, and a typed URB codec plus
bounded data relay (`USBIP_CMD_SUBMIT`/`USBIP_RET_SUBMIT`/
`USBIP_CMD_UNLINK`/`USBIP_RET_UNLINK`) with an injectable endpoint. The server
hands the same authenticated connection to the URB relay after a successful
import, through an injectable `UsbIpKernelDataEndpoint` seam; the default seam is
a typed `usbip-kernel-required` failure. Real adapters are now provided:
`LinuxUsbIpHostBackend` drives the kernel `usbip`/sysfs host path with typed
module/command checks, and `LinuxUsbIpKernelDataEndpoint` speaks the imported
USB/IP TCP URB phase with devid translation and per-request deadlines. It
sequences FunctionFS from ConfigFS function creation through descriptor writes,
readiness, and UDC bind/unbind, and includes a `poll(2)`-based nonblocking
`PollingFileEndpointIo`/`PollingFileBulkTransport` with enforced per-call
deadlines alongside the original blocking file adapter.
Default paths do not mount filesystems, open `/dev/hidg*`, or write a block
device; an explicit `CommandFunctionFsMounter` can mount FunctionFS when the
caller opts in and supplies the required privileges. `ozma-node` wires the
gadget and NBD layers behind explicit USB flags (see below); there is still no
automatic gadget creation on a hardware profile.

## What is implemented

| Area | API |
|---|---|
| Composite gadget model | `UsbGadgetConfig`, `UsbFunction`, `UsbFunctionKind`, `HidProtocol` |
| Functions | HID keyboard/mouse/custom, mass storage, UAC2 (`Uac2Config`), FunctionFS vendor (`FunctionFsConfig`) |
| FunctionFS model | `FunctionFsConfig`, `FunctionFsEndpoint`, `FunctionFsStrings`, `FunctionFsDaemonSpec`, `FunctionFsReadiness`, `FunctionFsStatus`, `FunctionFsDaemon`/`MissingFunctionFsDaemon`/`ObservedFunctionFsDaemon`, generated device/config/string descriptors, class-specific descriptors, Microsoft OS descriptors |
| FunctionFS daemon core | `FunctionFsMount`, `FunctionFsDescriptors` (v2 `ep0` blob + strings), `FunctionFsDaemonCore`, `LoopbackFunctionFsDaemon`/`FakeFunctionFsDaemon`, `FileFunctionFsDaemon`, `FunctionFsControlRequest`/`FunctionFsControlResponse`, `FunctionFsEvent`, injectable `FunctionFsEp0Io`/`FunctionFsEndpointIo`, bounded endpoint loops, blocking `FileEndpointIo` and nonblocking `PollingFileEndpointIo`/`PollingFileEndpointOpener` |
| FunctionFS lifecycle | `FunctionFsLifecycleConfig`, `FunctionFsLifecycle`, `FunctionFsLifecyclePhase`/`Status`, `FunctionFsStateMachine`, `FunctionFsDeviceState`/`Status`, injectable `FunctionFsMounter` with `CommandFunctionFsMounter`/`FakeFunctionFsMounter`, deterministic `FunctionFsLifecycleClock`; ConfigFS function/link creation, mountpoint expectation, `ep0` descriptor write, readiness timeout, explicit UDC bind/unbind readiness, bind rollback, unbind/cleanup |
| FunctionFS control | Standard `GET_DESCRIPTOR` (device/configuration/string), `SET_CONFIGURATION`, `GET_CONFIGURATION`, `GET/SET_INTERFACE`, `GET_STATUS`, feature requests; typed rejection for class/vendor and unknown requests |
| FunctionFS status | `FunctionFsDaemonStatus` nested in `FunctionFsStatus.daemon`: `live` only when the descriptor blob, string table, and enable event all succeeded; `state`/`running`/`endpoint_events` only reach `running` after real endpoint traffic |
| UAC2 model | `Uac2Config`, `Uac2Mode::{Microphone,Headset,Speaker}`, `Uac2DescriptorSet`, `Uac2Status`, `Uac2BridgeStatus::{Unsupported,Configured,Running,Stopped}` |
| UAC2 bridge core | `Uac2Bridge`, `Uac2BridgeConfig`, injectable `Uac2EndpointIo` and `Uac2AudioSink`/`Uac2AudioSource`, `Uac2Packetizer`, `Uac2PcmFormat`/`Uac2SampleSize`, `convert_pcm`, `Uac2BridgeCounters`/`Uac2BridgeReport`, `FakeUac2Endpoint`/`FakeUac2AudioSink`/`FakeUac2AudioSource` |
| Validation | Safe gadget/function names, VID/PID, strings, power, duplicate links, HID report shape, UAC2 rates/sizes/channels/modes, FunctionFS endpoint bounds/descriptor size, CD-ROM read-only |
| Virtual media | `MassStorageImage`, `VirtualMedia`, `MediaMode::{Disk,CdRom}`, `MediaFormat::{Raw,Iso9660,Fat32,Nbd}`, `MediaState::{Attached,Detached}` |
| Controller trait | `UsbGadgetController` (`create`, `bind`, `unbind`, `status`, `list`, `attach_media`, `detach_media`, `media_state`, `capabilities`) |
| Linux ConfigFS | `ConfigFsUsbController`, injectable root, default `/sys/kernel/config/usb_gadget` |
| Test/dry-run fake | `FakeUsbGadgetController` |
| USB/IP model | `UsbIpConfig`, `UsbIpDevice`, `UsbIpState`, `UsbIpAttachment`, `UsbIpAuthorization`/`UsbIpKey`/`UsbIpToken` (host + token allow-lists), `UsbIpPeerGrant`, `UsbIpHotplugLog`/`UsbIpHotplugEvent`, `UsbIpTlsPolicy`/`UsbIpTlsStatus` |
| USB/IP control | `UsbIpController`, `SubprocessUsbIpController` (`attach_with_token`, peer-side `authorize_peer`/`export_with_token`), bounded hotplug events, argv-only `CommandRunner`; legacy `UsbIpControl`/`SubprocessUsbController` |
| USB/IP daemon | `UsbIpDaemon`, `UsbIpDaemonConfig`/`UsbIpDaemonState`/`UsbIpDaemonStatus`, bounded `UsbIpConnection`s, injectable `UsbIpKernelBackend` with explicit `NoUsbIpKernel`, export/list/attach/detach/unexport state machine, typed `usbip-kernel-required`, fail-closed TLS |
| USB/IP kernel adapter | `LinuxUsbIpHostBackend` over argv-only `CommandRunner` + injectable `UsbIpKernelSysfs` (default `/sys`): `usbip bind`/`unbind`/`attach`/`detach`/`port`, module/driver/attach-attribute checks, binding verification and rollback, vhci port parsing (zero-based, never invented), peer authorization before any command |
| USB/IP wire protocol | `UsbIpOpHeader`/`UsbIpOpCode`/`UsbIpStatus`, `UsbIpDeviceDescriptor`/`UsbIpInterface`, `UsbIpDevListRequest`/`UsbIpDevListReply`, `UsbIpImportRequest`/`UsbIpImportReply`, version `0x0111`, network byte order, fixed-width path/busid, bounded counts, typed truncation/oversize/version/opcode errors |
| USB/IP control server | `UsbIpServer`/`UsbIpServerHandle`/`UsbIpServerConfig`, injectable `UsbIpTransport`/`UsbIpClock`/`UsbIpCredentialSource`/`UsbIpKernelDataEndpoint`, bound TCP listener or in-memory duplex, `OP_REQ_DEVLIST` from descriptor-registered exports, authorized `OP_REQ_IMPORT` only after a real kernel attach **and** an opened data endpoint, same-connection URB handoff, bounded connections/frames/idle timeout |
| USB/IP URB codec | `UsbIpCmdSubmit`/`UsbIpRetSubmit`/`UsbIpCmdUnlink`/`UsbIpRetUnlink`, `UsbIpUrb`/`UsbIpUrbCommand`, `UsbIpDirection`/`UsbIpIsoPacketDescriptor`, 48-byte header at kernel offsets, network byte order, transfer/ISO/count bounds, typed truncation/oversize/direction/endpoint errors, payload-redacted `Debug` |
| USB/IP data relay | `UsbIpRelay` (seqnum pairing, in-flight ceiling, unlink, drain), `UsbIpDataEndpoint` (submit → completion, unlink), `UsbIpKernelDataEndpoint`/`NoUsbIpKernelDataEndpoint` adapter seam, `LinuxUsbIpKernelDataEndpoint`/`LinuxUsbIpUrbSession` over an imported TCP URB stream with `TcpUsbIpConnector`/injectable `UsbIpUrbConnector`, `serve_data_session` (concurrent bounded workers, out-of-order completions, idle timeout, disconnect cleanup), `UsbIpRelayConfig`/`UsbIpRelayStats`/`UsbIpRelaySessionOutcome` |
| SCSI mass storage | `MassStorageDevice`, `ScsiCommandBlock`, `ScsiReply`/`ScsiSense`/`ScsiStatus`, `MassStoragePersistPolicy`, `MassStorageOverlayStatus`; BBB dispatch, read-only/CD-ROM rejection, copy-on-write writes through `WriteOverlay`, FLUSH/stop persistence |
| FAT32 builder | `Fat32ImageBuilder`, `Fat32Node`, `build_fat32_image`, `fat32_image_from_directory`, `parse_fat32_image`, `Fat32Info`/`Fat32Layout` |
| ISO media | `IsoImage::inspect`, `parse_iso_volume_descriptors`, read-only `MediaFormat::Iso9660` |
| NBD model | `NbdExport`, `NbdExportDescriptor`, `NbdHandshake`, `NbdRequest`/`NbdReply` framing, newstyle option constants |
| NBD server | `NbdServer`/`NbdServerHandle` over TCP or Unix sockets, `NbdExportCatalog`, `NbdExportSource` (`overlay_file`/`overlay_media`/`overlay_bytes`), `NbdMemoryImage`, `NbdFileImage`, `NbdOverlayBackend`/`NbdOverlayPolicy`/`NbdOverlayStatus`, injected `NbdTransport`/`NbdClock`, bounded read/write/flush/trim/disc, read-only exports, copy-on-write writable exports |
| Write overlay | `WriteOverlay`, `OverlayConfig`, `OverlayWriteReport`; bounded copy-on-write over FAT32/raw base bytes, per-block allocation accounting, checksummed atomic persistence, typed `overlay-bounds`/`overlay-corrupt` failures; wired into writable NBD exports |
| Media restart | `MediaRestartPolicy`, `MediaRestartTracker`, `MediaRestartAttempt`; capped attempts, capped exponential backoff, overlay-reload policy, and a success reset for the userspace SCSI export |
| Capabilities/status | `UsbGadgetCapabilities`, `GadgetStatus`, `FunctionStatus` (`media`, `uac2`, `function_fs` details); `protocol_capability()` projects onto `ozma_protocol::UsbCapability` |

## Quick start

```rust
use ozma_usb::{
    ConfigFsUsbController, MassStorageImage, UsbFunction, UsbGadgetConfig,
    UsbGadgetController,
};

let controller = ConfigFsUsbController::with_default_root();
let config = UsbGadgetConfig::new("ozma", "dummy_udc.0")
    .with_function(UsbFunction::keyboard("kbd"))
    .with_function(UsbFunction::mouse("mouse"))
    .with_function(UsbFunction::mass_storage(
        "usb0",
        Some(MassStorageImage::cdrom("/srv/installer.iso")),
    ));

let status = controller.create_and_bind(&config)?;
assert!(status.bound);
# Ok::<(), ozma_usb::UsbError>(())
```

Runtime media can be swapped without recreating the gadget:

```rust
use ozma_usb::{UsbGadgetController, VirtualMedia};

let media = VirtualMedia::disk("usb0", "/srv/media.img");
controller.attach_media("ozma", &media)?;
// ... target reads/writes the LUN ...
controller.detach_media("ozma", "usb0")?;
# Ok::<(), ozma_usb::UsbError>(())
```

The node now selects the controller from configuration:

```bash
ozma-node --usb-gadget --usb-udc dummy_udc.0 \
  --usb-configfs-root /sys/kernel/config/usb_gadget \
  --usb-media /srv/installer.iso --usb-media-cdrom \
  --usb-media-export /srv/installer.iso \
  --usb-media-export-format iso9660 \
  --usb-uac2 --usb-uac2-bridge \
  --usb-uac2-sink-device hw:0,0 --usb-uac2-source-device hw:0,1 \
  --usb-vendor --usb-vendor-daemon /usr/local/bin/ozma-ffs
```

`--usb-media` creates the ConfigFS kernel mass-storage LUN; `--usb-media-export`
is the separate opt-in userspace SCSI export. For a writable raw export add
`--usb-media-export-read-write --usb-media-export-persist /var/lib/ozma/overlay.bin`
and optionally `--usb-media-export-reload`. Restart recovery defaults to three
attempts with 250 ms→4 s capped exponential backoff; override it with
`--usb-media-restart-max-attempts`, `--usb-media-restart-backoff-ms`, and
`--usb-media-restart-max-backoff-ms`.

Equivalent environment variables are `OZMA_USB_GADGET`, `OZMA_USB_CONFIGFS_ROOT`,
`OZMA_USB_UDC`, `OZMA_USB_MEDIA`, `OZMA_USB_MEDIA_RO`,
`OZMA_USB_MEDIA_CDROM`, `OZMA_USB_MEDIA_EXPORT`,
`OZMA_USB_MEDIA_EXPORT_FORMAT`, `OZMA_USB_MEDIA_EXPORT_READ_WRITE`,
`OZMA_USB_MEDIA_EXPORT_PERSIST`, `OZMA_USB_UAC2`, `OZMA_USB_UAC2_BRIDGE`,
`OZMA_USB_UAC2_SINK_DEVICE`, `OZMA_USB_UAC2_SOURCE_DEVICE`,
`OZMA_USB_VENDOR`, and `OZMA_USB_VENDOR_DAEMON`. The node builds a HID
keyboard + mouse gadget, optionally a ConfigFS mass-storage LUN, an opt-in
userspace SCSI export, and opt-in UAC2/FunctionFS vendor functions, then
creates and binds it only when a UDC is supplied and the root exists. A missing
UDC/root/permission/media or a bind failure is logged with its typed `kind()`;
the node keeps running, advertises no `usb` capability, and `GET /usb` answers a
typed 503 rather than fabricating a gadget. Without `--usb-gadget`, `/usb` stays
a typed 501 and no ConfigFS write occurs.

When bound, `ozma-node` advertises `usb` in capabilities, TXT/registration, and
`/api/v1/connection.usb`, and exposes:

| Route | Behaviour |
|---|---|
| `GET /usb` | Bearer-guarded read-only gadget status, function list, capabilities, LUN media state, UAC2 descriptor/bridge status, and FunctionFS daemon readiness (no backing paths). Nested `media_export` reports `not-configured`/`configured`/`running`/`failed`, `auto_start`, overlay state, counters, and bounded restart state. |
| `GET /usb/media` | Userspace SCSI export state, capacity, overlay/persistence summary, counters, and typed `last_error`. |
| `POST /usb/media/start` | Configure a userspace SCSI export from an existing image (`path`, `function`, `format`, `read_only`, `persist_path`, `persist_on_flush`, `persist_on_stop`, `reload`, `max_transfer_bytes`, `timeout_ms`). Never claims `running` before a transfer. |
| `POST /usb/media/stop` | Stop the export and apply its overlay persistence policy. |
| `GET /usb/hotplug` | Bounded poll view of `export_start`, `export_stop`, `attach`, `detach`, `unbind`, `restart`, and `rebind` events. Auto-start is recorded as `export_start` with state `auto-start`. |
| `POST /usb/media/attach` | Attach an existing absolute regular file to a mass-storage instance; enforces read-only/CD-ROM. |
| `POST /usb/media/detach` | Clear the LUN `file` attribute; the function/gadget stay in place. |
| `GET /usb/nbd` | Running loopback NBD export summary and counters (`running`, URL, export, size, read-only, writable overlay status, stats). |
| `POST /usb/nbd/start` | Serve one existing absolute FAT32/ISO/raw file over loopback NBD (`path`, optional `export`, `format`, `read_only`, `persist_path`, `persist_on_flush`, `persist_on_stop`). ISO/FAT32 are forced read-only; raw defaults to read-only; a second start is a typed 409. A writable raw export is copy-on-write. |
| `POST /usb/nbd/stop` | Stop the opt-in NBD listener, persist any configured overlay, and return its final counters. |

The userspace export auto-starts only when `--usb-media-export` is set. It is
reported as `configured` (not `running`) and uses `IdleBulkTransport`; only a
real command cycle over an attached transport can mark it running. A configured
auto-start failure keeps the typed reason in `/usb` and `/usb/media` instead of
failing node startup. On SIGINT/SIGTERM the export is stopped and its overlay is
flushed according to `persist_on_stop`.

Attach/detach write only ConfigFS `file`/`ro`/`cdrom` attributes. The NBD
routes only bind a loopback TCP socket and read the backing file; they do not
mount, create, truncate, or write the backing image. A raw export started with
`read_only: false` writes into a bounded in-memory `WriteOverlay`; the base file
stays immutable, and an optional `persist_path` receives an atomic checksummed
overlay file on FLUSH/stop according to `persist_on_flush`/`persist_on_stop`.
Like the rest of the USB router, the NBD routes answer a typed 501 unless the
node was started with `--usb-gadget`.

Formats are declarative and additive. Existing `disk`/`cdrom` images keep the
opaque `Raw` format, so their JSON and ConfigFS behaviour is unchanged:

```rust
use ozma_usb::{MassStorageImage, MediaFormat, VirtualMedia};

let iso = MassStorageImage::iso("/srv/installer.iso"); // CD-ROM + read-only
let fat = MassStorageImage::fat32("/srv/generated.img"); // disk + read-only
let nbd = MassStorageImage::nbd("/srv/live.img"); // descriptor, not a LUN file
assert_eq!(iso.format, MediaFormat::Iso9660);
assert_eq!(fat.format, MediaFormat::Fat32);
assert_eq!(nbd.format, MediaFormat::Nbd);
```

`MassStorageImage::validate_for_attach` probes an ISO primary volume descriptor
or a FAT32 boot sector before ConfigFS attach. NBD media is rejected for direct
ConfigFS attach with `unsupported-media-format`; serve it through
`NbdExportSource`/`NbdServer` instead (or the opt-in `/usb/nbd/*` routes).

## ConfigFS behavior

`ConfigFsUsbController` uses ordinary `std::fs` calls against its injected
root. It creates:

```text
<root>/<name>/
  idVendor  idProduct  bcdDevice  bcdUSB  UDC
  strings/0x409/{manufacturer,product,serialnumber}
  configs/c.1/
    MaxPower
    strings/0x409/configuration
    <function-name> -> <absolute path to functions/<function-name>>
  functions/
    hid.<instance>/{protocol,subclass,report_length,report_desc}
    mass_storage.<instance>/{stall,lun.0/{file,ro,cdrom,removable}}
    uac2.<instance>/{p_chmask,c_chmask,p_srate,c_srate,p_ssize,c_ssize,req_number}
    ffs.<instance>/        (FunctionFS daemon supplies descriptors/endpoints)
```

- `create_gadget` leaves the gadget **unbound**; `bind` writes `UDC`, or
  `create_and_bind` does both and rolls back on a bind failure.
- `mass_storage` LUN flags (`ro`, `cdrom`) are written while the LUN is empty,
  then `file` is written. `detach_media` clears `file` only.
- CD-ROM images are forced read-only at the model layer.
- `remove_gadget` unbinds, removes configuration symlinks, and removes the
  function/config/string directories. Its cleanup fallback only descends into
  a directory when `rmdir` returns `ENOTEMPTY`; on a real ConfigFS tree
  `rmdir` removes virtual children directly.
- Missing roots, missing gadgets, missing functions, duplicate gadgets,
  invalid names, and missing media files are all typed `UsbError` values with
  stable `kind()` strings.
- The implementation does not call `mount`, open block devices, or use
  `unsafe`.

## FunctionFS and UAC2

### FunctionFS vendor functions

`FunctionFsConfig` models the descriptor contract a userspace daemon must
satisfy. `UsbFunction::vendor_with_config` embeds it in a gadget; the older
`UsbFunction::vendor(instance, vid, pid)` constructor still creates a
vendor-class function with the default strings.

```rust
use ozma_usb::{
    FunctionFsConfig, FunctionFsDaemonSpec, FunctionFsEndpoint, FunctionFsStrings,
    UsbFunction, UsbGadgetConfig,
};

let vendor = FunctionFsConfig::new("vendor0", 0x1209, 0x0001)
    .with_interface(0xff, 0x00, 0x00)
    .with_strings(FunctionFsStrings {
        manufacturer: "Ozma Labs".into(),
        product: "Ozma Vendor".into(),
        serial_number: "OZMA-VENDOR-1".into(),
        configuration: "Vendor Configuration".into(),
    })
    .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
    .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512))
    .with_daemon(
        FunctionFsDaemonSpec::new("/usr/local/bin/ozma-ffs")
            .with_arg("--instance")
            .with_arg("vendor0")
            .with_ready_marker("/run/ozma/ffs-vendor0.ready"),
    );
let gadget = UsbGadgetConfig::new("ozma", "dummy_udc.0")
    .with_function(UsbFunction::vendor_with_config(vendor));
```

`FunctionFsConfig::descriptors()` generates the 18-byte device descriptor,
configuration/interface/endpoint descriptors, and standard string descriptors
used by status/control surfaces. `FunctionFsConfig::ep0_descriptors()` builds
what FunctionFS actually accepts on `ep0`:

- a v2 descriptor header (magic `3`, length, flags, FS/HS/OS counts);
- function-scoped full-speed and high-speed descriptor sets (interface,
  optional class-specific descriptors, endpoints; device and configuration
  descriptors are reserved for the composite gadget);
- optional Microsoft OS compatible-ID/properties blocks (`wIndex` 4/5);
- a separate UTF-8, NUL-terminated string table with one `0x0409` language.

Bounds are validated before any write: 1–15 non-control endpoints, unique
endpoint addresses in `0x01..=0x0f` / `0x81..=0x8f`, bulk packets ≤512 bytes,
interrupt/isochronous packets ≤1024 bytes, intervals 1..=16 for
interrupt/isochronous, strings ≤126 bytes, class descriptors 2–255 bytes,
bounded OS properties, and a descriptor blob ≤4096 bytes. The full-speed set
clamps bulk/interrupt packets to 64 and isochronous packets to 1023; the
high-speed set keeps the configured sizes. Malformed magic, length, `bLength`,
reserved gadget descriptors, trailing bytes, and truncated OS descriptors are
typed `invalid-functionfs-descriptor` failures.

ConfigFS only gets the `functions/ffs.<instance>` directory and the
configuration symlink. The kernel does **not** take descriptors from ConfigFS.
The daemon core models the userspace contract without mounting anything:

```rust
use ozma_usb::{
    FunctionFsConfig, FunctionFsDaemon, FunctionFsEndpoint, FunctionFsEvent,
    FakeFunctionFsDaemon,
};

let config = FunctionFsConfig::new("vendor0", 0x1209, 0x0001)
    .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
    .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512));
let descriptors = config.ep0_descriptors()?;
let mut daemon = FakeFunctionFsDaemon::new(config.clone())?;
daemon.write_descriptors(&config, &descriptors)?; // two ep0 writes
daemon.handle_event(&config, FunctionFsEvent::Enable)?; // opens endpoints
if let Some(endpoint) = daemon.endpoint_mut(0x01) {
    // read/write through an injectable FunctionFsEndpointIo
}
# Ok::<(), ozma_usb::UsbError>(())
```

`FunctionFsMount::new` / `for_instance` validates a mount directory and `ep0`
(regular test file, character device, or FIFO) and `write_descriptors` writes
the blob then the string table. `FunctionFsDaemonCore` takes injectable
`FunctionFsEp0Io` and `FunctionFsEndpointOpener`; `LoopbackFunctionFsDaemon`
uses in-memory endpoints, and `FileFunctionFsDaemon::open` uses an explicitly
supplied mount (endpoint files open lazily on enable, as FunctionFS creates
them after descriptors). Read/write loops reject wrong-direction transfers and
buffers over 64 KiB. `handle_control_request` answers standard
`GET_DESCRIPTOR` (device/configuration/string), `SET_CONFIGURATION`,
`GET_CONFIGURATION`, `GET/SET_INTERFACE`, `GET_STATUS`, and feature requests;
class/vendor and unknown requests return typed
`unsupported-functionfs-control` errors.

#### FunctionFS lifecycle helper

`FunctionFsLifecycle` sequences the ConfigFS, mount, descriptor, readiness, and
UDC steps. It is deliberately explicit about privileged operations: by default
it has **no mounter**, so a missing mountpoint is a typed
`missing-functionfs-mount`/`missing-functionfs-ep0` error rather than an
automatic `mount` call. Callers can inject:

- `CommandFunctionFsMounter` — argv-only `mount -t functionfs <instance>
  <mountpoint>` / `umount <mountpoint>`. It requires the caller's privileges and
  returns `missing-command`/`command-failed` when the tools or mount fail.
- `FakeFunctionFsMounter` — creates a temporary mountpoint with `ep0`/`epN`
  regular files; used by the default tests and needs no root.
- `FunctionFsLifecycleClock` — `SystemFunctionFsLifecycleClock` or the
  deterministic `ManualFunctionFsLifecycleClock`.

```rust
use ozma_usb::{
    FakeFunctionFsMounter, FunctionFsConfig, FunctionFsEndpoint,
    FunctionFsLifecycle, FunctionFsLifecycleConfig,
};

let function = FunctionFsConfig::new("vendor0", 0x1209, 0x0001)
    .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
    .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512));
let config = FunctionFsLifecycleConfig::new(
    "ozma-test",
    "/sys/kernel/config/usb_gadget",
    "dummy_udc.0",
    function,
)
.with_mountpoint("/dev/ffs-vendor0")
.with_readiness(5_000, 50);
let mut lifecycle =
    FunctionFsLifecycle::new(config)?.with_mounter(FakeFunctionFsMounter::new());
lifecycle.prepare()?;            // gadget + ffs.vendor0 + config link
lifecycle.mount()?;              // validates or calls the mounter
lifecycle.write_descriptors()?;  // v2 blob then strings to ep0
lifecycle.wait_for_ready()?;     // marker polling, never assumed
lifecycle.bind()?;               // writes UDC
lifecycle.unbind()?;             // clears UDC
lifecycle.cleanup()?;            // removes gadget; unmounts only if it mounted
# Ok::<(), ozma_usb::UsbError>(())
```

`prepare` uses `ConfigFsUsbController` against an injectable root and verifies
the `functions/ffs.<instance>` directory and configuration symlink.
`wait_for_ready` polls the daemon readiness marker with a deadline; a missing
marker/program or expired deadline returns `functionfs-readiness-timeout` and
never marks the function ready. `bind` refuses unless the phase is `ready`. A
bind failure unbinds/removes the gadget, unmounts a lifecycle-owned mount, and
returns the original typed error with phase `failed`. `cleanup` is idempotent
and leaves a caller-supplied mount in place. `FunctionFsLifecycleStatus` exposes
the phase, bound/mounted/ep0/string flags, paths, the nested `FunctionFsStatus`,
and a `device` snapshot.

#### FunctionFS device state machine

`FunctionFsStateMachine` is the typed, event-driven companion to the ConfigFS
lifecycle helper. Its states and transitions are explicit:

```text
unconfigured --bind_descriptors--> descriptors_bound
descriptors_bound --mark_ep0_ready--> ep0_ready
ep0_ready --bind_udc (UDC write)--> ep0_ready + udc_bound
ep0_ready --enable event--> enabled
enabled --endpoint transfer--> running
enabled|running --disable event--> disabled
any bound state --unbind event/UDC clear--> unbound
```

`udc_bind_ready()` is true only after descriptors and `ep0` readiness and while
no UDC is bound; `udc_unbind_ready()` is true only while `udc_bound`. Out-of-order
events return `invalid-functionfs-state`. Crucially, `running` requires a
completed endpoint transfer (`record_endpoint_event`): descriptors, a mount, a
readiness marker, a UDC bind, and an enable event are never enough. The same
machine is embedded in `FunctionFsDaemonCore`; its status exposes `state`,
`running`, `udc_bound`, and `endpoint_events`. `FunctionFsLifecycle` uses the
machine for descriptor/ep0/bind/unbind readiness but never opens data endpoints,
so `status.running()` stays false there (the ignored `real_udc_*` test asserts
this).

Readiness is explicit and never assumed:

| `FunctionFsReadiness` | Meaning |
|---|---|
| `daemon_not_configured` | No daemon path is modelled. |
| `daemon_missing` | A configured daemon path does not exist. |
| `daemon_not_ready` | The program exists but the readiness marker does not. |
| `ready` | Marker present, or a live daemon core reported ready. |
| `unknown` | A ConfigFS status reader has no daemon metadata. |

`FunctionFsStatus` is embedded in `FunctionStatus.function_fs`. Its optional
`daemon` field reports `ep0_written`, `strings_written`, `enabled`,
`configured`, open endpoints, and counters. `daemon.live` is true only after
both `ep0` writes succeeded **and** an enable event opened the endpoints; a
marker file alone never sets it. The `FunctionFsDaemon` trait
(`MissingFunctionFsDaemon`, `ObservedFunctionFsDaemon`,
`LoopbackFunctionFsDaemon`, `FileFunctionFsDaemon`) and
`FunctionFsConfig::require_ready()` provide a typed
`unsupported-functionfs` failure when no daemon is ready. No daemon is spawned
and no vendor protocol is implemented.

#### FunctionFS SCSI bulk transport loop

`MassStorageLoop` connects the SCSI model to injectable bulk endpoints. The
transport trait is deliberately small:

```rust
use ozma_usb::{
    MassStorageDevice, MassStorageLoop, MassStorageLoopConfig, MemoryBulkTransport,
};

let mut transport = MemoryBulkTransport::new();
transport.push_host_data(&read10_cbw)?; // a 31-byte CBW
let device = MassStorageDevice::read_only_bytes(image_bytes, 256 * 1024 * 1024)?;
let mut loop_ = MassStorageLoop::new(device, transport, MassStorageLoopConfig::default())?;
let outcome = loop_.step()?; // reads CBW/data, dispatches SCSI, writes data + CSW
assert!(loop_.is_running()); // only after a completed command cycle
# Ok::<(), ozma_usb::UsbError>(())
```

`MassStorageBulkTransport` has `read_out`, `write_in`, and `flush_in`; each call
receives the configured deadline and must return
`UsbError::MassStorageEndpointTimeout` for a stalled endpoint. The loop assembles
short CBW/data transfers, rejects declared data phases above
`MassStorageLoopConfig::max_transfer_bytes` (sector aligned, default 1 MiB,
hard ceiling `SCSI_MAX_TRANSFER_BYTES`), counts typed timeout/bound/transport
failures, and never claims `running` before a CSW was written. Malformed CBWs
and transport failures are typed errors; SCSI failures are normal check
conditions with a CSW. `SYNCHRONIZE CACHE(10)` and `stop()` apply the configured
`MassStoragePersistPolicy` to the overlay.

`FunctionFsBulkTransport` adapts an enabled `FunctionFsDaemonCore`; it clamps
reads and chunks writes to the FunctionFS 64 KiB per-transfer limit, so a 1 MiB
SCSI data phase is assembled across several endpoint calls. `FileBulkTransport`
owns two explicitly supplied endpoint files for a caller-managed mount, so it
can be boxed into a long-lived export. Default tests drive
the loop through `MemoryBulkTransport`, owned temporary endpoint files, and
FunctionFS loopback endpoints only.
No test mounts FunctionFS, opens a real UDC endpoint, or claims a live host
transfer. The node uses `IdleBulkTransport` for a configured-but-not-attached
export, and only attaches a transport after the daemon enables its endpoints.

#### Nonblocking endpoint deadlines

`PollingFileEndpointIo` is the deadline-enforcing sibling of the original
blocking `FileEndpointIo` (which remains available unchanged). It switches an
already-open endpoint descriptor to `O_NONBLOCK` and waits with `poll(2)` for
readability/writability or the supplied deadline; it never spins and never
blocks an export worker forever. `read_timeout`/`write_timeout`/`flush_timeout`
accept explicit millisecond deadlines (1 ms..=60 s), and the
`FunctionFsEndpointIo` trait methods use the handle's configured default. A
timed-out read consumes no bytes; a timed-out write reports
`mass-storage-endpoint-timeout` after whatever prefix the kernel already
accepted, so the caller must abort rather than retry as if unwritten. Closing
the handle closes the descriptor and any blocked poll has already returned at
its deadline.

`PollingFileBulkTransport` owns one OUT and one IN endpoint and forwards the
`MassStorageLoop` per-call timeout to those methods, so a `FileBulkTransport`
style export can now satisfy the typed timeout contract against real pipes or
FIFOs. `PollingFileEndpointOpener` does the same for a caller-supplied
`FunctionFsMount`. Tests use real OS pipes/FIFOs (zero-length readiness, empty
timeout, partial write prefix, EOF, a stalled `MassStorageLoop` that later
recovers after a CBW arrives); there is still no live UDC transfer in the
default suite.

#### Bounded media restart supervision

`MediaRestartPolicy` bounds recovery of a failed export:

```rust
use ozma_usb::{MediaRestartPolicy, MediaRestartTracker};

let policy = MediaRestartPolicy::new(3, 250, 4_000) // attempts, first delay, ceiling
    .with_overlay_reload(true);
let mut tracker = MediaRestartTracker::new();
let attempt = tracker.record_failure(&policy).unwrap(); // Some(1, 250 ms)
assert_eq!(attempt.backoff_ms, 250);
tracker.record_success(); // a real CSW written clears the failure streak
assert_eq!(tracker.attempts(), 0);
# Ok::<(), ozma_usb::UsbError>(())
```

The policy is data-only: it sleeps nothing and touches no endpoint. Attempts
are capped (0..=16), backoff is capped exponential, and a cap of `0` disables
restart. When the cap is exhausted the export stays `failed` instead of
retrying forever. The node applies it to bulk-transport errors and to an
observed FunctionFS `bound → unbound` transition; counters and the typed last
error stay visible across a rebuild. If `reload_overlay` is enabled and the
export has a persistence path, the overlay is committed before the rebuild and
reloaded afterwards, so a writable restart resumes from the persisted bytes.

### UAC2 audio functions

`Uac2Config` expands the previous flat channel/rate/depth tuple with multiple
advertised rates/sizes, microphone/headset/speaker modes, and optional
volume/mute:

```rust
use ozma_usb::{Uac2Config, UsbFunction};

let audio = Uac2Config::headset(2, 1, 48_000, 24)
    .with_sample_rates([44_100, 48_000])
    .with_sample_sizes([16, 24])
    .with_volume(true)
    .with_mute(true);
let function = UsbFunction::uac2_with_config("audio", audio);
```

Validation requires at least one direction, 0..=8 channels per direction, a
mode consistent with the enabled channels, rates in 8000..=192000 Hz, sample
sizes of 16/24/32 bits, unique advertised values containing the primary value,
and isochronous packets ≤1024 bytes. `Uac2Config::descriptors()` produces the
Audio Control interface (clock, terminals, optional feature unit) and Audio
Streaming interfaces (format type I with all advertised rates, isochronous
endpoint) with a compact `Uac2DescriptorSummary`.

The kernel's `uac2.<instance>` ConfigFS attributes only accept one primary rate
and sample size per direction. The controller writes:

```text
p_chmask = (1 << playback_channels) - 1     c_chmask = (1 << capture_channels) - 1
p_srate  = sample_rate_hz                   c_srate  = sample_rate_hz
p_ssize  = bit_depth / 8                    c_ssize  = bit_depth / 8
req_number = 2
```

Additional advertised rates/sizes stay in the descriptor/status model and are
not written to ConfigFS. The generated format descriptor carries every
advertised rate at the primary sample size; additional sample sizes are
capability metadata because one UAC2 format-type-I descriptor has a single
subframe size. `FunctionStatus.uac2` reports the mode, both stream models, and
descriptor shape. Descriptor-only status remains `bridge: "unsupported"` unless
a bridge report is attached.

#### UAC2 isochronous bridge core

`Uac2Bridge` is the transport-independent audio core. It never opens a real
endpoint or sound device; the caller injects an isochronous endpoint handle and
an audio path for each direction:

- **Playback** (host → device, USB **OUT** `0x01`): OUT packets are read,
  converted to the sink format, and pushed into a bounded frame-aligned ring.
  The sink drains that ring, with short writes counted as backpressure.
- **Capture** (device → host, USB **IN** `0x82`): source PCM is converted to the
  USB format and packetised into sample-accurate isochronous IN packets.

```rust
use ozma_usb::{
    FakeUac2AudioSink, FakeUac2AudioSource, FakeUac2Endpoint, Uac2Bridge,
    Uac2BridgeConfig, Uac2BridgeInputs, Uac2Config,
};

let uac2 = Uac2Config::headset(2, 1, 48_000, 24);
let config = Uac2BridgeConfig::from_uac2("audio", &uac2)?;
let playback_format = config.playback.unwrap().format;
let capture_format = config.capture.unwrap().format;
let playback_out = FakeUac2Endpoint::out(64)?;
let capture_in = FakeUac2Endpoint::in_endpoint(64)?;
let mut bridge = Uac2Bridge::new(
    config,
    Uac2BridgeInputs {
        playback_endpoint: Some(Box::new(playback_out)),
        playback_sink: Some(Box::new(FakeUac2AudioSink::new(playback_format))),
        capture_endpoint: Some(Box::new(capture_in)),
        capture_source: Some(Box::new(FakeUac2AudioSource::silence(capture_format))),
    },
)?;
bridge.start()?;
bridge.pump()?;
let status = bridge.status("audio", &uac2);
```

Formats are interleaved signed little-endian PCM at 16/24/32 bits, 1–8
channels, and 8–192 kHz. `convert_pcm` scales bit depths, replicates mono,
averages many-to-one downmixes, and zero-fills/truncates other channel-count
changes. **Sample-rate conversion is never implicit**: a rate mismatch is a
typed `uac2-format` error, so there is no hidden resampler.

`Uac2Packetizer` accumulates `sample_rate * interval` over USB microframes, so
44.1 kHz alternates 5- and 6-frame packets and sums to exactly 44 100 frames per
second. Partial capture reads are padded with silence by default
(`Uac2UnderrunPolicy::PadSilence`); `SkipPacket` writes only the available
frames or skips an empty interval. Both modes count short packets, padding, and
underruns.

The bridge reports `configured`, `running`, or `stopped` only from real state:
`running` requires PCM to have moved since the last start/flush, and a
started-but-idle bridge stays `configured`. `Uac2BridgeReport` carries the
per-direction format, endpoint, packet interval, maximum packet size, and
cumulative `Uac2BridgeCounters` (packets/bytes/frames, drops, overflow events,
sink backpressure, short/underrun counts). `Uac2Status::with_bridge_report`
embeds it; `Uac2Status::with_bridge_unavailable` attaches a typed
`bridge: "unsupported"` reason. `Uac2Bridge::new` fails with typed
`uac2-missing-path`, `uac2-format`, `uac2-endpoint`, or `uac2-overflow` errors
when an endpoint/audio path is absent or inconsistent; it never synthesises a
successful fake.

#### Real ALSA/PipeWire adapters and node wiring

`ozma-audio` now implements the bridge's audio traits with bounded,
non-blocking devices:

| Type | Path |
|---|---|
| `AlsaUac2Sink` | host→device USB OUT PCM into an ALSA playback PCM |
| `AlsaUac2Source` | device→host USB IN PCM from an ALSA capture PCM |
| `PipeWireUac2Sink` | host→device PCM into an existing `PipeWireAudioSink` (S16) |
| `PipeWireUac2Source` | device→host PCM from an existing `PipeWireAudioSource` (S16) |
| `Uac2PcmWriter`/`Uac2PcmReader` + factories | injectable byte devices used by default tests |

ALSA PCMs are opened non-blocking and negotiated at exactly the bridge format
(rate, channel count, packed S16/S24_3/S32); a nearest-match is rejected as a
typed `uac2-format` error rather than silently resampled. `EAGAIN` becomes a
zero-byte transfer (backpressure/underrun, never fake progress), and short
writes/reads are returned and counted. `Uac2SinkStats`/`Uac2SourceStats` expose
write/read calls, frames, short transfers, backpressure, underruns, device
errors, and flushes through a cloneable handle. PipeWire streams only carry
interleaved `i16`, so the PipeWire adapters accept S16 formats and reject wider
ones.

`ozma-node --usb-uac2 --usb-uac2-bridge` starts the pump only when the UAC2
ConfigFS function and the gadget ALSA card exist and both host devices open.
The kernel gadget card is exposed as an `Uac2EndpointIo`: its capture PCM
(`hw:N,1`) is USB OUT playback, its playback PCM (`hw:N,0`) is USB IN capture.
A missing function/card/device is a typed startup error and `/usb` keeps
`bridge: "unsupported"`. While running, the UAC2 function carries
`bridge_report` with `configured`/`running`/`stopped`, per-direction endpoint and
format, packet interval/size, and cumulative counters. `running` is only emitted
after real PCM bytes move; a started-but-idle bridge stays `configured`. A
typed `uac2_bridge_error` (stable `kind` plus message) is added when the pump
observes a device error. `--usb-uac2` without `--usb-uac2-bridge` remains
descriptor-only and reports a typed unsupported reason; the legacy
`usb_audio` ALSA loop is not started when the new bridge flag is set.

The node wiring and adapter tests use fake PCM writers/readers, fake USB
endpoints, and a fake device factory; they never open ALSA/PipeWire or a UDC:

```bash
cargo test -p ozma-audio --test uac2_adapters
cargo test -p ozma-node --lib usb_uac2
cargo test -p ozma-node --lib usb_status_reports
```

The only real-device test stays explicitly opt-in:

```bash
OZMA_AUDIO_TEST_DEVICE=default \
  cargo test -p ozma-audio --test uac2_bridge_real -- --ignored
```

## USB/IP

`UsbIpConfig` records whether USB/IP is enabled, the default host/port
(`127.0.0.1:3240`), configured local/remote `UsbIpDevice`s, and optional
`UsbIpAuthorization` metadata (allowed hosts, key fingerprints, and bearer
tokens). Validation rejects empty/malformed hosts, port 0, duplicate host+bus-id
pairs, oversized key/token lists, malformed bus IDs, and option-like values
beginning with `-`.

`UsbIpAuthorization` is an enforceable **allow-list** at the controller layer:
`SubprocessUsbIpController::attach_with_token(host, bus_id, token)` checks the
host and token before building any `usbip attach` argv. A non-empty host list
requires an exact host match; a non-empty token list requires a matching token
(16–256 printable ASCII bytes, compared without an early length-mismatch
return);
an authorization with no hosts and no tokens fails closed. Tokens are redacted
from `Debug` output and never echoed in errors. `attach` (the trait method)
passes no token and therefore fails when a token is required. No command is run
on denial (`usbip-unauthorized`).

The server-side path uses `UsbIpAuthorization::authorize_peer(host, token,
key_fingerprint)` (or the controller's `authorize_peer`/`export_with_token`):
when configured, the peer host must match the allow-list, a token must match in
constant time, and a presented key fingerprint must match one of the configured
`UsbIpKey` fingerprints (also constant-time). A successful check returns a
`UsbIpPeerGrant` that records the host and matched key metadata but never the
raw token or presented fingerprint. `export_with_token` runs the `usbip bind`
argv only after that grant; `export` remains the local-administration path with
no peer check.

Each export/unexport/attach/detach transition is appended to a bounded
`UsbIpHotplugLog` (default and maximum 64 events, drop-oldest, monotonic
sequence numbers) exposed through `hotplug_events`/`take_hotplug_events`. It is
a library model for a future node route/event stream; no kernel hotplug
monitor is started.

USB/IP TLS is **not available in-process**. `UsbIpConfig::tls =
UsbIpTlsPolicy::Required` fails closed with the typed
`usbip-tls-unsupported` error (`UsbIpTlsStatus` reports `available: false` and
the reason) instead of silently running plaintext. This is a deliberate
architecture limit, not a missing rustls call: the optional in-process control
listener is plaintext, the crate does not implement TLS termination, and it
cannot intercept the kernel `vhci_hcd` USB/IP data path. An external `usbipd`,
firewall, TLS terminator, or encrypted tunnel (for example WireGuard/stunnel)
must protect the wire protocol and the control port. No kernel `vhci_hcd`
lifecycle exists in this crate.

### In-process USB/IP daemon state machine

`UsbIpDaemon` is the bounded control-plane daemon that the node can own without
pretending to be `usbipd`. It keeps `stopped`/`listening` state, the local
export list, the remote attachment list, up to
`MAX_USBIP_DAEMON_CONNECTIONS` authorized peer sessions, and the existing
bounded `UsbIpHotplugLog`. It reuses `UsbIpAuthorization::authorize_peer`, so a
host allow-list, bearer token, and (when configured) key fingerprint are all
checked **before** any kernel call. Accepted peers are stored as
`UsbIpPeerGrant` metadata only; raw tokens and presented fingerprints are never
retained, logged, or serialized.

The kernel boundary is an injected `UsbIpKernelBackend`. The explicit default,
`NoUsbIpKernel`, makes `export`, `unexport`, `attach`, and `detach` fail with
`usbip-kernel-required` (mentioning `vhci_hcd`/`usbip-host` and privileges)
rather than reporting a fabricated export or attachment. Tests inject an
in-process fake backend; a real adapter would drive the existing argv-only
`usbip` CLI. A failed detach leaves the recorded attachment in place so cleanup
is never falsely claimed. Operations require `start()`; `stop()` clears tracked
peer sessions but does not fabricate kernel unexport/detach.

The daemon itself is the state/authorization layer: the separate
`usbip_server` module below accepts sockets and serves the control messages.
TLS is exactly the fail-closed `UsbIpTlsPolicy` above: `Required` rejects
construction, and `UsbIpDaemonStatus` reports `tls.available: false` with the
external-terminator requirement. No TLS is faked.

### USB/IP control wire protocol and server

`usbip_protocol` is a typed codec for the version-`0x0111` control plane. It
encodes and decodes the packed, network-byte-order kernel records:

| Message | Layout |
|---|---|
| `OP_REQ_DEVLIST` (`0x8005`) | 8-byte `op_common` |
| `OP_REP_DEVLIST` (`0x0005`) | `op_common` + `ndev: u32` + `ndev` device records |
| `OP_REQ_IMPORT` (`0x8003`) | `op_common` + 32-byte NUL-padded `busid` |
| `OP_REP_IMPORT` (`0x0003`) | `op_common` + 312-byte device record on success, header only on error |

A `UsbIpDeviceDescriptor` carries `path` (256 bytes), `bus_id` (32 bytes),
`busnum`/`devnum`/`speed`, `idVendor`/`idProduct`/`bcdDevice`, the device
class/subclass/protocol triple, configuration value/count, declared
`bNumInterfaces`, and 4-byte interface class/subclass/protocol records. The
codec bounds a frame at `USBIP_MAX_FRAME_BYTES`, devices at 256, interfaces at
32, and requires non-empty printable strings that fit their buffers with a NUL.
Bad version, unknown opcode, unexpected opcode, unknown status, truncation,
trailing bytes, oversized counts/strings/frames, and inconsistent reply bodies
are distinct `UsbIpProtocolError` values surfaced as the stable
`usbip-protocol` kind. Import replies intentionally contain no interface
records (the kernel record does not carry them), so decoding preserves the
declared count with an empty interface list.

`UsbIpDaemon::export_with_descriptor(bus_id, descriptor)` (and its
peer-authorized sibling) registers the descriptor that
`OP_REP_DEVLIST` may serve. An export made with the older `export` method is
listed by `list()` but is **not** advertised on the wire, because the daemon
will not invent a path, bus/device number, speed, or interface list.

`UsbIpServer` is the bounded listener:

```rust
use std::sync::Arc;
use ozma_usb::{
    UsbIpDaemon, UsbIpDaemonConfig, UsbIpServer, UsbIpServerConfig,
    UsbIpDeviceDescriptor,
};

let daemon = Arc::new(
    UsbIpDaemon::new(UsbIpDaemonConfig::server("127.0.0.1", 3240))?,
);
daemon.start()?;
daemon.export_with_descriptor(
    "1-1",
    UsbIpDeviceDescriptor::new("/sys/devices/usb1/1-1", "1-1")
        .with_ids(0x1d6b, 0x0002)
        .with_numbers(1, 2)
        .with_speed(3),
)?;

let server = UsbIpServer::bind_tcp(
    (std::net::Ipv4Addr::LOCALHOST, 0),
    Arc::clone(&daemon),
    UsbIpServerConfig::default(),
)?;
let handle = server.start()?;
handle.stop();
# Ok::<(), ozma_usb::UsbError>(())
```

`serve_session` is the same session loop over any injectable `UsbIpTransport`
(an in-memory duplex in tests, `TcpStream` in production). `UsbIpServerConfig`
bounds concurrent sessions, the largest accepted/produced control frame, and
the idle deadline between frames; blocked reads use a short poll interval so
`UsbIpServerHandle::stop` interrupts them. Sessions call
`UsbIpDaemon::accept_connection`, so the daemon connection ceiling and
host/token/key-fingerprint allow-list apply before any frame is read.

`OP_REQ_IMPORT` first requires an exported, descriptor-registered bus id. It
then calls the same authorized `UsbIpDaemon::attach` path as the CLI
controller. A successful `OP_REP_IMPORT` is written only after **both** the
injected kernel backend returned a real local `vhci` port **and** the injected
`UsbIpKernelDataEndpoint` opened a URB endpoint for that attachment. The reply
then carries the 312-byte device record including the requested bus id, and the
same authenticated connection is handed to `serve_data_session`. An unknown or
unregistered bus id is `usbip-unknown-device` on the wire `ST_NODEV`; a kernel
attach failure (including the default `NoUsbIpKernel`) is
`usbip-kernel-required` on the wire `ST_DEV_ERR`; a missing data adapter (the
default `NoUsbIpKernelDataEndpoint`) is also typed `usbip-kernel-required`. In
every failure case the reply contains no device body, the server attempts to
roll back a local attachment made in the same request, and no URB frame is
produced. Successful export/import transitions still append to the shared
bounded `UsbIpHotplugLog`; a rollback appends the matching detach event.

`serve_session` uses the default no-kernel data seam. `serve_session_with_data`
takes an explicit `UsbIpKernelDataEndpoint` plus a `UsbIpRelayConfig`; the
`UsbIpServer` builder exposes the same injection through
`with_data_endpoint`/`with_relay_config`. The seam is a bounded factory: it is
called at most once per successful import and the session owns exactly one
endpoint. It distinguishes an absent kernel adapter (typed `UsbError`, no
success reply) from a runtime endpoint failure (`UsbIpDataError`, returned as a
negative-status `USBIP_RET_SUBMIT` with zero bytes).

The wire protocol carries no token or key-fingerprint field. `ozma-usb`
therefore takes an injectable `UsbIpCredentialSource`; the embedder supplies
credentials out-of-band (for example a TLS terminator's verified client
certificate fingerprint or a trusted proxy's token). `UsbIpPresentedCredentials`
holds them only for the session, has a redacted `Debug`, is not serializable,
and is never written to a reply, status, or log. The default
`NoUsbIpCredentials` presents nothing, so a configured token/fingerprint
requirement fails closed.

This listener is plaintext. A successful IMPORT records a local `vhci` port only
when the injected kernel backend actually returned one. TLS remains external as
described above.

### Linux kernel adapters

`LinuxUsbIpHostBackend` is the real `UsbIpKernelBackend`. It drives the existing
argv-only `CommandRunner` (never a shell) with these operations:

| Operation | Command | Kernel precondition |
|---|---|---|
| export | `usbip bind -b <busid>` | `module/usbip_host` and `bus/usb/drivers/usbip-host` |
| unexport | `usbip unbind -b <busid>` | `module/usbip_host` |
| attach | `usbip attach -r <host> -b <busid>` | `module/vhci_hcd` and a `vhci_hcd*/attach` attribute |
| detach | `usbip detach -p <port>` | the same vhci attribute |
| status | `usbip port`, `usbip list -l` | the relevant module |

The sysfs root is injectable (`UsbIpKernelSysfs::with_root`); production uses
`/sys`. A missing module, missing platform attribute, or missing `usbip`
executable is the stable `usbip-kernel-required` kind and no command runs. A
non-zero exit remains `command-failed`. Export verifies the kernel created
`bus/usb/drivers/usbip-host/<busid>`; if the binding is absent it runs
`usbip unbind -b <busid>` as rollback and never reports a false export. Attach
parses `usbip: info: using port 00` or falls back to `usbip port`; if neither
reports a port it returns `usbip-kernel-required` instead of inventing one.
Kernel vhci ports are zero-based, so `Port 00` is preserved as port `0` in
attachments and detach calls.

Peer-facing entry points (`export_for_peer`, `attach_authorized`) run the same
constant-time host/token/key-fingerprint allow-list as the daemon before any
sysfs probe or command. Tokens and fingerprints are never retained by the
backend, never placed in argv, and never rendered in errors; the trait
`export`/`attach` methods remain the local-administration path (the daemon has
already authorized the peer before calling them). Tests use a fake sysfs tree
and a scripted runner for bind/unbind/attach/detach, port parsing (including
port zero), missing module/attribute/executable, command failure, authorization
denial, invalid values, and bind rollback. One real-kernel test is `#[ignore]`
and requires `OZMA_USBIP_REAL_BUSID` plus root and loaded modules.

`LinuxUsbIpKernelDataEndpoint` is the real data seam. It validates the remote
host/bus id, enforces any configured authorization, dials the remote USB/IP
server through an injectable `UsbIpUrbConnector` (production
`TcpUsbIpConnector`, default port `3240`), sends the real `OP_REQ_IMPORT`, and
validates the reply with the control codec before returning a
`LinuxUsbIpUrbSession`. That session writes `USBIP_CMD_SUBMIT`/`USBIP_CMD_UNLINK`
frames on one writer lock while a reader thread pairs
`USBIP_RET_SUBMIT`/`USBIP_RET_UNLINK` by sequence number. The downstream devid is
rewritten to the remote `(busnum << 16) | devnum` on the way out and restored on
the completion; unlink targets and reply statuses are translated the same way.

Each submit/unlink has a configurable deadline (default 5 s, max 60 s). A
timeout is a negative `USBIP_RET_SUBMIT` status `-ETIMEDOUT` for that URB only;
a closed stream fails every waiter with `-ECONNRESET`; a refused import, missing
device record, bus-id mismatch, wrong version/opcode, or malformed URB closes
with a typed error and never fabricates bytes. The default
`NoUsbIpKernelDataEndpoint` still fails closed. Tests use a loopback TCP server
speaking the same codec (IN/OUT submit, devid translation, unlink, silent-remote
timeout, refused import, version failure) plus fake connectors for authorization
and availability; the real remote import test is `#[ignore]` and requires
`OZMA_USBIP_REAL_HOST`/`OZMA_USBIP_REAL_BUSID`.

This TCP adapter is a userspace implementation of the imported URB phase; it
does not hand a socket to the kernel `usbip-host` driver, and it does not itself
create a local `vhci_hcd` device. TLS is still not terminated in-process.

### USB/IP URB data relay

`usbip_urb` is the typed codec for the four messages that follow a successful
`OP_REP_IMPORT` on the same connection:

| Message | Raw value | Direction | Body |
|---|---|---|---|
| `USBIP_CMD_SUBMIT` | `0x00000001` | client → server | 48-byte header + OUT payload + ISO descriptors |
| `USBIP_CMD_UNLINK` | `0x00000002` | client → server | 48-byte header with target `seqnum` |
| `USBIP_RET_SUBMIT` | `0x00000003` | server → client | 48-byte header + IN payload + ISO descriptors |
| `USBIP_RET_UNLINK` | `0x00000004` | server → client | 48-byte header with status |

The common 20-byte basic header is `command`, `seqnum`, `devid`, `direction`,
`ep`; the 28-byte union holds the submit flags/length/start-frame/
packet-count/interval/setup, completion status/actual-length/start-frame/
packet-count/error-count, or unlink target/status. All values are network byte
order and every field is encoded at the kernel `usbip_header` offsets. Transfer
lengths reuse the control frame ceiling (`USBIP_URB_MAX_TRANSFER_BYTES`), ISO
descriptors are capped at `USBIP_MAX_ISO_PACKETS`, and each descriptor's
`offset + length` plus `actual_length <= length` is checked. Non-ISO transfers
use the kernel `0xffffffff` sentinel (`0` is also accepted); a submit with the
wrong payload direction, an endpoint above 15, an unknown command, an
unsupported direction, a negative signed length, truncation, trailing bytes, or
an oversized count/length is a distinct `UsbIpUrbError` (kind `usbip-urb`).
`Debug` prints `data_len`, never payload bytes, so URB data cannot leak through
a log line. Kernel-style server replies that write `direction = 0` even for an
IN payload can be decoded with `UsbIpRetSubmit::decode_with_direction`.

`usbip_relay` adds the bounded data path:

* `UsbIpDataEndpoint` is the injectable device boundary. `submit` receives a
  `UsbIpCmdSubmit` and returns a `UsbIpRetSubmit` carrying real device bytes (or
  a typed `UsbIpDataError` status); `unlink` cancels by `devid`/`seqnum`.
* `UsbIpRelay` is the deterministic core. It pairs SUBMIT/RET by `seqnum`,
  records only metadata while in flight, enforces `max_in_flight`, drops a
  completion that arrives after a successful unlink, restores a target when the
  endpoint could not cancel it, and drains every outstanding URB on cleanup.
* `serve_data_session` runs the URB stream over any `UsbIpTransport`. Each
  accepted submit runs on its own bounded worker thread, so completions may be
  written out of order; a submit above the ceiling is answered with
  `USBIP_RET_SUBMIT` status `-EBUSY` and never silently dropped. Endpoint
  errors become negative-status completions with zero bytes. Peer close, stop,
  protocol error, and the idle deadline all unlink outstanding URBs and record
  `cancelled_on_disconnect`.

`UsbIpRelayConfig` bounds in-flight URBs (`1..=256`, default 32), transfer bytes
(default the codec ceiling), ISO descriptors, and an optional idle deadline
(default 30 s). The USB/IP control server now wires this relay directly: after a
successful authorized import and a successful `UsbIpKernelDataEndpoint::open`,
it calls `serve_data_session` on the same authenticated `UsbIpTransport` that
carried `OP_REP_IMPORT`. The session outcome embeds the relay counters in
`UsbIpSessionOutcome::data`.

The relay does **not** invent device data. `UsbIpKernelDataEndpoint` is the
kernel adapter seam (trait plus the `NoUsbIpKernelDataEndpoint` no-op default);
it is separate from `UsbIpDataEndpoint`, which carries the actual submit/unlink
calls. `LinuxUsbIpKernelDataEndpoint` now supplies the real userspace adapter
over an imported USB/IP TCP URB stream (devid translation, unlink, deadlines);
`LinuxUsbIpHostBackend` supplies the real `usbip`/sysfs export/attach path. The
crate still has no in-tree kernel `usbip-host`/`vhci_hcd` module or socket
hand-off: the TCP adapter does not create a local kernel vhci device. The default
seam still fails a successful import with typed `usbip-kernel-required` before
any success body or URB is written, and default tests use in-memory endpoints or
a loopback TCP fake server only.

`SubprocessUsbIpController` implements `UsbIpController` with an injected
`CommandRunner`:

| Method | `usbip` argv |
|---|---|
| `list()` | `list -l` |
| `export(bus_id)` | `bind -b <bus_id>` |
| `export_with_token(bus_id, host, token, key)` | peer check, then `bind -b <bus_id>` |
| `unexport(bus_id)` | `unbind -b <bus_id>` |
| `attach(host, bus_id)` | `attach -r <host> -b <bus_id>` |
| `detach(port)` | `detach -p <port>` |
| `status()` | `port` (parsed into `UsbIpAttachment`s) |

Every argument is passed as a separate argv element; there is no shell,
interpolation, or `sudo`. `status()` on a disabled configuration returns a
typed disabled `UsbIpState` without running a command; an enabled status whose
`usbip port` call fails propagates `MissingCommand`/`CommandFailed` rather than
fabricating devices. The legacy `SubprocessUsbController`/`UsbIpControl` API
remains available for the existing node wiring.

## FAT32, ISO, and NBD

- **FAT32 builder** (`fat32`): `Fat32ImageBuilder` accepts in-memory
  `Fat32Node`s, `add_file`/`add_directory`, `add_file_from_path` (bounded read),
  and `add_directory_tree` (skips dotfiles and symlinks). It emits a complete
  byte image with a valid FAT32 BPB, FSInfo sectors, two FATs, root directory,
  LFN/8.3 entries, file clusters, and a backup boot sector. Defaults bound
  files (64 MiB), total data (128 MiB), file count (4096), name length (255),
  and depth (32); images use at least the FAT32 minimum cluster count. Files
  carry the read-only attribute, and `Fat32Image::is_read_only()` is always
  true. `parse_fat32_image`/`parse_fat32_boot_sector` validate an existing
  image without mounting it. Cluster sizes 512–65536 bytes are supported;
  the default is one 512-byte sector per cluster for compact test images.
- **Write overlay** (`overlay`): `WriteOverlay` layers a bounded copy-on-write
  block cache over an immutable FAT32 or raw base. The first write to a block
  copies that base block and allocates one block (the final block may be
  shorter); later writes to the same block are free. Writes are preflighted
  against `max_overlay_bytes` and the base length, so a rejected write does not
  partially allocate, and a read falls back to the base when no overlay block
  exists. `from_fat32`/`from_bytes`/`from_file` accept a base; `read_at`,
  `read_sectors`, `write_at`, and `write_sectors` operate on raw bytes;
  `materialize` returns the merged image; `commit_to` writes a checksummed
  persistence file atomically via a sibling temp file and rename, and
  `load_from`/`reload` validate it before replacing blocks. The persistence
  header records magic `OZOV`, version, block size, base length, block count,
  an FNV-1a header checksum, and a base-image checksum; each block has its own
  checksum. Bad magic,
  version, header/block checksum, base-length/checksum mismatch, block-size
  mismatch, duplicate or out-of-range blocks, truncation, and trailing bytes
  are typed `overlay-corrupt`; allocation failures are `overlay-bounds`. The base is
  never written, mounted, or opened as a block device. Writable NBD exports are
  backed by this overlay (`NbdOverlayBackend`): WRITE copies the touched base
  block, READ prefers overlay blocks, FLUSH/stop commit the checksummed
  persistence file according to `NbdOverlayPolicy`, and TRIM is a bounded
  no-op.
- **SCSI mass storage** (`mass_storage`): `MassStorageDevice` decodes USB Mass
  Storage BBB CBWs (`ScsiCommandBlock`) and serves `TEST UNIT READY`,
  `REQUEST SENSE`, `INQUIRY`, `MODE SENSE(6/10)`, `START STOP UNIT`,
  `PREVENT ALLOW MEDIUM REMOVAL`, `READ CAPACITY(10)`, `READ FORMAT
  CAPACITIES`, `READ(10)`, `WRITE(10)`, and `SYNCHRONIZE CACHE(10)`. Read-only
  media (raw/FAT32/ISO/CD-ROM) is served directly and rejects writes with
  `read-only-media` or a SCSI `DATA PROTECT / WRITE PROTECTED` sense; the
  FAT32/ISO builders are unchanged. Writable media goes through the same
  bounded `WriteOverlay` as NBD: reads prefer overlay blocks, writes copy the
  touched base block, the base is immutable, and out-of-range/allocation
  failures are typed. `SYNCHRONIZE CACHE(10)` calls `flush`, and `stop()`
  commits the checksummed persistence file according to
  `MassStoragePersistPolicy`; `load_writable_file` reloads it. Requests are
  bounded to 1 MiB (2048 sectors), and `ScsiReply` returns the IN data,
  residue, CSW status, and sense. `MassStorageLoop` drives the same device over
  injectable bulk endpoints (`FunctionFsBulkTransport`, `MemoryBulkTransport`),
  assembles short transfers, enforces a configured data-phase bound and typed
  endpoint timeouts, and only reports `running` after a CSW was written. No
  block device, mount, or real USB transfer is performed by these modules.
- **ISO** (`iso`): `IsoImage::inspect` reads at most 32 volume descriptors from
  an existing file, verifies `CD001`, the primary descriptor, both-endian
  size/block fields, and root-directory bounds. `MediaFormat::Iso9660` always
  means CD-ROM + read-only. There is no ISO writing, El Torito parsing,
  directory-tree extraction, or mounting.
- **NBD** (`nbd` + `nbd_server`): `NbdExport` validates name/size/block
  size/read-only flags; `NbdExportDescriptor` models TCP or Unix-socket
  endpoints; `NbdHandshake`, `NbdRequestHeader`, `NbdRequest`, and `NbdReply`
  encode/decode bounded big-endian framing with request-size limits.
  `NbdServer` speaks fixed-newstyle NBD over TCP or Unix sockets: it advertises
  `NBD_FLAG_FIXED_NEWSTYLE|NBD_FLAG_NO_ZEROES`, handles `EXPORT_NAME`, `LIST`,
  `INFO`, `GO`, `ABORT`, and unknown options (typed `ERR_UNSUP`), lists exports,
  and serves `READ`/`WRITE`/`FLUSH`/`TRIM`/`DISC` with per-request and
  per-connection bounds. `NbdMemoryImage`/`NbdFileImage` back exports from
  memory or an existing regular file; `NbdExportSource::from_media` probes
  ISO/FAT32 and forces read-only, and serves a writable raw image through a
  copy-on-write `NbdOverlayBackend` (base file immutable). `NbdOverlayStatus`
  exposes block size, base length, allocated blocks/bytes, the ceiling, dirty
  state, persistence path, and FLUSH/stop policy. `NbdServerConfig` bounds
  request size, clients, handshake/idle deadlines, and option payloads;
  `NbdServerHandle::stop` wakes idle sessions, joins them, and persists
  configured overlays (failures increment `persist_failures`). The server never
  opens a block device, and it does not advertise write-zeroes.

### Live NBD/QEMU evidence

`tests/nbd_qemu.rs` builds a FAT32 image from a temporary directory and an ISO
from synthetic bytes, starts the real server on `127.0.0.1:0`, and lets QEMU
negotiate:

```text
qemu-img FAT32 NBD info:
{"children":[{"info":{"children":[],"filename":"nbd://127.0.0.1:41801/fat32",
 "format":"nbd","virtual-size":34089472},"name":"file"}],
 "filename":"nbd://127.0.0.1:41801/fat32","format":"raw","virtual-size":34089472}

qemu-img ISO NBD info:
{"filename":"nbd://127.0.0.1:36899/iso","format":"raw","virtual-size":36864}

qemu-io -r -c "read -v 0 64" nbd://...: read 64/64 bytes
qemu-io -c "write -P 0x11 0 64" nbd://...:
qemu-io: can't open device ...: Could not open image: Permission denied
```

A writable raw export is also validated live with `qemu-io` against the
copy-on-write overlay backend (`tests/nbd_qemu.rs`):

```text
qemu-io -c "write -P 0x5a 0 4096" -c "flush" -c "read -P 0x5a 0 4096" nbd://...
wrote 4096/4096 bytes at offset 0
read 4096/4096 bytes at offset 0
```

The test then confirms the base file is byte-for-byte unchanged, the overlay
read-back matches, FLUSH wrote the checksummed persistence file, and a fresh
`NbdOverlayBackend` reloads the QEMU-written block.

The optional QMP test (`OZMA_NBD_QMP_TEST=1 ... --ignored`) attaches the same
export to a disposable `qemu-system-x86_64` as a USB storage device and reports
via `query-block`:

```text
"device": "nbd0", ... "inserted": {..., "virtual-size": 34089472,
"filename": "nbd://127.0.0.1:38703/fat32", "format": "raw", "ro": true, ...},
"qdev": "/machine/peripheral/usbdisk/usbdisk.0/legacy[0]"
```

`tests/nbd_boot.rs` (opt-in `OZMA_NBD_BOOT_TEST=1`) also proves a real boot
path rather than only attachment. It builds a synthetic ISO 9660 image with an
El Torito no-emulation boot catalog and a tiny serial boot sector, serves the
ISO **read-only** over NBD, and boots a disposable `qemu-system-x86_64` with
`-drive ... file=nbd://.../boot-iso,readonly=on,media=cdrom` plus
`ide-cd,bootindex=1`:

```text
guest serial boot log: "OZMA-NBD-BOOT\r\n"
QMP query-block: {"return":[{"device":"cd0", ..., "inserted":{...
  "virtual-size":51200,"filename":"nbd://127.0.0.1:41541/boot-iso",
  "format":"raw","ro":true,...}}]}
```

The test captures the guest serial line and QMP block/device evidence, quits
QEMU over QMP (killing it only on timeout), and stops the NBD server. It skips
without claiming a boot when `qemu-system-x86_64` is absent. No test mounts a
filesystem, opens a block device, writes an image, or changes an existing VM.

## Tests

Default tests are CI-safe and use a temporary ConfigFS-shaped root, a fake
controller, recording/scripted command runners, a fake sysfs tree, synthetic
ISO/FAT32/NBD bytes, in-memory images/overlays, a fake FunctionFS
mounter/clock, real OS pipes/FIFOs, and in-memory transports. They never create
a real gadget, bind a real UDC, load a USB/IP module, export a real device, mount
a filesystem, or write a block device. The NBD listener tests bind ephemeral
loopback TCP/Unix sockets; the USB/IP control tests use an in-memory duplex plus
ephemeral loopback TCP listeners; the QEMU client tests invoke
`qemu-img`/`qemu-io` only when present, and the boot test is opt-in.

```bash
cargo test -p ozma-usb
cargo check -p ozma-usb --all-targets
cargo fmt -p ozma-usb
```

Relevant suites:

| Test | Coverage |
|---|---|
| `tests/fat32_image.rs` | BPB/FAT/root entries/file clusters, LFN + 8.3 collisions, read-only attributes, directory import, bounds, corrupt BPB |
| `tests/media_formats.rs` | ISO PVD/read-only media, `MediaFormat` consistency/JSON, FAT32 attach probe, NBD descriptors and framing |
| `tests/functionfs.rs` | FunctionFS descriptor generation, endpoint/string/daemon bounds, explicit readiness states, ConfigFS `ffs.<instance>` creation/status, fake status |
| `tests/functionfs_daemon.rs` | v2 descriptor/string encoding and alignment, class/OS descriptors, temporary-mount `ep0` writes, standard control requests, enable/disable/live status, endpoint read/write/loopback, malformed/oversized inputs, lazily opened file endpoints |
| `tests/functionfs_polling.rs` | real OS pipes/FIFOs: immediate zero-length read, empty-pipe deadline, data after readiness, EOF without invented bytes, partial-write prefix before timeout, invalid deadlines, a stalled `MassStorageLoop` that recovers after a CBW |
| `tests/functionfs_lifecycle.rs` | function/link creation, mountpoint validation, fake-mounter mount/unmount, `ep0` descriptor write, readiness success/timeout, bind failure rollback, unbind/cleanup, caller-supplied mount preservation, missing mount tool, config validation |
| `tests/functionfs_state.rs` | typed `FunctionFsStateMachine` ordering, out-of-order typed errors, explicit UDC bind/unbind readiness, daemon status never `running` before endpoint traffic, bind/unbind events, lifecycle helper readiness gate, ignored real-UDC run |
| `tests/mass_storage_scsi.rs` | SCSI CBW parse/dispatch, write/read-back through the overlay, base immutability, FLUSH/stop persistence + reload, read-only media and SCSI `DATA PROTECT`, typed bounds, malformed/phase errors, overlay exhaustion without partial allocation, read-only FAT32 image byte identity |
| `tests/mass_storage_loop.rs` | CBW→data→CSW round trips over fake endpoints, write/read/flush, read-only rejection with sense, overlay persistence/reload, transfer bounds, short-transfer assembly, typed stall/I/O failures, stop cleanup, owned file endpoints, FunctionFS loopback endpoint adapter |
| `tests/usbip_daemon.rs` | authorized/denied attach never reaches the kernel on denial, token/fingerprint enforcement, bounded connections, start/stop, export/detach/unexport cleanup, bounded hotplug log, typed `usbip-kernel-required`, fail-closed TLS, no secrets in status/Debug |
| `tests/usbip_kernel.rs` | fake sysfs tree + scripted runner: availability/module checks, exact bind/unbind/attach/detach argv, attach port parsing (stdout and `usbip port`, including port zero), no invented port, missing module/attribute/executable, command failure, peer authorization before commands, invalid values, bind rollback; ignored real-kernel run |
| `tests/usbip_tcp.rs` | loopback TCP fake USB/IP server: import reply validation, OUT/IN submit with devid translation, unlink forwarding, silent-remote deadline, refused import, version failure, authorization before any connect, unavailable connector; ignored real remote import |
| `tests/usbip_protocol.rs` | version-`0x0111` header/opcode/status golden bytes, device-record network byte order at kernel offsets, DEVLIST/IMPORT round trips, fixed-width strings, truncated/trailing/oversized/unknown-version/opcode errors, import-reply body rules |
| `tests/usbip_server.rs` | in-memory duplex DEVLIST empty/populated, descriptor-only advertising, authorized/denied IMPORT, kernel-required and unknown-device error replies with no device body, same-connection post-import URB relay (one/many submits, unlink, disconnect cleanup, endpoint error), default no-kernel data seam typed with no success body, authorization before control and data phases, version/opcode/truncation/frame-ceiling errors, idle timeout, concurrent connection ceiling, hotplug events, loopback TCP control + URB exchange |
| `tests/usbip_urb.rs` | 48-byte URB header offsets and network byte order for submit/completion/unlink, OUT/IN payload rules, kernel `number_of_packets` sentinel, ISO descriptor round trips and offset/length/count bounds, truncation/trailing/unknown-command/direction/endpoint/oversize errors, bounded decode, payload-redacted `Debug`, typed `usbip-urb` kind |
| `tests/usbip_relay.rs` | one/many in-flight URBs, out-of-order completions, unlink before and after completion, endpoint errors without invented bytes, disconnect cleanup, in-flight ceiling `-EBUSY`, idle timeout cleanup, no payload in relay outcome `Debug` |
| `tests/fat32_overlay.rs` | FAT32/raw writes and reads, base immutability, per-block copy-on-write, allocation ceiling and preflight atomicity, commit/reload, checksummed persistence corruption, discard/materialise |
| `tests/uac2.rs` | UAC2 descriptor/channel/rate/size consistency, microphone/speaker/headset modes, invalid rates/channels/modes, ConfigFS attribute writes, descriptor-only unsupported bridge status |
| `tests/uac2_bridge.rs` | sample-accurate 44.1/48 kHz packetization, 16/24/32-bit and mono/stereo conversion, no implicit resampling, playback ring drops/backpressure, partial/padded capture packets, flush/stop/restart, typed missing path/endpoint/overflow errors, status never live without data |
| `ozma-audio/tests/uac2_adapters.rs` | ALSA-adapter round trip through the real bridge core, sink backpressure/drop counters, typed device errors, partial capture reads/padding, stop/flush, no live claim without PCM; in-memory fake PCM devices only |
| `ozma-node` `usb_uac2`/`usb_status_*` | fake device factory configured/running/stopped/error states, typed start failures, `/usb` bridge overlay, endpoint/format/counters, no `running` without PCM counters |
| `tests/usbip_controller.rs` | exact argv, list/port parsing (IPv4/IPv6), attach/detach state, disabled/missing/non-zero typed errors, config validation, host/token allow-list enforcement, no real `usbip` |
| `tests/usbip_security.rs` | server-side peer host/token/key-fingerprint pairing, constant-time token/fingerprint comparison, no secrets in errors/Debug, TLS-required fail-closed policy, bounded ordered hotplug events, JSON metadata, no real `usbip` |
| `tests/nbd_server.rs` | newstyle handshake, export name/list/info/go, read/write/flush/disc, request bounds, read-only write rejection, malformed/truncated frames, injected transport/clock deadline, concurrent loopback clients, Unix socket, server stop, file/media backends |
| `tests/nbd_overlay.rs` | copy-on-write writable NBD writes/read-back, trim no-op, base immutability, typed bounds/allocation ceiling, FLUSH and stop persistence, reload through a new backend, policy-controlled commits, ENOSPC through the wire protocol |
| `tests/nbd_qemu.rs` | live `qemu-img info` on generated FAT32 + read-only ISO, `qemu-io` read and read-only write refusal, live `qemu-io` write+flush+read on a writable overlay export with persistence reload; opt-in QMP USB attachment |
| `tests/nbd_boot.rs` | opt-in disposable QEMU boot from a synthetic El Torito ISO served read-only over NBD; guest serial boot marker plus QMP `query-block` evidence |

The one real-ConfigFS round trip is ignored and env-gated:

```bash
OZMA_USB_CONFIGFS_TEST=1 OZMA_USB_UDC=dummy_udc.0 \
  cargo test -p ozma-usb --test real_configfs -- --ignored
```

It still requires root, a mounted ConfigFS root (optionally
`OZMA_USB_CONFIGFS_ROOT`), and a free UDC. It creates only HID functions and
removes the test gadget.

The real FunctionFS descriptor write is also ignored and env-gated:

```bash
OZMA_FUNCTIONFS_MOUNT=/dev/ffs-ozma-test \
  cargo test -p ozma-usb --test functionfs_daemon -- --ignored
```

It requires an existing FunctionFS mount (made by the caller; no mount or sudo
is performed), opens `ep0`, writes descriptors/strings, and asserts that the
status is not `live` without an observed enable event. A second ignored test,
`real_udc_binds_after_readiness_and_unbinds`, requires
`OZMA_FUNCTIONFS_UDC`, `OZMA_FUNCTIONFS_MOUNT`,
`OZMA_FUNCTIONFS_DAEMON`, and `OZMA_FUNCTIONFS_READY_MARKER` plus a real
ConfigFS root; it binds and unbinds a UDC but still never claims `running`
because the ConfigFS helper opens no data endpoints. A third ignored test in
`functionfs_polling` opens a caller-supplied `epN` through
`PollingFileEndpointIo` when `OZMA_FUNCTIONFS_MOUNT` is set. The lifecycle
helper's default tests use a temp root, a fake mounter, and a fake UDC name;
they never mount or bind.

The real USB/IP kernel adapters are opt-in and never run by default:

```bash
OZMA_USBIP_REAL_BUSID=1-1 cargo test -p ozma-usb --test usbip_kernel -- --ignored
OZMA_USBIP_REAL_HOST=192.0.2.10 OZMA_USBIP_REAL_BUSID=1-1 \
  cargo test -p ozma-usb --test usbip_tcp -- --ignored
```

The first requires root, a real `/sys`, and loaded `vhci-hcd`/`usbip-host`
modules; it binds and unbinds the named local device. The second dials a real
remote USB/IP export, performs `OP_REQ_IMPORT`, and sends one IN URB. Neither
loads modules, uses sudo, or writes a real USB device.

The real ALSA → UAC2 capture-packetiser test is opt-in and lives in
`ozma-audio`:

```bash
OZMA_AUDIO_TEST_DEVICE=default \
  cargo test -p ozma-audio --test uac2_bridge_real -- --ignored
```

It opens a real ALSA capture device, captures one 20 ms 48 kHz stereo frame,
and packetises it through the same `Uac2Bridge` core with an in-memory USB IN
endpoint. The default UAC2 tests need no device and use the in-memory fakes.

The live NBD boot test is also opt-in:

```bash
OZMA_NBD_BOOT_TEST=1 cargo test -p ozma-usb --test nbd_boot -- --ignored
```

It requires `qemu-system-x86_64` and skips otherwise. It never touches an
existing VM; the disposable QEMU is quit over QMP or killed on timeout.

## Remaining integration and security gaps

- Real gadget validation on `dummy_hcd` and hardware UDCs; currently only the
  ignored round trip exists, and node tests use a temp root/fake controller.
- USB/IP now has a typed version-`0x0111` control codec, a bounded plaintext
  control listener (`UsbIpServer`), the in-process daemon (`UsbIpDaemon`) with
  start/stop, export/list/attach/detach/unexport, peer connections, peer
  authorization, and a bounded hotplug log, plus a typed URB codec and bounded
  data relay (`usbip_urb`/`usbip_relay`) with injectable endpoints. The control
  server hands a successful authorized import to that relay on the same
  connection through the `UsbIpKernelDataEndpoint` seam.
  `LinuxUsbIpHostBackend` now drives the real `usbip`/sysfs bind/unbind/attach/
  detach path with module checks, binding verification/rollback, and zero-based
  port parsing. `LinuxUsbIpKernelDataEndpoint` speaks the real imported TCP URB
  phase with devid translation, unlink, deadlines, and typed failures. What is
  still missing is kernel integration beyond the userspace `usbip` CLI and TCP
  protocol: the data adapter does not hand a socket to `usbip-host`, does not
  create a local `vhci_hcd` device through usbfs, and there is no in-tree kernel
  module, `vhci_hcd` stub, or kernel hotplug monitor. `OP_REQ_IMPORT` reaches a
  successful reply only through an injected `UsbIpKernelBackend` that returns a
  real local `vhci` port **and** an injected data seam that opens a real
  endpoint; the defaults `NoUsbIpKernel`/`NoUsbIpKernelDataEndpoint` (and every
  fake used in tests) still return typed `usbip-kernel-required` instead of
  faking a device or payload. ISO descriptor arrays are bounded and round-trip,
  but ISO wire-padding compaction into a real URB buffer is left to the endpoint
  adapter. The optional listener is plaintext and has no in-process TLS; an
  external TLS terminator/tunnel must protect it, and `Required` TLS fails
  closed. A node route exposing the daemon/server/relay and the client-side
  import orchestration remain open.
- The FAT32 builder is a static snapshot, not the Python live `FATSynthesiser`
  (no sector-on-demand reads, inotify rescan, or long-name rescan stability).
  Generated images are read-only. `WriteOverlay` is wired into writable NBD
  exports and the SCSI mass-storage data path with bounded copy-on-write blocks
  and persistence, but it has no FAT metadata/consistency repair, directory
  coalescing, compaction, or write-back to a base image. `ozma-usb` still never
  writes a generated image to disk or a block device.
- ISO support validates descriptors only; no Joliet/Rock Ridge/El Torito
  parsing, directory extraction, writing, or mounting.
- NBD is a bounded userspace server, not a kernel block device: no structured
  replies, TLS, write-zeroes, cache, multi-conn, or persistent export state
  beyond an optional overlay persistence file. The live boot evidence covers a
  read-only ISO over a disposable QEMU; it is not a hardware boot matrix and
  does not exercise installers, UEFI, or a real target. Node wiring is one
  loopback export behind the existing `--usb-gadget` state and is not exposed
  through capabilities or registration.
- FunctionFS now has an `ep0`/endpoint daemon core, a typed
  `FunctionFsStateMachine` (descriptors → ep0 → enabled → running → disabled →
  unbound, with explicit UDC readiness), and an explicit
  `FunctionFsLifecycle` helper (ConfigFS function/link creation, mountpoint
  expectation, descriptor write, marker readiness/timeout, bind rollback,
  unbind/cleanup), but mounting in the default path still requires an injected
  mounter and caller privileges. The command mounter is untested against a real
  FunctionFS mount/UDC in the default suite; there is no persistent daemon
  process, real UDC event loop, or vendor protocol. The SCSI `MassStorageLoop`
  and node `/usb/media*` routes exist, and `--usb-media-export` auto-starts a
  configured export, but a live FunctionFS transport is still attached only by
  explicit code; the default node path starts with `IdleBulkTransport` and never
  opens a UDC endpoint. The original `FileBulkTransport` remains blocking, but
  `PollingFileBulkTransport`/`PollingFileEndpointIo` now enforce the loop deadline
  with `poll(2)` and are covered by real pipe/FIFO tests; there is still no live
  UDC transfer in the default suite, and the node does not select the polling
  transport automatically. `running` is deliberately only reachable after a
  real command cycle; a mount, marker, UDC bind, or enable event never suffices.
  Restart supervision is bounded by `MediaRestartPolicy` and exercised through
  fake transports; there is no background endpoint driver, so a production
  transport error/rebind is only retried when the node observes it again.
  Class/vendor control requests are rejected with typed errors until a protocol
  handler exists. Suspend and resume events are accepted but not acted on.
  UAC2 models descriptors/capabilities, writes
  the primary ConfigFS attributes, and now includes a testable isochronous
  bridge core (`Uac2Bridge`) with injectable endpoints and audio paths. It does
  **not** yet service a real UDC/FunctionFS isochronous endpoint, open ALSA or
  PipeWire, or wire the core to `dummy_hcd`/hardware; descriptor-only status
  still reports `bridge: "unsupported"`, and no node path attaches a live bridge
  report. Node flags exist (`--usb-uac2`, `--usb-vendor`,
  `--usb-vendor-daemon`), but `/usb` surfaces readiness/absence rather than
  starting either backend; the lifecycle status is library-only for now.
- Node media export routes (`GET/POST /usb/media`, `/usb/hotplug`) are backed by
  the userspace SCSI/overlay path and are independent of the ConfigFS
  mass-storage LUN. `--usb-media-export` auto-starts that path at node startup;
  it still does not drive a real FunctionFS endpoint, and hotplug records only
  transitions the node caused or observed (no SSE stream or kernel hotplug
  monitor). Shutdown stops the export and applies the overlay persistence
  policy.
- Node-side gadget teardown/recovery across restart, and hardware-profile
  validation. Mounting, block-device I/O, and real USB/IP transport remain
  deliberately outside this crate.
