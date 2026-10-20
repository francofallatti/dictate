//! Dictate — push-to-talk voice dictation for Windows.
//!
//! Hold the chord (default Ctrl+Win) → speak → release. The audio is transcribed
//! (whisper.cpp locally or OpenAI), optionally cleaned up by an LLM, and pasted into the
//! focused application. A hub window (history, settings, diagnostics) runs on the main thread;
//! the dictation orchestrator runs on its own thread.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod format;
mod gesture;
mod history;
mod hotkey;
mod inject;
mod panel;
mod stt;
mod ui;

use anyhow::{Context, Result};
use config::{Config, Paths};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use format::{FormatContext, Formatter};
use gesture::{Action, Gesture, Input};
use hotkey::{Chord, HookEvent};
use panel::{PanelCommand, PanelLink, Shared, SharedState};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stt::{Hint, Transcriber};
use tracing::{error, info, warn};
use ui::{AppState, Sound, UiEvent};

fn main() {
    if let Err(e) = run() {
        eprintln!("dictate: {e:#}");
        error!("{e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = Paths::discover()?;
    paths.ensure_dirs()?;

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help(&paths);
        return Ok(());
    }

    let _log_guard = init_logging(&paths, args.iter().any(|a| a == "--verbose"))?;
    let (cfg, created) = Config::load_or_create(&paths.config_file)?;
    cfg.validate()?;
    if created {
        info!(path = %paths.config_file.display(), "created default config");
    }

    match args.first().map(String::as_str) {
        Some("--list-devices") => {
            for d in audio::list_input_devices()? {
                println!("{d}");
            }
            Ok(())
        }
        Some("--config") => {
            println!("{}", paths.config_file.display());
            Ok(())
        }
        Some("--paste-text") => {
            let text = args.get(1).context("usage: dictate --paste-text <text>")?;
            inject::paste_text(text, &cfg.paste)
        }
        Some("--transcribe") => {
            let file = args.get(1).context("usage: dictate --transcribe <file.wav> [--paste]")?;
            let paste = args.iter().any(|a| a == "--paste");
            transcribe_file(&cfg, &paths, std::path::Path::new(file), paste)
        }
        Some(other) if other.starts_with("--") && !matches!(other, "--verbose" | "--panel") => {
            anyhow::bail!("unknown option {other}; try --help")
        }
        _ => {
            let show_panel = created || args.iter().any(|a| a == "--panel");
            run_app(cfg, paths, show_panel)
        }
    }
}

fn print_help(paths: &Paths) {
    println!(
        "Dictate — push-to-talk dictation\n\n\
USAGE:\n  dictate                    run in the tray (hold the chord to dictate)\n  \
dictate --panel            same, and open the hub window right away\n  \
dictate --transcribe F.wav [--paste]   transcribe a WAV file and print (or paste) the result\n  \
dictate --paste-text TEXT  paste TEXT into the focused app (tests the injection path)\n  \
dictate --list-devices     list microphones\n  dictate --config           print the config path\n  \
dictate --verbose          debug logging\n\nCONFIG: {}\nLOGS:   {}\nMODELS: {}",
        paths.config_file.display(),
        paths.log_dir.display(),
        paths.models_dir.display()
    );
}

fn init_logging(paths: &Paths, verbose: bool) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    let default = if verbose { "dictate=debug,whisper_rs=info,info" } else { "dictate=info,whisper_rs=warn,warn" };
    let filter = EnvFilter::try_from_env("DICTATE_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let file_appender = tracing_appender::rolling::daily(&paths.log_dir, "dictate.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr).with_target(false))
        .with(fmt::layer().with_writer(file_writer).with_ansi(false).with_target(false))
        .init();
    Ok(guard)
}

