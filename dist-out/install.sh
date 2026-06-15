#!/usr/bin/env bash
# Veles fork (watch + dashboard) installer.
# Downloads the prebuilt `veles` binary for this machine from the
# kundeng/Veles GitHub release and installs it to ~/.cargo/bin.
#
#   curl -fsSL https://github.com/kundeng/Veles/releases/download/dashboard-latest/install.sh | bash
#
set -euo pipefail

REPO="kundeng/Veles"
TAG="dashboard-latest"
DEST="${VELES_INSTALL_DIR:-$HOME/.cargo/bin}"

os="$(uname -s)"; arch="$(uname -m)"
case "$os-$arch" in
  Darwin-arm64)        asset="veles-0.6.0-dashboard-aarch64-apple-darwin.tar.gz" ;;
  Linux-x86_64)        asset="veles-0.6.0-dashboard-x86_64-unknown-linux-gnu.tar.gz" ;;
  *) echo "No prebuilt veles for $os-$arch. Build from source: cargo build --release -p veles-cli --features dashboard" >&2; exit 1 ;;
esac

url="https://github.com/$REPO/releases/download/$TAG/$asset"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

echo "→ downloading $asset"
curl -fsSL "$url" -o "$tmp/v.tar.gz"
tar -xzf "$tmp/v.tar.gz" -C "$tmp"
mkdir -p "$DEST"
install -m 0755 "$tmp/veles" "$DEST/veles"

# macOS: strip quarantine and ad-hoc sign so Gatekeeper allows the copied binary.
if [ "$os" = "Darwin" ]; then
  xattr -d com.apple.quarantine "$DEST/veles" 2>/dev/null || true
  codesign --force --sign - "$DEST/veles" 2>/dev/null || true
fi

echo "✓ installed $("$DEST/veles" --version) → $DEST/veles"
echo "  (ensure $DEST is on your PATH)"
