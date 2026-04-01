# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Screen reader — advanced OCR + UI element detection for AI agent control.

Five levels of screen understanding:

  Level 1 — Bitmap font OCR (BIOS/terminal)
    Uses text_capture.py bitmap matcher. <2ms, zero deps.
    Auto-detected when the screen looks like a fixed-width text display.

  Level 2 — Tesseract OCR (GUI text)
    Multi-pass with preprocessing: upscale, contrast, adaptive threshold,
    color-region splitting (dark-on-light + light-on-dark separately).
    Word-level bounding boxes grouped into lines and paragraphs.

  Level 3 — UI element detection (heuristic)
    Combines OCR results with edge/rectangle detection to identify:
    buttons, dialogs, text fields, checkboxes, menus, links, errors.

  Level 4 — AI vision (semantic)
    Sends screenshot + structured prompt to a vision model (Ollama,
    Claude via Connect, GPT-4V). Returns description + suggested action.

  Level 5 — Vector/structural extraction
    Edges → lines → rectangles → classified shapes with colors.
    Lightweight alternative to full AI vision.

Advanced preprocessing pipeline:
  1. Auto-detect screen type (terminal vs GUI vs dark theme vs light theme)
  2. Upscale small images (< 1280px) for better OCR accuracy
  3. Color-region splitting: segment into light-bg and dark-bg regions
  4. Per-region adaptive threshold + Tesseract
  5. Merge word results from all passes, deduplicate by overlap
  6. Group words → lines → paragraphs by spatial proximity
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.screen_reader")


@dataclass
class TextRegion:
    """A piece of text found on screen."""
    text: str
    x: int
    y: int
    width: int
    height: int
    confidence: float = 0.0


@dataclass
class UIElement:
    """A detected UI element."""
    element_type: str   # button, dialog, text_field, menu, checkbox, label, icon
    text: str           # label/content
    x: int
    y: int
    width: int
    height: int
    clickable: bool = False
    children: list["UIElement"] = field(default_factory=list)

    @property
    def center(self) -> tuple[int, int]:
        return (self.x + self.width // 2, self.y + self.height // 2)

    def to_dict(self) -> dict:
        return {
            "type": self.element_type,
            "text": self.text,
            "x": self.x, "y": self.y,
            "width": self.width, "height": self.height,
            "center": self.center,
            "clickable": self.clickable,
            "children": [c.to_dict() for c in self.children],
        }


@dataclass
class TextLine:
    """A line of text (grouped from word-level TextRegions)."""
    text: str
    x: int
    y: int
    width: int
    height: int
    words: list[TextRegion] = field(default_factory=list)


@dataclass
class TextParagraph:
    """A paragraph (grouped from adjacent lines)."""
    text: str
    x: int
    y: int
    width: int
    height: int
    lines: list[TextLine] = field(default_factory=list)


@dataclass
class ScreenState:
    """Complete understanding of what's on screen."""
    text_regions: list[TextRegion] = field(default_factory=list)
    lines: list[TextLine] = field(default_factory=list)
    paragraphs: list[TextParagraph] = field(default_factory=list)
    elements: list[UIElement] = field(default_factory=list)
    description: str = ""
    raw_text: str = ""
    screen_type: str = ""  # "terminal", "gui_light", "gui_dark", "bios", "unknown"
    ocr_method: str = ""   # "bitmap", "tesseract", "multi_pass"

    def find_button(self, label: str) -> UIElement | None:
        """Find a button by its label text."""
        label_lower = label.lower()
        for el in self.elements:
            if el.element_type == "button" and label_lower in el.text.lower():
                return el
            for child in el.children:
                if child.element_type == "button" and label_lower in child.text.lower():
                    return child
        return None

    def find_text(self, text: str) -> TextRegion | None:
        """Find a text region containing the given string."""
        text_lower = text.lower()
        for tr in self.text_regions:
            if text_lower in tr.text.lower():
                return tr
        return None

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "text_regions": [{"text": t.text, "x": t.x, "y": t.y,
                              "width": t.width, "height": t.height,
                              "confidence": t.confidence}
                             for t in self.text_regions],
            "lines": [{"text": l.text, "x": l.x, "y": l.y,
                        "width": l.width, "height": l.height}
                       for l in self.lines],
            "paragraphs": [{"text": p.text, "x": p.x, "y": p.y,
                            "width": p.width, "height": p.height}
                           for p in self.paragraphs],
            "elements": [e.to_dict() for e in self.elements],
            "description": self.description,
            "raw_text": self.raw_text,
            "screen_type": self.screen_type,
            "ocr_method": self.ocr_method,
        }
        return d

    def to_prompt(self) -> str:
        """Format as a prompt for an AI agent."""
        parts = ["Current screen state:"]
        if self.screen_type:
            parts.append(f"  Screen type: {self.screen_type}")
        if self.description:
            parts.append(f"  Description: {self.description}")
        if self.paragraphs:
            parts.append("  Text on screen:")
            for p in self.paragraphs[:20]:
                parts.append(f"    \"{p.text}\" at ({p.x},{p.y})")
        elif self.raw_text:
            parts.append(f"  Text on screen: {self.raw_text[:500]}")
        if self.elements:
            parts.append("  UI elements:")
            for el in self.elements:
                parts.append(f"    [{el.element_type}] \"{el.text}\" at ({el.x},{el.y}) "
                             f"size {el.width}x{el.height}"
                             f"{' (clickable)' if el.clickable else ''}")
                for child in el.children:
                    parts.append(f"      [{child.element_type}] \"{child.text}\" "
                                 f"at ({child.x},{child.y})"
                                 f"{' (clickable)' if child.clickable else ''}")
        return "\n".join(parts)


