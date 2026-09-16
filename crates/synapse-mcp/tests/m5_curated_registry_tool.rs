use std::{
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use anyhow::Context;
use serde_json::{Value, json};
use synapse_storage::cf;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

static CURATED_REGISTRY_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

#[tokio::test]
async fn curated_package_install_writes_seed_target_row() -> anyhow::Result<()> {
    let _guard = CURATED_REGISTRY_TEST_LOCK.lock().await;
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[("SYNAPSE_DB", db_path_string.as_str())],
    )
    .await?;

    let before = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(before["cf_row_counts"][cf::CF_PROFILES], 0);

    let manifests = TempDir::new()?;
    let manifest_path = install_curated_manifest(&mut client, manifests.path()).await?;
    assert_curated_search_and_inspect(&mut client).await?;
    assert_duplicate_is_idempotent(&mut client, &manifest_path).await?;
    assert_edge_manifests_fail_closed(&mut client, manifests.path()).await?;

    let after = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(after["cf_row_counts"][cf::CF_PROFILES], 6);
    assert_eq!(after["cf_row_counts"][cf::CF_KV], 1);

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

#[tokio::test]
async fn curated_productivity_package_accepts_single_token_profile_id() -> anyhow::Result<()> {
    let _guard = CURATED_REGISTRY_TEST_LOCK.lock().await;
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[("SYNAPSE_DB", db_path_string.as_str())],
    )
    .await?;

    let manifests = TempDir::new()?;
    let manifest_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/curated_notepad_package_manifest.toml",
        manifests.path(),
        "curated-notepad.toml",
    )?;
    let install = structured(
        &client
            .tools_call(
                "profile_registry_install",
                json!({"manifest_path": manifest_path.display().to_string()}),
            )
            .await?,
    )?;
    assert_eq!(install["wrote_rows"], true);
    assert_eq!(install["profile_id"], "notepad");
    let row_keys = install["cf_profile_row_keys"]
        .as_array()
        .context("cf_profile_row_keys missing")?;
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/curated_target/starter.v1/notepad.windows")
    }));

    let inspect = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "inspect", "row_key": "profile_registry/v1/curated_target/starter.v1/notepad.windows"}),
            )
            .await?,
    )?;
    let value = &inspect["inspect"]["row"]["value"];
    assert_eq!(value["row_kind"], "curated_profile_target");
    assert_eq!(value["target_id"], "notepad.windows");
    assert_eq!(value["profile_id"], "notepad");
    assert_eq!(value["use_scope"], "productivity");
    assert_eq!(value["quality_signal"], "profile_quality.notepad");

    let mismatch_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_notepad_profile_mismatch_manifest.toml",
        manifests.path(),
        "notepad-profile-mismatch.toml",
    )?;
    let mismatch = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": mismatch_path.display().to_string()}),
        )
        .await?;
    assert_eq!(mismatch["data"]["code"], "TOOL_PARAMS_INVALID");
    assert_eq!(mismatch["data"]["reason"], "profile_toml_id_mismatch");

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

#[tokio::test]
async fn curated_vscode_package_writes_editor_targets_and_edges() -> anyhow::Result<()> {
    let _guard = CURATED_REGISTRY_TEST_LOCK.lock().await;
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[("SYNAPSE_DB", db_path_string.as_str())],
    )
    .await?;

    let before = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(before["cf_row_counts"][cf::CF_PROFILES], 0);
    assert_eq!(before["cf_row_counts"][cf::CF_KV], 0);

    let manifests = TempDir::new()?;
    let manifest_path = install_vscode_curated_manifest(&mut client, manifests.path()).await?;
    assert_vscode_curated_search_and_inspect(&mut client).await?;
    assert_duplicate_is_idempotent(&mut client, &manifest_path).await?;
    let after_duplicate = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(after_duplicate["cf_row_counts"][cf::CF_PROFILES], 7);
    assert_eq!(after_duplicate["cf_row_counts"][cf::CF_KV], 1);

    assert_vscode_edge_manifests_fail_closed(&mut client, manifests.path()).await?;
    let after_edges = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(after_edges["cf_row_counts"][cf::CF_PROFILES], 7);
    assert_eq!(after_edges["cf_row_counts"][cf::CF_KV], 1);

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

