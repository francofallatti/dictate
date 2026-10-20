//! The hub window: history + stats (like Wispr Flow's Home), settings and diagnostics.
//!
//! Runs on the main thread with eframe. The dictation orchestrator lives on another thread and
//! talks to the panel through `SharedState` (data) and `PanelLink` (repaint / show / close).

use crate::audio;
use crate::config::{Config, Paths};
use crate::history::{self, HistoryRecord};
use crate::hotkey::Chord;
use crate::ui::AppState;
use chrono::{DateTime, Datelike, Local};
use crossbeam_channel::Sender;
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Live data the orchestrator publishes for the panel.
pub struct Shared {
    pub state: AppState,
    pub backend_ready: bool,
    pub backend_error: Option<String>,
    pub last_error: Option<String>,
    pub last_result: Option<String>,
    pub mic_name: String,
    pub chord_label: String,
    pub jobs_in_flight: usize,
    pub events: VecDeque<String>,
    pub history_dirty: bool,
    pub quitting: bool,
    pub config: Config,
}

pub type SharedState = Arc<Mutex<Shared>>;

impl Shared {
    pub fn new(config: Config, chord_label: String) -> Self {
        Self {
            state: AppState::Loading,
            backend_ready: false,
            backend_error: None,
            last_error: None,
            last_result: None,
            mic_name: String::new(),
            chord_label,
            jobs_in_flight: 0,
            events: VecDeque::new(),
            history_dirty: true,
            quitting: false,
            config,
        }
    }

    /// Appends a timestamped line to the diagnostics ring buffer.
    pub fn event(&mut self, line: impl AsRef<str>) {
        let stamp = Local::now().format("%H:%M:%S%.3f");
        self.events.push_back(format!("{stamp}  {}", line.as_ref()));
        while self.events.len() > 400 {
            self.events.pop_front();
        }
    }
}

/// Requests from the panel to the orchestrator.
#[derive(Debug, Clone)]
pub enum PanelCommand {
    /// The user saved settings; the file is already written.
    ApplyConfig(Config),
    /// Reload the speech backend (e.g. after a failed download).
    ReloadBackend,
    Quit,
}

/// Handle the orchestrator uses to poke the panel.
#[derive(Clone, Default)]
pub struct PanelLink {
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

impl PanelLink {
    fn attach(&self, ctx: egui::Context) {
        *self.ctx.lock().unwrap() = Some(ctx);
    }
    pub fn repaint(&self) {
        if let Some(c) = self.ctx.lock().unwrap().as_ref() {
            c.request_repaint();
        }
    }
    pub fn show(&self) {
        if let Some(c) = self.ctx.lock().unwrap().as_ref() {
            c.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            c.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            c.send_viewport_cmd(egui::ViewportCommand::Focus);
            c.request_repaint();
        }
    }
    pub fn close(&self) {
        if let Some(c) = self.ctx.lock().unwrap().as_ref() {
            c.send_viewport_cmd(egui::ViewportCommand::Close);
            c.request_repaint();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Home,
    Settings,
    Diagnostics,
}

pub struct PanelApp {
    shared: SharedState,
    paths: Arc<Paths>,
    cmd_tx: Sender<PanelCommand>,
    page: Page,
    // Home
    history: Vec<HistoryRecord>,
    search: String,
    expanded: Option<String>,
    confirm_delete: Option<String>,
    toast: Option<(String, Instant)>,
    // Settings
    draft: Config,
    chord_text: String,
    chord_error: Option<String>,
    devices: Vec<String>,
    settings_saved_at: Option<Instant>,
    settings_error: Option<String>,
    // Diagnostics
    log_tail: Vec<String>,
    log_loaded_at: Option<Instant>,
}

const SIDEBAR_BG: Color32 = Color32::from_rgb(246, 246, 248);
const PANEL_BG: Color32 = Color32::from_rgb(250, 250, 251);
const CARD_BG: Color32 = Color32::WHITE;
const CARD_STROKE: Color32 = Color32::from_rgb(228, 228, 232);
const ACCENT: Color32 = Color32::from_rgb(93, 95, 239);
const MUTED: Color32 = Color32::from_rgb(120, 120, 130);
const OK_GREEN: Color32 = Color32::from_rgb(34, 139, 84);
const WARN_ORANGE: Color32 = Color32::from_rgb(203, 118, 22);
const ERR_RED: Color32 = Color32::from_rgb(200, 60, 60);

/// Starts the panel event loop on the calling (main) thread. Returns when the window closes
/// for real (quit), not when the user hides it.
pub fn run(
    shared: SharedState,
    paths: Arc<Paths>,
    cmd_tx: Sender<PanelCommand>,
    link: PanelLink,
    start_visible: bool,
) -> eframe::Result<()> {
    let icon = window_icon();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Dictate")
            .with_app_id("dictate")
            .with_inner_size([960.0, 620.0])
            .with_min_inner_size([800.0, 520.0])
            .with_visible(start_visible)
            .with_icon(icon),
        ..Default::default()
    };
    let draft = shared.lock().unwrap().config.clone();
    eframe::run_native(
        "Dictate",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(light_visuals());
            link.attach(cc.egui_ctx.clone());
            let chord_text = draft.hotkey.chord.join(" + ");
            Ok(Box::new(PanelApp {
                shared,
                paths,
                cmd_tx,
                page: Page::Home,
                history: Vec::new(),
                search: String::new(),
                expanded: None,
                confirm_delete: None,
                toast: None,
                draft,
                chord_text,
                chord_error: None,
                devices: audio::input_device_names().unwrap_or_default(),
                settings_saved_at: None,
                settings_error: None,
                log_tail: Vec::new(),
                log_loaded_at: None,
            }))
        }),
    )
}

