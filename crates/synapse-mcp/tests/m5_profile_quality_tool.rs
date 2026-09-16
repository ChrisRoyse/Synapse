use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use serde_json::{Value, json};
use synapse_core::{PROFILE_SCHEMA_VERSION, SCHEMA_VERSION, error_codes};
use synapse_storage::{Db, cf, encode_json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn profile_quality_refresh_persists_explainable_snapshot() -> anyhow::Result<()> {
    let profiles = TempDir::new()?;
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    write_profile(&profiles.path().join("quality.synthetic.toml"))?;
    write_audit_rows(&db_path)?;

    let profile_dir = profiles.path().to_string_lossy().to_string();
    let db_path_string = db_path.to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_DB", db_path_string.as_str()),
            ("SYNAPSE_PROFILE_DIR", profile_dir.as_str()),
        ],
    )
    .await?;

    let before = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(before["cf_row_counts"][cf::CF_PROFILES], 0);
    assert_eq!(before["cf_row_counts"][cf::CF_ACTION_LOG], 7);
    assert_eq!(before["cf_row_counts"][cf::CF_OBSERVATIONS], 2);
    assert_eq!(before["cf_row_counts"][cf::CF_EVENTS], 2);

    let first = structured(
        &client
            .tools_call(
                "profile_quality_refresh",
                json!({
                    "profile_id": "quality.synthetic",
                    "manual_fsv_evidence_ref": "https://github.com/ChrisRoyse/Synapse/issues/498#synthetic"
                }),
            )
            .await?,
    )?;
    assert_first_snapshot(&first);

    let after = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(after["cf_row_counts"][cf::CF_PROFILES], 1);

    let second = structured(
        &client
            .tools_call(
                "profile_quality_refresh",
                json!({
                    "profile_id": "quality.synthetic",
                    "manual_fsv_evidence_ref": "https://github.com/ChrisRoyse/Synapse/issues/498#synthetic"
                }),
            )
            .await?,
    )?;
    assert_eq!(second["wrote_snapshot"], false);
    assert_eq!(
        second["snapshot"]["evidence_hash"],
        first["snapshot"]["evidence_hash"]
    );
    assert_eq!(
        second["snapshot"]["generated_at_ns"],
        first["snapshot"]["generated_at_ns"]
    );

    let status = client.shutdown().await?;
    assert!(status.success());

    let stored = read_profile_quality_row(&db_path, "quality.synthetic")?;
    assert_eq!(stored["profile_id"], "quality.synthetic");
    assert_eq!(stored["counts"]["quality_eligible_ok_rows"], 1);
    assert_eq!(stored["counts"]["quality_eligible_error_rows"], 1);
    assert_eq!(stored["versioning"]["mixed_profile_schema_versions"], true);
    assert_eq!(stored["runtime_evidence"]["event_rows_profile_relevant"], 1);
    Ok(())
}

fn write_profile(path: &Path) -> anyhow::Result<()> {
    fs::write(
        path,
        r#"
id = "quality.synthetic"
label = "Quality Synthetic"
schema_version = 2
use_scope = "operator_owned_test"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "notepad.exe"

[metadata]
"registry.quality_signal" = "profile_quality.quality.synthetic"
"registry.compatibility_target" = "quality.synthetic.target"
"#,
    )?;
    Ok(())
}

