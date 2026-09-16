//! Durable agent-spawn templates (#909, simplified).
//!
//! A template is a saved, reusable spawn preset with exactly five operator
//! fields: a **name**, a **description** (what it's for), a **model**, a
//! **directory**, and a **prompt**. That is the whole contract — nothing else.
//!
//! `model` is a single string that selects the agent kind and the concrete
//! model in one field:
//! - `"claude"` spawns a Claude agent,
//! - `"codex"` spawns a Codex agent,
//! - any other value is the **name of a registered local/API model** (the
//!   #931 runner row, e.g. `deepseek-flash`) and spawns through it.
//!
//! `act_spawn_agent` renders a concrete spawn from a template by id (the prompt
//! is used verbatim — there is no placeholder substitution). Storage is the
//! daemon-owned `CF_KV` handle: a single current row per template id, edited in
//! place. There is no versioning — a template is just its latest definition.

use std::collections::BTreeMap;

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

/// CF_KV key namespace for template rows. `v2` is the simplified five-field
/// shape; the version is in the prefix so a format change is a clean re-key,
/// never an in-place migration (old `v1` rows are simply ignored).
const TEMPLATE_NAMESPACE: &str = "agent-template/v2";
/// Schema version stamped onto every stored template row.
const TEMPLATE_SCHEMA_VERSION: u32 = 2;

const MAX_TEMPLATE_ID_CHARS: usize = 200;
const MAX_TEMPLATE_NAME_CHARS: usize = 200;
const MAX_TEMPLATE_DESCRIPTION_CHARS: usize = 4000;
/// Matches the spawn prompt cap (`MAX_AGENT_SPAWN_PROMPT_BYTES`); a template
/// prompt is held to the same limit so it can never produce a prompt the spawn
/// would reject.
const MAX_PROMPT_BYTES: usize = 128 * 1024;
const MAX_DIRECTORY_CHARS: usize = 4096;
const MAX_MODEL_BYTES: usize = 256;
const MAX_LIST_TEMPLATES: usize = 1000;

/// Reserved model values that select a first-party agent kind rather than a
/// registered local-model row.
const MODEL_CLAUDE: &str = "claude";
const MODEL_CODEX: &str = "codex";

/// The durable template record: exactly the five operator fields plus identity
/// and timestamps. Field order is fixed so the canonical JSON used for
/// `config_hash` is deterministic.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnTemplate {
    pub schema_version: u32,
    pub template_id: String,
    /// Human-readable label surfaced in the dashboard template list.
    pub name: String,
    /// What this template is for — a free-text description for easy
    /// identification. May be empty.
    pub description: String,
    /// `claude`, `codex`, or the name of a registered local/API model.
    pub model: String,
    /// Working directory the spawned agent runs in. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// The prompt, used verbatim (no placeholder substitution).
    pub prompt: String,
    /// sha256 of the canonical operator fields (name/description/model/
    /// directory/prompt). Stable provenance anchor recorded on each spawn.
    pub config_hash: String,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

/// The operator-supplied fields; hashing this gives `config_hash`. Kept as a
/// distinct struct so the hash is independent of timestamps/identity.
#[derive(Serialize)]
struct CanonicalTemplateConfig<'a> {
    name: &'a str,
    description: &'a str,
    model: &'a str,
    directory: &'a Option<String>,
    prompt: &'a str,
}

