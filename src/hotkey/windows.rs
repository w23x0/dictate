//! Windows 热键：`WH_KEYBOARD_LL` 低级钩子。
//!
//! `RegisterHotKey` 只在按下时给 `WM_HOTKEY`，没有松开事件，做不了「按住说话」。
//! 低级钩子不需要管理员权限，但**必须**跑在有消息循环的线程里。

use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use super::{HotkeyEvent, Key, SharedSpec, Spec};

static TX: OnceLock<Sender<HotkeyEvent>> = OnceLock::new();
static SPEC: OnceLock<Spec> = OnceLock::new();

fn vk_for(key: Key) -> u32 {
    match key {
        Key::Char(c) => c.to_ascii_uppercase() as u32,
        Key::Digit(d) => 0x30 + d as u32,
        Key::F(n) => 0x70 + (n as u32 - 1),
        Key::Space => 0x20,
        Key::Enter => 0x0D,
        Key::Tab => 0x09,
        Key::Escape => 0x1B,
    }
}

fn held(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) & 0x8000u16 as i16 != 0 }
}

fn modifiers_match(spec: &Spec) -> bool {
    held(VK_CONTROL as i32) == spec.ctrl
        && held(VK_MENU as i32) == spec.alt
        && held(VK_SHIFT as i32) == spec.shift
        && (held(VK_LWIN as i32) || held(VK_RWIN as i32)) == spec.meta
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let is_down = wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN;
        let is_up = wparam as u32 == WM_KEYUP || wparam as u32 == WM_SYSKEYUP;
        if (is_down || is_up) && SPEC.get().is_some_and(|s| vk_for(s.key) == kb.vkCode) {
            // 松开时修饰键可能已经先松了，用「按下时匹配、松开时不匹配」会导致漏 release，
            // 所以 release 无条件发出，由上层按状态机忽略多余事件。
            let ev = if is_down {
                if modifiers_match(SPEC.get().unwrap()) {
                    Some(HotkeyEvent::Press)
                } else {
                    None
                }
            } else {
                Some(HotkeyEvent::Release)
            };
            if let (Some(ev), Some(tx)) = (ev, TX.get()) {
                let _ = tx.send(ev);
            }
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

pub fn spawn(spec: Spec, tx: Sender<HotkeyEvent>) -> Result<SharedSpec> {
    SPEC.set(spec).ok();
    TX.set(tx).map_err(|_| anyhow!("热键监听已经启动过"))?;
    std::thread::spawn(|| unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), std::ptr::null_mut(), 0);
        if hook.is_null() {
            eprintln!("[hotkey] 安装键盘钩子失败");
            return;
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
    });
    // 钩子装的时候定死了：返回共享定义只是为了和 Linux 端签名一致，改它不生效，
    // 换热键要重启进程。
    Ok(std::sync::Arc::new(std::sync::RwLock::new(spec)))
}
