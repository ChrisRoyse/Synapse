//! Durable, versioned agent-spawn templates (#909).
//!
//! A spawn template is the **config-time identity** of an agent: its kind,
//! model, prompt template with typed parameter slots, working dir, and target
//! binding — named and versioned as one unit. `act_spawn_agent` renders a
//! concrete spawn from a template in a single atomic pass and records the exact
//! `(template_id, version, config_hash)` it used, so every run is reproducible
//! and the fleet is auditable (the "agent factory" pattern: an instance is never
//! assembled piecemeal and always carries the template version it was rendered
//! from).
//!
//! Storage is the daemon-owned `CF_KV` handle, mirroring the mailbox (#908):
//! - a **current pointer** row per template id holds the latest version, and
//! - an **immutable version snapshot** row per `(id, version)` lets a spawn's
//!   recorded version resolve back to the exact config that produced it, even
//!   after the template is later edited or deleted.
//!
//! Editing a template never mutates a row in place: each `agent_template_put`
//! writes a fresh immutable snapshot and atomically advances the pointer. A
//! delete removes the pointer (so new spawns from it fail loudly) while keeping
//! the version snapshots that past runs reference.

use std::collections::{BTreeMap, BTreeSet};

use rmcp::{RoleServer, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use synapse_core::error_codes;
use synapse_storage::{Db, cf};

use super::{
    ActSpawnAgentCli, ActSpawnAgentTarget, ErrorData, Json, Parameters, SynapseService, mcp_error,
    session_registry::unix_time_ms_now, tool, tool_router,
};

/// CF_KV key namespace for template rows. The version is in the prefix so a
/// future on-disk format change is a clean re-key, never an in-place migration.
const TEMPLATE_NAMESPACE: &str = "agent-template/v1";
/// Schema version stamped onto every stored template row.
const TEMPLATE_SCHEMA_VERSION: u32 = 1;

const MAX_TEMPLATE_ID_CHARS: usize = 200;
const MAX_TEMPLATE_NAME_CHARS: usize = 200;
const MAX_REQUIRED_PARAMS: usize = 64;
const MAX_PARAM_NAME_CHARS: usize = 128;
/// Matches the spawn prompt cap (`MAX_AGENT_SPAWN_PROMPT_BYTES`); the template
/// prompt and every rendered prompt are held to the same limit so a template can
/// never produce a prompt the spawn would reject.
const MAX_PROMPT_TEMPLATE_BYTES: usize = 128 * 1024;
const MAX_MODEL_BYTES: usize = 256;
/// Per-parameter value cap; the *rendered* prompt is additionally capped, so this
/// only bounds a single substitution.
const MAX_PARAM_VALUE_BYTES: usize = 16 * 1024;
const MAX_LIST_TEMPLATES: usize = 1000;

/// The two agent CLIs the spawner supports, as the template's `agent_kind`.
const SUPPORTED_AGENT_KINDS: [&str; 2] = ["claude", "codex"];

/// The durable, versioned template record. Field order is fixed so the canonical
/// JSON used for `config_hash` is deterministic.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnTemplate {
    pub schema_version: u32,
    pub template_id: String,
    /// Monotonically increasing per template id; bumped on every edit. A spawn
    /// records the version it rendered so the run is reproducible.
    pub version: u32,
    pub name: String,
    /// `claude` or `codex`. Validated on write — unknown kinds are rejected
    /// loudly, never silently coerced.
    pub agent_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Prompt with `${slot}` placeholders. Every placeholder must be declared in
    /// `required_params` and vice-versa, so a template is never internally
    /// inconsistent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
    pub required_params: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ActSpawnAgentTarget>,
    /// sha256 of the canonical config (everything above `config_hash` except the
    /// volatile `version`/timestamps). Two templates with the same hash render
    /// identical spawns — the exact-provenance anchor recorded on each run.
    pub config_hash: String,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

/// The immutable config inputs an operator supplies; hashing this gives
/// `config_hash`. Kept as a distinct struct so the hash is independent of
/// version/timestamps and reorderings of the public record.
#[derive(Serialize)]
struct CanonicalTemplateConfig<'a> {
    name: &'a str,
    agent_kind: &'a str,
    model: &'a Option<String>,
    prompt_template: &'a Option<String>,
    required_params: &'a [String],
    working_dir: &'a Option<String>,
    target: &'a Option<ActSpawnAgentTarget>,
}

/// Provenance stamped onto a spawn rendered from a template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TemplateProvenance {
    pub template_id: String,
    pub version: u32,
    pub config_hash: String,
}

