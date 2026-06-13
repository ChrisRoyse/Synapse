//! Storage-backed tests for the #901 cost tools. These exercise the real
//! `CF_KV` price table and real `CF_AGENT_TRANSCRIPTS` rows through the same
//! daemon storage handle the tools use — no mock storage, no mock usage. Each
//! test writes physical rows and verifies the rollup reconciles with them.

use std::{num::NonZeroUsize, path::Path, path::PathBuf, sync::Arc};

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::server::agent_transcripts::ingest_spawn_dir_once;
use crate::{m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig};
use synapse_core::{
    AgentTranscriptRecord, TranscriptParseStatus, TranscriptRole, TranscriptSource, TranscriptUsage,
};
use synapse_storage::{
    Db, agent_transcripts::agent_transcript_key, agent_transcripts::agent_transcript_spawn_prefix,
    agent_transcripts::decode_agent_transcript_key, cf,
};

const CLAUDE_REAL_STREAM: &str = include_str!("../../../tests/fixtures/claude_stream_real.jsonl");
const CODEX_REAL_STREAM: &str = include_str!("../../../tests/fixtures/codex_exec_real.jsonl");

fn service_with_db(path: &Path) -> SynapseService {
    SynapseService::try_with_m2_shutdown_reason_and_m3_config(
        CancellationToken::new(),
        "test",
        CancellationToken::new(),
        &M2ServiceConfig::default(),
        M3ServiceConfig::from_cli_parts(
            Some(path.join("db")),
            Some(path.to_path_buf()),
            false,
            "127.0.0.1:0".to_owned(),
            NonZeroUsize::new(4).expect("nonzero"),
            false,
            true,
            None,
            false,
            None,
        ),
        M4ServiceConfig::default(),
    )
    .expect("construct service")
}

fn db_of(service: &SynapseService) -> Arc<Db> {
    service.agent_cost_db().expect("open storage")
}

/// Builds a parsed transcript row and writes it physically.
#[allow(clippy::too_many_arguments)]
fn write_row(
    db: &Db,
    spawn_id: &str,
    line_no: u64,
    ts_ns: u64,
    source: TranscriptSource,
    event_kind: &str,
    model: Option<&str>,
    role: TranscriptRole,
    usage: Option<TranscriptUsage>,
) {
    let mut record =
        AgentTranscriptRecord::new(ts_ns, spawn_id.to_owned(), line_no, source, 16, "a".repeat(64));
    record.status = TranscriptParseStatus::Parsed;
    record.role = Some(role);
    record.event_kind = Some(event_kind.to_owned());
    record.model = model.map(ToOwned::to_owned);
    record.usage = usage;
    record.validate().expect("synthetic row must be valid");
    let value = serde_json::to_vec(&record).expect("serialize row");
    // Mirror the #900 ingester, which writes transcript rows via the
    // pressure-bypass path so they are immediately durable (plain put_batch
    // enqueues through the async batcher and is not visible until flush).
    db.put_batch_pressure_bypass(
        cf::CF_AGENT_TRANSCRIPTS,
        [(agent_transcript_key(spawn_id, line_no), value)],
    )
    .expect("write transcript row");
}

fn claude_usage(
    input: u64,
    output: u64,
    cr: u64,
    cc: u64,
    cost_micro: Option<u64>,
) -> TranscriptUsage {
    TranscriptUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cache_read_input_tokens: Some(cr),
        cache_creation_input_tokens: Some(cc),
        cache_creation_5m_input_tokens: None,
        cache_creation_1h_input_tokens: None,
        reasoning_output_tokens: None,
        total_cost_micro_usd: cost_micro,
        model_usage: Vec::new(),
    }
}

fn codex_usage(input: u64, output: u64, cached: u64, reasoning: u64) -> TranscriptUsage {
    TranscriptUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cache_read_input_tokens: Some(cached),
        cache_creation_input_tokens: None,
        cache_creation_5m_input_tokens: None,
        cache_creation_1h_input_tokens: None,
        reasoning_output_tokens: Some(reasoning),
        total_cost_micro_usd: None,
        model_usage: Vec::new(),
    }
}

