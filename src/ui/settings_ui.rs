//! 终端内配置界面：所有设置都在这里改，不用手写 TOML。

use std::sync::mpsc::{channel, Receiver};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

use unicode_width::UnicodeWidthStr;

use crate::core::config::{self, Config};
use crate::ui::theme::{Mode, Theme};

/// 按**显示宽度**补齐（中文占两格），否则中英混排的列会歪。
fn pad(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// 支持的模型（四种报文形状都能跑）。tui 的 /model 参数面板共用。
pub(crate) const MODELS: &[(&str, &str)] = &[
    (
        "qwen3-asr-flash",
        "OpenAI 兼容 · 10 MB / 5 分钟 · 支持 ITN、语言指定",
    ),
    ("qwen-audio-3.1-asr-flash", "DashScope 同步 · 2 GB / 5 分钟"),
    ("qwen-audio-3.0-asr-flash", "DashScope 同步 · 2 GB / 5 分钟"),
    (
        "qwen-audio-3.1-asr-flash-message",
        "WebSocket · 只收 16 kHz",
    ),
    ("qwen-audio-3.1-asr-flash-streaming", "WebSocket 流式输出"),
    ("qwen3-asr-flash-realtime", "WebSocket 实时"),
    (
        "qwen3-asr-flash-filetrans",
        "DashScope 异步 · 上传转写，延迟高",
    ),
    (
        "qwen-audio-3.1-asr-flash-filetrans",
        "DashScope 异步 · 上传转写，延迟高",
    ),
];

pub(crate) const BASES: &[(&str, &str)] = &[
    ("https://dashscope.aliyuncs.com", "中国（北京）"),
    ("https://dashscope-intl.aliyuncs.com", "新加坡"),
];

const FIELDS: [&str; 15] = [
    "服务地址",
    "模型",
    "报文形状",
    "API key",
    "语言",
    "ITN（数字规范化）",
    "热键",
    "热键模式",
    "静音自动结束 (ms)",
    "单次上限 (s)",
    "超时 (s)",
    "配色 (auto/dark/light)",
    "交付方式",
    "流式转写（边说边出字）",
    "保留音频（last.wav，可 /retry）",
];

pub struct Form {
    cfg: Config,
    sel: usize,
    editing: bool,
    buf: String,
    status: String,
    dirty: bool,
    test: Option<Receiver<Result<(u64, String), String>>>,
    pub theme: Theme,
    picker: Option<Picker>,
}

struct Picker {
    title: &'static str,
    items: &'static [(&'static str, &'static str)],
    sel: usize,
    field: usize,
}

impl Form {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            sel: 0,
            editing: false,
            buf: String::new(),
            status: String::new(),
            dirty: false,
            test: None,
            theme: Theme::dark(),
            picker: None,
        }
    }

    fn value(&self, i: usize) -> String {
        match i {
            0 => {
                let hit = BASES.iter().find(|(u, _)| *u == self.cfg.base_url);
                match hit {
                    Some((_, label)) => format!("{}（{}）", self.cfg.base_url, label),
                    None => self.cfg.base_url.clone(),
                }
            }
            1 => match crate::asr::pick_style(&self.cfg.model, self.cfg.api_style.as_deref()) {
                Ok(style) => format!("{}（{}）", self.cfg.model, style.name()),
                Err(_) => self.cfg.model.clone(),
            },
            2 => match crate::asr::pick_style(&self.cfg.model, self.cfg.api_style.as_deref()) {
                Ok(style) => format!("{}（由模型决定）", style.name()),
                Err(_) => "未知".into(),
            },
            3 => match &self.cfg.api_key {
                Some(k) if !k.is_empty() => mask(k),
                _ => "（未设置，可用 DASHSCOPE_API_KEY）".into(),
            },
            4 => self.cfg.language.clone().unwrap_or_else(|| "auto".into()),
            5 => if self.cfg.enable_itn { "开" } else { "关" }.into(),
            6 => self.cfg.hotkey.clone(),
            7 => {
                if self.cfg.hotkey_mode.eq_ignore_ascii_case("toggle") {
                    "toggle（按一下开、再按一下停）".into()
                } else {
                    "hold（按住说话）".into()
                }
            }
            8 => self.cfg.silence_stop_ms.to_string(),
            9 => format!("{}", self.cfg.max_seconds),
            10 => self.cfg.timeout_secs.to_string(),
            11 => self
                .cfg
                .theme
                .clone()
                .unwrap_or_else(|| "auto（问终端）".into()),
            12 => match self.cfg.paste.as_str() {
                "off" => "off（只进历史）".into(),
                "clipboard" => "clipboard（只写剪贴板）".into(),
                _ => "auto（剪贴板 + 自动粘贴）".into(),
            },
            13 => {
                if !self.model_supports_ws() {
                    "关（当前模型不支持）".into()
                } else if self.cfg.streaming {
                    "开".into()
                } else {
                    "关".into()
                }
            }
            14 => {
                if self.cfg.keep_audio {
                    "开（失败后可 /retry 重转）".into()
                } else {
                    "关".into()
                }
            }
            _ => String::new(),
        }
    }

    fn model_supports_ws(&self) -> bool {
        crate::asr::pick_style(&self.cfg.model, self.cfg.api_style.as_deref())
            .map(|s| s == crate::asr::Style::Ws)
            .unwrap_or(false)
    }

    fn cycle_theme(&mut self, dir: i32) {
        let order = [None, Some("dark"), Some("light")];
        let cur = order
            .iter()
            .position(|s| s.map(|x| x.to_string()) == self.cfg.theme)
            .unwrap_or(0) as i32;
        let next = ((cur + dir).rem_euclid(order.len() as i32)) as usize;
        self.cfg.theme = order[next].map(|s| s.to_string());
        self.theme = Theme::resolve(self.cfg.theme.as_deref());
        self.dirty = true;
    }

    fn start_edit(&mut self) {
        let i = self.sel;
        if i == 5 {
            self.cfg.enable_itn = !self.cfg.enable_itn;
            self.dirty = true;
            return;
        }
        if i == 2 {
            // 报文形状由模型名决定，是只读展示：进编辑态只会造成「看似能改」的假象
            self.status = "报文形状由模型决定——改「模型」这一项即可".into();
            return;
        }
        if i == 14 {
            self.cfg.keep_audio = !self.cfg.keep_audio;
            self.dirty = true;
            return;
        }
        if i == 11 {
            self.cycle_theme(1);
            return;
        }
        if i == 12 {
            self.cfg.paste = match self.cfg.paste.as_str() {
                "auto" => "clipboard".into(),
                "clipboard" => "off".into(),
                _ => "auto".into(),
            };
            self.dirty = true;
            return;
        }
        if i == 13 {
            if self.model_supports_ws() {
                self.cfg.streaming = !self.cfg.streaming;
                self.dirty = true;
            } else {
                self.status =
                    "流式需要 WebSocket 系模型（-message / -streaming / -realtime）".into();
            }
            return;
        }
        if i == 7 {
            let toggle = self.cfg.hotkey_mode.eq_ignore_ascii_case("toggle");
            self.cfg.hotkey_mode = if toggle {
                "hold".into()
            } else {
                "toggle".into()
            };
            self.dirty = true;
            return;
        }
        if i == 1 {
            self.picker = Some(Picker {
                title: "选择模型",
                items: MODELS,
                sel: MODELS
                    .iter()
                    .position(|(m, _)| *m == self.cfg.model)
                    .unwrap_or(0),
                field: 1,
            });
            return;
        }
        if i == 0 {
            self.picker = Some(Picker {
                title: "选择服务地址",
                items: BASES,
                sel: BASES
                    .iter()
                    .position(|(u, _)| *u == self.cfg.base_url)
                    .unwrap_or(0),
                field: 0,
            });
            return;
        }
        self.editing = true;
        self.buf = match i {
            0 => {
                let hit = BASES.iter().find(|(u, _)| *u == self.cfg.base_url);
                match hit {
                    Some((_, label)) => format!("{}（{}）", self.cfg.base_url, label),
                    None => self.cfg.base_url.clone(),
                }
            }
            1 => match crate::asr::pick_style(&self.cfg.model, self.cfg.api_style.as_deref()) {
                Ok(style) => format!("{}（{}）", self.cfg.model, style.name()),
                Err(_) => self.cfg.model.clone(),
            },
            3 => self.cfg.api_key.clone().unwrap_or_default(),
            4 => self.cfg.language.clone().unwrap_or_default(),
            6 => self.cfg.hotkey.clone(),
            7 => self.cfg.hotkey_mode.clone(),
            8 => self.cfg.silence_stop_ms.to_string(),
            9 => format!("{}", self.cfg.max_seconds),
            10 => self.cfg.timeout_secs.to_string(),
            _ => String::new(),
        };
    }

    fn commit_edit(&mut self) {
        let v = self.buf.trim().to_string();
        let i = self.sel;
        let res: Result<()> = (|| {
            match i {
                0 => {
                    let _ = crate::asr::normalize_base(&v)?;
                    self.cfg.base_url = v.clone();
                }
                1 => {
                    if v.is_empty() {
                        anyhow::bail!("模型名不能为空");
                    }
                    crate::asr::pick_style(&v, self.cfg.api_style.as_deref())?;
                    self.cfg.model = v.clone();
                }
                3 => self.cfg.api_key = if v.is_empty() { None } else { Some(v.clone()) },
                4 => {
                    self.cfg.language = if v.is_empty() || v == "auto" {
                        None
                    } else {
                        Some(v.clone())
                    }
                }
                6 => {
                    crate::hotkey::Spec::parse(&v)?;
                    self.cfg.hotkey = v.clone();
                }
                7 => {
                    if v.eq_ignore_ascii_case("toggle") || v.eq_ignore_ascii_case("hold") {
                        self.cfg.hotkey_mode = v.to_lowercase();
                    } else {
                        anyhow::bail!("只能是 hold 或 toggle");
                    }
                }
                8 => {
                    self.cfg.silence_stop_ms =
                        v.parse().map_err(|_| anyhow::anyhow!("要是整数 ms"))?
                }
                9 => self.cfg.max_seconds = v.parse().map_err(|_| anyhow::anyhow!("要是数字秒"))?,
                10 => {
                    self.cfg.timeout_secs = v.parse().map_err(|_| anyhow::anyhow!("要是整数秒"))?
                }
                _ => {}
            }
            Ok(())
        })();
        match res {
            Ok(()) => {
                self.dirty = true;
                self.editing = false;
                self.status = "已修改（自动保存）".into();
            }
            Err(e) => self.status = format!("没改：{e:#}"),
        }
    }

    /// 自动保存：不发状态提示（状态由调用方决定）。
    fn save_silent(&mut self) -> Result<()> {
        crate::asr::pick_style(&self.cfg.model, self.cfg.api_style.as_deref())?;
        config::write(&config::config_path(), &self.cfg)?;
        self.dirty = false;
        Ok(())
    }

    fn save(&mut self) -> Result<()> {
        crate::asr::pick_style(&self.cfg.model, self.cfg.api_style.as_deref())?;
        let path = config::config_path();
        config::write(&path, &self.cfg)?;
        self.dirty = false;
        self.status = format!("已保存到 {}", path.display());
        Ok(())
    }

    pub fn open_model_picker(&mut self) {
        self.picker = Some(Picker {
            title: "选择模型",
            items: MODELS,
            sel: MODELS
                .iter()
                .position(|(m, _)| *m == self.cfg.model)
                .unwrap_or(0),
            field: 1,
        });
    }

    pub fn open_base_picker(&mut self) {
        self.picker = Some(Picker {
            title: "选择服务地址",
            items: BASES,
            sel: BASES
                .iter()
                .position(|(u, _)| *u == self.cfg.base_url)
                .unwrap_or(0),
            field: 0,
        });
    }

    /// 收起测试结果（内嵌模式下由外层循环调用）。
    pub fn poll_test(&mut self) {
        if let Some(rx) = &self.test {
            if let Ok(res) = rx.try_recv() {
                self.status = match res {
                    Ok((ms, text)) => format!("测试通过（{ms} ms）：{text:?}"),
                    Err(e) => format!("测试失败：{e}"),
                };
                self.test = None;
            }
        }
    }

    fn run_test(&mut self) {
        if self.test.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            let out = crate::core::pipeline::self_test(&cfg).map_err(|e| format!("{e:#}"));
            let _ = tx.send(out);
        });
        self.test = Some(rx);
        self.status = "测试中…".into();
    }
}

