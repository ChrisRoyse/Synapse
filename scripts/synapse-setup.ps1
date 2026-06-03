<#
.SYNOPSIS
  Windows-side Synapse setup: build/install the daemon binary, deploy bundled
  profiles, generate the bearer token, register the auto-start HTTP daemon, and
  (optionally) wire the Windows-side MCP clients. Idempotent and fail-loud.

.DESCRIPTION
  Synapse has exactly ONE controlling body: the Windows-native synapse-mcp.exe
  HTTP daemon. It is the only process that can do real Win32 SendInput / UI
  Automation / WGC-DXGI capture, and it controls BOTH Windows programs (native
  windows) and WSL programs (WSLg GUI apps render as real Windows windows;
  act_run_shell / act_launch reach WSL CLIs via wsl.exe). Every MCP client — on
  Windows or in WSL — connects to this one daemon.

  This script makes that body exist and run, then points the Windows-side
  clients at it. The WSL-side entry (scripts/synapse-install.sh) calls this same
  script through interop and then wires the WSL-side clients.

  Robustness decisions baked in here (learned the hard way):
    * Build from the LOCAL source path (cd into -SourceDir). Building over a
      \\wsl.localhost / pushd-mapped drive bakes transient Z:\ paths into the
      binary (CARGO_MANIFEST_DIR) and intermittently fails cargo's dep-info
      step. -SourceDir must be a real local path.
    * Deploy the bundled profiles NEXT TO the installed exe so the daemon's
      executable-relative profile lookup always resolves, and ALSO pass
      --profile-dir explicitly. A compile-time CARGO_MANIFEST_DIR profile path
      never exists on an installed host.
    * Use a persistent CARGO_TARGET_DIR so re-installs are incremental, not a
      ~25-minute RocksDB rebuild every time.

  Nothing here silently falls back: every prerequisite is checked and throws a
  clear error naming exactly what failed and how to fix it.

.PARAMETER SourceDir
  Path to a LOCAL synapse source checkout to build from. Required unless
  -SkipBuild is set. Must be on a real local drive (not \\wsl.localhost or a
  pushd-mapped UNC drive).

.PARAMETER SkipBuild
  Do not build; require an already-installed synapse-mcp.exe at -ExePath.

.PARAMETER Bind
  Loopback address the daemon binds. Default 127.0.0.1:7700.

.PARAMETER WireClients
  Wire the Windows-side MCP clients (Claude Code via HTTP, Codex + Claude
  Desktop via the connect bridge). Default $true.

.PARAMETER Remove
  Uninstall: stop + unregister the scheduled task. Leaves the DB, token, and
  binary in place unless -Purge is also given.

.PARAMETER Purge
  With -Remove, also delete the daemon DB, deployed profiles, and token.
#>
[CmdletBinding()]
param(
    [string]$SourceDir,
    [switch]$SkipBuild,
    [string]$Bind        = '127.0.0.1:7700',
    [string]$ExePath     = "$env:USERPROFILE\.cargo\bin\synapse-mcp.exe",
    [string]$CargoTarget = "$env:LOCALAPPDATA\synapse\build-target",
    [string]$DbPath      = "$env:LOCALAPPDATA\synapse\db-daemon",
    [string]$ProfilesDir = "$env:USERPROFILE\.cargo\bin\profiles",
    [string]$LogDir      = "$env:LOCALAPPDATA\synapse\logs",
    [string]$TokenPath   = "$env:APPDATA\synapse\token.txt",
    [string]$TaskName    = 'SynapseMcpDaemon',
    [switch]$SkipClientWiring,
    [switch]$Remove,
    [switch]$Purge
)

$ErrorActionPreference = 'Stop'
function Info($m)  { Write-Host "[synapse-setup] $m" }
function Step($m)  { Write-Host "`n=== $m ===" -ForegroundColor Cyan }
function Die($m)   { throw "[synapse-setup] FATAL: $m" }

# ---------------------------------------------------------------------------
# Uninstall path
# ---------------------------------------------------------------------------
if ($Remove) {
    Step "Removing scheduled task '$TaskName'"
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask  -TaskName $TaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Info "Unregistered '$TaskName'."
    } else { Info "Task '$TaskName' not present." }
    Info "Stopping any running daemon/bridge processes."
    taskkill /im synapse-mcp.exe /f 2>$null | Out-Null
    if ($Purge) {
        foreach ($p in @($DbPath, $ProfilesDir, (Split-Path -Parent $TokenPath))) {
            if (Test-Path $p) { Remove-Item -Recurse -Force $p; Info "Deleted $p" }
        }
    }
    Info "Done (remove)."
    return
}

