// NostosSmoke — live-replication E2E on the iPhone simulator.
//
// Drives the SAME two-direction round-trip the Rust reference template
// (`crates/nostos-client/tests/e2e_live_replication.rs`) proves, adapted to
// Swift + UniFFI:
//   1. connect() → subscribe("tasks") → run loop applies rows to nostos_data.
//   2. POST /push to the spine → server pushes a `tasks` row → poll query()
//      until the row lands on-device → [swift-e2e] PUSH_OK.
//   3. SDK write()s `swift-echo` to its durable outbox → the spine's echo
//      WriteBack re-emits it → run loop applies it → poll query() →
//      [swift-e2e] ECHO_OK.
//
// The spine (target/debug/examples/e2e_server) is spawned by build.sh as a
// background child; its NOSTOS_E2E_PORT is conveyed to this app via the
// SIMCTL_CHILD_NOSTOS_E2E_PORT env var (simctl's documented injection prefix).
// The iOS simulator shares the host's localhost, so the app reaches the spine
// at ws://127.0.0.1:<port>/sync — no 10.0.2.2-style remap needed (that's
// Android-emulator-only).
//
// This file is compiled INTO the same Swift module as the UniFFI-generated
// `nostos_swift.swift` (added to the target's sources via project.yml), so the
// `NostosClient` symbol is visible without a module import. The C FFI module
// `nostos_swiftFFI` (header in `../../swift-sources/`) is exposed via the
// target's HEADER_SEARCH_PATHS + the bridging header.
//
// ponytail: this is a test harness, not a shipping iOS app. It launches, runs
// the round-trip on a background queue, prints delimited [swift-e2e] lines to
// stdout, and `exit(0)`s. The upgrade path is the real SDK's iOS demo app.

import Foundation
import UIKit

// MARK: - Spine endpoint discovery

/// The port the spine announced via `NOSTOS_E2E_PORT=`. build.sh injects this
/// via `SIMCTL_CHILD_NOSTOS_E2E_PORT` (simctl's documented env-injection
/// prefix). Fatal if absent — there's no useful test without a live spine.
private func spinePort() -> UInt16 {
    guard let raw = ProcessInfo.processInfo.environment["NOSTOS_E2E_PORT"],
          let port = UInt16(raw)
    else {
        print("[swift-e2e] FAIL: NOSTOS_E2E_PORT env var not set or invalid")
        exit(1)
    }
    return port
}

// MARK: - Synchronous HTTP POST /push

/// POST a row to the spine's `/push` control endpoint and block until the
/// spine responds. Mirrors the Rust template's `http_push`: the spine only
/// needs `pk` + `payload` to inject a `tasks` row through the fan-out.
private func httpPush(port: UInt16, body: String) {
    let url = URL(string: "http://127.0.0.1:\(port)/push")!
    var req = URLRequest(url: url)
    req.httpMethod = "POST"
    req.setValue("application/json", forHTTPHeaderField: "Content-Type")
    req.httpBody = body.data(using: .utf8)
    let semaphore = DispatchSemaphore(value: 0)
    var failure: String? = nil
    URLSession.shared.dataTask(with: req) { _, response, error in
        if let error = error {
            failure = "transport: \(error)"
        } else if let http = response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            failure = "non-2xx status: \(http.statusCode)"
        }
        semaphore.signal()
    }.resume()
    if semaphore.wait(timeout: .now() + .seconds(5)) == .timedOut {
        print("[swift-e2e] FAIL: POST /push timed out after 5s")
        exit(1)
    }
    if let failure = failure {
        print("[swift-e2e] FAIL: POST /push \(failure)")
        exit(1)
    }
}

// MARK: - query() polling

/// Poll `client.query(sql)` until the result string contains `needle` or the
/// timeout fires. Returns true on hit, false on timeout. Mirrors the Rust
/// template's `poll_row`: 100ms interval, generous bound for the WS round-trip
/// + engine apply.
private func pollQueryContains(_ client: NostosClient, sql: String, needle: String,
                                timeoutSeconds: Double) -> Bool {
    let deadline = Date(timeIntervalSinceNow: timeoutSeconds)
    while Date() < deadline {
        do {
            let rows = try client.query(sql: sql)
            if rows.contains(needle) {
                return true
            }
        } catch {
            // query() can transiently fail if the run loop is mid-flush; the
            // poll loop will retry until the deadline.
        }
        Thread.sleep(forTimeInterval: 0.1)
    }
    return false
}

// MARK: - App shell

final class SmokeAppDelegate: UIResponder, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        // Run the round-trip off the main run loop so UIApplication's main
        // loop has been initialized before we tear the process down with
        // exit(0).
        DispatchQueue.global(qos: .userInitiated).async {
            Smoke.run()
        }
        return true
    }
}

