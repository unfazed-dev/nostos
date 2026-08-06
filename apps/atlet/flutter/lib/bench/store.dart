import 'dart:convert';
import 'dart:io';

import 'package:path_provider/path_provider.dart';

import 'runner.dart';

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
  }) : file = File('${directory.path}/$fileName');

  final File file;

  static Future<BenchStore> openAppDocuments({
    String fileName = 'atlet_runs.jsonl',
  }) async {
    final dir = await getApplicationDocumentsDirectory();
    return BenchStore(directory: dir, fileName: fileName);
  }

  Future<void> append(RunRecord record) async {
    await file.writeAsString(
      '${jsonEncode(record.toJson())}\n',
      mode: FileMode.append,
      flush: true,
    );
  }

  Future<List<RunRecord>> readAll() async {
    if (!await file.exists()) return const [];
    final lines = await file.readAsLines();
    return lines
        .where((line) => line.trim().isNotEmpty)
        .map(
          (line) => RunRecord.fromJson(jsonDecode(line) as Map<String, dynamic>),
        )
        .toList();
  }
}
