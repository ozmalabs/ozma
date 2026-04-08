# Physical Environment

Status: Draft

## RFC 2119 Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).

## Abstract

This document specifies the Ozma physical environment model: furniture,
racks, sites, spaces, floor plans, and building management structures. While
furniture is not a device in the routing graph (it has no ports, no data
flows through it), it is a first-class entity in the physical model because
devices are positioned relative to furniture, furniture state affects routing
(desk height informs zone inference and intent binding, chair occupancy
drives presence detection), and furniture defines the spatial structure from
which zones and the 3D scene are built. Physical location MAY be specified
on any entity in the system; the hierarchy is useful at every scale from a
single desk to a multi-site enterprise.

## Specification

### Furniture Entities

```yaml
FurnitureEntity:
  id: string                    # unique identifier
  type: FurnitureType           # what kind of furniture
  name: string                  # human-readable ("Main desk", "Couch")
  device_db_id: string?         # device database entry for dimensions, model, etc.
  location: PhysicalLocation    # where this furniture is
  contains: ContainedEntity[]   # devices and other entities on/in this furniture
  state: FurnitureState?        # current physical state (height, occupancy, etc.)
  actuator_device: DeviceRef?   # if motorised: the actuator device in the routing graph

FurnitureType: enum
  desk                          # work surface (may be sit/stand)
  table                         # non-work table (dining, coffee, conference)
  chair                         # seating for one (office, gaming)
  sofa                          # multi-seat seating
  shelf                         # wall or freestanding shelf
  rack                          # server/network rack (19", 10")
  cabinet                       # closed storage
  stand                         # monitor stand, speaker stand, headphone stand
  mount                         # wall mount, arm mount, ceiling mount
  cart                          # mobile cart or trolley
  custom                        # anything else

ContainedEntity:
  entity_id: string             # device ID, furniture ID, or non-device entity ID
  entity_type: string           # "device", "furniture", "decoration"
  relationship: string          # from RelativeLocation relationships
  slot: string?                 # named slot (for racks: "U12", for desks: "left_side")
  offset: Position3d?           # offset from furniture origin
```

Every `FurnitureEntity` MUST have a unique `id` and a valid `type`. The
`location` field MUST be populated with at least a parent reference or an
absolute position.

Relative positioning MUST resolve when a parent moves. If a desk moves from
one room to another, all devices whose location is relative to that desk
MUST have their resolved absolute positions updated automatically. An
implementation MUST NOT require manual re-positioning of child entities
when their parent entity moves.

### Furniture State

**FurnitureState** -- dynamic physical state that some furniture reports:

```yaml
FurnitureState:
  height_mm: float?             # current height (sit/stand desks)
  height_preset: string?        # matched preset name ("sitting", "standing")
  tilt_deg: float?              # current tilt (monitor arms)
  occupied: bool?               # is someone sitting here? (pressure sensor, camera inference)
  position_preset: string?      # matched position preset
  quality: InfoQuality          # how we know this state
  source: string?               # "ble_sensor", "serial", "camera_inference", "manual"
```

When a motorised furniture entity has an `actuator_device`, state changes
from the actuator MUST be reflected in `FurnitureState`. The `quality` field
MUST follow the standard InfoQuality trust ordering.

**Furniture in zones**: Furniture naturally defines zones. A desk and its
devices form a `workstation` zone. A sofa and TV form a `media` zone. Zones
MAY be defined explicitly, but the system MAY also infer them from furniture
groupings -- all devices whose `location.relative_to` points to the same desk
are in the same zone.

### Racks as Containment

A server rack is furniture that contains nodes, switches, patch panels, UPS
units, and PDUs. The rack is a `FurnitureEntity` of type `rack`. Each rack
unit is a named slot. Devices MUST be placed in slots using the
`ContainedEntity` structure:

