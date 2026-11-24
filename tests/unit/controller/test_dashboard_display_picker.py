# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Dashboard VM console picker tests.

The static check always runs. The browser flow serves the real
``controller/static/dashboard.html`` beside a fake controller/node display API
and drives the picker opened from a stream tile. Playwright is optional, as in
``test_dashboard_node_token_ui.py``.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse

import pytest

pytestmark = pytest.mark.unit

REPO_ROOT = Path(__file__).resolve().parents[3]
DASHBOARD = REPO_ROOT / "controller" / "static" / "dashboard.html"
NODE_ID = "display-picker-node._ozma._udp.local."

OUTPUTS = [
    {
        "index": 0,
        "name": "Console_0",
        "source_type": "dbus",
        "capture_source_id": "dbus0",
        "width": 1920,
        "height": 1080,
        "primary": True,
    },
    {
        "index": 1,
        "name": "Console_1",
        "source_type": "dbus",
        "capture_source_id": "",
        "width": 1280,
        "height": 1024,
        "primary": False,
    },
]


def test_dashboard_has_console_picker_and_limitation_note():
    html = DASHBOARD.read_text()
    assert 'id="kvm-display-select"' in html
    assert 'id="kvm-display-note"' in html
    assert "selectDisplay(" in html
    assert "/display/select" in html
    assert "/display`" in html or '/display"' in html
    assert "capture_source_id" in html
    assert "display_capture" in html


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args) -> None:  # silence test output
        pass

    def _send(
        self, status: int, payload, content_type: str = "application/json"
    ) -> None:
        if isinstance(payload, (dict, list)):
            raw = json.dumps(payload).encode()
        elif isinstance(payload, str):
            raw = payload.encode()
        else:
            raw = bytes(payload)
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(raw)

    def _read_body(self) -> dict:
        length = int(self.headers.get("Content-Length") or 0)
        if not length:
            return {}
        raw = self.rfile.read(length)
        try:
            return json.loads(raw)
        except ValueError:
            return {}

    def do_GET(self) -> None:  # noqa: N802 - http.server API
        path = urlparse(self.path).path
        state = self.server.state
        if path in ("/", "/dashboard.html"):
            self._send(200, DASHBOARD.read_bytes(), "text/html; charset=utf-8")
            return
        if path == "/api/v1/status":
            self._send(
                200,
                {
                    "active_node_id": NODE_ID,
                    "active_scenario_id": None,
                    "nodes": {
                        NODE_ID: {
                            "id": NODE_ID,
                            "host": "127.0.0.1",
                            "port": 7331,
                            "role": "compute",
                            "hw": "soft",
                            "fw_version": "0.1.0",
                            "proto_version": 1,
                            "capabilities": ["video", "power"],
                            "api_port": 7444,
                            "stream_port": 7445,
                            "display_outputs": state["outputs"],
                        }
                    },
                },
            )
            return
        if path == "/api/v1/scenarios":
            self._send(200, {"scenarios": []})
            return
        if path == "/api/v1/streams":
            self._send(
                200,
                {
                    "streams": [
                        {
                            "node_id": NODE_ID,
                            "type": "hls-remote",
                            "active": True,
                        }
                    ]
                },
            )
            return
        if path == f"/api/v1/nodes/{NODE_ID}/display":
            self._send(
                200,
                {
                    "ok": True,
                    "display_outputs": state["outputs"],
                    "display_capture": state["capture"],
                },
            )
            return
        if path == "/api/v1/audio/nodes":
            self._send(200, {"nodes": {}, "links": []})
            return
        if path == "/api/v1/users/me":
            self._send(200, {"auth_enabled": False})
            return
        if path == "/api/v1/events":
            self._send(404, {"detail": "no websocket in fake controller"})
            return
        if path.startswith(f"/api/v1/streams/{NODE_ID}"):
            self._send(503, {"detail": "no stream in fake controller"})
            return
        self._send(200, {})

    def do_POST(self) -> None:  # noqa: N802 - http.server API
        path = urlparse(self.path).path
        body = self._read_body()
        state = self.server.state
        if path == f"/api/v1/nodes/{NODE_ID}/display/select":
            self.server.requests.append({"path": path, "body": body})
            if state["fail"]:
                self._send(
                    409,
                    {
                        "ok": False,
                        "code": "conflict",
                        "error": "the vnc capture provider exposes one default console",
                    },
                )
                return
            selected = int(body.get("index", 0))
            outputs = []
            for output in state["outputs"]:
                copy = dict(output)
                copy["capture_source_id"] = "dbus0" if copy["index"] == selected else ""
                outputs.append(copy)
            state["outputs"] = outputs
            state["capture"] = {
                "active_console": selected,
                "provider": "dbus",
                "limitation": f"only Console_{selected} has a live capture source",
            }
            self._send(
                200,
                {
                    "ok": True,
                    "active_console": selected,
                    "previous_console": 0,
                    "provider": "dbus",
                    "display_outputs": outputs,
                    "display_capture": state["capture"],
                    "changed": True,
                    "restarted": {"hls": True, "webrtc": False},
                },
            )
            return
        self._send(200, {})


