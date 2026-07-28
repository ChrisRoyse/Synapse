<#
.SYNOPSIS
    Prove, up front, which best-practice research lanes are actually usable.

.DESCRIPTION
    The repo workflow requires independent best-practice research AFTER a defect
    is diagnosed and BEFORE a fix is proposed. Issue #1864 recorded that agents
    kept discovering the Exa MCP lane was dead at the moment of use, mid-task,
    across multiple sessions and multiple agents — each one re-diagnosing the
    same outage from scratch.

    This script makes that state discoverable in seconds instead. It drives the
    configured Exa MCP server exactly as an MCP client does — stdio,
    newline-delimited JSON-RPC 2.0, real `initialize` handshake, real
    `tools/list`, real `tools/call` — and reports what physically happened.

    It deliberately does NOT paper over a failure. A lane that cannot serve a
    real query is reported DOWN with the exact server error text, so the caller
    knows precisely what failed and what would fix it.

.PARAMETER TimeoutSeconds
    Per-response wait budget. `npx` cold starts are slow; the default allows for
    a package download.

.PARAMETER Probe
    The real query issued to prove the lane serves results rather than merely
    listing tools. Listing tools proves registration; only a call proves service.

.EXAMPLE
    pwsh -File scripts\check-research-lane.ps1
#>
[CmdletBinding()]
param(
    [int]$TimeoutSeconds = 90,
    [string]$Probe = 'windows job object kill on job close semantics'
)

$ErrorActionPreference = 'Stop'

function Write-Section { param([string]$Text) Write-Host "`n=== $Text ===" }

$report = [ordered]@{
    schema           = 'synapse_research_lane_readback/v1'
    observed_at_utc  = (Get-Date).ToUniversalTime().ToString('o')
    host             = $env:COMPUTERNAME
    lanes            = @()
}

# ---------------------------------------------------------------------------
# Lane: Exa MCP (stdio)
# ---------------------------------------------------------------------------
Write-Section 'Exa MCP lane'

$exa = [ordered]@{
    lane                 = 'exa_mcp'
    configured           = $false
    config_source        = $null
    command              = $null
    launcher_path        = $null
    launcher_exit_code   = $null
    launcher_stderr_tail = $null
    api_key_configured   = $false
    launched             = $false
    initialized          = $false
    server_name          = $null
    server_version       = $null
    tools_listed         = @()
    call_attempted       = $false
    call_succeeded       = $false
    failure_code         = $null
    failure_detail       = $null
    verdict              = 'unknown'
}

# The MCP client config is the Source of Truth for whether the lane exists at
# all. Read it rather than assuming the launcher command.
$claudeConfigPath = Join-Path $env:USERPROFILE '.claude.json'
if (Test-Path -LiteralPath $claudeConfigPath) {
    $exa.config_source = $claudeConfigPath
    try {
        $claudeConfig = Get-Content -LiteralPath $claudeConfigPath -Raw | ConvertFrom-Json -AsHashtable
        $servers = $claudeConfig['mcpServers']
        if ($servers -and $servers.ContainsKey('exa')) {
            $entry = $servers['exa']
            $exa.configured = $true
            $exa.command = (@($entry['command']) + @($entry['args'])) -join ' '
            $keys = @()
            if ($entry['env']) { $keys = @($entry['env'].Keys) }
            $exa.api_key_configured = ($keys -contains 'EXA_API_KEY')
        }
    } catch {
        $exa.failure_code = 'MCP_CLIENT_CONFIG_UNREADABLE'
        $exa.failure_detail = $_.Exception.Message
    }
}

