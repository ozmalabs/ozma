#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Operator tooling for display asset signing keys and payloads.

Commands
--------
``generate``       Create an Ed25519 key pair (private key mode 0600).
``fingerprint``    Print a key fingerprint.
``sign-index``     Sign a Connect asset index.
``verify-index``   Verify a signed index against a trust store.
``sign-manifest``  Sign a pack manifest.
``verify-manifest`` Verify a signed manifest and optional file hashes.

The private key is never printed.  Status output is deterministic JSON (except
``generate``/``fingerprint``, which print the fingerprint alone so it can be
captured without parsing).
"""

from __future__ import annotations

import argparse
import base64
import binascii
import json
import os
import sys
from pathlib import Path
from typing import Any, Iterable, Mapping

_CONTROLLER = Path(__file__).resolve().parents[1] / "controller"
if str(_CONTROLLER) not in sys.path:
    sys.path.insert(0, str(_CONTROLLER))

from asset_trust import (  # noqa: E402
    ED25519,
    ENV_VAR,
    TrustError,
    TrustStore,
    TrustedKey,
    canonical_index_payload,
    canonical_manifest_payload,
)

try:  # pragma: no cover - availability depends on the deployment
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import (
        Ed25519PrivateKey,
    )

    HAVE_ED25519 = True
except ImportError:  # pragma: no cover
    serialization = None  # type: ignore[assignment]
    Ed25519PrivateKey = None  # type: ignore[assignment]
    HAVE_ED25519 = False


class CliError(Exception):
    """A deterministic, user-facing failure with no secret material."""


# ── Small I/O helpers ───────────────────────────────────────────────────────


def _json_text(value: Any) -> str:
    return (
        json.dumps(
            value,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
            allow_nan=False,
        )
        + "\n"
    )


def _read_json_object(path: str | Path, *, label: str) -> dict[str, Any]:
    try:
        raw = Path(path).read_text(encoding="utf-8")
    except OSError as exc:
        raise CliError(f"cannot read {label}: {exc}") from None
    try:
        value = json.loads(raw)
    except ValueError as exc:
        raise CliError(f"{label} is not valid JSON: {exc}") from None
    if not isinstance(value, Mapping):
        raise CliError(f"{label} must contain a JSON object")
    return dict(value)


def _ensure_parent(path: Path) -> None:
    parent = path.parent
    if parent and not parent.exists():
        parent.mkdir(parents=True, exist_ok=True)


def _write_json(path: str | Path, value: Any) -> None:
    target = Path(path)
    _ensure_parent(target)
    temporary = target.with_name(f".{target.name}.{os.getpid()}.tmp")
    try:
        with open(temporary, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(_json_text(value))
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, target)
    finally:
        try:
            temporary.unlink()
        except OSError:
            pass


def _write_private(path: str | Path, data: bytes, *, force: bool) -> None:
    target = Path(path)
    _ensure_parent(target)
    if target.exists():
        if not force:
            raise CliError(f"refusing to overwrite existing file: {target}")
        target.unlink()
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    descriptor = os.open(target, flags, 0o600)
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "wb") as handle:
            descriptor = -1
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def _print_status(payload: Mapping[str, Any]) -> None:
    sys.stdout.write(_json_text(dict(payload)))


# ── Key loading ─────────────────────────────────────────────────────────────


def _require_ed25519() -> None:
    if not HAVE_ED25519 or Ed25519PrivateKey is None or serialization is None:
        raise CliError("Ed25519 operations require the 'cryptography' package")


def _decode_raw_private(data: bytes) -> bytes:
    if len(data) == 32 and b"-----BEGIN" not in data:
        return data
    try:
        text = data.decode("ascii", errors="strict").strip()
    except UnicodeError:
        raise CliError("private key is not valid PEM, base64, or hex") from None
    if not text:
        raise CliError("private key file is empty")
    lowered = text.lower()
    try:
        if lowered.startswith("hex:"):
            raw = bytes.fromhex(text[len("hex:"):].strip())
        elif lowered.startswith("base64:"):
            value = text[len("base64:"):].strip()
            raw = base64.b64decode(
                value + "=" * (-len(value) % 4), validate=True
            )
        elif len(text) % 2 == 0 and all(
            character in "0123456789abcdefABCDEF" for character in text
        ):
            raw = bytes.fromhex(text)
        else:
            raw = base64.b64decode(
                text + "=" * (-len(text) % 4), validate=True
            )
    except (binascii.Error, ValueError, UnicodeError):
        raise CliError("private key is not valid PEM, base64, or hex") from None
    if len(raw) != 32:
        raise CliError("raw Ed25519 private key must be 32 bytes")
    return raw


def _load_private_key(path: str | Path):
    _require_ed25519()
    try:
        data = Path(path).read_bytes()
    except OSError as exc:
        raise CliError(f"cannot read private key: {exc}") from None
    try:
        if b"-----BEGIN" in data:
            key = serialization.load_pem_private_key(data, password=None)
        else:
            key = Ed25519PrivateKey.from_private_bytes(
                _decode_raw_private(data)
            )
    except CliError:
        raise
    except (ValueError, TypeError, OSError):
        raise CliError("private key could not be loaded") from None
    if not isinstance(key, Ed25519PrivateKey):
        raise CliError("private key is not an Ed25519 key")
    return key


def _sign(private_key, payload: bytes) -> str:
    return base64.b64encode(private_key.sign(payload)).decode("ascii")


def _public_document(private_key, key_id: str) -> dict[str, Any]:
    public_raw = private_key.public_key().public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw,
    )
    return {
        "algorithm": ED25519,
        "comment": f"display signing key {key_id}",
        "key_id": key_id,
        "public_key": base64.b64encode(public_raw).decode("ascii"),
    }


# ── Trust store loading ─────────────────────────────────────────────────────


def _load_store(args: argparse.Namespace) -> TrustStore:
    trust = str(getattr(args, "trust", "") or "")
    trust_env = bool(getattr(args, "trust_env", False))
    if bool(trust) == trust_env:
        raise CliError("exactly one of --trust or --trust-env is required")
    if trust:
        return TrustStore.from_mapping(
            _read_json_object(trust, label="trust store")
        )
    try:
        return TrustStore.from_env()
    except TrustError as exc:
        raise CliError(str(exc)) from None


def _read_public_key(path: str | Path, *, algorithm: str = ED25519) -> TrustedKey:
    try:
        text = Path(path).read_text(encoding="utf-8").strip()
    except OSError as exc:
        raise CliError(f"cannot read public key: {exc}") from None
    if not text:
        raise CliError("public key file is empty")
    try:
        parsed = json.loads(text)
    except ValueError:
        parsed = None
    if parsed is not None:
        try:
            store = TrustStore.from_mapping(parsed)
            if store.default_key_id:
                return store.require(store.default_key_id)
            return _only_key(store)
        except TrustError as exc:
            raise CliError(str(exc)) from None
    return TrustedKey(key_id="public", algorithm=algorithm, public_key=text)


def _only_key(store: TrustStore) -> TrustedKey:
    keys = store.keys
    if len(keys) != 1:
        raise CliError("public key file contains more than one key")
    return keys[0]


# ── Manifest file maps ──────────────────────────────────────────────────────


def _safe_name(name: Any) -> str | None:
    text = str(name or "").strip()
    if not text or "\\" in text or text.startswith("/"):
        return None
    parts = text.split("/")
    if any(part in {"", ".", ".."} for part in parts):
        return None
    return text


def _declared_manifest_paths(manifest: Mapping[str, Any]) -> list[str]:
    names: list[str] = []

    def add(value: Any) -> None:
        name = _safe_name(value)
        if name is None:
            raise CliError(f"unsafe asset path in manifest: {value!r}")
        if name not in names:
            names.append(name)

    checksums = manifest.get("checksums")
    if isinstance(checksums, list):
        for item in checksums:
            if isinstance(item, Mapping) and item.get("path") is not None:
                add(item["path"])
    elif isinstance(checksums, Mapping):
        files = checksums.get("files")
        if isinstance(files, Mapping):
            for name in files:
                add(name)
        else:
            for name in checksums:
                if str(name) in {"algorithm", "schema", "signature"}:
                    continue
                add(name)
    assets = manifest.get("assets")
    if isinstance(assets, list):
        for asset in assets:
            if isinstance(asset, Mapping) and asset.get("path") is not None:
                add(asset["path"])
    return names


def _load_manifest_files(
    manifest: Mapping[str, Any], root: str | Path
) -> dict[str, bytes]:
    base = Path(root)
    files: dict[str, bytes] = {}
    for name in _declared_manifest_paths(manifest):
        candidate = base.joinpath(*name.split("/"))
        try:
            resolved = candidate.resolve()
            resolved.relative_to(base.resolve())
        except (OSError, ValueError):
            raise CliError(f"asset escapes the manifest root: {name!r}") from None
        try:
            files[name] = resolved.read_bytes()
        except OSError as exc:
            raise CliError(f"cannot read asset {name!r}: {exc}") from None
    return files


# ── Commands ────────────────────────────────────────────────────────────────


def _cmd_generate(args: argparse.Namespace) -> int:
    _require_ed25519()
    key_id = str(args.key_id or "").strip()
    if not key_id:
        raise CliError("--key-id must not be empty")
    private_path = Path(args.private_out)
    public_path = Path(args.public_out)
    if private_path.resolve() == public_path.resolve():
        raise CliError("--private-out and --public-out must differ")
    existing = [path for path in (private_path, public_path) if path.exists()]
    if existing and not args.force:
        raise CliError(f"refusing to overwrite existing file: {existing[0]}")

    private_key = Ed25519PrivateKey.generate()
    document = _public_document(private_key, key_id)
    fingerprint = TrustedKey(
        key_id=key_id,
        algorithm=ED25519,
        public_key=document["public_key"],
    ).fingerprint()
    private_pem = private_key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    )

    private_written = False
    try:
        _write_private(private_path, private_pem, force=args.force)
        private_written = True
        _write_json(public_path, document)
    except Exception:
        if private_written:
            try:
                private_path.unlink()
            except OSError:
                pass
        raise
    sys.stdout.write(fingerprint + "\n")
    return 0


def _cmd_fingerprint(args: argparse.Namespace) -> int:
    if args.public:
        key = _read_public_key(args.public, algorithm=args.algorithm)
        sys.stdout.write(key.fingerprint() + "\n")
        return 0
    store = _load_store(args)
    try:
        fingerprint = store.fingerprint(args.key_id)
    except TrustError as exc:
        raise CliError(str(exc)) from None
    sys.stdout.write(fingerprint + "\n")
    return 0


def _cmd_sign_index(args: argparse.Namespace) -> int:
    key_id = str(args.key_id or "").strip()
    if not key_id:
        raise CliError("--key-id must not be empty")
    private_key = _load_private_key(args.private)
    index = _read_json_object(args.input, label="index")
    signature = _sign(private_key, canonical_index_payload(index))
    signed = dict(index)
    signed["signature"] = {
        "algorithm": ED25519,
        "key_id": key_id,
        "value": signature,
    }
    _write_json(args.output, signed)
    _print_status({"signed": True, "kind": "index", "key_id": key_id})
    return 0


def _cmd_verify_index(args: argparse.Namespace) -> int:
    store = _load_store(args)
    index = _read_json_object(args.input, label="index")
    verified = store.verify_index(index)
    signature = index.get("signature")
    key_id = signature.get("key_id") if isinstance(signature, Mapping) else None
    _print_status({
        "kind": "index",
        "key_id": key_id if isinstance(key_id, str) else None,
        "verified": bool(verified),
    })
    return 0 if verified else 1


def _cmd_sign_manifest(args: argparse.Namespace) -> int:
    key_id = str(args.key_id or "").strip()
    if not key_id:
        raise CliError("--key-id must not be empty")
    private_key = _load_private_key(args.private)
    manifest = _read_json_object(args.input, label="manifest")
    signature = _sign(private_key, canonical_manifest_payload(manifest))
    signed = dict(manifest)
    signed["signature"] = {
        "algorithm": ED25519,
        "key_id": key_id,
        "value": signature,
    }
    _write_json(args.output, signed)
    _print_status({"signed": True, "kind": "manifest", "key_id": key_id})
    return 0


def _cmd_verify_manifest(args: argparse.Namespace) -> int:
    store = _load_store(args)
    manifest = _read_json_object(args.input, label="manifest")
    files = (
        _load_manifest_files(manifest, args.root)
        if args.root is not None
        else None
    )
    verified = store.verify_manifest(manifest, files)
    signature = manifest.get("signature")
    key_id = signature.get("key_id") if isinstance(signature, Mapping) else None
    _print_status({
        "files_checked": files is not None,
        "key_id": key_id if isinstance(key_id, str) else None,
        "kind": "manifest",
        "verified": bool(verified),
    })
    return 0 if verified else 1


# ── Parser ──────────────────────────────────────────────────────────────────


def _add_trust_options(parser: argparse.ArgumentParser) -> None:
    group = parser.add_mutually_exclusive_group(required=False)
    group.add_argument(
        "--trust",
        metavar="PATH",
        help="trust store JSON file",
    )
    group.add_argument(
        "--trust-env",
        action="store_true",
        help=f"read the trust store from {ENV_VAR}",
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="display_keys.py",
        description="Generate display signing keys and sign/verify asset payloads.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    generate = subparsers.add_parser(
        "generate", help="generate an Ed25519 signing key pair"
    )
    generate.add_argument("--key-id", required=True)
    generate.add_argument("--private-out", required=True, metavar="PATH")
    generate.add_argument("--public-out", required=True, metavar="PATH")
    generate.add_argument(
        "--force",
        action="store_true",
        help="overwrite existing output files",
    )
    generate.set_defaults(func=_cmd_generate)

    fingerprint = subparsers.add_parser(
        "fingerprint", help="print a public key fingerprint"
    )
    source = fingerprint.add_mutually_exclusive_group(required=True)
    source.add_argument("--public", metavar="PATH")
    source.add_argument("--key-id")
    fingerprint.add_argument(
        "--algorithm",
        choices=("ed25519", "hmac-sha256"),
        default="ed25519",
        help="algorithm for a raw (non-JSON) public key file",
    )
    _add_trust_options(fingerprint)
    fingerprint.set_defaults(func=_cmd_fingerprint)

    sign_index = subparsers.add_parser(
        "sign-index", help="sign a Connect asset index"
    )
    sign_index.add_argument("--private", required=True, metavar="PATH")
    sign_index.add_argument("--key-id", required=True)
    sign_index.add_argument("--in", dest="input", required=True, metavar="PATH")
    sign_index.add_argument("--out", dest="output", required=True, metavar="PATH")
    sign_index.set_defaults(func=_cmd_sign_index)

    verify_index = subparsers.add_parser(
        "verify-index", help="verify a signed Connect asset index"
    )
    verify_index.add_argument("--in", dest="input", required=True, metavar="PATH")
    _add_trust_options(verify_index)
    verify_index.set_defaults(func=_cmd_verify_index)

    sign_manifest = subparsers.add_parser(
        "sign-manifest", help="sign a pack manifest"
    )
    sign_manifest.add_argument("--private", required=True, metavar="PATH")
    sign_manifest.add_argument("--key-id", required=True)
    sign_manifest.add_argument("--in", dest="input", required=True, metavar="PATH")
    sign_manifest.add_argument("--out", dest="output", required=True, metavar="PATH")
    sign_manifest.set_defaults(func=_cmd_sign_manifest)

    verify_manifest = subparsers.add_parser(
        "verify-manifest", help="verify a signed pack manifest"
    )
    verify_manifest.add_argument("--in", dest="input", required=True, metavar="PATH")
    verify_manifest.add_argument(
        "--root",
        metavar="PATH",
        help="pack directory; verify declared asset hashes",
    )
    _add_trust_options(verify_manifest)
    verify_manifest.set_defaults(func=_cmd_verify_manifest)

    return parser


def main(argv: Iterable[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(list(argv) if argv is not None else None)
    try:
        return int(args.func(args))
    except (CliError, TrustError, OSError, UnicodeError, ValueError) as exc:
        sys.stderr.write(_json_text({"error": str(exc)}))
        return 1


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
