//! Scenes: what's open and where, and how loud, saved so one click can bring it back. The screens
//! keep the scenes and their settings; this is the part that has to talk to Windows.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::{files, mixer, shell};

#[derive(Serialize, Deserialize, Clone)]
pub struct SceneWin {
    /// What tells this app's windows apart from others, also after it updates to a new folder.
    pub key: String,
    pub name: String,
    pub title: String,
    pub icon: String,
    /// What opens the app: a Start menu shortcut, a Store app link, or the program itself.
    pub path: String,
    pub rect: [i32; 4],
    pub show: u32,
    pub front: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SceneSound {
    pub file: String,
    pub name: String,
    pub volume: f32,
    pub muted: bool,
}

/// How everything is right now.
#[derive(Serialize)]
pub struct Now {
    pub wins: Vec<SceneWin>,
    pub sounds: Vec<SceneSound>,
    pub master: Option<f32>,
    pub muted: bool,
    pub wallpaper: Option<files::Wallpaper>,
}

#[derive(Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub wins: Vec<SceneWin>,
    #[serde(default)]
    pub close_others: bool,
    #[serde(default)]
    pub sounds: Vec<SceneSound>,
    pub master: Option<f32>,
    pub muted: Option<bool>,
    pub wallpaper: Option<files::Wallpaper>,
}

fn file_of(path: &str) -> String {
    path.rsplit('\\').next().unwrap_or("").to_lowercase()
}

fn key_of(w: &shell::WinInfo) -> String {
    if w.app_id.is_empty() { file_of(&w.exe) } else { w.app_id.to_lowercase() }
}

/// Windows a scene is about: Pink Cloud's own and the desktop's aren't.
fn belongs(w: &shell::WinInfo) -> bool {
    !w.exe.is_empty() && !w.app_id.starts_with("pinkcloud.") && (!w.app_id.is_empty() || !shell::never_end(&w.exe))
}

/// How to open an app. A shortcut beats the program itself: apps like Discord move to a new
/// folder with every update, and their shortcut keeps working.
fn opener(apps: &[shell::AppEntry], exe: &str, app_id: &str, name: &str) -> String {
    if !app_id.is_empty() {
        return format!("shell:AppsFolder\\{app_id}");
    }
    let (full, file) = (exe.to_lowercase(), file_of(exe));
    let stem = file.trim_end_matches(".exe");
    apps.iter()
        .find(|a| a.exe == full)
        .or_else(|| apps.iter().find(|a| !a.exe.is_empty() && file_of(&a.exe) == file))
        .or_else(|| apps.iter().find(|a| a.name.eq_ignore_ascii_case(name) || a.name.eq_ignore_ascii_case(stem)))
        .map(|a| a.path.clone())
        .unwrap_or_else(|| exe.to_string())
}

pub fn now() -> Now {
    let apps = shell::list_apps();
    let wins = shell::list_windows(std::process::id()).into_iter().filter(belongs).enumerate().map(|(i, w)| {
        let name = mixer::name_of(&w.exe);
        let (rect, show) = shell::placement(w.hwnd);
        SceneWin { key: key_of(&w), path: opener(&apps, &w.exe, &w.app_id, &name), name, title: w.title, icon: w.exe, rect, show, front: i == 0 }
    }).collect();
    let own = std::env::current_exe().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
    let sounds = mixer::apps().into_iter().filter(|a| a.key != own && !shell::never_end(&a.key))
        .map(|a| SceneSound { file: file_of(&a.key), name: a.name, volume: a.volume, muted: a.muted }).collect();
    let status = shell::status(false);
    Now { wins, sounds, master: status.volume, muted: status.muted, wallpaper: files::current_wallpaper() }
}

/// The latest scene you picked: an earlier one still busy opening apps stops when this changes.
static RUN: AtomicU32 = AtomicU32::new(0);

struct Slot {
    win: SceneWin,
    hwnd: Option<i64>,
    placed: Option<Instant>,
    settled: bool,
}

