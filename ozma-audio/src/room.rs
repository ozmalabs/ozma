// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in persistence for the last measured room-correction profile.
//!
//! Storage is deliberately small, atomic, and local:
//!
//! - The operator opts in by configuring one directory. Without it every
//!   route reports a typed "unsupported" error and nothing is written.
//! - The file name is fixed ([`ROOM_PROFILE_FILE`]), so a request cannot choose
//!   a path.
//! - A profile is fully validated before it is written or returned. It must be
//!   derived from captured audio (`captured_frames > 0`), carry no more than
//!   [`MAX_SWEEP_BANDS`] bands, and every gain is bounded to
//!   [`MAX_SUGGESTED_GAIN_DB`].
//! - Writes go to a same-directory temporary file with `0600` permissions and
//!   are renamed into place. Reads are bounded by [`MAX_ROOM_PROFILE_BYTES`]
//!   and reject symlinks.
//!
//! No secret is stored: the profile contains measurement metadata, target
//! curve names, and suggested gains only. Nothing here applies EQ or changes
//! the audio graph.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::AudioError;
use crate::sweep::{
    SweepMeasurement, SweepProfile, MAX_SUGGESTED_GAIN_DB, MAX_SWEEP_BANDS,
    MAX_SWEEP_DURATION_SECS, MAX_SWEEP_HZ, MAX_SWEEP_SAMPLE_RATE, MIN_SWEEP_DURATION_SECS,
    MIN_SWEEP_HZ, MIN_SWEEP_SAMPLE_RATE,
};

/// Fixed file name used inside the configured profile directory.
pub const ROOM_PROFILE_FILE: &str = "room-correction-profile.json";
/// Largest profile JSON accepted or written.
pub const MAX_ROOM_PROFILE_BYTES: u64 = 256 * 1024;
/// Largest accepted text field in a stored profile.
pub const MAX_ROOM_PROFILE_TEXT: usize = 256;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A bounded, atomic store for one measured profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomProfileStore {
    root: PathBuf,
}

impl RoomProfileStore {
    /// Use `root` as the profile directory. The directory is created on write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The configured directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The fixed profile path.
    pub fn path(&self) -> PathBuf {
        self.root.join(ROOM_PROFILE_FILE)
    }

