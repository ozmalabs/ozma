# Phase 4 — opt-in 1-Wire, I²C/SMBus/PMBus, SPI, GPIO, SocketCAN, IIO, PWM, and DDC/CI bus report

Status: **implemented, wired, opt-in, and bounded**. The bus module is
`ozma-node/src/buses/` (`mod.rs`, `onewire.rs`, `i2c.rs`, `spi.rs`, `gpio.rs`,
`can.rs`, `iio.rs`, `pwm.rs`, `ddc.rs`, `pmbus.rs`). It adds explicit
allowlists, typed errors, and read/write/transfer/capture surfaces for the
protocols without auto-detecting any device.

The CLI, `ConnectionState`, router mount, and `buses` capability advertisement
are wired:

- `main.rs` builds `BusConfig::from_cli_all(...)` and `BusState::new(...)`;
- `controller_compat::ConnectionState::with_buses` carries the state;
- `controller_compat::compat_router` merges `crate::buses::router(...)` behind
  the node bearer guard;
- the `buses` capability is true when any protocol (including IIO, PWM, or
  DDC/CI) is configured. PMBus uses the I²C allowlist and adds no separate
  capability.

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
| `--iio-allow DEVICE:CHANNEL,...` | `iio.allow` | empty | Exact pairs; `DEVICE` normalizes to `iio:deviceN`; read-only |
| `--iio-root PATH` | `iio.root` | `/sys/bus/iio/devices` | Absolute, no `.`/`..` |
| `--pwm-allow CHIP:CHANNEL[:export]` | `pwm.allow` | empty | Exact pairs; `CHIP` normalizes to `pwmchipN`; `:export` is the only creation opt-in |
| `--pwm-root PATH` | `pwm.root` | `/sys/class/pwm` | Absolute, no `.`/`..` |
| `--ddc-allow BUS:ADDR[:write]` | `ddc.allow` | empty | Exact pairs; `ADDR` must be `0x37`; every pair must also be in `--i2c-allow`; `:write` opts into VCP writes |
| `--i2c-pec` | `i2c.pec` | false | Enables SMBus PEC; the adapter must advertise it or the transaction is typed `bus.unsupported` |
| `--bus-max-transfer-bytes N` | `max_transfer_bytes` | 4096 | 1–65,536 |
| `--bus-max-block-bytes N` | `max_block_bytes` | 32 | 1–32 (SMBus block limit) |
| `--bus-max-devices N` | `max_devices` | 64 | 1–256 (also caps GPIO lines/CAN interfaces) |

