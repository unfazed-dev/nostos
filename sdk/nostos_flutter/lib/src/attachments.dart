// T6 attachments — two-plane blob sync (ADR-0034 / contract T6).
//
// The metadata plane is an ordinary synced `attachments` table; the blob plane
// is a developer-supplied [AttachmentStorageAdapter]. Nostos drives WHEN blobs
// move (connectivity, retry, state machine); the adapter decides WHERE (the
// app's bucket). Blobs never transit the Nostos server (moat constraint).
//
// The pure state machine lives in `nostos-core` (`attachments.rs`); this file is
// the Flutter driver that binds those wire strings + transition rules and calls
// the adapter. See ADR-0034 for the design and the weaker-ordering choice.

import 'dart:async';
import 'dart:io';
import 'dart:math' as math show min;
import 'dart:typed_data';

import 'package:supabase_flutter/supabase_flutter.dart';

import 'nostos_database.dart';

/// Wire strings for the `attachments.state` column. Mirrors
/// `nostos_core::attachments::AttachmentState` (single source of truth — the
/// Rust crate's unit tests guard the round-trip). Duplicated shallowly here
/// because the state machine is pure data and crossing FFI for five string
/// constants would cost more than it pays (ADR-0034 §driver boundary).
class AttachmentStateWire {
  AttachmentStateWire._();
  static const queuedUpload = 'queued_upload';
  static const queuedDownload = 'queued_download';
  static const queuedDelete = 'queued_delete';
  static const synced = 'synced';
  static const archived = 'archived';

  /// All states the active driver considers actionable (a row in any of these
  /// is eligible for a blob op when connectivity allows).
  static const queued = <String>{queuedUpload, queuedDownload, queuedDelete};
}

/// The table name + column names the app MUST declare in its schema. Nostos
/// syncs this table like any other; it MUST be in the server's
/// `NOSTOS_WRITE_TABLES` allowlist (empty-default — ADR-0013; the #1 foot-gun).
class AttachmentSchema {
  AttachmentSchema._();
  static const table = 'attachments';
  static const colId = 'id';
  static const colFilename = 'filename';
  static const colSize = 'size';
  static const colMediaType = 'media_type';
  static const colState = 'state';
  static const colTimestamp = 'timestamp';
}

/// Remote blob storage — the developer's bucket. Implementations talk to a
/// storage backend (Supabase Storage, S3, …); Nostos never sees the bytes.
///
/// Methods MUST be idempotent under retry: the driver may call [upload] again
/// after a network blip, and [delete] on a path that's already gone MUST
/// succeed (return normally) so a `queued_delete` converges. Throwing marks
/// the attempt failed and feeds the backoff/dead-letter schedule.
abstract class AttachmentStorageAdapter {
  /// Upload [bytes] to [path], tagged with [mediaType]. Overwrites if the path
  /// already exists. MUST be idempotent under retry.
  Future<void> upload(String path, Uint8List bytes, String mediaType);

  /// Download the bytes at [path]. Throws if the path is absent at call time
  /// (the driver keeps the row `queued_download` and retries).
  Future<Uint8List> download(String path);

  /// Remove the blob at [path]. Idempotent: a missing path is success (so a
  /// `queued_delete` converges even if the blob was already removed).
  Future<void> delete(String path);
}

/// Local on-device blob cache. Holds bytes the app picked (pending upload) or
/// fetched (after download). Distinct from the [AttachmentStorageAdapter]
/// (which is the REMOTE bucket): a row is `synced` only after the adapter
/// confirms, but the bytes also live here for offline reads.
///
/// [wipe] is called on sign-out (ADR-0029) so the next principal sees no blob
/// bytes — consistent with the SQLite + outbox wipe.
abstract class BlobStore {
  Future<void> put(String id, Uint8List bytes);
  Future<Uint8List?> get(String id);
  Future<void> remove(String id);

  /// Remove every blob. Called on signOut. MUST be idempotent.
  Future<void> wipe();
}

