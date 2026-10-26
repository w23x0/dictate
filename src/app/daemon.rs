//! 无终端常驻：热键 → 采集 → 转写 → 历史，状态给托盘用。
//!
//! 支持真流式（WebSocket 系模型 + `streaming = true`）：边说边出字，
//! 中间文本实时写进 `status.json` 给终端视图。流式会话失败（断网、超时）
//! 自动回退到「录完整段再批量转写」——采集线程本来就留了整段样本，一个字不丢。

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::audio::Capture;
use crate::core::config::Config;
use crate::core::history::{new_id, now_ms, now_rfc3339, Entry, History};
use crate::hotkey::{self, HotkeyEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Recording(Instant),
    Transcribing(Instant),
}

/// 转写线程 → 主循环的通道。历史落库统一在主循环做（只有它拿得到录音时长）。
type Worker = (Sender<WorkerOut>, Receiver<WorkerOut>);

struct DoneText {
    text: String,
    latency_ms: u64,
}

enum WorkerOut {
    /// 流式中间结果（已定稿句子 + 当前半句）
    Partial(String),
    /// `Ok` = 拿到完整文本；`Err` = 流式或批量失败的原因
    Done(Result<DoneText, String>),
}

/// 一次录音在等谁的结果。
enum Awaiting {
    None,
    /// 流式会话进行中；stop 之后完整 PCM 留在这里备回退
    Stream {
        pcm: Option<(u32, Vec<f32>, f32)>,
    },
    /// 批量转写中（含流式失败后的回退）
    Batch {
        secs: f32,
    },
}

