param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$SpawnId,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$PromptPath,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$StdoutPath,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$StderrPath,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$FinalMessagePath,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$ControlPath,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$EventsPath,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$AppServerStdoutPath,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$AppServerStderrPath,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$WorkingDir,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$McpUrl,
    [string]$Model = "",
    [string]$NotifyScriptPath = "",
    [switch]$RequireApprovalGate
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)

function Write-TextNoBom([string]$Path, [string]$Value) {
    [System.IO.File]::WriteAllText($Path, $Value, $Utf8NoBom)
}

function Append-LineNoBom([string]$Path, [string]$Value) {
    [System.IO.File]::AppendAllText($Path, ($Value + [Environment]::NewLine), $Utf8NoBom)
}

function Get-UnixMs {
    return [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
}

function ConvertTo-TomlStringLiteral([string]$Value) {
    return ($Value | ConvertTo-Json -Compress)
}

function Get-CodexApprovalPolicy {
    if ($RequireApprovalGate.IsPresent) {
        return 'on-request'
    }
    return 'never'
}

function Get-CodexSandboxMode {
    if ($RequireApprovalGate.IsPresent) {
        return 'workspace-write'
    }
    return 'danger-full-access'
}

function New-CodexSandboxPolicy([string]$Mode, [string]$Root) {
    if ($Mode -eq 'workspace-write') {
        return [ordered]@{
            type = 'workspaceWrite'
            writableRoots = @($Root)
            networkAccess = $false
            excludeTmpdirEnvVar = $false
            excludeSlashTmp = $false
        }
    }
    return [ordered]@{ type = 'dangerFullAccess' }
}

function Get-CodexAppServerRequestBridgeUrl([string]$Url) {
    if (-not $Url.EndsWith('/mcp', [StringComparison]::OrdinalIgnoreCase)) {
        throw "cannot derive Codex app-server request bridge URL from MCP URL without /mcp suffix: $Url"
    }
    return ($Url.Substring(0, $Url.Length - 4) + '/codex-app-server/request')
}

function Add-JsonLine([string]$Path, [object]$Value) {
    $json = $Value
    if ($Value -isnot [string]) {
        $json = $Value | ConvertTo-Json -Compress -Depth 100
    }
    Append-LineNoBom -Path $Path -Value $json
}

function Get-JsonProperty($Object, [string]$Name) {
    if ($null -eq $Object) {
        return $null
    }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }
    return $property.Value
}

function Get-JsonShape($Value) {
    if ($null -eq $Value) {
        return 'null'
    }
    if ($Value -is [System.Array]) {
        return 'array'
    }
    if ($Value -is [System.Management.Automation.PSCustomObject]) {
        $names = @($Value.PSObject.Properties | ForEach-Object { $_.Name })
        return ('object:{0}' -f ($names -join ','))
    }
    return $Value.GetType().FullName
}

function New-CodexAppServerRpcFailure([int]$Id, [string]$ExpectedMethod, $ResponseError, [string]$Phase) {
    $errorCode = Get-JsonProperty $ResponseError 'code'
    $errorMessage = Get-JsonProperty $ResponseError 'message'
    $errorData = Get-JsonProperty $ResponseError 'data'
    return [ordered]@{
        code = 'SYNAPSE_CODEX_APP_SERVER_RPC_FAILED'
        protocol = 'codex_app_server_ws'
        phase = $Phase
        request_id = $Id
        expected_method = $ExpectedMethod
        response_error_shape = Get-JsonShape $ResponseError
        response_error_code = $errorCode
        response_error_message = $errorMessage
        response_error_data_shape = Get-JsonShape $errorData
        response_error_json = ($ResponseError | ConvertTo-Json -Compress -Depth 100)
        thread_id = $script:ThreadId
        turn_id = $script:TurnId
        turn_status = $script:TurnStatus
        control_path = $ControlPath
        events_path = $EventsPath
        stdout_path = $StdoutPath
        stderr_path = $StderrPath
        remediation = 'Inspect codex app-server events/control artifacts and fix the request shape or Codex app-server state that produced the JSON-RPC error.'
        at_unix_ms = Get-UnixMs
    }
}

