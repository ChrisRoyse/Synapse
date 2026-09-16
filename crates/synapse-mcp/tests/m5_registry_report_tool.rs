use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde_json::{Value, json};
use synapse_storage::cf;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn profile_registry_query_report_summarizes_registry_quality_consent_and_quarantine()
-> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let manifests = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_DEBUG_TOOLS", "1"),
            ("SYNAPSE_DB", db_path_string.as_str()),
        ],
    )
    .await?;

    assert_empty_report(&mut client).await?;
    install_curated_package(&mut client, manifests.path()).await?;
    write_stale_luanti_action_row(&mut client).await?;
    refresh_luanti_quality_from_stale_row(&mut client).await?;
    enable_luanti_export_consent(&mut client).await?;
    quarantine_bad_signature_package(&mut client, manifests.path()).await?;
    assert_populated_report(&mut client).await?;

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

async fn assert_empty_report(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    let empty = structured(
        &client
            .tools_call("profile_registry_query", json!({"view": "report"}))
            .await?,
    )?;
    let empty = empty["report"].clone();
    assert_eq!(empty["row_counts"][cf::CF_PROFILES], 0);
    assert_eq!(array_len(&empty, "installed_profiles")?, 0);
    assert!(has_sot_surface(&empty, "registry_rows_prefix"));
    Ok(())
}

async fn install_curated_package(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<()> {
    let manifest = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/curated_luanti_package_manifest.toml",
        manifest_dir,
        "curated.toml",
    )?;
    let install = structured(
        &client
            .tools_call(
                "profile_registry_install",
                json!({"manifest_path": manifest.display().to_string()}),
            )
            .await?,
    )?;
    assert_eq!(install["wrote_rows"], true);
    Ok(())
}

async fn write_stale_luanti_action_row(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    let response = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_ACTION_LOG,
                    "key_prefix": "registry-report-audit",
                    "rows": 1,
                    "value_bytes": 0,
                    "ts_ns_start": 1,
                    "value_json": {
                        "tool": "act_press",
                        "status": "ok",
                        "foreground": {
                            "profile_id": "luanti.minetest",
                            "profile_schema_version": 1,
                            "process_name": "luanti.exe"
                        },
                        "details": {"response": {"backend_used": "software"}}
                    }
                }),
            )
            .await?,
    )?;
    assert_eq!(response["rows_added"], 1);
    Ok(())
}

async fn refresh_luanti_quality_from_stale_row(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    let quality = structured(
        &client
            .tools_call(
                "profile_quality_refresh",
                json!({"profile_id": "luanti.minetest", "stale_after_ns": 1}),
            )
            .await?,
    )?;
    assert_eq!(quality["snapshot"]["source"]["audit_rows_stale"], 1);
    Ok(())
}

async fn enable_luanti_export_consent(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    let output = TempDir::new()?;
    let export = structured(
        &client
            .tools_call(
                "audit_export_bundle",
                json!({
                    "profile_id": "luanti.minetest",
                    "output_path": output.path().display().to_string(),
                    "consent": {
                        "enabled": true,
                        "redaction_policy": "strict"
                    }
                }),
            )
            .await?,
    )?;
    assert_eq!(export["consent_row"]["value"]["enabled"], true);
    Ok(())
}

async fn quarantine_bad_signature_package(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<()> {
    let manifest = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/profile_package_manifest/edge_bad_signature_package_manifest.toml",
        manifest_dir,
        "bad-signature.toml",
    )?;
    let trust_error = client
        .tools_call_error(
            "profile_registry_install",
            json!({
                "manifest_path": manifest.display().to_string(),
                "trust_policy": "signed_required"
            }),
        )
        .await?;
    assert_eq!(
        trust_error["data"]["code"],
        "PROFILE_TRUST_VERIFICATION_FAILED"
    );
    Ok(())
}

async fn assert_populated_report(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    let report = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "report", "profile_id": "luanti.minetest", "max_audit_rows": 10}),
            )
            .await?,
    )?;
    let report = report["report"].clone();
    assert_eq!(
        report["installed_profiles"][0]["profile_id"],
        "luanti.minetest"
    );
    assert_eq!(
        report["curated_targets"][0]["row_key"],
        "profile_registry/v1/curated_target/starter.v1/luanti.minetest"
    );
    assert_eq!(
        report["quality_snapshots"][0]["stale_evidence_present"],
        true
    );
    assert_eq!(
        report["consent"][0]["audit_export_status"],
        "local_export_ready"
    );
    assert_eq!(
        report["quarantined_packages"][0]["quarantine_reason"],
        "signature_invalid"
    );
    assert_eq!(report["recent_audit"]["matching_rows"], 1);
    assert!(has_sot_surface(&report, "profile_package_quarantine"));
    assert!(has_sot_surface(&report, "profile_quality_snapshot"));
    assert!(has_sot_surface(&report, "audit_export_consent"));
    Ok(())
}

fn prepare_manifest(
    fixture_relative_path: &str,
    output_dir: &Path,
    output_name: &str,
) -> anyhow::Result<PathBuf> {
    let root = repo_root()?;
    let profile_toml = root
        .join("crates/synapse-profiles/profiles/luanti.minetest.toml")
        .canonicalize()
        .context("canonicalize Luanti profile TOML")?;
    let source = fs::read_to_string(root.join(fixture_relative_path))
        .with_context(|| format!("read fixture manifest {fixture_relative_path}"))?;
    let rewritten = source.replace(
        "profile_toml = \"crates/synapse-profiles/profiles/luanti.minetest.toml\"",
        &format!("profile_toml = \"{}\"", toml_path(&profile_toml)),
    );
    let path = output_dir.join(output_name);
    fs::write(&path, rewritten)?;
    Ok(path)
}

fn repo_root() -> anyhow::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .context("canonicalize repo root")
}

fn toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn has_sot_surface(report: &Value, surface: &str) -> bool {
    report["source_of_truth"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["surface"] == surface))
}

fn array_len(value: &Value, field: &str) -> anyhow::Result<usize> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::len)
        .with_context(|| format!("{field} array missing"))
}

fn structured(response: &Value) -> anyhow::Result<Value> {
    let content = response
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .context("content[0] missing")?;
    let text = content
        .get("text")
        .and_then(Value::as_str)
        .context("content[0].text missing")?;
    serde_json::from_str(text).context("parse tool response json")
}