fn light_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::light();
    v.panel_fill = Color32::from_rgb(250, 250, 251);
    v.window_fill = Color32::WHITE;
    v.selection.bg_fill = ACCENT.gamma_multiply(0.18);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.widgets.hovered.bg_fill = Color32::from_rgb(236, 236, 242);
    v.widgets.active.bg_fill = ACCENT.gamma_multiply(0.25);
    // Text fields sit on white cards; give them a visible well and border.
    v.extreme_bg_color = Color32::from_rgb(243, 243, 246);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, CARD_STROKE);
    v
}

fn window_icon() -> egui::IconData {
    const S: u32 = 64;
    let mut rgba = Vec::with_capacity((S * S * 4) as usize);
    let c = (S as f32 - 1.0) / 2.0;
    for y in 0..S {
        for x in 0..S {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let a = if d <= c - 1.0 { 255 } else if d <= c { ((c - d) * 255.0) as u8 } else { 0 };
            rgba.extend_from_slice(&[ACCENT.r(), ACCENT.g(), ACCENT.b(), a]);
        }
    }
    egui::IconData { rgba, width: S, height: S }
}

impl eframe::App for PanelApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        // Hide instead of closing; the tray keeps the app alive. A real quit sets `quitting`.
        if ctx.input(|i| i.viewport().close_requested()) {
            let quitting = self.shared.lock().unwrap().quitting;
            if !quitting {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }

        self.refresh_history_if_dirty();
        if matches!(self.page, Page::Diagnostics) {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }

        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(200.0)
            .frame(egui::Frame::new().fill(SIDEBAR_BG).inner_margin(Margin::symmetric(14, 18)))
            .show(root, |ui| self.sidebar(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(PANEL_BG).inner_margin(Margin::symmetric(28, 22)))
            .show(root, |ui| match self.page {
                Page::Home => self.home(ui),
                Page::Settings => self.settings(ui),
                Page::Diagnostics => self.diagnostics(ui),
            });

        self.draw_toast(&ctx);
    }
}

