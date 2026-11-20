use crate::locale::{tr, trf};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::SystemTime;

use crate::media::{Media, PREVIEW_HEIGHT, PREVIEW_WIDTH};
use crate::platform::{self, wide};
use windows::Win32::Foundation::HWND as MediaHwnd;
use windows::Win32::Media::MediaFoundation::{
    IMFPMediaPlayer, IMFPMediaPlayerCallback, MFP_CREATION_OPTIONS, MFP_MEDIAPLAYER_STATE_PLAYING,
    MFP_POSITIONTYPE_100NS, MFPCreateMediaPlayer,
};
use windows::core::{PCWSTR, PROPVARIANT};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::{Com::*, DataExchange::*, Memory::*};
use windows_sys::Win32::UI::{
    Controls::*,
    HiDpi::{GetDpiForSystem, GetDpiForWindow},
    Input::KeyboardAndMouse::*,
    Shell::*,
    WindowsAndMessaging::*,
};

const CLASS: &str = "Recr.Native.Library";
const VIDEO_CLASS: &str = "Recr.Native.Video";
const REFRESH: u32 = WM_APP + 20;
const RESULT: u32 = WM_APP + 21;
const VIDEO_PAINT: u32 = WM_APP + 22;
const TIMER: usize = 1;
const BACK: u16 = 100;
const PLAY: u16 = 101;
const FORWARD: u16 = 102;
const SET_START: u16 = 110;
const SET_END: u16 = 111;
const KEEP: u16 = 112;
const CUT: u16 = 113;
const RESET: u16 = 114;
const SAVE: u16 = 115;
const CANCEL: u16 = 116;
const EDIT_FILE: u32 = 1;
const DELETE_FILE: u32 = 2;
const FAVORITE: u32 = 3;
const COPY_FILE: u32 = 4;
const REVEAL_FILE: u32 = 5;
const TRACK_MAX: usize = 10_000;
const TBM_GETPOS: u32 = WM_USER;
const SS_ENDELLIPSIS: u32 = 0x4000;

thread_local! {
    static LIBRARY: RefCell<Option<Library>> = const { RefCell::new(None) };
    static EDIT_TASKS: RefCell<Vec<(PathBuf, JoinHandle<()>)>> = const { RefCell::new(Vec::new()) };
}

pub fn wait_for_edits() {
    let tasks = EDIT_TASKS.with(|tasks| std::mem::take(&mut *tasks.borrow_mut()));
    for (_, task) in tasks {
        let _ = task.join();
    }
}

pub fn open(owner: HWND, folder: PathBuf, data: PathBuf) -> HWND {
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES,
        });
        register_window(CLASS, Some(library_proc));
        register_window(VIDEO_CLASS, Some(video_proc));
        let dpi = GetDpiForSystem().max(96) as i32;
        let window = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            wide(CLASS).as_ptr(),
            wide("").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            690 * dpi / 96,
            430 * dpi / 96,
            owner,
            null_mut(),
            null_mut(),
            null(),
        );
        if window.is_null() {
            return window;
        }
        LIBRARY.with(|slot| *slot.borrow_mut() = Some(Library::new(window, folder, data)));
        ShowWindow(window, SW_SHOWNORMAL);
        SetForegroundWindow(window);
        SetTimer(window, TIMER, 250, None);
        with_library(window, |state| {
            state.layout();
            state.reload();
        });
        window
    }
}

pub fn refresh(hwnd: HWND) {
    if !hwnd.is_null() {
        unsafe { PostMessageW(hwnd, REFRESH, 0, 0) };
    }
}

pub fn localize(hwnd: HWND) {
    with_library(hwnd, |state| state.localize());
}

pub fn set_folder(hwnd: HWND, folder: PathBuf) {
    with_library(hwnd, |state| {
        if state.folder != folder {
            state.clear_current();
            state.folder = folder;
        }
        state.reload();
    });
}

fn with_library<T>(window: HWND, call: impl FnOnce(&mut Library) -> T) -> Option<T> {
    LIBRARY.with(|slot| {
        let mut slot = slot.try_borrow_mut().ok()?;
        Some(call(slot.as_mut().filter(|state| state.window == window)?))
    })
}

unsafe fn register_window(class: &str, wndproc: WNDPROC) {
    let name = wide(class);
    let window = WNDCLASSW {
        lpfnWndProc: wndproc,
        lpszClassName: name.as_ptr(),
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
        ..unsafe { zeroed() }
    };
    unsafe { RegisterClassW(&window) };
}