pub fn apply(app: AppHandle, plan: Plan) {
    let run = RUN.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        shell::com_init();
        let Plan { wins, close_others, mut sounds, master, muted, wallpaper } = plan;
        if let Some(v) = master {
            shell::set_volume(v);
        }
        if let Some(m) = muted {
            shell::set_mute(m);
        }
        if let Some(w) = wallpaper {
            files::set_wallpaper(&w);
        }

        let pid = std::process::id();
        let live = shell::list_windows(pid);
        if close_others {
            // the normal close request, the same as the window's X: apps can still ask to save
            for w in live.iter().filter(|w| belongs(w) && !wins.iter().any(|s| s.key == key_of(w))) {
                shell::close_window(w.hwnd);
            }
        }

        // Each app that isn't running is opened once, so it gets one window back.
        let mut launched: Vec<String> = Vec::new();
        let mut slots: Vec<Slot> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for win in wins {
            let running = live.iter().any(|w| belongs(w) && key_of(w) == win.key);
            if !running {
                if launched.contains(&win.key) || missing.contains(&win.name) {
                    continue;
                }
                let mut path = win.path.clone();
                if Path::new(&path).is_absolute() && !Path::new(&path).exists() {
                    // it moved or was removed since the scene was saved
                    let apps = shell::list_apps();
                    path = opener(&apps, &win.icon, "", &win.name);
                    if Path::new(&path).is_absolute() && !Path::new(&path).exists() {
                        missing.push(win.name.clone());
                        continue;
                    }
                }
                shell::launch(&path);
                launched.push(win.key.clone());
            }
            slots.push(Slot { win, hwnd: None, placed: None, settled: false });
        }

        let start = Instant::now();
        let mut tick = 0u32;
        let mut fronted = false;
        while start.elapsed() < Duration::from_secs(40) && RUN.load(Ordering::SeqCst) == run {
            let live: Vec<shell::WinInfo> = shell::list_windows(pid).into_iter().filter(belongs).collect();
            // a splash screen that closed: its app's real window takes the place
            for s in slots.iter_mut() {
                if s.hwnd.is_some_and(|h| !live.iter().any(|w| w.hwnd == h)) {
                    (s.hwnd, s.placed, s.settled) = (None, None, false);
                }
            }
            // the window with the same title first, so two windows of one app don't swap places
            for same_title in [true, false] {
                for i in 0..slots.len() {
                    if slots[i].hwnd.is_some() {
                        continue;
                    }
                    let found = live.iter().find(|w| {
                        key_of(w) == slots[i].win.key && (!same_title || w.title == slots[i].win.title) && !slots.iter().any(|s| s.hwnd == Some(w.hwnd))
                    }).map(|w| w.hwnd);
                    if let Some(h) = found {
                        shell::place(h, slots[i].win.rect, slots[i].win.show);
                        let fresh = launched.contains(&slots[i].win.key);
                        (slots[i].hwnd, slots[i].placed, slots[i].settled) = (Some(h), Some(Instant::now()), !fresh);
                    }
                }
            }
            // Apps that just opened often put themselves back where they were a moment later.
            for s in slots.iter_mut().filter(|s| !s.settled && s.placed.is_some_and(|t| t.elapsed() > Duration::from_millis(2500))) {
                if let Some(h) = s.hwnd {
                    shell::place(h, s.win.rect, s.win.show);
                }
                s.settled = true;
            }
            // An app can only be given its volume once it has opened its sound.
            if tick % 3 == 0 {
                sounds.retain(|s| !mixer::set_file(&s.file, s.volume, s.muted));
            }
            let all_placed = slots.iter().all(|s| s.hwnd.is_some() && s.settled);
            if all_placed && !fronted {
                fronted = true;
                // not much later: by then you've probably started doing something else
                if start.elapsed() < Duration::from_secs(15) {
                    if let Some(h) = slots.iter().find(|s| s.win.front && s.win.show != 2).and_then(|s| s.hwnd) {
                        shell::activate(h, false);
                    }
                }
            }
            if all_placed && (launched.is_empty() || start.elapsed() > Duration::from_secs(12)) && sounds.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(400));
            tick += 1;
        }

        if RUN.load(Ordering::SeqCst) != run {
            return;
        }
        missing.extend(slots.iter().filter(|s| s.hwnd.is_none()).map(|s| s.win.name.clone()));
        if !missing.is_empty() {
            let card = serde_json::json!({
                "icon": "ph-hourglass-medium",
                "title": "Some apps didn't show up",
                "body": format!("No window from {} yet. It might be starting in the tray, or it may have been moved or removed.", missing.join(", ")),
            });
            let _ = app.emit_to("toast", "card", card);
        }
    });
}
