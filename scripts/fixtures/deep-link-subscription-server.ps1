param([Parameter(Mandatory = $true)][string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$listener.Start()
$utf8 = [Text.UTF8Encoding]::new($false)
$yaml = "proxies:`n  - name: Local test node`n    type: socks5`n    server: 127.0.0.1`n    port: 9`nproxy-groups:`n  - name: Main`n    type: select`n    proxies: [Local test node]`nrules:`n  - MATCH,Main`n"
$bundle = ConvertTo-Json -InputObject @($yaml, @{ outbounds = @(@{ protocol = 'freedom'; tag = 'direct' }) }) -Depth 5 -Compress
$body = $utf8.GetBytes($bundle)
[IO.File]::WriteAllText((Join-Path $OutputDirectory 'feed-port.txt'), [string]$listener.LocalEndpoint.Port, $utf8)
try {
    while ($true) {
        $client = $listener.AcceptTcpClient()
        try {
            $stream = $client.GetStream()
            $stream.ReadTimeout = 5000
            $reader = [IO.StreamReader]::new($stream, $utf8, $false, 1024, $true)
            try {
                $request = $reader.ReadLine()
                while (-not [string]::IsNullOrEmpty($reader.ReadLine())) {}
                if (-not $request.StartsWith('GET /sub/')) { throw 'Unexpected fixture request' }
                [IO.File]::AppendAllText((Join-Path $OutputDirectory 'feed-requests.txt'), $request + "`n", $utf8)
                $headers = "HTTP/1.1 200 OK`r`nContent-Type: application/json`r`nprofile-title: Windows Deep Link E2E`r`nContent-Length: $($body.Length)`r`nConnection: close`r`n`r`n"
                $headerBytes = [Text.Encoding]::ASCII.GetBytes($headers)
                $stream.Write($headerBytes, 0, $headerBytes.Length)
                $stream.Write($body, 0, $body.Length)
                $stream.Flush()
            } finally { $reader.Dispose() }
        } finally { $client.Dispose() }
    }
} finally { $listener.Stop() }