impl PanelApp {
    // ---------------------------------------------------------------- sidebar

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let (state, chord, ready) = {
            let s = self.shared.lock().unwrap();
            (s.state, s.chord_label.clone(), s.backend_ready)
        };
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 6.0, state_color(state));
            ui.heading(RichText::new("Dictate").strong());
        });
        ui.add_space(2.0);
        ui.label(RichText::new(state_label(state, ready)).small().color(MUTED));
        ui.add_space(18.0);

        for (page, label) in [(Page::Home, "Inicio"), (Page::Settings, "Configuración"), (Page::Diagnostics, "Diagnóstico")] {
            let selected = self.page == page;
            let text = RichText::new(label).size(15.0);
            let text = if selected { text.strong().color(ACCENT) } else { text };
            let resp = ui.add_sized([ui.available_width(), 32.0], egui::Button::selectable(selected, text));
            if resp.clicked() {
                self.page = page;
            }
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            if ui.button("Salir de Dictate").clicked() {
                let _ = self.cmd_tx.send(PanelCommand::Quit);
            }
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Mantené {chord} para dictar")).small().color(MUTED));
            ui.label(RichText::new("Doble toque: manos libres · Esc: cancelar").small().color(MUTED));
        });
    }

    // ------------------------------------------------------------------- home

    fn refresh_history_if_dirty(&mut self) {
        let dirty = {
            let mut s = self.shared.lock().unwrap();
            std::mem::replace(&mut s.history_dirty, false)
        };
        if dirty {
            self.history = history::read_all(&self.paths.history_file).unwrap_or_default();
            self.history.reverse(); // newest first
        }
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("Dictados").size(22.0).strong());
        ui.add_space(10.0);
        self.stats_row(ui);
        ui.add_space(14.0);

        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Buscar en dictados anteriores")
                    .desired_width(360.0),
            );
            if !self.search.is_empty() && ui.small_button("✕").clicked() {
                self.search.clear();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Abrir historial (JSONL)").clicked() {
                    open_path(&self.paths.history_file);
                }
                if ui.button("Actualizar").clicked() {
                    self.shared.lock().unwrap().history_dirty = true;
                }
            });
        });
        ui.add_space(10.0);

        if self.history.is_empty() {
            card(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(30.0);
                    ui.label(RichText::new("Todavía no hay dictados").size(16.0).strong());
                    ui.label(RichText::new("Cuando dictes en cualquier app, el historial completo va a aparecer acá.").color(MUTED));
                    ui.add_space(30.0);
                });
            });
            return;
        }

        let needle = self.search.trim().to_lowercase();
        let filtered: Vec<HistoryRecord> = self
            .history
            .iter()
            .filter(|r| {
                needle.is_empty()
                    || r.final_text.to_lowercase().contains(&needle)
                    || r.raw_text.to_lowercase().contains(&needle)
                    || r.app.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect();
        if !needle.is_empty() {
            ui.label(RichText::new(format!("{} resultado(s)", filtered.len())).small().color(MUTED));
        }

        let mut to_delete: Option<String> = None;
        let mut copy: Option<String> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let mut current_day = String::new();
            for rec in &filtered {
                let day = day_label(&rec.timestamp);
                if day != current_day {
                    current_day = day.clone();
                    ui.add_space(8.0);
                    ui.label(RichText::new(day).small().strong().color(MUTED));
                    ui.add_space(2.0);
                }
                self.history_row(ui, rec, &mut copy, &mut to_delete);
            }
            ui.add_space(20.0);
        });

        if let Some(text) = copy {
            match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                Ok(()) => self.toast("Transcripción copiada"),
                Err(e) => self.toast(format!("No se pudo copiar: {e}")),
            }
        }
        if let Some(ts) = to_delete {
            match history::delete(&self.paths.history_file, &ts) {
                Ok(()) => {
                    self.toast("Transcripción eliminada");
                    self.shared.lock().unwrap().history_dirty = true;
                }
                Err(e) => self.toast(format!("No se pudo eliminar: {e}")),
            }
        }
    }

    fn stats_row(&self, ui: &mut egui::Ui) {
        let total = self.history.len();
        let words_total: usize = self.history.iter().map(|r| word_count(&r.final_text)).sum();
        let week_start = Local::now() - chrono::Duration::days(7);
        let words_week: usize = self
            .history
            .iter()
            .filter(|r| parse_ts(&r.timestamp).map(|t| t >= week_start).unwrap_or(false))
            .map(|r| word_count(&r.final_text))
            .sum();
        let (w, secs) = self.history.iter().filter(|r| r.audio_secs > 0.5).fold((0usize, 0f32), |(w, s), r| {
            (w + word_count(&r.final_text), s + r.audio_secs)
        });
        let wpm = if secs > 0.0 { (w as f32 / secs * 60.0).round() as usize } else { 0 };
        let pasted_ok = self.history.iter().filter(|r| r.pasted).count();

        let gap = ui.spacing().item_spacing.x;
        let tile_w = ((ui.available_width() - 3.0 * gap) / 4.0).clamp(120.0, 230.0);
        ui.horizontal_wrapped(|ui| {
            stat_tile(ui, tile_w, &format_int(words_total), "palabras en total");
            stat_tile(ui, tile_w, &format_int(words_week), "palabras esta semana");
            stat_tile(ui, tile_w, &wpm.to_string(), "ppm (palabras por minuto)");
            stat_tile(ui, tile_w, &format!("{pasted_ok}/{total}"), "dictados pegados");
        });
    }

    fn history_row(&mut self, ui: &mut egui::Ui, rec: &HistoryRecord, copy: &mut Option<String>, to_delete: &mut Option<String>) {
        let id = rec.timestamp.clone();
        let expanded = self.expanded.as_deref() == Some(id.as_str());
        let status = if rec.final_text.trim().is_empty() {
            ("vacío", MUTED)
        } else if rec.pasted {
            ("pegado", OK_GREEN)
        } else {
            ("no pegado", ERR_RED)
        };

        egui::Frame::new()
            .fill(CARD_BG)
            .stroke(Stroke::new(1.0, CARD_STROKE))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(14, 10))
            .outer_margin(Margin::symmetric(0, 3))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(time_label(&rec.timestamp)).monospace().color(MUTED));
                    ui.label(RichText::new(app_label(&rec.app)).strong());
                    chip(ui, status.0, status.1);
                    if rec.format_backend != "none" {
                        chip(ui, &format!("IA: {}", rec.format_backend), ACCENT);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.confirm_delete.as_deref() == Some(id.as_str()) {
                            if ui.button(RichText::new("Sí, eliminar").color(ERR_RED)).clicked() {
                                *to_delete = Some(id.clone());
                                self.confirm_delete = None;
                            }
                            if ui.button("Cancelar").clicked() {
                                self.confirm_delete = None;
                            }
                        } else {
                            if ui.button("Eliminar").on_hover_text("Eliminar transcripción").clicked() {
                                self.confirm_delete = Some(id.clone());
                            }
                            if ui.button("Copiar").on_hover_text("Copiar transcripción").clicked() {
                                *copy = Some(rec.final_text.clone());
                            }
                            if ui.button(if expanded { "Menos" } else { "Detalles" }).clicked() {
                                self.expanded = if expanded { None } else { Some(id.clone()) };
                            }
                        }
                    });
                });
                ui.add_space(4.0);
                let text = if rec.final_text.trim().is_empty() {
                    RichText::new("(sin palabras detectadas)").italics().color(MUTED)
                } else {
                    RichText::new(rec.final_text.clone()).size(14.5)
                };
                ui.add(egui::Label::new(text).wrap());
                if expanded {
                    ui.add_space(6.0);
                    ui.separator();
                    egui::Grid::new(format!("det-{id}")).num_columns(2).spacing([18.0, 4.0]).show(ui, |ui| {
                        ui.label(RichText::new("Ventana").color(MUTED));
                        ui.label(&rec.window_title);
                        ui.end_row();
                        ui.label(RichText::new("Audio").color(MUTED));
                        ui.label(format!("{:.1} s", rec.audio_secs));
                        ui.end_row();
                        ui.label(RichText::new("Transcripción").color(MUTED));
                        ui.label(format!("{} · {} ms", rec.stt_backend, rec.stt_ms));
                        ui.end_row();
                        ui.label(RichText::new("Formateo").color(MUTED));
                        ui.label(format!("{} · {} ms", rec.format_backend, rec.format_ms));
                        ui.end_row();
                        if rec.raw_text != rec.final_text {
                            ui.label(RichText::new("Texto crudo").color(MUTED));
                            ui.add(egui::Label::new(&rec.raw_text).wrap());
                            ui.end_row();
                        }
                    });
                }
            });
    }

    // --------------------------------------------------------------- settings

    fn settings(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Configuración").size(22.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save = egui::Button::new(RichText::new("Guardar y aplicar").color(Color32::WHITE)).fill(ACCENT);
                if ui.add(save).clicked() {
                    self.save_settings();
                }
                if ui.button("Abrir config.toml").clicked() {
                    open_path(&self.paths.config_file);
                }
                if let Some(t) = self.settings_saved_at {
                    if t.elapsed().as_secs() < 4 {
                        ui.label(RichText::new("Guardado ✓").color(OK_GREEN));
                    }
                }
            });
        });
        if let Some(err) = &self.settings_error {
            ui.colored_label(ERR_RED, err);
        }
        ui.add_space(8.0);

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            section(ui, "General", |ui| {
                settings_grid(ui, "general", |ui| {
                    row(ui, "Atajo de dictado", "Teclas separadas por +. Ej: ctrl + win, ctrl + alt, f8, caps", |ui| {
                        let resp = ui.add(egui::TextEdit::singleline(&mut self.chord_text).desired_width(220.0));
                        if resp.changed() {
                            self.chord_error = None;
                            let names: Vec<String> = self.chord_text.split(['+', ',']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                            match Chord::parse(&names) {
                                Ok(c) => {
                                    self.draft.hotkey.chord = names;
                                    ui.label(RichText::new(c.describe()).color(MUTED));
                                }
                                Err(e) => self.chord_error = Some(e.to_string()),
                            }
                        } else if let Some(e) = &self.chord_error {
                            ui.colored_label(ERR_RED, e);
                        } else if let Ok(c) = Chord::parse(&self.draft.hotkey.chord) {
                            ui.label(RichText::new(c.describe()).color(MUTED));
                        }
                    });
                    row(ui, "Ventana de doble toque", "ms para detectar el doble toque (manos libres)", |ui| {
                        ui.add(egui::DragValue::new(&mut self.draft.hotkey.double_tap_ms).range(200..=1500).suffix(" ms"));
                    });
                    row(ui, "Esc cancela el dictado", "", |ui| {
                        ui.checkbox(&mut self.draft.hotkey.escape_cancels, "");
                    });
                    row(ui, "Duración máxima", "corte automático de un dictado", |ui| {
                        let mut mins = (self.draft.hotkey.max_duration_secs / 60).max(1);
                        if ui.add(egui::DragValue::new(&mut mins).range(1..=60).suffix(" min")).changed() {
                            self.draft.hotkey.max_duration_secs = mins * 60;
                        }
                    });
                    row(ui, "Micrófono", "vacío = dispositivo predeterminado del sistema", |ui| {
                        let current = if self.draft.audio.device.is_empty() { "Predeterminado".to_string() } else { self.draft.audio.device.clone() };
                        egui::ComboBox::from_id_salt("mic").selected_text(current).width(320.0).show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.draft.audio.device, String::new(), "Predeterminado");
                            for d in &self.devices {
                                ui.selectable_value(&mut self.draft.audio.device, d.clone(), d);
                            }
                        });
                    });
                    row(ui, "Idioma", "auto detecta; fijarlo mejora la precisión", |ui| {
                        egui::ComboBox::from_id_salt("lang").selected_text(lang_name(&self.draft.stt.language)).width(200.0).show_ui(ui, |ui| {
                            for (code, name) in LANGS {
                                ui.selectable_value(&mut self.draft.stt.language, code.to_string(), *name);
                            }
                        });
                    });
                });
            });

            section(ui, "Transcripción", |ui| {
                settings_grid(ui, "stt", |ui| {
                    row(ui, "Motor", "", |ui| {
                        ui.selectable_value(&mut self.draft.stt.backend, "whisper".into(), "whisper.cpp (local)");
                        ui.selectable_value(&mut self.draft.stt.backend, "openai".into(), "OpenAI (nube)");
                    });
                    row(ui, "Vocabulario", "nombres y jerga que el reconocedor debería conocer", |ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.draft.stt.initial_prompt).desired_width(380.0).hint_text("Global66, Franco, Jira…"));
                    });
                    if self.draft.stt.backend == "whisper" {
                        row(ui, "Modelo whisper", "más grande = más preciso y más lento; se descarga solo", |ui| {
                            egui::ComboBox::from_id_salt("wmodel").selected_text(&self.draft.stt.whisper.model).width(200.0).show_ui(ui, |ui| {
                                for m in ["tiny", "base", "small", "medium", "large-v3-turbo", "tiny.en", "base.en", "small.en", "medium.en"] {
                                    ui.selectable_value(&mut self.draft.stt.whisper.model, m.to_string(), m);
                                }
                            });
                        });
                        row(ui, "Ruta de modelo", "opcional; un .bin propio tiene prioridad", |ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.draft.stt.whisper.model_path).desired_width(380.0));
                        });
                        row(ui, "Hilos", "0 = automático", |ui| {
                            ui.add(egui::DragValue::new(&mut self.draft.stt.whisper.threads).range(0..=32));
                        });
                    } else {
                        row(ui, "Modelo OpenAI", "", |ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.draft.stt.openai.model).desired_width(240.0));
                        });
                        row(ui, "API key", "o dejá vacío y usá la variable de entorno", |ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.draft.stt.openai.api_key).password(true).desired_width(300.0));
                            ui.label(RichText::new(format!("env: {}", self.draft.stt.openai.api_key_env)).small().color(MUTED));
                        });
                        row(ui, "Base URL", "", |ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.draft.stt.openai.base_url).desired_width(300.0));
                        });
                    }
                });
            });

            section(ui, "Formateo con IA", |ui| {
                settings_grid(ui, "fmt", |ui| {
                    row(ui, "Limpieza del texto", "quita muletillas, puntúa y aplica comandos hablados", |ui| {
                        ui.selectable_value(&mut self.draft.format.backend, "none".into(), "Desactivado");
                        ui.selectable_value(&mut self.draft.format.backend, "claude".into(), "Claude");
                        ui.selectable_value(&mut self.draft.format.backend, "openai".into(), "OpenAI");
                    });
                    match self.draft.format.backend.as_str() {
                        "claude" => {
                            row(ui, "Modelo", "", |ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.draft.format.claude.model).desired_width(240.0));
                            });
                            row(ui, "Esfuerzo", "", |ui| {
                                for e in ["low", "medium", "high"] {
                                    ui.selectable_value(&mut self.draft.format.claude.effort, e.to_string(), e);
                                }
                            });
                            row(ui, "API key", "o dejá vacío y usá la variable de entorno", |ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.draft.format.claude.api_key).password(true).desired_width(300.0));
                                ui.label(RichText::new(format!("env: {}", self.draft.format.claude.api_key_env)).small().color(MUTED));
                            });
                        }
                        "openai" => {
                            row(ui, "Modelo", "", |ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.draft.format.openai.model).desired_width(240.0));
                            });
                            row(ui, "API key", "o dejá vacío y usá la variable de entorno", |ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.draft.format.openai.api_key).password(true).desired_width(300.0));
                                ui.label(RichText::new(format!("env: {}", self.draft.format.openai.api_key_env)).small().color(MUTED));
                            });
                        }
                        _ => {}
                    }
                    if self.draft.format.backend != "none" {
                        row(ui, "Instrucciones extra", "tono, firma, glosario…", |ui| {
                            ui.add(egui::TextEdit::multiline(&mut self.draft.format.extra_instructions).desired_width(380.0).desired_rows(3));
                        });
                    }
                });
            });

            section(ui, "Pegado y sistema", |ui| {
                settings_grid(ui, "sys", |ui| {
                    row(ui, "Restaurar el portapapeles", "vuelve a dejar lo que tenías copiado", |ui| {
                        ui.checkbox(&mut self.draft.paste.restore_clipboard, "");
                    });
                    row(ui, "Espera antes de restaurar", "tiempo que se le da a la app para leer el portapapeles", |ui| {
                        ui.add(egui::DragValue::new(&mut self.draft.paste.restore_delay_ms).range(50..=2000).suffix(" ms"));
                    });
                    row(ui, "Espacio al final", "", |ui| {
                        ui.checkbox(&mut self.draft.paste.trailing_space, "");
                    });
                    row(ui, "Sonidos de dictado", "", |ui| {
                        ui.checkbox(&mut self.draft.ui.sounds, "");
                    });
                    row(ui, "Guardar historial", "", |ui| {
                        ui.checkbox(&mut self.draft.history.enabled, "");
                    });
                });
            });
            ui.add_space(30.0);
        });
    }

    fn save_settings(&mut self) {
        self.settings_error = None;
        if let Some(e) = &self.chord_error {
            self.settings_error = Some(format!("Atajo inválido: {e}"));
            return;
        }
        if let Err(e) = self.draft.validate() {
            self.settings_error = Some(e.to_string());
            return;
        }
        if let Err(e) = self.draft.save(&self.paths.config_file) {
            self.settings_error = Some(format!("No se pudo guardar: {e:#}"));
            return;
        }
        self.shared.lock().unwrap().config = self.draft.clone();
        let _ = self.cmd_tx.send(PanelCommand::ApplyConfig(self.draft.clone()));
        self.settings_saved_at = Some(Instant::now());
        self.toast("Configuración aplicada");
    }

    // ------------------------------------------------------------ diagnostics

    fn diagnostics(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("Diagnóstico").size(22.0).strong());
        ui.add_space(8.0);
        let snapshot = {
            let s = self.shared.lock().unwrap();
            (
                s.state,
                s.backend_ready,
                s.backend_error.clone(),
                s.last_error.clone(),
                s.last_result.clone(),
                s.mic_name.clone(),
                s.chord_label.clone(),
                s.jobs_in_flight,
                s.events.iter().cloned().collect::<Vec<_>>(),
                s.config.stt.backend.clone(),
                s.config.format.backend.clone(),
            )
        };
        let (state, ready, backend_error, last_error, last_result, mic, chord, jobs, events, stt, fmt) = snapshot;

        card(ui, |ui| {
            egui::Grid::new("diag").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
                ui.label(RichText::new("Estado").color(MUTED));
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 5.0, state_color(state));
                    ui.label(state_label(state, ready));
                });
                ui.end_row();
                ui.label(RichText::new("Motor de voz").color(MUTED));
                ui.horizontal(|ui| {
                    ui.label(format!("{stt} · {}", if ready { "listo" } else { "cargando" }));
                    if ui.small_button("Recargar").clicked() {
                        let _ = self.cmd_tx.send(PanelCommand::ReloadBackend);
                    }
                });
                ui.end_row();
                ui.label(RichText::new("Formateo").color(MUTED));
                ui.label(fmt);
                ui.end_row();
                ui.label(RichText::new("Micrófono").color(MUTED));
                ui.label(mic);
                ui.end_row();
                ui.label(RichText::new("Atajo").color(MUTED));
                ui.label(chord);
                ui.end_row();
                ui.label(RichText::new("Trabajos en curso").color(MUTED));
                ui.label(jobs.to_string());
                ui.end_row();
                if let Some(e) = backend_error {
                    ui.label(RichText::new("Error del motor").color(ERR_RED));
                    ui.add(egui::Label::new(RichText::new(e).color(ERR_RED)).wrap());
                    ui.end_row();
                }
                if let Some(e) = last_error {
                    ui.label(RichText::new("Último error").color(ERR_RED));
                    ui.add(egui::Label::new(RichText::new(e).color(ERR_RED)).wrap());
                    ui.end_row();
                }
                if let Some(r) = last_result {
                    ui.label(RichText::new("Último dictado").color(MUTED));
                    ui.add(egui::Label::new(r).wrap());
                    ui.end_row();
                }
            });
        });

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("Abrir carpeta de logs").clicked() {
                open_path(&self.paths.log_dir);
            }
            if ui.button("Abrir carpeta de datos").clicked() {
                open_path(&self.paths.data_dir);
            }
            if ui.button("Recargar log").clicked() {
                self.log_loaded_at = None;
            }
        });
        ui.add_space(10.0);

        ui.columns(2, |cols| {
            cols[0].label(RichText::new("Eventos de la sesión").strong());
            egui::Frame::new().fill(CARD_BG).stroke(Stroke::new(1.0, CARD_STROKE)).corner_radius(CornerRadius::same(8)).inner_margin(Margin::same(8)).show(&mut cols[0], |ui| {
                egui::ScrollArea::vertical().id_salt("events").stick_to_bottom(true).max_height(360.0).auto_shrink([false, false]).show(ui, |ui| {
                    for e in &events {
                        ui.label(RichText::new(e).monospace().size(11.5));
                    }
                });
            });

            cols[1].label(RichText::new("Log del día (últimas líneas)").strong());
            if self.log_loaded_at.map(|t| t.elapsed().as_secs() >= 3).unwrap_or(true) {
                self.log_tail = read_log_tail(&self.paths.log_dir, 200);
                self.log_loaded_at = Some(Instant::now());
            }
            egui::Frame::new().fill(CARD_BG).stroke(Stroke::new(1.0, CARD_STROKE)).corner_radius(CornerRadius::same(8)).inner_margin(Margin::same(8)).show(&mut cols[1], |ui| {
                egui::ScrollArea::both().id_salt("logtail").stick_to_bottom(true).max_height(360.0).auto_shrink([false, false]).show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    for l in &self.log_tail {
                        let color = if l.contains("ERROR") { ERR_RED } else if l.contains("WARN") { WARN_ORANGE } else { Color32::from_rgb(60, 60, 70) };
                        ui.label(RichText::new(l).monospace().size(11.0).color(color));
                    }
                });
            });
        });
    }

    // ------------------------------------------------------------------ misc

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn draw_toast(&mut self, ctx: &egui::Context) {
        if let Some((msg, at)) = &self.toast {
            if at.elapsed().as_secs_f32() > 2.5 {
                self.toast = None;
                return;
            }
            egui::Area::new(egui::Id::unique("toast"))
                .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -24.0))
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(Color32::from_rgb(40, 40, 48))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(14, 8))
                        .show(ui, |ui| {
                            ui.label(RichText::new(msg).color(Color32::WHITE));
                        });
                });
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
        }
    }
}

