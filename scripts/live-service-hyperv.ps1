<#
Runs the always-ready service live test inside the Windows 10 Hyper-V VM from the Hyper-V host.

  - optionally reverts the VM to its clean checkpoint (-Revert);
  - makes sure the guest user is logged on interactively: enables auto-logon and reboots once
    when no explorer.exe runs for that user (the service only serves interactive-logon clients,
    and PowerShell Direct is a non-interactive session 0 logon);
  - copies the installer, scripts/live-service-windows.ps1 and scripts/live-service-driver.ps1
    to C:\mc-live in the guest (Copy-VMFile);
  - runs the orchestrator there through PowerShell Direct and copies every result/log back to
    -OutDir on the host.

ASCII only: Windows PowerShell 5.1 reads BOM-less files in the ANSI code page.

Example (host, Hyper-V Administrators; the guest password of the test VM is not a secret):
  .\scripts\live-service-hyperv.ps1 -InstallerPath .\dist\release\MultiCore-Setup-x64.exe -Revert -Mode Service -Uninstall InteractiveRemoveData
  .\scripts\live-service-hyperv.ps1 -InstallerPath .\dist\release\MultiCore-Setup-x64.exe -Mode PerUser -Uninstall InteractiveKeepData
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string]$InstallerPath,
    [ValidateSet('Service', 'PerUser')] [string]$Mode = 'Service',
    [ValidateSet('Silent', 'InteractiveKeepData', 'InteractiveRemoveData', 'Skip')] [string]$Uninstall = 'Silent',
    [switch]$Revert,
    [switch]$CleanDataTask,
    [switch]$WithDesktop,
    # Re-run on a guest where MultiCore is still installed (e.g. after -Uninstall Skip).
    [switch]$AllowExistingInstall,
    [string]$SubscriptionUrl,
    [string]$TrafficUrl = 'https://www.gstatic.com/generate_204',
    [string]$VMName = 'Win10-MultiCore-Test',
    [string]$Checkpoint = 'clean-win10-22H2',
    [string]$GuestUser = 'test-multicore-vm',
    [string]$GuestPassword = '123',
    [string]$OutDir = (Join-Path (Split-Path -Parent $PSScriptRoot) ("artifacts\live-service-win10-" + [DateTime]::Now.ToString('yyyyMMdd-HHmmss')))
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$credential = New-Object PSCredential($GuestUser, (ConvertTo-SecureString $GuestPassword -AsPlainText -Force))
$guestDir = 'C:\mc-live'

function Wait-Guest([int]$Minutes = 6) {
    # Never retry a rejected credential: repeated bad logons trigger the account lockout.
    $deadline = (Get-Date).AddMinutes($Minutes)
    do {
        Start-Sleep -Seconds 5
        try {
            Invoke-Command -VMName $VMName -Credential $credential -ScriptBlock { 1 } -ErrorAction Stop | Out-Null
            return
        } catch {
            if ($_.Exception.Message -match 'credential|locked|password') { throw "guest rejected the credential: $($_.Exception.Message)" }
        }
    } until ((Get-Date) -gt $deadline)
    throw "guest $VMName not reachable through PowerShell Direct"
}

function Test-GuestInteractive {
    Invoke-Command -VMName $VMName -Credential $credential -ArgumentList $GuestUser -ScriptBlock {
        param($user)
        foreach ($process in @(Get-CimInstance Win32_Process -Filter "Name='explorer.exe'")) {
            $owner = Invoke-CimMethod -InputObject $process -MethodName GetOwner
            if ($owner.ReturnValue -eq 0 -and $owner.User -eq $user) { return $true }
        }
        $false
    }
}

if ($Revert) {
    Write-Host "Reverting $VMName to $Checkpoint"
    Restore-VMSnapshot -VMName $VMName -Name $Checkpoint -Confirm:$false
}
if ((Get-VM -Name $VMName).State -ne 'Running') { Start-VM -Name $VMName }
Wait-Guest

