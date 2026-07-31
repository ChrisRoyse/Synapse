//! Manual FSV instrument: does an observed `tool_result` actually become a
//! grounded outcome anchor on the physical Anchors CF? (#1926)
//!
//! ## What #1926 found
//!
//! `ambient_agents.rs::classify_user` parses the `is_error` boolean off every
//! `tool_result` block, normalizes it, and persists it on
//! `TranscriptToolCall::status` — and then nothing consumes it as an outcome.
//! 5,954 `user/tool_result` rows on the live vault carried an adjudicated
//! result that reached no anchor, which is the whole reason the Differentiate
//! stack sits on a provisional corpus.
//!
//! ## What this proves, and how
//!
//! A return value is a claim. Every assertion below is read back from the
//! **physical Calyx Anchors CF** through `Db::read_grounding_anchors_for_source`
//! after the write, and the decoded anchor's boolean is compared against the
//! outcome that was known before the vault was ever opened.
//!
//! The corpus is **real**: rows are parsed out of this host's actual Claude
//! session JSONL files, not constructed. The one exception is the two
//! deliberately malformed rows in the boundary audit, which cannot be sampled
//! because the real corpus (correctly) contains no such row — those are
//! labelled where they appear.
//!
//! ```text
//! cargo run --release -p synapse-storage --example transcript_outcome_anchor_fsv -- <empty-scratch-dir>
//! ```
//!
//! ## The three adjudications
//!
//! The Anthropic Messages API defines `is_error` as an *optional* boolean set
//! to `true` on failure; success is spelled `false` **or omitted**. So a real
//! corpus contains three observations, and this instrument requires all three
//! to be present in the sample before it will report success — a run that saw
//! only one spelling would prove almost nothing.
//!
//! | observation        | adjudication       | anchor      |
//! |--------------------|--------------------|-------------|
//! | `is_error: true`   | `declared_error`   | `Bool(false)` |
//! | `is_error: false`  | `declared_ok`      | `Bool(true)`  |
//! | field absent       | `omitted_is_error` | `Bool(true)`  |

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use synapse_core::types::{
    AgentTranscriptRecord, TranscriptParseStatus, TranscriptRole, TranscriptSource,
    TranscriptToolCall,
};
use synapse_storage::constellations::{
    AGENT_TOOL_CALL_SUCCESS_ANCHOR_KIND, AGENT_TRANSCRIPT_TOOL_RESULT_EVENT_KIND,
    AgentTranscriptToolAdjudication, AgentTranscriptToolOutcome, AgentTranscriptUnadjudicable,
    SOURCE_AGENT_TRANSCRIPT_TOOL_RESULT, TRANSCRIPT_TOOL_STATUS_ERROR, TRANSCRIPT_TOOL_STATUS_OK,
    agent_transcript_tool_outcome,
};
use synapse_storage::{CalyxAnchorRow, Db, agent_transcripts::agent_transcript_key, cf};

/// Schema sentinel the daemon opens with.
const SCHEMA_VERSION: u32 = 1;
/// How many real rows of each adjudication to sample. Small on purpose: the
/// claim is "the adjudication and the anchor agree, per row", which one row
/// proves and a thousand only repeat. The sample exists to cover all three
/// spellings and a few distinct real payloads, not to reach a sample size.
const ROWS_PER_ADJUDICATION: usize = 4;

fn main() -> Result<(), Box<dyn Error>> {
    let scratch = scratch_dir()?;
    println!("== #1926 transcript outcome anchor FSV ==");
    println!("scratch vault : {}", scratch.display());

    let sampled = sample_real_tool_results()?;
    report_sample(&sampled);

    let vault_dir = scratch.join("vault");
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;

    let mut failures = Vec::new();
    happy_path(&db, &sampled, &mut failures)?;
    boundary_audit(&db, &mut failures)?;

    println!();
    if failures.is_empty() {
        println!("RESULT: PASS — every anchor read back from the physical Anchors CF matched the");
        println!("        outcome known before the vault was opened.");
        Ok(())
    } else {
        println!("RESULT: FAIL — {} check(s) did not hold:", failures.len());
        for failure in &failures {
            println!("  - {failure}");
        }
        Err("FSV failed; see the failures above".into())
    }
}

// ---------------------------------------------------------------------------
// Real corpus sampling
// ---------------------------------------------------------------------------

