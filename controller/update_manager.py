# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
A/B partition update manager — signed updates with automatic rollback.

Manages the update lifecycle for bare metal controller appliances:
  1. Check for updates (poll update server or detect USB)
  2. Download and verify (SHA256 + Ed25519 signature)
  3. Write to inactive root partition
  4. Swap boot configuration
  5. Reboot
  6. Post-boot health check → confirm or rollback

The update manager runs as part of the controller and also as a
standalone health-check script called from the init system.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import os
import shutil
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.update")

BOOT_CONFIG_PATH = Path("/data/config/ozma-boot.json")
SYSLINUX_CFG = Path("/boot/syslinux/syslinux.cfg")
UPDATE_SERVER = "https://updates.ozma.dev/api/v1"

# Import Ed25519 verification from transport
try:
    from transport import IdentityKeyPair
    _HAS_CRYPTO = True
except ImportError:
    _HAS_CRYPTO = False


@dataclass
class SlotInfo:
    version: str = ""
    image_hash: str = ""
    installed_at: str = ""
    boot_count_since_update: int = 0
    healthy: bool = False
    pending_validation: bool = False

    def to_dict(self) -> dict:
        return {
            "version": self.version,
            "image_hash": self.image_hash,
            "installed_at": self.installed_at,
            "boot_count_since_update": self.boot_count_since_update,
            "healthy": self.healthy,
            "pending_validation": self.pending_validation,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "SlotInfo":
        return cls(**{k: v for k, v in d.items() if k in cls.__dataclass_fields__})


@dataclass
class BootConfig:
    active_slot: str = "a"
    slot_a: SlotInfo | None = None
    slot_b: SlotInfo | None = None
    max_boot_attempts: int = 3
    update_channel: str = "stable"

    def to_dict(self) -> dict:
        return {
            "active_slot": self.active_slot,
            "slot_a": self.slot_a.to_dict() if self.slot_a else {},
            "slot_b": self.slot_b.to_dict() if self.slot_b else {},
            "max_boot_attempts": self.max_boot_attempts,
            "update_channel": self.update_channel,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "BootConfig":
        return cls(
            active_slot=d.get("active_slot", "a"),
            slot_a=SlotInfo.from_dict(d.get("slot_a", {})),
            slot_b=SlotInfo.from_dict(d.get("slot_b", {})),
            max_boot_attempts=d.get("max_boot_attempts", 3),
            update_channel=d.get("update_channel", "stable"),
        )

    @property
    def active(self) -> SlotInfo:
        return self.slot_a if self.active_slot == "a" else self.slot_b

    @property
    def inactive(self) -> SlotInfo:
        return self.slot_b if self.active_slot == "a" else self.slot_a

    @property
    def inactive_slot_name(self) -> str:
        return "b" if self.active_slot == "a" else "a"

    @property
    def inactive_device(self) -> str:
        """Device path for the inactive root partition."""
        # Detect by label
        label = f"ozma-root-{self.inactive_slot_name}"
        result = subprocess.run(
            ["blkid", "-L", label], capture_output=True, text=True,
        )
        if result.returncode == 0:
            return result.stdout.strip()
        # Fallback: assume standard layout
        return f"/dev/sda{'3' if self.inactive_slot_name == 'b' else '2'}"


def load_boot_config() -> BootConfig:
    if BOOT_CONFIG_PATH.exists():
        return BootConfig.from_dict(json.loads(BOOT_CONFIG_PATH.read_text()))
    return BootConfig(slot_a=SlotInfo(healthy=True), slot_b=SlotInfo())


def save_boot_config(config: BootConfig) -> None:
    BOOT_CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)
    BOOT_CONFIG_PATH.write_text(json.dumps(config.to_dict(), indent=2))


class UpdateManager:
    """Manages A/B updates for the controller appliance."""

    def __init__(self, firmware_ca_pubkey: bytes | None = None) -> None:
        self._config = load_boot_config()
        self._ca_pubkey = firmware_ca_pubkey
        self._checking = False
        self._applying = False

    @property
    def config(self) -> BootConfig:
        return self._config

    # ── Check for updates ───────────────────────────────────────────────────

    async def check_for_update(self) -> dict | None:
        """Check the update server for a new version."""
        import urllib.request

        current = self._config.active.version
        channel = self._config.update_channel

        try:
            loop = asyncio.get_running_loop()
            def _check():
                url = f"{UPDATE_SERVER}/updates/check"
                data = json.dumps({
                    "current_version": current,
                    "current_slot": self._config.active_slot,
                    "channel": channel,
                    "hw_type": "x86_64",
                }).encode()
                req = urllib.request.Request(
                    url, data=data,
                    headers={"Content-Type": "application/json"},
                )
                with urllib.request.urlopen(req, timeout=10) as r:
                    return json.loads(r.read())
            result = await loop.run_in_executor(None, _check)
            if result and result.get("available"):
                return result
        except Exception as e:
            log.debug("Update check failed: %s", e)
        return None

    # ── Apply update ────────────────────────────────────────────────────────

    async def apply_update(self, update_info: dict) -> bool:
        """Download, verify, and write an update to the inactive slot."""
        if self._applying:
            return False
        self._applying = True

        try:
            url = update_info.get("url", "")
            expected_hash = update_info.get("sha256", "")
            signature_b64 = update_info.get("signature", "")
            new_version = update_info.get("version", "")

            if not url or not expected_hash:
                log.error("Update missing URL or hash")
                return False

            # Download to temp file
            log.info("Downloading update %s...", new_version)
            import urllib.request
            tmp_path = Path("/tmp/ozma-update.img")
            loop = asyncio.get_running_loop()
            def _download():
                urllib.request.urlretrieve(url, str(tmp_path))
            await loop.run_in_executor(None, _download)

            # Verify SHA256
            file_hash = hashlib.sha256(tmp_path.read_bytes()).hexdigest()
            if file_hash != expected_hash:
                log.error("Hash mismatch: expected %s, got %s", expected_hash, file_hash)
                tmp_path.unlink()
                return False
            log.info("SHA256 verified")

            # Verify Ed25519 signature
            if signature_b64 and self._ca_pubkey and _HAS_CRYPTO:
                import base64
                sig = base64.b64decode(signature_b64)
                if not IdentityKeyPair.verify(tmp_path.read_bytes(), sig, self._ca_pubkey):
                    log.error("Signature verification FAILED — update rejected")
                    tmp_path.unlink()
                    return False
                log.info("Ed25519 signature verified")

            # Write to inactive partition
            device = self._config.inactive_device
            log.info("Writing update to %s (inactive slot %s)...",
                     device, self._config.inactive_slot_name)

            def _write():
                subprocess.run(
                    ["dd", f"if={tmp_path}", f"of={device}", "bs=4M", "conv=fsync"],
                    check=True, capture_output=True,
                )
            await loop.run_in_executor(None, _write)
            tmp_path.unlink()

            # Update boot config
            inactive = self._config.inactive
            inactive.version = new_version
            inactive.image_hash = f"sha256:{file_hash}"
            inactive.installed_at = time.strftime("%Y-%m-%dT%H:%M:%S")
            inactive.boot_count_since_update = 0
            inactive.healthy = False
            inactive.pending_validation = True

            # Swap active slot
            old_slot = self._config.active_slot
            self._config.active_slot = self._config.inactive_slot_name

            # Update syslinux DEFAULT
            self._update_syslinux(self._config.active_slot)

            save_boot_config(self._config)
            log.info("Update applied: %s → slot %s. Reboot to activate.",
                     new_version, self._config.active_slot)

            return True

        except Exception as e:
            log.error("Update failed: %s", e)
            return False
        finally:
            self._applying = False

    # ── Health check ────────────────────────────────────────────────────────

    async def health_check(self) -> bool:
        """
        Post-boot health check. Called after booting into a new slot.

        If the active slot is pending validation:
          - Increment boot count
          - Wait for the controller API to respond
          - If healthy: mark slot as confirmed
          - If unhealthy after max attempts: rollback

        Returns True if healthy, False if rolled back.
        """
        active = self._config.active
        if not active.pending_validation:
            return True  # Not a new update, nothing to validate

        active.boot_count_since_update += 1
        save_boot_config(self._config)

        if active.boot_count_since_update > self._config.max_boot_attempts:
            log.error("Max boot attempts exceeded — rolling back")
            return await self.rollback()

        # Wait for the controller to come up
        log.info("Health check: waiting for controller API (attempt %d/%d)...",
                 active.boot_count_since_update, self._config.max_boot_attempts)

        import urllib.request
        for _ in range(60):  # 60 seconds max
            try:
                with urllib.request.urlopen("http://localhost:7380/api/v1/status", timeout=2) as r:
                    data = json.loads(r.read())
                    if data.get("nodes") is not None:
                        # Controller is up and responding
                        active.healthy = True
                        active.pending_validation = False
                        save_boot_config(self._config)
                        log.info("Health check PASSED — slot %s confirmed (v%s)",
                                 self._config.active_slot, active.version)
                        return True
            except Exception:
                pass
            await asyncio.sleep(1)

        log.error("Health check FAILED — controller did not respond in 60s")
        active.boot_count_since_update += 1
        save_boot_config(self._config)

        if active.boot_count_since_update >= self._config.max_boot_attempts:
            return await self.rollback()

        return False

    # ── Rollback ────────────────────────────────────────────────────────────

    async def rollback(self) -> bool:
        """Rollback to the previous slot."""
        current = self._config.active_slot
        previous = "a" if current == "b" else "b"
        previous_slot = self._config.slot_a if previous == "a" else self._config.slot_b

        if not previous_slot or not previous_slot.healthy:
            log.error("Cannot rollback — previous slot is not healthy")
            return False

        log.info("Rolling back: slot %s → slot %s (v%s)",
                 current, previous, previous_slot.version)

        self._config.active_slot = previous
        active = self._config.active
        active.pending_validation = False
        save_boot_config(self._config)
        self._update_syslinux(previous)

        # Reboot
        log.info("Rebooting into slot %s...", previous)
        subprocess.run(["reboot"], check=False)
        return True

    # ── USB update detection ────────────────────────────────────────────────

    async def check_usb_update(self) -> dict | None:
        """Check mounted USB drives for update images."""
        usb_paths = [Path("/media"), Path("/mnt")]
        for base in usb_paths:
            if not base.exists():
                continue
            for mount in base.iterdir():
                for img in mount.glob("ozma-update-*.img"):
                    # Found an update image
                    sig_file = img.with_suffix(".sig")
                    hash_file = img.with_suffix(".sha256")

                    update_hash = ""
                    if hash_file.exists():
                        update_hash = hash_file.read_text().strip().split()[0]
                    else:
                        update_hash = hashlib.sha256(img.read_bytes()).hexdigest()

                    signature = ""
                    if sig_file.exists():
                        import base64
                        signature = base64.b64encode(sig_file.read_bytes()).decode()

                    # Extract version from filename: ozma-update-1.3.0.img
                    version = img.stem.replace("ozma-update-", "")

                    return {
                        "available": True,
                        "version": version,
                        "url": f"file://{img}",
                        "sha256": update_hash,
                        "signature": signature,
                        "source": "usb",
                    }
        return None

    # ── Syslinux config ─────────────────────────────────────────────────────

    def _update_syslinux(self, slot: str) -> None:
        """Update syslinux DEFAULT to boot the given slot."""
        if not SYSLINUX_CFG.exists():
            return
        try:
            # Remount boot partition read-write
            subprocess.run(["mount", "-o", "remount,rw", "/boot"],
                           capture_output=True, check=False)

            cfg = SYSLINUX_CFG.read_text()
            cfg = cfg.replace("DEFAULT ozma-a", f"DEFAULT ozma-{slot}")
            cfg = cfg.replace("DEFAULT ozma-b", f"DEFAULT ozma-{slot}")
            SYSLINUX_CFG.write_text(cfg)

            # Remount boot partition read-only
            subprocess.run(["mount", "-o", "remount,ro", "/boot"],
                           capture_output=True, check=False)

            log.info("Syslinux DEFAULT → ozma-%s", slot)
        except Exception as e:
            log.error("Failed to update syslinux: %s", e)

    # ── Status ──────────────────────────────────────────────────────────────

    def status(self) -> dict:
        return {
            "active_slot": self._config.active_slot,
            "current_version": self._config.active.version,
            "slot_a": self._config.slot_a.to_dict() if self._config.slot_a else {},
            "slot_b": self._config.slot_b.to_dict() if self._config.slot_b else {},
            "update_channel": self._config.update_channel,
            "applying": self._applying,
        }

    async def check_loop(self) -> None:
        """Poll for updates every hour."""
        while True:
            await asyncio.sleep(3600)
            try:
                info = await self.check_for_update()
                if info:
                    log.info("Update available: %s", info.get("version"))
            except Exception as e:
                log.debug("Update check failed: %s", e)

    def set_channel(self, channel: str) -> None:
        if channel in ("stable", "beta", "nightly"):
            self._config.update_channel = channel
            save_boot_config(self._config)


# ── CLI for health-check (called from init script) ─────────────────────────

def main():
    """Standalone health check — called from init system on boot."""
    import argparse
    p = argparse.ArgumentParser()
    p.add_argument("--health-check", action="store_true")
    args = p.parse_args()

    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")

    if args.health_check:
        mgr = UpdateManager()
        asyncio.run(mgr.health_check())


if __name__ == "__main__":
    main()
