use anyhow::Context;
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn audio_transcribe_schema_defaults_silence_and_edges() -> anyhow::Result<()> {
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
    let audio_transcribe_tool = tools
        .iter()
        .find(|tool| tool["name"] == "audio_transcribe")
        .context("audio_transcribe tool missing")?;
    assert_audio_transcribe_schema(audio_transcribe_tool);

    let silence = structured(
        &client
            .tools_call("audio_transcribe", json!({"seconds": 5}))
            .await?,
    )?;
    assert_eq!(silence["text"], "");
    assert_eq!(silence["confidence"], 0.0);
    assert_eq!(silence["confidence_source"], "not_applicable");
    assert_eq!(silence["latency_ms"], 0);
    assert_eq!(silence["model_id"], "whisper_tiny_int8");

    let short_silence = structured(
        &client
            .tools_call("audio_transcribe", json!({"seconds": 0.1}))
            .await?,
    )?;
    assert_eq!(short_silence["text"], "");
    assert_eq!(short_silence["confidence"], 0.0);
    assert_eq!(short_silence["confidence_source"], "not_applicable");

    let bad_language = client
        .tools_call_error("audio_transcribe", json!({"language": "xx"}))
        .await?;
    assert_eq!(bad_language["data"]["code"], "TOOL_PARAMS_INVALID");

    let too_large = client
        .tools_call_error("audio_transcribe", json!({"seconds": 31}))
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

fn assert_audio_transcribe_schema(tool: &Value) {
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
    assert_eq!(
        shape["inputSchema"]["properties"]["language"]["default"],
        "en"
    );
    assert_eq!(
        shape["outputSchema"]["required"],
        json!([
            "text",
            "confidence",
            "confidence_source",
            "latency_ms",
            "model_id"
        ])
    );
    insta::assert_json_snapshot!("m3_audio_transcribe_tool", shape);
}
