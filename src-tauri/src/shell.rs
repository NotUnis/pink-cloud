//! Everything that talks to Windows itself: the real taskbar, open windows,
//! app icons, Start menu shortcuts, volume, battery, Wi-Fi and power.

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Serialize;
use windows::core::{w, Interface, BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::*;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::System::Com::*;
use windows::Win32::System::Power::*;
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Controls::IImageList;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// SHAppBarMessage messages and values
const ABM_REMOVE: u32 = 1;
const ABM_GETSTATE: u32 = 4;
const ABM_SETSTATE: u32 = 10;
const ABS_AUTOHIDE: isize = 1;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn hw(h: isize) -> HWND {
    HWND(h as *mut c_void)
}

pub fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

// ---------- the real Windows taskbar ----------

fn state_file() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    Path::new(&base).join("PinkCloud").join("windows-taskbar-state.txt")
}

/// Explorer's real taskbar window. Pink Cloud runs its own window with the same class name
/// (that's how apps find the tray), so ours is skipped.
pub fn explorer_tray() -> HWND {
    let own = std::process::id();
    let mut after: Option<HWND> = None;
    unsafe {
        while let Ok(t) = FindWindowExW(None, after, w!("Shell_TrayWnd"), PCWSTR::null()) {
            if t.is_invalid() {
                break;
            }
            if pid_of(t) != own {
                return t;
            }
            after = Some(t);
        }
    }
    HWND::default()
}

fn tray_windows() -> Vec<HWND> {
    let mut found = Vec::new();
    unsafe {
        let t = explorer_tray();
        if !t.is_invalid() {
            found.push(t);
        }
        let mut after: Option<HWND> = None;
        while let Ok(s) = FindWindowExW(None, after, w!("Shell_SecondaryTrayWnd"), PCWSTR::null()) {
            if s.is_invalid() {
                break;
            }
            found.push(s);
            after = Some(s);
        }
    }
    found
}

fn explorer_bar_state(set: Option<isize>) -> isize {
    unsafe {
        let mut abd = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, ..Default::default() };
        abd.hWnd = explorer_tray();
        let current = SHAppBarMessage(ABM_GETSTATE, &mut abd) as isize;
        if let Some(v) = set {
            abd.lParam = LPARAM(v);
            SHAppBarMessage(ABM_SETSTATE, &mut abd);
        }
        current
    }
}

/// Hides the Windows taskbar and sets it to auto-hide so it stops reserving space.
/// The original setting is written to a file first, so it survives a crash.
pub fn take_over_taskbar() {
    let file = state_file();
    let current = explorer_bar_state(None);
    if !file.exists() {
        let _ = std::fs::create_dir_all(file.parent().unwrap());
        let _ = std::fs::write(&file, current.to_string());
    }
    explorer_bar_state(Some(current | ABS_AUTOHIDE));
    keep_windows_taskbar_hidden();
}

/// Called on every poll: if Explorer restarted (or wasn't ready yet when Pink Cloud
/// started at sign-in), hide its taskbar again and make sure it doesn't reserve space.
pub fn keep_windows_taskbar_hidden() {
    if !explorer_tray().is_invalid() {
        let state = explorer_bar_state(None);
        if state & ABS_AUTOHIDE == 0 {
            explorer_bar_state(Some(state | ABS_AUTOHIDE));
        }
    }
    for t in tray_windows() {
        unsafe {
            if IsWindowVisible(t).as_bool() {
                let _ = ShowWindow(t, SW_HIDE);
            }
        }
    }
}

/// Puts the Windows taskbar back exactly how it was.
pub fn restore_taskbar() {
    let file = state_file();
    let Ok(saved) = std::fs::read_to_string(&file) else { return };
    let original = saved.trim().parse::<isize>().unwrap_or(0);
    explorer_bar_state(Some(original));
    for t in tray_windows() {
        unsafe {
            let _ = ShowWindow(t, SW_SHOWNA);
        }
    }
    let _ = std::fs::remove_file(file);
}

// ---------- our own windows ----------

/// Releases any screen space reserved for our taskbar (older versions reserved a strip).
pub fn unregister_appbar(hwnd: isize) {
    unsafe {
        let mut abd = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, hWnd: hw(hwnd), ..Default::default() };
        SHAppBarMessage(ABM_REMOVE, &mut abd);
    }
}

pub fn round_corners(hwnd: isize) {
    unsafe {
        let pref = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hw(hwnd), DWMWA_WINDOW_CORNER_PREFERENCE, &pref as *const _ as *const c_void, size_of_val(&pref) as u32);
    }
}

/// Clicking the taskbar shouldn't steal focus from the app you're using.
pub fn no_activate(hwnd: isize) {
    unsafe {
        let ex = GetWindowLongPtrW(hw(hwnd), GWL_EXSTYLE);
        SetWindowLongPtrW(hw(hwnd), GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize);
    }
}

// ---------- open windows ----------

