#!/usr/bin/env bash
# End-to-end check of containerized sites: publish a site with a backend in a throwaway
# Sitewright, download its image, then load and run the image with Docker and use the
# site. Without Docker (pass --no-docker, or when docker is missing) it stops after
# checking the archive's structure.
#
#   scripts/container-smoke.sh [--no-docker] [path/to/sitewright]
set -euo pipefail
cd "$(dirname "$0")/.."

use_docker=1
if [ "${1:-}" = "--no-docker" ]; then use_docker=0; shift; fi
bin="$(realpath "${1:-target/debug/sitewright}")"
[ -x "$bin" ] || { echo "error: $bin is not an executable" >&2; exit 1; }
if [ "$use_docker" = 1 ] && ! command -v docker >/dev/null; then
  echo "note: docker not found; checking the archive only"
  use_docker=0
fi

work="$(mktemp -d)"
daemon_pid=""
cleanup() {
  [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null || true
  if [ "$use_docker" = 1 ]; then
    docker rm -f sw-smoke sw-smoke-2 >/dev/null 2>&1 || true
    docker volume rm sw-smoke-data sw-smoke-data-2 >/dev/null 2>&1 || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

data="$work/data"
mkdir -p "$data" "$work/site/backend/migrations" "$work/out"
chmod 700 "$data"
cat > "$data/config.toml" <<TOML
[server]
listen = "127.0.0.1:18990"
[domains]
zones = ["smoke.test"]
public_scheme = "http"
[metrics]
uptime_interval_secs = 0
TOML
sw() { "$bin" --data-dir "$data" "$@"; }

cat > "$work/site/index.html" <<'HTML'
<!doctype html><html lang="en"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Container smoke</title><h1>Container smoke</h1>
HTML
cp skills/build-backend/assets/0001_create_entries.sql "$work/site/backend/migrations/"
cat > "$work/site/backend/index.js" <<'JS'
import { Router, json, db, secrets, HttpError } from "sitewright";
const app = new Router();
app.get("/api/entries", async () => json(await db.query("SELECT id, name, message FROM entries ORDER BY id")));
app.post("/api/entries", async (req) => {
  const { name, message } = await req.data();
  if (!name) throw new HttpError(400, "name is required");
  const { lastInsertRowid } = await db.run(
    "INSERT INTO entries (name, message, visitor, created_at) VALUES (?, ?, ?, ?)",
    name, message ?? "", req.headers.get("x-sitewright-visitor") ?? "", Date.now());
  return json({ id: lastInsertRowid }, 201);
});
app.get("/api/secret-length", async () => json({ length: (await secrets.get("SMOKE_KEY")).length }));
app.get("/api/forbidden", async () => json({ leaked: await secrets.get("LLM_FAST_API_KEY") }));
export default app;
JS

echo "==> starting a throwaway Sitewright"
sw serve >"$work/daemon.log" 2>&1 &
daemon_pid=$!
for _ in $(seq 1 50); do [ -S "$data/admin.sock" ] && break; sleep 0.2; done
sw site import smoke "$work/site" >/dev/null
printf 'value-from-sitewright\n' | sw backend set-secret smoke SMOKE_KEY >/dev/null
curl -fsS -X POST -H 'Host: smoke.smoke.test' -H 'content-type: application/json' \
  -d '{"name":"Before","message":"made under Sitewright"}' http://127.0.0.1:18990/api/entries >/dev/null

echo "==> containerizing and downloading"
sw site containerize smoke >/dev/null
sw site image smoke -o "$work/out" >/dev/null
kill "$daemon_pid"; wait "$daemon_pid" 2>/dev/null || true; daemon_pid=""

image="$work/out/smoke-image.tar"
guide="$work/out/smoke-SETUP.md"
test -s "$image" && test -s "$guide" && test -s "$work/out/smoke-data.db"
tar -tf "$image" | grep -qx 'manifest.json'
grep -q 'SMOKE_KEY' "$guide"
grep -q 'docker load -i smoke-image.tar' "$guide"
if grep -q 'value-from-sitewright' "$guide" || grep -aq 'value-from-sitewright' "$image"; then
  echo "error: a secret's value is in the package" >&2; exit 1
fi
echo "    image: $(du -h "$image" | cut -f1), guide: $(wc -l < "$guide") lines"

if [ "$use_docker" = 0 ]; then
  echo "==> archive checks passed (Docker steps skipped)"
  exit 0
fi

echo "==> docker load and run"
tag="$(docker load -i "$image" | sed -n 's/^Loaded image: //p')"
test -n "$tag"
printf 'SMOKE_KEY=value-in-the-container\nLLM_FAST_API_KEY=must-not-leak\n' > "$work/env"
docker run -d --name sw-smoke -p 18991:8080 --env-file "$work/env" -e PUBLIC_HTTPS=false \
  -v sw-smoke-data:/data --read-only --cap-drop ALL --security-opt no-new-privileges "$tag" >/dev/null
url=http://127.0.0.1:18991
for _ in $(seq 1 50); do curl -fsS "$url/" >/dev/null 2>&1 && break; sleep 0.2; done
curl -fsS "$url/" | grep -q 'Container smoke'
test "$(curl -s -o /dev/null -w '%{http_code}' "$url/backend/index.js")" = 404
test "$(curl -fsS "$url/api/entries")" = '[]'
curl -fsS -X POST -H 'content-type: application/json' -d '{"name":"Ada","message":"hi"}' "$url/api/entries" | grep -q '"id":1'
curl -fsS "$url/api/secret-length" | grep -q '"length":22'
test "$(curl -s -o /dev/null -w '%{http_code}' "$url/api/forbidden")" = 500
test "$(docker inspect -f '{{.Config.User}}' sw-smoke)" = "65532:65532"

echo "==> data survives a restart of the container"
docker rm -f sw-smoke >/dev/null
docker run -d --name sw-smoke -p 18991:8080 --env-file "$work/env" -e PUBLIC_HTTPS=false \
  -v sw-smoke-data:/data --read-only --cap-drop ALL "$tag" >/dev/null
for _ in $(seq 1 50); do curl -fsS "$url/" >/dev/null 2>&1 && break; sleep 0.2; done
curl -fsS "$url/api/entries" | grep -q '"name":"Ada"'

echo "==> importing the data downloaded from Sitewright into a fresh volume"
docker volume create sw-smoke-data-2 >/dev/null
docker run --rm -v sw-smoke-data-2:/data -v "$work/out":/in --user 0 --entrypoint /sitewright "$tag" \
  import-data /in/smoke-data.db >/dev/null
docker run -d --name sw-smoke-2 -p 18992:8080 --env-file "$work/env" -e PUBLIC_HTTPS=false \
  -v sw-smoke-data-2:/data --read-only --cap-drop ALL "$tag" >/dev/null
for _ in $(seq 1 50); do curl -fsS http://127.0.0.1:18992/ >/dev/null 2>&1 && break; sleep 0.2; done
curl -fsS http://127.0.0.1:18992/api/entries | grep -q '"name":"Before"'

echo "==> all container checks passed"
