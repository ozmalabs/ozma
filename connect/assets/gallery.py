"""Static marketplace shell and review UI for approved Connect packs.

The server renders an escaped HTML shell; all filtering, pagination, pack
details, and review actions are performed by the same-origin JavaScript in
``static/marketplace.js`` through the public ``/assets`` JSON endpoints.  No
manifest-derived value is embedded in an inline script, and private, pending,
or rejected packs never reach this module because it uses the service's public
search path.

The page has no third-party assets, fonts, or network references.  The only
static files served are the known marketplace JavaScript and stylesheet.
"""

from __future__ import annotations

import html
import re
from pathlib import Path
from typing import Any, Iterable
from urllib.parse import quote, urlsplit

CSP = (
    "default-src 'self'; script-src 'self'; "
    "style-src 'self' 'unsafe-inline'; img-src 'self' data:; "
    "connect-src 'self'; object-src 'none'; base-uri 'none'; "
    "form-action 'none'"
)

STATIC_DIR = Path(__file__).resolve().parent / "static"

#: Exactly the files the router may serve.  Keys are single path segments.
STATIC_FILES: dict[str, str] = {
    "marketplace.js": "text/javascript; charset=utf-8",
    "marketplace.css": "text/css; charset=utf-8",
}

STATIC_CACHE_CONTROL = "public, max-age=3600"

_MAX_QUERY = 200
_MAX_FILTER = 128
_MAX_LIMIT = 100
_MAX_OFFSET = 100_000
_MAX_PREVIEW = 4096

_SORTS = {
    "name": "Name (A-Z)",
    "name_desc": "Name (Z-A)",
    "author": "Author",
    "published": "Newest published",
    "id": "Pack id",
}

_IMAGE_DATA_RE = re.compile(
    r"^data:image/(?:png|jpeg|jpg|gif|webp|avif);base64,[A-Za-z0-9+/=\s]+$",
    re.IGNORECASE,
)
_UNSAFE_URL_CHARS = re.compile(r"[\x00-\x1f\x7f\\]")


def _esc(value: Any) -> str:
    return html.escape(str(value if value is not None else ""), quote=True)


def _clip(value: Any, limit: int) -> str:
    return str(value if value is not None else "")[:limit]


def _entry_url(pack_id: Any) -> str:
    return "/assets/entry/" + quote(str(pack_id or ""), safe="")


def _safe_image_url(value: Any) -> str | None:
    """Return an image URL only when it is relative or a safe data image."""
    if not isinstance(value, str):
        return None
    raw = value.strip()
    if not raw or len(raw) > _MAX_PREVIEW:
        return None
    if _UNSAFE_URL_CHARS.search(raw):
        return None
    lowered = raw.lower()
    if lowered.startswith("data:"):
        return raw if _IMAGE_DATA_RE.match(raw) else None
    if raw.startswith("//"):
        return None
    parsed = urlsplit(raw)
    if parsed.scheme or parsed.netloc:
        return None
    return raw


def _render_pack(entry: dict[str, Any]) -> str:
    pack_id = entry.get("id") or ""
    name = _esc(entry.get("name") or pack_id or "Untitled pack")
    author = _esc(entry.get("author") or "Unknown author")
    version = _esc(entry.get("version") or "")
    licence = _esc(entry.get("license") or "Unknown licence")
    pack_type = _esc(entry.get("type") or "")
    description = _clip(entry.get("description") or "", 280)
    tags = [
        f'<span class="tag">{_esc(tag)}</span>'
        for tag in (entry.get("tags") or [])
        if str(tag).strip()
    ]
    tag_markup = (
        f'<div class="tags">{"".join(tags)}</div>' if tags else ""
    )
    preview = _safe_image_url(entry.get("preview"))
    preview_markup = (
        '<img class="preview" src="' + _esc(preview) + '" alt="" '
        'loading="lazy" decoding="async" referrerpolicy="no-referrer">'
        if preview
        else ""
    )
    description_markup = (
        f'<p class="description">{_esc(description)}</p>'
        if description.strip()
        else ""
    )
    type_markup = f"<span>{pack_type}</span>" if pack_type else ""
    return (
        f'<li class="pack" data-pack-id="{_esc(pack_id)}">'
        f"{preview_markup}"
        f'<h2><a href="{_entry_url(pack_id)}">{name}</a></h2>'
        '<div class="meta">'
        f"<span>{author}</span>"
        f"<span>Version {version}</span>"
        f"<span>{licence}</span>"
        f"{type_markup}"
        "</div>"
        f"{description_markup}"
        f"{tag_markup}"
        "</li>"
    )