// ---------------------------------------------------------------- helpers

const LANGS: &[(&str, &str)] = &[
    ("auto", "Auto detectar"),
    ("es", "Español"),
    ("en", "English"),
    ("pt", "Português"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("it", "Italiano"),
    ("ca", "Català"),
    ("ja", "日本語"),
    ("zh", "中文"),
];

fn lang_name(code: &str) -> String {
    LANGS.iter().find(|(c, _)| *c == code).map(|(_, n)| n.to_string()).unwrap_or_else(|| code.to_string())
}

fn state_color(s: AppState) -> Color32 {
    match s {
        AppState::Loading => Color32::from_rgb(170, 170, 178),
        AppState::Idle => Color32::from_rgb(90, 90, 100),
        AppState::Listening => ERR_RED,
        AppState::Locked => WARN_ORANGE,
        AppState::Processing => Color32::from_rgb(38, 139, 210),
        AppState::Error => Color32::from_rgb(181, 137, 0),
    }
}

fn state_label(s: AppState, ready: bool) -> &'static str {
    match s {
        AppState::Loading => "Cargando modelo…",
        AppState::Idle if ready => "Listo para dictar",
        AppState::Idle => "Esperando al motor…",
        AppState::Listening => "Escuchando",
        AppState::Locked => "Escuchando (manos libres)",
        AppState::Processing => "Transcribiendo…",
        AppState::Error => "Error (ver Diagnóstico)",
    }
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(CARD_BG)
        .stroke(Stroke::new(1.0, CARD_STROKE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).size(16.0).strong());
    ui.add_space(4.0);
    card(ui, add);
    ui.add_space(8.0);
}

