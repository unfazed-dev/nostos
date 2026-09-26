# Atlet across Nostos SDKs — cloud reference contract

## Goal

Ship the same **visual Atlet application** through the Rust client and every
published SDK: Flutter, web, Node, Tauri, React Native, Capacitor, Swift,
Kotlin, and .NET. Each version exercises Nostos's local read model and durable
outbox. An admin and two customers use one hosted database per provider, so a
change made by one SDK is visible in another SDK after sync. Supabase and
Appwrite have separate hosted databases and provider-specific migrations; the
app's behavior and acceptance scenarios are the same.

## Application contract

- Customer: sign in, see catalog, record training sessions, add cart items,
  complete a demo payment, see order and fulfilment events. Private sessions,
  cart, orders, and events never appear for another customer.
- Admin: manage catalog, orders, and user profiles. Admins may fulfil orders
  and disable another user's sync access, but cannot see private sessions or
  disable their own access.
- Offline: a write renders immediately in the local store, survives a process
  restart, then reaches cloud and another device after reconnect. A remote
  delete removes the local row. Push is a wake-up hint; a missed push cannot
  prevent catch-up.
- Every app offers **direct** (default) and **server** transport selection.
  Provider selection is fixed for a signed-in session. Changing it signs out,
  closes the engine, and uses separate local storage.
- Every UI has the same core screens: sign-in, sessions, catalog, cart,
  checkout, order history, and admin catalog/orders/users. The controls show
  connection state, pending writes, pause/resume, and sign-out.

## Implementation order and gates

| Phase | Implementation | Required proof |
|---|---|---|
| 1 | Flutter native + Appwrite direct | Cloud migrations and Function, admin/A/B visual flow, offline sessions and order, private ACL, four independent SQLite clients, role reset, CI gate |
| 2 | Flutter web + Appwrite direct | Browser OPFS path, real Auth/Function transport, restart and offline replay, admin/A/B UI on Chrome |
| 3 | Flutter Appwrite server mode | Hosted `nostos-server` adapter over the same TablesDB journal; parity with direct against the same project |
| 4 | Flutter push | Real Appwrite Messaging provider and physical target; notification and missed-push catch-up |
| 5 | Supabase parity | Same Flutter journeys in both modes against the existing hosted Supabase project; versioned schema fixture |
| 6 | Remaining SDKs | Each SDK's visual Atlet port, direct/server parity, shared cloud journeys, on-device or browser validation |
| 7 | Cross-SDK convergence | Simultaneous devices using different SDKs, both providers independently, offline conflict/replay/isolation checks |

Phase 1 is implemented on the `atlet-appwrite-flutter` worktree and has passed
the real cloud native and macOS visual flows. Phase 2 is implemented on the
`atlet-appwrite-web` worktree; its 15 Chrome broker regressions and hosted
admin/customer A/customer B cloud gate pass locally, including OPFS reload,
checkout, fulfilment, and a same-origin account switch. Source review found
no remaining lifecycle blocker; PR CI is pending. Phases 3–7 are open. The
[Flutter Appwrite design](atlet-appwrite-flutter-design-2026-09-26.md) and
[ADR-0050](../adr/0050-appwrite-direct-sync-function-and-journal.md) define
the first provider path. The historical
[`nostos-reference-demo-app.md`](nostos-reference-demo-app.md) described a
Tasks server-mode prototype and does not define this Atlet contract.

## Repeatable runners

The persistent launchers live in `apps/atlet/harness` and are **Rust binaries**.
The existing Appwrite migration, Function, native, and Flutter runners are
the first slice. Give each SDK one Rust launcher with parameters for provider,
mode, device/OS, and suite (`smoke`, `e2e`, `pressure`, `stress`, `soak`). A
runner records project ID, commit, SDK and app versions, platform, unique
fixture IDs, timing, errors, and observed convergence in ignored JSON results.
It must clean temporary rows through the journal; delivered demo orders remain
only when the scenario explicitly says so. Tests never claim a pass because
credentials, a toolchain, a device, or a provider was absent.

The arxa CI jobs mirror `scripts/check.sh` areas. Real cloud checks use scoped
demo credentials; every required check must execute on a PR event, because
GitHub does not count `workflow_dispatch` jobs as PR required checks. A PR
stays draft while any platform or physical push gate remains unverified.

## External inputs

The ADS Appwrite `nostos` project already exists, with Auth demo accounts and
the `atlet` TablesDB schema. Appwrite Messaging currently has no FCM/APNs
provider. A provider credential and physical test target are needed for phase
4. Keep those secrets out of the repository and runner evidence.