/// The concrete spawn inputs a template renders into. The spawn path applies
/// these over its existing param struct; the template never reaches the spawner
/// directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenderedSpawn {
    pub cli: ActSpawnAgentCli,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub working_dir: Option<String>,
    pub target: Option<ActSpawnAgentTarget>,
    pub provenance: TemplateProvenance,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplatePutParams {
    /// Stable template identity. Reusing an existing id edits it (a new version
    /// snapshot is written and the current pointer advanced). `[a-z0-9._-]`.
    pub template_id: String,
    /// Human-readable label surfaced in fleet/spawn UIs.
    pub name: String,
    /// `claude` or `codex`. Any other value is rejected.
    pub agent_kind: String,
    #[serde(default)]
    #[schemars(default)]
    pub model: Option<String>,
    /// Prompt body with `${slot}` placeholders. Each placeholder must appear in
    /// `required_params` exactly once and vice-versa.
    #[serde(default)]
    #[schemars(default)]
    pub prompt_template: Option<String>,
    /// Declared parameter contract: the slot names a spawn must supply.
    #[serde(default)]
    #[schemars(default)]
    pub required_params: Vec<String>,
    #[serde(default)]
    #[schemars(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub target: Option<ActSpawnAgentTarget>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplatePutResponse {
    pub ok: bool,
    pub template: SpawnTemplate,
    /// True when this put created a brand-new template id (version 1).
    pub created: bool,
    /// Physical CF_KV rows written, for state verification.
    pub written_rows: Vec<TemplateRowReadback>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateRowReadback {
    pub cf_name: String,
    pub row_key: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplateGetParams {
    pub template_id: String,
    /// Omit for the current version; set to resolve a specific historical
    /// version snapshot (what a past run recorded).
    #[serde(default)]
    #[schemars(default)]
    pub version: Option<u32>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplateGetResponse {
    pub ok: bool,
    pub template: SpawnTemplate,
    pub row_key: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplateListParams {
    #[serde(default = "default_max_list")]
    #[schemars(default = "default_max_list", range(min = 1, max = 1000))]
    pub max: usize,
}

fn default_max_list() -> usize {
    MAX_LIST_TEMPLATES
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplateListResponse {
    pub ok: bool,
    pub count: usize,
    pub templates: Vec<SpawnTemplate>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplateDeleteParams {
    pub template_id: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplateDeleteResponse {
    pub ok: bool,
    pub template_id: String,
    /// The version the deleted current pointer held, for the audit trail.
    pub deleted_version: u32,
    pub deleted_row_key: String,
    /// Immutable version snapshots are intentionally retained so past runs stay
    /// reproducible; this is how many remain.
    pub retained_version_snapshots: usize,
}

// ---- key encoding ---------------------------------------------------------

fn current_pointer_key(template_id: &str) -> String {
    format!("{TEMPLATE_NAMESPACE}/cur/{template_id}")
}

fn current_pointer_prefix() -> String {
    format!("{TEMPLATE_NAMESPACE}/cur/")
}

fn version_snapshot_key(template_id: &str, version: u32) -> String {
    // Zero-padded so a lexical prefix scan returns versions in numeric order.
    format!("{TEMPLATE_NAMESPACE}/ver/{template_id}/{version:010}")
}

fn version_snapshot_prefix(template_id: &str) -> String {
    format!("{TEMPLATE_NAMESPACE}/ver/{template_id}/")
}

// ---- validation & rendering (pure, unit-tested) ---------------------------

fn params_error(message: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, message.into())
}

fn is_kebab_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'))
}

fn is_param_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

/// Extracts the set of `${name}` placeholders from a prompt template. Returns an
/// error on a malformed placeholder (`${`, unterminated, or non-identifier
/// body) so a template can never carry a slot the renderer would silently miss.
fn extract_placeholders(prompt: &str) -> Result<BTreeSet<String>, ErrorData> {
    let mut placeholders = BTreeSet::new();
    let bytes = prompt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let Some(rel_end) = prompt[start..].find('}') else {
                return Err(params_error(
                    "agent_template prompt_template has an unterminated ${...} placeholder",
                ));
            };
            let name = &prompt[start..start + rel_end];
            if !is_param_name(name) {
                return Err(params_error(format!(
                    "agent_template prompt_template placeholder ${{{name}}} must be [a-z0-9_]+"
                )));
            }
            placeholders.insert(name.to_owned());
            i = start + rel_end + 1;
        } else {
            i += 1;
        }
    }
    Ok(placeholders)
}

/// Renders `${name}` placeholders from `values`. Caller has already verified the
/// key set matches; an absent key here is an internal error, surfaced loudly.
fn render_prompt(prompt: &str, values: &BTreeMap<String, String>) -> Result<String, ErrorData> {
    let mut out = String::with_capacity(prompt.len());
    let bytes = prompt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let rel_end = prompt[start..].find('}').ok_or_else(|| {
                params_error("agent_template prompt_template has an unterminated ${...} placeholder")
            })?;
            let name = &prompt[start..start + rel_end];
            let value = values.get(name).ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("agent_template render missing already-validated slot ${{{name}}}"),
                )
            })?;
            out.push_str(value);
            i = start + rel_end + 1;
        } else {
            // Copy this byte as part of a char; push the whole char to keep UTF-8
            // boundaries intact.
            let ch = prompt[i..].chars().next().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "agent_template render walked off a UTF-8 boundary",
                )
            })?;
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