/// One real `tool_result` line, plus the outcome derived from the raw JSON
/// **independently of the production parser**, so the two can be compared.
struct SampledRow {
    record: AgentTranscriptRecord,
    /// Derived here, straight off the raw block, from the Anthropic contract.
    expected_success: bool,
    expected_adjudication: AgentTranscriptToolAdjudication,
    source_file: String,
    source_line: u64,
}

fn sample_real_tool_results() -> Result<Vec<SampledRow>, Box<dyn Error>> {
    let root = claude_projects_root()?;
    let mut buckets: BTreeMap<&'static str, Vec<SampledRow>> = BTreeMap::new();
    let mut line_no = 0_u64;

    for path in session_files(&root)? {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            if !line.contains("\"tool_result\"") {
                continue;
            }
            let Ok(Value::Object(object)) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(block) = sole_tool_result_block(&object) else {
                continue;
            };
            // Derived from the raw JSON here, NOT from the production parser.
            let (expected_adjudication, expected_success) = match block.get("is_error") {
                Some(Value::Bool(true)) => (AgentTranscriptToolAdjudication::DeclaredError, false),
                Some(Value::Bool(false)) => (AgentTranscriptToolAdjudication::DeclaredOk, true),
                None | Some(Value::Null) => {
                    (AgentTranscriptToolAdjudication::OmittedIsError, true)
                }
                Some(_) => continue,
            };
            let bucket = buckets.entry(expected_adjudication.label()).or_default();
            if bucket.len() >= ROWS_PER_ADJUDICATION {
                continue;
            }
            line_no += 1;
            bucket.push(SampledRow {
                record: transcript_row_from_block(line, block, line_no),
                expected_success,
                expected_adjudication,
                source_file: path
                    .file_name()
                    .map_or_else(|| "?".to_owned(), |name| name.to_string_lossy().into_owned()),
                source_line: index as u64 + 1,
            });
        }
    }

    for adjudication in [
        AgentTranscriptToolAdjudication::DeclaredError,
        AgentTranscriptToolAdjudication::DeclaredOk,
        AgentTranscriptToolAdjudication::OmittedIsError,
    ] {
        if buckets.get(adjudication.label()).is_none_or(Vec::is_empty) {
            return Err(format!(
                "the real corpus under {} yielded no `{}` row, so this run could not prove that \
                 spelling; refusing to report a partial pass",
                root.display(),
                adjudication.label()
            )
            .into());
        }
    }
    Ok(buckets.into_values().flatten().collect())
}

/// The sole `tool_result` block on a `user` line, or `None`.
///
/// Deliberately refuses a line carrying more than one block: the production
/// adjudication refuses those too, and sampling one would test a path the
/// writer does not take.
fn sole_tool_result_block(object: &Map<String, Value>) -> Option<&Map<String, Value>> {
    if object.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let blocks = object
        .get("message")?
        .as_object()?
        .get("content")?
        .as_array()?;
    let mut found: Option<&Map<String, Value>> = None;
    for block in blocks {
        let block = block.as_object()?;
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(block);
    }
    found
}

/// Builds the transcript row the ambient parser would build for this block.
///
/// The `status` mapping mirrors `classify_user` exactly — that is the thing
/// under test, so it is spelled out here rather than imported, and any drift
/// between the two shows up as a failed comparison instead of passing silently.
fn transcript_row_from_block(
    raw_line: &str,
    block: &Map<String, Value>,
    line_no: u64,
) -> AgentTranscriptRecord {
    let mut record = AgentTranscriptRecord::new(
        1_785_000_000_000_000_000 + line_no,
        "agent-spawn-fsv1926".to_owned(),
        line_no,
        TranscriptSource::ClaudeSessionJsonl,
        raw_line.len() as u64,
        sha256_hex(raw_line.as_bytes()),
    );
    record.role = Some(TranscriptRole::Tool);
    record.event_kind = Some(AGENT_TRANSCRIPT_TOOL_RESULT_EVENT_KIND.to_owned());
    record.tool_calls = vec![TranscriptToolCall {
        tool_name: "tool_result".to_owned(),
        tool_call_id: block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        result_summary: Some(bounded(block.get("content").unwrap_or(&Value::Null))),
        result_bytes: Some(0),
        status: match block.get("is_error") {
            Some(Value::Bool(true)) => Some(TRANSCRIPT_TOOL_STATUS_ERROR.to_owned()),
            Some(Value::Bool(false)) => Some(TRANSCRIPT_TOOL_STATUS_OK.to_owned()),
            _ => None,
        },
        ..TranscriptToolCall::default()
    }];
    record
}

