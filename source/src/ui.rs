use crate::locale::{self, tr, trf};
use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::{
    Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
    LibraryLoader::GetModuleHandleW,
};
use windows_sys::Win32::UI::{
    Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*,
};

use crate::audio::{Microphone, microphones};
use crate::capture;
use crate::engine::{self, Event, Hotkey, MAX_REPLAY_SECONDS, Mode, Notify, Request, Settings};
use crate::library;
use crate::media::Media;
use crate::platform::{self, wide};

const TRAY_CLASS: &str = "recr.Native.Tray";
const SETTINGS_CLASS: &str = "recr.Native.Settings";
const WM_TRAY: u32 = WM_APP + 1;
const WM_MICROPHONES: u32 = WM_APP + 3;
const WM_BIND_FOCUS: u32 = WM_APP + 4;
const RECORD_KEY: i32 = 1;
const REPLAY_KEY: i32 = 2;
const SAVE_REPLAY: usize = 5;
const SS_CENTERIMAGE: u32 = 0x0200;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

type MicrophoneResult = (isize, std::io::Result<Vec<Microphone>>);

struct App {
    hwnd: HWND,
    taskbar_created: u32,
    icons: [HICON; 4],
    settings: Settings,
    mode: Mode,
    data: PathBuf,
    worker: Option<(Sender<Request>, JoinHandle<()>)>,
    events: Receiver<Event>,
    panel: Option<SettingsPanel>,
    recordings: HWND,
    saved_folder: Option<PathBuf>,
    micro_tx: Sender<MicrophoneResult>,
    micro_rx: Receiver<MicrophoneResult>,
    hotkeys_suspended: bool,
    quitting: bool,
}

pub fn run() {
    let Ok(Some(_instance)) = platform::single_instance() else {
        return;
    };
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        let com = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) >= 0;
        let controls = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_HOTKEY_CLASS | ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&controls);
        run_ui();
        if com {
            CoUninitialize();
        }
    }
}

