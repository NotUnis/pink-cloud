//! Volume for each app: the same per-app levels as Windows' volume mixer, grouped by program
//! (a browser can have many audio sessions; they move together).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use serde::Serialize;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::*;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;

use crate::shell;

const SYSTEM: &str = "system";

#[derive(Serialize)]
pub struct AudioApp {
    /// What the set_ calls below take: the program's path in lowercase, or "system".
    pub key: String,
    pub name: String,
    pub icon: String,
    pub volume: f32,
    pub muted: bool,
    pub playing: bool,
}

static NAMES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// The friendly name a program gives itself ("Google Chrome", not "chrome").
fn description(exe: &str) -> Option<String> {
    let path: Vec<u16> = exe.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(path.as_ptr()), None, size, data.as_mut_ptr() as *mut c_void).ok()?;
        let (mut p, mut len) = (std::ptr::null_mut::<c_void>(), 0u32);
        if !VerQueryValueW(data.as_ptr() as *const c_void, w!("\\VarFileInfo\\Translation"), &mut p, &mut len).as_bool() || len < 4 {
            return None;
        }
        let pair = std::slice::from_raw_parts(p as *const u16, 2);
        let query: Vec<u16> = format!("\\StringFileInfo\\{:04x}{:04x}\\FileDescription", pair[0], pair[1]).encode_utf16().chain(Some(0)).collect();
        if !VerQueryValueW(data.as_ptr() as *const c_void, PCWSTR(query.as_ptr()), &mut p, &mut len).as_bool() || len == 0 {
            return None;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(p as *const u16, len as usize));
        // some programs report a length past the name's end, with leftovers after it
        let text = text.split('\0').next().unwrap_or("").trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

fn name_of(exe: &str) -> String {
    let mut names = NAMES.lock().unwrap();
    let names = names.get_or_insert_with(HashMap::new);
    names
        .entry(exe.to_lowercase())
        .or_insert_with(|| {
            description(exe).unwrap_or_else(|| {
                let stem = exe.rsplit('\\').next().unwrap_or(exe).trim_end_matches(".exe").trim_end_matches(".EXE");
                let mut c = stem.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_else(|| "App".into())
            })
        })
        .clone()
}

/// Apps that show web pages through WebView2 play their sound from a helper process:
/// this finds the app the helper belongs to (Pink Cloud's own sounds included).
fn owner_of_helper(pid: u32) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut parents = HashMap::new();
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                parents.insert(e.th32ProcessID, e.th32ParentProcessID);
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        let mut at = pid;
        for _ in 0..6 {
            at = *parents.get(&at)?;
            if !is_helper(&shell::exe_of(at)) {
                return Some(at);
            }
        }
        None
    }
}

fn is_helper(exe: &str) -> bool {
    exe.rsplit('\\').next().unwrap_or("").eq_ignore_ascii_case("msedgewebview2.exe")
}

struct Session {
    key: String,
    exe: String,
    volume: ISimpleAudioVolume,
    playing: bool,
}

/// Every open audio session on the speakers in use right now.
fn sessions() -> Vec<Session> {
    shell::com_init();
    let mut out = Vec::new();
    unsafe {
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return out };
        let Ok(dev) = en.GetDefaultAudioEndpoint(eRender, eConsole) else { return out };
        let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { return out };
        let Ok(list) = mgr.GetSessionEnumerator() else { return out };
        for i in 0..list.GetCount().unwrap_or(0) {
            let Ok(ctl) = list.GetSession(i) else { continue };
            let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
            let Ok(state) = ctl.GetState() else { continue };
            if state == AudioSessionStateExpired {
                continue;
            }
            let Ok(volume) = ctl.cast::<ISimpleAudioVolume>() else { continue };
            let playing = state == AudioSessionStateActive;
            if ctl2.IsSystemSoundsSession().0 == 0 {
                out.push(Session { key: SYSTEM.into(), exe: String::new(), volume, playing });
                continue;
            }
            let Ok(mut pid) = ctl2.GetProcessId() else { continue };
            if pid == 0 {
                continue;
            }
            let mut exe = shell::exe_of(pid);
            if is_helper(&exe) {
                if let Some(owner) = owner_of_helper(pid) {
                    pid = owner;
                    exe = shell::exe_of(pid);
                }
            }
            if exe.is_empty() {
                continue;
            }
            out.push(Session { key: exe.to_lowercase(), exe, volume, playing });
        }
    }
    out
}

/// The apps that have sound open, A to Z with Windows' own sounds last, so rows don't jump.
pub fn apps() -> Vec<AudioApp> {
    let own = std::env::current_exe().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut out: Vec<AudioApp> = Vec::new();
    for s in sessions() {
        let (v, m) = unsafe { (s.volume.GetMasterVolume().unwrap_or(1.0), s.volume.GetMute().map(|b| b.as_bool()).unwrap_or(false)) };
        if let Some(a) = out.iter_mut().find(|a| a.key == s.key) {
            a.volume = a.volume.max(v);
            a.muted = a.muted && m;
            a.playing = a.playing || s.playing;
            continue;
        }
        let name = if s.key == SYSTEM {
            "Windows sounds".to_string()
        } else if s.key == own {
            "Pink Cloud".to_string()
        } else {
            name_of(&s.exe)
        };
        out.push(AudioApp { key: s.key, name, icon: s.exe, volume: v, muted: m, playing: s.playing });
    }
    out.sort_by_key(|a| (a.key == SYSTEM, a.name.to_lowercase()));
    out
}

pub fn set_volume(key: &str, value: f32) {
    for s in sessions().iter().filter(|s| s.key == key) {
        unsafe {
            let _ = s.volume.SetMasterVolume(value.clamp(0.0, 1.0), std::ptr::null());
        }
    }
}

pub fn set_mute(key: &str, muted: bool) {
    for s in sessions().iter().filter(|s| s.key == key) {
        unsafe {
            let _ = s.volume.SetMute(muted, std::ptr::null());
        }
    }
}
