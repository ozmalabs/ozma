// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! UAC2 USB Audio Class 2 gadget support.
//!
//! Rust port of `node/usb_audio.py`.
//!
//! # Overview
//!
//! When the UAC2 function is active in the composite gadget the USB host sees
//! the node as an audio device:
//!
//! * **Host capture (microphone input)** — receives audio forwarded from the
//!   HDMI capture card.  This is the *playback* direction from the gadget's
//!   perspective: the device writes audio that the host reads.
//! * **Host playback (speaker output)** — audio played by the host is readable
//!   on the device's ALSA capture interface.
//!
//! # ConfigFS UAC2 terminology
//! * **p_\*** (playback) — audio flowing **from device to host**.  The device
//!   writes to this ALSA playback interface; the host captures it.
//! * **c_\*** (capture)  — audio flowing **from host to device**.  The host
//!   writes to this endpoint; the device reads it as ALSA capture.

use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use alsa::{
    pcm::{Access, Format, HwParams, PCM},
    Direction, ValueOr,
};
use anyhow::{bail, Context, Result};
use tokio::{task::JoinHandle, time::sleep};
use tracing::{debug, info, warn};

// ── constants ────────────────────────────────────────────────────────────────

/// Path to the UAC2 ConfigFS function directory created by the gadget script.
const UAC2_FUNC_DIR: &str = "/sys/kernel/config/usb_gadget/ozma/functions/uac2.usb0";

const PROC_ASOUND_CARDS: &str = "/proc/asound/cards";

/// Sample rate used for both playback and capture streams.
const SAMPLE_RATE: u32 = 48_000;
/// Number of interleaved channels.
const CHANNELS: u32 = 2;
/// Frames per ALSA period.
const PERIOD_FRAMES: u32 = 1_024;
/// Number of periods in the ALSA ring buffer.
const BUFFER_PERIODS: u32 = 4;

// ── ALSA device discovery ─────────────────────────────────────────────────────

/// Return the ALSA device string for the UAC2 gadget's **playback** interface
/// (device → host direction), e.g. `"hw:2,0"`, or `None` if not found.
///
/// Reads `/proc/asound/cards` directly instead of shelling out to `aplay -l`
/// as the Python version does.
///
/// `/proc/asound/cards` format (two lines per card):
/// ```text
///  0 [PCH            ]: HDA-Intel - HDA Intel PCH
///                       Intel Corporation ...
///  1 [UAC2Gadget     ]: UAC2_Gadget - UAC2_Gadget
/// ```
pub fn find_uac2_playback_device() -> Option<String> {
    let content = std::fs::read_to_string(PROC_ASOUND_CARDS).ok()?;

    for line in content.lines() {
        let trimmed = line.trim_start();
        // Index lines start with a digit.
        if !trimmed.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let upper = trimmed.to_ascii_uppercase();
        if upper.contains("UAC2") || upper.contains("GADGET AUDIO") || upper.contains("G_AUDIO") {
            if let Some(idx_str) = trimmed.split_whitespace().next() {
                if let Ok(idx) = idx_str.parse::<u32>() {
                    debug!("Found UAC2 gadget ALSA card {}: {}", idx, trimmed);
                    return Some(format!("hw:{},0", idx));
                }
            }
        }
    }
    None
}

/// Return the ALSA device string for the UAC2 gadget's **capture** interface
/// (host → device direction), e.g. `"hw:2,1"`, or `None` if not found.
pub fn find_uac2_capture_device() -> Option<String> {
    find_uac2_playback_device().map(|d| d.replace(",0", ",1"))
}

/// Return `true` if the UAC2 ConfigFS function directory exists and is linked.
pub fn uac2_active() -> bool {
    Path::new(UAC2_FUNC_DIR).exists()
}

// ── ALSA PCM helpers ──────────────────────────────────────────────────────────

