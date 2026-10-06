# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

First public release (0.1.0).

### Added

- Online backup: `litebucket admin backup <destination>` (admin API
  `POST /v1/backup`) takes the same verified backup while the server keeps
  serving; garbage collection pauses until it completes.
- `http.trusted_proxy_addresses` takes ranges in CIDR notation as well as
  addresses, for applications in containers whose addresses change.
- S3-compatible API: path-style addressing, SigV4 header and presigned
  authentication, all SDK payload modes including checksum trailers,
  CRC32/CRC32C/CRC64NVME/SHA1/SHA256 and Content-MD5 verification.
- Operations: buckets, PutObject, GetObject/HeadObject (ranges, conditions,
  `partNumber`), DeleteObject(s), CopyObject, ListObjectsV2, multipart
  uploads, bucket CORS.
- Durable write pipeline (fsync, no-clobber publish, directory sync, SQLite
  commit before acknowledgement) with crash recovery at every boundary.
- Access keys stored in the metadata database, managed at runtime with
  `litebucket admin` over a local Unix-socket admin API: create (generated ids
  and 256-bit secrets, shown once), list, enable/disable, expiry, delete,
  rotate with a grace period, bucket/prefix grants and global grants. Changes
  apply to the next request and are recorded in an audit log; the last admin
  key cannot be removed.
- Secrets encrypted at rest with AES-256-GCM under a master key file
  (default), or stored plaintext (`[secrets] protection`); switching modes
  converts stored secrets at startup.
- `init` creates the master key and a first admin key; `admin recover`
  (offline) restores admin access, with `--reset-keys` for a lost master key;
  `master-key generate`.
- Buckets, quotas and CORS manageable through the admin API as well as S3.
- `config template [--docker]` prints a commented configuration file with
  every setting and its default.
- Offline `doctor`, `check --full`, `gc`, verified backup/restore (access
  keys included, encrypted; restore checks them with `--master-key-file`);
  `/livez`, `/readyz`, Prometheus `/metrics`.
- Static Linux (musl) and macOS release binaries, a multi-arch distroless
  image on GHCR, an install script, a one-step Docker Compose setup script
  (`deploy/setup.sh`) and a systemd unit.

[Unreleased]: https://github.com/tirtauntario/litebucket/commits/main
