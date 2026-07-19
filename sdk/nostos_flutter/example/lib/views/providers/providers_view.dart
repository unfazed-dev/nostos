// Providers view — list + add + rate management.
//
// Shows each provider with their specialty, rate badge (color-coded by
// rate_type), and contact info. The edit dialog lets a provider set/change
// their rate_type and all three rate values (hourly/flat/subscription). The
// rate_type governs how invoices are auto-calculated (BillingService).

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

import '../../models.dart';
import '../../widgets/connection_badge.dart' show InitialsAvatar, EmptyState;
import '../../widgets/form_dialogs.dart';
import '../dashboard_shell.dart';

class ProvidersView extends StatefulWidget {
  const ProvidersView({super.key, required this.db, required this.write});
  final NostosDatabase db;
  final NostosWrite write;
  @override
  State<ProvidersView> createState() => _ProvidersViewState();
}

class _ProvidersViewState extends State<ProvidersView> {
  late final Stream<List<Provider>> _rows = widget.db
      .watchMapped<Provider>('SELECT * FROM providers', Provider.fromRow);

  Future<void> _add() async {
    final form = await showFormDialog(
      context,
      title: 'New provider',
      fields: const [
        DialogField(key: 'name', label: 'Name'),
        DialogField(key: 'specialty', label: 'Specialty'),
        DialogField(key: 'email', label: 'Email'),
        DialogField(key: 'phone', label: 'Phone'),
      ],
      saveLabel: 'Create',
    );
    if (form == null || form['name'] == null) return;
    await widget.write(
      table: 'providers',
      op: 'upsert',
      pk: uuidV4(),
      payload: {
        ...form,
        'rate_type': 'hourly',
        'hourly_rate_cents': 0,
        'flat_rate_cents': 0,
        'subscription_rate_cents': 0,
        'created_at': DateTime.now().toUtc().toIso8601String(),
      },
    );
  }

  Future<void> _editRates(Provider p) async {
    final result = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (_) => _RateEditDialog(provider: p),
    );
    if (result == null) return;
    await widget.write(
      table: 'providers',
      op: 'patch',
      pk: p.id,
      payload: result,
    );
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        body: StreamBuilder<List<Provider>>(
          stream: _rows,
          builder: (context, snap) {
            final providers = snap.data ?? const [];
            if (providers.isEmpty) {
              return const EmptyState(
                icon: Icons.medical_services_outlined,
                message: 'No providers yet. Tap + to add one.',
              );
            }
            return ListView.builder(
              itemCount: providers.length,
              itemBuilder: (context, i) {
                final p = providers[i];
                return Card(
                  margin: const EdgeInsets.symmetric(
                      horizontal: 16, vertical: 6),
                  child: ListTile(
                    leading: InitialsAvatar(
                      initials: p.initials,
                      color: Color(p.avatarColorValue),
                    ),
                    title: Text(p.name,
                        style: const TextStyle(fontWeight: FontWeight.w600)),
                    subtitle: Padding(
                      padding: const EdgeInsets.only(top: 4),
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          if (p.specialty != null)
                            Text(p.specialty!,
                                style: TextStyle(
                                    fontSize: 13,
                                    color: Theme.of(context)
                                        .colorScheme
                                        .onSurfaceVariant)),
                          const SizedBox(height: 6),
                          RateBadgeInline(provider: p),
                        ],
                      ),
                    ),
                    trailing: IconButton(
                      tooltip: 'Edit rates',
                      icon: const Icon(Icons.tune, size: 20),
                      onPressed: () => _editRates(p),
                    ),
                    onTap: () => _showDetail(context, p),
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

  void _showDetail(BuildContext context, Provider p) {
    showDialog(
      context: context,
      builder: (_) => AlertDialog(
        title: Row(
          children: [
            InitialsAvatar(
                initials: p.initials, color: Color(p.avatarColorValue)),
            const SizedBox(width: 12),
            Expanded(child: Text(p.name)),
          ],
        ),
        content: SizedBox(
          width: 320,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              if (p.specialty != null) _detailRow('Specialty', p.specialty!),
              if (p.email != null) _detailRow('Email', p.email!),
              if (p.phone != null) _detailRow('Phone', p.phone!),
              const Divider(height: 24),
              _detailRow('Rate type', p.rateType.label),
              _detailRow('Active rate', p.rateLabel),
              if (p.bio != null) ...[
                const SizedBox(height: 8),
                Text(p.bio!,
                    style: TextStyle(
                        fontSize: 13,
                        color: Theme.of(context).colorScheme.onSurfaceVariant)),
              ],
            ],
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Close'),
          ),
          FilledButton.tonalIcon(
            icon: const Icon(Icons.tune, size: 18),
            label: const Text('Edit rates'),
            onPressed: () {
              Navigator.pop(context);
              _editRates(p);
            },
          ),
        ],
      ),
    );
  }

  Widget _detailRow(String label, String value) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 3),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            SizedBox(
              width: 90,
              child: Text(label,
                  style: TextStyle(
                      fontSize: 13,
                      color: Theme.of(context).colorScheme.outline)),
            ),
            Expanded(
                child: Text(value,
                    style: const TextStyle(
                        fontSize: 13, fontWeight: FontWeight.w500))),
          ],
        ),
      );
}

