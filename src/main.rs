//! Dictate — push-to-talk voice dictation for Windows.
//!
//! Hold the chord (default Ctrl+Win) → speak → release. The audio is transcribed
//! (whisper.cpp locally or OpenAI), optionally cleaned up by an LLM, and pasted into the
//! focused application.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod format;
mod gesture;
mod history;
mod hotkey;
mod inject;
mod stt;
mod ui;

use anyhow::{Context, Result};
use config::{Config, Paths};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use format::{FormatContext, Formatter};
use gesture::{Action, Gesture, Input};
use hotkey::{Chord, HookEvent};
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
        Some(other) if other.starts_with("--") && other != "--verbose" => {
            anyhow::bail!("unknown option {other}; try --help")
        }
        _ => run_app(cfg, paths),
    }
}

fn print_help(paths: &Paths) {
    println!(
        "Dictate — push-to-talk dictation\n\n\
USAGE:\n  dictate                    run in the tray (hold the chord to dictate)\n  \
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

struct Pipeline {
    cfg: Arc<Config>,
    paths: Arc<Paths>,
    transcriber: Arc<dyn Transcriber>,
    formatter: Arc<dyn Formatter>,
}

impl Pipeline {
    fn run(&self, pcm: Vec<f32>, app: inject::ForegroundApp) -> JobDone {
        let audio_secs = pcm.len() as f32 / audio::TARGET_RATE as f32;
        let hint = Hint { language: self.cfg.stt.language.clone(), initial_prompt: self.cfg.stt.initial_prompt.clone() };
        let t0 = Instant::now();
        let raw = match self.transcriber.transcribe(&pcm, &hint) {
            Ok(t) => t,
            Err(e) => {
                error!(error = %format!("{e:#}"), "transcription failed");
                return JobDone { ok: false, summary: format!("transcription failed: {e}") };
            }
        };
        let stt_ms = t0.elapsed().as_millis() as u64;
        if raw.trim().is_empty() {
            info!(audio_secs, "nothing recognised");
            return JobDone { ok: true, summary: "nothing recognised".into() };
        }

        let ctx = FormatContext { app_name: app.process.clone(), window_title: app.title.clone(), language: self.cfg.stt.language.clone() };
        let t1 = Instant::now();
        let final_text = match self.formatter.format(&raw, &ctx) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %format!("{e:#}"), "formatting failed; pasting raw transcript");
                raw.clone()
            }
        };
        let format_ms = t1.elapsed().as_millis() as u64;

        let mut to_paste = final_text.clone();
        if self.cfg.paste.trailing_space && !to_paste.ends_with(char::is_whitespace) {
            to_paste.push(' ');
        }
        let pasted = match inject::paste_text(&to_paste, &self.cfg.paste) {
            Ok(()) => true,
            Err(e) => {
                error!(error = %format!("{e:#}"), "paste failed");
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
            }
        }
        info!(audio_secs, stt_ms, format_ms, pasted, app = %app.process, "dictation complete");
        JobDone { ok: pasted, summary: final_text }
    }
}