unsafe fn run_ui() {
    let icons = tray_icons();
    if !register_class(TRAY_CLASS, tray_proc, icons[0], false)
        || !register_class(SETTINGS_CLASS, settings_proc, icons[0], true)
    {
        for icon in icons {
            unsafe { DestroyIcon(icon) };
        }
        return;
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            wide(TRAY_CLASS).as_ptr(),
            wide("recr").as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    };
    if hwnd.is_null() {
        for icon in icons {
            unsafe { DestroyIcon(icon) };
        }
        return;
    }

    let data = platform::data_dir();
    locale::load(&data);
    let settings = Settings::read(&data);
    let media = Media {
        videos: platform::videos_dir(),
    };
    let (tx, events) = mpsc::channel();
    let notify = Notify {
        sender: tx,
        hwnd: hwnd as isize,
    };
    let worker = Some(engine::start(
        media,
        data.join("buffer"),
        settings.clone(),
        notify,
    ));
    let (micro_tx, micro_rx) = mpsc::channel();
    let app = App {
        hwnd,
        icons,
        settings,
        data,
        worker,
        events,
        micro_tx,
        micro_rx,
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        mode: Mode::Idle,
        panel: None,
        recordings: null_mut(),
        saved_folder: None,
        hotkeys_suspended: false,
        quitting: false,
    };
    app.tray(NIM_ADD, None);
    if !app.install_hotkeys(&app.settings) {
        app.tray(
            NIM_MODIFY,
            Some((
                tr(
                    "Одна из горячих клавиш уже занята.",
                    "One of the hotkeys is already in use.",
                ),
                true,
            )),
        );
    }
    APP.with(|slot| *slot.borrow_mut() = Some(app));

    let mut msg: MSG = unsafe { zeroed() };
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        let panel = APP.with(|slot| {
            slot.borrow()
                .as_ref()
                .and_then(|a| a.panel.as_ref().map(|p| p.hwnd))
        });
        if panel.is_some_and(|hwnd| unsafe { IsDialogMessageW(hwnd, &msg) != 0 }) {
            continue;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    APP.with(|slot| {
        if let Some(mut app) = slot.borrow_mut().take() {
            if let Some((sender, task)) = app.worker.take() {
                let _ = sender.send(Request::Quit);
                let _ = task.join();
            }
            library::wait_for_edits();
            for icon in app.icons {
                unsafe { DestroyIcon(icon) };
            }
        }
    });
}

fn register_class(
    name: &str,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    icon: HICON,
    dialog: bool,
) -> bool {
    let name = wide(name);
    unsafe {
        let mut class: WNDCLASSW = zeroed();
        class.lpfnWndProc = Some(proc);
        class.hInstance = GetModuleHandleW(null());
        class.hIcon = icon;
        class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        class.hbrBackground = if dialog {
            (COLOR_BTNFACE + 1) as HBRUSH
        } else {
            null_mut()
        };
        class.lpszClassName = name.as_ptr();
        RegisterClassW(&class) != 0
    }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return 0;
    }
    APP.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return unsafe { DefWindowProcW(hwnd, msg, w, l) };
        };
        let Some(app) = slot.as_mut() else {
            return unsafe { DefWindowProcW(hwnd, msg, w, l) };
        };
        match msg {
            WM_TRAY if l as u32 == WM_RBUTTONUP => app.menu(),
            WM_TRAY if l as u32 == NIN_BALLOONUSERCLICK => app.open_saved_folder(),
            WM_COMMAND if w >> 16 == 0 => app.tray_command(w),
            WM_HOTKEY if !app.hotkeys_suspended => match w as i32 {
                RECORD_KEY => app.record(),
                REPLAY_KEY if app.settings.replay => app.send(Request::Replay),
                _ => {}
            },
            engine::WM_ENGINE => app.events(),
            WM_MICROPHONES => app.microphone_results(),
            WM_BIND_FOCUS => app.sync_hotkey_focus(),
            WM_CLOSE => app.quit(),
            WM_ENDSESSION if w != 0 => app.quit(),
            m if m == app.taskbar_created => app.tray(NIM_ADD, None),
            _ => return unsafe { DefWindowProcW(hwnd, msg, w, l) },
        }
        0
    })
}

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == DM_GETDEFID {
        return ((DC_HASDEFID as isize) << 16) | IDOK as isize;
    }
    if matches!(msg, WM_CLOSE | WM_COMMAND) {
        let handled = APP.with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                return false;
            };
            let Some(app) = slot.as_mut() else {
                return false;
            };
            if !app.panel.as_ref().is_some_and(|panel| panel.hwnd == hwnd) {
                return false;
            }
            if msg == WM_CLOSE {
                app.close_settings();
            } else {
                return app.settings_command(w & 0xffff, (w >> 16) as u32);
            }
            true
        });
        if handled {
            return 0;
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

impl App {
    fn folder(&self) -> PathBuf {
        self.settings
            .videos
            .clone()
            .unwrap_or_else(platform::videos_dir)
    }

    fn active(&self) -> bool {
        matches!(
            self.mode,
            Mode::Starting | Mode::Recording | Mode::Finishing
        )
    }

    fn tray(&self, command: NOTIFY_ICON_MESSAGE, balloon: Option<(&str, bool)>) {
        let recording = matches!(self.mode, Mode::Recording | Mode::Finishing);
        let replay = self.settings.replay && (recording || self.mode == Mode::Buffering);
        let index = usize::from(recording) + 2 * usize::from(replay);
        let mut data: NOTIFYICONDATAW = unsafe { zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.icons[index];
        fill(
            &mut data.szTip,
            match index {
                1 => tr("recr · Идёт запись", "recr · Recording"),
                2 => tr("recr · Буфер отката", "recr · Replay buffer"),
                3 => tr(
                    "recr · Запись и буфер отката",
                    "recr · Recording and replay buffer",
                ),
                _ => "recr",
            },
        );
        if let Some((message, error)) = balloon {
            data.uFlags |= NIF_INFO;
            data.dwInfoFlags = (if error { NIIF_WARNING } else { NIIF_INFO }) | NIIF_NOSOUND;
            fill(&mut data.szInfoTitle, "recr");
            fill(&mut data.szInfo, message);
        }
        unsafe {
            if Shell_NotifyIconW(command, &data) != 0 && command == NIM_ADD {
                data.Anonymous.uVersion = NOTIFYICON_VERSION;
                Shell_NotifyIconW(NIM_SETVERSION, &data);
            }
        }
    }

    fn open_saved_folder(&self) {
        let Some(folder) = self.saved_folder.as_ref() else {
            return;
        };
        let result = unsafe {
            ShellExecuteW(
                self.hwnd,
                wide("open").as_ptr(),
                wide(folder).as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            )
        };
        if result as usize <= 32 {
            platform::message(
                self.hwnd,
                tr(
                    "Не удалось открыть папку с записями.",
                    "Could not open the recordings folder.",
                ),
                true,
            );
        }
    }

    fn menu(&mut self) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            let (text, state) = match self.mode {
                Mode::Recording => (tr("Остановить запись", "Stop recording"), MF_STRING),
                Mode::Starting => (tr("Начало записи…", "Starting recording…"), MF_GRAYED),
                Mode::Finishing => (tr("Сохранение записи…", "Saving recording…"), MF_GRAYED),
                _ => (tr("Начать запись", "Start recording"), MF_STRING),
            };
            AppendMenuW(menu, state, 1, wide(text).as_ptr());
            if self.settings.replay {
                AppendMenuW(
                    menu,
                    MF_STRING,
                    SAVE_REPLAY,
                    wide(tr("Откатить", "Save replay")).as_ptr(),
                );
            }
            AppendMenuW(
                menu,
                MF_STRING,
                2,
                wide(tr("Записи", "Recordings")).as_ptr(),
            );
            AppendMenuW(
                menu,
                MF_STRING,
                3,
                wide(tr("Настройки", "Settings")).as_ptr(),
            );
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, 4, wide(tr("Закрыть", "Exit")).as_ptr());
            let mut cursor: POINT = zeroed();
            GetCursorPos(&mut cursor);
            SetForegroundWindow(self.hwnd);
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                cursor.x,
                cursor.y,
                0,
                self.hwnd,
                null(),
            );
            DestroyMenu(menu);
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            self.events();
            self.microphone_results();
            self.tray_command(command as usize);
        }
    }

    fn tray_command(&mut self, command: usize) {
        match command {
            1 => self.record(),
            2 => self.open_library(),
            3 => self.open_settings(),
            4 => self.quit(),
            SAVE_REPLAY if self.settings.replay => self.send(Request::Replay),
            _ => {}
        }
    }

    fn send(&self, request: Request) {
        if let Some((sender, _)) = &self.worker {
            let _ = sender.send(request);
        }
    }

    fn record(&mut self) {
        if !matches!(self.mode, Mode::Starting | Mode::Finishing) {
            self.mode = if self.mode == Mode::Recording {
                Mode::Finishing
            } else {
                Mode::Starting
            };
            self.update_settings_controls();
            self.tray(NIM_MODIFY, None);
            self.send(Request::Toggle);
        }
    }

    fn events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Mode(mode) => {
                    self.mode = mode;
                    self.tray(NIM_MODIFY, None);
                    self.update_settings_controls();
                }
                Event::Notice(text, error) => {
                    self.saved_folder = None;
                    self.tray(NIM_MODIFY, Some((&text, error)));
                }
                Event::Saved(path) => {
                    self.saved_folder = path.parent().map(Path::to_owned);
                    self.tray(
                        NIM_MODIFY,
                        Some((
                            tr(
                                "Видео сохранено. Нажмите, чтобы открыть папку.",
                                "Video saved. Click to open its folder.",
                            ),
                            false,
                        )),
                    );
                    if path.parent().is_some_and(|folder| folder == self.folder()) {
                        library::refresh(self.recordings);
                    }
                }
            }
        }
    }

    fn open_library(&mut self) {
        unsafe {
            if IsWindow(self.recordings) != 0 {
                library::refresh(self.recordings);
                ShowWindow(self.recordings, SW_RESTORE);
                SetForegroundWindow(self.recordings);
                return;
            }
        }
        let folder = self.folder();

        self.recordings = library::open(self.hwnd, folder, self.data.clone());
        unsafe {
            SendMessageW(
                self.recordings,
                WM_SETICON,
                ICON_SMALL as usize,
                self.icons[0] as isize,
            );
        }
    }

    fn open_settings(&mut self) {
        if let Some(panel) = &self.panel {
            unsafe {
                ShowWindow(panel.hwnd, SW_RESTORE);
                SetForegroundWindow(panel.hwnd);
            }
            return;
        }
        let Some(panel) =
            SettingsPanel::new(self.hwnd, self.icons[0], &self.settings, &self.folder())
        else {
            return;
        };
        let panel_hwnd = panel.hwnd;
        self.panel = Some(panel);
        self.update_settings_controls();
        unsafe {
            ShowWindow(panel_hwnd, SW_SHOWNORMAL);
            SetForegroundWindow(panel_hwnd);
        }

        let sender = self.micro_tx.clone();
        let owner = self.hwnd as isize;
        let panel_id = panel_hwnd as isize;
        thread::spawn(move || {
            if sender.send((panel_id, microphones())).is_ok() {
                unsafe { PostMessageW(owner as HWND, WM_MICROPHONES, 0, 0) };
            }
        });
    }

    fn update_settings_controls(&self) {
        if let Some(panel) = &self.panel {
            panel.update_controls(!self.active());
        }
    }

    fn close_settings(&mut self) {
        if let Some(panel) = self.panel.take() {
            unsafe { DestroyWindow(panel.hwnd) };
        }
        self.sync_hotkey_focus();
    }

    fn settings_command(&mut self, id: usize, notification: u32) -> bool {
        match id {
            n if n == IDOK as usize => self.save_settings(),
            n if n == IDCANCEL as usize => self.close_settings(),
            ID_REPLAY => self.update_settings_controls(),
            ID_LANGUAGE if notification == BN_CLICKED => self.switch_language(),
            ID_DURATION if notification == EN_CHANGE as u32 => {
                if let Some(panel) = &self.panel {
                    panel.update_duration_hint();
                }
            }
            ID_BROWSE => {
                if let Some(panel) = &self.panel {
                    if let Some(path) = browse(panel.hwnd) {
                        unsafe { SetWindowTextW(panel.path, wide(path).as_ptr()) };
                    }
                }
                self.events();
                self.microphone_results();
            }
            _ => return false,
        }
        true
    }

    fn switch_language(&mut self) {
        let Some(panel) = &mut self.panel else { return };
        if let Err(error) = locale::toggle(&self.data) {
            platform::message(
                panel.hwnd,
                &trf!(
                    "Не удалось сохранить язык интерфейса.\n{}",
                    "Could not save the interface language.\n{}",
                    error
                ),
                true,
            );
            return;
        }
        let quality = selection(panel.quality).unwrap_or(self.settings.quality);
        let mic = panel.selected_microphone(&self.settings.mic);
        panel.localize(quality, mic);
        library::localize(self.recordings);
        self.tray(NIM_MODIFY, None);
    }

    fn save_settings(&mut self) {
        let Some(panel) = &self.panel else { return };
        let hwnd = panel.hwnd;
        let mut next = self.settings.clone();
        next.record_key = get_hotkey(panel.record);
        next.replay_key = get_hotkey(panel.replay_key);
        if !next.record_key.valid() || !next.replay_key.valid() {
            platform::message(
                hwnd,
                tr(
                    "Укажите F-клавишу или сочетание Ctrl, Alt либо Shift с буквой или цифрой.",
                    "Use a function key or combine Ctrl, Alt, or Shift with a letter or number.",
                ),
                true,
            );
            return;
        }
        if next.record_key == next.replay_key {
            platform::message(
                hwnd,
                tr(
                    "Выберите разные сочетания для записи и отката.",
                    "Use different hotkeys for recording and replay.",
                ),
                true,
            );
            return;
        }

        if !self.active() {
            next.quality = selection(panel.quality).unwrap_or(next.quality);
            next.replay = checked(panel.replay);
            next.hide_border = checked(panel.hide_border);
            if let Some(seconds) = panel
                .duration_seconds()
                .filter(|&n| (1..=MAX_REPLAY_SECONDS).contains(&n))
            {
                next.seconds = seconds;
            } else if next.replay {
                platform::message(
                    hwnd,
                    &trf!(
                        "Введите от 1 до {} секунд.",
                        "Enter 1 to {} seconds.",
                        MAX_REPLAY_SECONDS
                    ),
                    true,
                );
                unsafe { SetFocus(panel.duration) };
                return;
            }
            if let Some(index) = selection(panel.mic) {
                next.mic = panel.microphones[index].clone();
            }
            let path = PathBuf::from(read_text(panel.path));
            if !path.is_absolute() {
                platform::message(
                    hwnd,
                    tr(
                        "Укажите полный путь к папке для записей.",
                        "Enter the full path to the recordings folder.",
                    ),
                    true,
                );
                return;
            }
            if let Err(error) = fs::create_dir_all(&path) {
                platform::message(
                    hwnd,
                    &trf!(
                        "Не удалось открыть папку.\n{}",
                        "Could not open the folder.\n{}",
                        error
                    ),
                    true,
                );
                return;
            }
            next.videos = (path != platform::videos_dir()).then_some(path);
        }

        let startup = checked(panel.startup);
        let old_startup = platform::startup_enabled();
        let changed_keys = next.record_key != self.settings.record_key
            || next.replay_key != self.settings.replay_key
            || next.replay != self.settings.replay;
        if changed_keys {
            self.remove_hotkeys();
            if !self.install_hotkeys(&next) {
                self.restore_hotkeys();
                platform::message(
                    hwnd,
                    tr(
                        "Одна из горячих клавиш уже занята Windows или другим приложением.",
                        "One of the hotkeys is already in use by Windows or another app.",
                    ),
                    true,
                );
                return;
            }
        }
        if startup != old_startup {
            if let Err(error) = platform::set_startup(startup) {
                self.restore_hotkeys();
                platform::message(
                    hwnd,
                    &trf!(
                        "Не удалось изменить автозапуск.\n{}",
                        "Could not change startup settings.\n{}",
                        error
                    ),
                    true,
                );
                return;
            }
        }
        if let Err(error) = next.save(&self.data) {
            if startup != old_startup {
                let _ = platform::set_startup(old_startup);
            }
            self.restore_hotkeys();
            platform::message(
                hwnd,
                &trf!(
                    "Не удалось сохранить настройки.\n{}",
                    "Could not save settings.\n{}",
                    error
                ),
                true,
            );
            return;
        }
        let folder_changed = next.videos != self.settings.videos;
        self.settings = next;
        self.send(Request::Configure(self.settings.clone()));
        if folder_changed {
            let folder = self.folder();

            library::set_folder(self.recordings, folder);
        }
        self.close_settings();
        self.tray(NIM_MODIFY, None);
    }

    fn microphone_results(&mut self) {
        while let Ok((id, result)) = self.micro_rx.try_recv() {
            if let Some(panel) = self.panel.as_mut().filter(|p| p.hwnd as isize == id) {
                let selected = panel.selected_microphone(&self.settings.mic);
                panel.devices = Some(result.unwrap_or_default());
                panel.fill_microphones(selected);
            }
        }
        self.update_settings_controls();
    }

    fn install_hotkeys(&self, settings: &Settings) -> bool {
        let register = |id, key: Hotkey| unsafe {
            RegisterHotKey(self.hwnd, id, key.modifiers | MOD_NOREPEAT, key.key) != 0
        };
        let record = register(RECORD_KEY, settings.record_key);
        let replay = !settings.replay || register(REPLAY_KEY, settings.replay_key);
        record && replay
    }

    fn remove_hotkeys(&self) {
        unsafe {
            UnregisterHotKey(self.hwnd, RECORD_KEY);
            UnregisterHotKey(self.hwnd, REPLAY_KEY);
        }
    }

    fn restore_hotkeys(&self) {
        self.remove_hotkeys();
        if !self.hotkeys_suspended {
            self.install_hotkeys(&self.settings);
        }
    }

    fn sync_hotkey_focus(&mut self) {
        let focus = unsafe { GetFocus() };
        let editing = self
            .panel
            .as_ref()
            .is_some_and(|panel| focus == panel.record || focus == panel.replay_key);
        if editing != self.hotkeys_suspended {
            self.hotkeys_suspended = editing;
            self.restore_hotkeys();
        }
    }

    fn quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.tray(NIM_DELETE, None);
        self.close_settings();
        unsafe {
            if IsWindow(self.recordings) != 0 {
                DestroyWindow(self.recordings);
            }
            DestroyWindow(self.hwnd);
        }
    }
}

