// Invoices view — list + create (auto-calc from provider rate) + mark paid.
//
// Create an invoice by selecting an appointment; the amount is AUTO-CALCULATED
// from the provider's rate type × the appointment duration (BillingService),
// and the rate is SNAPSHOTTED into the invoice row so it never changes if the
// provider updates their rate later. The list shows each invoice's amount,
// status, and line breakdown (rate × hours). Mark-as-paid patches the status.

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

import '../../models.dart';
import '../../services/billing_service.dart';
import '../../widgets/connection_badge.dart'
    show EmptyState, StatusChip;

class InvoicesView extends StatefulWidget {
  const InvoicesView({super.key, required this.db});
  final NostosDatabase db;
  @override
  State<InvoicesView> createState() => _InvoicesViewState();
}

class _InvoicesViewState extends State<InvoicesView> {
  late final _invoices = widget.db.collection<Invoice>(
      table: 'invoices', fromRow: Invoice.fromRow);
  late final Stream<List<Invoice>> _rows = _invoices.watch();

  Future<void> _add() async {
    final payload = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (_) => _InvoiceDialog(db: widget.db),
    );
    if (payload == null) return;
    await _invoices.upsertRow({...payload, 'id': uuidV4()});
  }

  Future<void> _markPaid(Invoice inv) async {
    await _invoices.patch(inv.id, {
      'status': 'paid',
      'paid_at': DateTime.now().toUtc().toIso8601String(),
    });
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        body: StreamBuilder<List<Invoice>>(
          stream: _rows,
          builder: (context, snap) {
            final invoices = snap.data ?? const [];
            if (invoices.isEmpty) {
              return const EmptyState(
                icon: Icons.receipt_long_outlined,
                message: 'No invoices yet. Tap + to create one.',
              );
            }
            return ListView.builder(
              itemCount: invoices.length,
              itemBuilder: (context, i) {
                final inv = invoices[i];
                return Card(
                  margin: const EdgeInsets.symmetric(
                      horizontal: 16, vertical: 6),
                  child: ListTile(
                    leading: const CircleAvatar(
                      child: Icon(Icons.receipt_long, size: 20),
                    ),
                    title: Row(
                      children: [
                        Text(inv.amount,
                            style: const TextStyle(
                                fontWeight: FontWeight.w700, fontSize: 16)),
                        const SizedBox(width: 8),
                        StatusChip(
                            status: inv.status.label,
                            positive: inv.status == InvoiceStatus.paid),
                      ],
                    ),
                    subtitle: Padding(
                      padding: const EdgeInsets.only(top: 4),
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(inv.lineSummary,
                              style: TextStyle(
                                  fontSize: 13,
                                  color: Theme.of(context)
                                      .colorScheme
                                      .onSurfaceVariant)),
                          if (inv.description != null)
                            Text(inv.description!,
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: TextStyle(
                                    fontSize: 12,
                                    color: Theme.of(context)
                                        .colorScheme
                                        .outline)),
                        ],
                      ),
                    ),
                    trailing: inv.status == InvoiceStatus.issued
                        ? IconButton(
                            tooltip: 'Mark paid',
                            icon: const Icon(Icons.check_circle_outline,
                                size: 20),
                            onPressed: () => _markPaid(inv),
                          )
                        : null,
                    onTap: () => _showDetail(context, inv),
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

  void _showDetail(BuildContext context, Invoice inv) {
    showDialog(
      context: context,
      builder: (_) => AlertDialog(
        title: Text('Invoice ${inv.amount}'),
        content: SizedBox(
          width: 340,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              _row('Status', inv.status.label),
              _row('Line type', inv.lineType.label),
              _row('Rate', inv.rateFormatted),
              if (inv.hoursMin > 0) _row('Hours', inv.hoursFormatted),
              const Divider(height: 24),
              _row('Amount', inv.amount),
              if (inv.issuedAt != null)
                _row('Issued', _shortDate(inv.issuedAt!)),
              if (inv.dueAt != null) _row('Due', _shortDate(inv.dueAt!)),
              if (inv.paidAt != null) _row('Paid', _shortDate(inv.paidAt!)),
              if (inv.description != null) ...[
                const Divider(height: 24),
                Text(inv.description!,
                    style: TextStyle(
                        fontSize: 13, color: Theme.of(context).colorScheme.onSurfaceVariant)),
              ],
            ],
          ),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(context),
              child: const Text('Close')),
          if (inv.status == InvoiceStatus.issued)
            FilledButton.tonalIcon(
              icon: const Icon(Icons.check, size: 18),
              label: const Text('Mark paid'),
              onPressed: () {
                Navigator.pop(context);
                _markPaid(inv);
              },
            ),
        ],
      ),
    );
  }

  String _shortDate(String iso) {
    final d = DateTime.tryParse(iso);
    if (d == null) return iso;
    return '${d.month}/${d.day}/${d.year}';
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

/// Invoice creation dialog — picks an appointment and AUTO-CALCULATES the amount
/// from the provider's rate × the appointment duration (BillingService).
class _InvoiceDialog extends StatefulWidget {
  const _InvoiceDialog({required this.db});
  final NostosDatabase db;
  @override
  State<_InvoiceDialog> createState() => _InvoiceDialogState();
}

class _InvoiceDialogState extends State<_InvoiceDialog> {
  List<Appointment> _appts = const [];
  List<Client> _clients = const [];
  List<Provider> _providers = const [];
  String? _apptId;
  bool _loading = true;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    final as_ = await widget.db.getAll('SELECT * FROM appointments');
    final cs = await widget.db.getAll('SELECT * FROM clients');
    final ps = await widget.db.getAll('SELECT * FROM providers');
    if (!mounted) return;
    setState(() {
      _appts = as_.map(Appointment.fromRow).toList();
      _clients = cs.map(Client.fromRow).toList();
      _providers = ps.map(Provider.fromRow).toList();
      _apptId = _appts.isEmpty ? null : _appts.first.id;
      _loading = false;
    });
  }

  void _submit() {
    final appt = _appts.where((a) => a.id == _apptId).firstOrNull;
    if (appt == null) return;
    final provider =
        _providers.where((p) => p.id == appt.providerId).firstOrNull;
    if (provider == null) return;

    final billing = BillingService.calculate(
        provider: provider, durationMinutes: appt.durationMin);
    final payload = BillingService.buildInvoicePayload(
      appointmentId: appt.id,
      clientId: appt.clientId,
      providerId: provider.id,
      billing: billing,
    );
    Navigator.pop(context, payload);
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
    if (_appts.isEmpty) {
      return AlertDialog(
        title: const Text('No appointments'),
        content: const Text(
            'Create an appointment first — invoices are generated from appointments.'),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(context, null),
              child: const Text('OK')),
        ],
      );
    }

    // Live preview of the auto-calculated amount.
    final selectedAppt =
        _appts.where((a) => a.id == _apptId).firstOrNull;
    final provider = selectedAppt != null
        ? _providers.where((p) => p.id == selectedAppt.providerId).firstOrNull
        : null;
    final client = selectedAppt != null
        ? _clients.where((c) => c.id == selectedAppt.clientId).firstOrNull
        : null;
    final preview = (selectedAppt != null && provider != null)
        ? BillingService.calculate(
            provider: provider, durationMinutes: selectedAppt.durationMin)
        : null;

    return AlertDialog(
      title: const Text('New invoice'),
      content: SizedBox(
        width: 360,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            DropdownButtonFormField<String>(
              decoration: const InputDecoration(labelText: 'Appointment'),
              initialValue: _apptId,
              items: [
                for (final a in _appts)
                  DropdownMenuItem(
                    value: a.id,
                    child: Text(
                        '${a.formattedStart} (${a.durationMin}min)'),
                  ),
              ],
              onChanged: (v) => setState(() => _apptId = v),
            ),
            const SizedBox(height: 16),
            if (preview != null && provider != null) ...[
              Container(
                width: double.infinity,
                padding: const EdgeInsets.all(12),
                decoration: BoxDecoration(
                  color: Theme.of(context)
                      .colorScheme
                      .primaryContainer
                      .withValues(alpha: 0.5),
                  borderRadius: BorderRadius.circular(8),
                ),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text('Auto-calculated',
                        style: TextStyle(
                            fontSize: 11,
                            fontWeight: FontWeight.w700,
                            color: Theme.of(context)
                                .colorScheme
                                .onPrimaryContainer)),
                    const SizedBox(height: 8),
                    _previewRow('Provider', provider.name),
                    if (client != null) _previewRow('Client', client.name),
                    _previewRow('Rate model',
                        '${provider.rateType.label} · ${provider.rateLabel}'),
                    _previewRow('Duration',
                        '${selectedAppt!.durationMin} min'),
                    if (preview.hoursMin > 0)
                      _previewRow(
                          'Hours', formatHours(preview.hoursMin)),
                    const Divider(height: 16),
                    Row(
                      mainAxisAlignment: MainAxisAlignment.spaceBetween,
                      children: [
                        const Text('Total',
                            style: TextStyle(
                                fontWeight: FontWeight.w700)),
                        Text(formatCents(preview.amountCents),
                            style: TextStyle(
                                fontSize: 18,
                                fontWeight: FontWeight.w800,
                                color: Theme.of(context)
                                    .colorScheme
                                    .primary)),
                      ],
                    ),
                  ],
                ),
              ),
            ],
          ],
        ),
      ),
      actions: [
        TextButton(
            onPressed: () => Navigator.pop(context, null),
            child: const Text('Cancel')),
        FilledButton(onPressed: _submit, child: const Text('Create invoice')),
      ],
    );
  }

  Widget _previewRow(String label, String value) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 2),
        child: Row(
          mainAxisAlignment: MainAxisAlignment.spaceBetween,
          children: [
            Text(label,
                style: TextStyle(
                    fontSize: 12,
                    color: Theme.of(context).colorScheme.outline)),
            Text(value,
                style: const TextStyle(
                    fontSize: 12, fontWeight: FontWeight.w500)),
          ],
        ),
      );
}
