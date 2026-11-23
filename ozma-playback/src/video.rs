// SPDX-License-Identifier: AGPL-3.0-only
//! Video sinks: null recorder, PPM writer, and an optional window.
//!
//! [`DecodedVideoFrame`] carries tightly packed I420 planes and can already
//! convert itself to RGB or a complete PPM file. A sink's job is therefore only
//! to decide where the picture goes. The default sinks have no GUI dependency:
//!
//! | Sink | Purpose |
//! |---|---|
//! | [`NullVideoSink`] | headless tests and CI; records dimensions, sequences, bytes |
//! | [`PpmVideoSink`] | writes the first frame or numbered frames for inspection |
//! | `WindowVideoSink` | `minifb` window, compiled only with `--features window` |
//!
//! Window support is deliberately feature-gated so a node, a server, or a
//! headless client can depend on this crate without pulling a display server
//! into the build.

use std::path::{Path, PathBuf};

use ozma_decode::DecodedVideoFrame;

use crate::error::PlaybackError;

/// A presentation destination for decoded video frames.
///
/// Implementations are `Send` because the playback engine moves them to a
/// dedicated worker thread. Methods are synchronous.
pub trait VideoSink: Send {
    /// Open the destination.
    ///
    /// Most sinks (a window, a PPM directory) are open when constructed, so the
    /// default is a no-op. A test sink may use this to refuse frames before it
    /// is explicitly started.
    fn open(&mut self) -> Result<(), PlaybackError> {
        Ok(())
    }

    /// Present one frame.
    ///
    /// A sink may drop frames it cannot display, but it must not block the
    /// caller indefinitely; the playback engine already bounds its queue.
    fn present(&mut self, frame: &DecodedVideoFrame) -> Result<(), PlaybackError>;

    /// Flush any internal buffering.
    fn flush(&mut self) -> Result<(), PlaybackError>;

    /// Stop accepting frames and release the destination. Repeated calls
    /// return `Ok(())`.
    fn stop(&mut self) -> Result<(), PlaybackError>;

    /// True while the sink is open and able to accept frames.
    fn is_open(&self) -> bool;
}

/// One frame recorded by [`NullVideoSink`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoRecord {
    /// Presentation timestamp copied from the frame.
    pub pts_micros: u64,
    /// Sequence number copied from the frame.
    pub sequence: u64,
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Bytes in the I420 representation.
    pub yuv_bytes: usize,
    /// Whether the source access unit was a keyframe.
    pub keyframe: bool,
}

/// A video sink that records frame metadata instead of rendering.
#[derive(Debug, Default)]
pub struct NullVideoSink {
    opened: bool,
    records: Vec<VideoRecord>,
    bytes_presented: u64,
    flushes: u64,
    stop_calls: u64,
}

impl NullVideoSink {
    /// Create a closed null sink. Call [`VideoSink::open`] before presenting.
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames recorded so far, oldest first.
    pub fn records(&self) -> &[VideoRecord] {
        &self.records
    }

    /// Number of frames presented.
    pub fn frames_presented(&self) -> u64 {
        u64::try_from(self.records.len()).unwrap_or(u64::MAX)
    }

    /// Total I420 bytes presented.
    pub fn bytes_presented(&self) -> u64 {
        self.bytes_presented
    }

    /// Dimensions of the most recently presented frame, if any.
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.records
            .last()
            .map(|record| (record.width, record.height))
    }

    /// Number of [`VideoSink::flush`] calls.
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// Number of [`VideoSink::stop`] calls, including repeated ones.
    pub fn stop_calls(&self) -> u64 {
        self.stop_calls
    }

    /// The most recently presented frame, if any.
    pub fn last(&self) -> Option<&VideoRecord> {
        self.records.last()
    }
}

impl VideoSink for NullVideoSink {
    fn open(&mut self) -> Result<(), PlaybackError> {
        self.opened = true;
        Ok(())
    }

