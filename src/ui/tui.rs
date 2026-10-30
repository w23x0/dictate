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
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::Terminal;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::config::Config;
use crate::core::history::{Entry, History};
use crate::ui::history_list::{HistoryList, GUTTER, ROW_PREFIX};
use crate::ui::theme::Theme;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// 守护进程心跳超过这个毫秒数没有刷新，就当它死了（状态文件还留着 = 崩溃残留）。
const DAEMON_STALE_MS: u64 = 6_000;
/// 命令/参数面板一次最多显示几行。
const PALETTE_ROWS: usize = 8;

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
    /// 心跳新鲜——false 时录音/转写命令直接拒绝，不装作「已开始」
    daemon_alive: bool,
    /// 流式中间文本（录音中实时刷新）
    partial: String,
    phase: Phase,
    /// 顶栏副标题（热键 · 模型），只在 run 模式有
    subtitle: String,
    theme: Theme,
    overlay: Option<Overlay>,
    /// 内嵌的设置页（`/settings` 进入，Esc 退出）
    settings: Option<crate::ui::settings_ui::Form>,
    /// 底部命令行（grok-cli 的位置），`/` 开头是命令
    input: String,
    /// `input` 的字节光标位置（永远落在字符边界）
    input_cursor: usize,
    /// 命令面板里选中的条目
    palette_sel: usize,
    /// 标记的历史条目 id（`m` 切换；⏎ 时一起复制）
    marks: std::collections::HashSet<String>,
    /// 标记变更计数：签名用它，避免「标记数没变但对象换了」导致 ✓ 残留
    marks_rev: u64,
    toast: Option<Toast>,
    quit_requested: bool,
    // ── 列表渲染缓存：条目文本折行按 (id, 宽度) 记住，列表行只在签名变化时重建，
    //    每帧只画视口内的行（history_list），100 ms 一帧不再对全部历史做 O(n) 分配
    wrap_cache: std::collections::HashMap<(String, usize), Vec<String>>,
    list_items: Vec<Line<'static>>,
    list_first_of: Vec<usize>,
    list_sig: String,
    list_at: Instant,
    /// 列表视口第一行的行号（选中项会被自动滚进视口）
    list_scroll: usize,
}

/// 斜杠命令：名字、参数提示、说明
const COMMANDS: &[(&str, &str, &str)] = &[
    ("/settings", "", "打开设置"),
    ("/model", "[名字]", "切换模型（不带参数则列表选）"),
    ("/base", "[url]", "切换服务地址（不带参数则列表选）"),
    ("/key", "[sk-…]", "设置 API key"),
    ("/theme", "[auto|dark|light]", "配色（不带参数则循环切换）"),
    ("/hotkey", "[组合键]", "设置热键，如 ctrl+space"),
    ("/mode", "hold|toggle", "热键语义：按住说话 / 按一下开关"),
    ("/record", "", "手动开始或停止录音"),
    ("/retry", "", "重转上一次的音频（设置里开「保留音频」）"),
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
            daemon_alive: false,
            partial: String::new(),
            phase: Phase::Idle,
            subtitle: String::new(),
            theme,
            overlay: None,
            settings: None,
            input: String::new(),
            input_cursor: 0,
            palette_sel: 0,
            marks: std::collections::HashSet::new(),
            marks_rev: 0,
            toast: None,
            quit_requested: false,
            wrap_cache: std::collections::HashMap::new(),
            list_items: Vec::new(),
            list_first_of: Vec::new(),
            list_sig: String::new(),
            list_at: Instant::now(),
            list_scroll: 0,
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
        // 倒序：**最新的一条在最上面**，选中项默认就是它。
        // 过滤同时看 text 与 status——错误条目 text 为空，只匹配 text 会把
        // 刚发生的失败从筛选视图里藏掉
        self.shown = self
            .all
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, e)| {
                q.is_empty()
                    || e.text.to_lowercase().contains(&q)
                    || e.status.to_lowercase().contains(&q)
            })
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

// ───────────────────────── 输入行编辑 ─────────────────────────
//
// 借 opentui 的 EditBuffer / rune 的 TextEdit：光标不是永远在末尾，
// ←/→/Home/End/Delete/Ctrl+W 都能用，筛选和命令输入共用一套。

fn prev_boundary(s: &str, mut i: usize) -> usize {
    if i == 0 {
        return 0;
    }
    i -= 1;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    i += 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 输入变化后同步筛选状态：`/` 开头是命令，不拿去做历史过滤。
fn input_sync(app: &mut App) {
    app.palette_sel = 0;
    if app.input.starts_with('/') {
        app.query.clear();
    } else {
        app.query = app.input.clone();
    }
    app.refilter();
}

/// 整体替换输入（命令补全、清空等），光标放到末尾。
fn set_input(app: &mut App, s: impl Into<String>) {
    app.input = s.into();
    app.input_cursor = app.input.len();
    input_sync(app);
}

fn input_insert(app: &mut App, c: char) {
    let at = app.input_cursor.min(app.input.len());
    app.input.insert(at, c);
    app.input_cursor = at + c.len_utf8();
    input_sync(app);
}

fn input_backspace(app: &mut App) {
    if app.input_cursor == 0 {
        return;
    }
    let from = prev_boundary(&app.input, app.input_cursor.min(app.input.len()));
    let to = app.input_cursor.min(app.input.len());
    app.input.replace_range(from..to, "");
    app.input_cursor = from;
    input_sync(app);
}

fn input_delete(app: &mut App) {
    let at = app.input_cursor.min(app.input.len());
    if at >= app.input.len() {
        return;
    }
    let to = next_boundary(&app.input, at);
    app.input.replace_range(at..to, "");
    input_sync(app);
}

fn input_left(app: &mut App) {
    app.input_cursor = prev_boundary(&app.input, app.input_cursor.min(app.input.len()));
}

fn input_right(app: &mut App) {
    app.input_cursor = next_boundary(&app.input, app.input_cursor.min(app.input.len()));
}

fn input_home(app: &mut App) {
    app.input_cursor = 0;
}

fn input_end(app: &mut App) {
    app.input_cursor = app.input.len();
}

/// Ctrl+W：删光标前一个词（先吞掉尾随空格，再删到空白）。
fn input_kill_word(app: &mut App) {
    let at = app.input_cursor.min(app.input.len());
    let mut start = at;
    while start > 0 {
        let prev = prev_boundary(&app.input, start);
        if app.input[prev..start].chars().all(char::is_whitespace) {
            start = prev;
        } else {
            break;
        }
    }
    while start > 0 {
        let prev = prev_boundary(&app.input, start);
        if app.input[prev..start].chars().all(char::is_whitespace) {
            break;
        }
        start = prev;
    }
    if start != at {
        app.input.replace_range(start..at, "");
        app.input_cursor = start;
        input_sync(app);
    }
}

fn input_clear(app: &mut App) {
    app.input.clear();
    app.input_cursor = 0;
    input_sync(app);
}

/// 光标前文本的显示宽度，用于把终端光标放到正确位置。
fn display_width_before(s: &str, cursor: usize) -> usize {
    UnicodeWidthStr::width(&s[..cursor.min(s.len())])
}

/// 输入行实际画出来的文本 + 光标列（`/key` 的密钥部分画成圆点）。
fn input_display(app: &App) -> (String, usize) {
    if let Some(secret) = app.input.strip_prefix("/key ") {
        let cursor = app.input_cursor.min(app.input.len());
        let before = if cursor <= 5 {
            cursor
        } else {
            5 + app.input[5..cursor].chars().count()
        };
        (
            format!("/key {}", "•".repeat(secret.chars().count())),
            before,
        )
    } else {
        (
            app.input.clone(),
            display_width_before(&app.input, app.input_cursor),
        )
    }
}

/// 输入比一行宽时，把左边滚出去，保证光标始终可见。
fn fit_input(s: &str, cursor: usize, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    let cursor = cursor.min(s.len());
    let before = UnicodeWidthStr::width(&s[..cursor]);
    if before < width {
        return (s.to_string(), before);
    }
    let mut start = cursor;
    let mut w = 0usize;
    while start > 0 {
        let prev = prev_boundary(s, start);
        let cw = UnicodeWidthStr::width(&s[prev..start]);
        if w + cw >= width {
            break;
        }
        w += cw;
        start = prev;
    }
    (s[start..].to_string(), w)
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

/// 面板里一次显示 `rows` 行时，视口第一行的下标：选中项尽量居中，
/// 但不能越过末尾（否则选到第 9 条以后就看不见了）。
fn window_start(sel: usize, len: usize, rows: usize) -> usize {
    if len <= rows || rows == 0 {
        0
    } else {
        sel.saturating_sub(rows / 2).min(len - rows)
    }
}

/// 已带参数的命令行（"/model xxx"）底下那行提示用。
fn cmd_hint(line: &str) -> Option<(&'static str, &'static str, &'static str)> {
    let first = line.split(' ').next().unwrap_or("");
    COMMANDS.iter().find(|(n, _, _)| *n == first).copied()
}

// ───────────────────── /model、/base 的参数面板 ─────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgKind {
    Model,
    Base,
}

impl ArgKind {
    fn items(self) -> &'static [(&'static str, &'static str)] {
        match self {
            ArgKind::Model => crate::ui::settings_ui::MODELS,
            ArgKind::Base => crate::ui::settings_ui::BASES,
        }
    }
    fn title(self) -> &'static str {
        match self {
            ArgKind::Model => " 模型 ",
            ArgKind::Base => " 服务地址 ",
        }
    }
    fn cmd(self) -> &'static str {
        match self {
            ArgKind::Model => "/model",
            ArgKind::Base => "/base",
        }
    }
}

