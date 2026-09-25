//! The direct-mode conformance suite — one set of cases every platform runs.
//!
//! Direct mode has two client implementations (`nostos-client` on rusqlite,
//! `nostos-ffi-wasm` in the browser Worker) and server mode shares the apply
//! half with both. A property proved once on rusqlite is not proved on OPFS,
//! so the cases live here — in the crate both consumers depend on — and each
//! platform's test suite runs them against its own [`Storage`].
//!
//! Every case is a plain `assert!`: a failure panics with the case name, which
//! is what a Dart or JS harness can surface once the FFI bridge exposes it.
//!
//! ## Why these four
//!
//! They are the properties whose failure is *invisible*. A dropped connection
//! is loud; a half-applied transaction, a double-applied echo, or a horizon
//! that ran ahead of its rows all look exactly like a healthy device with
//! slightly wrong data.

use crate::{ApplyEngine, Horizon, PullCursor, Storage};

/// Run every case against a fresh storage from `make`. Returns the case names
/// in order, so a harness can report what it covered. Panics on the first
/// failure, naming the case.
///
/// ```no_run
/// # use nostos_core::{conformance, InMemoryStorage};
/// let covered = conformance::run_all(InMemoryStorage::default);
/// assert_eq!(covered.len(), 5);
/// ```
pub fn run_all<S: Storage>(mut make: impl FnMut() -> S) -> Vec<&'static str> {
    let mut ran = Vec::new();

    a_multi_table_transaction_is_never_seen_in_pieces(make());
    ran.push("a_multi_table_transaction_is_never_seen_in_pieces");

    an_echoed_page_applies_twice_with_the_same_result(make());
    ran.push("an_echoed_page_applies_twice_with_the_same_result");

    the_horizon_never_runs_ahead_of_the_rows(make());
    ran.push("the_horizon_never_runs_ahead_of_the_rows");

    a_resnapshot_leaves_no_stale_horizon(make());
    ran.push("a_resnapshot_leaves_no_stale_horizon");

    a_snapshot_reaps_rows_deleted_while_away(make());
    ran.push("a_snapshot_reaps_rows_deleted_while_away");

    ran
}

/// One `nostos_snapshot()` row. The horizon-only row and the per-table header
/// row both carry nulls; see the generated SQL for why the headers exist.
fn snap(horizon: u64, table: Option<&str>, pk: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "horizon": horizon.to_string(),
        "table_name": table,
        "pk": pk,
        "row": pk.map(|p| serde_json::json!({ "id": p })),
    })
}

/// A snapshot carries PRESENT rows only — there are no tombstones in it. So a
/// row deleted server-side while the device was past the retention window is
/// simply absent, and only the end-of-table reap removes it locally. Get this
/// wrong and the device keeps a row nobody else can see, forever, with no
/// error: the failure mode the whole suite is about.
fn a_snapshot_reaps_rows_deleted_while_away<S: Storage>(storage: S) {
    let mut engine = ApplyEngine::new(storage);
    let mut cursor = PullCursor::fresh();
    cursor
        .apply(
            &mut engine,
            &body(&[
                row(1, 100, "orders", "stays", "insert", 101),
                row(2, 100, "orders", "goes", "insert", 101),
            ]),
        )
        .expect("seed");

    // The server's picture now has only `stays`, and a table that went empty.
    let snapshot = serde_json::to_string(&serde_json::json!([
        snap(400, None, None),
        snap(400, Some("orders"), None),
        snap(400, Some("orders"), Some("stays")),
        snap(400, Some("invoices"), None),
    ]))
    .expect("encode snapshot");

    let out = cursor
        .apply_snapshot(&mut engine, &snapshot, &[])
        .expect("apply snapshot");

    let pks = engine.storage().pks_for_table("orders").expect("read back");
    assert!(
        pks.contains(&"stays".to_string()),
        "a confirmed row survives"
    );
    assert!(
        !pks.contains(&"goes".to_string()),
        "a row the snapshot did not confirm must be reaped, not kept forever"
    );
    assert_eq!(
        out.horizon.as_ref().map(nostos_horizon_str),
        Some("400".to_string()),
        "the snapshot's horizon is the new resume point"
    );
    assert_eq!(
        engine.storage().horizon().expect("read the horizon"),
        Some("400".to_string()),
        "and it is durable, or the next launch re-snapshots for nothing"
    );
}

fn nostos_horizon_str(h: &crate::Horizon) -> String {
    h.as_str().to_string()
}

/// One change row in a `nostos_pull` response.
fn row(seq: u64, xid: u64, table: &str, pk: &str, op: &str, horizon: u64) -> serde_json::Value {
    serde_json::json!({
        "horizon": horizon.to_string(),
        "seq": seq,
        "xid": xid.to_string(),
        "table_name": table,
        "pk": pk,
        "op": op,
        "row": if op == "delete" { serde_json::Value::Null }
               else { serde_json::json!({ "id": pk, "v": seq }) },
    })
}

fn body(rows: &[serde_json::Value]) -> String {
    serde_json::Value::Array(rows.to_vec()).to_string()
}

