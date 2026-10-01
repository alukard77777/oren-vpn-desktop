[CmdletBinding()]
param(
    [string]$DestinationPath,
    [string]$ManifestPath,
    [string]$OfflineSourceDirectory,
    [string]$DesktopExecutablePath,
    [string]$DaemonExecutablePath,
    [string]$UpdaterExecutablePath,
    [string]$CoreHostExecutablePath,
    [string]$UpdateRepository,
    [string]$ReleaseAssetPath,
    [switch]$AssertReleaseReady,
    # x64 (default, PE32+ AMD64) or x86 (PE32 i386, Rust target i686-pc-windows-msvc).
    [ValidateSet('x64', 'x86')]
    [string]$Architecture = 'x64',
    [Parameter(DontShow = $true)]
    [switch]$FixtureContractMode
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Task 4 may flip this single marker only after the broker and package inventory are complete.
$PrivilegedBrokerReleaseReady = $true
$ReleaseGateMessage = 'Production packaging is disabled until the least-privilege core broker and package inventory are complete (Tasks 2-4).'
$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$MetadataRoot = Join-Path $RepositoryRoot "packaging\windows-$Architecture"
# Both flavours ship the same notices (identical core versions and licenses).
$NoticesPath = Join-Path $RepositoryRoot 'packaging\windows-x64\THIRD_PARTY_NOTICES.md'
$DefaultManifestPath = Join-Path $MetadataRoot 'versions.json'
$Platform = "windows-$Architecture"
$ReleaseAssetName = "multicore-$Platform.zip"
# x64 keeps the host target (no --target) exactly as before; x86 cross-compiles.
$RustTarget = if ($Architecture -ceq 'x86') { 'i686-pc-windows-msvc' } else { $null }
if (-not $ManifestPath) { $ManifestPath = $DefaultManifestPath }
$manifestCandidate = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ManifestPath))
$isFixtureInvocation = $false
if ($FixtureContractMode -and $OfflineSourceDirectory -and [IO.File]::Exists($manifestCandidate)) {
    try {
        $temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
        $offlineCandidate = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OfflineSourceDirectory))
        $manifestItem = Get-Item -LiteralPath $manifestCandidate -Force
        $manifestIsSafeTemporaryFile = $manifestCandidate.StartsWith($temporaryRoot, [StringComparison]::OrdinalIgnoreCase) -and
            $offlineCandidate.StartsWith($temporaryRoot, [StringComparison]::OrdinalIgnoreCase) -and
            -not (($manifestItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -and
            $manifestItem.Length -gt 0 -and $manifestItem.Length -le 1MB
        if ($manifestIsSafeTemporaryFile) {
            $fixtureManifest = Get-Content -Raw -LiteralPath $manifestCandidate | ConvertFrom-Json
            $isFixtureInvocation = $fixtureManifest.fixtureContract -ceq 'multicore-package-test-v1' -and
                $fixtureManifest.components.xray.version -ceq 'fixture' -and
                $fixtureManifest.components.mihomo.version -ceq 'fixture' -and
                ([string]$fixtureManifest.components.xray.asset.url).StartsWith('https://example.invalid/', [StringComparison]::Ordinal) -and
                ([string]$fixtureManifest.components.mihomo.asset.url).StartsWith('https://example.invalid/', [StringComparison]::Ordinal)
        }
    } catch {
        $isFixtureInvocation = $false
    }
}
if (-not $isFixtureInvocation) {
    if (-not $PrivilegedBrokerReleaseReady) { throw $ReleaseGateMessage }
    if ($AssertReleaseReady) {
        Write-Output 'PASS: privileged broker release gate is open'
        return
    }
}
if ($AssertReleaseReady) { throw 'AssertReleaseReady is valid only for production packaging readiness checks.' }
if ([String]::IsNullOrWhiteSpace($DestinationPath)) { throw 'DestinationPath is required.' }

