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
| `cargo test --locked --features failpoints` | macOS | FINAL_MAC |
| `cargo test --locked --features failpoints` (ext4 volume) | Linux container | FINAL_LINUX |

Breakdown by file (both environments): unit tests in `src/` (SigV4 AWS
vectors, aws-chunked decoder, XML, config, credentials, checksums, IDs/keys,
fsutil, metadata/migrations/queries, capacity, locks, listing proptest),
`tests/objects.rs`, `tests/protocol.rs`, `tests/multipart.rs`,
`tests/operations.rs` (in-process server, real files, real SQLite with
production durability settings), and `tests/crash.rs` (real binary in
subprocesses; SIGABRT via failpoints at every durable boundary; injected
EIO/ENOSPC/uncertain commits; forced storage-ID collisions; log redaction).

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

Payload modes observed from real clients (server logs): HTTP — `signed`
(PutObject 62, UploadPart 17), `unsigned` (4); HTTPS —
`streaming-unsigned-trailer` (PutObject 39, UploadPart 12), `unsigned`
(PutObject 27, UploadPart 5). The signed chunk modes are not emitted by these
clients' defaults; they are covered by AWS-published vectors and fixtures.

Issues found and fixed through these suites: Ruby `download_file` requires
`partNumber` reads (implemented); AWS CLI v2 sends
`x-amz-object-annotation-directive: EXCLUDE` on copies (accepted as a no-op);
Boto3's legacy presigner emits SigV2 (client must use `s3v4`; server correctly
refuses SigV2).

## Container smoke test

`deploy/Dockerfile` image with `deploy/config.container.toml` (TLS, secrets
volume owned by uid 65532 mode 0400, `--read-only`, `--cap-drop ALL`,
`no-new-privileges`): `storlite init` → `serve` → AWS CLI 12 MB multipart
upload/download over TLS (ETag `…-2`, byte-identical) → `healthcheck` OK →
`docker stop` (log `drained: true`) → offline `check --full`: `result: OK`.

## Not run

- Power-loss / device-cache tests (only process crashes and injected errors).
- x86-64 Linux, XFS, and non-Docker Linux hosts.
- Long-duration soak tests.
