"""Unit tests for screen reader — dataclasses, screen type detection, text grouping."""

import pytest

pytestmark = pytest.mark.unit


class TestTextRegion:
    def test_construction(self):
        from screen_reader import TextRegion
        tr = TextRegion(text="OK", x=100, y=200, width=40, height=20, confidence=0.95)
        assert tr.text == "OK"
        assert tr.x == 100
        assert tr.confidence == 0.95

    def test_default_confidence(self):
        from screen_reader import TextRegion
        tr = TextRegion(text="x", x=0, y=0, width=1, height=1)
        assert tr.confidence == 0.0


class TestUIElement:
    def test_center(self):
        from screen_reader import UIElement
        el = UIElement(element_type="button", text="OK", x=100, y=200, width=80, height=30)
        assert el.center == (140, 215)

    def test_to_dict(self):
        from screen_reader import UIElement
        el = UIElement(element_type="button", text="Save", x=10, y=20, width=60, height=25, clickable=True)
        d = el.to_dict()
        assert d["type"] == "button"
        assert d["text"] == "Save"
        assert d["clickable"] is True
        assert d["center"] == (40, 32)
        assert d["children"] == []

    def test_children(self):
        from screen_reader import UIElement
        child = UIElement(element_type="button", text="OK", x=50, y=50, width=40, height=20, clickable=True)
        parent = UIElement(element_type="dialog", text="Confirm", x=0, y=0, width=200, height=150,
                           children=[child])
        d = parent.to_dict()
        assert len(d["children"]) == 1
        assert d["children"][0]["text"] == "OK"


class TestScreenState:
    def _state(self, **kw):
        from screen_reader import ScreenState, TextRegion, UIElement
        s = ScreenState(**kw)
        return s

    def test_find_button_by_label(self):
        from screen_reader import ScreenState, UIElement
        btn = UIElement(element_type="button", text="Cancel", x=0, y=0, width=60, height=25, clickable=True)
        state = ScreenState(elements=[btn])
        assert state.find_button("Cancel") is btn
        assert state.find_button("cancel") is btn  # case insensitive
        assert state.find_button("Canc") is btn    # substring
        assert state.find_button("Save") is None

    def test_find_button_in_children(self):
        from screen_reader import ScreenState, UIElement
        btn = UIElement(element_type="button", text="OK", x=50, y=50, width=40, height=20, clickable=True)
        dialog = UIElement(element_type="dialog", text="Error", x=0, y=0, width=200, height=150,
                           children=[btn])
        state = ScreenState(elements=[dialog])
        assert state.find_button("OK") is btn

    def test_find_text(self):
        from screen_reader import ScreenState, TextRegion
        tr = TextRegion(text="Hello World", x=10, y=20, width=100, height=15)
        state = ScreenState(text_regions=[tr])
        assert state.find_text("Hello") is tr
        assert state.find_text("world") is tr  # case insensitive
        assert state.find_text("xyz") is None

    def test_to_dict(self):
        from screen_reader import ScreenState
        state = ScreenState(raw_text="test", screen_type="gui_light", ocr_method="tesseract")
        d = state.to_dict()
        assert d["raw_text"] == "test"
        assert d["screen_type"] == "gui_light"
        assert d["ocr_method"] == "tesseract"
        assert "text_regions" in d
        assert "elements" in d
        assert "lines" in d
        assert "paragraphs" in d

    def test_to_prompt(self):
        from screen_reader import ScreenState
        state = ScreenState(raw_text="Hello", screen_type="terminal", description="A terminal")
        prompt = state.to_prompt()
        assert "terminal" in prompt
        assert "A terminal" in prompt


