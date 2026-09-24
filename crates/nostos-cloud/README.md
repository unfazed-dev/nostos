# nostos-cloud — the Cloud control plane

A separate axum + rusqlite binary: accounts, projects, API keys, Stripe
billing, and license minting via `nostos-license`. Auth is dual-path —
a Supabase `Authorization: Bearer <jwt>` or the email/password session cookie
the web admin uses. Feature `ui` embeds the admin SPA and landing page.

```sh
NOSTOS_CLOUD_BIND=0.0.0.0:9100 NOSTOS_LICENSE_SECRET=change-me \
  cargo run -p nostos-cloud
cargo test -p nostos-cloud
```

Optional: `NOSTOS_CLOUD_DB`, `NOSTOS_CLOUD_PUBLIC_URL`, `STRIPE_SECRET_KEY`,
`STRIPE_WEBHOOK_SECRET`, `STRIPE_PRICE_PRO`. Depends on `nostos-domain`,
`nostos-infra` (env/config parsing) and `nostos-license`. Platform assembly:
[`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) §7.
