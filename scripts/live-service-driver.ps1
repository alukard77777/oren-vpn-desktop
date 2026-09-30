<#
Live test driver for the always-ready MultiCore service (Windows).

Runs INSIDE the interactive session of the logged-on user. The MultiCore service only accepts
a daemon started from %ProgramFiles%\MultiCore\current\runtime by a token with the INTERACTIVE
logon SID, so SSH and PowerShell Direct sessions cannot drive it: scripts/live-service-windows.ps1
registers a one-shot interactive scheduled task that runs this file and collects its result.

ASCII only: Windows PowerShell 5.1 reads BOM-less files in the ANSI code page.

Modes:
  Service    installed with the service (/ALLUSERS): cores warm before Connect, latency and
             dashboard while disconnected, TUN connect without UAC, disconnect keeps cores,
             refresh while connected re-prepares, proxy mode, quit and crash stop the cores.
  PerUser    installed without the service (/CURRENTUSER): no service, no warm cores, latency
             needs a connection, proxy-mode connect without UAC runs the cores as the user.
  Uninstall  runs the Inno uninstaller non-silently and answers its dialogs through UI
             Automation (the "remove data" question is only asked in a non-silent uninstall).
             Service installs need an elevated task (the orchestrator registers it with
             RunLevel Highest, so no UAC prompt appears).

Output: -ResultPath receives a JSON array of { check, ok, detail }. A file "<ResultPath>.done"
is written last so the orchestrator can tell a finished run from a hung one.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [ValidateSet('Service', 'PerUser', 'Uninstall')] [string]$Mode,
    [Parameter(Mandatory)] [string]$ResultPath,
    # Package root that holds current\runtime\multicore-daemon.exe (defaults per mode).
    [string]$InstallRoot,
    # A subscription URL to import instead of the built-in local fixture (Mihomo-only, DIRECT).
    [string]$SubscriptionUrl,
    # Fetched while connected to prove traffic flows; empty skips the check.
    [string]$TrafficUrl = 'https://www.gstatic.com/generate_204',
    # Service mode: also start the desktop (MultiCore.exe --background) and kill it.
    [switch]$WithDesktop,
    # Uninstall mode: the uninstaller to run and the answer to "remove subscriptions and settings?".
    [string]$Uninstaller,
    [switch]$RemoveData
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$script:results = New-Object System.Collections.Generic.List[object]
$logDirectory = Split-Path -Parent $ResultPath
$logPath = Join-Path $logDirectory ("live-service-driver-{0}.log" -f $Mode.ToLowerInvariant())

function Write-Log([string]$Text) {
    $line = '{0:HH:mm:ss.fff} {1}' -f (Get-Date), $Text
    [IO.File]::AppendAllText($logPath, $line + "`r`n", [Text.UTF8Encoding]::new($false))
}

function Add-Result([string]$Name, [bool]$Ok, [string]$Detail) {
    $script:results.Add([pscustomobject]@{ check = $Name; ok = $Ok; detail = $Detail })
    Write-Log ("{0} {1}: {2}" -f $(if ($Ok) { 'PASS' } else { 'FAIL' }), $Name, $Detail)
}

function Check([string]$Name, [scriptblock]$Body) {
    try {
        $detail = & $Body
        Add-Result $Name $true (($detail | ForEach-Object { [string]$_ }) -join ' ')
        return $true
    } catch {
        Add-Result $Name $false $_.Exception.Message
        return $false
    }
}