/// `/model `、`/base ` 敲了空格后弹参数列表（不用再进设置页挑）。
/// 过滤是名字上的不区分大小写子串；返回命中下标（保持声明顺序）。
fn arg_hits(input: &str) -> Option<(ArgKind, Vec<usize>)> {
    let (kind, rest) = if let Some(r) = input.strip_prefix("/model ") {
        (ArgKind::Model, r)
    } else {
        let r = input.strip_prefix("/base ")?;
        (ArgKind::Base, r)
    };
    let q = rest.trim().to_lowercase();
    let hits: Vec<usize> = kind
        .items()
        .iter()
        .enumerate()
        .filter(|(_, (n, _))| q.is_empty() || n.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect();
    Some((kind, hits))
}

/// 参数面板里选定一项：写配置 + toast。
fn apply_arg(app: &mut App, kind: ArgKind, name: &str) {
    let Ok(mut cfg) = crate::core::config::load() else {
        toast_err(app, "读配置失败");
        return;
    };
    match kind {
        ArgKind::Model => {
            cfg.model = name.to_string();
            cfg.api_style = None;
            if let Err(e) = crate::core::config::write(&crate::core::config::config_path(), &cfg) {
                toast_err(app, format!("保存失败：{e}"));
                return;
            }
            toast(app, format!("模型 → {name}（下次录音生效）"));
        }
        ArgKind::Base => match crate::asr::normalize_base(name) {
            Ok(b) => {
                cfg.base_url = b;
                if let Err(e) =
                    crate::core::config::write(&crate::core::config::config_path(), &cfg)
                {
                    toast_err(app, format!("保存失败：{e}"));
                    return;
                }
                toast(app, format!("服务地址 → {}", cfg.base_url));
            }
            Err(e) => toast_err(app, format!("地址不合法：{e}")),
        },
    }
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

/// 最近失败的处理建议：错误不同，下一步该做的事也不同
///（延续「服务端错误翻人话」的思路，把「怎么办」也带上）。
fn error_hint(note: &str) -> &'static str {
    let n = note.to_lowercase();
    if n.contains("没在跑") || n.contains("没在响应") {
        "先把守护进程跑起来：dictate run（终端视图），或直接运行 dictate"
    } else if n.contains("保留的音频") || n.contains("keep_audio") {
        "设置页（F2）把「保留音频」打开，下次录音后 /retry 就能用"
    } else if n.contains("401") || n.contains("403") || n.contains("key") {
        "/key 换个 key（或 F2 设置里改），改完说一句试试"
    } else if n.contains("429") {
        "限流了，等一会儿再试"
    } else if n.contains("语音") || n.contains("门限") || n.contains("电平") {
        "麦克风没信号：终端跑 dictate probe --seconds 3 看电平"
    } else if n.contains("超时") || n.contains("timeout") {
        "网络不通或太慢：/base 换个区域，或稍后再试"
    } else {
        "跑 dictate doctor 逐项排查（key / 区域 / 热键 / 粘贴）"
    }
}

/// 历史条目按天分组的小标题。今天/昨天按日历日期算（来自 ts_ms，不依赖追加序），
/// 跨年的旧记录带上年份，否则「10-01」分不清是哪一年。
fn day_label_at(
    d: chrono::NaiveDate,
    today: chrono::NaiveDate,
    yesterday: chrono::NaiveDate,
) -> String {
    use chrono::Datelike as _;
    if d == today {
        "今天".into()
    } else if d == yesterday {
        "昨天".into()
    } else if d.year() != today.year() {
        d.format("%Y-%m-%d").to_string()
    } else {
        d.format("%m-%d").to_string()
    }
}

/// 条目的日历日期：优先 ts_ms（load 时已回填并排序），旧格式坏行退回解析 ts 字符串。
fn entry_date(e: &Entry) -> Option<chrono::NaiveDate> {
    use chrono::TimeZone as _;
    if e.ts_ms > 0 {
        return chrono::Local
            .timestamp_millis_opt(e.ts_ms as i64)
            .single()
            .map(|dt| dt.date_naive());
    }
    chrono::DateTime::parse_from_rfc3339(&e.ts)
        .ok()
        .map(|dt| dt.date_naive())
}

/// OSC 52：把文本放进终端所在机器的剪贴板（SSH 里也有效）。
pub fn osc52(text: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    wrap_for_tmux(
        &format!("\x1b]52;c;{b64}\x07"),
        std::env::var_os("TMUX").is_some(),
    )
}

/// tmux 会吞掉应用的裸 OSC 序列；在 tmux 里用 DCS passthrough 包一层，
/// 内部的 ESC 双写。（tmux ≥3.3 要 allow-passthrough on；不满足时静默失效，
/// 不比不包差。）
fn wrap_for_tmux(seq: &str, in_tmux: bool) -> String {
    if !in_tmux {
        return seq.to_string();
    }
    format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
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
    /// 非失败信息（按键太短、流式回退批量等），暗色显示、不带建议
    #[serde(default)]
    note: Option<String>,
    /// 心跳（unix ms）。守护进程每次内容变化或每 3 秒刷新一次；
    /// 超时未刷新且文件还在 = 进程崩了，状态是残留的
    #[serde(default)]
    hb_ms: u64,
    /// 流式中间文本（录音中才有）
    #[serde(default)]
    partial: Option<String>,
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
/// 返回 true 表示可见状态变了，调用方需要重画。
fn apply_status_data(present: bool, st: Option<&StatusFile>, app: &mut App) -> bool {
    let now = crate::core::history::now_ms();
    let phase_key = |p: Phase| match p {
        Phase::Idle => 0u8,
        Phase::Recording(_) => 1,
        Phase::Transcribing(_) => 2,
    };
    let old = (
        app.daemon_alive,
        phase_key(app.phase),
        app.daemon_note.clone(),
        app.partial.clone(),
    );

    if !present {
        app.phase = Phase::Idle;
        app.daemon_alive = false;
        app.partial.clear();
        app.daemon_note = "守护进程没在跑 —— 先执行 dictate".into();
    } else if let Some(st) = st {
        // 心跳过期 = 进程崩了但状态文件残留：不再照着残留文件演「就绪/录音中」
        if st.hb_ms > 0 && now.saturating_sub(st.hb_ms) > DAEMON_STALE_MS {
            app.phase = Phase::Idle;
            app.daemon_alive = false;
            app.partial.clear();
            app.daemon_note = "守护进程没在响应 —— 重启：dictate run".into();
        } else {
            app.daemon_alive = true;
            let elapsed = Duration::from_millis(now.saturating_sub(st.since_ms));
            let at = Instant::now()
                .checked_sub(elapsed)
                .unwrap_or_else(Instant::now);
            app.phase = match st.phase.as_str() {
                "recording" => Phase::Recording(at),
                "transcribing" => Phase::Transcribing(at),
                _ => Phase::Idle,
            };
            app.partial = st.partial.clone().unwrap_or_default();
            app.daemon_note = match (&st.error, &st.note, &st.text) {
                (Some(e), _, _) => format!("失败：{e}"),
                (None, Some(n), _) => n.clone(),
                (None, None, Some(t)) => {
                    format!("完成：{}", t.chars().take(40).collect::<String>())
                }
                (None, None, None) => String::new(),
            };
        }
    }

    old != (
        app.daemon_alive,
        phase_key(app.phase),
        app.daemon_note.clone(),
        app.partial.clone(),
    )
}

#[cfg(test)]
fn apply_status_from(path: &std::path::Path, app: &mut App) {
    let body = std::fs::read_to_string(path).ok();
    let st = body
        .as_deref()
        .and_then(|b| serde_json::from_str::<StatusFile>(b).ok());
    let present = path.exists();
    apply_status_data(present, st.as_ref(), app);
}

fn file_stamp(path: &std::path::Path) -> (u64, u64) {
    std::fs::metadata(path)
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
        .unwrap_or((0, 0))
}

/// 状态文件只在 mtime/大小变化时重新读+解析；心跳过期仍每轮用缓存里的 hb_ms 判断。
#[derive(Default)]
struct StatusCache {
    loaded: bool,
    present: bool,
    stamp: (u64, u64),
    parsed: Option<StatusFile>,
}

impl StatusCache {
    fn refresh(&mut self, path: &std::path::Path) -> bool {
        let present = path.exists();
        let stamp = file_stamp(path);
        if !self.loaded || stamp != self.stamp || present != self.present {
            self.loaded = true;
            self.present = present;
            self.stamp = stamp;
            self.parsed = if present {
                std::fs::read_to_string(path)
                    .ok()
                    .and_then(|b| serde_json::from_str(&b).ok())
            } else {
                None
            };
        }
        self.present
    }
}

fn toast_ttl(t: &Toast) -> Duration {
    Duration::from_secs(if t.err { 8 } else { 5 })
}

/// 附着模式：只画，不抢热键/麦克风。
///
/// 借 rune 的「idle 不画」：没有状态变化、没有动画时不做 draw；
/// 录音/转写才按 80 ms 出帧，普通空闲 250 ms 只 stat 一下文件。
fn attach_loop(term: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    let mut hist_stamp = (0u64, 0u64);
    let mut status_cache = StatusCache::default();
    let mut dirty = true;
    let mut prev_toast = false;
    loop {
        let mut changed = false;
        if let Some(form) = app.settings.as_mut() {
            if form.poll_test() {
                changed = true;
            }
        }
        let present = status_cache.refresh(&crate::core::config::status_path());
        if apply_status_data(present, status_cache.parsed.as_ref(), app) {
            changed = true;
        }
        let now = file_stamp(&crate::core::config::history_path());
        if now != hist_stamp {
            hist_stamp = now;
            app.reload_keep_position();
            changed = true;
        }
        if changed {
            dirty = true;
        }

        let animating = matches!(app.phase, Phase::Recording(_) | Phase::Transcribing(_));
        let toast_alive = app
            .toast
            .as_ref()
            .map(|t| t.at.elapsed() < toast_ttl(t))
            .unwrap_or(false);
        if toast_alive != prev_toast {
            prev_toast = toast_alive;
            dirty = true;
        }
        if dirty || animating {
            term.draw(|f| draw(f, app))?;
            dirty = false;
        }

        let testing = app.settings.as_ref().map(|f| f.testing()).unwrap_or(false);
        let timeout = if animating {
            Duration::from_millis(80)
        } else if toast_alive || testing {
            Duration::from_millis(150)
        } else {
            Duration::from_millis(250)
        };
        if !event::poll(timeout)? {
            continue;
        }
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                dirty = true;
                match handle_key(app, &k)? {
                    Outcome::Continue => {}
                    Outcome::Quit => return Ok(()),
                }
            }
            Event::Resize(_, _) => dirty = true,
            _ => {}
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
    // 守护进程不在就直说，别 toast「开始录音…」装作成功——控制文件写了也没人读
    if !app.daemon_alive {
        toast_err(
            app,
            "守护进程没在跑——先执行 dictate run（或直接运行 dictate）",
        );
        return;
    }
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

/// 打开设置页。配置读不出来（比如手改 TOML 写坏了语法）只报错，不炸掉整个视图。
fn open_settings(app: &mut App) {
    match crate::core::config::load() {
        Ok(cfg) => {
            let mut form = crate::ui::settings_ui::Form::new(cfg);
            form.theme = app.theme;
            app.settings = Some(form);
        }
        Err(e) => toast_err(app, format!("打开设置失败：{e:#}")),
    }
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
                    match app.history.clear() {
                        Ok(()) => {
                            toast(app, "已清空");
                            app.marks.clear();
                            app.overlay = None;
                        }
                        // 一次 IO 错误不该把整个 TUI 炸回 shell
                        Err(e) => {
                            toast_err(app, format!("清空失败：{e:#}"));
                            app.overlay = None;
                        }
                    }
                    app.reload();
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
    // /model、/base 的参数面板（有命中才算激活）
    let arg = arg_hits(&app.input);
    let arg_active = matches!(&arg, Some((_, h)) if !h.is_empty());
    let arg_sel = app.palette_sel.min(
        arg.as_ref()
            .map(|(_, h)| h.len())
            .unwrap_or(1)
            .saturating_sub(1),
    );

    // Ctrl 系列与功能键
    match k.code {
        KeyCode::Char('c') if ctrl => return Ok(Outcome::Quit),
        KeyCode::Char('a') if ctrl => {
            input_home(app);
            return Ok(Outcome::Continue);
        }
        KeyCode::Char('e') if ctrl => {
            input_end(app);
            return Ok(Outcome::Continue);
        }
        KeyCode::Char('w') if ctrl => {
            input_kill_word(app);
            return Ok(Outcome::Continue);
        }
        KeyCode::Char('u') if ctrl && !app.input.is_empty() => {
            input_clear(app);
            return Ok(Outcome::Continue);
        }
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
                input_clear(app);
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
            open_settings(app);
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
            input_clear(app);
        }
        KeyCode::Enter => {
            if arg_active {
                let (kind, h) = arg.unwrap();
                let name = kind.items()[h[arg_sel]].0.to_string();
                apply_arg(app, kind, &name);
                input_clear(app);
            } else if palette {
                // 面板里回车：无参命令直接执行，带参命令把名字填进去继续敲参数
                let (ci, _) = hits[sel];
                let (name, arg, _) = COMMANDS[ci];
                if arg.is_empty() {
                    set_input(app, name);
                    run_input(app)?;
                    if app.quit_requested {
                        return Ok(Outcome::Quit);
                    }
                } else {
                    set_input(app, format!("{name} "));
                }
            } else {
                run_input(app)?;
                if app.quit_requested {
                    return Ok(Outcome::Quit);
                }
            }
        }
        KeyCode::Backspace => input_backspace(app),
        KeyCode::Left => input_left(app),
        KeyCode::Right => input_right(app),
        KeyCode::Up => {
            if arg_active {
                app.palette_sel = arg_sel.saturating_sub(1);
            } else if palette {
                app.palette_sel = sel.saturating_sub(1);
            } else {
                app.selected = app.selected.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if arg_active {
                let n = arg.as_ref().unwrap().1.len();
                app.palette_sel = (arg_sel + 1).min(n - 1);
            } else if palette {
                app.palette_sel = (sel + 1).min(hits.len() - 1);
            } else if app.selected + 1 < app.shown.len() {
                app.selected += 1;
            }
        }
        KeyCode::PageUp => app.selected = app.selected.saturating_sub(10),
        KeyCode::PageDown => {
            app.selected = (app.selected + 10).min(app.shown.len().saturating_sub(1));
        }
        // 列表是倒序的：Home/g = 最新，End/G = 最旧；输入非空时先服务光标
        KeyCode::Home if !app.input.is_empty() => input_home(app),
        KeyCode::End if !app.input.is_empty() => input_end(app),
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = app.shown.len().saturating_sub(1),
        KeyCode::Tab if arg_active => {
            let (kind, h) = arg.as_ref().unwrap();
            let name = kind.items()[h[arg_sel]].0;
            set_input(app, format!("{} {name}", kind.cmd()));
        }
        KeyCode::Tab if palette => {
            let (ci, _) = hits[sel];
            let (name, arg, _) = COMMANDS[ci];
            set_input(
                app,
                if arg.is_empty() {
                    name.to_string()
                } else {
                    format!("{name} ")
                },
            );
        }
        KeyCode::Delete if !app.input.is_empty() => input_delete(app),
        KeyCode::Delete => {
            if let Some(e) = app.current() {
                let id = e.id.clone();
                match app.history.delete(&id) {
                    Ok(()) => toast(app, "已删除一条"),
                    Err(err) => toast_err(app, format!("删除失败：{err:#}")),
                }
                app.reload();
            }
        }
        // 输入为空时这些键是导航/操作，不进输入框（想按文字过滤就用别的字符起头）
        KeyCode::Char(' ') if app.input.is_empty() => toggle_record(app),
        KeyCode::Char('m') if app.input.is_empty() => {
            if let Some(e) = app.current() {
                let id = e.id.clone();
                if !app.marks.remove(&id) {
                    app.marks.insert(id);
                }
                app.marks_rev = app.marks_rev.wrapping_add(1);
            }
        }
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
        KeyCode::Char(c) if !ctrl => input_insert(app, c),
        _ => {}
    }
    Ok(Outcome::Continue)
}

/// 回车：`/` 开头当命令，否则当作过滤词（输入时已经实时过滤）。
fn run_input(app: &mut App) -> Result<()> {
    let line = app.input.trim().to_string();
    if line.is_empty() {
        // 有标记就整批复制（⏎ = 把标记的条目拼起来送 OSC 52）
        let marked: Vec<String> = app
            .shown
            .iter()
            .map(|i| &app.all[*i])
            .filter(|e| app.marks.contains(&e.id))
            .map(|e| e.text.clone())
            .collect();
        if !marked.is_empty() {
            let joined = marked.join("\n");
            print!("{}", osc52(&joined));
            toast(
                app,
                format!(
                    "已复制 {} 条（共 {} 字）",
                    marked.len(),
                    joined.chars().count()
                ),
            );
            return Ok(());
        }
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
        app.input_cursor = 0;
        return Ok(());
    }
    // 命令不参与过滤
    app.query.clear();

    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim().to_string();
    // 命令里的配置读写一律「失败 toast、视图活着」——手改 TOML 写坏后按个键
    // 不该把整个终端视图炸回 shell
    let save_cfg =
        |app: &mut App, cfg: &crate::core::config::Config| match crate::core::config::write(
            &crate::core::config::config_path(),
            cfg,
        ) {
            Ok(()) => true,
            Err(e) => {
                toast_err(app, format!("保存失败：{e:#}"));
                false
            }
        };
    match cmd {
        "/help" => app.overlay = Some(Overlay::Help),
        "/settings" => open_settings(app),
        "/model" => {
            if arg.is_empty() {
                match crate::core::config::load() {
                    Ok(cfg) => {
                        let mut form = crate::ui::settings_ui::Form::new(cfg);
                        form.theme = app.theme;
                        form.open_model_picker();
                        app.settings = Some(form);
                    }
                    Err(e) => toast_err(app, format!("读配置失败：{e:#}")),
                }
            } else {
                match crate::core::config::load() {
                    Ok(mut cfg) => {
                        if arg.trim().is_empty() {
                            toast_err(app, "模型名不能为空");
                        } else {
                            cfg.model = arg.clone();
                            cfg.api_style = None;
                            if save_cfg(app, &cfg) {
                                toast(app, format!("模型 → {arg}（下次录音生效）"));
                            }
                        }
                    }
                    Err(e) => toast_err(app, format!("读配置失败：{e:#}")),
                }
            }
        }
        "/base" => {
            if arg.is_empty() {
                match crate::core::config::load() {
                    Ok(cfg) => {
                        let mut form = crate::ui::settings_ui::Form::new(cfg);
                        form.theme = app.theme;
                        form.open_base_picker();
                        app.settings = Some(form);
                    }
                    Err(e) => toast_err(app, format!("读配置失败：{e:#}")),
                }
            } else {
                match crate::core::config::load() {
                    Ok(mut cfg) => match crate::asr::normalize_base(&arg) {
                        Ok(b) => {
                            cfg.base_url = b;
                            if save_cfg(app, &cfg) {
                                toast(app, format!("服务地址 → {}", cfg.base_url));
                            }
                        }
                        Err(e) => toast_err(app, format!("地址不合法：{e}")),
                    },
                    Err(e) => toast_err(app, format!("读配置失败：{e:#}")),
                }
            }
        }
        "/key" => {
            if arg.is_empty() {
                toast(app, "用法：/key sk-…");
            } else {
                match crate::core::config::load() {
                    Ok(mut cfg) => {
                        cfg.api_key = Some(arg);
                        if save_cfg(app, &cfg) {
                            if std::env::var_os("DASHSCOPE_API_KEY")
                                .map(|v| !v.is_empty())
                                .unwrap_or(false)
                            {
                                toast(
                                    app,
                                    "已保存 key（但环境变量 DASHSCOPE_API_KEY 优先——unset 它才用这份）",
                                );
                            } else {
                                toast(app, "已保存 key（存在配置文件里，权限 600）");
                            }
                        }
                    }
                    Err(e) => toast_err(app, format!("读配置失败：{e:#}")),
                }
            }
        }
        "/theme" => {
            // 不带参数循环 auto → dark → light；带了就按参数（auto 会重新问终端背景色）
            let next = if arg.is_empty() {
                match crate::core::config::load()
                    .ok()
                    .and_then(|c| c.theme)
                    .as_deref()
                    .map(|s| s.trim().to_ascii_lowercase())
                {
                    Some(ref s) if s == "dark" => "light".to_string(),
                    Some(ref s) if s == "light" => "auto".to_string(),
                    _ => "dark".to_string(),
                }
            } else {
                match arg.to_ascii_lowercase().as_str() {
                    "auto" | "dark" | "light" => arg.to_ascii_lowercase(),
                    _ => {
                        toast_err(app, "用法：/theme auto|dark|light");
                        app.input.clear();
                        app.input_cursor = 0;
                        return Ok(());
                    }
                }
            };
            match crate::core::config::load() {
                Ok(mut cfg) => {
                    cfg.theme = if next == "auto" {
                        None
                    } else {
                        Some(next.clone())
                    };
                    if save_cfg(app, &cfg) {
                        app.theme = Theme::resolve(if next == "auto" { None } else { Some(&next) });
                        toast(app, format!("配色 → {next}"));
                    }
                }
                Err(e) => toast_err(app, format!("读配置失败：{e:#}")),
            }
        }
        "/hotkey" => match crate::hotkey::Spec::parse(&arg) {
            Ok(_) => match crate::core::config::load() {
                Ok(mut cfg) => {
                    cfg.hotkey = arg.clone();
                    if save_cfg(app, &cfg) {
                        toast(app, format!("热键 → {arg}（守护进程会自动热加载）"));
                    }
                }
                Err(e) => toast_err(app, format!("读配置失败：{e:#}")),
            },
            Err(e) => toast_err(app, format!("热键不合法：{e}")),
        },
        "/mode" => {
            let m = arg.to_ascii_lowercase();
            if m == "hold" || m == "toggle" {
                match crate::core::config::load() {
                    Ok(mut cfg) => {
                        cfg.hotkey_mode = m.clone();
                        if save_cfg(app, &cfg) {
                            toast(app, format!("热键模式 → {m}"));
                        }
                    }
                    Err(e) => toast_err(app, format!("读配置失败：{e:#}")),
                }
            } else {
                toast(app, "用法：/mode hold|toggle");
            }
        }
        "/record" => toggle_record(app),
        "/retry" => {
            send_control("retry");
            toast(app, "已请求重转（结果进历史并交付）");
        }
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
            app.input_cursor = 0;
            app.quit_requested = true;
        }
        other => toast_err(app, format!("没有这个命令：{other}（/help）")),
    }
    app.input.clear();
    app.input_cursor = 0;
    Ok(())
}

fn draw(f: &mut ratatui::Frame, app: &mut App) {
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

    // 界面不装样子：守护不在/上次失败都要从顶栏读出来，不能照旧亮绿灯「就绪」
    let (dot, mut dot_color, phase_text) = if !app.daemon_alive {
        ("○", th.faint, "无守护")
    } else {
        match app.phase {
            Phase::Idle if app.daemon_note.starts_with("失败") => ("●", th.rec, "上次失败"),
            Phase::Idle => ("●", th.ok, "就绪"),
            Phase::Recording(_) => ("●", th.rec, "录音中"),
            Phase::Transcribing(_) => ("◐", th.warn, "转写中"),
        }
    };
    // 录音时呼吸灯：500 ms 一闪
    if let Phase::Recording(t) = app.phase {
        if (t.elapsed().as_millis() / 500) % 2 == 1 {
            dot_color = th.faint;
        }
    }
    // 条数只在顶栏出现一处：平时显示总数，筛选时显示 可见/总数
    let count = if app.query.is_empty() {
        format!("{} 条 ", app.all.len())
    } else {
        format!("{}/{} 条 ", app.shown.len(), app.all.len())
    };

    // ── 顶栏：右侧状态固定宽度，左侧品牌区按剩余宽度收缩（窄终端不挤掉状态）
    let right = Line::from(vec![
        Span::styled(dot, Style::default().fg(dot_color)),
        Span::styled(
            format!(" {phase_text}   {count}"),
            Style::default().fg(th.dim),
        ),
    ])
    .alignment(ratatui::layout::Alignment::Right);
    let right_w = (right.width() as u16).min(chunks[0].width);
    let left_w = chunks[0].width.saturating_sub(right_w + 1);
    let mut left_spans = vec![
        Span::styled("◆ ", Style::default().fg(th.accent)),
        Span::styled("dictate", Style::default().add_modifier(Modifier::BOLD)),
    ];
    if left_w >= 22 {
        left_spans.push(Span::styled(
            format!("  v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(th.faint),
        ));
    }
    if left_w >= 44 && !app.subtitle.is_empty() {
        left_spans.push(Span::styled(
            format!("  {}", app.subtitle),
            Style::default().fg(th.dim),
        ));
    }
    let left = Line::from(left_spans);
    let wide = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_w), Constraint::Length(right_w)])
        .split(chunks[0]);
    f.render_widget(Paragraph::new(left), wide[0]);
    f.render_widget(Paragraph::new(right), wide[1]);

    // ── 第二行：失败（带下一步建议）> 筛选状态 > 守护进程最近结果。
    //    失败排最前：筛选期间发生的转写失败不能被「筛选 …」行遮掉
    let note_bad = app.daemon_note.starts_with("失败")
        || app.daemon_note.contains("没在跑")
        || app.daemon_note.contains("没在响应");
    if note_bad {
        // 失败不只报红字：带上「下一步怎么办」
        let hint = error_hint(&app.daemon_note);
        let hint_w = UnicodeWidthStr::width(hint) + 5;
        let budget = chunks[1].width.saturating_sub(hint_w as u16).max(10) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(trunc(&app.daemon_note, budget), Style::default().fg(th.rec)),
                Span::styled("  →  ", Style::default().fg(th.faint)),
                Span::styled(hint, Style::default().fg(th.dim)),
            ])),
            chunks[1],
        );
    } else if !app.query.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("筛选 ", Style::default().fg(th.faint)),
                Span::styled(
                    trunc(&app.query, 30),
                    Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  Esc 清除", Style::default().fg(th.faint)),
            ])),
            chunks[1],
        );
    } else if !app.daemon_note.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                trunc(&app.daemon_note, chunks[1].width.saturating_sub(2) as usize),
                Style::default().fg(th.dim),
            ))),
            chunks[1],
        );
    }

    // ── 历史列表：按天分组 + 自动折行。
    //    行内容只在签名变化或每满一分钟时重建——签名覆盖条数/标记版本/筛选词/配色/宽度；
    //    选中态由 history_list 在渲染时叠加，所以 j/k 不再重建整表，每帧也只画视口。
    let list_area = chunks[2];
    if app.shown.is_empty() {
        app.list_scroll = 0;
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
        let sig = format!(
            "{}|{}|{:?}|{}|{}|{}|{}",
            app.all.len(),
            app.shown.len(),
            app.shown.first(),
            app.marks_rev,
            app.query,
            app.theme.mode as u8,
            list_area.width,
        );
        if sig != app.list_sig || app.list_at.elapsed() > Duration::from_secs(60) {
            rebuild_list(app, list_area.width);
            app.list_sig = sig;
            app.list_at = Instant::now();
        }
        f.render_widget(
            HistoryList {
                lines: &app.list_items,
                first_of: &app.list_first_of,
                selected: app.selected,
                theme: th,
                scroll: &mut app.list_scroll,
            },
            list_area,
        );
    }

    // ── 流式字幕：录音中把中间文本顶在列表上方，边说边出字。
    //    只画**最后 4 行**——最新说的话在尾部，取头部会让字幕看似冻结
    if let Phase::Recording(_) = app.phase {
        if !app.partial.is_empty() {
            let w = list_area.width.saturating_sub(4);
            let lines = wrap_width(&app.partial, w as usize);
            let skip = lines.len().saturating_sub(4);
            let view = &lines[skip..];
            let n = view.len() as u16;
            let rect = Rect {
                x: list_area.x + 2,
                y: list_area.y + 1,
                width: w,
                height: n,
            };
            clear_with_margin(f, rect);
            let rows: Vec<Line> = view
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let mut spans = Vec::new();
                    if i == 0 {
                        spans.push(Span::styled("● ", Style::default().fg(th.rec)));
                        if skip > 0 {
                            spans.push(Span::styled("… ", Style::default().fg(th.faint)));
                        }
                    } else {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled(l.clone(), Style::default().fg(Color::Reset)));
                    Line::from(spans)
                })
                .collect();
            f.render_widget(
                Paragraph::new(rows).style(Style::default().bg(th.panel_bg)),
                rect,
            );
        }
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

    let (display, cursor_col) = input_display(app);
    let (display, cursor_col) =
        fit_input(&display, cursor_col, inner.width.saturating_sub(2) as usize);
    let input_spans = if app.input.is_empty() {
        vec![
            Span::styled("❯ ", Style::default().fg(th.focus)),
            Span::styled(
                "输入过滤历史 · / 命令 · m 标记 · ⏎ 复制",
                Style::default().fg(th.faint),
            ),
        ]
    } else {
        vec![
            Span::styled("❯ ", Style::default().fg(th.focus)),
            Span::styled(display, Style::default().fg(Color::Reset)),
        ]
    };
    f.render_widget(
        Paragraph::new(Line::from(input_spans)).style(Style::default().bg(th.panel_bg)),
        inner,
    );
    // 光标位置按「画出来的宽度」算：/key 打码后每个字符画成 1 格圆点
    f.set_cursor_position((
        (inner.x + 2 + cursor_col as u16).min(inner.x + inner.width.saturating_sub(1)),
        inner.y,
    ));

    // ── 第二行：命令/参数面板提示 > toast > 相位
    let hits = palette_hits(&app.input);
    let arg = arg_hits(&app.input);
    let second: Line = if app.input.starts_with("/key ") {
        Line::from(Span::styled(
            "key 只写进本地配置（权限 600），输入行已打码   ⏎ 保存",
            Style::default().fg(th.faint),
        ))
    } else if let Some((kind, ref h)) = arg {
        if h.is_empty() {
            Line::from(Span::styled(
                if kind == ArgKind::Model {
                    "没有匹配的模型（回车按原样执行）"
                } else {
                    "没有匹配的地址（回车按原样执行）"
                },
                Style::default().fg(th.faint),
            ))
        } else {
            Line::from(Span::styled(
                format!("{}  ↑↓ 选择  ⏎ 使用  Tab 补名", kind.title().trim()),
                Style::default().fg(th.faint),
            ))
        }
    } else if app.input.starts_with('/') {
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
        let fresh = t.at.elapsed() < toast_ttl(t);
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

    // ── 浮动面板（命令 / 参数），opencode 的菜单样式：浮在输入框上方
    if let Some((kind, h)) = &arg {
        if !h.is_empty() {
            let sel = app.palette_sel.min(h.len() - 1);
            let items = kind.items();
            let start = window_start(sel, h.len(), PALETTE_ROWS);
            let rows: Vec<Line> = h
                .iter()
                .skip(start)
                .take(PALETTE_ROWS)
                .enumerate()
                .map(|(row, &i)| {
                    let (name, desc) = items[i];
                    let mut line = Line::from(vec![
                        Span::styled(format!(" {name}  "), Style::default().fg(th.accent)),
                        Span::styled(desc.to_string(), Style::default().fg(th.dim)),
                    ]);
                    if start + row == sel {
                        line = line.style(Style::default().bg(th.selected_bg));
                    }
                    line
                })
                .collect();
            let n = rows.len() as u16;
            let title = format!(" {} {}/{} ", kind.title().trim(), sel + 1, h.len());
            render_float_box(
                f,
                th,
                Rect {
                    x: area.x + 2,
                    y: panel.y.saturating_sub(n + 3),
                    width: 64.min(area.width.saturating_sub(4)),
                    height: n + 2,
                },
                &title,
                rows,
            );
        }
    } else if palette_query(&app.input).is_some() && !hits.is_empty() {
        let sel = app.palette_sel.min(hits.len() - 1);
        let start = window_start(sel, hits.len(), PALETTE_ROWS);
        let n = hits.len().min(PALETTE_ROWS) as u16;
        let w = 64.min(area.width.saturating_sub(4));
        let rect = Rect {
            x: area.x + 2,
            y: panel.y.saturating_sub(n + 2 + 1),
            width: w,
            height: n + 2,
        };
        let rows: Vec<Line> = hits
            .iter()
            .skip(start)
            .take(PALETTE_ROWS)
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
                if start + row == sel {
                    line = line.style(Style::default().bg(th.selected_bg));
                }
                line
            })
            .collect();
        let title = format!(" 命令 {}/{} ", sel + 1, hits.len());
        render_float_box(f, th, rect, &title, rows);
    }

    if let Some(ov) = app.overlay {
        draw_overlay(f, app, ov);
    }
}

