// One order event, in full — and everything about the notification it did (or
// did not) produce.
//
// Two audiences on purpose: the user gets "order 983979e8 shipped, then
// delivered, at these times", and whoever is smoke-testing push gets the exact
// payload, the deep link, and whether the platform accepted the post. Reached
// by tapping a History row OR by tapping the notification itself — same screen,
// same id, which is the point: if the deep link is wrong, this screen is what
// disagrees with the row you tapped.
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../adapters/sync_adapter.dart';
import '../design/tokens.dart';
import '../push/order_push.dart';
import 'history.dart';

class HistoryDetailScreen extends StatelessWidget {
  const HistoryDetailScreen({
    super.key,
    required this.eventId,
    required this.events,
  });

  /// Looked up in [events] rather than passed as a row: a deep link arrives
  /// carrying an id and nothing else, and one lookup path means the tapped-row
  /// case cannot silently diverge from the tapped-notification case.
  final String eventId;
  final Stream<List<OrderEventRow>> events;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      key: const Key('history-detail-screen'),
      backgroundColor: AtletTokens.paper,
      appBar: AppBar(
        title: const Text('Order update'),
        backgroundColor: AtletTokens.bone,
      ),
      body: StreamBuilder<List<OrderEventRow>>(
        stream: events,
        builder: (context, snapshot) {
          final items = snapshot.data;
          if (items == null) {
            return const Center(child: CircularProgressIndicator());
          }
          final event = items.where((e) => e.id == eventId).firstOrNull;
          if (event == null) {
            // Reachable for real: a notification for an event this device has
            // not pulled yet, or one pruned past the retention horizon.
            return Center(
              key: const Key('history-detail-missing'),
              child: Padding(
                padding: const EdgeInsets.all(24),
                child: Text(
                  'No order event $eventId on this device yet.',
                  textAlign: TextAlign.center,
                  style: const TextStyle(color: AtletTokens.ink3),
                ),
              ),
            );
          }
          return _Body(event: event);
        },
      ),
    );
  }
}

class _Body extends StatelessWidget {
  const _Body({required this.event});

  final OrderEventRow event;

  @override
  Widget build(BuildContext context) {
    final payload = orderPushPayload(event);
    final json = const JsonEncoder.withIndent('  ').convert(payload);
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        Text(
          historyLine(event),
          key: const Key('history-detail-line'),
          style: const TextStyle(
            fontSize: AtletTokens.title2,
            fontWeight: FontWeight.w600,
          ),
        ),
        const SizedBox(height: 4),
        Text(
          historyStamp(event.createdAt),
          style: const TextStyle(color: AtletTokens.ink3),
        ),
        const SizedBox(height: 20),
        _Card(
          title: 'Event',
          children: [
            _Fact(label: 'Order', value: event.orderId),
            _Fact(label: 'Event id', value: event.id),
            _Fact(label: 'Status', value: event.status),
            _Fact(label: 'Previous', value: event.previousStatus ?? '—'),
            _Fact(
              label: 'Recorded (UTC)',
              value: event.createdAt.toUtc().toIso8601String(),
            ),
            if (event.note != null) _Fact(label: 'Note', value: event.note!),
          ],
        ),
        _Card(
          title: 'Deep link',
          children: [
            _Fact(label: 'In-app', value: historyRoute(event.id)),
            _Fact(label: 'External', value: deepLinkFor(event.id)),
          ],
        ),
        _Card(
          title: 'Push payload',
          trailing: IconButton(
            key: const Key('copy-payload-button'),
            tooltip: 'Copy payload',
            icon: const Icon(Icons.copy_all_outlined, size: 18),
            onPressed: () async {
              await Clipboard.setData(ClipboardData(text: json));
              if (context.mounted) {
                ScaffoldMessenger.of(context).showSnackBar(
                  const SnackBar(content: Text('Payload copied.')),
                );
              }
            },
          ),
          children: [
            SelectableText(
              json,
              key: const Key('history-detail-payload'),
              style: const TextStyle(
                fontFamily: AtletTokens.monoFamily,
                fontSize: AtletTokens.footnote,
              ),
            ),
          ],
        ),
        ValueListenableBuilder<List<PushAttempt>>(
          valueListenable: pushLog,
          builder: (context, all, _) {
            final mine = attemptsFor(all, event.id);
            return _Card(
              title: 'Delivery (this session)',
              children: [
                if (mine.isEmpty)
                  const Text(
                    // Not a failure: the backfilled rows predate this run, and
                    // a banner is only posted for a change seen while running.
                    'No banner posted for this event in this session.',
                    key: Key('history-detail-no-delivery'),
                    style: TextStyle(color: AtletTokens.ink3),
                  ),
                for (final a in mine)
                  _Fact(
                    label: historyStamp(a.at),
                    value: a.error == null
                        ? '${a.channel} — accepted'
                        : '${a.channel} — FAILED: ${a.error}',
                  ),
              ],
            );
          },
        ),
      ],
    );
  }
}

class _Card extends StatelessWidget {
  const _Card({required this.title, required this.children, this.trailing});

  final String title;
  final List<Widget> children;
  final Widget? trailing;

  @override
  Widget build(BuildContext context) {
    return Container(
      margin: const EdgeInsets.only(bottom: 16),
      padding: const EdgeInsets.all(16),
      decoration: BoxDecoration(
        color: AtletTokens.bone,
        borderRadius: BorderRadius.circular(12),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  title,
                  style: const TextStyle(
                    fontWeight: FontWeight.w600,
                    color: AtletTokens.ink3,
                  ),
                ),
              ),
              ?trailing,
            ],
          ),
          const SizedBox(height: 8),
          ...children,
        ],
      ),
    );
  }
}

class _Fact extends StatelessWidget {
  const _Fact({required this.label, required this.value});

  final String label;
  final String value;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 3),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: 110,
            child: Text(label, style: const TextStyle(color: AtletTokens.ink3)),
          ),
          Expanded(child: SelectableText(value)),
        ],
      ),
    );
  }
}