/// Inline rate badge (separate widget to avoid import cycle with the main
/// connection_badge file which exports the full RateBadge).
class RateBadgeInline extends StatelessWidget {
  const RateBadgeInline({super.key, required this.provider});
  final Provider provider;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final bg = switch (provider.rateType) {
      RateType.hourly => scheme.primaryContainer,
      RateType.flat => const Color(0xFFFFE0B2),
      RateType.subscription => const Color(0xFFE1BEE7),
    };
    final fg = switch (provider.rateType) {
      RateType.hourly => scheme.onPrimaryContainer,
      RateType.flat => Colors.brown.shade900,
      RateType.subscription => Colors.purple.shade900,
    };
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 4),
      decoration: BoxDecoration(
          color: bg, borderRadius: BorderRadius.circular(20)),
      child: Text(
        '${provider.rateType.label} · ${provider.rateLabel}',
        style: TextStyle(
            fontSize: 12, fontWeight: FontWeight.w600, color: fg),
      ),
    );
  }
}

/// Rate editor dialog — lets a provider set their rate_type + all three rate
/// values. The rate_type determines which rate is "active" for billing.
class _RateEditDialog extends StatefulWidget {
  const _RateEditDialog({required this.provider});
  final Provider provider;
  @override
  State<_RateEditDialog> createState() => _RateEditDialogState();
}

class _RateEditDialogState extends State<_RateEditDialog> {
  late RateType _rateType = widget.provider.rateType;
  late final _hourly =
      TextEditingController(text: (widget.provider.hourlyRateCents / 100).toStringAsFixed(2));
  late final _flat =
      TextEditingController(text: (widget.provider.flatRateCents / 100).toStringAsFixed(2));
  late final _sub = TextEditingController(
      text: (widget.provider.subscriptionRateCents / 100).toStringAsFixed(2));

  @override
  void dispose() {
    _hourly.dispose();
    _flat.dispose();
    _sub.dispose();
    super.dispose();
  }

  int _parseDollars(TextEditingController c) =>
      ((double.tryParse(c.text.trim()) ?? 0) * 100).round();

  void _submit() {
    Navigator.pop(context, <String, dynamic>{
      'rate_type': _rateType.name,
      'hourly_rate_cents': _parseDollars(_hourly),
      'flat_rate_cents': _parseDollars(_flat),
      'subscription_rate_cents': _parseDollars(_sub),
    });
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: Text('${widget.provider.name} — rates'),
      content: SizedBox(
        width: 340,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            const Text('Billing model determines how invoices auto-calculate:',
                style: TextStyle(fontSize: 12)),
            const SizedBox(height: 8),
            SegmentedButton<RateType>(
              segments: const [
                ButtonSegment(
                    value: RateType.hourly,
                    label: Text('Hourly'),
                    icon: Icon(Icons.schedule, size: 18)),
                ButtonSegment(
                    value: RateType.flat,
                    label: Text('Flat'),
                    icon: Icon(Icons.looks_one, size: 18)),
                ButtonSegment(
                    value: RateType.subscription,
                    label: Text('Sub'),
                    icon: Icon(Icons.autorenew, size: 18)),
              ],
              selected: {_rateType},
              onSelectionChanged: (s) => setState(() => _rateType = s.first),
            ),
            const SizedBox(height: 16),
            TextField(
              controller: _hourly,
              decoration: const InputDecoration(
                  labelText: 'Hourly rate (\$/hr)',
                  prefixText: '\$ ',
                  suffixText: '/hr'),
              keyboardType:
                  const TextInputType.numberWithOptions(decimal: true),
            ),
            const SizedBox(height: 8),
            TextField(
              controller: _flat,
              decoration: const InputDecoration(
                  labelText: 'Flat fee (\$/visit)',
                  prefixText: '\$ ',
                  suffixText: '/visit'),
              keyboardType:
                  const TextInputType.numberWithOptions(decimal: true),
            ),
            const SizedBox(height: 8),
            TextField(
              controller: _sub,
              decoration: const InputDecoration(
                  labelText: 'Subscription (\$/month)',
                  prefixText: '\$ ',
                  suffixText: '/mo'),
              keyboardType:
                  const TextInputType.numberWithOptions(decimal: true),
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
            onPressed: () => Navigator.pop(context, null),
            child: const Text('Cancel')),
        FilledButton(onPressed: _submit, child: const Text('Save rates')),
      ],
    );
  }
}
