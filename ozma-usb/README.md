# ozma-usb

Safe USB gadget, virtual-media, and USB/IP control foundations for Ozma nodes.

This crate is the model and controller layer of the Python
`softnode/virtual_usb.py`, `softnode/usb_mass_storage.py`, and
`softnode/virtual_media.py` paths. It is a pure library: it does not mount
filesystems, synthesize FAT32, serve NBD, open `/dev/hidg*`, or write a block
device. `ozma-node` now wires it behind explicit USB flags (see below); there is
still no automatic gadget creation on a hardware profile.

## What is implemented

| Area | API |
|---|---|
| Composite gadget model | `UsbGadgetConfig`, `UsbFunction`, `UsbFunctionKind`, `HidProtocol` |
| Functions | HID keyboard/mouse/custom, mass storage, UAC2, FunctionFS vendor |
| Validation | Safe gadget/function names, VID/PID, strings, power, duplicate links, HID report shape, UAC2 rates/sizes, CD-ROM read-only |
| Virtual media | `MassStorageImage`, `VirtualMedia`, `MediaMode::{Disk,CdRom}`, `MediaState::{Attached,Detached}` |
| Controller trait | `UsbGadgetController` (`create`, `bind`, `unbind`, `status`, `list`, `attach_media`, `detach_media`, `media_state`, `capabilities`) |
| Linux ConfigFS | `ConfigFsUsbController`, injectable root, default `/sys/kernel/config/usb_gadget` |
| Test/dry-run fake | `FakeUsbGadgetController` |
| USB/IP fallback | `SubprocessUsbController`, `UsbIpControl`, `CommandRunner`, `SystemCommandRunner` |
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

## USB/IP fallback

`SubprocessUsbController` builds `usbip` argv vectors and passes them to a
`CommandRunner`; there is no shell and no interpolation. Bus IDs, hosts, and
ports are validated and may not start with `-`. `list_local` parses
`usbip list -l`. The production `SystemCommandRunner` maps a missing executable
to `UsbError::MissingCommand` (install `usbip`).

## Tests

Default tests are CI-safe and use a temporary ConfigFS-shaped root, a fake
controller, and a recording command runner. They never create a real gadget,
bind a real UDC, mount a filesystem, or write a block device.

```bash
cargo test -p ozma-usb
cargo check -p ozma-usb --all-targets
cargo fmt -p ozma-usb
```

The one real-ConfigFS round trip is ignored and env-gated:

```bash
OZMA_USB_CONFIGFS_TEST=1 OZMA_USB_UDC=dummy_udc.0 \
  cargo test -p ozma-usb --test real_configfs -- --ignored
```

It still requires root, a mounted ConfigFS root (optionally
`OZMA_USB_CONFIGFS_ROOT`), and a free UDC. It creates only HID functions and
removes the test gadget.

## Remaining integration work

- Real gadget validation on `dummy_hcd` and hardware UDCs; currently only the
  ignored round trip exists, and node tests use a temp root/fake controller.
- USB/IP pairing with the controller, authorization, and hotplug.
- FunctionFS vendor daemon (descriptors, endpoints, userspace I/O) and real
  UAC2 playback/capture bridging.
- Virtual-media backends: FAT32 synthesis, NBD serving, ISO handling, and
  read-write sync/overlay semantics from `virtual_media.py` /
  `usb_mass_storage.py`.
- Node-side gadget teardown/recovery across restart, and hardware-profile
  validation. Mounting, image creation, and block-device I/O remain
  deliberately outside this crate.
