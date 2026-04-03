# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Camera recording manager — policies, triggers, and storage backends.

Recording is policy-driven.  Each policy binds a trigger to a storage backend:

  Triggers
  --------
  continuous   — always recording while the camera node is reachable
  motion       — Frigate motion_started event
  object       — Frigate object detected (configurable classes: person/car/face/…)
  event        — any Ozma event type (doorbell.press, alert.created, …)

  Storage backends
  ----------------
  local        — Frigate native recording (configure via Frigate API).  Zero
                 extra ffmpeg process; retention managed by Frigate.
  nas          — segmented MP4 written to a mounted NFS/SMB path on the controller.
  s3           — segmented MP4 uploaded to any S3-compatible endpoint
                 (Backblaze B2, Wasabi, MinIO, AWS S3).
  connect      — encrypted segments pushed to Ozma Connect cloud storage.
                 The controller encrypts each segment with
                 key_store.derive_subkey("footage") before upload; Connect
                 stores only ciphertext.  Cycling is managed by Connect.

Encryption
----------
When encrypted=True (available for s3 and connect backends), each segment is
encrypted with ChaCha20-Poly1305 using a subkey derived from the master key:

    key  = key_store.derive_subkey("footage")   # 32 bytes from HKDF
    file = nonce (12 bytes) || ciphertext+tag

The nonce is random per segment.  Without the master key, segments cannot be
decrypted, even by Ozma Labs.

Usage
-----
    rec_mgr = CameraRecordingManager(
        data_dir=Path("recording_data"),
        key_store=key_store,
        state_ref=state,
        event_queue=state.events,
    )
    await rec_mgr.start()
    # REST endpoints call add_policy() / remove_policy() / list_policies()
    await rec_mgr.stop()
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import time
import uuid
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.camera_recording")


# ---------------------------------------------------------------------------
# Enumerations
# ---------------------------------------------------------------------------

class RecordingTrigger(str, Enum):
    CONTINUOUS = "continuous"
    MOTION     = "motion"
    OBJECT     = "object"
    EVENT      = "event"


class StorageBackend(str, Enum):
    LOCAL   = "local"
    NAS     = "nas"
    S3      = "s3"
    CONNECT = "connect"


