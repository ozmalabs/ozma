// SPDX-License-Identifier: AGPL-3.0-only
//! Safe V4L2 discovery using sysfs and `/dev` only.
//!
//! This module deliberately does not use `ioctl` or any C library. Linux
//! exposes enough metadata in sysfs to list video nodes and read their names,
//! which is all a capability advertisement needs. Resolution, pixel formats,
//! and streaming require `VIDIOC_QUERYCAP`/`VIDIOC_ENUM_FMT` and MMAP buffers;
//! those are a later milestone. Until then [`V4l2Capture`] is an explicit
//! scaffold that reports a structured error instead of pretending to stream.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};

use crate::provider::CaptureProvider;

/// A discovered V4L2 video node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V4l2Node {
    /// Numeric suffix of `/dev/videoN`.
    pub index: u32,
    /// Stable id, normally the device path.
    pub id: String,
    /// Path to the device node.
    pub device_path: PathBuf,
    /// Human-readable name from sysfs, or a generated fallback.
    pub name: String,
    /// Whether the name looks like a capture node rather than a metadata or
    /// output-only node.
    pub capture_like: bool,
}

impl V4l2Node {
    /// Protocol capability for this node.
    ///
    /// Width, height, FPS, and format list are left at zero/empty because
    /// sysfs does not expose them. Consumers must treat zero as "unknown
    /// until opened", not as a limit.
    pub fn capability(&self) -> CaptureCapability {
        CaptureCapability {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: CaptureKind::V4l2,
            formats: Vec::new(),
            max_width: 0,
            max_height: 0,
            max_fps: 0,
            compressed: false,
            hdr: false,
            has_audio: false,
        }
    }
}

impl fmt::Display for V4l2Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.id, self.name)
    }
}

/// Parse the numeric index from a `videoN` filename.
pub fn parse_video_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("video")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Heuristic for whether a sysfs name describes a capture node.
///
/// Without `VIDIOC_QUERYCAP` this cannot be exact. It intentionally excludes
/// names that clearly say "metadata" or "output" and includes the names used by
/// UVC cameras, HDMI capture dongles, and virtual video devices. Discovery
/// keeps the raw node list available through [`discover_v4l2`], so callers that
/// need every node are not limited to this filter.
pub fn is_capture_like_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name.contains("metadata") || name.contains("output") {
        return false;
    }
    [
        "capture",
        "camera",
        "webcam",
        "hdmi",
        "uvc",
        "grabber",
        "elgato",
        "avermedia",
        "video",
        "usb",
    ]
    .iter()
    .any(|keyword| name.contains(keyword))
}

fn api_io_error(path: &Path, error: std::io::Error) -> ApiError {
    ApiError::new(
        codes::CAPTURE_UNAVAILABLE,
        format!("Failed to inspect capture metadata at {}: {error}", path.display()),
    )
    .with_details(path.display().to_string())
    .with_remediation("Check device permissions and that udev has created the video node.")
}

/// Enumerate `/dev/video*` nodes and read their sysfs names.
///
/// `dev_root` is normally `/dev` and `sys_root` is normally
/// `/sys/class/video4linux`; tests inject temporary directories. A missing
/// directory yields an empty list, which is the correct result on a host with
/// no V4L2 subsystem. Other filesystem errors are returned.
pub fn discover_v4l2(dev_root: &Path, sys_root: &Path) -> Result<Vec<V4l2Node>, ApiError> {
    let entries = match fs::read_dir(dev_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(api_io_error(dev_root, error)),
    };

    let mut nodes = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| api_io_error(dev_root, error))?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(index) = parse_video_index(&file_name) else {
            continue;
        };

        let device_path = entry.path();
        match fs::metadata(&device_path) {
            Ok(metadata) if metadata.is_dir() => continue,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(api_io_error(&device_path, error)),
        }

        let sys_name_path = sys_root.join(format!("video{index}")).join("name");
        let name = fs::read_to_string(&sys_name_path)
            .ok()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("V4L2 video{index}"));
        let capture_like = is_capture_like_name(&name);

        nodes.push(V4l2Node {
            index,
            id: device_path.to_string_lossy().into_owned(),
            device_path,
            name,
            capture_like,
        });
    }

    nodes.sort_by_key(|node| node.index);
    Ok(nodes)
}

