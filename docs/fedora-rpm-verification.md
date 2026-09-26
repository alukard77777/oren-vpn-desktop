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

The contract verifies package identity and architecture, runtime dependencies,
inventory and file permissions, and byte-for-byte preservation of the staged
executables. RPM stripping/debug post-processing must not change the pinned
Xray or Mihomo executable hashes.

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
