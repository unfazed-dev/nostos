// Tab 3: the shop's history — every status an order reached, newest first.
//
// This is the push pilot's receipt. A banner is gone the moment it is
// dismissed; the row it came from is not (migration 0007's `order_events`,
// synced like any other table). Tapping a row opens the same detail view a
// tapped notification deep-links to, which is what makes the two testable
// against each other.
//
// The bench run launcher lives in ui/bench.dart now — one tap away in the app
// bar, off the list.
import 'package:flutter/material.dart';

import '../adapters/sync_adapter.dart';
import '../bench/store.dart';
import '../bench/upload.dart';
import '../design/tokens.dart';
import 'bench.dart';
import 'connectivity_led.dart';
import 'history_detail.dart';

/// [events] is the one thing here that comes from the engine, and it arrives
/// as a stream rather than a [SyncAdapter] so a test can hand it a literal
/// list. The bench trio is passed straight through to [BenchScreen].
class HistoryScreen extends StatelessWidget {
  const HistoryScreen({
    super.key,
    required this.events,
    required this.store,
    required this.uploadRuns,
    required this.runSuite,
  });

  /// Newest first. Read-only: the device never writes an order event.
  final Stream<List<OrderEventRow>> events;
  final BenchStore store;
  final RunsUploader uploadRuns;
  final Future<void> Function() runSuite;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      key: const Key('history-screen'),
      backgroundColor: AtletTokens.paper,
      appBar: AppBar(
        title: const Text('History'),
        backgroundColor: AtletTokens.bone,
        actions: [
          IconButton(
            key: const Key('open-bench-button'),
            tooltip: 'Bench',
            icon: const Icon(Icons.speed_outlined),
            onPressed: () => Navigator.of(context).push(
              MaterialPageRoute<void>(
                builder: (_) => BenchScreen(
                  store: store,
                  uploadRuns: uploadRuns,
                  runSuite: runSuite,
                ),
              ),
            ),
          ),
          const ConnectivityLed(),
        ],
      ),
      body: _OrderEventFeed(events: events),
    );
  }
}

/// `983979e8 · paid → shipped`, or `983979e8 · pending` for the row that
/// records the order's creation.
///
/// Pure so the shaping is testable without pumping a widget.
String historyLine(OrderEventRow e) {
  final id = e.orderId.length >= 8 ? e.orderId.substring(0, 8) : e.orderId;
  final from = e.previousStatus;
  return from == null ? '$id · ${e.status}' : '$id · $from → ${e.status}';
}

/// `09-23 16:40` — local time, no date library.
///
/// ponytail: month-day, not a locale-aware format. This is a smoke-test feed
/// read by whoever just flipped a status; add `intl` when a user who is not
/// us reads it.
String historyStamp(DateTime t) {
  final l = t.toLocal();
  String two(int v) => v.toString().padLeft(2, '0');
  return '${two(l.month)}-${two(l.day)} ${two(l.hour)}:${two(l.minute)}';
}

class _OrderEventFeed extends StatelessWidget {
  const _OrderEventFeed({required this.events});

  final Stream<List<OrderEventRow>> events;

  @override
  Widget build(BuildContext context) {
    return StreamBuilder<List<OrderEventRow>>(
      stream: events,
      builder: (context, snapshot) {
        final items = snapshot.data;
        if (items == null) {
          return const Center(child: CircularProgressIndicator());
        }
        if (items.isEmpty) {
          return const Center(
            key: Key('history-feed-empty'),
            child: Text('No order history yet.'),
          );
        }
        return ListView.separated(
          key: const Key('history-feed'),
          itemCount: items.length,
          separatorBuilder: (_, _) => const Divider(height: 1),
          itemBuilder: (context, i) {
            final e = items[i];
            return ListTile(
              key: Key('history-event-${e.id}'),
              dense: true,
              trailing: const Icon(
                Icons.chevron_right,
                color: AtletTokens.ink3,
              ),
              onTap: () => Navigator.of(context).push(
                MaterialPageRoute<void>(
                  builder: (_) =>
                      HistoryDetailScreen(eventId: e.id, events: events),
                ),
              ),
              leading: Icon(_statusIcon(e.status), color: AtletTokens.ink3),
              title: Text(historyLine(e)),
              subtitle: Text(
                e.note == null
                    ? historyStamp(e.createdAt)
                    : '${historyStamp(e.createdAt)} · ${e.note}',
                style: const TextStyle(color: AtletTokens.ink3),
              ),
            );
          },
        );
      },
    );
  }
}

IconData _statusIcon(String status) => switch (status) {
  'paid' => Icons.payments_outlined,
  'shipped' => Icons.local_shipping_outlined,
  'delivered' => Icons.check_circle_outline,
  'failed' => Icons.error_outline,
  _ => Icons.schedule,
};
