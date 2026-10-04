# Adds version-less copies of the Linux release packages next to the versioned ones, so
# download buttons can link releases/latest/download/<name> without knowing the version.
# The versioned files stay: the AppImage updater of every installed release looks for
# MultiCore-<version>-x86_64.AppImage, and the .zsync file names it too.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Directory,

    [Parameter(Mandatory = $true)]
    [string]$Version
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "Not a release version: $Version"
}
if (-not (Test-Path -LiteralPath $Directory -PathType Container)) {
    throw "Linux package directory does not exist: $Directory"
}

$names = [ordered]@{
    "multicore_$($Version)_amd64.deb"         = 'multicore_amd64.deb'
    "MultiCore-$Version-x86_64.AppImage"      = 'MultiCore-x86_64.AppImage'
    "multicore-$Version-1-x86_64.pkg.tar.zst" = 'multicore-x86_64.pkg.tar.zst'
    "multicore-$Version-1.x86_64.rpm"         = 'multicore.x86_64.rpm'
}
foreach ($versioned in $names.Keys) {
    $source = Join-Path $Directory $versioned
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Missing Linux package $source"
    }
    $destination = Join-Path $Directory $names[$versioned]
    Copy-Item -LiteralPath $source -Destination $destination -Force
    $expected = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
    $actual = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash
    if ($expected -ne $actual) {
        throw "Copy of $versioned differs from its source"
    }
    Write-Output "$($names[$versioned]) = $versioned ($($expected.ToLowerInvariant()))"
}
