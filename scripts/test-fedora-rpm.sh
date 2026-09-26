#!/usr/bin/env bash
# Destructive package lifecycle test. Run only inside disposable Fedora CI containers.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
[[ "${MULTICORE_DISPOSABLE_RPM_TEST:-}" == 1 ]] ||
  fail 'Refusing mutation: set MULTICORE_DISPOSABLE_RPM_TEST=1 only in a disposable Fedora container'

usage() { echo 'Usage: MULTICORE_DISPOSABLE_RPM_TEST=1 test-fedora-rpm.sh --package <local.rpm>' >&2; exit 2; }
[[ $# == 2 && "$1" == --package && -n "$2" ]] || usage
PACKAGE="$2"
[[ "$EUID" == 0 ]] || fail 'The disposable container smoke must run as root'
[[ -f /.dockerenv || -f /run/.containerenv ]] || fail 'A Docker/Podman container marker is required'
[[ -r /etc/os-release ]] || fail 'Missing /etc/os-release'
# shellcheck disable=SC1091
source /etc/os-release
[[ "${ID:-}" == fedora && "${VERSION_ID:-}" =~ ^(43|44)$ ]] || fail 'Only Fedora 43/44 containers are supported'
[[ "$(uname -m)" == x86_64 ]] || fail 'Only x86_64 is supported'
[[ -f "$PACKAGE" && "$PACKAGE" == *.rpm ]] || fail 'The package must be an existing local RPM'
PACKAGE="$(realpath -- "$PACKAGE")"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE="$ROOT/scripts/fixtures/fedora_rpm_smoke.py"
[[ -r "$FIXTURE" ]] || fail 'Missing Fedora smoke fixture'

# CI provisions only smoke tooling first. dnf below must resolve all application dependencies.
for command in dnf rpm python3 useradd userdel runuser timeout getcap file readelf ldd \
  desktop-file-validate gio xdg-mime dbus-run-session Xvfb xauth xdotool; do
  command -v "$command" >/dev/null || fail "Missing smoke tool: $command"
done
VERSION="$(sed -n '/^version = / { s/version = "\([^"]*\)"/\1/p; q; }' "$ROOT/apps/multicore-desktop/Cargo.toml")"
[[ -n "$VERSION" ]] || fail 'Could not read application version'
IDENTITY="multicore $VERSION 1 x86_64"
[[ "$(rpm -qp --queryformat '%{NAME} %{VERSION} %{RELEASE} %{ARCH}' "$PACKAGE")" == "$IDENTITY" ]] ||
  fail 'Unexpected RPM name, version, release or architecture'
if rpm -q multicore >/dev/null 2>&1; then
  fail 'MultiCore is already installed; use a fresh disposable container'
fi

OUTPUT="${MULTICORE_RPM_SMOKE_OUTPUT:-$ROOT/artifacts/fedora-rpm-smoke}"
install -d -m 0755 "$OUTPUT"
OUTPUT="$(realpath -- "$OUTPUT")"
TEST_ROOT="$(mktemp -d /tmp/multicore-rpm-smoke.XXXXXXXX)"
TEST_HOME="$TEST_ROOT/home"
TEST_USER="mc-rpm-${TEST_ROOT##*.}"
TEST_UID=''
USER_CREATED=0
cleanup() {
  local status=$?
  trap - EXIT
  if [[ -d "$TEST_ROOT/evidence" ]]; then
    cp -a "$TEST_ROOT/evidence/." "$OUTPUT/" || status=1
  fi
  if [[ "$USER_CREATED" == 1 ]]; then
    # The fixture checks this freshly allocated UID plus the unique HOME, then
    # records /proc start times before sending a signal to each individual PID.
    python3 "$FIXTURE" --cleanup-user "$TEST_UID" --test-home "$TEST_HOME" || status=1
    userdel "$TEST_USER" >"$OUTPUT/user-cleanup.log" 2>&1 || status=1
  fi
  # Keep the isolated home and logs for inspection; the disposable container owns their lifetime.
  if [[ "$status" != 0 ]]; then
    echo "FAIL: Fedora RPM smoke; retained logs: $OUTPUT; isolated data: $TEST_ROOT" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo "Installing $IDENTITY on Fedora $VERSION_ID with dependency resolution"
timeout --kill-after=15s 300s dnf -y --setopt=localpkg_gpgcheck=False install "$PACKAGE" >"$OUTPUT/install.log" 2>&1
[[ "$(rpm -q --queryformat '%{NAME} %{VERSION} %{RELEASE} %{ARCH}' multicore)" == "$IDENTITY" ]] || fail 'Installed RPM identity differs'
rpm -V multicore >"$OUTPUT/rpm-verify-install.log" 2>&1
python3 "$FIXTURE" --verify-install
desktop-file-validate /usr/share/applications/multicore.desktop

executables=(
  /usr/lib/multicore/multicore
  /usr/lib/multicore/runtime/multicore-daemon
  /usr/lib/multicore/runtime/multicore-core-host
  /usr/lib/multicore/cores/mihomo
  /usr/lib/multicore/cores/xray
)
for binary in "${executables[@]}"; do
  description="$(file -b "$binary")"
  [[ "$description" == *'ELF 64-bit'* && "$description" == *'x86-64'* ]] || fail 'Unexpected installed executable architecture'
  # A static Go core legitimately has no dynamic loader. Dynamic ELF files must
  # pass ldd and resolve every needed library; do not ignore arbitrary ldd errors.
  dynamic="$(readelf -d "$binary")"
  if [[ "$dynamic" == *'(NEEDED)'* ]]; then
    ldd "$binary" >>"$OUTPUT/library-resolution.log" 2>&1 || fail 'Dynamic executable failed ldd'
  else
    [[ "$description" == *'statically linked'* || "$description" == *'static-pie linked'* ]] ||
      fail 'ELF has no dependencies but was not identified as static'
    printf 'Verified static ELF: %s\n' "$binary" >>"$OUTPUT/library-resolution.log"
  fi
done
if grep -q 'not found' "$OUTPUT/library-resolution.log"; then fail 'An installed ELF dependency is unresolved'; fi

useradd --no-create-home --user-group --home-dir "$TEST_HOME" --shell /bin/bash "$TEST_USER"
USER_CREATED=1
TEST_UID="$(id -u "$TEST_USER")"
chmod 0755 "$TEST_ROOT"
install -d -m 0700 -o "$TEST_USER" -g "$TEST_USER" "$TEST_HOME" \
  "$TEST_HOME/.local" "$TEST_HOME/.local/share" "$TEST_HOME/.local/share/multicore" \
  "$TEST_HOME/.config" "$TEST_HOME/.cache" "$TEST_ROOT/runtime" "$TEST_ROOT/evidence"
printf 'synthetic Fedora RPM user data; preserve during package maintenance\n' >"$TEST_HOME/.local/share/multicore/rpm-smoke-sentinel"
chown "$TEST_USER:$TEST_USER" "$TEST_HOME/.local/share/multicore/rpm-smoke-sentinel"
SENTINEL_BEFORE="$(sha256sum "$TEST_HOME/.local/share/multicore/rpm-smoke-sentinel" | cut -d ' ' -f 1)"
[[ "$(stat -c '%u:%a' "$TEST_ROOT/runtime")" == "$TEST_UID:700" ]] || fail 'XDG_RUNTIME_DIR is not private'

# Clear inherited credentials, proxy settings and application override variables.
# The installed desktop owns its normal packaged daemon; no test bypass is used.
timeout --signal=TERM --kill-after=15s 150s runuser -u "$TEST_USER" -- env -i \
  HOME="$TEST_HOME" USER="$TEST_USER" LOGNAME="$TEST_USER" PATH=/usr/bin:/bin LANG=C.UTF-8 \
  XDG_DATA_HOME="$TEST_HOME/.local/share" XDG_CONFIG_HOME="$TEST_HOME/.config" \
  XDG_CACHE_HOME="$TEST_HOME/.cache" XDG_RUNTIME_DIR="$TEST_ROOT/runtime" XDG_DATA_DIRS=/usr/local/share:/usr/share \
  LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe SLINT_BACKEND=winit SLINT_SCALE_FACTOR=1 \
  dbus-run-session -- python3 "$FIXTURE" --session --evidence "$TEST_ROOT/evidence" >"$OUTPUT/session.log" 2>&1
cp -a "$TEST_ROOT/evidence/." "$OUTPUT/"
# Prove all owned app/daemon processes have stopped before package maintenance.
python3 "$FIXTURE" --cleanup-user "$TEST_UID" --test-home "$TEST_HOME"

timeout --kill-after=15s 300s dnf -y --setopt=localpkg_gpgcheck=False reinstall "$PACKAGE" >"$OUTPUT/reinstall.log" 2>&1
rpm -V multicore >"$OUTPUT/rpm-verify-reinstall.log" 2>&1
python3 "$FIXTURE" --verify-install
[[ "$(sha256sum "$TEST_HOME/.local/share/multicore/rpm-smoke-sentinel" | cut -d ' ' -f 1)" == "$SENTINEL_BEFORE" ]] || fail 'Reinstall changed user data'
timeout --kill-after=15s 300s dnf -y remove multicore >"$OUTPUT/remove.log" 2>&1
if rpm -q multicore >/dev/null 2>&1; then fail 'RPM remains installed after removal'; fi
for path in /usr/bin/multicore /usr/lib/multicore /usr/share/applications/multicore.desktop \
  /usr/share/icons/hicolor/scalable/apps/multicore.svg \
  /usr/share/polkit-1/actions/io.github.fatyzzz.multicore.core-host.policy /usr/share/licenses/multicore; do
  [[ ! -e "$path" && ! -L "$path" ]] || fail "Package-owned path remains after removal: $path"
done
[[ "$(sha256sum "$TEST_HOME/.local/share/multicore/rpm-smoke-sentinel" | cut -d ' ' -f 1)" == "$SENTINEL_BEFORE" ]] || fail 'Removal changed user data'
echo "PASS: Fedora $VERSION_ID RPM install, metadata/linking, nonroot desktop URI consent, reinstall and removal; logs: $OUTPUT"
echo 'Not exercised: Connect, live VPN routing, graphical polkit authorization, SELinux enforcing desktop or Wayland.'
