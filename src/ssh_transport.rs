//! Shared SFTP transport plumbing — the PURE half of the ssh-drop upload
//! machinery (#26), hoisted out of `gui/ssh_drop.rs` so the daemon's remote
//! CLI-resume probes (docs/remote-cli-resume-spec.md D4) can ride the exact
//! same argv translation, output parsers, and failure classifiers. GUI and
//! daemon are the SAME crate/exe — this is a file move, not a port; the
//! goldens moved with it and pin byte-identical behavior.
//!
//! Contents: `sftp_args`/`sftp_args_transport` (the §3.2 flag-translation
//! table), `parse_pwd`/`parse_ls1`/`parse_ls_l` (+ `parse_ls_l_full` for
//! callers that keep the path prefix and mode char), `classify_conn`/
//! `classify_file` (real captured stderr fixtures), `resolve_sftp`
//! (sibling-of-ssh, PATH fallback), and the tiny impure helpers both sides
//! want: `write_batch` + `run_sftp` (CREATE_NO_WINDOW, stdin null, optional
//! deadline watchdog that `TerminateProcess`es a wedged child — `kill_pid`
//! moved here too).
//!
//! egui-free by construction; tc.exe does not include this module.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ─────────────────────────── §3.2 argv synthesis ───────────────────────────

/// Build the sftp argv from the session's PERSISTED `meta.args` (user flags +
/// destination; the synthesized per-spawn tail never reaches meta). Ours are
/// PREPENDED (`-q -o BatchMode=yes` — first-occurrence-wins beats any user
/// BatchMode, inv. 3) and the automatic timeouts APPENDED (user overrides
/// win, the P6c keepalive rule). Destination rides VERBATIM so `~/.ssh/
/// config` aliases resolve exactly like the session's ssh (T5).
pub fn sftp_args(meta_args: &[String], batch: &str) -> Vec<String> {
    let mut out: Vec<String> = vec!["-q".into(), "-o".into(), "BatchMode=yes".into()];
    let mut login: Option<String> = None;
    if meta_args.is_empty() {
        // Unreachable for an Ssh-classified terminal (destination required);
        // degrade to a shape that fails honestly rather than panic.
        out.extend(["-b".into(), batch.to_string()]);
        return out;
    }
    let dest_idx = meta_args.len() - 1;
    let mut i = 0;
    while i < dest_idx {
        let a = &meta_args[i];
        // Value-taking OpenSSH flags (state::ssh_destination's set), with the
        // spec §3.2 translation table. Values may be glued (`-p2222`).
        let (flag, glued): (&str, Option<&str>) = if a.len() > 2 && a.starts_with('-') {
            (&a[..2], Some(&a[2..]))
        } else {
            (a.as_str(), None)
        };
        // Take this flag's value: glued rides in `a`, else the next token.
        let take_value = |i: &mut usize| -> Option<String> {
            if let Some(g) = glued {
                Some(g.to_string())
            } else if *i + 1 < dest_idx {
                *i += 1;
                Some(meta_args[*i].clone())
            } else {
                None // malformed (value would be the destination) — can't
                     // happen for an Ssh-classified argv
            }
        };
        match flag {
            // Renamed by sftp: -p (port) → -P; -l (login) folds into the
            // destination (sftp has no login flag; -l is bandwidth limit).
            "-p" => {
                if let Some(v) = take_value(&mut i) {
                    if glued.is_some() {
                        out.push(format!("-P{v}"));
                    } else {
                        out.push("-P".into());
                        out.push(v);
                    }
                }
            }
            "-l" => login = take_value(&mut i),
            // sftp lacks these short forms; the -o spellings are identical.
            "-b" => {
                if let Some(v) = take_value(&mut i) {
                    out.push("-o".into());
                    out.push(format!("BindAddress={v}"));
                }
            }
            "-B" => {
                if let Some(v) = take_value(&mut i) {
                    out.push("-o".into());
                    out.push(format!("BindInterface={v}"));
                }
            }
            "-m" => {
                if let Some(v) = take_value(&mut i) {
                    out.push("-o".into());
                    out.push(format!("MACs={v}"));
                }
            }
            // Carried verbatim (identity/config/jump/options/cipher).
            "-i" | "-F" | "-J" | "-o" | "-c" => {
                if glued.is_some() {
                    out.push(a.clone());
                } else {
                    out.push(a.clone());
                    if let Some(v) = take_value(&mut i) {
                        out.push(v);
                    }
                }
            }
            // Carried booleans.
            "-4" | "-6" | "-C" => out.push(a.clone()),
            // Dropped value flags (tty/forwarding/mux/query/log — session-
            // shaped; §3.2). Consume the value so it isn't misread as a flag.
            "-e" | "-E" | "-Q" | "-O" | "-L" | "-R" | "-D" | "-w" | "-W" | "-S" | "-I" => {
                let _ = take_value(&mut i);
            }
            // Everything else (booleans -t -T -N -f -G -K -k -M -a -A -x -X
            // -Y -g -n -q -v and unknowns): DROP, never guess (DO-NOT 6).
            _ => {}
        }
        i += 1;
    }
    out.extend([
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ]);
    // `-b` MUST precede the destination: OpenSSH's getopt does not permute,
    // so a trailing `-b <file>` after the first non-option argument is a
    // usage error — sftp exits 1 INSTANTLY, before any connection. This was
    // the field failure behind every "sftp exited with Some(1)" probe line
    // in the user's daemon.log (and the same argv feeds the ssh-drop
    // uploads): the staging transport (`-D`, no destination) never
    // exercised the destination-last shape, so the bug only existed against
    // real hosts. Reproduced verbatim with `sftp ... host -b B` → "usage:".
    out.extend(["-b".into(), batch.to_string()]);
    // Destination: scheme rewrite first (sftp rejects ssh://), then the -l
    // fold (skipped when a user is already present).
    let mut dest = meta_args[dest_idx].clone();
    if let Some(rest) = dest.strip_prefix("ssh://") {
        dest = format!("sftp://{rest}");
    }
    if let Some(u) = login {
        if !dest.contains('@') {
            dest = match dest.find("://") {
                Some(pos) => format!("{}{u}@{}", &dest[..pos + 3], &dest[pos + 3..]),
                None => format!("{u}@{dest}"),
            };
        }
    }
    out.push(dest);
    out
}

