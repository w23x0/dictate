//! 托盘常驻入口。Linux 走 GTK + libayatana-appindicator，Windows 走 tray-icon 的消息泵。

use anyhow::Result;

use crate::app::daemon::Daemon;
use crate::core::config::Config;

#[cfg(target_os = "linux")]
pub fn run(cfg: Config) -> Result<()> {
    use gtk::glib;
    use std::time::Duration;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    gtk::init().map_err(|e| anyhow::anyhow!("GTK 初始化失败：{e}"))?;
    crate::app::desktop::ensure_installed()?;

    let mut daemon = Daemon::new(cfg)?;

    let menu = Menu::new();
    let history = MenuItem::with_id("history", "打开历史…", true, None);
    let settings = MenuItem::with_id("settings", "设置…", true, None);
    let sep = PredefinedMenuItem::separator();
    let quit = MenuItem::with_id("quit", "退出", true, None);
    menu.append_items(&[&history, &settings, &sep, &quit])?;

    let icon = Icon::from_rgba(crate::ui::icon::rgba(), 64, 64)?;
    let tray = TrayIconBuilder::new()
        .with_id("dictate")
        .with_menu(Box::new(menu))
        .with_tooltip("dictate — 就绪")
        .with_icon(icon)
        .build()?;

    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            match ev.id.0.as_str() {
                "history" => {
                    let _ = crate::app::desktop::spawn_terminal("history");
                }
                "settings" => {
                    let _ = crate::app::desktop::spawn_terminal("settings");
                }
                "quit" => {
                    gtk::main_quit();
                    return glib::ControlFlow::Break;
                }
                _ => {}
            }
        }
        let tip = daemon.tick();
        tray.set_tooltip(Some(&tip)).ok();
        if daemon.quit {
            gtk::main_quit();
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });

    gtk::main();
    Ok(())
}

#[cfg(windows)]
pub fn run(cfg: Config) -> Result<()> {
    use std::time::Duration;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    crate::app::desktop::ensure_installed()?;
    let mut daemon = Daemon::new(cfg)?;

    let menu = Menu::new();
    let history = MenuItem::with_id("history", "打开历史…", true, None);
    let settings = MenuItem::with_id("settings", "设置…", true, None);
    let sep = PredefinedMenuItem::separator();
    let quit = MenuItem::with_id("quit", "退出", true, None);
    menu.append_items(&[&history, &settings, &sep, &quit])?;

    let icon = Icon::from_rgba(crate::ui::icon::rgba(), 64, 64)?;
    let tray = TrayIconBuilder::new()
        .with_id("dictate")
        .with_menu(Box::new(menu))
        .with_tooltip("dictate — 就绪")
        .with_icon(icon)
        .build()?;

    // 简单消息泵：菜单事件由 muda 自己的线程投递，这里只轮询 + 睡。
    loop {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            match ev.id.0.as_str() {
                "history" => {
                    let _ = crate::app::desktop::spawn_terminal("history");
                }
                "settings" => {
                    let _ = crate::app::desktop::spawn_terminal("settings");
                }
                "quit" => return Ok(()),
                _ => {}
            }
        }
        let tip = daemon.tick();
        tray.set_tooltip(Some(&tip)).ok();
        if daemon.quit {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
