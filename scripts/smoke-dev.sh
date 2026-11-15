#!/usr/bin/env bash
# Smoke-test the dev deployment. Checks run on the VPS against the daemon directly
# (always), and through the public hostnames (once nginx is bootstrapped).
set -uo pipefail
HOST="${SW_DEV_HOST:-racknerd-dev}"
ZONE="${SW_DEV_ZONE:-machandler.com}"
SSH="ssh -o BatchMode=yes -o ConnectTimeout=20 $HOST"
fail=0

check() { # check NAME EXPECTED ACTUAL
  if [ "$2" = "$3" ]; then echo "ok    $1"; else echo "FAIL  $1 (expected '$2', got '$3')"; fail=1; fi
}

body="$($SSH "curl -fsS -m 10 -H 'Host: anything.dev.$ZONE' http://127.0.0.1:18080/" 2>&1)"
check "daemon answers on 127.0.0.1:18080" "hello" "$body"

# Compare bodies, not status codes: before the bootstrap, nginx's default site answers 200.
body="$(curl -s -m 10 "http://anything.dev.$ZONE/" | head -c 200)"
check "public site wildcard (needs nginx bootstrap)" "hello" "$body"

body="$(curl -s -m 10 "https://sitewright-dev.$ZONE/" | head -c 200)"
check "public admin host (needs nginx bootstrap)" "hello" "$body"

# The sites wildcard must be DNS-only for standalone-mode TLS tests (M6).
if getent ahostsv4 "anything.dev.$ZONE" | awk '{print $1}' | grep -qE '^(104\.|172\.6[4-9]\.|172\.7[01]\.)'; then
  echo "note  *.dev.$ZONE resolves to Cloudflare (proxied); M6 needs it DNS-only"
fi

exit $fail
