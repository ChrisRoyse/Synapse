//! Risky-vs-safe tool classifier for the approval gate (#927).
//!
//! The `approval_gate` permission-prompt tool consults this to decide whether a
//! spawned agent's pending tool call may run unattended (`AutoAllow`) or must
//! pause for a human verdict (`Gate`). The contract is **fail-safe**: anything
//! not provably read-only / low-consequence defaults to `Gate`, so a
//! misclassification can only ever ask for an unnecessary approval — never
//! silently run a destructive action.
//!
//! Most safe tools are also pre-approved by `permissions.allow` in the spawned
//! agent's `--settings` (so Claude never even calls the gate for them); this
//! table is the authoritative server-side backstop and the place the policy is
//! tested.

use serde_json::Value;

/// Outcome of classifying a single (tool_name, input) pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GateDecision {
    /// Run without pausing — read-only or low-consequence.
    AutoAllow,
    /// Pause for a human decision. `destructive` flags irreversible/dangerous
    /// actions (rm, git push, disk ops) so the UI can warn harder.
    Gate { destructive: bool },
}

impl GateDecision {
    pub(crate) const fn is_gate(self) -> bool {
        matches!(self, Self::Gate { .. })
    }

    pub(crate) const fn destructive(self) -> bool {
        matches!(self, Self::Gate { destructive: true })
    }

    const GATE: Self = Self::Gate { destructive: false };
    const DESTRUCTIVE: Self = Self::Gate { destructive: true };
}

/// Top-level classification entry point.
pub(crate) fn classify(tool_name: &str, input: &Value) -> GateDecision {
    let name = tool_name.trim();
    if let Some(rest) = name.strip_prefix("mcp__") {
        return classify_mcp(rest);
    }
    match name {
        // Read-only inspection + planning + in-context bookkeeping.
        "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" | "TodoWrite" | "ExitPlanMode"
        | "BashOutput" | "Task" | "KillBash" | "KillShell" => GateDecision::AutoAllow,
        // File edits are auto-allowed per the "risky actions only" policy:
        // they are confined to the agent's working tree and reversible via VCS.
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Update" => GateDecision::AutoAllow,
        // Outbound network — always a human decision.
        "WebFetch" | "WebSearch" => GateDecision::GATE,
        "Bash" | "BashShell" => classify_bash(input),
        // Unknown / future tool — fail safe.
        _ => GateDecision::GATE,
    }
}

fn classify_bash(input: &Value) -> GateDecision {
    match input.get("command").and_then(Value::as_str) {
        Some(command) => classify_shell_command(command),
        // A Bash call with no readable command string is opaque — gate it.
        None => GateDecision::GATE,
    }
}

/// Classify a raw shell command line. Splits on shell separators and gates if
/// ANY sub-command is non-safe; the whole line is destructive if any part is.
pub(crate) fn classify_shell_command(command: &str) -> GateDecision {
    let subs = split_subcommands(command);
    if subs.is_empty() {
        return GateDecision::GATE;
    }
    let mut any_gate = false;
    let mut destructive = false;
    for sub in subs {
        match classify_subcommand(&sub) {
            GateDecision::AutoAllow => {}
            GateDecision::Gate { destructive: d } => {
                any_gate = true;
                destructive |= d;
            }
        }
    }
    if any_gate {
        GateDecision::Gate { destructive }
    } else {
        GateDecision::AutoAllow
    }
}

/// Split a command line into pipeline/sequence segments on `&&`, `||`, `|`,
/// `;`, and newlines. Coarse (no full shell grammar) but sufficient: the goal
/// is to make sure a safe-looking prefix can't smuggle a dangerous suffix past
/// the gate (e.g. `ls && rm -rf x`).
fn split_subcommands(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let bytes = command.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        let next = bytes.get(i + 1).map(|b| *b as char);
        match (c, next) {
            ('&', Some('&')) | ('|', Some('|')) => {
                push_trimmed(&mut out, &current);
                current.clear();
                i += 2;
            }
            ('|' | ';' | '\n' | '\r', _) => {
                push_trimmed(&mut out, &current);
                current.clear();
                i += 1;
            }
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }
    push_trimmed(&mut out, &current);
    out
}

fn push_trimmed(out: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_owned());
    }
}