    fn present(&mut self, frame: &DecodedVideoFrame) -> Result<(), PlaybackError> {
        if !self.opened {
            return Err(PlaybackError::not_open("null-video"));
        }
        self.records.push(VideoRecord {
            pts_micros: frame.pts_micros,
            sequence: frame.sequence,
            width: frame.width,
            height: frame.height,
            yuv_bytes: frame.yuv_len(),
            keyframe: frame.keyframe,
        });
        self.bytes_presented = self
            .bytes_presented
            .saturating_add(u64::try_from(frame.yuv_len()).unwrap_or(u64::MAX));
        Ok(())
    }

    fn flush(&mut self) -> Result<(), PlaybackError> {
        self.flushes = self.flushes.saturating_add(1);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlaybackError> {
        self.opened = false;
        self.stop_calls = self.stop_calls.saturating_add(1);
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.opened
    }
}

/// How [`PpmVideoSink`] names frames on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PpmMode {
    /// Write only the first frame, to `frame.ppm`.
    FirstFrame,
    /// Write every frame as `frame-000001.ppm`, `frame-000002.ppm`, ...
    Numbered,
}

/// A video sink that writes decoded frames as binary PPM files.
///
/// The directory is created by [`PpmVideoSink::new`]. PPM needs no image
/// dependency, which makes this sink usable for CI screenshots, diagnostics,
/// and a first "did a frame arrive?" check on a headless box.
#[derive(Debug)]
pub struct PpmVideoSink {
    directory: PathBuf,
    mode: PpmMode,
    written: u64,
    skipped: u64,
    flushes: u64,
    stop_calls: u64,
    opened: bool,
}

impl PpmVideoSink {
    /// Create a sink that writes only the first frame to `frame.ppm`.
    pub fn new(directory: impl AsRef<Path>) -> Result<Self, PlaybackError> {
        Self::with_mode(directory, PpmMode::FirstFrame)
    }

    /// Create a sink with an explicit naming mode.
    pub fn with_mode(directory: impl AsRef<Path>, mode: PpmMode) -> Result<Self, PlaybackError> {
        let directory = directory.as_ref().to_path_buf();
        std::fs::create_dir_all(&directory).map_err(|error| {
            PlaybackError::io(directory.display().to_string(), error.to_string())
        })?;
        Ok(Self {
            directory,
            mode,
            written: 0,
            skipped: 0,
            flushes: 0,
            stop_calls: 0,
            opened: true,
        })
    }

    /// Directory this sink writes into.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Naming mode.
    pub fn mode(&self) -> PpmMode {
        self.mode
    }

    /// Number of files written.
    pub fn frames_written(&self) -> u64 {
        self.written
    }

    /// Number of frames deliberately skipped after the first (in
    /// [`PpmMode::FirstFrame`]).
    pub fn frames_skipped(&self) -> u64 {
        self.skipped
    }

    /// Path of the next file this sink would write.
    pub fn next_path(&self) -> PathBuf {
        match self.mode {
            PpmMode::FirstFrame => self.directory.join("frame.ppm"),
            PpmMode::Numbered => self
                .directory
                .join(format!("frame-{:06}.ppm", self.written.saturating_add(1))),
        }
    }

    /// Number of [`VideoSink::flush`] calls.
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// Number of [`VideoSink::stop`] calls, including repeated ones.
    pub fn stop_calls(&self) -> u64 {
        self.stop_calls
    }
}

impl VideoSink for PpmVideoSink {
    fn open(&mut self) -> Result<(), PlaybackError> {
        self.opened = true;
        Ok(())
    }

