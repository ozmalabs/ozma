// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! UAC2 USB Audio Class 2 gadget support.
//!
//! Mirrors the logic in `node/usb_audio.py`:
//!
//! * Detect (and optionally trigger setup of) the UAC2 ConfigFS function via
//!   the `usb-gadget` crate.
//! * Discover the ALSA card that the kernel creates for the gadget.
//! * Bridge audio between the UAC2 gadget capture interface (host → device)
//!   and the gadget playback interface (device → host), handling device
//!   appear / disappear gracefully.
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
use tracing::{debug, info, warn};
use usb_gadget::{
    function::uac2::Uac2Builder, Class, Config, Gadget, Id, RegGadget, Strings,
};

// ── constants ────────────────────────────────────────────────────────────────

/// Path to the UAC2 ConfigFS function directory created by the gadget script.
const UAC2_FUNC_DIR: &str = "/sys/kernel/config/usb_gadget/ozma/functions/uac2.usb0";

/// ALSA card name fragments used to identify the UAC2 gadget sound card.
const UAC2_NAME_PATTERNS: &[&str] = &["UAC2", "uac2", "Gadget Audio", "g_audio"];

/// Sample rate used for both playback and capture streams.
const SAMPLE_RATE: u32 = 48_000;
/// Number of interleaved channels.
const CHANNELS: u32 = 2;
/// Frames per ALSA period.
const PERIOD_FRAMES: u32 = 1_024;
/// Total ALSA ring-buffer size in frames.
const BUFFER_FRAMES: u32 = PERIOD_FRAMES * 4;

// ── ALSA device discovery ─────────────────────────────────────────────────────

