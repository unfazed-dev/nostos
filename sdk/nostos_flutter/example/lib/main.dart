// Nostos Provider Dashboard — offline-first multi-table (Flutter, macOS).
//
// Showcases the RATIFIED nostos Flutter SDK surface (ADR-0022) through 5 tables
// on one /sync socket:
//   - multi-table subscribe → ONE socket, 5 tables, one checkpoint (D1)
//   - reactive typed watch   → watchMapped<T>('SELECT * FROM <table>', fromRow)
//                              per NavigationRail tab
//   - durable offline writes → write(table, op, pk, payload) lands in the local
//                              SQLite outbox + flushes on reconnect (ADR-0013)
//   - REAL pause/resume (D2) → disconnect() aborts ONLY the /sync loop (client
//                              + storage + watch pumps stay alive); resume()
//                              reconnects + flushes. NOT a dead-URL hack.
//   - connection state       → aggregate badge across the shared session
//
// API note: uses the SHIPPED NostosDatabase + SQL + watchMapped + write +
// disconnect/resume surface. The dashboard plan's "D5" names (watchOf /
// insert / writes-stream / unified Nostos) belong to the PROPOSED, unratified
// connection-redesign; the app's structure is identical, so a future D5 port is
// a mechanical rename of the call sites below.
//
// Backend: `nostos dev` binds ws://127.0.0.1:8800; override via --dart-define=
// NOSTOS_URL=... Writes round-trip only when NOSTOS_WRITE_TABLES lists the 5
// tables (see example/README.md).

import 'dart:async';
import 'dart:io';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

import 'nostos.g.dart' show nostosConfig, nostosSchema;
import 'models.dart';

/// Optional /sync URL override:
/// `flutter run --dart-define=NOSTOS_URL=ws://host:port/sync`.
/// When empty, the URL comes from `nostosConfig` (generated from `.nostos/config.json`
/// by `nostos gen`).
const _kUrlOverride = String.fromEnvironment('NOSTOS_URL');

// Schema + config are GENERATED into nostos.g.dart from `.nostos/` by
// `nostos link && nostos pull && nostos gen` — no hand-written schema here. The
// generated nostosSchema covers all 6 published tables (incl. tasks); the
// dashboard subscribes only to the 5 it renders (see _kTables). Re-running
// `nostos pull && nostos gen` after a server-side schema change IS the migration.

/// The 5 dashboard tables on ONE socket (D1/ADR-0022). Single-tenant v1 → no
/// where_sql; every row syncs. subscribeTables is the multi-table primitive
/// (calling subscribe() repeatedly would replace, not add — one active sub per
/// instance).
const _kTables = <NostosTableSub>[
  NostosTableSub(name: 'providers'),
  NostosTableSub(name: 'clients'),
  NostosTableSub(name: 'availabilities'),
  NostosTableSub(name: 'appointments'),
  NostosTableSub(name: 'invoices'),
];

/// A durable write into the local outbox. Pages call this (not the db directly)
/// so the shell can count writes made while paused.
typedef NostosWrite = Future<void> Function({
  required String table,
  required String op,
  required String pk,
  Map<String, dynamic>? payload,
});

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  runApp(const NostosDashboardApp());
}

class NostosDashboardApp extends StatelessWidget {
  const NostosDashboardApp({super.key});
  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'Nostos Provider Dashboard',
        debugShowCheckedModeBanner: false,
        theme: ThemeData(
          useMaterial3: true,
          colorSchemeSeed: const Color(0xFF2E6FDB),
        ),
        home: const DashboardShell(),
      );
}

class DashboardShell extends StatefulWidget {
  const DashboardShell({super.key});
  @override
  State<DashboardShell> createState() => _DashboardShellState();
}

class _DashboardShellState extends State<DashboardShell> {
  NostosDatabase? _db;
  NostosConnectionState _state = NostosConnectionState.disconnected;
  int _selectedIndex = 0;
  int _queued = 0; // writes made while paused (≈ pending; cleared on reconnect)
  bool _paused = false; // real disconnect (D2)
  String? _error;
  StreamSubscription<NostosConnectionState>? _stateSub;

  @override
  void initState() {
    super.initState();
    _boot();
  }

  @override
  void dispose() {
    _stateSub?.cancel();
    _db?.close();
    super.dispose();
  }

