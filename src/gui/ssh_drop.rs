//! SSH drag-drop upload (#26) — drop local files on an ssh terminal, upload
//! them to `~/.tc-drops/` on the remote over SFTP, then paste the remote
//! paths (docs/ssh-drop-spec.md, followed verbatim).
//!
//! GUI-ONLY (spec inv. 1): uploads are `std::process` children of the GUI —
//! the daemon is wire-frozen and never hears about any of this. Transport is
//! `sftp.exe` in batch mode, two parse-after-exit connections per drop (T1/
//! T2: Windows sftp stdout is FULLY buffered over pipes, so interactive
//! driving is impossible; probe `pwd`+`mkdir`+`ls` first, then `-put`×N +
//! `ls -l` verification). Success authority is EXCLUSIVELY the name+size
//! match in the `ls -l` tail (inv. 5/T8 — `-put` failures leave exit 0 and
//! disk-full leaves a partial file at the right name).
//!
//! The pure transport half (argv translation, parsers, classifiers,
//! `resolve_sftp`, the spawn/watchdog helpers) lives in `crate::
//! ssh_transport` — hoisted for the daemon's remote CLI-resume probes
//! (remote-cli-resume-spec D4); its goldens moved with it and pin
//! byte-identical behavior. This file keeps the upload-shaped pieces: name
//! resolution, batch/toast builders, and the impure `Uploads` queue + worker
//! threads (std thread + mpsc + `ctx.request_repaint`, all toast/paste work
//! on the GUI thread via the `logic()` drain).
//!
//! `TC_SSH_DROP_TRANSPORT=<sftp -D command>` (spec §9.1, PERMANENT env-gated
//! staging knob — the TC_SSH_VIA_WSL precedent): replaces the translated
//! flags + destination with `-D <value>`, running the whole pipeline against
//! a local sftp-server (WSL) with zero network.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

// The hoisted transport surface, re-exported so consumers (gui/mod.rs) keep
// their `ssh_drop::` spellings — the hoist is invisible from outside.
pub use crate::ssh_transport::{
    classify_conn, classify_file, parse_ls1, parse_ls_l, parse_pwd, resolve_sftp, sftp_args,
    sftp_args_transport, ConnErr, FileErr,
};

use super::toast::ToastId;

// ─────────────────────────── §3.3 name resolution ───────────────────────────

/// Final remote names for a batch: keep the original filename; on collision
/// (against the remote `ls -1` ∪ names already chosen in this batch) append
/// `-2`, `-3`… before the extension, `-99` rollover falls back to a unix-
/// seconds tag (never fails). Order = drop order.
pub fn resolve_names(
    dropped: &[PathBuf],
    existing: &HashSet<String>,
) -> Vec<(PathBuf, String)> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    resolve_names_at(dropped, existing, secs)
}

fn resolve_names_at(
    dropped: &[PathBuf],
    existing: &HashSet<String>,
    secs: u64,
) -> Vec<(PathBuf, String)> {
    let mut taken: HashSet<String> = existing.clone();
    let mut out = Vec::with_capacity(dropped.len());
    for p in dropped {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Split "stem.ext" the way Path does (dotfiles keep their dot; a
        // missing extension suffixes bare).
        let np = Path::new(&name);
        let stem = np
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.clone());
        let ext = np.extension().map(|e| e.to_string_lossy().into_owned());
        let with = |tag: &str| match &ext {
            Some(e) => format!("{stem}{tag}.{e}"),
            None => format!("{stem}{tag}"),
        };
        let mut final_name = name.clone();
        if taken.contains(&final_name) {
            final_name = (2..=99)
                .map(|n| with(&format!("-{n}")))
                .find(|c| !taken.contains(c))
                .unwrap_or_else(|| with(&format!("-{secs}")));
        }
        taken.insert(final_name.clone());
        out.push((p.clone(), final_name));
    }
    out
}