const ID_REPLAY: usize = 101;
const ID_BROWSE: usize = 102;
const ID_DURATION: usize = 103;
const ID_LANGUAGE: usize = 104;

struct SettingsPanel {
    hwnd: HWND,
    quality: HWND,
    record: HWND,
    replay_key: HWND,
    replay: HWND,
    duration: HWND,
    duration_hint: HWND,
    mic: HWND,
    mic_hint: HWND,
    path: HWND,
    browse: HWND,
    startup: HWND,
    hide_border: HWND,
    captions: [(HWND, &'static str, &'static str); 14],
    devices: Option<Vec<Microphone>>,
    microphones: Vec<Option<String>>,
    font: HFONT,
    language_icon: HICON,
}

impl SettingsPanel {
    fn new(owner: HWND, icon: HICON, value: &Settings, folder: &Path) -> Option<Self> {
        unsafe {
            let dpi = GetDpiForSystem().max(96);
            let scale = |n| (n * dpi as i32 + 48) / 96;
            let style = WS_CAPTION | WS_SYSMENU;
            let mut bounds = RECT {
                left: 0,
                top: 0,
                right: scale(394),
                bottom: scale(298),
            };
            AdjustWindowRectExForDpi(&mut bounds, style, 0, WS_EX_DLGMODALFRAME, dpi);
            let mut mouse: POINT = zeroed();
            GetCursorPos(&mut mouse);
            let mut tray: NOTIFYICONIDENTIFIER = zeroed();
            tray.cbSize = size_of::<NOTIFYICONIDENTIFIER>() as u32;
            tray.hWnd = owner;
            tray.uID = 1;
            let mut tray_rect: RECT = zeroed();
            let monitor = if Shell_NotifyIconGetRect(&tray, &mut tray_rect) >= 0 {
                MonitorFromRect(&tray_rect, MONITOR_DEFAULTTOPRIMARY)
            } else {
                MonitorFromPoint(mouse, MONITOR_DEFAULTTOPRIMARY)
            };
            let mut info: MONITORINFO = zeroed();
            info.cbSize = size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(monitor, &mut info);
            let w = bounds.right - bounds.left;
            let h = bounds.bottom - bounds.top;
            let x = (info.rcWork.right - w - scale(12)).max(info.rcWork.left);
            let y = (info.rcWork.bottom - h - scale(12)).max(info.rcWork.top);
            let instance = GetModuleHandleW(null());
            let hwnd = CreateWindowExW(
                WS_EX_DLGMODALFRAME,
                wide(SETTINGS_CLASS).as_ptr(),
                wide("").as_ptr(),
                style,
                x,
                y,
                w,
                h,
                owner,
                null_mut(),
                instance,
                null(),
            );
            if hwnd.is_null() {
                return None;
            }
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, icon as isize);
            let font = system_font(dpi);
            let create = |class: &str, label: &str, extra, control_style, id, (x, y, w, h)| {
                let child = CreateWindowExW(
                    extra,
                    wide(class).as_ptr(),
                    wide(label).as_ptr(),
                    WS_CHILD | WS_VISIBLE | control_style,
                    scale(x),
                    scale(y),
                    scale(w),
                    scale(h),
                    hwnd,
                    id as HMENU,
                    instance,
                    null(),
                );
                SendMessageW(child, WM_SETFONT, font as usize, 1);
                child
            };
            let label = |y| create("STATIC", "", 0, SS_CENTERIMAGE, 0, (12, y, 126, 23));
            let combo = |y| {
                create(
                    "COMBOBOX",
                    "",
                    0,
                    WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
                    0,
                    (142, y, 240, 205),
                )
            };
            let hotkey = |y| {
                let child = create("msctls_hotkey32", "", 0, WS_TABSTOP, 0, (142, y, 240, 23));
                SetWindowSubclass(child, Some(hotkey_proc), 1, owner as usize);
                child
            };
            let quality_label = label(10);
            let quality = combo(10);
            let record_label = label(39);
            let record = hotkey(39);
            let replay = create(
                "BUTTON",
                "",
                0,
                WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                ID_REPLAY,
                (12, 68, 300, 22),
            );
            let replay_label = label(96);
            let replay_key = hotkey(96);
            let duration_label = label(125);
            let duration = create(
                "EDIT",
                "",
                WS_EX_CLIENTEDGE,
                WS_TABSTOP | ES_NUMBER as u32,
                ID_DURATION,
                (142, 125, 60, 23),
            );
            SendMessageW(duration, EM_SETLIMITTEXT, 3, 0);
            SetWindowTextW(duration, wide(value.seconds.to_string()).as_ptr());
            let duration_hint = create("STATIC", "", 0, SS_CENTERIMAGE, 0, (210, 125, 172, 23));
            let mic_label = label(154);
            let mic = combo(154);
            SendMessageW(mic, CB_SETDROPPEDWIDTH, scale(310) as usize, 0);
            let mic_hint = create("STATIC", "", 0, SS_CENTERIMAGE, 0, (142, 177, 240, 18));
            let path_label = label(195);
            let path = create(
                "EDIT",
                "",
                WS_EX_CLIENTEDGE,
                WS_TABSTOP | ES_AUTOHSCROLL as u32,
                0,
                (142, 195, 164, 23),
            );
            let browse = create(
                "BUTTON",
                "",
                0,
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                ID_BROWSE,
                (312, 195, 70, 23),
            );
            let startup = create(
                "BUTTON",
                "",
                0,
                WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                0,
                (12, 226, 196, 23),
            );
            let hide_border = create(
                "BUTTON",
                "",
                0,
                WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                0,
                (209, 226, 173, 23),
            );
            let language = create(
                "BUTTON",
                "",
                0,
                WS_TABSTOP | BS_ICON as u32,
                ID_LANGUAGE,
                (12, 260, 28, 26),
            );
            let language_icon = load_icon(include_bytes!("../assets/globe.ico"), scale(16));
            SendMessageW(
                language,
                BM_SETIMAGE,
                IMAGE_ICON as usize,
                language_icon as isize,
            );
            let save = create(
                "BUTTON",
                "",
                0,
                WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
                IDOK as usize,
                (186, 260, 96, 26),
            );
            let cancel = create(
                "BUTTON",
                "",
                0,
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                IDCANCEL as usize,
                (286, 260, 96, 26),
            );

            SendMessageW(quality, CB_SETDROPPEDWIDTH, scale(330) as usize, 0);
            tick(replay, value.replay);
            tick(startup, platform::startup_enabled());
            tick(hide_border, value.hide_border);
            put_hotkey(record, value.record_key);
            put_hotkey(replay_key, value.replay_key);
            SetWindowTextW(path, wide(folder).as_ptr());

            let mut panel = Self {
                hwnd,
                quality,
                record,
                replay_key,
                replay,
                duration,
                duration_hint,
                mic,
                mic_hint,
                path,
                browse,
                startup,
                hide_border,
                captions: [
                    (hwnd, "Настройки — recr", "Settings — recr"),
                    (quality_label, "Качество", "Quality"),
                    (record_label, "Клавиша записи", "Record hotkey"),
                    (replay, "Включить буфер отката", "Enable replay buffer"),
                    (replay_label, "Клавиша отката", "Replay hotkey"),
                    (duration_label, "Длительность отката", "Replay length"),
                    (mic_label, "Микрофон", "Microphone"),
                    (path_label, "Папка для записей", "Recordings folder"),
                    (browse, "Обзор…", "Browse…"),
                    (
                        startup,
                        "Запускать при входе в Windows",
                        "Start with Windows",
                    ),
                    (hide_border, "Скрывать рамку захвата", "Hide capture border"),
                    (language, "Переключить на английский", "Switch to Russian"),
                    (save, "Сохранить", "Save"),
                    (cancel, "Отмена", "Cancel"),
                ],
                devices: None,
                microphones: Vec::new(),
                font,
                language_icon,
            };
            panel.localize(value.quality, value.mic.clone());
            Some(panel)
        }
    }

    fn localize(&mut self, quality: usize, mic: Option<String>) {
        for (control, russian, english) in self.captions {
            unsafe { SetWindowTextW(control, wide(tr(russian, english)).as_ptr()) };
        }
        let (width, height) = capture::planned_size();
        unsafe { SendMessageW(self.quality, CB_RESETCONTENT, 0, 0) };
        for (name, (fps, bitrate)) in [
            tr("Эконом", "Low"),
            tr("Стандарт", "Standard"),
            tr("Высокое", "High"),
        ]
        .into_iter()
        .zip(capture::QUALITY_PRESETS)
        {
            add_item(
                self.quality,
                &format!(
                    "{name} · {width}×{height} · {fps} {} · {} {}",
                    tr("к/с", "fps"),
                    bitrate / 1_000_000,
                    tr("Мбит/с", "Mbps"),
                ),
            );
        }
        choose(self.quality, quality);
        self.fill_microphones(mic);
        self.update_duration_hint();
    }

    fn duration_seconds(&self) -> Option<usize> {
        read_text(self.duration).to_str()?.parse().ok()
    }

    fn update_duration_hint(&self) {
        let hint = match self.duration_seconds() {
            Some(seconds) if seconds >= 60 => {
                trf!(
                    "сек. ({} мин {} сек)",
                    "sec ({} min {} sec)",
                    seconds / 60,
                    seconds % 60
                )
            }
            _ => tr("сек.", "sec").to_owned(),
        };
        unsafe { SetWindowTextW(self.duration_hint, wide(hint).as_ptr()) };
    }

    fn selected_microphone(&self, fallback: &Option<String>) -> Option<String> {
        selection(self.mic)
            .and_then(|index| self.microphones.get(index))
            .unwrap_or(fallback)
            .clone()
    }

    fn fill_microphones(&mut self, selected: Option<String>) {
        unsafe { SendMessageW(self.mic, CB_RESETCONTENT, 0, 0) };
        self.microphones.clear();
        let devices = self.devices.as_deref().unwrap_or_default();
        let mut add = |label: &str, id| {
            add_item(self.mic, label);
            self.microphones.push(id);
        };
        add(tr("Автоматически", "Automatic"), None);
        for device in devices {
            add(&device.name, Some(device.id.clone()));
        }
        add(tr("Без микрофона", "No microphone"), Some(String::new()));
        let index = self
            .microphones
            .iter()
            .position(|id| *id == selected)
            .unwrap_or_else(|| {
                add_item(
                    self.mic,
                    tr(
                        "Выбранный микрофон недоступен",
                        "Selected microphone unavailable",
                    ),
                );
                self.microphones.push(selected);
                self.microphones.len() - 1
            });
        choose(self.mic, index);
        let hint = if self.devices.is_none() {
            tr("Поиск микрофонов…", "Looking for microphones…")
        } else if devices.is_empty() {
            tr("Микрофоны не найдены.", "No microphones found.")
        } else {
            ""
        };
        unsafe { SetWindowTextW(self.mic_hint, wide(hint).as_ptr()) };
    }

    fn update_controls(&self, editable: bool) {
        unsafe {
            for hwnd in [
                self.quality,
                self.replay,
                self.path,
                self.browse,
                self.hide_border,
            ] {
                EnableWindow(hwnd, i32::from(editable));
            }
            EnableWindow(self.mic, i32::from(editable));
            EnableWindow(self.duration, i32::from(editable && checked(self.replay)));
            EnableWindow(self.replay_key, i32::from(checked(self.replay)));
        }
    }
}

