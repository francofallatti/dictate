# CLAUDE.md — Dictate

Push-to-talk voice dictation for Windows in Rust, modelled on Wispr Flow's "Dictate" mode
(analysed with REA from the installed Electron app). Hold a chord → speak → release → text is
transcribed and pasted into the focused app. Single binary, tray app with a hub window.

## Decisions already made (ask before changing)

- **Windows only** for now. Win32 via the `windows` crate; no cross-platform abstraction yet.
- **STT behind `stt::Transcriber`**: `whisper` (whisper.cpp in-process via `whisper-rs`, default,
  model auto-downloaded from Hugging Face) and `openai` (multipart `/v1/audio/transcriptions`).
- **LLM clean-up behind `format::Formatter`**: `none` by default; `claude` (Messages API over raw
  HTTP, `claude-haiku-5-5`, `output_config.effort = low`) or `openai` (chat completions).
  There is no Rust SDK for Claude; keep the raw-HTTP client in `src/format/claude.rs`.
- **UI language is Spanish (rioplatense: "mantené", "tocá")**, code and comments in English.
- **No commits unless the user asks.** The repo has `git init` but no history yet.

## Layout

```
src/main.rs        CLI (--panel, --transcribe, --paste-text, --list-devices), wiring,
                   `orchestrate()` = dictation state loop on the `dictate-core` thread,
                   `Pipeline` = transcribe → format → paste → history (one thread per job)
src/panel.rs       Hub window (eframe/egui 0.37, MAIN thread): Inicio (history + stats),
                   Configuración (edits config.toml, hot-reloads), Diagnóstico (live state,
                   events, log tail). Theme palette via is_dark()/pick(); sun/moon painted.
src/ui.rs          "ui thread": Win32 message loop hosting the keyboard hook + tray icon.
                   Main thread posts WM_APP messages to it (set_state / shutdown).
src/hotkey.rs      WH_KEYBOARD_LL hook, chord parsing ("ctrl + win"), Win-key masking,
                   stale-key reconciliation with GetAsyncKeyState, runtime chord update.
src/gesture.rs     Pure state machine: hold = PTT, quick double tap = hands-free lock,
                   triple tap / Esc = cancel, 500 ms debounce, max duration. Unit-tested.
src/audio.rs       cpal capture, always-on stream with a pre-roll ring buffer, 16 kHz mono.
src/stt/, src/format/   backends (see above)
src/inject.rs      foreground app detection, clipboard save → Ctrl+V (SendInput) → restore,
                   clipboard retries, waits for modifiers to be released first.
src/perf.rs        opts the process out of Windows power throttling (EcoQoS), boosts job thread.
src/history.rs     JSONL history (append / read_all / delete).
src/config.rs      config.toml schema + defaults + `Paths`.
vendor/whisper-rs-sys   vendored crate with a build.rs patch (see below). Do not drop it.
```

Runtime files: config `%APPDATA%\Dictate\config\config.toml`; models, logs and
`history.jsonl` under `%APPDATA%\Dictate\data\`. `dictate --help` prints the exact paths.

## Build (the non-obvious parts)

- `cargo build` / `cargo build --release`. Rust MSVC toolchain + VS 2022 C++ tools + CMake.
- CMake was installed with winget as a *portable* package and is not on PATH in every shell;
  `.cargo/config.toml` points `CMAKE` at it. Update that path if CMake moves.
- No libclang on this machine → `WHISPER_DONT_GENERATE_BINDINGS=1` (pregenerated bindings).
  Those bindings were generated on Linux, so `vendor/whisper-rs-sys/build.rs` strips the glibc
  layout asserts and retypes `ggml_*`/`whisper_*` enum aliases to `i32` (MSVC shape).
  `[patch.crates-io]` in Cargo.toml points at the vendored copy.
- **The `cmake` crate drops `/O2`** when it overrides CMake's per-config flags. Without the
  `CMAKE_*_FLAGS_RELEASE` / `_RELWITHDEBINFO` variables in `.cargo/config.toml`, whisper.cpp is
  built at `/Od` and a 7 s clip takes ~35 s instead of ~5 s. Changing those env vars does
  **not** trigger a rebuild: run `cargo clean -p whisper-rs-sys` first.
- `[profile.dev.package.whisper-rs-sys] opt-level = 3` keeps debug builds usable.
- If the user's instance is running, `target/release/dictate.exe` is locked; build into
  `CARGO_TARGET_DIR=target-rel` instead (git-ignored) and point the user at that binary.

## Test and verify

- `cargo test` — 15 unit tests (gesture, chord parsing, downsampler, ring buffer, cleaners).
- `dictate --transcribe <wav>` exercises STT (+ formatter) without a hotkey. Test clips live in
  the session scratchpad (`test_es.wav`, `test_en.wav`, generated with Windows TTS).
- `dictate --paste-text "x"` exercises only clipboard + Ctrl+V. **Both `--paste*` commands type
  into whatever window is in the foreground** — never run them on the live desktop; use a
  WinForms text box harness that first takes the foreground (AttachThreadInput trick).
- Panel screenshots: DPI-aware PowerShell (`SetProcessDPIAware`), `CopyFromScreen` of the client
  rect, clicks via `SetCursorPos` + `mouse_event`. **Find the window by the harness's own
  process id**, not by title — the user usually has their own Dictate panel open.
- The hook ignores injected input (`LLKHF_INJECTED`), so the chord cannot be simulated; the
  tray/hotkey path is verified by the user pressing keys.
- Performance numbers are only meaningful with no `cargo build` running. Baseline on this
  Core Ultra 7 155U laptop in "Better battery" mode: ~5 s per 7 s clip with `base`, 4 threads.

## Gotchas

- egui 0.37: `eframe::App::ui(&mut self, ui, frame)` (not `update`), `egui::Panel::left(..)`
  with `exact_size`, `Button::selectable`, `Frame::new()` has no fill, `Id::unique`.
- More than ~4 ggml threads is slower on hybrid CPUs; 12 threads once took 40 s.
- Close on the panel window only hides it; a real quit sets `Shared::quitting` and closes it.
- Config hot-reload: `PanelCommand::ApplyConfig` rebuilds gesture/chord/recorder/backends as
  needed; keep that branch in `orchestrate()` in sync when adding config fields, and add the
  field to the Configuración page in `panel.rs`.
- Keep `README.md` in sync with user-facing behaviour; it is the user manual.
