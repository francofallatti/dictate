//! The "UI thread": owns the Win32 message loop that the keyboard hook and the tray icon need.
//! The main thread talks to it with `PostThreadMessageW`.

use crate::hotkey::{Chord, HookEvent, KeyboardHook};
use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Sender;
use std::thread::JoinHandle;
use tracing::{error, info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows::core::w;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_APP, WM_QUIT};

const WM_SET_STATE: u32 = WM_APP + 1;
const WM_SHUTDOWN: u32 = WM_APP + 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    Loading,
    Idle,
    Listening,
    Locked,
    Processing,
    Error,
}

impl AppState {
    fn code(self) -> usize {
        match self {
            AppState::Loading => 0,
            AppState::Idle => 1,
            AppState::Listening => 2,
            AppState::Locked => 3,
            AppState::Processing => 4,
            AppState::Error => 5,
        }
    }
    fn from_code(c: usize) -> Self {
        match c {
            0 => AppState::Loading,
            2 => AppState::Listening,
            3 => AppState::Locked,
            4 => AppState::Processing,
            5 => AppState::Error,
            _ => AppState::Idle,
        }
    }
    fn label(self) -> &'static str {
        match self {
            AppState::Loading => "cargando modelo…",
            AppState::Idle => "listo",
            AppState::Listening => "escuchando",
            AppState::Locked => "escuchando (manos libres)",
            AppState::Processing => "transcribiendo…",
            AppState::Error => "error (ver log)",
        }
    }
    fn color(self) -> [u8; 3] {
        match self {
            AppState::Loading => [150, 150, 150],
            AppState::Idle => [90, 90, 90],
            AppState::Listening => [220, 50, 47],
            AppState::Locked => [203, 75, 22],
            AppState::Processing => [38, 139, 210],
            AppState::Error => [181, 137, 0],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiEvent {
    Quit,
    OpenConfig,
    OpenHistory,
}

pub struct UiHandle {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl UiHandle {
    pub fn set_state(&self, state: AppState) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_SET_STATE, WPARAM(state.code()), LPARAM(0));
        }
    }

    pub fn shutdown(mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_SHUTDOWN, WPARAM(0), LPARAM(0));
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub struct UiOptions {
    pub chord: Chord,
    pub escape_cancels: bool,
    pub tray: bool,
    pub chord_label: String,
}

/// Spawns the hook + tray thread and blocks until it is ready.
pub fn spawn(opts: UiOptions, hook_tx: Sender<HookEvent>, ui_tx: Sender<UiEvent>) -> Result<UiHandle> {
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<u32>>(1);
    let join = std::thread::Builder::new()
        .name("dictate-ui".into())
        .spawn(move || {
            let thread_id = unsafe { GetCurrentThreadId() };
            let _hook = match KeyboardHook::install(opts.chord, opts.escape_cancels, hook_tx) {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let tray = if opts.tray {
                match build_tray(&opts.chord_label, ui_tx) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        warn!(error = %e, "tray icon unavailable; continuing without it");
                        None
                    }
                }
            } else {
                None
            };
            let _ = ready_tx.send(Ok(thread_id));
            message_loop(tray, &opts.chord_label);
            info!("ui thread exiting");
        })
        .context("spawning ui thread")?;
    let thread_id = ready_rx.recv().map_err(|_| anyhow!("ui thread died during start-up"))??;
    Ok(UiHandle { thread_id, join: Some(join) })
}

fn message_loop(tray: Option<TrayIcon>, chord_label: &str) {
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_SET_STATE {
                let state = AppState::from_code(msg.wParam.0);
                if let Some(t) = &tray {
                    if let Err(e) = t.set_icon(Some(make_icon(state.color()))) {
                        warn!(error = %e, "set_icon failed");
                    }
                    let _ = t.set_tooltip(Some(format!("Dictate — {} ({})", state.label(), chord_label)));
                }
                continue;
            }
            if msg.message == WM_SHUTDOWN || msg.message == WM_QUIT {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    drop(tray);
}

fn build_tray(chord_label: &str, ui_tx: Sender<UiEvent>) -> Result<TrayIcon> {
    let menu = Menu::new();
    let title = MenuItem::new(format!("Dictate — mantené {chord_label} para dictar"), false, None);
    let open_config = MenuItem::new("Abrir configuración", true, None);
    let open_history = MenuItem::new("Abrir historial", true, None);
    let quit = MenuItem::new("Salir", true, None);
    menu.append(&title)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&open_config)?;
    menu.append(&open_history)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit)?;

    let ids = (open_config.id().clone(), open_history.id().clone(), quit.id().clone());
    MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
        let event = if ev.id == ids.0 {
            UiEvent::OpenConfig
        } else if ev.id == ids.1 {
            UiEvent::OpenHistory
        } else if ev.id == ids.2 {
            UiEvent::Quit
        } else {
            return;
        };
        let _ = ui_tx.send(event);
    }));

    let tray = TrayIconBuilder::new()
        .with_id("dictate")
        .with_menu(Box::new(menu))
        .with_tooltip(format!("Dictate — {} ({chord_label})", AppState::Loading.label()))
        .with_icon(make_icon(AppState::Loading.color()))
        .build()
        .context("creating tray icon")?;
    Ok(tray)
}

/// A filled circle with a thin dark outline, 32×32 RGBA.
fn make_icon(rgb: [u8; 3]) -> Icon {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let c = (SIZE as f32 - 1.0) / 2.0;
    let r_outer = SIZE as f32 / 2.0 - 1.0;
    let r_inner = r_outer - 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let (px, a) = if d <= r_inner {
                (rgb, 255u8)
            } else if d <= r_outer {
                let t = (r_outer - d).clamp(0.0, 1.0);
                ([30, 30, 30], (t * 255.0) as u8)
            } else {
                ([0, 0, 0], 0)
            };
            rgba.extend_from_slice(&[px[0], px[1], px[2], a]);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("valid icon buffer")
}

#[derive(Debug, Clone, Copy)]
pub enum Sound {
    Start,
    Stop,
    Cancel,
    Error,
}

pub fn play(sound: Sound) {
    let alias = match sound {
        Sound::Start => w!("SystemAsterisk"),
        Sound::Stop => w!("SystemNotification"),
        Sound::Cancel => w!("SystemHand"),
        Sound::Error => w!("SystemExclamation"),
    };
    unsafe {
        if !PlaySoundW(alias, None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT).as_bool() {
            error!("PlaySound failed");
        }
    }
}
