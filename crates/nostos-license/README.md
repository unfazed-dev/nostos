# nostos-license — offline license claims

HMAC-signed entitlement tokens: `LicenseClaims::sign` (minted by
`nostos-cloud`) and `LicenseClaims::verify` / `resolve_entitlement` (checked
by `nostos-server`). Keeps the crypto dependencies out of `nostos-domain`.

Licenses gate Nostos Cloud and Enterprise features only — never the
self-hosted Apache-2.0 binary. Depends on `nostos-domain` (for `Tier`).

```sh
cargo test -p nostos-license
```