@pytest.fixture()
def fake_controller():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    server.requests = []
    server.state = {
        "outputs": [dict(output) for output in OUTPUTS],
        "capture": {
            "active_console": 0,
            "provider": "dbus",
            "limitation": "only Console_0 has a live capture source; select another console to switch",
        },
        "fail": False,
    }
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server, f"http://127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


_FAKE_WS = """
(() => {
  class FakeWebSocket {
    constructor(url) {
      this.url = url; this.readyState = 1; this.onopen = null; this.onclose = null;
      this.onerror = null; this.onmessage = null;
      setTimeout(() => { if (this.onopen) this.onopen({}); }, 0);
    }
    send() {}
    close() { this.readyState = 3; if (this.onclose) this.onclose({}); }
    addEventListener(name, fn) { this['on' + name] = fn; }
  }
  window.WebSocket = FakeWebSocket;
})();
"""


async def _browser():
    async_api = pytest.importorskip("playwright.async_api")
    try:
        pw = await async_api.async_playwright().start()
        browser = await pw.chromium.launch(headless=True)
    except Exception as exc:  # pragma: no cover - environment dependent
        pytest.skip(f"Playwright Chromium unavailable: {exc}")
    return pw, browser


@pytest.mark.asyncio
async def test_display_picker_renders_switches_and_shows_a_409(fake_controller):
    server, url = fake_controller
    pw, browser = await _browser()
    try:
        page = await browser.new_page()
        await page.add_init_script(_FAKE_WS)
        await page.add_init_script(
            "try { localStorage.setItem('ozma-ui-mode', 'advanced'); } catch (e) {}"
        )
        await page.goto(f"{url}/")
        await page.wait_for_function("typeof openStream === 'function'")
        await page.evaluate(f"openStream({json.dumps(NODE_ID)})")

        select = page.locator("#kvm-display-select")
        await select.wait_for(state="visible", timeout=5000)
        options = await select.locator("option").all_text_contents()
        assert options == ["Console_0 ● live", "Console_1"]
        assert await select.input_value() == "0"
        note = await page.locator("#kvm-display-note").text_content()
        assert "Console_0" in note

        await select.select_option("1")
        await page.wait_for_function(
            "document.querySelector('#kvm-display-note').textContent.includes('Console_1')",
            timeout=5000,
        )
        assert server.requests[-1]["body"] == {"index": 1}
        assert await page.locator("#kvm-display-select").input_value() == "1"
        options = await page.locator("#kvm-display-select option").all_text_contents()
        assert options == ["Console_0", "Console_1 ● live"]

        # A typed node 409 stays visible instead of looking like a success.
        server.state["fail"] = True
        await page.locator("#kvm-display-select").select_option("0")
        await page.wait_for_function(
            "document.querySelector('#kvm-display-note').textContent.includes('one default console')",
            timeout=5000,
        )
        assert await page.locator("#kvm-display-select").input_value() == "1"
    finally:
        await browser.close()
        await pw.stop()