fn validate_agent_kind(agent_kind: &str) -> Result<ActSpawnAgentCli, ErrorData> {
    match agent_kind {
        "claude" => Ok(ActSpawnAgentCli::Claude),
        "codex" => Ok(ActSpawnAgentCli::Codex),
        other => Err(params_error(format!(
            "agent_template agent_kind {other:?} is not supported; must be one of {SUPPORTED_AGENT_KINDS:?}"
        ))),
    }
}

fn validate_model(model: &str) -> Result<(), ErrorData> {
    if model.trim().is_empty() {
        return Err(params_error(
            "agent_template model must not be empty when provided",
        ));
    }
    if model.len() > MAX_MODEL_BYTES {
        return Err(params_error(format!(
            "agent_template model must be <= {MAX_MODEL_BYTES} bytes"
        )));
    }
    if model.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(params_error(
            "agent_template model must not contain whitespace or control characters",
        ));
    }
    Ok(())
}

fn validate_target(target: &ActSpawnAgentTarget) -> Result<(), ErrorData> {
    if let ActSpawnAgentTarget::Cdp { cdp_target_id, .. } = target {
        if cdp_target_id.trim().is_empty()
            || !cdp_target_id.chars().all(|ch| ('!'..='~').contains(&ch))
        {
            return Err(params_error(
                "agent_template target cdp_target_id must contain only visible ASCII characters",
            ));
        }
    }
    Ok(())
}

/// Validates an operator's put params and returns the validated CLI plus the
/// normalized required-params set. Internal consistency (placeholders ==
/// required_params) is enforced here so a stored template is always renderable.
fn validate_put_params(params: &AgentTemplatePutParams) -> Result<(), ErrorData> {
    if !is_kebab_id(&params.template_id) || params.template_id.len() > MAX_TEMPLATE_ID_CHARS {
        return Err(params_error(format!(
            "agent_template template_id must be non-empty [a-z0-9._-] and <= {MAX_TEMPLATE_ID_CHARS} chars"
        )));
    }
    if params.name.trim().is_empty() || params.name.len() > MAX_TEMPLATE_NAME_CHARS {
        return Err(params_error(format!(
            "agent_template name must be non-empty and <= {MAX_TEMPLATE_NAME_CHARS} chars"
        )));
    }
    validate_agent_kind(&params.agent_kind)?;
    if let Some(model) = &params.model {
        validate_model(model)?;
    }
    if params.required_params.len() > MAX_REQUIRED_PARAMS {
        return Err(params_error(format!(
            "agent_template required_params must be <= {MAX_REQUIRED_PARAMS} entries"
        )));
    }
    let mut declared = BTreeSet::new();
    for name in &params.required_params {
        if !is_param_name(name) || name.len() > MAX_PARAM_NAME_CHARS {
            return Err(params_error(format!(
                "agent_template required_params entry {name:?} must be non-empty [a-z0-9_]+ and <= {MAX_PARAM_NAME_CHARS} chars"
            )));
        }
        if !declared.insert(name.clone()) {
            return Err(params_error(format!(
                "agent_template required_params has a duplicate entry {name:?}"
            )));
        }
    }
    match &params.prompt_template {
        Some(prompt) => {
            if prompt.len() > MAX_PROMPT_TEMPLATE_BYTES {
                return Err(params_error(format!(
                    "agent_template prompt_template must be <= {MAX_PROMPT_TEMPLATE_BYTES} bytes"
                )));
            }
            let placeholders = extract_placeholders(prompt)?;
            if placeholders != declared {
                let missing_decl: Vec<_> = placeholders.difference(&declared).cloned().collect();
                let unused_decl: Vec<_> = declared.difference(&placeholders).cloned().collect();
                return Err(params_error(format!(
                    "agent_template prompt_template placeholders must exactly match required_params; \
                     placeholders not declared: {missing_decl:?}; declared but absent from prompt: {unused_decl:?}"
                )));
            }
        }
        None => {
            if !declared.is_empty() {
                return Err(params_error(
                    "agent_template declares required_params but has no prompt_template to substitute them into",
                ));
            }
        }
    }
    if let Some(working_dir) = &params.working_dir {
        if working_dir.trim().is_empty() {
            return Err(params_error(
                "agent_template working_dir must not be empty when provided",
            ));
        }
    }
    if let Some(target) = &params.target {
        validate_target(target)?;
    }
    Ok(())
}

