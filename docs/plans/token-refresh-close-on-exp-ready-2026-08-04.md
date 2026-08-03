# Ready-to-land: live-socket close-on-JWT-expiry (ADR-0029 §Decision-4 hardening)

**Status:** READY TO LAND — not applied. Gate: `make bench` before/after (the writer `select!` is on the 833k hot path; project rule: perf changes ship with numbers or get reverted). **2026-08-04.**

## The gap (verified)

A live `/sync` WebSocket does **not** outlive its JWT today — it outlives it indefinitely. Server auth runs exactly once at the HTTP→WS upgrade (`sync_handler`, `transport.rs:224-238`); the `exp` check from `04360f6` lives in `verify_supabase_hs256` (`auth.rs:142`), reachable **only** through `authenticate()` at handshake. `run_session` (`transport.rs:260`) moves a **fixed** `Principal` in and never re-checks `exp`. A revoked user keeps syncing until the socket drops for any other reason. Not a v0.1 blocker (OSS default `sync_auth: none`), but a real compliance gap for the managed multi-tenant path. `token_exp`/`exp` is **discarded** after handshake — `Principal` (domain) carries only `sub`.

## The fix (3 changes, ~25 lines)

**1. `crates/nostos-infra/src/auth.rs`** — expose `exp` without putting auth-lifecycle on the domain `Principal`:
- `const JWT_LEEWAY_SECS` → `pub const JWT_LEEWAY_SECS: i64 = 60;` (line 186)
- add `pub fn token_exp(token: &str) -> Option<i64>` reusing the in-module `decode_base64url_to_bytes` + private `SupabaseClaims` (best-effort `exp`; `None` if absent/malformed). Never accepts/rejects — that stays `SyncAuth`'s job.

**2. `crates/nostos-infra/src/transport.rs` `sync_handler`** — compute + thread `exp`:
```rust
let exp = crate::auth::token_exp(&token); // after authenticate() succeeds
ws.on_upgrade(move |socket| run_session(socket, state, principal, exp))
```

**3. `crates/nostos-infra/src/transport.rs` `run_session`** — arm a close-on-exp deadline:
- signature gains `exp: Option<i64>`;
- before the writer `tokio::spawn`, create `let exp_fired = Arc::new(Notify::new());` and (only if `Some`) spawn a deadline task `sleep(remaining).await; exp_fired.notify_one();` where `remaining = u64::try_from((exp_secs + JWT_LEEWAY_SECS).saturating_sub(now_secs).max(0)).unwrap_or(0)`; keep its `JoinHandle` as `exp_task` and `exp_task.abort()` in teardown (no lingering-task leak);
- in the writer `select!` loop add a 3rd branch:
```rust
_ = exp_fired.notified() => {
    debug!("closing socket: token expired (ADR-0029 §Decision-4)");
    let frame = axum::extract::ws::CloseFrame { code: 4401, reason: "nostos: token expired".into() };
    let _ = writer.send(axum::extract::ws::Message::Close(Some(frame))).await;
    break;
}
```
The client's existing reconnect loop treats the drop as "flush outbox, reconnect"; `connect_url()` picks up the refreshed token from `set_token`. `Notify` is safe across loop iterations (a stored permit is consumed by the next `notified()`).

## Tests to add (`crates/nostos-infra/tests/auth_sync.rs`, harness already exists)

Add `mint_jwt_exp(sub, exp_secs)` (extend `mint_jwt`), then:
- **positive:** `live_socket_is_closed_after_token_exp` — real-auth server, token with `exp = now - 55` (within 60s leeway ⇒ handshake accepts; `exp+60 ≈ now+5`), connect+subscribe, assert the server sends Close/None within ~12s.
- **negative:** `live_socket_without_exp_stays_open` — no-`exp` token, assert **no** Close within ~3s (proves the close is exp-driven, not accidental — protects the OSS/anon default).

## Landing gate (mandatory)

1. `make ci` green (incl. the two new tests).
2. **`make bench` before/after** — the writer-loop branch is polled per-iteration on every connection (incl. anon, the benchmark path). If 833k@1k/0% moves materially, switch to the **zero-overhead alternative** (below) instead of the Notify branch.
3. If bench clean: ADR-0029 §Decision-4 → "fully shipped" (handshake `exp` + live-socket close).

## Zero-overhead alternative (if the Notify branch regresses the bench)

Keep the writer `select!` unchanged. Add a `SinkMsg::Shutdown` variant; the deadline task sends it via the sink's `tx`; the writer's **existing** `SinkMsg` match handles it (send Close + break). No new `select!` branch ⇒ zero per-iteration cost on anon/benchmark connections. ~10 more lines, slightly more coupling than the Notify version. Prefer this only if measured impact warrants it.

## Why this was deferred from the 2026-08-04 "fix all" pass

Design + test harness are done and verified-feasible; the only blocker is the project's categorical "perf changes need before/after numbers" rule, which requires a `make bench` run (833k @ 1k clients, minutes) this session couldn't run. Landing it unmeasured risks an unquantified regression to the headline moat figure — exactly what the rule forbids.