fn fable_price() -> AgentCostPricePutParams {
    AgentCostPricePutParams {
        model_id: "claude-fable-5".to_owned(),
        provider: Some("anthropic".to_owned()),
        input_usd_per_mtok: 3.0,
        output_usd_per_mtok: 15.0,
        cache_read_usd_per_mtok: 0.30,
        cache_creation_usd_per_mtok: 3.75,
        cache_creation_5m_usd_per_mtok: None,
        cache_creation_1h_usd_per_mtok: None,
    }
}

/// Real published Claude Fable 5 rates, including the distinct 5m/1h cache-write
/// tiers, so a priced rollup reproduces Claude's own `modelUsage` cost exactly.
fn fable_price_real() -> AgentCostPricePutParams {
    AgentCostPricePutParams {
        model_id: "claude-fable-5".to_owned(),
        provider: Some("anthropic".to_owned()),
        input_usd_per_mtok: 10.0,
        output_usd_per_mtok: 50.0,
        cache_read_usd_per_mtok: 1.0,
        cache_creation_usd_per_mtok: 12.5, // aggregate == 5m write
        cache_creation_5m_usd_per_mtok: Some(12.5),
        cache_creation_1h_usd_per_mtok: Some(20.0),
    }
}

/// Real published Claude Haiku 4.5 rates (the multi-model session's sub-agent).
fn haiku_price_real() -> AgentCostPricePutParams {
    AgentCostPricePutParams {
        model_id: "claude-haiku-4-5-20251001".to_owned(),
        provider: Some("anthropic".to_owned()),
        input_usd_per_mtok: 1.0,
        output_usd_per_mtok: 5.0,
        cache_read_usd_per_mtok: 0.10,
        cache_creation_usd_per_mtok: 1.25,
        cache_creation_5m_usd_per_mtok: Some(1.25),
        cache_creation_1h_usd_per_mtok: Some(2.0),
    }
}

fn cost_params(spawn: Option<&str>, since: Option<u64>, until: Option<u64>) -> AgentCostParams {
    AgentCostParams {
        spawn_id: spawn.map(ToOwned::to_owned),
        since_ns: since,
        until_ns: until,
    }
}

// ---------------------------------------------------------------------------
// Price table CRUD against physical CF_KV rows
// ---------------------------------------------------------------------------

#[test]
fn price_put_list_delete_roundtrip_physical_rows() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);

    let mut params = fable_price();
    params.model_id = "  Claude-Fable-5 ".to_owned();
    let put = service.agent_cost_price_put_impl(params).expect("put price");
    assert_eq!(put.price.model_id, "claude-fable-5");
    assert_eq!(put.price.input_micro_usd_per_mtok, 3_000_000);
    assert_eq!(put.price.cache_read_micro_usd_per_mtok, 300_000);
    assert_eq!(put.price.cache_creation_micro_usd_per_mtok, 3_750_000);

    // FSV: the row physically exists under the expected key and decodes back.
    let key = price_row_key("claude-fable-5");
    let stored = get_exact_kv_row(&db, &key)
        .expect("read")
        .expect("row present");
    let decoded: ModelPrice = serde_json::from_slice(&stored).expect("decode physical row");
    assert_eq!(decoded, put.price);
    assert_eq!(put.storage_readback.value_len_bytes, stored.len() as u64);

    let list = service.agent_cost_price_list_impl().expect("list");
    assert_eq!(list.count, 1);
    assert_eq!(list.prices[0].model_id, "claude-fable-5");

    let del = service
        .agent_cost_price_delete_impl(AgentCostPriceDeleteParams {
            model_id: "claude-fable-5".to_owned(),
        })
        .expect("delete");
    assert!(del.existed);
    assert!(
        get_exact_kv_row(&db, &key).expect("read").is_none(),
        "price row must be physically gone after delete"
    );

    let del_again = service
        .agent_cost_price_delete_impl(AgentCostPriceDeleteParams {
            model_id: "claude-fable-5".to_owned(),
        })
        .expect("delete idempotent");
    assert!(!del_again.existed);
}

#[test]
fn put_rejects_negative_and_nonfinite_rates() {
    assert!(usd_per_mtok_to_micro(-1.0, "input").is_err());
    assert!(usd_per_mtok_to_micro(f64::NAN, "input").is_err());
    assert!(usd_per_mtok_to_micro(f64::INFINITY, "input").is_err());
    assert_eq!(usd_per_mtok_to_micro(0.25, "input").expect("ok"), 250_000);
}

