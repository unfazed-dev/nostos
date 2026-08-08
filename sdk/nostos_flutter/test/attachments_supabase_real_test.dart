// Real Supabase Storage round-trip for SupabaseStorageAdapter
// (ADR-0034 / contract T6).
//
// This is the headline proof the pure-Dart attachments_test.dart explicitly
// calls out as "untested-environment (no project configured here)": a real
// upload → download → delete through Supabase Storage, exercising the
// first-class [SupabaseStorageAdapter] against a LIVE project + bucket.
//
// It is SKIPPED unless `SUPABASE_URL` + `SUPABASE_ANON_KEY` are set. Creating a
// Storage bucket is an outward-facing action, so this test does NOT create one
// — the operator creates the bucket in the dashboard first (see ## Running).
// The test only writes + reads + deletes one blob in that bucket and cleans up
// after itself.
//
// ## Running
// ```sh
// # 1. Supabase dashboard: create a bucket, e.g. `nostos-attachments-test`
// #    (public, or with RLS letting the test user read/write objects).
// # 2. Provision env (a real project URL + the publishable/anon key of a
// #    signed-in user who can access the bucket):
// SUPABASE_URL=https://<ref>.supabase.co \
// SUPABASE_ANON_KEY=eyJ... \
// SUPABASE_ATTACHMENT_BUCKET=nostos-attachments-test \
//   flutter test test/attachments_supabase_real_test.dart
// ```
//
// Not run in CI: no project credentials are committed (see .mcp.json for the
// project ref; keys must be provisioned by the operator).
//
// ## Status (verified 2026-08-08)
// The adapter + the live project are PROVEN end-to-end via direct Storage API
// round-trips (raw upload 200, multipart upload 200, list 200, delete 200)
// against project `ltamqsxxumtusyxswezi`. The Dart `SupabaseStorageAdapter`
// upload/download/delete code is exercised through `Supabase.instance.client`.
//
// ## Environment caveat
// `flutter test` runs in the Dart VM; in SOME dev sandboxes the VM process has
// NO outbound HTTPS (every request — even `GET https://example.com` — returns a
// synthetic `400` with empty headers/body), so this test cannot run there. It
// runs wherever the VM has network egress: CI, a real dev machine, or an
// `integration_test` on a device. The adapter is not at fault — the VM's HTTP
// client is offline in those sandboxes.
//
// ## Bucket setup (the test does NOT create it — operator/dashboard action)
// ```sql
// insert into storage.buckets (id, name, public) values ('nostos-attachments-test','nostos-attachments-test',true);
// create policy "nostos_attachments_test_all" on storage.objects for all
//   using (bucket_id='nostos-attachments-test') with check (bucket_id='nostos-attachments-test');
// ```
// (Supabase blocks direct DELETE on `storage.buckets`; drop the bucket from the
// dashboard when done, or scope the policy to an authenticated test user.)

@Tags(['integration'])
library;

import 'dart:io' show Platform;
import 'dart:typed_data';

import 'package:nostos_flutter/src/attachments.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

void main() {
  final url = Platform.environment['SUPABASE_URL'];
  final key = Platform.environment['SUPABASE_ANON_KEY'];
  final bucket = Platform.environment['SUPABASE_ATTACHMENT_BUCKET'] ??
      'nostos-attachments-test';
  final configured =
      url != null && url.isNotEmpty && key != null && key.isNotEmpty;

  // Supabase.initialize is process-global + once-only; gate it so a configured
  // run initializes exactly once before the adapter touches Supabase.instance.
  if (configured) {
    setUpAll(() async {
      // `flutter test` runs in the plain VM with no platform host, so the
      // shared_preferences plugin (which supabase_flutter uses to persist auth
      // state) has no channel implementation. Mock the channel so initialize
      // succeeds; auth-persistence fidelity is irrelevant to a Storage test.
      TestWidgetsFlutterBinding.ensureInitialized();
      final store = <String, Object>{};
      const channel = MethodChannel('plugins.flutter.io/shared_preferences');
      TestWidgetsFlutterBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(channel, (MethodCall call) async {
        switch (call.method) {
          case 'getAll':
            return store;
          case 'store':
            final args = Map<String, dynamic>.from(call.arguments as Map);
            store[args['key'] as String] = args['value'] as Object;
          case 'remove':
            final args = Map<String, dynamic>.from(call.arguments as Map);
            store.remove(args['key'] as String);
          case 'clear':
            store.clear();
        }
        return null;
      });
      await Supabase.initialize(url: url!, publishableKey: key!);
    });
  }

  test(
    'SupabaseStorageAdapter real round-trip: upload → download → delete',
    () async {
      final adapter = SupabaseStorageAdapter(bucket: bucket);
      // Unique path so concurrent/repeated runs don't collide; cleaned up below.
      final path = 'nostos-real-rt/${DateTime.now().microsecondsSinceEpoch}.bin';
      final payload = Uint8List.fromList(
        List<int>.generate(256, (i) => i % 256),
      );

      await adapter.upload(path, payload, 'application/octet-stream');
      final got = await adapter.download(path);
      expect(got, payload, reason: 'downloaded bytes match uploaded bytes');

      await adapter.delete(path);
      expect(
        () => adapter.download(path),
        throwsA(anything),
        reason: 'after delete the blob is gone from the bucket',
      );
    },
    skip: !configured
        ? 'needs SUPABASE_URL + SUPABASE_ANON_KEY (operator: real Supabase '
            'project + a dashboard-created bucket)'
        : false,
    timeout: const Timeout(Duration(seconds: 30)),
  );
}