    /// Validate and atomically store one profile.
    pub fn store(&self, profile: &SweepProfile) -> Result<(), AudioError> {
        validate_profile(profile)?;
        let json = serde_json::to_vec(profile).map_err(|error| AudioError::Process {
            command: self.path().display().to_string(),
            message: format!("could not serialise the room profile: {error}"),
        })?;
        if json.len() as u64 > MAX_ROOM_PROFILE_BYTES {
            return Err(invalid(
                "profile",
                format!(
                    "serialised profile is {} bytes; the limit is {}",
                    json.len(),
                    MAX_ROOM_PROFILE_BYTES
                ),
            ));
        }
        fs::create_dir_all(&self.root)
            .map_err(|error| self.io_error("create the directory", error))?;
        let path = self.path();
        let temporary = self.temporary_path();
        let write = || -> std::io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&json)?;
            file.flush()?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            Ok(())
        };
        if let Err(error) = write() {
            let _ = fs::remove_file(&temporary);
            return Err(self.io_error("write the profile", error));
        }
        Ok(())
    }

    /// Load and validate the stored profile, or `None` when none exists.
    pub fn load(&self) -> Result<Option<SweepProfile>, AudioError> {
        let path = self.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(self.io_error("inspect the profile", error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(invalid(
                "file",
                format!(
                    "{} is a symlink; a stored profile must be a regular file",
                    path.display()
                ),
            ));
        }
        if !metadata.is_file() {
            return Err(invalid(
                "file",
                format!("{} is not a regular file", path.display()),
            ));
        }
        if metadata.len() > MAX_ROOM_PROFILE_BYTES {
            return Err(invalid(
                "file",
                format!(
                    "{} is {} bytes; the limit is {}",
                    path.display(),
                    metadata.len(),
                    MAX_ROOM_PROFILE_BYTES
                ),
            ));
        }
        let file = File::open(&path).map_err(|error| self.io_error("open the profile", error))?;
        if !file
            .metadata()
            .map_err(|error| self.io_error("inspect the profile", error))?
            .is_file()
        {
            return Err(invalid(
                "file",
                format!("{} is not a regular file", path.display()),
            ));
        }
        let mut bytes = Vec::new();
        let mut limited = file.take(MAX_ROOM_PROFILE_BYTES + 1);
        limited
            .read_to_end(&mut bytes)
            .map_err(|error| self.io_error("read the profile", error))?;
        if bytes.len() as u64 > MAX_ROOM_PROFILE_BYTES {
            return Err(invalid(
                "file",
                format!(
                    "{} grew past the {} byte limit",
                    path.display(),
                    MAX_ROOM_PROFILE_BYTES
                ),
            ));
        }
        let profile: SweepProfile =
            serde_json::from_slice(&bytes).map_err(|error| AudioError::OutputParse {
                command: path.display().to_string(),
                output: String::new(),
                reason: format!("stored room profile is not valid JSON: {error}"),
            })?;
        validate_profile(&profile).map_err(|error| AudioError::OutputParse {
            command: path.display().to_string(),
            output: String::new(),
            reason: format!("stored room profile failed validation: {error}"),
        })?;
        Ok(Some(profile))
    }

    /// Remove the stored profile. `Ok(false)` means nothing was stored.
    pub fn clear(&self) -> Result<bool, AudioError> {
        match fs::remove_file(self.path()) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(self.io_error("remove the profile", error)),
        }
    }

    fn temporary_path(&self) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        self.root.join(format!(
            ".{ROOM_PROFILE_FILE}.{}.{}.{}.tmp",
            std::process::id(),
            nanos,
            sequence
        ))
    }

    fn io_error(&self, action: &str, error: std::io::Error) -> AudioError {
        AudioError::Process {
            command: self.path().display().to_string(),
            message: format!("could not {action}: {error}"),
        }
    }
}

/// Validate a profile before it is stored or returned.
///
/// This is intentionally stricter than plain deserialisation: a profile that
/// claims bands without captured audio, an applied profile, or an out-of-range
/// gain is rejected rather than silently normalised.
pub fn validate_profile(profile: &SweepProfile) -> Result<(), AudioError> {
    validate_text("id", &profile.id, MAX_ROOM_PROFILE_TEXT)?;
    validate_text("name", &profile.name, MAX_ROOM_PROFILE_TEXT)?;
    validate_text("mic_type", &profile.mic_type, 128)?;
    validate_text("phone_model", &profile.phone_model, 128)?;
    validate_text("mic_model", &profile.mic_model, 128)?;
    validate_text("target_curve", &profile.target_curve, 128)?;
    validate_text("room_name", &profile.room_name, 128)?;
    validate_text("node_id", &profile.node_id, 128)?;
    if !profile.created_at.is_finite() || profile.created_at < 0.0 {
        return Err(invalid(
            "created_at",
            "must be a finite, non-negative timestamp",
        ));
    }
    if profile.active {
        return Err(invalid(
            "active",
            "a stored measurement cannot claim that correction was applied",
        ));
    }
    if profile.bands.len() != profile.band_count {
        return Err(invalid(
            "band_count",
            format!(
                "is {} but the profile carries {} bands",
                profile.band_count,
                profile.bands.len()
            ),
        ));
    }
    if profile.bands.len() > MAX_SWEEP_BANDS {
        return Err(invalid(
            "bands",
            format!("carries more than {MAX_SWEEP_BANDS} bands"),
        ));
    }
    for band in &profile.bands {
        if !band.freq.is_finite() || !(MIN_SWEEP_HZ..=MAX_SWEEP_HZ).contains(&band.freq) {
            return Err(invalid(
                "band.freq",
                format!(
                    "{} is outside {MIN_SWEEP_HZ}..={MAX_SWEEP_HZ} Hz",
                    band.freq
                ),
            ));
        }
        if !band.gain.is_finite() || band.gain.abs() > MAX_SUGGESTED_GAIN_DB {
            return Err(invalid(
                "band.gain",
                format!("{} is outside +/-{MAX_SUGGESTED_GAIN_DB} dB", band.gain),
            ));
        }
        if !band.q.is_finite() || !(0.1..=10.0).contains(&band.q) {
            return Err(invalid("band.q", format!("{} is outside 0.1..=10", band.q)));
        }
        if !matches!(
            band.band_type.as_str(),
            "peaking" | "low_shelf" | "high_shelf"
        ) {
            return Err(invalid(
                "band.type",
                format!(
                    "`{}` is not peaking, low_shelf, or high_shelf",
                    band.band_type
                ),
            ));
        }
    }
    validate_measurement(&profile.measurement)?;
    if let Some(analysis) = &profile.analysis {
        validate_analysis(analysis)?;
    }
    Ok(())
}

