//! 配色：按终端背景亮/暗选一套，**不再假设深色背景**。
//!
//! 检测顺序：配置 `theme` → 环境变量 `COLORFGBG` → OSC 11 问终端 → 默认深色。

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
    /// 底部输入面板的底色（grok-cli 那种"填充块"而不是描边）
    pub panel_bg: Color,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            mode: Mode::Dark,
            accent: Color::Rgb(0x8b, 0x7d, 0xff),
            dim: Color::Rgb(0x6b, 0x72, 0x80),
            faint: Color::Rgb(0x4b, 0x51, 0x5c),
            ok: Color::Rgb(0x34, 0xd3, 0x99),
            rec: Color::Rgb(0xf8, 0x71, 0x71),
            warn: Color::Rgb(0xfb, 0xbf, 0x24),
            selected_bg: Color::Rgb(0x26, 0x2a, 0x33),
            panel_bg: Color::Rgb(0x1a, 0x1d, 0x23),
        }
    }

    pub fn light() -> Self {
        Self {
            mode: Mode::Light,
            accent: Color::Rgb(0x5b, 0x3d, 0xd6),
            dim: Color::Rgb(0x5b, 0x61, 0x6b),
            faint: Color::Rgb(0x9a, 0xa1, 0xac),
            ok: Color::Rgb(0x15, 0x80, 0x3d),
            rec: Color::Rgb(0xb9, 0x1c, 0x1c),
            warn: Color::Rgb(0x9a, 0x5b, 0x00),
            selected_bg: Color::Rgb(0xdf, 0xe3, 0xe8),
            panel_bg: Color::Rgb(0xec, 0xee, 0xf1),
        }
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
        if let Some(mode) = from_colorfgbg(std::env::var("COLORFGBG").ok().as_deref()) {
            return Self::for_mode(mode);
        }
        if let Some(mode) = query_terminal_background() {
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

/// 从 OSC 11 的回包里取亮度：`\x1b]11;rgb:ffff/ffff/ffff\x1b\\`
pub fn parse_osc11(reply: &[u8]) -> Option<Mode> {
    let s = String::from_utf8_lossy(reply);
    let at = s.find("]11;")? + 4;
    let rest = &s[at..];
    let spec = rest.split(['\x07', '\x1b']).next()?.trim();
    let rgb = spec.strip_prefix("rgb:")?;
    let mut parts = rgb.split('/');
    let r = u16::from_str_radix(parts.next()?.get(..2)?, 16).ok()? as f32;
    let g = u16::from_str_radix(parts.next()?.get(..2)?, 16).ok()? as f32;
    let b = u16::from_str_radix(parts.next()?.get(..2)?, 16).ok()? as f32;
    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    Some(if luma > 127.0 { Mode::Light } else { Mode::Dark })
}

/// 问终端背景色，最多等 150 ms；拿不到就算了。
#[cfg(unix)]
fn query_terminal_background() -> Option<Mode> {
    use std::io::{Read, Write};

    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x1b\\").ok()?;
    out.flush().ok()?;

    let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    let ready = unsafe { libc::poll(&mut pfd, 1, 150) };
    if ready <= 0 {
        return None;
    }
    let mut buf = [0u8; 128];
    let n = std::io::stdin().read(&mut buf).ok()?;
    let mode = parse_osc11(&buf[..n]);
    // 把可能残留的字节读干净，别让回包漏到 TUI 里
    while unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
        let mut junk = [0u8; 64];
        if std::io::stdin().read(&mut junk).unwrap_or(0) == 0 {
            break;
        }
        pfd.revents = 0;
    }
    mode
}

#[cfg(not(unix))]
fn query_terminal_background() -> Option<Mode> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorfgbg_maps_dark_and_light() {
        assert_eq!(from_colorfgbg(Some("15;0")), Some(Mode::Dark));
        assert_eq!(from_colorfgbg(Some("0;15")), Some(Mode::Light));
        assert_eq!(from_colorfgbg(Some("0;7")), Some(Mode::Light));
        assert_eq!(from_colorfgbg(None), None);
        assert_eq!(from_colorfgbg(Some("garbage")), None);
    }

    #[test]
    fn osc11_reply_gives_luminance() {
        assert_eq!(parse_osc11(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"), Some(Mode::Light));
        assert_eq!(parse_osc11(b"\x1b]11;rgb:0000/0000/0000\x07"), Some(Mode::Dark));
        assert_eq!(parse_osc11(b"nonsense"), None);
    }

    #[test]
    fn configured_theme_wins() {
        assert_eq!(Theme::resolve(Some("light")).mode, Mode::Light);
        assert_eq!(Theme::resolve(Some("dark")).mode, Mode::Dark);
    }
}