fn report_sample(rows: &[SampledRow]) {
    println!("\n-- real corpus sample --");
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for row in rows {
        *counts.entry(row.expected_adjudication.label()).or_default() += 1;
    }
    for (label, count) in &counts {
        println!("  {label:<18} {count} real row(s)");
    }
    println!("  total              {} row(s)", rows.len());
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

fn happy_path(
    db: &Db,
    rows: &[SampledRow],
    failures: &mut Vec<String>,
) -> Result<(), Box<dyn Error>> {
    println!("\n-- happy path: measure, ground, read the bytes back --");
    for row in rows {
        let key = agent_transcript_key(&row.record.spawn_id, row.record.line_no);
        let encoded = serde_json::to_vec(&row.record)?;

        // BEFORE: no anchors exist for this source row.
        let before = physical_anchors(db, &key, &encoded)?;
        if !before.is_empty() {
            failures.push(format!(
                "line {} had {} anchor(s) before any write",
                row.record.line_no,
                before.len()
            ));
        }

        // The production adjudication must agree with the outcome derived
        // independently from the raw JSON.
        match agent_transcript_tool_outcome(&row.record) {
            AgentTranscriptToolOutcome::Adjudicated(actual)
                if actual == row.expected_adjudication => {}
            other => {
                failures.push(format!(
                    "line {}: adjudication was {other:?}, expected {:?}",
                    row.record.line_no, row.expected_adjudication
                ));
                continue;
            }
        }

        db.put_agent_transcript_constellation(&key, &encoded, &row.record)?;
        let report = db.put_agent_transcript_outcome_anchor(&key, &encoded, &row.record)?;
        let Some(report) = report else {
            failures.push(format!(
                "line {}: a real tool_result row produced no anchor",
                row.record.line_no
            ));
            continue;
        };

        // AFTER: read the physical Anchors CF, do not trust the report.
        let after = physical_anchors(db, &key, &encoded)?;
        let matching: Vec<_> = after
            .iter()
            // The physical row renders a labelled kind as `label:<kind>`;
            // matching the bare kind would silently find nothing.
            .filter(|anchor| anchor.kind == format!("label:{AGENT_TOOL_CALL_SUCCESS_ANCHOR_KIND}"))
            .collect();
        if matching.len() != 1 {
            failures.push(format!(
                "line {}: physical Anchors CF holds {} `{AGENT_TOOL_CALL_SUCCESS_ANCHOR_KIND}` \
                 row(s), expected exactly 1",
                row.record.line_no,
                matching.len()
            ));
            continue;
        }
        let stored = matching[0];
        let stored_bool = stored.value.bool_value;
        let ok = stored.value.value_type == "bool"
            && stored_bool == Some(row.expected_success)
            && stored.source == SOURCE_AGENT_TRANSCRIPT_TOOL_RESULT;
        println!(
            "  {:<18} {}:{:<6} anchor={:<5} expected={:<5} cx_id={} {}",
            row.expected_adjudication.label(),
            truncate(&row.source_file, 12),
            row.source_line,
            stored_bool.map_or_else(|| "?".to_owned(), |value| value.to_string()),
            row.expected_success,
            &report.cx_id[..12.min(report.cx_id.len())],
            if ok { "OK" } else { "MISMATCH" }
        );
        if !ok {
            failures.push(format!(
                "line {}: physical anchor value={stored_bool:?} source={:?}, expected \
                 value={} source={SOURCE_AGENT_TRANSCRIPT_TOOL_RESULT:?}",
                row.record.line_no, stored.source, row.expected_success
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Boundary audit
// ---------------------------------------------------------------------------

fn boundary_audit(db: &Db, failures: &mut Vec<String>) -> Result<(), Box<dyn Error>> {
    println!("\n-- boundary audit --");

    // (1) A non-tool_result row. Real shape, taken from the same vocabulary:
    //     an assistant row carries tool_calls[] too, and must NOT be anchored.
    let mut assistant = base_row(9_001);
    assistant.role = Some(TranscriptRole::Assistant);
    assistant.event_kind = Some("assistant".to_owned());
    assistant.tool_calls = vec![TranscriptToolCall {
        tool_name: "Read".to_owned(),
        tool_call_id: Some("toolu_fsv_assistant".to_owned()),
        arguments: Some("{\"file_path\":\"C:/code/synapse/Cargo.toml\"}".to_owned()),
        ..TranscriptToolCall::default()
    }];
    let key = agent_transcript_key(&assistant.spawn_id, assistant.line_no);
    let encoded = serde_json::to_vec(&assistant)?;
    db.put_agent_transcript_constellation(&key, &encoded, &assistant)?;
    let anchored = db.put_agent_transcript_outcome_anchor(&key, &encoded, &assistant)?;
    let after = physical_anchors(db, &key, &encoded)?;
    let held = anchored.is_none() && after.is_empty();
    println!(
        "  assistant row (has tool_calls, no outcome) -> anchor={} physical_rows={} {}",
        anchored.is_some(),
        after.len(),
        verdict(held)
    );
    if !held {
        failures.push(
            "an assistant row was anchored, or left anchors on the physical CF".to_owned(),
        );
    }

    // (2) A tool_result row carrying two blocks. Constructed, because the real
    //     corpus contains zero such lines (9,034 lines, all exactly one block)
    //     — which is precisely why the adjudication may refuse rather than
    //     aggregate. Proving the refusal needs a row that cannot be sampled.
    let mut two_blocks = base_row(9_002);
    two_blocks.role = Some(TranscriptRole::Tool);
    two_blocks.event_kind = Some(AGENT_TRANSCRIPT_TOOL_RESULT_EVENT_KIND.to_owned());
    two_blocks.tool_calls = vec![
        TranscriptToolCall {
            tool_name: "tool_result".to_owned(),
            status: Some(TRANSCRIPT_TOOL_STATUS_OK.to_owned()),
            ..TranscriptToolCall::default()
        },
        TranscriptToolCall {
            tool_name: "tool_result".to_owned(),
            status: Some(TRANSCRIPT_TOOL_STATUS_ERROR.to_owned()),
            ..TranscriptToolCall::default()
        },
    ];
    let key = agent_transcript_key(&two_blocks.spawn_id, two_blocks.line_no);
    let encoded = serde_json::to_vec(&two_blocks)?;
    // The constellation MUST still publish. Refusing to adjudicate must cost an
    // anchor, never the durable evidence row: an error here would propagate
    // through commit_transcript_chunk, hold the ambient cursor, and wedge that
    // session's ingestion permanently the first time a legal multi-result line
    // appeared.
    let measured = db
        .put_agent_transcript_constellation(&key, &encoded, &two_blocks)
        .is_ok();
    let anchor_result = db.put_agent_transcript_outcome_anchor(&key, &encoded, &two_blocks);
    let no_anchor = matches!(anchor_result, Ok(None));
    let labelled = matches!(
        agent_transcript_tool_outcome(&two_blocks),
        AgentTranscriptToolOutcome::Unadjudicable(AgentTranscriptUnadjudicable::MultipleResults)
    );
    let after = physical_anchors(db, &key, &encoded)?;
    let held = measured && no_anchor && labelled && after.is_empty();
    println!(
        "  two-block tool_result (mixed ok/error)     -> measured={measured} anchor={} label={} physical_rows={} {}",
        if no_anchor { "none" } else { "WROTE ONE" },
        if labelled {
            AgentTranscriptUnadjudicable::MultipleResults.label()
        } else {
            "WRONG"
        },
        after.len(),
        verdict(held)
    );
    if !held {
        failures.push(format!(
            "a mixed two-block tool_result did not resolve as \"measured, unanchored, labelled\":              measured={measured} no_anchor={no_anchor} labelled={labelled} anchors={}",
            after.len()
        ));
    }

    // (3) An un-measured row. This is the case that decides whether the whole
    //     fix is real: an anchor whose constellation does not exist would be a
    //     dangling write that every grounding readback ignores. It must fail.
    let mut unmeasured = base_row(9_003);
    unmeasured.role = Some(TranscriptRole::Tool);
    unmeasured.event_kind = Some(AGENT_TRANSCRIPT_TOOL_RESULT_EVENT_KIND.to_owned());
    unmeasured.tool_calls = vec![TranscriptToolCall {
        tool_name: "tool_result".to_owned(),
        status: Some(TRANSCRIPT_TOOL_STATUS_ERROR.to_owned()),
        ..TranscriptToolCall::default()
    }];
    let key = agent_transcript_key(&unmeasured.spawn_id, unmeasured.line_no);
    let encoded = serde_json::to_vec(&unmeasured)?;
    // NOTE: no put_agent_transcript_constellation call here, on purpose.
    let error = db
        .put_agent_transcript_outcome_anchor(&key, &encoded, &unmeasured)
        .err();
    let after = physical_anchors(db, &key, &encoded)?;
    let held = error.is_some() && after.is_empty();
    println!(
        "  un-measured row (no constellation)         -> refused={} physical_rows={} {}",
        error.is_some(),
        after.len(),
        verdict(held)
    );
    if let Some(error) = &error {
        println!("      refusal: {error}");
    }
    if !held {
        failures.push(
            "an anchor was accepted for a row with no constellation, so it would be a dangling \
             write no grounding readback can see"
                .to_owned(),
        );
    }

    // (4) Idempotency: re-grounding an already-grounded row must not create a
    //     second anchor. The backfill re-runs over rows it already touched.
    let mut repeat = base_row(9_004);
    repeat.role = Some(TranscriptRole::Tool);
    repeat.event_kind = Some(AGENT_TRANSCRIPT_TOOL_RESULT_EVENT_KIND.to_owned());
    repeat.tool_calls = vec![TranscriptToolCall {
        tool_name: "tool_result".to_owned(),
        status: None,
        ..TranscriptToolCall::default()
    }];
    let key = agent_transcript_key(&repeat.spawn_id, repeat.line_no);
    let encoded = serde_json::to_vec(&repeat)?;
    db.put_agent_transcript_constellation(&key, &encoded, &repeat)?;
    db.put_agent_transcript_outcome_anchor(&key, &encoded, &repeat)?;
    let once = physical_anchors(db, &key, &encoded)?;
    db.put_agent_transcript_outcome_anchor(&key, &encoded, &repeat)?;
    let twice = physical_anchors(db, &key, &encoded)?;
    let held = once.len() == 1 && twice.len() == 1;
    println!(
        "  re-ground the same row twice               -> after_1={} after_2={} {}",
        once.len(),
        twice.len(),
        verdict(held)
    );
    if !held {
        failures.push(format!(
            "re-grounding was not idempotent: {} anchor(s) after one write, {} after two",
            once.len(),
            twice.len()
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decoded physical Calyx `Anchors` CF rows for one source row. This is the
/// source of truth every assertion in this instrument reads.
fn physical_anchors(
    db: &Db,
    key: &[u8],
    encoded: &[u8],
) -> Result<Vec<CalyxAnchorRow>, Box<dyn Error>> {
    Ok(db
        .calyx_anchor_scan_for_source(cf::CF_AGENT_TRANSCRIPTS, key, encoded)?
        .anchors)
}

fn base_row(line_no: u64) -> AgentTranscriptRecord {
    let mut record = AgentTranscriptRecord::new(
        1_785_100_000_000_000_000 + line_no,
        "agent-spawn-fsv1926".to_owned(),
        line_no,
        TranscriptSource::ClaudeSessionJsonl,
        128,
        sha256_hex(format!("fsv-1926-boundary-{line_no}").as_bytes()),
    );
    record.status = TranscriptParseStatus::Parsed;
    record
}

fn verdict(held: bool) -> &'static str {
    if held { "OK" } else { "FAIL" }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_owned()
    } else {
        text[..max].to_owned()
    }
}

fn bounded(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    text.chars().take(512).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn claude_projects_root() -> Result<PathBuf, Box<dyn Error>> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| "neither USERPROFILE nor HOME is set, so the real corpus cannot be located")?;
    let root = Path::new(&home).join(".claude").join("projects");
    if !root.is_dir() {
        return Err(format!(
            "{} does not exist; this instrument measures the real corpus and will not \
             substitute a synthetic one",
            root.display()
        )
        .into());
    }
    Ok(root)
}

fn session_files(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut files = Vec::new();
    for project in std::fs::read_dir(root)? {
        let project = project?.path();
        if !project.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&project)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn scratch_dir() -> Result<PathBuf, Box<dyn Error>> {
    let arg = std::env::args().nth(1).ok_or(
        "usage: transcript_outcome_anchor_fsv <empty-scratch-dir>\n\
         the directory must be empty: this instrument opens a fresh vault and must not \
         inherit any prior state",
    )?;
    let dir = PathBuf::from(arg);
    std::fs::create_dir_all(&dir)?;
    if std::fs::read_dir(&dir)?.next().is_some() {
        return Err(format!("{} is not empty", dir.display()).into());
    }
    Ok(dir)
}