class TestTextGrouping:
    """Test word → line → paragraph grouping."""

    def test_single_line(self):
        from screen_reader import ScreenReader, TextRegion
        reader = ScreenReader.__new__(ScreenReader)
        words = [
            TextRegion(text="Hello", x=10, y=100, width=50, height=15),
            TextRegion(text="World", x=70, y=102, width=50, height=15),
        ]
        lines, paragraphs = reader._group_text(words)
        assert len(lines) == 1
        assert lines[0].text == "Hello World"

    def test_two_lines(self):
        from screen_reader import ScreenReader, TextRegion
        reader = ScreenReader.__new__(ScreenReader)
        words = [
            TextRegion(text="Line1", x=10, y=100, width=50, height=15),
            TextRegion(text="Line2", x=10, y=130, width=50, height=15),
        ]
        lines, paragraphs = reader._group_text(words)
        assert len(lines) == 2
        assert lines[0].text == "Line1"
        assert lines[1].text == "Line2"

    def test_paragraph_grouping(self):
        from screen_reader import ScreenReader, TextRegion
        reader = ScreenReader.__new__(ScreenReader)
        words = [
            TextRegion(text="Para1A", x=10, y=100, width=50, height=15),
            TextRegion(text="Para1B", x=10, y=118, width=50, height=15),
            # Gap > 1.5× line height
            TextRegion(text="Para2", x=10, y=200, width=50, height=15),
        ]
        lines, paragraphs = reader._group_text(words)
        assert len(paragraphs) == 2
        assert "Para1A" in paragraphs[0].text
        assert "Para2" in paragraphs[1].text

    def test_empty_input(self):
        from screen_reader import ScreenReader
        reader = ScreenReader.__new__(ScreenReader)
        lines, paragraphs = reader._group_text([])
        assert lines == []
        assert paragraphs == []


class TestScreenTypeDetection:
    """Test _detect_screen_type with synthetic numpy arrays."""

    def test_dark_uniform_is_terminal(self):
        pytest.importorskip("numpy")
        import numpy as np
        from screen_reader import ScreenReader
        reader = ScreenReader.__new__(ScreenReader)
        # All-dark screen (like a terminal)
        arr = np.full((768, 1024, 3), 20, dtype=np.uint8)
        result = reader._detect_screen_type(arr)
        assert result in ("terminal", "gui_dark")

    def test_bright_is_gui_light(self):
        pytest.importorskip("numpy")
        import numpy as np
        from screen_reader import ScreenReader
        reader = ScreenReader.__new__(ScreenReader)
        arr = np.full((768, 1024, 3), 220, dtype=np.uint8)
        result = reader._detect_screen_type(arr)
        assert result == "gui_light"

    def test_blue_is_bios(self):
        pytest.importorskip("numpy")
        import numpy as np
        from screen_reader import ScreenReader
        reader = ScreenReader.__new__(ScreenReader)
        # Blue-dominant, medium brightness (>80 overall), uniform
        arr = np.zeros((768, 1024, 3), dtype=np.uint8)
        arr[:, :, 0] = 50   # R
        arr[:, :, 1] = 80   # G
        arr[:, :, 2] = 180  # B — blue dominant, avg brightness ~103
        result = reader._detect_screen_type(arr)
        assert result == "bios"

    def test_dark_with_bright_bar_is_gui_dark(self):
        pytest.importorskip("numpy")
        import numpy as np
        from screen_reader import ScreenReader
        reader = ScreenReader.__new__(ScreenReader)
        # Dark screen with bright taskbar at bottom
        arr = np.full((768, 1024, 3), 30, dtype=np.uint8)
        arr[708:768, :, :] = 200  # bright bottom bar
        result = reader._detect_screen_type(arr)
        assert result == "gui_dark"


class TestDeduplication:
    def test_exact_duplicates_removed(self):
        from screen_reader import ScreenReader, TextRegion
        regions = [
            TextRegion(text="Hello", x=10, y=10, width=50, height=15, confidence=0.9),
            TextRegion(text="Hello", x=10, y=10, width=50, height=15, confidence=0.8),
        ]
        result = ScreenReader._deduplicate_regions(regions)
        assert len(result) == 1
        assert result[0].confidence == 0.9  # higher confidence kept

    def test_non_overlapping_kept(self):
        from screen_reader import ScreenReader, TextRegion
        regions = [
            TextRegion(text="Left", x=10, y=10, width=50, height=15, confidence=0.9),
            TextRegion(text="Right", x=200, y=10, width=50, height=15, confidence=0.9),
        ]
        result = ScreenReader._deduplicate_regions(regions)
        assert len(result) == 2

    def test_empty_input(self):
        from screen_reader import ScreenReader
        assert ScreenReader._deduplicate_regions([]) == []
