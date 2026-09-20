[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$smokePath = Join-Path $PSScriptRoot 'smoke-test-one-click.ps1'
$source = Get-Content -Raw -LiteralPath $smokePath

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "ASSERTION FAILED: $Message" }
}

foreach ($required in @(
    "'runtime\multicore-core-host.exe'",
    'Get-ExactProcess $coreHostExe',
    'Core host started before explicit Connect',
    "CoreHostBeforeConnect = 'absent'",
    "LiveConnectUac = 'manual release gate (not exercised)'"
)) {
    Assert-True ($source.Contains($required)) "one-click smoke is missing: $required"
}

Assert-True (-not $source.Contains('latest-core.log')) 'one-click smoke must not require a persistent core log'
Assert-True (-not [regex]::IsMatch($source, 'Invoke-(WebRequest|RestMethod)[^\r\n]*/v1/connect')) 'one-click smoke must not launch a live tunnel'

$inventoryLoop = [regex]::Match(
    $source,
    'foreach \(\$file in @\((?<paths>[^\r\n]+)\)\) \{',
    [Text.RegularExpressions.RegexOptions]::CultureInvariant
)
Assert-True $inventoryLoop.Success 'one-click smoke executable inventory loop is missing'
Assert-True ($inventoryLoop.Groups['paths'].Value.Contains('$coreHostExe')) 'core host must be in executable inventory'

$cleanupLoop = [regex]::Match(
    $source,
    'foreach \(\$path in @\((?<paths>[^\r\n]+)\)\) \{',
    [Text.RegularExpressions.RegexOptions]::CultureInvariant
)
Assert-True $cleanupLoop.Success 'one-click smoke cleanup inventory loop is missing'
Assert-True ($cleanupLoop.Groups['paths'].Value.Contains('$coreHostExe')) 'core host must be in shutdown leak checks'

Write-Output 'PASS: one-click smoke remains non-live and inventories the lazy core host'
