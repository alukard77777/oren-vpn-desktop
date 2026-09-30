[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$DefinitionPath = Join-Path $RepositoryRoot 'packaging\windows-x64\installer.iss'
$BuilderPath = Join-Path $PSScriptRoot 'build-windows-installer.ps1'
$InstalledUpdateSmokePath = Join-Path $PSScriptRoot 'smoke-test-windows-installed-update.ps1'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "FAIL: $Message" }
}

Assert-True (Test-Path -LiteralPath $DefinitionPath -PathType Leaf) 'installer definition must exist'
Assert-True (Test-Path -LiteralPath $BuilderPath -PathType Leaf) 'installer builder must exist'

$definition = Get-Content -Raw -LiteralPath $DefinitionPath
$builder = Get-Content -Raw -LiteralPath $BuilderPath
$installedUpdateSmoke = Get-Content -Raw -LiteralPath $InstalledUpdateSmokePath
& (Join-Path $PSScriptRoot 'test-installer-shortcut-state.ps1')

Assert-True ($definition.Contains('Name: "{autoprograms}\MultiCore"; Filename: "{app}\current\MultiCore.exe"; WorkingDir: "{app}\current"')) `
    'installer must always create a searchable per-user Start menu shortcut'
Assert-True ($installedUpdateSmoke.Contains('Restore-InstallerShortcutState $shortcutSnapshots $ownedShortcutTargets')) `
    'installed-update smoke must restore user shortcuts after disposable uninstall'
Assert-True ($installedUpdateSmoke.IndexOf('Save-InstallerShortcutState') -lt $installedUpdateSmoke.IndexOf('Start-Process -FilePath $installer')) `
    'user shortcuts must be saved before the disposable installer runs'

Assert-True ($installedUpdateSmoke.Contains('reg.exe export')) `
    'installed-update smoke must snapshot an existing URL protocol before testing the installer'
Assert-True ($installedUpdateSmoke.Contains('reg.exe import')) `
    'installed-update smoke must restore the previous URL protocol after testing the uninstaller'
Assert-True ($installedUpdateSmoke.IndexOf('reg.exe export') -lt $installedUpdateSmoke.IndexOf('Start-Process -FilePath $installer')) `
    'URL protocol snapshot must happen before the temporary installer runs'

Assert-True (-not $definition.Contains('Verb: "runas"')) 'installer must not elevate desktop postinstall launch'

foreach ($token in @(
    'DefaultDirName={autopf}\MultiCore',
    'PrivilegesRequired=admin',
    'ArchitecturesAllowed=x64compatible',
    'OutputBaseFilename=MultiCore-Setup-x64',
    'CloseApplications=yes',
    'UninstallDisplayIcon={app}\current\MultiCore.exe',
    'DestDir: "{app}\current"',
    'Name: "desktopicon"',
    'Name: "autostart"',
    'Software\Microsoft\Windows\CurrentVersion\Run',
    '""{app}\current\MultiCore.exe"" --background',
    'Software\Classes\multicore',
    'ValueName: "URL Protocol"',
    'ValueData: "{app}\current\MultiCore.exe,0"',
    '""{app}\current\MultiCore.exe"" ""%1""',
    'Filename: "{app}\current\MultiCore.exe"',
    'WorkingDir: "{app}\current"',
    'Flags: postinstall shellexec skipifsilent',
    'Type: filesandordirs; Name: "{app}\.multicore-previous-*"',
    'Type: filesandordirs; Name: "{app}\.multicore-failed-*"',
    'Type: filesandordirs; Name: "{app}\.multicore-stage-*"',
    'RegQueryStringValue(',
    "CompareText(AutostartValue, ExpectedAutostartValue) = 0",
    "RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'MultiCore')"
)) {
    Assert-True ($definition.Contains($token)) "installer definition is missing: $token"
}

foreach ($forbiddenToken in @(
    'DestDir: "{app}"',
    'UninstallDisplayIcon={app}\MultiCore.exe',
    'Filename: "{app}\MultiCore.exe"',
    'ValueData: "{app}\MultiCore.exe,0"',
    '""{app}\MultiCore.exe"" --background',
    '""{app}\MultiCore.exe"" ""%1""',
    'uninsdeletevalue',
    'Flags: nowait postinstall skipifsilent'
)) {
    Assert-True (-not $definition.Contains($forbiddenToken)) "installer definition still uses the root payload path: $forbiddenToken"
}

