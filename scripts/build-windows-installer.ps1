[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$PackagePath,
    [string]$OutputDirectory = 'dist\release',
    [string]$AppVersion = '1.1.6',
    [string]$CompilerPath,
    # x64 (default), x86 or arm64; the payload must be the package built for the same flavour.
    [ValidateSet('x64', 'x86', 'arm64')]
    [string]$Architecture = 'x64',
    [switch]$ValidateOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$DefinitionPath = Join-Path $RepositoryRoot 'packaging\windows-x64\installer.iss'
# Fixed, non-negotiable payload every package must carry. The optional
# vendored dashboard's asset filenames are content-hashed by its own build
# and change on every re-vendor, so they are never hardcoded here: instead
# the full inventory is taken from the package's own SHA256SUMS.txt below,
# which package-windows-release.ps1 already generated from hash-verified,
# pinned-provenance files. This still fails closed on any untracked file,
# missing file, wrong hash, or malformed/unsafe checksum line - it only
# stops assuming the exact dashboard filenames in advance.
$RequiredCoreFiles = @(
    'MultiCore.exe',
    'README.md',
    'THIRD_PARTY_NOTICES.md',
    'cores/mihomo.exe',
    'cores/xray.exe',
    'licenses/Xray-core-MPL-2.0.txt',
    'licenses/mihomo-GPL-3.0.txt',
    'runtime/multicore-daemon.exe',
    'runtime/multicore-updater.exe',
    'runtime/multicore-core-host.exe',
    'versions.json'
)

function Get-FullPath {
    param([string]$Path)
    [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path))
}

function Assert-NoReparseAncestor {
    param([string]$Path)
    $full = Get-FullPath $Path
    $root = [IO.Path]::GetPathRoot($full)
    $current = $root
    foreach ($part in ($full.Substring($root.Length) -split '[\\/]')) {
        if (-not $part) { continue }
        $current = Join-Path $current $part
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Reparse-point path component is forbidden: $current"
            }
        }
    }
}

function Assert-Package {
    param([string]$Path)
    $root = Get-FullPath $Path
    Assert-NoReparseAncestor $root
    if (-not (Test-Path -LiteralPath $root -PathType Container)) {
        throw "Portable package directory does not exist: $root"
    }

    $actual = @(Get-ChildItem -LiteralPath $root -File -Recurse | ForEach-Object {
        $_.FullName.Substring($root.Length).TrimStart('\', '/').Replace('\', '/')
    } | Sort-Object)

    $sumPath = Join-Path $root 'SHA256SUMS.txt'
    if (-not (Test-Path -LiteralPath $sumPath -PathType Leaf)) {
        throw "Portable package is missing SHA256SUMS.txt: $root"
    }
    $lines = @(Get-Content -LiteralPath $sumPath | Where-Object { $_ })
    $seen = @{}
    foreach ($line in $lines) {
        if ($line -cnotmatch '^([0-9a-f]{64}) \*([A-Za-z0-9._/-]+)$') {
            throw "Malformed SHA256SUMS.txt line: $line"
        }
        $expectedHash = $Matches[1]
        $relative = $Matches[2]
        if ($relative -eq 'SHA256SUMS.txt' -or $relative.Contains('..') -or $seen.ContainsKey($relative)) {
            throw "Unsafe or duplicate checksum path: $relative"
        }
        $candidate = Join-Path $root $relative.Replace('/', '\')
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            throw "Checksum target is missing: $relative"
        }
        $actualHash = (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -cne $expectedHash) {
            throw "Checksum mismatch for $relative"
        }
        $seen[$relative] = $true
    }

    # The package's own hash-verified manifest is the source of truth for the
    # full inventory (dashboard assets included); every file on disk must be
    # covered by it, and nothing covered by it may be missing from disk.
    $expected = @(@($seen.Keys) + @('SHA256SUMS.txt') | Sort-Object)
    if (($actual -join "`n") -cne ($expected -join "`n")) {
        throw "Portable package inventory does not match its own SHA256SUMS.txt.`nExpected:`n$($expected -join "`n")`nActual:`n$($actual -join "`n")"
    }
    foreach ($relative in $RequiredCoreFiles) {
        if (-not $seen.ContainsKey($relative)) { throw "Checksum is missing for required file: $relative" }
    }
    return $root
}

function Resolve-Compiler {
    param([string]$RequestedPath)
    $candidates = @()
    if ($RequestedPath) { $candidates += $RequestedPath }
    $command = Get-Command 'ISCC.exe' -ErrorAction SilentlyContinue
    if ($command) { $candidates += $command.Source }
    if ($env:LOCALAPPDATA) { $candidates += (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe') }
    if (${env:ProgramFiles(x86)}) { $candidates += (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe') }
    if ($env:ProgramFiles) { $candidates += (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe') }
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate -PathType Leaf)) { return Get-FullPath $candidate }
    }
    throw 'ISCC.exe was not found. Install Inno Setup 6 or pass -CompilerPath.'
}

if ($AppVersion -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
    throw 'AppVersion must be a numeric major.minor.patch version'
}
if (-not (Test-Path -LiteralPath $DefinitionPath -PathType Leaf)) {
    throw "Installer definition is missing: $DefinitionPath"
}

$payload = Assert-Package $PackagePath
if ($ValidateOnly) {
    Write-Output "Validated installer payload: $payload"
    return
}

# The package's own hash-verified manifest names its flavour: an x86 or arm64 installer must never
# carry the x64 payload (or the reverse).
$payloadManifest = Get-Content -Raw -LiteralPath (Join-Path $payload 'versions.json') | ConvertFrom-Json
if ([string]$payloadManifest.platform -cne "windows-$Architecture") {
    throw "Payload platform '$($payloadManifest.platform)' does not match installer architecture windows-$Architecture"
}

$compiler = Resolve-Compiler $CompilerPath
$output = Get-FullPath $OutputDirectory
Assert-NoReparseAncestor $output
if (-not (Test-Path -LiteralPath $output)) { New-Item -ItemType Directory -Path $output | Out-Null }
$installerName = "MultiCore-Setup-$Architecture.exe"
$finalPath = Join-Path $output $installerName
if (Test-Path -LiteralPath $finalPath) { throw "Refusing to overwrite existing installer: $finalPath" }

$temporaryOutput = Join-Path $output ('.multicore-installer-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temporaryOutput | Out-Null
try {
    $arguments = @(
        "/DPayloadDir=$payload",
        "/DOutputDir=$temporaryOutput",
        "/DAppVersion=$AppVersion",
        "/DArch=$Architecture",
        '/Qp',
        $DefinitionPath
    )
    & $compiler @arguments
    if ($LASTEXITCODE -ne 0) { throw "ISCC.exe failed with exit code $LASTEXITCODE" }
    $builtPath = Join-Path $temporaryOutput $installerName
    if (-not (Test-Path -LiteralPath $builtPath -PathType Leaf)) { throw 'ISCC.exe did not produce the expected installer' }
    Move-Item -LiteralPath $builtPath -Destination $finalPath
    Write-Output "Created Windows installer: $finalPath"
} finally {
    $resolvedTemporary = Get-FullPath $temporaryOutput
    $outputPrefix = $output.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if ($resolvedTemporary.StartsWith($outputPrefix, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolvedTemporary)) {
        Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force
    }
}
