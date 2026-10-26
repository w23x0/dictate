//! 把识别结果送到“当前拿焦点的那个窗口”。
//!
//! 策略（由强到弱，和原版 LazyTyper 一致：剪贴板 + 模拟粘贴键，失败再退化为只复制）：
//! 1. 存一份原剪贴板 → 写入新文本
//! 2. 模拟粘贴快捷键：Wayland 优先 `ydotool`（uinput）→ `wtype`，X11 用 `xdotool`。
//!    Wayland 下默认**不**用 `xdotool`：它只能打给 XWayland 窗口，却会让 Xwayland
//!    去申请 RemoteDesktop portal，GNOME 每次都会弹「Allow Remote Interaction」授权框。
//!    需要时用 `DICTATE_XDOTOOL=1` 显式打开。
//! 3. 确认自动粘贴成功后，400 ms 把原剪贴板还回去（不污染用户的剪贴板）；
//!    没粘上（或 clipboard 模式）时保留新文本——用户还要手动 Ctrl+V，不能盖回去
//! 4. 一个都不可用时只留剪贴板，并明确告诉用户“按 Ctrl+V”

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 只写剪贴板，不模拟按键
    Clipboard,
    /// 剪贴板 + 模拟粘贴
    Auto,
    /// 什么都不做（只进历史）
    Off,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "history" => Mode::Off,
            "clipboard" | "copy" => Mode::Clipboard,
            _ => Mode::Auto,
        }
    }
    #[allow(dead_code)] // 设置页/诊断会用
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Clipboard => "clipboard",
            Mode::Auto => "auto",
        }
    }
}

/// 实现方式，返回给状态栏显示。
pub fn deliver(text: &str, mode: Mode) -> Result<String> {
    if text.trim().is_empty() {
        bail!("空文本，不粘贴");
    }
    if mode == Mode::Off {
        return Ok("只进历史".into());
    }

    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();

    // 1) 先保存原剪贴板（拿得到才恢复）
    let previous = read_clipboard();

    if !write_clipboard(text) {
        bail!("写剪贴板失败（xclip/wl-copy 都没有？）");
    }

    // 2) 模拟粘贴
    let mut pasted = false;
    let mut how = "已复制到剪贴板（按 Ctrl+V 粘贴）".to_string();
    if mode == Mode::Auto {
        match send_paste(wayland) {
            Some("xdotool") if wayland => {
                pasted = true;
                how = "已复制到剪贴板，并尝试过 xdotool（只对 XWayland 窗口有效）——没粘贴上就按 Ctrl+V"
                    .into();
            }
            Some(tool) => {
                pasted = true;
                how = format!("已粘贴（{tool}）");
            }
            None => {
                how = if wayland {
                    "已复制到剪贴板；Wayland 下自动粘贴需要 ydotool（推荐，apt install ydotool 后 \
                     systemctl --user enable --now ydotool）或 wtype——按 Ctrl+V 也可以"
                        .into()
                } else {
                    "已复制到剪贴板；没找到 xdotool，按 Ctrl+V".into()
                }
            }
        }
    }

    // 3) 还回剪贴板（延迟，等粘贴动作读完）。只在粘贴成功时做：
    //    目标窗口已经把文本读走了；否则用户接下来要手动 Ctrl+V，还原等于把旧内容盖回去。
    if pasted {
        if let Some(prev) = previous {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(400));
                if !prev.is_empty() {
                    write_clipboard(&prev);
                }
            });
        }
    }
    Ok(how)
}

