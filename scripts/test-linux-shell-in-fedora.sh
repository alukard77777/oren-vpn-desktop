#!/usr/bin/env bash
# Compile on the runner, execute the native shell tests in disposable Fedora.
# Avoid Slint's deliberate WSL -> X11 fallback on a WSL-hosted runner.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
root="$PWD"
test_uid="$(id -u)"
[[ "$test_uid" =~ ^[1-9][0-9]*$ ]] || { echo 'Run shell smoke as a non-root user' >&2; exit 1; }
mkdir -p artifacts/linux-shell
cargo test -p multicore-desktop --locked --no-run --message-format=json > artifacts/linux-shell/build.jsonl
test_binary="$(python3 -c '
import json, sys
paths = [v["executable"] for line in sys.stdin if (v := json.loads(line)).get("reason") == "compiler-artifact" and v.get("profile", {}).get("test") and v.get("executable") and v["target"]["name"] == "multicore-desktop"]
assert len(paths) == 1, paths
print(paths[0])
' < artifacts/linux-shell/build.jsonl)"
test_binary="$(realpath -e -- "$test_binary")"
[[ "$test_binary" == "$root/target/"* && -x "$test_binary" ]] || { echo 'Test executable must be under repository target/' >&2; exit 1; }
docker run --rm --init \
  --mount "type=bind,src=$root,dst=/source,readonly" \
  --mount "type=bind,src=$root/artifacts/linux-shell,dst=/evidence" \
  --env "MULTICORE_TEST_BINARY=/source/${test_binary#"$root/"}" \
  --env MULTICORE_SHELL_EVIDENCE=/evidence \
  --env "MULTICORE_SHELL_UID=$test_uid" \
  fedora:44 bash -euo pipefail -c '
    timeout 600s dnf install -y weston xorg-x11-server-Xvfb dbus-daemon gtk3 libX11 libXcursor libXi libXrandr \
      libxkbcommon-x11 libappindicator-gtk3 mesa-dri-drivers mesa-libEGL \
      mesa-libGL dejavu-sans-fonts python3-gobject-base shadow-utils util-linux
    useradd --create-home --uid "$MULTICORE_SHELL_UID" shelltest
    runuser -u shelltest -- bash /source/scripts/test-linux-shell.sh --test-binary "$MULTICORE_TEST_BINARY"
  '
