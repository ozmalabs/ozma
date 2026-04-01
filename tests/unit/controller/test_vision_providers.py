"""Unit tests for vision providers — dataclasses and provider management."""

import pytest

pytestmark = pytest.mark.unit


class TestDetectedElement:
    def test_center(self):
        from vision_providers import DetectedElement
        el = DetectedElement(element_type="button", text="OK",
                              x=100, y=200, width=80, height=30)
        assert el.center == (140, 215)

    def test_to_dict(self):
        from vision_providers import DetectedElement
        el = DetectedElement(element_type="icon", text="Save",
                              x=10, y=20, width=32, height=32,
                              confidence=0.85, clickable=True)
        d = el.to_dict()
        assert d["type"] == "icon"
        assert d["text"] == "Save"
        assert d["confidence"] == 0.85
        assert d["clickable"] is True
        assert d["center"] == [26, 36]

    def test_default_not_clickable(self):
        from vision_providers import DetectedElement
        el = DetectedElement(element_type="label", text="Name:", x=0, y=0, width=50, height=15)
        assert not el.clickable
        assert not el.interactable


class TestVisionResult:
    def test_empty(self):
        from vision_providers import VisionResult
        r = VisionResult()
        assert r.elements == []
        assert r.provider == ""

    def test_to_dict(self):
        from vision_providers import VisionResult, DetectedElement
        el = DetectedElement(element_type="button", text="X", x=0, y=0, width=20, height=20)
        r = VisionResult(elements=[el], provider="omniparser", inference_ms=84.5)
        d = r.to_dict()
        assert len(d["elements"]) == 1
        assert d["provider"] == "omniparser"
        assert d["inference_ms"] == 84.5


class TestProviderManager:
    def test_create_default(self):
        from vision_providers import VisionProviderManager
        mgr = VisionProviderManager.create_default()
        providers = mgr.list_providers()
        assert len(providers) >= 1
        names = [p["name"] for p in providers]
        # At minimum, OmniParser server + in-process + YOLO + Ollama should be listed
        assert any("omniparser" in n for n in names)

    def test_empty_manager(self):
        from vision_providers import VisionProviderManager
        mgr = VisionProviderManager()
        assert mgr.list_providers() == []

    @pytest.mark.asyncio
    async def test_detect_with_no_providers(self):
        from vision_providers import VisionProviderManager
        mgr = VisionProviderManager()
        PIL = pytest.importorskip("PIL")
        from PIL import Image
        img = Image.new("RGB", (100, 100))
        result = await mgr.detect(img)
        assert result.elements == []

    def test_provider_availability(self):
        from vision_providers import VisionProviderManager
        mgr = VisionProviderManager.create_default()
        for p in mgr.list_providers():
            assert "name" in p
            assert "available" in p
            assert isinstance(p["available"], bool)


class TestOllamaProvider:
    def test_parse_elements_valid_json(self):
        from vision_providers import OllamaVisionProvider
        provider = OllamaVisionProvider()
        text = '[{"type": "button", "text": "OK", "bbox": [100, 200, 80, 30]}]'
        elements = provider._parse_elements(text, (1024, 768))
        assert len(elements) == 1
        assert elements[0].element_type == "button"
        assert elements[0].text == "OK"

    def test_parse_elements_no_json(self):
        from vision_providers import OllamaVisionProvider
        provider = OllamaVisionProvider()
        elements = provider._parse_elements("No JSON here", (1024, 768))
        assert elements == []

    def test_parse_elements_normalised_coords(self):
        from vision_providers import OllamaVisionProvider
        provider = OllamaVisionProvider()
        # Normalised 0-1 coordinates
        text = '[{"type": "icon", "bbox": [0.5, 0.5, 0.1, 0.1]}]'
        elements = provider._parse_elements(text, (1000, 1000))
        assert elements[0].x == 500
        assert elements[0].y == 500