#[derive(Serialize, Clone)]
pub struct WinInfo {
    pub hwnd: i64,
    pub title: String,
    pub exe: String,
    /// Set for web apps and Store apps: they run inside a host program (Edge, Chrome,
    /// ApplicationFrameHost), so their own app ID tells them apart and finds their icon.
    pub app_id: String,
    pub active: bool,
    pub minimized: bool,
    /// Stays above other windows ("Keep on top").
    pub topmost: bool,
}

/// Programs that host other apps' windows.
const HOSTS: [&str; 7] = ["applicationframehost.exe", "msedge.exe", "chrome.exe", "brave.exe", "vivaldi.exe", "opera.exe", "firefox.exe"];

/// The app ID Windows groups a window under, if it's an app hosted by another program.
fn hosted_app_id(h: HWND, exe: &str) -> String {
    let name = exe.rsplit('\\').next().unwrap_or("").to_lowercase();
    if !HOSTS.contains(&name.as_str()) {
        return String::new();
    }
    unsafe {
        let Ok(store) = windows::Win32::UI::Shell::PropertiesSystem::SHGetPropertyStoreForWindow::<windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore>(h) else { return String::new() };
        let Ok(v) = store.GetValue(&windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID) else { return String::new() };
        let id = v.to_string();
        // the browser's own windows have a plain ID like "MSEdge"; web apps add more after it
        if name == "applicationframehost.exe" || id.contains('!') || id.contains('_') { id } else { String::new() }
    }
}

/// The icon a window shows in its own title bar, for apps whose program has no good icon.
pub fn window_icon(hwnd: i64) -> Option<String> {
    unsafe {
        let h = hw(hwnd as isize);
        let mut res = 0usize;
        let _ = SendMessageTimeoutW(h, WM_GETICON, WPARAM(ICON_BIG as usize), LPARAM(0), SMTO_ABORTIFHUNG, 200, Some(&mut res));
        if res == 0 {
            res = GetClassLongPtrW(h, GCLP_HICON);
        }
        if res == 0 {
            let _ = SendMessageTimeoutW(h, WM_GETICON, WPARAM(ICON_SMALL2 as usize), LPARAM(0), SMTO_ABORTIFHUNG, 200, Some(&mut res));
        }
        if res == 0 { None } else { hicon_data_url(HICON(res as *mut c_void)) }
    }
}

unsafe extern "system" fn collect(h: HWND, lp: LPARAM) -> BOOL {
    let v = &mut *(lp.0 as *mut Vec<HWND>);
    v.push(h);
    TRUE
}

fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn pid_of(h: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid
}

pub fn exe_of(pid: u32) -> String {
    unsafe {
        let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(p);
        if ok { String::from_utf16_lossy(&buf[..len as usize]) } else { String::new() }
    }
}

/// Windows that belong on a taskbar, top of the stack first.
pub fn list_windows(own_pid: u32) -> Vec<WinInfo> {
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
    }
    let fg = unsafe { GetForegroundWindow() };
    all.into_iter()
        .filter_map(|h| unsafe {
            if !IsWindowVisible(h).as_bool() {
                return None;
            }
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
            let has_owner = GetWindow(h, GW_OWNER).map(|o| !o.is_invalid()).unwrap_or(false);
            if ex & WS_EX_APPWINDOW.0 == 0 && (has_owner || ex & WS_EX_TOOLWINDOW.0 != 0) {
                return None;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut c_void, 4);
            if cloaked != 0 {
                return None;
            }
            let len = GetWindowTextLengthW(h);
            if len == 0 {
                return None;
            }
            let class = class_of(h);
            if class == "Progman" || class == "WorkerW" || class == "Shell_TrayWnd" {
                return None;
            }
            let pid = pid_of(h);
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(h, &mut buf);
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            // Pink Cloud's own windows stay off the taskbar, except Files and Settings,
            // which get an app ID each so they don't share one button
            let (exe, app_id) = if pid == own_pid {
                let id = match title.as_str() {
                    "Files" => "pinkcloud.files",
                    "Settings" => "pinkcloud.settings",
                    "Task Manager" => "pinkcloud.tasks",
                    _ => return None,
                };
                (std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(), id.to_string())
            } else {
                let exe = exe_of(pid);
                let id = hosted_app_id(h, &exe);
                (exe, id)
            };
            Some(WinInfo {
                hwnd: h.0 as i64,
                title,
                app_id,
                exe,
                active: h == fg,
                minimized: IsIconic(h).as_bool(),
                topmost: ex & WS_EX_TOPMOST.0 != 0,
            })
        })
        .collect()
}

/// True when the app in front covers the whole screen (games, videos, F11).
pub fn foreground_is_fullscreen(own_pid: u32) -> bool {
    fullscreen_window(own_pid).is_some()
}

