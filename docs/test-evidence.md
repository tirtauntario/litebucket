# Test evidence

All results below were produced by commands actually run on 2026-10-04.
"Not run" is stated explicitly where applicable.

## Environments

| Name | Details |
|---|---|
| macOS dev host | macOS 26.5.2 (Darwin 25.5.0), Apple Silicon arm64, 8 CPUs, 16 GiB RAM, APFS (disk ~95% full during runs); rustc 1.97.1 (Homebrew) |
| Linux container | Docker Desktop 29.7.2, kernel 7.0.12-linuxkit aarch64, image `rust:1.97.1-slim-trixie`; test temp dirs on an **ext4** named volume (`/tmp`) |
| Container image | `deploy/Dockerfile` → `gcr.io/distroless/cc-debian13:nonroot` runtime, 64.3 MB |

## Static checks

```sh
cargo fmt --all -- --check                                        # pass
cargo clippy --locked --all-targets -- -D warnings                # pass
cargo clippy --locked --all-targets --features failpoints -- -D warnings   # pass
```

## Automated tests

| Command | Environment | Result |
|---|---|---|
| `cargo test --locked --features failpoints` | macOS | 87 unit + 67 integration (admin 9, objects 14, protocol 13, multipart 9, operations 13, crash 9): **154 passed, 0 failed** |
| `cargo test --locked --features failpoints` (ext4 volume) | Linux container | 87 unit + 67 integration (admin 9, objects 14, protocol 13, multipart 9, operations 13, crash 9): **154 passed, 0 failed**; fmt and clippy (`-D warnings`, with failpoints) also pass in the container |

Breakdown by file (both environments): unit tests in `src/` (SigV4 AWS
vectors, aws-chunked decoder, XML, config, credentials, checksums, IDs/keys,
fsutil, metadata/migrations/queries, capacity, locks, listing proptest,
secret sealing/tamper detection, admin client parsing),
`tests/admin.rs` (key lifecycle, grants, rotation grace, last-admin guard,
buckets/quota/CORS, audit without secrets, protection-mode conversion and
wrong-master-key refusal, offline `admin recover`, the CLI over the real
socket), `tests/objects.rs`, `tests/protocol.rs`, `tests/multipart.rs`,
`tests/operations.rs` (in-process server, real files, real SQLite with
production durability settings), and `tests/crash.rs` (real binary in
subprocesses; SIGABRT via failpoints at every durable boundary; injected
EIO/ENOSPC/uncertain commits; forced storage-ID collisions; log redaction;
admin changes aborted before/after commit stay atomic with their audit
record, and issued secrets never reach the log).

Test configurations pin `min_disk_free_percent = 0` and a 64 MiB reserve so
results don't depend on host disk fullness (the production default reserve of
max(1 GiB, 5%) correctly refused writes on the 95%-full dev disk, which is how
this was discovered).

## Crash matrix (OPS-02)

Each case: seed state with a clean server → restart with
`STORLITE_FAILPOINTS=<point>=abort` → issue one request (process aborts) →
restart → verify → offline `gc --apply` and `check --full` must pass with no
staging files left.

| Scenario | Crash points | Verified |
|---|---|---|
| New object PUT | after register commit, create staging, write, after body received, after file sync, before publish, after publish before dir sync, after dir sync, before object commit, after object commit | never acknowledged; object absent or (only after commit) complete; prior objects intact |
| Overwrite PUT | same 10 points | old object intact until commit; new object complete after commit |
| Part replacement | same 10 points (`:part`) | old part mapping until commit; completion works afterwards |
| Multipart completion | after begin-completion commit, during assembly, write, after file sync, after publish before dir sync, after dir sync, before completion commit (guarded), before/after completion commit | partial output never visible; upload reopens with parts intact, or committed object + receipt |
| Delete | before/after delete commit | object present or gone; never partial |
| Garbage collection | before unlink, after unlink before row delete | live object intact; replay idempotent |

## Real-client interoperability (`scripts/interop.sh`)

Ambient AWS variables/config are cleared; every suite refuses any endpoint
other than `127.0.0.1`. Run both plainly and with `INTEROP_TLS=1` (throwaway
self-signed certificate; SDKs then use `STREAMING-UNSIGNED-PAYLOAD-TRAILER`).

| Suite | Client versions | HTTP | HTTPS |
|---|---|---|---|
| Ruby | aws-sdk-s3 1.229.0, aws-sdk-core 3.254.1, Ruby 3.4.10 | 9 runs, 31 assertions, 0 failures | 9 runs, 31 assertions, 0 failures |
| Boto3 | boto3/botocore 1.43.108, Python 3.14.6 | 7/7 OK | 7/7 OK |
| AWS CLI | aws-cli 2.37.9 (source install in venv) | 12/12 | 12/12 |
| Rails | Rails/Active Storage 8.1.3.1 (generated app, sqlite3) | 12/12 | 12/12 |
| Browser | Google Chrome 154.0.8037.93 headless | 9/9 | 9/9 |

