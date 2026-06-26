//! Stripe integration — webhook signature verification (hand-rolled to the
//! official spec) + Checkout session creation via the REST API.
//!
//! **Why no SDK:** the Stripe Rust SDK is heavy and overkill for the two calls
//! we need (create Checkout Session, parse the webhook). Webhook verification
//! is the security-critical path and is best implemented directly against the
//! spec (verified 2026-06-26): `{timestamp}.{raw_body}` HMAC-SHA256, hex sig,
//! 5-minute tolerance, constant-time compare.
//!
//! **Card data:** we NEVER touch it. Stripe Checkout hosts the payment form on
//! Stripe's domain; we only ever receive the webhook + redirect. This is the
//! lowest-billing-liability path for a launch.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// Default replay-attack tolerance (Stripe's recommended 5 minutes).
pub const DEFAULT_TOLERANCE_SECS: u64 = 300;

/// Verify a Stripe webhook signature against the raw request body.
///
/// Implements the official spec: parse `Stripe-Signature` as `t=...,v1=...`,
/// compute `HMAC-SHA256("{t}.{raw_body}", secret)`, constant-time compare to
/// `v1`, and reject if the timestamp is outside the tolerance window.
///
/// # Errors
/// [`StripeError::BadSignature`] on any parse/timing/mismatch failure.
pub fn verify_webhook(
    signature_header: &str,
    raw_body: &[u8],
    secret: &str,
    tolerance_secs: u64,
) -> Result<(), StripeError> {
    let mut timestamp: Option<i64> = None;
    let mut v1: Option<String> = None;
    for kv in signature_header.split(',') {
        if let Some(rest) = kv.strip_prefix("t=") {
            timestamp = rest.trim().parse().ok();
        } else if let Some(rest) = kv.strip_prefix("v1=") {
            v1 = Some(rest.trim().to_string());
        }
    }
    let timestamp = timestamp.ok_or(StripeError::BadSignature("missing t".into()))?;
    let v1 = v1.ok_or(StripeError::BadSignature("missing v1".into()))?;

    // Tolerance check (replay protection).
    let now: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed());
    if (timestamp - now).unsigned_abs() > tolerance_secs {
        return Err(StripeError::BadSignature(
            "timestamp outside tolerance".into(),
        ));
    }

    // signed_payload = "{timestamp}.{raw_body}"
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| StripeError::BadSecret)?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(raw_body);
    // `verify_slice` is constant-time.
    let expected = hex::decode(&v1).map_err(|_| StripeError::BadSignature("v1 not hex".into()))?;
    mac.verify_slice(&expected)
        .map_err(|_| StripeError::BadSignature("hmac mismatch".into()))
}

/// The subset of a Stripe Event we care about for provisioning.
#[derive(Debug, Clone, Deserialize)]
pub struct StripeEvent {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: StripeEventData,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StripeEventData {
    pub object: serde_json::Value,
}

/// Pull the subscription id + customer id out of a `checkout.session.completed`
/// or `customer.subscription.*` event payload. Returns None if not present.
#[must_use]
pub fn extract_subscription_ids(object: &serde_json::Value) -> Option<(String, String)> {
    // checkout.session.completed: object has subscription + customer fields.
    let sub_id = object
        .get("subscription")
        .and_then(|v| v.as_str())
        .or_else(|| object.get("id").and_then(|v| v.as_str()))?
        .to_string();
    let cust_id = object.get("customer").and_then(|v| v.as_str())?.to_string();
    Some((sub_id, cust_id))
}

/// Request to create a Stripe Checkout Session.
#[derive(Debug, Clone, Serialize)]
pub struct CreateCheckout<'a> {
    /// The Stripe Price id for the chosen tier (`price_...`).
    pub price_id: &'a str,
    /// Where Stripe redirects on success — carries the project id back.
    pub success_url: &'a str,
    pub cancel_url: &'a str,
    /// Embedded in the session so the webhook can map back to our project.
    pub client_reference_id: &'a str,
}

/// The piece of the Checkout Session creation response we need.
#[derive(Debug, Clone, Deserialize)]
pub struct CheckoutSession {
    pub url: String,
    pub id: String,
}

/// Create a Checkout Session via the Stripe REST API.
///
/// `secret_key` is the `sk_...`/`sk_test_...` key (server-side only, never
/// exposed to the browser). Uses HTTP Basic auth (`sk:` as the password) per
/// Stripe's API convention.
///
/// # Errors
/// Network or Stripe API errors.
pub async fn create_checkout_session(
    http: &reqwest::Client,
    secret_key: &str,
    req: &CreateCheckout<'_>,
) -> Result<CheckoutSession, StripeError> {
    let form = [
        ("mode", "subscription"),
        ("line_items[0][price]", req.price_id),
        ("line_items[0][quantity]", "1"),
        ("success_url", req.success_url),
        ("cancel_url", req.cancel_url),
        ("client_reference_id", req.client_reference_id),
    ];
    let resp = http
        .post("https://api.stripe.com/v1/checkout/sessions")
        .basic_auth(secret_key, Some(""))
        .form(&form)
        .send()
        .await
        .map_err(|e| StripeError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(StripeError::Api(format!("{status}: {body}")));
    }
    resp.json::<CheckoutSession>()
        .await
        .map_err(|e| StripeError::Http(e.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum StripeError {
    #[error("webhook signature invalid: {0}")]
    BadSignature(String),
    #[error("bad secret")]
    BadSecret,
    #[error("http error: {0}")]
    Http(String),
    #[error("stripe api error: {0}")]
    Api(String),
}

#[cfg(test)]
#[allow(clippy::cast_possible_wrap)]
mod tests {
    use super::*;

    fn sign(timestamp: i64, body: &[u8], secret: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        format!(
            "t={timestamp},v1={}",
            hex::encode(mac.finalize().into_bytes())
        )
    }

    #[test]
    fn verifies_a_well_formed_webhook() {
        let secret = "whsec_test";
        let body = br#"{"type":"checkout.session.completed"}"#;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let header = sign(now, body, secret);
        verify_webhook(&header, body, secret, DEFAULT_TOLERANCE_SECS).unwrap();
    }

    #[test]
    fn rejects_tampered_body() {
        let secret = "whsec_test";
        let body = br#"{"type":"x"}"#;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let header = sign(now, body, secret);
        // Flip one body byte.
        let tampered = br#"{"type":"y"}"#;
        assert!(verify_webhook(&header, tampered, secret, DEFAULT_TOLERANCE_SECS).is_err());
    }

    #[test]
    fn rejects_replay_outside_tolerance() {
        let secret = "whsec_test";
        let body = b"x";
        // 1 hour ago — outside the 5-min tolerance.
        let old = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 3600;
        let header = sign(old, body, secret);
        assert!(verify_webhook(&header, body, secret, DEFAULT_TOLERANCE_SECS).is_err());
    }

    #[test]
    fn extracts_sub_and_customer_from_checkout_object() {
        let obj = serde_json::json!({
            "subscription": "sub_abc",
            "customer": "cus_xyz"
        });
        let (sub, cust) = extract_subscription_ids(&obj).unwrap();
        assert_eq!(sub, "sub_abc");
        assert_eq!(cust, "cus_xyz");
    }
}
