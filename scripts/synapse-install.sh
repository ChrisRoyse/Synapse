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
#   4. Wires the WSL-side Claude Code + Codex to the daemon over Streamable HTTP.
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

install_synapse_token_loader() {
  local loader="$HOME/.config/synapse/mcp-env.sh"
  mkdir -p "$(dirname "$loader")"
  cat > "$loader" <<EOF
# Synapse MCP bearer token bridge for WSL agent clients.
# Source this from shell startup before launching Codex/Claude HTTP MCP clients.
_synapse_mcp_token_path="$TOKEN_WSL"
_synapse_mcp_surface_path="$SURFACE_WSL"

if [ ! -r "\$_synapse_mcp_token_path" ]; then
    printf '%s\n' "SYNAPSE_MCP_TOKEN_UNREADABLE path=\$_synapse_mcp_token_path remediation=start the Windows synapse-mcp daemon or repair token generation" >&2
else
    _synapse_mcp_token="\$(tr -d '\r\n' < "\$_synapse_mcp_token_path")"
    if [ -z "\$_synapse_mcp_token" ]; then
        printf '%s\n' "SYNAPSE_MCP_TOKEN_EMPTY path=\$_synapse_mcp_token_path remediation=regenerate the Synapse MCP bearer token" >&2
    elif [ "\${SYNAPSE_BEARER_TOKEN:-}" != "\$_synapse_mcp_token" ]; then
        export SYNAPSE_BEARER_TOKEN="\$_synapse_mcp_token"
    fi
    unset _synapse_mcp_token
fi

if [ ! -r "\$_synapse_mcp_surface_path" ]; then
    printf '%s\n' "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_MISSING path=\$_synapse_mcp_surface_path remediation=run scripts/synapse-setup.ps1 to write the current daemon tools/list fingerprint before starting Codex" >&2
else
    _synapse_mcp_surface_hash="\$(sed -n 's/.*"tool_surface_sha256"[[:space:]]*:[[:space:]]*"\([0-9a-fA-F][0-9a-fA-F]*\)".*/\1/p' "\$_synapse_mcp_surface_path" | head -n 1)"
    _synapse_mcp_surface_count="\$(sed -n 's/.*"tool_count"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "\$_synapse_mcp_surface_path" | head -n 1)"
    if [ -z "\$_synapse_mcp_surface_hash" ]; then
        printf '%s\n' "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_INVALID path=\$_synapse_mcp_surface_path remediation=delete the invalid snapshot and rerun scripts/synapse-setup.ps1" >&2
    else
        _synapse_mcp_start_dir="\$HOME/.config/synapse/codex-start-snapshots"
        _synapse_mcp_start_surface="\$_synapse_mcp_start_dir/codex-tool-surface-\$\$-\$(date +%s).json"
        if ! mkdir -p "\$_synapse_mcp_start_dir" || ! cp "\$_synapse_mcp_surface_path" "\$_synapse_mcp_start_surface"; then
            printf '%s\n' "SYNAPSE_CODEX_TOOL_SURFACE_START_SNAPSHOT_FAILED path=\$_synapse_mcp_start_surface remediation=repair permissions on \$_synapse_mcp_start_dir before starting Codex" >&2
        else
        export SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START="\$_synapse_mcp_surface_hash"
        export SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START="\$_synapse_mcp_surface_count"
        export SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START="\$_synapse_mcp_start_surface"
        fi
    fi
    unset _synapse_mcp_surface_hash _synapse_mcp_surface_count _synapse_mcp_start_dir _synapse_mcp_start_surface
fi

unset _synapse_mcp_token_path _synapse_mcp_surface_path
EOF
  chmod 600 "$loader"

  local marker='if [ -f "$HOME/.config/synapse/mcp-env.sh" ]; then'
  local block='# Synapse MCP HTTP bearer token bridge for WSL agent clients.
if [ -f "$HOME/.config/synapse/mcp-env.sh" ]; then
    . "$HOME/.config/synapse/mcp-env.sh"
else
    printf '"'"'%s\n'"'"' "SYNAPSE_MCP_ENV_MISSING path=$HOME/.config/synapse/mcp-env.sh remediation=restore the WSL Synapse MCP env loader" >&2
fi'
  for rc in "$HOME/.profile" "$HOME/.bashrc"; do
    [ -f "$rc" ] || continue
    if ! grep -Fq "$marker" "$rc"; then
      printf '\n%s\n' "$block" >> "$rc"
    fi
  done

  # Populate this process too, so the commands below see the same SoT.
  # shellcheck disable=SC1090
  . "$loader"
  [ -n "${SYNAPSE_BEARER_TOKEN:-}" ] || die "SYNAPSE_BEARER_TOKEN did not load from $TOKEN_WSL."
}

