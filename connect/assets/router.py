"""FastAPI router for the Connect shared asset database.

``build_router(service)`` is the testable constructor.  The module-level
``router`` is used by ``connect.main`` and resolves a service from
``app.state.asset_service`` at request time, so importing the application does
not open a database connection.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Body, Header, HTTPException, Request, Response

from connect.assets.index import AssetService, index_bytes
from connect.assets.models import AssetError


def _http_error(exc: AssetError) -> HTTPException:
    return HTTPException(
        status_code=exc.status_code,
        detail={"code": exc.code, "message": str(exc)},
    )


def _service_for(
    service: AssetService | None,
    request: Request,
) -> AssetService:
    if service is not None:
        return service
    found = getattr(request.app.state, "asset_service", None)
    if found is None:
        found = getattr(request.app.state, "assets_service", None)
    if found is None:
        raise HTTPException(
            status_code=503,
            detail={
                "code": "asset_service_unavailable",
                "message": "asset service is not configured",
            },
        )
    return found


def build_router(service: AssetService | None = None) -> APIRouter:
    """Build the ``/assets`` router around a service instance."""
    router = APIRouter(prefix="/assets", tags=["assets"])

    @router.get("/search")
    def search_assets(
        request: Request,
        q: str | None = None,
        type: str | None = None,
        tag: str | None = None,
        licence: str | None = None,
        permission: str | None = None,
        include_yanked: bool = False,
        limit: int = 50,
        offset: int = 0,
    ) -> dict[str, Any]:
        current = _service_for(service, request)
        try:
            return current.search(
                q,
                type=type,
                tag=tag,
                licence=licence,
                permission=permission,
                include_yanked=include_yanked,
                limit=limit,
                offset=offset,
            )
        except AssetError as exc:
            raise _http_error(exc) from exc

    @router.get("/entry/{pack_id}")
    def get_entry(
        request: Request,
        pack_id: str,
        version: str | None = None,
    ) -> dict[str, Any]:
        current = _service_for(service, request)
        try:
            return current.entry(pack_id, version)
        except AssetError as exc:
            raise _http_error(exc) from exc

    @router.get("/versions/{pack_id}")
    def get_versions(
        request: Request,
        pack_id: str,
        include_yanked: bool = True,
    ) -> dict[str, Any]:
        current = _service_for(service, request)
        try:
            return current.versions(
                pack_id,
                include_yanked=include_yanked,
            )
        except AssetError as exc:
            raise _http_error(exc) from exc

    @router.get("/categories")
    def get_categories(request: Request) -> dict[str, Any]:
        current = _service_for(service, request)
        try:
            return current.categories()
        except AssetError as exc:
            raise _http_error(exc) from exc

    @router.post("/submit")
    def submit_pack(
        request: Request,
        payload: Any = Body(...),
    ) -> dict[str, Any]:
        current = _service_for(service, request)
        try:
            return current.submit_request(payload)
        except AssetError as exc:
            raise _http_error(exc) from exc

    @router.get("/stats")
    def get_stats(request: Request) -> dict[str, Any]:
        current = _service_for(service, request)
        try:
            return current.stats()
        except AssetError as exc:
            raise _http_error(exc) from exc

    @router.get("/index/{channel}")
    def get_index(
        request: Request,
        channel: str,
        if_none_match: str | None = Header(default=None, alias="If-None-Match"),
    ) -> Response:
        current = _service_for(service, request)
        try:
            snapshot = current.snapshot(
                channel,
                if_none_match=if_none_match,
            )
        except AssetError as exc:
            raise _http_error(exc) from exc
        headers = dict(snapshot.headers)
        headers["Cache-Control"] = "public, max-age=300"
        if snapshot.not_modified:
            return Response(status_code=304, headers=headers)
        assert snapshot.index is not None
        return Response(
            content=index_bytes(snapshot.index),
            media_type="application/json",
            headers=headers,
        )

    return router


#: Application-level router.  Endpoints return 503 until
#: ``app.state.asset_service`` is configured.
router = build_router()


__all__ = ["build_router", "router"]
