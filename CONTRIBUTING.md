# Contributing to litebucket

Bug reports, compatibility reports and pull requests are welcome. Report
security issues privately (see [SECURITY.md](SECURITY.md)), not in public
issues.

## Before you start

litebucket is deliberately small. [docs/SPEC.md](docs/SPEC.md) is the contract,
and its invariants (INV-01..INV-14) are mandatory. For anything beyond a bug
fix, such as a new S3 operation, a new config key or a storage-format change,
open an issue first to agree on scope. Unsupported S3 features are rejected
explicitly, so "add X" may be intentionally out of scope.

## Development setup

You need Rust (the exact toolchain is pinned in `rust-toolchain.toml`;
`rustup` installs it automatically) and a C compiler for the bundled SQLite.

```sh
cargo build
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features failpoints -- -D warnings
cargo test --locked                          # unit + integration (in-process server)
cargo test --locked --features failpoints    # + subprocess crash/fault matrix
cargo test --test objects put_03 -- --nocapture   # a single test
```

CI runs all of these on Linux x86_64, Linux arm64 and macOS, and builds the
Docker image. Linux results are authoritative for durability behavior. On
macOS, you can run the suite in a container:

```sh
docker run --rm -v "$PWD":/src -w /src rust:1.97.1-slim-trixie \
  cargo test --locked --features failpoints
```

Real-client interoperability (AWS CLI, Boto3, Ruby SDK, Rails, Chrome) uses
`scripts/interop.sh`. It expects a Python venv at `.interop/venv` (git-ignored)
with `boto3` and the AWS CLI v2, Ruby with `aws-sdk-s3` and Rails, and Google
Chrome for the browser suite. Run it with only the suites you have (for example
`scripts/interop.sh boto3 awscli`) when you change protocol handling.

## Code guidelines

- No `unsafe`. No `unwrap`/`expect` on network, filesystem or database
  results in production paths.
- Lock order: upload guard → per-key guard → metadata transaction. Never hold
  more than one key guard, and never await a guard inside a transaction.
- All SQL lives in `src/metadata/queries.rs`. Schema changes are new numbered
  files in `migrations/`; never edit an existing migration.
- New durable steps need a failpoint and a case in `tests/crash.rs`.
- Logs and metrics must never contain secrets, signatures, presigned URLs,
  bodies or (by default) object keys.
- Access-key secrets are returned only by key create/rotate. Never add them
  to listings, the audit log, errors or logs.
- Match the surrounding style. `cargo fmt` is the formatter.

`CLAUDE.md` summarizes the architecture for both human and AI-assisted
contributors. Design rationale lives in `docs/architecture-decisions/` and
`docs/implementation-report.md`.

## Pull requests

- Keep each PR to one change, with tests. Bug fixes need a regression test.
- Update the docs that the change affects: `docs/compatibility.md` for
  protocol behavior, `docs/operations.md` or `docs/installation.md` for
  operator-facing changes, and `CHANGELOG.md` under "Unreleased".
- Make sure `cargo fmt`, both `clippy` runs and the test suite pass.

By contributing, you agree that your contributions are licensed under the
Apache License 2.0, as stated in [LICENSE](LICENSE).