unsafe extern "system" fn library_proc(
    window: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    match message {
        WM_CLOSE => {
            unsafe { DestroyWindow(window) };
            return 0;
        }
        WM_DESTROY => {
            unsafe { KillTimer(window, TIMER) };
            LIBRARY.with(|slot| {
                if let Ok(mut slot) = slot.try_borrow_mut() {
                    if slot.as_ref().is_some_and(|state| state.window == window) {
                        slot.take();
                    }
                }
            });
            return 0;
        }
        WM_GETMINMAXINFO => {
            let info = unsafe { &mut *(l as *mut MINMAXINFO) };
            let dpi = unsafe { GetDpiForWindow(window) }.max(96);
            info.ptMinTrackSize = POINT {
                x: (625 * dpi / 96) as i32,
                y: (380 * dpi / 96) as i32,
            };
            return 0;
        }
        WM_DPICHANGED => {
            let rect = unsafe { &*(l as *const RECT) };
            unsafe {
                SetWindowPos(
                    window,
                    null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                )
            };
            return 0;
        }
        _ => {}
    }
    let handled = with_library(window, |state| match message {
        WM_SIZE => {
            state.layout();
            true
        }
        WM_TIMER => {
            state.tick();
            true
        }
        WM_HSCROLL if l as HWND == state.track => {
            state.scroll((w & 0xffff) as u16);
            true
        }
        WM_HSCROLL if l as HWND == state.volume_slider => {
            state.change_volume();
            true
        }
        WM_CONTEXTMENU if w as HWND == state.list && l == -1 => {
            state.list_menu(l);
            true
        }
        WM_NOTIFY => {
            let Some(notification) = (unsafe { (l as *const NMHDR).as_ref() }) else {
                return false;
            };
            if notification.hwndFrom != state.list {
                return false;
            }
            match notification.code {
                LVN_ITEMCHANGED => {
                    let change = unsafe { &*(l as *const NMLISTVIEW) };
                    if change.uChanged & LVIF_STATE != 0
                        && change.uNewState & LVIS_SELECTED != 0
                        && change.uOldState & LVIS_SELECTED == 0
                        && unsafe { GetKeyState(VK_RBUTTON as i32) } >= 0
                    {
                        state.select(change.iItem, true);
                    }
                }
                NM_CLICK => {
                    let click = unsafe { &*(l as *const NMITEMACTIVATE) };
                    state.select(click.iItem, true);
                }
                NM_RCLICK => {
                    let mut point = unsafe { zeroed() };
                    unsafe { GetCursorPos(&mut point) };
                    let POINT { x, y } = point;
                    state.list_menu(((y as u16 as usize) << 16 | x as u16 as usize) as isize);
                }
                LVN_KEYDOWN => {
                    let key = unsafe { (*(l as *const NMLVKEYDOWN)).wVKey };
                    if key == VK_LEFT {
                        state.command(BACK);
                    } else if key == VK_RIGHT {
                        state.command(FORWARD);
                    }
                }
                _ => return false,
            }
            true
        }
        WM_COMMAND => {
            state.command((w & 0xffff) as u16);
            true
        }
        VIDEO_PAINT => {
            if let Some(player) = &state.player {
                let _ = unsafe { player.UpdateVideo() };
            }
            true
        }
        REFRESH => {
            state.reload();
            true
        }
        RESULT => {
            state.results();
            true
        }
        _ => false,
    })
    .unwrap_or(false);
    if handled {
        0
    } else {
        unsafe { DefWindowProcW(window, message, w, l) }
    }
}

unsafe extern "system" fn video_proc(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let parent = unsafe { GetParent(window) };
    match message {
        WM_PAINT => {
            unsafe {
                let mut paint = zeroed();
                let dc = BeginPaint(window, &mut paint);
                FillRect(dc, &paint.rcPaint, GetSysColorBrush(COLOR_WINDOW));
                EndPaint(window, &paint);
                PostMessageW(parent, VIDEO_PAINT, 0, 0);
            }
            return 0;
        }
        WM_SIZE => {
            unsafe { PostMessageW(parent, VIDEO_PAINT, 0, 0) };
        }
        WM_LBUTTONUP => {
            unsafe {
                SetFocus(window);
                PostMessageW(parent, WM_COMMAND, PLAY as usize, 0);
            }
            return 0;
        }
        WM_KEYDOWN => match w as u16 {
            VK_LEFT | VK_RIGHT | VK_SPACE => {
                let command = if w as u16 == VK_SPACE {
                    PLAY
                } else if w as u16 == VK_LEFT {
                    BACK
                } else {
                    FORWARD
                };
                unsafe { PostMessageW(parent, WM_COMMAND, command as usize, 0) };
                return 0;
            }
            _ => {}
        },
        WM_GETDLGCODE => return (DLGC_WANTARROWS | DLGC_WANTCHARS) as isize,
        _ => {}
    }
    unsafe { DefWindowProcW(window, message, w, l) }
}

struct Entry {
    path: PathBuf,
    modified: SystemTime,
}

struct Edit {
    duration: f64,
    ranges: Vec<(f64, f64)>,
}

struct Editor {
    start_label: HWND,
    end_label: HWND,
    start: HWND,
    end: HWND,
    set_start: HWND,
    set_end: HWND,
    keep: HWND,
    cut: HWND,
    reset: HWND,
    save: HWND,
    cancel: HWND,
    summary: HWND,
}

impl Editor {
    fn all(&self) -> [HWND; 12] {
        [
            self.start_label,
            self.end_label,
            self.start,
            self.end,
            self.set_start,
            self.set_end,
            self.keep,
            self.cut,
            self.reset,
            self.save,
            self.cancel,
            self.summary,
        ]
    }

    fn new(window: HWND) -> Self {
        Self {
            start_label: label(window, ""),
            end_label: label(window, ""),
            start: child(
                window,
                "EDIT",
                "",
                WS_TABSTOP | ES_AUTOHSCROLL as u32,
                WS_EX_CLIENTEDGE,
                0,
            ),
            end: child(
                window,
                "EDIT",
                "",
                WS_TABSTOP | ES_AUTOHSCROLL as u32,
                WS_EX_CLIENTEDGE,
                0,
            ),
            set_start: button(window, "", SET_START),
            set_end: button(window, "", SET_END),
            keep: button(window, "", KEEP),
            cut: button(window, "", CUT),
            reset: button(window, "", RESET),
            save: button(window, "", SAVE),
            cancel: button(window, "", CANCEL),
            summary: label(window, ""),
        }
    }
}

enum WorkResult {
    Thumb(PathBuf, SystemTime, Option<Vec<u8>>),
    Edited(io::Result<PathBuf>),
}