#[tokio::test]
async fn curated_terminal_package_writes_shell_target_and_edges() -> anyhow::Result<()> {
    let _guard = CURATED_REGISTRY_TEST_LOCK.lock().await;
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[("SYNAPSE_DB", db_path_string.as_str())],
    )
    .await?;

    let before = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(before["cf_row_counts"][cf::CF_PROFILES], 0);
    assert_eq!(before["cf_row_counts"][cf::CF_KV], 0);

    let manifests = TempDir::new()?;
    let manifest_path = install_terminal_curated_manifest(&mut client, manifests.path()).await?;
    assert_terminal_curated_search_and_inspect(&mut client).await?;
    assert_duplicate_is_idempotent(&mut client, &manifest_path).await?;
    let after_duplicate = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(after_duplicate["cf_row_counts"][cf::CF_PROFILES], 6);
    assert_eq!(after_duplicate["cf_row_counts"][cf::CF_KV], 1);

    assert_terminal_edge_manifests_fail_closed(&mut client, manifests.path()).await?;
    let after_edges = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(after_edges["cf_row_counts"][cf::CF_PROFILES], 6);
    assert_eq!(after_edges["cf_row_counts"][cf::CF_KV], 1);

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

#[tokio::test]
async fn curated_chrome_package_writes_browser_targets_and_edges() -> anyhow::Result<()> {
    let _guard = CURATED_REGISTRY_TEST_LOCK.lock().await;
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[("SYNAPSE_DB", db_path_string.as_str())],
    )
    .await?;

    let before = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(before["cf_row_counts"][cf::CF_PROFILES], 0);
    assert_eq!(before["cf_row_counts"][cf::CF_KV], 0);

    let manifests = TempDir::new()?;
    let manifest_path = install_chrome_curated_manifest(&mut client, manifests.path()).await?;
    assert_chrome_curated_search_and_inspect(&mut client).await?;
    assert_duplicate_is_idempotent(&mut client, &manifest_path).await?;
    let after_duplicate = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(after_duplicate["cf_row_counts"][cf::CF_PROFILES], 8);
    assert_eq!(after_duplicate["cf_row_counts"][cf::CF_KV], 1);

    assert_chrome_edge_manifests_fail_closed(&mut client, manifests.path()).await?;
    let after_edges = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(after_edges["cf_row_counts"][cf::CF_PROFILES], 8);
    assert_eq!(after_edges["cf_row_counts"][cf::CF_KV], 1);

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

#[tokio::test]
async fn curated_minecraft_package_writes_game_target_and_edges() -> anyhow::Result<()> {
    let _guard = CURATED_REGISTRY_TEST_LOCK.lock().await;
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[("SYNAPSE_DB", db_path_string.as_str())],
    )
    .await?;

    let before = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(before["cf_row_counts"][cf::CF_PROFILES], 0);
    assert_eq!(before["cf_row_counts"][cf::CF_KV], 0);

    let manifests = TempDir::new()?;
    let manifest_path = install_minecraft_curated_manifest(&mut client, manifests.path()).await?;
    assert_minecraft_curated_search_and_inspect(&mut client).await?;
    assert_duplicate_is_idempotent(&mut client, &manifest_path).await?;
    let after_duplicate = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(after_duplicate["cf_row_counts"][cf::CF_PROFILES], 6);
    assert_eq!(after_duplicate["cf_row_counts"][cf::CF_KV], 1);

    assert_minecraft_edge_manifests_fail_closed(&mut client, manifests.path()).await?;
    let after_edges = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(after_edges["cf_row_counts"][cf::CF_PROFILES], 6);
    assert_eq!(after_edges["cf_row_counts"][cf::CF_KV], 1);

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

