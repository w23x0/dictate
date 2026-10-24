//! 终端界面：历史列表 + 实时状态。两种入口——
//!
//! * [`run`]：只看历史（`dictate history`）。
//! * [`run_live`]：热键守护 + 历史（`dictate run`），录音/转写状态显示在顶栏。
//!
//! 操作逻辑对着 grok-cli / opencode 抄：
//! 底部一行输入（`/` 开头是命令，其余是实时过滤）、`/` 弹出命令面板
//!（模糊过滤 + ↑↓ + ⏎）、j/k/g/gG 导航、toast 式状态提示、圆角对话框。

use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::config::Config;
use crate::core::history::{Entry, History};
use crate::ui::theme::Theme;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// 列表项前缀 "▌ 22:37  ❯ " 的显示宽度，折行的续行按它缩进。
const LIST_PREFIX: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Recording(Instant),
    Transcribing(Instant),
}

/// 短暂提示（toast）：显示几秒后自动消失，不再被状态文件轮询冲掉。
#[derive(Clone)]
struct Toast {
    msg: String,
    err: bool,
    at: Instant,
}

struct App {
    history: History,
    all: Vec<Entry>,
    shown: Vec<usize>,
    query: String,
    selected: usize,
    /// 守护进程状态文件里带出来的持久提示（完成/失败/没在跑），显示在第二行
    daemon_note: String,
    phase: Phase,
    /// 顶栏副标题（热键 · 模型），只在 run 模式有
    subtitle: String,
    theme: Theme,
    overlay: Option<Overlay>,
    /// 内嵌的设置页（`/settings` 进入，Esc 退出）
    settings: Option<crate::ui::settings_ui::Form>,
    /// 底部命令行（grok-cli 的位置），`/` 开头是命令
    input: String,
    /// 命令面板里选中的条目
    palette_sel: usize,
    toast: Option<Toast>,
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
            daemon_note: String::new(),
            phase: Phase::Idle,
            subtitle: String::new(),
            theme,
            overlay: None,
            settings: None,
            input: String::new(),
            palette_sel: 0,
            toast: None,
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
        // 倒序：**最新的一条在最上面**，选中项默认就是它
        self.shown = self
            .all
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, e)| q.is_empty() || e.text.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.shown.len() {
            self.selected = self.shown.len().saturating_sub(1);
        }
    }

    /// 有新记录时：如果用户还停在最新那条，就跟着走；否则保持他正在看的位置。
    fn reload_keep_position(&mut self) {
        let was_newest = self.selected == 0;
        self.reload();
        if was_newest {
            self.selected = 0;
        }
    }

    fn current(&self) -> Option<&Entry> {
        self.shown.get(self.selected).map(|i| &self.all[*i])
    }
}

fn toast(app: &mut App, msg: impl Into<String>) {
    app.toast = Some(Toast {
        msg: msg.into(),
        err: false,
        at: Instant::now(),
    });
}

fn toast_err(app: &mut App, msg: impl Into<String>) {
    app.toast = Some(Toast {
        msg: msg.into(),
        err: true,
        at: Instant::now(),
    });
}

// ───────────────────────── 命令面板 ─────────────────────────

/// 输入处于「命令面板」状态：`/` 开头且还没敲参数（一敲空格就变成带参命令行）。
fn palette_query(input: &str) -> Option<&str> {
    let s = input.strip_prefix('/')?;
    if s.contains(' ') {
        None
    } else {
        Some(s)
    }
}

/// fzf 味的匹配：子串优先（返回子串下标），否则子序列；都不行返回 None。
/// 下标按**字符**计，相对 `text`，用于高亮命中的字符。
fn fuzzy_hits(pat: &str, text: &str) -> Option<Vec<usize>> {
    let pat = pat.to_lowercase();
    if pat.is_empty() {
        return Some(Vec::new());
    }
    if let Some(at) = text.to_lowercase().find(&pat) {
        let start = text[..at].chars().count();
        return Some((0..pat.chars().count()).map(|i| start + i).collect());
    }
    let mut idx = Vec::new();
    let mut pi = pat.chars();
    let mut want = pi.next()?;
    for (ti, ch) in text.chars().enumerate() {
        if ch == want {
            idx.push(ti);
            match pi.next() {
                Some(n) => want = n,
                None => return Some(idx),
            }
        }
    }
    None
}

/// 面板条目：（命令在 COMMANDS 里的下标，命中的字符下标——相对去掉 `/` 的名字）。
/// 前缀命中排前面，剩下的按声明顺序，这样 `/re` ⏎ 一定是 `/record`。
fn palette_hits(input: &str) -> Vec<(usize, Vec<usize>)> {
    let Some(pat) = palette_query(input) else {
        return Vec::new();
    };
    let mut head = Vec::new();
    let mut tail = Vec::new();
    for (i, (name, _, _)) in COMMANDS.iter().enumerate() {
        let bare = &name[1..];
        if let Some(hl) = fuzzy_hits(pat, bare) {
            if bare.starts_with(pat) {
                head.push((i, hl));
            } else {
                tail.push((i, hl));
            }
        }
    }
    head.extend(tail);
    head
}

/// 已带参数的命令行（"/model xxx"）底下那行提示用。
fn cmd_hint(line: &str) -> Option<(&'static str, &'static str, &'static str)> {
    let first = line.split(' ').next().unwrap_or("");
    COMMANDS.iter().find(|(n, _, _)| *n == first).copied()
}

// ───────────────────────── 文本加工 ─────────────────────────

