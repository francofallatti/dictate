//! Global low-level keyboard hook (WH_KEYBOARD_LL) that reports when the configured chord
//! becomes fully pressed / released, plus Escape presses.
//!
//! The hook must be installed from a thread that runs a Win32 message loop (see `ui.rs`).

use anyhow::{anyhow, bail, Result};
use crossbeam_channel::Sender;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    ChordDown,
    ChordUp,
    Escape,
}

/// A chord is a set of key groups; every group must have at least one of its keys held.
/// `ctrl` expands to the group `[VK_LCONTROL, VK_RCONTROL]`, so either side works.
#[derive(Debug, Clone)]
pub struct Chord {
    groups: Vec<Vec<u32>>,
}

const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
/// Unassigned virtual key injected while the Windows key is held so the Start menu
/// does not open when it is released (same trick AutoHotkey and dictation tools use).
const VK_MASK: u16 = 0xE8;

impl Chord {
    pub fn parse(names: &[String]) -> Result<Self> {
        let mut groups = Vec::new();
        for name in names {
            groups.push(key_group(name)?);
        }
        if groups.is_empty() {
            bail!("chord is empty");
        }
        Ok(Self { groups })
    }

    fn is_complete(&self, down: &HashSet<u32>) -> bool {
        self.groups.iter().all(|g| g.iter().any(|k| down.contains(k)))
    }

    fn contains_win(&self) -> bool {
        self.groups.iter().flatten().any(|&k| k == VK_LWIN || k == VK_RWIN)
    }

