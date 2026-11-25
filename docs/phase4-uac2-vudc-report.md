# Phase 4 — UAC2 over USB/IP `usbip-vudc`

Date: 2026-11-26
Host: `SFFPC`, kernel `7.3.5-2-cachyos` (`#1 SMP PREEMPT_DYNAMIC`),
`usbip (usbip-utils 2.0)`, `usbip-vudc`, `vhci_hcd`, `snd-aloop`.

Scope: UAC2 enumeration and PCM validation over a virtual USB/IP link created
with `usbip-vudc` + `vhci_hcd`, using only virtual/loopback audio. No real USB
hardware, real sound card, default sink, or user audio device was touched.
Nothing was committed. `docs/parity-matrix.md` and
`docs/plan-execution-status.md` were not edited.

Related reports:
[`phase4-usb-hardware-report.md`](phase4-usb-hardware-report.md) (dummy_hcd has
no ISO endpoints) and
[`phase4-uac2-node-report.md`](phase4-uac2-node-report.md) (bridge/node
wiring).

## Verdict

| Check | Result | Evidence |
|---|---|---|
| UAC2 function binds to `usbip-vudc.0` | **PASS** | bind returned 0; no `afunc_bind` / `-ENODEV`; `/sys/class/udc/usbip-vudc.0/function = uac2.audio` |
| ISO endpoints exposed by the gadget | **PASS** | `0x03` OUT ISO async 196 B, `0x84` IN ISO feedback 4 B, `0x85` IN ISO async 98 B, all `bInterval=4` |
| USB/IP export + local import | **PASS** | `usbip list --device` showed `usbip-vudc.0`; `usbip attach` created vhci port 00; `usb 8-1` enumerated as `1d6b:0104` |
| Host `snd-usb-audio` card | **PASS** | card 7 `[Gadget] USB-Audio - Ozma USB Gadget`; S16_LE 48 kHz; playback 2 ch, capture 1 ch |
| USB OUT (host→device) PCM | **FAIL (kernel block)** | 4 999 ISO OUT completions status `-18` (`-EXDEV`); loopback sink captured 1 434 192 frames, all zero |
| USB IN (device→host) PCM | **FAIL (kernel block)** | 3 420 ISO IN completions status `-18`; host `arecord` got `EIO`, 44-byte header-only WAV |
| Node `--usb-uac2-bridge` lifecycle | **PASS (typed failure)** | bridge opened devices and started; `/usb` stayed `configured` with `uac2_bridge_error.kind = "uac2-endpoint"`; never claimed live audio |
| Real UAC2 PCM over this virtual UDC | **BLOCKED** | `usbip-vudc` advertises ISO but its transfer path returns `-EXDEV` for ISO URBs |

Audio was **not** faked. The host playback command exits 0 because
`snd-usb-audio` consumes the ALSA buffer, but USB/IP completed every
isochronous URB with `-18`, and the measured sink was silent.

## Baseline

Before the lab:

```text
dummy_hcd            absent
usbip-vudc           absent
vhci_hcd             absent
u_audio / usb_f_uac2 absent
usb_f_hid / usb_f_mass_storage absent
snd-aloop            absent
usbmon               absent
libcomposite         LOADED (pre-existing)
udc_core             LOADED (pre-existing through libcomposite)
usbip_host           LOADED (pre-existing)
usbip_core           LOADED (pre-existing)
/sys/class/udc       empty
ConfigFS             mounted at /sys/kernel/config
Sound cards          0 PCH, 1 Generic, 2 Generic_1,
                     3 C4K (USB), 4 Rubix22 (USB)
```

Modules loaded for the lab (all removed afterwards):

```bash
sudo modprobe usbip-vudc
sudo modprobe usb_f_uac2          # pulls in u_audio
sudo modprobe usb_f_hid usb_f_mass_storage
sudo modprobe vhci_hcd
sudo modprobe snd-aloop
sudo modprobe usbmon
```

`usbip-vudc` created exactly one UDC:

```text
/sys/class/udc/usbip-vudc.0
maximum_speed: high-speed
state:         not attached
```

