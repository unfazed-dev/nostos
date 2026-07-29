// Clients view — list + add + detail.
//
// Shows each client with contact info and notes. The detail dialog surfaces
// notes prominently (the free-text context field).

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

import '../../nostos.g.dart' as gen;
import '../../models.dart';
import '../../widgets/connection_badge.dart' show InitialsAvatar, EmptyState;
import '../../widgets/form_dialogs.dart';

class ClientsView extends StatefulWidget {
  const ClientsView({super.key, required this.db});
  final NostosDatabase db;
  @override
  State<ClientsView> createState() => _ClientsViewState();
}

class _ClientsViewState extends State<ClientsView> {
  late final _clientsColl = widget.db.collection<Client>(
      table: 'clients', fromRow: Client.fromRow);
  late final Stream<List<Client>> _clients = _clientsColl.watch();
  // Typed write image (ADR-0024 Option C) — keep the read collection over the
  // presentation model (UI uses Client.initials etc.), write via gen.Client.
  late final _clientsWrite = widget.db.collection<gen.Client>(
      table: 'clients',
      fromRow: gen.Client.fromRow,
      toRow: (c) => c.toPayload());

  Future<void> _add() async {
    final form = await showFormDialog(
      context,
      title: 'New client',
      fields: const [
        DialogField(key: 'name', label: 'Name'),
        DialogField(key: 'email', label: 'Email'),
        DialogField(key: 'phone', label: 'Phone'),
        DialogField(key: 'notes', label: 'Notes', maxLines: 3),
      ],
      saveLabel: 'Create',
    );
    if (form == null || form['name'] == null) return;
    await _clientsWrite.upsert(gen.Client(
      id: uuidV4(),
      name: form['name'],
      email: form['email'],
      phone: form['phone'],
      notes: form['notes'],
      createdAt: DateTime.now().toUtc().toIso8601String(),
    ));
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        body: StreamBuilder<List<Client>>(
          stream: _clients,
          builder: (context, snap) {
            final clients = snap.data ?? const [];
            if (clients.isEmpty) {
              return const EmptyState(
                icon: Icons.people_outline,
                message: 'No clients yet. Tap + to add one.',
              );
            }
            return ListView.builder(
              itemCount: clients.length,
              itemBuilder: (context, i) {
                final c = clients[i];
                return Card(
                  margin: const EdgeInsets.symmetric(
                      horizontal: 16, vertical: 6),
                  child: ListTile(
                    leading: InitialsAvatar(
                      initials: c.initials,
                      color: Theme.of(context).colorScheme.tertiary,
                    ),
                    title: Text(c.name,
                        style: const TextStyle(fontWeight: FontWeight.w600)),
                    subtitle: Text(
                      [c.email, c.phone]
                          .whereType<String>()
                          .where((s) => s.isNotEmpty)
                          .join('  •  '),
                      style: TextStyle(
                          fontSize: 13,
                          color: Theme.of(context)
                              .colorScheme
                              .onSurfaceVariant),
                    ),
                    onTap: () => _showDetail(context, c),
                  ),
                );
              },
            );
          },
        ),
        floatingActionButton: FloatingActionButton(
          onPressed: _add,
          child: const Icon(Icons.add),
        ),
      );

  void _showDetail(BuildContext context, Client c) {
    showDialog(
      context: context,
      builder: (_) => AlertDialog(
        title: Row(
          children: [
            InitialsAvatar(
                initials: c.initials,
                color: Theme.of(context).colorScheme.tertiary),
            const SizedBox(width: 12),
            Expanded(child: Text(c.name)),
          ],
        ),
        content: SizedBox(
          width: 320,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              if (c.email != null) _row('Email', c.email!),
              if (c.phone != null) _row('Phone', c.phone!),
              if (c.notes != null && c.notes!.isNotEmpty) ...[
                const Divider(height: 24),
                Text('Notes',
                    style: TextStyle(
                        fontSize: 13,
                        color: Theme.of(context).colorScheme.outline)),
                const SizedBox(height: 4),
                Text(c.notes!,
                    style: const TextStyle(fontSize: 14, height: 1.4)),
              ],
            ],
          ),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(context),
              child: const Text('Close')),
        ],
      ),
    );
  }

  Widget _row(String label, String value) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 3),
        child: Row(
          children: [
            SizedBox(
                width: 70,
                child: Text(label,
                    style: TextStyle(
                        fontSize: 13,
                        color: Theme.of(context).colorScheme.outline))),
            Expanded(
                child: Text(value,
                    style: const TextStyle(
                        fontSize: 13, fontWeight: FontWeight.w500))),
          ],
        ),
      );
}
