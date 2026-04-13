//! V4L2 capture → ffmpeg-sidecar → HLS output.
//!
//! Direct port of `node/capture.py`.
//!
//! Pipeline
//! --------
//! V4L2 device
//!   └─ ffmpeg (encoder chosen by caller)
//!        ├─ video: H.265 / H.264 → HLS segments + manifest
//!        └─ audio: AAC → muxed into same HLS segments
//!
//! The HLS manifest is written to `{out_dir}/stream.m3u8`.
//! Segment files are `{out_dir}/seg_NNNNN.ts`.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use ffmpeg_sidecar::{
    command::FfmpegCommand,
    event::{FfmpegEvent, LogLevel},
};
use tokio::{sync::Notify, task::JoinHandle};
use tracing::{debug, info, warn};
use v4l::{framesize::FrameSizeEnum, Device};

// ── Encoder configuration ─────────────────────────────────────────────────────

/// Encoder configuration selected by hardware detection.
/// Mirrors `hw_detect.EncoderConfig` from `node/capture.py`.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Human-readable label, e.g. `"h264_v4l2m2m"`.
    pub name: String,
    /// ffmpeg encoder name passed to `-c:v`.
    pub ffmpeg_encoder: String,
    /// Extra flags inserted after `-c:v <encoder>`.
    pub encode_flags: Vec<String>,
    /// Extra flags inserted before the first `-i` (e.g. VAAPI device init).
    pub input_flags: Vec<String>,
    /// VAAPI device path if applicable, e.g. `/dev/dri/renderD128`.
    pub vaapi_device: Option<String>,
}

impl EncoderConfig {
    /// Software H.264 via libx264 — always available.
    pub fn software_h264() -> Self {
        Self {
            name: "libx264".into(),
            ffmpeg_encoder: "libx264".into(),
            encode_flags: vec![
                "-preset".into(), "veryfast".into(),
                "-tune".into(), "zerolatency".into(),
                "-crf".into(), "28".into(),
            ],
            input_flags: vec![],
            vaapi_device: None,
        }
    }

    /// V4L2 M2M H.264 (Raspberry Pi / i.MX8 / etc.).
    pub fn v4l2m2m_h264() -> Self {
        Self {
            name: "h264_v4l2m2m".into(),
            ffmpeg_encoder: "h264_v4l2m2m".into(),
            encode_flags: vec!["-b:v".into(), "4M".into()],
            input_flags: vec![],
            vaapi_device: None,
        }
    }

    /// VAAPI H.264 (Intel / AMD iGPU).
    pub fn vaapi_h264(device: impl Into<String>) -> Self {
        let dev = device.into();
        Self {
            name: "h264_vaapi".into(),
            ffmpeg_encoder: "h264_vaapi".into(),
            encode_flags: vec![
                "-vf".into(), "format=nv12,hwupload".into(),
                "-b:v".into(), "4M".into(),
            ],
            input_flags: vec!["-vaapi_device".into(), dev.clone()],
            vaapi_device: Some(dev),
        }
    }
}

// ── Capture device description ────────────────────────────────────────────────

/// Mirrors `hw_detect.CaptureDevice`.
#[derive(Debug, Clone)]
pub struct CaptureDevice {
    /// V4L2 device path, e.g. `/dev/video0`.
    pub path: String,
    /// Maximum supported width in pixels.
    pub max_width: u32,
    /// Maximum supported height in pixels.
    pub max_height: u32,
    /// Supported pixel format FourCC strings, e.g. `["MJPG", "YUYV"]`.
    pub formats: Vec<String>,
    /// Paired ALSA device string, e.g. `"hw:1,0"`.
    pub audio_device: Option<String>,
}

