# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.gpu_inventory — GPU discovery and encoder probing."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from agent.multiseat.gpu_inventory import (
    GPUInventory, GPUInfo, EncoderInfo,
    _NVENC_SESSION_LIMITS, _VENDOR_NVIDIA, _VENDOR_AMD, _VENDOR_INTEL,
)


# ── GPUInfo data model ───────────────────────────────────────────────────────

class TestGPUInfo:
    def test_basic_gpu(self):
        gpu = GPUInfo(
            index=0, name="NVIDIA GeForce RTX 4070",
            vendor="nvidia", is_igpu=False, pci_slot="0000:01:00.0",
            vram_mb=12288, render_device="/dev/dri/renderD128",
        )
        assert gpu.index == 0
        assert gpu.vendor == "nvidia"
        assert gpu.is_igpu is False
        assert gpu.vram_mb == 12288
        assert gpu.encoders == []

    def test_igpu(self):
        gpu = GPUInfo(
            index=0, name="Intel UHD 770",
            vendor="intel", is_igpu=True, pci_slot="0000:00:02.0",
        )
        assert gpu.is_igpu is True

    def test_to_dict(self):
        gpu = GPUInfo(
            index=0, name="Test GPU", vendor="nvidia",
            is_igpu=False, pci_slot="0000:01:00.0",
            vram_mb=8192, render_device="/dev/dri/renderD128",
            encoders=[
                EncoderInfo(name="h264_nvenc", codec="h264", gpu_index=0,
                            max_sessions=5, quality=8, latency=2),
            ],
        )
        d = gpu.to_dict()
        assert d["index"] == 0
        assert d["name"] == "Test GPU"
        assert d["vendor"] == "nvidia"
        assert d["is_igpu"] is False
        assert d["vram_mb"] == 8192
        assert len(d["encoders"]) == 1
        assert d["encoders"][0]["name"] == "h264_nvenc"


# ── EncoderInfo data model ───────────────────────────────────────────────────

class TestEncoderInfo:
    def test_nvenc_encoder(self):
        enc = EncoderInfo(
            name="h264_nvenc", codec="h264", gpu_index=0,
            max_sessions=5, quality=8, latency=2,
        )
        assert enc.name == "h264_nvenc"
        assert enc.codec == "h264"
        assert enc.max_sessions == 5

    def test_software_encoder(self):
        enc = EncoderInfo(
            name="libx264", codec="h264", gpu_index=-1,
            max_sessions=-1, quality=9, latency=8,
        )
        assert enc.gpu_index == -1
        assert enc.max_sessions == -1

    def test_to_dict(self):
        enc = EncoderInfo(
            name="h264_qsv", codec="h264", gpu_index=0,
            max_sessions=-1, quality=7, latency=3,
        )
        d = enc.to_dict()
        assert d["name"] == "h264_qsv"
        assert d["codec"] == "h264"
        assert d["gpu_index"] == 0
        assert d["max_sessions"] == -1


# ── NVENC session limits ─────────────────────────────────────────────────────

