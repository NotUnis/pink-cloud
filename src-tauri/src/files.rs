//! What Pink Cloud Files needs from Windows: folders, drives and file actions.

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;
use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

#[derive(Serialize)]
pub struct Entry {
    name: String,
    path: String,
    dir: bool,
    size: u64,
    modified: u64,
    hidden: bool,
}

pub fn list_dir(path: &str) -> Result<Vec<Entry>, String> {
    let rd = std::fs::read_dir(path).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        let attrs = md.file_attributes();
        out.push(Entry {
            name: e.file_name().to_string_lossy().to_string(),
            path: e.path().to_string_lossy().to_string(),
            dir: md.is_dir(),
            size: if md.is_dir() { 0 } else { md.len() },
            modified: md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0),
            // hidden (2) or system (4)
            hidden: attrs & 0x6 != 0,
        });
    }
    Ok(out)
}

#[derive(Serialize)]
pub struct Place {
    name: String,
    path: String,
    icon: String,
}

fn known(id: &GUID) -> Option<String> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

/// Home, Desktop, Downloads, ... at their real locations (OneDrive can move them).
pub fn places() -> Vec<Place> {
    [
        ("Home", &FOLDERID_Profile, "ph-house-line"),
        ("Desktop", &FOLDERID_Desktop, "ph-desktop"),
        ("Downloads", &FOLDERID_Downloads, "ph-download-simple"),
        ("Documents", &FOLDERID_Documents, "ph-file-text"),
        ("Pictures", &FOLDERID_Pictures, "ph-image"),
        ("Music", &FOLDERID_Music, "ph-music-notes"),
        ("Videos", &FOLDERID_Videos, "ph-film-strip"),
    ]
    .iter()
    .filter_map(|(name, id, icon)| known(id).map(|path| Place { name: name.to_string(), path, icon: icon.to_string() }))
    .collect()
}

#[derive(Serialize)]
pub struct Drive {
    path: String,
    label: String,
    total: u64,
    free: u64,
    kind: String,
}

pub fn drives() -> Vec<Drive> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| unsafe {
            let root = format!("{}:\\", (b'A' + i) as char);
            let r = wide(&root);
            let kind = match GetDriveTypeW(PCWSTR(r.as_ptr())) {
                2 => "usb",
                3 => "disk",
                4 => "network",
                5 => "disc",
                _ => return None,
            };
            let mut label = [0u16; 261];
            let _ = GetVolumeInformationW(PCWSTR(r.as_ptr()), Some(&mut label), None, None, None, None);
            let (mut free, mut total) = (0u64, 0u64);
            let _ = GetDiskFreeSpaceExW(PCWSTR(r.as_ptr()), None, Some(&mut total), Some(&mut free));
            let n = label.iter().position(|&c| c == 0).unwrap_or(0);
            let mut name = String::from_utf16_lossy(&label[..n]);
            if name.is_empty() {
                name = if kind == "usb" { "USB drive".into() } else { "Local Disk".into() };
            }
            Some(Drive { path: root, label: name, total, free, kind: kind.into() })
        })
        .collect()
}

fn shell_verb(path: &str, verb: PCWSTR) {
    let p = wide(path);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        lpVerb: verb,
        lpFile: PCWSTR(p.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        let _ = ShellExecuteExW(&mut info);
    }
}

pub fn open_with(path: &str) {
    shell_verb(path, w!("openas"));
}

pub fn properties(path: &str) {
    shell_verb(path, w!("properties"));
}

pub fn show_in_explorer(path: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{path}")).spawn();
}

pub fn rename(path: &str, new_name: &str) -> Result<String, String> {
    let name = new_name.trim();
    if name.is_empty() || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return Err("A name can't be empty or contain \\ / : * ? \" < > |".into());
    }
    let from = Path::new(path);
    let to = from.parent().unwrap_or(Path::new("")).join(name);
    if to.exists() && to != from {
        return Err(format!("There's already something called \"{name}\" here."));
    }
    std::fs::rename(from, &to).map_err(|e| e.to_string())?;
    Ok(to.to_string_lossy().to_string())
}

