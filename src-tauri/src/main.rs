// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod clip;
mod discord;
mod files;
mod folders;
mod keys;
mod media;
mod mixer;
mod notify;
mod procs;
mod safe;
mod shell;
mod tray;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::window::{Color, Effect, EffectsBuilder};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, RunEvent, State, WebviewWindow};

/// Taskbar height and the gap between popups and the taskbar, in logical pixels.
const BAR_HEIGHT: f64 = 56.0;
const GAP: f64 = 10.0;
/// Extra see-through room above the taskbar where the music visualizer can rise into.
/// Clicks there go straight through to whatever is underneath.
const VIS_HEIGHT: f64 = 340.0;
const POPUPS: [&str; 6] = ["start", "quick", "calendar", "tray", "notes", "search"];
/// Whether this is the very first time Pink Cloud runs on this PC (for the welcome tour).
static FIRST_RUN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Default)]
struct AppState {
    /// When each popup last closed because it lost focus. Clicking its taskbar button
    /// also takes focus away, so this stops that same click from reopening it.
    last_blur: Mutex<HashMap<String, Instant>>,
    icons: Mutex<HashMap<String, Option<String>>>,
    /// Which screen opened the right-click menu, so the pick goes back to it.
    menu_owner: Mutex<Option<String>>,
    hit_rects: Mutex<Vec<[i32; 4]>>,
    /// while you drag an app along the taskbar, the whole taskbar catches the mouse
    bar_capture: std::sync::atomic::AtomicBool,
    /// clickable parts of the other see-through layers, and whether each catches the mouse now
    layer_hits: Mutex<HashMap<String, Vec<[i32; 4]>>>,
    hide_banners: std::sync::atomic::AtomicBool,
    idle_minutes: std::sync::atomic::AtomicU64,
    /// the window you were using when the idle screen came up
    before_idle: Mutex<Option<i64>>,
    /// live window previews currently shown, and whether the mouse is on the preview popup
    thumbs: Mutex<Vec<isize>>,
    preview_hold: Mutex<bool>,
    /// last Now Playing state, for screens that open later
    now_playing: Mutex<Option<media::NowPlaying>>,
    /// a song recognized by sound, for when no app reports one
    heard: Mutex<Option<media::NowPlaying>>,
    switcher: Mutex<Switcher>,
    /// last picture of each window, for previews of minimized windows
    snaps: Mutex<HashMap<i64, String>>,
    /// the game you're playing (its exe name), while game mode is on
    game: Mutex<Option<String>>,
    /// game mode switched off in Settings
    game_off: std::sync::atomic::AtomicBool,
}

/// Keeps a recent picture of every open window, so a minimized window's preview still shows
/// what's in it (Windows stops drawing windows while they're minimized). One window at a
/// time, so it stays light: the one you're using every few seconds (and the one you just
/// left), plus the others in turn.
fn snapshot_watcher(app: AppHandle) {
    let pid = std::process::id();
    let mut taken: HashMap<i64, Instant> = HashMap::new();
    let mut last_front: Option<i64> = None;
    let mut round = 0u32;
    loop {
        std::thread::sleep(Duration::from_millis(700));
        round += 1;
        let wins = shell::list_windows(pid);
        let state = app.state::<AppState>();
        let shoot = |h: i64, taken: &mut HashMap<i64, Instant>| {
            if let Some(pic) = shell::snapshot(h, 480) {
                state.snaps.lock().unwrap().insert(h, pic);
                taken.insert(h, Instant::now());
            }
        };
        let front = wins.iter().find(|w| w.active && !w.minimized).map(|w| w.hwnd);
        if front != last_front {
            // you just switched away: picture the window you left while it's still drawn
            if let Some(prev) = last_front {
                if wins.iter().any(|w| w.hwnd == prev && !w.minimized) {
                    shoot(prev, &mut taken);
                }
            }
            last_front = front;
        }
        if let Some(f) = front {
            if taken.get(&f).map_or(true, |t| t.elapsed() > Duration::from_millis(2500)) {
                shoot(f, &mut taken);
                continue;
            }
        }
        // otherwise, refresh whichever other open window has the oldest picture
        if let Some(w) = wins
            .iter()
            .filter(|w| !w.minimized && taken.get(&w.hwnd).map_or(true, |t| t.elapsed() > Duration::from_secs(8)))
            .min_by_key(|w| taken.get(&w.hwnd).copied())
        {
            shoot(w.hwnd, &mut taken);
        }
        // forget windows that closed
        if round % 15 == 0 {
            let open: std::collections::HashSet<i64> = wins.iter().map(|w| w.hwnd).collect();
            state.snaps.lock().unwrap().retain(|h, _| open.contains(h));
            taken.retain(|h, _| open.contains(h));
        }
    }
}

/// The Alt+Tab switcher: the windows it shows, which one is picked, and its live views.
#[derive(Default)]
struct Switcher {
    wins: Vec<shell::WinInfo>,
    selected: usize,
    cols: usize,
    thumbs: Vec<isize>,
}

fn switcher_close(app: &AppHandle, sw: &mut Switcher) {
    for t in sw.thumbs.drain(..) {
        shell::drop_thumbnail(t);
    }
    sw.wins.clear();
    if let Some(w) = app.get_webview_window("switcher") {
        let _ = w.hide();
    }
}

fn switcher_key(app: &AppHandle, key: keys::Key) {
    let state = app.state::<AppState>();
    let mut sw = state.switcher.lock().unwrap();
    match key {
        keys::Key::WinTap => {
            drop(sw);
            toggle(app, "start");
        }
        keys::Key::SwitchNext { back } => {
            if sw.wins.is_empty() {
                safe::log("keyboard: Alt+Tab caught, opening the switcher");
                switcher_open(app, &mut sw, back);
            } else {
                let n = sw.wins.len();
                sw.selected = if back { (sw.selected + n - 1) % n } else { (sw.selected + 1) % n };
                let _ = app.emit_to("switcher", "switcher-select", sw.selected);
            }
        }
        keys::Key::SwitchMove { dx, dy } => {
            let n = sw.wins.len() as i32;
            if n > 0 {
                let step = dx + dy * sw.cols as i32;
                sw.selected = (sw.selected as i32 + step).rem_euclid(n) as usize;
                let _ = app.emit_to("switcher", "switcher-select", sw.selected);
            }
        }
        keys::Key::SwitchCommit => {
            if let Some(w) = sw.wins.get(sw.selected) {
                shell::activate(w.hwnd, false);
            }
            switcher_close(app, &mut sw);
        }
        keys::Key::SwitchCancel => switcher_close(app, &mut sw),
        keys::Key::Search => {
            drop(sw);
            toggle(app, "search");
        }
        keys::Key::Overlay => {
            drop(sw);
            toggle_overlay(app.clone());
        }
        keys::Key::Files => {
            drop(sw);
            open_files(app.clone(), None);
        }
        keys::Key::Tap => {}
    }
}

/// Shows every open window as a live view in a big panel in the middle of the screen.
fn switcher_open(app: &AppHandle, sw: &mut Switcher, back: bool) {
    let Some(win) = app.get_webview_window("switcher") else { return };
    let wins: Vec<_> = shell::list_windows(std::process::id()).into_iter().take(18).collect();
    if wins.is_empty() {
        return;
    }
    const CARD_W: f64 = 250.0;
    const CARD_H: f64 = 186.0;
    const PAD: f64 = 14.0;
    const TITLE: f64 = 42.0; // the title bar, like Pink Cloud's other windows
    const TOP: f64 = 36.0;
    const AREA_W: f64 = 230.0;
    const AREA_H: f64 = 136.0;
    let (x, y, w, h, sf) = screen(&win);
    let max_cols = (((w as f64 / sf) * 0.86 - PAD * 2.0) / CARD_W).floor().max(1.0) as usize;
    let cols = wins.len().min(max_cols);
    let rows = (wins.len() + cols - 1) / cols;
    // cards are 8 narrower and shorter than their slot, which leaves the gap between them
    let (ww, wh) = (cols as f64 * CARD_W + PAD * 2.0 - 8.0, TITLE + rows as f64 * CARD_H + PAD - 8.0);
    let _ = win.set_size(PhysicalSize::new((ww * sf) as u32, (wh * sf) as u32));
    let _ = win.set_position(PhysicalPosition::new(x + (w - (ww * sf) as i32) / 2, y + (h - (wh * sf) as i32) / 2));
    let _ = win.show();

    let mut cards = Vec::new();
    for (i, wi) in wins.iter().enumerate() {
        let (cx, cy) = (PAD + (i % cols) as f64 * CARD_W, TITLE + (i / cols) as f64 * CARD_H);
        let mut live = false;
        if !wi.minimized {
            if let Some((t, sw_, sh)) = shell::thumbnail(hwnd(&win), wi.hwnd) {
                let scale = (AREA_W / sw_.max(1) as f64).min(AREA_H / sh.max(1) as f64) * sf;
                let (tw, th) = ((sw_ as f64 * scale) as i32, (sh as f64 * scale) as i32);
                let ax = ((cx + 6.0) * sf) as i32 + ((AREA_W * sf) as i32 - tw) / 2;
                let ay = ((cy + TOP) * sf) as i32 + ((AREA_H * sf) as i32 - th) / 2;
                shell::place_thumbnail(t, ax, ay, ax + tw, ay + th);
                sw.thumbs.push(t);
                live = true;
            }
        }
        let snap = if live { None } else { app.state::<AppState>().snaps.lock().unwrap().get(&wi.hwnd).cloned() };
        cards.push(serde_json::json!({ "hwnd": wi.hwnd, "title": wi.title, "exe": wi.exe, "app_id": wi.app_id, "x": cx, "y": cy, "live": live, "snap": snap }));
    }
    let n = wins.len();
    sw.selected = if back { n - 1 } else { 1 % n };
    sw.cols = cols;
    sw.wins = wins;
    let _ = app.emit_to(
        "switcher",
        "switcher",
        serde_json::json!({ "cards": cards, "w": CARD_W - 8.0, "h": CARD_H - 8.0, "selected": sw.selected }),
    );
}

