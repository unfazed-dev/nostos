/// Platform-selected [NostosEngine] factory (ADR-0036).
///
/// Native builds resolve to [RustNostosEngine] (flutter_rust_bridge + the Rust
/// dylib + `path_provider` for the SQLite path). Web builds resolve to
/// [WebNostosEngine], which drives the *shared* `nostos-ffi-wasm` backend over a
/// Worker via `dart:js_interop` — the same backend `@nostos-sync/web` uses, so
/// Flutter-web inherits Wave-2 opfs-sahpool durability rather than the
/// rejected `frb_generated.web.dart` path (which would strand it on an
/// in-memory, rusqlite-compiled-to-wasm backend).
///
/// Selection is compile-time via a Dart conditional import, so the native path
/// (`RustLib.init`, `path_provider`, the frb dylib loader) is never even
/// *imported* on web — keeping `flutter build web` entirely off frb's web
/// codegen. Both selectors expose the same `createNostosEngine` signature.
library;

export 'engine_selector_io.dart'
    if (dart.library.js_interop) 'engine_selector_web.dart';