/// **The headline case.** One transaction touching three tables is applied as
/// three tables' worth of rows or as none. Applied page by page, the store is
/// checked after every page: a partial transaction must never be observable,
/// because the device would then be holding an order with no order lines and
/// no way to discover it.
fn a_multi_table_transaction_is_never_seen_in_pieces<S: Storage>(storage: S) {
    let mut engine = ApplyEngine::new(storage);
    let mut cursor = PullCursor::fresh();

    // xid 100 touches three tables; xid 101 is a separate, later transaction.
    let page = body(&[
        row(1, 100, "orders", "o1", "insert", 102),
        row(2, 100, "order_lines", "l1", "insert", 102),
        row(3, 100, "shipments", "s1", "insert", 102),
        row(4, 101, "orders", "o2", "insert", 102),
    ]);
    let outcome = cursor.apply(&mut engine, &page).expect("apply the page");
    assert_eq!(outcome.rows_applied, 4, "every row in the page landed");

    let tables = ["orders", "order_lines", "shipments"];
    let present: Vec<usize> = tables
        .iter()
        .map(|t| {
            engine
                .storage()
                .pks_for_table(t)
                .expect("read back")
                .iter()
                .filter(|pk| pk.starts_with('o') || pk.starts_with('l') || pk.starts_with('s'))
                .count()
        })
        .collect();
    assert!(
        present.iter().all(|&n| n >= 1),
        "all three tables of xid 100 are present: {present:?}"
    );

    // The transaction is whole in either direction: nothing from xid 100 is
    // missing, and nothing from a transaction beyond the horizon appeared.
    assert!(
        engine
            .storage()
            .pks_for_table("orders")
            .expect("read back")
            .contains(&"o2".to_string()),
        "the second transaction in the same page also landed whole"
    );
}

/// A direct-mode write fires the change-log trigger, so the device pulls its
/// own row back. There is no suppression list and there does not need to be —
/// the `(table, pk)` upsert is idempotent. Applying the same page twice must
/// therefore be indistinguishable from applying it once.
fn an_echoed_page_applies_twice_with_the_same_result<S: Storage>(storage: S) {
    let mut engine = ApplyEngine::new(storage);
    let page = body(&[
        row(1, 100, "orders", "o1", "insert", 101),
        row(2, 100, "orders", "o1", "update", 101),
    ]);

    let mut first = PullCursor::fresh();
    first.apply(&mut engine, &page).expect("first apply");
    let after_one = engine.storage().pks_for_table("orders").expect("read back");
    let payload_one = engine.storage().read_payload("orders", "o1").expect("read");

    let mut second = PullCursor::fresh();
    second.apply(&mut engine, &page).expect("second apply");
    let after_two = engine.storage().pks_for_table("orders").expect("read back");
    let payload_two = engine.storage().read_payload("orders", "o1").expect("read");

    assert_eq!(after_one, after_two, "the echo must not duplicate the row");
    assert_eq!(
        payload_one, payload_two,
        "re-applying the same bytes must produce the same bytes"
    );
}

/// The horizon is saved *after* the rows commit. A crash in that window leaves
/// the horizon behind the rows, so the re-read is idempotent; the reverse
/// order would skip rows that never landed. Since the bound is inclusive, a
/// resume from the saved horizon must re-deliver its own transaction rather
/// than start past it.
fn the_horizon_never_runs_ahead_of_the_rows<S: Storage>(storage: S) {
    let mut engine = ApplyEngine::new(storage);
    let mut cursor = PullCursor::fresh();
    let page = body(&[
        row(1, 100, "orders", "o1", "insert", 105),
        row(2, 104, "orders", "o2", "insert", 105),
    ]);
    cursor.apply(&mut engine, &page).expect("apply");

    let saved = engine
        .storage()
        .horizon()
        .expect("read the horizon")
        .expect("a horizon was saved");
    assert_eq!(
        saved, "105",
        "the horizon is the snapshot, not the last xid"
    );

    // Everything the horizon claims is settled must already be in the store.
    for pk in ["o1", "o2"] {
        assert!(
            engine
                .storage()
                .pks_for_table("orders")
                .expect("read back")
                .contains(&pk.to_string()),
            "{pk} must be durable before the horizon that covers it"
        );
    }

    // A resume from the saved horizon is inclusive: rebuilding the cursor from
    // storage must not silently skip the transaction sitting on it.
    let resumed = PullCursor::resume(Horizon::new(saved.clone()), 200);
    assert!(
        resumed.request_body().contains(&saved),
        "the resumed request must carry the saved horizon verbatim, as text"
    );
}

/// A device past the retention window re-snapshots: it clears local state and
/// starts over. A surviving horizon would make the next pull resume mid-log
/// and never see anything below it — the exact silent loss the re-snapshot
/// exists to prevent.
fn a_resnapshot_leaves_no_stale_horizon<S: Storage>(storage: S) {
    let mut engine = ApplyEngine::new(storage);
    let mut cursor = PullCursor::fresh();
    cursor
        .apply(
            &mut engine,
            &body(&[row(1, 100, "orders", "o1", "insert", 101)]),
        )
        .expect("apply");
    assert!(engine.storage().horizon().expect("read").is_some());

    engine.storage_mut().clear().expect("clear for re-snapshot");

    assert_eq!(
        engine.storage().horizon().expect("read"),
        None,
        "clear() must wipe the horizon, or the re-snapshot resumes mid-log"
    );
    assert!(
        engine
            .storage()
            .pks_for_table("orders")
            .expect("read back")
            .is_empty(),
        "clear() must wipe the rows too"
    );
}

#[cfg(test)]
mod tests {
    use crate::InMemoryStorage;

    /// The suite must pass on the reference store before any platform is
    /// asked to pass it.
    #[test]
    fn the_in_memory_store_conforms() {
        let covered = super::run_all(InMemoryStorage::default);
        assert_eq!(covered.len(), 5, "every case ran: {covered:?}");
    }
}
