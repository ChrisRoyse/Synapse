param(
    [string]$ProjectDir = (Get-Location).Path,
    [string]$SourceDir = (Split-Path -Parent $PSScriptRoot),
    [string]$Bind = '127.0.0.1:7700',
    [string]$ConfigPath = (Join-Path $env:USERPROFILE '.codex\config.toml'),
    [string]$TokenPath = (Join-Path $env:APPDATA 'synapse\token.txt'),
    [string]$ToolSurfaceSnapshotPath = (Join-Path $env:APPDATA 'synapse\codex-tool-surface.json'),
    [string]$RunRoot = (Join-Path $env:LOCALAPPDATA 'synapse\codex-no-facade-doctor'),
    [string]$HandoffRoot = (Join-Path $env:LOCALAPPDATA 'synapse\codex-restart-handoffs'),
    [string]$ActiveIssue = $env:SYNAPSE_ACTIVE_ISSUE,
    [int]$FreshProbeTimeoutSec = 240,
    [switch]$ObservedSynapseFacadeAbsent
)

$ErrorActionPreference = 'Stop'

function Write-SynapseUtf8NoBomFile {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$Text
    )

    $dir = Split-Path -Parent $Path
    if (-not [string]::IsNullOrWhiteSpace($dir)) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }
    $encoding = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($Path, $Text, $encoding)
}

function Get-SynapseNowStamp {
    return [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
}

function New-SynapseDoctorRunDir {
    param([Parameter(Mandatory=$true)][string]$Root)

    $path = Join-Path $Root ("run-{0}-{1}" -f $PID, (Get-SynapseNowStamp))
    New-Item -ItemType Directory -Force -Path $path | Out-Null
    return $path
}

function Get-SynapseRecoveryNotesPath {
    param([AllowNull()][string]$RepoRoot)

    if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
        return $null
    }
    return (Join-Path $RepoRoot 'STATE\RECOVERY_NOTES.md')
}

function Get-SynapseNormalizedIssueRef {
    param([AllowNull()][string]$Issue)

    if ([string]::IsNullOrWhiteSpace($Issue)) {
        return $null
    }
    $trimmed = $Issue.Trim()
    if ($trimmed -match '^#?(?<number>[0-9]+)$') {
        return "#$($Matches['number'])"
    }
    if ($trimmed -match '^https://github\.com/ChrisRoyse/Synapse/issues/(?<number>[0-9]+)(?:[/?#].*)?$') {
        return "#$($Matches['number'])"
    }
    throw "SYNAPSE_ACTIVE_ISSUE_INVALID value=$trimmed remediation=pass an issue number like 1715, #1715, or https://github.com/ChrisRoyse/Synapse/issues/1715"
}

function Get-SynapseIssueNumberFromRef {
    param([AllowNull()][string]$IssueRef)

    if ([string]::IsNullOrWhiteSpace($IssueRef)) {
        return $null
    }
    if ($IssueRef -match '^#(?<number>[0-9]+)$') {
        return $Matches['number']
    }
    return $null
}

function Get-SynapseProcessLineage {
    param([int]$StartPid = $PID)

    $lineage = @()
    $seen = @{}
    $current = $StartPid
    $child = $null
    while ($current -and -not $seen.ContainsKey($current)) {
        $seen[$current] = $true
        $process = Get-CimInstance Win32_Process -Filter "ProcessId=$current" -ErrorAction SilentlyContinue
        if (-not $process) {
            break
        }
        if ($child -and $process.CreationDate -and $child.CreationDate -and $process.CreationDate -gt $child.CreationDate) {
            break
        }
        $lineage += $process
        $child = $process
        $current = [int]$process.ParentProcessId
    }
    return $lineage
}

function ConvertTo-SynapseProcessRecord {
    param([AllowNull()]$Process)

    if ($null -eq $Process) {
        return $null
    }
    return [ordered]@{
        pid = [int]$Process.ProcessId
        parent_pid = [int]$Process.ParentProcessId
        name = [string]$Process.Name
        executable_path = [string]$Process.ExecutablePath
        command_line = [string]$Process.CommandLine
        creation_date = [string]$Process.CreationDate
    }
}

function Get-SynapseCurrentCodexAncestor {
    $lineage = @(Get-SynapseProcessLineage)
    return ($lineage | Where-Object {
        $_.Name -ieq 'codex.exe' -or $_.CommandLine -match '@openai[\\/]+codex|codex\.js|codex-win32|openai\.chatgpt'
    } | Select-Object -First 1)
}

