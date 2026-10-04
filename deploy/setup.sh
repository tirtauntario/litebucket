#!/bin/sh
# Set up storlite with Docker Compose in one step.
#
#   mkdir storlite && cd storlite
#   curl -fsSL https://raw.githubusercontent.com/tirtauntario/storlite/main/deploy/setup.sh | sh
#
# Creates (never overwrites) in the target directory:
#   compose.yaml            the service definition
#   .env                    STORLITE_IMAGE pinned to the exact version, STORLITE_PORT
#   config.toml             commented server configuration (edit, then restart)
#   secrets/credentials.toml  an enabled admin access key
#   secrets/tls.crt, tls.key  a self-signed certificate, unless you put real ones there first
# then initializes the data volume and starts the container. Running it again
# skips what already exists, so it is safe to re-run after a partial failure.
#
# Usage: setup.sh [DIR]          (default: current directory)
# Environment:
#   STORLITE_IMAGE      image to use (default ghcr.io/tirtauntario/storlite:latest)
#   STORLITE_PORT       host port (default 9000)
#   STORLITE_HOSTNAMES  extra names/IPs for the self-signed certificate,
#                       comma-separated, e.g. "storage.example.com,10.0.0.5"
#   STORLITE_START=0    create files only; do not initialize or start
#   STORLITE_REF        git ref to download compose.yaml from (default main)
set -eu

REPO="tirtauntario/storlite"
DEFAULT_IMAGE="ghcr.io/${REPO}"
IMAGE="${STORLITE_IMAGE:-${DEFAULT_IMAGE}:latest}"
PORT="${STORLITE_PORT:-9000}"
REF="${STORLITE_REF:-main}"
START="${STORLITE_START:-1}"
CONTAINER_UID=65532

say() { printf 'storlite-setup: %s\n' "$*"; }
die() { say "error: $*" >&2; exit 1; }

# Resolve this script's directory before changing directories (empty when piped).
script_dir=""
case "$0" in */setup.sh) script_dir="$(cd "$(dirname "$0")" && pwd)" ;; esac

command -v docker >/dev/null 2>&1 || die "docker is required"
docker compose version >/dev/null 2>&1 || die "the docker compose plugin is required"

dir="${1:-.}"
mkdir -p "$dir"
cd "$dir"
say "setting up in $(pwd)"

# Image: pull if needed, then pin the exact version in .env.
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  say "pulling $IMAGE"
  docker pull "$IMAGE" >/dev/null || die "cannot pull $IMAGE"
fi
version="$(docker run --rm "$IMAGE" --version | awk '{print $2}')"
[ -n "$version" ] || die "cannot read the version from $IMAGE"
pinned="$IMAGE"
case "$IMAGE" in "${DEFAULT_IMAGE}:latest") pinned="${DEFAULT_IMAGE}:${version}" ;; esac

if [ ! -f compose.yaml ]; then
  if [ -n "$script_dir" ] && [ -f "$script_dir/compose.yaml" ] && [ "$script_dir" != "$(pwd)" ]; then
    cp "$script_dir/compose.yaml" compose.yaml
  else
    url="https://raw.githubusercontent.com/${REPO}/${REF}/deploy/compose.yaml"
    if command -v curl >/dev/null 2>&1; then
      curl -fsSL "$url" -o compose.yaml || die "cannot download $url"
    elif command -v wget >/dev/null 2>&1; then
      wget -qO compose.yaml "$url" || die "cannot download $url"
    else
      die "curl or wget is required"
    fi
  fi
  say "created compose.yaml"
fi

if [ ! -f .env ]; then
  printf 'STORLITE_IMAGE=%s\nSTORLITE_PORT=%s\n' "$pinned" "$PORT" > .env
  say "created .env (image $pinned, port $PORT)"
fi

if [ ! -f config.toml ]; then
  docker run --rm "$IMAGE" config template --docker > config.toml
  say "created config.toml"
fi

[ -d secrets ] || mkdir -m 0700 secrets

if [ ! -f secrets/credentials.toml ]; then
  docker run --rm --user "$(id -u):$(id -g)" -v "$(pwd)/secrets:/out" "$IMAGE" \
    credentials generate --id admin --enable --global-grant admin \
    --output /out/credentials.toml >/dev/null
  say "created secrets/credentials.toml (access key id: admin)"
fi

