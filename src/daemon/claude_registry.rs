//! Claude Code PID-registry attribution (Layer 1 of per-terminal session
//! attribution): claude ≥2.1.200 maintains `~/.claude/sessions/<pid>.json` —
//! `{pid, sessionId, cwd, startedAt, procStart, status, updatedAt, …}` —
//! LIVE-updated (~100ms) when an in-TUI `/clear` rotates or `/resume`
//! SWITCHES the conversation. Older claudes (2.1.91-era, the WSL population)
//! write a launch snapshot only. Reading the file for a terminal's tracked
//! claude pid gives an exact, zero-config live session id.
//!
//! Trust gates (staged ground truth, 2026-07-05, byte-verified):
//! - `-p` runs leave no file; HARD KILLS leave a STALE file (no cleanup) —
//!   so a file is only believed for a pid the caller has independently
//!   proven to be a LIVE claude process (Win32: a Toolhelp descendant of the
//!   terminal; WSL: `\\wsl$\<distro>\proc\<pid>` exists), AND
//! - `startedAt` (unix-epoch UTC ms of launch; present in BOTH schema
//!   generations, measured ~0.7s from the real process start) must match
//!   the process start time within `START_SLACK_MS` — the pid-reuse gate.
//!   (`procStart` also exists but is .NET ticks in LOCAL time — a DST
//!   hazard — and 2.1.200 sometimes writes it empty; it is deliberately
//!   NOT used.)
//! - Unparseable JSON ⇒ None (mid-write snapshot; the caller retries next
//!   tick). No start evidence on either side ⇒ None (never guess).

use std::path::{Path, PathBuf};

use uuid::Uuid;

/// |registry startedAt − real process start| tolerance. Measured skew is
/// ~0.7s (node bootstrap between process start and the registry write);
/// 15s is pid-reuse-proof at any realistic churn while immune to clock
/// jitter and file-flush latency.
const START_SLACK_MS: u64 = 15_000;

/// Unix epoch in FILETIME 100ns ticks.
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

/// One parsed registry entry (lenient: only the fields the gates read).
#[derive(Debug, Clone, PartialEq)]
pub struct RegEntry {
    pub pid: u32,
    pub session_id: Uuid,
    pub cwd: Option<String>,
    /// unix-epoch UTC ms of launch (both schema generations).
    pub started_at_ms: Option<u64>,
}

#[derive(serde::Deserialize)]
struct RawEntry {
    #[serde(default)]
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(rename = "startedAt", default)]
    started_at: Option<u64>,
}

/// Parse one registry file body. None = malformed/mid-write (retry later).
pub fn parse_entry(bytes: &[u8]) -> Option<RegEntry> {
    let raw: RawEntry = serde_json::from_slice(bytes).ok()?;
    let session_id = Uuid::parse_str(raw.session_id.trim()).ok()?;
    Some(RegEntry {
        pid: raw.pid,
        session_id,
        cwd: raw.cwd,
        started_at_ms: raw.started_at,
    })
}

/// The local registry dir: `~/.claude/sessions`.
pub fn local_sessions_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("sessions"))
}

fn filetime_to_unix_ms(ft: u64) -> u64 {
    ft.saturating_sub(FILETIME_UNIX_EPOCH) / 10_000
}

/// Start-evidence gate: does this entry describe a process started at
/// `start_ft` (Win32 FILETIME, UTC)? `startedAt` decides; absent ⇒ false
/// (never guess).
pub fn start_matches(entry: &RegEntry, start_ft: u64) -> bool {
    entry
        .started_at_ms
        .is_some_and(|sa| sa.abs_diff(filetime_to_unix_ms(start_ft)) <= START_SLACK_MS)
}

/// Layer-1 local read: the live session id for a claude process the caller
/// has already proven alive (a tracked descendant). `start_ft` is the
/// process's Win32 start FILETIME; None ⇒ ungateable ⇒ None (never guess).
pub fn live_session_for_pid(dir: &Path, pid: u32, start_ft: Option<u64>) -> Option<Uuid> {
    let start_ft = start_ft?;
    let bytes = std::fs::read(dir.join(format!("{pid}.json"))).ok()?;
    let entry = parse_entry(&bytes)?;
    if entry.pid != 0 && entry.pid != pid {
        return None; // file body disagrees with its own name — never guess
    }
    start_matches(&entry, start_ft).then_some(entry.session_id)
}

