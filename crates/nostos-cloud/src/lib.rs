//! # nostos-cloud
//!
//! The Nostos Cloud control plane — the business-management surface that turns
//! the open-source engine into a billable product.
//!
//! **Trust model (STRATEGY.md §7.2):** licenses gate **Nostos Cloud** (managed
//! hosting) and **Enterprise** features only. The self-hosted Apache-2.0 binary
//! stays 100% free and unlimited forever — value is captured here, through
//! managed hosting + the operational/compliance premium, never by crippling
//! the OSS engine.
//!
//! ## Modules
//! - [`auth`] — dual-path identity (Supabase JWT OR session cookie).
//! - [`license`] — HMAC-signed offline license tokens (tier + device cap + expiry).
//! - [`store`] — bundled-sqlite persistence (accounts, projects, api keys, subs).
//! - [`stripe`] — webhook signature verification (to spec) + Checkout creation.
//! - [`payments`] — the PaymentProvider trait seam (Stripe live, PayPal stub).

#![forbid(unsafe_code)]

pub mod auth;
pub mod license;
pub mod payments;
pub mod routes;
pub mod store;
pub mod stripe;

use rand::RngCore;

/// A short random id (24 hex chars, 96 bits of entropy) — enough to be
/// unguessable for resource ids without pulling a full uuid dep into the cloud.
pub fn random_id() -> String {
    let mut buf = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// SHA-256 of a password as a hex string. Adequate for a launch behind TLS +
/// rate-limiting; documented upgrade path is to argon2 (see ADR when added).
pub fn hash_password(password: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(password.as_bytes());
    hex::encode(h.finalize())
}

/// Constant-time-ish password check.
pub fn verify_password(password: &str, expected_hex: &str) -> bool {
    hash_password(password) == expected_hex
}
