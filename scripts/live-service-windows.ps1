<#
Live test orchestrator for the always-ready MultiCore service on Windows 10/11.

Run ELEVATED as the same user who is logged on interactively (console, auto-logon or a kept-open
RDP session). SSH (build host 192.168.0.148) and PowerShell Direct (Hyper-V VM) are fine for
this script; the checks that need the interactive session run in scripts/live-service-driver.ps1
through a one-shot scheduled task with an Interactive principal, because the service rejects a
daemon whose token lacks the INTERACTIVE logon SID (network and service logons).

DESTRUCTIVE: installs and uninstalls MultiCore, registers/deletes the MultiCore service and moves
%LOCALAPPDATA%\MultiCore aside for the run (restored at the end). Use a test machine or VM.

ASCII only: Windows PowerShell 5.1 reads BOM-less files in the ANSI code page.

Modes:
  Service      /ALLUSERS install: service registered (auto, LocalSystem, restart on failure),
               Program Files layout, admin-only ProgramData, then the driver's Service checks.
  PerUser      /CURRENTUSER install: no service, %LOCALAPPDATA%\Programs\MultiCore, then the
               driver's PerUser checks (proxy mode without UAC, no warm cores).
  UpdateCheck  no install: verify the machine after an update through the service that was
               started from the app (Settings); see -ExpectedVersion.

Uninstall (Service/PerUser):
  Silent                 /VERYSILENT: keeps %LOCALAPPDATA%\MultiCore by design (no question).
  InteractiveKeepData    non-silent uninstall, the driver answers "No" to the data question.
  InteractiveRemoveData  non-silent uninstall, the driver answers "Yes": data is removed.
  Skip                   leave MultiCore installed (e.g. before a manual update test).

Examples:
  .\live-service-windows.ps1 -InstallerPath C:\mc-live\MultiCore-Setup-x64.exe -Mode Service -Uninstall InteractiveRemoveData
  .\live-service-windows.ps1 -InstallerPath C:\mc-live\MultiCore-Setup-x64.exe -Mode PerUser -Uninstall InteractiveKeepData -CleanDataTask
  .\live-service-windows.ps1 -Mode UpdateCheck -ExpectedVersion 1.1.1

Result: <WorkDir>\live-service-result.json, one { check, ok, detail } per step; exit 1 on any failure.
#>
[CmdletBinding()]
param(
    [string]$InstallerPath,
    [ValidateSet('Service', 'PerUser', 'UpdateCheck')] [string]$Mode = 'Service',
    [ValidateSet('Silent', 'InteractiveKeepData', 'InteractiveRemoveData', 'Skip')] [string]$Uninstall = 'Silent',
    [string]$ConsoleUser = $env:USERNAME,
    [string]$SubscriptionUrl,
    [string]$TrafficUrl = 'https://www.gstatic.com/generate_204',
    # Install with the "remove old subscriptions and settings" task and check it wiped the data.
    [switch]$CleanDataTask,
    # Service mode: the driver also starts MultiCore.exe --background and kills it.
    [switch]$WithDesktop,
    [switch]$AllowExistingInstall,
    [string]$ExpectedVersion,
    [string]$WorkDir = (Join-Path $env:ProgramData 'MultiCoreLiveTest'),
    [int]$DriverTimeoutMinutes = 20
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$AppId = '{D95AB954-248C-44C9-9E3A-952398802A2D}_is1'
$TaskName = 'MultiCoreLiveTest'
# The x86 installer installs in 32-bit mode: Program Files (x86) on 64-bit Windows.
$IsX86Installer = [bool]($InstallerPath -and (Split-Path -Leaf $InstallerPath) -like '*-x86.exe')
$ServiceRoot = if ($IsX86Installer -and ${env:ProgramFiles(x86)}) { Join-Path ${env:ProgramFiles(x86)} 'MultiCore' } else { Join-Path $env:ProgramFiles 'MultiCore' }
$PerUserRoot = Join-Path $env:LOCALAPPDATA 'Programs\MultiCore'
$DataRoot = Join-Path $env:LOCALAPPDATA 'MultiCore'
$ProgramDataRoot = Join-Path $env:ProgramData 'MultiCore'
$results = New-Object System.Collections.Generic.List[object]

function Add-Result([string]$Name, [bool]$Ok, [string]$Detail) {
    $results.Add([pscustomobject]@{ check = $Name; ok = $Ok; detail = $Detail })
    Write-Host ("{0} {1}: {2}" -f $(if ($Ok) { 'PASS' } else { 'FAIL' }), $Name, $Detail)
}

function Check([string]$Name, [scriptblock]$Body) {
    try {
        $detail = & $Body
        Add-Result $Name $true (($detail | ForEach-Object { [string]$_ }) -join ' ')
        $true
    } catch {
        Add-Result $Name $false $_.Exception.Message
        $false
    }
}

function Wait-Until([scriptblock]$Condition, [int]$Seconds, [string]$What) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $value = $null
        try { $value = & $Condition } catch { $value = $null }
        if ($value) { return $value }
        Start-Sleep -Milliseconds 500
    }
    throw "timed out after $Seconds s waiting for $What"
}

