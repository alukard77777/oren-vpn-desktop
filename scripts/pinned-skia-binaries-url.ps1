[CmdletBinding()]
param(
    [ValidateSet('x64', 'x86')]
    [string]$Architecture = 'x64',
    [string]$SkiaBinariesDirectory = $env:MULTICORE_SKIA_BINARIES_DIR
)

# Prints the SKIA_BINARIES_URL for the SHA-256-pinned Skia archive of this architecture
# (packaging/windows-<arch>/versions.json, build.skiaBinaries), after verifying the archive.
# For cargo runs outside package-windows-release.ps1 that need Skia but have no prebuilt
# download, e.g. the 32-bit test/clippy step of the release workflow; such runs must also
# link the static C runtime (+crt-static), which the archive is built for.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$manifest = Get-Content -Raw -LiteralPath (Join-Path $RepositoryRoot "packaging\windows-$Architecture\versions.json") | ConvertFrom-Json
$pin = $manifest.build.skiaBinaries
if ([string]$pin.sha256 -cnotmatch '^[0-9a-f]{64}$') { throw 'build.skiaBinaries.sha256 is not a SHA-256 pin' }
if ([string]$pin.key -cnotmatch '^[0-9a-f]{20}-[a-z0-9_-]+-static$') { throw 'build.skiaBinaries.key is not a static-CRT key' }
if ([string]::IsNullOrWhiteSpace($SkiaBinariesDirectory)) {
    throw "Set MULTICORE_SKIA_BINARIES_DIR (or pass -SkiaBinariesDirectory) to the directory holding skia-binaries-$($pin.key).tar.gz (scripts/build-skia-binaries.ps1)"
}
$directory = [IO.Path]::GetFullPath($SkiaBinariesDirectory).TrimEnd([IO.Path]::DirectorySeparatorChar)
$archive = Join-Path $directory "skia-binaries-$($pin.key).tar.gz"
if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) { throw "Pinned Skia binaries archive is missing: $archive" }
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant() -cne [string]$pin.sha256) {
    throw "Skia binaries archive does not match its SHA-256 pin: $archive"
}
Write-Output ('file://' + $directory.Replace([IO.Path]::DirectorySeparatorChar, '/') + '/skia-binaries-{key}.tar.gz')
