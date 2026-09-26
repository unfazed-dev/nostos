import 'package:appwrite/appwrite.dart' as appwrite;
import 'package:flutter/foundation.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

import 'appwrite_local_signout_web.dart'
    if (dart.library.io) 'appwrite_local_signout_io.dart'
    as local_signout;

/// Build-time provider switch for the same visual Atlet app.
/// `--dart-define=ATLET_PROVIDER=appwrite` selects the hosted ADS project.
const atletProvider = String.fromEnvironment(
  'ATLET_PROVIDER',
  defaultValue: 'supabase',
);
const appwriteEndpoint = String.fromEnvironment(
  'APPWRITE_ENDPOINT',
  defaultValue: 'https://fra.cloud.appwrite.io/v1',
);
const appwriteProjectId = String.fromEnvironment(
  'APPWRITE_PROJECT_ID',
  defaultValue: '6ab741900038c74d1086',
);

bool get usesAppwrite => atletProvider == 'appwrite';

class CloudSession {
  const CloudSession({
    required this.userId,
    required this.jwt,
    this.isAdmin = false,
  });

  final String userId;
  final String jwt;
  final bool isAdmin;
}

/// One Appwrite Client for the whole process, as required for persisted sessions.
class AtletCloudAuth {
  AtletCloudAuth._();

  static final AtletCloudAuth instance = AtletCloudAuth._();
  appwrite.Client? _appwriteClient;

  appwrite.Account get _account => appwrite.Account(
    _appwriteClient ??= appwrite.Client()
      ..setEndpoint(appwriteEndpoint)
      ..setProject(appwriteProjectId),
  );

  Future<void> signIn(String email, String password) async {
    if (usesAppwrite) {
      await _account.createEmailPasswordSession(
        email: email,
        password: password,
      );
    } else {
      await Supabase.instance.client.auth.signInWithPassword(
        email: email,
        password: password,
      );
    }
  }

  /// Appwrite JWTs last 15 minutes; callers renew them while the app is open.
  Future<CloudSession?> session() async {
    if (usesAppwrite) {
      try {
        final user = await _account.get();
        final jwt = await _account.createJWT();
        final teams = await appwrite.Teams(_appwriteClient!).list();
        return CloudSession(
          userId: user.$id,
          jwt: jwt.jwt,
          isAdmin: teams.teams.any((team) => team.$id == 'atlet_admins'),
        );
      } on appwrite.AppwriteException catch (error) {
        if (error.code == 401) return null;
        rethrow;
      }
    }
    final session = Supabase.instance.client.auth.currentSession;
    return session == null
        ? null
        : CloudSession(userId: session.user.id, jwt: session.accessToken);
  }

  Future<String> refreshedJwt() async => usesAppwrite
      ? (await _account.createJWT()).jwt
      : Supabase.instance.client.auth.currentSession!.accessToken;

  Future<void> signOut() async {
    if (usesAppwrite) {
      final account = _account;
      final client = _appwriteClient!;
      try {
        await account.deleteSession(sessionId: 'current');
      } catch (error) {
        // A disconnected device still has to leave this account locally.
        // The remote session remains until Appwrite expires or revokes it.
        debugPrint('Appwrite remote sign-out deferred: $error');
      } finally {
        await local_signout.clearLocalAppwriteSession(client);
      }
    } else {
      await Supabase.instance.client.auth.signOut();
    }
  }
}