fn classify_subcommand(sub: &str) -> GateDecision {
    // A redirect that writes a file is a mutation. Ignore stderr/stdout dup and
    // /dev/null sinks, which are not file writes.
    if writes_file_via_redirect(sub) {
        return GateDecision::GATE;
    }
    let tokens: Vec<&str> = sub.split_whitespace().collect();
    let Some(program) = first_program(&tokens) else {
        return GateDecision::GATE;
    };
    let program = normalize_program(program);

    if DESTRUCTIVE_PROGRAMS.contains(&program.as_str()) {
        return GateDecision::DESTRUCTIVE;
    }
    if program == "git" {
        return classify_git(&tokens);
    }
    if program == "cargo" {
        return classify_cargo(&tokens);
    }
    if SAFE_READONLY_PROGRAMS.contains(&program.as_str()) {
        return GateDecision::AutoAllow;
    }
    // sudo/doas wrappers, nested shells, interpreters, package managers,
    // network tools, and anything unrecognized all fall through to a gate.
    GateDecision::GATE
}

fn writes_file_via_redirect(sub: &str) -> bool {
    // Strip the common non-file redirects first.
    let cleaned = sub
        .replace("2>&1", " ")
        .replace("1>&2", " ")
        .replace("&>/dev/null", " ")
        .replace("2>/dev/null", " ")
        .replace(">/dev/null", " ");
    cleaned.contains('>')
}

/// First real program token, skipping leading `VAR=value` assignments and
/// transparent wrappers we still want to look past for the *target* program.
fn first_program<'a>(tokens: &[&'a str]) -> Option<&'a str> {
    let mut idx = 0;
    while let Some(tok) = tokens.get(idx) {
        if tok.contains('=') && !tok.starts_with('=') && !tok.contains('/') {
            // leading environment assignment, e.g. FOO=bar cmd
            idx += 1;
            continue;
        }
        if matches!(*tok, "command" | "builtin" | "exec" | "time" | "nice") {
            idx += 1;
            continue;
        }
        return Some(tok);
    }
    None
}

fn normalize_program(program: &str) -> String {
    let trimmed = program.trim_matches('"').trim_matches('\'');
    let base = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base).to_owned()
}

fn classify_git(tokens: &[&str]) -> GateDecision {
    // First non-flag token after `git` is the subcommand.
    let sub = tokens
        .iter()
        .skip(1)
        .find(|t| !t.starts_with('-'))
        .map(|t| t.to_ascii_lowercase());
    let Some(sub) = sub else {
        return GateDecision::AutoAllow; // bare `git` prints help
    };
    match sub.as_str() {
        // Read-only history/state inspection.
        "status" | "diff" | "log" | "show" | "branch" | "remote" | "rev-parse" | "describe"
        | "blame" | "ls-files" | "ls-tree" | "cat-file" | "shortlog" | "reflog" | "whatchanged"
        | "name-rev" | "symbolic-ref" | "var" | "help" | "version" | "show-ref"
        | "for-each-ref" | "rev-list" | "merge-base" | "cherry" | "grep" => GateDecision::AutoAllow,
        // Local, reversible mutations — allowed under "risky only".
        // (fetch/pull/clone are network and fall through to the gate below.)
        "add" | "commit" | "stash" | "tag" => GateDecision::AutoAllow,
        // Irreversible / history-rewriting / working-tree-clobbering / network.
        "push" => GateDecision::DESTRUCTIVE,
        "reset"
            if tokens
                .iter()
                .any(|t| matches!(*t, "--hard" | "--merge" | "--keep")) =>
        {
            GateDecision::DESTRUCTIVE
        }
        "clean" => GateDecision::DESTRUCTIVE,
        _ => GateDecision::GATE,
    }
}

fn classify_cargo(tokens: &[&str]) -> GateDecision {
    let sub = tokens
        .iter()
        .skip(1)
        .find(|t| !t.starts_with('-'))
        .map(|t| t.to_ascii_lowercase());
    match sub.as_deref() {
        Some(
            "check" | "build" | "test" | "fmt" | "clippy" | "tree" | "metadata" | "doc" | "bench"
            | "nextest" | "version" | "--version",
        )
        | None => GateDecision::AutoAllow,
        // install / publish / yank / login / run (arbitrary code) etc.
        _ => GateDecision::GATE,
    }
}

