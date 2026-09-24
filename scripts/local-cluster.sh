#!/usr/bin/env bash
# Run the local multi-node deployment: Postgres metastore + Cachey (docker) and
# 3 rustie-nodes + rustie-serve (host processes) from configs/rustie-deploy.local.yaml.
#
#   scripts/local-cluster.sh check     # preflight + rustie-node --check (live checks)
#   scripts/local-cluster.sh up        # check, then start nodes and rustie-serve
#   scripts/local-cluster.sh status
#   scripts/local-cluster.sh down      # stop nodes and serve (docker containers stay up)
#   scripts/local-cluster.sh down --all
#   PROFILE=debug scripts/local-cluster.sh up      # default profile: release
#   NODE_EXTRA_ARGS=--no-cachey-fallback scripts/local-cluster.sh up   # extra rustie-node args
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CONFIG="${DEPLOY_CONFIG:-configs/rustie-deploy.local.yaml}"
PROFILE="${PROFILE:-release}"
BIN="$ROOT/target/$PROFILE"
RUN="$ROOT/.run/local"
NODES=(rustie-node-1 rustie-node-2 rustie-node-3)
export PROTOC="${PROTOC:-/home/saurav/miniconda3/lib/python3.13/site-packages/torch/bin/protoc}"

log() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# .env: S3 credentials / endpoint / region (never printed).
if [ -f .env ]; then set -a; . ./.env; set +a; fi
: "${PG_PASSWORD:=rustie}"; export PG_PASSWORD

need_env() {
  local missing=0 v hint
  for pair in "S3_ENDPOINT|your provider's S3 URL, e.g. https://<region>.contabostorage.com" \
              "S3_REGION|the region name your provider signs with" \
              "S3_ACCESS_KEY|the S3 access key" \
              "S3_SECRET_KEY|the S3 secret key"; do
    v="${pair%%|*}"; hint="${pair#*|}"
    if [ -z "${!v:-}" ]; then
      printf 'missing %s in .env: %s\n' "$v" "$hint" >&2; missing=1
    fi
  done
  [ "$missing" -eq 0 ] || die "add the values above to $ROOT/.env (see .env.example) and save the file"
}

ensure_binaries() {
  local flag=(); [ "$PROFILE" = release ] && flag=(--release)
  if [ ! -x "$BIN/rustie-node" ] || [ ! -x "$BIN/rustie-serve" ]; then
    log "building rustie-node and rustie-serve ($PROFILE); this can take several minutes"
    cargo build "${flag[@]}" -p rustie-node -p rustie-search --bins
  fi
}

ensure_postgres() {
  if docker ps --format '{{.Names}}' | grep -qx rustie-postgres; then return; fi
  if docker ps -a --format '{{.Names}}' | grep -qx rustie-postgres; then
    log "starting existing rustie-postgres container"; docker start rustie-postgres >/dev/null
  else
    die "no rustie-postgres container. Create it once with: docker run -d --name rustie-postgres -p 5433:5432 -e POSTGRES_USER=rustie -e POSTGRES_PASSWORD=rustie -e POSTGRES_DB=rustie postgres:16"
  fi
  for _ in $(seq 1 30); do
    docker exec rustie-postgres pg_isready -U rustie -d rustie >/dev/null 2>&1 && return
    sleep 1
  done
  die "rustie-postgres did not become ready"
}

ensure_cachey() {
  log "starting Cachey (docker-compose.local.yml)"
  docker compose -f docker-compose.local.yml up -d cachey
  for _ in $(seq 1 30); do
    curl -fsS http://127.0.0.1:9020/stats >/dev/null 2>&1 && return
    sleep 1
  done
  die "Cachey did not answer on :9020; see: docker logs rustie-cachey-s3"
}

preflight() { need_env; ensure_binaries; ensure_postgres; ensure_cachey; }

check() {
  preflight
  "$BIN/rustie-node" --deploy-config "$CONFIG" --check
}

pid_alive() { [ -f "$1" ] && kill -0 "$(cat "$1")" 2>/dev/null; }

wait_port() { # host port label
  for _ in $(seq 1 60); do
    (exec 3<>"/dev/tcp/$1/$2") 2>/dev/null && return 0
    sleep 1
  done
  die "$3 did not open $1:$2; see $RUN/*.log"
}

up() {
  check
  mkdir -p "$RUN"
  for id in "${NODES[@]}"; do
    if pid_alive "$RUN/$id.pid"; then log "$id already running"; continue; fi
    log "starting $id"
    nohup "$BIN/rustie-node" --deploy-config "$CONFIG" --node-id "$id" ${NODE_EXTRA_ARGS:-} >"$RUN/$id.log" 2>&1 &
    echo $! >"$RUN/$id.pid"
  done
  wait_port 127.0.0.1 7281 rustie-node-1
  wait_port 127.0.0.1 7381 rustie-node-2
  wait_port 127.0.0.1 7481 rustie-node-3
  if pid_alive "$RUN/serve.pid"; then log "rustie-serve already running"; else
    log "starting rustie-serve"
    nohup "$BIN/rustie-serve" --deploy-config "$CONFIG" >"$RUN/serve.log" 2>&1 &
    echo $! >"$RUN/serve.pid"
  fi
  wait_port 127.0.0.1 8080 rustie-serve
  status
  echo
  echo "try: curl -G localhost:8080/v1/search --data-urlencode 'q=[word=John]' -d limit=3"
}

status() {
  for id in "${NODES[@]}" serve; do
    if pid_alive "$RUN/$id.pid"; then printf '%-14s running (pid %s)\n' "$id" "$(cat "$RUN/$id.pid")"
    else printf '%-14s stopped\n' "$id"; fi
  done
  docker ps --format '{{.Names}}\t{{.Status}}' | grep -E 'rustie-postgres|rustie-cachey-s3' || true
}

stop_pid() {
  local f="$1"
  if pid_alive "$f"; then
    local pid; pid="$(cat "$f")"
    # Only signal a process that is really one of ours.
    if tr '\0' ' ' <"/proc/$pid/cmdline" | grep -q 'rustie-'; then kill "$pid"; fi
  fi
  rm -f "$f"
}

down() {
  for id in serve "${NODES[@]}"; do stop_pid "$RUN/$id.pid"; done
  log "nodes and rustie-serve stopped"
  if [ "${1:-}" = "--all" ]; then
    docker compose -f docker-compose.local.yml down
    docker stop rustie-postgres >/dev/null 2>&1 || true
    log "Cachey and Postgres stopped"
  fi
}

case "${1:-}" in
  check) check ;;
  up) up ;;
  status) status ;;
  down) shift; down "$@" ;;
  *) sed -n '2,11p' "$0"; exit 1 ;;
esac
