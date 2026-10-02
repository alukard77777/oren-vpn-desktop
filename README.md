# MULTICORE

MULTICORE is a native Windows x64 and Linux x86_64 client written in Rust with a
Slint UI. It runs
two independent cores: Mihomo owns TUN, DNS, rules, process routing, groups, and
selectors; Xray provides the configured Shadowsocks and FinalMask outbound
transports through local SOCKS inbounds. No WebView is used.

The desktop and its private daemon run as the current user. Windows UAC is
requested only when **Connect** launches `multicore-core-host.exe`. That elevated
host is reused across Connect operations for the daemon lifetime and owns Xray
and Mihomo in a kill-on-close Job Object. Disconnect stops both cores; closing
the authenticated named pipe stops the cores and exits the host. Normal launch,
subscription refresh, settings, autostart, and update checks do not require
elevation.

> **Preview status:** this feature branch is a preview until it is merged and a
> signed production release is published. Current binaries are not
> Authenticode-signed. The elevated host and cores also live in a user-writable
> portable/per-user installation, so this layout is not a hardened privilege
> boundary.

## Install or run

- Portable: keep the generated directory intact and run `MultiCore.exe`.
- Installer: run `MultiCore-Setup-x64.exe`. It installs to
  `%LOCALAPPDATA%\Programs\MultiCore\current` and can add an HKCU autostart entry.
- Source checkout: build with the commands below, then use the packaging script;
  a bare Cargo executable is not a complete runtime package.

The portable package contains `MultiCore.exe`, the private daemon, updater and
core host under `runtime\`, pinned Xray and Mihomo binaries under `cores\`, and
the package manifest, notices, licenses, and checksums. Runtime never downloads
the cores independently.

While connected, *Открыть дашборд* (Status page) opens the pinned, vendored
Zashboard static build (`dashboard\`, staged and CSP-patched by the packaging
scripts at build time) in the default browser, pointed at Mihomo's local
controller with that session's one-time secret in the URL. It is served by
Mihomo itself (`external-ui`), never fetched at runtime, and the button is
disabled whenever there is no live controller session.

## Linux

Linux packaging supports four formats built from one staged tree. All of them
install the same layout under `/usr/lib/multicore` (or inside the AppImage):
the desktop, `runtime/multicore-daemon`, `runtime/multicore-core-host`, and the
pinned Xray and Mihomo binaries under `cores/`.

| Package | Install |
|---|---|
| Arch Linux `multicore-<v>-1-x86_64.pkg.tar.zst` | `sudo pacman -U multicore-<v>-1-x86_64.pkg.tar.zst` |
| Debian 12+ / Ubuntu 22.04+ `multicore_<v>_amd64.deb` | `sudo apt install ./multicore_<v>_amd64.deb` |
| Fedora 43/44 x86_64 `multicore-<v>-1.x86_64.rpm` | `sudo dnf install ./multicore-<v>-1.x86_64.rpm` |
| AppImage `MultiCore-<v>-x86_64.AppImage` | `chmod +x` and run; needs GTK 3, polkit, and AppIndicator on the host |

- **Elevation.** The desktop and daemon run as the user. **Connect** starts the
  core host through `pkexec`; installed packages ship the polkit action
  `io.github.fatyzzz.multicore.core-host`, so the password prompt names
  MULTICORE. The daemon listens on an owner-only socket in
  `$XDG_RUNTIME_DIR/multicore`, requires the connected peer to be root with the
  launched PID, and the host requires the daemon's PID and the invoking user.
  The host resolves configs and cores only through owner- and permission-checked,
  symlink-free paths, and keeps Xray and Mihomo in process groups that die with
  it. An AppImage copies the host and cores to
  `$XDG_CACHE_HOME/multicore/appimage-<hash>/` before elevating, because root
  cannot read a user's FUSE mount; like the Windows preview, that copy is
  user-writable and not a hardened privilege boundary.
- **Tray.** With a StatusNotifierItem host (KDE Plasma, GNOME with the
  AppIndicator extension, waybar, XFCE, Cinnamon, MATE) the tray shows a native
  menu. X11 window managers with an XEmbed tray and no StatusNotifierItem host
  (i3bar, polybar, awesome) show the branded popup. Without any tray host the
  window stays visible and closing it exits.
- **Autostart.** *Launch at sign-in* writes
  `$XDG_CONFIG_HOME/autostart/multicore.desktop` with `--background`. Window
  managers such as i3 need `dex --autostart` or an explicit `exec` line to honour
  XDG autostart.
- **Display scale.** On X11 without `Xft.dpi` the client renders at scale 1 like
  other X11 applications; set `Xft.dpi` or `WINIT_X11_SCALE_FACTOR` to change it.
- **Data.** `$XDG_DATA_HOME/multicore` (default `~/.local/share/multicore`) holds
  profiles, generated runtime configs, preferences, and the device identity.
- **Updates.** Arch, Debian, and Fedora packages are updated by the package manager; the
  client only announces new releases. The AppImage downloads
  `MultiCore-<v>-x86_64.AppImage` from the release channel, verifies size and
  SHA-256, keeps the previous image as `<name>.old`, and relaunches.

## Subscription profiles

MULTICORE stores up to 16 independent profiles. Exactly one profile is active;
profiles are never merged. Route selections are persisted per profile. Existing
single-subscription data under the legacy snapshot directory is migrated locally
and offline on first profile-store startup. Activating another profile or editing
its source URL is blocked while connected. Refresh and delete stay available: an
inactive profile is changed without touching the tunnel. Refreshing the active
profile also keeps the current tunnel up and saves the new configuration for
the next reconnect; deleting the active profile stops both cores safely.

The subscription strip on the Home screen is a compact active-profile selector
with a small refresh action above the last-updated date. Adding, inspecting, and
managing every profile also lives under the top-level **Subscriptions** page, so
routine switching stays separate from profile management. Rows keep metadata
and actions compact; saved subscription URLs are not displayed back in the
profile list.

The tray icon uses exact mouse behavior: left click restores the main window;
right click toggles MULTICORE's own compact control popup with connection,
group, node, open, and exit actions. The Home page's ambient signal waves are
bounded behind content and stop while hidden, minimized, or when Windows asks
applications to reduce motion.

Settings offers separate launch-at-sign-in and auto-connect switches. Auto-connect
defaults off; when enabled, a background sign-in launch waits for a ready profile
and network route, then makes one connection attempt. Home shows a dismissible
update banner only when a newer release is found. MULTICORE checks for releases
at startup and about hourly while it remains open: it reads the one-line
`release.txt` on the release repository's `main` branch
(`https://raw.githubusercontent.com/<owner>/<repo>/main/release.txt`) and asks the
GitHub releases API (which supplies the assets and digests) only when that file names
a newer version, or at most every 4 hours while the file is missing.

