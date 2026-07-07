//! Windows process introspection for session tracking.
//!
//! The `sysinfo` crate does not expose a live process cwd on Windows, so we read
//! it (and the command line) directly from the target's PEB via
//! NtQueryInformationProcess + ReadProcessMemory. Offsets are the documented x64
//! layout; `verify_self_offset` self-checks the 0x38 CurrentDirectory offset
//! against a known cwd before the tracker trusts it.

use std::path::PathBuf;

use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, IsWow64Process, OpenProcess, ProcessPowerThrottling, SetProcessInformation,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_VM_READ,
};

// x64 PEB / RTL_USER_PROCESS_PARAMETERS field offsets.
const PEB_PROCESS_PARAMETERS: usize = 0x20;
const PARAMS_CURRENT_DIRECTORY: usize = 0x38; // CURDIR.DosPath (UNICODE_STRING)
const PARAMS_COMMAND_LINE: usize = 0x70; // CommandLine (UNICODE_STRING)

/// Minimal mirror of PROCESS_BASIC_INFORMATION (x64) — avoids pulling the PEB
/// type and its feature gate; we only need PebBaseAddress.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ProcessBasicInfo {
    exit_status: i32,
    peb_base_address: usize,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

/// RAII guard for a process handle.
struct ProcHandle(HANDLE);
impl Drop for ProcHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open(pid: u32) -> Option<ProcHandle> {
    unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
        .ok()
        .map(ProcHandle)
    }
}

fn read_bytes(h: HANDLE, addr: usize, len: usize) -> Option<Vec<u8>> {
    if addr == 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    let mut read = 0usize;
    unsafe {
        ReadProcessMemory(
            h,
            addr as *const core::ffi::c_void,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            len,
            Some(&mut read),
        )
        .ok()?;
    }
    if read < len {
        return None;
    }
    Some(buf)
}

fn read_u64(h: HANDLE, addr: usize) -> Option<u64> {
    let b = read_bytes(h, addr, 8)?;
    Some(u64::from_le_bytes(b.try_into().ok()?))
}

