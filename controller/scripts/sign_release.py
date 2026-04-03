#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Sign an OTA artifact locally using a GPG-encrypted signing key.

The private key is stored GPG-encrypted (e.g. encrypted to your YubiKey's
OpenPGP subkey). Signing requires the physical YubiKey to be connected —
GPG will request PIN + touch to decrypt the key, then PyNaCl signs the
artifact. The output .sig format is identical to what the CI job produces
and what update_manager.py verifies.

Setup (one-time):
    # 1. Generate the OTA keypair
    python controller/scripts/generate_ota_key.py --out /tmp/ozma-ota

    # 2. Encrypt the private key to your YubiKey's GPG key
    gpg --encrypt --recipient YOUR_KEY_ID /tmp/ozma-ota.key
    #   → /tmp/ozma-ota.key.gpg

    # 3. Store the encrypted key somewhere durable (see docs/ota-key-management.md)
    #    e.g. internal repo, encrypted backup drive, etc.

    # 4. Delete the plaintext private key
    rm /tmp/ozma-ota.key

Usage:
    python controller/scripts/sign_release.py \\
        --key /path/to/ozma-ota.key.gpg \\
        --artifact dist/ozma-controller-v1.2.0.whl

    # Sign multiple artifacts:
    python controller/scripts/sign_release.py \\
        --key /path/to/ozma-ota.key.gpg \\
        --artifact dist/*.whl

    # Verify a signature (no key needed):
    python controller/scripts/sign_release.py \\
        --verify --pubkey controller/ota_signing.pub \\
        --artifact dist/ozma-controller-v1.2.0.whl
"""
import argparse
import base64
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))


def _decrypt_key(encrypted_key_path: Path) -> bytes:
    """Decrypt the GPG-encrypted private key. Requires YubiKey touch + PIN."""
    result = subprocess.run(
        ["gpg", "--quiet", "--decrypt", str(encrypted_key_path)],
        capture_output=True,
    )
    if result.returncode != 0:
        stderr = result.stderr.decode(errors="replace").strip()
        print(f"ERROR: GPG decryption failed:\n{stderr}", file=sys.stderr)
        print("\nIs your YubiKey connected? Is the GPG agent running?", file=sys.stderr)
        print("Try: gpg --card-status", file=sys.stderr)
        sys.exit(1)
    # Output is the base64-encoded private key (with possible trailing newline)
    return base64.b64decode(result.stdout.strip())


def _sign_artifact(artifact: Path, private_bytes: bytes) -> Path:
    from transport import IdentityKeyPair
    kp = IdentityKeyPair.from_private_bytes(private_bytes)
    data = artifact.read_bytes()
    sig = kp.sign(data)
    sig_path = artifact.with_suffix(artifact.suffix + ".sig")
    sig_path.write_bytes(sig)
    return sig_path


def _verify_artifact(artifact: Path, pubkey_hex: str) -> bool:
    from transport import IdentityKeyPair
    public_key = bytes.fromhex(pubkey_hex.strip())
    sig_path = artifact.with_suffix(artifact.suffix + ".sig")
    if not sig_path.exists():
        print(f"ERROR: signature file not found: {sig_path}", file=sys.stderr)
        return False
    data = artifact.read_bytes()
    sig = sig_path.read_bytes()
    return IdentityKeyPair.verify(data, sig, public_key)


def main() -> None:
    ap = argparse.ArgumentParser(description="Sign or verify OTA artifacts")
    ap.add_argument("--artifact", required=True, nargs="+",
                    help="Artifact file(s) to sign or verify")
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--key", metavar="KEY_GPG",
                      help="Path to GPG-encrypted private key (.key.gpg)")
    mode.add_argument("--verify", action="store_true",
                      help="Verify existing .sig files instead of signing")
    ap.add_argument("--pubkey", metavar="PUB_HEX",
                    help="Public key hex file for --verify (default: controller/ota_signing.pub)")
    args = ap.parse_args()

    if not args.verify and not args.key:
        ap.error("Either --key (to sign) or --verify (to verify) is required")

    artifacts = [Path(a) for a in args.artifact]
    for a in artifacts:
        if not a.exists():
            print(f"ERROR: artifact not found: {a}", file=sys.stderr)
            sys.exit(1)

    if args.verify:
        pub_path = Path(args.pubkey) if args.pubkey else (
            Path(__file__).parent.parent / "ota_signing.pub"
        )
        if not pub_path.exists():
            print(f"ERROR: public key not found: {pub_path}", file=sys.stderr)
            sys.exit(1)
        pubkey_hex = pub_path.read_text().strip()
        ok = True
        for artifact in artifacts:
            if _verify_artifact(artifact, pubkey_hex):
                print(f"OK  {artifact.name}")
            else:
                print(f"FAIL  {artifact.name}")
                ok = False
        sys.exit(0 if ok else 1)

    # Signing path
    key_path = Path(args.key)
    if not key_path.exists():
        print(f"ERROR: key file not found: {key_path}", file=sys.stderr)
        sys.exit(1)

    print("OTA signing requires physical approval.")
    print("Connect your YubiKey now if not already inserted.")
    print("You will be prompted for PIN + physical touch.")
    print()
    private_bytes = _decrypt_key(key_path)

    for artifact in artifacts:
        sig_path = _sign_artifact(artifact, private_bytes)
        print(f"Signed  {artifact.name}  →  {sig_path.name}")

    # Zero out key bytes from memory as best-effort
    private_bytes = bytes(len(private_bytes))


if __name__ == "__main__":
    main()
