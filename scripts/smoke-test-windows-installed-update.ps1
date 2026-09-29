[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [Parameter(Mandatory = $true)]
    [string]$PackagePath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "FAIL: $Message" }
}

function ConvertTo-NativeQuotedArgument {
    param([Parameter(Mandatory = $true)][string]$Value)

    Assert-True (-not $Value.Contains('"')) 'native process argument must not contain a quote'
    return '"' + $Value + '"'
}

function Assert-NoCredentialLeak {
    param([Parameter(Mandatory = $true)][string[]]$Paths)

    $maximumDiagnosticBytes = 1MB
    foreach ($path in $Paths) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { continue }
        $diagnostic = Get-Item -LiteralPath $path
        Assert-True ($diagnostic.Length -le $maximumDiagnosticBytes) `
            "diagnostic output exceeds the bounded read limit: $($diagnostic.FullName)"
        $contents = [IO.File]::ReadAllText($diagnostic.FullName)
        Assert-True (-not $contents.Contains('private-token')) `
            "diagnostic output contains the subscription credential marker: $($diagnostic.Name)"
        Assert-True (-not $contents.Contains('https://backup.invalid/public-feed?private-token')) `
            "diagnostic output contains the credential-bearing subscription URL: $($diagnostic.Name)"
    }
}

function Get-FileHashManifest {
    param([Parameter(Mandatory = $true)][string]$Root)

    $resolvedRoot = [IO.Path]::GetFullPath($Root).TrimEnd('\', '/')
    return @(Get-ChildItem -LiteralPath $resolvedRoot -File -Recurse |
        ForEach-Object {
            $relative = $_.FullName.Substring($resolvedRoot.Length).TrimStart('\', '/').Replace('\', '/')
            [PSCustomObject]@{
                RelativePath = $relative
                Sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        } |
        Sort-Object -Property RelativePath)
}

function Assert-FileHashManifestEqual {
    param(
        [Parameter(Mandatory = $true)][object[]]$Expected,
        [Parameter(Mandatory = $true)][object[]]$Actual
    )

    Assert-True ($Actual.Count -eq $Expected.Count) "mutable data file count changed from $($Expected.Count) to $($Actual.Count)"
    for ($index = 0; $index -lt $Expected.Count; $index++) {
        Assert-True ($Actual[$index].RelativePath -ceq $Expected[$index].RelativePath) `
            "mutable data file set changed at '$($Expected[$index].RelativePath)'"
        Assert-True ($Actual[$index].Sha256 -ceq $Expected[$index].Sha256) `
            "mutable data file changed at '$($Expected[$index].RelativePath)'"
    }
}

function Assert-SafeChildPath {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Child,
        [Parameter(Mandatory = $true)][string]$Description
    )

    $resolvedRoot = [IO.Path]::GetFullPath($Root).TrimEnd('\', '/')
    $resolvedChild = [IO.Path]::GetFullPath($Child)
    $rootPrefix = $resolvedRoot + [IO.Path]::DirectorySeparatorChar
    Assert-True ($resolvedChild.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) `
        "$Description must remain below its approved root"
}

