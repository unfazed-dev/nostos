// T6 attachment driver — round-trip + dead-letter + sign-out wipe tests
// (ADR-0034 / contract T6).
//
// Pure-Dart: injects a fake NostosEngine (no native library) that models the
// `attachments` metadata table in memory. The driver's pump→getAll→write cycle
// runs against that model end-to-end, and a shared in-memory fake adapter
// stands in for Supabase Storage so we prove the full queue→reconnect→upload→
// second-client-download path WITHOUT a live server or bucket. The real
// Supabase-Storage round-trip is the headline proof and is called out as
// untested-environment in the wave report (no project configured here).
//
// What is NOT exercised here: the metadata plane's actual replication between
// two clients (the existing sdk-e2e + Wave-1 suite cover replication); this
// test simulates the second client's metadata arrival by writing the row into
// the second fake engine directly.

import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:nostos_flutter/src/attachments.dart';
import 'package:nostos_flutter/src/nostos.dart';
import 'package:nostos_flutter/src/nostos_database.dart';
import 'package:nostos_flutter/src/engine.dart';
import 'package:nostos_flutter/src/schema.dart';
import 'package:flutter_test/flutter_test.dart';

/// Fake NostosEngine that models ONLY the `attachments` table in memory.
/// `write(upsert|patch|delete)` mutates the model; `query` answers the two SQL
/// shapes the driver emits (the queued-rows SELECT and the single-state SELECT).
/// Every other NostosEngine method is a harmless stub.
class _AttachFakeEngine implements NostosEngine {
  @override
  Stream<bool> get webStorageDegraded => const Stream<bool>.empty();
  final Map<String, Map<String, dynamic>> attachments = {};

  final rowsController = StreamController<String>.broadcast();
  final stateController = StreamController<NostosConnectionState>.broadcast();
  final writeStatusController =
      StreamController<
        ({int pending, int deadLettered, String? lastError})
      >.broadcast();

  @override
  Future<String> query({required String sql}) async {
    if (sql.contains('state IN')) {
      // Pump: queued rows.
      const queued = {'queued_upload', 'queued_download', 'queued_delete'};
      final out = attachments.values
          .where((r) => queued.contains(r['state']))
          .map(
            (r) => {
              'id': r['id'],
              'state': r['state'],
              'media_type': r['media_type'] ?? '',
              'filename': r['filename'] ?? '',
            },
          )
          .toList();
      return jsonEncode(out);
    }
    if (sql.contains('id =')) {
      // currentState read: `... WHERE id = '<id>' LIMIT 1`.
      final m = RegExp(r"id = '([^']+)'").firstMatch(sql);
      final id = m?.group(1) ?? '';
      final row = attachments[id];
      if (row == null) return '[]';
      return jsonEncode([
        {'state': row['state']},
      ]);
    }
    return '[]';
  }

  @override
  @override
  Future<String> subscribeStream({
    required String name,
    required String paramsJson,
  }) async => 'fake-stream';

  @override
  Future<void> unsubscribeStream({required String id}) async {}

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async {
    // Only the attachments table is modelled; ignore other tables.
    if (table != 'attachments') return 1;
    switch (op) {
      case 'upsert':
        final payload = payloadJson == null
            ? <String, dynamic>{}
            : jsonDecode(payloadJson);
        attachments[pk] = Map<String, dynamic>.from(payload);
      case 'patch':
        final payload = payloadJson == null
            ? <String, dynamic>{}
            : jsonDecode(payloadJson);
        attachments[pk] = {...?attachments[pk], ...payload};
      case 'delete':
        attachments.remove(pk);
    }
    return 1;
  }

