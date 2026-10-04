# storlite

[![CI](https://github.com/tirtauntario/storlite/actions/workflows/ci.yml/badge.svg)](https://github.com/tirtauntario/storlite/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/tirtauntario/storlite?sort=semver)](https://github.com/tirtauntario/storlite/releases)
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
- **Scoped credentials:** grants per bucket and key prefix. Private by default.
- **Operations:** offline `doctor`, `check --full`, `gc`, per-bucket quotas,
  and verifiable backup/restore.

Tested against AWS CLI v2, Boto3, the Ruby SDK, Rails Active Storage and
Chrome (see [docs/compatibility.md](docs/compatibility.md)). It is **not** a
full Amazon S3 replacement: unsupported features are rejected explicitly, never
silently ignored. There is no replication or high availability.

## Install

| Method | Command |
|---|---|
| Docker (Linux amd64/arm64) | `docker pull ghcr.io/tirtauntario/storlite:latest` |
| Install script (Linux, macOS) | `curl -fsSL https://raw.githubusercontent.com/tirtauntario/storlite/main/install.sh \| sh` |
| Prebuilt binary | Download from [Releases](https://github.com/tirtauntario/storlite/releases) |
| From source (Rust 1.97+) | `cargo install --locked --git https://github.com/tirtauntario/storlite` |

Linux binaries are statically linked (musl) and run on any distribution.
macOS builds are meant for development; Linux is the supported production
platform. [docs/installation.md](docs/installation.md) covers every method,
checksum verification, systemd, Docker Compose, TLS and upgrades.

## Quick start (standalone, local only)

This setup serves plain HTTP on `127.0.0.1` for local testing only.

```sh
mkdir storlite && cd storlite
curl -fsSLO https://raw.githubusercontent.com/tirtauntario/storlite/main/docs/examples/config.example.toml
mv config.example.toml config.toml        # loopback HTTP, data in ./data

# One admin credential, written with mode 0600. The secret is in this file.
storlite credentials generate --id admin --enable --global-grant admin --output credentials.toml

storlite init  --config config.toml
storlite serve --config config.toml
```

In another terminal:

```sh
export AWS_ACCESS_KEY_ID=admin
export AWS_SECRET_ACCESS_KEY="$(sed -n 's/^secret_access_key = "\(.*\)"/\1/p' credentials.toml)"
export AWS_DEFAULT_REGION=us-east-1

aws --endpoint-url http://127.0.0.1:9000 s3 mb s3://documents
aws --endpoint-url http://127.0.0.1:9000 s3 cp ./report.pdf s3://documents/
aws --endpoint-url http://127.0.0.1:9000 s3 ls s3://documents/
```

## Quick start (Docker)

The container listens on all interfaces, so it requires TLS. A self-signed
certificate is enough for a trial.

```sh
mkdir storlite && cd storlite
base=https://raw.githubusercontent.com/tirtauntario/storlite/main/deploy
curl -fsSL -O "$base/compose.yaml" -O "$base/config.container.toml"

mkdir -m 0700 secrets
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD/secrets:/out" \
  ghcr.io/tirtauntario/storlite:latest \
  credentials generate --id admin --enable --global-grant admin --output /out/credentials.toml
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 365 \
  -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
  -keyout secrets/tls.key -out secrets/tls.crt
chmod 0400 secrets/*
sudo chown 65532:65532 secrets/*     # Linux only: the container runs as uid 65532

docker compose run --rm storlite init --config /etc/storlite/config.toml
docker compose up -d
aws --endpoint-url https://localhost:9000 --ca-bundle secrets/tls.crt s3 ls
```

See [docs/installation.md#docker](docs/installation.md#docker) for real
certificates, reverse proxies and upgrades.

## Client configuration

Clients need an explicit endpoint, the configured region (default
`us-east-1`), path-style addressing and SigV4.

<details>
<summary>AWS CLI, Boto3, Ruby / Rails, JavaScript</summary>

```ini
# ~/.aws/config
[profile storlite]
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
storlite:
  service: S3
  endpoint: https://storage.example.com:9000
  region: us-east-1
  bucket: documents
  access_key_id: <%= ENV["STORLITE_KEY_ID"] %>
  secret_access_key: <%= ENV["STORLITE_SECRET"] %>
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
| [docs/installation.md](docs/installation.md) | Install, run as a systemd service or in Docker, TLS, upgrades, uninstall |
| [docs/operations.md](docs/operations.md) | Configuration reference, credentials and grants, health checks, failure handling, backup/restore |
| [docs/compatibility.md](docs/compatibility.md) | Supported S3 operations, tested client versions, deviations |
| [SECURITY.md](SECURITY.md) | Security model and how to report vulnerabilities |
| [docs/benchmarks.md](docs/benchmarks.md) | Measured resource use and throughput |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development setup, tests, pull requests |
| [docs/releasing.md](docs/releasing.md) | How releases are built and published (maintainers) |
| [docs/SPEC.md](docs/SPEC.md), [docs/IMPLEMENTATION_PLAN.md](docs/IMPLEMENTATION_PLAN.md) | The design specification and acceptance matrix |
| [docs/implementation-report.md](docs/implementation-report.md), [docs/implementation-status.md](docs/implementation-status.md), [docs/test-evidence.md](docs/test-evidence.md), [docs/architecture-decisions/](docs/architecture-decisions/) | Design decisions, acceptance results and test evidence |

## License

Apache License 2.0. See [LICENSE](LICENSE).
