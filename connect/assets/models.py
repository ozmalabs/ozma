"""Shared asset-database models and portable schema helpers.

The Connect asset database mirrors the local controller contract in
``controller/asset_repository.py``.  The SQLAlchemy models are intentionally
SQLite-friendly so the same code can be exercised in unit tests without a
Postgres service; the checked-in migration uses portable SQL types.

Unknown manifest fields are retained as JSON text rather than discarded.  This
module never imports, evaluates, or otherwise executes pack content.
"""

from __future__ import annotations

import json
import re
import uuid
from pathlib import Path
from typing import Any, Mapping

from sqlalchemy import (
    Boolean,
    Column,
    ForeignKey,
    ForeignKeyConstraint,
    Index,
    Integer,
    String,
    Text,
    UniqueConstraint,
    create_engine,
)
from sqlalchemy.orm import declarative_base, relationship

ASSET_SCHEMA = "org.ozma.asset-pack/1"
INDEX_SCHEMA = "org.ozma.asset-index/1"

PACK_TYPES = frozenset({
    "widget_pack",
    "theme_pack",
    "description_pack",
    "import_pack",
})

#: Controller-side sync modes mirrored for API and metadata consumers.
SYNC_MODES = frozenset({"full", "installed", "manual", "offline"})

#: Licences accepted for public submission.  AGPL is accepted only for
#: first-party packs (see :func:`license_is_allowed`).
DEFAULT_ALLOWED_LICENSES = frozenset({
    "MIT",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Apache-2.0",
    "OFL-1.1",
    "CC0-1.0",
})

AGPL_LICENSES = frozenset({
    "AGPL-3.0",
    "AGPL-3.0-only",
    "AGPL-3.0-or-later",
})

_LICENSE_ALIASES = {
    "agpl": "AGPL-3.0-only",
    "agpl 3.0": "AGPL-3.0-only",
    "apache 2.0": "Apache-2.0",
    "apache-2": "Apache-2.0",
    "bsd": "BSD-3-Clause",
    "cc0": "CC0-1.0",
    "mit license": "MIT",
    "ofl": "OFL-1.1",
}

_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")

#: Review states for a public pack version.  A missing row means "approved" so
#: databases created before review support keep their existing public packs.
REVIEW_STATUSES = frozenset({"pending", "approved", "rejected"})
DEFAULT_REVIEW_STATUS = "approved"

#: Manifest fields that have authoritative columns or a dedicated serializer.
#: Everything else is retained verbatim in ``asset_pack.extra_json``.
MANIFEST_KNOWN_FIELDS = frozenset({
    "id",
    "type",
    "name",
    "version",
    "schema",
    "author",
    "description",
    "license",
    "licence",
    "source_url",
    "tags",
    "capabilities",
    "permissions",
    "min_ozma_version",
    "min_display_schema",
    "assets",
    "preview",
    "provenance",
    "confidence",
    "vibe",
    "shape",
    "coherence",
    "signature",
    "published_at",
    "yanked",
    "private",
    "public",
})

Base = declarative_base()


# ── Errors ───────────────────────────────────────────────────────────────────


class AssetError(Exception):
    """Base class for errors that map to a typed HTTP response."""

    status_code = 400
    code = "invalid_asset"

    def __init__(
        self,
        message: str,
        *,
        code: str | None = None,
        status_code: int | None = None,
    ) -> None:
        super().__init__(message)
        if code is not None:
            self.code = code
        if status_code is not None:
            self.status_code = status_code


class InvalidAssetError(AssetError):
    status_code = 400
    code = "invalid_asset"


class AssetNotFoundError(AssetError):
    status_code = 404
    code = "not_found"


class AssetConflictError(AssetError):
    status_code = 409
    code = "conflict"


class StorageError(InvalidAssetError):
    code = "storage_error"


class UnsafeStorageKey(StorageError):
    code = "unsafe_storage_key"


class StorageLimitError(StorageError):
    code = "storage_limit"


class StorageNotFoundError(AssetNotFoundError):
    code = "blob_not_found"


# ── Small helpers ────────────────────────────────────────────────────────────


def utc_now_iso() -> str:
    from datetime import datetime, timezone

    return datetime.now(timezone.utc).isoformat()


def json_dumps(value: Any) -> str:
    """Encode a value for a text column without losing non-ASCII content."""
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def json_loads(text: str | None, default: Any = None) -> Any:
    if not text:
        return default
    try:
        return json.loads(text)
    except (TypeError, ValueError):
        return default


def normalize_license(expression: str) -> str:
    text = (expression or "").strip()
    return _LICENSE_ALIASES.get(text.lower(), text)


