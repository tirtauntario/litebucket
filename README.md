# storlite

A compact, single-host, S3-compatible object store written in Rust (Axum +
Tokio). Metadata lives in embedded SQLite (WAL, `synchronous=FULL`); object
bytes live in immutable files sharded Rails-style as `area/aa/bb/<id>`. One
process, one data directory, no external database or worker service.

- Path-style S3 API with SigV4 (header and presigned), all SDK payload modes
  including checksum trailers, CRC32/CRC32C/CRC64NVME/SHA1/SHA256 and
  Content-MD5, multipart uploads, CopyObject, DeleteObjects, ListObjectsV2,
  conditional reads/writes, ranges, CORS.
- Durable by construction: every acknowledged write is fsynced, published with
  no-clobber rename, directory-synced, and committed in SQLite before the
  response; crash recovery is tested at every durable boundary.
- Scoped credentials (bucket + literal prefix grants), private by default.
- Offline `doctor`, `check --full`, `gc`, quotas, and verifiable backup/restore.

Tested against AWS CLI v2, Boto3, the Ruby SDK, Rails Active Storage, and
Chrome (see `docs/compatibility.md`). It is **not** a full Amazon S3
replacement; unsupported features are rejected explicitly.

## Quick start (local)

```sh
cargo build --release
cp docs/examples/config.example.toml config.toml        # loopback HTTP, ./data
./target/release/storlite credentials generate --id dev --output dev.secret.toml
# create credentials.toml (mode 0600) from dev.secret.toml: add
#   global_grants = ["admin"] and enabled = true
./target/release/storlite init  --config config.toml
./target/release/storlite serve --config config.toml
```

```sh
aws --endpoint-url http://127.0.0.1:9000 s3 mb s3://documents
aws --endpoint-url http://127.0.0.1:9000 s3 cp ./report.pdf s3://documents/
```

Clients need an explicit endpoint, the configured region, path-style
addressing, and SigV4 (Boto3 presigning: `Config(signature_version="s3v4")`).

## Documentation

| Document | Contents |
|---|---|
| `docs/SPEC.md`, `docs/IMPLEMENTATION_PLAN.md` | The specification and acceptance matrix this implements |
| `docs/implementation-report.md` | What was built, how, and every assumption made |
| `docs/implementation-status.md` | Milestones and acceptance results |
| `docs/compatibility.md` | Supported operations, client versions, deviations |
| `docs/operations.md` | Configuration, credentials, deployment, recovery, backup/restore, upgrades |
| `docs/test-evidence.md` | Commands, environments, and results |
| `docs/benchmarks.md` | Measured resource use and throughput |
| `docs/architecture-decisions/` | ADRs |
| `SECURITY.md` | Security model and reporting |

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked                          # unit + integration
cargo test --locked --features failpoints    # + process crash/fault matrix
scripts/interop.sh                           # real clients (needs .interop venv, Ruby, Rails, Chrome)
INTEROP_TLS=1 scripts/interop.sh             # same over HTTPS
.interop/venv/bin/python scripts/bench.py    # resource/performance evidence
```

The `failpoints` feature is test-only; release builds never include it.
