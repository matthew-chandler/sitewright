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

on_vps() { $SSH "curl -s -m 10 -o /dev/null -w '%{http_code}' -H 'Host: $1' http://127.0.0.1:18080$2" 2>/dev/null; }

# --- on the VPS, straight to the daemon (works before the nginx bootstrap) ---
$SSH "~/bin/sitewright --data-dir ~/sitewright-data site list" >/dev/null 2>&1
check "CLI reaches the daemon over the admin socket" "0" "$?"
check "known site answers 200 (smoke.$ZONE)" "200" "$(on_vps "smoke.$ZONE" /)"
check "unknown host answers 404" "404" "$(on_vps "no-such-site-zz.$ZONE" /)"
check "admin sign-in page answers 200" "200" "$(on_vps "sitewright.$ZONE" /login)"
check "admin home redirects to sign-in" "303" "$(on_vps "sitewright.$ZONE" /)"

# --- through the public hostnames (need the nginx bootstrap) ---
# Compare bodies, not just status codes: before the bootstrap nginx's default site answers.
body="$(curl -s -m 10 "https://smoke.$ZONE/" | grep -c "<h1>Hello from Sitewright</h1>")"
check "public: known site (needs nginx bootstrap)" "1" "$body"
check "public: unknown host is 404 (needs nginx bootstrap)" "404" "$(curl -s -m 10 -o /dev/null -w '%{http_code}' "https://no-such-site-zz.$ZONE/")"
body="$(curl -s -m 10 "https://sitewright.$ZONE/login" | grep -c '<h1>Sitewright</h1>')"
check "public: admin sign-in page over HTTPS (needs a certificate for the admin host)" "1" "$body"

exit $fail
