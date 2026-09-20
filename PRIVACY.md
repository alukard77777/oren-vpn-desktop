# Privacy

MultiCore is a native Windows client. The project does not operate a MultiCore
account, analytics, telemetry, crash-reporting, or hosted subscription service.

## Data on this device

Application data is stored under `%LOCALAPPDATA%\MultiCore`. It can contain
subscription URLs, downloaded Mihomo/Xray snapshots, the stable local device
identifier, profile metadata, per-profile selector choices, generated runtime
configuration, UI preferences, and updater downloads/results. The per-user
installer places program files under `%LOCALAPPDATA%\Programs\MultiCore\current`.

Core stdout/stderr is bounded and redacted before it is returned by the
authenticated diagnostics endpoint or shown in the UI. In the current elevated
core-host architecture, that feed is kept in memory; MultiCore does not promise
a persistent `logs\latest-core.log`. Redaction is defense in depth, so review any
diagnostic text before sharing it.

These files are not claimed to be encrypted. Access is controlled by the current
Windows account and filesystem permissions. Uninstalling the program does not
promise to remove `%LOCALAPPDATA%\MultiCore`; delete that directory after exiting
MultiCore if you also want to remove profiles and local state. The updater uses
`%LOCALAPPDATA%\MultiCore\updates` and temporary sibling staging/rollback
directories beside the installed or portable package while applying an update.

## Network requests

Importing or refreshing a profile sends requests to the URL supplied by the
user. Configuration requests use `multicore-json-massive`, `multicore-mihomo`,
or `multicore-xray` and include these identity headers:

- `x-hwid`: stable installation identifier;
- `x-device-os`: `windows`;
- `x-ver-os`: bounded Windows version;
- `x-device-model`: bounded device model.

An optional provider logo request uses `multicore-service-logo` and deliberately
omits all four identity headers. The provider can still observe normal HTTP
information such as the source IP, timing, requested URL, and User-Agent.
Provider privacy practices are outside MultiCore's control.

The desktop and daemon communicate only through authenticated loopback HTTP. A
separate authenticated, local-only named pipe connects the daemon to the elevated
core host after the user chooses Connect. These are local application channels,
not project-operated network services.

If an update repository is compiled into the client, update checks request the
latest stable release metadata and selected asset from GitHub. Official updater
builds use the public `fatyzzz/multicore` repository and require anonymous access;
no GitHub access token is embedded in the client. GitHub's privacy terms apply.

## Elevation and preview security

Normal desktop and daemon processes run as the current user. Connect can invoke
Windows UAC for `multicore-core-host.exe`, which owns Xray and Mihomo and their
cleanup. The host does not expose a remote MultiCore service.

Current preview binaries are unsigned, so SmartScreen may warn. The elevated host
and core executables are also stored in a user-writable portable/per-user
installation. Treat this as preview-grade rather than a hardened privilege
boundary; production distribution requires signed binaries and a hardened
installation/update policy.

This notice describes the current feature branch. Recheck the notice and release
documentation for the exact version you install.
