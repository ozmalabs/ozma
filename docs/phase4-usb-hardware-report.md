# Phase 4 — USB hardware validation on `dummy_hcd`

Date: 2026-11-25
Host: `SFFPC`, kernel `7.3.5-2-cachyos` (`#1 SMP PREEMPT_DYNAMIC Fri, 20 Nov
2026`), `usbip (usbip-utils 2.0)`.

Scope: real kernel gadget/FunctionFS/UAC2/USB-IP validation using only
`dummy_hcd`, `libcomposite`, `vhci_hcd`, and `usbip_host`. No real USB hardware
was used. Nothing was committed. `docs/parity-matrix.md` and
`docs/plan-execution-status.md` were not edited.

## Verdict

| Path | Result | Evidence |
|---|---|---|
| 1. ConfigFS mass-storage gadget + host SCSI disk | **PASS** | `usb 8-1` enumerated as `1d6b:0104`; `usb-storage` created `/dev/sda` (32 MiB); mount + write + read-back succeeded |
| 2. Node `--usb-gadget` mass-storage | **PASS** | node bound gadget `ozma` with 3 functions; `/usb` reported `available=true`; `/dev/sda` mount + write + read-back succeeded |
| 3. FunctionFS + `FunctionFsMediaDaemon` (real CBW/CSW) | **PASS** | `usb-storage` drove the daemon; 109 CBWs / 109 CSWs, 18 writes, 27,648 OUT bytes, 387,194 IN bytes; file written and read back |
| 4. UAC2 gadget + host sound card | **FAIL (kernel block)** | `afunc_bind:1251 Error!` / `-ENODEV`; `dummy_hcd` has no isochronous endpoints by design |
| 4a. UAC2 bridge core + ALSA adapters (supporting) | **PARTIAL PASS** | 16/16 bridge tests, 7/7 adapter tests, real ALSA loopback capture→packetiser test passed; no real UAC2 endpoint |
| 5. USB/IP kernel host path (`usbip bind` + `vhci_hcd`) | **PASS** | vhci port 00 in use, `usb 9-1` enumerated, `/dev/sda` read through the imported device |

## Baseline / module state

Before the lab:

```text
dummy_hcd            absent
libcomposite         absent
vhci_hcd             absent
usb_f_* / u_audio    absent
usbip_host           LOADED
usbip_core           LOADED
configfs             mounted on /sys/kernel/config
/sys/class/udc       empty
```

`dummy_hcd` parameters on this kernel:

```text
is_high_speed=Y
is_super_speed=N
num=1
```

So only a high-speed (USB 2.0) UDC is available and there is no SuperSpeed
path.

## 1. ConfigFS mass-storage gadget (PASS)

```bash
sudo -n modprobe dummy_hcd
sudo -n modprobe libcomposite
sudo -n modprobe usb_f_mass_storage
ls /sys/class/udc/                    # dummy_udc.0

truncate -s 32M /tmp/opencode/usb-lab/disk.img
mkfs.ext4 -q -F /tmp/opencode/usb-lab/disk.img

G=/sys/kernel/config/usb_gadget/g_ms
mkdir -p "$G"
echo 0x1d6b > "$G/idVendor"
echo 0x0104 > "$G/idProduct"
mkdir -p "$G/strings/0x409" "$G/configs/c.1/strings/0x409"
echo Ozma > "$G/strings/0x409/manufacturer"
echo "Ozma dummy mass storage" > "$G/strings/0x409/product"
echo OZMAMS0001 > "$G/strings/0x409/serialnumber"
echo "mass-storage" > "$G/configs/c.1/strings/0x409/configuration"
echo 250 > "$G/configs/c.1/MaxPower"
mkdir -p "$G/functions/mass_storage.0"
echo /tmp/opencode/usb-lab/disk.img > "$G/functions/mass_storage.0/lun.0/file"
echo 0 > "$G/functions/mass_storage.0/lun.0/ro"
echo 0 > "$G/functions/mass_storage.0/lun.0/cdrom"
echo 1 > "$G/functions/mass_storage.0/lun.0/removable"
ln -s "$G/functions/mass_storage.0" "$G/configs/c.1/mass_storage.0"
echo dummy_udc.0 > "$G/UDC"
```

