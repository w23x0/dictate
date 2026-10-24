//! WebSocket 形状：`wss://{host}/api-ws/v1/inference`，run-task / finish-task。
//!
//! 流程（官方文档「Interaction flow」）：
//! 1. 连上后发 `run-task`；
//! 2. 收到 `task-started` 才能发音频；
//! 3. 发二进制音频（16 kHz 单声道），边收 `result-generated`；
//! 4. 发 `finish-task`；
//! 5. 收到 `task-finished` 结束。
//!
//! 两种用法：
//! * [`transcribe`]：整段 WAV 一次性发（批量，不给中间结果）；
//! * [`stream_session`]：真流式——音频块实时进、中间文本实时出（边说边出字）。
//!
//! `-message` / `-streaming` / `-realtime` 三个型号共用这套事件，差别在模型本身。

use std::sync::mpsc::Sender;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use super::QwenClient;
use crate::audio::{CaptureOut, Downsampler};

/// 批量：每片 100 ms @ 16 kHz 16 bit 单声道（3200 字节）。
const CHUNK: usize = 3200;
/// 流式：每帧 100 ms @ 16 kHz（1600 个样本）。
const FRAME: usize = 1600;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 流式会话吐回的事件。
pub enum StreamEvent {
    /// 中间结果：已定稿句子 + 当前半句的拼接，给 UI 直接显示。
    Partial(String),
    /// `Ok(完整文本)` / `Err(原因)`。
    Final(Result<String, String>),
}

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
    let mut ws = connect(c).await?;
    let task_id = uuid::Uuid::new_v4().simple().to_string();
    ws.send(Message::Text(run_task(&task_id, c, false).to_string()))
        .await?;
    wait_started(&mut ws).await?;

    // 整段按 100 ms 切片发掉（不额外等待）
    for chunk in wav.chunks(CHUNK) {
        ws.send(Message::Binary(chunk.to_vec())).await?;
    }
    ws.send(Message::Text(finish_task(&task_id).to_string()))
        .await?;

    let mut finals: Vec<String> = Vec::new();
    let mut partial = String::new();
    loop {
        let msg = ws
            .next()
            .await
            .ok_or_else(|| anyhow!("连接在 task-finished 之前关闭"))?
            .map_err(|e| anyhow!("WebSocket 读失败：{e}"))?;
        match handle_event(msg, None, &mut finals, &mut partial)? {
            EvOutcome::Continue => {}
            EvOutcome::Done(text) => {
                let _ = ws.close(None).await;
                return Ok(text);
            }
        }
    }
}

/// 真流式会话：跑在自己的线程里（内部起 current-thread tokio）。
/// 音频从 `audio` 来（设备采样率单声道 f32，采集线程实时转发），
/// 事件从 `events` 出；`Finish`（或对端断开）后以 `Final` 收尾。
pub fn stream_session(
    c: QwenClient,
    audio: tokio::sync::mpsc::UnboundedReceiver<CaptureOut>,
    events: Sender<StreamEvent>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = events.send(StreamEvent::Final(Err(format!("建运行时失败：{e}"))));
            return;
        }
    };
    // run_stream 成功路径里自己发 Final；失败路径在这里补发
    let mut audio = audio;
    let res = rt.block_on(run_stream(&c, &mut audio, &events));
    if let Err(e) = res {
        let _ = events.send(StreamEvent::Final(Err(format!("{e:#}"))));
    }
}

