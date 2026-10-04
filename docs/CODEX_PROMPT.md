# Starter prompt for Codex

Copy the following into a Codex session opened in the target repository after placing this specification bundle at the repository root. Preserve and merge any existing repository instructions rather than overwriting unrelated work.

```text
Build the compact single-host S3-compatible object store defined in this repository.

Read AGENTS.md, SPEC.md, IMPLEMENTATION_PLAN.md, the reference SQL, and the example
configuration before writing implementation code. Follow the specification's
fixed architecture and INV-01 through INV-14. The working name compact-s3 is a
placeholder, not a reason to spend time naming the product.

Inspect the repository, then execute milestone 0 and proceed to milestone 1 and
a tested authenticated storage vertical slice. Implement incrementally with
real persisted-file tests. Continue through the plan in bounded increments;
do not stop after only generating another design or success-returning stubs.

Use Rust + Axum + Tokio, bundled SQLite metadata, and two-level sharded ordinary
files for objects, staging, and parts. Keep one executable/process. Do not add
PostgreSQL, Redis, clustering, or mandatory external services.

Verify the chosen S3 adapter and resolved SQLite release. Preserve signature,
checksum, trailer, authorization, resource-limit, and durability guarantees.
Do not disable client checksum defaults or use real AWS endpoints in tests.

Maintain docs/implementation-status.md, docs/compatibility.md, and operational
documentation. At each checkpoint, report files changed, commands actually run,
results, remaining failures, and the next milestone. Mark unexecuted tests as
not run, and do not claim full v1 compatibility until all release gates pass.
```

## Continuing a later session

```text
Read AGENTS.md, SPEC.md, IMPLEMENTATION_PLAN.md, and docs/implementation-status.md.
Inspect the existing implementation and test results, then implement the next
incomplete milestone without weakening the contract. Re-run relevant tests and
update the status and compatibility evidence. Preserve unrelated changes.
```
