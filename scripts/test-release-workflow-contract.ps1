[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$workflowPath = Join-Path $root '.github\workflows\release.yml'
$ciPath = Join-Path $root '.github\workflows\ci.yml'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "FAIL: $Message" }
}

Assert-True (Test-Path -LiteralPath $workflowPath -PathType Leaf) 'release workflow must exist'
Assert-True (Test-Path -LiteralPath $ciPath -PathType Leaf) 'CI workflow must exist'
$workflow = Get-Content -Raw -LiteralPath $workflowPath
$ci = Get-Content -Raw -LiteralPath $ciPath

foreach ($token in @(
    'tags:',
    "- 'v*'",
    'permissions:',
    'contents: write',
    'runs-on: windows-latest',
    'cargo test --workspace --all-targets --locked',
    'cargo clippy --workspace --all-targets --locked -- -D warnings',
    '-AssertReleaseReady',
    '-UpdateRepository fatyzzz/multicore',
    'multicore-windows-x64.zip',
    'MultiCore-Setup-x64.exe',
    'test-windows-installer-contract.ps1',
    'test-smoke-test-one-click-contract.ps1',
    '-DesktopExecutablePath dist/release/multicore-windows-x64/MultiCore.exe',
    '-DaemonExecutablePath dist/release/multicore-windows-x64/runtime/multicore-daemon.exe',
    '-UpdaterExecutablePath dist/release/multicore-windows-x64/runtime/multicore-updater.exe',
    '-CoreHostExecutablePath dist/release/multicore-windows-x64/runtime/multicore-core-host.exe',
    '$env:MULTICORE_RELEASE_BUNDLE',
    'cargo test -p multicore-release --test archive_contract built_release_bundle_matches_updater_contract --locked -- --ignored --exact',
    'smoke-test-one-click.ps1',
    '-PackagePath dist/release/multicore-windows-x64',
    '-Background',
    'build-windows-installer.ps1',
    'smoke-test-windows-installed-update.ps1',
    'Xray-core-source.zip',
    'mihomo-source.zip',
    'gh release create',
    '--draft',
    'Create unsigned preview as draft',
    'Keep this release as a draft'
)) {
    Assert-True ($workflow.Contains($token)) "release workflow is missing: $token"
}

Assert-True (-not $workflow.Contains('pull_request_target')) 'release workflow must not run in pull_request_target'
Assert-True (-not $workflow.Contains('MULTICORE_DAEMON_TOKEN')) 'release workflow must not embed runtime secrets'
Assert-True (-not $workflow.Contains('gh release edit')) 'unsigned workflow must not publish the draft release'
Assert-True (-not $workflow.Contains('--draft=false')) 'unsigned workflow must not clear draft state'
Assert-True ($ci.Contains('test-smoke-test-one-click-contract.ps1')) 'normal CI must run the one-click smoke contract'

$packageIndex = $workflow.IndexOf('-ReleaseAssetPath dist/release/multicore-windows-x64.zip', [StringComparison]::Ordinal)
$realBundleIndex = $workflow.IndexOf('$env:MULTICORE_RELEASE_BUNDLE', [StringComparison]::Ordinal)
$backgroundSmokeIndex = $workflow.IndexOf('smoke-test-one-click.ps1', [StringComparison]::Ordinal)
Assert-True ($packageIndex -ge 0 -and $realBundleIndex -gt $packageIndex) 'real updater bundle contract must run after release ZIP creation'
Assert-True ($backgroundSmokeIndex -gt $packageIndex) 'background one-click smoke must run after portable package creation'

Write-Output 'PASS: GitHub release workflow contract'
