# storlite operations guide

## Deployment model

- One `storlite` process per data directory, on one host, on a **local**
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

`docs/examples/config.example.toml` is the starting shape. Unknown keys are
errors. Relative paths resolve against the config file's directory. Byte
values are integers (`_bytes`), durations carry units (`_seconds`, `_ms`).

Precedence: built-in defaults < config file < environment
(`STORLITE_HTTP_LISTEN`, `STORLITE_MANAGEMENT_LISTEN`, `STORLITE_LOG_LEVEL`,
`STORLITE_LOG_FORMAT`) < CLI flags (`serve --listen`, `--management-listen`,
`--log-level`). Secrets never come from flags.

Keys added beyond the example (all optional):

| Key | Default | Purpose |
|---|---|---|
| `credentials_allow_group_read` | `false` | Accept a 0640-style credentials file (secret mounts with a dedicated group). World-readable is always refused. |
| `http.trusted_proxy_addresses` | `[]` | Required with `trusted_proxy_mode`; only these peer IPs may connect. |
| `http.max_clock_skew_seconds` | `900` | Header-signed request skew. |
| `database.queue_wait_ms` | `5000` | Wait for metadata queue space before `SlowDown`. |
| `limits.admission_timeout_ms` | `5000` | Wait for a transfer permit before `SlowDown`. |

Transport rules enforced at startup:

- Non-loopback plaintext is refused unless `trusted_proxy_mode = true` with a
  non-empty `trusted_proxy_addresses` (connections from other peers are
  dropped). Forwarded headers are never trusted; a proxy must preserve `Host`,
  the raw path/query, and the body (no normalization, decompression, trailer
  stripping, or full buffering).
- Loopback plaintext requires `allow_insecure_loopback_http = true`.
- TLS: set both `tls_certificate_file` and `tls_private_key_file` (PEM).

`storlite config check --config config.toml` validates without starting.

## Credentials

Credentials live in a separate TOML file (`credentials_file`), mode `0600`,
owned by the service user. See `docs/examples/credentials.example.toml`.

```sh
storlite credentials generate --id admin --enable --global-grant admin --output ./credentials.toml  # first key
storlite credentials generate --id app-key --output ./app-key.secret.toml   # 0600, disabled, never overwrites
# merge the [[credentials]] block into credentials.toml, add grants, set enabled = true
storlite credentials check --file ./credentials.toml
kill -HUP <pid>          # atomic reload; an invalid file keeps the previous set
```

Grants: per bucket and literal byte prefix, actions `read`, `list`, `write`,
`delete`, `manage_bucket` (whole bucket only); global `list_buckets`,
`create_bucket`, `admin`. The service refuses to start without at least one
enabled credential; placeholder secrets are rejected.

Revocation boundary: after a reload, new requests use the new set
immediately (including presigned URLs, which are re-validated against the
current set); requests already authorized finish under their original
snapshot. Rotation: add new key → reload → update apps → disable old key →
reload.

## Lifecycle commands

```sh
storlite init  --config config.toml     # new store; refuses a non-empty directory
storlite serve --config config.toml
storlite healthcheck --url http://127.0.0.1:9001/readyz
storlite doctor --config config.toml           # offline, read-only report
storlite check  --config config.toml --full    # + hash every referenced file, report untracked files
storlite gc     --config config.toml --dry-run # default
storlite gc     --config config.toml --apply   # recovery + delete eligible tracked garbage
storlite bucket set-quota --config config.toml --name documents --bytes 10G   # or --clear
storlite backup  --config config.toml --destination /backup/snapshot-001
storlite restore --source /backup/snapshot-001 --data-dir /srv/storlite-restored
```

Offline commands take the same exclusive lock; stop the server first.
`doctor`/`check`/`gc` never delete untracked files; they report them for an
operator decision. Exit codes: `0` OK, `2` problems found, `1` error,
`3` (serve) shutdown did not fully drain.

## Startup, readiness, shutdown

Startup: validate config → lock → validate ownership/permissions/same device →
open metadata (refuses a missing database in a non-empty directory, newer
schemas, and failed `quick_check`) → apply migrations → recovery (WRITING
blobs become garbage, COMPLETING uploads reopen) → load credentials → start
workers and the management listener → bind S3 → ready.

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
| `readyz` `halted` / writes return 503 | An fsync/publication `EIO` or an unreconciled commit outcome. Reads continue; GC pauses. | Inspect logs (`event=mutations_halted`), check the disk, restart (recovery runs at startup), then `storlite check --full`. |
| `readyz` `integrity_failure`, GET returns 500 `InternalError` | A referenced file is missing/short/corrupt. Metadata is never deleted automatically. | `storlite check --full` to enumerate; restore affected objects from backup. |
| `QuotaExceeded` (403) | Bucket logical quota reached. Reads/deletes still work. | Raise with `bucket set-quota`. |
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
- The backup contains object data, metadata, and the listing-cursor HMAC key:
  treat it as sensitive (it is created mode 0700). It does **not** include the
  configuration or credentials file; back those up separately (or provision new
  credentials after restore). Store identity and region are preserved.
- Same-host backups do not protect against host loss; copy the completed
  directory to another failure domain and verify it there.
- `restore` requires `BACKUP_COMPLETE`, verifies manifest, database and file
  hashes, restores into a new/empty directory, recreates the lock file, and
  runs the startup reference checks. Any missing or corrupt file fails the
  restore.

## Upgrades

Schema migrations are versioned and checksummed (`migrations/`). `serve`
applies compatible migrations transactionally at startup; an executable
refuses a store with a newer schema or a tampered migration record. Procedure:
stop → `backup` → install new binary → `doctor` (reports pending migrations)
→ `serve`. Keep the backup until the new version is verified.

## Container

Installation, Docker Compose, systemd and TLS setup are covered in
[installation.md](installation.md). Container specifics:

- Image: `ghcr.io/tirtauntario/storlite` (`linux/amd64`, `linux/arm64`), or
  build it with `docker build -f deploy/Dockerfile -t storlite:local .`.
- It runs as non-root uid/gid 65532 (distroless `nonroot`) and has one
  writable volume (`/data`, mode 0700). The compose example uses a read-only
  root filesystem. There is no shell, build toolchain, SQLite CLI or database
  server. The binary links glibc dynamically and uses rustls/ring for TLS
  (no OpenSSL).
- Secret files must be readable by the container user and not by others. On
  Linux hosts, run `chown 65532:65532 secrets/* && chmod 0400 secrets/*`.
  Docker Desktop maps bind-mount access for you.