function Throw-CodexAppServerRpcFailure([int]$Id, [string]$ExpectedMethod, $ResponseError, [string]$Phase) {
    $failure = New-CodexAppServerRpcFailure -Id $Id -ExpectedMethod $ExpectedMethod -ResponseError $ResponseError -Phase $Phase
    Add-JsonLine -Path $EventsPath -Value ([ordered]@{
        direction = 'server'
        phase = 'response_error'
        id = $Id
        expected_method = $ExpectedMethod
        failure = $failure
        at_unix_ms = Get-UnixMs
    })
    throw ($failure | ConvertTo-Json -Compress -Depth 100)
}

function Write-Control([hashtable]$Patch) {
    $current = [ordered]@{}
    if (Test-Path -LiteralPath $ControlPath) {
        try {
            $existing = Get-Content -Raw -LiteralPath $ControlPath -Encoding UTF8 | ConvertFrom-Json
            foreach ($property in $existing.PSObject.Properties) {
                $current[$property.Name] = $property.Value
            }
        } catch {
            $current['previous_control_parse_error'] = $_.Exception.Message
        }
    }
    $current['schema_version'] = 1
    $current['protocol'] = 'codex_app_server_ws'
    $current['endpoint'] = $script:Endpoint
    $current['control_path'] = $ControlPath
    $current['events_path'] = $EventsPath
    $current['app_server_process_id'] = $script:AppServerPid
    $current['thread_id'] = $script:ThreadId
    $current['turn_id'] = $script:TurnId
    $current['turn_status'] = $script:TurnStatus
    $current['last_error'] = $script:LastErrorText
    $current['approval_policy'] = $script:CodexApprovalPolicy
    $current['sandbox_mode'] = $script:CodexSandboxMode
    $current['app_server_request_bridge_url'] = $script:CodexAppServerRequestBridgeUrl
    foreach ($key in $Patch.Keys) {
        $current[$key] = $Patch[$key]
    }
    $current['updated_at_unix_ms'] = Get-UnixMs
    [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($ControlPath)) | Out-Null
    $tmp = "$ControlPath.tmp.$PID"
    Write-TextNoBom -Path $tmp -Value ($current | ConvertTo-Json -Depth 100)
    Move-Item -LiteralPath $tmp -Destination $ControlPath -Force
}

function Get-FreeTcpPort {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Parse('127.0.0.1'), 0)
    $listener.Start()
    try {
        return [int]$listener.LocalEndpoint.Port
    } finally {
        $listener.Stop()
    }
}

function Wait-AppServerReady([string]$Url, [int]$TimeoutMs) {
    $deadline = [DateTimeOffset]::UtcNow.AddMilliseconds($TimeoutMs)
    do {
        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri $Url -TimeoutSec 1
            if ($response.StatusCode -ge 200 -and $response.StatusCode -lt 300) {
                return
            }
        } catch {
            Start-Sleep -Milliseconds 100
        }
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    throw "codex app-server did not become ready at $Url within ${TimeoutMs}ms"
}

function Get-ChildProcessIds([int]$ParentPid) {
    $children = @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $ParentPid" -ErrorAction SilentlyContinue)
    foreach ($child in $children) {
        Get-ChildProcessIds -ParentPid ([int]$child.ProcessId)
        [int]$child.ProcessId
    }
}

function Stop-OwnedProcessTree([int]$RootPid) {
    $ids = @(Get-ChildProcessIds -ParentPid $RootPid) + @($RootPid)
    foreach ($id in ($ids | Select-Object -Unique)) {
        try {
            $process = Get-Process -Id $id -ErrorAction Stop
            Stop-Process -Id $process.Id -Force -ErrorAction Stop
        } catch {}
    }
}

function Get-CodexLaunchSpec([object[]]$AppArgs) {
    $command = Get-Command codex.ps1 -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        $command = Get-Command codex.cmd -ErrorAction SilentlyContinue
    }
    if ($null -eq $command) {
        $command = Get-Command codex -ErrorAction Stop
    }
    $path = $command.Path
    if ([string]::IsNullOrWhiteSpace($path)) {
        $path = $command.Source
    }
    if ([string]::IsNullOrWhiteSpace($path)) {
        throw 'codex command resolved without an executable path'
    }
    if ($path.EndsWith('.ps1', [StringComparison]::OrdinalIgnoreCase)) {
        return [pscustomobject]@{
            File = 'powershell.exe'
            Args = @('-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $path) + $AppArgs
        }
    }
    return [pscustomobject]@{ File = $path; Args = $AppArgs }
}

