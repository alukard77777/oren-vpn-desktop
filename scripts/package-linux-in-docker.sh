#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "usage: $0 --output <dir> [--formats deb,appimage] [--cache-dir <dir>]" >&2
    exit 2
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT=""
FORMATS="deb,appimage"
CACHE_DIR=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output) OUTPUT="$2"; shift 2 ;;
        --formats) FORMATS="$2"; shift 2 ;;
        --cache-dir) CACHE_DIR="$2"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$OUTPUT" ]] || usage
mkdir -p "$OUTPUT"
OUTPUT="$(cd "$OUTPUT" && pwd)"

IMAGE="multicore-linux-build:ubuntu22.04"
docker build -t "$IMAGE" -f "$ROOT/packaging/linux/Dockerfile.build" "$ROOT/packaging/linux"

# Optional persistent caches for repeated local builds: the crates.io registry, the Cargo target
# directory and the pinned-download cache. They are mounted at exactly the in-container paths a
# cache-less build uses, so the build flags, the path remapping and the staged files are the same;
# only already-compiled dependencies and already-verified downloads are reused.
CACHE_MOUNTS=()
if [[ -n "$CACHE_DIR" ]]; then
    mkdir -p "$CACHE_DIR/cargo-registry" "$CACHE_DIR/target" "$CACHE_DIR/downloads" \
        "$ROOT/target/linux-release-ubuntu" "$ROOT/target/linux-downloads"
    CACHE_DIR="$(cd "$CACHE_DIR" && pwd)"
    CACHE_MOUNTS=(
        -v "$CACHE_DIR/cargo-registry:/opt/cargo/registry"
        -v "$CACHE_DIR/target:/build/multicore/target/linux-release-ubuntu"
        -v "$CACHE_DIR/downloads:/build/multicore/target/linux-downloads"
    )
fi

docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp/home \
    -e CARGO_TARGET_DIR=/build/multicore/target/linux-release-ubuntu \
    -e MULTICORE_UPDATE_REPOSITORY="${MULTICORE_UPDATE_REPOSITORY:-}" \
    -v "$ROOT:/build/multicore" \
    -v "$OUTPUT:/output" \
    ${CACHE_MOUNTS[@]+"${CACHE_MOUNTS[@]}"} \
    -w /build/multicore \
    "$IMAGE" \
    bash -euo pipefail -c '
        mkdir -p "$HOME"
        rm -rf target/linux-stage-ubuntu
        ./scripts/package-linux-release.sh --output target/linux-stage-ubuntu
        ./scripts/build-linux-packages.sh --stage target/linux-stage-ubuntu --output /output --formats '"$FORMATS"'
    '
