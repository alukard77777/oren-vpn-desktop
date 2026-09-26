# Fedora RPM

## Supported target

Ordinary Fedora 43 and 44 on x86_64 (Workstation/KDE). Atomic desktops, EPEL,
ARM, and unsupported Fedora releases are outside this package's test matrix.
The RPM is a direct-download upstream package, not an official Fedora repository
submission. Like the existing release channel, it is unsigned.

Install or upgrade from a downloaded file:

```bash
sudo dnf install ./multicore-0.3.0-1.x86_64.rpm
```

Use DNF rather than bypassing dependency resolution with `rpm --nodeps`. The app
announces updates but does not overwrite package-managed files. Download a newer
RPM and install it through DNF. No repository is registered by this package.

The package installs `/usr/bin/multicore`, `/usr/lib/multicore`, the application
menu entry, icon, `multicore://` handler, polkit action, and bundled licenses.
Desktop and daemon run as the user. Only Connect requests authorization through
polkit; installation never starts a VPN, root service, or autoconnection.

## Build and package verification

Reuse the existing Ubuntu 22.04 stage, then run RPM packaging in Fedora 43:

```bash
./scripts/package-linux-in-docker.sh --output dist/linux --formats deb,appimage
./scripts/build-linux-packages.sh --stage target/linux-stage-ubuntu --output dist/linux --formats rpm
./scripts/test-linux-package-contract.sh --stage target/linux-stage-ubuntu --packages dist/linux --formats rpm
```

The RPM builder needs `rpmbuild`, the Fedora RPM configuration, `tar`, `gzip`,
`zstd`, `file`, `cpio`, `diffutils`, and normal GNU core utilities. CI installs the
tools inside a disposable Fedora container, not on the runner host.

Fedora container jobs install Git before checkout. They can leave root-owned
files in the self-hosted runner's shared checkout; native Linux jobs therefore
restore ownership before checkout, after validating the exact canonical
`multicore/multicore` workspace beneath the runner work directory. Recovery never
follows symlinks or targets the runner work root. Portable regression tests cover
outside paths, traversal, symlink escapes, and invalid/root runner identities.

The contract verifies package identity and architecture, runtime dependencies,
inventory and file permissions, and byte-for-byte preservation of the staged
executables. RPM stripping/debug post-processing must not change the pinned
Xray or Mihomo executable hashes.

The RPM explicitly requires `libxkbcommon-x11`: the desktop backend loads it
dynamically, so automatic ELF requirements and `ldd` alone do not detect its
absence. The installed GUI smoke caught this missing dependency on both Fedora
versions; the package dependency contract now checks it as well.

## Installed smoke and limits

CI builds one RPM, then tests that exact artifact in Fedora 43 and 44. Installed
checks use a disposable container and a separate unprivileged account; they never
use the runner user's subscriptions or start a real tunnel. The smoke entry point
requires explicit opt-in:

```bash
MULTICORE_DISPOSABLE_RPM_TEST=1 ./scripts/test-fedora-rpm.sh --package /path/to/multicore-0.3.0-1.x86_64.rpm
```

Do not run that command on a normal workstation: it installs, reinstalls, and
removes the package. Container checks cover dependency resolution, installed
metadata and permissions, linking, unprivileged startup in Xvfb/D-Bus, desktop URI
dispatch, and persistence of test user data across package removal/reinstallation.

The initial DNF install has a 15-minute limit for cold dependency downloads;
reinstall/removal each retain a five-minute limit. The first Fedora 44 attempt
exhausted the earlier five-minute install limit while downloading dependencies
from a mirror returning partial files, before any application checks ran.

To repeat only the installed matrix against an existing same-repository build,
dispatch `linux-packages.yml` with its run ID and the RPM SHA-256 recorded by the
successful packaging job. This does not build or publish anything:

```bash
gh workflow run linux-packages.yml --ref main \
  -f rpm-run-id=VERIFIED_BUILD_RUN_ID -f rpm-sha256=VERIFIED_RPM_SHA256
```

The selected artifact must contain exactly one RPM with the expected digest.
The workflow rejects missing/multiple files, malformed hashes, and changed bytes
before installation. Its token has only read permissions; installed tests still
use disposable Fedora containers and the same safety guards as release CI.

Before declaring live Fedora VPN support verified, also check on a disposable
Fedora desktop/VM with SELinux enforcing:

1. Install with DNF and launch from the application menu in GNOME or KDE.
2. Open a synthetic `multicore://install-sub?url=...` link. Confirm only the domain
   appears, No/Escape makes no request, and Yes imports the chosen subscription.
3. Exercise Connect, cancel the polkit prompt, then authorize Connect. Check tunnel
   routing/DNS and cleanup after Disconnect and after closing the app.
4. Inspect SELinux AVC denials; do not disable SELinux to hide a failure.
5. Check Wayland and the desktop's tray behavior. GNOME without an indicator host
   uses the existing visible-window fallback.
6. Upgrade/reinstall and remove the RPM; profiles, selections, and identity in the
   user's XDG data directory must remain intact.

A headless container cannot establish the result of these live desktop checks.

## Recorded 0.3.0 verification

[CI run 36252019792](https://github.com/fatyzzz/multicore/actions/runs/36252019792)
passed all seven jobs at packaging commit
`c1452030a7a59973e82284de8b5666d4af8693da`.
Both Fedora 43 and 44 reported desktop/daemon UID 1000, zero requests after
declining cold-start consent, one request and one exact persisted synthetic
profile after accepting warm-start consent, and `connected: false`.
Installed metadata/linking, reinstall/removal, and test-user cleanup passed.

The RPM is 41,168,968 bytes, SHA-256
`3e786ba4998df5068d902f2375926bbfec367ed57499899146ae6b83497d682b`.
It and a separate `.rpm.sha256` were added to existing public `v0.3.0`; the eight
previous asset IDs/digests, version, and tag were preserved. The desktop tests do
not expand the live-environment guarantees listed above.