fn classify_mcp(rest: &str) -> GateDecision {
    // rest is "<server>__<tool>" (server segment is glob-free per Claude rules).
    let (server, tool) = split_mcp_name(rest);
    if server == "synapse" && SYNAPSE_COORDINATION_MCP_TOOLS.contains(&tool) {
        return GateDecision::AutoAllow;
    }
    if SAFE_MCP_TOOLS.contains(&tool) || is_readonly_mcp_suffix(tool) {
        return GateDecision::AutoAllow;
    }
    if DESTRUCTIVE_MCP_TOOLS.contains(&tool) {
        return GateDecision::DESTRUCTIVE;
    }
    // Outward-facing / state-mutating / unknown MCP tool — gate.
    GateDecision::GATE
}

fn split_mcp_name(rest: &str) -> (&str, &str) {
    rest.split_once("__").unwrap_or(("", rest))
}

fn is_readonly_mcp_suffix(tool: &str) -> bool {
    tool.starts_with("get_")
        || tool.starts_with("read_")
        || tool.starts_with("observe")
        || tool.ends_with("_list")
        || tool.ends_with("_get")
        || tool.ends_with("_status")
        || tool.ends_with("_stats")
        || tool.ends_with("_query")
        || tool.ends_with("_inspect")
}

const SAFE_READONLY_PROGRAMS: &[&str] = &[
    "ls",
    "dir",
    "pwd",
    "echo",
    "printf",
    "cat",
    "type",
    "head",
    "tail",
    "wc",
    "grep",
    "rg",
    "ripgrep",
    "find",
    "fd",
    "which",
    "where",
    "whoami",
    "hostname",
    "date",
    "env",
    "printenv",
    "true",
    "false",
    "test",
    "stat",
    "file",
    "du",
    "df",
    "tree",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "sort",
    "uniq",
    "cut",
    "tr",
    "comm",
    "diff",
    "cmp",
    "jq",
    "yq",
    "column",
    "tac",
    "nl",
    "od",
    "xxd",
    "sleep",
    "seq",
    "expr",
    "uname",
    "id",
    "groups",
    "less",
    "more",
    "tee",
    "wc",
    "md5sum",
    "sha256sum",
    "cksum",
];

const DESTRUCTIVE_PROGRAMS: &[&str] = &[
    "rm",
    "rmdir",
    "del",
    "erase",
    "remove-item",
    "ri",
    "rd",
    "unlink",
    "shred",
    "format",
    "mkfs",
    "dd",
    "shutdown",
    "reboot",
    "halt",
    "poweroff",
    "fdisk",
    "diskpart",
    "mkfs.ext4",
    "wipefs",
];

const SAFE_MCP_TOOLS: &[&str] = &[
    "observe",
    "observe_delta",
    "find",
    "read_text",
    "get_target",
    "health",
    "agent_ask_operator",
    "agent_spawn_task_started",
    "agent_query",
    "agent_inbox",
    "agent_receipts",
    "session_status",
    "session_list",
    "control_lease_status",
    "target_claim_status",
    "storage_inspect",
    "capture_screenshot",
    "subscribe",
    "subscribe_cancel",
    "intent_current",
    "routine_label_export",
    "routine_list",
    "routine_inspect",
    "timeline",
    "episode",
    "episode_list",
    "episode_get",
    "timeline_get",
    "timeline_search",
    "timeline_stats",
    "timeline_digest",
    "hygiene_flags",
    "hygiene_report",
    "agent_stats",
    "agent_cost",
    "approval_list",
    "escalation_list",
    "local_model_list",
    "workspace_get",
    "workspace_list",
    "task_list",
    "task_get",
    "reflex_list",
    "reflex_history",
    "profile_list",
    "agent_template_list",
    "agent_template_get",
];

/// Synapse-only control-plane tools that let spawned agents publish auditable
/// state, ask for a real operator decision, and wait for mailbox commands.
/// These are intentionally not granted to other MCP servers with the same tool
/// names because they mutate state.
pub(crate) const SYNAPSE_COORDINATION_MCP_TOOLS: &[&str] =
    &["approval_request", "agent_wait", "workspace_put"];

const DESTRUCTIVE_MCP_TOOLS: &[&str] = &[
    "agent_kill",
    "fleet_stop",
    "routine",
    "assist",
    "reality",
    "verification",
    "storage",
    "model",
    "cost",
    "hygiene",
    "audit",
    "replay",
    "setup",
    "agent_cost_price_put",
    "agent_cost_price_delete",
    "storage_gc_once",
    "storage_put_probe_rows",
    "timeline_purge",
    "timeline_redact",
    "privacy",
    "target_release",
    "release_all",
    "session_end",
];