fn mask(key: &str) -> String {
    let n = key.chars().count();
    if n <= 8 {
        "****".into()
    } else {
        format!("{}…{}", &key[..4], &key[n - 4..])
    }
}

pub fn run() -> Result<()> {
    let cfg = config::load()?;
    let mut form = Form::new(cfg);

    enable_raw_mode()?;
    form.theme = Theme::resolve(form.cfg.theme.as_deref());
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;
    let res = event_loop(&mut term, &mut form);
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

fn event_loop(term: &mut Terminal<CrosstermBackend<std::io::Stdout>>, f: &mut Form) -> Result<()> {
    loop {
        if let Some(rx) = &f.test {
            if let Ok(res) = rx.try_recv() {
                f.status = match res {
                    Ok((ms, text)) => format!("测试通过（{ms} ms）：{text:?}"),
                    Err(e) => format!("测试失败：{e}"),
                };
                f.test = None;
            }
        }
        term.draw(|fr| draw(fr, f))?;
        if !event::poll(std::time::Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        if handle_form_key(f, &k)? {
            return Ok(());
        }
    }
}

/// 按键处理。true = 关闭设置页。
/// 任何路径（包括关闭）都会先把未保存的改动落盘——不存在「已确认的修改静默丢失」的出口。
fn key(f: &mut Form, k: &crossterm::event::KeyEvent) -> Result<bool> {
    if let Some(p) = f.picker.as_mut() {
        // q 只关选择器，不关整个设置页（j/k 导航和 q 同键区，误触不该把人踢出去）
        if k.code == KeyCode::Char('q') && k.modifiers.is_empty() {
            f.picker = None;
            return Ok(false);
        }
        match k.code {
            KeyCode::Esc => f.picker = None,
            KeyCode::Up | KeyCode::Char('k') => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if p.sel + 1 < p.items.len() {
                    p.sel += 1;
                }
            }
            KeyCode::Enter => {
                let chosen = p.items[p.sel].0;
                let field = p.field;
                if field == 0 {
                    f.cfg.base_url = chosen.to_string();
                } else {
                    f.cfg.model = chosen.to_string();
                    f.cfg.api_style = None;
                }
                f.dirty = true;
                f.picker = None;
                f.status = format!("已选择 {chosen}");
            }
            _ => {}
        }
        return Ok(false);
    }

    if f.editing {
        match k.code {
            KeyCode::Esc => {
                f.editing = false;
                f.status = "已取消".into();
            }
            KeyCode::Enter => f.commit_edit(),
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => f.buf.clear(),
            KeyCode::Backspace => {
                f.buf.pop();
            }
            KeyCode::Char(c) => f.buf.push(c),
            _ => {}
        }
        return Ok(false);
    }

    match k.code {
        KeyCode::Esc => return Ok(true),
        KeyCode::Up | KeyCode::Char('k') => f.sel = f.sel.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            if f.sel + 1 < FIELDS.len() {
                f.sel += 1;
            }
        }
        KeyCode::Enter => f.start_edit(),
        KeyCode::Left if f.sel == 11 => f.cycle_theme(-1),
        KeyCode::Right if f.sel == 11 => f.cycle_theme(1),
        KeyCode::F(2) | KeyCode::Char('s')
            if k.modifiers.is_empty() || k.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            match f.save() {
                Ok(()) => {}
                Err(e) => f.status = format!("保存失败：{e:#}"),
            }
        }
        KeyCode::F(3) => f.run_test(),
        _ => {}
    }
    Ok(false)
}