The kernel's vudc endpoint setup advertises all general-purpose endpoints as
`type_iso = true` (verified in the UDC source and by the successful UAC2
bind), unlike `dummy_hcd`, where ISO endpoints are commented out.

## 1. UAC2 gadget creation and bind (PASS)

Initial manual gadget (later replaced by the node's gadget):

```bash
G=/sys/kernel/config/usb_gadget/g_uac2
mkdir -p "$G"/{strings/0x409,configs/c.1/strings/0x409,functions/uac2.usb0}
echo 0x1d6b > "$G/idVendor"; echo 0x0105 > "$G/idProduct"
# strings omitted here
echo 3 > "$G/functions/uac2.usb0/p_chmask"     # manual probe: 2 ch USB IN
echo 1 > "$G/functions/uac2.usb0/c_chmask"     # manual probe: 1 ch USB OUT
echo 48000 > "$G/functions/uac2.usb0/p_srate"
echo 48000 > "$G/functions/uac2.usb0/c_srate"
echo 2 > "$G/functions/uac2.usb0/p_ssize"
echo 2 > "$G/functions/uac2.usb0/c_ssize"
echo 2 > "$G/functions/uac2.usb0/req_number"
ln -s "$G/functions/uac2.usb0" "$G/configs/c.1/uac2.usb0"
echo usbip-vudc.0 > "$G/UDC"                   # rc=0
```

The kernel accepted the ISO endpoints and created the gadget-side ALSA card:

```text
 6 [UAC2Gadget     ]: UAC2_Gadget - UAC2_Gadget
 06-00: UAC2 PCM : UAC2 PCM : playback 1 : capture 1
```

No `afunc_bind` error and no `-ENODEV`. This is the step that failed on
`dummy_hcd`.

## 2. USB/IP export and host enumeration (PASS)

`usbipd` was started in device mode (the vudc export path), not the normal
host-device mode:

```bash
sudo usbipd -e -D -P /tmp/opencode/uac2-vudc/usbipd.pid
sudo usbip list --device
#  - busid usbip-vudc.0 (0x1d:0x1d)
#    Linux Foundation : Multifunction Composite Gadget (1d6b:0104)

sudo modprobe vhci_hcd
sudo usbip attach -r 127.0.0.1 -b usbip-vudc.0
usbip port
# Port 00: <Port in Use> at High Speed(480Mbps)
#        Linux Foundation : Multifunction Composite Gadget (1d6b:0104)
```

Enumerated through the same host's vhci controller:

```text
vhci_hcd vhci_hcd.0: pdev(0) rhport(0) sockfd(3)
vhci_hcd vhci_hcd.0: devid(0) speed(3) speed_str(high-speed)
vhci_hcd vhci_hcd.0: Device attached
usb 8-1: new high-speed USB device number 2 using vhci_hcd
usb 8-1: SetAddress Request (2) to port 0
usb 8-1: New USB device found, idVendor=1d6b, idProduct=0104
usb 8-1: Product: Ozma USB Gadget
usb 8-1: Manufacturer: Ozma Labs
```

`/sys/devices/platform/vhci_hcd.0/status`:

```text
hs  0000 006 003 00000000 000003 8-1
```

Host sound card and PCM parameters:

```text
 6 [UAC2Gadget     ]: UAC2_Gadget - UAC2_Gadget
 7 [Gadget         ]: USB-Audio - Ozma USB Gadget
                      Ozma Labs Ozma USB Gadget at usb-vhci_hcd.0-1, high speed

hw:7,0 playback: S16_LE, 48000 Hz, 2 channels
hw:7,0 capture:  S16_LE, 48000 Hz, 1 channel
```

Descriptor evidence from `lsusb -v` (node gadget after the endpoint-direction
fix described below):

```text
bEndpointAddress 0x03  EP 3 OUT
  Transfer Type  Isochronous
  Synch Type     Asynchronous
  wMaxPacketSize 0x00c4 (196)
  bInterval      4
bEndpointAddress 0x84  EP 4 IN
  Transfer Type  Isochronous
  wMaxPacketSize 0x0004 (4)      # feedback
  bInterval      4
bEndpointAddress 0x85  EP 5 IN
  Transfer Type  Isochronous
  Synch Type     Asynchronous
  wMaxPacketSize 0x0062 (98)
  bInterval      4
```

The earlier manual gadget enumerated as `1d6b:0105` on the same vhci bus and
also produced a `snd-usb-audio` card (`[lab] USB-Audio - Ozma UAC2 vudc lab`),
with the same ISO layout.

## 3. PCM attempt and measurements (FAIL)

The node created its `ozma` gadget with `hid.kbd`, `hid.mouse`, and
`uac2.audio`, bound it to `usbip-vudc.0`, and started the bridge:

```text
INFO ozma_node: USB gadget created and bound gadget="ozma" udc="usbip-vudc.0" functions=3
INFO ozma_node::usb_uac2: UAC2 bridge started; waiting for PCM to move instance="audio"
```

The test used the enumerated host card for the USB side and `snd-aloop` for
the device side:

```bash
# device-side sink monitor (host→device USB OUT must land here)
arecord -D hw:Loopback,1,0 -c 2 -r 48000 -f S16_LE -t wav monitor-out.wav &

# device-side capture feed (device→host USB IN source)
while true; do
  aplay -D hw:Loopback,0,1 -c 2 -r 48000 -f S16_LE -t wav tone_stereo.wav
done &

# host playback into the enumerated UAC2 card (USB OUT)
aplay -D hw:7,0 -c 2 -r 48000 -f S16_LE -t wav tone_stereo.wav

# host capture from the enumerated UAC2 card (USB IN)
arecord -D hw:7,0 -c 1 -r 48000 -f S16_LE -t wav in-capture.wav
```

The source tone is 440 Hz at 0.5 full scale, S16_LE, 48 kHz, 5 s.

Results:

| Direction | Command result | Measurement |
|---|---|---|
| USB OUT host→device | `aplay` rc 0 (ALSA buffer consumed) | `monitor-out.wav`: 1 434 192 frames, peak 0.00000, RMS 0.00000, 0 non-zero samples |
| USB IN device→host | `arecord` rc 1 | `pcm_read:2285: read error: Input/output error`; WAV is 44 bytes (header only) |

`usbmon` bus 8 (the vhci bus) shows the real completion status for every ISO
URB:

```text
# 4 999 completions
C Zo:005:03 -18 192 >          # USB OUT data, 2 ch S16 (196-byte max)
# 3 420 completions
C Zi:005:05 -18 98 = 0000...   # USB IN data, 1 ch S16
# 1 664 completions
C Zi:005:04 -18 4 = 0000       # USB IN feedback
```

`-18` is `-EXDEV`. The host controller submitted and completed the URBs, so
USB/IP and `vhci_hcd` pass isochronous URBs through; the virtual **device**
controller rejects them.

Kernel log also contains the vhci frame-number limitation whenever a host
audio stream starts:

```text
usb usb8: Not yet implemented
```

from `vhci_get_frame_number()` (`vhci_hcd.c` returns 0). This did not prevent
the URBs from being submitted, but it is a second ISO-timing risk on real
hardware.

### Node counters and status

Final `GET /usb` after the host PCM attempt:

```json
{
  "bridge": "configured",
  "uac2_bridge_error": {
    "kind": "uac2-endpoint",
    "message": "UAC2 endpoint 0x82 write failed: short isochronous write: 0 of 12 bytes"
  },
  "counters": {
    "capture_packets_out": 746,
    "capture_bytes_out": 8952,
    "capture_frames_out": 4476,
    "capture_short_packets": 377,
    "capture_underruns": 377,
    "empty_packets": 17325,
    "errors": 16579,
    "playback_packets_in": 0,
    "playback_bytes_in": 0,
    "sink_bytes": 0
  }
}
```

The capture counters describe bytes accepted by the gadget playback ALSA
buffer, not USB delivery; the host capture still produced 0 frames. No
`running` state was observed with the final code.

Before the status fix described below, the same run serialised as `running`
because those ALSA-side capture counters had moved. That was a false positive:
the endpoint error was already present and the host heard nothing.

## 4. Root cause — `usbip-vudc` ISO transfer is a TODO

The failure is in the virtual UDC transfer path, not in descriptor generation,
`vhci_hcd`, or the node bridge. The vudc transfer loop handles bulk, interrupt,
and control URB data, but its ISO case is an explicit stub (upstream
`drivers/usb/usbip/vudc_transfer.c`, inspected at v6.12; the installed kernel
behaves identically):

```c
case USB_ENDPOINT_XFER_ISOC:
    /* TODO: support */
    urb->status = -EXDEV;
    break;
```

`-EXDEV` maps to ALSA `EIO`, which is exactly what both `arecord` and the
gadget-side writes observed. The UDC advertises `type_iso = true` and
`afunc_bind()` succeeds, but the data path never completes an ISO request with
data. This is the same class of limitation as `dummy_hcd`, one layer lower: it
is not an Ozma regression and cannot be fixed in userspace.

Additional vudc instability observed during bind/attach:

```text
list_add double add: new=... prev=... next=...
WARNING: lib/list_debug.c:37 at __list_add_valid_or_report
  vep_queue+0x8e/0xa0 [usbip_vudc]
  usb_ep_queue+0x6b/0xd0 [udc_core]
  composite_setup+0xb01/0x23d0 [libcomposite]
  v_timer+0x3f2/0x830 [usbip_vudc]
```

It occurred three times in this lab (during bind/attach/build while a gadget
was present). It did not stop enumeration, but it taints the kernel and is a
second reason not to treat vudc as production audio hardware.

## 5. Product bugs found and fixed

The real path exposed four UAC2-only defects. All changes are confined to
`ozma-usb`, `ozma-audio`, and `ozma-node` UAC2 code.

### 5.1 ConfigFS `p_chmask`/`c_chmask` were inverted

`Uac2Config` names directions from the host's point of view (playback =
host→device = USB OUT). `f_uac2` names them from the gadget's point of view:

- `p_chmask` drives USB **IN** (device→host, host capture),
- `c_chmask` drives USB **OUT** (host→device, host playback).

Observed on the virtual UDC: `p_chmask=3`, `c_chmask=1` produced host
playback = 1 ch and host capture = 2 ch, the opposite of the requested
2-playback/1-capture model. A microphone (`capture_channels=1`) was written as
`p_chmask=0`, `c_chmask=1`, which makes a host playback sink, not a
microphone.

`ozma-usb/src/configfs.rs` now writes `p_chmask` from `capture_channels` and
`c_chmask` from `playback_channels`, and `read_uac2_status()` reads them back
with the same crossing. Tests in `ozma-usb/tests/uac2.rs` and
`ozma-usb/tests/configfs_controller.rs` were updated. After the fix the host
card reports the expected playback 2 ch / capture 1 ch.

### 5.2 Gadget capture PCM was looked up as `hw:N,1`

`u_audio` registers both directions on PCM device 0 of the gadget card. The
node assumed playback `hw:N,0` and capture `hw:N,1`, so the bridge failed with:

```text
UAC2 endpoint 0x01 open failed: cannot open UAC2 gadget ALSA device "hw:6,1":
No such file or directory (2)
```

`ozma-node/src/usb_audio.rs::find_uac2_capture_device` now uses `hw:N,0` unless
a real `/dev/snd/pcmCND1c` exists, and `usb_uac2.rs` uses that helper instead
of `replace(",0", ",1")`. This changed the startup failure from
`unsupported` to a started bridge.

