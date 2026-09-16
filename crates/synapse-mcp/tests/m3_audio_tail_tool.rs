use anyhow::Context;
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

const BYTES_PER_SAMPLE: usize = 2;

#[tokio::test]
async fn audio_tail_schema_defaults_cap_and_byte_counts() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_ENABLE_AUDIO", "true"),
            ("SYNAPSE_AUDIO_LOOPBACK", "0"),
        ],
    )
    .await?;

    let tools = client.tools_list().await?;
    let tools = tools
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;
    let audio_tail_tool = tools
        .iter()
        .find(|tool| tool["name"] == "audio_tail")
        .context("audio_tail tool missing")?;
    assert_audio_tail_schema(audio_tail_tool);

    let tenth = structured(
        &client
            .tools_call("audio_tail", json!({"seconds": 0.1}))
            .await?,
    )?;
    assert_eq!(tenth["format"], "s16le");
    assert_pcm_len(&tenth, 0.1)?;
    assert_eq!(tenth["sample_rate"], 48000);
    assert_eq!(tenth["channels"], 2);
    assert_eq!(tenth["requested_seconds"], 0.1);
    assert_eq!(tenth["captured_seconds"], 0.0);
    assert_eq!(tenth["frames"], 0);
    assert_eq!(tenth["rms_db"], -120.0);
    assert_eq!(tenth["vad_speech_pct"], 0.0);
    assert_eq!(tenth["recent_events"], json!([]));

    let five = structured(
        &client
            .tools_call("audio_tail", json!({"seconds": 5}))
            .await?,
    )?;
    assert_pcm_len(&five, 5.0)?;

    let zero = structured(
        &client
            .tools_call("audio_tail", json!({"seconds": 0}))
            .await?,
    )?;
    assert_eq!(pcm_len(&zero)?, 0);

    let too_large = client
        .tools_call_error("audio_tail", json!({"seconds": 31}))
        .await?;
    assert_eq!(too_large["data"]["code"], "TOOL_PARAMS_INVALID");

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
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

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "test helper converts bounded synthetic seconds and sample rates into PCM frame counts"
)]
fn assert_pcm_len(payload: &Value, seconds: f64) -> anyhow::Result<()> {
    let sample_rate = usize::try_from(
        payload["sample_rate"]
            .as_u64()
            .context("sample_rate missing")?,
    )
    .context("sample_rate does not fit usize")?;
    let channels = usize::try_from(payload["channels"].as_u64().context("channels missing")?)
        .context("channels does not fit usize")?;
    let frames = (seconds * sample_rate as f64).round() as usize;
    let expected = frames
        .saturating_mul(channels)
        .saturating_mul(BYTES_PER_SAMPLE);
    assert_eq!(pcm_len(payload)?, expected);
    Ok(())
}

fn pcm_len(payload: &Value) -> anyhow::Result<usize> {
    payload["pcm"]
        .as_array()
        .map(Vec::len)
        .context("pcm array missing")
}

fn assert_audio_tail_schema(tool: &Value) {
    let shape = json!({
        "name": tool.get("name").cloned().unwrap_or(Value::Null),
        "inputSchema": tool.get("inputSchema").cloned().unwrap_or(Value::Null),
        "outputSchema": tool.get("outputSchema").cloned().unwrap_or(Value::Null),
    });
    assert_eq!(shape["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        shape["inputSchema"]["properties"]["seconds"]["default"],
        5.0
    );
    assert_eq!(shape["inputSchema"]["properties"]["seconds"]["maximum"], 30);
    assert_eq!(shape["inputSchema"]["properties"]["seconds"]["minimum"], 0);
    assert_eq!(
        shape["outputSchema"]["required"],
        json!([
            "pcm",
            "sample_rate",
            "channels",
            "format",
            "requested_seconds",
            "captured_seconds",
            "frames",
            "rms_db",
            "vad_speech_pct",
            "recent_events"
        ])
    );
    insta::assert_json_snapshot!("m3_audio_tail_tool", shape);
}
