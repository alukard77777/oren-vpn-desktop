#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "usage: $0 --stage <stage-dir> --output <dir> --formats <arch,deb,appimage>" >&2
    exit 2
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STAGE=""
OUTPUT=""
FORMATS=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --stage) STAGE="$(cd "$2" && pwd)"; shift 2 ;;
        --output) OUTPUT="$2"; shift 2 ;;
        --formats) FORMATS="$2"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$STAGE" && -n "$OUTPUT" && -n "$FORMATS" ]] || usage
[[ -x "$STAGE/usr/lib/multicore/multicore" ]] || { echo "$STAGE is not a MultiCore stage" >&2; exit 1; }
mkdir -p "$OUTPUT"
OUTPUT="$(cd "$OUTPUT" && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/apps/multicore-desktop/Cargo.toml" | head -1)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"

build_arch() {
    local work="$WORK/arch"
    mkdir -p "$work"
    tar --zstd -C "$(dirname "$STAGE")" --transform "s,^$(basename "$STAGE"),stage," \
        --owner=0 --group=0 --numeric-owner -cf "$work/multicore-$VERSION-stage.tar.zst" "$(basename "$STAGE")"
    local sha
    sha="$(sha256sum "$work/multicore-$VERSION-stage.tar.zst" | cut -d' ' -f1)"
    sed -e "s/@VERSION@/$VERSION/" -e "s/@SHA256@/$sha/" "$ROOT/packaging/linux/arch/PKGBUILD" > "$work/PKGBUILD"
    (cd "$work" && PKGDEST="$OUTPUT" makepkg --force --nodeps --noconfirm)
}

build_deb() {
    local work="$WORK/deb/multicore_${VERSION}_amd64"
    mkdir -p "$work/DEBIAN"
    cp -a "$STAGE/." "$work/"
    local size
    size="$(du -sk --exclude=DEBIAN "$work" | cut -f1)"
    sed -e "s/@VERSION@/$VERSION/" "$ROOT/packaging/linux/deb/control" > "$work/DEBIAN/control"
    echo "Installed-Size: $size" >> "$work/DEBIAN/control"
    dpkg-deb --root-owner-group -Zxz --build "$work" "$OUTPUT/multicore_${VERSION}_amd64.deb"
}

build_appimage() {
    local tool="$WORK/appimagetool"
    curl -fsSL --proto '=https' --tlsv1.2 -o "$tool" "$APPIMAGETOOL_URL"
    echo "$APPIMAGETOOL_SHA256  $tool" | sha256sum -c --quiet
    chmod 0755 "$tool"
    local appdir="$WORK/MultiCore.AppDir"
    mkdir -p "$appdir"
    cp -a "$STAGE/." "$appdir/"
    install -m 0755 "$ROOT/packaging/linux/appimage/AppRun" "$appdir/AppRun"
    cp "$STAGE/usr/share/applications/multicore.desktop" "$appdir/multicore.desktop"
    cp "$STAGE/usr/share/icons/hicolor/scalable/apps/multicore.svg" "$appdir/multicore.svg"
    ln -s multicore.svg "$appdir/.DirIcon"
    ARCH=x86_64 "$tool" --appimage-extract-and-run --no-appstream "$appdir" \
        "$OUTPUT/MultiCore-$VERSION-x86_64.AppImage"
}

IFS=',' read -r -a requested <<<"$FORMATS"
for format in "${requested[@]}"; do
    case "$format" in
        arch) build_arch ;;
        deb) build_deb ;;
        appimage) build_appimage ;;
        *) echo "unknown format $format" >&2; exit 2 ;;
    esac
done
ls -l "$OUTPUT"