function Connect-AppServer([string]$Endpoint) {
    $socket = [System.Net.WebSockets.ClientWebSocket]::new()
    [void]$socket.ConnectAsync([Uri]$Endpoint, [Threading.CancellationToken]::None).GetAwaiter().GetResult()
    return $socket
}

function Send-WebSocketJson($Socket, [object]$Message) {
    $json = $Message | ConvertTo-Json -Compress -Depth 20
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
    Add-JsonLine -Path $EventsPath -Value ([ordered]@{
        direction = 'client'
        phase = 'send_start'
        id = Get-JsonProperty $Message 'id'
        method = Get-JsonProperty $Message 'method'
        bytes = $bytes.Length
        at_unix_ms = Get-UnixMs
    })
    $segment = [ArraySegment[byte]]::new($bytes)
    [void]$Socket.SendAsync($segment, [System.Net.WebSockets.WebSocketMessageType]::Text, $true, [Threading.CancellationToken]::None).GetAwaiter().GetResult()
    Add-JsonLine -Path $EventsPath -Value ([ordered]@{
        direction = 'client'
        phase = 'send_ok'
        id = Get-JsonProperty $Message 'id'
        method = Get-JsonProperty $Message 'method'
        bytes = $bytes.Length
        at_unix_ms = Get-UnixMs
    })
}

function Invoke-CodexAppServerRequestBridge($Message) {
    $method = [string](Get-JsonProperty $Message 'method')
    $id = Get-JsonProperty $Message 'id'
    $params = Get-JsonProperty $Message 'params'
    if ([string]::IsNullOrWhiteSpace($method) -or $null -eq $id) {
        throw 'app-server request bridge requires method and id'
    }
    $token = $env:SYNAPSE_BEARER_TOKEN
    if ([string]::IsNullOrWhiteSpace($token)) {
        throw 'SYNAPSE_BEARER_TOKEN is not set in the Codex app-server runner environment'
    }
    $payload = [ordered]@{
        spawn_id = $SpawnId
        method = $method
        id = $id
        params = $params
    }
    $body = $payload | ConvertTo-Json -Compress -Depth 100
    Add-JsonLine -Path $EventsPath -Value ([ordered]@{
        direction = 'bridge'
        phase = 'request_start'
        method = $method
        id = $id
        bytes = ([System.Text.Encoding]::UTF8.GetByteCount($body))
        at_unix_ms = Get-UnixMs
    })
    $response = Invoke-RestMethod -Method Post -Uri $script:CodexAppServerRequestBridgeUrl -Headers @{ Authorization = "Bearer $token" } -ContentType 'application/json' -Body $body -TimeoutSec 1800
    Add-JsonLine -Path $EventsPath -Value ([ordered]@{
        direction = 'bridge'
        phase = 'request_ok'
        method = $method
        id = $id
        approval_id = [string](Get-JsonProperty $response 'approval_id')
        final_status = [string](Get-JsonProperty $response 'final_status')
        at_unix_ms = Get-UnixMs
    })
    return $response
}

function Send-AppServerErrorResponse($Socket, $Id, [string]$Message) {
    Send-WebSocketJson $Socket ([ordered]@{
        id = $Id
        error = [ordered]@{
            code = -32000
            message = $Message
        }
    })
}