function Get-SynapseGitReadback {
    param([AllowNull()][string]$RepoRoot)

    if ([string]::IsNullOrWhiteSpace($RepoRoot) -or -not (Test-Path -LiteralPath $RepoRoot)) {
        return [ordered]@{
            available = $false
            reason = 'source_dir_missing_or_not_supplied'
            source_dir = $RepoRoot
        }
    }
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        return [ordered]@{
            available = $false
            reason = 'git_not_found'
            source_dir = $RepoRoot
        }
    }

    try {
        $status = @(& git -C $RepoRoot status --short --branch 2>&1)
        $head = @(& git -C $RepoRoot rev-parse HEAD 2>&1)
        $origin = @(& git -C $RepoRoot rev-parse origin/main 2>&1)
        $branch = @(& git -C $RepoRoot branch --show-current 2>&1)
        return [ordered]@{
            available = $true
            source_dir = $RepoRoot
            branch = (($branch | Select-Object -First 1) -join '').Trim()
            head = (($head | Select-Object -First 1) -join '').Trim()
            origin_main = (($origin | Select-Object -First 1) -join '').Trim()
            status_short_branch = @($status)
        }
    } catch {
        return [ordered]@{
            available = $false
            reason = 'git_readback_failed'
            source_dir = $RepoRoot
            error = $_.Exception.Message
        }
    }
}

function Get-SynapseProjectReadback {
    param([Parameter(Mandatory=$true)][string]$Path)

    $exists = Test-Path -LiteralPath $Path -PathType Container
    return [ordered]@{
        path = $Path
        exists = [bool]$exists
        resolved_path = if ($exists) { [string](Resolve-Path -LiteralPath $Path) } else { $null }
    }
}

