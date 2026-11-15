#!/usr/bin/env bash
# Build a static musl binary and deploy it to the dev VPS as the unprivileged devtest user.
# Usage: scripts/deploy-dev.sh [--skip-build]
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

HOST="${SW_DEV_HOST:-racknerd-dev}"
SSH="ssh -o BatchMode=yes -o ConnectTimeout=20 $HOST"

case "$($SSH uname -m)" in
  x86_64) target=x86_64-unknown-linux-musl ;;
  aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
  *) echo "unsupported remote architecture" >&2; exit 1 ;;
esac

if [ "${1:-}" != "--skip-build" ]; then
  echo "==> building for $target"
  SQLX_OFFLINE=true cargo zigbuild --release --target "$target" -p sitewright
fi
bin="target/$target/release/sitewright"

echo "==> uploading"
$SSH 'mkdir -p ~/bin ~/sitewright-data ~/.config/systemd/user'
scp -q -o BatchMode=yes "$bin" "$HOST:bin/sitewright.new"
scp -q -o BatchMode=yes deploy/sitewright-dev.service "$HOST:.config/systemd/user/sitewright.service"
if [ -f deploy/dev-vps.toml ]; then
  # Never overwrite a config that was edited on the server.
  $SSH 'test -f ~/sitewright-data/config.toml' || scp -q -o BatchMode=yes deploy/dev-vps.toml "$HOST:sitewright-data/config.toml"
fi

echo "==> swapping binary and restarting"
$SSH 'set -e
  chmod 0755 ~/bin/sitewright.new
  ~/bin/sitewright.new --version
  [ -f ~/bin/sitewright ] && cp -p ~/bin/sitewright ~/bin/sitewright.prev
  mv -f ~/bin/sitewright.new ~/bin/sitewright
  export XDG_RUNTIME_DIR=/run/user/$(id -u)
  systemctl --user daemon-reload
  systemctl --user enable sitewright >/dev/null 2>&1 || true
  systemctl --user restart sitewright
  sleep 1
  systemctl --user is-active sitewright'
echo "==> deployed"
