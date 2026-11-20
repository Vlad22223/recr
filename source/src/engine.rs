use crate::locale::tr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::capture::Capture;
use crate::media::{Media, recording_name, unique_id};
use crate::platform::persist;

pub const WM_ENGINE: u32 = WM_APP + 2;
pub const MAX_REPLAY_SECONDS: usize = 300;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub key: u32,
}

impl Hotkey {
    pub fn valid(self) -> bool {
        let function = (0x70..=0x87).contains(&self.key) && self.key != 0x7b;
        self.modifiers <= 7
            && (function
                || (self.modifiers != 0
                    && ((0x41..=0x5a).contains(&self.key) || (0x30..=0x39).contains(&self.key))))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    pub quality: usize,
    pub replay: bool,
    pub hide_border: bool,
    pub seconds: usize,
    pub record_key: Hotkey,
    pub replay_key: Hotkey,
    // None follows the default Windows microphone. An empty string intentionally mutes audio.
    pub mic: Option<String>,
    pub videos: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            quality: 1,
            replay: false,
            hide_border: true,
            seconds: 30,
            record_key: Hotkey {
                modifiers: 3,
                key: 0x78,
            },
            replay_key: Hotkey {
                modifiers: 3,
                key: 0x79,
            },
            mic: None,
            videos: None,
        }
    }
}

impl Settings {
    pub fn read(dir: &Path) -> Self {
        let mut settings = Self::default();
        let text = fs::read_to_string(dir.join("settings.txt")).unwrap_or_default();
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            if key == "mic" {
                settings.mic = Some(value.to_owned());
                continue;
            }
            if key == "videos" {
                let path = PathBuf::from(value);
                settings.videos = path.is_absolute().then_some(path);
                continue;
            }
            let Ok(value) = value.trim().parse::<usize>() else {
                continue;
            };
            match key {
                "quality" if value < 3 => settings.quality = value,
                "replay" => settings.replay = value == 1,
                "hide_border" => settings.hide_border = value == 1,
                "seconds" if (1..=MAX_REPLAY_SECONDS).contains(&value) => settings.seconds = value,
                "record_modifiers" if value <= 7 => settings.record_key.modifiers = value as u32,
                "record_key" if value <= 255 => settings.record_key.key = value as u32,
                "replay_modifiers" if value <= 7 => settings.replay_key.modifiers = value as u32,
                "replay_key" if value <= 255 => settings.replay_key.key = value as u32,
                _ => {}
            }
        }
        if !settings.record_key.valid() {
            settings.record_key = Self::default().record_key;
        }
        if !settings.replay_key.valid() || settings.replay_key == settings.record_key {
            settings.replay_key = Self::default().replay_key;
            if settings.replay_key == settings.record_key {
                settings.replay_key = Self::default().record_key;
            }
        }
        settings
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let mut text = format!(
            "quality={}\nreplay={}\nhide_border={}\nseconds={}\nrecord_modifiers={}\nrecord_key={}\nreplay_modifiers={}\nreplay_key={}\n",
            self.quality,
            u8::from(self.replay),
            u8::from(self.hide_border),
            self.seconds,
            self.record_key.modifiers,
            self.record_key.key,
            self.replay_key.modifiers,
            self.replay_key.key
        );
        if let Some(device) = &self.mic {
            text.push_str(&format!("mic={device}\n"));
        }
        if let Some(videos) = &self.videos {
            text.push_str(&format!("videos={}\n", videos.display()));
        }
        persist(&dir.join("settings.txt"), &text)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Starting,
    Buffering,
    Recording,
    Finishing,
}

pub enum Request {
    Configure(Settings),
    Toggle,
    Replay,
    Quit,
}

pub enum Event {
    Mode(Mode),
    Notice(String, bool),
    Saved(PathBuf),
}

#[derive(Clone)]
pub struct Notify {
    pub sender: Sender<Event>,
    pub hwnd: isize,
}

impl Notify {
    pub fn send(&self, event: Event) {
        if self.sender.send(event).is_ok() {
            unsafe { PostMessageW(self.hwnd as _, WM_ENGINE, 0, 0) };
        }
    }
}

pub fn start(
    media: Media,
    root: PathBuf,
    settings: Settings,
    notify: Notify,
) -> (Sender<Request>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        Worker {
            media,
            root,
            settings,
            notify,
            capture: None,
            record: None,
            closing: None,
            exports: Vec::new(),
        }
        .run(receiver);
    });
    (sender, handle)
}

struct Worker {
    media: Media,
    root: PathBuf,
    settings: Settings,
    notify: Notify,
    capture: Option<Session>,
    record: Option<usize>,
    closing: Option<(usize, usize, Instant)>,
    exports: Vec<JoinHandle<()>>,
}