/// 重建历史列表行。折行结果按 (id, 宽度) 缓存——条目一旦落库就不变，
/// 所以只有首次（或宽度变化）才真正做宽度计算。
fn rebuild_list(app: &mut App, width: u16) {
    let th = app.theme;
    let today = chrono::Local::now().date_naive();
    let yesterday = today.pred_opt().unwrap_or(today);
    let body_w = (width.saturating_sub(GUTTER + ROW_PREFIX as u16 + 2)) as usize;
    // 缓存条数远超条目数（宽度来回变）时整体丢弃，防止无界增长
    if app.wrap_cache.len() > app.all.len() * 3 + 128 {
        app.wrap_cache.clear();
    }

    /// 分组键：正常条目用日历日期（来自 ts_ms，与追加序无关）；坏行退回 ts 前缀
    enum DayKey {
        D(chrono::NaiveDate),
        Raw(String),
    }

    let mut items: Vec<Line<'static>> = Vec::with_capacity(app.shown.len() + 8);
    let mut first_of: Vec<usize> = Vec::with_capacity(app.shown.len());
    let mut prev_key: Option<DayKey> = None;

    for i in app.shown.iter() {
        let e = &app.all[*i];
        let key = match entry_date(e) {
            Some(d) => DayKey::D(d),
            None => DayKey::Raw(e.ts.get(0..10).unwrap_or("").to_string()),
        };
        let new_day = match (&prev_key, &key) {
            (Some(DayKey::D(a)), DayKey::D(b)) => a != b,
            (Some(DayKey::Raw(a)), DayKey::Raw(b)) => a != b,
            _ => true,
        };
        if new_day {
            let label = match &key {
                DayKey::D(d) => day_label_at(*d, today, yesterday),
                DayKey::Raw(r) => r.clone(),
            };
            let lw = UnicodeWidthStr::width(label.as_str());
            // 分隔线画满整行（宽终端下不再被 60 列截短）
            let fill = width.saturating_sub(lw as u16 + 4) as usize;
            items.push(Line::from(Span::styled(
                format!(" {label} {}", "─".repeat(fill)),
                Style::default().fg(th.faint),
            )));
            prev_key = Some(key);
        }
        first_of.push(items.len());

        let is_marked = app.marks.contains(&e.id);
        let time = e.ts.get(11..16).unwrap_or("--:--");
        let (body, color) = if e.is_ok() {
            (e.text.replace('\n', " "), Color::Reset)
        } else {
            (
                e.status.trim_start_matches("error: ").replace('\n', " "),
                th.rec,
            )
        };
        let lines = app
            .wrap_cache
            .entry((e.id.clone(), body_w))
            .or_insert_with(|| wrap_width(&body, body_w))
            .clone();
        let last = lines.len().saturating_sub(1);
        for (li, text) in lines.iter().enumerate() {
            let mut spans: Vec<Span> = Vec::new();
            if li == 0 {
                // 左侧 gutter 由 history_list 画选中标记；这里只留内容前缀
                spans.push(Span::styled(
                    format!(" {time}  "),
                    Style::default().fg(th.faint),
                ));
                spans.push(Span::styled("❯ ", Style::default().fg(th.accent)));
            } else {
                spans.push(Span::raw(" ".repeat(ROW_PREFIX)));
            }
            spans.push(Span::styled(text.clone(), Style::default().fg(color)));
            if li == last {
                if is_marked {
                    spans.push(Span::styled(" ✓", Style::default().fg(th.accent)));
                }
                // 错误条目 latency 恒 0，"→ 0.0s" 是没测不是瞬时——不显示
                if e.is_ok() {
                    spans.push(Span::styled(
                        format!(
                            "  {:.1}s → {:.1}s",
                            e.duration_ms as f32 / 1000.0,
                            e.latency_ms as f32 / 1000.0
                        ),
                        Style::default().fg(th.faint),
                    ));
                }
            }
            items.push(Line::from(spans));
        }
    }

    app.list_items = items;
    app.list_first_of = first_of;
}

