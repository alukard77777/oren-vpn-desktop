"""Portable tests for the Fedora smoke's consent and URL checks (no GUI required)."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

spec = importlib.util.spec_from_file_location(
    "fedora_smoke", Path(__file__).with_name("fedora_rpm_smoke.py")
)
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class FedoraSmokeTests(unittest.TestCase):
    def test_uri_preserves_nested_escaping_and_query_separators(self):
        source = "http://127.0.0.1:12345/sub/warm-approved?token=a%2Fb&next=1"
        uri = smoke.install_uri(source)
        parsed = urlsplit(uri)
        self.assertEqual((parsed.scheme, parsed.netloc), ("multicore", "install-sub"))
        self.assertEqual(parse_qs(parsed.query), {"url": [source]})

    def test_uri_rejects_non_loopback_or_unexpected_fixture_routes(self):
        for source in (
            "https://example.com/sub/warm-approved", "http://localhost/sub/warm-approved",
            "http://127.0.0.1.evil/sub/warm-approved", "http://127.0.0.1:12/other",
            "http://user:pass@127.0.0.1:12/sub/warm-approved",
        ):
            with self.subTest(source=source), self.assertRaises(ValueError):
                smoke.install_uri(source)

    def test_modal_requires_all_surface_samples_and_both_selected_button_samples(self):
        colors = [(28, 32, 38)] * 7 + [(33, 44, 58)] * 2
        self.assertTrue(smoke.modal_colors_match(colors))
        self.assertFalse(smoke.modal_colors_match([(13, 15, 18)] * 9))
        colors[0] = (0, 0, 0)
        self.assertFalse(smoke.modal_colors_match(colors))
        self.assertFalse(smoke.modal_colors_match([(28, 32, 38)] * 9))
        self.assertFalse(smoke.modal_colors_match([]))

    def test_fixture_is_valid_dual_core_bundle_with_only_loopback_proxy(self):
        bundle = json.loads(smoke.subscription_body())
        self.assertEqual(len(bundle), 2)
        self.assertIn("server: 127.0.0.1", bundle[0])
        self.assertIn("port: 9", bundle[0])
        self.assertEqual(bundle[1], {"outbounds": [{"protocol": "freedom", "tag": "direct"}]})

    def test_cleanup_does_not_signal_a_recycled_pid(self):
        original = (123, "100", 2000)
        replacement = (123, "101", 2000)
        with patch.object(smoke, "process_identity", return_value=replacement), \
                patch.object(smoke.signal, "SIGKILL", 9, create=True), \
                patch.object(smoke.os, "kill") as kill:
            smoke.stop_processes([original])
        kill.assert_not_called()

    def test_failed_command_does_not_echo_credential_arguments(self):
        result = smoke.subprocess.CompletedProcess(["xauth"], 1, "", "fixture-secret")
        with patch.object(smoke.subprocess, "run", return_value=result):
            with self.assertRaises(RuntimeError) as failure:
                smoke.command(["xauth", "add", ":0", ".", "fixture-secret"])
        self.assertNotIn("fixture-secret", str(failure.exception))


if __name__ == "__main__":
    unittest.main()
