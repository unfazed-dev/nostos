// Nostos reference demo app — offline-first Tasks (Flutter).
//
// Demonstrates EVERY nostos capability through one screen:
//   - live replication        → the list updates as rows arrive over /sync
//   - reactive watch          → watch(<SQL>) drives the ListView
//   - durable offline writes  → add a task while OFFLINE; it queues in the local
//                               SQLite outbox + flushes on RECONNECT (ADR-0013)
//   - client↔server echo      → your own write round-trips back into the list
//   - per-row delete / edit   → trailing delete + tap-to-edit (upsert by pk)
//   - connection state        → the badge tracks connecting/connected/reconnecting/disconnected
//   - operator controls       → Connect/Resume · Disconnect · Stop · Airplane (each distinct)
//
// nostos operates as a LOCAL offline-first store: reads + writes hit the on-device
// SQLite immediately; the server is just a sync peer. Pull the connection
// (Disconnect/Stop) and the app keeps working — writes land locally and sync
// the moment the link is back. That is the PowerSync-equivalent contract.
//
// Backend: point NOSTOS_URL at a `nostos-server` (fake/pg replicator) or the shared
// e2e spine (`ws://127.0.0.1:<port>/sync`). The spine + the pg replicator deliver
// real JSON payloads (title/completed render); the fake replicator delivers
// opaque filler (rows still arrive + queue, but render as raw keys).
//
// Run: see docs/plans/nostos-reference-demo-app.md.

import 'dart:async';
import 'dart:io';
import 'dart:math';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

/// The /sync WebSocket URL. Override at launch with
/// `flutter run --dart-define=NOSTOS_URL=ws://host:port/sync`.
const _kUrl = String.fromEnvironment(
  'NOSTOS_URL',
  defaultValue: 'ws://127.0.0.1:8800/sync',
);

/// A stable on-device SQLite path so Disconnect→Resume (and Stop→Connect)
/// re-open the SAME durable store — pending writes + already-synced rows
/// survive across object lifecycles.
String get _sqlitePath =>
    '${Directory.systemTemp.path}/nostos-demo-tasks.sqlite';

// Reactive SELECT for the list. The WS2 `tasks` view (materialized by
// `apply_schema`) projects the row key as `_pk` plus the typed payload
// columns, so the PowerSync-idiomatic `SELECT * FROM tasks` carries everything
// the list + toggle/edit/delete need (keyed on `_pk`).
//
// For the fake replicator the payload is opaque filler bytes, so the
// json_extract'd columns come back NULL — the row still carries `_pk` and the
// list falls back to rendering the raw key (see _list).
const _kWatchSql = 'SELECT * FROM tasks';

// RFC 4122 v4 UUID for the `tasks.id` column (uuid PK). Hand-rolled from
// dart:math so the demo adds no dependency; the write-back binds pk as $1 → id.
final _rng = Random();
String _uuidV4() {
  final b = List<int>.generate(16, (_) => _rng.nextInt(256));
  b[6] = (b[6] & 0x0f) | 0x40; // version 4
  b[8] = (b[8] & 0x3f) | 0x80; // variant 10
  final h = b.map((x) => x.toRadixString(16).padLeft(2, '0')).join();
  return '${h.substring(0, 8)}-${h.substring(8, 12)}-${h.substring(12, 16)}-'
      '${h.substring(16, 20)}-${h.substring(20)}';
}

void main() => runApp(const NostosDemoApp());

class NostosDemoApp extends StatelessWidget {
  const NostosDemoApp({super.key});
  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'Nostos Tasks',
        debugShowCheckedModeBanner: false,
        theme: ThemeData(useMaterial3: true, colorSchemeSeed: Colors.indigo),
        home: const TasksPage(),
      );
}

class TasksPage extends StatefulWidget {
  const TasksPage({super.key});
  @override
  State<TasksPage> createState() => _TasksPageState();
}

