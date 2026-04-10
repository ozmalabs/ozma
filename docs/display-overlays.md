# Ozma Display — Overlays, Smart Mirror, and Information Display

> MagicMirror² is the reference point for information display. Ozma Display
> meets feature parity and then goes further in every direction that matters:
> overlays render on top of live KVM streams, data feeds the same metric bus
> that drives Stream Decks and node status, proximity sensing integrates with
> the existing camera and sensor infrastructure, and face recognition
> personalises layouts using Frigate models already running on the mesh.

---

## The overlay concept

MagicMirror renders widgets onto a blank screen. That is useful when the screen
is dedicated to information display — a mirror, a lobby panel, a dashboard TV.

Ozma Display can do the same. But it can also render widgets as a **composited
layer floating over a live KVM stream**. The machine's desktop is visible
underneath; the widget layer sits on top like a HUD.

```
┌────────────────────────────────────────────────┐
│  ┌──────────────────────────────────────────┐  │
│  │                                          │  │
│  │    Machine A desktop (live stream)       │  │
│  │                                          │  │
│  │                                          │  │
│  │                                          │  │
│  └──────────────────────────────────────────┘  │
│                                                │
│  [HUD layer — compositor surface above stream] │
│  ┌─────────┐  ┌───────────────┐  ┌──────────┐ │
│  │ 14:32   │  │ Next: standup │  │ 🔔 Front │ │
│  │ Tue 10  │  │ in 8 minutes  │  │   door   │ │
│  └─────────┘  └───────────────┘  └──────────┘ │
└────────────────────────────────────────────────┘
```

In OzmaOS (Wayland compositor mode) the HUD layer is a `wlr-layer-shell`
surface rendered above the stream surface. In app mode it is a borderless
always-on-top window. In embedded mode it is a framebuffer plane above the
video plane, hardware-composited by the display engine.

The overlay is **input-transparent**: mouse clicks and keyboard events pass
through it to the stream below, unless the user is directly interacting with
a widget (tap on a notification, click a camera thumbnail).

### Overlay modes

| Mode | Behaviour | Triggered by |
|---|---|---|
| Persistent | Widgets always visible at reduced opacity | Default |
| Edge reveal | Widgets slide in when cursor reaches screen edge | Mouse proximity |
| Motion reveal | Appear when idle cursor detected | Timeout |
| Auto-hide | Hide on first keystroke, reappear after N seconds idle | Active typing |
| Clean | All overlays hidden | Gaming / presentation scenario |
| Notification-only | Hidden except when an alert fires | Focus mode |

Overlay mode is set per-scenario. Switch to Gaming → Clean mode activates
automatically. Switch to Work → Persistent mode with calendar and clock.

---

## Layout zones and positioning

The existing widget layout uses absolute pixel coordinates. For full-screen
information displays (smart mirror, lobby panel) the higher-friction approach
to positioning makes sense. For overlays on top of variable-resolution streams
it does not.

Ozma Display adds a **zone-based positioning system** alongside absolute
coordinates. Zones are proportional regions of the display surface that
scale with the screen resolution:

```
┌────────────┬──────────────────┬────────────┐
│            │                  │            │
│  top_left  │   top_center     │  top_right │
│            │                  │            │
├────────────┤                  ├────────────┤
│            │                  │            │
│ middle_left│     center       │middle_right│
│            │                  │            │
├────────────┤                  ├────────────┤
│            │                  │            │
│bottom_left │  bottom_center   │bottom_right│
│            │                  │            │
└────────────┴──────────────────┴────────────┘
```

This is intentionally the same nine-zone layout as MagicMirror² to make
porting MagicMirror modules straightforward. `fullscreen_above` and
`fullscreen_below` are also supported.

Widgets specify `zone` instead of `x/y` and the layout engine places them.
Absolute coordinates override zone positioning for precise control.

---

## Data sources — external integrations

The current metric system drives widgets from device metrics (CPU temperature,
audio levels, node status). For information display parity with MagicMirror,
the same metric bus needs to carry **external data**:

### Data provider modules

Data providers are controller-side modules that fetch external data and publish
it as named metrics. Widgets bind to these metrics exactly as they bind to
device metrics. All display surfaces — Stream Decks, OLEDs, mirrors, monitors —
automatically have access.

**Built-in data providers at parity with MagicMirror:**