function Get-InteractiveSession([string]$User) {
    $short = ($User -split '\\')[-1]
    foreach ($process in @(Get-CimInstance Win32_Process -Filter "Name='explorer.exe'")) {
        $owner = Invoke-CimMethod -InputObject $process -MethodName GetOwner
        if ($owner.ReturnValue -eq 0 -and $owner.User -eq $short) { return [int]$process.SessionId }
    }
    $null
}

function Get-ScQc {
    $text = (& sc.exe qc MultiCore 4096) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw "sc qc failed: $text" }
    $text
}

function Test-UserAccess([string]$Path) {
    # Any ACE for these broad principals on an admin-only directory is a finding.
    $acl = (& icacls.exe $Path) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw "icacls failed for ${Path}: $acl" }
    $broad = @($acl -split "`n" | Where-Object { $_ -match 'BUILTIN\\Users|Authenticated Users|\\Everyone|Everyone:|NT AUTHORITY\\INTERACTIVE|CREATOR OWNER' })
    [pscustomobject]@{ Acl = $acl; Broad = $broad }
}

function Invoke-DriverTask {
    param([string[]]$DriverArguments, [string]$ResultPath, [ValidateSet('Limited', 'Highest')] [string]$RunLevel = 'Limited')
    Remove-Item -LiteralPath $ResultPath, ($ResultPath + '.done') -Force -ErrorAction SilentlyContinue
    $driver = Join-Path $WorkDir 'live-service-driver.ps1'
    $quoted = foreach ($argument in $DriverArguments) {
        if ($argument -match '^-') { $argument } else { '"' + $argument + '"' }
    }
    $arguments = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$driver`" -ResultPath `"$ResultPath`" " + ($quoted -join ' ')
    $userId = if ($ConsoleUser -match '\\') { $ConsoleUser } else { "$env:COMPUTERNAME\$ConsoleUser" }
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $arguments -WorkingDirectory $WorkDir
    $principal = New-ScheduledTaskPrincipal -UserId $userId -LogonType Interactive -RunLevel $RunLevel
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
        -ExecutionTimeLimit (New-TimeSpan -Minutes ($DriverTimeoutMinutes + 5)) -MultipleInstances IgnoreNew
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    Register-ScheduledTask -TaskName $TaskName -Action $action -Principal $principal -Settings $settings -Force | Out-Null
    try {
        Start-ScheduledTask -TaskName $TaskName
        $deadline = (Get-Date).AddMinutes($DriverTimeoutMinutes)
        $startDeadline = (Get-Date).AddMinutes(1)
        while (-not (Test-Path -LiteralPath ($ResultPath + '.done')) -and (Get-Date) -lt $deadline) {
            Start-Sleep -Seconds 2
            $info = Get-ScheduledTaskInfo -TaskName $TaskName
            $task = Get-ScheduledTask -TaskName $TaskName
            if ($info.LastRunTime -lt [datetime]'2000-01-01' -and (Get-Date) -gt $startDeadline) {
                throw "the driver task never started; is $ConsoleUser logged on interactively (console, auto-logon or RDP)?"
            }
            if ($task.State -ne 'Running' -and $info.LastRunTime -gt [datetime]'2000-01-01' -and -not (Test-Path -LiteralPath ($ResultPath + '.done'))) {
                Start-Sleep -Seconds 3
                if (-not (Test-Path -LiteralPath ($ResultPath + '.done'))) {
                    throw ("driver task ended without a result (last result 0x{0:x}); is {1} logged on interactively?" -f $info.LastTaskResult, $ConsoleUser)
                }
            }
        }
        if (-not (Test-Path -LiteralPath ($ResultPath + '.done'))) { throw "driver produced no result within $DriverTimeoutMinutes min" }
        # Windows PowerShell 5.1 emits a parsed JSON array as one object: enumerate it.
        $parsed = [IO.File]::ReadAllText($ResultPath) | ConvertFrom-Json
        $parsed | ForEach-Object { $_ }
    } finally {
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    }
}

