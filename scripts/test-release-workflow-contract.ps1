[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$workflowPath = Join-Path $root '.github\workflows\release.yml'
$ciPath = Join-Path $root '.github\workflows\ci.yml'
$linuxWorkflowPath = Join-Path $root '.github\workflows\linux-packages.yml'
$linuxDockerScriptPath = Join-Path $root 'scripts\package-linux-in-docker.sh'
$linuxPackageContractPath = Join-Path $root 'scripts\test-linux-package-contract.sh'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "FAIL: $Message" }
}

Assert-True (Test-Path -LiteralPath $workflowPath -PathType Leaf) 'release workflow must exist'
Assert-True (Test-Path -LiteralPath $ciPath -PathType Leaf) 'CI workflow must exist'
Assert-True (Test-Path -LiteralPath $linuxWorkflowPath -PathType Leaf) 'Linux package workflow must exist'
Assert-True (Test-Path -LiteralPath $linuxDockerScriptPath -PathType Leaf) 'Linux Docker packager must exist'
Assert-True (Test-Path -LiteralPath $linuxPackageContractPath -PathType Leaf) 'Linux package artifact contract must exist'
$workflow = Get-Content -Raw -LiteralPath $workflowPath
$ci = Get-Content -Raw -LiteralPath $ciPath
$linuxWorkflow = Get-Content -Raw -LiteralPath $linuxWorkflowPath
$linuxDockerScript = Get-Content -Raw -LiteralPath $linuxDockerScriptPath
$cacheAction = 'actions/cache@55cc8345863c7cc4c66a329aec7e433d2d1c52a9'

foreach ($token in @(
    'tags:',
    "- 'v*'",
    'permissions:',
    'contents: read',
    'runs-on: [self-hosted, Windows, X64, windowsbuild]',
    'cargo test --workspace --locked',
    'cargo clippy --workspace --all-targets --locked -- -D warnings',
    '-AssertReleaseReady',
    'GH_TOKEN: ${{ secrets.RELEASES_TOKEN }}',
    '-UpdateRepository fatyzzz/multicore-releases',
    'multicore-windows-x64.zip',
    'MultiCore-Setup-x64.exe',
    'test-windows-installer-contract.ps1',
    'test-smoke-test-one-click-contract.ps1',
    'test-packaged-daemon-smoke-contract.ps1',
    '-DesktopExecutablePath dist/release/multicore-windows-x64/MultiCore.exe',
    '-DaemonExecutablePath dist/release/multicore-windows-x64/runtime/multicore-daemon.exe',
    '-UpdaterExecutablePath dist/release/multicore-windows-x64/runtime/multicore-updater.exe',
    '-CoreHostExecutablePath dist/release/multicore-windows-x64/runtime/multicore-core-host.exe',
    '$env:MULTICORE_RELEASE_BUNDLE',
    'cargo test -p multicore-release --test archive_contract built_release_bundle_matches_updater_contract --locked -- --ignored --exact',
    'smoke-test-packaged-daemon.ps1',
    '-PackagePath dist/release/multicore-windows-x64',
    'build-windows-installer.ps1',
    'smoke-test-windows-installed-update.ps1',
    'Xray-core-source.zip',
    'mihomo-source.zip',
    'gh release create',
    '--repo fatyzzz/multicore-releases',
    '--latest',
    'Publish release to update channel',
    'PUBLIC UNSIGNED PREVIEW'
)) {
    Assert-True ($workflow.Contains($token)) "release workflow is missing: $token"
}