```yaml
# Server rack with nodes
id: "rack-1"
type: rack
name: "Server Rack"
device_db_id: "generic-42u-rack"
location:
  physical:
    space: "server_closet"
    zone: "server-zone"
    pos: { x: 0, y: 0, z: 0 }
contains:
  - { entity_id: "pdu-1", entity_type: "device", slot: "pdu-left" }
  - { entity_id: "ups-1", entity_type: "device", slot: "U1-U4" }
  - { entity_id: "switch-1", entity_type: "device", slot: "U5" }
  - { entity_id: "node-server-1", entity_type: "device", slot: "U8" }
  - { entity_id: "node-server-2", entity_type: "device", slot: "U9" }
  - { entity_id: "patch-panel", entity_type: "device", slot: "U6" }
```

Rack slot identifiers SHOULD follow the "U{number}" convention for standard
19-inch rack units, with "U1" at the bottom. Multi-unit devices SHOULD use
range notation ("U1-U4"). Non-unit slots (e.g., vertical PDU mounts) MAY
use descriptive names ("pdu-left", "pdu-right").

### Rooms and Sites

Rooms are containers for zones and furniture. Sites are containers for rooms.
This hierarchy is OPTIONAL -- a single-desk home setup does not need it. But
a multi-room office or a school with multiple labs does:

```yaml
Site:
  id: string
  name: string                  # "Home", "Office HQ", "School Main Building"
  address: string?              # physical address
  timezone: string?             # IANA timezone
  spaces: Space[]

Space:
  id: string
  name: string                  # "Study", "Living Room", "Server Closet", "Lab 3"
  type: SpaceType               # room type
  floor: int?                   # floor number (0 = ground)
  dimensions_mm: Dimensions?    # room dimensions
  zones: SpatialZone[]          # zones within this space
  furniture: FurnitureEntity[]  # furniture in this space
  floor_plan: FloorPlan?        # physical layout with walls, doors, materials (for RF modelling)
  datacentre: DatacentreSpaceSpec?  # datacentre-specific properties (if applicable)
```

When `Site` is defined, every `Space` within it MUST inherit the site's
`timezone` unless the space explicitly overrides it. The `address` field
on a `Site` SHOULD be populated for multi-site deployments to enable
geographic grouping.

### Datacentre Space Properties

```yaml
DatacentreSpaceSpec:
  # --- Rack layout ---
  rows: RackRow[]?              # rack rows in this space
  hot_cold_aisle: bool?         # hot/cold aisle arrangement
  containment: string?          # "hot_aisle_containment", "cold_aisle_containment",
                                # "chimney", "none"
  raised_floor: bool?           # raised floor (cable routing, cold air plenum)
  raised_floor_height_mm: uint? # clearance under raised floor
  overhead_cable_tray: bool?    # overhead cable trays / ladder rack

  # --- Power infrastructure ---
  power_feeds: PowerFeed[]?     # utility/generator feeds to this space
  power_redundancy: string?     # "single_feed", "a_b_redundant", "2n",
                                # "2n_plus_1", "concurrent_maintainable"
  total_power_kw: float?        # total available power for this space
  pue: float?                   # Power Usage Effectiveness (total facility / IT load)
                                # PUE 1.0 = perfect. Typical DC: 1.3-1.6. Good: 1.1-1.2.

  # --- Cooling ---
  cooling_type: string?         # "crac" (computer room AC), "crah" (air handler),
                                # "in_row", "rear_door_heat_exchanger", "liquid",
                                # "free_air", "evaporative"
  cooling_capacity_kw: float?   # total cooling capacity
  target_temp_c: float?         # cold aisle target temperature
  humidity_range: { min: float, max: float }?  # relative humidity range (%)

  # --- Physical security ---
  access_control: string?       # "badge", "biometric", "mantrap", "none"
  cameras: bool?                # CCTV monitoring
  fire_suppression: string?     # "sprinkler", "fm200", "novec_1230", "inergen", "none"

RackRow:
  id: string                    # "row_a", "row_1"
  name: string?
  orientation: string?          # "north_south", "east_west"
  racks: string[]               # rack device IDs in this row, in order
  aisle: string?                # which aisle this row faces ("hot", "cold")
  power_feed: string?           # which power feed serves this row ("a", "b")

PowerFeed:
  id: string                    # "feed_a", "feed_b", "generator"
  source: string                # "utility", "generator", "solar", "ups_output"
  voltage: string               # "208v_3phase", "480v_3phase", "240v_single"
  capacity_a: float             # amperage capacity
  capacity_kw: float?           # kilowatt capacity
  redundant_with: string?       # which other feed provides redundancy
  transfer_switch: string?      # "ats" (automatic transfer switch), "sts" (static), "manual"
  transfer_time_ms: float?      # switchover time (ATS: 10-20 ms, STS: 4-8 ms, manual: minutes)
  # A+B redundancy means every rack has two power feeds from different
  # UPS systems, and every server has dual PSUs -- each on a different feed.
  # If feed A fails, feed B carries 100% of the load.
```

