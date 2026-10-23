//! 三种报文形状的端到端测试：起本地假服务端，走真 socket。
//!
//! 目的是把「报文对不对、事件顺序对不对、结果取哪一路」钉在测试里，
//! 不依赖 API key 与公网。

use std::io::{Read, Write};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};

use super::{QwenClient, Style};

fn client(base: String, model: &str, style: Style) -> QwenClient {
    QwenClient {
        style,
        base,
        key: "test-key".into(),
        model: model.into(),
        language: Some("zh".into()),
        enable_itn: false,
        timeout: Duration::from_secs(10),
    }
}

/// 最小 HTTP/1.1 服务端：读一个请求，回一段固定 JSON。
fn http_server(reply: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        // 读到 header 结束
        let header_end = loop {
            let n = sock.read(&mut tmp).unwrap();
            assert!(n > 0, "客户端没发请求");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        let mut body = buf[header_end..].to_vec();
        while body.len() < len {
            let n = sock.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        let body = String::from_utf8_lossy(&body).to_string();

        let payload = reply.as_bytes();
        write!(
            sock,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            payload.len()
        )
        .unwrap();
        sock.write_all(payload).unwrap();
        head + &body
    });
    (base, handle)
}

#[test]
fn openai_shape_end_to_end() {
    let (base, srv) = http_server(
        r#"{"choices":[{"message":{"content":"你好，世界。"}}],"usage":{"seconds":1}}"#,
    );
    let c = client(base, "qwen3-asr-flash", Style::OpenAi);
    let out = super::openai::transcribe(&c, b"RIFF....").unwrap();
    assert_eq!(out, "你好，世界。");

    let req = srv.join().unwrap();
    assert!(
        req.starts_with("POST /compatible-mode/v1/chat/completions"),
        "{req}"
    );
    assert!(req
        .to_lowercase()
        .contains("authorization: bearer test-key"));
    let body: Value = serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["model"], "qwen3-asr-flash");
    assert!(body["messages"][0]["content"][0]["input_audio"]["data"]
        .as_str()
        .unwrap()
        .starts_with("data:audio/wav;base64,"));
}

#[test]
fn dashscope_sync_shape_end_to_end() {
    let (base, srv) = http_server(
        r#"{"output":{"choices":[{"message":{"content":[{"text":"欢迎使用。"}]}}]},"request_id":"r1"}"#,
    );
    let c = client(base, "qwen-audio-3.1-asr-flash", Style::DashScope);
    let out = super::dashscope::transcribe(&c, b"RIFF....").unwrap();
    assert_eq!(out, "欢迎使用。");

    let req = srv.join().unwrap();
    assert!(
        req.starts_with("POST /api/v1/services/aigc/multimodal-generation/generation"),
        "{req}"
    );
    assert!(req.contains("x-dashscope-sse: disable"));
    let body: Value = serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body["input"]["messages"][0]["content"][0]["type"],
        "input_audio"
    );
    assert_eq!(body["parameters"]["format"], "wav");
}

/// 假 WS 服务端：按文档的事件顺序陪客户端走一遍。
fn ws_server() -> (String, std::thread::JoinHandle<Vec<String>>) {
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            // 必须在运行时内 bind：把 std 的 listener 塞进 tokio 会被拒绝。
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            port_tx.send(listener.local_addr().unwrap().port()).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            let mut seen = Vec::new();
            // run-task
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) = ws.next().await {
                seen.push(t.to_string());
            }
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"header":{"event":"task-started","task_id":"t"}})
                    .to_string(),
            ))
            .await
            .unwrap();

            // 音频 chunk + finish-task
            let mut audio_bytes = 0usize;
            loop {
                match ws.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(b))) => {
                        audio_bytes += b.len()
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => {
                        seen.push(t.to_string());
                        break;
                    }
                    _ => break,
                }
            }
            assert!(audio_bytes > 0, "客户端没发音频");

            for (text, end) in [("你好，", false), ("你好，世界。", true)] {
                ws.send(tokio_tungstenite::tungstenite::Message::Text(
                    json!({"header":{"event":"result-generated"},
                           "payload":{"output":{"sentence":{"text":text,"sentence_end":end}}}})
                    .to_string(),
                ))
                .await
                .unwrap();
            }
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"header":{"event":"task-finished"}})
                    .to_string(),
            ))
            .await
            .unwrap();
            seen
        })
    });
    let port = port_rx.recv().unwrap();
    (format!("http://127.0.0.1:{port}"), handle)
}

#[test]
fn websocket_shape_end_to_end() {
    let (base, srv) = ws_server();
    let c = client(base, "qwen-audio-3.1-asr-flash-message", Style::Ws);
    let text = super::ws::transcribe(&c, &vec![7u8; 7000]).unwrap();
    assert_eq!(text, "你好，世界。");

    let seen = srv.join().unwrap();
    let run: Value = serde_json::from_str(&seen[0]).unwrap();
    assert_eq!(run["header"]["action"], "run-task");
    assert_eq!(run["payload"]["model"], "qwen-audio-3.1-asr-flash-message");
    assert_eq!(run["payload"]["parameters"]["language_hints"][0], "zh");
    let fin: Value = serde_json::from_str(&seen[1]).unwrap();
    assert_eq!(fin["header"]["action"], "finish-task");
}
