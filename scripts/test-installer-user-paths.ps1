<#
.SYNOPSIS
  Runs the installer's own [Code] against hostile per-user paths, without installing anything.

.DESCRIPTION
  Compiles the [Code] section of packaging\windows-x64\installer.iss into a throwaway Setup
  (PrivilegesRequired=lowest, no files, no registry entries) whose InitializeSetup calls the
  helpers and returns False, so Setup exits before its wizard. Checks that a process running
  from a folder named with ’ ' & % ^ ! spaces and Cyrillic is found by the running-copy check
  (1.1.0 built PowerShell source from that path and broke on ’), and that the signed-in user's
  SID comes back validated. Needs Inno Setup 6 (ISCC.exe); skipped when it is missing.
#>
[CmdletBinding()]
param([string]$CompilerPath)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$definition = [IO.File]::ReadAllText((Join-Path $RepositoryRoot 'packaging\windows-x64\installer.iss'), [Text.Encoding]::UTF8)

$candidates = @($CompilerPath, (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
    (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'), (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'))
$compiler = $candidates | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) } | Select-Object -First 1
if (-not $compiler) { Write-Output 'SKIP: ISCC.exe not found'; return }

$code = $definition.Substring($definition.IndexOf("[Code]`r`n") + "[Code]`r`n".Length)
$root = Join-Path ([IO.Path]::GetTempPath()) ('multicore-installer-paths-' + [Guid]::NewGuid().ToString('N'))
$hostile = Join-Path $root ("Иван" + [char]0x2019 + "s 'q' & %PATH% ^! x")
$probeExe = Join-Path $hostile 'multicore-probe.exe'
$process = $null
try {
    New-Item -ItemType Directory -Path $hostile | Out-Null
    $probeSource = @"
#define AppExeName "MultiCore.exe"
[Setup]
AppName=MultiCore installer probe
AppVersion=0.0.0
DefaultDirName={tmp}\probe
PrivilegesRequired=lowest
Uninstallable=no
CreateAppDir=no
OutputDir=$root
OutputBaseFilename=probe
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Tasks]
Name: "cleandata"; Description: "probe"
Name: "autostart"; Description: "probe"

[Code]
$code

function YesNo(const Value: Boolean): String;
begin
  if Value then Result := 'yes' else Result := 'no';
end;

function InitializeSetup: Boolean;
var
  Lines: TArrayOfString;
  Dir, Sid: String;
begin
  Dir := GetEnv('MULTICORE_PROBE_DIR');
  Sid := InteractiveUserSid;
  SetArrayLength(Lines, 7);
  Lines[0] := 'running=' + YesNo(ProcessRunningUnder(Dir, False));
  Lines[1] := 'running-in-session=' + YesNo(ProcessRunningUnder(Dir, True));
  Lines[2] := 'sibling-running=' + YesNo(ProcessRunningUnder(Dir + 'x', False));
  Lines[3] := 'sid=' + Sid;
  Lines[4] := 'sid-valid=' + YesNo(IsSidString(Sid));
  Lines[5] := 'bad-sid-rejected=' + YesNo(not IsSidString('S-1-5-21-1\..\x') and not IsSidString('') and not IsSidString('S-1-'));
  Lines[6] := 'per-user-mode-lad=' + UserLocalAppData;
  SaveStringsToUTF8File(GetEnv('MULTICORE_PROBE_OUT'), Lines, False);
  Result := False;
end;
"@
    $probeIss = Join-Path $root 'probe.iss'
    [IO.File]::WriteAllText($probeIss, $probeSource, [Text.UTF8Encoding]::new($true))
    $log = & $compiler '/Qp' $probeIss 2>&1
    if ($LASTEXITCODE -ne 0) { throw "FAIL: probe did not compile:`n$($log -join "`n")" }
    if ($log -match '(?i)warning') { throw "FAIL: probe compiled with warnings:`n$($log -join "`n")" }

    Copy-Item -LiteralPath (Join-Path $env:SystemRoot 'System32\ping.exe') -Destination $probeExe
    $process = Start-Process -FilePath $probeExe -ArgumentList '-n 60 127.0.0.1' -WindowStyle Hidden -PassThru
    Start-Sleep -Milliseconds 500

    $out = Join-Path $root 'probe.txt'
    $env:MULTICORE_PROBE_DIR = $hostile
    $env:MULTICORE_PROBE_OUT = $out
    $setup = Start-Process -FilePath (Join-Path $root 'probe.exe') -ArgumentList '/VERYSILENT /SUPPRESSMSGBOXES' -Wait -PassThru
    if (-not (Test-Path -LiteralPath $out)) { throw "FAIL: probe wrote nothing (exit $($setup.ExitCode))" }
    $result = @{}
    foreach ($line in [IO.File]::ReadAllLines($out, [Text.Encoding]::UTF8)) {
        $name, $value = $line -split '=', 2
        $result[$name] = $value
    }
    # The signed-in user is the owner of explorer.exe in this session; a CI runner in session 0
    # has none, and the lookup must then say so instead of guessing.
    $session = (Get-Process -Id $PID).SessionId
    $shell = Get-CimInstance Win32_Process -Filter "Name = 'explorer.exe' AND SessionId = $session" | Select-Object -First 1
    $currentSid = if ($shell) { (Invoke-CimMethod -InputObject $shell -MethodName GetOwnerSid).Sid } else { '' }
    $expected = [ordered]@{
        'running' = 'yes'
        'running-in-session' = 'yes'
        'sibling-running' = 'no'
        'sid' = $currentSid
        'sid-valid' = $(if ($currentSid) { 'yes' } else { 'no' })
        'bad-sid-rejected' = 'yes'
        'per-user-mode-lad' = $env:LOCALAPPDATA
    }
    foreach ($name in $expected.Keys) {
        if ($result[$name] -ne $expected[$name]) { throw "FAIL: $name = '$($result[$name])', expected '$($expected[$name])'" }
    }
    Write-Output 'PASS: installer code handles hostile per-user paths'
} finally {
    Remove-Item Env:\MULTICORE_PROBE_DIR, Env:\MULTICORE_PROBE_OUT -ErrorAction SilentlyContinue
    if ($process -and -not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if ([IO.Path]::GetFullPath($root).StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $root)) {
        Remove-Item -LiteralPath $root -Recurse -Force
    }
}