    pub fn describe(&self) -> String {
        self.groups
            .iter()
            .map(|g| describe_group(g))
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

fn key_group(name: &str) -> Result<Vec<u32>> {
    let n = name.trim().to_ascii_lowercase();
    let g = match n.as_str() {
        "ctrl" | "control" => vec![0xA2, 0xA3],
        "lctrl" => vec![0xA2],
        "rctrl" => vec![0xA3],
        "win" | "meta" | "super" | "cmd" => vec![VK_LWIN, VK_RWIN],
        "lwin" => vec![VK_LWIN],
        "rwin" => vec![VK_RWIN],
        "alt" | "opt" | "option" => vec![0xA4, 0xA5],
        "lalt" => vec![0xA4],
        "ralt" | "altgr" => vec![0xA5],
        "shift" => vec![0xA0, 0xA1],
        "lshift" => vec![0xA0],
        "rshift" => vec![0xA1],
        "space" => vec![0x20],
        "caps" | "capslock" => vec![0x14],
        "tab" => vec![0x09],
        "esc" | "escape" => vec![0x1B],
        "enter" | "return" => vec![0x0D],
        "pause" => vec![0x13],
        "scrolllock" => vec![0x91],
        "insert" => vec![0x2D],
        "home" => vec![0x24],
        "end" => vec![0x23],
        "pageup" => vec![0x21],
        "pagedown" => vec![0x22],
        _ => {
            if let Some(hex) = n.strip_prefix("vk:0x").or_else(|| n.strip_prefix("vk:")) {
                let v = u32::from_str_radix(hex, 16).map_err(|_| anyhow!("bad virtual key {name:?}"))?;
                vec![v]
            } else if let Some(num) = n.strip_prefix('f') {
                let f: u32 = num.parse().map_err(|_| anyhow!("unknown key {name:?}"))?;
                if !(1..=24).contains(&f) {
                    bail!("unknown key {name:?}");
                }
                vec![0x70 + f - 1]
            } else if n.len() == 1 {
                let c = n.as_bytes()[0];
                if c.is_ascii_alphanumeric() {
                    vec![c.to_ascii_uppercase() as u32]
                } else {
                    bail!("unknown key {name:?}");
                }
            } else {
                bail!("unknown key {name:?}");
            }
        }
    };
    Ok(g)
}

fn describe_group(g: &[u32]) -> String {
    match g {
        [0xA2, 0xA3] => "Ctrl".into(),
        [0x5B, 0x5C] => "Win".into(),
        [0xA4, 0xA5] => "Alt".into(),
        [0xA0, 0xA1] => "Shift".into(),
        [0x20] => "Space".into(),
        [0x14] => "CapsLock".into(),
        [k] if (0x41..=0x5A).contains(k) || (0x30..=0x39).contains(k) => char::from(*k as u8).to_string(),
        [k] if (0x70..=0x87).contains(k) => format!("F{}", k - 0x70 + 1),
        [k] => format!("vk:0x{k:02X}"),
        many => many.iter().map(|k| format!("0x{k:02X}")).collect::<Vec<_>>().join("/"),
    }
}

struct HookState {
    chord: Chord,
    escape_cancels: bool,
    down: HashSet<u32>,
    active: bool,
    tx: Sender<HookEvent>,
}

static STATE: OnceLock<Mutex<HookState>> = OnceLock::new();

pub struct KeyboardHook {
    handle: HHOOK,
}

impl KeyboardHook {
    /// Installs the hook on the calling thread. Only one hook per process is supported.
    pub fn install(chord: Chord, escape_cancels: bool, tx: Sender<HookEvent>) -> Result<Self> {
        STATE
            .set(Mutex::new(HookState { chord, escape_cancels, down: HashSet::new(), active: false, tx }))
            .map_err(|_| anyhow!("keyboard hook already installed"))?;
        let hmod = unsafe { GetModuleHandleW(None)? };
        let handle = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(HINSTANCE(hmod.0)), 0)? };
        Ok(Self { handle })
    }
}

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.handle);
        }
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if !kb.flags.contains(LLKHF_INJECTED) {
            let msg = wparam.0 as u32;
            let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            on_key(kb.vkCode, is_down);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Replaces the chord at runtime (settings changed). Takes effect on the next key event.
pub fn update_chord(chord: Chord, escape_cancels: bool) -> Result<()> {
    let state = STATE.get().ok_or_else(|| anyhow!("keyboard hook not installed"))?;
    let mut s = state.lock().map_err(|_| anyhow!("hook state poisoned"))?;
    s.chord = chord;
    s.escape_cancels = escape_cancels;
    Ok(())
}

fn physically_down(vk: u32) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

fn on_key(vk: u32, is_down: bool) {
    let Some(state) = STATE.get() else { return };
    let Ok(mut s) = state.lock() else { return };
    if is_down {
        s.down.insert(vk);
    } else {
        s.down.remove(&vk);
    }
    // Key-ups can be lost (secure desktop, UAC prompts, remote sessions, sleep). Drop any key
    // the OS no longer reports as pressed so the chord cannot get stuck "active".
    s.down.retain(|&k| k == vk && is_down || physically_down(k));
    if s.escape_cancels && is_down && vk == VK_ESCAPE.0 as u32 {
        let _ = s.tx.try_send(HookEvent::Escape);
    }
    let complete = s.chord.is_complete(&s.down);
    if complete && !s.active {
        s.active = true;
        if s.chord.contains_win() {
            mask_win_key();
        }
        let _ = s.tx.try_send(HookEvent::ChordDown);
    } else if !complete && s.active {
        s.active = false;
        let _ = s.tx.try_send(HookEvent::ChordUp);
    }
}

/// Injects a harmless key press so Windows does not treat the Win key as "pressed alone".
fn mask_win_key() {
    let inputs = [key_input(VK_MASK, false), key_input(VK_MASK, true)];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

pub fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_chord() {
        let c = Chord::parse(&["ctrl".into(), "win".into()]).unwrap();
        assert_eq!(c.describe(), "Ctrl + Win");
        assert!(c.contains_win());
        let mut down = HashSet::new();
        down.insert(0xA3);
        assert!(!c.is_complete(&down));
        down.insert(0x5B);
        assert!(c.is_complete(&down));
    }

    #[test]
    fn parses_letters_and_function_keys() {
        assert_eq!(key_group("a").unwrap(), vec![0x41]);
        assert_eq!(key_group("F12").unwrap(), vec![0x7B]);
        assert_eq!(key_group("vk:0xE8").unwrap(), vec![0xE8]);
        assert!(key_group("banana").is_err());
    }
}