  Future<void> _boot() async {
    try {
      // Generated contract: nostosConfig + nostosSchema come from `.nostos/`
      // (produced by `nostos link && nostos pull && nostos gen`). A
      // --dart-define=NOSTOS_URL still wins for dev runs against another server.
      var config = nostosConfig;
      if (_kUrlOverride.isNotEmpty) {
        config = NostosConfig(
          url: _kUrlOverride,
          supabaseUrl: config.supabaseUrl,
          supabaseAnonKey: config.supabaseAnonKey,
          sqliteFilename: config.sqliteFilename,
        );
      }
      final db = await NostosDatabase.open(
        config: config,
        schema: nostosSchema, // generated from GET /schema — re-applied == migrated
        sqliteDir: Directory.systemTemp.path,
      );
      await db.subscribeTables(_kTables);
      _stateSub = db.connectionState.listen((s) {
        if (!mounted) return;
        setState(() {
          _state = s;
          if (s == NostosConnectionState.connected) _queued = 0;
        });
      });
      if (!mounted) return;
      setState(() => _db = db);
    } catch (e) {
      if (!mounted) return;
      setState(() => _error = e.toString());
    }
  }

  // Real pause/resume (D2): disconnect aborts ONLY the /sync loop — client +
  // storage + watch pumps stay alive, so reads/writes/UI keep working offline.
  // resume respawns the loop on the SAME client; the outbox flushes on reconnect.
  Future<void> _toggleConnection() async {
    final db = _db;
    if (db == null) return;
    if (_paused) {
      db.resume();
      if (mounted) setState(() => _paused = false);
    } else {
      await db.disconnect();
      if (mounted) setState(() => _paused = true);
    }
  }

  // Page-facing write wrapper: enqueues via the durable outbox and bumps the
  // offline counter while paused (the honest "pending" signal; cleared on
  // reconnect). Writes are NEVER gated on connectivity — they always land.
  Future<void> _write({
    required String table,
    required String op,
    required String pk,
    Map<String, dynamic>? payload,
  }) async {
    final db = _db;
    if (db == null) return;
    if (_paused && op != 'delete' && mounted) {
      setState(() => _queued++);
    }
    try {
      await db.write(table: table, op: op, pk: pk, payload: payload);
    } catch (e) {
      if (!mounted) return;
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text('write failed: $e')));
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('Nostos Provider Dashboard'),
        actions: [
          _StateBadge(state: _state),
          IconButton(
            tooltip: _paused ? 'Resume syncing' : 'Disconnect',
            icon: Icon(_paused ? Icons.wifi : Icons.wifi_off),
            onPressed: _db == null ? null : _toggleConnection,
          ),
        ],
      ),
      body: Row(
        children: [
          NavigationRail(
            selectedIndex: _selectedIndex,
            onDestinationSelected: (i) => setState(() => _selectedIndex = i),
            extended: MediaQuery.of(context).size.width > 1000,
            leading: _paused
                ? const Padding(
                    padding: EdgeInsets.symmetric(vertical: 8),
                    child: Chip(
                      avatar: Icon(Icons.cloud_off, size: 18),
                      label: Text('paused', style: TextStyle(fontSize: 12)),
                    ),
                  )
                : null,
            destinations: const [
              NavigationRailDestination(
                icon: Icon(Icons.medical_services_outlined),
                selectedIcon: Icon(Icons.medical_services),
                label: Text('Providers'),
              ),
              NavigationRailDestination(
                icon: Icon(Icons.people_outline),
                selectedIcon: Icon(Icons.people),
                label: Text('Clients'),
              ),
              NavigationRailDestination(
                icon: Icon(Icons.calendar_month_outlined),
                selectedIcon: Icon(Icons.calendar_month),
                label: Text('Availabilities'),
              ),
              NavigationRailDestination(
                icon: Icon(Icons.event_outlined),
                selectedIcon: Icon(Icons.event),
                label: Text('Appointments'),
              ),
              NavigationRailDestination(
                icon: Icon(Icons.receipt_long_outlined),
                selectedIcon: Icon(Icons.receipt_long),
                label: Text('Invoices'),
              ),
            ],
          ),
          const VerticalDivider(width: 0),
          Expanded(child: _body()),
        ],
      ),
    );
  }

  Widget _body() {
    if (_db == null) {
      return Center(
        child: _error == null
            ? const CircularProgressIndicator()
            : Padding(
                padding: const EdgeInsets.all(24),
                child: Text('Connect failed:\n$_error'),
              ),
      );
    }
    final NostosWrite write = _write;
    final db = _db!;
    // IndexedStack keeps ALL five page States mounted simultaneously, so each
    // page's StreamBuilder subscribes to its watch stream ONCE (on first build)
    // and STAYS subscribed across tab switches. A `switch` here would recreate
    // each page's State on every tab change — the new StreamBuilder then races
    // the watch stream's initial snapshot and renders empty ("No providers
    // yet." while the rows are on disk). IndexedStack is the canonical Flutter
    // pattern for preserving state across NavigationRail destinations.
    final Widget body = IndexedStack(
      index: _selectedIndex,
      children: [
        _ProvidersPage(db: db, write: write),
        _ClientsPage(db: db, write: write),
        _AvailabilitiesPage(db: db),
        _AppointmentsPage(db: db, write: write),
        _InvoicesPage(db: db, write: write),
      ],
    );
    return Column(
      children: [
        if (_queued > 0)
          Material(
            color: Colors.amber.shade100,
            child: ListTile(
              leading: const Icon(Icons.cloud_upload, size: 20),
              title: Text(
                '$_queued write(s) queued locally — flush on resume',
                style: const TextStyle(fontSize: 13),
              ),
            ),
          ),
        Expanded(child: body),
      ],
    );
  }
}

