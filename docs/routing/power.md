# Power Model

Status: Draft

## RFC 2119 Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).

## Abstract

This document specifies the Ozma power model: how the routing graph tracks
voltage rails, power delivery, power consumption, negotiation state, and
power distribution infrastructure. Power is a first-class concern -- every
device consumes power, many devices deliver power to other devices, and the
available power constrains what the system can do. Running 300 RGB LEDs at
full white on a USB port rated for 500 mA will brown out the node. The
routing protocol MUST model power delivery, consumption, measurement, and
pressure so the router can make safe decisions.

## Specification

### Voltage Rails

Power flows through **voltage rails** -- named power paths at a specific
voltage. A device MAY have multiple rails (a PC has 3.3 V, 5 V, 12 V). Every
voltage rail MUST track capacity and current usage. Each rail has a capacity
and a current state:

```yaml
VoltageRail:
  id: string                    # rail identifier ("5v_usb", "12v_pcie", "3v3_gpio", "48v_poe")
  nominal_voltage_v: float      # expected voltage (5.0, 12.0, 3.3, 48.0)
  voltage_range_v:              # acceptable operating range
    min: float                  # below this: under-voltage, device may malfunction
    warn_low: float             # below this: warn, approaching limit
    nominal: float              # expected voltage
    warn_high: float            # above this: warn, over-voltage
    max: float                  # above this: over-voltage, damage risk
  current_capacity_ma: float    # maximum deliverable current on this rail
  current_used_ma: float        # current draw (measured or inferred)
  current_available_ma: float   # capacity - used
  measured_voltage_v: float?    # actual measured voltage (if available)
  power_w: float?               # computed: voltage x current (for reporting)
  quality: InfoQuality          # how we know these numbers
  source: PowerSource           # where this rail's power comes from
```

Implementations MUST compute `current_available_ma` as `current_capacity_ma`
minus `current_used_ma`. If `measured_voltage_v` is available, the
implementation SHOULD expose `power_w` as the product of measured voltage and
current.

**PowerSource** -- where a rail gets its power:

```yaml
PowerSource:
  type: string                  # "usb_host", "usb_pd", "poe", "external_psu",
                                # "gpio_header", "battery", "barrel_jack",
                                # "pcie_slot", "sata_power", "molex"
  upstream_device: DeviceRef?   # which device supplies this power
  upstream_rail: string?        # which rail on the upstream device
  negotiated: bool              # was this power level negotiated? (USB PD, PoE)
  negotiation_state: PowerNegotiationState?  # if negotiated: current state
```

If `negotiated` is `true`, the implementation MUST populate
`negotiation_state`.

### Voltage as a Measurement Proxy

Many devices cannot directly measure current draw. They MAY measure voltage,
and voltage drop under load reveals current draw:

```yaml
VoltageMeasurement:
  rail_id: string               # which rail
  voltage_v: float              # measured voltage
  expected_v: float             # nominal voltage for this rail
  drop_v: float                 # expected - measured
  inferred_current_ma: float?   # estimated from voltage drop + known rail impedance
  inference_quality: InfoQuality # always lower than direct current measurement
  sensor: string?               # what's measuring ("ina219", "adc_ch3", "sysfs")
  timestamp: timestamp
```

A 5 V USB rail measuring 4.72 V tells you the combined load is drawing
enough current to cause a 0.28 V drop across the cable and connector
resistance. If you know the cable resistance (~0.5 ohm for a typical 0.5 m USB
cable), you can estimate ~560 mA. This is `measured` quality for the voltage,
but only `inferred` quality for the current.

Implementations that derive current from voltage measurements MUST assign
`inferred` quality to the resulting current value, never `measured`.

```yaml
InfoQuality: enum
  user
  measured
  inferred                      # derived from measured data + known/assumed parameters
  reported
  commanded
  spec
  assumed
```

**Trust ordering**: `user > measured > inferred > reported > commanded > spec > assumed`

The `inferred` level sits between `measured` and `reported` -- it is based on
real measurements but requires assumptions (cable resistance, connector
quality) that MAY be wrong. Implementations MUST NOT promote `inferred`
quality data to a higher trust level.

### Power Budgets on Ports and Links

Every port and link that carries power MUST have a power budget:

```yaml
PortPowerBudget:
  delivers_power: bool          # does this port supply power to connected devices?
  consumes_power: bool          # does this port draw power from the connection?
  rail: VoltageRail?            # the voltage rail this port connects to
  max_current_ma: float?        # maximum current this specific port can deliver/draw
  current_draw_ma: float?       # current draw of the device on this port
  quality: InfoQuality
```