When `power_redundancy` is set to `a_b_redundant`, `2n`, or
`2n_plus_1`, every rack in the space SHOULD have at least two power feeds
from independent sources. The implementation SHOULD warn if any rack has
only a single power feed in a redundancy-declared space.

Posture tighten-only inheritance MUST be enforced: a child space MUST NOT
have a weaker security posture than its parent site. If the site requires
`biometric` access control, no space within it MAY downgrade to `badge` or
`none`. Implementations MUST reject configuration changes that would violate
this constraint.

### Space Types

```yaml
SpaceType: enum
  office                        # private office
  open_plan                     # open plan workspace
  meeting_room                  # conference / meeting room
  lab                           # computer lab, workshop
  studio                        # recording / production studio
  living_room                   # residential living area
  bedroom                       # residential bedroom
  server_room                   # dedicated server/network room
  data_hall                     # datacentre compute hall (rows of racks)
  network_room                  # MDF/IDF, meet-me room, cross-connects
  power_room                    # UPS room, PDU switchboards, generator, battery
  cooling_plant                 # CRAC/CRAH units, chiller plant
  loading_dock                  # receiving, staging
  utility_room                  # storage, mechanical
  outdoor                       # patio, garden, yard
  classroom                     # teaching space
  common_area                   # kitchen, break room, hallway
  custom                        # anything else
```

### Floor Plans

**FloorPlan** -- physical layout of a space with walls, doors, windows, and
construction materials. Enables WiFi coverage prediction, LoRa/BLE range
estimation, audio acoustics modelling, and cable run planning:

```yaml
FloorPlan:
  source: string?               # "manual", "imported_dxf", "imported_image",
                                # "imported_pdf", "3d_scan"
  scale: float?                 # if traced from image: mm per pixel
  origin: Position2d?           # floor plan coordinate origin
  walls: Wall[]
  doors: Door[]?
  windows: Window[]?
  floors: FloorSurface[]?       # floor material (affects acoustics + cable routing)
  ceilings: CeilingSurface[]?   # ceiling material and height
  columns: Column[]?            # structural columns
  cable_paths: CablePath[]?     # known cable routes (conduit, trunking, floor void, overhead)

Wall:
  id: string
  start: Position2d             # start point (mm)
  end: Position2d               # end point (mm)
  height_mm: float?             # wall height (default: room height)
  thickness_mm: float?          # wall thickness
  material: WallMaterial        # construction material (determines RF attenuation)
  exterior: bool?               # is this an external wall?
  load_bearing: bool?           # structural (cannot be modified)

WallMaterial:
  type: string                  # material type (see RF attenuation table below)
  rf_attenuation_2_4ghz_db: float?  # signal loss at 2.4 GHz
  rf_attenuation_5ghz_db: float?    # signal loss at 5 GHz
  rf_attenuation_6ghz_db: float?    # signal loss at 6 GHz (WiFi 6E/7)
  acoustic_stc: uint?           # Sound Transmission Class (for audio leakage)
  # Custom materials MAY specify measured attenuation. Standard materials
  # use lookup values from the table below.
```