Kernel/host evidence:

```text
usb 8-1: new high-speed USB device number 2 using dummy_hcd
usb 8-1: New USB device found, idVendor=1d6b, idProduct=0104
usb-storage 8-1:1.0: USB Mass Storage device detected
scsi 6:0:0:0: Direct-Access Linux File-Stor Gadget 0703
sd 6:0:0:0: [sda] 65536 512-byte logical blocks: (33.6 MB/32.0 MiB)
sd 6:0:0:0: [sda] Attached SCSI removable disk
```

```text
$ lsusb -d 1d6b:0104
Bus 008 Device 002: ID 1d6b:0104 Linux Foundation Multifunction Composite Gadget
$ lsblk /dev/sda
sda 32M disk File-Stor Gadget OZMAMS0001
```

Transfer through the exported SCSI disk:

```bash
sudo mount -t ext4 /dev/sda /tmp/opencode/usb-lab/mnt
echo "ozma-usb-transfer-OK $(date -u +%s)" | sudo tee /tmp/opencode/usb-lab/mnt/ozma-proof.txt
sudo sync
sudo cat /tmp/opencode/usb-lab/mnt/ozma-proof.txt
sudo dd if=/dev/sda of=/tmp/opencode/usb-lab/sda-head.bin bs=1M count=1
sudo umount /tmp/opencode/usb-lab/mnt
```

Read-back produced `ozma-usb-transfer-OK 1795591741`. The gadget was then
unbound (`echo '' > UDC`), the config link/functions/gadget were removed, and
the host `sda` disappeared.

## 2. Node `--usb-gadget` mass storage (PASS)

The node creates `hid.kbd`, `hid.mouse`, and `mass_storage.usb0` for
`--usb-gadget`, so this validates the node's own gadget wiring rather than a
hand-written ConfigFS tree.

```bash
sudo -n modprobe dummy_hcd
truncate -s 32M /tmp/opencode/usb-lab/node-disk.img
mkfs.ext4 -q -F /tmp/opencode/usb-lab/node-disk.img

sudo -n env RUST_LOG=ozma_node=info,ozma_usb=info \
  target/debug/ozma-node --name ozma-usb-lab --profile vm --cap '' \
  --http-port 7399 --hid-udp-port 7398 \
  --usb-gadget --usb-udc dummy_udc.0 \
  --usb-configfs-root /sys/kernel/config/usb_gadget \
  --usb-media /tmp/opencode/usb-lab/node-disk.img
```

Node evidence:

```text
INFO ozma_node: USB gadget created and bound gadget=ozma udc="dummy_udc.0" functions=3
INFO ozma_node: USB gadget created and bound; usb capability advertised ... media=Some(".../node-disk.img")
```

```text
GET /usb -> available=true, gadget.bound=true, udc=dummy_udc.0,
            functions hid.kbd / hid.mouse / mass_storage.usb0,
            mass_storage mode=disk state=attached size_bytes=33554432
```

`usb-storage` again created `/dev/sda` (`32M File-Stor Gadget OZMA0001`).
Mounting it and writing/reading a file succeeded:

```bash
sudo mount -t ext4 /dev/sda /tmp/opencode/usb-lab/mnt-node
echo "ozma-node-gadget-OK $(date -u +%s)" | sudo tee /tmp/opencode/usb-lab/mnt-node/ozma-node-proof.txt
sudo sync
sudo cat /tmp/opencode/usb-lab/mnt-node/ozma-node-proof.txt   # ozma-node-gadget-OK 1795594208
sudo umount /tmp/opencode/usb-lab/mnt-node
```

After `SIGTERM`, the node process exited, the block device disappeared, and the
`ozma` gadget was removed from ConfigFS.

## 3. FunctionFS + `FunctionFsMediaDaemon` (PASS)

