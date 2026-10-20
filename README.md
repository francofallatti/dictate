# Dictate

Push-to-talk voice dictation for Windows, written in Rust. Hold a chord (default **Ctrl + Win**),
speak, release: the audio is transcribed and the text is pasted into whatever app has focus.

Inspired by how Wispr Flow is built (global keyboard hook → audio capture → streaming
transcription → optional LLM clean-up → clipboard paste with restore), but self-contained:
no accounts, local transcription by default, optional cloud backends you control.

## Features

- **Push-to-talk, hands-free and cancel gestures**, like the commercial tools:
  - hold the chord → dictate, release → paste;
  - tap the chord twice quickly → hands-free lock, press once more → stop;
  - a quick single tap, a quick triple tap or **Esc** cancels.
- **Transcription backends** behind a `Transcriber` trait:
  - `whisper` (default): whisper.cpp in-process via `whisper-rs`; the ggml model is downloaded
    from Hugging Face on first use (`tiny`, `base`, `small`, `medium`, `large-v3-turbo`…);
  - `openai`: `POST /v1/audio/transcriptions` (`gpt-4o-mini-transcribe`, `whisper-1`, or any
    compatible server through `base_url`).
- **Optional LLM clean-up** behind a `Formatter` trait: `none` (default), `claude`
  (Messages API, `claude-haiku-5-5`, effort `low`) or `openai` (chat completions).
  Removes fillers and false starts, fixes punctuation, applies spoken commands
  ("nueva línea", "punto y aparte"), adapts register to the destination app.
- **Paste that behaves**: saves the clipboard, pastes with Ctrl+V after the chord modifiers are
  released (so Win+V never pops up), restores the previous clipboard only if nobody else
  changed it. Injects a masking key so the Win key alone does not open the Start menu.
- **Tray icon** with state colours (grey idle, red listening, orange hands-free, blue processing),
  system sounds, daily log files and a JSONL history of every dictation.
- **CLI mode** for testing backends without a microphone: `dictate --transcribe file.wav`.

## Build

Requirements on Windows: Rust (MSVC toolchain), Visual Studio C++ build tools and CMake
(whisper.cpp is compiled from source by `whisper-rs-sys`).

```powershell
cargo build --release
```

`.cargo/config.toml` sets `WHISPER_DONT_GENERATE_BINDINGS=1` (uses the pregenerated
bindings, so libclang is not needed) and points `CMAKE` at the winget-installed CMake.
Adjust that path if CMake lives elsewhere, or remove the line if `cmake` is on `PATH`.

To build without local whisper (cloud only): `cargo build --release --no-default-features`.

## Run

```powershell
# tray mode
.\target\release\dictate.exe

# useful switches
dictate --list-devices
dictate --config                      # prints the config path
dictate --transcribe sample.wav       # runs STT (+ formatter) on a file and prints the result
dictate --transcribe sample.wav --paste
dictate --paste-text "hello"          # exercises only the clipboard + Ctrl+V injection path
dictate --verbose                     # debug logging (or set DICTATE_LOG=debug)
```

`--paste` and `--paste-text` type into **whatever window is in the foreground** a few seconds
after you run them, so focus the target text box first (or run them from a script that does).

On first run a default `config.toml` is created at `%APPDATA%\Dictate\config\config.toml`.
Models, logs and history live under `%APPDATA%\Dictate\data\` (`dictate --help` prints the
exact paths).

## Configuration (excerpt)

```toml
[hotkey]
chord = ["ctrl", "win"]       # ctrl, win, alt, shift, space, caps, f1..f24, a..z, vk:0x..
double_tap_ms = 500
escape_cancels = true
max_duration_secs = 1200

[stt]
backend = "whisper"           # "whisper" | "openai"
language = "auto"             # "es", "en", ...
initial_prompt = ""           # vocabulary hint: names, jargon

[stt.whisper]
model = "base"                # tiny | base | small | medium | large-v3-turbo (+ ".en")
model_path = ""               # explicit ggml .bin path overrides `model`
threads = 0                   # 0 = auto

[stt.openai]
api_key_env = "OPENAI_API_KEY"
model = "gpt-4o-mini-transcribe"

[format]
backend = "none"              # "none" | "claude" | "openai"
extra_instructions = ""       # e.g. "Sign emails as Franco."

[format.claude]
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-haiku-5-5"
effort = "low"

[paste]
restore_clipboard = true
restore_delay_ms = 250
trailing_space = true
```

## Architecture

```
 keyboard hook (WH_KEYBOARD_LL, ui thread)  ──HookEvent──▶  gesture state machine (main thread)
                                                                 │ Start / Locked / Stop / Cancel
 cpal input stream ─▶ mono ─▶ 16 kHz f32 ◀───────────────────────┘
                                                                 │ Stop
                                        job thread: Transcriber ─▶ Formatter ─▶ inject::paste_text
                                                                 │
                                        history.jsonl ◀──────────┘      tray icon ◀─ PostThreadMessage
```

- `src/gesture.rs` – pure state machine with unit tests (hold / double-tap lock / triple-tap cancel / Esc / max duration).
- `src/hotkey.rs` – chord parsing and the low-level hook; ignores injected events; masks the Win key.
- `src/audio.rs` – cpal capture, channel downmix and a box-filtered linear downsampler to 16 kHz.
- `src/stt/` – `Transcriber` trait, `whisper.rs` (model download + inference), `openai.rs`.
- `src/format/` – `Formatter` trait, shared prompt, `claude.rs`, `openai.rs`.
- `src/inject.rs` – foreground app detection, clipboard save/paste/restore, Ctrl+V via `SendInput`.
- `src/ui.rs` – ui thread with the Win32 message loop, tray icon, sounds.
- `src/main.rs` – wiring, CLI, job threads.

## Roadmap / ideas

- Streaming partial results (chunked whisper or a WebSocket backend) for lower latency.
- UI Automation read-back of the focused text box to give the LLM `before/after` context and
  verify the paste, as Wispr does.
- Personal dictionary / replacements applied before pasting.
- Per-app register presets and a settings window.