Floor plan wall materials SHOULD include RF attenuation values. When standard
material types are used, the implementation SHOULD apply the default lookup
values from the table below. Custom materials MAY override these defaults
with measured values.

**Standard wall material RF attenuation**:

| Material | 2.4 GHz | 5 GHz | 6 GHz | Notes |
|----------|---------|-------|-------|-------|
| plasterboard (drywall) | 3 dB | 4 dB | 5 dB | Standard interior partition |
| plasterboard_double | 5 dB | 7 dB | 9 dB | Double-layer drywall |
| brick_single | 6 dB | 10 dB | 12 dB | Single brick (110 mm) |
| brick_double | 10 dB | 18 dB | 22 dB | Double brick cavity wall |
| concrete_100mm | 10 dB | 15 dB | 18 dB | Poured concrete |
| concrete_200mm | 15 dB | 23 dB | 28 dB | Thick concrete (fire wall, lift shaft) |
| concrete_block | 8 dB | 12 dB | 15 dB | Concrete masonry unit |
| glass_single | 2 dB | 3 dB | 4 dB | Single glazing |
| glass_double | 4 dB | 6 dB | 8 dB | Double glazing |
| glass_tinted | 5 dB | 8 dB | 10 dB | Tinted/coated glass (metal oxide) |
| glass_low_e | 8 dB | 15 dB | 20 dB | Low-E glass (metallic coating -- WiFi killer!) |
| wood_door | 3 dB | 4 dB | 5 dB | Solid wood door |
| hollow_door | 2 dB | 3 dB | 3 dB | Hollow-core interior door |
| metal_door | 10 dB | 15 dB | 18 dB | Steel security door |
| fire_door | 6 dB | 10 dB | 12 dB | Fire-rated door (dense core) |
| metal_stud | 4 dB | 6 dB | 8 dB | Metal stud partition with plasterboard |
| curtain_wall | 6 dB | 10 dB | 15 dB | Glass curtain wall (aluminium frame) |
| elevator_shaft | 20 dB | 30 dB | 35 dB | Concrete + steel (essentially opaque) |
| metal_clad | 15 dB | 25 dB | 30 dB | Metal-clad wall (warehouse, server room) |
| floor_concrete | 12 dB | 18 dB | 22 dB | Concrete floor/ceiling between levels |
| floor_wood | 5 dB | 8 dB | 10 dB | Timber floor between levels |

These are typical values. Actual attenuation varies with construction
quality, age, moisture content, and exact thickness. Community-verified
measurements for specific buildings MAY override the defaults.

### Doors, Windows, and Structural Elements

```yaml
Door:
  id: string
  position: Position2d          # centre point on the wall
  wall_id: string               # which wall this door is in
  width_mm: float
  height_mm: float?
  material: WallMaterial        # door material (wood, metal, glass, fire-rated)
  normally_open: bool?          # is this door typically left open? (affects RF model)
  # An open door is ~0 dB attenuation. A closed hollow-core door is ~2 dB.
  # A closed fire door is ~10 dB. Whether the door is open or closed
  # significantly affects WiFi prediction. The model defaults to the
  # `normally_open` state but MAY be overridden by sensor data (door
  # contact sensor).

Window:
  id: string
  position: Position2d          # centre point on the wall
  wall_id: string               # which wall this window is in
  width_mm: float
  height_mm: float
  material: WallMaterial        # glass type (single, double, low-E, tinted)
  # Low-E glass deserves special attention -- the metallic coating that
  # reflects heat also reflects RF. A building with floor-to-ceiling low-E
  # glass can have 15-20 dB attenuation at 5 GHz per window. This is why
  # modern office buildings often have terrible WiFi -- the glass walls
  # that let light through block radio.

FloorSurface:
  material: string              # "concrete", "raised_floor", "hardwood", "carpet",
                                # "tile", "vinyl"
  rf_attenuation_db: float?     # between-floor attenuation (if modelling multi-storey)
  cable_accessible: bool?       # can cables be run under this floor?
  void_depth_mm: float?         # raised floor void depth (for cable routing)

CeilingSurface:
  height_mm: float              # ceiling height
  material: string              # "plasterboard", "acoustic_tile", "exposed_concrete",
                                # "suspended_grid", "exposed_structure"
  plenum: bool?                 # is the space above the ceiling a plenum?
  cable_accessible: bool?       # can cables be run above this ceiling?

Column:
  position: Position2d
  diameter_mm: float?           # for circular columns
  dimensions_mm: Dimensions?    # for rectangular columns { w, d }
  material: string              # "concrete", "steel"
  # Columns are RF obstacles -- concrete columns can shadow WiFi significantly.

CablePath:
  id: string
  type: string                  # "conduit", "trunking", "floor_void", "ceiling_void",
                                # "cable_tray", "wall_chase", "external"
  waypoints: Position2d[]       # path through the building
  capacity: string?             # how many cables can this path carry
  current_usage: string?        # approximately how full
  accessible: bool              # can new cables be pulled through this path?
```

