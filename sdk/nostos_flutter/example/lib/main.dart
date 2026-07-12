import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

/// A minimal example: connect to a local `nostos-server`, subscribe to
/// `tasks`, and render whatever rows show up. Run a server first:
/// `cargo run -p nostos-server` (zero-setup default: fake replicator, no
/// auth, ws://127.0.0.1:8800/sync).
void main() {
  runApp(const NostosExampleApp());
}

class NostosExampleApp extends StatelessWidget {
  const NostosExampleApp({super.key});

  @override
  Widget build(BuildContext context) {
    return const MaterialApp(home: TasksPage());
  }
}

class TasksPage extends StatefulWidget {
  const TasksPage({super.key});

  @override
  State<TasksPage> createState() => _TasksPageState();
}

class _TasksPageState extends State<TasksPage> {
  Nostos? _nostos;
  NostosConnectionState _state = NostosConnectionState.disconnected;

  @override
  void initState() {
    super.initState();
    _connect();
  }

  Future<void> _connect() async {
    final nostos = await Nostos.connect(url: 'ws://127.0.0.1:8800/sync');
    await nostos.subscribe('tasks');
    nostos.connectionState.listen((s) {
      if (mounted) setState(() => _state = s);
    });
    if (mounted) setState(() => _nostos = nostos);
  }

  @override
  Widget build(BuildContext context) {
    final nostos = _nostos;
    return Scaffold(
      appBar: AppBar(title: Text('nostos_flutter example — ${_state.name}')),
      body: nostos == null
          ? const Center(child: CircularProgressIndicator())
          : StreamBuilder<List<Map<String, dynamic>>>(
              stream: nostos.watch('tasks'),
              builder: (context, snapshot) {
                final rows = snapshot.data ?? const <Map<String, dynamic>>[];
                if (rows.isEmpty) {
                  return const Center(child: Text('No rows yet'));
                }
                return ListView.builder(
                  itemCount: rows.length,
                  itemBuilder: (context, i) => ListTile(
                    title: Text(rows[i]['_pk']?.toString() ?? '?'),
                    subtitle: Text(rows[i].toString()),
                  ),
                );
              },
            ),
    );
  }
}
