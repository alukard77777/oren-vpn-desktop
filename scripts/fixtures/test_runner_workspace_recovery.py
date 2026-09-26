"""Exercise the actual pre-checkout shell guards without changing ownership."""

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
BASH = (
    "C:/Program Files/Git/bin/bash.exe"
    if os.name == "nt"
    else shutil.which("bash")
)
GUARD_NAME = "Recover host workspace ownership"


def job_body(path, name):
    text = (ROOT / path).read_text(encoding="utf-8")
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [\w-]+:\n|\Z)", text
    )
    if not match:
        raise AssertionError(f"Missing workflow job: {path}/{name}")
    return match.group(1)


def guard_script(body):
    match = re.search(
        rf"(?ms)^      - name: {GUARD_NAME}\n(.*?)(?=^      - |\Z)", body
    )
    if not match:
        raise AssertionError("Missing pre-checkout ownership recovery guard")
    step = match.group(1)
    assert "        shell: bash\n" in step
    assert "        working-directory: ${{ runner.temp }}\n" in step
    run = re.search(r"(?ms)^        run: \|\n((?:          .*\n|\n)+)", step)
    assert run, "Recovery guard must contain an inline shell script"
    return "\n".join(line[10:] for line in run.group(1).splitlines())


class WorkspaceRecoveryTests(unittest.TestCase):
    def guards(self):
        for path, name in (
            (".github/workflows/ci.yml", "linux"),
            (".github/workflows/linux-packages.yml", "debian-appimage"),
        ):
            body = job_body(path, name)
            self.assertNotIn("    container:", body)
            script = guard_script(body)
            self.assertLess(body.index(GUARD_NAME), body.index("actions/checkout@"))
            yield f"{path}/{name}", script

    def test_guard_execution_and_scope(self):
        for name, script in self.guards():
            for case in (
                "expected", "outside", "runner-root", "runner-temp",
                "filesystem-root", "traversal", "symlink-escape", "root-user",
                "invalid-uid",
            ):
                with self.subTest(job=name, case=case), tempfile.TemporaryDirectory() as temporary:
                    base = Path(temporary)
                    runner_work = base / "work"
                    runner_temp = runner_work / "_temp"
                    expected = runner_work / "multicore" / "multicore"
                    outside = runner_work / "other" / "other"
                    for directory in (runner_temp, expected, outside):
                        directory.mkdir(parents=True)
                    workspace = {
                        "outside": outside,
                        "runner-root": runner_work,
                        "runner-temp": runner_temp,
                        "filesystem-root": Path(base.anchor),
                        "traversal": expected / ".." / ".." / "other" / "other",
                    }.get(case, expected)
                    if case == "symlink-escape":
                        expected.rmdir()
                        try:
                            expected.symlink_to(outside, target_is_directory=True)
                        except OSError:
                            if os.name != "nt":
                                raise
                            # A Windows junction needs no administrator token;
                            # realpath must reject this directory escape too.
                            subprocess.run(
                                ["cmd", "/c", "mklink", "/J", str(expected), str(outside)],
                                check=True, capture_output=True, text=True,
                            )
                    log = base / "sudo-arguments"
                    environment = dict(os.environ)
                    environment.update(
                        RUNNER_TEMP=str(runner_temp),
                        GITHUB_WORKSPACE=str(workspace),
                        TEST_SUDO_LOG=str(log),
                        TEST_RUNNER_UID="0" if case == "root-user" else "invalid" if case == "invalid-uid" else "1001",
                    )
                    # Functions precede the verbatim workflow script. sudo only
                    # records arguments; it never calls chown or another program.
                    stubs = """id() {
  case "$1" in -u) printf '%s\\n' "$TEST_RUNNER_UID";; -g) printf '1002\\n';; *) return 1;; esac
}
sudo() { printf '%s\\0' "$@" > "$TEST_SUDO_LOG"; }
"""
                    result = subprocess.run(
                        [BASH, "-c", stubs + script], env=environment,
                        capture_output=True, text=True,
                    )
                    if case == "expected":
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                        canonical = subprocess.check_output(
                            [BASH, "-c", 'realpath -e -- "$1"', "fixture", str(expected)],
                            text=True,
                        ).strip()
                        self.assertEqual(
                            log.read_bytes().decode().split("\0")[:-1],
                            ["chown", "-hR", "-P", "--", "1001:1002", canonical],
                        )
                        self.assertIn("Workspace owner before recovery:", result.stdout)
                        self.assertIn("Workspace owner after recovery:", result.stdout)
                    else:
                        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                        self.assertFalse(log.exists(), "Rejected paths/users must never reach sudo")

    def test_fedora_tools_precede_checkout(self):
        for name, step in (
            ("rpm", "Install RPM build and verification tools"),
            ("rpm-smoke", "Install isolated Fedora smoke tools"),
        ):
            with self.subTest(job=name):
                body = job_body(".github/workflows/linux-packages.yml", name)
                self.assertLess(body.index(step), body.index("actions/checkout@"))
                before_checkout = body[:body.index("actions/checkout@")]
                self.assertRegex(before_checkout, r"dnf install -y git ")
                self.assertIn("git --version", before_checkout)
                self.assertNotIn(GUARD_NAME, body)


if __name__ == "__main__":
    unittest.main()
