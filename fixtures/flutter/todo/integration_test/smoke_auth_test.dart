import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:todo/env.dart';
import 'package:todo/infra/fake_auth_gateway.dart';
import 'package:todo/main.dart' as app;

/// Dual-mode smoke (Task 10 of the Flutter fixtures plan). ONE step list runs
/// in TWO modes, selected purely by `--dart-define-from-file=env.json`:
///   - Mock mode (default, CI-safe): no credentials; `Env.isLive` is false, so
///     `main()` wires `FakeAuthGateway` + `InMemoryTodoRepository`.
///   - Live mode (operator): same steps against real Supabase cloud auth +
///     database, activated only when BOTH `SUPABASE_URL` and
///     `SUPABASE_ANON_KEY` (and a pre-provisioned test user's email/password)
///     arrive via env.json.
///
/// The CRITICAL design property: this suite must NEVER silently fall back to
/// mock mode and "pass" when it should be live. The first test
/// (`adapter selection matches the provided environment`) is the fail-closed
/// guard — a contradictory env (exactly one of url/key set) aborts loudly.
void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  // Fail-closed branch guard: a contradictory env (exactly one of url/key)
  // must abort loudly, never silently fall back to mock and "pass".
  test('adapter selection matches the provided environment', () {
    final url = Env.supabaseUrl.isNotEmpty;
    final key = Env.supabaseAnonKey.isNotEmpty;
    expect(url == key, isTrue,
        reason: 'contradictory env: provide BOTH SUPABASE_URL and '
            'SUPABASE_ANON_KEY (live) or NEITHER (mock)');
    expect(Env.isLive, url && key);
    if (Env.isLive) {
      expect(Env.testEmail.isNotEmpty && Env.testPassword.isNotEmpty, isTrue,
          reason: 'live mode needs SUPABASE_TEST_EMAIL/PASSWORD for a '
              'PRE-PROVISIONED, email-confirmed test user (the suite '
              'does not create users)');
    }
  });

  final email = Env.isLive ? Env.testEmail : FakeAuthGateway.demoEmail;
  final password = Env.isLive ? Env.testPassword : FakeAuthGateway.demoPassword;

  testWidgets(
    'smoke: boot -> reject bad password -> authenticate -> add todo -> sign out',
    timeout: const Timeout(Duration(seconds: 30)), // live-mode latency budget
    (tester) async {
      app.main();
      await tester.pumpAndSettle();

      // Boot: sign-in screen renders.
      expect(find.byKey(const Key('auth.email')), findsOneWidget);

      // Wrong password is rejected with a visible error (real round-trip in
      // live mode; FakeAuthGateway path in mock mode — same assertion).
      await _enter(tester, 'auth.email', email);
      await _enter(tester, 'auth.password', 'wrong-password');
      await tester.tap(find.byKey(const Key('auth.submit')));
      await _settle(tester);
      expect(find.byKey(const Key('auth.error')), findsOneWidget);

      // Correct credentials authenticate into the todo home. The field is
      // re-entered AFTER the error widget inserted (a tree rebuild); on the
      // macOS desktop integration target a bare `enterText` against a rebuilt
      // field can fail to update its controller, so `_enter` taps-to-focus
      // first — deterministic in both mock and live modes.
      await _enter(tester, 'auth.password', password);
      await tester.tap(find.byKey(const Key('auth.submit')));
      await _settle(tester);
      expect(find.byKey(const Key('todos.list')), findsOneWidget);

      // A write lands and renders (in live mode this is a real insert under
      // RLS as the test user, streamed back from Supabase). Scoped to the list
      // so the still-populated input field (which holds the marker text) isn't
      // also matched.
      final marker = 'smoke ${DateTime.now().millisecondsSinceEpoch}';
      await _enter(tester, 'todos.input', marker);
      await tester.tap(find.byKey(const Key('todos.add')));
      await _settle(tester);
      expect(
        find.descendant(
            of: find.byKey(const Key('todos.list')),
            matching: find.text(marker)),
        findsOneWidget,
      );

      // Sign out returns to the sign-in screen.
      await tester.tap(find.byKey(const Key('auth.signout')));
      await _settle(tester);
      expect(find.byKey(const Key('auth.email')), findsOneWidget);
    },
  );
}

/// pumpAndSettle with a poll loop tolerant of live-mode stream latency.
///
/// A fixed 50×100ms pump batch is reliable in mock mode (no network latency —
/// FakeAuthGateway + InMemoryTodoRepository settle on a couple of frames); the
/// generous count is the headroom budget for live-mode round-trips, which the
/// operator's run exercises but this mock-mode run does not.
Future<void> _settle(WidgetTester tester) async {
  for (var i = 0; i < 50; i++) {
    await tester.pump(const Duration(milliseconds: 100));
  }
}

/// Focus-then-enter a keyed [TextField] by its string key.
///
/// On the macOS desktop integration target, a bare `enterText` against a field
/// that was rebuilt (e.g. after an error widget inserts above it, reordering
/// the column) can fail to update the field's controller — the field loses
/// focus on the rebuild and the keystrokes don't land. Tapping to (re-)focus
/// first makes the entry deterministic. This is the same tapKey-style helper
/// the pomodoro fixture's journeys use; it costs one frame and works in both
/// mock and live modes.
Future<void> _enter(WidgetTester tester, String key, String text) async {
  await tester.tap(find.byKey(Key(key)));
  await tester.pump();
  await tester.enterText(find.byKey(Key(key)), text);
}
