-- SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
--
-- Portable SQL mirror of connect/assets/models.py.
-- Uses only common SQLite/Postgres types and IF NOT EXISTS, so it is safe to
-- apply after metadata.create_all (the create_schema helper does exactly that)
-- and idempotent for operators running it directly.

CREATE TABLE IF NOT EXISTS asset_pack (
    id VARCHAR(255) PRIMARY KEY,
    type VARCHAR(64) NOT NULL DEFAULT 'widget_pack',
    name VARCHAR(512) NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    license VARCHAR(255) NOT NULL DEFAULT '',
    source_url TEXT NOT NULL DEFAULT '',
    author VARCHAR(512) NOT NULL DEFAULT '',
    confidence VARCHAR(64) NOT NULL DEFAULT '',
    published_at VARCHAR(64) NOT NULL DEFAULT '',
    yanked BOOLEAN NOT NULL DEFAULT FALSE,
    private BOOLEAN NOT NULL DEFAULT TRUE,
    search_text TEXT NOT NULL DEFAULT '',
    capabilities_json TEXT NOT NULL DEFAULT '[]',
    permissions_json TEXT NOT NULL DEFAULT '[]',
    extra_json TEXT NOT NULL DEFAULT '{}',
    created_at VARCHAR(64) NOT NULL DEFAULT '',
    updated_at VARCHAR(64) NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS ix_asset_pack_type ON asset_pack (type);
CREATE INDEX IF NOT EXISTS ix_asset_pack_license ON asset_pack (license);
CREATE INDEX IF NOT EXISTS ix_asset_pack_yanked ON asset_pack (yanked);
CREATE INDEX IF NOT EXISTS ix_asset_pack_private ON asset_pack (private);

CREATE TABLE IF NOT EXISTS asset_version (
    pack_id VARCHAR(255) NOT NULL,
    version VARCHAR(128) NOT NULL,
    manifest_json TEXT NOT NULL DEFAULT '{}',
    checksums_json TEXT NOT NULL DEFAULT '{}',
    signature TEXT NOT NULL DEFAULT '',
    min_ozma_version VARCHAR(64) NOT NULL DEFAULT '',
    min_display_schema VARCHAR(128) NOT NULL DEFAULT '',
    published_at VARCHAR(64) NOT NULL DEFAULT '',
    yanked BOOLEAN NOT NULL DEFAULT FALSE,
    created_at VARCHAR(64) NOT NULL DEFAULT '',
    PRIMARY KEY (pack_id, version),
    CONSTRAINT uq_asset_version_pack_version UNIQUE (pack_id, version),
    CONSTRAINT fk_asset_version_pack FOREIGN KEY (pack_id)
        REFERENCES asset_pack (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS ix_asset_version_pack_id ON asset_version (pack_id);
CREATE INDEX IF NOT EXISTS ix_asset_version_yanked ON asset_version (yanked);

CREATE TABLE IF NOT EXISTS asset_file (
    id VARCHAR(36) PRIMARY KEY,
    pack_id VARCHAR(255) NOT NULL,
    version VARCHAR(128) NOT NULL,
    path VARCHAR(1024) NOT NULL,
    sha256 VARCHAR(64) NOT NULL DEFAULT '',
    size_bytes INTEGER NOT NULL DEFAULT 0,
    mime_type VARCHAR(255) NOT NULL DEFAULT '',
    executable BOOLEAN NOT NULL DEFAULT FALSE,
    sandbox VARCHAR(64),
    extra_json TEXT NOT NULL DEFAULT '{}',
    CONSTRAINT fk_asset_file_version FOREIGN KEY (pack_id, version)
        REFERENCES asset_version (pack_id, version) ON DELETE CASCADE,
    CONSTRAINT uq_asset_file_path UNIQUE (pack_id, version, path)
);

CREATE INDEX IF NOT EXISTS ix_asset_file_pack_version
    ON asset_file (pack_id, version);
CREATE INDEX IF NOT EXISTS ix_asset_file_sha256 ON asset_file (sha256);

CREATE TABLE IF NOT EXISTS asset_tag (
    pack_id VARCHAR(255) NOT NULL,
    tag VARCHAR(255) NOT NULL,
    PRIMARY KEY (pack_id, tag),
    CONSTRAINT fk_asset_tag_pack FOREIGN KEY (pack_id)
        REFERENCES asset_pack (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS ix_asset_tag_tag ON asset_tag (tag);

CREATE TABLE IF NOT EXISTS asset_provenance (
    id VARCHAR(36) PRIMARY KEY,
    pack_id VARCHAR(255) NOT NULL,
    type VARCHAR(64) NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT '',
    author VARCHAR(512) NOT NULL DEFAULT '',
    method VARCHAR(255) NOT NULL DEFAULT '',
    notes TEXT NOT NULL DEFAULT '',
    first_party BOOLEAN NOT NULL DEFAULT FALSE,
    CONSTRAINT fk_asset_provenance_pack FOREIGN KEY (pack_id)
        REFERENCES asset_pack (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS ix_asset_provenance_pack_id
    ON asset_provenance (pack_id);

CREATE TABLE IF NOT EXISTS asset_report (
    id VARCHAR(36) PRIMARY KEY,
    pack_id VARCHAR(255) NOT NULL,
    version VARCHAR(128),
    reason TEXT NOT NULL DEFAULT '',
    reporter VARCHAR(255) NOT NULL DEFAULT '',
    status VARCHAR(32) NOT NULL DEFAULT 'open',
    created_at VARCHAR(64) NOT NULL DEFAULT '',
    CONSTRAINT fk_asset_report_pack FOREIGN KEY (pack_id)
        REFERENCES asset_pack (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS ix_asset_report_pack_id ON asset_report (pack_id);