// ---------------------------------------------------------------------------
// Claude rollup: result row is authoritative; assistant rows never summed
// ---------------------------------------------------------------------------

#[test]
fn claude_bills_result_row_not_partial_assistant_rows() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    service.agent_cost_price_put_impl(fable_price()).expect("price");

    let spawn = "agent-spawn-claude1";
    write_row(&db, spawn, 1, 100, TranscriptSource::ClaudeStreamJson, "system/init", Some("claude-fable-5"), TranscriptRole::System, None);
    write_row(&db, spawn, 2, 110, TranscriptSource::ClaudeStreamJson, "assistant", Some("claude-fable-5"), TranscriptRole::Assistant, Some(claude_usage(3684, 6, 20866, 4563, None)));
    write_row(&db, spawn, 3, 120, TranscriptSource::ClaudeStreamJson, "assistant", Some("claude-fable-5"), TranscriptRole::Assistant, Some(claude_usage(2, 73, 20959, 8899, None)));
    write_row(&db, spawn, 4, 130, TranscriptSource::ClaudeStreamJson, "result/success", Some("claude-fable-5"), TranscriptRole::Result, Some(claude_usage(4118, 1906, 283040, 22189, Some(864_631))));

    let out = service
        .agent_cost_impl(cost_params(Some(spawn), None, None))
        .expect("rollup");

    assert_eq!(out.scanned_rows, 4);
    assert_eq!(out.per_spawn.len(), 1);
    let s = &out.per_spawn[0];
    assert_eq!(s.status, "complete");
    assert_eq!(s.authoritative_line_no, Some(4));
    // Billed usage == the result row exactly, NOT the sum of assistant rows.
    assert_eq!(s.usage.input_tokens, 4118);
    assert_eq!(s.usage.output_tokens, 1906);
    assert_eq!(s.usage.cache_read_tokens, 283_040);
    assert_eq!(s.usage.cache_creation_tokens, 22_189);

    // Hand-computed micro-USD (rates per Mtok):
    //   input        = 4118   * 3_000_000 /1e6 = 12_354
    //   output       = 1906   * 15_000_000/1e6 = 28_590
    //   cache_read   = 283040 * 300_000   /1e6 = 84_912
    //   cache_create = 22189  * 3_750_000 /1e6 = 83_208 (floor 83208.75)
    //   total = 209_064
    match &s.cost {
        CostOutcome::Priced { cost } => {
            assert_eq!(cost.input_micro_usd, 12_354);
            assert_eq!(cost.output_micro_usd, 28_590);
            assert_eq!(cost.cache_read_micro_usd, 84_912);
            assert_eq!(cost.cache_creation_micro_usd, 83_208);
            assert_eq!(cost.total_micro_usd, 209_064);
        }
        CostOutcome::Unpriced { .. } => panic!("model is priced"),
    }
    assert_eq!(s.source_reported_micro_usd, Some(864_631));
    assert_eq!(s.reconciliation_delta_micro_usd, Some(864_631 - 209_064));

    assert_eq!(out.fleet.spawns_complete, 1);
    assert_eq!(out.fleet.computed_micro_usd, 209_064);
    assert_eq!(out.fleet.source_reported_micro_usd, 864_631);
    assert!(out.fleet.unpriced_models.is_empty());

    // Per-model rollup mirrors the single spawn.
    assert_eq!(out.per_model.len(), 1);
    assert_eq!(out.per_model[0].model, "claude-fable-5");
    assert_eq!(out.per_model[0].computed_micro_usd, Some(209_064));
}

// ---------------------------------------------------------------------------
// Codex rollup: cumulative max, cached subtracted from input
// ---------------------------------------------------------------------------