/// Provenance stamped onto a spawn rendered from a template. `version` is
/// retained for the spawn-journal / cost-accounting contract but templates are
/// unversioned, so it is always `1`.
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
    pub model_ref: Option<String>,
    pub prompt: Option<String>,
    pub working_dir: Option<String>,
    pub target: Option<ActSpawnAgentTarget>,
    pub provenance: TemplateProvenance,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplatePutParams {
    /// Stable template identity (`[a-z0-9._-]`). Reusing an existing id edits it
    /// in place.
    pub template_id: String,
    /// Human-readable label.
    pub name: String,
    /// What this template is for. Optional.
    #[serde(default)]
    #[schemars(default)]
    pub description: String,
    /// `claude`, `codex`, or the name of a registered local/API model.
    pub model: String,
    /// Working directory for the spawned agent. Optional.
    #[serde(default)]
    #[schemars(default)]
    pub directory: Option<String>,
    /// The prompt, used verbatim.
    pub prompt: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplatePutResponse {
    pub ok: bool,
    pub template: SpawnTemplate,
    /// True when this put created a brand-new template id.
    pub created: bool,
    /// Physical CF_KV row written, for state verification.
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
    pub deleted_row_key: String,
}

// ---- key encoding ---------------------------------------------------------

fn template_key(template_id: &str) -> String {
    format!("{TEMPLATE_NAMESPACE}/cur/{template_id}")
}

fn template_prefix() -> String {
    format!("{TEMPLATE_NAMESPACE}/cur/")
}

// ---- validation & rendering (pure, unit-tested) ---------------------------

fn params_error(message: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, message.into())
}

fn is_kebab_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-')
        })
}

/// Resolves a template's `model` string into the agent CLI it selects and, for
/// a registered local/API model, the registry row name (`model_ref`).
fn resolve_model(model: &str) -> Result<(ActSpawnAgentCli, Option<String>), ErrorData> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return Err(params_error("agent_template model must not be empty"));
    }
    if trimmed.len() > MAX_MODEL_BYTES {
        return Err(params_error(format!(
            "agent_template model must be <= {MAX_MODEL_BYTES} bytes"
        )));
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(params_error(
            "agent_template model must not contain whitespace or control characters",
        ));
    }
    match trimmed {
        MODEL_CLAUDE => Ok((ActSpawnAgentCli::Claude, None)),
        MODEL_CODEX => Ok((ActSpawnAgentCli::Codex, None)),
        other => Ok((ActSpawnAgentCli::LocalModel, Some(other.to_owned()))),
    }
}

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
    if params.description.len() > MAX_TEMPLATE_DESCRIPTION_CHARS {
        return Err(params_error(format!(
            "agent_template description must be <= {MAX_TEMPLATE_DESCRIPTION_CHARS} chars"
        )));
    }
    resolve_model(&params.model)?;
    if params.prompt.trim().is_empty() {
        return Err(params_error("agent_template prompt must not be empty"));
    }
    if params.prompt.len() > MAX_PROMPT_BYTES {
        return Err(params_error(format!(
            "agent_template prompt must be <= {MAX_PROMPT_BYTES} bytes"
        )));
    }
    if let Some(directory) = &params.directory {
        if directory.trim().is_empty() {
            return Err(params_error(
                "agent_template directory must not be empty when provided",
            ));
        }
        if directory.chars().count() > MAX_DIRECTORY_CHARS {
            return Err(params_error(format!(
                "agent_template directory must be <= {MAX_DIRECTORY_CHARS} chars"
            )));
        }
    }
    Ok(())
}

/// Lowercase-hex sha256 of `bytes`.
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
    let bytes = serde_json::to_vec(config).unwrap_or_default();
    sha256_hex(&bytes)
}

/// Builds the stored record for a put, preserving `created_unix_ms` across edits.
fn build_template(
    params: &AgentTemplatePutParams,
    prior: Option<&SpawnTemplate>,
    now_unix_ms: u64,
) -> SpawnTemplate {
    let model = params.model.trim().to_owned();
    let directory = params
        .directory
        .as_ref()
        .map(|value| value.trim().to_owned());
    let canonical = CanonicalTemplateConfig {
        name: &params.name,
        description: &params.description,
        model: &model,
        directory: &directory,
        prompt: &params.prompt,
    };
    let hash = config_hash(&canonical);
    let created = prior.map_or(now_unix_ms, |p| p.created_unix_ms);
    SpawnTemplate {
        schema_version: TEMPLATE_SCHEMA_VERSION,
        template_id: params.template_id.clone(),
        name: params.name.clone(),
        description: params.description.clone(),
        model,
        directory,
        prompt: params.prompt.clone(),
        config_hash: hash,
        created_unix_ms: created,
        updated_unix_ms: now_unix_ms,
    }
}

