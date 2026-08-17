//! # nostos-push — the nostos-pushd standalone push daemon (ADR-0038)
//!
//! Composition-root workspace member (lib + the `nostos-pushd` bin), Wave 1
//! of docs/plans/nostos-push-daemon-implementation.md:
//!
//! | module | plan task |
//! |---|---|
//! | [`config`] | 1.1 — clap + `NOSTOS_PUSHD_*` env (server pattern) |
//! | [`auth`] | 1.3 — API-key middleware, constant-time compare, tenant stamping |
//! | [`store`] | 1.2 — token registry + receipt log (SQLite, pin 0.3 schema; PgStore behind `pg`, v1.1) |
//! | [`api`] | 1.4 — token routes; 1.5 — send route (contract-exact) |
//! | [`coalescer`] | 1.6 — per-(tenant, token) debounce, receipts, prune |
//! | [`rail`] | 1.7 — the rails' env contract via from_env(); 1.5 dispatch seam |
//! | [`limit`] | 4.1 — per-tenant send token bucket (2026-08-17 audit) |
//!
//! Scope law (ADR-0038 §2): token-addressed sends, daemon-owned registry,
//! debounce coalescing — NOT a marketing platform (no topics, scheduling,
//! segments, A/B). Push is a wake-up trigger, not a data channel; delivery
//! is best-effort and outcomes are reported via receipts, never promised.
//! The wire format stays human-debuggable JSON.

pub mod api;
pub mod auth;
pub mod coalescer;
pub mod config;
pub mod limit;
pub mod rail;
pub mod store;

pub use api::{build_router, AppState};
pub use auth::ApiKeys;
pub use coalescer::Coalescer;
pub use rail::Rails;
pub use store::{Platform, SqliteStore, Store};
// The v1.1 Postgres registry (ADR-0038 §4 addendum) — only under `pg`.
#[cfg(feature = "pg")]
pub use store::PgStore;
