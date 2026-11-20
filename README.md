# recr

A lightweight screen recorder for Windows, written in Rust.

## Download

Download `recr.exe` from the [latest release](https://github.com/Vlad22223/recr/releases/latest) and run it on Windows 10 or 11 (64-bit). No installer or FFmpeg required.

## Getting started

When recr starts, its icon appears in the **system tray** near the clock. If you don’t see it, click the **^** arrow to show hidden icons.

Right-click the recr icon to start a recording, browse saved videos, or open Settings. By default, **Ctrl + Alt + F9** starts or stops recording, and **Ctrl + Alt + F10** saves a replay when the replay buffer is enabled.

## Features

- Screen recording with optional microphone audio
- Instant replay with an adjustable buffer duration
- Adjustable recording quality, customizable hotkeys, and a configurable save folder
- Optional launch at Windows sign-in
- English and Russian interface with one-click switching in Settings

## Build from source

On Windows, with the Rust toolchain installed, run:

```powershell
cargo build --release --locked
rustc --edition 2024 tools/resources.rs -o target/resources.exe
.\target\resources.exe .\target\release\recr.exe
```

The executable is `target\release\recr.exe`.
