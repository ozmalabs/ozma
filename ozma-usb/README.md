# ozma-usb

Safe USB gadget, virtual-media, and USB/IP control foundations for Ozma nodes.

This crate is the model and controller layer of the Python
`softnode/virtual_usb.py`, `softnode/usb_mass_storage.py`, and
`softnode/virtual_media.py` paths. It is a pure library: it builds FAT32 images
in memory and models ISO/NBD media, but it does not mount filesystems, serve
NBD over a socket, open `/dev/hidg*`, or write a block device. `ozma-node` now
wires the gadget layer behind explicit USB flags (see below); there is still no
automatic gadget creation on a hardware profile, and the USB/IP/FAT32/ISO/NBD
models are not exposed by node routes yet.

## What is implemented

| Area | API |
|---|---|
| Composite gadget model | `UsbGadgetConfig`, `UsbFunction`, `UsbFunctionKind`, `HidProtocol` |
| Functions | HID keyboard/mouse/custom, mass storage, UAC2, FunctionFS vendor |
| Validation | Safe gadget/function names, VID/PID, strings, power, duplicate links, HID report shape, UAC2 rates/sizes, CD-ROM read-only |
| Virtual media | `MassStorageImage`, `VirtualMedia`, `MediaMode::{Disk,CdRom}`, `MediaFormat::{Raw,Iso9660,Fat32,Nbd}`, `MediaState::{Attached,Detached}` |
| Controller trait | `UsbGadgetController` (`create`, `bind`, `unbind`, `status`, `list`, `attach_media`, `detach_media`, `media_state`, `capabilities`) |
| Linux ConfigFS | `ConfigFsUsbController`, injectable root, default `/sys/kernel/config/usb_gadget` |
| Test/dry-run fake | `FakeUsbGadgetController` |
| USB/IP model | `UsbIpConfig`, `UsbIpDevice`, `UsbIpState`, `UsbIpAttachment`, `UsbIpAuthorization`/`UsbIpKey` (advisory pairing metadata) |
| USB/IP control | `UsbIpController`, `SubprocessUsbIpController`, argv-only `CommandRunner`; legacy `UsbIpControl`/`SubprocessUsbController` |
| FAT32 builder | `Fat32ImageBuilder`, `Fat32Node`, `build_fat32_image`, `fat32_image_from_directory`, `parse_fat32_image`, `Fat32Info`/`Fat32Layout` |
| ISO media | `IsoImage::inspect`, `parse_iso_volume_descriptors`, read-only `MediaFormat::Iso9660` |
| NBD model | `NbdExport`, `NbdExportDescriptor`, `NbdHandshake`, `NbdRequest`/`NbdReply` framing (no socket server) |
| Capabilities/status | `UsbGadgetCapabilities`, `GadgetStatus`, `FunctionStatus`; `protocol_capability()` projects onto `ozma_protocol::UsbCapability` |

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
  --usb-media /srv/installer.iso --usb-media-cdrom
