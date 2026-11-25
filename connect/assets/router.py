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

from connect.assets.gallery import (
    CSP,
    STATIC_CACHE_CONTROL,
    STATIC_FILES,
    render_marketplace,
    static_file,
)
from connect.assets.index import AssetService, index_bytes
from connect.assets.models import AssetError

_MAX_QUERY = 200
_MAX_FILTER = 128
_MAX_SORT_RESULTS = 5_000
_SORTS = frozenset({"name", "name_desc", "author", "published", "id"})


def _bounded(value: str | None, limit: int) -> str | None:
    if value is None:
        return None
    return str(value)[:limit]


def _sorted_search(
    current: AssetService,
    q: str | None,
    *,
    type: str | None,
    tag: str | None,
    licence: str | None,
    permission: str | None,
    include_yanked: bool,
    limit: int,
    offset: int,
    sort: str | None,
) -> dict[str, Any]:
    result = current.search(
        q,
        type=type,
        tag=tag,
        licence=licence,
        permission=permission,
        include_yanked=include_yanked,
        limit=limit,
        offset=offset,
    )
    if not sort or sort == "name":
        return result

    entries: list[dict[str, Any]] = []
    position = 0
    while position < _MAX_SORT_RESULTS:
        page = current.search(
            q,
            type=type,
            tag=tag,
            licence=licence,
            permission=permission,
            include_yanked=include_yanked,
            limit=200,
            offset=position,
        )
        chunk = list(page.get("entries") or [])
        entries.extend(chunk)
        position += len(chunk)
        total = int(page.get("total") or 0)
        if not chunk or position >= total:
            break

    if sort == "name_desc":
        entries.sort(
            key=lambda item: (
                str(item.get("name", "")).lower(),
                str(item.get("id", "")),
            ),
            reverse=True,
        )
    elif sort == "author":
        entries.sort(key=lambda item: (
            str(item.get("author", "")).lower(),
            str(item.get("name", "")).lower(),
            str(item.get("id", "")),
        ))
    elif sort == "published":
        entries.sort(
            key=lambda item: (
                str(item.get("published_at", "")),
                str(item.get("name", "")).lower(),
                str(item.get("id", "")),
            ),
            reverse=True,
        )
    else:
        entries.sort(key=lambda item: str(item.get("id", "")))

    page_entries = []
    try:
        page_limit = int(limit)
    except (TypeError, ValueError):
        page_limit = 0
    page_limit = max(0, min(page_limit, 200))
    if page_limit:
        page_entries = entries[offset:offset + page_limit]
    merged = dict(result)
    merged.update({
        "entries": page_entries,
        "count": len(page_entries),
        "sort": sort,
    })
    return merged


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
        sort: str | None = None,
        include_yanked: bool = False,
        limit: int = 50,
        offset: int = 0,
    ) -> dict[str, Any]:
        current = _service_for(service, request)
        chosen_sort = _bounded(sort, 32)
        if chosen_sort not in _SORTS:
            chosen_sort = "name"
        try:
            return _sorted_search(
                current,
                _bounded(q, _MAX_QUERY),
                type=_bounded(type, _MAX_FILTER),
                tag=_bounded(tag, _MAX_FILTER),
                licence=_bounded(licence, _MAX_FILTER),
                permission=_bounded(permission, _MAX_FILTER),
                include_yanked=include_yanked,
                limit=limit,
                offset=offset,
                sort=chosen_sort,
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

    def _marketplace_response(
        request: Request,
        *,
        q: str | None,
        type: str | None,
        tag: str | None,
        licence: str | None,
        sort: str | None,
        limit: int,
        offset: int,
    ) -> HTMLResponse:
        current = _service_for(service, request)
        try:
            page = render_marketplace(
                current,
                q=_bounded(q, _MAX_QUERY),
                type=_bounded(type, _MAX_FILTER),
                tag=_bounded(tag, _MAX_FILTER),
                licence=_bounded(licence, _MAX_FILTER),
                sort=_bounded(sort, _MAX_FILTER),
                limit=limit,
                offset=offset,
            )
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

    @router.get("/gallery", response_class=HTMLResponse)
    def get_gallery(
        request: Request,
        q: str | None = None,
        type: str | None = None,
        tag: str | None = None,
        licence: str | None = None,
        sort: str | None = None,
        limit: int = 24,
        offset: int = 0,
    ) -> HTMLResponse:
        # Kept as an alias of the marketplace for existing bookmarks and links.
        return _marketplace_response(
            request,
            q=q,
            type=type,
            tag=tag,
            licence=licence,
            sort=sort,
            limit=limit,
            offset=offset,
        )

    @router.get("/marketplace", response_class=HTMLResponse)
    def get_marketplace(
        request: Request,
        q: str | None = None,
        type: str | None = None,
        tag: str | None = None,
        licence: str | None = None,
        sort: str | None = None,
        limit: int = 24,
        offset: int = 0,
    ) -> HTMLResponse:
        return _marketplace_response(
            request,
            q=q,
            type=type,
            tag=tag,
            licence=licence,
            sort=sort,
            limit=limit,
            offset=offset,
        )

    @router.get("/static/{filename:path}")
    def get_static(filename: str) -> Response:
        body = static_file(filename)
        if body is None:
            raise HTTPException(
                status_code=404,
                detail={
                    "code": "not_found",
                    "message": "unknown static asset",
                },
            )
        return Response(
            content=body,
            media_type=STATIC_FILES[filename],
            headers={
                "Cache-Control": STATIC_CACHE_CONTROL,
                "X-Content-Type-Options": "nosniff",
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
