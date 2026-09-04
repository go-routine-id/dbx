#!/usr/bin/env bash
# dbx installer — detects your platform, downloads the right prebuilt binary
# from the latest GitHub release, and installs it.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/go-routine-id/dbx/main/install.sh | bash
#
# Optional overrides:
#   DBX_VERSION=v0.4.3        pin a release tag instead of latest
#   DBX_INSTALL_DIR=~/bin     install somewhere else (default: /usr/local/bin)
set -euo pipefail

REPO="go-routine-id/dbx"
VERSION="${DBX_VERSION:-latest}"
INSTALL_DIR="${DBX_INSTALL_DIR:-/usr/local/bin}"

info()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
fail()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || fail "curl is required but not installed"

# --- Detect platform ---------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin)
    case "$arch" in
      arm64)          asset="dbx-macos-arm64" ;;
      x86_64)         asset="dbx-macos-x86_64" ;;
      *)              fail "unsupported macOS architecture: $arch" ;;
    esac
    ;;
  Linux)
    case "$arch" in
      x86_64|amd64)   asset="dbx-linux-x86_64" ;;
      *)              fail "no prebuilt binary for Linux/$arch yet — grab another asset from https://github.com/$REPO/releases" ;;
    esac
    ;;
  MINGW*|MSYS*|CYGWIN*)
    cat >&2 <<'EOF'
error: this script is for macOS and Linux.
On Windows, download dbx-windows-x86_64.exe from
  https://github.com/go-routine-id/dbx/releases/latest
rename it to dbx.exe and put it in a folder on your PATH.
EOF
    exit 1
    ;;
  *) fail "unsupported OS: $os" ;;
esac

info "platform: $os $arch → asset $asset"

# --- Download ----------------------------------------------------------------
if [ "$VERSION" = "latest" ]; then
  url="https://github.com/$REPO/releases/latest/download/$asset"
else
  url="https://github.com/$REPO/releases/download/$VERSION/$asset"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

info "downloading $url"
curl -fsSL -o "$tmp/dbx" "$url" || fail "download failed (asset $asset, version $VERSION)"

# --- Verify integrity ---------------------------------------------------------
# Releases ship <asset>.sha256 next to each binary. Older releases predate
# checksums, so a missing file only warns; a mismatched one fails hard.
if curl -fsSL -o "$tmp/dbx.sha256" "$url.sha256" 2>/dev/null; then
  expected="$(awk '{print $1}' "$tmp/dbx.sha256")"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/dbx" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/dbx" | awk '{print $1}')"
  else
    warn "no sha256sum/shasum available — skipping integrity check"
    actual="$expected"
  fi
  [ "$expected" = "$actual" ] || fail "checksum mismatch — the download is corrupted or tampered with, aborting"
  info "checksum verified (sha256)"
else
  warn "this release has no published checksum — skipping integrity check"
fi

chmod +x "$tmp/dbx"

# --- Install -----------------------------------------------------------------
# Use sudo only when the install dir is not writable by this user.
sudo_cmd=""
if ! { [ -d "$INSTALL_DIR" ] && [ -w "$INSTALL_DIR" ]; } && ! mkdir -p "$INSTALL_DIR" 2>/dev/null; then
  command -v sudo >/dev/null 2>&1 || fail "cannot write to $INSTALL_DIR and sudo is unavailable — set DBX_INSTALL_DIR to a writable directory"
  sudo_cmd="sudo"
fi
$sudo_cmd mkdir -p "$INSTALL_DIR"
$sudo_cmd mv "$tmp/dbx" "$INSTALL_DIR/dbx"
$sudo_cmd chmod +x "$INSTALL_DIR/dbx"

# macOS: clear the quarantine flag if present (curl usually doesn't set it,
# but Gatekeeper complains loudly when it is set).
if [ "$os" = "Darwin" ]; then
  $sudo_cmd xattr -d com.apple.quarantine "$INSTALL_DIR/dbx" 2>/dev/null || true
fi

info "installed to $INSTALL_DIR/dbx"

# --- Verify ------------------------------------------------------------------
if "$INSTALL_DIR/dbx" --version >/dev/null 2>&1; then
  info "version: $("$INSTALL_DIR/dbx" --version)"
else
  warn "installed, but '$INSTALL_DIR/dbx --version' failed — check the binary matches your platform"
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) warn "$INSTALL_DIR is not on your PATH — add it, or run dbx as $INSTALL_DIR/dbx" ;;
esac

printf '\nRun \033[1mdbx\033[0m to start. Upgrade any time with \033[1mdbx --self-update\033[0m.\n'
