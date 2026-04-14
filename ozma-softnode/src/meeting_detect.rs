//! Meeting detection — process scanning + ICS calendar enrichment.
//!
//! # Design
//! - [`MeetingDetector`] owns a Tokio task that polls every `interval`.
//! - Process detection uses the `sysinfo` crate (no subprocess spawning).
//! - Calendar metadata is read from `~/.local/share/gnome-calendar/**/*.ics`
//!   and any extra ICS paths supplied via config.
//! - Results are broadcast on a `tokio::sync::broadcast` channel so the IPC
//!   layer can fan-out to all connected clients.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use icalendar::{Calendar, Component, Event};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
use tracing::{debug, info};

use crate::ipc::{ActiveMeeting, MeetingPlatform, MeetingStatus, ServerMessage};

// ── Platform detection patterns ──────────────────────────────────────────────

struct PlatformPattern {
    platform:      MeetingPlatform,
    /// Substrings matched case-insensitively against the process name.
    process_names: &'static [&'static str],
}

const PATTERNS: &[PlatformPattern] = &[
    PlatformPattern {
        platform:      MeetingPlatform::Zoom,
        process_names: &["zoom", "zoom.us", "zoomlauncher", "zoom_linux_qt"],
    },
    PlatformPattern {
        platform:      MeetingPlatform::Teams,
        process_names: &["teams", "ms-teams", "msteams"],
    },
    PlatformPattern {
        // Google Meet runs inside Chrome; we detect the browser process and
        // rely on calendar enrichment to confirm it is a Meet call.
        platform:      MeetingPlatform::Meet,
        process_names: &["chrome", "chromium", "google-chrome"],
    },
    PlatformPattern {
        platform:      MeetingPlatform::Slack,
        process_names: &["slack"],
    },
    PlatformPattern {
        platform:      MeetingPlatform::Discord,
        process_names: &["discord", "discordcanary", "discordptb"],
    },
    PlatformPattern {
        platform:      MeetingPlatform::FaceTime,
        process_names: &["facetime"],
    },
];

// ── ICS calendar helpers ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct CalendarEvent {
    uid:       String,
    summary:   String,
    organizer: String,
    attendees: Vec<String>,
}

/// Return all `.ics` files directly inside `dir` (non-recursive).
fn collect_ics_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    rd.flatten()
        .filter(|e| e.path().extension().map_or(false, |x| x == "ics"))
        .map(|e| e.path())
        .collect()
}

/// Walk well-known calendar directories plus any caller-supplied paths and
/// return every `.ics` file found.
fn find_ics_files(extra_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        // GNOME Calendar
        let gnome_cal = home.join(".local/share/gnome-calendar");
        if gnome_cal.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&gnome_cal) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        files.extend(collect_ics_files(&p));
                    } else if p.extension().map_or(false, |x| x == "ics") {
                        files.push(p);
                    }
                }
            }
        }

        // Evolution data server
        let evolution = home.join(".local/share/evolution/calendar");
        if evolution.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&evolution) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        files.extend(collect_ics_files(&p));
                    }
                }
            }
        }
    }

    for path in extra_paths {
        if path.is_dir() {
            files.extend(collect_ics_files(path));
        } else if path.is_file() {
            files.push(path.clone());
        }
    }

    files
}

/// Extract string value from a &PropertyValue (icalendar 0.15)
fn extract_property_string(value: &icalendar::PropertyValue) -> Option<String> {
    match value {
        icalendar::PropertyValue::Text(s) => Some(s.to_string()),
        icalendar::PropertyValue::CalAddress(s) => Some(s.to_string()),
        icalendar::PropertyValue::Uri(s) => Some(s.to_string()),
        _ => None,
    }
}

/// Extract datetime from &PropertyValue (icalendar 0.15)
fn extract_datetime(value: &icalendar::PropertyValue) -> Option<DateTime<Utc>> {
    match value {
        icalendar::PropertyValue::DateTime(dt) => Some(*dt),
        icalendar::PropertyValue::Date(dt) => Some((*dt).into()),
        _ => None,
    }
}

/// Parse a single ICS file and return events whose time window contains now.
fn parse_active_events(path: &Path) -> Vec<CalendarEvent> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let calendar: Calendar = match content.parse() {
        Ok(c) => c,
        Err(e) => {
            debug!("ICS parse error {:?}: {}", path, e);
            return vec![];
        }
    };

    let now = Utc::now();
    let mut events = Vec::new();

    // icalendar 0.15: iterate components() which yields &(dyn Component + 'static)
    for component in calendar.components() {
        let ev: &Event = match component.as_event() {
            Some(e) => e,
            None => continue,
        };

        // icalendar 0.15: start_datetime()/end_datetime() return Option<DateTime<Utc>>
        let start_dt = ev.start_datetime();
        let end_dt   = ev.end_datetime();

        let active = match (start_dt, end_dt) {
            (Some(s), Some(e)) => s <= now && now <= e,
            _ => false,
        };
        if !active {
            continue;
        }

        // icalendar 0.15: property() returns Option<&Property>
        // Property has value() returning &PropertyValue
        let uid = ev
            .property("UID")
            .and_then(|p| {
                extract_property_string(p.value())
            })
            .unwrap_or_default();

        // icalendar 0.15: summary() returns Option<&Summary>
        // Summary implements Display
        let summary = ev
            .summary()
            .map(|s| s.to_string())
            .unwrap_or_default();

        // icalendar 0.15: property() for ORGANIZER
        let organizer = ev
            .property("ORGANIZER")
            .and_then(|p| {
                extract_property_string(p.value())
            })
            .map(|s| s.trim_start_matches("mailto:").to_string())
            .unwrap_or_default();

        // icalendar 0.15: properties() returns iterator over &Property
        let attendees: Vec<String> = ev
            .properties("ATTENDEE")
            .filter_map(|p| {
                extract_property_string(p.value())
                    .map(|s| s.trim_start_matches("mailto:").to_string())
            })
            .collect();

        events.push(CalendarEvent { uid, summary, organizer, attendees });
    }

    events
}