// ───────────────────────────── WSL registry scan ─────────────────────────────

/// Registered WSL distro name for a family's `distro` field (None = the
/// default distro, resolved from HKCU\...\Lxss). Cached: the registry read
/// runs once per daemon lifetime (changing the default distro requires a
/// wsl restart that respawns terminals anyway).
pub fn resolve_distro(distro: Option<&str>) -> Option<String> {
    if let Some(d) = distro {
        return Some(d.to_string());
    }
    static DEFAULT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DEFAULT
        .get_or_init(|| {
            let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
            let lxss = hkcu
                .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Lxss")
                .ok()?;
            let guid: String = lxss.get_value("DefaultDistribution").ok()?;
            let sub = lxss.open_subkey(&guid).ok()?;
            sub.get_value("DistributionName").ok()
        })
        .clone()
}

/// `\\wsl$\<distro>` + a POSIX path, as a Windows UNC PathBuf.
fn wsl_unc(distro: &str, posix: &str) -> PathBuf {
    PathBuf::from(format!("\\\\wsl$\\{distro}{}", posix.replace('/', "\\")))
}

/// WSL Layer-1: the ONE live claude in `cwd` per the distro's registry —
/// exactly-one or nothing (never guess). Liveness = `/proc/<pid>` exists
/// through the same UNC mount (2.1.91-era registries never live-update and
/// hard kills leave stale files; a dead pid's proc dir is gone, and pid
/// reuse onto a same-cwd claude would rewrite the file anyway).
/// Testable core: `sessions_dir` + `proc_root` injected.
pub fn wsl_live_claude_in(sessions_dir: &Path, proc_root: &Path, cwd: &str) -> Option<Uuid> {
    let rd = std::fs::read_dir(sessions_dir).ok()?;
    let mut hit: Option<Uuid> = None;
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Some(entry) = parse_entry(&bytes) else { continue };
        if entry.cwd.as_deref() != Some(cwd) {
            continue;
        }
        if entry.pid == 0 || !proc_root.join(entry.pid.to_string()).is_dir() {
            continue; // stale file from a hard kill
        }
        if hit.is_some() {
            return None; // two live claudes in one cwd — ambiguous
        }
        hit = Some(entry.session_id);
    }
    hit
}

