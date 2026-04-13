#!/usr/bin/env python3
"""
Hardware Detection for OzmaOS First-Boot Wizard

Detects:
- ZFS-capable disks
- GPU (NVIDIA, AMD, Intel)
- Hailo NPU
"""

import json
import subprocess
import re
from pathlib import Path
from typing import Any


def run_command(cmd: list[str], timeout: int = 10) -> str:
    """Run a command and return stdout."""
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return result.stdout + result.stderr
    except Exception:
        return ""


def detect_disks() -> list[dict[str, Any]]:
    """Detect available disks and their characteristics."""
    disks = []
    
    # Get all block devices
    try:
        result = subprocess.run(
            ["lsblk", "-d", "-J", "-o", "NAME,SIZE,TYPE,MODEL,ROTA,TRAN"],
            capture_output=True, text=True, timeout=10
        )
        data = json.loads(result.stdout)
        
        # Get root disk
        root_disk = None
        try:
            result = subprocess.run(
                ["findmnt", "-n", "-o", "SOURCE", "/"],
                capture_output=True, text=True, timeout=5
            )
            root_dev = result.stdout.strip()
            # Extract just the device name (e.g., /dev/sda)
            m = re.match(r'/dev/([a-z]+)', root_dev)
            if m:
                root_disk = m.group(1)
        except:
            pass
        
        for device in data.get("blockdevices", []):
            name = device.get("name", "")
            dev_type = device.get("type", "")
            
            # Only include physical disks
            if dev_type != "disk":
                continue
            
            # Skip loop devices
            if name.startswith("loop"):
                continue
            
            device_path = f"/dev/{name}"
            
            # Check if this is the root disk
            is_root = (name == root_disk)
            
            # Check if it's a rotational (HDD) or SSD
            is_ssd = device.get("rota", "1") == "0"
            
            # Try to get more info
            size = device.get("size", "unknown")
            model = device.get("model", "")
            transport = device.get("tran", "")
            
            # Check if disk is extra (not root)
            is_extra = not is_root
            
            disk_info = {
                "device": device_path,
                "name": name,
                "size": size,
                "model": model or "Unknown",
                "is_ssd": is_ssd,
                "is_root": is_root,
                "is_extra": is_extra,
                "transport": transport,
            }
            
            # Only add extra disks or important info
            disks.append(disk_info)
            
    except Exception as e:
        print(f"Error detecting disks: {e}", flush=True)
    
    return disks


def detect_gpu() -> dict[str, Any]:
    """Detect GPU hardware."""
    gpu = {
        "nvidia": {"available": False, "model": None},
        "amd": {"available": False, "model": None},
        "intel": {"available": False, "model": None},
    }
    
    # Check NVIDIA
    try:
        result = subprocess.run(
            ["nvidia-smi", "-L"],
            capture_output=True, text=True, timeout=5
        )
        if result.returncode == 0:
            # Parse model from output like "GPU 0: NVIDIA GeForce RTX 3080"
            match = re.search(r'NVIDIA\s+(.+)', result.stdout)
            gpu["nvidia"]["available"] = True
            gpu["nvidia"]["model"] = match.group(1).strip() if match else "NVIDIA GPU"
    except:
        pass
    
    # Check AMD
    try:
        result = subprocess.run(
            ["lspci"],
            capture_output=True, text=True, timeout=5
        )
        if "VGA" in result.stdout:
            for line in result.stdout.split("\n"):
                if "VGA" in line and "AMD" in line:
                    gpu["amd"]["available"] = True
                    match = re.search(r'AMD\s+(.+?)(?:\(|\[|$)', line)
                    gpu["amd"]["model"] = match.group(1).strip() if match else "AMD GPU"
                    break
    except:
        pass
    
    # Check Intel
    try:
        result = subprocess.run(
            ["lspci"],
            capture_output=True, text=True, timeout=5
        )
        if "VGA" in result.stdout:
            for line in result.stdout.split("\n"):
                if "VGA" in line and "Intel" in line:
                    gpu["intel"]["available"] = True
                    match = re.search(r'Intel\s+(.+?)(?:\(|\[|$)', line)
                    gpu["intel"]["model"] = match.group(1).strip() if match else "Intel GPU"
                    break
    except:
        pass
    
    return gpu


def detect_hailo() -> dict[str, Any]:
    """Detect Hailo NPU."""
    hailo = {"available": False, "model": None}
    
    # Check for Hailo device
    try:
        result = subprocess.run(
            ["lsusb"],
            capture_output=True, text=True, timeout=5
        )
        for line in result.stdout.split("\n"):
            if "Hailo" in line:
                hailo["available"] = True
                match = re.search(r'Hailo Technologies\s+(.+)', line)
                hailo["model"] = match.group(1).strip() if match else "Hailo NPU"
                break
    except:
        pass
    
    # Also check /dev
    if Path("/dev/hailo0").exists():
        hailo["available"] = True
        hailo["model"] = "Hailo NPU"
    
    return hailo


def detect_network() -> list[dict[str, Any]]:
    """Detect network interfaces."""
    interfaces = []
    
    try:
        result = subprocess.run(
            ["ip", "-j", "link", "show"],
            capture_output=True, text=True, timeout=5
        )
        data = json.loads(result.stdout)
        
        for iface in data:
            name = iface.get("ifname", "")
            if name in ("lo",):
                continue
            
            info = {
                "name": name,
                "mac": iface.get("address", ""),
                "up": iface.get("operstate") == "UP",
            }
            
            # Get driver info
            try:
                driver_path = Path(f"/sys/class/net/{name}/device/driver")
                if driver_path.exists():
                    info["driver"] = driver_path.resolve().name
            except:
                pass
            
            interfaces.append(info)
    except:
        pass
    
    return interfaces


def detect_memory() -> dict[str, Any]:
    """Detect memory information."""
    mem = {"total_gb": 0, "swap_gb": 0}
    
    try:
        with open("/proc/meminfo", "r") as f:
            for line in f:
                if line.startswith("MemTotal:"):
                    mem["total_gb"] = int(line.split()[1]) // 1024 // 1024
                elif line.startswith("SwapTotal:"):
                    mem["swap_gb"] = int(line.split()[1]) // 1024 // 1024
    except:
        pass
    
    return mem


def detect() -> dict[str, Any]:
    """Run all hardware detection and return combined results."""
    print("Detecting hardware...", flush=True)
    
    hw = {
        "disks": detect_disks(),
        "gpu": detect_gpu(),
        "hailo": detect_hailo(),
        "network": detect_network(),
        "memory": detect_memory(),
    }
    
    # Count available GPUs
    hw["gpu_count"] = sum(1 for g in hw["gpu"].values() if g["available"])
    
    # Count extra disks
    hw["extra_disk_count"] = sum(1 for d in hw["disks"] if d["is_extra"])
    
    print(f"  Detected {len(hw['disks'])} disk(s), {hw['gpu_count']} GPU(s), "
          f"{hw['extra_disk_count']} extra disk(s)", flush=True)
    
    return hw


if __name__ == "__main__":
    import sys
    result = detect()
    print(json.dumps(result, indent=2))