function Save-Results {
    $json = ConvertTo-Json -InputObject @($script:results.ToArray()) -Depth 4
    [IO.File]::WriteAllText($ResultPath, $json, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($ResultPath + '.done', (Get-Date).ToString('o'), [Text.UTF8Encoding]::new($false))
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

# ---------------------------------------------------------------- HTTP helpers (no proxy)

function Invoke-Http {
    param(
        [string]$Method,
        [string]$Uri,
        [hashtable]$Headers = @{},
        [string]$Body,
        [int]$TimeoutSec = 30,
        [switch]$UseSystemProxy
    )
    $request = [Net.HttpWebRequest]::Create($Uri)
    $request.Method = $Method
    $request.Timeout = $TimeoutSec * 1000
    $request.ReadWriteTimeout = $TimeoutSec * 1000
    $request.AllowAutoRedirect = $false
    $request.KeepAlive = $false
    if (-not $UseSystemProxy) { $request.Proxy = $null }
    foreach ($name in $Headers.Keys) { $request.Headers[$name] = [string]$Headers[$name] }
    if ($PSBoundParameters.ContainsKey('Body') -and $null -ne $Body) {
        $bytes = [Text.Encoding]::UTF8.GetBytes($Body)
        $request.ContentType = 'application/json'
        $request.ContentLength = $bytes.Length
        $stream = $request.GetRequestStream()
        try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
    } elseif ($Method -ne 'GET') {
        $request.ContentLength = 0
    }
    $response = $null
    try {
        $response = $request.GetResponse()
    } catch {
        # A .NET method call surfaces as MethodInvocationException wrapping the WebException.
        $exception = $_.Exception
        while ($exception -and -not ($exception -is [Net.WebException])) { $exception = $exception.InnerException }
        if ($null -eq $exception -or $null -eq $exception.Response) {
            return [pscustomobject]@{ Status = 0; Body = $_.Exception.Message }
        }
        $response = $exception.Response
    }
    try {
        $reader = New-Object IO.StreamReader($response.GetResponseStream(), [Text.Encoding]::UTF8)
        $text = $reader.ReadToEnd()
        $reader.Dispose()
        [pscustomobject]@{ Status = [int]$response.StatusCode; Body = $text }
    } finally {
        $response.Close()
    }
}

function Api([string]$Method, [string]$Path, [string]$Body, [int]$TimeoutSec = 120) {
    $arguments = @{
        Method = $Method; Uri = "$script:base$Path"; TimeoutSec = $TimeoutSec
        Headers = @{ Authorization = "Bearer $script:token" }
    }
    if ($PSBoundParameters.ContainsKey('Body') -and $Body) { $arguments.Body = $Body }
    $response = Invoke-Http @arguments
    if ($response.Status -lt 200 -or $response.Status -ge 300) {
        throw "$Method $Path -> $($response.Status) $($response.Body)"
    }
    if ($response.Body) { $response.Body | ConvertFrom-Json } else { $null }
}

function Get-Status { Api GET '/v1/status' }

# A failed check can leave the tunnel up; later checks start from a disconnected daemon.
function Reset-Disconnected {
    try {
        if ((Get-Status).state -in 'connected', 'connecting') {
            [void](Api POST '/v1/disconnect')
            Wait-Until { (Get-Status).state -notin 'connected', 'connecting' } 30 'disconnect' | Out-Null
        }
    } catch { }
}

# ---------------------------------------------------------------- process helpers

function Get-CoreProcesses([string]$Name, [int]$SessionId) {
    @(Get-Process -Name $Name -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq $SessionId })
}

function Get-ServiceCorePids {
    $ids = @(Get-CoreProcesses 'mihomo' 0) + @(Get-CoreProcesses 'xray' 0) | ForEach-Object { $_.Id }
    (@($ids) | Sort-Object) -join ','
}

function Get-AdapterStatus {
    $adapter = Get-NetAdapter -Name 'MultiCore' -ErrorAction SilentlyContinue
    if ($adapter) { [string]$adapter.Status } else { 'absent' }
}

function Get-ProxySettings {
    $settings = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings'
    $server = ''
    if ($settings.PSObject.Properties['ProxyServer']) { $server = [string]$settings.ProxyServer }
    [pscustomobject]@{ Enable = [int]$settings.ProxyEnable; Server = $server }
}

function Read-SharedText([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return '' }
    try {
        $sharing = [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
        $stream = [IO.FileStream]::new($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, $sharing)
        $reader = [IO.StreamReader]::new($stream)
        try { $reader.ReadToEnd() } finally { $reader.Dispose() }
    } catch [IO.IOException] { '' }
}

function Get-FreePort {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    try { $listener.LocalEndpoint.Port } finally { $listener.Stop() }
}

# ---------------------------------------------------------------- local subscription fixture

# A Mihomo-only subscription (no loopback SOCKS bridge, so the Xray half is optional) whose only
# nodes are DIRECT: nothing is proxied anywhere. Revision 2 adds a node so a refresh produces a
# new snapshot and the daemon must re-prepare the service cores.
$fixtureServer = @'
param($listener, $state)
function Yaml([int]$revision) {
    $nodes = @('live-direct-a')
    if ($revision -ge 2) { $nodes += 'live-direct-b' }
    $lines = @('mode: rule', 'proxies:')
    foreach ($node in $nodes) { $lines += "  - {name: $node, type: direct, udp: true}" }
    $lines += 'proxy-groups:'
    $lines += ('  - {name: LiveTest, type: select, proxies: [' + ($nodes -join ', ') + ']}')
    $lines += 'rules:'
    $lines += '  - MATCH,LiveTest'
    ($lines -join "`n") + "`n"
}
while (-not $state.Stop) {
    if (-not $listener.Pending()) { Start-Sleep -Milliseconds 50; continue }
    $client = $listener.AcceptTcpClient()
    try {
        $stream = $client.GetStream()
        $stream.ReadTimeout = 5000
        $reader = New-Object IO.StreamReader($stream, [Text.Encoding]::ASCII)
        $requestLine = $reader.ReadLine()
        while ($true) { $header = $reader.ReadLine(); if ([string]::IsNullOrEmpty($header)) { break } }
        $state.Requests = $state.Requests + 1
        $body = [Text.Encoding]::UTF8.GetBytes((Yaml $state.Revision))
        $head = "HTTP/1.1 200 OK`r`nContent-Type: text/yaml; charset=utf-8`r`n" +
            "Profile-Title: MultiCore live test`r`nContent-Length: $($body.Length)`r`nConnection: close`r`n`r`n"
        $headBytes = [Text.Encoding]::ASCII.GetBytes($head)
        $stream.Write($headBytes, 0, $headBytes.Length)
        $stream.Write($body, 0, $body.Length)
        $stream.Flush()
    } catch {
    } finally {
        $client.Close()
    }
}
'@

function Start-Fixture {
    $script:fixtureState = [hashtable]::Synchronized(@{ Revision = 1; Requests = 0; Stop = $false })
    $script:fixtureListener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $script:fixtureListener.Start()
    $script:fixturePs = [PowerShell]::Create()
    [void]$script:fixturePs.AddScript($fixtureServer).AddArgument($script:fixtureListener).AddArgument($script:fixtureState)
    $script:fixtureHandle = $script:fixturePs.BeginInvoke()
    "http://127.0.0.1:$($script:fixtureListener.LocalEndpoint.Port)/sub/live-test"
}

function Stop-Fixture {
    if ($null -ne $script:fixtureState) {
        $script:fixtureState.Stop = $true
        try { [void]$script:fixturePs.EndInvoke($script:fixtureHandle) } catch { }
        try { $script:fixtureListener.Stop() } catch { }
        $script:fixturePs.Dispose()
    }
}

# ---------------------------------------------------------------- daemon lifecycle

$script:daemon = $null
$script:fixtureState = $null
$script:subscription = $null
$script:warmPids = ''
$script:nodesBefore = 0
$script:token = ''
$script:base = ''

function Start-Daemon {
    $daemonExe = Join-Path $InstallRoot 'current\runtime\multicore-daemon.exe'
    if (-not (Test-Path -LiteralPath $daemonExe -PathType Leaf)) { throw "daemon missing: $daemonExe" }
    $script:token = -join ((1..32) | ForEach-Object { '{0:x2}' -f (Get-Random -Maximum 256) })
    $stamp = [DateTime]::Now.ToString('HHmmssfff')
    $readyPath = Join-Path $logDirectory "daemon-$stamp.stdout.txt"
    $errorPath = Join-Path $logDirectory "daemon-$stamp.stderr.txt"
    # The desktop starts the daemon exactly like this (see scripts/smoke-test-packaged-daemon.ps1).
    # The service resolves the staged generation from %LOCALAPPDATA%\MultiCore of the client
    # token, so the data directory must be the real one (the orchestrator backs it up).
    $env:MULTICORE_DAEMON_TOKEN = $script:token
    $env:MULTICORE_DAEMON_ADDR = '127.0.0.1:0'
    $env:MULTICORE_DAEMON_DATA_DIR = Join-Path $env:LOCALAPPDATA 'MultiCore'
    $env:MULTICORE_DAEMON_READY_STDOUT = '1'
    $env:MULTICORE_MIHOMO_CONTROLLER_ADDR = "127.0.0.1:$(Get-FreePort)"
    $script:daemon = Start-Process -FilePath $daemonExe -WorkingDirectory (Split-Path -Parent $daemonExe) `
        -RedirectStandardOutput $readyPath -RedirectStandardError $errorPath -PassThru -WindowStyle Hidden
    $deadline = (Get-Date).AddSeconds(30)
    $ready = ''
    do {
        Start-Sleep -Milliseconds 100
        $script:daemon.Refresh()
        $ready = Read-SharedText $readyPath
    } until ($script:daemon.HasExited -or $ready.Contains("`n") -or (Get-Date) -ge $deadline)
    if ($script:daemon.HasExited) { throw "daemon exited before readiness: $(Read-SharedText $errorPath)" }
    $match = [regex]::Match($ready, '^MULTICORE_READY (127\.0\.0\.1:\d{1,5})\r?\n')
    if (-not $match.Success) { throw "invalid readiness line: '$ready'" }
    $script:base = 'http://' + $match.Groups[1].Value
    "pid $($script:daemon.Id) at $script:base"
}

function Stop-Daemon {
    if ($script:daemon -and -not $script:daemon.HasExited) {
        Stop-Process -Id $script:daemon.Id -Force
        [void]$script:daemon.WaitForExit(10000)
    }
}

function Save-DaemonEvidence([string]$Label) {
    foreach ($path in '/v1/events?after=0', '/v1/diagnostics') {
        try {
            $response = Invoke-Http -Method GET -Uri "$script:base$path" -Headers @{ Authorization = "Bearer $script:token" }
            $name = ($path -replace '[^a-z]', '-').Trim('-')
            [IO.File]::WriteAllText((Join-Path $logDirectory "$Label-$name.json"), $response.Body, [Text.UTF8Encoding]::new($false))
        } catch { }
    }
}

# ---------------------------------------------------------------- common preflight

function Test-Preflight([bool]$ExpectElevated) {
    Check 'session-interactive' {
        $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
        $interactive = @($identity.Groups | Where-Object { $_.Value -eq 'S-1-5-4' }).Count -gt 0
        if (-not $interactive) { throw 'token has no INTERACTIVE SID: the service would reject this daemon' }
        "user $($identity.Name), session $((Get-Process -Id $PID).SessionId)"
    } | Out-Null
    Check 'elevation' {
        $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
        $elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        if ($elevated -ne $ExpectElevated) { throw "elevated=$elevated, expected $ExpectElevated" }
        "elevated=$elevated"
    } | Out-Null
}

function Import-Subscription {
    Check 'fixture-subscription' {
        if (-not $script:subscription) { $script:subscription = Start-Fixture }
        $script:subscription
    } | Out-Null
    Check 'import' {
        $status = Api POST '/v1/subscriptions/import' (@{ url = $script:subscription } | ConvertTo-Json -Compress)
        "state=$($status.state) profile=$($status.profile)"
    }
}

# ================================================================ Service mode

function Invoke-ServiceMode {
    Test-Preflight $false
    Check 'service-running' {
        $service = Get-Service -Name 'MultiCore'
        if ($service.Status -ne 'Running') { throw "service is $($service.Status)" }
        'Running'
    } | Out-Null
    Check 'no-cores-before-daemon' {
        $ids = Get-ServiceCorePids
        if ($ids) { throw "service cores already running: $ids" }
        'idle'
    } | Out-Null
    if (-not (Check 'daemon-start' { Start-Daemon })) { return }
    Check 'service-mode' {
        $status = Get-Status
        if (-not $status.service_mode) { throw 'daemon did not select the service (service_mode=false)' }
        "service_mode=true state=$($status.state)"
    } | Out-Null
    if (-not (Import-Subscription)) { return }

    Check 'cores-warm-before-connect' {
        $status = Wait-Until { $s = Get-Status; if ($s.cores_ready) { $s } } 90 'cores_ready'
        if ($status.state -eq 'connected') { throw 'daemon connected on its own' }
        $script:warmPids = Get-ServiceCorePids
        if (-not $script:warmPids) { throw 'cores_ready but no mihomo/xray in session 0' }
        "state=$($status.state) service cores $script:warmPids"
    } | Out-Null
    Check 'no-user-session-core-host' {
        $mine = @(Get-Process -Name 'multicore-core-host', 'mihomo', 'xray' -ErrorAction SilentlyContinue |
            Where-Object { $_.SessionId -ne 0 })
        if ($mine.Count) { throw "cores or core host in the user session: $(($mine | ForEach-Object { "$($_.Name):$($_.Id)" }) -join ',')" }
        'none'
    } | Out-Null
    Check 'catalog-before-connect' {
        $catalog = Api GET '/v1/catalog'
        $nodes = @($catalog.groups | ForEach-Object { $_.nodes }).Count
        if ($nodes -lt 1) { throw 'catalog has no nodes' }
        $script:nodesBefore = $nodes
        "revision=$($catalog.revision) groups=$(@($catalog.groups).Count) nodes=$nodes"
    } | Out-Null
    Check 'latency-before-connect' {
        $response = Api POST '/v1/latencies' '{}'
        $entries = @($response.entries)
        if ($entries.Count -lt 1) { throw 'no latency entries' }
        if ((Get-Status).state -eq 'connected') { throw 'latency probe connected the tunnel' }
        ($entries | ForEach-Object { "$($_.status):$($_.latency_ms)" }) -join ' '
    } | Out-Null
    Check 'dashboard-before-connect' {
        $script:dashboardUrl = (Api GET '/v1/dashboard-url').url
        if ($script:dashboardUrl -notmatch '^http://127\.0\.0\.1:(\d+)/ui/.*[?&]secret=([0-9a-f]+)') { throw 'unexpected dashboard URL shape' }
        $script:dashboardPort = $Matches[1]
        $script:dashboardToken = $Matches[2]
        $page = Invoke-Http -Method GET -Uri "http://127.0.0.1:$script:dashboardPort/ui/"
        if ($page.Status -ne 200) { throw "dashboard page returned $($page.Status)" }
        "port $script:dashboardPort, page 200"
    } | Out-Null
    Check 'dashboard-read-only' {
        $auth = @{ Authorization = "Bearer $script:dashboardToken" }
        $root = "http://127.0.0.1:$script:dashboardPort"
        $version = Invoke-Http -Method GET -Uri "$root/version" -Headers $auth
        if ($version.Status -ne 200) { throw "GET /version returned $($version.Status)" }
        $denied = @(
            @('PATCH', '/configs', '{"mode":"global"}'),
            @('PUT', '/configs', '{"payload":"mode: global"}'),
            @('POST', '/upgrade', '{}'),
            @('PUT', '/proxies/LiveTest', '{"name":"live-direct-a"}'),
            @('GET', '/proxies', $null)
        )
        $codes = foreach ($request in $denied) {
            $arguments = @{ Method = $request[0]; Uri = "$root$($request[1])"; Headers = $auth }
            if ($request[2]) { $arguments.Body = $request[2] }
            $code = (Invoke-Http @arguments).Status
            if ($code -ne 403) { throw "$($request[0]) $($request[1]) returned $code, expected 403" }
            "$($request[0]) $($request[1])=403"
        }
        $anonymous = (Invoke-Http -Method GET -Uri "$root/version").Status
        if ($anonymous -eq 200) { throw 'GET /version without the token was allowed' }
        "GET /version=200 no-token=$anonymous " + ($codes -join ' ')
    } | Out-Null

    $connected = Check 'connect-tun-without-uac' {
        $watch = [Diagnostics.Stopwatch]::StartNew()
        [void](Api POST '/v1/connect' '{"tun":true}')
        $status = Wait-Until { $s = Get-Status; if ($s.state -eq 'connected') { $s } } 60 'state=connected'
        $watch.Stop()
        if (@(Get-Process -Name 'consent' -ErrorAction SilentlyContinue).Count) { throw 'a UAC consent prompt is open' }
        $elevatedHost = @(Get-Process -Name 'multicore-core-host' -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 })
        if ($elevatedHost.Count) { throw 'an on-demand elevated core host was started (fallback instead of the service)' }
        "connected in $($watch.ElapsedMilliseconds) ms, node=$($status.current_node)"
    }
    if ($connected) {
        Check 'connect-reuses-warm-cores' {
            $now = Get-ServiceCorePids
            if ($now -ne $script:warmPids) { throw "cores restarted on connect: $script:warmPids -> $now" }
            $now
        } | Out-Null
        Check 'adapter-up' {
            Wait-Until { (Get-AdapterStatus) -eq 'Up' } 30 'MultiCore adapter Up' | Out-Null
            'MultiCore Up'
        } | Out-Null
        if ($TrafficUrl) {
            Check 'traffic' {
                $response = Invoke-Http -Method GET -Uri $TrafficUrl -TimeoutSec 20 -UseSystemProxy
                if ($response.Status -lt 200 -or $response.Status -ge 400) { throw "status $($response.Status) $($response.Body)" }
                "HTTP $($response.Status)"
            } | Out-Null
        }
        Check 'disconnect-keeps-cores' {
            [void](Api POST '/v1/disconnect')
            $status = Wait-Until { $s = Get-Status; if ($s.state -ne 'connected' -and $s.state -ne 'connecting' -and $s.cores_ready) { $s } } 30 'disconnected and cores_ready'
            $now = Get-ServiceCorePids
            if ($now -ne $script:warmPids) { throw "cores changed on disconnect: $script:warmPids -> $now" }
            "state=$($status.state) cores $now"
        } | Out-Null
        Check 'adapter-down-after-disconnect' {
            Wait-Until { (Get-AdapterStatus) -ne 'Up' } 30 'MultiCore adapter not Up' | Out-Null
            Get-AdapterStatus
        } | Out-Null
    }

    Reset-Disconnected
    Check 'refresh-while-connected-reprepares' {
        [void](Api POST '/v1/connect' '{"tun":true}')
        Wait-Until { (Get-Status).state -eq 'connected' } 60 'state=connected' | Out-Null
        $before = Get-ServiceCorePids
        if ($null -eq $script:fixtureState) { throw 'needs the built-in fixture (an external subscription may not change on refresh)' }
        $script:fixtureState.Revision = 2
        [void](Api POST '/v1/subscriptions/refresh')
        $after = Wait-Until { $p = Get-ServiceCorePids; $s = Get-Status; if ($p -and $p -ne $before -and $s.cores_ready) { $p } } 90 'new service cores after refresh'
        $status = Get-Status
        $nodes = @((Api GET '/v1/catalog').groups | ForEach-Object { $_.nodes }).Count
        if ($nodes -le $script:nodesBefore) { throw "catalog did not change after refresh ($nodes nodes)" }
        $adapter = Get-AdapterStatus
        [void](Api POST '/v1/disconnect')
        Wait-Until { $s = Get-Status; $s.state -ne 'connected' -and $s.cores_ready } 60 'disconnected and warm' | Out-Null
        $script:warmPids = Get-ServiceCorePids
        "cores $before -> $after, nodes $script:nodesBefore -> $nodes, state after refresh=$($status.state), adapter=$adapter"
    } | Out-Null

    Reset-Disconnected
    Check 'proxy-mode-through-service' {
        $original = Get-ProxySettings
        [void](Api POST '/v1/connect' '{"tun":false}')
        Wait-Until { (Get-Status).state -eq 'connected' } 60 'state=connected (proxy)' | Out-Null
        $during = Get-ProxySettings
        $adapter = Get-AdapterStatus
        $userCores = @(Get-Process -Name 'mihomo' -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 }).Count
        [void](Api POST '/v1/disconnect')
        Wait-Until { (Get-Status).state -ne 'connected' } 30 'disconnected (proxy)' | Out-Null
        $after = Get-ProxySettings
        if ($during.Enable -ne 1 -or $during.Server -notmatch '127\.0\.0\.1:\d+') { throw "system proxy not set: $($during.Enable) $($during.Server)" }
        if ($adapter -eq 'Up') { throw 'TUN adapter is up in proxy mode' }
        if ($userCores) { throw 'proxy mode started user-session cores instead of using the service' }
        if ($after.Enable -ne $original.Enable -or $after.Server -ne $original.Server) { throw "proxy not restored: $($after.Enable) $($after.Server)" }
        "during $($during.Server), restored to $($after.Enable)"
    } | Out-Null

    Reset-Disconnected
    Check 'quit-stops-cores' {
        Save-DaemonEvidence 'service-first-daemon'
        if (-not (Get-ServiceCorePids)) { throw 'cores were not running before quit' }
        Stop-Daemon
        Wait-Until { -not (Get-ServiceCorePids) } 30 'service cores to stop' | Out-Null
        if ((Get-Service -Name 'MultiCore').Status -ne 'Running') { throw 'service stopped instead of idling' }
        'cores stopped, service idle'
    } | Out-Null

    if (Check 'daemon-restart-rewarms' { Start-Daemon; Wait-Until { (Get-Status).cores_ready } 90 'cores_ready' | Out-Null; Get-ServiceCorePids }) {
        Check 'crash-while-connected-stops-cores' {
            $proxyBefore = Get-ProxySettings
            [void](Api POST '/v1/connect' '{"tun":true}')
            Wait-Until { (Get-Status).state -eq 'connected' } 60 'state=connected' | Out-Null
            Wait-Until { (Get-AdapterStatus) -eq 'Up' } 30 'adapter Up' | Out-Null
            Save-DaemonEvidence 'service-second-daemon'
            Stop-Process -Id $script:daemon.Id -Force
            Wait-Until { -not (Get-ServiceCorePids) } 30 'service cores to stop after the crash' | Out-Null
            Wait-Until { (Get-AdapterStatus) -ne 'Up' } 30 'adapter down after the crash' | Out-Null
            $proxyAfter = Get-ProxySettings
            if ($proxyAfter.Enable -ne $proxyBefore.Enable) { throw 'system proxy changed by the crash' }
            if ((Get-Service -Name 'MultiCore').Status -ne 'Running') { throw 'service did not survive the client crash' }
            "cores stopped, adapter $(Get-AdapterStatus), service Running"
        } | Out-Null
    }

    if ($WithDesktop) {
        Check 'desktop-warms-and-exit-stops-cores' {
            $desktopExe = Join-Path $InstallRoot 'current\MultiCore.exe'
            $desktop = Start-Process -FilePath $desktopExe -ArgumentList '--background' -WorkingDirectory (Split-Path -Parent $desktopExe) -PassThru
            try {
                $pids = Wait-Until { Get-ServiceCorePids } 120 'cores warmed by the desktop'
                $daemons = @(Get-Process -Name 'multicore-daemon' -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 })
                if ($daemons.Count -ne 1) { throw "expected one desktop daemon, found $($daemons.Count)" }
            } finally {
                Stop-Process -Id $desktop.Id -Force -ErrorAction SilentlyContinue
            }
            # The daemon lives in the desktop's kill-on-close job; its channel drop stops the cores.
            Wait-Until { -not (Get-Process -Name 'multicore-daemon' -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 }) } 30 'desktop daemon to exit' | Out-Null
            Wait-Until { -not (Get-ServiceCorePids) } 30 'service cores to stop after the desktop exit' | Out-Null
            "desktop warmed cores $pids; killing it stopped them"
        } | Out-Null
    }
}

