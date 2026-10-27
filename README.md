# litebucket

[![CI](https://github.com/tirtauntario/litebucket/actions/workflows/ci.yml/badge.svg)](https://github.com/tirtauntario/litebucket/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/tirtauntario/litebucket?sort=semver)](https://github.com/tirtauntario/litebucket/releases)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

A small S3-compatible object store for one server. One binary, one data
directory, no database server or other services to run. Your application
talks to it with a normal S3 client, so you can move to AWS S3 later by
changing the endpoint and keys.

- **Works with standard S3 clients.** Tested with the AWS CLI v2, Boto3, the
  Ruby SDK, Rails Active Storage and browser uploads through presigned URLs.
  It covers the operations applications use: buckets, upload, download,
  delete, copy, listing, multipart uploads, range and conditional requests,
  CORS, and checksums (CRC32, CRC32C, CRC64NVME, SHA1, SHA256, Content-MD5).
- **Safe writes.** A write is confirmed only after the data is flushed to disk
  and recorded in the metadata database (embedded SQLite). Crash recovery is
  tested at every step.
- **Access keys managed at runtime.** `litebucket admin` creates, scopes (per
  bucket and key prefix), rotates (with a grace period) and revokes keys.
  Changes apply to the next request, with no restart. Secrets are stored
  encrypted and every change is logged. There is no anonymous access.
- **Simple to operate.** Health checks, Prometheus metrics, per-bucket quotas,
  offline checks and garbage collection, and verified backup and restore
  (online or offline).
- **Small.** A single binary of about 7 MB, a 64 MB container image, and
  under 10 MiB of memory while streaming a 1 GiB object
  ([benchmarks](docs/benchmarks.md)).

Unsupported S3 features are rejected with an error, never silently ignored.

## Why litebucket

I wanted S3 storage that is simple to run on one server, and that I can swap
for AWS S3 or any other S3-compatible service once an app needs it in
production. The app speaks plain S3 either way, so moving means changing the
endpoint and keys, not the code.

MinIO, the usual answer, has been moving away from open source. Most other
options are built for clusters: many nodes, replication, erasure coding. That
is more than I need when everything runs on one host.

litebucket is the drop-in for that case: one binary, one data directory, no
other services to run.

## Is it for you?

**A good fit if:**

- an application on one server needs S3-style storage: uploads, attachments,
  images, documents, backups, build artifacts;
- you want development, staging or a small production setup to use the same
  S3 code you would use with AWS S3;
- you want storage you can run, back up and restore as one directory on one
  Linux host (or in one Docker container).

**Not a fit if you need:**

- high availability, replication or more than one node. litebucket runs on
  one host, and your data is as safe as that host's disk and your backups;
- object versioning, Object Lock or retention, lifecycle rules, bucket
  policies, IAM or STS, event notifications, or static website hosting;
- server-side encryption. Objects are stored as uploaded; use an encrypted
  volume if you need encryption at rest;
- virtual-hosted-style addressing (`bucket.host`). Only path-style
  (`host/bucket/key`) works;
- a data directory on NFS, SMB or another shared volume, or to run natively on
  Windows.

[docs/compatibility.md](docs/compatibility.md) lists every supported
operation, header and checksum, the tested client versions, and where
litebucket differs from S3.

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

This setup serves plain HTTP on `127.0.0.1` for local testing only. It needs
the `litebucket` binary ([Install](#install)) and, for the example commands,
the [AWS CLI](https://aws.amazon.com/cli/).

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

[docs/README.md](docs/README.md) lists every document. The ones you need to
run litebucket:

| Document | Contents |
|---|---|
| [docs/installation.md](docs/installation.md) | Install, run as a systemd service or in Docker, manage access keys and buckets, TLS, upgrades |
| [docs/operations.md](docs/operations.md) | Configuration, admin API, health checks, failure handling, backup and restore |
| [docs/compatibility.md](docs/compatibility.md) | Supported S3 operations, tested client versions, differences from S3 |
| [docs/benchmarks.md](docs/benchmarks.md) | Measured memory use and throughput |

## Contributing

Bug reports, compatibility reports and pull requests are welcome. Read
[CONTRIBUTING.md](CONTRIBUTING.md) first: litebucket is deliberately small,
so larger changes start with an issue. Questions go to
[Discussions](https://github.com/tirtauntario/litebucket/discussions). This
project follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## Security

Report vulnerabilities privately, not in public issues. See
[SECURITY.md](SECURITY.md) for how to report and for the security model.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