| Provider | Metrics published | Config |
|---|---|---|
| `weather` | `weather.temp`, `weather.feels_like`, `weather.condition`, `weather.humidity`, `weather.wind_speed`, `weather.forecast_N_{field}` (N=0–6 days) | lat/lon, provider (OpenMeteo default — no API key required, Open-Meteo is FOSS), units |
| `calendar` | `calendar.next_event_title`, `calendar.next_event_time`, `calendar.next_event_in_min`, `calendar.events` (JSON array), `calendar.today_count` | iCal URLs, Google Calendar OAuth, local .ics |
| `news` | `news.item_N_title`, `news.item_N_source`, `news.item_N_age` | RSS/Atom URLs, N items, update interval |
| `commute` | `commute.home_to_work_min`, `commute.work_to_home_min`, `commute.route_summary` | Origin/destination coordinates, provider |
| `transit` | `transit.departure_N_line`, `transit.departure_N_in_min`, `transit.departure_N_dest` | Stop ID, GTFS-RT endpoint |
| `stocks` | `stocks.{ticker}.price`, `stocks.{ticker}.change_pct` | Tickers, provider (Yahoo Finance default) |
| `crypto` | `crypto.{symbol}.price_usd`, `crypto.{symbol}.change_24h_pct` | Symbols, currency |
| `now_playing` | `media.title`, `media.artist`, `media.album`, `media.art_url`, `media.progress_pct`, `media.state` | Spotify OAuth, Last.fm, HA media player, local MPD |
| `air_quality` | `air.aqi`, `air.pm25`, `air.pm10`, `air.condition` | lat/lon or station ID |
| `astronomy` | `astro.sunrise`, `astro.sunset`, `astro.moon_phase`, `astro.moon_illumination` | lat/lon |
| `quote` | `quote.text`, `quote.author` | Source (ZenQuotes, local file, custom API) |
| `compliment` | `compliment.text` | Time-of-day sets, custom sets, tone |
| `countdown` | `countdown.{id}.days`, `countdown.{id}.label` | Target dates with labels |
| `ha_entity` | `ha.{entity_id}.state`, `ha.{entity_id}.{attribute}` | HA base URL + token |
| `public_ip` | `network.public_ip`, `network.public_ip_changed` | Update interval |

**Ozma-native data providers (no MagicMirror equivalent):**

| Provider | Metrics published |
|---|---|
| `mesh_status` | Per-node online/offline, active scenario, last-seen, assurance level |
| `audio_routing` | Active audio paths, current volume per node, now-playing from any node |
| `camera_events` | Latest Frigate detection events, event counts by zone |
| `network_health` | Per-node latency, jitter, packet-loss from network_health.py |
| `backup_status` | Per-node backup health, last success, days since backup |
| `compliance` | Overall compliance score, failing controls |
| `ups_status` | Battery %, runtime, on-battery state |
| `wan_speed` | Last speedtest download/upload/ping |

All providers poll or stream on configurable intervals and push updates into
the metric bus as they arrive. A weather fetch failure publishes a stale
indicator metric, not a crash.

---

## Inter-widget event bus

MagicMirror's `notificationReceived()` / `sendNotification()` allows modules
to communicate. Ozma does not currently have this.

The addition is lightweight: a **named event bus** where widgets can subscribe
to event topics and other widgets (or data providers, or external triggers)
can fire events. This maps onto the existing `state.events` queue that already
drives WebSocket clients.

```python
# Data provider fires
await state.events.put({
    "type": "display.event",
    "topic": "doorbell.ring",
    "payload": {"camera_id": "front_door", "snapshot_url": "..."}
})

# Widget subscribes in its layout definition
{
    "type": "camera_popup",
    "trigger": "doorbell.ring",    # activates when this event fires
    "zone": "top_right",
    "auto_dismiss_s": 30
}
```

Standard event topics:

| Topic | Fired by | Typical response |
|---|---|---|
| `doorbell.ring` | DoorbellManager | Camera PiP pops up in corner |
| `calendar.upcoming` | calendar provider (15 min before) | Meeting reminder banner |
| `alert.motion` | Frigate / camera_events | Camera thumbnail |
| `scenario.changed` | ScenarioManager | Overlay mode transition |
| `media.changed` | now_playing provider | Now-playing bar animates in |
| `node.offline` | NetworkHealthMonitor | Node status badge changes |
| `weather.alert` | weather provider | Full-width alert banner |
| `ups.on_battery` | UPS monitor | Battery warning banner |
| `presence.away` | presence provider | Display dims / switches mode |
| `presence.arrived` | presence provider | Personalized layout loads |

