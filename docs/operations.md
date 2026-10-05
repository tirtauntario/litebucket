# litebucket operations guide

## Deployment model

- One `litebucket` process per data directory, on one host, on a **local**
  filesystem (validated: APFS on macOS for development; ext4 on Linux for the
  authoritative test run). NFS/SMB/shared multi-host volumes are unsupported.
- The data directory holds everything: `store.lock`, `metadata.sqlite3`
  (+ SQLite-managed `-wal`/`-shm`), and the sharded `objects/`, `staging/`,
  `multipart/` areas (`area/aa/bb/<32-hex-id>`). All three areas must be on the
  same device; startup refuses otherwise.
- No PostgreSQL, Redis, or worker containers. Background work (GC, expiry,
  checkpoints) runs inside the process.
- Never run two processes against the same data directory. The exclusive
  `flock` on `store.lock` refuses a second `serve` or any offline command
  while the server runs. Do not delete `store.lock`, `metadata.sqlite3-wal`, or
  `metadata.sqlite3-shm` to "fix" startup.

## Configuration

`litebucket config template` prints a commented file listing every setting
with its default (`--docker` for the container layout; sources:
`docs/examples/config.example.toml`, `deploy/config.toml`). Unknown keys are
errors. Relative paths resolve against the config file's directory. Byte
values are integers (`_bytes`), durations carry units (`_seconds`, `_ms`).

Precedence: built-in defaults < config file < environment
(`LITEBUCKET_HTTP_LISTEN`, `LITEBUCKET_MANAGEMENT_LISTEN`, `LITEBUCKET_LOG_LEVEL`,
`LITEBUCKET_LOG_FORMAT`) < CLI flags (`serve --listen`, `--management-listen`,
`--log-level`). Secrets never come from flags.

Transport rules enforced at startup:

- Non-loopback plaintext is refused unless `trusted_proxy_mode = true` with a
  non-empty `trusted_proxy_addresses` (connections from other peers are
  dropped). Forwarded headers are never trusted; a proxy must preserve `Host`,
  the raw path/query, and the body (no normalization, decompression, trailer
  stripping, or full buffering).
- Loopback plaintext requires `allow_insecure_loopback_http = true`.
- TLS: set both `tls_certificate_file` and `tls_private_key_file` (PEM).

`litebucket config check --config config.toml` validates without starting.

## Access keys and the admin API

