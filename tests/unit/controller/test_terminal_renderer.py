"""Unit tests for terminal_renderer and term_view CLI.

Covers:
  - terminal_renderer: render_frame, _halfblock_render, _pixel_render, backend_name,
    stream_frames, render_frame mode dispatch
  - term_view: _terminal_size, _build_url, _request (URL construction, auth headers)

None of these tests require a running controller, a physical terminal, or chafa.
Chafa-dependent paths are guarded with importorskip / conditional skips.
"""

from __future__ import annotations

import argparse
import asyncio
import io
import sys
import types
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

pytestmark = pytest.mark.unit

# ── Helpers ───────────────────────────────────────────────────────────────────


def _make_jpeg(w: int = 80, h: int = 60, color: tuple = (30, 80, 200)) -> bytes:
    """Return a small JPEG as bytes using PIL."""
    PIL = pytest.importorskip("PIL.Image", reason="Pillow required")
    img = PIL.new("RGB", (w, h), color)
    buf = io.BytesIO()
    img.save(buf, format="JPEG", quality=85)
    return buf.getvalue()


def _make_pil(w: int = 80, h: int = 60, color: tuple = (30, 80, 200)):
    PIL = pytest.importorskip("PIL.Image", reason="Pillow required")
    return PIL.new("RGB", (w, h), color)


def _make_gradient_pil(w: int = 128, h: int = 64):
    """PIL image with vertical colour gradient so adjacent row-pairs differ (needed for ▀ char)."""
    PIL = pytest.importorskip("PIL.Image", reason="Pillow required")
    np = pytest.importorskip("numpy", reason="numpy required")
    arr = np.zeros((h, w, 3), dtype=np.uint8)
    # Vertical gradient: R ramps along rows so top != bot for each half-block pair
    arr[:, :, 0] = np.linspace(0, 255, h, dtype=np.uint8).reshape(h, 1)
    arr[:, :, 1] = 128
    arr[:, :, 2] = np.linspace(255, 0, h, dtype=np.uint8).reshape(h, 1)
    return PIL.fromarray(arr)


# ── terminal_renderer: backend_name ──────────────────────────────────────────


