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
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

use unicode_width::UnicodeWidthStr;

use crate::config::{self, Config};
use crate::theme::{Mode, Theme};

/// 按**显示宽度**补齐（中文占两格），否则中英混排的列会歪。
fn pad(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

const FIELDS: [&str; 11] = [
    "服务地址",
    "模型",
    "报文形状",
    "API key",
    "语言",
    "ITN（数字规范化）",
    "热键",
    "静音自动结束 (ms)",
    "单次上限 (s)",
    "超时 (s)",
    "配色 (auto/dark/light)",
];

struct Form {
    cfg: Config,
    sel: usize,
    editing: bool,
    buf: String,
    status: String,
    dirty: bool,
    test: Option<Receiver<Result<(u64, String), String>>>,
    theme: Theme,
}

impl Form {
    fn new(cfg: Config) -> Self {
        Self {
            cfg,
            sel: 0,
            editing: false,
            buf: String::new(),
            status: String::new(),
            dirty: false,
            test: None,
            theme: Theme::dark(),
        }
    }

    fn value(&self, i: usize) -> String {
        match i {
            0 => self.cfg.base_url.clone(),
            1 => self.cfg.model.clone(),
            2 => self
                .cfg
                .api_style
                .clone()
                .unwrap_or_else(|| "auto（按模型名）".into()),
            3 => match &self.cfg.api_key {
                Some(k) if !k.is_empty() => mask(k),
                _ => "（未设置，可用 DASHSCOPE_API_KEY）".into(),
            },
            4 => self.cfg.language.clone().unwrap_or_else(|| "auto".into()),
            5 => if self.cfg.enable_itn { "开" } else { "关" }.into(),
            6 => self.cfg.hotkey.clone(),
            7 => self.cfg.silence_stop_ms.to_string(),
            8 => format!("{}", self.cfg.max_seconds),
            9 => self.cfg.timeout_secs.to_string(),
            10 => self
                .cfg
                .theme
                .clone()
                .unwrap_or_else(|| "auto（问终端）".into()),
            _ => String::new(),
        }
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
            self.cycle_style(1);
            return;
        }
        if i == 10 {
            self.cycle_theme(1);
            return;
        }
        self.editing = true;
        self.buf = match i {
            0 => self.cfg.base_url.clone(),
            1 => self.cfg.model.clone(),
            3 => self.cfg.api_key.clone().unwrap_or_default(),
            4 => self.cfg.language.clone().unwrap_or_default(),
            6 => self.cfg.hotkey.clone(),
            7 => self.cfg.silence_stop_ms.to_string(),
            8 => format!("{}", self.cfg.max_seconds),
            9 => self.cfg.timeout_secs.to_string(),
            _ => String::new(),
        };
    }

    fn cycle_style(&mut self, dir: i32) {
        let order = [None, Some("openai"), Some("dashscope"), Some("ws")];
        let cur = order
            .iter()
            .position(|s| s.map(|x| x.to_string()) == self.cfg.api_style)
            .unwrap_or(0) as i32;
        let next = ((cur + dir).rem_euclid(order.len() as i32)) as usize;
        self.cfg.api_style = order[next].map(|s| s.to_string());
        self.dirty = true;
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
                4 => self.cfg.language = if v.is_empty() || v == "auto" { None } else { Some(v.clone()) },
                6 => {
                    crate::hotkey::Spec::parse(&v)?;
                    self.cfg.hotkey = v.clone();
                }
                7 => self.cfg.silence_stop_ms = v.parse().map_err(|_| anyhow::anyhow!("要是整数 ms"))?,
                8 => self.cfg.max_seconds = v.parse().map_err(|_| anyhow::anyhow!("要是数字秒"))?,
                9 => self.cfg.timeout_secs = v.parse().map_err(|_| anyhow::anyhow!("要是整数秒"))?,
                _ => {}
            }
            Ok(())
        })();
        match res {
            Ok(()) => {
                self.dirty = true;
                self.editing = false;
                self.status = "已修改（按 s 保存）".into();
            }
            Err(e) => self.status = format!("没改：{e:#}"),
        }
    }

    fn save(&mut self) -> Result<()> {
        crate::asr::pick_style(&self.cfg.model, self.cfg.api_style.as_deref())?;
        let path = config::config_path();
        config::write(&path, &self.cfg)?;
        self.dirty = false;
        self.status = format!("已保存到 {}", path.display());
        Ok(())
    }

    fn run_test(&mut self) {
        if self.test.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            let out = crate::pipeline::self_test(&cfg).map_err(|e| format!("{e:#}"));
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
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }

        if f.editing {
            match k.code {
                KeyCode::Esc => {
                    f.editing = false;
                    f.status = "已取消".into();
                }
                KeyCode::Enter => f.commit_edit(),
                KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    f.buf.clear()
                }
                KeyCode::Backspace => {
                    f.buf.pop();
                }
                KeyCode::Char(c) => f.buf.push(c),
                _ => {}
            }
            continue;
        }

        let plain = k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT;
        match k.code {
            KeyCode::Esc => return Ok(()),
            KeyCode::Char('q') if plain => {
                if f.dirty {
                    f.status = "有未保存的修改：按 s 保存，或再按 q 放弃".into();
                    f.dirty = false;
                } else {
                    return Ok(());
                }
            }
            KeyCode::Up | KeyCode::Char('k') if plain => f.sel = f.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') if plain => {
                if f.sel + 1 < FIELDS.len() {
                    f.sel += 1;
                }
            }
            KeyCode::Enter => f.start_edit(),
            KeyCode::Left if f.sel == 2 => f.cycle_style(-1),
            KeyCode::Right if f.sel == 2 => f.cycle_style(1),
            KeyCode::Left if f.sel == 10 => f.cycle_theme(-1),
            KeyCode::Right if f.sel == 10 => f.cycle_theme(1),
            KeyCode::Char(' ') if f.sel == 5 => {
                f.cfg.enable_itn = !f.cfg.enable_itn;
                f.dirty = true;
            }
            KeyCode::Char('s') if plain => f.save()?,
            KeyCode::Char('t') if plain => f.run_test(),
            _ => {}
        }
    }
}

