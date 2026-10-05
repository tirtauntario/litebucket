#!/bin/sh
# litebucket installer: downloads a release binary from GitHub, verifies its
# SHA-256 checksum, and installs it.
#
#   curl -fsSL https://raw.githubusercontent.com/tirtauntario/litebucket/main/install.sh | sh
#
# Environment:
#   LITEBUCKET_VERSION      release tag to install (default: latest, e.g. v0.1.0)
#   LITEBUCKET_INSTALL_DIR  destination directory (default: /usr/local/bin)
set -eu

REPO="tirtauntario/litebucket"
VERSION="${LITEBUCKET_VERSION:-latest}"
INSTALL_DIR="${LITEBUCKET_INSTALL_DIR:-/usr/local/bin}"

say() { printf 'litebucket-install: %s\n' "$*"; }
die() { say "error: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required"; }

need uname
need tar
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  die "curl or wget is required"
fi

case "$(uname -s)" in
  Linux) os="unknown-linux-musl" ;;
  Darwin) os="apple-darwin" ;;
  *) die "unsupported OS $(uname -s); build from source instead" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) die "unsupported architecture $(uname -m); build from source instead" ;;
esac
target="${arch}-${os}"

if [ "$VERSION" = "latest" ]; then
  tmp_json="$(mktemp)"
  fetch "https://api.github.com/repos/${REPO}/releases/latest" "$tmp_json" ||
    die "cannot query the latest release (set LITEBUCKET_VERSION=vX.Y.Z)"
  VERSION="$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$tmp_json" | head -n1)"
  rm -f "$tmp_json"
  [ -n "$VERSION" ] || die "cannot determine the latest release"
fi

name="litebucket-${VERSION}-${target}"
base="https://github.com/${REPO}/releases/download/${VERSION}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

say "downloading ${name}.tar.gz"
fetch "${base}/${name}.tar.gz" "${work}/${name}.tar.gz" || die "download failed"
fetch "${base}/${name}.tar.gz.sha256" "${work}/${name}.tar.gz.sha256" || die "checksum download failed"

expected="$(cut -d' ' -f1 "${work}/${name}.tar.gz.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${work}/${name}.tar.gz" | cut -d' ' -f1)"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "${work}/${name}.tar.gz" | cut -d' ' -f1)"
else
  die "sha256sum or shasum is required to verify the download"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for ${name}.tar.gz"
say "checksum OK"

tar -xzf "${work}/${name}.tar.gz" -C "$work"

sudo=""
mkdir -p "$INSTALL_DIR" 2>/dev/null || true
if [ ! -d "$INSTALL_DIR" ] || [ ! -w "$INSTALL_DIR" ]; then
  if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
    sudo="sudo"
    say "installing to ${INSTALL_DIR} (needs sudo)"
  fi
  $sudo mkdir -p "$INSTALL_DIR"
fi
$sudo install -m 0755 "${work}/${name}/litebucket" "${INSTALL_DIR}/litebucket"

say "installed $("${INSTALL_DIR}/litebucket" --version) to ${INSTALL_DIR}/litebucket"
say "example config and systemd unit: https://github.com/${REPO}/tree/${VERSION}"
