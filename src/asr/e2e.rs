//! 四种报文形状的端到端测试：起本地假服务端，走真 socket。
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
                json!({"header":{"event":"task-started","task_id":"t"}}).to_string(),
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
                json!({"header":{"event":"task-finished"}}).to_string(),
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

/// 假服务端（异步任务版）：一条线程按顺序接待
/// getPolicy → OSS 上传 → 建任务 → 轮询（PENDING→SUCCEEDED）→ 取结果，共 6 个请求。
fn filetrans_server() -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let b = base.clone();
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..6 {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
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
            let req_line = head.lines().next().unwrap().to_string();
            let body_s = String::from_utf8_lossy(&body).to_string();
            seen.push(format!("{head}{body_s}"));

            let reply: String = if req_line.starts_with("GET /api/v1/uploads") {
                format!(
                    r#"{{"request_id":"r0","data":{{"policy":"pol","signature":"sig","upload_dir":"DASHSCOPE","upload_host":"{b}","oss_access_key_id":"ak","x_oss_object_acl":"private","x_oss_forbid_overwrite":"true"}}}}"#
                )
            } else if req_line.starts_with("POST / ") {
                String::new()
            } else if req_line.starts_with("POST /api/v1/services/audio/asr/transcription") {
                r#"{"request_id":"r1","output":{"task_id":"t-e2e","task_status":"PENDING"}}"#.into()
            } else if req_line.starts_with("GET /api/v1/tasks/t-e2e") {
                if seen.len() == 4 {
                    r#"{"request_id":"r2","output":{"task_id":"t-e2e","task_status":"PENDING"}}"#
                        .into()
                } else {
                    format!(
                        r#"{{"output":{{"task_id":"t-e2e","task_status":"SUCCEEDED","results":[{{"file_url":"oss://DASHSCOPE/x.wav","subtask_status":"SUCCEEDED","transcription_url":"{b}/fake-result.json"}}]}}}}"#
                    )
                }
            } else {
                r#"{"transcripts":[{"channel_id":0,"text":"异步转写成功。"}]}"#.into()
            };
            let payload = reply.as_bytes();
            write!(
                sock,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            sock.write_all(payload).unwrap();
        }
        seen
    });
    (base, handle)
}

