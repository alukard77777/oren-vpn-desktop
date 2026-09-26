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

check_rpm() (
  # Subshell keeps the temporary extraction and its cleanup local to this format.
  export LC_ALL=C
  local package="$PACKAGES/multicore-${VERSION}-1.x86_64.rpm"
  [[ -f "$package" ]] || { echo "Missing RPM package: $package" >&2; exit 1; }
  local identity
  identity="$(rpm -qp --queryformat '%{NAME} %{VERSION} %{RELEASE} %{ARCH}' "$package")"
  [[ "$identity" == "multicore $VERSION 1 x86_64" ]] || {
    echo "Incorrect RPM identity: $identity" >&2
    exit 1
  }

  local requires dependency
  requires="$(rpm -qp --requires "$package")"
  for dependency in 'gtk3(x86-64)' 'libayatana-appindicator-gtk3(x86-64)' \
      polkit iproute 'libxkbcommon(x86-64)' 'libxkbcommon-x11(x86-64)' 'fontconfig(x86-64)' \
      'libglvnd-glx(x86-64)' 'libglvnd-egl(x86-64)' hicolor-icon-theme \
      'libc.so.6()(64bit)'; do
    grep -Fxq "$dependency" <<<"$requires" || {
      echo "RPM is missing dependency: $dependency" >&2
      exit 1
    }
  done
  grep -Eq '^libc\.so\.6\(GLIBC_[0-9.]+\)\(64bit\)$' <<<"$requires" || {
    echo 'RPM must retain automatic versioned ELF requirements' >&2
    exit 1
  }

  local licenses=(
    usr/share/licenses/multicore/LICENSE-MIT
    usr/share/licenses/multicore/LICENSE-APACHE
    usr/share/licenses/multicore/THIRD_PARTY_NOTICES.md
    usr/share/licenses/multicore/Xray-core-MPL-2.0.txt
    usr/share/licenses/multicore/mihomo-GPL-3.0.txt
  )
  local directories=(
    usr/lib/multicore
    usr/lib/multicore/runtime
    usr/lib/multicore/cores
    usr/share/licenses/multicore
  )
  local expected_inventory inventory relative
  expected_inventory="$(printf '/%s\n' "${executables[@]}" "${metadata[@]}" \
      "${licenses[@]}" "${directories[@]}" usr/bin/multicore | sort)"
  inventory="$(rpm -qpl "$package" | sort)"
  if [[ "$inventory" != "$expected_inventory" ]]; then
    diff -u <(printf '%s\n' "$expected_inventory") <(printf '%s\n' "$inventory") >&2 || true
    echo 'RPM inventory differs from the fixed runtime/desktop/license layout' >&2
    exit 1
  fi

  # Check the stored installation metadata, even when extraction is unprivileged.
  # Exact modes reject setuid/setgid, writable executables, and extra file types.
  local records path mode owner group link capabilities expected_mode
  records="$(rpm -qp --queryformat '[%{FILENAMES}|%{FILEMODES:octal}|%{FILEUSERNAME}|%{FILEGROUPNAME}|%{FILELINKTOS}|%{FILECAPS}\n]' "$package")"
  while IFS='|' read -r path mode owner group link capabilities; do
    expected_mode=100644
    for relative in "${executables[@]}"; do
      [[ "$path" != "/$relative" ]] || expected_mode=100755
    done
    for relative in "${directories[@]}"; do
      [[ "$path" != "/$relative" ]] || expected_mode=40755
    done
    if [[ "$path" == /usr/bin/multicore ]]; then
      expected_mode=120777
      [[ "$link" == ../lib/multicore/multicore ]] || { echo 'Incorrect RPM launcher symlink' >&2; exit 1; }
    else
      [[ -z "$link" ]] || { echo "Unexpected RPM symlink: $path" >&2; exit 1; }
    fi
    [[ "$mode" == "$expected_mode" && "$owner" == root && "$group" == root ]] || {
      echo "Unsafe RPM ownership/mode: $path $owner:$group $mode (expected $expected_mode)" >&2
      exit 1
    }
    [[ -z "$capabilities" || "$capabilities" == '(none)' ]] || {
      echo "Unexpected RPM file capabilities: $path $capabilities" >&2
      exit 1
    }
  done <<<"$records"

  local scriptlets query
  for query in --scripts --triggers --filetriggers; do
    scriptlets="$(rpm -qp "$query" "$package")"
    [[ -z "$scriptlets" ]] || { echo 'RPM must not ship install/removal/trigger scriptlets' >&2; exit 1; }
  done

  local work
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  rpm2cpio "$package" | (cd "$work" && cpio -idm --quiet --no-absolute-filenames --no-preserve-owner)
  [[ -L "$work/usr/bin/multicore" && "$(readlink "$work/usr/bin/multicore")" == ../lib/multicore/multicore ]] || {
    echo 'Incorrect extracted RPM launcher symlink' >&2
    exit 1
  }
  # Check the payload too, not only the package header. Extra services, autostart
  # entries, lib64 paths, or other unlisted payload files must fail this check.
  inventory="$(cd "$work" && find . -mindepth 1 ! -type d -printf '%P\n' | sort)"
  expected_inventory="$(printf '%s\n' "${executables[@]}" "${metadata[@]}" "${licenses[@]}" usr/bin/multicore | sort)"
  [[ "$inventory" == "$expected_inventory" ]] || { echo 'Unexpected extracted RPM payload inventory' >&2; exit 1; }
  for relative in "${executables[@]}" "${metadata[@]}" "${licenses[@]}"; do
    [[ -f "$work/$relative" && ! -L "$work/$relative" ]] || { echo "Missing regular RPM payload file: $relative" >&2; exit 1; }
    cmp "$STAGE/$relative" "$work/$relative" || { echo "RPM changed staged file: $relative" >&2; exit 1; }
  done
  local stage_hash payload_hash
  for relative in "${executables[@]}"; do
    stage_hash="$(sha256sum "$STAGE/$relative" | cut -d' ' -f1)"
    payload_hash="$(sha256sum "$work/$relative" | cut -d' ' -f1)"
    [[ "$stage_hash" == "$payload_hash" ]] || { echo "RPM binary digest mismatch: $relative" >&2; exit 1; }
    printf 'RPM payload SHA256 %s  /%s\n' "$payload_hash" "$relative"
  done
  grep -Fq '<annotate key="org.freedesktop.policykit.exec.path">/usr/lib/multicore/runtime/multicore-core-host</annotate>' \
      "$work/usr/share/polkit-1/actions/io.github.fatyzzz.multicore.core-host.policy" || {
    echo 'Incorrect RPM polkit executable path' >&2
    exit 1
  }
)

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
    rpm)
      check_rpm
      ;;
    *) echo "Unsupported package format: $format" >&2; exit 2 ;;
  esac
done

echo "PASS: Linux package contract ($FORMATS)"