struct Library {
    window: HWND,
    list: HWND,
    video: HWND,
    track: HWND,
    volume_slider: HWND,
    volume_label: HWND,
    volume: u8,
    initial_volume: u8,
    status: HWND,
    status_text: (&'static str, &'static str),
    time: HWND,
    back: HWND,
    play: HWND,
    forward: HWND,
    editor: Editor,
    images: HIMAGELIST,
    entries: Vec<Entry>,
    favorites: HashSet<PathBuf>,
    thumbnails: HashMap<PathBuf, (SystemTime, i32)>,
    pending_thumb: bool,
    selected: Option<PathBuf>,
    folder: PathBuf,
    data: PathBuf,
    player: Option<IMFPMediaPlayer>,
    playing: bool,
    duration: f64,
    position: f64,
    dragging: bool,
    editing: Option<Edit>,
    export_source: Option<PathBuf>,
    initialized_com: bool,
    results: Receiver<WorkResult>,
    worker: Sender<WorkResult>,
}

impl Library {
    fn new(window: HWND, folder: PathBuf, data: PathBuf) -> Self {
        let list = child(
            window,
            "SysListView32",
            "",
            WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOCOLUMNHEADER,
            WS_EX_CLIENTEDGE,
            0,
        );
        let video = child(window, VIDEO_CLASS, "", WS_TABSTOP, WS_EX_CLIENTEDGE, 0);
        let track = child(
            window,
            "msctls_trackbar32",
            "",
            WS_TABSTOP | TBS_NOTICKS,
            0,
            0,
        );
        let status = label(window, "");
        let volume = fs::read_to_string(data.join("player-volume.txt"))
            .ok()
            .and_then(|text| text.trim().parse::<u8>().ok())
            .filter(|&value| value <= 100)
            .unwrap_or(100);
        let volume_slider = child(
            window,
            "msctls_trackbar32",
            "",
            WS_TABSTOP | TBS_NOTICKS,
            0,
            0,
        );
        let volume_label = label(window, "");
        let time = label(window, "00:00 / 00:00");
        let back = button(window, "", BACK);
        let play = button(window, "", PLAY);
        let forward = button(window, "", FORWARD);
        let editor = Editor::new(window);
        let dpi = unsafe { GetDpiForWindow(window) }.max(96);
        let images = unsafe {
            let images = ImageList_Create(
                (64 * dpi / 96) as i32,
                (36 * dpi / 96) as i32,
                ILC_COLOR24 | ILC_MASK,
                8,
                8,
            );
            ImageList_ReplaceIcon(images, -1, LoadIconW(null_mut(), IDI_APPLICATION));
            SendMessageW(
                list,
                LVM_SETIMAGELIST,
                LVSIL_SMALL as usize,
                images as isize,
            );
            SendMessageW(
                list,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                0,
                (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize,
            );
            let column = LVCOLUMNW {
                mask: LVCF_WIDTH,
                cx: 220,
                ..zeroed()
            };
            SendMessageW(list, LVM_INSERTCOLUMNW, 0, &column as *const _ as isize);
            SendMessageW(track, TBM_SETRANGEMAX, 1, TRACK_MAX as isize);
            SendMessageW(volume_slider, TBM_SETRANGEMAX, 1, 100);
            SendMessageW(volume_slider, TBM_SETPOS, 1, volume as isize);
            images
        };
        let favorites = fs::read_to_string(data.join("favorites.txt"))
            .unwrap_or_default()
            .lines()
            .map(PathBuf::from)
            .collect();
        let (worker, results) = mpsc::channel();
        let initialized_com =
            unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) >= 0 };
        let state = Self {
            window,
            list,
            video,
            track,
            volume_slider,
            volume_label,
            volume,
            initial_volume: volume,
            status,
            status_text: ("Выберите запись", "Select a recording"),
            time,
            back,
            play,
            forward,
            editor,
            images,
            entries: Vec::new(),
            favorites,
            thumbnails: HashMap::new(),
            pending_thumb: false,
            selected: None,
            folder,
            data,
            player: None,
            playing: false,
            duration: 0.0,
            position: 0.0,
            dragging: false,
            editing: None,
            export_source: None,
            initialized_com,
            results,
            worker,
        };
        state.playback_controls(false);
        state.localize();
        state
    }

    fn localize(&self) {
        for (control, russian, english) in [
            (self.window, "Записи — recr", "Recordings — recr"),
            (self.back, "← 5 с", "← 5 s"),
            (self.forward, "5 с →", "5 s →"),
            (self.editor.start_label, "От, с", "From"),
            (self.editor.end_label, "До, с", "To"),
            (self.editor.set_start, "Текущая", "Use current"),
            (self.editor.set_end, "Текущая", "Use current"),
            (self.editor.keep, "Оставить отрезок", "Keep selection"),
            (self.editor.cut, "Вырезать отрезок", "Cut selection"),
            (self.editor.reset, "Сбросить", "Reset"),
            (self.editor.save, "Сохранить копию", "Save a copy"),
            (self.editor.cancel, "Отмена", "Cancel"),
        ] {
            set_text(control, tr(russian, english));
        }
        self.update_status();
        self.update_play_label();
        self.update_volume_label();
        self.update_editor();
    }

    fn set_status(&mut self, russian: &'static str, english: &'static str) {
        self.status_text = (russian, english);
        self.update_status();
    }

    fn update_status(&self) {
        set_text(self.status, tr(self.status_text.0, self.status_text.1));
    }

    fn update_play_label(&self) {
        set_text(
            self.play,
            if self.playing {
                tr("Пауза", "Pause")
            } else {
                tr("Пуск", "Play")
            },
        );
    }

    fn update_volume_label(&self) {
        set_text(
            self.volume_label,
            trf!("Громкость {}%", "Volume {}%", self.volume),
        );
    }

    fn layout(&self) {
        let mut rect: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.window, &mut rect) };
        if rect.right == 0 || rect.bottom == 0 {
            return;
        }
        let dpi = unsafe { GetDpiForWindow(self.window) }.max(96) as i32;
        let px = |value: i32| (dpi * value + 48) / 96;
        let left = (rect.right * 32 / 100).clamp(px(156), px(220));
        let x = left + px(16);
        let right = rect.right - x - px(8);
        let edit_height = if self.editing.is_some() { px(106) } else { 0 };
        let y = (rect.bottom - px(83) - edit_height).max(px(154));
        let cx = |offset: i32| x + px(offset);
        place(self.list, px(8), px(8), left, rect.bottom - px(16));
        unsafe { SendMessageW(self.list, LVM_SETCOLUMNWIDTH, 0, (left - px(22)) as isize) };
        place(self.video, x, px(8), right, y - px(15));
        place(self.track, x, y, right, px(26));
        place(self.back, x, y + px(28), px(63), px(27));
        place(self.play, cx(69), y + px(28), px(80), px(27));
        place(self.forward, cx(155), y + px(28), px(63), px(27));
        place(self.time, cx(226), y + px(32), right - px(226), px(20));
        let volume_x = x + right - px(104);
        let volume_label_x = volume_x - px(96);
        place(self.status, x, y + px(59), right - px(206), px(19));
        place(
            self.volume_label,
            volume_label_x,
            y + px(59),
            px(94),
            px(19),
        );
        place(self.volume_slider, volume_x, y + px(55), px(104), px(25));
        for control in self.editor.all() {
            unsafe {
                ShowWindow(
                    control,
                    if self.editing.is_some() {
                        SW_SHOW
                    } else {
                        SW_HIDE
                    },
                )
            };
        }
        if self.editing.is_some() {
            let y = y + px(82);
            let next = y + px(30);
            place(self.editor.start_label, x, y + px(4), px(35), px(20));
            place(self.editor.start, cx(36), y, px(58), px(24));
            place(self.editor.set_start, cx(99), y, px(79), px(24));
            place(self.editor.end_label, cx(189), y + px(4), px(36), px(20));
            place(self.editor.end, cx(225), y, px(58), px(24));
            place(self.editor.set_end, cx(288), y, px(79), px(24));
            place(self.editor.keep, x, next, px(116), px(25));
            place(self.editor.cut, cx(122), next, px(116), px(25));
            place(self.editor.reset, cx(244), next, px(68), px(25));
            place(
                self.editor.summary,
                cx(210),
                next + px(35),
                right - px(210),
                px(19),
            );
            place(self.editor.save, x, next + px(31), px(121), px(25));
            place(self.editor.cancel, cx(127), next + px(31), px(76), px(25));
        }
    }

    fn reload(&mut self) {
        let mut entries: Vec<_> = fs::read_dir(&self.folder)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let metadata = entry.metadata().ok()?;
                (metadata.is_file() && is_video(&path)).then(|| Entry {
                    path,
                    modified: metadata
                        .modified()
                        .or_else(|_| metadata.created())
                        .unwrap_or(SystemTime::UNIX_EPOCH),
                })
            })
            .collect();
        entries.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| b.path.cmp(&a.path))
        });
        let selected = entries
            .iter()
            .position(|entry| self.selected.as_ref() == Some(&entry.path));
        if selected.is_none() && self.selected.is_some() {
            self.clear_current();
        }
        self.entries = entries;
        unsafe {
            SendMessageW(self.list, WM_SETREDRAW, 0, 0);
            SendMessageW(self.list, LVM_DELETEALLITEMS, 0, 0);
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let mut name = self.entry_label(&entry.path);
            let image = self
                .thumbnails
                .get(&entry.path)
                .filter(|(modified, _)| *modified == entry.modified)
                .map_or(0, |(_, image)| *image);
            let item = LVITEMW {
                mask: LVIF_TEXT | LVIF_IMAGE,
                iItem: index as i32,
                pszText: name.as_mut_ptr(),
                iImage: image,
                ..unsafe { zeroed() }
            };
            unsafe { SendMessageW(self.list, LVM_INSERTITEMW, 0, &item as *const _ as isize) };
        }
        if let Some(index) = selected {
            self.mark_list_item(index as i32);
            unsafe { SendMessageW(self.list, LVM_ENSUREVISIBLE, index, 0) };
        }
        unsafe {
            SendMessageW(self.list, WM_SETREDRAW, 1, 0);
            InvalidateRect(self.list, null(), 0);
        }
        if self.selected.is_none() {
            if self.entries.is_empty() {
                self.set_status("Записей пока нет", "No recordings yet");
            } else {
                self.set_status("Выберите запись", "Select a recording");
            }
        }
        self.next_thumbnail();
    }

    fn entry_label(&self, path: &Path) -> Vec<u16> {
        let name = path.file_name().unwrap_or_default();
        if !self.favorites.contains(path) {
            return wide(name);
        }
        let mut value = OsString::from("★ ");
        value.push(name);
        wide(value)
    }

    fn mark_list_item(&self, index: i32) {
        let selection = LVITEMW {
            stateMask: LVIS_FOCUSED | LVIS_SELECTED,
            state: LVIS_FOCUSED | LVIS_SELECTED,
            ..unsafe { zeroed() }
        };
        unsafe {
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                index as usize,
                &selection as *const _ as isize,
            )
        };
    }

    fn select(&mut self, index: i32, autoplay: bool) {
        let Some(path) = usize::try_from(index)
            .ok()
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        if self.selected.as_ref() == Some(&path) {
            return;
        }
        self.clear_current();
        self.selected = Some(path.clone());
        let mut player = None;
        let url = wide(&path);
        let created = unsafe {
            MFPCreateMediaPlayer(
                PCWSTR(url.as_ptr()),
                false,
                MFP_CREATION_OPTIONS(0),
                None::<&IMFPMediaPlayerCallback>,
                MediaHwnd(self.video),
                Some(&mut player),
            )
        };
        let Some(player) = player.filter(|_| created.is_ok()) else {
            self.set_status("Не удалось открыть видео", "Could not open the video");
            return;
        };
        unsafe {
            let _ = player.SetVolume(f32::from(self.volume) / 100.0);
            if autoplay {
                let _ = player.Play();
            }
        }
        self.player = Some(player);
        self.playback_controls(true);
        self.set_status("", "");
        self.tick();
    }

    fn change_volume(&mut self) {
        let position = unsafe { SendMessageW(self.volume_slider, TBM_GETPOS, 0, 0) };
        self.volume = position.clamp(0, 100) as u8;
        self.update_volume_label();
        if let Some(player) = &self.player {
            let _ = unsafe { player.SetVolume(f32::from(self.volume) / 100.0) };
        }
    }

    fn clear_current(&mut self) {
        if let Some(player) = self.player.take() {
            let _ = unsafe { player.Shutdown() };
        }
        self.selected = None;
        self.playing = false;
        self.duration = 0.0;
        self.position = 0.0;
        self.dragging = false;
        if self.editing.take().is_some() {
            self.layout();
        }
        self.playback_controls(false);
        self.update_play_label();
        set_text(self.time, "00:00 / 00:00");
        unsafe {
            SendMessageW(self.track, TBM_SETPOS, 1, 0);
            InvalidateRect(self.video, null(), 1);
        }
    }

    fn playback_controls(&self, enabled: bool) {
        for control in [self.back, self.play, self.forward, self.track] {
            unsafe { EnableWindow(control, i32::from(enabled)) };
        }
    }

    fn tick(&mut self) {
        if let Some(player) = &self.player {
            if self.duration == 0.0 {
                if let Ok(value) = unsafe { player.GetDuration(&MFP_POSITIONTYPE_100NS) } {
                    self.duration =
                        i64::try_from(&value).unwrap_or_default().max(0) as f64 / 10_000_000.0;
                }
            }
            if let Ok(value) = unsafe { player.GetPosition(&MFP_POSITIONTYPE_100NS) } {
                if let Ok(time) = i64::try_from(&value) {
                    self.position = time as f64 / 10_000_000.0;
                }
            }
            let playing = unsafe { player.GetState() }
                .is_ok_and(|state| state == MFP_MEDIAPLAYER_STATE_PLAYING);
            if self.playing != playing {
                self.playing = playing;
                self.update_play_label();
            }
            if !self.dragging {
                self.position_ui(self.position);
            }
        }
        self.next_thumbnail();
    }

    fn position_ui(&self, position: f64) {
        set_text(
            self.time,
            format!("{} / {}", clock(position), clock(self.duration)),
        );
        if !self.dragging && self.duration > 0.0 {
            let pos = (position / self.duration * TRACK_MAX as f64).round();
            unsafe { SendMessageW(self.track, TBM_SETPOS, 1, pos as isize) };
        }
    }

    fn scroll(&mut self, action: u16) {
        if self.duration <= 0.0 {
            return;
        }
        let pos = unsafe { SendMessageW(self.track, TBM_GETPOS, 0, 0) };
        let seconds = self.duration * pos as f64 / TRACK_MAX as f64;
        if action == TB_THUMBTRACK as u16 {
            self.dragging = true;
            self.position_ui(seconds);
        } else if action != TB_ENDTRACK as u16 || self.dragging {
            self.dragging = false;
            self.seek(seconds);
        }
    }

    fn seek(&mut self, seconds: f64) {
        let Some(player) = &self.player else { return };
        let seconds = seconds.clamp(0.0, (self.duration - 0.02).max(0.0));
        let timestamp = PROPVARIANT::from((seconds * 10_000_000.0).round() as i64);
        if unsafe { player.SetPosition(&MFP_POSITIONTYPE_100NS, &timestamp) }.is_ok() {
            self.position = seconds;
            self.position_ui(seconds);
        }
    }

    fn toggle(&mut self) {
        let Some(player) = self.player.clone() else {
            return;
        };
        if unsafe { player.GetState() }.is_ok_and(|state| state == MFP_MEDIAPLAYER_STATE_PLAYING) {
            let _ = unsafe { player.Pause() };
        } else {
            if self.duration > 0.0 && self.position >= (self.duration - 0.1).max(0.0) {
                self.seek(0.0);
            }
            let _ = unsafe { player.Play() };
        }
    }

    fn list_menu(&mut self, coordinates: LPARAM) {
        let keyboard = coordinates == -1;
        let mut point = POINT {
            x: coordinates as i16 as i32,
            y: (coordinates >> 16) as i16 as i32,
        };
        let index = unsafe {
            if keyboard {
                SendMessageW(
                    self.list,
                    LVM_GETNEXTITEM,
                    usize::MAX,
                    LVNI_SELECTED as isize,
                ) as i32
            } else {
                let mut hit = LVHITTESTINFO {
                    pt: point,
                    ..zeroed()
                };
                ScreenToClient(self.list, &mut hit.pt);
                SendMessageW(self.list, LVM_HITTEST, 0, &mut hit as *mut _ as isize) as i32
            }
        };
        let Some(path) = usize::try_from(index)
            .ok()
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        if keyboard {
            let mut item: RECT = unsafe { zeroed() };
            item.left = LVIR_BOUNDS as i32;
            unsafe {
                SendMessageW(
                    self.list,
                    LVM_GETITEMRECT,
                    index as usize,
                    &mut item as *mut _ as isize,
                )
            };
            point = POINT {
                x: item.left + 12,
                y: item.bottom,
            };
            unsafe { ClientToScreen(self.list, &mut point) };
        }
        let can_delete = !EDIT_TASKS.with(|tasks| {
            tasks
                .borrow()
                .iter()
                .any(|(source, task)| *source == path && !task.is_finished())
        });
        let can_edit = self.export_source.is_none();
        let favorite = self.favorites.contains(&path);
        let choice = unsafe {
            let menu = CreatePopupMenu();
            for (id, name, enabled) in [
                (EDIT_FILE, tr("Изменить", "Edit"), can_edit),
                (DELETE_FILE, tr("Удалить", "Delete"), can_delete),
                (
                    FAVORITE,
                    if favorite {
                        tr("Убрать из избранного", "Remove from favorites")
                    } else {
                        tr("В избранное", "Add to favorites")
                    },
                    true,
                ),
                (COPY_FILE, tr("Скопировать", "Copy"), true),
                (REVEAL_FILE, tr("Открыть в папке", "Show in folder"), true),
            ] {
                AppendMenuW(
                    menu,
                    MF_STRING | if enabled { MF_ENABLED } else { MF_GRAYED },
                    id as usize,
                    wide(name).as_ptr(),
                );
            }
            let choice = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.window,
                null(),
            ) as u32;
            DestroyMenu(menu);
            choice
        };
        match choice {
            EDIT_FILE if can_edit => {
                self.mark_list_item(index);
                self.select(index, false);
                self.begin_edit();
            }
            DELETE_FILE if can_delete => self.delete(&path),
            FAVORITE => self.favorite(path, index, favorite),
            COPY_FILE => {
                if copy_file(self.window, &path) {
                    self.set_status("Файл в буфере обмена", "File copied to the clipboard");
                } else {
                    platform::message(
                        self.window,
                        tr("Не удалось скопировать файл.", "Could not copy the file."),
                        true,
                    );
                }
            }
            REVEAL_FILE => {
                let mut args = OsString::from("/select,\"");
                args.push(path);
                args.push("\"");
                unsafe {
                    ShellExecuteW(
                        self.window,
                        wide("open").as_ptr(),
                        wide("explorer.exe").as_ptr(),
                        wide(args).as_ptr(),
                        null(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            _ => {}
        }
        self.results();
    }

    fn delete(&mut self, path: &Path) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let confirmed = unsafe {
            MessageBoxW(
                self.window,
                wide(trf!(
                    "Переместить «{}» в корзину?",
                    "Move “{}” to the Recycle Bin?",
                    name
                ))
                .as_ptr(),
                wide("recr").as_ptr(),
                MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
            )
        } == IDYES;
        if !confirmed {
            return;
        }
        if self.selected.as_deref() == Some(path) {
            self.clear_current();
        }
        let mut paths = wide(path);
        paths.push(0);
        let options = &mut SHFILEOPSTRUCTW {
            hwnd: self.window,
            wFunc: FO_DELETE,
            pFrom: paths.as_ptr(),
            fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING) as u16,
            ..unsafe { zeroed() }
        };
        if unsafe { SHFileOperationW(options) } != 0 {
            platform::message(
                self.window,
                tr(
                    "Не удалось удалить запись.",
                    "Could not delete the recording.",
                ),
                true,
            );
        } else if options.fAnyOperationsAborted == 0 && self.favorites.remove(path) {
            let _ = self.save_favorites();
        }
        self.reload();
    }

    fn favorite(&mut self, path: PathBuf, index: i32, was_favorite: bool) {
        if was_favorite {
            self.favorites.remove(&path);
        } else {
            self.favorites.insert(path.clone());
        }
        if self.save_favorites().is_err() {
            if was_favorite {
                self.favorites.insert(path);
            } else {
                self.favorites.remove(&path);
            }
            platform::message(
                self.window,
                tr(
                    "Не удалось сохранить избранное.",
                    "Could not save favorites.",
                ),
                true,
            );
            return;
        }
        let mut label = self.entry_label(&path);
        let item = LVITEMW {
            pszText: label.as_mut_ptr(),
            ..unsafe { zeroed() }
        };
        unsafe {
            SendMessageW(
                self.list,
                LVM_SETITEMTEXTW,
                index as usize,
                &item as *const _ as isize,
            )
        };
    }

    fn save_favorites(&self) -> io::Result<()> {
        fs::create_dir_all(&self.data)?;
        let mut lines: Vec<_> = self
            .favorites
            .iter()
            .map(|path| path.to_string_lossy())
            .collect();
        lines.sort_unstable();
        platform::persist(&self.data.join("favorites.txt"), &lines.join("\n"))
    }

    fn command(&mut self, command: u16) {
        match command {
            BACK => self.seek(self.position - 5.0),
            FORWARD => self.seek(self.position + 5.0),
            PLAY => self.toggle(),
            SET_START => seconds_in(self.editor.start, self.position),
            SET_END => seconds_in(self.editor.end, self.position),
            KEEP | CUT => self.make_edit(command == CUT),
            RESET => {
                if let Some(edit) = &mut self.editing {
                    edit.ranges = vec![(0.0, edit.duration)];
                    seconds_in(self.editor.start, 0.0);
                    seconds_in(self.editor.end, edit.duration);
                    self.update_editor();
                }
            }
            SAVE => self.export(),
            CANCEL => {
                self.editing = None;
                self.layout();
            }
            _ => {}
        }
    }

    fn begin_edit(&mut self) {
        let Some(path) = &self.selected else {
            return;
        };
        if let Some(player) = &self.player {
            if self.playing {
                let _ = unsafe { player.Pause() };
            }
        }
        let duration = if self.duration > 0.0 {
            Some(self.duration)
        } else {
            Media {
                videos: self.folder.clone(),
            }
            .duration(path)
            .ok()
        };
        let Some(duration) = duration else {
            self.set_status(
                "Не удалось узнать длительность видео",
                "Could not read the video duration",
            );
            return;
        };
        self.editing = Some(Edit {
            duration,
            ranges: vec![(0.0, duration)],
        });
        seconds_in(self.editor.start, 0.0);
        seconds_in(self.editor.end, duration);
        self.set_status("", "");
        self.update_editor();
        self.layout();
    }

    fn make_edit(&mut self, remove: bool) {
        let Some(edit) = &mut self.editing else {
            return;
        };
        let parse = |control| {
            text_of(control)
                .to_string_lossy()
                .trim()
                .replace(',', ".")
                .parse::<f64>()
                .ok()
        };
        let (Some(start), Some(end)) = (parse(self.editor.start), parse(self.editor.end)) else {
            self.set_status(
                "Укажите начало и конец в секундах",
                "Enter start and end times in seconds",
            );
            return;
        };
        if !start.is_finite()
            || !end.is_finite()
            || start < 0.0
            || end > edit.duration + 0.001
            || end - start < 0.03
        {
            self.set_status("Проверьте границы отрезка", "Check the selection bounds");
            return;
        }
        let end = end.min(edit.duration);
        let mut next = Vec::new();
        for &(left, right) in &edit.ranges {
            if remove {
                let before = right.min(start);
                let after = left.max(end);
                if before - left >= 0.03 {
                    next.push((left, before));
                }
                if right - after >= 0.03 {
                    next.push((after, right));
                }
            } else {
                let left = left.max(start);
                let right = right.min(end);
                if right - left >= 0.03 {
                    next.push((left, right));
                }
            }
        }
        if next.is_empty() {
            self.set_status(
                "В копии должен остаться хотя бы один отрезок",
                "Keep at least one segment",
            );
            return;
        }
        edit.ranges = next;
        self.set_status("", "");
        self.update_editor();
    }

    fn update_editor(&self) {
        if let Some(edit) = &self.editing {
            let total: f64 = edit.ranges.iter().map(|(a, b)| b - a).sum();
            set_text(
                self.editor.summary,
                trf!("Итог {}", "Total {}", clock(total)),
            );
            unsafe { EnableWindow(self.editor.save, i32::from(self.export_source.is_none())) };
        }
    }

    fn export(&mut self) {
        let (Some(edit), Some(path)) = (&self.editing, self.selected.clone()) else {
            return;
        };
        if self.export_source.is_some() {
            return;
        }
        let ranges = edit.ranges.clone();
        let file = path.clone();
        let media = Media {
            videos: self.folder.clone(),
        };
        let callback = self.worker.clone();
        let window = self.window as isize;
        self.export_source = Some(path.clone());
        self.update_editor();
        self.set_status("Сохранение копии…", "Saving copy…");
        let task = std::thread::spawn(move || {
            send_result(
                &callback,
                window,
                WorkResult::Edited(media.edit(&file, &ranges)),
            );
        });
        EDIT_TASKS.with(|tasks| {
            let mut tasks = tasks.borrow_mut();
            tasks.retain(|(_, task)| !task.is_finished());
            tasks.push((path, task));
        });
    }

    fn next_thumbnail(&mut self) {
        if self.pending_thumb || unsafe { IsIconic(self.window) } != 0 {
            return;
        }
        let first = unsafe { SendMessageW(self.list, LVM_GETTOPINDEX, 0, 0) }.max(0) as usize;
        let visible = unsafe { SendMessageW(self.list, LVM_GETCOUNTPERPAGE, 0, 0) }.max(1) as usize;
        let Some(entry) = self
            .entries
            .iter()
            .skip(first)
            .take(visible + 3)
            .find(|entry| {
                self.thumbnails
                    .get(&entry.path)
                    .is_none_or(|(modified, _)| entry.modified != *modified)
            })
        else {
            return;
        };
        let path = entry.path.clone();
        let modified = entry.modified;
        self.pending_thumb = true;
        let media = Media {
            videos: self.folder.clone(),
        };
        let callback = self.worker.clone();
        let window = self.window as isize;
        std::thread::spawn(move || {
            let frame = media
                .preview(&path, 0.4)
                .or_else(|_| media.preview(&path, 0.0))
                .ok();
            send_result(&callback, window, WorkResult::Thumb(path, modified, frame));
        });
    }

    fn results(&mut self) {
        while let Ok(result) = self.results.try_recv() {
            match result {
                WorkResult::Thumb(path, modified, frame) => {
                    self.pending_thumb = false;
                    let index = self
                        .entries
                        .iter()
                        .position(|entry| entry.path == path && entry.modified == modified);
                    if let Some(index) = index {
                        let image = frame
                            .as_deref()
                            .and_then(|frame| thumbnail(self.window, self.images, frame))
                            .unwrap_or(0);
                        self.thumbnails.insert(path, (modified, image));
                        let item = LVITEMW {
                            mask: LVIF_IMAGE,
                            iItem: index as i32,
                            iImage: image,
                            ..unsafe { zeroed() }
                        };
                        unsafe {
                            SendMessageW(self.list, LVM_SETITEMW, 0, &item as *const _ as isize)
                        };
                    }
                }
                WorkResult::Edited(result) => {
                    let was_current = self.export_source.as_ref() == self.selected.as_ref();
                    self.export_source = None;
                    match result {
                        Ok(_) => {
                            if was_current {
                                self.editing = None;
                                self.layout();
                            }
                            self.reload();
                            self.set_status("Копия сохранена", "Copy saved");
                        }
                        Err(error) => {
                            let _ = fs::create_dir_all(&self.data);
                            let _ = fs::write(self.data.join("edit-error.log"), error.to_string());
                            self.set_status(
                                "Не удалось сохранить копию",
                                "Could not save the copy",
                            );
                        }
                    }
                    self.update_editor();
                }
            }
        }
        self.next_thumbnail();
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        if self.volume != self.initial_volume && fs::create_dir_all(&self.data).is_ok() {
            let _ = platform::persist(
                &self.data.join("player-volume.txt"),
                &self.volume.to_string(),
            );
        }
        if let Some(player) = self.player.take() {
            let _ = unsafe { player.Shutdown() };
        }
        unsafe {
            SendMessageW(self.list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, 0);
            ImageList_Destroy(self.images);
            if self.initialized_com {
                CoUninitialize();
            }
        }
    }
}

