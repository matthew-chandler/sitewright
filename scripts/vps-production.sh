#!/usr/bin/env bash
# Put Sitewright on the main zone of a server where nginx already hosts other sites.
# Run once, as root:
#
#   sudo ./vps-production.sh --like /etc/nginx/sites-enabled/<existing site> --dry-run
#   sudo ./vps-production.sh --like /etc/nginx/sites-enabled/<existing site>
#
# What it does (and nothing else):
#   Adds ONE new nginx file, `sitewright`, with one server block for `*.<zone>` that
#   proxies to the Sitewright daemon. It uses the same listen and TLS lines as the --like
#   site. If that site's certificate is a Let's Encrypt one, certbot is asked for a
#   certificate for the admin hostname, which the block then uses.
#
# Why this is safe for the sites already there:
#   nginx prefers an exact server_name over a wildcard, so every existing
#   `name.<zone>` block keeps answering for its name. Only hostnames that no block
#   names today (which currently fall through to nginx's default site) go to Sitewright.
#   No existing file is modified, `nginx -t` runs before the reload, and the new file is
#   removed again if the test fails.
#
# To undo: remove the file it names in its summary, then `nginx -t && systemctl reload nginx`.
set -euo pipefail

ZONE="machandler.com"
ADMIN_HOST=""              # default: sitewright.$ZONE
UPSTREAM="127.0.0.1:18080"
NAME="sitewright"
LIKE=""
DRY_RUN=0

usage() { sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --like) LIKE="${2:?--like needs a file}"; shift 2 ;;
    --zone) ZONE="${2:?}"; shift 2 ;;
    --admin-host) ADMIN_HOST="${2:?}"; shift 2 ;;
    --upstream) UPSTREAM="${2:?}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage 1 ;;
  esac
done
ADMIN_HOST="${ADMIN_HOST:-sitewright.$ZONE}"

[ -n "$LIKE" ] || { echo "error: --like <existing nginx site file> is required" >&2; exit 1; }
[ -r "$LIKE" ] || { echo "error: cannot read $LIKE" >&2; exit 1; }
case "$ADMIN_HOST" in
  *."$ZONE") ;;
  *) echo "error: the admin host $ADMIN_HOST is not inside $ZONE" >&2; exit 1 ;;
esac
if [ "$DRY_RUN" -eq 0 ] && [ "$(id -u)" -ne 0 ]; then
  echo "error: run with sudo (or use --dry-run)" >&2; exit 1
fi

