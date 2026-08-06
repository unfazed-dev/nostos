// Route-transition test: signin -> home, using an injected fake sign-in so
// this runs without a live Supabase project (see ui/signin.dart's
// PasswordSignIn typedef). This is the honest substitute for the live
// "sign-in reaches home shell" check task-6-brief Step 4 asks for; a real
// Supabase round trip needs operator-supplied SUPABASE_URL/ANON_KEY and the
// flutter@atlet.internal password (not available to this scaffold).

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:atlet/main.dart';
import 'package:atlet/ui/signin.dart';

void main() {
  testWidgets('signin -> home via injected password sign-in', (tester) async {
    await tester.pumpWidget(
      MaterialApp(
        initialRoute: '/signin',
        routes: {
          '/signin': (context) => SigninScreen(
                passwordSignIn: (email, password) async {},
                onSignedIn: () =>
                    Navigator.of(context).pushReplacementNamed('/home'),
              ),
          '/home': (context) => const HomeScreen(),
        },
      ),
    );

    expect(find.text('Home'), findsNothing);

    await tester.enterText(find.byKey(const Key('signin-email')), 'flutter@atlet.internal');
    await tester.enterText(find.byKey(const Key('signin-password')), 'password');
    await tester.pump();

    await tester.tap(find.widgetWithText(FilledButton, 'Sign in'));
    await tester.pumpAndSettle();

    // 'Home' now also labels the bottom-nav destination (I-1's tab shell),
    // so pin this to the app bar title specifically.
    expect(find.widgetWithText(AppBar, 'Home'), findsOneWidget);
  });
}