/// Clicking a window in the switcher picks it.
#[tauri::command]
fn switcher_pick(app: AppHandle, index: usize) {
    let state = app.state::<AppState>();
    let mut sw = state.switcher.lock().unwrap();
    if let Some(w) = sw.wins.get(index) {
        shell::activate(w.hwnd, false);
    }
    switcher_close(&app, &mut sw);
}

fn visible(app: &AppHandle, label: &str) -> bool {
    app.get_webview_window(label).and_then(|w| w.is_visible().ok()).unwrap_or(false)
}

fn hwnd(w: &WebviewWindow) -> isize {
    w.hwnd().map(|h| h.0 as isize).unwrap_or(0)
}

/// Primary monitor: x, y, width, height (physical pixels) and scale.
fn screen(w: &WebviewWindow) -> (i32, i32, i32, i32, f64) {
    match w.primary_monitor().ok().flatten() {
        Some(m) => (m.position().x, m.position().y, m.size().width as i32, m.size().height as i32, m.scale_factor()),
        None => (0, 0, 1920, 1080, 1.0),
    }
}

/// Preview mode (`--preview` or PINK_CLOUD_PREVIEW=1): runs next to the normal
/// Windows taskbar instead of replacing it. Nothing about Windows is changed.
fn preview() -> bool {
    std::env::args().any(|a| a == "--preview") || std::env::var("PINK_CLOUD_PREVIEW").is_ok()
}

/// Height of whatever sits below our taskbar: the Windows taskbar in preview mode, nothing otherwise.
fn bottom_offset(sf: f64) -> i32 {
    if preview() { (48.0 * sf).round() as i32 } else { 0 }
}

/// The taskbar window (bar plus visualizer room above it): left, top, right, bottom (physical pixels).
fn bar_rect(bar: &WebviewWindow) -> (i32, i32, i32, i32) {
    let (x, y, w, h, sf) = screen(bar);
    let bottom = y + h - bottom_offset(sf);
    (x, bottom - ((BAR_HEIGHT + VIS_HEIGHT) * sf).round() as i32, x + w, bottom)
}

/// The taskbar floats over windows instead of reserving space, so maximized
/// windows use the full screen height.
fn place_taskbar(app: &AppHandle) {
    let Some(bar) = app.get_webview_window("taskbar") else { return };
    let (l, t, r, b) = bar_rect(&bar);
    let _ = bar.set_size(PhysicalSize::new((r - l) as u32, (b - t) as u32));
    let _ = bar.set_position(PhysicalPosition::new(l, t));
}

/// Fits the full-screen see-through layers (lyrics, pop-ups, widgets) to the screen again.
fn fit_layers(app: &AppHandle) {
    for label in LAYERS.iter().chain(&["lyrics"]) {
        if let Some(w) = app.get_webview_window(label) {
            let (x, y, sw, sh, sf) = screen(&w);
            let lh = sh - bottom_offset(sf) - (BAR_HEIGHT * sf) as i32;
            let _ = w.set_size(PhysicalSize::new(sw as u32, lh as u32));
            let _ = w.set_position(PhysicalPosition::new(x, y));
            shell::no_activate(hwnd(&w));
        }
    }
}

/// Resolution, display scale or monitors changed (a game can do this too), or Windows moved the
/// bar: everything is fitted to the screen again, instead of staying as it was at startup.
fn screen_watcher(app: AppHandle) {
    let mut last = None;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let Some(bar) = app.get_webview_window("taskbar") else { continue };
        let (x, y, w, h, sf) = screen(&bar);
        let now = (x, y, w, h, (sf * 100.0).round() as i32);
        let (l, t, r, b) = bar_rect(&bar);
        // a hidden bar (a fullscreen game) is left alone until it comes back
        let bar_off = bar.is_visible().unwrap_or(false)
            && (bar.outer_position().map(|p| (p.x, p.y)).ok() != Some((l, t)) || bar.outer_size().map(|s| (s.width as i32, s.height as i32)).ok() != Some((r - l, b - t)));
        if bar_off || last.is_some_and(|was| was != now) {
            safe::log(&format!("screen: fitting to {w}x{h} at {:.0}%", sf * 100.0));
            place_taskbar(&app);
            fit_layers(&app);
        }
        last = Some(now);
    }
}

fn cleanup(app: &AppHandle) {
    safe::clean_exit();
    if preview() {
        return;
    }
    if let Some(bar) = app.get_webview_window("taskbar") {
        // earlier versions reserved screen space; make sure that's released
        shell::unregister_appbar(hwnd(&bar));
    }
    tray::stop();
    shell::restore_taskbar();
    // folders open in Explorer again while Pink Cloud isn't running
    if folders::ON.load(std::sync::atomic::Ordering::SeqCst) {
        folders::register(false);
    }
}

#[derive(serde::Deserialize)]
struct MenuItem {
    #[serde(default)]
    sep: bool,
    #[serde(default)]
    text: String,
}

/// Opens the pink right-click menu at the mouse. `items` is passed straight to menu.html;
/// the choice comes back to the caller as a "menu-pick" event.
#[tauri::command]
fn show_menu(app: AppHandle, state: State<AppState>, window: WebviewWindow, items: serde_json::Value) {
    let Some(menu) = app.get_webview_window("menu") else { return };
    let parsed: Vec<MenuItem> = serde_json::from_value(items.clone()).unwrap_or_default();
    let seps = parsed.iter().filter(|i| i.sep).count() as f64;
    let rows = parsed.len() as f64 - seps;
    let (x, y, w, h, sf) = screen(&menu);
    // wide enough for the longest item (window titles can be long), within reason
    let longest = parsed.iter().map(|i| i.text.chars().count()).max().unwrap_or(0) as f64;
    let width = (60.0 + longest * 7.2).clamp(236.0, 380.0);
    let (mw, mh) = ((width * sf) as i32, ((12.0 + rows * 38.0 + seps * 9.0) * sf) as i32);
    let (cx, cy) = shell::cursor();
    // open upward when there's no room below (always the case on the taskbar)
    let up = cy + mh > y + h - (BAR_HEIGHT * sf) as i32;
    let px = cx.min(x + w - mw - 4).max(x + 4);
    let py = if up { (cy - mh).max(y + 4) } else { cy };
    let _ = menu.set_size(PhysicalSize::new(mw as u32, mh as u32));
    let _ = menu.set_position(PhysicalPosition::new(px, py));
    *state.menu_owner.lock().unwrap() = Some(window.label().to_string());
    let _ = app.emit_to("menu", "menu-items", serde_json::json!({ "items": items, "up": up }));
    let _ = menu.show();
    let _ = menu.set_focus();
}

/// The menu closed: `id` is the chosen item, or None when it was dismissed by clicking elsewhere.
#[tauri::command]
fn menu_pick(app: AppHandle, state: State<AppState>, id: Option<String>) {
    if let Some(menu) = app.get_webview_window("menu") {
        let _ = menu.hide();
    }
    let Some(owner) = state.menu_owner.lock().unwrap().take() else { return };
    let _ = app.emit_to(owner.as_str(), "menu-pick", id.clone());
    if let Some(w) = app.get_webview_window(&owner) {
        if POPUPS.contains(&owner.as_str()) {
            // picked: hand focus back to the popup; clicked away: close it too
            if id.is_some() {
                let _ = w.set_focus();
            } else {
                let _ = w.hide();
            }
        }
    }
}