function Assert-CredentialStoredOnlyAtExpectedPath {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$ExpectedPath,
        [Parameter(Mandatory = $true)][string]$Marker
    )

    $resolvedExpected = [IO.Path]::GetFullPath($ExpectedPath)
    $matches = @(Get-ChildItem -LiteralPath $Root -File -Recurse | Where-Object {
        $_.Length -le 1MB -and [IO.File]::ReadAllText($_.FullName).Contains($Marker)
    })
    Assert-True ($matches.Count -eq 1) 'credential marker must exist in exactly one private persisted file'
    Assert-True ($matches[0].FullName -ceq $resolvedExpected) `
        'credential marker must only exist in the expected private subscription snapshot'
}

function Assert-ProductionProfileState {
    param(
        [Parameter(Mandatory = $true)][string]$DaemonPath,
        [Parameter(Mandatory = $true)][string]$DataRoot,
        [Parameter(Mandatory = $true)][string]$ExpectedActiveId,
        [Parameter(Mandatory = $true)][string]$ExpectedRevision,
        [Parameter(Mandatory = $true)][string]$ExpectedGroupId,
        [Parameter(Mandatory = $true)][string]$ExpectedNodeId,
        [Parameter(Mandatory = $true)][string]$ExpectedGroupLabel,
        [Parameter(Mandatory = $true)][string]$ExpectedNodeLabel,
        [Parameter(Mandatory = $true)][string]$ExpectedSourceHost,
        [Parameter(Mandatory = $true)][string]$ExpectedDisplayName,
        [Parameter(Mandatory = $true)][int]$ExpectedUpdatedAt,
        [Parameter(Mandatory = $true)][string]$DiagnosticPrefix
    )

    $token = 'installed-update-production-loader-token'
    $stdoutPath = $DiagnosticPrefix + '-stdout.log'
    $stderrPath = $DiagnosticPrefix + '-stderr.log'
    $environmentNames = @(
        'MULTICORE_DAEMON_TOKEN',
        'MULTICORE_DAEMON_ADDR',
        'MULTICORE_DAEMON_DATA_DIR',
        'MULTICORE_DAEMON_READY_STDOUT')
    $previousEnvironment = @{}
    foreach ($name in $environmentNames) {
        $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    }
    $daemon = $null
    try {
        [Environment]::SetEnvironmentVariable('MULTICORE_DAEMON_TOKEN', $token, 'Process')
        [Environment]::SetEnvironmentVariable('MULTICORE_DAEMON_ADDR', '127.0.0.1:0', 'Process')
        [Environment]::SetEnvironmentVariable('MULTICORE_DAEMON_DATA_DIR', $DataRoot, 'Process')
        [Environment]::SetEnvironmentVariable('MULTICORE_DAEMON_READY_STDOUT', '1', 'Process')
        $daemon = Start-Process -FilePath $DaemonPath `
            -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath `
            -PassThru -WindowStyle Hidden
        $readyLine = $null
        for ($attempt = 0; $attempt -lt 100; $attempt++) {
            if (Test-Path -LiteralPath $stdoutPath -PathType Leaf) {
                $readyLine = Get-Content -LiteralPath $stdoutPath -ErrorAction SilentlyContinue |
                    Where-Object { $_ -cmatch '^MULTICORE_READY 127[.]0[.]0[.]1:[0-9]+$' } |
                    Select-Object -First 1
                if ($null -ne $readyLine) { break }
            }
            if ($daemon.HasExited) {
                throw "production daemon exited before readiness with $($daemon.ExitCode)"
            }
            Start-Sleep -Milliseconds 100
        }
        Assert-True ($null -ne $readyLine) 'production daemon did not publish readiness'
        $address = $readyLine.Substring('MULTICORE_READY '.Length)
        $headers = @{ Authorization = "Bearer $token" }
        $subscriptions = Invoke-RestMethod -Method Get -Uri "http://$address/v1/subscriptions" -Headers $headers
        Assert-True (@($subscriptions.profiles).Count -eq 2) 'production loader must expose both profiles'
        Assert-True ($subscriptions.active_profile_id -ceq $ExpectedActiveId) 'production loader active profile changed'
        $first = @($subscriptions.profiles | Where-Object { $_.id -ceq '11111111-2222-4333-8444-555555555555' })
        $second = @($subscriptions.profiles | Where-Object { $_.id -ceq '66666666-7777-4888-8999-aaaaaaaaaaaa' })
        Assert-True ($first.Count -eq 1 -and $first[0].available) 'production loader rejected the first profile'
        Assert-True ($second.Count -eq 1 -and $second[0].available) 'production loader rejected the second profile'
        Assert-True ($first[0].display_name -ceq 'Primary Profile') `
            "production loader did not recognize metadata: first='$($first[0].display_name)', second='$($second[0].display_name)'"
        Assert-True ($second[0].display_name -ceq 'Backup Profile') `
            "production loader did not recognize second metadata: '$($second[0].display_name)'"

        $status = Invoke-RestMethod -Method Get -Uri "http://$address/v1/status" -Headers $headers
        Assert-True ($status.active_profile_id -ceq $ExpectedActiveId) 'production status active profile changed'
        Assert-True ($status.subscription.source_name -ceq $ExpectedSourceHost) 'production loader source host changed'
        Assert-True ($status.subscription.display_name -ceq $ExpectedDisplayName) 'production loader display name changed'
        Assert-True ($status.subscription.updated_at_unix -eq $ExpectedUpdatedAt) 'production loader metadata timestamp changed'
        Assert-True ($status.current_node -ceq $ExpectedNodeLabel) 'production loader did not restore the persisted second-node selection'

        $catalog = Invoke-RestMethod -Method Get -Uri "http://$address/v1/catalog" -Headers $headers
        Assert-True ($catalog.revision.ToString() -ceq $ExpectedRevision) 'production catalog revision derivation changed'
        Assert-True (@($catalog.groups).Count -eq 1) 'production catalog group count changed'
        Assert-True ($catalog.groups[0].id -ceq $ExpectedGroupId) 'production catalog group ID changed'
        Assert-True ($catalog.groups[0].label -ceq $ExpectedGroupLabel) 'production catalog group label changed'
        Assert-True (@($catalog.groups[0].nodes).Count -eq 2) 'production catalog node count changed'
        Assert-True (-not $catalog.groups[0].nodes[0].selected) 'production loader fell back to the first node'
        Assert-True ($catalog.groups[0].nodes[1].id -ceq $ExpectedNodeId) 'production catalog selected node ID changed'
        Assert-True ($catalog.groups[0].nodes[1].label -ceq $ExpectedNodeLabel) 'production catalog selected node label changed'
        Assert-True ($catalog.groups[0].nodes[1].selected) 'production loader did not restore the exact persisted selection'
    } finally {
        if ($null -ne $daemon -and -not $daemon.HasExited) {
            Stop-Process -Id $daemon.Id -Force
            $daemon.WaitForExit()
        }
        foreach ($name in $environmentNames) {
            [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], 'Process')
        }
    }
    return @($stdoutPath, $stderrPath)
}

$installer = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallerPath))
$package = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($PackagePath))
Assert-True (Test-Path -LiteralPath $installer -PathType Leaf) 'installer must exist'
Assert-True (Test-Path -LiteralPath $package -PathType Container) 'portable package must exist'

$temporaryBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$testRoot = Join-Path $temporaryBase ('multicore-installed-update-smoke ' + [Guid]::NewGuid().ToString('N'))
$installRoot = Join-Path $testRoot 'installation'
$foreignInstallRoot = Join-Path $testRoot 'foreign-installation'
$installSiblingCanary = Join-Path $testRoot 'installation-sibling-canary.bin'
$payloadSiblingCanary = Join-Path $installRoot 'payload-sibling-canary.bin'
$localAppDataSiblingCanary = Join-Path $testRoot 'local-app-data-sibling-canary.bin'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$protocolKey = 'HKCU:\Software\Classes\multicore'
. (Join-Path $PSScriptRoot 'installer-shortcut-state.ps1')
$startMenuShortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'MultiCore.lnk'
$desktopShortcut = Join-Path ([Environment]::GetFolderPath('DesktopDirectory')) 'MultiCore.lnk'
$shortcutSnapshots = @(Save-InstallerShortcutState @($startMenuShortcut, $desktopShortcut))
$ownedShortcutTargets = @(
    (Join-Path $installRoot 'current\MultiCore.exe'),
    (Join-Path $foreignInstallRoot 'current\MultiCore.exe')
)
$foreignAutostart = '"C:\Foreign Tool\agent.exe" --background'
$protocolBackup = Join-Path $temporaryBase ('multicore-protocol-backup-' + [Guid]::NewGuid().ToString('N') + '.reg')
$protocolWasPresent = Test-Path -LiteralPath $protocolKey
if ($protocolWasPresent) {
    & reg.exe export 'HKCU\Software\Classes\multicore' $protocolBackup /y | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw 'Failed to back up the existing MultiCore URL protocol registration'
    }
}

# The outer guard restores the pre-existing handler even if the smoke test or its cleanup fails.
try {
    try {
    New-Item -ItemType Directory -Path $testRoot | Out-Null
    $install = Start-Process -FilePath $installer `
        -ArgumentList @(
            '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=""',
            ('/DIR=' + (ConvertTo-NativeQuotedArgument $installRoot))) `
        -Wait -PassThru -WindowStyle Hidden
    Assert-True ($install.ExitCode -eq 0) "installer exited with $($install.ExitCode)"

    $current = Join-Path $installRoot 'current'
    Assert-True (Test-Path -LiteralPath $startMenuShortcut -PathType Leaf) 'installer must create a Start menu shortcut without an optional task'
    $shortcutShell = New-Object -ComObject WScript.Shell
    $installedShortcut = $shortcutShell.CreateShortcut($startMenuShortcut)
    Assert-True ($installedShortcut.TargetPath -ieq (Join-Path $current 'MultiCore.exe')) 'Start menu shortcut must target the installed current payload'
    Assert-True ($installedShortcut.WorkingDirectory -ieq $current) 'Start menu shortcut must use the installed working directory'
    $uninstaller = Join-Path $installRoot 'unins000.exe'
    $uninstallerData = Join-Path $installRoot 'unins000.dat'
    Assert-True (Test-Path -LiteralPath (Join-Path $current 'MultiCore.exe') -PathType Leaf) 'installed app must live under current'
    Assert-True (Test-Path -LiteralPath $uninstaller -PathType Leaf) 'uninstaller executable must remain at install root'
    Assert-True (Test-Path -LiteralPath $uninstallerData -PathType Leaf) 'uninstaller metadata must remain at install root'
    $uninstallerHash = (Get-FileHash -LiteralPath $uninstaller -Algorithm SHA256).Hash
    $uninstallerDataHash = (Get-FileHash -LiteralPath $uninstallerData -Algorithm SHA256).Hash

    $helper = Join-Path $testRoot 'multicore-apply.exe'
    Copy-Item -LiteralPath (Join-Path $current 'runtime\multicore-updater.exe') -Destination $helper
    # The real desktop would start packaged sidecars, so MultiCore.exe is replaced by an inert
    # x64 client. The updater keeps the previous version unless the client it launches confirms
    # its start (as the real desktop does via updates\started-ok), so the stand-in writes that
    # confirmation when the smoke asks for it and stays alive briefly; otherwise it just exits.
    # Compiled out of process with the .NET Framework compiler so no process keeps it loaded.
    $fakeClient = Join-Path $testRoot 'fake-client.exe'
    $fakeClientSource = Join-Path $testRoot 'fake-client.cs'
    [IO.File]::WriteAllText($fakeClientSource, @'
using System;
using System.IO;
using System.Threading;

public static class MultiCoreSmokeClient
{
    public static int Main()
    {
        string directory = Environment.GetEnvironmentVariable("MULTICORE_SMOKE_CONFIRM_DIR");
        string version = Environment.GetEnvironmentVariable("MULTICORE_SMOKE_CONFIRM_VERSION");
        if (String.IsNullOrEmpty(directory) || String.IsNullOrEmpty(version)) { return 0; }
        string temporary = Path.Combine(directory, "started-ok.tmp");
        string marker = Path.Combine(directory, "started-ok");
        File.WriteAllText(temporary, version);
        if (File.Exists(marker)) { File.Delete(marker); }
        File.Move(temporary, marker);
        Thread.Sleep(3000);
        return 0;
    }
}
'@, [Text.UTF8Encoding]::new($false))
    $csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
    Assert-True (Test-Path -LiteralPath $csc -PathType Leaf) 'the .NET Framework C# compiler is required'
    & $csc /nologo /platform:x64 /target:winexe "/out:$fakeClient" $fakeClientSource | Out-Null
    Assert-True ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $fakeClient -PathType Leaf)) 'inert smoke client was not compiled'
    Copy-Item -LiteralPath $fakeClient -Destination (Join-Path $current 'MultiCore.exe') -Force
    $updatePayload = Join-Path $testRoot 'update-payload'
    Copy-Item -LiteralPath $package -Destination $updatePayload -Recurse
    Copy-Item -LiteralPath $fakeClient -Destination (Join-Path $updatePayload 'MultiCore.exe') -Force
    $sumPath = Join-Path $updatePayload 'SHA256SUMS.txt'
    $sumLines = Get-ChildItem -LiteralPath $updatePayload -File -Recurse |
        Where-Object { $_.FullName -cne $sumPath } |
        ForEach-Object {
            $relative = $_.FullName.Substring($updatePayload.Length).TrimStart('\', '/').Replace('\', '/')
            $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            [PSCustomObject]@{ Relative = $relative; Line = "$hash *$relative" }
        } |
        Sort-Object -Property Relative |
        Select-Object -ExpandProperty Line
    [IO.File]::WriteAllLines($sumPath, $sumLines, [Text.UTF8Encoding]::new($false))
    $archive = Join-Path $testRoot 'update.zip'
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archiveStream = [IO.File]::Open($archive, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $zip = [IO.Compression.ZipArchive]::new($archiveStream, [IO.Compression.ZipArchiveMode]::Create, $false)
        try {
            foreach ($file in Get-ChildItem -LiteralPath $updatePayload -File -Recurse | Sort-Object -Property FullName) {
                $entryName = $file.FullName.Substring($updatePayload.Length).TrimStart('\', '/').Replace('\', '/')
                [IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                    $zip,
                    $file.FullName,
                    $entryName,
                    [IO.Compression.CompressionLevel]::Optimal) | Out-Null
            }
        } finally {
            $zip.Dispose()
        }
    } finally {
        $archiveStream.Dispose()
    }
    $archiveSize = (Get-Item -LiteralPath $archive).Length
    $archiveHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()

    $testLocalAppData = Join-Path $testRoot 'local-app-data'
    $mutableRoot = Join-Path $testLocalAppData 'MultiCore'
    $foreignLocalAppRoot = Join-Path $testLocalAppData 'ForeignProduct'
    $foreignLocalAppCanary = Join-Path $foreignLocalAppRoot 'state.bin'
    $profileId = '11111111-2222-4333-8444-555555555555'
    $secondProfileId = '66666666-7777-4888-8999-aaaaaaaaaaaa'
    $profileRoot = Join-Path $mutableRoot (Join-Path 'profiles' $profileId)
    $secondProfileRoot = Join-Path $mutableRoot (Join-Path 'profiles' $secondProfileId)
    $profileGeneration = Join-Path $profileRoot 'snapshot-00000000000000000001'
    $secondProfileGeneration = Join-Path $secondProfileRoot 'snapshot-00000000000000000002'
    [UInt64]$profileCatalogRevision = 10450735088111108437
    [UInt64]$secondProfileCatalogRevision = 15679144259818742127
    $profileGroupId = 'g-910878313892c155-0000'
    $profileNodeId = 'n-910878313892c155-0000-0001'
    $secondProfileGroupId = 'g-d99782a1f499e16f-0000'
    $secondProfileNodeId = 'n-d99782a1f499e16f-0000-0001'
    Assert-SafeChildPath -Root $mutableRoot -Child $profileRoot -Description 'first profile root'
    Assert-SafeChildPath -Root $mutableRoot -Child $secondProfileRoot -Description 'second profile root'
    New-Item -ItemType Directory -Path $profileGeneration -Force | Out-Null
    New-Item -ItemType Directory -Path $secondProfileGeneration -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $mutableRoot 'logs') -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $mutableRoot 'identity') -Force | Out-Null
    $utf8NoBom = [Text.UTF8Encoding]::new($false)
    # This fixture uses the production multi-sub disk contract and is reloaded through
    # ProfileStore after the packaged update to verify exact profile preservation.
    [IO.File]::WriteAllText(
        (Join-Path $mutableRoot 'preferences.json'),
        "{`n  `"schema_version`": 1,`n  `"restored_bounds`": {`"x`":40,`"y`":60,`"width`":920,`"height`":700},`n  `"maximized`": true,`n  `"visible_page`": `"status`",`n  `"active_profile_hint`": `"$secondProfileId`",`n  `"last_group_by_profile`": {`"$profileId`":`"$profileGroupId`",`"$secondProfileId`":`"$secondProfileGroupId`"},`n  `"selections_by_profile`": {`"$profileId`":{`"$profileGroupId`":`"$profileNodeId`"},`"$secondProfileId`":{`"$secondProfileGroupId`":`"$secondProfileNodeId`"}},`n  `"ambient_background`": true`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $mutableRoot 'profiles\index.json'),
        "{`n  `"schema_version`": 1,`n  `"active_id`": `"$secondProfileId`",`n  `"profiles`": [{`"id`":`"$profileId`",`"created_at_unix`":1757959100},{`"id`":`"$secondProfileId`",`"created_at_unix`":1757959150}]`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $profileGeneration 'mihomo.yaml'),
        "proxies:`n  - name: node-default`n    type: socks5`n    server: 127.0.0.1`n    port: 1080`n  - name: node-smoke`n    type: socks5`n    server: 127.0.0.2`n    port: 1081`nproxy-groups:`n  - name: proxy-group`n    type: select`n    proxies:`n      - node-default`n      - node-smoke`nrules:`n  - MATCH,proxy-group`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $profileGeneration 'xray.json'),
        "{`n  `"log`": {`"loglevel`":`"warning`"},`n  `"outbounds`": []`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $profileGeneration 'subscription.json'),
        "{`n  `"source_url`": `"https://backup.invalid/public-feed?private-token`",`n  `"info`": {`"source_host`":`"backup.invalid`",`"display_name`":`"Primary Profile`",`"updated_at_unix`":1757959200},`n  `"targets`": {`"home_url`":null,`"support_url`":null,`"announcement_url`":null,`"logo_url`":null}`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $profileRoot 'selections.json'),
        "{`n  `"schema_version`": 1,`n  `"catalog_revision`": $profileCatalogRevision,`n  `"selections`": {`"$profileGroupId`": `"$profileNodeId`"}`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $secondProfileGeneration 'mihomo.yaml'),
        "proxies:`n  - name: node-backup-default`n    type: http`n    server: 127.0.0.2`n    port: 8080`n  - name: node-backup`n    type: http`n    server: 127.0.0.3`n    port: 8081`nproxy-groups:`n  - name: fallback-group`n    type: select`n    proxies:`n      - node-backup-default`n      - node-backup`nrules:`n  - MATCH,fallback-group`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $secondProfileGeneration 'xray.json'),
        "{`n  `"log`": {`"loglevel`":`"error`"},`n  `"outbounds`": [{`"protocol`":`"freedom`"}]`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $secondProfileGeneration 'subscription.json'),
        "{`n  `"source_url`": `"https://backup.invalid/public-feed`",`n  `"info`": {`"source_host`":`"backup.invalid`",`"display_name`":`"Backup Profile`",`"updated_at_unix`":1757959250},`n  `"targets`": {`"home_url`":null,`"support_url`":null,`"announcement_url`":null,`"logo_url`":null}`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $secondProfileRoot 'selections.json'),
        "{`n  `"schema_version`": 1,`n  `"catalog_revision`": $secondProfileCatalogRevision,`n  `"selections`": {`"$secondProfileGroupId`": `"$secondProfileNodeId`"}`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $mutableRoot 'identity\device_id'),
        "DA91DA42-1CC9-8995-58D2-A73BF1DE7512",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $mutableRoot 'logs\latest-core.log'),
        "2025-09-15T12:00:00Z INFO smoke sentinel log line`n",
        $utf8NoBom)
    New-Item -ItemType Directory -Path $foreignLocalAppRoot -Force | Out-Null
    [IO.File]::WriteAllText($installSiblingCanary, 'foreign-install-sibling-v1', $utf8NoBom)
    [IO.File]::WriteAllText($payloadSiblingCanary, 'foreign-payload-sibling-v1', $utf8NoBom)
    [IO.File]::WriteAllText($localAppDataSiblingCanary, 'foreign-localappdata-sibling-v1', $utf8NoBom)
    [IO.File]::WriteAllText($foreignLocalAppCanary, 'foreign-localappdata-product-v1', $utf8NoBom)
    $boundaryCanaries = @(
        $installSiblingCanary,
        $payloadSiblingCanary,
        $localAppDataSiblingCanary,
        $foreignLocalAppCanary)
    $boundaryHashesBefore = @{}
    foreach ($canary in $boundaryCanaries) {
        $boundaryHashesBefore[$canary] = (Get-FileHash -LiteralPath $canary -Algorithm SHA256).Hash
    }
    $credentialPath = Join-Path $profileGeneration 'subscription.json'
    Assert-CredentialStoredOnlyAtExpectedPath `
        -Root $mutableRoot `
        -ExpectedPath $credentialPath `
        -Marker 'https://backup.invalid/public-feed?private-token'
    $protectedManifestBefore = Get-FileHashManifest -Root $testLocalAppData
    Assert-True ($protectedManifestBefore.Count -eq 13) 'protected local-app-data fixture must contain both profiles and the foreign-product canary'

    $updaterStdout = Join-Path $testRoot 'updater-stdout.log'
    $updaterStderr = Join-Path $testRoot 'updater-stderr.log'
    $previousLocalAppData = [Environment]::GetEnvironmentVariable('LOCALAPPDATA', 'Process')
    try {
        [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $testLocalAppData, 'Process')
        # The updater keeps its state beside the archive; the stand-in client confirms there.
        [Environment]::SetEnvironmentVariable('MULTICORE_SMOKE_CONFIRM_DIR', (Split-Path -Parent $archive), 'Process')
        [Environment]::SetEnvironmentVariable('MULTICORE_SMOKE_CONFIRM_VERSION', '0.1.1', 'Process')
        $apply = Start-Process -FilePath $helper `
            -ArgumentList @(
                '--wait-pid', '4294967295',
                '--archive', (ConvertTo-NativeQuotedArgument $archive),
                '--target', (ConvertTo-NativeQuotedArgument $current),
                '--size', [string]$archiveSize,
                '--sha256', $archiveHash,
                '--version', '0.1.1') `
            -RedirectStandardOutput $updaterStdout `
            -RedirectStandardError $updaterStderr `
            -Wait -PassThru -WindowStyle Hidden
    } finally {
        [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $previousLocalAppData, 'Process')
        [Environment]::SetEnvironmentVariable('MULTICORE_SMOKE_CONFIRM_DIR', $null, 'Process')
        [Environment]::SetEnvironmentVariable('MULTICORE_SMOKE_CONFIRM_VERSION', $null, 'Process')
    }
    Assert-True ($apply.ExitCode -eq 0) "packaged updater exited with $($apply.ExitCode)"
    # The confirmed stand-in client lingers briefly from the install directory; let it exit
    # before later steps (uninstall) need that directory free.
    $installedClient = [IO.Path]::GetFullPath((Join-Path $current 'MultiCore.exe'))
    @(Get-Process -Name 'MultiCore' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and [IO.Path]::GetFullPath($_.Path) -ieq $installedClient }) |
        Wait-Process -Timeout 30 -ErrorAction SilentlyContinue

    $protectedManifestAfter = Get-FileHashManifest -Root $testLocalAppData
    Assert-FileHashManifestEqual -Expected $protectedManifestBefore -Actual $protectedManifestAfter
    foreach ($canary in $boundaryCanaries) {
        Assert-True (Test-Path -LiteralPath $canary -PathType Leaf) "updater removed boundary canary: $canary"
        Assert-True ((Get-FileHash -LiteralPath $canary -Algorithm SHA256).Hash -ceq $boundaryHashesBefore[$canary]) `
            "updater mutated data outside staging/install payload: $canary"
    }
    # This canary intentionally lives beside current/staging during the update. Remove the
    # harness-owned file after its preservation assertion so the uninstaller can remove its root.
    Remove-Item -LiteralPath $payloadSiblingCanary -Force
    Assert-CredentialStoredOnlyAtExpectedPath `
        -Root $mutableRoot `
        -ExpectedPath $credentialPath `
        -Marker 'https://backup.invalid/public-feed?private-token'

    $stateReader = Join-Path $testRoot 'read persisted state.ps1'
    $stateSignal = Join-Path $testRoot 'state-read.ok'
    $stateReaderSource = @'
[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$SignalPath)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-State {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

$root = Join-Path ([Environment]::GetEnvironmentVariable('LOCALAPPDATA', 'Process')) 'MultiCore'
$expectedProfileId = '11111111-2222-4333-8444-555555555555'
$expectedSecondProfileId = '66666666-7777-4888-8999-aaaaaaaaaaaa'
$preferences = Get-Content -Raw -LiteralPath (Join-Path $root 'preferences.json') | ConvertFrom-Json
$preferenceKeys = @($preferences.PSObject.Properties.Name | Sort-Object)
Assert-State (($preferenceKeys -join ',') -ceq 'active_profile_hint,ambient_background,last_group_by_profile,maximized,restored_bounds,schema_version,selections_by_profile,visible_page') 'unexpected preferences fields'
Assert-State ($preferences.schema_version -eq 1) 'unexpected preferences schema'
Assert-State ($preferences.schema_version -is [int]) 'preferences schema must be an integer'
$restoredBoundsKeys = @($preferences.restored_bounds.PSObject.Properties.Name | Sort-Object)
Assert-State (($restoredBoundsKeys -join ',') -ceq 'height,width,x,y') 'unexpected restored-bounds fields'
Assert-State ($preferences.restored_bounds.x -eq 40 -and $preferences.restored_bounds.x -is [int]) 'unexpected restored x bound'
Assert-State ($preferences.restored_bounds.y -eq 60 -and $preferences.restored_bounds.y -is [int]) 'unexpected restored y bound'
Assert-State ($preferences.restored_bounds.width -eq 920 -and $preferences.restored_bounds.width -is [int]) 'unexpected restored width'
Assert-State ($preferences.restored_bounds.height -eq 700 -and $preferences.restored_bounds.height -is [int]) 'unexpected restored height'
Assert-State ($preferences.maximized -is [bool] -and $preferences.maximized) 'unexpected maximized preference'
Assert-State ($preferences.visible_page -is [string] -and $preferences.visible_page -ceq 'status') 'unexpected visible page'
Assert-State ($preferences.active_profile_hint -is [string] -and $preferences.active_profile_hint -ceq $expectedSecondProfileId) 'unexpected active profile hint'
Assert-State (@($preferences.last_group_by_profile.PSObject.Properties).Count -eq 2) 'unexpected last-group profile count'
Assert-State ($preferences.last_group_by_profile.$expectedProfileId -is [string] -and $preferences.last_group_by_profile.$expectedProfileId -ceq 'g-910878313892c155-0000') 'unexpected last group ID'
Assert-State ($preferences.last_group_by_profile.$expectedSecondProfileId -is [string] -and $preferences.last_group_by_profile.$expectedSecondProfileId -ceq 'g-d99782a1f499e16f-0000') 'unexpected second-profile last group ID'
Assert-State (@($preferences.selections_by_profile.PSObject.Properties).Count -eq 2) 'unexpected selection profile count'
Assert-State (@($preferences.selections_by_profile.$expectedProfileId.PSObject.Properties).Count -eq 1) 'unexpected preference selection count'
Assert-State ($preferences.selections_by_profile.$expectedProfileId.'g-910878313892c155-0000' -is [string] -and $preferences.selections_by_profile.$expectedProfileId.'g-910878313892c155-0000' -ceq 'n-910878313892c155-0000-0001') 'unexpected preference selected node ID'
Assert-State (@($preferences.selections_by_profile.$expectedSecondProfileId.PSObject.Properties).Count -eq 1) 'unexpected second-profile preference selection count'
Assert-State ($preferences.selections_by_profile.$expectedSecondProfileId.'g-d99782a1f499e16f-0000' -is [string] -and $preferences.selections_by_profile.$expectedSecondProfileId.'g-d99782a1f499e16f-0000' -ceq 'n-d99782a1f499e16f-0000-0001') 'unexpected second-profile selected node ID'
Assert-State ($preferences.ambient_background -is [bool] -and $preferences.ambient_background) 'unexpected ambient background preference'

$index = Get-Content -Raw -LiteralPath (Join-Path $root 'profiles\index.json') | ConvertFrom-Json
Assert-State ((@($index.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'active_id,profiles,schema_version') 'unexpected profile index fields'
Assert-State ($index.schema_version -eq 1) 'unexpected profiles schema'
Assert-State ($index.schema_version -is [int]) 'profile index schema must be an integer'
Assert-State ($index.active_id -is [string] -and $index.active_id -ceq $expectedSecondProfileId) 'unexpected active profile'
Assert-State ($index.profiles -is [array]) 'ordered profiles must be an array'
Assert-State ($index.profiles.Count -eq 2) 'unexpected ordered profile count'
$profile = $index.profiles[0]
Assert-State ((@($profile.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'created_at_unix,id') 'unexpected profile entry fields'
Assert-State ($profile.id -is [string] -and $profile.id -ceq $expectedProfileId) 'unexpected ordered profile ID'
Assert-State ($profile.id -cmatch '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$') 'profile ID must be UUID v4 shaped'
Assert-State ($profile.created_at_unix -is [int] -and $profile.created_at_unix -eq 1757959100) 'unexpected profile creation timestamp'
$secondProfile = $index.profiles[1]
Assert-State ((@($secondProfile.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'created_at_unix,id') 'unexpected second profile entry fields'
Assert-State ($secondProfile.id -is [string] -and $secondProfile.id -ceq $expectedSecondProfileId) 'unexpected second ordered profile ID'
Assert-State ($secondProfile.id -cmatch '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$') 'second profile ID must be UUID v4 shaped'
Assert-State ($secondProfile.created_at_unix -is [int] -and $secondProfile.created_at_unix -eq 1757959150) 'unexpected second profile creation timestamp'
$profileRoot = Join-Path $root (Join-Path 'profiles' $expectedProfileId)
$generation = Join-Path $profileRoot 'snapshot-00000000000000000001'
$secondProfileRoot = Join-Path $root (Join-Path 'profiles' $expectedSecondProfileId)
$secondGeneration = Join-Path $secondProfileRoot 'snapshot-00000000000000000002'

$expectedMihomo = "proxies:`n  - name: node-default`n    type: socks5`n    server: 127.0.0.1`n    port: 1080`n  - name: node-smoke`n    type: socks5`n    server: 127.0.0.2`n    port: 1081`nproxy-groups:`n  - name: proxy-group`n    type: select`n    proxies:`n      - node-default`n      - node-smoke`nrules:`n  - MATCH,proxy-group`n"
Assert-State (([IO.File]::ReadAllText((Join-Path $generation 'mihomo.yaml'))) -ceq $expectedMihomo) 'mihomo generation changed'
$xray = Get-Content -Raw -LiteralPath (Join-Path $generation 'xray.json') | ConvertFrom-Json
Assert-State ((@($xray.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'log,outbounds') 'unexpected Xray fields'
Assert-State ($xray.log.loglevel -is [string] -and $xray.log.loglevel -ceq 'warning') 'unexpected Xray log level'
Assert-State (@($xray.outbounds).Count -eq 0) 'unexpected Xray outbounds'

$subscription = Get-Content -Raw -LiteralPath (Join-Path $generation 'subscription.json') | ConvertFrom-Json
Assert-State ((@($subscription.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'info,source_url,targets') 'unexpected subscription fields'
Assert-State ($subscription.source_url -is [string] -and $subscription.source_url -ceq 'https://backup.invalid/public-feed?private-token') 'subscription sentinel changed'
Assert-State ((@($subscription.info.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'display_name,source_host,updated_at_unix') 'unexpected subscription info fields'
Assert-State ($subscription.info.source_host -is [string] -and $subscription.info.source_host -ceq 'backup.invalid') 'subscription host changed'
Assert-State ($subscription.info.display_name -is [string] -and $subscription.info.display_name -ceq 'Primary Profile') 'subscription display name changed'
Assert-State ($subscription.info.updated_at_unix -is [int] -and $subscription.info.updated_at_unix -eq 1757959200) 'subscription timestamp changed'
Assert-State ((@($subscription.targets.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'announcement_url,home_url,logo_url,support_url') 'unexpected subscription target fields'
Assert-State ($null -eq $subscription.targets.home_url) 'subscription home URL changed'
Assert-State ($null -eq $subscription.targets.support_url) 'subscription support URL changed'
Assert-State ($null -eq $subscription.targets.announcement_url) 'subscription announcement URL changed'
Assert-State ($null -eq $subscription.targets.logo_url) 'subscription logo URL changed'

$selections = Get-Content -Raw -LiteralPath (Join-Path $profileRoot 'selections.json') | ConvertFrom-Json
Assert-State ((@($selections.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'catalog_revision,schema_version,selections') 'unexpected selections fields'
Assert-State ($selections.schema_version -eq 1) 'unexpected selections schema'
Assert-State ($selections.schema_version -is [int]) 'selections schema must be an integer'
Assert-State ($selections.catalog_revision.ToString() -ceq '10450735088111108437') 'unexpected catalog revision'
Assert-State (@($selections.selections.PSObject.Properties).Count -eq 1) 'unexpected persisted selection count'
Assert-State ($selections.selections.'g-910878313892c155-0000' -is [string] -and $selections.selections.'g-910878313892c155-0000' -ceq 'n-910878313892c155-0000-0001') 'selected node changed'

$expectedSecondMihomo = "proxies:`n  - name: node-backup-default`n    type: http`n    server: 127.0.0.2`n    port: 8080`n  - name: node-backup`n    type: http`n    server: 127.0.0.3`n    port: 8081`nproxy-groups:`n  - name: fallback-group`n    type: select`n    proxies:`n      - node-backup-default`n      - node-backup`nrules:`n  - MATCH,fallback-group`n"
Assert-State (([IO.File]::ReadAllText((Join-Path $secondGeneration 'mihomo.yaml'))) -ceq $expectedSecondMihomo) 'second mihomo generation changed'
$secondXray = Get-Content -Raw -LiteralPath (Join-Path $secondGeneration 'xray.json') | ConvertFrom-Json
Assert-State ($secondXray.log.loglevel -is [string] -and $secondXray.log.loglevel -ceq 'error') 'unexpected second Xray log level'
Assert-State (@($secondXray.outbounds).Count -eq 1 -and $secondXray.outbounds[0].protocol -ceq 'freedom') 'unexpected second Xray outbound'
$secondSubscription = Get-Content -Raw -LiteralPath (Join-Path $secondGeneration 'subscription.json') | ConvertFrom-Json
Assert-State ($secondSubscription.source_url -is [string] -and $secondSubscription.source_url -ceq 'https://backup.invalid/public-feed') 'second subscription source changed'
Assert-State ($secondSubscription.info.source_host -ceq 'backup.invalid' -and $secondSubscription.info.display_name -ceq 'Backup Profile') 'second subscription presentation changed'
Assert-State ($secondSubscription.info.updated_at_unix -is [int] -and $secondSubscription.info.updated_at_unix -eq 1757959250) 'second subscription timestamp changed'
$secondSelectionsPath = Join-Path $secondProfileRoot 'selections.json'
$secondSelections = Get-Content -Raw -LiteralPath $secondSelectionsPath | ConvertFrom-Json
Assert-State ((@($secondSelections.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'catalog_revision,schema_version,selections') 'unexpected second selections fields'
Assert-State ($secondSelections.schema_version -is [int] -and $secondSelections.schema_version -eq 1) 'unexpected second selections schema'
Assert-State ($secondSelections.catalog_revision.ToString() -ceq '15679144259818742127') 'unexpected second catalog revision'
Assert-State (@($secondSelections.selections.PSObject.Properties).Count -eq 1) 'unexpected second persisted selection count'
Assert-State ($secondSelections.selections.'g-d99782a1f499e16f-0000' -is [string] -and $secondSelections.selections.'g-d99782a1f499e16f-0000' -ceq 'n-d99782a1f499e16f-0000-0001') 'second selected node changed'
Assert-State (([IO.File]::ReadAllText((Join-Path $root 'identity\device_id'))) -ceq 'DA91DA42-1CC9-8995-58D2-A73BF1DE7512') 'device identity changed'
Assert-State (([IO.File]::ReadAllText((Join-Path $root 'logs\latest-core.log'))) -ceq "2025-09-15T12:00:00Z INFO smoke sentinel log line`n") 'core log changed'

$signalTemporary = $SignalPath + '.tmp'
[IO.File]::WriteAllText($signalTemporary, 'multicore-state-readable-v1', [Text.UTF8Encoding]::new($false))
[IO.File]::Move($signalTemporary, $SignalPath)
'@
    [IO.File]::WriteAllText($stateReader, $stateReaderSource, $utf8NoBom)
    $stateReaderStdout = Join-Path $testRoot 'state-reader-stdout.log'
    $stateReaderStderr = Join-Path $testRoot 'state-reader-stderr.log'
    $previousLocalAppData = [Environment]::GetEnvironmentVariable('LOCALAPPDATA', 'Process')
    try {
        [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $testLocalAppData, 'Process')
        $stateRead = Start-Process -FilePath 'powershell.exe' `
            -ArgumentList @(
                '-NoProfile', '-ExecutionPolicy', 'Bypass',
                '-File', (ConvertTo-NativeQuotedArgument $stateReader),
                '-SignalPath', (ConvertTo-NativeQuotedArgument $stateSignal)) `
            -RedirectStandardOutput $stateReaderStdout `
            -RedirectStandardError $stateReaderStderr `
            -Wait -PassThru -WindowStyle Hidden
    } finally {
        [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $previousLocalAppData, 'Process')
    }
    Assert-True ($stateRead.ExitCode -eq 0) "persisted state reader exited with $($stateRead.ExitCode)"
    Assert-True (Test-Path -LiteralPath $stateSignal -PathType Leaf) 'persisted state reader must emit a success signal'
    Assert-True ((Get-Item -LiteralPath $stateSignal).Length -le 64) 'persisted state reader signal must remain bounded'
    Assert-True ([IO.File]::ReadAllText($stateSignal) -ceq 'multicore-state-readable-v1') 'persisted state reader signal must be exact'

    $productionValidationRoot = Join-Path $testRoot 'production-loader-validation'
    Copy-Item -LiteralPath $mutableRoot -Destination $productionValidationRoot -Recurse
    $productionDaemon = Join-Path $current 'runtime\multicore-daemon.exe'
    Assert-True (Test-Path -LiteralPath $productionDaemon -PathType Leaf) 'updated production daemon must exist'
    $productionDiagnostics = @()
    $productionDiagnostics += Assert-ProductionProfileState `
        -DaemonPath $productionDaemon `
        -DataRoot $productionValidationRoot `
        -ExpectedActiveId $secondProfileId `
        -ExpectedRevision '15679144259818742127' `
        -ExpectedGroupId $secondProfileGroupId `
        -ExpectedNodeId $secondProfileNodeId `
        -ExpectedGroupLabel 'fallback-group' `
        -ExpectedNodeLabel 'node-backup' `
        -ExpectedSourceHost 'backup.invalid' `
        -ExpectedDisplayName 'Backup Profile' `
        -ExpectedUpdatedAt 1757959250 `
        -DiagnosticPrefix (Join-Path $testRoot 'production-loader-second')

    $validationIndexPath = Join-Path $productionValidationRoot 'profiles\index.json'
    $validationIndex = [IO.File]::ReadAllText($validationIndexPath)
    $activeNeedle = '  "active_id": "' + $secondProfileId + '"'
    $firstActiveReplacement = '  "active_id": "' + $profileId + '"'
    Assert-True ($validationIndex.Contains($activeNeedle)) 'validation index active ID replacement must be exact'
    [IO.File]::WriteAllText(
        $validationIndexPath,
        $validationIndex.Replace($activeNeedle, $firstActiveReplacement),
        $utf8NoBom)
    $productionDiagnostics += Assert-ProductionProfileState `
        -DaemonPath $productionDaemon `
        -DataRoot $productionValidationRoot `
        -ExpectedActiveId $profileId `
        -ExpectedRevision '10450735088111108437' `
        -ExpectedGroupId $profileGroupId `
        -ExpectedNodeId $profileNodeId `
        -ExpectedGroupLabel 'proxy-group' `
        -ExpectedNodeLabel 'node-smoke' `
        -ExpectedSourceHost 'backup.invalid' `
        -ExpectedDisplayName 'Primary Profile' `
        -ExpectedUpdatedAt 1757959200 `
        -DiagnosticPrefix (Join-Path $testRoot 'production-loader-first')

    Assert-NoCredentialLeak -Paths (@(
        $updaterStdout,
        $updaterStderr,
        (Join-Path $testRoot 'updater.log'),
        (Join-Path $testRoot 'result.json'),
        $stateReaderStdout,
        $stateReaderStderr) + $productionDiagnostics)

    Assert-True (Test-Path -LiteralPath (Join-Path $current 'MultiCore.exe') -PathType Leaf) 'updated payload must be published back to current'
    Assert-True ((Get-FileHash -LiteralPath $uninstaller -Algorithm SHA256).Hash -ceq $uninstallerHash) 'app update must preserve the uninstaller executable'
    Assert-True ((Get-FileHash -LiteralPath $uninstallerData -Algorithm SHA256).Hash -ceq $uninstallerDataHash) 'app update must preserve uninstaller metadata'
    Assert-True (@(Get-ChildItem -LiteralPath $installRoot -Directory -Filter '.multicore-*').Count -eq 0) 'successful updater must clean staging and previous-version directories'

    $protocolCommand = (Get-ItemPropertyValue -LiteralPath (Join-Path $protocolKey 'shell\open\command') -Name '(default)')
    Assert-True ($protocolCommand -ceq ('"' + (Join-Path $current 'MultiCore.exe') + '" "%1"')) 'URL protocol must target current payload'

    $expectedAutostart = '"' + (Join-Path $current 'MultiCore.exe') + '" --background'
    New-Item -Path $runKey -Force | Out-Null
    New-ItemProperty -LiteralPath $runKey -Name 'MultiCore' -PropertyType String -Value $expectedAutostart -Force | Out-Null
    $abandonedStage = Join-Path $installRoot '.multicore-stage-smoke'
    New-Item -ItemType Directory -Path $abandonedStage | Out-Null
    [IO.File]::WriteAllText((Join-Path $abandonedStage 'leftover.txt'), 'staged')

    $uninstall = Start-Process -FilePath $uninstaller `
        -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') `
        -Wait -PassThru -WindowStyle Hidden
    Assert-True ($uninstall.ExitCode -eq 0) "uninstaller exited with $($uninstall.ExitCode)"

    Assert-True (-not (Test-Path -LiteralPath $installRoot)) 'uninstall must remove current payload, backup, and install root'
    Assert-True (-not (Test-Path -LiteralPath $protocolKey)) 'uninstall must remove URL protocol registration'
    $remainingAutostart = Get-ItemProperty -LiteralPath $runKey -Name 'MultiCore' -ErrorAction SilentlyContinue
    Assert-True ($null -eq $remainingAutostart) 'uninstall must remove app-owned autostart value even when enabled after install'

    $foreignInstall = Start-Process -FilePath $installer `
        -ArgumentList @(
            '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=""',
            ('/DIR=' + (ConvertTo-NativeQuotedArgument $foreignInstallRoot))) `
        -Wait -PassThru -WindowStyle Hidden
    Assert-True ($foreignInstall.ExitCode -eq 0) "second installer exited with $($foreignInstall.ExitCode)"
    New-ItemProperty -LiteralPath $runKey -Name 'MultiCore' -PropertyType String -Value $foreignAutostart -Force | Out-Null
    $foreignUninstaller = Join-Path $foreignInstallRoot 'unins000.exe'
    $foreignUninstall = Start-Process -FilePath $foreignUninstaller `
        -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') `
        -Wait -PassThru -WindowStyle Hidden
    Assert-True ($foreignUninstall.ExitCode -eq 0) "second uninstaller exited with $($foreignUninstall.ExitCode)"
    $preservedAutostart = Get-ItemPropertyValue -LiteralPath $runKey -Name 'MultiCore'
    Assert-True ($preservedAutostart -ceq $foreignAutostart) 'uninstall must preserve a foreign replacement autostart value'

    Write-Output 'PASS: packaged updater preserves mutable data without credential leakage and ownership-safe uninstall cleanup'
} finally {
    $resolvedRoot = [IO.Path]::GetFullPath($testRoot)
    $temporaryPrefix = $temporaryBase.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    $resolvedRootName = [IO.Path]::GetFileName($resolvedRoot.TrimEnd('\', '/'))
    if ($resolvedRoot.StartsWith($temporaryPrefix, [StringComparison]::OrdinalIgnoreCase) -and
        $resolvedRootName -match '^multicore-installed-update-smoke [0-9a-f]{32}$' -and
        (Test-Path -LiteralPath $resolvedRoot)) {
        $leftoverUninstaller = Join-Path $installRoot 'unins000.exe'
        if (Test-Path -LiteralPath $leftoverUninstaller -PathType Leaf) {
            $cleanup = Start-Process -FilePath $leftoverUninstaller `
                -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') `
                -Wait -PassThru -WindowStyle Hidden
        }
        $foreignLeftoverUninstaller = Join-Path $foreignInstallRoot 'unins000.exe'
        if (Test-Path -LiteralPath $foreignLeftoverUninstaller -PathType Leaf) {
            $foreignCleanup = Start-Process -FilePath $foreignLeftoverUninstaller `
                -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') `
                -Wait -PassThru -WindowStyle Hidden
        }
        if (Test-Path -LiteralPath $resolvedRoot) {
            Remove-Item -LiteralPath $resolvedRoot -Recurse -Force
        }
    }
    $currentAutostart = Get-ItemProperty -LiteralPath $runKey -Name 'MultiCore' -ErrorAction SilentlyContinue
    if ($null -ne $currentAutostart -and $currentAutostart.MultiCore -ceq $foreignAutostart) {
        Remove-ItemProperty -LiteralPath $runKey -Name 'MultiCore' -Force
    }
    }
} finally {
    try {
    # The disposable install shares HKCU with the user's real installation. Its uninstaller
    # removes the global multicore:// handler, so restore exactly what existed beforehand.
    if (Test-Path -LiteralPath $protocolKey) {
        Remove-Item -LiteralPath $protocolKey -Recurse -Force
    }
    if ($protocolWasPresent) {
        & reg.exe import $protocolBackup | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to restore MultiCore URL protocol registration; backup: $protocolBackup"
        }
        Remove-Item -LiteralPath $protocolBackup -Force
    }
    } finally {
        Restore-InstallerShortcutState $shortcutSnapshots $ownedShortcutTargets
    }
}