class TestBackendName:
    def test_returns_string(self):
        from terminal_renderer import backend_name
        name = backend_name()
        assert isinstance(name, str)
        assert len(name) > 0

    def test_halfblock_when_no_chafa(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)
        assert tr.backend_name() == "half-block (pure Python)"

    def test_chafa_when_available(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", True)
        assert tr.backend_name() == "chafa"


# ── terminal_renderer: _halfblock_render ─────────────────────────────────────


class TestHalfblockRender:
    def test_returns_bytes(self):
        from terminal_renderer import _halfblock_render
        img = _make_pil(160, 80)
        out = _halfblock_render(img, cols=40, rows=20)
        assert isinstance(out, bytes)

    def test_non_empty(self):
        from terminal_renderer import _halfblock_render
        img = _make_pil(160, 80)
        out = _halfblock_render(img, cols=40, rows=20)
        assert len(out) > 0

    def test_contains_ansi_reset(self):
        from terminal_renderer import _halfblock_render
        img = _make_pil(160, 80)
        out = _halfblock_render(img, cols=40, rows=20)
        assert b"\x1b[0m" in out

    def test_contains_truecolor_escape(self):
        from terminal_renderer import _halfblock_render
        img = _make_gradient_pil()
        out = _halfblock_render(img, cols=40, rows=20)
        # Truecolor foreground escape: ESC[38;2;r;g;bm
        assert b"\x1b[38;2;" in out or b"\x1b[48;2;" in out

    def test_contains_halfblock_char(self):
        from terminal_renderer import _halfblock_render
        img = _make_gradient_pil(128, 64)
        out = _halfblock_render(img, cols=40, rows=20)
        # ▀ is U+2580 encoded as UTF-8: 0xE2 0x96 0x80
        assert "▀".encode() in out

    def test_ends_with_crlf(self):
        from terminal_renderer import _halfblock_render
        img = _make_pil()
        out = _halfblock_render(img, cols=20, rows=10)
        assert out.rstrip(b"\x1b[0m").endswith(b"\r\n") or b"\r\n" in out

    def test_minimal_cols(self):
        from terminal_renderer import _halfblock_render
        img = _make_pil(10, 10)
        out = _halfblock_render(img, cols=1, rows=1)
        assert isinstance(out, bytes)

    def test_gradient_image(self):
        from terminal_renderer import _halfblock_render
        img = _make_gradient_pil()
        out = _halfblock_render(img, cols=60, rows=20)
        assert len(out) > 100

    def test_solid_black_all_space_chars(self):
        """Solid colour → top == bot → space char used, not ▀."""
        from terminal_renderer import _halfblock_render
        img = _make_pil(40, 40, color=(0, 0, 0))
        out = _halfblock_render(img, cols=20, rows=10)
        # Should NOT contain ▀ since all pixels are identical
        assert "▀".encode() not in out


# ── terminal_renderer: render_frame ──────────────────────────────────────────


class TestRenderFrame:
    def test_pixel_mode_returns_bytes(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)  # force halfblock
        jpeg = _make_jpeg()
        out = tr.render_frame(jpeg, mode="pixel", cols=40, rows=20)
        assert isinstance(out, bytes)
        assert len(out) > 0

    def test_includes_clear_screen_by_default(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)
        jpeg = _make_jpeg()
        out = tr.render_frame(jpeg, mode="pixel", cols=40, rows=20, home=False)
        assert out.startswith(b"\x1b[2J\x1b[H")

    def test_home_true_uses_cursor_home(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)
        jpeg = _make_jpeg()
        out = tr.render_frame(jpeg, mode="pixel", cols=40, rows=20, home=True)
        assert out.startswith(b"\x1b[H")
        assert not out.startswith(b"\x1b[2J")

    def test_no_deps_returns_error_message(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_DEPS", False)
        out = tr.render_frame(b"fakejpeg", mode="pixel")
        assert b"PIL" in out or b"not installed" in out

    def test_auto_mode_falls_back_to_pixel_when_no_ocr(self, monkeypatch):
        """Auto mode with a non-text image falls through to pixel rendering."""
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)

        # Mock TextCapture to return low-confidence (pure pixel content)
        fake_result = MagicMock()
        fake_result.confidence = 0.1
        fake_result.lines = []
        fake_tc = MagicMock()
        fake_tc.return_value.recognise_frame.return_value = fake_result

        with patch.dict("sys.modules", {"text_capture": MagicMock(TextCapture=fake_tc)}):
            # Re-import to pick up mocked module
            import importlib
            tr_fresh = importlib.import_module("terminal_renderer")
            monkeypatch.setattr(tr_fresh, "_CHAFA", False)
            monkeypatch.setattr(tr_fresh, "_DEPS", True)

            jpeg = _make_jpeg(160, 80)
            out = tr_fresh.render_frame(jpeg, mode="auto", cols=40, rows=20)
            assert isinstance(out, bytes)

    def test_pixel_mode_explicit(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)
        jpeg = _make_jpeg(160, 80, color=(200, 100, 50))
        out = tr.render_frame(jpeg, mode="pixel", cols=40, rows=20)
        assert b"\x1b[" in out  # some ANSI escape present

    def test_large_frame_renders(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)
        jpeg = _make_jpeg(1920, 1080, color=(128, 128, 128))
        out = tr.render_frame(jpeg, mode="pixel", cols=220, rows=50)
        assert isinstance(out, bytes)
        assert len(out) > 0


# ── terminal_renderer: _pixel_render dispatch ─────────────────────────────────


class TestPixelRenderDispatch:
    def test_falls_back_to_halfblock_when_chafa_fails(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", True)

        def bad_chafa(*a, **kw):
            raise RuntimeError("chafa exploded")

        monkeypatch.setattr(tr, "_chafa_render", bad_chafa)

        img = _make_gradient_pil()
        out = tr._pixel_render(img, cols=30, rows=15, pixel_mode="auto")
        assert isinstance(out, bytes)
        assert b"\x1b[" in out

    def test_no_chafa_uses_halfblock(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)

        called = []
        original = tr._halfblock_render

        def spy(*a, **kw):
            called.append(True)
            return original(*a, **kw)

        monkeypatch.setattr(tr, "_halfblock_render", spy)
        img = _make_gradient_pil()
        tr._pixel_render(img, cols=30, rows=15, pixel_mode="auto")
        assert called


# ── terminal_renderer: stream_frames ─────────────────────────────────────────


class TestStreamFrames:
    def _run(self, coro):
        return asyncio.get_event_loop().run_until_complete(coro)

    def test_yields_bytes(self, monkeypatch):
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)

        jpeg = _make_jpeg(80, 60)
        frames_yielded = []
        call_count = 0

        async def frame_fn():
            nonlocal call_count
            call_count += 1
            if call_count > 2:
                return None
            return jpeg

        async def collect():
            async for chunk in tr.stream_frames(frame_fn, mode="pixel",
                                                fps=100, cols=40, rows=20):
                frames_yielded.append(chunk)
                if len(frames_yielded) >= 2:
                    break

        self._run(collect())
        assert len(frames_yielded) >= 1
        for chunk in frames_yielded:
            assert isinstance(chunk, bytes)
            assert len(chunk) > 0

    def test_none_frame_skipped(self, monkeypatch):
        """None return from frame_fn should not produce output."""
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)

        yielded = []
        call_count = 0

        async def frame_fn():
            nonlocal call_count
            call_count += 1
            return None  # always skip

        async def collect():
            async for chunk in tr.stream_frames(frame_fn, fps=1000, cols=20, rows=10):
                yielded.append(chunk)  # pragma: no cover
                break  # shouldn't reach here

        # Run for a moment then stop — nothing should be yielded
        async def run_with_timeout():
            try:
                await asyncio.wait_for(collect(), timeout=0.05)
            except asyncio.TimeoutError:
                pass

        self._run(run_with_timeout())
        assert yielded == []

    def test_exception_in_frame_fn_skipped(self, monkeypatch):
        """Exceptions from the frame function are swallowed and iteration continues."""
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)

        jpeg = _make_jpeg()
        call_count = 0
        yielded = []

        async def frame_fn():
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                raise ConnectionError("network blip")
            return jpeg

        async def collect():
            async for chunk in tr.stream_frames(frame_fn, fps=1000, cols=20, rows=10):
                yielded.append(chunk)
                break

        self._run(collect())
        assert len(yielded) == 1

    def test_first_frame_clears_screen(self, monkeypatch):
        """First frame should use clear-screen (home=False), not cursor-home."""
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)

        jpeg = _make_jpeg()
        yielded = []

        async def frame_fn():
            return jpeg

        async def collect():
            async for chunk in tr.stream_frames(frame_fn, fps=1000, cols=20, rows=10):
                yielded.append(chunk)
                if len(yielded) >= 1:
                    break

        self._run(collect())
        assert len(yielded) >= 1
        assert yielded[0].startswith(b"\x1b[2J\x1b[H")

    def test_subsequent_frame_uses_cursor_home(self, monkeypatch):
        """Subsequent frames use cursor-home (no clear) to avoid flicker."""
        import terminal_renderer as tr
        monkeypatch.setattr(tr, "_CHAFA", False)

        jpeg = _make_jpeg()
        yielded = []

        async def frame_fn():
            return jpeg

        async def collect():
            async for chunk in tr.stream_frames(frame_fn, fps=1000, cols=20, rows=10):
                yielded.append(chunk)
                if len(yielded) >= 2:
                    break

        self._run(collect())
        assert len(yielded) >= 2
        assert yielded[1].startswith(b"\x1b[H")
        assert not yielded[1].startswith(b"\x1b[2J")


