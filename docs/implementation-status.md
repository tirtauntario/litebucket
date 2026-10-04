# Implementation status

Contract: `docs/SPEC.md` 1.0 and `docs/IMPLEMENTATION_PLAN.md`. Statuses reflect
executed evidence only; anything not executed is **not run**.

| Milestone | Status |
|---|---|
| 0 — protocol/dependency feasibility | done: dependencies pinned, runtime SQLite verified, adapter decision in ADR 0001 |
| 1 — executable and storage foundation | in progress |
| 2 — authenticated object vertical slice | not started |
| 3 — listing, copy, batch delete, quotas | not started |
| 4 — multipart | not started |
| 5 — protocol coverage and client verification | not started |
| 6 — operations, stress, backup, release | not started |

## Milestone 0 evidence

- `cargo test --lib` → `metadata::tests::bundled_sqlite_is_patched` passes; runtime
  `sqlite_version()` = 3.53.2 (source id in ADR 0001).
- DEP-01: versions recorded in `docs/architecture-decisions/0001-s3-protocol-adapter.md`.
- DEP-02: pending the S3 listener (milestone 2).
