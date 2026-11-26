# Phase 4 — opt-in 1-Wire, I²C, SPI, GPIO, and SocketCAN bus report

Status: **implemented, wired, opt-in, and bounded**. The bus module is
`ozma-node/src/buses/` (`mod.rs`, `onewire.rs`, `i2c.rs`, `spi.rs`, `gpio.rs`,
`can.rs`). It adds explicit allowlists, typed errors, and read/write/transfer/
capture surfaces for the five protocols without auto-detecting any device.

The CLI, `ConnectionState`, router mount, and `buses` capability advertisement
are wired:

- `main.rs` builds `BusConfig::from_cli_full(...)` and `BusState::new(...)`;
- `controller_compat::ConnectionState::with_buses` carries the state;
- `controller_compat::compat_router` merges `crate::buses::router(...)` behind
  the node bearer guard;
- the `buses` capability is true when any of the five protocols is configured.

## Configuration model

`BusConfig` is validated before any route can open a device or run a CLI tool.
`BusState::new(config)` uses the real Linux backends; `BusState::with_backends`
injects deterministic test/fake backends. `BusState::default()` is valid and
enables nothing.

| CLI flag | Config field | Default | Validation |
|---|---|---|---|
| `--onewire` | `onewire` | unset | Enables the read-only 1-Wire sysfs surface |
| `--onewire-root PATH` | `onewire.root` | `/sys/bus/w1/devices` | Absolute, no `.`/`..` |
| `--i2c-allow BUS:ADDR,...` | `i2c.allow` | empty | Exact pairs only; bus 0–65535; address `0x08..=0x77` |
| `--spi-allow /dev/spidevX.Y` | `spi.devices` | empty | Absolute `/dev/spidevX.Y` only |
| `--gpio-allow CHIP:LINE[:input\|output]` | `gpio.allow` | empty | Exact chip/line; default input; output only when spelled |
| `--can-allow IFACE[:send]` | `can.allow` | empty | Exact interface; `:send` opts that interface into TX |
| `--can-send-allow IFACE` | `can.allow[*].send` | empty | Names must already be in `--can-allow` |
| `--can-max-frames N` | `max_can_frames` | 128 | 1–1024 per interface |
| `--bus-max-transfer-bytes N` | `max_transfer_bytes` | 4096 | 1–65,536 |
| `--bus-max-block-bytes N` | `max_block_bytes` | 32 | 1–32 (SMBus block limit) |
| `--bus-max-devices N` | `max_devices` | 64 | 1–256 (also caps GPIO lines/CAN interfaces) |

`BusConfig::from_cli_full(onewire, i2c, spi, gpio, can, can_send, ...)` is the
node startup adapter. `BusConfig::from_cli`/`from_cli_with_limits` remain for
callers that only use the original three protocols. `onewire` follows clap bool
semantics: `None` disables the surface, `Some("")` or `Some("true")` selects the
default sysfs root, and any other string is an explicit root (for
containers/tests). Each allowlist string may contain comma- or whitespace-
separated entries. I²C addresses accept decimal or `0x` hex; GPIO line offsets
accept decimal or `0x` hex.

GPIO `CHIP` accepts `gpiochipN`, `/dev/gpiochipN`, or a bare numeric `N` and is
normalized to `/dev/gpiochipN`. A line defaults to `input`; a duplicate line
with two different directions is rejected. CAN `IFACE` is 1–15 ASCII
letters/digits/`_-.`; `:send` is the only accepted suffix. `--can-send-allow`
can narrow but never widen `--can-allow`.

The router deliberately contains **no authentication logic**. It remains
inside the existing node bearer guard. With an unconfigured `BusState`, every
route returns a typed `bus.not-configured` 503; `GET /buses/status` remains a
200 summary with all `configured` flags false.

Mount line (already present in `controller_compat::compat_router`):

```rust
.merge(crate::buses::router(state.buses.clone()))
```

## Routes

All routes are node routes and are meant to sit behind the bearer guard.