/// Staging shape: `-D <sftp_server_command>` replaces the transport entirely
/// (no ssh runs, no destination). Validated recipe:
/// `sftp -q -b <batch> -D "C:/Windows/System32/wsl.exe -d Ubuntu -- …"`.
pub fn sftp_args_transport(transport: &str, batch: &str) -> Vec<String> {
    vec![
        "-q".into(),
        "-D".into(),
        transport.to_string(),
        "-b".into(),
        batch.to_string(),
    ]
}

// ─────────────────────────── output parsers ───────────────────────────

/// `pwd` prints `Remote working directory: /path` (§2).
pub fn parse_pwd(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|l| {
        l.trim_end_matches('\r')
            .strip_prefix("Remote working directory: ")
            .map(str::to_string)
    })
}

/// `ls -1 .tc-drops` lines → existing names. Skips `sftp> ` command echoes,
/// strips the `.tc-drops/` path prefix the server prints.
pub fn parse_ls1(stdout: &str) -> HashSet<String> {
    stdout
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty() && !l.starts_with("sftp>"))
        .filter(|l| !l.starts_with("Remote working directory:"))
        .map(|l| {
            l.strip_prefix(".tc-drops/")
                .unwrap_or(l)
                .to_string()
        })
        .collect()
}

/// `ls -l` line shape, one entry: (requested-path name, size, is_dir). The
/// listing is CLIENT-formatted from SFTP v3 attrs (link count renders `?`,
/// perms render masked `-rw-******` — no server produces that shape), so
/// this parse is stable across OpenSSH/BSD/macOS servers. Field 5 = size,
/// fields 6-8 = date (`Jul  4  2026` or `Jul  4 04:08` — both `\S+`; the
/// form FLIPS on sub-second clock skew, so dates are NEVER read), trailing
/// group = name WITH the full requested-path prefix (spaces legal).
pub fn parse_ls_l_full(stdout: &str) -> Vec<(String, u64, bool)> {
    let re = regex::Regex::new(
        r"^(\S+)\s+\S+\s+\S+\s+\S+\s+(\d+)\s+\S+\s+\d+\s+\S+\s+(.+)$",
    )
    .expect("static regex");
    stdout
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.starts_with("sftp>"))
        .filter_map(|l| {
            let caps = re.captures(l)?;
            let size: u64 = caps[2].parse().ok()?;
            let is_dir = caps[1].starts_with('d');
            Some((caps[3].to_string(), size, is_dir))
        })
        .collect()
}

/// `ls -l .tc-drops` tail → (name, size) per entry, the ssh-drop shape:
/// `.tc-drops/` prefix stripped. Spec §6.3.5's exact semantics, delegating
/// to the shared line parse.
pub fn parse_ls_l(stdout: &str) -> Vec<(String, u64)> {
    parse_ls_l_full(stdout)
        .into_iter()
        .map(|(name, size, _)| {
            (
                name.strip_prefix(".tc-drops/").unwrap_or(&name).to_string(),
                size,
            )
        })
        .collect()
}

