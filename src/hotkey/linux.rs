//! Linux 热键：直接读 `/dev/input/event*`。
//!
//! 为什么不是 XGrabKey / 桌面门户：`XGrabKey` 在 Wayland 下不存在，门户的
//! GlobalShortcuts 只给「按下」不给「松开」——而按住说话必须要松开。
//! evdev 是唯一在 X11 / Wayland 都成立、且能拿到释放事件的路子，代价是 `input` 组权限。

use std::sync::mpsc::Sender;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use evdev::{Device, EventType};

use super::{HotkeyEvent, Key, Spec};

// Linux 输入子系统的键码是稳定 ABI，直接写数字比跟着 crate 的常量走更不容易漂。
const KEY_ESC: u16 = 1;
const KEY_1: u16 = 2;
const KEY_0: u16 = 11;
const KEY_TAB: u16 = 15;
const KEY_ENTER: u16 = 28;
const KEY_LEFTCTRL: u16 = 29;
// 字母键在 Linux 输入子系统里**不是连续**的（B 是 48、C 是 46…），只能查表。
const LETTER_CODES: [u16; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
    45, 21, 44,
];
const KEY_LEFTSHIFT: u16 = 42;
const KEY_LEFTALT: u16 = 56;
const KEY_SPACE: u16 = 57;
const KEY_F1: u16 = 59;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;

fn code_for(key: Key) -> u16 {
    match key {
        Key::Char(c) => LETTER_CODES[(c as u8 - b'a') as usize],
        Key::Digit(d) => {
            if d == 0 {
                KEY_0
            } else {
                KEY_1 + (d as u16 - 1)
            }
        }
        Key::F(n) => KEY_F1 + (n as u16 - 1),
        Key::Space => KEY_SPACE,
        Key::Enter => KEY_ENTER,
        Key::Tab => KEY_TAB,
        Key::Escape => KEY_ESC,
    }
}

#[derive(Default, Clone, Copy)]
struct Mods {
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
}

impl Mods {
    fn update(&mut self, code: u16, down: bool) {
        match code {
            KEY_LEFTCTRL | KEY_RIGHTCTRL => self.ctrl = down,
            KEY_LEFTALT | KEY_RIGHTALT => self.alt = down,
            KEY_LEFTSHIFT => self.shift = down,
            KEY_LEFTMETA | KEY_RIGHTMETA => self.meta = down,
            _ => {}
        }
    }
    fn matches(&self, s: &Spec) -> bool {
        self.ctrl == s.ctrl && self.alt == s.alt && self.shift == s.shift && self.meta == s.meta
    }
}

pub fn spawn(spec: Spec, tx: Sender<HotkeyEvent>) -> Result<()> {
    let paths = device_paths();
    if paths.is_empty() {
        return Err(anyhow!("找不到 /dev/input/event*"));
    }
    let tx = Arc::new(tx);
    let mut opened = 0usize;
    let mut last_err: Option<String> = None;

    for path in paths {
        let dev = match Device::open(&path) {
            Ok(d) => d,
            Err(e) => {
                last_err = Some(format!("{}：{e}", path.display()));
                continue;
            }
        };
        // 只跟有按键能力的设备（跳过纯鼠标/电源键以外的东西）
        if dev.supported_keys().is_none() {
            continue;
        }
        opened += 1;
        let name = dev.name().unwrap_or("?").to_string();
        eprintln!("[hotkey] 监听 {name}（{}）", path.display());
        let tx = Arc::clone(&tx);
        std::thread::spawn(move || watch(dev, spec, tx));
    }

    if opened == 0 {
        let hint = last_err.unwrap_or_else(|| "没有可读的键盘设备".into());
        return Err(anyhow!(
            "读不到任何输入设备（{hint}）。把用户加进 input 组后重新登录：\n  \
             sudo usermod -aG input $USER"
        ));
    }
    Ok(())
}

fn device_paths() -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir("/dev/input")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("event"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn watch(mut dev: Device, spec: Spec, tx: Arc<Sender<HotkeyEvent>>) {
    let target = code_for(spec.key);
    let mut mods = Mods::default();
    let mut down = false;
    loop {
        let events = match dev.fetch_events() {
            Ok(e) => e,
            Err(_) => return, // 设备拔了
        };
        for ev in events {
            if ev.event_type() != EventType::KEY {
                continue;
            }
            let code = ev.code();
            let value = ev.value();
            if value == 2 {
                continue; // 自动重复
            }
            let pressed = value == 1;
            mods.update(code, pressed);
            if code == target {
                if pressed && !down && mods.matches(&spec) {
                    down = true;
                    let _ = tx.send(HotkeyEvent::Press);
                } else if !pressed && down {
                    down = false;
                    let _ = tx.send(HotkeyEvent::Release);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycodes_match_linux_abi() {
        assert_eq!(code_for(Key::Char('a')), 30);
        assert_eq!(code_for(Key::Char('z')), 44);
        assert_eq!(code_for(Key::Digit(1)), 2);
        assert_eq!(code_for(Key::Digit(0)), 11);
        assert_eq!(code_for(Key::Space), 57);
        assert_eq!(code_for(Key::F(1)), 59);
        assert_eq!(code_for(Key::F(10)), 68);
    }

    #[test]
    fn modifier_tracking_and_match() {
        let spec = Spec::parse("ctrl+space").unwrap();
        let mut m = Mods::default();
        assert!(!m.matches(&spec));
        m.update(KEY_LEFTCTRL, true);
        assert!(m.matches(&spec));
        m.update(KEY_LEFTSHIFT, true);
        assert!(!m.matches(&spec), "多按了 shift 就不该触发");
        m.update(KEY_LEFTSHIFT, false);
        m.update(KEY_LEFTCTRL, false);
        assert!(!m.matches(&spec));
    }
}