/// The foreground window if it covers its whole screen (a game, or a video in fullscreen).
fn fullscreen_window(own_pid: u32) -> Option<HWND> {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() || pid_of(fg) == own_pid {
            return None;
        }
        let class = class_of(fg);
        if class == "Progman" || class == "WorkerW" {
            return None;
        }
        // A maximized window also fills the screen now that the taskbar floats; that's not
        // fullscreen. But browsers keep the "maximized" flag when a video goes fullscreen from a
        // maximized window, so what tells them apart is the frame: fullscreen drops both the
        // title bar and the resizable border. (Apps that draw their own title bar, like Discord
        // or ChatGPT, have no title bar but keep the border.)
        let style = GetWindowLongW(fg, GWL_STYLE) as u32;
        if IsZoomed(fg).as_bool() && (style & WS_CAPTION.0 == WS_CAPTION.0 || style & WS_THICKFRAME.0 != 0) {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(fg, &mut r).ok()?;
        let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return None;
        }
        // only the main screen counts: that's where the taskbar is
        if mi.dwFlags & MONITORINFOF_PRIMARY == 0 {
            return None;
        }
        let m = mi.rcMonitor;
        (r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom).then_some(fg)
    }
}

/// Apps that go fullscreen for videos rather than games.
const VIDEO_APPS: [&str; 19] = [
    "chrome.exe", "msedge.exe", "firefox.exe", "opera.exe", "brave.exe", "vivaldi.exe", "avgbrowser.exe", "arc.exe",
    "shift.exe", "zen.exe", "librewolf.exe",
    "vlc.exe", "mpc-hc64.exe", "mpc-be64.exe", "potplayermini64.exe", "video.ui.exe", "applicationframehost.exe",
    "powerpnt.exe", "mpv.exe",
];

/// The game you're playing: the exe name of a fullscreen window that isn't a browser or a
/// video player.
pub fn fullscreen_game(own_pid: u32) -> Option<String> {
    let fg = fullscreen_window(own_pid)?;
    let exe = exe_of(pid_of(fg));
    let name = exe.rsplit('\\').next().unwrap_or(&exe).to_lowercase();
    (!name.is_empty() && !VIDEO_APPS.contains(&name.as_str())).then_some(name)
}

// ---------- live window previews ----------

/// Starts drawing a live copy of window `src` inside our window `dest`.
/// Returns the preview handle and the source's size.
pub fn thumbnail(dest: isize, src: i64) -> Option<(isize, i32, i32)> {
    unsafe {
        let t = DwmRegisterThumbnail(hw(dest), hw(src as isize)).ok()?;
        let s = DwmQueryThumbnailSourceSize(t).ok()?;
        Some((t, s.cx, s.cy))
    }
}

/// Puts a live preview at this spot (physical pixels inside our window).
pub fn place_thumbnail(t: isize, l: i32, top: i32, r: i32, b: i32) {
    unsafe {
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
            rcDestination: RECT { left: l, top, right: r, bottom: b },
            opacity: 255,
            fVisible: TRUE,
            ..Default::default()
        };
        let _ = DwmUpdateThumbnailProperties(t, &props);
    }
}

/// A still picture of a window (shrunk to at most `max_w` wide), taken while it's on screen,
/// so its preview still shows something after it's minimized.
pub fn snapshot(hwnd: i64, max_w: i32) -> Option<String> {
    unsafe {
        let h = hw(hwnd as isize);
        if IsIconic(h).as_bool() || !IsWindowVisible(h).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(h, &mut r).ok()?;
        let (w, ht) = (r.right - r.left, r.bottom - r.top);
        if w < 50 || ht < 50 {
            return None;
        }
        let screen = GetDC(None);
        let full_dc = CreateCompatibleDC(Some(screen));
        let full = CreateCompatibleBitmap(screen, w, ht);
        let old = SelectObject(full_dc, HGDIOBJ(full.0));
        // 2 = PW_RENDERFULLCONTENT: also works for apps drawn by the graphics card
        let ok = windows::Win32::Storage::Xps::PrintWindow(h, full_dc, windows::Win32::Storage::Xps::PRINT_WINDOW_FLAGS(2)).as_bool();
        let scale = (max_w as f64 / w as f64).min(1.0);
        let (sw, sh) = (((w as f64) * scale) as i32, ((ht as f64) * scale) as i32);
        let small_dc = CreateCompatibleDC(Some(screen));
        let small = CreateCompatibleBitmap(screen, sw, sh);
        let old_small = SelectObject(small_dc, HGDIOBJ(small.0));
        SetStretchBltMode(small_dc, HALFTONE);
        let _ = StretchBlt(small_dc, 0, 0, sw, sh, Some(full_dc), 0, 0, w, ht, SRCCOPY);
        SelectObject(small_dc, old_small);
        SelectObject(full_dc, old);
        let pixels = if ok { bitmap_bgra(small) } else { None };
        let _ = DeleteObject(HGDIOBJ(small.0));
        let _ = DeleteObject(HGDIOBJ(full.0));
        let _ = DeleteDC(small_dc);
        let _ = DeleteDC(full_dc);
        ReleaseDC(None, screen);
        let (pw, ph, mut px) = pixels?;
        // a fully black picture means the app wouldn't draw itself; no picture is better
        if px.chunks(4).all(|c| c[0] < 8 && c[1] < 8 && c[2] < 8) {
            return None;
        }
        for c in px.chunks_mut(4) {
            c.swap(0, 2);
            c[3] = 255;
        }
        encode_png(pw, ph, &px).map(|b| format!("data:image/png;base64,{}", B64.encode(b)))
    }
}