def license_is_allowed(
    expression: str,
    allowed: frozenset[str] | set[str] | None = None,
    *,
    first_party: bool = False,
) -> bool:
    """Return whether an SPDX-ish expression may be published.

    ``AND`` requires every component to be allowed and ``OR`` requires at
    least one.  AGPL is added to the allowlist only for first-party packs.
    Unknown or missing licences are rejected.
    """
    allowlist = set(allowed or DEFAULT_ALLOWED_LICENSES)
    if first_party:
        allowlist.update(AGPL_LICENSES)
    text = normalize_license(expression)
    if not text:
        return False
    if text in allowlist:
        return True
    text = text.replace("(", " ").replace(")", " ").strip()
    if " OR " in text:
        return any(
            license_is_allowed(part, allowlist, first_party=first_party)
            for part in text.split(" OR ")
        )
    if " AND " in text:
        return all(
            license_is_allowed(part, allowlist, first_party=first_party)
            for part in text.split(" AND ")
        )
    return False


def flatten_text(value: Any) -> set[str]:
    """Collect lower-case strings from arbitrary metadata values."""
    found: set[str] = set()
    if isinstance(value, str):
        found.add(value.strip().lower())
    elif isinstance(value, Mapping):
        for item in value.values():
            found.update(flatten_text(item))
    elif isinstance(value, (list, tuple, set)):
        for item in value:
            found.update(flatten_text(item))
    return {item for item in found if item}


def as_bool(value: Any, default: bool = False) -> bool:
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        return value.strip().lower() not in {"", "0", "false", "no", "off"}
    return bool(value)


def as_mapping(value: Any) -> dict[str, Any]:
    return dict(value) if isinstance(value, Mapping) else {}


def as_str_list(value: Any) -> list[str]:
    if isinstance(value, str):
        return [value] if value else []
    if isinstance(value, (list, tuple, set)):
        return [str(item) for item in value if isinstance(item, (str, int, float))]
    return []


def version_key(version: str) -> tuple[int, int, int, int, str]:
    """A forgiving semantic-version sort key that never raises."""
    text = str(version or "0").strip()
    main, _, prerelease = text.partition("-")
    parts: list[int] = []
    for chunk in main.split("."):
        match = re.match(r"^(\d+)", chunk)
        parts.append(int(match.group(1)) if match else 0)
    while len(parts) < 3:
        parts.append(0)
    return (parts[0], parts[1], parts[2], 1 if not prerelease else 0, prerelease)


def _uuid() -> str:
    return str(uuid.uuid4())


# ── Models ───────────────────────────────────────────────────────────────────


class AssetPack(Base):
    __tablename__ = "asset_pack"

    id = Column(String(255), primary_key=True)
    type = Column(String(64), nullable=False, default="widget_pack", index=True)
    name = Column(String(512), nullable=False, default="")
    description = Column(Text, nullable=False, default="")
    license = Column(String(255), nullable=False, default="", index=True)
    source_url = Column(Text, nullable=False, default="")
    author = Column(String(512), nullable=False, default="")
    confidence = Column(String(64), nullable=False, default="")
    published_at = Column(String(64), nullable=False, default="")
    yanked = Column(Boolean, nullable=False, default=False, index=True)
    private = Column(Boolean, nullable=False, default=True, index=True)
    search_text = Column(Text, nullable=False, default="")
    capabilities_json = Column(Text, nullable=False, default="[]")
    permissions_json = Column(Text, nullable=False, default="[]")
    extra_json = Column(Text, nullable=False, default="{}")
    created_at = Column(String(64), nullable=False, default=utc_now_iso)
    updated_at = Column(String(64), nullable=False, default=utc_now_iso)

    versions = relationship(
        "AssetVersion",
        back_populates="pack",
        cascade="all, delete-orphan",
        passive_deletes=True,
        order_by="AssetVersion.version",
    )
    tags = relationship(
        "AssetTag",
        back_populates="pack",
        cascade="all, delete-orphan",
        passive_deletes=True,
        order_by="AssetTag.tag",
    )
    provenance = relationship(
        "AssetProvenance",
        back_populates="pack",
        cascade="all, delete-orphan",
        passive_deletes=True,
        order_by="AssetProvenance.id",
    )
    reports = relationship(
        "AssetReport",
        back_populates="pack",
        cascade="all, delete-orphan",
        passive_deletes=True,
        order_by="AssetReport.created_at",
    )
    reviews = relationship(
        "AssetReview",
        back_populates="pack",
        cascade="all, delete-orphan",
        passive_deletes=True,
        order_by="AssetReview.version",
    )
    review_events = relationship(
        "AssetReviewEvent",
        back_populates="pack",
        cascade="all, delete-orphan",
        passive_deletes=True,
        order_by="AssetReviewEvent.created_at",
    )

    def capabilities(self) -> list[str]:
        value = json_loads(self.capabilities_json, [])
        return [str(item) for item in value] if isinstance(value, list) else []

    def permissions(self) -> list[str]:
        value = json_loads(self.permissions_json, [])
        return [str(item) for item in value] if isinstance(value, list) else []

    def extra(self) -> dict[str, Any]:
        value = json_loads(self.extra_json, {})
        return dict(value) if isinstance(value, Mapping) else {}


