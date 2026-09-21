// Availabilities view — recurring weekly windows, grouped by provider.
//
// Shows each provider's availability windows (weekday + time range). An add
// dialog lets you create a new window: pick a provider, a weekday, and a
// start/end time. Times are stored as minutes-from-midnight (0..1440).

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

import '../../models.dart';
import '../../widgets/connection_badge.dart' show EmptyState;

class AvailabilitiesView extends StatefulWidget {
  const AvailabilitiesView({super.key, required this.db});
  final NostosDatabase db;
  @override
  State<AvailabilitiesView> createState() => _AvailabilitiesViewState();
}

class _AvailabilitiesViewState extends State<AvailabilitiesView> {
  late final _avail = widget.db.collection<Availability>(
    table: 'availabilities',
    fromRow: Availability.fromRow,
  );
  late final Stream<List<Availability>> _rows = _avail.watch();
  late final _providersColl = widget.db.collection<Provider>(
    table: 'providers',
    fromRow: Provider.fromRow,
  );
  late final Stream<List<Provider>> _providers = _providersColl.watch();

  @override
  Widget build(BuildContext context) => Scaffold(
    body: StreamBuilder<List<Availability>>(
      stream: _rows,
      builder: (context, snap) {
        final availabilities = snap.data ?? const [];
        if (availabilities.isEmpty) {
          return const EmptyState(
            icon: Icons.calendar_month_outlined,
            message: 'No availability windows yet.',
          );
        }
        return StreamBuilder<List<Provider>>(
          stream: _providers,
          builder: (context, pSnap) {
            final providerName = {
              for (final p in pSnap.data ?? const <Provider>[]) p.id: p.name,
            };
            // Group by provider.
            final grouped = <String, List<Availability>>{};
            for (final a in availabilities) {
              grouped.putIfAbsent(a.providerId, () => []).add(a);
            }
            // Sort each group by weekday then start_min.
            for (final list in grouped.values) {
              list.sort((a, b) {
                final d = a.weekday.compareTo(b.weekday);
                return d != 0 ? d : a.startMin.compareTo(b.startMin);
              });
            }
            return ListView.builder(
              itemCount: grouped.length,
              itemBuilder: (context, i) {
                final providerId = grouped.keys.elementAt(i);
                final windows = grouped[providerId]!;
                return Card(
                  margin: const EdgeInsets.symmetric(
                    horizontal: 16,
                    vertical: 6,
                  ),
                  child: Padding(
                    padding: const EdgeInsets.all(12),
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(
                          providerName[providerId] ??
                              'Provider ${providerId.substring(0, 8)}…',
                          style: TextStyle(
                            fontSize: 13,
                            fontWeight: FontWeight.w700,
                            color: Theme.of(context).colorScheme.primary,
                          ),
                        ),
                        const SizedBox(height: 8),
                        Wrap(
                          spacing: 8,
                          runSpacing: 6,
                          children: [
                            for (final w in windows)
                              Chip(
                                avatar: Icon(
                                  Icons.access_time_filled,
                                  size: 16,
                                  color: Theme.of(context)
                                      .colorScheme
                                      .onSurfaceVariant,
                                ),
                                label: Text(
                                  w.summary,
                                  style: const TextStyle(
                                    fontSize: 12,
                                    fontWeight: FontWeight.w500,
                                  ),
                                ),
                                side: BorderSide.none,
                              ),
                          ],
                        ),
                      ],
                    ),
                  ),
                );
              },
            );
          },
        );
      },
    ),
  );
}