impl Drop for SettingsPanel {
    fn drop(&mut self) {
        unsafe {
            if !self.font.is_null() {
                DeleteObject(self.font);
            }
            if !self.language_icon.is_null() {
                DestroyIcon(self.language_icon);
            }
        }
    }
}

unsafe extern "system" fn hotkey_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    owner: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_SETFOCUS | WM_KILLFOCUS => {
                PostMessageW(owner as HWND, WM_BIND_FOCUS, 0, 0);
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(hotkey_proc), id);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, w, l)
    }
}

fn system_font(dpi: u32) -> HFONT {
    unsafe {
        let mut metric: NONCLIENTMETRICSW = zeroed();
        metric.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        if SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            metric.cbSize,
            (&mut metric as *mut NONCLIENTMETRICSW).cast(),
            0,
            dpi,
        ) != 0
        {
            CreateFontIndirectW(&metric.lfMessageFont)
        } else {
            null_mut()
        }
    }
}

fn browse(owner: HWND) -> Option<PathBuf> {
    let title = wide(tr("Папка для записей", "Recordings folder"));
    let mut info: BROWSEINFOW = unsafe { zeroed() };
    info.hwndOwner = owner;
    info.lpszTitle = title.as_ptr();
    info.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX;
    let item = unsafe { SHBrowseForFolderW(&info) };
    if item.is_null() {
        return None;
    }
    let mut path = [0; MAX_PATH as usize];
    let ok = unsafe { SHGetPathFromIDListW(item, path.as_mut_ptr()) };
    unsafe { CoTaskMemFree(item.cast()) };
    let end = path.iter().position(|&c| c == 0)?;
    (ok != 0 && end != 0).then(|| PathBuf::from(OsString::from_wide(&path[..end])))
}

