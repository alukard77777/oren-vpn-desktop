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
  usr/lib/systemd/system/multicore.service
  usr/share/metainfo/io.github.fatyzzz.multicore.metainfo.xml
)
SERVICE_UNIT=usr/lib/systemd/system/multicore.service
METAINFO=usr/share/metainfo/io.github.fatyzzz.multicore.metainfo.xml

for relative in "${executables[@]}"; do
  path="$STAGE/$relative"
  [[ -f "$path" && -x "$path" ]] || { echo "Missing executable: $relative" >&2; exit 1; }
  description="$(file -b "$path")"
  [[ "$description" == *"ELF 64-bit"* && "$description" == *"x86-64"* ]] || {
    echo "Unexpected executable format for $relative: $description" >&2
    exit 1
  }
done

# The staged Mihomo is the pinned patched build (packaging/mihomo-patches), never a stock core.
mihomo_pin="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["components"]["mihomo"]["build"]["executableSha256"])' "$ROOT/packaging/linux/versions.json")"
echo "$mihomo_pin  $STAGE/usr/lib/multicore/cores/mihomo" | sha256sum -c --quiet || {
  echo "Staged Mihomo is not the pinned patched build" >&2
  exit 1
}

for relative in "${metadata[@]}"; do
  [[ -f "$STAGE/$relative" && -r "$STAGE/$relative" ]] || { echo "Missing metadata: $relative" >&2; exit 1; }
done

# The vendored dashboard is optional in the manifest (see packaging/linux/versions.json), but
# a real production manifest always carries it, so a staged tree from an actual release build
# is expected to have it. Skip these checks only for a stage that genuinely has no dashboard/
# (e.g. an older or deliberately dashboard-less manifest), rather than requiring it unconditionally.
dashboard_dir="$STAGE/usr/lib/multicore/dashboard"
if [[ -d "$dashboard_dir" ]]; then
  [[ -f "$dashboard_dir/index.html" ]] || { echo "Dashboard is staged but missing index.html" >&2; exit 1; }
  grep -q 'Content-Security-Policy' "$dashboard_dir/index.html" || {
    echo 'Staged dashboard index.html must carry a Content-Security-Policy meta tag' >&2
    exit 1
  }
  grep -q "default-src 'self'" "$dashboard_dir/index.html" || {
    echo "Staged dashboard CSP must lock default-src down to 'self'" >&2
    exit 1
  }
  grep -Fq '<script>' "$dashboard_dir/index.html" && {
    echo 'Staged dashboard index.html must not retain an inline <script> block' >&2
    exit 1
  }
  [[ -f "$dashboard_dir/theme-favicon.js" ]] || {
    echo 'Staged dashboard is missing the externalized theme-favicon.js' >&2
    exit 1
  }
  cmp -s "$ROOT/packaging/dashboard/multicore-readonly.js" "$dashboard_dir/multicore-readonly.js" || {
    echo 'Staged dashboard must carry packaging/dashboard/multicore-readonly.js unchanged' >&2
    exit 1
  }
  grep -Fq '<script src="./multicore-readonly.js"></script>' "$dashboard_dir/index.html" || {
    echo 'Staged dashboard index.html must load the read-only script' >&2
    exit 1
  }
  find "$dashboard_dir" -type l | grep -q . && {
    echo 'Staged dashboard must not contain symlinks' >&2
    exit 1
  }
  license_name="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["components"]["dashboard"]["licenseFile"]["name"])' "$ROOT/packaging/linux/versions.json")"
  [[ -f "$STAGE/usr/share/licenses/multicore/$license_name" ]] || {
    echo "Missing staged dashboard license: usr/share/licenses/multicore/$license_name" >&2
    exit 1
  }
fi