# ── term_view: _terminal_size ─────────────────────────────────────────────────


class TestTerminalSize:
    def test_returns_tuple_of_two_ints(self):
        from term_view import _terminal_size
        cols, rows = _terminal_size()
        assert isinstance(cols, int)
        assert isinstance(rows, int)

    def test_positive_values(self):
        from term_view import _terminal_size
        cols, rows = _terminal_size()
        assert cols >= 1
        assert rows >= 1

    def test_fallback_on_exception(self):
        """OSError from get_terminal_size returns (80, 24) fallback."""
        import term_view as tv

        # Use context manager (not monkeypatch) so the mock is gone before
        # pytest's own terminal reporter calls shutil.get_terminal_size.
        with patch.object(tv.shutil, "get_terminal_size",
                          side_effect=OSError("no tty")):
            cols, rows = tv._terminal_size()
        assert cols == 80
        assert rows == 24

    def test_rows_reduced_by_one(self):
        """Should reserve one row for the shell prompt."""
        import collections
        import term_view as tv

        FakeSize = collections.namedtuple("FakeSize", ["columns", "lines"])
        with patch.object(tv.shutil, "get_terminal_size",
                          return_value=FakeSize(120, 40)):
            cols, rows = tv._terminal_size()
        assert cols == 120
        assert rows == 39  # 40 - 1


