# ADR-0041 D5 — the field leg (cellular QR-pair + relay resume)

- **Status:** runbook + desktop automation ready (2026-09-01). The in-hand
  cellular leg is the owner's — everything scriptable is scripted in
  [`tool/d5_field_leg.sh`](../../tool/d5_field_leg.sh).
- **Acceptance (from `docs/plans/nostos-integration-tauri-flutter-push.md` §Track D):**
  a phone on **cellular** (Wi-Fi off) pairs via QR and completes an
  **offline→online resume through the relay path**. Until then no consumer
  defaults to `iroh://` (the off-default posture D7 pinned is the rule).

## Why this is the last ADR-0041 gate

D6 (native run_session refactor, `da772aa`), D7 (Flutter SDK iroh feature,
`4c7de71`+`599cbf1`), D8 (relay guidance + privacy note, `c060bee`) are closed;
ws/iroh conformance parity re-ran green at `680852f`. What no rig has proven yet
is the phone on a **cellular** network — no LAN shortcut, relay or nothing.

## Part 1 — desktop bring-up (automated: `bash tool/d5_field_leg.sh`)

The script boots `nostos-server --transport iroh` (fake replicator, so rows keep
flowing), scrapes the **QR-native dial URL** (`iroh://<node>?ticket=…` — the
ticket carries relay + direct-address hints), health-checks it, then proves
**resume-not-restart** from this Mac: dial once (checkpoint N), let the server
keep emitting, dial again (checkpoint must be **> N** — the second session
resumed from the durable checkpoint rather than replaying from zero). That is
the same engine behavior the phone must show; only the network path differs.

Self-hosted relay (recommended for the field leg — drops the n0-fleet variable):

    iroh-relay --dev            # OPERATING.md §9 — or a TLS-configured real one
    NOSTOS_IROH_RELAY_URL=http://127.0.0.1:3340 bash tool/d5_field_leg.sh

Privacy note (carried from OPERATING.md §9): the relay sees connection **metadata
only** — payloads are device-keyed E2E QUIC; `iroh.link` discovery publishes
endpoint-id↔addr mappings. A self-hosted relay keeps even that in-house.

## Part 2 — the phone leg (owner, in hand, ~10 minutes)

1. **Build the fixture with iroh on** — prebuilt binaries never carry it (D7):

       cd <flutter fixture using nostos_flutter>
       NOSTOS_FLUTTER_CARGO_FEATURES=iroh flutter run -d <device> --profile

2. **Cellular only**: Wi-Fi OFF on the phone (this is the point — no LAN path
   can exist, so success proves the relay). Both devices on the internet.
3. **Pair via QR**: render the script's printed dial URL as a QR (the arxa
   pairing QR already carries exactly this ticket shape — `iroh_sync.rs` docs)
   and scan it with the fixture.
4. **Sync, then go offline**: let the first sync complete, then airplane-mode
   for ~30s while the desktop keeps emitting (fake replicator or real traffic).
5. **Reconnect and watch**: the client must **resume** — checkpoint advances,
   missed rows appear, no full re-snapshot storm. `iroh_dial_check` from any
   machine that can reach the endpoint gives the desktop-side comparison view.

## Pass criteria + where to record

PASS = phone-on-cellular pair + one offline→online resume with a monotonically
advancing checkpoint. Record in `docs/plans/nostos-integration-tauri-flutter-push.md`
(the D5 bullet flips to CLOSED with date + evidence: probe logs, checkpoint
numbers, relay used). Only then may a consumer default to `iroh://`.

## Triage

- **No dial URL printed** — the server failed to bind the iroh endpoint;
  `tail /tmp/nostos-d5/server.log`.
- **Dial times out from the Mac** — relay unreachable: with
  `NOSTOS_IROH_RELAY_URL` set, check the relay process/port; without it, the n0
  fleet must be reachable from THIS network.
- **Resume replays from zero** — checkpoint persistence broke; that is a bug,
  not a network artifact — file it against ADR-0025/0041 resume semantics with
  both probe logs.
- **Phone works on Wi-Fi but not cellular** — carrier NAT blocking UDP QUIC to
  the relay is the prime suspect; try the self-hosted relay on a public host
  with 443/UDP open.