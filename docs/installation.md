# Installing and running litebucket

litebucket is one binary. It needs a config file, a master key file (which
encrypts the access keys stored in the database), and a data directory on a
local disk. Access keys and buckets are managed at runtime with
`litebucket admin`. This guide covers:

1. [Supported platforms](#supported-platforms)
2. [Installing the binary](#installing-the-binary)
3. [Running as a systemd service](#running-as-a-systemd-service) (standalone production)
4. [Docker](#docker)
5. [Managing access keys and buckets](#managing-access-keys-and-buckets)
6. [TLS and reverse proxies](#tls-and-reverse-proxies)
7. [Upgrading](#upgrading)
8. [Uninstalling](#uninstalling)

For configuration keys, health checks, failure handling and backup/restore,
see [operations.md](operations.md).

## Supported platforms

| Platform | Status |
|---|---|
| Linux x86_64 / aarch64, local ext4 or XFS | Production. Release binaries are static (musl), so they run on any distribution, including Alpine. |
| Docker on Linux (amd64 / arm64) | Production. The image is distroless and runs as non-root. |
| macOS (Apple Silicon / Intel) | Development and testing. Durability uses `F_FULLFSYNC`. |
| NFS, SMB, or any shared or multi-host volume | Not supported. |
| Windows | Not supported (use Docker or WSL2 with a Linux filesystem). |

Run one litebucket process per data directory. A lock file stops a second
process from opening the same data directory.

## Installing the binary

### Install script

```sh
curl -fsSL https://raw.githubusercontent.com/tirtauntario/litebucket/main/install.sh | sh
```

The script detects your OS and CPU, downloads the matching release archive,
verifies its SHA-256 checksum and installs `litebucket` to `/usr/local/bin`
(using `sudo` if needed). Options:

```sh
# a specific version, into ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/tirtauntario/litebucket/main/install.sh \
  | LITEBUCKET_VERSION=v0.1.0 LITEBUCKET_INSTALL_DIR="$HOME/.local/bin" sh
```

### Manual download

Each [release](https://github.com/tirtauntario/litebucket/releases) has these
archives:

| Archive | Platform |
|---|---|
| `litebucket-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64 |
| `litebucket-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz` | Linux arm64 |
| `litebucket-vX.Y.Z-aarch64-apple-darwin.tar.gz` | macOS Apple Silicon |
| `litebucket-vX.Y.Z-x86_64-apple-darwin.tar.gz` | macOS Intel |

Each archive contains the binary, the example config file, the systemd unit,
the license and the changelog.

```sh
v=v0.1.0; t=x86_64-unknown-linux-musl
curl -fsSLO "https://github.com/tirtauntario/litebucket/releases/download/$v/litebucket-$v-$t.tar.gz"
curl -fsSLO "https://github.com/tirtauntario/litebucket/releases/download/$v/litebucket-$v-$t.tar.gz.sha256"
sha256sum -c "litebucket-$v-$t.tar.gz.sha256"      # macOS: shasum -a 256 -c ...
tar -xzf "litebucket-$v-$t.tar.gz"
sudo install -m 0755 "litebucket-$v-$t/litebucket" /usr/local/bin/litebucket
litebucket --version
```

On macOS, a binary downloaded with a browser is quarantined by Gatekeeper.
Clear the flag with `xattr -d com.apple.quarantine litebucket`, or use the
install script, which downloads with `curl`.

### From source

Requires Rust 1.97 or newer and a C compiler (SQLite is compiled in).

```sh
# install the latest main branch into ~/.cargo/bin
cargo install --locked --git https://github.com/tirtauntario/litebucket
# or a tagged version
cargo install --locked --git https://github.com/tirtauntario/litebucket --tag v0.1.0

# or from a clone
git clone https://github.com/tirtauntario/litebucket && cd litebucket
cargo build --release --locked        # binary at target/release/litebucket
```

## Running as a systemd service

This sets up litebucket on a Linux host with this layout:

| Path | Owner / mode | Contents |
|---|---|---|
| `/usr/local/bin/litebucket` | root, 0755 | binary |
| `/etc/litebucket/config.toml` | root, 0644 | configuration |
| `/etc/litebucket/master.key` | litebucket, 0400 | encrypts the access keys in the database; back it up separately |
| `/etc/litebucket/tls.crt`, `tls.key` | litebucket, 0644 / 0600 | TLS certificate and key |
| `/var/lib/litebucket/data` | litebucket, 0700 | object store and metadata (including access keys) |
| `/run/litebucket/admin.sock` | litebucket, 0600 | admin API socket (created by systemd's `RuntimeDirectory`) |

**1. Create the service user and directories.**

```sh
sudo useradd --system --home-dir /var/lib/litebucket --shell /usr/sbin/nologin litebucket
sudo install -d -o root -g litebucket -m 0750 /etc/litebucket
sudo install -d -o litebucket -g litebucket -m 0750 /var/lib/litebucket
```

**2. Write the configuration.** `litebucket config template` prints a
commented file with every setting and its default
([`examples/config.example.toml`](examples/config.example.toml)). Generate it
and edit it, or write a minimal production file:

```sh
litebucket config template | sudo tee /etc/litebucket/config.toml >/dev/null
```

```toml
# /etc/litebucket/config.toml
data_dir = "/var/lib/litebucket/data"
region = "us-east-1"

[secrets]
master_key_file = "/etc/litebucket/master.key"

[admin]
socket = "/run/litebucket/admin.sock"

[http]
listen = "0.0.0.0:9000"
tls_certificate_file = "/etc/litebucket/tls.crt"
tls_private_key_file = "/etc/litebucket/tls.key"

[management]
listen = "127.0.0.1:9001"     # health and metrics; keep it private
```

litebucket refuses plaintext HTTP on non-loopback addresses. Either configure
TLS as shown, or see [TLS and reverse proxies](#tls-and-reverse-proxies).

**3. Create the master key** and give it to the service user:

```sh
sudo litebucket master-key generate --output /etc/litebucket/master.key
sudo chown litebucket:litebucket /etc/litebucket/master.key && sudo chmod 0400 /etc/litebucket/master.key
```

Copy the master key to your secret store now. Without it, the access keys in
the database (and in every backup) cannot be used. You can always create new
keys offline with `litebucket admin recover --reset-keys`; objects are never
affected.

**4. Install the TLS certificate and key** (see [TLS](#tls-and-reverse-proxies)),
then check the configuration:

```sh
sudo chown litebucket:litebucket /etc/litebucket/tls.key && sudo chmod 0600 /etc/litebucket/tls.key
sudo -u litebucket litebucket config check --config /etc/litebucket/config.toml
```

**5. Initialize the store** as the service user, so that the data directory
has the right owner. This prints the first admin access key once; store it
in your password manager:

```sh
sudo -u litebucket litebucket init --config /etc/litebucket/config.toml
```

**6. Install and start the unit.** The unit file is
[`deploy/litebucket.service`](../deploy/litebucket.service) (also in the release
archive).

```sh
sudo install -m 0644 litebucket.service /etc/systemd/system/litebucket.service
sudo systemctl daemon-reload
sudo systemctl enable --now litebucket
litebucket healthcheck                    # exit 0 when ready
journalctl -u litebucket -f               # JSON logs
```

Day-to-day:

| Task | Command |
|---|---|
| Manage access keys and buckets | `sudo litebucket admin --config /etc/litebucket/config.toml ...` ([details](#managing-access-keys-and-buckets)) |
| Apply config or certificate changes | `sudo systemctl restart litebucket` |
| Offline check, gc, backup | `sudo systemctl stop litebucket`, then `sudo -u litebucket litebucket <command> --config /etc/litebucket/config.toml` |

Tip: `alias litebucket-admin='sudo litebucket admin --config /etc/litebucket/config.toml'`.

To listen on a port below 1024 (for example 443), add
`AmbientCapabilities=CAP_NET_BIND_SERVICE` and
`CapabilityBoundingSet=CAP_NET_BIND_SERVICE` to the unit with
`systemctl edit litebucket`.

## Docker

Images are published to GitHub Container Registry for `linux/amd64` and
`linux/arm64`:

| Tag | Meaning |
|---|---|
| `ghcr.io/tirtauntario/litebucket:latest` | Latest stable release |
| `ghcr.io/tirtauntario/litebucket:X.Y.Z` | Exact version (recommended for production) |
| `ghcr.io/tirtauntario/litebucket:X.Y` | Latest patch of a minor version |

The image is based on `gcr.io/distroless/cc-debian13:nonroot`. It has no
shell and runs as uid/gid `65532`. Its only writable path is the `/data`
volume, and the entrypoint is `litebucket`, so any subcommand can be run with
`docker run ... ghcr.io/tirtauntario/litebucket <subcommand>`. A health check
probes `/readyz` on the internal management listener.

### Docker Compose: one-step setup

```sh
mkdir litebucket && cd litebucket
curl -fsSL https://raw.githubusercontent.com/tirtauntario/litebucket/main/deploy/setup.sh | sh
```

[`deploy/setup.sh`](../deploy/setup.sh) does the following, never overwriting a
file that already exists:

1. Downloads `compose.yaml`.
2. Writes `.env` with the image pinned to the exact version it pulled (for
   example `ghcr.io/tirtauntario/litebucket:0.1.0`) and the host port.
3. Writes `config.toml` from `litebucket config template --docker`. Every
   setting is listed, commented, with its default.
4. Creates `secrets/master.key`, which encrypts the access keys stored in the
   database.
5. Creates a self-signed certificate in `secrets/tls.crt` and `secrets/tls.key`,
   unless you put your own certificate there first.
6. Sets file modes, and on Linux gives the secrets to the container user
   (uid 65532, using `sudo`).
7. Validates `config.toml` and initializes the data volume. `init` creates
   the first admin access key, which the script saves to `secrets/admin.env`
   (mode 0600, readable by you).
8. Starts the container, waits until it is healthy, and prints test and
   admin commands.

Options are environment variables, set on the `sh` side of the pipe:

| Variable | Default | Purpose |
|---|---|---|
| `LITEBUCKET_PORT` | `9000` | Host port |
| `LITEBUCKET_HOSTNAMES` | none | Extra certificate names/IPs, comma-separated (`storage.example.com,10.0.0.5`) |
| `LITEBUCKET_IMAGE` | `ghcr.io/tirtauntario/litebucket:latest` | Image to use (pinned to its version in `.env`) |
| `LITEBUCKET_START` | `1` | `0` creates the files only, so you can edit `config.toml` before the first start |

```sh
curl -fsSL https://raw.githubusercontent.com/tirtauntario/litebucket/main/deploy/setup.sh \
  | LITEBUCKET_PORT=443 LITEBUCKET_HOSTNAMES=storage.example.com LITEBUCKET_START=0 sh
```

The script is safe to re-run: it fills in whatever is missing and reports an
already initialized volume. Back up `secrets/master.key` separately: backups
of the data volume contain the access keys only in encrypted form.

### Editing the configuration

All server settings live in `config.toml` next to `compose.yaml`. It is
mounted read-only into the container. Every available setting is in the file
as a commented `# key = default` line. Remove the `# ` and change the value:

```toml
[limits]
max_object_bytes = 10737418240        # was: # max_object_bytes = 107374182400
```

Then validate and apply:

```sh
docker compose run --rm litebucket config check
docker compose restart litebucket
```

| File | What it controls | Apply with |
|---|---|---|
| `config.toml` | Server settings (limits, timeouts, logging, TLS file paths, proxy mode) | `docker compose restart litebucket` |
| `.env` | `LITEBUCKET_IMAGE` (version) and `LITEBUCKET_PORT` | `docker compose up -d` |
| `secrets/tls.crt`, `secrets/tls.key` | TLS certificate chain and key | `docker compose restart litebucket` |
| Access keys, grants, buckets, quotas, CORS | Stored in the database | `docker compose exec litebucket litebucket admin ...` (applies immediately; [details](#managing-access-keys-and-buckets)) |

On Linux the files in `secrets/` belong to uid 65532, so edit them with `sudo`
and keep them mode 0400. To get a fresh copy of the commented template, for
example after an upgrade adds settings, run
`docker run --rm ghcr.io/tirtauntario/litebucket:<version> config template --docker`.

Do not change `data_dir`, `master_key_file`, the admin `socket` or the
`tls_*` paths in `config.toml`. They are paths inside the container, and
`compose.yaml` mounts the files there. `region` is fixed once the store is
initialized.

### Docker Compose: manual setup

These are the same steps the script runs, if you prefer to do them yourself:

```sh
mkdir litebucket && cd litebucket
curl -fsSL -O https://raw.githubusercontent.com/tirtauntario/litebucket/main/deploy/compose.yaml
image=ghcr.io/tirtauntario/litebucket:latest

# configuration
docker run --rm "$image" config template --docker > config.toml

# master key (written with mode 0600, owned by you)
mkdir -m 0700 secrets
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD/secrets:/out" "$image" \
  master-key generate --output /out/master.key

# TLS: copy a real certificate chain and key to secrets/tls.crt and
# secrets/tls.key, or create a self-signed pair:
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 365 \
  -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
  -keyout secrets/tls.key -out secrets/tls.crt

# Compose mounts secrets with their host owner and mode. litebucket refuses a
# master key file that group or others can read.
chmod 0400 secrets/*
sudo chown 65532:65532 secrets/*     # Linux only; Docker Desktop maps access itself

docker compose run --rm litebucket init   # prints the first admin key once; save it
docker compose up -d
docker compose ps                    # STATUS shows (healthy) when ready
```

Test it:

```sh
export AWS_ACCESS_KEY_ID=<access_key_id from init>
export AWS_SECRET_ACCESS_KEY=<secret_access_key from init>
export AWS_DEFAULT_REGION=us-east-1
aws --endpoint-url https://localhost:9000 --ca-bundle secrets/tls.crt s3 mb s3://documents
aws --endpoint-url https://localhost:9000 --ca-bundle secrets/tls.crt s3 ls
```

(`--ca-bundle` is only needed for a self-signed certificate.)

The compose file also sets a read-only root filesystem, drops all
capabilities, sets `no-new-privileges`, and sets a 40 s stop grace period so
in-flight writes drain. Data lives in the named volume `litebucket-data`.

### Operating the container

| Task | Command |
|---|---|
| Manage keys and buckets | `docker compose exec litebucket litebucket admin ...` |
| Offline check | `docker compose stop && docker compose run --rm litebucket check --full` |
| Lost every admin key | `docker compose stop && docker compose run --rm litebucket admin recover`, then `docker compose start` |
| Backup | see below |

Backups are offline. Stop the service, then write the backup to a host
directory owned by uid 65532:

```sh
sudo install -d -o 65532 -g 65532 -m 0700 /srv/litebucket-backups
docker compose stop
docker compose run --rm -v /srv/litebucket-backups:/backup litebucket \
  backup --destination /backup/$(date +%Y%m%d-%H%M%S)
docker compose start
```

### Building the image yourself

```sh
git clone https://github.com/tirtauntario/litebucket && cd litebucket
docker build -f deploy/Dockerfile -t litebucket:local .
LITEBUCKET_IMAGE=litebucket:local sh deploy/setup.sh ~/litebucket
```

## Managing access keys and buckets

Access keys, their grants, buckets, quotas and CORS rules live in the
metadata database. You manage them with `litebucket admin`, which talks to the
running server over a local Unix socket (`[admin] socket` in the config).
Changes apply to the very next S3 request; no restart or reload is needed.
Only the server's own user and root may use the socket.

| Deployment | Prefix every command with |
|---|---|
| Docker Compose | `docker compose exec litebucket litebucket admin` (as the default container user; root is refused because the compose file drops all capabilities) |
| systemd | `sudo litebucket admin --config /etc/litebucket/config.toml` |
| Local trial | `litebucket admin --config config.toml` |

### Access keys

```sh
# An application key limited to one bucket. The secret is shown once.
litebucket admin key create --description "rails app" \
  --grant 'documents:read,list,write,delete'

# Read-only access to one customer's prefix, expiring next year
litebucket admin key create --grant 'documents/customers/123/:read,list' \
  --expires-at 2027-01-01T00:00:00Z

# Print AWS_ACCESS_KEY_ID=/AWS_SECRET_ACCESS_KEY= lines, or write them to a 0600 file
litebucket admin key create --grant 'documents:read' --format env
litebucket admin key create --grant 'documents:read' --output app.env

litebucket admin key list                      # never shows secrets
litebucket admin key show SLABCDEFGHIJKLMNOPQR
litebucket admin key disable SLABCDEFGHIJKLMNOPQR
litebucket admin key enable SLABCDEFGHIJKLMNOPQR
litebucket admin key update SLABCDEFGHIJKLMNOPQR --description "billing" --no-expiry
litebucket admin key delete SLABCDEFGHIJKLMNOPQR
```

Generated access key ids look like `SL` followed by 18 letters and digits.
Secrets are 256-bit random values (43 characters) and are shown only when a
key is created or rotated. If one is lost, rotate the key.

**Rotation without downtime:** issue a new secret and keep the old one
working for a grace period while you redeploy the application:

```sh
litebucket admin key rotate SLABCDEFGHIJKLMNOPQR --grace 24h
```

With `--grace 0` (the default) the old secret stops working immediately.

### Grants

A grant gives a key actions on a bucket, optionally limited to a key prefix:
`bucket[/prefix]:action[,action...]`.

| Action | Allows |
|---|---|
| `read` | GetObject, HeadObject, presigned GET |
| `list` | ListObjectsV2 under the prefix, multipart listings |
| `write` | PutObject, CopyObject destination, multipart uploads |
| `delete` | DeleteObject, DeleteObjects, AbortMultipartUpload |
| `manage_bucket` | DeleteBucket and CORS through the S3 API (whole bucket only) |

Global grants: `admin` (everything), `list_buckets` (ListBuckets shows the
buckets the key has grants on), `create_bucket` (S3 CreateBucket).

```sh
litebucket admin grant add SLABCDEFGHIJKLMNOPQR 'reports:read,list'      # adds or replaces
litebucket admin grant remove SLABCDEFGHIJKLMNOPQR reports
litebucket admin global-grant add SLABCDEFGHIJKLMNOPQR list_buckets
```

litebucket refuses any change that would leave no enabled admin key.

### Buckets

```sh
litebucket admin bucket create documents --quota 10G --cors-file cors.json
litebucket admin bucket list
litebucket admin bucket show documents
litebucket admin bucket set-quota documents --bytes 50G     # or --clear
litebucket admin bucket set-cors documents --file cors.json
litebucket admin bucket clear-cors documents
litebucket admin bucket delete documents                    # only when empty
```

The CORS file is a JSON array of rules, or an S3 `CORSConfiguration` XML
document:

```json
[{"allowed_origins": ["https://app.example.com"],
  "allowed_methods": ["GET", "PUT"],
  "allowed_headers": ["*"],
  "expose_headers": ["ETag"],
  "max_age_seconds": 3600}]
```

S3 clients with an admin key (or the matching grants) can still create
buckets, delete them, and manage CORS through the S3 API.

### Audit log and status

```sh
litebucket admin audit --limit 20     # who changed what (never secrets)
litebucket admin status               # version, store id, secret protection, counts
```

Add `--json` to any `litebucket admin` command for machine-readable output.

### Lost admin keys or master key

With the server stopped:

```sh
litebucket admin recover --config config.toml               # adds a new admin key
litebucket admin recover --config config.toml --reset-keys  # deletes all keys first
```

Use `--reset-keys` when the master key is lost: stored secrets can no longer
be decrypted, so every key must be reissued. Buckets and objects are not
touched. Point `master_key_file` at a new key (`litebucket master-key
generate`) before running it.

### Secret protection

By default (`[secrets] protection = "encrypted"`) each secret is encrypted
with AES-256-GCM under the master key. With `protection = "plaintext"` the
secrets are stored as-is, and every backup contains usable secrets. Change
the setting and restart to convert all stored secrets; switching to plaintext
needs the master key one last time.

## TLS and reverse proxies

litebucket only accepts plaintext HTTP from the loopback interface. For any
other network access, pick one of these options.

**Built-in TLS (recommended).** Set `http.tls_certificate_file` and
`http.tls_private_key_file` to PEM files. The certificate file should contain
the full chain. Certificates are loaded at startup, so restart after
renewal. For example, with certbot:

```sh
certbot certonly --standalone -d storage.example.com \
  --deploy-hook 'install -o litebucket -m 0644 $RENEWED_LINEAGE/fullchain.pem /etc/litebucket/tls.crt &&
                 install -o litebucket -m 0600 $RENEWED_LINEAGE/privkey.pem /etc/litebucket/tls.key &&
                 systemctl restart litebucket'
```

**Reverse proxy on the same host.** Bind litebucket to loopback and let the
proxy terminate TLS:

```toml
[http]
listen = "127.0.0.1:9000"
allow_insecure_loopback_http = true
```

The proxy must preserve `Host`, the raw path and query, and the body. It must
not normalize, decompress, buffer whole requests, or strip trailers, because
SigV4 signs all of these. For nginx:

```nginx
server {
    listen 443 ssl;
    server_name storage.example.com;
    # ssl_certificate / ssl_certificate_key ...

    client_max_body_size 0;          # litebucket enforces its own limits
    proxy_request_buffering off;
    proxy_buffering off;
    proxy_http_version 1.1;

    location / {
        proxy_pass http://127.0.0.1:9000;   # no URI part: the request URI is passed unchanged
        proxy_set_header Host $http_host;
        proxy_read_timeout 3600s;           # large multipart completions answer only after commit
    }
}
```

The automated interoperability suite covers direct connections only. Test
your proxy with the clients you use before relying on it.

**Reverse proxy on another host.** Set `http.trusted_proxy_mode = true` and
list the proxy's IP addresses in `http.trusted_proxy_addresses`. Connections
from any other peer are dropped.

**Applications in containers** (Docker, Kamal) get new addresses on every
restart, so list their network's range instead, e.g.
`trusted_proxy_addresses = ["172.18.0.0/16"]` (`docker network inspect`
shows it). Every container on that network can then connect without TLS;
keep untrusted containers off it. Forwarded headers are never trusted. See
[operations.md](operations.md#configuration).

## Upgrading

Schema migrations run automatically at startup. A binary refuses a store
whose schema is newer than it understands. Always take a backup first.

**systemd:**

```sh
sudo systemctl stop litebucket
sudo install -d -o litebucket -g litebucket -m 0700 /srv/litebucket-backups
sudo -u litebucket litebucket backup --config /etc/litebucket/config.toml --destination /srv/litebucket-backups/pre-upgrade
curl -fsSL https://raw.githubusercontent.com/tirtauntario/litebucket/main/install.sh | LITEBUCKET_VERSION=vX.Y.Z sh
sudo -u litebucket litebucket doctor --config /etc/litebucket/config.toml    # reports pending migrations
sudo systemctl start litebucket
```

**Docker:** take a backup as shown above, set the new tag (or pull
`latest`), then:

```sh
docker compose pull
docker compose up -d        # stops the old container before starting the new one
```

Never run two containers against the same volume. Rolling updates with
overlapping writers are not supported.

Read [CHANGELOG.md](../CHANGELOG.md) before upgrading across minor versions.

## Uninstalling

```sh
# systemd
sudo systemctl disable --now litebucket
sudo rm /etc/systemd/system/litebucket.service /usr/local/bin/litebucket
# data and configuration (irreversible):
sudo rm -rf /var/lib/litebucket /etc/litebucket && sudo userdel litebucket

# Docker
docker compose down          # add -v to also delete the data volume (irreversible)
```