if (-not (Test-GuestInteractive)) {
    Write-Host "No interactive logon for ${GuestUser}: enabling auto-logon and rebooting the guest"
    Invoke-Command -VMName $VMName -Credential $credential -ArgumentList $GuestUser, $GuestPassword -ScriptBlock {
        param($user, $password)
        $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
        Set-ItemProperty $winlogon -Name AutoAdminLogon -Value '1'
        Set-ItemProperty $winlogon -Name DefaultUserName -Value $user
        Set-ItemProperty $winlogon -Name DefaultPassword -Value $password
        Set-ItemProperty $winlogon -Name DefaultDomainName -Value $env:COMPUTERNAME
    }
    Restart-VM -Name $VMName -Force -Wait -For Heartbeat
    Wait-Guest
    $deadline = (Get-Date).AddMinutes(5)
    while (-not (Test-GuestInteractive)) {
        if ((Get-Date) -gt $deadline) { throw "$GuestUser did not log on interactively after the reboot" }
        Start-Sleep -Seconds 5
    }
}
# Give the fresh desktop a moment (Explorer, network) before installing.
Start-Sleep -Seconds 10

$installerName = Split-Path -Leaf $InstallerPath
$files = @(
    @{ Source = (Resolve-Path -LiteralPath $InstallerPath).Path; Name = $installerName },
    @{ Source = (Join-Path $PSScriptRoot 'live-service-windows.ps1'); Name = 'live-service-windows.ps1' },
    @{ Source = (Join-Path $PSScriptRoot 'live-service-driver.ps1'); Name = 'live-service-driver.ps1' }
)
foreach ($file in $files) {
    Copy-VMFile -Name $VMName -SourcePath $file.Source -DestinationPath (Join-Path $guestDir $file.Name) -CreateFullPath -FileSource Host -Force
}

$orchestratorArguments = @(
    '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $guestDir 'live-service-windows.ps1'),
    '-InstallerPath', (Join-Path $guestDir $installerName), '-Mode', $Mode, '-Uninstall', $Uninstall,
    '-ConsoleUser', $GuestUser, '-TrafficUrl', $(if ($TrafficUrl) { $TrafficUrl } else { 'none' })
)
if ($SubscriptionUrl) { $orchestratorArguments += @('-SubscriptionUrl', $SubscriptionUrl) }
if ($CleanDataTask) { $orchestratorArguments += '-CleanDataTask' }
if ($WithDesktop) { $orchestratorArguments += '-WithDesktop' }
if ($AllowExistingInstall) { $orchestratorArguments += '-AllowExistingInstall' }

Write-Host "Running the orchestrator in $VMName ($Mode, uninstall $Uninstall)"
$exitCode = 1
try {
    $exitCode = Invoke-Command -VMName $VMName -Credential $credential -ArgumentList (, $orchestratorArguments) -ScriptBlock {
        param($arguments)
        & powershell.exe @arguments | ForEach-Object { Write-Host $_ }
        $LASTEXITCODE
    }
} finally {
    # Collect whatever the run left, even when the orchestrator failed. A daemon the driver
    # could not stop keeps its stdout/stderr files open: stop it and read with shared access.
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $collected = Invoke-Command -VMName $VMName -Credential $credential -ScriptBlock {
        Get-Process -Name 'multicore-daemon' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
        $work = Join-Path $env:ProgramData 'MultiCoreLiveTest'
        $files = @(Get-ChildItem -LiteralPath $work -File -ErrorAction SilentlyContinue | Where-Object { $_.Extension -in '.json', '.log', '.txt', '.done' })
        # Still installed (-Uninstall Skip, or a failed uninstall): the service's own logs too.
        $files += @(Get-ChildItem -LiteralPath (Join-Path $env:ProgramData 'MultiCore\service\logs') -File -ErrorAction SilentlyContinue)
        foreach ($item in $files) {
            try {
                $stream = [IO.File]::Open($item.FullName, 'Open', 'Read', 'ReadWrite, Delete')
                try { $text = (New-Object IO.StreamReader($stream, [Text.Encoding]::UTF8)).ReadToEnd() } finally { $stream.Dispose() }
                $name = if ($item.DirectoryName -like '*\service\logs') { 'service-installed-' + $item.Name } else { $item.Name }
                [pscustomobject]@{ Name = $name; Text = $text }
            } catch {
                [pscustomobject]@{ Name = $item.Name + '.unreadable.txt'; Text = $_.Exception.Message }
            }
        }
    }
    foreach ($file in @($collected)) {
        if ($file) { [IO.File]::WriteAllText((Join-Path $OutDir $file.Name), [string]$file.Text, [Text.UTF8Encoding]::new($false)) }
    }
    Write-Host "Guest exit code $exitCode; results in $OutDir"
}
exit ([int]$exitCode)