async fn install_curated_manifest(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let manifest_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/curated_luanti_package_manifest.toml",
        manifest_dir,
        "curated.toml",
    )?;
    let install = structured(
        &client
            .tools_call(
                "profile_registry_install",
                json!({"manifest_path": manifest_path.display().to_string()}),
            )
            .await?,
    )?;
    assert_eq!(install["wrote_rows"], true);
    assert_eq!(install["idempotent"], false);
    let row_keys = install["cf_profile_row_keys"]
        .as_array()
        .context("cf_profile_row_keys missing")?;
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/curated_target/starter.v1/luanti.minetest")
    }));
    Ok(manifest_path)
}

async fn assert_curated_search_and_inspect(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    let search = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "search", "row_kind": "curated_profile_target"}),
            )
            .await?,
    )?;
    assert_eq!(search["search"]["total_matched"], 1);
    assert_eq!(
        search["search"]["rows"][0]["key"],
        "profile_registry/v1/curated_target/starter.v1/luanti.minetest"
    );
    let inspect = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "inspect", "row_key": "profile_registry/v1/curated_target/starter.v1/luanti.minetest"}),
            )
            .await?,
    )?;
    let value = &inspect["inspect"]["row"]["value"];
    assert_eq!(value["row_kind"], "curated_profile_target");
    assert_eq!(value["seed_set_id"], "starter.v1");
    assert_eq!(value["target_id"], "luanti.minetest");
    assert_eq!(value["profile_id"], "luanti.minetest");
    assert_eq!(value["backlog_issue"], "#471,#472,#473,#474,#475,#476");
    let fsv = value["minimum_manual_fsv"]
        .as_array()
        .context("minimum_manual_fsv array missing")?;
    assert_eq!(fsv.len(), 8);
    Ok(())
}

async fn install_vscode_curated_manifest(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let manifest_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/curated_vscode_package_manifest.toml",
        manifest_dir,
        "curated-vscode.toml",
    )?;
    let install = structured(
        &client
            .tools_call(
                "profile_registry_install",
                json!({"manifest_path": manifest_path.display().to_string()}),
            )
            .await?,
    )?;
    assert_eq!(install["wrote_rows"], true);
    assert_eq!(install["profile_id"], "vscode");
    let row_keys = install["cf_profile_row_keys"]
        .as_array()
        .context("cf_profile_row_keys missing")?;
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/curated_target/starter.v1/vscode.windows")
    }));
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/compat/vscode.windows/vscode/1.0.0")
    }));
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/compat/vscodium.windows/vscode/1.0.0")
    }));
    Ok(manifest_path)
}

async fn assert_vscode_curated_search_and_inspect(
    client: &mut StdioMcpClient,
) -> anyhow::Result<()> {
    let search = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "search", "row_kind": "curated_profile_target"}),
            )
            .await?,
    )?;
    assert_eq!(search["search"]["total_matched"], 1);
    assert_eq!(
        search["search"]["rows"][0]["key"],
        "profile_registry/v1/curated_target/starter.v1/vscode.windows"
    );
    let inspect = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "inspect", "row_key": "profile_registry/v1/curated_target/starter.v1/vscode.windows"}),
            )
            .await?,
    )?;
    let value = &inspect["inspect"]["row"]["value"];
    assert_eq!(value["row_kind"], "curated_profile_target");
    assert_eq!(value["target_id"], "vscode.windows");
    assert_eq!(value["profile_id"], "vscode");
    assert_eq!(value["use_scope"], "productivity");
    assert_eq!(value["quality_signal"], "profile_quality.vscode");
    assert_eq!(value["app_version"], "1.121.0");
    let fsv = value["minimum_manual_fsv"]
        .as_array()
        .context("minimum_manual_fsv array missing")?;
    assert_eq!(fsv.len(), 10);
    Ok(())
}

