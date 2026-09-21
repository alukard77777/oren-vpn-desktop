# MultiCore Windows x64 portable preview

This file is copied into packages by `scripts/package-windows-release.ps1`.
Keep the generated directory intact and start `MultiCore.exe`.

The package contains exactly:

- `MultiCore.exe`;
- `runtime/multicore-daemon.exe`, `multicore-updater.exe`, and
  `multicore-core-host.exe`;
- pinned `cores/xray.exe` and `cores/mihomo.exe`;
- `versions.json`, `THIRD_PARTY_NOTICES.md`, two upstream core license texts,
  this README, and `SHA256SUMS.txt`.

`SHA256SUMS.txt` covers every package file except itself. The packaging script
pins official upstream URLs and archive/executable hashes, validates PE32+ AMD64
files, and builds Rust executables with toolchain 1.98.1. This is deterministic
staging, not a claim that upstream binaries are reproducible compiler outputs.

The desktop and daemon run as the current user. Connect requests UAC for the
packaged core host, which owns both cores and their Job Object/pipe cleanup. Core
diagnostics are bounded, redacted, and currently retained in memory rather than
written to a promised persistent log.

Runtime data lives under `%LOCALAPPDATA%\MultiCore`; update downloads are under
`%LOCALAPPDATA%\MultiCore\updates`. The optional per-user installer installs this
bundle beneath `%LOCALAPPDATA%\Programs\MultiCore\current`.

Build the updater asset for the official anonymous channel with:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\package-windows-release.ps1 `
  -DestinationPath dist\multicore-windows-x64 `
  -UpdateRepository fatyzzz/multicore-releases `
  -ReleaseAssetPath dist\release\multicore-windows-x64.zip
```

The updater accepts the latest public, non-draft `vX.Y.Z` GitHub Release, requires
the asset name `multicore-windows-x64.zip` and GitHub `sha256:` digest, then also
verifies this package's internal checksums before replacing the full bundle.
The public release-only repository contains binary releases so installed clients
can update anonymously without an embedded GitHub credential.

This preview is not Authenticode-signed, and its elevated binaries are stored in
a user-writable directory. Public releases must stay clearly labeled as unsigned
previews until the production signing boundary is complete.
Public distribution also requires protected signing/release procedures,
immutable corresponding-source artifacts for the bundled cores, fulfillment of
MPL-2.0/GPL-3.0 obligations, and a complete Rust dependency/license inventory.
`THIRD_PARTY_NOTICES.md` documents the included core artifacts and selected Rust
libraries but explicitly does not claim that the Rust inventory is complete.
