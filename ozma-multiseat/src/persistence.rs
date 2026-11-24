// SPDX-License-Identifier: AGPL-3.0-only
//! Versioned, atomic seat persistence.
//!
//! `ozma-multiseat` distinguishes a *plan* (what the host can start right now)
//! from a *seat store* (the stable identity an operator expects across replans).
//! The store records the explicit seat id, display output, input hub, ports,
//! audio sink, and profile for each seat. A replan reuses those values when the
//! display is still present; a persisted seat whose display disappeared stays
//! in the file but is **not** started and carries a
//! [`DiagnosticCode::PersistedSeatMissingDisplay`] diagnostic.
//!
//! The on-disk shape is:
//!
//! ```json
//! {
//!   "version": 1,
//!   "seats": [
//!     {
//!       "id": "host-seat-0",
//!       "display": {"output": "HDMI-1", "index": 0, "width": 1920, "height": 1080, "x": 0, "y": 0, "x_screen": ":0"},
//!       "input_hub": "1-1",
//!       "input_devices": ["/dev/input/event0", "/dev/input/event1"],
//!       "audio_sink": "ozma-seat-0",
//!       "udp_port": 7331,
//!       "http_port": 7382,
//!       "profile": "desktop"
//!     }
//!   ]
//! }
//! ```
//!
//! Unversioned files are treated as version 0 and migrated in memory to the
//! current version. A file with a newer version is rejected with a typed error
//! rather than partially interpreted. Writes go to a same-directory temporary
//! file, are flushed, and are renamed over the destination so an interrupted
//! save cannot leave a truncated `seats.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::display::DisplayOutput;
use crate::error::MultiseatError;
use crate::seat::{MultiseatPlan, SeatPlan, SeatProfile};

/// Highest seat-store format version this build understands.
pub const SEAT_STORE_VERSION: u32 = 1;

/// Conventional file name for the seat store.
pub const DEFAULT_SEAT_STORE_NAME: &str = "seats.json";

/// Display identity persisted with a seat.
///
/// `output` is the stable key used to match a persisted seat to a discovered
/// display. Geometry and X screen are recorded for diagnostics and are updated
/// when the display is rediscovered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedDisplay {
    /// Display output/connector name, for example `HDMI-1`.
    pub output: String,
    /// Backend display index at the time it was persisted.
    #[serde(default)]
    pub index: u32,
    /// Width in pixels at persistence time.
    #[serde(default)]
    pub width: u32,
    /// Height in pixels at persistence time.
    #[serde(default)]
    pub height: u32,
    /// X offset at persistence time.
    #[serde(default)]
    pub x: i32,
    /// Y offset at persistence time.
    #[serde(default)]
    pub y: i32,
    /// X display string, for example `:0`.
    #[serde(default)]
    pub x_screen: String,
}

impl PersistedDisplay {
    /// Capture a discovered display.
    pub fn from_output(display: &DisplayOutput) -> Self {
        Self {
            output: display.name.clone(),
            index: display.index,
            width: display.width,
            height: display.height,
            x: display.x,
            y: display.y,
            x_screen: display.x_screen.clone(),
        }
    }

    /// Whether this persisted identity names the discovered display.
    pub fn matches(&self, display: &DisplayOutput) -> bool {
        self.output == display.name
    }
}

/// One explicitly persisted seat identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSeat {
    /// Stable seat id and controller-visible node name.
    pub id: String,
    /// Display identity that owns the seat.
    pub display: PersistedDisplay,
    /// Primary input hub key, for example `1-1` or `internal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hub: Option<String>,
    /// Input device paths assigned at persistence time.
    #[serde(default)]
    pub input_devices: Vec<String>,
    /// Audio sink name, for example `ozma-seat-0`.
    pub audio_sink: String,
    /// Planned UDP HID port.
    pub udp_port: u16,
    /// Planned HTTP API port.
    pub http_port: u16,
    /// Seat profile.
    #[serde(default)]
    pub profile: SeatProfile,
}

impl PersistedSeat {
    /// Capture a resolved seat plan.
    pub fn from_plan(seat: &SeatPlan) -> Self {
        Self {
            id: seat.config.id.clone(),
            display: PersistedDisplay::from_output(&seat.display),
            input_hub: seat.config.input_hub.clone(),
            input_devices: seat.config.input_devices.clone(),
            audio_sink: seat.config.audio_sink.clone(),
            udp_port: seat.config.udp_port,
            http_port: seat.config.http_port,
            profile: seat.config.profile,
        }
    }
}

/// A versioned collection of persisted seats.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatStore {
    /// Store format version. Missing versions deserialize as `0` and migrate.
    #[serde(default)]
    pub version: u32,
    /// Persisted seats in deterministic order.
    #[serde(default)]
    pub seats: Vec<PersistedSeat>,
}

