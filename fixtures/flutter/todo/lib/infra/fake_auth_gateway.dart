import 'package:todo/domain/auth_gateway.dart';

/// Fake auth gateway. Used in mock mode (the default) and for the persona smoke
/// suite. Accepts exactly [demoEmail]/[demoPassword]; anything else throws
/// [AuthException.invalidCredentials]. No persisted session — [restore] is null.
class FakeAuthGateway implements AuthGateway {
  static const String demoEmail = 'demo@nostos.run';
  static const String demoPassword = 'demo-1234';

  @override
  Future<Session?> restore() async => null;

  @override
  Future<Session> signIn(String email, String password) async {
    if (email == demoEmail && password == demoPassword) {
      return Session(userId: 'demo-user', email: email);
    }
    throw const AuthException.invalidCredentials();
  }

  @override
  Future<void> signOut() async {}
}
