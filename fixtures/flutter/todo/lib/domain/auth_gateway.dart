class Session {
  const Session({required this.userId, required this.email});
  final String userId;
  final String email;
}

sealed class AuthException implements Exception {
  const AuthException();
  const factory AuthException.invalidCredentials() = InvalidCredentials;
  const factory AuthException.network(String message) = NetworkAuthException;
}

final class InvalidCredentials extends AuthException {
  const InvalidCredentials();
}

final class NetworkAuthException extends AuthException {
  const NetworkAuthException(this.message);
  final String message;
}

/// Auth port. Fake by default; Supabase adapter when Env.isLive.
abstract interface class AuthGateway {
  Future<Session?> restore();
  Future<Session> signIn(String email, String password);
  Future<void> signOut();
}