fn run_app(cfg: Config, paths: Paths) -> Result<()> {
    let cfg = Arc::new(cfg);
    let paths = Arc::new(paths);
    let chord = Chord::parse(&cfg.hotkey.chord)?;
    let chord_label = chord.describe();
    info!(chord = %chord_label, stt = %cfg.stt.backend, format = %cfg.format.backend, "starting Dictate");

    let transcriber = stt::build(&cfg, &paths)?;
    let formatter = format::build(&cfg)?;
    let pipeline = Arc::new(Pipeline { cfg: Arc::clone(&cfg), paths: Arc::clone(&paths), transcriber: Arc::clone(&transcriber), formatter });

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

    // Model download / load happens off the main loop so the tray appears immediately.
    {
        let t = Arc::clone(&transcriber);
        std::thread::Builder::new()
            .name("dictate-warmup".into())
            .spawn(move || {
                let _ = warm_tx.send(t.warm_up());
            })?;
    }

    let mut recorder = audio::Recorder::new(&cfg.audio.device)?;
    info!(device = %recorder.device_name(), "microphone selected");

    let mut gesture = Gesture::new(
        Duration::from_millis(cfg.hotkey.double_tap_ms),
        Duration::from_secs(cfg.hotkey.max_duration_secs.max(5)),
    );
    let mut ready = false;
    let mut jobs_in_flight: usize = 0;
    let mut current_app = inject::ForegroundApp::default();
    let sounds = cfg.ui.sounds;

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
                    Ok(UiEvent::OpenConfig) => open_path(&paths.config_file),
                    Ok(UiEvent::OpenHistory) => open_path(&paths.history_file),
                }
                None
            },
            recv(warm_rx) -> r => {
                match r {
                    Ok(Ok(())) => { ready = true; info!("backend ready"); if !gesture.is_recording() { ui.set_state(AppState::Idle); } }
                    Ok(Err(e)) => { error!(error = %format!("{e:#}"), "backend warm-up failed"); ui.set_state(AppState::Error); if sounds { ui::play(Sound::Error); } }
                    Err(_) => {}
                }
                None
            },
            recv(done_rx) -> r => {
                jobs_in_flight = jobs_in_flight.saturating_sub(1);
                if let Ok(d) = r {
                    if !d.ok { if sounds { ui::play(Sound::Error); } }
                    info!(summary = %d.summary.chars().take(80).collect::<String>(), "job finished");
                }
                if !gesture.is_recording() {
                    ui.set_state(if jobs_in_flight > 0 { AppState::Processing } else { AppState::Idle });
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
                    if sounds { ui::play(Sound::Error); }
                    continue;
                }
                current_app = inject::foreground_app();
                match recorder.start() {
                    Ok(()) => {
                        ui.set_state(AppState::Listening);
                        if sounds { ui::play(Sound::Start); }
                        info!(app = %current_app.process, title = %current_app.title, "recording started");
                    }
                    Err(e) => {
                        error!(error = %format!("{e:#}"), "could not start recording");
                        ui.set_state(AppState::Error);
                        if sounds { ui::play(Sound::Error); }
                    }
                }
            }
            Action::Locked => {
                ui.set_state(AppState::Locked);
                info!("hands-free mode");
            }
            Action::Stop => {
                let pcm = recorder.stop();
                let secs = pcm.len() as f32 / audio::TARGET_RATE as f32;
                let level = audio::rms_dbfs(&pcm);
                info!(secs, level_dbfs = level, "recording stopped");
                if secs < 0.4 || level < -70.0 {
                    info!("recording too short or silent; discarded");
                    if sounds { ui::play(Sound::Cancel); }
                    ui.set_state(if jobs_in_flight > 0 { AppState::Processing } else { AppState::Idle });
                    continue;
                }
                if sounds { ui::play(Sound::Stop); }
                ui.set_state(AppState::Processing);
                jobs_in_flight += 1;
                spawn_job(Arc::clone(&pipeline), pcm, current_app.clone(), done_tx.clone());
            }
            Action::Cancel => {
                let _ = recorder.stop();
                info!("recording cancelled");
                if sounds { ui::play(Sound::Cancel); }
                ui.set_state(if jobs_in_flight > 0 { AppState::Processing } else { AppState::Idle });
            }
        }
    }

    info!("shutting down");
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

fn open_path(path: &std::path::Path) {
    if !path.exists() {
        if let Err(e) = std::fs::write(path, "") {
            warn!(error = %e, "cannot create {}", path.display());
        }
    }
    if let Err(e) = std::process::Command::new("cmd").args(["/C", "start", "", &path.to_string_lossy()]).spawn() {
        warn!(error = %e, "cannot open {}", path.display());
    }
}

#[allow(dead_code)]
fn _assert_send(_: &Receiver<JobDone>) {}