/// A [BlobStore] backed by a directory on the local filesystem. The app
/// supplies the directory (normally via `path_provider`'s
/// `getApplicationSupportDirectory`); `nostos_flutter` deliberately does NOT
/// depend on `path_provider` so the package stays lean — pass the path in.
///
/// Each blob is one file named `<dir>/<id>`. [wipe] deletes the directory's
/// contents (not the directory itself).
class LocalFileBlobStore implements BlobStore {
  LocalFileBlobStore(this._dir);

  final Directory _dir;

  File _file(String id) => File('${_dir.path}/$id');

  @override
  Future<void> put(String id, Uint8List bytes) async {
    if (!_dir.existsSync()) {
      _dir.createSync(recursive: true);
    }
    await _file(id).writeAsBytes(bytes, flush: true);
  }

  @override
  Future<Uint8List?> get(String id) async {
    final f = _file(id);
    if (!f.existsSync()) return null;
    return f.readAsBytes();
  }

  @override
  Future<void> remove(String id) async {
    final f = _file(id);
    if (f.existsSync()) await f.delete();
  }

  @override
  Future<void> wipe() async {
    if (!_dir.existsSync()) return;
    await for (final entry in _dir.list()) {
      await entry.delete(recursive: true);
    }
  }
}

/// First-class [AttachmentStorageAdapter] for Supabase Storage. Uploads,
/// downloads, and deletes through `supabase.storage.from(bucket)`. The bucket
/// name + the path scheme are the app's choice (path = attachment id by
/// default, so the metadata row's `id` is the storage object key).
class SupabaseStorageAdapter implements AttachmentStorageAdapter {
  SupabaseStorageAdapter({required this.bucket, this.pathPrefix = ''});

  /// The Supabase Storage bucket name (created in the Supabase dashboard).
  final String bucket;

  /// Optional prefix prepended to every object key (e.g. `'attachments/'`).
  /// Empty means the attachment id IS the object key.
  final String pathPrefix;

  String _key(String path) => pathPrefix.isEmpty ? path : '$pathPrefix$path';

  @override
  Future<void> upload(String path, Uint8List bytes, String mediaType) async {
    // uploadBinary + upsert:true is idempotent under retry (the driver may
    // re-dispatch after a network blip). contentType sets the object's MIME.
    // (supabase-dart folds contentType into FileOptions, not a top-level param.)
    await Supabase.instance.client.storage
        .from(bucket)
        .uploadBinary(
          _key(path),
          bytes,
          fileOptions: FileOptions(contentType: mediaType, upsert: true),
        );
  }

  @override
  Future<Uint8List> download(String path) async =>
      Supabase.instance.client.storage.from(bucket).download(_key(path));

  @override
  Future<void> delete(String path) async {
    // Idempotent: a missing object is a 404 the app should treat as converged.
    // Supabase throws on not-found; swallow it so queued_delete converges.
    try {
      await Supabase.instance.client.storage.from(bucket).remove([_key(path)]);
    } on StorageException catch (_) {
      // Already gone — success per the idempotent contract.
    }
  }
}

/// One row of attachment metadata, decoded for the driver.
class AttachmentRow {
  AttachmentRow({
    required this.id,
    required this.state,
    required this.mediaType,
    required this.filename,
  });

  final String id;
  final String state;
  final String mediaType;
  final String filename;

  factory AttachmentRow.fromJson(Map<String, dynamic> json) => AttachmentRow(
    id: json[AttachmentSchema.colId]?.toString() ?? '',
    state: json[AttachmentSchema.colState]?.toString() ?? '',
    mediaType: json[AttachmentSchema.colMediaType]?.toString() ?? '',
    filename: json[AttachmentSchema.colFilename]?.toString() ?? '',
  );
}

