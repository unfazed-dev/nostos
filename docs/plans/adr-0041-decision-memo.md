# ADR-0041 Decision Memo — accept/reject packet for `spike/iroh-transport`

- **Date:** 2026-08-29 (evidence updated in a second pass the same day: spike suite re-run, merge rehearsed, mobile build viability verified — see §1 and §6)
- **Decision requested:** accept or reject ADR-0041 (transport abstraction — `ws` | `iroh` as a first-class option), currently **Proposed**. Spike branch `spike/iroh-transport` (single commit `680852f`) is green and waiting.
- **Recommendation:** **Accept, with conditions** (§4). The spike passes the ADR's own conformance bar over both transports; every unfinished item is already enumerated as accept-gated in the spike ADR itself.

## 1. What the spike proves

| # | Claim | Status | Evidence |
|---|---|---|---|
| 1 | Same fixture + assertions pass over `ws://` and `iroh://` — snapshot rows arrive, checkpoint advances, reconnect idempotent | **verified 2026-08-29 (re-run)** | `cargo test -p nostos-client --features iroh --test iroh_ws_conformance` at `680852f` in a detached worktree: **both legs green in 1.74 s** — one shared `conformance_leg(url)` driven by `conformance_over_ws` / `conformance_over_iroh` |
| 2 | iroh fully OFF-default; default builds gain zero dependency weight | verified 2026-08-29 | `nostos-infra`: `default = ["webpush"]`, `iroh = ["dep:iroh", "dep:iroh-tickets"]`; `nostos-server`: `default = ["pg"]`, `iroh = ["nostos-infra/iroh"]`; `nostos-client`: no default features; workspace pin `iroh = "1.1"` |
| 3 | Shipped on iroh **1.1.0** — the proposal's 0.91.2 doc citations predate the 1.x line (`NodeAddr`→`EndpointAddr`, tickets → `iroh-tickets`) | verified 2026-08-29 | spike `Cargo.lock`: `iroh` / `iroh-base` / `iroh-dns` all `1.1.0` |
| 4 | Server shape under `NOSTOS_TRANSPORT=iroh`: HTTP surface binds loopback-only, iroh accept loop bridges bi-streams, boot prints QR-native `dial_url=iroh://…/sync?ticket=…` | verified 2026-08-29 (diff inspection) | `nostos-server/src/main.rs`: `--transport`/`NOSTOS_TRANSPORT` default `"ws"`; `TcpListener::bind("127.0.0.1:0")`; bridge ponytail recorded in code comments |
| 5 | Client shape: dial-by-scheme; the iroh leg runs the standard WS handshake over the QUIC stream — session loop untouched (`SyncWs` enum unifies stream types) | verified 2026-08-29 (diff inspection) | `nostos-client/src/client.rs` ±35, new `iroh_dial.rs` +302 |
| 6 | Merge cost onto today's main: **one hunk, rehearsed and green** | **verified 2026-08-29 (rehearsed)** | Trial merge in a detached worktree: the only conflict is `reset_subscribed` in `client.rs` (spike's older closure vs main's clippy-fixed `std::mem::take` — resolved to main's form). Merged tree: conformance green (1.75 s), `clippy -p nostos-client --features iroh --all-targets -- -D warnings` clean, `cargo check -p nostos-server --features iroh` clean. Rehearsal discarded; the resolution is now known and mechanical |

**True spike footprint** (merge-base `9a8cfc6`): 14 files, +2909/−113 — of which +2206 is `Cargo.lock` (iroh + quinn tree, off-default). Main has moved **9** commits since the fork (tenant-CRDT trio, ADR-0040 tests, fmt/clippy, FRB regen, status addendum). The raw `main..spike` two-dot diff overstates the spike with reverse-applied main changes — ignore it.

## 2. What accepting means

- Merge `spike/iroh-transport` → main (the conflict and its resolution are already rehearsed, §1.6), flip ADR-0041 status `Proposed → Accepted` (dated), and update the References line from iroh 0.91.2 to 1.1.0 docs.
- iroh stays an off-default build option; the default `ws` path is byte-for-byte the status quo.
- The accept-gated backlog (verbatim from the spike ADR) becomes tracked work: **(a)** field leg — phone on cellular, relay path; **(b)** Flutter/tauri SDK wiring (FRB bridge with the feature enabled); **(c)** native `run_session` refactor to kill the loopback-bridge ponytail; **(d)** self-hosted relay guidance + the n0-fleet privacy note in operator docs.

## 3. What rejecting means

- Branch parked or deleted; main is already ws-only — zero cleanup needed, nothing of the spike is on main.
- Cost: arxa keeps wrapping nostos's WS inside an iroh tunnel (QUIC ∘ TCP/TLS ∘ HTTP ∘ WS) and keeps owning the glue plus the TLS/DNS/port-forwarding pain for any naked-LAN or remote deployment. The QR-native `iroh://<NodeId>` addressing story and the per-transport conformance seam are discarded.

## 4. Recommended conditions of acceptance

1. **No consumer defaults to `iroh://` until the field leg passes** — the ADR's "test that matters" has a second half (phone on cellular, relay path) that has never run.
2. **Ponytail resolution scheduled:** either the native `run_session` frame-io refactor lands, or the bridge is explicitly re-accepted with its ceiling restated (one loopback TCP hop per connection).
3. **Docs before exposure:** self-hosted relay guidance + the n0-fleet privacy note land with or before the SDK wiring.
4. **Pin policy:** the exact iroh version pin stays; upgrades are budgeted spikes. Churn is not hypothetical — the proposal cited 0.91.2 and the spike shipped 1.1.0 with a renamed addressing API: one breaking rename observed inside a single spike.
5. iroh remains off-default in every shipped artifact until conditions 1–3 clear.

## 5. Reversal trigger (carried verbatim from the ADR)

> If iroh API churn or field holepunch/relay reliability exceeds maintenance budget, drop the `iroh` transport and keep the seam — ws-only, no protocol change, no consumer lock-in beyond a URL prefix.

## 6. Unknowns the spike did not retire — updated 2026-08-29

- ~~iroh/quinn build viability on iOS/Android~~ — **verified 2026-08-29**: `cargo check -p nostos-client --features iroh` clean for `aarch64-apple-ios` (29.0 s) and `aarch64-linux-android` (19.1 s). *Runtime* behavior on real phones (holepunch/relay, OS backgrounding, network switching) remains unproven — that is the field leg.
- Real-world holepunch/relay reliability on cellular — still genuinely unknown; requires a physical phone off-LAN. Not closable from this machine.
- ~~CI compile-time cost of the quinn tree~~ — **measured 2026-08-29**: cold feature-on build (fresh target dir, warm registry cache, Apple Silicon) = **45.5 s** for the conformance test profile; incremental rebuilds afterwards are seconds. Off-default, so only feature-on jobs pay it.

## 7. Exact steps

**Accept:**
1. `git merge spike/iroh-transport` — the one conflict hunk is `reset_subscribed` in `client.rs`; resolve to main's `std::mem::take` form (rehearsed 2026-08-29, merged tree green).
2. Edit ADR-0041: status → `Accepted` (dated), References 0.91.2 → 1.1.0, record the §4 conditions in the ADR or the integration plan.
3. `make ci`, plus `cargo test -p nostos-client --features iroh --test iroh_ws_conformance` on the merge result.
4. File the four gated items into `docs/plans/nostos-integration-tauri-flutter-push.md`.

**Reject:** status → `Rejected` with a one-paragraph rationale; delete or park the branch. No code changes needed.
