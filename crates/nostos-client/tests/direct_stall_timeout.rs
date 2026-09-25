//! A PostgREST that accepts the TCP connection and never answers — the shape
//! measured on the iPhone 2026-09-25 (VPN egress): sign-in succeeded, then
//! zero HTTP for minutes. Without a read timeout the first `sync()` hung
//! forever and `run()` never reached the doorbell, so nothing retried.
//!
//! Paused tokio time: the 30 s read timeout and the backoff sleeps elapse
//! instantly, and auto-advance is inhibited while `spawn_blocking` runs
//! (tokio 1.53 `time::pause` docs), so the SQLite apply tasks stay honest.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostos_client::{DirectClient, PostgrestError, SqliteStorage};
use tokio::time::Instant;

/// Accepts every connection and holds it open without reading or writing.
async fn black_hole() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            held.push(sock);
        }
    });
    format!("http://{addr}")
}

fn client(base: &str) -> DirectClient<SqliteStorage> {
    DirectClient::new(base, "anon-key", SqliteStorage::open_in_memory().unwrap()).unwrap()
}

#[tokio::test(start_paused = true)]
async fn stalled_server_fails_sync_instead_of_hanging() {
    let base = black_hole().await;
    let client = client(&base);
    let outcome = tokio::time::timeout(Duration::from_secs(120), client.sync())
        .await
        .expect("sync() hung past any sane read timeout");
    assert!(
        matches!(outcome, Err(PostgrestError::Transport(_))),
        "{outcome:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn run_retries_a_failed_first_sync_before_the_floor() {
    let base = black_hole().await;
    let client = client(&base);
    let attempts = Arc::new(AtomicUsize::new(0));
    let gaps = Arc::new(Mutex::new(Vec::new()));
    let (seen, gaps_w) = (attempts.clone(), gaps.clone());
    let started = Instant::now();
    let _ = tokio::time::timeout(
        Duration::from_secs(300),
        client.run("nostos:sub:u1", move |r| {
            assert!(r.is_err(), "black hole cannot produce a sync outcome");
            seen.fetch_add(1, Ordering::SeqCst);
            gaps_w.lock().unwrap().push(started.elapsed());
        }),
    )
    .await;
    let gaps = gaps.lock().unwrap();
    assert!(
        attempts.load(Ordering::SeqCst) >= 2,
        "sync attempts: {gaps:?}"
    );
    // The 60 s floor is not a retry: a failed sync must come back sooner.
    assert!(
        gaps[1].saturating_sub(gaps[0]) < Duration::from_secs(60),
        "{gaps:?}"
    );
}
