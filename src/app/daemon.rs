//! 无终端常驻：热键 → 采集 → 转写 → 历史，状态给托盘用。

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;

use anyhow::Result;

use crate::audio::Capture;
use crate::core::config::Config;
use crate::core::history::History;
use crate::hotkey::{self, HotkeyEvent};
use crate::pipeline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Recording(Instant),
    Transcribing(Instant),
}

/// 转写线程 → 主循环的结果通道。
type Worker = (
    Sender<Result<String, String>>,
    Receiver<Result<String, String>>,
);

pub struct Daemon {
    cfg: Config,
    published: String,
    capture: Capture,
    hk: Receiver<HotkeyEvent>,
    hk_spec: hotkey::SharedSpec,
    worker: Worker,
    /// 上次看到的配置文件 mtime；变了就热加载（模型/key/交付方式/热键都不用重启）。
    cfg_mtime: Option<std::time::SystemTime>,
    pub phase: Phase,
    /// 本次录音的开始时刻（判断"误触"用）
    started: Option<Instant>,
    pub quit: bool,
    pub last_error: Option<String>,
    /// 最近一次交付方式（托盘提示用）
    pub paste_note: Option<String>,
    pub last_text: Option<String>,
}

impl Daemon {
    pub fn new(cfg: Config) -> Result<Self> {
        let spec = hotkey::Spec::parse(&cfg.hotkey)?;
        let (tx, hk) = channel();
        let hk_spec = hotkey::spawn(spec, tx, true)?;
        let capture = crate::audio::spawn_capture(cfg.input_device.clone())?;
        let cfg_mtime = std::fs::metadata(crate::core::config::config_path())
            .and_then(|m| m.modified())
            .ok();
        Ok(Self {
            cfg,
            published: String::new(),
            capture,
            hk,
            hk_spec,
            worker: channel(),
            cfg_mtime,
            phase: Phase::Idle,
            started: None,
            quit: false,
            last_error: None,
            paste_note: None,
            last_text: None,
        })
    }