### 5.3 Starting an empty ALSA playback buffer returned `EPIPE`

`AlsaUac2Sink`/`AlsaUac2EndpointIo` called `snd_pcm_start()` immediately after
`prepare()`. `snd-aloop` rejects starting an empty playback buffer:

```text
ALSA device "hw:Loopback,0,0" cannot provide start:
ALSA function 'snd_pcm_start' failed with error 'Broken pipe (32)'
```

Both paths now program `SwParams::set_start_threshold(1)` and let the first
`readi`/`writei` start the stream, matching the legacy `usb_audio` bridge
behaviour. This also avoids priming the USB gadget with a block of silence.

### 5.4 `/usb` could report `running` after a failed pump

A report with non-zero ALSA-side counters kept `status: running` even while the
last pump returned a typed endpoint error. Since ALSA can accept PCM into a
gadget buffer that never crosses USB, `Uac2BridgeShared::set_with_error` now
downgrades `running` to `configured` and retains the typed error. A successful
later pump restores `running`. New regression test:
`running_report_with_counters_is_downgraded_when_the_pump_failed`.

### Tests run

```text
cargo test -p ozma-usb
  all suites passed (incl. 5 UAC2 tests, 12 ConfigFS controller tests,
  14 USB/IP URB tests; 1 ignored)

cargo test -p ozma-audio --lib uac2
  8 passed, 0 failed

cargo test -p ozma-audio --test uac2_adapters
  7 passed, 0 failed

cargo build -p ozma-node --bin ozma-node
  ok
```

