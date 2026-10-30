//! 历史列表的渲染器：只画视口里的行，不克隆整份列表。
//!
//! 借鉴 rune 的 virtual rows 和 opentui 的 ScrollBox：数据变化时构建行，
//! 每帧只把 `scroll..scroll+height` 这一段交给 ratatui。选中态也不再写进
//! 行内容，而是渲染时叠底色 + 左侧 `▌`，所以 j/k 不再触发整表重建。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::ui::theme::Theme;

/// 左侧选择标记占 1 列，行内容从 `area.x + 1` 开始。
pub const GUTTER: u16 = 1;
/// 行内容前缀（时间 + ❯）的显示宽度，折行续行按它缩进。
pub const ROW_PREFIX: usize = 10;

pub struct HistoryList<'a> {
    pub lines: &'a [Line<'static>],
    /// 每个历史条目第一行在 `lines` 里的下标；升序。
    pub first_of: &'a [usize],
    pub selected: usize,
    pub theme: Theme,
    pub scroll: &'a mut usize,
}

impl Widget for HistoryList<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || self.lines.is_empty() || self.first_of.is_empty() {
            *self.scroll = 0;
            return;
        }
        let view_h = area.height as usize;
        let sel = self.selected.min(self.first_of.len() - 1);
        let sel_start = self.first_of[sel];
        let sel_end = self
            .first_of
            .get(sel + 1)
            .copied()
            .unwrap_or(self.lines.len());
        let sel_h = sel_end.saturating_sub(sel_start).max(1);

        // 选中项完整可见：先保证头在视口内，再保证尾在视口内。
        let mut scroll = (*self.scroll).min(self.lines.len().saturating_sub(1));
        if sel_start < scroll {
            scroll = sel_start;
        }
        if sel_start + sel_h > scroll + view_h {
            scroll = (sel_start + sel_h).saturating_sub(view_h);
        }
        scroll = scroll.min(self.lines.len().saturating_sub(view_h));
        *self.scroll = scroll;

        let content_w = area.width.saturating_sub(GUTTER);
        for row in 0..view_h {
            let line_idx = scroll + row;
            if line_idx >= self.lines.len() {
                break;
            }
            let y = area.y + row as u16;
            let selected_row = line_idx >= sel_start && line_idx < sel_end;
            if selected_row {
                buf.set_style(
                    Rect {
                        x: area.x,
                        y,
                        width: area.width,
                        height: 1,
                    },
                    Style::default().bg(self.theme.selected_bg),
                );
            }
            buf.set_line(area.x + GUTTER, y, &self.lines[line_idx], content_w);
        }

        // 标记画在选中条目第一行的 gutter 里；多行条目也只画一次。
        if sel_start >= scroll && sel_start < scroll + view_h {
            let y = area.y + (sel_start - scroll) as u16;
            buf.set_stringn(area.x, y, "▌", 1, Style::default().fg(self.theme.focus));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::text::Span;

    fn line(s: &str) -> Line<'static> {
        Line::from(Span::raw(s.to_string()))
    }

    fn render(
        lines: &[Line<'static>],
        first_of: &[usize],
        selected: usize,
        h: u16,
    ) -> (String, usize) {
        let area = Rect::new(0, 0, 20, h);
        let mut buf = Buffer::empty(area);
        let mut scroll = 0;
        HistoryList {
            lines,
            first_of,
            selected,
            theme: Theme::dark(),
            scroll: &mut scroll,
        }
        .render(area, &mut buf);
        let mut out = String::new();
        for y in 0..h {
            for x in 0..20 {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        (out, scroll)
    }

    #[test]
    fn only_the_viewport_is_drawn() {
        let lines: Vec<_> = (0..50).map(|i| line(&format!("row {i}"))).collect();
        let first_of: Vec<_> = (0..50).collect();
        let (out, scroll) = render(&lines, &first_of, 0, 5);
        assert_eq!(scroll, 0);
        assert!(out.contains("row 0") && out.contains("row 4"));
        assert!(!out.contains("row 5"), "视口外的行不该画：{out}");
    }

    #[test]
    fn selected_item_scrolls_into_view() {
        let lines: Vec<_> = (0..20).map(|i| line(&format!("row {i}"))).collect();
        let first_of: Vec<_> = (0..20).collect();
        let (out, scroll) = render(&lines, &first_of, 18, 5);
        assert_eq!(scroll, 14);
        assert!(out.contains("row 18"), "{out}");
        assert!(!out.contains("row 13"), "{out}");
    }

    #[test]
    fn multi_line_selection_keeps_the_tail_visible() {
        // 条目 0 占 3 行，条目 1 占 1 行；视口 3 行，选中 0 时应显示它的 3 行
        let lines: Vec<_> = (0..4).map(|i| line(&format!("l{i}"))).collect();
        let first_of = vec![0, 3];
        let (out, scroll) = render(&lines, &first_of, 0, 3);
        assert_eq!(scroll, 0);
        assert!(out.contains("l0") && out.contains("l1") && out.contains("l2"));
        // 选中第二项时滚到它
        let (out, scroll) = render(&lines, &first_of, 1, 3);
        assert_eq!(scroll, 1);
        assert!(out.contains("l3"), "{out}");
    }

    #[test]
    fn marker_uses_the_gutter_not_the_text() {
        let lines = vec![line(" 10:00  hello")];
        let first_of = vec![0];
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let mut scroll = 0;
        HistoryList {
            lines: &lines,
            first_of: &first_of,
            selected: 0,
            theme: Theme::dark(),
            scroll: &mut scroll,
        }
        .render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "▌");
        assert_eq!(buf[(0, 0)].fg, Theme::dark().focus);
        assert_eq!(buf[(1, 0)].symbol(), " ");
        // 选中底色铺满整行
        assert_eq!(buf[(19, 0)].bg, Theme::dark().selected_bg);
    }
}
