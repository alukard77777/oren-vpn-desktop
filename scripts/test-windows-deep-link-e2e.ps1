[CmdletBinding()]
param(
    [string]$DesktopExecutablePath = 'target\debug\multicore-desktop.exe',
    [string]$DaemonExecutablePath = 'target\debug\multicore-daemon.exe',
    [string]$OutputDirectory = 'artifacts\windows-deep-link-e2e',
    [switch]$ReleaseBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$desktopPath = (Resolve-Path -LiteralPath $DesktopExecutablePath).Path
$daemonPath = (Resolve-Path -LiteralPath $DaemonExecutablePath).Path
$root = [IO.Path]::GetFullPath($OutputDirectory)
[void][IO.Directory]::CreateDirectory($root)
$testRoot = Join-Path $root ([Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($testRoot)
if ($ReleaseBuild -and @(Get-Process -Name MultiCore,multicore-desktop -ErrorAction SilentlyContinue).Count -gt 0) {
    throw 'Release smoke requires no existing desktop instance; it never stops a user instance'
}
if (-not $ReleaseBuild -and $desktopPath -notmatch '\\target\\debug\\multicore-desktop\.exe$') {
    throw 'The isolated preview smoke requires the debug desktop; use -ReleaseBuild on an idle test machine'
}
if (@(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $desktopPath }).Count -gt 0) {
    throw 'The test executable is already running'
}

Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class DeepLinkSmokeWindow {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int t, uint flags);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr h, int command);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] private static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
    [DllImport("dwmapi.dll")] public static extern int DwmFlush();
    private delegate bool EnumWindowCallback(IntPtr h, IntPtr p);
    [DllImport("user32.dll")] private static extern bool EnumWindows(EnumWindowCallback callback, IntPtr p);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr h, out uint process);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern int GetWindowText(IntPtr h, StringBuilder text, int count);
    public static IntPtr FindMainWindow(int process) {
        IntPtr result = IntPtr.Zero;
        EnumWindows((h, p) => {
            uint owner;
            GetWindowThreadProcessId(h, out owner);
            if (owner != process) return true;
            var title = new StringBuilder(256);
            GetWindowText(h, title, title.Capacity);
            if (title.ToString() != "MULTICORE") return true;
            result = h;
            return false;
        }, IntPtr.Zero);
        return result;
    }
    public static void Click(IntPtr h, int x, int y) {
        IntPtr point = (IntPtr)((y << 16) | (x & 0xffff));
        PostMessage(h, 0x0200, IntPtr.Zero, point);
        PostMessage(h, 0x0201, (IntPtr)1, point);
        PostMessage(h, 0x0202, IntPtr.Zero, point);
    }
    public static void Escape() {
        keybd_event(27, 1, 0, UIntPtr.Zero);
        keybd_event(27, 1, 2, UIntPtr.Zero);
    }
}
'@