    /// 把状态写进 `status.json`，供 `dictate run` 的终端视图读取。
    fn publish(&mut self) {
        let (phase, since) = match self.phase {
            Phase::Idle => ("idle", 0),
            Phase::Recording(t) => ("recording", now_ms(t)),
            Phase::Transcribing(t) => ("transcribing", now_ms(t)),
        };
        let v = serde_json::json!({
            "phase": phase,
            "since_ms": since,
            "text": self.last_text,
            "error": self.last_error,
        });
        let body = v.to_string();
        if body == self.published {
            return;
        }
        let path = crate::core::config::status_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &body).is_ok() {
            std::fs::rename(&tmp, &path).ok();
            self.published = body;
        }
    }

    /// 手动开始录音（来自终端视图或托盘）。
    pub fn start(&mut self) {
        if self.phase != Phase::Idle {
            return;
        }
        match self.capture.start(self.cfg.max_seconds) {
            Ok(()) => {
                self.started = Some(Instant::now());
                self.phase = Phase::Recording(Instant::now());
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(format!("启动录音失败：{e}")),
        }
    }

    /// 取消本次录音（不转写）。
    pub fn cancel(&mut self) {
        if matches!(self.phase, Phase::Recording(_)) {
            let _ = self.capture.stop();
            self.phase = Phase::Idle;
            self.started = None;
            self.last_error = None;
        }
    }

    /// 手动提交（停止并转写）。
    pub fn stop(&mut self) {
        if !matches!(self.phase, Phase::Recording(_)) {
            return;
        }
        // 误触：按一下就松（< 200 ms）直接丢弃，免得每次点错都产生一条空记录
        let elapsed = self.started.map(|t| t.elapsed()).unwrap_or_default();
        if elapsed < std::time::Duration::from_millis(200) {
            self.cancel();
            self.last_error = Some("按键太短，已忽略".into());
            return;
        }
        self.started = None;
        match self.capture.stop() {
            Ok((rate, pcm)) => {
                let secs = pcm.len() as f32 / rate as f32;
                self.phase = Phase::Transcribing(Instant::now());
                let cfg = self.cfg.clone();
                let hist = History::new(crate::core::config::history_path());
                let tx = self.worker.0.clone();
                std::thread::spawn(move || {
                    let out = pipeline::prepare_wav(&cfg, &pcm, rate)
                        .and_then(|wav| pipeline::transcribe_and_record(&cfg, &hist, &wav, secs))
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(out);
                });
            }
            Err(e) => {
                self.phase = Phase::Idle;
                self.last_error = Some(format!("取录音失败：{e}"));
            }
        }
    }

    /// 读一次控制文件（终端视图 / 外部脚本下命令）。
    fn poll_control(&mut self) {
        let path = crate::core::config::control_path();
        let Ok(cmd) = std::fs::read_to_string(&path) else {
            return;
        };
        std::fs::remove_file(&path).ok();
        match cmd.trim() {
            "start" => self.start(),
            "stop" => self.stop(),
            "quit" => self.quit = true,
            other => self.last_error = Some(format!("未知控制命令：{other}")),
        }
    }

    /// 配置文件变了就热加载：模型 / key / 交付方式即时生效，热键也直接换
    ///（Linux 监听线程每次按键重读共享定义）。Windows 端热键仍需重启进程。
    fn poll_config(&mut self) {
        let mtime = std::fs::metadata(crate::core::config::config_path())
            .and_then(|m| m.modified())
            .ok();
        if mtime == self.cfg_mtime {
            return;
        }
        self.cfg_mtime = mtime;
        match crate::core::config::load() {
            Ok(cfg) => match hotkey::Spec::parse(&cfg.hotkey) {
                Ok(spec) => {
                    *self.hk_spec.write().unwrap_or_else(|e| e.into_inner()) = spec;
                    self.cfg = cfg;
                }
                Err(e) => {
                    // 热键没看懂就不动它，其余配置照常生效
                    self.cfg = cfg;
                    self.last_error = Some(format!("热键配置无效（{e}），沿用旧热键"));
                }
            },
            Err(e) => self.last_error = Some(format!("重读配置失败：{e}")),
        }
    }

    /// 处理一遍事件；返回托盘要显示的提示文字。
    pub fn tick(&mut self) -> String {
        self.poll_control();
        self.poll_config();
        while let Ok(ev) = self.hk.try_recv() {
            let toggle = self.cfg.hotkey_mode.eq_ignore_ascii_case("toggle");
            match (ev, self.phase) {
                (HotkeyEvent::Press, Phase::Recording(_)) if toggle => self.stop(),
                (HotkeyEvent::Release, _) if toggle => {}
                (HotkeyEvent::Press, Phase::Idle) => self.start(),
                (HotkeyEvent::Release, Phase::Recording(_)) => self.stop(),
                _ => {}
            }
        }

        while let Ok(out) = self.worker.1.try_recv() {
            self.phase = Phase::Idle;
            match out {
                Ok(text) => {
                    // 原版逻辑：识别完直接送到当前焦点窗口（剪贴板 + 模拟粘贴）
                    let mode = crate::app::inject::Mode::parse(&self.cfg.paste);
                    match crate::app::inject::deliver(&text, mode) {
                        Ok(how) => {
                            self.last_text = Some(text);
                            self.paste_note = Some(how);
                            self.last_error = None;
                        }
                        Err(e) => {
                            self.last_text = Some(text);
                            self.paste_note = None;
                            self.last_error = Some(format!("交付失败：{e}"));
                        }
                    }
                }
                Err(e) => self.last_error = Some(e),
            }
        }

        self.publish();
        match self.phase {
            Phase::Idle => match (&self.last_error, &self.last_text) {
                (Some(e), _) => format!("dictate — 失败：{}", truncate(e, 60)),
                (None, Some(t)) => format!(
                    "dictate — {}（{}）",
                    truncate(t, 50),
                    self.paste_note.as_deref().unwrap_or("已识别")
                ),
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

fn now_ms(t: Instant) -> u64 {
    let elapsed = t.elapsed().as_millis() as u64;
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64)
        .saturating_sub(elapsed)
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out.replace('\n', " ")
}
