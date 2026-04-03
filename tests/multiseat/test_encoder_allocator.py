# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.encoder_allocator — encoder session management.

This is the most critical test file. The encoder allocator determines which
GPU encoder each seat uses, respecting NVENC session limits and gaming GPU
avoidance. Getting the scoring math wrong means degraded quality or
exhausted session slots.
"""

from __future__ import annotations

import pytest

from agent.multiseat.gpu_inventory import GPUInventory, GPUInfo, EncoderInfo
from agent.multiseat.encoder_allocator import (
    EncoderAllocator, EncoderHints, EncoderSession,
)


# ── Helpers ──────────────────────────────────────────────────────────────────

def _make_inventory(
    *,
    igpu: bool = False,
    nvidia_count: int = 0,
    nvidia_sessions: int = 5,
    software: bool = True,
) -> GPUInventory:
    """Create a GPUInventory with controlled GPU configuration."""
    inv = GPUInventory()
    inv._discovered = True
    gpu_idx = 0

    if igpu:
        gpu = GPUInfo(
            index=gpu_idx, name="Intel UHD 770", vendor="intel",
            is_igpu=True, pci_slot="0000:00:02.0",
            render_device="/dev/dri/renderD128",
            encoders=[
                EncoderInfo(name="h264_qsv", codec="h264", gpu_index=gpu_idx,
                            max_sessions=-1, quality=7, latency=3),
                EncoderInfo(name="hevc_qsv", codec="h265", gpu_index=gpu_idx,
                            max_sessions=-1, quality=7, latency=3),
            ],
        )
        inv._gpus.append(gpu)
        gpu_idx += 1

    for i in range(nvidia_count):
        gpu = GPUInfo(
            index=gpu_idx, name=f"NVIDIA GeForce RTX 4070 #{i}",
            vendor="nvidia", is_igpu=False,
            pci_slot=f"0000:0{gpu_idx}:00.0",
            vram_mb=12288,
            render_device=f"/dev/dri/renderD{128 + gpu_idx}",
            encoders=[
                EncoderInfo(name="h264_nvenc", codec="h264",
                            gpu_index=gpu_idx,
                            max_sessions=nvidia_sessions,
                            quality=8, latency=2),
                EncoderInfo(name="hevc_nvenc", codec="h265",
                            gpu_index=gpu_idx,
                            max_sessions=nvidia_sessions,
                            quality=8, latency=2),
            ],
        )
        inv._gpus.append(gpu)
        gpu_idx += 1

    if software:
        inv._software_encoders = [
            EncoderInfo(name="libx264", codec="h264", gpu_index=-1,
                        max_sessions=-1, quality=9, latency=8),
            EncoderInfo(name="libx265", codec="h265", gpu_index=-1,
                        max_sessions=-1, quality=9, latency=8),
        ]

    return inv


# ── Basic allocation ─────────────────────────────────────────────────────────

class TestBasicAllocation:
    def test_allocate_software_only(self):
        """With only software encoders, should allocate libx264."""
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)

        session = alloc.allocate("seat-0")
        assert session.encoder.name == "libx264"
        assert session.gpu_index == -1
        assert session.seat_name == "seat-0"

    def test_allocate_returns_ffmpeg_args(self):
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)

        session = alloc.allocate("seat-0")
        assert len(session.ffmpeg_args) > 0
        assert "-c:v" in session.ffmpeg_args
        assert "libx264" in session.ffmpeg_args

    def test_allocate_with_nvenc(self):
        """NVENC should score higher than software."""
        inv = _make_inventory(nvidia_count=1, software=True)
        alloc = EncoderAllocator(inv)

        session = alloc.allocate("seat-0")
        assert session.encoder.name == "h264_nvenc"
        assert session.gpu_index == 1 if inv._gpus[0].is_igpu else 0

    def test_allocate_no_encoders_raises(self):
        """With no encoders at all, should raise RuntimeError."""
        inv = _make_inventory(software=False)
        alloc = EncoderAllocator(inv)

        with pytest.raises(RuntimeError, match="No encoder available"):
            alloc.allocate("seat-0")

    def test_allocate_h265(self):
        """Request H.265 codec."""
        inv = _make_inventory(nvidia_count=1, software=True)
        alloc = EncoderAllocator(inv)

        hints = EncoderHints(codec="h265")
        session = alloc.allocate("seat-0", hints)
        assert session.encoder.codec == "h265"


# ── iGPU preference ──────────────────────────────────────────────────────────

class TestIGPUPreference:
    def test_igpu_preferred_over_dgpu(self):
        """iGPU encoder should score higher than dGPU for encoding."""
        inv = _make_inventory(igpu=True, nvidia_count=1, software=True)
        alloc = EncoderAllocator(inv)

        session = alloc.allocate("seat-0")
        # iGPU QSV should be preferred (score +100 base)
        assert session.encoder.name == "h264_qsv"
        igpu = inv._gpus[0]
        assert session.gpu_index == igpu.index

    def test_igpu_preferred_over_software(self):
        """iGPU should beat software encoder."""
        inv = _make_inventory(igpu=True, software=True)
        alloc = EncoderAllocator(inv)

        session = alloc.allocate("seat-0")
        assert session.encoder.name == "h264_qsv"


# ── NVENC session limits ─────────────────────────────────────────────────────

class TestNVENCSessionLimits:
    def test_session_count_tracked(self):
        """Allocating should increment session count."""
        inv = _make_inventory(nvidia_count=1, software=True)
        alloc = EncoderAllocator(inv)
        gpu_idx = inv._gpus[0].index

        alloc.allocate("seat-0")
        assert alloc.active_sessions_on(gpu_idx, "h264_nvenc") == 1

        alloc.allocate("seat-1")
        assert alloc.active_sessions_on(gpu_idx, "h264_nvenc") == 2

    def test_session_limit_enforced(self):
        """When NVENC sessions are full, should fall back to software."""
        inv = _make_inventory(nvidia_count=1, nvidia_sessions=2, software=True)
        alloc = EncoderAllocator(inv)

        # Fill all NVENC slots
        alloc.allocate("seat-0")
        alloc.allocate("seat-1")

        # Third allocation should fall back to software
        session = alloc.allocate("seat-2")
        assert session.encoder.name == "libx264"

    def test_unlimited_sessions(self):
        """Quadro/professional GPUs have unlimited sessions (-1)."""
        inv = GPUInventory()
        inv._discovered = True
        gpu = GPUInfo(
            index=0, name="Quadro RTX 6000", vendor="nvidia",
            is_igpu=False, pci_slot="",
            encoders=[
                EncoderInfo(name="h264_nvenc", codec="h264", gpu_index=0,
                            max_sessions=-1, quality=8, latency=2),
            ],
        )
        inv._gpus.append(gpu)
        inv._software_encoders = [
            EncoderInfo(name="libx264", codec="h264", gpu_index=-1,
                        max_sessions=-1, quality=9, latency=8),
        ]

        alloc = EncoderAllocator(inv)
        # Should be able to allocate many sessions
        for i in range(10):
            session = alloc.allocate(f"seat-{i}")
            assert session.encoder.name == "h264_nvenc"

    def test_multiple_gpus_distribute(self):
        """With two NVENC GPUs, sessions should spread across them."""
        inv = _make_inventory(nvidia_count=2, nvidia_sessions=2, software=True)
        alloc = EncoderAllocator(inv)

        sessions = []
        for i in range(4):
            sessions.append(alloc.allocate(f"seat-{i}"))

        # All should be NVENC (2 per GPU = 4 total)
        nvenc_count = sum(1 for s in sessions if "nvenc" in s.encoder.name)
        assert nvenc_count == 4

    def test_sessions_across_different_gpus(self):
        inv = _make_inventory(nvidia_count=2, nvidia_sessions=1, software=True)
        alloc = EncoderAllocator(inv)

        s0 = alloc.allocate("seat-0")
        s1 = alloc.allocate("seat-1")

        # Should be on different GPUs
        assert s0.gpu_index != s1.gpu_index
        assert "nvenc" in s0.encoder.name
        assert "nvenc" in s1.encoder.name


# ── Gaming GPU avoidance ─────────────────────────────────────────────────────

class TestGamingGPUAvoidance:
    def test_encoder_avoids_gaming_gpu(self):
        """When gaming hint is set, prefer encoding on different GPU."""
        inv = _make_inventory(igpu=True, nvidia_count=1, software=True)
        alloc = EncoderAllocator(inv)

        nvidia_gpu = [g for g in inv._gpus if g.vendor == "nvidia"][0]
        hints = EncoderHints(gaming_gpu_index=nvidia_gpu.index)

        session = alloc.allocate("seat-0", hints)
        # Should prefer iGPU (score +100) and also it's different from gaming GPU
        assert session.encoder.name == "h264_qsv"

    def test_same_gpu_still_usable(self):
        """NVENC on gaming GPU is still better than software."""
        inv = _make_inventory(nvidia_count=1, software=True)
        alloc = EncoderAllocator(inv)

        nvidia_gpu = inv._gpus[0]
        hints = EncoderHints(gaming_gpu_index=nvidia_gpu.index)

        session = alloc.allocate("seat-0", hints)
        # Even on the same GPU, NVENC scores +60 vs software +20
        assert session.encoder.name == "h264_nvenc"


# ── Release ──────────────────────────────────────────────────────────────────

class TestRelease:
    def test_release_decrements_count(self):
        inv = _make_inventory(nvidia_count=1, software=True)
        alloc = EncoderAllocator(inv)
        gpu_idx = inv._gpus[0].index

        alloc.allocate("seat-0")
        assert alloc.active_sessions_on(gpu_idx, "h264_nvenc") == 1

        alloc.release("seat-0")
        assert alloc.active_sessions_on(gpu_idx, "h264_nvenc") == 0

    def test_release_unknown_seat(self):
        """Releasing an unknown seat should not raise."""
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)
        alloc.release("nonexistent")  # should be a no-op

    def test_release_frees_slot(self):
        """After release, the slot should be usable again."""
        inv = _make_inventory(nvidia_count=1, nvidia_sessions=1, software=True)
        alloc = EncoderAllocator(inv)

        alloc.allocate("seat-0")
        # NVENC is full now
        s1 = alloc.allocate("seat-1")
        assert s1.encoder.name == "libx264"

        # Release seat-0
        alloc.release("seat-0")

        # Now seat-2 should get NVENC
        s2 = alloc.allocate("seat-2")
        assert s2.encoder.name == "h264_nvenc"

    def test_double_allocate_releases_first(self):
        """Allocating the same seat name twice should release the first session."""
        inv = _make_inventory(nvidia_count=1, nvidia_sessions=2, software=True)
        alloc = EncoderAllocator(inv)
        gpu_idx = inv._gpus[0].index

        alloc.allocate("seat-0")
        assert alloc.active_sessions_on(gpu_idx, "h264_nvenc") == 1

        alloc.allocate("seat-0")  # re-allocate
        assert alloc.active_sessions_on(gpu_idx, "h264_nvenc") == 1  # still 1, not 2


# ── Rebalance ────────────────────────────────────────────────────────────────

class TestRebalance:
    def test_rebalance_no_change_when_optimal(self):
        """Rebalance should return empty list when already optimal."""
        inv = _make_inventory(igpu=True, nvidia_count=1, software=True)
        alloc = EncoderAllocator(inv)

        alloc.allocate("seat-0")  # gets iGPU QSV
        reassigned = alloc.rebalance()
        assert reassigned == []

    def test_rebalance_upgrades_to_better_encoder(self):
        """Rebalance after NVENC slot frees up should upgrade software sessions."""
        inv = _make_inventory(nvidia_count=1, nvidia_sessions=1, software=True)
        alloc = EncoderAllocator(inv)

        alloc.allocate("seat-0")  # gets NVENC
        alloc.allocate("seat-1")  # gets software (NVENC full)

        assert alloc.sessions["seat-1"].encoder.name == "libx264"

        # Free the NVENC slot
        alloc.release("seat-0")

        # Rebalance should move seat-1 to NVENC (score diff > 20)
        reassigned = alloc.rebalance()
        assert "seat-1" in reassigned
        assert alloc.sessions["seat-1"].encoder.name == "h264_nvenc"

    def test_rebalance_threshold(self):
        """Rebalance only reassigns when score difference > 20 points."""
        inv = _make_inventory(nvidia_count=1, software=True)
        alloc = EncoderAllocator(inv)

        alloc.allocate("seat-0")
        # Already on the best encoder, rebalance should not change anything
        reassigned = alloc.rebalance()
        assert reassigned == []


# ── History tracking ─────────────────────────────────────────────────────────

class TestHistoryTracking:
    def test_allocate_records_history(self):
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)

        alloc.allocate("seat-0")
        history = alloc.get_history()
        assert len(history) == 1
        assert history[0]["action"] == "allocate"
        assert history[0]["seat"] == "seat-0"

    def test_release_records_history(self):
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)

        alloc.allocate("seat-0")
        alloc.release("seat-0")
        history = alloc.get_history()
        assert len(history) == 2
        assert history[1]["action"] == "release"

    def test_history_ring_buffer(self):
        """History should not grow beyond 100 entries."""
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)

        for i in range(120):
            alloc.allocate(f"seat-{i}")
            alloc.release(f"seat-{i}")

        history = alloc.get_history()
        assert len(history) == 100

    def test_rebalance_records_history(self):
        inv = _make_inventory(nvidia_count=1, nvidia_sessions=1, software=True)
        alloc = EncoderAllocator(inv)

        alloc.allocate("seat-0")
        alloc.allocate("seat-1")
        alloc.release("seat-0")
        alloc.rebalance()

        history = alloc.get_history()
        # Find rebalance events
        rebalances = [h for h in history if h["action"] == "rebalance"]
        assert len(rebalances) >= 1


# ── ffmpeg args generation ───────────────────────────────────────────────────

class TestFFmpegArgs:
    def test_nvenc_h264_args(self):
        inv = _make_inventory(nvidia_count=1)
        alloc = EncoderAllocator(inv)
        session = alloc.allocate("seat-0")

        args = session.ffmpeg_args
        assert "-c:v" in args
        assert "h264_nvenc" in args
        assert "-preset" in args
        assert "-gpu" in args

    def test_qsv_h264_args(self):
        inv = _make_inventory(igpu=True)
        alloc = EncoderAllocator(inv)
        session = alloc.allocate("seat-0")

        args = session.ffmpeg_args
        assert "h264_qsv" in args
        assert "-init_hw_device" in args

    def test_software_h264_args(self):
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)
        session = alloc.allocate("seat-0")

        args = session.ffmpeg_args
        assert "libx264" in args
        assert "-preset" in args
        assert "-tune" in args

    def test_get_ffmpeg_args_by_name(self):
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)
        alloc.allocate("seat-0")

        args = alloc.get_ffmpeg_args("seat-0")
        assert "libx264" in args

    def test_get_ffmpeg_args_unknown_seat(self):
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)

        args = alloc.get_ffmpeg_args("nonexistent")
        # Fallback to software
        assert "libx264" in args

    def test_nvenc_h265_args(self):
        inv = _make_inventory(nvidia_count=1)
        alloc = EncoderAllocator(inv)
        hints = EncoderHints(codec="h265")
        session = alloc.allocate("seat-0", hints)

        args = session.ffmpeg_args
        assert "hevc_nvenc" in args

    def test_quality_preference_changes_preset(self):
        inv = _make_inventory(nvidia_count=1)
        alloc = EncoderAllocator(inv)

        # Default (low latency)
        s1 = alloc.allocate("seat-0")
        alloc.release("seat-0")

        # Quality mode
        hints = EncoderHints(prefer_quality=True)
        s2 = alloc.allocate("seat-0", hints)

        # p7 for quality vs p4 for latency
        assert "p7" in s2.ffmpeg_args

    def test_bitrate_in_args(self):
        inv = _make_inventory(nvidia_count=1)
        alloc = EncoderAllocator(inv)
        hints = EncoderHints(max_bitrate_kbps=12000)
        session = alloc.allocate("seat-0", hints)

        assert "12000k" in session.ffmpeg_args


# ── Scoring math ─────────────────────────────────────────────────────────────

class TestScoringMath:
    def test_igpu_base_score_100(self):
        inv = _make_inventory(igpu=True)
        alloc = EncoderAllocator(inv)
        igpu_enc = inv._gpus[0].encoders[0]

        score, reason = alloc._score_encoder(igpu_enc, EncoderHints())
        assert score >= 100
        assert "iGPU" in reason

    def test_software_base_score_20(self):
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)
        sw_enc = inv._software_encoders[0]

        score, reason = alloc._score_encoder(sw_enc, EncoderHints())
        assert score >= 20
        assert "software" in reason

    def test_dgpu_different_card_score_80(self):
        inv = _make_inventory(nvidia_count=2)
        alloc = EncoderAllocator(inv)

        gpu0_enc = inv._gpus[0].encoders[0]
        gpu1_idx = inv._gpus[1].index
        hints = EncoderHints(gaming_gpu_index=gpu1_idx)

        score, reason = alloc._score_encoder(gpu0_enc, hints)
        assert score >= 80
        assert "different card" in reason

    def test_dgpu_same_card_score_60(self):
        inv = _make_inventory(nvidia_count=1)
        alloc = EncoderAllocator(inv)

        gpu0_enc = inv._gpus[0].encoders[0]
        hints = EncoderHints(gaming_gpu_index=inv._gpus[0].index)

        score, reason = alloc._score_encoder(gpu0_enc, hints)
        assert score >= 60
        assert "same card" in reason

    def test_session_headroom_bonus(self):
        inv = _make_inventory(nvidia_count=1, nvidia_sessions=5)
        alloc = EncoderAllocator(inv)

        enc = inv._gpus[0].encoders[0]
        score_before, _ = alloc._score_encoder(enc, EncoderHints())

        # Allocate some sessions to reduce headroom
        alloc.allocate("seat-0")
        alloc.allocate("seat-1")
        alloc.allocate("seat-2")

        score_after, _ = alloc._score_encoder(enc, EncoderHints())
        # Score should decrease as headroom decreases
        assert score_after < score_before


# ── Serialization ────────────────────────────────────────────────────────────

class TestAllocatorSerialization:
    def test_to_dict(self):
        inv = _make_inventory(nvidia_count=1, software=True)
        alloc = EncoderAllocator(inv)
        alloc.allocate("seat-0")

        d = alloc.to_dict()
        assert "sessions" in d
        assert "session_counts" in d
        assert "seat-0" in d["sessions"]

    def test_session_to_dict(self):
        inv = _make_inventory(software=True)
        alloc = EncoderAllocator(inv)
        session = alloc.allocate("seat-0")

        d = session.to_dict()
        assert d["seat_name"] == "seat-0"
        assert "encoder" in d
        assert "ffmpeg_args" in d
        assert "score" in d