enum Smoke {
    static func run() {
        print("[swift-e2e] BEGIN iPhone-sim live round-trip")

        let port = spinePort()
        let wsUrl = "ws://127.0.0.1:\(port)/sync"
        print("[swift-e2e] spine wsUrl=\(wsUrl)")

        // File-based DB so the engine's apply path + durable outbox survive
        // across the run loop's flush + echo re-emit (mirrors the Rust
        // template's PID-unique temp path). Clean remove first so a stale
        // file from a prior run can't yield a false positive.
        let dbPath = (NSTemporaryDirectory() as NSString)
            .appendingPathComponent("nostos-swift-e2e-\(getpid()).sqlite")
        try? FileManager.default.removeItem(atPath: dbPath)

        do {
            let client = try NostosClient(url: wsUrl, token: nil, dbPath: dbPath)
            try client.connect()
            print("[swift-e2e] connect() ok")
            try client.subscribe(table: "tasks")
            print("[swift-e2e] subscribe(\"tasks\") ok — run loop driving replication")

            // Let the subscribe land + the session register with the fan-out
            // (the spine only delivers to sessions registered at fan-out
            // time). Mirrors the Rust template's 500ms settle.
            Thread.sleep(forTimeInterval: 0.5)

            // ---- direction 1: server PUSH → on-device query ----
            let pushBody = """
                {"pk":"swift-push","payload":{"title":"from-server","status":"open","priority":"5"}}
                """
            httpPush(port: port, body: pushBody)
            let pushSql = "SELECT pk FROM nostos_data WHERE table_name='tasks' AND pk='swift-push'"
            if pollQueryContains(client, sql: pushSql, needle: "swift-push", timeoutSeconds: 8) {
                print("[swift-e2e] PUSH_OK")
            } else {
                let rows = (try? client.query(sql: pushSql)) ?? "<query failed>"
                print("[swift-e2e] FAIL: swift-push never landed in nostos_data; rows=\(rows)")
                exit(1)
            }

            // ---- direction 2: SDK write → server echo → on-device query ----
            let echoPayload = "{\"title\":\"from-swift\",\"status\":\"open\",\"priority\":\"7\"}"
            let writeId = try client.write(
                table: "tasks",
                op: "upsert",
                pk: "swift-echo",
                payloadJson: echoPayload
            )
            print("[swift-e2e] write() id=\(writeId) (swift-echo enqueued)")
            let echoSql = "SELECT pk FROM nostos_data WHERE table_name='tasks' AND pk='swift-echo'"
            if pollQueryContains(client, sql: echoSql, needle: "swift-echo", timeoutSeconds: 8) {
                print("[swift-e2e] ECHO_OK")
            } else {
                let rows = (try? client.query(sql: echoSql)) ?? "<query failed>"
                print("[swift-e2e] FAIL: swift-echo never echoed back; rows=\(rows)")
                exit(1)
            }

            // ---- direction 3: setToken hot-swap (ADR-0029 #3) ----
            // setToken swaps the interior-mutable bearer WITHOUT tearing down the
            // live session — it's infallible locally (no I/O; the reconnect loop
            // reads the new token on its NEXT attempt). Proof at this layer: the
            // call returns AND the live session + its data stay usable (a query
            // still resolves with the row still present). Spine-side bearer
            // validation is a separate concern the e2e_server (anonymous by
            // default) doesn't exercise.
            client.setToken(token: "principal-B-bearer")
            guard (try client.query(sql: echoSql)).contains("swift-echo") else {
                print("[swift-e2e] FAIL: session/data unusable after setToken")
                exit(1)
            }
            print("[swift-e2e] SETTOKEN_OK (bearer hot-swapped; live session + data intact)")

            // ---- direction 4: signOut wipe (ADR-0029) ----
            // swift-echo (from direction 2) is the principal-A row on disk.
            // signOut() aborts the run loop, awaits quiescence, and wipes
            // nostos_data + checkpoint + epoch + outbox (the same clear_local_state
            // primitive every other SDK calls). To prove the wipe SURVIVES a
            // reopen WITHOUT the still-live spine refilling it, reopen the SAME
            // dbPath against ws://localhost:0 (no server: connect() starts the run
            // loop, which retries silently; query() reads the LOCAL store and
            // NOTHING re-replicates). This is the dead-endpoint trick the Rust
            // `clear_local_state_wipes_on_sign_out` test uses — the "B must not
            // see A's rows" guarantee signout_test.dart lands for Flutter.
            let aRowSql = "SELECT pk FROM nostos_data WHERE table_name='tasks' AND pk='swift-echo'"
            try client.signOut()
            print("[swift-e2e] SIGNOUT_OK (run loop aborted + local state wiped)")

            let clientB = try NostosClient(url: "ws://localhost:0/sync", token: nil, dbPath: dbPath)
            // Dead endpoint: no spine to refill from. connect() lifts the query()
            // connect-gate (it returns Ok — the run loop retries the unreachable
            // host in the background); the local wiped store is then readable.
            try clientB.connect()
            let rowsAfter = try clientB.query(sql: aRowSql)
            if rowsAfter.contains("swift-echo") {
                print("[swift-e2e] FAIL: principal-A row survived signOut — WIPE BROKEN; rows=\(rowsAfter)")
                exit(1)
            }
            print("[swift-e2e] SIGNOUT_WIPE_OK (principal-B reopen sees no A row)")

            print("[swift-e2e] SUCCESS")
        } catch {
            print("[swift-e2e] FAIL: \(error)")
            exit(1)
        }

        // Terminate so simctl launch returns and the harness captures stdout.
        // Session::Drop aborts the spawned run_with_reconnect task; exit(0)
        // rips the rest.
        exit(0)
    }
}

// UIApplicationMain shims — keep the file self-contained (no @main attribute,
// which SwiftUI's App protocol would otherwise drive).
_ = UIApplicationMain(
    CommandLine.argc,
    CommandLine.unsafeArgv,
    nil,
    NSStringFromClass(SmokeAppDelegate.self)
)