This applies to:

| Port type | Delivers | Consumes | Typical budget |
|-----------|----------|----------|---------------|
| USB-A host port | Yes | No | 500 mA (USB2), 900 mA (USB3) |
| USB-C host port | Yes | No | 1500 mA (default), 3000 mA (USB-C current) |
| USB-C with PD | Yes | No | Up to 5 A @ 5-48 V (negotiated) |
| USB gadget port | No | Yes | Declared via `max_power_ma` in ConfigFS |
| PoE port | Yes | No | 15.4 W (af), 30 W (at), 60 W (bt Type 3), 90 W (bt Type 4) |
| GPIO pin | Yes/No | Yes/No | 2-16 mA per pin (SoC dependent) |
| GPIO power header | Yes | No | Total limited by regulator (e.g., 300 mA on 3.3 V) |
| PCIe slot | Yes | No | 75 W (x16), 25 W (x1) |
| SATA power | Yes | No | 4.5 A @ 5 V, 4.5 A @ 12 V |
| Barrel jack | Yes | No | PSU rating |
| LED data pin | No | No | Signal only -- power is separate |

### USB Power in Detail

USB power is the most complex case because it involves negotiation, multiple
standards, and widespread non-compliance.

USB PD negotiation state MUST be reported when a USB PD connection is active:

```yaml
UsbPowerState:
  standard: string              # "usb2", "usb3", "usb_c_default", "usb_c_1.5a",
                                # "usb_c_3a", "usb_pd"
  negotiated_voltage_v: float   # actual negotiated voltage (5 V default, up to 48 V PD)
  negotiated_current_ma: float  # actual negotiated current limit
  pd_state: UsbPdState?         # if USB PD: full negotiation state

UsbPdState:
  source_pdos: PdObject[]       # what the source offered (Power Data Objects)
  selected_pdo: PdObject        # which PDO was selected
  active_voltage_v: float       # current voltage
  active_current_ma: float      # current limit
  pps: bool                     # Programmable Power Supply mode active?
  pps_range: { min_v: float, max_v: float, max_ma: float }?

PdObject:
  type: string                  # "fixed", "variable", "pps", "avs"
  voltage_v: float              # for fixed: exact voltage. For variable: max voltage.
  min_voltage_v: float?         # for variable/PPS: minimum voltage
  current_ma: float             # maximum current at this voltage
  power_w: float                # computed: voltage x current
```

When a USB PD negotiation completes, the implementation MUST emit a
`routing.power.pd_negotiated` event. The `UsbPdState` MUST include all PDOs
offered by the source, not only the selected PDO.

### PoE Power

```yaml
PoePowerState:
  standard: string              # "802.3af", "802.3at", "802.3bt_type3", "802.3bt_type4",
                                # "passive_24v", "passive_48v"
  class: uint?                  # PoE class (0-8)
  allocated_w: float            # power allocated by the switch
  used_w: float?                # actual power draw (if switch reports it)
  voltage_v: float?             # measured voltage (typically 48 V nominal)
```

PoE-powered devices MUST report the `standard` and `allocated_w` fields.
The `used_w` field SHOULD be populated when the PoE switch reports actual
draw.

### Device Power Profile

Every device in the graph MUST have a power profile describing what it
consumes and what it delivers:

```yaml
DevicePowerProfile:
  # What this device consumes
  consumption: PowerConsumption
  # What this device delivers to other devices
  delivery: PowerDelivery?
  # Battery (if applicable)
  battery: BatteryState?

PowerConsumption:
  idle_w: float?                # power draw when idle
  typical_w: float?             # power draw under typical load
  peak_w: float?                # maximum power draw (transient)
  per_function: FunctionPowerCost[]?  # breakdown by function
  source_rail: string           # which rail this device draws from
  quality: InfoQuality

FunctionPowerCost:
  function: string              # what function ("hid_gadget", "video_capture",
                                # "rgb_all_white", "rgb_typical", "wifi_active",
                                # "cpu_full_load", "encode_h264")
  current_ma: float             # additional current draw when this function is active
  voltage_v: float              # on which voltage rail
  quality: InfoQuality          # how we know this
  notes: string?                # e.g., "60 mA per LED at full white, 20 mA typical"

PowerDelivery:
  rails: VoltageRail[]          # voltage rails this device provides
  total_power_w: float?         # total deliverable power across all rails
  ports: PortPowerBudget[]      # per-port power budgets
```

