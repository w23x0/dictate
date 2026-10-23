//! 通义千问 ASR（OpenAI 兼容模式）。
//!
//! 形状固定，不做 provider 抽象：
//! `POST {base}/compatible-mode/v1/chat/completions`
//! `messages[0].content[0] = {type:"input_audio", input_audio:{data:"data:audio/wav;base64,…"}}`
//! 结果在 `choices[0].message.content`。

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use crate::config::Config;

pub struct Transcript {
    pub text: String,
    pub latency_ms: u64,
}

pub struct QwenClient {
    endpoint: String,
    key: String,
    model: String,
    language: Option<String>,
    enable_itn: bool,
    timeout: Duration,
}

impl QwenClient {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let key = cfg
            .api_key()
            .ok_or_else(|| anyhow!("缺少 API key：设置 DASHSCOPE_API_KEY 或配置文件 api_key"))?;
        Ok(Self {
            endpoint: cfg.endpoint(),
            key,
            model: cfg.model.clone(),
            language: cfg.language.clone().filter(|s| !s.trim().is_empty()),
            enable_itn: cfg.enable_itn,
            timeout: Duration::from_secs(cfg.timeout_secs.max(5)),
        })
    }

    pub fn request_body(&self, wav: &[u8]) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD.encode(wav);
        let mut asr = json!({ "enable_itn": self.enable_itn });
        if let Some(lang) = &self.language {
            asr["language"] = json!(lang);
        }
        json!({
            "model": self.model,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "input_audio",
                    "input_audio": { "data": format!("data:audio/wav;base64,{b64}") }
                }]
            }],
            "asr_options": asr,
            "stream": false
        })
    }

    pub fn transcribe_wav(&self, wav: &[u8]) -> Result<Transcript> {
        let started = Instant::now();
        let client = reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .build()
            .context("建 HTTP 客户端失败")?;
        let resp = client
            .post(&self.endpoint)
            .bearer_auth(&self.key)
            .json(&self.request_body(wav))
            .send()
            .context("请求失败（网络）")?;

        let status = resp.status();
        let body = resp.text().context("读响应体失败")?;
        if !status.is_success() {
            let msg = extract_error(&body).unwrap_or_else(|| body.chars().take(300).collect());
            bail!("HTTP {status}：{msg}");
        }
        let text = parse_content(&body)?;
        Ok(Transcript {
            text,
            latency_ms: started.elapsed().as_millis() as u64,
        })
    }
}

/// 从成功响应里取 `choices[0].message.content`。
pub fn parse_content(body: &str) -> Result<String> {
    let v: Value = serde_json::from_str(body).context("响应不是 JSON")?;
    let content = v
        .pointer("/choices/0/message/content")
        .ok_or_else(|| anyhow!("响应缺少 choices[0].message.content"))?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        other => bail!("无法识别的 content 形状：{other}"),
    };
    Ok(text.trim().to_string())
}

fn extract_error(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.pointer("/error/message")
        .or_else(|| v.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(language: Option<&str>) -> QwenClient {
        QwenClient {
            endpoint: "http://127.0.0.1:1/x".into(),
            key: "k".into(),
            model: "qwen3-asr-flash".into(),
            language: language.map(|s| s.to_string()),
            enable_itn: false,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn body_carries_data_url_and_options() {
        let body = client(Some("zh")).request_body(b"RIFFxxxx");
        assert_eq!(body["model"], "qwen3-asr-flash");
        let audio = body.pointer("/messages/0/content/0/input_audio/data").unwrap();
        assert!(audio.as_str().unwrap().starts_with("data:audio/wav;base64,"));
        assert_eq!(body["asr_options"]["language"], "zh");
        assert_eq!(body["asr_options"]["enable_itn"], false);
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn language_is_omitted_when_auto() {
        let body = client(None).request_body(b"x");
        assert!(body["asr_options"].get("language").is_none());
    }

    #[test]
    fn parses_plain_and_partial_content() {
        let plain = r#"{"choices":[{"message":{"content":"你好，世界"}}]}"#;
        assert_eq!(parse_content(plain).unwrap(), "你好，世界");
        let parts = r#"{"choices":[{"message":{"content":[{"text":"你"},{"text":"好"}]}}]}"#;
        assert_eq!(parse_content(parts).unwrap(), "你好");
    }

    #[test]
    fn rejects_malformed_responses() {
        assert!(parse_content("not json").is_err());
        assert!(parse_content(r#"{"choices":[]}"#).is_err());
    }

    #[test]
    fn extracts_error_message() {
        let e = extract_error(r#"{"error":{"message":"Invalid API-key provided."}}"#);
        assert_eq!(e.as_deref(), Some("Invalid API-key provided."));
    }
}