Payload modes observed from real clients in the final run (server logs):
HTTP — `signed` (PutObject 62, UploadPart 17), `unsigned` (PutObject 5);
HTTPS — `streaming-unsigned-trailer` (PutObject 39, UploadPart 12),
`unsigned` (PutObject 28, UploadPart 5). The signed chunk modes are not emitted by these
clients' defaults; they are covered by AWS-published vectors and fixtures.

Issues found and fixed through these suites: Ruby `download_file` requires
`partNumber` reads (implemented); AWS CLI v2 sends
`x-amz-object-annotation-directive: EXCLUDE` on copies (accepted as a no-op);
Boto3's legacy presigner emits SigV2 (client must use `s3v4`; server correctly
refuses SigV2).

## Container smoke test

`deploy/Dockerfile` image with the container config (now `deploy/config.toml`; TLS, secrets
volume owned by uid 65532 mode 0400, `--read-only`, `--cap-drop ALL`,
`no-new-privileges`): `storlite init` → `serve` → AWS CLI 12 MB multipart
upload/download over TLS (ETag `…-2`, byte-identical) → `healthcheck` OK →
`docker stop` (log `drained: true`) → offline `check --full`: `result: OK`.

## Release packaging

| Check | Environment | Result |
|---|---|---|
| `cargo test --locked --features failpoints --target aarch64-unknown-linux-musl` (static release target) | Linux container (`rust:1.97.1-slim-trixie` + `musl-tools`, ext4 volume) | 82 unit + 57 integration: **139 passed, 0 failed** |
| `cargo build --release --locked --target aarch64-unknown-linux-musl` | same | 6.9 MB static binary; `storlite --version` OK |
| README standalone quick start, run verbatim (`config template` → `init --admin-key-output` (creates `master.key`) → `serve` → AWS CLI `mb`/`cp`/`ls` → `admin key create` → SIGTERM) | macOS | pass; socket mode 0600 while serving and removed at shutdown; shutdown `drained: true` |
| `deploy/setup.sh` with the local image (`STORLITE_PORT=9443`) after the access-key move | Docker Desktop 29.7.2 | creates `master.key` (0400), TLS pair, `admin.env` (0600, from `init` output); container `healthy`; admin key works for `s3 mb`; `docker compose exec storlite storlite admin key create/list`, `bucket create --quota`, `status`, `audit` work (actor `uid:65532`); a key scoped to one bucket is refused on another |
| Admin socket access in the compose stack | Docker Desktop 29.7.2 | `exec` as the server uid 65532: allowed. As uid 1000: refused (permission denied on the 0600 socket in the 0700 tmpfs). As root: also refused, because `cap_drop: ALL` removes root's file-permission override; use the default exec user |
| Earlier `deploy/setup.sh` runs (credentials-file era) | Docker Desktop 29.7.2 | re-run skips existing files and reports "already initialized"; invalid `config.toml` stops setup with the parse error |
| Config edit flow: `sed -i` on `config.toml` (`max_buckets = 1`) → `docker compose restart` | Docker Desktop 29.7.2 | new value in effect (second CreateBucket → `TooManyBuckets`) |

The GitHub Actions workflows (`.github/workflows/ci.yml`, `release.yml`) and
`install.sh` against a published release run only on GitHub; their first
run happens with the first tag. The Linux `chown 65532` step of the Docker
quick start was covered by the earlier container smoke test (secrets volume
owned by 65532, mode 0400), not by the compose bind-mount flow; the same
applies to the Linux `chown` branch of `deploy/setup.sh` (checked with
shellcheck only). Other uids are stopped by the socket's file mode before the peer-uid check
runs (see the compose row above); the peer-uid check itself is not exercised
by an automated test. The musl build
was last tested before the access-key move.

## Real-client interoperability after the access-key move

`scripts/interop.sh` now bootstraps with `init --admin-key-output` and uses
the generated admin key. Run on macOS with the same client versions as above:
HTTP and HTTPS (`INTEROP_TLS=1`) both pass all suites (Ruby 9 runs / 31
assertions, Boto3 7/7, AWS CLI 12/12, Rails 12/12, browser 9/9).

## Not run

- Power-loss / device-cache tests (only process crashes and injected errors).
- x86-64 Linux, XFS, and non-Docker Linux hosts.
- Long-duration soak tests.
