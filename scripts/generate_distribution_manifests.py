#!/usr/bin/env python3
"""Writes the winget and AUR (multicore-bin) manifests of one GitHub release.

The installer and package digests come from the release's RELEASE-SHA256SUMS.txt, so the
manifests describe exactly the published files. Nothing is submitted: see
docs/distribution-manifests.md for how to hand them to winget-pkgs and the AUR.

    python3 scripts/generate_distribution_manifests.py --version 1.2.0 \\
        --checksums dist/release/RELEASE-SHA256SUMS.txt --output dist/distribution
"""
import argparse
import datetime
import pathlib
import re
import shlex
import shutil
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
WINGET_TEMPLATES = ROOT / "packaging" / "winget"
AUR_TEMPLATES = ROOT / "packaging" / "aur"
ARCH_INSTALL = ROOT / "packaging" / "linux" / "arch" / "multicore.install"
PACKAGE_IDENTIFIER = "fatyzzz.MULTICORE"
DEFAULT_REPOSITORY = "fatyzzz/multicore-releases"

VERSION_PATTERN = re.compile(r"^\d+\.\d+\.\d+$")
REPOSITORY_PATTERN = re.compile(r"^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")


class ManifestError(Exception):
    pass


def read_checksums(path):
    """`<sha256>  <name>` lines (sha256sum format) as {name: lowercase digest}."""
    digests = {}
    for number, line in enumerate(path.read_text(encoding="utf-8-sig").splitlines(), 1):
        if not line.strip():
            continue
        parts = line.split(None, 1)
        if len(parts) != 2:
            raise ManifestError(f"{path}:{number}: not a checksum line")
        digest, name = parts[0].lower(), parts[1].strip().lstrip("*")
        if not SHA256_PATTERN.match(digest):
            raise ManifestError(f"{path}:{number}: not a SHA-256 digest")
        if name in digests and digests[name] != digest:
            raise ManifestError(f"{path}:{number}: {name} listed twice with different digests")
        digests[name] = digest
    return digests


def required(digests, name):
    try:
        return digests[name]
    except KeyError:
        raise ManifestError(f"the checksums do not list {name}") from None


def render(template, values):
    text = template.read_text(encoding="utf-8")
    for key, value in values.items():
        text = text.replace(f"@{key}@", value)
    leftover = re.findall(r"@[A-Z0-9_]+@", text)
    if leftover:
        raise ManifestError(f"{template.name}: unfilled placeholders {sorted(set(leftover))}")
    return text


def pkgbuild_fields(pkgbuild):
    """The top-level `name=value` and `name=(...)` assignments of a simple PKGBUILD, with
    `${name}` references to earlier ones expanded (enough for .SRCINFO)."""
    fields = {}
    lines = iter(pkgbuild.splitlines())
    for line in lines:
        match = re.match(r"^([A-Za-z_][A-Za-z0-9_]*)=(.*)$", line)
        if not match:
            continue
        name, value = match.groups()
        if value.startswith("(") and not value.rstrip().endswith(")"):
            for continued in lines:
                value += " " + continued.strip()
                if continued.rstrip().endswith(")"):
                    break

        def expand(text):
            return re.sub(
                r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}",
                lambda ref: " ".join(fields.get(ref.group(1), [""])),
                text,
            )

        if value.startswith("("):
            words = shlex.split(value.strip()[1:-1])
        else:
            words = shlex.split(value)
        fields[name] = [expand(word) for word in words]
    return fields


SRCINFO_ORDER = [
    ("pkgdesc", False),
    ("pkgver", False),
    ("pkgrel", False),
    ("url", False),
    ("install", False),
    ("arch", True),
    ("license", True),
    ("depends", True),
    ("optdepends", True),
    ("provides", True),
    ("conflicts", True),
    ("noextract", True),
    ("options", True),
    ("source", True),
    ("sha256sums", True),
]


def srcinfo(pkgbuild):
    fields = pkgbuild_fields(pkgbuild)
    lines = [f"pkgbase = {fields['pkgname'][0]}"]
    for name, _ in SRCINFO_ORDER:
        for value in fields.get(name, []):
            lines.append(f"\t{name} = {value}")
    lines += ["", f"pkgname = {fields['pkgname'][0]}", ""]
    return "\n".join(lines)


def generate(version, checksums, output, repository=DEFAULT_REPOSITORY, release_date=None):
    if not VERSION_PATTERN.match(version):
        raise ManifestError(f"not a release version: {version}")
    if not REPOSITORY_PATTERN.match(repository):
        raise ManifestError(f"not a GitHub repository: {repository}")
    release_date = release_date or datetime.date.today().isoformat()
    if not re.match(r"^\d{4}-\d{2}-\d{2}$", release_date):
        raise ManifestError(f"not a date: {release_date}")
    digests = read_checksums(checksums)
    download_base = f"https://github.com/{repository}/releases/download/v{version}"
    values = {
        "VERSION": version,
        "RELEASE_DATE": release_date,
        "DOWNLOAD_BASE": download_base,
        "DOWNLOAD_BASE_PKGVER": f"https://github.com/{repository}/releases/download/v${{pkgver}}",
        "RELEASE_PAGE": f"https://github.com/{repository}/releases/tag/v{version}",
        # winget expects upper-case digests, makepkg lower-case ones.
        "SHA256_SETUP_X64": required(digests, "MultiCore-Setup-x64.exe").upper(),
        "SHA256_SETUP_X86": required(digests, "MultiCore-Setup-x86.exe").upper(),
        "SHA256_SETUP_ARM64": required(digests, "MultiCore-Setup-arm64.exe").upper(),
        "SHA256_ARCH": required(digests, f"multicore-{version}-1-x86_64.pkg.tar.zst"),
    }

    winget = output / "winget" / "manifests" / "f" / "fatyzzz" / "MULTICORE" / version
    winget.mkdir(parents=True, exist_ok=True)
    for suffix in ["", ".installer", ".locale.en-US", ".locale.ru-RU"]:
        name = f"{PACKAGE_IDENTIFIER}{suffix}.yaml"
        (winget / name).write_text(
            render(WINGET_TEMPLATES / f"{name}.in", values), encoding="utf-8", newline="\n"
        )

    aur = output / "aur" / "multicore-bin"
    aur.mkdir(parents=True, exist_ok=True)
    pkgbuild = render(AUR_TEMPLATES / "PKGBUILD.in", values)
    (aur / "PKGBUILD").write_text(pkgbuild, encoding="utf-8", newline="\n")
    (aur / ".SRCINFO").write_text(srcinfo(pkgbuild), encoding="utf-8", newline="\n")
    # The same install hooks as the release's own Arch package.
    shutil.copyfile(ARCH_INSTALL, aur / "multicore-bin.install")
    return winget, aur


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--version", required=True)
    parser.add_argument("--checksums", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--repository", default=DEFAULT_REPOSITORY)
    parser.add_argument("--release-date")
    args = parser.parse_args(argv)
    try:
        winget, aur = generate(
            args.version, args.checksums, args.output, args.repository, args.release_date
        )
    except (ManifestError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"winget manifests: {winget}")
    print(f"AUR package:      {aur}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