/// 居中样式的浮动小面板（圆角 + 标题 + 清底）。
fn render_float_box(f: &mut ratatui::Frame, th: Theme, rect: Rect, title: &str, rows: Vec<Line>) {
    clear_with_margin(f, rect);
    f.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .border_type(BorderType::Rounded)
                .borders(ratatui::widgets::Borders::ALL)
                .title(Span::styled(title, Style::default().fg(th.accent)))
                .border_style(Style::default().fg(th.border))
                .style(Style::default().bg(th.panel_bg)),
        ),
        rect,
    );
}

fn hint_line(app: &App, th: Theme) -> Line<'static> {
    let _ = app;
    Line::from(Span::styled(
        "↑↓/jk 选择 · m 标记 · 空格录音 · / 命令 · ⏎ 复制 · Del 删除 · F1 帮助",
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
                    // subtitle 是「热键 · 模型」拼接，只取热键那段，别把模型名读成热键
                    format!(
                        "热键 {}（hold 按住说话 / toggle 按一下开关）",
                        app.subtitle.split(" · ").next().unwrap_or(&app.subtitle)
                    ),
                    Style::default().fg(th.dim),
                )),
                Line::from(""),
                Line::from("导航    ↑↓ 或 j k 选择    g 最新    G 最旧"),
                Line::from("        Ctrl+D / Ctrl+U 快速跳 12 条"),
                Line::from(""),
                Line::from("录音    空格 或 /record 手动开始 / 停止"),
                Line::from(""),
                Line::from("记录    ⏎ 复制选中（OSC 52）   Del 删除"),
                Line::from("        m 标记多条，⏎ 一起复制"),
                Line::from(""),
                Line::from("命令    / 打开命令面板：↑↓ 选、⏎ 执行、Tab 补全"),
                Line::from("        /model 或 /base 敲个空格 = 直接列表选"),
                Line::from("        /theme 切配色 · /key 存 key（输入行打码）"),
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
                    .border_style(Style::default().fg(th.border))
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
        let s = wrap_for_tmux("\x1b]52;c;5L2g5aW9\x07", false);
        assert!(s.starts_with("\x1b]52;c;"));
        assert!(s.ends_with('\x07'));
        assert!(s.contains("5L2g5aW9"));
    }

    #[test]
    fn osc52_wraps_for_tmux_passthrough() {
        let s = wrap_for_tmux("\x1b]52;c;5L2g5aW9\x07", true);
        assert!(s.starts_with("\x1bPtmux;"), "DCS 开头：{s:?}");
        assert!(s.ends_with("\x1b\\"), "DCS 结尾：{s:?}");
        // 里面的 ESC 要双写，否则 tmux 解不开
        assert!(s.contains("\x1b\x1b]52;c;"), "ESC 双写：{s:?}");
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

    #[test]
    fn arg_palette_filters_models_and_bases() {
        use crate::ui::settings_ui::{BASES, MODELS};
        // 敲了空格才算参数面板
        assert!(arg_hits("/model").is_none());
        let (kind, h) = arg_hits("/model ").unwrap();
        assert_eq!(kind, ArgKind::Model);
        assert_eq!(h.len(), MODELS.len());
        let (_, h) = arg_hits("/model qwen3").unwrap();
        let names: Vec<_> = h.iter().map(|&i| MODELS[i].0).collect();
        assert!(names.contains(&"qwen3-asr-flash"), "{names:?}");
        assert!(names.contains(&"qwen3-asr-flash-realtime"), "{names:?}");
        assert!(arg_hits("/model nope-xyz").unwrap().1.is_empty());
        let (kind, h) = arg_hits("/base 新").unwrap(); // 地址里没有中文
        assert_eq!(kind, ArgKind::Base);
        assert!(h.is_empty());
        let (_, h) = arg_hits("/base ").unwrap();
        assert_eq!(h.len(), BASES.len());
    }

    #[test]
    fn arg_palette_navigates_and_tabs() {
        use crate::ui::settings_ui::MODELS;
        let dir = std::env::temp_dir().join(format!("dictate-argpal-{}", std::process::id()));
        let mut app = app_with_entries(dir.to_str().unwrap());
        let kc = |c: KeyCode| crossterm::event::KeyEvent::new(c, KeyModifiers::NONE);
        for c in "/model ".chars() {
            handle_key(&mut app, &key(c)).unwrap();
        }
        handle_key(&mut app, &kc(KeyCode::Down)).unwrap();
        handle_key(&mut app, &kc(KeyCode::Down)).unwrap();
        // Tab 把面板里选中的名字补进输入行
        handle_key(&mut app, &kc(KeyCode::Tab)).unwrap();
        assert_eq!(app.input, format!("/model {}", MODELS[2].0));
        // 补全后名字本身就是过滤器：Up/Tab 停在这一项上（回车即用）
        handle_key(&mut app, &kc(KeyCode::Up)).unwrap();
        handle_key(&mut app, &kc(KeyCode::Tab)).unwrap();
        assert_eq!(app.input, format!("/model {}", MODELS[2].0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mark_entries_and_batch_copy() {
        let dir = std::env::temp_dir().join(format!("dictate-marks-{}", std::process::id()));
        let mut app = app_with_entries(dir.to_str().unwrap());
        // m 标记当前（new）→ j 下移 → m 标记（mid）
        handle_key(&mut app, &key('m')).unwrap();
        handle_key(&mut app, &key('j')).unwrap();
        handle_key(&mut app, &key('m')).unwrap();
        assert_eq!(app.marks.len(), 2);
        assert!(app.marks.contains("new") && app.marks.contains("mid"));
        // 空输入 ⏎ = 整批复制（toast 报条数；OSC 52 走 stdout，不在这里验）
        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, &enter).unwrap();
        let t = app.toast.as_ref().expect("应有复制 toast");
        assert!(t.msg.contains("2 条"), "toast={}", t.msg);
        // 输入非空时 m 是普通字符
        handle_key(&mut app, &key('x')).unwrap();
        handle_key(&mut app, &key('m')).unwrap();
        assert_eq!(app.input, "xm");
        std::fs::remove_dir_all(&dir).ok();
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
        assert_eq!(day_label_at(today, today, yesterday), "今天");
        assert_eq!(day_label_at(yesterday, today, yesterday), "昨天");
        assert_eq!(
            day_label_at(
                chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                today,
                yesterday
            ),
            "10-01"
        );
        // 跨年要带年份，否则分不清是哪一年
        assert_eq!(
            day_label_at(
                chrono::NaiveDate::from_ymd_opt(2025, 10, 1).unwrap(),
                today,
                yesterday
            ),
            "2025-10-01"
        );
    }

    // ── 输入编辑（借 opentui EditBuffer / rune TextEdit 的光标语义）──

    #[test]
    fn fit_input_scrolls_to_keep_the_cursor_visible() {
        // 宽字符按显示宽度算，滚动起点必须落在字符边界上
        let (shown, col) = fit_input("abc中文def", "abc中文def".len(), 6);
        assert_eq!(shown, "文def");
        assert_eq!(col, 5);
        // 光标在开头时不滚动
        assert_eq!(fit_input("abcdef", 0, 3), ("abcdef".to_string(), 0));
        // 宽度为 0 时不要 panic
        assert_eq!(fit_input("abc", 1, 0), (String::new(), 0));
    }

    #[test]
    fn input_editing_keeps_utf8_and_cursor_in_sync() {
        let dir = std::env::temp_dir().join(format!("dictate-edit-{}", std::process::id()));
        let mut app = app_with_entries(dir.to_str().unwrap());
        set_input(&mut app, "ab中c");
        input_left(&mut app); // c 前
        input_backspace(&mut app); // 整个「中」一起删，不能切在 UTF-8 中间
        assert_eq!(app.input, "abc");
        assert_eq!(app.input_cursor, 2);

        input_home(&mut app);
        input_insert(&mut app, '你');
        assert_eq!(app.input, "你abc");
        assert_eq!(app.input_cursor, "你".len());

        input_end(&mut app);
        input_left(&mut app);
        input_delete(&mut app); // 删掉光标后的 c
        assert_eq!(app.input, "你ab");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ctrl_w_kills_the_word_before_the_cursor() {
        let dir = std::env::temp_dir().join(format!("dictate-kill-{}", std::process::id()));
        let mut app = app_with_entries(dir.to_str().unwrap());
        set_input(&mut app, "alpha beta  ");
        input_kill_word(&mut app);
        assert_eq!(app.input, "alpha ");
        input_kill_word(&mut app);
        assert_eq!(app.input, "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn key_input_masks_every_secret_char_and_maps_cursor() {
        let dir = std::env::temp_dir().join(format!("dictate-mask-{}", std::process::id()));
        let mut app = app_with_entries(dir.to_str().unwrap());
        set_input(&mut app, "/key sk-abc");
        let (shown, col) = input_display(&app);
        assert_eq!(shown, "/key ••••••");
        assert_eq!(col, 5 + 6);
        app.input_cursor = 7; // 两个 secret 字符之后
        let (_, col) = input_display(&app);
        assert_eq!(col, 7);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn palette_window_centers_selection_without_running_past_the_end() {
        assert_eq!(window_start(0, 20, 8), 0);
        assert_eq!(window_start(5, 20, 8), 1);
        assert_eq!(window_start(19, 20, 8), 12);
        assert_eq!(window_start(3, 5, 8), 0);
        assert_eq!(window_start(3, 5, 0), 0);
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

        std::fs::write(
            &path,
            r#"{"phase":"recording","since_ms":0,"partial":"你好"}"#,
        )
        .unwrap();
        apply_status_from(&path, &mut app);
        assert_eq!(app.partial, "你好", "流式中间文本要进 App");

        std::fs::write(&path, r#"{"phase":"idle","since_ms":0,"text":"你好"}"#).unwrap();
        apply_status_from(&path, &mut app);
        assert_eq!(app.phase, Phase::Idle);
        assert!(app.daemon_note.contains("你好"));

        std::fs::write(&path, r#"{"phase":"idle","since_ms":0,"error":"HTTP 401"}"#).unwrap();
        apply_status_from(&path, &mut app);
        assert!(app.daemon_note.contains("401"));
        assert!(
            app.daemon_note.starts_with("失败"),
            "错误要走失败通道（红字+建议）"
        );

        // note 通道：非失败信息（防抖忽略、流式回退）不该渲染成失败
        std::fs::write(
            &path,
            r#"{"phase":"idle","since_ms":0,"note":"按键太短，已忽略"}"#,
        )
        .unwrap();
        apply_status_from(&path, &mut app);
        assert!(app.daemon_note.contains("按键太短"));
        assert!(!app.daemon_note.starts_with("失败"));

        // 心跳过期 = 守护进程崩了但状态文件残留，不能再照着演「就绪/录音中」
        let stale = crate::core::history::now_ms().saturating_sub(DAEMON_STALE_MS + 60_000);
        std::fs::write(
            &path,
            format!(r#"{{"phase":"recording","since_ms":0,"hb_ms":{stale}}}"#),
        )
        .unwrap();
        apply_status_from(&path, &mut app);
        assert!(!app.daemon_alive, "心跳过期应判定守护进程已死");
        assert!(matches!(app.phase, Phase::Idle));
        assert!(app.daemon_note.contains("没在响应"));

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
        let mut a = App {
            history: History::new(std::path::Path::new("/nonexistent")),
            all: app.all.clone(),
            shown: app.shown.clone(),
            query: app.query.clone(),
            selected: app.selected,
            daemon_note: app.daemon_note.clone(),
            daemon_alive: app.daemon_alive,
            partial: app.partial.clone(),
            phase: app.phase,
            subtitle: app.subtitle.clone(),
            theme: app.theme,
            overlay: app.overlay,
            settings: None,
            input: input.to_string(),
            input_cursor: input.len(),
            palette_sel: app.palette_sel,
            marks: app.marks.clone(),
            marks_rev: app.marks_rev,
            toast: app.toast.clone(),
            quit_requested: false,
            wrap_cache: std::collections::HashMap::new(),
            list_items: Vec::new(),
            list_first_of: Vec::new(),
            list_sig: String::new(),
            list_at: Instant::now(),
            list_scroll: 0,
        };
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, &mut a)).unwrap();
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
    fn narrow_header_keeps_status_and_drops_optional_text() {
        let today = chrono::Local::now().date_naive();
        let app = demo_app(today, "narrow");
        let scr = render(&app, "", 30, 10);
        let compact: String = scr.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("dictate"), "{scr}");
        assert!(compact.contains("无守护"), "窄终端也要看见守护状态：{scr}");
        assert!(
            !compact.contains("qwen3-asr-flash"),
            "窄终端应丢掉副标题：{scr}"
        );
        assert!(!compact.contains("v0.1.2"), "更窄时应先丢版本号：{scr}");
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

    #[test]
    fn render_arg_palette_and_marks() {
        let today = chrono::Local::now().date_naive();
        let app = demo_app(today, "argpal");
        let scr = render(&app, "/model qwen3", 100, 28);
        let compact: String = scr.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("╭模型"), "参数面板应有圆角边框和标题");
        assert!(
            compact.contains("qwen3-asr-flash"),
            "面板里应有过滤后的模型"
        );
        assert!(
            !compact.contains("qwen-audio-3.1-asr-flash\n"),
            "没匹配的不该排前面"
        );

        let mut marked = demo_app(today, "marks");
        let first_id = marked.all[marked.shown[0]].id.clone();
        marked.marks.insert(first_id);
        let scr = render(&marked, "", 100, 28);
        assert!(scr.contains("✓"), "被标记的条目应有 ✓ 标记");
    }

    #[test]
    fn key_input_is_masked_on_screen() {
        let today = chrono::Local::now().date_naive();
        let app = demo_app(today, "keymask");
        let scr = render(&app, "/key sk-topsecret123", 100, 28);
        assert!(!scr.contains("sk-topsecret123"), "key 明文绝不能上屏");
        assert!(scr.contains("•••"), "key 应画成圆点");
        let compact: String = scr.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("已打码"), "应说明输入行已打码");
    }

    #[test]
    fn error_hints_point_to_the_next_step() {
        assert!(error_hint("失败：HTTP 401 Unauthorized：key 不对").contains("/key"));
        assert!(error_hint("失败：HTTP 429 Too Many Requests").contains("限流"));
        assert!(error_hint("没有检测到语音：峰值 -90 dBFS，低于门限 -35 dBFS").contains("probe"));
        assert!(error_hint("失败：请求超时").contains("/base"));
        // /retry 撞墙时建议必须是「打开保留音频」，不能让用户再跑一次 /retry 原地打转
        assert!(
            error_hint("没有保留的音频——keep_audio = true 后的录音才能重转").contains("保留音频")
        );
        assert!(error_hint("守护进程没在跑 —— 先执行 dictate").contains("dictate run"));
        assert!(error_hint("失败：别的什么").contains("doctor"));
    }

    #[test]
    fn render_failure_note_with_hint() {
        let today = chrono::Local::now().date_naive();
        let mut app = demo_app(today, "errhint");
        app.daemon_note = "失败：HTTP 401 Unauthorized：key 不对".into();
        let scr = render(&app, "", 100, 28);
        let compact: String = scr.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("401"), "错误本身要显示");
        assert!(compact.contains("/key换个key"), "失败要带下一步建议");
    }

    #[test]
    fn render_live_caption_while_recording() {
        let today = chrono::Local::now().date_naive();
        let mut app = demo_app(today, "capt");
        app.phase = Phase::Recording(std::time::Instant::now());
        app.partial = "今天下午三点开会，顺便对齐一下进度。".into();
        let scr = render(&app, "", 100, 28);
        let compact: String = scr.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("今天下午三点开会"), "录音中应显示流式字幕");
        // 空闲时不显示字幕
        let mut idle = demo_app(today, "capt2");
        idle.partial = "残留文本".into();
        let scr = render(&idle, "", 100, 28);
        assert!(
            !scr.replace(' ', "").contains("残留文本"),
            "非录音态不该有字幕"
        );
    }

    #[test]
    fn caption_shows_the_latest_lines_not_the_first() {
        // 字幕超 4 行时要显示尾部（正在说的话），取头部会让字幕看似冻结
        let today = chrono::Local::now().date_naive();
        let mut app = demo_app(today, "capt3");
        app.phase = Phase::Recording(std::time::Instant::now());
        let mut partial = String::new();
        for i in 0..8 {
            // 每句约一行宽（100 列终端、字幕区 96 列），8 句 = 8 行，字幕只留最后 4 行
            partial.push_str(&format!("第{i}句话{}。", "很长".repeat(20)));
        }
        app.partial = partial;
        let scr = render(&app, "", 100, 24);
        let compact: String = scr.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("第7句"), "字幕应包含最新的内容：{compact}");
        assert!(!compact.contains("第0句"), "旧内容应该滚出字幕区");
    }

    #[test]
    fn filter_matches_error_status_so_failures_stay_visible() {
        // 错误条目 text 为空：只匹配 text 会把刚发生的失败从筛选视图里藏掉
        let dir = std::env::temp_dir().join(format!("dictate-filt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        use std::io::Write;
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "id": "e1", "ts": "2026-10-26T10:00:00+08:00", "ts_ms": 1,
                "duration_ms": 900, "latency_ms": 0, "model": "m",
                "status": "error: HTTP 401 Unauthorized：key 不对", "text": ""
            })
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "id": "ok1", "ts": "2026-10-26T10:01:00+08:00", "ts_ms": 2,
                "duration_ms": 900, "latency_ms": 100, "model": "m",
                "status": "ok", "text": "正常的一条"
            })
        )
        .unwrap();
        let mut app = App::new(History::new(&path), Theme::dark());
        app.query = "401".into();
        app.refilter();
        assert_eq!(app.shown.len(), 1, "筛 401 应命中错误条目");
        assert_eq!(app.all[app.shown[0]].id, "e1");
        std::fs::remove_dir_all(&dir).ok();
    }
}