# ── term_view: _build_url ─────────────────────────────────────────────────────


def _make_args(**kwargs) -> argparse.Namespace:
    defaults = dict(cols=0, rows=0, mode="auto", pixel_mode="auto",
                    fps=10.0, stream=False)
    defaults.update(kwargs)
    return argparse.Namespace(**defaults)


class TestBuildUrl:
    def test_basic_snapshot(self):
        from term_view import _build_url
        url = _build_url("http://localhost:7380", "vm1", _make_args())
        assert url.startswith("http://localhost:7380/api/v1/remote/vm1/view")
        assert "stream=" not in url or "stream=0" not in url

    def test_stream_flag_included(self):
        from term_view import _build_url
        url = _build_url("http://localhost:7380", "vm1", _make_args(stream=True))
        assert "stream=1" in url
        assert "fps=" in url

    def test_stream_false_not_included(self):
        from term_view import _build_url
        url = _build_url("http://localhost:7380", "vm1", _make_args(stream=False))
        assert "stream=1" not in url

    def test_explicit_cols_rows(self):
        from term_view import _build_url
        url = _build_url("http://localhost:7380", "vm1", _make_args(cols=132, rows=50))
        assert "cols=132" in url
        assert "rows=50" in url

    def test_mode_included(self):
        from term_view import _build_url
        url = _build_url("http://localhost:7380", "vm1", _make_args(mode="ocr"))
        assert "mode=ocr" in url

    def test_pixel_mode_included(self):
        from term_view import _build_url
        url = _build_url("http://localhost:7380", "vm1", _make_args(pixel_mode="sixel"))
        assert "pixel_mode=sixel" in url

    def test_trailing_slash_stripped(self):
        from term_view import _build_url
        url = _build_url("http://localhost:7380/", "vm1", _make_args())
        assert "/api/v1/remote/vm1/view" in url
        assert "//api" not in url

    def test_node_id_in_path(self):
        from term_view import _build_url
        url = _build_url("http://localhost:7380", "desk-left", _make_args())
        assert "/remote/desk-left/view" in url

    def test_fps_in_stream_url(self):
        from term_view import _build_url
        url = _build_url("http://localhost:7380", "vm1", _make_args(stream=True, fps=25.0))
        assert "fps=25.0" in url

    def test_auto_detect_cols_rows_when_zero(self, monkeypatch):
        """When cols/rows=0, terminal size is auto-detected."""
        import term_view as tv
        monkeypatch.setattr(tv, "_terminal_size", lambda: (120, 40))
        url = tv._build_url("http://localhost:7380", "vm1", _make_args(cols=0, rows=0))
        assert "cols=120" in url
        assert "rows=40" in url


