import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('NostosConfig.fromJson', () {
    test('parses a minimal config (url only) with defaults', () {
      final c = NostosConfig.fromJson({'url': 'ws://localhost:8800/sync'});
      expect(c.url, 'ws://localhost:8800/sync');
      expect(c.sqliteFilename, 'cairn.sqlite');
      expect(c.hasSupabase, isFalse);
    });

    test('parses a full supabase-cloud config', () {
      final c = NostosConfig.fromJson({
        'url': 'wss://nostos.example.com/sync',
        'supabase': {
          'url': 'https://xyz.supabase.co',
          'anon_key': 'eyJ.test',
        },
        'sqlite_filename': 'app.sqlite',
      });
      expect(c.hasSupabase, isTrue);
      expect(c.supabaseUrl, 'https://xyz.supabase.co');
      expect(c.supabaseAnonKey, 'eyJ.test');
      expect(c.sqliteFilename, 'app.sqlite');
    });

    test('accepts publishable_key as the anon_key successor spelling', () {
      final c = NostosConfig.fromJson({
        'url': 'wss://nostos.example.com/sync',
        'supabase': {
          'url': 'https://xyz.supabase.co',
          'publishable_key': 'sb_publishable_test',
        },
      });
      expect(c.hasSupabase, isTrue);
      expect(c.supabaseAnonKey, 'sb_publishable_test');
    });

    test('rejects a missing url with a pointed message', () {
      expect(
        () => NostosConfig.fromJson({}),
        throwsA(
          isA<FormatException>().having(
            (e) => e.message,
            'message',
            contains('"url" is required'),
          ),
        ),
      );
    });

    test('rejects a non-websocket url', () {
      expect(
        () => NostosConfig.fromJson({'url': 'https://nostos.example.com/sync'}),
        throwsA(
          isA<FormatException>().having(
            (e) => e.message,
            'message',
            contains('ws:// or wss://'),
          ),
        ),
      );
    });

    test('rejects a supabase block missing the key', () {
      expect(
        () => NostosConfig.fromJson({
          'url': 'ws://localhost:8800/sync',
          'supabase': {'url': 'https://xyz.supabase.co'},
        }),
        throwsA(isA<FormatException>()),
      );
    });
  });

  group('declared NostosSchema (app-side migrations)', () {
    test('NostosTable/NostosColumn declaration maps to the FFI mirror', () {
      const schema = NostosSchema(tables: [
        NostosTable(name: 'tasks', primaryKey: ['id'], columns: [
          NostosColumn.text('id'),
          NostosColumn.text('title'),
          NostosColumn.integer('completed'),
          NostosColumn.real('effort'),
        ]),
      ]);
      final ffi = schema.toClientTables();
      expect(ffi, hasLength(1));
      expect(ffi.single.name, 'tasks');
      expect(ffi.single.primaryKey, ['id']);
      expect(ffi.single.columns, ['id', 'title', 'completed', 'effort']);
      // Affinity is carried on the Dart side for typed records.
      expect(schema.tables.single.columns[2].affinity, 'INTEGER');
      expect(schema.tables.single.columns[3].affinity, 'REAL');
    });
  });
}