async fn run_stream(
    c: &QwenClient,
    audio: &mut tokio::sync::mpsc::UnboundedReceiver<CaptureOut>,
    events: &Sender<StreamEvent>,
) -> Result<()> {
    let mut ws = connect(c).await?;
    let task_id = uuid::Uuid::new_v4().simple().to_string();
    ws.send(Message::Text(run_task(&task_id, c, true).to_string()))
        .await?;
    wait_started(&mut ws).await?;

    let mut ds: Option<Downsampler> = None;
    // Begin 之前到的块先攒着（channel 跨 sender 不保证顺序，这里兜住）
    let mut stash: Vec<Vec<f32>> = Vec::new();
    let mut frame: Vec<i16> = Vec::with_capacity(FRAME);
    let mut finals: Vec<String> = Vec::new();
    let mut partial = String::new();

    // 双工：音频进来就发帧；服务端结果随到随收
    loop {
        tokio::select! {
            biased;
            out = audio.recv() => {
                match out {
                    Some(CaptureOut::Begin { rate }) => {
                        let d = Downsampler::new(rate)?;
                        ds = Some(d);
                        let d = ds.as_mut().unwrap();
                        let mut mono = Vec::new();
                        for ch in stash.drain(..) {
                            d.push(&ch, &mut mono)?;
                        }
                        queue(&mut frame, &mono, &mut ws).await?;
                    }
                    Some(CaptureOut::Chunk(v)) => {
                        if let Some(d) = ds.as_mut() {
                            let mut mono = Vec::new();
                            d.push(&v, &mut mono)?;
                            queue(&mut frame, &mono, &mut ws).await?;
                        } else {
                            stash.push(v);
                        }
                    }
                    Some(CaptureOut::Finish) | None => {
                        if let Some(d) = ds.as_mut() {
                            let mut mono = Vec::new();
                            d.finish(&mut mono)?;
                            queue(&mut frame, &mono, &mut ws).await?;
                        }
                        if !frame.is_empty() {
                            ws.send(Message::Binary(le(&frame))).await?;
                            frame.clear();
                        }
                        ws.send(Message::Text(finish_task(&task_id).to_string()))
                            .await?;
                        break;
                    }
                }
            }
            msg = ws.next() => {
                let msg = msg
                    .ok_or_else(|| anyhow!("连接在收音期间关闭"))?
                    .map_err(|e| anyhow!("WebSocket 读失败：{e}"))?;
                if matches!(
                    handle_event(msg, Some(events), &mut finals, &mut partial)?,
                    EvOutcome::Done(_)
                ) {
                    let _ = ws.close(None).await;
                    return Ok(());
                }
            }
        }
    }

    // finish-task 已发：读完剩余事件直到终局
    loop {
        let msg = tokio::time::timeout(c.timeout, ws.next())
            .await
            .map_err(|_| anyhow!("等最终结果超时（{} 秒）", c.timeout.as_secs()))?
            .ok_or_else(|| anyhow!("连接在 task-finished 之前关闭"))?
            .map_err(|e| anyhow!("WebSocket 读失败：{e}"))?;
        if matches!(
            handle_event(msg, Some(events), &mut finals, &mut partial)?,
            EvOutcome::Done(_)
        ) {
            let _ = ws.close(None).await;
            return Ok(());
        }
    }
}

/// 攒满一帧（100 ms）就发，尾巴留着和下一块拼。
async fn queue(frame: &mut Vec<i16>, mono: &[f32], ws: &mut WsStream) -> Result<()> {
    for s in mono {
        frame.push((s.clamp(-1.0, 1.0) * 32767.0).round() as i16);
    }
    while frame.len() >= FRAME {
        let chunk: Vec<i16> = frame.drain(..FRAME).collect();
        ws.send(Message::Binary(le(&chunk))).await?;
    }
    Ok(())
}

fn le(frame: &[i16]) -> Vec<u8> {
    frame.iter().flat_map(|s| s.to_le_bytes()).collect()
}

async fn connect(c: &QwenClient) -> Result<WsStream> {
    let url = c.ws_url()?;
    let mut req = url.as_str().into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {}", c.key).parse()?);
    let (ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| anyhow!("连接 {url} 失败：{e}"))?;
    Ok(ws)
}