pub fn drop_thumbnail(t: isize) {
    unsafe {
        let _ = DwmUnregisterThumbnail(t);
    }
}

pub fn cursor() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

pub fn activate(hwnd: i64, minimize_if_active: bool) {
    let h = hw(hwnd as isize);
    unsafe {
        if minimize_if_active && !IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_MINIMIZE);
            return;
        }
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        if !SetForegroundWindow(h).as_bool() {
            // Windows sometimes refuses; briefly sharing input with the window in front lets it through
            let front = GetWindowThreadProcessId(GetForegroundWindow(), None);
            let me = GetCurrentThreadId();
            let _ = AttachThreadInput(me, front, true);
            let _ = BringWindowToTop(h);
            let _ = SetForegroundWindow(h);
            let _ = AttachThreadInput(me, front, false);
        }
    }
}

pub fn close_window(hwnd: i64) {
    unsafe {
        let _ = PostMessageW(Some(hw(hwnd as isize)), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// Keeps a window above the others, or stops. Windows refuses this for apps running as
/// administrator; the menu shows the window's real state either way.
pub fn set_topmost(hwnd: i64, on: bool) {
    unsafe {
        let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
        let _ = SetWindowPos(hw(hwnd as isize), Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// Windows' own programs that keep the desktop running: ending them from a menu does more
/// harm than good, so End task leaves them alone.
const NEVER_END: [&str; 8] = ["explorer.exe", "applicationframehost.exe", "dwm.exe", "csrss.exe", "winlogon.exe", "svchost.exe", "lsass.exe", "shellexperiencehost.exe"];

/// Is this program (a path or file name) one of those?
pub fn never_end(exe: &str) -> bool {
    NEVER_END.contains(&exe.rsplit('\\').next().unwrap_or("").to_lowercase().as_str())
}

/// The process behind a window. A Store app's window belongs to a shared host, and the app
/// itself owns a window inside it.
pub fn real_pid(hwnd: i64) -> u32 {
    let h = hw(hwnd as isize);
    let pid = pid_of(h);
    if !exe_of(pid).rsplit('\\').next().unwrap_or("").eq_ignore_ascii_case("applicationframehost.exe") {
        return pid;
    }
    let mut kids: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(Some(h), Some(collect), LPARAM(&mut kids as *mut _ as isize));
    }
    kids.into_iter().map(pid_of).find(|&p| p != pid).unwrap_or(pid)
}

/// Force-closes the program a window belongs to. False when Windows wouldn't let us.
pub fn end_task(hwnd: i64) -> bool {
    let pid = real_pid(hwnd);
    if pid == 0 || pid == std::process::id() || never_end(&exe_of(pid)) {
        return false;
    }
    unsafe {
        let Ok(p) = OpenProcess(PROCESS_TERMINATE, false, pid) else { return false };
        let ok = TerminateProcess(p, 1).is_ok();
        let _ = CloseHandle(p);
        ok
    }
}

pub fn launch(target: &str) {
    let t = wide(target);
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(t.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

// ---------- icons ----------

/// The shell icon for any file, shortcut or program, as a PNG data URL. Uses the
/// 256px version when the app ships one (sharp at any scale), otherwise 48px.
///
/// The shell's icon lists hand out wrong or missing icons when used from several
/// threads at once, so every request is queued onto one worker thread.
pub fn icon_data_url(path: &str) -> Option<String> {
    type Job = (String, std::sync::mpsc::Sender<Option<String>>);
    static WORKER: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Job>>> = std::sync::OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || {
            com_init();
            for (p, reply) in rx {
                let _ = reply.send(extract_icon(&p));
            }
        });
        std::sync::Mutex::new(tx)
    });
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    worker.lock().ok()?.send((path.to_string(), reply_tx)).ok()?;
    reply_rx.recv().ok().flatten()
}

/// `path` is a real file, or `type:.ext` for the generic icon of a file type
/// (no disk access, so a folder of 1000 photos doesn't mean 1000 lookups).
fn extract_icon(path: &str) -> Option<String> {
    if path.len() > 17 && path[..17].eq_ignore_ascii_case("shell:AppsFolder\\") {
        return app_folder_icon(path);
    }
    unsafe {
        let (p, attrs, flags) = match path.strip_prefix("type:") {
            Some(ext) => (wide(&format!("file{ext}")), FILE_FLAGS_AND_ATTRIBUTES(0x80), SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES),
            None => (wide(path), FILE_FLAGS_AND_ATTRIBUTES(0), SHGFI_SYSICONINDEX),
        };
        let mut sfi = SHFILEINFOW::default();
        let ok = SHGetFileInfoW(PCWSTR(p.as_ptr()), attrs, Some(&mut sfi), size_of::<SHFILEINFOW>() as u32, flags);
        if ok == 0 {
            return None;
        }
        // Apps without a big icon get their small one centred in a 256px canvas: skip those.
        let jumbo = icon_from_list(SHIL_JUMBO as i32, sfi.iIcon).filter(|(w, h, px)| content_size(*w, *h, px) > 96);
        let (w, h, px) = jumbo.or_else(|| icon_from_list(SHIL_EXTRALARGE as i32, sfi.iIcon))?;
        let (w, h, px) = trim(w, h, px);
        encode_png(w, h, &px).map(|b| format!("data:image/png;base64,{}", B64.encode(b)))
    }
}

/// Any icon handle (also one owned by another app, like a tray icon) as a PNG data URL.
pub fn hicon_data_url(icon: HICON) -> Option<String> {
    unsafe {
        let copy = CopyIcon(icon).ok()?;
        let out = icon_rgba(copy);
        let _ = DestroyIcon(copy);
        let (w, h, px) = out?;
        encode_png(w, h, &px).map(|b| format!("data:image/png;base64,{}", B64.encode(b)))
    }
}

unsafe fn icon_from_list(size: i32, index: i32) -> Option<(u32, u32, Vec<u8>)> {
    let list: IImageList = SHGetImageList(size).ok()?;
    let icon = list.GetIcon(index, 1 /* ILD_TRANSPARENT */).ok()?;
    let out = icon_rgba(icon);
    let _ = DestroyIcon(icon);
    out
}

/// Bounds of the non-transparent pixels: (left, top, right, bottom), exclusive.
fn bounds(w: u32, h: u32, px: &[u8]) -> (u32, u32, u32, u32) {
    let (mut l, mut t, mut r, mut b) = (w, h, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if px[((y * w + x) * 4 + 3) as usize] > 8 {
                l = l.min(x);
                t = t.min(y);
                r = r.max(x + 1);
                b = b.max(y + 1);
            }
        }
    }
    if r <= l { (0, 0, w, h) } else { (l, t, r, b) }
}

fn content_size(w: u32, h: u32, px: &[u8]) -> u32 {
    let (l, t, r, b) = bounds(w, h, px);
    (r - l).max(b - t)
}

/// Crops empty transparent margins (keeping the icon square) so every icon fills its slot evenly.
fn trim(w: u32, h: u32, px: Vec<u8>) -> (u32, u32, Vec<u8>) {
    let (l, t, r, b) = bounds(w, h, &px);
    let side = (r - l).max(b - t);
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    let x0 = cx.saturating_sub(side / 2).min(w - side);
    let y0 = cy.saturating_sub(side / 2).min(h - side);
    let mut out = Vec::with_capacity((side * side * 4) as usize);
    for y in y0..y0 + side {
        let s = ((y * w + x0) * 4) as usize;
        out.extend_from_slice(&px[s..s + (side * 4) as usize]);
    }
    (side, side, out)
}

fn encode_png(w: u32, h: u32, px: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().ok()?;
        wr.write_image_data(px).ok()?;
    }
    Some(out)
}

/// Icon of an app from Windows' own app list (Store apps like Claude included).
fn app_folder_icon(path: &str) -> Option<String> {
    unsafe {
        let p = wide(path);
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(p.as_ptr()), None).ok()?;
        let bmp = factory.GetImage(SIZE { cx: 128, cy: 128 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let pixels = bitmap_bgra(bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let (w, h, mut px) = pixels?;
        for c in px.chunks_mut(4) {
            // these come with premultiplied alpha
            let a = c[3] as u32;
            if a > 0 && a < 255 {
                for v in &mut c[..3] {
                    *v = ((*v as u32 * 255) / a).min(255) as u8;
                }
            }
            c.swap(0, 2);
        }
        let (w, h, px) = trim(w, h, px);
        encode_png(w, h, &px).map(|b| format!("data:image/png;base64,{}", B64.encode(b)))
    }
}

/// A bitmap's pixels as top-down BGRA.
unsafe fn bitmap_bgra(bmp: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    let mut bm = BITMAP::default();
    GetObjectW(HGDIOBJ(bmp.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void));
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    if w <= 0 || h <= 0 {
        return None;
    }
    let mut px = vec![0u8; (w * h * 4) as usize];
    let mut bi = BITMAPINFO::default();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let dc = GetDC(None);
    let lines = GetDIBits(dc, bmp, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
    ReleaseDC(None, dc);
    if lines == 0 { None } else { Some((w as u32, h as u32, px)) }
}

unsafe fn icon_rgba(icon: HICON) -> Option<(u32, u32, Vec<u8>)> {
    let mut ii = ICONINFO::default();
    GetIconInfo(icon, &mut ii).ok()?;
    let pixels = bitmap_bgra(ii.hbmColor);
    let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
    let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
    let (w, h, mut px) = pixels?;
    let has_alpha = px.chunks(4).any(|c| c[3] != 0);
    for c in px.chunks_mut(4) {
        c.swap(0, 2);
        if !has_alpha {
            c[3] = 255;
        }
    }
    Some((w, h, px))
}

// ---------- Start menu apps ----------

#[derive(Serialize, Clone)]
pub struct AppEntry {
    pub name: String,
    /// what to launch (a shortcut, a program or a ms-settings: link)
    pub path: String,
    /// the program the shortcut points to, lowercase, used to match open windows
    pub exe: String,
}

pub fn link_target(lnk: &Path) -> String {
    unsafe {
        let Ok(link) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER) else { return String::new() };
        let Ok(file) = link.cast::<IPersistFile>() else { return String::new() };
        let p = wide(&lnk.to_string_lossy());
        if file.Load(PCWSTR(p.as_ptr()), STGM_READ).is_err() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        if link.GetPath(&mut buf, std::ptr::null_mut(), 0).is_err() {
            return String::new();
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
        String::from_utf16_lossy(&buf[..n])
    }
}

fn walk(dir: &Path, out: &mut Vec<AppEntry>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
            continue;
        }
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
        if ext != "lnk" && ext != "url" {
            continue;
        }
        let name = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let lower = name.to_lowercase();
        if ["uninstall", "readme", "help", "documentation", "website", "release notes"].iter().any(|s| lower.contains(s)) {
            continue;
        }
        let exe = if ext == "lnk" { link_target(&p).to_lowercase() } else { String::new() };
        out.push(AppEntry { name, path: p.to_string_lossy().to_string(), exe });
    }
}

/// Windows' own app list (the "Applications" folder), launched as shell:AppsFolder\<id>.
fn app_folder_apps() -> Vec<AppEntry> {
    let mut out = Vec::new();
    unsafe {
        let Ok(folder) = SHGetKnownFolderItem::<IShellItem>(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None) else { return out };
        let Ok(items) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else { return out };
        let text = |item: &IShellItem, kind: SIGDN| -> String {
            match item.GetDisplayName(kind) {
                Ok(p) => {
                    let s = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.0 as *const c_void));
                    s
                }
                Err(_) => String::new(),
            }
        };
        loop {
            let mut batch: [Option<IShellItem>; 1] = [None];
            let mut fetched = 0u32;
            if items.Next(&mut batch, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(item) = batch[0].take() else { break };
            let name = text(&item, SIGDN_NORMALDISPLAY);
            let id = text(&item, SIGDN_PARENTRELATIVEPARSING);
            let lower = name.to_lowercase();
            if name.is_empty() || id.is_empty() || id.starts_with("http") || ["uninstall", "readme", "documentation", "release notes"].iter().any(|s| lower.contains(s)) {
                continue;
            }
            out.push(AppEntry { name, path: format!("shell:AppsFolder\\{id}"), exe: String::new() });
        }
    }
    out
}

pub fn list_apps() -> Vec<AppEntry> {
    com_init();
    let mut apps = vec![
        AppEntry { name: "File Explorer".into(), path: r"C:\Windows\explorer.exe".into(), exe: r"c:\windows\explorer.exe".into() },
        AppEntry { name: "Settings".into(), path: "ms-settings:".into(), exe: String::new() },
    ];
    for var in ["ProgramData", "APPDATA"] {
        if let Ok(base) = std::env::var(var) {
            walk(&Path::new(&base).join(r"Microsoft\Windows\Start Menu\Programs"), &mut apps);
        }
    }
    // Then everything else Windows lists under All apps: Store apps (like Claude) have no
    // shortcut file, so they're only found here. Shortcuts above win for the same name,
    // because they tell us which program they start (to match open windows).
    apps.extend(app_folder_apps());
    let mut seen = std::collections::HashSet::new();
    apps.retain(|a| seen.insert(a.name.to_lowercase()));
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

// ---------- quick settings ----------

#[derive(Serialize)]
pub struct Status {
    pub volume: Option<f32>,
    pub muted: bool,
    pub battery: Option<u8>,
    pub charging: bool,
    pub wifi: Option<String>,
}

fn endpoint() -> Option<IAudioEndpointVolume> {
    com_init();
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        dev.Activate(CLSCTX_ALL, None).ok()
    }
}

pub fn set_volume(v: f32) {
    if let Some(ep) = endpoint() {
        unsafe {
            let _ = ep.SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), std::ptr::null());
        }
    }
}

