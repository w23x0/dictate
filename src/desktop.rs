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

/// 找一个能开的终端模拟器，在它里面跑 `dictate <sub>`。
pub fn spawn_terminal(sub: &str) -> Result<()> {
    use std::process::Command;
    let exe = std::env::current_exe()?;
    let exe_s = exe.to_string_lossy().to_string();

    // 每家参数形状不同：gnome-terminal 用 `--`，其余用 `-e`。
    let candidates: Vec<Vec<String>> = vec![
        vec!["gnome-terminal".into(), "--".into(), exe_s.clone(), sub.into()],
        vec!["konsole".into(), "-e".into(), exe_s.clone(), sub.into()],
        vec!["xfce4-terminal".into(), "-x".into(), exe_s.clone(), sub.into()],
        vec!["kitty".into(), exe_s.clone(), sub.into()],
        vec!["alacritty".into(), "-e".into(), exe_s.clone(), sub.into()],
        vec!["x-terminal-emulator".into(), "-e".into(), exe_s.clone(), sub.into()],
    ];
    for c in candidates {
        if Command::new(&c[0]).args(&c[1..]).spawn().is_ok() {
            return Ok(());
        }
    }
    anyhow::bail!("找不到可用的终端模拟器（试过 gnome-terminal / konsole / xfce4-terminal / kitty / alacritty）")
}
