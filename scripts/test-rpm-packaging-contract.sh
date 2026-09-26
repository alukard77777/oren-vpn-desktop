#!/usr/bin/env bash
# Local dispatch/structure regression only. Real RPM validation is performed by
# test-linux-package-contract.sh --formats rpm on Fedora, using the real stage.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/stage/usr/lib/multicore" "$WORK/bin"
printf '#!/bin/sh\n# Dispatch fixture, not an ELF executable.\nexit 1\n' > "$WORK/stage/usr/lib/multicore/multicore"
chmod 0755 "$WORK/stage/usr/lib/multicore/multicore"

# Stop at the actual rpmbuild invocation: never manufacture an RPM or fake a
# successful build. This fixture remains useful on hosts without Linux/RPM.
cat > "$WORK/bin/rpmbuild" <<'STUB'
#!/usr/bin/env bash
printf 'RPMBUILD_DISPATCH_ONLY\n'
printf '<%s>\n' "$@"
exit 73
STUB
chmod 0755 "$WORK/bin/rpmbuild"
status=0
PATH="$WORK/bin:$PATH" bash "$ROOT/scripts/build-linux-packages.sh" \
    --stage "$WORK/stage" --output "$WORK/output" --formats rpm > "$WORK/output.log" 2>&1 || status=$?
if [[ "$status" != 73 ]] || ! grep -qx 'RPMBUILD_DISPATCH_ONLY' "$WORK/output.log"; then
    cat "$WORK/output.log" >&2
    echo "FAIL: --formats rpm must dispatch to rpmbuild and propagate its failure" >&2
    exit 1
fi
grep -qx '<-bb>' "$WORK/output.log"
grep -qx '<x86_64>' "$WORK/output.log"
grep -q '^<_topdir ' "$WORK/output.log"

spec="$ROOT/packaging/linux/rpm/multicore.spec"
[[ -f "$spec" ]] || { echo 'FAIL: missing RPM spec' >&2; exit 1; }
grep -Eq '^Release:[[:space:]]+1$' "$spec"
grep -Eq '^BuildArch:[[:space:]]+x86_64$' "$spec"
grep -Fxq '/usr/lib/multicore/runtime/multicore-core-host' "$spec"
grep -Eq '^Requires:[[:space:]]+libxkbcommon-x11%\{\?_isa\}$' "$spec" || {
    echo 'FAIL: RPM must require the X11 XKB library loaded dynamically by the desktop backend' >&2
    exit 1
}
for macro in debug_package __brp_strip __brp_strip_static_archive __brp_strip_comment_note __brp_strip_lto __brp_llvm_compile_lto_elf; do
    grep -Eq "^%global $macro[[:space:]]+%\{nil\}$" "$spec"
done
grep -qx '%global _build_id_links none' "$spec"
if grep -Eq '^%(pre|preun|pretrans|post|postun|posttrans|trigger[a-z]*|filetrigger[a-z]*|transfiletrigger[a-z]*)([[:space:]]|$)|^AutoReq(Prov)?:[[:space:]]*(no|0)|^%global (__os_install_post|__spec_install_post)' "$spec"; then
    echo 'FAIL: RPM must retain normal dependency/metadata processing and contain no scriptlets' >&2
    exit 1
fi

echo 'PASS: RPM dispatch and spec structure (real RPM build/extraction not performed)'
