"""Self-contained HTML gallery for approved public Connect packs.

The page deliberately has no scripts, no external stylesheets, fonts, images, or
network references.  Every value that originates in a manifest is HTML-escaped,
and only the fields needed for browsing are rendered.  Pending, rejected, and
private packs never reach this module because it uses the service's public
search path.
"""

from __future__ import annotations

import html
from typing import Any
from urllib.parse import quote

CSP = "default-src 'none'; style-src 'unsafe-inline'"

_STYLE = """
:root { color-scheme: light dark; }
* { box-sizing: border-box; }
body {
  margin: 0 auto;
  max-width: 60rem;
  padding: 2rem 1.25rem 3rem;
  font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
  line-height: 1.5;
}
header { border-bottom: 1px solid #8884; margin-bottom: 1.5rem; }
h1 { font-size: 1.6rem; margin: 0 0 .25rem; }
h2 { font-size: 1.1rem; margin: 0 0 .35rem; }
p { margin: .35rem 0; }
.muted { color: #777; font-size: .9rem; }
.packs { list-style: none; margin: 0; padding: 0; }
.pack {
  border: 1px solid #8884;
  border-radius: .5rem;
  padding: 1rem;
  margin: 0 0 1rem;
}
.meta { display: flex; flex-wrap: wrap; gap: .4rem 1rem; font-size: .9rem; }
.tags { display: flex; flex-wrap: wrap; gap: .3rem; margin-top: .5rem; }
.tag { border: 1px solid #8886; border-radius: 999px; padding: .05rem .5rem; font-size: .8rem; }
a { color: #2b6cb0; }
""".strip()


def _esc(value: Any) -> str:
    return html.escape(str(value if value is not None else ""), quote=True)


def _entry_url(pack_id: Any) -> str:
    return "/assets/entry/" + quote(str(pack_id or ""), safe="")


def _render_pack(entry: dict[str, Any]) -> str:
    pack_id = entry.get("id") or ""
    name = _esc(entry.get("name") or pack_id or "Untitled pack")
    author = _esc(entry.get("author") or "Unknown author")
    version = _esc(entry.get("version") or "")
    licence = _esc(entry.get("license") or "Unknown licence")
    tags = [
        f'<span class="tag">{_esc(tag)}</span>'
        for tag in (entry.get("tags") or [])
        if str(tag).strip()
    ]
    tag_markup = (
        f'<div class="tags">{"".join(tags)}</div>' if tags else ""
    )
    return (
        '<li class="pack">'
        f'<h2><a href="{_entry_url(pack_id)}">{name}</a></h2>'
        '<div class="meta">'
        f"<span>{author}</span>"
        f"<span>Version {version}</span>"
        f"<span>{licence}</span>"
        "</div>"
        f"{tag_markup}"
        "</li>"
    )


def render_gallery(
    service: Any,
    *,
    q: str | None = None,
    type: str | None = None,
    tag: str | None = None,
    limit: int = 100,
) -> str:
    """Render the gallery HTML for approved public packs.

    ``service.search`` applies licence/visibility policy and, when review is
    enabled, excludes pending and rejected versions.
    """
    result = service.search(
        q,
        type=type,
        tag=tag,
        limit=limit,
        offset=0,
    )
    entries = result.get("entries") or []
    active = [
        f"{label}: {_esc(value)}"
        for label, value in (
            ("Search", q),
            ("Type", type),
            ("Tag", tag),
        )
        if value
    ]
    filters = (
        f'<p class="muted">Filters — {" · ".join(active)}</p>'
        if active
        else ""
    )
    if entries:
        body = f'<ul class="packs">{"".join(_render_pack(item) for item in entries)}</ul>'
    else:
        body = "<p>No approved packs match this view.</p>"

    return (
        "<!doctype html>\n"
        '<html lang="en">\n'
        "<head>\n"
        '<meta charset="utf-8">\n'
        '<meta name="viewport" content="width=device-width, initial-scale=1">\n'
        f'<meta http-equiv="Content-Security-Policy" content="{CSP}">\n'
        '<meta name="referrer" content="no-referrer">\n'
        "<title>Connect asset gallery</title>\n"
        f"<style>{_STYLE}</style>\n"
        "</head>\n"
        "<body>\n"
        "<header>\n"
        "<h1>Connect asset gallery</h1>\n"
        '<p class="muted">Approved public packs. Install links open the pack '
        "entry as JSON.</p>\n"
        f"{filters}"
        "</header>\n"
        f"<main>{body}</main>\n"
        "</body>\n"
        "</html>\n"
    )


__all__ = ["CSP", "render_gallery"]
