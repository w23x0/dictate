//! 桌面集成：.desktop 入口 + 图标，让 `dictate` 能被应用搜索找到。

use std::path::PathBuf;

use anyhow::Result;

pub fn applications_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("applications"))
}

pub fn icon_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("icons/hicolor/64x64/apps"))
}

pub fn desktop_path() -> Option<PathBuf> {
    Some(applications_dir()?.join("dictate.desktop"))
}

/// 写（或更新）`.desktop` 与图标。exe 路径用当前可执行文件的绝对路径。
pub fn ensure_installed() -> Result<()> {
    let exe = std::env::current_exe()?;
    if let Some(icon) = icon_dir() {
        crate::icon::write_png(&icon.join("dictate.png"))?;
    }
    let Some(path) = desktop_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Dictate\n\
         Comment=按热键说话，文字进历史（通义千问 ASR）\n\
         Exec={exe}\n\
         Icon=dictate\n\
         Terminal=false\n\
         Categories=Utility;AudioVideo;\n\
         Keywords=dictate;asr;speech;语音;听写;\n\
         StartupNotify=false\n",
        exe = exe.display()
    );
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current != body {
        std::fs::write(&path, body)?;
    }
    Ok(())
}

/// 单实例锁：托盘常驻被点两次时，第二个进程直接退出。
pub fn acquire_single_instance() -> Option<std::fs::File> {
    let dir = dirs::data_dir()?.join("dictate");
    std::fs::create_dir_all(&dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join("daemon.lock"))
        .ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}

/// 终端模拟器的启动命令。优先用用户自己正在用的那个（`TERM_PROGRAM` / `TERMINAL`），
/// 否则按常见顺序找——**不能**默认起 gnome-terminal，那会在别人的 Ghostty 之外再开一个窗口。
/// 守护进程是否在跑（锁被别处持有）。
pub fn daemon_running() -> bool {
    let Some(dir) = dirs::data_dir().map(|d| d.join("dictate")) else {
        return false;
    };
    let Ok(f) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("daemon.lock"))
    else {
        return false;
    };
    f.try_lock().is_err() // 拿不到锁 = 别人持有 = 在跑
}

/// 后台拉起守护进程（新会话组，父进程退出也不影响它）。
pub fn spawn_daemon() -> Result<()> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("tray")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;
    Ok(())
}

pub fn terminal_candidates(exe: &str, sub: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    if let Ok(t) = std::env::var("TERMINAL") {
        if !t.trim().is_empty() {
            out.push(vec![t, "-e".into(), exe.into(), sub.into()]);
        }
    }
    // 用户当前终端优先
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase();
    if program.contains("ghostty") {
        out.push(vec!["ghostty".into(), "-e".into(), exe.into(), sub.into()]);
    }
    out.extend([
        vec!["ghostty".into(), "-e".into(), exe.into(), sub.into()],
        vec!["kitty".into(), exe.into(), sub.into()],
        vec!["alacritty".into(), "-e".into(), exe.into(), sub.into()],
        vec!["wezterm".into(), "start".into(), "--".into(), exe.into(), sub.into()],
        vec!["konsole".into(), "-e".into(), exe.into(), sub.into()],
        vec!["xfce4-terminal".into(), "-x".into(), exe.into(), sub.into()],
        vec!["gnome-terminal".into(), "--".into(), exe.into(), sub.into()],
        vec!["x-terminal-emulator".into(), "-e".into(), exe.into(), sub.into()],
    ]);
    out
}

/// 在终端模拟器里跑 `dictate <sub>`。
pub fn spawn_terminal(sub: &str) -> Result<()> {
    use std::process::Command;
    let exe = std::env::current_exe()?;
    let exe_s = exe.to_string_lossy().to_string();
    for c in terminal_candidates(&exe_s, sub) {
        if Command::new(&c[0]).args(&c[1..]).spawn().is_ok() {
            return Ok(());
        }
    }
    anyhow::bail!("找不到可用的终端模拟器（试过 ghostty / kitty / alacritty / wezterm / konsole / xfce4-terminal / gnome-terminal）")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghostty_is_preferred_when_running_in_it() {
        std::env::set_var("TERM_PROGRAM", "ghostty");
        let c = terminal_candidates("/bin/dictate", "history");
        assert_eq!(c[0][0], "ghostty", "在 Ghostty 里就该用 Ghostty 开");
        assert_eq!(c[0][1], "-e");
        std::env::remove_var("TERM_PROGRAM");
    }

    #[test]
    fn fallback_list_ends_with_xterm() {
        std::env::remove_var("TERM_PROGRAM");
        std::env::remove_var("TERMINAL");
        let c = terminal_candidates("/bin/dictate", "settings");
        assert!(c.iter().any(|v| v[0] == "gnome-terminal"));
        assert_eq!(c.last().unwrap()[0], "x-terminal-emulator");
    }
}