/// 按显示宽度贪心折行：优先在空格断，装不下的长词（或没有空格的中文）硬断。
pub fn wrap_width(s: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for tok in s.split(' ') {
        let mut rest = tok;
        loop {
            let rw = UnicodeWidthStr::width(rest);
            let sep = if cur_w == 0 { 0 } else { 1 };
            if cur_w + sep + rw <= width {
                if sep == 1 {
                    cur.push(' ');
                    cur_w += 1;
                }
                cur.push_str(rest);
                cur_w += rw;
                break;
            }
            // 当前词放不进这行：先把凑合的行收掉
            if cur_w > 0 {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            if rw <= width {
                cur.push_str(rest);
                cur_w = rw;
                break;
            }
            // 词本身就比一行宽：按字符硬断
            let mut w = 0usize;
            let mut cut = rest.len();
            for (i, ch) in rest.char_indices() {
                let cw = ch.width().unwrap_or(0);
                if w + cw > width {
                    cut = i;
                    break;
                }
                w += cw;
            }
            let (head, tail) = rest.split_at(cut);
            out.push(head.to_string());
            rest = tail;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// 按显示宽度截断，尾巴加省略号。
fn trunc(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    let mut w = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// 历史条目按天分组的小标题。
fn day_label_at(ts: &str, today: chrono::NaiveDate, yesterday: chrono::NaiveDate) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => {
            let d = dt.date_naive();
            if d == today {
                "今天".into()
            } else if d == yesterday {
                "昨天".into()
            } else {
                d.format("%m-%d").to_string()
            }
        }
        Err(_) => ts.get(0..10).unwrap_or("").into(),
    }
}

fn day_label(ts: &str) -> String {
    let today = chrono::Local::now().date_naive();
    let yesterday = today.pred_opt().unwrap_or(today);
    day_label_at(ts, today, yesterday)
}

/// OSC 52：把文本放进终端所在机器的剪贴板（SSH 里也有效）。
pub fn osc52(text: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x07")
}

/// `dictate history`：只读视图，但**实时**（轮询守护进程状态 + 历史文件）。
pub fn run(history: History) -> Result<()> {
    with_terminal(|term| {
        let cfg = crate::core::config::load().ok();
        let theme = Theme::resolve(cfg.as_ref().and_then(|c| c.theme.as_deref()));
        let mut app = App::new(history, theme);
        if let Some(c) = cfg {
            app.subtitle = format!("{} · {}", c.hotkey, c.model);
        }
        attach_loop(term, &mut app)
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
    apply_status_from(&crate::core::config::status_path(), app);
}

fn apply_status_from(path: &std::path::Path, app: &mut App) {
    let Ok(body) = std::fs::read_to_string(path) else {
        app.phase = Phase::Idle;
        app.daemon_note = "守护进程没在跑 —— 先执行 dictate".into();
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
    let at = Instant::now()
        .checked_sub(elapsed)
        .unwrap_or_else(Instant::now);
    app.phase = match st.phase.as_str() {
        "recording" => Phase::Recording(at),
        "transcribing" => Phase::Transcribing(at),
        _ => Phase::Idle,
    };
    app.daemon_note = match (&st.error, &st.text) {
        (Some(e), _) => format!("失败：{e}"),
        (None, Some(t)) => format!("完成：{}", t.chars().take(40).collect::<String>()),
        (None, None) => String::new(),
    };
}

/// 附着模式：只画，不抢热键/麦克风。
fn attach_loop(term: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    let mut stamp = (0u64, 0u64);
    loop {
        if let Some(form) = app.settings.as_mut() {
            form.poll_test();
        }
        apply_status(app);
        let now = std::fs::metadata(crate::core::config::history_path())
            .map(|m| {
                use std::time::UNIX_EPOCH;
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                (m.len(), t)
            })
            .unwrap_or((0, 0));
        if now != stamp {
            stamp = now;
            app.reload_keep_position();
        }
        term.draw(|f| draw(f, app))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
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
    let path = crate::core::config::control_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let _ = std::fs::write(&path, cmd);
}

fn toggle_record(app: &mut App) {
    let recording = matches!(app.phase, Phase::Recording(_));
    send_control(if recording { "stop" } else { "start" });
    toast(
        app,
        if recording {
            "已停止并提交"
        } else {
            "开始录音…"
        },
    );
}

fn open_settings(app: &mut App) -> Result<()> {
    let cfg = crate::core::config::load()?;
    let mut form = crate::ui::settings_ui::Form::new(cfg);
    form.theme = app.theme;
    app.settings = Some(form);
    Ok(())
}

/// 处理一个按键。输入行为和 grok-cli 一致：底部一行输入框，`/` 开头是命令，
/// 其余文字实时过滤历史；输入为空时 j/k/g/G 等 vim 键归列表导航。
fn handle_key(app: &mut App, k: &crossterm::event::KeyEvent) -> Result<Outcome> {
    if let Some(form) = app.settings.as_mut() {
        if crate::ui::settings_ui::handle_form_key(form, k)? {
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
                    toast(app, "已清空");
                    app.overlay = None;
                }
                KeyCode::Esc => app.overlay = None,
                _ => {}
            },
            Overlay::ConfirmQuitDaemon => match k.code {
                KeyCode::Enter => {
                    send_control("quit");
                    toast(app, "已通知守护进程退出");
                    app.overlay = None;
                }
                KeyCode::Esc => app.overlay = None,
                _ => {}
            },
        }
        return Ok(Outcome::Continue);
    }

    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let hits = palette_hits(&app.input);
    let palette = palette_query(&app.input).is_some() && !hits.is_empty();
    let sel = app.palette_sel.min(hits.len().saturating_sub(1));

    // Ctrl 系列与功能键
    match k.code {
        KeyCode::Char('c') if ctrl => return Ok(Outcome::Quit),
        KeyCode::Char('d') if ctrl && app.input.is_empty() => {
            app.selected = (app.selected + 12).min(app.shown.len().saturating_sub(1));
            return Ok(Outcome::Continue);
        }
        KeyCode::Char('u') if ctrl && app.input.is_empty() => {
            app.selected = app.selected.saturating_sub(12);
            return Ok(Outcome::Continue);
        }
        KeyCode::Char('f') if ctrl => {
            if app.input.starts_with('/') {
                app.input.clear();
            }
            toast(app, "直接输入文字即可过滤历史");
            return Ok(Outcome::Continue);
        }
        KeyCode::Char('r') if ctrl => {
            app.reload();
            toast(app, "已刷新");
            return Ok(Outcome::Continue);
        }
        KeyCode::Char('l') if ctrl => {
            app.overlay = Some(Overlay::ConfirmClear);
            return Ok(Outcome::Continue);
        }
        KeyCode::Char('q') if ctrl => {
            app.overlay = Some(Overlay::ConfirmQuitDaemon);
            return Ok(Outcome::Continue);
        }
        KeyCode::F(1) => {
            app.overlay = Some(Overlay::Help);
            return Ok(Outcome::Continue);
        }
        KeyCode::F(2) => {
            open_settings(app)?;
            return Ok(Outcome::Continue);
        }
        KeyCode::F(10) => return Ok(Outcome::Quit),
        _ => {}
    }

    match k.code {
        KeyCode::Esc => {
            if app.input.is_empty() {
                return Ok(Outcome::Quit);
            }
            app.input.clear();
            app.query.clear();
            app.palette_sel = 0;
            app.refilter();
        }
        KeyCode::Enter => {
            if palette {
                // 面板里回车：无参命令直接执行，带参命令把名字填进去继续敲参数
                let (ci, _) = hits[sel];
                let (name, arg, _) = COMMANDS[ci];
                if arg.is_empty() {
                    app.input = name.to_string();
                    run_input(app)?;
                    if app.quit_requested {
                        return Ok(Outcome::Quit);
                    }
                } else {
                    app.input = format!("{name} ");
                    app.palette_sel = 0;
                }
            } else {
                run_input(app)?;
                if app.quit_requested {
                    return Ok(Outcome::Quit);
                }
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
            app.palette_sel = 0;
            if app.input.starts_with('/') {
                app.query.clear();
            } else {
                app.query = app.input.clone();
            }
            app.refilter();
        }
        KeyCode::Up => {
            if palette {
                app.palette_sel = sel.saturating_sub(1);
            } else {
                app.selected = app.selected.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if palette {
                app.palette_sel = (sel + 1).min(hits.len() - 1);
            } else if app.selected + 1 < app.shown.len() {
                app.selected += 1;
            }
        }
        KeyCode::PageUp => app.selected = app.selected.saturating_sub(10),
        KeyCode::PageDown => {
            app.selected = (app.selected + 10).min(app.shown.len().saturating_sub(1));
        }
        // 列表是倒序的：Home/g = 最新，End/G = 最旧
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = app.shown.len().saturating_sub(1),
        KeyCode::Tab if palette => {
            let (ci, _) = hits[sel];
            let (name, arg, _) = COMMANDS[ci];
            app.input = if arg.is_empty() {
                name.to_string()
            } else {
                format!("{name} ")
            };
        }
        KeyCode::Delete => {
            if let Some(e) = app.current() {
                let id = e.id.clone();
                app.history.delete(&id)?;
                app.reload();
                toast(app, "已删除一条");
            }
        }
        // 输入为空时这些键是导航/操作，不进输入框（想按文字过滤就用别的字符起头）
        KeyCode::Char(' ') if app.input.is_empty() => toggle_record(app),
        KeyCode::Char('j') if app.input.is_empty() => {
            if app.selected + 1 < app.shown.len() {
                app.selected += 1;
            }
        }
        KeyCode::Char('k') if app.input.is_empty() => app.selected = app.selected.saturating_sub(1),
        KeyCode::Char('g') if app.input.is_empty() => app.selected = 0,
        KeyCode::Char('G') if app.input.is_empty() => {
            app.selected = app.shown.len().saturating_sub(1)
        }
        KeyCode::Char(c) if !ctrl => {
            app.input.push(c);
            app.palette_sel = 0;
            // `/` 开头是命令，不拿去做过滤，否则打命令时列表会瞬间空掉
            if app.input.starts_with('/') {
                app.query.clear();
            } else {
                app.query = app.input.clone();
            }
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
            toast(app, format!("已复制 {} 字", text.chars().count()));
        }
        return Ok(());
    }
    if !line.starts_with('/') {
        app.query = line.clone();
        app.refilter();
        toast(app, format!("筛选：{line}（Esc 清除）"));
        app.input.clear();
        return Ok(());
    }
    // 命令不参与过滤
    app.query.clear();

    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim().to_string();
    match cmd {
        "/help" => app.overlay = Some(Overlay::Help),
        "/settings" => open_settings(app)?,
        "/model" => {
            if arg.is_empty() {
                let cfg = crate::core::config::load()?;
                let mut form = crate::ui::settings_ui::Form::new(cfg);
                form.theme = app.theme;
                form.open_model_picker();
                app.settings = Some(form);
            } else {
                let mut cfg = crate::core::config::load()?;
                if crate::asr::pick_style(&arg, None).is_err() {
                    toast_err(app, format!("{arg} 不支持（异步 filetrans 没做）"));
                } else {
                    cfg.model = arg.clone();
                    cfg.api_style = None;
                    crate::core::config::write(&crate::core::config::config_path(), &cfg)?;
                    toast(app, format!("模型 → {arg}（下次录音生效）"));
                }
            }
        }
        "/base" => {
            if arg.is_empty() {
                let cfg = crate::core::config::load()?;
                let mut form = crate::ui::settings_ui::Form::new(cfg);
                form.theme = app.theme;
                form.open_base_picker();
                app.settings = Some(form);
            } else {
                let mut cfg = crate::core::config::load()?;
                match crate::asr::normalize_base(&arg) {
                    Ok(b) => {
                        cfg.base_url = b;
                        crate::core::config::write(&crate::core::config::config_path(), &cfg)?;
                        toast(app, format!("服务地址 → {}", cfg.base_url));
                    }
                    Err(e) => toast_err(app, format!("地址不合法：{e}")),
                }
            }
        }
        "/key" => {
            if arg.is_empty() {
                toast(app, "用法：/key sk-…");
            } else {
                let mut cfg = crate::core::config::load()?;
                cfg.api_key = Some(arg);
                crate::core::config::write(&crate::core::config::config_path(), &cfg)?;
                toast(app, "已保存 key（存在配置文件里，权限 600）");
            }
        }
        "/hotkey" => match crate::hotkey::Spec::parse(&arg) {
            Ok(_) => {
                let mut cfg = crate::core::config::load()?;
                cfg.hotkey = arg.clone();
                crate::core::config::write(&crate::core::config::config_path(), &cfg)?;
                toast(app, format!("热键 → {arg}（守护进程会自动热加载）"));
            }
            Err(e) => toast_err(app, format!("热键不合法：{e}")),
        },
        "/mode" => {
            let m = arg.to_ascii_lowercase();
            if m == "hold" || m == "toggle" {
                let mut cfg = crate::core::config::load()?;
                cfg.hotkey_mode = m.clone();
                crate::core::config::write(&crate::core::config::config_path(), &cfg)?;
                toast(app, format!("热键模式 → {m}"));
            } else {
                toast(app, "用法：/mode hold|toggle");
            }
        }
        "/record" => toggle_record(app),
        "/search" => {
            app.query = arg.clone();
            app.refilter();
            if arg.is_empty() {
                toast(app, "已清除筛选");
            } else {
                toast(app, format!("筛选：{arg}"));
            }
        }
        "/clear" => app.overlay = Some(Overlay::ConfirmClear),
        "/refresh" => {
            app.reload();
            toast(app, "已刷新");
        }
        "/daemon" => {
            if arg == "stop" {
                app.overlay = Some(Overlay::ConfirmQuitDaemon);
            } else {
                toast(app, "用法：/daemon stop");
            }
        }
        "/quit" => {
            app.input.clear();
            app.quit_requested = true;
        }
        other => toast_err(app, format!("没有这个命令：{other}（/help）")),
    }
    app.input.clear();
    Ok(())
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    if let Some(form) = &app.settings {
        crate::ui::settings_ui::draw_form(f, form);
        return;
    }
    let th = app.theme;

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
    let (dot, mut dot_color, phase_text) = match app.phase {
        Phase::Idle => ("●", th.ok, "就绪"),
        Phase::Recording(_) => ("●", th.rec, "录音中"),
        Phase::Transcribing(_) => ("◐", th.warn, "转写中"),
    };
    // 录音时呼吸灯：500 ms 一闪
    if let Phase::Recording(t) = app.phase {
        if (t.elapsed().as_millis() / 500) % 2 == 1 {
            dot_color = th.faint;
        }
    }
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

    // ── 第二行：筛选状态 / 守护进程最近结果
    if !app.query.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("筛选 ", Style::default().fg(th.faint)),
                Span::styled(
                    trunc(&app.query, 30),
                    Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}/{} 条  Esc 清除", app.shown.len(), app.all.len()),
                    Style::default().fg(th.faint),
                ),
            ])),
            chunks[1],
        );
    } else if !app.daemon_note.is_empty() {
        let bad = app.daemon_note.starts_with("失败") || app.daemon_note.contains("没在跑");
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                trunc(&app.daemon_note, chunks[1].width.saturating_sub(2) as usize),
                Style::default().fg(if bad { th.rec } else { th.dim }),
            ))),
            chunks[1],
        );
    }

    // ── 历史列表：按天分组 + 自动折行
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
        let mut items: Vec<ListItem> = Vec::new();
        let mut first_item_of: Vec<usize> = Vec::with_capacity(app.shown.len());
        let mut prev_day = String::new();
        let body_w = (list_area.width.saturating_sub(LIST_PREFIX as u16 + 2)) as usize;

        for (pos, i) in app.shown.iter().enumerate() {
            let e = &app.all[*i];
            let day = e.ts.get(0..10).unwrap_or("").to_string();
            if day != prev_day {
                prev_day = day;
                let label = day_label(&e.ts);
                let lw = UnicodeWidthStr::width(label.as_str());
                let fill = list_area.width.saturating_sub(lw as u16 + 4).min(60) as usize;
                items.push(ListItem::new(Line::from(Span::styled(
                    format!(" {label} {}", "─".repeat(fill)),
                    Style::default().fg(th.faint),
                ))));
            }
            first_item_of.push(items.len());

            let is_selected = pos == app.selected;
            let time = e.ts.get(11..16).unwrap_or("--:--");
            let (body, color) = if e.is_ok() {
                (e.text.replace('\n', " "), Color::Reset)
            } else {
                (
                    e.status.trim_start_matches("error: ").replace('\n', " "),
                    th.rec,
                )
            };
            let lines = wrap_width(&body, body_w);
            let last = lines.len() - 1;
            for (li, text) in lines.iter().enumerate() {
                let mut spans: Vec<Span> = Vec::new();
                if li == 0 {
                    spans.push(Span::styled(
                        if is_selected { "▌" } else { " " }.to_string(),
                        Style::default().fg(th.accent),
                    ));
                    spans.push(Span::styled(
                        format!(" {time}  "),
                        Style::default().fg(th.faint),
                    ));
                    spans.push(Span::styled("❯ ", Style::default().fg(th.accent)));
                } else {
                    spans.push(Span::raw(" ".repeat(LIST_PREFIX)));
                }
                spans.push(Span::styled(text.clone(), Style::default().fg(color)));
                if li == last {
                    spans.push(Span::styled(
                        format!(
                            "  {:.1}s → {:.1}s",
                            e.duration_ms as f32 / 1000.0,
                            e.latency_ms as f32 / 1000.0
                        ),
                        Style::default().fg(th.faint),
                    ));
                }
                let mut line = Line::from(spans);
                if is_selected {
                    line = line.style(Style::default().bg(th.selected_bg));
                }
                items.push(ListItem::new(line));
            }
        }

        let list = List::new(items)
            .highlight_style(Style::default().bg(th.selected_bg))
            .highlight_symbol("");
        let mut state = ListState::default();
        state.select(first_item_of.get(app.selected).copied());
        f.render_stateful_widget(list, list_area, &mut state);
    }

    // ── 底部输入面板
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

    let input_spans = if app.input.is_empty() {
        vec![
            Span::styled("❯ ", Style::default().fg(th.accent)),
            Span::styled(
                "输入过滤历史 · / 命令 · ⏎ 复制选中",
                Style::default().fg(th.faint),
            ),
        ]
    } else {
        vec![
            Span::styled("❯ ", Style::default().fg(th.accent)),
            Span::styled(app.input.clone(), Style::default().fg(Color::Reset)),
        ]
    };
    f.render_widget(
        Paragraph::new(Line::from(input_spans)).style(Style::default().bg(th.panel_bg)),
        inner,
    );
    let w = unicode_width::UnicodeWidthStr::width(app.input.as_str()) as u16;
    f.set_cursor_position(((inner.x + 2 + w).min(inner.x + inner.width - 1), inner.y));

    // ── 第二行：命令提示 > toast > 相位
    let hits = palette_hits(&app.input);
    let second: Line = if app.input.starts_with('/') {
        if let Some(&(ci, _)) = hits.first() {
            // 面板打开着：提示怎么操作
            let (_, _, desc) = COMMANDS[ci];
            Line::from(Span::styled(
                format!("{desc}   ↑↓ 选择  ⏎ 执行  Tab 补全"),
                Style::default().fg(th.faint),
            ))
        } else if let Some((name, arg, desc)) = cmd_hint(&app.input) {
            Line::from(Span::styled(
                format!("{name} {arg} — {desc}"),
                Style::default().fg(th.faint),
            ))
        } else {
            Line::from(Span::styled(
                "没有匹配的命令",
                Style::default().fg(th.faint),
            ))
        }
    } else if let Some(t) = &app.toast {
        let fresh = t.at.elapsed().as_secs() < if t.err { 8 } else { 5 };
        if fresh {
            Line::from(Span::styled(
                t.msg.clone(),
                Style::default().fg(if t.err { th.rec } else { th.dim }),
            ))
        } else if matches!(app.phase, Phase::Idle) {
            hint_line(app, th)
        } else {
            phase_line(app, th)
        }
    } else if matches!(app.phase, Phase::Idle) {
        hint_line(app, th)
    } else {
        phase_line(app, th)
    };
    f.render_widget(
        Paragraph::new(second).style(Style::default().bg(th.panel_bg)),
        Rect {
            y: panel.y + 1,
            ..inner
        },
    );

    // ── 命令面板（浮在输入框上方，opencode 的命令菜单样式）
    if palette_query(&app.input).is_some() && !hits.is_empty() {
        let sel = app.palette_sel.min(hits.len() - 1);
        let n = hits.len().min(8) as u16;
        let w = 64.min(area.width.saturating_sub(4));
        let rect = Rect {
            x: area.x + 2,
            y: panel.y.saturating_sub(n + 2 + 1),
            width: w,
            height: n + 2,
        };
        clear_with_margin(f, rect);
        let rows: Vec<Line> = hits
            .iter()
            .take(8)
            .enumerate()
            .map(|(row, &(ci, ref hl))| {
                let (name, arg, desc) = COMMANDS[ci];
                let bare = &name[1..];
                let mut spans = vec![Span::styled("/", Style::default().fg(th.faint))];
                for (ci2, ch) in bare.chars().enumerate() {
                    let hit = hl.contains(&ci2);
                    spans.push(Span::styled(
                        ch.to_string(),
                        Style::default()
                            .fg(if hit { th.accent } else { th.dim })
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                if !arg.is_empty() {
                    spans.push(Span::styled(
                        format!(" {arg}"),
                        Style::default().fg(th.faint),
                    ));
                }
                let used = 1 + bare.len() + arg.len() + 4;
                spans.push(Span::styled(
                    format!("  {}", trunc(desc, (w as usize).saturating_sub(used))),
                    Style::default().fg(th.dim),
                ));
                let mut line = Line::from(spans);
                if row == sel {
                    line = line.style(Style::default().bg(th.selected_bg));
                }
                line
            })
            .collect();
        f.render_widget(
            Paragraph::new(rows).block(
                Block::default()
                    .border_type(BorderType::Rounded)
                    .borders(ratatui::widgets::Borders::ALL)
                    .title(Span::styled(" 命令 ", Style::default().fg(th.accent)))
                    .border_style(Style::default().fg(th.faint))
                    .style(Style::default().bg(th.panel_bg)),
            ),
            rect,
        );
    }

    if let Some(ov) = app.overlay {
        draw_overlay(f, app, ov);
    }
}

fn hint_line(app: &App, th: Theme) -> Line<'static> {
    let _ = app;
    Line::from(Span::styled(
        "↑↓/jk 选择 · 空格录音 · / 命令 · ⏎ 复制 · Del 删除 · F1 帮助",
        Style::default().fg(th.faint),
    ))
}

fn phase_line(app: &App, th: Theme) -> Line<'static> {
    match app.phase {
        Phase::Recording(t) => Line::from(vec![
            Span::styled("● ", Style::default().fg(th.rec)),
            Span::styled(
                format!(
                    "录音中 {:.1}s   松开提交（空格或 /record 手动停）",
                    t.elapsed().as_secs_f32()
                ),
                Style::default().fg(th.rec).add_modifier(Modifier::BOLD),
            ),
        ]),
        Phase::Transcribing(t) => {
            let frame = FRAMES[(t.elapsed().as_millis() / 80) as usize % FRAMES.len()];
            Line::from(vec![
                Span::styled(format!("{frame} "), Style::default().fg(th.warn)),
                Span::styled("转写中…", Style::default().fg(th.warn)),
            ])
        }
        Phase::Idle => hint_line(app, th),
    }
}