/// Pre-flight refusals the drop router applies BEFORE consent (§3.3):
/// directories (v1 refuses; sftp `put` of a dir needs -R + tree mkdir) and
/// non-Unicode names. Returns (uploadable, refusal lines for the toast).
pub fn preflight_partition(paths: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<String>) {
    let mut ok = Vec::new();
    let mut refused = Vec::new();
    for p in paths {
        let name = match p.file_name().and_then(|s| s.to_str()) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => {
                refused.push(format!(
                    "{} — name isn't valid Unicode",
                    p.to_string_lossy()
                ));
                continue;
            }
        };
        if p.is_dir() {
            refused.push(format!("{name} — folders can't be uploaded (v1)"));
        } else {
            ok.push(p);
        }
    }
    (ok, refused)
}

// ─────────────────────────── §3.1 batch builders ───────────────────────────

/// Conn 1 — probe: home dir, ensure the landing dir, list existing names.
/// `-mkdir` ignore-prefixed (exists = harmless noise); a failing `ls` aborts
/// with exit 1 = the dir could not be created (§7 row 7).
pub fn conn1_batch() -> String {
    "pwd\n-mkdir .tc-drops\nls -1 .tc-drops\n".to_string()
}

/// Conn 2 — upload: `-put` per file (one bad file doesn't abort the rest,
/// T11) + the `ls -l` verification tail. Local paths forward-slashed (the
/// Windows port normalizes; proven §9); `"` is illegal in Windows filenames
/// so double-quoting is always safe.
pub fn conn2_batch(pairs: &[(PathBuf, String)]) -> String {
    let mut b = String::new();
    for (local, name) in pairs {
        let l = local.to_string_lossy().replace('\\', "/");
        b.push_str(&format!("-put \"{l}\" \".tc-drops/{name}\"\n"));
    }
    b.push_str("ls -l .tc-drops\n");
    b
}

/// Conn 3 — cleanup: best-effort `-rm` of names THIS job created but did not
/// verify (inv. 6 — our garbage, seconds old, never pasted). Output is
/// discarded; the connection itself is never toasted.
pub fn conn3_batch(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("-rm \".tc-drops/{n}\"\n"))
        .collect()
}

// ─────────────────────────── §7 toast copy (pure) ───────────────────────────

fn n_files(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{n} files")
    }
}

/// Connection-failure toast (title, detail) per the §7 table.
pub fn conn_err_toast(err: &ConnErr, host: &str, files: &[String]) -> (String, Vec<String>) {
    let n = files.len();
    match err {
        ConnErr::Timeout => (
            format!("{host} didn't answer"),
            vec![format!("network timeout — {} not uploaded", n_files(n))],
        ),
        ConnErr::Refused => (
            format!("{host} refused the connection"),
            vec!["is sshd running? — nothing uploaded".into()],
        ),
        ConnErr::SftpMissing { looked } => (
            "can't upload — sftp.exe not found".into(),
            vec![format!(
                "install the Windows \"OpenSSH Client\" feature (looked beside {looked})"
            )],
        ),
        ConnErr::Auth => (
            format!("{host}: key or agent auth required for drops"),
            vec![
                "password prompts can't run in the background — set up a key to enable drops"
                    .into(),
            ],
        ),
        ConnErr::Dns => (
            format!("can't find {host}"),
            vec!["hostname didn't resolve — nothing uploaded".into()],
        ),
        ConnErr::HostKeyUntrusted => (
            format!("{host} isn't trusted yet"),
            vec!["open the terminal and accept the host key once, then retry".into()],
        ),
        ConnErr::MkdirDenied => (
            format!("couldn't create ~/.tc-drops on {host}"),
            vec!["permission denied in the home directory".into()],
        ),
        ConnErr::Dropped => {
            let mut detail: Vec<String> = files.iter().take(4).cloned().collect();
            if n > 4 {
                detail.push(format!("+{} more", n - 4));
            }
            detail.push("did not finish — nothing pasted for them".into());
            (format!("connection to {host} was lost"), detail)
        }
        ConnErr::NoSftpSubsystem => (
            format!("{host} doesn't support file transfer"),
            vec!["the server disables SFTP — uploads can't work here".into()],
        ),
        ConnErr::Other(line) => (
            format!("can't upload to {host}"),
            vec![line.clone(), "nothing uploaded".into()],
        ),
    }
}

