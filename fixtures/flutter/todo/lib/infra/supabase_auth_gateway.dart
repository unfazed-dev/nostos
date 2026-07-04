import 'package:supabase_flutter/supabase_flutter.dart' hide Session, AuthException;
import 'package:todo/domain/auth_gateway.dart';

/// Supabase-backed [AuthGateway]. Only constructed when [Env.isLive] — the mock
/// app uses [FakeAuthGateway]. Verified against supabase_flutter 2.15.4 /
/// gotrue 2.25.0: [AuthApiException.statusCode] is a `String?` (compared to
/// `'400'`), and [AuthResponse.user] is a nullable `User`.
class SupabaseAuthGateway implements AuthGateway {
  SupabaseAuthGateway(this._client);
  final SupabaseClient _client;

  @override
  Future<Session?> restore() async {
    final s = _client.auth.currentSession;
    final u = s?.user;
    return u == null ? null : Session(userId: u.id, email: u.email ?? '');
  }

  @override
  Future<Session> signIn(String email, String password) =>
      _withRetry(() async {
        try {
          final res = await _client.auth
              .signInWithPassword(email: email, password: password);
          final u = res.user!;
          return Session(userId: u.id, email: u.email ?? '');
        } on AuthApiException catch (e) {
          // Wrong creds are NOT retried — only transport failures are.
          throw e.statusCode == '400'
              ? const AuthException.invalidCredentials()
              : AuthException.network(e.message);
        }
      });

  @override
  Future<void> signOut() => _client.auth.signOut();

  /// ponytail: 2 retries, 1s/2s backoff, network errors only — enough to
  /// absorb transient Supabase 5xx in the live smoke; a real app grows a
  /// proper retry policy with the SDK retrofit.
  Future<T> _withRetry<T>(Future<T> Function() op) async {
    for (var attempt = 0;; attempt++) {
      try {
        return await op();
      } on NetworkAuthException {
        if (attempt >= 2) rethrow;
        await Future<void>.delayed(Duration(seconds: 1 << attempt));
      }
    }
  }
}