```

Equivalent environment variables are `OZMA_USB_GADGET`, `OZMA_USB_CONFIGFS_ROOT`,
`OZMA_USB_UDC`, `OZMA_USB_MEDIA`, `OZMA_USB_MEDIA_RO`, and
`OZMA_USB_MEDIA_CDROM`. The node builds a HID keyboard + mouse gadget and
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
| `GET /usb` | Bearer-guarded read-only gadget status, function list, capabilities, and LUN media state (no backing paths). |
| `POST /usb/media/attach` | Attach an existing absolute regular file to a mass-storage instance; enforces read-only/CD-ROM. |
| `POST /usb/media/detach` | Clear the LUN `file` attribute; the function/gadget stay in place. |

Attach/detach write only ConfigFS `file`/`ro`/`cdrom` attributes. They do not
mount, create, truncate, or write the backing image.

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
ConfigFS attach with `unsupported-media-format`; use the NBD descriptor model
and a userspace/NBD consumer instead.

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
    ffs.<instance>/        (FunctionFS descriptors come from userspace)
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

## USB/IP

`UsbIpConfig` records whether USB/IP is enabled, the default host/port
(`127.0.0.1:3240`), configured local/remote `UsbIpDevice`s, and optional
advisory `UsbIpAuthorization` metadata (allowed hosts and key fingerprints).
Validation rejects empty/malformed hosts, port 0, duplicate host+bus-id pairs,
oversized key lists, malformed bus IDs, and option-like values beginning with
`-`. Pairing metadata is **not cryptographic enforcement**: no USB/IP wire
protocol, TLS, key exchange, daemon, or kernel `vhci_hcd` operation exists in
this crate. An external `usbipd`, firewall, or TLS terminator must enforce it.

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
- **ISO** (`iso`): `IsoImage::inspect` reads at most 32 volume descriptors from
  an existing file, verifies `CD001`, the primary descriptor, both-endian
  size/block fields, and root-directory bounds. `MediaFormat::Iso9660` always
  means CD-ROM + read-only. There is no ISO writing, El Torito parsing,
  directory-tree extraction, or mounting.
- **NBD** (`nbd`): `NbdExport` validates name/size/block size/read-only flags;
  `NbdExportDescriptor` models TCP or Unix-socket endpoints; `NbdHandshake`,
  `NbdRequestHeader`, `NbdRequest`, and `NbdReply` encode/decode bounded
  big-endian framing with request-size limits. This is a framing model only:
  no listener, socket, block device, write overlay, or userspace server is
  started. QEMU/`nbdkit` integration remains node work.

## Tests

Default tests are CI-safe and use a temporary ConfigFS-shaped root, a fake
controller, recording/scripted command runners, synthetic ISO/FAT32/NBD bytes,
and in-memory images. They never create a real gadget, bind a real UDC, run
`usbip`, open an NBD socket, mount a filesystem, or write a block device.

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
| `tests/usbip_controller.rs` | exact argv, list/port parsing (IPv4/IPv6), attach/detach state, disabled/missing/non-zero typed errors, config validation, no real `usbip` |

The one real-ConfigFS round trip is ignored and env-gated:

```bash
OZMA_USB_CONFIGFS_TEST=1 OZMA_USB_UDC=dummy_udc.0 \
  cargo test -p ozma-usb --test real_configfs -- --ignored
```

It still requires root, a mounted ConfigFS root (optionally
`OZMA_USB_CONFIGFS_ROOT`), and a free UDC. It creates only HID functions and
removes the test gadget.

## Remaining integration and security gaps

- Real gadget validation on `dummy_hcd` and hardware UDCs; currently only the
  ignored round trip exists, and node tests use a temp root/fake controller.
- USB/IP security is not implemented: no TLS, no cryptographic pairing, no
  challenge/response, no client authentication, and no kernel `vhci_hcd`
  lifecycle. `UsbIpAuthorization` is advisory metadata for an external
  `usbipd`/firewall/proxy. USB/IP hotplug and controller/API pairing remain
  node work, and no route currently exposes `UsbIpController`.
- The FAT32 builder is a static snapshot, not the Python live `FATSynthesiser`
  (no sector-on-demand reads, inotify rescan, long-name rescan stability, or
  write overlay). Generated images are read-only. Exposing a built image to a
  gadget still requires writing it to a file/exported medium outside this
  crate; `ozma-usb` never writes it.
- ISO support validates descriptors only; no Joliet/Rock Ridge/El Torito
  parsing, directory extraction, writing, or mounting.
- NBD is a descriptor/framing model only; no socket server, negotiation loop,
  write overlay, flush/trim execution, or QEMU integration.
- FunctionFS vendor daemon (descriptors, endpoints, userspace I/O) and real
  UAC2 playback/capture bridging.
- Node-side gadget teardown/recovery across restart, and hardware-profile
  validation. Mounting, block-device I/O, and real USB/IP/NBD transport remain
  deliberately outside this crate.
