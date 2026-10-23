//! 一条录音的加工链：PCM → 16 kHz → 裁静音 → AGC → WAV；再交给 ASR 并落历史。
//!
//! 单独成模块是为了让 CLI（`transcribe` / `record`）和热键守护（TUI）共用同一份实现。

use anyhow::{bail, Context, Result};

use crate::agc::{Agc, AgcConfig};
use crate::audio::{self, Downsampler};
use crate::config::Config;
use crate::history::{new_id, now_rfc3339, Entry, History};

/// 任意率/声道的 PCM → 16 kHz、去首尾静音、AGC、WAV 字节。
pub fn prepare_wav(cfg: &Config, pcm: &[f32], rate: u32) -> Result<Vec<u8>> {
    let mut mono = Vec::new();
    let mut ds = Downsampler::new(rate)?;
    ds.push(pcm, &mut mono)?;
    ds.finish(&mut mono)?;

    let trimmed = crate::vad::trim_silence(&mono, audio::TARGET_RATE, 100).to_vec();
    if trimmed.is_empty() {
        bail!("这段里没有检测到语音（全在 {} dBFS 以下）", crate::vad::SPEECH_DBFS);
    }
    let secs = trimmed.len() as f32 / audio::TARGET_RATE as f32;
    if secs > cfg.max_seconds {
        bail!(
            "录音 {secs:.1} 秒超过上限 {:.0} 秒（服务端 10 MB / 5 分钟硬限）",
            cfg.max_seconds
        );
    }
    let mut voiced = trimmed;
    let mut agc = Agc::new(
        audio::TARGET_RATE,
        AgcConfig { target_dbfs: cfg.target_rms_dbfs, ..Default::default() },
    );
    agc.process(&mut voiced);

    let path = std::env::temp_dir().join(format!("dictate-{}.wav", new_id()));
    audio::write_wav(&path, &voiced)?;
    let bytes =
        std::fs::read(&path).with_context(|| format!("读回临时 WAV 失败：{}", path.display()))?;
    std::fs::remove_file(&path).ok();
    Ok(bytes)
}

/// 不录音也能验通路：发一段 1 秒正弦，返回（耗时 ms，文本）。
pub fn self_test(cfg: &Config) -> Result<(u64, String)> {
    let samples: Vec<f32> = (0..audio::TARGET_RATE)
        .map(|i| {
            0.1 * (2.0 * std::f32::consts::PI * 440.0 * i as f32
                / audio::TARGET_RATE as f32)
                .sin()
        })
        .collect();
    let path = std::env::temp_dir().join(format!("dictate-selftest-{}.wav", new_id()));
    audio::write_wav(&path, &samples)?;
    let wav = std::fs::read(&path)?;
    std::fs::remove_file(&path).ok();
    let client = crate::asr::QwenClient::from_config(cfg)?;
    let t = client.transcribe_wav(&wav)?;
    Ok((t.latency_ms, t.text))
}

/// 转写并把结果写进历史；失败也写一条（带错误原因），返回值仍然是错误。
pub fn transcribe_and_record(
    cfg: &Config,
    hist: &History,
    wav: &[u8],
    duration_secs: f32,
) -> Result<String> {
    let client = crate::asr::QwenClient::from_config(cfg)?;
    let entry = match client.transcribe_wav(wav) {
        Ok(t) => Entry {
            id: new_id(),
            ts: now_rfc3339(),
            duration_ms: (duration_secs * 1000.0) as u64,
            latency_ms: t.latency_ms,
            model: cfg.model.clone(),
            status: "ok".into(),
            text: t.text.clone(),
        },
        Err(e) => {
            hist.append(&Entry {
                id: new_id(),
                ts: now_rfc3339(),
                duration_ms: (duration_secs * 1000.0) as u64,
                latency_ms: 0,
                model: cfg.model.clone(),
                status: format!("error: {e:#}"),
                text: String::new(),
            })
            .ok();
            return Err(e);
        }
    };
    hist.append(&entry)?;
    Ok(entry.text)
}