class AssetVersion(Base):
    __tablename__ = "asset_version"

    pack_id = Column(
        String(255),
        ForeignKey("asset_pack.id", ondelete="CASCADE"),
        primary_key=True,
    )
    version = Column(String(128), primary_key=True)
    manifest_json = Column(Text, nullable=False, default="{}")
    checksums_json = Column(Text, nullable=False, default="{}")
    signature = Column(Text, nullable=False, default="")
    min_ozma_version = Column(String(64), nullable=False, default="")
    min_display_schema = Column(String(128), nullable=False, default="")
    published_at = Column(String(64), nullable=False, default="")
    yanked = Column(Boolean, nullable=False, default=False, index=True)
    created_at = Column(String(64), nullable=False, default=utc_now_iso)

    __table_args__ = (
        UniqueConstraint(
            "pack_id",
            "version",
            name="uq_asset_version_pack_version",
        ),
        Index("ix_asset_version_pack_id", "pack_id"),
    )

    pack = relationship("AssetPack", back_populates="versions")
    files = relationship(
        "AssetFile",
        back_populates="version_record",
        cascade="all, delete-orphan",
        passive_deletes=True,
        order_by="AssetFile.path",
    )

    def manifest(self) -> dict[str, Any]:
        value = json_loads(self.manifest_json, {})
        return dict(value) if isinstance(value, Mapping) else {}

    def checksums(self) -> dict[str, Any]:
        value = json_loads(self.checksums_json, {})
        return dict(value) if isinstance(value, Mapping) else {}


class AssetFile(Base):
    __tablename__ = "asset_file"

    id = Column(String(36), primary_key=True, default=_uuid)
    pack_id = Column(String(255), nullable=False)
    version = Column(String(128), nullable=False)
    path = Column(String(1024), nullable=False)
    sha256 = Column(String(64), nullable=False, default="", index=True)
    size_bytes = Column(Integer, nullable=False, default=0)
    mime_type = Column(String(255), nullable=False, default="")
    executable = Column(Boolean, nullable=False, default=False)
    sandbox = Column(String(64), nullable=True)
    extra_json = Column(Text, nullable=False, default="{}")

    __table_args__ = (
        ForeignKeyConstraint(
            ["pack_id", "version"],
            ["asset_version.pack_id", "asset_version.version"],
            ondelete="CASCADE",
            name="fk_asset_file_version",
        ),
        UniqueConstraint(
            "pack_id",
            "version",
            "path",
            name="uq_asset_file_path",
        ),
        Index("ix_asset_file_pack_version", "pack_id", "version"),
    )

    version_record = relationship(
        "AssetVersion",
        back_populates="files",
        primaryjoin=(
            "and_(AssetFile.pack_id == AssetVersion.pack_id, "
            "AssetFile.version == AssetVersion.version)"
        ),
        foreign_keys="[AssetFile.pack_id, AssetFile.version]",
    )

    def extra(self) -> dict[str, Any]:
        value = json_loads(self.extra_json, {})
        return dict(value) if isinstance(value, Mapping) else {}

    def to_dict(self) -> dict[str, Any]:
        data = self.extra()
        data.update({
            "path": self.path,
            "sha256": self.sha256,
            "size": int(self.size_bytes or 0),
            "size_bytes": int(self.size_bytes or 0),
            "mime": self.mime_type,
            "mime_type": self.mime_type,
            "executable": bool(self.executable),
            "sandbox": self.sandbox,
        })
        return data


class AssetTag(Base):
    __tablename__ = "asset_tag"

    pack_id = Column(
        String(255),
        ForeignKey("asset_pack.id", ondelete="CASCADE"),
        primary_key=True,
    )
    tag = Column(String(255), primary_key=True, index=True)

    pack = relationship("AssetPack", back_populates="tags")


