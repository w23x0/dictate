//! 终端界面：历史列表 + 实时状态。两种入口——
//!
//! * [`run`]：只看历史（`dictate history`）。
//! * [`run_live`]：热键守护 + 历史（`dictate run`），录音/转写状态显示在顶栏。

use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

use crate::config::Config;
use crate::history::{Entry, History};
use crate::theme::Theme;
use crate::hotkey::{self, HotkeyEvent};
use crate::pipeline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Recording(Instant),
    Transcribing(Instant),
}

struct App {
    history: History,
    all: Vec<Entry>,
    shown: Vec<usize>,
    query: String,
    searching: bool,
    selected: usize,
    status: String,
    phase: Phase,
    /// 顶栏副标题（热键 · 模型），只在 run 模式有
    subtitle: String,
    theme: Theme,
}

impl App {
    fn new(history: History, theme: Theme) -> Self {
        let mut app = Self {
            history,
            all: Vec::new(),
            shown: Vec::new(),
            query: String::new(),
            searching: false,
            selected: 0,
            status: String::new(),
            phase: Phase::Idle,
            subtitle: String::new(),
            theme,
        };
        app.reload();
        app
    }

    fn reload(&mut self) {
        self.all = self.history.load();
        self.refilter();
    }

    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.shown = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, e)| q.is_empty() || e.text.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.shown.len() {
            self.selected = self.shown.len().saturating_sub(1);
        }
    }

    fn current(&self) -> Option<&Entry> {
        self.shown.get(self.selected).map(|i| &self.all[*i])
    }
}

/// OSC 52：把文本放进终端所在机器的剪贴板（SSH 里也有效）。
pub fn osc52(text: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x07")
}

pub fn run(history: History) -> Result<()> {
    // 配色要问终端背景色，必须在 raw mode 之后问，否则回包会被回显
    with_terminal(|term| {
        let theme = Theme::resolve(crate::config::load().ok().and_then(|c| c.theme).as_deref());
        event_loop(term, &mut App::new(history, theme), None)
    })
}

/// 热键守护：一边听热键，一边显示历史。
pub fn run_live(cfg: Config, history: History) -> Result<()> {
    let spec = hotkey::Spec::parse(&cfg.hotkey)?;
    let (tx, hk_rx) = channel();
    hotkey::spawn(spec, tx, false)?;
    let capture = crate::audio::spawn_capture()?;

    let theme_cfg = cfg.theme.clone();
    let mut app = App::new(history, Theme::dark());
    app.theme = Theme::resolve(theme_cfg.as_deref());
    app.subtitle = format!("{} · {}", cfg.hotkey, cfg.model);
    app.status = "等待热键…".to_string();
    let mut live = Live { cfg, capture, hk: hk_rx, worker: channel(), busy: false };

    with_terminal(|term| event_loop(term, &mut app, Some(&mut live)))
}

struct Live {
    cfg: Config,
    capture: crate::audio::Capture,
    hk: Receiver<HotkeyEvent>,
    worker: (Sender<Result<String, String>>, Receiver<Result<String, String>>),
    busy: bool,
}

fn with_terminal<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()>,
{
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;
    let res = f(&mut term);
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

fn event_loop(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    mut live: Option<&mut Live>,
) -> Result<()> {
    loop {
        if let Some(live) = live.as_deref_mut() {
            pump_live(app, live)?;
        }
        term.draw(|f| draw(f, app))?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        if app.searching {
            match k.code {
                KeyCode::Esc => {
                    app.searching = false;
                    app.query.clear();
                    app.refilter();
                }
                KeyCode::Enter => app.searching = false,
                KeyCode::Backspace => {
                    app.query.pop();
                    app.refilter();
                }
                KeyCode::Char(c) => {
                    app.query.push(c);
                    app.refilter();
                }
                _ => {}
            }
            continue;
        }
        // 带修饰键的按键一律不当作动作：Ctrl+D 是终端 EOF、Ctrl+C 是中断，
        // 把裸字母当动作时**必须**确认没有修饰键，否则会误删。
        let plain = k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT;
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            match k.code {
                KeyCode::Char('c') | KeyCode::Char('d') => return Ok(()),
                _ => continue,
            }
        }
        match k.code {
            KeyCode::Esc => return Ok(()),
            KeyCode::Up => app.selected = app.selected.saturating_sub(1),
            KeyCode::Down => {
                if app.selected + 1 < app.shown.len() {
                    app.selected += 1;
                }
            }
            KeyCode::Char('q') if plain => return Ok(()),
            KeyCode::Char('k') if plain => app.selected = app.selected.saturating_sub(1),
            KeyCode::Char('j') if plain => {
                if app.selected + 1 < app.shown.len() {
                    app.selected += 1;
                }
            }
            KeyCode::Char('/') if plain => {
                app.searching = true;
                app.query.clear();
                app.refilter();
            }
            KeyCode::Enter => {
                if let Some(e) = app.current() {
                    let text = e.text.clone();
                    print!("{}", osc52(&text));
                    app.status = format!("已复制 {} 字", text.chars().count());
                }
            }
            KeyCode::Char('d') if plain => {
                if let Some(e) = app.current() {
                    let id = e.id.clone();
                    app.history.delete(&id)?;
                    app.reload();
                    app.status = "已删除一条".into();
                }
            }
            KeyCode::Char('c') if plain => {
                app.history.clear()?;
                app.reload();
                app.status = "已清空".into();
            }
            KeyCode::Char('r') if plain => {
                app.reload();
                app.status = "已刷新".into();
            }
            _ => {}
        }
    }
}