Widgets can also fire events — a "snooze" button on a calendar reminder fires
`calendar.reminder.snoozed`, which the calendar provider picks up and adjusts
its next notification timing.

---

## Layout pages and scenes

MagicMirror has `MMM-Pages` for switching between layouts. Ozma Display has
this built in via the scenario system, extended with display-specific scenes.

A **scene** is a named overlay/display layout — which widgets show, in which
zones, with which data bindings and overlay mode. Scenes are stored on the
controller. A display client loads its assigned scene on connect and switches
scenes on command.

Scene triggers:

| Trigger | Example |
|---|---|
| Scenario switch | Gaming scenario → clean scene; Work → calendar+clock scene |
| Time of day | 06:30 → morning scene (weather, commute, calendar); 22:00 → night scene (clock only) |
| Presence detected | Someone approaches mirror → active scene |
| Presence cleared | Room empty → screensaver / off scene |
| Face recognised | Person A → their personal scene |
| Calendar event starting | Meeting in 5 min → meeting-prep scene |
| Manual override | Voice command or button press |

Scene definitions are JSON, stored alongside scenario definitions, and pushed
to display clients via the existing config WebSocket. A display with no active
scene shows the default scene.

---

## Proximity and presence sensing

MagicMirror uses PIR sensors (MMM-Pir) to blank the display when no one is
present. Ozma has the sensor infrastructure via `device_metrics.py` and the
camera infrastructure via Frigate.

### PIR / distance sensor

Any Ozma node with GPIO access can read a PIR or ToF distance sensor and
publish `presence.detected` / `presence.cleared` metrics. These are already
supportable via the node's sensor publishing path. A display-side GPIO pin
(on a Pi) can publish directly.

### Camera-based presence (Frigate)

If a Frigate camera covers the display area, Frigate's person detection
serves as a higher-quality presence sensor with zero extra hardware:

- Person detected in zone → `presence.arrived` event → active scene loads
- Zone clear for N seconds → `presence.away` event → screensaver / display off
- Person count tracked (meeting room: 0/1/2+ people adjust layout)

### Distance-to-display estimation

Frigate bounding box size can estimate how close someone is to the display:

- Far (approaching): simplified scene, large text
- Near (reading distance): full scene with fine detail
- Very close (interacting): touch/gesture mode

This requires calibration per installation but the data is already in the
Frigate event stream.

---

## Face recognition and personalised layouts

MagicMirror has a Facerec module. Ozma can use Frigate's built-in face
recognition (available in Frigate+ / self-hosted models) since Frigate is
already part of the Ozma camera stack.

On recognition:

1. Frigate fires a person event with `sub_label` = recognised person name
2. The camera_events provider publishes `presence.person` = name
3. The display event bus fires `presence.recognised` with person metadata
4. The display client loads that person's named scene

Each user registered in Ozma (via the user manager) can have:

- A named display scene (their layout preferences)
- A greeting compliment set
- Calendar source bindings (their calendars, not the household default)
- Media source preference (their Spotify account)
- Preferred language / units

For a household mirror: each family member walks up and sees their own
calendar, their own commute time, a personalised greeting. Without face
recognition, proximity alone triggers a common household scene.

---

## Smart mirror mode

Smart mirror hardware is a two-way mirror with a display mounted behind it.
The reflective coating is semi-transparent: you see your reflection in the
mirror, and the display is visible through the coating wherever the screen
is lit. Black pixels are invisible (the mirror shows through); lit pixels
appear as bright graphics overlaid on your reflection.

Ozma Display in **mirror mode** renders on a black background. Widget zones
correspond to the physical areas of the mirror where graphics are visible —
typically the border area, with the centre left dark (reflection zone).

```
┌─────────────────────────────────────────────┐
│  [time]         [date]         [weather]    │  ← top zone
│                                             │
│                                             │
│                                             │  ← centre dark
│           (reflection area)                 │     (mirror shows through)
│                                             │
│                                             │
│  [calendar]                  [commute]      │  ← bottom zone
│  [news]                      [transit]      │
└─────────────────────────────────────────────┘
```

