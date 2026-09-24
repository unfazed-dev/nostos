# nostos-domain — the pure core

Business types and invariants for Nostos: `Lsn`, `RowOp` / `ReplicationEvent`,
`Predicate` (the dynamic boolean-tree filter, ADR-0012), `SyncSession`,
`Principal` (ADR-0010), `Tier`, sync rules (ADR-0031) and CRDT merge types
(ADR-0030).

Zero I/O, zero async, no framework types: it depends only on `serde`,
`serde_json`, `uuid`, `bytes` and `thiserror`, and every other crate depends on
it. If a type needs a runtime to test, it does not belong here (ADR-0001).

```sh
cargo test -p nostos-domain
```

See [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) §2.1.