  // ──────────────────── unused-but-required NostosEngine surface ────────────────────
  @override
  Stream<NostosConnectionState> subscribe({
    required List<NostosTableSub> tables,
    Set<String> orSetTables = const <String>{},
    Set<String> counterTables = const <String>{},
  }) => stateController.stream;
  @override
  Stream<String> watch({required String table}) => rowsController.stream;
  @override
  Stream<({int pending, int deadLettered, String? lastError})>
  watchWriteStatus() => writeStatusController.stream;
  @override
  Future<List<int>> writeBatch({
    required List<({String table, String op, String pk, String? payloadJson})>
    ops,
  }) async => List.generate(ops.length, (i) => i + 1);
  @override
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) async => 1;
  @override
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) async => 1;
  @override
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  }) async => 1;
  @override
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) async => 1;
  @override
  void applySchema(List<ClientTableFfi> tables) {}
  @override
  Future<void> setToken(String? token) async {}
  @override
  Future<void> disconnect() async {}
  @override
  Stream<NostosConnectionState> resume() => stateController.stream;
  @override
  Future<void> close() async {
    await rowsController.close();
    await stateController.close();
    await writeStatusController.close();
  }

  int signOutCalls = 0;
  @override
  Future<void> signOut() async {
    signOutCalls++;
  }
}

/// In-memory local blob store.
class _MemBlobStore implements BlobStore {
  final Map<String, Uint8List> _data = {};
  int wipeCalls = 0;
  @override
  Future<void> put(String id, Uint8List bytes) async => _data[id] = bytes;
  @override
  Future<Uint8List?> get(String id) async => _data[id];
  @override
  Future<void> remove(String id) async => _data.remove(id);
  @override
  Future<void> wipe() async {
    wipeCalls++;
    _data.clear();
  }

  bool has(String id) => _data.containsKey(id);
}

/// Fake remote adapter — stands in for Supabase Storage. Shared between two
/// clients to prove cross-client transfer through a common bucket.
class _FakeAdapter implements AttachmentStorageAdapter {
  final Map<String, Uint8List> remote = {};
  int uploadCalls = 0;
  int downloadCalls = 0;
  int deleteCalls = 0;

  /// When non-null, every upload throws this (simulates a down bucket). Clear
  /// it to let uploads succeed again.
  Object? uploadError;

  @override
  Future<void> upload(String path, Uint8List bytes, String mediaType) async {
    uploadCalls++;
    if (uploadError != null) throw uploadError!;
    remote[path] = bytes;
  }

  @override
  Future<Uint8List> download(String path) async {
    downloadCalls++;
    final b = remote[path];
    if (b == null) throw StateError('not found: $path');
    return b;
  }

  @override
  Future<void> delete(String path) async {
    deleteCalls++;
    remote.remove(path); // idempotent
  }
}

NostosDatabase _newDb(_AttachFakeEngine engine) => NostosDatabase.forTest(
  Nostos.withEngine(engine),
  const NostosSchema(tables: []),
);

/// Build a db with `attachments` subscribed (the write-path guard requires an
/// active subscription — same as the facade tests subscribing to `todos`).
Future<NostosDatabase> _newSubscribedDb(_AttachFakeEngine engine) async {
  final db = _newDb(engine);
  await db.subscribe('attachments');
  return db;
}

