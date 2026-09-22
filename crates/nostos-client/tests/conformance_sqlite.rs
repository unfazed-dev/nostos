//! The shared conformance suite, run against the native `rusqlite` store.
//!
//! `nostos-core`'s own test runs the same cases against `InMemoryStorage`. Both
//! are needed and neither substitutes for the other: the in-memory store is a
//! `HashMap`, so it cannot fail the way a real transaction, a real `PRAGMA`,
//! or a real `DELETE` can. The browser Worker leg (`nostos-ffi-wasm` over OPFS)
//! is the third platform and is not wired up yet — the bridge has to export
//! `run_all` first.

use nostos_client::SqliteStorage;

#[test]
fn sqlite_storage_conforms() {
    let covered = nostos_core::conformance::run_all(|| {
        SqliteStorage::open_in_memory().expect("open an in-memory sqlite store")
    });
    assert_eq!(
        covered.len(),
        4,
        "every conformance case ran against rusqlite: {covered:?}"
    );
}