**BatteryState** (phones, wireless peripherals, UPS, laptops):

```yaml
BatteryState:
  present: bool
  chemistry: string?            # "li_ion", "li_po", "nimh", "lead_acid"
  capacity_mah: uint?           # rated capacity
  current_percent: float        # 0-100
  current_voltage_v: float?     # current battery voltage
  charging: bool
  charge_rate_ma: float?        # current charge rate
  time_to_empty_min: float?     # estimated runtime
  time_to_full_min: float?      # estimated charge time
  health_percent: float?        # battery health (capacity vs design)
  cycles: uint?                 # charge cycle count
  quality: InfoQuality
```

Devices with batteries MUST report `present`, `current_percent`, and
`charging`. The remaining fields SHOULD be populated when the hardware
provides them.

### RGB Power -- the Biggest Pressure Point

RGB is the most common case where power pressure actually matters in
practice. A WS2812B LED draws up to 60 mA at full white (20 mA per channel),
but only ~20 mA at typical use. This scales linearly:

| LED count | Full white (60 mA/LED) | Typical (20 mA/LED) | 5 V rail current |
|----------|----------------------|--------------------|-----------------| 
| 30 | 1.8 A / 9 W | 0.6 A / 3 W | Within USB limits |
| 60 | 3.6 A / 18 W | 1.2 A / 6 W | Exceeds USB 3.0 (900 mA) |
| 144 | 8.6 A / 43 W | 2.9 A / 14.4 W | Needs dedicated PSU |
| 300 | 18 A / 90 W | 6 A / 30 W | Needs beefy 5 V PSU |

The router MUST know:
1. How many LEDs are on this strip (from device database)
2. What power the current effect demands (from the RGB compositor)
3. What power is available on the rail feeding this strip
4. Whether the effect would exceed the power budget

**RGB power estimation**: The RGB compositor knows what colours it is
rendering. Full white = 60 mA/LED. Pure red/green/blue = 20 mA/LED.
Black = ~1 mA/LED (quiescent). The compositor MUST compute frame-by-frame
power estimates:

```
estimated_current_ma = sum(
  led_current(r, g, b)  # per-channel: (channel_value / 255) x 20 mA
  for each LED in the strip
)
```

This feeds into the power model as a `measured` quality current estimate
(based on known LED characteristics and the actual frame data).

**Power limiting in the RGB compositor**: When estimated power exceeds the
rail budget, the compositor SHOULD:
1. Scale global brightness to fit within budget (preferred -- invisible to user)
2. Reduce colour saturation toward black
3. Alert the user that the effect exceeds the available power

When the compositor reduces brightness due to power limits, it MUST emit a
`routing.power.rgb_power_limited` event.

### Power in the Routing Graph

Power adds a new dimension to the graph. Every device MUST have:
- `power_profile` on the Device (what it consumes and delivers)
- `power_budget` on each Port that carries power
- `power_state` reflecting current measurements

The router uses this for:

**1. Pipeline feasibility**: Adding a pipeline through a device increases its
power draw. If the device's power source cannot handle it, the pipeline MUST
be rejected:

```
For each device in pipeline:
  for each rail the pipeline's functions draw from:
    if (rail.current_used_ma + pipeline.function_cost_ma) > rail.current_capacity_ma:
      reject pipeline  # would exceed rail capacity
```

**2. Warm pipeline power accounting**: Keeping pipelines warm costs power.
The WarmCost already tracks this informally -- now it is backed by the power
model. Three warm ffmpeg processes on a Pi 5 might collectively draw 2 W.
The router SHOULD check this against the Pi's power supply headroom before
deciding to keep them warm.

**3. Power pressure alerts**: When any rail drops below `warn_low` voltage
or approaches current capacity, the router MUST:
- Degrade RGB effects (reduce brightness)
- Cool warm pipelines (reduce idle power)
- Alert the user ("USB port on Node 1 is near power limit")

**4. Power-aware device placement**: When the user adds a new device to the
graph (e.g., plugs a USB capture card into a node), the router SHOULD check
whether the node's USB power budget can support it. If the node is already
running an RGB strip on the same USB controller's 5 V rail, the
implementation SHOULD warn about the combined draw.

### Power Discovery