// ---------------------------------------------------------------------------
// Shared widgets
// ---------------------------------------------------------------------------

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
      padding: const EdgeInsets.only(right: 8),
      child: Chip(
        avatar: Icon(icon, color: color, size: 18),
        label: Text(state.name,
            style: const TextStyle(fontSize: 12, fontWeight: FontWeight.w600)),
      ),
    );
  }
}

class _ReactiveList<T> extends StatelessWidget {
  const _ReactiveList({
    required this.stream,
    required this.tile,
    this.emptyText = 'No rows yet',
  });
  final Stream<List<T>> stream;
  final Widget Function(T) tile;
  final String emptyText;

  @override
  Widget build(BuildContext context) => StreamBuilder<List<T>>(
        stream: stream,
        builder: (context, snap) {
          final rows = snap.data ?? const [];
          if (rows.isEmpty) {
            return Center(child: Text(emptyText));
          }
          return ListView(children: rows.map(tile).toList());
        },
      );
}

/// A short id chip for FK columns (the UUID itself is uninformative in a list).
String _short(String id) =>
    id.length > 8 ? '${id.substring(0, 8)}…' : id;

/// Simple text-field form dialog. Returns the entered map (null on cancel).
Future<Map<String, String>?> _showTextForm(
  BuildContext context, {
  required String title,
  required List<({String key, String label})> fields,
}) async {
  final controllers = {
    for (final f in fields) f.key: TextEditingController(),
  };
  final result = await showDialog<Map<String, String>>(
    context: context,
    builder: (ctx) => AlertDialog(
      title: Text(title),
      content: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          for (final f in fields)
            Padding(
              padding: const EdgeInsets.symmetric(vertical: 4),
              child: TextField(
                controller: controllers[f.key],
                decoration: InputDecoration(labelText: f.label, isDense: true),
              ),
            ),
        ],
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(ctx, null),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: () {
            final m = <String, String>{};
            for (final f in fields) {
              final v = controllers[f.key]!.text.trim();
              if (v.isNotEmpty) m[f.key] = v;
            }
            Navigator.pop(ctx, m);
          },
          child: const Text('Save'),
        ),
      ],
    ),
  );
  for (final c in controllers.values) {
    c.dispose();
  }
  return result;
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

class _ProvidersPage extends StatefulWidget {
  const _ProvidersPage({required this.db, required this.write});
  final NostosDatabase db;
  final NostosWrite write;
  @override
  State<_ProvidersPage> createState() => _ProvidersPageState();
}

class _ProvidersPageState extends State<_ProvidersPage> {
  late final Stream<List<Provider>> _rows =
      widget.db.watchMapped<Provider>('SELECT * FROM providers', Provider.fromRow);

