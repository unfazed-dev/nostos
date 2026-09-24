// ADR-0029 D3 — Flutter sign-out wipe proof. Mirrors the kotlin/swift
// `sign_out_wipes_local_state_so_next_user_sees_nothing` test: user A writes +
// subscribes against a temp FILE store, signs out; user B reopens the SAME file
// and must not see A's row. A `:memory:` store would hide the wipe (a fresh DB
// per connect), so this uses a temp file — only `clear_local_state()` (run
// inside `signOut`) empties a file that persists across connects.
//
// No server is required: `ws://localhost:0` fails to connect, but local
// `write()` renders the row instantly (optimistic `apply_local`) and the
// run loop's failed reconnects do not affect the on-disk store.
import 'dart:io';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test(
    'signOut wipes local state so the next principal sees nothing (ADR-0029)',
    () async {
      final db =
          '${Directory.systemTemp.path}/nostos_flutter_signout_test.sqlite';
      final dbFile = File(db);
      if (dbFile.existsSync()) {
        dbFile.deleteSync();
      }

      // User A: connect, subscribe, seed a row that lands in the durable store.
      final a = await Nostos.connect(
        url: 'ws://localhost:0',
        token: 'token-a',
        sqlitePath: db,
      );
      await a.subscribe('tasks');
      await a.write(
        'tasks',
        op: 'upsert',
        pk: 'pk1',
        payload: {'title': 'seed'},
      );
      final before = await a.query('SELECT pk FROM nostos_data');
      expect(
        before,
        contains('pk1'),
        reason: 'seeded row must be present before signOut',
      );

      // signOut must return promptly (abort+await quiesce) and wipe the file.
      await a.signOut();

      // User B reopens the SAME file: A's row must not survive signOut.
      final b = await Nostos.connect(
        url: 'ws://localhost:0',
        token: 'token-b',
        sqlitePath: db,
      );
      await b.subscribe('tasks');
      final after = await b.query('SELECT pk FROM nostos_data');
      expect(
        after,
        isNot(contains('pk1')),
        reason: 'prior principal row must not survive signOut',
      );
      await b.close();

      if (dbFile.existsSync()) {
        dbFile.deleteSync();
      }
    },
  );

  test(
    'signOut with no active subscription is an idempotent no-op (ADR-0029)',
    () async {
      final c = await Nostos.connect(
        url: 'ws://localhost:0',
        token: 't',
        sqlitePath: ':memory:',
      );
      await c.signOut(); // must not throw
    },
  );
}
