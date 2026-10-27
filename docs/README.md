# litebucket documentation

New to litebucket? Start with the [README](../README.md), then
[installation.md](installation.md).

## User docs

For people who install, run and connect applications to litebucket.

| Document | Contents |
|---|---|
| [installation.md](installation.md) | Install the binary or image, run it with systemd or Docker Compose, manage access keys and buckets, set up TLS, upgrade. |
| [operations.md](operations.md) | Configuration rules, the admin API, health checks and metrics, what to do when something fails, backup and restore. |
| [compatibility.md](compatibility.md) | Which S3 operations, headers and checksums work, the client versions tested, and where litebucket differs from S3. |
| [benchmarks.md](benchmarks.md) | Measured memory use, file descriptors, throughput and listing speed. |
| [examples/config.example.toml](examples/config.example.toml) | Every configuration setting with its default (also printed by `litebucket config template`). |
| [../SECURITY.md](../SECURITY.md) | How to report a vulnerability, and what litebucket does and does not protect. |

## Design and maintainer docs

For contributors and anyone who wants to know why litebucket works the way it
does. You do not need these to use it.

| Document | Contents |
|---|---|
| [SPEC.md](SPEC.md) | The original design specification. Its invariants (INV-01..INV-14) are the contract the code must keep. |
| [architecture-decisions/](architecture-decisions/) | Architecture decision records (ADRs): one short file per major design choice and its reasons. |
| [implementation-report.md](implementation-report.md) | How the implementation met the spec, and every assumption made where the spec left room. |
| [test-evidence.md](test-evidence.md) | What was tested, on which hosts, with which commands and results. |
| [releasing.md](releasing.md) | How a version tag becomes binaries, a GitHub Release and a container image. |
| [../CONTRIBUTING.md](../CONTRIBUTING.md) | Development setup, tests, code rules and pull requests. |

The architecture decision records:

- [0001](architecture-decisions/0001-s3-protocol-adapter.md): S3 protocol handling written in-house instead of using `s3s`.
- [0002](architecture-decisions/0002-durable-publication-and-locking.md): how a write is made durable, the file lifecycle, and lock order.
- [0003](architecture-decisions/0003-metadata-workers-and-group-commit.md): metadata worker threads and group commit.
- [0004](architecture-decisions/0004-access-keys-in-sqlite-and-admin-api.md): access keys stored in SQLite and managed through a local admin API.
- [0005](architecture-decisions/0005-online-backup.md): online backup by pausing garbage collection.