#[test]
fn codex_takes_cumulative_max_and_subtracts_cache() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    service
        .agent_cost_price_put_impl(AgentCostPricePutParams {
            model_id: "gpt-5.2".to_owned(),
            provider: Some("openai".to_owned()),
            input_usd_per_mtok: 1.75,
            output_usd_per_mtok: 14.0,
            cache_read_usd_per_mtok: 0.175,
            cache_creation_usd_per_mtok: 0.0,
            cache_creation_5m_usd_per_mtok: None,
            cache_creation_1h_usd_per_mtok: None,
        })
        .expect("price");

    let spawn = "agent-spawn-codex1";
    write_row(&db, spawn, 1, 200, TranscriptSource::CodexExecJson, "thread.started", Some("gpt-5.2"), TranscriptRole::System, None);
    write_row(&db, spawn, 2, 210, TranscriptSource::CodexExecJson, "turn.completed", Some("gpt-5.2"), TranscriptRole::Result, Some(codex_usage(50_000, 1_000, 30_000, 500)));
    write_row(&db, spawn, 3, 220, TranscriptSource::CodexExecJson, "turn.completed", Some("gpt-5.2"), TranscriptRole::Result, Some(codex_usage(144_733, 2_110, 103_296, 1_380)));

    let out = service
        .agent_cost_impl(cost_params(Some(spawn), None, None))
        .expect("rollup");
    let s = &out.per_spawn[0];
    assert_eq!(s.status, "complete");
    assert_eq!(s.usage.input_tokens, 144_733 - 103_296);
    assert_eq!(s.usage.cache_read_tokens, 103_296);
    assert_eq!(s.usage.cache_creation_tokens, 0);
    assert_eq!(s.usage.output_tokens, 2_110);

    // Hand-computed micro-USD:
    //   input      = 41437  * 1_750_000 /1e6 = 72_514 (floor 72514.75)
    //   output     = 2110   * 14_000_000/1e6 = 29_540
    //   cache_read = 103296 * 175_000   /1e6 = 18_076 (floor 18076.8)
    //   total = 120_130
    match &s.cost {
        CostOutcome::Priced { cost } => {
            assert_eq!(cost.input_micro_usd, 72_514);
            assert_eq!(cost.output_micro_usd, 29_540);
            assert_eq!(cost.cache_read_micro_usd, 18_076);
            assert_eq!(cost.total_micro_usd, 120_130);
        }
        CostOutcome::Unpriced { .. } => panic!("priced"),
    }
    assert_eq!(s.source_reported_micro_usd, None);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn unpriced_model_surfaces_without_guessing() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    let spawn = "agent-spawn-unpriced";
    write_row(&db, spawn, 1, 300, TranscriptSource::ClaudeStreamJson, "result/success", Some("mystery-model"), TranscriptRole::Result, Some(claude_usage(100, 200, 0, 0, Some(42))));

    let out = service
        .agent_cost_impl(cost_params(Some(spawn), None, None))
        .expect("rollup");
    let s = &out.per_spawn[0];
    assert_eq!(s.total_tokens, 300);
    match &s.cost {
        CostOutcome::Unpriced { model_id } => assert_eq!(model_id, "mystery-model"),
        CostOutcome::Priced { .. } => panic!("must be unpriced"),
    }
    assert_eq!(out.fleet.computed_micro_usd, 0);
    assert_eq!(out.fleet.unpriced_models, vec!["mystery-model".to_owned()]);
    assert_eq!(s.source_reported_micro_usd, Some(42));
}

#[test]
fn running_agent_with_no_terminal_row_is_incomplete_not_billed() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    let spawn = "agent-spawn-running";
    write_row(&db, spawn, 1, 400, TranscriptSource::ClaudeStreamJson, "assistant", Some("claude-fable-5"), TranscriptRole::Assistant, Some(claude_usage(10, 2, 0, 0, None)));

    let out = service
        .agent_cost_impl(cost_params(Some(spawn), None, None))
        .expect("rollup");
    let s = &out.per_spawn[0];
    assert_eq!(s.status, "no_terminal_usage");
    assert_eq!(s.total_tokens, 0);
    assert_eq!(out.fleet.spawns_incomplete, 1);
    assert_eq!(out.fleet.spawns_complete, 0);
}

