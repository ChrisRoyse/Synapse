#!/usr/bin/env bash
# =============================================================================
# Synapse installer — WSL entry point.
#
# Synapse's controlling body is ALWAYS the Windows-native synapse-mcp.exe HTTP
# daemon (only Windows has real SendInput / UI Automation / WGC capture; it
# drives Windows windows AND WSLg GUI windows, and reaches WSL CLIs via wsl.exe).
# Installing "in WSL" therefore means: build + run that Windows daemon through
# interop, then point the WSL-side MCP clients (Claude Code, Codex) at it.
#
# This script:
#   1. Verifies it is running in WSL with working interop + a Windows Rust
#      toolchain.
#   2. Syncs this source tree to a LOCAL Windows path (building over
#      \\wsl.localhost bakes transient drive paths into the binary).
#   3. Invokes scripts/synapse-setup.ps1 on the Windows side to build, install,
#      deploy profiles, register the auto-start daemon, and wire Windows clients.
#   4. Wires the WSL-side Claude Code + Codex at the connect bridge.
#
# Fail-loud: every prerequisite is checked; on any failure the script stops and
# prints exactly what failed and how to fix it. No silent fallbacks.
# =============================================================================
set -euo pipefail

say()  { printf '\033[36m[synapse-install]\033[0m %s\n' "$*"; }
die()  { printf '\033[31m[synapse-install] FATAL:\033[0m %s\n' "$*" >&2; exit 1; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIND="127.0.0.1:7700"

# --- 1. Environment checks --------------------------------------------------
say "Checking environment"
grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null \
  || die "Not running under WSL. On native Windows run scripts/synapse-setup.ps1 in PowerShell instead."

CMD="/mnt/c/Windows/System32/cmd.exe"
PWSH="/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"
[ -x "$CMD" ]  || die "cmd.exe not found at $CMD — WSL interop to Windows is required."
[ -x "$PWSH" ] || die "powershell.exe not found at $PWSH — WSL interop to Windows is required."

# Resolve the Windows user profile (the Windows user can differ from the WSL user).
WIN_USERPROFILE="$("$CMD" /c 'echo %USERPROFILE%' 2>/dev/null | tr -d '\r')"
[ -n "$WIN_USERPROFILE" ] || die "Could not read Windows %USERPROFILE% via interop."
WIN_HOME_WSL="$(wslpath "$WIN_USERPROFILE")"
say "Windows profile: $WIN_USERPROFILE  ($WIN_HOME_WSL)"

WIN_CARGO="$WIN_HOME_WSL/.cargo/bin/cargo.exe"
[ -x "$WIN_CARGO" ] || die "Windows cargo not found at $WIN_CARGO. Install the Rust toolchain on Windows (https://rustup.rs) — the daemon is a Windows binary and must be built with the Windows toolchain."

WIN_EXE_WSL="$WIN_HOME_WSL/.cargo/bin/synapse-mcp.exe"   # /mnt path used by WSL clients
WIN_EXE_WIN="$WIN_USERPROFILE\\.cargo\\bin\\synapse-mcp.exe"

# --- 2. Sync source to a local Windows path ---------------------------------
SRC_WSL="$WIN_HOME_WSL/synapse-src"
SRC_WIN="$WIN_USERPROFILE\\synapse-src"
say "Syncing source -> $SRC_WIN"
mkdir -p "$SRC_WSL"
rsync -a --delete \
  --exclude='/target' --exclude='/.git' --exclude='/docs2' \
  --exclude='/.playwright-mcp' --exclude='*.log' \
  "$REPO_ROOT/" "$SRC_WSL/"
[ -f "$SRC_WSL/Cargo.toml" ] || die "Source sync failed: $SRC_WSL/Cargo.toml missing."
[ -d "$SRC_WSL/crates/synapse-profiles/profiles" ] || die "Source sync failed: bundled profiles missing."

# --- 3. Build + configure the Windows daemon via the PowerShell setup --------
say "Running Windows-side setup (build + daemon + Windows clients) — this can take ~25 min on a cold build"
PS1_WIN="$SRC_WIN\\scripts\\synapse-setup.ps1"
"$PWSH" -NoProfile -ExecutionPolicy Bypass -File "$PS1_WIN" -SourceDir "$SRC_WIN" -Bind "$BIND" \
  || die "Windows-side setup failed. See the [synapse-setup] output above for the exact failing step."

# --- 4. Wire WSL-side clients at the connect bridge -------------------------
say "Wiring WSL-side MCP clients"

# Claude Code (WSL): stdio client -> connect bridge (launches the Windows .exe via interop)
if command -v claude >/dev/null 2>&1; then
  claude mcp remove synapse -s user >/dev/null 2>&1 || true
  claude mcp add --scope user synapse -- "$WIN_EXE_WSL" --mode connect --bind "$BIND"
  say "Claude Code (WSL) wired -> connect bridge ($WIN_EXE_WSL)."
else
  say "claude CLI not found in WSL; skipping Claude Code wiring."
fi

# Codex (WSL): stdio-only -> connect bridge, with the operator hotkey disabled
CODEX_CFG="$HOME/.codex/config.toml"
if [ -f "$CODEX_CFG" ]; then
  if ! grep -qE '^\[mcp_servers\.synapse\]' "$CODEX_CFG"; then
    {
      printf '\n[mcp_servers.synapse]\n'
      printf 'command = "%s"\n' "$WIN_EXE_WSL"
      printf 'args = ["--mode", "connect", "--bind", "%s"]\n' "$BIND"
      printf 'env = { SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY = "1" }\n'
    } >> "$CODEX_CFG"
    say "Codex (WSL) wired -> [mcp_servers.synapse] connect bridge."
  else
    say "Codex already has [mcp_servers.synapse]; left as-is."
  fi
else
  say "No Codex config at $CODEX_CFG; skipping."
fi

# --- 5. Verify the daemon is reachable from WSL -----------------------------
say "Verifying daemon health from WSL"
TOKEN_WSL="$(wslpath "$("$CMD" /c 'echo %APPDATA%' 2>/dev/null | tr -d '\r')")/synapse/token.txt"
if [ -f "$TOKEN_WSL" ]; then
  TOK="$(tr -d '\r\n' < "$TOKEN_WSL")"
  if curl -fsS -m 5 -H "Authorization: Bearer $TOK" "http://$BIND/health" >/dev/null 2>&1; then
    say "Daemon healthy and reachable from WSL on http://$BIND."
  else
    die "Daemon not reachable from WSL on http://$BIND. The Windows daemon may not have started; check %LOCALAPPDATA%\\synapse\\logs\\daemon.log."
  fi
else
  die "Token not found at $TOKEN_WSL — the Windows setup did not complete."
fi

say "Done. Restart Claude Code / Codex; call the synapse 'health' tool to confirm."