/// The attachment manager + driver. Construct via [NostosDatabase.attachments].
///
/// Lifecycle:
/// - [queueUpload] / [queueDownload] / [remove] enqueue metadata writes through
///   the same durable outbox as any business write (so they survive a crash
///   and flush on reconnect). The blob bytes are cached in the [BlobStore].
/// - [pump] is the driver tick: when online, it reads queued metadata rows and
///   dispatches their blob ops to the adapter, flipping `state` on success or
///   dead-lettering after retries. Wire [pump] to a timer + the connection
///   stream (see [Attachments.start] for the default self-driving loop).
class Attachments {
  Attachments({
    required this._db,
    required this._adapter,
    required BlobStore blobStore,
    required this._isOnline,
    this._maxAttempts = 5,
    DateTime Function()? clock,
  }) : _blob = blobStore,
       _clock = clock ?? DateTime.now;

  final NostosDatabase _db;
  final AttachmentStorageAdapter _adapter;
  final BlobStore _blob;
  final Future<bool> Function() _isOnline;
  final int _maxAttempts;
  final DateTime Function() _clock;

  /// In-flight blob paths (ids being transferred right now) so a re-tick does
  /// not double-dispatch.
  final Set<String> _inFlight = <String>{};

  /// Per-id failed-attempt count + next-eligible timestamp for backoff.
  /// ponytail: in-memory only — a process restart resets attempts. Acceptable
  /// because the metadata row stays `queued_*` across a restart (its state is
  /// synced), so the driver simply retries fresh. Persisting attempts would be
  /// a local-only table; deferred until a measurement shows user-visible churn.
  final Map<String, int> _attempts = <String, int>{};
  final Map<String, DateTime> _nextEligible = <String, DateTime>{};

  /// The last adapter error surfaced for an attachment id (dead-letter reason).
  /// Cleared on a successful transition. Local-only — NOT synced.
  String? lastErrorFor(String id) => _lastErrors[id];
  final Map<String, String> _lastErrors = <String, String>{};

  Timer? _timer;
  bool _started = false;

  /// Self-driving loop: ticks every 2s while online and reacts to connectivity
  /// transitions (a reconnect pumps immediately). Idempotent. The app normally
  /// calls this once after [NostosDatabase.subscribe] includes `attachments`.
  void start() {
    if (_started) return;
    _started = true;
    _timer = Timer.periodic(const Duration(seconds: 2), (_) => pump());
  }

  /// Stop the self-driving loop (e.g. on [NostosDatabase.close]). Idempotent.
  void stop() {
    _timer?.cancel();
    _timer = null;
    _started = false;
  }

  // ──────────────────────────── public API ────────────────────────────

  /// Queue a blob for upload. Bytes are cached locally at once; a metadata row
  /// is enqueued with `state = queued_upload` through the durable outbox. The
  /// driver uploads on the next online [pump]. Returns the attachment id
  /// (also the blob's storage key).
  Future<String> queueUpload({
    required String filename,
    required Uint8List bytes,
    required String mediaType,
    String? id,
  }) async {
    final attachmentId = id ?? _newId();
    await _blob.put(attachmentId, bytes);
    await _db.write(
      table: AttachmentSchema.table,
      op: 'upsert',
      pk: attachmentId,
      payload: {
        AttachmentSchema.colId: attachmentId,
        AttachmentSchema.colFilename: filename,
        AttachmentSchema.colSize: bytes.length,
        AttachmentSchema.colMediaType: mediaType,
        AttachmentSchema.colState: AttachmentStateWire.queuedUpload,
        AttachmentSchema.colTimestamp: _clock().millisecondsSinceEpoch,
      },
    );
    return attachmentId;
  }

  /// Queue a download for an attachment whose metadata row already exists (its
  /// `state` is normally `synced` on a second client). Patches `state` to
  /// `queued_download`; the driver fetches bytes on the next online [pump].
  Future<void> queueDownload(String id) =>
      _transition(id, AttachmentStateWire.queuedDownload);

  /// Queue a blob for deletion from the remote bucket. The metadata row is
  /// retained as a tombstone (`archived` after the delete confirms).
  Future<void> remove(String id) =>
      _transition(id, AttachmentStateWire.queuedDelete);