/// Return the ALSA device string for the UAC2 gadget's **playback** interface
/// (device → host direction), e.g. `"hw:2,0"`, or `None` if not found.
///
/// Reads `/proc/asound/cards` directly instead of shelling out to `aplay -l`
/// as the Python version does.
pub fn find_uac2_playback_device() -> Option<String> {
    let content = std::fs::read_to_string("/proc/asound/cards").ok()?;

    // /proc/asound/cards format (one card per two lines):
    //  0 [PCH            ]: HDA-Intel - HDA Intel PCH
    //                       Intel Corporation ...
    //  1 [UAC2Gadget     ]: UAC2_Gadget - UAC2_Gadget
    for line in content.lines() {
        let trimmed = line.trim_start();
        // Index lines start with a digit.
        if !trimmed.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        let matches = UAC2_NAME_PATTERNS
            .iter()
            .any(|pat| lower.contains(&pat.to_ascii_lowercase()));
        if matches {
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

// ── gadget registration ───────────────────────────────────────────────────────

/// Attempt to register the UAC2 function via the `usb-gadget` crate.
///
/// This replaces the Python version's call to `setup_gadget.sh`.
/// If the gadget is already active (ConfigFS dir exists) this is a no-op and
/// returns `Ok(None)`.
async fn register_uac2_gadget() -> Result<Option<RegGadget>> {
    if uac2_active() {
        debug!("UAC2 ConfigFS function already present — skipping registration");
        return Ok(None);
    }

    let uac2 = Uac2Builder::new()
        .p_chmask(0x03)    // stereo playback (device → host)
        .p_srate(SAMPLE_RATE)
        .p_ssize(2)         // 16-bit samples
        .c_chmask(0x03)    // stereo capture  (host → device)
        .c_srate(SAMPLE_RATE)
        .c_ssize(2)
        .build()
        .context("building UAC2 function descriptor")?;

    let gadget = Gadget::new(
        Class::new(0xef, 0x02, 0x01), // Miscellaneous / IAD
        Id::new(0x1d6b, 0x0104),       // Linux Foundation composite gadget
        Strings::new("Ozma", "UAC2 Audio", "000000000001"),
    )
    .with_config(Config::new("config").with_function(uac2));

    let udc = usb_gadget::default_udc()
        .context("no UDC found — is the gadget driver loaded?")?;
    let reg = gadget
        .bind(&udc)
        .context("binding UAC2 gadget to UDC")?;

    info!("UAC2 gadget registered via usb-gadget crate (UDC: {})", udc.name());
    Ok(Some(reg))
}

// ── ALSA bridge ───────────────────────────────────────────────────────────────

/// Open an ALSA PCM handle configured for the UAC2 bridge parameters.
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
        hwp.set_buffer_size(BUFFER_FRAMES as alsa::pcm::Frames)
            .context("set_buffer_size")?;
        pcm.hw_params(&hwp).context("hw_params")?;
    }

    pcm.start().context("pcm start")?;
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
    let period = PERIOD_FRAMES as usize;
    let mut buf: Vec<i16> = vec![0i16; period * CHANNELS as usize];

    let src_io = src.io_i16().context("src io_i16")?;
    let dst_io = dst.io_i16().context("dst io_i16")?;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Read one period from the UAC2 capture interface (host → device).
        match src_io.readi(&mut buf) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(e) => {
                if src.try_recover(e, true).is_err() {
                    bail!("unrecoverable capture error: {}", e);
                }
                continue;
            }
        }

        // Write one period to the UAC2 playback interface (device → host).
        match dst_io.writei(&buf) {
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

/// Manages the UAC2 gadget function lifecycle and the ALSA audio bridge.
///
/// Equivalent to `USBAudioGadget` in `node/usb_audio.py`.
pub struct UsbAudioGadget {
    /// ALSA device for the gadget playback interface (device → host).
    pub playback_device: Option<String>,
    /// ALSA device for the gadget capture interface (host → device).
    pub capture_device: Option<String>,
    /// Registered gadget handle — kept alive to maintain the ConfigFS binding.
    _reg: Option<RegGadget>,
    /// Signals the bridge loop to stop cleanly.
    stop: Arc<AtomicBool>,
}

impl UsbAudioGadget {
    /// Detect the UAC2 ALSA interface.  If it is not present and `auto_setup`
    /// is `true`, attempt to register the gadget via the `usb-gadget` crate.
    ///
    /// Waits up to 5 seconds for the kernel to create the ALSA sound card
    /// after gadget registration, mirroring `USBAudioGadget.open()` in Python.
    pub async fn open(auto_setup: bool) -> Result<Self> {
        let mut reg: Option<RegGadget> = None;

        if auto_setup {
            match register_uac2_gadget().await {
                Ok(r) => reg = r,
                Err(e) => warn!("UAC2 gadget registration failed: {:#}", e),
            }
        }

        // Wait up to 5 s for the kernel to create the ALSA sound card.
        let playback_device = wait_for_alsa(Duration::from_secs(5)).await;
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

        Ok(Self {
            playback_device,
            capture_device,
            _reg: reg,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Run the ALSA bridge loop, forwarding audio between the UAC2 gadget
    /// capture interface (host → device) and the playback interface
    /// (device → host).
    ///
    /// Handles device disappear / reappear: if the PCM device goes away (e.g.
    /// USB cable unplugged) the loop waits 1 s and reopens the streams.
    /// Returns only when [`stop`](Self::stop) is called or a spawn error occurs.
    pub async fn run_bridge(&self) -> Result<()> {
        let (Some(ref pb_dev), Some(ref cap_dev)) =
            (&self.playback_device, &self.capture_device)
        else {
            warn!("UAC2 PCM devices unavailable — bridge not started");
            return Ok(());
        };

        let pb_dev = pb_dev.clone();
        let cap_dev = cap_dev.clone();
        let stop = Arc::clone(&self.stop);

        // Run the blocking bridge on a dedicated OS thread so it never blocks
        // the async runtime.
        tokio::task::spawn_blocking(move || {
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                let src = match open_pcm(&cap_dev, Direction::Capture) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Cannot open UAC2 capture PCM: {:#} — retrying in 2 s", e);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };

                let dst = match open_pcm(&pb_dev, Direction::Playback) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Cannot open UAC2 playback PCM: {:#} — retrying in 2 s", e);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };

                info!("UAC2 audio bridge running ({} → {})", cap_dev, pb_dev);

                match bridge_loop(&src, &dst, &stop) {
                    Ok(()) => {
                        info!("UAC2 bridge stopped cleanly");
                        break;
                    }
                    Err(e) => {
                        // Device disappeared or unrecoverable error — reopen.
                        warn!("UAC2 bridge error: {:#} — reopening PCM in 1 s", e);
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        })
        .await
        .context("UAC2 bridge thread panicked")?;

        Ok(())
    }

    /// Signal the bridge loop to stop after the current period completes.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Poll for the UAC2 ALSA card to appear, returning as soon as it is found or
/// after `timeout` has elapsed.
async fn wait_for_alsa(timeout: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(dev) = find_uac2_playback_device() {
            return Some(dev);
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // One final check after the deadline.
    find_uac2_playback_device()
}
