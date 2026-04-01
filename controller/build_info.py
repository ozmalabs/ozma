# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Build information — version, signature, and edition detection.

Signed builds are produced by the Ozma Labs release pipeline. The
build hash covers the controller package contents, and the signature
is an Ed25519 detached signature over that hash. The public key is
embedded here — anyone can verify, only Ozma Labs can sign.

Edition labels:
  "open_source"  — no signature present (self-built from AGPL source)
  "free"         — signed by Ozma Labs, distributed from website/PyPI/GHCR
  "commercial"   — signed by Ozma Labs, commercial release channel

The edition affects nothing locally — all features work the same.
It's reported to Connect so the dashboard can display the correct
label and Connect can track which builds are in the field.
"""

from __future__ import annotations

VERSION = "1.0.0-dev"

# Ed25519 public key for Ozma Labs release signing.
# The private key is stored offline on an air-gapped machine.
# This key verifies that a build was produced by the official pipeline.
RELEASE_PUBLIC_KEY_HEX = ""  # Set during first real release

# Populated by CI during release builds. Empty = community/dev build.
BUILD_HASH = ""       # SHA-256 of the controller package
BUILD_SIGNATURE = ""  # Ed25519 signature of BUILD_HASH, hex-encoded
BUILD_CHANNEL = ""    # "stable", "beta", "nightly", "lts", or ""


def get_edition() -> str:
    """
    Determine the build edition from the signature state.

    Returns:
      "open_source"  — unsigned (self-built from source)
      "free"         — signed, free channel
      "commercial"   — signed, commercial channel
    """
    if not BUILD_SIGNATURE or not BUILD_HASH or not RELEASE_PUBLIC_KEY_HEX:
        return "open_source"

    if verify_signature():
        if BUILD_CHANNEL in ("stable", "lts"):
            return "commercial"
        return "free"

    # Signature present but invalid — treat as open source
    return "open_source"


def verify_signature() -> bool:
    """Verify the build signature against the release public key."""
    if not BUILD_SIGNATURE or not BUILD_HASH or not RELEASE_PUBLIC_KEY_HEX:
        return False

    try:
        from nacl.signing import VerifyKey
        vk = VerifyKey(bytes.fromhex(RELEASE_PUBLIC_KEY_HEX))
        vk.verify(BUILD_HASH.encode(), bytes.fromhex(BUILD_SIGNATURE))
        return True
    except Exception:
        return False


def build_info() -> dict:
    """Return build metadata for registration and status display."""
    return {
        "version": VERSION,
        "edition": get_edition(),
        "channel": BUILD_CHANNEL or "dev",
        "signed": bool(BUILD_SIGNATURE and verify_signature()),
        "build_hash": BUILD_HASH[:16] if BUILD_HASH else "",
    }