/// Single-file failure toast (title, detail) per §7 rows 8-10.
pub fn file_err_toast(name: &str, err: &FileErr, host: &str) -> (String, Vec<String>) {
    match err {
        FileErr::LocalUnreadable => (
            format!("can't read {name}"),
            vec!["the local file is missing or locked".into()],
        ),
        FileErr::RemoteWriteFailed => (
            format!("upload failed: {name}"),
            vec![format!("{host} couldn't write it — disk full or quota?")],
        ),
        FileErr::DestDenied => (
            format!("upload failed: {name}"),
            vec![format!("{host} refused the write in ~/.tc-drops")],
        ),
    }
}

/// One partial-batch detail line per failed file (T11's itemization).
pub fn file_err_line(name: &str, err: &FileErr, host: &str) -> String {
    match err {
        FileErr::LocalUnreadable => format!("{name} — local file missing or locked"),
        FileErr::RemoteWriteFailed => {
            format!("{name} — {host} couldn't write it (disk full or quota?)")
        }
        FileErr::DestDenied => format!("{name} — write refused in ~/.tc-drops"),
    }
}

// ─────────────────────────── §7.1 paste text ───────────────────────────

/// Absolute single-quoted POSIX paths, space-joined, ONE trailing space —
/// inert in bash/zsh/fish/dash (quoting shared with drop.rs, qol D7 parity).
pub fn paste_text(home: &str, names: &[String]) -> String {
    let quoted: Vec<String> = names
        .iter()
        .map(|n| super::drop::bash_single_quote(&format!("{home}/.tc-drops/{n}")))
        .collect();
    format!("{} ", quoted.join(" "))
}

// ─────────────────────────── §6 upload engine ───────────────────────────

/// A drop batch headed for one terminal's host.
pub struct Job {
    pub job_id: u64,
    pub terminal: Uuid,
    /// Destination verbatim — the toast/consent display string too.
    pub host: String,
    /// The session's persisted program (its ssh) — sftp resolves as its
    /// sibling (T4).
    pub program: String,
    /// The session's persisted args (user flags + destination).
    pub args: Vec<String>,
    pub files: Vec<PathBuf>,
    pub toast: ToastId,
}

/// Worker → GUI events; ALL toast/paste work happens on the GUI thread
/// (DO-NOT 10).
pub enum Event {
    Done {
        terminal: Uuid,
        job_id: u64,
        home: String,
        /// Drop order. Ok(final remote name) = verified uploaded (inv. 5).
        verdicts: Vec<(PathBuf, Result<String, FileErr>)>,
    },
    ConnFailed {
        terminal: Uuid,
        job_id: u64,
        err: ConnErr,
    },
    Cancelled {
        terminal: Uuid,
        job_id: u64,
    },
}

pub struct Running {
    pub job_id: u64,
    pub toast: ToastId,
    pub host: String,
    pub files: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
    /// Handle-based kill slot (r1-F5/r3-F10 pid-reuse class): the worker's
    /// live sftp child, killable without ever trusting a recyclable pid.
    kill: Arc<crate::ssh_transport::KillSlot>,
}

/// Per-terminal FIFO queues (T13: paste order == drop order per terminal),
/// parallel across terminals, ≤1 worker thread per terminal.
pub struct Uploads {
    ctx: egui::Context,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    queues: HashMap<Uuid, VecDeque<Job>>,
    running: HashMap<Uuid, Running>,
    next_job: u64,
}

