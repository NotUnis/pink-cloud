//! Pink Cloud's notification area: the little icons of apps running in the background.
//!
//! Apps hand their tray icons to whichever window is called "Shell_TrayWnd". With the
//! Windows taskbar hidden, Pink Cloud opens its own window by that name on top, then
//! tells every app to re-send its icons. Everything else sent there (questions about
//! the taskbar's position, ...) is passed on to Explorer's real taskbar unchanged.
//! When Pink Cloud stops, its window goes away and apps are told to re-send their
//! icons again, this time to Windows.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::shell;

const SIGNATURE: i32 = 0x3475_3423;
const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETVERSION: u32 = 4;
const NIF_MESSAGE: u32 = 0x1;
const NIF_ICON: u32 = 0x2;
const NIF_TIP: u32 = 0x4;
const NIF_STATE: u32 = 0x8;
const NIF_GUID: u32 = 0x20;
const NIS_HIDDEN: u32 = 0x1;
const NIN_SELECT: u32 = WM_USER;
const TIMER_TOPMOST: usize = 1;

/// The 32-bit layout of NOTIFYICONDATA that Windows uses when passing tray icons around
/// (window and icon handles are 32-bit values here, even on 64-bit Windows).
#[repr(C)]
struct NotifyIconData {
    cb_size: u32,
    hwnd: u32,
    uid: u32,
    flags: u32,
    callback: u32,
    hicon: u32,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: GUID,
    balloon_icon: u32,
}

#[repr(C)]
struct TrayMessage {
    signature: i32,
    message: u32,
    data: NotifyIconData,
}

#[derive(Clone)]
struct Icon {
    hwnd: u32,
    uid: u32,
    guid: Option<GUID>,
    callback: u32,
    version: u32,
    tip: String,
    image: Option<String>,
    hidden: bool,
}

impl Icon {
    fn key(&self) -> String {
        match self.guid {
            Some(g) => format!("{g:?}"),
            None => format!("{}:{}", self.hwnd, self.uid),
        }
    }
}

#[derive(Serialize)]
pub struct TrayIcon {
    pub key: String,
    pub tip: String,
    pub image: Option<String>,
}

static ICONS: Mutex<Vec<Icon>> = Mutex::new(Vec::new());
static ON_CHANGE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
static OUR_TRAY: AtomicIsize = AtomicIsize::new(0);