/// 在同一帧里画设置页（供 TUI 内嵌使用）。
pub fn draw_form(fr: &mut ratatui::Frame, f: &Form) {
    draw(fr, f)
}

/// 处理一个按键。返回 true = 关闭设置页。
/// 收尾自动保存在这里做：key() 内部不管从哪条路径提前 return（picker 回车、编辑态、
/// Esc 关页），改动都会先落盘，不存在「已确认的修改静默丢失」的出口。
pub fn handle_form_key(f: &mut Form, k: &crossterm::event::KeyEvent) -> Result<bool> {
    let close = key(f, k)?;
    if f.dirty {
        match f.save_silent() {
            Ok(()) => {}
            Err(e) => f.status = format!("保存失败：{e:#}"),
        }
    }
    Ok(close)
}

fn draw(fr: &mut ratatui::Frame, f: &Form) {
    let th = f.theme;
    use ratatui::layout::Rect;

    // 选择器：居中圆角对话框，浮在设置列表上（opencode 的菜单样式）
    if let Some(p) = &f.picker {
        let area = fr.area();
        let inner_w = p
            .items
            .iter()
            .map(|(n, d)| UnicodeWidthStr::width(*n) + UnicodeWidthStr::width(*d) + 8)
            .max()
            .unwrap_or(24)
            .max(28) as u16;
        let w = (inner_w + 2).min(area.width.saturating_sub(2));
        let h = p.items.len() as u16 + 2;
        let rect = Rect {
            x: area.x + area.width.saturating_sub(w) / 2,
            y: area.y + area.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        };
        // Clear 左边多清一格，避免底层宽字符吃掉对话框左边框（见 tui::clear_with_margin）
        {
            let x = rect.x.saturating_sub(1);
            let width = rect
                .width
                .saturating_add(1)
                .min(fr.area().width.saturating_sub(x));
            fr.render_widget(Clear, Rect { x, width, ..rect });
        }
        let rows: Vec<ListItem> = p
            .items
            .iter()
            .map(|(name, desc)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {name}  "), Style::default()),
                    Span::styled(desc.to_string(), Style::default().fg(th.dim)),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(p.sel));
        let list = List::new(rows)
            .highlight_style(
                Style::default()
                    .bg(th.selected_bg)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("❯ ")
            .block(
                Block::default()
                    .border_type(BorderType::Rounded)
                    .borders(ratatui::widgets::Borders::ALL)
                    .title(Span::styled(
                        format!(" {} ", p.title),
                        Style::default().fg(th.accent),
                    ))
                    .border_style(Style::default().fg(th.faint))
                    .style(Style::default().bg(th.panel_bg)),
            );
        fr.render_stateful_widget(list, rect, &mut state);
        let footer = Rect {
            x: rect.x,
            y: rect.y + h,
            width: rect.width,
            height: 1,
        };
        if footer.y < area.bottom() {
            fr.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "↑↓ 选择   ⏎ 确认   Esc 取消",
                    Style::default().fg(th.faint),
                )))
                .alignment(ratatui::layout::Alignment::Center),
                footer,
            );
        }
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // 品牌
            Constraint::Length(1), // 空行
            Constraint::Min(3),    // 字段
            Constraint::Length(2), // 面板
        ])
        .split(fr.area());

    fr.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◆ ", Style::default().fg(th.accent)),
            Span::styled("dictate", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("  v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(th.faint),
            ),
            Span::styled(
                format!(
                    "  ·  设置（{}）",
                    match th.mode {
                        Mode::Dark => "深色",
                        Mode::Light => "浅色",
                    }
                ),
                Style::default().fg(th.dim),
            ),
        ])),
        chunks[0],
    );

    let items: Vec<ListItem> = FIELDS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let value = if f.editing && i == f.sel {
                format!("{}▏", f.buf)
            } else {
                f.value(i)
            };
            ListItem::new(Line::from(vec![Span::styled(
                format!(" {}{}", pad(label, 24), value),
                Style::default(),
            )]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(f.sel));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(th.selected_bg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("❯ ");
    fr.render_stateful_widget(list, chunks[2], &mut state);

    // 编辑中把光标显示在输入处：中文输入法的候选框是跟着**终端光标**走的，
    // 光标藏起来的话 fcitx 只能瞎猜位置，于是候选框飘到屏幕外面。
    if f.editing {
        let row = chunks[2].y + f.sel as u16;
        let prefix = 2 + 24 + unicode_width::UnicodeWidthStr::width(f.buf.as_str());
        let max_x = chunks[2].x + chunks[2].width.saturating_sub(2);
        fr.set_cursor_position((max_x.min(chunks[2].x + prefix as u16), row));
    }

    // 底部面板
    let panel = chunks[3];
    fr.render_widget(
        Block::default().style(Style::default().bg(th.panel_bg)),
        panel,
    );
    let inner = Rect {
        x: panel.x + 2,
        y: panel.y,
        width: panel.width.saturating_sub(4),
        height: 1,
    };
    let status = if f.status.is_empty() {
        format!("配置文件：{}", config::config_path().display())
    } else {
        f.status.clone()
    };
    fr.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("❯ ", Style::default().fg(th.accent)),
            Span::styled(status, Style::default().fg(th.dim)),
        ]))
        .style(Style::default().bg(th.panel_bg)),
        inner,
    );
    let hints = Rect {
        y: panel.y + 1,
        ..inner
    };
    fr.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ 选择   ⏎ 修改/选择   F2 或 s 保存   F3 测试   Esc 退出",
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
    fn mask_hides_middle() {
        assert_eq!(mask("sk-1234567890ab"), "sk-1…90ab");
        assert_eq!(mask("short"), "****");
    }

    #[test]
    fn model_pick_follows_model_name() {
        // 报文形状由模型名推断，不再手填
        assert_eq!(
            crate::asr::pick_style("qwen3-asr-flash", None)
                .unwrap()
                .name(),
            "openai-compatible"
        );
        assert_eq!(
            crate::asr::pick_style("qwen-audio-3.1-asr-flash-message", None)
                .unwrap()
                .name(),
            "websocket"
        );
    }

    #[test]
    fn commit_rejects_bad_values() {
        let mut f = Form::new(Config::default());
        f.sel = 7;
        f.editing = true;
        f.buf = "abc".into();
        f.commit_edit();
        // 非法值：保持编辑态让用户改，配置不变
        assert!(
            f.editing && f.status.starts_with("没改"),
            "status={}",
            f.status
        );
        assert_eq!(f.cfg.silence_stop_ms, 1500);
    }

    fn press(c: KeyCode, m: KeyModifiers) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(c, m)
    }

    #[test]
    fn keep_audio_is_toggleable_here() {
        // /retry 依赖 keep_audio，以前设置页里没有这个字段、全 UI 无开关
        let mut f = Form::new(Config::default());
        assert!(!f.cfg.keep_audio);
        f.sel = 14;
        key(&mut f, &press(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert!(f.cfg.keep_audio && f.dirty, "回车应切换并置 dirty");
    }

    #[test]
    fn picker_q_closes_picker_not_the_page() {
        let mut f = Form::new(Config::default());
        f.open_model_picker();
        let close = key(&mut f, &press(KeyCode::Char('q'), KeyModifiers::NONE)).unwrap();
        assert!(!close, "q 只关选择器，不该关掉整个设置页");
        assert!(f.picker.is_none());
    }

    #[test]
    fn readonly_shape_field_refuses_editing() {
        let mut f = Form::new(Config::default());
        f.sel = 2;
        key(&mut f, &press(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert!(!f.editing && !f.dirty, "报文形状只读，不该进编辑态");
        assert!(f.status.contains("由模型决定"));
    }
}
