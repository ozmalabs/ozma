# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Dashboard node-token UI tests.

The browser tests serve the real ``controller/static/dashboard.html`` against a
small fake controller API. They exercise metadata rendering, one-time token
reveal, dismissal (no token left in the DOM or browser storage), subsequent
token-free requests, revocation, and 401/403/global-fallback states. They skip
when Playwright or a browser binary is unavailable; the static checks still run.
"""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote, urlparse

import pytest

pytestmark = pytest.mark.unit

REPO_ROOT = Path(__file__).resolve().parents[3]
DASHBOARD = REPO_ROOT / "controller" / "static" / "dashboard.html"
NODE_ID = "ui-token-node._ozma._udp.local."
ONE_TIME = "one-time-ui-token-do-not-persist"


# ── Static safety checks (always run) ────────────────────────────────────────


def test_dashboard_token_ui_is_metadata_only_and_has_no_storage():
    html = DASHBOARD.read_text()

    assert 'id="token-overlay"' in html
    assert 'id="token-meta"' in html
    assert 'id="token-reveal"' in html
    assert 'id="token-reveal-value"' in html
    assert "openNodeToken(" in html
    assert "/token`" in html or "/token\"" in html

    # Inspect only the token implementation block: no browser storage or console
    # logging of anything (the one-time token must never reach either).
    start = html.index("// ── Node API token status")
    end = html.index("// ── Render: nodes")
    block = html[start:end]
    assert "localStorage" not in block
    assert "sessionStorage" not in block
    assert "console." not in block
    assert "token_returned_once" in block
    assert "tokenClearReveal" in block
    assert "DELETE" in block
    assert "write or admin" in block


# ── Fake controller ──────────────────────────────────────────────────────────


def _node_payload() -> dict:
    return {
        "id": NODE_ID,
        "host": "127.0.0.1",
        "port": 7331,
        "role": "compute",
        "hw": "soft",
        "fw_version": "0.1.0",
        "proto_version": 1,
        "capabilities": [],
        "api_port": 7444,
        "stream_port": 7445,
    }


class _FakeControllerHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args) -> None:  # silence test output
        pass

    def _read_body(self) -> bytes:
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def _send(self, status: int, payload, content_type: str = "application/json") -> None:
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

    def _record(self, method: str, body: bytes) -> None:
        self.server.requests.append({
            "method": method,
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": body.decode("utf-8", "replace"),
        })

    def _metadata(self) -> dict:
        state = self.server.state
        configured = bool(state["configured"])
        return {
            "node_id": NODE_ID,
            "configured": configured,
            "revoked": bool(state["revoked"]),
            "source": state["source"] if configured else None,
            "updated_at": state["updated_at"] if configured else None,
            "fallback_configured": bool(state["fallback"]),
        }

    def _token_request(self) -> tuple[bool, str]:
        path = urlparse(self.path).path
        prefix = "/api/v1/nodes/"
        if not path.startswith(prefix) or not path.endswith("/token"):
            return False, ""
        return True, unquote(path[len(prefix):-len("/token")])

    def _handle(self, method: str) -> None:
        body = self._read_body()
        self._record(method, body)
        path = urlparse(self.path).path
        state = self.server.state

        if path in ("/", "/dashboard.html"):
            self._send(200, DASHBOARD.read_bytes(), "text/html; charset=utf-8")
            return
        if path.startswith("/static/"):
            self._send(404, b"not found", "text/plain")
            return

        is_token, node_id = self._token_request()
        if is_token and node_id == NODE_ID:
            if method == "GET":
                if state["read_status"]:
                    self._send(state["read_status"], {"detail": "read denied"})
                    return
                self._send(200, self._metadata())
                return
            if method == "PUT":
                if state["write_status"]:
                    self._send(state["write_status"], {"detail": "write denied"})
                    return
                state["configured"] = True
                state["revoked"] = False
                state["source"] = "rotation"
                state["updated_at"] = time.time()
                payload = {
                    "ok": True,
                    "node_id": NODE_ID,
                    "rotated": True,
                    "changed": True,
                    "fallback_configured": bool(state["fallback"]),
                    **self._metadata(),
                }
                if state["generate_once"]:
                    payload["token"] = ONE_TIME
                    payload["token_returned_once"] = True
                self._send(200, payload)
                return
            if method == "DELETE":
                if state["write_status"]:
                    self._send(state["write_status"], {"detail": "write denied"})
                    return
                state["configured"] = False
                state["revoked"] = True
                state["source"] = None
                state["updated_at"] = None
                self._send(200, {
                    "ok": True,
                    "node_id": NODE_ID,
                    "cleared": True,
                    "fallback_configured": bool(state["fallback"]),
                    **self._metadata(),
                })
                return

        if path == "/api/v1/status":
            self._send(200, {
                "active_node_id": NODE_ID,
                "active_scenario_id": None,
                "nodes": {NODE_ID: _node_payload()},
            })
            return
        if path == "/api/v1/scenarios":
            self._send(200, {"scenarios": []})
            return
        if path == "/api/v1/streams":
            self._send(200, {"streams": []})
            return
        if path == "/api/v1/audio/nodes":
            self._send(200, {"nodes": {}, "links": []})
            return
        if path == "/api/v1/users/me":
            self._send(200, {"auth_enabled": False})
            return
        if path == "/api/v1/nodes":
            self._send(200, {"nodes": []})
            return
        if path == "/api/v1/events":
            # The dashboard opens a WebSocket; a plain 404 just surfaces a
            # browser console message and does not affect this test.
            self._send(404, {"detail": "no websocket in fake controller"})
            return

        # Every other dashboard fetch is non-essential for this test.
        self._send(200, {})

    def do_GET(self) -> None:  # noqa: N802 — http.server API
        self._handle("GET")

    def do_PUT(self) -> None:  # noqa: N802 — http.server API
        self._handle("PUT")

    def do_DELETE(self) -> None:  # noqa: N802 — http.server API
        self._handle("DELETE")

    def do_POST(self) -> None:  # noqa: N802 — http.server API
        self._handle("POST")


@pytest.fixture()
def fake_controller():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _FakeControllerHandler)
    server.requests = []
    server.state = {
        "configured": True,
        "revoked": False,
        "source": "rotation",
        "updated_at": time.time(),
        "fallback": False,
        "generate_once": True,
        "read_status": 0,
        "write_status": 0,
    }
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server, f"http://127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


@pytest.fixture()
async def browser():
    async_api = pytest.importorskip("playwright.async_api")
    try:
        pw = await async_api.async_playwright().start()
        launched = await pw.chromium.launch(headless=True)
    except Exception as exc:  # pragma: no cover - environment dependent
        pytest.skip(f"Playwright Chromium unavailable: {exc}")
    try:
        yield launched
    finally:
        await launched.close()
        await pw.stop()


# ── Browser flow ─────────────────────────────────────────────────────────────

_FAKE_WS = """
(() => {
  class FakeWebSocket {
    constructor(url) {
      this.url = url;
      this.readyState = 1;
      this.onopen = null;
      this.onclose = null;
      this.onerror = null;
      this.onmessage = null;
      setTimeout(() => { if (this.onopen) this.onopen({}); }, 0);
    }
    send() {}
    close() { this.readyState = 3; if (this.onclose) this.onclose({}); }
    addEventListener(name, fn) { this['on' + name] = fn; }
  }
  window.WebSocket = FakeWebSocket;
})();
"""


async def _prepare_page(page) -> None:
    # The dashboard loads its data only after the events WebSocket opens. Use a
    # tiny fake socket so the fake HTTP controller does not need an upgrade.
    await page.add_init_script(_FAKE_WS)
    await page.add_init_script(
        "try { localStorage.setItem('ozma-ui-mode', 'advanced'); } catch (e) {}"
    )


async def _open_token_dialog(page, base_url: str) -> None:
    await page.goto(base_url + "/dashboard.html", wait_until="domcontentloaded")
    await page.wait_for_selector("#nodes-list .node-card")
    await page.locator("#nodes-list button", has_text="Token").first.click()
    await page.wait_for_selector("#token-content", state="visible")


def _accept_dialogs(page) -> list[str]:
    import asyncio

    messages: list[str] = []

    def _accept(dialog):
        messages.append(dialog.message)
        asyncio.ensure_future(dialog.accept())

    page.on("dialog", _accept)
    return messages


async def test_metadata_one_time_reveal_dismiss_and_revoke(fake_controller, browser):
    server, base_url = fake_controller
    context = await browser.new_context()
    page = await context.new_page()
    _accept_dialogs(page)
    # Capture any console message so we can prove the secret never reaches it.
    console: list[str] = []
    page.on("console", lambda msg: console.append(msg.text))
    await _prepare_page(page)

    try:
        await _open_token_dialog(page, base_url)

        meta = await page.locator("#token-meta").inner_text()
        assert "Configured" in meta
        assert "rotation" in meta
        assert "updated" in meta.lower()
        assert ONE_TIME not in await page.content()
        assert ONE_TIME not in json.dumps(console)

        # Rotate: the fake controller returns the one-time secret exactly once.
        await page.locator("#token-rotate").click()
        await page.wait_for_selector("#token-reveal", state="visible")
        assert await page.locator("#token-reveal-value").inner_text() == ONE_TIME
        reveal = await page.locator("#token-reveal").inner_text()
        assert "shown once" in reveal.lower()
        assert "not stored in this browser" in reveal

        put_requests = [r for r in server.requests if r["method"] == "PUT"]
        assert len(put_requests) == 1
        assert ONE_TIME not in put_requests[0]["body"]

        # Dismissing removes the token from the DOM entirely.
        await page.locator("#token-dismiss").click()
        await page.wait_for_selector("#token-reveal", state="hidden")
        assert await page.locator("#token-reveal-value").text_content() == ""
        assert ONE_TIME not in await page.content()
        assert ONE_TIME not in await page.evaluate("document.body.innerText")

        storage = await page.evaluate(
            "() => ({local: {...localStorage}, session: {...sessionStorage}})"
        )
        assert ONE_TIME not in json.dumps(storage)

        # Subsequent reads are metadata-only and must not carry the secret.
        dismissed_at = len(server.requests)
        await page.evaluate("() => loadNodeToken()")
        await page.wait_for_selector("#token-content", state="visible")
        await page.locator("#token-done").click()
        await _open_token_dialog(page, base_url)
        later = server.requests[dismissed_at:]
        assert later
        assert all(ONE_TIME not in json.dumps(r) for r in later)
        assert all("authorization" not in r["headers"] for r in later)

        # Revoke with confirmation, then show the global-fallback state.
        server.state["fallback"] = True
        await page.locator("#token-revoke").click()
        await page.wait_for_function(
            "() => document.getElementById('token-meta').innerText.includes('Revoked')"
        )
        delete_requests = [r for r in server.requests if r["method"] == "DELETE"]
        assert len(delete_requests) == 1
        assert ONE_TIME not in await page.content()
        await page.evaluate("() => loadNodeToken()")
        await page.wait_for_function(
            "() => document.getElementById('token-note').innerText.includes('OZMA_NODE_API_TOKEN')"
        )
        assert "fallback" in (await page.locator("#token-note").inner_text()).lower()
        assert ONE_TIME not in await page.content()
        assert ONE_TIME not in json.dumps(console)
    finally:
        await context.close()


async def test_no_token_global_fallback_and_403_401(fake_controller, browser):
    server, base_url = fake_controller
    server.state.update({
        "configured": False,
        "revoked": False,
        "source": None,
        "updated_at": None,
        "fallback": True,
    })
    context = await browser.new_context()
    page = await context.new_page()
    _accept_dialogs(page)
    await _prepare_page(page)

    try:
        await _open_token_dialog(page, base_url)
        meta = await page.locator("#token-meta").inner_text()
        assert "Not configured" in meta
        note = await page.locator("#token-note").inner_text()
        assert "No per-node token" in note
        assert "OZMA_NODE_API_TOKEN" in note

        # Write scope is required for rotation: surface a clear 403 message.
        server.state["write_status"] = 403
        await page.locator("#token-rotate").click()
        await page.wait_for_selector("#token-error", state="visible")
        error = await page.locator("#token-error").inner_text()
        assert "write or admin" in error
        assert ONE_TIME not in await page.content()

        # An unauthenticated metadata read gets an explicit 401 message.
        server.state["read_status"] = 401
        await page.evaluate("() => loadNodeToken()")
        await page.wait_for_function(
            "() => document.getElementById('token-error').innerText.includes('not authenticated')"
        )
        assert ONE_TIME not in await page.content()
    finally:
        await context.close()