if (-not $exa.configured) {
    $exa.verdict = 'not_configured'
    $exa.failure_code = $exa.failure_code ?? 'EXA_MCP_NOT_REGISTERED'
    $exa.failure_detail = $exa.failure_detail ?? "no 'exa' entry under mcpServers in $claudeConfigPath"
    Write-Host "Exa MCP: NOT CONFIGURED ($($exa.failure_detail))"
} else {
    Write-Host "Exa MCP configured: $($exa.command)"
    Write-Host "  EXA_API_KEY present in server env: $($exa.api_key_configured)"

    # The npx shim resolves npm's own modules relative to the process working
    # directory, so launching it from a checkout that contains a node_modules
    # tree makes it look for npm inside THAT tree and die with MODULE_NOT_FOUND.
    # Resolve the launcher absolutely and run it from a neutral directory, or
    # the probe measures the launcher instead of Exa.
    # Get-Command in pwsh prefers npx.ps1, which CreateProcess cannot start.
    # Select an actually-launchable form in the order Windows can execute.
    $npxCandidates = @(Get-Command npx -All -ErrorAction SilentlyContinue |
        Where-Object { $_.Source } |
        Sort-Object { switch ([System.IO.Path]::GetExtension($_.Source).ToLowerInvariant()) {
            '.exe' { 0 } '.cmd' { 1 } '.bat' { 2 } default { 9 } } })
    $npxPath = @($npxCandidates | Where-Object {
        [System.IO.Path]::GetExtension($_.Source).ToLowerInvariant() -in @('.exe', '.cmd', '.bat')
    } | Select-Object -First 1).Source

    $proc = $null
    if (-not $npxPath) {
        $exa.verdict = 'down'
        $exa.failure_code = 'EXA_MCP_LAUNCHER_ABSENT'
        $exa.failure_detail = 'npx is not on PATH, so the configured stdio launcher cannot run'
        Write-Host "Exa MCP: LAUNCHER ABSENT -> npx not on PATH"
    } else {
        $exa.launcher_path = $npxPath
        Write-Host "  launcher: $npxPath (run from $env:USERPROFILE)"
        $psi = [System.Diagnostics.ProcessStartInfo]::new()
        # A .cmd shim is not a PE image; it must be run through the command
        # processor rather than handed to CreateProcess directly.
        if ([System.IO.Path]::GetExtension($npxPath) -in @('.cmd', '.bat')) {
            $psi.FileName = (Join-Path $env:SystemRoot 'System32\cmd.exe')
            $psi.Arguments = "/c `"`"$npxPath`" -y exa-mcp-server`""
        } else {
            $psi.FileName = $npxPath
            $psi.Arguments = '-y exa-mcp-server'
        }
        $psi.WorkingDirectory = $env:USERPROFILE
        $psi.RedirectStandardInput = $true
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.UseShellExecute = $false
        $psi.CreateNoWindow = $true

        try {
            $proc = [System.Diagnostics.Process]::Start($psi)
            $exa.launched = $true
        } catch {
            $exa.verdict = 'down'
            $exa.failure_code = 'EXA_MCP_LAUNCH_FAILED'
            $exa.failure_detail = $_.Exception.Message
            Write-Host "Exa MCP: LAUNCH FAILED -> $($_.Exception.Message)"
        }
    }

    if ($proc) {
        # Never block on stderr. The launcher spawns node as a grandchild that
        # inherits the stderr pipe handle, so ReadToEnd()/ReadToEndAsync() only
        # completes once EVERY handle holder exits — which hangs this script
        # whenever a descendant outlives the kill. Accumulate incrementally
        # through the event stream instead and simply stop reading.
        $stderrLines = [System.Collections.Concurrent.ConcurrentQueue[string]]::new()
        $stderrSub = Register-ObjectEvent -InputObject $proc -EventName ErrorDataReceived -Action {
            if ($null -ne $EventArgs.Data) { $Event.MessageData.Enqueue([string]$EventArgs.Data) }
        } -MessageData $stderrLines
        $proc.BeginErrorReadLine()

        function Send-Rpc {
            param($Process, $Object)
            $Process.StandardInput.WriteLine(($Object | ConvertTo-Json -Depth 12 -Compress))
            $Process.StandardInput.Flush()
        }

        function Receive-Rpc {
            param($Process, [int]$Seconds)
            $deadline = (Get-Date).AddSeconds($Seconds)
            while ((Get-Date) -lt $deadline) {
                if ($Process.HasExited -and $Process.StandardOutput.EndOfStream) {
                    return @{ _process_exited = $Process.ExitCode }
                }
                $readTask = $Process.StandardOutput.ReadLineAsync()
                $remaining = [int](($deadline - (Get-Date)).TotalMilliseconds)
                if ($remaining -le 0) { break }
                if (-not $readTask.Wait($remaining)) { break }
                $line = $readTask.Result
                if ($null -eq $line) { return @{ _stream_closed = $true } }
                $line = $line.Trim()
                if (-not $line.StartsWith('{')) { continue }
                try { return ($line | ConvertFrom-Json -AsHashtable) } catch { continue }
            }
            return @{ _timeout = $true }
        }

        try {
            Send-Rpc -Process $proc -Object @{
                jsonrpc = '2.0'; id = 1; method = 'initialize'
                params  = @{
                    protocolVersion = '2024-11-05'; capabilities = @{}
                    clientInfo = @{ name = 'synapse-research-lane-check'; version = '1' }
                }
            }
            $init = Receive-Rpc -Process $proc -Seconds $TimeoutSeconds
            if ($init['result']) {
                $exa.initialized = $true
                $exa.server_name = $init['result']['serverInfo']['name']
                $exa.server_version = $init['result']['serverInfo']['version']
                Write-Host "  initialize OK: $($exa.server_name) v$($exa.server_version)"

                Send-Rpc -Process $proc -Object @{ jsonrpc = '2.0'; method = 'notifications/initialized'; params = @{} }
                Send-Rpc -Process $proc -Object @{ jsonrpc = '2.0'; id = 2; method = 'tools/list'; params = @{} }
                $list = Receive-Rpc -Process $proc -Seconds $TimeoutSeconds
                $exa.tools_listed = @(@($list['result']['tools']) | ForEach-Object { $_['name'] })
                Write-Host "  tools/list -> $(@($exa.tools_listed).Count) tool(s): $(@($exa.tools_listed) -join ', ')"

                # Registration is not service. Only a real call proves the lane
                # can answer a research question.
                if (@($exa.tools_listed) -contains 'web_search_exa') {
                    $exa.call_attempted = $true
                    Send-Rpc -Process $proc -Object @{
                        jsonrpc = '2.0'; id = 3; method = 'tools/call'
                        params = @{ name = 'web_search_exa'; arguments = @{ query = $Probe; numResults = 1 } }
                    }
                    $call = Receive-Rpc -Process $proc -Seconds $TimeoutSeconds
                    $isError = [bool]$call['result']['isError']
                    $text = ''
                    if ($call['result']['content']) { $text = (@($call['result']['content']) | ForEach-Object { $_['text'] }) -join ' ' }
                    if ($call['error']) { $isError = $true; $text = ($call['error'] | ConvertTo-Json -Compress) }
                    if ($isError -or $call['_timeout'] -or $call['_process_exited']) {
                        $exa.call_succeeded = $false
                        $exa.failure_detail = if ($text) { $text.Trim() } else { ($call | ConvertTo-Json -Compress) }
                        $exa.failure_code = if ($exa.failure_detail -match '\b402\b|credits limit|top up') {
                            'EXA_API_CREDITS_EXHAUSTED_402'
                        } elseif ($exa.failure_detail -match '\b401\b|unauthor|api key') {
                            'EXA_API_UNAUTHORIZED'
                        } else {
                            'EXA_SEARCH_CALL_FAILED'
                        }
                        Write-Host "  tools/call web_search_exa -> FAILED [$($exa.failure_code)]"
                        Write-Host "    $($exa.failure_detail)"
                    } else {
                        $exa.call_succeeded = $true
                        Write-Host "  tools/call web_search_exa -> OK ($($text.Length) chars returned)"
                    }
                } else {
                    $exa.failure_code = 'EXA_SEARCH_TOOL_ABSENT'
                    $exa.failure_detail = 'server initialized but did not advertise web_search_exa'
                }
            } else {
                # Distinguish "the launcher never produced a server" from "Exa
                # refused the request". Reporting a launcher fault as an Exa
                # outage sends the operator to the wrong subsystem.
                $launcherDied = $proc.HasExited -or $init['_process_exited'] -or $init['_stream_closed']
                $exa.failure_code = if ($launcherDied) { 'EXA_MCP_LAUNCHER_FAILED' } else { 'EXA_MCP_INITIALIZE_FAILED' }
                $exa.failure_detail = ($init | ConvertTo-Json -Compress -Depth 6)
                Write-Host "  initialize FAILED [$($exa.failure_code)] -> $($exa.failure_detail)"
            }
        } finally {
            try { if (-not $proc.HasExited) { $proc.Kill($true) } } catch {}
            try { if ($stderrSub) { Unregister-Event -SourceIdentifier $stderrSub.Name -ErrorAction SilentlyContinue; Remove-Job -Id $stderrSub.Id -Force -ErrorAction SilentlyContinue } } catch {}
            $stderrText = (@($stderrLines.ToArray()) -join "`n")
            $exa.launcher_exit_code = if ($proc.HasExited) { $proc.ExitCode } else { $null }
            if ($stderrText) {
                $exa.launcher_stderr_tail = (($stderrText -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -Last 6) -join ' | ')
                if (-not $exa.call_attempted -and -not $exa.initialized) {
                    $exa.failure_detail = "$($exa.failure_detail); stderr=$($exa.launcher_stderr_tail)"
                    Write-Host "    launcher stderr: $($exa.launcher_stderr_tail)"
                }
            }
        }
    }

    $exa.verdict = if ($exa.call_succeeded) { 'live' } elseif ($exa.initialized) { 'registered_but_unusable' } else { 'down' }
}

