#!/usr/bin/env bash
# A private Wayland compositor and session bus; never attaches to the user's desktop.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
[[ $# == 2 && "$1" == --test-binary && -x "$2" ]] || { echo 'Usage: test-linux-shell.sh --test-binary /absolute/test-executable' >&2; exit 1; }
test_binary="$2"
for command in weston dbus-run-session timeout python3; do command -v "$command" >/dev/null; done
evidence="${MULTICORE_SHELL_EVIDENCE:?private evidence directory required}"
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
unset DISPLAY DBUS_SESSION_BUS_ADDRESS SLINT_BACKEND MULTICORE_SMOKE_EXIT_ON_CLOSE
weston --backend=headless-backend.so --renderer=pixman --socket="$WAYLAND_DISPLAY" \
  --width=1280 --height=900 --idle-time=0 --no-config --log="$evidence/weston.log" &
compositor_pid=$!
for ((attempt=0; attempt<100; attempt++)); do
  [[ -S "$runtime/$WAYLAND_DISPLAY" ]] && break
  kill -0 "$compositor_pid" 2>/dev/null || { cat "$evidence/weston.log"; exit 1; }
  sleep 0.1
done
[[ -S "$runtime/$WAYLAND_DISPLAY" ]] || { cat "$evidence/weston.log"; exit 1; }
result=0
check_test_count() {
  grep -Fqx 'running 1 test' "$1" || { echo "Expected exactly one native test in $1" >&2; result=1; }
}
for test in wayland_title_buttons_hide_and_restore_without_exiting wayland_close_without_tray_exits wayland_minimize_without_tray_keeps_recoverable_window; do
  if ! WAYLAND_DEBUG=client timeout 30s dbus-run-session -- "$test_binary" \
    "linux_shell_tests::$test" --ignored --exact --nocapture --test-threads=1 2>&1 | tee "$evidence/$test.log"; then
    result=1
  fi
  check_test_count "$evidence/$test.log"
done
grep -Eq 'xdg_toplevel@[0-9]+\.set_minimized\(' "$evidence/wayland_minimize_without_tray_keeps_recoverable_window.log" || {
  echo 'The compositor did not receive a native minimize request' >&2; result=1;
}
for host in absent false true; do
  if ! timeout 30s dbus-run-session -- python3 scripts/fixtures/status_notifier_host.py --host "$host" -- \
    "$test_binary" tray::linux::tests::notifier_availability_matches_registered_host \
    --ignored --exact --nocapture --test-threads=1 2>&1 | tee "$evidence/tray-host-$host.log"; then
    result=1
  fi
  check_test_count "$evidence/tray-host-$host.log"
done
if ! timeout 30s dbus-run-session -- python3 scripts/fixtures/status_notifier_host.py --host true --exercise-menu -- \
  "$test_binary" linux_shell_tests::wayland_native_tray_menu_restores_and_exits \
  --ignored --exact --nocapture --test-threads=1 2>&1 | tee "$evidence/tray-menu.log"; then
  result=1
fi
check_test_count "$evidence/tray-menu.log"
exit "$result"
