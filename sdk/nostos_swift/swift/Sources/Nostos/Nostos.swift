// Nostos Swift SDK — idiomatic reactive facade over the UniFFI-generated
// `NostosClient` (defined in `swift-sources/nostos_swift.swift`, produced by
// `uniffi-bindgen generate`).
//
// The generated `NostosClient` exposes a low-level `watch(table:sink:)` that
// takes a `SnapshotSink` callback — a TRUE Rust→Swift PUSH (the app never
// wall-clock-polls; it is the Swift port of Flutter's `watch(table, rows_sink)`
// and a sibling of nostos_kotlin's identical `watch(table, sink)`). This file
// layers the Swift-native push primitive — `AsyncStream` — on top of that
// callback, so consumers write:
//
//     let stream = nostos.watch(table: "tasks")
//     for await snapshot in stream {
//         // `snapshot` is a JSON-array-of-objects String; fresh on every tick.
//     }
//
// The stream yields the initial snapshot immediately, then a fresh full-table
// snapshot after every change tick (remote apply or local write). Cancellation
// (Task.cancel / `break` / dropping the iterator) finishes the stream promptly;
// the Rust pump's lifecycle is tied to the sync session (`NostosClient` deinit /
// a session-replacing `connect()` aborts every pump on the Rust side).
//
// Why `AsyncStream` (not Combine / not a delegate): it is the canonical
// Swift structured-concurrency push primitive, composes with `Task` cancellation
// for free, and needs no external dependency — mirroring how nostos_node used
// napi's ThreadsafeFunction + an EventEmitter and nostos_dotnet used
// `IObservable`/`SnapshotSink` for the same reactive seam.

import Foundation

public extension NostosClient {
    /// Reactive watch: returns an `AsyncStream<String>` that yields the
    /// full-table snapshot (JSON array-of-objects) immediately, then again after
    /// every change tick (remote apply or local write). This is the idiomatic
    /// Swift port of Flutter's `watch(table, rows_sink)` and Kotlin's
    /// `watch(table, sink)` — a true Rust→Swift push surfaced as Swift's native
    /// push primitive. The consumer `await`s the stream and never wall-clock-
    /// polls the store.
    ///
    /// Overloads the generated low-level `watch(table:sink:)` (which takes a
    /// `SnapshotSink` directly). Use this method from app code; reach for
    /// `watch(table:sink:)` only if you supply your own sink.
    ///
    /// - Parameter table: must match the session's table (v1: one table per
    ///   client).
    /// - Returns: `AsyncStream<String>` of full-table snapshot JSON, one element
    ///   per tick (initial snapshot + every subsequent change).
    func watch(table: String) -> AsyncStream<String> {
        AsyncStream { continuation in
            let bridge = SnapshotStreamBridge(continuation: continuation)
            do {
                try self.watch(table: table, sink: bridge)
            } catch {
                // `connect()` not run / table mismatch: finish immediately so the
                // consumer's `for await` exits with nothing rather than hanging.
                continuation.finish()
                return
            }
            // Tear down on stream cancellation (Task.cancel, `break`, iterator
            // drop). The Rust pump is tied to the session and has no per-watch
            // cancel handle today (the floor; a `stopWatch(table:)` is the
            // mechanical follow-on). Finishing here releases the bridge and lets
            // the consumer's `for await` exit promptly; subsequent `onSnapshot`
            // calls from a still-live pump hit a finished continuation and are
            // no-ops (`AsyncStream.Continuation.yield` after `finish` is safe).
            continuation.onTermination = { @Sendable _ in
                bridge.finish()
            }
        }
    }
}

/// Bridge from the UniFFI `SnapshotSink` callback (synchronous, invoked from the
/// Rust pump task on a tokio worker) into a Swift `AsyncStream<String>`.
/// `onSnapshot(json:)` forwards each full-table snapshot to the stream
/// continuation; `finish()` (on stream termination) closes the continuation.
///
/// `@unchecked Sendable`: the Rust pump invokes `onSnapshot` from a tokio worker
/// thread, so instances cross thread boundaries. The only state is the
/// `AsyncStream.Continuation` (itself `Sendable`) and an `NSLock` guarding
/// `finish()` idempotency — both safe to share, so the unchecked conformance is
/// sound.
final class SnapshotStreamBridge: SnapshotSink, @unchecked Sendable {
    private let continuation: AsyncStream<String>.Continuation
    private let lock = NSLock()

    init(continuation: AsyncStream<String>.Continuation) {
        self.continuation = continuation
    }

    func onSnapshot(json: String) {
        continuation.yield(json)
    }

    /// Idempotent: called from `AsyncStream.onTermination`. A `yield` after
    /// `finish` is a no-op; the lock only guards against double-`finish`.
    func finish() {
        lock.lock()
        defer { lock.unlock() }
        continuation.finish()
    }
}
