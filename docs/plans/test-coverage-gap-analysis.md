# Test Coverage Gap Analysis — 2026-07-13

Static analysis of test files vs source surface across the workspace. **No
coverage tooling is installed** (`cargo-llvm-cov` / `cargo-tarpaulin` /
`llvm-tools-preview` all absent) — so this is a *gap* analysis from surface
inspection, not a line-coverage report. First recommendation below is to close
that hole; until then every percentage here is inferred, not measured.

## Methodology

- Inventoried `#[test]` density per source file.
- Cross-referenced each crate's `pub` surface against its inline
  `#[cfg(test)]` modules and `tests/` integration suites.
- Prioritised **trust boundaries** (auth, JWKS, cloud store, server routes)
  and **pure-domain quick wins** (tier, error parsing) over trivial accessors.

## Headline gaps (priority order)

| # | File / area | Test count | Why it matters | Risk |
|---|-------------|-----------|----------------|------|
| 1 | `nostos-infra/src/jwks.rs` | **0** | JWKS verify + cache + refetch rate-limit (DoS guard). Trust boundary. | **High** — untested fail-closed paths |
| 2 | `nostos-cloud/src/store.rs` | **0** (13 pub async fns) | Control-plane persistence: accounts, projects, api keys, subscriptions | **High** — money + auth data |
| 3 | `nostos-server` (whole crate) | **0** (`NO_SERVER_TESTS`) | `/healthz`, `/sync` route wiring, auth rejection at HTTP layer | Medium |
| 4 | `nostos-domain/src/tier.rs` | **0** | Pure domain, trivially testable, shared by cloud + server | Low risk / **high ROI** |
| 5 | `nostos-cloud/src/routes.rs` | **0** | `router()`, `ApiError` mapping, `checkout_ok` redirect | Medium |
| 6 | `nostos-infra/src/auth.rs` | 2 | Only happy-path HS/RS + alg-confusion. Missing: expired, malformed, no-kid, `from_config` matrix, `AllowAnonymous` | Medium |
| 7 | Coverage tooling | — | "Measure before optimize" is a project rule; no tool installed to measure | Process |
| 8 | `nostos-infra/src/transport.rs` builders | 0 on builders | `with_buffer`/`with_tenant_column`/`with_write_back`/`with_write_tables` defaults untested | Low |

## Edge-case / error-handling gaps (not whole files, specific branches)

- **JWKS**: unknown `kid` → refetch → second miss fails closed; `MIN_REFETCH_INTERVAL` rate-limit under a flood of bogus kids; JWKS endpoint unreachable → fail closed; alg mismatch reject; empty `sub` reject; expired token reject (jwks path checks `exp`, HS256 path does not — undocumented asymmetry).
- **auth.rs**: token with <3 segments; `from_config(None, None)` (anonymous-only); `from_config(Some, Some)` precedence; `exp` in the past.
- **store.rs `Role::parse`**: unknown string → `None`; round-trip case sensitivity.
- **store.rs `project_for_api_key`**: unknown key → `None` (not an error).
- **`LsnParseError` / `ParseError`**: confirm every variant has a构造-and-match test (lsn.rs has 5 tests, predicate_compile 15 — likely covered, verify the empty/overflow variants).

## Integration-test gaps

- **No `nostos-server` integration suite.** `/healthz` and the `/sync` auth-rejection path (no token, bad token) have no end-to-end test — only the WS contract is exercised through `nostos-infra/tests/ws_contract.rs`.
- **No `nostos-cloud` store integration test** despite a ready-made `CloudStore::in_memory()` — the entire SQLite mapping is unverified.
- **No cloud `routes.rs` test** — checkout redirect + `ApiError`→status mapping untested.

---

## Test skeletons

Each is paste-ready into the named file. Kept minimal (ponytail): one assertion
per behaviour, no framework ceremony.

### 1. `nostos-domain/src/tier.rs` — add inline module

Pure domain, zero deps, highest ROI. Do this first.

```rust
#[cfg(test)]
mod tests {
    use super::Tier;

    #[test]
    fn device_cap_orders_paid_above_hobby() {
        assert!(Tier::Hobby.device_cap() < Tier::Pro.device_cap());
        assert!(Tier::Pro.device_cap() <= Tier::Team.device_cap());
    }

    #[test]
    fn serde_round_trips() {
        for t in [Tier::Hobby, Tier::Pro, Tier::Team /* , … all variants */] {
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(serde_json::from_str::<Tier>(&json).unwrap(), t);
        }
    }

    #[test]
    fn default_is_hobby() {
        assert_eq!(Tier::default(), Tier::Hobby);
    }
}
```

### 2. `nostos-cloud/src/store.rs` — uses existing `in_memory()`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn account_project_apikey_subscription_round_trip() {
        let store = CloudStore::in_memory().unwrap();
        let acc = store.create_account("a@b.co", "hash").await.unwrap();
        let proj = store.create_project(&acc.id, "demo").await.unwrap();
        let key = store.create_api_key(&proj.id).await.unwrap();

        // unknown key -> None, not error
        assert!(store.project_for_api_key("bogus").await.unwrap().is_none());
        // known key resolves to the project
        let got = store.project_for_api_key(&key.key).await.unwrap().unwrap();
        assert_eq!(got.id, proj.id);
    }

    #[test]
    fn role_parse_rejects_unknown() {
        assert_eq!(Role::parse("owner"), Some(Role::Owner));   // adjust variant names
        assert_eq!(Role::parse("wizard"), None);
    }

    #[tokio::test]
    async fn waitlist_count_increments() {
        let store = CloudStore::in_memory().unwrap();
        assert_eq!(store.waitlist_count().await.unwrap(), 0);
        store.add_waitlist("x@y.co").await.unwrap();
        assert_eq!(store.waitlist_count().await.unwrap(), 1);
    }
}
```

### 3. `nostos-server` — new `tests/healthz.rs`

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

// Adjust to whatever builder the binary exposes; if none is pub, extract
// `build_app()` from main.rs and call it here + in main().
async fn app() -> axum::Router {
    nostos_server::build_app(/* cfg or default */)
}

#[tokio::test]
async fn healthz_returns_200() {
    let resp = app()
        .await
        .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn sync_without_token_is_rejected() {
    // WS upgrade without auth — expect 401/403, not a successful upgrade.
    let resp = app()
        .await
        .oneshot(
            Request::builder()
                .uri("/sync")
                .header("upgrade", "websocket")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(resp.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN));
}
```

