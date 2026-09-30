#!/usr/bin/env python3
"""Checks the pure dashboard patch against a synthetic index.html with the pinned markers."""
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import patch_dashboard_csp  # noqa: E402

SYNTHETIC = (
    "<!doctype html>\n<html>\n  <head>\n    <meta charset=\"UTF-8\" />\n  </head>\n  <body>\n"
    "    <div id=\"app\"></div>\n"
    "    <script>\n      ;(function () {\n        document.title = 'z'\n      })()\n    </script>\n"
    "  </body>\n</html>\n"
)
CSP = "default-src 'self'; script-src 'self'"


class PatchIndexTest(unittest.TestCase):
    def test_csp_theme_and_readonly_script_are_injected(self):
        patched, theme = patch_dashboard_csp.patch_index(SYNTHETIC, CSP)
        self.assertIn(f'content="{CSP}"', patched)
        self.assertNotIn("<script>", patched)
        self.assertIn(
            '<script src="./theme-favicon.js"></script>\n    <script src="./multicore-readonly.js"></script>',
            patched,
        )
        self.assertIn("document.title", theme)

    def test_readonly_script_hides_everything_but_logs_and_rules(self):
        script = (
            pathlib.Path(__file__).resolve().parent.parent
            / "packaging" / "dashboard" / "multicore-readonly.js"
        ).read_text(encoding="utf-8")
        for token in ["'#/logs'", "'#/rules'", "'#/setup'", "location.replace('#/logs')", "hashchange"]:
            self.assertIn(token, script)
        self.assertNotIn("fetch(", script)
        self.assertNotIn("XMLHttpRequest", script)
        # The Logs page mutates constantly: DOM passes are coalesced per frame.
        self.assertIn("requestAnimationFrame", script)
        self.assertIn("new MutationObserver(scheduleHide)", script)

    def test_readonly_script_bytes_do_not_depend_on_the_checkout(self):
        path = (
            pathlib.Path(__file__).resolve().parent.parent
            / "packaging" / "dashboard" / "multicore-readonly.js"
        )
        self.assertNotIn(b"\r", path.read_bytes())


if __name__ == "__main__":
    unittest.main()
