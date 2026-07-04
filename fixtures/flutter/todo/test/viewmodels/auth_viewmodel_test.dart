import 'package:flutter_test/flutter_test.dart';
import 'package:mocktail/mocktail.dart';
import 'package:todo/domain/auth_gateway.dart';
import 'package:todo/viewmodels/auth_viewmodel.dart';

class _MockAuthGateway extends Mock implements AuthGateway {}

void main() {
  late _MockAuthGateway auth;

  setUp(() {
    auth = _MockAuthGateway();
    // mocktail needs concrete fallback types for the sealed AuthException
    // factories when no specific return is registered.
    registerFallbackValue(const AuthException.invalidCredentials());
  });

  AuthViewModel vm() => AuthViewModel(auth);

  test('initial state: signed out, not busy, no error', () {
    final m = vm();
    expect(m.session, isNull);
    expect(m.busy, isFalse);
    expect(m.errorMessage, isNull);
  });

  test('signIn success exposes session and clears busy', () async {
    final session = Session(userId: 'u-1', email: 'a@b.com');
    when(() => auth.signIn(any(), any()))
        .thenAnswer((_) async => session);

    final m = vm();
    await m.signIn('a@b.com', 'pw');

    expect(m.session, session);
    expect(m.busy, isFalse);
    expect(m.errorMessage, isNull);
  });

  test('signIn invalidCredentials exposes errorMessage and never throws to view',
      () async {
    when(() => auth.signIn(any(), any()))
        .thenThrow(const AuthException.invalidCredentials());

    final m = vm();
    // Must NOT throw to the caller — the view never sees an exception.
    await expectLater(m.signIn('a@b.com', 'wrong'), completes);

    expect(m.session, isNull);
    expect(m.busy, isFalse);
    expect(m.errorMessage, 'Invalid email or password');
  });

  test('signIn network error exposes the network message', () async {
    const msg = 'No internet connection';
    when(() => auth.signIn(any(), any()))
        .thenThrow(const AuthException.network(msg));

    final m = vm();
    await m.signIn('a@b.com', 'pw');

    expect(m.session, isNull);
    expect(m.busy, isFalse);
    expect(m.errorMessage, msg);
  });

  test('signOut clears session', () async {
    final session = Session(userId: 'u-1', email: 'a@b.com');
    when(() => auth.signIn(any(), any()))
        .thenAnswer((_) async => session);
    when(auth.signOut).thenAnswer((_) async {});

    final m = vm();
    await m.signIn('a@b.com', 'pw');
    expect(m.session, isNotNull);

    await m.signOut();

    expect(m.session, isNull);
    verify(auth.signOut).called(1);
  });
}
