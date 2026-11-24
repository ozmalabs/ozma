# ozma-usb

Safe USB gadget, virtual-media, and USB/IP control foundations for Ozma nodes.

This crate is the model and controller layer of the Python
`softnode/virtual_usb.py`, `softnode/usb_mass_storage.py`, and
`softnode/virtual_media.py` paths. It builds FAT32 images in memory, adds a
bounded copy-on-write write overlay, models ISO/NBD media, includes a bounded
fixed-newstyle NBD server backed by a regular file or in-memory image, and
sequences FunctionFS from ConfigFS function creation through descriptor writes,
readiness, and UDC bind/unbind. Default paths do not mount filesystems, open
`/dev/hidg*`, or write a block device; an explicit `CommandFunctionFsMounter`
can mount FunctionFS when the caller opts in and supplies the required
privileges. `ozma-node` wires the gadget and NBD layers behind explicit USB
flags (see below); there is still no automatic gadget creation on a hardware
profile.

## What is implemented

| Area | API |
|---|---|
| Composite gadget model | `UsbGadgetConfig`, `UsbFunction`, `UsbFunctionKind`, `HidProtocol` |
| Functions | HID keyboard/mouse/custom, mass storage, UAC2 (`Uac2Config`), FunctionFS vendor (`FunctionFsConfig`) |
| FunctionFS model | `FunctionFsConfig`, `FunctionFsEndpoint`, `FunctionFsStrings`, `FunctionFsDaemonSpec`, `FunctionFsReadiness`, `FunctionFsStatus`, `FunctionFsDaemon`/`MissingFunctionFsDaemon`/`ObservedFunctionFsDaemon`, generated device/config/string descriptors, class-specific descriptors, Microsoft OS descriptors |
| FunctionFS daemon core | `FunctionFsMount`, `FunctionFsDescriptors` (v2 `ep0` blob + strings), `FunctionFsDaemonCore`, `LoopbackFunctionFsDaemon`/`FakeFunctionFsDaemon`, `FileFunctionFsDaemon`, `FunctionFsControlRequest`/`FunctionFsControlResponse`, `FunctionFsEvent`, injectable `FunctionFsEp0Io`/`FunctionFsEndpointIo`, bounded endpoint loops |
| FunctionFS lifecycle | `FunctionFsLifecycleConfig`, `FunctionFsLifecycle`, `FunctionFsLifecyclePhase`/`Status`, injectable `FunctionFsMounter` with `CommandFunctionFsMounter`/`FakeFunctionFsMounter`, deterministic `FunctionFsLifecycleClock`; ConfigFS function/link creation, mountpoint expectation, `ep0` descriptor write, readiness timeout, bind rollback, unbind/cleanup |
| FunctionFS control | Standard `GET_DESCRIPTOR` (device/configuration/string), `SET_CONFIGURATION`, `GET_CONFIGURATION`, `GET/SET_INTERFACE`, `GET_STATUS`, feature requests; typed rejection for class/vendor and unknown requests |
| FunctionFS status | `FunctionFsDaemonStatus` nested in `FunctionFsStatus.daemon`: `live` only when the descriptor blob, string table, and enable event all succeeded |
| UAC2 model | `Uac2Config`, `Uac2Mode::{Microphone,Headset,Speaker}`, `Uac2DescriptorSet`, `Uac2Status`, `Uac2BridgeStatus::Unsupported` |
| Validation | Safe gadget/function names, VID/PID, strings, power, duplicate links, HID report shape, UAC2 rates/sizes/channels/modes, FunctionFS endpoint bounds/descriptor size, CD-ROM read-only |
| Virtual media | `MassStorageImage`, `VirtualMedia`, `MediaMode::{Disk,CdRom}`, `MediaFormat::{Raw,Iso9660,Fat32,Nbd}`, `MediaState::{Attached,Detached}` |
| Controller trait | `UsbGadgetController` (`create`, `bind`, `unbind`, `status`, `list`, `attach_media`, `detach_media`, `media_state`, `capabilities`) |
| Linux ConfigFS | `ConfigFsUsbController`, injectable root, default `/sys/kernel/config/usb_gadget` |
| Test/dry-run fake | `FakeUsbGadgetController` |
| USB/IP model | `UsbIpConfig`, `UsbIpDevice`, `UsbIpState`, `UsbIpAttachment`, `UsbIpAuthorization`/`UsbIpKey`/`UsbIpToken` (host + token allow-lists) |
| USB/IP control | `UsbIpController`, `SubprocessUsbIpController` (`attach_with_token`), argv-only `CommandRunner`; legacy `UsbIpControl`/`SubprocessUsbController` |
| FAT32 builder | `Fat32ImageBuilder`, `Fat32Node`, `build_fat32_image`, `fat32_image_from_directory`, `parse_fat32_image`, `Fat32Info`/`Fat32Layout` |
| ISO media | `IsoImage::inspect`, `parse_iso_volume_descriptors`, read-only `MediaFormat::Iso9660` |
| NBD model | `NbdExport`, `NbdExportDescriptor`, `NbdHandshake`, `NbdRequest`/`NbdReply` framing, newstyle option constants |
| NBD server | `NbdServer`/`NbdServerHandle` over TCP or Unix sockets, `NbdExportCatalog`, `NbdExportSource`, `NbdMemoryImage`, `NbdFileImage`, injected `NbdTransport`/`NbdClock`, bounded read/write/flush/trim/disc, read-only exports |
| Write overlay | `WriteOverlay`, `OverlayConfig`, `OverlayWriteReport`; bounded copy-on-write over FAT32/raw base bytes, per-block allocation accounting, checksummed atomic persistence, typed `overlay-bounds`/`overlay-corrupt` failures |
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
  --usb-uac2 --usb-vendor --usb-vendor-daemon /usr/local/bin/ozma-ffs