// ─────────────────────────── failure classifiers ───────────────────────────

/// Connection-class failure: one toast, whole batch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnErr {
    /// Row 1: network timeout.
    Timeout,
    /// Row 2: connection refused.
    Refused,
    /// Row 3: no sftp.exe (spawn failed / never found). Carries where we
    /// looked so the toast can say it.
    SftpMissing { looked: String },
    /// Row 4: auth needs interaction BatchMode can't give.
    Auth,
    /// Row 5: hostname didn't resolve.
    Dns,
    /// Row 6: host key not in known_hosts (BatchMode can't prompt).
    HostKeyUntrusted,
    /// Row 7: `~/.tc-drops` could not be created (conn 1 exit 1).
    MkdirDenied,
    /// Row 11: conn 2 died mid-upload (exit 255 after the puts started).
    Dropped,
    /// Row 13: sshd disables the SFTP subsystem (documented, unverified).
    NoSftpSubsystem,
    /// Unmatched stderr — surfaced honestly with its first line.
    Other(String),
}

/// First match wins, case-sensitive substrings on the raw stderr (§7; every
/// pattern is from a REAL capture). `Connection closed` alone is never
/// matched — it trails every failure.
pub fn classify_conn(stderr: &str) -> ConnErr {
    if stderr.contains("Connection timed out") {
        return ConnErr::Timeout;
    }
    if stderr.contains("Connection refused") {
        return ConnErr::Refused;
    }
    if stderr.contains("Permission denied (") {
        return ConnErr::Auth;
    }
    if stderr.contains("Could not resolve hostname") {
        return ConnErr::Dns;
    }
    if stderr.contains("Host key verification failed.") {
        return ConnErr::HostKeyUntrusted;
    }
    if stderr.contains("subsystem request failed") {
        return ConnErr::NoSftpSubsystem;
    }
    let first = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("connection failed")
        .to_string();
    ConnErr::Other(first)
}

/// Per-file failure reason (rows 8-10), matched by filename within the
/// stderr line. The ls name+size verify already decided FAILURE — this only
/// picks the reason (unmatched ⇒ generic write failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileErr {
    /// Row 9: local file missing/locked (pre-flight or sftp `stat`).
    LocalUnreadable,
    /// Row 8: remote write failed (SFTP v3's bare `Failure` — disk full /
    /// quota / generic; the toast text hedges).
    RemoteWriteFailed,
    /// Row 10: remote refused the write (`dest open … Permission denied` /
    /// dir vanished after conn 1).
    DestDenied,
}

pub fn classify_file(local_fwd: &str, final_name: &str, stderr: &str) -> FileErr {
    for line in stderr.lines() {
        if line.contains("stat ") && line.contains(local_fwd) {
            return FileErr::LocalUnreadable;
        }
        if line.contains("write remote") && line.contains(final_name) {
            return FileErr::RemoteWriteFailed;
        }
        if line.contains("dest open") && line.contains(final_name) {
            return FileErr::DestDenied;
        }
    }
    FileErr::RemoteWriteFailed
}

// ─────────────────────────── process helpers ───────────────────────────

/// §3.4: sftp.exe as SIBLING of the session's resolved ssh (same client
/// config/known_hosts semantics — and sftp finds its ssh.exe by app-dir
/// search, proven §9.9), falling back to a PATH search. Err carries where we
/// looked (the row-3 toast names it).
pub fn resolve_sftp(program: &str) -> Result<PathBuf, String> {
    let p = Path::new(program);
    let ssh_dir: Option<PathBuf> = if p.components().count() > 1 {
        p.parent().map(Path::to_path_buf)
    } else {
        // Bare name: walk PATH the way resolve_program does.
        let stem = if p.extension().is_some() {
            program.to_string()
        } else {
            format!("{program}.exe")
        };
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(&stem))
                .find(|c| c.is_file())
                .and_then(|c| c.parent().map(Path::to_path_buf))
        })
    };
    if let Some(dir) = &ssh_dir {
        let cand = dir.join("sftp.exe");
        if cand.is_file() {
            return Ok(cand);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        if let Some(c) = std::env::split_paths(&paths)
            .map(|d| d.join("sftp.exe"))
            .find(|c| c.is_file())
        {
            return Ok(c);
        }
    }
    Err(ssh_dir
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string()))
}

pub fn kill_pid(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(h, 1);
            let _ = CloseHandle(h);
        }
    }
}

