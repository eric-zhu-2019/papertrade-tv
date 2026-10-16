#!/usr/bin/env bash
# Deploy to a Docker host over SSH:  ./deploy.sh [ssh-host] [remote-dir]
# Safe to re-run: it never overwrites the remote .env or the database volume.
# Runs smoke_test.py on the host afterwards; SMOKE=0 ./deploy.sh skips it.
set -euo pipefail
HOST="${1:?usage: ./deploy.sh <ssh-host> [remote-dir]}"
DIR="${2:-apps/papertrade}"
cd "$(dirname "$0")"

echo "==> copying project to $HOST:~/$DIR"
ssh "$HOST" "mkdir -p $DIR"
rsync -az --delete --exclude target --exclude .git --exclude .env --exclude data --exclude __pycache__ ./ "$HOST:$DIR/"

echo "==> building and starting on $HOST (the first build takes several minutes)"
ssh "$HOST" "bash -s -- $DIR ${SMOKE:-1}" <<'REMOTE'
set -euo pipefail
cd "$HOME/$1"
if docker compose version >/dev/null 2>&1; then DC="docker compose"; else DC="docker-compose"; fi
if [ ! -f .env ]; then
  rand() { LC_ALL=C tr -dc 'a-z0-9' </dev/urandom | head -c "$1" || true; }
  port=8080
  while (ss -ltn 2>/dev/null || netstat -ltn 2>/dev/null) | grep -q ":$port "; do port=$((port + 1)); done
  sed -e "s/^ADMIN_PASSWORD=.*/ADMIN_PASSWORD=$(rand 20)/" -e "s/^INVITE_CODE=.*/INVITE_CODE=club-$(rand 6)/" .env.example > .env
  echo "PT_PORT=$port" >> .env
  chmod 600 .env
  echo "created .env (new admin password and invite code, port $port)"
fi
$DC up -d --build
port=$(grep '^PT_PORT=' .env | cut -d= -f2); port=${port:-8080}
echo "==> waiting for the server on 127.0.0.1:$port"
for i in $(seq 1 30); do curl -fsS "http://127.0.0.1:$port/api/config" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "http://127.0.0.1:$port/api/config" >/dev/null && echo "server is up" || { echo "server did not answer"; $DC logs --tail 40; exit 1; }
sleep 8
echo "==> recent log (look for 'kraken feed connected')"
$DC logs --tail 15 2>&1 | sed 's/\x1b\[[0-9;]*m//g'
echo "==> sign-in details"
grep -E '^(ADMIN_USER|ADMIN_PASSWORD|INVITE_CODE|PT_PORT)=' .env
ip=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for (i = 1; i < NF; i++) if ($i == "src") print $(i + 1)}' || true)
echo "==> open from any device on the LAN:  http://${ip:-<this-host-ip>}:$port"
if [ "$2" != 0 ]; then
  echo "==> smoke test"
  if command -v python3 >/dev/null; then python3 -u smoke_test.py --env-file .env "http://127.0.0.1:$port"
  else echo "python3 not found on this host; smoke test skipped"; fi
fi
REMOTE