| Route | Method | Behaviour |
|---|---|---|
| `/buses/status` | GET | 200 summary: per-protocol configured/available facts and advertised bounds. Never probes I²C or SPI. |
| `/buses/1wire/devices` | GET | Lists valid `family-id` directories, sorted, capped by `max_devices`; includes `family`, optional kernel `name`. |
| `/buses/1wire/{id}` | GET | One validated id: family, `name`, `w1_slave_available`, `crc_valid`, `temperature_millicelsius`, `temperature_c`, and bounded/sanitized `raw_lines`. |
| `/buses/i2c` | GET | Allowlisted buses, `/dev/i2c-N` existence, and exact addresses. No scan. |
| `/buses/i2c/{bus}/detect` | GET | Bounded `i2cdetect -y -r BUS` quick scan; only allowlisted buses run and only allowlisted addresses are returned. |
| `/buses/i2c/{bus}/{addr}/read` | POST | SMBus `byte`/`word`/`block` read. |
| `/buses/i2c/{bus}/{addr}/write` | POST | SMBus `byte`/`word`/`block` write. |
| `/buses/spi` | GET | Allowlisted `/dev/spidevX.Y` nodes and existence. No open. |
| `/buses/spi/{device}/transfer` | POST | Full-duplex hex TX/RX; mode 0–3; 8/16 bits; bounded speed and length. |
| `/buses/spi/dev/{device}/transfer` | POST | Alias for callers that spell the device as `dev/spidevX.Y`. |
| `/buses/gpio` | GET | Allowlisted exact chip/line pairs, configured direction, `/dev/gpiochipN` existence. No chip open. |
| `/buses/gpio/{chip}/{line}` | GET | Kernel direction/name/consumer/flags, active-low, open-drain/source, value for an idle input. |
| `/buses/gpio/{chip}/{line}/set` | POST | Drive 0/1; optional bounded pulse (1–5000 ms) that restores the sampled value. Output allowlist required. |
| `/buses/can` | GET | Allowlisted interfaces, `/sys/class/net` existence, send opt-in, capture buffer bound. No socket. |
| `/buses/can/{iface}/status` | GET | `ip -details -statistics -json`: operstate, MTU, kind, bitrate, controller state, error counters, link stats. |
| `/buses/can/{iface}/frames?limit=N` | GET | Bounded recent capture (default buffer 128, absolute 1024); starts one receive socket lazily. |
| `/buses/can/{iface}/send` | POST | Classic/FD data, standard/extended ID, explicit RTR; requires `:send` or `--can-send-allow`. |

Axum 0.7 uses `:param` internally; the docs above use the controller-facing
`{param}` notation. SPI `{device}` accepts `spidev0.0`, `0.0`, `/dev/spidev0.0`,
or `dev/spidev0.0`, always normalized to the exact allowlisted
`/dev/spidevX.Y`. GPIO `{chip}` accepts `gpiochipN`, `/dev/gpiochipN`, or `N`;
`{line}` is a decimal/`0x` offset. CAN `{iface}` is the exact interface name.

### I²C request bodies

Read (`POST /buses/i2c/1/0x50/read`):

```json
{"op":"byte","register":16}
{"op":"word","register":17}
{"op":"block","register":18,"length":3}
```

Write (`POST /buses/i2c/1/0x50/write`):

```json
{"op":"byte","register":16,"value":90}
{"op":"word","register":17,"value":4660}
{"op":"block","register":18,"data":"01 02 03"}
```

- `byte` may omit `register` for the no-register `read_byte`/`write_byte`
  transaction.
- `word` and `block` require `register`.
- `block` reads require `length` (1–`max_block_bytes`); block writes require
  non-empty bounded hex `data`.
- Unknown fields, wrong field combinations, out-of-range values, and oversized
  hex are `bus.invalid-request` before the backend is called.
- Read responses include the exact `data` hex, `bytes`, `register`, and (for
  `byte`/`word`) the assembled `value`. Word values are little-endian on the
  SMBus wire.
- Write responses include `bytes` written.

### SPI request body

```json
{"tx":"de ad be ef","mode":3,"speed_hz":500000,"bits":8}
```

`tx` is required and bounded by `max_transfer_bytes` (default 4 KiB, absolute
64 KiB). `mode` defaults to 0 and must be 0–3. `speed_hz` defaults to 1 MHz and
must be 1 kHz–50 MHz. `bits` defaults to 8 and must be 8 or 16; 16-bit transfers
require an even number of TX bytes. The response returns the exact `rx` hex and
byte count. RX length must equal TX length.

### GPIO request body

```json
{"value":1}
{"value":1,"pulse":{"duration_ms":25}}
```

`value` must be 0 or 1. `pulse.duration_ms` must be 1–5000 ms. The line must be
allowlisted as `:output`; input lines are read-only and return `bus.not-allowed`
before the backend is called. A pulse samples the current value, drives the
requested value for the bounded duration, and restores the sampled value. The
response reports `value`, `pulsed`, `duration_ms`, `previous_value`, and
`restored_value`. Writing the same line concurrently returns `bus.busy` rather
than racing.

