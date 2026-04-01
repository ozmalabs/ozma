# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Userspace USB mass storage gadget via FunctionFS.

No image files. No block devices. No kernel mass_storage module.
The Python process IS the USB drive. SCSI commands arrive as bulk
transfers, the FATSynthesiser computes sector data on demand, sends
it back. All in userspace.

This is the same code path for soft nodes (dummy_hcd) and hardware
nodes (real UDC like dwc2/musb). The only difference is which UDC
the gadget binds to.

Architecture:
  FATSynthesiser.read_sectors(offset, length)
      ↑ direct Python call
  USBMassStorageFunction (this file)
      ↑ handles SCSI BBB protocol on bulk endpoints
  FunctionFS + ConfigFS
      ↑ kernel USB gadget subsystem
  UDC (dummy_hcd for VMs, dwc2 for hardware)
      ↑
  Target sees a real USB mass storage device

Uses python-functionfs (pip install functionfs) for the USB plumbing.
We handle the SCSI layer on top.

USB Mass Storage Bulk-Only Transport (BBB):
  1. Host sends CBW (Command Block Wrapper) — 31 bytes, bulk OUT
  2. Device processes SCSI command
  3. Data phase: bulk IN (read) or bulk OUT (write)
  4. Device sends CSW (Command Status Wrapper) — 13 bytes, bulk IN