desktop_file="$STAGE/usr/share/applications/multicore.desktop"
grep -qx 'Exec=multicore %u' "$desktop_file" || {
  echo 'Linux desktop entry must pass one protocol URL to MultiCore' >&2
  exit 1
}
grep -qx 'MimeType=x-scheme-handler/multicore;' "$desktop_file" || {
  echo 'Linux desktop entry must advertise the multicore URL scheme' >&2
  exit 1
}
# English base strings with Russian translations (freedesktop Key[ru]); the name stays MULTICORE.
grep -qx 'Name=MULTICORE' "$desktop_file" && ! grep -q '^Name\[' "$desktop_file" || {
  echo 'Linux desktop entry must be named MULTICORE in every language' >&2
  exit 1
}
for key in GenericName Comment Keywords; do
  LC_ALL=C grep -Eq "^$key=[ -~]+\$" "$desktop_file" && grep -q "^$key\[ru\]=" "$desktop_file" || {
    echo "Linux desktop entry must carry an English $key and its Russian translation" >&2
    exit 1
  }
done
policy_file="$STAGE/usr/share/polkit-1/actions/io.github.fatyzzz.multicore.core-host.policy"
for element in description message; do
  LC_ALL=C grep -Eq "^    <$element>[ -~]+</$element>\$" "$policy_file" && grep -q "<$element xml:lang=\"ru\">" "$policy_file" || {
    echo "polkit action must carry an English <$element> and its Russian translation" >&2
    exit 1
  }
done
# AppStream: the component launches this desktop entry and names this release.
grep -Fq '<launchable type="desktop-id">multicore.desktop</launchable>' "$STAGE/$METAINFO" &&
  grep -Eq "<release version=\"$VERSION\" date=\"[0-9]{4}-[0-9]{2}-[0-9]{2}\"/>" "$STAGE/$METAINFO" &&
  ! grep -q '@[A-Z]*@' "$STAGE/$METAINFO" || {
  echo 'AppStream metadata must launch multicore.desktop and name this release' >&2
  exit 1
}
grep -Fxq 'exec "$HERE/usr/lib/multicore/multicore" "$@"' "$ROOT/packaging/linux/appimage/AppRun" || {
  echo 'AppImage launcher must forward the protocol URL unchanged' >&2
  exit 1
}

cmp -s "$ROOT/packaging/linux/multicore.service" "$STAGE/$SERVICE_UNIT" || {
  echo 'Staged systemd unit must be packaging/linux/multicore.service unchanged' >&2
  exit 1
}
grep -Fxq 'ExecStart=/usr/lib/multicore/runtime/multicore-core-host --service' "$STAGE/$SERVICE_UNIT" || {
  echo 'Staged systemd unit must start the packaged core host in service mode' >&2
  exit 1
}

# Every package manager format must enable the service on install and disable it on removal.
# Scriptlets are plain text, so these fragments are checked verbatim in each package.
require_service_scriptlets() {
  local label="$1" install_text="$2" removal_text="$3"
  grep -Fq 'systemctl enable multicore.service' <<<"$install_text" || {
    echo "$label does not enable the service on install" >&2
    exit 1
  }
  grep -Fq 'systemctl restart multicore.service' <<<"$install_text" || {
    echo "$label does not start the service on install" >&2
    exit 1
  }
  # Enable only when the service was never installed, so an administrator's disable sticks.
  grep -Fq 'if [ ! -d /var/lib/multicore ]; then' <<<"$install_text" || {
    echo "$label re-enables the service on every upgrade" >&2
    exit 1
  }
  grep -Fq 'systemctl is-enabled --quiet multicore.service' <<<"$install_text" || {
    echo "$label starts the service even when it is disabled" >&2
    exit 1
  }
  grep -Fq 'systemctl disable --now multicore.service' <<<"$removal_text" || {
    echo "$label does not stop and disable the service on removal" >&2
    exit 1
  }
}

