# nostos-dotnet

UniFFI bridge exposing `nostos_client::SyncClient<SqliteStorage>` to **.NET**
(iOS / Android / Windows / macOS) via the **Nord UniFFI-CS bindgen**. Mirrors
`sdk/nostos_swift` and `sdk/nostos_kotlin` — the SAME `SyncClient<SqliteStorage>`
the native, Tauri, Flutter, Swift, Kotlin, and Node SDKs drive, loaded into
.NET via UniFFI's proc-macro FFI, with no engine/wire changes.

> **Pre-1.0 caveat (honest):** this is a **v0.1 alpha**, not a polished SDK.
> No `.nupkg` is produced and no NuGet feed is wired, so there is no
> `dotnet add package Nostos.DotNet` yet (A11).
>
> **A live C# E2E round-trip now passes** (`make sdk-e2e dotnet` — real PUSH +
> ECHO against the Rust spine). This paragraph previously claimed "no C#
> runtime E2E has run on this host; `dotnet` is not installed; E2E is
> SKIP-with-reason" — that was true when written and is now false. The stale
> version of the same claim, duplicated into a comment in
> `dotnet/Nostos.DotNet.csproj`, is how two fatal XML errors in that file went
> unnoticed until 2026-07-30: nothing builds it, because the E2E builds
> `dotnet/smoke/Smoke.csproj` instead.

## Usage

```csharp
using uniffi.nostos;

var client = new NostosClient("ws://127.0.0.1:8080/sync", token: null, dbPath: "cairn.db");
client.Connect();
client.Subscribe("tasks");

client.Write("tasks", "upsert", "t1", JsonSerializer.Serialize(new { title = "Walk dog" }));
var rowsJson = client.Query("SELECT * FROM tasks");   // JSON string
var lsn = client.Checkpoint();
```

The Nord bindgen PascalCases the UniFFI method names (`connect` → `Connect`) and
puts everything in the `uniffi.nostos` namespace (from `uniffi.toml`), **not** a
`Nostos.DotNet` namespace — the assembly name and the namespace differ on purpose.

All calls are **blocking**: the Rust side owns a multi-thread tokio runtime and
`block_on`s. `Write` returns the outbox id once the write is durable locally,
**not** when the server acks it — see
[ADR-0027](../../docs/adr/0027-write-outcome-visibility-in-the-client-sdk.md).

This exact sequence is what [`dotnet/smoke/Program.cs`](dotnet/smoke/Program.cs)
runs in the passing E2E.

## Push notifications (ADR-0037)

Push is a **doorbell**, not a data channel: a data-only `{table, lsn}` hint
wakes the app, and the sync connection — never the push rail — delivers the
rows, resuming from the durable LSN checkpoint.

MAUI push is **host-dependent**: .NET ships no push rail of its own — the
host app registers with FCM (Android), APNs (iOS), or WNS (Windows) via its
own plugin (e.g. `Plugin.PushNotification`) or platform APIs, then hands the
token here:

```csharp
// From the host app's push plugin callback (token acquisition is NOT ours):
client.RegisterPushToken("fcm", fcmToken);   // "apns" / "webpush" accepted too
// SignOut() deregisters session-registered tokens automatically (best-effort).
```

Nothing here adds a NuGet dependency — `RegisterPushToken` is a plain REST
call (`POST /push-tokens`, same JWT the client uses for `/sync`); the server
stamps tenant/account and the SDK never attests them. A non-`204` throws
`NostosError.Message` carrying the status + body. Tokens registered before a
process restart are not auto-deregistered on a later sign-out — the server
prunes them when the rail reports the token dead (410 / `UNREGISTERED`).

**Wake entry** — the host app's background push handler (an Android
`FirebaseMessagingService`, an iOS `UNUserNotificationCenterDelegate` /
`didReceiveRemoteNotification`, a Windows toast background task) makes sync
run; nostos picks up from there:

```csharp
// Paused app (a live NostosClient whose loop was Disconnect()-ed): the delta
// past the durable checkpoint applies on reconnect.
client.Resume();

// Killed app: no handle survives. Cold-open the SAME dbPath — the durable
// checkpoint lives in the SQLite file, so this is a delta catch-up, not a
// resync:
var nostos = new NostosClient(url, token, dbPath);
nostos.Connect();
nostos.Subscribe("tasks");   // delta applies from the checkpoint
```

Verified claims (engine-level, shared with kotlin/swift): the warm path
(`disconnect` → `resume` applies the delta from the checkpoint, no loss) is
pinned by nostos-client's
`disconnect_then_resume_applies_delta_from_checkpoint_without_loss`; the
cold path's premise — the checkpoint survives process death on disk — is
pinned by `checkpoint_survives_drop_and_reopen_on_disk` (ADR-0016). This
crate's `disconnect_keeps_local_state_queryable_and_resume_reenters` pins
the FFI port.

## Why UniFFI-CS (Nord)

The official `mozilla/uniffi` ships bindgens for Swift, Kotlin, Python, Ruby —
but **not C#**. The Nord Security fork
([`NordSecurity/uniffi-bindgen-cs`](https://github.com/NordSecurity/uniffi-bindgen-cs))
is the canonical C# bindgen; it tracks upstream UniFFI metadata versions via
its `--tag v0.9.2+v0.28.3` (bindgen 0.9.2 + UniFFI metadata 0.28.3). The Rust
crate in this SDK is pinned to `uniffi = "=0.28.3"` for the same reason —
the metadata encoding version on both sides MUST agree.

`cbindgen` + hand-written P/Invoke was the alternative; it was rejected because
it loses the **REUSE** thesis: nostos_swift and nostos_kotlin already use the
UniFFI proc-macro surface (`setup_scaffolding!` + `#[derive(uniffi::Object)]`
+ `#[uniffi::export]`), and nostos_dotnet points the SAME Rust interface at a
fourth foreign binding (C#). One `src/lib.rs`, four foreign bindings.

## Layout

```
sdk/nostos_dotnet/
├── Cargo.toml              # standalone workspace, uniffi = "=0.28.3" PIN
├── src/lib.rs              # NostosClient Object: connect/subscribe/query/write/checkpoint
├── uniffi.toml             # namespace = "Nostos" (bindgen config)
├── .cargo/config.toml      # Android NDK linker (mirrors nostos_kotlin)
├── dotnet/
│   ├── Nostos.DotNet.csproj # net8.0 multi-target (iOS/Android/Windows/maccatalyst)
│   └── generated/
│       └── nostos.cs        # COMMITTED output of uniffi-bindgen-cs (namespace uniffi.nostos)
```

## Build

### 1. Install the Nord UniFFI-CS bindgen (one-time)

```bash
cargo install uniffi-bindgen-cs \
  --git https://github.com/NordSecurity/uniffi-bindgen-cs \
  --tag v0.9.2+v0.28.3
```

Verify: `uniffi-bindgen-cs --version` → `uniffi-bindgen 0.9.2+v0.28.3`.

### 2. Build the Rust cdylib for each target

```bash
cd sdk/nostos_dotnet

# Host (aarch64-apple-darwin or x86_64-apple-darwin)
cargo build --release

# iOS device
cargo build --release --target aarch64-apple-ios

# iOS simulator
cargo build --release --target aarch64-apple-ios-sim

# Android (arm64-v8a) — needs the NDK env vars (mirrors nostos_kotlin's harness).
# All THREE are required: cc-rs looks up the C compiler via CC_<target>, the
# archiver via AR_<target> (defaults to aarch64-linux-android-ar which is NOT
# in the NDK bin — the NDK ships llvm-ar instead), and ANDROID_NDK_HOME for
# sysroot discovery. Without AR_aarch64_linux_android, the build fails with
# `cc-rs: failed to find tool "aarch64-linux-android-ar"`.
NDK=/Users/$USER/Library/Android/sdk/ndk/28.2.13676358
CC_aarch64_linux_android=$NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin/aarch64-linux-android24-clang \
AR_aarch64_linux_android=$NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-ar \
ANDROID_NDK_HOME=$NDK \
cargo build --release --target aarch64-linux-android
```

**Windows**: `cargo build --release --target x86_64-pc-windows-msvc` (or
`aarch64-pc-windows-msvc`) **FAILS** on this macOS host — the `ring` C
dependency can't cross-compile without the MSVC C toolchain, and the final
link needs the Windows SDK + MSVC linker, neither of which is installed. This
is a **known limitation of cross-compiling to windows-msvc from macOS**, NOT a
bug in this scaffold. Build Windows artifacts in CI on a `windows-latest`
runner.

### 3. Regenerate the committed C# bindings

```bash
cd sdk/nostos_dotnet
uniffi-bindgen-cs \
  --library target/release/libnostos_dotnet.dylib \
  --out-dir dotnet/generated \
  --config uniffi.toml
```

The `--library` flag reads proc-macro metadata embedded in the cdylib (no UDL
file needed — same proc-macro-only path nostos_swift/nostos_kotlin use, just with
a different bindgen reading the metadata). The output
`dotnet/generated/nostos.cs` (the Nord bindgen names the file after the
namespace `nostos`, not after the primary class) is **committed** so reviewers
can read the C# surface without installing .NET. The C# namespace is
`uniffi.nostos` (Nord bindgen convention: `uniffi.<namespace>`).

### 4. (Optional) Build the multi-target .csproj

`Nostos.DotNet.csproj` is multi-target
(`net8.0-ios;net8.0-android;net8.0-windows;net8.0-maccatalyst`) and sets
`<AllowUnsafeBlocks>true</AllowUnsafeBlocks>` (C# side only — the bindgen emits
`IntPtr` / P/Invoke pointers; the Rust crate stays `#![forbid(unsafe_code)]`).

```bash
cd sdk/nostos_dotnet/dotnet
dotnet build Nostos.DotNet.csproj      # needs the iOS/Android/Windows workloads
```

**Attempted for real on 2026-07-30 and it FAILS on this host** — `4× error
NETSDK1147: To build this project, the following workloads must be installed:
ios`. `dotnet` itself is fine (8.0.422 at `~/.dotnet/dotnet`), but `dotnet
workload list` is **empty**, and every one of the four TFMs is workload-gated,
so there is no TFM that builds here without `dotnet workload restore` (a
multi-GB install). Do not read the earlier "(Optional)" as "unverified but
probably fine": it is *known not to compile on a stock host*.

NETSDK1147 fires during workload import, **before any C# compiles**, so this
does not clear the file — it only proves MSBuild got as far as resolving
`TargetFrameworks`. Everything past that point is still unproven.

**This project is not built by any test** — the E2E builds
`dotnet/smoke/Smoke.csproj` (plain `net8.0`, host-only) instead, because the
multi-target build needs mobile workloads. That is precisely why two fatal XML
errors survived in it until 2026-07-30 (a mismatched `PackageProjectUrl` closing
tag and a double hyphen inside an XML comment, which XML forbids).

**Guard, if you edit this file** — evaluate it, don't just parse it. This needs
no workloads, runs in ~2s, and is strictly stronger than an XML parse: it proves
MSBuild can *evaluate* every property and item, and it shows you the packaging
metadata that would otherwise only be checked at `dotnet pack` time.

```bash
dotnet msbuild Nostos.DotNet.csproj \
  -getProperty:TargetFrameworks -getProperty:PackageId \
  -getProperty:Version -getProperty:PackageLicenseExpression
# → TargetFrameworks net8.0-ios;net8.0-android;net8.0-windows;net8.0-maccatalyst
#   PackageId Nostos.DotNet · Version 0.1.0 · PackageLicenseExpression Apache-2.0
```

Verified passing 2026-07-30. `-getItem:PackageReference` returns `[]`, but read
that narrowly: the evaluation ran **without workloads**, so it cannot see item
groups the workload SDKs contribute or that are conditioned on a TFM it never
resolved. It is evidence of no *hand-declared* NuGet dependency in this file,
not proof that workloads are the only thing between it and a compile.

## API surface

The SAME surface as nostos_swift and nostos_kotlin — `NostosClient` Object with:

| method | signature | notes |
|---|---|---|
| `new` (constructor) | `(url, token: Option<String>, db_path) -> NostosClient` | no I/O; runtime + handle only |
| `connect` | `() -> ()` | opens SQLite store, builds `SyncClient`. Idempotent. |
| `subscribe` | `(table) -> ()` | spawns `run_with_reconnect` on owned runtime. Poll-only. |
| `write` | `(table, op, pk, payload_json: Option<String>) -> u64` | op ∈ `"upsert"`/`"delete"`/`"patch"`. Returns outbox seq. |
| `query` | `(sql) -> String` | JSON-array-of-objects (same as nostos_node/nostos_tauri/nostos_swift/nostos_kotlin). |
| `checkpoint` | `() -> u64` | current durable LSN. Fresh store = `0`. |
| `register_push_token` | `(platform, token) -> ()` | platform ∈ `"fcm"`/`"apns"`/`"webpush"`; `POST /push-tokens`, same JWT as the sync connection (ADR-0037). C#: `RegisterPushToken`. |
| `deregister_push_token` | `(token) -> ()` | `DELETE /push-tokens/{token}`; `SignOut` calls it for session-registered tokens. C#: `DeregisterPushToken`. |

## `unsafe` policy

- **Rust crate** (`src/lib.rs`): `#![forbid(unsafe_code)]`. UniFFI's
  macro-generated FFI scaffolding lives in the `uniffi` dependency's proc-macro
  output, not in this crate's hand-written source, so the forbid does not
  interact with it — the ADR-0015 addendum machine-generated exception. No
  hand-written `unsafe` exists in this crate.
- **C# project** (`dotnet/Nostos.DotNet.csproj`):
  `<AllowUnsafeBlocks>true</AllowUnsafeBlocks>` — this is a **C# project
  property**, not a Rust property; the Nord bindgen emits `IntPtr` / P/Invoke
  glue that C# requires `unsafe` blocks to touch. The Rust crate stays
  forbid-unsafe regardless.

## E2E status: PASSING

```bash
make sdk-e2e dotnet        # from the repo root
```

A live C# round-trip runs against the shared Rust spine and is gated on both
directions: **PUSH** (a row pushed server-side lands in on-device SQLite and is
visible via `Query`) and **ECHO** (a C# `Write` comes back through the server's
write-back fan-out). Driven by [`dotnet/smoke/Program.cs`](dotnet/smoke/Program.cs)
via `scripts/run-dotnet-e2e.sh`.

> Superseded, kept as the record: this section used to read "SKIP-with-reason —
> no C# runtime E2E has run on this host because `dotnet` is not installed
> (`which dotnet` → empty)". `dotnet` lives at `~/.dotnet/dotnet`, which is not
> on `PATH` — hence the original `which` check failing. The harness resolves that
> fallback explicitly (`scripts/run-dotnet-e2e.sh`), so a bare `which dotnet`
> returning empty does **not** mean .NET is unavailable.

Also verified:

1. Rust compiles + cross-compiles (host / iOS / iOS-sim / Android).
2. Windows-msvc FAILS (known — `ring`'s C dep + link need an MSVC toolchain not
   present on macOS; see Build §2).
3. Nord `uniffi-bindgen-cs` generates committed C# (`dotnet/generated/nostos.cs`).
4. `forbid(unsafe_code)` holds on the Rust crate.

## Verbs

```bash
# host build
cd sdk/nostos_dotnet && cargo build --release

# regenerate committed C#
uniffi-bindgen-cs --library target/release/libnostos_dotnet.dylib \
  --out-dir dotnet/generated --config uniffi.toml

# Rust unit tests (offline — no .NET, no Postgres)
cargo test
```

## Reading order

- `src/lib.rs` — the UniFFI surface (mirrors `sdk/nostos_swift/src/lib.rs`).
- `Cargo.toml` — the `=0.28.3` pin rationale (header comment).
- `dotnet/generated/nostos.cs` — the committed bindgen output (namespace `uniffi.nostos`, class `NostosClient`).
- ADR-0015 (FFI bridge strategy) + ADR-0015 addendum (machine-generated `unsafe` exception).
