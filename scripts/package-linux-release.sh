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

# Skia (the desktop's GPU renderer): skia-bindings links the SHA-256-pinned rust-skia
# prebuilt archive from the download cache instead of fetching an unverified one itself.
mkdir -p "$CACHE"
read -r SKIA_KEY SKIA_URL SKIA_SHA <<<"$(python3 - "$MANIFEST" <<'PY'
import json, sys
skia = json.load(open(sys.argv[1]))["build"]["skiaBinaries"]
print(skia["key"], skia["url"], skia["sha256"])
PY
)"
SKIA_ARCHIVE="$CACHE/skia-binaries-$SKIA_KEY.tar.gz"
if [[ ! -f "$SKIA_ARCHIVE" ]] || ! echo "$SKIA_SHA  $SKIA_ARCHIVE" | sha256sum -c --status; then
    curl -fsSL --proto '=https' --tlsv1.2 -o "$SKIA_ARCHIVE.part" "$SKIA_URL"
    mv "$SKIA_ARCHIVE.part" "$SKIA_ARCHIVE"
fi
echo "$SKIA_SHA  $SKIA_ARCHIVE" | sha256sum -c --quiet
CACHE_ABS="$(cd "$CACHE" && pwd)"
export SKIA_BINARIES_URL="file://$CACHE_ABS/skia-binaries-{key}.tar.gz"
unset FORCE_SKIA_BUILD FORCE_SKIA_BINARIES_DOWNLOAD SKIA_SOURCE_DIR SKIA_LIBRARY_SEARCH_PATH SKIA_DEBUG
export RUSTFLAGS="--remap-path-prefix=$ROOT=/source/multicore --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup"
cargo build --release --locked \
    --manifest-path "$ROOT/Cargo.toml" \
    -p multicore-desktop -p multicore-daemon -p multicore-core-host
# The newest skia-bindings output is this build's (a persistent target dir keeps older ones).
linked_skia="$(ls -t "$CARGO_TARGET_DIR"/release/build/skia-bindings-*/out/skia/key.txt 2>/dev/null | head -n 1)"
[[ -n "$linked_skia" && "$(cat "$linked_skia")" == "$SKIA_KEY" ]] || {
    echo "the build did not link the pinned Skia binaries $SKIA_KEY" >&2
    exit 1
}

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

# Mihomo is never the stock MetaCubeX download: it is the pinned upstream source plus the patch
# series in packaging/mihomo-patches (REALITY fixes, docs/reality.md), built by
# scripts/build-mihomo.sh. MULTICORE_MIHOMO_BINARIES_DIR may hold an earlier output of that script;
# either way the binary must match the SHA-256 pinned in versions.json.
install_mihomo() {
    local fields
    fields="$(python3 - "$MANIFEST" <<'PY'
import json, sys
mihomo = json.load(open(sys.argv[1]))["components"]["mihomo"]
if "asset" in mihomo or "build" not in mihomo:
    sys.exit("versions.json must pin the patched Mihomo build (components.mihomo.build), not a stock asset")
build = mihomo["build"]
print(build["target"], build["file"], build["executableSha256"])
PY
)"
    local target file exe_sha
    read -r target file exe_sha <<<"$fields"
    local directory="${MULTICORE_MIHOMO_BINARIES_DIR:-}" scratch=""
    if [[ -z "$directory" ]]; then
        scratch="$(mktemp -d "$CACHE/mihomo-build.XXXXXX")"
        directory="$scratch/out"
        "$ROOT/scripts/build-mihomo.sh" --cache "$CACHE/mihomo" --output "$directory" "$target"
    fi
    echo "$exe_sha  $directory/$file" | sha256sum -c --quiet || {
        echo "the Mihomo binary in $directory does not match its pinned SHA-256" >&2
        exit 1
    }
    install -m 0755 "$directory/$file" "$STAGE_CORES/mihomo"
    echo "$exe_sha  $STAGE_CORES/mihomo" | sha256sum -c --quiet
    if [[ -n "$scratch" ]]; then rm -rf "$scratch"; fi
}

PACKAGE_ROOT="$OUTPUT/usr/lib/multicore"
STAGE_CORES="$PACKAGE_ROOT/cores"
install -d -m 0755 "$PACKAGE_ROOT/runtime" "$STAGE_CORES" "$OUTPUT/usr/bin" \
    "$OUTPUT/usr/share/applications" "$OUTPUT/usr/share/icons/hicolor/scalable/apps" \
    "$OUTPUT/usr/share/polkit-1/actions" "$OUTPUT/usr/share/licenses/multicore" \
    "$OUTPUT/usr/lib/systemd/system" "$OUTPUT/usr/share/metainfo"

