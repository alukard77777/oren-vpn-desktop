#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STAGE=""
PACKAGES=""
FORMATS=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --stage) STAGE="$2"; shift 2 ;;
    --packages) PACKAGES="$2"; shift 2 ;;
    --formats) FORMATS="$2"; shift 2 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ -n "$STAGE" && -d "$STAGE" ]] || { echo "Missing Linux stage: $STAGE" >&2; exit 1; }
[[ -n "$PACKAGES" && -d "$PACKAGES" ]] || { echo "Missing package directory: $PACKAGES" >&2; exit 1; }
[[ -n "$FORMATS" ]] || { echo "No package formats supplied" >&2; exit 1; }

VERSION="$(sed -n '/^version = / { s/version = "\([^"]*\)"/\1/p; q; }' "$ROOT/apps/multicore-desktop/Cargo.toml")"
[[ -n "$VERSION" ]] || { echo "Could not read desktop version" >&2; exit 1; }

executables=(
  usr/lib/multicore/multicore
  usr/lib/multicore/runtime/multicore-daemon
  usr/lib/multicore/runtime/multicore-core-host
  usr/lib/multicore/cores/xray
  usr/lib/multicore/cores/mihomo
)
metadata=(
  usr/share/applications/multicore.desktop
  usr/share/icons/hicolor/scalable/apps/multicore.svg
  usr/share/polkit-1/actions/io.github.fatyzzz.multicore.core-host.policy
)

for relative in "${executables[@]}"; do
  path="$STAGE/$relative"
  [[ -f "$path" && -x "$path" ]] || { echo "Missing executable: $relative" >&2; exit 1; }
  description="$(file -b "$path")"
  [[ "$description" == *"ELF 64-bit"* && "$description" == *"x86-64"* ]] || {
    echo "Unexpected executable format for $relative: $description" >&2
    exit 1
  }
done

for relative in "${metadata[@]}"; do
  [[ -f "$STAGE/$relative" && -r "$STAGE/$relative" ]] || { echo "Missing metadata: $relative" >&2; exit 1; }
done

desktop_file="$STAGE/usr/share/applications/multicore.desktop"
grep -qx 'Exec=multicore %u' "$desktop_file" || {
  echo 'Linux desktop entry must pass one protocol URL to MultiCore' >&2
  exit 1
}
grep -qx 'MimeType=x-scheme-handler/multicore;' "$desktop_file" || {
  echo 'Linux desktop entry must advertise the multicore URL scheme' >&2
  exit 1
}
grep -Fxq 'exec "$HERE/usr/lib/multicore/multicore" "$@"' "$ROOT/packaging/linux/appimage/AppRun" || {
  echo 'AppImage launcher must forward the protocol URL unchanged' >&2
  exit 1
}

[[ -L "$STAGE/usr/bin/multicore" ]] || { echo "Missing /usr/bin/multicore symlink" >&2; exit 1; }
[[ "$(readlink "$STAGE/usr/bin/multicore")" == "../lib/multicore/multicore" ]] || {
  echo "Incorrect /usr/bin/multicore symlink" >&2
  exit 1
}

require_listing_entries() {
  local listing="$1"
  local prefix="$2"
  local relative
  for relative in "${executables[@]}" "${metadata[@]}"; do
    grep -Fq "${prefix}${relative}" <<<"$listing" || {
      echo "Package is missing ${prefix}${relative}" >&2
      exit 1
    }
  done
}

IFS=',' read -ra requested_formats <<<"$FORMATS"
for format in "${requested_formats[@]}"; do
  case "$format" in
    deb)
      deb="$PACKAGES/multicore_${VERSION}_amd64.deb"
      [[ -f "$deb" ]] || { echo "Missing Debian package: $deb" >&2; exit 1; }
      [[ "$(dpkg-deb -f "$deb" Version)" == "$VERSION" ]] || { echo "Incorrect Debian version" >&2; exit 1; }
      [[ "$(dpkg-deb -f "$deb" Architecture)" == "amd64" ]] || { echo "Incorrect Debian architecture" >&2; exit 1; }
      require_listing_entries "$(dpkg-deb -c "$deb")" "./"
      ;;
    appimage)
      appimage="$PACKAGES/MultiCore-${VERSION}-x86_64.AppImage"
      [[ -f "$appimage" && -x "$appimage" ]] || { echo "Missing executable AppImage: $appimage" >&2; exit 1; }
      description="$(file -b "$appimage")"
      [[ "$description" == *"ELF 64-bit"* && "$description" == *"x86-64"* ]] || {
        echo "Unexpected AppImage format: $description" >&2
        exit 1
      }
      ;;
    arch)
      arch="$PACKAGES/multicore-${VERSION}-1-x86_64.pkg.tar.zst"
      [[ -f "$arch" ]] || { echo "Missing Arch package: $arch" >&2; exit 1; }
      require_listing_entries "$(tar --zstd -tf "$arch")" ""
      ;;
    *) echo "Unsupported package format: $format" >&2; exit 2 ;;
  esac
done

echo "PASS: Linux package contract ($FORMATS)"