impl CaptureDevice {
    /// Probe a V4L2 device path and return a populated `CaptureDevice`.
    pub fn probe(path: &str) -> Result<Self> {
        let dev = Device::with_path(path)
            .with_context(|| format!("open V4L2 device {path}"))?;

        let mut formats: Vec<String> = Vec::new();
        let mut max_width = 640u32;
        let mut max_height = 480u32;

        if let Ok(fmt_descs) = dev.enum_formats() {
            for fd in fmt_descs {
                let fourcc = String::from_utf8_lossy(&fd.fourcc.repr)
                    .trim_end_matches('\0')
                    .to_uppercase();
                formats.push(fourcc);

                if let Ok(sizes) = dev.enum_framesizes(fd.fourcc) {
                    for sz in sizes {
                        match sz.size {
                            FrameSizeEnum::Discrete(d) => {
                                if d.width * d.height > max_width * max_height {
                                    max_width = d.width;
                                    max_height = d.height;
                                }
                            }
                            FrameSizeEnum::Stepwise(s) => {
                                if s.max_width * s.max_height > max_width * max_height {
                                    max_width = s.max_width;
                                    max_height = s.max_height;
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(CaptureDevice {
            path: path.to_string(),
            max_width,
            max_height,
            formats,
            audio_device: None,
        })
    }
}

/// Enumerate all `/dev/videoN` devices present on the system.
pub fn enumerate_v4l2_devices() -> Vec<CaptureDevice> {
    let mut devices = Vec::new();
    for n in 0..32u32 {
        let path = format!("/dev/video{n}");
        if !Path::new(&path).exists() {
            continue;
        }
        match CaptureDevice::probe(&path) {
            Ok(dev) => {
                info!(
                    "V4L2 device: {} ({}x{}) formats={:?}",
                    dev.path, dev.max_width, dev.max_height, dev.formats
                );
                devices.push(dev);
            }
            Err(e) => debug!("Skipping {path}: {e}"),
        }
    }
    devices
}

// ── MediaCapture ──────────────────────────────────────────────────────────────

/// Manages the ffmpeg capture-and-encode subprocess.
///
/// Call [`MediaCapture::start`] to launch, [`MediaCapture::stop`] to terminate.
/// The HLS manifest appears at `{out_dir}/stream.m3u8` once ffmpeg has written
/// the first segment (~2 s after start).
pub struct MediaCapture {
    device: CaptureDevice,
    encoder: EncoderConfig,
    out_dir: PathBuf,
    cap_w: u32,
    cap_h: u32,
    cap_fps: u32,
    out_w: u32,
    out_h: u32,
    hls_seg: f32,
    hls_list: u32,
    audio_device: Option<String>,
    uac2_device: Option<String>,
    stop_flag: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl MediaCapture {
    pub fn new(device: CaptureDevice, encoder: EncoderConfig, out_dir: PathBuf) -> Self {
        let cap_w = device.max_width.max(1);
        let cap_h = device.max_height.max(1);
        let out_w = cap_w.min(1920);
        // Keep aspect ratio; ensure even height.
        let out_h = (out_w as u64 * cap_h as u64 / cap_w as u64) as u32 & !1;
        let audio_device = device.audio_device.clone();

        Self {
            device,
            encoder,
            out_dir,
            cap_w,
            cap_h,
            cap_fps: 30,
            out_w,
            out_h,
            hls_seg: 1.0,
            hls_list: 4,
            audio_device,
            uac2_device: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            stop_notify: Arc::new(Notify::new()),
            task: None,
        }
    }

    // ── Builder-style setters ─────────────────────────────────────────────────

    pub fn with_capture_size(mut self, w: u32, h: u32) -> Self {
        self.cap_w = w;
        self.cap_h = h;
        self
    }

    pub fn with_stream_size(mut self, w: u32, h: u32) -> Self {
        self.out_w = w;
        self.out_h = h & !1; // ensure even
        self
    }

    pub fn with_fps(mut self, fps: u32) -> Self {
        self.cap_fps = fps;
        self
    }

    pub fn with_hls(mut self, segment_secs: f32, list_size: u32) -> Self {
        self.hls_seg = segment_secs;
        self.hls_list = list_size;
        self
    }

    pub fn with_audio(mut self, alsa_device: impl Into<String>) -> Self {
        self.audio_device = Some(alsa_device.into());
        self
    }

    /// Add a UAC2 ALSA output alongside the HLS stream.
    /// Must be called before [`start`](Self::start).
    pub fn with_uac2_output(mut self, alsa_device: impl Into<String>) -> Self {
        self.uac2_device = Some(alsa_device.into());
        self
    }

    // ── Public API ────────────────────────────────────────────────────────────

    pub fn manifest_path(&self) -> PathBuf {
        self.out_dir.join("stream.m3u8")
    }

    pub fn is_active(&self) -> bool {
        self.task
            .as_ref()
            .map(|t| !t.is_finished())
            .unwrap_or(false)
    }

    /// Launch the ffmpeg subprocess (with automatic restart on failure).
    pub async fn start(&mut self) -> Result<()> {
        std::fs::create_dir_all(&self.out_dir)
            .with_context(|| format!("create output dir {:?}", self.out_dir))?;

        self.stop_flag.store(false, Ordering::SeqCst);
        self.stop_notify = Arc::new(Notify::new());

        let args = self.build_ffmpeg_args();
        let out_dir = self.out_dir.clone();
        let stop_flag = Arc::clone(&self.stop_flag);
        let stop_notify = Arc::clone(&self.stop_notify);

        info!(
            "MediaCapture starting: {} → {}x{}  encoder={}",
            self.device.path, self.out_w, self.out_h, self.encoder.name,
        );

        self.task = Some(tokio::spawn(run_with_backoff(
            args,
            out_dir,
            stop_flag,
            stop_notify,
        )));
        Ok(())
    }

    /// Terminate the ffmpeg subprocess and wait for the task to finish.
    pub async fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.stop_notify.notify_waiters();
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }

    // ── ffmpeg command builder ────────────────────────────────────────────────

    fn build_ffmpeg_args(&self) -> Vec<String> {
        let enc = &self.encoder;
        let cap = &self.device;

        // Pixel format hint for the v4l2 demuxer.
        let mut pix_fmt_args: Vec<String> = Vec::new();
        if cap.formats.iter().any(|f| f == "MJPG") {
            pix_fmt_args.extend(["-input_format".into(), "mjpeg".into()]);
        } else if cap.formats.iter().any(|f| f == "NV12") {
            pix_fmt_args.extend(["-input_format".into(), "nv12".into()]);
        }

        let mut args: Vec<String> = vec![
            "-y".into(), "-hide_banner".into(),
            "-loglevel".into(), "warning".into(),
        ];

        // Hardware-specific input flags (e.g. -vaapi_device).
        args.extend(enc.input_flags.iter().cloned());

        // Video input.
        args.extend(["-f".into(), "v4l2".into()]);
        args.extend(pix_fmt_args);
        args.extend([
            "-video_size".into(), format!("{}x{}", self.cap_w, self.cap_h),
            "-framerate".into(), self.cap_fps.to_string(),
            "-i".into(), cap.path.clone(),
        ]);

        // Audio input (optional).
        let has_audio = self.audio_device.is_some();
        if let Some(ref adev) = self.audio_device {
            args.extend([
                "-f".into(), "alsa".into(),
                "-channels".into(), "2".into(),
                "-sample_rate".into(), "48000".into(),
                "-i".into(), adev.clone(),
            ]);
        }

        // Stream mapping.
        if has_audio {
            args.extend(["-map".into(), "0:v:0".into(), "-map".into(), "1:a:0".into()]);
        } else {
            args.extend(["-map".into(), "0:v:0".into()]);
        }

        // Video encoder.
        args.extend(["-c:v".into(), enc.ffmpeg_encoder.clone()]);

        // Scale / pixel-format filter.
        let needs_scale = self.out_w != self.cap_w || self.out_h != self.cap_h;
        if enc.vaapi_device.is_some() {
            // Strip any -vf already in encode_flags, then add ours.
            let mut skip_next = false;
            for flag in &enc.encode_flags {
                if skip_next { skip_next = false; continue; }
                if flag == "-vf" { skip_next = true; continue; }
                args.push(flag.clone());
            }
            let vf = if needs_scale {
                format!("format=nv12,hwupload,scale_vaapi={}:{}", self.out_w, self.out_h)
            } else {
                "format=nv12,hwupload".into()
            };
            args.extend(["-vf".into(), vf]);
        } else {
            args.extend(enc.encode_flags.iter().cloned());
            let vf = if needs_scale {
                format!("scale={}:{},format=yuv420p", self.out_w, self.out_h)
            } else {
                "format=yuv420p".into()
            };
            args.extend(["-vf".into(), vf]);
        }

        // Audio encode.
        if has_audio {
            args.extend([
                "-c:a".into(), "aac".into(),
                "-b:a".into(), "128k".into(),
                "-ar".into(), "48000".into(),
                "-ac".into(), "2".into(),
            ]);
        }

        // HLS output.
        let seg_path = self.out_dir.join("seg_%05d.ts").to_string_lossy().into_owned();
        let m3u8_path = self.out_dir.join("stream.m3u8").to_string_lossy().into_owned();
        args.extend([
            "-f".into(), "hls".into(),
            "-hls_time".into(), format!("{}", self.hls_seg),
            "-hls_list_size".into(), self.hls_list.to_string(),
            "-hls_flags".into(), "delete_segments+independent_segments+append_list".into(),
            "-hls_segment_filename".into(), seg_path,
            m3u8_path,
        ]);

        // Optional UAC2 audio output.
        if let (Some(ref uac2), true) = (&self.uac2_device, has_audio) {
            args.extend([
                "-map".into(), "1:a:0".into(),
                "-c:a".into(), "pcm_s16le".into(),
                "-ar".into(), "48000".into(),
                "-ac".into(), "2".into(),
                "-f".into(), "alsa".into(),
                uac2.clone(),
            ]);
        }

        args
    }
}

// ── Background task ───────────────────────────────────────────────────────────

async fn run_with_backoff(
    args: Vec<String>,
    out_dir: PathBuf,
    stop_flag: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
) {
    let mut backoff = Duration::from_secs(2);
    loop {
        if stop_flag.load(Ordering::SeqCst) {
            return;
        }
        match capture_loop(&args, &out_dir, &stop_flag).await {
            Ok(()) => return, // clean stop
            Err(e) => {
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }
                warn!("Capture error: {e} — retry in {:.0}s", backoff.as_secs_f32());
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop_notify.notified() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Run one ffmpeg invocation.  Returns `Ok(())` on clean stop, `Err` on
/// unexpected exit (triggers a retry in the caller).
async fn capture_loop(
    args: &[String],
    _out_dir: &Path,
    stop_flag: &Arc<AtomicBool>,
) -> Result<()> {
    debug!("ffmpeg args: {}", args.join(" "));

    let mut child = FfmpegCommand::new().args(args).spawn().context("spawn ffmpeg")?;
    let iter = child.iter().context("ffmpeg event iterator")?;
    let stop_flag_clone = Arc::clone(stop_flag);

    let drain = tokio::task::spawn_blocking(move || {
        for event in iter {
            match event {
                FfmpegEvent::Log(LogLevel::Warning, msg)
                | FfmpegEvent::Log(LogLevel::Error, msg)
                | FfmpegEvent::Log(LogLevel::Fatal, msg) => {
                    warn!("ffmpeg: {}", msg.trim());
                }
                FfmpegEvent::Log(_, msg) => {
                    debug!("ffmpeg: {}", msg.trim());
                }
                FfmpegEvent::Done => break,
                _ => {}
            }
            if stop_flag_clone.load(Ordering::SeqCst) {
                break;
            }
        }
    });

    drain.await.context("drain task panicked")?;

    let status = child.wait().context("wait for ffmpeg")?;
    if !status.success() && !stop_flag.load(Ordering::SeqCst) {
        anyhow::bail!("ffmpeg exited with {status}");
    }
    Ok(())
}