# Runs the package's own scriptlets through install, upgrade, administrator disable, removal
# and reinstall against a fake systemctl and a relocated /var/lib/multicore and
# /run/systemd/system. `$2` is a function taking one phase: install (fresh), upgrade, remove.
# The fake `restart` creates the state directory, as systemd's StateDirectory= does.
SIM=""
sim_run() {
  # sim_run <shell> <scriptlet text> [args...]
  local shell="$1" text="$2"
  shift 2
  local file
  file="$(mktemp "$SIM/scriptlet.XXXXXX")"
  sed -e "s#/var/lib/multicore#$SIM/var/lib/multicore#g" \
      -e "s#/run/systemd/system#$SIM/run/systemd/system#g" <<<"$text" > "$file"
  PATH="$SIM/bin:$PATH" SIM_STATE="$SIM/state" SIM_VAR="$SIM/var/lib/multicore" "$shell" "$file" "$@"
}

expect_service() {
  local label="$1" step="$2" enabled="$3" running="$4" actual_enabled=no actual_running=no
  [[ -e "$SIM/state/enabled" ]] && actual_enabled=yes
  [[ -e "$SIM/state/running" ]] && actual_running=yes
  [[ "$actual_enabled/$actual_running" == "$enabled/$running" ]] || {
    echo "$label, $step: service enabled/running is $actual_enabled/$actual_running, expected $enabled/$running" >&2
    exit 1
  }
}

simulate_service_lifecycle() {
  local label="$1" runner="$2"
  SIM="$(mktemp -d)"
  mkdir -p "$SIM/bin" "$SIM/state" "$SIM/run/systemd/system"
  cat > "$SIM/bin/systemctl" <<'FAKE'
#!/bin/sh
case "$1" in
  daemon-reload) ;;
  enable) touch "$SIM_STATE/enabled" ;;
  disable) rm -f "$SIM_STATE/enabled"; if [ "$2" = "--now" ]; then rm -f "$SIM_STATE/running"; fi ;;
  restart) touch "$SIM_STATE/running"; mkdir -p "$SIM_VAR" ;;
  stop) rm -f "$SIM_STATE/running" ;;
  is-enabled) [ -e "$SIM_STATE/enabled" ] ;;
  *) echo "unexpected systemctl $*" >&2; exit 1 ;;