fn text(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn changed() {
    if let Some(f) = ON_CHANGE.get() {
        f();
    }
}

fn handle(message: u32, d: &NotifyIconData) {
    let guid = if d.flags & NIF_GUID != 0 { Some(d.guid) } else { None };
    let same = |i: &Icon| match (guid, i.guid) {
        (Some(a), Some(b)) => a == b,
        _ => i.hwnd == d.hwnd && i.uid == d.uid,
    };
    let mut icons = ICONS.lock().unwrap();
    // what the popup shows; only tell it about changes to this
    let looks = |i: &Icon| (i.tip.clone(), i.image.clone(), i.hidden);
    let before = icons.iter().find(|i| same(i)).map(looks);
    match message {
        NIM_ADD | NIM_MODIFY => {
            let pos = icons.iter().position(same);
            if message == NIM_ADD && pos.is_some() {
                return;
            }
            let icon = match pos {
                Some(p) => &mut icons[p],
                None => {
                    icons.push(Icon { hwnd: d.hwnd, uid: d.uid, guid, callback: 0, version: 0, tip: String::new(), image: None, hidden: false });
                    icons.last_mut().unwrap()
                }
            };
            icon.hwnd = d.hwnd;
            icon.uid = d.uid;
            if d.flags & NIF_MESSAGE != 0 {
                icon.callback = d.callback;
            }
            if d.flags & NIF_TIP != 0 {
                icon.tip = text(&d.tip);
            }
            if d.flags & NIF_ICON != 0 {
                icon.image = shell::hicon_data_url(HICON(d.hicon as isize as *mut c_void));
            }
            if d.flags & NIF_STATE != 0 && d.state_mask & NIS_HIDDEN != 0 {
                icon.hidden = d.state & NIS_HIDDEN != 0;
            }
        }
        NIM_DELETE => icons.retain(|i| !same(i)),
        NIM_SETVERSION => {
            if let Some(i) = icons.iter_mut().find(|i| same(i)) {
                i.version = d.version;
            }
            return;
        }
        _ => return,
    }
    let after = icons.iter().find(|i| same(i)).map(looks);
    drop(icons);
    if before != after {
        changed();
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COPYDATA => {
            let cds = &*(lp.0 as *const COPYDATASTRUCT);
            if cds.dwData == 1 && cds.cbData as usize >= 8 && !cds.lpData.is_null() {
                // copy into a full-size struct: older apps send a shorter version
                let mut m: TrayMessage = std::mem::zeroed();
                let n = (cds.cbData as usize).min(size_of::<TrayMessage>());
                std::ptr::copy_nonoverlapping(cds.lpData as *const u8, &mut m as *mut _ as *mut u8, n);
                if m.signature == SIGNATURE {
                    handle(m.message, &m.data);
                    return LRESULT(1);
                }
            }
            let explorer = shell::explorer_tray();
            if explorer.is_invalid() { LRESULT(0) } else { SendMessageW(explorer, msg, Some(wp), Some(lp)) }
        }
        WM_TIMER if wp.0 == TIMER_TOPMOST => {
            // stay first in line, ahead of Explorer's own tray window
            let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            let before = ICONS.lock().unwrap().len();
            ICONS.lock().unwrap().retain(|i| IsWindow(Some(HWND(i.hwnd as isize as *mut c_void))).as_bool());
            if ICONS.lock().unwrap().len() != before {
                changed();
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(h);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

fn announce_taskbar() {
    unsafe {
        let msg = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = SendNotifyMessageW(HWND_BROADCAST, msg, WPARAM(0), LPARAM(0));
    }
}

/// Starts hosting tray icons. `on_change` runs whenever icons are added, change or go away.
pub fn start(on_change: impl Fn() + Send + Sync + 'static) {
    let _ = ON_CHANGE.set(Box::new(on_change));
    std::thread::spawn(|| unsafe {
        let class = w!("Shell_TrayWnd");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let Ok(h) = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, class, PCWSTR::null(), WS_POPUP, 0, 0, 0, 0, None, None, None, None) else { return };
        OUR_TRAY.store(h.0 as isize, Ordering::SeqCst);
        SetTimer(Some(h), TIMER_TOPMOST, 2000, None);
        announce_taskbar();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

/// Closes our tray window and tells apps to hand their icons back to Windows.
pub fn stop() {
    let h = OUR_TRAY.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe {
            SendMessageW(HWND(h as *mut c_void), WM_CLOSE, None, None);
        }
        announce_taskbar();
    }
}

pub fn icons() -> Vec<TrayIcon> {
    ICONS.lock().unwrap().iter().filter(|i| !i.hidden).map(|i| TrayIcon { key: i.key(), tip: i.tip.clone(), image: i.image.clone() }).collect()
}

/// Passes a click on one of our tray icons on to the app it belongs to.
pub fn click(key: &str, button: &str) {
    let Some(icon) = ICONS.lock().unwrap().iter().find(|i| i.key() == key).cloned() else { return };
    let target = HWND(icon.hwnd as isize as *mut c_void);
    let (x, y) = shell::cursor();
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(target, Some(&mut pid));
        // let the app bring its own window or menu to the front
        let _ = AllowSetForegroundWindow(pid);
        let sequence: &[u32] = match (button, icon.version >= 4) {
            ("right", true) => &[WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU],
            ("right", false) => &[WM_RBUTTONDOWN, WM_RBUTTONUP],
            ("double", _) => &[WM_LBUTTONDBLCLK, WM_LBUTTONUP],
            (_, true) => &[WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT],
            (_, false) => &[WM_LBUTTONDOWN, WM_LBUTTONUP],
        };
        for &m in sequence {
            let (wp, lp) = if icon.version >= 4 {
                let at = ((y as u16 as u32) << 16) | (x as u16 as u32);
                (WPARAM(at as usize), LPARAM((((icon.uid & 0xffff) << 16) | m) as isize))
            } else {
                (WPARAM(icon.uid as usize), LPARAM(m as isize))
            };
            let _ = PostMessageW(Some(target), icon.callback, wp, lp);
        }
    }
}
