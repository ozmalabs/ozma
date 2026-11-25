# Deploying the Connect shared asset service

Connect exposes the shared display asset database under `/assets`. The service
is optional: when no database is configured, every `/assets` route returns
`503 asset_service_unavailable` and the rest of Connect keeps working.

`connect/assets/bootstrap.py` reads deployment configuration when
`connect.main` is imported. All settings are environment variables; no secrets
are bundled with the application.

## Environment variables

| Variable | Required | Meaning |
|---|---|---|
| `OZMA_CONNECT_ASSET_DB` | yes to enable | SQLAlchemy URL for the asset database. `OZMA_CONNECT_DATABASE_URL` is accepted as a legacy alias. Unset or empty disables the service. |
| `OZMA_CONNECT_ASSET_STORAGE` | no | Filesystem directory for content-addressed payloads. Default `./connect-assets`. Created with mode `0700` when new. |
| `OZMA_CONNECT_ASSET_SIGNING_KEY` | no | Ed25519 private key as raw base64/hex or PEM. Production deployments should set this so the index is signed. HMAC keys are not accepted by the bootstrap path. |
| `OZMA_CONNECT_ASSET_SIGNING_KEY_ID` | no | Key identifier recorded in the signed index. Default `connect`; must match the controller's trust store. |
| `OZMA_CONNECT_ASSET_INDEX_CHANNEL` | no | Default channel recorded on `app.state.asset_index_channel`. Default `stable`. |
| `OZMA_CONNECT_REVIEW_REQUIRED` | no | When `1`/`true`/`yes`/`on`, new public submissions are held as `pending` and excluded from public search, stats, and indexes until approved. Default off keeps today's behaviour and approves public submissions automatically. |
| `OZMA_CONNECT_REVIEW_TOKEN` | no | Shared secret for the admin review API. Unset means `/assets/review*` returns `503 review_unavailable`; review is never open by default. |

A minimal SQLite deployment:

```bash
export OZMA_CONNECT_ASSET_DB="sqlite:////var/lib/ozma/connect/assets.db"
export OZMA_CONNECT_ASSET_STORAGE="/var/lib/ozma/connect/objects"
export OZMA_CONNECT_ASSET_INDEX_CHANNEL="stable"
export OZMA_CONNECT_ASSET_SIGNING_KEY="$(cat /etc/ozma/connect-private.pem)"
export OZMA_CONNECT_ASSET_SIGNING_KEY_ID="connect-prod-1"
export OZMA_CONNECT_REVIEW_REQUIRED="1"
export OZMA_CONNECT_REVIEW_TOKEN="$(cat /etc/ozma/connect-review-token)"
```

Invalid database URLs or keys raise a typed `BootstrapError`. Connect logs a
sanitized warning and leaves `/assets` unavailable; URLs and key material are
never logged.

## Storage layout

```text
/var/lib/ozma/connect/assets.db          # relational metadata
/var/lib/ozma/connect/objects/
  sha256/ab/abcdef...                    # content-addressed payloads
```

The SQLite database and the object directory are separate failure domains.
Back them up together, or keep the database on a managed Postgres service and
the object directory on durable local storage.

## SQLite or Postgres

- **SQLite** is suitable for a single Connect process and small deployments.
  The bootstrap engine sets `check_same_thread=False`; in-memory URLs use a
  shared static pool. Do not run multiple writers against the same file.
- **Postgres** is recommended for multiple workers or hosts. Use a SQLAlchemy
  URL with the installed driver, for example
  `postgresql+psycopg://connect:password@db.example.test/ozma_connect`.

The service creates the portable schema on startup (idempotent) and also ships
the SQL migrations in `connect/assets/migrations/` for managed databases that
prefer an out-of-band migration step:

- `001_asset_db.sql` creates the core asset tables;
- `002_asset_review.sql` adds the optional review and audit tables.

Both migrations use `IF NOT EXISTS`, so applying them after startup or
re-applying them is safe on SQLite and Postgres. A version with no row in
`asset_review` is treated as approved, so deploying review support does not
retroactively hide existing public packs.

## Signing keys

Generate an Ed25519 key pair with the repository tool:

```bash
python scripts/display_keys.py generate \
  --key-id connect-prod-1 \
  --private-out /etc/ozma/connect-private.pem \
  --public-out /etc/ozma/connect-public.json
```

Keep the private key off the controller and out of logs. Set
`OZMA_CONNECT_ASSET_SIGNING_KEY` from the key contents and distribute the public
document to controllers out of band. `/assets/index/{channel}` then contains an
`ed25519` signature envelope over the canonical unsigned index.
`scripts/display_keys.py verify-index` can verify a downloaded index.

## Reverse proxy and TLS

Terminate TLS at a reverse proxy and expose only the required paths:

```nginx
location /assets/ {
    proxy_pass http://127.0.0.1:8000/assets/;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto https;
    client_max_body_size 32m;   # match the submission limit you intend to allow
}
```

Do not expose the database or the object directory. Review endpoints are
authenticated with `X-Ozma-Review-Token` and should additionally be restricted
at the proxy to trusted operator networks. Controllers must use the public
HTTPS URL, for example `https://connect.example.test/assets/index/stable`.

## Review workflow and gallery

Review is optional and backward compatible:

- private submissions and first-party public submissions are approved
  automatically;
- with `OZMA_CONNECT_REVIEW_REQUIRED` off, public submissions are approved
  automatically, exactly as before;
- with it on, new public submissions start as `pending` and are excluded from
  `/assets/search`, `/assets/stats`, `/assets/categories`, `/assets/versions`,
  `/assets/entry`, and every signed index until approved;
- rejected versions stay excluded; private packs remain excluded from public
  views regardless of review state.

The review API requires `OZMA_CONNECT_REVIEW_TOKEN` and the matching
`X-Ozma-Review-Token` request header. Missing header returns `401`; a wrong
token returns `403`; an unset deployment token returns `503 review_unavailable`.
The token is never logged.

```http
GET  /assets/review?status=pending&limit=50&offset=0
POST /assets/review/{pack_id}/approve
POST /assets/review/{pack_id}/reject
X-Ozma-Review-Token: <token>
Content-Type: application/json

{"version": "1.2.0", "note": "licence and provenance checked"}
```

`version` is optional and defaults to the newest version. `note` is required
for rejection. Decisions are idempotent: repeating an approval or rejection
returns the existing state without adding another audit row. Every real
transition writes an append-only `asset_review_event` row with the previous
status, reviewer, note, and timestamp. An optional
`X-Ozma-Review-Reviewer` header records an operator name (default `admin`).
Unknown packs or versions return `404`; private or yanked review targets return
`409`; malformed bodies and a missing rejection note return `400`.

`GET /assets/gallery` is a public, read-only HTML page listing approved public
packs with name, author, version, tags, licence, and links to
`/assets/entry/{id}`. It accepts optional `q`, `type`, and `tag` query filters.
The page is self-contained: no scripts, external styles, images, or network
requests. It sends a strict `default-src 'none'; style-src 'unsafe-inline'`
CSP as both a meta tag and a response header, and HTML-escapes every
manifest-derived value. It never lists private, pending, or rejected packs.

## Backup and restore

1. Stop writes, or snapshot the database and object directory at the same
   point in time.
2. Back up the database plus the full `OZMA_CONNECT_ASSET_STORAGE` tree.
3. Back up the signing key separately; losing it prevents publishing a
   verifiable index under the same key id.
4. Restore the database first, then the object directory, and restart Connect.
5. Check `GET /assets/index/stable` and verify the signature before pointing
   controllers at the restored service.

Content-addressed objects are immutable, so incremental backup tools can treat
them as append-only until a pack is deliberately removed.

## How controllers sync

On each controller, opt into shared assets and point at the channel endpoint:

```bash
export OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED=1
export OZMA_DISPLAY_SYNC_MODE=manual          # or installed/full
export OZMA_DISPLAY_ASSET_INDEX_URL="https://connect.example.test/assets/index/stable"
export OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS=0
```

The controller sends `If-None-Match` with its cached ETag and accepts `304`.
It replaces its local cache only after the canonical index hash and signature
verify against the trusted public key. A failed sync keeps the previous cached
index, so `offline` mode continues to render without Connect.

See [operations.md](operations.md) for controller-side sync modes and
[publishing.md](publishing.md) for the manifest and licence contract.