impl Default for SeatStore {
    fn default() -> Self {
        Self {
            version: SEAT_STORE_VERSION,
            seats: Vec::new(),
        }
    }
}

impl SeatStore {
    /// An empty current-version store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load a store. A missing file is an empty store; unreadable/corrupt files
    /// are typed errors rather than silently discarded state.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, MultiseatError> {
        let path = path.as_ref();
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(error) => {
                return Err(MultiseatError::SeatStoreRead {
                    path: path.display().to_string(),
                    message: error.to_string(),
                })
            }
        };
        Self::from_json(path, &text)
    }

    /// Parse a store from JSON, applying version migration and validation.
    pub fn from_json(path: impl AsRef<Path>, text: &str) -> Result<Self, MultiseatError> {
        let path = path.as_ref();
        let mut store: Self =
            serde_json::from_str(text).map_err(|error| MultiseatError::SeatStoreParse {
                path: path.display().to_string(),
                message: error.to_string(),
            })?;

        if store.version > SEAT_STORE_VERSION {
            return Err(MultiseatError::SeatStoreVersion {
                path: path.display().to_string(),
                found: store.version,
                supported: SEAT_STORE_VERSION,
            });
        }
        if store.version < SEAT_STORE_VERSION {
            // Version 0 was the unversioned shape; all fields are identical and
            // absent optional fields already have stable defaults.
            store.version = SEAT_STORE_VERSION;
        }

        store.validate()?;
        Ok(store)
    }

    /// Encode the store as pretty JSON with a trailing newline.
    ///
    /// A store still carrying version 0 is written at the current version, so
    /// migration is persisted on the next save.
    pub fn to_json(&self) -> Result<String, MultiseatError> {
        self.validate()?;
        let mut normalized = self.clone();
        if normalized.version < SEAT_STORE_VERSION {
            normalized.version = SEAT_STORE_VERSION;
        }
        let mut encoded = serde_json::to_string_pretty(&normalized)
            .map_err(|error| MultiseatError::SeatStoreInvalid(error.to_string()))?;
        encoded.push('\n');
        Ok(encoded)
    }

    /// Validate identity and allocation invariants before a save or merge.
    pub fn validate(&self) -> Result<(), MultiseatError> {
        if self.version > SEAT_STORE_VERSION {
            return Err(MultiseatError::SeatStoreVersion {
                path: "<memory>".to_owned(),
                found: self.version,
                supported: SEAT_STORE_VERSION,
            });
        }

        let mut ids: BTreeMap<&str, &str> = BTreeMap::new();
        let mut displays: BTreeMap<&str, &str> = BTreeMap::new();
        let mut sinks: BTreeMap<&str, &str> = BTreeMap::new();
        let mut udp_ports: BTreeMap<u16, &str> = BTreeMap::new();
        let mut http_ports: BTreeMap<u16, &str> = BTreeMap::new();

        for seat in &self.seats {
            if seat.id.trim().is_empty() {
                return Err(MultiseatError::SeatStoreInvalid(
                    "seat id must not be empty".to_owned(),
                ));
            }
            if let Some(previous) = ids.insert(&seat.id, &seat.display.output) {
                return Err(MultiseatError::SeatStoreConflict(format!(
                    "seat id {:?} is used by both display {:?} and {:?}",
                    seat.id, previous, seat.display.output
                )));
            }

            if seat.display.output.trim().is_empty() {
                return Err(MultiseatError::SeatStoreInvalid(format!(
                    "seat {:?} has an empty display output",
                    seat.id
                )));
            }
            if let Some(previous) = displays.insert(&seat.display.output, &seat.id) {
                return Err(MultiseatError::SeatStoreConflict(format!(
                    "display {:?} is assigned to both seat {:?} and {:?}",
                    seat.display.output, previous, seat.id
                )));
            }

            if seat.audio_sink.trim().is_empty() {
                return Err(MultiseatError::SeatStoreInvalid(format!(
                    "seat {:?} has an empty audio sink",
                    seat.id
                )));
            }
            if let Some(previous) = sinks.insert(&seat.audio_sink, &seat.id) {
                return Err(MultiseatError::SeatStoreConflict(format!(
                    "audio sink {:?} is used by both seat {:?} and {:?}",
                    seat.audio_sink, previous, seat.id
                )));
            }

            // UDP and TCP share the numeric space, so a HID UDP port may equal
            // another seat's HTTP port. Uniqueness is per protocol.
            for (kind, port, ports) in [
                ("UDP", seat.udp_port, &mut udp_ports),
                ("HTTP", seat.http_port, &mut http_ports),
            ] {
                if port == 0 {
                    return Err(MultiseatError::SeatStoreInvalid(format!(
                        "seat {:?} has a zero {kind} port",
                        seat.id
                    )));
                }
                if let Some(previous) = ports.insert(port, &seat.id) {
                    return Err(MultiseatError::SeatStoreConflict(format!(
                        "{kind} port {port} is used by both seat {:?} and {:?}",
                        previous, seat.id
                    )));
                }
            }

            for device in &seat.input_devices {
                if device.trim().is_empty() {
                    return Err(MultiseatError::SeatStoreInvalid(format!(
                        "seat {:?} has an empty input device path",
                        seat.id
                    )));
                }
                // Device paths are not required to be globally unique in the
                // store: a retained entry for a missing display may still name
                // a hub/device that a current seat uses. The planner's
                // persisted-hub assignment is the authority that prevents two
                // active seats from claiming the same group.
            }
        }

        Ok(())
    }

    /// Persist atomically: write a same-directory temporary file, fsync it,
    /// rename over `path`, then fsync the directory where supported.
    pub fn save_atomic(&self, path: impl AsRef<Path>) -> Result<(), MultiseatError> {
        let path = path.as_ref();
        self.validate()?;
        let encoded = self.to_json()?;

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent).map_err(|error| MultiseatError::SeatStoreRead {
                path: parent.display().to_string(),
                message: error.to_string(),
            })?;
        }
        let directory = parent.unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(DEFAULT_SEAT_STORE_NAME);

        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let temp_path = directory.join(format!(
            ".{file_name}.{}.{}.{nanos}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));

        let write_result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(encoded.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(MultiseatError::SeatStoreRead {
                path: temp_path.display().to_string(),
                message: error.to_string(),
            });
        }

        if let Err(error) = fs::rename(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            return Err(MultiseatError::SeatStoreRead {
                path: path.display().to_string(),
                message: error.to_string(),
            });
        }

        // Best-effort directory fsync so the rename itself is durable. Some
        // filesystems reject opening a directory for this; the atomic rename is
        // still complete.
        if let Ok(directory) = File::open(directory) {
            let _ = directory.sync_all();
        }

        Ok(())
    }

    /// Build a fresh store from a plan.
    pub fn from_plan(plan: &MultiseatPlan) -> Self {
        let mut seats: Vec<PersistedSeat> =
            plan.seats.iter().map(PersistedSeat::from_plan).collect();
        sort_seats(&mut seats);
        Self {
            version: SEAT_STORE_VERSION,
            seats,
        }
    }

    /// Update the store from a plan while retaining entries for displays that
    /// are currently absent.
    ///
    /// Retaining the entry preserves stable ids/ports for a returning display,
    /// but it never causes a seat to be planned or started while the display is
    /// missing: the planner only consumes entries it can match to discovered
    /// topology and emits a `PersistedSeatMissingDisplay` diagnostic for the
    /// rest.
    pub fn merge_plan(&self, plan: &MultiseatPlan) -> Self {
        let planned_displays: BTreeSet<&str> = plan
            .seats
            .iter()
            .map(|seat| seat.config.display_output.as_str())
            .collect();
        let mut merged: Vec<PersistedSeat> =
            plan.seats.iter().map(PersistedSeat::from_plan).collect();
        for seat in &self.seats {
            if !planned_displays.contains(seat.display.output.as_str()) {
                merged.push(seat.clone());
            }
        }

        // Defensive de-duplication by seat id; planner allocation reserves all
        // persisted ids, so this only protects a manually edited store.
        let mut seen = BTreeSet::new();
        merged.retain(|seat| seen.insert(seat.id.clone()));
        sort_seats(&mut merged);

        Self {
            version: SEAT_STORE_VERSION,
            seats: merged,
        }
    }

    /// Find the persisted seat for a discovered display.
    pub fn persisted_for_display(&self, display: &DisplayOutput) -> Option<&PersistedSeat> {
        self.seats.iter().find(|seat| seat.display.matches(display))
    }

    /// All persisted seat ids.
    pub fn seat_ids(&self) -> BTreeSet<&str> {
        self.seats.iter().map(|seat| seat.id.as_str()).collect()
    }

    /// Persisted display outputs, useful for diagnostics and tests.
    pub fn display_outputs(&self) -> BTreeSet<&str> {
        self.seats
            .iter()
            .map(|seat| seat.display.output.as_str())
            .collect()
    }
}

fn sort_seats(seats: &mut [PersistedSeat]) {
    seats.sort_by(|left, right| {
        left.display
            .index
            .cmp(&right.display.index)
            .then_with(|| left.display.output.cmp(&right.display.output))
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Conventional path for a seat store under a configuration directory.
pub fn default_seat_store_path(config_dir: impl AsRef<Path>) -> PathBuf {
    config_dir.as_ref().join(DEFAULT_SEAT_STORE_NAME)
}