#[test]
fn time_window_excludes_rows_outside_range() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    let spawn = "agent-spawn-window";
    write_row(&db, spawn, 1, 500, TranscriptSource::ClaudeStreamJson, "result/success", Some("mystery"), TranscriptRole::Result, Some(claude_usage(100, 100, 0, 0, None)));

    let out = service
        .agent_cost_impl(cost_params(Some(spawn), Some(600), Some(700)))
        .expect("rollup");
    // The row is physically scanned but filtered out of the window, so the
    // spawn contributes nothing and is not listed.
    assert_eq!(out.scanned_rows, 1);
    assert!(out.per_spawn.is_empty(), "out-of-window row must not appear");
    assert_eq!(out.fleet.spawns_total, 0);

    // Widening the window to include ts=500 surfaces the spawn and bills it.
    let included = service
        .agent_cost_impl(cost_params(Some(spawn), Some(400), Some(600)))
        .expect("rollup");
    assert_eq!(included.per_spawn.len(), 1);
    assert_eq!(included.per_spawn[0].status, "complete");
}

#[test]
fn fleet_scan_aggregates_multiple_spawns_and_models() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    service.agent_cost_price_put_impl(fable_price()).expect("price");

    // Two Claude spawns on the priced model, one Codex spawn unpriced.
    write_row(&db, "agent-spawn-a", 1, 100, TranscriptSource::ClaudeStreamJson, "result/success", Some("claude-fable-5"), TranscriptRole::Result, Some(claude_usage(1_000_000, 0, 0, 0, Some(1))));
    write_row(&db, "agent-spawn-b", 1, 100, TranscriptSource::ClaudeStreamJson, "result/success", Some("claude-fable-5"), TranscriptRole::Result, Some(claude_usage(2_000_000, 0, 0, 0, Some(2))));
    write_row(&db, "agent-spawn-c", 1, 100, TranscriptSource::CodexExecJson, "turn.completed", Some("gpt-5.2"), TranscriptRole::Result, Some(codex_usage(500, 0, 0, 0)));

    let out = service
        .agent_cost_impl(cost_params(None, None, None))
        .expect("fleet rollup");
    assert_eq!(out.scanned_rows, 3);
    assert_eq!(out.fleet.spawns_total, 3);
    assert_eq!(out.fleet.spawns_complete, 3);
    // Claude input $3/Mtok: 1M -> 3_000_000 micro, 2M -> 6_000_000 micro.
    assert_eq!(out.fleet.computed_micro_usd, 9_000_000);
    assert_eq!(out.fleet.source_reported_micro_usd, 3);
    assert_eq!(out.fleet.unpriced_models, vec!["gpt-5.2".to_owned()]);
    // Per-model: fable aggregates the two spawns.
    let fable = out
        .per_model
        .iter()
        .find(|m| m.model == "claude-fable-5")
        .expect("fable present");
    assert_eq!(fable.spawns, 2);
    assert_eq!(fable.computed_micro_usd, Some(9_000_000));
}

#[test]
fn empty_store_yields_zero_rollup() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let out = service
        .agent_cost_impl(cost_params(None, None, None))
        .expect("rollup");
    assert_eq!(out.scanned_rows, 0);
    assert_eq!(out.fleet.spawns_total, 0);
    assert_eq!(out.fleet.computed_micro_usd, 0);
    assert!(out.per_spawn.is_empty());
}

// ---------------------------------------------------------------------------
// End-to-end FSV against the REAL captured CLI fixtures (no synthetic usage).
// Ingest through the real #900 parser, then reconcile agent_cost against the
// physical rows and the CLI's own reported cost.
// ---------------------------------------------------------------------------

fn plant_spawn_dir(root: &Path, spawn_id: &str, source: TranscriptSource, stdout: &str) -> PathBuf {
    let log_dir = root.join(spawn_id);
    std::fs::create_dir_all(&log_dir).expect("create spawn dir");
    let marker = match source {
        TranscriptSource::ClaudeStreamJson => "claude-mcp-config.json",
        TranscriptSource::CodexExecJson => "codex-notify.ps1",
    };
    std::fs::write(log_dir.join(marker), b"{}").expect("marker");
    std::fs::write(log_dir.join("stdout.jsonl"), stdout).expect("stdout");
    std::fs::write(
        log_dir.join("completion-status.json"),
        br#"{"schema_version":1,"status":"ok"}"#,
    )
    .expect("completion");
    log_dir
}