# ================================================================ PerUser mode

function Invoke-PerUserMode {
    Test-Preflight $false
    Check 'service-absent' {
        if (Get-Service -Name 'MultiCore' -ErrorAction SilentlyContinue) { throw 'the MultiCore service is registered' }
        'absent'
    } | Out-Null
    if (-not (Check 'daemon-start' { Start-Daemon })) { return }
    Check 'on-demand-mode' {
        $status = Get-Status
        if ($status.service_mode) { throw 'per-user daemon selected the service' }
        "service_mode=false state=$($status.state)"
    } | Out-Null
    if (-not (Import-Subscription)) { return }
    Check 'no-warm-cores-without-service' {
        Start-Sleep -Seconds 5
        $status = Get-Status
        $cores = @(Get-Process -Name 'mihomo', 'xray' -ErrorAction SilentlyContinue).Count
        if ($status.cores_ready -or $cores) { throw "cores running while disconnected (cores_ready=$($status.cores_ready), processes=$cores)" }
        'none'
    } | Out-Null
    Check 'latency-needs-connection-without-service' {
        $response = Invoke-Http -Method POST -Uri "$script:base/v1/latencies" -Headers @{ Authorization = "Bearer $script:token" } -Body '{}'
        if ($response.Status -ge 200 -and $response.Status -lt 300) { throw 'latency answered without cores' }
        "HTTP $($response.Status)"
    } | Out-Null
    Check 'proxy-connect-without-uac' {
        $original = Get-ProxySettings
        [void](Api POST '/v1/connect' '{"tun":false}')
        $status = Wait-Until { $s = Get-Status; if ($s.state -eq 'connected') { $s } } 60 'state=connected (proxy)'
        $session = (Get-Process -Id $PID).SessionId
        $cores = @(Get-CoreProcesses 'mihomo' $session).Count
        $during = Get-ProxySettings
        if (@(Get-Process -Name 'consent' -ErrorAction SilentlyContinue).Count) { throw 'a UAC consent prompt is open' }
        if (-not $cores) { throw 'no user-session mihomo while connected in proxy mode' }
        if ($during.Enable -ne 1) { throw 'system proxy not enabled' }
        if ($TrafficUrl) {
            $traffic = Invoke-Http -Method GET -Uri $TrafficUrl -TimeoutSec 20 -UseSystemProxy
            if ($traffic.Status -lt 200 -or $traffic.Status -ge 400) { throw "traffic through the proxy failed: $($traffic.Status)" }
        }
        [void](Api POST '/v1/disconnect')
        Wait-Until { (Get-Status).state -ne 'connected' } 30 'disconnected' | Out-Null
        Wait-Until { -not (Get-CoreProcesses 'mihomo' $session) } 30 'user cores to stop' | Out-Null
        $after = Get-ProxySettings
        if ($after.Enable -ne $original.Enable -or $after.Server -ne $original.Server) { throw 'system proxy not restored' }
        "connected via $($during.Server) as the user; cores stopped on disconnect"
    } | Out-Null
    Save-DaemonEvidence 'peruser-daemon'
    Stop-Daemon
}

