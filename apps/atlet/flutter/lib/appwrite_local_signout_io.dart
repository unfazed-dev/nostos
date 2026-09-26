import 'package:appwrite/appwrite.dart' as appwrite;
// ignore: implementation_imports -- SDK 27 exposes cookie clearing only here.
import 'package:appwrite/src/client_io.dart' show ClientIO;

/// Clear persisted native Appwrite cookies even when cloud sign-out is offline.
Future<void> clearLocalAppwriteSession(appwrite.Client client) async {
  if (client is! ClientIO) {
    throw StateError('Appwrite native client was not initialized');
  }
  // ponytail: Appwrite SDK 27 has no public cookie-clear method on Client.
  // This internal getter is the only way to remove its persisted session
  // without a network round trip; replace when the SDK exposes a public API.
  await client.cookieJar.deleteAll();
}
