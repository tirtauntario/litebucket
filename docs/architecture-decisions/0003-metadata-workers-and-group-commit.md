# ADR 0003 — Metadata workers and group commit

**Status:** accepted (milestone 6)

## Context

SPEC §5 requires one writer connection, bounded queues in front of every
worker, `journal_mode=WAL`, `synchronous=FULL`, and short `BEGIN IMMEDIATE`
transactions. Every object write needs two durable transactions (register the
`WRITING` blob, then commit the mapping). With one transaction per commit, the
first benchmark on macOS (where SQLite uses `F_FULLFSYNC`) reached only
≈45 small PUTs/s with 16 clients, p50 214 ms: requests queued behind one WAL
flush each.

## Decision

- Workers are dedicated OS threads owning `rusqlite` connections (one writer,
  N readers, default 2). Each has a bounded `tokio::sync::mpsc` queue; callers
  wait at most `database.queue_wait_ms` for space, then get a retryable
  `SlowDown`. No async SQLite wrapper is used.
- Readers run with `PRAGMA query_only=ON`. Every connection is opened with
  WAL, `synchronous=FULL`, `foreign_keys=ON`, and a busy timeout, and the
  settings are read back and verified (startup fails otherwise).
- **Group commit.** `Db::write_tx(name, f)` submits a transaction body. When
  the writer picks one up, it also drains any already-queued bodies (up to 64)
  and runs them inside a single `BEGIN IMMEDIATE` transaction, each in its own
  `SAVEPOINT`. A body that returns an error is rolled back to its savepoint
  and gets its own error; the others are unaffected. One `COMMIT` (one WAL
  sync) then makes the batch durable, and only after it returns are results
  delivered. A caller therefore never sees success before its changes are
  durable (INV-01), and a commit failure is reported to every member as
  not-committed (`BUSY`/`LOCKED`) or `CommitUncertain` (anything else; the
  connection is recycled and callers reconcile).
- No batching delay is introduced: a lone transaction commits immediately;
  batching only happens when work is already queued.
- Transactions remain short: bodies are pure SQL over already-received data
  (no I/O waits, no network transfer, no assembly).

## Consequences

- Same durability contract with far fewer WAL syncs under concurrency.
  Metadata-only DELETEs reach ~11k/s on Linux/ext4 and ~2k/s on macOS with 16
  clients; small PUTs are then bounded by their own file/directory syncs
  (`docs/benchmarks.md`).
- A body sees earlier bodies' uncommitted changes in the same batch (same
  connection). That is the same serialization order the writer would produce
  anyway; invariants are checked inside each body against that state.
- Test failpoints `before_commit:<name>` / `after_commit:<name>` /
  `commit:<name>` fire at the shared commit for every name in the batch; the
  crash matrix still exercises each named transition.
- Raw jobs (`Db::write`) still exist for statements that are not transactions
  (`wal_checkpoint`) and for reconciliation reads on the writer connection.
