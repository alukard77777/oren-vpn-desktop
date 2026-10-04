[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$Packager = Join-Path $PSScriptRoot 'package-windows-release.ps1'
$PackagerSource = Get-Content -Raw -LiteralPath $Packager
$MetadataRoot = Join-Path $RepositoryRoot 'packaging\windows-x64'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "ASSERTION FAILED: $Message" }
}

function Assert-NoStageResidue {
    param([string]$Parent, [string]$Context)
    $residue = @(Get-ChildItem -LiteralPath $Parent -Force -Directory -Filter '.multicore-stage-*' -ErrorAction SilentlyContinue)
    $residueNames = @($residue | ForEach-Object { $_.Name })
    Assert-True ($residue.Count -eq 0) "$Context left sibling staging residue: $($residueNames -join ', ')"
}

function Set-UInt16 {
    param([byte[]]$Bytes, [int]$Offset, [uint16]$Value)
    [BitConverter]::GetBytes($Value).CopyTo($Bytes, $Offset)
}

function Set-UInt32 {
    param([byte[]]$Bytes, [int]$Offset, [uint32]$Value)
    [BitConverter]::GetBytes($Value).CopyTo($Bytes, $Offset)
}

function Set-UInt64 {
    param([byte[]]$Bytes, [int]$Offset, [uint64]$Value)
    [BitConverter]::GetBytes($Value).CopyTo($Bytes, $Offset)
}

function New-TestPe {
    param(
        [string]$Path,
        [uint16]$Machine = 0x8664,
        [switch]$Dll,
        [switch]$Malformed,
        [uint32]$SizeOfHeaders = 0x0200,
        [uint32]$SectionAlignment = 0x1000,
        [uint32]$FileAlignment = 0x0200,
        [uint16]$Subsystem = 3,
        [uint64]$StackReserve = 0x100000,
        [uint64]$StackCommit = 0x1000,
        [uint64]$HeapReserve = 0x100000,
        [uint64]$HeapCommit = 0x1000
    )
    $bytes = New-Object byte[] 1024
    if (-not $Malformed) {
        $bytes[0] = 0x4d; $bytes[1] = 0x5a
        Set-UInt32 $bytes 0x3c 0x80
        $bytes[0x80] = 0x50; $bytes[0x81] = 0x45
        Set-UInt16 $bytes 0x84 $Machine
        Set-UInt16 $bytes 0x86 1
        Set-UInt16 $bytes 0x94 0xf0
        $characteristics = [uint16]0x0022
        if ($Dll) { $characteristics = [uint16]($characteristics -bor 0x2000) }
        Set-UInt16 $bytes 0x96 $characteristics
        Set-UInt16 $bytes 0x98 0x020b
        Set-UInt32 $bytes 0xa8 0x1000
        Set-UInt64 $bytes 0xb0 0x0000000140000000
        Set-UInt32 $bytes 0xb8 $SectionAlignment
        Set-UInt32 $bytes 0xbc $FileAlignment
        Set-UInt32 $bytes 0xd0 0x2000
        Set-UInt32 $bytes 0xd4 $SizeOfHeaders
        Set-UInt16 $bytes 0xdc $Subsystem
        Set-UInt64 $bytes 0xe0 $StackReserve
        Set-UInt64 $bytes 0xe8 $StackCommit
        Set-UInt64 $bytes 0xf0 $HeapReserve
        Set-UInt64 $bytes 0xf8 $HeapCommit
        Set-UInt32 $bytes 0x104 16
        [Text.Encoding]::ASCII.GetBytes('.text').CopyTo($bytes, 0x188)
        Set-UInt32 $bytes 0x190 0x100
        Set-UInt32 $bytes 0x194 0x1000
        Set-UInt32 $bytes 0x198 0x200
        Set-UInt32 $bytes 0x19c 0x200
        Set-UInt32 $bytes 0x1ac 0x60000020
    }
    [IO.File]::WriteAllBytes($Path, $bytes)
}

# A PE32 i386 image for the x86 flavour (PE32: 32-bit ImageBase and stack/heap fields, 0xe0-byte
# optional header, section table at 0x178).
function New-TestPe32 {
    param([string]$Path, [uint16]$Machine = 0x014c)
    $bytes = New-Object byte[] 1024
    $bytes[0] = 0x4d; $bytes[1] = 0x5a
    Set-UInt32 $bytes 0x3c 0x80
    $bytes[0x80] = 0x50; $bytes[0x81] = 0x45
    Set-UInt16 $bytes 0x84 $Machine
    Set-UInt16 $bytes 0x86 1
    Set-UInt16 $bytes 0x94 0xe0
    Set-UInt16 $bytes 0x96 0x0102
    Set-UInt16 $bytes 0x98 0x010b
    Set-UInt32 $bytes 0xa8 0x1000
    Set-UInt32 $bytes 0xb4 0x00400000
    Set-UInt32 $bytes 0xb8 0x1000
    Set-UInt32 $bytes 0xbc 0x0200
    Set-UInt32 $bytes 0xd0 0x2000
    Set-UInt32 $bytes 0xd4 0x0200
    Set-UInt16 $bytes 0xdc 3
    Set-UInt32 $bytes 0xe0 0x100000
    Set-UInt32 $bytes 0xe4 0x1000
    Set-UInt32 $bytes 0xe8 0x100000
    Set-UInt32 $bytes 0xec 0x1000
    Set-UInt32 $bytes 0xf4 16
    [Text.Encoding]::ASCII.GetBytes('.text').CopyTo($bytes, 0x178)
    Set-UInt32 $bytes 0x180 0x100
    Set-UInt32 $bytes 0x184 0x1000
    Set-UInt32 $bytes 0x188 0x200
    Set-UInt32 $bytes 0x18c 0x200
    Set-UInt32 $bytes 0x19c 0x60000020
    [IO.File]::WriteAllBytes($Path, $bytes)
}

function New-TestZip {
    param([string]$Path, [object[]]$Entries)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $stream = [IO.File]::Open($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            foreach ($item in $Entries) {
                $entry = $archive.CreateEntry([string]$item.Name, [IO.Compression.CompressionLevel]::NoCompression)
                if ($null -ne $item.Attributes) { $entry.ExternalAttributes = [int]$item.Attributes }
                $output = $entry.Open()
                try { $output.Write([byte[]]$item.Data, 0, ([byte[]]$item.Data).Length) } finally { $output.Dispose() }
            }
        } finally { $archive.Dispose() }
    } finally { $stream.Dispose() }
}