impl Worker {
    fn run(mut self, receiver: Receiver<Request>) {
        self.clear_abandoned_buffers();
        if self.settings.replay {
            self.begin(false);
        }
        loop {
            let request = if self.capture.is_some() {
                match receiver.recv_timeout(Duration::from_millis(180)) {
                    Ok(request) => Some(request),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match receiver.recv() {
                    Ok(request) => Some(request),
                    Err(_) => break,
                }
            };
            self.exports.retain(|task| !task.is_finished());
            self.update();
            match request {
                Some(Request::Quit) => break,
                Some(Request::Toggle) if self.record.is_some() => self.stop_recording(),
                Some(Request::Toggle) if self.closing.is_none() => self.begin(true),
                Some(Request::Replay) => self.replay(),
                Some(Request::Configure(settings)) => self.configure(settings),
                _ => {}
            }
        }
        if let Some(mut capture) = self.capture.take() {
            let _ = capture.finish();
            if let Some((first, limit, _)) = self.closing.take() {
                self.save(
                    &capture,
                    first,
                    capture.next.min(limit),
                    tr("Запись", "Recording"),
                );
            } else if let Some(first) = self.record.take() {
                self.save(&capture, first, capture.next, tr("Запись", "Recording"));
            }
        }
        for task in self.exports {
            let _ = task.join();
        }
    }

    fn state(&self, mode: Mode) {
        self.notify.send(Event::Mode(mode));
    }

    fn notice(&self, text: &str, error: bool) {
        self.notify.send(Event::Notice(text.into(), error));
    }

    fn log_error(&self, error: &io::Error) {
        let _ = fs::create_dir_all(&self.root);
        let _ = fs::write(self.root.join("last-error.log"), error.to_string());
    }

    fn clear_abandoned_buffers(&self) {
        // This directory belongs to the new edition and is guarded by its instance mutex.
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir())
                && entry.file_name().to_string_lossy().starts_with("capture-")
            {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    fn begin(&mut self, recording: bool) {
        if self.capture.is_none() {
            self.state(Mode::Starting);
            let seconds = self
                .settings
                .replay
                .then(|| segment_seconds(self.settings.seconds));
            match Session::start(&self.root, &self.settings, seconds) {
                Ok(session) => self.capture = Some(session),
                Err(error) => {
                    self.log_error(&error);
                    self.state(Mode::Idle);
                    self.notice(tr("Не удалось начать захват. Проверьте экран и микрофон в настройках Windows.", "Could not start capture. Check your display and microphone in Windows Settings."), true);
                    return;
                }
            }
        }
        if recording {
            self.record = self.capture.as_ref().map(|capture| capture.next);
        }
        self.state(if recording {
            Mode::Recording
        } else {
            Mode::Buffering
        });
        if self.settings.mic.as_deref() != Some("")
            && self
                .capture
                .as_ref()
                .is_some_and(|session| !session.capture.audio)
        {
            self.notice(
                tr(
                    "Микрофон недоступен. Захват идёт без звука.",
                    "Microphone unavailable. Capturing without audio.",
                ),
                true,
            );
        }
    }

    fn configure(&mut self, settings: Settings) {
        let restart = self.settings.replay != settings.replay
            || self.settings.quality != settings.quality
            || self.settings.mic != settings.mic
            || self.settings.hide_border != settings.hide_border
            || (settings.replay
                && segment_seconds(self.settings.seconds) != segment_seconds(settings.seconds));
        self.settings = settings;
        if restart && self.record.is_none() && self.closing.is_none() {
            self.capture = None;
            if self.settings.replay {
                self.begin(false);
            } else {
                self.state(Mode::Idle);
            }
        }
    }

    fn stop_recording(&mut self) {
        let Some(first) = self.record.take() else {
            return;
        };
        let Some(mut capture) = self.capture.take() else {
            return;
        };
        self.state(Mode::Finishing);
        if capture.seconds.is_some() {
            self.closing = Some((first, capture.next + 1, Instant::now()));
            self.capture = Some(capture);
        } else {
            if let Err(error) = capture.finish() {
                self.log_error(&error);
                self.notice(
                    tr(
                        "Запись завершилась с ошибкой.",
                        "Recording ended with an error.",
                    ),
                    true,
                );
            }
            self.save(&capture, first, capture.next, tr("Запись", "Recording"));
            self.state(Mode::Idle);
        }
    }

    fn replay(&mut self) {
        if !self.settings.replay {
            return;
        }
        if self.exports.len() >= 2 {
            self.notice(
                tr(
                    "Подождите, пока сохранится видео.",
                    "Wait for the video to finish saving.",
                ),
                false,
            );
            return;
        }
        if let Some(capture) = self.capture.take() {
            let first = capture.buffer_first(self.settings.seconds);
            self.save(&capture, first, capture.next, tr("Откат", "Replay"));
            self.capture = Some(capture);
        } else {
            self.begin(false);
        }
    }

    fn update(&mut self) {
        let Some(mut capture) = self.capture.take() else {
            return;
        };
        capture.refresh();
        if !capture.audio_warned && capture.capture.audio_lost() {
            capture.audio_warned = true;
            self.notice(
                tr(
                    "Микрофон отключился. Видео продолжает записываться.",
                    "The microphone disconnected. Video recording continues.",
                ),
                true,
            );
        }
        if capture.capture.is_finished() {
            if let Err(error) = capture.finish() {
                self.log_error(&error);
            }
            if let Some((first, limit, _)) = self.closing.take() {
                self.save(
                    &capture,
                    first,
                    capture.next.min(limit),
                    tr("Запись", "Recording"),
                );
            } else if let Some(first) = self.record.take() {
                self.save(&capture, first, capture.next, tr("Запись", "Recording"));
            }
            self.state(Mode::Idle);
            self.notice(
                tr(
                    "Захват экрана прервался.",
                    "Screen capture was interrupted.",
                ),
                true,
            );
            return;
        }
        if let Some((first, limit, requested)) = self.closing {
            if capture.next >= limit || requested.elapsed() >= Duration::from_secs(6) {
                self.closing = None;
                self.save(
                    &capture,
                    first,
                    capture.next.min(limit),
                    tr("Запись", "Recording"),
                );
                self.state(Mode::Buffering);
            }
        }
        if capture.seconds.is_some() {
            let keep = capture.buffer_first(self.settings.seconds);
            let pinned = self.record.or(self.closing.map(|(first, _, _)| first));
            capture.prune(pinned.map_or(keep, |first| keep.min(first)));
        }
        self.capture = Some(capture);
    }

    fn save(&mut self, session: &Session, first: usize, end: usize, prefix: &str) {
        if end <= first {
            self.notice(
                tr(
                    "Пока нет готового фрагмента.",
                    "No replay segment is ready yet.",
                ),
                false,
            );
            return;
        }
        let snapshot = self.root.join(format!("export-{}", unique_id()));
        let prepared = (|| -> io::Result<()> {
            fs::create_dir(&snapshot)?;
            for part in first..end {
                let name = segment_name(part);
                let source = session.dir.join(&name);
                let linked = snapshot.join(name);
                if fs::hard_link(&source, &linked).is_err() {
                    fs::copy(source, linked)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            let _ = fs::remove_dir_all(&snapshot);
            self.log_error(&error);
            self.notice(
                tr(
                    "Не удалось подготовить видео.",
                    "Could not prepare the video.",
                ),
                true,
            );
            return;
        }
        let name = recording_name(prefix);
        let mut media = self.media.clone();
        if let Some(videos) = &self.settings.videos {
            media.videos.clone_from(videos);
        }
        let notify = self.notify.clone();
        let log = self.root.join("last-error.log");
        self.exports.push(thread::spawn(move || {
            match media.export(&snapshot, &name) {
                Ok(path) => {
                    let _ = fs::remove_dir_all(&snapshot);
                    notify.send(Event::Saved(path));
                }
                Err(error) => {
                    let _ = fs::write(log, format!("{error}\n{}", snapshot.display()));
                    notify.send(Event::Notice(
                        tr("Не удалось сохранить видео.", "Could not save the video.").into(),
                        true,
                    ));
                }
            }
        }));
    }
}

fn segment_seconds(buffer_seconds: usize) -> u32 {
    if buffer_seconds % 2 == 0 { 2 } else { 1 }
}

struct Session {
    dir: PathBuf,
    capture: Capture,
    seconds: Option<u32>,
    next: usize,
    oldest: usize,
    stopped: bool,
    audio_warned: bool,
}

impl Session {
    fn start(root: &Path, settings: &Settings, seconds: Option<u32>) -> io::Result<Self> {
        let dir = root.join(format!("capture-{}", unique_id()));
        fs::create_dir_all(&dir)?;
        match Capture::start(
            settings.quality,
            settings.mic.clone(),
            dir.clone(),
            seconds,
            settings.hide_border,
        ) {
            Ok(capture) => Ok(Self {
                dir,
                capture,
                seconds,
                next: 0,
                oldest: 0,
                stopped: false,
                audio_warned: false,
            }),
            Err(error) => {
                let _ = fs::remove_dir_all(dir);
                Err(error)
            }
        }
    }

    fn refresh(&mut self) {
        while self.dir.join(segment_name(self.next)).is_file() {
            self.next += 1;
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        let result = self.capture.stop();
        self.refresh();
        result
    }

    fn buffer_first(&self, duration: usize) -> usize {
        let pieces = duration.div_ceil(self.seconds.unwrap_or(1) as usize);
        self.next.saturating_sub(pieces).max(self.oldest)
    }

    fn prune(&mut self, keep: usize) {
        while self.oldest < keep {
            let result = fs::remove_file(self.dir.join(segment_name(self.oldest)));
            if result.is_err_and(|error| error.kind() != io::ErrorKind::NotFound) {
                break;
            }
            self.oldest += 1;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.finish();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn segment_name(index: usize) -> String {
    format!("seg-{index:09}.mp4")
}
