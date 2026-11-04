//! Clipboard history for quick search. Off until you switch it on. Text only, the last 30
//! items, kept in memory and never written to disk; closing Pink Cloud forgets them.
//! Anything an app marks as private (password managers do) is never kept.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;

const CF_UNICODETEXT: u32 = 13;
const MAX_ITEMS: usize = 30;
/// Longer copies aren't kept: a shortened copy would paste back wrong.
const MAX_CHARS: usize = 100_000;

#[derive(Serialize, Clone)]
pub struct Clip {
    pub text: String,
    pub at: i64,
}

static ON: AtomicBool = AtomicBool::new(false);
static LAST_SEQ: AtomicU32 = AtomicU32::new(0);
static ITEMS: Mutex<Vec<Clip>> = Mutex::new(Vec::new());
static WORKER: Once = Once::new();

pub fn set_on(on: bool) {
    if on && !ON.load(Ordering::SeqCst) {
        // starts from the next thing you copy, not whatever was already there
        LAST_SEQ.store(unsafe { GetClipboardSequenceNumber() }, Ordering::SeqCst);
    }
    ON.store(on, Ordering::SeqCst);
    if !on {
        clear();
        return;
    }
    WORKER.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_millis(700));
            if ON.load(Ordering::SeqCst) {
                check();
            }
        });
    });
}

pub fn items() -> Vec<Clip> {
    ITEMS.lock().unwrap().clone()
}

pub fn clear() {
    ITEMS.lock().unwrap().clear();
}

/// Did the clipboard change? If it did, remember the new text.
fn check() {
    let seq = unsafe { GetClipboardSequenceNumber() };
    if seq == LAST_SEQ.load(Ordering::SeqCst) {
        return;
    }
    // another app is holding the clipboard: leave the number alone and look again next time
    let Some(text) = read() else { return };
    LAST_SEQ.store(seq, Ordering::SeqCst);
    if let Some(text) = text {
        add(text);
    }
}

fn add(text: String) {
    if text.trim().is_empty() || text.chars().count() > MAX_CHARS {
        return;
    }
    let at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let mut items = ITEMS.lock().unwrap();
    items.retain(|c| c.text != text);
    items.insert(0, Clip { text, at });
    items.truncate(MAX_ITEMS);
}

/// Has an app asked for what's on the clipboard to stay out of histories? The clipboard must be open.
unsafe fn is_private() -> bool {
    let fmt = |name: PCWSTR| RegisterClipboardFormatW(name);
    if IsClipboardFormatAvailable(fmt(w!("ExcludeClipboardContentFromMonitorProcessing"))).is_ok()
        || IsClipboardFormatAvailable(fmt(w!("Clipboard Viewer Ignore"))).is_ok()
    {
        return true;
    }
    let can = fmt(w!("CanIncludeInClipboardHistory"));
    if IsClipboardFormatAvailable(can).is_ok() {
        if let Ok(h) = GetClipboardData(can) {
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u32;
            if !p.is_null() {
                let allowed = GlobalSize(g) >= 4 && *p != 0;
                let _ = GlobalUnlock(g);
                return !allowed;
            }
        }
        return true; // it says something but we can't read it: stay on the safe side
    }
    false
}

/// None: the clipboard is busy. Some(None): nothing to keep (not text, or private).
fn read() -> Option<Option<String>> {
    unsafe {
        OpenClipboard(None).ok()?;
        let mut out = None;
        if !is_private() && IsClipboardFormatAvailable(CF_UNICODETEXT).is_ok() {
            if let Ok(h) = GetClipboardData(CF_UNICODETEXT) {
                let g = HGLOBAL(h.0);
                let p = GlobalLock(g) as *const u16;
                if !p.is_null() {
                    let units = GlobalSize(g) / 2;
                    let slice = std::slice::from_raw_parts(p, units);
                    let end = slice.iter().position(|&c| c == 0).unwrap_or(units);
                    out = Some(String::from_utf16_lossy(&slice[..end]));
                    let _ = GlobalUnlock(g);
                }
            }
        }
        let _ = CloseClipboard();
        Some(out)
    }
}

/// Puts text on the clipboard, ready to paste.
pub fn copy(text: &str) -> bool {
    let data: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2) else { return false };
        let p = GlobalLock(g) as *mut u16;
        if p.is_null() {
            let _ = GlobalFree(Some(g));
            return false;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        let _ = GlobalUnlock(g);
        for _ in 0..6 {
            if OpenClipboard(None).is_ok() {
                let _ = EmptyClipboard();
                let ok = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(g.0))).is_ok();
                let _ = CloseClipboard();
                if !ok {
                    let _ = GlobalFree(Some(g));
                }
                return ok;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        let _ = GlobalFree(Some(g));
        false
    }
}

/// Developer check: `pink-cloud --clip-history-test` copies a normal and a private text,
/// and shows that only the normal one is kept. Puts back the text that was on the clipboard.
pub fn self_test() {
    let before = read().flatten();
    set_on(true);
    copy("pink cloud normal text");
    std::thread::sleep(Duration::from_millis(1000));
    let after_normal = items().iter().map(|c| c.text.clone()).collect::<Vec<_>>();
    copy("pink cloud secret text");
    unsafe {
        if OpenClipboard(None).is_ok() {
            let _ = SetClipboardData(RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")), None);
            let _ = CloseClipboard();
        }
    }
    std::thread::sleep(Duration::from_millis(1000));
    let after_private = items().iter().map(|c| c.text.clone()).collect::<Vec<_>>();
    set_on(false);
    println!("after normal copy: {after_normal:?}\nafter private copy: {after_private:?}\nafter switching off: {:?}", items().len());
    if let Some(text) = before {
        copy(&text);
    }
}