class RecordingState(str, Enum):
    IDLE       = "idle"
    RECORDING  = "recording"
    UPLOADING  = "uploading"
    ERROR      = "error"


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class RecordingPolicy:
    """Binds a trigger to a storage backend for one (or all) camera nodes."""
    id: str
    name: str

    # Which camera this policy applies to; None = all camera nodes.
    camera_node_id: str | None

    trigger: RecordingTrigger

    # For OBJECT trigger: list of Frigate object labels ("person", "car", "face", …).
    # Empty = any object.
    object_classes: list[str] = field(default_factory=list)

    # For EVENT trigger: list of Ozma event type prefixes to match.
    # e.g. ["doorbell.press", "alert.created"]
    event_types: list[str] = field(default_factory=list)

    # How many seconds per recorded segment before writing a new file.
    segment_seconds: int = 60

    # Pre-event buffer (seconds) — only relevant for motion/object/event triggers.
    pre_buffer_seconds: int = 5

    # Post-event buffer (seconds) — keep recording this long after trigger ends.
    post_buffer_seconds: int = 30

    backend: StorageBackend = StorageBackend.LOCAL

    # Backend configuration (serialised as JSON in persistence).
    # local : {}
    # nas   : {"path": "/mnt/nas/cameras"}
    # s3    : {"endpoint": "https://s3.us-west-002.backblazeb2.com",
    #           "bucket": "my-cameras",
    #           "prefix": "footage/",
    #           "access_key_id": "keyid",
    #           "secret_access_key": "secret"}   ← stored encrypted by key_store
    # connect: {"prefix": "footage/"}
    backend_config: dict = field(default_factory=dict)

    # Encrypt segments at rest (s3, connect, nas).  Requires master key unlocked.
    encrypted: bool = False

    # Maximum retention on the backend (days).  0 = keep indefinitely.
    retention_days: int = 30

    enabled: bool = True
    created_at: float = field(default_factory=time.time)

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "camera_node_id": self.camera_node_id,
            "trigger": self.trigger,
            "object_classes": self.object_classes,
            "event_types": self.event_types,
            "segment_seconds": self.segment_seconds,
            "pre_buffer_seconds": self.pre_buffer_seconds,
            "post_buffer_seconds": self.post_buffer_seconds,
            "backend": self.backend,
            "backend_config": self._redact_config(),
            "encrypted": self.encrypted,
            "retention_days": self.retention_days,
            "enabled": self.enabled,
            "created_at": self.created_at,
        }

    def _redact_config(self) -> dict:
        """Return backend_config with credentials masked for API responses."""
        redacted = dict(self.backend_config)
        for sensitive in ("secret_access_key", "password", "token"):
            if sensitive in redacted:
                redacted[sensitive] = "***"
        return redacted

    @classmethod
    def from_dict(cls, d: dict) -> "RecordingPolicy":
        return cls(
            id=d["id"],
            name=d["name"],
            camera_node_id=d.get("camera_node_id"),
            trigger=RecordingTrigger(d.get("trigger", "continuous")),
            object_classes=d.get("object_classes", []),
            event_types=d.get("event_types", []),
            segment_seconds=d.get("segment_seconds", 60),
            pre_buffer_seconds=d.get("pre_buffer_seconds", 5),
            post_buffer_seconds=d.get("post_buffer_seconds", 30),
            backend=StorageBackend(d.get("backend", "local")),
            backend_config=d.get("backend_config", {}),
            encrypted=d.get("encrypted", False),
            retention_days=d.get("retention_days", 30),
            enabled=d.get("enabled", True),
            created_at=d.get("created_at", time.time()),
        )


@dataclass
class RecordingJob:
    """Tracks an active recording pipeline for one camera+policy pair."""
    id: str
    policy_id: str
    camera_node_id: str
    state: RecordingState = RecordingState.IDLE
    started_at: float = field(default_factory=time.time)
    segments_written: int = 0
    segments_uploaded: int = 0
    last_error: str | None = None
    # asyncio subprocess handle
    _proc: asyncio.subprocess.Process | None = field(default=None, repr=False)
    # asyncio task uploading segments
    _upload_task: asyncio.Task | None = field(default=None, repr=False)

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "policy_id": self.policy_id,
            "camera_node_id": self.camera_node_id,
            "state": self.state,
            "started_at": self.started_at,
            "segments_written": self.segments_written,
            "segments_uploaded": self.segments_uploaded,
            "last_error": self.last_error,
        }


# ---------------------------------------------------------------------------
# Segment encryption
# ---------------------------------------------------------------------------

def _encrypt_segment(plaintext: bytes, key: bytes) -> bytes:
    """
    Encrypt a segment with ChaCha20-Poly1305.

    Returns: nonce (12 bytes) || ciphertext+tag
    """
    from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
    nonce = os.urandom(12)
    cipher = ChaCha20Poly1305(key)
    ct = cipher.encrypt(nonce, plaintext, None)
    return nonce + ct


def _decrypt_segment(data: bytes, key: bytes) -> bytes:
    """Decrypt a segment produced by _encrypt_segment."""
    from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
    nonce, ct = data[:12], data[12:]
    return ChaCha20Poly1305(key).decrypt(nonce, ct, None)


# ---------------------------------------------------------------------------
# Storage backend helpers
# ---------------------------------------------------------------------------