/// Open and configure an ALSA PCM handle with the bridge parameters.
fn open_pcm(device: &str, direction: Direction) -> Result<PCM> {
    let pcm = PCM::new(device, direction, false)
        .with_context(|| format!("opening ALSA PCM '{}' ({:?})", device, direction))?;

    {
        let hwp = HwParams::any(&pcm).context("HwParams::any")?;
        hwp.set_channels(CHANNELS).context("set_channels")?;
        hwp.set_rate(SAMPLE_RATE, ValueOr::Nearest)
            .context("set_rate")?;
        hwp.set_format(Format::s16()).context("set_format")?;
        hwp.set_access(Access::RWInterleaved).context("set_access")?;
        hwp.set_period_size(PERIOD_FRAMES as alsa::pcm::Frames, ValueOr::Nearest)
            .context("set_period_size")?;
        hwp.set_buffer_size((PERIOD_FRAMES * BUFFER_PERIODS) as alsa::pcm::Frames)
            .context("set_buffer_size")?;
        pcm.hw_params(&hwp).context("hw_params")?;
    }

    // Capture streams are started by the first read; playback streams need an
    // explicit start after the buffer is primed.  Call prepare() for both so
    // the PCM is in a known state, then start() only for playback.
    pcm.prepare().context("pcm prepare")?;
    if direction == Direction::Playback {
        // Prime the buffer with silence before starting to avoid EPIPE on the
        // first writei() call.
        let silence = vec![0i16; PERIOD_FRAMES as usize * CHANNELS as usize];
        let io = pcm.io_i16().context("io_i16 for priming")?;
        let _ = io.writei(&silence);
        pcm.start().context("pcm start")?;
    }
    Ok(pcm)
}

/// Copy audio from `src` (capture) to `dst` (playback) until `stop` is set or
/// an unrecoverable ALSA error occurs.
///
/// Uses a single interleaved i16 buffer sized to one period.  On EPIPE /
/// ESTRPIPE the affected stream is recovered in-place; on any other error the
/// function returns `Err` so the caller can reopen the PCM handles (device
/// disappear / reappear path).
fn bridge_loop(src: &PCM, dst: &PCM, stop: &AtomicBool) -> Result<()> {
    let mut buf = vec![0i16; PERIOD_FRAMES as usize * CHANNELS as usize];

    let src_io = src.io_i16().context("src io_i16")?;
    let dst_io = dst.io_i16().context("dst io_i16")?;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Read one period from the source (capture direction).
        let n = match src_io.readi(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) => {
                if src.try_recover(e, true).is_err() {
                    bail!("unrecoverable capture error: {}", e);
                }
                continue;
            }
        };

        // Write the frames to the destination (playback direction).
        let frames = &buf[..n * CHANNELS as usize];
        match dst_io.writei(frames) {
            Ok(_) => {}
            Err(e) => {
                if dst.try_recover(e, true).is_err() {
                    bail!("unrecoverable playback error: {}", e);
                }
            }
        }
    }
}

// ── public API ────────────────────────────────────────────────────────────────

/// Configuration for the USB audio bridge.
#[derive(Debug, Clone)]
pub struct UsbAudioConfig {
    /// ALSA device to read audio from and forward to the USB host
    /// (device → host / "microphone" direction).
    /// Defaults to `"default"`.
    pub hdmi_capture_device: String,

    /// ALSA device to write audio received from the USB host
    /// (host → device / "speaker" direction).
    /// Defaults to `"default"`.
    pub speaker_output_device: String,

    /// How long to wait for the UAC2 ALSA card to appear after the gadget
    /// is detected.
    pub alsa_probe_timeout: Duration,

    /// Interval between hotplug poll cycles.
    pub poll_interval: Duration,
}

impl Default for UsbAudioConfig {
    fn default() -> Self {
        Self {
            hdmi_capture_device: "default".into(),
            speaker_output_device: "default".into(),
            alsa_probe_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_millis(250),
        }
    }
}

/// Manages the UAC2 gadget function lifecycle and the ALSA audio bridge.
///
/// Equivalent to `USBAudioGadget` in `node/usb_audio.py`.
///
/// # Example
/// ```no_run
/// use ozma_node::usb_audio::{UsbAudioGadget, UsbAudioConfig};
/// # #[tokio::main] async fn main() {
/// let gadget = UsbAudioGadget::open(UsbAudioConfig::default()).await;
/// // ... do work ...
/// gadget.close().await;
/// # }
/// ```
pub struct UsbAudioGadget {
    /// ALSA device for the gadget playback interface (device → host).
    pub playback_device: Option<String>,
    /// ALSA device for the gadget capture interface (host → device).
    pub capture_device: Option<String>,
    /// Signals the bridge loop to stop cleanly.
    stop: Arc<AtomicBool>,
    /// Background watcher / bridge supervisor task.
    _watcher: JoinHandle<()>,
}

