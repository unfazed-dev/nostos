import 'dart:async';
import '../adapters/sync_adapter.dart';

class MarkDeriver {
  final Stopwatch clock;
  final _seen = <String>{};
  final _acked = <String>{};
  final localIds = <String>{};
  final _out = StreamController<SyncMark>.broadcast();

  Stream<SyncMark> get marks => _out.stream;

  MarkDeriver(this.clock);

  void onEmission(List<SessionRow> rows) {
    final t = clock.elapsed;
    for (final r in rows) {
      if (_seen.add(r.id)) {
        if (localIds.contains(r.id)) {
          _out.add(SyncMark(MarkKind.localVisible, r.id, t));
        } else if (r.serverCommittedAt != null) {
          _out.add(SyncMark(MarkKind.remoteVisible, r.id, t,
              serverCommittedAt: r.serverCommittedAt));
        }
      }
      if (r.serverCommittedAt != null &&
          _acked.add(r.id) &&
          localIds.contains(r.id)) {
        _out.add(SyncMark(MarkKind.serverAcked, r.id, t,
            serverCommittedAt: r.serverCommittedAt));
      }
    }
  }

  void reset() {
    _seen.clear();
    _acked.clear();
    localIds.clear();
  }

  void dispose() {
    _out.close();
  }
}