# ---------------------------------------------------------------------------
# 1. Preflight
# ---------------------------------------------------------------------------
Step "Preflight"
$cargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"
if (-not $SkipBuild) {
    if (-not (Test-Path $cargo)) {
        Die "cargo not found at $cargo. Install the Rust toolchain (https://rustup.rs) on Windows, then re-run. Synapse builds with the current stable toolchain."
    }
    if (-not $SourceDir) { Die "-SourceDir is required unless -SkipBuild is set." }
    if (-not (Test-Path (Join-Path $SourceDir 'Cargo.toml'))) {
        Die "-SourceDir '$SourceDir' has no Cargo.toml. Point it at a synapse source checkout on a LOCAL drive."
    }
    if ($SourceDir -match '^\\\\' -or $SourceDir -match '^[Zz]:\\home\\') {
        Die "-SourceDir '$SourceDir' looks like a UNC / WSL-mapped path. Build from a real local copy: building over \\wsl.localhost bakes transient drive paths into the binary."
    }
    Info "cargo: $((& $cargo --version))"
}

# ---------------------------------------------------------------------------
# 2. Build (local source -> persistent target) and verify the binary
# ---------------------------------------------------------------------------
if (-not $SkipBuild) {
    Step "Building synapse-mcp (release) from $SourceDir"
    New-Item -ItemType Directory -Force -Path $CargoTarget | Out-Null
    $env:CARGO_TARGET_DIR = $CargoTarget
    Push-Location $SourceDir
    try {
        & $cargo build --release -p synapse-mcp
        if ($LASTEXITCODE -ne 0) { Die "cargo build failed (exit $LASTEXITCODE). See output above." }
    } finally { Pop-Location }
    $built = Join-Path $CargoTarget 'release\synapse-mcp.exe'
    if (-not (Test-Path $built)) { Die "Build reported success but $built is missing." }
    Info "Built: $built ($([math]::Round((Get-Item $built).Length/1MB,1)) MB)"
}

# ---------------------------------------------------------------------------
# 3. Stop the running daemon/bridges so the .exe is not locked, then install
# ---------------------------------------------------------------------------
Step "Installing daemon binary -> $ExePath"
if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
}
taskkill /im synapse-mcp.exe /f 2>$null | Out-Null
Start-Sleep -Seconds 2
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ExePath) | Out-Null
if (-not $SkipBuild) {
    if (Test-Path $ExePath) { Copy-Item $ExePath "$ExePath.bak" -Force; Info "Backed up old binary -> $ExePath.bak" }
    Copy-Item (Join-Path $CargoTarget 'release\synapse-mcp.exe') $ExePath -Force
}
if (-not (Test-Path $ExePath)) { Die "No daemon binary at $ExePath (build skipped and none installed)." }
$ver = (& $ExePath --version) 2>&1
Info "Installed binary reports: $ver"

# ---------------------------------------------------------------------------
# 4. Deploy bundled profiles next to the exe (executable-relative lookup) +
#    keep an explicit --profile-dir for belt-and-suspenders.
# ---------------------------------------------------------------------------
Step "Deploying bundled profiles -> $ProfilesDir"
$srcProfiles = if ($SourceDir) { Join-Path $SourceDir 'crates\synapse-profiles\profiles' } else { $null }
if ($srcProfiles -and (Test-Path $srcProfiles)) {
    New-Item -ItemType Directory -Force -Path $ProfilesDir | Out-Null
    Copy-Item "$srcProfiles\*" $ProfilesDir -Recurse -Force
    $n = (Get-ChildItem $ProfilesDir -Filter *.toml -File).Count
    if ($n -lt 1) { Die "Copied profiles but found 0 .toml files in $ProfilesDir." }
    Info "Deployed $n profiles."
} elseif (-not (Test-Path $ProfilesDir)) {
    Die "No bundled profiles found (source '$srcProfiles' missing and $ProfilesDir absent). Profile-dependent tools (reflexes, action policy) need these."
} else { Info "Reusing existing profiles at $ProfilesDir." }

# ---------------------------------------------------------------------------
# 5. Token, DB and log dirs
# ---------------------------------------------------------------------------
Step "Bearer token + data dirs"
$tokDir = Split-Path -Parent $TokenPath
New-Item -ItemType Directory -Force -Path $tokDir, $DbPath, $LogDir | Out-Null
if (-not (Test-Path $TokenPath)) {
    $bytes = New-Object byte[] 32
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    ($bytes | ForEach-Object { $_.ToString('x2') }) -join '' | Set-Content -Path $TokenPath -NoNewline -Encoding ascii
    Info "Generated token -> $TokenPath"
} else { Info "Reusing token -> $TokenPath" }
$token = (Get-Content -Raw $TokenPath).Trim()
if ($token.Length -lt 16) { Die "Token at $TokenPath is too short ($($token.Length) chars); delete it and re-run to regenerate." }

