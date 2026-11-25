# Fuzzing and robustness regression (WGT-082)

This document covers the bounded fuzz regression for display importers,
archives, font metadata parsing, and the Rust/Python SVG/CSS shape extractors.
The goal is narrow and verifiable: a malformed source produces either a typed
error or a lossless capture. It must not panic, hang, execute source code, or
allocate without bound.

The regression is a normal unit/integration test. No fuzz runner, external
service, or new dependency is required in CI.

## What is covered

### Importers (`tests/unit/controller/test_display_fuzz.py`)

A seeded LCG mutates small, valid-ish sources and random bytes for every
importer:

| Importer | Formats exercised |
|---|---|
| AIDA64 | ZIP container, XML, JSON, HTML, opaque random bytes |
| Rainmeter | INI sections/keys, duplicates, malformed lines |
| Home Assistant | YAML dashboards, cards, tags, anchors, nesting |
| MagicMirror | `config.js` objects, comments, strings, expressions |
| Grafana | dashboard/panel JSON, nested panels, constants |
| Adaptive Cards | card JSON, nested elements, actions |

Assertions per case:

- `ImportError` (or a subclass) is accepted as a typed rejection.
- Otherwise the result must be `lossless=True`, `raw` must be retained
  byte-for-byte, `source_capture.source_hash` must equal the SHA-256 of the
  input, and widget output must be bounded.
- Each call has a 2-second wall-clock alarm. A timeout is a test failure, not a
  skipped case.

### Archive attacks (AIDA64 ZIP path)

Archives are built in memory. The suite covers:

- path traversal (`../`, `..\\`), absolute POSIX and Windows paths
- symlink entries, encrypted entries, duplicate names
- declared-vs-actual uncompressed size mismatch
- compression-ratio bombs, too many members, truncated central directory
- corrupt local-header offsets (fuzz-found regression)

All cases must fail closed with `ImportError` or be retained opaquely with a
bounded result. Archive limits live in `controller/display_importers/aida64.py`:
`MAX_MEMBERS`, `MAX_MEMBER_SIZE`, `MAX_TOTAL_UNCOMPRESSED`, and
`MAX_COMPRESSION_RATIO`.

### YAML

Unsafe tags (`!!python/object/apply`, `!!python/name`), alias/anchor expansion,
deep nesting, and huge scalars. Only `yaml.safe_load` is used. Deep flow nesting
is translated to `ImportError`, and the 4 MiB source cap runs first.

### MagicMirror JavaScript

Comments and strings containing braces, unterminated comments/strings,
`require(...)` and function-call values, template interpolation, and nesting
past `MAX_DEPTH`. The importer never evaluates JavaScript: unparseable input is
retained opaquely with `executed: false`. The 1 MiB `MAX_SOURCE_BYTES` limit is
checked before the bounded reader runs.

### Fonts (`FontMetadata.from_bytes`)

Random bytes, truncated SFNT headers, wrong SFNT versions, and absurd table
lengths. Inputs are capped by the test at a few hundred bytes; each call is
alarmed. `WOFF`/`WOFF2` and BDF are rejected as unsupported before parsing.

### Shape extraction (Python and Rust)

- Python: `controller/shape_extract.py` (`extract_from_svg`,
  `extract_from_css`, `MAX_TEXT_INPUT = 256 KiB`)
- Rust: `ozma-display-ir/tests/fuzz_extract.rs` (`extract_from_svg`,
  `extract_from_css`, `parse_svg_path`, same 256 KiB input cap)

Random bytes, entity references, unterminated tags/comments, deep nesting, huge
gradients, unbalanced CSS, and oversize input are exercised. Output collections
must stay bounded by the input size and non-finite geometry is rejected.

## How to run

```bash
python -m pytest tests/unit/controller/test_display_fuzz.py -q
cargo test -p ozma-display-ir --test fuzz_extract
ruff check tests/unit/controller/test_display_fuzz.py
cargo fmt --check --manifest-path ozma-display-ir/Cargo.toml
```

The Python module finishes in roughly 2 seconds and the Rust test in well under
one second after compilation. Both are safe for the default CI tier.

To reproduce a Python failure, use the failing test id and the seed printed in
the assertion. The RNG is a plain LCG (`_Rng`), so a seed and a generator step
uniquely identify the input. To reproduce a Rust failure, the panic message
includes the loop seed; the generator is the `Rng` struct in the test file.

