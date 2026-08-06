# Atlet — nostos vs PowerSync comparison suite

Benchmark-first training app (Atlet design) exercising every nostos SDK against
one Supabase database, with PowerSync behind the same adapter for neck-to-neck
internal evaluation.

## Isolation rules
- Not a Cargo workspace member. `make ci` and `sdk-e2e` never touch this tree.
- Each SDK app dir is fully self-contained (own lockfile, own build).

## Numbers policy
All numbers produced here are **internal evaluation — not a published benchmark**.
Publication requires: FSL legal review + docs/BENCHMARK-METHODOLOGY.md conformance
+ landing in benches/results/RESULTS.md.