# ---------------------------------------------------------------------------
# 6. Register + start the auto-start HTTP daemon (interactive desktop session)
# ---------------------------------------------------------------------------
Step "Registering auto-start daemon task '$TaskName'"
$launcher = Join-Path $LogDir 'synapse-daemon-launch.cmd'
$daemonLog = Join-Path $LogDir 'daemon.log'
@"
@echo off
set SYNAPSE_BEARER_TOKEN=$token
"$ExePath" --mode http --bind $Bind --db "$DbPath" --profile-dir "$ProfilesDir" --log-level info >> "$daemonLog" 2>&1
"@ | Set-Content -Path $launcher -Encoding ascii

$action  = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\cmd.exe" -Argument "/c `"$launcher`""
$trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$princ   = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
$set     = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
            -StartWhenAvailable -MultipleInstances IgnoreNew -RestartCount 3 `
            -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
$set.Hidden = $true
if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
}
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $princ `
    -Settings $set -Description "Synapse MCP HTTP daemon (loopback) - the single body controlling Windows + WSL programs." | Out-Null
Start-ScheduledTask -TaskName $TaskName
Info "Task registered and started."

# ---------------------------------------------------------------------------
# 7. Health verify (source of truth: the live daemon)
# ---------------------------------------------------------------------------
Step "Verifying daemon health (http://$Bind/health)"
$ok = $false
for ($i=0; $i -lt 15; $i++) {
    Start-Sleep -Seconds 2
    try {
        $h = Invoke-RestMethod -Uri "http://$Bind/health" -Headers @{ Authorization = "Bearer $token" } -TimeoutSec 4
        if ($h.ok) {
            Info ("Daemon OK: pid={0} version={1} db={2}" -f $h.pid, $h.version, $h.subsystems.storage.db_path)
            $ok = $true; break
        }
    } catch { }
}
if (-not $ok) { Die "Daemon did not become healthy on http://$Bind/health. Check $daemonLog for STORAGE_* / bind errors." }

# ---------------------------------------------------------------------------
# 8. Wire the Windows-side MCP clients
# ---------------------------------------------------------------------------
if (-not $SkipClientWiring) {
    Step "Wiring Windows-side MCP clients"

    # Claude Code (Windows) speaks Streamable HTTP natively -> point at the daemon.
    $claude = Get-Command claude -ErrorAction SilentlyContinue
    if ($claude) {
        try {
            & $claude.Source mcp remove synapse -s user 2>$null | Out-Null
            & $claude.Source mcp add --scope user --transport http synapse "http://$Bind/mcp" --header "Authorization: Bearer $token"
            Info "Claude Code (Windows) wired via HTTP transport."
        } catch { Info "WARN: 'claude mcp add' failed: $($_.Exception.Message). Wire it manually (transport http -> http://$Bind/mcp)." }
    } else { Info "claude CLI not found on Windows PATH; skipping Claude Code wiring." }

    # Codex + Claude Desktop are stdio-only -> connect bridge.
    $bridgeArgs = @('--mode','connect','--bind',$Bind)

    $codexCfg = "$env:USERPROFILE\.codex\config.toml"
    if (Test-Path $codexCfg) {
        $c = Get-Content -Raw $codexCfg
        if ($c -notmatch '(?m)^\[mcp_servers\.synapse\]') {
            $argsToml = ($bridgeArgs | ForEach-Object { '"' + $_ + '"' }) -join ', '
            Add-Content $codexCfg "`n[mcp_servers.synapse]`ncommand = `"$($ExePath -replace '\\','\\')`"`nargs = [$argsToml]`nenv = { SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY = `"1`" }`n"
            Info "Codex (Windows) wired -> [mcp_servers.synapse] connect bridge."
        } else { Info "Codex already has [mcp_servers.synapse]; left as-is." }
    } else { Info "No Codex config at $codexCfg; skipping." }

    $desktopCfg = "$env:APPDATA\Claude\claude_desktop_config.json"
    if (Test-Path $desktopCfg) {
        try {
            $j = Get-Content -Raw $desktopCfg | ConvertFrom-Json
            if (-not $j.mcpServers) { $j | Add-Member -NotePropertyName mcpServers -NotePropertyValue (@{}) -Force }
            $j.mcpServers.synapse = @{ command = $ExePath; args = $bridgeArgs; env = @{ SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY = '1' } }
            ($j | ConvertTo-Json -Depth 12) | Set-Content $desktopCfg -Encoding utf8
            Info "Claude Desktop wired -> connect bridge."
        } catch { Info "WARN: could not update $desktopCfg : $($_.Exception.Message)" }
    } else { Info "No Claude Desktop config at $desktopCfg; skipping." }
}

Step "Done"
Info "Synapse daemon is live on http://$Bind (MCP: http://$Bind/mcp)."
Info "Token: $TokenPath   DB: $DbPath   Profiles: $ProfilesDir"
Info "WSL clients: run scripts/synapse-install.sh from WSL to wire Claude Code + Codex there."
