# Codex 0.157.x on Windows can allocate visible consoles from its detached server.
# Keep new npm-launched sessions attached to their terminal. Existing sessions stay alive.
[CmdletBinding()]
param(
    [string]$LauncherPath = "$env:APPDATA\npm\node_modules\@openai\codex\bin\codex.js",
    [switch]$Restore
)
$ErrorActionPreference = 'Stop'
$original = 'const child = spawn(binaryPath, process.argv.slice(2), {'
$replacement = @'
// Synapse Windows console workaround: begin (issue #2266)
// --no-daemon avoids both starting and reusing the detached shared server.
// Explicit remote connections and the operator opt-out retain upstream behavior.
const synapseCodexArgs = process.argv.slice(2);
if (
  process.platform === "win32" &&
  process.env.SYNAPSE_CODEX_USE_SHARED_DAEMON !== "1" &&
  !synapseCodexArgs.includes("--no-daemon") &&
  !synapseCodexArgs.some((arg) => arg === "--remote" || arg.startsWith("--remote="))
) {
  synapseCodexArgs.unshift("--no-daemon");
}
// Synapse Windows console workaround: end
const child = spawn(binaryPath, synapseCodexArgs, {
'@
$text = [IO.File]::ReadAllText($LauncherPath)
$newline = if ($text.Contains("`r`n")) { "`r`n" } else { "`n" }
$replacement = $replacement.Replace("`r`n", "`n").Replace("`n", $newline)
$backupPath = "$LauncherPath.synapse-console-backup"

if ($Restore) {
    if (!$text.Contains($replacement)) {
        throw 'CODEX_CONSOLE_RESTORE_REFUSED: expected workaround not present; launcher left unchanged.'
    }
    $restored = $text.Replace($replacement, $original)
    [IO.File]::WriteAllText($LauncherPath, $restored, [Text.UTF8Encoding]::new($false))
    Write-Output "CODEX_CONSOLE_WORKAROUND_RESTORED path=$LauncherPath"
    return
}
if ($text.Contains($replacement)) {
    Write-Output "CODEX_CONSOLE_WORKAROUND_PRESENT path=$LauncherPath"
    return
}
if ([regex]::Matches($text, [regex]::Escape($original)).Count -ne 1 -or
    $text.Contains('Synapse Windows console workaround: begin')) {
    throw 'CODEX_CONSOLE_INSTALL_REFUSED: unsupported launcher shape; launcher left unchanged.'
}
if (!(Test-Path -LiteralPath $backupPath)) {
    [IO.File]::WriteAllText($backupPath, $text, [Text.UTF8Encoding]::new($false))
}
[IO.File]::WriteAllText($LauncherPath, $text.Replace($original, $replacement), [Text.UTF8Encoding]::new($false))
Write-Output "CODEX_CONSOLE_WORKAROUND_INSTALLED path=$LauncherPath backup=$backupPath"
Write-Output 'New launches use --no-daemon. Existing terminals and the shared server were not restarted.'
