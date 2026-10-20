#!/usr/bin/env bash
# One-line installer for bella-presenter on macOS and Linux, from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/tschinz/bella-presenter/main/install.sh | bash
#
# macOS: installs bella-presenter.app into /Applications (Apple Silicon only).
# Linux: installs the .deb via dpkg (Debian/Ubuntu). On other distros, build from source.
set -euo pipefail

REPO="tschinz/bella-presenter"
LATEST_DL="https://github.com/$REPO/releases/latest/download"
API="https://api.github.com/repos/$REPO/releases/latest"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin)
    if [ "$arch" != "arm64" ]; then
      echo "Only Apple Silicon (arm64) is published; detected '$arch'. Build from source: https://github.com/$REPO#build-from-source" >&2
      exit 1
    fi
    echo "Downloading bella-presenter for macOS (arm64)…"
    curl -fL "$LATEST_DL/bella-presenter-macos-arm64.zip" -o "$tmp/p.zip"
    echo "Installing to /Applications/bella-presenter.app…"
    rm -rf "/Applications/bella-presenter.app"
    unzip -q "$tmp/p.zip" -d /Applications
    xattr -dr com.apple.quarantine "/Applications/bella-presenter.app" 2>/dev/null || true
    echo "✓ Installed. Launch 'bella-presenter' from /Applications or Spotlight."
    ;;
  Linux)
    if ! command -v dpkg >/dev/null 2>&1; then
      echo "The Linux release is a .deb (Debian/Ubuntu); 'dpkg' was not found. Build from source: https://github.com/$REPO#build-from-source" >&2
      exit 1
    fi
    echo "Finding the latest .deb…"
    url="$(curl -fsSL "$API" | grep -o '"browser_download_url": *"[^"]*\.deb"' | head -1 | sed 's/.*"browser_download_url": *"\([^"]*\)".*/\1/')"
    [ -n "$url" ] || { echo "No .deb asset in the latest release." >&2; exit 1; }
    echo "Downloading $url"
    curl -fL "$url" -o "$tmp/bella-presenter.deb"
    echo "Installing (needs sudo)…"
    sudo dpkg -i "$tmp/bella-presenter.deb" || sudo apt-get -f install -y
    echo "✓ Installed. Launch 'bella-presenter' from your applications menu."
    ;;
  *)
    echo "Unsupported OS '$os'. Build from source: https://github.com/$REPO#build-from-source" >&2
    exit 1
    ;;
esac