fn assert_first_snapshot(first: &Value) {
    assert_eq!(first["wrote_snapshot"], true);
    assert_eq!(first["snapshot"]["source"]["audit_rows_scanned"], 7);
    assert_eq!(first["snapshot"]["source"]["audit_rows_decode_failed"], 1);
    assert_eq!(first["snapshot"]["source"]["audit_rows_stale"], 1);
    assert_eq!(
        first["snapshot"]["source"]["audit_rows_profile_relevant"],
        5
    );
    assert_eq!(first["snapshot"]["counts"]["quality_eligible_ok_rows"], 1);
    assert_eq!(
        first["snapshot"]["counts"]["quality_eligible_error_rows"],
        1
    );
    assert_eq!(first["snapshot"]["counts"]["denied_rows"], 1);
    assert_eq!(first["snapshot"]["counts"]["backend_unavailable_rows"], 1);
    assert_eq!(first["snapshot"]["counts"]["launch_error_rows"], 1);
    assert_eq!(
        first["snapshot"]["compatibility"]["profile_mismatch_rows"],
        1
    );
    assert_eq!(
        first["snapshot"]["versioning"]["current_profile_schema_version"],
        PROFILE_SCHEMA_VERSION
    );
    assert_eq!(
        first["snapshot"]["versioning"]["rows_with_profile_schema_version"],
        5
    );
    assert_eq!(first["snapshot"]["versioning"]["current_version_rows"], 4);
    assert_eq!(first["snapshot"]["versioning"]["older_version_rows"], 1);
    assert_eq!(first["snapshot"]["versioning"]["newer_version_rows"], 0);
    assert_eq!(first["snapshot"]["versioning"]["unknown_version_rows"], 0);
    assert_eq!(
        first["snapshot"]["versioning"]["mixed_profile_schema_versions"],
        true
    );
    assert_eq!(first["snapshot"]["redaction"]["local_only"], true);
    assert_eq!(first["snapshot"]["contribution"]["export_allowed"], false);
    assert_eq!(
        first["snapshot"]["manual_fsv_evidence_ref"],
        "https://github.com/ChrisRoyse/Synapse/issues/498#synthetic"
    );
    assert_eq!(
        first["snapshot"]["runtime_evidence"]["observation_rows_scanned"],
        2
    );
    assert_eq!(
        first["snapshot"]["runtime_evidence"]["observation_rows_profile_relevant"],
        1
    );
    assert_eq!(
        first["snapshot"]["runtime_evidence"]["event_rows_profile_relevant"],
        1
    );
    assert_eq!(
        first["snapshot"]["runtime_evidence"]["observed_target_ids"]["quality.synthetic.target"],
        2
    );
    assert_eq!(
        first["snapshot"]["runtime_evidence"]["observed_log_event_kinds"]["runtime.log.cast_begins"],
        2
    );
}

fn write_audit_rows(db_path: &Path) -> anyhow::Result<()> {
    let now = now_ns();
    let rows = vec![
        action_row(
            now - 6_000,
            0,
            "act_press",
            "started",
            None,
            ProfileRefs::same("quality.synthetic"),
            ProfileSchemaVersions::same(PROFILE_SCHEMA_VERSION),
        )?,
        action_row(
            now - 5_000,
            1,
            "act_press",
            "ok",
            None,
            ProfileRefs::same("quality.synthetic"),
            ProfileSchemaVersions::same(PROFILE_SCHEMA_VERSION),
        )?,
        action_row(
            now - 4_000,
            2,
            "act_press",
            "error",
            Some(error_codes::ACTION_BACKEND_UNAVAILABLE),
            ProfileRefs::same("quality.synthetic"),
            ProfileSchemaVersions::same(PROFILE_SCHEMA_VERSION),
        )?,
        action_row(
            now - 3_000,
            3,
            "act_press",
            "denied",
            Some(error_codes::SAFETY_PROFILE_ACTION_DENIED),
            ProfileRefs::same("quality.synthetic"),
            ProfileSchemaVersions::same(PROFILE_SCHEMA_VERSION),
        )?,
        action_row(
            now - 2_000,
            4,
            "act_launch",
            "error",
            Some("LAUNCH_FAILED"),
            ProfileRefs {
                active: "quality.synthetic",
                foreground: "other.profile",
            },
            ProfileSchemaVersions {
                active: 1,
                foreground: PROFILE_SCHEMA_VERSION + 1,
            },
        )?,
        stale_action_row(1, 5)?,
        (audit_key(now - 1_000, 6), b"not-json-audit-row".to_vec()),
    ];
    {
        let db = Db::open(db_path, SCHEMA_VERSION)?;
        db.put_batch(cf::CF_ACTION_LOG, rows)?;
        db.put_batch(
            cf::CF_OBSERVATIONS,
            vec![
                observation_row(now - 1_800, 7, "quality.synthetic", "notepad.exe")?,
                observation_row(now - 1_700, 8, "other.profile", "calc.exe")?,
            ],
        )?;
        db.put_batch(
            cf::CF_EVENTS,
            vec![
                event_row(
                    now - 1_600,
                    9,
                    "quality.synthetic",
                    "runtime.log.cast_begins",
                )?,
                event_row(now - 1_500, 10, "other.profile", "perception.observed")?,
            ],
        )?;
        db.flush()?;
    }
    Ok(())
}