fn validate_measurement(measurement: &SweepMeasurement) -> Result<(), AudioError> {
    if !(MIN_SWEEP_SAMPLE_RATE..=MAX_SWEEP_SAMPLE_RATE).contains(&measurement.sample_rate) {
        return Err(invalid(
            "measurement.sample_rate",
            format!(
                "{} is outside {MIN_SWEEP_SAMPLE_RATE}..={MAX_SWEEP_SAMPLE_RATE} Hz",
                measurement.sample_rate
            ),
        ));
    }
    if !(1..=2).contains(&measurement.channels) {
        return Err(invalid("measurement.channels", "must be 1 or 2"));
    }
    if measurement.captured_frames == 0 || measurement.captured_secs <= 0.0 {
        return Err(invalid(
            "measurement.captured_frames",
            "a stored profile must be derived from captured audio",
        ));
    }
    for (field, value) in [
        ("captured_secs", measurement.captured_secs),
        ("peak", f64::from(measurement.peak)),
        ("peak_dbfs", f64::from(measurement.peak_dbfs)),
        ("rms_dbfs", f64::from(measurement.rms_dbfs)),
        ("noise_floor_dbfs", f64::from(measurement.noise_floor_dbfs)),
        ("impulse_peak", f64::from(measurement.impulse_peak)),
        ("impulse_index_secs", measurement.impulse_index_secs),
    ] {
        if !value.is_finite() {
            return Err(invalid(format!("measurement.{field}"), "must be finite"));
        }
    }
    if measurement.peak <= 0.0 || measurement.impulse_peak <= 0.0 {
        return Err(invalid(
            "measurement.peak",
            "must be positive; a silent capture is not a measurement",
        ));
    }
    if let Some(rt60) = measurement.rt60_secs {
        if !rt60.is_finite() || rt60 < 0.0 {
            return Err(invalid(
                "measurement.rt60_secs",
                "must be finite and non-negative",
            ));
        }
    }
    if let Some(decay) = measurement.decay_db_per_sec {
        if !decay.is_finite() {
            return Err(invalid("measurement.decay_db_per_sec", "must be finite"));
        }
    }
    let sweep = &measurement.sweep;
    if !sweep.start_hz.is_finite()
        || !sweep.end_hz.is_finite()
        || !sweep.duration_secs.is_finite()
        || !(MIN_SWEEP_HZ..=MAX_SWEEP_HZ).contains(&sweep.start_hz)
        || !(MIN_SWEEP_HZ..=MAX_SWEEP_HZ).contains(&sweep.end_hz)
        || sweep.end_hz <= sweep.start_hz
        || !(MIN_SWEEP_DURATION_SECS..=MAX_SWEEP_DURATION_SECS).contains(&sweep.duration_secs)
    {
        return Err(invalid(
            "measurement.sweep",
            "must be a finite, increasing sweep within the documented bounds",
        ));
    }
    Ok(())
}