# ================================================================ Uninstall mode

function Invoke-UninstallMode {
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
    # The installer's Russian "remove subscriptions and settings?" question, as UTF-8 Base64.
    $dataQuestion = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('0KPQtNCw0LvQuNGC0Ywg0L/QvtC00L/QuNGB0LrQuCDQuCDQvdCw0YHRgtGA0L7QudC60Lg/'))
    Check 'uninstaller-present' {
        if (-not (Test-Path -LiteralPath $Uninstaller -PathType Leaf)) { throw "missing $Uninstaller" }
        $Uninstaller
    } | Out-Null
    Check 'uninstall-interactive' {
        $started = Get-Date
        $process = Start-Process -FilePath $Uninstaller -PassThru
        $answered = New-Object System.Collections.Generic.List[string]
        $root = [Windows.Automation.AutomationElement]::RootElement
        $windowCondition = New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::ControlTypeProperty, [Windows.Automation.ControlType]::Window)
        $deadline = (Get-Date).AddMinutes(5)
        while ((Get-Date) -lt $deadline) {
            # Inno's uninstaller re-launches itself from %TEMP% as _iu*.tmp; follow both images.
            $owners = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
                    ($_.Id -eq $process.Id) -or ($_.ProcessName -like '_iu*') -or ($_.ProcessName -like 'unins0*')
                } | Where-Object { $_.StartTime -ge $started.AddSeconds(-2) } | ForEach-Object { $_.Id })
            if (-not $owners.Count -and $process.HasExited) { break }
            foreach ($window in $root.FindAll([Windows.Automation.TreeScope]::Children, $windowCondition)) {
                if ($owners -notcontains $window.Current.ProcessId) { continue }
                $text = (@($window.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition)) |
                        ForEach-Object { $_.Current.Name }) -join ' | '
                $buttons = @($window.FindAll([Windows.Automation.TreeScope]::Descendants,
                        (New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::ControlTypeProperty, [Windows.Automation.ControlType]::Button))))
                # MessageBox button control IDs: IDOK=1, IDYES=6, IDNO=7.
                $wanted = if ($text.Contains($dataQuestion)) { if ($RemoveData) { '6' } else { '7' } } elseif (@($buttons | Where-Object { $_.Current.AutomationId -eq '6' }).Count) { '6' } else { '1' }
                $button = @($buttons | Where-Object { $_.Current.AutomationId -eq $wanted }) | Select-Object -First 1
                if (-not $button) { continue }
                $pattern = $button.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern)
                $pattern.Invoke()
                $label = if ($text.Contains($dataQuestion)) { 'data-question' } else { 'dialog' }
                $answered.Add("$label->$wanted")
                Start-Sleep -Milliseconds 700
            }
            Start-Sleep -Milliseconds 300
        }
        if (-not $answered.Count) { throw 'no uninstaller dialog was answered' }
        if (-not ($answered | Where-Object { $_ -like 'data-question*' })) { throw "the data question was never asked (answered: $($answered -join ', '))" }
        "answered: $($answered -join ', ')"
    } | Out-Null
}