pub fn new_folder(dir: &str) -> Result<String, String> {
    let base = Path::new(dir);
    let mut target: PathBuf = base.join("New folder");
    let mut n = 2;
    while target.exists() {
        target = base.join(format!("New folder ({n})"));
        n += 1;
    }
    std::fs::create_dir(&target).map_err(|e| e.to_string())?;
    Ok(target.to_string_lossy().to_string())
}

// ---------- clipboard: works with Explorer and other apps ----------

const CF_HDROP: u32 = 15;
const DROPEFFECT_COPY: u32 = 1;
const DROPEFFECT_MOVE: u32 = 2;

fn double_null(paths: &[String]) -> Vec<u16> {
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        list.extend(p.encode_utf16());
        list.push(0);
    }
    list.push(0);
    list
}

unsafe fn global_with(bytes: &[u8]) -> Option<windows::Win32::Foundation::HGLOBAL> {
    use windows::Win32::System::Memory::*;
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
    let p = GlobalLock(h) as *mut u8;
    if p.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    let _ = GlobalUnlock(h);
    Some(h)
}

/// Puts files on the clipboard the way Explorer does, so you can paste them anywhere.
pub fn clipboard_set(paths: &[String], cut: bool) -> Result<(), String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::*;
    unsafe {
        // DROPFILES header (20 bytes: offset of the list, a point, two flags), then the list
        let list = double_null(paths);
        let mut bytes = Vec::with_capacity(20 + list.len() * 2);
        bytes.extend(20u32.to_le_bytes());
        bytes.extend([0u8; 12]);
        bytes.extend(1u32.to_le_bytes()); // wide characters
        bytes.extend(list.iter().flat_map(|c| c.to_le_bytes()));
        let files = global_with(&bytes).ok_or("Couldn't use the clipboard.")?;
        let effect = global_with(&(if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY }).to_le_bytes()).ok_or("Couldn't use the clipboard.")?;
        OpenClipboard(None).map_err(|_| "The clipboard is busy. Try again.")?;
        let _ = EmptyClipboard();
        let _ = SetClipboardData(CF_HDROP, Some(HANDLE(files.0)));
        let _ = SetClipboardData(RegisterClipboardFormatW(w!("Preferred DropEffect")), Some(HANDLE(effect.0)));
        let _ = CloseClipboard();
    }
    Ok(())
}

/// Files on the clipboard (copied here or in Explorer), and whether they were cut.
pub fn clipboard_get() -> (Vec<String>, bool) {
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::*;
    let mut out = Vec::new();
    let mut cut = false;
    unsafe {
        if OpenClipboard(None).is_err() {
            return (out, false);
        }
        if let Ok(h) = GetClipboardData(CF_HDROP) {
            let drop = HDROP(h.0);
            let n = DragQueryFileW(drop, u32::MAX, None);
            for i in 0..n {
                let mut buf = vec![0u16; DragQueryFileW(drop, i, None) as usize + 1];
                let len = DragQueryFileW(drop, i, Some(&mut buf));
                out.push(String::from_utf16_lossy(&buf[..len as usize]));
            }
        }
        if let Ok(h) = GetClipboardData(RegisterClipboardFormatW(w!("Preferred DropEffect"))) {
            let g = windows::Win32::Foundation::HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u32;
            if !p.is_null() {
                cut = *p & DROPEFFECT_MOVE != 0;
                let _ = GlobalUnlock(g);
            }
        }
        let _ = CloseClipboard();
    }
    (out, cut)
}

/// Copies or moves files into `dest`. Windows shows its own progress window for big jobs,
/// and pasting into the same folder makes "name - Copy" instead of overwriting.
pub fn paste(paths: &[String], dest: &str, cut: bool) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let from = double_null(paths);
    let to = double_null(&[dest.to_string()]);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: if cut { FO_MOVE } else { FO_COPY },
        pFrom: PCWSTR(from.as_ptr()),
        pTo: PCWSTR(to.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_RENAMEONCOLLISION).0 as u16,
        ..Default::default()
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 {
        return Err("Some items couldn't be pasted.".into());
    }
    if cut {
        // they've moved; the clipboard would point at nothing now
        unsafe {
            use windows::Win32::System::DataExchange::*;
            if OpenClipboard(None).is_ok() {
                let _ = EmptyClipboard();
                let _ = CloseClipboard();
            }
        }
    }
    Ok(())
}