## Optional libFuzzer / AFL guidance

No fuzz crate or AFL harness is added to the repository. If you want coverage
guidance locally, keep the same input caps and use an external runner:

- Rust: expose a temporary `#[no_mangle] pub extern "C" fn LLVMFuzzerTestOneInput`
  shim in a **scratch** crate (not committed) that calls
  `extract_from_svg`, `extract_from_css`, and `parse_svg_path`. Build with
  `cargo +nightly fuzz run` and a `cargo fuzz` `fuzz_targets/` directory in the
  scratch crate. Reject inputs longer than `MAX_TEXT_INPUT` before parsing.
- Python: use Atheris locally against
  `display_importers.registry.import_source` and `FontMetadata.from_bytes`.
  Pass a format hint so detection does not dominate; skip inputs above the
  documented per-format cap.

When a runner finds a crashing input, reduce it, then add it as a normal seeded
case to these test files. Do not commit a fuzz target that downloads corpora,
needs a network service, or runs longer than the CI tier's budget.

## Limits and timeouts

| Layer | Limit | Enforced before expensive parsing |
|---|---|---|
| Import API / store | 32 MiB request (`DEFAULT_MAX_IMPORT_BYTES`) | yes, at the API boundary |
| MagicMirror | 1 MiB, depth 64, 200k nodes | yes, before the bounded reader |
| Adaptive Cards | 2 MiB source, depth 64, export caps | yes, at import entry |
| AIDA64 archives | 4096 members, 64 MiB/member, 256 MiB total, ratio 1000 | yes, before reading each member |
| AIDA64 plain source | 8 MiB (`MAX_SOURCE_BYTES`) | yes, at parse entry |
| Rainmeter | 4 MiB (`MAX_SOURCE_BYTES`) | yes, before decode/parse |
| Home Assistant | 4 MiB (`MAX_SOURCE_BYTES`) | yes, before YAML parse |
| Grafana | 8 MiB (`MAX_SOURCE_BYTES`) | yes, before JSON parse |
| Fonts | 32 MiB (`MAX_SOURCE_BYTES`) | yes, before sfnt parse |
| Shape extraction | 256 KiB (`MAX_TEXT_INPUT`) | yes, first check |
| Test harness | 2 s/case, 24 KiB fuzz inputs | yes, alarm and generator cap |

`test_source_size_limits_are_declared_and_enforced` asserts that every
parser declares and enforces its limit. A new parser without a cap fails the
suite instead of being skipped.

## Triage steps

1. Re-run the single failing test with `-q`; confirm it is deterministic.
2. Classify the failure:
   - **untyped exception** (e.g. `KeyError`, `AssertionError`,
     `RecursionError`, `BadZipFile`, raw `ValueError`): parser must translate
     it to its typed error in the production module.
   - **hang/alarm**: inspect loops and recursive descent; add/correct a
     depth/node/byte cap.
   - **unbounded output**: cap collection sizes or reject the input before the
     parse.
   - **lossy capture**: restore the raw source/asset and set
     `lossless=True`; imports must never drop bytes silently.
3. Minimize the input. Keep it in the test as a seeded case and record the
   production limit or exception class in the failure message.
4. Do not weaken the assertion to `pytest.skip` or a broad `except Exception`.
   If the production fix is out of scope, leave the test failing and track the
   gap in the execution plan.
5. Run all four commands above, then the wider importer/font suites.

## Findings fixed by this suite

The first run surfaced real production gaps. They are now fixed and covered by
regression tests:

- `controller/display_importers/home_assistant.py`: deeply nested YAML flow
  collections now raise `ImportError` instead of leaking `RecursionError`.
- `controller/font_translation.py`: required-table `KeyError`s and hostile
  table lengths are translated to `FontError`; direct parsing is capped at
  32 MiB.
- `controller/display_importers/aida64.py`: malformed ZIP metadata and corrupt
  local-header offsets now raise `ImportError` rather than leaking
  `BadZipFile`/`ValueError`.
- Source cap added before parsing for AIDA64 plain XML/JSON/HTML (8 MiB),
  Rainmeter (4 MiB), Home Assistant (4 MiB), Grafana (8 MiB), and font metadata
  (32 MiB). The API's 32 MiB cap still protects the HTTP boundary; the direct
  parser caps protect import-store and controller callers.