pub fn set_mute(m: bool) {
    if let Some(ep) = endpoint() {
        unsafe {
            let _ = ep.SetMute(m, std::ptr::null());
        }
    }
}

fn wifi_name() -> Option<String> {
    let out = Command::new("netsh").args(["wlan", "show", "interfaces"]).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with("SSID") && !l.starts_with("SSID BSSID"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn status(with_wifi: bool) -> Status {
    let (volume, muted) = match endpoint() {
        Some(ep) => unsafe { (ep.GetMasterVolumeLevelScalar().ok(), ep.GetMute().map(|b| b.as_bool()).unwrap_or(false)) },
        None => (None, false),
    };
    let mut ps = SYSTEM_POWER_STATUS::default();
    let (battery, charging) = unsafe {
        if GetSystemPowerStatus(&mut ps).is_ok() && ps.BatteryFlag & 128 == 0 && ps.BatteryLifePercent <= 100 {
            (Some(ps.BatteryLifePercent), ps.ACLineStatus == 1)
        } else {
            (None, false)
        }
    };
    Status { volume, muted, battery, charging, wifi: if with_wifi { wifi_name() } else { None } }
}

// ---------- about this PC (for Settings) ----------

#[derive(Serialize)]
pub struct SysInfo {
    pub pc_name: String,
    pub windows: String,
    pub version: String,
    pub build: String,
    pub cpu: String,
    pub cores: u32,
    pub ram_gb: f64,
    pub ram_used: f64,
    pub uptime_min: u64,
}

fn reg_text(key: windows::Win32::System::Registry::HKEY, sub: &str, value: &str) -> String {
    use windows::Win32::System::Registry::*;
    let (s, v) = (wide(sub), wide(value));
    let mut buf = [0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    let ok = unsafe { RegGetValueW(key, PCWSTR(s.as_ptr()), PCWSTR(v.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut c_void), Some(&mut len)) };
    if ok.is_err() {
        return String::new();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    String::from_utf16_lossy(&buf[..n]).trim().to_string()
}

pub fn sys_info() -> SysInfo {
    use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE as HKLM;
    use windows::Win32::System::SystemInformation::*;
    let nt = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build = reg_text(HKLM, nt, "CurrentBuild");
    // Windows 11 still calls itself "Windows 10" in this spot; the build number tells them apart
    let mut windows = reg_text(HKLM, nt, "ProductName");
    if build.parse::<u32>().unwrap_or(0) >= 22000 {
        windows = windows.replace("Windows 10", "Windows 11");
    }
    let mut mem = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    let _ = unsafe { GlobalMemoryStatusEx(&mut mem) };
    let gb = |b: u64| (b as f64 / 1_073_741_824.0 * 10.0).round() / 10.0;
    SysInfo {
        pc_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
        windows,
        version: reg_text(HKLM, nt, "DisplayVersion"),
        build,
        cpu: reg_text(HKLM, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "ProcessorNameString"),
        cores: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(0),
        ram_gb: gb(mem.ullTotalPhys),
        ram_used: gb(mem.ullTotalPhys - mem.ullAvailPhys),
        uptime_min: unsafe { GetTickCount64() } / 60_000,
    }
}

// ---------- idle screen ----------

/// How long since the last mouse or keyboard input, in ms.
pub fn idle_ms() -> u64 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut li = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if !GetLastInputInfo(&mut li).as_bool() {
            return 0;
        }
        (windows::Win32::System::SystemInformation::GetTickCount64() as u32).wrapping_sub(li.dwTime) as u64
    }
}

/// True when an app asks to keep the screen on (a playing video, a presentation, ...).
pub fn screen_kept_on() -> bool {
    use windows::Win32::System::Power::{CallNtPowerInformation, SystemExecutionState};
    let mut state = 0u32;
    unsafe {
        let ok = CallNtPowerInformation(SystemExecutionState, None, 0, Some(&mut state as *mut _ as *mut c_void), 4);
        ok.is_ok() && state & 0x2 != 0 // ES_DISPLAY_REQUIRED
    }
}

// ---------- widgets ----------

/// Keeps a window just above the desktop: in front of the wallpaper and icons, behind every app.
pub fn stick_to_desktop(hwnd: isize) {
    unsafe {
        if let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) {
            SetWindowLongPtrW(hw(hwnd), GWLP_HWNDPARENT, progman.0 as isize);
        }
    }
}

#[derive(Serialize)]
pub struct Usage {
    pub cpu: f64,
    pub ram: f64,
    pub ram_used: f64,
    pub ram_total: f64,
}

/// CPU use since the last call, and memory use.
pub fn usage() -> Usage {
    use std::sync::Mutex;
    use windows::Win32::System::SystemInformation::*;
    use windows::Win32::System::Threading::GetSystemTimes;
    static LAST: Mutex<(u64, u64)> = Mutex::new((0, 0));
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    let mut cpu = 0.0;
    unsafe {
        if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
            let (i, total) = (ft(idle), ft(kernel) + ft(user));
            let mut last = LAST.lock().unwrap();
            let (di, dt) = (i.saturating_sub(last.0), total.saturating_sub(last.1));
            if last.1 != 0 && dt > 0 {
                cpu = (1.0 - di as f64 / dt as f64) * 100.0;
            }
            *last = (i, total);
        }
    }
    let mut mem = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    let _ = unsafe { GlobalMemoryStatusEx(&mut mem) };
    let gb = |b: u64| b as f64 / 1_073_741_824.0;
    Usage {
        cpu: cpu.clamp(0.0, 100.0),
        ram: mem.dwMemoryLoad as f64,
        ram_used: gb(mem.ullTotalPhys - mem.ullAvailPhys),
        ram_total: gb(mem.ullTotalPhys),
    }
}

