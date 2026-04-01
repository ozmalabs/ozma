# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Virtual display capture — creates a V4L2 loopback device for a QEMU VM.

Makes a soft node's VM display appear as a standard /dev/video* device,
identical to a real USB capture card on a hardware node. The entire
capture pipeline (display_capture.py, OCR, streaming, recording)
works without knowing whether the source is hardware or virtual.

Architecture:
  QEMU VM → VNC display → ffmpeg → v4l2loopback → /dev/videoN
                                                      ↓
                                               same as hardware:
                                               display_capture.py
                                               text_capture.py (OCR)
                                               session_recording.py
                                               replay_buffer.py

This bridges the gap between virtual and physical. The soft node
becomes a true emulation of a hardware node — same device paths,
same capture pipeline, same APIs.

Requirements:
  - v4l2loopback kernel module (modprobe v4l2loopback)
  - ffmpeg with VNC input support (standard)
"""

from __future__ import annotations

import asyncio
import logging
import shutil
import subprocess
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.softnode.virtual_capture")


def _find_v4l2loopback_device(label: str) -> str | None:
    """Find an existing v4l2loopback device by its card label.

    Matches on any substring — so "vm1" matches "Ozma_Virtual_vm1" and
    "Ozma Virtual vm1". Handles both underscore and space in labels
    (v4l2loopback may normalise spaces to underscores).
    """
    # Normalise the search label
    label_variants = {label, label.replace(" ", "_"), label.replace("_", " ")}

    for dev in sorted(Path("/sys/class/video4linux").glob("video*")):
        name_file = dev / "name"
        if name_file.exists():
            name = name_file.read_text().strip()
            for variant in label_variants:
                if variant in name:
                    return f"/dev/{dev.name}"
    return None


def _next_video_device() -> int:
    """Find the next available /dev/video number."""
    existing = set()
    for dev in Path("/dev").glob("video*"):
        try:
            existing.add(int(dev.name.replace("video", "")))
        except ValueError:
            pass
    n = 10  # start at 10 to avoid conflicting with real cameras
    while n in existing:
        n += 1
    return n


class VirtualCapture:
    """
    Creates a v4l2loopback virtual capture device for a QEMU VM and
    feeds the VM's VNC display into it via ffmpeg.

    The resulting /dev/videoN device is indistinguishable from a real
    USB capture card to any V4L2 consumer.

    Usage:
        cap = VirtualCapture(vm_name="vm1", vnc_host="127.0.0.1", vnc_port=5901)
        device_path = await cap.start()
        # device_path is "/dev/video10" or similar
        # display_capture.py can now capture from it like any other card
        await cap.stop()
    """

    def __init__(self, vm_name: str, vnc_host: str = "127.0.0.1",
                 vnc_port: int = 5901, width: int = 1024, height: int = 768,
                 fps: int = 20) -> None:
        self._vm_name = vm_name
        self._vnc_host = vnc_host
        self._vnc_port = vnc_port
        self._width = width
        self._height = height
        self._fps = fps
        self._device_path: str | None = None
        self._device_num: int | None = None
        self._ffmpeg_proc: asyncio.subprocess.Process | None = None
        self._loopback_created = False

    @property
    def device_path(self) -> str | None:
        """The /dev/videoN path, or None if not started."""
        return self._device_path

    @property
    def active(self) -> bool:
        if getattr(self, '_hls_active', False):
            return True
        return self._ffmpeg_proc is not None and self._ffmpeg_proc.returncode is None

    async def start(self) -> str | None:
        """
        Create a v4l2loopback device and start feeding VNC frames into it.

        Returns the /dev/videoN path, or None on failure.
        """
        if not shutil.which("ffmpeg"):
            log.warning("ffmpeg not found — virtual capture disabled")
            return None

        # Try v4l2loopback first — makes the VM look like a real capture card
        v4l2_ok = False
        if await self._ensure_v4l2loopback():
            label = f"Ozma Virtual {self._vm_name}"
            existing = _find_v4l2loopback_device(label)
            if existing:
                self._device_path = existing
            else:
                self._device_num = _next_video_device()
                ok = await self._create_loopback_device(self._device_num, label)
                if ok:
                    self._device_path = f"/dev/video{self._device_num}"
                    self._loopback_created = True

            if self._device_path:
                await self._start_ffmpeg()
                v4l2_ok = self.active

        if v4l2_ok:
            log.info("Virtual capture active: %s VNC :%d → %s (%dx%d@%dfps)",
                     self._vm_name, self._vnc_port - 5900,
                     self._device_path, self._width, self._height, self._fps)
        else:
            # No v4l2loopback — the controller's StreamManager will handle
            # VNC→HLS streaming directly (it has its own asyncvnc connection)
            log.info("v4l2loopback unavailable — controller will stream VNC directly")
            return self._device_path
        return None

    async def stop(self) -> None:
        """Stop the capture and optionally remove the loopback device."""
        self._hls_active = False
        if self._ffmpeg_proc is not None and hasattr(self._ffmpeg_proc, 'returncode'):
            if self._ffmpeg_proc.returncode is None:
                self._ffmpeg_proc.terminate()
                try:
                    await asyncio.wait_for(self._ffmpeg_proc.wait(), timeout=5.0)
                except asyncio.TimeoutError:
                    self._ffmpeg_proc.kill()
            self._ffmpeg_proc = None

        if self._loopback_created and self._device_num is not None:
            await self._remove_loopback_device(self._device_num)
            self._loopback_created = False

        self._device_path = None
        log.info("Virtual capture stopped: %s", self._vm_name)

    async def _ensure_v4l2loopback(self) -> bool:
        """Ensure v4l2loopback kernel module is loaded."""
        try:
            result = subprocess.run(
                ["lsmod"], capture_output=True, text=True, timeout=5
            )
            if "v4l2loopback" in result.stdout:
                return True

            # Try to load it
            proc = await asyncio.create_subprocess_exec(
                "sudo", "modprobe", "v4l2loopback",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr = await proc.communicate()
            if proc.returncode != 0:
                log.warning("Failed to load v4l2loopback: %s",
                            stderr.decode().strip())
                return False
            return True
        except Exception as e:
            log.warning("v4l2loopback check failed: %s", e)
            return False

    async def _create_loopback_device(self, device_num: int, label: str) -> bool:
        """Create a v4l2loopback device with a specific number and label."""
        try:
            # Remove and reload with our device number
            proc = await asyncio.create_subprocess_exec(
                "sudo", "modprobe", "v4l2loopback",
                f"video_nr={device_num}",
                f"card_label={label}",
                "exclusive_caps=1",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr = await proc.communicate()
            if proc.returncode != 0:
                # Module may already be loaded — try v4l2loopback-ctl
                if shutil.which("v4l2loopback-ctl"):
                    proc2 = await asyncio.create_subprocess_exec(
                        "sudo", "v4l2loopback-ctl", "add",
                        "-n", label,
                        stdout=asyncio.subprocess.PIPE,
                        stderr=asyncio.subprocess.PIPE,
                    )
                    stdout2, _ = await proc2.communicate()
                    if proc2.returncode == 0:
                        # Parse the device path from output
                        dev_path = stdout2.decode().strip()
                        if dev_path.startswith("/dev/"):
                            self._device_path = dev_path
                            return True
                log.warning("Failed to create v4l2loopback device %d: %s",
                            device_num, stderr.decode().strip())
                return False

            # Wait for device to appear
            dev_path = Path(f"/dev/video{device_num}")
            for _ in range(20):
                if dev_path.exists():
                    return True
                await asyncio.sleep(0.1)

            log.warning("v4l2loopback device did not appear: %s", dev_path)
            return False
        except Exception as e:
            log.warning("Failed to create loopback device: %s", e)
            return False

    async def _remove_loopback_device(self, device_num: int) -> None:
        """Remove a v4l2loopback device."""
        if shutil.which("v4l2loopback-ctl"):
            try:
                await asyncio.create_subprocess_exec(
                    "sudo", "v4l2loopback-ctl", "delete",
                    f"/dev/video{device_num}",
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                )
            except Exception:
                pass

    async def _start_ffmpeg_hls(self) -> None:
        """VNC screenshots → ffmpeg pipe → HLS. Runs as async task on main loop."""
        import asyncvnc

        hls_dir = Path(f"/tmp/ozma-stream-{self._vm_name}")
        hls_dir.mkdir(parents=True, exist_ok=True)

        try:
            async with asyncvnc.connect(self._vnc_host, self._vnc_port) as client:
                w, h = client.video.width, client.video.height
                log.info("VNC connected for HLS: %s:%d (%dx%d)",
                         self._vnc_host, self._vnc_port, w, h)

                cmd = [
                    "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
                    "-f", "rawvideo", "-pix_fmt", "bgr24",
                    "-video_size", f"{w}x{h}", "-r", str(self._fps),
                    "-i", "pipe:0",
                    "-c:v", "libx264", "-preset", "ultrafast",
                    "-tune", "zerolatency", "-pix_fmt", "yuv420p", "-crf", "28",
                    "-f", "hls", "-hls_time", "2", "-hls_list_size", "4",
                    "-hls_flags", "delete_segments+independent_segments",
                    "-hls_segment_filename", str(hls_dir / "seg_%05d.ts"),
                    str(hls_dir / "stream.m3u8"),
                ]

                proc = await asyncio.create_subprocess_exec(
                    *cmd, stdin=asyncio.subprocess.PIPE,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                )

                interval = 1.0 / self._fps
                frame_count = 0
                while self._hls_active and proc.returncode is None:
                    try:
                        frame = await client.screenshot()
                        if frame is not None and proc.stdin:
                            bgr = frame[:, :, :3].copy()
                            # Use to_thread for the blocking pipe write
                            await asyncio.to_thread(proc.stdin.write, bgr.tobytes())
                            frame_count += 1
                            if frame_count == 1:
                                log.info("First HLS frame (%d bytes)", len(bgr.tobytes()))
                        await asyncio.sleep(interval)
                    except (BrokenPipeError, ConnectionResetError):
                        log.warning("HLS pipe broken after %d frames", frame_count)
                        break
                    except Exception as e:
                        log.debug("VNC frame error: %s", e)
                        await asyncio.sleep(1)

                log.info("HLS capture ended after %d frames", frame_count)

        except Exception as e:
            log.warning("VNC→HLS failed: %s", e)

    async def _start_ffmpeg(self) -> None:
        """Start ffmpeg reading VNC and writing to v4l2loopback."""
        vnc_url = f"vnc://{self._vnc_host}:{self._vnc_port}"

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning",
            # Input: VNC
            "-f", "rawvideo",
            "-r", str(self._fps),
            # We use the VNC protocol via ffmpeg's built-in VNC input
            "-i", vnc_url,
            # Scale to target size and convert pixel format
            "-vf", f"scale={self._width}:{self._height},format=yuv420p",
            # Output: v4l2loopback
            "-f", "v4l2",
            "-pix_fmt", "yuv420p",
            self._device_path,
        ]

        try:
            self._ffmpeg_proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            # Monitor stderr in background
            asyncio.create_task(self._monitor_stderr(), name=f"vcap-log-{self._vm_name}")
        except Exception as e:
            log.warning("Failed to start virtual capture ffmpeg: %s", e)

    async def _monitor_stderr(self) -> None:
        """Log ffmpeg stderr output."""
        if not self._ffmpeg_proc or not self._ffmpeg_proc.stderr:
            return
        try:
            async for line in self._ffmpeg_proc.stderr:
                text = line.decode(errors="replace").rstrip()
                if text:
                    log.debug("Virtual capture %s: %s", self._vm_name, text)
        except Exception:
            pass

    def to_dict(self) -> dict:
        return {
            "vm_name": self._vm_name,
            "device_path": self._device_path,
            "active": self.active,
            "resolution": f"{self._width}x{self._height}",
            "fps": self._fps,
        }
