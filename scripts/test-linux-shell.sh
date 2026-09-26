#!/usr/bin/env bash
# A private Wayland compositor and session bus; never attaches to the user's desktop.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
for command in weston dbus-run-session cargo timeout; do command -v "$command" >/dev/null; done
evidence="$PWD/artifacts/linux-shell"
mkdir -p "$evidence"
runtime="$(mktemp -d -t multicore-wayland.XXXXXXXX)"
compositor_pid=''
cleanup() {
  if [[ -n "$compositor_pid" ]]; then
    kill "$compositor_pid" 2>/dev/null || true
    wait "$compositor_pid" 2>/dev/null || true
  fi
  # mktemp creates this exact private tree; refuse broad/unexpected targets.
  if [[ "$runtime" == /tmp/multicore-wayland.* && -d "$runtime" && ! -L "$runtime" ]]; then
    rm -rf -- "$runtime"
  fi
}
trap cleanup EXIT
export XDG_RUNTIME_DIR="$runtime" WAYLAND_DISPLAY=multicore-test
export GDK_BACKEND=wayland LIBGL_ALWAYS_SOFTWARE=1 MULTICORE_ISOLATED_SHELL_TEST=1
unset DISPLAY DBUS_SESSION_BUS_ADDRESS SLINT_BACKEND MULTICORE_SMOKE_CLOSE
weston --backend=headless-backend.so --renderer=pixman --socket="$WAYLAND_DISPLAY" \
  --width=1280 --height=900 --idle-time=0 --no-config --log="$evidence/weston.log" &
compositor_pid=$!
for ((attempt=0; attempt<100; attempt++)); do
  [[ -S "$runtime/$WAYLAND_DISPLAY" ]] && break
  kill -0 "$compositor_pid" 2>/dev/null || { cat "$evidence/weston.log"; exit 1; }
  sleep 0.1
done
[[ -S "$runtime/$WAYLAND_DISPLAY" ]] || { cat "$evidence/weston.log"; exit 1; }
for test in wayland_title_buttons_hide_and_restore_without_exiting wayland_close_without_tray_exits; do
  timeout 90s dbus-run-session -- cargo test -p multicore-desktop --locked \
    "linux_shell_tests::$test" -- --ignored --exact --nocapture --test-threads=1 2>&1 | tee "$evidence/$test.log"
done
