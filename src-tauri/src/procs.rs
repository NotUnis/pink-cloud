//! Pink Cloud's Task Manager: what's running, grouped by program, with how much CPU and memory
//! each one uses. Apps (they have a window), background apps, and Windows' own processes.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE};

use crate::{mixer, shell};

#[derive(Serialize, Clone)]
pub struct Task {
    /// The program's path in lowercase (or its name when Windows won't say): what end_app takes.
    pub key: String,
    pub name: String,
    pub exe: String,
    /// "app", "background" or "windows".
    pub kind: &'static str,
    /// Percent of the whole PC, like Windows' Task Manager.
    pub cpu: f64,
    /// Bytes of memory the program keeps to itself.
    pub memory: u64,
    /// How many processes this row adds up.
    pub count: u32,
    /// A window to bring up when you click the row.
    pub hwnd: Option<i64>,
    /// False for Pink Cloud itself and what Windows needs to keep running.
    pub can_end: bool,
    #[serde(skip)]
    pids: Vec<u32>,
}

#[derive(Serialize)]
pub struct Tasks {
    pub tasks: Vec<Task>,
    pub processes: usize,
    /// Seconds since the PC started.
    pub uptime: u64,
}

/// Each process's CPU time at the last look, and when that was.
static LAST: Mutex<Option<(Instant, HashMap<u32, u64>)>> = Mutex::new(None);

fn ticks(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// (process id, parent id, file name) of everything running.
fn snapshot() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                out.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..len])));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// (CPU time in 100 ns steps, private memory in bytes) for a process, if Windows lets us look.
fn measure(pid: u32) -> Option<(u64, u64)> {
    unsafe {
        let p: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut a, mut b, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let cpu = if GetProcessTimes(p, &mut a, &mut b, &mut kernel, &mut user).is_ok() { ticks(kernel) + ticks(user) } else { 0 };
        let mut mem = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
        let got = K32GetProcessMemoryInfo(p, &mut mem as *mut _ as *mut PROCESS_MEMORY_COUNTERS, mem.cb).as_bool();
        let _ = CloseHandle(p);
        Some((cpu, if got { mem.PrivateUsage as u64 } else { 0 }))
    }
}

/// Everything running right now, grouped by program.
pub fn list() -> Tasks {
    let own_exe = std::env::current_exe().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into()).to_lowercase() + "\\";
    let procs = snapshot();
    let parents: HashMap<u32, u32> = procs.iter().map(|p| (p.0, p.1)).collect();
    let names: HashMap<u32, &String> = procs.iter().map(|p| (p.0, &p.2)).collect();
    let is_helper = |name: &str| name.eq_ignore_ascii_case("msedgewebview2.exe");

    // which programs have a window on the taskbar
    let mut windows: HashMap<String, i64> = HashMap::new();
    for w in shell::list_windows(u32::MAX) {
        let pid = shell::real_pid(w.hwnd);
        let exe = shell::exe_of(pid).to_lowercase();
        if !exe.is_empty() {
            windows.entry(exe).or_insert(w.hwnd);
        }
    }

    let mut guard = LAST.lock().unwrap();
    let previous = guard.take();
    let (elapsed, before) = match &previous {
        Some((at, map)) => (at.elapsed().as_secs_f64(), Some(map)),
        None => (0.0, None),
    };
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;

    let mut times: HashMap<u32, u64> = HashMap::new();
    let mut rows: HashMap<String, Task> = HashMap::new();
    for (pid, _, file) in &procs {
        if *pid == 0 {
            continue; // the "idle" time, not a program
        }
        let mut exe = shell::exe_of(*pid);
        // a web view helper counts for the app that started it
        if is_helper(file) {
            let mut at = *pid;
            for _ in 0..6 {
                let Some(&up) = parents.get(&at) else { break };
                at = up;
                match names.get(&at) {
                    Some(n) if !is_helper(n) => {
                        exe = shell::exe_of(at);
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
        let key = if exe.is_empty() { file.to_lowercase() } else { exe.to_lowercase() };
        let (cpu, memory) = measure(*pid).unwrap_or((0, 0));
        times.insert(*pid, cpu);
        let used = before
            .and_then(|b| b.get(pid))
            .filter(|_| elapsed > 0.05)
            .map(|&was| cpu.saturating_sub(was) as f64 / 10_000_000.0 / elapsed / cores * 100.0)
            .unwrap_or(0.0);
        let row = rows.entry(key.clone()).or_insert_with(|| {
            let hwnd = windows.get(&key).copied();
            let mine = key == own_exe;
            let system = key.starts_with(&windir) || exe.is_empty();
            let kind = if mine || hwnd.is_some() { "app" } else if system { "windows" } else { "background" };
            let name = if mine {
                "Pink Cloud".to_string()
            } else if exe.is_empty() {
                file.trim_end_matches(".exe").to_string()
            } else {
                mixer::name_of(&exe)
            };
            let can_end = !mine && !exe.is_empty() && kind != "windows" && !shell::never_end(&key);
            Task { key: key.clone(), name, exe: exe.clone(), kind, cpu: 0.0, memory: 0, count: 0, hwnd: if mine { None } else { hwnd }, can_end, pids: Vec::new() }
        });
        row.cpu += used;
        row.memory += memory;
        row.count += 1;
        row.pids.push(*pid);
    }
    *guard = Some((Instant::now(), times));
    drop(guard);

    let mut tasks: Vec<Task> = rows.into_values().collect();
    for t in &mut tasks {
        t.cpu = t.cpu.clamp(0.0, 100.0);
    }
    tasks.sort_by_key(|t| t.name.to_lowercase());
    Tasks { tasks, processes: procs.len().saturating_sub(1), uptime: unsafe { GetTickCount64() } / 1000 }
}

/// Force-closes every process of a program. Looks it up fresh, so a stale screen can't end the
/// wrong thing and what mustn't be ended stays safe. False if nothing could be ended.
pub fn end_app(key: &str) -> bool {
    let Some(task) = list().tasks.into_iter().find(|t| t.key == key && t.can_end) else { return false };
    let mut any = false;
    for pid in task.pids {
        unsafe {
            if let Ok(p) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                any |= TerminateProcess(p, 1).is_ok();
                let _ = CloseHandle(p);
            }
        }
    }
    any
}

/// Developer check: prints the biggest programs in each group.
pub fn self_test() {
    let _ = list();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let all = list();
    println!("{} processes, {} programs, up {} s", all.processes, all.tasks.len(), all.uptime);
    let mut tasks = all.tasks;
    tasks.sort_by(|a, b| b.memory.cmp(&a.memory));
    for kind in ["app", "background", "windows"] {
        for t in tasks.iter().filter(|t| t.kind == kind).take(8) {
            println!("{kind:10} | {:32} | {:5.1}% | {:6} MB | x{} | end: {} | window: {}", t.name, t.cpu, t.memory / 1_048_576, t.count, t.can_end, t.hwnd.is_some());
        }
    }
}