impl Uploads {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            ctx,
            tx,
            rx,
            queues: HashMap::new(),
            running: HashMap::new(),
            next_job: 1,
        }
    }

    pub fn alloc_job(&mut self) -> u64 {
        let id = self.next_job;
        self.next_job += 1;
        id
    }

    /// A job is running or waiting on this terminal (drives the `queued —`
    /// toast prefix).
    pub fn busy(&self, terminal: Uuid) -> bool {
        self.running.contains_key(&terminal)
            || self.queues.get(&terminal).is_some_and(|q| !q.is_empty())
    }

    pub fn enqueue(&mut self, job: Job) {
        if self.running.contains_key(&job.terminal) {
            self.queues.entry(job.terminal).or_default().push_back(job);
        } else {
            self.start(job);
        }
    }

    pub fn drain(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Remove the finished running job (event handled by the GUI drain).
    pub fn finish(&mut self, terminal: Uuid, job_id: u64) -> Option<Running> {
        if self.running.get(&terminal).is_some_and(|r| r.job_id == job_id) {
            self.running.remove(&terminal)
        } else {
            None
        }
    }

    /// Start the terminal's next queued job; returns its toast id so the
    /// caller can strip the `queued — ` prefix.
    pub fn start_next(&mut self, terminal: Uuid) -> Option<ToastId> {
        if self.running.contains_key(&terminal) {
            return None;
        }
        let job = self.queues.get_mut(&terminal)?.pop_front()?;
        let toast = job.toast;
        self.start(job);
        Some(toast)
    }

    /// Toast ✕ (T15). A running job: flag + kill the child; the worker wakes,
    /// cleans up its unverified names and sends `Cancelled`. A queued job is
    /// removed directly — its toast id is returned for the immediate
    /// cancelled-morph (no worker exists to speak for it).
    pub fn cancel(&mut self, job_id: u64) -> Option<ToastId> {
        if let Some(r) = self.running.values().find(|r| r.job_id == job_id) {
            r.cancel.store(true, Ordering::SeqCst);
            r.kill.kill();
            return None;
        }
        for q in self.queues.values_mut() {
            if let Some(idx) = q.iter().position(|j| j.job_id == job_id) {
                let job = q.remove(idx).expect("index from position");
                return Some(job.toast);
            }
        }
        None
    }

    /// GUI exit (§6.8): terminate live children — an orphaned hidden
    /// sftp.exe uploading forever is worse than a truncated partial. No
    /// resume-on-relaunch (v1, documented).
    pub fn shutdown(&mut self) {
        for r in self.running.values() {
            r.cancel.store(true, Ordering::SeqCst);
            r.kill.kill();
        }
        self.queues.clear();
    }

    fn start(&mut self, job: Job) {
        let cancel = Arc::new(AtomicBool::new(false));
        let kill = Arc::new(crate::ssh_transport::KillSlot::new());
        self.running.insert(
            job.terminal,
            Running {
                job_id: job.job_id,
                toast: job.toast,
                host: job.host.clone(),
                files: job.files.clone(),
                cancel: cancel.clone(),
                kill: kill.clone(),
            },
        );
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let name = format!("ssh-drop-{}", job.job_id);
        let _ = std::thread::Builder::new().name(name).spawn(move || {
            let w = Worker {
                job,
                cancel,
                kill,
                tx,
                ctx,
            };
            w.run();
        });
    }
}

// ─────────────────────────── worker (§6.3) ───────────────────────────

struct Worker {
    job: Job,
    cancel: Arc<AtomicBool>,
    kill: Arc<crate::ssh_transport::KillSlot>,
    tx: Sender<Event>,
    ctx: egui::Context,
}