function Wait-Until([scriptblock]$Condition, [string]$Failure) {
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    throw $Failure
}
function Start-Hidden([string]$Executable, [string]$Arguments) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable
    $info.Arguments = $Arguments
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    [Diagnostics.Process]::Start($info)
}
function Get-TestWindow {
    [DeepLinkSmokeWindow]::FindMainWindow($script:desktop.Id)
}
function Save-Window([string]$Name) {
    $handle = Get-TestWindow
    if ($handle -eq [IntPtr]::Zero) { return $false }
    [void][DeepLinkSmokeWindow]::BringWindowToTop($handle)
    [void][DeepLinkSmokeWindow]::SetForegroundWindow($handle)
    [void][DeepLinkSmokeWindow]::DwmFlush()
    $rect = New-Object DeepLinkSmokeWindow+RECT
    [void][DeepLinkSmokeWindow]::GetWindowRect($handle, [ref]$rect)
    $bitmap = [Drawing.Bitmap]::new($rect.Right-$rect.Left, $rect.Bottom-$rect.Top)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    try {
        $dc = $graphics.GetHdc()
        try { [void][DeepLinkSmokeWindow]::PrintWindow($handle, $dc, 2) }
        finally { $graphics.ReleaseHdc($dc) }
        $center = $bitmap.GetPixel([int]($bitmap.Width/2), [int]($bitmap.Height/2))
        if ($Name) { $bitmap.Save((Join-Path $testRoot ($Name+'.png')), [Drawing.Imaging.ImageFormat]::Png) }
        # The actual modal surface must be visible, not merely a running process.
        return ([Math]::Abs($center.R-28) -le 3 -and [Math]::Abs($center.G-32) -le 3 -and [Math]::Abs($center.B-38) -le 3)
    } finally { $graphics.Dispose(); $bitmap.Dispose() }
}
function Wait-Modal([string]$Name) {
    Wait-Until { (Get-TestWindow) -ne [IntPtr]::Zero } 'Desktop window did not open'
    $handle = Get-TestWindow
    Wait-Until { [DeepLinkSmokeWindow]::IsWindowVisible($handle) -and -not [DeepLinkSmokeWindow]::IsIconic($handle) } 'Desktop did not restore its own window'
    [void][DeepLinkSmokeWindow]::SetWindowPos($handle, [IntPtr]::Zero, 60, 60, 840, 720, 0)
    Wait-Until { Save-Window '' } "Consent dialog did not appear: $Name"
    [void](Save-Window $Name)
}
function Click-Consent([bool]$Approve) {
    $x = if ($Approve) { 562 } else { 466 }
    [DeepLinkSmokeWindow]::Click((Get-TestWindow), $x, 430)
}
function Open-Link([string]$Url) {
    $uri = 'multicore://install-sub?url=' + [Uri]::EscapeDataString($Url)
    Start-Process -FilePath $uri -PassThru -WindowStyle Hidden
}
function Get-Profiles {
    $response = Invoke-RestMethod -Uri ($script:api+'/v1/subscriptions') -Headers $script:headers -TimeoutSec 3
    @($response.profiles)
}
function Get-FeedRequests {
    $path = Join-Path $testRoot 'feed-requests.txt'
    if (Test-Path -LiteralPath $path) { [IO.File]::ReadAllLines($path) }
}
function Stop-TestDesktop {
    if ($null -ne $script:desktop -and -not $script:desktop.HasExited) {
        [void]$script:desktop.CloseMainWindow()
        if (-not $script:desktop.WaitForExit(5000)) { $script:desktop.Kill(); $script:desktop.WaitForExit() }
    }
    $script:desktop = $null
}