/// Keeps the taskbar always there, but lets clicks on its see-through parts fall through to
/// the window underneath: it only catches the mouse while the pointer is over one of its
/// buttons (the screen reports where those are). Steps aside for fullscreen games and videos.
fn hit_watcher(app: AppHandle) {
    let pid = std::process::id();
    // Tauri answers questions about a window on the main thread, which can be busy for a while,
    // and until now this loop waited for those answers: a click arriving meanwhile fell through
    // to the window underneath. So everything here asks Windows directly, using these handles.
    let bar = loop {
        match app.get_webview_window("taskbar") {
            Some(w) => break w,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let bar_h = hwnd(&bar);
    let popups: Vec<isize> = POPUPS.iter().chain(&["menu"]).filter_map(|l| app.get_webview_window(l)).map(|w| hwnd(&w)).collect();
    let layers: Vec<(&str, isize)> = LAYERS.iter().filter_map(|l| app.get_webview_window(l).map(|w| (*l, hwnd(&w)))).collect();
    // game mode has its own loop, so it can't hold this one up either
    let handle = app.clone();
    std::thread::spawn(move || loop {
        check_game(&handle);
        std::thread::sleep(Duration::from_millis(480));
    });
    let mut hidden = false;
    loop {
        std::thread::sleep(Duration::from_millis(30));

        let fullscreen = shell::foreground_is_fullscreen(pid) && !popups.iter().any(|h| shell::is_visible(*h));
        if fullscreen != hidden {
            hidden = fullscreen;
            let _ = if hidden { bar.hide() } else { bar.show() };
        }
        if hidden {
            continue;
        }

        let (l, t) = shell::window_pos(bar_h);
        let (cx, cy) = shell::cursor();
        let (x, y) = (cx - l, cy - t);
        let state = app.state::<AppState>();
        let over_button = state.bar_capture.load(std::sync::atomic::Ordering::SeqCst)
            || state.hit_rects.lock().unwrap().iter().any(|r| x >= r[0] && x < r[2] && y >= r[1] && y < r[3]);
        shell::set_click_through(bar_h, !over_button);

        // the notification pop-ups and the desktop widgets work the same way: they only catch
        // the mouse over a pop-up or a widget, clicks everywhere else go through
        for (label, h) in &layers {
            if !shell::is_visible(*h) {
                continue;
            }
            let (lx, ly) = shell::window_pos(*h);
            let (x, y) = (cx - lx, cy - ly);
            let over = state.layer_hits.lock().unwrap().get(*label).is_some_and(|rs| rs.iter().any(|r| x >= r[0] && x < r[2] && y >= r[1] && y < r[3]));
            shell::set_click_through(*h, !over);
        }
    }
}

/// Game mode turns on while a game fills the screen (videos in a browser don't count) and
/// every screen hears about it: pop-ups wait, lyrics get small, the overlay is ready.
fn check_game(app: &AppHandle) {
    let state = app.state::<AppState>();
    let off = state.game_off.load(std::sync::atomic::Ordering::SeqCst);
    // while the overlay is up, the game isn't in front, but you're still playing
    if visible(app, "overlay") {
        return;
    }
    let now = if off { None } else { shell::fullscreen_game(std::process::id()) };
    let mut game = state.game.lock().unwrap();
    if *game != now {
        *game = now.clone();
        drop(game);
        let msg = serde_json::json!({ "on": now.is_some(), "app": now.unwrap_or_default() });
        for target in ["taskbar", "toast", "lyrics", "widgets", "overlay", "quick", "settings"] {
            let _ = app.emit_to(target, "game-mode", msg.clone());
        }
    }
}

#[tauri::command]
async fn wallpaper_key() -> Option<String> {
    files::wallpaper_key()
}

#[tauri::command]
async fn wallpaper_data() -> Option<String> {
    files::wallpaper_data()
}

#[tauri::command]
fn game_state(state: State<AppState>) -> Option<String> {
    state.game.lock().unwrap().clone()
}

#[tauri::command]
fn set_game_off(state: State<AppState>, off: bool) {
    state.game_off.store(off, std::sync::atomic::Ordering::SeqCst);
}

/// Which extra shortcuts are on: Alt+Space (search), Alt+G (game overlay), Win+E (Files).
#[tauri::command]
fn set_shortcuts(search: bool, overlay: bool, files: bool, win_tab: bool) {
    use std::sync::atomic::Ordering::SeqCst;
    keys::SEARCH_KEY.store(search, SeqCst);
    keys::OVERLAY_KEY.store(overlay, SeqCst);
    keys::FILES_KEY.store(files, SeqCst);
    keys::WIN_TAB_KEY.store(win_tab, SeqCst);
    // Pink Cloud Files as the folder app (not in preview mode, which changes nothing)
    if !preview() && folders::ON.load(SeqCst) != files {
        std::thread::spawn(move || folders::register(files));
    }
}

/// The game overlay (Alt+G): a little card in the top right corner with the time, your song
/// and volume. It never takes focus, so the game keeps your keyboard and mouse.
#[tauri::command]
fn toggle_overlay(app: AppHandle) {
    let Some(win) = on_demand(&app, "overlay") else { return };
    if win.is_visible().unwrap_or(false) {
        let _ = app.emit_to("overlay", "overlay-close", ());
        return;
    }
    let (x, y, w, _, sf) = screen(&win);
    let size = win.outer_size().unwrap_or(PhysicalSize::new(400, 300));
    let _ = win.set_position(PhysicalPosition::new(x + w - size.width as i32 - (18.0 * sf) as i32, y + (18.0 * sf) as i32));
    shell::round_corners(hwnd(&win));
    let _ = win.show();
    shell::no_activate(hwnd(&win));
    let _ = app.emit_to("overlay", "opened", ());
}

/// Quick search grows and shrinks with its results; its top stays put.
#[tauri::command]
fn fit_search(window: WebviewWindow, height: f64) {
    let (_, _, _, _, sf) = screen(&window);
    let w = window.outer_size().map(|s| s.width).unwrap_or(680);
    let _ = window.set_size(PhysicalSize::new(w, (height * sf) as u32));
}

/// See-through layers that catch the mouse only over their visible parts.
const LAYERS: [&str; 2] = ["toast", "widgets"];

#[tauri::command]
fn set_bar_capture(state: State<AppState>, on: bool) {
    state.bar_capture.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// The clickable parts of a see-through layer (pop-ups, widgets), relative to its window.
#[tauri::command]
fn set_layer_hits(state: State<AppState>, window: WebviewWindow, rects: Vec<[i32; 4]>) {
    state.layer_hits.lock().unwrap().insert(window.label().to_string(), rects);
}

// ---------- notifications ----------

#[tauri::command]
async fn notes_recent() -> Vec<notify::Note> {
    notify::recent(40)
}

#[tauri::command]
async fn app_name(app: String) -> String {
    shell::app_name(&app)
}

/// Opt-in: hide Windows' own pop-ups for apps whose notifications Pink Cloud shows.
#[tauri::command]
async fn set_hide_banners(state: State<'_, AppState>, on: bool) -> Result<(), ()> {
    state.hide_banners.store(on, std::sync::atomic::Ordering::SeqCst);
    if on {
        let mut apps: Vec<String> = notify::recent(80).into_iter().map(|n| n.app).collect();
        apps.sort();
        apps.dedup();
        for a in apps {
            notify::hide_banner(&a);
        }
    } else {
        notify::restore_banners();
    }
    Ok(())
}

// ---------- idle screen ----------

/// Shows the idle screen over everything (also from the Start menu's power panel).
#[tauri::command]
fn show_idle(app: AppHandle) {
    let Some(w) = on_demand(&app, "idle") else { return };
    if w.is_visible().unwrap_or(false) {
        return;
    }
    let state = app.state::<AppState>();
    *state.before_idle.lock().unwrap() = shell::list_windows(std::process::id()).into_iter().find(|x| x.active).map(|x| x.hwnd);
    let (x, y, sw, sh, _) = screen(&w);
    let _ = w.set_size(PhysicalSize::new(sw as u32, sh as u32));
    let _ = w.set_position(PhysicalPosition::new(x, y));
    let _ = w.show();
    let _ = w.set_focus();
    let _ = app.emit_to("idle", "idle-show", ());
}

/// The idle screen goes away (any key or mouse movement); the window you were using comes back.
#[tauri::command]
fn hide_idle(app: AppHandle) {
    if let Some(w) = app.get_webview_window("idle") {
        let _ = w.hide();
        // it's only needed while you're away, so its memory is given back
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let _ = w.destroy();
        });
    }
    if let Some(h) = app.state::<AppState>().before_idle.lock().unwrap().take() {
        shell::activate(h, false);
    }
}

/// Minutes without input before the idle screen shows (0 = never).
#[tauri::command]
fn set_idle_minutes(state: State<AppState>, minutes: u64) {
    state.idle_minutes.store(minutes, std::sync::atomic::Ordering::SeqCst);
}

fn idle_watcher(app: AppHandle) {
    let pid = std::process::id();
    loop {
        std::thread::sleep(Duration::from_millis(1500));
        let minutes = app.state::<AppState>().idle_minutes.load(std::sync::atomic::Ordering::SeqCst);
        if minutes == 0 || shell::idle_ms() < minutes * 60_000 {
            continue;
        }
        // not over a video, a presentation or a fullscreen game
        let showing = visible(&app, "idle");
        if !showing && !shell::screen_kept_on() && !shell::foreground_is_fullscreen(pid) {
            show_idle(app.clone());
        }
    }
}

// ---------- updates ----------

/// A newer version is on GitHub: tell the pop-ups and Settings.
#[tauri::command]
fn announce_update(app: AppHandle, version: String, notes: String) {
    let _ = app.emit_to("toast", "update-available", serde_json::json!({ "version": version, "notes": notes }));
}

/// Downloads the new installer and hands over to it: it closes Pink Cloud if it's still
/// open, installs quietly and starts Pink Cloud again (/R). Only ever downloads from Pink
/// Cloud's own GitHub page.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const URL: &str = "https://github.com/NotUnis/pink-cloud/raw/main/download/Pink-Cloud-Setup.exe";
    let temp = std::env::temp_dir();
    let setup = temp.join("PinkCloud-Update.exe");
    let ok = std::process::Command::new("curl.exe")
        .args(["-L", "--fail", "-s", "-o"])
        .arg(&setup)
        .arg(URL)
        .creation_flags(0x0800_0000)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || std::fs::metadata(&setup).map(|m| m.len() < 1_000_000).unwrap_or(true) {
        return Err("Couldn't download the update. Check your internet and try again.".into());
    }
    // (Older versions waited in a hidden cmd script, which stalls when started detached:
    // Pink Cloud closed and the update never ran.)
    let _ = std::fs::remove_file(temp.join("PinkCloud-Update.cmd"));
    // keep this version in case the new one doesn't start, and tell the crash helper this is on purpose
    safe::keep_previous(&app.package_info().version.to_string());
    safe::clean_exit();
    std::process::Command::new(&setup)
        .args(["/S", "/R", "/UPDATE"])
        .spawn()
        .map_err(|e| format!("Couldn't start the update: {e}"))?;
    cleanup(&app);
    app.exit(0);
    Ok(())
}

