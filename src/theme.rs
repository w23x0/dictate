//! 配色：从终端**实际背景色**推导一整套颜色，而不是假设黑底或白底。
//!
//! 顺序：配置 `theme`（dark/light 强制）→ 环境变量 `COLORFGBG` → OSC 11 问终端 → 默认深色。
//!
//! 为什么要背景色：面板底色、选中行底色必须"比背景略亮/略暗"，弱化文字必须是
//! "背景与前景之间"。写死一套色在酒红/绿/蓝各种主题上都会脏。

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub mode: Mode,
    pub accent: Color,
    pub dim: Color,
    pub faint: Color,
    pub ok: Color,
    pub rec: Color,
    pub warn: Color,
    pub selected_bg: Color,
    pub panel_bg: Color,
}

fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::Rgb(f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

impl Theme {
    /// 由终端背景色推导。`bg` 是 OSC 11 拿到的值。
    pub fn from_bg(bg: (u8, u8, u8)) -> Self {
        let luma = 0.2126 * bg.0 as f32 + 0.7152 * bg.1 as f32 + 0.0722 * bg.2 as f32;
        let light = luma > 127.0;
        let fg = if light { (0x1a, 0x1c, 0x20) } else { (0xf2, 0xf3, 0xf5) };
        Self {
            mode: if light { Mode::Light } else { Mode::Dark },
            accent: if light {
                Color::Rgb(0x5b, 0x3d, 0xd6)
            } else {
                Color::Rgb(0xa7, 0x8b, 0xfa)
            },
            dim: mix(bg, fg, 0.58),
            faint: mix(bg, fg, 0.34),
            ok: if light {
                Color::Rgb(0x09, 0x69, 0x3f)
            } else {
                Color::Rgb(0x4a, 0xde, 0x80)
            },
            rec: if light {
                Color::Rgb(0xb9, 0x1c, 0x1c)
            } else {
                Color::Rgb(0xff, 0x7b, 0x72)
            },
            warn: if light {
                Color::Rgb(0x92, 0x40, 0x0e)
            } else {
                Color::Rgb(0xfd, 0xba, 0x74)
            },
            // 面板/选中只挪一点点：在任意底色上都不会变成一块突兀的色块
            selected_bg: mix(bg, fg, 0.16),
            panel_bg: mix(bg, fg, 0.09),
        }
    }

    pub fn dark() -> Self {
        Self::from_bg((0x10, 0x12, 0x16))
    }

    pub fn light() -> Self {
        Self::from_bg((0xff, 0xff, 0xff))
    }

    pub fn for_mode(mode: Mode) -> Self {
        match mode {
            Mode::Dark => Self::dark(),
            Mode::Light => Self::light(),
        }
    }

    /// 配置 > 环境变量 > 问终端 > 深色。
    pub fn resolve(configured: Option<&str>) -> Self {
        match configured.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("light") => return Self::light(),
            Some("dark") => return Self::dark(),
            _ => {}
        }
        if let Some(bg) = query_terminal_background() {
            return Self::from_bg(bg);
        }
        if let Some(mode) = from_colorfgbg(std::env::var("COLORFGBG").ok().as_deref()) {
            return Self::for_mode(mode);
        }
        Self::dark()
    }
}

/// `COLORFGBG` 形如 `15;0`（fg;bg），背景 0–6/8 算深色，7/9–15 算浅色。
pub fn from_colorfgbg(v: Option<&str>) -> Option<Mode> {
    let bg = v?.rsplit(';').next()?.trim().parse::<u8>().ok()?;
    Some(if bg == 7 || bg >= 9 { Mode::Light } else { Mode::Dark })
}

/// 从 OSC 11 的回包里取背景色：`\x1b]11;rgb:3b1a/2b2b/2b2b\x1b\\`
pub fn parse_osc11_rgb(reply: &[u8]) -> Option<(u8, u8, u8)> {
    let s = String::from_utf8_lossy(reply);
    let at = s.find("]11;")? + 4;
    let spec = s[at..].split(['\x07', '\x1b']).next()?.trim();
    let rgb = spec.strip_prefix("rgb:")?;
    let mut parts = rgb.split('/');
    let take = |p: &str| -> Option<u8> {
        let hi = u16::from_str_radix(p.get(..2)?, 16).ok()?;
        Some(hi as u8)
    };
    Some((take(parts.next()?)?, take(parts.next()?)?, take(parts.next()?)?))
}

/// 亮度判定（测试用）。
#[cfg(test)]
pub fn parse_osc11(reply: &[u8]) -> Option<Mode> {
    let (r, g, b) = parse_osc11_rgb(reply)?;
    let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
    Some(if luma > 127.0 { Mode::Light } else { Mode::Dark })
}

/// 问终端背景色，最多等 150 ms；拿不到就算了。
#[cfg(unix)]
fn query_terminal_background() -> Option<(u8, u8, u8)> {
    use std::io::{Read, Write};

    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x1b\\").ok()?;
    out.flush().ok()?;

    let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    if unsafe { libc::poll(&mut pfd, 1, 150) } <= 0 {
        return None;
    }
    let mut buf = [0u8; 128];
    let n = std::io::stdin().read(&mut buf).ok()?;
    let rgb = parse_osc11_rgb(&buf[..n]);
    // 把可能残留的字节读干净，别让回包漏到 TUI 里
    while unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
        let mut junk = [0u8; 64];
        if std::io::stdin().read(&mut junk).unwrap_or(0) == 0 {
            break;
        }
        pfd.revents = 0;
    }
    rgb
}

#[cfg(not(unix))]
fn query_terminal_background() -> Option<(u8, u8, u8)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorfgbg_maps_dark_and_light() {
        assert_eq!(from_colorfgbg(Some("15;0")), Some(Mode::Dark));
        assert_eq!(from_colorfgbg(Some("0;15")), Some(Mode::Light));
        assert_eq!(from_colorfgbg(None), None);
    }

    #[test]
    fn osc11_reply_gives_bg_rgb() {
        assert_eq!(
            parse_osc11_rgb(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
            Some((255, 255, 255))
        );
        assert_eq!(parse_osc11_rgb(b"\x1b]11;rgb:3b1a/2b2b/2b2b\x07"), Some((0x3b, 0x2b, 0x2b)));
        assert_eq!(parse_osc11(b"\x1b]11;rgb:3b1a/2b2b/2b2b\x07"), Some(Mode::Dark));
    }

    #[test]
    fn wine_background_gets_its_own_panel() {
        // 酒红底（截图里的那种）不该被套成蓝灰面板
        let t = Theme::from_bg((0x3b, 0x1a, 0x2b));
        assert_eq!(t.mode, Mode::Dark);
        let bg = (0x3b, 0x1a, 0x2b);
        match (t.panel_bg, t.selected_bg) {
            (Color::Rgb(r, g, b), _) => {
                assert!(r > bg.0 && g > bg.1 && b > bg.2, "面板应比背景略亮")
            }
            _ => panic!("应是 RGB"),
        }
    }

    #[test]
    fn light_and_dark_differ() {
        assert_ne!(Theme::dark().accent, Theme::light().accent);
        assert_eq!(Theme::light().mode, Mode::Light);
    }
}