say()  { printf '\n==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }

if ! command -v nginx >/dev/null && [ ! -x /usr/sbin/nginx ]; then
  if [ "$DRY_RUN" -eq 1 ]; then note "warning: nginx not found on this machine"
  else echo "error: nginx not found" >&2; exit 1; fi
fi

if [ -d /etc/nginx/sites-available ] && [ -d /etc/nginx/sites-enabled ]; then
  CONF="/etc/nginx/sites-available/$NAME"
  LINK="/etc/nginx/sites-enabled/$NAME"
else
  CONF="/etc/nginx/conf.d/$NAME.conf"
  LINK=""
fi

say "Existing sites that keep their own server block"
existing="$(grep -rhoE "server_name[^;]*;" /etc/nginx/sites-enabled /etc/nginx/conf.d 2>/dev/null \
  | tr -s ' \t' '\n' | tr -d ';' | grep -E "(^|\.)$(printf '%s' "$ZONE" | sed 's/\./\\./g')\$" | grep -v '^\*' | sort -u || true)"
if [ -n "$existing" ]; then
  printf '%s\n' "$existing" | sed 's/^/    /'
  note "These exact names win over the wildcard and are not affected."
else
  note "(none found)"
fi

# The listen and TLS directives of the reference site, without per-socket options.
like_lines="$(grep -E '^[[:space:]]*(listen|ssl_[a-z_]+|include[[:space:]]+/etc/letsencrypt/)[^;]*;' "$LIKE" \
  | grep -vE '^[[:space:]]*#' \
  | sed -E 's/^[[:space:]]+//; s/[[:space:]]+#.*$//' \
  | sed -E '/^listen/ s/[[:space:]]+(default_server|ipv6only=[a-z]+|reuseport|deferred|backlog=[0-9]+)//g' \
  | awk '!seen[$0]++')"
if ! printf '%s\n' "$like_lines" | grep -q '^listen'; then
  note "no listen lines found in $LIKE; listening on port 80 only"
  like_lines="listen 80;"
fi
http_lines="$(printf '%s\n' "$like_lines" | grep -E '^listen' | grep -vE '(ssl|443)' || true)"
http_lines="${http_lines:-listen 80;}"

# A Let's Encrypt certificate names specific hosts. Get one for the admin hostname and
# use it for this block, rather than borrowing another site's.
le_ref="$(printf '%s\n' "$like_lines" | sed -nE 's#^ssl_certificate[[:space:]]+/etc/letsencrypt/live/([^/]+)/.*#\1#p' | head -1)"
need_cert=0
if [ -n "$le_ref" ] && [ "$le_ref" != "$ADMIN_HOST" ]; then
  if [ -d "/etc/letsencrypt/live/$ADMIN_HOST" ]; then
    note "using the existing Let's Encrypt certificate for $ADMIN_HOST"
  elif command -v certbot >/dev/null; then
    need_cert=1
  else
    note "WARNING: certbot is not installed; keeping the reference site's certificate ($le_ref)."
    ADMIN_CERT_FALLBACK=1
  fi
  if [ "${ADMIN_CERT_FALLBACK:-0}" -eq 0 ]; then
    like_lines="$(printf '%s\n' "$like_lines" | sed "s#/etc/letsencrypt/live/$le_ref/#/etc/letsencrypt/live/$ADMIN_HOST/#g")"
  fi
fi

# render_conf LISTEN_AND_TLS_LINES
render_conf() {
  cat <<EOF
# Managed by sitewright scripts/vps-production.sh. Safe to delete to undo.
# Every hostname under $ZONE that has no server block of its own goes to the Sitewright
# daemon on $UPSTREAM, which serves the admin UI ($ADMIN_HOST) and the sites.

# Keep the scheme the visitor used when a CDN in front (Cloudflare) already set it.
map \$http_x_forwarded_proto \$${NAME}_proto {
    default \$http_x_forwarded_proto;
    ""      \$scheme;
}

server {
$(printf '%s\n' "$1" | sed 's/^/    /')
    server_name *.$ZONE;

    location / {
        proxy_pass http://$UPSTREAM;
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$${NAME}_proto;
        proxy_set_header Connection "";
        # Server-sent events (live agent progress) need unbuffered, long-lived responses.
        proxy_buffering off;
        proxy_cache off;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
        client_max_body_size 25m;
    }
}
EOF
}

install_conf() {
  local backup=""
  if [ -f "$CONF" ]; then backup="$(mktemp)"; cp -p "$CONF" "$backup"; fi
  printf '%s\n' "$1" > "$CONF"
  chmod 0644 "$CONF"
  [ -n "$LINK" ] && ln -sfn "$CONF" "$LINK"
  if nginx -t; then
    systemctl reload nginx
    note "nginx reloaded"
    [ -n "$backup" ] && rm -f "$backup"
    return 0
  fi
  echo "error: nginx -t failed; reverting our file. nginx was NOT reloaded." >&2
  if [ -n "$backup" ]; then
    cp -p "$backup" "$CONF"; rm -f "$backup"
  else
    rm -f "$CONF"; [ -n "$LINK" ] && rm -f "$LINK"
  fi
  return 1
}

say "nginx"
note "reference site: $LIKE"
note "target file:    $CONF"
cert_failed=0
if [ "$DRY_RUN" -eq 1 ]; then
  if [ "$need_cert" -eq 1 ]; then
    note "[dry-run] first install the file on plain HTTP only, then:"
    note "[dry-run] certbot certonly --nginx -d $ADMIN_HOST --non-interactive --keep-until-expiring"
  fi
  note "[dry-run] would write this file:"
  render_conf "$like_lines" | sed 's/^/      | /'
  [ -n "$LINK" ] && note "[dry-run] ln -sfn $CONF $LINK"
  note "[dry-run] nginx -t && systemctl reload nginx"
else
  if [ "$need_cert" -eq 1 ]; then
    install_conf "$(render_conf "$http_lines")" || exit 1
    if certbot certonly --nginx -d "$ADMIN_HOST" --non-interactive --keep-until-expiring; then
      note "certificate issued for $ADMIN_HOST (renewed by certbot's existing timer)"
    else
      cert_failed=1
      echo "warning: certbot could not get a certificate for $ADMIN_HOST." >&2
      echo "         The block stays on plain HTTP; fix the cause and re-run this script." >&2
    fi
  fi
  if [ "$cert_failed" -eq 0 ]; then
    install_conf "$(render_conf "$like_lines")" || exit 1
  fi
fi

say "Summary"
cat <<EOF
    nginx file:  $CONF
    serves:      *.$ZONE  ->  http://$UPSTREAM  (admin UI: $ADMIN_HOST)
    untouched:   every existing server block, including the earlier sitewright-dev file

    DNS: a wildcard record for *.$ZONE must point at this server (it may be proxied by
    Cloudflare). Nothing else is needed per site.

    To undo: rm -f $CONF ${LINK:-} && nginx -t && systemctl reload nginx
EOF
[ "$DRY_RUN" -eq 1 ] && printf '\n    Dry run: nothing was changed.\n'
if [ "$cert_failed" -eq 1 ]; then
  printf '\n    WARNING: no certificate for %s yet; HTTPS will fail until this is re-run.\n' "$ADMIN_HOST"
  exit 2
fi
exit 0
