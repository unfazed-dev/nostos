//! ADR-0041 spike operator check: dial an iroh:// sync URL with a real
//! SyncClient and report what arrived — the one-command proof that a
//! server's printed dial URL works from any machine that can reach it
//! (direct or relay).
//!
//!     cargo run -p nostos-client --features iroh --example iroh_dial_check -- \
//!         'iroh://<node>/sync?ticket=...' [seconds]
//!
//! Prints frames received + checkpoint, then exits. In-memory storage: it
//! never touches your data. The optional `seconds` arg caps the run: a
//! continuously-emitting server (e.g. the fake replicator) never lets the
//! idle timeout fire, so the D5 field-leg rig caps the probe explicitly —
//! the checkpoint prints from the durable storage view at cap time.

use std::time::Duration;

use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .expect("usage: iroh_dial_check <iroh://.../sync?ticket=...> [seconds]");
    let cap = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs);
    let storage = SqliteStorage::open_in_memory().expect("open in-memory sqlite");
    let config = SyncClientConfig {
        idle_timeout: Some(Duration::from_millis(1500)),
        ..SyncClientConfig::default()
    };
    let client = SyncClient::new(url, storage, config);
    let run = client.run_once();
    let outcome = match cap {
        Some(secs) => match tokio::time::timeout(secs, run).await {
            Ok(finished) => Some(finished.expect("run_once")),
            Err(_) => {
                // Capped mid-session: the durable checkpoint still tells the
                // resume story; frames/commits are unknown from outside.
                let lsn = client.checkpoint().await.expect("checkpoint");
                println!(
                    "OK: capped after {}s, checkpoint LSN {}",
                    secs.as_secs(),
                    lsn.raw()
                );
                None
            }
        },
        None => Some(run.await.expect("run_once")),
    };
    if let Some(outcome) = outcome {
        println!(
            "OK: {} frames, {} commits, checkpoint LSN {}",
            outcome.frames_received,
            outcome.commits,
            outcome.checkpoint.raw()
        );
    }
}
