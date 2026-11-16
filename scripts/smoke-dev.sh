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
check "known site answers 200 (smoke.dev.$ZONE)" "200" "$(on_vps "smoke.dev.$ZONE" /)"
check "unknown host answers 404" "404" "$(on_vps "no-such-site.dev.$ZONE" /)"
check "admin sign-in page answers 200" "200" "$(on_vps "sitewright-dev.$ZONE" /login)"
check "admin home redirects to sign-in" "303" "$(on_vps "sitewright-dev.$ZONE" /)"

# --- through the public hostnames (need the nginx bootstrap) ---
# Compare bodies, not just status codes: before the bootstrap nginx's default site answers.
body="$(curl -s -m 10 "http://smoke.dev.$ZONE/" | grep -c "<h1>Hello from Sitewright</h1>")"
check "public: known site (needs nginx bootstrap)" "1" "$body"
check "public: unknown host is 404 (needs nginx bootstrap)" "404" "$(curl -s -m 10 -o /dev/null -w '%{http_code}' "http://no-such-site.dev.$ZONE/")"
body="$(curl -s -m 10 "https://sitewright-dev.$ZONE/login" | grep -c '<h1>Sitewright</h1>')"
check "public: admin sign-in page over HTTPS (needs a certificate for the admin host)" "1" "$body"

# Standalone mode (M6): the daemon serves HTTPS itself on 18443 with a certificate for the
# exact hostname, and gives unknown names no certificate at all.
subject="$(echo | timeout 10 openssl s_client -connect "smoke.dev.$ZONE:18443" -servername "smoke.dev.$ZONE" 2>/dev/null | openssl x509 -noout -subject 2>/dev/null)"
check "standalone: certificate for smoke.dev.$ZONE on port 18443" "subject=CN = smoke.dev.$ZONE" "$subject"
code="$(curl -sk -m 10 -o /dev/null -w '%{http_code}' "https://smoke.dev.$ZONE:18443/")"
check "standalone: site served over HTTPS on 18443" "200" "$code"
unknown="$(echo | timeout 10 openssl s_client -connect "no-such-site.dev.$ZONE:18443" -servername "no-such-site.dev.$ZONE" 2>/dev/null | openssl x509 -noout -subject 2>/dev/null)"
check "standalone: no certificate for an unknown hostname" "" "$unknown"

# The sites wildcard must be DNS-only for standalone-mode TLS tests (M6).
if getent ahostsv4 "anything.dev.$ZONE" | awk '{print $1}' | grep -qE '^(104\.|172\.6[4-9]\.|172\.7[01]\.)'; then
  echo "note  *.dev.$ZONE resolves to Cloudflare (proxied); M6 needs it DNS-only"
fi

exit $fail