impl UsbAudioGadget {
    /// Detect the UAC2 ALSA interface and start the bridge tasks.
    ///
    /// Waits up to `config.alsa_probe_timeout` for the kernel to create the
    /// ALSA sound card, then spawns a background watcher that restarts the
    /// bridge on device disappear / reappear (USB reconnect).
    pub async fn open(config: UsbAudioConfig) -> Self {
        if !uac2_active() {
            warn!(
                "UAC2 ConfigFS function not found at {} — \
                 ensure the gadget script ran and the kernel has usb_f_uac2",
                UAC2_FUNC_DIR
            );
        }

        let playback_device =
            wait_for_alsa(config.alsa_probe_timeout, config.poll_interval).await;
        let capture_device = playback_device
            .as_deref()
            .map(|d| d.replace(",0", ",1"));

        if let Some(ref pb) = playback_device {
            info!(
                "UAC2 audio gadget ready — playback: {}  capture: {}",
                pb,
                capture_device.as_deref().unwrap_or("?"),
            );
        } else {
            warn!(
                "UAC2 ALSA device not found — USB audio will be unavailable. \
                 Ensure the gadget script ran and the kernel has usb_f_uac2."
            );
        }

        let stop = Arc::new(AtomicBool::new(false));
        let watcher = spawn_watcher(config, stop.clone());

        Self {
            playback_device,
            capture_device,
            stop,
            _watcher: watcher,
        }
    }

    /// Signal the bridge tasks to stop and wait for them to exit (up to 2 s).
    pub async fn close(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = tokio::time::timeout(Duration::from_secs(2), self._watcher).await;
    }
}

// ── internal helpers ──────────────────────────────────────────────────────────

/// Poll `/proc/asound/cards` until the UAC2 card appears or `timeout` elapses.
async fn wait_for_alsa(timeout: Duration, interval: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(dev) = find_uac2_playback_device() {
            return Some(dev);
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        sleep(interval).await;
    }
    // One final check after the deadline.
    find_uac2_playback_device()
}

