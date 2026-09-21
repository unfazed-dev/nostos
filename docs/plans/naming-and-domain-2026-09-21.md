# Naming + domain: what actually needs a domain, and nostos vs qairn

Status: decision doc, 2026-09-21. Operator call. Nothing here is implemented.

## 1. You do not need a domain to ship v0.2.0

Three different things keep getting called "the domain". They are unrelated.

| Thing | Needs a domain? | What to do instead |
|---|---|---|
| **Maven Central groupId** | No | `io.github.<github-username>` — verified by a GitHub signup on the Central Portal, no DNS, no TXT record |
| **The marketing page** | No (nice later) | GitHub Pages on the repo, or Cloudflare Pages. Free, 10 minutes |
| **Nostos Cloud** (hosted) | Yes, eventually | Not a launch blocker. Not built yet |

### Why a Cloudflare Worker does NOT solve the Maven problem
Central verifies **ownership of a domain** via a DNS TXT record on a domain
you control. A `*.workers.dev` subdomain is Cloudflare's domain, not yours —
you cannot set its TXT record and Central will not accept it. The Worker is
irrelevant to publishing. `io.github.<user>` sidesteps the whole question.

### Why a Cloudflare Worker cannot host Nostos Cloud either
Nostos Cloud is a long-lived stateful Rust process: it holds a Postgres
**logical replication** connection (streaming replication protocol over a
persistent TCP socket) and thousands of concurrent WebSockets with per-session
state. Workers are short-lived isolates with no raw TCP to a Postgres
replication slot and no durable in-process session store. Wrong shape. When
Cloud happens it wants a VM / Fly.io / Railway / Kubernetes, and a real domain
for TLS + auth callbacks + billing. A Worker is fine for the *marketing page*
and nothing else here.

**Conclusion: zero domains needed for the OSS launch.** Revisit at Cloud alpha.

## 2. nostos vs qairn — verified registry state (checked 2026-09-21)

| Registry | `nostos` | `qairn` |
|---|---|---|
| crates.io `<name>` | **TAKEN** — v0.0.0, 242 dl, placeholder squat | free |
| crates.io `<name>-core` | **TAKEN** — v0.1.0 | free |
| crates.io `<name>-cli` | **TAKEN** — v0.1.6, actively versioned | free |
| crates.io `<name>-server` | free | free |
| npm `<name>` | **TAKEN** | free |
| npm `@<name>` scope | unclaimed | unclaimed |
| pub.dev `<name>` | free | free |
| pub.dev `<name>_flutter` | free | free |

### Recommendation: rename to **qairn**

The blocker is `nostos-cli`. The product's primary UX is a CLI called `nostos`,
and `cargo install nostos-cli` already installs a stranger's crate at v0.1.6.
`nostos` itself is squatted at v0.0.0, so the flagship crate name is gone
permanently — no amount of waiting gets it back. Publishing as
`nostos-sync-cli` that produces a binary named `nostos` is a papercut shipped
on day one and a support burden forever.

`qairn` is free on every registry simultaneously — Rust, npm (bare name *and*
scope), pub.dev, and the Maven artifactId. One name, no hyphen gymnastics, and
it owns its search results outright; "nostos" competes with a Scottish terrier
breed, Nostos Energy, Nostos Capital and nostos.info.

Cost of the rename, honestly: it is mechanical but wide — crate names, the
`NOSTOS_*` env vars, the Maven artifactIds, the Flutter/RN package names, the
CLI binary, every doc and ADR, the repo name, and the unpushed `v0.2.0` tag
gets re-cut. **Now is the cheapest this will ever be: zero published
artifacts, zero users, one unpushed tag.** After the first publish it is
permanent (crates.io and Maven Central releases cannot be deleted).

Against: `qairn` loses the metaphor. A cairn is a trail marker — stones that
persist and guide you back — which *is* the product story, and the launch post
leans on it. `qairn` keeps the sound and drops the meaning, and q-without-u
reads as a typo to some. That is a brand judgement, not a technical one, and
it is the operator's call.

### If you keep `nostos`
Workable but compromised: publish `nostos-server` (free), and find free names
for the core and CLI crates. The Maven groupId `io.github.<user>` and the
`@nostos-sync` npm scope and all four pub.dev names are unaffected either way.

## 3. What to do, in order
1. Decide the name. Everything below is blocked on it and nothing else is.
2. Claim the npm scope and the crates.io names the same day the decision lands
   — both are first-come and this doc is evidence they are currently free.
3. Central Portal signup with `io.github.<user>`; generate the GPG key.
4. Marketing page on GitHub Pages. Domain optional, later, Cloud-only.
