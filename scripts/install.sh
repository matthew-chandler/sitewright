#!/bin/sh
# Install or upgrade Sitewright on a Linux server with systemd. Run as root:
#
#   curl -fsSL https://github.com/matthew-chandler/sitewright/releases/latest/download/install.sh | sudo sh
#
# What it does:
#   1. Downloads the release for this machine and checks its SHA-256 (and its minisign
#      signature, if `minisign` is installed).
#   2. Installs the binary to /opt/sitewright/bin, with a link in /usr/local/bin.
#   3. Creates the unprivileged `sitewright` user and /var/lib/sitewright (mode 0700).
#   4. Installs the hardened systemd unit.
# It does not start the daemon or touch DNS, firewalls or other web servers.
#
# Environment: SITEWRIGHT_VERSION=v1.2.3 installs a specific release (default: latest).
set -eu

REPO="matthew-chandler/sitewright"
PUBLIC_KEY="RWTPQ7yYcfU++y7W1z77ZcllL+32prmfxxhMwcPmGzxfGKEfl+faUAYO"
PREFIX="/opt/sitewright"
DATA="/var/lib/sitewright"

say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run this script as root (for example with sudo)"
[ "$(uname -s)" = "Linux" ] || die "Sitewright runs on Linux only"
command -v systemctl >/dev/null 2>&1 || die "systemd is required"
command -v curl >/dev/null 2>&1 || die "curl is required"

case "$(uname -m)" in
  x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
  aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

version="${SITEWRIGHT_VERSION:-latest}"
if [ "$version" = "latest" ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$version"
fi
archive="sitewright-$target.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "Downloading $archive ($version)"
curl -fsSL "$base/$archive" -o "$tmp/$archive"
curl -fsSL "$base/$archive.sha256" -o "$tmp/$archive.sha256"
curl -fsSL "$base/$archive.minisig" -o "$tmp/$archive.minisig"

say "Verifying"
expected="$(cut -d' ' -f1 "$tmp/$archive.sha256")"
actual="$(sha256sum "$tmp/$archive" | cut -d' ' -f1)"
[ "$expected" = "$actual" ] || die "checksum mismatch: the download is corrupt or has been tampered with"
if command -v minisign >/dev/null 2>&1; then
  minisign -Vm "$tmp/$archive" -x "$tmp/$archive.minisig" -P "$PUBLIC_KEY" >/dev/null \
    || die "signature verification failed: do not install this download"
  echo "    checksum and signature verified"
else
  echo "    checksum verified (install 'minisign' to verify the signature too;"
  echo "    'sitewright update' always verifies signatures itself)"
fi

tar -xzf "$tmp/$archive" -C "$tmp"
src="$tmp/sitewright-$target"
[ -x "$src/sitewright" ] || die "the archive does not contain the sitewright binary"

say "Creating the sitewright user and directories"
if ! id sitewright >/dev/null 2>&1; then
  useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin sitewright
fi
install -d -m 0755 -o sitewright -g sitewright "$PREFIX" "$PREFIX/bin"
install -d -m 0700 -o sitewright -g sitewright "$DATA"

say "Installing the binary"
# Write beside the target and rename, so a running daemon keeps its old file until restart.
install -m 0755 -o sitewright -g sitewright "$src/sitewright" "$PREFIX/bin/sitewright.new"
mv -f "$PREFIX/bin/sitewright.new" "$PREFIX/bin/sitewright"
ln -sfn "$PREFIX/bin/sitewright" /usr/local/bin/sitewright

say "Installing the systemd unit"
install -m 0644 "$src/deploy/sitewright.service" /etc/systemd/system/sitewright.service
systemctl daemon-reload

installed="$("$PREFIX/bin/sitewright" --version)"
if systemctl is-active --quiet sitewright; then
  systemctl restart sitewright
  say "Upgraded to $installed and restarted the daemon"
  exit 0
fi

cat <<NEXT

Installed $installed.

Next steps:
  1. Write the configuration (asks for your domain and mode):
       sudo -u sitewright sitewright init
  2. Point DNS at this server:   https://github.com/$REPO/blob/main/docs/dns-and-tls.md
  3. Start it:
       sudo systemctl enable --now sitewright
  4. Enroll your passkey (prints a one-time link to open on your phone):
       sudo sitewright admin invite
  5. Check the installation:
       sudo sitewright doctor
NEXT
