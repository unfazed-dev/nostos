// Native-assets build hook for nostos_flutter, following the pattern proven in
// docs/plans/w4-packaging-fallback.md's W0a spike
// (scratchpad: hello_frb_prebuilt/hook/build.dart) — adapted from a
// throwaway hello-world crate to this package's real crate name/asset path,
// then extended in W6 to be target-aware (macOS/iOS/Android, not just the
// build host) so a single published manifest can serve every platform.
//
// Consumes hook/prebuilt.json (a checked-in manifest keyed by target — see
// that file's comment for the exact key scheme and for why a checked-in
// manifest beats an env var: build-hook subprocesses don't inherit the
// invoking shell's environment, verified empirically in the spike). When the
// current target has a manifest entry and the download+sha256-verify
// succeeds, no `cargo build` runs at all — that's the end-developer path (no
// Rust toolchain required on their machine). On any failure (missing entry,
// HTTP error, hash mismatch) falls back to `cargo build --release` in
// `rust/`. Today — no GitHub Release exists yet, see prebuilt.json — that
// fallback IS the active path for every target. W6 wires CI to publish real
// release artifacts and fill in prebuilt.json (see
// packaging-manifest-update flow below and .github/workflows/release.yml's
// `update-manifest` job).
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data' show BytesBuilder;

import 'package:code_assets/code_assets.dart';
import 'package:crypto/crypto.dart' show sha256;
import 'package:hooks/hooks.dart';

const _assetName = 'src/rust/frb_generated.io.dart';
const _crateName = 'nostos_flutter_rust';

void main(List<String> args) async {
  await build(args, (input, output) async {
    // `flutter run` (unlike `flutter test`) also invokes hooks in phases
    // that don't build code assets; touching `config.code` there throws
    // "Bad state". Guard first, per the hooks API contract.
    if (!input.config.buildCodeAssets) {
      return;
    }
    final code = input.config.code;
    final libFileName = switch (code.targetOS) {
      OS.windows => '$_crateName.dll',
      OS.macOS || OS.iOS => 'lib$_crateName.dylib',
      _ => 'lib$_crateName.so',
    };
    final targetFile = File.fromUri(
      input.outputDirectory.resolve(libFileName),
    );
    await targetFile.parent.create(recursive: true);

    // The manifest key this specific build targets. Must match exactly one
    // of the keys W6's release pipeline fills into hook/prebuilt.json's
    // `artifacts` map — see that file's `_comment` for the full scheme and
    // .github/workflows/release.yml's flutter-* jobs for what produces each
    // key's artifact.
    final key = _manifestKey(code);

    final manifestFile = File.fromUri(
      input.packageRoot.resolve('hook/prebuilt.json'),
    );
    String? url;
    String? expectedSha256;
    if (key != null && manifestFile.existsSync()) {
      final manifest =
          jsonDecode(manifestFile.readAsStringSync()) as Map<String, dynamic>;
      final artifacts = manifest['artifacts'] as Map<String, dynamic>?;
      final entry = artifacts?[key] as Map<String, dynamic>?;
      final rawUrl = entry?['url'] as String?;
      url = (rawUrl == null || rawUrl.isEmpty) ? null : rawUrl;
      expectedSha256 = entry?['sha256'] as String?;
    }

    var source = 'unset';
    if (url != null) {
      try {
        await _downloadAndVerify(
          url: url,
          expectedSha256: expectedSha256,
          destination: targetFile,
        );
        source = 'prebuilt:$url';
      } catch (e) {
        stderr.writeln(
          '[nostos_flutter] prebuilt download failed for target '
          '"$key" ($e); falling back to `cargo build`',
        );
        await _cargoBuildFallback(
          input: input,
          destination: targetFile,
          libFileName: libFileName,
        );
        source = 'cargo-fallback';
      }
    } else {
      // The current, expected v1 state: no GitHub Release exists yet
      // (hook/prebuilt.json's per-target `url`s are placeholders), or this
      // target has no manifest key at all (e.g. Linux/Windows — not yet a
      // published Flutter-glue target, see README's Platforms table). No
      // wasted network round-trip — go straight to the fallback.
      await _cargoBuildFallback(
        input: input,
        destination: targetFile,
        libFileName: libFileName,
      );
      source = 'cargo-fallback';
    }
    stderr.writeln(
      '[nostos_flutter] target=$key artifact source=$source '
      'path=${targetFile.path}',
    );

    output.assets.code.add(
      CodeAsset(
        package: input.packageName,
        name: _assetName,
        linkMode: DynamicLoadingBundled(),
        file: targetFile.uri,
      ),
      routing: const ToAppBundle(),
    );
    output.dependencies.add(targetFile.uri);
  });
}

