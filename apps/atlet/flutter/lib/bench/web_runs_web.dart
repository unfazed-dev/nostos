import 'package:web/web.dart' as web;

/// localStorage-backed bench-run persistence for the web build of
/// store.dart — one JSONL string under one key.
String? loadWebRuns(String key) => web.window.localStorage.getItem(key);

void saveWebRuns(String key, String jsonl) =>
    web.window.localStorage.setItem(key, jsonl);