/// Renders a concrete spawn from a stored template. The prompt is used verbatim.
pub(crate) fn render_spawn(template: &SpawnTemplate) -> Result<RenderedSpawn, ErrorData> {
    let (cli, model_ref) = resolve_model(&template.model)?;
    Ok(RenderedSpawn {
        cli,
        model: None,
        model_ref,
        prompt: Some(template.prompt.clone()),
        working_dir: template.directory.clone(),
        target: None,
        provenance: TemplateProvenance {
            template_id: template.template_id.clone(),
            // Templates are unversioned; the journal/cost contract keeps the
            // field, pinned to 1.
            version: 1,
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

    /// For a local-model template, confirm the `model` names a registered
    /// runner row. `claude`/`codex` need no registry check.
    fn validate_template_runtime_refs(
        &self,
        params: &AgentTemplatePutParams,
    ) -> Result<(), ErrorData> {
        let (cli, model_ref) = resolve_model(&params.model)?;
        if !cli.is_local_model() {
            return Ok(());
        }
        let model_ref = model_ref.unwrap_or_default();
        let rows = self.local_model_registry_snapshot()?;
        if rows.iter().any(|row| row.name == model_ref) {
            return Ok(());
        }
        Err(mcp_error(
            error_codes::MODEL_REGISTRY_NOT_FOUND,
            format!(
                "agent_template model {model_ref:?} is not 'claude'/'codex' and is not a registered local model"
            ),
        ))
    }

    /// Reads the stored row for a template id, if any.
    fn read_template(db: &Db, template_id: &str) -> Result<Option<SpawnTemplate>, ErrorData> {
        let key = template_key(template_id);
        let rows = db
            .scan_cf_prefix(cf::CF_KV, key.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_template failed to read template {key}: {error}"),
                )
            })?;
        for (raw_key, raw_value) in rows {
            // scan_cf_prefix is a prefix scan; only the exact key is this row.
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
        self.validate_template_runtime_refs(&params)?;
        let db = self.agent_template_db()?;
        let prior = Self::read_template(&db, &params.template_id)?;
        let created = prior.is_none();
        let now = unix_time_ms_now();
        let template = build_template(&params, prior.as_ref(), now);

        let encoded = encode_template(&template)?;
        let key = template_key(&template.template_id);
        db.put_batch(cf::CF_KV, [(key.clone().into_bytes(), encoded.clone())])
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "agent_template failed to persist template {:?}: {error}",
                        template.template_id
                    ),
                )
            })?;
        // Templates are durable config, not a stream: flush so the row is on disk
        // (and visible to the read path) before this returns OK (read-after-write).
        db.flush().map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "agent_template persisted template {:?} but failed to flush it to disk: {error}",
                    template.template_id
                ),
            )
        })?;

        tracing::info!(
            code = "AGENT_TEMPLATE_PUT",
            template_id = %template.template_id,
            created,
            config_hash = %template.config_hash,
            "readback=agent_templates edge=put"
        );

        Ok(AgentTemplatePutResponse {
            ok: true,
            written_rows: vec![row_readback(&key, &encoded)],
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
        let key = template_key(&params.template_id);
        let template = Self::read_template(&db, &params.template_id)?
            .ok_or_else(|| template_not_found(&params.template_id))?;
        Ok(AgentTemplateGetResponse {
            ok: true,
            template,
            row_key: key,
        })
    }

    fn agent_template_list_impl(
        &self,
        params: AgentTemplateListParams,
    ) -> Result<AgentTemplateListResponse, ErrorData> {
        let db = self.agent_template_db()?;
        let prefix = template_prefix();
        let rows = db
            .scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_template failed to list templates: {error}"),
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

    pub(crate) fn dashboard_list_agent_templates(
        &self,
    ) -> Result<AgentTemplateListResponse, ErrorData> {
        self.agent_template_list_impl(AgentTemplateListParams {
            max: MAX_LIST_TEMPLATES,
        })
    }

    pub(crate) fn dashboard_put_agent_template(
        &self,
        params: AgentTemplatePutParams,
    ) -> Result<AgentTemplatePutResponse, ErrorData> {
        self.agent_template_put_impl(params)
    }

    pub(crate) fn dashboard_delete_agent_template(
        &self,
        template_id: &str,
    ) -> Result<AgentTemplateDeleteResponse, ErrorData> {
        self.agent_template_delete_impl(AgentTemplateDeleteParams {
            template_id: template_id.to_owned(),
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
        Self::read_template(&db, &params.template_id)?
            .ok_or_else(|| template_not_found(&params.template_id))?;
        let key = template_key(&params.template_id);
        db.delete_batch(cf::CF_KV, [key.clone().into_bytes()])
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_template failed to delete template {key}: {error}"),
                )
            })?;
        // Flush so the deletion is durable and immediately reflected (read-after-write).
        db.flush().map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "agent_template deleted template {key} but failed to flush to disk: {error}"
                ),
            )
        })?;

        tracing::info!(
            code = "AGENT_TEMPLATE_DELETE",
            template_id = %params.template_id,
            "readback=agent_templates edge=delete"
        );

        Ok(AgentTemplateDeleteResponse {
            ok: true,
            template_id: params.template_id,
            deleted_row_key: key,
        })
    }

    /// Resolves a template id to a rendered spawn for `act_spawn_agent` (#909).
    /// `version` and `provided_params` are accepted for the (unchanged)
    /// `act_spawn_agent` contract but ignored: templates are unversioned and the
    /// prompt is verbatim.
    pub(crate) fn resolve_spawn_template(
        &self,
        template_id: &str,
        _version: Option<u32>,
        _provided_params: &BTreeMap<String, String>,
    ) -> Result<RenderedSpawn, ErrorData> {
        if !is_kebab_id(template_id) {
            return Err(params_error(
                "act_spawn_agent template_id must be non-empty [a-z0-9._-]",
            ));
        }
        let db = self.agent_template_db()?;
        let template = Self::read_template(&db, template_id)?
            .ok_or_else(|| template_not_found(template_id))?;
        render_spawn(&template)
    }
}

