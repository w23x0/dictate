//! DashScope 异步任务形状：`/api/v1/services/audio/asr/transcription`。
//!
//! `*-filetrans` 模型（qwen3-asr-flash-filetrans、qwen-audio-3.x-asr-flash-filetrans）
//! 只收可公网取回的音频 URL，所以本地录音要走四步：
//!
//! 1. `GET /api/v1/uploads?action=getPolicy&model=…` 拿上传凭证（policy）；
//! 2. 按凭证把 WAV 表单上传到 OSS，得到 `oss://{upload_dir}/{file}` 形式的引用；
//! 3. `POST …/transcription`（`X-DashScope-Async: enable`）建任务，拿 `task_id`；
//! 4. `GET /api/v1/tasks/{task_id}` 轮询到 SUCCEEDED，再取 `transcription_url` 里的文本。
//!
//! 两个不带鉴权的请求：OSS 上传（带鉴权头反而会搅乱 PostObject 校签）和
//! 结果下载（URL 自带签名）。轮询间隔 0.5 s 起步、×1.5 到 2 s 封顶，
//! 总预算 [`POLL_BUDGET`]。

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use super::{humanize_error, text_from, QwenClient};

/// 轮询总预算：短录音服务端几秒出结果，给足余量又不至于吊死听写流程。
const POLL_BUDGET: Duration = Duration::from_secs(120);
const POLL_START: Duration = Duration::from_millis(500);
const POLL_MAX: Duration = Duration::from_secs(2);

pub fn transcribe(c: &QwenClient, wav: &[u8]) -> Result<String> {
    let http = reqwest::blocking::Client::builder()
        .timeout(c.timeout)
        .build()
        .context("建 HTTP 客户端失败")?;
    let audio_url = upload(c, &http, wav)?;
    let task_id = submit(c, &audio_url)?;
    let task = poll(c, &http, &task_id)?;
    fetch_text(&http, &task)
}

/// 拿上传凭证并把 WAV 传到 OSS，返回 `oss://{upload_dir}/{file}` 引用。
fn upload(c: &QwenClient, http: &reqwest::blocking::Client, wav: &[u8]) -> Result<String> {
    let url = format!(
        "{}/api/v1/uploads?action=getPolicy&model={}",
        c.base, c.model
    );
    let v = get_json(c, http, &url)?;
    let d = v
        .get("data")
        .ok_or_else(|| anyhow!("上传凭证响应缺 data：{}", brief(&v)))?;
    let host = str_field(d, "upload_host")?;
    let dir = str_field(d, "upload_dir")?;
    let name = format!("dictate-{}.wav", uuid::Uuid::new_v4().simple());
    let key = format!("{dir}/{name}");

    let fields = [
        ("OSSAccessKeyId", str_field(d, "oss_access_key_id")?),
        ("Signature", str_field(d, "signature")?),
        ("policy", str_field(d, "policy")?),
        ("key", key.as_str()),
        ("success_action_status", "200"),
        ("x-oss-object-acl", str_field(d, "x_oss_object_acl")?),
        (
            "x-oss-forbid-overwrite",
            str_field(d, "x_oss_forbid_overwrite")?,
        ),
    ];
    let (ct, body) = multipart(&fields, &name, wav);
    let resp = http
        .post(host)
        .header("content-type", ct.as_str())
        .body(body)
        .send()
        .context("上传音频失败（网络）")?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("上传音频失败：HTTP {status}：{}", brief_str(&text));
    }
    Ok(format!("oss://{key}"))
}

/// 带鉴权的 GET，非 2xx 时把服务端的 message 抛出来。
fn get_json(c: &QwenClient, http: &reqwest::blocking::Client, url: &str) -> Result<Value> {
    let resp = http
        .get(url)
        .bearer_auth(&c.key)
        .send()
        .context("请求失败（网络）")?;
    let status = resp.status();
    let text = resp.text().context("读响应体失败")?;
    if !status.is_success() {
        let msg = humanize_error(&text).unwrap_or_else(|| brief_str(&text));
        bail!("HTTP {status}：{msg}");
    }
    serde_json::from_str(&text).map_err(|_| anyhow!("响应不是 JSON：{}", brief_str(&text)))
}

