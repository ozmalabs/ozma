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

A minimal SQLite deployment:

```bash
export OZMA_CONNECT_ASSET_DB="sqlite:////var/lib/ozma/connect/assets.db"
export OZMA_CONNECT_ASSET_STORAGE="/var/lib/ozma/connect/objects"
export OZMA_CONNECT_ASSET_INDEX_CHANNEL="stable"
export OZMA_CONNECT_ASSET_SIGNING_KEY="$(cat /etc/ozma/connect-private.pem)"
export OZMA_CONNECT_ASSET_SIGNING_KEY_ID="connect-prod-1"
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
the SQL migration in `connect/assets/migrations/001_asset_db.sql` for managed
databases that prefer an out-of-band migration step.

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

Do not expose the database or the object directory. Keep admin/review surfaces
behind authentication when they are added. Controllers must use the public
HTTPS URL, for example `https://connect.example.test/assets/index/stable`.

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
