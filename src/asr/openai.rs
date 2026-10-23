//! OpenAI 兼容形状：`chat/completions` + `input_audio` 的 data URL。
//!
//! 只有 `qwen3-asr-flash` 走这条（官方文档唯一支持 OpenAI 兼容模式的 ASR 模型）。

use anyhow::Result;
use serde_json::json;

use super::{text_from, QwenClient};

pub fn request_body(c: &QwenClient, wav: &[u8]) -> serde_json::Value {
    let mut asr = json!({ "enable_itn": c.enable_itn });
    if let Some(lang) = &c.language {
        asr["language"] = json!(lang);
    }
    json!({
        "model": c.model,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "input_audio",
                "input_audio": { "data": c.data_url(wav) }
            }]
        }],
        "asr_options": asr,
        "stream": false
    })
}

pub fn transcribe(c: &QwenClient, wav: &[u8]) -> Result<String> {
    let body = request_body(c, wav);
    let raw = super::post_json(c, &c.http_endpoint(), &body, &[])?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|_| {
        anyhow::anyhow!("响应不是 JSON：{}", raw.chars().take(200).collect::<String>())
    })?;
    text_from(&v, &["/choices/0/message/content", "/choices/0/text"])
}
