import 'package:flutter/material.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

import '../design/tokens.dart';

/// Injectable auth calls so route transitions are testable without a live
/// Supabase project (see apps/atlet/design/auth.jsx for the reference React
/// flow this layout is ported from).
typedef PasswordSignIn = Future<void> Function(String email, String password);
typedef SendEmailOtp = Future<void> Function(String email);
typedef VerifyEmailOtp = Future<void> Function(String email, String token);

Future<void> _defaultPasswordSignIn(String email, String password) =>
    Supabase.instance.client.auth
        .signInWithPassword(email: email, password: password);

Future<void> _defaultSendEmailOtp(String email) =>
    Supabase.instance.client.auth.signInWithOtp(email: email);

Future<void> _defaultVerifyEmailOtp(String email, String token) =>
    Supabase.instance.client.auth
        .verifyOTP(type: OtpType.email, email: email, token: token);

/// Signin screen. Password sign-in is the default path (deterministic for
/// bench runs); OTP is reachable via "use a code instead" but is a secondary
/// path, matching task-6-brief Step 3.
class SigninScreen extends StatefulWidget {
  const SigninScreen({
    super.key,
    required this.onSignedIn,
    this.passwordSignIn = _defaultPasswordSignIn,
    this.sendEmailOtp = _defaultSendEmailOtp,
    this.verifyEmailOtp = _defaultVerifyEmailOtp,
  });

  final VoidCallback onSignedIn;
  final PasswordSignIn passwordSignIn;
  final SendEmailOtp sendEmailOtp;
  final VerifyEmailOtp verifyEmailOtp;

  @override
  State<SigninScreen> createState() => _SigninScreenState();
}

enum _Mode { password, otpRequest, otpVerify }

class _SigninScreenState extends State<SigninScreen> {
  _Mode _mode = _Mode.password;
  // Prefill the seeded conformance user. NOTE: `.internal` addresses are
  // rejected by GoTrue's email validation, so the SDK users live under
  // `@atlet.dev` (see supabase/scripts/create_sdk_users.sh).
  final _email = TextEditingController(text: 'flutter@atlet.dev');
  final _password = TextEditingController(text: 'atlet-flutter-2026');
  final _otp = TextEditingController();
  bool _working = false;
  String? _error;

  @override
  void dispose() {
    _email.dispose();
    _password.dispose();
    _otp.dispose();
    super.dispose();
  }

  bool get _emailValid =>
      RegExp(r'^[^\s@]+@[^\s@]+\.[^\s@]+$').hasMatch(_email.text);

  Future<void> _submitPassword() async {
    setState(() {
      _working = true;
      _error = null;
    });
    try {
      await widget.passwordSignIn(_email.text, _password.text);
      widget.onSignedIn();
    } on AuthException catch (e) {
      // Surface the server's reason (e.g. "Invalid login credentials") —
      // a generic message here previously masked a wrong-email root cause.
      setState(() => _error = 'Sign-in failed: ${e.message}');
    } catch (e) {
      setState(() => _error = 'Sign-in failed. Check the password and try again.');
    } finally {
      if (mounted) setState(() => _working = false);
    }
  }

  Future<void> _requestOtp() async {
    setState(() {
      _working = true;
      _error = null;
    });
    try {
      await widget.sendEmailOtp(_email.text);
      setState(() => _mode = _Mode.otpVerify);
    } catch (e) {
      setState(() => _error = "Couldn't send a code. Try again.");
    } finally {
      if (mounted) setState(() => _working = false);
    }
  }

