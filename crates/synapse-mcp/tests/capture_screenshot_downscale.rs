//! Supporting real-process integration evidence for #1336 screenshot
//! pixel-budget downscaling; manual FSV remains separate.
//!
//! Drives the real `synapse-mcp` binary over the stdio MCP protocol, captures a
//! real screen region with a downscale budget, and then INDEPENDENTLY decodes the
//! written PNG file (the source of truth on disk) to prove the pixel dimensions
//! actually changed. Return values alone are not trusted: every assertion is
//! cross-checked against the decoded image bytes.
//!
//! Synthetic, deterministic inputs (X+X=Y): a `1000x500` region under a 400 px
//! long-edge budget MUST land at exactly `400x200` (scale 0.4); a `1000x1000`
//! region under a `250000` px budget MUST land at `500x500` (scale 0.5); a budget
//! larger than the capture is a no-op; a zero budget fails loudly.
#![allow(clippy::too_many_lines)]

use anyhow::{Context, bail, ensure};
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;

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

fn u64_field(value: &Value, key: &str) -> anyhow::Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("response missing u64 field {key}: {value}"))
}

fn f64_field(value: &Value, key: &str) -> anyhow::Result<f64> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .with_context(|| format!("response missing f64 field {key}: {value}"))
}

/// Decode the written file from disk and return its true (width, height). This is
/// the Source of Truth — independent of anything the tool reported.
fn decoded_dimensions(path: &str) -> anyhow::Result<(u32, u32)> {
    let bytes = std::fs::read(path).with_context(|| format!("read written screenshot {path}"))?;
    ensure!(!bytes.is_empty(), "written screenshot {path} is empty");
    let image = image::load_from_memory(&bytes)
        .with_context(|| format!("decode written screenshot {path}"))?;
    Ok(image::GenericImageView::dimensions(&image))
}

#[tokio::test]
async fn capture_screenshot_downscale_writes_budgeted_pixels() -> anyhow::Result<()> {
    let mut client = StdioMcpClient::launch_and_init().await?;
    let dir = tempfile::tempdir()?;

    // --- Happy path: 1000x500 native, long-edge budget 400 -> 400x200, scale 0.4 ---
    let happy_path = dir.path().join("happy.png");
    let happy = structured(
        &client
            .tools_call(
                "capture_screenshot",
                json!({
                    "path": happy_path.to_string_lossy(),
                    "region": { "x": 0, "y": 0, "w": 1000, "h": 500 },
                    "overwrite": true,
                    "max_long_edge": 400
                }),
            )
            .await?,
    )?;
    eprintln!("happy response = {happy}");
    assert_eq!(u64_field(&happy, "native_width")?, 1000, "native_width");
    assert_eq!(u64_field(&happy, "native_height")?, 500, "native_height");
    assert_eq!(u64_field(&happy, "width")?, 400, "reported width");
    assert_eq!(u64_field(&happy, "height")?, 200, "reported height");
    let happy_scale = f64_field(&happy, "scale")?;
    ensure!(
        (happy_scale - 0.4).abs() < 1e-9,
        "scale should be 0.4, was {happy_scale}"
    );
    // SOURCE OF TRUTH: decode the file from disk.
    let happy_path_str = happy
        .get("path")
        .and_then(Value::as_str)
        .context("response path missing")?;
    let (dw, dh) = decoded_dimensions(happy_path_str)?;
    assert_eq!(
        (dw, dh),
        (400, 200),
        "decoded file dimensions must match the budget, got {dw}x{dh}"
    );

    // --- Pixel-area budget: 1000x1000 = 1_000_000 px, budget 250_000 -> 500x500 ---
    let area_path = dir.path().join("area.png");
    let area = structured(
        &client
            .tools_call(
                "capture_screenshot",
                json!({
                    "path": area_path.to_string_lossy(),
                    "region": { "x": 0, "y": 0, "w": 1000, "h": 1000 },
                    "overwrite": true,
                    "max_pixels": 250_000
                }),
            )
            .await?,
    )?;
    eprintln!("area response = {area}");
    assert_eq!(u64_field(&area, "width")?, 500, "area width");
    assert_eq!(u64_field(&area, "height")?, 500, "area height");
    let area_scale = f64_field(&area, "scale")?;
    ensure!(
        (area_scale - 0.5).abs() < 1e-9,
        "area scale should be 0.5, was {area_scale}"
    );
    let (aw, ah) = decoded_dimensions(
        area.get("path")
            .and_then(Value::as_str)
            .context("area path missing")?,
    )?;
    assert_eq!((aw, ah), (500, 500), "decoded area dims, got {aw}x{ah}");

    // --- No-op: budget larger than the capture leaves pixels untouched, scale 1.0 ---
    let noop_path = dir.path().join("noop.png");
    let noop = structured(
        &client
            .tools_call(
                "capture_screenshot",
                json!({
                    "path": noop_path.to_string_lossy(),
                    "region": { "x": 0, "y": 0, "w": 800, "h": 600 },
                    "overwrite": true,
                    "max_pixels": 10_000_000u64
                }),
            )
            .await?,
    )?;
    eprintln!("noop response = {noop}");
    assert_eq!(u64_field(&noop, "width")?, 800, "noop width");
    assert_eq!(u64_field(&noop, "height")?, 600, "noop height");
    assert_eq!(u64_field(&noop, "native_width")?, 800, "noop native_width");
    let noop_scale = f64_field(&noop, "scale")?;
    ensure!(
        (noop_scale - 1.0).abs() < 1e-9,
        "noop scale should be 1.0, was {noop_scale}"
    );
    let (nw, nh) = decoded_dimensions(
        noop.get("path")
            .and_then(Value::as_str)
            .context("noop path missing")?,
    )?;
    assert_eq!((nw, nh), (800, 600), "decoded noop dims, got {nw}x{nh}");

    // --- Edge: zero budget must fail loudly, not silently capture full-res ---
    let bad_path = dir.path().join("bad.png");
    let error = client
        .tools_call_error(
            "capture_screenshot",
            json!({
                "path": bad_path.to_string_lossy(),
                "region": { "x": 0, "y": 0, "w": 100, "h": 100 },
                "overwrite": true,
                "max_long_edge": 0
            }),
        )
        .await?;
    eprintln!("zero-budget error = {error}");
    let code = error
        .get("data")
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str);
    ensure!(
        code == Some("TOOL_PARAMS_INVALID"),
        "zero budget should fail with TOOL_PARAMS_INVALID, got {error}"
    );
    ensure!(
        !bad_path.exists(),
        "a rejected zero-budget capture must not write a file"
    );

    let status = client.shutdown().await?;
    if !status.success() {
        bail!("daemon exited with non-success status: {status:?}");
    }
    Ok(())
}
