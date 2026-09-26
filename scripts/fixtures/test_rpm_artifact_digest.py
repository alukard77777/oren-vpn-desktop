"""Run the workflow's real artifact digest guard against disposable fixtures."""

import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
BASH = "C:/Program Files/Git/bin/bash.exe" if os.name == "nt" else shutil.which("bash")


def guard_script():
    workflow = (ROOT / ".github/workflows/linux-packages.yml").read_text(encoding="utf-8")
    match = re.search(
        r"(?ms)^      - name: Verify selected RPM digest\n(.*?)(?=^      - |\Z)",
        workflow,
    )
    assert match, "Missing artifact digest guard"
    step = match.group(1)
    assert "        shell: bash\n" in step
    run = re.search(r"(?ms)^        run: \|\n((?:          .*\n|\n)+)", step)
    assert run, "Missing artifact guard script"
    return "\n".join(line[10:] for line in run.group(1).splitlines())


class RpmArtifactDigestTests(unittest.TestCase):
    def test_exact_artifact_required(self):
        payload = b"synthetic RPM digest fixture; never installed\n"
        expected = hashlib.sha256(payload).hexdigest()
        for case in (
            "valid", "changed-bytes", "wrong-hash", "missing", "multiple",
            "directory", "bad-run-id", "empty-run-id", "short-hash", "injected-hash",
        ):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                package_dir = Path(temporary) / "dist/linux"
                package_dir.mkdir(parents=True)
                package = package_dir / "multicore-0.3.0-1.x86_64.rpm"
                if case == "directory":
                    package.mkdir()
                elif case != "missing":
                    package.write_bytes(payload + (b"changed" if case == "changed-bytes" else b""))
                if case == "multiple":
                    (package_dir / "another.rpm").write_bytes(payload)
                run_id = {"bad-run-id": "123;exit 0", "empty-run-id": ""}.get(case, "123")
                digest = {
                    "wrong-hash": "0" * 64,
                    "short-hash": "0" * 63,
                    "injected-hash": expected + "\n" + "0" * 64,
                }.get(case, expected)
                result = subprocess.run(
                    [BASH, "-c", guard_script()],
                    cwd=temporary,
                    env={**os.environ, "SOURCE_RUN_ID": run_id, "EXPECTED_SHA256": digest},
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                self.assertEqual(result.returncode == 0, case == "valid", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
