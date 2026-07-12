# ADR-0019: Typed payload mapping in `PgReplicator` (server-side, OID-keyed)

- **Status:** Accepted (shipped)
- **Date:** 2026-07-12

## Context

`PgReplicator::tuple_to_json_payload` rendered every column value as a JSON
**string**, regardless of its Postgres type: `{"done":"false","priority":"5"}`
instead of `{"done":false,"priority":5}`. This was a deliberate deferral
(ADR-0016/ADR-0012 slice 2 shipped predicate evaluation over the all-string
shape defensively, coercing text at match time) — but it is the first thing
every typed consumer hits. W5's Flutter todo showcase needed a dual-format
`_asBool` shim (`bool | String` → `bool`) specifically because a real Postgres
source delivers `"done":"false"`, not the JSON boolean a mock source would use
(`fixtures/flutter/todo/lib/infra/nostos_todo_repository.dart`). The launch
readiness sweep (`docs/plans/launch-readiness-gap-list.md`, item F5) flagged
this as a pre-launch DX blocker: it's the kind of thing a developer discovers
in the first five minutes and loses trust over.

pgoutput's `Relation` message already carries each column's type OID (the wire
protocol's "data type ID") — `RelationMeta` just discarded it. No client
schema artifact is required to fix this: the server already has everything it
needs.

## Decision

Server-side, OID-keyed mapping inside `PgReplicator`, applied identically to
BOTH the streaming path and the initial-snapshot path. Zero config knobs,
zero client-side type machinery — this is the anti-PowerSync differentiator
(no separate sync-rules/schema DSL) and matches the direction Supabase
Realtime and current ElectricSQL both take.

`RelationMeta.columns` becomes `Vec<(String, i32)>` — `(column name, type
OID)` — populated from pgoutput `Relation` messages
(`pg.rs::cache_relation`) and from a shared catalog query
(`pg.rs::catalog_relations`, used by both the streaming bootstrap and the
snapshot path). ONE mapping function,
`typed::append_typed_value(out, type_oid, cell)`, is the single place a
column's TEXT-mode value becomes JSON — both
`pg.rs::tuple_to_json_payload` (streaming) and
`snapshot.rs::build_json_payload` (initial snapshot) call it. This is what
makes a snapshot row and a streamed row of identical content render
byte-identical JSON (proven directly by
`e2e_pg_typed_payload.rs::snapshot_and_streamed_rows_of_identical_content_are_byte_identical`).

### Mapping table

| Postgres type (OID) | JSON shape |
|---|---|
| `bool` (16) | JSON bool |
| `int2` (21), `int4` (23) | JSON number |
| `float4` (700), `float8` (701) | JSON number; `NaN`/`Infinity`/`-Infinity` → quoted string |
| `int8` (20), `oid` (26), `numeric` (1700), `money` (790) | JSON string (decimal/formatted text as Postgres sends it) |
| `timestamp` (1114), `timestamptz` (1184), `timetz` (1266) | JSON string, RFC 3339 UTC (`...Z`) |
| `date` (1082), `time` (1083) | JSON string, passthrough (no timezone concept to normalize) |
| `uuid` (2950) | JSON string, canonical lowercase text as-is |
| `json` (114), `jsonb` (3802) | JSON string containing the serialized JSON text (Debezium convention — NOT embedded as a JSON value) |
| `bytea` (17) | JSON string, base64 (Postgres's hex `\x...` text form decoded and re-encoded) |
| enum / array / domain / anything unrecognized | JSON string passthrough (today's default behavior, unchanged) |
| SQL `NULL` (any type) | JSON `null` — always, regardless of the column's type; never a fabricated `false`/`0`/`""` |

A value that claims a numeric/bool/timestamp OID but fails to parse (corrupt
text, an unsupported session `DateStyle`, a non-hex `bytea`, ...) falls back
to quoted-string passthrough. **A parse failure never drops a row and never
panics** — the ceiling is "less precise than expected for this one field,"
not "sync stops."

### The int8-as-string call

`int8`/`oid`/`numeric`/`money` render as JSON strings, not numbers, even
though the "true" mapping for `int8` would naively look like a plain JSON
number. This was the one genuinely contested decision, settled 2026-07-12 by
a cross-industry, non-CDC-tool survey:

- **CDC tools mostly emit int64 as JSON numbers** (Debezium's default JSON
  converter, wal2json, several others) — this is the "precedent" that would
  argue for a number.
- **Every general-purpose JSON API convention outside CDC is unanimous the
  other way**: Protobuf's canonical JSON mapping (int64 → string), Google API
  Discovery documents (`format: int64` → string), Twitter's API (`id` +
  `id_str`, the latter existing *specifically* because JS/JSON int64
  precision loss bit real consumers), the OpenAPI `int64` format convention,
  GraphQL (no native 64-bit int scalar — big integers are `String`/custom
  scalars by convention), and Stripe/Microsoft Graph's practice of treating
  large/opaque IDs as strings.
- **The concrete, currently-shipped cautionary tale**: Supabase Realtime has
  a documented `parseFloat`-on-`bigint` precision bug — a JS consumer that
  naively numifies a large Postgres `int8`/`bigint` silently loses precision.
  That is exactly the failure mode this decision avoids by construction
  (there is nothing to naively numify if the wire already says "string").
- **Nostos's own official client makes this concrete, not hypothetical**:
  `dart2js` compiles Dart's `int` to a JS `number`, which is IEEE-754
  double-bounded — exactly the 2^53 ceiling. Flutter Web is in Nostos's
  supported target set, so Nostos's own first-party SDK is in the vulnerable
  population if `int8` ships as a bare number.

Given the ceiling is real for the project's own supported clients (not just a
theoretical third-party consumer), and the CDC-tool precedent doesn't hold up
against the near-unanimous non-CDC JSON API convention, `int8`/`oid`
(sequence-backed, can exceed 2^53 for high-volume tables) /`numeric` (needs
arbitrary precision, not float-lossy) /`money` (Postgres's locale-formatted
text, not parseable as a bare decimal anyway) all render as strings.

**The SDK contract this implies**: consuming SDKs (including Nostos's own)
must never call `Number()`/`parseFloat()`/`int.parse()`-into-a-double on these
string-typed columns as if they were pre-parsed numbers — they are opaque
precision-preserving text. Dart: `int.parse` (exact for the `int8` range) or
a `Decimal` type for `numeric`. This is documented in
`sdk/nostos_flutter/README.md` and `docs/QUICKSTART.md`.

### The NaN/Infinity guard

RFC 8259 (the JSON spec) does not define `NaN`, `Infinity`, or `-Infinity` as
valid JSON tokens — many parsers accept them as an extension, but a strict
parser (and most `JSON.parse` implementations) will reject a bare `NaN` in
the token stream. Postgres's `float4`/`float8` text output uses exactly those
three spellings for non-finite values, so `typed::append_float` special-cases
them to quoted strings before attempting a numeric parse (both because they
would emit invalid JSON as bare tokens, AND because Rust's own `f64::from_str`
accepts those spellings case-insensitively, which would otherwise silently
produce a non-finite value from the generic numeric-parse path).

### Wire-compat note

Pre-launch, no released clients exist yet — this is a breaking change to the
wire payload shape, but it's **free right now**. After the v0.1 tag ships,
changing an already-typed column's JSON representation becomes a real
breaking change for real consumers.

### Deliberately NOT done

- **pgoutput binary mode** stays off. Text-mode parsing only — the module
  docs on `pg.rs` already explain why (`BinaryValueTraitOff`: the whole tuple
  image is stored as opaque text-decodable payload, which keeps the payload
  debuggable and lets the wire codec re-hex it without a binary decoder).
- **Arrays, enum label resolution, domain unwrapping** beyond string
  passthrough. ponytail: the upgrade path is a relation-metadata wire frame
  so the client can materialize enum labels / array element types without
  the server guessing at parse time — named explicitly rather than silently
  deferred; see the `ponytail:` comment on `typed::append_typed_value`'s
  wildcard arm.
- **Toasted-unchanged columns** (`TupleDataColumn::PGUnchangedToastedValue`,
  only reachable for toastable types — text/bytea/json/jsonb/numeric, all of
  which already map to the quoted-string branch) render as an empty string
  placeholder, matching pre-ADR-0019 behavior. This is type-safe (every
  toastable builtin OID lands in the string branch) but ambiguous with a
  genuinely empty value. ponytail: `REPLICA IDENTITY FULL` (forces Postgres
  to always resend the full tuple) or a distinct wire sentinel are the
  upgrade paths; see the comment at `pg.rs::tuple_to_json_payload`.

## Consequences

- **Positive**: typed consumers (Flutter/Dart, JS, any JSON consumer) get
  real `bool`/number JSON without a client-side coercion shim. The W5 todo
  showcase's `_asBool` dual-format shim (`bool | String` → `bool`) becomes
  dead code for the real-Postgres path — kept as-is since it's harmless and
  costs nothing to leave (a mock/fake source could still emit either shape).
- **Positive**: `crates/nostos-infra/src/replicator/extract.rs`'s
  `json_value_to_column_value`, which already defensively handled
  `Value::Number`/`Value::Bool` "in case a future payload format emits typed
  JSON" (its own doc comment), needed ZERO changes — that defensive path is
  now the primary path, not a hypothetical one.
- **Positive**: `nostos-domain::predicate::cmp_op`'s `Number`-vs-`Number` /
  `Bool`-vs-`Bool` direct-comparison arms now apply on real traffic, not just
  the `Text`-coercion fallback arms — verified by the existing predicate test
  suite passing unchanged, plus the full pg e2e suite (105+ tests) green
  after the change.
- **Negative / accepted**: the OID→JSON mapping is a hardcoded `match` over
  ~20 builtin OIDs. Adding a new special-cased type (e.g. `interval`, arrays)
  means touching `typed.rs`'s `oid` module and match arm — an acceptable
  single-file-touch cost for the DX win.
- **Deferred**: arrays and full enum-label materialization remain string
  passthrough (see "Deliberately NOT done" above).

## Files

- `crates/nostos-infra/src/replicator/typed.rs` — the mapping function, the
  OID table, timestamp/base64 helpers, and their unit tests (new file).
- `crates/nostos-infra/src/replicator/pg.rs` — `RelationMeta.columns` now
  `Vec<(String, i32)>`; `cache_relation` captures the type OID from pgoutput
  `Relation` messages; the new shared `catalog_relations` (extracted from
  the former `bootstrap_relations_from_catalog`, now selects `atttypid`
  too); `tuple_to_json_payload` rewritten to call `typed::append_typed_value`.
- `crates/nostos-infra/src/replicator/snapshot.rs` — `catalog_relations`
  duplicate query removed (calls `pg::catalog_relations`); `unescape_copy_field`
  now returns `Option<String>` (was `String`) so SQL NULL survives distinctly
  through to `typed::append_typed_value` instead of collapsing to `""`;
  `build_json_payload`/`build_pk_string` updated to match.
- `crates/nostos-infra/tests/e2e_pg_typed_payload.rs` — real-Postgres e2e:
  every mapped OID, asserted for both the snapshot path and the streaming
  path, plus the byte-identical cross-path assertion (new file).
