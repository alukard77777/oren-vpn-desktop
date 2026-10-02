[CmdletBinding()]
param([string]$ExpectedVersion)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$workflowPath = Join-Path $root '.github\workflows\release.yml'
$ciPath = Join-Path $root '.github\workflows\ci.yml'
$linuxWorkflowPath = Join-Path $root '.github\workflows\linux-packages.yml'
$linuxDockerScriptPath = Join-Path $root 'scripts\package-linux-in-docker.sh'
$linuxPackageContractPath = Join-Path $root 'scripts\test-linux-package-contract.sh'
$publishReleaseScriptPath = Join-Path $root 'scripts\publish-github-release.ps1'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "FAIL: $Message" }
}

Assert-True (Test-Path -LiteralPath $workflowPath -PathType Leaf) 'release workflow must exist'
Assert-True (Test-Path -LiteralPath $ciPath -PathType Leaf) 'CI workflow must exist'
Assert-True (Test-Path -LiteralPath $linuxWorkflowPath -PathType Leaf) 'Linux package workflow must exist'
Assert-True (Test-Path -LiteralPath $linuxDockerScriptPath -PathType Leaf) 'Linux Docker packager must exist'
Assert-True (Test-Path -LiteralPath $linuxPackageContractPath -PathType Leaf) 'Linux package artifact contract must exist'
Assert-True (Test-Path -LiteralPath $publishReleaseScriptPath -PathType Leaf) 'GitHub release publisher must exist'
$workflow = Get-Content -Raw -LiteralPath $workflowPath
$ci = Get-Content -Raw -LiteralPath $ciPath
$linuxWorkflow = Get-Content -Raw -LiteralPath $linuxWorkflowPath
$linuxDockerScript = Get-Content -Raw -LiteralPath $linuxDockerScriptPath
$publishReleaseScript = Get-Content -Raw -LiteralPath $publishReleaseScriptPath

& (Join-Path $PSScriptRoot 'test-windows-workflow-fail-fast.ps1')

