//! Keyboard: a tap of the Windows key opens Pink Cloud's Start menu, and Alt+Tab opens
//! Pink Cloud's window switcher instead of the Windows ones. Other shortcuts that use
//! these keys (Win+E, Win+D, Alt+F4, ...) keep working.

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    /// the Windows key was tapped on its own
    WinTap,
    /// Alt+Tab pressed (back = Shift held): open the switcher, or move along if it's open
    SwitchNext { back: bool },
    /// arrow keys while the switcher is open: columns step sideways, rows up and down
    SwitchMove { dx: i32, dy: i32 },
    /// Alt released or Enter: go to the selected window
    SwitchCommit,
    /// Esc: close the switcher without switching
    SwitchCancel,
    /// Alt+Space: quick search
    Search,
    /// Alt+G: the game overlay
    Overlay,
    /// Win+E: Pink Cloud Files instead of Explorer
    Files,
}

/// Which of the extra shortcuts are switched on (Settings).
pub static SEARCH_KEY: AtomicBool = AtomicBool::new(true);
pub static OVERLAY_KEY: AtomicBool = AtomicBool::new(true);
pub static FILES_KEY: AtomicBool = AtomicBool::new(true);
const VK_E: VIRTUAL_KEY = VIRTUAL_KEY(0x45);
const VK_G: VIRTUAL_KEY = VIRTUAL_KEY(0x47);
static SWALLOW_UP: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

// Events are handled in order on another thread; the hook itself must return quickly.
static EVENTS: OnceLock<Mutex<Sender<Key>>> = OnceLock::new();
static WIN_DOWN: AtomicBool = AtomicBool::new(false);
static USED_IN_SHORTCUT: AtomicBool = AtomicBool::new(false);
static SWITCHING: AtomicBool = AtomicBool::new(false);

/// An unassigned key code. Tapping it makes Windows treat a Win press as a shortcut (so
/// its Start menu stays closed), and stops apps from opening their menu bar when Alt is
/// let go after switching.
const VK_UNUSED: u16 = 0xE8;

fn send(k: Key) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.lock().unwrap().send(k);
    }
}

fn tap_unused() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(VK_UNUSED), dwFlags: flags, ..Default::default() } },
    };
    unsafe {
        SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32);
    }
}

fn held(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let injected = k.flags.0 & LLKHF_INJECTED.0 != 0;
        let msg = wp.0 as u32;
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let vk = VIRTUAL_KEY(k.vkCode as u16);
        let alt = k.flags.0 & LLKHF_ALTDOWN.0 != 0;
        if !injected {
            // ----- Alt+Space, Alt+G, Win+E (AltGr is Ctrl+Alt, so Ctrl must be up) -----
            if !down && SWALLOW_UP.load(Ordering::SeqCst) == vk.0 {
                SWALLOW_UP.store(0, Ordering::SeqCst);
                return LRESULT(1);
            }
            if down && !SWITCHING.load(Ordering::SeqCst) {
                let plain_alt = alt && !held(VK_CONTROL) && !WIN_DOWN.load(Ordering::SeqCst);
                let pick = if plain_alt && vk == VK_SPACE && SEARCH_KEY.load(Ordering::SeqCst) {
                    Some(Key::Search)
                } else if plain_alt && vk == VK_G && OVERLAY_KEY.load(Ordering::SeqCst) {
                    Some(Key::Overlay)
                } else if WIN_DOWN.load(Ordering::SeqCst) && vk == VK_E && FILES_KEY.load(Ordering::SeqCst) {
                    USED_IN_SHORTCUT.store(true, Ordering::SeqCst);
                    Some(Key::Files)
                } else {
                    None
                };
                if let Some(k) = pick {
                    SWALLOW_UP.store(vk.0, Ordering::SeqCst);
                    // so letting go of Alt doesn't open the app's menu bar
                    if k != Key::Files {
                        tap_unused();
                    }
                    send(k);
                    return LRESULT(1);
                }
            }
        }
        // ----- Alt+Tab -----
        // Keys sent by software count here: some keyboard software and remote desktop tools
        // deliver every key that way. (Pink Cloud's own taps use a different, unassigned key.)
        {
            if vk == VK_TAB && (alt || SWITCHING.load(Ordering::SeqCst)) {
                if down {
                    SWITCHING.store(true, Ordering::SeqCst);
                    send(Key::SwitchNext { back: held(VK_SHIFT) });
                }
                return LRESULT(1);
            }
            if SWITCHING.load(Ordering::SeqCst) {
                let step = match vk {
                    VK_LEFT => Some((-1, 0)),
                    VK_RIGHT => Some((1, 0)),
                    VK_UP => Some((0, -1)),
                    VK_DOWN => Some((0, 1)),
                    _ => None,
                };
                if let Some((dx, dy)) = step {
                    if down {
                        send(Key::SwitchMove { dx, dy });
                    }
                    return LRESULT(1);
                }
                if vk == VK_ESCAPE || vk == VK_RETURN {
                    if down {
                        SWITCHING.store(false, Ordering::SeqCst);
                        tap_unused();
                        send(if vk == VK_ESCAPE { Key::SwitchCancel } else { Key::SwitchCommit });
                    }
                    return LRESULT(1);
                }
                if !down && (vk == VK_MENU || vk == VK_LMENU || vk == VK_RMENU) {
                    SWITCHING.store(false, Ordering::SeqCst);
                    tap_unused();
                    send(Key::SwitchCommit);
                }
            }
        }
        if !injected {
            // ----- Windows key -----
            if vk == VK_LWIN || vk == VK_RWIN {
                if down {
                    if !WIN_DOWN.swap(true, Ordering::SeqCst) {
                        USED_IN_SHORTCUT.store(false, Ordering::SeqCst);
                    }
                } else {
                    WIN_DOWN.store(false, Ordering::SeqCst);
                    if !USED_IN_SHORTCUT.load(Ordering::SeqCst) {
                        tap_unused();
                        send(Key::WinTap);
                    }
                }
            } else if down && WIN_DOWN.load(Ordering::SeqCst) {
                USED_IN_SHORTCUT.store(true, Ordering::SeqCst);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// Starts listening on a background thread; `on_key` gets each event, in order.
pub fn watch(on_key: impl Fn(Key) + Send + 'static) {
    let (tx, rx) = std::sync::mpsc::channel::<Key>();
    let _ = EVENTS.set(Mutex::new(tx));
    std::thread::spawn(move || {
        for k in rx {
            on_key(k);
        }
    });
    std::thread::spawn(|| unsafe {
        // Windows silently drops a keyboard hook that answers too slowly, and without this a
        // busy PC could leave that thread waiting
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        let mut current = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), None, 0).ok();
        crate::safe::log(if current.is_some() { "keyboard: listening" } else { "keyboard: could not start listening" });
        // There's no way to ask if the hook is still there, so it's renewed now and then. That
        // brings it back if Windows dropped it, and puts it ahead of other keyboard tools.
        let _ = SetTimer(None, 0, 30_000, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_TIMER && !SWITCHING.load(Ordering::SeqCst) && !WIN_DOWN.load(Ordering::SeqCst) {
                if let Ok(fresh) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), None, 0) {
                    if let Some(old) = current.replace(fresh) {
                        let _ = UnhookWindowsHookEx(old);
                    }
                }
            }
        }
        if let Some(h) = current {
            let _ = UnhookWindowsHookEx(h);
        }
    });
}