async fn install_terminal_curated_manifest(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let manifest_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/curated_terminal_package_manifest.toml",
        manifest_dir,
        "curated-terminal.toml",
    )?;
    let install = structured(
        &client
            .tools_call(
                "profile_registry_install",
                json!({"manifest_path": manifest_path.display().to_string()}),
            )
            .await?,
    )?;
    assert_eq!(install["wrote_rows"], true);
    assert_eq!(install["profile_id"], "terminal");
    let row_keys = install["cf_profile_row_keys"]
        .as_array()
        .context("cf_profile_row_keys missing")?;
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/curated_target/starter.v1/terminal.windows")
    }));
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/compat/terminal.windows/terminal/1.0.0")
    }));
    Ok(manifest_path)
}

async fn assert_terminal_curated_search_and_inspect(
    client: &mut StdioMcpClient,
) -> anyhow::Result<()> {
    let search = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "search", "row_kind": "curated_profile_target"}),
            )
            .await?,
    )?;
    assert_eq!(search["search"]["total_matched"], 1);
    assert_eq!(
        search["search"]["rows"][0]["key"],
        "profile_registry/v1/curated_target/starter.v1/terminal.windows"
    );
    let inspect = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "inspect", "row_key": "profile_registry/v1/curated_target/starter.v1/terminal.windows"}),
            )
            .await?,
    )?;
    let value = &inspect["inspect"]["row"]["value"];
    assert_eq!(value["row_kind"], "curated_profile_target");
    assert_eq!(value["target_id"], "terminal.windows");
    assert_eq!(value["profile_id"], "terminal");
    assert_eq!(value["use_scope"], "productivity");
    assert_eq!(value["quality_signal"], "profile_quality.terminal");
    assert_eq!(
        value["app_version"],
        "package:1.24.11321.0; file:1.24.2605.12001"
    );
    let fsv = value["minimum_manual_fsv"]
        .as_array()
        .context("minimum_manual_fsv array missing")?;
    assert_eq!(fsv.len(), 9);
    assert!(fsv.iter().any(|entry| entry == "act_clipboard"));
    assert!(!fsv.iter().any(|entry| entry == "act_type"));
    Ok(())
}

async fn install_chrome_curated_manifest(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let manifest_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/curated_chrome_package_manifest.toml",
        manifest_dir,
        "curated-chrome.toml",
    )?;
    let install = structured(
        &client
            .tools_call(
                "profile_registry_install",
                json!({"manifest_path": manifest_path.display().to_string()}),
            )
            .await?,
    )?;
    assert_eq!(install["wrote_rows"], true);
    assert_eq!(install["profile_id"], "chrome");
    let row_keys = install["cf_profile_row_keys"]
        .as_array()
        .context("cf_profile_row_keys missing")?;
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/curated_target/starter.v1/chrome.windows")
    }));
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/compat/chrome.windows/chrome/1.0.0")
    }));
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/compat/edge.windows/chrome/1.0.0")
    }));
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/compat/chromium.windows/chrome/1.0.0")
    }));
    Ok(manifest_path)
}

async fn assert_chrome_curated_search_and_inspect(
    client: &mut StdioMcpClient,
) -> anyhow::Result<()> {
    let search = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "search", "row_kind": "curated_profile_target"}),
            )
            .await?,
    )?;
    assert_eq!(search["search"]["total_matched"], 1);
    assert_eq!(
        search["search"]["rows"][0]["key"],
        "profile_registry/v1/curated_target/starter.v1/chrome.windows"
    );
    let inspect = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "inspect", "row_key": "profile_registry/v1/curated_target/starter.v1/chrome.windows"}),
            )
            .await?,
    )?;
    let value = &inspect["inspect"]["row"]["value"];
    assert_eq!(value["row_kind"], "curated_profile_target");
    assert_eq!(value["target_id"], "chrome.windows");
    assert_eq!(value["profile_id"], "chrome");
    assert_eq!(value["use_scope"], "productivity");
    assert_eq!(value["quality_signal"], "profile_quality.chrome");
    assert_eq!(value["app_version"], "148.0.7778.179");
    let fsv = value["minimum_manual_fsv"]
        .as_array()
        .context("minimum_manual_fsv array missing")?;
    assert_eq!(fsv.len(), 9);
    assert!(fsv.iter().any(|entry| entry == "act_clipboard"));
    assert!(fsv.iter().any(|entry| entry == "browser_history_readback"));
    Ok(())
}