// ── Detector ─────────────────────────────────────────────────────────────────

/// Configuration for [`MeetingDetector`].
#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// How often to poll for process changes.
    pub poll_interval: Duration,
    /// Extra ICS file/directory paths to scan for calendar metadata.
    pub extra_ics_paths: Vec<PathBuf>,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            poll_interval:   Duration::from_secs(5),
            extra_ics_paths: Vec::new(),
        }
    }
}

pub struct MeetingDetector {
    config: DetectorConfig,
    tx:     tokio::sync::broadcast::Sender<ServerMessage>,
    /// Currently tracked meetings keyed by stable bucket ID.
    active: HashMap<String, ActiveMeeting>,
    sys:    System,
}

impl MeetingDetector {
    pub fn new(
        config: DetectorConfig,
        tx: tokio::sync::broadcast::Sender<ServerMessage>,
    ) -> Self {
        Self {
            config,
            tx,
            active: HashMap::new(),
            sys: System::new_with_specifics(
                RefreshKind::new().with_processes(ProcessRefreshKind::new()),
            ),
        }
    }

    /// Spawn the polling loop as a Tokio task and return its handle.
    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!(
                interval_secs = self.config.poll_interval.as_secs(),
                "Meeting detector started"
            );
            let mut poll_tick      = tokio::time::interval(self.config.poll_interval);
            let mut heartbeat_tick = tokio::time::interval(Duration::from_secs(30));
            // Consume the immediate first tick to avoid a double-fire.
            poll_tick.tick().await;

            loop {
                tokio::select! {
                    _ = poll_tick.tick() => self.scan_once(),
                    _ = heartbeat_tick.tick() => {
                        let _ = self.tx.send(ServerMessage::Heartbeat { ts: Utc::now() });
                    }
                }
            }
        })
    }

    /// Run one detection cycle.
    pub fn scan_once(&mut self) {
        self.sys.refresh_processes();

        let detected     = self.detect_platforms();
        let detected_ids: HashSet<String> = detected.iter().map(|m| m.id.clone()).collect();
        let active_ids:   HashSet<String> = self.active.keys().cloned().collect();

        // New meetings
        for mut meeting in detected {
            if !active_ids.contains(&meeting.id) {
                self.enrich_from_calendar(&mut meeting);
                info!(
                    platform = %meeting.platform,
                    title    = %meeting.title,
                    id       = %meeting.id,
                    "Meeting detected"
                );
                let _ = self.tx.send(ServerMessage::MeetingStarted {
                    meeting: meeting.clone(),
                });
                self.active.insert(meeting.id.clone(), meeting);
                self.broadcast_snapshot();
            }
        }

        // Ended meetings
        for id in active_ids.difference(&detected_ids).cloned().collect::<Vec<_>>() {
            if let Some(mut meeting) = self.active.remove(&id) {
                meeting.finish();
                info!(
                    platform = %meeting.platform,
                    title    = %meeting.title,
                    id       = %meeting.id,
                    "Meeting ended"
                );
                let _ = self.tx.send(ServerMessage::MeetingEnded {
                    meeting: meeting.clone(),
                });
                self.broadcast_snapshot();
            }
        }
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    fn detect_platforms(&self) -> Vec<ActiveMeeting> {
        let mut meetings = Vec::new();
        for pattern in PATTERNS {
            if self.process_running(pattern.process_names) {
                // 5-minute time bucket gives a stable ID across polls.
                let bucket = Utc::now().timestamp() / 300;
                let id     = format!("{}-{}", pattern.platform, bucket);
                meetings.push(ActiveMeeting::new(id, pattern.platform.clone()));
            }
        }
        meetings
    }

    /// Returns `true` if any running process name contains one of `names`
    /// (case-insensitive substring match).
    fn process_running(&self, names: &[&str]) -> bool {
        // sysinfo 0.30: processes() returns Iterator<Item = (&Pid, &Process)>
        self.sys.processes().iter().any(|(_, proc)| {
            let pname = proc.name().to_string_lossy().to_lowercase();
            names.iter().any(|n| pname.contains(*n))
        })
    }

    /// Attempt to enrich `meeting` with metadata from local ICS files.
    fn enrich_from_calendar(&self, meeting: &mut ActiveMeeting) {
        let ics_files = find_ics_files(&self.config.extra_ics_paths);
        if ics_files.is_empty() {
            return;
        }
        for path in &ics_files {
            for ev in parse_active_events(path) {
                if ev.summary.is_empty() {
                    continue;
                }
                let summary_lower = ev.summary.to_lowercase();
                let platform_str  = meeting.platform.to_string();
                // Match if the event mentions the platform or common call keywords.
                if summary_lower.contains(&platform_str)
                    || summary_lower.contains("call")
                    || summary_lower.contains("meeting")
                    || summary_lower.contains("standup")
                    || summary_lower.contains("sync")
                {
                    meeting.title        = ev.summary;
                    meeting.organizer    = ev.organizer;
                    meeting.attendees    = ev.attendees;
                    meeting.calendar_uid = ev.uid;
                    return;
                }
            }
        }
    }

    fn broadcast_snapshot(&self) {
        let meetings: Vec<ActiveMeeting> = self.active.values().cloned().collect();
        let status = if meetings.iter().any(|m| m.active) {
            MeetingStatus::Active
        } else {
            MeetingStatus::Idle
        };
        let _ = self.tx.send(ServerMessage::Snapshot { status, meetings });
    }
}