  Future<void> _add() async {
    final form = await _showTextForm(
      context,
      title: 'New provider',
      fields: const [
        (key: 'name', label: 'Name'),
        (key: 'specialty', label: 'Specialty'),
        (key: 'email', label: 'Email'),
        (key: 'phone', label: 'Phone'),
      ],
    );
    if (form == null || form['name'] == null) return;
    await widget.write(
      table: 'providers',
      op: 'upsert',
      pk: uuidV4(),
      payload: {
        ...form,
        'created_at': DateTime.now().toUtc().toIso8601String(),
      },
    );
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        body: _ReactiveList<Provider>(
          stream: _rows,
          emptyText: 'No providers yet.',
          tile: (p) => ListTile(
            leading: const CircleAvatar(child: Icon(Icons.person)),
            title: Text(p.name),
            subtitle: Text(
              [p.specialty, p.email, p.phone]
                  .whereType<String>()
                  .join('  •  '),
            ),
          ),
        ),
        floatingActionButton: FloatingActionButton(
          onPressed: _add,
          child: const Icon(Icons.add),
        ),
      );
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

class _ClientsPage extends StatefulWidget {
  const _ClientsPage({required this.db, required this.write});
  final NostosDatabase db;
  final NostosWrite write;
  @override
  State<_ClientsPage> createState() => _ClientsPageState();
}

class _ClientsPageState extends State<_ClientsPage> {
  late final Stream<List<Client>> _rows =
      widget.db.watchMapped<Client>('SELECT * FROM clients', Client.fromRow);

  Future<void> _add() async {
    final form = await _showTextForm(
      context,
      title: 'New client',
      fields: const [
        (key: 'name', label: 'Name'),
        (key: 'email', label: 'Email'),
        (key: 'phone', label: 'Phone'),
        (key: 'notes', label: 'Notes'),
      ],
    );
    if (form == null || form['name'] == null) return;
    await widget.write(
      table: 'clients',
      op: 'upsert',
      pk: uuidV4(),
      payload: {
        ...form,
        'created_at': DateTime.now().toUtc().toIso8601String(),
      },
    );
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        body: _ReactiveList<Client>(
          stream: _rows,
          emptyText: 'No clients yet.',
          tile: (c) => ListTile(
            leading: const CircleAvatar(child: Icon(Icons.person_outline)),
            title: Text(c.name),
            subtitle: Text(
              [c.email, c.phone, c.notes].whereType<String>().join('  •  '),
            ),
          ),
        ),
        floatingActionButton: FloatingActionButton(
          onPressed: _add,
          child: const Icon(Icons.add),
        ),
      );
}

// ---------------------------------------------------------------------------
// Availabilities (read-only list)
// ---------------------------------------------------------------------------

class _AvailabilitiesPage extends StatefulWidget {
  const _AvailabilitiesPage({required this.db});
  final NostosDatabase db;
  @override
  State<_AvailabilitiesPage> createState() => _AvailabilitiesPageState();
}

class _AvailabilitiesPageState extends State<_AvailabilitiesPage> {
  late final Stream<List<Availability>> _rows = widget.db
      .watchMapped<Availability>('SELECT * FROM availabilities', Availability.fromRow);

  @override
  Widget build(BuildContext context) => Scaffold(
        body: _ReactiveList<Availability>(
          stream: _rows,
          emptyText: 'No availabilities yet.',
          tile: (a) => ListTile(
            leading: const Icon(Icons.access_time),
            title: Text('${a.day}  ${a.range}'),
            subtitle: Text('provider: ${_short(a.providerId)}'),
          ),
        ),
      );
}

// ---------------------------------------------------------------------------
// Appointments (create + complete/cancel)
// ---------------------------------------------------------------------------

class _AppointmentsPage extends StatefulWidget {
  const _AppointmentsPage({required this.db, required this.write});
  final NostosDatabase db;
  final NostosWrite write;
  @override
  State<_AppointmentsPage> createState() => _AppointmentsPageState();
}

class _AppointmentsPageState extends State<_AppointmentsPage> {
  late final Stream<List<Appointment>> _rows = widget.db.watchMapped<Appointment>(
    'SELECT * FROM appointments',
    Appointment.fromRow,
  );

  Future<void> _add() async {
    final form = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (_) => _AppointmentDialog(db: widget.db),
    );
    if (form == null) return;
    await widget.write(
      table: 'appointments',
      op: 'upsert',
      pk: uuidV4(),
      payload: form,
    );
  }