/// Production entry: registry + proc through `\\wsl$`. `home` comes from the
/// terminal's init hook (POSIX-verbatim).
pub fn wsl_live_claude(distro: Option<&str>, home: &str, cwd: &str) -> Option<Uuid> {
    let distro = resolve_distro(distro)?;
    if home.is_empty() || !home.starts_with('/') {
        return None;
    }
    let sessions = wsl_unc(&distro, &format!("{home}/.claude/sessions"));
    let proc_root = wsl_unc(&distro, "/proc");
    wsl_live_claude_in(&sessions, &proc_root, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tc_reg_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Registry parse: both schema generations, malformed bodies ⇒ None.
    #[test]
    fn parse_entry_schemas() {
        let sid = Uuid::new_v4();
        // 2.1.200 live schema (extra fields ignored, incl. the sometimes-
        // empty procStart captured live).
        let e = parse_entry(
            format!(
                r#"{{"pid":67604,"sessionId":"{sid}","cwd":"C:\\proj","startedAt":1783003492841,"procStart":639185858922518110,"version":"2.1.200","status":"busy","updatedAt":1783003492900}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(e.pid, 67604);
        assert_eq!(e.session_id, sid);
        assert_eq!(e.started_at_ms, Some(1783003492841));
        assert_eq!(e.cwd.as_deref(), Some("C:\\proj"));
        // 2.1.91 launch snapshot (no procStart/updatedAt/status).
        let e = parse_entry(
            format!(
                r#"{{"pid":4242,"sessionId":"{sid}","cwd":"/home/z/p","startedAt":1783003492841,"kind":"interactive","entrypoint":"cli"}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(e.pid, 4242);
        assert_eq!(e.cwd.as_deref(), Some("/home/z/p"));
        // Malformed: truncated JSON / bad uuid / empty ⇒ None (retry next tick).
        assert!(parse_entry(br#"{"pid":1,"sessionId":"2c17"#).is_none());
        assert!(parse_entry(br#"{"pid":1,"sessionId":"not-a-uuid"}"#).is_none());
        assert!(parse_entry(b"").is_none());
    }

    /// The startedAt gate against a real captured pair: registry
    /// startedAt=1783218577679 vs process-start UTC .NET ticks
    /// 639188153769702503 (≈709ms apart — inside slack); a shifted value is
    /// rejected; no evidence is rejected.
    #[test]
    fn start_gate_uses_started_at() {
        let sid = Uuid::new_v4();
        // FILETIME = .NET UTC ticks − the 1601-epoch offset.
        let start_ft: u64 = 639188153769702503 - 504_911_232_000_000_000;
        let entry = RegEntry {
            pid: 1,
            session_id: sid,
            cwd: None,
            started_at_ms: Some(1783218577679),
        };
        assert!(start_matches(&entry, start_ft));
        // One minute off ⇒ stale file from a reused pid: rejected.
        let stale = RegEntry {
            started_at_ms: Some(1783218577679 - 60_000),
            ..entry.clone()
        };
        assert!(!start_matches(&stale, start_ft));
        // No start evidence at all ⇒ rejected (never guess).
        let bare = RegEntry {
            started_at_ms: None,
            ..entry
        };
        assert!(!start_matches(&bare, start_ft));
    }

    /// live_session_for_pid: reads `<pid>.json`, applies the gates, rejects
    /// a file whose body names a different pid, requires a start time.
    #[test]
    fn live_session_for_pid_gates() {
        let dir = temp("pid");
        let sid = Uuid::new_v4();
        let start_ft: u64 = 639188153769702503 - 504_911_232_000_000_000;
        write(
            &dir,
            "500.json",
            &format!(r#"{{"pid":500,"sessionId":"{sid}","startedAt":1783218577679}}"#),
        );
        assert_eq!(live_session_for_pid(&dir, 500, Some(start_ft)), Some(sid));
        // Ungateable process start ⇒ None.
        assert_eq!(live_session_for_pid(&dir, 500, None), None);
        // Missing file ⇒ None.
        assert_eq!(live_session_for_pid(&dir, 501, Some(start_ft)), None);
        // Body pid disagrees with the filename ⇒ None.
        write(
            &dir,
            "502.json",
            &format!(r#"{{"pid":999,"sessionId":"{sid}","startedAt":1783218577679}}"#),
        );
        assert_eq!(live_session_for_pid(&dir, 502, Some(start_ft)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WSL scan: exactly-one live cwd-match wins; a stale entry (no proc
    /// dir) is ignored; two live matches ⇒ None (ambiguous, never guess).
    #[test]
    fn wsl_scan_exactly_one_live() {
        let root = temp("wsl");
        let sessions = root.join("sessions");
        let proc = root.join("proc");
        let live = Uuid::new_v4();
        let stale = Uuid::new_v4();
        std::fs::create_dir_all(proc.join("100")).unwrap();
        write(
            &sessions,
            "100.json",
            &format!(r#"{{"pid":100,"sessionId":"{live}","cwd":"/home/z/p","startedAt":1}}"#),
        );
        // Stale: registry file present, /proc gone (hard kill).
        write(
            &sessions,
            "101.json",
            &format!(r#"{{"pid":101,"sessionId":"{stale}","cwd":"/home/z/p","startedAt":1}}"#),
        );
        // Different cwd: never a candidate.
        std::fs::create_dir_all(proc.join("102")).unwrap();
        write(
            &sessions,
            "102.json",
            &format!(
                r#"{{"pid":102,"sessionId":"{}","cwd":"/home/z/other","startedAt":1}}"#,
                Uuid::new_v4()
            ),
        );
        assert_eq!(wsl_live_claude_in(&sessions, &proc, "/home/z/p"), Some(live));
        // A second LIVE claude in the same cwd flips it ambiguous.
        std::fs::create_dir_all(proc.join("101")).unwrap();
        assert_eq!(wsl_live_claude_in(&sessions, &proc, "/home/z/p"), None);
        // Missing dir ⇒ None.
        assert_eq!(
            wsl_live_claude_in(&root.join("nope"), &proc, "/home/z/p"),
            None
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