The desktop talks only to authenticated loopback `/v1/*` endpoints on its private
daemon. Requests use a random per-launch bearer token, bounded bodies, no proxy,
and no redirects.

## Provider HTTP contract

The subscription URL supports three fixed configuration User-Agent values:

| User-Agent | Response |
| --- | --- |
| `multicore-json-massive` | `["<Mihomo YAML>", {<Xray JSON>}]` JSON tuple |
| `multicore-mihomo` | complete Mihomo YAML |
| `multicore-xray` | one Xray JSON object |

A Mihomo YAML without loopback SOCKS bridges into Xray is a Mihomo-only
subscription: the Xray response is then optional and an idle Xray config runs.
Panels such as Remnawave use this with a single `multicore-mihomo` response rule.
Subscription pages can link `multicore://add/<subscription URL>`.

Configuration requests include `x-hwid`, `x-device-os`, `x-ver-os`, and
`x-device-model`. Provider metadata may supply a separate optional logo URL in
any response header matching `<vendor>-servicelogo` (case-insensitive), for
example `flclashx-servicelogo` or `ajsdjddj-servicelogo`. The vendor prefix must
be non-empty; it is not a fixed allowlist. The value must be an HTTPS URL with a
host and without embedded credentials. MULTICORE fetches that URL with
`multicore-service-logo` and deliberately omits the configuration identity
headers. Mihomo `proxy-groups` entries may name an HTTPS image in `icon:`;
those group icons are fetched the same way (`multicore-group-icon`) on each
refresh and kept with the subscription. A `dumb-mode: on` header (or
`<vendor>-dumb-mode`) makes the app show only the first proxy group, without
group tabs. A
successful HTTP response to `multicore-json-massive` is authoritative: parsing
failure is reported and does not fall back. Only an unavailable or explicitly
unsupported massive response triggers the atomic Mihomo + Xray fallback pair.

The Xray JSON is retained byte-for-byte in the saved snapshot. At Connect time,
an ephemeral runtime copy receives only the client-local DNS/interface overlay
needed to bootstrap before TUN. See the complete
[subscription HTTP contract](docs/subscription-http-contract.md) and the
[provider implementation contract](docs/contracts/multicore-subscription-backend-prompt.md).

