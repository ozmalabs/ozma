# Publishing packs and themes

A publishable display pack is a directory with a `manifest.json`, declared
assets, licence information, provenance, and hashes. Local installs are
deliberately more permissive than public submission; publishing is where the
community licence gate is enforced.

The shared Connect asset database (M2) is implemented in `connect/assets/`.
The manifest and policy below are the contract that local tooling, Connect, and
the licence gate enforce. Public submission requires explicit attestation and an
allowlisted licence; private submissions stay out of public search and indexes.

## Manifest fields

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | Stable pack identifier (`[A-Za-z0-9][A-Za-z0-9._-]*`) |
| `type` | yes | `widget_pack`, `theme_pack`, `description_pack`, or `import_pack` |
| `name` | yes | Human-readable name |
| `version` | yes | Pack version; versions are immutable once published |
| `schema` | recommended | Defaults to `org.ozma.asset-pack/1` |
| `author` | yes in practice | Author or publisher identity |
| `description` | recommended | Short summary |
| `license` / `licence` | yes | SPDX expression, for example `MIT` |
| `source_url` | recommended | Upstream or project URL |
| `tags` | recommended | Searchable keywords |
| `capabilities` | recommended | What the pack can do, for example `rendered_widget` |
| `permissions` | recommended | Requested capabilities such as `internet` |
| `min_ozma_version` | recommended | Oldest compatible Ozma release |
| `min_display_schema` | recommended | Oldest compatible display schema |
| `assets` | yes if files | Path, SHA-256, size, MIME, executable flag, sandbox |
| `checksums` | recommended | `{"algorithm":"sha256","files":{...}}` |
| `preview` | recommended | Pack-relative preview image |
| `provenance` | yes | `type`, `source`, `author`, `method`, `notes` |
| `confidence` | recommended | Confidence in provenance/classification |
| `vibe` / `shape` / `coherence` | optional | Design-language metadata |
| `signature` | optional | Detached/embedded signature value |
| `published_at` | optional | Publication timestamp |
| `yanked` | optional | Withdraw a version from automatic installs |

Unknown fields are preserved on read and write. Do not use that tolerance to
smuggle in renderer-specific state; put extensions in a clearly namespaced
field and document them in the pack README.

## Licence allowlist

Published packs must use one of these SPDX licences:

- MIT
- BSD-2-Clause
- BSD-3-Clause
- ISC
- Apache-2.0
- OFL-1.1
- CC0-1.0

Simple `AND`/`OR` SPDX expressions are evaluated conservatively: every `AND`
branch must be allowed, and at least one `OR` branch must be allowed. An
unknown or missing licence is rejected.

Published mode rejects:

- AGPL and GPL licences;
- CC BY-NC and other non-commercial terms;
- "personal use only" and similar restrictions;
- unknown or custom licences that are not on the allowlist.

Local mode has one narrow exception: an AGPL-family licence is accepted when
the manifest author matches the configured first-party author (default
`ozmalabs`). That keeps the built-in `retro-gauges` pack installable. It does
not make AGPL acceptable for public submission.

Licence metadata is necessary but not sufficient. Do not vendor fonts, icons,
or images whose own terms conflict with the pack licence. Record third-party
notices in the pack and, where required, in the repository
`THIRD_PARTY_NOTICES.md`.

## Provenance and confidence

Provenance answers "where did this come from and how was it made?" A typical
first-party block looks like:

```json
{
  "type": "original",
  "source": "ozmalabs",
  "author": "ozmalabs",
  "method": "authored",
  "notes": "First-party pack shipped with Ozma"
}
```

Use honest values for imported or adapted material, for example
`type: "imported"` with the original source and the user's attestation. Derived
vibe/shape tags carry their own confidence and never replace source or user
tags. `confidence` is a statement, not a score to inflate; reviews may ask for
evidence.

## Checksums and signing

Every declared asset should carry a SHA-256. The local repository verifies
declared hashes, sizes, and undeclared payload files before installing, and
fails closed on mismatch. The licence gate independently re-hashes declared
assets that are present in the checkout.

The repository supports signature verification through an injectable verifier.
If a manifest or remote index carries a signature and no verifier is
configured, verification fails closed. Requiring signatures is a policy
(`verify_signatures` / `require_signature`); unsigned packs can be refused even
when a signature is absent.

Connect signs the canonical index payload (canonical JSON without the signature
envelope) with an injectable Ed25519 or HMAC signer. The controller verifies that
same canonical payload before replacing its cache. Deployments should use
Ed25519 in production and distribute public keys out of band.

- local installs can use hash verification without a signature;
- deployments that enable signature requirements must supply a verifier;
- the licence gate checks licences and hashes only, not signatures.

## Yanking

`yanked: true` withdraws a specific version from automatic resolution:

- yanked versions are excluded from search and default version selection;
- automatic install and update refuse a yanked version;
- an explicit user install of a known version may still proceed;
- yanked versions remain listed when `include_yanked` is requested so audits
  and rollback records do not lose history.

Yanking is not deletion. If material must be removed for legal or security
reasons, coordinate removal of the object and index entry in addition to
yanking the version.

## Review workflow

The Connect submission/review workflow is not live yet. When it lands, the
expected flow is:

1. **Local validation.** The author runs the repository tests and the licence
   gate in local mode.
2. **Pre-submission gate.** The author runs the gate in published mode. This
   rejects third-party AGPL/GPL, non-commercial, personal-use, and unknown
   licences, and checks declared hashes and sandbox declarations.
3. **Submission.** The pack is uploaded with manifest, provenance, previews,
   hashes, and any signature. Submission is explicit; importing a file never
   auto-publishes it.
4. **Review.** A reviewer checks licence compatibility, provenance, bundled
   assets, executable content, sandboxing, and whether the preview is
   representative.
5. **Publication.** The index entry is signed and versioned. Published versions
   are immutable; fixes use a new version.
6. **Post-publication.** Reports, yanks, and security takedowns are recorded
   against the version.

Run the gate with:

```bash
python scripts/check_display_licenses.py --root . --mode published --json
```

A non-zero exit means at least one manifest would fail the published policy.

## Example manifest

```json
{
  "id": "example-gauges",
  "type": "theme_pack",
  "name": "Example Gauges",
  "version": "1.0.0",
  "schema": "org.ozma.asset-pack/1",
  "author": "example-author",
  "description": "Original gauge theme",
  "license": "MIT",
  "source_url": "https://example.test/example-gauges",
  "tags": ["gauges", "minimal"],
  "capabilities": ["rendered_widget"],
  "permissions": [],
  "min_ozma_version": "1.0.0",
  "min_display_schema": "org.ozma.display/1",
  "provenance": {
    "type": "original",
    "source": "example-author",
    "author": "example-author",
    "method": "authored",
    "notes": "Authored for this example"
  },
  "confidence": "exact",
  "assets": [
    {
      "path": "theme.json",
      "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      "size": 1234,
      "mime": "application/json",
      "executable": false,
      "sandbox": null
    }
  ],
  "checksums": {
    "algorithm": "sha256",
    "files": {
      "theme.json": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    }
  },
  "preview": "preview.png"
}
```

See [operations.md](operations.md) for install/sync behaviour and
[fonts.md](fonts.md) for font embedding and substitution. The milestone and
task ledger is in the
[execution plan](../display-platform-execution-plan.md) (§7, WGT-080 and
WGT-034); the licensing rationale is in the
[research document](../display-widget-library-research.md).
