"""Review workflow for public Connect asset submissions.

Review state is per pack version.  A version without a row in ``asset_review``
is treated as approved, so databases created before this feature keep working
and no public pack disappears merely because review support was deployed.

Public submissions are only marked ``pending`` when the deployment explicitly
enables review (``OZMA_CONNECT_REVIEW_REQUIRED=1``).  Private and first-party
submissions are approved automatically.  Every real transition writes an
append-only ``asset_review_event`` row; repeating the same decision is
idempotent and does not duplicate audit entries.
"""

from __future__ import annotations

from typing import Any

from sqlalchemy import select
from sqlalchemy.orm import Session

from connect.assets.models import (
    DEFAULT_REVIEW_STATUS,
    REVIEW_STATUSES,
    AssetConflictError,
    AssetNotFoundError,
    AssetPack,
    AssetReview,
    AssetReviewEvent,
    AssetVersion,
    InvalidAssetError,
    apply_pack_summary,
    manifest_summary,
    utc_now_iso,
    version_key,
)

_MAX_PAGE_SIZE = 200


def _pagination(limit: int, offset: int) -> tuple[int, int]:
    try:
        limit = int(limit)
        offset = int(offset)
    except (TypeError, ValueError):
        raise InvalidAssetError("limit and offset must be integers") from None
    if limit < 0 or offset < 0:
        raise InvalidAssetError("limit and offset cannot be negative")
    return min(limit, _MAX_PAGE_SIZE), offset


def _review_item(review: AssetReview, pack: AssetPack) -> dict[str, Any]:
    return {
        "id": review.pack_id,
        "pack_id": review.pack_id,
        "version": review.version,
        "status": review.status,
        "reviewer": review.reviewer,
        "note": review.note,
        "reviewed_at": review.reviewed_at,
        "submitted_at": review.created_at,
        "name": pack.name,
        "author": pack.author,
        "type": pack.type,
        "private": bool(pack.private),
    }