| Source | Platform | What it provides | Quality |
|--------|----------|-----------------|---------|
| INA219 (I2C current sensor) | Any | Direct voltage + current measurement | `measured` |
| sysfs power supply class | Linux | Battery state, USB PD state, charger info | `reported` |
| USB descriptor `bMaxPower` | Any | Declared max current draw of USB device | `spec` |
| USB PD source capabilities | USB-C | Available PDOs from the power source | `reported` |
| PoE switch LLDP/CDP | Network | Allocated PoE power class | `reported` |
| Device database entry | Any | Rated power consumption per function | `spec` |
| Voltage measurement (ADC) | SBC | Rail voltage -> inferred current | `inferred` |
| Smart PSU (PMBus/IPMI) | Server | Per-rail voltage, current, power | `measured` |
| UPS via NUT | Any | Input/output voltage, load %, battery state | `reported` |
| None (unknown device) | Any | USB class default (500 mA USB2, 900 mA USB3) | `assumed` |

Implementations MUST assign the `InfoQuality` level from the table above to
each power datum. A higher-quality source MUST take precedence over a
lower-quality source for the same measurement.

### Power in the Device Database

The `PowerSpec` block on device database entries:

```yaml
PowerSpec:
  input_voltage: VoltageRange?          # what voltage this device expects
  input_current_max_ma: float?          # maximum input current draw
  idle_power_w: float?                  # typical idle power
  peak_power_w: float?                  # maximum power (all functions active)
  power_source_options: string[]?       # ["usb", "barrel_5v", "poe", "battery"]
  per_function_power: FunctionPowerCost[]?  # breakdown by function
  efficiency_percent: float?            # PSU/regulator efficiency (for delivery devices)

  # --- Power input connector (the physical power port) ---
  power_input: PowerConnectorSpec?      # what connector and what it actually needs
  power_inputs: PowerConnectorSpec[]?   # if multiple power inputs (redundant PSU, AC+USB)

  # For devices that deliver power:
  output_rails: OutputRail[]?

  # For batteries:
  battery_spec: BatterySpec?

  # For LED devices:
  led_power: LedPowerSpec?

PowerConnectorSpec:
  connector: string             # physical connector type (see table below)
  label_voltage_v: float?       # what's printed on the device label
  actual_voltage_v: float?      # what the device actually needs (may differ from label!)
  actual_voltage_range: VoltageRange?  # full acceptable range
  current_ma: float?            # required/rated current
  power_w: float?               # wattage rating
  polarity: string?             # "center_positive", "center_negative", "ac" (for barrel jacks)
  barrel: BarrelJackSpec?       # barrel jack dimensions (if barrel_dc connector)
  iec: IecSpec?                 # IEC connector details (if IEC)
  country_plug: string?         # wall plug standard if hardwired ("nema_5_15", "type_g", etc.)
  included_adapter: PowerAdapterSpec?  # PSU/adapter that ships with the device
  compatible_adapters: string[]?  # device database IDs of known compatible adapters
  incompatible_adapters: string[]?  # adapters known to NOT work (wrong voltage, wrong polarity)
  notes: string?                # free-text notes ("label says 9V but ships with 12V adapter")
  source: string?               # where this info came from ("label", "measured", "manual",
                                # "community_verified")

BarrelJackSpec:
  outer_diameter_mm: float      # outer barrel diameter
  inner_diameter_mm: float      # inner pin diameter
  barrel_length_mm: float?      # barrel depth
  # Common sizes:
  # 5.5x2.1mm -- most common (Arduino, LED strips, many devices)
  # 5.5x2.5mm -- many laptops, some pro audio
  # 4.0x1.7mm -- some small devices
  # 3.5x1.35mm -- smaller devices
  # 6.3x3.0mm -- some older equipment
  # 2.1 and 2.5mm inner pins physically fit the same 5.5mm barrel --
  # a 2.5mm plug in a 2.1mm jack makes intermittent contact.
  # A 2.1mm plug in a 2.5mm jack wobbles and may not connect.
  # The database captures exact dimensions to warn about this.

IecSpec:
  type: string                  # IEC 60320 type
  temperature_rating_c: uint?   # max temperature (C15/C17 = 120 deg C, C13 = 70 deg C)
  current_rating_a: float       # rated current
  voltage_rating_v: uint        # rated voltage (typically 250 V AC)
  fused: bool?                  # fused connector (UK C13 with built-in fuse)
  locking: bool?                # locking variant (IEC 60320-1 C13L/C19L)
  # IEC types that look similar but are NOT interchangeable:
  # C13 (70 deg C, 10 A) vs C15 (120 deg C, 10 A) -- C15 has a notch, for hot devices
  # C19 (70 deg C, 16 A) vs C21 (120 deg C, 16 A) -- C21 has a notch
  # Using a C13 cable on a device that needs C15 (e.g., kettle, high-temp
  # equipment) is a fire hazard -- the cable is not rated for the temperature.

PowerAdapterSpec:
  type: string                  # "ac_adapter" (wall wart), "inline_psu", "usb_charger",
                                # "internal_psu", "open_frame_psu"
  input_voltage: string?        # "100-240V AC" (universal), "120V AC", "230V AC"
  input_frequency: string?      # "50/60Hz", "50Hz", "60Hz"
  output_voltage_v: float       # DC output voltage
  output_current_ma: float      # DC output current
  output_power_w: float?        # output power
  output_connector: string      # connector type on the output
  output_barrel: BarrelJackSpec?  # barrel dimensions if barrel_dc
  output_polarity: string?      # "center_positive", "center_negative"
  regulation: string?           # "regulated", "unregulated"
  # Unregulated adapters output higher voltage at low load (a "12V"
  # unregulated adapter may output 15V with no load, 12V at rated current,
  # and 10V when overloaded). Regulated adapters maintain constant voltage.
  efficiency: string?           # "level_vi" (modern efficient), "level_v", "linear"
  standby_power_w: float?       # power consumed when device is off/standby
  replacement_available: bool?  # can you still buy this adapter?
  generic_compatible: bool?     # can a generic adapter with matching specs work?

# Example -- your FireWire mixer that says 9V but needs 12V:
#
# power_input:
#   connector: "barrel_dc"
#   label_voltage_v: 9          # WRONG -- label is incorrect
#   actual_voltage_v: 12        # what it actually needs
#   actual_voltage_range: { nominal: 12, min: 11, max: 13 }
#   current_ma: 1500
#   polarity: "center_positive"
#   barrel: { outer_diameter_mm: 5.5, inner_diameter_mm: 2.1 }
#   included_adapter:
#     output_voltage_v: 12      # ships with 12V despite 9V label
#     output_current_ma: 2000
#     output_connector: "barrel_dc"
#     output_polarity: "center_positive"
#   notes: "Device label reads 9V DC but ships with 12V adapter and
#           requires 12V to operate. 9V supply causes erratic behaviour.
#           Community verified."
#   source: "community_verified"
```

