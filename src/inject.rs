//! Text injection into the focused application: clipboard + Ctrl+V, then restore the
//! previous clipboard. Also resolves which app is in the foreground (for the LLM context).

use crate::config::PasteConfig;
use crate::hotkey::key_input;
use anyhow::{Context, Result};
use std::thread::sleep;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    VK_RWIN, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId};

#[derive(Debug, Clone, Default)]
pub struct ForegroundApp {
    /// Executable file name, e.g. `slack.exe`.
    pub process: String,
    pub title: String,
}

pub fn foreground_app() -> ForegroundApp {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return ForegroundApp::default();
        }
        let mut title_buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, &mut title_buf);
        let title = String::from_utf16_lossy(&title_buf[..n.max(0) as usize]);

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut process = String::new();
        if pid != 0 {
            if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                let mut buf = [0u16; 1024];
                let mut len = buf.len() as u32;
                if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
                    let full = String::from_utf16_lossy(&buf[..len as usize]);
                    process = full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string();
                }
                let _ = CloseHandle(h);
            }
        }
        ForegroundApp { process, title }
    }
}

const MODIFIERS: [u16; 8] = [
    VK_LWIN.0, VK_RWIN.0, VK_LCONTROL.0, VK_RCONTROL.0, VK_LMENU.0, VK_RMENU.0, VK_LSHIFT.0, VK_RSHIFT.0,
];

fn is_down(vk: u16) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// Waits for the user to let go of modifiers (otherwise Ctrl+V becomes Ctrl+Win+V and opens the
/// clipboard history). After the timeout, forcibly releases whatever is still down.
fn wait_modifiers_released(timeout: Duration) {
    let start = Instant::now();
    loop {
        let held: Vec<u16> = MODIFIERS.iter().copied().filter(|&vk| is_down(vk)).collect();
        if held.is_empty() {
            return;
        }
        if start.elapsed() > timeout {
            warn!(?held, "modifiers still held, releasing them before paste");
            let inputs: Vec<INPUT> = held.iter().map(|&vk| key_input(vk, true)).collect();
            unsafe {
                SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
            }
            sleep(Duration::from_millis(20));
            return;
        }
        sleep(Duration::from_millis(10));
    }
}

fn send_ctrl_v() -> Result<()> {
    let inputs = [
        key_input(VK_CONTROL.0, false),
        key_input(VK_V.0, false),
        key_input(VK_V.0, true),
        key_input(VK_CONTROL.0, true),
    ];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        let err = unsafe { windows::Win32::Foundation::GetLastError() };
        anyhow::bail!("SendInput injected {sent} of {} events (win32 error {})", inputs.len(), err.0);
    }
    Ok(())
}

/// Pastes `text` into the focused control.
pub fn paste_text(text: &str, cfg: &PasteConfig) -> Result<()> {
    let mut clip = arboard::Clipboard::new().context("opening clipboard")?;
    let previous = clip.get_text().ok();
    debug!(prev_len = previous.as_ref().map(|p| p.len()).unwrap_or(0), "saved previous clipboard");

    clip.set_text(text.to_string()).context("writing clipboard")?;
    wait_modifiers_released(Duration::from_millis(1500));
    send_ctrl_v()?;
    info!(chars = text.chars().count(), "paste sent");

    if cfg.restore_clipboard {
        sleep(Duration::from_millis(cfg.restore_delay_ms.max(50)));
        // Only restore if nobody else touched the clipboard meanwhile.
        let unchanged = clip.get_text().map(|t| t == text).unwrap_or(false);
        if unchanged {
            match previous {
                Some(p) => {
                    if let Err(e) = clip.set_text(p) {
                        warn!(error = %e, "could not restore clipboard");
                    }
                }
                None => {
                    let _ = clip.clear();
                }
            }
        } else {
            debug!("clipboard changed after paste; not restoring");
        }
    }
    Ok(())
}
