//! nostos Tauri fixture app (Track A4). `cargo run` opens a window whose
//! `dist/index.html` drives the plugin over `window.__TAURI__` against
//! `plugins.cairn` in `tauri.conf.json` (two tables). `cargo test` drives the
//! same command boundary headlessly via `tauri::test`.
#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_cairn::init())
        .run(tauri::generate_context!())
        .expect("run nostos fixture");
}