When a device's `label_voltage_v` differs from `actual_voltage_v`, the
implementation MUST surface a warning to the user. This protects against
mislabelled equipment, particularly second-hand devices sold without
original adapters.

**Why this matters**:

1. **Second-hand equipment**: When you buy a mixer/interface/device without
   its original adapter, you need to know the actual voltage, current,
   polarity, and barrel size. The label MAY be wrong. The device database
   is the source of truth -- community-verified, not manufacturer labels.

2. **Cable/adapter shopping**: "I need a replacement power cable for my
   monitor" -- the database tells you it is IEC C14 (not C8, not C6),
   10 A rated. "I need a power supply for this LED strip" -- 5 V, 10 A,
   5.5x2.1 mm centre-positive barrel.

3. **Polarity warnings**: Centre-negative barrel jacks exist (some older
   effects pedals, some Yamaha keyboards). Plugging in a centre-positive
   adapter can damage the device. The database captures polarity per device
   and MUST warn on mismatch.

4. **IEC temperature ratings**: A C13 cable on a device that generates heat
   (laboratory equipment, some industrial gear) is a fire hazard if the
   device needs C15 (120 deg C rated). They look almost identical -- C15 has
   a small notch. The database MUST record which type is required and MUST
   warn when the wrong IEC type is connected.

5. **Universal adapter compatibility**: The database tracks which devices
   MAY use generic adapters (voltage + current + polarity + barrel match)
   vs which REQUIRE specific vendor adapters (proprietary connectors,
   specific regulation requirements, communication pins).

6. **Complete cable inventory**: With power connectors modelled alongside
   data connectors, the system can generate a complete cable shopping list
   for any setup: "Your rack needs: 3x IEC C13-C14 cables (1.8 m), 2x
   IEC C19-C20 cables (1.0 m), 1x 12 V/3 A 5.5x2.1 mm centre-positive
   barrel adapter, 4x Cat6a patch cables (0.5 m), 2x SFF-8643 SAS cables
   (1.0 m)."