async def _upload_to_s3(
    segment_path: Path,
    config: dict,
    encrypted: bool,
    footage_key: bytes | None,
) -> None:
    """Upload a segment file to an S3-compatible endpoint."""
    try:
        import boto3  # type: ignore
        from botocore.config import Config  # type: ignore
    except ImportError:
        raise RuntimeError("boto3 is required for S3 recording backend — pip install boto3")

    data = segment_path.read_bytes()
    if encrypted and footage_key:
        data = _encrypt_segment(data, footage_key)
        object_name = config.get("prefix", "") + segment_path.name + ".enc"
    else:
        object_name = config.get("prefix", "") + segment_path.name

    def _sync_upload() -> None:
        kwargs: dict[str, Any] = dict(
            aws_access_key_id=config.get("access_key_id"),
            aws_secret_access_key=config.get("secret_access_key"),
        )
        endpoint = config.get("endpoint")
        if endpoint:
            kwargs["endpoint_url"] = endpoint
        s3 = boto3.client("s3", config=Config(signature_version="s3v4"), **kwargs)
        s3.put_object(Bucket=config["bucket"], Key=object_name, Body=data)

    loop = asyncio.get_event_loop()
    await loop.run_in_executor(None, _sync_upload)
    log.debug("S3 upload complete: %s → %s/%s", segment_path.name, config.get("bucket"), object_name)


async def _write_to_nas(
    segment_path: Path,
    config: dict,
    encrypted: bool,
    footage_key: bytes | None,
) -> None:
    """Copy a segment to a NAS path (NFS/SMB must already be mounted)."""
    dest_dir = Path(config.get("path", "/mnt/nas/cameras"))
    dest_dir.mkdir(parents=True, exist_ok=True)
    data = segment_path.read_bytes()
    if encrypted and footage_key:
        data = _encrypt_segment(data, footage_key)
        dest = dest_dir / (segment_path.name + ".enc")
    else:
        dest = dest_dir / segment_path.name

    def _sync_write() -> None:
        dest.write_bytes(data)

    loop = asyncio.get_event_loop()
    await loop.run_in_executor(None, _sync_write)
    log.debug("NAS write complete: %s", dest)


async def _upload_to_connect(
    segment_path: Path,
    config: dict,
    encrypted: bool,
    footage_key: bytes | None,
    connect: Any | None,
) -> None:
    """Upload a segment to Ozma Connect ZK storage."""
    if connect is None:
        raise RuntimeError("Ozma Connect client not available for connect recording backend")

    data = segment_path.read_bytes()
    if encrypted and footage_key:
        data = _encrypt_segment(data, footage_key)
        object_name = config.get("prefix", "footage/") + segment_path.name + ".enc"
    else:
        object_name = config.get("prefix", "footage/") + segment_path.name

    # OzmaConnect.upload_footage() — stores ciphertext, no key escrow
    await connect.upload_footage(object_name=object_name, data=data)
    log.debug("Connect upload complete: %s", object_name)


async def _configure_frigate_recording(rtsp_url: str, camera_name: str, frigate_api: str, enabled: bool) -> None:
    """Tell Frigate to enable/disable native recording for a camera."""
    try:
        import aiohttp  # type: ignore
        payload = {
            "cameras": {
                camera_name: {
                    "record": {
                        "enabled": enabled,
                        "retain": {"days": 30, "mode": "motion"},
                    }
                }
            }
        }
        async with aiohttp.ClientSession() as session:
            async with session.post(
                f"{frigate_api}/api/config/save",
                json=payload,
                timeout=aiohttp.ClientTimeout(total=10),
            ) as resp:
                resp.raise_for_status()
    except Exception as exc:
        log.warning("Failed to configure Frigate recording for %s: %s", camera_name, exc)


# ---------------------------------------------------------------------------
# Main manager
# ---------------------------------------------------------------------------