function Add-DriverResults([string]$Prefix, [object[]]$Checks) {
    foreach ($check in $Checks) { Add-Result "${Prefix}:$($check.check)" ([bool]$check.ok) ([string]$check.detail) }
}

function Get-UninstallEntry {
    foreach ($hive in 'HKLM:', 'HKLM:\Software\WOW6432Node', 'HKCU:') {
        $key = $(if ($hive -like '*WOW6432Node') { "$hive" } else { "$hive\Software" }) + "\Microsoft\Windows\CurrentVersion\Uninstall\$AppId"
        if (Test-Path $key) { return Get-ItemProperty $key }
    }
    $null
}

function Save-Results {
    $path = Join-Path $WorkDir 'live-service-result.json'
    $json = ConvertTo-Json -InputObject @($results.ToArray()) -Depth 4
    [IO.File]::WriteAllText($path, $json, [Text.UTF8Encoding]::new($false))
    Write-Host ''
    $results | Format-Table -AutoSize -Wrap | Out-String -Width 220 | Write-Host
    Write-Host "Result: $path"
}

# ================================================================ UpdateCheck

function Invoke-UpdateCheck {
    Check 'service-running' {
        $service = Get-Service -Name 'MultiCore'
        if ($service.Status -ne 'Running') { throw "service is $($service.Status)" }
        'Running'
    } | Out-Null
    Check 'service-binary-path-unchanged' {
        $qc = Get-ScQc
        $expected = Join-Path $ServiceRoot 'current\runtime\multicore-core-host.exe'
        if ($qc.IndexOf($expected, [StringComparison]::OrdinalIgnoreCase) -lt 0 -or $qc -notmatch '--service') { throw "unexpected binPath:`n$qc" }
        $expected
    } | Out-Null
    Check 'installed-version' {
        $versions = foreach ($relative in 'MultiCore.exe', 'runtime\multicore-daemon.exe', 'runtime\multicore-core-host.exe') {
            $item = Get-Item -LiteralPath (Join-Path $ServiceRoot "current\$relative")
            "$relative=$($item.VersionInfo.ProductVersion)"
        }
        if ($ExpectedVersion -and -not ($versions | Where-Object { $_ -match [regex]::Escape($ExpectedVersion) })) {
            throw "expected $ExpectedVersion, found $($versions -join ' ')"
        }
        $versions -join ' '
    } | Out-Null
    Check 'no-failed-or-stage-leftovers' {
        $left = @(Get-ChildItem -LiteralPath $ServiceRoot -Force -Directory | Where-Object { $_.Name -like '.multicore-stage-*' -or $_.Name -like '.multicore-failed-*' })
        if ($left.Count) { throw "leftovers: $(($left | ForEach-Object Name) -join ', ')" }
        $previous = @(Get-ChildItem -LiteralPath $ServiceRoot -Force -Directory | Where-Object { $_.Name -like '.multicore-previous-*' }).Count
        "none (rollback copies: $previous)"
    } | Out-Null
    Check 'desktop-relaunched-in-user-session' {
        $session = Get-InteractiveSession $ConsoleUser
        if ($null -eq $session) { throw "$ConsoleUser has no interactive session" }
        $desktop = @(Get-Process -Name 'MultiCore' -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq $session })
        if (-not $desktop.Count) { throw "MultiCore.exe is not running in session $session" }
        $cores = @(Get-Process -Name 'mihomo' -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq 0 }).Count
        "MultiCore.exe pid $($desktop[0].Id) in session $session; service mihomo processes: $cores"
    } | Out-Null
}

# ================================================================ install / uninstall

