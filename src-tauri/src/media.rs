//! Now Playing: what Windows' media controls know about the current song (the same info
//! as the media flyout Windows shows when you press a volume key).

use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Serialize;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session, GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::DataReader;

#[derive(Serialize, serde::Deserialize, Clone, PartialEq, Default)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    /// album art as a data URL; only sent when the song changes
    pub art: Option<String>,
    /// the app that plays it, e.g. "Spotify.exe"
    pub app: String,
    /// where in the song playback was (ms) at `stamp` (unix ms); apps only report this
    /// now and then, so the screens count on from there themselves
    pub position: i64,
    pub stamp: i64,
    pub duration: i64,
}

fn manager() -> windows::core::Result<Manager> {
    Manager::RequestAsync()?.get()
}

/// The app whose music to show. Windows' own "current" pick isn't always the best one: it
/// can be paused while another app plays, or be a companion app that mirrors Spotify without
/// the artist name or playback position. So among the apps that are playing, the one that
/// tells us the most wins (artist, then position), and Windows' pick breaks ties.
fn session() -> Option<Session> {
    let m = manager().ok()?;
    let current = m.GetCurrentSession().ok();
    let current_id = current.as_ref().and_then(|c| c.SourceAppUserModelId().ok()).map(|h| h.to_string());
    let score = |s: &Session| -> i32 {
        let playing = s.GetPlaybackInfo().ok().and_then(|i| i.PlaybackStatus().ok()) == Some(Status::Playing);
        if !playing {
            return 0;
        }
        let has_artist = s.TryGetMediaPropertiesAsync().ok().and_then(|a| a.get().ok()).and_then(|p| p.Artist().ok()).is_some_and(|a| !a.is_empty());
        let has_position = s.GetTimelineProperties().ok().and_then(|t| t.Position().ok()).is_some_and(|p| p.Duration > 0);
        let is_current = s.SourceAppUserModelId().ok().map(|h| h.to_string()) == current_id;
        10 + if has_artist { 4 } else { 0 } + if has_position { 2 } else { 0 } + if is_current { 1 } else { 0 }
    };
    let best = m.GetSessions().ok().and_then(|all| all.into_iter().map(|s| (score(&s), s)).filter(|(sc, _)| *sc > 0).max_by_key(|(sc, _)| *sc));
    best.map(|(_, s)| s).or(current)
}

fn art_of(s: &Session) -> Option<String> {
    let props = s.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()? as u32;
    if size == 0 || size > 4_000_000 {
        return None;
    }
    let mime = stream.ContentType().map(|c| c.to_string()).unwrap_or_else(|_| "image/png".into());
    let reader = DataReader::CreateDataReader(&stream).ok()?;
    reader.LoadAsync(size).ok()?.get().ok()?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes).ok()?;
    Some(format!("data:{};base64,{}", if mime.is_empty() { "image/png" } else { &mime }, B64.encode(bytes)))
}

fn read() -> Option<NowPlaying> {
    let s = session()?;
    let props = s.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let title = props.Title().ok()?.to_string();
    if title.is_empty() {
        return None;
    }
    let playing = s.GetPlaybackInfo().ok().and_then(|i| i.PlaybackStatus().ok()) == Some(Status::Playing);
    // TimeSpan and DateTime are in 100 ns steps; DateTime counts from the year 1601
    let (mut position, mut stamp, mut duration) = (0, 0, 0);
    if let Ok(tl) = s.GetTimelineProperties() {
        position = tl.Position().map(|p| p.Duration / 10_000).unwrap_or(0);
        duration = tl.EndTime().map(|p| p.Duration / 10_000).unwrap_or(0);
        stamp = tl.LastUpdatedTime().map(|t| (t.UniversalTime - 116_444_736_000_000_000) / 10_000).unwrap_or(0);
    }
    Some(NowPlaying {
        title,
        artist: props.Artist().map(|a| a.to_string()).unwrap_or_default(),
        playing,
        art: None,
        app: s.SourceAppUserModelId().map(|a| a.to_string()).unwrap_or_default(),
        position,
        stamp,
        duration,
    })
}

/// Checks about twice a second; `on_change` gets the new state (None = nothing playing).
pub fn watch(on_change: impl Fn(Option<NowPlaying>) + Send + 'static) {
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let mut last: Option<NowPlaying> = None;
        let mut last_art: Option<String> = None;
        loop {
            let now = read();
            if now != last {
                let song_changed = now.as_ref().map(|n| (&n.title, &n.artist)) != last.as_ref().map(|l| (&l.title, &l.artist));
                let mut send = now.clone();
                if let Some(n) = send.as_mut() {
                    if song_changed || last_art.is_none() {
                        // art often arrives a moment after the title; retry briefly
                        let mut art = None;
                        for _ in 0..4 {
                            art = session().and_then(|s| art_of(&s));
                            if art.is_some() {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(250));
                        }
                        last_art = art.clone();
                    }
                    n.art = last_art.clone();
                } else {
                    last_art = None;
                }
                on_change(send);
                last = now;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// The current song right now (developer check).
pub fn read_now() -> Option<NowPlaying> {
    read()
}

/// Prints every field of the current song (developer check).
pub fn dump() {
    if let Ok(all) = manager().and_then(|m| m.GetSessions()) {
        for s in all {
            let status = s.GetPlaybackInfo().and_then(|i| i.PlaybackStatus()).map(|st| format!("{st:?}")).unwrap_or_default();
            let props = s.TryGetMediaPropertiesAsync().and_then(|a| a.get());
            let (t, a) = props.map(|p| (p.Title().map(|h| h.to_string()).unwrap_or_default(), p.Artist().map(|h| h.to_string()).unwrap_or_default())).unwrap_or_default();
            let pos = s.GetTimelineProperties().and_then(|tl| tl.Position()).map(|p| p.Duration / 10_000_000).unwrap_or(0);
            println!("session: {} | {t:?} by {a:?} | {status} | at {pos}s", s.SourceAppUserModelId().map(|h| h.to_string()).unwrap_or_default());
        }
    }
    let Some(s) = session() else { return println!("nothing playing") };
    let Ok(p) = s.TryGetMediaPropertiesAsync().and_then(|a| a.get()) else { return };
    let txt = |r: windows::core::Result<windows::core::HSTRING>| r.map(|h| h.to_string()).unwrap_or_default();
    println!("title: {:?}", txt(p.Title()));
    println!("artist: {:?}", txt(p.Artist()));
    println!("subtitle: {:?}", txt(p.Subtitle()));
    println!("album: {:?}", txt(p.AlbumTitle()));
    println!("album artist: {:?}", txt(p.AlbumArtist()));
    println!("app: {:?}", s.SourceAppUserModelId().map(|h| h.to_string()).unwrap_or_default());
}

pub fn control(action: &str) {
    let Some(s) = session() else { return };
    let _ = match action {
        "next" => s.TrySkipNextAsync().and_then(|a| a.get()),
        "previous" => s.TrySkipPreviousAsync().and_then(|a| a.get()),
        _ => s.TryTogglePlayPauseAsync().and_then(|a| a.get()),
    };
}