In mirror mode, the centre zone is reserved by default. An arriving event
(doorbell, urgent alert) can temporarily illuminate the centre with a camera
feed or large notification — and return to dark when dismissed.

### Mirror hardware configurations

| Config | Hardware | Use case |
|---|---|---|
| Portrait mirror | 24–32" display, 2-way glass, Pi 5 | Hallway / bathroom mirror |
| Landscape mirror | 32–55" display, 2-way glass, Pi 5 or N100 | Living room / gym |
| Window display | Standard monitor, Pi, no glass | Dashboard window, always-on panel |
| Lobby panel | Commercial display, mini-PC, wall mount | Meeting room door, office entry |
| Bedside display | Small 7–10" display, Pi Zero 2W | Night mode, alarm, sleep tracking |

The Ozma Display image ships with a `mirror` display profile that:
- Sets background to pure black
- Disables stream sources (no remote desktop content on a mirror)
- Enables the external data providers
- Enables proximity-based scene switching
- Configures default zone layout

---

## Widget types — additions for information display

The current 11 built-in widget types cover device metrics well. Information
display parity requires additional types:

| New type | Description | MagicMirror equivalent |
|---|---|---|
| `weather_current` | Icon + temperature + condition, localised | CurrentWeather |
| `weather_forecast` | N-day forecast strip with icons | WeatherForecast |
| `calendar_list` | Upcoming events list with time-until | Calendar |
| `news_ticker` | Scrolling headline bar or stacked list | NewsFeed |
| `now_playing` | Album art + title + artist + progress bar | MMM-Spotify |
| `transit_board` | Departure board format (line, dest, minutes) | MMM-Traffic |
| `compliment` | Time-of-day text message | Compliments |
| `countdown` | Days to labelled target date | MMM-CountDown |
| `camera_thumb` | Last Frigate snapshot for a camera | (no equivalent) |
| `notification_banner` | Animated slide-in banner for events | Alert |
| `camera_popup` | Temporary PiP triggered by event | (no equivalent) |
| `map_route` | Commute route with traffic colouring | MMM-Traffic |
| `media_art` | Large album art for now-playing | (no equivalent) |
| `person_greeting` | Personalised greeting + name | Compliments + Facerec |
| `page_indicator` | Dot/bar indicator for multi-scene display | MMM-Page-Indicator |
| `ha_state` | Home Assistant entity state + icon | (requires third-party MM) |
| `ha_history` | HA entity history sparkline | (no equivalent) |
| `webview` | Embedded URL (for third-party web widgets) | (Electron renders all) |
| `qr_code` | Generated QR for any URL or text | (no equivalent) |
| `markdown` | Rendered markdown text block | (no equivalent) |
| `iframe_widget` | Sandboxed web content (weather radar, maps) | iframe in MM |

---

## Community widget packs

MagicMirror has 1000+ community modules. Ozma's widget pack system already
supports installable packs from Connect. The gap is ecosystem size, not
architecture.

To grow the ecosystem:

**Pack format** (extend existing `widget_packs/` system):

```
my-widget-pack/
  manifest.json         name, version, author, ozma_min_version, permissions
  widgets/
    my_widget.json      widget type definition + options schema
  providers/
    my_provider.py      data provider module (runs on controller)
  renderer/
    my_widget.js        client-side renderer for Tier 1 (server frames)
    my_widget_native.js client-side renderer for Tier 2 (native endpoints)
  preview/
    screenshot.png      Connect marketplace preview image
  README.md
```

**Data provider API** (the missing piece for MagicMirror parity):

A provider module is a Python class with a standard interface:

```python
class MyDataProvider:
    METRICS: list[str] = ["my_source.value", "my_source.label"]
    UPDATE_INTERVAL: float = 60.0   # seconds

    async def fetch(self) -> dict[str, Any]:
        # Fetch from external API, return {metric_key: value}
        ...
```

The controller discovers providers from installed packs, instantiates them,
and merges their output into the metric bus. Providers declare what metrics
they publish so the widget editor can offer metric completion.

**Permission model**: community packs declare permissions they require —
`internet` (outbound HTTP), `ha_read`, `mesh_read`, `camera_read`. The
controller prompts for approval on install. A pack without `internet`
permission cannot make outbound requests.

---

## MagicMirror feature parity table