/// `--transcribe`: run the pipeline on a WAV file. Useful to test backends without a hotkey.
fn transcribe_file(cfg: &Config, paths: &Paths, file: &std::path::Path, paste: bool) -> Result<()> {
    let pcm = stt::read_wav_16k(file).with_context(|| format!("reading {}", file.display()))?;
    info!(secs = pcm.len() as f32 / audio::TARGET_RATE as f32, "loaded audio");
    let transcriber = stt::build(cfg, paths)?;
    let formatter = format::build(cfg)?;
    let hint = Hint { language: cfg.stt.language.clone(), initial_prompt: cfg.stt.initial_prompt.clone() };
    let t0 = Instant::now();
    let raw = transcriber.transcribe(&pcm, &hint)?;
    let stt_ms = t0.elapsed().as_millis();
    println!("[{} in {stt_ms} ms] {raw}", transcriber.name());
    if formatter.name() != "none" {
        let t1 = Instant::now();
        let ctx = FormatContext { language: cfg.stt.language.clone(), ..Default::default() };
        let formatted = formatter.format(&raw, &ctx)?;
        println!("[{} in {} ms] {formatted}", formatter.name(), t1.elapsed().as_millis());
        if paste {
            inject::paste_text(&formatted, &cfg.paste)?;
        }
    } else if paste {
        inject::paste_text(&raw, &cfg.paste)?;
    }
    Ok(())
}

/// Result of one background dictation job.
struct JobDone {
    ok: bool,
    summary: String,
}

/// Everything a dictation job needs; rebuilt when settings change.
struct Pipeline {
    cfg: Arc<Config>,
    paths: Arc<Paths>,
    transcriber: Arc<dyn Transcriber>,
    formatter: Arc<dyn Formatter>,
    shared: SharedState,
    link: PanelLink,
}

impl Pipeline {
    fn note(&self, line: impl AsRef<str>) {
        self.shared.lock().unwrap().event(line);
        self.link.repaint();
    }

    fn run(&self, pcm: Vec<f32>, app: inject::ForegroundApp) -> JobDone {
        let audio_secs = pcm.len() as f32 / audio::TARGET_RATE as f32;
        let hint = Hint { language: self.cfg.stt.language.clone(), initial_prompt: self.cfg.stt.initial_prompt.clone() };
        self.note(format!("transcribiendo {audio_secs:.1}s con {}", self.transcriber.name()));
        let t0 = Instant::now();
        let raw = match self.transcriber.transcribe(&pcm, &hint) {
            Ok(t) => t,
            Err(e) => {
                error!(error = %format!("{e:#}"), "transcription failed");
                let msg = format!("transcripción falló: {e:#}");
                self.shared.lock().unwrap().last_error = Some(msg.clone());
                self.note(&msg);
                return JobDone { ok: false, summary: msg };
            }
        };
        let stt_ms = t0.elapsed().as_millis() as u64;
        if raw.trim().is_empty() {
            info!(audio_secs, "nothing recognised");
            self.note(format!("sin palabras reconocidas ({stt_ms} ms)"));
            return JobDone { ok: true, summary: "nothing recognised".into() };
        }
        self.note(format!("texto ({stt_ms} ms): {}", preview(&raw)));

        let ctx = FormatContext { app_name: app.process.clone(), window_title: app.title.clone(), language: self.cfg.stt.language.clone() };
        let t1 = Instant::now();
        let final_text = match self.formatter.format(&raw, &ctx) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %format!("{e:#}"), "formatting failed; pasting raw transcript");
                self.note(format!("formateo falló, se pega el texto crudo: {e}"));
                raw.clone()
            }
        };
        let format_ms = t1.elapsed().as_millis() as u64;

        let mut to_paste = final_text.clone();
        if self.cfg.paste.trailing_space && !to_paste.ends_with(char::is_whitespace) {
            to_paste.push(' ');
        }
        let pasted = match inject::paste_text(&to_paste, &self.cfg.paste) {
            Ok(()) => {
                self.note(format!("pegado en {}", if app.process.is_empty() { "app desconocida" } else { &app.process }));
                true
            }
            Err(e) => {
                error!(error = %format!("{e:#}"), "paste failed");
                let msg = format!("pegado falló: {e:#}");
                self.shared.lock().unwrap().last_error = Some(msg.clone());
                self.note(&msg);
                false
            }
        };

        if self.cfg.history.enabled {
            let entry = history::HistoryEntry {
                timestamp: history::now_iso(),
                app: &app.process,
                window_title: &app.title,
                audio_secs,
                stt_backend: self.transcriber.name(),
                format_backend: self.formatter.name(),
                raw_text: &raw,
                final_text: &final_text,
                stt_ms,
                format_ms,
                pasted,
            };
            if let Err(e) = history::append(&self.paths.history_file, &entry) {
                warn!(error = %e, "could not write history");
                self.note(format!("no se pudo escribir el historial: {e}"));
            }
        }
        {
            let mut s = self.shared.lock().unwrap();
            s.history_dirty = true;
            s.last_result = Some(final_text.clone());
        }
        info!(audio_secs, stt_ms, format_ms, pasted, app = %app.process, "dictation complete");
        JobDone { ok: pasted, summary: final_text }
    }
}