fn template_not_found(template_id: &str) -> ErrorData {
    mcp_error(
        error_codes::AGENT_TEMPLATE_NOT_FOUND,
        format!("agent_template not found: no template with id {template_id:?}"),
    )
}

#[tool_router(router = agent_template_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Create or edit a reusable agent-spawn template: name, description, model, directory, and prompt. `model` is 'claude', 'codex', or the name of a registered local/API model. Reusing a template_id edits it in place. act_spawn_agent spawns from a template by id, using the prompt verbatim."
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
        description = "Read one agent-spawn template by id. Errors AGENT_TEMPLATE_NOT_FOUND if absent."
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

    #[tool(description = "List agent-spawn templates (one row per template id), sorted by id.")]
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
        description = "Delete an agent-spawn template by id so new spawns from it fail loudly. Errors AGENT_TEMPLATE_NOT_FOUND if absent."
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
            description: "Reviews the repo for correctness bugs.".to_owned(),
            model: "claude".to_owned(),
            directory: Some(r"C:\code\Synapse".to_owned()),
            prompt: "Review the repo for correctness bugs.".to_owned(),
        }
    }

    #[test]
    fn resolve_model_maps_first_party_and_registered() {
        assert_eq!(
            resolve_model("claude").expect("ok").0,
            ActSpawnAgentCli::Claude
        );
        assert_eq!(
            resolve_model("codex").expect("ok").0,
            ActSpawnAgentCli::Codex
        );
        let (cli, model_ref) = resolve_model("deepseek-flash").expect("ok");
        assert_eq!(cli, ActSpawnAgentCli::LocalModel);
        assert_eq!(model_ref.as_deref(), Some("deepseek-flash"));
    }

    #[test]
    fn resolve_model_rejects_empty_and_whitespace() {
        assert_eq!(
            error_code_str(&resolve_model("   ").expect_err("reject")),
            error_codes::TOOL_PARAMS_INVALID
        );
        assert!(
            resolve_model("two words")
                .expect_err("reject")
                .message
                .contains("whitespace")
        );
    }

    #[test]
    fn validate_put_rejects_empty_prompt() {
        let mut params = base_put("rev");
        params.prompt = "   ".to_owned();
        let err = validate_put_params(&params).expect_err("must reject");
        assert!(
            err.message.contains("prompt must not be empty"),
            "{}",
            err.message
        );
    }

    #[test]
    fn validate_put_rejects_bad_id() {
        let mut params = base_put("Bad Id");
        params.template_id = "Bad Id".to_owned();
        let err = validate_put_params(&params).expect_err("must reject");
        assert!(err.message.contains("template_id"), "{}", err.message);
    }

    #[test]
    fn validate_put_rejects_empty_directory_when_present() {
        let mut params = base_put("rev");
        params.directory = Some("  ".to_owned());
        let err = validate_put_params(&params).expect_err("must reject");
        assert!(err.message.contains("directory"), "{}", err.message);
    }

    #[test]
    fn build_template_preserves_created_and_hashes_config() {
        let params = base_put("rev");
        let v1 = build_template(&params, None, 1_000);
        assert_eq!(v1.created_unix_ms, 1_000);
        assert_eq!(v1.schema_version, TEMPLATE_SCHEMA_VERSION);
        let v2 = build_template(&params, Some(&v1), 2_000);
        assert_eq!(v2.created_unix_ms, 1_000);
        assert_eq!(v2.updated_unix_ms, 2_000);
        // Identical config => identical hash.
        assert_eq!(v1.config_hash, v2.config_hash);
    }

    #[test]
    fn config_hash_changes_when_config_changes() {
        let v1 = build_template(&base_put("rev"), None, 1_000);
        let mut edited = base_put("rev");
        edited.model = "codex".to_owned();
        let v2 = build_template(&edited, Some(&v1), 2_000);
        assert_ne!(v1.config_hash, v2.config_hash);
    }

    #[test]
    fn render_spawn_uses_prompt_verbatim_and_carries_provenance() {
        let template = build_template(&base_put("rev"), None, 1_000);
        let rendered = render_spawn(&template).expect("render");
        assert_eq!(rendered.cli, ActSpawnAgentCli::Claude);
        assert_eq!(
            rendered.prompt.as_deref(),
            Some("Review the repo for correctness bugs.")
        );
        assert_eq!(rendered.model, None);
        assert_eq!(rendered.model_ref, None);
        assert_eq!(rendered.working_dir.as_deref(), Some(r"C:\code\Synapse"));
        assert_eq!(rendered.provenance.template_id, "rev");
        assert_eq!(rendered.provenance.version, 1);
        assert_eq!(rendered.provenance.config_hash, template.config_hash);
    }

    #[test]
    fn render_spawn_supports_registered_local_model() {
        let mut params = base_put("local");
        params.model = "ollama-gemma4-e4b".to_owned();
        let template = build_template(&params, None, 1_000);
        assert_eq!(template.model, "ollama-gemma4-e4b");
        let rendered = render_spawn(&template).expect("render");
        assert_eq!(rendered.cli, ActSpawnAgentCli::LocalModel);
        assert_eq!(rendered.model, None);
        assert_eq!(rendered.model_ref.as_deref(), Some("ollama-gemma4-e4b"));
    }

    #[test]
    fn key_encoding_is_stable() {
        assert_eq!(template_key("rev"), "agent-template/v2/cur/rev");
        assert_eq!(template_prefix(), "agent-template/v2/cur/");
    }
}