impl Worker {
    fn send(&self, ev: Event) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn run(&self) {
        let terminal = self.job.terminal;
        let job_id = self.job.job_id;
        // 1. Resolve sftp beside the session's ssh (T4), else PATH.
        let sftp = match resolve_sftp(&self.job.program) {
            Ok(p) => p,
            Err(looked) => {
                return self.send(Event::ConnFailed {
                    terminal,
                    job_id,
                    err: ConnErr::SftpMissing { looked },
                })
            }
        };

        // 2. Local pre-flight: unreadable files get their row-9 verdict up
        // front; the rest still upload (§6.3.2).
        let mut verdicts: Vec<Option<Result<String, FileErr>>> =
            vec![None; self.job.files.len()];
        let mut live: Vec<(usize, PathBuf, u64)> = Vec::new();
        for (i, p) in self.job.files.iter().enumerate() {
            match std::fs::File::open(p).and_then(|f| f.metadata()) {
                Ok(md) if md.is_file() => live.push((i, p.clone(), md.len())),
                _ => verdicts[i] = Some(Err(FileErr::LocalUnreadable)),
            }
        }
        let finish = |verdicts: Vec<Option<Result<String, FileErr>>>, home: String| {
            Event::Done {
                terminal,
                job_id,
                home,
                verdicts: self
                    .job
                    .files
                    .iter()
                    .cloned()
                    .zip(
                        verdicts
                            .into_iter()
                            .map(|v| v.unwrap_or(Err(FileErr::LocalUnreadable))),
                    )
                    .collect(),
            }
        };
        if live.is_empty() {
            return self.send(finish(verdicts, String::new()));
        }
        if self.cancelled() {
            return self.send(Event::Cancelled { terminal, job_id });
        }

        // 3. Conn 1 — probe (pwd + mkdir + ls -1).
        let out1 = match self.run_batch(&sftp, &conn1_batch(), "1") {
            Ok(o) => o,
            Err(_) => {
                return self.send(Event::ConnFailed {
                    terminal,
                    job_id,
                    err: ConnErr::SftpMissing {
                        looked: sftp.to_string_lossy().into_owned(),
                    },
                })
            }
        };
        if self.cancelled() {
            return self.send(Event::Cancelled { terminal, job_id });
        }
        let stderr1 = String::from_utf8_lossy(&out1.stderr);
        match out1.status.code() {
            Some(0) => {}
            Some(255) => {
                return self.send(Event::ConnFailed {
                    terminal,
                    job_id,
                    err: classify_conn(&stderr1),
                })
            }
            _ => {
                // Exit 1: the batch aborted at `ls` ⇒ .tc-drops could not be
                // created (mkdir was ignore-prefixed) — §7 row 7.
                return self.send(Event::ConnFailed {
                    terminal,
                    job_id,
                    err: ConnErr::MkdirDenied,
                });
            }
        }
        let stdout1 = String::from_utf8_lossy(&out1.stdout);
        let Some(home) = parse_pwd(&stdout1) else {
            return self.send(Event::ConnFailed {
                terminal,
                job_id,
                err: ConnErr::Other("could not read the remote home directory".into()),
            });
        };
        let existing = parse_ls1(&stdout1);

        // 4. Conn 2 — upload + verify tail.
        let paths: Vec<PathBuf> = live.iter().map(|(_, p, _)| p.clone()).collect();
        let pairs = resolve_names(&paths, &existing);
        let all_names: Vec<String> = pairs.iter().map(|(_, n)| n.clone()).collect();
        let out2 = match self.run_batch(&sftp, &conn2_batch(&pairs), "2") {
            Ok(o) => o,
            Err(_) => {
                return self.send(Event::ConnFailed {
                    terminal,
                    job_id,
                    err: ConnErr::SftpMissing {
                        looked: sftp.to_string_lossy().into_owned(),
                    },
                })
            }
        };
        if self.cancelled() {
            // Mid-transfer cancel: everything this conn attempted is
            // unverified — our garbage, remove it (T15/inv. 6).
            self.cleanup(&sftp, &all_names);
            return self.send(Event::Cancelled { terminal, job_id });
        }
        let stderr2 = String::from_utf8_lossy(&out2.stderr);
        if out2.status.code() == Some(255) {
            // Connection died mid-upload (§7 row 11): the ls tail never ran,
            // nothing is verified, nothing is pasted.
            self.cleanup(&sftp, &all_names);
            return self.send(Event::ConnFailed {
                terminal,
                job_id,
                err: ConnErr::Dropped,
            });
        }

        // 5. Verdicts: name+size in the ls -l tail is the ONLY success
        // authority (inv. 5/T8); stderr only supplies the failure reason.
        let listed = parse_ls_l(&String::from_utf8_lossy(&out2.stdout));
        let mut failed_names: Vec<String> = Vec::new();
        for ((idx, local, size), (_, final_name)) in live.iter().zip(pairs.iter()) {
            let verified = listed
                .iter()
                .any(|(n, s)| n == final_name && s == size);
            verdicts[*idx] = Some(if verified {
                Ok(final_name.clone())
            } else {
                failed_names.push(final_name.clone());
                let local_fwd = local.to_string_lossy().replace('\\', "/");
                Err(classify_file(&local_fwd, final_name, &stderr2))
            });
        }

        // 6. Cleanup failed/partial names (inv. 6 — never a verified file).
        if !failed_names.is_empty() {
            self.cleanup(&sftp, &failed_names);
        }
        self.send(finish(verdicts, home));
    }