fn draw(fr: &mut ratatui::Frame, f: &Form) {
    let theme = f.theme;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(fr.area());
    fr.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◆ ", Style::default().fg(theme.accent)),
            Span::styled("dictate", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("  v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(theme.faint),
            ),
            Span::styled(
                format!(
                    "  ·  设置（{}）",
                    match theme.mode {
                        Mode::Dark => "深色",
                        Mode::Light => "浅色",
                    }
                ),
                Style::default().fg(theme.dim),
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
            ListItem::new(Line::from(vec![
                Span::styled(
                    pad(label, 24),
                    Style::default().fg(theme.dim),
                ),
                Span::styled(value, Style::default()),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(f.sel));
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(Style::default().fg(theme.faint))
                .title(Span::styled(
                    " 设置 ",
                    Style::default().fg(theme.dim),
                ))
                .padding(ratatui::widgets::Padding::horizontal(1)),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selected_bg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("❯ ");
    fr.render_stateful_widget(list, chunks[1], &mut state);

    let help =
        "↑↓ 选择   Enter 修改（编辑中 Ctrl+U 清空）   ←→ 切报文形状   s 保存   t 测试   q 退出";
    let status = if f.status.is_empty() {
        format!("配置文件：{}", config::config_path().display())
    } else {
        f.status.clone()
    };
    fr.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(help, Style::default().fg(theme.faint))),
            Line::from(Span::styled(status, Style::default().fg(theme.ok))),
        ]),
        chunks[2],
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
    fn style_cycles_through_all_values() {
        let mut f = Form::new(Config::default());
        f.sel = 2;
        f.cycle_style(1);
        assert_eq!(f.cfg.api_style.as_deref(), Some("openai"));
        f.cycle_style(1);
        assert_eq!(f.cfg.api_style.as_deref(), Some("dashscope"));
        f.cycle_style(-1);
        assert_eq!(f.cfg.api_style.as_deref(), Some("openai"));
    }

    #[test]
    fn commit_rejects_bad_values() {
        let mut f = Form::new(Config::default());
        f.sel = 7;
        f.editing = true;
        f.buf = "abc".into();
        f.commit_edit();
        // 非法值：保持编辑态让用户改，配置不变
        assert!(f.editing && f.status.starts_with("没改"), "status={}", f.status);
        assert_eq!(f.cfg.silence_stop_ms, 1500);
    }
}