function Get-SynapseCodexConfigReadback {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$ExpectedBind,
        [Parameter(Mandatory=$true)][string]$ProjectPath
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        return [ordered]@{
            path = $Path
            exists = $false
            readable = $false
            has_synapse_section = $false
            valid = $false
            reason = 'missing'
        }
    }

    try {
        $content = Get-Content -Raw -LiteralPath $Path
    } catch {
        return [ordered]@{
            path = $Path
            exists = $true
            readable = $false
            has_synapse_section = $false
            valid = $false
            reason = 'unreadable'
            error = $_.Exception.Message
        }
    }

    $section = [regex]::Match($content, '(?ms)^\[mcp_servers\.synapse\]\s*(?<body>.*?)(?=^\[|\z)')
    $body = if ($section.Success) { [string]$section.Groups['body'].Value } else { '' }
    $expectedUrlRegex = [regex]::Escape("http://$ExpectedBind/mcp")
    $projectKey = ($ProjectPath.TrimEnd('\') -replace '/', '\').ToLowerInvariant()
    $projectRegex = '(?ms)^\[projects\.(?:''|")' + [regex]::Escape($projectKey) + '(?:''|")\]\s*(?<body>.*?)(?=^\[|\z)'
    $projectSection = [regex]::Match($content.ToLowerInvariant(), $projectRegex)
    $projectBody = if ($projectSection.Success) { [string]$projectSection.Groups['body'].Value } else { '' }

    $urlMatches = ($body -match "url\s*=\s*`"$expectedUrlRegex`"")
    $bearerMatches = ($body -match 'bearer_token_env_var\s*=\s*"SYNAPSE_BEARER_TOKEN"')
    $requiredMatches = ($body -match '(?m)^\s*required\s*=\s*true\s*$')
    $approvalMatches = ($body -match '(?m)^\s*default_tools_approval_mode\s*=\s*"approve"\s*$')
    $trustedProject = ($projectBody -match '(?m)^\s*trust_level\s*=\s*"trusted"\s*$')

    return [ordered]@{
        path = $Path
        exists = $true
        readable = $true
        has_synapse_section = [bool]$section.Success
        url_matches = [bool]$urlMatches
        bearer_token_env_var_matches = [bool]$bearerMatches
        required_true = [bool]$requiredMatches
        default_tools_approval_approve = [bool]$approvalMatches
        project_trust_detected = [bool]$trustedProject
        project_key = $projectKey
        valid = [bool]($section.Success -and $urlMatches -and $bearerMatches -and $requiredMatches -and $approvalMatches)
    }
}

function Get-SynapseTokenReadback {
    param([Parameter(Mandatory=$true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) {
        return [ordered]@{
            path = $Path
            exists = $false
            readable = $false
            nonempty = $false
            token = $null
            token_length = 0
            current_process_env_present = -not [string]::IsNullOrWhiteSpace($env:SYNAPSE_BEARER_TOKEN)
            current_process_env_matches_file = $false
        }
    }
    try {
        $raw = Get-Content -Raw -LiteralPath $Path
        $token = if ($null -eq $raw) { '' } else { $raw.Trim() }
        return [ordered]@{
            path = $Path
            exists = $true
            readable = $true
            nonempty = -not [string]::IsNullOrWhiteSpace($token)
            token = $token
            token_length = $token.Length
            current_process_env_present = -not [string]::IsNullOrWhiteSpace($env:SYNAPSE_BEARER_TOKEN)
            current_process_env_length = if ($env:SYNAPSE_BEARER_TOKEN) { $env:SYNAPSE_BEARER_TOKEN.Length } else { 0 }
            current_process_env_matches_file = ($env:SYNAPSE_BEARER_TOKEN -eq $token)
        }
    } catch {
        return [ordered]@{
            path = $Path
            exists = $true
            readable = $false
            nonempty = $false
            token = $null
            token_length = 0
            current_process_env_present = -not [string]::IsNullOrWhiteSpace($env:SYNAPSE_BEARER_TOKEN)
            current_process_env_matches_file = $false
            error = $_.Exception.Message
        }
    }
}

function Get-SynapseToolSurfaceSnapshotReadback {
    param([Parameter(Mandatory=$true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) {
        return [ordered]@{
            path = $Path
            exists = $false
            readable = $false
            valid = $false
            reason = 'missing'
        }
    }
    try {
        $json = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json
        $hash = [string]$json.tool_surface_sha256
        $count = 0
        if ($json.PSObject.Properties['tool_count']) {
            $count = [int]$json.tool_count
        }
        return [ordered]@{
            path = $Path
            exists = $true
            readable = $true
            valid = (-not [string]::IsNullOrWhiteSpace($hash) -and $count -gt 0)
            daemon_pid = if ($json.PSObject.Properties['daemon_pid']) { $json.daemon_pid } else { $null }
            tool_count = $count
            tool_surface_sha256 = $hash
            tool_names = @($json.tool_names | ForEach-Object { [string]$_ })
        }
    } catch {
        return [ordered]@{
            path = $Path
            exists = $true
            readable = $false
            valid = $false
            reason = 'unreadable_or_invalid_json'
            error = $_.Exception.Message
        }
    }
}

function Split-SynapseBind {
    param([Parameter(Mandatory=$true)][string]$Value)

    $parts = $Value.Split(':')
    if ($parts.Count -ne 2 -or [string]::IsNullOrWhiteSpace($parts[0])) {
        throw "SYNAPSE_BIND_INVALID value=$Value remediation=pass bind as host:port, for example 127.0.0.1:7700"
    }
    return [ordered]@{
        host = $parts[0]
        port = [int]$parts[1]
    }
}

function Get-SynapseTcpReadback {
    param([Parameter(Mandatory=$true)][string]$BindValue)

    $bindParts = Split-SynapseBind -Value $BindValue
    $rows = @()
    try {
        $rows = @(Get-NetTCPConnection -LocalPort $bindParts.port -ErrorAction Stop | Where-Object {
            $_.LocalAddress -eq $bindParts.host -or $_.LocalAddress -eq '0.0.0.0' -or $_.LocalAddress -eq '::'
        })
    } catch {
        return [ordered]@{
            bind = $BindValue
            readable = $false
            listeners = @()
            connections = @()
            listener_count = 0
            error = $_.Exception.Message
        }
    }

    $records = @($rows | ForEach-Object {
        $owner = Get-CimInstance Win32_Process -Filter "ProcessId=$([int]$_.OwningProcess)" -ErrorAction SilentlyContinue
        [ordered]@{
            state = [string]$_.State
            local_address = [string]$_.LocalAddress
            local_port = [int]$_.LocalPort
            remote_address = [string]$_.RemoteAddress
            remote_port = [int]$_.RemotePort
            owning_process = [int]$_.OwningProcess
            owner_name = if ($owner) { [string]$owner.Name } else { $null }
            owner_path = if ($owner) { [string]$owner.ExecutablePath } else { $null }
            owner_command_line = if ($owner) { [string]$owner.CommandLine } else { $null }
        }
    })
    $listeners = @($records | Where-Object { $_.state -eq 'Listen' })
    return [ordered]@{
        bind = $BindValue
        readable = $true
        listeners = $listeners
        connections = $records
        listener_count = $listeners.Count
    }
}

function Get-SynapseDaemonProcessReadback {
    $processes = @(Get-CimInstance Win32_Process -Filter "Name='synapse-mcp.exe'" -ErrorAction SilentlyContinue)
    return @($processes | ForEach-Object { ConvertTo-SynapseProcessRecord -Process $_ })
}

function Invoke-SynapseHealthDiagnostic {
    param(
        [Parameter(Mandatory=$true)][string]$BindValue,
        [Parameter(Mandatory=$true)][string]$Token
    )

    try {
        $response = Invoke-RestMethod `
            -Method Get `
            -Uri "http://$BindValue/health" `
            -Headers @{ Authorization = "Bearer $Token" } `
            -UserAgent 'synapse-codex-doctor/diagnostic-only' `
            -TimeoutSec 10
        return [ordered]@{
            ok = [bool]$response.ok
            pid = if ($response.PSObject.Properties['pid']) { $response.pid } else { $null }
            tool_count = if ($response.PSObject.Properties['tool_count']) { $response.tool_count } else { $null }
            tool_surface_sha256 = if ($response.PSObject.Properties['tool_surface_sha256']) { [string]$response.tool_surface_sha256 } else { $null }
            chrome_bridge_status = if ($response.subsystems -and $response.subsystems.chrome_bridge) { [string]$response.subsystems.chrome_bridge.status } else { $null }
            diagnostic_only_not_fsv_trigger = $true
        }
    } catch {
        return [ordered]@{
            ok = $false
            error = $_.Exception.Message
            diagnostic_only_not_fsv_trigger = $true
        }
    }
}

function Invoke-SynapseFreshCodexProbe {
    param(
        [Parameter(Mandatory=$true)][string]$ProjectPath,
        [Parameter(Mandatory=$true)][string]$OutputDir,
        [Parameter(Mandatory=$true)][int]$TimeoutSec
    )

    $codex = Get-Command codex -ErrorAction SilentlyContinue
    if (-not $codex) {
        return [ordered]@{
            ok = $false
            reason_code = 'SYNAPSE_CODEX_COMMAND_MISSING'
            error = 'codex command was not found on PATH'
        }
    }

    $codexSource = [string]$codex.Source
    $launcherFile = $codexSource
    $launcherPrefixArgs = @()
    $extension = [System.IO.Path]::GetExtension($codexSource)
    if ($extension -ieq '.ps1') {
        $pwsh = Get-Command pwsh -ErrorAction SilentlyContinue
        if (-not $pwsh) {
            return [ordered]@{
                ok = $false
                reason_code = 'SYNAPSE_CODEX_PWSH_MISSING'
                command = $codexSource
                error = 'codex resolved to a PowerShell launcher but pwsh was not found on PATH'
            }
        }
        $launcherFile = [string]$pwsh.Source
        $launcherPrefixArgs = @('-NoProfile', '-File', $codexSource)
    } elseif ($extension -ieq '.cmd' -or $extension -ieq '.bat') {
        $launcherFile = Join-Path $env:SystemRoot 'System32\cmd.exe'
        $launcherPrefixArgs = @('/d', '/c', $codexSource)
    }

    $stdoutPath = Join-Path $OutputDir 'fresh-codex-probe.stdout.jsonl'
    $stderrPath = Join-Path $OutputDir 'fresh-codex-probe.stderr.txt'
    $lastMessagePath = Join-Path $OutputDir 'fresh-codex-probe.last-message.json'
    $prompt = 'Use the real Synapse MCP server from this production Codex session. Call mcp__synapse.health with {"detail":"compact"}. Return only compact JSON with fields synapse_found, health_called, health_ok, pid, tool_count, tool_surface_sha256, error.'

    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $launcherFile
    $psi.WorkingDirectory = $ProjectPath
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    foreach ($arg in @($launcherPrefixArgs + @('exec', '--json', '-C', $ProjectPath, '--output-last-message', $lastMessagePath, $prompt))) {
        [void]$psi.ArgumentList.Add($arg)
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $psi
    $startedAt = [DateTime]::UtcNow
    try {
        [void]$process.Start()
    } catch {
        return [ordered]@{
            ok = $false
            reason_code = 'SYNAPSE_CODEX_FRESH_SESSION_MCP_PROBE_LAUNCH_FAILED'
            command = $codexSource
            launcher_file = $launcherFile
            launcher_prefix_args = @($launcherPrefixArgs)
            stdout_path = $stdoutPath
            stderr_path = $stderrPath
            last_message_path = $lastMessagePath
            error = $_.Exception.Message
        }
    }
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    if (-not $process.WaitForExit($TimeoutSec * 1000)) {
        $pidToKill = $process.Id
        try {
            $process.Kill($true)
        } catch {
        }
        return [ordered]@{
            ok = $false
            reason_code = 'SYNAPSE_CODEX_FRESH_SESSION_MCP_PROBE_TIMEOUT'
            command = $codexSource
            launcher_file = $launcherFile
            launcher_prefix_args = @($launcherPrefixArgs)
            child_pid = $pidToKill
            timeout_sec = $TimeoutSec
            stdout_path = $stdoutPath
            stderr_path = $stderrPath
            last_message_path = $lastMessagePath
            started_at_utc = $startedAt.ToString('o')
            ended_at_utc = [DateTime]::UtcNow.ToString('o')
            error = 'fresh Codex probe timed out; exact child process was killed'
        }
    }
    $stdout = $stdoutTask.GetAwaiter().GetResult()
    $stderr = $stderrTask.GetAwaiter().GetResult()
    Write-SynapseUtf8NoBomFile -Path $stdoutPath -Text $stdout
    Write-SynapseUtf8NoBomFile -Path $stderrPath -Text $stderr

    $mcpHealthCallObserved = $false
    $mcpHealthCallError = $null
    $nonJsonLines = @()
    foreach ($line in ($stdout -split "`r?`n")) {
        if ([string]::IsNullOrWhiteSpace($line)) {
            continue
        }
        if (-not $line.TrimStart().StartsWith('{')) {
            $nonJsonLines += $line
            continue
        }
        try {
            $event = $line | ConvertFrom-Json
        } catch {
            continue
        }
        if ($event.type -eq 'item.completed' -and $event.item.type -eq 'mcp_tool_call' -and $event.item.server -eq 'synapse' -and $event.item.tool -eq 'health') {
            if ($null -eq $event.item.error -and $event.item.status -eq 'completed') {
                $mcpHealthCallObserved = $true
            } else {
                $mcpHealthCallError = $event.item.error
            }
        }
    }

    $last = $null
    $lastParseError = $null
    if (Test-Path -LiteralPath $lastMessagePath) {
        try {
            $last = Get-Content -Raw -LiteralPath $lastMessagePath | ConvertFrom-Json
        } catch {
            $lastParseError = $_.Exception.Message
        }
    }

    $exitCode = [int]$process.ExitCode
    $finalHealthy = (
        $exitCode -eq 0 -and
        $mcpHealthCallObserved -and
        $null -ne $last -and
        $last.synapse_found -eq $true -and
        $last.health_called -eq $true -and
        $last.health_ok -eq $true -and
        [int]$last.tool_count -gt 0 -and
        -not [string]::IsNullOrWhiteSpace([string]$last.tool_surface_sha256)
    )

    return [ordered]@{
        ok = [bool]$finalHealthy
        reason_code = if ($finalHealthy) { 'OK' } else { 'SYNAPSE_CODEX_FRESH_SESSION_MCP_PROBE_FAILED' }
        command = $codexSource
        launcher_file = $launcherFile
        launcher_prefix_args = @($launcherPrefixArgs)
        exit_code = $exitCode
        child_pid = $process.Id
        started_at_utc = $startedAt.ToString('o')
        ended_at_utc = [DateTime]::UtcNow.ToString('o')
        stdout_path = $stdoutPath
        stderr_path = $stderrPath
        last_message_path = $lastMessagePath
        last_message_parse_error = $lastParseError
        mcp_health_call_observed_in_jsonl = [bool]$mcpHealthCallObserved
        mcp_health_call_error = $mcpHealthCallError
        non_json_stdout_lines = @($nonJsonLines)
        result = if ($last) {
            [ordered]@{
                synapse_found = [bool]$last.synapse_found
                health_called = [bool]$last.health_called
                health_ok = [bool]$last.health_ok
                pid = $last.pid
                tool_count = $last.tool_count
                tool_surface_sha256 = [string]$last.tool_surface_sha256
                error = if ($last.PSObject.Properties['error']) { $last.error } else { $null }
            }
        } else {
            $null
        }
        client_parity_basis = 'codex exec --json startup with required mcp_servers.synapse plus observed real mcp_tool_call event server=synapse tool=health'
    }
}

function Write-SynapseNoFacadeHandoff {
    param(
        [Parameter(Mandatory=$true)]$Report,
        [Parameter(Mandatory=$true)][string]$Root,
        [AllowNull()][string]$RepoRoot,
        [AllowNull()][string]$Issue
    )

    $activeIssueRef = Get-SynapseNormalizedIssueRef -Issue $Issue
    $activeIssueNumber = Get-SynapseIssueNumberFromRef -IssueRef $activeIssueRef
    $activeIssueRead = if ([string]::IsNullOrWhiteSpace($activeIssueNumber)) {
        $null
    } else {
        "gh issue view $activeIssueNumber --repo ChrisRoyse/Synapse --comments"
    }
    $codexPid = if ($Report.current_codex_process -and $Report.current_codex_process.pid) { [int]$Report.current_codex_process.pid } else { 0 }
    $stamp = Get-SynapseNowStamp
    $baseName = "codex-no-facade-handoff-$codexPid-$stamp"
    $jsonPath = Join-Path $Root "$baseName.json"
    $mdPath = Join-Path $Root "$baseName.md"
    $recoveryNotesPath = Get-SynapseRecoveryNotesPath -RepoRoot $RepoRoot
    $postRestartRequiredReads = @(
        'C:\Users\hotra\Downloads\AICodingAgentSuperPrompt.md',
        'C:\code\Synapse\docs\compressionprompt.md',
        'C:\code\Synapse\AGENTS.md',
        $recoveryNotesPath
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    $githubReads = @(
        'gh issue view 351 --repo ChrisRoyse/Synapse --comments',
        $activeIssueRead,
        'gh issue list --repo ChrisRoyse/Synapse --state open --limit 100'
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    $restartHint = if ([string]::IsNullOrWhiteSpace($activeIssueRef)) {
        "Close this Codex session completely, start a new Codex session through the patched launcher, verify the active Codex PID differs from $codexPid, then resume from GitHub issue state."
    } else {
        "Close this Codex session completely, start a new Codex session through the patched launcher, verify the active Codex PID differs from $codexPid, then resume $activeIssueRef."
    }

    $record = [ordered]@{
        schema_version = 1
        artifact_kind = 'synapse_codex_no_facade_restart_handoff'
        created_at_utc = [DateTime]::UtcNow.ToString('o')
        reason_code = 'SYNAPSE_CODEX_CURRENT_PROCESS_MCP_ABSENT'
        reason = 'The current Codex session was observed without any callable Synapse MCP namespace, while a fresh production Codex process proved the configured Synapse MCP server can load and call health.'
        phase = 'current_session_no_synapse_facade'
        required_restart = $true
        no_in_process_hot_add = $true
        explanation = 'Codex loads MCP servers and callable metadata at client startup. A running session that did not load Synapse cannot be repaired from inside that same missing namespace. The safe same-agent boundary is a restart through the patched launcher after reading this handoff.'
        current_codex_process = $Report.current_codex_process
        current_process_lineage = $Report.current_process_lineage
        project = $Report.project
        codex_config = $Report.codex_config
        token = $Report.token_without_secret
        tool_surface_snapshot = $Report.tool_surface_snapshot
        daemon_processes = $Report.daemon_processes
        tcp = $Report.tcp
        health_diagnostic = $Report.health_diagnostic
        fresh_codex_probe = $Report.fresh_codex_probe
        active_issue = [ordered]@{
            issue_ref = $activeIssueRef
            issue_number = $activeIssueNumber
            source = 'ActiveIssue parameter or SYNAPSE_ACTIVE_ISSUE environment variable'
            status = if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown' } else { 'provided' }
        }
        github_reads = $githubReads
        post_restart_required_reads = $postRestartRequiredReads
        post_restart_verification = @(
            "Read the active Codex process parent chain and confirm the active codex.exe PID is not stale PID $codexPid from this handoff.",
            'Run deferred tool discovery for Synapse from the fresh session, then call real mcp__synapse.health and verify ok=true.',
            'Call real mcp__synapse.browser_tabs or browser_debugger against the already-open Chrome profile before claiming existing-Chrome reachability.',
            'If Synapse is still absent, rerun scripts\synapse-codex-doctor.ps1 from that fresh session and keep the GitHub issue open.',
            'Do not treat direct HTTP diagnostics in this handoff as manual FSV acceptance.'
        )
        restart_command_hint = $restartHint
        repo_readback = Get-SynapseGitReadback -RepoRoot $RepoRoot
        recovery_notes_path = $recoveryNotesPath
        report_path = $Report.report_path
    }

    try {
        New-Item -ItemType Directory -Force -Path $Root | Out-Null
        Write-SynapseUtf8NoBomFile -Path $jsonPath -Text (($record | ConvertTo-Json -Depth 40) + "`n")
        $md = @(
            '# Synapse Codex No-Facade Restart Handoff',
            '',
            '- Reason: SYNAPSE_CODEX_CURRENT_PROCESS_MCP_ABSENT',
            "- Created UTC: $($record.created_at_utc)",
            "- Codex PID: $codexPid",
            "- Project: $($Report.project.path)",
            "- Fresh Codex proof: ok=$($Report.fresh_codex_probe.ok) pid=$($Report.fresh_codex_probe.result.pid) tool_count=$($Report.fresh_codex_probe.result.tool_count) tool_surface_sha256=$($Report.fresh_codex_probe.result.tool_surface_sha256)",
            "- Active issue: $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown; recover from caller/session context or open issue queue' } else { $activeIssueRef })",
            '',
            '## Required Restart',
            "The current Codex session has no callable Synapse MCP namespace. Close stale Codex PID $codexPid completely, start a new Codex session through the patched launcher, and prove the active codex.exe PID changed before continuing.",
            '',
            '## Read After Restart'
        )
        foreach ($item in $postRestartRequiredReads) {
            $md += "- $item"
        }
        $md += @(
            '',
            '## GitHub Reads'
        )
        foreach ($item in $githubReads) {
            $md += "- $item"
        }
        $md += @(
            '',
            '## Verification'
        )
        foreach ($item in $record.post_restart_verification) {
            $md += "- $item"
        }
        $md += @(
            '',
            "JSON artifact: $jsonPath",
            "Doctor report: $($Report.report_path)",
            ''
        )
        Write-SynapseUtf8NoBomFile -Path $mdPath -Text (($md -join "`n") + "`n")

        if (-not [string]::IsNullOrWhiteSpace($recoveryNotesPath)) {
            $notes = @(
                '# Synapse Recovery Notes',
                '',
                '## Latest Codex No-Facade Restart Handoff',
                '',
                '- Reason: SYNAPSE_CODEX_CURRENT_PROCESS_MCP_ABSENT',
                "- Created UTC: $($record.created_at_utc)",
                "- JSON: $jsonPath",
                "- Markdown: $mdPath",
                "- Doctor report: $($Report.report_path)",
                "- Stale Codex PID: $codexPid",
                "- Active issue: $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown; recover from caller/session context or open issue queue' } else { $activeIssueRef })",
                '',
                "After restart, re-read AGENTS.md, #351, $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'the active issue from the caller/session context' } else { $activeIssueRef }), git status, and this file before resuming. Run deferred Synapse tool discovery before calling real mcp__synapse tools for FSV; direct helper calls are diagnostics only.",
                ''
            )
            Write-SynapseUtf8NoBomFile -Path $recoveryNotesPath -Text (($notes -join "`n") + "`n")
        }
    } catch {
        throw "SYNAPSE_CODEX_NO_FACADE_HANDOFF_WRITE_FAILED path=$jsonPath error=$($_.Exception.Message) remediation=repair permissions on %LOCALAPPDATA%\synapse\codex-restart-handoffs and rerun this doctor"
    }

    return [ordered]@{
        json_path = $jsonPath
        markdown_path = $mdPath
        recovery_notes_path = $recoveryNotesPath
    }
}

$runDir = New-SynapseDoctorRunDir -Root $RunRoot
$reportPath = Join-Path $runDir 'doctor-report.json'
$activeIssueRef = Get-SynapseNormalizedIssueRef -Issue $ActiveIssue
$script:Report = [ordered]@{
    schema_version = 1
    artifact_kind = 'synapse_codex_no_facade_doctor_report'
    started_at_utc = [DateTime]::UtcNow.ToString('o')
    status = 'running'
    reason_code = $null
    report_path = $reportPath
    run_dir = $runDir
    observed_synapse_facade_absent = [bool]$ObservedSynapseFacadeAbsent
    bind = $Bind
    active_issue = $activeIssueRef
}

function Get-SynapseDoctorRedactedReport {
    $redacted = [ordered]@{}
    foreach ($entry in $script:Report.GetEnumerator()) {
        if ($entry.Key -eq 'token_secret') {
            continue
        }
        $redacted[$entry.Key] = $entry.Value
    }
    return $redacted
}

function Write-SynapseDoctorReport {
    $script:Report.ended_at_utc = [DateTime]::UtcNow.ToString('o')
    $redacted = Get-SynapseDoctorRedactedReport
    Write-SynapseUtf8NoBomFile -Path $script:Report.report_path -Text (($redacted | ConvertTo-Json -Depth 40) + "`n")
}

function Stop-SynapseDoctor {
    param(
        [Parameter(Mandatory=$true)][string]$ReasonCode,
        [Parameter(Mandatory=$true)][string]$Message
    )

    $script:Report.status = 'failed'
    $script:Report.reason_code = $ReasonCode
    $script:Report.message = $Message
    Write-SynapseDoctorReport
    [Console]::Error.WriteLine("$ReasonCode $Message report=$($script:Report.report_path)")
    exit 1
}

try {
    $script:Report.project = Get-SynapseProjectReadback -Path $ProjectDir
    if (-not $script:Report.project.exists) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_CODEX_PROJECT_PATH_MISSING' -Message "project path does not exist: $ProjectDir"
    }

    $script:Report.current_process_lineage = @(Get-SynapseProcessLineage | ForEach-Object { ConvertTo-SynapseProcessRecord -Process $_ })
    $script:Report.current_codex_process = ConvertTo-SynapseProcessRecord -Process (Get-SynapseCurrentCodexAncestor)

    $script:Report.codex_config = Get-SynapseCodexConfigReadback -Path $ConfigPath -ExpectedBind $Bind -ProjectPath $ProjectDir
    if (-not $script:Report.codex_config.exists) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_CODEX_CONFIG_MISSING' -Message "Codex config missing: $ConfigPath"
    }
    if (-not $script:Report.codex_config.readable) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_CODEX_CONFIG_UNREADABLE' -Message "Codex config unreadable: $ConfigPath"
    }
    if (-not $script:Report.codex_config.valid) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_CODEX_CONFIG_INVALID' -Message "Codex Synapse MCP config is incomplete or does not match http://$Bind/mcp with bearer token, required=true, and approve mode"
    }

    $tokenReadback = Get-SynapseTokenReadback -Path $TokenPath
    $script:Report.token_secret = $tokenReadback.token
    $script:Report.token_without_secret = [ordered]@{
        path = $tokenReadback.path
        exists = $tokenReadback.exists
        readable = $tokenReadback.readable
        nonempty = $tokenReadback.nonempty
        token_length = $tokenReadback.token_length
        current_process_env_present = $tokenReadback.current_process_env_present
        current_process_env_length = $tokenReadback.current_process_env_length
        current_process_env_matches_file = $tokenReadback.current_process_env_matches_file
        error = if ($tokenReadback.Contains('error')) { $tokenReadback.error } else { $null }
    }
    if (-not $tokenReadback.exists) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_CODEX_TOKEN_MISSING' -Message "token file missing: $TokenPath"
    }
    if (-not $tokenReadback.readable) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_CODEX_TOKEN_UNREADABLE' -Message "token file unreadable: $TokenPath"
    }
    if (-not $tokenReadback.nonempty) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_CODEX_TOKEN_EMPTY' -Message "token file is empty: $TokenPath"
    }

    $script:Report.tool_surface_snapshot = Get-SynapseToolSurfaceSnapshotReadback -Path $ToolSurfaceSnapshotPath
    if (-not $script:Report.tool_surface_snapshot.exists) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_MISSING' -Message "Codex tool-surface snapshot missing: $ToolSurfaceSnapshotPath"
    }
    if (-not $script:Report.tool_surface_snapshot.valid) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_INVALID' -Message "Codex tool-surface snapshot is unreadable or lacks tool_count/tool_surface_sha256: $ToolSurfaceSnapshotPath"
    }

    $script:Report.daemon_processes = @(Get-SynapseDaemonProcessReadback)
    $script:Report.tcp = Get-SynapseTcpReadback -BindValue $Bind
    if (-not $script:Report.tcp.readable) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_DAEMON_SOCKET_READ_FAILED' -Message "failed to read TCP socket Source of Truth for $Bind"
    }
    if ([int]$script:Report.tcp.listener_count -lt 1) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_DAEMON_BIND_ABSENT' -Message "no listener found at $Bind"
    }

    $script:Report.health_diagnostic = Invoke-SynapseHealthDiagnostic -BindValue $Bind -Token $script:Report.token_secret
    if (-not $script:Report.health_diagnostic.ok) {
        Stop-SynapseDoctor -ReasonCode 'SYNAPSE_DAEMON_HEALTH_FAILED' -Message "direct diagnostic health failed for $Bind"
    }

    $script:Report.fresh_codex_probe = Invoke-SynapseFreshCodexProbe -ProjectPath $script:Report.project.resolved_path -OutputDir $runDir -TimeoutSec $FreshProbeTimeoutSec
    if (-not $script:Report.fresh_codex_probe.ok) {
        Stop-SynapseDoctor -ReasonCode $script:Report.fresh_codex_probe.reason_code -Message "fresh production Codex session did not prove a real Synapse health MCP call"
    }

    if ($ObservedSynapseFacadeAbsent) {
        $script:Report.handoff = Write-SynapseNoFacadeHandoff -Report $script:Report -Root $HandoffRoot -RepoRoot $SourceDir -Issue $ActiveIssue
        $script:Report.status = 'handoff_written'
        $script:Report.reason_code = 'SYNAPSE_CODEX_CURRENT_PROCESS_MCP_ABSENT'
        $script:Report.message = 'current session was observed without Synapse MCP; fresh Codex probe succeeded; restart handoff written'
    } else {
        $script:Report.status = 'healthy_no_handoff'
        $script:Report.reason_code = 'OK'
        $script:Report.message = 'configured Synapse/Codex path is healthy; no no-facade observation was supplied, so no restart handoff was written'
    }
    Write-SynapseDoctorReport
    Write-Output ((Get-SynapseDoctorRedactedReport | ConvertTo-Json -Depth 12 -Compress))
    exit 0
} catch {
    $script:Report.status = 'failed'
    $script:Report.reason_code = 'SYNAPSE_CODEX_DOCTOR_UNHANDLED_EXCEPTION'
    $script:Report.message = $_.Exception.Message
    Write-SynapseDoctorReport
    [Console]::Error.WriteLine("SYNAPSE_CODEX_DOCTOR_UNHANDLED_EXCEPTION $($_.Exception.Message) report=$($script:Report.report_path)")
    exit 1
}
