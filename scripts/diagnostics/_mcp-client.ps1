# Minimal MCP-over-HTTP client for the live Synapse daemon.
$script:SynToken = (Get-Content "$env:APPDATA\synapse\token.txt" -Raw).Trim()
$script:SynBase  = 'http://127.0.0.1:7700/mcp'

function New-SynSession {
    param([string]$ClientName = 'fsv-client')
    $h = @{ Authorization = "Bearer $script:SynToken"; 'Content-Type'='application/json'; 'Accept'='application/json, text/event-stream' }
    $init = @{ jsonrpc='2.0'; id=1; method='initialize'; params=@{ protocolVersion='2025-06-18'; capabilities=@{}; clientInfo=@{ name=$ClientName; version='1' } } } | ConvertTo-Json -Depth 6
    $resp = Invoke-WebRequest -Uri $script:SynBase -Method Post -Headers $h -Body $init -TimeoutSec 30
    $sid = $resp.Headers['Mcp-Session-Id']
    if ($sid -is [array]) { $sid = $sid[0] }
    # required notifications/initialized
    $h2 = $h.Clone(); $h2['Mcp-Session-Id'] = $sid
    $note = @{ jsonrpc='2.0'; method='notifications/initialized' } | ConvertTo-Json
    try { Invoke-WebRequest -Uri $script:SynBase -Method Post -Headers $h2 -Body $note -TimeoutSec 30 | Out-Null } catch {}
    return $sid
}

function Invoke-SynRpc {
    param([string]$SessionId, [string]$Method, $Params, [int]$TimeoutSec = 120)
    $h = @{ Authorization = "Bearer $script:SynToken"; 'Content-Type'='application/json'; 'Accept'='application/json, text/event-stream'; 'Mcp-Session-Id'=$SessionId }
    $body = @{ jsonrpc='2.0'; id=[int](Get-Random -Minimum 2 -Maximum 100000); method=$Method }
    if ($null -ne $Params) { $body['params'] = $Params }
    $json = $body | ConvertTo-Json -Depth 12
    $resp = Invoke-WebRequest -Uri $script:SynBase -Method Post -Headers $h -Body $json -TimeoutSec $TimeoutSec
    foreach ($line in ($resp.Content -split "`r?`n")) {
        if ($line.StartsWith('data: ') -and $line.Length -gt 6) {
            $payload = $line.Substring(6)
            if ($payload.StartsWith('{')) { return ($payload | ConvertFrom-Json) }
        }
    }
    if ($resp.Content.TrimStart().StartsWith('{')) { return ($resp.Content | ConvertFrom-Json) }
    throw "no JSON payload in response: $($resp.Content)"
}

function Invoke-SynTool {
    param([string]$SessionId, [string]$Name, $Arguments = @{}, [int]$TimeoutSec = 120)
    $r = Invoke-SynRpc -SessionId $SessionId -Method 'tools/call' -Params @{ name=$Name; arguments=$Arguments } -TimeoutSec $TimeoutSec
    if ($r.error) { return [pscustomobject]@{ ok=$false; error=$r.error; raw=$r } }
    $text = $null
    if ($r.result.content) { $text = ($r.result.content | Where-Object { $_.type -eq 'text' } | ForEach-Object { $_.text }) -join "`n" }
    $obj = $null
    if ($text -and $text.TrimStart().StartsWith('{')) { try { $obj = $text | ConvertFrom-Json } catch {} }
    return [pscustomobject]@{ ok = (-not $r.result.isError); isError = $r.result.isError; text = $text; obj = $obj; raw = $r }
}
