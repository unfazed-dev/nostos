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
//   resolveDbPath(name)       → native path resolver (no UniFFI analogue). JS has no FS access on RN, so the
//                              native side maps a bare `name` to a writable per-app path (iOS:
//                              NSTemporaryDirectory()/name; Android: filesDir/name). Needed so a file-backed
//                              db survives a signOut-and-reopen — the cross-reopen wipe proof (:memory: gives
//                              each client its OWN empty store, so the wipe is unobservable across instances).
//   watchChanges(table, cb)   → NostosClient::watch(table, sink: SnapshotSink). Rust→JS PUSH: the native side
//                              retains `cb` (a Codegen-emitted RCTResponseSenderBlock, which self-marshals to
//                              the JS thread — safe to invoke from the nostos tokio worker) and calls it once
//                              with the INITIAL snapshot, then after every applied change. Verified on iOS
//                              AND Android (Android mirrors iOS: RN `Callback` wrapped in a UniFFI
//                              `SnapshotSink` adapter retained per-table in `NostosTurboModule.sinks`).
//   unwatchChanges(table)     → releases the retained JS callback so further ticks are no-ops. BINDING FLOOR:
//                              nostos_swift has no stop_watch — the Rust pump is tied to the session (it dies
//                              on connect-replace/signOut/deinit). So unwatch stops DELIVERY to JS, not the
//                              pump itself; the sink object is retained until session end (avoids a use-after-
//                              free: UniFFI holds a handle back into the Swift object). Honest ceiling.
//   setToken(token)           → NostosClient::set_token(token: Option<String>) (ADR-0029 #3). Hot-swap the
//                              bearer on the interior-mutable token cell — the reconnect loop reads it on
//                              its NEXT attempt, so a live session picks up the new token with NO disconnect.
//                              `null` = clear (anonymous). Callable before connect AND on a live session.
//   signOut()                 → NostosClient::sign_out() (ADR-0029): abort run loop → await quiesce →
//                              clear_local_state (rows + checkpoint + epoch + outbox + dead-letter) → drop
//                              session → clear token. Idempotent; the "B must not see A's rows" wipe.
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
  /**
   * Subscribe to a stream of full-table snapshots for `table`. The native side
   * retains `onSnapshot` and invokes it ON THE JS THREAD (the RN analogue of
   * napi's `ThreadsafeFunction` in nostos_node and the `SnapshotSink` UniFFI
   * callback in nostos_kotlin): once with the INITIAL snapshot immediately, then
   * again after every applied change. Each invocation carries a JSON
   * array-of-objects string — a FULL snapshot per tick (not a diff; self-healing
   * on lag), the same shape `query()` returns.
   *
   * The returned Promise resolves AFTER the initial snapshot has been emitted
   * to `onSnapshot`, so the caller knows the first frame has fired.
   *
   * Wave-B Codegen note: a `(rowsJson: string) => void` param is emitted as an
   * `RCTResponseSenderBlock`, which self-marshals onto the JS thread (via the
   * JSCallInvoker) — so the native impl MAY invoke it from the nostos tokio
   * worker directly; no explicit JS-thread hop is needed (verified on the iOS
   * sim: a `SnapshotSink.onSnapshot` firing on the runtime thread reaches JS).
   */
  watchChanges(
    table: string,
    onSnapshot: (rowsJson: string) => void,
  ): Promise<void>;
  /**
   * Release the retained JS callback for `table`; after this resolves,
   * `onSnapshot` will not be invoked again for `table`. Idempotent.
   *
   * BINDING FLOOR: nostos_swift has no `stop_watch` — the underlying Rust pump
   * is tied to the session lifecycle (it stops on a session-replacing
   * `connect()`, `signOut()`, or client drop). So this stops DELIVERY to JS
   * (the callback ref is nil'd), not the pump itself; the sink is retained
   * until session end so UniFFI's handle into it can't dangle. The honest
   * ceiling until a `stop_watch(table)` lands in the binding.
   */
  unwatchChanges(table: string): Promise<void>;
  /**
   * Resolve a bare `name` to a writable per-app SQLite file path (iOS:
   * `NSTemporaryDirectory()/name`). RN JS has no filesystem access, so this is
   * how JS obtains a db path that survives across a signOut-and-reopen — the
   * precondition for observing the cross-reopen wipe (`:memory:` gives each
   * client its own empty store, hiding the wipe across instances). The
   * returned path is passed straight to `connect(url, token, dbPath)`.
   *
   * iOS-verified; Android pending (Android ships watchChanges, but the path
   * resolver is still iOS-only — see `NostosTurboModule` for the gap).
   */
  resolveDbPath(name: string): Promise<string>;
  /**
   * Hot-swap the auth bearer WITHOUT tearing down the live session
   * (ADR-0029 #3). Maps to UniFFI `NostosClient::set_token(token: Option<String>)`
   * in nostos-swift / nostos-kotlin: it swaps the interior-mutable token cell the
   * reconnect loop reads on its NEXT attempt, so an already-running session
   * picks up the new token without a forced disconnect. Pass `null` to clear
   * (anonymous).
   *
   * Callable before `connect()` (stages the token for the first connect) AND on
   * a live session. It does NOT force a reconnect or tear anything down — the
   * same shape `nostos-client`'s `SyncClient::set_token` exposes.
   */
  setToken(token: string | null): Promise<void>;
  /**
   * Sign out (ADR-0029): abort the run loop, await quiescence, wipe local state
   * (rows + checkpoint + epoch + outbox + dead-letter), drop the session, and
   * clear the token. Maps to UniFFI `NostosClient::sign_out()`. The next
   * principal connecting on the same device sees a clean store — the "B must
   * not see A's rows, and A's unsynced writes must not be attributed to B"
   * guarantee. Idempotent — a no-op if no session is live.
   */
  signOut(): Promise<void>;
}

export default TurboModuleRegistry.getEnforcing<Spec>("NativeNostos");
