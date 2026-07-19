// Appointments view — create + status workflow + optional auto-invoice.
//
// Create an appointment (pick provider + client + time + duration). On
// creation, a checkbox offers to auto-generate an invoice from the provider's
// rate (BillingService calculates the amount). Status actions let you mark an
// appointment completed or cancelled.

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

import '../../models.dart';
import '../../services/billing_service.dart';
import '../../widgets/connection_badge.dart'
    show EmptyState, StatusChip, shortId;

class AppointmentsView extends StatefulWidget {
  const AppointmentsView({super.key, required this.db});
  final NostosDatabase db;
  @override
  State<AppointmentsView> createState() => _AppointmentsViewState();
}

class _AppointmentsViewState extends State<AppointmentsView> {
  late final _appts = widget.db.collection<Appointment>(
      table: 'appointments', fromRow: Appointment.fromRow);
  late final Stream<List<Appointment>> _rows = _appts.watch(orderBy: 'starts_at');
  late final _invoices = widget.db.collection<Invoice>(
      table: 'invoices', fromRow: Invoice.fromRow);

  Future<void> _add() async {
    final form = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (_) => _AppointmentDialog(db: widget.db),
    );
    if (form == null) return;
    final appointmentId = uuidV4();
    final appointment =
        Map<String, dynamic>.from(form['appointment'] as Map);
    appointment['id'] = appointmentId;
    await _appts.upsertRow(appointment);
    // Auto-generate invoice if requested — stamp the real appointment_id now.
    final invoice = form['invoice'];
    if (invoice is Map<String, dynamic>) {
      invoice['appointment_id'] = appointmentId;
      await _invoices.upsertRow({...invoice, 'id': uuidV4()});
    }
  }

  Future<void> _setStatus(Appointment a, String status) async {
    await _appts.patch(a.id, {'status': status});
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        body: StreamBuilder<List<Appointment>>(
          stream: _rows,
          builder: (context, snap) {
            final appts = snap.data ?? const [];
            if (appts.isEmpty) {
              return const EmptyState(
                icon: Icons.event_outlined,
                message: 'No appointments yet. Tap + to book one.',
              );
            }
            return ListView.builder(
              itemCount: appts.length,
              itemBuilder: (context, i) {
                final a = appts[i];
                return Card(
                  margin: const EdgeInsets.symmetric(
                      horizontal: 16, vertical: 6),
                  child: ListTile(
                    leading: const CircleAvatar(
                      child: Icon(Icons.event, size: 20),
                    ),
                    title: Text(a.formattedStart,
                        style: const TextStyle(fontWeight: FontWeight.w600)),
                    subtitle: Padding(
                      padding: const EdgeInsets.only(top: 4),
                      child: Text(
                        'provider ${shortId(a.providerId)}  •  '
                        'client ${shortId(a.clientId)}  •  '
                        '${a.durationMin} min',
                        style: TextStyle(
                            fontSize: 13,
                            color: Theme.of(context)
                                .colorScheme
                                .onSurfaceVariant),
                      ),
                    ),
                    trailing: _statusActions(a),
                    onTap: () => _showDetail(context, a),
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

  Widget _statusActions(Appointment a) {
    if (a.status == AppointmentStatus.confirmed) {
      return Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          StatusChip(status: a.status.label),
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
    return StatusChip(status: a.status.label);
  }

  void _showDetail(BuildContext context, Appointment a) {
    showDialog(
      context: context,
      builder: (_) => AlertDialog(
        title: Text(a.formattedStart),
        content: SizedBox(
          width: 320,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              _row('Duration', '${a.durationMin} min'),
              _row('Status', a.status.label),
              _row('Provider', shortId(a.providerId)),
              _row('Client', shortId(a.clientId)),
              if (a.notes != null && a.notes!.isNotEmpty) ...[
                const Divider(height: 24),
                Text('Notes',
                    style: TextStyle(
                        fontSize: 13,
                        color: Theme.of(context).colorScheme.outline)),
                const SizedBox(height: 4),
                Text(a.notes!,
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
                width: 80,
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

/// Appointment creation dialog with optional auto-invoice.
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
  final _notes = TextEditingController();
  bool _autoInvoice = true;
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
    _notes.dispose();
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
    final duration = int.tryParse(_duration.text.trim()) ?? 30;
    final now = DateTime.now().toUtc().toIso8601String();
    final appointment = <String, dynamic>{
      'provider_id': _providerId,
      'client_id': _clientId,
      'starts_at': _starts.text.trim(),
      'duration_min': duration,
      'status': 'confirmed',
      'created_at': now,
    };

    Map<String, dynamic>? invoice;
    if (_autoInvoice) {
      final provider =
          _providers.where((p) => p.id == _providerId).firstOrNull;
      if (provider != null) {
        final billing = BillingService.calculate(
            provider: provider, durationMinutes: duration);
        invoice = BillingService.buildInvoicePayload(
          appointmentId: '', // filled by caller — see below
          clientId: _clientId!,
          providerId: provider.id,
          billing: billing,
        );
      }
    }

    Navigator.pop(context, <String, dynamic>{
      'appointment': appointment,
      'invoice': invoice,
      '_provider_id': _providerId, // for the caller to stamp appointment_id
    });
  }

  @override
  Widget build(BuildContext context) {
    if (_loading) {
      return const AlertDialog(
          content: SizedBox(
              height: 48,
              width: 48,
              child: Center(child: CircularProgressIndicator())));
    }
    // Estimate the live invoice amount for the checkbox subtitle.
    final provider =
        _providers.where((p) => p.id == _providerId).firstOrNull;
    final duration = int.tryParse(_duration.text.trim()) ?? 30;
    final estimated = provider != null
        ? BillingService.calculateAmount(
            provider: provider, durationMinutes: duration)
        : 0;

    return AlertDialog(
      title: const Text('New appointment'),
      content: SizedBox(
        width: 360,
        child: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              DropdownButtonFormField<String>(
                decoration: const InputDecoration(labelText: 'Provider'),
                initialValue: _providerId,
                items: [
                  for (final p in _providers)
                    DropdownMenuItem(
                      value: p.id,
                      child: Text(
                          '${p.name}${p.specialty == null ? '' : ' — ${p.specialty}'}'),
                    ),
                ],
                onChanged: (v) => setState(() => _providerId = v),
              ),
              const SizedBox(height: 8),
              DropdownButtonFormField<String>(
                decoration: const InputDecoration(labelText: 'Client'),
                initialValue: _clientId,
                items: [
                  for (final c in _clients)
                    DropdownMenuItem(value: c.id, child: Text(c.name)),
                ],
                onChanged: (v) => setState(() => _clientId = v),
              ),
              const SizedBox(height: 8),
              TextField(
                controller: _starts,
                decoration: const InputDecoration(
                    labelText: 'Starts at (ISO 8601)'),
              ),
              const SizedBox(height: 8),
              TextField(
                controller: _duration,
                decoration: const InputDecoration(
                    labelText: 'Duration (min)', suffixText: 'min'),
                keyboardType: TextInputType.number,
                onChanged: (_) => setState(() {}),
              ),
              const SizedBox(height: 8),
              TextField(
                controller: _notes,
                decoration: const InputDecoration(labelText: 'Notes'),
                maxLines: 2,
              ),
              const SizedBox(height: 12),
              if (provider != null)
                CheckboxListTile(
                  dense: true,
                  contentPadding: EdgeInsets.zero,
                  title: const Text('Auto-generate invoice'),
                  subtitle: Text(
                    '${provider.rateType.label} · ${formatCents(estimated)}',
                    style: TextStyle(
                        fontSize: 12,
                        color: Theme.of(context).colorScheme.primary),
                  ),
                  value: _autoInvoice,
                  onChanged: (v) =>
                      setState(() => _autoInvoice = v ?? false),
                ),
            ],
          ),
        ),
      ),
      actions: [
        TextButton(
            onPressed: () => Navigator.pop(context, null),
            child: const Text('Cancel')),
        FilledButton(onPressed: _submit, child: const Text('Create')),
      ],
    );
  }
}
