#!/usr/bin/env bash
# Contract test for scripts/build-mihomo.sh: a patched Mihomo is only ever produced from exactly
# the pinned patch series, and nothing unpinned comes out of it.
#
# usage: scripts/test-build-mihomo.sh [--full --cache <dir>]
#   default  offline checks (tampered, extra and missing patches) that fail before any download
#   --full   also: a patch that no longer applies, and a binary that differs from its pin
#            (downloads/uses the pinned Go and source; reuse a build-mihomo.sh --cache)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FULL=0
CACHE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --full) FULL=1; shift ;;
        --cache) CACHE="$2"; shift 2 ;;
        *) echo "usage: $0 [--full --cache <dir>]" >&2; exit 2 ;;
    esac
done
if [[ "$FULL" == 1 && -z "$CACHE" ]]; then CACHE="$ROOT/target/mihomo-cache"; fi

PY=python3
command -v python3 >/dev/null 2>&1 && python3 -c 'import sys' >/dev/null 2>&1 || PY=python
native() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/test-build-mihomo.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
PASSED=0

# fresh_root: a copy of just what build-mihomo.sh reads.
fresh_root() {
    local root="$WORK/root-$1"
    mkdir -p "$root/scripts" "$root/packaging"
    cp "$ROOT/scripts/build-mihomo.sh" "$root/scripts/"
    cp -R "$ROOT/packaging/mihomo-patches" "$root/packaging/"
    printf '%s' "$root"
}

# edit_manifest <root> <python statement over m>
edit_manifest() {
    "$PY" - "$(native "$1/packaging/mihomo-patches/manifest.json")" "$2" <<'PY'
import json, sys
path, statement = sys.argv[1:]
m = json.load(open(path, encoding="utf-8"))
exec(statement)
json.dump(m, open(path, "w", encoding="utf-8", newline="\n"), indent=2)
PY
}

sha() { "$PY" -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$(native "$1")"; }

# expect_failure <name> <message> <root> [build args...]
expect_failure() {
    local name="$1" message="$2" root="$3"
    shift 3
    local log="$WORK/$name.log" out="$WORK/out-$name"
    if bash "$root/scripts/build-mihomo.sh" --output "$out" "$@" >"$log" 2>&1; then
        cat "$log" >&2
        echo "FAIL: $name: the build succeeded" >&2
        exit 1
    fi
    grep -qF -- "$message" "$log" || {
        cat "$log" >&2
        echo "FAIL: $name: expected an error containing: $message" >&2
        exit 1
    }
    if compgen -G "$out/mihomo-*" >/dev/null; then
        echo "FAIL: $name: a binary was left behind in $out" >&2
        exit 1
    fi
    PASSED=$((PASSED + 1))
    echo "ok: $name"
}

root="$(fresh_root tampered)"
printf ' ' >> "$root/packaging/mihomo-patches/0002-reality-adaptive-mlkem.patch"
expect_failure tampered-patch "patch 0002-reality-adaptive-mlkem.patch: SHA-256" "$root"

root="$(fresh_root extra)"
cp "$root/packaging/mihomo-patches/0001-reality-configurable-client-version.patch" "$root/packaging/mihomo-patches/0004-unreviewed.patch"
expect_failure extra-patch "does not match the manifest patch list" "$root"

root="$(fresh_root missing)"
rm "$root/packaging/mihomo-patches/0003-reality-config-keys.patch"
expect_failure missing-patch "does not match the manifest patch list" "$root"

root="$(fresh_root repinned-elsewhere)"
edit_manifest "$root" 'm["patches"][0]["sha256"] = "0" * 64'
expect_failure manifest-pin-changed "patch 0001-reality-configurable-client-version.patch: SHA-256" "$root"

root="$(fresh_root unknown-target)"
expect_failure unknown-target "target linux-riscv64 is not in the manifest" "$root" linux-riscv64

if [[ "$FULL" == 1 ]]; then
    mkdir -p "$CACHE"
    # A patch whose bytes are pinned but whose context no longer matches the source.
    root="$(fresh_root stale)"
    patch="$root/packaging/mihomo-patches/0001-reality-configurable-client-version.patch"
    "$PY" - "$(native "$patch")" <<'PY'
import sys
path = sys.argv[1]
data = open(path, "rb").read()
old = b" const RealityMaxShortIDLen = 8\n"
assert data.count(old) == 1
open(path, "wb").write(data.replace(old, b" const RealityMaxShortIDLen = 9\n"))
PY
    edit_manifest "$root" "m['patches'][0]['sha256'] = '$(sha "$patch")'"
    expect_failure stale-patch "patch 0001-reality-configurable-client-version.patch does not apply" "$root" --cache "$CACHE" linux-amd64

    # A build whose output differs from the pinned binary never survives.
    root="$(fresh_root wrong-binary-pin)"
    edit_manifest "$root" 'm["targets"]["linux-amd64"]["sha256"] = "0" * 64'
    export MIHOMO_SKIP_TESTS=1
    expect_failure wrong-binary-pin "mihomo-linux-amd64: SHA-256" "$root" --cache "$CACHE" linux-amd64
    unset MIHOMO_SKIP_TESTS
fi

echo "PASS: build-mihomo.sh refuses every unpinned input ($PASSED cases)"