class _TasksPageState extends State<TasksPage> {
  // The NostosDatabase handle. Null after Stop (or before first Connect);
  // non-null but `_held` after Disconnect (object retained, session closed —
  // see _disconnect/_stop for why that distinction is visible to the user).
  NostosDatabase? _db;
  // `_held` = a Disconnect closed the session but we KEPT the NostosDatabase
  // reference (and the local SQLite file). Visibly distinct from Stop (which
  // nulls _db) at the object-lifecycle level: Resume re-runs
  // NostosDatabase.connect on the same _sqlitePath and picks up the durable
  // store + WS2 views without a cold start.
  bool _held = false;
  // Airplane: a CLIENT-SIDE offline toggle (no FFI call). The UI badge
  // reflects it; the underlying reconnect loop (connectionState emits
  // 'reconnecting') demonstrates real server-side retry. To observe a TRUE
  // network cut, stop nostos-server — the app shows 'reconnecting' and any
  // queued writes flush on restore.
  //
  // ponytail: a TRUE pause (stop the sync loop but keep accepting local
  // writes) and a real network offline-toggle need an FFI extension —
  // pause()/resume()/setOffline() on the underlying Nostos handle. Deferred
  // (do NOT add FFI surface in WS5). Today Airplane is a UI hint + badge,
  // not a wire cut.
  bool _offline = false;
  NostosConnectionState _state = NostosConnectionState.disconnected;
  final List<Map<String, dynamic>> _rows = [];
  StreamSubscription? _rowsSub;
  StreamSubscription? _stateSub;
  final TextEditingController _title = TextEditingController();
  int _writesQueuedWhileOffline = 0;

  bool get _isLive => _db != null && !_held;
  bool get _isBadLink =>
      _state == NostosConnectionState.disconnected ||
      _state == NostosConnectionState.reconnecting;

  @override
  void initState() {
    super.initState();
    _connect();
  }

  @override
  void dispose() {
    _rowsSub?.cancel();
    _stateSub?.cancel();
    _db?.close();
    _title.dispose();
    super.dispose();
  }

  // --- connection lifecycle --------------------------------------------------

  // Connect (cold start, _db == null) OR Resume (fast re-subscribe, _held).
  // Both routes call this; NostosDatabase.connect re-opens the handle on the
  // SAME _sqlitePath, so the durable store + WS2 views survive across
  // close() and queued writes flush on the next connected tick. Idempotent
  // while live.
  Future<void> _connect() async {
    if (_isLive) return;
    final db = await NostosDatabase.connect(
      url: _kUrl,
      sqlitePath: _sqlitePath,
    );
    _stateSub?.cancel();
    _stateSub = db.connectionState.listen((s) {
      if (!mounted) return;
      setState(() {
        _state = s;
        if (s == NostosConnectionState.connected) _writesQueuedWhileOffline = 0;
      });
    });
    await db.subscribe('tasks');
    _rowsSub?.cancel();
    _rowsSub = db.watch(_kWatchSql).listen((rows) {
      if (!mounted) return;
      setState(() => _rows
        ..clear()
        ..addAll(_sortRows(rows)));
    });
    if (!mounted) return;
    setState(() {
      _db = db;
      _held = false;
      _state = NostosConnectionState.connecting;
    });
  }

  // Shared teardown: cancel the watch + state subs and close the session.
  // `keep = true` = Disconnect (retain `_db` + the local SQLite file so the
  // primary button re-labels to "Resume" and the next `_connect` re-opens the
  // persisted store fast). `keep = false` = Stop (null `_db` — full teardown,
  // cold-start next "Connect"; the local SQLite file still survives).
  Future<void> _tearDown({required bool keep}) async {
    await _rowsSub?.cancel();
    await _stateSub?.cancel();
    await _db?.close();
    if (!mounted) return;
    setState(() {
      _held = keep;
      if (!keep) _db = null;
      _state = NostosConnectionState.disconnected;
    });
  }

  Future<void> _disconnect() => _tearDown(keep: true);

  Future<void> _stop() => _tearDown(keep: false);

  // Airplane toggle — flips the client-side `_offline` flag only. See its
  // ponytail above: no FFI surface for a real wire cut in WS5.
  void _toggleAirplane() => setState(() => _offline = !_offline);

  // --- writes ----------------------------------------------------------------

