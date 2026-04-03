# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Encoder session allocator for multi-seat.

Tracks encoder sessions across GPUs, enforces NVENC session limits,
and picks the best encoder for each seat based on GPU affinity,
session availability, and quality/latency preferences.

Allocation priority (highest to lowest):
  1. iGPU encoder (dedicated to encoding, keeps dGPU free for gaming)
  2. dGPU encoder on a DIFFERENT card than the seat's gaming GPU
  3. dGPU encoder on the SAME card (NVENC has dedicated silicon)
  4. VAAPI encoder
  5. Software libx264 ultrafast (always available)

NVENC session limits are a real hardware constraint:
  - GeForce GTX 10xx/16xx: 3 concurrent sessions
  - GeForce RTX 20xx-50xx: 5 concurrent sessions
  - Quadro/RTX A-series/Tesla: unlimited
"""

from __future__ import annotations

import logging
from collections import deque
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

from .gpu_inventory import GPUInventory, GPUInfo, EncoderInfo

log = logging.getLogger("ozma.agent.multiseat.encoder")


@dataclass
class EncoderHints:
    """Hints from the seat manager about what this seat needs."""
    gaming_gpu_index: int | None = None   # "this seat is gaming on GPU 0"
    prefer_quality: bool = False           # quality over latency
    max_bitrate_kbps: int = 8000          # bitrate target
    resolution: tuple[int, int] = (1920, 1080)
    fps: int = 60
    codec: str = "h264"                   # preferred codec family


@dataclass
class EncoderSession:
    """An active encoder session allocated to a seat."""
    seat_name: str
    encoder: EncoderInfo
    gpu_index: int
    ffmpeg_args: list[str]
    hints: EncoderHints
    score: int = 0
    reason: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "seat_name": self.seat_name,
            "encoder": self.encoder.to_dict(),
            "gpu_index": self.gpu_index,
            "ffmpeg_args": self.ffmpeg_args,
            "score": self.score,
            "reason": self.reason,
        }


class EncoderAllocator:
    """
    Manages encoder allocation across GPUs for multi-seat sessions.

    Tracks active sessions per GPU, enforces session limits, and picks
    the best available encoder for each seat.
    """

    def __init__(self, inventory: GPUInventory) -> None:
        self._inventory = inventory
        self._sessions: dict[str, EncoderSession] = {}  # seat_name -> session
        # Track session counts per encoder (gpu_index, encoder_name) -> count
        self._session_counts: dict[tuple[int, str], int] = {}
        # Ring buffer of allocation events for diagnostics
        self._history: deque[dict[str, Any]] = deque(maxlen=100)

    @property
    def sessions(self) -> dict[str, EncoderSession]:
        return dict(self._sessions)

    def active_sessions_on(self, gpu_index: int, encoder_name: str) -> int:
        """Number of active sessions for a specific encoder on a specific GPU."""
        return self._session_counts.get((gpu_index, encoder_name), 0)

    def allocate(self, seat_name: str, hints: EncoderHints | None = None) -> EncoderSession:
        """
        Pick the best available encoder for this seat.

        If the seat already has a session, release it first.

        Args:
            seat_name: Unique identifier for the seat.
            hints: Optional preferences (gaming GPU, quality, bitrate).

        Returns:
            An EncoderSession with ready-to-use ffmpeg args.

        Raises:
            RuntimeError: If no encoder is available at all (shouldn't happen
                          since software fallback is always present).
        """
        if seat_name in self._sessions:
            self.release(seat_name)

        hints = hints or EncoderHints()
        candidates = self._score_candidates(hints)

        if not candidates:
            raise RuntimeError("No encoder available — not even software fallback")

        # Pick the highest-scored candidate
        best_score, best_encoder, best_reason = candidates[0]
        ffmpeg_args = self._build_ffmpeg_args(best_encoder, hints)

        session = EncoderSession(
            seat_name=seat_name,
            encoder=best_encoder,
            gpu_index=best_encoder.gpu_index,
            ffmpeg_args=ffmpeg_args,
            hints=hints,
            score=best_score,
            reason=best_reason,
        )

        # Track the session
        self._sessions[seat_name] = session
        key = (best_encoder.gpu_index, best_encoder.name)
        self._session_counts[key] = self._session_counts.get(key, 0) + 1

        gpu = self._inventory.gpu_by_index(best_encoder.gpu_index)
        gpu_name = gpu.name if gpu else "software"
        log.info("Encoder allocated: seat=%s encoder=%s gpu=[%d] %s "
                 "score=%d sessions=%d/%s",
                 seat_name, best_encoder.name, best_encoder.gpu_index,
                 gpu_name, best_score,
                 self._session_counts[key],
                 str(best_encoder.max_sessions) if best_encoder.max_sessions > 0 else "unlimited")

        self._history.append({
            "ts": datetime.now(timezone.utc).isoformat(),
            "seat": seat_name,
            "action": "allocate",
            "encoder": best_encoder.name,
            "gpu_index": best_encoder.gpu_index,
            "score": best_score,
            "reason": best_reason,
        })

        return session

    def release(self, seat_name: str) -> None:
        """Release encoder session when a seat stops."""
        session = self._sessions.pop(seat_name, None)
        if not session:
            return

        key = (session.encoder.gpu_index, session.encoder.name)
        count = self._session_counts.get(key, 0)
        if count > 0:
            self._session_counts[key] = count - 1

        log.info("Encoder released: seat=%s encoder=%s gpu=[%d]",
                 seat_name, session.encoder.name, session.encoder.gpu_index)

        self._history.append({
            "ts": datetime.now(timezone.utc).isoformat(),
            "seat": seat_name,
            "action": "release",
            "encoder": session.encoder.name,
            "gpu_index": session.encoder.gpu_index,
        })

    def rebalance(self) -> list[str]:
        """
        Rebalance all seats to optimal encoders after load changes.

        Returns list of seat names that were reassigned (require capture restart).
        Only reassigns if a significantly better encoder is now available,
        to avoid unnecessary capture restarts.
        """
        reassigned: list[str] = []

        for seat_name, session in list(self._sessions.items()):
            candidates = self._score_candidates(session.hints)
            if not candidates:
                continue

            best_score, best_encoder, _best_reason = candidates[0]
            current_score, _current_reason = self._score_encoder(session.encoder, session.hints)

            # Only reassign if the new option is significantly better (>20 points)
            # to avoid flip-flopping between similar options
            if best_score > current_score + 20:
                old_encoder = session.encoder.name
                self.release(seat_name)
                new_session = self.allocate(seat_name, session.hints)
                reassigned.append(seat_name)
                reason = f"{old_encoder} -> {new_session.encoder.name}: better option available"
                log.info("Rebalanced seat=%s: %s (score=%d) -> %s (score=%d)",
                         seat_name, old_encoder, current_score,
                         new_session.encoder.name, best_score)
                self._history.append({
                    "ts": datetime.now(timezone.utc).isoformat(),
                    "seat": seat_name,
                    "action": "rebalance",
                    "from": old_encoder,
                    "to": new_session.encoder.name,
                    "score": best_score,
                    "reason": reason,
                })

        if not reassigned:
            log.debug("Rebalance: no changes needed")

        return reassigned

    def get_history(self) -> list[dict[str, Any]]:
        """Return the allocation history ring buffer as a list (newest last)."""
        return list(self._history)

    def get_ffmpeg_args(self, seat_name: str) -> list[str]:
        """Return ffmpeg encoder arguments for this seat's allocated encoder."""
        session = self._sessions.get(seat_name)
        if not session:
            # Fallback to software
            return ["-c:v", "libx264", "-preset", "ultrafast",
                    "-tune", "zerolatency", "-b:v", "8000k"]
        return list(session.ffmpeg_args)

    # ── Scoring ──────────────────────────────────────────────────────────────

    def _score_candidates(self, hints: EncoderHints) -> list[tuple[int, EncoderInfo, str]]:
        """
        Score all available encoders and return sorted (highest first).

        Filters out encoders at their session limit.
        Returns list of (score, encoder, reason) tuples.
        """
        candidates: list[tuple[int, EncoderInfo, str]] = []

        for encoder in self._inventory.all_encoders:
            # Filter by codec
            if encoder.codec != hints.codec:
                continue

            # Check session limit
            if encoder.max_sessions > 0:
                current = self.active_sessions_on(encoder.gpu_index, encoder.name)
                if current >= encoder.max_sessions:
                    continue

            score, reason = self._score_encoder(encoder, hints)
            candidates.append((score, encoder, reason))

        # Sort by score descending
        candidates.sort(key=lambda x: x[0], reverse=True)
        return candidates

    def _score_encoder(self, encoder: EncoderInfo, hints: EncoderHints) -> tuple[int, str]:
        """
        Score an encoder for a given set of hints.

        Returns (score, reason) tuple.

        Scoring breakdown:
          Base score by encoder type:
            iGPU encoder:                +100
            dGPU on different card:       +80
            dGPU on same card as gaming:  +60
            VAAPI:                        +40
            Software:                     +20

          Adjustments:
            Quality preference:           +encoder.quality
            Latency preference:           +(10 - encoder.latency)
            Session headroom:             +0..10
        """
        score = 0
        reason = ""
        gpu = self._inventory.gpu_by_index(encoder.gpu_index)

        if encoder.gpu_index == -1:
            # Software encoder
            score += 20
            reason = "software fallback"
        elif gpu and gpu.is_igpu:
            # iGPU — always preferred for encoding
            score += 100
            reason = "iGPU preferred"
        elif gpu and hints.gaming_gpu_index is not None:
            if encoder.gpu_index != hints.gaming_gpu_index:
                # Different GPU than gaming — great
                score += 80
                reason = "dGPU dedicated encoder (different card from gaming)"
            else:
                # Same GPU as gaming — NVENC dedicated silicon still useful
                score += 60
                reason = "dGPU dedicated encoder (gaming on same card)"
        elif gpu:
            # No gaming hint — score based on whether it's a dedicated GPU
            score += 70
            reason = "dGPU encoder"

        # Quality vs latency preference
        if hints.prefer_quality:
            score += encoder.quality
        else:
            score += (10 - encoder.latency)

        # Session headroom — prefer encoders with more room
        if encoder.max_sessions > 0:
            current = self.active_sessions_on(encoder.gpu_index, encoder.name)
            remaining = encoder.max_sessions - current
            headroom_score = min(remaining * 2, 10)
            score += headroom_score
        else:
            # Unlimited sessions — full headroom bonus
            score += 10

        return score, reason

    # ── ffmpeg argument builders ─────────────────────────────────────────────

    def _build_ffmpeg_args(self, encoder: EncoderInfo, hints: EncoderHints) -> list[str]:
        """Build ffmpeg command-line arguments for the selected encoder."""
        bitrate = f"{hints.max_bitrate_kbps}k"

        builders = {
            "h264_nvenc": self._args_nvenc_h264,
            "hevc_nvenc": self._args_nvenc_h265,
            "h264_qsv": self._args_qsv_h264,
            "hevc_qsv": self._args_qsv_h265,
            "h264_vaapi": self._args_vaapi_h264,
            "hevc_vaapi": self._args_vaapi_h265,
            "h264_amf": self._args_amf_h264,
            "hevc_amf": self._args_amf_h265,
            "libx264": self._args_x264,
            "libx265": self._args_x265,
        }

        builder = builders.get(encoder.name)
        if builder:
            return builder(encoder, hints, bitrate)

        # Unknown encoder — minimal args
        return ["-c:v", encoder.name, "-b:v", bitrate]

    def _args_nvenc_h264(self, enc: EncoderInfo, hints: EncoderHints, bitrate: str) -> list[str]:
        preset = "p7" if hints.prefer_quality else "p4"
        args = [
            "-c:v", "h264_nvenc",
            "-preset", preset,
            "-tune", "ll",
            "-rc", "cbr",
            "-b:v", bitrate,
            "-gpu", str(enc.gpu_index),
            "-bf", "0",
        ]
        return args

    def _args_nvenc_h265(self, enc: EncoderInfo, hints: EncoderHints, bitrate: str) -> list[str]:
        preset = "p7" if hints.prefer_quality else "p4"
        args = [
            "-c:v", "hevc_nvenc",
            "-preset", preset,
            "-tune", "ll",
            "-rc", "cbr",
            "-b:v", bitrate,
            "-gpu", str(enc.gpu_index),
            "-bf", "0",
        ]
        return args

    def _args_qsv_h264(self, enc: EncoderInfo, hints: EncoderHints, bitrate: str) -> list[str]:
        gpu = self._inventory.gpu_by_index(enc.gpu_index)
        render_dev = gpu.render_device if gpu else "/dev/dri/renderD128"
        preset = "medium" if hints.prefer_quality else "veryfast"
        return [
            "-init_hw_device", f"qsv=hw,child_device={render_dev}",
            "-c:v", "h264_qsv",
            "-preset", preset,
            "-global_quality", "25",
            "-b:v", bitrate,
            "-bf", "0",
        ]

    def _args_qsv_h265(self, enc: EncoderInfo, hints: EncoderHints, bitrate: str) -> list[str]:
        gpu = self._inventory.gpu_by_index(enc.gpu_index)
        render_dev = gpu.render_device if gpu else "/dev/dri/renderD128"
        preset = "medium" if hints.prefer_quality else "veryfast"
        return [
            "-init_hw_device", f"qsv=hw,child_device={render_dev}",
            "-c:v", "hevc_qsv",
            "-preset", preset,
            "-global_quality", "25",
            "-b:v", bitrate,
            "-bf", "0",
        ]

    def _args_vaapi_h264(self, enc: EncoderInfo, hints: EncoderHints, bitrate: str) -> list[str]:
        gpu = self._inventory.gpu_by_index(enc.gpu_index)
        render_dev = gpu.render_device if gpu else "/dev/dri/renderD128"
        return [
            "-init_hw_device", f"vaapi=hw:{render_dev}",
            "-filter_hw_device", "hw",
            "-vf", "format=nv12,hwupload",
            "-c:v", "h264_vaapi",
            "-b:v", bitrate,
            "-bf", "0",
        ]

    def _args_vaapi_h265(self, enc: EncoderInfo, hints: EncoderHints, bitrate: str) -> list[str]:
        gpu = self._inventory.gpu_by_index(enc.gpu_index)
        render_dev = gpu.render_device if gpu else "/dev/dri/renderD128"
        return [
            "-init_hw_device", f"vaapi=hw:{render_dev}",
            "-filter_hw_device", "hw",
            "-vf", "format=nv12,hwupload",
            "-c:v", "hevc_vaapi",
            "-b:v", bitrate,
            "-bf", "0",
        ]

    def _args_amf_h264(self, enc: EncoderInfo, hints: EncoderHints, bitrate: str) -> list[str]:
        return [
            "-c:v", "h264_amf",
            "-quality", "balanced" if hints.prefer_quality else "speed",
            "-b:v", bitrate,
            "-bf", "0",
        ]

    def _args_amf_h265(self, enc: EncoderInfo, hints: EncoderHints, bitrate: str) -> list[str]:
        return [
            "-c:v", "hevc_amf",
            "-quality", "balanced" if hints.prefer_quality else "speed",
            "-b:v", bitrate,
            "-bf", "0",
        ]

    def _args_x264(self, enc: EncoderInfo, hints: EncoderHints, bitrate: str) -> list[str]:
        preset = "fast" if hints.prefer_quality else "ultrafast"
        return [
            "-c:v", "libx264",
            "-preset", preset,
            "-tune", "zerolatency",
            "-b:v", bitrate,
            "-bf", "0",
        ]

    def _args_x265(self, enc: EncoderInfo, hints: EncoderHints, bitrate: str) -> list[str]:
        preset = "fast" if hints.prefer_quality else "ultrafast"
        return [
            "-c:v", "libx265",
            "-preset", preset,
            "-tune", "zerolatency",
            "-b:v", bitrate,
            "-bf", "0",
        ]

    # ── Status ───────────────────────────────────────────────────────────────

    def to_dict(self) -> dict[str, Any]:
        return {
            "sessions": {k: v.to_dict() for k, v in self._sessions.items()},
            "session_counts": {
                f"gpu{k[0]}:{k[1]}": v
                for k, v in self._session_counts.items()
            },
        }