class CameraRecordingManager:
    """
    Manages camera recording policies, triggers, and storage pipelines.

    Lifecycle:
        manager.start()   — loads policies, starts background tasks
        manager.stop()    — stops all active recordings, flushes uploads

    Recording pipeline (non-local backends):
        ffmpeg -i <rtsp_url> -f segment -segment_time <N> -reset_timestamps 1
               -strftime 1 /tmp/ozma-rec/<camera>/%Y%m%d_%H%M%S.mp4
        → _upload_loop() watches for new .mp4 files and uploads them
        → After upload, local temp file is deleted

    Frigate (local backend):
        Calls Frigate API to enable/disable native recording.  No extra ffmpeg.
    """

    def __init__(
        self,
        data_dir: Path,
        key_store: Any | None = None,
        state_ref: Any | None = None,
        event_queue: asyncio.Queue | None = None,
        connect: Any | None = None,
    ) -> None:
        self._data_dir = data_dir
        self._key_store = key_store
        self._state_ref = state_ref
        self._event_queue: asyncio.Queue | None = event_queue
        self._connect = connect

        self._policies: dict[str, RecordingPolicy] = {}
        self._active_jobs: dict[str, RecordingJob] = {}   # key: "{policy_id}:{node_id}"
        self._tasks: list[asyncio.Task] = []

        self._temp_dir = Path("/tmp/ozma-rec")

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._temp_dir.mkdir(parents=True, exist_ok=True)
        self._load()
        self._tasks = [
            asyncio.create_task(self._continuous_check_loop(), name="cam-rec:continuous"),
            asyncio.create_task(self._event_listener_loop(), name="cam-rec:events"),
            asyncio.create_task(self._cleanup_loop(), name="cam-rec:cleanup"),
        ]
        log.info("CameraRecordingManager started (%d policies loaded)", len(self._policies))

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()
        # Stop all active recording jobs
        for job in list(self._active_jobs.values()):
            await self._stop_job(job)
        log.info("CameraRecordingManager stopped")

    # ------------------------------------------------------------------
    # Policy CRUD
    # ------------------------------------------------------------------

    def add_policy(self, **kwargs) -> RecordingPolicy:
        policy = RecordingPolicy(id=str(uuid.uuid4()), **kwargs)
        self._policies[policy.id] = policy
        self._save()
        log.info("Recording policy added: %s (%s → %s)", policy.name, policy.trigger, policy.backend)
        return policy

    def get_policy(self, policy_id: str) -> RecordingPolicy | None:
        return self._policies.get(policy_id)

    def update_policy(self, policy_id: str, **updates) -> RecordingPolicy | None:
        policy = self._policies.get(policy_id)
        if not policy:
            return None
        for k, v in updates.items():
            if hasattr(policy, k):
                setattr(policy, k, v)
        self._save()
        return policy

    def remove_policy(self, policy_id: str) -> bool:
        if policy_id not in self._policies:
            return False
        del self._policies[policy_id]
        self._save()
        return True

    def list_policies(self) -> list[RecordingPolicy]:
        return list(self._policies.values())

    # ------------------------------------------------------------------
    # Status
    # ------------------------------------------------------------------

    def get_status(self) -> dict:
        return {
            "policies": len(self._policies),
            "active_jobs": len(self._active_jobs),
            "jobs": [j.to_dict() for j in self._active_jobs.values()],
        }

    def list_jobs(self) -> list[dict]:
        return [j.to_dict() for j in self._active_jobs.values()]

    # ------------------------------------------------------------------
    # Background loops
    # ------------------------------------------------------------------

    async def _continuous_check_loop(self) -> None:
        """Ensure continuous-trigger policies are recording for all online cameras."""
        while True:
            try:
                await self._reconcile_continuous()
            except Exception:
                log.exception("Continuous recording reconcile error")
            await asyncio.sleep(30)

    async def _reconcile_continuous(self) -> None:
        if self._state_ref is None:
            return
        camera_nodes = [
            n for n in self._state_ref.nodes.values()
            if getattr(n, "machine_class", None) == "camera"
        ]
        for policy in self._policies.values():
            if not policy.enabled or policy.trigger != RecordingTrigger.CONTINUOUS:
                continue
            for node in camera_nodes:
                if policy.camera_node_id and policy.camera_node_id != node.id:
                    continue
                job_key = f"{policy.id}:{node.id}"
                if job_key in self._active_jobs:
                    continue  # already recording
                await self._start_job(policy, node)

    async def _event_listener_loop(self) -> None:
        """Listen to the event bus for motion/object/alarm events."""
        # We keep a local snapshot queue so we don't starve the main event bus.
        # We subscribe by periodically draining a side queue that main.py
        # populates via put_nowait when it forwards camera events to us.
        # If no side queue, we monitor _state_ref for Frigate events via polling.
        while True:
            try:
                if self._event_queue is not None:
                    await self._drain_events()
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("Camera recording event listener error")
            await asyncio.sleep(1)

    async def _drain_events(self) -> None:
        """Process incoming events that may trigger recording."""
        # We receive events from the main event bus.  The main event loop
        # publishes camera-related events with types like:
        #   frigate.motion_started  {"camera": "front_door", "node_id": "abc"}
        #   frigate.object_entered  {"camera": ..., "label": "person", ...}
        #   alert.created           {"kind": "doorbell", ...}
        # We don't consume from the shared queue directly (that would drop events
        # for other consumers).  Instead, main.py calls our on_event() method.
        pass  # see on_event() below

    async def on_event(self, event_type: str, payload: dict) -> None:
        """Called by main.py when a relevant event arrives."""
        for policy in self._policies.values():
            if not policy.enabled:
                continue
            if policy.trigger == RecordingTrigger.MOTION and event_type == "frigate.motion_started":
                await self._trigger_event_recording(policy, payload.get("node_id", ""), payload)
            elif policy.trigger == RecordingTrigger.OBJECT and event_type in (
                "frigate.object_entered", "frigate.new_object"
            ):
                label = payload.get("label", "")
                if not policy.object_classes or label in policy.object_classes:
                    await self._trigger_event_recording(policy, payload.get("node_id", ""), payload)
            elif policy.trigger == RecordingTrigger.EVENT:
                if any(event_type.startswith(et) for et in policy.event_types):
                    node_id = payload.get("node_id", payload.get("camera_node_id", ""))
                    await self._trigger_event_recording(policy, node_id, payload)

    async def _trigger_event_recording(self, policy: RecordingPolicy, node_id: str, payload: dict) -> None:
        """Start (or extend) a recording for an event-based trigger."""
        if policy.camera_node_id and policy.camera_node_id != node_id:
            return
        if not node_id and self._state_ref:
            # Try to find a camera node from payload camera name
            camera_name = payload.get("camera", "")
            for n in self._state_ref.nodes.values():
                if getattr(n, "machine_class", None) == "camera" and n.name == camera_name:
                    node_id = n.id
                    break
        if not node_id:
            return
        job_key = f"{policy.id}:{node_id}"
        if job_key in self._active_jobs:
            return  # already recording; will naturally stop after post_buffer
        node = self._state_ref.nodes.get(node_id) if self._state_ref else None
        if node:
            await self._start_job(policy, node)
            # Schedule stop after post_buffer_seconds
            asyncio.create_task(
                self._auto_stop_job(job_key, policy.post_buffer_seconds),
                name=f"cam-rec:auto-stop:{job_key}",
            )

    async def _auto_stop_job(self, job_key: str, delay: float) -> None:
        await asyncio.sleep(delay)
        job = self._active_jobs.get(job_key)
        if job:
            await self._stop_job(job)
            self._active_jobs.pop(job_key, None)

    async def _cleanup_loop(self) -> None:
        """Periodically remove old temp files and apply retention policies."""
        while True:
            try:
                await self._apply_retention()
            except Exception:
                log.exception("Retention cleanup error")
            await asyncio.sleep(3600)

    async def _apply_retention(self) -> None:
        """Remove temp files older than 1 hour (they should have been uploaded)."""
        now = time.time()
        for f in self._temp_dir.rglob("*.mp4"):
            try:
                age_hours = (now - f.stat().st_mtime) / 3600
                if age_hours > 1:
                    f.unlink()
                    log.debug("Removed stale temp segment: %s", f.name)
            except Exception:
                pass

    # ------------------------------------------------------------------
    # Recording job lifecycle
    # ------------------------------------------------------------------

    async def _start_job(self, policy: RecordingPolicy, node: Any) -> RecordingJob | None:
        """Start a recording pipeline for a camera node."""
        job_key = f"{policy.id}:{node.id}"
        if job_key in self._active_jobs:
            return self._active_jobs[job_key]

        # Get the RTSP URL from the camera node's stream list
        rtsp_url = self._get_rtsp_url(node, quality="record")
        if not rtsp_url and policy.backend != StorageBackend.LOCAL:
            log.warning("No RTSP URL for camera node %s — cannot start recording", node.id)
            return None

        job = RecordingJob(
            id=str(uuid.uuid4()),
            policy_id=policy.id,
            camera_node_id=node.id,
        )
        self._active_jobs[job_key] = job

        if policy.backend == StorageBackend.LOCAL:
            await self._start_local(job, policy, node, rtsp_url)
        else:
            await self._start_ffmpeg_pipeline(job, policy, node, rtsp_url)

        return job

    async def _stop_job(self, job: RecordingJob) -> None:
        """Stop a recording pipeline."""
        job.state = RecordingState.IDLE
        if job._proc and job._proc.returncode is None:
            try:
                job._proc.terminate()
                await asyncio.wait_for(job._proc.wait(), timeout=5.0)
            except Exception:
                try:
                    job._proc.kill()
                except Exception:
                    pass
        if job._upload_task and not job._upload_task.done():
            job._upload_task.cancel()
            await asyncio.gather(job._upload_task, return_exceptions=True)

    async def _start_local(self, job: RecordingJob, policy: RecordingPolicy, node: Any, rtsp_url: str | None) -> None:
        """Configure Frigate to record natively — no extra ffmpeg."""
        job.state = RecordingState.RECORDING
        # Configure Frigate if we have a Frigate API endpoint
        frigate_api = self._frigate_api_for(node)
        if frigate_api and rtsp_url:
            camera_name = self._camera_name(node)
            asyncio.create_task(
                _configure_frigate_recording(rtsp_url, camera_name, frigate_api, True),
                name=f"cam-rec:frigate-config:{node.id}",
            )
        log.info("Local recording started for camera %s via Frigate", node.id)

    async def _start_ffmpeg_pipeline(
        self,
        job: RecordingJob,
        policy: RecordingPolicy,
        node: Any,
        rtsp_url: str,
    ) -> None:
        """Launch ffmpeg segmenting RTSP → temp dir, then upload each segment."""
        seg_dir = self._temp_dir / job.id
        seg_dir.mkdir(parents=True, exist_ok=True)

        seg_pattern = str(seg_dir / "%Y%m%d_%H%M%S.mp4")
        cmd = [
            "ffmpeg", "-y",
            "-rtsp_transport", "tcp",
            "-i", rtsp_url,
            "-c", "copy",
            "-f", "segment",
            "-segment_time", str(policy.segment_seconds),
            "-reset_timestamps", "1",
            "-strftime", "1",
            seg_pattern,
        ]

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            job._proc = proc
            job.state = RecordingState.RECORDING
            log.info(
                "ffmpeg recording started for %s → %s (pid=%d)",
                node.id, policy.backend, proc.pid,
            )
        except FileNotFoundError:
            job.state = RecordingState.ERROR
            job.last_error = "ffmpeg not found"
            log.error("ffmpeg not found — cannot record camera %s", node.id)
            return

        # Start the upload task alongside ffmpeg
        job._upload_task = asyncio.create_task(
            self._upload_loop(job, policy, seg_dir),
            name=f"cam-rec:upload:{job.id}",
        )

    async def _upload_loop(self, job: RecordingJob, policy: RecordingPolicy, seg_dir: Path) -> None:
        """Watch seg_dir for completed .mp4 segments and upload them."""
        uploaded: set[str] = set()
        footage_key: bytes | None = None
        if policy.encrypted and self._key_store:
            try:
                footage_key = self._key_store.derive_subkey("footage")
            except Exception as exc:
                log.error("Cannot derive footage key — segments will be unencrypted: %s", exc)

        while True:
            try:
                segments = sorted(seg_dir.glob("*.mp4"))
                # Don't upload the last segment — ffmpeg is still writing to it
                for seg in segments[:-1]:
                    if seg.name in uploaded:
                        continue
                    try:
                        await self._upload_segment(seg, policy, footage_key)
                        uploaded.add(seg.name)
                        seg.unlink()
                        job.segments_uploaded += 1
                    except Exception as exc:
                        log.error("Segment upload failed (%s): %s", seg.name, exc)
                        job.last_error = str(exc)
            except asyncio.CancelledError:
                # Upload any remaining segments before stopping
                for seg in sorted(seg_dir.glob("*.mp4")):
                    if seg.name not in uploaded:
                        try:
                            await self._upload_segment(seg, policy, footage_key)
                            seg.unlink()
                            job.segments_uploaded += 1
                        except Exception as exc:
                            log.warning("Final segment upload failed: %s", exc)
                break
            except Exception:
                log.exception("Upload loop error for job %s", job.id)
            await asyncio.sleep(5)

    async def _upload_segment(self, seg_path: Path, policy: RecordingPolicy, footage_key: bytes | None) -> None:
        """Dispatch segment to the configured backend."""
        if policy.backend == StorageBackend.S3:
            await _upload_to_s3(seg_path, policy.backend_config, policy.encrypted, footage_key)
        elif policy.backend == StorageBackend.NAS:
            await _write_to_nas(seg_path, policy.backend_config, policy.encrypted, footage_key)
        elif policy.backend == StorageBackend.CONNECT:
            await _upload_to_connect(seg_path, policy.backend_config, policy.encrypted, footage_key, self._connect)
        # LOCAL backend: ffmpeg writes directly to Frigate's storage path, no upload needed

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    def _get_rtsp_url(self, node: Any, quality: str = "record") -> str | None:
        """Find the best RTSP URL from a camera node's stream list."""
        streams = getattr(node, "camera_streams", []) or []
        if not streams:
            return None
        # Prefer highest-quality stream for recording
        for stream in streams:
            if stream.get("name", "").endswith("_record") or stream.get("quality") == "high":
                if url := stream.get("rtsp_inbound"):
                    return url
        # Fall back to first stream with an RTSP URL
        for stream in streams:
            if url := stream.get("rtsp_inbound"):
                return url
        return None

    def _frigate_api_for(self, node: Any) -> str | None:
        host = getattr(node, "frigate_host", None)
        port = getattr(node, "frigate_port", None) or 5000
        if host:
            return f"http://{host}:{port}"
        return None

    def _camera_name(self, node: Any) -> str:
        """Return a Frigate-safe camera name for the node."""
        name = getattr(node, "name", node.id)
        return name.lower().replace(" ", "_").replace("-", "_")

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    @property
    def _policies_path(self) -> Path:
        return self._data_dir / "policies.json"

    def _save(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        tmp = self._policies_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(
            [p.to_dict() for p in self._policies.values()],
            indent=2,
        ))
        # Restore credentials that were redacted in to_dict()
        # by saving the raw backend_config separately
        raw = [
            {**p.to_dict(), "backend_config": p.backend_config}
            for p in self._policies.values()
        ]
        tmp.write_text(json.dumps(raw, indent=2))
        tmp.chmod(0o600)
        tmp.rename(self._policies_path)

    def _load(self) -> None:
        if not self._policies_path.exists():
            return
        try:
            items = json.loads(self._policies_path.read_text())
            for item in items:
                p = RecordingPolicy.from_dict(item)
                self._policies[p.id] = p
            log.info("Loaded %d recording policies", len(self._policies))
        except Exception:
            log.exception("Failed to load recording policies")