async fn install_minecraft_curated_manifest(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let manifest_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/curated_minecraft_package_manifest.toml",
        manifest_dir,
        "curated-minecraft.toml",
    )?;
    let install = structured(
        &client
            .tools_call(
                "profile_registry_install",
                json!({"manifest_path": manifest_path.display().to_string()}),
            )
            .await?,
    )?;
    assert_eq!(install["wrote_rows"], true);
    assert_eq!(install["profile_id"], "minecraft.java");
    let row_keys = install["cf_profile_row_keys"]
        .as_array()
        .context("cf_profile_row_keys missing")?;
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/curated_target/starter.v1/minecraft.java")
    }));
    assert!(row_keys.iter().any(|key| {
        key.as_str() == Some("profile_registry/v1/compat/minecraft.java/minecraft.java/1.0.0")
    }));
    Ok(manifest_path)
}

async fn assert_minecraft_curated_search_and_inspect(
    client: &mut StdioMcpClient,
) -> anyhow::Result<()> {
    let search = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "search", "row_kind": "curated_profile_target"}),
            )
            .await?,
    )?;
    assert_eq!(search["search"]["total_matched"], 1);
    assert_eq!(
        search["search"]["rows"][0]["key"],
        "profile_registry/v1/curated_target/starter.v1/minecraft.java"
    );
    let inspect = structured(
        &client
            .tools_call(
                "profile_registry_query",
                json!({"view": "inspect", "row_key": "profile_registry/v1/curated_target/starter.v1/minecraft.java"}),
            )
            .await?,
    )?;
    let value = &inspect["inspect"]["row"]["value"];
    assert_eq!(value["row_kind"], "curated_profile_target");
    assert_eq!(value["target_id"], "minecraft.java");
    assert_eq!(value["profile_id"], "minecraft.java");
    assert_eq!(value["use_scope"], "single_player");
    assert_eq!(value["quality_signal"], "profile_quality.minecraft.java");
    assert_eq!(
        value["safe_default_backend_policy"],
        "auto; operator-owned local single-player worlds only; no remote server automation; no credential/payment/account changes; audit export requires explicit consent"
    );
    let fsv = value["minimum_manual_fsv"]
        .as_array()
        .context("minimum_manual_fsv array missing")?;
    assert_eq!(fsv.len(), 8);
    assert!(
        fsv.iter()
            .any(|entry| entry == "minecraft_world_log_readback")
    );
    Ok(())
}

async fn assert_duplicate_is_idempotent(
    client: &mut StdioMcpClient,
    manifest_path: &Path,
) -> anyhow::Result<()> {
    let duplicate = structured(
        &client
            .tools_call(
                "profile_registry_install",
                json!({"manifest_path": manifest_path.display().to_string()}),
            )
            .await?,
    )?;
    assert_eq!(duplicate["idempotent"], true);
    assert_eq!(duplicate["wrote_rows"], false);
    Ok(())
}

async fn assert_vscode_edge_manifests_fail_closed(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<()> {
    let unknown_scope_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_vscode_unknown_use_scope_manifest.toml",
        manifest_dir,
        "vscode-unknown-scope.toml",
    )?;
    let unknown_scope = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": unknown_scope_path.display().to_string()}),
        )
        .await?;
    assert_eq!(unknown_scope["data"]["code"], "PROFILE_PARSE_ERROR");
    let missing_compat_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_vscode_missing_compatibility_manifest.toml",
        manifest_dir,
        "vscode-missing-compat.toml",
    )?;
    let missing_compat = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": missing_compat_path.display().to_string()}),
        )
        .await?;
    assert_eq!(missing_compat["data"]["code"], "PROFILE_PARSE_ERROR");
    let mismatch_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_vscode_profile_mismatch_manifest.toml",
        manifest_dir,
        "vscode-profile-mismatch.toml",
    )?;
    let mismatch = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": mismatch_path.display().to_string()}),
        )
        .await?;
    assert_eq!(mismatch["data"]["code"], "TOOL_PARAMS_INVALID");
    assert_eq!(mismatch["data"]["reason"], "profile_toml_id_mismatch");
    Ok(())
}