`dummy_hcd` supports bulk endpoints, so the FunctionFS SCSI path is usable.
Because the node creates and binds the gadget before it opens
`--usb-functionfs-mount`, the mount cannot exist early enough in a normal node
startup. To exercise the real daemon we used the crate's own
`FunctionFsMediaDaemon::file` in the canonical order:

1. create the ConfigFS gadget + `ffs.vendor0` + config link, **unbound**;
2. `mount -t functionfs vendor0 /tmp/opencode/usb-lab/ffs`;
3. build/spawn `FunctionFsMediaDaemon` (writes the ep0 descriptor blob);
4. write the UDC to bind;
5. let the host `usb-storage` issue real CBWs.

The temporary example declared the interface as class `0x08`/subclass `0x06`/
protocol `0x50` (Mass Storage / SCSI transparent / Bulk-Only Transport), bulk
OUT EP1 and bulk IN EP2, so the host bound `usb-storage` automatically.

```bash
# gadget + FunctionFS mount + daemon + bind are performed by the example;
# its effective commands were:
mount -t functionfs vendor0 /tmp/opencode/usb-lab/ffs
# FunctionFsMediaDaemon::file(...) -> spawn() -> controller.bind("ozma-ffs-lab", "dummy_udc.0")
```

Kernel evidence:

```text
file system registered
read descriptors
read strings
usb 8-1: new high-speed USB device number 2 using dummy_hcd
usb-storage 8-1:1.0: USB Mass Storage device detected
scsi 6:0:0:0: Direct-Access OZMA Virtual Media 1.0
sd 6:0:0:0: [sda] 65536 512-byte logical blocks: (33.6 MB/32.0 MiB)
sd 6:0:0:0: [sda] Attached SCSI disk
```

Daemon counters from the shared `MediaDaemonStatus`:

```text
# after host enumeration only
state=Running live=true enabled=true attached=true running=true
commands=50 cbws=50 csws=50 data_out=0 data_in=341114
reads=38 writes=0 flush=0 check=1 transport_errors=0 timeouts=0 bound_errors=0

# after mounting, writing, syncing and reading back a file
state=Running live=true enabled=true attached=true running=true
commands=109 cbws=109 csws=109 data_out=27648 data_in=387194
reads=79 writes=18 flush=0 check=1 transport_errors=0 timeouts=0 bound_errors=0
```

The write path was a real SCSI WRITE through the FunctionFS bulk OUT endpoint:

```bash
sudo mount -t ext4 /dev/sda /tmp/opencode/usb-lab/mnt-ffs
echo "ozma-ffs-transfer-OK $(date -u +%s)" | sudo tee /tmp/opencode/usb-lab/mnt-ffs/ozma-ffs-proof.txt
sudo sync
sudo cat /tmp/opencode/usb-lab/mnt-ffs/ozma-ffs-proof.txt   # ozma-ffs-transfer-OK 1795593632
sudo umount /tmp/opencode/usb-lab/mnt-ffs
```

`writes=18`, `data_out=27648`, and a successful read-back prove the CSW/read/
write loop, not just descriptor negotiation.

### FunctionFS shutdown observation

Graceful shutdown was slow: the helper's first `controller.unbind()` returned
without clearing the gadget `UDC` attribute (its error was ignored by the
helper), and `MediaDaemonHandle::shutdown()` then waited until the UDC was
unbound a second time by hand. After the second `echo '' > UDC`, the daemon
exited, unmounted FunctionFS, and removed the gadget. This did not affect the
data path, but it is a real shutdown-ordering gap worth fixing or asserting in
the daemon/node.

## 4. UAC2 (FAIL — blocked by `dummy_hcd`)

A UAC2 function was created with playback/capture enabled (`p_chmask=3`,
`c_chmask=3`, 48 kHz, S16, `req_number=2`) and bound to `dummy_udc.0`.

```text
configfs-gadget.g_uac2 gadget.0: afunc_bind:1251 Error!
udc dummy_udc.0: failed to start g_uac2: -19
UDC core: g_uac2: couldn't find an available UDC or it's busy
```

No `1d6b:0105` device appeared and no UAC2 card was added to
`/proc/asound/cards`. The same failure reproduced after a clean
`rmmod/modprobe dummy_hcd`, so it is not stale UDC state.

