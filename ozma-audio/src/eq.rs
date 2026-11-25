// SPDX-License-Identifier: AGPL-3.0-only
//! A reversible PipeWire filter-chain insertion for a measured room profile.
//!
//! # Why a filter-chain *process*
//!
//! `pipewire-pulse` does not implement the PulseAudio `module-filter-chain` or
//! `module-equalizer-sink` names, so `pactl load-module module-filter-chain`
//! fails with `No such entity` even on a PipeWire host. The native
//! `libpipewire-module-filter-chain` can still be loaded safely without
//! restarting the graph: a short-lived `pipewire -c <config>` client owns the
//! module, publishes one explicit EQ sink, and disappears when the process is
//! killed. Killing that client removes its nodes; no daemon restart, default
//! sink, default source, or existing stream is touched.
//!
//! The generated config uses `filter.smart` with an explicit
//! `filter.smart.target`, so the EQ sink is linked to one named target sink by
//! WirePlumber. Audio is **not** moved onto it implicitly: a caller must route
//! playback to the reported [`RoomEqApplication::eq_sink`]. This is the
//! closest safe insertion path on this host; a transparent in-place insertion
//! into an existing sink would require changing stream routing or the default
//! sink, which this implementation deliberately refuses to do.
//!
//! # Bounds
//!
//! - 1 to [`MAX_SWEEP_BANDS`] bands.
//! - Gains within +/-[`MAX_SUGGESTED_GAIN_DB`] dB, Q within 0.1..=10.
//! - Frequencies within the sweep bounds, finite values only.
//! - 1 or 2 channels (the builtin biquad chain is per channel), sample rate
//!   8 kHz to 192 kHz.
//!
//! A request outside these bounds is [`AudioError::InvalidSpec`] and no file is
//! written and no process is spawned.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::AudioError;
use crate::router::validate_token;
use crate::sweep::{
    SweepBand, MAX_SUGGESTED_GAIN_DB, MAX_SWEEP_BANDS, MAX_SWEEP_HZ, MAX_SWEEP_SAMPLE_RATE,
    MIN_SWEEP_HZ, MIN_SWEEP_SAMPLE_RATE,
};

/// Prefix of the explicit EQ sink published by the filter-chain client.
pub const EQ_SINK_PREFIX: &str = "ozma-eq-";
/// Stable name of the filter-chain insertion mechanism.
pub const EQ_INSERTION: &str = "pipewire-filter-chain-smart-sink";
/// PipeWire module loaded by the generated config.
pub const EQ_MODULE_NAME: &str = "libpipewire-module-filter-chain";
/// PipeWire helper binary used to host the filter-chain.
pub const PIPEWIRE: &str = "pipewire";
/// Highest accepted EQ sample rate.
pub const MAX_EQ_RATE: u32 = MAX_SWEEP_SAMPLE_RATE;
/// Lowest accepted EQ sample rate.
pub const MIN_EQ_RATE: u32 = MIN_SWEEP_SAMPLE_RATE;
/// Highest accepted EQ channel count.
pub const MAX_EQ_CHANNELS: u16 = 2;
/// Default EQ sample rate when the request does not choose one.
pub const DEFAULT_EQ_RATE: u32 = 48_000;
/// Default EQ channel count when the request does not choose one.
pub const DEFAULT_EQ_CHANNELS: u16 = 2;
/// How long to wait for the filter-chain client to publish the EQ sink.
pub const EQ_LOAD_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval while waiting for the EQ sink.
pub const EQ_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Largest accepted active-EQ state document stored next to the profile.
pub const MAX_ACTIVE_EQ_STATE_BYTES: u64 = 64 * 1024;

static CONFIG_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static EQ_SINK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A validated request to insert one room-correction filter chain.
#[derive(Clone, Debug, PartialEq)]
pub struct RoomEqSpec {
    /// Profile id this application was derived from.
    pub profile_id: String,
    /// Existing sink the filter chain must target.
    pub target_sink: String,
    /// New EQ sink to publish. Must not already exist.
    pub eq_sink: String,
    /// Parametric bands to load.
    pub bands: Vec<SweepBand>,
    /// Filter sample rate.
    pub rate: u32,
    /// Filter channel count (1 or 2).
    pub channels: u16,
}