# ================================================================ main

try {
    New-Item -ItemType Directory -Force -Path $logDirectory | Out-Null
    Remove-Item -LiteralPath $ResultPath, ($ResultPath + '.done') -Force -ErrorAction SilentlyContinue
    Write-Log "driver start: mode=$Mode user=$env:USERNAME"
    if (-not $InstallRoot) {
        $InstallRoot = if ($Mode -eq 'PerUser') { Join-Path $env:LOCALAPPDATA 'Programs\MultiCore' } else { Join-Path $env:ProgramFiles 'MultiCore' }
    }
    if ($SubscriptionUrl) { $script:subscription = $SubscriptionUrl }
    # Task Scheduler cannot pass an empty argument reliably; 'none' disables the traffic check.
    if ($TrafficUrl -eq 'none') { $TrafficUrl = '' }
    switch ($Mode) {
        'Service' { Invoke-ServiceMode }
        'PerUser' { Invoke-PerUserMode }
        'Uninstall' { Invoke-UninstallMode }
    }
} catch {
    Add-Result 'driver' $false ("unexpected: " + $_.Exception.Message + ' ' + $_.InvocationInfo.PositionMessage)
} finally {
    try { Stop-Daemon } catch { }
    try { Stop-Fixture } catch { }
    Save-Results
}
