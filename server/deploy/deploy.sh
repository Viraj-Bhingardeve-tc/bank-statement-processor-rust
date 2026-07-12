#!/usr/bin/env bash
# server/deploy/deploy.sh — deploy the latest license-server code to this
# VPS (PHASE4_DESIGN.md §8.3: "Deploys — a new image build + `docker
# compose up -d --no-deps server` on the VPS (manually or via a simple CI
# step that SSHes in and runs it); no blue/green or zero-downtime deploy
# machinery designed here — a few seconds of downtime during a `server`
# container restart is an accepted trade-off at this stage").
#
# Run FROM the repository root on the VPS (or set REPO_DIR), e.g. over SSH
# or as a CI step that SSHes in and runs this script.
#
# Template — this covers the steps PHASE4_DESIGN.md §8.3 describes; adjust
# to your actual VPS/CI setup (e.g. swap `git pull` for whatever gets your
# code onto the box) before relying on it in production.

set -euo pipefail

REPO_DIR="${REPO_DIR:-$(pwd)}"
cd "$REPO_DIR"

if [[ ! -f server/.env ]]; then
    echo "server/.env not found — copy server/.env.example and fill it in first (see server/README.md)." >&2
    exit 1
fi

echo "==> Pulling latest code"
git pull --ff-only

echo "==> Building and restarting license-server only"
# --no-deps: rebuild/recreate license-server without touching the already-
# running postgres/caddy containers — this is a server-only deploy, exactly
# PHASE4_DESIGN.md §8.3's described step. Migrations run automatically at
# license-server startup (server/src/main.rs), before it accepts traffic —
# no separate migration step here.
docker compose up -d --build --no-deps license-server

echo "==> Recent license-server logs"
docker compose logs --tail=50 license-server

cat <<'EOF'

Deploy triggered. Verify:
  - Logs above show "starting license-server" with no migration/bind errors.
  - curl -f https://<your-domain>/healthz
  - curl -f https://<your-domain>/readyz   (also checks DB connectivity)

Rollback (PHASE4_DESIGN.md §10 stage 5): re-deploy the previous known-good
commit the same way, or `docker compose up -d --no-deps license-server`
after `git checkout <previous-tag>`. If the bad deploy already wrote bad
data, restore the database with server/deploy/restore.sh before/instead of
just rolling the image back.
EOF