#[tauri::command]
async fn sys_usage() -> shell::Usage {
    shell::usage()
}

/// The taskbar's clickable areas, in physical pixels relative to the bar's top-left corner.
#[tauri::command]
fn set_hit_rects(state: State<AppState>, rects: Vec<[i32; 4]>) {
    *state.hit_rects.lock().unwrap() = rects;
}

#[tauri::command]
fn toggle_popup(app: AppHandle, name: String) {
    toggle(&app, &name);
}

fn toggle(app: &AppHandle, name: &str) {
    let state = app.state::<AppState>();
    let name = name.to_string();
    let Some(win) = app.get_webview_window(&name) else { return };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
        return;
    }
    if let Some(t) = state.last_blur.lock().unwrap().get(&name) {
        if t.elapsed().as_millis() < 300 {
            return;
        }
    }
    for other in POPUPS.iter().filter(|p| **p != name) {
        if let Some(o) = app.get_webview_window(other) {
            let _ = o.hide();
        }
    }
    let (x, y, w, h, sf) = screen(&win);
    if name == "tray" {
        // sized to fit the icons: up to 5 per row
        let n = tray::icons().len().max(1) as f64;
        let cols = n.min(5.0);
        let rows = (n / 5.0).ceil();
        let _ = win.set_size(PhysicalSize::new(((cols * 52.0 + 20.0) * sf) as u32, ((rows * 52.0 + 20.0) * sf) as u32));
    }
    let size = win.outer_size().unwrap_or(PhysicalSize::new(400, 400));
    let (bh, gap) = ((BAR_HEIGHT * sf) as i32, (GAP * sf) as i32);
    let px = match name.as_str() {
        "start" | "search" => x + (w - size.width as i32) / 2,
        // the tray opens right above the ^ button
        "tray" => (shell::cursor().0 - size.width as i32 / 2).clamp(x + gap, x + w - size.width as i32 - gap),
        _ => x + w - size.width as i32 - gap,
    };
    let py = if name == "search" { y + h / 5 } else { y + h - bottom_offset(sf) - bh - gap - size.height as i32 };
    let _ = win.set_position(PhysicalPosition::new(px, py));
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit_to(name.as_str(), "opened", ());
}

#[tauri::command]
fn popup_blurred(app: AppHandle, state: State<AppState>, name: String) {
    // focus moved to a menu this popup opened: stay open underneath it
    if visible(&app, "menu") {
        return;
    }
    if let Some(win) = app.get_webview_window(&name) {
        let _ = win.hide();
    }
    state.last_blur.lock().unwrap().insert(name, Instant::now());
}

/// Called by the taskbar about once a second: the open windows, and keeps the
/// Windows taskbar hidden if Explorer restarted.
// Not on the thread that runs every window: a slow answer here would freeze all the buttons.
#[tauri::command]
async fn get_windows() -> Vec<shell::WinInfo> {
    if !preview() {
        shell::keep_windows_taskbar_hidden();
    }
    shell::list_windows(std::process::id())
}

#[tauri::command]
fn activate_window(hwnd: i64, minimize_if_active: bool) {
    shell::activate(hwnd, minimize_if_active);
}

#[tauri::command]
fn close_window(hwnd: i64) {
    shell::close_window(hwnd);
}

#[tauri::command]
fn end_task(hwnd: i64) -> bool {
    shell::end_task(hwnd)
}

#[tauri::command]
fn set_topmost(hwnd: i64, on: bool) {
    shell::set_topmost(hwnd, on);
}

// ---------- Task Manager ----------

#[tauri::command]
async fn list_tasks() -> procs::Tasks {
    procs::list()
}

#[tauri::command]
async fn end_app(key: String) -> bool {
    procs::end_app(&key)
}

/// Brings up Pink Cloud's Task Manager.
#[tauri::command]
fn open_tasks(app: AppHandle) {
    let Some(win) = on_demand(&app, "tasks") else { return };
    if win.is_minimized().unwrap_or(false) {
        let _ = win.unminimize();
    }
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit_to("tasks", "tasks-open", ());
}

// ---------- volume for each app ----------

#[tauri::command]
async fn audio_apps() -> Vec<mixer::AudioApp> {
    mixer::apps()
}

#[tauri::command]
async fn set_app_volume(key: String, value: f32) {
    mixer::set_volume(&key, value);
}

#[tauri::command]
async fn set_app_mute(key: String, muted: bool) {
    mixer::set_mute(&key, muted);
}

// ---------- clipboard history ----------

#[tauri::command]
fn clip_set_on(on: bool) {
    clip::set_on(on);
}

#[tauri::command]
fn clip_history() -> Vec<clip::Clip> {
    clip::items()
}

#[tauri::command]
fn clip_copy(text: String) -> bool {
    clip::copy(&text)
}

#[tauri::command]
fn clip_clear() {
    clip::clear();
}

/// Opens an app or file. Windows can take seconds over this (a slow disk, an antivirus check, a
/// prompt), so it gets its own thread, and every button keeps working meanwhile. The thread
/// stays a little afterwards because Windows may still be finishing up on it.
#[tauri::command]
fn launch(path: String) {
    std::thread::spawn(move || {
        shell::com_init();
        shell::launch(&path);
        std::thread::sleep(Duration::from_secs(10));
    });
}

#[tauri::command]
async fn get_icon(state: State<'_, AppState>, path: String) -> Result<Option<String>, ()> {
    let key = path.to_lowercase();
    if let Some(hit) = state.icons.lock().unwrap().get(&key) {
        return Ok(hit.clone());
    }
    let icon = shell::icon_data_url(&path);
    state.icons.lock().unwrap().insert(key, icon.clone());
    Ok(icon)
}

#[tauri::command]
async fn list_apps() -> Vec<shell::AppEntry> {
    shell::list_apps()
}

#[tauri::command]
async fn get_status(with_wifi: bool) -> shell::Status {
    shell::status(with_wifi)
}

#[tauri::command]
fn set_volume(value: f32) {
    shell::set_volume(value);
}

