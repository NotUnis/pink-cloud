//! Safe start: if Pink Cloud ever crashes, your desktop doesn't stay broken.
//!
//! When Pink Cloud starts, a small helper copy of it (`--watch <pid>`, run from a copy in the
//! temp folder so updates can still replace the real one) waits in the background. Pink Cloud
//! writes "clean" to the session file when it closes normally. If it goes away without doing
//! that, it crashed: the helper puts the Windows taskbar and folders back, writes a crash
//! report, and then:
//!   - after a long run, starts Pink Cloud again (which offers to send the report);
//!   - right after starting (probably a bad update), asks: try again, go back to the version
//!     you had before the last update, or leave it closed.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// A crash this soon after starting counts as "it doesn't start" rather than "it crashed once".
const EARLY_SECS: u64 = 120;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

fn session_file() -> PathBuf {
    temp("PinkCloud-session.txt")
}

/// The crash report a crash leaves behind, until it's sent or dismissed.
pub fn crash_file() -> PathBuf {
    temp("PinkCloud-crash.txt")
}

/// Script errors and other notes from this and earlier runs (kept short), for bug reports.
pub fn log_file() -> PathBuf {
    temp("PinkCloud-log.txt")
}

fn install_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_default()
}

/// The version you had before the last update, kept so you can go back to it.
fn previous_exe() -> PathBuf {
    install_dir().join("pink-cloud-previous.exe")
}

fn previous_version_file() -> PathBuf {
    install_dir().join("previous-version.txt")
}

/// An update you went back from; it isn't offered again (a newer one is).
pub fn skipped_version() -> Option<String> {
    std::fs::read_to_string(install_dir().join("skip-version.txt")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Adds a line to the log, keeping only the last part of it.
pub fn log(line: &str) {
    let path = log_file();
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.len() > 60_000 {
        let cut = text.len() - 40_000;
        let cut = text[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
        text = text[cut..].to_string();
    }
    text.push_str(&format!("{} {}\n", now(), line));
    let _ = std::fs::write(path, text);
}

/// The last `n` lines of the log.
pub fn recent_log(n: usize) -> String {
    let text = std::fs::read_to_string(log_file()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Called as Pink Cloud starts: notes this run and starts the helper that watches it.
pub fn start(version: &str) {
    let pid = std::process::id();
    let _ = std::fs::write(session_file(), format!("{pid} running {version} {}", now()));
    // a panic leaves a note saying where, for the report
    std::panic::set_hook(Box::new(|info| {
        log(&format!("panic: {info}"));
    }));
    let Ok(exe) = std::env::current_exe() else { return };
    let helper = temp("PinkCloud-watch.exe");
    let from = if std::fs::copy(&exe, &helper).is_ok() { helper } else { exe.clone() };
    let _ = Command::new(from).arg("--watch").arg(pid.to_string()).arg(&exe).creation_flags(0x0800_0000).spawn();
}

/// Called whenever Pink Cloud closes on purpose (quitting, updating): no crash.
pub fn clean_exit() {
    let _ = std::fs::write(session_file(), format!("{} clean", std::process::id()));
}

/// Before an update: keep this version, so you can go back if the new one doesn't work.
pub fn keep_previous(version: &str) {
    if let Ok(exe) = std::env::current_exe() {
        if std::fs::copy(&exe, previous_exe()).is_ok() {
            let _ = std::fs::write(previous_version_file(), version);
        }
    }
}

fn message(text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let t: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe { MessageBoxW(None, PCWSTR(t.as_ptr()), w!("Pink Cloud"), flags | MB_TOPMOST | MB_SETFOREGROUND) }
}

/// The helper: `pink-cloud --watch <pid>`. Waits for Pink Cloud to close and cleans up after a crash.
pub fn watch(pid: u32, real_exe: PathBuf) {
    unsafe {
        let Ok(proc) = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return };
        WaitForSingleObject(proc, INFINITE);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(proc, &mut code);
        let _ = CloseHandle(proc);
        let session = std::fs::read_to_string(session_file()).unwrap_or_default();
        let mut parts = session.split_whitespace();
        let (who, what) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        // closed on purpose, or Windows is shutting down / signing out
        if who != pid.to_string() || what != "running" || GetSystemMetrics(SM_SHUTTINGDOWN) != 0 {
            return;
        }
        let version = parts.next().unwrap_or("?").to_string();
        let started: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let ran = now().saturating_sub(started);

        // put the desktop back the way Windows has it
        crate::shell::restore_taskbar();
        crate::folders::register(false);
        let report = format!(
            "Pink Cloud {version} closed unexpectedly after running {ran} s (exit code {code:#x}).\n\nLast log lines:\n{}",
            recent_log(25)
        );
        let _ = std::fs::write(crash_file(), &report);
        let _ = std::fs::write(session_file(), format!("{pid} crashed"));
        log(&format!("crash: {version} after {ran} s, exit code {code:#x}"));

        if ran >= EARLY_SECS {
            // it was working fine for a while: just bring it back (once every 10 minutes at most)
            let last = temp("PinkCloud-restarted.txt");
            let recent = std::fs::read_to_string(&last).ok().and_then(|s| s.trim().parse::<u64>().ok()).map(|t| now() - t < 600).unwrap_or(false);
            if !recent {
                let _ = std::fs::write(&last, now().to_string());
                let _ = Command::new(&real_exe).arg("--crashed").spawn();
                return;
            }
        }

        let prev = std::fs::read_to_string(previous_version_file()).ok().map(|s| s.trim().to_string()).filter(|v| !v.is_empty() && *v != version && previous_exe().exists());
        let text = match &prev {
            Some(p) => format!(
                "Pink Cloud {version} stopped working. Your normal Windows taskbar is back.\n\n\
                 Yes: try starting Pink Cloud {version} again\n\
                 No: go back to Pink Cloud {p}, the version you had before\n\
                 Cancel: leave Pink Cloud closed for now\n\n\
                 A report was saved; Pink Cloud will offer to send it."
            ),
            None => format!(
                "Pink Cloud {version} stopped working. Your normal Windows taskbar is back.\n\n\
                 Yes: start Pink Cloud again\n\
                 No: leave it closed for now\n\n\
                 A report was saved; Pink Cloud will offer to send it."
            ),
        };
        let pick = message(&text, if prev.is_some() { MB_YESNOCANCEL | MB_ICONWARNING } else { MB_YESNO | MB_ICONWARNING });
        if pick == IDYES {
            let _ = Command::new(&real_exe).arg("--crashed").spawn();
        } else if pick == IDNO {
            if let Some(p) = prev {
                go_back(&real_exe, &version, &p);
            }
        }
    }
}

/// Swaps the broken version for the previous one and starts it.
fn go_back(real_exe: &PathBuf, broken: &str, prev: &str) {
    let aside = real_exe.with_file_name("pink-cloud-broken.exe");
    let _ = std::fs::remove_file(&aside);
    let swapped = std::fs::rename(real_exe, &aside).is_ok() && std::fs::copy(previous_exe(), real_exe).is_ok();
    if !swapped {
        // put it back the way it was
        if !real_exe.exists() {
            let _ = std::fs::rename(&aside, real_exe);
        }
        message("Couldn't go back to the previous version. Download Pink Cloud again from its GitHub page.", MB_OK | MB_ICONERROR);
        return;
    }
    let _ = std::fs::write(real_exe.with_file_name("skip-version.txt"), broken);
    log(&format!("went back from {broken} to {prev}"));
    let _ = Command::new(real_exe).arg("--crashed").spawn();
}