```yaml
VoltageRange:
  nominal: float                # expected input voltage
  min: float                    # minimum operating voltage
  max: float                    # maximum safe voltage

OutputRail:
  voltage_v: float              # output voltage
  max_current_ma: float         # maximum deliverable current
  regulation: string?           # "linear", "switching", "unregulated"
  shared_with: string[]?        # other outputs sharing this rail's capacity

BatterySpec:
  chemistry: string             # "li_ion", "li_po", "nimh", "lead_acid"
  capacity_mah: uint
  nominal_voltage_v: float
  charge_voltage_v: float?      # full charge voltage
  cutoff_voltage_v: float?      # discharge cutoff
  max_charge_rate_c: float?     # maximum charge rate (C-rate)

LedPowerSpec:
  type: string                  # "ws2812b", "sk6812", "apa102", "ws2815"
  per_led_max_ma: float         # max current per LED at full white (60 mA for WS2812B)
  per_channel_max_ma: float     # max per colour channel (20 mA for WS2812B)
  quiescent_ma: float           # current per LED when displaying black (~1 mA)
  voltage: float                # LED operating voltage (5 V or 12 V)
  recommended_psu_headroom: float  # recommended PSU headroom factor (1.2 = 20% over max)
```

### Power Distribution Devices

PDUs, UPS units, power strips, surge protectors, extension cords, splitters
-- anything between the wall and the equipment. These are power routing
devices: they sit in the power graph and distribute mains or DC power to
downstream devices. They have input capacity, output outlets with individual
ratings, and potentially monitoring, switching, and battery backup. They are
the power equivalent of a network switch.

Modular PSU cables from incompatible families MUST trigger a DANGER-level
warning. Modular cables are NOT interchangeable between PSU brands (and
often not between models within a brand); using the wrong cable can destroy
connected equipment.

