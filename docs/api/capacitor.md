# Capacitor — `@nostos-sync/capacitor`

Extracted from `sdk/nostos_capacitor/src/definitions.ts` on 2026-07-30.
Index: [`README.md`](README.md).

A Capacitor plugin wrapping [`@nostos-sync/web`](web.md)'s WASM engine, so it works on the web target as
well as native shells. Reads are the **KV store**, not SQL.

## `NostosPlugin`

`definitions.ts:94`. Every method takes a single options object — Capacitor's convention.

| Method | Signature |
|---|---|
| `configure` | `configure(options: ConfigureOptions): Promise<void>` |
| `connect` | `connect(options: ConnectOptions): Promise<NostosConnectResult>` |
| `subscribe` | `subscribe(options: { table: string; whereSql?: string \| null }): Promise<void>` |
| `write` | `write(options: WriteOptions): Promise<void>` |
| `query` | `query(options: QueryOptions): Promise<{ rows: NostosRow[] }>` |
| `checkpoint` | `checkpoint(): Promise<{ checkpoint: number }>` |
| `rowCount` | `rowCount(): Promise<{ rowCount: number }>` |
| `close` | `close(): Promise<void>` |

Returns are **wrapped objects**, not bare values — `{ checkpoint }` and `{ rowCount }`, not a
number. Easy to trip on.

### Option shapes

| Interface | Fields |
|---|---|
| `ConfigureOptions` | `wasmUrl: string` — where the plugin loads the WASM bundle from |
| `ConnectOptions` | `url: string`, `token?: string \| null`, `table?: string`, `whereSql?: string \| null` |
| `NostosConnectResult` | `rowCount: number`, `checkpoint: number` |
| `WriteOptions` | `table: string`, `op: string`, `pk: string`, `payload?: unknown`, `payloadJson?: string`, `clientWriteId: string` |
| `QueryOptions` | `table: string` — a **table name, not SQL** |
| `NostosRow` | `pk: string`, `payload: unknown` |

Two things to notice in `WriteOptions`: `clientWriteId` is **required** (it is how a write is
de-duplicated if you retry), and `payload` / `payloadJson` are alternatives — pass the object or
the pre-serialised string, not both.

```ts
import { Nostos } from "@nostos-sync/capacitor";

await Nostos.configure({ wasmUrl: "/assets/nostos_ffi_wasm_bg.wasm" });
const { rowCount } = await Nostos.connect({ url: "ws://127.0.0.1:8800/sync", table: "tasks" });
await Nostos.subscribe({ table: "tasks" });
await Nostos.write({
  table: "tasks", op: "upsert", pk: "1",
  payload: { title: "buy milk" }, clientWriteId: crypto.randomUUID(),
});
const { rows } = await Nostos.query({ table: "tasks" });
```

`configure` before `connect` — the plugin needs to know where its WASM lives, and the URL depends
on how your bundler emits assets.

## Ceilings

- **Reads are KV, not SQL** — `query` takes a table name and returns `{pk, payload}`. Inherited
  from the WASM apply engine (see [`web.md`](web.md)).
- One table per connection.
- No reactive stream — poll `query` after writes.
- **`package.json` depends on `"@nostos-sync/web": "file:../nostos_web"`.** That path dependency breaks the
  moment this is published, so `@nostos-sync/web` has to go to the registry first. A real publish
  blocker, not a nit.

## Proven by

`sdk-e2e` `capacitor` slice — builds the plugin, installs `example-app`, and runs Playwright
against it for a full PUSH + ECHO round-trip.
