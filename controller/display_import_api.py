# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Private display import API.

Imports are stored locally, marked private, and never published automatically.
The API accepts bytes or text only; it never fetches a URL and never executes
source content.
"""

from __future__ import annotations

import base64
import binascii
from typing import Any

from fastapi import APIRouter, Body, HTTPException, Response
from fastapi.responses import JSONResponse

try:  # controller modules are imported top-level in the app
    from display_import_store import (
        ImportStore,
        ImportStoreError,
    )
    from display_importers.base import ImportError as SourceImportError
    from display_importers.registry import importer_formats
except ImportError:  # package-style import
    from .display_import_store import (
        ImportStore,
        ImportStoreError,
    )
    from .display_importers.base import ImportError as SourceImportError
    from .display_importers.registry import importer_formats


def _typed(error: ImportStoreError | SourceImportError) -> HTTPException:
    status = getattr(error, "status_code", 400)
    return HTTPException(status, detail={"code": getattr(error, "code", "import_error"), "message": str(error)})


def _decode_body(body: dict[str, Any], max_bytes: int) -> tuple[str, bytes, str | None]:
    name = str(body.get("name") or "").strip()
    if not name:
        raise HTTPException(400, detail={"code": "invalid_import", "message": "name is required"})
    if body.get("data_base64") is not None:
        encoded = str(body["data_base64"])
        if len(encoded) > (max_bytes * 4 // 3) + 16:
            raise HTTPException(413, detail={"code": "import_too_large", "message": "source exceeds the import budget"})
        try:
            data = base64.b64decode(encoded, validate=True)
        except (binascii.Error, ValueError) as error:
            raise HTTPException(400, detail={"code": "invalid_import", "message": f"invalid base64: {error}"}) from error
    elif body.get("text") is not None:
        data = str(body["text"]).encode("utf-8")
    else:
        raise HTTPException(
            400,
            detail={"code": "invalid_import", "message": "one of data_base64 or text is required"},
        )
    if len(data) > max_bytes:
        raise HTTPException(413, detail={"code": "import_too_large", "message": "source exceeds the import budget"})
    fmt = body.get("format")
    return name, data, str(fmt) if fmt else None


def build_router(store: ImportStore) -> APIRouter:
    """Build the private display-import router."""
    router = APIRouter(prefix="/api/v1/display", tags=["display-import"])

    @router.get("/importers")
    def list_importers() -> dict[str, Any]:
        return {"importers": importer_formats()}

    @router.post("/import", status_code=201)
    def import_source(body: dict[str, Any] = Body(...)) -> dict[str, Any]:
        if body.get("private") is False:
            raise HTTPException(
                400,
                detail={"code": "invalid_import", "message": "imports are always private"},
            )
        name, data, fmt = _decode_body(body, store.max_import_bytes)
        try:
            manifest = store.save(name, data, format=fmt)
        except (ImportStoreError, SourceImportError) as error:
            raise _typed(error) from error
        return {"import": manifest}

    @router.get("/imports")
    def list_imports() -> dict[str, Any]:
        items = store.list_imports()
        return {"imports": items, "count": len(items)}

    @router.get("/imports/{import_id}")
    def get_import(import_id: str) -> dict[str, Any]:
        try:
            return {"import": store.get(import_id)}
        except ImportStoreError as error:
            raise _typed(error) from error

    @router.get("/imports/{import_id}/raw")
    def get_raw(import_id: str) -> Response:
        try:
            manifest = store.get(import_id)
            data = store.load_raw(import_id)
        except ImportStoreError as error:
            raise _typed(error) from error
        media_type = str((manifest.get("source") or {}).get("format") or "application/octet-stream")
        return Response(content=data, media_type="application/octet-stream", headers={"X-Ozma-Source-Format": media_type})

    @router.get("/imports/{import_id}/assets/{sha256}")
    def get_asset(import_id: str, sha256: str) -> Response:
        try:
            data = store.load_asset(import_id, sha256)
        except ImportStoreError as error:
            raise _typed(error) from error
        return Response(content=data, media_type="application/octet-stream")

    @router.delete("/imports/{import_id}")
    def delete_import(import_id: str) -> JSONResponse:
        try:
            store.delete(import_id)
        except ImportStoreError as error:
            raise _typed(error) from error
        return JSONResponse({"deleted": import_id})

    return router


__all__ = ["build_router"]