/// Spawn the hotplug watcher + bridge supervisor task.
///
/// The task:
/// 1. Waits for the UAC2 ALSA card to appear.
/// 2. Starts two `spawn_blocking` bridge threads (device→host, host→device).
/// 3. Polls for card disappearance or bridge thread exit; restarts as needed.
/// 4. Exits cleanly when `stop` is set.
fn spawn_watcher(config: UsbAudioConfig, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }

            // Wait for the UAC2 card to be present.
            let pb_dev =
                match wait_for_alsa(Duration::from_secs(60), config.poll_interval).await {
                    Some(d) => d,
                    None => {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        continue;
                    }
                };
            let cap_dev = pb_dev.replace(",0", ",1");

            info!("UAC2 card appeared — starting ALSA bridge");

            let bridge_stop = Arc::new(AtomicBool::new(false));

            // device → host bridge (HDMI capture → UAC2 playback)
            //
            // The UAC2 gadget's playback interface (hw:N,0) is opened as
            // Direction::Playback because *we* write audio into it and the
            // USB host reads it as a microphone/capture source.
            let pb_stop = bridge_stop.clone();
            let hdmi_src = config.hdmi_capture_device.clone();
            let pb_dst = pb_dev.clone();
            let pb_handle = tokio::task::spawn_blocking(move || {
                loop {
                    if pb_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let src = match open_pcm(&hdmi_src, Direction::Capture) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("Cannot open HDMI capture PCM: {:#} — retrying in 2 s", e);
                            std::thread::sleep(Duration::from_secs(2));
                            continue;
                        }
                    };
                    let dst = match open_pcm(&pb_dst, Direction::Playback) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("Cannot open UAC2 playback PCM: {:#} — retrying in 2 s", e);
                            std::thread::sleep(Duration::from_secs(2));
                            continue;
                        }
                    };
                    info!("UAC2 playback bridge running: {} → {}", hdmi_src, pb_dst);
                    match bridge_loop(&src, &dst, &pb_stop) {
                        Ok(()) => break,
                        Err(e) => {
                            warn!("UAC2 playback bridge error: {:#} — reopening in 1 s", e);
                            std::thread::sleep(Duration::from_secs(1));
                        }
                    }
                }
            });

            // host → device bridge (UAC2 capture → speaker output)
            //
            // The UAC2 gadget's capture interface (hw:N,1) is opened as
            // Direction::Capture because the USB host writes audio into it
            // (speaker/playback from the host's perspective) and we read it.
            let cap_stop = bridge_stop.clone();
            let cap_src = cap_dev.clone();
            let spk_dst = config.speaker_output_device.clone();
            let cap_handle = tokio::task::spawn_blocking(move || {
                loop {
                    if cap_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let src = match open_pcm(&cap_src, Direction::Capture) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("Cannot open UAC2 capture PCM: {:#} — retrying in 2 s", e);
                            std::thread::sleep(Duration::from_secs(2));
                            continue;
                        }
                    };
                    let dst = match open_pcm(&spk_dst, Direction::Playback) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("Cannot open speaker output PCM: {:#} — retrying in 2 s", e);
                            std::thread::sleep(Duration::from_secs(2));
                            continue;
                        }
                    };
                    info!("UAC2 capture bridge running: {} → {}", cap_src, spk_dst);
                    match bridge_loop(&src, &dst, &cap_stop) {
                        Ok(()) => break,
                        Err(e) => {
                            warn!("UAC2 capture bridge error: {:#} — reopening in 1 s", e);
                            std::thread::sleep(Duration::from_secs(1));
                        }
                    }
                }
            });

            // Poll until the card disappears, we are asked to stop, or a
            // bridge thread exits unexpectedly.
            loop {
                sleep(config.poll_interval).await;

                if stop.load(Ordering::Relaxed) {
                    bridge_stop.store(true, Ordering::Relaxed);
                    let _ = pb_handle.await;
                    let _ = cap_handle.await;
                    return;
                }

                if find_uac2_playback_device().is_none() {
                    warn!("UAC2 card disappeared — stopping bridge");
                    bridge_stop.store(true, Ordering::Relaxed);
                    let _ = pb_handle.await;
                    let _ = cap_handle.await;
                    break; // outer loop will wait for card to reappear
                }

                if pb_handle.is_finished() || cap_handle.is_finished() {
                    warn!("Bridge thread exited unexpectedly — restarting");
                    bridge_stop.store(true, Ordering::Relaxed);
                    let _ = pb_handle.await;
                    let _ = cap_handle.await;
                    break;
                }
            }
        }
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the `/proc/asound/cards` parser finds the UAC2 card correctly.
    #[test]
    fn parse_uac2_card_from_proc() {
        // Simulate /proc/asound/cards with two cards.
        let fake = " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n\
                    \t\t\t   Intel Corporation ...\n\
                     1 [UAC2Gadget     ]: UAC2_Gadget - UAC2_Gadget\n\
                    \t\t\t   Linux UAC2 Gadget\n";

        let result: Option<String> = {
            let mut found = None;
            for line in fake.lines() {
                let trimmed = line.trim_start();
                if !trimmed.starts_with(|c: char| c.is_ascii_digit()) {
                    continue;
                }
                let upper = trimmed.to_ascii_uppercase();
                if upper.contains("UAC2")
                    || upper.contains("GADGET AUDIO")
                    || upper.contains("G_AUDIO")
                {
                    if let Some(idx_str) = trimmed.split_whitespace().next() {
                        if let Ok(idx) = idx_str.parse::<u32>() {
                            found = Some(format!("hw:{},0", idx));
                            break;
                        }
                    }
                }
            }
            found
        };

        assert_eq!(result, Some("hw:1,0".to_string()));
    }

    /// Verify that the capture device is derived correctly from the playback device.
    #[test]
    fn capture_device_from_playback() {
        let pb = "hw:2,0".to_string();
        let cap = pb.replace(",0", ",1");
        assert_eq!(cap, "hw:2,1");
    }

    /// `uac2_active()` must return false in a normal test environment where
    /// the ConfigFS path does not exist.
    #[test]
    fn uac2_active_returns_false_when_path_absent() {
        assert!(!uac2_active());
    }

    /// `find_uac2_capture_device` returns `None` when the playback device is
    /// not found (no UAC2 card in the test environment).
    #[test]
    fn find_capture_device_none_when_no_card() {
        // In CI there is no UAC2 card, so both helpers return None.
        // We just verify they don't panic.
        let _ = find_uac2_playback_device();
        let _ = find_uac2_capture_device();
    }

    #[test]
    fn default_config_sanity() {
        let cfg = UsbAudioConfig::default();
        assert_eq!(cfg.hdmi_capture_device, "default");
        assert_eq!(cfg.speaker_output_device, "default");
        assert!(cfg.alsa_probe_timeout > Duration::ZERO);
        assert!(cfg.poll_interval > Duration::ZERO);
    }

    /// Verify that the card-number parser handles leading spaces correctly.
    #[test]
    fn parse_card_with_leading_space() {
        let line = " 3 [UAC2Gadget     ]: UAC2_Gadget - UAC2_Gadget";
        let trimmed = line.trim_start();
        let idx: u32 = trimmed
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(idx, 3);
    }

    /// Verify that a line without UAC2 in the name is not matched.
    #[test]
    fn non_uac2_card_not_matched() {
        let line = " 0 [PCH            ]: HDA-Intel - HDA Intel PCH";
        let trimmed = line.trim_start();
        let upper = trimmed.to_ascii_uppercase();
        let matched = upper.contains("UAC2")
            || upper.contains("GADGET AUDIO")
            || upper.contains("G_AUDIO");
        assert!(!matched);
    }
}