pub struct Daemon {
    cfg: Config,
    published: String,
    capture: Capture,
    hk: Receiver<HotkeyEvent>,
    hk_spec: hotkey::SharedSpec,
    /// 上次看到的配置文件 mtime；变了就热加载（模型/key/交付方式/热键都不用重启）。
    cfg_mtime: Option<std::time::SystemTime>,
    worker: Worker,
    awaiting: Awaiting,
    /// 流式会话在录音阶段就挂了：松开后直接走批量
    stream_dead: bool,
    /// 流式中间文本（给托盘提示与 status.json）
    partial: Option<String>,
    pub phase: Phase,
    /// 本次录音的开始时刻（判断"误触"用）
    started: Option<Instant>,
    pub quit: bool,
    pub last_error: Option<String>,
    /// 非失败的信息（按键太短、流式回退批量等）：显示为暗色提示，不红、不带"下一步"建议
    pub last_note: Option<String>,
    /// 最近一次交付方式（托盘提示用）
    pub paste_note: Option<String>,
    pub last_text: Option<String>,
    /// 上次状态文件实际落盘时间（心跳节流用）
    last_publish: Instant,
    /// 上次落盘的状态内容（去掉心跳字段），变了才写
    published_key: String,
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
            cfg_mtime,
            worker: channel(),
            awaiting: Awaiting::None,
            stream_dead: false,
            partial: None,
            phase: Phase::Idle,
            started: None,
            quit: false,
            last_error: None,
            last_note: None,
            paste_note: None,
            last_text: None,
            last_publish: Instant::now(),
            published_key: String::new(),
        })
    }

    /// 把状态写进 `status.json`，供 `dictate run` 的终端视图读取。
    /// `hb_ms` 心跳随内容变化或每 3 秒刷新一次——终端视图靠它发现守护进程崩溃后的冻结状态。
    fn publish(&mut self) {
        let (phase, since) = match self.phase {
            Phase::Idle => ("idle", 0),
            Phase::Recording(t) => ("recording", now_ms_of(t)),
            Phase::Transcribing(t) => ("transcribing", now_ms_of(t)),
        };
        let key = format!(
            "{phase}|{since}|{:?}|{:?}|{:?}|{:?}",
            self.last_text, self.last_error, self.last_note, self.partial
        );
        let fresh = self.last_publish.elapsed() < Duration::from_secs(3);
        if key == self.published_key && fresh {
            return;
        }
        let v = serde_json::json!({
            "phase": phase,
            "since_ms": since,
            "hb_ms": now_ms(),
            "text": self.last_text,
            "error": self.last_error,
            "note": self.last_note,
            "partial": if matches!(self.phase, Phase::Recording(_)) {
                self.partial.clone()
            } else {
                None
            },
        });
        let body = v.to_string();
        let path = crate::core::config::status_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
            self.published = body;
            self.published_key = key;
            self.last_publish = Instant::now();
        }
    }

    /// 流式可用：开关开着 + 模型是 WebSocket 系 + key 拿得到。
    fn streaming_ready(&self) -> bool {
        self.cfg.streaming
            && crate::asr::pick_style(&self.cfg.model, self.cfg.api_style.as_deref())
                .map(|s| s == crate::asr::Style::Ws)
                .unwrap_or(false)
            && crate::asr::QwenClient::from_config(&self.cfg).is_ok()
    }

    /// 手动开始录音（来自终端视图或托盘）。
    pub fn start(&mut self) {
        if self.phase != Phase::Idle {
            return;
        }
        let max = self.cfg.max_seconds;
        let want_stream = self.streaming_ready();
        let mut streamed = false;
        // 流式起不来（没设备/key 问题的边缘情况）就当场退回批量开录
        let r = if want_stream {
            match self.begin_stream(max) {
                Ok(()) => {
                    streamed = true;
                    Ok(())
                }
                Err(e) => {
                    eprintln!("[daemon] 流式起不来，退回批量：{e:#}");
                    self.capture.start(max)
                }
            }
        } else {
            self.capture.start(max)
        };
        match r {
            Ok(()) => {
                self.started = Some(Instant::now());
                self.phase = Phase::Recording(Instant::now());
                self.last_error = None;
                self.last_note = None;
                self.partial = None;
                self.stream_dead = false;
                self.awaiting = if streamed {
                    Awaiting::Stream { pcm: None }
                } else {
                    Awaiting::None
                };
            }
            Err(e) => self.last_error = Some(format!("启动录音失败：{e}")),
        }
    }

    /// 起一路真流式：采集转发 → WS 会话线程 → 事件回主循环。
    fn begin_stream(&mut self, max: f32) -> anyhow::Result<()> {
        let client = crate::asr::QwenClient::from_config(&self.cfg)?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.capture.start_stream(max, tx)?;
        let worker_tx = self.worker.0.clone();
        std::thread::spawn(move || {
            let (ev_tx, ev_rx) = channel::<crate::asr::ws::StreamEvent>();
            let inner = ev_tx.clone();
            let session = std::thread::spawn(move || {
                crate::asr::ws::stream_session(client, rx, inner);
            });
            drop(ev_tx);
            let started = Instant::now();
            let mut ok: Option<String> = None;
            let mut err: Option<String> = None;
            // 会话线程（持有唯一剩下的 sender）结束后 iter() 自然终止
            for ev in ev_rx.iter() {
                match ev {
                    crate::asr::ws::StreamEvent::Partial(p) => {
                        let _ = worker_tx.send(WorkerOut::Partial(p));
                    }
                    crate::asr::ws::StreamEvent::Final(Ok(t)) => ok = Some(t),
                    crate::asr::ws::StreamEvent::Final(Err(e)) => err = Some(e),
                }
            }
            let _ = session.join();
            let out = match (ok, err) {
                (Some(text), _) => Ok(DoneText {
                    text,
                    latency_ms: started.elapsed().as_millis() as u64,
                }),
                (None, Some(e)) => Err(e),
                (None, None) => Err("流式会话没有产出结果".into()),
            };
            let _ = worker_tx.send(WorkerOut::Done(out));
        });
        Ok(())
    }

    /// 重转上一次送服务端的音频（`keep_audio` 留的 last.wav）。
    fn retry(&mut self) {
        if !matches!(self.phase, Phase::Idle) {
            self.last_note = Some("正在录音/转写，等它结束再重试".into());
            return;
        }
        let path = crate::core::config::status_path()
            .parent()
            .map(|d| d.join("last.wav"))
            .unwrap_or_else(|| std::env::temp_dir().join("last.wav"));
        if !path.exists() {
            self.last_error = Some("没有保留的音频——keep_audio = true 后的录音才能重转".into());
            return;
        }
        match crate::audio::read_wav(&path) {
            Ok((rate, pcm)) if !pcm.is_empty() => {
                let secs = pcm.len() as f32 / rate as f32;
                self.phase = Phase::Transcribing(Instant::now());
                self.last_error = None;
                self.spawn_batch(rate, pcm, secs);
            }
            Ok(_) => self.last_error = Some("last.wav 是空的".into()),
            Err(e) => self.last_error = Some(format!("读 last.wav 失败：{e}")),
        }
    }

    /// 取消本次录音（不转写）。
    pub fn cancel(&mut self) {
        if matches!(self.phase, Phase::Recording(_)) {
            let _ = self.capture.stop();
            self.phase = Phase::Idle;
            self.started = None;
            self.last_error = None;
            self.partial = None;
            // 迟到的 Done 会在 tick 里因 Awaiting::None 被丢弃
            self.awaiting = Awaiting::None;
        }
    }

    /// 手动提交（停止并转写）。
    pub fn stop(&mut self) {
        if !matches!(self.phase, Phase::Recording(_)) {
            return;
        }
        // 误触：按一下就松（< 200 ms）直接丢弃，免得每次点错都产生一条空记录。
        // 这是正常防抖，不是转写失败——走 note 通道，别在界面渲染成红字失败
        let elapsed = self.started.map(|t| t.elapsed()).unwrap_or_default();
        if elapsed < Duration::from_millis(200) {
            self.cancel();
            self.last_error = None;
            self.last_note = Some("按键太短，已忽略".into());
            return;
        }
        self.started = None;
        match self.capture.stop() {
            Ok((rate, pcm)) => {
                let secs = pcm.len() as f32 / rate as f32;
                self.phase = Phase::Transcribing(Instant::now());
                let prev = std::mem::replace(&mut self.awaiting, Awaiting::None);
                match prev {
                    // 会话早就挂了：拿着整段样本直接批量
                    Awaiting::Stream { .. } if self.stream_dead => {
                        self.spawn_batch(rate, pcm, secs)
                    }
                    Awaiting::Stream { .. } => {
                        self.awaiting = Awaiting::Stream {
                            pcm: Some((rate, pcm, secs)),
                        };
                    }
                    _ => self.spawn_batch(rate, pcm, secs),
                }
            }
            Err(e) => {
                self.phase = Phase::Idle;
                self.last_error = Some(format!("取录音失败：{e}"));
            }
        }
    }

    /// 批量转写（也承担流式失败后的回退）。
    fn spawn_batch(&mut self, rate: u32, pcm: Vec<f32>, secs: f32) {
        self.awaiting = Awaiting::Batch { secs };
        let cfg = self.cfg.clone();
        let tx = self.worker.0.clone();
        std::thread::spawn(move || {
            let out = crate::pipeline::prepare_wav(&cfg, &pcm, rate)
                .and_then(|wav| {
                    let t = crate::asr::QwenClient::from_config(&cfg)?.transcribe_wav(&wav)?;
                    Ok(DoneText {
                        text: t.text,
                        latency_ms: t.latency_ms,
                    })
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(WorkerOut::Done(out));
        });
    }

    fn finish_ok(&mut self, dt: DoneText, secs: f32) {
        self.awaiting = Awaiting::None;
        self.started = None;
        let hist = History::new(crate::core::config::history_path());
        let mut append_err = None;
        if let Err(e) = hist.append_with_cap(
            &Entry {
                id: new_id(),
                ts: now_rfc3339(),
                ts_ms: now_ms(),
                duration_ms: (secs * 1000.0) as u64,
                latency_ms: dt.latency_ms,
                model: self.cfg.model.clone(),
                status: "ok".into(),
                text: dt.text.clone(),
            },
            self.cfg.keep_entries,
        ) {
            append_err = Some(format!("历史写入失败：{e:#}"));
        }
        // 识别完直接送到当前焦点窗口（剪贴板 + 模拟粘贴）
        let mode = crate::app::inject::Mode::parse(&self.cfg.paste);
        match crate::app::inject::deliver(&dt.text, mode) {
            Ok(how) => {
                self.last_text = Some(dt.text);
                self.paste_note = Some(how);
                self.last_error = append_err;
            }
            Err(e) => {
                self.last_text = Some(dt.text);
                self.paste_note = None;
                let msg = format!("交付失败：{e}");
                self.last_error = Some(match append_err {
                    Some(ae) => format!("{msg}（{ae}）"),
                    None => msg,
                });
            }
        }
        self.phase = Phase::Idle;
        self.partial = None;
    }

    fn finish_err(&mut self, e: String, secs: f32) {
        // 失败也写一条错误历史（沿用原行为），返回值仍然是错误
        let hist = History::new(crate::core::config::history_path());
        if let Err(ae) = hist.append_with_cap(
            &Entry {
                id: new_id(),
                ts: now_rfc3339(),
                ts_ms: now_ms(),
                duration_ms: (secs * 1000.0) as u64,
                latency_ms: 0,
                model: self.cfg.model.clone(),
                status: format!("error: {e}"),
                text: String::new(),
            },
            self.cfg.keep_entries,
        ) {
            eprintln!("[history] 失败条目没写进历史：{ae:#}");
        }
        self.awaiting = Awaiting::None;
        self.started = None;
        self.last_error = Some(e);
        self.phase = Phase::Idle;
        self.partial = None;
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
            "retry" => self.retry(),
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

        // 录音到 max_seconds 时采集线程会自己停（整段样本落进结果通道，流式会话也随之收尾）。
        // 守护进程在这里主动收尾，不等用户松手——否则相位虚挂在「录音中」计时虚涨，
        // 样本滞留结果通道，下一次录音 stop() 按 FIFO 会先取到这段旧音频
        if let Phase::Recording(t) = self.phase {
            if t.elapsed() >= Duration::from_secs_f32(self.cfg.max_seconds + 0.5) {
                self.stop();
            }
        }

        // 流式卡死兜底：超时还等不到 Final，就带着整段样本回退批量。
        // 这是「恢复中」不是失败——走 note 通道，别在界面渲染成红字
        if let Phase::Transcribing(t) = self.phase {
            let budget = Duration::from_secs(self.cfg.timeout_secs.max(30) + 10);
            if t.elapsed() > budget {
                if let Awaiting::Stream {
                    pcm: Some((rate, pcm, secs)),
                } = std::mem::replace(&mut self.awaiting, Awaiting::None)
                {
                    self.last_note = Some("流式超时，回退批量转写".into());
                    self.spawn_batch(rate, pcm, secs);
                }
            }
        }

        while let Ok(out) = self.worker.1.try_recv() {
            match out {
                WorkerOut::Partial(p) => self.partial = Some(p),
                WorkerOut::Done(res) => {
                    let prev = std::mem::replace(&mut self.awaiting, Awaiting::None);
                    match prev {
                        // 已取消/过期，丢弃
                        Awaiting::None => {}
                        // 还在录音阶段就出 Final：只可能是采集侧先结束了——
                        // max_seconds 自停（样本已先落进结果通道）或采集流挂掉
                        Awaiting::Stream { pcm: None } => match res {
                            Ok(dt) => {
                                // 立即取走整段样本按真实时长落库交付；取不到就用已录时长近似。
                                // 绝不能按 0 秒落库（污染按天统计），也不能让样本滞留到下一次录音
                                let secs = match self.capture.try_take() {
                                    Some((rate, pcm)) if !pcm.is_empty() => {
                                        pcm.len() as f32 / rate as f32
                                    }
                                    _ => self
                                        .started
                                        .map(|t| t.elapsed().as_secs_f32())
                                        .unwrap_or(0.0),
                                };
                                self.finish_ok(dt, secs);
                            }
                            // 会话挂了：标记后等松开/自动收尾，拿整段样本走批量
                            Err(_) => self.stream_dead = true,
                        },
                        Awaiting::Stream {
                            pcm: Some((rate, pcm, secs)),
                        } => match res {
                            Ok(dt) => self.finish_ok(dt, secs),
                            // 流式失败：整段样本还在，批量来一遍
                            Err(_) => self.spawn_batch(rate, pcm, secs),
                        },
                        Awaiting::Batch { secs } => match res {
                            Ok(dt) => self.finish_ok(dt, secs),
                            Err(e) => self.finish_err(e, secs),
                        },
                    }
                }
            }
        }

        self.publish();
        match self.phase {
            Phase::Idle => match (&self.last_error, &self.last_note, &self.last_text) {
                (Some(e), _, _) => format!("dictate — 失败：{}", truncate(e, 60)),
                (None, Some(n), _) => format!("dictate — {}", truncate(n, 60)),
                (None, None, Some(t)) => format!(
                    "dictate — {}（{}）",
                    truncate(t, 50),
                    self.paste_note.as_deref().unwrap_or("已识别")
                ),
                (None, None, None) => "dictate — 就绪".to_string(),
            },
            Phase::Recording(t) => match self.partial.as_deref() {
                Some(p) if !p.is_empty() => format!(
                    "dictate — ● {:.1}s {}",
                    t.elapsed().as_secs_f32(),
                    truncate(p, 24)
                ),
                _ => format!("dictate — ● 录音中 {:.1}s", t.elapsed().as_secs_f32()),
            },
            Phase::Transcribing(t) => {
                format!("dictate — ◐ 转写中 {:.1}s", t.elapsed().as_secs_f32())
            }
        }
    }
}

fn now_ms_of(t: Instant) -> u64 {
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