`BusConfig::from_cli_all(onewire, i2c, spi, gpio, can, can_send, iio,
iio_root, pwm, pwm_root, ddc, i2c_pec, ...)` is the node startup adapter.
`BusConfig::from_cli_full` remains as a compatibility wrapper for callers that
predate IIO/PWM/DDC, and `BusConfig::from_cli`/`from_cli_with_limits` remain for
callers that only use the original three protocols. `onewire` follows clap bool
semantics: `None` disables the surface, `Some("")` or `Some("true")` selects the
default sysfs root, and any other string is an explicit root (for
containers/tests). IIO/PWM roots are normal optional paths; `None` keeps the
kernel default. Each allowlist string may contain comma- or whitespace-
separated entries. I²C addresses accept decimal or `0x` hex; GPIO line offsets,
IIO device numbers, and PWM channels accept decimal or `0x` hex where numeric.

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
| `/buses/status` | GET | 200 summary: per-protocol configured/available facts and advertised bounds. Never probes I²C, SPI, IIO, PWM, or DDC. |
| `/buses/1wire/devices` | GET | Lists valid `family-id` directories, sorted, capped by `max_devices`; includes `family`, optional kernel `name`. |
| `/buses/1wire/{id}` | GET | One validated id: family, `name`, `w1_slave_available`, `crc_valid`, `temperature_millicelsius`, `temperature_c`, and bounded/sanitized `raw_lines`. |
| `/buses/i2c` | GET | Allowlisted buses, `/dev/i2c-N` existence, and exact addresses. No scan. |
| `/buses/i2c/{bus}/detect` | GET | Bounded `i2cdetect -y -r BUS` quick scan; only allowlisted buses run and only allowlisted addresses are returned. |
| `/buses/i2c/{bus}/{addr}/read` | POST | SMBus `byte`/`word`/`block`/`smbus-block` read, with optional PEC. |
| `/buses/i2c/{bus}/{addr}/write` | POST | SMBus `byte`/`word`/`block`/`smbus-block` write, with optional PEC. |
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
| `/buses/pmbus/{bus}/{addr}` | GET | Best-effort PMBus identification for an address already in `--i2c-allow`: revision, MFR strings, raw VOUT/IOUT/temperature, and linear-format VOUT volts when available. Typed `bus.unavailable` when the revision register does not answer. No writes. |
| `/buses/iio` | GET | Allowlisted IIO devices/channels and device-directory existence. No channel read and no write. |
| `/buses/iio/{device}` | GET | Device `name` plus each allowlisted channel's raw/scale/offset/processed/sampling/type facts. |
| `/buses/iio/{device}/{channel}` | GET | One channel's bounded facts. Missing optional files are `null`; a channel with no raw/input file is typed `bus.unavailable`. |
| `/buses/pwm` | GET | Allowlisted PWM chip/channel pairs, export opt-in, chip/channel existence, and current values for already-exported channels. Never exports. |
| `/buses/pwm/{chip}/{channel}` | GET | Current `period_ns`, `duty_ns`, `enabled`; typed `bus.unavailable` when the channel is not exported. |
| `/buses/pwm/{chip}/{channel}/set` | POST | Bounded period/duty/enable writes. Requires the exact allowlisted pair; a missing channel additionally requires `:export`. |
| `/buses/ddc` | GET | Allowlisted DDC/CI bus/address pairs, `/dev/i2c-N` existence, and write opt-in. No I/O. |
| `/buses/ddc/{bus}/{addr}` | GET | DDC/CI target facts and the three supported VCP codes. |
| `/buses/ddc/{bus}/{addr}/vcp/{code}` | GET | Bounded VCP read for `0x10` brightness, `0x12` contrast, or `0x60` input source. |
| `/buses/ddc/{bus}/{addr}/vcp/{code}/set` | POST | Bounded VCP write; requires `:write` on the exact `--ddc-allow` pair. |
| `/buses/ddc/{bus}/{addr}/capabilities` | GET | One bounded (32-byte) DDC/CI capabilities fragment, sanitized to printable ASCII. |

Axum 0.7 uses `:param` internally; the docs above use the controller-facing
`{param}` notation. SPI `{device}` accepts `spidev0.0`, `0.0`, `/dev/spidev0.0`,
or `dev/spidev0.0`, always normalized to the exact allowlisted
`/dev/spidevX.Y`. GPIO `{chip}` accepts `gpiochipN`, `/dev/gpiochipN`, or `N`;
`{line}` is a decimal/`0x` offset. CAN `{iface}` is the exact interface name.
IIO `{device}` accepts `iio:deviceN`, `deviceN`, or `N`; `{channel}` is a
letters/digits/`_`/`-` name. PWM `{chip}` accepts `pwmchipN`,
`/sys/class/pwm/pwmchipN`, or `N`; `{channel}` is a decimal/`0x` number.
DDC `{bus}`/`{addr}` accept decimal or `0x` hex and `{addr}` must be `0x37`.

### I²C request bodies

Read (`POST /buses/i2c/1/0x50/read`):

```json
{"op":"byte","register":16}
{"op":"word","register":17}
{"op":"block","register":18,"length":3}
{"op":"smbus-block","register":0x20,"length":4}
```

Write (`POST /buses/i2c/1/0x50/write`):

```json
{"op":"byte","register":16,"value":90}
{"op":"word","register":17,"value":4660}
{"op":"block","register":18,"data":"01 02 03"}
{"op":"smbus-block","register":0x21,"data":"aa bb"}
```

- `byte` may omit `register` for the no-register `read_byte`/`write_byte`
  transaction.
- `word`, `block`, and `smbus-block` require `register`.
- `block` is the SMBus I²C-block form and requires `length`
  (1–`max_block_bytes`). `smbus-block` is the count-prefixed SMBus block form;
  `length` is optional and acts as an upper bound. Block writes require
  non-empty bounded hex `data`.
- `--i2c-pec` enables SMBus Packet Error Checking on every allowlisted
  transaction. Before the first PEC transaction on a bus, the node checks
  adapter functionality with `i2cdetect -F` (capability query only, no device
  addressing). If the adapter does not advertise SMBus PEC, the request is
  typed `bus.unsupported` rather than silently degrading to a non-PEC
  transaction. The response reports `pec:true|false`.
- Unknown fields, wrong field combinations, out-of-range values, and oversized
  hex are `bus.invalid-request` before the backend is called.