def _options(
    values: Iterable[str],
    selected: str | None,
    default_label: str,
) -> str:
    ordered = sorted({str(value) for value in values if str(value)})
    if selected and selected not in ordered:
        ordered.append(selected)
    parts = [f'<option value="">{_esc(default_label)}</option>']
    for value in ordered:
        marker = " selected" if value == selected else ""
        parts.append(
            f'<option value="{_esc(value)}"{marker}>{_esc(value)}</option>'
        )
    return "".join(parts)


def _render_detail() -> str:
    return (
        '<section id="marketplace-detail" class="detail" hidden '
        'aria-labelledby="detail-title">'
        '<button type="button" id="detail-back" class="secondary">'
        "Back to results</button>"
        '<h2 id="detail-title" tabindex="-1">Pack details</h2>'
        '<div id="detail-body"></div>'
        "</section>"
    )


def _render_review_panel() -> str:
    return (
        '<section id="review-panel" class="review-panel" hidden '
        'aria-labelledby="review-heading">'
        '<h2 id="review-heading">Review queue</h2>'
        '<p class="muted">Operator only. The token stays in this browser tab '
        "and is sent only in the <code>X-Ozma-Review-Token</code> header."
        "</p>"
        '<form id="review-token-form" class="review-token-form">'
        '<label for="review-token">Review token</label>'
        '<input id="review-token" type="password" '
        'autocomplete="off" autocapitalize="off" spellcheck="false">'
        '<button type="submit">Open review queue</button>'
        '<button type="button" id="review-forget" class="secondary" hidden>'
        "Forget token</button>"
        "</form>"
        '<p id="review-message" class="status" role="status" '
        'aria-live="polite"></p>'
        '<div id="review-content" hidden>'
        '<button type="button" id="review-refresh">Refresh pending</button>'
        '<ul id="review-list" class="review-list"></ul>'
        "</div>"
        "</section>"
    )


def _render_shell(
    service: Any,
    *,
    q: str,
    type: str,
    tag: str,
    licence: str,
    sort: str,
    limit: int,
    offset: int,
) -> str:
    result = service.search(
        q or None,
        type=type or None,
        tag=tag or None,
        licence=licence or None,
        limit=limit,
        offset=offset,
    )
    categories = service.categories()
    entries = result.get("entries") or []
    total = int(result.get("total") or 0)
    active = [
        f"{label}: {_esc(value)}"
        for label, value in (
            ("Search", q),
            ("Type", type),
            ("Tag", tag),
            ("Licence", licence),
        )
        if value
    ]
    filters = (
        f'<p class="muted">Filters — {" · ".join(active)}</p>'
        if active
        else ""
    )
    if entries:
        body = (
            '<ul class="packs" id="marketplace-results">'
            f'{"".join(_render_pack(item) for item in entries)}</ul>'
        )
    else:
        body = (
            '<ul class="packs" id="marketplace-results">'
            '<li class="empty">No approved packs match this view.</li></ul>'
        )

    shown = len(entries)
    range_text = (
        f"Showing {shown} of {total} approved packs."
        if total
        else "No approved packs match this view."
    )
    type_options = _options(
        categories.get("types", {}).keys(),
        type or None,
        "All types",
    )
    tag_options = _options(
        categories.get("tags", {}).keys(),
        tag or None,
        "All tags",
    )
    licence_options = _options(
        categories.get("licences", {}).keys(),
        licence or None,
        "All licences",
    )
    sort_options = "".join(
        f'<option value="{_esc(key)}"'
        f'{" selected" if key == sort else ""}>{_esc(label)}</option>'
        for key, label in _SORTS.items()
    )
    page_sizes = sorted({24, 48, 96, limit})
    page_size_options = "".join(
        f'<option value="{size}"'
        f'{" selected" if size == limit else ""}>{size}</option>'
        for size in page_sizes
    )

    return (
        "<!doctype html>\n"
        '<html lang="en">\n'
        "<head>\n"
        '<meta charset="utf-8">\n'
        '<meta name="viewport" content="width=device-width, initial-scale=1">\n'
        f'<meta http-equiv="Content-Security-Policy" content="{CSP}">\n'
        '<meta name="referrer" content="no-referrer">\n'
        "<title>Connect asset marketplace</title>\n"
        '<link rel="stylesheet" href="/assets/static/marketplace.css">\n'
        '<script src="/assets/static/marketplace.js" defer></script>\n'
        "</head>\n"
        "<body>\n"
        '<a class="skip-link" href="#marketplace-main">Skip to packs</a>\n'
        "<header>\n"
        "<h1>Connect asset marketplace</h1>\n"
        '<p class="muted">Browse approved public packs. Pack details and '
        "version history come from the public asset API.</p>"
        f"{filters}"
        '<div class="admin-access">'
        '<button type="button" id="review-toggle" '
        'aria-controls="review-panel" aria-expanded="false">'
        "Review mode</button></div>"
        "</header>\n"
        '<main id="marketplace-main">'
        '<form id="marketplace-search" role="search" '
        'aria-labelledby="search-heading">'
        '<h2 id="search-heading">Find a pack</h2>'
        '<div class="search-grid">'
        '<label class="field field-wide" for="marketplace-q">Search'
        f'<input id="marketplace-q" name="q" type="search" value="{_esc(q)}" '
        'maxlength="200" autocomplete="off"></label>'
        '<label class="field" for="marketplace-type">Type'
        f'<select id="marketplace-type" name="type">{type_options}</select>'
        "</label>"
        '<label class="field" for="marketplace-licence">Licence'
        f'<select id="marketplace-licence" name="licence">'
        f"{licence_options}</select></label>"
        '<label class="field" for="marketplace-tag">Tag'
        f'<select id="marketplace-tag" name="tag">{tag_options}</select>'
        "</label>"
        '<label class="field" for="marketplace-sort">Sort'
        f'<select id="marketplace-sort" name="sort">{sort_options}</select>'
        "</label>"
        '<label class="field" for="marketplace-page-size">Page size'
        '<select id="marketplace-page-size" name="limit">'
        f"{page_size_options}"
        "</select></label>"
        "</div>"
        '<div class="actions">'
        '<button type="submit">Search</button>'
        '<button type="reset" class="secondary" id="marketplace-reset">'
        "Clear</button>"
        "</div>"
        "</form>"
        f'<p id="marketplace-status" class="status" role="status" '
        f'aria-live="polite">{_esc(range_text)}</p>'
        f"<div id=\"marketplace-error\" class=\"status error\" role=\"alert\" "
        'hidden></div>'
        f'<section aria-labelledby="results-heading">'
        '<h2 id="results-heading">Approved packs</h2>'
        f"{body}"
        '<nav class="pager" aria-label="Marketplace pages">'
        '<button type="button" id="marketplace-prev" class="secondary" '
        'disabled>Previous</button>'
        '<span id="marketplace-page" class="muted"></span>'
        '<button type="button" id="marketplace-next" class="secondary" '
        'disabled>Next</button>'
        "</nav>"
        "</section>"
        f"{_render_detail()}"
        f"{_render_review_panel()}"
        "</main>\n"
        "<noscript><p class=\"status\">Enable JavaScript to search, sort, "
        "paginate, and open pack details.</p></noscript>\n"
        "</body>\n"
        "</html>\n"
    )


