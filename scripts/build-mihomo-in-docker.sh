#!/usr/bin/env bash
# Runs scripts/build-mihomo.sh inside the Linux release build image (packaging/linux/Dockerfile.build),
# the same container the Linux packages are built in. Go, the Mihomo source and the patches are
# all SHA-256-pinned by packaging/mihomo-patches/manifest.json, so the binaries are byte-identical
# to a build on any other host.
#
# usage: scripts/build-mihomo-in-docker.sh --output <new-dir> [--cache-dir <dir>] [target ...]
set -euo pipefail

usage() {
    echo "usage: $0 --output <new-dir> [--cache-dir <dir>] [target ...]" >&2
    exit 2
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT=""
CACHE_DIR=""
TARGETS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output) [[ $# -ge 2 ]] || usage; OUTPUT="$2"; shift 2 ;;
        --cache-dir) [[ $# -ge 2 ]] || usage; CACHE_DIR="$2"; shift 2 ;;
        -*) usage ;;
        *) TARGETS+=("$1"); shift ;;
    esac
done
[[ -n "$OUTPUT" ]] || usage
[[ ! -e "$OUTPUT" ]] || { echo "refusing to overwrite existing $OUTPUT" >&2; exit 1; }
for target in ${TARGETS[@]+"${TARGETS[@]}"}; do
    [[ "$target" =~ ^[a-z0-9-]+$ ]] || { echo "invalid target $target" >&2; exit 2; }
done
OUTPUT_PARENT="$(mkdir -p "$(dirname "$OUTPUT")" && cd "$(dirname "$OUTPUT")" && pwd)"
OUTPUT_NAME="$(basename "$OUTPUT")"
CACHE_DIR="${CACHE_DIR:-$ROOT/target/mihomo-cache}"
mkdir -p "$CACHE_DIR"
CACHE_DIR="$(cd "$CACHE_DIR" && pwd)"

IMAGE="multicore-linux-build:ubuntu22.04"
docker build -t "$IMAGE" -f "$ROOT/packaging/linux/Dockerfile.build" "$ROOT/packaging/linux"

docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp/home \
    -e GOMAXPROCS="${GOMAXPROCS:-4}" \
    -e MIHOMO_SKIP_TESTS="${MIHOMO_SKIP_TESTS:-0}" \
    -v "$ROOT:/build/multicore:ro" \
    -v "$CACHE_DIR:/cache" \
    -v "$OUTPUT_PARENT:/output" \
    -w /build/multicore \
    "$IMAGE" \
    bash -euo pipefail -c 'mkdir -p "$HOME" && ./scripts/build-mihomo.sh --cache /cache --output "/output/$0" "$@"' \
    "$OUTPUT_NAME" ${TARGETS[@]+"${TARGETS[@]}"}