fn validate_analysis(analysis: &crate::sweep::BandAnalysis) -> Result<(), AudioError> {
    validate_text("analysis.method", &analysis.method, 128)?;
    validate_text("analysis.reference", &analysis.reference, 128)?;
    validate_text("analysis.normalisation", &analysis.normalisation, 128)?;
    validate_text(
        "analysis.target_curve_requested",
        &analysis.target_curve_requested,
        128,
    )?;
    validate_text(
        "analysis.target_curve_used",
        &analysis.target_curve_used,
        128,
    )?;
    if analysis.bands.len() > MAX_SWEEP_BANDS || analysis.bands_measured > MAX_SWEEP_BANDS {
        return Err(invalid(
            "analysis.bands",
            format!("carries more than {MAX_SWEEP_BANDS} bands"),
        ));
    }
    if analysis.bands_skipped > MAX_SWEEP_BANDS {
        return Err(invalid(
            "analysis.bands_skipped",
            format!("must be at most {MAX_SWEEP_BANDS}"),
        ));
    }
    if analysis.bands_measured != analysis.bands.len() {
        return Err(invalid(
            "analysis.bands_measured",
            "must equal the number of response bands",
        ));
    }
    if !analysis.mean_confidence.is_finite() || !(0.0..=1.0).contains(&analysis.mean_confidence) {
        return Err(invalid(
            "analysis.mean_confidence",
            "must be finite and between 0 and 1",
        ));
    }
    if !analysis.gain_limit_db.is_finite() || !(0.0..=24.0).contains(&analysis.gain_limit_db) {
        return Err(invalid(
            "analysis.gain_limit_db",
            "must be finite and between 0 and 24 dB",
        ));
    }
    if let Some(normalisation) = analysis.normalisation_db {
        if !normalisation.is_finite() || normalisation.abs() > 120.0 {
            return Err(invalid(
                "analysis.normalisation_db",
                "must be finite and within +/-120 dB",
            ));
        }
    }
    for band in &analysis.bands {
        if !band.freq.is_finite() || !(MIN_SWEEP_HZ..=MAX_SWEEP_HZ).contains(&band.freq) {
            return Err(invalid(
                "analysis.band.freq",
                "must be a finite sweep frequency",
            ));
        }
        for (field, value) in [
            ("measured_db", band.measured_db),
            ("target_db", band.target_db),
            ("deviation_db", band.deviation_db),
            ("suggested_gain_db", band.suggested_gain_db),
            ("confidence", band.confidence),
        ] {
            if !value.is_finite() {
                return Err(invalid(format!("analysis.band.{field}"), "must be finite"));
            }
        }
        if band.suggested_gain_db.abs() > MAX_SUGGESTED_GAIN_DB as f32 {
            return Err(invalid(
                "analysis.band.suggested_gain_db",
                format!("must be within +/-{MAX_SUGGESTED_GAIN_DB} dB"),
            ));
        }
        if !(0.0..=1.0).contains(&band.confidence) {
            return Err(invalid(
                "analysis.band.confidence",
                "must be between 0 and 1",
            ));
        }
        if let Some(snr) = band.snr_db {
            if !snr.is_finite() {
                return Err(invalid("analysis.band.snr_db", "must be finite"));
            }
        }
    }
    Ok(())
}