/// The name Windows shows for an app ID (e.g. "Claude_xxx!Claude" -> "Claude").
pub fn app_name(app_id: &str) -> String {
    com_init();
    unsafe {
        let p = wide(&format!("shell:AppsFolder\\{app_id}"));
        let name = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(p.as_ptr()), None)
            .and_then(|item| item.GetDisplayName(SIGDN_NORMALDISPLAY))
            .map(|n| {
                let s = n.to_string().unwrap_or_default();
                CoTaskMemFree(Some(n.0 as *const c_void));
                s
            })
            .unwrap_or_default();
        if !name.is_empty() {
            return name;
        }
    }
    // fall back to a tidied-up ID
    app_id.rsplit(['!', '\\']).next().unwrap_or(app_id).trim_end_matches(".exe").to_string()
}

/// False when another Pink Cloud is already running.
pub fn first_instance() -> bool {
    unsafe {
        match CreateMutexW(None, true, w!("PinkCloud.SingleInstance")) {
            // the handle is never closed, so the mutex stays ours for as long as we run
            Ok(_) => GetLastError() != ERROR_ALREADY_EXISTS,
            Err(_) => true,
        }
    }
}

// ---------- start with Windows ----------

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const TASK: &str = "Pink Cloud";

fn in_run_key() -> bool {
    Command::new("reg").args(["query", RUN_KEY, "/v", "PinkCloud"]).creation_flags(CREATE_NO_WINDOW).output().map(|o| o.status.success()).unwrap_or(false)
}