fn settings_grid(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id).num_columns(2).spacing([28.0, 12.0]).min_col_width(220.0).show(ui, add);
}

fn row(ui: &mut egui::Ui, label: &str, hint: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.vertical(|ui| {
        ui.label(RichText::new(label).strong());
        if !hint.is_empty() {
            ui.label(RichText::new(hint).small().color(MUTED));
        }
    });
    ui.horizontal_wrapped(add);
    ui.end_row();
}

fn stat_tile(ui: &mut egui::Ui, width: f32, value: &str, label: &str) {
    egui::Frame::new()
        .fill(CARD_BG)
        .stroke(Stroke::new(1.0, CARD_STROKE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.set_width((width - 34.0).max(80.0));
            ui.vertical(|ui| {
                ui.label(RichText::new(value).size(22.0).strong());
                ui.add(egui::Label::new(RichText::new(label).small().color(MUTED)).wrap());
            });
        });
}

fn chip(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(color));
        });
}

fn parse_ts(ts: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(ts).ok().map(|t| t.with_timezone(&Local))
}

fn day_label(ts: &str) -> String {
    let Some(t) = parse_ts(ts) else { return "Fecha desconocida".into() };
    let today = Local::now().date_naive();
    let d = t.date_naive();
    if d == today {
        "Hoy".into()
    } else if d == today - chrono::Duration::days(1) {
        "Ayer".into()
    } else if d.year() == today.year() {
        t.format("%A %d de %B").to_string()
    } else {
        t.format("%d/%m/%Y").to_string()
    }
}

fn time_label(ts: &str) -> String {
    parse_ts(ts).map(|t| t.format("%H:%M").to_string()).unwrap_or_else(|| "--:--".into())
}

fn app_label(process: &str) -> String {
    let base = process.trim_end_matches(".exe").trim_end_matches(".EXE");
    if base.is_empty() {
        return "App desconocida".into();
    }
    let mut c = base.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

fn format_int(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push('.');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn read_log_tail(dir: &std::path::Path, n: usize) -> Vec<String> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    files.sort();
    let Some(latest) = files.last() else { return vec!["(sin logs todavía)".into()] };
    let Ok(text) = std::fs::read_to_string(latest) else { return vec![] };
    let lines: Vec<&str> = text.lines().collect();
    lines.iter().rev().take(n).rev().map(|l| l.to_string()).collect()
}

pub fn open_path(path: &std::path::Path) {
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", &path.to_string_lossy()]).spawn();
}
