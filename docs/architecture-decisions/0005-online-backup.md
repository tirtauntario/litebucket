# ADR 0005 — Online backup by pausing garbage collection

**Status:** accepted (extends SPEC §16.1)

## Context

The v1 backup is offline: the operator stops the server so nothing changes
while the database snapshot and the files it references are copied. An
application that runs the store as a container accessory (Kamal, Compose)
wants nightly backups without stopping it. A naive live copy of the data
directory is racy: a file can be removed between the snapshot and the copy,
or the copied files can belong to a different state than the database.

## Decision

Take the backup inside the running server, through the admin socket
(`POST /v1/backup`, `litebucket admin backup <destination>`):

1. Pin: wait for a garbage-collection pass in progress, then keep collection
   paused until the backup ends. One backup at a time.
2. Snapshot the metadata with `VACUUM INTO` (one read transaction).
3. Copy and verify the files that snapshot references, write the manifests,
   publish `BACKUP_COMPLETE` (the same steps and format as the offline
   backup), then release the pin.

This is consistent because committed object and part files are immutable
and the collector is the only thing that removes them. A file referenced by
the snapshot is either still referenced or has become garbage after it, and
garbage stays on disk while the pin is held. Uploads in progress are not in
the snapshot; restore recovery reclaims their records as usual.

## Consequences

- Garbage accumulates for the length of a backup and is collected after it.
- The destination is on the server's file system; in a container it must be a
  mounted volume. Relative destinations are refused by the API (the CLI
  resolves them against its own working directory).
- The request answers only when the backup is complete, so the CLI waits
  without a read timeout. Shutdown waits for a running backup within its
  grace period; one cut short stays `BACKUP_INCOMPLETE` and is refused by
  `restore`.
- The offline backup remains for upgrades and stopped stores.