if [ ! -f secrets/tls.crt ] && [ ! -f secrets/tls.key ]; then
  command -v openssl >/dev/null 2>&1 ||
    die "openssl is required to create a self-signed certificate (or put tls.crt and tls.key in secrets/)"
  san="DNS:localhost,IP:127.0.0.1"
  cn="localhost"
  old_ifs="$IFS"; IFS=','
  for name in ${STORLITE_HOSTNAMES:-}; do
    if [ -z "$name" ]; then continue; fi
    case "$name" in
      *[!0-9.]* ) case "$name" in *:*) san="$san,IP:$name" ;; *) san="$san,DNS:$name"; cn="$name" ;; esac ;;
      *) san="$san,IP:$name" ;;
    esac
  done
  IFS="$old_ifs"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 825 \
    -subj "/CN=$cn" -addext "subjectAltName=$san" \
    -keyout secrets/tls.key -out secrets/tls.crt >/dev/null 2>&1 ||
    die "openssl failed to create the certificate"
  say "created a self-signed certificate for $san (replace secrets/tls.* with a real one for production)"
elif [ ! -f secrets/tls.crt ] || [ ! -f secrets/tls.key ]; then
  die "secrets/ must contain both tls.crt and tls.key"
fi

# The container runs as uid 65532 and refuses a credentials file that others
# can read. Docker Desktop maps bind-mount access itself; on Linux the files
# must be owned by the container user.
for f in secrets/credentials.toml secrets/tls.crt secrets/tls.key; do
  chmod 0400 "$f" 2>/dev/null || true   # fails harmlessly once owned by the container user
done
read_secret="cat secrets/credentials.toml"
if [ "$(uname -s)" = "Linux" ]; then
  sudo=""
  if [ "$(id -u)" -ne 0 ]; then sudo="sudo"; fi
  for f in secrets/credentials.toml secrets/tls.crt secrets/tls.key; do
    if [ "$(stat -c %u "$f")" != "$CONTAINER_UID" ]; then
      say "giving $f to the container user (uid $CONTAINER_UID)"
      $sudo chown "$CONTAINER_UID:$CONTAINER_UID" "$f"
    fi
  done
  if [ -n "$sudo" ]; then read_secret="sudo cat secrets/credentials.toml"; fi
fi

if ! out="$(docker compose run --rm storlite config check --config /etc/storlite/config.toml 2>&1)"; then
  printf '%s\n' "$out" | grep -v '^ ' >&2 || true   # drop compose progress lines
  die "config.toml is invalid"
fi

# Report the port compose will actually use.
PORT="$(sed -n 's/^STORLITE_PORT=//p' .env | tail -n1)"
PORT="${PORT:-9000}"

if [ "$START" = "0" ]; then
  say "files are ready. Start with:"
  say "  docker compose run --rm storlite init --config /etc/storlite/config.toml"
  say "  docker compose up -d"
  exit 0
fi

if out="$(docker compose run --rm storlite init --config /etc/storlite/config.toml 2>&1)"; then
  say "initialized the data volume"
else
  case "$out" in
    *"non-empty directory"*) say "data volume already initialized" ;;
    *) printf '%s\n' "$out" >&2; die "init failed" ;;
  esac
fi

docker compose up -d >/dev/null 2>&1 || die "docker compose up failed (see: docker compose logs)"
i=0
until [ "$(docker compose ps --format '{{.Health}}' storlite 2>/dev/null)" = "healthy" ]; do
  i=$((i + 1))
  [ "$i" -le 60 ] || die "storlite did not become healthy (see: docker compose logs storlite)"
  sleep 1
done

cat <<EOF

storlite $version is running at https://localhost:${PORT}

  Access key id:      admin
  Secret access key:  ${read_secret}   (the secret_access_key line)
  Region:             us-east-1, path-style addressing

  Try it:
    export AWS_ACCESS_KEY_ID=admin AWS_DEFAULT_REGION=us-east-1
    export AWS_SECRET_ACCESS_KEY="\$(${read_secret} | sed -n 's/^secret_access_key = "\\(.*\\)"/\\1/p')"
    aws --endpoint-url https://localhost:${PORT} --ca-bundle secrets/tls.crt s3 mb s3://my-bucket

  Change settings:   edit config.toml, then: docker compose restart storlite
  Change port/image: edit .env, then:        docker compose up -d
  Logs:              docker compose logs -f storlite
EOF