/// Lowercase-hex sha256 of `bytes` (`Sha256::digest` returns a `GenericArray`
/// that does not implement `LowerHex`, so we encode the bytes ourselves).
fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn config_hash(config: &CanonicalTemplateConfig<'_>) -> String {
    // serde_json over a fixed-field-order struct is deterministic.
    let bytes = serde_json::to_vec(config).unwrap_or_default();
    sha256_hex(&bytes)
}

/// Builds the stored record for a put, given the prior version (if editing).
fn build_template(
    params: &AgentTemplatePutParams,
    prior: Option<&SpawnTemplate>,
    now_unix_ms: u64,
) -> SpawnTemplate {
    let canonical = CanonicalTemplateConfig {
        name: &params.name,
        agent_kind: &params.agent_kind,
        model: &params.model,
        prompt_template: &params.prompt_template,
        required_params: &params.required_params,
        working_dir: &params.working_dir,
        target: &params.target,
    };
    let hash = config_hash(&canonical);
    let version = prior.map_or(1, |p| p.version + 1);
    let created = prior.map_or(now_unix_ms, |p| p.created_unix_ms);
    SpawnTemplate {
        schema_version: TEMPLATE_SCHEMA_VERSION,
        template_id: params.template_id.clone(),
        version,
        name: params.name.clone(),
        agent_kind: params.agent_kind.clone(),
        model: params.model.clone(),
        prompt_template: params.prompt_template.clone(),
        required_params: params.required_params.clone(),
        working_dir: params.working_dir.clone(),
        target: params.target.clone(),
        config_hash: hash,
        created_unix_ms: created,
        updated_unix_ms: now_unix_ms,
    }
}

/// Renders a concrete spawn from a stored template and caller-supplied params.
/// The param key set must equal the template's `required_params` exactly:
/// missing or unknown keys are rejected loudly (no silent defaults).
pub(crate) fn render_spawn(
    template: &SpawnTemplate,
    provided_params: &BTreeMap<String, String>,
) -> Result<RenderedSpawn, ErrorData> {
    let cli = validate_agent_kind(&template.agent_kind)?;
    let declared: BTreeSet<&String> = template.required_params.iter().collect();
    let provided: BTreeSet<&String> = provided_params.keys().collect();

    let missing: Vec<String> = declared.difference(&provided).map(|s| (*s).clone()).collect();
    if !missing.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "act_spawn_agent template {:?} (v{}) is missing required template_params: {missing:?}",
                template.template_id, template.version
            ),
        ));
    }
    let unknown: Vec<String> = provided.difference(&declared).map(|s| (*s).clone()).collect();
    if !unknown.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "act_spawn_agent template {:?} (v{}) was given unknown template_params not in its contract: {unknown:?}",
                template.template_id, template.version
            ),
        ));
    }
    for (name, value) in provided_params {
        if value.len() > MAX_PARAM_VALUE_BYTES {
            return Err(params_error(format!(
                "act_spawn_agent template_params value for {name:?} must be <= {MAX_PARAM_VALUE_BYTES} bytes"
            )));
        }
        if value.contains('\0') {
            return Err(params_error(format!(
                "act_spawn_agent template_params value for {name:?} must not contain NUL"
            )));
        }
    }

    let prompt = match &template.prompt_template {
        Some(prompt_template) => {
            let rendered = render_prompt(prompt_template, provided_params)?;
            if rendered.len() > MAX_PROMPT_TEMPLATE_BYTES {
                return Err(params_error(format!(
                    "act_spawn_agent rendered prompt from template {:?} is {} bytes, over the {MAX_PROMPT_TEMPLATE_BYTES} cap",
                    template.template_id,
                    rendered.len()
                )));
            }
            Some(rendered)
        }
        None => None,
    };

    Ok(RenderedSpawn {
        cli,
        model: template.model.clone(),
        prompt,
        working_dir: template.working_dir.clone(),
        target: template.target.clone(),
        provenance: TemplateProvenance {
            template_id: template.template_id.clone(),
            version: template.version,
            config_hash: template.config_hash.clone(),
        },
    })
}

