/// Native stub of the web push arm — see push_pilot_web.dart. Compiled on
/// every non-web build; the conditional import in push_pilot.dart guarantees
/// this is unreachable there.
library;

import '../../adapters/nostos_adapter.dart';

/// No-op: Web Push does not exist off the browser.
Future<void> attachWebPush(NostosAdapter adapter, void Function(String) log) async {}