#[tauri::command]
fn set_mute(muted: bool) {
    shell::set_mute(muted);
}

/// Script errors from the screens end up in the console, so they're easy to spot while developing.
#[tauri::command]
fn js_log(window: WebviewWindow, msg: String) {
    eprintln!("[{}] {}", window.label(), msg);
    safe::log(&format!("[{}] {}", window.label(), msg.chars().take(400).collect::<String>()));
}

// ---------- help: bug reports, crashes, the welcome tour, pop-up cards ----------

/// The technical part of a bug report: versions, the PC, and the last log lines.
#[tauri::command]
async fn report_details(app: AppHandle) -> String {
    let i = shell::sys_info();
    format!(
        "Pink Cloud {} | Windows {} {} (build {}) | {} ({} threads) | {} GB RAM | on for {} min

Recent log:
{}",
        app.package_info().version,
        i.windows,
        i.version,
        i.build,
        i.cpu,
        i.cores,
        i.ram_gb,
        i.uptime_min,
        safe::recent_log(30)
    )
}

/// A crash report left by the last run, if it hasn't been sent or dismissed yet.
#[tauri::command]
fn crash_report() -> Option<String> {
    std::fs::read_to_string(safe::crash_file()).ok()
}

#[tauri::command]
fn dismiss_crash() {
    let _ = std::fs::remove_file(safe::crash_file());
}

/// An update you went back from after it didn't work; it isn't offered again.
#[tauri::command]
fn skipped_version() -> Option<String> {
    safe::skipped_version()
}

#[tauri::command]
fn first_run() -> bool {
    FIRST_RUN.load(std::sync::atomic::Ordering::SeqCst)
}

#[tauri::command]
fn open_tour(app: AppHandle) {
    let Some(win) = on_demand(&app, "tour") else { return };
    let _ = win.center();
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit_to("tour", "tour-open", ());
}

/// A pop-up card in the corner (what's new, achievements, focus finished...), from any screen.
#[tauri::command]
fn show_card(app: AppHandle, card: serde_json::Value) {
    let _ = app.emit_to("toast", "card", card);
}

// ---------- Discord status ----------

#[tauri::command]
fn discord_available() -> bool {
    discord::available()
}

#[tauri::command]
fn set_discord(state: State<AppState>, on: bool) {
    discord::set_on(on);
    discord::tell(combine(state.now_playing.lock().unwrap().clone(), state.heard.lock().unwrap().clone()));
}

#[tauri::command]
fn user_name() -> String {
    std::env::var("USERNAME").unwrap_or_default()
}

#[tauri::command]
fn power(action: String) {
    shell::power(&action);
}

/// Retints the frosted glass behind popups and windows to the color theme (and light/dark).
/// Every screen asks when it loads, so the same tint twice in a row is skipped.
#[tauri::command]
fn set_tint(app: AppHandle, rgba: [u8; 4]) {
    {
        let mut last = TINT.lock().unwrap();
        if *last == Some(rgba) {
            return;
        }
        *last = Some(rgba);
    }
    let color = Color(rgba[0], rgba[1], rgba[2], rgba[3]);
    for p in POPUPS.iter().chain(&["menu", "files", "preview", "switcher", "settings", "tasks"]) {
        if let Some(w) = app.get_webview_window(p) {
            let _ = w.set_effects(EffectsBuilder::new().effect(Effect::Acrylic).color(color).build());
        }
    }
}

#[tauri::command]
fn tray_icons() -> Vec<tray::TrayIcon> {
    tray::icons()
}

#[tauri::command]
fn tray_click(app: AppHandle, key: String, button: String) {
    if let Some(w) = app.get_webview_window("tray") {
        let _ = w.hide();
    }
    tray::click(&key, &button);
}

/// Whether Pink Cloud is hosting tray icons (not in preview mode, where Windows keeps them).
#[tauri::command]
fn tray_enabled() -> bool {
    !preview()
}

#[derive(serde::Deserialize)]
struct PreviewWin {
    hwnd: i64,
    title: String,
    minimized: bool,
}

/// Live previews of an app's windows, in a popup above its taskbar button.
/// `anchor` is the button's centre, in logical pixels from the taskbar's left edge.
#[tauri::command]
fn show_preview(app: AppHandle, state: State<AppState>, wins: Vec<PreviewWin>, anchor: f64) {
    let Some(pv) = app.get_webview_window("preview") else { return };
    for t in state.thumbs.lock().unwrap().drain(..) {
        shell::drop_thumbnail(t);
    }
    let wins: Vec<_> = wins.into_iter().take(5).collect();
    if wins.is_empty() {
        let _ = pv.hide();
        return;
    }
    // card layout, logical pixels: title row on top, preview area below
    const CARD_W: f64 = 228.0;
    const PAD: f64 = 8.0;
    const TOP: f64 = 38.0;
    const AREA_W: f64 = 212.0;
    const AREA_H: f64 = 120.0;
    let (x, y, w, h, sf) = screen(&pv);
    let (win_w, win_h) = (wins.len() as f64 * CARD_W + PAD, TOP + AREA_H + PAD * 2.0);
    let pw = (win_w * sf) as i32;
    let px = (x + (anchor * sf) as i32 - pw / 2).clamp(x + 8, x + w - pw - 8);
    let py = y + h - bottom_offset(sf) - ((BAR_HEIGHT + GAP + win_h) * sf) as i32;
    // already open for another app: glide over to the new one instead of jumping
    let from = if pv.is_visible().unwrap_or(false) { pv.outer_position().ok().map(|p| p.x) } else { None };
    let _ = pv.set_size(PhysicalSize::new(pw as u32, (win_h * sf) as u32));
    match from {
        Some(fx) if (fx - px).abs() > 2 => {
            let win = pv.clone();
            std::thread::spawn(move || {
                const STEPS: i32 = 14;
                for s in 1..=STEPS {
                    let t = s as f64 / STEPS as f64;
                    let ease = 1.0 - (1.0 - t).powi(3);
                    let _ = win.set_position(PhysicalPosition::new(fx + ((px - fx) as f64 * ease) as i32, py));
                    std::thread::sleep(Duration::from_millis(12));
                }
            });
        }
        _ => {
            let _ = pv.set_position(PhysicalPosition::new(px, py));
        }
    }
    let _ = pv.show();

    let mut cards = Vec::new();
    let mut thumbs = state.thumbs.lock().unwrap();
    for (i, win) in wins.iter().enumerate() {
        let card_x = PAD + i as f64 * CARD_W;
        let mut live = false;
        if !win.minimized {
            if let Some((t, sw, sh)) = shell::thumbnail(hwnd(&pv), win.hwnd) {
                // fit the window into the preview area, keeping its shape
                let scale = (AREA_W / sw.max(1) as f64).min(AREA_H / sh.max(1) as f64) * sf;
                let (tw, th) = ((sw as f64 * scale) as i32, (sh as f64 * scale) as i32);
                let ax = ((card_x + 4.0) * sf) as i32 + ((AREA_W * sf) as i32 - tw) / 2;
                let ay = (TOP * sf) as i32 + ((AREA_H * sf) as i32 - th) / 2;
                shell::place_thumbnail(t, ax, ay, ax + tw, ay + th);
                thumbs.push(t);
                live = true;
            }
        }
        let snap = if live { None } else { state.snaps.lock().unwrap().get(&win.hwnd).cloned() };
        cards.push(serde_json::json!({ "hwnd": win.hwnd, "title": win.title, "x": card_x, "live": live, "snap": snap }));
    }
    let _ = app.emit_to("preview", "preview", serde_json::json!({ "cards": cards, "width": CARD_W - PAD }));
}

/// Hides the previews; unless `force`, it stays while the mouse is on it.
#[tauri::command]
fn hide_preview(app: AppHandle, state: State<AppState>, force: bool) {
    if !force && *state.preview_hold.lock().unwrap() {
        return;
    }
    for t in state.thumbs.lock().unwrap().drain(..) {
        shell::drop_thumbnail(t);
    }
    if let Some(pv) = app.get_webview_window("preview") {
        let _ = pv.hide();
    }
}

#[tauri::command]
fn preview_hold(state: State<AppState>, hold: bool) {
    *state.preview_hold.lock().unwrap() = hold;
}

#[tauri::command]
fn now_playing(state: State<AppState>) -> Option<media::NowPlaying> {
    combine(state.now_playing.lock().unwrap().clone(), state.heard.lock().unwrap().clone())
}

