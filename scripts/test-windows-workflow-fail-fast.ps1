[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$nativeCommands = [Collections.Generic.List[object]]::new()
$guardPattern = '\A\s*(if\s*\(\s*\$LASTEXITCODE\s+-ne\s+0\s*\)\s*\{\s*exit\s+\$LASTEXITCODE\s*\})'

foreach ($workflowName in @('ci.yml', 'release.yml')) {
    $workflow = Get-Content -Raw -LiteralPath (Join-Path $root ".github/workflows/$workflowName")
    $steps = [regex]::Matches($workflow, '(?ms)^      - .*?(?=^      - |\z)')
    foreach ($step in $steps) {
        if ($step.Value -notmatch '(?m)^        shell: pwsh\s*$') { continue }
        $run = [regex]::Match($step.Value, '(?m)^        run: ([^\r\n]*)\r?$')
        if (-not $run.Success) { throw "FAIL: missing run body in $workflowName PowerShell step" }
        if ($run.Groups[1].Value.Trim() -eq '|') {
            $body = [Collections.Generic.List[string]]::new()
            foreach ($line in ($step.Value.Substring($run.Index + $run.Length) -split '\r?\n')) {
                if ([string]::IsNullOrWhiteSpace($line)) { $body.Add(''); continue }
                if (-not $line.StartsWith('          ')) { break }
                $body.Add($line.Substring(10))
            }
            $script = $body -join "`n"
        } else {
            $script = $run.Groups[1].Value
        }
        $tokens = $null
        $parseErrors = $null
        $ast = [Management.Automation.Language.Parser]::ParseInput($script, [ref]$tokens, [ref]$parseErrors)
        if ($parseErrors.Count -ne 0) { throw "FAIL: invalid PowerShell in ${workflowName}: $parseErrors" }
        $commands = $ast.FindAll({ param($node) $node -is [Management.Automation.Language.CommandAst] }, $true)
        foreach ($command in $commands) {
            $name = $command.GetCommandName()
            if ([string]::IsNullOrWhiteSpace($name)) { throw "FAIL: cannot classify dynamic workflow command in $workflowName" }
            # Explicit names make the contract independent of the Rust toolchain being on PATH.
            $isNative = $name -match '^(cargo|rustup|rustc)$|\.(exe|cmd|bat)$'
            if (-not $isNative -and $name -notmatch '\.ps1$') {
                $resolved = Get-Command -Name $name -ErrorAction SilentlyContinue
                if ($null -eq $resolved) { throw "FAIL: cannot classify workflow command $name in $workflowName" }
                $isNative = $resolved.CommandType -eq [Management.Automation.CommandTypes]::Application
            }
            if (-not $isNative) { continue }
            $tail = $script.Substring($command.Extent.EndOffset)
            $guard = [regex]::Match($tail, $guardPattern)
            $nativeCommands.Add([pscustomobject]@{
                Workflow = $workflowName
                Command = $command.Extent.Text
                Guard = $guard.Groups[1].Value
            })
        }
    }
}

if ($nativeCommands.Count -lt 20) { throw 'FAIL: Windows workflow native-command coverage unexpectedly shrank' }

function Invoke-NativeExitProbe {
    param([string]$PowerShellPath, [string]$Guard, [int]$NativeExitCode)

    # Real cmd.exe failures do not throw even with ErrorActionPreference=Stop on Windows
    # PowerShell 5.1. The extracted workflow guard must stop the following native command.
    $probe = @"
`$ErrorActionPreference = 'Stop'
`$PSNativeCommandUseErrorActionPreference = `$false
& `$env:ComSpec /d /c 'exit $NativeExitCode'
$Guard
Write-Output 'FOLLOWING_COMMAND_REACHED'
& `$env:ComSpec /d /c 'exit 0'
exit `$LASTEXITCODE
"@
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $PowerShellPath
    $start.Arguments = '-NoLogo -NoProfile -NonInteractive -EncodedCommand ' + [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($probe))
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    try {
        if (-not $process.Start()) { throw 'FAIL: could not start native-exit regression probe' }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(15000)) {
            $process.Kill()
            throw 'FAIL: native-exit regression probe timed out'
        }
        return [pscustomobject]@{ ExitCode = $process.ExitCode; Output = $stdout.Result; Error = $stderr.Result }
    } finally {
        $process.Dispose()
    }
}

$hosts = @((Get-Command powershell.exe -ErrorAction Stop).Source)
$pwsh = Get-Command pwsh.exe -ErrorAction SilentlyContinue
if ($null -ne $pwsh) { $hosts += $pwsh.Source }
foreach ($hostPath in $hosts) {
    # Run before the structural assertion so the old workflow reproduces the actual masking.
    $failure = Invoke-NativeExitProbe -PowerShellPath $hostPath -Guard $nativeCommands[0].Guard -NativeExitCode 37
    if ($failure.ExitCode -ne 37 -or $failure.Output.Contains('FOLLOWING_COMMAND_REACHED')) {
        throw "FAIL: native exit 37 was masked or the following command ran in $hostPath (exit $($failure.ExitCode))"
    }
    $success = Invoke-NativeExitProbe -PowerShellPath $hostPath -Guard $nativeCommands[0].Guard -NativeExitCode 0
    if ($success.ExitCode -ne 0 -or -not $success.Output.Contains('FOLLOWING_COMMAND_REACHED')) {
        throw "FAIL: successful native command did not continue in $hostPath (exit $($success.ExitCode)): $($success.Error)"
    }
}

foreach ($native in $nativeCommands) {
    if ([string]::IsNullOrWhiteSpace($native.Guard)) {
        throw "FAIL: $($native.Workflow) must immediately check LASTEXITCODE after: $($native.Command)"
    }
}

Write-Output "PASS: every Windows workflow native command is fail-fast ($($nativeCommands.Count) commands); native failure and success tested in $($hosts.Count) PowerShell hosts"
