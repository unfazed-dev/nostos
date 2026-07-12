//! Tauri 2 plugin build script. Generates the permission schema and command
//! permission identifiers (`allow-<cmd>`) consumed by `permissions/default.toml`.
//!
//! ponytail: this is the canonical Tauri-2 plugin build.rs — it does no
//! hand-rolled codegen. If the upstream `tauri-plugin` API changes the
//! Builder shape, this is the one-line fix point.

fn main() {
    // tauri-plugin 2.6+ `Builder::new` takes the command-name slice so it can
    // generate the `allow-<cmd>` permission identifiers. Must match the
    // `#[tauri::command]` fns registered in `init()` (src/lib.rs).
    tauri_plugin::Builder::new(&["connect", "write", "query", "checkpoint"]).build();
}