Access keys, grants, global grants, buckets, quotas and CORS live in the
metadata database (`credentials`, `credential_grants`,
`credential_global_grants`, `buckets`; every change is recorded in
`admin_audit`). They are managed through the admin API while the server runs.
See [installation.md](installation.md#managing-access-keys-and-buckets) for the
`litebucket admin` commands.

**Secret storage.** SigV4 needs the raw secret, so secrets cannot be hashed.
With `[secrets] protection = "encrypted"` (default) each secret is sealed with
AES-256-GCM under the 32-byte master key in `master_key_file` (base64, one
line, mode 0600/0400, outside `data_dir`). The associated data binds each
ciphertext to the store id and access key id, so a sealed value cannot be
moved to another key or store. With `protection = "plaintext"` secrets are
stored as-is. At startup, litebucket converts every stored secret to the
configured mode in one transaction; it refuses to start when any secret
cannot be read (missing or wrong master key, altered record) and changes
nothing in that case.

**Admin socket.** `[admin] socket` (default `./admin.sock` next to the config;
`/run/litebucket/admin.sock` in the Docker and systemd setups). The server
replaces a stale socket left by a crash, refuses a socket another process is
serving, sets mode 0600, removes it at shutdown, and serves only peers whose
effective uid (from the socket's peer credentials) is its own or root.

**Admin API** (JSON over HTTP/1.1 on the socket; used by `litebucket admin`):

| Method and path | Purpose |
|---|---|
| `GET /v1/status` | Version, store id, region, secret protection, key and bucket counts |
| `GET /v1/keys`, `GET /v1/keys/{id}` | Key metadata and grants (never secrets) |
| `POST /v1/keys` | Create; body `{access_key_id?, description?, enabled?, expires_at?, global_grants[], grants[]}`; returns the secret once |
| `PATCH /v1/keys/{id}` | `{enabled?, description?, expires_at?, clear_expiry?}` |
| `DELETE /v1/keys/{id}` | Delete the key and its grants |
| `POST /v1/keys/{id}/rotate` | `{grace_seconds}` (≤ 30 days); returns the new secret once |
| `POST /v1/keys/{id}/grants` | Add or replace `{bucket, prefix, actions[]}` |
| `POST /v1/keys/{id}/grants/remove` | `{bucket, prefix}` |
| `PUT`/`DELETE /v1/keys/{id}/global-grants/{name}` | `admin`, `list_buckets`, `create_bucket` |
| `GET`/`POST /v1/buckets`, `GET`/`DELETE /v1/buckets/{name}` | List, create `{name, quota_bytes?, cors?}`, show, delete (empty only) |
| `PUT /v1/buckets/{name}/quota` | `{quota_bytes: n \| null}` |
| `PUT`/`DELETE /v1/buckets/{name}/cors` | `{rules: [...]}` (same limits as PutBucketCors) |
| `GET /v1/audit?limit=N` | Newest audit records first |

Errors are `{"error": {"code", "message"}}` with 400 (`invalid_request`),
404 (`not_found`), 409 (`conflict`), 503 (`overloaded`) or 500.

**Consistency.** Each change runs in one metadata transaction together with
its audit record, then the in-memory key snapshot is rebuilt before the
response is sent: new, disabled, rotated and deleted keys take effect on the
next request, including presigned URLs, which are re-validated against the
current set. Requests already authorized finish under their original
snapshot. A change that would leave no enabled, unexpired admin key is
refused (409). Previous secrets past their rotation grace period stop
authenticating immediately and are deleted by the expiry task within a
minute.

**Grants:** per bucket and literal byte prefix, actions `read`, `list`,
`write`, `delete`, `manage_bucket` (whole bucket only); global
`list_buckets`, `create_bucket`, `admin`. The service refuses to start
without at least one enabled access key.

**Recovery:** `litebucket admin recover` (server stopped; it takes the store
lock) adds a new admin key directly in the database. It first checks that
the configured master key opens every stored secret, so keys sealed under
different master keys are never mixed; `--reset-keys` deletes all keys first
(for a lost master key).

## Lifecycle commands

```sh
litebucket init  --config config.toml     # new store + master key (if missing) + first admin key
litebucket serve --config config.toml
litebucket healthcheck --url http://127.0.0.1:9001/readyz
litebucket doctor --config config.toml           # offline, read-only report
litebucket check  --config config.toml --full    # + hash every referenced file, report untracked files
litebucket gc     --config config.toml --dry-run # default
litebucket gc     --config config.toml --apply   # recovery + delete eligible tracked garbage
litebucket admin  --config config.toml recover   # offline: new admin key (--reset-keys: drop all keys first)
litebucket master-key generate --output ./master.key
litebucket backup  --config config.toml --destination /backup/snapshot-001
litebucket restore --source /backup/snapshot-001 --data-dir /srv/litebucket-restored --master-key-file ./master.key
```

Offline commands take the same exclusive lock; stop the server first.
`doctor`/`check`/`gc` never delete untracked files; they report them for an
operator decision. Exit codes: `0` OK, `2` problems found, `1` error,
`3` (serve) shutdown did not fully drain.

## Startup, readiness, shutdown

Startup: validate config → lock → validate ownership/permissions/same device →
open metadata (refuses a missing database in a non-empty directory, newer
schemas, and failed `quick_check`) → apply migrations → recovery (WRITING
blobs become garbage, COMPLETING uploads reopen) → convert stored secrets to
the configured protection mode and load access keys (refused if any cannot be
decrypted or none is enabled) → start workers, the management listener and
the admin socket → bind S3 → ready.

Management listener (default `127.0.0.1:9001`, keep it private):

- `GET /livez` – process alive.
- `GET /readyz` – `200` when serving; `503` while starting/stopping, after
  mutations were halted, or after an integrity failure. Body:
  `{"ready","state","integrity_failure","capacity_pressure","mutations_halted"}`
  with `state` ∈ `writable`, `read_only_capacity_pressure`, `halted`,
  `integrity_failure`, `starting_or_stopping`.
- `GET /metrics` – Prometheus text, bounded labels (operation, status class).

SIGTERM/SIGINT: readiness off → stop accepting → drain requests and supervised
commits within `maintenance.shutdown_grace_seconds` → stop workers (truncating
WAL checkpoint) → release the lock last. In containers, stop the old process
before starting a new one; rolling overlap with two writers is unsupported.

## Failure handling

| Signal | Meaning | Operator action |
|---|---|---|
| `readyz` `halted` / writes return 503 | An fsync/publication `EIO` or an unreconciled commit outcome. Reads continue; GC pauses. | Inspect logs (`event=mutations_halted`), check the disk, restart (recovery runs at startup), then `litebucket check --full`. |
| `readyz` `integrity_failure`, GET returns 500 `InternalError` | A referenced file is missing/short/corrupt. Metadata is never deleted automatically. | `litebucket check --full` to enumerate; restore affected objects from backup. |
| `QuotaExceeded` (403) | Bucket logical quota reached. Reads/deletes still work. | Raise with `litebucket admin bucket set-quota`. |
| Startup: "cannot decrypt the secret of access key …" | Wrong or replaced master key, or an altered key record. Nothing was changed. | Restore the right `master_key_file`; if it is lost, `litebucket admin recover --reset-keys`. |
| Startup: "no enabled access keys" | Every key was deleted or disabled offline. | `litebucket admin recover` (server stopped). |
| 503 `ServiceUnavailable` "storage capacity" | Free space/inodes below reserve (max(`min_disk_free_bytes`, `min_disk_free_percent`)) or temporary-space cap. | Free space; reads, deletes, and aborts remain available. |
| 503 `SlowDown` | Permit or metadata-queue wait exceeded. | Client retries; tune limits if sustained. |
| Untracked files reported | Manual intervention or a collision. Never auto-deleted. | Investigate; remove manually only when certain. |

## File descriptors

Budget = sockets (concurrent connections) + downloads (`active_downloads`,
each holds one object file) + uploads (one staging file each) + 1 per
assembly input at a time + SQLite (≈ 3 files × (1 writer + readers)) + TLS
files + directory handles during publication + margin. With defaults (64
downloads, 16 uploads, 2 assemblies) a `nofile` limit of 4096 is ample;
`deploy/compose.yaml` sets 8192.

## Backup and restore

- Backups are **offline** (stop the server). `backup` normalizes interrupted
  work, writes `BACKUP_INCOMPLETE`, snapshots SQLite (`VACUUM INTO`), copies
  every referenced object/part file into the same sharded layout while
  verifying size and SHA-256, writes `files.jsonl` + `manifest.json`, syncs
  everything, then publishes `BACKUP_COMPLETE`.
- The backup contains object data, metadata (including access keys, their
  grants and the audit log), and the listing-cursor HMAC key: treat it as
  sensitive (it is created mode 0700). Access-key secrets are encrypted unless
  the store uses `plaintext` protection (`backup` warns). It does **not**
  include the configuration or the master key; back those up separately.
  Store identity and region are preserved.
- Same-host backups do not protect against host loss; copy the completed
  directory to another failure domain and verify it there.
- `restore` requires `BACKUP_COMPLETE`, verifies manifest, database and file
  hashes, checks with `--master-key-file` that every stored secret decrypts
  (before writing anything; refused without it when encrypted secrets exist,
  unless `--skip-key-check`), restores into a new/empty directory, recreates
  the lock file, and runs the startup reference checks. Any missing or corrupt
  file fails the restore.

## Upgrades

Schema migrations are versioned and checksummed (`migrations/`). `serve`
applies compatible migrations transactionally at startup; an executable
refuses a store with a newer schema or a tampered migration record. Procedure:
stop → `backup` → install new binary → `doctor` (reports pending migrations)
→ `serve`. Keep the backup until the new version is verified.

## Container

Installation, Docker Compose, systemd and TLS setup are covered in
[installation.md](installation.md). Container specifics:

- Image: `ghcr.io/tirtauntario/litebucket` (`linux/amd64`, `linux/arm64`), or
  build it with `docker build -f deploy/Dockerfile -t litebucket:local .`.
- It runs as non-root uid/gid 65532 (distroless `nonroot`) and has one
  writable volume (`/data`, mode 0700). The compose example uses a read-only
  root filesystem. There is no shell, build toolchain, SQLite CLI or database
  server. The binary links glibc dynamically and uses rustls/ring for TLS
  (no OpenSSL).
- Secret files must be readable by the container user and not by others. On
  Linux hosts, run `chown 65532:65532 secrets/* && chmod 0400 secrets/*`.
  Docker Desktop maps bind-mount access for you.