`GET /buses/gpio/{chip}/{line}` never re-requests an output line as an input;
its `value` is omitted for output or kernel-owned lines. `active_low`,
`open_drain`, and `open_source` come from the kernel line info. The GPIO v1
character-device ABI does not expose bias, so `bias` is omitted rather than
invented.

### CAN request body

```json
{"id":291,"data":"de ad be ef"}
{"id":291,"extended":false,"fd":true,"brs":true,"data":"00112233"}
{"id":291,"rtr":true,"dlc":4}
```

- `id` must be ≤ `0x7ff` standard or ≤ `0x1fff_ffff` extended (`extended:true`).
- Classic data is at most 8 bytes; `fd:true` allows up to 64. Hex accepts
  spaces/`:`/`-`/`_` and an optional `0x`.
- `rtr:true` sends an explicit remote frame; `data` must be empty and `dlc`
  (0–8) is the requested length. RTR and FD are mutually exclusive.
- `brs:true` requires `fd:true`. `dlc` is rejected without `rtr:true`.
- Send requires `--can-allow IFACE:send` or `--can-send-allow IFACE`; a listed
  read-only interface returns `bus.not-allowed` without opening a send socket.
- One send socket per interface is in flight at a time; a concurrent send gets
  `bus.busy`. No bus-off or controller restart operation exists.

`GET /buses/can/{iface}/frames` starts one bounded receive socket for that exact
interface on first use. The buffer keeps the most recent `--can-max-frames`
(default 128, absolute 1024); `?limit=N` slices the latest N and reports
`matched`, `truncated`, `dropped`, and `total`. `last_error` is non-null when a
capture socket later failed.

### 1-Wire detail parsing

`w1_slave` is read with a hard 4 KiB cap (also capped by `max_transfer_bytes`).
Only the first 8 lines are emitted, each sanitized and truncated to 256
characters. `crc_valid` is `true`/`false` from the kernel's `crc=... YES|NO`
token and `null` when absent. `temperature_millicelsius` is parsed from `t=...`
and `temperature_c` is that value divided by 1000. A valid device directory with
no `w1_slave` is a 200 response with `w1_slave_available:false` and null
temperature/CRC, not an invented reading.

## Safety model

- **No default bus or device.** I²C requires an exact `BUS:ADDR` pair; SPI
  requires an exact `/dev/spidevX.Y`; GPIO requires an exact `CHIP:LINE`; CAN
  requires an exact interface name; 1-Wire requires the `--onewire` opt-in.
  There is no implicit bus 0, chip 0, interface, glob, or fallback device.
- **GPIO is line-scoped.** No chip-wide request, export, scan, or line
  enumeration exists. A chip/line is checked against the allowlist before
  `/dev/gpiochipN` is opened. The line offset is then checked against the
  kernel's line count. Input is the default; only an explicit `:output` can be
  driven. A line the node has driven is kept requested for the life of the
  process, so `set` is a latched output and a later pulse restores the value
  that was actually driven. `set`/`pulse` are serialized per line and a GET
  never turns an output line into an input.
- **CAN is read-only unless opted in.** `--can-allow IFACE` permits only status
  and capture. Transmission needs `:send` or `--can-send-allow IFACE`, and the
  separate send list cannot name an interface absent from `--can-allow`. The
  node never runs `ip link set`, never restarts a controller, and never
  enumerates interfaces. Status shells out to `ip -details -statistics -json`
  with a 2 s deadline and bounded output.
- **Bounded CAN capture.** One receive socket per allowlisted interface,
  started lazily; the buffer is capped at `--can-max-frames` (128 default,
  1024 absolute). Outbound sockets are capped at one in flight per interface.
  `?limit=N` is capped at the absolute frame bound and cannot over-read.
- **Allowlist before any I/O.** I²C read/write checks the pair before opening
  `/dev/i2c-N`; detect checks the bus before spawning `i2cdetect`; SPI checks the
  normalized path before `Spidev::open`; 1-Wire validates the id before joining
  it to the root.
- **No broadcast/reserved I²C writes.** `0x00` (general call/broadcast),
  `0x01..=0x07`, and `0x78..=0x7f` are rejected at configuration and request
  time. This applies to reads as well as writes.
- **No default probing.** `GET /buses/status`, `GET /buses/i2c`, and
  `GET /buses/spi` only read configuration, stat `/dev` nodes, and optionally
  list 1-Wire sysfs. They never scan I²C buses or open SPI devices.
