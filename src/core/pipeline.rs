//! 一条录音的加工链：PCM → 16 kHz → 裁静音 → AGC → WAV；再交给 ASR 并落历史。
//!
//! 单独成模块是为了让 CLI（`transcribe` / `record`）和热键守护（TUI）共用同一份实现。

use anyhow::{bail, Result};

use crate::audio::agc::{Agc, AgcConfig};
use crate::audio::{self, Downsampler};
use crate::core::config::Config;
use crate::core::history::{new_id, now_rfc3339, Entry, History};

/// 任意率/声道的 PCM → 16 kHz、去首尾静音、AGC、WAV 字节。
pub fn prepare_wav(cfg: &Config, pcm: &[f32], rate: u32) -> Result<Vec<u8>> {
    let mut mono = Vec::new();
    let mut ds = Downsampler::new(rate)?;
    ds.push(pcm, &mut mono)?;
    ds.finish(&mut mono)?;

    // 先 AGC 再裁静音：反过来的话，说得很轻的真话会被当成静音整段丢掉。
    let mut processed = mono;
    let mut agc = Agc::new(
        audio::TARGET_RATE,
        AgcConfig {
            target_dbfs: cfg.target_rms_dbfs,
            ..Default::default()
        },
    );
    agc.process(&mut processed);

    let peak = crate::audio::agc::lin_to_db(processed.iter().fold(0.0f32, |m, s| m.max(s.abs())));
    let trimmed = crate::audio::vad::trim_silence(&processed, audio::TARGET_RATE, 100).to_vec();
    if trimmed.is_empty() {
        bail!(
            "没有检测到语音：峰值 {peak:.0} dBFS，低于门限 {:.0} dBFS。             检查麦克风（`dictate probe --seconds 3` 看电平，或用 `dictate devices` 换设备）",
            crate::audio::vad::SPEECH_DBFS
        );
    }
    let secs = trimmed.len() as f32 / audio::TARGET_RATE as f32;
    if secs > cfg.max_seconds {
        bail!(
            "录音 {secs:.1} 秒超过上限 {:.0} 秒（服务端 10 MB / 5 分钟硬限）",
            cfg.max_seconds
        );
    }
    let voiced = trimmed;

    if cfg.keep_audio {
        let keep = crate::core::config::status_path()
            .parent()
            .map(|d| d.join("last.wav"))
            .unwrap_or_else(|| std::env::temp_dir().join("last.wav"));
        audio::write_wav(&keep, &voiced).ok();
    }
    Ok(audio::wav_bytes(&voiced))
}

/// 不录音也能验通路：发一段 1 秒正弦，返回（耗时 ms，文本）。
pub fn self_test(cfg: &Config) -> Result<(u64, String)> {
    let samples: Vec<f32> = (0..audio::TARGET_RATE)
        .map(|i| {
            0.1 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / audio::TARGET_RATE as f32).sin()
        })
        .collect();
    let wav = audio::wav_bytes(&samples);
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
