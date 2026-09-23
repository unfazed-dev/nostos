// Chat view — realtime messaging via the synced `messages` table.
//
// This is the 2026 local-first best practice: a synced table IS the
// realtime stream. No
// separate WebSocket. Messages flow through nostos replication like any other
// row; the chat view watches the table reactively via db.collection<Message>,
// and sending a message writes to the local outbox → it round-trips back
// through the watch once the server applies it.
//
// UI: a thread list (provider↔client pairs, derived client-side from the
// messages stream). Tapping a thread opens the message bubbles view. A send
// bar at the bottom writes new messages. For the demo, the sender toggle
// switches between "as provider" and "as client" (a real app would fix this
// to the signed-in user).

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

import '../../nostos.g.dart' as gen;
import '../../models.dart';
import '../../widgets/connection_badge.dart' show EmptyState, shortId;

class ChatView extends StatefulWidget {
  const ChatView({super.key, required this.db});
  final NostosDatabase db;
  @override
  State<ChatView> createState() => _ChatViewState();
}

class _ChatViewState extends State<ChatView> {
  late final _messagesColl = widget.db.collection<Message>(
      table: 'messages', fromRow: Message.fromRow);
  late final Stream<List<Message>> _messages = _messagesColl.watch();
  // Typed write image (ADR-0024 Option C). Read collection stays over the
  // presentation Message (timeLabel/isFromProvider getters).
  late final _messagesWrite = widget.db.collection<gen.Message>(
      table: 'messages',
      fromRow: gen.Message.fromRow,
      toRow: (m) => m.toPayload());
  late final _providersColl = widget.db.collection<Provider>(
      table: 'providers', fromRow: Provider.fromRow);
  late final Stream<List<Provider>> _providers = _providersColl.watch();
  late final _clientsColl = widget.db.collection<Client>(
      table: 'clients', fromRow: Client.fromRow);
  late final Stream<List<Client>> _clients = _clientsColl.watch();

  ChatThread? _openThread;

  @override
  Widget build(BuildContext context) {
    return StreamBuilder<List<Message>>(
      stream: _messages,
      builder: (context, msgSnap) {
        final messages = msgSnap.data ?? const <Message>[];
        return StreamBuilder<List<Provider>>(
          stream: _providers,
          builder: (context, pSnap) {
            final providerName = {
              for (final p in pSnap.data ?? const <Provider>[]) p.id: p.name,
            };
            return StreamBuilder<List<Client>>(
              stream: _clients,
              builder: (context, cSnap) {
                final clientName = {
                  for (final c in cSnap.data ?? const <Client>[]) c.id: c.name,
                };
                // Derive threads from messages.
                final threads = _deriveThreads(messages);
                if (_openThread != null) {
                  return _threadView(
                      context, _openThread!, messages, providerName, clientName);
                }
                if (threads.isEmpty) {
                  return const EmptyState(
                    icon: Icons.chat_bubble_outline,
                    message: 'No conversations yet.',
                  );
                }
                return _threadList(context, threads, providerName, clientName);
              },
            );
          },
        );
      },
    );
  }

  /// Group messages into threads by (provider_id, client_id), tracking the
  /// last message + count per thread.
  List<ChatThread> _deriveThreads(List<Message> messages) {
    final byKey = <String, List<Message>>{};
    for (final m in messages) {
      byKey.putIfAbsent('${m.providerId}:${m.clientId}', () => []).add(m);
    }
    final threads = <ChatThread>[];
    for (final entry in byKey.entries) {
      final sorted = entry.value
        ..sort((a, b) => a.createdAt.compareTo(b.createdAt));
      if (sorted.isEmpty) continue;
      threads.add(ChatThread(
        providerId: sorted.last.providerId,
        clientId: sorted.last.clientId,
        lastMessage: sorted.last,
        messageCount: sorted.length,
      ));
    }
    threads.sort((a, b) =>
        b.lastMessage.createdAt.compareTo(a.lastMessage.createdAt));
    return threads;
  }

