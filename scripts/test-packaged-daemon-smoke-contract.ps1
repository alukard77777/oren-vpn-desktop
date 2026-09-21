[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$smokePath = Join-Path $PSScriptRoot 'smoke-test-packaged-daemon.ps1'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "ASSERTION FAILED: $Message" }
}

Assert-True (Test-Path -LiteralPath $smokePath -PathType Leaf) 'packaged daemon smoke must exist'
$source = Get-Content -Raw -LiteralPath $smokePath
$parseTokens = $null
$parseErrors = $null
[void][Management.Automation.Language.Parser]::ParseFile(
    $smokePath,
    [ref]$parseTokens,
    [ref]$parseErrors
)

Assert-True ($parseErrors.Count -eq 0) 'packaged daemon smoke must parse as valid PowerShell'
foreach ($required in @(
    "'runtime\multicore-daemon.exe'",
    "`$env:MULTICORE_DAEMON_ADDR = '127.0.0.1:0'",
    "`$env:MULTICORE_MIHOMO_CONTROLLER_ADDR = '127.0.0.1:19090'",
    "`$env:MULTICORE_DAEMON_READY_STDOUT = '1'",
    'MULTICORE_READY 127\.0\.0\.1:',
    'Tokenless daemon status returned',
    'Authenticated daemon status returned',
    'Authorization = "Bearer $token"',
    '[IO.FileShare]::ReadWrite',
    'Stop-Process -Id $daemon.Id',
    "Cleanup = 'verified'"
)) {
    Assert-True ($source.Contains($required)) "packaged daemon smoke is missing: $required"
}

Assert-True (-not $source.Contains('MultiCore.exe')) 'hosted daemon smoke must not require a GUI desktop'
Assert-True (-not $source.Contains('/v1/connect')) 'packaged daemon smoke must not launch a live tunnel'

Write-Output 'PASS: packaged daemon smoke is headless, authenticated, and non-live'