Every `Door` and `Window` MUST reference a valid `wall_id`. The
implementation MUST validate that the referenced wall exists in the same
floor plan. Doors with `normally_open` set to `true` SHOULD use 0 dB
attenuation in RF models; closed doors MUST use the attenuation of their
`material`.

### What the Floor Plan Enables

1. **WiFi coverage prediction**: Place an AP on the floor plan and the system
   calculates signal strength at every point using the wall materials and
   distances. "Your AP in the study gives -72 dBm in the bedroom (through
   2 plasterboard walls at 3+3 dB = 6 dB loss). That is marginal for 5 GHz.
   Consider a second AP or switch to 2.4 GHz for the bedroom."

2. **AP placement optimisation**: "Given these walls, the optimal placement
   for a single AP to cover all rooms is here. For two APs, place them
   here and here. Channel assignment: AP1 on channel 36, AP2 on channel
   149 to avoid co-channel interference."

3. **BLE/LoRa range estimation**: Same attenuation model, different
   frequencies. BLE beacon in the living room: "Signal reaches the garage
   through a brick wall at -85 dBm (marginal). Place a second beacon near
   the garage door."

4. **Cable run planning**: "The shortest Cat6a run from the server room
   patch panel to Office 3 is 28 m via the ceiling void (accessible),
   through the corridor, down the wall chase. Cat6a supports 10GbE at
   this distance."

5. **Audio leakage**: Wall STC (Sound Transmission Class) values predict
   how much sound bleeds between rooms. "Your studio shares a plasterboard
   wall (STC 33) with the bedroom. Loud monitoring will be clearly audible
   next door. Consider adding mass-loaded vinyl for STC 45+."

6. **Security camera coverage**: Place cameras on the floor plan and
   calculate field-of-view coverage considering walls and obstructions.
   "This camera covers 80% of the hallway. The column at position
   (3200, 1500) creates a blind spot of 2.5 m^2."

### Floor Plan Import

Users SHOULD NOT have to draw walls from scratch. The system SHOULD accept:
- **Image trace**: Upload a floor plan image (PDF, PNG from architect or
  real estate listing), set the scale, trace walls over the image
- **DXF/DWG import**: Import from CAD drawings (architects provide these)
- **Simple drawing**: Draw walls, doors, windows in the dashboard editor
- **3D scan import**: From LiDAR scans (iPhone Pro, Matterport) -- generates
  walls and dimensions automatically
- **Template**: Common house/apartment layouts as starting points

### Accuracy

The RF attenuation values are estimates. Real-world results vary with
construction quality, moisture, furniture, and the specific frequency and
antenna pattern. The model provides a useful prediction (+/- 5 dB) without
requiring a professional RF survey. Community-measured values for specific
building types MAY improve accuracy over time.

### Building Management Devices

Locks, HVAC, lighting, and occupancy sensors are devices in the graph with
control paths, state, events, and physical locations. They use the same
primitives as everything else -- the building management system is not a
separate product, it is a natural extension of the routing graph into the
physical environment. Building management devices MAY use intent bindings
for automation.

