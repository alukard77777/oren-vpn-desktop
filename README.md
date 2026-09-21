# MultiCore

MultiCore is a native Windows x64 client written in Rust with a Slint UI. It runs
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

## Subscription profiles

MultiCore stores up to 16 independent profiles. Exactly one profile is active;
profiles are never merged. Route selections are persisted per profile. Existing
single-subscription data under the legacy snapshot directory is migrated locally
and offline on first profile-store startup. Activating another profile or editing
its source URL is blocked while connected. Refresh and delete stay available: an
inactive profile is changed without touching the tunnel, while changing the
active profile first stops both cores safely and leaves MultiCore ready to
reconnect.

The subscription strip on the Home screen is a compact active-profile selector
with a small refresh action above the last-updated date. Adding, inspecting, and
managing every profile also lives under the top-level **Subscriptions** page, so
routine switching stays separate from profile management. Rows keep metadata
and actions compact; saved subscription URLs are not displayed back in the
profile list.

The tray icon uses exact mouse behavior: left click restores the main window;
right click toggles MultiCore's own compact control popup with connection,
group, node, open, and exit actions. The Home page's ambient signal waves are
bounded behind content and stop while hidden, minimized, or when Windows asks
applications to reduce motion.

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

Configuration requests include `x-hwid`, `x-device-os`, `x-ver-os`, and
`x-device-model`. Provider metadata may supply a separate optional logo URL in
any response header matching `<vendor>-servicelogo` (case-insensitive), for
example `flclashx-servicelogo` or `ajsdjddj-servicelogo`. The vendor prefix must
be non-empty; it is not a fixed allowlist. The value must be an HTTPS URL with a
host and without embedded credentials. MultiCore fetches that URL with
`multicore-service-logo` and deliberately omits the configuration identity
headers. A
successful HTTP response to `multicore-json-massive` is authoritative: parsing
failure is reported and does not fall back. Only an unavailable or explicitly
unsupported massive response triggers the atomic Mihomo + Xray fallback pair.

The Xray JSON is retained byte-for-byte in the saved snapshot. At Connect time,
an ephemeral runtime copy receives only the client-local DNS/interface overlay
needed to bootstrap before TUN. See the complete
[subscription HTTP contract](docs/subscription-http-contract.md) and the
[provider implementation contract](docs/contracts/multicore-subscription-backend-prompt.md).

While the active profile is disconnected and ready, MultiCore refreshes it
automatically using `profile-update-interval`; when the provider omits that
header, the safe default is six hours. Provider values remain clamped from 15
minutes through 30 days, and failed automatic attempts back off for 15 minutes.
The Home subscription strip shows the active snapshot date in a compact gray
`DD.MM.YYYY` form. Its clock indicator changes to warning state only after the
effective refresh interval has elapsed.

The configuration provider sees the usual HTTP request data, the subscription
URL, the four configuration identity headers, and the selected configuration
User-Agent. A metadata-provided logo endpoint sees its logo URL and the
`multicore-service-logo` User-Agent, but not those identity headers. MultiCore
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
cargo test --workspace --all-targets --locked
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
  -AppVersion 0.1.5
```

Pushing a matching `vX.Y.Z` tag builds and verifies the artifacts, then publishes
them to `fatyzzz/multicore-releases`; installed clients read that public channel
anonymously. Current releases are explicitly unsigned previews. A hardened
production release still requires Authenticode signing, protected release
credentials, immutable corresponding source for the bundled cores, and a
complete Rust dependency/license inventory. The current third-party notice is
explicitly incomplete for the Rust dependency graph.
