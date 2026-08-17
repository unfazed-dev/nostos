/// VM stub for the conditional import in store.dart — localStorage exists
/// only in the browser, and `BenchStore`'s kIsWeb gate means these are never
/// called off the web build.
String? loadWebRuns(String key) => null;

void saveWebRuns(String key, String jsonl) {}