impl RoomEqSpec {
    /// Validate names, bands, rate, and channels before anything is written.
    pub fn validate(&self) -> Result<(), AudioError> {
        validate_text("profile id", &self.profile_id, 256)?;
        validate_token("room eq target sink", &self.target_sink)?;
        validate_token("room eq sink", &self.eq_sink)?;
        if self.eq_sink == self.target_sink {
            return Err(invalid(
                "eq_sink",
                "must differ from the target sink; the filter chain publishes a new sink",
            ));
        }
        if !(MIN_EQ_RATE..=MAX_EQ_RATE).contains(&self.rate) {
            return Err(invalid(
                "rate",
                format!("must be between {MIN_EQ_RATE} and {MAX_EQ_RATE} Hz"),
            ));
        }
        if !(1..=MAX_EQ_CHANNELS).contains(&self.channels) {
            return Err(invalid(
                "channels",
                format!("must be between 1 and {MAX_EQ_CHANNELS}"),
            ));
        }
        validate_bands(&self.bands)
    }

    /// Render the PipeWire filter-chain config for this plan.
    ///
    /// This is pure and deterministic. It is public so tests can assert the
    /// exact graph and so the generated file can be inspected after a failure.
    pub fn filter_chain_config(&self) -> Result<String, AudioError> {
        self.validate()?;
        let position = match self.channels {
            1 => "[ MONO ]",
            2 => "[ FL FR ]",
            _ => {
                return Err(invalid(
                    "channels",
                    format!("must be between 1 and {MAX_EQ_CHANNELS}"),
                ))
            }
        };

        let mut nodes = String::new();
        for (index, band) in self.bands.iter().enumerate() {
            let label = match band.band_type.as_str() {
                "peaking" => "bq_peaking",
                "low_shelf" => "bq_lowshelf",
                "high_shelf" => "bq_highshelf",
                other => {
                    return Err(invalid(
                        "band.type",
                        format!("`{other}` is not peaking, low_shelf, or high_shelf"),
                    ))
                }
            };
            nodes.push_str(&format!(
                "                    {{ type = builtin name = band{index} label = {label} control = {{ \"Freq\" = {:.3} \"Q\" = {:.3} \"Gain\" = {:.3} }} }}\n",
                band.freq, band.q, band.gain
            ));
        }

        let mut links = String::new();
        for index in 1..self.bands.len() {
            links.push_str(&format!(
                "                        {{ output = \"band{}:Out\" input = \"band{}:In\" }}\n",
                index - 1,
                index
            ));
        }

        Ok(format!(
            "# Generated by ozma-node for profile `{profile}`. Do not edit.\n\
             context.properties = {{\n\
             \x20   log.level = 1\n\
             }}\n\
             context.spa-libs = {{\n\
             \x20   audio.convert.* = audioconvert/libspa-audioconvert\n\
             \x20   support.*       = support/libspa-support\n\
             }}\n\
             context.modules = [\n\
             \x20   {{ name = libpipewire-module-protocol-native }}\n\
             \x20   {{ name = libpipewire-module-client-node }}\n\
             \x20   {{ name = libpipewire-module-adapter }}\n\
             \x20   {{ name = libpipewire-module-filter-chain\n\
             \x20       args = {{\n\
             \x20           node.description = \"Ozma room correction\"\n\
             \x20           media.name       = \"Ozma room correction\"\n\
             \x20           audio.channels   = {channels}\n\
             \x20           audio.position   = {position}\n\
             \x20           filter.graph = {{\n\
             \x20               nodes = [\n\
             {nodes}\
             \x20               ]\n\
             \x20               links = [\n\
             {links}\
             \x20               ]\n\
             \x20           }}\n\
             \x20           capture.props = {{\n\
             \x20               node.name           = \"{eq_sink}\"\n\
             \x20               node.description    = \"Ozma room EQ for {target_sink}\"\n\
             \x20               media.class         = Audio/Sink\n\
             \x20               filter.smart        = true\n\
             \x20               filter.smart.name   = \"{eq_sink}\"\n\
             \x20               filter.smart.target = {{ node.name = \"{target_sink}\" }}\n\
             \x20           }}\n\
             \x20           playback.props = {{\n\
             \x20               node.passive = true\n\
             \x20               media.role   = \"DSP\"\n\
             \x20           }}\n\
             \x20       }}\n\
             \x20   }}\n\
             ]\n",
            profile = self.profile_id,
            channels = self.channels,
            position = position,
            nodes = nodes,
            links = links,
            eq_sink = self.eq_sink,
            target_sink = self.target_sink,
        ))
    }
}

