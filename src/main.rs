//! dictate — 通义千问 ASR 听写器。
//!
//! 子命令：
//! * `probe [--seconds N] [--out f.wav]` 录一段，打印 RMS，落 WAV
//! * `transcribe <file.wav>`              转写一个 WAV，打印并写历史
//! * `record [--seconds N]`               录一段并直接转写
//! * `history`                            终端历史窗口

mod agc;
mod asr;
mod audio;
mod config;
mod history;
mod tui;
mod vad;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use agc::{lin_to_db, rms, Agc, AgcConfig};
use audio::Downsampler;
use history::{new_id, now_rfc3339, Entry, History};

fn main() {
    if let Err(e) = run() {
        eprintln!("错误：{e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        return help();
    };
    match cmd.as_str() {
        "probe" => cmd_probe(&args[1..]),
        "transcribe" => cmd_transcribe(&args[1..]),
        "record" => cmd_record(&args[1..]),
        "history" => tui::run(History::new(config::history_path())),
        "-h" | "--help" | "help" => help(),
        other => {
            eprintln!("未知子命令：{other}\n");
            help()
        }
    }
}

fn help() -> Result<()> {
    println!(
        "dictate — Qwen ASR 听写器\n\n\
         用法：\n  \
         dictate probe [--seconds 5] [--out out.wav]   录一段，打印电平，可落盘\n  \
         dictate transcribe <file.wav>                转写 WAV 并写入历史\n  \
         dictate record [--seconds 5]                 录一段并转写\n  \
         dictate history                              终端历史窗口\n\n\
         配置：{}\n\
         历史：{}",
        config::config_path().display(),
        config::history_path().display()
    );
    Ok(())
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).map(|s| s.as_str())
}

fn cmd_probe(args: &[String]) -> Result<()> {
    let seconds: f32 = flag(args, "--seconds")
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(5.0);
    let out = flag(args, "--out").map(PathBuf::from);

    println!("录音 {seconds} 秒…");
    let (rate, raw) = audio::record_seconds(seconds)?;
    println!("设备采样率 {rate} Hz，{} 个样本", raw.len());

    let mut pcm = Vec::new();
    let mut ds = Downsampler::new(rate)?;
    ds.push(&raw, &mut pcm)?;
    ds.finish(&mut pcm)?;

    let before = lin_to_db(rms(&pcm));
    let trimmed = vad::trim_silence(&pcm, audio::TARGET_RATE, 100);
    let mut voiced = trimmed.to_vec();
    let mut agc = Agc::new(
        audio::TARGET_RATE,
        AgcConfig {
            target_dbfs: config::load()?.target_rms_dbfs,
            ..Default::default()
        },
    );
    let gain = agc.process(&mut voiced);
    let after = lin_to_db(rms(&voiced));

    println!(
        "16 kHz 后：{} 样本（{:.2} s），原始 {before:.1} dBFS，语音段 {} 样本，AGC 后 {after:.1} dBFS（增益 ×{gain:.2}）",
        pcm.len(),
        pcm.len() as f32 / audio::TARGET_RATE as f32,
        voiced.len()
    );

    if let Some(path) = out {
        audio::write_wav(&path, &voiced)?;
        println!("已写 {}", path.display());
    }
    Ok(())
}

fn cmd_transcribe(args: &[String]) -> Result<()> {
    let Some(path) = args.first() else {
        bail!("用法：dictate transcribe <file.wav>");
    };
    let (rate, pcm) = audio::read_wav(std::path::Path::new(path))?;
    let wav = prepare_wav(&pcm, rate)?;
    let cfg = config::load()?;
    let text = transcribe_and_record(&cfg, &wav, pcm.len() as f32 / rate as f32)?;
    println!("{text}");
    Ok(())
}

fn cmd_record(args: &[String]) -> Result<()> {
    let cfg = config::load()?;
    let max: f32 = flag(args, "--seconds")
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(cfg.max_seconds);
    println!(
        "录音中…（最长 {max:.0} 秒，静音 {:.1} 秒自动结束）",
        cfg.silence_stop_ms as f32 / 1000.0
    );
    let (rate, raw) = audio::record_until(max, cfg.silence_stop_ms)?;
    let secs = raw.len() as f32 / rate as f32;
    let wav = prepare_wav(&raw, rate)?;
    let text = transcribe_and_record(&cfg, &wav, secs)?;
    println!("{text}");
    Ok(())
}

/// 任意率/声道的 PCM → 16 kHz、去首尾静音、AGC、WAV 字节。
fn prepare_wav(pcm: &[f32], rate: u32) -> Result<Vec<u8>> {
    let cfg = config::load()?;
    let mut mono = Vec::new();
    let mut ds = Downsampler::new(rate)?;
    ds.push(pcm, &mut mono)?;
    ds.finish(&mut mono)?;

    let trimmed = vad::trim_silence(&mono, audio::TARGET_RATE, 100).to_vec();
    if trimmed.is_empty() {
        bail!("这段里没有检测到语音（全在 {} dBFS 以下）", vad::SPEECH_DBFS);
    }
    if trimmed.len() as f32 / audio::TARGET_RATE as f32 > cfg.max_seconds {
        bail!(
            "录音 {:.1} 秒超过上限 {:.0} 秒（服务端 10 MB / 5 分钟硬限）",
            trimmed.len() as f32 / audio::TARGET_RATE as f32,
            cfg.max_seconds
        );
    }
    let mut voiced = trimmed;
    let mut agc = Agc::new(
        audio::TARGET_RATE,
        AgcConfig {
            target_dbfs: cfg.target_rms_dbfs,
            ..Default::default()
        },
    );
    agc.process(&mut voiced);

    let path = std::env::temp_dir().join(format!("dictate-{}.wav", new_id()));
    audio::write_wav(&path, &voiced)?;
    let bytes = std::fs::read(&path).with_context(|| format!("读回临时 WAV 失败：{}", path.display()))?;
    std::fs::remove_file(&path).ok();
    Ok(bytes)
}

fn transcribe_and_record(cfg: &config::Config, wav: &[u8], duration_secs: f32) -> Result<String> {
    let client = asr::QwenClient::from_config(cfg)?;
    let hist = History::new(config::history_path());
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
            let entry = Entry {
                id: new_id(),
                ts: now_rfc3339(),
                duration_ms: (duration_secs * 1000.0) as u64,
                latency_ms: 0,
                model: cfg.model.clone(),
                status: format!("error: {e:#}"),
                text: String::new(),
            };
            hist.append(&entry).ok();
            return Err(e);
        }
    };
    hist.append(&entry)?;
    Ok(entry.text)
}