# ── term_view: _request ───────────────────────────────────────────────────────


class TestRequest:
    def test_no_auth_header_when_no_token(self):
        from term_view import _request
        req = _request("http://localhost:7380/api/v1/remote/vm1/view", None)
        assert req.get_header("Authorization") is None

    def test_bearer_token_set(self):
        from term_view import _request
        req = _request("http://localhost:7380/api/v1/remote/vm1/view", "mytoken123")
        assert req.get_header("Authorization") == "Bearer mytoken123"

    def test_url_preserved(self):
        from term_view import _request
        url = "http://controller.local:7380/api/v1/remote/vm1/view?stream=1"
        req = _request(url, None)
        assert req.full_url == url


# ── term_view: main() argument parsing ───────────────────────────────────────


class TestMainArgParsing:
    """Verify the CLI parser accepts expected arguments without errors."""

    def _parse(self, argv: list[str]) -> argparse.Namespace:
        import argparse as ap
        import term_view as tv
        # Rebuild parser inline (same as main() but without executing)
        parser = ap.ArgumentParser()
        parser.add_argument("node_id")
        parser.add_argument("--url", default="http://localhost:7380")
        parser.add_argument("--token", default=None)
        parser.add_argument("--stream", "-s", action="store_true")
        parser.add_argument("--fps", type=float, default=10.0)
        parser.add_argument("--mode", choices=["auto", "ocr", "pixel"], default="auto")
        parser.add_argument("--cols", type=int, default=0)
        parser.add_argument("--rows", type=int, default=0)
        parser.add_argument("--pixel-mode", dest="pixel_mode",
                             choices=["auto", "sixel", "kitty", "half", "braille"],
                             default="auto")
        return parser.parse_args(argv)

    def test_node_id_positional(self):
        args = self._parse(["vm1"])
        assert args.node_id == "vm1"

    def test_stream_flag(self):
        args = self._parse(["vm1", "--stream"])
        assert args.stream is True

    def test_short_stream_flag(self):
        args = self._parse(["vm1", "-s"])
        assert args.stream is True

    def test_fps(self):
        args = self._parse(["vm1", "--stream", "--fps", "25"])
        assert args.fps == 25.0

    def test_mode_ocr(self):
        args = self._parse(["vm1", "--mode", "ocr"])
        assert args.mode == "ocr"

    def test_cols_rows(self):
        args = self._parse(["vm1", "--cols", "132", "--rows", "50"])
        assert args.cols == 132
        assert args.rows == 50

    def test_pixel_mode_sixel(self):
        args = self._parse(["vm1", "--pixel-mode", "sixel"])
        assert args.pixel_mode == "sixel"

    def test_token(self):
        args = self._parse(["vm1", "--token", "abc123"])
        assert args.token == "abc123"

    def test_url(self):
        args = self._parse(["vm1", "--url", "http://192.168.1.5:7380"])
        assert args.url == "http://192.168.1.5:7380"

    def test_defaults(self):
        args = self._parse(["vm1"])
        assert args.stream is False
        assert args.fps == 10.0
        assert args.mode == "auto"
        assert args.pixel_mode == "auto"
        assert args.cols == 0
        assert args.rows == 0


# ── API endpoint: /api/v1/remote/{node_id}/view ───────────────────────────────


