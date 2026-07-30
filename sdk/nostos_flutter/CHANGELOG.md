## 0.1.0

Initial release. Plug-and-play local-first sync for Flutter, backed by
[Nostos](https://github.com/unfazed-dev/nostos): `Nostos.connect`/`subscribe`/`watch`/`write` over
a Rust-owned SQLite + WebSocket sync loop (flutter_rust_bridge native-assets
backend — no codegen, no Xcode/Gradle wiring). Supabase auth pass-through via
`NostosSupabase.connect`. Platforms: macOS verified; iOS/Android build config
present, verified by `.github/workflows/release.yml`'s cross-compile matrix.
Windows/Linux/Web are fast-follow (see README's Platforms table).