function Invoke-InstallChecks {
    if ($Mode -eq 'Service') {
        Check 'install:service-registered' {
            $service = Get-Service -Name 'MultiCore'
            if ($service.Status -ne 'Running') { throw "service is $($service.Status)" }
            if ([string]$service.StartType -ne 'Automatic') { throw "start type $($service.StartType)" }
            "Running, $($service.StartType)"
        } | Out-Null
        Check 'install:service-config' {
            $qc = Get-ScQc
            $expected = Join-Path $ServiceRoot 'current\runtime\multicore-core-host.exe'
            if ($qc.IndexOf($expected, [StringComparison]::OrdinalIgnoreCase) -lt 0) { throw "binPath is not $expected`n$qc" }
            if ($qc -notmatch '--service') { throw 'binPath lacks --service' }
            if ($qc -notmatch 'SERVICE_START_NAME\s*:\s*LocalSystem') { throw "not LocalSystem`n$qc" }
            $failure = (& sc.exe qfailure MultiCore) -join ' '
            if ($failure -notmatch 'RESTART') { throw "no restart failure action: $failure" }
            "LocalSystem, --service, restart on failure"
        } | Out-Null
        Check 'install:program-files-layout' {
            foreach ($relative in 'current\MultiCore.exe', 'current\runtime\multicore-daemon.exe', 'current\runtime\multicore-core-host.exe', 'unins000.exe') {
                if (-not (Test-Path -LiteralPath (Join-Path $ServiceRoot $relative))) { throw "missing $relative" }
            }
            if (Test-Path -LiteralPath $PerUserRoot) { throw "per-user copy present at $PerUserRoot" }
            $access = Test-UserAccess (Join-Path $ServiceRoot 'current')
            # CREATOR OWNER is inherit-only (it applies to files a user creates), not a grant to Users.
            $writable = @($access.Broad | Where-Object { $_ -notmatch 'CREATOR OWNER' -and $_ -cmatch '\((F|M|W)\)|[(,](W|WD|AD|WDAC|WO)[,)]' })
            if ($writable.Count) { throw "users can write to current: $($writable -join '; ')" }
            $ServiceRoot
        } | Out-Null
    } else {
        Check 'install:no-service' {
            if (Get-Service -Name 'MultiCore' -ErrorAction SilentlyContinue) { throw 'MultiCore service registered by a per-user install' }
            'absent'
        } | Out-Null
        Check 'install:per-user-layout' {
            foreach ($relative in 'current\MultiCore.exe', 'current\runtime\multicore-daemon.exe', 'unins000.exe') {
                if (-not (Test-Path -LiteralPath (Join-Path $PerUserRoot $relative))) { throw "missing $relative" }
            }
            if (Test-Path -LiteralPath (Join-Path $ServiceRoot 'current')) { throw "Program Files copy present at $ServiceRoot" }
            $PerUserRoot
        } | Out-Null
    }
    Check 'install:uninstall-entry' {
        $entry = Get-UninstallEntry
        if (-not $entry) { throw "no uninstall entry $AppId" }
        "$($entry.DisplayName) $($entry.DisplayVersion) ($($entry.PSPath -replace '^.*::', ''))"
    } | Out-Null
    if ($CleanDataTask) {
        Check 'install:cleandata-task-wiped-data' {
            if (Test-Path -LiteralPath (Join-Path $DataRoot 'live-marker.txt')) { throw 'marker survived the cleandata task' }
            'old data removed'
        } | Out-Null
    }
}

function Invoke-PostDriverChecks {
    if ($Mode -eq 'Service') {
        Check 'post:programdata-admin-only' {
            $details = foreach ($path in $ProgramDataRoot, (Join-Path $ProgramDataRoot 'service')) {
                if (-not (Test-Path -LiteralPath $path)) { throw "$path does not exist after a prepared session" }
                $access = Test-UserAccess $path
                if ($access.Broad.Count) { throw "broad ACEs on ${path}: $($access.Broad -join '; ')" }
                $item = Get-Item -LiteralPath $path -Force
                if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "$path is a reparse point" }
                $owner = (Get-Acl -LiteralPath $path).Owner
                if ($owner -notmatch '^(NT AUTHORITY\\SYSTEM|BUILTIN\\Administrators)$') { throw "$path is owned by $owner" }
                "$path ok"
            }
            $details -join '; '
        } | Out-Null
    }
    Check 'post:no-cores-left' {
        $left = @(Get-Process -Name 'mihomo', 'xray', 'multicore-daemon' -ErrorAction SilentlyContinue)
        if ($left.Count) { throw "still running: $(($left | ForEach-Object { "$($_.Name):$($_.Id)/s$($_.SessionId)" }) -join ', ')" }
        'none'
    } | Out-Null
    Check 'post:system-proxy-restored' {
        $now = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings'
        if ([int]$now.ProxyEnable -ne $script:proxyBefore) { throw "ProxyEnable $($now.ProxyEnable), was $script:proxyBefore" }
        "ProxyEnable=$($now.ProxyEnable)"
    } | Out-Null
}

