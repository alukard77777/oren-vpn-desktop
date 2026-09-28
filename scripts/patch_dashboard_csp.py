#!/usr/bin/env python3
"""Patches a vendored zashboard index.html in place for Linux packaging: inserts the pinned
Content-Security-Policy <meta> tag (read from packaging/linux/versions.json) and externalizes
the inline theme/favicon script into a sibling theme-favicon.js, so the shipped dashboard never
needs 'unsafe-inline' in script-src.

This is the Linux-packaging counterpart of the Protect-DashboardIndexHtml PowerShell function
used by scripts/package-windows-release.ps1; both read the same "dashboard" component from their
respective versions.json so the two platforms cannot silently drift apart.

Fails closed (non-zero exit, no file changes) if the upstream index.html does not match the
exact pinned pre-patch content (dashboard.asset.indexHtmlSha256), or if the expected markers are
not found -- a future zashboard re-vendor must review and re-pin this deliberately rather than
silently shipping a dashboard without the CSP.
"""
import hashlib
import json
import pathlib
import sys


def fail(message: str) -> "None":
    print(f"patch_dashboard_csp: {message}", file=sys.stderr)
    sys.exit(1)


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: patch_dashboard_csp.py <versions.json> <index.html>")
    manifest_path, index_path = sys.argv[1], sys.argv[2]
    with open(manifest_path, encoding="utf-8") as handle:
        manifest = json.load(handle)
    dashboard = manifest["components"]["dashboard"]
    expected_sha = dashboard["asset"]["indexHtmlSha256"]
    csp = dashboard["contentSecurityPolicy"]

    index_file = pathlib.Path(index_path)
    original_bytes = index_file.read_bytes()
    actual_sha = hashlib.sha256(original_bytes).hexdigest()
    if actual_sha != expected_sha:
        fail(
            "index.html does not match the pinned pre-patch SHA-256 "
            f"(expected {expected_sha}, got {actual_sha}); refusing to patch"
        )
    original = original_bytes.decode("utf-8")

    charset_marker = '<meta charset="UTF-8" />\n'
    if original.count(charset_marker) != 1:
        fail("expected exactly one charset marker; upstream layout may have changed")
    csp_meta_line = f'    <meta http-equiv="Content-Security-Policy" content="{csp}" />\n'
    patched = original.replace(charset_marker, charset_marker + csp_meta_line, 1)

    script_start = "    <script>\n      ;(function () {"
    script_end = "    </script>\n  </body>"
    start = patched.find(script_start)
    if start < 0:
        fail("expected inline theme script not found; upstream layout may have changed")
    end = patched.find(script_end, start)
    if end < 0:
        fail("inline theme script is not closed as expected")
    block_end = end + len("    </script>")
    block = patched[start:block_end]
    inline_open, inline_close = "    <script>\n", "\n    </script>"
    if not (block.startswith(inline_open) and block.endswith(inline_close)):
        fail("inline theme script markers are inconsistent")
    inline_js = block[len(inline_open):-len(inline_close)]
    dedented = "\n".join(
        line[6:] if line.startswith(" " * 6) else line for line in inline_js.split("\n")
    ).strip("\n") + "\n"

    theme_script = index_file.parent / "theme-favicon.js"
    if theme_script.exists():
        fail(f"refusing to overwrite an existing {theme_script}")
    theme_script.write_text(dedented, encoding="utf-8", newline="\n")

    patched = patched[:start] + '    <script src="./theme-favicon.js"></script>' + patched[block_end:]

    if csp not in patched:
        fail("patched index.html unexpectedly does not carry the pinned CSP")
    if "<script>" in patched:
        fail("patched index.html unexpectedly still contains an inline <script> block")

    index_file.write_text(patched, encoding="utf-8", newline="\n")
    print(f"patch_dashboard_csp: patched {index_path}")


if __name__ == "__main__":
    main()