$report.lanes += [pscustomobject]$exa

# ---------------------------------------------------------------------------
# Lane: built-in agent web search / fetch
# ---------------------------------------------------------------------------
Write-Section 'Built-in web lane'
Write-Host 'The built-in WebSearch/WebFetch tools are provided by the agent harness,'
Write-Host 'not by this host, so this script cannot probe them. Verify by issuing one'
Write-Host 'real search and confirming it returns sources.'
$report.lanes += [pscustomobject]@{
    lane    = 'builtin_web'
    verdict = 'harness_provided_verify_in_session'
    note    = 'issue one real WebSearch/WebFetch call; it is the documented primary research lane while exa_mcp is not live'
}

# ---------------------------------------------------------------------------
Write-Section 'VERDICT'
foreach ($lane in $report.lanes) {
    Write-Host ("  {0,-14} {1}" -f $lane.lane, $lane.verdict)
}
$exaLane = $report.lanes | Where-Object { $_.lane -eq 'exa_mcp' }
if ($exaLane.verdict -ne 'live') {
    Write-Host ''
    Write-Host "Exa is NOT a usable research lane on this host."
    Write-Host "  failure_code   : $($exaLane.failure_code)"
    Write-Host "  failure_detail : $($exaLane.failure_detail)"
    switch ($exaLane.failure_code) {
        'EXA_API_CREDITS_EXHAUSTED_402' {
            Write-Host "  remediation    : this is an ACCOUNT BILLING state, not a code or registration defect."
            Write-Host "                   Top up at https://dashboard.exa.ai and set a real EXA_API_KEY in the"
            Write-Host "                   'exa' mcpServers env block of $claudeConfigPath, then re-run this script."
        }
        'EXA_API_UNAUTHORIZED' {
            Write-Host "  remediation    : set a valid EXA_API_KEY in the 'exa' mcpServers env block of $claudeConfigPath."
        }
        { $_ -in @('EXA_MCP_LAUNCHER_FAILED','EXA_MCP_LAUNCHER_ABSENT','EXA_MCP_LAUNCH_FAILED') } {
            Write-Host "  remediation    : this is a LAUNCHER fault, not an Exa service verdict. Repair the npx/node"
            Write-Host "                   install named above (launcher_exit_code/launcher_stderr_tail in the JSON"
            Write-Host "                   readback carry the exact cause), then re-run. Exa's real state is UNKNOWN"
            Write-Host "                   until the launcher starts a server."
        }
        default {
            Write-Host "  remediation    : inspect the failure_detail above; do not treat Exa as available until this"
            Write-Host "                   script reports verdict=live."
        }
    }
    Write-Host ''
    Write-Host "Use the built-in web search/fetch lane instead. Per AGENTS.md that is an"
    Write-Host "accepted primary research lane, so an Exa outage is NOT a reason to stop work,"
    Write-Host "and NOT a reason to re-diagnose the outage. Record which lane you used."
}

$reportPath = Join-Path $env:TEMP 'synapse-research-lane-readback.json'
$report | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $reportPath -Encoding UTF8
Write-Host "`nStructured readback -> $reportPath"
