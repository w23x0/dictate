//! 通义千问 ASR：三种报文形状，按模型名自动选，也可用配置 `api_style` 强制。
//!
//! | 模型 | 形状 | 端点 |
//! |---|---|---|
//! | `qwen3-asr-flash` | OpenAI 兼容 | `{base}/compatible-mode/v1/chat/completions` |
//! | `qwen-audio-3.x-asr-flash` | DashScope 同步 | `{base}/api/v1/services/aigc/multimodal-generation/generation` |
//! | `…-message` / `…-streaming` / `qwen3-asr-flash-realtime` | WebSocket | `wss://{host}/api-ws/v1/inference` |
//!
//! `*-filetrans` 走的是「上传 → 建异步任务 → 轮询」，本项目不实现，选到它会直接报错，
//! 而不是发一个注定失败的请求。

mod dashscope;
mod openai;
pub(crate) mod ws;

#[cfg(test)]
mod e2e;

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Url;
use serde_json::Value;

use crate::core::config::Config;

pub struct Transcript {
    pub text: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `chat/completions` + `input_audio`
    OpenAi,
    /// `multimodal-generation/generation`（同步）
    DashScope,
    /// `api-ws/v1/inference`（run-task / finish-task）
    Ws,
}

impl Style {
    pub fn name(&self) -> &'static str {
        match self {
            Style::OpenAi => "openai-compatible",
            Style::DashScope => "dashscope-sync",
            Style::Ws => "websocket",
        }
    }
}

pub struct QwenClient {
    pub style: Style,
    /// 归一化后的站点根，例如 `https://dashscope.aliyuncs.com`
    pub base: String,
    pub key: String,
    pub model: String,
    pub language: Option<String>,
    pub enable_itn: bool,
    pub timeout: Duration,
}

impl QwenClient {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let key = cfg
            .api_key()
            .ok_or_else(|| anyhow!("缺少 API key：设置 DASHSCOPE_API_KEY 或配置文件 api_key"))?;
        let style = pick_style(&cfg.model, cfg.api_style.as_deref())?;
        Ok(Self {
            style,
            base: normalize_base(&cfg.base_url)?,
            key,
            model: cfg.model.clone(),
            language: cfg.language.clone().filter(|s| !s.trim().is_empty()),
            enable_itn: cfg.enable_itn,
            timeout: Duration::from_secs(cfg.timeout_secs.max(5)),
        })
    }

    pub fn transcribe_wav(&self, wav: &[u8]) -> Result<Transcript> {
        let started = std::time::Instant::now();
        let text = match self.style {
            Style::OpenAi => openai::transcribe(self, wav)?,
            Style::DashScope => dashscope::transcribe(self, wav)?,
            Style::Ws => ws::transcribe(self, wav)?,
        };
        Ok(Transcript {
            text,
            latency_ms: started.elapsed().as_millis() as u64,
        })
    }

    pub fn http_endpoint(&self) -> String {
        match self.style {
            Style::OpenAi => format!("{}/compatible-mode/v1/chat/completions", self.base),
            Style::DashScope => {
                format!(
                    "{}/api/v1/services/aigc/multimodal-generation/generation",
                    self.base
                )
            }
            Style::Ws => format!("{}/api-ws/v1/inference", self.base),
        }
    }

    pub fn ws_url(&self) -> Result<String> {
        let url = Url::parse(&self.base)?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("base_url 里没有主机名：{}", self.base))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        Ok(format!("{scheme}://{host}{port}/api-ws/v1/inference"))
    }

    pub fn data_url(&self, wav: &[u8]) -> String {
        use base64::Engine;
        format!(
            "data:audio/wav;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(wav)
        )
    }
}

/// 把用户填的 base_url 归一化成站点根：`…/compatible-mode/v1` 之类的尾巴都剥掉。
pub fn normalize_base(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let url = Url::parse(trimmed).with_context(|| format!("base_url 不是合法 URL：{raw}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("base_url 里没有主机名：{raw}"))?;
    let mut out = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    Ok(out)
}

/// 模型名 → 报文形状。`api_style` 有值时以它为准（`openai` / `dashscope` / `ws`）。
pub fn pick_style(model: &str, configured: Option<&str>) -> Result<Style> {
    let m = model.to_ascii_lowercase();
    if m.contains("filetrans") {
        bail!(
            "`{model}` 只能走 DashScope 异步任务（上传 → 建任务 → 轮询），本项目没实现；\
             换用同步模型（qwen3-asr-flash / qwen-audio-3.1-asr-flash）"
        );
    }
    if let Some(style) = configured {
        return match style.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" => Ok(Style::OpenAi),
            "dashscope" | "dashscope-sync" => Ok(Style::DashScope),
            "ws" | "websocket" | "realtime" => Ok(Style::Ws),
            other => bail!("未知的 api_style：{other}（可选 openai / dashscope / ws）"),
        };
    }
    if m.ends_with("-message") || m.contains("-streaming") || m.ends_with("-realtime") {
        return Ok(Style::Ws);
    }
    if m.starts_with("qwen-audio-3") {
        return Ok(Style::DashScope);
    }
    Ok(Style::OpenAi)
}

/// 从 JSON 里依次尝试几种取文本的位置。
pub fn text_from(value: &Value, pointers: &[&str]) -> Result<String> {
    for p in pointers {
        if let Some(v) = value.pointer(p) {
            match v {
                Value::String(s) => return Ok(s.trim().to_string()),
                Value::Array(parts) => {
                    let joined: String = parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect();
                    return Ok(joined.trim().to_string());
                }
                _ => {}
            }
        }
    }
    bail!("响应里找不到文本字段（试过 {}）", pointers.join(", "))
}

/// 发一个 JSON POST，非 2xx 时把服务端的 message 抛出来。
pub(crate) fn post_json(
    c: &QwenClient,
    url: &str,
    body: &Value,
    extra_headers: &[(&str, &str)],
) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(c.timeout)
        .build()
        .context("建 HTTP 客户端失败")?;
    let mut req = client.post(url).bearer_auth(&c.key).json(body);
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().context("请求失败（网络）")?;
    let status = resp.status();
    let text = resp.text().context("读响应体失败")?;
    if !status.is_success() {
        let msg = humanize_error(&text).unwrap_or_else(|| text.chars().take(300).collect());
        bail!("HTTP {status}：{msg}");
    }
    Ok(text)
}