void main() {
  test(
    'queueUpload caches bytes locally and enqueues a queued_upload row',
    () async {
      final engine = _AttachFakeEngine();
      final db = await _newSubscribedDb(engine);
      final blob = _MemBlobStore();
      final adapter = _FakeAdapter();
      final driver = Attachments(
        db: db,
        adapter: adapter,
        blobStore: blob,
        isOnline: () async => true,
      );

      final id = await driver.queueUpload(
        filename: 'photo.png',
        bytes: Uint8List.fromList([1, 2, 3, 4]),
        mediaType: 'image/png',
      );

      expect(blob.has(id), isTrue); // bytes cached locally at once
      expect(engine.attachments[id]?['state'], 'queued_upload');
      expect(engine.attachments[id]?['size'], 4);
      expect(engine.attachments[id]?['media_type'], 'image/png');
      expect(adapter.uploadCalls, 0); // not yet pumped
    },
  );

  test('pump uploads on reconnect and flips state to synced', () async {
    final engine = _AttachFakeEngine();
    final db = await _newSubscribedDb(engine);
    final blob = _MemBlobStore();
    final adapter = _FakeAdapter();
    final driver = Attachments(
      db: db,
      adapter: adapter,
      blobStore: blob,
      isOnline: () async => true,
    );

    final id = await driver.queueUpload(
      filename: 'a.bin',
      bytes: Uint8List.fromList([9, 9]),
      mediaType: 'application/octet-stream',
    );
    await driver.pump();

    expect(adapter.uploadCalls, 1);
    expect(
      adapter.remote[id],
      Uint8List.fromList([9, 9]),
    ); // bytes reached the bucket
    expect(engine.attachments[id]?['state'], 'synced');
    expect(driver.lastErrorFor(id), isNull);
  });

  test('second client downloads the blob through the shared bucket '
      '(offline/local-adapter round-trip)', () async {
    // Client A uploads.
    final engineA = _AttachFakeEngine();
    final dbA = await _newSubscribedDb(engineA);
    final blobA = _MemBlobStore();
    final adapter = _FakeAdapter(); // shared "remote bucket"
    final driverA = Attachments(
      db: dbA,
      adapter: adapter,
      blobStore: blobA,
      isOnline: () async => true,
    );
    final id = await driverA.queueUpload(
      filename: 'shared.png',
      bytes: Uint8List.fromList([5, 6, 7]),
      mediaType: 'image/png',
    );
    await driverA.pump();
    expect(adapter.remote[id], Uint8List.fromList([5, 6, 7]));

    // Client B: simulate the synced metadata row arriving via replication by
    // writing it into B's engine directly (the metadata plane is the existing
    // replication path, covered by the sdk-e2e suite).
    final engineB = _AttachFakeEngine();
    final dbB = await _newSubscribedDb(engineB);
    final blobB = _MemBlobStore();
    final driverB = Attachments(
      db: dbB,
      adapter: adapter, // same bucket
      blobStore: blobB,
      isOnline: () async => true,
    );
    engineB.attachments[id] = {
      'id': id,
      'state': 'synced',
      'filename': 'shared.png',
      'media_type': 'image/png',
      'size': 3,
    };
    await driverB.queueDownload(id);
    expect(engineB.attachments[id]?['state'], 'queued_download');
    await driverB.pump();

    expect(adapter.downloadCalls, 1);
    expect(blobB.has(id), isTrue); // bytes landed on client B
    expect(blobB.get(id), completion(Uint8List.fromList([5, 6, 7])));
    expect(engineB.attachments[id]?['state'], 'synced');
  });

  test(
    'adapter failure retries with backoff then dead-letters to archived',
    () async {
      final engine = _AttachFakeEngine();
      final db = await _newSubscribedDb(engine);
      final blob = _MemBlobStore();
      final adapter = _FakeAdapter()
        ..uploadError = Exception('bucket unreachable');
      var now = DateTime(2026, 1, 1);
      final driver = Attachments(
        db: db,
        adapter: adapter,
        blobStore: blob,
        isOnline: () async => true,
        maxAttempts: 2,
        clock: () => now,
      );

      final id = await driver.queueUpload(
        filename: 'failing.bin',
        bytes: Uint8List.fromList([1]),
        mediaType: 'application/octet-stream',
      );

      // Attempt 1: fails, schedules backoff.
      await driver.pump();
      expect(adapter.uploadCalls, 1);
      expect(engine.attachments[id]?['state'], 'queued_upload'); // still queued
      expect(driver.lastErrorFor(id), contains('bucket unreachable'));

      // Same instant: backoff not elapsed → skipped.
      await driver.pump();
      expect(adapter.uploadCalls, 1);

      // After the backoff window: attempt 2 → fail again → dead-letter (max=2).
      now = now.add(const Duration(seconds: 3));
      await driver.pump();
      expect(adapter.uploadCalls, 2);
      expect(engine.attachments[id]?['state'], 'archived');
      expect(driver.lastErrorFor(id), contains('bucket unreachable'));
    },
  );

  test('delete archives the blob in the remote bucket', () async {
    final engine = _AttachFakeEngine();
    final db = await _newSubscribedDb(engine);
    final blob = _MemBlobStore();
    final adapter = _FakeAdapter();
    final driver = Attachments(
      db: db,
      adapter: adapter,
      blobStore: blob,
      isOnline: () async => true,
    );
    final id = await driver.queueUpload(
      filename: 'gone.bin',
      bytes: Uint8List.fromList([1]),
      mediaType: 'application/octet-stream',
    );
    await driver.pump(); // upload + synced
    expect(adapter.remote.containsKey(id), isTrue);

    await driver.remove(id); // queue delete
    expect(engine.attachments[id]?['state'], 'queued_delete');
    await driver.pump(); // adapter.delete → archived (tombstone)
    expect(adapter.deleteCalls, 1);
    expect(adapter.remote.containsKey(id), isFalse);
    expect(engine.attachments[id]?['state'], 'archived');
  });

  test('driver does not dispatch while offline', () async {
    final engine = _AttachFakeEngine();
    final db = await _newSubscribedDb(engine);
    final blob = _MemBlobStore();
    final adapter = _FakeAdapter();
    var online = false;
    final driver = Attachments(
      db: db,
      adapter: adapter,
      blobStore: blob,
      isOnline: () async => online,
    );
    final id = await driver.queueUpload(
      filename: 'offline.bin',
      bytes: Uint8List.fromList([1]),
      mediaType: 'application/octet-stream',
    );
    await driver.pump(); // offline → no-op
    expect(adapter.uploadCalls, 0);
    expect(engine.attachments[id]?['state'], 'queued_upload');

    online = true; // reconnect
    await driver.pump();
    expect(adapter.uploadCalls, 1);
    expect(engine.attachments[id]?['state'], 'synced');
  });

  test('signOut wipes the local blob store (ADR-0029 / ADR-0034)', () async {
    final engine = _AttachFakeEngine();
    final db = await _newSubscribedDb(engine);
    final blob = _MemBlobStore();
    final adapter = _FakeAdapter();
    // Build via the extension so the signOut hook is registered.
    final driver = db.attachments(adapter: adapter, blobStore: blob);
    final id = await driver.queueUpload(
      filename: 'wipe.bin',
      bytes: Uint8List.fromList([1]),
      mediaType: 'application/octet-stream',
    );
    expect(blob.has(id), isTrue);

    await db.signOut();

    expect(blob.wipeCalls, 1); // blobs gone — next principal sees nothing
    expect(engine.signOutCalls, 1); // core signOut ran too
  });

  test('keepLocalOnSignOut keeps the blob store too (ADR-0049)', () async {
    final engine = _AttachFakeEngine();
    final db = NostosDatabase.forTest(
      Nostos.withEngine(engine),
      const NostosSchema(tables: []),
      keepLocalOnSignOut: true,
    );
    await db.subscribe('attachments');
    final blob = _MemBlobStore();
    final driver = db.attachments(adapter: _FakeAdapter(), blobStore: blob);
    final id = await driver.queueUpload(
      filename: 'keep.bin',
      bytes: Uint8List.fromList([1]),
      mediaType: 'application/octet-stream',
    );

    await db.signOut();

    expect(blob.wipeCalls, 0);
    expect(blob.has(id), isTrue); // same principal returns to its cache
    expect(engine.signOutCalls, 1);
  });

  test('bytes() reads through the cache without touching metadata', () async {
    final engine = _AttachFakeEngine();
    final db = await _newSubscribedDb(engine);
    final blob = _MemBlobStore();
    final adapter = _FakeAdapter()
      ..remote['shared.jpg'] = Uint8List.fromList([9, 9]);
    var online = false;
    final driver = Attachments(
      db: db,
      adapter: adapter,
      blobStore: blob,
      isOnline: () async => online,
    );

    expect(await driver.bytes('shared.jpg'), isNull); // offline, not cached
    online = true;
    expect(await driver.bytes('shared.jpg'), [9, 9]);
    expect(await driver.bytes('shared.jpg'), [9, 9]);
    expect(adapter.downloadCalls, 1); // second read served from the store
    expect(engine.attachments, isEmpty); // no state flip through the outbox
    expect(await driver.bytes('missing.jpg'), isNull);
    expect(driver.lastErrorFor('missing.jpg'), contains('not found'));
  });
}