Posture tighten-only inheritance MUST be enforced at space level -- a space
inherits the security posture of its parent site, and MAY tighten but MUST
NOT relax it.

```yaml
LockSpec:
  lock_type: string             # "smart_lock", "badge_reader", "keypad",
                                # "intercom", "electric_strike", "mag_lock",
                                # "turnstile", "garage_door"
  authentication: string[]      # ["badge_rfid", "badge_nfc", "pin", "fingerprint",
                                # "face", "ble_phone", "key", "remote", "buzzer"]
  credential_formats: string[]? # ["hid_iclass", "hid_prox", "mifare_classic",
                                # "mifare_desfire", "em4100", "nfc_ndef"]
  state: LockState
  control_interface: string?    # "zwave", "zigbee", "ble", "wifi", "wiegand",
                                # "osdp", "ip", "dry_contact"
  camera: bool?                 # integrated camera (video doorbell, intercom)
  two_way_audio: bool?          # intercom capability
  auto_lock: bool?              # re-locks after timeout
  door_sensor: bool?            # detects door open/closed (not just locked/unlocked)
  tamper_detection: bool?       # detects physical tampering
  battery: BatterySpec?         # for battery-powered locks
  fire_release: bool?           # unlocks on fire alarm (safety requirement)
  fail_mode: string?            # "fail_secure" (stays locked on power loss) or
                                # "fail_safe" (unlocks on power loss -- for fire exits)

LockState:
  locked: bool
  door_open: bool?              # door position (if sensor present)
  last_access: LockAccessEvent?
  battery_percent: float?

LockAccessEvent:
  timestamp: timestamp
  action: string                # "unlock", "lock", "denied", "forced_entry", "held_open"
  method: string?               # "badge", "pin", "fingerprint", "ble", "remote", "key", "exit_button"
  credential_id: string?        # which badge/code was used (not the person -- privacy)
  # Access events feed into the state change journal and MAY
  # trigger intent bindings: "office door unlocked -> lights on,
  # HVAC to occupied mode, WiFi APs at full power"

HvacSpec:
  hvac_type: string             # "thermostat", "crac", "crah", "split_system",
                                # "fan_coil", "vrf", "ptac", "baseboard", "radiant"
  heating: bool?
  cooling: bool?
  fan: bool?
  humidity_control: bool?
  temperature_range_c: { min: float, max: float }?
  control_interface: string?    # "zwave", "zigbee", "wifi", "modbus", "bacnet",
                                # "ip", "ir", "dry_contact"
  zones: uint?                  # number of independently controlled zones
  schedule: bool?               # supports scheduling
  occupancy_aware: bool?        # has or accepts occupancy input
  state: HvacState

HvacState:
  mode: string                  # "heating", "cooling", "auto", "fan_only", "off"
  current_temp_c: float?
  target_temp_c: float?
  humidity_percent: float?
  fan_speed: string?            # "auto", "low", "medium", "high"
  running: bool?                # compressor/heater currently active

LightingSpec:
  lighting_type: string         # "smart_switch", "smart_dimmer", "smart_bulb",
                                # "scene_controller", "occupancy_switch",
                                # "daylight_harvesting", "emergency_lighting"
  dimmable: bool?
  color_temp: bool?             # adjustable color temperature (warm/cool white)
  color: bool?                  # full RGB color (overlaps with rgb device type -- this is room lighting, not accent/indicator)
  control_interface: string?    # "zwave", "zigbee", "wifi", "dali", "dmx",
                                # "0_10v", "phase_cut", "ip"
  wattage: float?
  lumens: float?
  circuits: uint?               # number of independently controllable circuits
  emergency: bool?              # emergency lighting (battery backup, legally required)
  state: LightingState

LightingState:
  on: bool
  brightness_percent: float?    # 0-100
  color_temp_k: uint?           # color temperature in Kelvin
  scene: string?                # active scene name

OccupancySpec:
  sensor_type: string           # "pir", "ultrasonic", "dual_tech", "mmwave_radar",
                                # "camera_ai", "desk_sensor", "thermal_array",
                                # "people_counter", "ble_beacon", "wifi_probe"
  detection_range_m: float?     # maximum detection range
  detection_angle_deg: float?   # field of detection
  people_counting: bool?        # can count people, not just detect presence
  max_count: uint?              # maximum people it can count simultaneously
  direction_detection: bool?    # can detect entry vs exit direction
  desk_level: bool?             # per-desk occupancy (under-desk sensor or booking system)
  control_interface: string?    # "zigbee", "ble", "wifi", "dry_contact", "ip"
  state: OccupancyState

OccupancyState:
  occupied: bool
  count: uint?                  # number of people detected
  last_motion: timestamp?       # when motion was last detected
  confidence: float?            # 0.0-1.0 detection confidence
```