    /// One sftp batch connection: temp batch file → spawn hidden → wait,
    /// through the shared `ssh_transport::run_sftp` (no watchdog: uploads
    /// rely on the child's own ServerAlive clock + the explicit cancel path,
    /// which kills through the published pid). stdout is parse-after-exit
    /// ONLY (fully buffered over pipes, DO-NOT 2).
    fn run_batch(
        &self,
        sftp: &Path,
        batch: &str,
        tag: &str,
    ) -> std::io::Result<std::process::Output> {
        let bpath = std::env::temp_dir().join(format!("tc-drop-{}.{tag}", self.job.job_id));
        // UTF-8, no BOM, LF — proven round-trip for spaces/unicode (§9).
        crate::ssh_transport::write_batch(&bpath, batch)?;
        let bstr = bpath.to_string_lossy().into_owned();
        let args = match std::env::var("TC_SSH_DROP_TRANSPORT") {
            Ok(t) if !t.is_empty() => sftp_args_transport(&t, &bstr),
            _ => sftp_args(&self.job.args, &bstr),
        };
        let out = crate::ssh_transport::run_sftp(sftp, &args, None, Some(&self.kill));
        let _ = std::fs::remove_file(&bpath);
        out
    }

    /// Conn 3: best-effort, output discarded, never toasted (§3.1). Skipped
    /// entirely when the GUI is going down (shutdown just kills pids).
    fn cleanup(&self, sftp: &Path, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let _ = self.run_batch(sftp, &conn3_batch(names), "3");
    }
}

/// ServerAlive bound: a dead link fails within ~45s (15s × 3) — documented
/// for the row-11 path; nothing polls this, it's the sftp child's own clock.
#[allow(dead_code)]
pub const DEAD_LINK_BOUND: Duration = Duration::from_secs(45);