/// What to show as Now Playing: the app's song, or (when no app reports one) a song recognized
/// by sound. When the app only gives the title (some web players), the recognized song fills in
/// the artist, album art and length, so the lyrics find the right song.
fn combine(real: Option<media::NowPlaying>, heard: Option<media::NowPlaying>) -> Option<media::NowPlaying> {
    let norm = |s: &str| s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    match (real, heard) {
        (None, h) => h,
        (Some(mut r), Some(h)) if r.artist.is_empty() && !h.title.is_empty() && norm(&r.title).contains(&norm(&h.title)) => {
            r.title = h.title;
            r.artist = h.artist;
            if r.art.is_none() {
                r.art = h.art;
            }
            if r.duration == 0 {
                r.duration = h.duration;
            }
            Some(r)
        }
        (r, _) => r,
    }
}

fn tell_now_playing(app: &AppHandle, np: Option<media::NowPlaying>) {
    discord::tell(np.clone());
    for target in ["taskbar", "lyrics", "idle", "widgets", "overlay"] {
        let _ = app.emit_to(target, "now-playing", np.clone());
    }
}

// ---------- recognizing songs by sound (games and other apps that don't report songs) ----------

/// Records ~10 s of what the PC is playing and asks AudD (audd.io) which song it is.
/// Only runs when you switched song recognition on and gave your own AudD key.
#[tauri::command]
async fn hear_song(token: String) -> Result<serde_json::Value, String> {
    use std::os::windows::process::CommandExt;
    let (wav, start) = audio::record_clip(10).ok_or("Nothing is playing right now.")?;
    let dir = std::env::temp_dir();
    let (clip, key) = (dir.join("PinkCloud-clip.wav"), dir.join("PinkCloud-audd-key.txt"));
    std::fs::write(&clip, wav).map_err(|e| e.to_string())?;
    // the key goes in through a file so it doesn't show up in the process list
    std::fs::write(&key, token.trim()).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("curl.exe")
        .args(["-s", "--max-time", "40", "-F"])
        .arg(format!("api_token=<{}", key.display()))
        .args(["-F", "return=spotify,apple_music", "-F"])
        .arg(format!("file=@{}", clip.display()))
        .arg("https://api.audd.io/")
        .creation_flags(0x0800_0000)
        .output();
    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(&clip);
    let out = out.map_err(|_| "Couldn't reach the song recognition service.")?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| "The song recognition service sent something unexpected.")?;
    Ok(serde_json::json!({ "answer": json, "start": start }))
}

/// A song recognized by sound (or None when it's over). Shown wherever no app reports a song.
#[tauri::command]
fn set_heard(app: AppHandle, state: State<AppState>, np: Option<media::NowPlaying>) {
    *state.heard.lock().unwrap() = np.clone();
    let real = state.now_playing.lock().unwrap().clone();
    tell_now_playing(&app, combine(real, np));
}

#[tauri::command]
async fn media_control(action: String) {
    media::control(&action);
}

#[tauri::command]
fn set_visualizer(on: bool) {
    audio::set_enabled(on);
}

#[tauri::command]
fn get_autostart() -> bool {
    shell::autostart()
}

#[tauri::command]
fn set_autostart(on: bool) -> bool {
    shell::set_autostart(on)
}