  Widget _threadList(
    BuildContext context,
    List<ChatThread> threads,
    Map<String, String> providerNames,
    Map<String, String> clientNames,
  ) {
    return ListView.builder(
      itemCount: threads.length,
      itemBuilder: (context, i) {
        final t = threads[i];
        final pName = providerNames[t.providerId] ?? shortId(t.providerId);
        final cName = clientNames[t.clientId] ?? shortId(t.clientId);
        return Card(
          margin: const EdgeInsets.symmetric(horizontal: 16, vertical: 6),
          child: ListTile(
            leading: const CircleAvatar(child: Icon(Icons.forum, size: 20)),
            title: Text('$pName ↔ $cName',
                style: const TextStyle(fontWeight: FontWeight.w600)),
            subtitle: Text(
              t.lastMessage.body,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(
                  fontSize: 13,
                  color:
                      Theme.of(context).colorScheme.onSurfaceVariant),
            ),
            trailing: Column(
              mainAxisAlignment: MainAxisAlignment.center,
              crossAxisAlignment: CrossAxisAlignment.end,
              children: [
                Text(t.lastMessage.timeLabel,
                    style: TextStyle(
                        fontSize: 11,
                        color: Theme.of(context).colorScheme.outline)),
                const SizedBox(height: 2),
                Container(
                  padding:
                      const EdgeInsets.symmetric(horizontal: 6, vertical: 1),
                  decoration: BoxDecoration(
                    color: Theme.of(context)
                        .colorScheme
                        .secondaryContainer
                        .withValues(alpha: 0.6),
                    borderRadius: BorderRadius.circular(10),
                  ),
                  child: Text('${t.messageCount}',
                      style: TextStyle(
                          fontSize: 11,
                          fontWeight: FontWeight.w600,
                          color: Theme.of(context)
                              .colorScheme
                              .onSecondaryContainer)),
                ),
              ],
            ),
            onTap: () => setState(() => _openThread = t),
          ),
        );
      },
    );
  }

  Widget _threadView(
    BuildContext context,
    ChatThread thread,
    List<Message> allMessages,
    Map<String, String> providerNames,
    Map<String, String> clientNames,
  ) {
    final threadMessages = allMessages
        .where((m) =>
            m.providerId == thread.providerId &&
            m.clientId == thread.clientId)
        .toList()
      ..sort((a, b) => a.createdAt.compareTo(b.createdAt));
    final pName = providerNames[thread.providerId] ?? 'Provider';
    final cName = clientNames[thread.clientId] ?? 'Client';

    return _MessageThread(
      providerName: pName,
      clientName: cName,
      providerId: thread.providerId,
      clientId: thread.clientId,
      messages: threadMessages,
      onBack: () => setState(() => _openThread = null),
      onSend: (body, asProvider) => _send(
          thread.providerId, thread.clientId, body, asProvider),
    );
  }

  Future<void> _send(
      String providerId, String clientId, String body, bool asProvider) async {
    await _messagesWrite.upsert(gen.Message(
      id: uuidV4(),
      providerId: providerId,
      clientId: clientId,
      senderType: asProvider ? 'provider' : 'client',
      senderId: asProvider ? providerId : clientId,
      body: body,
      createdAt: DateTime.now().toUtc().toIso8601String(),
    ));
  }
}

/// The message-bubbles view for a single thread.
class _MessageThread extends StatefulWidget {
  const _MessageThread({
    required this.providerName,
    required this.clientName,
    required this.providerId,
    required this.clientId,
    required this.messages,
    required this.onBack,
    required this.onSend,
  });

  final String providerName;
  final String clientName;
  final String providerId;
  final String clientId;
  final List<Message> messages;
  final VoidCallback onBack;
  final Future<void> Function(String body, bool asProvider) onSend;

  @override
  State<_MessageThread> createState() => _MessageThreadState();
}

class _MessageThreadState extends State<_MessageThread> {
  final _controller = TextEditingController();
  final _scrollController = ScrollController();
  // Demo toggle: send as provider (true) or client (false). A real app fixes
  // this to the signed-in user.
  bool _asProvider = true;

  @override
  void dispose() {
    _controller.dispose();
    _scrollController.dispose();
    super.dispose();
  }