fn authoritative_row(db: &Db, spawn_id: &str, want_kind_prefix: &str) -> AgentTranscriptRecord {
    db.scan_cf_prefix(cf::CF_AGENT_TRANSCRIPTS, &agent_transcript_spawn_prefix(spawn_id))
        .expect("scan")
        .into_iter()
        .filter_map(|(key, value)| {
            let (_id, line) = decode_agent_transcript_key(&key).expect("key");
            let record: AgentTranscriptRecord = serde_json::from_slice(&value).expect("decode");
            record
                .event_kind
                .as_deref()
                .is_some_and(|k| k.starts_with(want_kind_prefix))
                .then_some((line, record))
        })
        .max_by_key(|(line, _)| *line)
        .map(|(_, record)| record)
        .expect("authoritative row present")
}

#[test]
fn fsv_claude_real_fixture_multi_model_attribution_reconciles_to_zero() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    let root = TempDir::new().expect("spawn root");
    let spawn = "agent-spawn-claudefsv";

    // 1. Ingest the REAL Claude capture through the production #900 parser.
    let log_dir = plant_spawn_dir(root.path(), spawn, TranscriptSource::ClaudeStreamJson, CLAUDE_REAL_STREAM);
    let outcome = ingest_spawn_dir_once(&db, spawn, &log_dir, false).expect("ingest");
    assert_eq!(outcome.new_invalid_rows, 0, "a real capture must parse fully");

    // 2. Source of truth: the physical result row carries the cache-creation
    //    TTL split (#949 part 3) and the per-model breakdown (#949 part 2).
    let result = authoritative_row(&db, spawn, "result/");
    let usage = result.usage.expect("result row carries usage");
    assert_eq!(usage.cache_creation_input_tokens, Some(22_189));
    assert_eq!(usage.cache_creation_5m_input_tokens, Some(0));
    assert_eq!(usage.cache_creation_1h_input_tokens, Some(22_189));
    assert_eq!(usage.total_cost_micro_usd, Some(864_631));
    // The real session used TWO models; the top-level usage reflects only the
    // primary (claude-fable-5), so the breakdown is required for exactness.
    assert_eq!(usage.model_usage.len(), 2, "fixture is a multi-model session");
    let fable = usage
        .model_usage
        .iter()
        .find(|m| m.model == "claude-fable-5")
        .expect("fable present");
    assert_eq!(fable.input_tokens, 4118);
    assert_eq!(fable.cache_creation_input_tokens, 22_189);
    assert_eq!(fable.cost_micro_usd, Some(863_300)); // $0.8633
    let haiku = usage
        .model_usage
        .iter()
        .find(|m| m.model == "claude-haiku-4-5-20251001")
        .expect("haiku present");
    assert_eq!(haiku.input_tokens, 1246);
    assert_eq!(haiku.cost_micro_usd, Some(1_331)); // $0.001331
    println!(
        "FSV[claude] modelUsage: fable={:?} haiku={:?} top_level_cc_1h={:?} session_cost_micro={:?}",
        fable.cost_micro_usd, haiku.cost_micro_usd,
        usage.cache_creation_1h_input_tokens, usage.total_cost_micro_usd
    );

    // 3. Price BOTH models at their real published rates (with TTL tiers) and
    //    roll up.
    service.agent_cost_price_put_impl(fable_price_real()).expect("price fable");
    service.agent_cost_price_put_impl(haiku_price_real()).expect("price haiku");
    let out = service
        .agent_cost_impl(cost_params(Some(spawn), None, None))
        .expect("rollup");
    let s = &out.per_spawn[0];

    // 4. Billed usage == the SUM across models (not just the primary): the
    //    multi-model session no longer undercounts.
    assert_eq!(s.usage.input_tokens, 4118 + 1246); // 5364
    assert_eq!(s.usage.output_tokens, 1906 + 17); // 1923
    assert_eq!(s.usage.cache_read_tokens, 283_040);
    assert_eq!(s.usage.cache_creation_tokens, 22_189);
    assert_eq!(s.usage.cache_creation_1h_tokens, 22_189);

    // 5. THE #949 ACCEPTANCE: exact per-model attribution + per-TTL cache
    //    pricing drives the reconciliation delta to ZERO.
    let computed = match &s.cost {
        CostOutcome::Priced { cost } => cost.total_micro_usd,
        CostOutcome::Unpriced { .. } => panic!("priced"),
    };
    assert_eq!(computed, 864_631, "computed == sum of per-model costs");
    assert_eq!(s.source_reported_micro_usd, Some(864_631));
    assert_eq!(
        s.reconciliation_delta_micro_usd,
        Some(0),
        "multi-model session reconciles exactly"
    );

    // 6. The per-spawn breakdown carries both priced models, each matching the
    //    CLI's own per-model costUSD.
    assert_eq!(s.models.len(), 2);
    let fable_cost = s.models.iter().find(|m| m.model == "claude-fable-5").expect("fable");
    let fable_computed = match &fable_cost.cost {
        CostOutcome::Priced { cost } => cost.total_micro_usd,
        CostOutcome::Unpriced { .. } => panic!("fable priced"),
    };
    assert_eq!(fable_computed, 863_300);
    assert_eq!(fable_cost.source_reported_micro_usd, Some(863_300));
    let haiku_cost = s.models.iter().find(|m| m.model == "claude-haiku-4-5-20251001").expect("haiku");
    let haiku_computed = match &haiku_cost.cost {
        CostOutcome::Priced { cost } => cost.total_micro_usd,
        CostOutcome::Unpriced { .. } => panic!("haiku priced"),
    };
    assert_eq!(haiku_computed, 1_331);
    println!(
        "FSV[claude] computed_micro={computed} source_reported_micro={:?} delta_micro={:?} (fable={fable_computed} + haiku={haiku_computed})",
        s.source_reported_micro_usd, s.reconciliation_delta_micro_usd
    );

    assert_eq!(out.fleet.computed_micro_usd, 864_631);
    assert!(out.fleet.unpriced_models.is_empty(), "both models priced");
}