async fn assert_terminal_edge_manifests_fail_closed(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<()> {
    let unknown_scope_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_terminal_unknown_use_scope_manifest.toml",
        manifest_dir,
        "terminal-unknown-scope.toml",
    )?;
    let unknown_scope = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": unknown_scope_path.display().to_string()}),
        )
        .await?;
    assert_eq!(unknown_scope["data"]["code"], "PROFILE_PARSE_ERROR");
    let missing_compat_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_terminal_missing_compatibility_manifest.toml",
        manifest_dir,
        "terminal-missing-compat.toml",
    )?;
    let missing_compat = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": missing_compat_path.display().to_string()}),
        )
        .await?;
    assert_eq!(missing_compat["data"]["code"], "PROFILE_PARSE_ERROR");
    let mismatch_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_terminal_profile_mismatch_manifest.toml",
        manifest_dir,
        "terminal-profile-mismatch.toml",
    )?;
    let mismatch = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": mismatch_path.display().to_string()}),
        )
        .await?;
    assert_eq!(mismatch["data"]["code"], "TOOL_PARAMS_INVALID");
    assert_eq!(mismatch["data"]["reason"], "profile_toml_id_mismatch");
    Ok(())
}

async fn assert_chrome_edge_manifests_fail_closed(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<()> {
    let unknown_scope_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_chrome_unknown_use_scope_manifest.toml",
        manifest_dir,
        "chrome-unknown-scope.toml",
    )?;
    let unknown_scope = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": unknown_scope_path.display().to_string()}),
        )
        .await?;
    assert_eq!(unknown_scope["data"]["code"], "PROFILE_PARSE_ERROR");
    let missing_compat_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_chrome_missing_compatibility_manifest.toml",
        manifest_dir,
        "chrome-missing-compat.toml",
    )?;
    let missing_compat = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": missing_compat_path.display().to_string()}),
        )
        .await?;
    assert_eq!(missing_compat["data"]["code"], "PROFILE_PARSE_ERROR");
    let mismatch_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_chrome_profile_mismatch_manifest.toml",
        manifest_dir,
        "chrome-profile-mismatch.toml",
    )?;
    let mismatch = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": mismatch_path.display().to_string()}),
        )
        .await?;
    assert_eq!(mismatch["data"]["code"], "TOOL_PARAMS_INVALID");
    assert_eq!(mismatch["data"]["reason"], "profile_toml_id_mismatch");
    Ok(())
}

async fn assert_minecraft_edge_manifests_fail_closed(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<()> {
    let unknown_scope_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_minecraft_unknown_use_scope_manifest.toml",
        manifest_dir,
        "minecraft-unknown-scope.toml",
    )?;
    let unknown_scope = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": unknown_scope_path.display().to_string()}),
        )
        .await?;
    assert_eq!(unknown_scope["data"]["code"], "PROFILE_PARSE_ERROR");
    let missing_compat_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_minecraft_missing_compatibility_manifest.toml",
        manifest_dir,
        "minecraft-missing-compat.toml",
    )?;
    let missing_compat = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": missing_compat_path.display().to_string()}),
        )
        .await?;
    assert_eq!(missing_compat["data"]["code"], "PROFILE_PARSE_ERROR");
    let remote_allowed_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_minecraft_remote_allowed_manifest.toml",
        manifest_dir,
        "minecraft-remote-allowed.toml",
    )?;
    let remote_allowed = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": remote_allowed_path.display().to_string()}),
        )
        .await?;
    assert_eq!(remote_allowed["data"]["code"], "PROFILE_PARSE_ERROR");
    let mismatch_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_minecraft_profile_mismatch_manifest.toml",
        manifest_dir,
        "minecraft-profile-mismatch.toml",
    )?;
    let mismatch = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": mismatch_path.display().to_string()}),
        )
        .await?;
    assert_eq!(mismatch["data"]["code"], "TOOL_PARAMS_INVALID");
    assert_eq!(mismatch["data"]["reason"], "profile_toml_id_mismatch");
    Ok(())
}