function Handle-AppServerRequest($Socket, $Message) {
    $method = Get-JsonProperty $Message 'method'
    $id = Get-JsonProperty $Message 'id'
    if ($null -eq $method -or $null -eq $id) {
        return $false
    }
    try {
        $bridge = Invoke-CodexAppServerRequestBridge $Message
        $result = Get-JsonProperty $bridge 'app_server_response'
        if ($null -eq $result) {
            throw 'bridge response did not contain app_server_response'
        }
        Send-WebSocketJson $Socket ([ordered]@{
            id = $id
            result = $result
        })
        Write-Control @{
            last_app_server_request_status = 'responded'
            last_app_server_request_method = [string]$method
            last_app_server_request_id = [string]$id
            last_app_server_request_approval_id = [string](Get-JsonProperty $bridge 'approval_id')
            last_app_server_request_final_status = [string](Get-JsonProperty $bridge 'final_status')
            last_app_server_request_at_unix_ms = Get-UnixMs
        }
    } catch {
        $detail = $_.Exception.Message
        Add-JsonLine -Path $EventsPath -Value ([ordered]@{
            direction = 'bridge'
            phase = 'request_failed'
            method = [string]$method
            id = $id
            error = $detail
            at_unix_ms = Get-UnixMs
        })
        Send-AppServerErrorResponse -Socket $Socket -Id $id -Message $detail
        Write-Control @{
            last_app_server_request_status = 'failed'
            last_app_server_request_method = [string]$method
            last_app_server_request_id = [string]$id
            last_app_server_request_error = $detail
            last_app_server_request_at_unix_ms = Get-UnixMs
        }
    }
    return $true
}

function Receive-WebSocketText($Socket) {
    $buffer = [byte[]]::new(65536)
    $builder = [System.Text.StringBuilder]::new()
    do {
        $segment = [ArraySegment[byte]]::new($buffer)
        $result = $Socket.ReceiveAsync($segment, [Threading.CancellationToken]::None).GetAwaiter().GetResult()
        if ($result.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) {
            throw 'codex app-server websocket closed before the turn completed'
        }
        [void]$builder.Append([System.Text.Encoding]::UTF8.GetString($buffer, 0, $result.Count))
    } while (-not $result.EndOfMessage)
    return $builder.ToString()
}

function Read-Message($Socket) {
    $text = Receive-WebSocketText $Socket
    Add-JsonLine -Path $EventsPath -Value $text
    Add-JsonLine -Path $StdoutPath -Value $text
    return ($text | ConvertFrom-Json)
}

function Update-FromNotification($Message) {
    $method = Get-JsonProperty $Message 'method'
    $params = Get-JsonProperty $Message 'params'
    if ($null -eq $method -or $null -eq $params) {
        return
    }
    if ($method -eq 'item/completed') {
        $item = Get-JsonProperty $params 'item'
        $itemType = Get-JsonProperty $item 'type'
        if ($itemType -eq 'agentMessage') {
            $itemText = [string](Get-JsonProperty $item 'text')
            if (-not [string]::IsNullOrWhiteSpace($itemText)) {
                $script:LastAgentMessageText = $itemText
                $phase = [string](Get-JsonProperty $item 'phase')
                if ($phase -eq 'final_answer') {
                    $script:LastFinalAgentMessageText = $itemText
                }
            }
        }
    }
    $turn = Get-JsonProperty $params 'turn'
    $turnId = Get-JsonProperty $turn 'id'
    if ($method -eq 'turn/started' -and $null -ne $turnId) {
        $script:TurnId = [string]$turnId
        $script:TurnStatus = [string](Get-JsonProperty $turn 'status')
        Write-Control @{}
    }
    if ($method -eq 'turn/completed' -and $null -ne $turnId) {
        $script:TurnId = [string]$turnId
        $script:TurnStatus = [string](Get-JsonProperty $turn 'status')
        Write-Control @{ turn_status = $script:TurnStatus }
    }
}

function Receive-Response($Socket, [int]$Id, [string]$ExpectedMethod) {
    while ($true) {
        $message = Read-Message $Socket
        if (Handle-AppServerRequest -Socket $Socket -Message $message) {
            continue
        }
        Update-FromNotification $message
        $messageId = Get-JsonProperty $message 'id'
        if ($null -ne $messageId -and [int]$messageId -eq $Id) {
            $responseError = Get-JsonProperty $message 'error'
            if ($null -ne $responseError) {
                Throw-CodexAppServerRpcFailure -Id $Id -ExpectedMethod $ExpectedMethod -ResponseError $responseError -Phase 'receive_response'
            }
            return $message
        }
    }
}

