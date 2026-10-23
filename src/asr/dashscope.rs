//! DashScope 同步形状：`/api/v1/services/aigc/multimodal-generation/generation`。
//!
//! 两个家族的内容字段不一样，是官方文档里两套示例：
//! * `qwen3-asr-flash` → `content: [{"audio": "<data url>"}]`，参数走 `parameters.asr_options`
//! * `qwen-audio-3.x-asr-flash` → `content: [{"type":"input_audio","input_audio":{"data":…}}]`，
//!   参数走 `parameters.format` / `parameters.sample_rate`

use anyhow::Result;
use serde_json::json;

use super::{text_from, QwenClient};

pub fn is_qwen3_family(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("qwen3-asr")
}

pub fn request_body(c: &QwenClient, wav: &[u8]) -> serde_json::Value {
    let data = c.data_url(wav);
    if is_qwen3_family(&c.model) {
        let mut asr = json!({ "enable_itn": c.enable_itn });
        if let Some(lang) = &c.language {
            asr["language"] = json!(lang);
        }
        json!({
            "model": c.model,
            "input": { "messages": [{
                "role": "user",
                "content": [{ "audio": data }]
            }]},
            "parameters": { "asr_options": asr }
        })
    } else {
        json!({
            "model": c.model,
            "input": { "messages": [{
                "role": "user",
                "content": [{
                    "type": "input_audio",
                    "input_audio": { "data": data }
                }]
            }]},
            "parameters": {
                "format": "wav",
                "sample_rate": "16000"
            }
        })
    }
}

pub fn transcribe(c: &QwenClient, wav: &[u8]) -> Result<String> {
    let body = request_body(c, wav);
    let raw = super::post_json(
        c,
        &c.http_endpoint(),
        &body,
        &[("X-DashScope-SSE", "disable")],
    )?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|_| {
        anyhow::anyhow!(
            "响应不是 JSON：{}",
            raw.chars().take(200).collect::<String>()
        )
    })?;
    // 标准形状是 output.choices[0].message.content[0].text；
    // 有的模型只回 output.text，兜一下。
    text_from(
        &v,
        &[
            "/output/choices/0/message/content",
            "/output/choices/0/message/content/0/text",
            "/output/text",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn client(model: &str) -> QwenClient {
        QwenClient {
            style: super::super::Style::DashScope,
            base: "https://x.example".into(),
            key: "k".into(),
            model: model.into(),
            language: Some("zh".into()),
            enable_itn: true,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn qwen3_uses_audio_field_and_asr_options() {
        let b = request_body(&client("qwen3-asr-flash"), b"RIFF");
        assert!(b["input"]["messages"][0]["content"][0]["audio"]
            .as_str()
            .unwrap()
            .starts_with("data:audio/wav;base64,"));
        assert_eq!(b["parameters"]["asr_options"]["language"], "zh");
        assert_eq!(b["parameters"]["asr_options"]["enable_itn"], true);
        assert!(b["parameters"].get("format").is_none());
    }

    #[test]
    fn audio_family_uses_input_audio_and_format() {
        let b = request_body(&client("qwen-audio-3.1-asr-flash"), b"RIFF");
        assert_eq!(
            b["input"]["messages"][0]["content"][0]["type"],
            "input_audio"
        );
        assert_eq!(b["parameters"]["format"], "wav");
        assert_eq!(b["parameters"]["sample_rate"], "16000");
    }
}
