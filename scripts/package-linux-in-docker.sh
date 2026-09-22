#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "usage: $0 --output <dir> [--formats deb,appimage]" >&2
    exit 2
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT=""
FORMATS="deb,appimage"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output) OUTPUT="$2"; shift 2 ;;
        --formats) FORMATS="$2"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$OUTPUT" ]] || usage
mkdir -p "$OUTPUT"
OUTPUT="$(cd "$OUTPUT" && pwd)"

IMAGE="multicore-linux-build:ubuntu22.04"
docker build -t "$IMAGE" -f "$ROOT/packaging/linux/Dockerfile.build" "$ROOT/packaging/linux"

docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp/home \
    -e CARGO_TARGET_DIR=/build/multicore/target/linux-release-ubuntu \
    -v "$ROOT:/build/multicore" \
    -v "$OUTPUT:/output" \
    -w /build/multicore \
    "$IMAGE" \
    bash -euo pipefail -c '
        mkdir -p "$HOME"
        rm -rf target/linux-stage-ubuntu
        ./scripts/package-linux-release.sh --output target/linux-stage-ubuntu
        ./scripts/build-linux-packages.sh --stage target/linux-stage-ubuntu --output /output --formats '"$FORMATS"'
    '
