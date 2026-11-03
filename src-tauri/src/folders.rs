//! Pink Cloud Files as the folder app: folders you open anywhere (the desktop, other apps,
//! Win+E) open in Pink Cloud Files instead of Explorer.
//!
//! Three parts: folders' default "open" action points at Pink Cloud (in your account's part
//! of the registry, undone when Pink Cloud closes or the setting is switched off); a second
//! Pink Cloud started for a folder hands it to the one that's running; and Explorer windows
//! that open anyway (some apps open them directly) are closed and shown in Files instead.

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use windows::core::{w, Interface};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowserApp, ShellWindows};
use windows::Win32::UI::WindowsAndMessaging::*;

const NO_WINDOW: u32 = 0x0800_0000;
/// Whether folders open in Pink Cloud Files (Settings → Files).
pub static ON: AtomicBool = AtomicBool::new(false);
/// Explorer windows you asked for on purpose stay Explorer: any that open before this time
/// (unix ms) are left alone, and so are the windows in ALLOWED for as long as they're open.
static ALLOW_UNTIL: AtomicU64 = AtomicU64::new(0);
static ALLOWED: Mutex<Vec<isize>> = Mutex::new(Vec::new());

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Opens the normal Windows File Explorer (at a folder, or with a file selected) and lets it be.
pub fn open_explorer(path: Option<&str>, select: bool) {
    ALLOW_UNTIL.store(now_ms() + 5000, Ordering::SeqCst);
    let mut cmd = Command::new("explorer.exe");
    match path {
        Some(p) if select => cmd.arg(format!("/select,{p}")),
        Some(p) => cmd.arg(p),
        None => &mut cmd,
    };
    let _ = cmd.spawn();
}

fn reg(args: &[&str]) {
    let _ = Command::new("reg").args(args).creation_flags(NO_WINDOW).status();
}

/// Makes "open" on folders and drives start Pink Cloud Files, or puts Explorer back.
pub fn register(on: bool) {
    ON.store(on, Ordering::SeqCst);
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    for kind in ["Directory", "Drive"] {
        let shell = format!(r"HKCU\Software\Classes\{kind}\shell");
        let verb = format!(r"{shell}\PinkCloud");
        if on {
            reg(&["add", &verb, "/ve", "/d", "Open in Pink Cloud Files", "/f"]);
            reg(&["add", &verb, "/v", "Icon", "/d", &exe, "/f"]);
            reg(&["add", &format!(r"{verb}\command"), "/ve", "/d", &format!("\"{exe}\" --open \"%1\""), "/f"]);
            reg(&["add", &shell, "/ve", "/d", "PinkCloud", "/f"]);
        } else {
            reg(&["delete", &shell, "/ve", "/f"]);
            reg(&["delete", &verb, "/f"]);
        }
    }
}

fn request_file() -> std::path::PathBuf {
    std::env::temp_dir().join("PinkCloud-open.txt")
}

/// In a second Pink Cloud started for a folder: hands the folder to the running one.
pub fn forward(path: &str) {
    let _ = std::fs::write(request_file(), path);
    unsafe {
        // the running Pink Cloud may bring its window to the front
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(ASFW_ANY);
        if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, w!("PinkCloud.OpenFolder")) {
            let _ = SetEvent(ev);
            let _ = CloseHandle(ev);
        }
    }
}

/// In the running Pink Cloud: waits for folders that other Pink Clouds hand over.
pub fn listen(on_open: impl Fn(String) + Send + 'static) {
    std::thread::spawn(move || unsafe {
        let Ok(ev) = CreateEventW(None, false, false, w!("PinkCloud.OpenFolder")) else { return };
        loop {
            if WaitForSingleObject(ev, INFINITE) != WAIT_OBJECT_0 {
                break;
            }
            if let Ok(p) = std::fs::read_to_string(request_file()) {
                let _ = std::fs::remove_file(request_file());
                on_open(p.trim().to_string());
            }
        }
    });
}

/// "file:///C:/Users/me/My%20Stuff" → C:\Users\me\My Stuff
fn path_of_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file:///")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(String::from_utf8_lossy(&out).replace('/', "\\"))
}

/// Explorer windows showing a folder get closed, and the folder opens in Files instead.
/// (Explorer windows for other things, like Control Panel, are left alone.)
pub fn watch_explorer(on_open: impl Fn(String) + Send + 'static) {
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if !ON.load(Ordering::SeqCst) {
                continue;
            }
            let Ok(all) = CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL) else { continue };
            // forget Explorer windows you opened on purpose once they're closed
            ALLOWED.lock().unwrap().retain(|h| IsWindow(Some(HWND(*h as *mut _))).as_bool());
            let n = all.Count().unwrap_or(0);
            for i in 0..n {
                let Ok(item) = all.Item(&VARIANT::from(i)) else { continue };
                let Ok(app) = item.cast::<IWebBrowserApp>() else { continue };
                let Ok(h) = app.HWND() else { continue };
                let hwnd = HWND(h.0 as *mut _);
                {
                    let mut allowed = ALLOWED.lock().unwrap();
                    if allowed.contains(&(h.0 as isize)) {
                        continue;
                    }
                    if now_ms() < ALLOW_UNTIL.load(Ordering::SeqCst) {
                        allowed.push(h.0 as isize);
                        continue;
                    }
                }
                let Ok(url) = app.LocationURL() else { continue };
                let Some(path) = path_of_url(&url.to_string()) else { continue };
                let _ = ShowWindow(hwnd, SW_HIDE);
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                on_open(path);
            }
        }
    });
}