fn draw_overlay(f: &mut ratatui::Frame, app: &App, ov: Overlay) {
    let th = app.theme;
    let area = f.area();
    let (title, lines): (&str, Vec<Line>) = match ov {
        Overlay::Help => (
            " 帮助 ",
            vec![
                Line::from(Span::styled(
                    format!("热键 {}（hold 按住说话 / toggle 按一下开关）", app.subtitle),
                    Style::default().fg(th.dim),
                )),
                Line::from(""),
                Line::from("导航    ↑↓ 或 j k 选择    g 最新    G 最旧"),
                Line::from("        Ctrl+D / Ctrl+U 翻半页"),
                Line::from(""),
                Line::from("录音    空格 或 /record 手动开始 / 停止"),
                Line::from(""),
                Line::from("记录    ⏎ 复制选中（OSC 52）   Del 删除"),
                Line::from(""),
                Line::from("命令    / 打开命令面板：↑↓ 选、⏎ 执行、Tab 补全"),
                Line::from("        直接打字 = 实时过滤历史（Esc 清除）"),
                Line::from(""),
                Line::from("其它    F2 设置   Ctrl+R 刷新   Ctrl+L 清空历史"),
                Line::from("        F10 / Esc / Ctrl+C 退出视图（守护进程继续）"),
                Line::from("        Ctrl+Q 退出守护进程（需确认）"),
                Line::from(""),
                Line::from(Span::styled("任意键关闭", Style::default().fg(th.faint))),
            ],
        ),
        Overlay::ConfirmClear => (
            " 确认 ",
            vec![
                Line::from(Span::styled(
                    "清空全部历史？",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "回车确认，Esc 取消",
                    Style::default().fg(th.dim),
                )),
            ],
        ),
        Overlay::ConfirmQuitDaemon => (
            " 确认 ",
            vec![
                Line::from(Span::styled(
                    "退出守护进程？",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "托盘图标会消失、热键失效。回车确认，Esc 取消",
                    Style::default().fg(th.dim),
                )),
            ],
        ),
    };

    let w = lines.iter().map(|l| l.width() as u16).max().unwrap_or(30) + 6;
    let h = lines.len() as u16 + 2;
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    };
    clear_with_margin(f, rect);
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(th.panel_bg))
            .block(
                Block::default()
                    .border_type(BorderType::Rounded)
                    .borders(ratatui::widgets::Borders::ALL)
                    .title(Span::styled(title, Style::default().fg(th.accent)))
                    .border_style(Style::default().fg(th.faint))
                    .style(Style::default().bg(th.panel_bg)),
            ),
        rect,
    );
}