Root cause is in the kernel's `dummy_hcd` driver itself:

```text
Note: The emulation does not include isochronous transfers!

/* we don't provide isochronous endpoints since we don't support them */
...
EP_INFO("ep3in-iso", ... )   /* commented out */
EP_INFO("ep4out-iso", ... )  /* commented out */
```

UAC2 requires isochronous IN/OUT (and a feedback IN) endpoints, so
`afunc_bind`'s `usb_ep_autoconfig()` returns `NULL`, which becomes `-ENODEV`
(`-19`). This is a hard kernel/UDC limitation, not an Ozma regression.

Running the node's UAC2 bridge against the same UDC produced the matching
typed failure:

```text
WARN ozma_node: USB gadget unavailable: create/bind failed ...
error=ConfigFS write attribute failed at .../ozma/UDC: Device or resource busy (os error 16)
GET /usb -> "USB gadget is configured but unavailable: configfs-io: ..."
```

**Audio did not move over UAC2.** No UAC2 counters exist because the function
never bound.

### Supporting audio evidence (not a UAC2 gadget)

The transport-independent bridge and the real ALSA adapters were still
exercised without touching the user's audio devices:

```text
cargo test -p ozma-usb --test uac2_bridge
  16 passed; 0 failed

cargo test -p ozma-audio --test uac2_adapters
  7 passed; 0 failed
```

Real ALSA capture into the UAC2 packetiser, using `snd-aloop` and a
`speaker-test` feeder:

```bash
sudo modprobe snd-aloop
speaker-test -D hw:Loopback,0,0 -c 2 -r 48000 -F S16_LE -t sine -l 20 &
OZMA_AUDIO_TEST_DEVICE=hw:Loopback,1,0 \
  cargo test -p ozma-audio --test uac2_bridge_real -- --ignored
# 1 passed; 0 failed
```

This proves the ALSA adapter/capture/packetiser path moves PCM correctly, but
it uses the injected UAC2 endpoint; it does not prove a real UAC2 gadget
enumerated or that host↔device PCM crossed USB.

## 5. USB/IP kernel path (PASS)

The already-enumerated dummy mass-storage device was exported with
`usbip-host` and re-imported through `vhci_hcd`:

```bash
sudo -n modprobe vhci_hcd
sudo -n usbipd -D -P /tmp/opencode/usb-lab/usbipd.pid

sudo -n usbip bind -b 8-1
# usbip: info: bind device on busid 8-1: complete

usbip list -r 127.0.0.1
# 8-1: Linux Foundation : Multifunction Composite Gadget (1d6b:0104)

sudo -n usbip attach -r 127.0.0.1 -b 8-1
usbip port
# Port 00: <Port in Use> at High Speed(480Mbps)
#        Linux Foundation : Multifunction Composite Gadget (1d6b:0104)
```

Evidence of a real vhci device and control+URB exchange:

```text
usbip-host 8-1: stub up
vhci_hcd vhci_hcd.0: pdev(0) rhport(0) sockfd(3)
vhci_hcd vhci_hcd.0: devid(524290) speed(3) speed_str(high-speed)
vhci_hcd vhci_hcd.0: Device attached
usb 9-1: new high-speed USB device number 2 using vhci_hcd
usb 9-1: SetAddress Request (2) to port 0
usb 9-1: New USB device found, idVendor=1d6b, idProduct=0104
usb-storage 9-1:1.0: USB Mass Storage device detected
sd 6:0:0:0: [sda] 65536 512-byte logical blocks: (33.6 MB/32.0 MiB)
```

`/sys/devices/platform/vhci_hcd.0/status` showed:

```text
hs  0000 006 003 00080002 000003 9-1
```

The imported device was mounted read-only and the previously written proof file
was read back through USB/IP:

```bash
sudo mount -t ext4 -o ro /dev/sda /tmp/opencode/usb-lab/mnt
sudo cat /tmp/opencode/usb-lab/mnt/ozma-proof.txt   # ozma-usb-transfer-OK 1795591741
sudo umount /tmp/opencode/usb-lab/mnt
```

