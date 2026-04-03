# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GPU discovery and encoder capability detection.

Enumerates all GPUs on the system (discrete + integrated) and probes
which hardware encoders are available on each. Used by the encoder
allocator to assign the best encoder to each multi-seat session.

Linux detection:
  - /sys/class/drm/card*/device/ for PCI vendor, slot, boot_vga
  - nvidia-smi for NVIDIA GPU names and NVENC capability
  - vainfo for VAAPI encoder support
  - ffmpeg -encoders for software fallbacks

Windows detection:
  - DXGI IDXGIFactory1::EnumAdapters1 via ctypes for GPU name, vendor, VRAM
  - nvidia-smi fallback for NVIDIA GPU details
  - ffmpeg encoder probes for NVENC, QSV, AMF availability
"""

from __future__ import annotations

import asyncio
import logging
import os
import platform
import re
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.agent.multiseat.encoder")

# PCI vendor IDs
_VENDOR_NVIDIA = "0x10de"
_VENDOR_AMD = "0x1002"
_VENDOR_INTEL = "0x8086"

_VENDOR_NAMES = {
    _VENDOR_NVIDIA: "nvidia",
    _VENDOR_AMD: "amd",
    _VENDOR_INTEL: "intel",
}

# NVENC session limits by GPU generation.
# GeForce consumer cards have firmware-enforced session limits.
# Quadro/RTX A-series/Tesla have no limit.
_NVENC_SESSION_LIMITS: dict[str, int] = {
    # Quadro / professional — unlimited
    "quadro": -1,
    "rtx a": -1,
    "tesla": -1,
    "a100": -1,
    "a10": -1,
    "a30": -1,
    "a40": -1,
    "h100": -1,
    "l40": -1,
    # GeForce — session-limited
    "gtx 10": 3,
    "gtx 16": 3,
    "rtx 20": 5,
    "rtx 30": 5,
    "rtx 40": 5,
    "rtx 50": 5,
    "geforce": 5,  # catch-all for unrecognised GeForce
}


@dataclass
class EncoderInfo:
    """A single hardware or software encoder on a specific GPU."""
    name: str           # "h264_nvenc", "h264_qsv", "h264_vaapi", "h264_amf"
    codec: str          # "h264" or "h265"
    gpu_index: int      # which GPU this encoder lives on
    max_sessions: int   # GeForce: 3-5, Quadro: unlimited (-1), QSV: unlimited (-1)
    quality: int        # relative quality score 1-10 (higher = better)
    latency: int        # relative latency score 1-10 (lower = better)

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "codec": self.codec,
            "gpu_index": self.gpu_index,
            "max_sessions": self.max_sessions,
            "quality": self.quality,
            "latency": self.latency,
        }


@dataclass
class GPUInfo:
    """A single GPU (discrete or integrated) detected on the system."""
    index: int                  # card index (drm card number)
    name: str                   # "NVIDIA GeForce RTX 4070", "Intel UHD 770"
    vendor: str                 # "nvidia", "amd", "intel"
    is_igpu: bool               # integrated vs discrete
    pci_slot: str               # "0000:01:00.0"
    encoders: list[EncoderInfo] = field(default_factory=list)
    vram_mb: int = 0            # 0 for iGPU (shared memory)
    render_device: str = ""     # "/dev/dri/renderD128"

    def to_dict(self) -> dict[str, Any]:
        return {
            "index": self.index,
            "name": self.name,
            "vendor": self.vendor,
            "is_igpu": self.is_igpu,
            "pci_slot": self.pci_slot,
            "vram_mb": self.vram_mb,
            "render_device": self.render_device,
            "encoders": [e.to_dict() for e in self.encoders],
        }


class GPUInventory:
    """
    Detects all GPUs and their encoding capabilities.

    Call ``discover()`` once at startup; the results are cached.
    """

    def __init__(self) -> None:
        self._gpus: list[GPUInfo] = []
        self._software_encoders: list[EncoderInfo] = []
        self._discovered = False

    @property
    def gpus(self) -> list[GPUInfo]:
        return list(self._gpus)

    @property
    def all_encoders(self) -> list[EncoderInfo]:
        """All encoders across all GPUs plus software fallbacks."""
        result = []
        for gpu in self._gpus:
            result.extend(gpu.encoders)
        result.extend(self._software_encoders)
        return result

    @property
    def software_encoders(self) -> list[EncoderInfo]:
        return list(self._software_encoders)

    def gpu_by_index(self, index: int) -> GPUInfo | None:
        for gpu in self._gpus:
            if gpu.index == index:
                return gpu
        return None

    async def discover(self) -> list[GPUInfo]:
        """
        Detect all GPUs and probe their encoder capabilities.

        Safe to call multiple times — results are cached after first run.
        """
        if self._discovered:
            return self._gpus

        system = platform.system()
        if system == "Linux":
            await self._discover_linux()
        elif system == "Windows":
            await self._discover_windows()
        else:
            log.info("GPU discovery not supported on %s", system)

        # Always probe software encoders as fallback
        await self._probe_software_encoders()

        self._discovered = True

        total = sum(len(g.encoders) for g in self._gpus) + len(self._software_encoders)
        log.info("GPU inventory: %d GPUs, %d hardware encoders, %d software encoders",
                 len(self._gpus), total - len(self._software_encoders),
                 len(self._software_encoders))
        for gpu in self._gpus:
            enc_names = ", ".join(e.name for e in gpu.encoders) or "none"
            log.info("  [%d] %s (%s%s) — vram=%dMB render=%s encoders=[%s]",
                     gpu.index, gpu.name, gpu.vendor,
                     " iGPU" if gpu.is_igpu else "",
                     gpu.vram_mb, gpu.render_device, enc_names)

        return self._gpus

    # ── Linux GPU discovery ──────────────────────────────────────────────────

    async def _discover_linux(self) -> None:
        """Enumerate GPUs from /sys/class/drm/card*."""
        drm_dir = Path("/sys/class/drm")
        if not drm_dir.exists():
            log.warning("No /sys/class/drm — GPU discovery unavailable")
            return

        # Find card directories (card0, card1, ...) — skip card0-DP-1 etc.
        card_dirs = sorted(
            d for d in drm_dir.iterdir()
            if d.name.startswith("card") and d.name[4:].isdigit()
        )

        # Map card index to render device
        render_map = self._build_render_device_map()

        # Get NVIDIA GPU details if nvidia-smi is available
        nvidia_info = await self._probe_nvidia_smi()

        for card_dir in card_dirs:
            card_index = int(card_dir.name[4:])
            device_dir = card_dir / "device"

            # Read vendor ID
            vendor_path = device_dir / "vendor"
            if not vendor_path.exists():
                continue
            vendor_id = vendor_path.read_text().strip()
            vendor = _VENDOR_NAMES.get(vendor_id, "unknown")
            if vendor == "unknown":
                continue

            # Read PCI slot
            pci_slot = ""
            try:
                pci_slot = device_dir.resolve().name  # e.g. "0000:01:00.0"
            except Exception:
                pass

            # Determine iGPU — boot_vga=1 for the primary display adapter,
            # which for Intel is almost always the iGPU. Also check class.
            is_igpu = False
            boot_vga_path = device_dir / "boot_vga"
            if boot_vga_path.exists():
                boot_vga = boot_vga_path.read_text().strip()
                if vendor == "intel" and boot_vga == "1":
                    is_igpu = True
            # Intel GPUs without a discrete GPU partner are always iGPUs
            if vendor == "intel":
                is_igpu = True  # Intel discrete (Arc) handled below

            # Check for Intel Arc (discrete) — has dedicated VRAM
            if vendor == "intel":
                mem_path = device_dir / "resource"
                if mem_path.exists():
                    try:
                        # Intel Arc GPUs have substantial VRAM (>= 4GB)
                        # reported in lspci. For sysfs, check class code.
                        class_path = device_dir / "class"
                        if class_path.exists():
                            class_code = class_path.read_text().strip()
                            # VGA compatible controller = 0x030000
                            # 3D controller = 0x030200 (used by some Arc models)
                            pass  # Still default to iGPU; Arc detection below
                    except Exception:
                        pass

            # GPU name — get from nvidia-smi or construct from vendor
            name = f"{vendor.upper()} GPU {card_index}"
            vram_mb = 0

            if vendor == "nvidia" and nvidia_info:
                for nv in nvidia_info:
                    if nv.get("index") == card_index or nv.get("pci_slot") == pci_slot:
                        name = nv.get("name", name)
                        vram_mb = nv.get("vram_mb", 0)
                        is_igpu = False  # NVIDIA GPUs are always discrete
                        break
            elif vendor == "amd":
                is_igpu = self._check_amd_igpu(device_dir)
                name = self._read_amd_name(device_dir, card_index)
            elif vendor == "intel":
                name = self._read_intel_name(device_dir, card_index)
                # Check if this is Intel Arc (discrete)
                if "arc" in name.lower():
                    is_igpu = False
                    # Intel Arc has dedicated VRAM
                    vram_mb = self._read_intel_vram(device_dir)

            render_dev = render_map.get(card_index, f"/dev/dri/renderD{128 + card_index}")

            gpu = GPUInfo(
                index=card_index,
                name=name,
                vendor=vendor,
                is_igpu=is_igpu,
                pci_slot=pci_slot,
                vram_mb=vram_mb,
                render_device=render_dev,
            )
            self._gpus.append(gpu)

        # Probe encoders for each GPU
        await self._probe_gpu_encoders()

    def _build_render_device_map(self) -> dict[int, str]:
        """Map card indices to /dev/dri/renderDNNN devices."""
        result: dict[int, str] = {}
        dri_path = Path("/dev/dri")
        if not dri_path.exists():
            return result
        for dev in sorted(dri_path.iterdir()):
            if dev.name.startswith("renderD"):
                try:
                    render_num = int(dev.name[7:])
                    card_index = render_num - 128
                    result[card_index] = str(dev)
                except ValueError:
                    pass
        return result

    async def _probe_nvidia_smi(self) -> list[dict]:
        """Query nvidia-smi for GPU names, VRAM, and PCI slots."""
        if not shutil.which("nvidia-smi"):
            return []
        try:
            proc = await asyncio.create_subprocess_exec(
                "nvidia-smi",
                "--query-gpu=index,name,memory.total,pci.bus_id",
                "--format=csv,noheader,nounits",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10)
            if proc.returncode != 0:
                return []

            results = []
            for line in stdout.decode().strip().splitlines():
                parts = [p.strip() for p in line.split(",")]
                if len(parts) >= 4:
                    # nvidia-smi index may differ from DRM card index.
                    # Use PCI slot for matching.
                    pci_raw = parts[3].lower()
                    # nvidia-smi outputs like "00000000:01:00.0", normalise
                    pci_slot = pci_raw.split(":")[-3] + ":" if ":" in pci_raw else ""
                    pci_slot = pci_raw  # keep full for matching
                    results.append({
                        "index": int(parts[0]),
                        "name": parts[1],
                        "vram_mb": int(float(parts[2])),
                        "pci_slot": pci_slot,
                    })
            return results
        except Exception as e:
            log.debug("nvidia-smi query failed: %s", e)
            return []

    def _check_amd_igpu(self, device_dir: Path) -> bool:
        """Check if an AMD GPU is integrated (APU) vs discrete."""
        # AMD APUs (Ryzen with Radeon Graphics) have boot_vga=1
        # and no dedicated VRAM bar in sysfs
        boot_vga = device_dir / "boot_vga"
        if boot_vga.exists() and boot_vga.read_text().strip() == "1":
            # Could be iGPU — check if there are other AMD GPUs (which would
            # make this the iGPU and the other the dGPU)
            return True
        return False

    def _read_amd_name(self, device_dir: Path, index: int) -> str:
        """Read AMD GPU marketing name from sysfs or fallback."""
        # Try reading from DRM via product name
        product_path = device_dir / "product_name"
        if product_path.exists():
            return product_path.read_text().strip()
        # Fallback: use lspci-style device ID lookup
        device_id_path = device_dir / "device"
        if device_id_path.exists():
            return f"AMD GPU [{device_id_path.read_text().strip()}]"
        return f"AMD GPU {index}"

    def _read_intel_name(self, device_dir: Path, index: int) -> str:
        """Read Intel GPU name."""
        # Try i915 or xe driver label
        label_path = device_dir / "label"
        if label_path.exists():
            return label_path.read_text().strip()
        device_id_path = device_dir / "device"
        if device_id_path.exists():
            did = device_id_path.read_text().strip()
            # Well-known Intel iGPU device IDs
            _KNOWN_INTEL = {
                "0x4680": "Intel UHD 770",
                "0x46a6": "Intel UHD 770",
                "0xa780": "Intel UHD 770",
                "0xa788": "Intel UHD 730",
                "0x56a0": "Intel Arc A770",
                "0x56a1": "Intel Arc A750",
                "0x5690": "Intel Arc A770M",
            }
            return _KNOWN_INTEL.get(did, f"Intel GPU [{did}]")
        return f"Intel GPU {index}"

    def _read_intel_vram(self, device_dir: Path) -> int:
        """Read dedicated VRAM for Intel Arc GPUs."""
        # Intel Arc reports VRAM via DRM memory regions
        # This is a best-effort read
        try:
            mem_info = device_dir / "mem_info_vram_total"
            if mem_info.exists():
                return int(mem_info.read_text().strip()) // (1024 * 1024)
        except Exception:
            pass
        return 0

    async def _probe_gpu_encoders(self) -> None:
        """Probe which hardware encoders work on each GPU."""
        for gpu in self._gpus:
            if gpu.vendor == "nvidia":
                await self._probe_nvenc(gpu)
            elif gpu.vendor == "intel":
                await self._probe_intel_encoders(gpu)
            elif gpu.vendor == "amd":
                await self._probe_amd_encoders(gpu)

    async def _probe_nvenc(self, gpu: GPUInfo) -> None:
        """Probe NVENC encoders on an NVIDIA GPU."""
        max_sessions = self._nvenc_session_limit(gpu.name)

        for codec, encoder_name in [("h264", "h264_nvenc"), ("h265", "hevc_nvenc")]:
            if await self._test_encoder(encoder_name, gpu_index=gpu.index):
                gpu.encoders.append(EncoderInfo(
                    name=encoder_name,
                    codec=codec,
                    gpu_index=gpu.index,
                    max_sessions=max_sessions,
                    quality=8,
                    latency=2,
                ))

    def _nvenc_session_limit(self, gpu_name: str) -> int:
        """Look up NVENC concurrent session limit for a GPU."""
        name_lower = gpu_name.lower()
        for pattern, limit in _NVENC_SESSION_LIMITS.items():
            if pattern in name_lower:
                return limit
        # Unknown NVIDIA GPU — assume consumer limit
        return 5

    async def _probe_intel_encoders(self, gpu: GPUInfo) -> None:
        """Probe QSV and VAAPI encoders on an Intel GPU."""
        render_dev = gpu.render_device

        # Quick Sync (QSV)
        for codec, encoder_name in [("h264", "h264_qsv"), ("h265", "hevc_qsv")]:
            if await self._test_encoder(encoder_name):
                gpu.encoders.append(EncoderInfo(
                    name=encoder_name,
                    codec=codec,
                    gpu_index=gpu.index,
                    max_sessions=-1,  # QSV has no hard session limit
                    quality=7,
                    latency=3,
                ))

        # VAAPI
        for codec, encoder_name in [("h264", "h264_vaapi"), ("h265", "hevc_vaapi")]:
            if await self._test_vaapi_encoder(encoder_name, render_dev):
                gpu.encoders.append(EncoderInfo(
                    name=encoder_name,
                    codec=codec,
                    gpu_index=gpu.index,
                    max_sessions=-1,
                    quality=6,
                    latency=4,
                ))

    async def _probe_amd_encoders(self, gpu: GPUInfo) -> None:
        """Probe AMF and VAAPI encoders on an AMD GPU."""
        render_dev = gpu.render_device

        # AMF (proprietary AMD encoder)
        for codec, encoder_name in [("h264", "h264_amf"), ("h265", "hevc_amf")]:
            if await self._test_encoder(encoder_name):
                gpu.encoders.append(EncoderInfo(
                    name=encoder_name,
                    codec=codec,
                    gpu_index=gpu.index,
                    max_sessions=-1,
                    quality=7,
                    latency=3,
                ))

        # VAAPI (Mesa open-source encoder)
        for codec, encoder_name in [("h264", "h264_vaapi"), ("h265", "hevc_vaapi")]:
            if await self._test_vaapi_encoder(encoder_name, render_dev):
                gpu.encoders.append(EncoderInfo(
                    name=encoder_name,
                    codec=codec,
                    gpu_index=gpu.index,
                    max_sessions=-1,
                    quality=6,
                    latency=4,
                ))

    async def _test_encoder(self, encoder: str, gpu_index: int | None = None) -> bool:
        """Test if an ffmpeg encoder works by encoding a tiny frame."""
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
            "-frames:v", "1",
        ]
        if gpu_index is not None and "nvenc" in encoder:
            cmd.extend(["-gpu", str(gpu_index)])
        cmd.extend(["-c:v", encoder, "-f", "null", "-"])

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await asyncio.wait_for(proc.wait(), timeout=10)
            return proc.returncode == 0
        except Exception:
            return False

    async def _test_vaapi_encoder(self, encoder: str, device: str) -> bool:
        """Test a VAAPI encoder with the correct render device."""
        if not device or not Path(device).exists():
            return False

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-init_hw_device", f"vaapi=hw:{device}",
            "-filter_hw_device", "hw",
            "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
            "-vf", "format=nv12,hwupload",
            "-frames:v", "1",
            "-c:v", encoder, "-f", "null", "-",
        ]
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await asyncio.wait_for(proc.wait(), timeout=10)
            return proc.returncode == 0
        except Exception:
            return False

    async def _probe_software_encoders(self) -> None:
        """Probe software encoders (always available as fallback)."""
        for codec, encoder_name in [("h264", "libx264"), ("h265", "libx265")]:
            if await self._test_encoder(encoder_name):
                self._software_encoders.append(EncoderInfo(
                    name=encoder_name,
                    codec=codec,
                    gpu_index=-1,
                    max_sessions=-1,
                    quality=9 if codec == "h264" else 9,
                    latency=8,  # software is high latency
                ))

    # ── Windows GPU discovery ───────────────────────────────────────────────

    async def _discover_windows(self) -> None:
        """Discover GPUs on Windows via DXGI adapter enumeration."""
        if platform.system() != "Windows":
            log.info("Windows GPU discovery called on non-Windows — using stub")
            self._gpus.append(GPUInfo(
                index=0, name="Default GPU", vendor="unknown",
                is_igpu=False, pci_slot="", vram_mb=0,
            ))
            return

        try:
            self._enumerate_windows_dxgi()
        except Exception as e:
            log.warning("DXGI GPU discovery failed: %s — trying nvidia-smi fallback", e)
            self._discover_windows_fallback()

        # Probe encoders (async — uses ffmpeg subprocess calls)
        print(f"[OZMA DEBUG] GPU discovery done, {len(self._gpus)} GPUs. Probing encoders...", flush=True)
        try:
            await self._probe_windows_encoders()
            print(f"[OZMA DEBUG] Encoder probing complete", flush=True)
        except Exception as e:
            print(f"[OZMA DEBUG] Encoder probing failed: {e}", flush=True)
            log.warning("Encoder probing failed: %s", e)

    def _enumerate_windows_dxgi(self) -> None:
        """Enumerate GPUs via DXGI COM (IDXGIFactory1::EnumAdapters1). Synchronous."""
        import ctypes
        from ctypes import HRESULT, POINTER, Structure, byref, c_uint, c_void_p, c_wchar, windll

        class DXGI_ADAPTER_DESC1(Structure):
            _fields_ = [
                ("Description", c_wchar * 128),
                ("VendorId", c_uint),
                ("DeviceId", c_uint),
                ("SubSysId", c_uint),
                ("Revision", c_uint),
                ("DedicatedVideoMemory", ctypes.c_size_t),
                ("DedicatedSystemMemory", ctypes.c_size_t),
                ("SharedSystemMemory", ctypes.c_size_t),
                ("AdapterLuid", ctypes.c_byte * 8),
            ]

        IID = ctypes.c_byte * 16
        import uuid
        iid_factory1 = IID(*uuid.UUID("770aae78-f26f-4dba-a829-253c83d1b387").bytes_le)

        ole32 = windll.ole32
        hr = ole32.CoInitializeEx(None, 0)
        needs_uninit = hr >= 0

        factory = c_void_p()
        try:
            dxgi = windll.dxgi
            hr = dxgi.CreateDXGIFactory1(byref(iid_factory1), byref(factory))
            if hr < 0 or not factory:
                raise RuntimeError(f"CreateDXGIFactory1 failed: 0x{hr & 0xFFFFFFFF:08x}")

            def _vtable_call(iface, idx, argtypes, *args):
                vtable = ctypes.cast(iface, POINTER(c_void_p))[0]
                fn_ptr = ctypes.cast(vtable, POINTER(c_void_p))[idx]
                fn = ctypes.CFUNCTYPE(HRESULT, c_void_p, *argtypes)(fn_ptr)
                return fn(iface, *args)

            adapter_idx = 0
            while True:
                adapter = c_void_p()
                hr = _vtable_call(factory, 12, [c_uint, POINTER(c_void_p)],
                                  c_uint(adapter_idx), byref(adapter))
                if hr < 0 or not adapter:
                    break

                try:
                    desc = DXGI_ADAPTER_DESC1()
                    _vtable_call(adapter, 10, [POINTER(DXGI_ADAPTER_DESC1)], byref(desc))

                    name = desc.Description.rstrip("\x00")
                    vendor_id = f"0x{desc.VendorId:04x}"
                    vendor = _VENDOR_NAMES.get(vendor_id, "unknown")
                    vram_mb = desc.DedicatedVideoMemory // (1024 * 1024)

                    # Skip Microsoft Basic Render Driver and similar
                    if "Microsoft" in name and "Basic" in name:
                        adapter_idx += 1
                        continue

                    is_igpu = (vendor == "intel" and vram_mb < 512)
                    # Intel Arc has >= 4GB dedicated
                    if vendor == "intel" and vram_mb >= 4096:
                        is_igpu = False

                    gpu = GPUInfo(
                        index=adapter_idx,
                        name=name,
                        vendor=vendor,
                        is_igpu=is_igpu,
                        pci_slot="",
                        vram_mb=vram_mb,
                    )
                    self._gpus.append(gpu)
                    log.info("DXGI adapter %d: %s (%s, %dMB VRAM)",
                             adapter_idx, name, vendor, vram_mb)

                finally:
                    _vtable_call(adapter, 2, [])  # Release

                adapter_idx += 1

        finally:
            if factory:
                def _release(iface):
                    vtable = ctypes.cast(iface, POINTER(c_void_p))[0]
                    fn_ptr = ctypes.cast(vtable, POINTER(c_void_p))[2]
                    fn = ctypes.CFUNCTYPE(HRESULT, c_void_p)(fn_ptr)
                    fn(iface)
                _release(factory)
            if needs_uninit:
                ole32.CoUninitialize()

        if not self._gpus:
            raise RuntimeError("DXGI found no suitable adapters")

    async def _probe_windows_encoders(self) -> None:
        """Probe hardware encoders on Windows GPUs."""
        if not shutil.which("ffmpeg"):
            log.warning("ffmpeg not found — skipping encoder probing. "
                        "Install ffmpeg for hardware encoding support.")
            # Add assumed NVENC for NVIDIA GPUs (very likely to work)
            for gpu in self._gpus:
                if gpu.vendor == "nvidia":
                    limit = self._nvenc_session_limit(gpu.name)
                    gpu.encoders.append(EncoderInfo(
                        name="h264_nvenc", codec="h264", gpu_index=gpu.index,
                        max_sessions=limit, quality=8, latency=2,
                    ))
            return

        for gpu in self._gpus:
            if gpu.vendor == "nvidia":
                await self._probe_nvenc(gpu)
            elif gpu.vendor == "intel":
                # QSV available on Windows via ffmpeg
                for codec, enc_name in [("h264", "h264_qsv"), ("h265", "hevc_qsv")]:
                    if await self._test_encoder(enc_name):
                        gpu.encoders.append(EncoderInfo(
                            name=enc_name, codec=codec, gpu_index=gpu.index,
                            max_sessions=-1, quality=7, latency=3,
                        ))
            elif gpu.vendor == "amd":
                for codec, enc_name in [("h264", "h264_amf"), ("h265", "hevc_amf")]:
                    if await self._test_encoder(enc_name):
                        gpu.encoders.append(EncoderInfo(
                            name=enc_name, codec=codec, gpu_index=gpu.index,
                            max_sessions=-1, quality=7, latency=3,
                        ))

    def _discover_windows_fallback(self) -> None:
        """Fallback: detect GPUs via nvidia-smi or create a generic entry."""
        # Try nvidia-smi (works on Windows too)
        if shutil.which("nvidia-smi"):
            try:
                result = subprocess.run(
                    ["nvidia-smi",
                     "--query-gpu=index,name,memory.total",
                     "--format=csv,noheader,nounits"],
                    capture_output=True, text=True, timeout=10,
                )
                if result.returncode == 0:
                    for line in result.stdout.strip().splitlines():
                        parts = [p.strip() for p in line.split(",")]
                        if len(parts) >= 3:
                            idx = int(parts[0])
                            self._gpus.append(GPUInfo(
                                index=idx,
                                name=parts[1],
                                vendor="nvidia",
                                is_igpu=False,
                                pci_slot="",
                                vram_mb=int(float(parts[2])),
                            ))
                    if self._gpus:
                        log.info("nvidia-smi fallback: found %d GPUs", len(self._gpus))
                        return
            except Exception as e:
                log.debug("nvidia-smi fallback failed: %s", e)

        # Last resort: single generic GPU entry
        log.info("Windows GPU discovery: using generic entry (probe encoders via ffmpeg)")
        self._gpus.append(GPUInfo(
            index=0, name="Default GPU", vendor="unknown",
            is_igpu=False, pci_slot="", vram_mb=0,
        ))

    def to_dict(self) -> dict[str, Any]:
        return {
            "gpus": [g.to_dict() for g in self._gpus],
            "software_encoders": [e.to_dict() for e in self._software_encoders],
        }
