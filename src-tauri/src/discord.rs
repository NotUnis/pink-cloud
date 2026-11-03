//! Discord status: "Listening to <song>" on your Discord profile while music plays.
//!
//! Talks to the Discord app on this PC through its local pipe (the same way games set their
//! status); nothing goes online from Pink Cloud itself. Off unless you switch it on in Settings.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::media::NowPlaying;

/// The "Pink Cloud" application on Discord's developer site; its name is what Discord shows.
pub const APP_ID: &str = "1567282052737269960";

static ON: AtomicBool = AtomicBool::new(false);
static TX: OnceLock<Mutex<Sender<Option<NowPlaying>>>> = OnceLock::new();
/// Discord's last answer, for the developer check.
static LAST: Mutex<String> = Mutex::new(String::new());

pub fn last_result() -> String {
    let l = LAST.lock().unwrap().clone();
    if l.is_empty() { "Discord didn't answer (is the Discord app open?)".into() } else { l }
}

pub fn available() -> bool {
    !APP_ID.is_empty()
}

fn frame(pipe: &mut File, op: u32, body: &serde_json::Value) -> std::io::Result<()> {
    let data = body.to_string().into_bytes();
    let mut msg = Vec::with_capacity(8 + data.len());
    msg.extend(op.to_le_bytes());
    msg.extend((data.len() as u32).to_le_bytes());
    msg.extend(data);
    pipe.write_all(&msg)
}

fn read_frame(pipe: &mut File) -> std::io::Result<serde_json::Value> {
    let mut head = [0u8; 8];
    pipe.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let mut body = vec![0u8; len.min(1 << 20)];
    pipe.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body).unwrap_or_default())
}

/// Finds the running Discord app and says hello.
fn connect() -> Option<File> {
    for i in 0..10 {
        let Ok(mut pipe) = OpenOptions::new().read(true).write(true).open(format!(r"\\?\pipe\discord-ipc-{i}")) else { continue };
        if frame(&mut pipe, 0, &serde_json::json!({ "v": 1, "client_id": APP_ID })).is_ok() && read_frame(&mut pipe).is_ok() {
            return Some(pipe);
        }
    }
    None
}

fn activity(np: &Option<NowPlaying>) -> serde_json::Value {
    let Some(n) = np.as_ref().filter(|n| !n.title.is_empty() && ON.load(Ordering::SeqCst)) else {
        return serde_json::Value::Null;
    };
    let mut a = serde_json::json!({
        "type": 2, // "Listening to"
        "status_display_type": 2, // the song's name shows next to your name
        "details": n.title.chars().take(120).collect::<String>(),
        "state": if n.artist.is_empty() { "Pink Cloud".to_string() } else { format!("by {}", n.artist.chars().take(110).collect::<String>()) },
        "assets": { "large_image": "pinkcloud", "large_text": "Listening with Pink Cloud" },
    });
    if n.playing {
        // the time bar on the profile: when the song started, and when it ends
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        let start = now - (n.position + (now - n.stamp).max(0));
        a["timestamps"] = serde_json::json!({ "start": start });
        if n.duration > 0 {
            a["timestamps"]["end"] = serde_json::json!(start + n.duration);
        }
    } else {
        a["state"] = serde_json::json!(format!("{} (paused)", a["state"].as_str().unwrap_or("")));
    }
    a
}

/// Starts the worker that keeps the Discord status in step with what's playing.
pub fn start() {
    if !available() {
        return;
    }
    let (tx, rx) = channel::<Option<NowPlaying>>();
    let _ = TX.set(Mutex::new(tx));
    std::thread::spawn(move || {
        let mut pipe: Option<File> = None;
        let mut last: Option<NowPlaying> = None;
        let mut sent = serde_json::Value::Bool(false);
        loop {
            // a new song (or nothing new for a while: try again if Discord wasn't open)
            if let Ok(np) = rx.recv_timeout(Duration::from_secs(20)) {
                last = np;
            }
            let act = activity(&last);
            if act == sent && pipe.is_some() {
                continue;
            }
            if act.is_null() && pipe.is_none() {
                sent = act;
                continue;
            }
            if pipe.is_none() {
                pipe = connect();
            }
            let Some(p) = pipe.as_mut() else { continue };
            let msg = serde_json::json!({
                "cmd": "SET_ACTIVITY",
                "args": { "pid": std::process::id(), "activity": act },
                "nonce": format!("{:?}", SystemTime::now()),
            });
            match frame(p, 1, &msg).and_then(|_| read_frame(p)) {
                Ok(answer) => {
                    *LAST.lock().unwrap() = answer.to_string();
                    sent = act;
                }
                Err(e) => {
                    *LAST.lock().unwrap() = format!("lost Discord: {e}");
                    pipe = None; // Discord closed; reconnect next time
                }
            }
        }
    });
}

pub fn set_on(on: bool) {
    ON.store(on, Ordering::SeqCst);
}

/// What's playing changed (or the setting did): update the status.
pub fn tell(np: Option<NowPlaying>) {
    if let Some(tx) = TX.get() {
        let _ = tx.lock().unwrap().send(np);
    }
}
