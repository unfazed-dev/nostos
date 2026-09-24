//! The JS command boundary on the multi-table shape (Track A4).
//!
//! `tauri::test::get_ipc_response` submits the SAME `InvokeRequest` the
//! frontend's `invoke("plugin:nostos|…", {camelCaseArgs})` produces — through
//! the ACL (capabilities/default.json), the plugin's config block
//! (tauri.conf.json → `plugins.nostos.tables`), and the command arg parser —
//! with no webview. A live spine on the other end makes the second-table
//! write a real server round-trip.

use std::time::Duration;

use tauri::ipc::{CallbackFn, InvokeBody, InvokeResponseBody};
use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{Manager, WebviewWindow, WebviewWindowBuilder};

#[path = "../../tests/common/mod.rs"]
#[allow(dead_code)]
mod common;

fn app() -> WebviewWindow<MockRuntime> {
    let app = mock_builder()
        .plugin(tauri_plugin_nostos::init())
        .build(tauri::generate_context!())
        .expect("build fixture app");
    // `tauri.conf.json` declares `main`; fall back to creating it for a
    // tauri version that leaves config windows to `run()`.
    app.get_webview_window("main").unwrap_or_else(|| {
        WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("main window")
    })
}

fn invoke(
    webview: &WebviewWindow<MockRuntime>,
    cmd: &str,
    args: serde_json::Value,
) -> Result<InvokeResponseBody, serde_json::Value> {
    get_ipc_response(
        webview,
        InvokeRequest {
            cmd: format!("plugin:nostos|{cmd}"),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            // The app origin: `tauri://localhost` on macOS/Linux (the ACL's
            // "URL: local" check); Windows/Android use http://tauri.localhost.
            url: if cfg!(any(windows, target_os = "android")) {
                "http://tauri.localhost"
            } else {
                "tauri://localhost"
            }
            .parse()
            .expect("url"),
            body: InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    )
}

/// connect (config tables) → subscribe → write on the SECOND table crosses
/// the IPC boundary and round-trips through the server; an unlisted table is
/// refused with the error the JS side will see.
#[test]
fn ipc_multi_table_write_round_trips_and_unlisted_table_is_refused() {
    // Plain #[test]: get_ipc_response blocks on a channel, so the spine +
    // observer live on an explicit runtime instead of a #[tokio::test] worker.
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let (port, mut child) = rt.block_on(common::spawn_spine());
    let obs = rt.block_on(common::observer_tables(
        port,
        "fixture-ipc",
        &["tasks", "notes"],
    ));

    let webview = app();
    let db = std::env::temp_dir().join(format!("nostos-fixture-ipc-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&db);

    // url + dbPath per call (the spine port is dynamic); `tables` comes from
    // tauri.conf.json — the shape a real app ships.
    invoke(
        &webview,
        "connect",
        serde_json::json!({ "url": format!("ws://127.0.0.1:{port}/sync"), "dbPath": db.to_str().unwrap() }),
    )
    .expect("connect over IPC");
    invoke(
        &webview,
        "subscribe",
        serde_json::json!({ "table": "tasks" }),
    )
    .expect("subscribe over IPC");
    std::thread::sleep(Duration::from_millis(300));

    let id = invoke(
        &webview,
        "write",
        serde_json::json!({ "table": "notes", "op": "upsert", "pk": "fx-ipc-1", "payloadJson": "{\"body\":\"via-ipc\"}" }),
    )
    .expect("write to the second table over IPC")
    .deserialize::<u64>()
    .expect("outbox id");
    assert!(id > 0);

    let n = rt.block_on(common::poll_table_rows_with_prefix(
        &obs,
        "notes",
        "fx-ipc-",
        1,
        Duration::from_secs(60),
    ));
    assert!(n >= 1, "observer never saw the notes row written over IPC");

    // The refusal the frontend sees (camelCase args, plugin-prefixed cmd).
    let err = invoke(
        &webview,
        "write",
        serde_json::json!({ "table": "other", "op": "upsert", "pk": "x", "payloadJson": null }),
    )
    .expect_err("unlisted table");
    assert!(
        err.as_str()
            .unwrap_or_default()
            .contains("is not in the session tables [\"tasks\", \"notes\"]"),
        "got {err}"
    );

    // query is the JS read path: both tables live in the one store.
    let rows = invoke(
        &webview,
        "query",
        serde_json::json!({ "sql": "SELECT table_name FROM nostos_data WHERE pk = 'fx-ipc-1'" }),
    )
    .expect("query over IPC")
    .deserialize::<String>()
    .expect("json string");
    assert!(rows.contains("notes"), "local store row: {rows}");

    println!("[fixture] IPC_MULTI_TABLE_OK");
    rt.block_on(async {
        let _ = child.kill().await;
    });
}