fn read_text(hwnd: HWND) -> OsString {
    let mut text = vec![0; unsafe { GetWindowTextLengthW(hwnd) } as usize + 1];
    let len = unsafe { GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32) };
    OsString::from_wide(&text[..len.max(0) as usize])
}

fn add_item(hwnd: HWND, text: &str) {
    unsafe { SendMessageW(hwnd, CB_ADDSTRING, 0, wide(text).as_ptr() as isize) };
}

fn choose(hwnd: HWND, index: usize) {
    unsafe { SendMessageW(hwnd, CB_SETCURSEL, index, 0) };
}

fn selection(hwnd: HWND) -> Option<usize> {
    let value = unsafe { SendMessageW(hwnd, CB_GETCURSEL, 0, 0) };
    (value >= 0).then_some(value as usize)
}

fn tick(hwnd: HWND, enabled: bool) {
    unsafe { SendMessageW(hwnd, BM_SETCHECK, usize::from(enabled), 0) };
}

fn checked(hwnd: HWND) -> bool {
    unsafe { SendMessageW(hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED as isize }
}

fn put_hotkey(hwnd: HWND, key: Hotkey) {
    let mut flags = 0;
    for (modifier, native) in [
        (MOD_ALT, HOTKEYF_ALT),
        (MOD_CONTROL, HOTKEYF_CONTROL),
        (MOD_SHIFT, HOTKEYF_SHIFT),
    ] {
        if key.modifiers & modifier != 0 {
            flags |= u32::from(native);
        }
    }
    unsafe { SendMessageW(hwnd, HKM_SETHOTKEY, (key.key | (flags << 8)) as usize, 0) };
}

fn get_hotkey(hwnd: HWND) -> Hotkey {
    let value = unsafe { SendMessageW(hwnd, HKM_GETHOTKEY, 0, 0) } as u32;
    let mut modifiers = 0;
    for (modifier, native) in [
        (MOD_ALT, HOTKEYF_ALT),
        (MOD_CONTROL, HOTKEYF_CONTROL),
        (MOD_SHIFT, HOTKEYF_SHIFT),
    ] {
        if value >> 8 & u32::from(native) != 0 {
            modifiers |= modifier;
        }
    }
    Hotkey {
        modifiers,
        key: value & 0xff,
    }
}

fn fill(dst: &mut [u16], value: &str) {
    let limit = dst.len().saturating_sub(1);
    for (out, input) in dst.iter_mut().take(limit).zip(value.encode_utf16()) {
        *out = input;
    }
}

fn tray_icons() -> [HICON; 4] {
    [
        include_bytes!("../assets/idle.ico").as_slice(),
        include_bytes!("../assets/record.ico").as_slice(),
        include_bytes!("../assets/replay.ico").as_slice(),
        include_bytes!("../assets/both.ico").as_slice(),
    ]
    .map(|file| load_icon(file, 0))
}

fn load_icon(file: &[u8], size: i32) -> HICON {
    unsafe {
        let data = &file[22..];
        let mut aligned = vec![0u32; data.len().div_ceil(4)];
        std::ptr::copy_nonoverlapping(data.as_ptr(), aligned.as_mut_ptr().cast(), data.len());
        let icon = CreateIconFromResourceEx(
            aligned.as_ptr().cast(),
            data.len() as u32,
            1,
            0x0003_0000,
            size,
            size,
            LR_DEFAULTCOLOR,
        );
        if icon.is_null() {
            CopyIcon(LoadIconW(null_mut(), IDI_APPLICATION))
        } else {
            icon
        }
    }
}