#### Intent Binding Examples

These devices do not need custom building management logic -- the existing
intent binding system handles it. Building management devices MAY use intent
bindings for automation:

```yaml
# Office opens -- first person arrives
- conditions:
    - { source: lock, field: state.action, op: eq, value: "unlock" }
    - { source: occupancy, field: state.occupied, op: eq, value: false }
      # was unoccupied, now someone unlocked the door
  actions:
    - { type: hvac.set_mode, target: "office_thermostat", mode: "auto", temp: 22 }
    - { type: lighting.on, target: "office_lights", brightness: 80 }
    - { type: ap.enable, target: "office_ap" }

# Office empties -- last person leaves
- conditions:
    - { source: occupancy, field: state.count, op: eq, value: 0 }
    - { source: occupancy, field: state.last_motion, op: age_gt, value: 600 }
      # no motion for 10 minutes
  actions:
    - { type: hvac.set_mode, target: "office_thermostat", mode: "off" }
    - { type: lighting.off, target: "office_lights" }
    - { type: ap.disable, target: "office_ap" }  # disable WiFi to save power
    - { type: lock.lock, target: "office_door" }

# After hours -- building unoccupied
- conditions:
    - { source: time, field: hour, op: gt, value: 22 }
    - { source: occupancy, field: state.count, op: eq, value: 0 }
  actions:
    - { type: hvac.setback, temp: 16 }            # heating setback
    - { type: lighting.off, target: "all" }
    - { type: ap.reduce_power, target: "all" }    # reduce WiFi to minimum
    - { type: security.arm, target: "alarm_panel" }

# Meeting room booked -- pre-condition
- conditions:
    - { source: calendar, field: event_active, op: eq, value: true }
    - { source: calendar, field: event_room, op: eq, value: "meeting_room_1" }
  actions:
    - { type: hvac.set_mode, target: "meeting_room_1_hvac", mode: "cooling", temp: 21 }
    - { type: lighting.scene, target: "meeting_room_1_lights", scene: "presentation" }
    - { type: display.power_on, target: "meeting_room_1_screen" }
    # Pre-cool and light the room before the meeting starts
```

What Ozma adds beyond Home Assistant: HA can do occupancy-driven
automation. The difference is that Ozma's graph also knows the IT
infrastructure -- when the office empties, Ozma does not just turn off
lights, it also: reduces AP transmit power (saves energy, reduces RF
exposure), cools warm pipelines (saves server power), adjusts thermal
profiles on servers (quiet mode when nobody is there to hear fans),
pauses non-essential monitoring refresh cycles (saves CPU and I/O on
constrained devices), and arms the security system. The building
automation and IT management share one event bus, one intent system,
one graph.

### Progressive Disclosure

Physical environment is OPTIONAL at every level. A user with one desk and
two machines needs none of this -- their devices just have bus-level
locations. A user who wants spatial RGB adds furniture positions. A business
with multiple rooms adds spaces and zones. An MSP managing 50 sites adds
the full hierarchy. A user who wants WiFi coverage prediction adds a floor
plan with wall materials. Each level is independently useful and none is
REQUIRED.
