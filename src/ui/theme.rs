//! 配色：从终端**实际背景色**推导一整套颜色，而不是假设黑底或白底。
//!
//! 顺序：配置 `theme`（dark/light 强制）→ 环境变量 `COLORFGBG` → OSC 11 问终端 → 默认深色。
//!
//! 为什么要背景色：面板底色、选中行底色必须"比背景略亮/略暗"，弱化文字必须是
//! "背景与前景之间"。写死一套色在酒红/绿/蓝各种主题上都会脏。
//!
//! 调色板借了 rune（anomalyco/rune）的语义：面板是 elevation，弱化文字有对比度下限，
//! 边框是独立 token，而不是把 `faint` 到处复用。弱化文字在**线性光**里混，避免 sRGB
//! 直接插值把中间调压暗；面板/选中/边框这种大块表面则在 sRGB 里混，否则一点点比例
//! 就会在暗色底上被线性化放大成明显的色块。

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
    /// 次要文字：对比度至少 4.5:1
    pub dim: Color,
    /// 提示/占位文字：对比度至少 3:1
    pub faint: Color,
    /// 面板边框：比面板底更清楚，但不抢文字
    pub border: Color,
    /// 焦点环/当前输入强调色
    pub focus: Color,
    pub ok: Color,
    pub rec: Color,
    pub warn: Color,
    pub selected_bg: Color,
    pub panel_bg: Color,
}

fn srgb_to_linear(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let c = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (c * 255.0).round() as u8
}

/// 在线性光里从 `a` 混到 `b`，`t = 0` 是 a，`t = 1` 是 b。
fn mix_linear(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let f = |x: u8, y: u8| {
        linear_to_srgb(srgb_to_linear(x) + (srgb_to_linear(y) - srgb_to_linear(x)) * t)
    };
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// 在 sRGB 里从 `a` 混到 `b`。适合面板/选中这类"只挪一点点"的表面色：
/// 线性混合同样比例会亮得多，暗色主题下容易变成突兀的色块。
fn mix_srgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

fn luminance(c: (u8, u8, u8)) -> f32 {
    0.2126 * srgb_to_linear(c.0) + 0.7152 * srgb_to_linear(c.1) + 0.0722 * srgb_to_linear(c.2)
}

/// WCAG 对比度（1.0–21.0）。
fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// 从 bg 往 fg 混，至少达到 `min_contrast`；fg 本身不够时就用 fg。
fn toward(bg: (u8, u8, u8), fg: (u8, u8, u8), t: f32, min_contrast: f32) -> (u8, u8, u8) {
    if contrast(bg, fg) < min_contrast {
        return fg;
    }
    let mut t = t.clamp(0.0, 1.0);
    while t < 1.0 && contrast(bg, mix_linear(bg, fg, t)) < min_contrast {
        t = (t + 0.02).min(1.0);
    }
    mix_linear(bg, fg, t)
}

#[cfg(test)]
fn rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    }
}