  void _send() async {
    final body = _controller.text.trim();
    if (body.isEmpty) return;
    _controller.clear();
    await widget.onSend(body, _asProvider);
    // Jump to bottom after the message round-trips.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_scrollController.hasClients) {
        _scrollController.animateTo(
          _scrollController.position.maxScrollExtent,
          duration: const Duration(milliseconds: 200),
          curve: Curves.easeOut,
        );
      }
    });
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Column(
      children: [
        // Thread header.
        Container(
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 8),
          decoration: BoxDecoration(
            color: scheme.surfaceContainerHighest,
            border: Border(bottom: BorderSide(color: scheme.outlineVariant)),
          ),
          child: Row(
            children: [
              IconButton(
                icon: const Icon(Icons.arrow_back),
                onPressed: widget.onBack,
              ),
              const SizedBox(width: 4),
              const Icon(Icons.forum, size: 20),
              const SizedBox(width: 8),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      '${widget.providerName} ↔ ${widget.clientName}',
                      style: const TextStyle(fontWeight: FontWeight.w600),
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                    ),
                  ],
                ),
              ),
              // Sender toggle (demo only).
              SegmentedButton<bool>(
                style: const ButtonStyle(
                    visualDensity: VisualDensity(horizontal: -3, vertical: -2)),
                segments: const [
                  ButtonSegment(value: true, label: Text('Dr')),
                  ButtonSegment(value: false, label: Text('Pt')),
                ],
                selected: {_asProvider},
                onSelectionChanged: (s) =>
                    setState(() => _asProvider = s.first),
              ),
            ],
          ),
        ),
        // Message bubbles.
        Expanded(
          child: widget.messages.isEmpty
              ? const Center(child: Text('Send the first message'))
              : ListView.builder(
                  controller: _scrollController,
                  padding: const EdgeInsets.symmetric(
                      horizontal: 12, vertical: 8),
                  itemCount: widget.messages.length,
                  itemBuilder: (context, i) {
                    final m = widget.messages[i];
                    final isMe =
                        m.isFromProvider == _asProvider;
                    return _MessageBubble(message: m, isMe: isMe);
                  },
                ),
        ),
        // Send bar.
        SafeArea(
          child: Container(
            padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 6),
            decoration: BoxDecoration(
              color: scheme.surfaceContainerHighest,
              border:
                  Border(top: BorderSide(color: scheme.outlineVariant)),
            ),
            child: Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: _controller,
                    decoration: InputDecoration(
                      hintText: 'Message as ${_asProvider ? widget.providerName : widget.clientName}…',
                      isDense: true,
                      filled: true,
                      border: OutlineInputBorder(
                        borderRadius: BorderRadius.circular(24),
                        borderSide: BorderSide.none,
                      ),
                    ),
                    textInputAction: TextInputAction.send,
                    onSubmitted: (_) => _send(),
                  ),
                ),
                const SizedBox(width: 8),
                IconButton.filled(
                  icon: const Icon(Icons.send, size: 18),
                  onPressed: _send,
                ),
              ],
            ),
          ),
        ),
      ],
    );
  }
}

/// A single chat bubble — right-aligned for "me", left for "them".
class _MessageBubble extends StatelessWidget {
  const _MessageBubble({required this.message, required this.isMe});
  final Message message;
  final bool isMe;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final align =
        isMe ? CrossAxisAlignment.end : CrossAxisAlignment.start;
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 3),
      child: Column(
        crossAxisAlignment: align,
        children: [
          Container(
            constraints: BoxConstraints(
              maxWidth: MediaQuery.of(context).size.width * 0.72,
            ),
            padding: const EdgeInsets.symmetric(
                horizontal: 14, vertical: 10),
            decoration: BoxDecoration(
              color: isMe ? scheme.primary : scheme.surfaceContainerHighest,
              borderRadius: BorderRadius.only(
                topLeft: const Radius.circular(16),
                topRight: const Radius.circular(16),
                bottomLeft:
                    isMe ? const Radius.circular(16) : Radius.zero,
                bottomRight:
                    isMe ? Radius.zero : const Radius.circular(16),
              ),
            ),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  message.body,
                  style: TextStyle(
                    color: isMe ? scheme.onPrimary : scheme.onSurface,
                    fontSize: 14,
                    height: 1.35,
                  ),
                ),
                const SizedBox(height: 4),
                Text(
                  message.timeLabel,
                  style: TextStyle(
                    color: isMe
                        ? scheme.onPrimary.withValues(alpha: 0.7)
                        : scheme.onSurfaceVariant.withValues(alpha: 0.7),
                    fontSize: 10,
                  ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}
