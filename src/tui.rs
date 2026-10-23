//! 历史窗口（终端 TUI）。只做四件事：看、搜、复制、删。

use std::io;

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
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

use crate::history::{Entry, History};

struct App {
    history: History,
    all: Vec<Entry>,
    shown: Vec<usize>,
    query: String,
    searching: bool,
    selected: usize,
    status: String,
}

impl App {
    fn new(history: History) -> Self {
        let mut app = Self {
            history,
            all: Vec::new(),
            shown: Vec::new(),
            query: String::new(),
            searching: false,
            selected: 0,
            status: String::new(),
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
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;

    let mut app = App::new(history);
    let res = event_loop(&mut term, &mut app);

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

fn event_loop(term: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        term.draw(|f| draw(f, app))?;
        if let Event::Key(k) = event::read()? {
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
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    let title = if app.searching {
        Line::from(vec![
            Span::styled("搜: ", Style::default().fg(Color::Yellow)),
            Span::raw(app.query.clone()),
            Span::raw("▏"),
        ])
    } else {
        Line::from(Span::styled(
            format!(
                "dictate — {} 条{}",
                app.shown.len(),
                if app.query.is_empty() {
                    String::new()
                } else {
                    format!("（筛选：{}）", app.query)
                }
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ))
    };
    f.render_widget(Paragraph::new(title), chunks[0]);

    let items: Vec<ListItem> = app
        .shown
        .iter()
        .map(|i| {
            let e = &app.all[*i];
            let time = e.ts.get(11..19).unwrap_or("");
            let color = if e.is_ok() { Color::Reset } else { Color::Red };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{time} "), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:>5.1}s ", e.duration_ms as f32 / 1000.0),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(e.text.replace('\n', " "), Style::default().fg(color)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("历史"))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut state = ListState::default();
    if !app.shown.is_empty() {
        state.select(Some(app.selected));
    }
    f.render_stateful_widget(list, chunks[1], &mut state);

    let help = if app.status.is_empty() {
        "↑↓/jk 选择   Enter 复制   / 搜索   d 删除   c 清空   r 刷新   q 退出".to_string()
    } else {
        app.status.clone()
    };
    f.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
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
}