impl Theme {
    /// 由终端背景色推导。`bg` 是 OSC 11 拿到的值。
    pub fn from_bg(bg: (u8, u8, u8)) -> Self {
        let luma = 0.2126 * bg.0 as f32 + 0.7152 * bg.1 as f32 + 0.0722 * bg.2 as f32;
        let light = luma > 127.0;
        let fg = if light {
            (0x1a, 0x1c, 0x20)
        } else {
            (0xf2, 0xf3, 0xf5)
        };
        let accent = if light {
            (0x5b, 0x3d, 0xd6)
        } else {
            (0xa7, 0x8b, 0xfa)
        };
        Self {
            mode: if light { Mode::Light } else { Mode::Dark },
            accent: Color::Rgb(accent.0, accent.1, accent.2),
            // 弱化文字不是"随便调低透明度"：深色/浅色主题都守住可读下限
            dim: {
                let c = toward(bg, fg, 0.58, 4.5);
                Color::Rgb(c.0, c.1, c.2)
            },
            faint: {
                let c = toward(bg, fg, 0.34, 3.0);
                Color::Rgb(c.0, c.1, c.2)
            },
            border: {
                let c = mix_srgb(bg, fg, if light { 0.24 } else { 0.20 });
                Color::Rgb(c.0, c.1, c.2)
            },
            focus: Color::Rgb(accent.0, accent.1, accent.2),
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
            selected_bg: {
                let c = mix_srgb(bg, fg, 0.14);
                Color::Rgb(c.0, c.1, c.2)
            },
            panel_bg: {
                let c = mix_srgb(bg, fg, 0.07);
                Color::Rgb(c.0, c.1, c.2)
            },
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

    /// 面板底色（浮层/输入区）与背景的亮度差，测试用。
    #[cfg(test)]
    pub fn panel_contrast(&self, bg: (u8, u8, u8)) -> f32 {
        contrast(bg, rgb(self.panel_bg))
    }
}

/// `COLORFGBG` 形如 `15;0`（fg;bg），背景 0–6/8 算深色，7/9–15 算浅色。
pub fn from_colorfgbg(v: Option<&str>) -> Option<Mode> {
    let bg = v?.rsplit(';').next()?.trim().parse::<u8>().ok()?;
    Some(if bg == 7 || bg >= 9 {
        Mode::Light
    } else {
        Mode::Dark
    })
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
    Some((
        take(parts.next()?)?,
        take(parts.next()?)?,
        take(parts.next()?)?,
    ))
}

/// 亮度判定（测试用）。
#[cfg(test)]
pub fn parse_osc11(reply: &[u8]) -> Option<Mode> {
    let (r, g, b) = parse_osc11_rgb(reply)?;
    let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
    Some(if luma > 127.0 {
        Mode::Light
    } else {
        Mode::Dark
    })
}

/// 问终端背景色，最多等 150 ms；拿不到就算了。
#[cfg(unix)]
fn query_terminal_background() -> Option<(u8, u8, u8)> {
    use std::io::{Read, Write};

    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x1b\\").ok()?;
    out.flush().ok()?;

    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
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
        assert_eq!(
            parse_osc11_rgb(b"\x1b]11;rgb:3b1a/2b2b/2b2b\x07"),
            Some((0x3b, 0x2b, 0x2b))
        );
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:3b1a/2b2b/2b2b\x07"),
            Some(Mode::Dark)
        );
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

    #[test]
    fn dim_and_faint_keep_contrast_floors() {
        // 浅色主题尤其容易把提示文字调到 3:1 以下；rune 的 palette 有同样的下限
        for (bg, th) in [
            ((0x10, 0x12, 0x16), Theme::dark()),
            ((0xff, 0xff, 0xff), Theme::light()),
            ((0x3b, 0x1a, 0x2b), Theme::from_bg((0x3b, 0x1a, 0x2b))),
            ((0xf5, 0xf2, 0xe8), Theme::from_bg((0xf5, 0xf2, 0xe8))),
        ] {
            let dim = contrast(bg, rgb(th.dim));
            let faint = contrast(bg, rgb(th.faint));
            assert!(dim >= 4.49, "dim 对比度 {dim:.2} 低于 4.5:1");
            assert!(faint >= 2.99, "faint 对比度 {faint:.2} 低于 3:1");
            assert!(
                th.panel_contrast(bg) > 1.0 && th.panel_contrast(bg) < 2.0,
                "面板应可辨但不刺眼：{:.2}",
                th.panel_contrast(bg)
            );
        }
    }

    #[test]
    fn border_is_visible_without_outshouting_text() {
        for th in [Theme::dark(), Theme::light()] {
            let bg = if th.mode == Mode::Dark {
                (0x10, 0x12, 0x16)
            } else {
                (0xff, 0xff, 0xff)
            };
            let border = contrast(bg, rgb(th.border));
            assert!(border >= 1.4, "边框至少应看得见：{border:.2}");
            assert!(
                border < contrast(bg, rgb(th.dim)),
                "边框不该比次要文字更抢眼：{border:.2}"
            );
        }
    }
}