async fn wait_started(ws: &mut WsStream) -> Result<()> {
    loop {
        let msg = ws
            .next()
            .await
            .ok_or_else(|| anyhow!("连接在 task-started 之前关闭"))?
            .map_err(|e| anyhow!("WebSocket 读失败：{e}"))?;
        let Some(text) = msg.to_text().ok() else {
            continue;
        };
        let v: Value =
            serde_json::from_str(text).map_err(|e| anyhow!("服务端帧不是 JSON：{e}：{text}"))?;
        match event(&v) {
            Some("task-started") => return Ok(()),
            Some("task-failed") | Some("task-finished") => return Err(failed(&v)),
            _ => {}
        }
    }
}

enum EvOutcome {
    Continue,
    /// 会话终局，带完整文本（`events` 有值时 Final 已经发过）
    Done(String),
}

/// 处理一条服务端事件；task-failed 以 Err 冒泡（由调用方决定怎么收尾）。
fn handle_event(
    msg: Message,
    events: Option<&Sender<StreamEvent>>,
    finals: &mut Vec<String>,
    partial: &mut String,
) -> Result<EvOutcome> {
    let Some(text) = msg.to_text().ok() else {
        return Ok(EvOutcome::Continue);
    };
    let v: Value =
        serde_json::from_str(text).map_err(|e| anyhow!("服务端帧不是 JSON：{e}：{text}"))?;
    match event(&v) {
        Some("result-generated") => {
            if let Some(s) = v.pointer("/payload/output/sentence") {
                if let Some(t) = s.get("text").and_then(|t| t.as_str()) {
                    if s.get("sentence_end")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(true)
                    {
                        finals.push(t.to_string());
                        partial.clear();
                    } else {
                        *partial = t.to_string();
                    }
                    if let Some(tx) = events {
                        let _ = tx.send(StreamEvent::Partial(format!(
                            "{}{}",
                            finals.concat(),
                            partial
                        )));
                    }
                }
            }
            Ok(EvOutcome::Continue)
        }
        Some("task-finished") => {
            let text = if finals.is_empty() {
                partial.clone()
            } else {
                finals.concat()
            };
            let text = text.trim().to_string();
            if let Some(tx) = events {
                let _ = tx.send(StreamEvent::Final(Ok(text.clone())));
            }
            Ok(EvOutcome::Done(text))
        }
        Some("task-failed") => Err(failed(&v)),
        _ => Ok(EvOutcome::Continue),
    }
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

/// `streaming` = 真流式会话：中间结果打开，PCM 裸流；
/// false = 批量：整段 WAV、不要中间结果（没地方显示，省流量也少噪声）。
fn run_task(task_id: &str, c: &QwenClient, streaming: bool) -> Value {
    let mut params = json!({
        "format": if streaming { "pcm" } else { "wav" },
        "sample_rate": 16_000,
        "intermediate_result_enabled": streaming,
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
        let v = run_task("tid", &client(), false);
        assert_eq!(v["header"]["action"], "run-task");
        assert_eq!(v["header"]["streaming"], "duplex");
        assert_eq!(v["payload"]["task_group"], "audio");
        assert_eq!(v["payload"]["task"], "asr");
        assert_eq!(v["payload"]["function"], "recognition");
        assert_eq!(v["payload"]["model"], "qwen-audio-3.1-asr-flash-message");
        assert_eq!(v["payload"]["parameters"]["format"], "wav");
        assert_eq!(v["payload"]["parameters"]["sample_rate"], 16_000);
        assert_eq!(
            v["payload"]["parameters"]["intermediate_result_enabled"],
            false
        );
        assert_eq!(v["payload"]["parameters"]["language_hints"][0], "zh");
    }

    #[test]
    fn run_task_streaming_variant() {
        let v = run_task("tid", &client(), true);
        assert_eq!(v["payload"]["parameters"]["format"], "pcm");
        assert_eq!(
            v["payload"]["parameters"]["intermediate_result_enabled"],
            true
        );
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

    #[test]
    fn pcm_frames_are_100ms_little_endian() {
        let bytes = le(&[0i16, 1, -1]);
        assert_eq!(bytes, vec![0, 0, 1, 0, 0xFF, 0xFF]);
    }
}