fn preview(s: &str) -> String {
    let t: String = s.chars().take(70).collect();
    if s.chars().count() > 70 { format!("{t}…") } else { t }
}

/// Builds the speech + formatting backends and warms them up on a helper thread.
fn build_pipeline(cfg: &Arc<Config>, paths: &Arc<Paths>, shared: &SharedState, link: &PanelLink, warm_tx: &Sender<Result<()>>) -> Result<Arc<Pipeline>> {
    let transcriber = stt::build(cfg, paths)?;
    let formatter = format::build(cfg)?;
    let pipeline = Arc::new(Pipeline {
        cfg: Arc::clone(cfg),
        paths: Arc::clone(paths),
        transcriber: Arc::clone(&transcriber),
        formatter,
        shared: Arc::clone(shared),
        link: link.clone(),
    });
    let tx = warm_tx.clone();
    std::thread::Builder::new()
        .name("dictate-warmup".into())
        .spawn(move || {
            let _ = tx.send(transcriber.warm_up());
        })?;
    Ok(pipeline)
}

fn run_app(cfg: Config, paths: Paths, show_panel: bool) -> Result<()> {
    let chord = Chord::parse(&cfg.hotkey.chord)?;
    let chord_label = chord.describe();
    let paths = Arc::new(paths);
    let shared: SharedState = Arc::new(std::sync::Mutex::new(Shared::new(cfg.clone(), chord_label.clone())));
    let link = PanelLink::default();
    let (cmd_tx, cmd_rx) = unbounded::<PanelCommand>();

    // The orchestrator owns the hook/tray thread, the recorder and the gesture state machine.
    let core = {
        let shared = Arc::clone(&shared);
        let link = link.clone();
        let paths = Arc::clone(&paths);
        std::thread::Builder::new()
            .name("dictate-core".into())
            .spawn(move || {
                if let Err(e) = orchestrate(cfg, paths, chord, chord_label, shared.clone(), link.clone(), cmd_rx) {
                    error!("{e:#}");
                    shared.lock().unwrap().event(format!("error fatal: {e:#}"));
                    eprintln!("dictate: {e:#}");
                }
                // Whatever the reason, when the core stops the process must stop too.
                shared.lock().unwrap().quitting = true;
                link.close();
                std::thread::sleep(Duration::from_millis(500));
                std::process::exit(0);
            })?
    };

    // eframe must run on the main thread. It returns only on a real quit.
    if let Err(e) = panel::run(Arc::clone(&shared), paths, cmd_tx, link, show_panel) {
        error!(error = %e, "panel failed");
    }
    drop(core);
    std::process::exit(0);
}

