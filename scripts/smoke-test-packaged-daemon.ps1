[CmdletBinding()]
param(
    [string]$PackagePath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $PackagePath) {
    $PackagePath = Join-Path (Split-Path -Parent $PSScriptRoot) 'dist\release\multicore-windows-x64'
}

function Get-ExactProcess {
    param([string]$ExecutablePath)
    $expected = [IO.Path]::GetFullPath($ExecutablePath)
    @(Get-CimInstance Win32_Process | Where-Object {
        $_.ExecutablePath -and
        [IO.Path]::GetFullPath($_.ExecutablePath).Equals($expected, [StringComparison]::OrdinalIgnoreCase)
    })
}

function Get-HttpStatus {
    param([string]$Uri, [hashtable]$Headers)
    try {
        $response = Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 -Uri $Uri -Headers $Headers
        [int]$response.StatusCode
    } catch {
        if ($_.Exception.Response) {
            [int]$_.Exception.Response.StatusCode
        } else {
            throw
        }
    }
}

function Read-SharedText {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return '' }

    $stream = $null
    $reader = $null
    try {
        $sharing = [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
        $stream = [IO.FileStream]::new(
            $Path,
            [IO.FileMode]::Open,
            [IO.FileAccess]::Read,
            $sharing
        )
        $reader = [IO.StreamReader]::new($stream, [Text.UTF8Encoding]::new($false, $true), $true)
        $reader.ReadToEnd()
    } catch [IO.IOException] {
        ''
    } finally {
        if ($reader) { $reader.Dispose() }
        elseif ($stream) { $stream.Dispose() }
    }
}

$package = (Resolve-Path -LiteralPath $PackagePath).Path
$daemonExe = [IO.Path]::GetFullPath((Join-Path $package 'runtime\multicore-daemon.exe'))
if (-not [IO.File]::Exists($daemonExe)) { throw "Package daemon is missing: $daemonExe" }
if ((Get-Item -LiteralPath $daemonExe -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw "Package daemon is a reparse point: $daemonExe"
}
if (@(Get-ExactProcess $daemonExe).Count -ne 0) {
    throw 'Refusing to adopt a pre-existing exact-package daemon'
}

$smokeRoot = Join-Path ([IO.Path]::GetTempPath()) ('multicore-daemon-smoke-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($smokeRoot) | Out-Null
$dataDirectory = Join-Path $smokeRoot 'data'
[IO.Directory]::CreateDirectory($dataDirectory) | Out-Null
$readyPath = Join-Path $smokeRoot 'ready.txt'
$errorPath = Join-Path $smokeRoot 'stderr.txt'
$token = '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
$environmentNames = @(
    'MULTICORE_DAEMON_TOKEN',
    'MULTICORE_DAEMON_ADDR',
    'MULTICORE_DAEMON_DATA_DIR',
    'MULTICORE_DAEMON_READY_STDOUT',
    'MULTICORE_MIHOMO_CONTROLLER_ADDR'
)
$savedEnvironment = @{}
foreach ($name in $environmentNames) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

$daemon = $null
$succeeded = $false
try {
    $env:MULTICORE_DAEMON_TOKEN = $token
    $env:MULTICORE_DAEMON_ADDR = '127.0.0.1:0'
    $env:MULTICORE_DAEMON_DATA_DIR = $dataDirectory
    $env:MULTICORE_DAEMON_READY_STDOUT = '1'
    $env:MULTICORE_MIHOMO_CONTROLLER_ADDR = '127.0.0.1:19090'

    $daemon = Start-Process -FilePath $daemonExe `
        -WorkingDirectory (Split-Path -Parent $daemonExe) `
        -RedirectStandardOutput $readyPath `
        -RedirectStandardError $errorPath `
        -PassThru `
        -WindowStyle Hidden

    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    $readiness = ''
    do {
        Start-Sleep -Milliseconds 100
        $daemon.Refresh()
        $readiness = Read-SharedText $readyPath
    } until ($daemon.HasExited -or $readiness.Length -gt 0 -or [DateTime]::UtcNow -ge $deadline)

    if ($daemon.HasExited) {
        $detail = Read-SharedText $errorPath
        throw "Packaged daemon exited before readiness: $($detail.Substring(0, [Math]::Min($detail.Length, 2048)))"
    }
    $readyMatch = [regex]::Match($readiness, '^MULTICORE_READY 127\.0\.0\.1:(?<port>[1-9][0-9]{0,4})\r?\n$')
    if (-not $readyMatch.Success) { throw 'Packaged daemon emitted an invalid readiness line' }
    $port = [int]$readyMatch.Groups['port'].Value
    if ($port -gt 65535) { throw 'Packaged daemon emitted an invalid readiness port' }

    $uri = "http://127.0.0.1:$port/v1/status"
    $tokenlessStatus = Get-HttpStatus -Uri $uri -Headers @{}
    if ($tokenlessStatus -ne 401) {
        throw "Tokenless daemon status returned $tokenlessStatus instead of 401"
    }

    $headers = @{ Authorization = "Bearer $token" }
    $authenticatedStatus = Get-HttpStatus -Uri $uri -Headers $headers
    if ($authenticatedStatus -ne 200) {
        throw "Authenticated daemon status returned $authenticatedStatus instead of 200"
    }

    Stop-Process -Id $daemon.Id
    if (-not $daemon.WaitForExit(10000)) { throw 'Packaged daemon did not exit by exact PID' }
    if (@(Get-ExactProcess $daemonExe).Count -ne 0) { throw 'Packaged daemon remained after exact-PID cleanup' }

    $succeeded = $true
    [pscustomobject]@{
        DaemonPid = $daemon.Id
        ApiPort = $port
        TokenlessStatus = $tokenlessStatus
        AuthenticatedStatus = $authenticatedStatus
        LiveConnect = 'not exercised'
        Cleanup = 'verified'
    } | Format-List
} finally {
    if ($daemon -and -not $daemon.HasExited) {
        Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
        [void]$daemon.WaitForExit(5000)
    }
    foreach ($name in $environmentNames) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
    if ($succeeded) {
        $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
        $smokeFull = [IO.Path]::GetFullPath($smokeRoot)
        if ($smokeFull.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and
            [IO.Path]::GetFileName($smokeFull).StartsWith('multicore-daemon-smoke-', [StringComparison]::Ordinal)) {
            Remove-Item -LiteralPath $smokeFull -Recurse -Force
        }
    }
}