// ---- storage access -------------------------------------------------------

fn encode_template(template: &SpawnTemplate) -> Result<Vec<u8>, ErrorData> {
    serde_json::to_vec(template).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("agent_template failed to encode template row: {error}"),
        )
    })
}

fn decode_template(row_key: &str, bytes: &[u8]) -> Result<SpawnTemplate, ErrorData> {
    serde_json::from_slice(bytes).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("agent_template row {row_key} is corrupt and could not be decoded: {error}"),
        )
    })
}

fn row_readback(row_key: &str, bytes: &[u8]) -> TemplateRowReadback {
    TemplateRowReadback {
        cf_name: cf::CF_KV.to_owned(),
        row_key: row_key.to_owned(),
        value_len_bytes: bytes.len() as u64,
        value_sha256: sha256_hex(bytes),
    }
}

impl SynapseService {
    fn agent_template_db(&self) -> Result<std::sync::Arc<Db>, ErrorData> {
        let state = self.m3_state_handle();
        let mut guard = state.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while opening agent template storage",
            )
        })?;
        guard
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    /// Reads the current pointer row for a template id, if any.
    fn read_current_template(
        db: &Db,
        template_id: &str,
    ) -> Result<Option<SpawnTemplate>, ErrorData> {
        let key = current_pointer_key(template_id);
        let rows = db.scan_cf_prefix(cf::CF_KV, key.as_bytes()).map_err(|error| {
            mcp_error(
                error.code(),
                format!("agent_template failed to read current pointer {key}: {error}"),
            )
        })?;
        for (raw_key, raw_value) in rows {
            // scan_cf_prefix is a prefix scan; only the exact key is the pointer.
            if raw_key == key.as_bytes() {
                return Ok(Some(decode_template(&key, &raw_value)?));
            }
        }
        Ok(None)
    }

    fn agent_template_put_impl(
        &self,
        params: AgentTemplatePutParams,
    ) -> Result<AgentTemplatePutResponse, ErrorData> {
        validate_put_params(&params)?;
        let db = self.agent_template_db()?;
        let prior = Self::read_current_template(&db, &params.template_id)?;
        let created = prior.is_none();
        let now = unix_time_ms_now();
        let template = build_template(&params, prior.as_ref(), now);

        let encoded = encode_template(&template)?;
        let pointer_key = current_pointer_key(&template.template_id);
        let snapshot_key = version_snapshot_key(&template.template_id, template.version);

        // Atomic: the immutable version snapshot and the advanced pointer land in
        // one batch, so a reader never sees a pointer to a missing snapshot.
        db.put_batch(
            cf::CF_KV,
            [
                (snapshot_key.clone().into_bytes(), encoded.clone()),
                (pointer_key.clone().into_bytes(), encoded.clone()),
            ],
        )
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "agent_template failed to persist template {:?} v{}: {error}",
                    template.template_id, template.version
                ),
            )
        })?;
        // Templates are durable config, not a high-throughput stream: flush the
        // storage batcher so the rows are physically on disk (and visible to the
        // RocksDB-backed read path) before this put returns OK. Without this a
        // subsequent get/list could miss the just-written row while it sat in the
        // in-memory pending batch (read-after-write would be broken).
        db.flush().map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "agent_template persisted template {:?} v{} but failed to flush it to disk: {error}",
                    template.template_id, template.version
                ),
            )
        })?;

        tracing::info!(
            code = "AGENT_TEMPLATE_PUT",
            template_id = %template.template_id,
            version = template.version,
            created,
            config_hash = %template.config_hash,
            "readback=agent_templates edge=put"
        );

        Ok(AgentTemplatePutResponse {
            ok: true,
            written_rows: vec![
                row_readback(&snapshot_key, &encoded),
                row_readback(&pointer_key, &encoded),
            ],
            created,
            template,
        })
    }

    fn agent_template_get_impl(
        &self,
        params: AgentTemplateGetParams,
    ) -> Result<AgentTemplateGetResponse, ErrorData> {
        if !is_kebab_id(&params.template_id) {
            return Err(params_error(
                "agent_template_get template_id must be non-empty [a-z0-9._-]",
            ));
        }
        let db = self.agent_template_db()?;
        let (row_key, template) = match params.version {
            None => {
                let key = current_pointer_key(&params.template_id);
                let template =
                    Self::read_current_template(&db, &params.template_id)?.ok_or_else(|| {
                        template_not_found(&params.template_id, None)
                    })?;
                (key, template)
            }
            Some(version) => {
                let key = version_snapshot_key(&params.template_id, version);
                let rows =
                    db.scan_cf_prefix(cf::CF_KV, key.as_bytes()).map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!("agent_template failed to read version snapshot {key}: {error}"),
                        )
                    })?;
                let found = rows
                    .into_iter()
                    .find(|(raw_key, _)| raw_key == key.as_bytes());
                match found {
                    Some((_, raw_value)) => (key.clone(), decode_template(&key, &raw_value)?),
                    None => return Err(template_not_found(&params.template_id, Some(version))),
                }
            }
        };
        Ok(AgentTemplateGetResponse {
            ok: true,
            template,
            row_key,
        })
    }

    fn agent_template_list_impl(
        &self,
        params: AgentTemplateListParams,
    ) -> Result<AgentTemplateListResponse, ErrorData> {
        let db = self.agent_template_db()?;
        let prefix = current_pointer_prefix();
        let rows = db
            .scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_template failed to list current pointers: {error}"),
                )
            })?;
        let mut templates = Vec::new();
        for (raw_key, raw_value) in rows {
            let key = String::from_utf8_lossy(&raw_key).into_owned();
            templates.push(decode_template(&key, &raw_value)?);
            if templates.len() >= params.max {
                break;
            }
        }
        templates.sort_by(|a, b| a.template_id.cmp(&b.template_id));
        Ok(AgentTemplateListResponse {
            ok: true,
            count: templates.len(),
            templates,
        })
    }

    fn agent_template_delete_impl(
        &self,
        params: AgentTemplateDeleteParams,
    ) -> Result<AgentTemplateDeleteResponse, ErrorData> {
        if !is_kebab_id(&params.template_id) {
            return Err(params_error(
                "agent_template_delete template_id must be non-empty [a-z0-9._-]",
            ));
        }
        let db = self.agent_template_db()?;
        let current = Self::read_current_template(&db, &params.template_id)?
            .ok_or_else(|| template_not_found(&params.template_id, None))?;
        let pointer_key = current_pointer_key(&params.template_id);
        db.delete_batch(cf::CF_KV, [pointer_key.clone().into_bytes()])
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_template failed to delete pointer {pointer_key}: {error}"),
                )
            })?;
        // Flush so the deletion is durable and immediately reflected on the
        // RocksDB read path before this returns (read-after-write).
        db.flush().map_err(|error| {
            mcp_error(
                error.code(),
                format!("agent_template deleted pointer {pointer_key} but failed to flush to disk: {error}"),
            )
        })?;
        // Count retained immutable snapshots (kept so past runs stay reproducible).
        let snapshot_prefix = version_snapshot_prefix(&params.template_id);
        let retained = db
            .scan_cf_prefix(cf::CF_KV, snapshot_prefix.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_template failed to count retained snapshots: {error}"),
                )
            })?
            .len();

        tracing::info!(
            code = "AGENT_TEMPLATE_DELETE",
            template_id = %params.template_id,
            deleted_version = current.version,
            retained_version_snapshots = retained,
            "readback=agent_templates edge=delete"
        );

        Ok(AgentTemplateDeleteResponse {
            ok: true,
            template_id: params.template_id,
            deleted_version: current.version,
            deleted_row_key: pointer_key,
            retained_version_snapshots: retained,
        })
    }

    /// Resolves a template id + version to a rendered spawn for `act_spawn_agent`
    /// (#909). Always reads the **exact recorded version snapshot** so a spawn is
    /// rendered from the config pinned at request time, never a drifted "latest".
    /// When `version` is `None` it pins the current version and records it.
    pub(crate) fn resolve_spawn_template(
        &self,
        template_id: &str,
        version: Option<u32>,
        provided_params: &BTreeMap<String, String>,
    ) -> Result<RenderedSpawn, ErrorData> {
        if !is_kebab_id(template_id) {
            return Err(params_error(
                "act_spawn_agent template_id must be non-empty [a-z0-9._-]",
            ));
        }
        let db = self.agent_template_db()?;
        let template = match version {
            None => Self::read_current_template(&db, template_id)?
                .ok_or_else(|| template_not_found(template_id, None))?,
            Some(version) => {
                let key = version_snapshot_key(template_id, version);
                let rows =
                    db.scan_cf_prefix(cf::CF_KV, key.as_bytes()).map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!("agent_template failed to read version snapshot {key}: {error}"),
                        )
                    })?;
                rows.into_iter()
                    .find(|(raw_key, _)| raw_key == key.as_bytes())
                    .map(|(_, raw_value)| decode_template(&key, &raw_value))
                    .transpose()?
                    .ok_or_else(|| template_not_found(template_id, Some(version)))?
            }
        };
        render_spawn(&template, provided_params)
    }
}

