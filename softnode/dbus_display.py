#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Async D-Bus display client for QEMU — real-time framebuffer + input.

Connects to QEMU's D-Bus display via p2p connection (QMP add_client).
No bus daemon needed. Implements RegisterListener for push-based
framebuffer at display refresh rate.

Architecture:
  1. Connect to QMP socket, create socketpair, pass FD via add_client
  2. Authenticate D-Bus on the display connection
  3. Call RegisterListener with a second socketpair
  4. Authenticate as CLIENT on the listener (QEMU is the server)
  5. Respond to GetAll with Listener interface properties
  6. QEMU pushes Scanout/Update with raw pixel data at display fps
  7. We encode to JPEG for streaming

Input goes via the display connection (Keyboard.Press/Release, Mouse.*).
"""

from __future__ import annotations

import asyncio
import io
import logging
import os
import socket
import struct
import time
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.softnode.dbus_display")

# Use dbus-fast for message serialization (it handles the wire format correctly)
from dbus_fast import Message, MessageType, Variant


def _build_method_call(path: str, interface: str, member: str,
                       signature: str = "", body: list | None = None,
                       serial: int = 1) -> bytes:
    """Build a serialized D-Bus method_call message."""
    msg = Message(
        message_type=MessageType.METHOD_CALL,
        serial=serial,
        path=path,
        interface=interface,
        member=member,
        signature=signature,
        body=body or [],
    )
    return msg._marshall(False)


def _build_method_return(reply_serial: int, signature: str = "",
                         body: list | None = None, serial: int = 1) -> bytes:
    """Build a serialized D-Bus method_return message."""
    msg = Message(
        message_type=MessageType.METHOD_RETURN,
        reply_serial=reply_serial,
        serial=serial,
        signature=signature,
        body=body or [],
    )
    return msg._marshall(False)


class DBusDisplayClient:
    """
    D-Bus p2p display client — framebuffer push + sub-ms input.

    Connects via QMP add_client. No bus daemon needed. QEMU pushes
    raw framebuffer data via RegisterListener at display refresh rate.
    """

    def __init__(self, qmp_socket_path: str = "", qmp_sock: socket.socket | None = None):
        self._qmp_path = qmp_socket_path
        self._qmp_sock = qmp_sock  # reuse existing QMP connection
        self._display_sock: socket.socket | None = None
        self._listener_sock: socket.socket | None = None
        self._connected = False
        self._width = 0
        self._height = 0
        self._stride = 0
        self._label = ""
        self._serial = 1
        # Framebuffer
        self._framebuffer: bytearray | None = None
        self._latest_jpeg: bytes | None = None
        self._frame_count = 0
        self._frame_event = asyncio.Event()
        self._jpeg_quality = 75
        self._stop = False

    @property
    def connected(self) -> bool:
        return self._connected

    @property
    def width(self) -> int:
        return self._width

    @property
    def height(self) -> int:
        return self._height

    @property
    def label(self) -> str:
        return self._label

    @property
    def latest_frame(self) -> bytes | None:
        return self._latest_jpeg

    @property
    def frame_count(self) -> int:
        return self._frame_count

    def _next_serial(self) -> int:
        s = self._serial
        self._serial += 1
        return s

    async def connect(self) -> bool:
        """Establish D-Bus p2p connection via QMP add_client."""
        import json as _json
        loop = asyncio.get_event_loop()

        try:
            def _setup():
                # QMP connection (reuse existing or create new)
                if self._qmp_sock:
                    qmp = self._qmp_sock
                else:
                    qmp = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    qmp.settimeout(5)
                    qmp.connect(self._qmp_path)
                    qmp.recv(4096)  # greeting
                    qmp.sendall(_json.dumps({"execute": "qmp_capabilities"}).encode() + b"\n")
                    qmp.recv(4096)

                # Display socketpair
                d_ours, d_theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
                cmd = _json.dumps({"execute": "getfd", "arguments": {"fdname": "ozma-display"}}).encode() + b"\n"
                qmp.sendmsg([cmd], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, struct.pack("i", d_theirs.fileno()))])
                d_theirs.close()
                resp = _json.loads(qmp.recv(4096))
                if "error" in resp:
                    d_ours.close(); qmp.close()
                    raise RuntimeError(f"getfd: {resp['error']}")

                qmp.sendall(_json.dumps({"execute": "add_client",
                    "arguments": {"protocol": "@dbus-display", "fdname": "ozma-display"}}).encode() + b"\n")
                resp = _json.loads(qmp.recv(4096))
                if not self._qmp_sock:
                    qmp.close()  # only close if we created it
                if "error" in resp:
                    d_ours.close()
                    raise RuntimeError(f"add_client: {resp['error']}")

                # D-Bus auth
                d_ours.settimeout(5)
                uid_hex = str(os.getuid()).encode().hex()
                d_ours.sendall(b"\0AUTH EXTERNAL " + uid_hex.encode() + b"\r\n")
                d_ours.recv(4096)
                d_ours.sendall(b"NEGOTIATE_UNIX_FD\r\n")
                d_ours.recv(4096)
                d_ours.sendall(b"BEGIN\r\n")

                return d_ours

            self._display_sock = await loop.run_in_executor(None, _setup)
            self._connected = True

            # Get console properties
            await self._get_properties()

            # Register framebuffer listener
            await self._register_listener()

            log.info("D-Bus p2p display: %s %dx%d (RegisterListener active)",
                     self._label, self._width, self._height)
            return True

        except Exception as e:
            log.warning("D-Bus p2p connect failed: %s", e)
            return False

    async def _get_properties(self):
        """Get Width, Height, Label from console properties."""
        msg = _build_method_call(
            "/org/qemu/Display1/Console_0",
            "org.freedesktop.DBus.Properties", "GetAll",
            signature="s", body=["org.qemu.Display1.Console"],
            serial=self._next_serial(),
        )
        loop = asyncio.get_event_loop()

        def _do():
            self._display_sock.sendall(msg)
            self._display_sock.settimeout(3)
            data = self._display_sock.recv(65536)
            # Parse response — look for width/height in the body
            if data and len(data) > 16:
                body = data
                if b"Width" in body:
                    idx = body.find(b"Width")
                    for i in range(idx, min(idx + 50, len(body) - 4)):
                        if body[i:i+1] == b"u":
                            val = struct.unpack("<I", body[i+1:i+5])[0]
                            if 100 < val < 10000:
                                self._width = val; break
                if b"Height" in body:
                    idx = body.find(b"Height")
                    for i in range(idx, min(idx + 50, len(body) - 4)):
                        if body[i:i+1] == b"u":
                            val = struct.unpack("<I", body[i+1:i+5])[0]
                            if 100 < val < 10000:
                                self._height = val; break
                if b"Label" in body:
                    idx = body.find(b"Label")
                    for i in range(idx, min(idx + 50, len(body) - 4)):
                        if body[i:i+1] == b"s":
                            slen = struct.unpack("<I", body[i+1:i+5])[0]
                            if 0 < slen < 100:
                                self._label = body[i+5:i+5+slen].decode("utf-8", errors="replace")
                                break

        await loop.run_in_executor(None, _do)

    async def _register_listener(self):
        """Register as framebuffer listener. QEMU pushes Scanout/Update to us."""
        loop = asyncio.get_event_loop()

        def _setup_listener():
            l_ours, l_theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)

            # Call RegisterListener with FD via SCM_RIGHTS
            msg = Message(
                message_type=MessageType.METHOD_CALL,
                serial=self._next_serial(),
                path="/org/qemu/Display1/Console_0",
                interface="org.qemu.Display1.Console",
                member="RegisterListener",
                signature="h",
                body=[0],
                unix_fds=[l_theirs.fileno()],
            )
            data = msg._marshall(negotiate_unix_fd=True)
            self._display_sock.sendmsg(
                [data],
                [(socket.SOL_SOCKET, socket.SCM_RIGHTS, struct.pack("i", l_theirs.fileno()))]
            )
            l_theirs.close()
            self._display_sock.settimeout(3)
            self._display_sock.recv(4096)  # method_return

            # D-Bus auth as CLIENT (QEMU is the server on this socket)
            l_ours.settimeout(5)
            uid_hex = str(os.getuid()).encode().hex()
            l_ours.sendall(b"\0AUTH EXTERNAL " + uid_hex.encode() + b"\r\n")
            l_ours.recv(4096)
            l_ours.sendall(b"NEGOTIATE_UNIX_FD\r\n")
            l_ours.recv(4096)
            l_ours.sendall(b"BEGIN\r\n")

            # Don't read anything — hand off immediately to async
            return l_ours

        self._listener_sock = await loop.run_in_executor(None, _setup_listener)

        # Run the listener in a dedicated thread with its own event loop
        # This isolates it from WebRTC/aiohttp/ffmpeg work that would
        # starve the read loop and cause QEMU to drop the connection
        import threading
        def _run_listener_thread():
            loop = asyncio.new_event_loop()
            asyncio.set_event_loop(loop)
            try:
                loop.run_until_complete(self._listener_main(self._listener_sock))
            except Exception as e:
                log.warning("Listener thread ended: %s", e)
            finally:
                loop.close()

        t = threading.Thread(target=_run_listener_thread, daemon=True, name="dbus-listener")
        t.start()
        log.info("Listener thread started")

    async def _listener_main(self, sock):
        """Full listener lifecycle in a dedicated thread."""
        sock.setblocking(False)
        reader, writer = await asyncio.open_connection(sock=sock)

        # Handle GetAll
        hdr = await asyncio.wait_for(reader.readexactly(12), timeout=5)
        body_len = struct.unpack("<I", hdr[4:8])[0]
        msg_serial = struct.unpack("<I", hdr[8:12])[0]
        fields_len = struct.unpack("<I", await reader.readexactly(4))[0]
        padded = (fields_len + 7) & ~7
        if padded: await reader.readexactly(padded)
        total = 12 + 4 + padded
        pad = ((total + 7) & ~7) - total
        if pad: await reader.readexactly(pad)
        if body_len: await reader.readexactly(body_len)

        reply = _build_method_return(
            reply_serial=msg_serial, signature="a{sv}",
            body=[{"Interfaces": Variant("as", ["org.qemu.Display1.Listener"])}],
            serial=1)
        writer.write(reply)
        await writer.drain()
        log.info("Listener GetAll done, reading frames")

        await self._listener_read(reader, writer)

    async def _listener_read(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        """Read loop — runs in dedicated thread, never starved."""
        reply_serial = 200

        try:
            while not self._stop:
                # Read header (12 bytes)
                hdr = await reader.readexactly(12)
                body_len = struct.unpack("<I", hdr[4:8])[0]
                msg_serial = struct.unpack("<I", hdr[8:12])[0]

                # Read fields length + fields
                fields_len_data = await reader.readexactly(4)
                fields_len = struct.unpack("<I", fields_len_data)[0]
                padded = (fields_len + 7) & ~7
                fields_data = await reader.readexactly(padded) if padded else b""

                # Pad header+fields to 8
                total = 12 + 4 + padded
                pad = ((total + 7) & ~7) - total
                if pad:
                    await reader.readexactly(pad)

                # Read body
                body = await reader.readexactly(body_len) if body_len else b""

                # Find member name
                member = ""
                for name in [b"Scanout", b"Update", b"Disable", b"CursorDefine", b"MouseSet"]:
                    if name in fields_data:
                        member = name.decode()
                        break

                # Reply FIRST — QEMU blocks until we reply
                reply_serial += 1
                reply = _build_method_return(reply_serial=msg_serial, serial=reply_serial)
                writer.write(reply)
                # Flush reply immediately — don't wait for encoding
                await writer.drain()

                # Then update framebuffer (fast — just memcpy, no encoding)
                if member == "Scanout" and body_len > 20:
                    try:
                        w, h, stride, fmt = struct.unpack("<IIII", body[:16])
                        arr_len = struct.unpack("<I", body[16:20])[0]
                        pixels = body[20:20 + arr_len]
                        if len(pixels) >= w * h * 4:
                            self._width = w
                            self._height = h
                            self._stride = stride
                            self._framebuffer = bytearray(pixels[:w * h * 4])
                            self._frame_count += 1
                            # Encode JPEG (skip some frames to keep up)
                            if self._frame_count % 3 == 0:
                                self._encode_jpeg()
                            if self._frame_count % 100 == 0:
                                log.info("Listener: %d frames, %dx%d", self._frame_count, w, h)
                    except Exception:
                        pass
                elif member == "Update" and body_len > 28:
                    self._handle_update(body)
                    asyncio.get_event_loop().run_in_executor(None, self._encode_jpeg)
                elif member == "Disable":
                    self._framebuffer = None
                    self._latest_jpeg = None

        except (asyncio.IncompleteReadError, ConnectionResetError, OSError) as e:
            log.info("Listener ended: %s", e)
        except Exception as e:
            log.warning("Listener error: %s", e)
            import traceback
            traceback.print_exc()

    def _handle_scanout(self, body: bytes):
        """Parse Scanout: (u width, u height, u stride, u pixman_format, ay data)."""
        try:
            w, h, stride, fmt = struct.unpack("<IIII", body[:16])
            arr_len = struct.unpack("<I", body[16:20])[0]
            pixels = body[20:20 + arr_len]
            if len(pixels) >= w * h * 4:
                self._width = w
                self._height = h
                self._stride = stride
                self._framebuffer = bytearray(pixels)
                self._encode_jpeg()
        except Exception as e:
            log.debug("Scanout parse: %s", e)

    def _handle_update(self, body: bytes):
        """Parse Update: (i x, i y, i w, i h, u stride, u format, ay data)."""
        if not self._framebuffer:
            return
        try:
            x, y, w, h = struct.unpack("<iiii", body[:16])
            stride, fmt = struct.unpack("<II", body[16:24])
            arr_len = struct.unpack("<I", body[24:28])[0]
            pixels = body[28:28 + arr_len]
            bpp = self._stride // self._width if self._width else 4
            for row in range(h):
                src_off = row * stride
                dst_off = (y + row) * self._stride + x * bpp
                chunk = w * bpp
                if src_off + chunk <= len(pixels) and dst_off + chunk <= len(self._framebuffer):
                    self._framebuffer[dst_off:dst_off + chunk] = pixels[src_off:src_off + chunk]
            self._encode_jpeg()
        except Exception as e:
            log.debug("Update parse: %s", e)

    def _encode_jpeg(self):
        """Encode framebuffer to JPEG."""
        if not self._framebuffer or not self._width or not self._height:
            return
        try:
            from PIL import Image
            img = Image.frombytes("RGBA", (self._width, self._height),
                                  bytes(self._framebuffer), "raw", "BGRA")
            buf = io.BytesIO()
            img.convert("RGB").save(buf, format="JPEG", quality=self._jpeg_quality)
            self._latest_jpeg = buf.getvalue()
            self._frame_count += 1
            if self._frame_count <= 3 or self._frame_count % 100 == 0:
                log.info("JPEG encoded: %d bytes, frame %d, %dx%d", len(self._latest_jpeg), self._frame_count, self._width, self._height)
            self._frame_event.set()
            self._frame_event.clear()
        except Exception as e:
            log.debug("JPEG encode: %s", e)

    async def wait_frame(self, timeout: float = 1.0) -> bytes | None:
        """Wait for next frame. Returns JPEG bytes or None."""
        try:
            await asyncio.wait_for(self._frame_event.wait(), timeout)
        except asyncio.TimeoutError:
            pass
        return self._latest_jpeg

    async def disconnect(self):
        self._stop = True
        self._connected = False
        for s in [self._display_sock, self._listener_sock]:
            if s:
                try: s.close()
                except: pass

    # ── Input (sub-ms via display connection) ─────────────────────────

    def _send_input(self, path: str, interface: str, member: str,
                    signature: str, body: list):
        """Send input via the display D-Bus connection (blocking, <1ms)."""
        if not self._display_sock:
            return
        msg = _build_method_call(path, interface, member, signature, body,
                                 serial=self._next_serial())
        try:
            self._display_sock.sendall(msg)
            # Drain response (fire-and-forget but prevent buffer buildup)
            self._display_sock.setblocking(False)
            try:
                self._display_sock.recv(4096)
            except BlockingIOError:
                pass
            finally:
                self._display_sock.setblocking(True)
        except Exception:
            pass

    async def key_press(self, keycode: int):
        self._send_input("/org/qemu/Display1/Console_0",
                         "org.qemu.Display1.Keyboard", "Press", "u", [keycode])

    async def key_release(self, keycode: int):
        self._send_input("/org/qemu/Display1/Console_0",
                         "org.qemu.Display1.Keyboard", "Release", "u", [keycode])

    async def mouse_move(self, x: int, y: int):
        self._send_input("/org/qemu/Display1/Console_0",
                         "org.qemu.Display1.Mouse", "SetAbsPosition", "uu", [x, y])

    async def mouse_press(self, button: int = 0):
        self._send_input("/org/qemu/Display1/Console_0",
                         "org.qemu.Display1.Mouse", "Press", "u", [button])

    async def mouse_release(self, button: int = 0):
        self._send_input("/org/qemu/Display1/Console_0",
                         "org.qemu.Display1.Mouse", "Release", "u", [button])

    async def mouse_click(self, x: int, y: int, button: int = 0):
        await self.mouse_move(x, y)
        await self.mouse_press(button)
        await asyncio.sleep(0.02)
        await self.mouse_release(button)