// ─────────────────────────── tests (§9.3, upload-shaped) ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn hs(v: &[&str]) -> HashSet<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // ── resolve_names ──

    #[test]
    fn resolve_names_clean_and_collisions() {
        let out = resolve_names_at(&[PathBuf::from(r"C:\a\shot.png")], &hs(&[]), 7);
        assert_eq!(out[0].1, "shot.png");
        let out = resolve_names_at(&[PathBuf::from(r"C:\a\shot.png")], &hs(&["shot.png"]), 7);
        assert_eq!(out[0].1, "shot-2.png");
        // Batch-internal collision: same filename from two directories.
        let out = resolve_names_at(
            &[PathBuf::from(r"C:\a\shot.png"), PathBuf::from(r"C:\b\shot.png")],
            &hs(&[]),
            7,
        );
        assert_eq!(out[0].1, "shot.png");
        assert_eq!(out[1].1, "shot-2.png");
    }

    #[test]
    fn resolve_names_dotfile_and_no_ext() {
        let out = resolve_names_at(&[PathBuf::from(r"C:\a\.bashrc")], &hs(&[".bashrc"]), 7);
        assert_eq!(out[0].1, ".bashrc-2");
        let out = resolve_names_at(&[PathBuf::from(r"C:\a\README")], &hs(&["README"]), 7);
        assert_eq!(out[0].1, "README-2");
    }

    #[test]
    fn resolve_names_99_rollover_to_timestamp() {
        let mut existing: Vec<String> = vec!["shot.png".into()];
        existing.extend((2..=99).map(|n| format!("shot-{n}.png")));
        let existing: HashSet<String> = existing.into_iter().collect();
        let out = resolve_names_at(&[PathBuf::from(r"C:\a\shot.png")], &existing, 1234);
        assert_eq!(out[0].1, "shot-1234.png");
    }

    // ── batches + paste ──

    #[test]
    fn conn_batches_shape() {
        assert_eq!(conn1_batch(), "pwd\n-mkdir .tc-drops\nls -1 .tc-drops\n");
        let pairs = vec![
            (PathBuf::from(r"C:\My Shots\local one.png"), "final one.png".to_string()),
            (PathBuf::from(r"C:\x\two.txt"), "two.txt".to_string()),
        ];
        assert_eq!(
            conn2_batch(&pairs),
            "-put \"C:/My Shots/local one.png\" \".tc-drops/final one.png\"\n\
             -put \"C:/x/two.txt\" \".tc-drops/two.txt\"\n\
             ls -l .tc-drops\n"
        );
        assert_eq!(
            conn3_batch(&["a b.png".to_string()]),
            "-rm \".tc-drops/a b.png\"\n"
        );
    }

    /// §7.1 quoting golden: absolute POSIX single-quoted, `'` escaped the
    /// POSIX way, one trailing space.
    #[test]
    fn paste_text_golden() {
        assert_eq!(
            paste_text("/home/z", &["a.png".to_string(), "it's.png".to_string()]),
            r"'/home/z/.tc-drops/a.png' '/home/z/.tc-drops/it'\''s.png' "
        );
    }

    // ── toast copy spot checks ──

    #[test]
    fn conn_err_toast_names_host_and_files() {
        let (t, d) = conn_err_toast(&ConnErr::Timeout, "192.0.2.14", &["a.png".into()]);
        assert_eq!(t, "192.0.2.14 didn't answer");
        assert_eq!(d, vec!["network timeout — 1 file not uploaded".to_string()]);
        let (t, d) = conn_err_toast(
            &ConnErr::SftpMissing { looked: r"C:\W\ssh".into() },
            "h",
            &[],
        );
        assert_eq!(t, "can't upload — sftp.exe not found");
        assert!(d[0].contains(r"looked beside C:\W\ssh"));
        let (t, d) = conn_err_toast(&ConnErr::Dropped, "h", &["a.png".into(), "b.png".into()]);
        assert_eq!(t, "connection to h was lost");
        assert!(d.contains(&"a.png".to_string()) && d.contains(&"b.png".to_string()));
        assert_eq!(d.last().unwrap(), "did not finish — nothing pasted for them");
    }

    #[test]
    fn file_err_rows_copy() {
        let (t, d) = file_err_toast("big.bin", &FileErr::RemoteWriteFailed, "h");
        assert_eq!(t, "upload failed: big.bin");
        assert_eq!(d, vec!["h couldn't write it — disk full or quota?".to_string()]);
        let (t, _) = file_err_toast("a.png", &FileErr::LocalUnreadable, "h");
        assert_eq!(t, "can't read a.png");
        assert_eq!(
            file_err_line("f.txt", &FileErr::DestDenied, "h"),
            "f.txt — write refused in ~/.tc-drops"
        );
    }
}
