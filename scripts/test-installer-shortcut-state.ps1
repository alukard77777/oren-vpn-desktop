$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'installer-shortcut-state.ps1')

$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ('multicore-shortcut-test-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($fixtureRoot)
$linkPath = Join-Path $fixtureRoot 'MultiCore.lnk'
$ownedTarget = Join-Path $fixtureRoot 'installation\current\MultiCore.exe'
$shell = New-Object -ComObject WScript.Shell
function Write-TestShortcut([string]$Target, [string]$Description) {
    $link = $shell.CreateShortcut($linkPath)
    $link.TargetPath = $Target
    $link.Description = $Description
    $link.Save()
}
function Assert-Test([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
try {
    Write-TestShortcut (Join-Path $env:WINDIR 'notepad.exe') 'Original user shortcut'
    $original = [Convert]::ToBase64String([IO.File]::ReadAllBytes($linkPath))
    $saved = @(Save-InstallerShortcutState @($linkPath))
    Write-TestShortcut $ownedTarget 'Disposable installer shortcut'
    Restore-InstallerShortcutState $saved @($ownedTarget)
    Assert-Test ([Convert]::ToBase64String([IO.File]::ReadAllBytes($linkPath)) -ceq $original) 'Existing shortcut was not restored byte-exact'

    Remove-Item -LiteralPath $linkPath
    Restore-InstallerShortcutState $saved @($ownedTarget)
    Assert-Test ([Convert]::ToBase64String([IO.File]::ReadAllBytes($linkPath)) -ceq $original) 'Uninstaller-deleted user shortcut was not restored'

    Write-TestShortcut (Join-Path $env:WINDIR 'System32\cmd.exe') 'New user shortcut created during the test'
    $changed = [Convert]::ToBase64String([IO.File]::ReadAllBytes($linkPath))
    Restore-InstallerShortcutState $saved @($ownedTarget)
    Assert-Test ([Convert]::ToBase64String([IO.File]::ReadAllBytes($linkPath)) -ceq $changed) 'Restoration overwrote an independently changed shortcut'

    Remove-Item -LiteralPath $linkPath
    $absent = @(Save-InstallerShortcutState @($linkPath))
    Write-TestShortcut $ownedTarget 'Disposable installer shortcut'
    Restore-InstallerShortcutState $absent @($ownedTarget)
    Assert-Test (-not (Test-Path -LiteralPath $linkPath)) 'New disposable shortcut was not removed'
    Restore-InstallerShortcutState $absent @($ownedTarget)
    Assert-Test (-not (Test-Path -LiteralPath $linkPath)) 'Absent shortcut was recreated'
    Write-Output 'PASS: installer shortcut restoration preserves originals and independent user changes'
} finally {
    if (Test-Path -LiteralPath $linkPath) { Remove-Item -LiteralPath $linkPath -Force }
    [IO.Directory]::Delete($fixtureRoot)
}