#[test]
fn fsv_claude_partial_pricing_surfaces_unpriced_side_model() {
    // When the operator prices only the primary model, the sub-agent model is
    // honestly surfaced as unpriced and the delta exposes exactly its cost —
    // never hidden, never guessed.
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    let root = TempDir::new().expect("spawn root");
    let spawn = "agent-spawn-claudepartial";

    let log_dir = plant_spawn_dir(root.path(), spawn, TranscriptSource::ClaudeStreamJson, CLAUDE_REAL_STREAM);
    ingest_spawn_dir_once(&db, spawn, &log_dir, false).expect("ingest");
    service.agent_cost_price_put_impl(fable_price_real()).expect("price fable only");

    let out = service
        .agent_cost_impl(cost_params(Some(spawn), None, None))
        .expect("rollup");
    let s = &out.per_spawn[0];
    let computed = match &s.cost {
        CostOutcome::Priced { cost } => cost.total_micro_usd,
        CostOutcome::Unpriced { .. } => panic!("fable is priced"),
    };
    assert_eq!(computed, 863_300, "only fable's cost is computed");
    // Delta == exactly haiku's cost ($0.001331): the gap is surfaced, not hidden.
    assert_eq!(s.reconciliation_delta_micro_usd, Some(864_631 - 863_300));
    assert_eq!(out.fleet.unpriced_models, vec!["claude-haiku-4-5-20251001".to_owned()]);
}

#[test]
fn fsv_codex_real_fixture_bills_cumulative_turn_with_cache_subtracted() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    let root = TempDir::new().expect("spawn root");
    let spawn = "agent-spawn-codexfsv";

    let log_dir = plant_spawn_dir(root.path(), spawn, TranscriptSource::CodexExecJson, CODEX_REAL_STREAM);
    let outcome = ingest_spawn_dir_once(&db, spawn, &log_dir, false).expect("ingest");
    assert_eq!(outcome.new_invalid_rows, 0, "a real capture must parse fully");

    // Source of truth: the real turn.completed row.
    let turn = authoritative_row(&db, spawn, "turn.completed");
    let usage = turn.usage.expect("turn row usage");
    assert_eq!(usage.input_tokens, Some(144_733));
    assert_eq!(usage.cache_read_input_tokens, Some(103_296));
    assert_eq!(usage.output_tokens, Some(2_110));
    println!(
        "FSV[codex] physical turn row: in={:?} cached={:?} out={:?} reasoning={:?}",
        usage.input_tokens, usage.cache_read_input_tokens, usage.output_tokens,
        usage.reasoning_output_tokens
    );

    let out = service
        .agent_cost_impl(cost_params(Some(spawn), None, None))
        .expect("rollup");
    let s = &out.per_spawn[0];
    // cached (103296) is subtracted from input (144733) -> 41437 full-rate.
    assert_eq!(s.usage.input_tokens, 41_437);
    assert_eq!(s.usage.cache_read_tokens, 103_296);
    assert_eq!(s.usage.cache_creation_tokens, 0);
    assert_eq!(s.usage.output_tokens, 2_110);
    // No price set for this model -> honestly unpriced, tokens still counted.
    match &s.cost {
        CostOutcome::Unpriced { model_id } => {
            println!("FSV[codex] unpriced model surfaced: {model_id}");
        }
        CostOutcome::Priced { .. } => panic!("model was not priced"),
    }
    assert_eq!(s.total_tokens, 41_437 + 103_296 + 2_110);
}

