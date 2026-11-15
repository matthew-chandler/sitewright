#!/usr/bin/env bash
# One-time VPS bootstrap for the Sitewright dev environment. Run once, as root:
#
#   sudo ./vps-bootstrap.sh --like /etc/nginx/sites-enabled/<existing site> [--pubkey "<ssh public key>"]
#   sudo ./vps-bootstrap.sh --like ... --dry-run     # print every change, make none
#
# What it does (and nothing else):
#   1. Ensures user `devtest` exists (no sudo), has the SSH key, and has systemd lingering.
#   2. Adds ONE new nginx file with two server blocks that proxy to 127.0.0.1:18080:
#        *.dev.<zone>            port 80, plain HTTP (sites; DNS-only record)
#        sitewright-dev.<zone>   same listen/ssl_* lines as the --like file (admin UI)
#      It runs `nginx -t` before reloading and removes its file again if the test fails.
#      No existing nginx file is modified.
#   3. Opens TCP 18443 in ufw, only if ufw is active.
#   4. Prints a summary, including the DNS records that must exist.
#
# Idempotent: re-running it rewrites the same nginx file and skips steps already done.
set -euo pipefail

DEV_USER="devtest"
ZONE="machandler.com"
SITES_WILDCARD=""          # default: *.dev.$ZONE
ADMIN_HOST=""              # default: sitewright-dev.$ZONE
UPSTREAM="127.0.0.1:18080"
TLS_TEST_PORT="18443"
LIKE=""
PUBKEY=""
DRY_RUN=0

usage() { sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --like) LIKE="${2:?--like needs a file}"; shift 2 ;;
    --pubkey) PUBKEY="${2:?--pubkey needs a key}"; shift 2 ;;
    --user) DEV_USER="${2:?}"; shift 2 ;;
    --zone) ZONE="${2:?}"; shift 2 ;;
    --admin-host) ADMIN_HOST="${2:?}"; shift 2 ;;
    --upstream) UPSTREAM="${2:?}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage 1 ;;
  esac
done

SITES_WILDCARD="${SITES_WILDCARD:-*.dev.$ZONE}"
ADMIN_HOST="${ADMIN_HOST:-sitewright-dev.$ZONE}"

[ -n "$LIKE" ] || { echo "error: --like <existing nginx site file> is required" >&2; exit 1; }
[ -r "$LIKE" ] || { echo "error: cannot read $LIKE" >&2; exit 1; }
if [ "$DRY_RUN" -eq 0 ] && [ "$(id -u)" -ne 0 ]; then
  echo "error: run with sudo (or use --dry-run)" >&2; exit 1
fi

say()  { printf '\n==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
# run CMD...: execute, or only print under --dry-run.
run() {
  if [ "$DRY_RUN" -eq 1 ]; then printf '    [dry-run] %s\n' "$*"; else "$@"; fi
}

# ---------------------------------------------------------------- 1. user
say "User $DEV_USER"
if id "$DEV_USER" >/dev/null 2>&1; then
  note "exists (uid $(id -u "$DEV_USER")); not modified"
  if id -nG "$DEV_USER" | tr ' ' '\n' | grep -qxE 'sudo|wheel|admin'; then
    echo "error: $DEV_USER is in a sudo group; refusing to continue" >&2; exit 1
  fi
else
  run useradd --create-home --shell /bin/bash "$DEV_USER"
fi

if [ -n "$PUBKEY" ]; then
  home="$(getent passwd "$DEV_USER" | cut -d: -f6 || true)"; home="${home:-/home/$DEV_USER}"
  auth="$home/.ssh/authorized_keys"
  if [ -f "$auth" ] && grep -qxF "$PUBKEY" "$auth"; then
    note "SSH key already installed"
  else
    run install -d -m 0700 -o "$DEV_USER" -g "$DEV_USER" "$home/.ssh"
    if [ "$DRY_RUN" -eq 1 ]; then
      note "[dry-run] append public key to $auth (mode 0600)"
    else
      printf '%s\n' "$PUBKEY" >> "$auth"
      chown "$DEV_USER:$DEV_USER" "$auth"; chmod 0600 "$auth"
    fi
  fi
else
  note "no --pubkey given; SSH keys left as they are"
fi

if [ "$(loginctl show-user "$DEV_USER" --property=Linger --value 2>/dev/null || true)" = "yes" ]; then
  note "lingering already enabled"
else
  run loginctl enable-linger "$DEV_USER"
fi

# ---------------------------------------------------------------- 2. nginx
say "nginx"
if ! command -v nginx >/dev/null && [ ! -x /usr/sbin/nginx ]; then
  if [ "$DRY_RUN" -eq 1 ]; then note "warning: nginx not found on this machine"
  else echo "error: nginx not found" >&2; exit 1; fi
fi

if [ -d /etc/nginx/sites-available ] && [ -d /etc/nginx/sites-enabled ]; then
  CONF="/etc/nginx/sites-available/sitewright-dev"
  LINK="/etc/nginx/sites-enabled/sitewright-dev"
else
  CONF="/etc/nginx/conf.d/sitewright-dev.conf"
  LINK=""
fi

# Copy the listen and ssl_* directives from the reference site, so the admin host
# terminates TLS exactly like the existing sites. `default_server` and socket options
# are stripped because only one server block per socket may carry them.
like_lines="$(grep -E '^[[:space:]]*(listen|ssl_[a-z_]+)[[:space:]]' "$LIKE" \
  | grep -vE '^[[:space:]]*#' \
  | sed -E 's/^[[:space:]]+//; s/[[:space:]]+#.*$//' \
  | sed -E '/^listen/ s/[[:space:]]+(default_server|ipv6only=[a-z]+|reuseport|deferred|backlog=[0-9]+)//g' \
  | awk '!seen[$0]++')"
if ! printf '%s\n' "$like_lines" | grep -q '^listen'; then
  note "no listen lines found in $LIKE; the admin host will listen on port 80 only"
  like_lines="listen 80;"
fi
# Mirror IPv6 for the wildcard block only if the reference site uses it.
wild_listen="listen 80;"
if printf '%s\n' "$like_lines" | grep -qE '^listen \[::\]'; then
  wild_listen="$wild_listen"$'\n'"    listen [::]:80;"
fi

proxy_block="$(cat <<EOF
    location / {
        proxy_pass http://$UPSTREAM;
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$sitewright_dev_proto;
        proxy_set_header Connection "";
        # Server-sent events (live agent progress) need unbuffered, long-lived responses.
        proxy_buffering off;
        proxy_cache off;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
        client_max_body_size 25m;
    }
EOF
)"

