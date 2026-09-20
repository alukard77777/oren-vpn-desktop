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
and offline on first profile-store startup. Activating or deleting a profile is
blocked while connected.

The subscription strip on the Home screen is a compact active-profile selector
only. Adding, refreshing, inspecting, and deleting profiles lives under
**Settings → Subscriptions**, so routine switching stays separate from profile
management. Saved subscription URLs are not displayed back in the profile list.

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
`x-device-model`. Provider metadata may supply a separate optional logo URL;
MultiCore fetches that URL with `multicore-service-logo` and deliberately omits
the configuration identity headers. A
successful HTTP response to `multicore-json-massive` is authoritative: parsing
failure is reported and does not fall back. Only an unavailable or explicitly
unsupported massive response triggers the atomic Mihomo + Xray fallback pair.

The Xray JSON is retained byte-for-byte in the saved snapshot. At Connect time,
an ephemeral runtime copy receives only the client-local DNS/interface overlay
needed to bootstrap before TUN. See the complete
[subscription HTTP contract](docs/subscription-http-contract.md) and the
[provider implementation contract](docs/contracts/multicore-subscription-backend-prompt.md).

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

Updater builds are pinned to the public repository `fatyzzz/multicore`. The
client reads the latest non-draft `vX.Y.Z` GitHub Release anonymously and expects
`multicore-windows-x64.zip` with GitHub's SHA-256 asset digest. It then verifies
the archive's internal `SHA256SUMS.txt`, stages a sibling directory, swaps the
complete bundle transactionally, and relaunches the client. Updates are blocked
while connected. A private repository or private release is incompatible with
anonymous update checks.

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
  -UpdateRepository fatyzzz/multicore `
  -ReleaseAssetPath dist\release\multicore-windows-x64.zip
```

Create the per-user installer from that bundle:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\build-windows-installer.ps1 `
  -PackagePath dist\multicore-windows-x64 `
  -OutputDirectory dist\release `
  -AppVersion 0.1.2
```

The tag workflow builds and verifies artifacts but leaves the GitHub Release as
a draft. Do not publish an unsigned draft as a stable update. Public production
release requires Authenticode signing, protected release credentials, a public
anonymous asset, immutable corresponding source for the bundled cores, and a
complete Rust dependency/license inventory. The current third-party notice is
explicitly incomplete for the Rust dependency graph.
