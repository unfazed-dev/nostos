/// The platform-neutral Worker transport seam for [WebNostosEngine] (ADR-0036).
///
/// [WebNostosEngine] drives the shared `nostos-ffi-wasm` backend through a
/// durable-storage Worker (OPFS is Worker-only by spec). To keep the engine's
/// protocol logic — request/response correlation, multi-table stream fan-out,
/// connection-state synthesis — unit-testable in the plain Dart VM (where
/// `dart:js_interop` is unavailable), the Worker boundary is abstracted behind
/// this pure-Dart [NostosWorkerPort]. The real JS Worker adapter
/// (`web_worker_port.dart`) implements this interface via `dart:js_interop`;
/// tests inject a [FakeNostosWorkerPort].
library;

import 'dart:async';

/// A bidirectional message channel to the nostos Worker.
///
/// Messages are plain `Map<String, Object?>` (JSON-serializable) in both
/// directions — the JS adapter `jsify`s outbound and `dartify`s inbound.
abstract class NostosWorkerPort {
  /// Send a request/event to the Worker.
  void send(Map<String, Object?> msg);

  /// Inbound messages from the Worker (responses + unsolicited pushes).
  Stream<Map<String, Object?>> get messages;

  /// Tear down the Worker (no further messages arrive).
  void terminate();
}

/// A pure-Dart in-process [NostosWorkerPort] for tests.
///
/// Call [receive] to read what the engine sent and [reply] to push a message
/// back. This lets a VM test drive [WebNostosEngine]'s full wiring (subscribe
/// state synthesis, per-table watch fan-out, request/response correlation)
/// without a browser or the wasm artifact.
class FakeNostosWorkerPort implements NostosWorkerPort {
  final List<Map<String, Object?>> sent = [];
  final _controller = StreamController<Map<String, Object?>>.broadcast();

  @override
  void send(Map<String, Object?> msg) => sent.add(msg);

  @override
  Stream<Map<String, Object?>> get messages => _controller.stream;

  /// Push an inbound message (a Worker response or push) to the engine.
  void reply(Map<String, Object?> msg) => _controller.add(msg);

  @override
  void terminate() => _controller.close();
}
