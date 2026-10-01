[CmdletBinding()]
param(
    # x64 (x86_64-pc-windows-msvc) or x86 (i686-pc-windows-msvc).
    [ValidateSet('x64', 'x86')]
    [string]$Architecture = 'x64',
    # Where skia-binaries-<key>.tar.gz is written (the directory package-windows-release.ps1
    # reads through -SkiaBinariesDirectory / MULTICORE_SKIA_BINARIES_DIR).
    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory,
    # An LLVM installation providing bin\clang-cl.exe and bin\libclang.dll (LLVM 23.1.2 was used
    # for the pinned binaries: clang+llvm-23.1.2-x86_64-pc-windows-msvc from llvm/llvm-project).
    [Parameter(Mandatory = $true)]
    [string]$LlvmHome,
    # Short scratch path: the Skia tree has deep third-party paths (MAX_PATH).
    [string]$WorkDirectory = (Join-Path $env:SystemDrive 'skb')
)

# Builds the Skia static libraries that skia-bindings links into MultiCore.exe.
#
# rust-skia publishes prebuilt Skia binaries only for the dynamic C runtime (/MD), while the
# release links the C runtime statically (+crt-static, see package-windows-release.ps1), and it
# publishes no 32-bit Windows build at all. So the Windows release links Skia built here: the
# pinned rust-skia Skia fork, built by skia-bindings itself (offline build) with clang-cl and
# packed exactly like a skia-binaries download. The archive's SHA-256 is pinned in
# packaging/windows-<arch>/versions.json (build.skiaBinaries); rebuilding produces a different
# archive (the build is not bit-reproducible), so a rebuild means updating that pin.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$Manifest = Get-Content -Raw -LiteralPath (Join-Path $RepositoryRoot "packaging\windows-$Architecture\versions.json") | ConvertFrom-Json
$Pin = $Manifest.build.skiaBinaries
if ($null -eq $Pin) { throw "packaging\windows-$Architecture\versions.json has no build.skiaBinaries pin" }
$SkiaRepository = "https://github.com/$($Pin.source.repository).git"
$SkiaTag = [string]$Pin.source.tag
$SkiaCommit = [string]$Pin.source.commit
if ($SkiaCommit -cnotmatch '^[0-9a-f]{40}$') { throw 'build.skiaBinaries.source.commit must be a full lowercase commit' }
$CargoTarget = if ($Architecture -ceq 'x86') { 'i686-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }

$LlvmHome = [IO.Path]::GetFullPath($LlvmHome)
foreach ($tool in @('bin\clang-cl.exe', 'bin\libclang.dll')) {
    if (-not (Test-Path -LiteralPath (Join-Path $LlvmHome $tool) -PathType Leaf)) { throw "LLVM installation lacks $tool`: $LlvmHome" }
}
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$WorkDirectory = [IO.Path]::GetFullPath($WorkDirectory)
New-Item -ItemType Directory -Force -Path $WorkDirectory | Out-Null

function Invoke-Checked {
    param([string]$Description, [scriptblock]$Command)
    & $Command
    if ($LASTEXITCODE -ne 0) { throw "$Description failed with exit code $LASTEXITCODE" }
}

# 1. The pinned Skia source with its third-party dependencies, gn and ninja.
$SkiaSource = Join-Path $WorkDirectory "skia-$SkiaTag"
if (-not (Test-Path -LiteralPath (Join-Path $SkiaSource '.git'))) {
    Invoke-Checked 'git clone' { git -c core.symlinks=false clone --quiet --depth 1 --branch $SkiaTag $SkiaRepository $SkiaSource }
}
$head = (git -C $SkiaSource rev-parse HEAD).Trim()
if ($head -cne $SkiaCommit) { throw "Skia checkout is at $head, expected $SkiaCommit" }
$hadSyncSkip = Test-Path Env:GIT_SYNC_DEPS_SKIP_EMSDK
$env:GIT_SYNC_DEPS_SKIP_EMSDK = '1'
try {
    Push-Location $SkiaSource
    try {
        Invoke-Checked 'git-sync-deps' { python tools/git-sync-deps }
        Invoke-Checked 'fetch-ninja' { python bin/fetch-ninja }
    } finally { Pop-Location }
} finally {
    if (-not $hadSyncSkip) { Remove-Item Env:GIT_SYNC_DEPS_SKIP_EMSDK -ErrorAction SilentlyContinue }
}
$Ninja = Join-Path $SkiaSource 'third_party\ninja\ninja.exe'
if (-not (Test-Path -LiteralPath $Ninja -PathType Leaf)) { throw "ninja was not fetched: $Ninja" }

# 2. skia-bindings' own offline build, configured exactly like the release build.
$TargetDirectory = Join-Path $WorkDirectory "target-$Architecture"
$saved = @{}
foreach ($name in @('LLVM_HOME', 'LIBCLANG_PATH', 'SKIA_SOURCE_DIR', 'SKIA_NINJA_COMMAND', 'CARGO_TARGET_DIR', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTFLAGS', 'SKIA_BINARIES_URL')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
try {
    $env:LLVM_HOME = $LlvmHome
    $env:LIBCLANG_PATH = Join-Path $LlvmHome 'bin'
    $env:SKIA_SOURCE_DIR = $SkiaSource
    $env:SKIA_NINJA_COMMAND = $Ninja
    $env:CARGO_TARGET_DIR = $TargetDirectory
    Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
    Remove-Item Env:SKIA_BINARIES_URL -ErrorAction SilentlyContinue
    $env:CARGO_ENCODED_RUSTFLAGS = '-Ctarget-feature=+crt-static'
    Push-Location $RepositoryRoot
    try {
        Invoke-Checked 'skia-bindings build' { cargo build --locked --release --target $CargoTarget -p skia-bindings }
    } finally { Pop-Location }
} finally {
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}

# 3. Pack the files skia-bindings imports from a skia-binaries download (binaries_config.rs:
#    the built libraries, bindings.rs and, with textlayout on Windows, icudtl.dat).
$built = @(Get-ChildItem -Path (Join-Path $TargetDirectory (Join-Path $CargoTarget 'release\build')) -Filter 'bindings.rs' -Recurse |
    Where-Object { $_.Directory.Name -ceq 'skia' -and $_.Directory.Parent.Name -ceq 'out' })
if ($built.Count -ne 1) { throw "Expected one skia-bindings output, found $($built.Count)" }
$skiaOut = $built[0].Directory.FullName
$key = [string]$Pin.key
if ($key -cnotmatch "^[0-9a-f]{20}-$CargoTarget-[a-z0-9-]+-static$") { throw "build.skiaBinaries.key is not a static-CRT key for $CargoTarget`: $key" }
$stage = Join-Path $WorkDirectory "pack-$Architecture"
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
$binaries = Join-Path $stage 'skia-binaries'
New-Item -ItemType Directory -Force -Path $binaries | Out-Null
foreach ($file in @('skia.lib', 'skparagraph.lib', 'skshaper.lib', 'skunicode_core.lib', 'skunicode_icu.lib', 'skia-bindings.lib', 'bindings.rs', 'icudtl.dat')) {
    $source = Join-Path $skiaOut $file
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Skia build output lacks $file" }
    Copy-Item -LiteralPath $source -Destination $binaries
}
Copy-Item -LiteralPath (Join-Path $SkiaSource 'LICENSE') -Destination (Join-Path $binaries 'LICENSE_SKIA')
[IO.File]::WriteAllText((Join-Path $binaries 'key.txt'), $key)
[IO.File]::WriteAllText((Join-Path $binaries 'tag.txt'), [string]$Pin.skiaBindings)
$archive = Join-Path $OutputDirectory "skia-binaries-$key.tar.gz"
if (Test-Path -LiteralPath $archive) { Remove-Item -LiteralPath $archive -Force }
Invoke-Checked 'tar' { tar.exe -czf $archive -C $stage skia-binaries }
$sha = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
Write-Output "Built $archive"
Write-Output "sha256 $sha (pinned: $($Pin.sha256))"