function Save-ServiceLogs {
    # The uninstaller removes %ProgramData%\MultiCore: keep the service's own logs (why a Prepare
    # was refused, the cores' output) with the results. Stop a leftover daemon first so nothing
    # holds the files being copied.
    Get-Process -Name 'multicore-daemon' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    $logs = Join-Path $ProgramDataRoot 'service\logs'
    foreach ($item in @(Get-ChildItem -LiteralPath $logs -File -ErrorAction SilentlyContinue)) {
        try {
            Copy-Item -LiteralPath $item.FullName -Destination (Join-Path $WorkDir ('service-' + $item.BaseName + '.log')) -Force
        } catch {
            Write-Host "could not copy $($item.FullName): $($_.Exception.Message)"
        }
    }
}

function Invoke-Uninstall {
    $root = if ($Mode -eq 'Service') { $ServiceRoot } else { $PerUserRoot }
    $uninstaller = Join-Path $root 'unins000.exe'
    if ($Uninstall -eq 'Silent') {
        Check 'uninstall:silent' {
            $process = Start-Process -FilePath $uninstaller -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait -PassThru
            Wait-Until { -not (Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -like '_iu*' }) } 180 'the uninstaller to exit' | Out-Null
            "exit $($process.ExitCode)"
        } | Out-Null
    } else {
        $resultPath = Join-Path $WorkDir 'driver-uninstall.json'
        $arguments = @('-Mode', 'Uninstall', '-Uninstaller', $uninstaller)
        if ($Uninstall -eq 'InteractiveRemoveData') { $arguments += '-RemoveData' }
        $runLevel = if ($Mode -eq 'Service') { 'Highest' } else { 'Limited' }
        try {
            Add-DriverResults 'uninstall' (Invoke-DriverTask -DriverArguments $arguments -ResultPath $resultPath -RunLevel $runLevel)
        } catch {
            Add-Result 'uninstall:driver' $false $_.Exception.Message
        }
        try { Wait-Until { -not (Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -like '_iu*' }) } 180 'the uninstaller to exit' | Out-Null } catch { }
    }
    Check 'uninstall:files-removed' {
        Wait-Until { -not (Test-Path -LiteralPath (Join-Path $root 'current')) } 60 "$root\current to disappear" | Out-Null
        $rest = if (Test-Path -LiteralPath $root) { @(Get-ChildItem -LiteralPath $root -Force | ForEach-Object Name) -join ',' } else { '' }
        if ($rest) { "current removed; left in ${root}: $rest" } else { "$root removed" }
    } | Out-Null
    Check 'uninstall:entry-removed' {
        if (Get-UninstallEntry) { throw 'uninstall entry still present' }
        'removed'
    } | Out-Null
    if ($Mode -eq 'Service') {
        Check 'uninstall:service-removed' {
            Wait-Until { -not (Get-Service -Name 'MultiCore' -ErrorAction SilentlyContinue) } 60 'the service to be deleted' | Out-Null
            'service deleted'
        } | Out-Null
        Check 'uninstall:programdata-removed' {
            if (Test-Path -LiteralPath $ProgramDataRoot) { throw "$ProgramDataRoot left behind" }
            'removed'
        } | Out-Null
    }
    Check 'uninstall:url-protocol-removed' {
        if (Test-Path 'HKCU:\Software\Classes\multicore') { throw 'HKCU\Software\Classes\multicore left behind' }
        'removed'
    } | Out-Null
    Check 'uninstall:user-data' {
        $exists = Test-Path -LiteralPath (Join-Path $DataRoot 'profiles')
        if ($Uninstall -eq 'InteractiveRemoveData') {
            if (Test-Path -LiteralPath $DataRoot) { throw "$DataRoot kept although removal was chosen" }
            'removed as chosen'
        } else {
            if (-not $exists) { throw "$DataRoot\profiles was removed although it should be kept" }
            'kept'
        }
    } | Out-Null
}

# ================================================================ main

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run this script elevated.' }
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

if ($Mode -eq 'UpdateCheck') {
    Invoke-UpdateCheck
    Save-Results
    if ($results | Where-Object { -not $_.ok }) { exit 1 }
    exit 0
}