#[tauri::command]
fn app_exe() -> String {
    std::env::current_exe().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// A Pink Cloud window, built again from its settings if it was ever closed for good.
fn window_or_rebuild(app: &AppHandle, label: &str) -> Option<WebviewWindow> {
    if let Some(w) = app.get_webview_window(label) {
        return Some(w);
    }
    let conf = app.config().app.windows.iter().find(|w| w.label == label)?.clone();
    let w = tauri::WebviewWindowBuilder::from_config(app, &conf).ok()?.build().ok()?;
    shell::round_corners(hwnd(&w));
    Some(w)
}

/// Windows being built on demand, waiting for their page to say it's ready.
static LOADING: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The last tint the frosted glass got (see set_tint).
static TINT: Mutex<Option<[u8; 4]>> = Mutex::new(None);

/// A rarely used screen, which only exists once it's needed so it costs nothing until then.
/// The first time this returns None and builds it in the background (building a window from
/// a command can hang on Windows); its page calls screen_ready when loaded, which opens it.
fn on_demand(app: &AppHandle, label: &str) -> Option<WebviewWindow> {
    if let Some(w) = app.get_webview_window(label) {
        return Some(w);
    }
    {
        let mut loading = LOADING.lock().unwrap();
        if loading.iter().any(|l| l == label) {
            return None;
        }
        loading.push(label.to_string());
    }
    let (app, label) = (app.clone(), label.to_string());
    std::thread::spawn(move || {
        let built = app.config().app.windows.iter().find(|w| w.label == label).cloned().and_then(|conf| tauri::WebviewWindowBuilder::from_config(&app, &conf).ok()?.build().ok());
        match built {
            Some(w) if label == "tasks" || label == "tour" => shell::round_corners(hwnd(&w)),
            Some(_) => {}
            None => LOADING.lock().unwrap().retain(|l| *l != label),
        }
    });
    None
}

/// Opens each on-demand screen, logs how long it took, and puts it away again. The idle screen
/// goes twice, because it's thrown away when you come back and has to build again.
fn try_screens(app: AppHandle) {
    std::thread::sleep(Duration::from_secs(8));
    for label in ["tasks", "overlay", "tour", "idle", "idle"] {
        let started = Instant::now();
        match label {
            "tasks" => open_tasks(app.clone()),
            "overlay" => toggle_overlay(app.clone()),
            "tour" => open_tour(app.clone()),
            _ => show_idle(app.clone()),
        }
        while !visible(&app, label) && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(25));
        }
        safe::log(&format!("screen check: {label} {} after {} ms", if visible(&app, label) { "opened" } else { "DID NOT OPEN" }, started.elapsed().as_millis()));
        std::thread::sleep(Duration::from_millis(2500));
        match label {
            "idle" => hide_idle(app.clone()),
            _ => {
                if let Some(w) = app.get_webview_window(label) {
                    let _ = w.hide();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    safe::log("screen check: done");
}

/// An on-demand screen finished loading: now it can open.
#[tauri::command]
fn screen_ready(app: AppHandle, window: WebviewWindow) {
    let label = window.label().to_string();
    {
        let mut loading = LOADING.lock().unwrap();
        if !loading.iter().any(|l| *l == label) {
            return;
        }
        loading.retain(|l| *l != label);
    }
    match label.as_str() {
        "tasks" => {
            // it missed the tint the other screens got when the theme was set
            if let Some(c) = *TINT.lock().unwrap() {
                let _ = window.set_effects(EffectsBuilder::new().effect(Effect::Acrylic).color(Color(c[0], c[1], c[2], c[3])).build());
            }
            open_tasks(app);
        }
        "tour" => open_tour(app),
        "overlay" => toggle_overlay(app),
        "idle" => show_idle(app),
        _ => {}
    }
}

/// Brings up Pink Cloud Files, optionally at a folder.
#[tauri::command]
fn open_files(app: AppHandle, path: Option<String>) {
    let Some(win) = window_or_rebuild(&app, "files") else { return };
    if win.is_minimized().unwrap_or(false) {
        let _ = win.unminimize();
    }
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit_to("files", "files-open", path);
}

/// Brings up Pink Cloud Settings, optionally on a page.
#[tauri::command]
fn open_settings(app: AppHandle, page: Option<String>) {
    let Some(win) = window_or_rebuild(&app, "settings") else { return };
    if win.is_minimized().unwrap_or(false) {
        let _ = win.unminimize();
    }
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit_to("settings", "settings-open", page);
}

#[tauri::command]
async fn sys_info() -> shell::SysInfo {
    shell::sys_info()
}

#[tauri::command]
fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
async fn fs_list(path: String) -> Result<Vec<files::Entry>, String> {
    files::list_dir(&path)
}

#[tauri::command]
fn fs_places() -> Vec<files::Place> {
    files::places()
}

#[tauri::command]
async fn fs_drives() -> Vec<files::Drive> {
    files::drives()
}

#[tauri::command]
fn fs_action(action: String, path: String) {
    match action.as_str() {
        "openwith" => files::open_with(&path),
        "properties" => files::properties(&path),
        "explorer" => folders::open_explorer(Some(&path), true),
        _ => shell::launch(&path),
    }
}

/// The normal Windows File Explorer, at a folder or on its Home page.
#[tauri::command]
fn open_explorer(path: Option<String>) {
    folders::open_explorer(path.as_deref(), false);
}

#[tauri::command]
fn fs_rename(path: String, name: String) -> Result<String, String> {
    files::rename(&path, &name)
}

#[tauri::command]
fn fs_new_folder(dir: String) -> Result<String, String> {
    files::new_folder(&dir)
}

#[tauri::command]
async fn fs_recycle(paths: Vec<String>) -> Result<(), String> {
    files::recycle(&paths)
}

#[tauri::command]
async fn window_icon(state: State<'_, AppState>, hwnd: i64) -> Result<Option<String>, ()> {
    let key = format!("hwnd:{hwnd}");
    if let Some(hit) = state.icons.lock().unwrap().get(&key) {
        if hit.is_some() {
            return Ok(hit.clone());
        }
    }
    let icon = shell::window_icon(hwnd);
    state.icons.lock().unwrap().insert(key, icon.clone());
    Ok(icon)
}

#[tauri::command]
async fn recent_files() -> Vec<files::Entry> {
    files::recent(12)
}

#[tauri::command]
async fn start_search(query: String) -> Vec<files::Entry> {
    files::quick_search(&query)
}

#[tauri::command]
fn fs_clip_set(paths: Vec<String>, cut: bool) -> Result<(), String> {
    files::clipboard_set(&paths, cut)
}

#[tauri::command]
fn fs_clip_get() -> (Vec<String>, bool) {
    files::clipboard_get()
}

#[tauri::command]
async fn fs_paste(dest: String) -> Result<(), String> {
    let (paths, cut) = files::clipboard_get();
    files::paste(&paths, &dest, cut)
}

#[tauri::command]
async fn fs_move(paths: Vec<String>, dest: String, copy: bool) -> Result<(), String> {
    files::transfer(&paths, &dest, copy)
}

#[tauri::command]
async fn fs_read_text(path: String) -> Result<String, String> {
    files::read_text(&path)
}

#[tauri::command]
async fn fs_search(root: String, query: String) -> Vec<files::Entry> {
    files::search(&root, &query)
}

#[tauri::command]
fn quit(app: AppHandle) {
    cleanup(&app);
    app.exit(0);
}

fn main() {
    // Developer check: `pink-cloud --icon-test <out dir> <path>...` saves each icon as a PNG and exits.
    // sound effects play without a click first (Tauri's own WebView2 flags are kept)
    std::env::set_var(
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required",
    );
    let args: Vec<String> = std::env::args().collect();
    // the safe-start helper (see safe.rs): `--watch <pid> <pink-cloud.exe>`
    if args.get(1).map(String::as_str) == Some("--watch") {
        if let (Some(pid), Some(exe)) = (args.get(2).and_then(|p| p.parse().ok()), args.get(3)) {
            safe::watch(pid, std::path::PathBuf::from(exe));
        }
        return;
    }
    // Developer check: `pink-cloud --now-playing` prints what the Now Playing pill would show.
    if args.get(1).map(String::as_str) == Some("--now-playing") {
        let (tx, rx) = std::sync::mpsc::channel();
        media::watch(move |np| {
            let _ = tx.send(np);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(6)).ok().flatten() {
            Some(n) => println!(
                "{} - {} | playing: {} | app: {} | art: {} | at {} of {} ms (as of {} ms ago)",
                n.title,
                n.artist,
                n.playing,
                n.app,
                n.art.map(|a| a.len()).unwrap_or(0),
                n.position,
                n.duration,
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0) - n.stamp
            ),
            None => println!("nothing playing"),
        }
        return;
    }
    // Developer check: `pink-cloud --timing` samples the reported position for a few seconds and
    // shows where in the song the screens would think we are each time.
    if args.get(1).map(String::as_str) == Some("--timing") {
        for _ in 0..8 {
            if let Some(n) = media::read_now() {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
                println!(
                    "{} | reported {} ms, {} ms ago | estimated now: {} ms | playing: {}",
                    n.title,
                    n.position,
                    now - n.stamp,
                    n.position + if n.playing { now - n.stamp } else { 0 },
                    n.playing
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
        return;
    }
    // Developer check: `pink-cloud --hear [key]` records 10 s of what's playing and asks AudD.
    if args.get(1).map(String::as_str) == Some("--hear") {
        let key = args.get(2).cloned().unwrap_or_default();
        match tauri::async_runtime::block_on(hear_song(key)) {
            Ok(v) => {
                let r = &v["answer"]["result"];
                if r.is_object() {
                    println!("heard: {} - {} at {} (song length {} ms)", r["artist"], r["title"], r["timecode"], r["spotify"]["duration_ms"]);
                } else {
                    println!("answer: {}", v["answer"]);
                }
            }
            Err(e) => println!("error: {e}"),
        }
        return;
    }
    // Developer check: `pink-cloud --notes` prints the latest notifications as Pink Cloud sees them.
    if args.get(1).map(String::as_str) == Some("--notes") {
        for n in notify::recent(5) {
            println!("{} | {} | {:?} | {:?} | picture: {}", n.time, shell::app_name(&n.app), n.title, n.body.chars().take(40).collect::<String>(), n.image.is_some());
        }
        let u = shell::usage();
        std::thread::sleep(Duration::from_millis(500));
        let u2 = shell::usage();
        println!("usage: cpu {:.0}% (first read {:.0}%), ram {:.0}% ({:.1}/{:.1} GB) | idle {} ms | screen kept on: {}", u2.cpu, u.cpu, u2.ram, u2.ram_used, u2.ram_total, shell::idle_ms(), shell::screen_kept_on());
        return;
    }
    // Developer check: `pink-cloud --discord-test` shows a test song as your Discord status for 20 s.
    if args.get(1).map(String::as_str) == Some("--discord-test") {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        discord::start();
        discord::set_on(true);
        discord::tell(Some(media::NowPlaying { title: "Test song".into(), artist: "Pink Cloud".into(), playing: true, position: 30_000, stamp: now, duration: 200_000, ..Default::default() }));
        std::thread::sleep(Duration::from_secs(20));
        println!("{}", discord::last_result());
        return;
    }
    // Developer check: `pink-cloud --media-fields` prints everything the playing app tells Windows.
    if args.get(1).map(String::as_str) == Some("--media-fields") {
        media::dump();
        return;
    }
    // Developer check: `pink-cloud --clip-test <file>` copies a file to the clipboard and reads it back.
    if args.get(1).map(String::as_str) == Some("--clip-test") {
        let set = files::clipboard_set(&[args[2].clone()], false);
        println!("set: {set:?} | read back: {:?}", files::clipboard_get());
        return;
    }
    if args.get(1).map(String::as_str) == Some("--clip-history-test") {
        clip::self_test();
        return;
    }
    // Developer check: `pink-cloud --window-test <exact title>` keeps that window on top, stops, then ends its app.
    if args.get(1).map(String::as_str) == Some("--window-test") {
        let pid = std::process::id();
        let find = || shell::list_windows(pid).into_iter().find(|w| w.title == args[2]);
        let Some(w) = find() else { return println!("no window with that title") };
        shell::set_topmost(w.hwnd, true);
        println!("after keep on top: {:?}", find().map(|w| w.topmost));
        shell::set_topmost(w.hwnd, false);
        println!("after stop: {:?}", find().map(|w| w.topmost));
        println!("end task said: {}", shell::end_task(w.hwnd));
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("still there: {}", find().is_some());
        return;
    }
    // Developer check: `pink-cloud --tasks-test` prints what the Task Manager sees.
    if args.get(1).map(String::as_str) == Some("--tasks-test") {
        // with a program's path after it, also tries to end that program
        if let Some(key) = args.get(2) {
            println!("end {key}: {}", procs::end_app(&key.to_lowercase()));
        } else {
            procs::self_test();
        }
        return;
    }
    // Developer check: `pink-cloud --mixer-test` prints the apps with sound open.
    if args.get(1).map(String::as_str) == Some("--mixer-test") {
        for a in mixer::apps() {
            println!("{} | {} | volume {:.2} | muted {} | playing {}", a.name, a.key, a.volume, a.muted, a.playing);
        }
        return;
    }
    // Developer check: `pink-cloud --windows` prints the taskbar's windows and how they're grouped.
    if args.get(1).map(String::as_str) == Some("--windows") {
        for w in shell::list_windows(std::process::id()) {
            let icon = if w.app_id.is_empty() { "-".to_string() } else { format!("{} bytes", shell::icon_data_url(&format!("shell:AppsFolder\\{}", w.app_id)).map(|i| i.len()).unwrap_or(0)) };
            let snap = shell::snapshot(w.hwnd, 480).map(|s| s.len()).unwrap_or(0);
            println!("{} | {} | app id: {} | app icon: {} | snapshot: {} bytes{}", w.title, w.exe.rsplit('\\').next().unwrap_or(""), w.app_id, icon, snap, if w.minimized { " (minimized)" } else { "" });
        }
        println!("recent files: {}", files::recent(12).len());
        return;
    }
    // Developer check: `pink-cloud --list-apps` prints what Start would show.
    if args.get(1).map(String::as_str) == Some("--list-apps") {
        let apps = shell::list_apps();
        println!("{} apps", apps.len());
        for a in apps.iter().filter(|a| args.get(2).map_or(true, |q| a.name.to_lowercase().contains(&q.to_lowercase()))) {
            println!("{} -> {}", a.name, a.path);
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--icon-test") {
        use base64::Engine;
        let out = std::path::Path::new(&args[2]);
        let _ = std::fs::create_dir_all(out);
        // all at once, on separate threads, the way the screens ask for them
        let jobs: Vec<_> = args[3..]
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, p)| {
                let out = out.to_path_buf();
                std::thread::spawn(move || match shell::icon_data_url(&p) {
                    Some(url) => {
                        let bytes = base64::engine::general_purpose::STANDARD.decode(url.trim_start_matches("data:image/png;base64,")).unwrap_or_default();
                        let _ = std::fs::write(out.join(format!("{i}.png")), bytes);
                        println!("{i}: ok {p}");
                    }
                    None => println!("{i}: no icon {p}"),
                })
            })
            .collect();
        for j in jobs {
            let _ = j.join();
        }
        return;
    }

    // `--open <folder>`: opening a folder anywhere in Windows starts this (see folders.rs).
    // A drive comes in as `C:"` (the quote after "C:\" gets read as part of it).
    let open_folder = args.iter().position(|a| a == "--open").and_then(|i| args.get(i + 1)).map(|p| {
        let p = p.trim_end_matches('"').to_string();
        if p.ends_with(':') { p + "\\" } else { p }
    });

    // Only one Pink Cloud at a time (e.g. autostart plus a double-click); a folder goes to the
    // one that's running.
    if !shell::first_instance() {
        if let Some(p) = &open_folder {
            folders::forward(p);
        }
        return;
    }
    // the screens keep their data in this folder; it isn't there yet on the very first run
    let data = std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("com.daan.pinkcloud");
    FIRST_RUN.store(!data.exists(), std::sync::atomic::Ordering::SeqCst);

    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            toggle_popup,
            popup_blurred,
            get_windows,
            activate_window,
            close_window,
            end_task,
            screen_ready,
            list_tasks,
            end_app,
            open_tasks,
            set_topmost,
            audio_apps,
            set_app_volume,
            set_app_mute,
            clip_set_on,
            clip_history,
            clip_copy,
            clip_clear,
            launch,
            get_icon,
            list_apps,
            get_status,
            set_volume,
            set_mute,
            user_name,
            js_log,
            power,
            set_tint,
            show_menu,
            menu_pick,
            set_hit_rects,
            set_bar_capture,
            set_layer_hits,
            notes_recent,
            app_name,
            set_hide_banners,
            show_idle,
            hide_idle,
            set_idle_minutes,
            sys_usage,
            announce_update,
            install_update,
            tray_icons,
            tray_click,
            tray_enabled,
            app_exe,
            get_autostart,
            set_autostart,
            game_state,
            set_game_off,
            set_shortcuts,
            toggle_overlay,
            fit_search,
            fs_move,
            open_explorer,
            fs_read_text,
            wallpaper_key,
            wallpaper_data,
            set_visualizer,
            show_preview,
            hide_preview,
            switcher_pick,
            preview_hold,
            now_playing,
            media_control,
            hear_song,
            set_heard,
            open_files,
            open_settings,
            sys_info,
            app_version,
            fs_list,
            fs_places,
            fs_drives,
            fs_action,
            fs_rename,
            fs_new_folder,
            fs_recycle,
            window_icon,
            recent_files,
            start_search,
            fs_clip_set,
            fs_clip_get,
            fs_paste,
            fs_search,
            report_details,
            crash_report,
            dismiss_crash,
            skipped_version,
            first_run,
            open_tour,
            show_card,
            discord_available,
            set_discord,
            quit
        ])
        .setup(move |app| {
            for p in POPUPS.iter().chain(&["menu", "files", "settings", "tasks", "tour", "switcher"]) {
                if let Some(w) = app.get_webview_window(p) {
                    shell::round_corners(hwnd(&w));
                }
            }
            let bar = app.get_webview_window("taskbar").expect("taskbar window");
            shell::no_activate(hwnd(&bar));
            shell::unregister_appbar(hwnd(&bar));
            place_taskbar(app.handle());
            // Developer check: with this file in the temp folder, opens every on-demand screen
            // once when Pink Cloud starts and logs whether it came up (the file is removed first).
            let check = std::env::temp_dir().join("PinkCloud-try-screens.txt");
            if std::fs::remove_file(&check).is_ok() {
                let handle = app.handle().clone();
                std::thread::spawn(move || try_screens(handle));
            }
            if !preview() {
                safe::start(&app.package_info().version.to_string());
                discord::start();
                shell::take_over_taskbar();
                std::thread::spawn(shell::upgrade_autostart);
                let handle = app.handle().clone();
                keys::watch(move |k| switcher_key(&handle, k));
                let handle = app.handle().clone();
                tray::start(move || {
                    let _ = handle.emit_to("taskbar", "tray-changed", ());
                    let _ = handle.emit_to("tray", "tray-changed", ());
                });
            }
            bar.show()?;
            let handle = app.handle().clone();
            std::thread::spawn(move || hit_watcher(handle));
            let handle = app.handle().clone();
            std::thread::spawn(move || screen_watcher(handle));

            // folders opened anywhere come to Pink Cloud Files
            let handle = app.handle().clone();
            folders::listen(move |p| open_files(handle.clone(), Some(p)));
            let handle = app.handle().clone();
            folders::watch_explorer(move |p| open_files(handle.clone(), Some(p)));
            if let Some(p) = open_folder.clone() {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    open_files(handle, Some(p));
                });
            }
            let handle = app.handle().clone();
            std::thread::spawn(move || snapshot_watcher(handle));
            let handle = app.handle().clone();
            std::thread::spawn(move || idle_watcher(handle));

            // notifications: pop-ups, the bell's count and the panel all hear about new ones
            let handle = app.handle().clone();
            notify::watch(move |notes| {
                if handle.state::<AppState>().hide_banners.load(std::sync::atomic::Ordering::SeqCst) {
                    for n in &notes {
                        notify::hide_banner(&n.app);
                    }
                }
                for target in ["toast", "taskbar", "notes", "widgets"] {
                    let _ = handle.emit_to(target, "notes-new", notes.clone());
                }
            });

            // see-through layers start out letting every click through
            for label in LAYERS {
                if let Some(w) = app.get_webview_window(label) {
                    let (x, y, sw, sh, sf) = screen(&w);
                    let lh = sh - bottom_offset(sf) - (BAR_HEIGHT * sf) as i32;
                    let _ = w.set_size(PhysicalSize::new(sw as u32, lh as u32));
                    let _ = w.set_position(PhysicalPosition::new(x, y));
                    let _ = w.set_ignore_cursor_events(true);
                    shell::no_activate(hwnd(&w));
                }
            }
            // widgets live on the desktop: in front of the wallpaper, behind your windows
            if let Some(w) = app.get_webview_window("widgets") {
                shell::stick_to_desktop(hwnd(&w));
            }
            let handle = app.handle().clone();
            audio::start(move |levels| {
                let _ = handle.emit_to("taskbar", "audio", levels.clone());
                if visible(&handle, "idle") {
                    let _ = handle.emit_to("idle", "audio", levels);
                }
            });
            let handle = app.handle().clone();
            media::watch(move |np| {
                *handle.state::<AppState>().now_playing.lock().unwrap() = np.clone();
                let heard = handle.state::<AppState>().heard.lock().unwrap().clone();
                tell_now_playing(&handle, combine(np, heard));
            });
            for label in ["preview", "switcher"] {
                if let Some(w) = app.get_webview_window(label) {
                    shell::no_activate(hwnd(&w));
                    shell::round_corners(hwnd(&w));
                }
            }
            // lyrics pop up anywhere on the screen above the taskbar; clicks go straight through
            if let Some(ly) = app.get_webview_window("lyrics") {
                let (x, y, w, h, sf) = screen(&ly);
                let lh = h - bottom_offset(sf) - (BAR_HEIGHT * sf) as i32;
                let _ = ly.set_size(PhysicalSize::new(w as u32, lh as u32));
                let _ = ly.set_position(PhysicalPosition::new(x, y));
                let _ = ly.set_ignore_cursor_events(true);
                shell::no_activate(hwnd(&ly));
            }
            Ok(())
        })
        // Closing a Pink Cloud window the Windows way (Alt+F4, "Close window" on the taskbar)
        // only hides it, so it can always be opened again. Quitting Pink Cloud still works.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("Pink Cloud failed to start")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                cleanup(app);
            }
        });
}
