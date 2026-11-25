"""FastAPI router for the Connect shared asset database.

``build_router(service)`` is the testable constructor.  The module-level
``router`` is used by ``connect.main`` and resolves a service from
``app.state.asset_service`` at request time, so importing the application does
not open a database connection.
"""

from __future__ import annotations

import hmac
from typing import Any, Mapping

from fastapi import APIRouter, Body, Header, HTTPException, Request, Response
from fastapi.responses import HTMLResponse

from connect.assets.gallery import CSP, render_gallery
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


def _review_config(
    service: AssetService,
    override: str | None,
) -> str:
    token = override if override is not None else getattr(
        service,
        "review_token",
        None,
    )
    return str(token).strip() if token else ""


def _require_review(
    service: AssetService,
    override: str | None,
    supplied: str | None,
) -> None:
    """Fail closed: 503 when unconfigured, 401 missing, 403 invalid."""
    expected = _review_config(service, override)
    if not expected:
        raise HTTPException(
            status_code=503,
            detail={
                "code": "review_unavailable",
                "message": "review token is not configured",
            },
        )
    if supplied is None or not str(supplied):
        raise HTTPException(
            status_code=401,
            detail={
                "code": "review_token_missing",
                "message": "X-Ozma-Review-Token header is required",
            },
        )
    if not hmac.compare_digest(str(supplied), expected):
        raise HTTPException(
            status_code=403,
            detail={
                "code": "review_token_invalid",
                "message": "review token is invalid",
            },
        )


def _review_body(
    payload: Any,
    reviewer_header: str | None,
) -> tuple[str | None, str, str]:
    if payload is None:
        data: Mapping[str, Any] = {}
    elif isinstance(payload, Mapping):
        data = payload
    else:
        raise HTTPException(
            status_code=400,
            detail={
                "code": "invalid_review_body",
                "message": "review body must be a JSON object",
            },
        )
    version = data.get("version")
    if version is not None and not isinstance(version, str):
        raise HTTPException(
            status_code=400,
            detail={
                "code": "invalid_review_body",
                "message": "version must be a string",
            },
        )
    note = data.get("note", "")
    if note is None:
        note = ""
    if not isinstance(note, str):
        raise HTTPException(
            status_code=400,
            detail={
                "code": "invalid_review_body",
                "message": "note must be a string",
            },
        )
    reviewer = data.get("reviewer") or reviewer_header or "admin"
    if not isinstance(reviewer, str):
        raise HTTPException(
            status_code=400,
            detail={
                "code": "invalid_review_body",
                "message": "reviewer must be a string",
            },
        )
    return (
        version.strip() if isinstance(version, str) and version.strip() else None,
        note.strip(),
        reviewer.strip() or "admin",
    )


def build_router(
    service: AssetService | None = None,
    review_token: str | None = None,
) -> APIRouter:
    """Build the ``/assets`` router around a service instance.

    ``review_token`` overrides the service's configured token, which is useful
    for tests and small embeddings.  The default remains the token on the
    service so ``build_router(service)`` keeps working unchanged.
    """
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

    @router.get("/gallery", response_class=HTMLResponse)
    def get_gallery(
        request: Request,
        q: str | None = None,
        type: str | None = None,
        tag: str | None = None,
    ) -> HTMLResponse:
        current = _service_for(service, request)
        try:
            page = render_gallery(current, q=q, type=type, tag=tag)
        except AssetError as exc:
            raise _http_error(exc) from exc
        return HTMLResponse(
            content=page,
            headers={
                "Content-Security-Policy": CSP,
                "X-Content-Type-Options": "nosniff",
                "Referrer-Policy": "no-referrer",
                "Cache-Control": "public, max-age=60",
            },
        )

    @router.get("/review")
    def list_reviews(
        request: Request,
        status: str | None = "pending",
        limit: int = 50,
        offset: int = 0,
        review_header: str | None = Header(
            default=None,
            alias="X-Ozma-Review-Token",
        ),
    ) -> dict[str, Any]:
        current = _service_for(service, request)
        _require_review(current, review_token, review_header)
        try:
            return current.reviews.list_reviews(
                status,
                limit=limit,
                offset=offset,
            )
        except AssetError as exc:
            raise _http_error(exc) from exc

    @router.post("/review/{pack_id}/approve")
    def approve_pack(
        request: Request,
        pack_id: str,
        payload: Any = Body(default=None),
        review_header: str | None = Header(
            default=None,
            alias="X-Ozma-Review-Token",
        ),
        reviewer_header: str | None = Header(
            default=None,
            alias="X-Ozma-Review-Reviewer",
        ),
    ) -> dict[str, Any]:
        current = _service_for(service, request)
        _require_review(current, review_token, review_header)
        version, note, reviewer = _review_body(payload, reviewer_header)
        try:
            return current.reviews.approve(
                pack_id,
                version=version,
                note=note,
                reviewer=reviewer,
            )
        except AssetError as exc:
            raise _http_error(exc) from exc

    @router.post("/review/{pack_id}/reject")
    def reject_pack(
        request: Request,
        pack_id: str,
        payload: Any = Body(default=None),
        review_header: str | None = Header(
            default=None,
            alias="X-Ozma-Review-Token",
        ),
        reviewer_header: str | None = Header(
            default=None,
            alias="X-Ozma-Review-Reviewer",
        ),
    ) -> dict[str, Any]:
        current = _service_for(service, request)
        _require_review(current, review_token, review_header)
        version, note, reviewer = _review_body(payload, reviewer_header)
        try:
            return current.reviews.reject(
                pack_id,
                version=version,
                note=note,
                reviewer=reviewer,
            )
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