fn read_u16(h: HANDLE, addr: usize) -> Option<u16> {
    let b = read_bytes(h, addr, 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

/// Read a UNICODE_STRING at `addr` (Length u16 @0, Buffer ptr @8) into a String.
fn read_unicode_string(h: HANDLE, addr: usize) -> Option<String> {
    let len_bytes = read_u16(h, addr)? as usize;
    if len_bytes == 0 || len_bytes > 64 * 1024 {
        return None;
    }
    let buffer = read_u64(h, addr + 8)? as usize;
    let raw = read_bytes(h, buffer, len_bytes)?;
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// True for a 32-bit (WOW64) process — its PEB layout differs, so we skip it.
fn is_wow64(h: HANDLE) -> bool {
    let mut wow = windows::core::BOOL(0);
    unsafe { IsWow64Process(h, &mut wow).is_ok() && wow.as_bool() }
}

fn process_params_ptr(h: HANDLE) -> Option<usize> {
    let mut pbi = ProcessBasicInfo::default();
    let status = unsafe {
        NtQueryInformationProcess(
            h,
            PROCESSINFOCLASS(0), // ProcessBasicInformation
            &mut pbi as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<ProcessBasicInfo>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status.is_err() || pbi.peb_base_address == 0 {
        return None;
    }
    read_u64(h, pbi.peb_base_address + PEB_PROCESS_PARAMETERS).map(|p| p as usize)
}

/// The live current directory of a process, read from its PEB. `None` on a
/// just-spawned/exited process or a WOW64 target (retry next tick).
pub fn read_process_cwd(pid: u32) -> Option<PathBuf> {
    let h = open(pid)?;
    if is_wow64(h.0) {
        return None;
    }
    let params = process_params_ptr(h.0)?;
    let s = read_unicode_string(h.0, params + PARAMS_CURRENT_DIRECTORY)?;
    // Same normalization as the OSC cwd path: trimming a drive root's
    // trailing separator would yield a drive-RELATIVE "C:" that resolves to
    // the daemon's own per-drive cwd at restore (the WINDOWS PATH TRAP the
    // OSC path was already patched for).
    super::session::normalize_win_path(s.trim_end_matches('\0'))
}

/// The command line of a process, split into argv-ish tokens.
pub fn read_process_cmdline(pid: u32) -> Option<Vec<String>> {
    let h = open(pid)?;
    if is_wow64(h.0) {
        return None;
    }
    let params = process_params_ptr(h.0)?;
    let line = read_unicode_string(h.0, params + PARAMS_COMMAND_LINE)?;
    Some(split_cmdline(&line))
}

/// Total CPU time (kernel + user) a process has consumed, in milliseconds.
/// Used by `--probe flood` to measure daemon cost during an output flood.
pub fn process_cpu_ms(pid: u32) -> Option<u64> {
    let h = open(pid)?;
    let mut create = windows::Win32::Foundation::FILETIME::default();
    let mut exit = windows::Win32::Foundation::FILETIME::default();
    let mut kernel = windows::Win32::Foundation::FILETIME::default();
    let mut user = windows::Win32::Foundation::FILETIME::default();
    unsafe {
        GetProcessTimes(h.0, &mut create, &mut exit, &mut kernel, &mut user).ok()?;
    }
    let ticks = |ft: windows::Win32::Foundation::FILETIME| {
        ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
    };
    Some((ticks(kernel) + ticks(user)) / 10_000)
}

/// Process creation time as a FILETIME (100ns ticks since 1601), for jsonl
/// birth-time correlation.
pub fn process_start_filetime(pid: u32) -> Option<u64> {
    let h = open(pid)?;
    let mut create = windows::Win32::Foundation::FILETIME::default();
    let mut exit = windows::Win32::Foundation::FILETIME::default();
    let mut kernel = windows::Win32::Foundation::FILETIME::default();
    let mut user = windows::Win32::Foundation::FILETIME::default();
    unsafe {
        GetProcessTimes(h.0, &mut create, &mut exit, &mut kernel, &mut user).ok()?;
    }
    Some(((create.dwHighDateTime as u64) << 32) | create.dwLowDateTime as u64)
}

/// Opt a process out of Windows power throttling (EcoQoS), pinning it at
/// High QoS. A windowless background process gets low QoS under any
/// foreground load: on hybrid CPUs Intel Thread Director parks it on E-cores
/// whenever something foreground (e.g. a game) owns the P-cores, which
/// measured ~1.75x the CPU-seconds for identical flood ingest (10.1s vs 5.7s
/// per 50MB, same binary, minutes apart — the 2026-07-02 "throughput
/// regression" was exactly this, not code). A foreground terminal's
/// conhost/shell get normal scheduling for free; sessions hosted by this
/// background broker deserve the same. StateMask 0 with the EXECUTION_SPEED
/// bit in ControlMask means "always High QoS", not "system decides". QoS is
/// per-process and NOT inherited, so user commands launched from a shell
/// keep default OS policy. Non-fatal on failure (pre-1709 Windows or a
/// vanished pid just keeps default QoS).
pub fn set_high_qos(pid: u32) -> bool {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SET_INFORMATION, false, pid) else {
            return false;
        };
        let guard = ProcHandle(h);
        SetProcessInformation(
            guard.0,
            ProcessPowerThrottling,
            &state as *const PROCESS_POWER_THROTTLING_STATE as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
        .is_ok()
    }
}

#[derive(Clone)]
pub struct ProcEntry {
    pub pid: u32,
    pub exe: String,
}

/// One system-wide process table snapshot: (pid, parent pid, exe name). Taken
/// once per tracker tick and reused for every session to keep idle CPU low.
pub fn snapshot_processes() -> Vec<(u32, u32, String)> {
    let mut all = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return all;
        };
        let guard = ProcHandle(snap);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(guard.0, &mut entry).is_ok() {
            loop {
                let exe = String::from_utf16_lossy(
                    &entry.szExeFile[..entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len())],
                );
                all.push((entry.th32ProcessID, entry.th32ParentProcessID, exe));
                if Process32NextW(guard.0, &mut entry).is_err() {
                    break;
                }
            }
        }
    }
    all
}

/// Descendants of `root_pid` (excluding the root) from a process table, BFS with
/// a depth cap that also guards against pid-reuse cycles.
pub fn descendants_of(table: &[(u32, u32, String)], root_pid: u32) -> Vec<ProcEntry> {
    let mut out = Vec::new();
    let mut frontier = vec![root_pid];
    let mut seen = vec![root_pid];
    for _ in 0..16 {
        let next: Vec<(u32, String)> = table
            .iter()
            .filter(|(pid, ppid, _)| frontier.contains(ppid) && !seen.contains(pid))
            .map(|(pid, _, exe)| (*pid, exe.clone()))
            .collect();
        if next.is_empty() {
            break;
        }
        for (pid, exe) in &next {
            seen.push(*pid);
            out.push(ProcEntry {
                pid: *pid,
                exe: exe.clone(),
            });
        }
        frontier = next.into_iter().map(|(pid, _)| pid).collect();
    }
    out
}

/// Split a Windows command line into tokens, honoring double quotes. Good enough
/// to spot `--resume <uuid>`; not a full CommandLineToArgvW.
fn split_cmdline(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// MANDATORY self-check: read our own process's cwd via the PEB path and compare
/// to the real cwd, validating the 0x38 CurrentDirectory offset on this build.
pub fn verify_self_offset() -> bool {
    let Some(real) = std::env::current_dir().ok() else {
        return false;
    };
    let Some(via_peb) = read_process_cwd(std::process::id()) else {
        return false;
    };
    let norm = |p: &std::path::Path| {
        p.to_string_lossy()
            .to_lowercase()
            .trim_end_matches(['\\', '/'])
            .to_string()
    };
    norm(&via_peb) == norm(&real)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peb_current_directory_offset_is_correct() {
        // Validates the 0x38 offset by round-tripping our own cwd.
        assert!(
            verify_self_offset(),
            "PEB CurrentDirectory offset self-check failed"
        );
    }

    #[test]
    fn splits_command_line() {
        assert_eq!(
            split_cmdline("claude --resume abc \"c:\\a b\\x\""),
            vec!["claude", "--resume", "abc", "c:\\a b\\x"]
        );
    }
}
