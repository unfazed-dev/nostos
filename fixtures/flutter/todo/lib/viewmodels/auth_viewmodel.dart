import 'package:flutter/foundation.dart';

import '../domain/auth_gateway.dart';

/// Auth view-state over the [AuthGateway] port. Never throws to the view —
/// [AuthException]s are translated into [errorMessage]. Mocked in tests via a
/// mocktail [AuthGateway]; backed by [FakeAuthGateway] (mock mode) or
/// [SupabaseAuthGateway] (live mode) in the app.
class AuthViewModel extends ChangeNotifier {
  AuthViewModel(this._auth);

  final AuthGateway _auth;

  Session? _session;
  bool _busy = false;
  String? _errorMessage;

  /// Null when signed out; set on a successful [signIn].
  Session? get session => _session;
  bool get busy => _busy;

  /// Null when there is no error to show. Set on a failed [signIn].
  String? get errorMessage => _errorMessage;

  /// Attempts to sign in. Sets [busy], clears any prior [errorMessage], and on
  /// failure records a message instead of throwing. The view never sees an
  /// exception.
  Future<void> signIn(String email, String password) async {
    _busy = true;
    _errorMessage = null;
    notifyListeners();

    try {
      _session = await _auth.signIn(email, password);
    } on AuthException catch (e) {
      _session = null;
      _errorMessage = switch (e) {
        InvalidCredentials() => 'Invalid email or password',
        NetworkAuthException(:final message) => message,
      };
    } finally {
      _busy = false;
      notifyListeners();
    }
  }

  /// Signs out and clears the local session.
  Future<void> signOut() async {
    await _auth.signOut();
    _session = null;
    notifyListeners();
  }
}