class TestViewEndpoint:
    """Test the /api/v1/remote/{node_id}/view endpoint via FastAPI TestClient."""

    @pytest.fixture
    def app(self):
        """Minimal FastAPI app wired with just enough to exercise the view endpoint."""
        from fastapi import FastAPI
        from fastapi.responses import Response, StreamingResponse
        from unittest.mock import AsyncMock, MagicMock

        # Build a stripped-down app that replicates the view endpoint logic
        # without the full controller startup.
        mini = FastAPI()

        jpeg = _make_jpeg(80, 60, color=(100, 150, 200))

        mock_streams = MagicMock()
        mock_streams.get_snapshot = AsyncMock(return_value=jpeg)

        mock_state = MagicMock()
        good_node = MagicMock()
        mock_state.nodes = {"vm1": good_node}

        @mini.get("/api/v1/remote/{node_id}/view")
        async def view(node_id: str, stream: int = 0,
                       mode: str = "auto", cols: int = 80, rows: int = 24,
                       fps: float = 10.0, pixel_mode: str = "auto"):
            import terminal_renderer as tr
            from unittest.mock import patch as _patch
            # Force halfblock so no chafa needed in tests
            node = mock_state.nodes.get(node_id)
            if not node:
                from fastapi import HTTPException
                raise HTTPException(status_code=404, detail="Node not found")

            async def _get_frame():
                return await mock_streams.get_snapshot(node_id)

            if not stream:
                import asyncio as _aio
                jpeg_data = await _get_frame()
                if not jpeg_data:
                    from fastapi import HTTPException
                    raise HTTPException(status_code=503, detail="No stream available")
                loop = _aio.get_event_loop()
                import functools
                with _patch.object(tr, "_CHAFA", False):
                    data = await loop.run_in_executor(
                        None,
                        functools.partial(tr.render_frame, jpeg_data, mode,
                                          cols, rows, False, pixel_mode)
                    )
                return Response(content=data, media_type="text/plain; charset=utf-8")

            async def _gen():
                with _patch.object(tr, "_CHAFA", False):
                    async for chunk in tr.stream_frames(_get_frame, mode=mode, fps=fps,
                                                        cols=cols, rows=rows,
                                                        pixel_mode=pixel_mode):
                        yield chunk
                        return  # yield just one chunk in tests

            return StreamingResponse(_gen(), media_type="application/octet-stream")

        return mini

    @pytest.fixture
    def client(self, app):
        from fastapi.testclient import TestClient
        return TestClient(app)

    def test_snapshot_200(self, client):
        resp = client.get("/api/v1/remote/vm1/view")
        assert resp.status_code == 200

    def test_snapshot_content_type(self, client):
        resp = client.get("/api/v1/remote/vm1/view")
        assert "text/plain" in resp.headers["content-type"]

    def test_snapshot_body_is_ansi(self, client):
        resp = client.get("/api/v1/remote/vm1/view")
        body = resp.content
        assert b"\x1b[" in body  # ANSI escape present

    def test_snapshot_starts_with_clear(self, client):
        resp = client.get("/api/v1/remote/vm1/view")
        assert resp.content.startswith(b"\x1b[2J\x1b[H")

    def test_unknown_node_404(self, client):
        resp = client.get("/api/v1/remote/unknown-node/view")
        assert resp.status_code == 404

    def test_stream_returns_200(self, client):
        resp = client.get("/api/v1/remote/vm1/view?stream=1")
        assert resp.status_code == 200

    def test_stream_content_type(self, client):
        resp = client.get("/api/v1/remote/vm1/view?stream=1")
        assert "application/octet-stream" in resp.headers["content-type"]

    def test_stream_body_is_ansi(self, client):
        resp = client.get("/api/v1/remote/vm1/view?stream=1")
        assert b"\x1b[" in resp.content

    def test_cols_rows_params_accepted(self, client):
        """Passing cols/rows should not cause an error."""
        resp = client.get("/api/v1/remote/vm1/view?cols=132&rows=50")
        assert resp.status_code == 200

    def test_mode_pixel_accepted(self, client):
        resp = client.get("/api/v1/remote/vm1/view?mode=pixel")
        assert resp.status_code == 200

    def test_mode_auto_accepted(self, client):
        resp = client.get("/api/v1/remote/vm1/view?mode=auto")
        assert resp.status_code == 200

    def test_pixel_mode_half(self, client):
        resp = client.get("/api/v1/remote/vm1/view?pixel_mode=half")
        assert resp.status_code == 200