ensure_codex_synapse_policy() {
  local cfg="$1"
  local bind="$2"
  local tmp
  mkdir -p "$(dirname "$cfg")"
  [ -f "$cfg" ] || : > "$cfg"
  tmp="$(mktemp)" || die "Could not allocate a temp file to update $cfg."
  awk -v bind="$bind" '
    function emit() {
      print "[mcp_servers.synapse]"
      print "url = \"http://" bind "/mcp\""
      print "bearer_token_env_var = \"SYNAPSE_BEARER_TOKEN\""
      print "required = true"
      print "default_tools_approval_mode = \"approve\""
      emitted = 1
    }
    BEGIN { in_synapse = 0; found = 0; emitted = 0 }
    /^\[mcp_servers\.synapse\][[:space:]]*$/ {
      found = 1
      in_synapse = 1
      emit()
      next
    }
    /^\[/ && in_synapse {
      in_synapse = 0
    }
    in_synapse {
      if ($0 ~ /^[[:space:]]*(url|bearer_token_env_var|required|default_tools_approval_mode)[[:space:]]*=/) {
        next
      }
      if ($0 ~ /^[[:space:]]*$/) {
        next
      }
      print
      next
    }
    { print }
    END {
      if (!found) {
        if (NR > 0) {
          print ""
        }
        emit()
      }
    }
  ' "$cfg" > "$tmp" || die "Failed to rewrite $cfg with Synapse Codex MCP policy."
  mv "$tmp" "$cfg" || die "Failed to replace $cfg with repaired Synapse Codex MCP policy."
  chmod 600 "$cfg" 2>/dev/null || true

  grep -Fqx "[mcp_servers.synapse]" "$cfg" \
    || die "Codex config $cfg missing [mcp_servers.synapse] after repair."
  grep -Fqx "url = \"http://$bind/mcp\"" "$cfg" \
    || die "Codex config $cfg missing Synapse HTTP URL after repair."
  grep -Fqx 'bearer_token_env_var = "SYNAPSE_BEARER_TOKEN"' "$cfg" \
    || die "Codex config $cfg missing SYNAPSE_BEARER_TOKEN bearer env after repair."
  grep -Fqx 'required = true' "$cfg" \
    || die "Codex config $cfg missing required=true after repair."
  grep -Fqx 'default_tools_approval_mode = "approve"' "$cfg" \
    || die "Codex config $cfg missing default_tools_approval_mode=approve after repair."
}

# --- 2. Sync source to a local Windows path ---------------------------------
SRC_WSL="$WIN_HOME_WSL/synapse-src"
SRC_WIN="$WIN_USERPROFILE\\synapse-src"
say "Syncing source -> $SRC_WIN"
mkdir -p "$SRC_WSL"
rsync -a --delete \
  --exclude='/target' --exclude='/.git' \
  --exclude='/.playwright-mcp' --exclude='*.log' \
  "$REPO_ROOT/" "$SRC_WSL/"
[ -f "$SRC_WSL/Cargo.toml" ] || die "Source sync failed: $SRC_WSL/Cargo.toml missing."
[ -d "$SRC_WSL/crates/synapse-profiles/profiles" ] || die "Source sync failed: bundled profiles missing."

# --- 3. Build + configure the Windows daemon via the PowerShell setup --------
say "Running Windows-side setup (build + daemon + Windows clients) — this can take ~25 min on a cold build"
PS1_WIN="$SRC_WIN\\scripts\\synapse-setup.ps1"
"$PWSH" -NoProfile -ExecutionPolicy Bypass -File "$PS1_WIN" -SourceDir "$SRC_WIN" -Bind "$BIND" \
  || die "Windows-side setup failed. See the [synapse-setup] output above for the exact failing step."

# Read the bearer token before client wiring. The raw token is never printed.
TOKEN_WSL="$(wslpath "$("$CMD" /c 'echo %APPDATA%' 2>/dev/null | tr -d '\r')")/synapse/token.txt"
[ -f "$TOKEN_WSL" ] || die "Token not found at $TOKEN_WSL — the Windows setup did not complete."
TOK="$(tr -d '\r\n' < "$TOKEN_WSL")"
[ -n "$TOK" ] || die "Token at $TOKEN_WSL is empty."
SURFACE_WSL="$(wslpath "$("$CMD" /c 'echo %APPDATA%' 2>/dev/null | tr -d '\r')")/synapse/codex-tool-surface.json"
[ -f "$SURFACE_WSL" ] || die "Codex tool-surface snapshot not found at $SURFACE_WSL — the Windows setup did not complete."
install_synapse_token_loader

# --- 4. Wire WSL-side clients to Streamable HTTP ----------------------------
say "Wiring WSL-side MCP clients"

# Claude Code (WSL): Streamable HTTP -> shared Windows daemon.
if command -v claude >/dev/null 2>&1; then
  claude mcp remove synapse -s user >/dev/null 2>&1 || true
  claude mcp add --scope user --transport http synapse "http://$BIND/mcp" --header "Authorization: Bearer $TOK"
  say "Claude Code (WSL) wired -> Streamable HTTP daemon."
else
  say "claude CLI not found in WSL; skipping Claude Code wiring."
fi

# Codex (WSL): Streamable HTTP -> shared Windows daemon.
CODEX_CFG="$HOME/.codex/config.toml"
if command -v codex >/dev/null 2>&1; then
  codex mcp remove synapse >/dev/null 2>&1 || true
  codex mcp add synapse --url "http://$BIND/mcp" --bearer-token-env-var SYNAPSE_BEARER_TOKEN
  ensure_codex_synapse_policy "$CODEX_CFG" "$BIND"
  say "Codex (WSL) wired -> Streamable HTTP daemon with required=true and default_tools_approval_mode=approve."
elif [ -f "$CODEX_CFG" ]; then
  die "Codex config exists at $CODEX_CFG but codex CLI is not on PATH, so the installer cannot safely replace stale synapse config. Install/repair Codex CLI, then re-run."
else
  say "codex CLI/config not found in WSL; skipping Codex wiring."
fi

# --- 5. Verify the daemon is reachable from WSL -----------------------------
say "Verifying daemon health from WSL"
if curl -fsS -m 5 -H "Authorization: Bearer $TOK" "http://$BIND/health" >/dev/null 2>&1; then
  say "Daemon healthy and reachable from WSL on http://$BIND."
else
  die "Daemon not reachable from WSL on http://$BIND. The Windows daemon may not have started; check %LOCALAPPDATA%\\synapse\\logs\\daemon.log."
fi

say "Done. Restart Claude Code / Codex; call the synapse 'health' tool to confirm."
