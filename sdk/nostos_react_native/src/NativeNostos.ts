// @nostos-sync/react-native — Codegen Turbo Native Module spec.
//
// This file is the CONTRACT the Wave-B native modules (Android Kotlin + iOS
// Swift) implement. It mirrors the UniFFI `NostosClient` surface exported by
// `sdk/nostos_swift/src/lib.rs` and `sdk/nostos_kotlin/src/lib.rs` —
// connect / subscribe / write / query / checkpoint — so the SAME
// `nostos_client::SyncClient<SqliteStorage>` that powers the native, Tauri,
// Flutter, Swift, Kotlin, and Node SDKs is reachable from JS over JSI.
//
// WHY A NATIVE MODULE (not WASM)
// --------------------------------
// RN's Hermes engine does NOT ship `global.WebAssembly` (Hermes issue #429,
// OPEN as of RN 0.84; the RN 0.84 release notes have zero WASM mentions).
// Nostos's `@nostos-sync/web` WASM core (`nostos-ffi-wasm`) is therefore a dead end
// inside RN. This TurboModule bridges to the already-shipped `nostos-swift` /
// `nostos-kotlin` UniFFI bindings instead — the SAME shape PowerSync's RN SDK
// validated (pure-TS facade over a native JSI backend).
//
// METHOD-BY-METHOD MAPPING (spec → UniFFI in sdk/nostos_swift + sdk/nostos_kotlin)
//   connect(url, token, dbPath) → NostosClient::new(url, token, db_path) + NostosClient::connect() -> Result<(), NostosError>
//   subscribe(table)          → NostosClient::subscribe(table: String) -> Result<(), NostosError>
//   write(table, op, pk, pj)  → NostosClient::write(table, op, pk, payload_json: Option<String>) -> Result<u64, NostosError>
//   query(sql)                → NostosClient::query(sql: String) -> Result<String, NostosError>  (JSON rows)
//   checkpoint()              → NostosClient::checkpoint() -> Result<u64, NostosError>
//
// Wave-B note: TurboModules are singletons instantiated by RN with a no-arg
// constructor — there is no JS-visible constructor surface to pass (url, token,
// dbPath) through. The spec therefore grows `connect(url, token, dbPath)` so
// the Kotlin module can lazily construct `uniffi.nostos_kotlin.NostosClient` on
// first `connect(...)`. The TS facade (`NostosClient.ts`) captures these in its
// config and passes them through on `connect()`.
//
// The native side blocks on its OWN tokio runtime (UniFFI sync methods — see
// the `ponytail:` in sdk/nostos_swift/src/lib.rs for why block-on-owned-runtime
// beat UniFFI async) and surfaces results to JS as resolved Promises. The
// JS-side API is therefore fully Promise-returning.

import type { TurboModule } from "react-native";
import { TurboModuleRegistry } from "react-native";

// Consumed by `@react-native/codegen` at native-build time (Wave B) to emit the
// C++/ObjC/Java bindings. `type: "modules"` = TurboModule (vs "components").
export const codegenConfig = {
  name: "NativeNostos",
  type: "modules",
  jsSrcsDir: "src",
};

/**
 * The TurboModule spec. Wave B's Kotlin/Swift implementations MUST satisfy
 * this interface exactly — Codegen generates the native bindings from it, and
 * drift between this spec and the native module's actual methods surfaces at
 * runtime as `TurboModuleRegistry.getEnforcing(...)` returning null (the
 * native side failed to register a conforming module).
 *
 * `payloadJson` is `string | null`: `null` matches UniFFI's
 * `Option<String>::None` (deletes carry no row image); a JSON string matches
 * `Some(String)`. The codegen TS parser recognizes `| null` as a nullable
 * annotation.
 */
export interface Spec extends TurboModule {
  /**
   * Construct the backing UniFFI `NostosClient(url, token, dbPath)` (idempotent
   * — re-connect reuses the existing handle) and open the local SQLite store +
   * build the SyncClient. No network I/O until `subscribe(table)`.
   *
   * `url` is the sync spine's WebSocket URL (e.g. `ws://host:port/sync`);
   * `token` is the optional auth bearer (null for anonymous); `dbPath` is the
   * SQLite file path (`:memory:` for ephemeral). These three match the UniFFI
   * `NostosClient::new` constructor args 1:1.
   */
  connect(url: string, token: string | null, dbPath: string): Promise<void>;
  /**
   * Start the live replication loop for `table` on the native side (spawns
   * `client.run_with_reconnect()` on the owned tokio runtime). The app polls
   * `query(sql)` / the facade's `pollRows(table)` to drain applied rows —
   * there is no row-tick callback to JS in this wave.
   */
  subscribe(table: string): Promise<void>;
  /**
   * Write a row. `op` is one of `"upsert"` | `"delete"` | `"patch"` (matches
   * `WriteOp::as_wire_str` in nostos-core; the native side rejects anything
   * else). Returns the durable sequence number / LSN.
   */
  write(
    table: string,
    op: string,
    pk: string,
    payloadJson: string | null,
  ): Promise<number>;
  /**
   * Run SQL against the on-device SQLite store. Returns a JSON-ROWS string
   * (UniFFI can't return `Vec<HashMap>` directly). The facade decodes it.
   */
  query(sql: string): Promise<string>;
  /** Current durable LSN (the resume_lsn on reconnect). */
  checkpoint(): Promise<number>;
}

export default TurboModuleRegistry.getEnforcing<Spec>("NativeNostos");
