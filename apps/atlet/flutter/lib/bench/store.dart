import 'dart:convert';
import 'dart:io';

import 'package:flutter/foundation.dart' show kIsWeb;
import 'package:path_provider/path_provider.dart';

import 'runner.dart';
import 'web_runs_stub.dart' if (dart.library.js_interop) 'web_runs_web.dart';

/// Neutral run-result store: newline-delimited JSON (JSONL) under a
/// directory (app documents dir in production, an injectable [Directory] in
/// tests).
///
/// ponytail: no DB for the neutral store, a file is enough — ceiling is
/// O(n) full-file scans on read and no concurrent-writer safety; upgrade to
/// sqlite (already a transitive dep via nostos_flutter) if run counts or
/// concurrent writers grow enough to matter.
class BenchStore {
  BenchStore({
    required Directory directory,
    String fileName = 'atlet_runs.jsonl',
  }) : file = File('${directory.path}/$fileName'),
       _memory = null;

  /// Web: path_provider has no browser implementation (the open future never
  /// resolves — the Analytics tab spun forever) and dart:io File throws there,
  /// so runs persist to localStorage as one JSONL string. ponytail: ~5MB
  /// localStorage cap and string-join-per-append; OPFS/IndexedDB if run
  /// history ever outgrows that.
  BenchStore.web() : file = null, _memory = _loadWebRuns();

  static const _webKey = 'atlet_runs.jsonl';

  static List<String> _loadWebRuns() {
    final raw = loadWebRuns(_webKey);
    if (raw == null || raw.isEmpty) return <String>[];
    return raw.split('\n');
  }

  final File? file;
  final List<String>? _memory;

  static Future<BenchStore> openAppDocuments({
    String fileName = 'atlet_runs.jsonl',
  }) async {
    if (kIsWeb) return BenchStore.web();
    final dir = await getApplicationDocumentsDirectory();
    return BenchStore(directory: dir, fileName: fileName);
  }

  Future<void> append(RunRecord record) async {
    final line = '${jsonEncode(record.toJson())}\n';
    final f = file;
    if (f == null) {
      _memory!.add(line);
      saveWebRuns(_webKey, _memory.join('\n'));
      return;
    }
    await f.writeAsString(line, mode: FileMode.append, flush: true);
  }

  Future<List<RunRecord>> readAll() async {
    final List<String> lines;
    final f = file;
    if (f == null) {
      lines = List.of(_memory!);
    } else {
      if (!await f.exists()) return const [];
      lines = await f.readAsLines();
    }
    return lines
        .where((line) => line.trim().isNotEmpty)
        .map(
          (line) =>
              RunRecord.fromJson(jsonDecode(line) as Map<String, dynamic>),
        )
        .toList();
  }
}
