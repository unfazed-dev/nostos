# Atlet Appwrite server mode — design

## Contract

The same Flutter Atlet UI must run in `appwrite/direct` (the default) and
`appwrite/server` against the ADS project's existing `atlet` TablesDB journal.
The user, catalog, orders, mutation IDs, and authorization rules are identical
in both modes. A device has a separate provider-and-mode SQLite/OPFS store, so
changing mode closes the old engine and cannot reuse a cursor or private cache.
The app continues to read and queue writes through Nostos's Rust local engine.

## Server boundary

Run `nostos-server` in an Appwrite gateway configuration with a fixed project,
Function ID, and Appwrite API endpoint. It exposes only `POST /appwrite/sync/pull`
and `POST /appwrite/sync/push`, plus `GET /healthz`. Each request carries the
signed-in user's Appwrite JWT as a bearer token and a JSON protocol body. The
server wraps that body in the Appwrite synchronous execution request, forwards
the JWT to the existing `atlet_sync` Function, and returns its execution
envelope. The Function remains the sole authority for user status, table ACL,
journal sequence allocation, idempotency, and TablesDB transactions. Neither
the server nor a client receives an Appwrite API key.

This is deliberately a server **transport adapter** over the same journal,
not an independent copy of the Function's domain policy. It adds an always-on
Nostos hop while retaining one authorization and mutation implementation. The
gateway has a fixed upstream URL and two fixed routes; it cannot be used as a
general HTTP proxy. It rejects missing bearer tokens, oversized or malformed
bodies, and non-POST sync requests before forwarding. It never logs the JWT or
request body. It preserves Function status and response bytes so the existing
Rust cursor, outbox, and revoked-account behavior work in either mode.

The gateway configuration is separate from the Postgres/fake/mirror replicator
path. In gateway mode the server does not start a Postgres slot or fake driver.
`GET /healthz` reports local readiness; an authenticated pull is the end-to-end
readiness probe because the gateway holds no service credential. CORS allows
the explicitly configured Atlet web origin. HTTPS terminates at
the cloud host; plain HTTP is permitted only inside that host's private
network.

## Client selection

The native and WASM Appwrite transports share one call contract: direct sends
the execution request to Appwrite's API; server sends the JSON protocol body
to the matching gateway route. The server URL is mandatory in server mode. The local principal and
storage key include provider, project, user, and mode. The UI displays the
selected mode and server endpoint without exposing the JWT. Appwrite Auth
still talks to Appwrite Cloud in both modes; a gateway outage leaves the
local read model and outbox available for offline work.

## Cloud proof

Use one hosted `nostos-server` instance near Appwrite's `fra` region. The
repository already has a Fly deployment template; the operator may select
another always-on Rust host. The Rust launcher performs the existing
admin/customer A/customer B Flutter scenarios in both modes against the same
cloud project, and records unique fixture IDs and convergence in ignored JSON.
The acceptance flow queues a customer write offline in server mode, restarts
the app, resumes through the gateway, sees the write in direct mode, then
fulfils an order in direct mode and observes it in server mode. It also proves
private-row isolation, disabled-account cache wipe, bearer rejection, and
gateway recovery without duplicate mutation. CI invokes the same launcher
through `scripts/check.sh` and treats missing host configuration as failure.

## Evidence and limits

Appwrite documents synchronous Function executions and passing a user's JWT to
the Function (`https://appwrite.io/docs/products/functions/execute`). The ADS
project currently has one active `atlet_sync` Function with `execute=[users]`.
This phase measures topology and behavior, not a performance advantage. A
gateway does not make the system independent of the hosted Function; that
would require moving the policy/transaction code into a shared Appwrite
adapter and separately proving both deployments.
