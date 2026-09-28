# Persist the verified Codex Windows console workaround without restarting clients.
[CmdletBinding()]
param([string]$CodexDirectory = "$env:USERPROFILE\.codex")
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Path $CodexDirectory -Force | Out-Null
$instructionsPath = Join-Path $CodexDirectory 'AGENTS.md'
$instruction = 'On native Windows, always pass `tty: true` to Codex `exec_command` for background commands.'
$existing = if (Test-Path -LiteralPath $instructionsPath -PathType Leaf) {
    [IO.File]::ReadAllText($instructionsPath)
} else { '' }
if ($existing.Contains($instruction)) {
    Write-Output "CODEX_BACKGROUND_POLICY_PRESENT path=$instructionsPath"
    return
}
$policy = @"

# Windows background commands

$instruction The managed Codex app-server plain-pipe runner can create visible PowerShell consoles. PTY execution avoids these windows. Never stop existing terminals or another agent session to suppress console flashes. Do not spawn additional agents unless the user explicitly requests delegation or parallel agents.
"@
[IO.File]::WriteAllText($instructionsPath, $existing.TrimEnd() + "`n" + $policy + "`n", [Text.UTF8Encoding]::new($false))
Write-Output "CODEX_BACKGROUND_POLICY_INSTALLED path=$instructionsPath"
