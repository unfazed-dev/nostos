# nostos-dotnet

UniFFI bridge exposing `nostos_client::SyncClient<SqliteStorage>` to **.NET**
(iOS / Android / Windows / macOS) via the **Nord UniFFI-CS bindgen**. Mirrors
`sdk/nostos_swift` and `sdk/nostos_kotlin` — the SAME `SyncClient<SqliteStorage>`
the native, Tauri, Flutter, Swift, Kotlin, and Node SDKs drive, loaded into
.NET via UniFFI's proc-macro FFI, with no engine/wire changes.

> **Pre-1.0 caveat (honest):** this is a **feasibility scaffold**, not a
> polished SDK. The Rust surface compiles + cross-compiles to every target the
> Swift and Kotlin SDKs already target, and the Nord bindgen emits committed
> C# the reviewer can read without installing .NET — but no `.nupkg` is
> produced, no NuGet feed is wired, and **no C# runtime E2E has run on this
> host** (`dotnet` is not installed; E2E is **SKIP-with-reason**, see below).
> The REUSE thesis — one Rust proc-macro interface, four foreign bindings — is
> what this scaffold proves.

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

### 4. (Optional) Build the .csproj — SKIPPED on this host

`dotnet` is not installed on this host. The `.csproj` is multi-target
(`net8.0-ios;net8.0-android;net8.0-windows;net8.0-maccatalyst`) and sets
`<AllowUnsafeBlocks>true</AllowUnsafeBlocks>` (C# side only — the bindgen emits
`IntPtr` / P/Invoke pointers; the Rust crate stays `#![forbid(unsafe_code)]`).

```bash
# NOT RUN on this host — dotnet not installed
cd sdk/nostos_dotnet/dotnet
dotnet build Nostos.DotNet.csproj
```

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

## E2E status: SKIP-with-reason

No C# runtime E2E has run on this host because `dotnet` is not installed
(`which dotnet` → empty). The deliverable is:

1. Rust compiles + cross-compiles (host / iOS / iOS-sim / Android).
2. Windows-msvc FAILS (known — `ring` C dep + link need MSVC toolchain not on macOS; see Build §2).
3. Nord `uniffi-bindgen-cs` generates committed C# (`dotnet/generated/nostos.cs`).
4. `forbid(unsafe_code)` holds on the Rust crate.

C# E2E (construct `NostosClient`, `connect()`, `query("SELECT 1 AS one")`,
assert the row) is the next increment once `dotnet` is installed.

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