- **Bounded scans.** `i2cdetect -y -r` runs only for an allowlisted bus with a
  2 s default / 10 s maximum deadline. The child is killed on expiry and output
  is read with a 64 KiB cap. Only allowlisted addresses are returned.
- **Bounded payloads.** I²C blocks are at most 32 bytes; SPI transfers are at
  most `max_transfer_bytes` (default 4 KiB, absolute 64 KiB); request bodies are
  capped before JSON extraction; listings and allowlists are capped by
  `max_devices`.
- **Path validation.** 1-Wire ids must match exactly two hex family digits,
  `-`, and twelve hex id digits. SPI paths must be absolute direct children of
  `/dev` named `spidevX.Y` with digits. Neither path accepts `.`/`..`, extra
  components, or non-numeric route segments.
- **Kernel sysfs links are intentional.** Real `/sys/bus/w1/devices/<id>`
  entries are symlinks to the device directory. The id basename is validated
  first, then the kernel-owned link is followed. No user-controlled path
  component is joined to the root.
- **No auth in this module.** All routes are designed to be merged behind the
  existing bearer guard, like `/serial/*` and `/host_surfaces`.

## Typed errors

Bodies use `{ok:false,supported,configured,available:false,code,error,remediation}`.

| Code | HTTP | Meaning |
|---|---|---|
| `bus.not-configured` | 503 | No allowlist/flag enables this protocol |
| `bus.unavailable` | 503 | Configured bus/device/sysfs entry is absent, unreadable, or no longer present |
| `bus.not-allowed` | 403 | Exact bus/address/path/GPIO line/CAN interface is not allowlisted or not send-enabled |
| `bus.permission-denied` | 403 | Kernel denied the GPIO chip/device (group/ACL) |
| `bus.busy` | 409 | GPIO line write/pulse or CAN send already in flight; kernel `EBUSY` |
| `bus.invalid-request` | 400 | Malformed id/path/JSON/operation or a bound violation |
| `bus.io-failed` | 502 | Kernel/backend I/O failed after the request was accepted |
| `bus.unsupported` | 501 | Non-Linux backend or missing `i2cdetect` |
| `bus.timeout` | 504 | Bounded `i2cdetect`/backend operation deadline expired |

Reserved/broadcast I²C addresses are `bus.invalid-request`; an unlisted but
otherwise valid target is `bus.not-allowed` and never reaches the backend.

## Dependencies

`ozma-node/Cargo.toml` adds four safe wrappers under
`[target.'cfg(target_os = "linux")'.dependencies]`:

- `i2cdev = "0.6.2"` for Linux `i2c-dev`/SMBus ioctls.
- `spidev = "0.7.1"` for Linux spidev ioctls.
- `gpio-cdev = "0.6.0"` for the GPIO v1 character-device ABI.
- `socketcan = { version = "4.0.0", default-features = false }` for raw
  SocketCAN sockets (netlink/dump features are not used).

No `unsafe` is written in this module. Non-Linux builds use cfg-gated real
backends that return `bus.unsupported`; the portable model, router, validation,
and fake-backend tests still compile.

## Tests

`cargo test -p ozma-node --lib buses` — 49 passed, 3 ignored (live i2c-stub,
gpio-sim, and vcan).

Coverage:

- **1-Wire:** strict id validation/traversal rejection; `w1_slave` CRC and
  positive/negative temperature parsing; raw-line sanitization and bounds;
  sorted/deduped/truncated fixture listing; kernel-style symlinked device
  directories; detail with/without `w1_slave`; malformed id, missing device,
  missing root, and unconfigured errors; status summary.
- **I²C:** allowlist parsing/deduplication and reserved-address rejection;
  route-id parsing; `i2cdetect` output parsing including ignored `UU` cells;
  listing without probing; detect bus allowlist and address filtering; byte/
  word/block read/write normalization; exact allowlist enforcement with no
  backend call on rejection; reserved-address write rejection; malformed body
  matrix; backend timeout/io/unsupported mapping; unconfigured behavior.
- **SPI:** exact absolute `/dev/spidevX.Y` allowlist; route normalization
  (`spidev0.0`, `0.0`, `/dev/...`, `dev/...`); listing without opening; transfer
  mode/speed/bits/length bounds; exact RX response; unlisted-device rejection
  before backend; backend unavailable/timeout mapping; unconfigured behavior.
