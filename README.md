# litebucket

[![CI](https://github.com/tirtauntario/litebucket/actions/workflows/ci.yml/badge.svg)](https://github.com/tirtauntario/litebucket/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/tirtauntario/litebucket?sort=semver)](https://github.com/tirtauntario/litebucket/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A compact, single-host, S3-compatible object store written in Rust. Metadata
lives in embedded SQLite (WAL, `synchronous=FULL`). Object bytes live in
immutable files sharded as `area/aa/bb/<id>`. It runs as one process with one
data directory, with no external database or worker service.

Use it when an application needs S3-style storage (Rails Active Storage,
uploads, backups, build artifacts) on a single server, and you want something
small, durable, and easy to back up.

- **S3 API:** path-style addressing and SigV4 (header and presigned URLs). All
  SDK payload modes, including checksum trailers, are supported. Checksums:
  CRC32, CRC32C, CRC64NVME, SHA1, SHA256 and Content-MD5. Operations:
  multipart uploads, CopyObject, DeleteObjects, ListObjectsV2, conditional
  reads/writes, ranges and CORS.
- **Durable by construction:** before any write is acknowledged, it is
  fsynced, published with a no-clobber rename, directory-synced and committed
  in SQLite. Crash recovery is tested at every durable boundary.
- **Access keys managed at runtime:** `litebucket admin` creates, scopes
  (per bucket and key prefix), rotates (with a grace period) and revokes keys
  through a local admin API; changes apply to the next request. Secrets are
  stored encrypted (AES-256-GCM) under a master key, with an audit log.
  Private by default.
- **Operations:** offline `doctor`, `check --full`, `gc`, per-bucket quotas,
  and verifiable backup/restore.

Tested against AWS CLI v2, Boto3, the Ruby SDK, Rails Active Storage and
Chrome (see [docs/compatibility.md](docs/compatibility.md)). It is **not** a
full Amazon S3 replacement: unsupported features are rejected explicitly, never
silently ignored. There is no replication or high availability.

## Install

| Method | Command |
|---|---|
| Docker Compose (Linux amd64/arm64) | `curl -fsSL https://raw.githubusercontent.com/tirtauntario/litebucket/main/deploy/setup.sh \| sh` ([details](#quick-start-docker)) |
| Install script (Linux, macOS) | `curl -fsSL https://raw.githubusercontent.com/tirtauntario/litebucket/main/install.sh \| sh` |
| Prebuilt binary | Download from [Releases](https://github.com/tirtauntario/litebucket/releases) |
| From source (Rust 1.97+) | `cargo install --locked --git https://github.com/tirtauntario/litebucket` |

Linux binaries are statically linked (musl) and run on any distribution.
macOS builds are meant for development; Linux is the supported production
platform. [docs/installation.md](docs/installation.md) covers every method,
checksum verification, systemd, Docker Compose, TLS and upgrades.

## Quick start (Docker)

One command creates everything (config, an admin key, a self-signed TLS
certificate), initializes the store and starts it:

```sh
mkdir litebucket && cd litebucket
curl -fsSL https://raw.githubusercontent.com/tirtauntario/litebucket/main/deploy/setup.sh | sh
```

You end up with:

```text
litebucket/
├── compose.yaml          service definition
├── .env                  image version and host port
├── config.toml           all settings, commented, with defaults
└── secrets/
    ├── admin.env         the first admin access key (AWS_* variables)
    ├── master.key        encrypts stored access keys; back it up
    ├── tls.crt           self-signed; replace with a real certificate
    └── tls.key
```

Create buckets and application keys with the admin CLI inside the container:

```sh
docker compose exec litebucket litebucket admin bucket create documents
docker compose exec litebucket litebucket admin key create --grant 'documents:read,list,write,delete'
```

To change a setting, edit `config.toml` and run `docker compose restart`. To
change the port or image version, edit `.env` and run `docker compose up -d`.
The script prints a ready-to-run AWS CLI example. To use your own hostname in
the certificate, run `LITEBUCKET_HOSTNAMES=storage.example.com sh` instead of
`sh`. See [docs/installation.md#docker](docs/installation.md#docker) for the
manual steps, real certificates and upgrades.

## Quick start (standalone, local only)

This setup serves plain HTTP on `127.0.0.1` for local testing only.

```sh
mkdir litebucket && cd litebucket
litebucket config template > config.toml     # loopback HTTP, data in ./data

# Creates ./master.key, the store, and a first admin key (written to admin.env, mode 0600).
litebucket init  --config config.toml --admin-key-output admin.env
litebucket serve --config config.toml
```

In another terminal:

```sh
set -a; . ./admin.env; set +a              # AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
export AWS_DEFAULT_REGION=us-east-1

aws --endpoint-url http://127.0.0.1:9000 s3 mb s3://documents
aws --endpoint-url http://127.0.0.1:9000 s3 cp ./report.pdf s3://documents/
aws --endpoint-url http://127.0.0.1:9000 s3 ls s3://documents/

# A key for an application, limited to one bucket (the secret is shown once)
litebucket admin --config config.toml key create --grant 'documents:read,list,write,delete'
```

## Client configuration

Clients need an explicit endpoint, the configured region (default
`us-east-1`), path-style addressing and SigV4.

<details>
<summary>AWS CLI, Boto3, Ruby / Rails, JavaScript</summary>

```ini
# ~/.aws/config
[profile litebucket]
region = us-east-1
endpoint_url = https://storage.example.com:9000
s3 =
  addressing_style = path
```

```python
import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="https://storage.example.com:9000",
    region_name="us-east-1",
    aws_access_key_id="app-key",
    aws_secret_access_key="...",
    # s3v4 is required for presigned URLs; the legacy presigner uses SigV2.
    config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
)
```

```yaml
# Rails config/storage.yml
litebucket:
  service: S3
  endpoint: https://storage.example.com:9000
  region: us-east-1
  bucket: documents
  access_key_id: <%= ENV["LITEBUCKET_KEY_ID"] %>
  secret_access_key: <%= ENV["LITEBUCKET_SECRET"] %>
  force_path_style: true
```

```js
import { S3Client } from "@aws-sdk/client-s3";

const s3 = new S3Client({
  endpoint: "https://storage.example.com:9000",
  region: "us-east-1",
  forcePathStyle: true,
  credentials: { accessKeyId: "app-key", secretAccessKey: "..." },
});
```

</details>

## Documentation

| Document | Contents |
|---|---|
| [docs/installation.md](docs/installation.md) | Install, run as a systemd service or in Docker, manage access keys and buckets, TLS, upgrades |
| [docs/operations.md](docs/operations.md) | Configuration reference, admin API, health checks, failure handling, backup/restore |
| [docs/compatibility.md](docs/compatibility.md) | Supported S3 operations, tested client versions, deviations |
| [SECURITY.md](SECURITY.md) | Security model and how to report vulnerabilities |
| [docs/benchmarks.md](docs/benchmarks.md) | Measured resource use and throughput |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development setup, tests, pull requests |
| [docs/releasing.md](docs/releasing.md) | How releases are built and published (maintainers) |
| [docs/SPEC.md](docs/SPEC.md) | The original design specification |
| [docs/implementation-report.md](docs/implementation-report.md), [docs/test-evidence.md](docs/test-evidence.md), [docs/architecture-decisions/](docs/architecture-decisions/) | Design decisions and test evidence |

## License

Apache License 2.0. See [LICENSE](LICENSE).