`cargo test -p ozma-node --lib` cannot currently compile because of
pre-existing, unrelated `ozma-node/src/video.rs` test initializers missing
`VideoParams` fields (`bitrate`, `codec`); that breakage predates this work.
The new `usb_uac2` regression test is therefore present but not run by the
suite in this tree.

## 6. Cleanup and final state

```bash
# node
kill -TERM <node-pid>
# USB/IP
usbip detach -p 00
kill <usbipd-pid>
# gadget: write "\n" to UDC, remove links, functions, strings, config, gadget
# modules loaded only for this lab
modprobe -r vhci_hcd
modprobe -r usbip-vudc
modprobe -r usbmon
modprobe -r usb_f_uac2          # also removes u_audio
modprobe -r usb_f_hid usb_f_mass_storage
echo snd_aloop.0 > /sys/bus/platform/drivers/snd_aloop/unbind
modprobe -r snd-aloop
# libcomposite/udc_core were pre-existing; libcomposite was restored after
# the usb_f_ removals released it
modprobe libcomposite
```

Final state:

```text
Sound cards:        0 PCH, 1 Generic, 2 Generic_1, 3 C4K, 4 Rubix22
                    (exactly the pre-existing user cards; no Loopback/UAC2)
/sys/class/udc:     empty
ConfigFS gadgets:   none (/sys/kernel/config/usb_gadget empty)
vhci/vudc platform: none
usbipd:             none
usbip port:         "is vhci_hcd loaded?" (expected)
Modules remaining:  libcomposite, udc_core, usbip_host, usbip_core
                    (all pre-existing)
Temporary data:     /tmp/opencode/uac2-vudc removed
```

The pre-existing USB cards (`C4K`, `Rubix22`) and the user's default
sink/source were not used or changed.

## 7. Remaining gaps

1. **No ISO-capable virtual UDC on this host.** `dummy_hcd` has no ISO
   endpoints and `usbip-vudc` returns `-EXDEV` for ISO URBs. Host-visible UAC2
   PCM still requires a real UDC/controller, a patched vudc, or another
   virtual UDC that implements ISO data transfers. This is the only blocker
   left between the current code and a full host↔device UAC2 PCM run.
2. **`vhci_get_frame_number()` is unimplemented.** It logs
   `Not yet implemented` and returns 0 for every query. That is a scheduling
   risk for real isochronous audio once the UDC side works.
3. **vudc list-corruption warning.** `vep_queue()` produced a
   `list_add double add` warning during gadget setup. It is a kernel/vudc
   issue, but it means this stack is not a production test bed.
4. **Node shutdown leaves the ConfigFS gadget bound.** SIGTERM stopped the
   bridge and node cleanly, but the `ozma` gadget remained bound until the
   lab cleanup ran. This is not UAC2-specific, but it means operators must
   unbind after a node stop on a vudc/real UDC.
5. **The bridge's endpoint addresses are model constants.** `/usb` reports
   `0x01`/`0x82`, while the composite kernel gadget used `0x03`/`0x85` plus a
   `0x84` feedback endpoint because HID functions consumed the lower
   endpoints. This does not affect the ALSA data path, but the reported
   endpoint numbers are not discovered from the actual descriptor.
6. **Per-pump error logging is unbounded.** A persisting endpoint error logs
   one warning per 1 ms pump (the lab log reached about 113 MB in a few
   minutes). A rate-limited or change-triggered log would be safer for long
   runs.
7. **Only S16 / 48 kHz / 2 playback + 1 capture is wired through the node.**
   The adapters support S24/S32 and more channels, but no CLI selects them and
   no hardware run has exercised them.