fn send_result(out: &Sender<WorkResult>, window: isize, result: WorkResult) {
    if out.send(result).is_ok() {
        unsafe { PostMessageW(window as HWND, RESULT, 0, 0) };
    }
}

fn is_video(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return false;
    };
    !path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .starts_with('.')
        && ["mp4", "mkv", "mov", "avi", "wmv", "webm"]
            .iter()
            .any(|kind| extension.eq_ignore_ascii_case(kind))
}

fn clock(value: f64) -> String {
    let seconds = value.max(0.0) as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

fn seconds_in(control: HWND, value: f64) {
    let value = format!("{value:.3}");
    set_text(control, value.trim_end_matches('0').trim_end_matches('.'));
}

fn child(parent: HWND, class: &str, caption: &str, style: u32, extra: u32, id: u16) -> HWND {
    unsafe {
        let window = CreateWindowExW(
            extra,
            wide(class).as_ptr(),
            wide(caption).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            0,
            0,
            parent,
            id as usize as HMENU,
            null_mut(),
            null(),
        );
        SendMessageW(
            window,
            WM_SETFONT,
            GetStockObject(DEFAULT_GUI_FONT) as usize,
            1,
        );
        window
    }
}

fn button(parent: HWND, caption: &str, id: u16) -> HWND {
    child(
        parent,
        "BUTTON",
        caption,
        WS_TABSTOP | BS_PUSHBUTTON as u32,
        0,
        id,
    )
}

fn label(parent: HWND, caption: &str) -> HWND {
    child(parent, "STATIC", caption, SS_ENDELLIPSIS, 0, 0)
}

fn place(control: HWND, x: i32, y: i32, width: i32, height: i32) {
    unsafe { MoveWindow(control, x, y, width.max(0), height.max(0), 1) };
}

fn set_text(control: HWND, text: impl AsRef<std::ffi::OsStr>) {
    unsafe { SetWindowTextW(control, wide(text).as_ptr()) };
}

fn text_of(control: HWND) -> OsString {
    unsafe {
        let mut text = vec![0; GetWindowTextLengthW(control) as usize + 1];
        let length = GetWindowTextW(control, text.as_mut_ptr(), text.len() as i32);
        OsString::from_wide(&text[..length.max(0) as usize])
    }
}

fn thumbnail(window: HWND, images: HIMAGELIST, frame: &[u8]) -> Option<i32> {
    if frame.len() != (PREVIEW_WIDTH * PREVIEW_HEIGHT * 4) as usize {
        return None;
    }
    unsafe {
        let (mut width, mut height) = (0, 0);
        if ImageList_GetIconSize(images, &mut width, &mut height) == 0 {
            return None;
        }
        let panel_dc = GetDC(window);
        if panel_dc.is_null() {
            return None;
        }
        let bitmap = CreateCompatibleBitmap(panel_dc, width, height);
        let dc = CreateCompatibleDC(panel_dc);
        ReleaseDC(window, panel_dc);
        if bitmap.is_null() || dc.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            if !dc.is_null() {
                DeleteDC(dc);
            }
            return None;
        }
        let previous = SelectObject(dc, bitmap);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: PREVIEW_WIDTH,
                biHeight: -PREVIEW_HEIGHT,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..zeroed()
            },
            ..zeroed()
        };
        SetStretchBltMode(dc, HALFTONE);
        StretchDIBits(
            dc,
            0,
            0,
            width,
            height,
            0,
            0,
            PREVIEW_WIDTH,
            PREVIEW_HEIGHT,
            frame.as_ptr().cast(),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
        SelectObject(dc, previous);
        let index = ImageList_Add(images, bitmap, null_mut());
        DeleteObject(bitmap);
        DeleteDC(dc);
        (index >= 0).then_some(index)
    }
}