function Get-FullPath { param([string]$Path) [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)) }
function Get-Sha256 { param([string]$Path) (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

function Assert-NoReparseAncestor {
    param([string]$Path)
    $full = Get-FullPath $Path
    $root = [IO.Path]::GetPathRoot($full)
    $current = $root
    $relative = $full.Substring($root.Length)
    foreach ($part in ($relative -split '[\\/]')) {
        if (-not $part) { continue }
        $current = Join-Path $current $part
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Reparse-point path component is forbidden: $current" }
        }
    }
}

function Assert-File {
    param([string]$Path, [string]$Description)
    Assert-NoReparseAncestor $Path
    if (-not [IO.File]::Exists((Get-FullPath $Path))) { throw "$Description is missing or is not a regular file: $Path" }
}

function Assert-NoPrivateBuildPaths {
    param([string]$Path, [string]$Description)
    $bytes = [IO.File]::ReadAllBytes((Get-FullPath $Path))
    $utf8 = [Text.Encoding]::UTF8.GetString($bytes)
    $utf16 = [Text.Encoding]::Unicode.GetString($bytes)
    $privateRoots = @(
        $RepositoryRoot,
        [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile),
        $env:CARGO_HOME,
        $env:RUSTUP_HOME
    ) | Where-Object { $_ } | Select-Object -Unique
    foreach ($root in $privateRoots) {
        if ($utf8.IndexOf($root, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
            $utf16.IndexOf($root, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw "$Description contains a private absolute build path"
        }
    }
}

function Invoke-PinnedRustBuild {
    param([object]$Build, [string]$TargetDirectory, [string]$UpdateRepository, [string]$Target)
    $hadRustFlags = Test-Path Env:RUSTFLAGS
    $savedRustFlags = $env:RUSTFLAGS
    $hadEncodedFlags = Test-Path Env:CARGO_ENCODED_RUSTFLAGS
    $savedEncodedFlags = $env:CARGO_ENCODED_RUSTFLAGS
    $hadCFlags = Test-Path Env:CFLAGS
    $savedCFlags = $env:CFLAGS
    $hadCxxFlags = Test-Path Env:CXXFLAGS
    $savedCxxFlags = $env:CXXFLAGS
    $hadTargetDirectory = Test-Path Env:CARGO_TARGET_DIR
    $savedTargetDirectory = $env:CARGO_TARGET_DIR
    $hadUpdateRepository = Test-Path Env:MULTICORE_UPDATE_REPOSITORY
    $savedUpdateRepository = $env:MULTICORE_UPDATE_REPOSITORY
    Push-Location $RepositoryRoot
    try {
        $cargoVersion = (& cargo -V 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0 -or $cargoVersion -cnotmatch '^cargo 1\.98\.1 \(') {
            throw "Packaging requires Cargo 1.98.1; observed: $cargoVersion"
        }
        $rustcVersion = (& rustc -Vv 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0 -or $rustcVersion -cnotmatch '(?m)^release: 1\.98\.1\r?$') {
            throw "Packaging requires rustc 1.98.1"
        }

        Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
        $userProfile = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
        if ([String]::IsNullOrWhiteSpace($userProfile)) { $userProfile = $env:USERPROFILE }
        if ([String]::IsNullOrWhiteSpace($userProfile) -or -not [IO.Path]::IsPathRooted($userProfile)) {
            throw 'Packaging requires an absolute builder user-profile path for deterministic native path remapping'
        }
        $userProfile = [IO.Path]::GetFullPath($userProfile).TrimEnd([IO.Path]::DirectorySeparatorChar)
        $encodedFlags = @(
            "--remap-path-prefix=$RepositoryRoot=multicore-src",
            "--remap-path-prefix=$userProfile=builder-home",
            '-Clink-arg=/PDBALTPATH:multicore.pdb',
            # Link the MSVC C runtime statically. Otherwise every shipped exe
            # imports vcruntime140.dll, which Windows does not include (it
            # comes only with the Visual C++ Redistributable): on a clean
            # Windows 10 install the daemon died at startup with
            # STATUS_DLL_NOT_FOUND (0xC0000135) before doing anything.
            '-Ctarget-feature=+crt-static'
        )
        $env:CARGO_ENCODED_RUSTFLAGS = $encodedFlags -join [char]0x1f
        $nativePathFlags = "/experimental:deterministic /pathmap:$userProfile=builder-home /d1trimfile:$userProfile\"
        $env:CFLAGS = $nativePathFlags
        $env:CXXFLAGS = $nativePathFlags
        $env:CARGO_TARGET_DIR = $TargetDirectory
        if ($UpdateRepository) {
            if ($UpdateRepository -cnotmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,99}/[A-Za-z0-9][A-Za-z0-9._-]{0,99}$') {
                throw 'UpdateRepository must use owner/repository syntax'
            }
            $env:MULTICORE_UPDATE_REPOSITORY = $UpdateRepository
        } else {
            Remove-Item Env:MULTICORE_UPDATE_REPOSITORY -ErrorAction SilentlyContinue
        }
        $targetArguments = @()
        if ($Target) { $targetArguments = @('--target', $Target) }
        & cargo build --locked --release @targetArguments -p $Build.desktopPackage -p $Build.daemonPackage -p $Build.updaterPackage -p $Build.coreHostPackage
        if ($LASTEXITCODE -ne 0) { throw 'Pinned Rust release build failed' }
    } finally {
        Pop-Location
        if ($hadRustFlags) { $env:RUSTFLAGS = $savedRustFlags } else { Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue }
        if ($hadEncodedFlags) { $env:CARGO_ENCODED_RUSTFLAGS = $savedEncodedFlags } else { Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue }
        if ($hadCFlags) { $env:CFLAGS = $savedCFlags } else { Remove-Item Env:CFLAGS -ErrorAction SilentlyContinue }
        if ($hadCxxFlags) { $env:CXXFLAGS = $savedCxxFlags } else { Remove-Item Env:CXXFLAGS -ErrorAction SilentlyContinue }
        if ($hadTargetDirectory) { $env:CARGO_TARGET_DIR = $savedTargetDirectory } else { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
        if ($hadUpdateRepository) { $env:MULTICORE_UPDATE_REPOSITORY = $savedUpdateRepository } else { Remove-Item Env:MULTICORE_UPDATE_REPOSITORY -ErrorAction SilentlyContinue }
    }
}

function Copy-CreateNew {
    param([string]$Source, [string]$Destination)
    Assert-File $Source 'Input file'
    $input = [IO.File]::Open((Get-FullPath $Source), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $output = [IO.File]::Open((Get-FullPath $Destination), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $input.CopyTo($output) } finally { $output.Dispose() }
    } finally { $input.Dispose() }
}

function New-ReleaseAsset {
    param([string]$PackageDirectory, [string]$AssetPath)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $assetFull = Get-FullPath $AssetPath
    if ([IO.Path]::GetFileName($assetFull) -cne $ReleaseAssetName) { throw "ReleaseAssetPath filename must be $ReleaseAssetName" }
    if (Test-Path -LiteralPath $assetFull) { throw "Release asset already exists: $assetFull" }
    $assetParent = Split-Path -Parent $assetFull
    if (-not (Test-Path -LiteralPath $assetParent)) { New-Item -ItemType Directory -Path $assetParent | Out-Null }
    Assert-NoReparseAncestor $assetParent
    $temporary = Join-Path $assetParent ('.multicore-release-{0}.zip' -f [Guid]::NewGuid().ToString('N'))
    try {
        $stream = [IO.File]::Open($temporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        try {
            $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
            try {
                $inventory = Get-StageInventory $PackageDirectory
                foreach ($relative in $inventory.Files) {
                    $entryName = $relative.Replace('\', '/')
                    $entry = $archive.CreateEntry($entryName, [IO.Compression.CompressionLevel]::Optimal)
                    $entry.LastWriteTime = [DateTimeOffset]::new(1980, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
                    $input = [IO.File]::Open((Join-Path $PackageDirectory $relative), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
                    try {
                        $output = $entry.Open()
                        try { $input.CopyTo($output) } finally { $output.Dispose() }
                    } finally { $input.Dispose() }
                }
            } finally { $archive.Dispose() }
        } finally { $stream.Dispose() }
        [IO.File]::Move($temporary, $assetFull)
        Write-Output "Created GitHub release asset: $assetFull"
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}

function Assert-PinnedHttpsUrl {
    param([string]$Url)
    $uri = [Uri]$Url
    if (-not $uri.IsAbsoluteUri -or $uri.Scheme -cne 'https' -or $uri.UserInfo -or $uri.Fragment) { throw "Only absolute pinned HTTPS URLs are permitted: $Url" }
    if ($uri.Host -cnotin @('github.com', 'raw.githubusercontent.com')) { throw "Unapproved download host: $($uri.Host)" }
}

function Get-InputFile {
    param([string]$Name, [string]$Url, [string]$Destination)
    if ($OfflineSourceDirectory) {
        $source = Join-Path (Get-FullPath $OfflineSourceDirectory) $Name
        Copy-CreateNew $source $Destination
    } else {
        Assert-PinnedHttpsUrl $Url
        if (Test-Path -LiteralPath $Destination) { throw "Refusing to overwrite staging input: $Destination" }
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Destination
    }
}

function Assert-Hash {
    param([string]$Path, [string]$Expected, [string]$Description)
    if ($Expected -cnotmatch '^[0-9a-f]{64}$') { throw "Invalid pinned SHA-256 for $Description" }
    $actual = Get-Sha256 $Path
    if ($actual -cne $Expected) { throw "SHA-256 mismatch for $Description (expected $Expected, got $actual)" }
}

function Assert-SafeArchiveName {
    param([string]$Name)
    if ([string]::IsNullOrWhiteSpace($Name) -or $Name.IndexOf([char]0) -ge 0) { throw 'ZIP contains an empty or NUL-bearing entry name' }
    if ($Name.StartsWith('/') -or $Name.StartsWith('\') -or $Name -match '^[A-Za-z]:' -or $Name.Contains(':')) { throw "ZIP contains an absolute, device, or ADS path: $Name" }
    $normalized = $Name.Replace('\', '/')
    $isDirectory = $normalized.EndsWith('/')
    $parts = $normalized.TrimEnd('/').Split('/')
    foreach ($part in $parts) {
        if (-not $part -or $part -eq '.' -or $part -eq '..' -or $part.EndsWith('.') -or $part.EndsWith(' ')) { throw "ZIP contains an unsafe path segment: $Name" }
        $baseName = $part.Split('.')[0]
        if ($baseName -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$') { throw "ZIP contains a reserved Windows path: $Name" }
    }
    if (-not $isDirectory -and $normalized.EndsWith('/')) { throw "ZIP contains an invalid entry: $Name" }
    return $normalized
}

function Assert-SafeLeafName {
    param([string]$Name, [string]$Description)
    $normalized = Assert-SafeArchiveName $Name
    if ($normalized.Contains('/') -or $normalized -cne $Name) { throw "$Description must be one safe filename, not a path: $Name" }
}

function Assert-ExactProductionPin {
    param([string]$Name, [object]$Component)
    $pins = @{
        'x64/xray' = @(
            'XTLS/Xray-core', 'v26.9.30', 'b26a91de4f3294e26a0ad0a970b81a386a41f789', 'MPL-2.0',
            'Xray-windows-64.zip', 'https://github.com/XTLS/Xray-core/releases/download/v26.9.30/Xray-windows-64.zip',
            'b17a619343c11b89d8faf36278298856749b15c52277d875d32051b33dcda617', 'xray.exe',
            '43fa465275a8a64ddce4a27c3317ae3e99f0c264fec1962e04c3fadda83adc79', '268435456',
            'Xray-core-MPL-2.0.txt', 'https://raw.githubusercontent.com/XTLS/Xray-core/b26a91de4f3294e26a0ad0a970b81a386a41f789/LICENSE',
            '1f256ecad192880510e84ad60474eab7589218784b9a50bc7ceee34c2b91f1d5',
            'https://github.com/XTLS/Xray-core/archive/b26a91de4f3294e26a0ad0a970b81a386a41f789.zip',
            'eb89ddcf9d2eb622efa6bb45c1f04ab8f2b52af81d891f05ba7e23ac57cd8a8b'
        )
        'x64/mihomo' = @(
            'MetaCubeX/mihomo', 'v1.19.32', '88dcbf7f1614a67c3b36b848ee3592dfa92ada36', 'GPL-3.0-only',
            'mihomo-windows-amd64-compatible-v1.19.32.zip', 'https://github.com/MetaCubeX/mihomo/releases/download/v1.19.32/mihomo-windows-amd64-compatible-v1.19.32.zip',
            '974a4d7ad69aed27aa2e8f91d61113573c14dadb14562c63e58effabf59816f0', 'mihomo-windows-amd64-compatible.exe',
            '04f8d7fc2b314771e1ecbf15951d59f0e1cb7914503c88cfb6972ff6a824e314', '268435456',
            'mihomo-GPL-3.0.txt', 'https://raw.githubusercontent.com/MetaCubeX/mihomo/88dcbf7f1614a67c3b36b848ee3592dfa92ada36/LICENSE',
            '3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986',
            'https://github.com/MetaCubeX/mihomo/archive/88dcbf7f1614a67c3b36b848ee3592dfa92ada36.zip',
            '63ad2b98fb7a1067422af350a172e2c656c45a2564d0202b5181c6dc4cc7dd30'
        )
        # x86: the same upstream releases and commits, 32-bit Windows assets.
        'x86/xray' = @(
            'XTLS/Xray-core', 'v26.9.30', 'b26a91de4f3294e26a0ad0a970b81a386a41f789', 'MPL-2.0',
            'Xray-windows-32.zip', 'https://github.com/XTLS/Xray-core/releases/download/v26.9.30/Xray-windows-32.zip',
            '709a5f478b7314805c19ad6248cfd15b40dd224efdebfaa48282a41821b7f715', 'xray.exe',
            '8fef35c35970e30a3cfff8a9e6bb60a500621d718f93967bc9ee69a7794b2365', '268435456',
            'Xray-core-MPL-2.0.txt', 'https://raw.githubusercontent.com/XTLS/Xray-core/b26a91de4f3294e26a0ad0a970b81a386a41f789/LICENSE',
            '1f256ecad192880510e84ad60474eab7589218784b9a50bc7ceee34c2b91f1d5',
            'https://github.com/XTLS/Xray-core/archive/b26a91de4f3294e26a0ad0a970b81a386a41f789.zip',
            'eb89ddcf9d2eb622efa6bb45c1f04ab8f2b52af81d891f05ba7e23ac57cd8a8b'
        )
        'x86/mihomo' = @(
            'MetaCubeX/mihomo', 'v1.19.32', '88dcbf7f1614a67c3b36b848ee3592dfa92ada36', 'GPL-3.0-only',
            'mihomo-windows-386-v1.19.32.zip', 'https://github.com/MetaCubeX/mihomo/releases/download/v1.19.32/mihomo-windows-386-v1.19.32.zip',
            '2fb4cfe1552e74eb1b2a0b98ec334041286a0de2ca01ca1ae16b4bf179c09b3f', 'mihomo-windows-386.exe',
            '113d2a9118c26288df743cd753da519f2b2b8de17f1db7a3a892c1dd795eec2e', '268435456',
            'mihomo-GPL-3.0.txt', 'https://raw.githubusercontent.com/MetaCubeX/mihomo/88dcbf7f1614a67c3b36b848ee3592dfa92ada36/LICENSE',
            '3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986',
            'https://github.com/MetaCubeX/mihomo/archive/88dcbf7f1614a67c3b36b848ee3592dfa92ada36.zip',
            '63ad2b98fb7a1067422af350a172e2c656c45a2564d0202b5181c6dc4cc7dd30'
        )
    }
    $actual = @(
        [string]$Component.project, [string]$Component.version, [string]$Component.commit, [string]$Component.license,
        [string]$Component.asset.name, [string]$Component.asset.url, [string]$Component.asset.sha256, [string]$Component.asset.entry,
        [string]$Component.asset.executableSha256, [string]$Component.asset.maximumExpandedBytes,
        [string]$Component.licenseFile.name, [string]$Component.licenseFile.url, [string]$Component.licenseFile.sha256,
        [string]$Component.source.url, [string]$Component.source.sha256
    )
    $expected = $pins["$Architecture/$Name"]
    if ($null -eq $expected -or $actual.Count -ne $expected.Count) { throw "Incomplete built-in pin tuple for $Name" }
    for ($index = 0; $index -lt $expected.Count; $index++) {
        if ($actual[$index] -cne $expected[$index]) { throw "Production manifest does not match the built-in immutable $Name pin tuple" }
    }
}

function Assert-ExactDashboardPin {
    param([object]$Component)
    $expected = @(
        'Zephyruso/zashboard', 'v3.29.1', '50717b02dac68425cdc5b80cb754bea768ee4fc9', 'MIT',
        'dist-no-fonts.zip', 'https://github.com/Zephyruso/zashboard/releases/download/v3.29.1/dist-no-fonts.zip',
        '21371cd111b6b3d87774f3ea3ddeacdcb0f6aff8690a16d652daa3ea6e3b0145', 'dist/',
        '70bb3dc4057cbc0feaff524672f2b0fbafa52279b757dbdc8cb8fdc3e6a73510',
        'zashboard-MIT.txt', 'https://raw.githubusercontent.com/Zephyruso/zashboard/50717b02dac68425cdc5b80cb754bea768ee4fc9/LICENSE',
        'ffc390eec51f16b58506b1a42c99fe0cd9ffea1e3081aab443100fd8c20182d9',
        "default-src 'self'; connect-src 'self' http://127.0.0.1:* ws://127.0.0.1:*; img-src 'self' data: https:; font-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'none'; object-src 'none'"
    )
    $actual = @(
        [string]$Component.project, [string]$Component.version, [string]$Component.commit, [string]$Component.license,
        [string]$Component.asset.name, [string]$Component.asset.url, [string]$Component.asset.sha256, [string]$Component.asset.rootPrefix,
        [string]$Component.asset.indexHtmlSha256,
        [string]$Component.licenseFile.name, [string]$Component.licenseFile.url, [string]$Component.licenseFile.sha256,
        [string]$Component.contentSecurityPolicy
    )
    if ($actual.Count -ne $expected.Count) { throw 'Incomplete built-in pin tuple for dashboard' }
    for ($index = 0; $index -lt $expected.Count; $index++) {
        if ($actual[$index] -cne $expected[$index]) { throw 'Production manifest does not match the built-in immutable dashboard pin tuple' }
    }
}

function Expand-ExactZipEntry {
    param([string]$ArchivePath, [string]$ExpectedEntry, [int64]$MaximumBytes, [string]$Destination)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    if ($MaximumBytes -le 0 -or $MaximumBytes -gt 1073741824) { throw 'Expanded-entry bound is invalid' }
    $expectedNormalized = Assert-SafeArchiveName $ExpectedEntry
    $stream = [IO.File]::OpenRead((Get-FullPath $ArchivePath))
    try {
        $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Read, $false)
        try {
            $seen = @{}
            $matches = New-Object System.Collections.Generic.List[object]
            foreach ($entry in $archive.Entries) {
                $normalized = Assert-SafeArchiveName $entry.FullName
                $key = $normalized.ToUpperInvariant()
                if ($seen.ContainsKey($key)) { throw "ZIP contains duplicate/aliased entries: $($entry.FullName)" }
                $seen[$key] = $true
                $attributes = [BitConverter]::ToUInt32([BitConverter]::GetBytes([int]$entry.ExternalAttributes), 0)
                $unixType = ($attributes -shr 16) -band 0xf000
                if ($unixType -eq 0xa000 -or ($attributes -band 0x400) -ne 0) { throw "ZIP contains a link/reparse entry: $($entry.FullName)" }
                if ($entry.Length -gt $MaximumBytes) { throw "ZIP entry exceeds the configured expanded-size bound: $($entry.FullName)" }
                if ($normalized -ceq $expectedNormalized) { $matches.Add($entry) }
            }
            if ($matches.Count -ne 1) { throw "ZIP must contain exactly one exact entry named $ExpectedEntry" }
            $entryStream = $matches[0].Open()
            try {
                $output = [IO.File]::Open((Get-FullPath $Destination), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
                try {
                    $buffer = New-Object byte[] 65536
                    [int64]$total = 0
                    while (($read = $entryStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                        $total += $read
                        if ($total -gt $MaximumBytes) { throw 'Expanded ZIP entry exceeded its configured bound while streaming' }
                        $output.Write($buffer, 0, $read)
                    }
                } finally { $output.Dispose() }
            } finally { $entryStream.Dispose() }
        } finally { $archive.Dispose() }
    } finally { $stream.Dispose() }
}

function Expand-SafeZipToDirectory {
    param([string]$ArchivePath, [string]$RootPrefix, [string]$Destination, [int64]$MaximumEntryBytes, [int64]$MaximumExpandedBytes, [int]$MaximumEntryCount)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    if ($MaximumEntryBytes -le 0 -or $MaximumEntryBytes -gt 268435456) { throw 'Expanded per-entry bound is invalid' }
    if ($MaximumExpandedBytes -le 0 -or $MaximumExpandedBytes -gt 1073741824) { throw 'Expanded total-size bound is invalid' }
    if ($MaximumEntryCount -le 0 -or $MaximumEntryCount -gt 4096) { throw 'Entry-count bound is invalid' }
    if (-not $RootPrefix -or -not $RootPrefix.EndsWith('/')) { throw 'RootPrefix must be a non-empty path ending with /' }
    [void](Assert-SafeArchiveName $RootPrefix)
    $destinationFull = Get-FullPath $Destination
    if (Test-Path -LiteralPath $destinationFull) { throw "Refusing to extract into an existing directory: $destinationFull" }
    New-Item -ItemType Directory -Path $destinationFull | Out-Null
    $stream = [IO.File]::OpenRead((Get-FullPath $ArchivePath))
    try {
        $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Read, $false)
        try {
            $seen = @{}
            [int64]$total = 0
            $count = 0
            $extractions = New-Object System.Collections.Generic.List[object]
            foreach ($entry in $archive.Entries) {
                $normalized = Assert-SafeArchiveName $entry.FullName
                if ($normalized.EndsWith('/')) { continue }
                if (-not $normalized.StartsWith($RootPrefix, [StringComparison]::Ordinal)) { throw "ZIP entry is outside the expected root ${RootPrefix}: $($entry.FullName)" }
                $relative = $normalized.Substring($RootPrefix.Length)
                if ([string]::IsNullOrEmpty($relative)) { throw 'ZIP contains an entry with an empty relative path' }
                $key = $relative.ToUpperInvariant()
                if ($seen.ContainsKey($key)) { throw "ZIP contains duplicate/aliased entries: $($entry.FullName)" }
                $seen[$key] = $true
                $attributes = [BitConverter]::ToUInt32([BitConverter]::GetBytes([int]$entry.ExternalAttributes), 0)
                $unixType = ($attributes -shr 16) -band 0xf000
                if ($unixType -eq 0xa000 -or ($attributes -band 0x400) -ne 0) { throw "ZIP contains a link/reparse entry: $($entry.FullName)" }
                if ($entry.Length -gt $MaximumEntryBytes) { throw "ZIP entry exceeds the configured per-entry size bound: $($entry.FullName)" }
                $total += $entry.Length
                if ($total -gt $MaximumExpandedBytes) { throw 'ZIP exceeds the configured total expanded-size bound' }
                $count += 1
                if ($count -gt $MaximumEntryCount) { throw 'ZIP exceeds the configured entry-count bound' }
                $extractions.Add([PSCustomObject]@{ Entry = $entry; Relative = $relative })
            }
            if ($extractions.Count -eq 0) { throw "ZIP contains no entries under the expected root: $RootPrefix" }
            foreach ($item in $extractions) {
                $target = Join-Path $destinationFull ($item.Relative.Replace('/', '\'))
                $targetDirectory = Split-Path -Parent $target
                if (-not (Test-Path -LiteralPath $targetDirectory)) { [IO.Directory]::CreateDirectory($targetDirectory) | Out-Null }
                $entryStream = $item.Entry.Open()
                try {
                    $output = [IO.File]::Open($target, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
                    try {
                        $buffer = New-Object byte[] 65536
                        [int64]$written = 0
                        while (($read = $entryStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                            $written += $read
                            if ($written -gt $MaximumEntryBytes) { throw 'Expanded ZIP entry exceeded its configured bound while streaming' }
                            $output.Write($buffer, 0, $read)
                        }
                    } finally { $output.Dispose() }
                } finally { $entryStream.Dispose() }
            }
        } finally { $archive.Dispose() }
    } finally { $stream.Dispose() }
    [string[]]$relativeFiles = @($extractions | ForEach-Object { $_.Relative })
    [Array]::Sort($relativeFiles, [StringComparer]::Ordinal)
    return , $relativeFiles
}

function Protect-DashboardIndexHtml {
    param([string]$IndexHtmlPath, [string]$ExpectedOriginalSha256, [string]$ContentSecurityPolicy, [string]$ReadonlyScriptSource)
    Assert-Hash $IndexHtmlPath $ExpectedOriginalSha256 'dashboard index.html (pre-patch)'
    $indexFull = Get-FullPath $IndexHtmlPath
    $utf8 = New-Object Text.UTF8Encoding($false)
    $original = [IO.File]::ReadAllText($indexFull, [Text.Encoding]::UTF8)

    $charsetMarker = "<meta charset=`"UTF-8`" />`n"
    if (([regex]::Matches($original, [regex]::Escape($charsetMarker))).Count -ne 1) {
        throw 'Dashboard index.html does not have exactly one expected charset marker; refusing to patch (upstream layout may have changed)'
    }
    $cspMetaLine = '    <meta http-equiv="Content-Security-Policy" content="' + $ContentSecurityPolicy + '" />' + "`n"
    $patched = $original.Replace($charsetMarker, $charsetMarker + $cspMetaLine)

    $scriptStartMarker = "    <script>`n      ;(function () {"
    $scriptEndMarker = "    </script>`n  </body>"
    $startIndex = $patched.IndexOf($scriptStartMarker, [StringComparison]::Ordinal)
    if ($startIndex -lt 0) { throw 'Dashboard index.html does not contain the expected inline theme script; refusing to patch (upstream layout may have changed)' }
    $endIndex = $patched.IndexOf($scriptEndMarker, $startIndex, [StringComparison]::Ordinal)
    if ($endIndex -lt 0) { throw 'Dashboard index.html inline theme script is not closed as expected; refusing to patch' }
    $blockEnd = $endIndex + '    </script>'.Length
    $inlineOpen = "    <script>`n"
    $inlineClose = "`n    </script>"
    $block = $patched.Substring($startIndex, $blockEnd - $startIndex)
    if (-not $block.StartsWith($inlineOpen, [StringComparison]::Ordinal) -or -not $block.EndsWith($inlineClose, [StringComparison]::Ordinal)) {
        throw 'Dashboard inline theme script markers are inconsistent; refusing to patch'
    }
    $inlineJs = $block.Substring($inlineOpen.Length, $block.Length - $inlineOpen.Length - $inlineClose.Length)
    $dedented = foreach ($line in ($inlineJs -split "`n")) { if ($line.StartsWith('      ')) { $line.Substring(6) } else { $line } }
    $externalJs = (($dedented -join "`n").Trim("`n")) + "`n"
    $themeScriptPath = Join-Path (Split-Path -Parent $indexFull) 'theme-favicon.js'
    if (Test-Path -LiteralPath $themeScriptPath) { throw "Refusing to overwrite an existing dashboard file: $themeScriptPath" }
    $jsBytes = $utf8.GetBytes($externalJs)
    $jsStream = [IO.File]::Open($themeScriptPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $jsStream.Write($jsBytes, 0, $jsBytes.Length) } finally { $jsStream.Dispose() }

    $patched = $patched.Substring(0, $startIndex) + '    <script src="./theme-favicon.js"></script>' + "`n" + '    <script src="./multicore-readonly.js"></script>' + $patched.Substring($blockEnd)

    if ($patched.IndexOf($ContentSecurityPolicy, [StringComparison]::Ordinal) -lt 0) { throw 'Patched dashboard index.html unexpectedly does not carry the pinned Content-Security-Policy' }
    if ($patched.IndexOf('<script>', [StringComparison]::Ordinal) -ge 0) { throw 'Patched dashboard index.html unexpectedly still contains an inline <script> block' }

    $readonlyScriptPath = Join-Path (Split-Path -Parent $indexFull) 'multicore-readonly.js'
    if (Test-Path -LiteralPath $readonlyScriptPath) { throw "Refusing to overwrite an existing dashboard file: $readonlyScriptPath" }
    [IO.File]::WriteAllBytes($readonlyScriptPath, [IO.File]::ReadAllBytes($ReadonlyScriptSource))

    $patchedBytes = $utf8.GetBytes($patched)
    [IO.File]::WriteAllBytes($indexFull, $patchedBytes)
}

function Read-UInt16 { param([byte[]]$Bytes, [int]$Offset) [BitConverter]::ToUInt16($Bytes, $Offset) }
function Read-UInt32 { param([byte[]]$Bytes, [int]$Offset) [BitConverter]::ToUInt32($Bytes, $Offset) }
function Read-UInt64 { param([byte[]]$Bytes, [int]$Offset) [BitConverter]::ToUInt64($Bytes, $Offset) }

# PE32+ AMD64 for x64, PE32 i386 for x86. PE32 has BaseOfData and 32-bit ImageBase/stack/heap
# fields, so everything after offset 24 of the optional header sits 16 bytes earlier.
function Assert-NativePeExecutable {
    param([string]$Path, [string]$Description)
    $isPe32 = $Architecture -ceq 'x86'
    $expectedMachine = if ($isPe32) { 0x014c } else { 0x8664 }
    $machineName = if ($isPe32) { 'i386' } else { 'AMD64' }
    $expectedMagic = if ($isPe32) { 0x010b } else { 0x020b }
    $formatName = if ($isPe32) { 'PE32' } else { 'PE32+' }
    $minimumOptional = if ($isPe32) { 0xe0 } else { 0xf0 }
    $fixedOptional = if ($isPe32) { 96 } else { 112 }
    Assert-File $Path $Description
    $bytes = [IO.File]::ReadAllBytes((Get-FullPath $Path))
    if ($bytes.Length -lt 512 -or $bytes[0] -ne 0x4d -or $bytes[1] -ne 0x5a) { throw "$Description is not a coherent PE executable" }
    $pe = [int](Read-UInt32 $bytes 0x3c)
    if ($pe -lt 0x40 -or $pe -gt ($bytes.Length - 24) -or $bytes[$pe] -ne 0x50 -or $bytes[$pe + 1] -ne 0x45 -or $bytes[$pe + 2] -ne 0 -or $bytes[$pe + 3] -ne 0) { throw "$Description has an invalid PE header" }
    $coff = $pe + 4
    $machine = Read-UInt16 $bytes $coff
    $sections = Read-UInt16 $bytes ($coff + 2)
    $optionalSize = Read-UInt16 $bytes ($coff + 16)
    $characteristics = Read-UInt16 $bytes ($coff + 18)
    $optional = $coff + 20
    if ($machine -ne $expectedMachine) { throw "$Description is not $machineName" }
    if ($sections -lt 1 -or $sections -gt 96 -or $optionalSize -lt $minimumOptional -or ($optional + $optionalSize) -gt $bytes.Length) { throw "$Description has incoherent PE/COFF dimensions" }
    if (($characteristics -band 0x0002) -eq 0 -or ($characteristics -band 0x2000) -ne 0) { throw "$Description is not a non-DLL executable image" }
    if ((Read-UInt16 $bytes $optional) -ne $expectedMagic) { throw "$Description is not $formatName" }
    $entryPoint = Read-UInt32 $bytes ($optional + 16)
    $sectionAlignment = Read-UInt32 $bytes ($optional + 32)
    $fileAlignment = Read-UInt32 $bytes ($optional + 36)
    $sectionTable = $optional + $optionalSize
    [uint64]$sectionTableEnd = $sectionTable + 40 * $sections
    if ($sectionTableEnd -gt $bytes.Length) { throw "$Description has a truncated section table" }
    $sizeOfImage = Read-UInt32 $bytes ($optional + 56)
    $sizeOfHeaders = Read-UInt32 $bytes ($optional + 60)
    $subsystem = Read-UInt16 $bytes ($optional + 68)
    if ($isPe32) {
        [uint64]$stackReserve = Read-UInt32 $bytes ($optional + 72)
        [uint64]$stackCommit = Read-UInt32 $bytes ($optional + 76)
        [uint64]$heapReserve = Read-UInt32 $bytes ($optional + 80)
        [uint64]$heapCommit = Read-UInt32 $bytes ($optional + 84)
    } else {
        [uint64]$stackReserve = Read-UInt64 $bytes ($optional + 72)
        [uint64]$stackCommit = Read-UInt64 $bytes ($optional + 80)
        [uint64]$heapReserve = Read-UInt64 $bytes ($optional + 88)
        [uint64]$heapCommit = Read-UInt64 $bytes ($optional + 96)
    }
    $directoryCount = Read-UInt32 $bytes ($optional + $fixedOptional - 4)
    if ($directoryCount -gt 16 -or ($fixedOptional + (8 * $directoryCount)) -gt $optionalSize) { throw "$Description has incoherent $formatName data directories" }
    if ($entryPoint -eq 0 -or $entryPoint -ge $sizeOfImage -or $sizeOfImage -eq 0 -or $sizeOfHeaders -lt $sectionTableEnd -or $sizeOfHeaders -gt $bytes.Length) { throw "$Description has incoherent entry/image/header sizes" }
    if ($subsystem -notin @(2, 3)) { throw "$Description does not target the supported Windows GUI/CUI subsystems" }
    if ($stackReserve -eq 0 -or $stackCommit -eq 0 -or $stackCommit -gt $stackReserve -or $heapReserve -eq 0 -or $heapCommit -eq 0 -or $heapCommit -gt $heapReserve) { throw "$Description has incoherent loader stack/heap bounds" }
    if ($fileAlignment -lt 512 -or $fileAlignment -gt 65536 -or (($fileAlignment -band ($fileAlignment - 1)) -ne 0)) { throw "$Description has an invalid PE file alignment" }
    if ($sectionAlignment -eq 0 -or (($sectionAlignment -band ($sectionAlignment - 1)) -ne 0) -or $sectionAlignment -lt $fileAlignment) { throw "$Description has an invalid PE section alignment" }
    if ($sectionAlignment -lt 4096 -and $sectionAlignment -ne $fileAlignment) { throw "$Description uses unsupported sub-page PE alignment" }
    if (($sizeOfHeaders % $fileAlignment) -ne 0 -or ($sizeOfImage % $sectionAlignment) -ne 0) { throw "$Description has unaligned PE header/image sizes" }
    $entryInExecutableSection = $false
    for ($index = 0; $index -lt $sections; $index++) {
        $section = $sectionTable + (40 * $index)
        [uint64]$virtualSize = Read-UInt32 $bytes ($section + 8)
        [uint64]$virtualAddress = Read-UInt32 $bytes ($section + 12)
        [uint64]$rawSize = Read-UInt32 $bytes ($section + 16)
        [uint64]$rawPointer = Read-UInt32 $bytes ($section + 20)
        [uint32]$sectionCharacteristics = Read-UInt32 $bytes ($section + 36)
        if (($virtualAddress % $sectionAlignment) -ne 0 -or ($rawSize % $fileAlignment) -ne 0 -or ($rawSize -gt 0 -and ($rawPointer % $fileAlignment) -ne 0)) { throw "$Description has an unaligned section" }
        if ($rawSize -gt 0 -and ($rawPointer -lt $sizeOfHeaders -or ($rawPointer + $rawSize) -gt $bytes.Length)) { throw "$Description has a section outside the file" }
        $mappedSize = [Math]::Max($virtualSize, $rawSize)
        if ($mappedSize -gt 0 -and ($virtualAddress + $mappedSize) -gt $sizeOfImage) { throw "$Description has a section outside the image" }
        if (($sectionCharacteristics -band 0x20000000) -ne 0 -and $entryPoint -ge $virtualAddress -and $entryPoint -lt ($virtualAddress + $mappedSize)) { $entryInExecutableSection = $true }
    }
    if (-not $entryInExecutableSection) { throw "$Description entry point is not in an executable section" }
}

function Get-StageInventory {
    param([string]$Root)
    $rootFull = Get-FullPath $Root
    $pending = New-Object 'System.Collections.Generic.Queue[string]'
    $pending.Enqueue($rootFull)
    $files = New-Object System.Collections.Generic.List[string]
    $directories = New-Object System.Collections.Generic.List[string]
    while ($pending.Count -gt 0) {
        $current = $pending.Dequeue()
        foreach ($item in @(Get-ChildItem -LiteralPath $current -Force)) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Staging inventory contains a reparse entry: $($item.FullName)" }
            $relative = $item.FullName.Substring($rootFull.Length + 1).Replace('\', '/')
            if ($item.PSIsContainer) {
                $directories.Add($relative)
                $pending.Enqueue($item.FullName)
            } else {
                $files.Add($relative)
            }
        }
    }
    [string[]]$fileArray = $files.ToArray()
    [string[]]$directoryArray = $directories.ToArray()
    [Array]::Sort($fileArray, [StringComparer]::Ordinal)
    [Array]::Sort($directoryArray, [StringComparer]::Ordinal)
    [PSCustomObject]@{ Files = $fileArray; Directories = $directoryArray }
}

$manifestFull = Get-FullPath $ManifestPath
Assert-File $manifestFull 'Version manifest'
$defaultManifestFull = Get-FullPath $DefaultManifestPath
$isProductionManifest = $manifestFull.Equals($defaultManifestFull, [StringComparison]::OrdinalIgnoreCase)
if (-not $OfflineSourceDirectory -and -not $isProductionManifest) {
    throw 'A custom manifest is permitted only with OfflineSourceDirectory fixture mode'
}

$destination = Get-FullPath $DestinationPath
Assert-NoReparseAncestor $destination
if (Test-Path -LiteralPath $destination) { throw "Destination already exists and will not be modified: $destination" }
$parent = Split-Path -Parent $destination
if (-not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent | Out-Null }
Assert-NoReparseAncestor $parent

$releaseAsset = $null
if ($ReleaseAssetPath) {
    $releaseAsset = Get-FullPath $ReleaseAssetPath
    $destinationPrefix = $destination.TrimEnd('\') + '\'
    if ($releaseAsset.StartsWith($destinationPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Release asset cannot be inside the portable package directory' }
    if (Test-Path -LiteralPath $releaseAsset) { throw "Release asset already exists: $releaseAsset" }
}

if ($OfflineSourceDirectory) { Assert-NoReparseAncestor $OfflineSourceDirectory }

$stage = Join-Path $parent ('.multicore-stage-{0}-{1}' -f ([IO.Path]::GetFileName($destination)), [Guid]::NewGuid().ToString('N'))
if (Test-Path -LiteralPath $stage) { throw 'Unexpected staging collision' }
New-Item -ItemType Directory -Path $stage | Out-Null
$published = $false
try {
    foreach ($directory in @('cores', 'runtime', 'licenses', '.inputs')) { New-Item -ItemType Directory -Path (Join-Path $stage $directory) | Out-Null }
    $stagedManifest = Join-Path $stage 'versions.json'
    Copy-CreateNew $manifestFull $stagedManifest
    $manifest = Get-Content -Raw -LiteralPath $stagedManifest | ConvertFrom-Json
    if ($manifest.schemaVersion -ne 1 -or $manifest.platform -cne $Platform) { throw "Unsupported Windows package manifest (expected platform $Platform)" }
    if ($manifest.build.rustToolchain -cne '1.98.1' -or $manifest.build.desktopPackage -cne 'multicore-desktop' -or $manifest.build.daemonPackage -cne 'multicore-daemon' -or $manifest.build.updaterPackage -cne 'multicore-updater' -or $manifest.build.coreHostPackage -cne 'multicore-core-host') { throw 'Unexpected build provenance in manifest' }
    foreach ($componentName in @('xray', 'mihomo')) {
        $component = $manifest.components.$componentName
        if ($component.commit -cnotmatch '^[0-9a-f]{40}$') { throw "$componentName must use a full lowercase commit pin" }
        foreach ($hash in @($component.asset.sha256, $component.asset.executableSha256, $component.licenseFile.sha256, $component.source.sha256)) {
            if ($hash -cnotmatch '^[0-9a-f]{64}$') { throw "$componentName contains an invalid SHA-256 pin" }
        }
        Assert-SafeLeafName $component.asset.name "$componentName archive name"
        Assert-SafeLeafName $component.licenseFile.name "$componentName license name"
        [void](Assert-SafeArchiveName $component.asset.entry)
        if (-not $OfflineSourceDirectory) {
            Assert-ExactProductionPin $componentName $component
            Assert-PinnedHttpsUrl $component.asset.url
            Assert-PinnedHttpsUrl $component.licenseFile.url
            Assert-PinnedHttpsUrl $component.source.url
            if (-not $component.licenseFile.url.Contains($component.commit) -or -not $component.source.url.Contains($component.commit)) { throw "$componentName license/source URLs are not pinned to its full commit" }
        }
    }

    # The vendored dashboard is optional so fixture/offline manifests that predate it keep
    # working unchanged; a real production manifest always carries it (see versions.json).
    $hasDashboard = $manifest.components.PSObject.Properties.Name -contains 'dashboard'
    if ($hasDashboard) {
        $dashboard = $manifest.components.dashboard
        if ($dashboard.commit -cnotmatch '^[0-9a-f]{40}$') { throw 'dashboard must use a full lowercase commit pin' }
        foreach ($hash in @($dashboard.asset.sha256, $dashboard.asset.indexHtmlSha256, $dashboard.licenseFile.sha256)) {
            if ($hash -cnotmatch '^[0-9a-f]{64}$') { throw 'dashboard contains an invalid SHA-256 pin' }
        }
        Assert-SafeLeafName $dashboard.asset.name 'dashboard archive name'
        Assert-SafeLeafName $dashboard.licenseFile.name 'dashboard license name'
        if ([string]$dashboard.asset.rootPrefix -cnotmatch '^[A-Za-z0-9._-]+/$') { throw 'dashboard archive rootPrefix is invalid' }
        if ([int64]$dashboard.asset.maximumEntryBytes -le 0 -or [int64]$dashboard.asset.maximumExpandedBytes -le 0 -or [int]$dashboard.asset.maximumEntryCount -le 0) { throw 'dashboard archive bounds are invalid' }
        $dashboardCsp = [string]$dashboard.contentSecurityPolicy
        if ([string]::IsNullOrWhiteSpace($dashboardCsp) -or $dashboardCsp -cnotmatch "(?:^|;)\s*default-src 'self'\s*(?:;|$)" -or $dashboardCsp -cnotmatch "(?:^|;)\s*script-src 'self'\s*(?:;|$)") {
            throw "dashboard contentSecurityPolicy must lock down default-src and script-src to exactly 'self' (no additional sources, no 'unsafe-inline'/'unsafe-eval')"
        }
        if (-not $OfflineSourceDirectory) {
            Assert-ExactDashboardPin $dashboard
            Assert-PinnedHttpsUrl $dashboard.asset.url
            Assert-PinnedHttpsUrl $dashboard.licenseFile.url
            if (-not $dashboard.licenseFile.url.Contains($dashboard.commit)) { throw 'dashboard license URL is not pinned to its full commit' }
        }
    }

    if ($isProductionManifest) {
        if ($DesktopExecutablePath -or $DaemonExecutablePath -or $UpdaterExecutablePath -or $CoreHostExecutablePath) {
            throw 'Production packaging builds all Rust executables internally; executable overrides are fixture-only'
        }
        $rustTarget = Join-Path $stage '.rust-target'
        Invoke-PinnedRustBuild $manifest.build $rustTarget $UpdateRepository $RustTarget
        $rustRelease = if ($RustTarget) { Join-Path $rustTarget "$RustTarget\release" } else { Join-Path $rustTarget 'release' }
        $DesktopExecutablePath = Join-Path $rustRelease 'multicore-desktop.exe'
        $DaemonExecutablePath = Join-Path $rustRelease 'multicore-daemon.exe'
        $UpdaterExecutablePath = Join-Path $rustRelease 'multicore-apply.exe'
        $CoreHostExecutablePath = Join-Path $rustRelease 'multicore-core-host.exe'
    } elseif (-not $DesktopExecutablePath -or -not $DaemonExecutablePath -or -not $UpdaterExecutablePath -or -not $CoreHostExecutablePath) {
        throw 'Fixture packaging requires all executable paths'
    }
    $stagedDesktop = Join-Path $stage 'MultiCore.exe'
    $stagedDaemon = Join-Path $stage 'runtime\multicore-daemon.exe'
    $stagedUpdater = Join-Path $stage 'runtime\multicore-updater.exe'
    $stagedCoreHost = Join-Path $stage 'runtime\multicore-core-host.exe'
    Copy-CreateNew $DesktopExecutablePath $stagedDesktop
    Copy-CreateNew $DaemonExecutablePath $stagedDaemon
    Copy-CreateNew $UpdaterExecutablePath $stagedUpdater
    Copy-CreateNew $CoreHostExecutablePath $stagedCoreHost
    Assert-NativePeExecutable $stagedDesktop 'Staged desktop executable'
    Assert-NativePeExecutable $stagedDaemon 'Staged daemon executable'
    Assert-NativePeExecutable $stagedUpdater 'Staged updater executable'
    Assert-NativePeExecutable $stagedCoreHost 'Staged core-host executable'
    if ($isProductionManifest) {
        Assert-NoPrivateBuildPaths $stagedDesktop 'Staged desktop executable'
        Assert-NoPrivateBuildPaths $stagedDaemon 'Staged daemon executable'
        Assert-NoPrivateBuildPaths $stagedUpdater 'Staged updater executable'
        Assert-NoPrivateBuildPaths $stagedCoreHost 'Staged core-host executable'
        $rustTargetFull = Get-FullPath $rustTarget
        if ((Split-Path -Parent $rustTargetFull) -cne (Get-FullPath $stage) -or (Split-Path -Leaf $rustTargetFull) -cne '.rust-target') {
            throw 'Refusing unsafe isolated Rust target cleanup'
        }
        Remove-Item -LiteralPath $rustTargetFull -Recurse -Force
    }

    foreach ($componentName in @('xray', 'mihomo')) {
        $component = $manifest.components.$componentName
        $archive = Join-Path $stage ('.inputs\' + $component.asset.name)
        Get-InputFile $component.asset.name $component.asset.url $archive
        Assert-Hash $archive $component.asset.sha256 "$componentName archive"
        $coreTarget = Join-Path $stage ("cores\$componentName.exe")
        Expand-ExactZipEntry $archive $component.asset.entry ([int64]$component.asset.maximumExpandedBytes) $coreTarget
        Assert-Hash $coreTarget $component.asset.executableSha256 "$componentName executable"
        Assert-NativePeExecutable $coreTarget "$componentName executable"

        $licenseTarget = Join-Path $stage ('licenses\' + $component.licenseFile.name)
        Get-InputFile $component.licenseFile.name $component.licenseFile.url $licenseTarget
        Assert-Hash $licenseTarget $component.licenseFile.sha256 "$componentName license"
    }

    $dashboardRelativeFiles = @()
    if ($hasDashboard) {
        $dashboard = $manifest.components.dashboard
        $dashboardArchive = Join-Path $stage ('.inputs\' + $dashboard.asset.name)
        Get-InputFile $dashboard.asset.name $dashboard.asset.url $dashboardArchive
        Assert-Hash $dashboardArchive $dashboard.asset.sha256 'dashboard archive'
        $dashboardTarget = Join-Path $stage 'dashboard'
        $dashboardRelativeFiles = Expand-SafeZipToDirectory $dashboardArchive $dashboard.asset.rootPrefix $dashboardTarget ([int64]$dashboard.asset.maximumEntryBytes) ([int64]$dashboard.asset.maximumExpandedBytes) ([int]$dashboard.asset.maximumEntryCount)
        if (-not ($dashboardRelativeFiles -ccontains 'index.html')) { throw 'Dashboard archive does not contain an index.html at its root' }
        $dashboardIndex = Join-Path $dashboardTarget 'index.html'
        Protect-DashboardIndexHtml $dashboardIndex $dashboard.asset.indexHtmlSha256 $dashboard.contentSecurityPolicy (Join-Path $RepositoryRoot 'packaging\dashboard\multicore-readonly.js')
        $dashboardRelativeFiles = @($dashboardRelativeFiles + @('theme-favicon.js', 'multicore-readonly.js'))

        $dashboardLicenseTarget = Join-Path $stage ('licenses\' + $dashboard.licenseFile.name)
        Get-InputFile $dashboard.licenseFile.name $dashboard.licenseFile.url $dashboardLicenseTarget
        Assert-Hash $dashboardLicenseTarget $dashboard.licenseFile.sha256 'dashboard license'
    }

    Copy-CreateNew (Join-Path $MetadataRoot 'README.md') (Join-Path $stage 'README.md')
    Copy-CreateNew $NoticesPath (Join-Path $stage 'THIRD_PARTY_NOTICES.md')

    $inputs = Get-FullPath (Join-Path $stage '.inputs')
    if ((Split-Path -Parent $inputs) -cne (Get-FullPath $stage) -or (Split-Path -Leaf $inputs) -cne '.inputs') { throw 'Refusing unsafe staging-input cleanup' }
    Remove-Item -LiteralPath $inputs -Recurse -Force
    # Dashboard filenames are content-hashed by its upstream build and change on every
    # re-vendor, so unlike cores/licenses they cannot be hardcoded here; instead they are
    # derived from the archive Expand-SafeZipToDirectory already extracted, whose bytes were
    # already verified against the pinned SHA-256 (and, for a real production manifest, against
    # the immutable dashboard pin tuple) before a single file was written to disk.
    $dashboardExpectedDirectories = New-Object System.Collections.Generic.List[string]
    $dashboardExpectedFiles = New-Object System.Collections.Generic.List[string]
    if ($hasDashboard) {
        $dashboardExpectedDirectories.Add('dashboard')
        foreach ($relative in $dashboardRelativeFiles) {
            $dashboardExpectedFiles.Add("dashboard/$relative")
            $segments = $relative.Split('/')
            for ($depth = 1; $depth -lt $segments.Length; $depth++) {
                $dashboardExpectedDirectories.Add('dashboard/' + (($segments[0..($depth - 1)]) -join '/'))
            }
        }
    }

    $inventory = Get-StageInventory $stage
    [string[]]$directories = $inventory.Directories
    [string[]]$expectedDirectories = @('cores', 'licenses', 'runtime') + @($dashboardExpectedDirectories | Select-Object -Unique)
    [Array]::Sort($expectedDirectories, [StringComparer]::Ordinal)
    if (($directories -join '|') -cne ($expectedDirectories -join '|')) { throw "Staged directory inventory is not exact: $($directories -join ', ')" }
    [string[]]$files = $inventory.Files
    [string[]]$expected = @('MultiCore.exe', 'README.md', 'THIRD_PARTY_NOTICES.md', 'cores/mihomo.exe', 'cores/xray.exe', 'licenses/Xray-core-MPL-2.0.txt', 'licenses/mihomo-GPL-3.0.txt', 'runtime/multicore-core-host.exe', 'runtime/multicore-daemon.exe', 'runtime/multicore-updater.exe', 'versions.json')
    if ($hasDashboard) { $expected = @($expected + @('licenses/' + $dashboard.licenseFile.name) + @($dashboardExpectedFiles)) }
    [Array]::Sort($expected, [StringComparer]::Ordinal)
    if (($files -join '|') -cne ($expected -join '|')) { throw "Staged file inventory is not exact: $($files -join ', ')" }
    $lines = foreach ($relative in $files) { '{0} *{1}' -f (Get-Sha256 (Join-Path $stage $relative)), $relative }
    $sumPath = Join-Path $stage 'SHA256SUMS.txt'
    $encoded = (New-Object Text.UTF8Encoding($false)).GetBytes(($lines -join "`n") + "`n")
    $sumStream = [IO.File]::Open($sumPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $sumStream.Write($encoded, 0, $encoded.Length) } finally { $sumStream.Dispose() }

    $finalInventory = Get-StageInventory $stage
    [string[]]$finalFiles = $finalInventory.Files
    [string[]]$expectedFinalFiles = @($expected + @('SHA256SUMS.txt'))
    [Array]::Sort($expectedFinalFiles, [StringComparer]::Ordinal)
    if (($finalFiles -join '|') -cne ($expectedFinalFiles -join '|')) { throw "Final file inventory is not exact: $($finalFiles -join ', ')" }
    [string[]]$finalDirectories = $finalInventory.Directories
    if (($finalDirectories -join '|') -cne ($expectedDirectories -join '|')) { throw "Final directory inventory is not exact: $($finalDirectories -join ', ')" }

    if (Test-Path -LiteralPath $destination) { throw 'Destination appeared during staging; refusing publication' }
    [IO.Directory]::Move($stage, $destination)
    try {
        if ($releaseAsset) { New-ReleaseAsset $destination $releaseAsset }
    } catch {
        $destinationFull = Get-FullPath $destination
        $parentPrefix = (Get-FullPath $parent).TrimEnd('\') + '\'
        if (-not $destinationFull.StartsWith($parentPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Refusing unsafe failed-publication cleanup' }
        if (Test-Path -LiteralPath $destinationFull) { Remove-Item -LiteralPath $destinationFull -Recurse -Force }
        throw
    }
    $published = $true
    Write-Output "Created local preview package: $destination"
} finally {
    if (-not $published -and (Test-Path -LiteralPath $stage)) {
        $stageFull = Get-FullPath $stage
        $parentPrefix = (Get-FullPath $parent).TrimEnd('\') + '\'
        if (-not $stageFull.StartsWith($parentPrefix, [StringComparison]::OrdinalIgnoreCase) -or -not ([IO.Path]::GetFileName($stageFull)).StartsWith('.multicore-stage-', [StringComparison]::Ordinal)) { throw 'Refusing unsafe staging cleanup' }
        Remove-Item -LiteralPath $stageFull -Recurse -Force
    }
}