```yaml
PowerDistributionSpec:
  device_type: string           # "ups", "pdu_basic", "pdu_metered", "pdu_switched",
                                # "pdu_managed", "power_strip", "surge_protector",
                                # "extension_cord", "dc_splitter", "dc_distribution"
  input: PowerDistInput         # power input (what feeds this device)
  outputs: PowerDistOutput[]    # power outputs (what this device feeds)
  total_capacity: PowerCapacity # total rated capacity
  monitoring: PowerMonitoring?  # metering/monitoring capabilities
  switching: PowerSwitching?    # per-outlet switching (if supported)
  surge: SurgeSpec?             # surge protection (if any)
  ups: UpsSpec?                 # battery backup (if UPS)
  cascading: CascadingSpec?     # daisy-chain / cascading rules
  cord_length_m: float?         # for extension cords and strips
  mounting: string?             # "rack_horizontal", "rack_vertical", "rack_0u",
                                # "wall", "desk", "floor", "none"
  rack_units: uint?             # for rack-mount PDUs (0U = vertical mount)

PowerDistInput:
  connector: string             # "iec_c14", "iec_c20", "nema_5_15", "nema_l6_30",
                                # "type_g", "type_f_schuko", "hardwired",
                                # "barrel_dc", "terminal_block"
  voltage: string               # "120v_ac", "230v_ac", "100-240v_ac", "208v_ac",
                                # "12v_dc", "24v_dc", "48v_dc"
  phase: string?                # "single", "three_phase", "split_phase"
  max_current_a: float          # input breaker/fuse rating
  max_power_w: float?           # total input power rating
  frequency_hz: string?         # "50", "60", "50/60"
  plug: PowerConnectorSpec?     # the physical plug (for cable shopping)
  cord_length_m: float?         # input cord length

PowerDistOutput:
  id: string                    # "outlet_1", "outlet_a1", "bank_a_1"
  connector: string             # output receptacle type
  bank: string?                 # which bank/group ("A", "B", "all")
  max_current_a: float?         # per-outlet rating (may be less than total)
  switched: bool?               # can this outlet be switched on/off?
  metered: bool?                # is power metered on this outlet?
  always_on: bool?              # designated always-on (UPS: on battery too)
  battery_backed: bool?         # UPS: on battery during outage?
  physical: PhysicalPortInfo?   # position on the device
  connected_device: string?     # device database ID of what's plugged in (user-configured)
  # connected_device enables the power graph: wall -> PDU outlet 3 -> server PSU.
  # The router traces power from wall to every device.

PowerCapacity:
  max_current_a: float          # total rated current
  max_power_va: float?          # total rated VA (apparent power)
  max_power_w: float?           # total rated watts (real power)
  outlets_total: uint           # total outlet count
  outlet_types: OutletCount[]?  # breakdown by type

OutletCount:
  connector: string             # "iec_c13", "iec_c19", "nema_5_15", "nema_5_20",
                                # "type_g", "type_f_schuko", "usb_a", "usb_c"
  count: uint
  max_current_a: float?         # per-outlet of this type

PowerMonitoring:
  type: string                  # "none", "total_input", "per_bank", "per_outlet"
  metrics: string[]?            # ["voltage", "current", "power_w", "power_va",
                                # "power_factor", "energy_kwh", "frequency",
                                # "temperature", "humidity"]
  protocol: string?             # "snmp", "http", "serial", "modbus", "nut",
                                # "apc_ap9630", "raritan_px", "servertech_sentry",
                                # "cyberpower_cloud", "eaton_ipp"
  # Per-outlet metered PDUs provide measured power per device -- this feeds
  # directly into the power model as `measured` quality data.
  # A metered PDU is a fleet of INA219 sensors for AC power.

PowerSwitching:
  type: string                  # "none", "per_outlet", "per_bank", "master"
  remote: bool?                 # switchable via network/serial (not just physical button)
  scheduled: bool?              # supports scheduled on/off
  delay_sequencing: bool?       # staggered power-on to avoid inrush current spike
  default_state: string?        # "last_state", "always_on", "always_off"
  # Remote-switched outlets enable: graceful shutdown sequencing (storage
  # before compute), power cycling as remediation, and scheduled
  # power for non-essential equipment.

SurgeSpec:
  joules: uint?                 # surge energy absorption rating
  clamping_voltage_v: float?    # voltage at which protection activates
  response_time_ns: float?      # surge response time
  protection_indicators: bool?  # LED showing protection status
  protection_modes: string[]?   # ["line_to_neutral", "line_to_ground", "neutral_to_ground"]
  emi_rfi_filter: bool?         # EMI/RFI noise filtering
  coax_protection: bool?        # coax/antenna surge protection
  ethernet_protection: bool?    # Ethernet surge protection
  phone_protection: bool?       # phone/DSL line protection
  connected_equipment_warranty: float?  # vendor warranty on connected equipment ($)
  # Surge ratings degrade over time as the MOV absorbs surges.
  # Trend analysis can track if a smart surge protector reports
  # declining protection capacity.

UpsSpec:
  topology: string              # "standby" (offline), "line_interactive", "online_double_conversion"
  capacity_va: uint             # rated VA capacity
  capacity_w: uint?             # rated watt capacity (typically 60% of VA)
  battery: BatterySpec          # battery type, capacity, chemistry
  runtime_minutes: RuntimeEstimate[]?  # estimated runtime at various load levels
  transfer_time_ms: float?      # time to switch to battery (standby: 5-12 ms,
                                # line-interactive: 2-4 ms, online: 0 ms)
  input_voltage_range: VoltageRange?  # input voltage tolerance before switching to battery
  avr: bool?                    # automatic voltage regulation (boosts/bucks without battery)
  pure_sine_wave: bool?         # pure sine output (vs simulated/stepped sine)
  # Simulated sine can cause problems with active PFC PSUs (most modern PSUs).
  # Pure sine is REQUIRED for reliable operation of computer equipment.
  outlets_battery: uint?        # outlets on battery backup
  outlets_surge_only: uint?     # outlets with surge only (no battery)
  usb_port: bool?               # USB for NUT/apcupsd communication
  serial_port: bool?            # serial for NUT/apcupsd
  network_card: bool?           # SNMP network management card (slot or built-in)
  network_protocol: string?     # "snmp", "apc_smartconnect", "eaton_ipp", "cyberpower_cloud"
  display: bool?                # LCD/LED status display
  audible_alarm: bool?          # beep on battery/overload
  self_test: bool?              # automatic self-test capability
  firmware: FirmwareInfo?       # UPS firmware (updateable on some models)

  # Generator compatibility
  generator_compatible: bool?   # works with generator power (frequency tolerance)
  frequency_tolerance_hz: float?  # input frequency range (generators may drift)

RuntimeEstimate:
  load_percent: uint            # load as percentage of capacity
  load_w: uint?                 # load in watts
  runtime_min: float            # estimated minutes of battery runtime

CascadingSpec:
  max_daisy_chain: uint?        # max units chained (some power strips have pass-through outlets)
  passthrough_outlet: bool?     # has an outlet that passes through to chain
  # Daisy-chaining power strips is generally unsafe and violates electrical
  # codes in most jurisdictions. The compatibility engine SHOULD warn if the
  # user models a chain: "Power strip connected to another power strip.
  # This is unsafe and may violate local electrical codes."
  # PDUs are different -- rack PDUs are designed for high-density deployment
  # and do not have the same daisy-chain issues.
```