/// Maps the build's target (OS + architecture, and for iOS, device vs.
/// simulator) to the key W6's release pipeline uses in
/// `hook/prebuilt.json`'s `artifacts` map. Returns null for targets the
/// Flutter glue doesn't publish prebuilt artifacts for yet (Linux, Windows —
/// see README's Platforms table); those always fall back to `cargo build`.
///
/// macOS is a single "universal" key regardless of architecture: W6 ships a
/// lipo'd fat dylib (`lipo -create` over aarch64-apple-darwin +
/// x86_64-apple-darwin), so one artifact serves both Apple Silicon and Intel
/// hosts.
String? _manifestKey(CodeConfig code) {
  return switch (code.targetOS) {
    OS.macOS => 'macos-universal',
    OS.android => switch (code.targetArchitecture) {
      Architecture.arm64 => 'android-arm64-v8a',
      Architecture.arm => 'android-armeabi-v7a',
      Architecture.x64 => 'android-x86_64',
      _ => null,
    },
    OS.iOS => switch (code.iOS.targetSdk) {
      IOSSdk.iPhoneOS => 'ios-device-arm64',
      IOSSdk.iPhoneSimulator => switch (code.targetArchitecture) {
        Architecture.arm64 => 'ios-simulator-arm64',
        Architecture.x64 => 'ios-simulator-x64',
        _ => null,
      },
      _ => null,
    },
    _ => null, // Linux, Windows: fast-follow, no published artifact yet.
  };
}

Future<void> _downloadAndVerify({
  required String url,
  required String? expectedSha256,
  required File destination,
}) async {
  final client = HttpClient();
  try {
    final request = await client.getUrl(Uri.parse(url));
    final response = await request.close();
    if (response.statusCode != 200) {
      throw Exception('HTTP ${response.statusCode} fetching $url');
    }
    final bytes = await response.fold<BytesBuilder>(
      BytesBuilder(),
      (b, chunk) => b..add(chunk),
    );
    await destination.writeAsBytes(bytes.takeBytes());
  } finally {
    client.close(force: true);
  }

  if (expectedSha256 != null && expectedSha256.isNotEmpty) {
    // package:crypto (pure Dart) instead of shelling out to `shasum`: the
    // latter is unix-only and silently breaks a future Windows leg — see
    // docs/plans/w4-packaging-fallback.md's friction notes for why the spike
    // used `shasum` in the first place (it avoided one extra pub dependency
    // for a spike that never targeted Windows). Streaming avoids buffering
    // the whole artifact a second time.
    final digest = await destination.openRead().transform(sha256).single;
    final actual = digest.toString();
    if (actual != expectedSha256) {
      await destination.delete();
      throw Exception('sha256 mismatch: got $actual, want $expectedSha256');
    }
  }
}

Future<void> _cargoBuildFallback({
  required BuildInput input,
  required File destination,
  required String libFileName,
}) async {
  final crateDir = Directory.fromUri(input.packageRoot.resolve('rust/')).path;
  final result = await Process.run('cargo', [
    'build',
    '--release',
  ], workingDirectory: crateDir);
  if (result.exitCode != 0) {
    throw Exception('cargo build failed:\n${result.stderr}');
  }
  final builtFile = File('$crateDir/target/release/$libFileName');
  if (!builtFile.existsSync()) {
    throw Exception('cargo build did not produce ${builtFile.path}');
  }
  await builtFile.copy(destination.path);
}
