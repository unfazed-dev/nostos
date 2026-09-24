//! Cold-stranger auth-risk probe — verify a REAL Supabase asymmetric JWT
//! (RS256/ES256/EdDSA — this project is **ES256**) against the project's live
//! `/.well-known/jwks.json`.
//!
//! This is the one code path the cold-stranger test depends on that has NEVER
//! been exercised against real Supabase: `jwks.rs::JwksVerifier` is unit-tested
//! only via `FixtureJwks` (a local axum server with minted keys). A stranger
//! following QUICKSTART against a real Supabase project hits exactly
//! `SupabaseJwtAuth::from_config(None, Some(jwks_url))` → RS256 → live JWKS fetch
//! → `jsonwebtoken::decode` first. An RS256/kid/edge-cache bug here blows the
//! stranger's first sync on launch day.
//!
//! `#[ignore]` + env-gated: it needs operator Supabase creds (a real project +
//! a real user access token), which CI does not have. Self-skips (loud) without
//! them. When it DOES run and returns `Some(principal)`, that is a structural
//! proof the JWKS/RS256 path verified the token: the adapter is built with
//! `hs256_secret = None`, so the HS256 path is unreachable — only the JWKS path
//! can produce a principal.
//!
//! ## Running
//! ```sh
//! NOSTOS_SUPABASE_URL=https://<ref>.supabase.co \
//! NOSTOS_SUPABASE_JWT=<a real user access_token from supabase.auth.signIn> \
//!   cargo test -p nostos-infra --test jwks_real_supabase -- --ignored --nocapture
//! ```

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey};
use nostos_application::ports::SyncAuth;
use nostos_infra::SupabaseJwtAuth;

/// Verify a real Supabase asymmetric access token end-to-end through the live
/// JWKS (RS256/ES256/EdDSA — this project is ES256). Needs a real user token.
#[tokio::test]
#[ignore = "needs NOSTOS_SUPABASE_URL + NOSTOS_SUPABASE_JWT (operator: real Supabase project + user token)"]
async fn real_supabase_asymmetric_token_verifies_via_live_jwks() {
    let base = match nostos_infra::env::var("NOSTOS_SUPABASE_URL") {
        Ok(u) if !u.trim().is_empty() => u.trim().trim_end_matches('/').to_string(),
        _ => {
            eprintln!(
                "skipping: set NOSTOS_SUPABASE_URL=https://<ref>.supabase.co and \
                 NOSTOS_SUPABASE_JWT=<user access_token> to run the cold-stranger JWKS probe"
            );
            return;
        }
    };
    let token = match nostos_infra::env::var("NOSTOS_SUPABASE_JWT") {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            eprintln!("skipping: NOSTOS_SUPABASE_JWT not set (need a real user access_token)");
            return;
        }
    };

    // The documented Supabase asymmetric-JWKS path (ADR-0010 addendum):
    // `<project>/auth/v1/.well-known/jwks.json`, edge-cached ~10 min.
    let jwks_url = format!("{base}/auth/v1/.well-known/jwks.json");
    eprintln!("[jwks-probe] fetching live JWKS at {jwks_url}");

    // Asymmetric-only: no HS256 secret ⇒ the HS256 path is structurally
    // unreachable. A `Some` result can ONLY come from the RS256/ES256/EdDSA JWKS
    // path. This is what makes a pass load-bearing for the cold-stranger test.
    let auth = SupabaseJwtAuth::from_config(None, Some(jwks_url.clone()));

    let Some(principal) = auth.authenticate(&token).await else {
        panic!(
            "JWKS probe FAILED: the real Supabase token did NOT verify against {jwks_url}. \
             This is the cold-stranger launch-day failure — investigate asymmetric/kid/edge-cache."
        )
    };

    assert!(
        !principal.account_id.is_empty(),
        "verified principal has an empty account_id (sub not lifted)"
    );
    assert_eq!(
        principal.tenant_id, principal.account_id,
        "Phase-0 invariant: tenant = account (sub). Divergence means the JWKS path lifted the \
         wrong claim."
    );

    eprintln!(
        "[jwks-probe] PASS: real Supabase asymmetric (ES256) token verified via live JWKS — \
         account_id={} tenant_id={}",
        principal.account_id, principal.tenant_id
    );
}

/// Fetch + parse Supabase's LIVE `/.well-known/jwks.json` through nostos's exact
/// algorithm-inference + `DecodingKey::from_jwk` logic. Needs only the project
/// URL (the JWKS is public) — NO user token — so this half of the never-exercised
/// path runs against real Supabase today. It catches the realistic launch-day
/// failure: nostos's parser choking on Supabase's actual JWKS shape (curve, kid,
/// key_ops). The signature-VERIFY half (`real_supabase_rs256_token_verifies…`)
/// still needs a real user token.
#[tokio::test]
#[ignore = "needs NOSTOS_SUPABASE_URL only (no token) — verifies the JWKS fetch+parse half"]
async fn live_supabase_jwks_fetches_and_parses() {
    let base = match nostos_infra::env::var("NOSTOS_SUPABASE_URL") {
        Ok(u) if !u.trim().is_empty() => u.trim().trim_end_matches('/').to_string(),
        _ => {
            eprintln!("skipping: set NOSTOS_SUPABASE_URL=https://<ref>.supabase.co to run");
            return;
        }
    };
    let jwks_url = format!("{base}/auth/v1/.well-known/jwks.json");
    eprintln!("[jwks-probe] fetching live JWKS at {jwks_url}");

    let resp = reqwest::get(&jwks_url)
        .await
        .unwrap_or_else(|e| panic!("JWKS fetch failed: {e}"));
    assert!(
        resp.status().is_success(),
        "JWKS endpoint returned {}: {}",
        resp.status(),
        jwks_url
    );
    let set: JwkSet = resp
        .json()
        .await
        .unwrap_or_else(|e| panic!("JWKS did not parse as a JwkSet: {e}"));

    // Mirror jwks.rs::infer_algorithm + the DecodingKey::from_jwk call the
    // verifier makes when caching each key. Anything nostos doesn't infer a
    // supported algorithm for (or that from_jwk rejects) is reported + skipped,
    // exactly as production does.
    let mut supported = 0usize;
    for jwk in &set.keys {
        let Some(kid) = &jwk.common.key_id else {
            eprintln!("[jwks-probe] skip a key with no kid");
            continue;
        };
        let alg = match &jwk.algorithm {
            AlgorithmParameters::RSA(_) => Algorithm::RS256,
            AlgorithmParameters::EllipticCurve(ec) if ec.curve == EllipticCurve::P256 => {
                Algorithm::ES256
            }
            AlgorithmParameters::OctetKeyPair(okp) if okp.curve == EllipticCurve::Ed25519 => {
                Algorithm::EdDSA
            }
            other => {
                eprintln!("[jwks-probe] skip kid={kid}: unsupported key shape {other:?}");
                continue;
            }
        };
        DecodingKey::from_jwk(jwk)
            .unwrap_or_else(|e| panic!("DecodingKey::from_jwk failed for kid={kid}: {e}"));
        supported += 1;
        eprintln!("[jwks-probe] parsed kid={kid} alg={alg:?}");
    }
    assert!(
        supported >= 1,
        "no supported keys parsed from the live JWKS — nostos would reject every Supabase token"
    );
    eprintln!(
        "[jwks-probe] fetch+parse PASS: {supported} supported key(s) from {jwks_url} (verify-signature half still needs a user token)"
    );
}