class AssetProvenance(Base):
    __tablename__ = "asset_provenance"

    id = Column(String(36), primary_key=True, default=_uuid)
    pack_id = Column(
        String(255),
        ForeignKey("asset_pack.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    type = Column(String(64), nullable=False, default="")
    source = Column(Text, nullable=False, default="")
    author = Column(String(512), nullable=False, default="")
    method = Column(String(255), nullable=False, default="")
    notes = Column(Text, nullable=False, default="")
    first_party = Column(Boolean, nullable=False, default=False)

    pack = relationship("AssetPack", back_populates="provenance")

    def to_dict(self) -> dict[str, Any]:
        return {
            "type": self.type,
            "source": self.source,
            "author": self.author,
            "method": self.method,
            "notes": self.notes,
            "first_party": bool(self.first_party),
        }


class AssetReport(Base):
    __tablename__ = "asset_report"

    id = Column(String(36), primary_key=True, default=_uuid)
    pack_id = Column(
        String(255),
        ForeignKey("asset_pack.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    version = Column(String(128), nullable=True)
    reason = Column(Text, nullable=False, default="")
    reporter = Column(String(255), nullable=False, default="")
    status = Column(String(32), nullable=False, default="open")
    created_at = Column(String(64), nullable=False, default=utc_now_iso)

    pack = relationship("AssetPack", back_populates="reports")


class AssetReview(Base):
    """Current review decision for a pack version.

    Rows are intentionally optional: a version without a row is treated as
    ``approved`` so pre-review databases remain public.  Public submissions
    created while review is required get an explicit ``pending`` row.
    """

    __tablename__ = "asset_review"

    pack_id = Column(
        String(255),
        ForeignKey("asset_pack.id", ondelete="CASCADE"),
        primary_key=True,
    )
    version = Column(String(128), primary_key=True)
    status = Column(String(32), nullable=False, default="pending", index=True)
    reviewer = Column(String(255), nullable=False, default="")
    note = Column(Text, nullable=False, default="")
    reviewed_at = Column(String(64), nullable=False, default="")
    created_at = Column(String(64), nullable=False, default=utc_now_iso)
    updated_at = Column(String(64), nullable=False, default=utc_now_iso)

    pack = relationship("AssetPack", back_populates="reviews")


class AssetReviewEvent(Base):
    """Append-only audit log for review decisions."""

    __tablename__ = "asset_review_event"

    id = Column(String(36), primary_key=True, default=_uuid)
    pack_id = Column(
        String(255),
        ForeignKey("asset_pack.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    version = Column(String(128), nullable=False, default="")
    status = Column(String(32), nullable=False, default="", index=True)
    previous_status = Column(String(32), nullable=False, default="")
    reviewer = Column(String(255), nullable=False, default="")
    note = Column(Text, nullable=False, default="")
    created_at = Column(String(64), nullable=False, default=utc_now_iso)

    pack = relationship("AssetPack", back_populates="review_events")


# ── Manifest summaries ───────────────────────────────────────────────────────


def manifest_summary(
    pack_id: str,
    manifest: Mapping[str, Any],
) -> dict[str, Any]:
    """Derive the pack-level summary columns from a stored manifest.

    Submission and review approval share this function so approving a pending
    version cannot leave public metadata pointing at unapproved content.
    """
    tags = as_str_list(manifest.get("tags"))
    capabilities = as_str_list(manifest.get("capabilities"))
    permissions = as_str_list(manifest.get("permissions"))
    provenance = as_mapping(manifest.get("provenance"))
    first_party = as_bool(provenance.get("first_party")) or as_bool(
        manifest.get("first_party")
    )
    licence = normalize_license(
        str(manifest.get("license", manifest.get("licence", "")) or "")
    )
    name = str(manifest.get("name") or pack_id)
    description = str(manifest.get("description") or "")
    search_text = " ".join(sorted(
        flatten_text(tags)
        | flatten_text(manifest.get("vibe"))
        | flatten_text(manifest.get("shape"))
        | flatten_text(manifest.get("coherence"))
        | flatten_text(capabilities)
        | flatten_text(permissions)
        | {
            pack_id.lower(),
            name.lower(),
            description.lower(),
            str(manifest.get("author") or "").lower(),
            licence.lower(),
        }
    ))
    extra = {
        key: value
        for key, value in manifest.items()
        if key not in MANIFEST_KNOWN_FIELDS
    }
    return {
        "type": str(manifest.get("type") or "widget_pack"),
        "name": name,
        "description": description,
        "license": licence,
        "source_url": str(manifest.get("source_url") or ""),
        "author": str(manifest.get("author") or ""),
        "confidence": str(manifest.get("confidence") or ""),
        "published_at": str(manifest.get("published_at") or ""),
        "yanked": as_bool(manifest.get("yanked", False)),
        "private": as_bool(manifest.get("private"), True),
        "search_text": search_text,
        "capabilities_json": json_dumps(capabilities),
        "permissions_json": json_dumps(permissions),
        "extra_json": json_dumps(extra),
        "tags": tags,
        "capabilities": capabilities,
        "permissions": permissions,
        "provenance": provenance,
        "first_party": first_party,
    }


def apply_pack_summary(pack: AssetPack, summary: Mapping[str, Any], *, now: str) -> None:
    """Write summary columns and relationship rows onto ``pack``."""
    pack.type = str(summary["type"])
    pack.name = str(summary["name"])
    pack.description = str(summary["description"])
    pack.license = str(summary["license"])
    pack.source_url = str(summary["source_url"])
    pack.author = str(summary["author"])
    pack.confidence = str(summary["confidence"])
    pack.published_at = str(summary["published_at"])
    pack.yanked = bool(summary["yanked"])
    pack.private = bool(summary["private"])
    pack.search_text = str(summary["search_text"])
    pack.capabilities_json = str(summary["capabilities_json"])
    pack.permissions_json = str(summary["permissions_json"])
    pack.extra_json = str(summary["extra_json"])
    provenance = summary.get("provenance")
    provenance = dict(provenance) if isinstance(provenance, Mapping) else {}
    pack.tags = [AssetTag(tag=tag) for tag in summary["tags"]]
    pack.provenance = [
        AssetProvenance(
            pack_id=pack.id,
            type=str(provenance.get("type") or ""),
            source=str(provenance.get("source") or ""),
            author=str(provenance.get("author") or ""),
            method=str(provenance.get("method") or ""),
            notes=str(provenance.get("notes") or ""),
            first_party=bool(summary.get("first_party")),
        )
    ]
    pack.updated_at = now


# ── Schema creation ──────────────────────────────────────────────────────────


def migration_path() -> Path:
    return Path(__file__).resolve().parent / "migrations" / "001_asset_db.sql"


def migration_paths() -> list[Path]:
    """Return every checked-in migration, in lexical (apply) order."""
    directory = Path(__file__).resolve().parent / "migrations"
    return sorted(directory.glob("*.sql"))


def _sql_statements(script: str) -> list[str]:
    statements: list[str] = []
    current: list[str] = []
    for line in script.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("--"):
            continue
        current.append(line)
        if stripped.endswith(";"):
            statement = "\n".join(current).strip()
            statements.append(statement.rstrip(";").strip())
            current = []
    if current:
        statements.append("\n".join(current).strip())
    return [statement for statement in statements if statement]


def create_schema(engine) -> None:
    """Create all asset tables, then apply the portable SQL migrations.

    ``metadata.create_all`` keeps the ORM and the SQL in lockstep for tests.
    Every migration uses ``IF NOT EXISTS`` so applying it afterwards is a safe
    no-op on a freshly created schema and remains usable by operators.
    """
    Base.metadata.create_all(engine)
    for path in migration_paths():
        if not path.exists():
            continue
        statements = _sql_statements(path.read_text(encoding="utf-8"))
        with engine.begin() as connection:
            for statement in statements:
                connection.exec_driver_sql(statement)


def create_sqlite_engine(path: str | Path) -> Any:
    """Small convenience used by tests and local tooling."""
    return create_engine(f"sqlite:///{Path(path)}")


__all__ = [
    "AGPL_LICENSES",
    "ASSET_SCHEMA",
    "INDEX_SCHEMA",
    "PACK_TYPES",
    "SYNC_MODES",
    "DEFAULT_ALLOWED_LICENSES",
    "AssetConflictError",
    "AssetError",
    "AssetFile",
    "AssetNotFoundError",
    "AssetPack",
    "AssetProvenance",
    "AssetReport",
    "AssetReview",
    "AssetReviewEvent",
    "AssetTag",
    "AssetVersion",
    "Base",
    "DEFAULT_REVIEW_STATUS",
    "InvalidAssetError",
    "MANIFEST_KNOWN_FIELDS",
    "REVIEW_STATUSES",
    "StorageError",
    "StorageLimitError",
    "StorageNotFoundError",
    "UnsafeStorageKey",
    "apply_pack_summary",
    "as_bool",
    "as_mapping",
    "as_str_list",
    "create_schema",
    "create_sqlite_engine",
    "flatten_text",
    "json_dumps",
    "json_loads",
    "license_is_allowed",
    "manifest_summary",
    "migration_path",
    "migration_paths",
    "normalize_license",
    "utc_now_iso",
    "version_key",
]