fn template_not_found(template_id: &str, version: Option<u32>) -> ErrorData {
    let detail = match version {
        Some(v) => format!("template {template_id:?} has no version {v}"),
        None => format!("no current template with id {template_id:?}"),
    };
    mcp_error(
        error_codes::AGENT_TEMPLATE_NOT_FOUND,
        format!("agent_template not found: {detail}"),
    )
}

#[tool_router(router = agent_template_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Create or edit a durable, versioned agent-spawn template (kind/model/prompt-template/params/working-dir/target). Reusing a template_id writes a new immutable version snapshot and advances the current pointer; prompt ${slot} placeholders must exactly match required_params. act_spawn_agent renders a spawn from a template by id and records the version used."
    )]
    pub async fn agent_template_put(
        &self,
        params: Parameters<AgentTemplatePutParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentTemplatePutResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_template_put",
            template_id = %params.0.template_id,
            "tool.invocation kind=agent_template_put"
        );
        self.agent_template_put_impl(params.0).map(Json)
    }

    #[tool(
        description = "Read one agent-spawn template by id. Omit version for the current version, or pass a version to resolve a historical immutable snapshot (what a past run recorded). Errors AGENT_TEMPLATE_NOT_FOUND if absent."
    )]
    pub async fn agent_template_get(
        &self,
        params: Parameters<AgentTemplateGetParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentTemplateGetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_template_get",
            template_id = %params.0.template_id,
            "tool.invocation kind=agent_template_get"
        );
        self.agent_template_get_impl(params.0).map(Json)
    }

    #[tool(
        description = "List current agent-spawn templates (one row per template id, latest version), sorted by id."
    )]
    pub async fn agent_template_list(
        &self,
        params: Parameters<AgentTemplateListParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentTemplateListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_template_list",
            "tool.invocation kind=agent_template_list"
        );
        self.agent_template_list_impl(params.0).map(Json)
    }

    #[tool(
        description = "Delete an agent-spawn template's current pointer so new spawns from it fail loudly. Immutable version snapshots are retained so past runs stay reproducible. Errors AGENT_TEMPLATE_NOT_FOUND if no current template."
    )]
    pub async fn agent_template_delete(
        &self,
        params: Parameters<AgentTemplateDeleteParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentTemplateDeleteResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_template_delete",
            template_id = %params.0.template_id,
            "tool.invocation kind=agent_template_delete"
        );
        self.agent_template_delete_impl(params.0).map(Json)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    /// Extracts the string error code stuffed into `ErrorData.data` by `mcp_error`.
    fn error_code_str(err: &ErrorData) -> &str {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("<no code>")
    }

    fn base_put(id: &str) -> AgentTemplatePutParams {
        AgentTemplatePutParams {
            template_id: id.to_owned(),
            name: "Code reviewer".to_owned(),
            agent_kind: "claude".to_owned(),
            model: Some("claude-opus-4-8".to_owned()),
            prompt_template: Some("Review ${repo} for ${focus}.".to_owned()),
            required_params: vec!["repo".to_owned(), "focus".to_owned()],
            working_dir: Some(r"C:\code\Synapse".to_owned()),
            target: None,
        }
    }

    #[test]
    fn extract_placeholders_collects_unique_slots() {
        let set = extract_placeholders("a ${x} b ${y} c ${x}").expect("parse");
        assert_eq!(
            set,
            ["x", "y"].into_iter().map(str::to_owned).collect()
        );
    }

    #[test]
    fn extract_placeholders_rejects_unterminated() {
        let err = extract_placeholders("hello ${oops").expect_err("must reject");
        assert_eq!(error_code_str(&err), error_codes::TOOL_PARAMS_INVALID);
        assert!(err.message.contains("unterminated"), "{}", err.message);
    }

    #[test]
    fn extract_placeholders_rejects_bad_identifier() {
        let err = extract_placeholders("hi ${Bad-Name}").expect_err("must reject");
        assert!(err.message.contains("[a-z0-9_]+"), "{}", err.message);
    }

    #[test]
    fn validate_put_rejects_unknown_agent_kind() {
        let mut params = base_put("rev");
        params.agent_kind = "gpt".to_owned();
        let err = validate_put_params(&params).expect_err("must reject");
        assert!(err.message.contains("not supported"), "{}", err.message);
    }

    #[test]
    fn validate_put_rejects_placeholder_mismatch() {
        let mut params = base_put("rev");
        params.required_params = vec!["repo".to_owned()]; // prompt also has ${focus}
        let err = validate_put_params(&params).expect_err("must reject");
        assert!(
            err.message.contains("placeholders not declared: [\"focus\"]"),
            "{}",
            err.message
        );
    }

    #[test]
    fn validate_put_rejects_declared_param_with_no_prompt() {
        let mut params = base_put("rev");
        params.prompt_template = None;
        let err = validate_put_params(&params).expect_err("must reject");
        assert!(err.message.contains("no prompt_template"), "{}", err.message);
    }

    #[test]
    fn validate_put_rejects_duplicate_required_param() {
        let mut params = base_put("rev");
        params.prompt_template = Some("${repo}".to_owned());
        params.required_params = vec!["repo".to_owned(), "repo".to_owned()];
        let err = validate_put_params(&params).expect_err("must reject");
        assert!(err.message.contains("duplicate"), "{}", err.message);
    }

    #[test]
    fn build_template_starts_at_v1_and_bumps_on_edit() {
        let params = base_put("rev");
        let v1 = build_template(&params, None, 1_000);
        assert_eq!(v1.version, 1);
        assert_eq!(v1.created_unix_ms, 1_000);
        let v2 = build_template(&params, Some(&v1), 2_000);
        assert_eq!(v2.version, 2);
        // created is preserved across edits; updated advances.
        assert_eq!(v2.created_unix_ms, 1_000);
        assert_eq!(v2.updated_unix_ms, 2_000);
        // Identical config => identical hash regardless of version/timestamps.
        assert_eq!(v1.config_hash, v2.config_hash);
    }

    #[test]
    fn config_hash_changes_when_config_changes() {
        let params = base_put("rev");
        let v1 = build_template(&params, None, 1_000);
        let mut edited = base_put("rev");
        edited.model = Some("claude-sonnet-4-6".to_owned());
        let v2 = build_template(&edited, Some(&v1), 2_000);
        assert_ne!(v1.config_hash, v2.config_hash);
    }

    #[test]
    fn render_spawn_substitutes_and_carries_provenance() {
        let template = build_template(&base_put("rev"), None, 1_000);
        let params = [
            ("repo".to_owned(), "Synapse".to_owned()),
            ("focus".to_owned(), "perf".to_owned()),
        ]
        .into_iter()
        .collect();
        let rendered = render_spawn(&template, &params).expect("render");
        assert_eq!(rendered.cli, ActSpawnAgentCli::Claude);
        assert_eq!(rendered.prompt.as_deref(), Some("Review Synapse for perf."));
        assert_eq!(rendered.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(rendered.provenance.template_id, "rev");
        assert_eq!(rendered.provenance.version, 1);
        assert_eq!(rendered.provenance.config_hash, template.config_hash);
    }

    #[test]
    fn render_spawn_rejects_missing_param() {
        let template = build_template(&base_put("rev"), None, 1_000);
        let params = [("repo".to_owned(), "Synapse".to_owned())]
            .into_iter()
            .collect();
        let err = render_spawn(&template, &params).expect_err("must reject");
        assert!(
            err.message.contains("missing required template_params: [\"focus\"]"),
            "{}",
            err.message
        );
    }

    #[test]
    fn render_spawn_rejects_unknown_param() {
        let template = build_template(&base_put("rev"), None, 1_000);
        let params = [
            ("repo".to_owned(), "Synapse".to_owned()),
            ("focus".to_owned(), "perf".to_owned()),
            ("extra".to_owned(), "nope".to_owned()),
        ]
        .into_iter()
        .collect();
        let err = render_spawn(&template, &params).expect_err("must reject");
        assert!(
            err.message.contains("unknown template_params"),
            "{}",
            err.message
        );
    }

    #[test]
    fn render_spawn_handles_no_param_template() {
        let mut params = base_put("idle");
        params.prompt_template = Some("Just idle and wait.".to_owned());
        params.required_params = vec![];
        let template = build_template(&params, None, 1_000);
        let rendered = render_spawn(&template, &BTreeMap::new()).expect("render");
        assert_eq!(rendered.prompt.as_deref(), Some("Just idle and wait."));
    }

    #[test]
    fn key_encoding_is_stable_and_ordered() {
        assert_eq!(current_pointer_key("rev"), "agent-template/v1/cur/rev");
        assert_eq!(
            version_snapshot_key("rev", 7),
            "agent-template/v1/ver/rev/0000000007"
        );
        // Zero-padding keeps a lexical scan in numeric order.
        assert!(version_snapshot_key("rev", 2) < version_snapshot_key("rev", 10));
    }
}