While the active profile is disconnected and ready, MULTICORE refreshes it
automatically using `profile-update-interval`; when the provider omits that
header, the safe default is six hours. Provider values remain clamped from 15
minutes through 30 days, and failed automatic attempts back off for 15 minutes.
The Home subscription strip shows the active snapshot date in a compact gray
`DD.MM.YYYY` form. Its clock indicator changes to warning state only after the
effective refresh interval has elapsed.

The configuration provider sees the usual HTTP request data, the subscription
URL, the four configuration identity headers, and the selected configuration
User-Agent. A metadata-provided logo endpoint sees its logo URL and the
`multicore-service-logo` User-Agent, but not those identity headers. MULTICORE
does not operate analytics, telemetry, crash-reporting, account, or subscription
services. See [PRIVACY.md](PRIVACY.md).

## Data, runtime, and updates

- User data: `%LOCALAPPDATA%\MultiCore`
- Profiles: `%LOCALAPPDATA%\MultiCore\profiles`
- Generated connect-time configs: `%LOCALAPPDATA%\MultiCore\runtime`
- Update downloads/helper state: `%LOCALAPPDATA%\MultiCore\updates`
- Per-user installation: `%LOCALAPPDATA%\Programs\MultiCore\current`

Core diagnostics are bounded and redacted before they are exposed through the
authenticated local API and UI. The current elevated-host path retains them in
memory; it does not promise a persistent `latest-core.log`.

Updater builds are pinned to the public release-only repository
`fatyzzz/multicore-releases`. The
client reads the latest non-draft `vX.Y.Z` GitHub Release anonymously and expects
`multicore-windows-x64.zip` with GitHub's SHA-256 asset digest. It then verifies
the archive's internal `SHA256SUMS.txt`, stages a sibling directory, swaps the
complete bundle transactionally, and relaunches the client. Updates are blocked
while connected. Release assets must remain public for anonymous update checks.
The source workflow uses a dedicated least-privilege `RELEASES_TOKEN` Actions
secret with permission to create releases in the separate public release
repository; the token is never embedded in the client.

## Build, verify, and package

Requirements: Windows x64, MSVC Build Tools, PowerShell, Rust 1.98.1, and Inno
Setup 6 for installer creation.

```powershell
$env:RUSTUP_TOOLCHAIN = '1.98.1-x86_64-pc-windows-msvc'
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings

powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-package-windows-release.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-smoke-test-one-click-contract.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-windows-installer-contract.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-release-workflow-contract.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-windows-elevation-manifest-contract.ps1
```

Create the verified portable bundle and updater asset:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\package-windows-release.ps1 `
  -DestinationPath dist\multicore-windows-x64 `
  -UpdateRepository fatyzzz/multicore-releases `
  -ReleaseAssetPath dist\release\multicore-windows-x64.zip
```

Create the per-user installer from that bundle:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\build-windows-installer.ps1 `
  -PackagePath dist\multicore-windows-x64 `
  -OutputDirectory dist\release `
  -AppVersion 0.3.0
```

Linux packages build with Rust 1.98.1 and, for the Debian package and AppImage,
inside the Ubuntu 22.04 image so they run on glibc 2.35 and newer:

```bash
./scripts/package-linux-release.sh --output target/linux-stage
./scripts/build-linux-packages.sh --stage target/linux-stage --output dist/linux --formats arch
./scripts/package-linux-in-docker.sh --output dist/linux --formats deb,appimage
# In a Fedora build environment, reuse the Ubuntu stage for the RPM:
./scripts/build-linux-packages.sh --stage target/linux-stage-ubuntu --output dist/linux --formats rpm
./scripts/test-linux-package-contract.sh --stage target/linux-stage-ubuntu --packages dist/linux --formats rpm
```

`packaging/linux/versions.json` pins the Linux core archives and executables by
SHA-256. CI verifies Linux and Windows on every pull request and builds the Linux
packages as artifacts on pushes to `main` and `feature/**`. The same RPM is installed
and smoke-tested in clean Fedora 43 and 44 containers before a tagged release can
publish. See [Fedora RPM verification](docs/fedora-rpm-verification.md) for the build
commands and the boundaries of the container checks. The verified RPM is also
added to the existing v0.3.0 release; its other binary assets are not replaced.

Pushing a matching `vX.Y.Z` tag builds and verifies the artifacts, then publishes
them to `fatyzzz/multicore-releases`; installed clients read that public channel
anonymously. Current releases are explicitly unsigned previews. A hardened
production release still requires Authenticode signing, protected release
credentials, immutable corresponding source for the bundled cores, and a
complete Rust dependency/license inventory. The current third-party notice is
explicitly incomplete for the Rust dependency graph.