# The vendored dashboard is optional so manifests that predate it keep staging unchanged; a real
# production manifest always carries it (see packaging/linux/versions.json). Its filenames are
# content-hashed by the upstream build and are not otherwise pinned here -- the archive's SHA-256
# already fully determines the set of files it can produce, and dashboard/index.html additionally
# gets its own pre-patch SHA-256 check before scripts/patch_dashboard_csp.py touches it.
fetch_dashboard() {
    local present
    present="$(python3 -c 'import json,sys; print("yes" if "dashboard" in json.load(open(sys.argv[1]))["components"] else "no")' "$MANIFEST")"
    [[ "$present" == "yes" ]] || return 0

    local fields
    fields="$(python3 - "$MANIFEST" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))["components"]["dashboard"]
print(d["asset"]["url"], d["asset"]["name"], d["asset"]["sha256"], d["asset"]["rootPrefix"],
      d["asset"]["indexHtmlSha256"], d["licenseFile"]["url"], d["licenseFile"]["name"], d["licenseFile"]["sha256"])
PY
)"
    read -r url file sha root_prefix index_sha license_url license_name license_sha <<<"$fields"

    local archive="$CACHE/$file"
    if [[ ! -f "$archive" ]] || ! echo "$sha  $archive" | sha256sum -c --status; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$archive.part" "$url"
        mv "$archive.part" "$archive"
    fi
    echo "$sha  $archive" | sha256sum -c --quiet

    local extract_tmp
    extract_tmp="$(mktemp -d "$CACHE/dashboard-extract.XXXXXX")"
    # unzip refuses absolute paths and ".." traversal by default; the archive's own SHA-256 is
    # already pinned above, so this is defense in depth, not the primary control.
    unzip -q "$archive" -d "$extract_tmp"
    local extracted_root="$extract_tmp/${root_prefix%/}"
    if [[ ! -d "$extracted_root" ]]; then
        echo "dashboard archive is missing its expected root: $root_prefix" >&2
        rm -rf "$extract_tmp"
        exit 1
    fi
    if find "$extracted_root" -type l | grep -q .; then
        echo "dashboard archive contains a symlink" >&2
        rm -rf "$extract_tmp"
        exit 1
    fi
    if [[ ! -f "$extracted_root/index.html" ]]; then
        echo "dashboard archive does not contain an index.html at its root" >&2
        rm -rf "$extract_tmp"
        exit 1
    fi

    local dashboard_dir="$PACKAGE_ROOT/dashboard"
    mv "$extracted_root" "$dashboard_dir"
    rm -rf "$extract_tmp"

    python3 "$ROOT/scripts/patch_dashboard_csp.py" "$MANIFEST" "$dashboard_dir/index.html"
    find "$dashboard_dir" -type d -exec chmod 0755 {} \;
    find "$dashboard_dir" -type f -exec chmod 0644 {} \;

    local license_archive="$CACHE/$license_name"
    if [[ ! -f "$license_archive" ]] || ! echo "$license_sha  $license_archive" | sha256sum -c --status; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$license_archive.part" "$license_url"
        mv "$license_archive.part" "$license_archive"
    fi
    echo "$license_sha  $license_archive" | sha256sum -c --quiet
    install -m 0644 "$license_archive" "$OUTPUT/usr/share/licenses/multicore/$license_name"
}

install -m 0755 "$CARGO_TARGET_DIR/release/multicore-desktop" "$PACKAGE_ROOT/multicore"
install -m 0755 "$CARGO_TARGET_DIR/release/multicore-daemon" "$PACKAGE_ROOT/runtime/multicore-daemon"
install -m 0755 "$CARGO_TARGET_DIR/release/multicore-core-host" "$PACKAGE_ROOT/runtime/multicore-core-host"
strip --strip-all "$PACKAGE_ROOT/multicore" "$PACKAGE_ROOT/runtime/multicore-daemon" \
    "$PACKAGE_ROOT/runtime/multicore-core-host"
fetch_core xray
install_mihomo
fetch_dashboard

ln -s ../lib/multicore/multicore "$OUTPUT/usr/bin/multicore"
install -m 0644 "$ROOT/packaging/linux/multicore.desktop" "$OUTPUT/usr/share/applications/multicore.desktop"
install -m 0644 "$ROOT/apps/multicore-desktop/assets/app-icon.svg" \
    "$OUTPUT/usr/share/icons/hicolor/scalable/apps/multicore.svg"
install -m 0644 "$ROOT/packaging/linux/io.github.fatyzzz.multicore.core-host.policy" \
    "$OUTPUT/usr/share/polkit-1/actions/io.github.fatyzzz.multicore.core-host.policy"
# AppStream metadata for software centres and AppImage catalogues; SOURCE_DATE_EPOCH pins the
# release date for a reproducible stage.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/apps/multicore-desktop/Cargo.toml" | head -1)"
RELEASE_DATE="$(date -u -d "@${SOURCE_DATE_EPOCH:-$(date -u +%s)}" +%Y-%m-%d)"
sed -e "s/@VERSION@/$VERSION/" -e "s/@DATE@/$RELEASE_DATE/" \
    "$ROOT/packaging/linux/io.github.fatyzzz.multicore.metainfo.xml" \
    > "$OUTPUT/usr/share/metainfo/io.github.fatyzzz.multicore.metainfo.xml"
chmod 0644 "$OUTPUT/usr/share/metainfo/io.github.fatyzzz.multicore.metainfo.xml"
# The always-ready service unit. deb/rpm/Arch enable it; the AppImage build removes it again
# (the AppImage keeps the on-demand pkexec host).
install -m 0644 "$ROOT/packaging/linux/multicore.service" "$OUTPUT/usr/lib/systemd/system/multicore.service"
install -m 0644 "$ROOT/LICENSE" "$ROOT/packaging/linux/THIRD_PARTY_NOTICES.md" \
    "$ROOT/packaging/windows-x64/licenses/Xray-core-MPL-2.0.txt" \
    "$ROOT/packaging/windows-x64/licenses/mihomo-GPL-3.0.txt" \
    "$OUTPUT/usr/share/licenses/multicore/"

for binary in "$PACKAGE_ROOT/multicore" "$PACKAGE_ROOT/runtime/multicore-daemon" "$PACKAGE_ROOT/runtime/multicore-core-host"; do
    if grep -a -q -F -e "$ROOT/" -e "$HOME/" "$binary"; then
        echo "private build path found in $binary" >&2
        exit 1
    fi
done

echo "staged MultiCore in $OUTPUT"