fn copy_file(owner: HWND, path: &Path) -> bool {
    const CF_HDROP: u32 = 15;
    let names: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    unsafe {
        if OpenClipboard(owner) == 0 {
            return false;
        }
        let bytes = size_of::<DROPFILES>() + names.len() * size_of::<u16>();
        let handle = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes);
        let memory = GlobalLock(handle);
        let success = !memory.is_null() && {
            let drop = memory.cast::<DROPFILES>();
            (*drop).pFiles = size_of::<DROPFILES>() as u32;
            (*drop).fWide = 1;
            std::ptr::copy_nonoverlapping(
                names.as_ptr(),
                memory
                    .cast::<u8>()
                    .add(size_of::<DROPFILES>())
                    .cast::<u16>(),
                names.len(),
            );
            GlobalUnlock(handle);
            EmptyClipboard() != 0 && !SetClipboardData(CF_HDROP, handle).is_null()
        };
        if !success && !handle.is_null() {
            GlobalFree(handle);
        }
        if success {
            let format = RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr());
            if format != 0 {
                // Explorer can use the same file list for Cut. State the requested copy effect.
                let effect = GlobalAlloc(GMEM_MOVEABLE, size_of::<u32>());
                let value = GlobalLock(effect).cast::<u32>();
                if !value.is_null() {
                    value.write(1);
                    GlobalUnlock(effect);
                }
                if (value.is_null() || SetClipboardData(format, effect).is_null())
                    && !effect.is_null()
                {
                    GlobalFree(effect);
                }
            }
        }
        CloseClipboard();
        success
    }
}
