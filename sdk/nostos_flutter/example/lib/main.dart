// Nostos reference demo app — offline-first Tasks (Flutter).
//
// Demonstrates EVERY nostos capability through one screen:
//   - live replication        → the list updates as rows arrive over /sync
//   - reactive watch          → watch('tasks') drives the ListView
//   - durable offline writes  → add a task while OFFLINE; it queues in the local
//                               SQLite outbox + flushes on RECONNECT (ADR-0013)
//   - client↔server echo      → your own write round-trips back into the list
//   - connection state        → the badge tracks connecting/connected/reconnecting/disconnected
//   - operator controls       → Connect / Disconnect / Stop / Airplane
//
// nostos operates as a LOCAL offline-first store: reads + writes hit the on-device
// SQLite immediately; the server is just a sync peer. Pull the connection
// (Disconnect/Airplane) and the app keeps working — writes land locally and
// sync the moment the link is back. That is the PowerSync-equivalent contract.
//
// Backend: point NOSTOS_URL at a `nostos-server` (fake/pg replicator) or the shared
// e2e spine (`ws://127.0.0.1:<port>/sync`). The spine + the pg replicator deliver
// real JSON payloads (title/completed render); the fake replicator delivers
// opaque filler (rows still arrive + queue, but render as raw bytes).
//
// Run: see docs/plans/nostos-reference-demo-app.md.

import 'dart:async';
import 'dart:io';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

/// The /sync WebSocket URL. Override at launch with
/// `flutter run --dart-define=NOSTOS_URL=ws://host:port/sync`.
const _kUrl = String.fromEnvironment(
  'NOSTOS_URL',
  defaultValue: 'ws://127.0.0.1:8800/sync',
);

/// A stable on-device SQLite path so Disconnect→Reconnect (which creates a new
/// Nostos instance) resumes the SAME durable store — pending writes survive.
String get _sqlitePath =>
    '${Directory.systemTemp.path}/nostos-demo-tasks.sqlite';

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
  Nostos? _nostos;
  NostosConnectionState _state = NostosConnectionState.disconnected;
  final List<Map<String, dynamic>> _rows = [];
  StreamSubscription? _rowsSub;
  StreamSubscription? _stateSub;
  final TextEditingController _title = TextEditingController();
  int _writesQueuedWhileOffline = 0;
  int _counter = 0;

  bool get _isLive => _nostos != null;
  bool get _isOffline => _state == NostosConnectionState.disconnected ||
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
    _nostos?.close();
    _title.dispose();
    super.dispose();
  }

  // --- connection lifecycle --------------------------------------------------

  Future<void> _connect() async {
    if (_isLive) return;
    final c = await Nostos.connect(url: _kUrl, sqlitePath: _sqlitePath);
    _stateSub?.cancel();
    _stateSub = c.connectionState.listen((s) {
      if (!mounted) return;
      setState(() {
        _state = s;
        if (s == NostosConnectionState.connected) _writesQueuedWhileOffline = 0;
      });
    });
    await c.subscribe('tasks');
    _rowsSub?.cancel();
    _rowsSub = c.watch('tasks').listen((rows) {
      if (!mounted) return;
      setState(() => _rows
        ..clear()
        ..addAll(_sortRows(rows)));
    });
    if (!mounted) return;
    setState(() {
      _nostos = c;
      _state = NostosConnectionState.connecting;
    });
  }

  // Disconnect = close() + drop the handle. The SQLite store (incl. the pending
  // outbox) persists at _sqlitePath; a fresh Nostos.connect on the same path
  // resumes it and flushes queued writes on reconnect.
  Future<void> _disconnect() async {
    await _rowsSub?.cancel();
    await _stateSub?.cancel();
    await _nostos?.close();
    if (!mounted) return;
    setState(() {
      _nostos = null;
      _state = NostosConnectionState.disconnected;
    });
  }

  // --- writes ----------------------------------------------------------------

  Future<void> _addTask() async {
    final title = _title.text.trim();
    if (title.isEmpty || _nostos == null) return;
    _title.clear();
    final pk = 'task-${DateTime.now().microsecondsSinceEpoch}-${_counter++}';
    setState(() {
      if (_isOffline) _writesQueuedWhileOffline++;
    });
    try {
      await _nostos!.write(
        'tasks',
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
    if (_nostos == null) return;
    final pk = row['_pk']?.toString() ?? row['pk']?.toString();
    if (pk == null) return;
    await _nostos!.write(
      'tasks',
      op: 'upsert',
      pk: pk,
      payload: {
        'title': row['title'] ?? '(untitled)',
        'completed': done,
        'org_id': row['org_id'] ?? '00000000-0000-0000-0000-000000000000',
      },
    );
  }

  // --- UI --------------------------------------------------------------------

  @override
  Widget build(BuildContext context) => Scaffold(
        appBar: AppBar(
          title: const Text('Nostos Tasks'),
          actions: [_StateBadge(state: _state)],
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
            Expanded(child: _list()),
            _addBar(),
          ],
        ),
      );

  Widget _controlBar() => Padding(
        padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
        child: Wrap(
          spacing: 8,
          children: [
            if (!_isLive)
              FilledButton.icon(
                icon: const Icon(Icons.power, size: 18),
                label: const Text('Connect'),
                onPressed: _connect,
              )
            else
              FilledButton.tonalIcon(
                icon: Icon(_isOffline ? Icons.cloud_off : Icons.cloud_done,
                    size: 18),
                label: const Text('Disconnect'),
                onPressed: _disconnect,
              ),
            FilledButton.tonalIcon(
              icon: Icon(_isOffline ? Icons.wifi : Icons.airplanemode_active,
                  size: 18),
              label: Text(_isOffline ? 'Airplane (resume)' : 'Airplane'),
              onPressed: _isLive
                  ? (_isOffline ? _connect : _disconnect)
                  : null,
            ),
            OutlinedButton.icon(
              icon: const Icon(Icons.stop_circle_outlined, size: 18),
              label: const Text('Stop'),
              onPressed: _isLive ? _disconnect : null,
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
        final key = row['_pk']?.toString() ?? row['pk']?.toString() ?? '$i';
        return ListTile(
          leading: Checkbox(
            value: completed,
            onChanged: _isLive ? (v) => _toggle(row, v ?? false) : null,
          ),
          title: title == null
              ? Text(key,
                  style: const TextStyle(
                      fontFamily: 'monospace', color: Colors.grey))
              : Text(title,
                  style: TextStyle(
                      decoration:
                          completed ? TextDecoration.lineThrough : null)),
          subtitle: title == null ? Text(row.toString()) : Text('pk: $key'),
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
  const _StateBadge({required this.state});
  final NostosConnectionState state;

  @override
  Widget build(BuildContext context) {
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
