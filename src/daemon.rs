//! 无终端常驻：热键 → 采集 → 转写 → 历史，状态给托盘用。

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;

use anyhow::Result;

use crate::audio::Capture;
use crate::config::Config;
use crate::history::History;
use crate::hotkey::{self, HotkeyEvent};
use crate::pipeline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Recording(Instant),
    Transcribing(Instant),
}

pub struct Daemon {
    cfg: Config,
    capture: Capture,
    hk: Receiver<HotkeyEvent>,
    worker: (Sender<Result<String, String>>, Receiver<Result<String, String>>),
    pub phase: Phase,
    pub last_error: Option<String>,
    pub last_text: Option<String>,
}

impl Daemon {
    pub fn new(cfg: Config) -> Result<Self> {
        let spec = hotkey::Spec::parse(&cfg.hotkey)?;
        let (tx, hk) = channel();
        hotkey::spawn(spec, tx)?;
        let capture = crate::audio::spawn_capture()?;
        Ok(Self {
            cfg,
            capture,
            hk,
            worker: channel(),
            phase: Phase::Idle,
            last_error: None,
            last_text: None,
        })
    }

    /// 处理一遍事件；返回托盘要显示的提示文字。
    pub fn tick(&mut self) -> String {
        while let Ok(ev) = self.hk.try_recv() {
            match (ev, self.phase) {
                (HotkeyEvent::Press, Phase::Idle) => match self.capture.start(self.cfg.max_seconds) {
                    Ok(()) => {
                        self.phase = Phase::Recording(Instant::now());
                        self.last_error = None;
                    }
                    Err(e) => self.last_error = Some(format!("启动录音失败：{e}")),
                },
                (HotkeyEvent::Release, Phase::Recording(_)) => match self.capture.stop() {
                    Ok((rate, pcm)) => {
                        let secs = pcm.len() as f32 / rate as f32;
                        self.phase = Phase::Transcribing(Instant::now());
                        let cfg = self.cfg.clone();
                        let hist = History::new(crate::config::history_path());
                        let tx = self.worker.0.clone();
                        std::thread::spawn(move || {
                            let out = pipeline::prepare_wav(&cfg, &pcm, rate)
                                .and_then(|wav| {
                                    pipeline::transcribe_and_record(&cfg, &hist, &wav, secs)
                                })
                                .map_err(|e| format!("{e:#}"));
                            let _ = tx.send(out);
                        });
                    }
                    Err(e) => {
                        self.phase = Phase::Idle;
                        self.last_error = Some(format!("取录音失败：{e}"));
                    }
                },
                _ => {}
            }
        }

        while let Ok(out) = self.worker.1.try_recv() {
            self.phase = Phase::Idle;
            match out {
                Ok(text) => {
                    self.last_text = Some(text);
                    self.last_error = None;
                }
                Err(e) => self.last_error = Some(e),
            }
        }

        match self.phase {
            Phase::Idle => match (&self.last_error, &self.last_text) {
                (Some(e), _) => format!("dictate — 失败：{}", truncate(e, 60)),
                (None, Some(t)) => format!("dictate — {}", truncate(t, 60)),
                (None, None) => "dictate — 就绪".to_string(),
            },
            Phase::Recording(t) => {
                format!("dictate — ● 录音中 {:.1}s", t.elapsed().as_secs_f32())
            }
            Phase::Transcribing(t) => {
                format!("dictate — ◐ 转写中 {:.1}s", t.elapsed().as_secs_f32())
            }
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out.replace('\n', " ")
}
