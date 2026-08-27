# ADR-0041: Transport abstraction — `ws` | `iroh` as a first-class server/client option

- **Status:** Proposed (upstream decision pending — the gate per the integration plan is explicit acceptance or rejection with recorded rationale).
- **Date:** 2026-08-27
- **References:** `crates/nostos-infra/src/transport/` (the sync handler seam), `crates/nostos-server/src/main.rs` router assembly (`/sync` mount), `crates/nostos-client/src/client.rs` generic session loop, ADR-0025 (LSN/epoch resume — unchanged by this proposal), ADR-0032 (unified API — unchanged), iroh 0.91.2 docs (docs.rs, fetched 2026-08-27).

## Context

The sync protocol is JSON frames over a WebSocket: `nostos_infra::transport::sync_handler` is mounted at `/sync` (`crates/nostos-server/src/main.rs`, the `route(&cfg.ws_path, get(sync_handler))` line) and every semantic the engine cares about — frame types, LSN checkpoints, ADR-0025 epoch resume, server-authoritative LWW — rides text frames. None of that assumes TCP/TLS/WS specifically; it assumes a **reliable, ordered channel carrying discrete frames**.

Two pieces of code already make a second transport cheap:

- **Client:** the session loop's write path is generic — `flush_outbox`, `drain_stream_commands`, and `ack_and_notify` in `crates/nostos-client/src/client.rs` are all `W: Sink<Message, Error = tungstenite::Error> + Unpin`, fed from `ws.split()` after one dial call (`tokio_tungstenite::connect_async`). Replace the dial + halves, keep the loop.
- **Server:** the session logic lives in `nostos-infra`'s transport module, not in the axum handler — the HTTP upgrade is already a shell around it.

The consumer reality (arxa studio): nostos's WS today rides **inside** an iroh tunnel — desktop-minted QR pairing, phones connected over iroh, nostos wrapped in glue inside that tunnel. That means QUIC (iroh) carrying TCP/TLS carrying HTTP carrying WS, with the consumer repo owning the glue and the operator owning TLS/DNS/port-forwarding pain for any naked-LAN or remote deployment.

What iroh offers (verified against docs.rs **iroh 0.91.2**, MIT OR Apache-2.0 — license-compatible): peer-to-peer **QUIC** connections and streams, direct connectivity via **hole punching complemented by relay servers**, addressed by `NodeId` (a cryptographic key — device-keyed transport encryption is built in; there is no certificate story to own). Connect side: `Endpoint::builder().bind()`, `ep.connect(addr: NodeAddr, alpn)`, `conn.open_bi()`/`open_uni()`. Accept side registers ALPNs on the endpoint builder. No IP, no DNS, no port-forward — a `NodeId` (+ optional relay hint) is a routable address a QR code can carry.

## Decision

### 1. One sync protocol, two transports

`--transport ws|iroh` on nostos-server (env `NOSTOS_TRANSPORT`, default `ws` — the zero-setup path is unchanged) and a first-class client URL scheme: `ws://`/`wss://` dial exactly as today; `iroh://<NodeId>` dials the iroh endpoint. The wire contract, auth (JWT on connect frame), LSN/epoch resume, predicates, write-back: all identical — transport is plumbing.

### 2. Server: an iroh accept loop beside the axum mount, one session per connection

An iroh `Endpoint` registered for ALPN `cairn/sync/1`; each accepted connection drives **the same session core** `sync_handler` drives today, factored one level up in `nostos-infra/src/transport/`. Mapping: one client session = one QUIC connection = one bidirectional stream; each JSON frame = one message. QUIC streams provide the same reliable ordered per-stream delivery WS provided, so the resume and dedup logic is untouched. HTTP surface (`/healthz`, `/schema`, `/rules`, `/metrics`, ADR-0037 push-token REST) **stays on the HTTP listener** — v1 is the sync session only.

### 3. Client: dial-by-scheme, generic loop unchanged in shape

The dial layer selects transport by URL scheme. The iroh adapter produces a Sink/Stream pair of the same frame type over `conn.open_bi()` halves. Today's bounds name `tungstenite::Error` explicitly; generalizing the loop's error parameter to a small `TransportError` is part of this work (mechanical, three sites). No engine, storage, or facade change reaches the SDK tiers.

### 4. Addressing: `iroh://<NodeId>` with hints, QR-native

v1 carries relay/direct-address hints as URL query parameters resolved to a `NodeAddr`; no discovery service in scope. This is the pairing payload arxa mints today, promoted to a first-class address: one line, no DNS, no IP, no cert.

### 5. Scope guard — transport ONLY (explicitly out of scope)

Serverless mesh or P2P sync between clients, gossip, client-relay-client topologies: **rejected**. Server-authoritative LWW, LSN ordering, the CRDT tier (ADR-0032 waves) assume a hub; making every table causally-merged P2P is a different product and a different correctness story. Topology stays hub-and-spoke: iroh replaces TCP/TLS/WS plumbing, nothing else. Also out of scope: serving the HTTP API over iroh, and any change to push rails (ADR-0037/0038 semantics are transport-agnostic — the doorbell wakes the app, the sync session delivers rows).

## Consequences

- **Positive:** consumer tunnel-wrapping glue deleted (studio dials `iroh://` directly at QR-pair — the arxa payoff); device-keyed encryption with zero certificate management; phones on LTE reach a desktop engine with no port-forwarding (holepunch, relay fallback); LAN deployments need no DNS/TLS story; the transport seam makes any future third transport (e.g. stdio/in-process for tests) a dial-layer addition.
- **Negative:** iroh (+ quinn tree) joins the dependency graph — contained behind a cargo feature (`iroh`), off by default so plain `cargo run -p nostos-server` builds exactly as today; two transports must stay conformant (the conformance suite grows a per-transport leg); iroh is 0.x (0.91 today) with real API churn risk — exact pin required, upgrade spikes budgeted; default relay usage routes through n0's fleet (self-hostable, but a privacy/stability consideration to document, not silence).
- **Reversal trigger:** if iroh API churn or field holepunch/relay reliability exceeds maintenance budget, drop the `iroh` transport and keep the seam — ws-only, no protocol change, no consumer lock-in beyond a URL prefix.

## The test that matters

The adapter conformance suite (the A4 shape: connect → subscribe → offline write → online → serverAcked, plus signOut wipe and no-engine-type-leak) passes **identically over both transports** — same fixture, same marks, URL swapped `ws://` ↔ `iroh://`. And the field leg: a phone on cellular (no shared LAN) pairs via QR and completes an offline→online resume through the relay path.

---

### Verification notes (2026-08-27, this proposal's evidence pass)

- Client genericity: `client.rs` `:1155` / `:1200` / `:1234` (`Sink<Message>` bounds), dial at `:1305`.
- Server seam: `nostos_infra::transport::sync_handler` + `SyncRouterState`, mounted in `main.rs` router assembly.
- iroh facts: docs.rs/iroh/0.91.2 crate docs (QUIC/holepunch/relay model, `Endpoint::builder().bind()`/`.alpns(...)`, `connect(NodeAddr, alpn)`, `open_uni()`/`open_bi()`, MIT OR Apache-2.0). The exact accept-side handler trait surface is to be pinned by the D4 spike before implementation; everything cited above is from the current crate docs.