class ReviewService:
    """List, approve, and reject public pack versions."""

    def __init__(self, engine, *, required: bool = False) -> None:
        self.engine = engine
        self.required = bool(required)

    # Queries ------------------------------------------------------------

    def list_reviews(
        self,
        status: str | None = "pending",
        *,
        limit: int = 50,
        offset: int = 0,
    ) -> dict[str, Any]:
        if status is not None:
            status = str(status).strip().lower()
            if not status:
                status = None
            elif status not in REVIEW_STATUSES:
                raise InvalidAssetError(
                    f"unknown review status: {status!r}",
                    code="invalid_review_status",
                )
        limit, offset = _pagination(limit, offset)

        statement = (
            select(AssetReview, AssetPack)
            .join(AssetPack, AssetReview.pack_id == AssetPack.id)
            .where(AssetPack.private.is_(False))
        )
        if status is not None:
            statement = statement.where(AssetReview.status == status)
        statement = statement.order_by(
            AssetReview.created_at,
            AssetReview.pack_id,
            AssetReview.version,
        )

        with Session(self.engine) as session:
            rows = session.execute(statement).all()
        items = [_review_item(review, pack) for review, pack in rows]
        total = len(items)
        page = items[offset:offset + limit] if limit else []
        return {
            "reviews": page,
            "entries": page,
            "count": len(page),
            "total": total,
            "status": status,
            "limit": limit,
            "offset": offset,
        }

    # Decisions ----------------------------------------------------------

    def approve(
        self,
        pack_id: str,
        *,
        version: str | None = None,
        note: str = "",
        reviewer: str = "",
    ) -> dict[str, Any]:
        return self.decide(
            pack_id,
            "approved",
            version=version,
            note=note,
            reviewer=reviewer,
        )

    def reject(
        self,
        pack_id: str,
        *,
        version: str | None = None,
        note: str = "",
        reviewer: str = "",
    ) -> dict[str, Any]:
        return self.decide(
            pack_id,
            "rejected",
            version=version,
            note=note,
            reviewer=reviewer,
        )

    def decide(
        self,
        pack_id: str,
        action: str,
        *,
        version: str | None = None,
        note: str = "",
        reviewer: str = "",
    ) -> dict[str, Any]:
        action = str(action or "").strip().lower()
        if action not in {"approved", "rejected"}:
            raise InvalidAssetError(
                f"unknown review decision: {action!r}",
                code="invalid_review_decision",
            )
        note = str(note or "").strip()
        if action == "rejected" and not note:
            raise InvalidAssetError(
                "rejection requires a note",
                code="review_note_required",
            )
        reviewer = str(reviewer or "").strip()[:255]
        now = utc_now_iso()

        with Session(self.engine) as session:
            pack = session.get(AssetPack, str(pack_id))
            if pack is None:
                raise AssetNotFoundError(f"unknown pack {pack_id!r}")
            if pack.private:
                raise AssetConflictError(
                    f"pack {pack_id!r} is private and cannot be reviewed",
                    code="review_conflict",
                )

            selected = self._select_version(pack, version)
            if selected.yanked:
                raise AssetConflictError(
                    f"pack {pack_id!r} version {selected.version!r} is yanked",
                    code="review_conflict",
                )

            review = session.get(
                AssetReview,
                {"pack_id": pack.id, "version": selected.version},
            )
            previous = (
                review.status if review is not None else DEFAULT_REVIEW_STATUS
            )
            unchanged = previous == action
            if review is None:
                review = AssetReview(
                    pack_id=pack.id,
                    version=selected.version,
                    status=previous,
                    created_at=now,
                    updated_at=now,
                )
                if action != previous:
                    session.add(review)

            if not unchanged:
                review.status = action
                review.reviewer = reviewer
                review.note = note
                review.reviewed_at = now
                review.updated_at = now
                if review not in session:
                    session.add(review)
                session.add(AssetReviewEvent(
                    pack_id=pack.id,
                    version=selected.version,
                    status=action,
                    previous_status=previous,
                    reviewer=reviewer,
                    note=note,
                    created_at=now,
                ))
                self._refresh_summary_from_latest_approved(session, pack, now)
                session.commit()

            return {
                "ok": True,
                "id": pack.id,
                "pack_id": pack.id,
                "version": selected.version,
                "status": review.status,
                "reviewer": review.reviewer,
                "note": review.note,
                "reviewed_at": review.reviewed_at,
                "unchanged": unchanged,
                "reviewed": not unchanged,
            }

    @staticmethod
    def _select_version(pack: AssetPack, version: str | None) -> AssetVersion:
        versions = sorted(
            pack.versions,
            key=lambda item: version_key(item.version),
            reverse=True,
        )
        if not versions:
            raise AssetNotFoundError(f"unknown pack {pack.id!r}")
        if version is None or not str(version).strip():
            return versions[0]
        wanted = str(version).strip()
        for item in versions:
            if item.version == wanted:
                return item
        raise AssetNotFoundError(
            f"unknown pack {pack.id!r} version {wanted!r}"
        )

    @staticmethod
    def _refresh_summary_from_latest_approved(
        session: Session,
        pack: AssetPack,
        now: str,
    ) -> None:
        """Keep public summary metadata aligned with the newest approved row.

        A pending version must not change the metadata shown for an older
        approved version, so submission defers the summary update.  Decisions
        re-derive the summary from whichever approved version is now newest; a
        rejection therefore falls back to an older approved version rather than
        leaving rejected metadata in public search results.
        """
        session.flush()
        reviews = session.execute(
            select(AssetReview).where(AssetReview.pack_id == pack.id)
        ).scalars().all()
        statuses = {
            (item.pack_id, item.version): item.status for item in reviews
        }
        approved = [
            item
            for item in pack.versions
            if statuses.get((pack.id, item.version), DEFAULT_REVIEW_STATUS)
            == "approved"
        ]
        if not approved:
            return
        latest = max(approved, key=lambda item: version_key(item.version))
        summary = manifest_summary(pack.id, latest.manifest())
        apply_pack_summary(pack, summary, now=now)
        pack.updated_at = now


__all__ = ["ReviewService"]
