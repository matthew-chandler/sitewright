#!/usr/bin/env bash
# Regenerate sqlx's offline query data (.sqlx/) against a freshly migrated scratch database.
# Run after changing any `sqlx::query!` call or any file in migrations/. Pass --check to
# verify that .sqlx/ is up to date without changing it (used by CI).
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

db="$PWD/target/sqlx-prepare.db"
mkdir -p target
rm -f "$db" "$db-wal" "$db-shm"
export DATABASE_URL="sqlite://$db"
export SQLX_OFFLINE=false

sqlx database create
sqlx migrate run --source migrations
cargo sqlx prepare --workspace "$@" -- --all-targets