/// 清出对话框底。左边多清一格：如果底层恰好有一个宽字符结束在对话框左边界上，
/// 它的第二格会被 ratatui 的 diff 判定为"跳过"，左边界/角标那一格就画不出来
///（TestBackend 和真实终端都一样）。
fn clear_with_margin(f: &mut ratatui::Frame, rect: Rect) {
    let x = rect.x.saturating_sub(1);
    let width = rect
        .width
        .saturating_add(1)
        .min(f.area().width.saturating_sub(x));
    f.render_widget(Clear, Rect { x, width, ..rect });
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

    // ── 命令面板 ──

    #[test]
    fn fuzzy_finds_substring_then_subsequence() {
        // 子串：返回子串的下标
        assert_eq!(
            fuzzy_hits("re", "/record".trim_start_matches('/')),
            Some(vec![0, 1])
        );
        assert_eq!(fuzzy_hits("cord", "record"), Some(vec![2, 3, 4, 5]));
        // "rd" 在 record 里本身是子串（尾部的 r+d），子串优先于子序列
        assert_eq!(fuzzy_hits("rd", "record"), Some(vec![4, 5]));
        // 纯子序列（不是子串）：o…d
        assert_eq!(fuzzy_hits("od", "record"), Some(vec![3, 5]));
        // 匹配不上
        assert_eq!(fuzzy_hits("z", "record"), None);
        // 空模式全命中（无高亮）
        assert_eq!(fuzzy_hits("", "record"), Some(vec![]));
    }

    #[test]
    fn palette_prefers_prefix_matches() {
        // "se" 是 /settings 的前缀，同时是 /search 的子序列 → /settings 排第一
        let hits = palette_hits("/se");
        assert_eq!(COMMANDS[hits[0].0].0, "/settings");
        assert!(hits.iter().any(|(i, _)| COMMANDS[*i].0 == "/search"));
        // 空输入 = 全部命令，按声明顺序
        let all = palette_hits("/");
        assert_eq!(all.len(), COMMANDS.len());
        assert_eq!(COMMANDS[all[0].0].0, "/settings");
        // 敲了参数就不算面板
        assert!(palette_hits("/model qwen").is_empty());
        assert!(palette_query("/model qwen").is_none());
    }

    #[test]
    fn cmd_hint_matches_first_token() {
        assert_eq!(cmd_hint("/model qwen3-asr-flash").unwrap().0, "/model");
        assert!(cmd_hint("/nope x").is_none());
    }

    // ── 折行 / 截断 / 日期 ──

    #[test]
    fn wrap_breaks_by_display_width() {
        // 中文每字占 2 格
        assert_eq!(wrap_width("你好世界", 6), vec!["你好世", "界"]);
        // 优先在空格断
        assert_eq!(wrap_width("aa bb cc", 5), vec!["aa bb", "cc"]);
        // 长词硬断
        assert_eq!(wrap_width("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // 混排
        assert_eq!(wrap_width("你好 abc", 6), vec!["你好", "abc"]);
    }

    #[test]
    fn trunc_uses_display_width() {
        assert_eq!(trunc("你好世界", 5), "你好…");
        assert_eq!(trunc("abc", 5), "abc");
    }

    #[test]
    fn day_labels_today_yesterday_and_date() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 24).unwrap();
        let yesterday = today.pred_opt().unwrap();
        assert_eq!(
            day_label_at("2026-10-24T10:00:00+08:00", today, yesterday),
            "今天"
        );
        assert_eq!(
            day_label_at("2026-10-23T22:00:00+08:00", today, yesterday),
            "昨天"
        );
        assert_eq!(
            day_label_at("2026-10-01T08:00:00+08:00", today, yesterday),
            "10-01"
        );
        assert_eq!(
            day_label_at("不是时间", today, yesterday),
            "不是时间".get(0..10).unwrap_or("")
        );
    }

    // ── 按键逻辑 ──

    fn key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl_key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn app_with_entries(tmp: &str) -> App {
        std::fs::create_dir_all(tmp).unwrap();
        let path = std::path::Path::new(tmp).join("history.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        for (id, text) in [("old", "第一条"), ("mid", "第二条"), ("new", "第三条")] {
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "id": id, "ts": "2026-10-23T10:00:00+08:00",
                    "duration_ms": 1000, "latency_ms": 10,
                    "model": "qwen3-asr-flash", "status": "ok", "text": text
                })
            )
            .unwrap();
        }
        App::new(History::new(&path), Theme::dark())
    }

    #[test]
    fn vim_keys_navigate_when_input_empty() {
        let dir = std::env::temp_dir().join(format!("dictate-vim-{}", std::process::id()));
        let mut app = app_with_entries(dir.to_str().unwrap());
        assert_eq!(app.selected, 0, "默认停在最新一条");
        handle_key(&mut app, &key('j')).unwrap();
        assert_eq!(app.selected, 1);
        handle_key(&mut app, &key('j')).unwrap();
        handle_key(&mut app, &key('j')).unwrap(); // 到底不动
        assert_eq!(app.selected, 2);
        handle_key(&mut app, &key('k')).unwrap();
        assert_eq!(app.selected, 1);
        handle_key(&mut app, &key('G')).unwrap();
        assert_eq!(app.selected, 2);
        handle_key(&mut app, &key('g')).unwrap();
        assert_eq!(app.selected, 0);
        // 输入非空时 j 就是普通字符
        handle_key(&mut app, &key('x')).unwrap();
        handle_key(&mut app, &key('j')).unwrap();
        assert_eq!(app.input, "xj");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn half_page_jumps() {
        let dir = std::env::temp_dir().join(format!("dictate-half-{}", std::process::id()));
        let mut app = app_with_entries(dir.to_str().unwrap());
        handle_key(&mut app, &ctrl_key('d')).unwrap();
        assert_eq!(app.selected, 2, "Ctrl+D 翻半页（到列表底）");
        handle_key(&mut app, &ctrl_key('u')).unwrap();
        assert_eq!(app.selected, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn typing_slash_opens_command_palette_state() {
        let dir = std::env::temp_dir().join(format!("dictate-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App::new(
            History::new(std::path::Path::new(&dir).join("history.jsonl")),
            Theme::dark(),
        );
        handle_key(&mut app, &key('/')).unwrap();
        assert_eq!(app.input, "/");
        handle_key(&mut app, &key('s')).unwrap();
        assert_eq!(app.input, "/s");
        // 过滤后前缀命中排前，其余模糊命中按声明顺序跟在后面
        let hits = palette_hits("/s");
        let names: Vec<_> = hits.iter().map(|(i, _)| COMMANDS[*i].0).collect();
        assert_eq!(names[..2], ["/settings", "/search"]);
        assert!(names.contains(&"/base") && names.contains(&"/refresh"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enter_runs_settings_command() {
        let dir = std::env::temp_dir().join(format!("dictate-cli2-{}", std::process::id()));
        let mut app = app_with_entries(dir.to_str().unwrap());
        for c in "/settings".chars() {
            handle_key(&mut app, &key(c)).unwrap();
        }
        handle_key(
            &mut app,
            &crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        assert!(app.settings.is_some(), "面板回车应该打开设置页");
        assert!(app.input.is_empty(), "提交后输入行清空");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn palette_enter_with_arg_command_fills_name() {
        let dir = std::env::temp_dir().join(format!("dictate-pal-{}", std::process::id()));
        let mut app = app_with_entries(dir.to_str().unwrap());
        for c in "/hotkey".chars() {
            handle_key(&mut app, &key(c)).unwrap();
        }
        handle_key(
            &mut app,
            &crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        // /hotkey 带参数：回车只把名字和空格填进去，等用户继续敲
        assert_eq!(app.input, "/hotkey ");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn esc_clears_input_then_quits() {
        let dir = std::env::temp_dir().join(format!("dictate-cli3-{}", std::process::id()));
        let mut app = app_with_entries(dir.to_str().unwrap());
        for c in "abc".chars() {
            handle_key(&mut app, &key(c)).unwrap();
        }
        let esc = crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            handle_key(&mut app, &esc).unwrap(),
            Outcome::Continue
        ));
        assert!(app.input.is_empty());
        assert!(matches!(handle_key(&mut app, &esc).unwrap(), Outcome::Quit));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn newest_entry_is_first_and_selected() {
        let dir = std::env::temp_dir().join(format!("dictate-order-{}", std::process::id()));
        let app = app_with_entries(dir.to_str().unwrap());
        assert_eq!(app.shown.len(), 3);
        assert_eq!(app.selected, 0);
        assert_eq!(app.all[app.shown[0]].id, "new", "最上面应该是最新一条");
        assert_eq!(app.all[app.shown[2]].id, "old");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn status_file_maps_to_phases() {
        // 直接构造状态文件，验证相位映射（attach 模式的核心）
        let dir = std::env::temp_dir().join(format!("dictate-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("status.json");

        std::fs::write(&path, r#"{"phase":"recording","since_ms":0}"#).unwrap();
        let mut app = App::new(History::new(dir.join("history.jsonl")), Theme::dark());
        apply_status_from(&path, &mut app);
        assert!(matches!(app.phase, Phase::Recording(_)));

        std::fs::write(&path, r#"{"phase":"idle","since_ms":0,"text":"你好"}"#).unwrap();
        apply_status_from(&path, &mut app);
        assert_eq!(app.phase, Phase::Idle);
        assert!(app.daemon_note.contains("你好"));

        std::fs::write(&path, r#"{"phase":"idle","since_ms":0,"error":"HTTP 401"}"#).unwrap();
        apply_status_from(&path, &mut app);
        assert!(app.daemon_note.contains("401"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn toast_does_not_die_immediately() {
        let dir = std::env::temp_dir().join(format!("dictate-toast-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App::new(
            History::new(std::path::Path::new(&dir).join("history.jsonl")),
            Theme::dark(),
        );
        toast(&mut app, "已复制 12 字");
        // 状态轮询不再冲掉命令提示（旧实现里 apply_status 每次都覆写 status）
        app.daemon_note = String::new();
        assert!(app.toast.as_ref().unwrap().at.elapsed().as_secs() < 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── 离屏渲染：不用真终端也能看布局 ──

    fn render(app: &App, input: &str, w: u16, h: u16) -> String {
        let a = App {
            history: History::new(std::path::Path::new("/nonexistent")),
            all: app.all.clone(),
            shown: app.shown.clone(),
            query: app.query.clone(),
            selected: app.selected,
            daemon_note: app.daemon_note.clone(),
            phase: app.phase,
            subtitle: app.subtitle.clone(),
            theme: app.theme,
            overlay: app.overlay,
            settings: None,
            input: input.to_string(),
            palette_sel: app.palette_sel,
            toast: app.toast.clone(),
            quit_requested: false,
        };
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, &a)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn demo_app(today: chrono::NaiveDate, tag: &str) -> App {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("dictate-render-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let y = today.pred_opt().unwrap();
        let long = "今天下午三点跟产品过一下这个需求，顺便把接口文档同步给后端，会议纪要发给所有相关同学，别漏了测试。";
        for (id, ts, dur, lat, status, text) in [
            (
                "a",
                format!("{y}T09:15:00+08:00"),
                3200,
                890,
                "ok",
                "昨天的记录：把 unwrap 换成 anyhow 的 context",
            ),
            (
                "b",
                format!("{today}T10:00:00+08:00"),
                4200,
                910,
                "ok",
                long,
            ),
            (
                "c",
                format!("{today}T10:05:00+08:00"),
                1200,
                450,
                "ok",
                "短的一条",
            ),
            (
                "d",
                format!("{today}T10:06:00+08:00"),
                1500,
                0,
                "error: HTTP 401 Unauthorized：key 不对",
                "",
            ),
        ] {
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "id": id, "ts": ts, "duration_ms": dur, "latency_ms": lat,
                    "model": "qwen3-asr-flash", "status": status, "text": text
                })
            )
            .unwrap();
        }
        let mut app = App::new(History::new(&path), Theme::dark());
        app.subtitle = "ctrl+space · qwen3-asr-flash".into();
        std::fs::remove_dir_all(&dir).ok();
        app
    }

    #[test]
    fn render_main_view_layout() {
        let today = chrono::Local::now().date_naive();
        let app = demo_app(today, "main");
        let scr = render(&app, "", 100, 28);
        println!("{scr}");
        // TestBackend 里宽字符的续格是空格，断言用去空格版本比对中文
        let compact: String = scr.chars().filter(|c| !c.is_whitespace()).collect();
        // 按天分组
        assert!(compact.contains("今天"), "应有「今天」分组头");
        assert!(compact.contains("昨天"), "应有「昨天」分组头");
        // 长文本折行：折出的行要按前缀宽度缩进（行首是空格、内容里没有时间戳）
        let wrapped = scr
            .lines()
            .find(|l| l.replace(' ', "").trim_start().starts_with("别漏了测试"))
            .expect("长文本应该折出第二行");
        assert!(wrapped.starts_with("           "), "折行要按前缀宽度缩进");
        assert!(!wrapped.trim().starts_with("10:0"), "折行不是新条目");
        // 时长 → 延迟
        assert!(compact.contains("4.2s→0.9s"), "应显示 时长→延迟");
        // 错误条目可见
        assert!(compact.contains("HTTP401"));
        // 占位符
        assert!(compact.contains("输入过滤历史"));
        let lines: Vec<&str> = scr.lines().collect();
        assert!(
            lines[26].replace(' ', "").starts_with("❯"),
            "倒数第二行应是输入行"
        );
    }

    #[test]
    fn render_command_palette_and_help() {
        let today = chrono::Local::now().date_naive();
        let app = demo_app(today, "pal");
        let pal = render(&app, "/se", 100, 28);
        println!("{pal}");
        let compact: String = pal.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("╭命令"), "面板应有圆角边框和标题");
        assert!(compact.contains("/settings"), "面板里应有 /settings");
        assert!(compact.contains("/search"), "面板里应有 /search");
        assert!(compact.contains("↑↓选择"), "面板提示操作键");

        let mut a = demo_app(today, "help");
        a.overlay = Some(Overlay::Help);
        let help = render(&a, "", 100, 28);
        let hc: String = help.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(hc.contains("╭帮助"), "帮助应是圆角对话框");
        assert!(hc.contains("空格或/record"), "帮助应写真实可用的键");
    }
}