/// A duplicated process handle for the child, as a raw `usize` so it can
/// cross into the watchdog thread. Unlike a pid, the handle stays bound to
/// THIS child forever — Windows can reuse an exited child's pid within the
/// watchdog's 200ms poll gap, and `TerminateProcess(OpenProcess(pid))` would
/// then hit an unrelated process. Caller owns the handle (CloseHandle).
fn dup_process_handle(child: &std::process::Child) -> Option<usize> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut dup = HANDLE::default();
        let cur = GetCurrentProcess();
        DuplicateHandle(
            cur,
            HANDLE(child.as_raw_handle()),
            cur,
            &mut dup,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
        .ok()?;
        Some(dup.0 as usize)
    }
}

/// Write an sftp batch file: UTF-8, no BOM, LF line endings (the proven
/// round-trip shape for spaces/unicode).
pub fn write_batch(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)
}

/// Cross-thread kill slot holding a duplicated process HANDLE — never a bare
/// pid (r1-F5's pid-reuse class: a pid can be recycled the moment the child
/// exits, and killing by pid would then hit an unrelated process). `run_sftp`
/// publishes the running child's handle here and closes it after the wait;
/// `kill()` terminates through the handle under the same lock, so the handle
/// can never be closed out from under a racing kill.
#[derive(Default)]
pub struct KillSlot(Mutex<Option<usize>>);

impl KillSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Terminate the currently-published child, if any. Safe against the
    /// child having already exited (the handle stays bound to that process).
    pub fn kill(&self) {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Threading::TerminateProcess;
        let g = self.0.lock().unwrap();
        if let Some(h) = *g {
            unsafe {
                let _ = TerminateProcess(HANDLE(h as _), 1);
            }
        }
    }

    fn publish(&self, h: usize) {
        self.clear_and_close();
        *self.0.lock().unwrap() = Some(h);
    }

    fn clear_and_close(&self) {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        let mut g = self.0.lock().unwrap();
        if let Some(h) = g.take() {
            unsafe {
                let _ = CloseHandle(HANDLE(h as _));
            }
        }
    }
}

impl Drop for KillSlot {
    fn drop(&mut self) {
        self.clear_and_close();
    }
}