$envNames = @('MULTICORE_DAEMON_URL','MULTICORE_DAEMON_TOKEN','MULTICORE_DAEMON_ADDR','MULTICORE_DAEMON_DATA_DIR','MULTICORE_DAEMON_READY_STDOUT','MULTICORE_PREVIEW_INSTANCE','MULTICORE_SMOKE_EXIT_ON_CLOSE','LOCALAPPDATA','SLINT_SCALE_FACTOR')
$savedEnvironment = @{}
foreach ($name in $envNames) { $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$protocolPath = 'Registry::HKEY_CURRENT_USER\Software\Classes\multicore'
$commandPath = $protocolPath+'\shell\open\command'
$protocolExisted = $false
$protocolBackup = Join-Path $testRoot 'protocol-before.reg'
$registrationChanged = $false
$desktop = $null
$daemon = $null
$daemonErrors = $null
$feed = $null
$secondaries = [Collections.Generic.List[Diagnostics.Process]]::new()
$runMutex = [Threading.Mutex]::new($false, 'Local\MultiCore.DeepLinkE2E.v1')
$runMutexHeld = $false
try {
    try { $runMutexHeld = $runMutex.WaitOne(0) }
    catch [Threading.AbandonedMutexException] { $runMutexHeld = $true }
    if (-not $runMutexHeld) { throw 'Another deep-link E2E test is running' }
    $instanceName = if ($ReleaseBuild) { 'Local\MultiCore.Desktop.SingleInstance.v1' } else { 'Local\MultiCore.Desktop.Preview.SingleInstance.v1' }
    $existingInstance = $null
    if ([Threading.Mutex]::TryOpenExisting($instanceName, [ref]$existingInstance)) {
        $existingInstance.Dispose()
        throw 'An existing desktop owns the selected instance scope; test aborted without changing registration'
    }
    $protocolExisted = Test-Path -LiteralPath $protocolPath
    if ($protocolExisted) {
        & reg.exe export 'HKCU\Software\Classes\multicore' $protocolBackup /y | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot back up protocol registration' }
    }
    $feedScript = Join-Path $PSScriptRoot 'fixtures\deep-link-subscription-server.ps1'
    $feed = Start-Hidden 'powershell.exe' ('-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "'+$feedScript+'" -OutputDirectory "'+$testRoot+'"')
    Wait-Until { Test-Path -LiteralPath (Join-Path $testRoot 'feed-port.txt') } 'Subscription fixture did not start'
    $feedPort = [IO.File]::ReadAllText((Join-Path $testRoot 'feed-port.txt')).Trim()
    $baseUrl = 'http://127.0.0.1:'+ $feedPort
    $token = [Guid]::NewGuid().ToString('N') + [Guid]::NewGuid().ToString('N')
    $env:MULTICORE_DAEMON_TOKEN = $token
    $env:MULTICORE_DAEMON_ADDR = '127.0.0.1:0'
    $env:MULTICORE_DAEMON_DATA_DIR = Join-Path $testRoot 'daemon-data'
    $env:MULTICORE_DAEMON_READY_STDOUT = '1'
    # Import prepares a controller and validates the packaged runtime layout,
    # although no core host is launched until Connect (which this test never clicks).
    $runtimeDirectory = Join-Path $testRoot 'package\runtime'
    [void][IO.Directory]::CreateDirectory($runtimeDirectory)
    $hostPath = Join-Path ([IO.Path]::GetDirectoryName($daemonPath)) 'multicore-core-host.exe'
    Copy-Item -LiteralPath $daemonPath -Destination (Join-Path $runtimeDirectory 'multicore-daemon.exe')
    Copy-Item -LiteralPath $hostPath -Destination (Join-Path $runtimeDirectory 'multicore-core-host.exe')
    $daemonInfo = [Diagnostics.ProcessStartInfo]::new()
    $daemonInfo.FileName = Join-Path $runtimeDirectory 'multicore-daemon.exe'
    $daemonInfo.UseShellExecute = $false
    $daemonInfo.CreateNoWindow = $true
    $daemonInfo.RedirectStandardOutput = $true
    $daemonInfo.RedirectStandardError = $true
    $daemon = [Diagnostics.Process]::Start($daemonInfo)
    $daemonErrors = $daemon.StandardError.ReadToEndAsync()
    $ready = $daemon.StandardOutput.ReadLineAsync()
    if (-not $ready.Wait(10000) -or $ready.Result -notmatch '^MULTICORE_READY (127\.0\.0\.1:\d+)$') { throw 'Daemon did not report readiness' }
    $api = 'http://'+$Matches[1]
    $headers = @{ Authorization = 'Bearer '+$token }
    $env:MULTICORE_DAEMON_URL = $api
    $env:MULTICORE_PREVIEW_INSTANCE = if ($ReleaseBuild) { '0' } else { '1' }
    $env:MULTICORE_SMOKE_EXIT_ON_CLOSE = '1'
    $env:LOCALAPPDATA = Join-Path $testRoot 'user-data'
    [void][IO.Directory]::CreateDirectory($env:LOCALAPPDATA)
    $env:SLINT_SCALE_FACTOR = '1'
    $registrationChanged = $true
    [void](New-Item -Path $commandPath -Force)
    [void](New-ItemProperty -LiteralPath $protocolPath -Name 'URL Protocol' -PropertyType String -Value '' -Force)
    Set-Item -LiteralPath $commandPath -Value ('"'+$desktopPath+'" "%1"')

    # Cold launch from ShellExecute; decline must cause zero subscription fetches.
    $desktop = Open-Link ($baseUrl+'/sub/cold-decline?token=a%2Fb&next=1')
    Wait-Modal 'cold-launch'
    Write-Output 'Cold ShellExecute: confirmation visible'
    Start-Sleep -Milliseconds 700
    if (@(Get-FeedRequests).Count -ne 0) { throw 'Subscription fetched before consent' }
    Click-Consent $false
    Wait-Until { -not (Save-Window '') } 'No did not close the consent dialog'
    if (@(Get-FeedRequests).Count -ne 0) { throw 'Declined subscription was fetched' }

    # Warm launch exercises the real named pipe and exact percent-encoded source persistence.
    $approvedUrl = $baseUrl+'/sub/warm-approved?token=a%2Fb&next=1'
    $secondary = Open-Link $approvedUrl
    if ($null -ne $secondary) { $secondaries.Add($secondary) }
    if ($null -ne $secondary -and -not $secondary.WaitForExit(12000)) { throw 'Secondary desktop did not forward and exit' }
    Wait-Modal 'warm-launch'
    Write-Output 'Warm ShellExecute: confirmation visible'
    if (@(Get-FeedRequests).Count -ne 0) { throw 'Warm launch fetched before consent' }
    Click-Consent $true
    Wait-Until { @(Get-Profiles).Count -eq 1 } 'Yes did not import the subscription'
    $sources = @(Get-ChildItem -LiteralPath $env:MULTICORE_DAEMON_DATA_DIR -Recurse -Filter subscription.json | ForEach-Object {
        (Get-Content -Raw -Encoding utf8 -LiteralPath $_.FullName | ConvertFrom-Json).source_url
    })
    if ($approvedUrl -cnotin $sources) { throw 'Saved subscription URL differs from the approved URI' }
    if (@(Get-FeedRequests | Where-Object { $_ -cne 'GET /sub/warm-approved?token=a%2Fb&next=1 HTTP/1.1' }).Count -gt 0) { throw 'Unexpected or incorrectly decoded subscription fetch' }
    [void](Save-Window 'imported-profile')

    # A second link must also restore a background/tray instance, and Escape must reject it.
    Stop-TestDesktop
    $desktop = Start-Hidden $desktopPath '--background'
    Wait-Until { (Get-TestWindow) -ne [IntPtr]::Zero } 'Background desktop window was not created'
    if ([DeepLinkSmokeWindow]::IsWindowVisible((Get-TestWindow))) { throw 'Background desktop unexpectedly started visible' }
    $requestsBefore = @(Get-FeedRequests).Count
    $secondary = Open-Link ($baseUrl+'/sub/tray-decline')
    if ($null -ne $secondary) { $secondaries.Add($secondary) }
    if ($null -ne $secondary -and -not $secondary.WaitForExit(12000)) { throw 'Tray activation did not forward and exit' }
    Wait-Modal 'tray-activation'
    if (@(Get-FeedRequests).Count -ne $requestsBefore) { throw 'Tray activation fetched without consent' }
    Wait-Until { [DeepLinkSmokeWindow]::GetForegroundWindow() -eq (Get-TestWindow) } 'Test window is not foreground; refusing to send keyboard input'
    [DeepLinkSmokeWindow]::Escape()
    Wait-Until { -not (Save-Window '') } 'Escape did not close the consent dialog'
    Start-Sleep -Milliseconds 500
    if (@(Get-FeedRequests).Count -ne $requestsBefore -or @(Get-Profiles).Count -ne 1) { throw 'Escape imported the declined link' }
    Write-Output ('PASS: Windows Shell cold/warm/tray activation, Yes/No/Escape, zero pre-consent fetches, exact URL persistence. Evidence: '+$testRoot)
} catch {
    if ($null -ne $desktop -and -not $desktop.HasExited) { [void](Save-Window 'failure') }
    throw
} finally {
    try {
        foreach ($process in @($desktop,$daemon,$feed) + @($secondaries.ToArray())) {
            try {
                if ($null -ne $process -and -not $process.HasExited) { $process.Kill(); [void]$process.WaitForExit(5000) }
            } catch { Write-Warning ('Could not stop an owned test process: '+$_.Exception.Message) }
        }
        if ($null -ne $daemonErrors -and $daemonErrors.Wait(1000)) {
            [IO.File]::WriteAllText((Join-Path $testRoot 'daemon-stderr.txt'), $daemonErrors.Result)
        }
    } finally {
        try {
            foreach ($name in $envNames) { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
        } finally {
            try {
                if ($registrationChanged) {
                    try {
                        if (Test-Path -LiteralPath $protocolPath) { Remove-Item -LiteralPath $protocolPath -Recurse -Force }
                    } finally {
                        if ($protocolExisted) {
                            & reg.exe import $protocolBackup | Out-Null
                            if ($LASTEXITCODE -ne 0) { throw ('Failed to restore protocol; recovery file: '+$protocolBackup) }
                        }
                    }
                }
            } finally {
                if ($runMutexHeld) { $runMutex.ReleaseMutex() }
                $runMutex.Dispose()
            }
        }
    }
}
