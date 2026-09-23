# Atlet — nostos benchmark suite

Benchmark-first training app (Atlet design) exercising every nostos SDK against
one Supabase database. The app runs direct-mode nostos (ADR-0045) and nothing
else — no engine picker; the Analytics tab's suite is what still runs two
engines, server-mode against direct-mode, behind the same adapter.

## Isolation rules
- Not a Cargo workspace member. `make ci` and `sdk-e2e` never touch this tree.
- Each SDK app dir is fully self-contained (own lockfile, own build).

## Numbers policy
All numbers produced here are **internal evaluation — not a published benchmark**.
Publication requires docs/BENCHMARK-METHODOLOGY.md conformance + landing in
benches/results/RESULTS.md.
