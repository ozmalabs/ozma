#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Generate an Ed25519 OTA signing keypair.

Usage (run from repo root or controller/):
    python controller/scripts/generate_ota_key.py --out keys/ota

Outputs:
    keys/ota.key   — private key (base64, 64 bytes)  — store in GitHub Secret OTA_SIGNING_KEY
    keys/ota.pub   — public key (hex, 32 bytes)       — committed to repo as controller/ota_signing.pub

The private key file must NEVER be committed to the repository.
"""
import argparse
import base64
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))


def main() -> None:
    ap = argparse.ArgumentParser(description="Generate OTA signing keypair")
    ap.add_argument("--out", required=True,
                    help="Output path prefix (e.g. keys/ota → keys/ota.key + keys/ota.pub)")
    args = ap.parse_args()

    try:
        from transport import IdentityKeyPair
    except ImportError as exc:
        print(f"ERROR: cannot import transport: {exc}", file=sys.stderr)
        print("Run from repo root: python controller/scripts/generate_ota_key.py --out keys/ota",
              file=sys.stderr)
        sys.exit(1)

    kp = IdentityKeyPair.generate()
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)

    priv_path = Path(str(out) + ".key")
    pub_path = Path(str(out) + ".pub")

    # Private key: base64-encoded 64 bytes (seed + public, libsodium format)
    priv_b64 = base64.b64encode(kp.private_key).decode()
    priv_path.write_text(priv_b64 + "\n")
    priv_path.chmod(0o600)

    # Public key: hex-encoded 32 bytes
    pub_hex = kp.public_key.hex()
    pub_path.write_text(pub_hex + "\n")

    print(f"Private key → {priv_path}")
    print(f"  Add content (without newline) to GitHub Secret: OTA_SIGNING_KEY")
    print(f"Public key  → {pub_path}")
    print(f"  Copy to controller/ota_signing.pub and commit.")
    print(f"Public key hex: {pub_hex}")


if __name__ == "__main__":
    main()
