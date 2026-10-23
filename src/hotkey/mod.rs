//! 全局热键：按下开始录音，松开提交。
//!
//! 两端实现不同，语义相同：
//! * Linux 读 `/dev/input/event*`（evdev）——Wayland / X11 都成立，需要 `input` 组。
//! * Windows 用 `WH_KEYBOARD_LL` 低级钩子——`RegisterHotKey` 只有按下、拿不到松开。

use std::sync::mpsc::Sender;

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Press,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Digit(u8),
    F(u8),
    Space,
    Enter,
    Tab,
    Escape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub key: Key,
}

impl Spec {
    /// `ctrl+space` / `f9` / `alt+shift+d` / `super+k`
    pub fn parse(text: &str) -> Result<Self> {
        let mut spec = Spec { ctrl: false, alt: false, shift: false, meta: false, key: Key::Space };
        let mut key: Option<Key> = None;
        for raw in text.split('+') {
            let part = raw.trim().to_ascii_lowercase();
            if part.is_empty() {
                continue;
            }
            match part.as_str() {
                "ctrl" | "control" => spec.ctrl = true,
                "alt" | "option" => spec.alt = true,
                "shift" => spec.shift = true,
                "super" | "meta" | "win" | "cmd" => spec.meta = true,
                "space" => key = Some(Key::Space),
                "enter" | "return" => key = Some(Key::Enter),
                "tab" => key = Some(Key::Tab),
                "esc" | "escape" => key = Some(Key::Escape),
                other => {
                    let mut chars = other.chars();
                    let c = chars.next().unwrap();
                    if chars.next().is_some() {
                        // f1..f12
                        if let Some(n) = other.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                            if (1..=12).contains(&n) {
                                key = Some(Key::F(n));
                                continue;
                            }
                        }
                        bail!("看不懂的按键：{other}");
                    }
                    key = Some(if c.is_ascii_digit() {
                        Key::Digit(c.to_digit(10).unwrap() as u8)
                    } else if c.is_ascii_alphabetic() {
                        Key::Char(c.to_ascii_lowercase())
                    } else {
                        bail!("不支持的键：{c}");
                    });
                }
            }
        }
        spec.key = key.ok_or_else(|| anyhow::anyhow!("热键里没有主键：{text}"))?;
        Ok(spec)
    }
}

/// 起监听线程；事件通过 `tx` 送出。返回的句柄被丢弃时线程仍继续（进程退出即结束）。
pub fn spawn(spec: Spec, tx: Sender<HotkeyEvent>, verbose: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::spawn(spec, tx, verbose)
    }
    #[cfg(windows)]
    {
        windows::spawn(spec, tx)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (spec, tx, verbose);
        bail!("这个平台还没有热键实现")
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_combinations() {
        assert_eq!(
            Spec::parse("ctrl+space").unwrap(),
            Spec { ctrl: true, alt: false, shift: false, meta: false, key: Key::Space }
        );
        assert_eq!(Spec::parse("F9").unwrap().key, Key::F(9));
        let s = Spec::parse("alt+shift+d").unwrap();
        assert!(s.alt && s.shift && !s.ctrl);
        assert_eq!(s.key, Key::Char('d'));
        assert_eq!(Spec::parse("super+k").unwrap().meta, true);
        assert_eq!(Spec::parse("7").unwrap().key, Key::Digit(7));
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(Spec::parse("ctrl").is_err());
        assert!(Spec::parse("ctrl+nope").is_err());
        assert!(Spec::parse("f13").is_err());
    }
}
