# Codex instructions — compact single-host object storage

## Read first

Read `SPEC.md` and `IMPLEMENTATION_PLAN.md` before changing the application. `SPEC.md` is the authoritative implementation contract. Its **INV-01 through INV-14** are mandatory; a short AGENTS file is not a replacement for the full specification. Read the relevant specification sections again before each milestone.

This repository starts with a specification, not a working or validated storage server. `compact-s3` is a working executable name only. Do not claim implementation, interoperability, crash safety, or performance until there is corresponding evidence.

## Fixed architecture

- Use Rust, Axum, Tokio, embedded SQLite, and ordinary sharded filesystem files.
- Keep one Cargo package and one deployed executable/process initially. Internal supervised tasks are allowed.
- Store metadata in SQLite using a patched bundled SQLite build through `rusqlite`. Run blocking database operations on bounded workers, not directly on async request threads.
- Store object bodies, staging files, and multipart parts in `area/id[0:2]/id[2:4]/id` paths, with `.tmp` only for staging. IDs are cryptographically random 128-bit internal identifiers, never client object keys.
- Do not introduce PostgreSQL, Redis, an ORM, a separate worker service, clustering, a public administration UI, or an external service prerequisite.
- Evaluate `s3s` behind a protocol boundary. Its sample filesystem implementation is not our backend. Verify its actual behavior and resolved dependencies before adopting it.

## Security and correctness boundaries

Never disable signature verification, authorization, checksums, SQLite durability, file synchronization, or request bounds to make a test pass. Never acknowledge an encryption, retention, ACL, or versioning request that is not implemented. Do not silently accept unsupported S3 subresources.

Preserve the raw URI and body frames until S3 signature/trailer processing is complete. Keep SigV4 payload hashes, ETags, and explicit S3 checksums separate. S3 multipart uploads are not HTTP multipart/form-data. Stream object bodies without collecting them in memory.

Every file creation must be registered, exclusively created, durably published, and committed through the specified blob lifecycle. Never overwrite an existing immutable blob file. Never delete a file on a guessed transaction outcome. Never garbage-collect a file still owned by an active writer, current object/part mapping, or protected commit. Open a selected read generation safely before allowing concurrent deletion.

Use one lifetime OS lock per data directory for serving and offline commands. Never unlink the lock file to bypass a conflict. Keep SQLite transactions brief; no transaction may span a network transfer or multipart assembly. Online cleanup uses indexed lifecycle records, not indiscriminate recursive filesystem deletion.

Test credentials and fixtures must remain local. Never load a default AWS profile or allow an integration harness to contact a real cloud endpoint. Do not print credentials, full presigned URLs, authorization headers, or sensitive object keys. Do not create usable credentials in example files.

## Implementation workflow

1. Inspect the repository and installed tools. Preserve unrelated work. Record the current milestone and relevant requirements in `docs/implementation-status.md`.
2. Complete the protocol/dependency spike in milestone 0. Record actual versions, resolved SQLite version, test results, unsupported body modes, and dependency risks. Fix or replace a failing boundary rather than silently changing the specification.
3. Implement milestone 1 and a tested vertical slice. Continue through the remaining milestones in order, with security and failure-path tests alongside each feature.
4. Create a lockfile for the application. Use stable Rust and verify supported toolchain/dependency versions; do not assume a version from this document is the newest.
5. Keep SQL migrations versioned and checked. The reference SQL is a starting point; transactional and filesystem invariants also require application enforcement.
6. Update `docs/compatibility.md`, `docs/operations.md`, and `docs/implementation-status.md` as behavior becomes real. Record deliberate local limits and deviations explicitly.
7. End each implementation session with changed files, tests actually executed, results, unresolved failures, and the next milestone. Mark unavailable tests as **not run**, not passed.

Do not stop after only proposing another architecture or producing stubs when implementation tools are available. Make the next bounded, testable increment. An unfinished milestone is not a finished v1 product.

## Validation commands

Once an executable Rust package and lockfile exist, run the applicable checks:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Add documented commands for protocol/SDK integration tests, Linux filesystem tests, crash/fault tests, and release builds as those harnesses are implemented. Run test-only fault instrumentation separately; it must not be remotely available in production. Check dependencies with the repository's chosen audit process without hiding unreviewed findings.

Do not replace real persisted-file tests with mocks only. Test databases must use the required production durability configuration unless the test explicitly covers a configuration rejection. Never label benchmarks as production results when safety features were disabled.

## Changes requiring special care

Storage format, object publication, garbage collection, lock ordering, authorization, request canonicalization, checksum interpretation, schema migrations, backup, and restore need explicit invariants and failure-path tests in the implementation change.

Prefer small modules and direct SQL over speculative frameworks. Avoid broad refactors unrelated to the current milestone. Do not add remote telemetry, deploy infrastructure, push commits, publish packages/images, or access production data without explicit user authorization.
