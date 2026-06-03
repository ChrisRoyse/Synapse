#!/usr/bin/env python3
"""Faithful MCP-over-stdio probe for synapse-mcp.

Launches the given synapse-mcp binary exactly as an MCP client (Claude Code)
would: as a stdio child speaking newline-delimited JSON-RPC 2.0. Performs the
initialize handshake, then calls the requested tools and prints raw results so
we can do Full State Verification of REAL desktop access (not return-value
trust, not health-only).

Usage:
    python3 fsv_stdio_probe.py <path-to-binary> <tool1> [tool2 ...]
Tool spec: name or name:{json-args}
"""
import json
import os
import subprocess
import sys
import threading
import time

def main():
    if len(sys.argv) < 3:
        print("usage: fsv_stdio_probe.py <binary> <tool[:jsonargs]> ...", file=sys.stderr)
        sys.exit(2)
    binary = sys.argv[1]
    tool_specs = sys.argv[2:]

    # Optional isolation/safety knobs via env so the probe faithfully mirrors
    # how a real client would be configured (isolated DB, hotkey policy).
    launch_args = [binary, "--mode", "stdio"]
    db = os.environ.get("SYNAPSE_PROBE_DB")
    if db:
        launch_args += ["--db", db]
    child_env = dict(os.environ)
    if os.environ.get("SYNAPSE_PROBE_NOHOTKEY") == "1":
        child_env["SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY"] = "1"

    proc = subprocess.Popen(
        launch_args,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=child_env,
    )

    stderr_lines = []
    def drain_stderr():
        for line in proc.stderr:
            stderr_lines.append(line.rstrip("\n"))
    t = threading.Thread(target=drain_stderr, daemon=True)
    t.start()

    def send(obj):
        proc.stdin.write(json.dumps(obj) + "\n")
        proc.stdin.flush()

    def read_response(expect_id, timeout=40):
        """Read lines until we get a JSON-RPC response with matching id."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = proc.stdout.readline()
            if not line:
                time.sleep(0.05)
                continue
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                print(f"[non-json stdout] {line}")
                continue
            if msg.get("id") == expect_id:
                return msg
            # notification or other; print briefly
            if "method" in msg:
                print(f"[server notif] {msg.get('method')}")
        return None

    results = {}
    try:
        # 1. initialize
        send({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "fsv-probe", "version": "0.0.1"},
            },
        })
        init = read_response(1)
        results["initialize"] = init
        if init is None:
            print("FATAL: no initialize response")
        else:
            sv = init.get("result", {}).get("serverInfo", {})
            pv = init.get("result", {}).get("protocolVersion")
            print(f"[init OK] server={sv} protocol={pv}")

        # 2. initialized notification
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        time.sleep(0.3)

        # 3. tool calls
        settle_ms = int(os.environ.get("SYNAPSE_PROBE_SETTLE_MS", "0"))
        next_id = 10
        for spec in tool_specs:
            if settle_ms > 0:
                time.sleep(settle_ms / 1000.0)
            if ":" in spec:
                name, raw = spec.split(":", 1)
                args = json.loads(raw)
            else:
                name, args = spec, {}
            send({
                "jsonrpc": "2.0", "id": next_id, "method": "tools/call",
                "params": {"name": name, "arguments": args},
            })
            resp = read_response(next_id)
            results[f"tool:{name}"] = resp
            print(f"\n========== TOOL CALL: {name} args={args} ==========")
            if resp is None:
                print("  NO RESPONSE (timeout)")
            elif "error" in resp:
                print(f"  ERROR: {json.dumps(resp['error'])}")
            else:
                content = resp.get("result", {}).get("content", [])
                for c in content:
                    if c.get("type") == "text":
                        print(c["text"])
                    else:
                        print(json.dumps(c))
            next_id += 1
    finally:
        try:
            proc.stdin.close()
        except Exception:
            pass
        time.sleep(0.5)
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except Exception:
            proc.kill()

    if stderr_lines:
        print("\n========== SERVER STDERR (last 40) ==========")
        for l in stderr_lines[-40:]:
            print(l)

if __name__ == "__main__":
    main()