fn action_row(
    ts_ns: u64,
    seq: u32,
    tool: &str,
    status: &str,
    error_code: Option<&str>,
    profiles: ProfileRefs<'_>,
    schema_versions: ProfileSchemaVersions,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let value = json!({
        "schema_version": 1,
        "audit_id": format!("{ts_ns:020}-{seq:010}"),
        "ts_ns": ts_ns,
        "seq": seq,
        "tool": tool,
        "status": status,
        "error_code": error_code,
        "foreground": {
            "process_name": if profiles.foreground == "quality.synthetic" { "notepad.exe" } else { "calc.exe" },
            "process_path": "C:\\redacted-by-quality-snapshot\\synthetic.exe",
            "window_title": "Synthetic title not copied into quality snapshot",
            "profile_id": profiles.foreground,
            "profile_schema_version": schema_versions.foreground,
        },
        "active_profile_id": profiles.active,
        "active_profile_schema_version": schema_versions.active,
        "details": {
            "response": {
                "backend": "software"
            }
        }
    });
    Ok((
        audit_key(ts_ns, seq),
        encode_json(&value).context("audit row should encode")?,
    ))
}

fn observation_row(
    ts_ns: u64,
    seq: u32,
    profile_id: &str,
    process_name: &str,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let value = json!({
        "schema_version": 1,
        "observation_id": format!("observe-{ts_ns:020}-{seq:010}"),
        "ts_ns": ts_ns,
        "foreground": {
            "profile_id": profile_id,
            "process_name": process_name
        },
        "recent_events": [
            {
                "seq": 1,
                "kind": "runtime.log.cast_begins",
                "data_excerpt": {
                    "redacted": true,
                    "summary": "synthetic compact event"
                }
            }
        ]
    });
    Ok((
        audit_key(ts_ns, seq),
        encode_json(&value).context("observation row should encode")?,
    ))
}

fn event_row(
    ts_ns: u64,
    seq: u32,
    profile_id: &str,
    kind: &str,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let value = json!({
        "schema_version": 1,
        "event_id": format!("event-{ts_ns:020}-{seq:010}"),
        "ts_ns": ts_ns,
        "audit_context": {
            "profile_id": profile_id,
            "app_context": {
                "process_name": "notepad.exe",
                "target_id": "quality.synthetic.target"
            }
        },
        "kind": kind,
        "data": {
            "profile_id": profile_id,
            "process_name": "notepad.exe"
        }
    });
    Ok((
        audit_key(ts_ns, seq),
        encode_json(&value).context("event row should encode")?,
    ))
}

fn stale_action_row(ts_ns: u64, seq: u32) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    action_row(
        ts_ns,
        seq,
        "act_press",
        "ok",
        None,
        ProfileRefs::same("quality.synthetic"),
        ProfileSchemaVersions::same(PROFILE_SCHEMA_VERSION),
    )
}

#[derive(Clone, Copy)]
struct ProfileRefs<'a> {
    active: &'a str,
    foreground: &'a str,
}

impl<'a> ProfileRefs<'a> {
    const fn same(profile_id: &'a str) -> Self {
        Self {
            active: profile_id,
            foreground: profile_id,
        }
    }
}

#[derive(Clone, Copy)]
struct ProfileSchemaVersions {
    active: u32,
    foreground: u32,
}

impl ProfileSchemaVersions {
    const fn same(version: u32) -> Self {
        Self {
            active: version,
            foreground: version,
        }
    }
}

fn audit_key(ts_ns: u64, seq: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&ts_ns.to_be_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn structured(response: &Value) -> anyhow::Result<Value> {
    if let Some(value) = response.get("structuredContent") {
        return Ok(value.clone());
    }
    let text = response
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .context("structured content missing")?;
    serde_json::from_str(text).context("parse text content")
}

fn read_profile_quality_row(db_path: &Path, profile_id: &str) -> anyhow::Result<Value> {
    let key = format!("profile_quality/v1/{profile_id}").into_bytes();
    let db = Db::open(db_path, SCHEMA_VERSION)?;
    let (_, value) = db
        .scan_cf(cf::CF_PROFILES)?
        .into_iter()
        .find(|(row_key, _)| row_key == &key)
        .context("profile quality row missing")?;
    serde_json::from_slice(&value).context("decode profile quality row")
}

fn now_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    u64::try_from(nanos).unwrap_or(u64::MAX)
}