/// One successfully loaded filter chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoomEqApplication {
    /// Profile id the applied bands came from.
    pub profile_id: String,
    /// Existing sink the filter chain targets.
    pub target_sink: String,
    /// The explicit EQ sink callers can route audio to.
    pub eq_sink: String,
    /// Runtime handle of the filter-chain client, reported as `module_id`.
    pub module_id: u64,
    /// PipeWire module loaded inside that client.
    pub module_name: String,
    /// Number of bands in the profile.
    pub band_count: usize,
    /// Number of bands loaded into the filter graph.
    pub active_bands: usize,
    /// Bands loaded into the graph.
    pub bands: Vec<SweepBand>,
    /// Filter sample rate.
    pub rate: u32,
    /// Filter channel count.
    pub channels: u16,
    /// Unix timestamp when the module was observed loaded.
    pub applied_at: f64,
    /// Audio backend (`pipewire`).
    pub backend: String,
    /// Insertion mechanism identifier.
    pub insertion: String,
}

impl RoomEqApplication {
    /// Build the observable application record for a validated spec.
    pub fn from_spec(spec: &RoomEqSpec, module_id: u64) -> Self {
        Self {
            profile_id: spec.profile_id.clone(),
            target_sink: spec.target_sink.clone(),
            eq_sink: spec.eq_sink.clone(),
            module_id,
            module_name: EQ_MODULE_NAME.to_owned(),
            band_count: spec.bands.len(),
            active_bands: spec.bands.len(),
            bands: spec.bands.clone(),
            rate: spec.rate,
            channels: spec.channels,
            applied_at: now_secs(),
            backend: "pipewire".to_owned(),
            insertion: EQ_INSERTION.to_owned(),
        }
    }

    /// Validate a record loaded from the active-state file.
    pub fn validate(&self) -> Result<(), AudioError> {
        validate_text("profile id", &self.profile_id, 256)?;
        validate_token("room eq target sink", &self.target_sink)?;
        validate_token("room eq sink", &self.eq_sink)?;
        if self.eq_sink == self.target_sink {
            return Err(invalid("eq_sink", "must differ from the target sink"));
        }
        if self.module_id == 0 {
            return Err(invalid("module_id", "must be a non-zero runtime handle"));
        }
        validate_text("module name", &self.module_name, 128)?;
        if self.band_count != self.bands.len() || self.active_bands != self.bands.len() {
            return Err(invalid(
                "active_bands",
                "must equal the number of stored bands",
            ));
        }
        validate_bands(&self.bands)?;
        if !(MIN_EQ_RATE..=MAX_EQ_RATE).contains(&self.rate) {
            return Err(invalid("rate", "must be a supported sample rate"));
        }
        if !(1..=MAX_EQ_CHANNELS).contains(&self.channels) {
            return Err(invalid("channels", "must be 1 or 2"));
        }
        if !self.applied_at.is_finite() || self.applied_at < 0.0 {
            return Err(invalid("applied_at", "must be a finite timestamp"));
        }
        validate_text("backend", &self.backend, 32)?;
        validate_text("insertion", &self.insertion, 64)?;
        Ok(())
    }
}

/// A generated config file that removes its private directory on drop.
pub struct TempEqConfig {
    directory: PathBuf,
    path: PathBuf,
}

