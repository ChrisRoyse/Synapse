mod errors;
pub(crate) mod find;
pub(crate) mod host_transition;
pub(crate) mod hygiene;
mod launchd_service;
mod model;
pub(crate) mod policy;
mod response;
mod routing;
mod setup;
mod storage;
mod telemetry;
pub(crate) mod telemetry_rollup;
mod types;
mod validation;

const FIND_TOOL: &str = "find";
const STORAGE_TOOL: &str = "storage";
const MODEL_TOOL: &str = "model";
const HYGIENE_TOOL: &str = "hygiene";
const SETUP_TOOL: &str = "setup";
const TELEMETRY_TOOL: &str = "telemetry";
const STORAGE_SOT: &str = "storage backend CF metadata + exact row readbacks";
/// Source of truth for the fused-memory half of `find` (#1676): the persisted
/// per-slot search generation and the Base ledger rows its provenance comes from.
const FIND_SIMILAR_SOT: &str = "Calyx vault persisted search generation manifest (per-slot DiskANN/BM25 indexes) + Base rows + Ledger provenance entries";
const MODEL_SOT: &str = "CF_KV local_model_registry/v1 rows + probe evidence rows";
const HYGIENE_SOT: &str = "CF_KV hygiene/flag/v1 rows + physical source rows";
const SETUP_SOT: &str = "%APPDATA%\\synapse\\token.txt + active daemon lifecycle run-current file + %LOCALAPPDATA%\\synapse\\db-daemon\\daemon-run-current.json shared installed-daemon readback + repo source checkout/setup script readback + Codex MCP config + exact macOS launchd gui/<euid>/com.synapse.mcp print output + durable launchd restart manifest + kernel host BootIdentifier + durable shell status files + %LOCALAPPDATA%\\synapse\\host-transitions guard/preflight/override/intent records + configured WSL kernel locks/checkpoints + Windows System Event 1074";
const TELEMETRY_SOT: &str = "native Aster TimeSeries collection syn-telemetry-v3 + sealed CF_AGENT_EVENTS outbox + daemon profile/tool counters";