foreach ($hostJob in @(
    @{ Text = $ci; Name = 'linux' },
    @{ Text = $linuxWorkflow; Name = 'debian-appimage' }
)) {
    $pattern = '(?ms)^  ' + [regex]::Escape($hostJob.Name) + ':\r?\n(?<body>.*?)(?=^  [\w-]+:\r?$|\z)'
    $body = [regex]::Match($hostJob.Text, $pattern).Groups['body'].Value
    $guardIndex = $body.IndexOf('name: Recover host workspace ownership', [StringComparison]::Ordinal)
    $checkoutIndex = $body.IndexOf('actions/checkout@', [StringComparison]::Ordinal)
    Assert-True ($guardIndex -ge 0 -and $guardIndex -lt $checkoutIndex) `
        "$($hostJob.Name) must recover host workspace ownership before checkout"
    Assert-True (-not $body.Contains('    container:')) 'ownership recovery must only run on the Linux host'
    foreach ($token in @(
        'working-directory: ${{ runner.temp }}',
        'runner_work="$(realpath -e -- "$(dirname -- "${RUNNER_TEMP:?}")")"',
        'expected="$runner_work/multicore/multicore"',
        'workspace="$(realpath -e -- "${GITHUB_WORKSPACE:?}")"',
        '"$workspace" == "$expected"',
        '"$runner_uid" != 0',
        'sudo chown -hR -P -- "$runner_uid:$runner_gid" "$expected"'
    )) {
        Assert-True ($body.Contains($token)) "host recovery guard is missing: $token"
    }
}
foreach ($fedoraJob in @(
    @{ Name = 'rpm'; Tools = 'Install RPM build and verification tools' },
    @{ Name = 'rpm-smoke'; Tools = 'Install isolated Fedora smoke tools' }
)) {
    $pattern = '(?ms)^  ' + [regex]::Escape($fedoraJob.Name) + ':\r?\n(?<body>.*?)(?=^  [\w-]+:\r?$|\z)'
    $body = [regex]::Match($linuxWorkflow, $pattern).Groups['body'].Value
    $toolsIndex = $body.IndexOf($fedoraJob.Tools, [StringComparison]::Ordinal)
    $checkoutIndex = $body.IndexOf('actions/checkout@', [StringComparison]::Ordinal)
    Assert-True ($toolsIndex -ge 0 -and $toolsIndex -lt $checkoutIndex) `
        "$($fedoraJob.Name) must install tools before checkout"
    Assert-True ($body.Substring(0, $checkoutIndex).Contains('dnf install -y git ')) `
        "$($fedoraJob.Name) must install Git before checkout to avoid the REST fallback"
}
Assert-True ($ci.Contains('python3 -B scripts/fixtures/test_runner_workspace_recovery.py')) `
    'Linux CI must exercise the actual workspace recovery guards'

foreach ($token in @(
    '  workflow_dispatch:',
    '      rpm-run-id:',
    '      rpm-sha256:',
    "if: github.event_name != 'workflow_dispatch'",
    'if: ${{ !cancelled() && (needs.rpm.result == ''success'' || github.event_name == ''workflow_dispatch'') }}',
    '    timeout-minutes: 45',
    'name: Verify selected RPM digest',
    'SOURCE_RUN_ID: ${{ inputs.rpm-run-id }}',
    'EXPECTED_SHA256: ${{ inputs.rpm-sha256 }}',
    'run-id: ${{ inputs.rpm-run-id }}',
    'github-token: ${{ github.token }}',
    'sha256sum --check --strict -'
)) {
    Assert-True ($linuxWorkflow.Contains($token)) "Immutable RPM smoke rerun is missing: $token"
}
foreach ($content in @($ci, $workflow, $linuxWorkflow)) {
    Assert-True ($content.Contains('  actions: read')) 'RPM artifact reuse requires read-only Actions permissions in each caller'
}

foreach ($token in @(
    '  rpm:',
    'container: fedora:43',
    '--formats rpm',
    'name: linux-packages-rpm',
    '  rpm-smoke:',
    "fedora: ['43', '44']",
    'container: fedora:${{ matrix.fedora }}',
    'MULTICORE_DISPOSABLE_RPM_TEST: 1',
    'test-fedora-rpm.sh'
)) {
    Assert-True ($linuxWorkflow.Contains($token)) "Fedora RPM workflow is missing: $token"
}
$rpmName = 'multicore-$($env:APP_VERSION)-1.x86_64.rpm'
Assert-True ([regex]::Matches($workflow, [regex]::Escape($rpmName)).Count -eq 4) `
    'RPM must appear in the required Linux set, checksums, release notes, and publication stage'
Assert-True ($workflow.Contains('needs: linux-packages')) 'release must wait for Fedora RPM smoke jobs'
Assert-True ($linuxWorkflow -match '(?m)^          test "\$#" -eq 1\r?$') `
    'RPM smoke must reject multiple artifacts with a standalone fail-fast test'
$linuxCiJob = [regex]::Match($ci, '(?ms)^  linux-packages:\r?\n(?<body>.*?)(?=^  [\w-]+:\r?$|\z)').Groups['body'].Value
Assert-True ($linuxCiJob -match "(?m)^    if: github\.event_name == 'push'\r?`$") `
    'Linux packaging must run on every configured push branch, without a main-only restriction'
Assert-True ($linuxCiJob -match '(?m)^      update-repository: fatyzzz/multicore-releases\r?$') `
    'Linux CI artifacts must use the public update channel before publication'

if ($ExpectedVersion) {
    $manifest = Get-Content -Raw -LiteralPath (Join-Path $root 'apps\multicore-desktop\Cargo.toml')
    $ui = Get-Content -Raw -LiteralPath (Join-Path $root 'apps\multicore-desktop\ui\app.slint')
    $installer = Get-Content -Raw -LiteralPath (Join-Path $root 'scripts\build-windows-installer.ps1')
    $versionPattern = '(?m)^version\s*=\s*"{0}"\s*$' -f [regex]::Escape($ExpectedVersion)
    Assert-True ([regex]::IsMatch($manifest, $versionPattern)) 'desktop manifest must match expected release version'
    Assert-True ($ui.Contains(('update-status: "MultiCore {0}";' -f $ExpectedVersion))) `
        'desktop UI must show the expected release version'
    Assert-True ($installer.Contains(('[string]$AppVersion = ''{0}''' -f $ExpectedVersion))) `
        'installer default must match the expected release version'
}

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
    # 32-bit Windows flavour.
    'rustup target add i686-pc-windows-msvc --toolchain 1.98.1-x86_64-pc-windows-msvc',
    'cargo test --workspace --locked --target i686-pc-windows-msvc',
    'cargo clippy --workspace --all-targets --locked --target i686-pc-windows-msvc -- -D warnings',
    # No rust-skia download exists for 32-bit Windows: the pinned static-CRT Skia build.
    '$env:SKIA_BINARIES_URL = powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/pinned-skia-binaries-url.ps1 -Architecture x86',
    "`$env:CARGO_TARGET_I686_PC_WINDOWS_MSVC_RUSTFLAGS = '-Ctarget-feature=+crt-static'",
    '-ReleaseAssetPath dist/release/multicore-windows-x86.zip',
    '-CoreHostExecutablePath dist/release/multicore-windows-x86/runtime/multicore-core-host.exe',
    "`$env:MULTICORE_RELEASE_BUNDLE_ARCH = 'x86'",
    '-PackagePath dist/release/multicore-windows-x86',
    # Windows on ARM flavour: cross-compiled, tests built but not run on the x64 runner.
    'rustup target add aarch64-pc-windows-msvc --toolchain 1.98.1-x86_64-pc-windows-msvc',
    "if (-not (Get-Command clang.exe -CommandType Application -ErrorAction SilentlyContinue)) { throw 'The ARM64 build needs LLVM clang.exe on PATH' }",
    '$env:SKIA_BINARIES_URL = powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/pinned-skia-binaries-url.ps1 -Architecture arm64',
    "`$env:CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUSTFLAGS = '-Ctarget-feature=+crt-static'",
    'cargo test --workspace --locked --target aarch64-pc-windows-msvc --no-run',
    'cargo clippy --workspace --all-targets --locked --target aarch64-pc-windows-msvc -- -D warnings',
    '-Architecture arm64',
    '-ReleaseAssetPath dist/release/multicore-windows-arm64.zip',
    '-CoreHostExecutablePath dist/release/multicore-windows-arm64/runtime/multicore-core-host.exe',
    "`$env:MULTICORE_RELEASE_BUNDLE_ARCH = 'arm64'",
    '-PackagePath dist/release/multicore-windows-arm64',
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
    'publish-github-release.ps1',
    '-Repository fatyzzz/multicore-releases',
    'Upload verified release publication',
    'Download verified release publication',
    'name: release-publication',
    'needs: release',
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
$publishJob = ($workflow -split '(?m)^  publish:\s*$', 2)[1]
Assert-True ($publishJob.Contains('runs-on: [self-hosted, Windows, X64, windowsbuild]')) `
    'release publication must not depend on GitHub-hosted runner billing'
Assert-True ($publishJob -match '(?m)^    timeout-minutes: 90\s*$') `
    'release publication must allow the complete asset set to upload over a slow connection'
Assert-True (-not $workflow.Contains('gh release create')) 'release publication must not depend on GitHub CLI being installed on the runner'
Assert-True ($publishReleaseScript.Contains('[Text.Encoding]::UTF8.GetBytes')) `
    'release metadata must be encoded as UTF-8 bytes for Windows PowerShell 5.1'
Assert-True ($publishReleaseScript.Contains('[IO.File]::ReadAllText')) `
    'release notes must be read as a plain string without PowerShell file metadata'
Assert-True ($publishReleaseScript.Contains('application/json; charset=utf-8')) `
    'release API requests must declare UTF-8 JSON'
Assert-True (-not $workflow.Contains('gh release edit')) 'unsigned workflow must not publish the draft release'
Assert-True (-not $workflow.Contains('--draft=false')) 'unsigned workflow must not clear draft state'
Assert-True (-not $workflow.Contains('--draft')) 'update channel release must be public rather than draft'
Assert-True ($ci.Contains('test-smoke-test-one-click-contract.ps1')) 'normal CI must run the one-click smoke contract'
Assert-True ($ci.Contains('test-packaged-daemon-smoke-contract.ps1')) 'normal CI must run the packaged daemon smoke contract'
Assert-True ($ci.Contains('cargo test --workspace --locked')) 'normal CI must run workspace tests without elevated binary targets'
foreach ($content in @($ci, $workflow, $linuxWorkflow)) {
    Assert-True (-not $content.Contains('actions/cache')) `
        'self-hosted workflows must not transfer oversized Cargo cache archives'
    Assert-True (-not $content.Contains('Restore Cargo build cache')) `
        'self-hosted workflows must not stall on Cargo cache restoration'
}
Assert-True ($ci.Contains('CARGO_PROFILE_DEV_DEBUG: 0')) 'CI must not create multi-gigabyte development debug symbols'
Assert-True ($ci.Contains('CARGO_PROFILE_TEST_DEBUG: 0')) 'CI must not create multi-gigabyte test debug symbols'
Assert-True ($ci.Contains('CARGO_INCREMENTAL: 0')) 'CI cache must not retain incremental compiler state'
Assert-True ($workflow.Contains('CARGO_PROFILE_TEST_DEBUG: 0')) 'release verification must use the bounded test profile'
Assert-True ($ci.Contains('DPkg::Lock::Timeout=180')) 'Linux CI must tolerate concurrent apt jobs on the self-hosted runner'
Assert-True ($linuxWorkflow.Contains('DPkg::Lock::Timeout=180')) 'Linux packaging must tolerate concurrent apt jobs on the self-hosted runner'
Assert-True ($ci.Contains('zstd')) 'Linux CI must install zstd for release-stage verification'
Assert-True ($linuxWorkflow.Contains('zstd')) 'Linux packaging must install zstd for release-stage verification'
Assert-True ($linuxWorkflow.Contains('zstd file')) `
    'Linux packaging must install file before validating ELF package contents'
Assert-True ($linuxWorkflow.Contains('./scripts/package-linux-in-docker.sh --output dist/linux --formats deb,appimage')) `
    'Debian and AppImage packages must be built inside the pinned Ubuntu 22.04 image'
Assert-True (-not $linuxWorkflow.Contains('./scripts/build-linux-packages.sh --stage target/linux-stage --output dist/linux --formats deb,appimage')) `
    'Linux workflow must not build glibc-bound release binaries directly on the Debian 13 runner'
Assert-True ($linuxWorkflow.Contains('target/linux-stage-ubuntu')) `
    'Arch packaging must reuse the exact Ubuntu 22.04 release stage'
foreach ($token in @(
    'https://api.github.com/repos/$Repository',
    'https://uploads.github.com/repos/$Repository',
    "'X-GitHub-Api-Version'",
    'Invoke-RestMethod -Method Delete',
    "make_latest      = 'true'"
)) {
    Assert-True ($publishReleaseScript.Contains($token)) "release publisher is missing: $token"
}
Assert-True ($linuxDockerScript.Contains('-e MULTICORE_UPDATE_REPOSITORY')) `
    'Docker packaging must forward the compiled update repository into the container'
# The optional local build cache must reuse the exact in-container paths of a cache-less build
# (same --remap-path-prefix inputs, same staged files), and CI must keep building without it.
foreach ($cacheMount in @(
    '-v "$CACHE_DIR/cargo-registry:/opt/cargo/registry"',
    '-v "$CACHE_DIR/target:/build/multicore/target/linux-release-ubuntu"',
    '-v "$CACHE_DIR/downloads:/build/multicore/target/linux-downloads"',
    '-e CARGO_TARGET_DIR=/build/multicore/target/linux-release-ubuntu'
)) {
    Assert-True ($linuxDockerScript.Contains($cacheMount)) "Docker packaging cache must keep the in-container path: $cacheMount"
}
Assert-True (-not $linuxWorkflow.Contains('--cache-dir')) 'CI Linux packages must be built without the local build cache'
Assert-True ($linuxWorkflow.Contains('./scripts/test-linux-package-contract.sh --stage target/linux-stage-ubuntu --packages dist/linux --formats deb,appimage')) `
    'Debian and AppImage artifacts must be inspected before upload'
Assert-True ($linuxWorkflow.Contains('./scripts/test-linux-package-contract.sh --stage target/linux-stage-ubuntu --packages dist/linux --formats arch')) `
    'Arch artifacts must be inspected before upload'

foreach ($x86Asset in @("'dist/release/multicore-windows-x86.zip',", "'dist/release/MultiCore-Setup-x86.exe',")) {
    Assert-True ([regex]::Matches($workflow, [regex]::Escape($x86Asset)).Count -eq 2) `
        "x86 asset must appear in the release checksums and the publication stage: $x86Asset"
}
foreach ($arm64Asset in @("'dist/release/multicore-windows-arm64.zip',", "'dist/release/MultiCore-Setup-arm64.exe',")) {
    Assert-True ([regex]::Matches($workflow, [regex]::Escape($arm64Asset)).Count -eq 2) `
        "arm64 asset must appear in the release checksums and the publication stage: $arm64Asset"
}
$arm64PackageIndex = $workflow.IndexOf('-ReleaseAssetPath dist/release/multicore-windows-arm64.zip', [StringComparison]::Ordinal)
$arm64BundleArchIndex = $workflow.IndexOf("`$env:MULTICORE_RELEASE_BUNDLE_ARCH = 'arm64'", [StringComparison]::Ordinal)
Assert-True ($arm64PackageIndex -ge 0 -and $arm64BundleArchIndex -gt $arm64PackageIndex) 'arm64 updater bundle contract must run after the arm64 release ZIP is created'
# The x64 runner cannot run ARM64 executables: never execute the ARM64 tests or packaged daemon.
Assert-True (-not ($workflow -match '(?m)cargo test [^\r\n]*--target aarch64-pc-windows-msvc(?![^\r\n]*--no-run)')) 'ARM64 tests must be built with --no-run, not executed on the x64 runner'
Assert-True (-not ($workflow -match 'smoke-test-packaged-daemon\.ps1 `\r?\n\s*-PackagePath dist/release/multicore-windows-arm64')) 'the packaged-daemon smoke test must not run the ARM64 package on the x64 runner'
$x86PackageIndex = $workflow.IndexOf('-ReleaseAssetPath dist/release/multicore-windows-x86.zip', [StringComparison]::Ordinal)
$x86BundleArchIndex = $workflow.IndexOf("`$env:MULTICORE_RELEASE_BUNDLE_ARCH = 'x86'", [StringComparison]::Ordinal)
Assert-True ($x86PackageIndex -ge 0 -and $x86BundleArchIndex -gt $x86PackageIndex) 'x86 updater bundle contract must run after the x86 release ZIP is created'
$packageIndex = $workflow.IndexOf('-ReleaseAssetPath dist/release/multicore-windows-x64.zip', [StringComparison]::Ordinal)
$realBundleIndex = $workflow.IndexOf('$env:MULTICORE_RELEASE_BUNDLE', [StringComparison]::Ordinal)
$daemonSmokeIndex = $workflow.IndexOf('smoke-test-packaged-daemon.ps1', [StringComparison]::Ordinal)
Assert-True ($packageIndex -ge 0 -and $realBundleIndex -gt $packageIndex) 'real updater bundle contract must run after release ZIP creation'
Assert-True ($daemonSmokeIndex -gt $packageIndex) 'headless daemon smoke must run after portable package creation'
Assert-True (-not $workflow.Contains('-File scripts/smoke-test-one-click.ps1')) 'hosted release CI must not require a GUI session'

Write-Output 'PASS: GitHub release workflow contract'
