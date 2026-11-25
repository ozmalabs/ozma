# ozma-thermal

Thermal zones, fan curves, anti-cycling control, and fan-noise estimation for
the Rust node.

## Safety model

- **Read-only by default.** The node samples hwmon and exposes status without
  writing PWM. Fan control requires both `--thermal-control` and a policy file.
- **Critical override is not optional.** At the critical threshold the engine
  commands 100% duty regardless of the curve, noise cap, duty cap, dwell timer,
  or ramp limit.
- **Noise never outranks safety.** The comfort dBA cap applies only below the
  zone's warning temperature. Warning and critical states relax or ignore it.
- **Restore on exit.** The hwmon backend records each fan's original
  `pwmN_enable` value and writes it back on stop/drop.
- **No guesses on missing sensors.** A zone with no fresh sensor reading uses a
  fail-safe duty and reports the gap instead of silently reducing cooling.

## Policy

`examples/policy.json` is a complete starting point. `ozma-node` loads it with:

```bash
ozma-node --profile desktop \
  --thermal-config /etc/ozma/thermal.json
```

To allow PWM writes:

```bash
ozma-node --profile desktop \
  --thermal-config /etc/ozma/thermal.json \
  --thermal-control
```

Sensor and fan IDs match the discovered hwmon inventory:

```text
hwmon:coretemp:temp1
hwmon:amdgpu:temp1
hwmon:it87:pwm1
```

Inspect discovered IDs and current readings at `GET /thermal` (or
`/api/v1/thermal/zones`, `/fans`, `/profiles`). The reads are authenticated with
the rest of the node API.

## Anti-cycling

Three independent guards prevent audible fan oscillation:

- **Hysteresis** keeps the previous duty when the temperature moves less than
  the curve's `hysteresis_c`.
- **Minimum dwell** (`min_dwell_secs`) prevents a new target from being applied
  before the previous change has had time to settle.
- **Ramp rate** (`ramp_rate_percent_per_sec`) limits how quickly duty can move.

## Noise estimate

`noise_dba`, `noise_at_idle_dba`, and optional `noise_points` produce an
estimated dBA per fan. Multiple fans combine acoustically
(`10*log10(sum(10^(dBA/10)))`). The number is an estimate from manufacturer
figures, not a calibrated measurement; the API exposes it alongside the
measured RPM and duty.