/// 热键 → 动作。纯函数，便于把状态机钉在测试里。
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Start,
    Stop,
    Ignore,
}

fn on_hotkey(ev: HotkeyEvent, phase: Phase) -> Action {
    match (ev, phase) {
        (HotkeyEvent::Press, Phase::Idle) => Action::Start,
        (HotkeyEvent::Release, Phase::Recording(_)) => Action::Stop,
        _ => Action::Ignore,
    }
}

/// 处理热键与转写结果，更新状态机。
fn pump_live(app: &mut App, live: &mut Live) -> Result<()> {
    while let Ok(ev) = live.hk.try_recv() {
        match on_hotkey(ev, app.phase) {
            Action::Start => {
                if let Err(e) = live.capture.start(live.cfg.max_seconds) {
                    app.status = format!("启动录音失败：{e}");
                    continue;
                }
                app.phase = Phase::Recording(Instant::now());
                app.status = "录音中…（松开结束）".into();
            }
            Action::Stop => {
                let (rate, pcm) = live.capture.stop()?;
                let secs = pcm.len() as f32 / rate as f32;
                app.phase = Phase::Transcribing(Instant::now());
                app.status = format!("转写中…（{secs:.1} 秒音频）");
                let cfg = live.cfg.clone();
                let hist = History::new(crate::config::history_path());
                let tx = live.worker.0.clone();
                live.busy = true;
                std::thread::spawn(move || {
                    let out = pipeline::prepare_wav(&cfg, &pcm, rate)
                        .and_then(|wav| pipeline::transcribe_and_record(&cfg, &hist, &wav, secs))
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(out);
                });
            }
            Action::Ignore => {}
        }
    }

    while let Ok(out) = live.worker.1.try_recv() {
        live.busy = false;
        app.phase = Phase::Idle;
        match out {
            Ok(text) => {
                app.status = format!("完成：{}", text.chars().take(40).collect::<String>());
                app.reload();
            }
            Err(e) => app.status = format!("失败：{e}"),
        }
    }
    Ok(())
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let th = app.theme;
    use ratatui::layout::Rect;

    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // 品牌
            Constraint::Length(1), // 空行（grok 也留白）
            Constraint::Min(3),    // 历史（无边框）
            Constraint::Length(2), // 输入面板（填充块）
        ])
        .split(area);

    // ── 顶栏
    let left = Line::from(vec![
        Span::styled("◆ ", Style::default().fg(th.accent)),
        Span::styled("dictate", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("  v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(th.faint),
        ),
        Span::styled(
            if app.subtitle.is_empty() {
                String::new()
            } else {
                format!("  {}", app.subtitle)
            },
            Style::default().fg(th.dim),
        ),
    ]);
    let (dot, dot_color, phase_text) = match app.phase {
        Phase::Idle => ("●", th.ok, "就绪"),
        Phase::Recording(_) => ("●", th.rec, "录音中"),
        Phase::Transcribing(_) => ("●", th.warn, "转写中"),
    };
    let right = Line::from(vec![
        Span::styled(dot, Style::default().fg(dot_color)),
        Span::styled(format!(" {phase_text}   {} 条 ", app.shown.len()), Style::default().fg(th.dim)),
    ])
    .alignment(ratatui::layout::Alignment::Right);
    let wide = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[0]);
    f.render_widget(Paragraph::new(left), wide[0]);
    f.render_widget(Paragraph::new(right), wide[1]);

    // ── 历史：平铺列表，没有边框
    let list_area = chunks[2];
    if app.shown.is_empty() {
        let msg = if app.query.is_empty() {
            "还没有记录 —— 按住热键说一句，松开就会出现在这里"
        } else {
            "没有匹配的记录"
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(th.dim))))
                .alignment(ratatui::layout::Alignment::Center),
            list_area,
        );
    } else {
        let items: Vec<ListItem> = app
            .shown
            .iter()
            .map(|i| {
                let e = &app.all[*i];
                let time = e.ts.get(11..16).unwrap_or("--:--");
                let (body, color) = if e.is_ok() {
                    (e.text.replace('\n', " "), Color::Reset)
                } else {
                    (e.status.trim_start_matches("error: ").replace('\n', " "), th.rec)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {time}  "), Style::default().fg(th.faint)),
                    Span::styled("❯ ", Style::default().fg(th.accent)),
                    Span::styled(body, Style::default().fg(color)),
                    Span::styled(
                        format!("  {:.1}s", e.duration_ms as f32 / 1000.0),
                        Style::default().fg(th.faint),
                    ),
                ]))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(Style::default().bg(th.selected_bg))
            .highlight_symbol("");
        let mut state = ListState::default();
        state.select(Some(app.selected));
        f.render_stateful_widget(list, list_area, &mut state);
    }

    // ── 底部面板：填充块（grok-cli 的输入框位置），不描边
    let panel = chunks[3];
    f.render_widget(
        Block::default().style(Style::default().bg(th.panel_bg)),
        panel,
    );
    let inner = Rect {
        x: panel.x + 2,
        y: panel.y,
        width: panel.width.saturating_sub(4),
        height: 1,
    };

    let status = if app.searching {
        Line::from(vec![
            Span::styled("/ ", Style::default().fg(th.accent)),
            Span::styled(app.query.clone(), Style::default().fg(Color::Reset)),
            Span::styled("▏", Style::default().fg(th.accent)),
        ])
    } else {
        match app.phase {
            Phase::Idle => Line::from(vec![
                Span::styled("❯ ", Style::default().fg(th.accent)),
                Span::styled(
                    if app.status.is_empty() {
                        if app.subtitle.is_empty() {
                            "只读模式 · q 退出".to_string()
                        } else {
                            "按住热键说话".to_string()
                        }
                    } else {
                        app.status.clone()
                    },
                    Style::default().fg(th.dim),
                ),
            ]),
            Phase::Recording(t) => Line::from(vec![
                Span::styled("● ", Style::default().fg(th.rec)),
                Span::styled(
                    format!("录音中 {:.1}s", t.elapsed().as_secs_f32()),
                    Style::default().fg(th.rec).add_modifier(Modifier::BOLD),
                ),
                Span::styled("   松开提交", Style::default().fg(th.dim)),
            ]),
            Phase::Transcribing(t) => {
                const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                let frame = FRAMES[(t.elapsed().as_millis() / 80) as usize % FRAMES.len()];
                Line::from(vec![
                    Span::styled(format!("{frame} "), Style::default().fg(th.warn)),
                    Span::styled("转写中…", Style::default().fg(th.warn)),
                ])
            }
        }
    };
    f.render_widget(Paragraph::new(status).style(Style::default().bg(th.panel_bg)), inner);

    let hints = Rect { y: panel.y + 1, ..inner };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ 选择   ⏎ 复制   / 搜索   d 删除   c 清空   q 退出",
            Style::default().fg(th.faint),
        )))
        .alignment(ratatui::layout::Alignment::Right)
        .style(Style::default().bg(th.panel_bg)),
        hints,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_state_machine_ignores_duplicates() {
        assert_eq!(on_hotkey(HotkeyEvent::Press, Phase::Idle), Action::Start);
        // 已经录着时重复按：忽略（而不是重开一段）
        assert_eq!(
            on_hotkey(HotkeyEvent::Press, Phase::Recording(Instant::now())),
            Action::Ignore
        );
        assert_eq!(
            on_hotkey(HotkeyEvent::Release, Phase::Recording(Instant::now())),
            Action::Stop
        );
        // 没在录就松开：忽略
        assert_eq!(on_hotkey(HotkeyEvent::Release, Phase::Idle), Action::Ignore);
        // 转写中再按：忽略，避免两段录音抢同一个转写
        assert_eq!(
            on_hotkey(HotkeyEvent::Press, Phase::Transcribing(Instant::now())),
            Action::Ignore
        );
    }

    #[test]
    fn osc52_encodes_text() {
        let s = osc52("你好");
        assert!(s.starts_with("\x1b]52;c;"));
        assert!(s.ends_with('\x07'));
        assert!(s.contains("5L2g5aW9"));
    }
}