fn validate_text(field: &str, value: &str, limit: usize) -> Result<(), AudioError> {
    if value.chars().count() > limit {
        return Err(invalid(
            field,
            format!("must be at most {limit} characters"),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(field, "must not contain control characters"));
    }
    Ok(())
}

fn invalid(field: impl Into<String>, reason: impl Into<String>) -> AudioError {
    AudioError::InvalidSpec {
        field: format!("room profile {}", field.into()),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::{BandAnalysis, RoomBand, SweepBand, SweepDescriptor, MAX_SUGGESTED_GAIN_DB};

    fn sample_profile() -> SweepProfile {
        SweepProfile {
            id: "sweep-1790000000000".to_owned(),
            name: "Office".to_owned(),
            created_at: 1_790_000_000.0,
            bands: vec![SweepBand {
                freq: 1_000.0,
                gain: -3.0,
                q: 1.4,
                band_type: "peaking".to_owned(),
            }],
            band_count: 1,
            mic_type: "phone".to_owned(),
            phone_model: "generic".to_owned(),
            mic_model: String::new(),
            target_curve: "harman".to_owned(),
            room_name: "Office".to_owned(),
            node_id: "node-a".to_owned(),
            active: false,
            measurement: SweepMeasurement {
                sample_rate: 48_000,
                channels: 1,
                captured_frames: 74_400,
                captured_secs: 1.55,
                peak: 0.5,
                peak_dbfs: -6.0,
                rms_dbfs: -12.0,
                noise_floor_dbfs: -80.0,
                impulse_peak: 0.5,
                impulse_index: 18_000,
                impulse_index_secs: 0.375,
                rt60_secs: Some(0.4),
                decay_db_per_sec: Some(-150.0),
                sweep: SweepDescriptor {
                    start_hz: 100.0,
                    end_hz: 8_000.0,
                    duration_secs: 1.0,
                },
            },
            analysis: Some(BandAnalysis {
                method: "goertzel-octave".to_owned(),
                reference: "generated log sweep".to_owned(),
                normalisation: "median of measured bands".to_owned(),
                normalisation_db: Some(1.0),
                target_curve_requested: "harman".to_owned(),
                target_curve_used: "harman".to_owned(),
                gain_limit_db: MAX_SUGGESTED_GAIN_DB as f32,
                bands_measured: 1,
                bands_skipped: 9,
                mean_confidence: 0.9,
                bands: vec![RoomBand {
                    freq: 1_000.0,
                    measured_db: 3.0,
                    target_db: 0.0,
                    deviation_db: 3.0,
                    suggested_gain_db: -3.0,
                    confidence: 0.9,
                    snr_db: Some(40.0),
                    suggested: true,
                }],
            }),
        }
    }

    fn temporary_root(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ozma-room-profile-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn store_load_and_clear_round_trip() {
        let root = temporary_root("round-trip");
        let store = RoomProfileStore::new(&root);
        assert_eq!(store.load().unwrap(), None);
        assert!(!store.clear().unwrap());

        let profile = sample_profile();
        store.store(&profile).unwrap();
        assert_eq!(store.load().unwrap(), Some(profile));
        assert!(store.clear().unwrap());
        assert_eq!(store.load().unwrap(), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_and_oversized_files_are_typed_errors() {
        let root = temporary_root("corrupt");
        fs::create_dir_all(&root).unwrap();
        let store = RoomProfileStore::new(&root);

        fs::write(store.path(), b"not json").unwrap();
        assert!(matches!(store.load(), Err(AudioError::OutputParse { .. })));

        let mut invalid = sample_profile();
        invalid.active = true;
        fs::write(store.path(), serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(matches!(store.load(), Err(AudioError::OutputParse { .. })));

        fs::write(
            store.path(),
            vec![b'{'; MAX_ROOM_PROFILE_BYTES as usize + 1],
        )
        .unwrap();
        assert!(matches!(store.load(), Err(AudioError::InvalidSpec { .. })));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn older_profiles_without_analysis_still_validate() {
        let mut value = serde_json::to_value(sample_profile()).unwrap();
        value.as_object_mut().unwrap().remove("analysis");
        let profile: SweepProfile = serde_json::from_value(value).unwrap();
        assert!(profile.analysis.is_none());
        validate_profile(&profile).unwrap();
    }

    #[test]
    fn profiles_without_captured_audio_or_with_unsafe_gains_are_rejected() {
        let mut profile = sample_profile();
        profile.measurement.captured_frames = 0;
        assert!(matches!(
            validate_profile(&profile),
            Err(AudioError::InvalidSpec { .. })
        ));

        let mut profile = sample_profile();
        profile.bands[0].gain = MAX_SUGGESTED_GAIN_DB + 1.0;
        assert!(matches!(
            validate_profile(&profile),
            Err(AudioError::InvalidSpec { .. })
        ));

        let mut profile = sample_profile();
        profile.active = true;
        assert!(matches!(
            validate_profile(&profile),
            Err(AudioError::InvalidSpec { .. })
        ));

        let mut profile = sample_profile();
        profile.band_count = 2;
        assert!(matches!(
            validate_profile(&profile),
            Err(AudioError::InvalidSpec { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn stored_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temporary_root("mode");
        let store = RoomProfileStore::new(&root);
        store.store(&sample_profile()).unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(&root);
    }
}