  Future<void> _setStatus(Appointment a, String status) async {
    await widget.write(
      table: 'appointments',
      op: 'patch',
      pk: a.id,
      payload: {'status': status},
    );
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        body: _ReactiveList<Appointment>(
          stream: _rows,
          emptyText: 'No appointments yet.',
          tile: (a) => ListTile(
            leading: const Icon(Icons.event),
            title: Text(a.startsAt.isEmpty ? '(unscheduled)' : a.startsAt),
            subtitle: Text(
              'provider ${_short(a.providerId)}  •  client ${_short(a.clientId)}'
              '  •  ${a.durationMin} min',
            ),
            trailing: _statusActions(a),
          ),
        ),
        floatingActionButton: FloatingActionButton(
          onPressed: _add,
          child: const Icon(Icons.add),
        ),
      );

  Widget _statusActions(Appointment a) {
    final chip = Chip(label: Text(a.status));
    if (a.status == 'confirmed') {
      return Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          chip,
          IconButton(
            tooltip: 'Complete',
            icon: const Icon(Icons.check_circle_outline, size: 20),
            onPressed: () => _setStatus(a, 'completed'),
          ),
          IconButton(
            tooltip: 'Cancel',
            icon: const Icon(Icons.cancel_outlined, size: 20),
            onPressed: () => _setStatus(a, 'cancelled'),
          ),
        ],
      );
    }
    return chip;
  }
}

class _AppointmentDialog extends StatefulWidget {
  const _AppointmentDialog({required this.db});
  final NostosDatabase db;
  @override
  State<_AppointmentDialog> createState() => _AppointmentDialogState();
}

class _AppointmentDialogState extends State<_AppointmentDialog> {
  List<Provider> _providers = const [];
  List<Client> _clients = const [];
  String? _providerId;
  String? _clientId;
  final _starts = TextEditingController();
  final _duration = TextEditingController(text: '30');
  bool _loading = true;

  @override
  void initState() {
    super.initState();
    _starts.text =
        DateTime.now().add(const Duration(hours: 1)).toUtc().toIso8601String();
    _load();
  }

  @override
  void dispose() {
    _starts.dispose();
    _duration.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    final ps = await widget.db.getAll('SELECT * FROM providers');
    final cs = await widget.db.getAll('SELECT * FROM clients');
    if (!mounted) return;
    setState(() {
      _providers = ps.map(Provider.fromRow).toList();
      _clients = cs.map(Client.fromRow).toList();
      _providerId = _providers.isEmpty ? null : _providers.first.id;
      _clientId = _clients.isEmpty ? null : _clients.first.id;
      _loading = false;
    });
  }

  void _submit() {
    if (_providerId == null || _clientId == null) return;
    Navigator.pop(context, <String, dynamic>{
      'provider_id': _providerId,
      'client_id': _clientId,
      'starts_at': _starts.text.trim(),
      'duration_min': int.tryParse(_duration.text.trim()) ?? 30,
      'status': 'confirmed',
      'created_at': DateTime.now().toUtc().toIso8601String(),
    });
  }

  @override
  Widget build(BuildContext context) {
    if (_loading) {
      return const AlertDialog(
        content: SizedBox(
          height: 48,
          width: 48,
          child: Center(child: CircularProgressIndicator()),
        ),
      );
    }

    return AlertDialog(
      title: const Text('New appointment'),
      content: SizedBox(
        width: 320,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            DropdownButtonFormField<String>(
              decoration: const InputDecoration(labelText: 'Provider'),
              initialValue: _providerId,
              items: [
                for (final p in _providers)
                  DropdownMenuItem<String>(
                      value: p.id,
                      child: Text(
                          '${p.name}${p.specialty == null ? '' : ' — ${p.specialty}'}')),
              ],
              onChanged: (v) => setState(() => _providerId = v),
            ),
            const SizedBox(height: 8),
            DropdownButtonFormField<String>(
              decoration: const InputDecoration(labelText: 'Client'),
              initialValue: _clientId,
              items: [
                for (final c in _clients)
                  DropdownMenuItem<String>(value: c.id, child: Text(c.name)),
              ],
              onChanged: (v) => setState(() => _clientId = v),
            ),
            const SizedBox(height: 8),
            TextField(
              controller: _starts,
              decoration: const InputDecoration(labelText: 'Starts at (ISO 8601)'),
            ),
            const SizedBox(height: 8),
            TextField(
              controller: _duration,
              decoration: const InputDecoration(labelText: 'Duration (min)'),
              keyboardType: TextInputType.number,
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context, null),
          child: const Text('Cancel'),
        ),
        FilledButton(onPressed: _submit, child: const Text('Create')),
      ],
    );
  }
}

