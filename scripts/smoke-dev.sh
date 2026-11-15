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

code="$(curl -s -o /dev/null -m 10 -w '%{http_code}' "http://anything.dev.$ZONE/")"
check "public site wildcard (needs nginx bootstrap)" "200" "$code"

code="$(curl -s -o /dev/null -m 10 -w '%{http_code}' "https://sitewright-dev.$ZONE/")"
check "public admin host (needs nginx bootstrap)" "200" "$code"

exit $fail