Assert-True (-not $workflow.Contains('pull_request_target')) 'release workflow must not run in pull_request_target'
Assert-True (-not $workflow.Contains('cargo test --workspace --all-targets --locked')) `
    'Windows release must not try to execute the requireAdministrator core-host binary as a test target'
Assert-True (-not $workflow.Contains('MULTICORE_DAEMON_TOKEN')) 'release workflow must not embed runtime secrets'
Assert-True (-not $workflow.Contains('GH_TOKEN: ${{ github.token }}')) 'cross-repository release must not use the source repository token'
Assert-True (-not $workflow.Contains('gh release edit')) 'unsigned workflow must not publish the draft release'
Assert-True (-not $workflow.Contains('--draft=false')) 'unsigned workflow must not clear draft state'
Assert-True (-not $workflow.Contains('--draft')) 'update channel release must be public rather than draft'
Assert-True ($ci.Contains('test-smoke-test-one-click-contract.ps1')) 'normal CI must run the one-click smoke contract'
Assert-True ($ci.Contains('test-packaged-daemon-smoke-contract.ps1')) 'normal CI must run the packaged daemon smoke contract'
Assert-True ($ci.Contains('cargo test --workspace --locked')) 'normal CI must run workspace tests without elevated binary targets'
Assert-True ($ci.Contains($cacheAction)) 'normal CI must restore the Cargo build cache after checkout'
Assert-True ($workflow.Contains($cacheAction)) 'release workflow must reuse the verified Windows Cargo build cache'
Assert-True ($linuxWorkflow.Contains($cacheAction)) 'Linux packaging must reuse Cargo artifacts across clean checkouts'
Assert-True ($ci.Contains('Git\usr\bin')) 'Windows CI must expose gzip before actions/cache saves its archive'
Assert-True ($workflow.Contains('Git\usr\bin')) 'Windows release must expose gzip before actions/cache saves its archive'
Assert-True ($ci.Contains('target/debug/deps')) 'CI cache must keep compiled dependencies'
Assert-True (-not $ci.Contains("            target`n")) 'CI cache must not archive the complete Cargo target tree'
Assert-True ($ci.Contains('CARGO_PROFILE_DEV_DEBUG: 0')) 'CI must not create multi-gigabyte development debug symbols'
Assert-True ($ci.Contains('CARGO_PROFILE_TEST_DEBUG: 0')) 'CI must not create multi-gigabyte test debug symbols'
Assert-True ($ci.Contains('CARGO_INCREMENTAL: 0')) 'CI cache must not retain incremental compiler state'
Assert-True ($workflow.Contains('CARGO_PROFILE_TEST_DEBUG: 0')) 'release verification must use the bounded test profile'
Assert-True ($linuxWorkflow.Contains('target/linux-release-ubuntu/release/deps')) `
    'Linux packaging must use a dedicated bounded release cache'
Assert-True ($ci.Contains('zstd')) 'Linux CI must install zstd before restoring or saving the cache'
Assert-True ($linuxWorkflow.Contains('zstd')) 'Linux packaging must install zstd before restoring or saving the cache'
foreach ($content in @($ci, $workflow, $linuxWorkflow)) {
    Assert-True ($content.Contains("hashFiles('**/Cargo.lock')")) 'every Cargo cache key must be invalidated by Cargo.lock'
}
Assert-True ($linuxWorkflow.Contains('./scripts/package-linux-in-docker.sh --output dist/linux --formats deb,appimage')) `
    'Debian and AppImage packages must be built inside the pinned Ubuntu 22.04 image'
Assert-True (-not $linuxWorkflow.Contains('./scripts/build-linux-packages.sh --stage target/linux-stage --output dist/linux --formats deb,appimage')) `
    'Linux workflow must not build glibc-bound release binaries directly on the Debian 13 runner'
Assert-True ($linuxWorkflow.Contains('target/linux-stage-ubuntu')) `
    'Arch packaging must reuse the exact Ubuntu 22.04 release stage'
Assert-True ($linuxDockerScript.Contains('-e MULTICORE_UPDATE_REPOSITORY')) `
    'Docker packaging must forward the compiled update repository into the container'
Assert-True ($linuxWorkflow.Contains('./scripts/test-linux-package-contract.sh --stage target/linux-stage-ubuntu --packages dist/linux --formats deb,appimage')) `
    'Debian and AppImage artifacts must be inspected before upload'
Assert-True ($linuxWorkflow.Contains('./scripts/test-linux-package-contract.sh --stage target/linux-stage-ubuntu --packages dist/linux --formats arch')) `
    'Arch artifacts must be inspected before upload'

$packageIndex = $workflow.IndexOf('-ReleaseAssetPath dist/release/multicore-windows-x64.zip', [StringComparison]::Ordinal)
$realBundleIndex = $workflow.IndexOf('$env:MULTICORE_RELEASE_BUNDLE', [StringComparison]::Ordinal)
$daemonSmokeIndex = $workflow.IndexOf('smoke-test-packaged-daemon.ps1', [StringComparison]::Ordinal)
Assert-True ($packageIndex -ge 0 -and $realBundleIndex -gt $packageIndex) 'real updater bundle contract must run after release ZIP creation'
Assert-True ($daemonSmokeIndex -gt $packageIndex) 'headless daemon smoke must run after portable package creation'
Assert-True (-not $workflow.Contains('-File scripts/smoke-test-one-click.ps1')) 'hosted release CI must not require a GUI session'

Write-Output 'PASS: GitHub release workflow contract'
