class SessionRow {
  final String id;
  final String title;
  final String type;
  final int metric;
  final String unit;
  final String? note;
  final int streak;
  final DateTime occurredOn;
  final DateTime? serverCommittedAt;

  const SessionRow({
    required this.id,
    required this.title,
    required this.type,
    required this.metric,
    required this.unit,
    this.note,
    this.streak = 0,
    required this.occurredOn,
    this.serverCommittedAt,
  });
}

class ProductRow {
  final String id;
  final String name;
  final String category;
  final int priceCents;
  final double? rating;
  final bool plantBased;
  final String? imageUrl;

  const ProductRow({
    required this.id,
    required this.name,
    required this.category,
    required this.priceCents,
    this.rating,
    required this.plantBased,
    this.imageUrl,
  });
}

enum MarkKind { localVisible, serverAcked, remoteVisible }

class SyncMark {
  final MarkKind kind;
  final String rowId;
  final Duration tMono; // from bench clock
  final DateTime? serverCommittedAt;

  const SyncMark(
    this.kind,
    this.rowId,
    this.tMono, {
    this.serverCommittedAt,
  });
}

abstract interface class SyncAdapter {
  String get engine; // 'cairn' | 'powersync'
  Future<void> init({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  });
  Future<void> signOut(); // disconnect + delete local DB files (full wipe)
  Future<String> addSession(SessionRow s); // serverCommittedAt must be null
  Future<void> deleteSession(String id);
  Stream<List<SessionRow>> watchSessions();
  Stream<List<ProductRow>> watchProducts();
  Stream<bool> get connected;
  Future<void> setConnected(bool up);
  Stream<SyncMark> get marks; // derived ONLY from watchSessions output
}
