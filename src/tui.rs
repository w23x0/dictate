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
    selected: usize,
    status: String,
    phase: Phase,
    /// 顶栏副标题（热键 · 模型），只在 run 模式有
    subtitle: String,
    theme: Theme,
    overlay: Option<Overlay>,
    /// 内嵌的设置页（`/settings` 进入，Esc 退出）
    settings: Option<crate::settings_ui::Form>,
    /// 底部命令行（grok-cli 那种：始终可输入，`/` 开头是命令）
    input: String,
    quit_requested: bool,
}

/// 斜杠命令：名字、参数提示、说明
const COMMANDS: &[(&str, &str, &str)] = &[
    ("/settings", "", "打开设置"),
    ("/model", "[名字]", "切换模型（不带参数则列表选）"),
    ("/base", "[url]", "切换服务地址（不带参数则列表选）"),
    ("/key", "[sk-…]", "设置 API key"),
    ("/hotkey", "[组合键]", "设置热键，如 ctrl+space"),
    ("/mode", "hold|toggle", "热键语义：按住说话 / 按一下开关"),
    ("/record", "", "手动开始或停止录音"),
    ("/search", "[文字]", "按内容过滤历史"),
    ("/clear", "", "清空全部历史（需确认）"),
    ("/refresh", "", "重新读历史文件"),
    ("/help", "", "帮助"),
    ("/quit", "", "退出视图（守护进程继续跑）"),
    ("/daemon", "stop", "退出守护进程"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    Help,
    /// 清空历史
    ConfirmClear,
    /// 让守护进程退出
    ConfirmQuitDaemon,
}

impl App {
    fn new(history: History, theme: Theme) -> Self {
        let mut app = Self {
            history,
            all: Vec::new(),
            shown: Vec::new(),
            query: String::new(),
            selected: 0,
            status: String::new(),
            phase: Phase::Idle,
            subtitle: String::new(),
            theme,
            overlay: None,
            settings: None,
            input: String::new(),
            quit_requested: false,
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
        if let Some(form) = app.settings.as_mut() {
            form.poll_test();
        }
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
        }
    }
}

enum Outcome {
    Continue,
    Quit,
}

/// 给守护进程递一条命令（它每 100 ms 读一次）。
fn send_control(cmd: &str) {
    let path = crate::config::control_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let _ = std::fs::write(&path, cmd);
}

/// 处理一个按键。模型和 grok-cli 一样：底部一行输入框，`/` 开头是命令。
fn handle_key(app: &mut App, k: &crossterm::event::KeyEvent) -> Result<Outcome> {
    if let Some(form) = app.settings.as_mut() {
        if crate::settings_ui::handle_form_key(form, k)? {
            app.settings = None;
            app.reload();
        }
        return Ok(Outcome::Continue);
    }

    if let Some(ov) = app.overlay {
        match ov {
            Overlay::Help => app.overlay = None,
            Overlay::ConfirmClear => match k.code {
                KeyCode::Enter => {
                    app.history.clear()?;
                    app.reload();
                    app.status = "已清空".into();
                    app.overlay = None;
                }
                KeyCode::Esc => app.overlay = None,
                _ => {}
            },
            Overlay::ConfirmQuitDaemon => match k.code {
                KeyCode::Enter => {
                    send_control("quit");
                    app.status = "已通知守护进程退出".into();
                    app.overlay = None;
                }
                KeyCode::Esc => app.overlay = None,
                _ => {}
            },
        }
        return Ok(Outcome::Continue);
    }

    match k.code {
        KeyCode::Esc => {
            if app.input.is_empty() {
                return Ok(Outcome::Quit);
            }
            app.input.clear();
            app.query.clear();
            app.refilter();
        }
        KeyCode::Enter => {
            run_input(app)?;
            if app.quit_requested {
                return Ok(Outcome::Quit);
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
            app.query = app.input.clone();
            app.refilter();
        }
        KeyCode::Up => app.selected = app.selected.saturating_sub(1),
        KeyCode::Down => {
            if app.selected + 1 < app.shown.len() {
                app.selected += 1;
            }
        }
        KeyCode::PageUp => app.selected = app.selected.saturating_sub(10),
        KeyCode::PageDown => {
            app.selected = (app.selected + 10).min(app.shown.len().saturating_sub(1));
        }
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = app.shown.len().saturating_sub(1),
        KeyCode::Delete => {
            if let Some(e) = app.current() {
                let id = e.id.clone();
                app.history.delete(&id)?;
                app.reload();
                app.status = "已删除一条".into();
            }
        }
        KeyCode::Tab => {
            // 补全：输入 `/` 后按 Tab 补第一个匹配的命令
            if let Some((name, _, _)) = COMMANDS.iter().find(|(c, _, _)| c.starts_with(app.input.as_str()) && !app.input.is_empty()) {
                app.input = (*name).to_string();
            }
        }
        KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.push(c);
            app.query = app.input.clone();
            app.refilter();
        }
        _ => {}
    }
    Ok(Outcome::Continue)
}

/// 回车：`/` 开头当命令，否则当作过滤词（输入时已经实时过滤）。
fn run_input(app: &mut App) -> Result<()> {
    let line = app.input.trim().to_string();
    if line.is_empty() {
        // 空输入按回车 = 复制选中记录（grok 里是"发送"，这里没有消息可发）
        if let Some(e) = app.current() {
            let text = e.text.clone();
            print!("{}", osc52(&text));
            app.status = format!("已复制 {} 字", text.chars().count());
        }
        return Ok(());
    }
    if !line.starts_with('/') {
        app.query = line.clone();
        app.refilter();
        app.status = format!("筛选：{line}（Esc 清除）");
        app.input.clear();
        return Ok(());
    }

    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim().to_string();
    match cmd {
        "/help" => app.overlay = Some(Overlay::Help),
        "/settings" => {
            let cfg = crate::config::load()?;
            let mut form = crate::settings_ui::Form::new(cfg);
            form.theme = app.theme;
            app.settings = Some(form);
        }
        "/model" => {
            if let Some(form) = app.settings.as_mut() {
                let _ = form;
            }
            if arg.is_empty() {
                let cfg = crate::config::load()?;
                let mut form = crate::settings_ui::Form::new(cfg);
                form.theme = app.theme;
                form.open_model_picker();
                app.settings = Some(form);
            } else {
                let mut cfg = crate::config::load()?;
                if crate::asr::pick_style(&arg, None).is_err() {
                    app.status = format!("{arg} 不支持（异步 filetrans 没做）");
                } else {
                    cfg.model = arg.clone();
                    cfg.api_style = None;
                    crate::config::write(&crate::config::config_path(), &cfg)?;
                    app.status = format!("模型 → {arg}（下次录音生效）");
                }
            }
        }
        "/base" => {
            if arg.is_empty() {
                let cfg = crate::config::load()?;
                let mut form = crate::settings_ui::Form::new(cfg);
                form.theme = app.theme;
                form.open_base_picker();
                app.settings = Some(form);
            } else {
                let mut cfg = crate::config::load()?;
                match crate::asr::normalize_base(&arg) {
                    Ok(b) => {
                        cfg.base_url = b;
                        crate::config::write(&crate::config::config_path(), &cfg)?;
                        app.status = format!("服务地址 → {}", cfg.base_url);
                    }
                    Err(e) => app.status = format!("地址不合法：{e}"),
                }
            }
        }
        "/key" => {
            if arg.is_empty() {
                app.status = "用法：/key sk-…".into();
            } else {
                let mut cfg = crate::config::load()?;
                cfg.api_key = Some(arg);
                crate::config::write(&crate::config::config_path(), &cfg)?;
                app.status = "已保存 key（存在配置文件里，权限 600）".into();
            }
        }
        "/hotkey" => {
            match crate::hotkey::Spec::parse(&arg) {
                Ok(_) => {
                    let mut cfg = crate::config::load()?;
                    cfg.hotkey = arg.clone();
                    crate::config::write(&crate::config::config_path(), &cfg)?;
                    app.status = format!("热键 → {arg}（重启 dictate 生效）");
                }
                Err(e) => app.status = format!("热键不合法：{e}"),
            }
        }
        "/mode" => {
            let m = arg.to_ascii_lowercase();
            if m == "hold" || m == "toggle" {
                let mut cfg = crate::config::load()?;
                cfg.hotkey_mode = m.clone();
                crate::config::write(&crate::config::config_path(), &cfg)?;
                app.status = format!("热键模式 → {m}");
            } else {
                app.status = "用法：/mode hold|toggle".into();
            }
        }
        "/record" => {
            let recording = matches!(app.phase, Phase::Recording(_));
            send_control(if recording { "stop" } else { "start" });
            app.status = if recording { "已停止并提交".into() } else { "开始录音…".into() };
        }
        "/search" => {
            app.query = arg.clone();
            app.refilter();
            app.status = if arg.is_empty() { "已清除筛选".into() } else { format!("筛选：{arg}") };
        }
        "/clear" => app.overlay = Some(Overlay::ConfirmClear),
        "/refresh" => {
            app.reload();
            app.status = "已刷新".into();
        }
        "/daemon" => {
            if arg == "stop" {
                app.overlay = Some(Overlay::ConfirmQuitDaemon);
            } else {
                app.status = "用法：/daemon stop".into();
            }
        }
        "/quit" => {
            app.input.clear();
            app.quit_requested = true;
        }
        other => app.status = format!("没有这个命令：{other}（/help）"),
    }
    app.input.clear();
    Ok(())
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    if let Some(form) = &app.settings {
        crate::settings_ui::draw_form(f, form);
        return;
    }
    let th = app.theme;
    use ratatui::layout::Rect;

    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);

    let left = Line::from(vec![
        Span::styled("◆ ", Style::default().fg(th.accent)),
        Span::styled("dictate", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), Style::default().fg(th.faint)),
        Span::styled(
            if app.subtitle.is_empty() { String::new() } else { format!("  {}", app.subtitle) },
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
        Span::styled(
            format!(" {phase_text}   {} 条 ", app.shown.len()),
            Style::default().fg(th.dim),
        ),
    ])
    .alignment(ratatui::layout::Alignment::Right);
    let wide = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[0]);
    f.render_widget(Paragraph::new(left), wide[0]);
    f.render_widget(Paragraph::new(right), wide[1]);

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
            .enumerate()
            .map(|(pos, i)| {
                let is_selected = pos == app.selected;
                let e = &app.all[*i];
                let time = e.ts.get(11..16).unwrap_or("--:--");
                let (body, color) = if e.is_ok() {
                    (e.text.replace('\n', " "), Color::Reset)
                } else {
                    (e.status.trim_start_matches("error: ").replace('\n', " "), th.rec)
                };
                let marker = if is_selected { "▌" } else { " " };
                ListItem::new(Line::from(vec![
                    Span::styled(marker.to_string(), Style::default().fg(th.accent)),
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
            .highlight_style(Style::default().bg(th.selected_bg).add_modifier(Modifier::BOLD))
            .highlight_symbol("");
        let mut state = ListState::default();
        state.select(Some(app.selected));
        f.render_stateful_widget(list, list_area, &mut state);
    }

    // 底部：真正的输入行（grok-cli 的位置），输入法光标就落在这里
    let panel = chunks[3];
    f.render_widget(Block::default().style(Style::default().bg(th.panel_bg)), panel);
    let inner = Rect {
        x: panel.x + 2,
        y: panel.y,
        width: panel.width.saturating_sub(4),
        height: 1,
    };

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("❯ ", Style::default().fg(th.accent)),
            Span::styled(app.input.clone(), Style::default().fg(Color::Reset)),
        ]))
        .style(Style::default().bg(th.panel_bg)),
        inner,
    );
    let w = unicode_width::UnicodeWidthStr::width(app.input.as_str()) as u16;
    f.set_cursor_position(((inner.x + 2 + w).min(inner.x + inner.width - 1), inner.y));

    let second = if app.input.starts_with('/') {
        let hits: Vec<&(&str, &str, &str)> = COMMANDS
            .iter()
            .filter(|(c, _, _)| c.starts_with(app.input.as_str()))
            .collect();
        if hits.is_empty() {
            Line::from(Span::styled("没有匹配的命令", Style::default().fg(th.faint)))
        } else {
            let mut spans = Vec::new();
            for (name, arg, desc) in hits.iter().take(4) {
                spans.push(Span::styled((*name).to_string(), Style::default().fg(th.accent)));
                if !arg.is_empty() {
                    spans.push(Span::styled(format!(" {arg}"), Style::default().fg(th.faint)));
                }
                spans.push(Span::styled(format!(" {desc}   "), Style::default().fg(th.dim)));
            }
            Line::from(spans)
        }
    } else if !app.status.is_empty() {
        Line::from(Span::styled(app.status.clone(), Style::default().fg(th.dim)))
    } else {
        match app.phase {
            Phase::Idle => Line::from(Span::styled(
                "↑↓ 选择   ⏎ 复制选中   / 命令   Del 删除   Esc 退出",
                Style::default().fg(th.faint),
            )),
            Phase::Recording(t) => Line::from(vec![
                Span::styled("● ", Style::default().fg(th.rec)),
                Span::styled(
                    format!("录音中 {:.1}s   松开提交", t.elapsed().as_secs_f32()),
                    Style::default().fg(th.rec).add_modifier(Modifier::BOLD),
                ),
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
    f.render_widget(
        Paragraph::new(second).style(Style::default().bg(th.panel_bg)),
        Rect { y: panel.y + 1, ..inner },
    );

    if let Some(ov) = app.overlay {
        draw_overlay(f, app, ov);
    }
}

fn draw_overlay(f: &mut ratatui::Frame, app: &App, ov: Overlay) {
    use ratatui::widgets::Clear;
    let th = app.theme;
    let area = f.area();
    let lines: Vec<Line> = match ov {
        Overlay::Help => vec![
            Line::from(vec![
                Span::styled("◆ ", Style::default().fg(th.accent)),
                Span::styled("dictate 帮助", Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(""),
            Line::from(Span::styled(format!("  热键：{}", app.subtitle), Style::default().fg(th.dim))),
            Line::from(Span::styled("       按住说话（配置 hotkey_mode = hold）", Style::default().fg(th.dim))),
            Line::from(Span::styled("       或按一下开、再按一下停（toggle）", Style::default().fg(th.dim))),
            Line::from(""),
            Line::from("  空格      手动开始 / 停止录音"),
            Line::from("  ⏎         复制选中记录（OSC 52）"),
            Line::from("  Del       删除选中记录"),
            Line::from("  Ctrl+F    搜索          Ctrl+R  刷新"),
            Line::from("  Ctrl+L    清空全部（需确认）"),
            Line::from("  F2        设置          F1      本页"),
            Line::from("  Ctrl+Q    退出守护进程（需确认）"),
            Line::from("  Esc/F10   退出这个视图（守护进程继续跑）"),
            Line::from(""),
            Line::from(Span::styled("  任意键关闭", Style::default().fg(th.faint))),
        ],
        Overlay::ConfirmClear => vec![
            Line::from(Span::styled("清空全部历史？", Style::default().add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from(Span::styled("回车确认，Esc 取消", Style::default().fg(th.dim))),
        ],
        Overlay::ConfirmQuitDaemon => vec![
            Line::from(Span::styled("退出守护进程？", Style::default().add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from(Span::styled("托盘图标会消失、热键失效。回车确认，Esc 取消", Style::default().fg(th.dim))),
        ],
    };

    let w = lines.iter().map(|l| l.width() as u16).max().unwrap_or(30) + 6;
    let h = lines.len() as u16 + 2;
    let rect = ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(th.panel_bg))
            .block(
                Block::default()
                    .style(Style::default().bg(th.panel_bg))
                    .border_style(Style::default().fg(th.faint)),
            ),
        rect,
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

    fn key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn app_with(tmp: &str) -> App {
        std::fs::create_dir_all(tmp).unwrap();
        let h = History::new(std::path::Path::new(tmp).join("history.jsonl"));
        App::new(h, Theme::dark())
    }

    #[test]
    fn typing_slash_opens_command_palette_state() {
        let dir = std::env::temp_dir().join(format!("dictate-cli-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &dir);
        let mut app = app_with(dir.join("dictate").to_str().unwrap());
        handle_key(&mut app, &key('/')).unwrap();
        assert_eq!(app.input, "/");
        handle_key(&mut app, &key('s')).unwrap();
        assert_eq!(app.input, "/s");
        // 过滤后确实只剩 /settings、/search
        let hits: Vec<_> = COMMANDS
            .iter()
            .filter(|(c, _, _)| c.starts_with(app.input.as_str()))
            .map(|(c, _, _)| *c)
            .collect();
        assert_eq!(hits, vec!["/settings", "/search"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enter_runs_settings_command() {
        let dir = std::env::temp_dir().join(format!("dictate-cli2-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &dir);
        let mut app = app_with(dir.join("dictate").to_str().unwrap());
        for c in "/settings".chars() {
            handle_key(&mut app, &key(c)).unwrap();
        }
        handle_key(&mut app, &crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert!(app.settings.is_some(), "/settings 应该打开设置页");
        assert!(app.input.is_empty(), "提交后输入行清空");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn esc_clears_input_then_quits() {
        let dir = std::env::temp_dir().join(format!("dictate-cli3-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &dir);
        let mut app = app_with(dir.join("dictate").to_str().unwrap());
        for c in "abc".chars() {
            handle_key(&mut app, &key(c)).unwrap();
        }
        let esc = crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(handle_key(&mut app, &esc).unwrap(), Outcome::Continue));
        assert!(app.input.is_empty());
        assert!(matches!(handle_key(&mut app, &esc).unwrap(), Outcome::Quit));
        std::fs::remove_dir_all(&dir).ok();
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