/// One sftp batch connection: spawn hidden (CREATE_NO_WINDOW, stdin null,
/// stdout+stderr piped), wait for exit, return the full Output. stdout is
/// parse-after-exit ONLY (fully buffered over pipes, ssh-drop DO-NOT 2).
///
/// `timeout`: a watchdog thread `TerminateProcess`es the child at the
/// deadline (probes must die fast; ConnectTimeout only covers connect, not a
/// wedged established link). None = no watchdog (the ssh-drop uploads rely
/// on the child's own ServerAlive clock + explicit cancel).
///
/// `kill_slot`: a duplicated handle to the running child is published here
/// (the GUI's cancel/shutdown paths kill through it — handle-based, so a
/// recycled pid can never be hit).
pub fn run_sftp(
    sftp: &Path,
    args: &[String],
    timeout: Option<Duration>,
    kill_slot: Option<&KillSlot>,
) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let child = std::process::Command::new(sftp)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    let pid = child.id();
    if let Some(slot) = kill_slot {
        if let Some(h) = dup_process_handle(&child) {
            slot.publish(h);
        }
    }
    let done = Arc::new(AtomicBool::new(false));
    if let Some(deadline) = timeout {
        let done = done.clone();
        // Kill through a duplicated handle (see dup_process_handle) so a
        // child that exits just before the deadline can never be confused
        // with a pid-reuse successor. kill_pid stays as the fallback for the
        // (never observed) dup failure.
        let handle = dup_process_handle(&child);
        let _ = std::thread::Builder::new()
            .name("sftp-watchdog".into())
            .spawn(move || {
                use windows::Win32::Foundation::{CloseHandle, HANDLE};
                use windows::Win32::System::Threading::TerminateProcess;
                let end = std::time::Instant::now() + deadline;
                while !done.load(Ordering::Relaxed) {
                    if std::time::Instant::now() >= end {
                        match handle {
                            Some(h) => unsafe {
                                let _ = TerminateProcess(HANDLE(h as _), 1);
                            },
                            None => kill_pid(pid),
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                if let Some(h) = handle {
                    unsafe {
                        let _ = CloseHandle(HANDLE(h as _));
                    }
                }
            });
    }
    let out = child.wait_with_output();
    done.store(true, Ordering::Relaxed);
    if let Some(slot) = kill_slot {
        slot.clear_and_close();
    }
    out
}

// ─────────────────────────── hook-install plumbing ───────────────────────────
//
// Shared by `claude_hooks::install_remote` and `codex_hooks::install_remote`,
// which carried verbatim copies of this runner (the same divergence class as
// the earlier sftp field bug). The per-lane batch BODIES and merge logic stay
// with their features — only the transport mechanics live here.

/// Forward-slash a local path for an sftp batch line.
pub fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// The sftp argv for probes/hook installs: the terminal's persisted flags
/// through the shared translation, or the `TC_SSH_PROBE_TRANSPORT` stand-in —
/// staging only, gated on TC_DATA_DIR isolation so an installed build can
/// never take the stand-in path.
pub fn install_argv(meta_args: &[String], batch: &str) -> Vec<String> {
    match std::env::var("TC_SSH_PROBE_TRANSPORT") {
        Ok(t) if !t.is_empty() && crate::state::data_dir_overridden() => {
            sftp_args_transport(&t, batch)
        }
        _ => sftp_args(meta_args, batch),
    }
}

/// One hook-install sftp batch connection: write the batch to the TC tmp dir
/// under a per-call UNIQUE name, run it under a watchdog deadline, clean up.
/// The name carries a fresh uuid, not just the pid — two installs concurrent
/// in one GUI process (two hosts consented in quick succession) previously
/// shared `{tag}-{pid}.batch`, so one worker could execute the other's batch
/// and then merge-from-empty against its own host.
pub fn run_install_batch(
    sftp: &Path,
    meta_args: &[String],
    tag: &str,
    body: &str,
    deadline: Duration,
) -> Result<std::process::Output, String> {
    let dir = crate::state::data_dir().join("tmp");
    let _ = std::fs::create_dir_all(&dir);
    let bpath = dir.join(format!(
        "{tag}-{}-{}.batch",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    write_batch(&bpath, body).map_err(|e| format!("batch write: {e}"))?;
    let argv = install_argv(meta_args, &bpath.to_string_lossy());
    let out = run_sftp(sftp, &argv, Some(deadline), None);
    let _ = std::fs::remove_file(&bpath);
    out.map_err(|e| format!("sftp spawn: {e}"))
}

/// Provable remote absence for a `-get`-fetched file: true ONLY when the
/// batch output PROVES the remote file does not exist — no listing line for
/// it on stdout (the fetch batch pairs every `-get` with a `-ls`) AND the
/// sftp client's "not found" error for it on stderr. Anything else — a file
/// that exists but couldn't be fetched (permission denied, dir-shaped,
/// transient) — is NOT absence: treating it as absent would merge-from-empty
/// and atomically replace the user's real file. Callers refuse instead.
///
/// `get`/`ls` of a nonexistent path both print `… "<absolute path>" not
/// found` (client-side glob, stable across servers); the relative batch path
/// is a suffix of the absolute one, so substring matching covers both.
pub fn remote_file_absent(remote_path: &str, stdout: &str, stderr: &str) -> bool {
    let listed = stdout
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.starts_with("sftp>"))
        .any(|l| l.contains(remote_path));
    let not_found = stderr
        .lines()
        .any(|l| l.contains(remote_path) && l.contains("not found"));
    !listed && not_found
}

/// R4-F3, the completeness prover for a `-get`-fetched file: true ONLY when
/// the paired `-ls -l` listing carries a non-directory entry for
/// `remote_path` whose size equals `local_len`. A `-get` that fails
/// MID-TRANSFER (remote read error with the connection surviving, local disk
/// full) leaves a PARTIAL local file while the `-`-prefixed batch continues
/// and exits 0 — and a TOML/JSON body truncated at a line boundary can still
/// parse, so `remote_file_absent`'s NotFound gate never sees it. Callers
/// refuse to merge unless this holds (never-clobber: the merged result is
/// atomically renamed over the user's real file).
///
/// Single-file `ls` listings print the ABSOLUTE path (client-side glob
/// prefixes the remote cwd — same shape `remote_file_absent` documents), so
/// the relative batch path is matched as a `/`-separated suffix. A size
/// mismatch from ANY cause (partial fetch, symlink whose lstat size is the
/// link itself, file changed between ls and get) refuses — refusal is always
/// recoverable, a clobbered config is not.
pub fn fetched_len_matches(remote_path: &str, stdout: &str, local_len: u64) -> bool {
    parse_ls_l_full(stdout).into_iter().any(|(name, size, is_dir)| {
        let name_matches = name == remote_path
            || name
                .strip_suffix(remote_path)
                .is_some_and(|prefix| prefix.ends_with('/'));
        !is_dir && name_matches && size == local_len
    })
}

// ─────────────────────────── tests (moved goldens) ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // ── sftp_args goldens ──

    /// The author's test host (anonymized to an RFC 5737 documentation
    /// address): bare destination, zero translation. `-b` sits
    /// BEFORE the destination — OpenSSH getopt does not permute, and the
    /// old destination-first order was an instant usage-error exit 1 on
    /// every real-host probe/upload (reproduced verbatim; the `-D` staging
    /// transport has no destination and never caught it).
    #[test]
    fn sftp_args_bare_host_golden() {
        assert_eq!(
            sftp_args(&s(&["192.0.2.14"]), "B"),
            s(&[
                "-q",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-b",
                "B",
                "192.0.2.14",
            ])
        );
    }

    /// The load-bearing argv invariant on its own: every flag (ours and the
    /// batch) precedes the destination, which is the LAST argument.
    #[test]
    fn sftp_args_batch_precedes_destination() {
        for argv in [
            sftp_args(&s(&["192.0.2.14"]), "B"),
            sftp_args(&s(&["-p", "2222", "-i", "k.pem", "u@h"]), "B"),
            sftp_args(&s(&["ssh://alice@devbox"]), "B"),
        ] {
            let b = argv.iter().position(|a| a == "-b").unwrap();
            assert_eq!(b + 2, argv.len() - 1, "-b <file> just before the destination: {argv:?}");
        }
    }

    /// Port renames, identities carry, -v drops.
    #[test]
    fn sftp_args_renames_carries_drops() {
        assert_eq!(
            sftp_args(&s(&["-p", "2222", "-i", "k.pem", "-v", "u@h"]), "B"),
            s(&[
                "-q",
                "-o",
                "BatchMode=yes",
                "-P",
                "2222",
                "-i",
                "k.pem",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-b",
                "B",
                "u@h",
            ])
        );
    }

    #[test]
    fn sftp_args_glued_port() {
        let args = sftp_args(&s(&["-p2222", "h"]), "B");
        assert!(args.contains(&"-P2222".to_string()));
        assert!(!args.iter().any(|a| a == "-p2222"));
    }

    /// -l folds into the destination; an existing user@ wins.
    #[test]
    fn sftp_args_login_fold() {
        let args = sftp_args(&s(&["-l", "u", "h"]), "B");
        assert!(args.contains(&"u@h".to_string()));
        assert!(!args.iter().any(|a| a == "-l" || a == "u"));
        let args = sftp_args(&s(&["-l", "u", "x@h"]), "B");
        assert!(args.contains(&"x@h".to_string()));
        assert!(!args.iter().any(|a| a.contains("u@")));
    }

    /// -b/-B/-m have different meanings in sftp — become -o spellings.
    #[test]
    fn sftp_args_o_translations() {
        let args = sftp_args(
            &s(&["-b", "1.2.3.4", "-B", "eth0", "-m", "hmac-sha2-256", "h"]),
            "B",
        );
        let joined = args.join(" ");
        assert!(joined.contains("-o BindAddress=1.2.3.4"));
        assert!(joined.contains("-o BindInterface=eth0"));
        assert!(joined.contains("-o MACs=hmac-sha2-256"));
    }

    /// Session-shaped flags (tty/forwarding/mux) drop WITH their values.
    #[test]
    fn sftp_args_drops_session_flags() {
        let args = sftp_args(
            &s(&["-t", "-L", "8080:localhost:80", "-S", "/mux", "-N", "h"]),
            "B",
        );
        assert_eq!(
            args,
            s(&[
                "-q",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-b",
                "B",
                "h",
            ])
        );
    }

    /// ssh:// scheme rewrites (sftp resolves literal host "ssh" otherwise).
    #[test]
    fn sftp_args_scheme_rewrite() {
        let args = sftp_args(&s(&["ssh://alice@devbox"]), "B");
        assert!(args.contains(&"sftp://alice@devbox".to_string()));
    }

    /// BatchMode is OURS and first; user -o flags ride after it (first-
    /// occurrence-wins ⇒ a user BatchMode=no can never win), automatic
    /// timeouts append after user flags (user overrides automatic).
    #[test]
    fn sftp_args_ordering_contract() {
        let args = sftp_args(&s(&["-o", "ConnectTimeout=99", "h"]), "B");
        let bm = args.iter().position(|a| a == "BatchMode=yes").unwrap();
        let user = args.iter().position(|a| a == "ConnectTimeout=99").unwrap();
        let ours = args.iter().position(|a| a == "ConnectTimeout=10").unwrap();
        assert!(bm < user && user < ours);
    }

    #[test]
    fn sftp_args_transport_shape() {
        assert_eq!(
            sftp_args_transport("C:/W/wsl.exe -d U -- /srv -d /tmp/h", "B"),
            s(&["-q", "-D", "C:/W/wsl.exe -d U -- /srv -d /tmp/h", "-b", "B"])
        );
    }

    // ── parsers ──

    #[test]
    fn parse_pwd_extracts_home() {
        let stdout = "sftp> pwd\r\nRemote working directory: /home/alice\r\nsftp> -mkdir .tc-drops\r\n";
        assert_eq!(parse_pwd(stdout).as_deref(), Some("/home/alice"));
        assert_eq!(parse_pwd("no pwd here"), None);
    }

    #[test]
    fn parse_ls1_skips_echoes_and_strips_prefix() {
        let stdout = "sftp> pwd\nRemote working directory: /home/z\nsftp> -mkdir .tc-drops\nsftp> ls -1 .tc-drops\n.tc-drops/file with space \u{e9}.png\n.tc-drops/b.txt\n";
        let names = parse_ls1(stdout);
        assert!(names.contains("file with space \u{e9}.png"));
        assert!(names.contains("b.txt"));
        assert_eq!(names.len(), 2);
    }

    /// Both real `ls -l` date forms (year vs clock), spaces in names, size
    /// extraction, echo-line skip.
    #[test]
    fn parse_ls_l_both_date_forms_and_spaces() {
        let stdout = "sftp> ls -l .tc-drops\r\n\
            -rw-r--r--    1 alice     alice          307 Jul  4  2026 file with space \u{e9}-2.png\r\n\
            -rw-r--r--    1 alice     alice       102400 Jul  4 04:08 b.txt\r\n";
        let l = parse_ls_l(stdout);
        assert_eq!(
            l,
            vec![
                ("file with space \u{e9}-2.png".to_string(), 307),
                ("b.txt".to_string(), 102400),
            ]
        );
    }

    /// Remote-resume spec §12.2 fixture (anonymized in lockstep with the
    /// spec — username/paths replaced, structure and sizes kept): masked
    /// perms + `?` link
    /// count (the client-formatted shape), BOTH date forms in one listing —
    /// the freshest file renders in YEAR form on sub-second clock skew —
    /// full requested-path prefixes kept by `parse_ls_l_full`, and the
    /// copilot `d` mode char detected.
    #[test]
    fn parse_ls_l_full_probe_shapes_verbatim() {
        let stdout = "sftp> -ls -l .claude/projects/-home-alice-proj\r\n\
            -rw-******    ? alice     alice            1 Jul  4 13:56 .claude/projects/-home-alice-proj/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl\r\n\
            -rw-******    ? alice     alice           10 Jul  4 15:46 .claude/projects/-home-alice-proj/bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb.jsonl\r\n\
            -rw-******    ? alice     alice            5 Jul  4  2026 .claude/projects/-home-alice-proj/cccccccc-cccc-cccc-cccc-cccccccccccc.jsonl\r\n\
            sftp> -ls -l .copilot/session-state\r\n\
            drwx******    ? alice     alice         4096 Jul  4  2026 .copilot/session-state/1b2f3c4d-0000-0000-0000-000000000000\r\n";
        let l = parse_ls_l_full(stdout);
        assert_eq!(l.len(), 4);
        assert_eq!(
            l[0],
            (
                ".claude/projects/-home-alice-proj/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl"
                    .to_string(),
                1,
                false
            )
        );
        assert_eq!(l[1].1, 10);
        // YEAR-form date parses identically (the D1 date-form trap: dates are
        // matched as \S+ and never interpreted).
        assert_eq!(l[2].1, 5);
        // Directory entries carry the `d` mode char.
        assert_eq!(
            l[3],
            (
                ".copilot/session-state/1b2f3c4d-0000-0000-0000-000000000000".to_string(),
                4096,
                true
            )
        );
    }

    // ── classify_conn: every §7 captured string verbatim ──

    #[test]
    fn classify_conn_captured_fixtures() {
        assert_eq!(
            classify_conn(
                "ssh: connect to host 10.255.255.1 port 22: Connection timed out\r\nConnection closed"
            ),
            ConnErr::Timeout
        );
        assert_eq!(
            classify_conn("banner exchange: Connection to UNKNOWN port -1: Connection refused"),
            ConnErr::Refused
        );
        assert_eq!(
            classify_conn("git@github.com: Permission denied (publickey)."),
            ConnErr::Auth
        );
        assert_eq!(
            classify_conn(
                "ssh: Could not resolve hostname no-such-host-zzz.invalid: No such host is known."
            ),
            ConnErr::Dns
        );
        assert_eq!(
            classify_conn("Host key verification failed."),
            ConnErr::HostKeyUntrusted
        );
        assert_eq!(
            classify_conn("subsystem request failed on channel 0"),
            ConnErr::NoSftpSubsystem
        );
        // The universal trailer alone is NEVER matched as a class.
        assert_eq!(
            classify_conn("Connection closed"),
            ConnErr::Other("Connection closed".into())
        );
    }

    // ── remote_file_absent: the never-clobber absence prover ──

    /// Absence needs BOTH signals: no listing line AND the client's
    /// "not found" for the path. Command echoes never count as listings.
    #[test]
    fn remote_absent_proven_only_by_both_signals() {
        let path = ".claude/settings.json";
        let echo_only = "sftp> -ls \".claude/settings.json\"\r\nsftp> -get \".claude/settings.json\" \"C:/t/f.json\"\r\n";
        let nf = "Can't ls: \"/home/alice/.claude/settings.json\" not found\r\nFile \"/home/alice/.claude/settings.json\" not found.\r\n";
        assert!(remote_file_absent(path, echo_only, nf));
        // File exists (listing line) but the -get failed (e.g. root-owned
        // 600): NOT absent, even with unrelated stderr noise.
        let listed = "sftp> -ls \".claude/settings.json\"\r\n-rw-------    ? root     root          210 Jul  4 13:56 /home/alice/.claude/settings.json\r\n";
        assert!(!remote_file_absent(path, listed, ""));
        assert!(!remote_file_absent(path, listed, nf));
        // No signals at all (transient failure, truncated output): NOT absent.
        assert!(!remote_file_absent(path, "", ""));
        // Permission-denied stderr without "not found": NOT absent.
        assert!(!remote_file_absent(
            path,
            echo_only,
            "Couldn't open remote file \"/home/alice/.claude/settings.json\": Permission denied\r\n"
        ));
    }

    /// R4-F3: the partial-fetch prover. Complete fetch (listed size ==
    /// local bytes) passes; a size mismatch (mid-transfer death), a missing
    /// listing (echoes only), and a directory entry all refuse.
    #[test]
    fn fetched_len_matches_table() {
        let path = ".codex/config.toml";
        // Single-file listings print the ABSOLUTE path.
        let listed = "sftp> -ls -l \".codex/config.toml\"\r\n\
            -rw-------    ? alice     alice          210 Jul  4 13:56 /home/alice/.codex/config.toml\r\n";
        assert!(fetched_len_matches(path, listed, 210), "complete fetch passes");
        assert!(!fetched_len_matches(path, listed, 100), "partial fetch refuses");
        assert!(!fetched_len_matches(path, listed, 0), "empty local file refuses");
        // Relative name form (directory-style listing) also matches.
        let rel = "sftp> -ls -l .codex/config.toml\r\n\
            -rw-r--r--    1 alice     alice          210 Jul  4  2026 .codex/config.toml\r\n";
        assert!(fetched_len_matches(path, rel, 210));
        // Echoes alone are never a listing; empty output refuses.
        let echo_only = "sftp> -ls -l \".codex/config.toml\"\r\nsftp> -get \".codex/config.toml\" \"C:/t/f.toml\"\r\n";
        assert!(!fetched_len_matches(path, echo_only, 210));
        assert!(!fetched_len_matches(path, "", 0));
        // A name that merely CONTAINS the path without a `/` boundary, or a
        // directory entry, never proves completeness.
        let dir = "drwx******    ? alice     alice          210 Jul  4  2026 /home/alice/.codex/config.toml\r\n";
        assert!(!fetched_len_matches(path, dir, 210), "dir-shaped entry refuses");
        let odd = "-rw-r--r--    1 alice     alice          210 Jul  4  2026 /home/alice/x.codex/config.toml\r\n";
        assert!(!fetched_len_matches(path, odd, 210), "non-boundary suffix refuses");
    }

    #[test]
    fn classify_file_rows() {
        assert_eq!(
            classify_file(
                "C:/gone-missing.bin",
                "gone-missing.bin",
                "stat C:/gone-missing.bin: No such file or directory"
            ),
            FileErr::LocalUnreadable
        );
        assert_eq!(
            classify_file(
                "C:/big.bin",
                "big.bin",
                "write remote \"/home/x/.tc-drops/big.bin\": Failure"
            ),
            FileErr::RemoteWriteFailed
        );
        assert_eq!(
            classify_file(
                "C:/f.txt",
                "f.txt",
                "dest open \"/home/x/.tc-drops/f.txt\": Permission denied"
            ),
            FileErr::DestDenied
        );
        // Unmatched stderr ⇒ generic write failure (the verify already said
        // FAILED; only the reason is unknown).
        assert_eq!(
            classify_file("C:/a.png", "a.png", ""),
            FileErr::RemoteWriteFailed
        );
    }
}
