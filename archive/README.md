# archive/ — reference only

**Nothing in here is built, tested, linted, published, or run by any `make` target or CI job.**
It is kept for reference and nothing else. Do not wire it back into a build; if you need something
here, move it out deliberately or copy the pattern.

Archived 2026-07-30 by operator decision: no example app should live inside an SDK
("no examples app to live in the SDK", then "archive the tests as well - take it all").

## Contents

| Path | Was | Files |
|---|---|---|
| `sdk/nostos_flutter/example/` | The Provider Dashboard host app — availabilities / clients / chat views over `Collection<T>.watch()`. Its `integration_test/nostos_server_test.dart` drove `connect → subscribe → fan-out → watch() emits` against a **real** `cargo run -p nostos-server` inside a genuine macOS `.app`. | 109 |
| `sdk/nostos_flutter/test/` | The Flutter SDK's Dart unit tests: `nostos_test.dart`, `nostos_facade_test.dart`, `nostos_ws6_test.dart`, `nostos_config_test.dart`. | 4 |
| `sdk/nostos_flutter/test_driver/` | `integration_test.dart` driver harness. | 1 |

## What this cost, recorded honestly

Both losses are real and neither is cosmetic:

1. **The Flutter SDK has no automated test coverage of its Dart surface.** `.github/workflows/ci.yml`'s
   `flutter` job ran `flutter test` on every push; that step is gone because there are no tests left
   to run. `flutter analyze` still runs.
2. **The Flutter SDK has no live sync proof.** `scripts/sdk-e2e.sh` dropped `flutter` from
   `ALL_SLICES`, taking the suite from **10 live slices to 9**. It is deliberately *not* a
   `skip_slice`: `SDK_E2E_STRICT=1` in CI converts a SKIP to a failure, and a listed slice would
   claim PUSH+ECHO coverage that no longer exists. Running `./scripts/sdk-e2e.sh flutter`
   explicitly now fails loudly with a pointer here.

One thing was **saved** rather than lost: `sdk/nostos_flutter/scripts/check-doc-signatures.py` used to
run only inside that sdk-e2e slice. It moved to the `flutter` job in `.github/workflows/ci.yml`, so it
keeps running. It is the check that caught `README.md` and `USAGE.md` both documenting three
`NostosDatabase.supabase` parameters that never existed — `make ci` is Rust-only and `dart analyze`
does not compile fenced markdown, so nothing else validates prose against `lib/`.

## Restoring

Everything is a plain `git mv` away, and the history is intact — these were moved, not rewritten:

```sh
git log --follow archive/sdk/nostos_flutter/example/lib/main.dart
```

The intended replacement is **not** a restore. It is a Flutter host app plus Dart tests under
`fixtures/`, per [`../docs/plans/multi-sdk-pomodoro-fixture-matrix.md`](../docs/plans/multi-sdk-pomodoro-fixture-matrix.md).
Until that lands, the two gaps above are open.

## Not archived, and why

- `crates/nostos-client/examples/` — `reactive_scroll.rs` is a **documented project verb** in
  `CLAUDE.md` (`cargo run -p nostos-client --example reactive_scroll`).
- `crates/nostos-infra/examples/` — `e2e_server.rs` is the **shared spine that 9 of the 10 sdk-e2e
  slices spawn.** Archiving it would break nine slices, not one. It is test infrastructure that
  happens to live in an `examples/` dir, not an example app.
- `web/src/routes/demo/` — a route in the marketing site, not an SDK.
- Other SDKs' `test/` and `e2e/` directories — these are wired into the 9 surviving live slices.