fn orchestrate(
    cfg: Config,
    paths: Arc<Paths>,
    chord: Chord,
    chord_label: String,
    shared: SharedState,
    link: PanelLink,
    cmd_rx: Receiver<PanelCommand>,
) -> Result<()> {
    let mut cfg = Arc::new(cfg);
    info!(chord = %chord_label, stt = %cfg.stt.backend, format = %cfg.format.backend, "starting Dictate");
    let note = |s: &str| {
        shared.lock().unwrap().event(s);
        link.repaint();
    };

    let (hook_tx, hook_rx) = unbounded::<HookEvent>();
    let (ui_tx, ui_rx) = unbounded::<UiEvent>();
    let (done_tx, done_rx) = unbounded::<JobDone>();
    let (warm_tx, warm_rx) = unbounded::<Result<()>>();

    let ui = ui::spawn(
        ui::UiOptions { chord, escape_cancels: cfg.hotkey.escape_cancels, tray: cfg.ui.tray, chord_label: chord_label.clone() },
        hook_tx,
        ui_tx,
    )?;
    ui.set_state(AppState::Loading);
    note(&format!("iniciado · atajo {chord_label}"));

    let mut pipeline = build_pipeline(&cfg, &paths, &shared, &link, &warm_tx)?;
    note(&format!("cargando motor {}", cfg.stt.backend));

    let mut recorder = audio::Recorder::new(&cfg.audio.device)?;
    info!(device = %recorder.device_name(), "microphone selected");
    shared.lock().unwrap().mic_name = recorder.device_name();

    let mut gesture = Gesture::new(
        Duration::from_millis(cfg.hotkey.double_tap_ms),
        Duration::from_secs(cfg.hotkey.max_duration_secs.max(5)),
    );
    let mut ready = false;
    let mut jobs_in_flight: usize = 0;
    let mut current_app = inject::ForegroundApp::default();
    let mut sounds = cfg.ui.sounds;

    let set_state = |state: AppState| {
        ui.set_state(state);
        shared.lock().unwrap().state = state;
        link.repaint();
    };

    loop {
        let timeout = gesture
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(3600));

        let input: Option<Input> = select! {
            recv(hook_rx) -> ev => match ev {
                Ok(HookEvent::ChordDown) => Some(Input::ChordDown),
                Ok(HookEvent::ChordUp) => Some(Input::ChordUp),
                Ok(HookEvent::Escape) => Some(Input::Escape),
                Err(_) => { error!("keyboard hook channel closed"); break; }
            },
            recv(ui_rx) -> ev => {
                match ev {
                    Ok(UiEvent::Quit) | Err(_) => break,
                    Ok(UiEvent::OpenPanel) => link.show(),
                    Ok(UiEvent::OpenConfig) => panel::open_path(&paths.config_file),
                    Ok(UiEvent::OpenHistory) => panel::open_path(&paths.history_file),
                }
                None
            },
            recv(cmd_rx) -> cmd => {
                match cmd {
                    Ok(PanelCommand::Quit) | Err(_) => break,
                    Ok(PanelCommand::ReloadBackend) => {
                        ready = false;
                        set_state(AppState::Loading);
                        match build_pipeline(&cfg, &paths, &shared, &link, &warm_tx) {
                            Ok(p) => { pipeline = p; note("recargando motor"); }
                            Err(e) => { note(&format!("no se pudo recargar el motor: {e:#}")); set_state(AppState::Error); }
                        }
                    }
                    Ok(PanelCommand::ApplyConfig(new_cfg)) => {
                        note("aplicando configuración nueva");
                        let old = Arc::clone(&cfg);
                        cfg = Arc::new(new_cfg);
                        sounds = cfg.ui.sounds;
                        gesture = Gesture::new(
                            Duration::from_millis(cfg.hotkey.double_tap_ms),
                            Duration::from_secs(cfg.hotkey.max_duration_secs.max(5)),
                        );
                        match Chord::parse(&cfg.hotkey.chord) {
                            Ok(c) => {
                                let label = c.describe();
                                if let Err(e) = hotkey::update_chord(c, cfg.hotkey.escape_cancels) {
                                    note(&format!("no se pudo cambiar el atajo: {e}"));
                                } else {
                                    shared.lock().unwrap().chord_label = label.clone();
                                    note(&format!("atajo ahora: {label}"));
                                }
                            }
                            Err(e) => note(&format!("atajo inválido, se mantiene el anterior: {e}")),
                        }
                        if cfg.audio.device != old.audio.device && !gesture.is_recording() {
                            match audio::Recorder::new(&cfg.audio.device) {
                                Ok(r) => { recorder = r; shared.lock().unwrap().mic_name = recorder.device_name(); note(&format!("micrófono: {}", recorder.device_name())); }
                                Err(e) => note(&format!("no se pudo abrir el micrófono: {e:#}")),
                            }
                        }
                        let backend_changed = cfg.stt != old.stt || cfg.format != old.format;
                        if backend_changed {
                            ready = false;
                            set_state(AppState::Loading);
                            match build_pipeline(&cfg, &paths, &shared, &link, &warm_tx) {
                                Ok(p) => pipeline = p,
                                Err(e) => { note(&format!("configuración de motor inválida: {e:#}")); shared.lock().unwrap().backend_error = Some(format!("{e:#}")); set_state(AppState::Error); }
                            }
                        } else {
                            // Only paste/history/ui settings changed: rebuild the pipeline with the new config,
                            // keeping the already-loaded backends.
                            pipeline = Arc::new(Pipeline {
                                cfg: Arc::clone(&cfg),
                                paths: Arc::clone(&paths),
                                transcriber: Arc::clone(&pipeline.transcriber),
                                formatter: Arc::clone(&pipeline.formatter),
                                shared: Arc::clone(&shared),
                                link: link.clone(),
                            });
                        }
                    }
                }
                None
            },
            recv(warm_rx) -> r => {
                match r {
                    Ok(Ok(())) => {
                        ready = true;
                        info!("backend ready");
                        { let mut s = shared.lock().unwrap(); s.backend_ready = true; s.backend_error = None; }
                        note("motor listo");
                        if !gesture.is_recording() { set_state(AppState::Idle); }
                    }
                    Ok(Err(e)) => {
                        error!(error = %format!("{e:#}"), "backend warm-up failed");
                        { let mut s = shared.lock().unwrap(); s.backend_ready = false; s.backend_error = Some(format!("{e:#}")); }
                        note(&format!("el motor no pudo cargar: {e:#}"));
                        set_state(AppState::Error);
                        if sounds { ui::play(Sound::Error); }
                    }
                    Err(_) => {}
                }
                None
            },
            recv(done_rx) -> r => {
                jobs_in_flight = jobs_in_flight.saturating_sub(1);
                if let Ok(d) = r {
                    if !d.ok { if sounds { ui::play(Sound::Error); } }
                    info!(summary = %preview(&d.summary), "job finished");
                }
                shared.lock().unwrap().jobs_in_flight = jobs_in_flight;
                if !gesture.is_recording() {
                    set_state(if jobs_in_flight > 0 { AppState::Processing } else { AppState::Idle });
                }
                None
            },
            default(timeout) => Some(Input::Tick),
        };

        let Some(input) = input else { continue };
        let Some(action) = gesture.handle(input, Instant::now()) else { continue };

        match action {
            Action::Start => {
                if !ready {
                    warn!("chord pressed before the speech backend finished loading");
                    note("atajo presionado pero el motor todavía no está listo");
                    if sounds { ui::play(Sound::Error); }
                    continue;
                }
                current_app = inject::foreground_app();
                match recorder.start() {
                    Ok(()) => {
                        set_state(AppState::Listening);
                        if sounds { ui::play(Sound::Start); }
                        info!(app = %current_app.process, title = %current_app.title, "recording started");
                        note(&format!("grabando · app: {}", if current_app.process.is_empty() { "?" } else { &current_app.process }));
                    }
                    Err(e) => {
                        error!(error = %format!("{e:#}"), "could not start recording");
                        shared.lock().unwrap().last_error = Some(format!("micrófono: {e:#}"));
                        note(&format!("no se pudo grabar: {e:#}"));
                        set_state(AppState::Error);
                        if sounds { ui::play(Sound::Error); }
                    }
                }
            }
            Action::Locked => {
                set_state(AppState::Locked);
                info!("hands-free mode");
                note("manos libres activado");
            }
            Action::Stop => {
                let pcm = recorder.stop();
                let secs = pcm.len() as f32 / audio::TARGET_RATE as f32;
                let level = audio::rms_dbfs(&pcm);
                info!(secs, level_dbfs = level, "recording stopped");
                if secs < 0.4 || level < -70.0 {
                    info!("recording too short or silent; discarded");
                    note(&format!("grabación descartada ({secs:.1}s, {level:.0} dBFS)"));
                    if sounds { ui::play(Sound::Cancel); }
                    set_state(if jobs_in_flight > 0 { AppState::Processing } else { AppState::Idle });
                    continue;
                }
                note(&format!("grabación de {secs:.1}s ({level:.0} dBFS), procesando"));
                if sounds { ui::play(Sound::Stop); }
                set_state(AppState::Processing);
                jobs_in_flight += 1;
                shared.lock().unwrap().jobs_in_flight = jobs_in_flight;
                spawn_job(Arc::clone(&pipeline), pcm, current_app.clone(), done_tx.clone());
            }
            Action::Cancel => {
                let _ = recorder.stop();
                info!("recording cancelled");
                note("grabación cancelada");
                if sounds { ui::play(Sound::Cancel); }
                set_state(if jobs_in_flight > 0 { AppState::Processing } else { AppState::Idle });
            }
        }
    }

    info!("shutting down");
    note("cerrando");
    if recorder.is_recording() {
        let _ = recorder.stop();
    }
    ui.shutdown();
    Ok(())
}

fn spawn_job(pipeline: Arc<Pipeline>, pcm: Vec<f32>, app: inject::ForegroundApp, done: Sender<JobDone>) {
    let r = std::thread::Builder::new().name("dictate-job".into()).spawn(move || {
        let result = pipeline.run(pcm, app);
        let _ = done.send(result);
    });
    if let Err(e) = r {
        error!(error = %e, "could not spawn job thread");
    }
}
