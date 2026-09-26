# ADR-0052 — Appwrite server transport over the Function journal

Date: 2026-09-27
Status: accepted for the Atlet reference

## Context

ADR-0050's direct transport uses a signed-in Appwrite JWT to execute the
hosted `atlet_sync` Function. That Function validates the account, enforces
Atlet ACL, and atomically commits data, mutation IDs, and journal sequence.
Flutter native and web already use the same Rust local engine and durable
outbox. The reference also needs a server route that reaches the **same**
TablesDB journal without creating a second authorization implementation.

## Decision

`nostos-server` gains an explicit Appwrite gateway backend. It starts only an
HTTP surface: fixed `POST /appwrite/sync/pull` and `/push` routes plus local
`GET /healthz`. It does not start a Postgres replication slot, fake event
producer, WebSocket sync route, or write-back handler in this backend. The
client submits its protocol JSON and Appwrite JWT as a bearer token. The server
wraps the request for synchronous execution of its configured Function and
returns only `responseStatusCode` and `responseBody`. The Function remains the
single writer and authorization authority. Server mode is a distinct Nostos
transport topology; it is not an independent copy of the cloud sync policy.

The gateway holds no Appwrite API key. The upstream endpoint, project, and
Function ID are fixed at process startup. It disables HTTP redirects, accepts
only the two sync paths, bounds request bodies, rejects missing bearers, and
never logs tokens or protocol bodies. Production upstream and gateway URLs use
HTTPS. The existing CORS allowlist handles Flutter web. An authenticated pull
is the end-to-end readiness probe; `/healthz` reports only local readiness.

Native and browser Appwrite clients use the same cursor and outbox in both
modes. Their local principal and storage location include transport mode, so
a direct/server switch cannot reuse private rows or a cursor accidentally.
Appwrite Auth itself still talks directly to Appwrite Cloud; the gateway only
mediates Nostos sync requests. On gateway failure, local reads and queued
writes remain available and replay after reconnect.

## Consequences and proof

Both modes converge through the same Functions API and TablesDB journal. This
keeps policy and schema migration versioning in one place, while server mode
adds one network hop and remains dependent on the Function. The hosted smoke
gate must demonstrate admin and two customers, offline replay, account
revocation, and direct-to-server cross-mode visibility. Local forwarding tests
verify fixed paths, bearer forwarding, and removal of execution request
headers from responses. The gateway is shipped as an optional backend of the
existing `nostos-server` binary, with a separate cloud deployment target.