/// 手拼 OSS PostObject 的 multipart/form-data：普通字段在前，`file` 必须最后一个。
fn multipart(fields: &[(&str, &str)], filename: &str, file: &[u8]) -> (String, Vec<u8>) {
    let boundary = format!("----dictate-{}", uuid::Uuid::new_v4().simple());
    let mut out = Vec::new();
    for (k, v) in fields {
        out.extend_from_slice(
            format!("--{boundary}\r\ncontent-disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    out.extend_from_slice(
        format!(
            "--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; \
             filename=\"{filename}\"\r\ncontent-type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(file);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), out)
}

fn submit_body(c: &QwenClient, audio_url: &str) -> Value {
    let mut params = json!({});
    if let Some(lang) = &c.language {
        params["language_hints"] = json!([lang]);
    }
    json!({
        "model": c.model,
        "input": { "file_urls": [audio_url] },
        // qwen-audio-3.x-asr-flash-filetrans 的 parameters 是必填字段（可为空对象），不能省
        "parameters": params,
    })
}

fn submit(c: &QwenClient, audio_url: &str) -> Result<String> {
    let raw = super::post_json(
        c,
        &c.http_endpoint(),
        &submit_body(c, audio_url),
        &[("X-DashScope-Async", "enable")],
    )?;
    let v: Value = serde_json::from_str(&raw)
        .map_err(|_| anyhow!("建任务响应不是 JSON：{}", brief_str(&raw)))?;
    v.pointer("/output/task_id")
        .and_then(|t| t.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("建任务响应里没有 task_id：{}", brief_str(&raw)))
}

fn poll(c: &QwenClient, http: &reqwest::blocking::Client, task_id: &str) -> Result<Value> {
    let url = format!("{}/api/v1/tasks/{task_id}", c.base);
    let budget = c.timeout.max(POLL_BUDGET);
    let deadline = Instant::now() + budget;
    let mut wait = POLL_START;
    loop {
        if Instant::now() >= deadline {
            bail!(
                "异步任务 {task_id} {:.0} 秒没出结果，已放弃（任务可能还在跑）",
                budget.as_secs_f32()
            );
        }
        std::thread::sleep(wait);
        let v = get_json(c, http, &url)?;
        match v
            .pointer("/output/task_status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
        {
            "SUCCEEDED" => return Ok(v),
            "PENDING" | "RUNNING" => wait = wait.mul_f64(1.5).min(POLL_MAX),
            other => {
                let shown = if other.is_empty() {
                    "未知状态"
                } else {
                    other
                };
                bail!(
                    "异步任务失败（{shown}）：{}",
                    error_of(v.pointer("/output").unwrap_or(&Value::Null))
                )
            }
        }
    }
}

/// 从任务/结果节点里挖 code + message（FAILED 的原因在这里）。
fn error_of(node: &Value) -> String {
    let code = node.get("code").and_then(|c| c.as_str()).unwrap_or("-");
    let msg = node.get("message").and_then(|m| m.as_str()).unwrap_or("");
    if msg.is_empty() {
        "服务端没给原因".to_string()
    } else {
        format!("{code}：{msg}")
    }
}

/// 结果 URL 自带签名，不带鉴权取。
fn fetch_text(http: &reqwest::blocking::Client, task: &Value) -> Result<String> {
    let r0 = task
        .pointer("/output/results/0")
        .ok_or_else(|| anyhow!("任务成功但没有结果：{}", brief(task)))?;
    if r0.get("subtask_status").and_then(|s| s.as_str()) == Some("FAILED") {
        bail!("转写失败：{}", error_of(r0));
    }
    let url = r0
        .get("transcription_url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("结果里没有 transcription_url：{}", brief(r0)))?;
    let resp = http.get(url).send().context("取转写结果失败（网络）")?;
    let status = resp.status();
    let text = resp.text().context("读转写结果失败")?;
    if !status.is_success() {
        let msg = humanize_error(&text).unwrap_or_else(|| brief_str(&text));
        bail!("取转写结果失败：HTTP {status}：{msg}");
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|_| anyhow!("转写结果不是 JSON：{}", brief_str(&text)))?;
    // transcripts[] 每个声道一段 text；单声道听写只有一段
    text_from(&v, &["/transcripts", "/text"])
}

fn str_field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("上传凭证里缺 {key}：{}", brief(v)))
}

fn brief(v: &Value) -> String {
    brief_str(&v.to_string())
}

fn brief_str(s: &str) -> String {
    s.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(model: &str) -> QwenClient {
        QwenClient {
            style: super::super::Style::FileTrans,
            base: "https://x.example".into(),
            key: "k".into(),
            model: model.into(),
            language: Some("zh".into()),
            enable_itn: false,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn submit_body_matches_documented_shape() {
        let b = submit_body(&client("qwen3-asr-flash-filetrans"), "oss://dir/a.wav");
        assert_eq!(b["model"], "qwen3-asr-flash-filetrans");
        assert_eq!(b["input"]["file_urls"][0], "oss://dir/a.wav");
        assert_eq!(b["parameters"]["language_hints"][0], "zh");
        // parameters 必须存在：qwen-audio 系 filetrans 缺了它会直接识别失败
        assert!(b["parameters"].is_object());
    }

    #[test]
    fn submit_body_without_language_keeps_empty_parameters() {
        let mut c = client("qwen-audio-3.1-asr-flash-filetrans");
        c.language = None;
        let b = submit_body(&c, "oss://dir/a.wav");
        assert_eq!(b["parameters"], json!({}));
    }

    #[test]
    fn multipart_puts_file_last_and_wraps_with_boundary() {
        let fields = [
            ("OSSAccessKeyId", "ak"),
            ("policy", "pol"),
            ("key", "dir/a.wav"),
        ];
        let (ct, body) = multipart(&fields, "a.wav", b"RIFF....");
        assert!(ct.starts_with("multipart/form-data; boundary="), "{ct}");
        let s = String::from_utf8(body).unwrap();
        let boundary = ct.rsplit('=').next().unwrap();
        assert!(s.starts_with(&format!("--{boundary}\r\n")));
        assert!(s.ends_with(&format!("--{boundary}--\r\n")));
        let file_pos = s.find("name=\"file\"").unwrap();
        for k in ["OSSAccessKeyId", "policy", "key"] {
            let pos = s.find(&format!("name=\"{k}\"")).unwrap();
            assert!(pos < file_pos, "{k} 必须在 file 之前");
        }
        assert!(s.contains("filename=\"a.wav\""));
        assert!(s.contains("audio/wav"));
        assert!(s.contains("RIFF...."));
    }

    #[test]
    fn error_of_digs_code_and_message() {
        let v: Value =
            serde_json::from_str(r#"{"code":"FILE_DOWNLOAD_FAILED","message":"下载失败"}"#)
                .unwrap();
        assert_eq!(error_of(&v), "FILE_DOWNLOAD_FAILED：下载失败");
        assert_eq!(error_of(&json!({})), "服务端没给原因");
    }

    #[test]
    fn missing_policy_fields_are_named() {
        let v: Value = serde_json::from_str(r#"{"data":{"upload_host":"https://h"}}"#).unwrap();
        let e = str_field(&v["data"], "upload_dir").unwrap_err();
        assert!(e.to_string().contains("upload_dir"), "{e}");
    }
}