    fn present(&mut self, frame: &DecodedVideoFrame) -> Result<(), PlaybackError> {
        if !self.opened {
            return Err(PlaybackError::not_open("ppm-video"));
        }
        if self.mode == PpmMode::FirstFrame && self.written > 0 {
            self.skipped = self.skipped.saturating_add(1);
            return Ok(());
        }
        let path = self.next_path();
        frame
            .write_ppm(&path)
            .map_err(|error| PlaybackError::io(path.display().to_string(), error.to_string()))?;
        self.written = self.written.saturating_add(1);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), PlaybackError> {
        self.flushes = self.flushes.saturating_add(1);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlaybackError> {
        self.opened = false;
        self.stop_calls = self.stop_calls.saturating_add(1);
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.opened
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_decode::{DecodeMeta, I420Planes};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_directory(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ozma-playback-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    fn frame(sequence: u64, width: u32, height: u32) -> DecodedVideoFrame {
        let luma = usize::try_from(width * height).unwrap();
        let chroma = usize::try_from(width.div_ceil(2) * height.div_ceil(2)).unwrap();
        DecodedVideoFrame::new(
            DecodeMeta::new(sequence * 1_000, sequence, sequence == 0),
            width,
            height,
            I420Planes::new(vec![16; luma], vec![128; chroma], vec![128; chroma]),
        )
        .unwrap()
    }

    #[test]
    fn null_sink_records_dimensions_and_stops_idempotently() {
        let mut sink = NullVideoSink::new();
        assert!(sink.present(&frame(0, 2, 2)).is_err());

        sink.open().unwrap();
        sink.open().unwrap();
        sink.present(&frame(0, 4, 2)).unwrap();
        sink.present(&frame(1, 6, 4)).unwrap();
        assert_eq!(sink.frames_presented(), 2);
        assert_eq!(sink.dimensions(), Some((6, 4)));
        assert_eq!(sink.records()[0].yuv_bytes, 4 * 2 + 2 * 1 * 2);
        assert!(sink.records()[0].keyframe);
        assert_eq!(
            sink.bytes_presented(),
            u64::try_from(sink.records()[0].yuv_bytes + sink.records()[1].yuv_bytes).unwrap()
        );

        sink.flush().unwrap();
        assert_eq!(sink.flushes(), 1);
        sink.stop().unwrap();
        sink.stop().unwrap();
        assert_eq!(sink.stop_calls(), 2);
        assert!(!sink.is_open());
        assert!(sink.present(&frame(2, 2, 2)).is_err());
    }

    #[test]
    fn ppm_sink_writes_a_valid_header_and_numbered_frames() {
        let directory = temp_directory("ppm-numbered");
        let mut sink = PpmVideoSink::with_mode(&directory, PpmMode::Numbered).unwrap();
        sink.present(&frame(0, 6, 4)).unwrap();
        sink.present(&frame(1, 6, 4)).unwrap();
        assert_eq!(sink.frames_written(), 2);

        let first = std::fs::read(directory.join("frame-000001.ppm")).unwrap();
        let header = b"P6\n6 4\n255\n";
        assert!(first.starts_with(header));
        assert_eq!(first.len(), header.len() + 6 * 4 * 3);

        let second = std::fs::read(directory.join("frame-000002.ppm")).unwrap();
        assert_eq!(second.len(), first.len());

        sink.stop().unwrap();
        sink.stop().unwrap();
        assert_eq!(sink.stop_calls(), 2);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn ppm_first_frame_mode_writes_once_and_reports_skips() {
        let directory = temp_directory("ppm-first");
        let mut sink = PpmVideoSink::new(&directory).unwrap();
        sink.present(&frame(0, 2, 2)).unwrap();
        sink.present(&frame(1, 2, 2)).unwrap();
        assert_eq!(sink.frames_written(), 1);
        assert_eq!(sink.frames_skipped(), 1);
        assert!(directory.join("frame.ppm").is_file());
        assert!(!directory.join("frame-000002.ppm").exists());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn ppm_io_errors_are_structured() {
        let directory = temp_directory("ppm-io");
        let _sink = PpmVideoSink::new(&directory).unwrap();
        let file = directory.join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();
        let error = PpmVideoSink::new(file).unwrap_err();
        assert_eq!(error.code(), crate::error::playback_codes::PLAYBACK_IO);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