Teardown:

```bash
sudo usbip detach -p 00      # usbip: info: Port 0 is now detached!
sudo usbip unbind -b 8-1     # usbip: info: unbind device on busid 8-1: complete
```

Notes:

- `usbipd` logs `Please load appropriate modules or export devices` at startup
  before the first `usbip bind`; it still serves exports afterwards.
- `usbip port` prints non-fatal `libusbip: error: fopen` / `read_record`
  messages on this host, then still lists the imported port correctly.
- During initial probe the export logged transient
  `usbip-host 8-1: urb completion with non-zero status -121` lines; the device
  enumerated and bulk reads succeeded, so these did not block the path.

## Cleanup and final host state

Strict teardown performed:

```bash
# all gadgets: unbind UDC, remove config links, functions, gadget dirs
# FunctionFS: umount /tmp/opencode/usb-lab/ffs
sudo usbip detach -p 00
sudo usbip unbind -b 8-1
pkill -x usbipd

sudo modprobe -r vhci_hcd
sudo modprobe -r usb_f_uac2 usb_f_mass_storage usb_f_fs usb_f_hid u_audio
sudo modprobe -r libcomposite
sudo modprobe -r dummy_hcd

# snd_aloop could not be removed while WirePlumber held controlC5:
# unbinding the loopback platform device released it cleanly:
echo snd_aloop.0 | sudo tee /sys/bus/platform/drivers/snd_aloop/unbind
sudo modprobe -r snd-aloop
```

Final state:

```text
UDC:                 none (/sys/class/udc empty)
ConfigFS usb_gadget: absent (libcomposite unloaded; configfs itself still mounted)
FunctionFS mounts:   none
dummy mass-storage:  none
usbipd / port 3240:  none / closed
vhci platform:       none
Block devices:       no File-Stor / Virtual Media devices
Sound cards:         0 PCH, 1 Generic, 2 Generic_1, 3 C4K, 4 Rubix22
                     (the pre-existing user cards; no Loopback/UAC2 card)
Modules remaining:   usbip_host, usbip_core (both pre-existing)
```

The temporary lab directory `/tmp/opencode/usb-lab` (images, logs, mountpoints)
and the temporary `ozma-usb/examples/ffs_media_lab.rs` helper were removed.
No commit was made.

## Remaining gaps

1. **`dummy_hcd` cannot enumerate UAC2.** The emulator deliberately omits
   isochronous endpoints, so the UAC2 descriptor/bridge node path can only be
   validated on a real UDC (or another UDC that implements ISO). The node
   surfaces the failure cleanly (`/usb` typed `configfs-io`, warning), but
   host-visible UAC2 PCM movement remains unproven.
2. **Node `--usb-functionfs-mount` startup ordering.** The node creates and
   binds the gadget in one step and only then opens the supplied FunctionFS
   mount. The mount only becomes possible after the `ffs.*` ConfigFS function
   exists, so a normal node startup cannot mount in time. This lab exercised the
   same `FunctionFsMediaDaemon` directly. The node path needs an explicit
   mount-before-bind handoff (or a start-route field for the mountpoint) to be
   usable end-to-end.
3. **FunctionFS daemon shutdown ordering.** A first UDC unbind during
   `MediaDaemonHandle::shutdown()` did not clear the UDC and the daemon join
   stalled until a second unbind. This should be reproduced and fixed; the
   data path itself is healthy.
4. **USB/IP ISO support.** Even with `vhci_hcd`, UAC2/isochronous devices over
   USB/IP are not validated here; `usbip`/vhci paths are exercised with
   bulk/control (mass storage) only.
5. **USB/IP transient `-121` URB completions.** Initial probe logged several
   `urb completion with non-zero status -121`, then enumeration and bulk I/O
   succeeded. Worth confirming on real hardware whether these are harmless
   probe retries or a relay timing issue.
6. **Node UAC2 CLI is fixed at 2 playback / 1 capture, 48 kHz, S16.** The
   adapters support more formats, but this was not exercised because no UAC2
   gadget could bind.
