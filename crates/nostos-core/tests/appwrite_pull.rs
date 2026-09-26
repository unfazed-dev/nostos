use nostos_core::{ApplyEngine, AppwriteCursor, InMemoryStorage, Storage};
use nostos_domain::{Lsn, RowOp};
use std::collections::HashSet;

struct FailingHorizon(InMemoryStorage);

impl Storage for FailingHorizon {
    fn checkpoint(&self) -> nostos_core::Result<Lsn> {
        self.0.checkpoint()
    }

    fn apply_batch(
        &mut self,
        ops: &[(RowOp, u64)],
        checkpoint: Lsn,
        snapshot_tables: &HashSet<String>,
    ) -> nostos_core::Result<()> {
        self.0.apply_batch(ops, checkpoint, snapshot_tables)
    }

    fn pks_for_table(&self, table: &str) -> nostos_core::Result<Vec<String>> {
        self.0.pks_for_table(table)
    }

    fn delete_pks(&mut self, table: &str, pks: &[String]) -> nostos_core::Result<()> {
        self.0.delete_pks(table, pks)
    }

    fn clear(&mut self) -> nostos_core::Result<()> {
        self.0.clear()
    }

    fn save_horizon(&mut self, _horizon: &str) -> nostos_core::Result<()> {
        Err(nostos_core::StorageError::Backend("disk full".into()))
    }
}

#[test]
fn hidden_changes_advance_the_durable_cursor_without_leaking_rows() {
    let mut cursor = AppwriteCursor::fresh();
    let mut engine = ApplyEngine::new(InMemoryStorage::new());
    let page = r#"{"head":"5","scanned_through":"5","changes":[],"has_more":false}"#;

    let outcome = cursor.apply(&mut engine, page).expect("empty private page");
    assert_eq!(outcome.rows_applied, 0);
    assert_eq!(cursor.after(), 5);
    assert_eq!(engine.storage().horizon().unwrap(), Some("5".into()));
}

#[test]
fn visible_rows_apply_before_cursor_advances() {
    let mut cursor = AppwriteCursor::fresh();
    let mut engine = ApplyEngine::new(InMemoryStorage::new());
    let page = r#"{"head":"3","scanned_through":"3","changes":[{"seq":"2","table":"products","pk":"p1","op":"insert","row":{"id":"p1","name":"Oat"}}],"has_more":false}"#;

    let outcome = cursor.apply(&mut engine, page).expect("visible page");
    assert_eq!(outcome.rows_applied, 1);
    assert_eq!(engine.storage().row_count(), 1);
    assert_eq!(engine.storage().horizon().unwrap(), Some("3".into()));
}

#[test]
fn cursor_refuses_a_page_that_skips_backwards() {
    let mut cursor = AppwriteCursor::resume(4);
    let mut engine = ApplyEngine::new(InMemoryStorage::new());
    let bad = r#"{"head":"6","scanned_through":"3","changes":[],"has_more":true}"#;
    assert!(cursor.apply(&mut engine, bad).is_err());
    assert_eq!(cursor.after(), 4);
    assert_eq!(engine.storage().horizon().unwrap(), None);
}

#[test]
fn failed_horizon_save_does_not_claim_a_durable_cursor() {
    let mut cursor = AppwriteCursor::fresh();
    let mut engine = ApplyEngine::new(FailingHorizon(InMemoryStorage::new()));
    let page = r#"{"head":"5","scanned_through":"5","changes":[],"has_more":false}"#;

    assert!(cursor.apply(&mut engine, page).is_err());
    assert_eq!(cursor.after(), 0);
}
