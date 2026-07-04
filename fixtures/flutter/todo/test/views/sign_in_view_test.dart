import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:mocktail/mocktail.dart';
import 'package:todo/domain/auth_gateway.dart';
import 'package:todo/views/sign_in_view.dart';

class _MockAuthGateway extends Mock implements AuthGateway {}

void main() {
  late _MockAuthGateway auth;

  setUp(() {
    auth = _MockAuthGateway();
    registerFallbackValue(const AuthException.invalidCredentials());
  });

  testWidgets(
    'invalid credentials surface a visible error message',
    (tester) async {
      when(() => auth.signIn(any(), any()))
          .thenThrow(const AuthException.invalidCredentials());

      await tester.pumpWidget(MaterialApp(
        home: SignInView(auth: auth),
      ));

      await tester.enterText(
          find.byKey(const Key('auth.email')), 'a@b.com');
      await tester.enterText(
          find.byKey(const Key('auth.password')), 'wrong');
      await tester.tap(find.byKey(const Key('auth.submit')));
      await tester.pump();
      await tester.pumpAndSettle();

      expect(find.byKey(const Key('auth.error')), findsOneWidget);
      expect(
        tester.widget<Text>(find.byKey(const Key('auth.error'))).data,
        'Invalid email or password',
      );
    },
  );
}
