// Dashboard shell — the responsive navigation scaffold.
//
// Desktop/tablet (width > 600): NavigationRail on the left with 6 destinations.
// Mobile (width ≤ 600): BottomNavigationBar.
// All 6 pages stay mounted via IndexedStack so their reactive watch streams
// survive tab switches (the IndexedStack + _replayLatest pattern that fixed the
// "empty lists on tab switch" bug — see nostos.dart:_replayLatest).

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

import '../widgets/connection_badge.dart' show ConnectionBadge;
import 'appointments/appointments_view.dart';
import 'availabilities/availabilities_view.dart';
import 'chat/chat_view.dart';
import 'clients/clients_view.dart';
import 'invoices/invoices_view.dart';
import 'providers/providers_view.dart';

/// The 6 dashboard tables on ONE socket (D1/ADR-0022). Single-tenant v1 → no
/// where_sql; every row syncs.
const kTables = <NostosTableSub>[
  NostosTableSub(name: 'providers'),
  NostosTableSub(name: 'clients'),
  NostosTableSub(name: 'availabilities'),
  NostosTableSub(name: 'appointments'),
  NostosTableSub(name: 'invoices'),
  NostosTableSub(name: 'messages'),
];

class DashboardShell extends StatefulWidget {
  const DashboardShell({super.key, required this.db});
  final NostosDatabase db;
  @override
  State<DashboardShell> createState() => _DashboardShellState();
}

class _DashboardShellState extends State<DashboardShell> {
  int _selectedIndex = 0;
  // Reactive source-of-truth for the badge + transition SnackBars is the
  // ADR-0024 `db.status: ValueListenable<SyncStatus>` (typed value object over
  // the underlying NostosConnectionState). We addListener on it directly — no
  // StreamSubscription, no hand-rolled NostosConnectionState bookkeeping.
  String _state = 'disconnected';
  // Seamless-offline UX: no visible queue, no manual pause toggle. The only
  // reconnect feedback is a transient SnackBar (optimistic local-first +
  // background reconciliation — see docs/plans/sync-strategy-analysis-2026-07-19.md).
  SyncStatus? _prevStatus;
  bool _everConnected = false;
  // In-app offline toggle — for testing the seamless-UX path WITHOUT toggling
  // the OS network. Tap drops the WS (badge → offline); writes while paused are
  // optimistic + silent (NO queue banner — that was the old dev-affordance we
  // removed). Tap again resumes; the listener above fires "Back online — data
  // synced". Also doubles as a user-facing "work offline" mode.
  bool _paused = false;

  @override
  void initState() {
    super.initState();
    // Seed prevStatus from the current value so the FIRST status tick has a
    // correct "wasConnected" baseline (matches the old stream's first-tick
    // behavior where _prevState started null → wasConnected=false).
    _prevStatus = widget.db.status.value;
    _state = _prevStatus!.conn.name;
    widget.db.status.addListener(_onStatus);
  }

  void _onStatus() {
    if (!mounted) return;
    final status = widget.db.status.value;
    final nowConnected = status.connected;
    final wasConnected = _prevStatus?.connected ?? false;
    setState(() => _state = status.conn.name);
    // "You're offline": fires on a genuine connected → disconnected drop
    // (real network loss OR the in-app toggle). Reassures the user their
    // edits are still being saved locally (optimistic local-first).
    if (wasConnected && !nowConnected) {
      ScaffoldMessenger.of(context).showSnackBar(
        const SnackBar(
          content: Text("You're offline — changes save locally"),
          duration: Duration(seconds: 2),
          behavior: SnackBarBehavior.floating,
        ),
      );
    }
    // "Back online — data synced" ONLY on a genuine reconnect: we were
    // connected before, dropped, and just came back. Suppresses first-boot
    // connect so a fresh app launch doesn't toast.
    if (nowConnected && !wasConnected && _everConnected) {
      ScaffoldMessenger.of(context).showSnackBar(
        const SnackBar(
          content: Text('Back online — data synced'),
          duration: Duration(seconds: 2),
          behavior: SnackBarBehavior.floating,
        ),
      );
    }
    if (nowConnected) _everConnected = true;
    _prevStatus = status;
  }

  @override
  void dispose() {
    widget.db.status.removeListener(_onStatus);
    super.dispose();
  }

  Future<void> _toggleConnection() async {
    if (_paused) {
      widget.db.resume();
      if (mounted) setState(() => _paused = false);
    } else {
      await widget.db.disconnect();
      if (mounted) setState(() => _paused = true);
    }
  }

  @override
  Widget build(BuildContext context) {
    final isWide = MediaQuery.of(context).size.width > 600;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Nostos Provider Dashboard'),
        actions: [
          ConnectionBadge(state: _state),
          IconButton(
            tooltip: _paused ? 'Reconnect' : 'Go offline',
            icon: Icon(_paused ? Icons.wifi : Icons.wifi_off),
            onPressed: _toggleConnection,
          ),
        ],
      ),
      body: isWide ? _wideLayout(context) : _narrowLayout(context),
    );
  }

  Widget _wideLayout(BuildContext context) {
    return Row(
      children: [
        NavigationRail(
          selectedIndex: _selectedIndex,
          onDestinationSelected: (i) => setState(() => _selectedIndex = i),
          extended: MediaQuery.of(context).size.width > 1000,
          destinations: _railDestinations(),
        ),
        const VerticalDivider(width: 0),
        Expanded(child: _page(_selectedIndex)),
      ],
    );
  }

  Widget _narrowLayout(BuildContext context) {
    return Column(
      children: [
        Expanded(child: _page(_selectedIndex)),
        NavigationBar(
          selectedIndex: _selectedIndex,
          onDestinationSelected: (i) => setState(() => _selectedIndex = i),
          destinations: _barDestinations(),
        ),
      ],
    );
  }

  // IndexedStack keeps ALL page States mounted simultaneously, so each page's
  // StreamBuilder subscribes ONCE and STAYS subscribed across tab switches.
  Widget _page(int index) => IndexedStack(
        index: index,
        children: [
          ProvidersView(db: widget.db),
          ClientsView(db: widget.db),
          AvailabilitiesView(db: widget.db),
          AppointmentsView(db: widget.db),
          InvoicesView(db: widget.db),
          ChatView(db: widget.db),
        ],
      );

  List<NavigationRailDestination> _railDestinations() => const [
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
          label: Text('Availability'),
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
        NavigationRailDestination(
          icon: Icon(Icons.chat_bubble_outline),
          selectedIcon: Icon(Icons.chat_bubble),
          label: Text('Chat'),
        ),
      ];

  List<NavigationDestination> _barDestinations() => const [
        NavigationDestination(
          icon: Icon(Icons.medical_services_outlined),
          selectedIcon: Icon(Icons.medical_services),
          label: 'Providers',
        ),
        NavigationDestination(
          icon: Icon(Icons.people_outline),
          selectedIcon: Icon(Icons.people),
          label: 'Clients',
        ),
        NavigationDestination(
          icon: Icon(Icons.event_outlined),
          selectedIcon: Icon(Icons.event),
          label: 'Appts',
        ),
        NavigationDestination(
          icon: Icon(Icons.receipt_long_outlined),
          selectedIcon: Icon(Icons.receipt_long),
          label: 'Invoices',
        ),
        NavigationDestination(
          icon: Icon(Icons.chat_bubble_outline),
          selectedIcon: Icon(Icons.chat_bubble),
          label: 'Chat',
        ),
      ];
}
