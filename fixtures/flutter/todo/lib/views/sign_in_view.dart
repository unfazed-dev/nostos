import 'package:flutter/material.dart';

import '../domain/auth_gateway.dart';
import '../viewmodels/auth_viewmodel.dart';

/// Sign-in form: email/password [TextField]s + a submit [FilledButton] + an
/// error [Text], surfaced by the load-bearing keys `auth.email`,
/// `auth.password`, `auth.submit`, `auth.error` (Task 10's smoke drives these).
///
/// Two entry modes share one widget:
/// - Standalone (widget tests): omit [viewModel]; the view creates and owns an
///   [AuthViewModel] internally from [auth].
/// - Composed ([TodoApp]): pass [viewModel]; the view renders against a
///   parent-owned VM and does NOT dispose it (the swap on its session is what
///   `TodoApp` observes).
class SignInView extends StatefulWidget {
  const SignInView({super.key, required this.auth, this.viewModel});

  final AuthGateway auth;
  final AuthViewModel? viewModel;

  @override
  State<SignInView> createState() => _SignInViewState();
}

class _SignInViewState extends State<SignInView> {
  late final AuthViewModel _vm = widget.viewModel ?? AuthViewModel(widget.auth);
  late final TextEditingController _email = TextEditingController();
  late final TextEditingController _password = TextEditingController();

  // True when this view created the VM (and so must dispose it).
  bool get _ownsVm => widget.viewModel == null;

  Future<void> _submit() => _vm.signIn(_email.text, _password.text);

  @override
  void dispose() {
    _email.dispose();
    _password.dispose();
    if (_ownsVm) _vm.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        body: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 320),
            child: ListenableBuilder(
              listenable: _vm,
              builder: (context, _) => Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  TextField(
                    key: const Key('auth.email'),
                    controller: _email,
                    decoration: const InputDecoration(labelText: 'Email'),
                    keyboardType: TextInputType.emailAddress,
                  ),
                  TextField(
                    key: const Key('auth.password'),
                    controller: _password,
                    decoration: const InputDecoration(labelText: 'Password'),
                    obscureText: true,
                  ),
                  const SizedBox(height: 16),
                  FilledButton(
                    key: const Key('auth.submit'),
                    onPressed: _submit,
                    child: const Text('Sign in'),
                  ),
                  if (_vm.errorMessage != null) ...[
                    const SizedBox(height: 12),
                    Text(
                      _vm.errorMessage!,
                      key: const Key('auth.error'),
                      style: TextStyle(color: Theme.of(context).colorScheme.error),
                    ),
                  ],
                ],
              ),
            ),
          ),
        ),
      );
}