  /// Bytes for an attachment, read-through: the local [BlobStore] first, else
  /// a direct adapter download that is cached and returned. `null` when the
  /// download fails (offline included — see [lastErrorFor]). No online gate:
  /// the first paint races the `connected` transition, and a gated miss is
  /// never retried while a failed attempt costs one request.
  ///
  /// Unlike [queueDownload] this never touches the metadata row, so it is the
  /// call for SHARED attachments (a public catalog's images): a state flip
  /// through the outbox would fan out to every device and needs write RLS the
  /// reader does not have. Per-user attachments keep using [queueDownload] so
  /// the transfer is durable across restarts.
  Future<Uint8List?> bytes(String id) async {
    final cached = await _blob.get(id);
    if (cached != null) return cached;
    try {
      final fetched = await _adapter.download(id);
      await _blob.put(id, fetched);
      return fetched;
    } on Object catch (e) {
      _lastErrors[id] = e.toString();
      return null;
    }
  }

  // ──────────────────────────── driver tick ───────────────────────────

  /// One driver tick. Reads queued rows (when online) and dispatches their blob
  /// ops. Safe to call repeatedly; no-op when offline. Exposed for tests + the
  /// self-driving [start] loop.
  Future<void> pump() async {
    if (!await _isOnline()) return;
    final List<Map<String, dynamic>> rows;
    try {
      rows = await _db.getAll(
        "SELECT ${AttachmentSchema.colId}, ${AttachmentSchema.colState}, "
        "${AttachmentSchema.colMediaType}, ${AttachmentSchema.colFilename} "
        "FROM ${AttachmentSchema.table} "
        "WHERE ${AttachmentSchema.colState} IN "
        "('${AttachmentStateWire.queuedUpload}','${AttachmentStateWire.queuedDownload}','${AttachmentStateWire.queuedDelete}')",
      );
    } on Object {
      // The table may not be subscribed yet, or the query shape drifted. The
      // next tick retries; surfacing this would noise the UI. ponytail: log
      // hook deferred.
      return;
    }
    final now = _clock();
    for (final json in rows) {
      final row = AttachmentRow.fromJson(json);
      if (_inFlight.contains(row.id)) continue;
      final eligible = _nextEligible[row.id];
      if (eligible != null && eligible.isAfter(now)) continue;
      await _dispatch(row);
    }
  }

  Future<void> _dispatch(AttachmentRow row) async {
    _inFlight.add(row.id);
    try {
      switch (row.state) {
        case AttachmentStateWire.queuedUpload:
          final bytes = await _blob.get(row.id);
          if (bytes == null) {
            // Bytes lost locally (app cleared cache, restored backup). We
            // cannot upload what we don't have — dead-letter immediately.
            await _fail(row.id, 'local blob missing for upload');
            return;
          }
          await _adapter.upload(row.id, bytes, row.mediaType);
          await _succeed(row.id);
        case AttachmentStateWire.queuedDownload:
          final bytes = await _adapter.download(row.id);
          await _blob.put(row.id, bytes);
          await _succeed(row.id);
        case AttachmentStateWire.queuedDelete:
          await _adapter.delete(row.id);
          await _succeed(row.id);
        default:
          break;
      }
    } on Object catch (e) {
      await _fail(row.id, e.toString());
    } finally {
      _inFlight.remove(row.id);
    }
  }

  Future<void> _succeed(String id) async {
    _attempts.remove(id);
    _lastErrors.remove(id);
    _nextEligible.remove(id);
    // Read the row's CURRENT state to resolve the success target (upload/
    // download → synced; delete → archived). We read because pump may have run
    // a second op; the state we dispatched from is the source of truth for the
    // target, but a concurrent patch could have flipped it. Re-reading is the
    // safe path. For the common case we patch to the on_success of the
    // dispatched state — approximated by reading current state.
    final cur = await _currentState(id);
    final target = _onSuccess(cur);
    await _transition(id, target);
  }