- Read responses include the exact `data` hex, `bytes`, `register`, `pec`, and
  (for `byte`/`word`) the assembled `value`. Word values are little-endian on
  the SMBus wire.
- Write responses include `bytes` written and `pec`.

### PMBus identification

`GET /buses/pmbus/{bus}/{addr}` accepts only an exact address already present in
`--i2c-allow`. It reads `PMBUS_REVISION` (`0x98`) as the identification
transaction; a NAK/no-response is typed `bus.unavailable`, never guessed. It
then best-effort reads `MFR_ID`/`MFR_MODEL`/`MFR_REVISION`, `VOUT_MODE`,
`READ_VOUT`, `READ_IOUT`, and `READ_TEMPERATURE_1`. VOUT is converted to volts
only when `VOUT_MODE` reports the PMBus linear format; IOUT and temperature are
returned as raw words because their exponent is device-specific. No PMBus write
or configuration command exists.

### IIO facts

A channel read uses `in_{channel}_raw`, `in_{channel}_scale`,
`in_{channel}_offset`, `in_{channel}_input`, `in_{channel}_type`, and
`in_{channel}_sampling_frequency`, with the device `sampling_frequency` as a
fallback. `processed` is the kernel `_input` value when present, otherwise
`(raw + offset) * scale` when raw and scale are both present. Missing optional
files are `null`, not invented. A channel with neither `_raw` nor `_input` is
typed `bus.unavailable`. There is no IIO write, trigger, buffer, or event
surface.

### PWM request body

```json
{"period_ns":1000000,"duty_ns":250000,"enable":true}
{"duty_ns":100000}
{"enable":false}
```

`period_ns` must be 1–1,000,000,000,000 (1000 s); `duty_ns` must be 0–the
period; at least one field is required. The exact channel is checked against
`--pwm-allow` before any write. If the channel directory is absent, the pair
must have been listed as `:export`; only then is the channel number written to
the chip's `export`. Writes are ordered to remain kernel-valid: disable first
when requested, lower duty to zero if the period shrinks below the current duty,
write the period, write the duty, and enable last. The response reports the
read-back `period_ns`, `duty_ns`, `enabled`, `exported_now`, and the changed
fields. There is no `unexport` route and the node never writes `unexport`.

### DDC/CI request body

```json
{"value":70}
```

`POST /buses/ddc/{bus}/{addr}/vcp/{code}/set` accepts a 16-bit `value` and only
the three supported VCP codes: brightness `0x10`, contrast `0x12`, and input
source `0x60`. Reads work for any allowlisted pair; writes require the exact
pair to be listed as `BUS:0x37:write` and to be present in `--i2c-allow`. The
real backend uses the DDC/CI framing (source `0x51`, length `0x80|n`, XOR
checksum), a bounded 50 ms turnaround, and fixed bounded reply reads: 11 bytes
for get-VCP and 35 bytes for one capabilities fragment. DDC/CI writes are only
attempted with the explicit write opt-in.

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
  requires an exact interface name; 1-Wire requires the `--onewire` opt-in;
  IIO requires an exact `DEVICE:CHANNEL`; PWM requires an exact
  `CHIP:CHANNEL`; DDC requires an exact `BUS:0x37` also present in
  `--i2c-allow`. There is no implicit bus 0, chip 0, interface, glob, or
  fallback device.
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
  `/dev/i2c-N`; detect checks the bus before spawning `i2cdetect`; SPI checks
  the normalized path before `Spidev::open`; 1-Wire validates the id before
  joining it to the root; IIO validates the device/channel pair before reading
  sysfs; PWM checks the exact chip/channel before any write; DDC checks both
  its own pair and the I²C allowlist before opening the bus.
- **No broadcast/reserved I²C writes.** `0x00` (general call/broadcast),
  `0x01..=0x07`, and `0x78..=0x7f` are rejected at configuration and request
  time. This applies to reads as well as writes.
- **No default probing.** `GET /buses/status`, `GET /buses/i2c`,
  `GET /buses/spi`, `GET /buses/iio`, `GET /buses/pwm`, and `GET /buses/ddc`
  only read configuration, stat `/dev`/sysfs nodes, and optionally list 1-Wire
  sysfs. They never scan I²C buses, open SPI devices, export PWM channels, or
  address a DDC display.
- **Bounded scans.** `i2cdetect -y -r` runs only for an allowlisted bus with a
  2 s default / 10 s maximum deadline. The child is killed on expiry and output
  is read with a 64 KiB cap. Only allowlisted addresses are returned.
