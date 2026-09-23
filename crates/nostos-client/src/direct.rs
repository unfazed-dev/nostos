//! Direct mode's native driver: the loop that has no server in it.
//!
//! [`postgrest`](crate::postgrest) supplies the HTTP I/O and
//! [`doorbell`](crate::doorbell) the wake-up; `nostos_core::PullCursor` owns the
//! protocol. Nothing tied them together, so every native SDK would have written
//! the same loop — pull on a ring, pull on every reconnect, push what the
//! outbox holds, re-snapshot on a 410. This is that loop, once.
//!
//! The order inside one [`DirectClient::sync`] is push-then-pull, and it is not
//! arbitrary: a queued write pushed first comes back in the same pull as the
//! echo, so a device that goes offline immediately afterwards has already
//! stored the server's version of its own row rather than only its local one.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use nostos_client::{DirectClient, SqliteStorage};
//!
//! let storage = SqliteStorage::open("cairn.sqlite")?;
//! let client = DirectClient::new("https://ref.supabase.co", "anon-key", storage)?;
//! client.set_token(std::env::var("USER_JWT")?).await;
//! client.run(&format!("sub:{}", "the-user-uuid"), |r| {
//!     if let Err(e) = r {
//!         eprintln!("sync: {e}");
//!     }
//! })
//! .await?;
//! # Ok(()) }
//! ```

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostos_core::{
    ApplyEngine, Horizon, Outbox, PendingWrite, PullCursor, PullError, Storage, DEFAULT_MAX_TXNS,
};
use tokio::sync::mpsc;

use crate::doorbell::{self, DoorbellConfig, DoorbellError};
use crate::postgrest::{PostgrestError, PostgrestSource};

/// How many `drain` rounds one [`DirectClient::sync`] will make before
/// returning to the caller.
///
/// `drain` already caps the pages inside one call; this caps the calls. A
/// device that is genuinely this far behind is better served by returning —
/// the horizon is durable, the next ring or reconnect resumes — than by
/// looping until the app is killed.
const MAX_DRAINS_PER_SYNC: usize = 64;

/// The longest gap between doorbell reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// The longest a connected device will go without syncing while the doorbell
/// stays quiet.
///
/// The doorbell is an optimisation, not the contract. A ring that is never
/// sent is indistinguishable from "nothing changed": `realtime.send` swallows
/// its own failures by design, so a project whose `realtime.messages` has no
/// partitions accepts every trigger and delivers nothing (caught 2026-09-23 on
/// a live Supabase project — a device sat on a stale order status for ten
/// minutes while its UI read "Online"). Without a floor the only sync such a
/// device ever does is the one at startup.
///
/// ponytail: a fixed floor, not adaptive. Stretch it while the doorbell is
/// provably delivering if one pull per device per minute ever costs anything.
const SYNC_FLOOR: Duration = Duration::from_secs(60);

/// What one [`DirectClient::sync`] moved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Queued writes accepted by PostgREST.
    pub pushed: usize,
    /// Rows committed to local storage.
    pub rows_applied: usize,
    /// This sync took a full picture instead of resuming the log — either
    /// because the device had never synced, or because its horizon had been
    /// pruned away. Surfacing it matters: it is the one path that reaps local
    /// rows.
    pub resnapshotted: bool,
}

/// A device syncing straight from Postgres — no Nostos server anywhere.
///
/// Owns the three things that must agree: the HTTP source, the apply engine
/// over the device's own storage, and the cursor. They are behind one type so
/// a caller cannot pull with one cursor and apply with another.
pub struct DirectClient<S> {
    /// `set_token` needs `&mut`, and a sync must not overlap another sync, so
    /// one async mutex serves as both the token guard and the sync lock.
    source: tokio::sync::Mutex<PostgrestSource>,
    engine: Arc<Mutex<ApplyEngine<S>>>,
    cursor: Arc<Mutex<PullCursor>>,
    base_url: String,
    apikey: String,
    token: Mutex<Option<String>>,
    /// Set while the device has never synced. The change log is not a history
    /// of the database — it begins where `nostos link` installed the trigger —
    /// so a device with no horizon cannot reach the rows that predate it by
    /// pulling, no matter how far back it asks. Only `cairn_snapshot` has
    /// them. Cleared by the bootstrap in [`DirectClient::sync`], and set again
    /// by [`DirectClient::sign_out`], which wipes the storage this was read
    /// from.
    needs_bootstrap: AtomicBool,
}

