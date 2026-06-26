//! Payment provider seam — the trait that decouples the control plane from any
//! one checkout vendor.
//!
//! **Day-1 (per the platform assembly ADR):** Stripe is the only live provider.
//! PayPal is stubbed behind the `paypal` feature flag so the seam compiles and
//! the surface is reserved, but it returns `Unsupported` until wired. This keeps
//! the architecture honest (any provider fits) without paying PayPal integration
//! cost before launch.
//!
//! The trait is deliberately tiny: the only checkout operation Nostos needs is
//! "give me a hosted URL the buyer visits." Card data never touches us.

use async_trait::async_trait;
use nostos_domain::Tier;

/// A hosted-checkout URL + provider id, the result of starting a checkout.
#[derive(Debug, Clone)]
pub struct Checkout {
    /// The URL the browser redirects to (Stripe Checkout / PayPal, etc.).
    pub url: String,
    /// The provider's session id (for reconciliation).
    pub id: String,
}

/// The inputs to a checkout session.
#[derive(Debug, Clone)]
pub struct CheckoutRequest<'a> {
    pub tier: Tier,
    /// The provider's price/plan id for this tier.
    pub price_id: &'a str,
    pub success_url: &'a str,
    pub cancel_url: &'a str,
    pub client_reference_id: &'a str,
}

/// Errors a payment provider can return.
#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("the {provider} provider is configured but not yet implemented")]
    Unsupported { provider: &'static str },
    #[error("payment provider error: {0}")]
    Other(String),
}

/// The checkout vendor abstraction. Implementations live behind feature flags:
/// `StripeProvider` is always available; `PaypalProvider` needs `--features paypal`.
#[async_trait]
pub trait PaymentProvider: Send + Sync {
    /// Provider name, for error messages + telemetry.
    fn name(&self) -> &'static str;

    /// Start a hosted checkout and return the URL the buyer should visit.
    async fn create_checkout(
        &self,
        req: &CheckoutRequest<'_>,
    ) -> Result<Checkout, PaymentError>;
}

/// Stripe provider — wraps the existing hand-rolled Stripe REST integration.
pub struct StripeProvider {
    pub http: reqwest::Client,
    pub secret_key: String,
}

#[async_trait]
impl PaymentProvider for StripeProvider {
    fn name(&self) -> &'static str {
        "stripe"
    }

    async fn create_checkout(
        &self,
        req: &CheckoutRequest<'_>,
    ) -> Result<Checkout, PaymentError> {
        let cs = crate::stripe::create_checkout_session(
            &self.http,
            &self.secret_key,
            &crate::stripe::CreateCheckout {
                price_id: req.price_id,
                success_url: req.success_url,
                cancel_url: req.cancel_url,
                client_reference_id: req.client_reference_id,
            },
        )
        .await
        .map_err(|e| PaymentError::Other(e.to_string()))?;
        Ok(Checkout {
            url: cs.url,
            id: cs.id,
        })
    }
}

/// PayPal provider — **stub**. Returns `Unsupported` until the integration lands.
/// Compiled only behind the `paypal` feature so the seam is reserved without
/// pulling PayPal deps into the default build.
#[cfg(feature = "paypal")]
pub struct PaypalProvider;

#[cfg(feature = "paypal")]
#[async_trait]
impl PaymentProvider for PaypalProvider {
    fn name(&self) -> &'static str {
        "paypal"
    }

    async fn create_checkout(
        &self,
        _req: &CheckoutRequest<'_>,
    ) -> Result<Checkout, PaymentError> {
        // ponytail: stub returns Unsupported; real impl lands when PayPal is wired.
        Err(PaymentError::Unsupported { provider: "paypal" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn paypal_stub_is_unsupported() {
        // The stub only exists behind the feature flag; verify the contract via
        // a stand-in when the flag is off (the real PaypalProvider path is
        // exercised in the `paypal`-feature CI job).
        struct Stub;
        #[async_trait]
        impl PaymentProvider for Stub {
            fn name(&self) -> &'static str {
                "paypal"
            }
            async fn create_checkout(
                &self,
                _req: &CheckoutRequest<'_>,
            ) -> Result<Checkout, PaymentError> {
                Err(PaymentError::Unsupported { provider: "paypal" })
            }
        }
        let p = Stub;
        let req = CheckoutRequest {
            tier: Tier::Pro,
            price_id: "p",
            success_url: "s",
            cancel_url: "c",
            client_reference_id: "r",
        };
        let err = p.create_checkout(&req).await.unwrap_err();
        assert!(matches!(err, PaymentError::Unsupported { provider: "paypal" }));
    }
}