- **GPIO:** exact `CHIP:LINE[:input|output]` parsing/normalization, default
  input, conflicting duplicate rejection, route chip/line normalization,
  listing without opening chips, kernel facts, input-only GET values, output
  GET never re-requesting input, output-required set/pulse, 0/1 and 1–5000 ms
  bounds, pulse restore shape, unlisted/invalid rejection before backend,
  busy/permission mapping, unconfigured behavior.
- **CAN:** exact interface parsing and `:send` opt-in, separate send allowlist
  narrowing, route list without sockets, status shape, send payload/ID/FD/RTR/
  BRS/DLC bounds, FD/extended normalization, `?limit` and buffer bounds, frames
  metadata, read-only send rejection, busy/permission mapping, `ip` JSON
  parsing without inventing absent fields, unconfigured behavior.
- **Router/config:** default/unconfigured state, full CLI parsing including
  GPIO/CAN, transfer/block/device/frame bounds, route presence, 404/405 shapes,
  typed `bus.not-configured` for every implemented route.

### Live i2c-stub evidence

The virtual adapter was created only with `i2c-stub`, never a GPU DDC/real bus:

```text
$ sudo modprobe i2c-stub chip_addr=0x50
$ cat /sys/bus/i2c/devices/i2c-22/name
SMBus stub driver
$ sudo i2cset -y 22 0x50 0x10 0x5a
$ sudo i2cget -y 22 0x50 0x10
0x5a
$ sudo i2cset -y 22 0x50 0x11 0x1234 w
$ sudo i2cget -y 22 0x50 0x11 w
0x1234
$ sudo i2cset -y 22 0x50 0x12 0x01 0x02 0x03 i
$ sudo i2cget -y 22 0x50 0x12 i 3
0x01 0x02 0x03
$ sudo i2cdetect -y -r 22
50: 50 -- -- -- -- -- -- -- -- -- -- -- -- -- -- --
```

The real backend test (`live_i2c_stub_real_backend_read_write_detect`, ignored by
default) was run against that bus:

```text
$ sudo env OZMA_LIVE_I2C_BUS=22 OZMA_LIVE_I2C_ADDR=0x50 \
    ./target/debug/deps/ozma_node-<hash> \
    --exact buses::i2c::tests::live_i2c_stub_real_backend_read_write_detect \
    --ignored --nocapture
running 1 test
test buses::i2c::tests::live_i2c_stub_real_backend_read_write_detect ... ok
test result: ok. 1 passed; 0 failed; 0 ignored
```

It wrote/read a byte (register `0x10`, `0x5a`), a word (register `0x11`,
`0x1234`), a 3-byte I²C block (register `0x12`), and ran `detect`, which
returned only the allowlisted `0x50`.

Cleanup:

```text
$ sudo modprobe -r i2c_stub
$ ls /dev/i2c-22
ls: cannot access '/dev/i2c-22': No such file or directory
```

### Live gpio-sim evidence

A virtual `gpio-sim` bank was created explicitly; no physical GPIO line was
touched:

```text
$ sudo modprobe gpio-sim
$ sudo mkdir /sys/kernel/config/gpio-sim/ozma-test
$ sudo mkdir /sys/kernel/config/gpio-sim/ozma-test/gpio-bank0
$ echo 4 | sudo tee /sys/kernel/config/gpio-sim/ozma-test/gpio-bank0/num_lines
$ sudo mkdir /sys/kernel/config/gpio-sim/ozma-test/gpio-bank0/line0
$ echo ozma-sim-line0 | sudo tee /sys/kernel/config/gpio-sim/ozma-test/gpio-bank0/line0/name
$ echo 1 | sudo tee /sys/kernel/config/gpio-sim/ozma-test/live
$ ls -l /dev/gpiochip2
crw------- 1 root root 254, 2 ... /dev/gpiochip2
```

The ignored real-backend test (`live_gpio_sim_set_pulse_read`) was run against
`gpiochip2` line 0:

```text
$ sudo env OZMA_LIVE_GPIO_CHIP=gpiochip2 OZMA_LIVE_GPIO_LINE=0 \
    ./target/debug/deps/ozma_node-<hash> \
    --exact buses::gpio::tests::live_gpio_sim_set_pulse_read \
    --ignored --nocapture
running 1 test
test buses::gpio::tests::live_gpio_sim_set_pulse_read ... ok
test result: ok. 1 passed; 0 failed; 0 ignored
```

