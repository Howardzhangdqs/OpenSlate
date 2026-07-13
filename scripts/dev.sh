#!/usr/bin/env bash
#
# scripts/dev.sh — OpenSlate development container helper (Linux).
#
# Builds the openslate binary with your host cargo (reusing the target/ cache),
# then copies it into a minimal ubuntu/debian image. No compile inside docker,
# no big rust base image — fast iteration.
#
# Usage:
#   scripts/dev.sh build              build the dev image
#   scripts/dev.sh run  <args...>     run an openslate command (repo mounted at /workspace)
#   scripts/dev.sh shell              interactive bash inside the container
#   scripts/dev.sh logs <args...>     run non-interactively, no tty (for piping)
#   scripts/dev.sh clean              remove the image + data volume
#   scripts/dev.sh help               show this help
#
# Examples:
#   scripts/dev.sh build
#   scripts/dev.sh run --config /workspace/openslate.toml validate
#   scripts/dev.sh run --config /workspace/openslate.toml run --prompt "hello"
#   scripts/dev.sh shell
#
# Env overrides:
#   OPENSLATE_IMAGE       image name (default: openslate:dev)
#   OPENSLATE_DATA_VOL    named data volume (default: openslate-data)
#   OPENSLATE_DATA_DIR    host dir to mount at /data (overrides the named volume)
#
set -euo pipefail

IMAGE="${OPENSLATE_IMAGE:-openslate:dev}"
DATA_VOL="${OPENSLATE_DATA_VOL:-openslate-data}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

have() { command -v "$1" >/dev/null 2>&1; }
say()  { printf '>> %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# pick a runtime base whose glibc >= host glibc (host-built binary must run inside)
pick_runtime_base() {
  local ver
  ver=$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+' | head -1 || echo 0)
  if   awk "BEGIN{exit !($ver+0 >= 2.39)}"; then echo "ubuntu:24.04"
  elif awk "BEGIN{exit !($ver+0 >= 2.36)}"; then echo "debian:bookworm-slim"
  else echo "ubuntu:24.04"
  fi
}

# locate a usable CA bundle on the host
find_ca() {
  for p in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt; do
    [ -f "$p" ] && { echo "$p"; return 0; }
  done
  return 1
}

ensure_image() {
  docker image inspect "$IMAGE" >/dev/null 2>&1 && return 0
  die "image $IMAGE not found; run '$0 build' first"
}

cmd_build() {
  [ "$(uname -s)" = Linux ] || die "this helper builds a Linux binary on the host; run it on Linux"
  have cargo  || die "cargo not found on PATH"
  have docker || die "docker not found on PATH"
  local base; base=$(pick_runtime_base)
  local ca;   ca=$(find_ca) || die "no CA bundle found (install ca-certificates)"

  say "cargo build --release (runtime base: $base)"
  ( cd "$ROOT" && cargo build --release -p openslate-cli )

  local ctx; ctx="$(mktemp -d "${TMPDIR:-/tmp}/openslate-dev.XXXXXX")"
  cp "$ROOT/target/release/openslate" "$ctx/openslate-bin"
  cp "$ca"                            "$ctx/ca-certificates.crt"

  say "docker build (copies host binary in, offline)"
  if docker build -t "$IMAGE" -f - "$ctx" <<EOF; then
FROM $base
COPY ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY openslate-bin /usr/local/bin/openslate
WORKDIR /workspace
VOLUME ["/workspace", "/data"]
ENTRYPOINT ["openslate"]
CMD ["--help"]
EOF
    rm -rf "$ctx"
    say "built $IMAGE"
  else
    rm -rf "$ctx"
    die "docker build failed"
  fi
}

# volume mount flags shared by run/shell/logs
mount_args=( -v "$ROOT:/workspace" )
if [ -n "${OPENSLATE_DATA_DIR:-}" ]; then
  mount_args+=( -v "$OPENSLATE_DATA_DIR:/data" )
else
  mount_args+=( -v "$DATA_VOL:/data" )
fi

cmd_run() {
  ensure_image
  [ $# -gt 0 ] || set -- --help
  local tty=(); [ -t 0 ] && tty=(-t)
  docker run --rm -i "${tty[@]}" "${mount_args[@]}" "$IMAGE" "$@"
}

cmd_logs() {
  ensure_image
  [ $# -gt 0 ] || die "logs: pass an openslate command"
  docker run --rm "${mount_args[@]}" "$IMAGE" "$@"
}

cmd_shell() {
  ensure_image
  docker run --rm -it --entrypoint bash "${mount_args[@]}" -w /workspace "$IMAGE"
}

cmd_clean() {
  docker rmi "$IMAGE" 2>/dev/null && say "removed image $IMAGE" || say "image $IMAGE not present"
  docker volume rm "$DATA_VOL" 2>/dev/null && say "removed volume $DATA_VOL" || true
}

show_help() { sed -n '2,20p' "$0"; }

[ $# -gt 0 ] || { show_help; exit 0; }
cmd="$1"; shift
case "$cmd" in
  build)         cmd_build "$@";;
  run)           cmd_run "$@";;
  logs)          cmd_logs "$@";;
  shell)         cmd_shell;;
  clean)         cmd_clean;;
  help|-h|--help) show_help;;
  *) die "unknown command '$cmd' (try: $0 help)";;
esac
