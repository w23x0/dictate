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
        crate::ui::icon::write_png(&icon.join("dictate.png"))?;
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

/// 单实例：用 pid 文件判断，避免 flock 在「上一个进程刚死」时的竞态。
pub fn acquire_single_instance() -> Option<std::fs::File> {
    if daemon_running() {
        return None;
    }
    let dir = dirs::data_dir()?.join("dictate");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("daemon.pid");
    std::fs::write(&path, std::process::id().to_string()).ok()?;
    std::fs::File::open(path).ok()
}

/// 守护进程是否在跑（pid 文件里那个进程还活着，且确实是 dictate）。
pub fn daemon_running() -> bool {
    let Some(dir) = dirs::data_dir().map(|d| d.join("dictate")) else {
        return false;
    };
    let Ok(pid) = std::fs::read_to_string(dir.join("daemon.pid")) else {
        return false;
    };
    let Ok(pid) = pid.trim().parse::<u32>() else {
        return false;
    };
    if pid == std::process::id() {
        return false;
    }
    let alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
    if !alive {
        return false;
    }
    // 确认那个 pid 真的是 dictate（防 pid 复用）
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| String::from_utf8_lossy(&c).contains("dictate"))
        .unwrap_or(false)
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
    let program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if program.contains("ghostty") {
        out.push(vec!["ghostty".into(), "-e".into(), exe.into(), sub.into()]);
    }
    out.extend([
        vec!["ghostty".into(), "-e".into(), exe.into(), sub.into()],
        vec!["kitty".into(), exe.into(), sub.into()],
        vec!["alacritty".into(), "-e".into(), exe.into(), sub.into()],
        vec![
            "wezterm".into(),
            "start".into(),
            "--".into(),
            exe.into(),
            sub.into(),
        ],
        vec!["konsole".into(), "-e".into(), exe.into(), sub.into()],
        vec!["xfce4-terminal".into(), "-x".into(), exe.into(), sub.into()],
        vec!["gnome-terminal".into(), "--".into(), exe.into(), sub.into()],
        vec![
            "x-terminal-emulator".into(),
            "-e".into(),
            exe.into(),
            sub.into(),
        ],
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