Per-outlet metering SHOULD feed measured quality data into the power model.
A metered PDU acts as a fleet of current sensors for AC power; when
per-outlet metrics are available, the implementation SHOULD use them in
preference to device-database estimates.

### Power Distribution in the Routing Graph

Power distribution devices sit in the power graph between the wall outlet
and the equipment. The graph traces power from source to every consumer:

```
Wall outlet (NEMA 5-15, 120 V/15 A = 1800 W max on this circuit)
+-- UPS: APC Smart-UPS 1500 (line-interactive, 1000 W)
    +-- Battery outlets:
    |   +-- Outlet 1 -> Server PSU (measured: 280 W) [via IEC C13 cable]
    |   +-- Outlet 2 -> Network switch (measured: 25 W) [via IEC C13 cable]
    |   +-- Outlet 3 -> NAS (measured: 65 W) [via IEC C13 cable]
    |   Total battery-backed: 370 W of 1000 W capacity (37% load)
    |   Estimated runtime on battery: 22 minutes
    |
    +-- Surge-only outlets:
        +-- Outlet 4 -> Monitor (measured: 45 W)
        +-- Outlet 5 -> Desk lamp (15 W)
        +-- Outlet 6 -> Phone charger (20 W)

Wall outlet 2 (same circuit -- shares 1800 W with outlet 1!)
+-- Power strip: Belkin 12-outlet surge protector (1875 W, 4320 J)
    +-- Outlet 1 -> Audio interface PSU (30 W)
    +-- Outlet 2 -> Monitor 2 (45 W)
    +-- Outlet 3 -> Powered speakers (2x 50 W = 100 W)
    +-- Outlet 4 -> LED strip PSU (60 W)
    +-- Outlet 5-12: empty

Total circuit load: 370 W + 80 W + 235 W = 685 W of 1800 W (38%)
```

The router MUST surface this as a power Sankey diagram. It knows:
- Total circuit capacity and current load
- UPS battery runtime at current load
- Which devices lose power on outage (surge-only outlets)
- Which devices stay up (battery-backed outlets)
- Whether adding a new device would overload the UPS or the circuit
- Per-outlet power via metered PDU (if available) or estimated from
  device database power specs

### UPS Integration with Ozma

The existing NUT integration (`controller/ups_monitor.py`) feeds UPS state
into the graph. With the full UPS model:

1. **Runtime estimation**: "At current load (370 W), battery runtime is
   22 minutes. If the server starts a heavy encode (450 W total), runtime
   drops to 14 minutes."

2. **Graceful shutdown sequencing**: On battery, after X minutes: shut down
   non-essential devices first (switched PDU outlets), then gracefully
   shut down the server, then the NAS. The switched PDU and the UPS
   MUST coordinate via the power graph.

3. **Overload prevention**: "Adding this GPU server (600 W) to the UPS would
   exceed its 1000 W capacity. Move it to a dedicated circuit or upgrade
   the UPS." The router MUST reject configurations that would exceed UPS
   capacity.

4. **Circuit-level awareness**: Two UPS units on the same wall circuit share
   the circuit's amperage limit. The graph MUST model this -- both UPS inputs
   trace to the same circuit.

5. **Battery health trending**: UPS battery capacity degrades over time.
   NUT reports remaining capacity. The implementation SHOULD track trends:
   "UPS battery capacity has dropped 15% in the last year. Runtime at full
   load is now 14 minutes vs 22 minutes when new. Consider battery
   replacement."

### Observability

```
GET /api/v1/routing/power                    # all power rails across all devices
GET /api/v1/routing/power/{device_id}        # power state for a specific device
GET /api/v1/routing/power/{device_id}/rails  # per-rail detail with measurements
GET /api/v1/routing/power/pressure           # devices with power pressure warnings
```

All four endpoints MUST be implemented. The `/pressure` endpoint MUST return
only devices where at least one rail is in a `warning` or `critical` state.

**Events**:

The implementation MUST emit the following events when conditions are met:

```
routing.power.rail_warning       # voltage or current approaching limits
routing.power.rail_critical      # voltage below min or current exceeding capacity
routing.power.budget_exceeded    # device power draw exceeds source capacity
routing.power.pd_negotiated      # USB PD negotiation completed (new voltage/current)
routing.power.battery_low        # battery below threshold
routing.power.rgb_power_limited  # RGB brightness scaled down due to power limit
```
