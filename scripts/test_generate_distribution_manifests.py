#!/usr/bin/env python3
"""Checks the winget and AUR manifests generated from a synthetic release checksum list."""
import pathlib
import re
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import generate_distribution_manifests as manifests  # noqa: E402

VERSION = "9.8.7"
DIGESTS = {
    "MultiCore-Setup-x64.exe": "a" * 64,
    "MultiCore-Setup-x86.exe": "b" * 64,
    "MultiCore-Setup-arm64.exe": "c" * 64,
    f"multicore-{VERSION}-1-x86_64.pkg.tar.zst": "d" * 64,
    "multicore-x86_64.pkg.tar.zst": "d" * 64,
    "multicore-windows-x64.zip": "e" * 64,
}


class GenerateTest(unittest.TestCase):
    def setUp(self):
        self.work = pathlib.Path(tempfile.mkdtemp())
        self.sums = self.work / "RELEASE-SHA256SUMS.txt"
        self.sums.write_text(
            "".join(f"{digest}  {name}\n" for name, digest in DIGESTS.items()), encoding="utf-8"
        )
        self.output = self.work / "out"

    def tearDown(self):
        import shutil

        shutil.rmtree(self.work, ignore_errors=True)

    def generate(self, **kwargs):
        return manifests.generate(
            VERSION, self.sums, self.output, release_date="2026-10-05", **kwargs
        )

    def test_winget_installers_point_at_the_release_with_upper_case_digests(self):
        winget, _ = self.generate()
        self.assertEqual(winget.parts[-4:], ("f", "fatyzzz", "MULTICORE", VERSION))
        installer = (winget / "fatyzzz.MULTICORE.installer.yaml").read_text(encoding="utf-8")
        base = f"https://github.com/fatyzzz/multicore/releases/download/v{VERSION}"
        for arch, digest in [("x64", "A"), ("x86", "B"), ("arm64", "C")]:
            self.assertIn(
                f"- Architecture: {arch}\n  InstallerUrl: {base}/MultiCore-Setup-{arch}.exe\n"
                f"  InstallerSha256: {digest * 64}\n",
                installer,
            )
        self.assertIn("InstallerType: inno\n", installer)
        self.assertIn("ProductCode: '{D95AB954-248C-44C9-9E3A-952398802A2D}_is1'\n", installer)
        self.assertIn("ReleaseDate: 2026-10-05\n", installer)
        for name in ["", ".installer", ".locale.en-US", ".locale.ru-RU"]:
            text = (winget / f"fatyzzz.MULTICORE{name}.yaml").read_text(encoding="utf-8")
            self.assertIn(f"PackageVersion: {VERSION}\n", text)
            self.assertNotRegex(text, r"@[A-Z0-9_]+@")
            try:
                import yaml
            except ImportError:
                continue
            document = yaml.safe_load(text)
            self.assertEqual(document["PackageIdentifier"], "fatyzzz.MULTICORE")
            self.assertEqual(document["ManifestVersion"], "1.6.0")

    def test_aur_package_repackages_the_release_arch_package(self):
        _, aur = self.generate()
        pkgbuild = (aur / "PKGBUILD").read_text(encoding="utf-8")
        self.assertIn(f"pkgver={VERSION}\n", pkgbuild)
        self.assertIn(f"sha256sums=('{'d' * 64}')\n", pkgbuild)
        self.assertIn(
            'source=("${_package}::https://github.com/fatyzzz/multicore/releases/'
            'download/v${pkgver}/${_package}")',
            pkgbuild,
        )
        srcinfo = (aur / ".SRCINFO").read_text(encoding="utf-8")
        self.assertTrue(srcinfo.startswith("pkgbase = multicore-bin\n"))
        self.assertIn(
            f"\tsource = multicore-{VERSION}-1-x86_64.pkg.tar.zst::https://github.com/fatyzzz/"
            f"multicore/releases/download/v{VERSION}/"
            f"multicore-{VERSION}-1-x86_64.pkg.tar.zst\n",
            srcinfo,
        )
        self.assertIn(f"\tsha256sums = {'d' * 64}\n", srcinfo)
        self.assertIn("\tprovides = multicore\n\tconflicts = multicore\n", srcinfo)
        self.assertTrue(srcinfo.endswith("\npkgname = multicore-bin\n"))
        install = (aur / "multicore-bin.install").read_text(encoding="utf-8")
        self.assertEqual(install, manifests.ARCH_INSTALL.read_text(encoding="utf-8"))

    def test_aur_dependencies_match_the_release_arch_package(self):
        _, aur = self.generate()
        ours = manifests.pkgbuild_fields((aur / "PKGBUILD").read_text(encoding="utf-8"))
        release = manifests.pkgbuild_fields(
            (manifests.ROOT / "packaging" / "linux" / "arch" / "PKGBUILD").read_text(
                encoding="utf-8"
            )
        )
        for field in ["depends", "optdepends", "license", "arch", "options", "url"]:
            self.assertEqual(ours[field], release[field], field)

    def test_a_missing_asset_or_bad_input_writes_nothing_misleading(self):
        self.sums.write_text(
            "".join(
                f"{digest}  {name}\n"
                for name, digest in DIGESTS.items()
                if name != "MultiCore-Setup-arm64.exe"
            ),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(manifests.ManifestError, "MultiCore-Setup-arm64.exe"):
            self.generate()
        self.assertFalse(self.output.exists())
        with self.assertRaisesRegex(manifests.ManifestError, "release version"):
            manifests.generate("v1.0", self.sums, self.output)
        with self.assertRaisesRegex(manifests.ManifestError, "repository"):
            self.generate(repository="https://github.com/x/y")
        self.sums.write_text("not-a-digest  file\n", encoding="utf-8")
        with self.assertRaisesRegex(manifests.ManifestError, "SHA-256"):
            self.generate()

    def test_checksums_written_by_powershell_with_a_bom_are_read(self):
        self.sums.write_bytes(
            b"\xef\xbb\xbf"
            + "".join(f"{d}  {n}\r\n" for n, d in DIGESTS.items()).encode("utf-8")
        )
        winget, _ = self.generate()
        self.assertTrue(re.search("A{64}", (winget / "fatyzzz.MULTICORE.installer.yaml").read_text(encoding="utf-8")))


if __name__ == "__main__":
    unittest.main()