> Requires exposing a `pub fn build_app(...)` (or similar) from `main.rs`.
> Today the router is built inline in `main()`; this refactor is the
> prerequisite for any server route test.

### 4. `nostos-infra/src/auth.rs` — extend the existing test module

```rust
#[tokio::test]
async fn malformed_token_with_wrong_segment_count_is_rejected() {
    let auth = SupabaseJwtAuth::new(b"secret".to_vec());
    assert!(auth.authenticate("not.a.jwt.token").await.is_none()); // 4 segments
    assert!(auth.authenticate("").await.is_none());
}

#[tokio::test]
async fn from_config_with_neither_secret_nor_url_is_anonymous_only() {
    let auth = SupabaseJwtAuth::from_config(None, None);
    // Any token must be rejected — anonymous path is AllowAnonymous's job,
    // not this authenticator. Pin the behaviour either way:
    let tok = mint_token_for("someone"); // reuse existing helper if generic
    assert!(auth.authenticate(&tok).await.is_none());
}

#[tokio::test]
async fn expired_rs256_token_is_rejected() {
    // RS path checks exp (HS path does not — see ADR-0010 addendum).
    // Mint a token whose `exp` is in the past and assert authenticate -> None.
    // TODO: add an `exp` parameter to the local mint_token helper.
}
```

### 5. `nostos-infra/src/jwks.rs` — needs a one-line production change first

`JwksVerifier` builds its own `reqwest::Client` and is `pub(crate)`, so the
fetch/cache/rate-limit paths can't be exercised without an injectable client
or a mock HTTP server. **Prerequisite:** accept an injected client:

```rust
pub(crate) fn new(jwks_url: String, ttl: Duration) -> Self {
    Self::with_client(jwks_url, ttl, reqwest::Client::new())
}
pub(crate) fn with_client(jwks_url: String, ttl: Duration, http: reqwest::Client) -> Self {
    Self { jwks_url, http, ttl, cache: RwLock::new(Cache::default()) }
}
```

Then an inline module driving `wiremock`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{matchers::*, Mock, MockServer, ResponseTemplate};

    async fn server_with_key(server: &MockServer, kid: &str) -> JwkSet {
        // reuse rsa_key_and_jwk from auth.rs tests, or duplicate minimally
        todo!("build a JwkSet whose keys[].kid == kid and the matching JWK")
    }

    #[tokio::test]
    async fn unknown_kid_after_refetch_fails_closed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::200().set_body_json(/* jwks with kid "k1" */))
            .up_to_n_times(1)             // only one fetch allowed
            .mount(&server).await;
        let v = JwksVerifier::with_client(server.uri(), Duration::from_secs(60), reqwest::Client::new());
        // bogus kid -> one refetch -> still missing -> None
        let hdr = Header { kid: Some("nope".into()), alg: Algorithm::RS256, ..Default::default() };
        assert!(v.verify("ignored", &hdr).await.is_none());
    }

    #[tokio::test]
    async fn unreachable_jwks_fails_closed() {
        let v = JwksVerifier::with_client(
            "http://127.0.0.1:1/jwks.json".into(),       // nothing listening
            Duration::from_secs(60),
            reqwest::Client::new(),
        );
        let hdr = Header { kid: Some("k1".into()), alg: Algorithm::RS256, ..Default::default() };
        assert!(v.verify("ignored", &hdr).await.is_none());
    }

    #[tokio::test]
    async fn bogus_kid_flood_does_not_hammer_endpoint() {
        // After MIN_REFETCH_INTERVAL kicks in, repeated distinct bogus kids
        // must NOT each trigger a fetch. Mount the JWKS with up_to_n_times(1)
        // and assert only one request lands despite N distinct kids.
    }
}
```

### 6. Coverage tooling — add a Makefile target

```make
coverage:
	cargo llvm-cov --workspace --html                  # requires: cargo install cargo-llvm-cov
```

Document the install in `CLAUDE.md` verbs section so the next agent inherits it.

---

## Deliberately skipped (YAGNI)

- **Trivial accessors / builders** (`transport.rs` `with_*` methods) — they're
  field assignment; a test would test Rust, not logic. Add if one grows a guard.
- **`nostos-bench` report/stats** — 3+1 tests exist; bench output shape is
  low-stakes and already exercised by every `make bench` run.
- **Exhaustive per-variant skeletons for `LsnParseError`/`ParseError`** — those
  files already have 5 and 15 tests; verify the empty/overflow variants exist
  rather than blanket-adding.
- **FFI (`nostos-ffi-wasm`, 38 tests) and `predicate.rs` (24 tests)** — already
  the densest-tested files in the tree; no gap to fill.