  Future<void> _fail(String id, String reason) async {
    _lastErrors[id] = reason;
    final attempts = (_attempts[id] ?? 0) + 1;
    _attempts[id] = attempts;
    if (attempts >= _maxAttempts) {
      // Dead-letter: archive the row (out of the active queue) and keep the
      // error surfaced locally. The app can revive it by patching state back.
      _attempts.remove(id);
      _nextEligible.remove(id);
      await _transition(id, AttachmentStateWire.archived);
      return;
    }
    // Exponential backoff: 2^attempts seconds, capped 60s. Mirrors
    // nostos_core::attachments::retry_after_ms.
    final shift = math.min(attempts, 6);
    final secs = (1 << shift).clamp(1, 60);
    _nextEligible[id] = _clock().add(Duration(seconds: secs));
  }

  // ──────────────────────────── helpers ──────────────────────────────

  Future<String> _currentState(String id) async {
    try {
      final rows = await _db.getAll(
        "SELECT ${AttachmentSchema.colState} FROM ${AttachmentSchema.table} "
        "WHERE ${AttachmentSchema.colId} = '$id' LIMIT 1",
      );
      if (rows.isEmpty) return AttachmentStateWire.synced;
      return rows.first[AttachmentSchema.colState]?.toString() ??
          AttachmentStateWire.synced;
    } on Object {
      return AttachmentStateWire.synced;
    }
  }

  /// on_success mapping mirroring nostos_core::AttachmentState::on_success.
  String _onSuccess(String state) {
    switch (state) {
      case AttachmentStateWire.queuedUpload:
      case AttachmentStateWire.queuedDownload:
        return AttachmentStateWire.synced;
      case AttachmentStateWire.queuedDelete:
        return AttachmentStateWire.archived;
      default:
        return state;
    }
  }

  Future<void> _transition(String id, String toState) => _db.write(
    table: AttachmentSchema.table,
    op: 'patch',
    pk: id,
    payload: {
      AttachmentSchema.colState: toState,
      AttachmentSchema.colTimestamp: _clock().millisecondsSinceEpoch,
    },
  );

  String _newId() {
    // UUIDv4 without a dep: use timestamp + random. Good enough for an
    // attachment key; collisions are not a correctness risk (upsert is
    // idempotent on id). ponytail: swap for `uuid` if a measurement shows
    // collisions — none expected at app scale.
    final r = DateTime.now().microsecondsSinceEpoch.toRadixString(36);
    final rand = (DateTime.now().microsecond * 1000003).toRadixString(36);
    return 'att_$r$rand';
  }
}

/// Extension glue: construct an [Attachments] from a [NostosDatabase], wiring
/// the blob store's [BlobStore.wipe] into sign-out (ADR-0029 consistency).
extension AttachmentDatabase on NostosDatabase {
  /// Build an [Attachments] driver over this database. The app MUST have
  /// `attachments` in its schema + subscribed tables. Server mode: also in
  /// the server's `NOSTOS_WRITE_TABLES` allowlist. Direct mode: the table
  /// needs a `nostos_log_attachments` change-log trigger (`nostos link --mode
  /// direct`, `--public attachments` for shared catalogs) and RLS that admits
  /// the device.
  ///
  /// Pass a [BlobStore] (normally a [LocalFileBlobStore] on a path_provider
  /// directory). Its [BlobStore.wipe] is registered as a sign-out hook so the
  /// next principal sees no blob bytes — see [registerSignOutHook]. With
  /// [NostosDatabase.keepLocalOnSignOut] (ADR-0049) the wipe is skipped, same
  /// as the engine keeps the rows.
  Attachments attachments({
    required AttachmentStorageAdapter adapter,
    required BlobStore blobStore,
    int maxAttempts = 5,
  }) {
    registerSignOutHook(() async {
      if (!keepLocalOnSignOut) await blobStore.wipe();
    });
    // Online = the connection has reached `connected` at least once and is not
    // currently `disconnected`. The NostosDatabase.isOnline getter snapshots the
    // status ValueNotifier (which tracks the connection-state stream).
    return Attachments(
      db: this,
      adapter: adapter,
      blobStore: blobStore,
      isOnline: () async => isOnline,
      maxAttempts: maxAttempts,
    );
  }
}
