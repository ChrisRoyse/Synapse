# API-model agents (DeepSeek and other OpenAI-compatible providers)

Synapse spawns three kinds of agents:

| Kind | Backed by | Auth |
|------|-----------|------|
| `claude` | Claude Code CLI (Max subscription) | the CLI's own login |
| `codex` | Codex CLI (subscription) | the CLI's own login |
| `local_model` | any **OpenAI-compatible `/v1/chat/completions` endpoint** | bearer API key (env var) |

The `local_model` kind is the substrate for **cloud API models** too — a remote
provider like **DeepSeek** is just an OpenAI-compatible endpoint reached over
https with a bearer key. There is no separate "API model" spawn path; a cloud
provider is a registry row with `allow_non_loopback = true` and an
`api_key_env_var`. (See issue #985, building on the #896 local-model harness.)

Cloud API-model agents get the same Synapse MCP harness as local models: the
spawned `local-agent` strict-loads the live `tools/list`, calls `health`, and
routes tool calls back through the daemon. If the provider cannot accept the
whole tool list directly (DeepSeek currently caps chat-completions tools at 128
functions), the runner exposes `synapse_tool_catalog` and `synapse_tool`. That
routed harness can still call every real Synapse tool from the live catalog,
including file reads/writes, shell/process actions, perception, browser/control
surfaces, dashboard endpoints, and the local-model registry tools.

## The api_key_env_var contract (no secret at rest)

A registry row never stores the API key. It stores the **name** of an
environment variable (`api_key_env_var`, e.g. `DEEPSEEK_API_KEY`). The key value
is read from the environment at two points:

1. **Probe / register** — the daemon reads the env var to authenticate the
   structured tool-call probe against the live endpoint. Registration is refused
   loudly if the var is unset/empty or the endpoint cannot tool-call.
2. **Spawn** — `act_spawn_agent` reads the env var from the daemon's environment
   and **forwards it into the spawned `local-agent` child**. This is required
   because `act_launch` clears the child environment and re-applies only a
   curated allow-list; without the forward, the agent would reach DeepSeek with
   no credentials and fail with HTTP 401 on its first turn. If the row declares
   an `api_key_env_var` the daemon does not have, the spawn is refused at the
   spawn boundary with `MODEL_API_KEY_MISSING` and a remediation hint — never a
   silent mid-run failure.

**Therefore the daemon process must carry the key in its environment.** Use
`scripts/run-daemon-with-secrets.ps1`, which injects the keys from Infisical
(via `infisical run`) so nothing is written to disk:

```powershell
pwsh -File scripts/run-daemon-with-secrets.ps1            # shared daemon on :7700
```

Or set it yourself before launching the daemon (less robust):

```powershell
$env:DEEPSEEK_API_KEY = "<from your secret manager>"
synapse-mcp --mode http --bind 127.0.0.1:7700 --db <path>
```

## Registering DeepSeek

The dashboard offers two DeepSeek presets:

| Registry row | Model | Runtime preset |
|--------------|-------|----------------|
| `deepseek-flash` | `deepseek-v4-flash` | `deepseek_v4_flash_non_thinking` |
| `deepseek-reasoning` | `deepseek-v4-flash` | `deepseek_v4_reasoning` |

DeepSeek's current API docs list `deepseek-chat` and `deepseek-reasoner` as
legacy compatibility aliases scheduled for deprecation on 2026-07-24 15:59 UTC.
Use `deepseek-v4-flash`/`deepseek-v4-pro` for new rows; the model field stays
editable if an operator needs V4 Pro or a short-lived legacy compatibility
check.

### From the dashboard
`POST /dashboard/api-model/register` (the spawn console's "Add API model" form):

```json
{
  "name": "deepseek-flash",
  "base_url": "https://api.deepseek.com",
  "model_id": "deepseek-v4-flash",
  "runtime_preset": "deepseek_v4_flash_non_thinking",
  "api_key_env_var": "DEEPSEEK_API_KEY",
  "context_length": 1000000,
  "max_tools": 128
}
```

For a reasoning agent, use the same endpoint/key and
`"runtime_preset": "deepseek_v4_reasoning"` (default dashboard row name:
`deepseek-reasoning`). The reasoning preset sends DeepSeek thinking mode with
max reasoning effort.

`api_shape` (`open_ai_chat_completions`) and `allow_non_loopback` (`true`) are
fixed server-side. Registration runs the real probe; a row only persists if the
probe is healthy.

### From MCP
`local_model_register` with the same fields plus `allow_non_loopback: true`.

## Spawning a DeepSeek agent

- Dashboard: the spawn console, or `POST /dashboard/local-model-spawn`
  `{ "model_ref": "deepseek-flash", "prompt": "..." }`. The agent's MCP endpoint
  is anchored to the daemon's own bind address automatically.
- MCP: `act_spawn_agent { "kind": "local_model", "model_ref": "deepseek-flash",
  "prompt": "..." }`.

The agent perceives and acts through the full Synapse MCP tool surface and emits
the same telemetry (CF_AGENT_EVENTS, CF_AGENT_TRANSCRIPTS, token/cost) as Claude
and Codex agents, and responds to `agent_interrupt` / `agent_kill` / steering.

## Adding another provider

Any OpenAI-compatible chat-completions endpoint that supports function/tool
calling works: register it with its `base_url`, `model_id`, and an
`api_key_env_var` the daemon has set. A model that cannot emit structured tool
calls is rejected at registration (the probe fails) — never worked around.