foreach ($token in @(
    '[switch]$ValidateOnly',
    'SHA256SUMS.txt',
    'MultiCore-Setup-x64.exe',
    'ISCC.exe',
    '/DPayloadDir=',
    '/DOutputDir=',
    '/DAppVersion='
)) {
    Assert-True ($builder.Contains($token)) "installer builder is missing: $token"
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('multicore-installer-contract-' + [Guid]::NewGuid().ToString('N'))
$payload = Join-Path $testRoot 'payload'
try {
    foreach ($directory in @($payload, (Join-Path $payload 'cores'), (Join-Path $payload 'licenses'), (Join-Path $payload 'runtime'))) {
        New-Item -ItemType Directory -Path $directory | Out-Null
    }
    $files = @(
        'MultiCore.exe',
        'README.md',
        'THIRD_PARTY_NOTICES.md',
        'cores\mihomo.exe',
        'cores\xray.exe',
        'licenses\Xray-core-MPL-2.0.txt',
        'licenses\mihomo-GPL-3.0.txt',
        'runtime\multicore-daemon.exe',
        'runtime\multicore-updater.exe',
        'runtime\multicore-core-host.exe',
        'versions.json'
    )
    $lines = foreach ($relative in $files) {
        $path = Join-Path $payload $relative
        [IO.File]::WriteAllText($path, "fixture:$relative", [Text.UTF8Encoding]::new($false))
        $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash *$($relative.Replace('\', '/'))"
    }
    [IO.File]::WriteAllLines((Join-Path $payload 'SHA256SUMS.txt'), $lines, [Text.UTF8Encoding]::new($false))

    & $BuilderPath -PackagePath $payload -AppVersion '0.1.0' -ValidateOnly

    [IO.File]::WriteAllText((Join-Path $payload 'README.md'), 'tampered', [Text.UTF8Encoding]::new($false))
    $failed = $false
    try { & $BuilderPath -PackagePath $payload -AppVersion '0.1.0' -ValidateOnly } catch { $failed = $true }
    Assert-True $failed 'tampered portable payload must fail installer validation'

    # The vendored dashboard's asset filenames are content-hashed and change on
    # every re-vendor, so the builder must trust the package's own hash-verified
    # SHA256SUMS.txt for that part of the inventory rather than a hardcoded list.
    $dashboardPayload = Join-Path $testRoot 'dashboard-payload'
    foreach ($directory in @($dashboardPayload, (Join-Path $dashboardPayload 'cores'), (Join-Path $dashboardPayload 'licenses'), (Join-Path $dashboardPayload 'runtime'), (Join-Path $dashboardPayload 'dashboard'))) {
        New-Item -ItemType Directory -Path $directory | Out-Null
    }
    $dashboardFiles = @($files + @('dashboard\index-a1b2c3.js', 'dashboard\index.html'))
    $dashboardLines = foreach ($relative in $dashboardFiles) {
        $path = Join-Path $dashboardPayload $relative
        [IO.File]::WriteAllText($path, "fixture:$relative", [Text.UTF8Encoding]::new($false))
        $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash *$($relative.Replace('\', '/'))"
    }
    [IO.File]::WriteAllLines((Join-Path $dashboardPayload 'SHA256SUMS.txt'), $dashboardLines, [Text.UTF8Encoding]::new($false))
    & $BuilderPath -PackagePath $dashboardPayload -AppVersion '0.1.0' -ValidateOnly
    Assert-True $true 'a payload with an unlisted-but-checksummed optional dashboard must validate'

    # A file that exists on disk but has no SHA256SUMS.txt entry must still fail
    # closed - dynamic inventory only trusts what the manifest actually covers.
    [IO.File]::WriteAllText((Join-Path $dashboardPayload 'dashboard\rogue.js'), 'not-in-manifest', [Text.UTF8Encoding]::new($false))
    $failed = $false
    try { & $BuilderPath -PackagePath $dashboardPayload -AppVersion '0.1.0' -ValidateOnly } catch { $failed = $true }
    Assert-True $failed 'a file missing from SHA256SUMS.txt must fail installer validation even under dashboard'
    Remove-Item -LiteralPath (Join-Path $dashboardPayload 'dashboard\rogue.js')

    # A required core file dropped from the manifest must still fail closed,
    # even though the dashboard directory itself is optional.
    $withoutCoreFile = $dashboardLines | Where-Object { $_ -notmatch 'cores/mihomo\.exe$' }
    [IO.File]::WriteAllLines((Join-Path $dashboardPayload 'SHA256SUMS.txt'), $withoutCoreFile, [Text.UTF8Encoding]::new($false))
    $failed = $false
    try { & $BuilderPath -PackagePath $dashboardPayload -AppVersion '0.1.0' -ValidateOnly } catch { $failed = $true }
    Assert-True $failed 'dropping a required core file from the manifest must fail installer validation'

    & (Join-Path $PSScriptRoot 'test-service-installer-contract.ps1')
    Write-Output 'PASS: Windows installer contract'
} finally {
    $resolvedRoot = [IO.Path]::GetFullPath($testRoot)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if ($resolvedRoot.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolvedRoot)) {
        Remove-Item -LiteralPath $resolvedRoot -Recurse -Force
    }
}