#[test]
fn filetrans_shape_end_to_end() {
    let (base, srv) = filetrans_server();
    let c = client(base, "qwen3-asr-flash-filetrans", Style::FileTrans);
    let out = super::filetrans::transcribe(&c, b"RIFF....").unwrap();
    assert_eq!(out, "异步转写成功。");

    let seen = srv.join().unwrap();
    // 1. getPolicy：模型名跟着走
    assert!(
        seen[0].starts_with("GET /api/v1/uploads?action=getPolicy&model=qwen3-asr-flash-filetrans"),
        "{}",
        seen[0]
    );
    // 2. OSS PostObject：multipart、字段在前 file 殿后、key 带上传目录
    assert!(seen[1].starts_with("POST / "), "{}", seen[1]);
    assert!(seen[1]
        .to_lowercase()
        .contains("content-type: multipart/form-data; boundary="));
    for f in [
        "OSSAccessKeyId",
        "Signature",
        "policy",
        "key",
        "success_action_status",
    ] {
        assert!(seen[1].contains(&format!("name=\"{f}\"")), "缺 {f}");
    }
    assert!(
        seen[1].find("name=\"key\"").unwrap() < seen[1].find("name=\"file\"").unwrap(),
        "file 必须是最后一个字段"
    );
    assert!(seen[1].contains("DASHSCOPE/dictate-"), "{}", seen[1]);
    assert!(seen[1].contains("RIFF...."));
    // 3. 建任务：异步头 + oss:// 引用 + parameters 必须存在（qwen-audio 系缺它直接失败）
    assert!(
        seen[2].starts_with("POST /api/v1/services/audio/asr/transcription"),
        "{}",
        seen[2]
    );
    assert!(seen[2].to_lowercase().contains("x-dashscope-async: enable"));
    let submit: Value = serde_json::from_str(seen[2].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(submit["model"], "qwen3-asr-flash-filetrans");
    assert!(submit["input"]["file_urls"][0]
        .as_str()
        .unwrap()
        .starts_with("oss://DASHSCOPE/dictate-"));
    assert_eq!(submit["parameters"]["language_hints"][0], "zh");
    // 4/5. 轮询：带鉴权
    assert!(
        seen[3].starts_with("GET /api/v1/tasks/t-e2e"),
        "{}",
        seen[3]
    );
    assert!(seen[3]
        .to_lowercase()
        .contains("authorization: bearer test-key"));
    assert!(
        seen[4].starts_with("GET /api/v1/tasks/t-e2e"),
        "{}",
        seen[4]
    );
    // 6. 取结果：签名 URL，不带鉴权
    assert!(seen[5].starts_with("GET /fake-result.json"), "{}", seen[5]);
    assert!(!seen[5].to_lowercase().contains("authorization"));
}

/// 假 WS 服务端（流式版）：收到第一帧音频就给中间结果，
/// 收到 finish-task 给定稿句子和 task-finished。
fn ws_stream_server() -> (String, std::thread::JoinHandle<Vec<String>>) {
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            port_tx.send(listener.local_addr().unwrap().port()).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            let mut seen = Vec::new();
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) = ws.next().await {
                seen.push(t.to_string());
            }
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"header":{"event":"task-started","task_id":"t"}}).to_string(),
            ))
            .await
            .unwrap();

            let mut audio_bytes = 0usize;
            let mut partial_sent = false;
            loop {
                match ws.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(b))) => {
                        audio_bytes += b.len();
                        if !partial_sent {
                            partial_sent = true;
                            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                                json!({"header":{"event":"result-generated"},
                                       "payload":{"output":{"sentence":{"text":"你好，","sentence_end":false}}}})
                                .to_string(),
                            ))
                            .await
                            .unwrap();
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => {
                        seen.push(t.to_string());
                        break;
                    }
                    _ => break,
                }
            }
            // 100 ms 的块重采样后可能不足一整帧，Finish 会把尾帧刷出来：
            // 只要求「有 PCM 且是 16bit 对齐」
            assert!(
                audio_bytes >= 2000 && audio_bytes.is_multiple_of(2),
                "应有 16bit 对齐的 PCM 到达（{audio_bytes} 字节）"
            );

            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"header":{"event":"result-generated"},
                       "payload":{"output":{"sentence":{"text":"你好，世界。","sentence_end":true}}}})
                .to_string(),
            ))
            .await
            .unwrap();
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"header":{"event":"task-finished"}}).to_string(),
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
fn websocket_stream_session_end_to_end() {
    use super::ws::StreamEvent;
    use crate::audio::CaptureOut;

    let (base, srv) = ws_stream_server();
    let c = client(base, "qwen3-asr-flash-realtime", Style::Ws);

    // 100 ms @ 48 kHz 的单声道块 → 重采样后正好一帧 16 kHz PCM
    let (atx, arx) = tokio::sync::mpsc::unbounded_channel();
    atx.send(CaptureOut::Begin { rate: 48_000 }).unwrap();
    atx.send(CaptureOut::Chunk(vec![0.1f32; 4800])).unwrap();
    atx.send(CaptureOut::Finish).unwrap();
    drop(atx);

    let (etx, erx) = std::sync::mpsc::channel();
    super::ws::stream_session(c, arx, etx);

    let mut partials = Vec::new();
    let mut final_text: Option<Result<String, String>> = None;
    for ev in erx.iter() {
        match ev {
            StreamEvent::Partial(p) => partials.push(p),
            StreamEvent::Final(r) => final_text = Some(r),
        }
    }
    assert_eq!(final_text.expect("必须有 Final").unwrap(), "你好，世界。");
    assert!(
        partials.iter().any(|p| p.contains("你好，")),
        "应收到中间结果：{partials:?}"
    );

    let seen = srv.join().unwrap();
    let run: Value = serde_json::from_str(&seen[0]).unwrap();
    assert_eq!(run["payload"]["parameters"]["format"], "pcm");
    assert_eq!(
        run["payload"]["parameters"]["intermediate_result_enabled"],
        true
    );
    let fin: Value = serde_json::from_str(&seen[1]).unwrap();
    assert_eq!(fin["header"]["action"], "finish-task");
}