impl<S> DirectClient<S>
where
    S: Storage + Outbox + Send + 'static,
{
    /// Open a client over `storage`, resuming from whatever horizon it holds.
    ///
    /// A database with no horizon has never synced, so its first
    /// [`sync`](Self::sync) bootstraps from `cairn_snapshot` rather than
    /// pulling: the log only holds what has changed since the trigger was
    /// installed, and pulling would silently skip everything older.
    ///
    /// # Errors
    /// [`PostgrestError::BadUrl`] if `base_url` is not a usable PostgREST base.
    pub fn new(base_url: &str, apikey: &str, storage: S) -> Result<Self, PostgrestError> {
        let source = PostgrestSource::new(base_url, apikey.to_string())?;
        let (cursor, bootstrap) = match storage.horizon() {
            Ok(Some(h)) => (PullCursor::resume(Horizon::new(h), DEFAULT_MAX_TXNS), false),
            // A storage that cannot read its own horizon is a storage that
            // cannot have written one: start over rather than refuse to open.
            Ok(None) | Err(_) => (PullCursor::fresh(), true),
        };
        Ok(Self {
            source: tokio::sync::Mutex::new(source),
            engine: Arc::new(Mutex::new(ApplyEngine::new(storage))),
            cursor: Arc::new(Mutex::new(cursor)),
            base_url: base_url.trim_end_matches('/').to_string(),
            apikey: apikey.to_string(),
            token: Mutex::new(None),
            needs_bootstrap: AtomicBool::new(bootstrap),
        })
    }

    /// The apply engine, for reads and for queueing writes on its outbox.
    #[must_use]
    pub fn engine(&self) -> &Arc<Mutex<ApplyEngine<S>>> {
        &self.engine
    }

    /// Present `jwt` as the signed-in user, on both the pull and the doorbell.
    pub async fn set_token(&self, jwt: impl Into<String>) {
        let jwt = jwt.into();
        self.source.lock().await.set_token(jwt.clone());
        *self.token.lock().expect("set_token: token mutex poisoned") = Some(jwt);
    }

    /// Drop back to the anon key — a sign-out, not a disconnect.
    pub async fn clear_token(&self) {
        self.source.lock().await.clear_token();
        *self
            .token
            .lock()
            .expect("clear_token: token mutex poisoned") = None;
    }

    /// Push every queued write, then pull until caught up.
    ///
    /// A device that has never synced bootstraps from `cairn_snapshot` first:
    /// see [`Self::new`]. The push still runs ahead of it, so a write queued
    /// offline before the first sync is in the server's picture rather than
    /// only in the reap exemption.
    ///
    /// A `410` on the way in is not an error to the caller: the log was pruned
    /// past this device's horizon, so it re-snapshots once and carries on. Only
    /// a second `410` in the same sync propagates, because that is a server
    /// pruning faster than a device can read, which retrying cannot fix.
    ///
    /// # Errors
    /// Any [`PostgrestError`] the push or the pull did not absorb.
    pub async fn sync(&self) -> Result<SyncOutcome, PostgrestError> {
        let source = self.source.lock().await;
        let mut out = SyncOutcome {
            pushed: self.push_with(&source).await?,
            ..SyncOutcome::default()
        };

        // Only after the push, so the snapshot this device reaps against
        // already contains its own queued writes.
        if self.needs_bootstrap.load(Ordering::Relaxed) {
            out.rows_applied += self.snapshot_with(&source).await?;
            out.resnapshotted = true;
            // Last, so a snapshot that failed is retried by the next sync
            // rather than skipped into a pull that cannot see those rows.
            self.needs_bootstrap.store(false, Ordering::Relaxed);
        }

        for _ in 0..MAX_DRAINS_PER_SYNC {
            match source.drain(&self.engine, &self.cursor).await {
                Ok(drained) => {
                    out.rows_applied += drained.rows_applied;
                    if !drained.more {
                        return Ok(out);
                    }
                }
                Err(PostgrestError::Gone(_)) if !out.resnapshotted => {
                    out.rows_applied += self.snapshot_with(&source).await?;
                    out.resnapshotted = true;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Queue writes for the server and render them locally in the same
    /// storage transaction, so the row is on screen before the network has an
    /// opinion about it. Returns the outbox ids, in the order given.
    ///
    /// The queue alone is not enough: `watch()` reads `cairn_data`, not the
    /// outbox, so a write that is only enqueued shows the user nothing until
    /// the echo lands — and nothing at all if the server refuses it. The
    /// optimistic image is deliberately *not* a commit: it does not advance
    /// the checkpoint, and the server's echo later upserts the authoritative
    /// row over it (same contract as [`SyncClient::write_batch`]).
    ///
    /// [`SyncClient::write_batch`]: crate::SyncClient::write_batch
    ///
    /// # Errors
    /// The enqueue failed, in which case nothing was queued and nothing was
    /// rendered. A local apply that fails is logged and skipped — the write
    /// stays queued, because a row the device cannot draw is still a row the
    /// server must hear about.
    pub fn write_batch(&self, writes: &[PendingWrite]) -> Result<Vec<u64>, PullError> {
        let mut engine = self
            .engine
            .lock()
            .expect("write_batch: engine mutex poisoned");
        let ids = engine.storage_mut().enqueue_batch(writes.to_vec())?;
        for w in writes {
            if let Err(e) = engine.storage_mut().apply_local(w) {
                tracing::warn!(error = %e, table = %w.table, pk = %w.pk,
                    "instant-local apply failed; the write is still queued");
            }
        }
        Ok(ids)
    }

    /// Push every queued write and leave the pull alone.
    ///
    /// # Errors
    /// The first transient [`PostgrestError`]; permanent ones dead-letter the
    /// offending write instead of stopping the queue.
    pub async fn push(&self) -> Result<usize, PostgrestError> {
        let source = self.source.lock().await;
        self.push_with(&source).await
    }

    /// Re-read every synced table and reap whatever the snapshot does not
    /// confirm. The way back from a `410`, and the only path that deletes
    /// local rows nobody asked to delete.
    ///
    /// # Errors
    /// As [`Self::sync`].
    pub async fn resnapshot(&self) -> Result<usize, PostgrestError> {
        let source = self.source.lock().await;
        self.snapshot_with(&source).await
    }

    /// Sign out: drop the token and wipe the device — local rows, the outbox,
    /// the epoch, and the horizon (ADR-0029).
    ///
    /// The cursor is reset with the storage, and that pairing is the whole
    /// point: a surviving `xid8` would make the next principal resume in the
    /// middle of a log whose rows this device no longer has, so their data
    /// below the horizon would never arrive.
    ///
    /// # Errors
    /// The storage wipe failed. The token is cleared first and regardless, so a
    /// failed wipe cannot leave the device still pulling as the old user.
    pub async fn sign_out(&self) -> Result<(), PostgrestError> {
        // Held across the wipe so no sync is mid-flight with the old token.
        let mut source = self.source.lock().await;
        source.clear_token();
        *self.token.lock().expect("sign_out: token mutex poisoned") = None;

        // `Storage::clear`, not `Outbox::clear`: the storage one wipes rows,
        // epoch, rules checksum, horizon AND the outbox in one transaction.
        Storage::clear(
            self.engine
                .lock()
                .expect("sign_out: engine mutex poisoned")
                .storage_mut(),
        )
        .map_err(PullError::from)?;
        *self.cursor.lock().expect("sign_out: cursor mutex poisoned") = PullCursor::fresh();
        // The wipe took the horizon with it, so the next principal is a device
        // that has never synced — and reaches its rows the same way one does.
        self.needs_bootstrap.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Sync now, then on every ring, every reconnect, and at least once per
    /// [`SYNC_FLOOR`], forever.
    ///
    /// `scope` is the value the change-log trigger stamps — `sub:<uuid>` for a
    /// user-scoped app. `on_sync` sees every result, including failures: a sync
    /// that fails is not fatal (the next ring retries it), so it is reported
    /// rather than returned.
    ///
    /// Returns only when the doorbell fails in a way reconnecting cannot fix —
    /// a malformed token, a URL that is not a Realtime endpoint.
    ///
    /// # Errors
    /// The fatal [`DoorbellError`] that ended the loop.
    pub async fn run<F>(&self, scope: &str, mut on_sync: F) -> Result<Infallible, DoorbellError>
    where
        F: FnMut(Result<SyncOutcome, PostgrestError>) + Send,
    {
        let mut backoff = Duration::from_millis(500);
        loop {
            // Unconditionally, before the socket exists: a ring that arrived
            // while this device was away was never delivered to anyone.
            on_sync(self.sync().await);

            let token = self
                .token
                .lock()
                .expect("run: token mutex poisoned")
                .clone()
                .unwrap_or_else(|| self.apikey.clone());
            let config = DoorbellConfig::new(&self.base_url, &self.apikey, scope, token)?;

            // Capacity 1 coalesces: N rings and one ring produce the same pull
            // from the same horizon.
            let (rings, mut ring) = mpsc::channel(1);
            let listening = doorbell::listen(&config, rings);
            tokio::pin!(listening);

            let err = loop {
                tokio::select! {
                    ended = &mut listening => break ended.unwrap_err(),
                    Some(()) = ring.recv() => on_sync(self.sync().await),
                    () = tokio::time::sleep(SYNC_FLOOR) => on_sync(self.sync().await),
                }
            };
            if !err.is_retryable() {
                return Err(err);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Push the outbox through an already-held source lock.
    async fn push_with(&self, source: &PostgrestSource) -> Result<usize, PostgrestError> {
        let pending = self
            .engine
            .lock()
            .expect("push: engine mutex poisoned")
            .storage()
            .pending()
            .map_err(PullError::from)?;

        let mut pushed = 0;
        for (id, write) in pending {
            match source.push_write(&write).await {
                Ok(()) => {
                    self.engine
                        .lock()
                        .expect("push: engine mutex poisoned")
                        .storage_mut()
                        .mark_done(id)
                        .map_err(PullError::from)?;
                    pushed += 1;
                }
                // Permanent means the queue entry itself is wrong — an RLS
                // refusal, a malformed op. Retrying it forever would block
                // every write behind it, so it leaves the queue.
                Err(e) if e.is_permanent() => {
                    let engine = self.engine.lock().expect("push: engine mutex poisoned");
                    engine
                        .storage()
                        .mark_dead_letter_with_error(id, Some(&e.to_string()))
                        .map_err(PullError::from)?;
                }
                // Transient: stop here rather than burning the rest of the
                // queue against a server that is down, and keep the order.
                Err(e) => {
                    let engine = self.engine.lock().expect("push: engine mutex poisoned");
                    let _ = engine.storage().bump_attempts(id);
                    return Err(e);
                }
            }
        }
        Ok(pushed)
    }

    /// Re-snapshot through an already-held source lock.
    async fn snapshot_with(&self, source: &PostgrestSource) -> Result<usize, PostgrestError> {
        let body = source.fetch_snapshot().await?;

        // The user's own unacked writes must survive the reap: they are not in
        // the snapshot precisely because the server has not seen them yet.
        //
        // ponytail: one flat pk list covers every table, because
        // `apply_snapshot` takes one. Two tables would have to share a pk value
        // for that to exempt the wrong row; with uuid keys that does not
        // happen. Per-table exemption is an `apply_snapshot` signature change,
        // not a change here.
        let exempt: Vec<String> = self
            .engine
            .lock()
            .expect("snapshot: engine mutex poisoned")
            .storage()
            .pending()
            .map_err(PullError::from)?
            .into_iter()
            .map(|(_, w)| w.pk)
            .collect();

        let engine = Arc::clone(&self.engine);
        let cursor = Arc::clone(&self.cursor);
        let outcome = tokio::task::spawn_blocking(move || {
            let mut engine = engine.lock().expect("snapshot: engine mutex poisoned");
            let mut cursor = cursor.lock().expect("snapshot: cursor mutex poisoned");
            cursor.apply_snapshot(&mut engine, &body, &exempt)
        })
        .await
        .map_err(|e| PostgrestError::Transport(format!("snapshot task failed: {e}")))??;

        Ok(outcome.rows_applied)
    }
}