esac
FAKE
  chmod 0755 "$SIM/bin/systemctl"
  "$runner" install
  expect_service "$label" 'fresh install' yes yes
  "$runner" upgrade
  expect_service "$label" 'upgrade' yes yes
  PATH="$SIM/bin:$PATH" SIM_STATE="$SIM/state" SIM_VAR="$SIM/var/lib/multicore" \
    systemctl disable --now multicore.service
  "$runner" upgrade
  expect_service "$label" 'upgrade after an administrator disabled it' no no
  "$runner" remove
  expect_service "$label" 'removal' no no
  [[ ! -e "$SIM/var/lib/multicore" ]] || { echo "$label: removal keeps /var/lib/multicore" >&2; exit 1; }
  "$runner" install
  expect_service "$label" 'reinstall after removal' yes yes
  "$runner" remove
  "$runner" upgrade
  expect_service "$label" 'upgrade from a version without the service' yes yes
  rm -rf "$SIM"
  echo "$label: service lifecycle (install, upgrade, admin disable, remove, reinstall) OK"
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
      'libglvnd-glx(x86-64)' 'libglvnd-egl(x86-64)' hicolor-icon-theme systemd \
      'libc.so.6()(64bit)' 'libfreetype.so.6()(64bit)' 'libstdc++.so.6()(64bit)'; do
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
  # Dashboard filenames are content-hashed by its upstream build and are not otherwise pinned
  # (see the matching comment in package-linux-release.sh), so they are derived here from the
  # staged tree rather than hardcoded, the same way scripts/package-windows-release.ps1 derives
  # its own exact dashboard inventory from the archive it just extracted.
  local dashboard_files=() dashboard_relative
  if [[ -d "$dashboard_dir" ]]; then
    directories+=(usr/lib/multicore/dashboard)
    while IFS= read -r dashboard_relative; do
      directories+=("usr/lib/multicore/dashboard/$dashboard_relative")
    done < <(cd "$dashboard_dir" && find . -mindepth 1 -type d -printf '%P\n' | sort)
    while IFS= read -r dashboard_relative; do
      dashboard_files+=("usr/lib/multicore/dashboard/$dashboard_relative")
    done < <(cd "$dashboard_dir" && find . -mindepth 1 -type f -printf '%P\n' | sort)
    licenses+=("usr/share/licenses/multicore/$license_name")
  fi
  local expected_inventory inventory relative
  expected_inventory="$(printf '/%s\n' "${executables[@]}" "${metadata[@]}" \
      "${licenses[@]}" "${directories[@]}" "${dashboard_files[@]}" usr/bin/multicore | sort)"
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

  # Install/removal scriptlets manage only the always-ready service; triggers stay forbidden.
  local scriptlets query
  scriptlets="$(rpm -qp --scripts "$package")"
  require_service_scriptlets RPM "$scriptlets" "$scriptlets"
  grep -Fq 'rm -rf /var/lib/multicore' <<<"$scriptlets" || {
    echo 'RPM final removal keeps /var/lib/multicore' >&2
    exit 1
  }
  for query in --triggers --filetriggers; do
    scriptlets="$(rpm -qp "$query" "$package")"
    [[ -z "$scriptlets" ]] || { echo 'RPM must not ship trigger scriptlets' >&2; exit 1; }
  done
  RPM_POSTIN="$(rpm -qp --queryformat '%{POSTIN}' "$package")"
  RPM_PREUN="$(rpm -qp --queryformat '%{PREUN}' "$package")"
  RPM_POSTUN="$(rpm -qp --queryformat '%{POSTUN}' "$package")"
  rpm_phase() {
    case "$1" in
      install) sim_run sh "$RPM_POSTIN" 1 ;;
      upgrade) sim_run sh "$RPM_POSTIN" 2; sim_run sh "$RPM_PREUN" 1; sim_run sh "$RPM_POSTUN" 1 ;;
      remove) sim_run sh "$RPM_PREUN" 0; sim_run sh "$RPM_POSTUN" 0 ;;
    esac
  }
  simulate_service_lifecycle RPM rpm_phase

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
  expected_inventory="$(printf '%s\n' "${executables[@]}" "${metadata[@]}" "${licenses[@]}" "${dashboard_files[@]}" usr/bin/multicore | sort)"
  [[ "$inventory" == "$expected_inventory" ]] || { echo 'Unexpected extracted RPM payload inventory' >&2; exit 1; }
  for relative in "${executables[@]}" "${metadata[@]}" "${licenses[@]}" "${dashboard_files[@]}"; do
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
      deb_control="$(mktemp -d)"
      dpkg-deb -e "$deb" "$deb_control/DEBIAN"
      for scriptlet in postinst prerm postrm; do
        [[ -x "$deb_control/DEBIAN/$scriptlet" ]] || { echo "Debian package lacks an executable $scriptlet" >&2; exit 1; }
      done
      require_service_scriptlets 'Debian package' "$(cat "$deb_control/DEBIAN/postinst")" "$(cat "$deb_control/DEBIAN/prerm")"
      grep -Fq 'rm -rf /var/lib/multicore' "$deb_control/DEBIAN/postrm" || {
        echo 'Debian purge keeps /var/lib/multicore' >&2
        exit 1
      }
      DEB_POSTINST="$(cat "$deb_control/DEBIAN/postinst")"
      DEB_PRERM="$(cat "$deb_control/DEBIAN/prerm")"
      DEB_POSTRM="$(cat "$deb_control/DEBIAN/postrm")"
      deb_phase() {
        case "$1" in
          install) sim_run sh "$DEB_POSTINST" configure ;;
          upgrade)
            sim_run sh "$DEB_PRERM" upgrade "$VERSION"
            sim_run sh "$DEB_POSTRM" upgrade "$VERSION"
            sim_run sh "$DEB_POSTINST" configure "$VERSION"
            ;;
          remove) sim_run sh "$DEB_PRERM" remove; sim_run sh "$DEB_POSTRM" remove ;;
        esac
      }
      simulate_service_lifecycle 'Debian package' deb_phase
      rm -rf "$deb_control"
      ;;
    appimage)
      appimage="$PACKAGES/MultiCore-${VERSION}-x86_64.AppImage"
      [[ -f "$appimage" && -x "$appimage" ]] || { echo "Missing executable AppImage: $appimage" >&2; exit 1; }
      description="$(file -b "$appimage")"
      [[ "$description" == *"ELF 64-bit"* && "$description" == *"x86-64"* ]] || {
        echo "Unexpected AppImage format: $description" >&2
        exit 1
      }
      # The AppImage keeps the on-demand pkexec host: no systemd unit inside.
      appimage_root="$(mktemp -d)"
      appimage_path="$(cd "$(dirname "$appimage")" && pwd)/$(basename "$appimage")"
      (cd "$appimage_root" && "$appimage_path" --appimage-extract >/dev/null)
      [[ -x "$appimage_root/squashfs-root/usr/lib/multicore/runtime/multicore-core-host" ]] || {
        echo 'AppImage extraction did not produce the core host' >&2
        exit 1
      }
      [[ ! -e "$appimage_root/squashfs-root/$SERVICE_UNIT" ]] || {
        echo 'AppImage must not carry the systemd service unit' >&2
        exit 1
      }
      [[ -f "$appimage_root/squashfs-root/$METAINFO" ]] || {
        echo 'AppImage must carry the AppStream metadata' >&2
        exit 1
      }
      # Built with an update channel: gh-releases-zsync update information and its .zsync
      # file, which names this versioned image.
      if [[ -e "$appimage.zsync" ]]; then
        update_info="$("$appimage_path" --appimage-updateinformation)"
        update_pattern='^gh-releases-zsync[|][A-Za-z0-9-]+[|][A-Za-z0-9._-]+[|]latest[|]MultiCore-[*]x86_64[.]AppImage[.]zsync$'
        [[ "$update_info" =~ $update_pattern ]] || {
          echo "Unexpected AppImage update information: $update_info" >&2
          exit 1
        }
        grep -aqx "Filename: MultiCore-${VERSION}-x86_64.AppImage" "$appimage.zsync" || {
          echo 'The .zsync file must describe the versioned AppImage' >&2
          exit 1
        }
      fi
      rm -rf "$appimage_root"
      ;;
    arch)
      arch="$PACKAGES/multicore-${VERSION}-1-x86_64.pkg.tar.zst"
      [[ -f "$arch" ]] || { echo "Missing Arch package: $arch" >&2; exit 1; }
      require_listing_entries "$(tar --zstd -tf "$arch")" ""
      arch_install="$(tar --zstd -xOf "$arch" .INSTALL)" || { echo 'Arch package lacks its .INSTALL hooks' >&2; exit 1; }
      require_service_scriptlets 'Arch package' "$arch_install" "$arch_install"
      grep -Fq 'rm -rf /var/lib/multicore' <<<"$arch_install" || {
        echo 'Arch removal keeps /var/lib/multicore' >&2
        exit 1
      }
      arch_phase() {
        case "$1" in
          install) sim_run bash "$arch_install"$'\n'post_install ;;
          upgrade) sim_run bash "$arch_install"$'\n'post_upgrade ;;
          remove) sim_run bash "$arch_install"$'\n'pre_remove$'\n'post_remove ;;
        esac
      }
      simulate_service_lifecycle 'Arch package' arch_phase
      ;;
    rpm)
      check_rpm
      ;;
    *) echo "Unsupported package format: $format" >&2; exit 2 ;;
  esac
done

echo "PASS: Linux package contract ($FORMATS)"
