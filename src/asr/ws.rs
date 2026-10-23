//! WebSocket 形状：`wss://{host}/api-ws/v1/inference`，run-task / finish-task。
//!
//! 流程（官方文档「Interaction flow」）：
//! 1. 连上后发 `run-task`；
//! 2. 收到 `task-started` 才能发音频；
//! 3. 发二进制音频（16 kHz 单声道 WAV/PCM），边收 `result-generated`；
//! 4. 发 `finish-task`；
//! 5. 收到 `task-finished` 结束。
//!
//! `-message` / `-streaming` / `-realtime` 三个型号共用这套事件，差别在模型本身。

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use super::QwenClient;

/// 每帧 100 ms @ 16 kHz 16 bit 单声道。
const CHUNK: usize = 3200;

pub fn transcribe(c: &QwenClient, wav: &[u8]) -> Result<String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow!("建 tokio 运行时失败：{e}"))?;
    rt.block_on(async {
        tokio::time::timeout(c.timeout, run(c, wav))
            .await
            .map_err(|_| anyhow!("WebSocket 超时（{} 秒）", c.timeout.as_secs()))?
    })
}

async fn run(c: &QwenClient, wav: &[u8]) -> Result<String> {
    let url = c.ws_url()?;
    let mut req = url.as_str().into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {}", c.key).parse()?);

    let (mut ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| anyhow!("连接 {url} 失败：{e}"))?;

    let task_id = uuid::Uuid::new_v4().simple().to_string();
    ws.send(Message::Text(run_task(&task_id, c).to_string().into()))
        .await?;

    // 1) 等 task-started
    loop {
        let msg = ws
            .next()
            .await
            .ok_or_else(|| anyhow!("连接在 task-started 之前关闭"))?
            .map_err(|e| anyhow!("WebSocket 读失败：{e}"))?;
        let Some(text) = msg.to_text().ok() else { continue };
        let v: Value = serde_json::from_str(text)
            .map_err(|e| anyhow!("服务端帧不是 JSON：{e}：{text}"))?;
        match event(&v) {
            Some("task-started") => break,
            Some("task-failed") | Some("task-finished") => return Err(failed(&v)),
            _ => {}
        }
    }

    // 2) 发音频（已录完的整段，按 100 ms 切片，不额外等待）
    for chunk in wav.chunks(CHUNK) {
        ws.send(Message::Binary(chunk.to_vec().into())).await?;
    }
    // 3) 收尾
    ws.send(Message::Text(finish_task(&task_id).to_string().into()))
        .await?;

    // 4) 收结果，直到 task-finished
    let mut finals: Vec<String> = Vec::new();
    let mut partial = String::new();
    loop {
        let msg = ws
            .next()
            .await
            .ok_or_else(|| anyhow!("连接在 task-finished 之前关闭"))?
            .map_err(|e| anyhow!("WebSocket 读失败：{e}"))?;
        let Some(text) = msg.to_text().ok() else { continue };
        let v: Value = serde_json::from_str(text)
            .map_err(|e| anyhow!("服务端帧不是 JSON：{e}：{text}"))?;
        match event(&v) {
            Some("result-generated") => {
                if let Some(s) = v.pointer("/payload/output/sentence") {
                    if let Some(t) = s.get("text").and_then(|t| t.as_str()) {
                        if s.get("sentence_end").and_then(|b| b.as_bool()).unwrap_or(true) {
                            finals.push(t.to_string());
                            partial.clear();
                        } else {
                            partial = t.to_string();
                        }
                    }
                }
            }
            Some("task-finished") => break,
            Some("task-failed") => return Err(failed(&v)),
            _ => {}
        }
    }
    let _ = ws.close(None).await;

    let text = if finals.is_empty() { partial } else { finals.concat() };
    Ok(text.trim().to_string())
}

fn event(v: &Value) -> Option<&str> {
    v.pointer("/header/event").and_then(|e| e.as_str())
}

fn failed(v: &Value) -> anyhow::Error {
    let msg = v
        .pointer("/header/error_message")
        .or_else(|| v.pointer("/payload/message"))
        .and_then(|m| m.as_str())
        .unwrap_or("未知错误");
    let code = v
        .pointer("/header/error_code")
        .and_then(|c| c.as_str())
        .unwrap_or("-");
    anyhow!("服务端报错 {code}：{msg}")
}

fn run_task(task_id: &str, c: &QwenClient) -> Value {
    let mut params = json!({
        "format": "wav",
        "sample_rate": 16_000,
        // 只要最终结果：中途文本没有地方显示，省流量也少噪声
        "intermediate_result_enabled": false,
    });
    if let Some(lang) = &c.language {
        params["language_hints"] = json!([lang]);
    }
    json!({
        "header": { "action": "run-task", "task_id": task_id, "streaming": "duplex" },
        "payload": {
            "task_group": "audio",
            "task": "asr",
            "function": "recognition",
            "model": c.model,
            "parameters": params,
            "input": {}
        }
    })
}

fn finish_task(task_id: &str) -> Value {
    json!({
        "header": { "action": "finish-task", "task_id": task_id, "streaming": "duplex" },
        "payload": { "input": {} }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn client() -> QwenClient {
        QwenClient {
            style: super::super::Style::Ws,
            base: "https://x.example".into(),
            key: "k".into(),
            model: "qwen-audio-3.1-asr-flash-message".into(),
            language: Some("zh".into()),
            enable_itn: false,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn run_task_payload_matches_documented_shape() {
        let v = run_task("tid", &client());
        assert_eq!(v["header"]["action"], "run-task");
        assert_eq!(v["header"]["streaming"], "duplex");
        assert_eq!(v["payload"]["task_group"], "audio");
        assert_eq!(v["payload"]["task"], "asr");
        assert_eq!(v["payload"]["function"], "recognition");
        assert_eq!(v["payload"]["model"], "qwen-audio-3.1-asr-flash-message");
        assert_eq!(v["payload"]["parameters"]["format"], "wav");
        assert_eq!(v["payload"]["parameters"]["sample_rate"], 16_000);
        assert_eq!(v["payload"]["parameters"]["language_hints"][0], "zh");
    }

    #[test]
    fn finish_task_shape() {
        let v = finish_task("tid");
        assert_eq!(v["header"]["action"], "finish-task");
        assert_eq!(v["header"]["task_id"], "tid");
        assert_eq!(v["payload"]["input"], json!({}));
    }

    #[test]
    fn event_extraction() {
        let v: Value =
            serde_json::from_str(r#"{"header":{"event":"task-finished","task_id":"t"}}"#).unwrap();
        assert_eq!(event(&v), Some("task-finished"));
        assert_eq!(event(&json!({})), None);
    }
}
