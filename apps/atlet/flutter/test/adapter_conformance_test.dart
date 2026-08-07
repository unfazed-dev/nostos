import 'dart:async';
import 'package:flutter_test/flutter_test.dart';
import 'package:atlet/adapters/sync_adapter.dart';
import 'package:atlet/bench/marks.dart';
import 'support/fake_cart_orders.dart';

class FakeAdapter with FakeCartOrdersDefaults implements SyncAdapter {
  late MarkDeriver _markDeriver;
  late Stopwatch _clock;
  late StreamController<List<SessionRow>> _sessionController;
  late StreamController<List<ProductRow>> _productController;
  late StreamController<bool> _connectedController;
  late StreamController<SyncMark> _marksController;

  final Map<String, SessionRow> _sessions = {};
  final Map<String, ProductRow> _products = {};
  bool _isConnected = true;
  Duration _ackDelay = const Duration(milliseconds: 100);

  @override
  String get engine => 'cairn';

  @override
  Future<void> init({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  }) async {
    _clock = Stopwatch()..start();
    _markDeriver = MarkDeriver(_clock);
    _sessionController = StreamController<List<SessionRow>>.broadcast();
    _productController = StreamController<List<ProductRow>>.broadcast();
    _connectedController = StreamController<bool>.broadcast();
    _marksController = StreamController<SyncMark>.broadcast();

    // forward mark deriver emissions through marks controller
    _markDeriver.marks.listen((mark) {
      _marksController.add(mark);
    });

    // broadcast watched sessions to mark deriver
    _sessionController.stream.listen((rows) {
      _markDeriver.onEmission(rows);
    });

    _connectedController.add(_isConnected);
  }

  @override
  Future<void> signOut() async {
    _sessions.clear();
    _products.clear();
    _isConnected = true;
    _markDeriver.reset();
    // Keep controllers alive for potential reuse
  }

  @override
  Future<String> addSession(SessionRow s) async {
    assert(s.serverCommittedAt == null, 'serverCommittedAt must be null');
    final id = s.id;
    _sessions[id] = s;
    _markDeriver.localIds.add(id);

    // Emit locally-visible version immediately
    _sessionController.add(List<SessionRow>.from(_sessions.values));

    // Simulate server ack after delay
    if (_isConnected) {
      await Future<void>.delayed(_ackDelay);
      if (_sessions.containsKey(id)) {
        final withAck = SessionRow(
          id: s.id,
          title: s.title,
          type: s.type,
          metric: s.metric,
          unit: s.unit,
          note: s.note,
          streak: s.streak,
          occurredOn: s.occurredOn,
          serverCommittedAt: DateTime.now(),
        );
        _sessions[id] = withAck;
        _sessionController.add(List<SessionRow>.from(_sessions.values));
      }
    }

    return id;
  }

  @override
  Future<void> deleteSession(String id) async {
    _sessions.remove(id);
    _markDeriver.localIds.remove(id);
    _sessionController.add(List<SessionRow>.from(_sessions.values));
  }

  @override
  Stream<List<SessionRow>> watchSessions() {
    return _sessionController.stream;
  }

  @override
  Stream<List<ProductRow>> watchProducts() {
    return _productController.stream;
  }

  @override
  Stream<bool> get connected {
    return _connectedController.stream;
  }

  @override
  Future<void> setConnected(bool up) async {
    _isConnected = up;
    _connectedController.add(up);
  }

  @override
  Stream<SyncMark> get marks {
    return _marksController.stream;
  }

  /// Internal method for tests: inject remote rows
  void injectRemoteSession(SessionRow s) {
    assert(s.serverCommittedAt != null, 'serverCommittedAt must be non-null');
    _sessions[s.id] = s;
    _sessionController.add(List<SessionRow>.from(_sessions.values));
  }

  /// Internal method for tests: set ack delay
  void setAckDelay(Duration delay) {
    _ackDelay = delay;
  }

  void dispose() {
    _sessionController.close();
    _productController.close();
    _connectedController.close();
    _marksController.close();
    _markDeriver.dispose();
  }
}

void main() {
  group('SyncAdapter conformance', () {
    late FakeAdapter adapter;

    setUp(() async {
      adapter = FakeAdapter();
      await adapter.init(
        supabaseUrl: 'http://localhost:3000',
        accessToken: 'test-token',
        userId: 'test-user',
        dbDir: '/tmp/test-db',
      );
    });

    tearDown(() {
      adapter.dispose();
    });

    test('localVisible→serverAcked ordering per row', () async {
      final marks = <SyncMark>[];
      adapter.marks.listen((mark) {
        marks.add(mark);
      });

      // Add a session
      final sessionId = await adapter.addSession(SessionRow(
        id: 'session-1',
        title: 'Morning Run',
        type: 'cardio',
        metric: 5000,
        unit: 'm',
        occurredOn: DateTime.now(),
      ));

      // Wait for ack
      await Future<void>.delayed(const Duration(milliseconds: 200));

      // Verify ordering: localVisible comes before serverAcked
      expect(marks.length, greaterThanOrEqualTo(2));
      expect(marks[0].kind, MarkKind.localVisible);
      expect(marks[0].rowId, sessionId);
      expect(marks[1].kind, MarkKind.serverAcked);
      expect(marks[1].rowId, sessionId);
    });

    test('remoteVisible for externally-injected rows', () async {
      final marks = <SyncMark>[];
      adapter.marks.listen((mark) {
        marks.add(mark);
      });

      // Inject a remote row (not added via addSession)
      adapter.injectRemoteSession(SessionRow(
        id: 'remote-session-1',
        title: 'Evening Walk',
        type: 'cardio',
        metric: 3000,
        unit: 'm',
        occurredOn: DateTime.now(),
        serverCommittedAt: DateTime.now(),
      ));

      // Wait a bit for mark emission
      await Future<void>.delayed(const Duration(milliseconds: 50));

      // Verify remoteVisible mark
      expect(
        marks.where((m) => m.kind == MarkKind.remoteVisible && m.rowId == 'remote-session-1'),
        isNotEmpty,
      );
      expect(
        marks.where((m) => m.kind == MarkKind.serverAcked && m.rowId == 'remote-session-1'),
        isEmpty,
      );
    });

    test('signOut resets derivation state', () async {
      final marks = <SyncMark>[];
      adapter.marks.listen((mark) {
        marks.add(mark);
      });

      // Add a session
      await adapter.addSession(SessionRow(
        id: 'session-1',
        title: 'Run',
        type: 'cardio',
        metric: 5000,
        unit: 'm',
        occurredOn: DateTime.now(),
      ));

      await Future<void>.delayed(const Duration(milliseconds: 150));
      final marksBefore = marks.length;

      // Sign out
      await adapter.signOut();

      // Add same ID again
      await adapter.addSession(SessionRow(
        id: 'session-1',
        title: 'Run 2',
        type: 'cardio',
        metric: 6000,
        unit: 'm',
        occurredOn: DateTime.now(),
      ));

      await Future<void>.delayed(const Duration(milliseconds: 150));

      // Verify we get localVisible mark again for the same ID
      final newMarks = marks.skip(marksBefore).toList();
      expect(
        newMarks.where((m) => m.kind == MarkKind.localVisible && m.rowId == 'session-1'),
        isNotEmpty,
      );
    });
  });
}