"""

from __future__ import annotations

import logging
import os
import struct
import threading
from pathlib import Path
from typing import Any, TYPE_CHECKING

if TYPE_CHECKING:
    from virtual_media import FATSynthesiser

log = logging.getLogger("ozma.usb_mass_storage")

# ── SCSI constants ─────────────────────────────────────────────────────────

SCSI_TEST_UNIT_READY = 0x00
SCSI_REQUEST_SENSE = 0x03
SCSI_INQUIRY = 0x12
SCSI_MODE_SENSE_6 = 0x1A
SCSI_START_STOP_UNIT = 0x1B
SCSI_PREVENT_ALLOW_MEDIUM_REMOVAL = 0x1E
SCSI_READ_CAPACITY_10 = 0x25
SCSI_READ_10 = 0x28
SCSI_WRITE_10 = 0x2A
SCSI_MODE_SENSE_10 = 0x5A
SCSI_READ_FORMAT_CAPACITIES = 0x23
SCSI_SYNCHRONIZE_CACHE = 0x35

# CBW/CSW
CBW_SIGNATURE = 0x43425355  # 'USBC'
CSW_SIGNATURE = 0x53425355  # 'USBS'
CBW_SIZE = 31
CSW_SIZE = 13

SECTOR_SIZE = 512


class SCSIHandler:
    """
    Handles SCSI commands backed by a FATSynthesiser.

    Stateless — every command is self-contained. The synthesiser
    provides sector data on demand.
    """

    def __init__(self, synth: "FATSynthesiser",
                 vendor: str = "Ozma",
                 product: str = "Virtual Drive",
                 revision: str = "1.0") -> None:
        self._synth = synth
        self._vendor = vendor.ljust(8)[:8].encode("ascii")
        self._product = product.ljust(16)[:16].encode("ascii")
        self._revision = revision.ljust(4)[:4].encode("ascii")
        self._total_sectors = synth.total_bytes // SECTOR_SIZE
        # Write overlay for read-write support
        self._write_overlay: dict[int, bytes] = {}

    def handle_cbw(self, cbw_data: bytes) -> tuple[bytes | None, bytes]:
        """
        Process a CBW and return (data_to_send, csw).

        Returns:
          data_to_send: bytes to send on bulk IN (or None if no data phase)
          csw: the 13-byte CSW response
        """
        if len(cbw_data) < CBW_SIZE:
            return None, self._make_csw(0, 0, 1)  # phase error

        sig, tag, data_len, flags, lun, cb_len = struct.unpack_from("<IIIBBB", cbw_data)
        cb_len = cb_len & 0x1F
        cb = cbw_data[15:15 + cb_len]
        direction_in = bool(flags & 0x80)

        if sig != CBW_SIGNATURE:
            log.warning("Bad CBW signature: 0x%08x", sig)
            return None, self._make_csw(tag, 0, 1)

        opcode = cb[0] if cb else 0
        data, residue, status = self._dispatch(opcode, cb, data_len, direction_in)

        csw = self._make_csw(tag, residue, status)
        return data, csw

    def handle_write_data(self, cbw_data: bytes, write_data: bytes) -> bytes:
        """Handle the data phase for a WRITE command. Returns CSW."""
        if len(cbw_data) < CBW_SIZE:
            return self._make_csw(0, 0, 1)

        sig, tag, data_len, flags, lun, cb_len = struct.unpack_from("<IIIBBB", cbw_data)
        cb = cbw_data[15:15 + (cb_len & 0x1F)]
        opcode = cb[0] if cb else 0

        if opcode == SCSI_WRITE_10 and len(cb) >= 10:
            lba = struct.unpack_from(">I", cb, 2)[0]
            offset = lba * SECTOR_SIZE
            self._write_overlay[offset] = write_data
            return self._make_csw(tag, 0, 0)

        return self._make_csw(tag, 0, 0)

    def _dispatch(self, opcode: int, cb: bytes, max_len: int,
                   direction_in: bool) -> tuple[bytes | None, int, int]:
        """
        Dispatch a SCSI command. Returns (data, residue, status).
        status: 0=good, 1=failed, 2=phase error
        """
        match opcode:
            case 0x00:  # TEST UNIT READY
                return None, 0, 0

            case 0x03:  # REQUEST SENSE
                sense = bytearray(18)
                sense[0] = 0x70  # current errors
                sense[7] = 10    # additional sense length
                return bytes(sense), max_len - 18, 0

            case 0x12:  # INQUIRY
                data = bytearray(36)
                data[0] = 0x00   # direct access block device
                data[1] = 0x80   # removable
                data[2] = 0x02   # SPC-2
                data[3] = 0x02   # response data format
                data[4] = 31     # additional length
                data[8:16] = self._vendor
                data[16:32] = self._product
                data[32:36] = self._revision
                return bytes(data), max_len - 36, 0

            case 0x1A:  # MODE SENSE(6)
                data = bytearray(4)
                data[0] = 3  # mode data length
                return bytes(data), max_len - 4, 0

            case 0x5A:  # MODE SENSE(10)
                data = bytearray(8)
                data[1] = 6  # mode data length
                return bytes(data), max_len - 8, 0

            case 0x1B:  # START STOP UNIT
                return None, 0, 0

            case 0x1E:  # PREVENT ALLOW MEDIUM REMOVAL
                return None, 0, 0

            case 0x25:  # READ CAPACITY(10)
                data = struct.pack(">II",
                                   self._total_sectors - 1,
                                   SECTOR_SIZE)
                return data, max_len - 8, 0

            case 0x23:  # READ FORMAT CAPACITIES
                data = bytearray(12)
                data[3] = 8  # capacity list length
                struct.pack_into(">I", data, 4, self._total_sectors)
                struct.pack_into(">I", data, 8, SECTOR_SIZE)
                data[8] = 0x02  # formatted media
                return bytes(data), max_len - 12, 0

            case 0x28:  # READ(10)
                if len(cb) < 10:
                    return None, 0, 1
                lba = struct.unpack_from(">I", cb, 2)[0]
                count = struct.unpack_from(">H", cb, 7)[0]
                offset = lba * SECTOR_SIZE
                length = count * SECTOR_SIZE

                # Check write overlay first
                data = bytearray(self._synth.read_sectors(offset, length))
                for ov_off, ov_data in self._write_overlay.items():
                    ov_end = ov_off + len(ov_data)
                    req_end = offset + length
                    if ov_off < req_end and ov_end > offset:
                        start = max(ov_off, offset)
                        end = min(ov_end, req_end)
                        data[start - offset:end - offset] = ov_data[start - ov_off:end - ov_off]

                return bytes(data), max_len - length, 0

            case 0x2A:  # WRITE(10) — data phase handled separately
                return None, 0, 0

            case 0x35:  # SYNCHRONIZE CACHE
                return None, 0, 0

            case _:
                log.debug("Unknown SCSI command: 0x%02x", opcode)
                return None, 0, 1  # command failed

    def _make_csw(self, tag: int, residue: int, status: int) -> bytes:
        return struct.pack("<IIII", CSW_SIGNATURE, tag, residue, status)[:CSW_SIZE]


# ── FunctionFS mass storage function ──────────────────────────────────────

try:
    import functionfs
    import functionfs.ch9
    from functionfs.gadget import (
        Gadget,
        ConfigFunctionFFS,
    )
    _HAS_FUNCTIONFS = True
except ImportError:
    _HAS_FUNCTIONFS = False


class MassStorageGadget:
    """
    USB mass storage via FunctionFS. No files, no block devices.

    Creates a ConfigFS gadget with a FunctionFS mass storage function.
    Bulk endpoints handle SCSI BBB protocol. The SCSIHandler dispatches
    commands to the FATSynthesiser.
    """

    def __init__(self, synth: "FATSynthesiser",
                 udc: str = "dummy_udc.0",
                 vendor_id: int = 0x1d6b,
                 product_id: int = 0x0104,
                 manufacturer: str = "Ozma Labs",
                 product_name: str = "Ozma Virtual Drive",
                 serial: str = "OZMA0001") -> None:
        if not _HAS_FUNCTIONFS:
            raise RuntimeError("pip install functionfs")
        self._synth = synth
        self._scsi = SCSIHandler(synth)
        self._udc = udc
        self._vid = vendor_id
        self._pid = product_id
        self._manufacturer = manufacturer
        self._product_name = product_name
        self._serial = serial
        self._running = False
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        """Start the gadget in a background thread."""
        self._running = True
        self._thread = threading.Thread(target=self._run, daemon=True, name="usb-mass-storage")
        self._thread.start()
        log.info("USB mass storage gadget starting on %s", self._udc)

    def stop(self) -> None:
        self._running = False
        if self._thread:
            self._thread.join(timeout=5)

    def _run(self) -> None:
        """Run the FunctionFS gadget (blocking)."""
        scsi = self._scsi
        func_ref = [None]  # mutable container to capture the function

        def make_function(path):
            func_ref[0] = _MSCFunction(path, scsi)
            return func_ref[0]

        try:
            with Gadget(
                udc=self._udc,
                idVendor=self._vid,
                idProduct=self._pid,
                lang_dict={
                    0x0409: {
                        "manufacturer": self._manufacturer,
                        "product": self._product_name,
                        "serialnumber": self._serial,
                    },
                },
                config_list=[{
                    "function_list": [
                        ConfigFunctionFFS(getFunction=make_function),
                    ],
                    "MaxPower": 500,
                    "bmAttributes": functionfs.ch9.USB_CONFIG_ATT_ONE,
                }],
            ) as gadget:
                log.info("USB mass storage gadget active on %s", self._udc)
                func = func_ref[0]
                # processEvents dispatches FunctionFS events (ENABLE, SETUP, etc.)
                # Our onEnable starts the I/O thread for bulk transfers.
                # processEvents blocks — it runs until the gadget is disconnected.
                while self._running:
                    func = func_ref[0]
                    if func:
                        try:
                            func.processEvents()
                        except Exception:
                            break
                    else:
                        import time
                        time.sleep(0.1)

        except Exception as e:
            log.error("USB mass storage gadget error: %s", e)
            import traceback
            traceback.print_exc()


class _MSCFunction(functionfs.Function):
    """FunctionFS function implementing USB mass storage BBB."""

    def __init__(self, path: str, scsi: SCSIHandler) -> None:
        self._scsi = scsi
        self._pending_cbw: bytes | None = None

        # Use the library's helper to build descriptors correctly
        fs_list, hs_list, ss_list = functionfs.getInterfaceInAllSpeeds(
            interface={
                "bInterfaceClass": 8,       # Mass Storage
                "bInterfaceSubClass": 6,    # SCSI transparent
                "bInterfaceProtocol": 80,   # Bulk-Only Transport
                "iInterface": 1,
            },
            endpoint_list=[
                {
                    "endpoint": {
                        "bEndpointAddress": functionfs.ch9.USB_DIR_IN,
                        "bmAttributes": functionfs.ch9.USB_ENDPOINT_XFER_BULK,
                    },
                },
                {
                    "endpoint": {
                        "bEndpointAddress": functionfs.ch9.USB_DIR_OUT,
                        "bmAttributes": functionfs.ch9.USB_ENDPOINT_XFER_BULK,
                    },
                },
            ],
        )

        super().__init__(
            path,
            fs_list=fs_list,
            hs_list=hs_list,
            ss_list=ss_list,
            lang_dict={0x0409: ["Ozma Mass Storage"]},
        )

    def onEnable(self):
        """Start a thread that handles bulk transfers synchronously."""
        log.info("Mass storage endpoints enabled")
        # Don't call super().onEnable() — we handle I/O ourselves
        # ep_list[0] = ep0 (control), [1] = IN (bulk), [2] = OUT (bulk)
        self._ep_in_fd = self._ep_list[1].fileno()
        self._ep_out_fd = self._ep_list[2].fileno()
        self._enabled = True
        self._io_thread = threading.Thread(
            target=self._io_loop, daemon=True, name="msc-io",
        )
        self._io_thread.start()

    def onDisable(self):
        log.info("Mass storage endpoints disabled")
        self._enabled = False

    def onSetup(self, request_type, request, value, index, length):
        """Handle class-specific control requests."""
        if request == 0xFE:  # Get Max LUN
            self.ep0.write(b'\x00')
        elif request == 0xFF:  # Bulk-Only Mass Storage Reset
            self.ep0.write(b'')
        else:
            super().onSetup(request_type, request, value, index, length)

    # ── Synchronous I/O loop for bulk transfers ────────────────────────

    def _io_loop(self):
        """
        Read CBWs from bulk OUT, process SCSI commands, write responses
        to bulk IN. Runs in a dedicated thread — simple blocking I/O.
        """
        log.info("Mass storage I/O loop started")
        while self._enabled:
            try:
                # Read CBW from host (blocking)
                cbw = os.read(self._ep_out_fd, CBW_SIZE)
                if len(cbw) < CBW_SIZE:
                    continue

                sig = struct.unpack_from("<I", cbw)[0]
                if sig != CBW_SIGNATURE:
                    continue

                flags = cbw[12]
                direction_in = bool(flags & 0x80)
                data_len = struct.unpack_from("<I", cbw, 8)[0]
                opcode = cbw[15]

                if opcode == SCSI_WRITE_10 and data_len > 0 and not direction_in:
                    # Read write data from host
                    write_data = b''
                    while len(write_data) < data_len:
                        chunk = os.read(self._ep_out_fd, data_len - len(write_data))
                        if not chunk:
                            break
                        write_data += chunk
                    csw = self._scsi.handle_write_data(cbw, write_data)
                    os.write(self._ep_in_fd, csw)
                    continue

                # Process SCSI command
                response_data, csw = self._scsi.handle_cbw(cbw)

                # Send data phase then CSW
                if response_data and direction_in:
                    os.write(self._ep_in_fd, response_data)
                os.write(self._ep_in_fd, csw)

            except OSError as e:
                if e.errno == 108:  # ESHUTDOWN — endpoint disabled
                    break
                log.debug("I/O error: %s", e)
            except Exception as e:
                log.debug("SCSI error: %s", e)

        log.info("Mass storage I/O loop stopped")


def create_mass_storage_gadget(
    source_dir: str | Path,
    udc: str = "dummy_udc.0",
    label: str = "OZMA",
) -> MassStorageFunction:
    """
    One-liner: turn a directory into a USB mass storage device.

    The directory contents appear as a FAT32 USB drive on the specified UDC.
    No image files, no block devices. Pure userspace.

    Usage:
        from virtual_media import FATSynthesiser
        synth = FATSynthesiser("/path/to/files")
        synth.scan()
        gadget = create_mass_storage_gadget(synth, udc="dummy_udc.0")
        gadget.start()
        # The USB drive is now live. Files appear in the target.
    """
    from virtual_media import FATSynthesiser
    synth = FATSynthesiser(str(source_dir), label=label)
    synth.scan()
    gadget = MassStorageFunction(synth, udc=udc)
    return gadget