def render_marketplace(
    service: Any,
    *,
    q: str | None = None,
    type: str | None = None,
    tag: str | None = None,
    licence: str | None = None,
    sort: str | None = None,
    limit: int = 24,
    offset: int = 0,
) -> str:
    """Render the public, read-only marketplace shell."""
    query = _clip(q, _MAX_QUERY).strip()
    pack_type = _clip(type, _MAX_FILTER).strip()
    wanted_tag = _clip(tag, _MAX_FILTER).strip()
    wanted_licence = _clip(licence, _MAX_FILTER).strip()
    chosen_sort = _clip(sort, _MAX_FILTER).strip()
    if chosen_sort not in _SORTS:
        chosen_sort = "name"
    try:
        size = int(limit)
    except (TypeError, ValueError):
        size = 24
    try:
        start = int(offset)
    except (TypeError, ValueError):
        start = 0
    size = max(1, min(size, _MAX_LIMIT))
    start = max(0, min(start, _MAX_OFFSET))
    return _render_shell(
        service,
        q=query,
        type=pack_type,
        tag=wanted_tag,
        licence=wanted_licence,
        sort=chosen_sort,
        limit=size,
        offset=start,
    )


def render_gallery(
    service: Any,
    *,
    q: str | None = None,
    type: str | None = None,
    tag: str | None = None,
    licence: str | None = None,
    sort: str | None = None,
    limit: int = 100,
    offset: int = 0,
) -> str:
    """Backward-compatible ``/assets/gallery`` renderer."""
    return render_marketplace(
        service,
        q=q,
        type=type,
        tag=tag,
        licence=licence,
        sort=sort,
        limit=limit,
        offset=offset,
    )


def static_file(name: str) -> bytes | None:
    """Return bytes for a known static file, or ``None`` for anything else."""
    if name not in STATIC_FILES:
        return None
    path = STATIC_DIR / name
    try:
        resolved = path.resolve(strict=True)
    except (OSError, RuntimeError):
        return None
    if resolved.parent != STATIC_DIR or not resolved.is_file():
        return None
    try:
        return resolved.read_bytes()
    except OSError:
        return None


__all__ = [
    "CSP",
    "STATIC_CACHE_CONTROL",
    "STATIC_DIR",
    "STATIC_FILES",
    "render_gallery",
    "render_marketplace",
    "static_file",
]
