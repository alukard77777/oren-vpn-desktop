#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "usage: $0 --output <stage-dir> [--cache <download-dir>]" >&2
    exit 2
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT=""
CACHE="$ROOT/target/linux-downloads"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output) OUTPUT="$2"; shift 2 ;;
        --cache) CACHE="$2"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$OUTPUT" ]] || usage
[[ ! -e "$OUTPUT" ]] || { echo "refusing to overwrite existing $OUTPUT" >&2; exit 1; }

MANIFEST="$ROOT/packaging/linux/versions.json"
TOOLCHAIN="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["build"]["rustToolchain"])' "$MANIFEST")"
rustc --version | grep -q "^rustc $TOOLCHAIN " || {
    echo "rustc $TOOLCHAIN is required" >&2
    exit 1
}

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/linux-release}"
export RUSTFLAGS="--remap-path-prefix=$ROOT=/build/multicore --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup"
cargo build --release --locked \
    --manifest-path "$ROOT/Cargo.toml" \
    -p multicore-desktop -p multicore-daemon -p multicore-core-host

mkdir -p "$CACHE"
fetch_core() {
    local name="$1"
    local fields
    fields="$(python3 - "$MANIFEST" "$name" <<'PY'
import json, sys
asset = json.load(open(sys.argv[1]))["components"][sys.argv[2]]["asset"]
print(asset["url"], asset["name"], asset["sha256"], asset["format"], asset["entry"], asset["executableSha256"])
PY
)"
    read -r url file sha format entry exe_sha <<<"$fields"
    local archive="$CACHE/$file"
    if [[ ! -f "$archive" ]] || ! echo "$sha  $archive" | sha256sum -c --status; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$archive.part" "$url"
        mv "$archive.part" "$archive"
    fi
    echo "$sha  $archive" | sha256sum -c --quiet
    local target="$STAGE_CORES/$name"
    case "$format" in
        zip) unzip -p "$archive" "$entry" > "$target" ;;
        gzip) gunzip -c "$archive" > "$target" ;;
        *) echo "unsupported archive format $format" >&2; exit 1 ;;
    esac
    echo "$exe_sha  $target" | sha256sum -c --quiet
    chmod 0755 "$target"
}

PACKAGE_ROOT="$OUTPUT/usr/lib/multicore"
STAGE_CORES="$PACKAGE_ROOT/cores"
install -d -m 0755 "$PACKAGE_ROOT/runtime" "$STAGE_CORES" "$OUTPUT/usr/bin" \
    "$OUTPUT/usr/share/applications" "$OUTPUT/usr/share/icons/hicolor/scalable/apps" \
    "$OUTPUT/usr/share/polkit-1/actions" "$OUTPUT/usr/share/licenses/multicore"

install -m 0755 "$CARGO_TARGET_DIR/release/multicore-desktop" "$PACKAGE_ROOT/multicore"
install -m 0755 "$CARGO_TARGET_DIR/release/multicore-daemon" "$PACKAGE_ROOT/runtime/multicore-daemon"
install -m 0755 "$CARGO_TARGET_DIR/release/multicore-core-host" "$PACKAGE_ROOT/runtime/multicore-core-host"
strip --strip-all "$PACKAGE_ROOT/multicore" "$PACKAGE_ROOT/runtime/multicore-daemon" \
    "$PACKAGE_ROOT/runtime/multicore-core-host"
fetch_core xray
fetch_core mihomo

ln -s ../lib/multicore/multicore "$OUTPUT/usr/bin/multicore"
install -m 0644 "$ROOT/packaging/linux/multicore.desktop" "$OUTPUT/usr/share/applications/multicore.desktop"
install -m 0644 "$ROOT/apps/multicore-desktop/assets/app-icon.svg" \
    "$OUTPUT/usr/share/icons/hicolor/scalable/apps/multicore.svg"
install -m 0644 "$ROOT/packaging/linux/io.github.fatyzzz.multicore.core-host.policy" \
    "$OUTPUT/usr/share/polkit-1/actions/io.github.fatyzzz.multicore.core-host.policy"
install -m 0644 "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE" "$ROOT/packaging/linux/THIRD_PARTY_NOTICES.md" \
    "$ROOT/packaging/windows-x64/licenses/Xray-core-MPL-2.0.txt" \
    "$ROOT/packaging/windows-x64/licenses/mihomo-GPL-3.0.txt" \
    "$OUTPUT/usr/share/licenses/multicore/"

for binary in "$PACKAGE_ROOT/multicore" "$PACKAGE_ROOT/runtime/multicore-daemon" "$PACKAGE_ROOT/runtime/multicore-core-host"; do
    if grep -a -q -F -e "$ROOT" -e "$HOME/" "$binary"; then
        echo "private build path found in $binary" >&2
        exit 1
    fi
done

echo "staged MultiCore in $OUTPUT"