if (-not $InstallerPath -or -not (Test-Path -LiteralPath $InstallerPath -PathType Leaf)) { throw "-InstallerPath is required: '$InstallerPath'" }
if (($ConsoleUser -split '\\')[-1] -ne $env:USERNAME) {
    throw "Run as the console user ($ConsoleUser): the installer and the checks use this account's %LOCALAPPDATA% and HKCU."
}
$session = Get-InteractiveSession $ConsoleUser
if ($null -eq $session) {
    throw "$ConsoleUser has no interactive session (no explorer.exe). Log on at the console, enable auto-logon, or keep an RDP session open."
}
Write-Host "Interactive session of ${ConsoleUser}: $session"
$existing = @()
if (Get-Service -Name 'MultiCore' -ErrorAction SilentlyContinue) { $existing += 'service' }
if (Test-Path -LiteralPath (Join-Path $ServiceRoot 'current')) { $existing += $ServiceRoot }
if (Test-Path -LiteralPath (Join-Path $PerUserRoot 'current')) { $existing += $PerUserRoot }
if ($existing.Count -and -not $AllowExistingInstall) { throw "MultiCore is already installed ($($existing -join ', ')); use a clean machine or -AllowExistingInstall." }
if (@(Get-Process -Name 'MultiCore', 'multicore-daemon', 'mihomo', 'xray' -ErrorAction SilentlyContinue).Count) { throw 'Close MultiCore (and any mihomo/xray) first.' }

# The user must be able to write the driver's results; admins and SYSTEM keep full control.
& icacls.exe $WorkDir /grant "${ConsoleUser}:(OI)(CI)M" | Out-Null
Copy-Item -Force -LiteralPath (Join-Path $PSScriptRoot 'live-service-driver.ps1') -Destination $WorkDir
$script:proxyBefore = [int](Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings').ProxyEnable

$backup = $null
if (Test-Path -LiteralPath $DataRoot) {
    $backup = "$DataRoot.live-backup-$([DateTime]::Now.ToString('yyyyMMdd-HHmmss'))"
    Move-Item -LiteralPath $DataRoot -Destination $backup
    Write-Host "Moved the existing data to $backup"
}
try {
    if ($CleanDataTask) {
        New-Item -ItemType Directory -Force -Path $DataRoot | Out-Null
        [IO.File]::WriteAllText((Join-Path $DataRoot 'live-marker.txt'), 'old data')
    }
    $installOk = Check 'install:run' {
        $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$(Join-Path $WorkDir 'install.log')`"")
        $arguments += if ($Mode -eq 'Service') { '/ALLUSERS' } else { '/CURRENTUSER' }
        if ($CleanDataTask) { $arguments += '/MERGETASKS=cleandata' }
        $process = Start-Process -FilePath $InstallerPath -ArgumentList $arguments -Wait -PassThru
        if ($process.ExitCode -ne 0) { throw "installer exit $($process.ExitCode), see install.log" }
        $arguments -join ' '
    }
    if ($installOk) {
        Invoke-InstallChecks
        $driverArguments = @('-Mode', $Mode)
        if ($Mode -eq 'Service') { $driverArguments += @('-InstallRoot', $ServiceRoot) }
        $driverArguments += @('-TrafficUrl', $(if ($TrafficUrl) { $TrafficUrl } else { 'none' }))
        if ($SubscriptionUrl) { $driverArguments += @('-SubscriptionUrl', $SubscriptionUrl) }
        if ($WithDesktop -and $Mode -eq 'Service') { $driverArguments += '-WithDesktop' }
        try {
            Add-DriverResults 'driver' (Invoke-DriverTask -DriverArguments $driverArguments -ResultPath (Join-Path $WorkDir 'driver-result.json'))
        } catch {
            Add-Result 'driver:task' $false $_.Exception.Message
        }
        Invoke-PostDriverChecks
        if ($Mode -eq 'Service') { Save-ServiceLogs }
        if ($Uninstall -ne 'Skip') { Invoke-Uninstall }
    }
} finally {
    if ($Uninstall -ne 'Skip') {
        Remove-Item -LiteralPath $DataRoot -Recurse -Force -ErrorAction SilentlyContinue
        if ($backup) { Move-Item -LiteralPath $backup -Destination $DataRoot }
    } elseif ($backup) {
        Write-Host "MultiCore stays installed; your previous data is in $backup"
    }
    Save-Results
}
if ($results | Where-Object { -not $_.ok }) { exit 1 }
exit 0