function Get-Sha256 { param([string]$Path) (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

function Write-TestManifest {
    param(
        [string]$Path,
        [string]$InputDirectory,
        [string]$XrayExecutableHash,
        [string]$MihomoExecutableHash,
        [int64]$MaximumExpandedBytes = 1048576,
        [string]$XrayArchiveHash,
        [string]$MihomoArchiveHash,
        [string]$Platform = 'windows-x64',
        # Pin Mihomo as a locally built binary (components.mihomo.build) instead of a ZIP asset.
        [switch]$MihomoBuild,
        [switch]$MihomoBuildAndAsset
    )
    if (-not $XrayArchiveHash) { $XrayArchiveHash = Get-Sha256 (Join-Path $InputDirectory 'xray-fixture.zip') }
    if (-not $MihomoArchiveHash -and -not $MihomoBuild) { $MihomoArchiveHash = Get-Sha256 (Join-Path $InputDirectory 'mihomo-fixture.zip') }
    $manifest = [ordered]@{
        schemaVersion = 1
        fixtureContract = 'multicore-package-test-v1'
        platform = $Platform
        build = [ordered]@{ rustToolchain = '1.98.1'; desktopPackage = 'multicore-desktop'; daemonPackage = 'multicore-daemon'; updaterPackage = 'multicore-updater'; coreHostPackage = 'multicore-core-host' }
        components = [ordered]@{
            xray = [ordered]@{
                project = 'XTLS/Xray-core'; version = 'fixture'; commit = ('1' * 40); license = 'MPL-2.0'
                asset = [ordered]@{ name = 'xray-fixture.zip'; url = 'https://example.invalid/xray-fixture.zip'; sha256 = $XrayArchiveHash; entry = 'xray.exe'; executableSha256 = $XrayExecutableHash; maximumExpandedBytes = $MaximumExpandedBytes }
                licenseFile = [ordered]@{ name = 'Xray-core-MPL-2.0.txt'; url = 'https://example.invalid/LICENSE'; sha256 = (Get-Sha256 (Join-Path $InputDirectory 'Xray-core-MPL-2.0.txt')) }
                source = [ordered]@{ url = 'https://example.invalid/source'; sha256 = ('2' * 64) }
            }
            mihomo = [ordered]@{
                project = 'MetaCubeX/mihomo'; version = 'fixture'; commit = ('3' * 40); license = 'GPL-3.0-only'
                asset = [ordered]@{ name = 'mihomo-fixture.zip'; url = 'https://example.invalid/mihomo-fixture.zip'; sha256 = $MihomoArchiveHash; entry = 'mihomo.exe'; executableSha256 = $MihomoExecutableHash; maximumExpandedBytes = $MaximumExpandedBytes }
                licenseFile = [ordered]@{ name = 'mihomo-GPL-3.0.txt'; url = 'https://example.invalid/LICENSE'; sha256 = (Get-Sha256 (Join-Path $InputDirectory 'mihomo-GPL-3.0.txt')) }
                source = [ordered]@{ url = 'https://example.invalid/source'; sha256 = ('4' * 64) }
            }
        }
    }
    if ($MihomoBuild) {
        $mihomo = $manifest.components.mihomo
        $build = [ordered]@{ series = 'fixture'; recipe = 'packaging/mihomo-patches/manifest.json'; target = 'windows-amd64'; file = 'mihomo-fixture.exe'; executableSha256 = $MihomoExecutableHash }
        $rebuilt = [ordered]@{}
        foreach ($key in @($mihomo.Keys)) {
            if ($key -ceq 'asset') {
                $rebuilt['build'] = $build
                if ($MihomoBuildAndAsset) { $rebuilt['asset'] = $mihomo['asset'] }
            } else {
                $rebuilt[$key] = $mihomo[$key]
            }
        }
        $manifest.components.mihomo = $rebuilt
    }
    $manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $Path -Encoding UTF8
}

function Invoke-Package {
    param([string]$Destination, [string]$InputDirectory, [string]$Manifest, [string]$Desktop, [string]$Daemon, [string]$CoreHost, [string]$ReleaseAsset, [string]$Architecture, [string]$UpdaterPath, [string]$MihomoBinaries)
    $parameters = @{
        DestinationPath = $Destination
        OfflineSourceDirectory = $InputDirectory
        ManifestPath = $Manifest
        DesktopExecutablePath = $Desktop
        DaemonExecutablePath = $Daemon
        CoreHostExecutablePath = $CoreHost
        UpdaterExecutablePath = $Updater
        FixtureContractMode = $true
    }
    if ($UpdaterPath) { $parameters.UpdaterExecutablePath = $UpdaterPath }
    if ($ReleaseAsset) { $parameters.ReleaseAssetPath = $ReleaseAsset }
    if ($Architecture) { $parameters.Architecture = $Architecture }
    if ($MihomoBinaries) { $parameters.MihomoBinariesDirectory = $MihomoBinaries }
    & $Packager @parameters
}

function Assert-PackageFailure {
    param([string]$Name, [scriptblock]$Arrange)
    $caseRoot = Join-Path $TestRoot $Name
    New-Item -ItemType Directory -Path $caseRoot | Out-Null
    $input = Join-Path $caseRoot 'input'; New-Item -ItemType Directory -Path $input | Out-Null
    Get-ChildItem -LiteralPath $BaseInput | Copy-Item -Destination $input -Recurse
    $manifest = Join-Path $caseRoot 'versions.json'
    & $Arrange $input $manifest
    $destination = Join-Path $caseRoot 'output'
    $failed = $false
    try { Invoke-Package $destination $input $manifest $Desktop $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed "$Name must fail"
    Assert-True (-not (Test-Path -LiteralPath $destination)) "$Name must not publish a partial destination"
    Assert-NoStageResidue $caseRoot $Name
}

Assert-True (Test-Path -LiteralPath $Packager) 'packager script must exist'
Assert-True ($PackagerSource.Contains('$PrivilegedBrokerReleaseReady = $true')) 'production release gate marker must be true after broker Tasks 2-4 are complete'
Assert-True ($PackagerSource.Contains('$AssertReleaseReady')) 'packager must expose the explicit CI readiness check'
Assert-True ($PackagerSource.Contains("fixtureContract -ceq 'multicore-package-test-v1'")) 'fixture authorization must require the synthetic manifest marker'
Assert-True ($PackagerSource.Contains("'-Ctarget-feature=+crt-static'")) 'release builds must link the C runtime statically (clean Windows has no vcruntime140.dll)'
Assert-True ($PackagerSource.Contains('$skia = Resolve-PinnedSkiaBinaries $Build')) 'release builds must link the SHA-256-pinned Skia binaries (no prebuilt exists for +crt-static, x86 or arm64)'
Assert-True ($PackagerSource.Contains('$env:SKIA_BINARIES_URL = $skia.Url')) 'skia-bindings must read the pinned archive instead of downloading or building Skia'
Assert-True ($PackagerSource.Contains('Assert-LinkedPinnedSkia $TargetDirectory $Target $skia.Key')) 'release builds must prove they linked the pinned Skia key'
Assert-True ($PackagerSource.Contains('$rustTarget = Join-Path $stage ''.rust-target''')) 'release builds must default to an isolated, discarded Cargo target directory'
Assert-True ($PackagerSource.Contains('if (-not $persistentRustTarget) {')) 'only the isolated Cargo target directory may be deleted after packaging'
Assert-True ($PackagerSource.Contains('CargoTargetDirectory must lie outside the package and its stage')) 'a persistent Cargo target directory must never become package content'
Assert-True ($PackagerSource.Contains("if (`$keys.Count -ne 1) { throw")) 'a persistent target directory must fail closed when more than one Skia build output exists'
foreach ($skiaArchitecture in @(@('x64', 'x86_64-pc-windows-msvc'), @('x86', 'i686-pc-windows-msvc'), @('arm64', 'aarch64-pc-windows-msvc'))) {
    $skiaManifest = Get-Content -Raw -LiteralPath (Join-Path $RepositoryRoot "packaging\windows-$($skiaArchitecture[0])\versions.json") | ConvertFrom-Json
    $skiaPin = $skiaManifest.build.skiaBinaries
    Assert-True ([string]$skiaPin.key -cmatch "^[0-9a-f]{20}-$($skiaArchitecture[1])-d3d-gl-jpegd-jpege-pdf-textlayout-static$") "windows-$($skiaArchitecture[0]) must pin a static-CRT Direct3D Skia build"
    Assert-True ([string]$skiaPin.sha256 -cmatch '^[0-9a-f]{64}$') "windows-$($skiaArchitecture[0]) Skia binaries need a SHA-256 pin"
    Assert-True ([string]$skiaPin.source.commit -cmatch '^[0-9a-f]{40}$') "windows-$($skiaArchitecture[0]) Skia source needs a full commit pin"
}
Assert-True ($PackagerSource.IndexOf('$PrivilegedBrokerReleaseReady = $true', [StringComparison]::Ordinal) -lt $PackagerSource.IndexOf('function Invoke-PinnedRustBuild', [StringComparison]::Ordinal)) 'release gate must be evaluated before Rust builds or downloads'
# Mihomo is the patched build of packaging/mihomo-patches, pinned identically in the recipe, every
# versions.json and the packager's immutable tuple; the stock download is gone from production.
$MihomoRecipe = Get-Content -Raw -LiteralPath (Join-Path $RepositoryRoot 'packaging\mihomo-patches\manifest.json') | ConvertFrom-Json
foreach ($patch in $MihomoRecipe.patches) {
    Assert-True ((Get-FileHash -LiteralPath (Join-Path $RepositoryRoot "packaging\mihomo-patches\$($patch.file)") -Algorithm SHA256).Hash.ToLowerInvariant() -ceq [string]$patch.sha256) "Mihomo patch $($patch.file) must match its recipe pin"
}
foreach ($flavour in @(@('windows-x64', 'windows-amd64'), @('windows-x86', 'windows-386'), @('windows-arm64', 'windows-arm64'), @('linux', 'linux-amd64'))) {
    $flavourMihomo = (Get-Content -Raw -LiteralPath (Join-Path $RepositoryRoot "packaging\$($flavour[0])\versions.json") | ConvertFrom-Json).components.mihomo
    Assert-True (-not ($flavourMihomo.PSObject.Properties.Name -contains 'asset')) "$($flavour[0]) must not pin a stock Mihomo download"
    $recipeTarget = $MihomoRecipe.targets.($flavour[1])
    Assert-True ([string]$flavourMihomo.build.target -ceq $flavour[1]) "$($flavour[0]) must pin the $($flavour[1]) Mihomo build"
    Assert-True ([string]$flavourMihomo.build.series -ceq [string]$MihomoRecipe.series) "$($flavour[0]) Mihomo series must match the recipe"
    Assert-True ([string]$flavourMihomo.build.file -ceq [string]$recipeTarget.file) "$($flavour[0]) Mihomo file must match the recipe"
    Assert-True ([string]$recipeTarget.sha256 -cmatch '^[0-9a-f]{64}$' -and [string]$flavourMihomo.build.executableSha256 -ceq [string]$recipeTarget.sha256) "$($flavour[0]) Mihomo SHA-256 must match the recipe"
    Assert-True ([string]$flavourMihomo.commit -ceq [string]$MihomoRecipe.upstream.commit -and [string]$flavourMihomo.version -ceq [string]$MihomoRecipe.upstream.version) "$($flavour[0]) Mihomo upstream must match the recipe"
    if ($flavour[0] -cne 'linux') {
        Assert-True ([string]$flavourMihomo.source.sha256 -ceq [string]$MihomoRecipe.upstream.source.sha256) "$($flavour[0]) Mihomo source must match the recipe"
        Assert-True ($PackagerSource.Contains("'$($recipeTarget.file)', '$($recipeTarget.sha256)'")) "packager must pin the $($flavour[1]) Mihomo build in its immutable tuple"
    }
}
Assert-True (-not $PackagerSource.Contains('mihomo-windows-amd64-compatible')) 'packager must not keep the stock Mihomo download pin'
foreach ($required in @('README.md', 'THIRD_PARTY_NOTICES.md', 'licenses\Xray-core-MPL-2.0.txt', 'licenses\mihomo-GPL-3.0.txt')) {
    Assert-True (Test-Path -LiteralPath (Join-Path $MetadataRoot $required)) "metadata file $required must exist"
}

$TestRoot = Join-Path ([IO.Path]::GetTempPath()) ("multicore-package-test-{0}" -f [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $TestRoot | Out-Null
try {
    $Desktop = Join-Path $TestRoot 'multicore-desktop.exe'; New-TestPe $Desktop
    $Daemon = Join-Path $TestRoot 'multicore-daemon.exe'; New-TestPe $Daemon
    $CoreHost = Join-Path $TestRoot 'multicore-core-host.exe'; New-TestPe $CoreHost
    $Updater = Join-Path $TestRoot 'multicore-apply.exe'; New-TestPe $Updater
    $xray = Join-Path $TestRoot 'xray.exe'; New-TestPe $xray
    $mihomo = Join-Path $TestRoot 'mihomo.exe'; New-TestPe $mihomo
    $xrayBytes = [IO.File]::ReadAllBytes($xray); $mihomoBytes = [IO.File]::ReadAllBytes($mihomo)
    $BaseInput = Join-Path $TestRoot 'base-input'; New-Item -ItemType Directory -Path $BaseInput | Out-Null
    New-TestZip (Join-Path $BaseInput 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = $xrayBytes; Attributes = $null })
    New-TestZip (Join-Path $BaseInput 'mihomo-fixture.zip') @(@{ Name = 'mihomo.exe'; Data = $mihomoBytes; Attributes = $null })
    Copy-Item -LiteralPath (Join-Path $MetadataRoot 'licenses\Xray-core-MPL-2.0.txt') -Destination $BaseInput
    Copy-Item -LiteralPath (Join-Path $MetadataRoot 'licenses\mihomo-GPL-3.0.txt') -Destination $BaseInput
    $xrayHash = Get-Sha256 $xray; $mihomoHash = Get-Sha256 $mihomo
    $manifest = Join-Path $TestRoot 'versions.json'
    Write-TestManifest $manifest $BaseInput $xrayHash $mihomoHash

    $releaseReadyOutput = (& $Packager -AssertReleaseReady | Out-String).Trim()
    Assert-True ($releaseReadyOutput -ceq 'PASS: privileged broker release gate is open') 'production release gate must report ready after broker Tasks 2-4 are complete'

    $productionOfflineProbeError = $null
    try {
        & $Packager -DestinationPath (Join-Path $TestRoot 'production-offline-probe') -OfflineSourceDirectory $BaseInput -DesktopExecutablePath $Desktop -DaemonExecutablePath $Daemon -UpdaterExecutablePath $Updater | Out-Null
    } catch {
        $productionOfflineProbeError = $_.Exception.Message
    }
    Assert-True ($productionOfflineProbeError -ceq 'Production packaging builds all Rust executables internally; executable overrides are fixture-only') 'offline inputs must not allow executable overrides with the production manifest'

    # Production packaging refuses to start (before any Rust build or download) without the
    # pinned patched Mihomo, and with a Mihomo whose bytes differ from the pin.
    $savedMihomoDirectory = $env:MULTICORE_MIHOMO_BINARIES_DIR
    Remove-Item Env:MULTICORE_MIHOMO_BINARIES_DIR -ErrorAction SilentlyContinue
    try {
        $mihomoProbeError = $null
        try { & $Packager -DestinationPath (Join-Path $TestRoot 'production-no-mihomo') | Out-Null } catch { $mihomoProbeError = $_.Exception.Message }
        Assert-True ([string]$mihomoProbeError).StartsWith('Packaging needs the patched Mihomo build') "production packaging must require the patched Mihomo build (got: $mihomoProbeError)"
        $tamperedMihomo = Join-Path $TestRoot 'tampered-mihomo'; New-Item -ItemType Directory -Path $tamperedMihomo | Out-Null
        Copy-Item -LiteralPath $mihomo -Destination (Join-Path $tamperedMihomo 'mihomo-windows-amd64.exe')
        $mihomoProbeError = $null
        try { & $Packager -DestinationPath (Join-Path $TestRoot 'production-tampered-mihomo') -MihomoBinariesDirectory $tamperedMihomo | Out-Null } catch { $mihomoProbeError = $_.Exception.Message }
        Assert-True ([string]$mihomoProbeError).StartsWith('SHA-256 mismatch for patched mihomo executable') "production packaging must refuse a Mihomo that differs from its pin (got: $mihomoProbeError)"
        Assert-True (-not (Test-Path -LiteralPath (Join-Path $TestRoot 'production-tampered-mihomo'))) 'a refused Mihomo must not publish'
        Assert-NoStageResidue $TestRoot 'production Mihomo refusal'
    } finally {
        if ($null -ne $savedMihomoDirectory) { $env:MULTICORE_MIHOMO_BINARIES_DIR = $savedMihomoDirectory }
    }

    # A fixture manifest that pins Mihomo as a local build stages exactly that binary.
    $mihomoBuildDirectory = Join-Path $TestRoot 'mihomo-build'; New-Item -ItemType Directory -Path $mihomoBuildDirectory | Out-Null
    Copy-Item -LiteralPath $mihomo -Destination (Join-Path $mihomoBuildDirectory 'mihomo-fixture.exe')
    $buildManifest = Join-Path $TestRoot 'versions-mihomo-build.json'
    Write-TestManifest $buildManifest $BaseInput $xrayHash $mihomoHash -MihomoBuild
    $buildOut = Join-Path $TestRoot 'package-mihomo-build'
    Invoke-Package $buildOut $BaseInput $buildManifest $Desktop $Daemon $CoreHost -MihomoBinaries $mihomoBuildDirectory | Out-Null
    Assert-True ((Get-Sha256 (Join-Path $buildOut 'cores\mihomo.exe')) -ceq $mihomoHash) 'the built Mihomo must be staged unmodified'
    foreach ($mihomoCase in @(
            @{ Name = 'mihomo-build-wrong-hash'; Hash = ('0' * 64); Directory = $mihomoBuildDirectory; Both = $false },
            @{ Name = 'mihomo-build-missing-directory'; Hash = $mihomoHash; Directory = $null; Both = $false },
            @{ Name = 'mihomo-build-empty-directory'; Hash = $mihomoHash; Directory = (Join-Path $TestRoot 'tampered-mihomo'); Both = $false },
            @{ Name = 'mihomo-build-and-asset'; Hash = $mihomoHash; Directory = $mihomoBuildDirectory; Both = $true })) {
        $caseManifest = Join-Path $TestRoot "$($mihomoCase.Name).json"
        Write-TestManifest $caseManifest $BaseInput $xrayHash $mihomoCase.Hash -MihomoBuild -MihomoBuildAndAsset:$mihomoCase.Both
        $caseOut = Join-Path $TestRoot $mihomoCase.Name
        $failed = $false
        try { Invoke-Package $caseOut $BaseInput $caseManifest $Desktop $Daemon $CoreHost -MihomoBinaries $mihomoCase.Directory | Out-Null } catch { $failed = $true }
        Assert-True $failed "$($mihomoCase.Name) must fail"
        Assert-True (-not (Test-Path -LiteralPath $caseOut)) "$($mihomoCase.Name) must not publish"
        Assert-NoStageResidue $TestRoot $mihomoCase.Name
    }

    $out1 = Join-Path $TestRoot 'package-one'; $out2 = Join-Path $TestRoot 'package-two'
    $releaseAsset = Join-Path $TestRoot 'multicore-windows-x64.zip'
    Invoke-Package $out1 $BaseInput $manifest $Desktop $Daemon $CoreHost $releaseAsset | Out-Null
    Invoke-Package $out2 $BaseInput $manifest $Desktop $Daemon $CoreHost | Out-Null
    [string[]]$expected = @('MultiCore.exe', 'README.md', 'SHA256SUMS.txt', 'THIRD_PARTY_NOTICES.md', 'cores/mihomo.exe', 'cores/xray.exe', 'licenses/Xray-core-MPL-2.0.txt', 'licenses/mihomo-GPL-3.0.txt', 'runtime/multicore-core-host.exe', 'runtime/multicore-daemon.exe', 'runtime/multicore-updater.exe', 'versions.json')
    $actualItems = @(Get-ChildItem -LiteralPath $out1 -Force -Recurse)
    foreach ($item in $actualItems) { Assert-True (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "published inventory must not contain reparse entries: $($item.FullName)" }
    [string[]]$actual = @($actualItems | Where-Object { -not $_.PSIsContainer } | ForEach-Object { $_.FullName.Substring($out1.Length + 1).Replace('\', '/') })
    [Array]::Sort($actual, [StringComparer]::Ordinal)
    Assert-True (($actual -join '|') -ceq ($expected -join '|')) 'published inventory must be exact'
    [string[]]$expectedDirectories = @('cores', 'licenses', 'runtime')
    [string[]]$actualDirectories = @($actualItems | Where-Object { $_.PSIsContainer } | ForEach-Object { $_.FullName.Substring($out1.Length + 1).Replace('\', '/') })
    [Array]::Sort($actualDirectories, [StringComparer]::Ordinal)
    Assert-True (($actualDirectories -join '|') -ceq ($expectedDirectories -join '|')) 'published directory inventory must reject extra empty directories'
    Assert-True ((Get-Sha256 (Join-Path $out1 'MultiCore.exe')) -eq (Get-Sha256 $Desktop)) 'desktop executable must be renamed without modification'
    Assert-True ((Get-Sha256 (Join-Path $out1 'runtime\multicore-daemon.exe')) -eq (Get-Sha256 $Daemon)) 'daemon hash must match'
    Assert-True ((Get-Sha256 (Join-Path $out1 'runtime\multicore-updater.exe')) -eq (Get-Sha256 $Updater)) 'updater hash must match'
    Assert-True ((Get-Sha256 (Join-Path $out1 'runtime\multicore-core-host.exe')) -eq (Get-Sha256 $CoreHost)) 'core-host hash must match'
    Assert-True ((Get-Sha256 (Join-Path $out1 'cores\xray.exe')) -eq $xrayHash) 'Xray hash must match'
    Assert-True ((Get-Sha256 (Join-Path $out1 'cores\mihomo.exe')) -eq $mihomoHash) 'Mihomo hash must match'
    foreach ($relative in $expected) {
        Assert-True ((Get-Sha256 (Join-Path $out1 $relative)) -eq (Get-Sha256 (Join-Path $out2 $relative))) "deterministic content mismatch: $relative"
    }
    $sumLines = Get-Content -LiteralPath (Join-Path $out1 'SHA256SUMS.txt')
    Assert-True (-not ($sumLines -match 'SHA256SUMS.txt')) 'SHA256SUMS must exclude itself'
    Assert-True ($sumLines.Count -eq 11) 'SHA256SUMS must cover every other file'
    Assert-True (-not ($sumLines -match [regex]::Escape($TestRoot))) 'SHA256SUMS must not contain host paths'
    $sumPaths = New-Object System.Collections.Generic.List[string]
    foreach ($line in $sumLines) {
        Assert-True ($line -cmatch '^([0-9a-f]{64}) \*(.+)$') "invalid SHA256SUMS line: $line"
        $relative = $Matches[2]
        $sumPaths.Add($relative)
        Assert-True ((Get-Sha256 (Join-Path $out1 $relative)) -ceq $Matches[1]) "SHA256SUMS digest mismatch: $relative"
    }
    [string[]]$expectedSumPaths = @('MultiCore.exe', 'README.md', 'THIRD_PARTY_NOTICES.md', 'cores/mihomo.exe', 'cores/xray.exe', 'licenses/Xray-core-MPL-2.0.txt', 'licenses/mihomo-GPL-3.0.txt', 'runtime/multicore-core-host.exe', 'runtime/multicore-daemon.exe', 'runtime/multicore-updater.exe', 'versions.json')
    Assert-True (($sumPaths -join '|') -ceq ($expectedSumPaths -join '|')) 'SHA256SUMS paths must be complete and ordered'

    Assert-True (Test-Path -LiteralPath $releaseAsset -PathType Leaf) 'release ZIP asset must be created when requested'
    $zip = [IO.Compression.ZipFile]::OpenRead($releaseAsset)
    try {
        [string[]]$zipNames = @($zip.Entries | ForEach-Object { $_.FullName })
        [Array]::Sort($zipNames, [StringComparer]::Ordinal)
        Assert-True (($zipNames -join '|') -ceq ($expected -join '|')) 'release ZIP inventory must exactly match the portable package'
        foreach ($entry in $zip.Entries) {
            $stream = $entry.Open()
            try {
                $sha = [Security.Cryptography.SHA256]::Create()
                try { $entryHash = ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() } finally { $sha.Dispose() }
            } finally { $stream.Dispose() }
            Assert-True ($entryHash -ceq (Get-Sha256 (Join-Path $out1 $entry.FullName.Replace('/', '\')))) "release ZIP content mismatch: $($entry.FullName)"
        }
    } finally { $zip.Dispose() }

    $customOnlineDestination = Join-Path $TestRoot 'custom-online-output'
    $failed = $false
    try { & $Packager -DestinationPath $customOnlineDestination -ManifestPath $manifest -DesktopExecutablePath $Desktop -DaemonExecutablePath $Daemon -UpdaterExecutablePath $Updater -CoreHostExecutablePath $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'online mode must reject every custom manifest before network access'
    Assert-True (-not (Test-Path -LiteralPath $customOnlineDestination)) 'custom online manifest rejection must not publish'
    Assert-NoStageResidue $TestRoot 'custom online manifest rejection'

    $persistentTargetDestination = Join-Path $TestRoot 'fixture-persistent-target-output'
    $persistentTargetParameters = @{
        DestinationPath = $persistentTargetDestination; OfflineSourceDirectory = $BaseInput; ManifestPath = $manifest
        DesktopExecutablePath = $Desktop; DaemonExecutablePath = $Daemon; CoreHostExecutablePath = $CoreHost; UpdaterExecutablePath = $Updater
        FixtureContractMode = $true; CargoTargetDirectory = (Join-Path $TestRoot 'persistent-target')
    }
    $failed = $false
    try { & $Packager @persistentTargetParameters | Out-Null } catch { $failed = $true }
    Assert-True $failed 'a persistent Cargo target directory must be rejected outside production packaging'
    Assert-True (-not (Test-Path -LiteralPath $persistentTargetDestination)) 'fixture persistent-target rejection must not publish'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $TestRoot 'persistent-target'))) 'fixture persistent-target rejection must not create the target directory'
    Assert-NoStageResidue $TestRoot 'fixture persistent-target rejection'

    $malformedDesktop = Join-Path $TestRoot 'malformed-desktop.exe'; New-TestPe $malformedDesktop -Malformed
    $malformedDesktopDestination = Join-Path $TestRoot 'malformed-desktop-output'
    $failed = $false
    try { Invoke-Package $malformedDesktopDestination $BaseInput $manifest $malformedDesktop $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'malformed staged desktop executable must be rejected'
    Assert-True (-not (Test-Path -LiteralPath $malformedDesktopDestination)) 'malformed staged desktop failure must not publish'
    Assert-NoStageResidue $TestRoot 'malformed staged desktop rejection'

    Assert-PackageFailure 'bad-archive-hash' { param($i, $m) Write-TestManifest $m $i $xrayHash $mihomoHash -XrayArchiveHash ('0' * 64) }
    Assert-PackageFailure 'bad-executable-hash' { param($i, $m) Write-TestManifest $m $i ('0' * 64) $mihomoHash }

    $unsafeNames = @('../evil', '/absolute', '\\server\share\evil', '\\?\C:\evil', 'C:\evil', 'xray.exe:stream', 'dir.\evil', 'CON\evil')
    for ($n = 0; $n -lt $unsafeNames.Count; $n++) {
        $unsafe = $unsafeNames[$n]
        Assert-PackageFailure "unsafe-name-$n" {
            param($i, $m)
            Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
            New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = $xrayBytes; Attributes = $null }, @{ Name = $unsafe; Data = [byte[]](1); Attributes = $null })
            Write-TestManifest $m $i $xrayHash $mihomoHash
        }
    }
    Assert-PackageFailure 'duplicate-entry' {
        param($i, $m)
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = $xrayBytes; Attributes = $null }, @{ Name = 'XRAY.EXE'; Data = $xrayBytes; Attributes = $null })
        Write-TestManifest $m $i $xrayHash $mihomoHash
    }
    Assert-PackageFailure 'symlink-entry' {
        param($i, $m)
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = $xrayBytes; Attributes = ([int64]0xA1FF0000) })
        Write-TestManifest $m $i $xrayHash $mihomoHash
    }
    Assert-PackageFailure 'oversized-entry' { param($i, $m) Write-TestManifest $m $i $xrayHash $mihomoHash -MaximumExpandedBytes 512 }

    foreach ($peCase in @(@('malformed-pe', 0x8664, $false, $true), @('x86-pe', 0x014c, $false, $false), @('arm64-pe', 0xaa64, $false, $false), @('dll-pe', 0x8664, $true, $false))) {
        $caseName = $peCase[0]; $machine = [uint16]$peCase[1]; $isDll = [bool]$peCase[2]; $malformed = [bool]$peCase[3]
        Assert-PackageFailure $caseName {
            param($i, $m)
            $bad = Join-Path $TestRoot "$caseName.exe"
            New-TestPe $bad -Machine $machine -Dll:$isDll -Malformed:$malformed
            $badBytes = [IO.File]::ReadAllBytes($bad)
            Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
            New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = $badBytes; Attributes = $null })
            Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
        }
    }
    Assert-PackageFailure 'headers-too-small-pe' {
        param($i, $m)
        $bad = Join-Path $TestRoot 'headers-too-small.exe'; New-TestPe $bad -SizeOfHeaders 1
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($bad); Attributes = $null })
        Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
    }
    Assert-PackageFailure 'bad-file-alignment-pe' {
        param($i, $m)
        $bad = Join-Path $TestRoot 'bad-file-alignment.exe'; New-TestPe $bad -FileAlignment 0x0300
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($bad); Attributes = $null })
        Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
    }
    Assert-PackageFailure 'bad-section-alignment-pe' {
        param($i, $m)
        $bad = Join-Path $TestRoot 'bad-section-alignment.exe'; New-TestPe $bad -SectionAlignment 0x0800
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($bad); Attributes = $null })
        Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
    }
    Assert-PackageFailure 'unsupported-subsystem-pe' {
        param($i, $m)
        $bad = Join-Path $TestRoot 'unsupported-subsystem.exe'; New-TestPe $bad -Subsystem 7
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($bad); Attributes = $null })
        Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
    }
    Assert-PackageFailure 'bad-loader-bounds-pe' {
        param($i, $m)
        $bad = Join-Path $TestRoot 'bad-loader-bounds.exe'; New-TestPe $bad -StackReserve 0x1000 -StackCommit 0x2000
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($bad); Attributes = $null })
        Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
    }

    $existing = Join-Path $TestRoot 'existing-output'; New-Item -ItemType Directory -Path $existing | Out-Null
    Set-Content -LiteralPath (Join-Path $existing 'keep.txt') -Value 'untouched' -NoNewline
    $failed = $false
    try { Invoke-Package $existing $BaseInput $manifest $Desktop $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'existing destination must be rejected'
    Assert-True ((Get-Content -Raw -LiteralPath (Join-Path $existing 'keep.txt')) -ceq 'untouched') 'existing destination must be untouched'
    Assert-NoStageResidue $TestRoot 'existing destination rejection'

    $junctionTarget = Join-Path $TestRoot 'junction-target'; New-Item -ItemType Directory -Path $junctionTarget | Out-Null
    $junction = Join-Path $TestRoot 'junction-parent'
    cmd.exe /d /c "mklink /J `"$junction`" `"$junctionTarget`"" | Out-Null
    Assert-True (Test-Path -LiteralPath $junction) 'junction fixture must be created'
    $junctionDestination = Join-Path $junction 'output'
    $failed = $false
    try { Invoke-Package $junctionDestination $BaseInput $manifest $Desktop $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'reparse-point destination ancestor must be rejected'
    Assert-True (-not (Test-Path -LiteralPath $junctionDestination)) 'reparse ancestor failure must not publish'
    Assert-NoStageResidue $junctionTarget 'destination reparse ancestor rejection'

    $inputJunctionTarget = Join-Path $TestRoot 'input-junction-target'; New-Item -ItemType Directory -Path $inputJunctionTarget | Out-Null
    Copy-Item -LiteralPath $Desktop -Destination (Join-Path $inputJunctionTarget 'desktop.exe')
    $inputJunction = Join-Path $TestRoot 'input-junction'
    cmd.exe /d /c "mklink /J `"$inputJunction`" `"$inputJunctionTarget`"" | Out-Null
    Assert-True (Test-Path -LiteralPath $inputJunction) 'input junction fixture must be created'
    $inputJunctionDestination = Join-Path $TestRoot 'input-junction-output'
    $failed = $false
    try { & $Packager -DestinationPath $inputJunctionDestination -OfflineSourceDirectory $BaseInput -ManifestPath $manifest -DesktopExecutablePath (Join-Path $inputJunction 'desktop.exe') -DaemonExecutablePath $Daemon -UpdaterExecutablePath $Updater -CoreHostExecutablePath $CoreHost -FixtureContractMode | Out-Null } catch { $failed = $true }
    Assert-True $failed 'reparse-point input ancestor must be rejected'
    Assert-True (-not (Test-Path -LiteralPath $inputJunctionDestination)) 'input reparse ancestor failure must not publish'
    Assert-NoStageResidue $TestRoot 'input reparse ancestor rejection'

    # --- Optional vendored dashboard: staging, CSP patch, and failure atomicity ---
    function New-DashboardFixtureIndexHtml {
        param([string]$Title = 'fixture')
        $lines = @(
            '<!doctype html>',
            '<html>',
            '  <head>',
            '    <meta charset="UTF-8" />',
            "    <title>$Title</title>",
            '  </head>',
            '  <body>',
            '    <script>',
            '      ;(function () {',
            "        console.log('fixture-theme')",
            '      })()',
            '    </script>',
            '  </body>',
            '</html>'
        )
        ($lines -join "`n") + "`n"
    }

    function Write-DashboardManifest {
        param(
            [string]$Path, [string]$InputDirectory, [string]$XrayExecutableHash, [string]$MihomoExecutableHash,
            [string]$DashboardArchiveName = 'dashboard-fixture.zip', [string]$DashboardArchiveHash, [string]$IndexHtmlSha256,
            [string]$RootPrefix = 'dist/', [int64]$MaximumEntryBytes = 1048576, [int64]$MaximumExpandedBytes = 4194304, [int]$MaximumEntryCount = 16,
            [string]$ContentSecurityPolicy = "default-src 'self'; script-src 'self'"
        )
        if (-not $DashboardArchiveHash) { $DashboardArchiveHash = Get-Sha256 (Join-Path $InputDirectory $DashboardArchiveName) }
        $manifest = [ordered]@{
            schemaVersion = 1
            fixtureContract = 'multicore-package-test-v1'
            platform = 'windows-x64'
            build = [ordered]@{ rustToolchain = '1.98.1'; desktopPackage = 'multicore-desktop'; daemonPackage = 'multicore-daemon'; updaterPackage = 'multicore-updater'; coreHostPackage = 'multicore-core-host' }
            components = [ordered]@{
                xray = [ordered]@{
                    project = 'XTLS/Xray-core'; version = 'fixture'; commit = ('1' * 40); license = 'MPL-2.0'
                    asset = [ordered]@{ name = 'xray-fixture.zip'; url = 'https://example.invalid/xray-fixture.zip'; sha256 = (Get-Sha256 (Join-Path $InputDirectory 'xray-fixture.zip')); entry = 'xray.exe'; executableSha256 = $XrayExecutableHash; maximumExpandedBytes = 1048576 }
                    licenseFile = [ordered]@{ name = 'Xray-core-MPL-2.0.txt'; url = 'https://example.invalid/LICENSE'; sha256 = (Get-Sha256 (Join-Path $InputDirectory 'Xray-core-MPL-2.0.txt')) }
                    source = [ordered]@{ url = 'https://example.invalid/source'; sha256 = ('2' * 64) }
                }
                mihomo = [ordered]@{
                    project = 'MetaCubeX/mihomo'; version = 'fixture'; commit = ('3' * 40); license = 'GPL-3.0-only'
                    asset = [ordered]@{ name = 'mihomo-fixture.zip'; url = 'https://example.invalid/mihomo-fixture.zip'; sha256 = (Get-Sha256 (Join-Path $InputDirectory 'mihomo-fixture.zip')); entry = 'mihomo.exe'; executableSha256 = $MihomoExecutableHash; maximumExpandedBytes = 1048576 }
                    licenseFile = [ordered]@{ name = 'mihomo-GPL-3.0.txt'; url = 'https://example.invalid/LICENSE'; sha256 = (Get-Sha256 (Join-Path $InputDirectory 'mihomo-GPL-3.0.txt')) }
                    source = [ordered]@{ url = 'https://example.invalid/source'; sha256 = ('4' * 64) }
                }
                dashboard = [ordered]@{
                    project = 'example/dashboard-fixture'; version = 'fixture'; commit = ('5' * 40); license = 'MIT'
                    asset = [ordered]@{
                        name = $DashboardArchiveName; url = 'https://example.invalid/dashboard-fixture.zip'; sha256 = $DashboardArchiveHash
                        rootPrefix = $RootPrefix; maximumEntryBytes = $MaximumEntryBytes; maximumExpandedBytes = $MaximumExpandedBytes; maximumEntryCount = $MaximumEntryCount
                        indexHtmlSha256 = $IndexHtmlSha256
                    }
                    licenseFile = [ordered]@{ name = 'dashboard-fixture-LICENSE.txt'; url = 'https://example.invalid/LICENSE'; sha256 = (Get-Sha256 (Join-Path $InputDirectory 'dashboard-fixture-LICENSE.txt')) }
                    contentSecurityPolicy = $ContentSecurityPolicy
                }
            }
        }
        $manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $Path -Encoding UTF8
    }

    $DashboardBaseInput = Join-Path $TestRoot 'dashboard-base-input'
    New-Item -ItemType Directory -Path $DashboardBaseInput | Out-Null
    Get-ChildItem -LiteralPath $BaseInput | Copy-Item -Destination $DashboardBaseInput -Recurse
    $dashboardIndexHtml = New-DashboardFixtureIndexHtml
    $dashboardIndexBytes = [Text.Encoding]::UTF8.GetBytes($dashboardIndexHtml)
    $dashboardIndexSha256 = (Get-FileHash -InputStream (New-Object IO.MemoryStream(, $dashboardIndexBytes)) -Algorithm SHA256).Hash.ToLowerInvariant()
    New-TestZip (Join-Path $DashboardBaseInput 'dashboard-fixture.zip') @(
        @{ Name = 'dist/'; Data = [byte[]]@(); Attributes = $null },
        @{ Name = 'dist/index.html'; Data = $dashboardIndexBytes; Attributes = $null },
        @{ Name = 'dist/assets/app.js'; Data = [Text.Encoding]::UTF8.GetBytes("console.log('fixture asset')`n"); Attributes = $null }
    )
    Set-Content -LiteralPath (Join-Path $DashboardBaseInput 'dashboard-fixture-LICENSE.txt') -Value 'MIT fixture license' -NoNewline

    function Invoke-DashboardPackage {
        param([string]$Destination, [string]$InputDirectory, [string]$Manifest)
        & $Packager -DestinationPath $Destination -OfflineSourceDirectory $InputDirectory -ManifestPath $Manifest -DesktopExecutablePath $Desktop -DaemonExecutablePath $Daemon -UpdaterExecutablePath $Updater -CoreHostExecutablePath $CoreHost -FixtureContractMode
    }

    function Assert-DashboardPackageFailure {
        param([string]$Name, [scriptblock]$Arrange)
        $caseRoot = Join-Path $TestRoot $Name
        New-Item -ItemType Directory -Path $caseRoot | Out-Null
        $input = Join-Path $caseRoot 'input'; New-Item -ItemType Directory -Path $input | Out-Null
        Get-ChildItem -LiteralPath $DashboardBaseInput | Copy-Item -Destination $input -Recurse
        $manifest = Join-Path $caseRoot 'versions.json'
        & $Arrange $input $manifest
        $destination = Join-Path $caseRoot 'output'
        $failed = $false
        try { Invoke-DashboardPackage $destination $input $manifest | Out-Null } catch { $failed = $true }
        Assert-True $failed "$Name must fail"
        Assert-True (-not (Test-Path -LiteralPath $destination)) "$Name must not publish a partial destination"
        Assert-NoStageResidue $caseRoot $Name
    }

    $dashboardManifest = Join-Path $TestRoot 'dashboard-versions.json'
    Write-DashboardManifest $dashboardManifest $DashboardBaseInput $xrayHash $mihomoHash -IndexHtmlSha256 $dashboardIndexSha256

    $dashboardOut1 = Join-Path $TestRoot 'dashboard-package-one'
    $dashboardOut2 = Join-Path $TestRoot 'dashboard-package-two'
    Invoke-DashboardPackage $dashboardOut1 $DashboardBaseInput $dashboardManifest | Out-Null
    Invoke-DashboardPackage $dashboardOut2 $DashboardBaseInput $dashboardManifest | Out-Null

    [string[]]$expectedDashboardFiles = @('MultiCore.exe', 'README.md', 'SHA256SUMS.txt', 'THIRD_PARTY_NOTICES.md',
        'cores/mihomo.exe', 'cores/xray.exe',
        'dashboard/assets/app.js', 'dashboard/index.html', 'dashboard/multicore-readonly.js', 'dashboard/theme-favicon.js',
        'licenses/Xray-core-MPL-2.0.txt', 'licenses/dashboard-fixture-LICENSE.txt', 'licenses/mihomo-GPL-3.0.txt',
        'runtime/multicore-core-host.exe', 'runtime/multicore-daemon.exe', 'runtime/multicore-updater.exe', 'versions.json')
    [Array]::Sort($expectedDashboardFiles, [StringComparer]::Ordinal)
    $dashboardActualItems = @(Get-ChildItem -LiteralPath $dashboardOut1 -Force -Recurse)
    foreach ($item in $dashboardActualItems) { Assert-True (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "published dashboard inventory must not contain reparse entries: $($item.FullName)" }
    [string[]]$dashboardActualFiles = @($dashboardActualItems | Where-Object { -not $_.PSIsContainer } | ForEach-Object { $_.FullName.Substring($dashboardOut1.Length + 1).Replace('\', '/') })
    [Array]::Sort($dashboardActualFiles, [StringComparer]::Ordinal)
    Assert-True (($dashboardActualFiles -join '|') -ceq ($expectedDashboardFiles -join '|')) "dashboard-enabled published inventory must be exact: $($dashboardActualFiles -join ', ')"
    [string[]]$expectedDashboardDirectories = @('cores', 'dashboard', 'dashboard/assets', 'licenses', 'runtime')
    [Array]::Sort($expectedDashboardDirectories, [StringComparer]::Ordinal)
    [string[]]$dashboardActualDirectories = @($dashboardActualItems | Where-Object { $_.PSIsContainer } | ForEach-Object { $_.FullName.Substring($dashboardOut1.Length + 1).Replace('\', '/') })
    [Array]::Sort($dashboardActualDirectories, [StringComparer]::Ordinal)
    Assert-True (($dashboardActualDirectories -join '|') -ceq ($expectedDashboardDirectories -join '|')) 'dashboard-enabled published directory inventory must be exact'

    $dashboardPatchedIndex = Get-Content -Raw -LiteralPath (Join-Path $dashboardOut1 'dashboard\index.html')
    Assert-True ($dashboardPatchedIndex.Contains('Content-Security-Policy')) 'staged dashboard index.html must carry a Content-Security-Policy meta tag'
    Assert-True ($dashboardPatchedIndex.Contains("default-src 'self'; script-src 'self'")) 'staged dashboard index.html must carry the exact pinned CSP'
    Assert-True (-not $dashboardPatchedIndex.Contains('<script>')) 'staged dashboard index.html must not retain an inline <script> block'
    Assert-True ($dashboardPatchedIndex.Contains('<script src="./theme-favicon.js"></script>')) 'staged dashboard index.html must reference the externalized theme script'
    Assert-True ($dashboardPatchedIndex.Contains("<script src=`"./theme-favicon.js`"></script>`n    <script src=`"./multicore-readonly.js`"></script>")) 'staged dashboard index.html must load the read-only script right after the theme script'
    Assert-True ((Get-Sha256 (Join-Path $dashboardOut1 'dashboard\multicore-readonly.js')) -eq (Get-Sha256 (Join-Path $RepositoryRoot 'packaging\dashboard\multicore-readonly.js'))) 'staged read-only dashboard script must be byte-identical to packaging/dashboard/multicore-readonly.js'
    $themeScript = Get-Content -Raw -LiteralPath (Join-Path $dashboardOut1 'dashboard\theme-favicon.js')
    Assert-True ($themeScript.Contains("console.log('fixture-theme')")) 'externalized theme script must keep the original inline script body'

    Assert-True ((Get-Sha256 (Join-Path $dashboardOut1 'dashboard\index.html')) -eq (Get-Sha256 (Join-Path $dashboardOut2 'dashboard\index.html'))) 'dashboard CSP patch must be byte-deterministic across runs'
    Assert-True ((Get-Sha256 (Join-Path $dashboardOut1 'dashboard\assets\app.js')) -eq (Get-Sha256 (Join-Path $dashboardOut2 'dashboard\assets\app.js'))) 'unpatched dashboard assets must be staged byte-identical across runs'
    Assert-True ((Get-Sha256 (Join-Path $dashboardOut1 'licenses\dashboard-fixture-LICENSE.txt')) -eq (Get-Sha256 (Join-Path $DashboardBaseInput 'dashboard-fixture-LICENSE.txt'))) 'dashboard license must be staged unmodified'

    $sumLines = Get-Content -LiteralPath (Join-Path $dashboardOut1 'SHA256SUMS.txt')
    Assert-True ($sumLines.Count -eq 16) 'SHA256SUMS must cover every staged file when the dashboard is enabled'
    Assert-True (($sumLines -match [regex]::Escape('dashboard/index.html')).Count -eq 1) 'SHA256SUMS must record the patched dashboard index.html'

    # Negative: archive checksum mismatch.
    Assert-DashboardPackageFailure 'dashboard-bad-archive-hash' {
        param($i, $m) Write-DashboardManifest $m $i $xrayHash $mihomoHash -DashboardArchiveHash ('0' * 64) -IndexHtmlSha256 $dashboardIndexSha256
    }

    # Negative: a future re-vendor changed index.html without anyone re-running this patch --
    # this is the regression check that a silently-changed upstream index.html fails the build
    # instead of shipping without the CSP.
    Assert-DashboardPackageFailure 'dashboard-index-html-changed-upstream' {
        param($i, $m) Write-DashboardManifest $m $i $xrayHash $mihomoHash -IndexHtmlSha256 ('6' * 64)
    }

    # Negative: entry escapes the declared archive root (path-traversal style).
    Assert-DashboardPackageFailure 'dashboard-entry-outside-root' {
        param($i, $m)
        Remove-Item -LiteralPath (Join-Path $i 'dashboard-fixture.zip')
        New-TestZip (Join-Path $i 'dashboard-fixture.zip') @(
            @{ Name = 'dist/index.html'; Data = $dashboardIndexBytes; Attributes = $null },
            @{ Name = 'other/escaped.txt'; Data = [byte[]](1); Attributes = $null }
        )
        Write-DashboardManifest $m $i $xrayHash $mihomoHash -IndexHtmlSha256 $dashboardIndexSha256
    }

    # Negative: symlink entry inside the archive.
    Assert-DashboardPackageFailure 'dashboard-symlink-entry' {
        param($i, $m)
        Remove-Item -LiteralPath (Join-Path $i 'dashboard-fixture.zip')
        New-TestZip (Join-Path $i 'dashboard-fixture.zip') @(
            @{ Name = 'dist/index.html'; Data = $dashboardIndexBytes; Attributes = $null },
            @{ Name = 'dist/assets/link'; Data = [byte[]](1); Attributes = ([int64]0xA1FF0000) }
        )
        Write-DashboardManifest $m $i $xrayHash $mihomoHash -IndexHtmlSha256 $dashboardIndexSha256
    }

    # Negative: a single entry exceeds the pinned per-entry size bound.
    Assert-DashboardPackageFailure 'dashboard-oversized-entry' {
        param($i, $m) Write-DashboardManifest $m $i $xrayHash $mihomoHash -IndexHtmlSha256 $dashboardIndexSha256 -MaximumEntryBytes 8
    }

    # Negative: the archive has no index.html at its declared root.
    Assert-DashboardPackageFailure 'dashboard-missing-index-html' {
        param($i, $m)
        Remove-Item -LiteralPath (Join-Path $i 'dashboard-fixture.zip')
        New-TestZip (Join-Path $i 'dashboard-fixture.zip') @(@{ Name = 'dist/other.txt'; Data = [byte[]](1); Attributes = $null })
        Write-DashboardManifest $m $i $xrayHash $mihomoHash -IndexHtmlSha256 (Get-FileHash -InputStream (New-Object IO.MemoryStream(, [byte[]](1))) -Algorithm SHA256).Hash.ToLowerInvariant()
    }

    # Negative: a manifest that weakens the pinned CSP (adds 'unsafe-inline' to script-src) must
    # be rejected before any network access or staging, regardless of OfflineSourceDirectory.
    Assert-DashboardPackageFailure 'dashboard-weak-csp' {
        param($i, $m) Write-DashboardManifest $m $i $xrayHash $mihomoHash -IndexHtmlSha256 $dashboardIndexSha256 -ContentSecurityPolicy "default-src 'self'; script-src 'self' 'unsafe-inline'"
    }

    # The x86 flavour: PE32 i386 executables, platform windows-x86, multicore-windows-x86.zip.
    $x86Root = Join-Path $TestRoot 'x86'; New-Item -ItemType Directory -Path $x86Root | Out-Null
    $x86Exes = @{}
    foreach ($name in @('desktop', 'daemon', 'core-host', 'updater', 'xray', 'mihomo')) {
        $x86Exes[$name] = Join-Path $x86Root "$name.exe"; New-TestPe32 $x86Exes[$name]
    }
    $x86Input = Join-Path $x86Root 'input'; New-Item -ItemType Directory -Path $x86Input | Out-Null
    New-TestZip (Join-Path $x86Input 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($x86Exes['xray']); Attributes = $null })
    New-TestZip (Join-Path $x86Input 'mihomo-fixture.zip') @(@{ Name = 'mihomo.exe'; Data = [IO.File]::ReadAllBytes($x86Exes['mihomo']); Attributes = $null })
    Copy-Item -LiteralPath (Join-Path $MetadataRoot 'licenses\Xray-core-MPL-2.0.txt') -Destination $x86Input
    Copy-Item -LiteralPath (Join-Path $MetadataRoot 'licenses\mihomo-GPL-3.0.txt') -Destination $x86Input
    $x86Manifest = Join-Path $x86Root 'versions.json'
    Write-TestManifest $x86Manifest $x86Input (Get-Sha256 $x86Exes['xray']) (Get-Sha256 $x86Exes['mihomo']) -Platform 'windows-x86'
    $x86Out = Join-Path $x86Root 'package'
    $x86Asset = Join-Path $x86Root 'multicore-windows-x86.zip'
    Invoke-Package $x86Out $x86Input $x86Manifest $x86Exes['desktop'] $x86Exes['daemon'] $x86Exes['core-host'] $x86Asset 'x86' $x86Exes['updater'] | Out-Null
    Assert-True (Test-Path -LiteralPath $x86Asset -PathType Leaf) 'x86 packaging must publish multicore-windows-x86.zip'
    Assert-True ((Get-Sha256 (Join-Path $x86Out 'runtime\multicore-core-host.exe')) -eq (Get-Sha256 $x86Exes['core-host'])) 'x86 core host must be staged unmodified'
    Assert-True ((Get-Content -Raw -LiteralPath (Join-Path $x86Out 'README.md')).Contains('x86 (32-bit)')) 'x86 package must carry the x86 README'

    $x86Failures = @(
        # x86 packaging must refuse x64 executables, the x64 manifest and the x64 asset name.
        @{ Name = 'x86-with-x64-desktop'; Desktop = $Desktop; Manifest = $x86Manifest; Asset = $null },
        @{ Name = 'x86-with-x64-manifest'; Desktop = $x86Exes['desktop']; Manifest = $manifest; Asset = $null },
        @{ Name = 'x86-with-x64-asset-name'; Desktop = $x86Exes['desktop']; Manifest = $x86Manifest; Asset = (Join-Path $x86Root 'wrong\multicore-windows-x64.zip') }
    )
    foreach ($case in $x86Failures) {
        $destination = Join-Path $x86Root $case.Name
        $failed = $false
        try { Invoke-Package $destination $x86Input $case.Manifest $case.Desktop $x86Exes['daemon'] $x86Exes['core-host'] $case.Asset 'x86' $x86Exes['updater'] | Out-Null } catch { $failed = $true }
        Assert-True $failed "$($case.Name) must fail"
        Assert-True (-not (Test-Path -LiteralPath $destination)) "$($case.Name) must not publish a partial destination"
        Assert-NoStageResidue $x86Root $case.Name
    }
    # And x64 packaging must refuse x86 executables (the x86-pe case above covers a core) and the
    # x86 manifest.
    $failed = $false
    try { Invoke-Package (Join-Path $x86Root 'x64-with-x86-manifest') $x86Input $x86Manifest $Desktop $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'x64 packaging must refuse the x86 manifest'

    # The arm64 flavour: PE32+ ARM64 executables, platform windows-arm64, multicore-windows-arm64.zip.
    $arm64Root = Join-Path $TestRoot 'arm64'; New-Item -ItemType Directory -Path $arm64Root | Out-Null
    $arm64Exes = @{}
    foreach ($name in @('desktop', 'daemon', 'core-host', 'updater', 'xray', 'mihomo')) {
        $arm64Exes[$name] = Join-Path $arm64Root "$name.exe"; New-TestPe $arm64Exes[$name] -Machine 0xaa64
    }
    $arm64Input = Join-Path $arm64Root 'input'; New-Item -ItemType Directory -Path $arm64Input | Out-Null
    New-TestZip (Join-Path $arm64Input 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($arm64Exes['xray']); Attributes = $null })
    New-TestZip (Join-Path $arm64Input 'mihomo-fixture.zip') @(@{ Name = 'mihomo.exe'; Data = [IO.File]::ReadAllBytes($arm64Exes['mihomo']); Attributes = $null })
    Copy-Item -LiteralPath (Join-Path $MetadataRoot 'licenses\Xray-core-MPL-2.0.txt') -Destination $arm64Input
    Copy-Item -LiteralPath (Join-Path $MetadataRoot 'licenses\mihomo-GPL-3.0.txt') -Destination $arm64Input
    $arm64Manifest = Join-Path $arm64Root 'versions.json'
    Write-TestManifest $arm64Manifest $arm64Input (Get-Sha256 $arm64Exes['xray']) (Get-Sha256 $arm64Exes['mihomo']) -Platform 'windows-arm64'
    $arm64Out = Join-Path $arm64Root 'package'
    $arm64Asset = Join-Path $arm64Root 'multicore-windows-arm64.zip'
    Invoke-Package $arm64Out $arm64Input $arm64Manifest $arm64Exes['desktop'] $arm64Exes['daemon'] $arm64Exes['core-host'] $arm64Asset 'arm64' $arm64Exes['updater'] | Out-Null
    Assert-True (Test-Path -LiteralPath $arm64Asset -PathType Leaf) 'arm64 packaging must publish multicore-windows-arm64.zip'
    Assert-True ((Get-Sha256 (Join-Path $arm64Out 'runtime\multicore-core-host.exe')) -eq (Get-Sha256 $arm64Exes['core-host'])) 'arm64 core host must be staged unmodified'
    Assert-True ((Get-Content -Raw -LiteralPath (Join-Path $arm64Out 'README.md')).Contains('Windows ARM64')) 'arm64 package must carry the arm64 README'

    $arm64Failures = @(
        # arm64 packaging must refuse x64 executables (which Windows on ARM would only emulate),
        # x86 cores, the x64 manifest and the x64 asset name.
        @{ Name = 'arm64-with-x64-desktop'; Desktop = $Desktop; Input = $arm64Input; Manifest = $arm64Manifest; Asset = $null },
        @{ Name = 'arm64-with-x86-cores'; Desktop = $arm64Exes['desktop']; Input = $x86Input; Manifest = $null; Asset = $null },
        @{ Name = 'arm64-with-x64-manifest'; Desktop = $arm64Exes['desktop']; Input = $arm64Input; Manifest = $manifest; Asset = $null },
        @{ Name = 'arm64-with-x64-asset-name'; Desktop = $arm64Exes['desktop']; Input = $arm64Input; Manifest = $arm64Manifest; Asset = (Join-Path $arm64Root 'wrong\multicore-windows-x64.zip') }
    )
    foreach ($case in $arm64Failures) {
        $destination = Join-Path $arm64Root $case.Name
        $caseManifest = $case.Manifest
        if (-not $caseManifest) {
            # An otherwise valid arm64 manifest pinning the x86 cores of $case.Input.
            $caseManifest = Join-Path $arm64Root "$($case.Name).json"
            Write-TestManifest $caseManifest $case.Input (Get-Sha256 $x86Exes['xray']) (Get-Sha256 $x86Exes['mihomo']) -Platform 'windows-arm64'
        }
        $failed = $false
        try { Invoke-Package $destination $case.Input $caseManifest $case.Desktop $arm64Exes['daemon'] $arm64Exes['core-host'] $case.Asset 'arm64' $arm64Exes['updater'] | Out-Null } catch { $failed = $true }
        Assert-True $failed "$($case.Name) must fail"
        Assert-True (-not (Test-Path -LiteralPath $destination)) "$($case.Name) must not publish a partial destination"
        Assert-NoStageResidue $arm64Root $case.Name
    }
    # And x64 packaging must refuse ARM64 executables (the arm64-pe case above covers a core) and
    # the arm64 manifest.
    $failed = $false
    try { Invoke-Package (Join-Path $arm64Root 'x64-with-arm64-manifest') $arm64Input $arm64Manifest $Desktop $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'x64 packaging must refuse the arm64 manifest'
    $failed = $false
    try { Invoke-Package (Join-Path $arm64Root 'x64-with-arm64-desktop') $BaseInput $manifest $arm64Exes['desktop'] $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'x64 packaging must refuse an ARM64 desktop executable'

    Write-Output 'PASS: Windows release package contract and failure atomicity'
} finally {
    if (Test-Path -LiteralPath $TestRoot) { Remove-Item -LiteralPath $TestRoot -Recurse -Force }
}
