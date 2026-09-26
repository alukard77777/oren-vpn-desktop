# Exercise ShellExecute without touching the real multicore protocol or user data.
$ErrorActionPreference = 'Stop'
$probeScheme = 'multicore-shell-test-' + [Guid]::NewGuid().ToString('N')
$probeDirectory = Join-Path ([IO.Path]::GetTempPath()) $probeScheme
[void](New-Item -ItemType Directory -Path $probeDirectory)
$probeExecutable = Join-Path $probeDirectory 'argv-probe.exe'
$resultPath = Join-Path $probeDirectory 'argv.txt'
$keyPath = 'Software\Classes\' + $probeScheme
$key = $null
$registered = $false
try {
    Add-Type -OutputType WindowsApplication -OutputAssembly $probeExecutable -TypeDefinition @'
using System;
using System.IO;
public static class ShellUriProbe {
    public static void Main(string[] args) {
        File.WriteAllLines(Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "argv.txt"), args);
    }
}
'@
    $existing = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($keyPath)
    if ($null -ne $existing) { $existing.Dispose(); throw 'Test protocol already exists' }
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($keyPath)
    $registered = $true
    $key.SetValue('', 'URL:MultiCore shell regression test')
    $key.SetValue('URL Protocol', '')
    $command = $key.CreateSubKey('shell\open\command')
    try { $command.SetValue('', ('"{0}" "%1"' -f $probeExecutable)) }
    finally { $command.Dispose() }

    $supplied = $probeScheme + '://install-sub?url=https%3A%2F%2Fexample.com%2Fsub%3Ftoken%3Da%252Fb%26next%3D1'
    $probe = Start-Process -FilePath $supplied -PassThru -WindowStyle Hidden
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while (-not (Test-Path -LiteralPath $resultPath)) {
        if ([DateTime]::UtcNow -ge $deadline) { throw 'Shell activation did not reach the test executable' }
        Start-Sleep -Milliseconds 50
    }
    if ($null -ne $probe) { [void]$probe.WaitForExit(5000) }
    $received = [IO.File]::ReadAllLines($resultPath)
    if ($received.Count -ne 1) { throw 'Shell did not deliver exactly one URI argument' }
    [pscustomobject]@{ scheme = $probeScheme; supplied = $supplied; received = $received[0] } |
        ConvertTo-Json -Compress
} finally {
    if ($null -ne $key) { $key.Dispose() }
    if ($registered) { [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($keyPath) }
    foreach ($file in @($probeExecutable, $resultPath)) {
        if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -Force }
    }
    [IO.Directory]::Delete($probeDirectory)
}