  Future<void> _addTask() async {
    final title = _title.text.trim();
    if (title.isEmpty || _db == null || _held) return;
    _title.clear();
    final pk = _uuidV4();
    setState(() {
      if (_offline || _isBadLink) _writesQueuedWhileOffline++;
    });
    try {
      await _db!.write(
        table: 'tasks',
        op: 'upsert',
        pk: pk,
        payload: {
          'title': title,
          'completed': false,
          'org_id': '00000000-0000-0000-0000-000000000000',
          'created_at': DateTime.now().toUtc().toIso8601String(),
        },
      );
    } catch (e) {
      if (!mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('write failed: $e')),
      );
    }
  }

  Future<void> _toggle(Map<String, dynamic> row, bool done) async {
    if (_db == null || _held) return;
    final pk = row['_pk']?.toString();
    if (pk == null) return;
    await _db!.write(
      table: 'tasks',
      op: 'upsert',
      pk: pk,
      payload: {
        'title': row['title'] ?? '(untitled)',
        'completed': done,
        'org_id': row['org_id'] ?? '00000000-0000-0000-0000-000000000000',
      },
    );
  }

  // Tap a row → edit-title dialog → upsert by pk (carries current completed
  // + org_id so the partial write doesn't blank them server-side).
  Future<void> _editRow(Map<String, dynamic> row) async {
    if (_db == null || _held) return;
    final pk = row['_pk']?.toString();
    if (pk == null) return;
    final controller =
        TextEditingController(text: row['title']?.toString() ?? '');
    final next = await showDialog<String>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Edit task'),
        content: TextField(
          controller: controller,
          autofocus: true,
          decoration: const InputDecoration(hintText: 'Title'),
          onSubmitted: (v) => Navigator.of(ctx).pop(v.trim()),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(null),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(ctx).pop(controller.text.trim()),
            child: const Text('Save'),
          ),
        ],
      ),
    );
    if (!mounted || next == null || next.isEmpty) return;
    await _db!.write(
      table: 'tasks',
      op: 'upsert',
      pk: pk,
      payload: {
        'title': next,
        'completed': row['completed'] == true,
        'org_id': row['org_id'] ?? '00000000-0000-0000-0000-000000000000',
      },
    );
  }

  // Per-row delete (trailing IconButton). `op: 'delete'` enqueues a
  // delete-by-pk in the durable outbox; the row round-trips out of `watch`
  // like any replicated change.
  Future<void> _deleteRow(Map<String, dynamic> row) async {
    if (_db == null || _held) return;
    final pk = row['_pk']?.toString();
    if (pk == null) return;
    await _db!.write(table: 'tasks', op: 'delete', pk: pk);
  }

  // --- UI --------------------------------------------------------------------

  @override
  Widget build(BuildContext context) => Scaffold(
        appBar: AppBar(
          title: const Text('Nostos Tasks'),
          actions: [
            _StateBadge(state: _state, airplane: _offline),
          ],
        ),
        body: Column(
          children: [
            _controlBar(),
            if (_writesQueuedWhileOffline > 0)
              Material(
                color: Colors.amber.shade100,
                child: ListTile(
                  leading: const Icon(Icons.cloud_upload, size: 20),
                  title: Text(
                    '$_writesQueuedWhileOffline write(s) queued locally — '
                    'will sync on reconnect',
                    style: const TextStyle(fontSize: 13),
                  ),
                ),
              ),
            if (_offline)
              Material(
                color: Colors.blue.shade50,
                child: ListTile(
                  leading: const Icon(Icons.airplanemode_active, size: 20),
                  title: const Text(
                    'Airplane mode (client-side). Stop nostos-server to see a '
                    'real network cut — the app will show reconnecting and '
                    'queued writes flush on restore.',
                    style: TextStyle(fontSize: 12),
                  ),
                ),
              ),
            Expanded(child: _list()),
            _addBar(),
          ],
        ),
      );

  Widget _controlBar() => Padding(
        padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
        child: Wrap(
          spacing: 8,
          runSpacing: 4,
          children: [
            // Primary lifecycle button: Connect (cold start) or Resume (fast
            // re-subscribe after Disconnect) while not live; Disconnect while
            // live. The label flips between Connect/Resume based on whether
            // _db was retained (_held) — the visible signal that Disconnect
            // and Stop differ at the object level.
            if (!_isLive)
              FilledButton.icon(
                icon: const Icon(Icons.power, size: 18),
                label: Text(_db == null ? 'Connect' : 'Resume'),
                onPressed: _connect,
              )
            else
              FilledButton.tonalIcon(
                icon: const Icon(Icons.cloud_off, size: 18),
                label: const Text('Disconnect'),
                onPressed: _disconnect,
              ),
            // Stop: full teardown (close + null _db). Enabled whenever a
            // handle exists (live OR held) so a Disconnect can be promoted to
            // a Stop. Disabled only when no handle exists at all.
            OutlinedButton.icon(
              icon: const Icon(Icons.stop_circle_outlined, size: 18),
              label: const Text('Stop'),
              onPressed: _db != null ? _stop : null,
            ),
            // Airplane: client-side toggle, always available. Selected state
            // drives the badge + the offline banner above.
            FilterChip(
              avatar: const Icon(Icons.airplanemode_active, size: 18),
              label: Text(_offline ? 'Airplane on' : 'Airplane'),
              selected: _offline,
              onSelected: (_) => _toggleAirplane(),
            ),
          ],
        ),
      );

  Widget _list() {
    if (_rows.isEmpty) {
      return Center(
        child: Text(
          _isLive ? 'No tasks yet — add one below.' : 'Disconnected.',
          style: Theme.of(context).textTheme.bodyLarge,
        ),
      );
    }
    return ListView.builder(
      itemCount: _rows.length,
      itemBuilder: (context, i) {
        final row = _rows[i];
        final title = row['title']?.toString();
        final completed = row['completed'] == true;
        final key = row['_pk']?.toString() ?? '$i';
        final canMutate = _isLive;
        return ListTile(
          leading: Checkbox(
            value: completed,
            onChanged: canMutate ? (v) => _toggle(row, v ?? false) : null,
          ),
          onTap: canMutate ? () => _editRow(row) : null,
          title: title == null
              ? Text(key,
                  style: const TextStyle(
                      fontFamily: 'monospace', color: Colors.grey))
              : Text(title,
                  style: TextStyle(
                      decoration:
                          completed ? TextDecoration.lineThrough : null)),
          subtitle: title == null
              ? Text(row.toString(),
                  style: const TextStyle(
                      fontFamily: 'monospace', fontSize: 11))
              : Text('pk: $key'),
          trailing: IconButton(
            tooltip: 'Delete task',
            icon: const Icon(Icons.delete_outline, size: 20),
            onPressed: canMutate ? () => _deleteRow(row) : null,
          ),
        );
      },
    );
  }

  Widget _addBar() => SafeArea(
        child: Padding(
          padding: const EdgeInsets.fromLTRB(8, 4, 8, 8),
          child: Row(
            children: [
              Expanded(
                child: TextField(
                  controller: _title,
                  decoration: const InputDecoration(
                    hintText: 'Add a task (writes locally first, then syncs)',
                    border: OutlineInputBorder(),
                    isDense: true,
                  ),
                  onSubmitted: (_) => _addTask(),
                ),
              ),
              const SizedBox(width: 8),
              FilledButton(
                onPressed: _isLive ? _addTask : null,
                child: const Text('Add'),
              ),
            ],
          ),
        ),
      );

  // newest first; tolerate rows lacking a created_at (e.g. raw filler bytes).
  List<Map<String, dynamic>> _sortRows(List<Map<String, dynamic>> rows) {
    final copy = [...rows];
    copy.sort((a, b) {
      final ay = a['created_at']?.toString() ?? '';
      final by = b['created_at']?.toString() ?? '';
      return by.compareTo(ay);
    });
    return copy;
  }
}

class _StateBadge extends StatelessWidget {
  const _StateBadge({required this.state, required this.airplane});
  final NostosConnectionState state;
  final bool airplane;

  @override
  Widget build(BuildContext context) {
    if (airplane) {
      return Padding(
        padding: const EdgeInsets.only(right: 12),
        child: Chip(
          avatar: const Icon(Icons.airplanemode_active,
              color: Colors.blue, size: 18),
          label: const Text('airplane — will retry',
              style:
                  TextStyle(fontSize: 12, fontWeight: FontWeight.w600)),
        ),
      );
    }
    final (color, icon) = switch (state) {
      NostosConnectionState.connected => (Colors.green, Icons.cloud_done),
      NostosConnectionState.connecting => (Colors.orange, Icons.sync),
      NostosConnectionState.reconnecting => (Colors.orange, Icons.sync_problem),
      NostosConnectionState.disconnected => (Colors.grey, Icons.cloud_off),
    };
    return Padding(
      padding: const EdgeInsets.only(right: 12),
      child: Chip(
        avatar: Icon(icon, color: color, size: 18),
        label: Text(state.name,
            style: const TextStyle(fontSize: 12, fontWeight: FontWeight.w600)),
      ),
    );
  }
}