async fn assert_edge_manifests_fail_closed(
    client: &mut StdioMcpClient,
    manifest_dir: &Path,
) -> anyhow::Result<()> {
    let unknown_scope_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_unknown_use_scope_manifest.toml",
        manifest_dir,
        "unknown-scope.toml",
    )?;
    let unknown_scope = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": unknown_scope_path.display().to_string()}),
        )
        .await?;
    assert_eq!(unknown_scope["data"]["code"], "PROFILE_PARSE_ERROR");
    let missing_compat_path = prepare_manifest(
        "crates/synapse-profiles/tests/fixtures/profile_registry/curated_starter_registry/edge_missing_compatibility_manifest.toml",
        manifest_dir,
        "missing-compat.toml",
    )?;
    let missing_compat = client
        .tools_call_error(
            "profile_registry_install",
            json!({"manifest_path": missing_compat_path.display().to_string()}),
        )
        .await?;
    assert_eq!(missing_compat["data"]["code"], "PROFILE_PARSE_ERROR");
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
    let notepad_profile_toml = root
        .join("crates/synapse-profiles/profiles/notepad.toml")
        .canonicalize()
        .context("canonicalize Notepad profile TOML")?;
    let vscode_profile_toml = root
        .join("crates/synapse-profiles/profiles/vscode.toml")
        .canonicalize()
        .context("canonicalize VS Code profile TOML")?;
    let terminal_profile_toml = root
        .join("crates/synapse-profiles/profiles/terminal.toml")
        .canonicalize()
        .context("canonicalize Windows Terminal profile TOML")?;
    let chrome_profile_toml = root
        .join("crates/synapse-profiles/profiles/chrome.toml")
        .canonicalize()
        .context("canonicalize Chromium browser profile TOML")?;
    let minecraft_profile_toml = root
        .join("crates/synapse-profiles/profiles/minecraft.java.toml")
        .canonicalize()
        .context("canonicalize Minecraft Java profile TOML")?;
    let source = fs::read_to_string(root.join(fixture_relative_path))
        .with_context(|| format!("read fixture manifest {fixture_relative_path}"))?;
    let rewritten = source
        .replace(
            "profile_toml = \"crates/synapse-profiles/profiles/luanti.minetest.toml\"",
            &format!("profile_toml = \"{}\"", toml_path(&profile_toml)),
        )
        .replace(
            "profile_toml = \"crates/synapse-profiles/profiles/notepad.toml\"",
            &format!("profile_toml = \"{}\"", toml_path(&notepad_profile_toml)),
        )
        .replace(
            "profile_toml = \"crates/synapse-profiles/profiles/vscode.toml\"",
            &format!("profile_toml = \"{}\"", toml_path(&vscode_profile_toml)),
        )
        .replace(
            "profile_toml = \"crates/synapse-profiles/profiles/terminal.toml\"",
            &format!("profile_toml = \"{}\"", toml_path(&terminal_profile_toml)),
        )
        .replace(
            "profile_toml = \"crates/synapse-profiles/profiles/chrome.toml\"",
            &format!("profile_toml = \"{}\"", toml_path(&chrome_profile_toml)),
        )
        .replace(
            "profile_toml = \"crates/synapse-profiles/profiles/minecraft.java.toml\"",
            &format!("profile_toml = \"{}\"", toml_path(&minecraft_profile_toml)),
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
