-- SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
--
-- Review workflow tables for Connect shared assets.
-- Portable SQLite/Postgres SQL.  Every statement is idempotent so operators can
-- re-apply this migration, and create_schema may run it after
-- metadata.create_all on a database that already has the tables.
--
-- A missing row in asset_review means "approved" for backward compatibility
-- with databases created before review support existed.

CREATE TABLE IF NOT EXISTS asset_review (
    pack_id VARCHAR(255) NOT NULL,
    version VARCHAR(128) NOT NULL,
    status VARCHAR(32) NOT NULL DEFAULT 'pending',
    reviewer VARCHAR(255) NOT NULL DEFAULT '',
    note TEXT NOT NULL DEFAULT '',
    reviewed_at VARCHAR(64) NOT NULL DEFAULT '',
    created_at VARCHAR(64) NOT NULL DEFAULT '',
    updated_at VARCHAR(64) NOT NULL DEFAULT '',
    PRIMARY KEY (pack_id, version),
    CONSTRAINT fk_asset_review_pack FOREIGN KEY (pack_id)
        REFERENCES asset_pack (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS ix_asset_review_status ON asset_review (status);

CREATE TABLE IF NOT EXISTS asset_review_event (
    id VARCHAR(36) PRIMARY KEY,
    pack_id VARCHAR(255) NOT NULL,
    version VARCHAR(128) NOT NULL DEFAULT '',
    status VARCHAR(32) NOT NULL DEFAULT '',
    previous_status VARCHAR(32) NOT NULL DEFAULT '',
    reviewer VARCHAR(255) NOT NULL DEFAULT '',
    note TEXT NOT NULL DEFAULT '',
    created_at VARCHAR(64) NOT NULL DEFAULT '',
    CONSTRAINT fk_asset_review_event_pack FOREIGN KEY (pack_id)
        REFERENCES asset_pack (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS ix_asset_review_event_pack_id
    ON asset_review_event (pack_id);
CREATE INDEX IF NOT EXISTS ix_asset_review_event_status
    ON asset_review_event (status);
