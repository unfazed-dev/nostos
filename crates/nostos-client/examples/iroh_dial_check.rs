//! ADR-0041 spike operator check: dial an iroh:// sync URL with a real
//! SyncClient and report what arrived — the one-command proof that a
//! server's printed dial URL works from any machine that can reach it
//! (direct or relay).
//!
//!     cargo run -p nostos-client --features iroh --example iroh_dial_check -- \
//!         'iroh://<node>/sync?ticket=...'
//!
//! Prints frames received + checkpoint, then exits. In-memory storage: it
//! never touches your data.

use std::time::Duration;

use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .expect("usage: iroh_dial_check <iroh://.../sync?ticket=...>");
    let storage = SqliteStorage::open_in_memory().expect("open in-memory sqlite");
    let config = SyncClientConfig {
        idle_timeout: Some(Duration::from_millis(1500)),
        ..SyncClientConfig::default()
    };
    let client = SyncClient::new(url.clone(), storage, config);
    match client.run_once().await {
        Ok(outcome) => {
            println!(
                "OK: {} frames, {} commits, checkpoint LSN {}",
                outcome.frames_received,
                outcome.commits,
                outcome.checkpoint.raw()
            );
        }
        Err(e) => {
            eprintln!("FAIL: {e}");
            std::process::exit(1);
        }
    }
}