It read the kernel line detail, drove the line to 1, ran a 20 ms pulse that
reported the sampled previous value and restored it, read the restored value
back through the input side of the same virtual line, and listed the configured
pair.

Cleanup:

```text
$ echo 0 | sudo tee /sys/kernel/config/gpio-sim/ozma-test/live
$ sudo rmdir /sys/kernel/config/gpio-sim/ozma-test/gpio-bank0/line0
$ sudo rmdir /sys/kernel/config/gpio-sim/ozma-test/gpio-bank0
$ sudo rmdir /sys/kernel/config/gpio-sim/ozma-test
$ sudo modprobe -r gpio_sim
$ ls /dev/gpiochip2
ls: cannot access '/dev/gpiochip2': No such file or directory
```

### Live vcan evidence

A virtual `vcan0` interface was created explicitly; no CAN hardware was touched:

```text
$ sudo modprobe vcan
$ sudo ip link add dev vcan0 type vcan
$ sudo ip link set up vcan0
$ ip -details -statistics -json link show dev vcan0
[{"ifname":"vcan0",...,"operstate":"UNKNOWN","linkinfo":{"info_kind":"vcan"},...}]
```

The ignored real-backend test (`live_vcan_send_capture_status`) was run against
that interface:

```text
$ sudo env OZMA_LIVE_CAN_IFACE=vcan0 \
    ./target/debug/deps/ozma_node-<hash> \
    --exact buses::can::tests::live_vcan_send_capture_status \
    --ignored --nocapture
running 1 test
test buses::can::tests::live_vcan_send_capture_status ... ok
test result: ok. 1 passed; 0 failed; 0 ignored
```

It read `status` via `ip` (available, `kind: vcan`, operstate accepted as
`UNKNOWN` because vcan reports that even when its link flags are UP), started
the bounded capture, sent a classic frame (`0x123`, `deadbeef`) and an extended
frame (`0x1abcdef`, `010203`), then captured both exact frames from the receive
socket before the deadline.

Cleanup:

```text
$ sudo ip link del dev vcan0
$ sudo modprobe -r vcan
$ ip link show vcan0
Device "vcan0" does not exist.
```

No `spidev` or 1-Wire hardware exists on this host (`/dev/spidev*` and
`/sys/bus/w1/devices/` are absent), so live 1-Wire/SPI evidence is the typed
`bus.unavailable` path exercised by the fixture/fake tests. No virtual spidev or
w1 module was loaded. Real GPIO lines (`gpiochip0/1`) and physical CAN
interfaces were never opened or modified.

## Remaining gaps

- Controller proxies are not added. The routes are node-only.
- GPIO uses the v1 character-device ABI (`gpio-cdev 0.6`). It reports
  active-low/open-drain/open-source but not bias, and it cannot observe an
  output line's value without briefly re-requesting it as an input, so GET
  deliberately omits output values. There is no edge-event/stream surface and
  no bias configuration.
- GPIO pulses are implemented in-process with `sleep`; the kernel does not arm
  a one-shot edge. Driven output handles are held for the process lifetime and
  there is no explicit release route; process exit releases them. A crash or
  kill during a pulse closes the handle, which releases the line but does not
  run the in-process restore.
- CAN capture uses a userspace timestamp at read time, not kernel/hardware
  RX timestamps. There is no filtering, error-frame subscription, BCM, ISO-TP,
  or J1939 surface.
- CAN FD transmission is implemented but the live virtual evidence covers
  classic and extended classic frames; FD requires an FD-capable vcan MTU and
  is covered by backend-shape/fake tests on this host.
- CAN status depends on `iproute2` (`ip -details -statistics -json`); the crate
  is built without socketcan's netlink feature, so there is no in-process
  bitrate/state query.
- I²C supports only 7-bit user addresses, SMBus byte/word/block transactions,
  and no PEC, 10-bit addressing, SMBus process calls, or raw `I2C_RDWR`
  combined transfers.
- I²C detection shells out to `i2cdetect`; there is no in-process scan and no
  per-address identify operation.
- SPI is one full-duplex `SPI_IOC_MESSAGE` per request; no multi-segment
  transfers, explicit CS changes, LSB-first flag, dual/quad modes, or partial
  reads independent of TX length.
- 1-Wire is read-only: no write/strong-pullup, parasite-power control, alarm
  search, or event/stream surface.
- No bus device hotplug/event stream; availability is reported by stat per
  request.
- Live tests are ignored by default and require the operator to create the
  stub bus / gpio-sim chip / vcan interface; they do not auto-load modules or
  alter hardware.