new_conf="$(cat <<EOF
# Managed by sitewright scripts/vps-bootstrap.sh. Safe to delete to undo.
# Dev environment only: proxies to the devtest user's daemon on $UPSTREAM.

# Keep the scheme the visitor used when a CDN in front (Cloudflare) already set it.
map \$http_x_forwarded_proto \$sitewright_dev_proto {
    default \$http_x_forwarded_proto;
    ""      \$scheme;
}

# Sites: $SITES_WILDCARD (DNS-only record, plain HTTP; also carries ACME HTTP-01 challenges)
# Visitors reach this block directly, so their X-Forwarded-Proto is not trusted here.
server {
    $wild_listen
    server_name $SITES_WILDCARD;

${proxy_block//\$sitewright_dev_proto/\$scheme}
}

# Admin UI: $ADMIN_HOST (proxied through Cloudflare, TLS as on the existing sites)
server {
$(printf '%s\n' "$like_lines" | sed 's/^/    /')
    server_name $ADMIN_HOST;

$proxy_block
}
EOF
)"

note "reference site: $LIKE"
note "target file:    $CONF"
if [ "$DRY_RUN" -eq 1 ]; then
  note "[dry-run] would write this file:"
  printf '%s\n' "$new_conf" | sed 's/^/      | /'
  [ -n "$LINK" ] && note "[dry-run] ln -sfn $CONF $LINK"
  note "[dry-run] nginx -t && systemctl reload nginx"
else
  backup=""
  if [ -f "$CONF" ]; then backup="$(mktemp)"; cp -p "$CONF" "$backup"; fi
  printf '%s\n' "$new_conf" > "$CONF"
  chmod 0644 "$CONF"
  [ -n "$LINK" ] && ln -sfn "$CONF" "$LINK"
  if nginx -t; then
    systemctl reload nginx
    note "nginx reloaded"
    [ -n "$backup" ] && rm -f "$backup"
  else
    echo "error: nginx -t failed; reverting our file. nginx was NOT reloaded." >&2
    if [ -n "$backup" ]; then
      cp -p "$backup" "$CONF"; rm -f "$backup"
    else
      rm -f "$CONF"; [ -n "$LINK" ] && rm -f "$LINK"
    fi
    exit 1
  fi
fi

# ---------------------------------------------------------------- 3. firewall
say "Firewall"
if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q '^Status: active'; then
  if ufw status | grep -qE "^$TLS_TEST_PORT/tcp[[:space:]]+ALLOW"; then
    note "ufw already allows $TLS_TEST_PORT/tcp"
  else
    run ufw allow "$TLS_TEST_PORT/tcp" comment "sitewright dev standalone TLS"
  fi
else
  note "ufw not active; nothing to do (check any provider-level firewall for TCP $TLS_TEST_PORT)"
fi

# ---------------------------------------------------------------- 4. summary
ip="$(curl -fsS4 --max-time 5 https://ifconfig.me 2>/dev/null || hostname -I | awk '{print $1}')"
say "Summary"
cat <<EOF
    user:        $DEV_USER (no sudo, lingering on)
    nginx file:  $CONF
    upstream:    http://$UPSTREAM  (the devtest daemon; 502 until it is running)
    firewall:    TCP $TLS_TEST_PORT for standalone-mode TLS tests

    DNS records that must exist (Cloudflare):
      $SITES_WILDCARD   A   $ip   DNS only (grey cloud)
      $ADMIN_HOST   A   $ip   Proxied (orange cloud)

    To undo: rm -f $CONF ${LINK:-} && nginx -t && systemctl reload nginx
EOF
[ "$DRY_RUN" -eq 1 ] && printf '\n    Dry run: nothing was changed.\n'
exit 0
