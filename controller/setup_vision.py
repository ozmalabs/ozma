#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Download and setup vision models for ozma's AI agent screen control.

Usage:
  python3 controller/setup_vision.py          # Download all models
  python3 controller/setup_vision.py --check  # Check what's available
  python3 controller/setup_vision.py --omniparser  # OmniParser only
  python3 controller/setup_vision.py --ollama      # Pull Ollama vision model

Models downloaded to ~/.cache/ozma/omniparser/ (~2GB total):
  - icon_detect/best.pt        — YOLOv8 fine-tuned for UI elements (~6MB)
  - icon_caption/               — Florence-2 for element description (~500MB)

Requires: uv pip install ultralytics torch huggingface_hub
Optional: uv pip install transformers (for Florence-2 icon captioning)
"""

import argparse
import shutil
import sys
from pathlib import Path

CACHE_DIR = Path.home() / ".cache" / "ozma" / "omniparser"


def check_deps() -> dict[str, bool]:
    """Check available dependencies."""
    deps = {}
    try:
        import torch
        deps["torch"] = True
        deps["cuda"] = torch.cuda.is_available()
        if deps["cuda"]:
            deps["gpu"] = torch.cuda.get_device_name(0)
    except ImportError:
        deps["torch"] = False
        deps["cuda"] = False

    try:
        import ultralytics
        deps["ultralytics"] = True
    except ImportError:
        deps["ultralytics"] = False

    try:
        import transformers
        deps["transformers"] = True
    except ImportError:
        deps["transformers"] = False

    try:
        import huggingface_hub
        deps["huggingface_hub"] = True
    except ImportError:
        deps["huggingface_hub"] = False

    deps["tesseract"] = bool(shutil.which("tesseract"))
    deps["ollama"] = bool(shutil.which("ollama"))

    return deps


def check_models() -> dict[str, bool]:
    """Check which models are downloaded."""
    models = {}
    models["yolo_icon_detect"] = (
        (CACHE_DIR / "icon_detect" / "model.pt").exists() or
        (CACHE_DIR / "icon_detect" / "best.pt").exists()
    )
    models["florence2"] = (
        (CACHE_DIR / "icon_caption" / "config.json").exists() or
        (CACHE_DIR / "icon_caption_florence" / "config.json").exists()
    )
    return models


def download_omniparser():
    """Download OmniParser models from HuggingFace."""
    try:
        from huggingface_hub import snapshot_download
    except ImportError:
        print("ERROR: uv pip install huggingface_hub first")
        sys.exit(1)

    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    print(f"Downloading OmniParser v2 to {CACHE_DIR}...")

    # Download icon detection model (YOLO weights)
    print("  Downloading icon detection model (YOLOv8)...")
    snapshot_download(
        repo_id="microsoft/OmniParser-v2.0",
        local_dir=str(CACHE_DIR),
        allow_patterns=["icon_detect/*"],
    )
    print("  ✓ Icon detection model ready")

    # Download Florence-2 caption model (optional, larger)
    print("  Downloading Florence-2 icon captioning model...")
    try:
        snapshot_download(
            repo_id="microsoft/OmniParser-v2.0",
            local_dir=str(CACHE_DIR),
            allow_patterns=["icon_caption/*"],
        )
        print("  ✓ Florence-2 model ready")
    except Exception as e:
        print(f"  ⚠ Florence-2 download failed (optional): {e}")

    print(f"\nModels saved to {CACHE_DIR}")


def pull_ollama_vision():
    """Pull a vision model for Ollama."""
    import subprocess
    ollama = shutil.which("ollama")
    if not ollama:
        print("ERROR: Ollama not installed. Install from https://ollama.ai")
        sys.exit(1)

    model = "qwen2.5vl:7b"
    print(f"Pulling Ollama model: {model}")
    subprocess.run([ollama, "pull", model], check=True)
    print(f"✓ {model} ready")


def main():
    parser = argparse.ArgumentParser(description="Setup vision models for ozma")
    parser.add_argument("--check", action="store_true", help="Check available deps and models")
    parser.add_argument("--omniparser", action="store_true", help="Download OmniParser models")
    parser.add_argument("--ollama", action="store_true", help="Pull Ollama vision model")
    args = parser.parse_args()

    if args.check or not (args.omniparser or args.ollama):
        print("=== Dependencies ===")
        deps = check_deps()
        for k, v in deps.items():
            status = f"✓ {v}" if isinstance(v, str) else ("✓" if v else "✗")
            print(f"  {k}: {status}")

        print("\n=== Models ===")
        models = check_models()
        for k, v in models.items():
            print(f"  {k}: {'✓ downloaded' if v else '✗ not found'}")

        if not args.check:
            print("\nTo download models:")
            print("  python3 controller/setup_vision.py --omniparser")
            print("  python3 controller/setup_vision.py --ollama")
        return

    if args.omniparser:
        download_omniparser()

    if args.ollama:
        pull_ollama_vision()


if __name__ == "__main__":
    main()