- **Bounded payloads.** I²C blocks are at most 32 bytes; SPI transfers are at
  most `max_transfer_bytes` (default 4 KiB, absolute 64 KiB); request bodies are
  capped before JSON extraction; listings and allowlists are capped by
  `max_devices`. IIO sysfs values are read with a 256-byte cap. PWM period/duty
  are capped at 1000 s and channel numbers at 4096. DDC/CI fragments are at
  most 32 data bytes and capabilities text is capped per response.
- **IIO is read-only and pair-scoped.** Only exact `DEVICE:CHANNEL` pairs are
  readable. Device names normalize to `iio:deviceN`; channel names admit only
  letters, digits, `_`, and `-`. No trigger, buffer, write, or event file is
  ever opened.
- **PWM is pair-scoped and never unexports.** The exact chip/channel pair is
  checked before any write. Only `period`, `duty_cycle`, `enable`, and (with
  `:export`) the chip `export` file are written. There is no `unexport` route
  and no chip-wide operation. A channel that already exists is usable without
  `:export`; `:export` is required only to create a missing one.
- **DDC/CI is layered on the I²C allowlist.** Every DDC pair must be an exact
  `--i2c-allow` entry and the address must be `0x37`. Reads are allowed once
  listed; writes additionally need the exact `:write` suffix. Only brightness,
  contrast, and input source are exposed. The node never scans a DDC bus.
- **PMBus never probes.** Identification is only for an explicit
  `GET /buses/pmbus/{bus}/{addr}` on an already-allowlisted address. There is
  no PMBus write or auto-detection, and a failed revision read is typed
  `bus.unavailable`.
- **PEC is explicit and verified.** `--i2c-pec` is off by default. If the
  adapter does not advertise SMBus PEC, the request fails typed rather than
  silently issuing a non-PEC transaction.
- **Path validation.** 1-Wire ids must match exactly two hex family digits,
  `-`, and twelve hex id digits. SPI paths must be absolute direct children of
  `/dev` named `spidevX.Y` with digits. IIO device names normalize to
  `iio:deviceN` and channels reject `/`, `\`, `:`, and `.`; PWM chips normalize
  to `pwmchipN` and reject non-`/sys/class/pwm` paths. No route accepts `.`/`..`
  or extra path components.
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
| `bus.not-allowed` | 403 | Exact bus/address/path/GPIO line/CAN interface/IIO pair/PWM pair/DDC pair is not allowlisted, or PWM lacks `:export`, or DDC lacks `:write` |
| `bus.permission-denied` | 403 | Kernel denied the GPIO chip/device, sysfs file, or bus node (group/ACL) |
| `bus.busy` | 409 | GPIO line write/pulse or CAN send already in flight; kernel `EBUSY` |
| `bus.invalid-request` | 400 | Malformed id/path/JSON/operation or a bound violation (including a PWM duty above its period and unsupported DDC VCP codes) |
| `bus.io-failed` | 502 | Kernel/backend I/O failed after the request was accepted |
| `bus.unsupported` | 501 | Non-Linux backend, missing `i2cdetect`, an adapter without SMBus PEC, an unsupported SMBus operation, or a display rejecting a VCP read |
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

`cargo test -p ozma-node --lib buses` — 72 passed, 6 ignored (live i2c-stub
read/write/detect, live i2c-stub PEC/SMBus-block typed unsupported, gpio-sim,
vcan, and live IIO/PWM typed unavailable on a host without those sysfs roots).

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
- **IIO:** exact pair parsing/device normalization/channel traversal rejection;
  fixture listing, device name, raw/scale/offset/type/sampling; processed value
  from `_input` and from `(raw+offset)*scale`; missing/unlisted/unavailable
  channels; unconfigured behavior; live typed unavailable with no IIO root.
- **PWM:** exact `CHIP:CHANNEL[:export]` parsing/normalization/traversal
  rejection; fixture list/detail; exact period/duty/enable sysfs write
  read-back; period shrink below the current duty; export opt-in required and
  no automatic export; bounds and unknown-field rejection; `unexport` sentinel
  proves it is never written; live typed unavailable with no PWM root.
- **SMBus/PMBus/PEC:** `smbus-block` read/write normalization and 32-byte
  bounds; PEC opt-in plumbing and response flag; live i2c-stub typed
  `bus.unsupported` for adapters without PEC or SMBus block; PMBus revision
  identification, MFR strings, linear VOUT conversion, raw IOUT/temperature,
  non-PMBus/unlisted/unconfigured typed errors.
- **DDC/CI:** exact `BUS:0x37[:write]` parsing, I²C membership cross-check, VCP
  code restriction, request/reply framing and checksum, fake get/set/
  capabilities, read-only write rejection, value bounds, and unconfigured
  behavior.
- **Router/config:** default/unconfigured state, full CLI parsing including
  IIO/PWM/DDC/PEC, transfer/block/device/frame bounds, route presence, 404/405
  shapes, typed `bus.not-configured` for every implemented route.

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

### Live IIO/PWM typed evidence

This host has neither `/sys/bus/iio/devices` nor `/sys/class/pwm`. The ignored
tests used the kernel roots with an explicit allowlist, loaded no modules, and
asserted the typed unavailable path:

```text
$ cargo test -p ozma-node --lib \
    buses::iio::tests::live_iio_root_is_typed_unavailable -- --ignored --nocapture
