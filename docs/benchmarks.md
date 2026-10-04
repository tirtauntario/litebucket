# Benchmarks and resource evidence

Measured 2026-10-04 with production durability settings: WAL +
`synchronous=FULL`, file fsync, destination and source directory syncs,
SigV4 authentication, MD5 + SHA-256 + CRC64NVME computed on every write. These
are developer-machine measurements, not capacity claims.

## Hosts

| Host | Details |
|---|---|
| macOS | macOS 26.5.2, Apple Silicon arm64, 8 CPUs, 16 GiB RAM, internal SSD (APFS, ~95% full). Durable syncs use `F_FULLFSYNC` (drive cache flush). |
| Linux | Docker Desktop VM, kernel 7.0.12-linuxkit aarch64 on the same Mac; data on an ext4 volume; `fsync`. |

## Memory and descriptors (PERF-01, CAP-01) — `scripts/bench.py`, macOS, release build

| Scenario | Wall time | Peak server RSS | Peak FDs |
|---|---:|---:|---:|
| Idle after start | — | 6.7 MiB | 28 |
| 1 GiB single PUT (Boto3, signed payload) | 13.1 s (78 MiB/s, client-bound: Boto3 hashes before sending) | **7.7 MiB** | 29 |
| 1 GiB single GET | 0.69 s (1.49 GiB/s) | **7.9 MiB** | 29 |
| 8 × 64 MiB concurrent PUT | 10.7 s | 10.5 MiB | 43 |
| 8 × 64 MiB concurrent GET | 0.84 s | 12.3 MiB | 35 |
| 1 GiB multipart (64 MiB parts, 4 concurrent) incl. assembly | 14.9 s | 13.3 MiB | 36 |

Memory follows the configured transfer buffers and concurrency, not object
size: streaming 1 GiB never raised RSS above 8 MiB, so whole-object buffering
would be immediately visible. The binary is 6.7 MB (release, stripped, thin
LTO); the container image is 64.3 MB.

## Small objects (metadata contention) — `examples/load.rs`, 16 concurrent clients, 1 KiB objects

| Host | PUT ops/s (p50 / p99 ms) | GET ops/s (p50 / p99) | DELETE ops/s (p50 / p99) |
|---|---|---|---|
| Linux ext4 | **1406** (11.2 / 15.0) | 39,191 (0.37 / 0.74) | 11,458 (1.24 / 3.16) |
| macOS APFS | 139 (112 / 168) | 19,146 (0.72 / 3.46) | 1,984 (8.0 / 14.1) |

History of the PUT figure (Linux): 794/s with group commit but a staging-
directory sync at file creation; 1406/s after removing that unnecessary sync
(ADR 0002). On macOS each PUT still pays three `F_FULLFSYNC`s for the file and
directories plus its share of WAL syncs; each one flushes the whole drive
cache, so macOS numbers are bounded by the device, not by storlite. With
Boto3 as the client (Python, 16 threads, one session per thread) the macOS
figure was 45/s before group commit and 54/s after; DELETE (metadata only)
shows the group-commit effect most clearly.

## Listing latency vs. key count — `scripts/bench.py`, macOS

| Keys in bucket | 1000-key page | Delimiter roll-up (`/`) |
|---:|---:|---:|
| 2,000 | 35.0 ms | 2.0 ms |
| 10,000 | 36.9 ms | 2.2 ms |
| 30,000 | 37.5 ms | 2.1 ms |

Page cost is flat in the total key count (bounded index range scans); a
roll-up of a 30k-key folder costs one row thanks to prefix-successor jumps.
Most of the page time is Boto3 XML parsing.

## SQLite

After ~32k objects and ~3 GiB of data: metadata database 12.3 MB, WAL 4.3 MB
(passive checkpoints every 60 s; truncating checkpoint at clean shutdown).

## Reproduce

```sh
cargo build --release
.interop/venv/bin/python scripts/bench.py --large-mib 1024 --small-count 2000
cargo run --release --example load -- 1600 16
docker run --rm -v "$PWD":/src:ro -v storlite-linux-target:/target -v storlite-linux-tmp:/tmp \
  -e CARGO_TARGET_DIR=/target -w /src rust:1.97.1-slim-trixie \
  cargo run --release --locked --example load -- 1600 16
```
