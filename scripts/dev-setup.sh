#!/usr/bin/env bash
# Set up a WSL/Ubuntu development environment for Sitewright.
# Idempotent: safe to re-run. Installs everything into the user's home; no sudo.
set -euo pipefail

BIN="$HOME/.local/bin"
mkdir -p "$BIN"
export PATH="$HOME/.cargo/bin:$BIN:$PATH"

ZIG_VERSION="${ZIG_VERSION:-0.13.0}"
PEBBLE_VERSION="${PEBBLE_VERSION:-v2.8.0}"
MINISIGN_VERSION="${MINISIGN_VERSION:-0.12}"

say() { printf '\n==> %s\n' "$*"; }

say "Rust toolchain"
if ! command -v rustup >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
fi
rustup toolchain install stable --profile minimal --component rustfmt --component clippy
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

say "cargo-binstall (prebuilt cargo tools)"
if ! command -v cargo-binstall >/dev/null; then
  curl -L --proto '=https' --tlsv1.2 -sSf \
    https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash
fi

say "Cargo tools"
cargo binstall --no-confirm --locked cargo-zigbuild cargo-nextest cargo-deny cargo-dist sqlx-cli cargo-insta

say "zig $ZIG_VERSION (linker for cargo-zigbuild)"
if ! command -v zig >/dev/null; then
  arch="$(uname -m)"
  tmp="$(mktemp -d)"
  curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${arch}-${ZIG_VERSION}.tar.xz" | tar -xJ -C "$tmp"
  rm -rf "$HOME/.local/zig"
  mv "$tmp"/zig-linux-* "$HOME/.local/zig"
  ln -sf "$HOME/.local/zig/zig" "$BIN/zig"
fi

say "minisign $MINISIGN_VERSION"
if ! command -v minisign >/dev/null; then
  tmp="$(mktemp -d)"
  curl -fsSL "https://github.com/jedisct1/minisign/releases/download/${MINISIGN_VERSION}/minisign-${MINISIGN_VERSION}-linux.tar.gz" | tar -xz -C "$tmp"
  install -m 0755 "$tmp/minisign-linux/$(uname -m)/minisign" "$BIN/minisign"
fi

say "Pebble $PEBBLE_VERSION (ACME test server)"
if ! command -v pebble >/dev/null; then
  case "$(uname -m)" in x86_64) parch=amd64 ;; aarch64) parch=arm64 ;; esac
  tmp="$(mktemp -d)"
  for tool in pebble pebble-challtestsrv; do
    curl -fsSL "https://github.com/letsencrypt/pebble/releases/download/${PEBBLE_VERSION}/${tool}-linux-${parch}.tar.gz" | tar -xz -C "$tmp"
    install -m 0755 "$(find "$tmp" -type f -name "$tool" | head -1)" "$BIN/$tool"
  done
fi

say "Node (for skills-ref validate)"
command -v node >/dev/null && node --version || echo "Node LTS not found: install it (e.g. via nvm) to run 'npx skills-ref validate'."

say "Done. Make sure \$HOME/.cargo/bin and $BIN are on your PATH."