class ScreenReader:
    """
    Advanced screen reader with multi-strategy OCR and vision AI.

    Automatically detects screen type (BIOS/terminal vs GUI, light vs dark)
    and applies the best OCR + element detection strategy:
      - Bitmap font matching for terminal/BIOS screens (<2ms)
      - Multi-pass Tesseract with preprocessing for GUI screens
      - OmniParser / YOLO / Ollama / Connect for AI-powered element detection
      - Heuristic edge detection as baseline fallback
    """

    def __init__(self, vision_manager: Any = None) -> None:
        self._tesseract_available = self._check_tesseract()
        self._text_capture = self._load_text_capture()
        self._vision = vision_manager  # VisionProviderManager (optional)

    def _check_tesseract(self) -> bool:
        import shutil
        return bool(shutil.which("tesseract"))

    def _load_text_capture(self):
        try:
            from text_capture import TextCapture
            return TextCapture()
        except ImportError:
            return None

    async def read_screen(self, image: Any, use_ai: bool = False,
                           level: str = "auto") -> ScreenState:
        """
        Analyse a screen image and return structured understanding.

        image: PIL Image, numpy array, or file path
        use_ai: if True, also generate an AI description (Level 4)
        level: "auto", "bitmap", "tesseract", "elements", "ai_vision"
        """
        import numpy as np
        from PIL import Image

        if isinstance(image, str):
            img = Image.open(image)
        elif isinstance(image, np.ndarray):
            img = Image.fromarray(image[:, :, :3] if image.ndim == 3 and image.shape[2] == 4 else image)
        else:
            img = image

        arr = np.array(img)
        state = ScreenState()

        # Auto-detect screen type
        state.screen_type = self._detect_screen_type(arr)

        # Level 1: Try bitmap font OCR for terminal/BIOS
        # Only use if explicitly requested OR if auto-detected AND high confidence
        if level == "bitmap" or (level == "auto" and state.screen_type in ("terminal", "bios")):
            regions = self._bitmap_ocr(arr, state)
            # Only trust bitmap results if confidence is high and text looks valid
            avg_conf = (sum(r.confidence for r in regions) / len(regions)) if regions else 0
            printable_ratio = 0
            if regions:
                all_text = " ".join(r.text for r in regions)
                printable = sum(1 for c in all_text if c.isalnum() or c in " .,:;!?-=+/\\()[]{}@#$%&*_")
                printable_ratio = printable / max(len(all_text), 1)

            if regions and avg_conf > 0.6 and printable_ratio > 0.5:
                state.text_regions = regions
                state.raw_text = " ".join(tr.text for tr in regions)
                state.ocr_method = "bitmap"
                state.lines, state.paragraphs = self._group_text(state.text_regions)
                if level == "bitmap":
                    return state
            elif level == "auto":
                # Bitmap results were garbage — force Tesseract
                regions = []
                state.screen_type = "gui_dark" if state.screen_type == "terminal" else state.screen_type

        # Level 2: Multi-pass Tesseract OCR
        if level in ("auto", "tesseract", "elements") and (
            not state.text_regions or state.screen_type.startswith("gui")
        ):
            regions = await self._advanced_ocr(img, arr, state.screen_type)
            if regions:
                # Merge with any bitmap results (bitmap takes priority for overlaps)
                if state.text_regions:
                    regions = self._merge_ocr_results(state.text_regions, regions)
                state.text_regions = regions
                state.raw_text = " ".join(tr.text for tr in regions)
                state.ocr_method = "multi_pass" if state.ocr_method else "tesseract"
            state.lines, state.paragraphs = self._group_text(state.text_regions)

        # Level 3: UI element detection
        # Try AI vision provider first (OmniParser, YOLO, Ollama, Connect)
        if self._vision and level in ("auto", "elements", "ai_vision"):
            vision_result = await self._vision.detect(img)
            if vision_result.elements:
                from vision_providers import DetectedElement
                for ve in vision_result.elements:
                    state.elements.append(UIElement(
                        element_type=ve.element_type,
                        text=ve.text or ve.description,
                        x=ve.x, y=ve.y,
                        width=ve.width, height=ve.height,
                        clickable=ve.clickable,
                    ))
                state.ocr_method += f"+{vision_result.provider}"

        # Always run heuristic detection to catch things vision models miss
        heuristic_elements = self._detect_elements(img, state.text_regions)
        # Merge: vision provider elements take priority, add non-overlapping heuristic ones
        if state.elements:
            for he in heuristic_elements:
                overlaps = False
                for ve in state.elements:
                    ox = max(0, min(he.x + he.width, ve.x + ve.width) - max(he.x, ve.x))
                    oy = max(0, min(he.y + he.height, ve.y + ve.height) - max(he.y, ve.y))
                    if ox * oy > he.width * he.height * 0.3:
                        overlaps = True
                        break
                if not overlaps:
                    state.elements.append(he)
        else:
            state.elements = heuristic_elements

        # Level 4: AI description
        if use_ai or level == "ai_vision":
            state.description = await self._ai_describe(state)

        return state

    async def read_node_screen(self, vnc_host: str, vnc_port: int,
                                use_ai: bool = False) -> ScreenState:
        """Take a VNC screenshot and analyse it."""
        import asyncvnc
        async with asyncvnc.connect(vnc_host, vnc_port) as client:
            frame = await client.screenshot()
            return await self.read_screen(frame, use_ai=use_ai)

    # ── Screen type detection ─────────────────────────────────────────

    def _detect_screen_type(self, arr: Any) -> str:
        """
        Detect whether this is a terminal/BIOS screen or a GUI.

        Samples multiple regions (center, corners, edges) to avoid
        being fooled by a single dark window on a light desktop.
        """
        import numpy as np
        h, w = arr.shape[:2]

        # Sample multiple regions
        regions = [
            arr[0:min(60, h), 0:min(w, w)],           # top bar (taskbar/menu)
            arr[max(0, h-60):h, 0:w],                  # bottom bar
            arr[h//4:3*h//4, w//4:3*w//4],             # center quadrant
            arr[0:h//4, 0:w//4],                       # top-left
            arr[0:h//4, 3*w//4:w],                     # top-right
        ]

        brightnesses = []
        color_counts = []
        for region in regions:
            if region.size == 0:
                continue
            avg = region.mean(axis=(0, 1))[:3]
            brightnesses.append(avg.mean())
            flat = region.reshape(-1, 3) // 32
            color_counts.append(len(set(map(tuple, flat[:1000]))))

        if not brightnesses:
            return "unknown"

        overall_brightness = sum(brightnesses) / len(brightnesses)
        max_brightness = max(brightnesses)
        min_brightness = min(brightnesses)
        overall_colors = max(color_counts) if color_counts else 0

        # Terminal: entire screen is uniformly dark with very few colors
        # AND no bright regions (no taskbar, no other windows)
        if max_brightness < 80 and overall_colors < 20 and (max_brightness - min_brightness) < 40:
            return "terminal"

        # BIOS: medium blue/gray background, uniformly colored, few UI elements
        center_avg = arr[h//4:3*h//4, w//4:3*w//4].mean(axis=(0, 1))[:3]
        if (overall_brightness < 120 and overall_colors < 40
            and center_avg[2] > center_avg[0] + 30  # blue-dominant
            and (max_brightness - min_brightness) < 60):
            return "bios"

        # GUI with taskbar: bright bar at top or bottom (Windows/macOS/Linux)
        has_bright_bar = (brightnesses[0] > 150 or brightnesses[1] > 150)

        # GUI dark: mostly dark but with bright elements (taskbar, windows)
        if overall_brightness < 100 and has_bright_bar:
            return "gui_dark"
        if overall_brightness < 80:
            return "gui_dark"

        # GUI light
        return "gui_light"

    # ── Level 1: Bitmap font OCR ──────────────────────────────────────

    def _bitmap_ocr(self, arr: Any, state: ScreenState) -> list[TextRegion]:
        """Try bitmap font matching (text_capture.py)."""
        if not self._text_capture:
            return []
        try:
            result = self._text_capture.recognise_frame(arr)
            if not result or not result.text.strip():
                return []

            regions = []
            for line_idx, line in enumerate(result.lines):
                if not line.strip():
                    continue
                # Each character is at a known grid position
                y = line_idx * result.cell_height
                # Find first and last non-space char for the region bounds
                stripped = line.rstrip()
                leading = len(line) - len(line.lstrip())
                x = leading * result.cell_width
                width = len(stripped.strip()) * result.cell_width
                regions.append(TextRegion(
                    text=stripped.strip(),
                    x=x, y=y,
                    width=max(width, 1),
                    height=result.cell_height,
                    confidence=result.confidence,
                ))
            return regions
        except Exception as e:
            log.debug("Bitmap OCR failed: %s", e)
            return []

    # ── Level 2: Advanced Tesseract OCR ───────────────────────────────

    async def _advanced_ocr(self, img: Any, arr: Any,
                              screen_type: str) -> list[TextRegion]:
        """
        Multi-pass Tesseract with preprocessing.

        Strategy:
        1. Upscale small images (< 1280px wide) — Tesseract works best at ~300 DPI
        2. For dark backgrounds: invert + threshold before OCR
        3. For light backgrounds: standard adaptive threshold
        4. Run Tesseract in multiple PSM modes:
           - PSM 3 (auto page segmentation) — best for documents
           - PSM 6 (single block) — catches text Tesseract misses in auto mode
           - PSM 11 (sparse text) — for scattered labels/buttons
        5. Merge all results, deduplicate overlapping regions
        """
        from PIL import Image, ImageEnhance, ImageFilter
        import numpy as np

        w, h = img.size
        scale = 1.0

        # Upscale small images
        if w < 1280:
            scale = 2.0
            img = img.resize((int(w * scale), int(h * scale)), Image.LANCZOS)

        all_regions: list[TextRegion] = []

        # ── Pass 1: Contrast-enhanced, PSM 3 (auto page segmentation) ──
        # This is the primary pass — handles most text
        enhanced = ImageEnhance.Contrast(img).enhance(1.3)
        enhanced = ImageEnhance.Sharpness(enhanced).enhance(1.3)
        regions = await self._tesseract_pass(enhanced, psm=3, scale=scale)
        all_regions.extend(regions)

        # ── Pass 2: For dark backgrounds, invert + threshold ──────
        # Only if we're on a dark screen — catches light-on-dark text
        if screen_type in ("gui_dark", "terminal", "bios"):
            gray = img.convert("L")
            inverted = Image.eval(gray, lambda x: 255 - x)
            inverted = ImageEnhance.Contrast(inverted).enhance(2.0)
            regions = await self._tesseract_pass(inverted, psm=6, scale=scale)
            all_regions.extend(regions)

        # ── Pass 3: Sparse text mode for scattered labels/buttons ──
        # Only if pass 1 found little text — avoid redundant work
        if len(all_regions) < 10:
            regions = await self._tesseract_pass(enhanced, psm=11, scale=scale)
            all_regions.extend(regions)

        # Merge and deduplicate
        merged = self._deduplicate_regions(all_regions)
        return merged

    async def _tesseract_pass(self, img: Any, psm: int = 3,
                                scale: float = 1.0,
                                lang: str = "eng") -> list[TextRegion]:
        """Single Tesseract pass with specific settings."""
        try:
            import pytesseract
            config = f"--psm {psm} -l {lang}"
            data = pytesseract.image_to_data(
                img, output_type=pytesseract.Output.DICT, config=config
            )
            regions = []
            n = len(data["text"])
            for i in range(n):
                text = data["text"][i].strip()
                conf = int(data["conf"][i]) if data["conf"][i] != "-1" else 0
                if text and conf > 25:
                    regions.append(TextRegion(
                        text=text,
                        x=int(data["left"][i] / scale),
                        y=int(data["top"][i] / scale),
                        width=max(1, int(data["width"][i] / scale)),
                        height=max(1, int(data["height"][i] / scale)),
                        confidence=conf / 100.0,
                    ))
            return regions
        except ImportError:
            return await self._basic_ocr_pass(img, psm, scale)
        except Exception as e:
            log.debug("Tesseract pass PSM %d failed: %s", psm, e)
            return []

    async def _basic_ocr_pass(self, img: Any, psm: int = 3,
                                scale: float = 1.0) -> list[TextRegion]:
        """Fallback: tesseract CLI directly."""
        import subprocess
        import tempfile
        from pathlib import Path

        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            img.save(f.name)
            try:
                result = subprocess.run(
                    ["tesseract", f.name, "-", f"--psm", str(psm),
                     "-l", "eng", "tsv"],
                    capture_output=True, text=True, timeout=30,
                )
                regions = []
                for line in result.stdout.splitlines()[1:]:
                    parts = line.split("\t")
                    if len(parts) >= 12:
                        text = parts[11].strip()
                        conf = int(parts[10]) if parts[10] != "-1" else 0
                        if text and conf > 25:
                            regions.append(TextRegion(
                                text=text,
                                x=int(int(parts[6]) / scale),
                                y=int(int(parts[7]) / scale),
                                width=max(1, int(int(parts[8]) / scale)),
                                height=max(1, int(int(parts[9]) / scale)),
                                confidence=conf / 100.0,
                            ))
                return regions
            except Exception:
                return []
            finally:
                Path(f.name).unlink(missing_ok=True)

    async def _color_split_ocr(self, img: Any, arr: Any | None,
                                  scale: float) -> list[TextRegion]:
        """
        Split image into light-bg and dark-bg regions, OCR each.

        This catches text that standard Tesseract misses because of
        mixed backgrounds (e.g. dark toolbar + light content area).
        """
        import numpy as np
        from PIL import Image

        pil_arr = np.array(img)
        gray = np.mean(pil_arr[:, :, :3], axis=2)
        h, w = gray.shape
        regions = []

        # Find dark and light regions using block analysis
        block = 64
        for by in range(0, h - block, block):
            for bx in range(0, w - block, block):
                block_mean = gray[by:by + block, bx:bx + block].mean()

                if block_mean < 80:
                    # Dark block — try inverting and OCR-ing just this area
                    crop = img.crop((bx, by, min(bx + block * 3, w), min(by + block * 2, h)))
                    # Invert for dark backgrounds
                    crop_arr = np.array(crop)
                    inverted = Image.fromarray(255 - crop_arr)
                    try:
                        import pytesseract
                        data = pytesseract.image_to_data(
                            inverted, output_type=pytesseract.Output.DICT,
                            config="--psm 7 -l eng"
                        )
                        for i in range(len(data["text"])):
                            text = data["text"][i].strip()
                            conf = int(data["conf"][i]) if data["conf"][i] != "-1" else 0
                            if text and conf > 40 and len(text) > 1:
                                regions.append(TextRegion(
                                    text=text,
                                    x=int((bx + data["left"][i]) / scale),
                                    y=int((by + data["top"][i]) / scale),
                                    width=max(1, int(data["width"][i] / scale)),
                                    height=max(1, int(data["height"][i] / scale)),
                                    confidence=conf / 100.0,
                                ))
                    except Exception:
                        pass

        return regions

    # ── Text grouping ─────────────────────────────────────────────────

    def _group_text(self, regions: list[TextRegion]) -> tuple[list[TextLine], list[TextParagraph]]:
        """Group word-level TextRegions into lines and paragraphs."""
        if not regions:
            return [], []

        # Sort by Y then X
        sorted_regions = sorted(regions, key=lambda r: (r.y, r.x))

        # Group into lines: words within similar Y range
        lines: list[TextLine] = []
        current_line_words: list[TextRegion] = []
        current_y = -1

        for tr in sorted_regions:
            if current_y < 0 or abs(tr.y - current_y) > max(tr.height * 0.6, 8):
                # New line
                if current_line_words:
                    lines.append(self._make_line(current_line_words))
                current_line_words = [tr]
                current_y = tr.y
            else:
                current_line_words.append(tr)

        if current_line_words:
            lines.append(self._make_line(current_line_words))

        # Group lines into paragraphs: adjacent lines with small vertical gap
        paragraphs: list[TextParagraph] = []
        current_para_lines: list[TextLine] = []

        for line in lines:
            if current_para_lines:
                prev = current_para_lines[-1]
                gap = line.y - (prev.y + prev.height)
                # Same paragraph if gap is small (< 1.5× line height)
                if gap < prev.height * 1.5 and abs(line.x - prev.x) < 100:
                    current_para_lines.append(line)
                    continue

            if current_para_lines:
                paragraphs.append(self._make_paragraph(current_para_lines))
            current_para_lines = [line]

        if current_para_lines:
            paragraphs.append(self._make_paragraph(current_para_lines))

        return lines, paragraphs

    @staticmethod
    def _make_line(words: list[TextRegion]) -> TextLine:
        sorted_words = sorted(words, key=lambda w: w.x)
        text = " ".join(w.text for w in sorted_words)
        x = sorted_words[0].x
        y = min(w.y for w in sorted_words)
        right = max(w.x + w.width for w in sorted_words)
        bottom = max(w.y + w.height for w in sorted_words)
        return TextLine(text=text, x=x, y=y, width=right - x, height=bottom - y, words=list(sorted_words))

    @staticmethod
    def _make_paragraph(lines: list[TextLine]) -> TextParagraph:
        text = "\n".join(l.text for l in lines)
        x = min(l.x for l in lines)
        y = lines[0].y
        right = max(l.x + l.width for l in lines)
        bottom = lines[-1].y + lines[-1].height
        return TextParagraph(text=text, x=x, y=y, width=right - x, height=bottom - y, lines=list(lines))

    # ── Deduplication ─────────────────────────────────────────────────

    @staticmethod
    def _deduplicate_regions(regions: list[TextRegion]) -> list[TextRegion]:
        """
        Remove duplicate/overlapping text regions from multi-pass OCR.

        When two regions overlap significantly, keep the one with higher confidence.
        """
        if not regions:
            return []

        # Sort by confidence (highest first)
        sorted_r = sorted(regions, key=lambda r: r.confidence, reverse=True)
        kept: list[TextRegion] = []

        for r in sorted_r:
            # Check if this region overlaps with any already-kept region
            overlaps = False
            for k in kept:
                # Calculate overlap
                ox = max(0, min(r.x + r.width, k.x + k.width) - max(r.x, k.x))
                oy = max(0, min(r.y + r.height, k.y + k.height) - max(r.y, k.y))
                overlap_area = ox * oy
                r_area = max(r.width * r.height, 1)

                if overlap_area > r_area * 0.5:
                    # Significant overlap — skip this one (lower confidence)
                    # But if the text is different, it might be a real addition
                    if r.text.lower().strip() == k.text.lower().strip():
                        overlaps = True
                        break
                    # Different text in same area — keep both if not too similar
                    if overlap_area > r_area * 0.8:
                        overlaps = True
                        break

            if not overlaps:
                kept.append(r)

        return kept

    def _merge_ocr_results(self, bitmap_regions: list[TextRegion],
                            tess_regions: list[TextRegion]) -> list[TextRegion]:
        """Merge bitmap OCR (high priority) with Tesseract results."""
        # Bitmap results are authoritative in their covered area
        merged = list(bitmap_regions)
        for tr in tess_regions:
            # Only add Tesseract result if it doesn't overlap bitmap results
            overlaps = False
            for br in bitmap_regions:
                ox = max(0, min(tr.x + tr.width, br.x + br.width) - max(tr.x, br.x))
                oy = max(0, min(tr.y + tr.height, br.y + br.height) - max(tr.y, br.y))
                if ox * oy > tr.width * tr.height * 0.3:
                    overlaps = True
                    break
            if not overlaps:
                merged.append(tr)
        return merged

    # ── Level 2: UI element detection ─────────────────────────────────

    def _detect_elements(self, img: Any, text_regions: list[TextRegion]) -> list[UIElement]:
        """
        Detect UI elements from the image + OCR data.

        Uses a combination of:
        1. Text-based heuristics (button labels, dialog titles, error keywords)
        2. Rectangle detection via edge analysis (buttons, dialogs, fields)
        3. Layout grouping (text + rectangle → labeled element)
        """
        import numpy as np
        arr = np.array(img)
        elements = []

        h, w = arr.shape[:2]
        gray = np.mean(arr[:, :, :3], axis=2).astype(np.uint8)

        # ── Rectangle detection via horizontal/vertical edge runs ─────
        rects = self._detect_rectangles(gray)

        # ── Button detection: keyword match + rectangle containment ───
        _BUTTON_WORDS = {
            "ok", "cancel", "next", "back", "yes", "no", "install", "close",
            "apply", "browse", "finish", "retry", "skip", "accept", "save",
            "delete", "remove", "add", "edit", "open", "send", "submit",
            "continue", "done", "start", "stop", "restart", "reboot",
            "shutdown", "sign in", "log in", "log out", "connect",
            "disconnect", "enable", "disable", "allow", "deny", "agree",
        }

        for tr in text_regions:
            text_lower = tr.text.lower().strip()
            # Check if this text is a known button label
            is_button = text_lower in _BUTTON_WORDS
            # Or a short text (1-3 words) inside a detected rectangle
            if not is_button and len(text_lower.split()) <= 3 and tr.width < 250:
                for r in rects:
                    if (r[0] <= tr.x <= r[0] + r[2] and
                        r[1] <= tr.y <= r[1] + r[3] and
                        r[2] < 300 and r[3] < 60):
                        is_button = True
                        break

            if is_button:
                elements.append(UIElement(
                    element_type="button",
                    text=tr.text,
                    x=tr.x - 10, y=tr.y - 5,
                    width=tr.width + 20, height=tr.height + 10,
                    clickable=True,
                ))

        # ── Dialog detection: title text near top + large rectangle ───
        _DIALOG_TITLES = {
            "windows setup", "error", "warning", "information", "confirm",
            "setup", "install", "properties", "settings", "preferences",
            "about", "help", "dialog", "alert", "notification",
            "user account control", "windows security", "open",
            "save as", "print", "run",
        }

        for tr in text_regions:
            text_lower = tr.text.lower().strip()
            if tr.y < h // 2 and text_lower in _DIALOG_TITLES:
                dialog = UIElement(
                    element_type="dialog",
                    text=tr.text,
                    x=max(0, tr.x - 50), y=max(0, tr.y - 30),
                    width=min(w, tr.width + 400), height=min(h, 300),
                )
                for el in elements:
                    if (el.element_type == "button" and
                        dialog.x <= el.x <= dialog.x + dialog.width and
                        dialog.y <= el.y <= dialog.y + dialog.height):
                        dialog.children.append(el)
                elements.append(dialog)

        # ── Input field detection: long thin rectangles with text cursor ─
        for r in rects:
            rx, ry, rw, rh = r
            if rw > 100 and 15 < rh < 40 and rw / rh > 4:
                # Looks like a text input field
                # Check if there's text inside or nearby
                label = ""
                for tr in text_regions:
                    if abs(tr.y - ry) < 30 and tr.x < rx:
                        label = tr.text
                        break
                elements.append(UIElement(
                    element_type="text_field",
                    text=label,
                    x=rx, y=ry, width=rw, height=rh,
                    clickable=True,
                ))

        # ── Checkbox detection: small squares near text ──────────────
        for r in rects:
            rx, ry, rw, rh = r
            if 10 < rw < 25 and 10 < rh < 25 and abs(rw - rh) < 5:
                # Small square — likely a checkbox
                label = ""
                for tr in text_regions:
                    if abs(tr.y - ry) < 10 and tr.x > rx:
                        label = tr.text
                        break
                if label:
                    elements.append(UIElement(
                        element_type="checkbox",
                        text=label,
                        x=rx, y=ry, width=rw, height=rh,
                        clickable=True,
                    ))

        # ── Error message detection ──────────────────────────────────
        _ERROR_WORDS = ("error", "could not", "cannot", "failed", "not found",
                        "access denied", "permission", "fatal", "exception",
                        "crash", "bsod", "blue screen", "kernel panic",
                        "segfault", "timeout", "refused", "unavailable")
        for tr in text_regions:
            if any(word in tr.text.lower() for word in _ERROR_WORDS):
                elements.append(UIElement(
                    element_type="error_message",
                    text=tr.text,
                    x=tr.x, y=tr.y,
                    width=tr.width, height=tr.height,
                ))

        # ── Link detection: underlined or blue text ──────────────────
        for tr in text_regions:
            # Check if the text region has blue-ish pixels
            if tr.y < h and tr.x < w:
                region = arr[tr.y:min(tr.y + tr.height, h),
                             tr.x:min(tr.x + tr.width, w)]
                if region.size > 0:
                    avg_color = region.mean(axis=(0, 1))
                    # Blue-dominant text (links)
                    if len(avg_color) >= 3 and avg_color[2] > 150 and avg_color[0] < 100:
                        elements.append(UIElement(
                            element_type="link",
                            text=tr.text,
                            x=tr.x, y=tr.y,
                            width=tr.width, height=tr.height,
                            clickable=True,
                        ))

        return elements

    def _detect_rectangles(self, gray: Any) -> list[tuple[int, int, int, int]]:
        """
        Detect rectangles in a grayscale image using edge analysis.

        Returns list of (x, y, width, height) tuples.
        """
        import numpy as np
        h, w = gray.shape

        # Compute gradients
        gx = np.abs(np.diff(gray.astype(np.int16), axis=1))
        gy = np.abs(np.diff(gray.astype(np.int16), axis=0))

        # Threshold for edges
        thresh = 30
        h_edges = gy > thresh  # horizontal edges
        v_edges = gx > thresh  # vertical edges

        # Find horizontal edge runs (potential top/bottom of rectangles)
        h_runs = []
        for y in range(0, h - 1, 2):
            run_start = None
            for x in range(w):
                if x < h_edges.shape[1] and h_edges[y, x]:
                    if run_start is None:
                        run_start = x
                else:
                    if run_start is not None and x - run_start > 20:
                        h_runs.append((run_start, y, x - run_start))
                    run_start = None

        # Find vertical edge runs
        v_runs = []
        for x in range(0, w - 1, 2):
            run_start = None
            for y in range(h):
                if y < v_edges.shape[0] and v_edges[y, x]:
                    if run_start is None:
                        run_start = y
                else:
                    if run_start is not None and y - run_start > 10:
                        v_runs.append((x, run_start, y - run_start))
                    run_start = None

        # Match horizontal pairs (top + bottom) with vertical pairs (left + right)
        # to form rectangles. This is a simplified approach — look for H runs
        # at similar X ranges with V runs connecting them.
        rects = []
        tolerance = 10

        for i, (hx1, hy1, hw1) in enumerate(h_runs):
            for hx2, hy2, hw2 in h_runs[i + 1:]:
                # Top and bottom edges should be at similar X, different Y
                if (abs(hx1 - hx2) < tolerance and
                    abs(hw1 - hw2) < tolerance * 2 and
                    20 < abs(hy2 - hy1) < 500):
                    rect_h = abs(hy2 - hy1)
                    rect_y = min(hy1, hy2)
                    rects.append((hx1, rect_y, hw1, rect_h))

        # Deduplicate overlapping rectangles
        if rects:
            rects = self._merge_rects(rects)

        return rects[:50]  # cap at 50 to avoid noise

    @staticmethod
    def _merge_rects(rects: list[tuple[int, int, int, int]]) -> list[tuple[int, int, int, int]]:
        """Merge overlapping rectangles."""
        merged = []
        used = set()
        for i, (x1, y1, w1, h1) in enumerate(rects):
            if i in used:
                continue
            # Check for overlaps
            for j, (x2, y2, w2, h2) in enumerate(rects[i + 1:], i + 1):
                if j in used:
                    continue
                # Check overlap
                if (x1 < x2 + w2 and x1 + w1 > x2 and
                    y1 < y2 + h2 and y1 + h1 > y2):
                    # Merge: take the larger one
                    if w2 * h2 > w1 * h1:
                        x1, y1, w1, h1 = x2, y2, w2, h2
                    used.add(j)
            merged.append((x1, y1, w1, h1))
        return merged

    # ── Level 2.5: Vector/line extraction ─────────────────────────────

    def extract_vectors(self, img: Any) -> dict:
        """
        Extract structural vectors from the image: rectangles, lines, text boxes.

        This gives AI agents a lightweight structural understanding of the screen
        without needing a full vision model. Useful for:
        - Detecting window/dialog boundaries
        - Finding buttons by their outlines
        - Identifying form layouts
        - Tracking UI changes between frames
        """
        import numpy as np
        arr = np.array(img)
        h, w = arr.shape[:2]
        gray = np.mean(arr[:, :, :3], axis=2).astype(np.uint8)

        # Detect rectangles
        rects = self._detect_rectangles(gray)

        # Classify rectangles by size
        result: dict = {
            "width": w, "height": h,
            "rectangles": [],
            "lines": [],
        }

        for rx, ry, rw, rh in rects:
            # Sample border and fill colors
            border_color = ""
            fill_color = ""
            try:
                # Border: average of top edge pixels
                if ry < h and rx < w:
                    border_px = arr[ry, rx:min(rx + rw, w)]
                    if len(border_px) > 0:
                        bc = border_px.mean(axis=0)[:3].astype(int)
                        border_color = f"#{bc[0]:02x}{bc[1]:02x}{bc[2]:02x}"
                # Fill: average of center region
                cy, cx = ry + rh // 2, rx + rw // 2
                if 0 <= cy < h and 0 <= cx < w:
                    fill_region = arr[max(0, cy - 5):min(h, cy + 5),
                                      max(0, cx - 5):min(w, cx + 5)]
                    if fill_region.size > 0:
                        fc = fill_region.mean(axis=(0, 1))[:3].astype(int)
                        fill_color = f"#{fc[0]:02x}{fc[1]:02x}{fc[2]:02x}"
            except Exception:
                pass

            # Classify
            if rw > w * 0.5 and rh > h * 0.3:
                rtype = "window"
            elif rw > 200 and rh > 100:
                rtype = "panel"
            elif rw > 50 and rh < 40:
                rtype = "button"
            elif rw < 30 and rh < 30:
                rtype = "checkbox"
            else:
                rtype = "rectangle"

            result["rectangles"].append({
                "type": rtype,
                "x": int(rx), "y": int(ry),
                "width": int(rw), "height": int(rh),
                "border_color": border_color,
                "fill_color": fill_color,
            })

        return result

    # ── Level 3: AI description ───────────────────────────────────────

    async def _ai_describe(self, state: ScreenState) -> str:
        """Use an AI model to describe what's on screen."""
        prompt = (
            "Describe this screen state concisely. What application is showing? "
            "What is the current state? What action should be taken?\n\n"
            f"Text visible: {state.raw_text[:1000]}\n"
            f"UI elements: {[e.to_dict() for e in state.elements[:10]]}"
        )

        # Try local Ollama first
        try:
            import urllib.request
            import json
            data = json.dumps({
                "model": "llama3.1:8b",
                "prompt": prompt,
                "stream": False,
            }).encode()
            req = urllib.request.Request(
                "http://localhost:11434/api/generate",
                data=data,
                headers={"Content-Type": "application/json"},
            )
            with urllib.request.urlopen(req, timeout=30) as r:
                result = json.loads(r.read())
                return result.get("response", "")
        except Exception:
            pass

        # Fallback: simple rule-based description
        desc_parts = []
        for el in state.elements:
            if el.element_type == "dialog":
                desc_parts.append(f"A '{el.text}' dialog is open.")
            elif el.element_type == "error_message":
                desc_parts.append(f"Error: {el.text}")
            elif el.element_type == "button":
                desc_parts.append(f"Button '{el.text}' is available.")
        return " ".join(desc_parts) if desc_parts else state.raw_text[:200]
