/// Live-mode switch. Values arrive ONLY via --dart-define / --dart-define-from-file
/// (see env.example.json); nothing is ever hardcoded or committed.
class Env {
  static const supabaseUrl = String.fromEnvironment('SUPABASE_URL');
  static const supabaseAnonKey = String.fromEnvironment('SUPABASE_ANON_KEY');
  static const testEmail = String.fromEnvironment('SUPABASE_TEST_EMAIL');
  static const testPassword = String.fromEnvironment('SUPABASE_TEST_PASSWORD');

  /// Both-or-neither: the smoke suite fails closed on a contradictory env
  /// (exactly one of url/key set) — see smoke_auth_test.dart's guard.
  static const isLive = supabaseUrl != '' && supabaseAnonKey != '';
}