running 1 test
test buses::iio::tests::live_iio_root_is_typed_unavailable ... ok

$ cargo test -p ozma-node --lib \
    buses::pwm::tests::live_pwm_root_is_typed_unavailable -- --ignored --nocapture
running 1 test
test buses::pwm::tests::live_pwm_root_is_typed_unavailable ... ok
```

Both return `bus.unavailable` before any sysfs write. IIO/PWM write behavior is
covered only by the temp-directory sysfs fixtures; no physical PWM or IIO device
was touched.

### Live i2c-stub PEC/SMBus-block evidence

`i2c-stub` advertises neither SMBus PEC nor SMBus block transfers
(`i2cdetect -F 22` reports both `no`). The ignored real-backend test asserted
the node fails typed instead of silently dropping PEC or changing the
transaction:

```text
$ sudo modprobe i2c-stub chip_addr=0x50
$ sudo env OZMA_LIVE_I2C_BUS=22 \
    ./target/debug/deps/ozma_node-<hash> \
    --exact buses::i2c::tests::live_i2c_stub_pec_and_smbus_block_are_typed_unsupported \
    --ignored --nocapture
running 1 test
test buses::i2c::tests::live_i2c_stub_pec_and_smbus_block_are_typed_unsupported ... ok
```

The existing i2c-stub byte/word/I²C-block/detect test also remained green. The
adapter was removed with `sudo modprobe -r i2c_stub`; `/dev/i2c-22` is absent.
SMBus block/PEC success paths are exercised by fake-backend tests because the
available virtual adapter does not implement them.

DDC/CI has no safe virtual sink on this host (the only `/dev/i2c-*` nodes are
real adapters and the DDC address is a physical display target), so get/set/
capabilities and framing are covered only by fakes and unit tests. No DDC bus
was opened or written.

No `spidev` or 1-Wire hardware exists on this host (`/dev/spidev*` and
`/sys/bus/w1/devices/` are absent), so live 1-Wire/SPI evidence is the typed
`bus.unavailable` path exercised by the fixture/fake tests. No virtual spidev or
w1 module was loaded. Real GPIO lines (`gpiochip0/1`), physical CAN interfaces,
and real I²C devices/DDC buses were never opened or modified.

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
- I²C supports only 7-bit user addresses, SMBus byte/word/I²C-block/SMBus-block
  transactions, and opt-in PEC. There is no 10-bit addressing, SMBus process
  call, or raw multi-message `I2C_RDWR` combined transfer exposed as a route.
  PEC support is verified with `i2cdetect -F`; adapters that cannot implement
  PEC return `bus.unsupported`.
- I²C detection shells out to `i2cdetect`; there is no in-process scan. PMBus
  identification is best-effort and standard-register only; it does not parse
  device-specific IOUT/temperature coefficients and does not expose PMBus
  writes/configuration.
- IIO is read-only and channel-scoped. There is no trigger, buffer, event,
  write, calibration, or sampling-frequency write, and no channel enumeration
  outside the allowlist.
- PWM has no unexport, polarity, capture, or chip-wide operation. Writes are
  direct `period`/`duty_cycle`/`enable` sysfs writes; there is no atomic
  multi-file transaction, so a crash mid-sequence can leave the channel at an
  intermediate valid value.
- DDC/CI supports only brightness, contrast, and input source, one capabilities
  fragment per request, and no multi-monitor discovery. The real backend reads
  fixed bounded reply sizes (11 bytes get-VCP, 35 bytes capabilities); a monitor
  that returns a shorter capabilities fragment may need a retry or a different
  read strategy. There is no live DDC evidence on this host.
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
