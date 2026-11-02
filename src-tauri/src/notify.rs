//! Notifications: Windows keeps every notification in a small database on your PC.
//! Pink Cloud reads a copy of it (never the original, and never writes to it) to show
//! its own pop-ups and the notification panel.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct Note {
    pub id: i64,
    /// the app's ID, e.g. "Discord" or "Claude_xxx!Claude" (also how its icon is found)
    pub app: String,
    pub title: String,
    pub body: String,
    /// a picture in the notification (a file on this PC), if any
    pub image: Option<String>,
    /// when it arrived, unix ms
    pub time: i64,
}

fn source() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join(r"Microsoft\Windows\Notifications\wpndatabase.db")
}

fn copy_dir() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join(r"PinkCloud\notifications")
}

/// Copies the database (and its recent-changes files) and opens the copy read-only.
fn open_copy() -> Option<Connection> {
    let dir = copy_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let src = source();
    for ext in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{ext}", src.display()));
        let to = dir.join(format!("wpndatabase.db{ext}"));
        if from.exists() {
            std::fs::copy(&from, &to).ok()?;
        } else {
            let _ = std::fs::remove_file(&to);
        }
    }
    Connection::open_with_flags(dir.join("wpndatabase.db"), OpenFlags::SQLITE_OPEN_READ_WRITE).ok()
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&#39;", "'").replace("&amp;", "&")
}

/// The texts and first picture of a notification's XML.
fn parse(xml: &str) -> (Vec<String>, Option<String>) {
    let mut texts = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<text") {
        rest = &rest[i..];
        let Some(open_end) = rest.find('>') else { break };
        if rest[..open_end].ends_with('/') {
            rest = &rest[open_end + 1..];
            continue;
        }
        let Some(close) = rest.find("</text>") else { break };
        let t = unescape(rest[open_end + 1..close].trim());
        if !t.is_empty() {
            texts.push(t);
        }
        rest = &rest[close + 7..];
    }
    let image = xml.find("<image").and_then(|i| {
        let tag = &xml[i..xml[i..].find('>').map(|e| i + e).unwrap_or(xml.len())];
        let s = tag.find("src=\"")? + 5;
        let src = &tag[s..s + tag[s..].find('"')?];
        let src = unescape(src);
        // only pictures stored on this PC can be shown
        src.strip_prefix("file:///").map(|p| p.replace('/', "\\")).filter(|p| std::path::Path::new(p).exists())
    });
    (texts, image)
}

/// Toast notifications newer than `after` (FILETIME), newest first.
fn read(after: i64, limit: usize) -> (Vec<Note>, i64) {
    let Some(db) = open_copy() else { return (Vec::new(), after) };
    let mut newest = after;
    let mut out = Vec::new();
    let Ok(mut q) = db.prepare(
        "SELECT n.Id, n.ArrivalTime, h.PrimaryId, n.Payload FROM Notification n
         JOIN NotificationHandler h ON h.RecordId = n.HandlerId
         WHERE n.Type = 'toast' AND n.ArrivalTime > ?1 ORDER BY n.ArrivalTime DESC LIMIT ?2",
    ) else {
        return (out, after);
    };
    let rows = q.query_map(rusqlite::params![after, limit as i64], |r| {
        let payload: Vec<u8> = r.get::<_, Vec<u8>>(3).or_else(|_| r.get::<_, String>(3).map(|s| s.into_bytes()))?;
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, payload))
    });
    if let Ok(rows) = rows {
        for (id, arrival, app, payload) in rows.flatten() {
            newest = newest.max(arrival);
            let (texts, image) = parse(&String::from_utf8_lossy(&payload));
            if texts.is_empty() {
                continue;
            }
            out.push(Note {
                id,
                app,
                title: texts[0].clone(),
                body: texts[1..].join("\n"),
                image,
                time: (arrival - 116_444_736_000_000_000) / 10_000,
            });
        }
    }
    (out, newest)
}

/// The most recent notifications, for the panel.
pub fn recent(limit: usize) -> Vec<Note> {
    read(0, limit).0
}

/// Checks for new notifications every 1.5 s; `on_new` gets them, oldest first.
pub fn watch(on_new: impl Fn(Vec<Note>) + Send + 'static) {
    std::thread::spawn(move || {
        // start from what's already there, so old notifications don't all pop up again
        let mut seen = read(0, 1).1;
        let mut last_change: Option<SystemTime> = None;
        loop {
            std::thread::sleep(Duration::from_millis(1500));
            let changed = ["", "-wal"]
                .iter()
                .filter_map(|ext| std::fs::metadata(format!("{}{ext}", source().display())).and_then(|m| m.modified()).ok())
                .max();
            if changed.is_none() || changed == last_change {
                continue;
            }
            last_change = changed;
            let (mut new, newest) = read(seen, 10);
            seen = newest;
            if !new.is_empty() {
                new.reverse();
                on_new(new);
            }
        }
    });
}

// ---------- hiding Windows' own pop-ups (opt-in, undoable) ----------

const SETTINGS: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Notifications\Settings";

fn hidden_list() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join(r"PinkCloud\hidden-banners.txt")
}

fn reg(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("reg").args(args).creation_flags(0x0800_0000).status().map(|s| s.success()).unwrap_or(false)
}

/// Turns off Windows' own pop-up for this app (its notifications still arrive and Pink Cloud
/// shows them). Every app changed is written down, so it can all be put back.
pub fn hide_banner(app: &str) {
    let list = std::fs::read_to_string(hidden_list()).unwrap_or_default();
    if list.lines().any(|l| l == app) {
        return;
    }
    if reg(&["add", &format!("{SETTINGS}\\{app}"), "/v", "ShowBanner", "/t", "REG_DWORD", "/d", "0", "/f"]) {
        let _ = std::fs::write(hidden_list(), format!("{list}{app}\n"));
    }
}

/// Puts Windows' own pop-ups back for every app Pink Cloud changed.
pub fn restore_banners() {
    let list = std::fs::read_to_string(hidden_list()).unwrap_or_default();
    for app in list.lines().filter(|l| !l.is_empty()) {
        reg(&["delete", &format!("{SETTINGS}\\{app}"), "/v", "ShowBanner", "/f"]);
    }
    let _ = std::fs::remove_file(hidden_list());
}
