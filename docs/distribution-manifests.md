# Download links, winget and the AUR

## Version-less Linux assets

Every release carries each Linux package twice: under its versioned name and under a
version-less one, so a download button can link `releases/latest/download/<name>` and never
needs updating.

| Versioned (updater, package managers) | Version-less (download buttons) |
| --- | --- |
| `multicore_<v>_amd64.deb` | `multicore_amd64.deb` |
| `MultiCore-<v>-x86_64.AppImage` | `MultiCore-x86_64.AppImage` |
| `multicore-<v>-1-x86_64.pkg.tar.zst` | `multicore-x86_64.pkg.tar.zst` |
| `multicore-<v>-1.x86_64.rpm` | `multicore.x86_64.rpm` |

For example `https://github.com/fatyzzz/multicore-releases/releases/latest/download/multicore_amd64.deb`.

The versioned files must stay: the in-app AppImage updater of every installed release looks
for `MultiCore-<v>-x86_64.AppImage`. `scripts/add-stable-linux-asset-names.ps1 -Directory
<dir> -Version <v>` adds the copies; the release workflow runs it after collecting the Linux
packages, and a manual release runs it the same way before writing the checksums.

## AppImage updates and AppStream

An AppImage built with `MULTICORE_UPDATE_REPOSITORY=<owner>/<repo>` carries the update
information `gh-releases-zsync|<owner>|<repo>|latest|MultiCore-*x86_64.AppImage.zsync`, and
`build-linux-packages.sh` writes `MultiCore-<v>-x86_64.AppImage.zsync` next to it. Publish
that `.zsync` with the release: AppImageUpdate, AppImageLauncher and Gear Lever use it to fetch
the next release as a delta. The version-less AppImage is the same file, so it updates too.

Every Linux package also ships AppStream metadata,
`/usr/share/metainfo/io.github.fatyzzz.multicore.metainfo.xml`, which software centres and
AppImage catalogues read.

## Generating the winget and AUR manifests

After a release is published, generate both from its checksum list (the
`RELEASE-SHA256SUMS.txt` asset), so the manifests describe exactly the published files:

```bash
python3 scripts/generate_distribution_manifests.py --version 1.2.0 \
    --checksums dist/release/RELEASE-SHA256SUMS.txt --output dist/distribution
```

`--repository` (default `fatyzzz/multicore-releases`) and `--release-date` (default today)
can be overridden. The templates live in `packaging/winget/` and `packaging/aur/`; the AUR
package takes its install hooks from `packaging/linux/arch/multicore.install`, and a test
keeps its dependencies equal to the release's own Arch package.

Nothing is submitted automatically.

### winget

The output is `dist/distribution/winget/manifests/f/fatyzzz/MULTICORE/<v>/` with the version,
installer and two locale manifests (`fatyzzz.MULTICORE`). It describes the three Inno Setup
installers (x64, x86, arm64), machine scope, elevating themselves.

1. Validate: `winget validate --manifest <that directory>`; optionally test the install in a
   VM: `winget install --manifest <that directory>`.
2. Fork `microsoft/winget-pkgs`, copy the directory to the same path under `manifests/`,
   commit and open a pull request (one package version per pull request). The first
   submission is reviewed by hand; later versions only need the new version directory.
   `wingetcreate submit <that directory>` does the fork, commit and pull request in one step.

### AUR (`multicore-bin`)

The output is `dist/distribution/aur/multicore-bin/` with `PKGBUILD`, `.SRCINFO` and
`multicore-bin.install`. It repackages the release's Arch package, so nothing is compiled.

1. On Arch, check it: `makepkg --printsrcinfo | diff - .SRCINFO` and `makepkg -si`.
2. The first time, create the package: `git clone ssh://aur@aur.archlinux.org/multicore-bin.git`
   with an AUR account and its SSH key; later, pull that clone.
3. Copy the three files into the clone, `git add PKGBUILD .SRCINFO multicore-bin.install`,
   commit (`git commit -m "Update to <v>"`) and `git push`.