/// Safe V4L2 discovery provider.
#[derive(Clone, Debug)]
pub struct V4l2Provider {
    dev_root: PathBuf,
    sys_root: PathBuf,
}

impl Default for V4l2Provider {
    fn default() -> Self {
        Self::new()
    }
}

impl V4l2Provider {
    /// Provider rooted at the host's `/dev` and `/sys/class/video4linux`.
    pub fn new() -> Self {
        Self {
            dev_root: PathBuf::from("/dev"),
            sys_root: PathBuf::from("/sys/class/video4linux"),
        }
    }

    /// Provider with injected roots, for tests and unusual containers.
    pub fn with_roots(dev_root: impl Into<PathBuf>, sys_root: impl Into<PathBuf>) -> Self {
        Self {
            dev_root: dev_root.into(),
            sys_root: sys_root.into(),
        }
    }

    /// Discover every video node, including metadata/output nodes.
    pub fn discover(&self) -> Result<Vec<V4l2Node>, ApiError> {
        discover_v4l2(&self.dev_root, &self.sys_root)
    }
}

impl CaptureProvider for V4l2Provider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::V4l2
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        Ok(self
            .discover()?
            .into_iter()
            .filter(|node| node.capture_like)
            .map(|node| node.capability())
            .collect())
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::V4l2 {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                "The V4L2 provider cannot open a non-V4L2 capability.",
            ));
        }
        let node = self
            .discover()?
            .into_iter()
            .find(|node| node.id == capability.id)
            .ok_or_else(|| {
                ApiError::new(
                    codes::NOT_FOUND,
                    format!("V4L2 device `{}` is no longer present.", capability.id),
                )
                .with_details(capability.id.clone())
                .with_remediation("Replug the capture device and refresh capture discovery.")
            })?;
        Ok(Box::new(V4l2Capture::new(node)))
    }
}

/// A scaffold for a real V4L2 capture source.
///
/// It exists so the registry and API can resolve a discovered device, and it
/// always returns a structured `capture.unavailable` error from frame reads.
/// It does **not** stream. Do not advertise it as a working source.
#[derive(Clone, Debug)]
pub struct V4l2Capture {
    node: V4l2Node,
    stopped: bool,
}

impl V4l2Capture {
    /// Wrap a discovered node without opening it.
    pub fn new(node: V4l2Node) -> Self {
        Self {
            node,
            stopped: false,
        }
    }

    /// The discovered node behind this scaffold.
    pub fn node(&self) -> &V4l2Node {
        &self.node
    }

    /// Device path, for diagnostics.
    pub fn device_path(&self) -> &Path {
        &self.node.device_path
    }
}

#[async_trait]
impl CaptureSource for V4l2Capture {
    fn id(&self) -> &str {
        &self.node.id
    }

    fn format(&self) -> FrameFormat {
        FrameFormat::new(PixelFormat::Mjpeg, 0, 0)
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        Err(MediaError::Api(
            ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                format!(
                    "V4L2 capture for `{}` is a discovery-only scaffold; MMAP streaming is not implemented yet.",
                    self.node.id
                ),
            )
            .with_details(self.node.id.clone())
            .with_remediation(
                "Use the fake capture source for CI, or enable the V4L2 MMAP backend when it lands.",
            ),
        ))
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        self.stopped = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "ozma-capture-{}-{tag}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (TempTree, PathBuf, PathBuf) {
        let tree = TempTree::new("discovery");
        let dev = tree.path().join("dev");
        let sys = tree.path().join("sys/class/video4linux");
        fs::create_dir_all(&dev).unwrap();
        fs::create_dir_all(&sys).unwrap();

        for (index, name) in [
            (10, "USB Capture HDMI"),
            (2, "Integrated Camera"),
            (0, "UVC Camera"),
            (3, "UVC metadata"),
        ] {
            fs::write(dev.join(format!("video{index}")), b"").unwrap();
            let node = sys.join(format!("video{index}"));
            fs::create_dir_all(&node).unwrap();
            fs::write(node.join("name"), format!("{name}\n")).unwrap();
        }