function Get-FinalAgentText($Turn) {
    $text = $null
    $items = Get-JsonProperty $Turn 'items'
    if ($null -eq $items) {
        return $null
    }
    foreach ($item in $items) {
        $itemType = Get-JsonProperty $item 'type'
        $itemText = Get-JsonProperty $item 'text'
        if ($itemType -eq 'agentMessage' -and -not [string]::IsNullOrWhiteSpace([string]$itemText)) {
            $text = [string]$itemText
        }
    }
    return $text
}

$script:Endpoint = $null
$script:AppServerPid = $null
$script:ThreadId = $null
$script:TurnId = $null
$script:TurnStatus = 'starting'
$script:LastErrorText = $null
$script:LastAgentMessageText = $null
$script:LastFinalAgentMessageText = $null
$script:CodexApprovalPolicy = Get-CodexApprovalPolicy
$script:CodexSandboxMode = Get-CodexSandboxMode
$script:CodexAppServerRequestBridgeUrl = Get-CodexAppServerRequestBridgeUrl -Url $McpUrl
$script:SynapseStartupApprovedMcpTools = @(
    'health',
    'session_list',
    'get_target',
    'agent'
)
$socket = $null
$appServer = $null

try {
    [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($ControlPath)) | Out-Null
    [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($EventsPath)) | Out-Null
    $port = Get-FreeTcpPort
    $script:Endpoint = "ws://127.0.0.1:$port"
    $healthUrl = "http://127.0.0.1:$port/healthz"

    $appArgs = @(
        'app-server',
        '--listen', $script:Endpoint,
        '-c', ('sandbox_mode=' + (ConvertTo-TomlStringLiteral $script:CodexSandboxMode)),
        '-c', ('approval_policy=' + (ConvertTo-TomlStringLiteral $script:CodexApprovalPolicy)),
        '-c', ('mcp_servers.synapse.url=' + (ConvertTo-TomlStringLiteral $McpUrl)),
        '-c', 'mcp_servers.synapse.bearer_token_env_var="SYNAPSE_BEARER_TOKEN"'
    )
    foreach ($tool in $script:SynapseStartupApprovedMcpTools) {
        $appArgs += @('-c', ('mcp_servers.synapse.tools.' + $tool + '.approval_mode=' + (ConvertTo-TomlStringLiteral 'approve')))
    }
    if (-not [string]::IsNullOrWhiteSpace($Model)) {
        $appArgs += @('-c', ('model=' + (ConvertTo-TomlStringLiteral $Model)))
    }
    # The app-server transport is already the turn-completion observer for this
    # spawn. Do not pass the legacy Codex `notify` hook here: on Windows, its
    # TOML array is shell-fragile through npm shims and can prevent app-server
    # startup before any control artifact is usable.

    $launch = Get-CodexLaunchSpec -AppArgs $appArgs
    $appServer = Start-Process -FilePath $launch.File -ArgumentList $launch.Args -WindowStyle Hidden -RedirectStandardOutput $AppServerStdoutPath -RedirectStandardError $AppServerStderrPath -PassThru
    $script:AppServerPid = [int]$appServer.Id
    $script:TurnStatus = 'app_server_started'
    Write-Control @{}

    Wait-AppServerReady -Url $healthUrl -TimeoutMs 15000
    $socket = Connect-AppServer $script:Endpoint

    Send-WebSocketJson $socket ([ordered]@{
        id = 1
        method = 'initialize'
        params = [ordered]@{
            clientInfo = [ordered]@{ name = 'synapse-act-spawn-agent'; version = '0.1.0' }
            capabilities = [ordered]@{ experimentalApi = $true }
        }
    })
    [void](Receive-Response $socket 1 'initialize')

    $threadParams = [ordered]@{
        cwd = $WorkingDir
        sandbox = $script:CodexSandboxMode
        approvalPolicy = $script:CodexApprovalPolicy
        approvalsReviewer = 'user'
        ephemeral = $true
        threadSource = 'subagent'
        sessionStartSource = 'startup'
        runtimeWorkspaceRoots = @($WorkingDir)
        config = [ordered]@{
            mcp_servers = [ordered]@{
                synapse = [ordered]@{
                    url = $McpUrl
                    bearer_token_env_var = 'SYNAPSE_BEARER_TOKEN'
                }
            }
        }
    }
    if (-not [string]::IsNullOrWhiteSpace($Model)) {
        $threadParams['model'] = $Model
    }
    Send-WebSocketJson $socket ([ordered]@{ id = 2; method = 'thread/start'; params = $threadParams })
    $threadResponse = Receive-Response $socket 2 'thread/start'
    $threadResult = Get-JsonProperty $threadResponse 'result'
    $thread = Get-JsonProperty $threadResult 'thread'
    $script:ThreadId = [string](Get-JsonProperty $thread 'id')
    $script:TurnStatus = 'thread_started'
    Write-Control @{}

    $prompt = [string](Get-Content -Raw -LiteralPath $PromptPath -Encoding UTF8)
    $turnParams = [ordered]@{
        threadId = $script:ThreadId
        input = @([ordered]@{ type = 'text'; text = $prompt })
        cwd = $WorkingDir
        approvalPolicy = $script:CodexApprovalPolicy
        approvalsReviewer = 'user'
        sandboxPolicy = (New-CodexSandboxPolicy -Mode $script:CodexSandboxMode -Root $WorkingDir)
        runtimeWorkspaceRoots = @($WorkingDir)
    }
    if (-not [string]::IsNullOrWhiteSpace($Model)) {
        $turnParams['model'] = $Model
    }
    $script:TurnStatus = 'turn_start_sending'
    Write-Control @{}
    Send-WebSocketJson $socket ([ordered]@{ id = 3; method = 'turn/start'; params = $turnParams })
    $script:TurnStatus = 'turn_start_sent'
    Write-Control @{}
    $turnResponse = Receive-Response $socket 3 'turn/start'
    $turnResult = Get-JsonProperty $turnResponse 'result'
    $turn = Get-JsonProperty $turnResult 'turn'
    $script:TurnId = [string](Get-JsonProperty $turn 'id')
    $script:TurnStatus = [string](Get-JsonProperty $turn 'status')
    Write-Control @{}

    while ($true) {
        $message = Read-Message $socket
        if (Handle-AppServerRequest -Socket $socket -Message $message) {
            continue
        }
        Update-FromNotification $message
        $method = Get-JsonProperty $message 'method'
        $params = Get-JsonProperty $message 'params'
        $completedTurn = Get-JsonProperty $params 'turn'
        if ($method -eq 'turn/completed' -and [string](Get-JsonProperty $params 'threadId') -eq $script:ThreadId -and [string](Get-JsonProperty $completedTurn 'id') -eq $script:TurnId) {
            $script:TurnStatus = [string](Get-JsonProperty $completedTurn 'status')
            $finalText = Get-FinalAgentText $completedTurn
            if ([string]::IsNullOrWhiteSpace($finalText)) {
                $finalText = $script:LastFinalAgentMessageText
            }
            if ([string]::IsNullOrWhiteSpace($finalText)) {
                $finalText = $script:LastAgentMessageText
            }
            if ([string]::IsNullOrWhiteSpace($finalText)) {
                $finalText = ([ordered]@{
                    schema_version = 1
                    spawn_id = $SpawnId
                    cli = 'codex'
                    protocol = 'codex_app_server_ws'
                    status = $script:TurnStatus
                    thread_id = $script:ThreadId
                    turn_id = $script:TurnId
                    control_path = $ControlPath
                } | ConvertTo-Json -Depth 20)
            }
            Write-TextNoBom -Path $FinalMessagePath -Value $finalText
            Write-Control @{ turn_status = $script:TurnStatus }
            exit 0
        }
    }
} catch {
    $script:LastErrorText = $_.Exception.Message
    Write-Control @{ last_error = $script:LastErrorText; turn_status = 'runner_error' }
    Append-LineNoBom -Path $StderrPath -Value ("SYNAPSE_CODEX_APP_SERVER_RUNNER_ERROR: " + $script:LastErrorText)
    exit 1
} finally {
    if ($null -ne $socket) {
        try { $socket.Dispose() } catch {}
    }
    if ($null -ne $appServer -and -not $appServer.HasExited) {
        Stop-OwnedProcessTree -RootPid ([int]$appServer.Id)
    }
}
