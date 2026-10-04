# ADR 0002 — Durable publication, blob lifecycle, and lock order

**Status:** accepted (milestones 1–4)

## Blob lifecycle (typestate)

Every file is created through one pipeline in `src/store.rs`:

1. `Store::new_blob` allocates a random 128-bit `StorageId`, checks that no
   staging or final path exists (no-follow `statat`), and registers a `WRITING`
   row in its own durable transaction (`register`). A unique-key violation is a
   tracked collision → retry; an existing path is an untracked collision →
   retry without touching it. Result: `WriteTicket` (+ in-process
   `ActiveGuard`).
2. `receive` / `copy_into` exclusively create `staging/aa/bb/<id>.tmp`
   (`O_CREAT|O_EXCL|O_NOFOLLOW`, 0600) and stream bytes in transfer-buffer
   batches on the blocking pool, computing MD5, internal SHA-256, and requested
   S3 checksums in one pass. Result: `ReceivedBlob`.
3. The caller verifies `Content-MD5`, checksum headers/trailers, payload hash,
   and length; only then `sync()` flushes and `fsync`s the file
   (`F_FULLFSYNC` on macOS). Result: `StagedBlob`.
4. `publish()` creates/verifies the destination shard chain (syncing parent
   directories the first time a shard is seen in this process), renames with
   `renameat2(RENAME_NOREPLACE)` (`renameatx_np(RENAME_EXCL)` on macOS), then
   syncs the destination and source directories. Result: `PublishedBlob`.
5. Only a `PublishedBlob` yields `BlobFinal` commit facts. The commit
   transaction re-checks preconditions and quota, flips the blob to `READY`,
   installs the mapping, marks any replaced blob `GARBAGE`, and updates
   counters atomically.

Dropping a ticket/received/staged/published blob before commit spawns a
supervised task that marks the row `GARBAGE`; the `ActiveGuard` (also held by
any in-flight blocking write) keeps the collector away until all writers are
gone. Publication and commit run inside `Store::supervise` (a tracked task),
so a client disconnect cannot interrupt them.

## Unknown commit outcomes (INV-13)

A commit error other than `SQLITE_BUSY`/`LOCKED` is `CommitUncertain`: the
writer connection is recycled and the operation re-reads its blob's state.
`ready` → committed; `writing` → not committed (normal cleanup); anything else
or an error → the ticket is left for restart recovery and the store **halts
mutations** (`readyz` reports `halted`). An `EIO` from any fsync/publication
also halts mutations. GC pauses while halted.

## Late collisions

If a staging path appears after registration, or the final path is occupied
at publication, the preexisting path is preserved, our own staging file (if
any) is removed, our `WRITING` row is deleted (so cleanup can never target the
other file), and an integrity fault is raised. `check --full` reports the
foreign file as untracked; nothing deletes it automatically.

## Garbage collection

Indexed batches of `GARBAGE` rows whose `garbage_after_ms` has passed and that
are not in the active set. For each: unlink the final and staging paths
derived from the validated ID (ENOENT is fine), sync the touched shard
directories, then delete the row only if it is still `GARBAGE` and
unreferenced. Replays after a crash are idempotent. No recursive deletion, no
shard-directory removal online. The grace period is extra protection only;
ownership and reference checks are the mechanism. Offline `gc --apply` runs
restart recovery first and ignores the grace period (no readers can exist).

## Restart recovery

With the store lock held and before serving: refuse if any `WRITING` blob is
referenced (impossible by construction → integrity error); `COMPLETING`
uploads return to `OPEN` (their output blob is still `WRITING`); every
`WRITING` blob becomes `GARBAGE`. Committed state is authoritative; a published
file whose commit never happened is reclaimed. No full file scan at startup.

## Reads

`GET` takes the per-key guard, reads the row, opens the file (no-follow), and
releases the guard before streaming; overwrite/delete commits take the same
guard, so the selected generation is always the one opened. The open
descriptor keeps the bytes readable after unlink. Size is checked against
metadata before headers are sent; mismatches are integrity faults (500), never
`NoSuchKey`.

## Lock order

1. Upload finalization guard (`upload_locks`, keyed by upload ID) — part
   commit, completion (held across assembly), abort, expiry.
2. Destination per-key commit guard (`key_locks`, keyed by bucket ID + key).
3. A short metadata write transaction (group-committed, see ADR 0003).

No guard is awaited inside a transaction; at most one key guard is held at a
time (CopyObject releases the source guard before taking the destination
guard). The guard registry only holds keys with a live holder or waiter.

## The store lock

`store.lock` is opened `O_NOFOLLOW` and locked with
`flock(LOCK_EX|LOCK_NB)` for the process lifetime; it is never unlinked.
`serve` and every offline command take it. The lock is released last at
shutdown (after workers stop).

## Platform notes

Linux is authoritative for durability (`fsync` semantics, `renameat2`).
macOS is supported for development: `F_FULLFSYNC` for files and directories,
SQLite `fullfsync`/`checkpoint_fullfsync`, and `renameatx_np(RENAME_EXCL)`.
The full suite (including the crash matrix) passes on both; see
`docs/test-evidence.md`.