fn task_exists() -> bool {
    Command::new("schtasks").args(["/query", "/tn", TASK]).creation_flags(CREATE_NO_WINDOW).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Whether Pink Cloud starts when you sign in.
pub fn autostart() -> bool {
    task_exists() || in_run_key()
}

/// Starting with Windows goes through a sign-in task rather than the registry's Run list:
/// Windows works through that list one app at a time, and with many apps in it Pink Cloud
/// could wait minutes, or never start when an app ahead of it hangs.
pub fn set_autostart(on: bool) -> bool {
    if !on {
        let _ = Command::new("schtasks").args(["/delete", "/tn", TASK, "/f"]).creation_flags(CREATE_NO_WINDOW).status();
        let _ = Command::new("reg").args(["delete", RUN_KEY, "/v", "PinkCloud", "/f"]).creation_flags(CREATE_NO_WINDOW).status();
        return !autostart();
    }
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let user = format!(r"{}\{}", std::env::var("USERDOMAIN").unwrap_or_default(), std::env::var("USERNAME").unwrap_or_default());
    let esc = |t: &str| t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    // normal priority (tasks default to below normal) and no time limit
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts Pink Cloud when you sign in.</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <StartWhenAvailable>true</StartWhenAvailable>
  </Settings>
  <Actions Context="Author"><Exec><Command>"{exe}"</Command></Exec></Actions>
</Task>"#,
        user = esc(&user),
        exe = esc(&exe)
    );
    let path = std::env::temp_dir().join("PinkCloud-task.xml");
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|u| u.to_le_bytes()));
    if std::fs::write(&path, bytes).is_err() {
        return false;
    }
    let made = Command::new("schtasks")
        .args(["/create", "/tn", TASK, "/xml", &path.to_string_lossy(), "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&path);
    if made {
        // the old Run entry would only start a second copy later
        let _ = Command::new("reg").args(["delete", RUN_KEY, "/v", "PinkCloud", "/f"]).creation_flags(CREATE_NO_WINDOW).status();
    }
    made
}

/// Moves an older Run-list autostart over to the sign-in task.
pub fn upgrade_autostart() {
    if in_run_key() {
        set_autostart(true);
    }
}

pub fn power(action: &str) {
    let run = |args: &[&str]| {
        let _ = Command::new("shutdown").args(args).creation_flags(CREATE_NO_WINDOW).spawn();
    };
    match action {
        "lock" => unsafe {
            let _ = LockWorkStation();
        },
        "sleep" => unsafe {
            let _ = SetSuspendState(false, false, false);
        },
        "signout" => run(&["/l"]),
        "restart" => run(&["/r", "/t", "0"]),
        "shutdown" => run(&["/s", "/t", "0"]),
        _ => {}
    }
}