/// 把服务端错误码翻成人话。
pub fn humanize_error(body: &str) -> Option<String> {
    let raw = error_message(body)?;
    let hint = if raw.contains("ASR_RESPONSE_HAVE_NO_WORDS") {
        "服务端没从音频里听出任何词（多半是静音或只有噪声）——检查麦克风，或先用 `dictate probe --seconds 3` 看电平"
    } else if raw.contains("InvalidApiKey") || raw.contains("Unauthorized") {
        "API key 不对，或与所在区域不匹配"
    } else if raw.contains("Throttling") || raw.contains("429") {
        "请求太频繁，稍后再试"
    } else if raw.contains("ModelNotFound") || raw.contains("model not exist") {
        "这个模型不存在或没有开通"
    } else {
        return Some(raw);
    };
    Some(format!("{hint}（原始：{raw}）"))
}

pub fn error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.pointer("/error/message")
        .or_else(|| v.pointer("/output/message"))
        .or_else(|| v.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_inference_covers_model_families() {
        assert_eq!(pick_style("qwen3-asr-flash", None).unwrap(), Style::OpenAi);
        assert_eq!(
            pick_style("qwen-audio-3.1-asr-flash", None).unwrap(),
            Style::DashScope
        );
        assert_eq!(
            pick_style("qwen-audio-3.0-asr-flash", None).unwrap(),
            Style::DashScope
        );
        assert_eq!(
            pick_style("qwen-audio-3.1-asr-flash-message", None).unwrap(),
            Style::Ws
        );
        assert_eq!(
            pick_style("qwen-audio-3.1-asr-flash-streaming", None).unwrap(),
            Style::Ws
        );
        assert_eq!(
            pick_style("qwen3-asr-flash-realtime", None).unwrap(),
            Style::Ws
        );
        assert!(pick_style("qwen3-asr-flash-filetrans", None).is_err());
    }

    #[test]
    fn explicit_style_wins() {
        assert_eq!(
            pick_style("qwen3-asr-flash", Some("dashscope")).unwrap(),
            Style::DashScope
        );
        assert!(pick_style("qwen3-asr-flash", Some("nope")).is_err());
    }

    #[test]
    fn base_normalization_strips_known_suffixes() {
        let b = normalize_base("https://ws-45tgzezfnpme08ey.cn-beijing.maas.aliyuncs.com").unwrap();
        assert_eq!(
            b,
            "https://ws-45tgzezfnpme08ey.cn-beijing.maas.aliyuncs.com"
        );
        let b = normalize_base("https://dashscope.aliyuncs.com/compatible-mode/v1/").unwrap();
        assert_eq!(b, "https://dashscope.aliyuncs.com");
        assert!(normalize_base("not a url").is_err());
    }

    #[test]
    fn ws_url_follows_scheme() {
        let client = QwenClient {
            style: Style::Ws,
            base: "https://ws-x.cn-beijing.maas.aliyuncs.com".into(),
            key: "k".into(),
            model: "qwen-audio-3.1-asr-flash-message".into(),
            language: None,
            enable_itn: false,
            timeout: Duration::from_secs(5),
        };
        assert_eq!(
            client.ws_url().unwrap(),
            "wss://ws-x.cn-beijing.maas.aliyuncs.com/api-ws/v1/inference"
        );
        assert_eq!(
            client.http_endpoint(),
            "https://ws-x.cn-beijing.maas.aliyuncs.com/api-ws/v1/inference"
        );
    }

    #[test]
    fn server_error_codes_are_explained() {
        let body = r#"{"request_id":"x","code":"InvalidParameter","message":"generate asr words failed. ASR_RESPONSE_HAVE_NO_WORDS"}"#;
        let msg = humanize_error(body).unwrap();
        assert!(msg.contains("没从音频里听出任何词"), "{msg}");
        assert!(
            msg.contains("ASR_RESPONSE_HAVE_NO_WORDS"),
            "要保留原始错误码"
        );
    }

    #[test]
    fn text_extraction_handles_string_and_parts() {
        let v: Value = serde_json::from_str(r#"{"a":"  你好  "}"#).unwrap();
        assert_eq!(text_from(&v, &["/a"]).unwrap(), "你好");
        let v: Value = serde_json::from_str(r#"{"o":[{"text":"你"},{"text":"好"}]}"#).unwrap();
        assert_eq!(text_from(&v, &["/o"]).unwrap(), "你好");
        assert!(text_from(&serde_json::json!({}), &["/x"]).is_err());
    }
}