        // Files that must not be mistaken for video nodes.
        fs::write(dev.join("videoX"), b"").unwrap();
        fs::write(dev.join("notvideo1"), b"").unwrap();
        (tree, dev, sys)
    }

    #[test]
    fn parses_only_numeric_video_names() {
        assert_eq!(parse_video_index("video0"), Some(0));
        assert_eq!(parse_video_index("video12"), Some(12));
        assert_eq!(parse_video_index("video"), None);
        assert_eq!(parse_video_index("videoX"), None);
        assert_eq!(parse_video_index("video12x"), None);
        assert_eq!(parse_video_index("camera0"), None);
    }

    #[test]
    fn capture_heuristic_excludes_metadata_and_output() {
        assert!(is_capture_like_name("USB Capture HDMI"));
        assert!(is_capture_like_name("Integrated Camera"));
        assert!(is_capture_like_name("UVC Camera"));
        assert!(!is_capture_like_name("UVC metadata"));
        assert!(!is_capture_like_name("Video Output"));
        assert!(!is_capture_like_name(""));
    }

    #[test]
    fn discovery_reads_names_and_sorts_numerically() {
        let (_tree, dev, sys) = fixture();
        let nodes = discover_v4l2(&dev, &sys).unwrap();
        let indexes: Vec<u32> = nodes.iter().map(|node| node.index).collect();
        assert_eq!(indexes, vec![0, 2, 3, 10]);
        assert_eq!(nodes[0].name, "UVC Camera");
        assert_eq!(nodes[3].name, "USB Capture HDMI");
        assert!(nodes[0].capture_like);
        assert!(!nodes[2].capture_like);
        assert_eq!(nodes[0].id, dev.join("video0").to_string_lossy().to_string());
    }

    #[test]
    fn provider_enumerates_capture_nodes_only() {
        let (_tree, dev, sys) = fixture();
        let provider = V4l2Provider::with_roots(dev, sys);
        let capabilities = provider.enumerate().unwrap();
        let ids: Vec<String> = capabilities.iter().map(|capability| capability.id.clone()).collect();
        assert_eq!(ids.len(), 3, "metadata node must be excluded");
        assert!(ids[0].ends_with("video0"));
        assert!(ids[1].ends_with("video2"));
        assert!(ids[2].ends_with("video10"));
        assert_eq!(capabilities[0].kind, CaptureKind::V4l2);
        assert_eq!(capabilities[0].max_width, 0, "unknown until opened");
    }

    #[test]
    fn missing_directories_yield_no_sources() {
        let tree = TempTree::new("empty");
        let nodes = discover_v4l2(&tree.path().join("dev"), &tree.path().join("sys")).unwrap();
        assert!(nodes.is_empty());
    }

    #[test]
    fn fallback_name_is_used_when_sysfs_is_missing() {
        let tree = TempTree::new("noname");
        let dev = tree.path().join("dev");
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("video7"), b"").unwrap();
        let nodes = discover_v4l2(&dev, &tree.path().join("sys")).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "V4L2 video7");
    }

    #[test]
    fn open_rejects_unknown_devices() {
        let (_tree, dev, sys) = fixture();
        let provider = V4l2Provider::with_roots(dev, sys);
        let capability = CaptureCapability {
            id: "/dev/video99".to_owned(),
            name: "Missing".to_owned(),
            kind: CaptureKind::V4l2,
            formats: Vec::new(),
            max_width: 0,
            max_height: 0,
            max_fps: 0,
            compressed: false,
            hdr: false,
            has_audio: false,
        };
        let error = provider.open(&capability).err().unwrap();
        assert_eq!(error.code, codes::NOT_FOUND);
    }

    #[tokio::test]
    async fn scaffold_reports_unavailable_instead_of_streaming() {
        let (_tree, dev, sys) = fixture();
        let provider = V4l2Provider::with_roots(dev, sys);
        let capability = provider.enumerate().unwrap().remove(0);
        let mut source = provider.open(&capability).unwrap();
        let error = source.next_frame().await.unwrap_err();
        let api = error.to_api_error();
        assert_eq!(api.code, codes::CAPTURE_UNAVAILABLE);
        assert!(api.message.contains("scaffold"));
        source.stop().await.unwrap();
    }
}
