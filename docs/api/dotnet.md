# .NET / MAUI — `Nostos.DotNet`

Extracted from `sdk/nostos_dotnet/` on 2026-07-30. Index: [`README.md`](README.md).

Same six members as [`kotlin.md`](kotlin.md), through `uniffi-bindgen-cs`. **C# gets PascalCase
method names** — verified against `dotnet/smoke/`, the app that actually compiles and runs
(`.Connect(`, `.Query(`, `.Dispose(`).

## Build

```bash
cargo build -p nostos_dotnet
uniffi-bindgen-cs --library target/debug/libnostos_dotnet.dylib --out-dir sdk/nostos_dotnet/dotnet
```

Ship the native library alongside your assembly per RID.

## `NostosClient`

| Member | C# signature | Notes |
|---|---|---|
| constructor | `new NostosClient(string url, string? token, string dbPath)` | pure handle |
| `Connect` | `void Connect()` | opens local SQLite + builds the client. **No network.** |
| `Subscribe` | `void Subscribe(string table)` | **opens the socket**, starts the run loop |
| `Write` | `ulong Write(string table, string op, string pk, string? payloadJson)` | durable sequence number |
| `Query` | `string Query(string sql)` | JSON array **string** — parse with `System.Text.Json` |
| `Checkpoint` | `ulong Checkpoint()` | durable LSN |

Implements `IDisposable` — the generated handle owns Rust-side resources, so `using` it.

```csharp
using var client = new NostosClient("ws://127.0.0.1:8800/sync", null,
    Path.Combine(Path.GetTempPath(), "nostos.db"));
client.Connect();
client.Subscribe("tasks");
client.Write("tasks", "upsert", "1", """{"title":"buy milk"}""");
using var doc = JsonDocument.Parse(client.Query("SELECT * FROM tasks"));
```

## ⚠️ The multi-target project does not build on a stock host

`dotnet/Nostos.DotNet.csproj` targets `net8.0-ios;net8.0-android;net8.0-windows;net8.0-maccatalyst`.
Attempted for real on 2026-07-30: it **fails** with `4× NETSDK1147 — the following workloads must
be installed: ios`. `dotnet` itself is fine; `dotnet workload list` is empty, and **every** TFM is
workload-gated, so nothing here builds without `dotnet workload restore` (a multi-GB install).

NETSDK1147 fires during workload import, **before any C# compiles**, so a "successful" run of that
command clears nothing past `TargetFrameworks` resolution.

**Nothing in CI builds this project** — the e2e slice builds `dotnet/smoke/Smoke.csproj` (plain
`net8.0`, host-only) instead. That is precisely why two fatal XML errors survived in the `.csproj`
until 2026-07-30. If you edit it, evaluate it — needs no workloads, ~2s, and strictly stronger than
an XML parse:

```bash
dotnet msbuild Nostos.DotNet.csproj -getProperty:TargetFrameworks -getProperty:PackageId \
  -getProperty:Version -getProperty:PackageLicenseExpression
```

`<AllowUnsafeBlocks>true</AllowUnsafeBlocks>` is set on the **C# side only** — the bindgen emits
`IntPtr`/P-Invoke. The Rust crate stays `#![forbid(unsafe_code)]`.

## Ceilings

One table per client; `Query` returns JSON text; no reactive stream.

## Proven by

`sdk-e2e` `dotnet` slice — the `smoke` console app loading the host `libnostos_dotnet.dylib` over
the UniFFI-CS surface, real PUSH + ECHO. The slice finds `dotnet` at `~/.dotnet/dotnet` even when
it is not on `PATH`.