// ---------------------------------------------------------------------------
// Invoices (create)
// ---------------------------------------------------------------------------

class _InvoicesPage extends StatefulWidget {
  const _InvoicesPage({required this.db, required this.write});
  final NostosDatabase db;
  final NostosWrite write;
  @override
  State<_InvoicesPage> createState() => _InvoicesPageState();
}

class _InvoicesPageState extends State<_InvoicesPage> {
  late final Stream<List<Invoice>> _rows =
      widget.db.watchMapped<Invoice>('SELECT * FROM invoices', Invoice.fromRow);

  Future<void> _add() async {
    final form = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (_) => _InvoiceDialog(db: widget.db),
    );
    if (form == null) return;
    await widget.write(
      table: 'invoices',
      op: 'upsert',
      pk: uuidV4(),
      payload: form,
    );
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        body: _ReactiveList<Invoice>(
          stream: _rows,
          emptyText: 'No invoices yet.',
          tile: (i) => ListTile(
            leading: const Icon(Icons.receipt_long),
            title: Text('${i.amount}  —  ${i.status}'),
            subtitle: Text(
              'appt ${_short(i.appointmentId)}  •  client ${_short(i.clientId)}',
            ),
          ),
        ),
        floatingActionButton: FloatingActionButton(
          onPressed: _add,
          child: const Icon(Icons.add),
        ),
      );
}

class _InvoiceDialog extends StatefulWidget {
  const _InvoiceDialog({required this.db});
  final NostosDatabase db;
  @override
  State<_InvoiceDialog> createState() => _InvoiceDialogState();
}

class _InvoiceDialogState extends State<_InvoiceDialog> {
  List<Appointment> _appts = const [];
  List<Client> _clients = const [];
  String? _apptId;
  String? _clientId;
  final _amount = TextEditingController();
  bool _loading = true;

  @override
  void initState() {
    super.initState();
    _load();
  }

  @override
  void dispose() {
    _amount.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    final as_ = await widget.db.getAll('SELECT * FROM appointments');
    final cs = await widget.db.getAll('SELECT * FROM clients');
    if (!mounted) return;
    setState(() {
      _appts = as_.map(Appointment.fromRow).toList();
      _clients = cs.map(Client.fromRow).toList();
      _apptId = _appts.isEmpty ? null : _appts.first.id;
      _clientId = _clients.isEmpty ? null : _clients.first.id;
      _loading = false;
    });
  }

  void _submit() {
    if (_apptId == null || _clientId == null) return;
    final dollars = double.tryParse(_amount.text.trim()) ?? 0;
    Navigator.pop(context, <String, dynamic>{
      'appointment_id': _apptId,
      'client_id': _clientId,
      'amount_cents': (dollars * 100).round(),
      'status': 'issued',
      'issued_at': DateTime.now().toUtc().toIso8601String(),
      'created_at': DateTime.now().toUtc().toIso8601String(),
    });
  }

  @override
  Widget build(BuildContext context) {
    if (_loading) {
      return const AlertDialog(
        content: SizedBox(
          height: 48,
          width: 48,
          child: Center(child: CircularProgressIndicator()),
        ),
      );
    }
    return AlertDialog(
      title: const Text('New invoice'),
      content: SizedBox(
        width: 320,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            DropdownButtonFormField<String>(
              decoration: const InputDecoration(labelText: 'Appointment'),
              initialValue: _apptId,
              items: [
                for (final a in _appts)
                  DropdownMenuItem<String>(
                    value: a.id,
                    child: Text('${a.startsAt} (${a.status})'),
                  ),
              ],
              onChanged: (v) => setState(() => _apptId = v),
            ),
            const SizedBox(height: 8),
            DropdownButtonFormField<String>(
              decoration: const InputDecoration(labelText: 'Client'),
              initialValue: _clientId,
              items: [
                for (final c in _clients)
                  DropdownMenuItem<String>(value: c.id, child: Text(c.name)),
              ],
              onChanged: (v) => setState(() => _clientId = v),
            ),
            const SizedBox(height: 8),
            TextField(
              controller: _amount,
              decoration: const InputDecoration(labelText: 'Amount (USD)'),
              keyboardType: const TextInputType.numberWithOptions(decimal: true),
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context, null),
          child: const Text('Cancel'),
        ),
        FilledButton(onPressed: _submit, child: const Text('Create')),
      ],
    );
  }
}