/// Searches `root` and everything below it for names containing `query`.
/// Stops at 400 results or after 4 seconds, and skips hidden and system folders.
pub fn search(root: &str, query: &str) -> Vec<Entry> {
    let q = query.to_lowercase();
    let started = std::time::Instant::now();
    let mut out = Vec::new();
    let mut stack = vec![(PathBuf::from(root), 0u32)];
    while let Some((dir, depth)) = stack.pop() {
        if out.len() >= 400 || started.elapsed().as_secs() >= 4 {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            let attrs = md.file_attributes();
            if attrs & 0x6 != 0 {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if md.is_dir() && depth < 12 && attrs & 0x400 == 0 {
                stack.push((e.path(), depth + 1));
            }
            if name.to_lowercase().contains(&q) {
                out.push(Entry {
                    name,
                    path: e.path().to_string_lossy().to_string(),
                    dir: md.is_dir(),
                    size: if md.is_dir() { 0 } else { md.len() },
                    modified: md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0),
                    hidden: false,
                });
            }
        }
    }
    out
}

/// Files you opened recently (what Windows keeps in its Recent folder), newest first.
pub fn recent(limit: usize) -> Vec<Entry> {
    // reading shortcuts needs COM on this thread
    crate::shell::com_init();
    let Some(dir) = std::env::var("APPDATA").ok().map(|a| Path::new(&a).join(r"Microsoft\Windows\Recent")) else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut links: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    links.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = Vec::new();
    for (when, lnk) in links {
        let target = crate::shell::link_target(&lnk);
        let p = Path::new(&target);
        // only files that still exist; folders are skipped
        let Ok(md) = p.metadata() else { continue };
        if md.is_dir() || out.iter().any(|e: &Entry| e.path == target) {
            continue;
        }
        out.push(Entry {
            name: p.file_name().unwrap_or_default().to_string_lossy().to_string(),
            path: target.clone(),
            dir: false,
            size: md.len(),
            modified: when.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
            hidden: false,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Quick search for Start: your main folders only, a few results, fast.
pub fn quick_search(query: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for place in places().into_iter().filter(|p| p.icon != "ph-house-line") {
        out.extend(search_limited(&place.path, query, 6, 400));
        if out.len() >= 20 {
            break;
        }
    }
    out.truncate(20);
    out
}

fn search_limited(root: &str, query: &str, max_depth: u32, max_ms: u128) -> Vec<Entry> {
    let q = query.to_lowercase();
    let started = std::time::Instant::now();
    let mut out = Vec::new();
    let mut stack = vec![(PathBuf::from(root), 0u32)];
    while let Some((dir, depth)) = stack.pop() {
        if out.len() >= 12 || started.elapsed().as_millis() >= max_ms {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.file_attributes() & 0x406 != 0 {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if md.is_dir() {
                if depth < max_depth && !name.starts_with('.') && name != "node_modules" {
                    stack.push((e.path(), depth + 1));
                }
                continue;
            }
            if name.to_lowercase().contains(&q) {
                out.push(Entry {
                    name,
                    path: e.path().to_string_lossy().to_string(),
                    dir: false,
                    size: md.len(),
                    modified: md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0),
                    hidden: false,
                });
            }
        }
    }
    out
}

/// Moves items to the Recycle Bin (never deletes them outright), so they can be restored.
pub fn recycle(paths: &[String]) -> Result<(), String> {
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        list.extend(p.encode_utf16());
        list.push(0);
    }
    list.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(list.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 || op.fAnyOperationsAborted.as_bool() {
        return Err("Some items couldn't be moved to the Recycle Bin.".into());
    }
    Ok(())
}
