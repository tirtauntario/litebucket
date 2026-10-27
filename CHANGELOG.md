# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

First public release (0.1.0).

### Added

- S3-compatible API with path-style addressing and SigV4 authentication
  (`Authorization` header and presigned URLs). All SDK payload modes,
  including checksum trailers, are supported. CRC32, CRC32C, CRC64NVME, SHA1,
  SHA256 and Content-MD5 are verified before an object becomes visible.
- Operations: ListBuckets, CreateBucket, HeadBucket, GetBucketLocation,
  DeleteBucket, PutObject, GetObject and HeadObject (ranges, conditions,
  `partNumber`), DeleteObject, DeleteObjects, CopyObject, ListObjectsV2,
  multipart uploads, and bucket CORS. Unsupported features are rejected with
  an error. See [docs/compatibility.md](docs/compatibility.md).
- Durable writes: each write is fsynced, published without overwriting,
  directory-synced and committed in SQLite before it is acknowledged. Crash
  recovery is tested at every step.
- Access keys stored in the metadata database and managed at runtime with
  `litebucket admin` over a local Unix-socket admin API: create (generated ids
  and 256-bit secrets, shown once), list, enable/disable, expiry, delete,
  rotate with a grace period, bucket/prefix grants and global grants. Changes
  apply to the next request and are recorded in an audit log. The last admin
  key cannot be removed.
- Secrets encrypted at rest with AES-256-GCM under a master key file
  (default), or stored as plaintext (`[secrets] protection`). Switching modes
  converts stored secrets at startup.
- `init` creates the master key and a first admin key. `admin recover`
  (offline) restores admin access, with `--reset-keys` for a lost master key.
  `master-key generate` creates a new master key.
- Buckets, per-bucket quotas and CORS rules manageable through the admin API
  as well as through S3.
- `config template [--docker]` prints a commented configuration file with
  every setting and its default.
- Plaintext HTTP only on loopback or from allowlisted proxies
  (`http.trusted_proxy_addresses`, addresses or CIDR ranges); built-in TLS
  otherwise.
- Verified backup and restore, offline (`litebucket backup`) or online while
  the server keeps serving (`litebucket admin backup`). Backups include access
  keys, still encrypted; `restore` checks them with `--master-key-file`.
- Offline `doctor`, `check --full` and `gc`. Health endpoints `/livez` and
  `/readyz`, and Prometheus metrics at `/metrics`.
- Static Linux (musl) and macOS release binaries, a multi-arch distroless
  image on GHCR, an install script, a one-step Docker Compose setup script
  (`deploy/setup.sh`) and a systemd unit.
- Dual license: MIT OR Apache-2.0.

[Unreleased]: https://github.com/tirtauntario/litebucket/commits/main
