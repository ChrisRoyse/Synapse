<#
.SYNOPSIS
  Start the shared Synapse daemon with cloud API-model keys injected from
  Infisical, so it can spawn API-model agents (DeepSeek, and any other
  OpenAI-compatible provider) without any secret ever living on disk.

.DESCRIPTION
  API-model agents (issue #985) are registered in the local-model registry with
  an `api_key_env_var` (the *name* of an env var, never the secret itself). The
  daemon must carry that env var in its own environment, because the spawn path
  forwards it into each spawned local-agent child (act_launch clears the child
  env otherwise). This script resolves the daemon's environment from Infisical
  at launch time using a machine identity (universal auth) and execs the daemon
  under `infisical run`, which injects every project secret as an env var.

  No secret value is printed. If Infisical auth or the project lookup fails, the
  script errors out loudly and the daemon is NOT started — there is no silent
  fallback to a keyless daemon (which would 401 on the first API-model turn).

.PARAMETER UaEnvFile
  Path to a file defining INFISICAL_UA_CLIENT_ID, INFISICAL_UA_CLIENT_SECRET,
  INFISICAL_PROJECT_ID (and optionally INFISICAL_ENV). Defaults to
  $env:USERPROFILE\.config\aiwonder.env.

.PARAMETER Bind
  Daemon bind address. Default 127.0.0.1:7700 (the shared daemon).

.PARAMETER DbPath
  RocksDB path. Default: the daemon's standard %LOCALAPPDATA% location.

.PARAMETER ProfileDir
  Profile directory passed to synapse-mcp. Defaults to the installed binary's
  sibling profiles directory, matching the standard shared daemon launcher.

.EXAMPLE
  pwsh -File scripts/run-daemon-with-secrets.ps1
  pwsh -File scripts/run-daemon-with-secrets.ps1 -Bind 127.0.0.1:7700
#>
[CmdletBinding()]
param(
  [string]$UaEnvFile = (Join-Path $env:USERPROFILE ".config\aiwonder.env"),
  [string]$Env = "dev",
  [string]$Bind = "127.0.0.1:7700",
  [string]$DbPath,
  [string]$ProfileDir,
  [string]$LogLevel = "info"
)
$ErrorActionPreference = "Stop"

function Fail($msg) { Write-Error "run-daemon-with-secrets: $msg"; exit 1 }

# --- resolve the synapse-mcp binary (the installed daemon) ---
$exe = (Get-Command synapse-mcp -ErrorAction SilentlyContinue)?.Source
if (-not $exe) { $exe = Join-Path $env:USERPROFILE ".cargo\bin\synapse-mcp.exe" }
if (-not (Test-Path $exe)) { Fail "synapse-mcp binary not found (looked for it on PATH and in ~/.cargo/bin)" }

if (-not (Get-Command infisical -ErrorAction SilentlyContinue)) {
  Fail "infisical CLI not found on PATH (install it, or set the API-model key env vars yourself before starting the daemon)"
}
if (-not (Test-Path $UaEnvFile)) { Fail "Infisical universal-auth env file not found: $UaEnvFile" }

# --- load machine-identity credentials (values never printed) ---
$ua = @{}
foreach ($line in Get-Content $UaEnvFile) {
  if ($line -match '^\s*([A-Z0-9_]+)\s*=\s*(.*)\s*$') { $ua[$Matches[1]] = $Matches[2] }
}
foreach ($k in 'INFISICAL_UA_CLIENT_ID','INFISICAL_UA_CLIENT_SECRET','INFISICAL_PROJECT_ID') {
  if ([string]::IsNullOrWhiteSpace($ua[$k])) { Fail "$k is missing/empty in $UaEnvFile" }
}
if ($ua['INFISICAL_ENV']) { $Env = $ua['INFISICAL_ENV'] }

Write-Host "Authenticating to Infisical (machine identity) ..."
$token = & infisical login --method=universal-auth `
  --client-id="$($ua['INFISICAL_UA_CLIENT_ID'])" `
  --client-secret="$($ua['INFISICAL_UA_CLIENT_SECRET'])" --plain --silent 2>$null
if ([string]::IsNullOrWhiteSpace($token)) { Fail "Infisical universal-auth login failed (check the UA client id/secret)" }

# --- exec the daemon under infisical run (secrets injected as env vars) ---
if (-not $ProfileDir) {
  $ProfileDir = Join-Path (Split-Path -Parent $exe) "profiles"
}

$daemonArgs = @('--mode','http','--bind',$Bind)
if ($DbPath) { $daemonArgs += @('--db',$DbPath) }
if ($ProfileDir) { $daemonArgs += @('--profile-dir',$ProfileDir) }
if ($LogLevel) { $daemonArgs += @('--log-level',$LogLevel) }

Write-Host "Starting daemon with Infisical-injected secrets: $exe $($daemonArgs -join ' ')"
& infisical run `
  --projectId "$($ua['INFISICAL_PROJECT_ID'])" `
  --env "$Env" `
  --token "$token" `
  -- $exe @daemonArgs
exit $LASTEXITCODE