```

Equivalent environment variables are `OZMA_USB_GADGET`, `OZMA_USB_CONFIGFS_ROOT`,
`OZMA_USB_UDC`, `OZMA_USB_MEDIA`, `OZMA_USB_MEDIA_RO`,
`OZMA_USB_MEDIA_CDROM`, `OZMA_USB_UAC2`, `OZMA_USB_VENDOR`, and
`OZMA_USB_VENDOR_DAEMON`. The node builds a HID keyboard + mouse gadget,
optionally a mass-storage LUN, and opt-in UAC2/FunctionFS vendor functions, then
optionally a mass-storage LUN, then creates and binds it only when a UDC is
supplied and the root exists. A missing UDC/root/permission/media or a bind
failure is logged with its typed `kind()`; the node keeps running, advertises no
`usb` capability, and `GET /usb` answers a typed 503 rather than fabricating a
gadget. Without `--usb-gadget`, `/usb` stays a typed 501 and no ConfigFS write
occurs.

When bound, `ozma-node` advertises `usb` in capabilities, TXT/registration, and
`/api/v1/connection.usb`, and exposes:

| Route | Behaviour |
|---|---|
| `GET /usb` | Bearer-guarded read-only gadget status, function list, capabilities, LUN media state, UAC2 descriptor/bridge status, and FunctionFS daemon readiness (no backing paths). |
| `POST /usb/media/attach` | Attach an existing absolute regular file to a mass-storage instance; enforces read-only/CD-ROM. |
| `POST /usb/media/detach` | Clear the LUN `file` attribute; the function/gadget stay in place. |
| `GET /usb/nbd` | Running loopback NBD export summary and counters (`running`, URL, export, size, read-only, stats). |
| `POST /usb/nbd/start` | Serve one existing absolute FAT32/ISO/raw file over loopback NBD (`path`, optional `export`, `format`, `read_only`). ISO/FAT32 are forced read-only; raw defaults to read-only; a second start is a typed 409. |
| `POST /usb/nbd/stop` | Stop the opt-in NBD listener and return its final counters. |

Attach/detach write only ConfigFS `file`/`ro`/`cdrom` attributes. The NBD
routes only bind a loopback TCP socket and read the backing file; they do not
mount, create, truncate, or write the backing image unless a raw export was
explicitly started with `read_only: false` (writes still go to that regular
file, never a block device). Like the rest of the USB router, the NBD routes
answer a typed 501 unless the node was started with `--usb-gadget`.

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
the phase, bound/mounted/ep0/string flags, paths, and the nested
`FunctionFsStatus`.

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
subframe size. `FunctionStatus.uac2` reports the mode, both stream models,
descriptor shape, and `bridge: "unsupported"`: no ALSA device is opened, no
isochronous data is moved, and `Uac2Config::require_bridge()` returns the typed
`unsupported-uac2-bridge` error.

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
(16–256 printable ASCII bytes, constant-time comparison for equal lengths);
an authorization with no hosts and no tokens fails closed. Tokens are redacted
from `Debug` output and never echoed in errors. `attach` (the trait method)
passes no token and therefore fails when a token is required. No command is run
on denial (`usbip-unauthorized`). This is still **not TLS, key exchange, or
cryptographic client authentication**: an external `usbipd`, firewall, or TLS
terminator must protect the USB/IP wire protocol. No kernel `vhci_hcd`
lifecycle exists in this crate.

`SubprocessUsbIpController` implements `UsbIpController` with an injected
`CommandRunner`:

| Method | `usbip` argv |
|---|---|
| `list()` | `list -l` |
| `export(bus_id)` | `bind -b <bus_id>` |
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
  never written, mounted, or opened as a block device. The overlay is a library
  layer only: it is not yet wired into the SCSI/NBD data path.
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
  ISO/FAT32 and forces read-only. `NbdServerConfig` bounds request size,
  clients, handshake/idle deadlines, and option payloads;
  `NbdServerHandle::stop` wakes idle sessions and joins them. The server never
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
controller, recording/scripted command runners, synthetic ISO/FAT32/NBD bytes,
in-memory images/overlays, a fake FunctionFS mounter/clock, and in-memory
transports. They never create a real gadget, bind a real UDC, run `usbip`,
mount a filesystem, or write a block device. The NBD listener tests bind
ephemeral loopback TCP/Unix sockets; the QEMU client tests invoke
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
| `tests/functionfs_lifecycle.rs` | function/link creation, mountpoint validation, fake-mounter mount/unmount, `ep0` descriptor write, readiness success/timeout, bind failure rollback, unbind/cleanup, caller-supplied mount preservation, missing mount tool, config validation |
| `tests/fat32_overlay.rs` | FAT32/raw writes and reads, base immutability, per-block copy-on-write, allocation ceiling and preflight atomicity, commit/reload, checksummed persistence corruption, discard/materialise |
| `tests/uac2.rs` | UAC2 descriptor/channel/rate/size consistency, microphone/speaker/headset modes, invalid rates/channels/modes, ConfigFS attribute writes, unsupported bridge status |
| `tests/usbip_controller.rs` | exact argv, list/port parsing (IPv4/IPv6), attach/detach state, disabled/missing/non-zero typed errors, config validation, host/token allow-list enforcement, no real `usbip` |
| `tests/nbd_server.rs` | newstyle handshake, export name/list/info/go, read/write/flush/disc, request bounds, read-only write rejection, malformed/truncated frames, injected transport/clock deadline, concurrent loopback clients, Unix socket, server stop, file/media backends |
| `tests/nbd_qemu.rs` | live `qemu-img info` on generated FAT32 + read-only ISO, `qemu-io` read and read-only write refusal; opt-in QMP USB attachment |
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
status is not `live` without an observed enable event. The lifecycle helper's
default tests use a temp root, a fake mounter, and a fake UDC name; they never
mount or bind.

The live NBD boot test is also opt-in:

```bash
OZMA_NBD_BOOT_TEST=1 cargo test -p ozma-usb --test nbd_boot -- --ignored
```

It requires `qemu-system-x86_64` and skips otherwise. It never touches an
existing VM; the disposable QEMU is quit over QMP or killed on timeout.

## Remaining integration and security gaps

- Real gadget validation on `dummy_hcd` and hardware UDCs; currently only the
  ignored round trip exists, and node tests use a temp root/fake controller.
- USB/IP security is still transport-level only: the host/token allow-list is
  enforced before `usbip attach`, but there is no TLS, cryptographic pairing,
  challenge/response, daemon, or kernel `vhci_hcd` lifecycle. An external
  `usbipd`/firewall/TLS terminator must protect the wire protocol. USB/IP
  hotplug, a node route exposing `UsbIpController`, and controller/API pairing
  remain open.
- The FAT32 builder is a static snapshot, not the Python live `FATSynthesiser`
  (no sector-on-demand reads, inotify rescan, or long-name rescan stability).
  Generated images are read-only. The new `WriteOverlay` supplies bounded
  copy-on-write blocks and persistence, but it is not yet connected to the NBD
  or SCSI data path, has no FAT metadata/consistency repair, directory-entry
  coalescing, compaction, or write-back to a base image, and no node/API route.
  `ozma-usb` still never writes a generated image to disk or a block device.
- ISO support validates descriptors only; no Joliet/Rock Ridge/El Torito
  parsing, directory extraction, writing, or mounting.
- NBD is a bounded userspace server, not a kernel block device: no structured
  replies, TLS, write-zeroes, cache, multi-conn, or persistent export state.
  The live boot evidence covers a read-only ISO over a disposable QEMU; it is
  not a hardware boot matrix and does not exercise installers, UEFI, or a real
  target. Node wiring is one loopback export behind the existing `--usb-gadget`
  state and is not exposed through capabilities or registration.
- FunctionFS now has an `ep0`/endpoint daemon core and an explicit
  `FunctionFsLifecycle` helper (ConfigFS function/link creation, mountpoint
  expectation, descriptor write, marker readiness/timeout, bind rollback,
  unbind/cleanup), but mounting in the default path still requires an injected
  mounter and caller privileges. The command mounter is untested against a real
  FunctionFS mount/UDC in the default suite; there is no persistent daemon
  process, restart supervision, real UDC event loop, node route that starts the
  lifecycle, or vendor protocol. Class/vendor control requests are rejected
  with typed errors until a protocol handler exists. Suspend and resume events
  are accepted but not acted on. UAC2 still models descriptors/capabilities and
  writes the primary ConfigFS attributes, but there is no ALSA or isochronous
  USB audio bridge; status always reports `bridge: "unsupported"`. Node flags
  exist (`--usb-uac2`, `--usb-vendor`, `--usb-vendor-daemon`), but `/usb`
  surfaces readiness/absence rather than starting either backend; the lifecycle
  status is library-only for now.
- Node-side gadget teardown/recovery across restart, and hardware-profile
  validation. Mounting, block-device I/O, and real USB/IP transport remain
  deliberately outside this crate.