#[test]
fn fsv_codex_real_fixture_priced_via_spawn_manifest_model() {
    // The Codex `exec --json` stream carries no model id (#949 part 1). The
    // spawn manifest written at launch is the authoritative source; the ingester
    // seeds the cursor from it so every Codex row is stamped and priceable.
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    let root = TempDir::new().expect("spawn root");
    let spawn = "agent-spawn-codexmanifest";

    let log_dir = plant_spawn_dir(root.path(), spawn, TranscriptSource::CodexExecJson, CODEX_REAL_STREAM);
    // Write the real manifest shape act_spawn_agent emits.
    std::fs::write(
        log_dir.join("spawn-manifest.json"),
        br#"{"version":1,"spawn_id":"agent-spawn-codexmanifest","cli":"codex","model":"gpt-5-codex","created_unix_ms":1781309519461}"#,
    )
    .expect("manifest");

    let outcome = ingest_spawn_dir_once(&db, spawn, &log_dir, false).expect("ingest");
    assert_eq!(outcome.new_invalid_rows, 0, "a real capture must parse fully");

    // FSV: the physical turn.completed row now carries the manifest model.
    let turn = authoritative_row(&db, spawn, "turn.completed");
    assert_eq!(
        turn.model.as_deref(),
        Some("gpt-5-codex"),
        "manifest model must be stamped onto Codex rows"
    );
    println!("FSV[codex] manifest-stamped model on physical row: {:?}", turn.model);

    // Price the model and confirm the Codex spawn is now PRICED, not unknown.
    service
        .agent_cost_price_put_impl(AgentCostPricePutParams {
            model_id: "gpt-5-codex".to_owned(),
            provider: Some("openai".to_owned()),
            input_usd_per_mtok: 1.25,
            output_usd_per_mtok: 10.0,
            cache_read_usd_per_mtok: 0.125,
            cache_creation_usd_per_mtok: 0.0,
            cache_creation_5m_usd_per_mtok: None,
            cache_creation_1h_usd_per_mtok: None,
        })
        .expect("price");
    let out = service
        .agent_cost_impl(cost_params(Some(spawn), None, None))
        .expect("rollup");
    let s = &out.per_spawn[0];
    assert_eq!(s.model.as_deref(), Some("gpt-5-codex"));
    // input 144733 - cached 103296 = 41437 full-rate.
    //   input      = 41437  * 1_250_000 /1e6 = 51_796 (floor 51796.25)
    //   output     = 2110   * 10_000_000/1e6 = 21_100
    //   cache_read = 103296 * 125_000   /1e6 = 12_912 (floor 12912.0)
    //   total = 85_808
    match &s.cost {
        CostOutcome::Priced { cost } => {
            assert_eq!(cost.input_micro_usd, 51_796);
            assert_eq!(cost.output_micro_usd, 21_100);
            assert_eq!(cost.cache_read_micro_usd, 12_912);
            assert_eq!(cost.total_micro_usd, 85_808);
            println!("FSV[codex] priced via manifest model: total_micro={}", cost.total_micro_usd);
        }
        CostOutcome::Unpriced { model_id } => panic!("must be priced, got unpriced {model_id}"),
    }
    assert!(out.fleet.unpriced_models.is_empty(), "model is priced");
}

#[test]
fn range_with_since_ge_until_is_rejected() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let err = service
        .agent_cost_impl(cost_params(None, Some(10), Some(10)))
        .expect_err("must reject");
    assert!(err.message.contains("AGENT_COST_RANGE_INVALID"), "{:?}", err.message);
}
