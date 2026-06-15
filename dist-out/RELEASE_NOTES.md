Prebuilt **veles** binaries for the `feat/mcp-watch-dashboard` fork build — adds
`serve-mcp --watch` (live incremental index) and `--dashboard` (per-repo web UI).
Built with `--features dashboard` from commit on branch `feat/mcp-watch-dashboard`.

This is a **rolling tag**: assets are replaced in place when a new fork build is cut,
so the download URLs below stay stable.

## Install

**One-liner (auto-detects macOS arm64 / Linux x86_64):**
```bash
curl -fsSL https://github.com/kundeng/Veles/releases/download/dashboard-latest/install.sh | bash
```

**Manual:**
```bash
# macOS (Apple Silicon)
curl -fsSL https://github.com/kundeng/Veles/releases/download/dashboard-latest/veles-0.6.0-dashboard-aarch64-apple-darwin.tar.gz | tar -xz
xattr -d com.apple.quarantine veles 2>/dev/null; codesign --force --sign - veles
install -m 0755 veles ~/.cargo/bin/veles

# Linux (x86_64)
curl -fsSL https://github.com/kundeng/Veles/releases/download/dashboard-latest/veles-0.6.0-dashboard-x86_64-unknown-linux-gnu.tar.gz | tar -xz
install -m 0755 veles ~/.cargo/bin/veles
```

Verify: `veles --version` → `veles 0.6.0`, and `veles serve-mcp --help | grep dashboard`.

Checksums in `SHA256SUMS.txt`.