| MagicMirror feature | Ozma equivalent | Status |
|---|---|---|
| Clock / date / timezone | `clock` widget (built-in) | Done |
| Current weather | `weather_current` widget + `weather` provider | Planned |
| Weather forecast | `weather_forecast` widget + `weather` provider | Planned |
| Calendar (Google, iCal) | `calendar_list` widget + `calendar` provider | Planned |
| News / RSS | `news_ticker` widget + `news` provider | Planned |
| Compliments | `compliment` widget + `compliment` provider | Planned |
| Traffic / commute time | `map_route` widget + `commute` provider | Planned |
| Now playing (Spotify etc.) | `now_playing` widget + `now_playing` provider | Planned |
| Stock ticker | `number`/`label` + `stocks` provider | Planned |
| System stats (CPU/RAM) | `gauge`/`bar` + existing device metrics | Done |
| Alert / notification banner | `notification_banner` widget + event bus | Planned |
| PIR / proximity sensing | device_metrics GPIO + presence provider | Planned |
| Face recognition | Frigate sub_label + presence.recognised event | Planned |
| Personalised layouts | Named scenes per user, face-triggered | Planned |
| Module inter-communication | Widget event bus (display.event topics) | Planned |
| Module backend (Node.js) | Data provider Python API | Planned |
| 9-zone positioning | Zone-based layout (top_left … bottom_right) | Planned |
| Page / layout switching | Scenes (controller-stored, event-triggered) | Planned |
| Display on/off (proximity) | presence.away event → screen off | Planned |
| Update notification | Part of OTA update system | Planned |
| Community modules (1000+) | Widget pack marketplace on Connect | Planned |
| Photo slideshow | `image` widget with URL rotation (provider) | Planned |
| Countdown to date | `countdown` widget + `countdown` provider | Planned |
| iFrame embed | `iframe_widget` type | Planned |

**Ozma-only (no MagicMirror equivalent):**

| Feature | Description |
|---|---|
| HUD overlay over KVM stream | Widgets float above live machine desktop |
| Mesh node status | Every machine's online/offline/scenario state |
| Audio routing visualisation | Live audio graph, now-playing from any node |
| Camera event popups | Doorbell ring → camera PiP auto-appears |
| Per-scenario overlay mode | Gaming → clean; Work → full HUD |
| Edge crossing in compositor | Cursor flows between machine surfaces |
| Reverse HID | Keyboard/mouse routes back to controller |
| Unified metric bus | Same data available to Stream Deck, OLED, mirror, monitor |
| HA entity metrics | Any HA entity state as a bindable metric |
| Frigate-native events | Camera events are first-class display triggers |
| QR code widget | Generate/display QR for any content |
| Hardware-accelerated composition | DRM planes, not DOM/Electron |
| Multi-surface synchronisation | Mirror + Stream Deck + OLED show coordinated state |
| Security (WireGuard mesh) | All data stays on your infrastructure |

---

## Implementation path

Widget overlays and the smart mirror feature set are **Ozma Display Phase 3+**,
building on the compositor work. However, data providers and the zone layout
system can ship in Phase 1 — they run on the controller and serve the existing
screen manager devices (Stream Decks, OLEDs) today, independently of the
display client.

**Phase 1 additions (controller-side, no display client needed):**
- Data provider framework + provider API
- Weather, calendar, news, stocks, crypto, now-playing providers
- Zone-based layout system in `screen_widgets.py`
- Widget event bus wired to `state.events`
- New widget types: `weather_current`, `weather_forecast`, `calendar_list`,
  `news_ticker`, `now_playing`, `notification_banner`, `compliment`

**Phase 3 additions (OzmaOS compositor):**
- `wlr-layer-shell` overlay surface rendering
- Overlay modes (persistent, edge-reveal, auto-hide, clean, notification-only)
- Overlay mode tied to scenario switching
- `camera_popup` and `notification_banner` animation layer

**Phase 4 additions (embedded image):**
- Mirror mode display profile in the embedded image
- DRM/KMS plane compositing for zero-GPU-overhead overlay
- First-boot mirror zone calibration utility

**Phase 5 additions (presence + personalisation):**
- Presence provider (PIR GPIO + Frigate integration)
- Face recognition pipeline (Frigate sub_label → person event)
- Per-user scene definitions in the user model
- Scene trigger engine (time, presence, face, scenario, calendar)