fn which(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn read_clipboard() -> Option<String> {
    for (cmd, args) in [
        ("wl-paste", vec!["--no-newline"]),
        ("xclip", vec!["-selection", "clipboard", "-o"]),
        ("xsel", vec!["--clipboard", "--output"]),
    ] {
        if !which(cmd) {
            continue;
        }
        if let Ok(out) = Command::new(cmd).args(&args).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn write_clipboard(text: &str) -> bool {
    for (cmd, args) in [
        ("wl-copy", vec![]),
        ("xclip", vec!["-selection", "clipboard"]),
        ("xsel", vec!["--clipboard", "--input"]),
    ] {
        if !which(cmd) {
            continue;
        }
        let child = Command::new(cmd)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut c) = child {
            if let Some(stdin) = c.stdin.as_mut() {
                if stdin.write_all(text.as_bytes()).is_err() {
                    continue;
                }
            }
            if c.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// Wayland 下才允许 `xdotool`（默认关）：它会触发 Xwayland 的 RemoteDesktop 授权框。
const PASTE_XDOTOOL_ENV: &str = "DICTATE_XDOTOOL";

const XDOTOOL: (&str, &[&str]) = ("xdotool", &["key", "--clearmodifiers", "ctrl+v"]);
const YDOTOOL: (&str, &[&str]) = ("ydotool", &["key", "29:1", "47:1", "47:0", "29:0"]);
const WTYPE: (&str, &[&str]) = ("wtype", &["-M", "ctrl", "v", "-m", "ctrl"]);

/// 按优先级列出要尝试的粘贴工具。
///
/// Wayland：`ydotool`（uinput，直连内核，不经 portal）→ `wtype`（需要合成器支持
/// virtual-keyboard 协议，GNOME 不支持）→ `xdotool`（仅当显式开启：它是 X11 客户端，
/// 打不进原生 Wayland 窗口，还会让 Xwayland 申请 RemoteDesktop 权限并弹系统授权框）。
fn attempts(wayland: bool, allow_xdotool: bool) -> Vec<(&'static str, &'static [&'static str])> {
    if wayland {
        let mut list = vec![YDOTOOL, WTYPE];
        if allow_xdotool {
            list.push(XDOTOOL);
        }
        list
    } else {
        vec![XDOTOOL, YDOTOOL, WTYPE]
    }
}

/// 返回用了哪个工具；都不可用返回 None。
fn send_paste(wayland: bool) -> Option<&'static str> {
    let attempts = attempts(wayland, std::env::var_os(PASTE_XDOTOOL_ENV).is_some());
    for (tool, args) in attempts {
        if !which(tool) {
            continue;
        }
        if let Ok(status) = Command::new(tool)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            if status.success() {
                return Some(tool);
            }
        }
    }
    None
}

/// 有哪些粘贴工具可用（给 `dictate doctor` 和设置页显示）。
pub fn capabilities() -> String {
    let mut have = Vec::new();
    for t in ["wl-copy", "xclip", "xsel", "wtype", "ydotool", "xdotool"] {
        if which(t) {
            have.push(t);
        }
    }
    if have.is_empty() {
        "没有可用的剪贴板/按键工具（装 xclip 或 wl-clipboard）".into()
    } else {
        have.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing() {
        assert_eq!(Mode::parse("off"), Mode::Off);
        assert_eq!(Mode::parse("clipboard"), Mode::Clipboard);
        assert_eq!(Mode::parse("auto"), Mode::Auto);
        assert_eq!(Mode::parse("whatever"), Mode::Auto);
    }

    #[test]
    fn empty_text_is_rejected() {
        assert!(deliver("   ", Mode::Auto).is_err());
    }

    #[test]
    fn wayland_paste_prefers_ydotool_and_skips_xdotool() {
        let tools: Vec<&str> = attempts(true, false).iter().map(|(t, _)| *t).collect();
        assert_eq!(tools, ["ydotool", "wtype"]);
    }

    #[test]
    fn wayland_xdotool_needs_explicit_opt_in() {
        let tools: Vec<&str> = attempts(true, true).iter().map(|(t, _)| *t).collect();
        assert_eq!(tools, ["ydotool", "wtype", "xdotool"]);
    }

    #[test]
    fn x11_paste_starts_with_xdotool() {
        let tools: Vec<&str> = attempts(false, false).iter().map(|(t, _)| *t).collect();
        assert_eq!(tools[0], "xdotool");
    }
}
