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
mod daemon;
mod desktop;
mod history;
mod hotkey;
mod icon;
mod pipeline;
mod settings_ui;
mod theme;
mod tray;
mod tui;
mod vad;

use std::path::PathBuf;

use anyhow::{bail, Result};

use agc::{lin_to_db, rms, Agc, AgcConfig};
use audio::Downsampler;
use history::History;

fn main() {
    if let Err(e) = run() {
        eprintln!("错误：{e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        // 无参数 = 托盘常驻（关掉终端也能用）
        return run_tray();
    };
    match cmd.as_str() {
        "settings" => settings_ui::run(),
        "tray" => run_tray(),
        "probe" => cmd_probe(&args[1..]),
        "transcribe" => cmd_transcribe(&args[1..]),
        "record" => cmd_record(&args[1..]),
        "doctor" => cmd_doctor(),
        "history" => tui::run(History::new(config::history_path())),
        "run" => {
            let cfg = config::load()?;
            tui::run_live(cfg, History::new(config::history_path()))
        }
        "-h" | "--help" | "help" => help(),
        other => {
            eprintln!("未知子命令：{other}\n");
            help()
        }
    }
}

fn run_tray() -> Result<()> {
    let _lock = match desktop::acquire_single_instance() {
        Some(l) => l,
        None => {
            println!("dictate 已经在跑了（托盘里找一下）。要改设置：dictate settings");
            return Ok(());
        }
    };
    let cfg = config::load()?;
    tray::run(cfg)
}

fn help() -> Result<()> {
    println!(
        "dictate — Qwen ASR 听写器\n\n\
         用法：\n  \
         dictate probe [--seconds 5] [--out out.wav]   录一段，打印电平，可落盘\n  \
         dictate transcribe <file.wav>                转写 WAV 并写入历史\n  \
         dictate record [--seconds 5]                 录一段并转写\n  \
         dictate settings                            终端里改配置（不用手写 TOML）\n  \
         dictate                                     托盘常驻（无终端）\n  \
         dictate doctor                              不发录音，验 key / 模型 / 区域\n  \
         dictate run                                  热键守护 + 终端历史（常驻）\n  \
         dictate history                              只看历史\n\n\
         配置：{}\n\
         历史：{}",
        config::config_path().display(),
        config::history_path().display()
    );
    Ok(())
}

/// 不录音也能验通路：发一段 1 秒正弦，看 key / 模型 / 区域对不对。
fn cmd_doctor() -> Result<()> {
    let cfg = config::load()?;
    println!("配置：{}", config::config_path().display());
    let style = asr::pick_style(&cfg.model, cfg.api_style.as_deref()).ok();
    let probe = asr::QwenClient::from_config(&cfg).ok();
    println!(
        "端点：{}",
        match (&probe, style) {
            (Some(c), Some(asr::Style::Ws)) => c.ws_url().unwrap_or_default(),
            _ => cfg.endpoint(),
        }
    );
    println!("模型：{}", cfg.model);
    if let Some(style) = style {
        println!("报文形状：{}", style.name());
    }
    println!(
        "区域 key：{}",
        match cfg.api_key() {
            Some(_) => "已找到",
            None => "缺失（DASHSCOPE_API_KEY 或配置文件 api_key）",
        }
    );
    print!("发一段 1 秒测试音… ");
    match pipeline::self_test(&cfg) {
        Ok((ms, text)) => {
            println!("OK（{ms} ms）");
            println!("返回文本：{text:?}");
            println!("说明：这段是正弦音，转写内容无意义，能返回就说明 key / 区域 / 模型都对。");
            Ok(())
        }
        Err(e) => {
            println!("失败");
            Err(e)
        }
    }
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
    let cfg = config::load()?;
    let wav = pipeline::prepare_wav(&cfg, &pcm, rate)?;
    let hist = History::new(config::history_path());
    let text = pipeline::transcribe_and_record(&cfg, &hist, &wav, pcm.len() as f32 / rate as f32)?;
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
    let wav = pipeline::prepare_wav(&cfg, &raw, rate)?;
    let hist = History::new(config::history_path());
    let text = pipeline::transcribe_and_record(&cfg, &hist, &wav, secs)?;
    println!("{text}");
    Ok(())
}