impl std::fmt::Debug for TempEqConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TempEqConfig")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl TempEqConfig {
    /// Write `text` to a fresh `0700` directory as a `0600` file.
    pub fn write(text: &str) -> Result<Self, AudioError> {
        let sequence = CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let directory = std::env::temp_dir().join(format!(
            "ozma-eq-{}-{}-{}",
            std::process::id(),
            nanos,
            sequence
        ));
        create_private_directory(&directory)?;
        let path = directory.join("filter-chain.conf");
        write_private_file(&path, text.as_bytes()).map_err(|error| AudioError::Process {
            command: path.display().to_string(),
            message: format!("could not write the filter-chain config: {error}"),
        })?;
        Ok(Self { directory, path })
    }

    /// The config file path passed to `pipewire -c`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The private directory that owns the file.
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

impl Drop for TempEqConfig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Validate a band list for filter-chain insertion.
pub fn validate_bands(bands: &[SweepBand]) -> Result<(), AudioError> {
    if bands.is_empty() {
        return Err(invalid(
            "bands",
            "must contain at least one band; there is nothing to apply",
        ));
    }
    if bands.len() > MAX_SWEEP_BANDS {
        return Err(invalid(
            "bands",
            format!("must contain at most {MAX_SWEEP_BANDS} bands"),
        ));
    }
    for band in bands {
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
    Ok(())
}

/// Build a token-safe, unique EQ sink name from a profile id.
pub fn eq_sink_name(profile_id: &str, sequence: u64) -> String {
    let mut label: String = profile_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    if label.is_empty() || label.starts_with('-') {
        label.insert(0, 'x');
    }
    format!("{EQ_SINK_PREFIX}{label}-{sequence}")
}

/// Build a process-unique EQ sink name for one application.
pub fn next_eq_sink_name(profile_id: &str) -> String {
    let sequence = EQ_SINK_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    format!(
        "{}-{}",
        eq_sink_name(profile_id, sequence),
        std::process::id()
    )
}

/// The typed "no filter-chain backend" error.
pub fn eq_unsupported(feature: &str) -> AudioError {
    AudioError::Unsupported {
        feature: format!("room-correction EQ insertion (`{feature}`)"),
        remediation: "Use a PipeWire router on a host with the `pipewire` binary and the \
                      `libpipewire-module-filter-chain` module. PulseAudio's \
                      module-equalizer-sink is not part of pipewire-pulse, and \
                      `pactl load-module module-filter-chain` is rejected there. No \
                      graph change was attempted."
            .to_owned(),
    }
}

fn invalid(field: impl Into<String>, reason: impl Into<String>) -> AudioError {
    AudioError::InvalidSpec {
        field: format!("room eq {}", field.into()),
        reason: reason.into(),
    }
}

fn validate_text(field: &str, value: &str, limit: usize) -> Result<(), AudioError> {
    if value.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
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

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn create_private_directory(path: &Path) -> Result<(), AudioError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|error| AudioError::Process {
        command: path.display().to_string(),
        message: format!("could not create the filter-chain directory: {error}"),
    })
}

fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(freq: f64, gain: f64, q: f64, band_type: &str) -> SweepBand {
        SweepBand {
            freq,
            gain,
            q,
            band_type: band_type.to_owned(),
        }
    }

    fn base_spec() -> RoomEqSpec {
        RoomEqSpec {
            profile_id: "sweep-test".to_owned(),
            target_sink: "ozma-vm1".to_owned(),
            eq_sink: "ozma-eq-sweep-test-1".to_owned(),
            bands: vec![
                band(31.5, 3.0, 1.0, "low_shelf"),
                band(1_000.0, -3.5, 1.4, "peaking"),
                band(16_000.0, -2.0, 0.7, "high_shelf"),
            ],
            rate: 48_000,
            channels: 2,
        }
    }

    #[test]
    fn config_contains_a_chained_band_graph_and_smart_target() {
        let spec = base_spec();
        spec.validate().unwrap();
        let config = spec.filter_chain_config().unwrap();
        assert!(config.contains("label = bq_lowshelf"));
        assert!(config.contains("label = bq_peaking"));
        assert!(config.contains("label = bq_highshelf"));
        assert!(config.contains("\"Freq\" = 1000.000 \"Q\" = 1.400 \"Gain\" = -3.500"));
        assert!(config.contains("output = \"band0:Out\" input = \"band1:In\""));
        assert!(config.contains("output = \"band1:Out\" input = \"band2:In\""));
        assert!(config
            .lines()
            .any(|line| line.trim_start().starts_with("filter.smart") && line.contains("= true")));
        assert!(config.contains("filter.smart.target = { node.name = \"ozma-vm1\" }"));
        assert!(config.contains("node.name           = \"ozma-eq-sweep-test-1\""));
        assert!(config.contains("audio.channels   = 2"));
        assert!(config.contains("audio.position   = [ FL FR ]"));
    }

    #[test]
    fn invalid_bands_are_rejected_before_any_file_is_written() {
        let mut spec = base_spec();
        spec.bands.clear();
        assert!(matches!(
            spec.validate(),
            Err(AudioError::InvalidSpec { .. })
        ));

        let mut spec = base_spec();
        spec.bands[0].gain = MAX_SUGGESTED_GAIN_DB + 1.0;
        assert!(matches!(
            spec.validate(),
            Err(AudioError::InvalidSpec { .. })
        ));

        let mut spec = base_spec();
        spec.bands[0].q = 0.0;
        assert!(matches!(
            spec.validate(),
            Err(AudioError::InvalidSpec { .. })
        ));

        let mut spec = base_spec();
        spec.bands[0].band_type = "notch".to_owned();
        assert!(matches!(
            spec.validate(),
            Err(AudioError::InvalidSpec { .. })
        ));

        let mut spec = base_spec();
        spec.channels = 3;
        assert!(matches!(
            spec.validate(),
            Err(AudioError::InvalidSpec { .. })
        ));

        let mut spec = base_spec();
        spec.rate = 1_000;
        assert!(matches!(
            spec.validate(),
            Err(AudioError::InvalidSpec { .. })
        ));

        let mut spec = base_spec();
        spec.eq_sink = spec.target_sink.clone();
        assert!(matches!(
            spec.validate(),
            Err(AudioError::InvalidSpec { .. })
        ));
    }

    #[test]
    fn temp_config_is_private_and_removed_on_drop() {
        let config = TempEqConfig::write("context.modules = [ ]\n").unwrap();
        let path = config.path().to_path_buf();
        let directory = config.directory().to_path_buf();
        assert!(path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let mode = fs::metadata(&directory).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        drop(config);
        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn eq_sink_names_are_tokens_and_do_not_start_with_a_dash() {
        assert_eq!(eq_sink_name("sweep-test", 7), "ozma-eq-sweep-test-7");
        let name = eq_sink_name("  spaced/id ", 2);
        validate_token("eq sink", &name).unwrap();
        assert!(!name.starts_with('-'));
    }

    #[test]
    fn application_round_trips_through_json() {
        let spec = base_spec();
        let application = RoomEqApplication::from_spec(&spec, 123);
        application.validate().unwrap();
        let json = serde_json::to_vec(&application).unwrap();
        let loaded: RoomEqApplication = serde_json::from_slice(&json).unwrap();
        assert_eq!(loaded.profile_id, application.profile_id);
        assert_eq!(loaded.target_sink, application.target_sink);
        assert_eq!(loaded.eq_sink, application.eq_sink);
        assert_eq!(loaded.module_id, application.module_id);
        assert_eq!(loaded.bands, application.bands);
        assert_eq!(loaded.active_bands, 3);
        assert_eq!(loaded.module_name, EQ_MODULE_NAME);
        assert!(
            (loaded.applied_at - application.applied_at).abs() < 1e-6,
            "timestamp must survive JSON serialisation"
        );
        loaded.validate().unwrap();
    }
}