  Future<void> _verifyOtp() async {
    setState(() {
      _working = true;
      _error = null;
    });
    try {
      await widget.verifyEmailOtp(_email.text, _otp.text);
      widget.onSignedIn();
    } catch (e) {
      setState(() => _error = "That code didn't match. Try again.");
    } finally {
      if (mounted) setState(() => _working = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: AtletTokens.paper,
      body: SafeArea(
        child: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 360),
            child: SingleChildScrollView(
              padding: const EdgeInsets.all(24),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  const _AtletMark(),
                  const SizedBox(height: 32),
                  Text(
                    'SIGN IN',
                    style: TextStyle(
                      fontSize: AtletTokens.footnote,
                      letterSpacing: 1.5,
                      color: AtletTokens.ink3,
                      fontWeight: FontWeight.w500,
                    ),
                  ),
                  const SizedBox(height: 8),
                  Text(
                    _mode == _Mode.otpVerify ? 'Enter your code.' : 'Welcome back.',
                    style: TextStyle(
                      fontSize: AtletTokens.title2,
                      fontWeight: FontWeight.w600,
                      color: AtletTokens.ink,
                    ),
                  ),
                  const SizedBox(height: 24),
                  // Google/Apple are present in the design (auth.jsx) but
                  // OAuth wiring is out of scope for this pilot — disabled.
                  _StubOAuthButton(label: 'Continue with Google'),
                  const SizedBox(height: 12),
                  _StubOAuthButton(label: 'Continue with Apple'),
                  const SizedBox(height: 24),
                  Row(
                    children: [
                      Expanded(child: Divider(color: AtletTokens.rule)),
                      Padding(
                        padding: const EdgeInsets.symmetric(horizontal: 12),
                        child: Text('or use email',
                            style: TextStyle(color: AtletTokens.ink3, fontSize: AtletTokens.footnote)),
                      ),
                      Expanded(child: Divider(color: AtletTokens.rule)),
                    ],
                  ),
                  const SizedBox(height: 24),
                  if (_mode != _Mode.otpVerify) ...[
                    TextField(
                      key: const Key('signin-email'),
                      controller: _email,
                      keyboardType: TextInputType.emailAddress,
                      autofillHints: const [AutofillHints.email],
                      decoration: const InputDecoration(
                        labelText: 'Email',
                        hintText: 'you@example.com',
                      ),
                      onChanged: (_) => setState(() {}),
                    ),
                  ],
                  if (_mode == _Mode.password) ...[
                    const SizedBox(height: 12),
                    TextField(
                      key: const Key('signin-password'),
                      controller: _password,
                      obscureText: true,
                      autofillHints: const [AutofillHints.password],
                      decoration: const InputDecoration(labelText: 'Password'),
                      onChanged: (_) => setState(() {}),
                      onSubmitted: (_) => _submitPassword(),
                    ),
                    const SizedBox(height: 20),
                    _PrimaryButton(
                      label: 'Sign in',
                      working: _working,
                      enabled: _emailValid && _password.text.isNotEmpty,
                      onPressed: _submitPassword,
                    ),
                    TextButton(
                      onPressed: _working ? null : () => setState(() => _mode = _Mode.otpRequest),
                      child: const Text('Use a code instead'),
                    ),
                  ] else if (_mode == _Mode.otpRequest) ...[
                    const SizedBox(height: 20),
                    _PrimaryButton(
                      label: 'Send code',
                      working: _working,
                      enabled: _emailValid,
                      onPressed: _requestOtp,
                    ),
                    TextButton(
                      onPressed: _working ? null : () => setState(() => _mode = _Mode.password),
                      child: const Text('Back to password'),
                    ),
                  ] else ...[
                    Text('Code sent to ${_email.text}',
                        style: TextStyle(color: AtletTokens.ink3, fontSize: AtletTokens.footnote)),
                    const SizedBox(height: 12),
                    TextField(
                      controller: _otp,
                      keyboardType: TextInputType.number,
                      maxLength: 6,
                      style: TextStyle(fontFamily: AtletTokens.monoFamily, letterSpacing: 4),
                      decoration: const InputDecoration(labelText: 'Code', counterText: ''),
                      onChanged: (_) => setState(() {}),
                    ),
                    const SizedBox(height: 12),
                    _PrimaryButton(
                      label: 'Verify',
                      working: _working,
                      enabled: _otp.text.length == 6,
                      onPressed: _verifyOtp,
                    ),
                  ],
                  if (_error != null) ...[
                    const SizedBox(height: 12),
                    Text(_error!, style: const TextStyle(color: Colors.red)),
                  ],
                  const SizedBox(height: 24),
                  Text(
                    "By continuing you agree to Atlet's Terms and Privacy Policy.",
                    textAlign: TextAlign.center,
                    style: TextStyle(fontSize: AtletTokens.footnote, color: AtletTokens.ink3),
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}

class _AtletMark extends StatelessWidget {
  const _AtletMark();

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Text(
        'ATLET',
        style: TextStyle(
          fontFamily: AtletTokens.sansFamily,
          fontWeight: FontWeight.w700,
          fontSize: AtletTokens.title2,
          letterSpacing: 2,
          color: AtletTokens.ink,
        ),
      ),
    );
  }
}

class _StubOAuthButton extends StatelessWidget {
  const _StubOAuthButton({required this.label});

  final String label;

  @override
  Widget build(BuildContext context) {
    return OutlinedButton(
      onPressed: null, // ponytail: OAuth not wired — email/password+OTP only.
      style: OutlinedButton.styleFrom(
        side: BorderSide(color: AtletTokens.rule),
        padding: const EdgeInsets.symmetric(vertical: 14),
      ),
      child: Text(label, style: TextStyle(color: AtletTokens.ink3)),
    );
  }
}

class _PrimaryButton extends StatelessWidget {
  const _PrimaryButton({
    required this.label,
    required this.working,
    required this.enabled,
    required this.onPressed,
  });

  final String label;
  final bool working;
  final bool enabled;
  final VoidCallback onPressed;

  @override
  Widget build(BuildContext context) {
    return FilledButton(
      onPressed: enabled && !working ? onPressed : null,
      style: FilledButton.styleFrom(
        backgroundColor: AtletTokens.accent,
        padding: const EdgeInsets.symmetric(vertical: 14),
      ),
      child: working
          ? const SizedBox(
              height: 18,
              width: 18,
              child: CircularProgressIndicator(strokeWidth: 2, color: Colors.white),
            )
          : Text(label),
    );
  }
}