class TestNVENCSessionLimits:
    def test_rtx_4070_limit(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("NVIDIA GeForce RTX 4070")
        assert limit == 5

    def test_rtx_3090_limit(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("NVIDIA GeForce RTX 3090")
        assert limit == 5

    def test_gtx_1080_limit(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("NVIDIA GeForce GTX 1080")
        assert limit == 3

    def test_gtx_1650_limit(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("NVIDIA GeForce GTX 1650")
        assert limit == 3

    def test_quadro_unlimited(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("Quadro RTX 6000")
        assert limit == -1

    def test_a100_unlimited(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("NVIDIA A100")
        assert limit == -1

    def test_tesla_unlimited(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("Tesla V100")
        assert limit == -1

    def test_rtx_a4000_unlimited(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("NVIDIA RTX A4000")
        assert limit == -1

    def test_unknown_nvidia_default(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("NVIDIA Unknown GPU 9999")
        # Unknown = assume consumer limit
        assert limit == 5

    def test_geforce_catchall(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("GeForce Something")
        assert limit == 5

    def test_rtx_5090_limit(self):
        inv = GPUInventory()
        limit = inv._nvenc_session_limit("NVIDIA GeForce RTX 5090")
        assert limit == 5


# ── GPUInventory ─────────────────────────────────────────────────────────────

class TestGPUInventory:
    def test_initial_state(self):
        inv = GPUInventory()
        assert inv.gpus == []
        assert inv.all_encoders == []
        assert inv.software_encoders == []

    def test_gpu_by_index(self):
        inv = GPUInventory()
        gpu = GPUInfo(index=0, name="Test", vendor="nvidia",
                      is_igpu=False, pci_slot="")
        inv._gpus.append(gpu)
        assert inv.gpu_by_index(0) is gpu
        assert inv.gpu_by_index(1) is None

    def test_all_encoders_includes_software(self):
        inv = GPUInventory()
        gpu = GPUInfo(index=0, name="Test", vendor="nvidia",
                      is_igpu=False, pci_slot="",
                      encoders=[
                          EncoderInfo(name="h264_nvenc", codec="h264",
                                      gpu_index=0, max_sessions=5,
                                      quality=8, latency=2),
                      ])
        inv._gpus.append(gpu)
        inv._software_encoders.append(
            EncoderInfo(name="libx264", codec="h264", gpu_index=-1,
                        max_sessions=-1, quality=9, latency=8),
        )

        all_enc = inv.all_encoders
        assert len(all_enc) == 2
        names = {e.name for e in all_enc}
        assert "h264_nvenc" in names
        assert "libx264" in names

    def test_to_dict(self):
        inv = GPUInventory()
        gpu = GPUInfo(index=0, name="Test", vendor="nvidia",
                      is_igpu=False, pci_slot="")
        inv._gpus.append(gpu)
        d = inv.to_dict()
        assert "gpus" in d
        assert "software_encoders" in d
        assert len(d["gpus"]) == 1

    @pytest.mark.asyncio
    async def test_discover_caches_results(self):
        inv = GPUInventory()
        inv._discovered = True
        inv._gpus = [GPUInfo(index=0, name="Cached", vendor="nvidia",
                             is_igpu=False, pci_slot="")]

        result = await inv.discover()
        assert len(result) == 1
        assert result[0].name == "Cached"


# ── Encoder probing with mocked ffmpeg ───────────────────────────────────────

class TestEncoderProbing:
    @pytest.mark.asyncio
    async def test_test_encoder_success(self):
        inv = GPUInventory()
        mock_proc = AsyncMock()
        mock_proc.returncode = 0
        mock_proc.wait = AsyncMock(return_value=0)

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await inv._test_encoder("libx264")
        assert result is True

    @pytest.mark.asyncio
    async def test_test_encoder_failure(self):
        inv = GPUInventory()
        mock_proc = AsyncMock()
        mock_proc.returncode = 1
        mock_proc.wait = AsyncMock(return_value=1)

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await inv._test_encoder("h264_nvenc_nonexistent")
        assert result is False

    @pytest.mark.asyncio
    async def test_test_encoder_exception(self):
        inv = GPUInventory()
        with patch("asyncio.create_subprocess_exec", side_effect=FileNotFoundError):
            result = await inv._test_encoder("libx264")
        assert result is False

    @pytest.mark.asyncio
    async def test_probe_software_encoders(self):
        inv = GPUInventory()
        mock_proc = AsyncMock()
        mock_proc.returncode = 0
        mock_proc.wait = AsyncMock(return_value=0)

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            await inv._probe_software_encoders()

        # Should find both libx264 and libx265
        names = {e.name for e in inv._software_encoders}
        assert "libx264" in names
        assert "libx265" in names

    @pytest.mark.asyncio
    async def test_probe_nvenc(self):
        inv = GPUInventory()
        gpu = GPUInfo(index=0, name="NVIDIA GeForce RTX 4070",
                      vendor="nvidia", is_igpu=False, pci_slot="")

        mock_proc = AsyncMock()
        mock_proc.returncode = 0
        mock_proc.wait = AsyncMock(return_value=0)

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            await inv._probe_nvenc(gpu)

        enc_names = {e.name for e in gpu.encoders}
        assert "h264_nvenc" in enc_names
        assert "hevc_nvenc" in enc_names
        # Check session limits match RTX 40 series
        for enc in gpu.encoders:
            assert enc.max_sessions == 5


# ── iGPU vs dGPU classification ─────────────────────────────────────────────

class TestGPUClassification:
    def test_nvidia_is_never_igpu(self):
        gpu = GPUInfo(index=0, name="NVIDIA GeForce RTX 4070",
                      vendor="nvidia", is_igpu=False, pci_slot="")
        assert gpu.is_igpu is False

    def test_intel_uhd_is_igpu(self):
        gpu = GPUInfo(index=0, name="Intel UHD 770",
                      vendor="intel", is_igpu=True, pci_slot="")
        assert gpu.is_igpu is True

    def test_intel_arc_is_dgpu(self):
        gpu = GPUInfo(index=1, name="Intel Arc A770",
                      vendor="intel", is_igpu=False, pci_slot="")
        assert gpu.is_igpu is False

    def test_amd_igpu_check(self):
        inv = GPUInventory()
        # AMD APU with boot_vga=1 is iGPU
        # We test the check method needs a device_dir, mock it
        with patch("pathlib.Path.exists", return_value=True), \
             patch("pathlib.Path.read_text", return_value="1\n"):
            result = inv._check_amd_igpu(Path("/sys/class/drm/card0/device"))
        assert result is True
