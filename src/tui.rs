//! 终端界面：历史列表 + 实时状态。两种入口——
//!
//! * [`run`]：只看历史（`dictate history`）。
//! * [`run_live`]：热键守护 + 历史（`dictate run`），录音/转写状态显示在顶栏。

use std::io;
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
        event_loop(term, &mut App::new(history, theme))
    })
}

/// 热键守护：一边听热键，一边显示历史。
pub fn run_live(cfg: Config, history: History) -> Result<()> {
    let theme_cfg = cfg.theme.clone();
    let subtitle = format!("{} · {}", cfg.hotkey, cfg.model);
    with_terminal(|term| {
        let theme = Theme::resolve(theme_cfg.as_deref());
        let mut app = App::new(history, theme);
        app.subtitle = subtitle;
        attach_loop(term, &mut app)
    })
}

/// 守护进程写的状态文件。终端视图只读它，不碰麦克风与热键。
#[derive(serde::Deserialize, Default)]
struct StatusFile {
    #[serde(default)]
    phase: String,
    #[serde(default)]
    since_ms: u64,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

fn init_term(term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    enable_raw_mode()?;
    execute!(term.backend_mut(), EnterAlternateScreen)?;
    term.clear()?;
    Ok(())
}

fn restore_term(term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

fn with_terminal<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()>,
{
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    init_term(&mut term)?;
    let res = f(&mut term);
    restore_term(&mut term)?;
    res
}

/// 把守护进程的状态文件映射成本地相位（用时间差还原已过去多久）。
fn apply_status(app: &mut App) {
    let path = crate::config::status_path();
    let Ok(body) = std::fs::read_to_string(&path) else {
        app.phase = Phase::Idle;
        app.status = "守护进程没在跑 —— 先执行 dictate".into();
        return;
    };
    let Ok(st) = serde_json::from_str::<StatusFile>(&body) else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let elapsed = Duration::from_millis(now.saturating_sub(st.since_ms));
    let at = Instant::now().checked_sub(elapsed).unwrap_or_else(Instant::now);
    app.phase = match st.phase.as_str() {
        "recording" => Phase::Recording(at),
        "transcribing" => Phase::Transcribing(at),
        _ => Phase::Idle,
    };
    app.status = match (&st.error, &st.text) {
        (Some(e), _) => format!("失败：{e}"),
        (None, Some(t)) => format!("完成：{}", t.chars().take(40).collect::<String>()),
        (None, None) => String::new(),
    };
}

/// 附着模式：只画，不抢热键/麦克风。
fn attach_loop(term: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    let mut last_len = usize::MAX;
    loop {
        apply_status(app);
        let len = std::fs::metadata(crate::config::history_path())
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        if len != last_len {
            last_len = len;
            app.reload();
        }
        term.draw(|f| draw(f, app))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match handle_key(app, &k)? {
            Outcome::Continue => {}
            Outcome::Quit => return Ok(()),
            Outcome::Settings => {
                restore_term(term)?;
                crate::settings_ui::run()?;
                init_term(term)?;
            }
        }
    }
}

/// 只看历史（`dictate history`），不读状态文件。
fn event_loop(term: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        term.draw(|f| draw(f, app))?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match handle_key(app, &k)? {
            Outcome::Continue => {}
            Outcome::Quit => return Ok(()),
            Outcome::Settings => {
                restore_term(term)?;
                crate::settings_ui::run()?;
                init_term(term)?;
            }
        }
    }
}

enum Outcome {
    Continue,
    Quit,
    Settings,
}

/// 给守护进程递一条命令（它每 100 ms 读一次）。
fn send_control(cmd: &str) {
    let path = crate::config::control_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let _ = std::fs::write(&path, cmd);
}

/// 处理一个按键。
fn handle_key(app: &mut App, k: &crossterm::event::KeyEvent) -> Result<Outcome> {
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
        return Ok(Outcome::Continue);
    }
    // 带修饰键的按键一律不当作动作：Ctrl+D 是终端 EOF、Ctrl+C 是中断
    let plain = k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT;
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        match k.code {
            KeyCode::Char('c') | KeyCode::Char('d') => return Ok(Outcome::Quit),
            _ => return Ok(Outcome::Continue),
        }
    }
    match k.code {
        KeyCode::Esc => return Ok(Outcome::Quit),
        KeyCode::Up => app.selected = app.selected.saturating_sub(1),
        KeyCode::Down => {
            if app.selected + 1 < app.shown.len() {
                app.selected += 1;
            }
        }
        KeyCode::Char('q') if plain => return Ok(Outcome::Quit),
        KeyCode::Char('s') if plain => return Ok(Outcome::Settings),
        KeyCode::Char(' ') => {
            // 手动录音：不依赖热键（托盘没起来时也能用）
            let recording = matches!(app.phase, Phase::Recording(_));
            send_control(if recording { "stop" } else { "start" });
            app.status = if recording {
                "已提交（手动停止）".into()
            } else {
                "手动开始录音…".into()
            };
        }
        KeyCode::Char('x') if plain => {
            send_control("quit");
            app.status = "已通知守护进程退出".into();
        }
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
    Ok(Outcome::Continue)
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
            "空格 手动录音   s 设置   ↑↓ 选择   ⏎ 复制   / 搜索   d 删除   c 清空   q 退出",
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
    fn osc52_encodes_text() {
        let s = osc52("你好");
        assert!(s.starts_with("\x1b]52;c;"));
        assert!(s.ends_with('\x07'));
        assert!(s.contains("5L2g5aW9"));
    }

    #[test]
    fn status_file_maps_to_phases() {
        // 直接构造状态文件，验证相位映射（attach 模式的核心）
        let dir = std::env::temp_dir().join(format!("dictate-status-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("dictate")).unwrap();
        std::env::set_var("XDG_DATA_HOME", &dir);
        let path = dir.join("dictate/status.json");

        std::fs::write(&path, r#"{"phase":"recording","since_ms":0}"#).unwrap();
        let mut app = App::new(History::new(dir.join("history.jsonl")), Theme::dark());
        apply_status(&mut app);
        assert!(matches!(app.phase, Phase::Recording(_)));

        std::fs::write(&path, r#"{"phase":"idle","since_ms":0,"text":"你好"}"#).unwrap();
        apply_status(&mut app);
        assert_eq!(app.phase, Phase::Idle);
        assert!(app.status.contains("你好"));

        std::fs::write(&path, r#"{"phase":"idle","since_ms":0,"error":"HTTP 401"}"#).unwrap();
        apply_status(&mut app);
        assert!(app.status.contains("401"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
