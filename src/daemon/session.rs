//! A live PTY session owned by the daemon.
//!
//! The daemon runs a headless VT parser per session and is the single
//! authoritative responder to terminal queries (DSR, DA, OSC color, size).
//! Without this, a shell spawned while no GUI is attached — e.g. during boot
//! auto-restore — stalls waiting for a cursor-position report.

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{self, test::TermSize, Term};
use alacritty_terminal::vte::ansi::Rgb;
use parking_lot::Mutex;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use crate::palette;
use crate::state::{path_namespace, shell_family, PathNamespace, ShellFamily, TerminalMeta};

/// Redefines the PowerShell `prompt` so each render emits OSC 9;9;<location>.
/// PowerShell does not update its process cwd on Set-Location, so this is the
/// only way to observe its live location. Single-quoted only (no inner `"`),
/// so it survives command-line quoting; wraps the existing prompt, no $PROFILE.
///
/// FALLBACK only: normal pwsh spawns dot-source the per-terminal bootstrap
/// file instead (which emits this same OSC 9;9 plus the block hooks); this
/// inline wrapper keeps cwd tracking alive if the bootstrap file could not be
/// written (disk error).
const PS_PROMPT_WRAPPER: &str = "$o=$function:prompt; function prompt { $p=$ExecutionContext.SessionState.Path.CurrentLocation.Path; [Console]::Write(([char]27+']9;9;'+$p+[char]7)); if($o){& $o}else{'PS '+$p+'> '} }";

/// Dot-source statement for the bootstrap script, single-quoted with embedded
/// quotes doubled (same escaping as the Set-Location below).
fn dot_source(script: &Path) -> String {
    format!(". '{}'", script.to_string_lossy().replace('\'', "''"))
}

/// PowerShell args for a restore-spawn: the bootstrap dot-source (block
/// hooks plus OSC 9;9 cwd; falls back to the inline prompt wrapper when no bootstrap
/// file exists), then an optional `Set-Location`, then an optional trailing
/// command (e.g. `claude --resume …`), all as one `-Command` so they run in
/// the interactive session (-NoExit).
pub fn powershell_restore_command(
    bootstrap: Option<&Path>,
    set_location: Option<&Path>,
    trailing: Option<&str>,
) -> Vec<String> {
    let mut script = match bootstrap {
        Some(p) => dot_source(p),
        None => PS_PROMPT_WRAPPER.to_string(),
    };
    if let Some(cwd) = set_location {
        // Single-quote LiteralPath; escape embedded single quotes by doubling.
        let esc = cwd.to_string_lossy().replace('\'', "''");
        script.push_str(&format!("; Set-Location -LiteralPath '{esc}'"));
    }
    if let Some(cmd) = trailing {
        script.push_str("; ");
        script.push_str(cmd);
    }
    vec!["-NoExit".into(), "-Command".into(), script]
}

use super::{Core, ImmediateProcessor};

pub struct Session {
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Arc so resize sites clone it out and call ResizePseudoConsole OUTSIDE
    /// the global sessions mutex (a busy/wedged conhost must never stall the
    /// whole fleet's input). The per-session mutex serializes concurrent
    /// resizes of THIS terminal — the correct granularity.
    pub master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub killer: Box<dyn ChildKiller + Send + Sync>,
    /// Monotonic spawn generation: `on_exit` is keyed by (id, gen) so a stale
    /// exit/panic from a PREVIOUS process can never tear down a successor
    /// session that reused the terminal id (see Core::launching's doc for the
    /// double-launch flavor of the same hazard).
    pub gen: u64,
    /// Mirror of conhost's world — contains EXACTLY what the PTY has emitted
    /// this session and nothing else, so its coordinates (which answer the
    /// shell's DSR queries) can never diverge from conhost's, including
    /// across resizes. Older sessions' content lives in `preface`.
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    /// Pre-rendered older-session content, prepended at attach time.
    pub preface: Arc<Mutex<super::serialize::Preface>>,
    /// pid of the root shell process (conhost is outside this tree), for
    /// PEB-based process-tree tracking.
    pub root_pid: Option<u32>,
    /// Freshest cwd reported by an OSC 7 / 9;9 sequence, if the shell emits one.
    pub osc_cwd: Arc<Mutex<Option<PathBuf>>>,
    /// Latest win32-input-mode (DECSET 9001) state conhost requested. vte
    /// ignores private mode 9001, so a raw scan in the reader thread tracks
    /// it; attach replays re-assert it so late-attaching clients encode keys
    /// as win32 key events (see `crate::win32_input`).
    pub win32_input: Arc<AtomicBool>,
    /// Cold-attach prompt certification (task #15): `Some(col)` while the
    /// mirror sits at a clean interactive prompt end (a scanned OSC 133;B with
    /// no command running since), holding the cursor column where the shell's
    /// prompt text ended; `None` otherwise (command running, or between a
    /// closing `pre` and the next prompt's 133;B). The reader thread maintains
    /// it from the same hook scan that feeds blocks; Attach reads it (with the
    /// block store's open state and the post-resize mirror cursor) to tell the
    /// GUI where to arm its composer on app open.
    pub prompt: Arc<Mutex<Option<usize>>>,
    /// Wall-clock ms of the last PTY output chunk (now_ms domain). Written by
    /// the reader thread AFTER the chunk is ingested/journaled (one Relaxed
    /// store per read), read by the controller's List for idle_ms/activity
    /// (P5) and the Shutdown drain.
    pub last_output: Arc<AtomicU64>,
    /// Bytes read from the PTY that are not yet journaled: incremented by the
    /// reader thread the moment a read() returns, decremented by the ingest
    /// thread only after the batch is parsed+journaled+fanned out (L-7). The
    /// Shutdown drain treats >0 as busy regardless of stamp age, so "quiet"
    /// proves the last bytes are journaled — not merely read, and not sitting
    /// in the reader→ingest channel. Happens-before for the journaled bytes
    /// rides the journal mutex that flush_all also takes.
    pub in_flight: Arc<AtomicU64>,
    /// SLEEP freeze-frame: set by `sleep_terminals` between the frame capture
    /// and the kill. The reader keeps JOURNALING every byte (mirror purity —
    /// the teardown wipe is real conhost output), but live fanout stops, so
    /// attached clients keep the frozen frame on screen instead of parsing
    /// the dying TUI's graceful-exit wipe (claude's `?1049l` + erase) and the
    /// ConPTY mode-reset trailer. Dies with the Session; a wake's successor
    /// session starts un-muted. One relaxed load per ingest batch — nothing
    /// on the awake path.
    pub mute_fanout: Arc<AtomicBool>,
}

/// Claude-Code SESSION-LINEAGE markers that must never reach a spawned
/// terminal's environment (see the scrub in `spawn()`): the exact-name set
/// plus the whole `CLAUDE_CODE_*` prefix. Everything else is kept —
/// deliberately NOT scrubbed: `ANTHROPIC_*` (user-level API config),
/// `CLAUDE_CONFIG_DIR` (a user-set global changes claude's home on purpose),
/// `AI_AGENT` (origin uncertain; harmless to persistence).
pub(crate) fn is_claude_session_var(name: &str) -> bool {
    name.eq_ignore_ascii_case("CLAUDECODE")
        || name.eq_ignore_ascii_case("CLAUDE_EFFORT")
        || name.to_ascii_uppercase().starts_with("CLAUDE_CODE_")
}

/// Synthesize the spawned ssh argv (P6c §3.4.1): user flags verbatim, then
/// our additions — `-t` (only when a remote command rides along; ssh would
/// otherwise suppress the tty), the ServerAlive keepalive pair (AFTER user
/// args: ssh takes the FIRST occurrence of an option, so a user-supplied
/// ServerAlive* wins automatically, Q7) — then the destination, then the
/// one-shot remote command. The classifier guarantees the destination is the
/// LAST user arg (anything after it would be a remote command we didn't
/// synthesize ⇒ family Other, never hooked).
pub(crate) fn synth_ssh_args(user_args: &[String], remote_cmd: Option<&str>) -> Vec<String> {
    let Some((host, flags)) = user_args.split_last() else {
        return Vec::new(); // unreachable for family Ssh (a destination exists)
    };
    let mut out: Vec<String> = flags.to_vec();
    if remote_cmd.is_some() {
        out.push("-t".into());
    }
    // 30s×4 = a link survives ≤2min of silence (Wi-Fi blips, brief PC
    // sleeps) WITH its remote state intact; softened from 15×3=45s once
    // auto-reconnect existed to catch real deaths (field evidence: two
    // hooked sessions killed at the same second across a PC sleep). A
    // user-supplied ServerAlive* still wins (first occurrence).
    out.extend([
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ServerAliveCountMax=4".into(),
    ]);
    out.push(host.clone());
    if let Some(rc) = remote_cmd {
        out.push(rc.to_string());
    }
    out
}

/// Synthesize the wsl.exe argv tail (P6a §3.1.1): `--cd <cwd>`, then
/// `--exec /bin/sh -c <guard>`. `--cd` accepts Windows paths, leading-/
/// POSIX paths, AND `~` — all resolved by wsl.exe itself (fresh explicit
/// dirs pass the Windows path, restores the tracked POSIX live_cwd, and
/// v0.1.1 default rows pass `~` = the distro user's LINUX home).
///
/// An EMPTY/whitespace cwd emits `--cd ~` (v0.1.1): the old no-flag path
/// did NOT "start in the distro home" — wsl.exe inherits the parent's
/// Windows CWD, and portable-pty defaults the child's lpCurrentDirectory to
/// %USERPROFILE% when no cwd is set (cmdbuilder.rs `cwd.or(home)`), so a
/// flag-less spawn landed in /mnt/c/Users/<u> on every distro. (`--cd ""`
/// remains Wsl/E_INVALIDARG — the 2026-07-04 regression — which is why the
/// empty case must never pass the raw string.) `rc_mnt` is the
/// /mnt-translated rcfile path, already vetted single-quote-free by the
/// caller and single-quoted inside the guard ($-expansion-proof); None
/// degrades to plain `exec bash -i` (hookless, never broken).
pub(crate) fn synth_wsl_args(cwd: &Path, rc_mnt: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let cwd_str = cwd.to_string_lossy();
    out.push("--cd".into());
    if cwd_str.trim().is_empty() {
        out.push("~".into());
    } else {
        out.push(cwd_str.into_owned());
    }
    out.push("--exec".into());
    out.push("/bin/sh".into());
    out.push("-c".into());
    out.push(match rc_mnt {
        Some(rc) => format!(
            "if [ -r '{rc}' ]; then exec bash --rcfile '{rc}' -i; else exec bash -i; fi"
        ),
        None => "exec bash -i".into(),
    });
    out
}

/// Source of `Session.gen`: process-global so generations are unique across
/// every terminal id (only per-id monotonicity is needed; global is simpler).
static SPAWN_GEN: AtomicU64 = AtomicU64::new(1);

pub const DEFAULT_COLS: u16 = 160;
pub const DEFAULT_ROWS: u16 = 42;
const NOMINAL_CELL_W: u16 = 8;
const NOMINAL_CELL_H: u16 = 16;

pub struct EventProxy(mpsc::Sender<Event>);

impl EventProxy {
    pub fn new(tx: mpsc::Sender<Event>) -> Self {
        Self(tx)
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

/// Resolve a bare program name the way the shell would, honoring PATHEXT.
/// Returns (program, prefix_args): .cmd/.bat scripts must run under cmd.exe.
fn resolve_program(program: &str) -> (String, Vec<String>) {
    let p = Path::new(program);
    let has_path = p.components().count() > 1;
    let candidates: Vec<PathBuf> = if has_path || p.extension().is_some() {
        vec![PathBuf::from(program)]
    } else {
        let mut found = Vec::new();
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                for ext in ["exe", "com", "cmd", "bat"] {
                    let cand = dir.join(format!("{program}.{ext}"));
                    if cand.is_file() {
                        found.push(cand);
                    }
                }
                if !found.is_empty() {
                    break;
                }
            }
        }
        found
    };

    let resolved = candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from(program));
    let ext = resolved
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "cmd" || ext == "bat" {
        (
            "cmd.exe".to_string(),
            vec!["/c".to_string(), resolved.to_string_lossy().to_string()],
        )
    } else {
        (resolved.to_string_lossy().to_string(), Vec::new())
    }
}

/// Spawn the terminal's process on a fresh ConPTY, wire up the reader thread
/// (parse + respond + journal + broadcast) and the exit-watcher thread.
///
/// `preface` carries older sessions' pre-rendered content for attach-time
/// reconstruction. It is deliberately NOT fed into the mirror Term: the
/// mirror must contain exactly what conhost has seen, or their coordinate
/// systems diverge on resize (alacritty would pull preface rows from
/// scrollback that conhost never had) and the shell's DSR-guided drawing
/// lands rows away from the prompt.
pub fn spawn(
    core: Arc<Core>,
    meta: &TerminalMeta,
    cols: u16,
    rows: u16,
    preface: super::serialize::Preface,
    bootstrap: Option<&Path>,
) -> anyhow::Result<Session> {
    let id = meta.id;
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let (program, args) = meta.launch_command();
    let (mut resolved, mut full_args) = resolve_program(&program);
    full_args.extend(args);

    // Per-family hook injection (argv/env/file only — never typed into the
    // PTY, never fed to the mirror: P6 inv. 1). Classified from the ORIGINAL
    // meta shape (the synthesized tails below are per-spawn, never persisted).
    let family = shell_family(&meta.kind, &meta.program, &meta.args);
    match &family {
        // Plain PowerShell: dot-source the per-spawn bootstrap file (block
        // hooks + the OSC 9;9 cwd report). Falls back to the inline prompt
        // wrapper (cwd only, no blocks) if the bootstrap file could not be
        // generated.
        ShellFamily::Pwsh => {
            full_args.push("-NoExit".into());
            full_args.push("-Command".into());
            full_args.push(match bootstrap {
                Some(p) => dot_source(p),
                None => PS_PROMPT_WRAPPER.into(),
            });
        }
        // WSL (P6a §3.1.1): explicit bash inside the distro, hooked via the
        // per-spawn rcfile reached over the /mnt automount. The argv tail is
        // synthesized by `synth_wsl_args` (golden-tested — see its doc for
        // the --cd/empty-cwd/guard contract); this arm only vets the rc path:
        // a Windows path can legally contain a single quote, in which case we
        // degrade with a log line rather than mis-quote the guard.
        ShellFamily::WslShell { .. } => {
            let rc_mnt = match bootstrap.map(super::bootstrap::wsl_mnt_path) {
                Some(Some(rc)) if rc.contains('\'') => {
                    log::warn!(
                        "terminal {id}: bootstrap path contains a single quote; spawning hookless"
                    );
                    None
                }
                Some(Some(rc)) => Some(rc),
                Some(None) => {
                    log::warn!(
                        "terminal {id}: bootstrap path not drive-letter-translatable to /mnt; spawning hookless"
                    );
                    None
                }
                None => None,
            };
            if meta.cwd.to_string_lossy().trim().is_empty() {
                log::info!(
                    "terminal {id}: empty cwd — wsl spawn starts in the Linux home (--cd ~)"
                );
            }
            full_args.extend(synth_wsl_args(&meta.cwd, rc_mnt.as_deref()));
        }
        // ssh (P6c §3.4.1): the hooks ride a one-shot remote bootstrap in the
        // ssh command line — mktemp + base64-decoded rc + `exec bash
        // --rcfile`, POSIX-quoted so ANY remote login shell survives it, and
        // self-healing to plain bash/sh when the pieces are missing (D9). No
        // persistent remote mutation, ever. User args come FIRST, our
        // ServerAlive keepalives after (ssh first-occurrence-wins ⇒ user
        // overrides are automatic, D13/Q7); `-t` forces the interactive tty a
        // remote command would otherwise suppress. remote_hooks:false (or an
        // unreadable rcfile) degrades to plain `ssh <args> <host>` — still
        // keepalive-guarded so link death turns Dead within ~45s.
        ShellFamily::Ssh { host } => {
            let rc_bytes: Option<Vec<u8>> = bootstrap.and_then(|p| match std::fs::read(p) {
                Ok(rc) if !rc.is_empty() => Some(rc),
                _ => {
                    log::warn!("terminal {id}: ssh bootstrap rc unreadable; spawning hookless");
                    None
                }
            });
            if rc_bytes.is_some()
                && std::env::var("TC_SSH_VIA_WSL").is_ok_and(|v| v == *host)
                && crate::state::data_dir_overridden()
            {
                // Probe-only WSL transport stand-in (§12 P6): for the ONE
                // host named by TC_SSH_VIA_WSL (never a blanket hijack — the
                // probe's full `ssh 127.0.0.1` variant must ride the real
                // link), execute the EXACT remote bootstrap body through a
                // real ConPTY in the default WSL distro, bypassing only the
                // ssh transport — proves mktemp/base64/exec-bash/rc/
                // self-delete end-to-end. Gated on TC_DATA_DIR isolation so
                // an installed daemon can never take this path.
                log::info!("terminal {id}: TC_SSH_VIA_WSL transport stand-in active");
                resolved = "wsl.exe".into();
                full_args = vec![
                    "--exec".into(),
                    "/bin/sh".into(),
                    "-c".into(),
                    super::bootstrap::ssh_bootstrap_body(rc_bytes.as_deref().unwrap()),
                ];
            } else {
                let remote = rc_bytes
                    .as_deref()
                    .map(super::bootstrap::ssh_remote_command);
                full_args = synth_ssh_args(&full_args, remote.as_deref());
            }
        }
        // Cmd rides an env var (below); Other is the pre-P6 path.
        _ => {}
    }

    // cmd.exe (P6b §3.3): the hooks ride the PROMPT environment variable —
    // per-process, macro-expanded at every render, never typed into the PTY
    // and never a machine-global mutation (AutoRun is forbidden, DO-NOT #1).
    // launch() wrote the generated value into the bootstrap file (token
    // rotates per spawn); a missing/unreadable file degrades to a hookless
    // working cmd rather than a broken one.
    let cmd_prompt_env: Option<String> = if matches!(family, ShellFamily::Cmd) {
        match bootstrap.map(std::fs::read_to_string) {
            Some(Ok(v)) if !v.trim().is_empty() => Some(v.trim().to_string()),
            Some(_) => {
                log::warn!("terminal {id}: cmd PROMPT bootstrap unreadable; spawning hookless");
                None
            }
            None => None,
        }
    } else {
        None
    };

    let mut cmd = CommandBuilder::new(&resolved);
    // ENV LINEAGE SCRUB (the "sleeping claude never resumes" root cause,
    // field-proven 2026-07-04): the daemon is frequently (re)started by
    // `--install` from a shell INSIDE a Claude Code session (every agent-run
    // install), so its environment carries Claude-Code session markers —
    // and Claude Code treats a process with CLAUDE_CODE_CHILD_SESSION (or
    // sibling markers) in its environment as a CHILD session and SKIPS
    // transcript persistence entirely: no ~/.claude/projects/<munged>/
    // <session-id>.jsonl is ever written, `claude --resume <id>` says "No
    // conversation found", and launch_command's jsonl-exists check can only
    // ever pick the fresh `--session-id` branch. Boot daemons (HKCU Run
    // key) had a clean env — which is why resume USED to work. Every
    // terminal must behave as if launched from a normal shell, whatever
    // spawned the daemon.
    for (name, _) in std::env::vars() {
        if is_claude_session_var(&name) {
            cmd.env_remove(&name);
        }
    }
    if let Some(v) = &cmd_prompt_env {
        cmd.env("PROMPT", v);
    }
    // WSLENV relays TC_SESSION_ID into the distro (and back out through
    // interop), so a tc.exe invoked from inside the WSL shell still
    // self-identifies for the recursion guard.
    if matches!(family, ShellFamily::WslShell { .. }) {
        let wslenv = match std::env::var("WSLENV") {
            Ok(v) if !v.is_empty() => format!("{v}:TC_SESSION_ID"),
            _ => "TC_SESSION_ID".to_string(),
        };
        cmd.env("WSLENV", wslenv);
    }
    // Recursion guard (P5): the whole ConPTY tree — shell, claude, any `tc`
    // an agent runs inside it — can identify its own host terminal. `tc`
    // forwards it as HelloCtl.self_session; the daemon then refuses
    // input/kill/restart/delete against that id unless forced.
    cmd.env("TC_SESSION_ID", id.to_string());
    cmd.args(&full_args);
    // Posix-namespace cwds (WSL rides `--cd` above; ssh cds remotely inside
    // the rc) never touch the Windows process cwd — `.is_dir()` on a "/tmp"
    // PathBuf resolves drive-relative on Windows (DO-NOT #4).
    if matches!(path_namespace(&family), PathNamespace::Win) && meta.cwd.is_dir() {
        cmd.cwd(&meta.cwd);
    }

    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let root_pid = child.process_id();
    // The session's terminal infrastructure runs at High QoS: under a
    // background daemon the whole ConPTY pipeline (shell + conhost) is
    // background-QoS, and any foreground load (a game) parks it on E-cores —
    // conhost is the pipeline's throughput ceiling, so output crawls. A
    // foreground terminal's children get normal scheduling for free; ours
    // need the explicit opt-out. QoS is not inherited: whatever the user
    // launches from the shell keeps default OS policy. The conhost for this
    // pseudoconsole is a direct child of the daemon with no exposed pid, so
    // sweep all conhost children (idempotent; they all belong to sessions).
    if let Some(pid) = root_pid {
        super::procinfo::set_high_qos(pid);
    }
    let daemon_pid = std::process::id();
    for (pid, ppid, exe) in super::procinfo::snapshot_processes() {
        if ppid == daemon_pid && exe.eq_ignore_ascii_case("conhost.exe") {
            super::procinfo::set_high_qos(pid);
        }
    }
    let killer = child.clone_killer();
    let gen = SPAWN_GEN.fetch_add(1, Ordering::Relaxed);
    let mut reader = pair.master.try_clone_reader()?;
    let writer: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(pair.master.take_writer()?));
    let osc_cwd: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let reader_osc = osc_cwd.clone();
    // Per-session path namespace for the OSC cwd scanner: WSL shells report
    // POSIX paths, which are stored verbatim (P6 §4).
    let ns = path_namespace(&family);
    let win32_input = Arc::new(AtomicBool::new(false));
    let reader_win32 = win32_input.clone();
    let prompt: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let reader_prompt = prompt.clone();
    let last_output = Arc::new(AtomicU64::new(super::now_ms()));
    let ingest_last_output = last_output.clone();
    let in_flight = Arc::new(AtomicU64::new(0));
    let reader_in_flight = in_flight.clone();
    let ingest_in_flight = in_flight.clone();
    let mute_fanout = Arc::new(AtomicBool::new(false));
    let ingest_mute = mute_fanout.clone();
    let spawned_at = Instant::now();

    let (event_tx, event_rx) = mpsc::channel();
    let term = Arc::new(FairMutex::new(Term::new(
        term::Config {
            // The daemon Term is the source of truth for attach-time
            // serialization, so it keeps real scrollback (bounded: memory is
            // ~cols × 2000 cells per session).
            scrolling_history: 2000,
            ..term::Config::default()
        },
        &TermSize::new(cols as usize, rows as usize),
        EventProxy(event_tx),
    )));

    // Reader → ingest pipeline. The PTY delivers output in line-sized pieces
    // (~275B average under a flood — measured), and paying the full
    // parse+journal+fanout+socket path per piece is what flood CPU was made
    // of (~194k frame builds + write syscalls per 50MB). The pipeline is
    // producer-limited — every stage keeps up with conhost — so no queue ever
    // forms on its own and coalescing must be DELIBERATE: the ingest thread
    // rate-gates exactly like the GUI's RepaintGate. A quiet stream (typing
    // echo, prompt paints) is processed the instant it lands — the leading
    // edge never waits, so felt latency is one channel hop (~µs). Only a
    // stream already proven to be flooding (≥32KiB processed inside the
    // current 50ms window) accumulates, with one 8ms sleep per batch; the
    // trailing edge always drains on the very next recv.
    let (chunk_tx, chunk_rx) = mpsc::sync_channel::<Vec<u8>>(1024);

    // Reader: PTY → channel. Deliberately lean — its per-piece cost is the
    // one thing that cannot be amortized (reads happen at conhost's write
    // granularity no matter what).
    let reader_panic_core = core.clone();
    std::thread::Builder::new()
        .name(format!("pty-read-{id}"))
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut buf = vec![0u8; 16 * 1024]; // conhost never delivers >4.3KB/read; 16KiB keeps a wide margin
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // In-flight marker FIRST (L-7): from here until
                            // the ingest thread's post-journal decrement, the
                            // Shutdown drain must treat this session as busy
                            // even if either thread stalls >300ms — bytes in
                            // this channel are read but NOT yet journaled.
                            reader_in_flight.fetch_add(n as u64, Ordering::Relaxed);
                            if chunk_tx.send(buf[..n].to_vec()).is_err() {
                                break; // ingest thread died; its handler surfaced it
                            }
                        }
                    }
                }
                // chunk_tx drops here (normal EOF and panic paths alike): the
                // ingest thread drains what remains in the channel, then exits.
            }));
            // A panic here would otherwise silently freeze the terminal;
            // surface it as an exit so the GUI shows Dead and restore works.
            if let Err(e) = result {
                log::error!("pty-read-{id} panicked: {:?}", panic_payload(&e));
                reader_panic_core.on_exit(id, gen, None);
            }
        })?;

    // Ingest: channel → parser (query responses) + journal + clients.
    let ingest_core = core.clone();
    let ingest_term = term.clone();
    let ingest_writer = writer.clone();
    let panic_core = core.clone();
    std::thread::Builder::new()
        .name(format!("pty-ingest-{id}"))
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut parser = ImmediateProcessor::new();
                let mut osc = OscScanner::new(ns);
                let mut win32_scan = crate::win32_input::ModeScanner::new();
                let mut block_scan = super::blocks::BlockScanner::new();
                // Rate gate: never wait below GATE bytes per WINDOW (typing
                // stays immediate); during a proven flood accumulate ACCUM
                // per batch. BATCH_CAP bounds memory and journal-lock holds.
                const GATE: usize = 32 * 1024;
                const BATCH_CAP: usize = 256 * 1024;
                const WINDOW: std::time::Duration = std::time::Duration::from_millis(50);
                const ACCUM: std::time::Duration = std::time::Duration::from_millis(8);
                fn drain(batch: &mut Vec<u8>, rx: &mpsc::Receiver<Vec<u8>>) {
                    while batch.len() < BATCH_CAP {
                        match rx.try_recv() {
                            Ok(c) => batch.extend_from_slice(&c),
                            Err(_) => break,
                        }
                    }
                }
                let mut batch: Vec<u8> = Vec::with_capacity(BATCH_CAP);
                let mut win_start = Instant::now();
                let mut win_bytes: usize = 0;
                // Boot-timeline marker (perf-wave-3): the first hooked prompt
                // is "this session is interactive again" — the restore metric.
                let mut first_prompt_pending = super::perf::on();
                while let Ok(first) = chunk_rx.recv() {
                    batch.clear();
                    batch.extend_from_slice(&first);
                    drain(&mut batch, &chunk_rx);
                    let now = Instant::now();
                    if now.duration_since(win_start) >= WINDOW {
                        win_start = now;
                        win_bytes = 0;
                    }
                    if win_bytes >= GATE && batch.len() < BATCH_CAP {
                        // Mid-flood: sleep one beat so the batch fills. A
                        // plain sleep (not recv_timeout) keeps the reader's
                        // sends wakeup-free — nobody is parked on the channel.
                        std::thread::sleep(ACCUM);
                        drain(&mut batch, &chunk_rx);
                    }
                    win_bytes += batch.len();
                    // vte drops OSC 7 / 9;9, so scan the raw bytes for a cwd
                    // report before/besides the parser. Fed BEFORE the block
                    // scan below (P6b): cmd's PROMPT renders the tokenless
                    // 9;9 immediately before the token-bearing `pre` in the
                    // same output burst, and on_block_event substitutes the
                    // pre's empty cwd payload from Session.osc_cwd — the
                    // scanner must have seen this batch's 9;9 by then.
                    super::perf::time(&super::perf::SCAN_NS, || {
                        osc.feed(&batch, &reader_osc);
                        // vte also ignores private mode 9001 — track
                        // conhost's win32-input-mode request the same way.
                        if let Some(on) = win32_scan.feed(&batch) {
                            reader_win32.store(on, Ordering::Relaxed);
                        }
                    });
                    // Parse + journal + fanout happen atomically under the
                    // journal lock (Core::ingest), so an Attach serialization
                    // can never observe a chunk in the Term that hasn't been
                    // fanned out yet (which the client would then receive
                    // twice). ≤64KiB slices keep any single journal-lock hold
                    // (the Attach serialization point) bounded and each Output
                    // frame within the granularity clients already assume.
                    for chunk in batch.chunks(64 * 1024) {
                        // ingest returns the chunk's absolute stream offset
                        // (pre-append), anchoring block events to journal
                        // coordinates.
                        let chunk_off = ingest_core.ingest(
                            id,
                            &ingest_term,
                            &mut parser,
                            chunk,
                            ingest_mute.load(Ordering::Relaxed),
                        );
                        // Block hooks are scanned per ingested chunk so each
                        // event's offset is exact even if a daemon-authored
                        // append (seam bytes) lands between chunks. Handled
                        // AFTER ingest returns — the journal lock is released;
                        // the blocks lock is a leaf. Benign race: an Attach
                        // between ingest and on_block_event sees the store a
                        // moment stale — harmless, block frames are
                        // idempotent upserts.
                        for ev in super::perf::time(&super::perf::SCAN_NS, || {
                            block_scan.feed(chunk)
                        }) {
                            // Cold-attach prompt certification: the chunk is
                            // already parsed into the mirror, so at a 133;B
                            // (which the bootstrap's 15ms drain lands alone)
                            // the mirror cursor sits exactly at the prompt
                            // end. Latch its column; a running command (Exec)
                            // or a closing prompt (Pre, before the next
                            // 133;B) clears it — at_prompt stays false
                            // through the render gap, never a false clean.
                            // on_block_event still owns blocks.
                            match ev.verb {
                                super::blocks::HookVerb::PromptEnd => {
                                    let col = ingest_term
                                        .lock()
                                        .grid()
                                        .cursor
                                        .point
                                        .column
                                        .0;
                                    *reader_prompt.lock() = Some(col);
                                    if first_prompt_pending {
                                        first_prompt_pending = false;
                                        log::info!(
                                            "[perf] first_prompt id={id} boot_ms={}",
                                            super::perf::boot_ms()
                                        );
                                    }
                                }
                                super::blocks::HookVerb::Exec { .. }
                                | super::blocks::HookVerb::Pre { .. } => {
                                    *reader_prompt.lock() = None;
                                }
                                // A beacon fires mid-TUI, never at a prompt —
                                // the latch is untouched (like Init).
                                super::blocks::HookVerb::Init { .. }
                                | super::blocks::HookVerb::Beacon { .. } => {}
                            }
                            ingest_core.on_block_event(
                                id,
                                chunk_off + ev.offset_in_chunk as u64,
                                ev,
                            );
                        }
                    }
                    // Freshness stamp for the controller's idle_ms and the
                    // Shutdown drain — stamped AFTER ingest, and the in-flight
                    // counter released only now, so "quiet" proves the bytes
                    // are journaled, not merely read (L-7).
                    ingest_last_output.store(super::now_ms(), Ordering::Relaxed);
                    ingest_in_flight.fetch_sub(batch.len() as u64, Ordering::Relaxed);
                    super::perf::time(&super::perf::RESPOND_NS, || {
                        respond_to_queries(&event_rx, &ingest_term, &ingest_writer);
                    });
                }
            }));
            // A panic here would otherwise silently freeze the terminal; surface
            // it as an exit so the GUI shows Dead and restore is possible.
            if let Err(e) = result {
                log::error!("pty-ingest-{id} panicked: {:?}", panic_payload(&e));
                panic_core.on_exit(id, gen, None);
            }
        })?;

    // Exit watcher: mark dead, notify clients.
    let exit_core = core;
    std::thread::Builder::new()
        .name(format!("pty-wait-{id}"))
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let code = child.wait().ok().map(|s| s.exit_code());
                let fast = spawned_at.elapsed().as_secs() < 3;
                exit_core.note_fast_exit(id, fast);
                exit_core.on_exit(id, gen, code);
            }));
            if let Err(e) = result {
                log::error!("pty-wait-{id} panicked: {:?}", panic_payload(&e));
                exit_core.on_exit(id, gen, None);
            }
        })?;

    log::info!(
        "spawned terminal {id}: {resolved} {full_args:?} in {:?}",
        meta.cwd
    );
    Ok(Session {
        writer,
        master: Arc::new(Mutex::new(pair.master)),
        killer,
        gen,
        term,
        preface: Arc::new(Mutex::new(preface)),
        root_pid,
        osc_cwd,
        win32_input,
        prompt,
        last_output,
        in_flight,
        mute_fanout,
    })
}

fn respond_to_queries(
    events: &mpsc::Receiver<Event>,
    term: &Arc<FairMutex<Term<EventProxy>>>,
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
) {
    let mut reply = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            Event::PtyWrite(s) => reply.extend_from_slice(s.as_bytes()),
            Event::ColorRequest(index, fmt) => {
                let (r, g, b) = palette::query_rgb(index);
                reply.extend_from_slice(fmt(Rgb { r, g, b }).as_bytes());
            }
            Event::TextAreaSizeRequest(fmt) => {
                use alacritty_terminal::grid::Dimensions;
                let (cols, lines) = {
                    let term = term.lock();
                    (term.columns() as u16, term.screen_lines() as u16)
                };
                reply.extend_from_slice(
                    fmt(WindowSize {
                        num_cols: cols,
                        num_lines: lines,
                        cell_width: NOMINAL_CELL_W,
                        cell_height: NOMINAL_CELL_H,
                    })
                    .as_bytes(),
                );
            }
            _ => {}
        }
    }
    if !reply.is_empty() {
        let mut w = writer.lock();
        let _ = w.write_all(&reply);
        let _ = w.flush();
    }
}

/// Raw-byte scanner for OSC cwd reports that vte 0.15 drops (OSC 7 and 9;9).
/// Carries a partial sequence across reads, capped so a malformed stream can't
/// grow it unbounded.
struct OscScanner {
    body: Vec<u8>,
    in_osc: bool,
    pending_esc: bool,
    /// Which world this session's reported paths live in (P6 §4).
    ns: PathNamespace,
}

impl OscScanner {
    fn new(ns: PathNamespace) -> Self {
        Self {
            body: Vec::new(),
            in_osc: false,
            pending_esc: false,
            ns,
        }
    }

    fn feed(&mut self, data: &[u8], out: &Arc<Mutex<Option<PathBuf>>>) {
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if !self.in_osc {
                if self.pending_esc {
                    self.pending_esc = false;
                    if b == b']' {
                        self.in_osc = true;
                        self.body.clear();
                        i += 1;
                        continue;
                    }
                } else if b != 0x1b {
                    // Ground state: SIMD-skip to the next ESC (same gate as
                    // BlockScanner/ModeScanner — plain text pays no DFA step).
                    match memchr::memchr(0x1b, &data[i..]) {
                        Some(off) => {
                            i += off;
                            continue;
                        }
                        None => break,
                    }
                }
                if b == 0x1b {
                    // ESC ] opens an OSC; handle ESC at a chunk boundary too.
                    if i + 1 < data.len() {
                        if data[i + 1] == b']' {
                            self.in_osc = true;
                            self.body.clear();
                            i += 2;
                            continue;
                        }
                    } else {
                        self.pending_esc = true;
                    }
                }
                i += 1;
            } else {
                match b {
                    0x07 => {
                        self.finish(out);
                        i += 1;
                    }
                    0x1b => {
                        // ST is ESC '\'; anything else aborts the sequence.
                        if i + 1 < data.len() && data[i + 1] == b'\\' {
                            self.finish(out);
                            i += 2;
                        } else {
                            self.in_osc = false;
                            self.body.clear();
                            i += 1;
                        }
                    }
                    _ => {
                        if self.body.len() < 4096 {
                            self.body.push(b);
                        } else {
                            self.in_osc = false;
                            self.body.clear();
                        }
                        i += 1;
                    }
                }
            }
        }
    }

    fn finish(&mut self, out: &Arc<Mutex<Option<PathBuf>>>) {
        if let Some(path) = parse_osc_cwd(&self.body, self.ns) {
            *out.lock() = Some(path);
        }
        self.in_osc = false;
        self.body.clear();
    }
}

/// Parse an OSC body (bytes after `ESC ]`) into a cwd: `9;9;<path>` or
/// `7;file://<host>/<url-encoded path>` (the latter Win-namespace only — the
/// bash bootstrap reports cwd via 9;9). Namespace decides normalization: a
/// POSIX path must NEVER route through `normalize_win_path` (it would strip
/// "/" to nothing).
fn parse_osc_cwd(body: &[u8], ns: PathNamespace) -> Option<PathBuf> {
    let s = String::from_utf8_lossy(body);
    if let Some(rest) = s.strip_prefix("9;9;") {
        return match ns {
            PathNamespace::Win => normalize_win_path(rest),
            PathNamespace::Posix => normalize_posix_path(rest),
        };
    }
    if ns == PathNamespace::Win {
        if let Some(rest) = s.strip_prefix("7;") {
            let after_scheme = rest.strip_prefix("file://").unwrap_or(rest);
            // Skip the host component (up to the first '/').
            let path_part = match after_scheme.find('/') {
                Some(idx) => &after_scheme[idx + 1..],
                None => after_scheme,
            };
            return normalize_win_path(&url_decode(path_part));
        }
    }
    None
}

/// POSIX namespace normalization (P6 §4): trim trailing '/' EXCEPT preserve
/// bare "/", reject empty/relative, keep verbatim otherwise. The PathBuf is
/// an opaque byte container here — displayed, compared, and fed back to
/// `wsl --cd` losslessly; nothing may call `.is_dir()` on it.
pub(crate) fn normalize_posix_path(p: &str) -> Option<PathBuf> {
    let t = p.trim();
    if !t.starts_with('/') {
        return None;
    }
    let trimmed = t.trim_end_matches('/');
    let s = if trimmed.is_empty() { "/" } else { trimmed };
    Some(PathBuf::from(s))
}

/// Trim trailing separators WITHOUT producing a bare drive letter: "C:" is
/// drive-RELATIVE on Windows (it resolves to that drive's current directory —
/// i.e. wherever the daemon happens to sit), so a drive root must keep its
/// backslash. This bit as a real bug: a shell at C:\ reported OSC cwd "C:\",
/// the trim stored "C:", and the restore spawned in the daemon's own cwd.
/// pub(crate): the PEB cwd reader (procinfo::read_process_cwd) must route
/// through the SAME normalization or it reintroduces the exact trap for
/// non-hooked shells that never emit OSC 9;9.
pub(crate) fn normalize_win_path(p: &str) -> Option<PathBuf> {
    let t = p.trim().trim_end_matches(['\\', '/']);
    if t.is_empty() {
        return None;
    }
    let s = if t.len() == 2 && t.as_bytes()[1] == b':' {
        format!("{t}\\")
    } else {
        t.to_string()
    };
    Some(PathBuf::from(s))
}

/// Minimal percent-decoding for OSC 7 file URLs.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}


/// Best-effort extraction of a panic message for logging.
pub(crate) fn panic_payload(e: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod path_tests {
    use super::normalize_win_path;
    use std::path::Path;

    /// The env-lineage scrub set (Bug 1 root cause): session markers go,
    /// user-level configuration stays. A daemon (re)started from inside a
    /// Claude Code session must not make every spawned claude think it is a
    /// child session (child sessions skip transcript persistence ⇒ nothing
    /// to --resume, ever).
    #[test]
    fn claude_session_env_scrub_set() {
        use super::is_claude_session_var;
        for name in [
            "CLAUDECODE",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_SSE_PORT",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_EFFORT",
            "claude_code_child_session", // env names are case-insensitive on Windows
        ] {
            assert!(is_claude_session_var(name), "{name} must be scrubbed");
        }
        for name in [
            "ANTHROPIC_API_KEY",
            "CLAUDE_CONFIG_DIR",
            "AI_AGENT",
            "PATH",
            "TC_SESSION_ID",
        ] {
            assert!(!is_claude_session_var(name), "{name} must be kept");
        }
    }

    /// U4 (ssh half): the EXACT spawned argv — user args first (ssh
    /// first-occurrence-wins keeps user overrides winning), `-t` only with a
    /// remote command, keepalives always, destination then remote command
    /// last.
    #[test]
    fn ssh_argv_golden() {
        use super::synth_ssh_args;
        let s = |v: &[&str]| -> Vec<String> { v.iter().map(|x| x.to_string()).collect() };
        assert_eq!(
            synth_ssh_args(&s(&["alice@devbox"]), Some("sh -c 'X'")),
            s(&[
                "-t",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=4",
                "alice@devbox",
                "sh -c 'X'",
            ])
        );
        // User flags (their own keepalive included) stay FIRST; ours append.
        assert_eq!(
            synth_ssh_args(
                &s(&["-p", "2222", "-o", "ServerAliveInterval=60", "devbox"]),
                None
            ),
            s(&[
                "-p",
                "2222",
                "-o",
                "ServerAliveInterval=60",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=4",
                "devbox",
            ])
        );
        assert_eq!(synth_ssh_args(&[], Some("x")), Vec::<String>::new());
    }

    /// The EXACT synthesized wsl.exe argv tail (P6a §3.1.1 + the 2026-07-04
    /// `--cd ""` regression + the v0.1.1 Linux-home default): fresh create
    /// with a Windows cwd, restore with a tracked POSIX cwd, `~` and
    /// empty/whitespace cwds ⇒ `--cd ~` (the old no-flag path landed in
    /// /mnt/c/%USERPROFILE% via the parent-CWD/portable-pty default — there
    /// was NO path to the Linux home; `--cd ""` remains Wsl/E_INVALIDARG so
    /// the raw empty string must never ride), and the hookless degrade.
    #[test]
    fn wsl_argv_golden() {
        use super::synth_wsl_args;
        let s = |v: &[&str]| -> Vec<String> { v.iter().map(|x| x.to_string()).collect() };
        let rc = "/mnt/c/Users/alice/AppData/Local/Pulse/bootstrap/x.bashrc";
        let guard = format!(
            "if [ -r '{rc}' ]; then exec bash --rcfile '{rc}' -i; else exec bash -i; fi"
        );
        // Fresh create with an explicit dir: the Windows dir rides --cd
        // verbatim ("open this Windows project in WSL" is a feature).
        assert_eq!(
            synth_wsl_args(Path::new("C:\\Users\\alice"), Some(rc)),
            s(&["--cd", "C:\\Users\\alice", "--exec", "/bin/sh", "-c", &guard])
        );
        // Restore: the tracked POSIX live_cwd rides --cd verbatim.
        assert_eq!(
            synth_wsl_args(Path::new("/tmp"), Some(rc)),
            s(&["--cd", "/tmp", "--exec", "/bin/sh", "-c", &guard])
        );
        // The launcher's default WSL rows: `~` rides verbatim — wsl.exe
        // resolves it to the distro default user's Linux home.
        assert_eq!(
            synth_wsl_args(Path::new("~"), Some(rc)),
            s(&["--cd", "~", "--exec", "/bin/sh", "-c", &guard])
        );
        // Empty (and whitespace) cwd: --cd ~ (defense in depth for raw-API
        // creates; never the raw empty string, never flag-less).
        assert_eq!(
            synth_wsl_args(Path::new(""), Some(rc)),
            s(&["--cd", "~", "--exec", "/bin/sh", "-c", &guard])
        );
        assert_eq!(
            synth_wsl_args(Path::new("  "), Some(rc)),
            s(&["--cd", "~", "--exec", "/bin/sh", "-c", &guard])
        );
        // Untranslatable/quoted/missing rcfile: degraded-hookless bash,
        // never a broken spawn.
        assert_eq!(
            synth_wsl_args(Path::new("C:\\Users\\alice"), None),
            s(&["--cd", "C:\\Users\\alice", "--exec", "/bin/sh", "-c", "exec bash -i"])
        );
        assert_eq!(
            synth_wsl_args(Path::new(""), None),
            s(&["--cd", "~", "--exec", "/bin/sh", "-c", "exec bash -i"])
        );
    }

    /// Restore-with-CLI-resume golden: the exact -NoExit/-Command composition
    /// (bootstrap dot-source, Set-Location with single-quote doubling, then
    /// the trailing resume command in the SAME interactive session).
    #[test]
    fn powershell_restore_golden() {
        use super::powershell_restore_command;
        let args = powershell_restore_command(
            Some(Path::new("C:\\tc\\bootstrap\\a.ps1")),
            Some(Path::new("C:\\Users\\o'brien\\proj")),
            Some("claude --resume 1234"),
        );
        assert_eq!(
            args,
            vec![
                "-NoExit".to_string(),
                "-Command".into(),
                ". 'C:\\tc\\bootstrap\\a.ps1'; Set-Location -LiteralPath 'C:\\Users\\o''brien\\proj'; claude --resume 1234"
                    .into(),
            ]
        );
        // No cwd / no trailing: just the dot-source, still -NoExit.
        let args = powershell_restore_command(Some(Path::new("C:\\b.ps1")), None, None);
        assert_eq!(
            args,
            vec!["-NoExit".to_string(), "-Command".into(), ". 'C:\\b.ps1'".into()]
        );
    }

    /// The drive-root trap, now guarded for BOTH producers (OSC 9;9 and the
    /// PEB reader): a root must keep its backslash, never become "C:".
    #[test]
    fn drive_root_keeps_backslash() {
        assert_eq!(normalize_win_path("C:\\").as_deref(), Some(Path::new("C:\\")));
        assert_eq!(normalize_win_path("d:/").as_deref(), Some(Path::new("d:\\")));
        // PEB strings arrive NUL-padded; callers strip NULs first — a bare
        // "C:" after that still gets its root backslash restored.
        assert_eq!(normalize_win_path("C:").as_deref(), Some(Path::new("C:\\")));
    }

    #[test]
    fn normal_paths_trim_trailing_separators_only() {
        assert_eq!(
            normalize_win_path("C:\\Users\\alice\\").as_deref(),
            Some(Path::new("C:\\Users\\alice"))
        );
        assert_eq!(normalize_win_path("  ").as_deref(), None);
        assert_eq!(normalize_win_path("").as_deref(), None);
    }

    /// U3: the POSIX branch — "/" survives, trailing slashes trim, empty and
    /// relative reject; and the namespace routing keeps the Win regression
    /// suite untouched (drive-root trap can't fire on a posix session).
    #[test]
    fn posix_namespace_normalization() {
        use super::{normalize_posix_path, parse_osc_cwd};
        use crate::state::PathNamespace;
        assert_eq!(normalize_posix_path("/").as_deref(), Some(Path::new("/")));
        assert_eq!(
            normalize_posix_path("/home/z/").as_deref(),
            Some(Path::new("/home/z"))
        );
        assert_eq!(
            normalize_posix_path("/mnt/c/A B").as_deref(),
            Some(Path::new("/mnt/c/A B"))
        );
        assert_eq!(normalize_posix_path(""), None);
        assert_eq!(normalize_posix_path("  "), None);
        assert_eq!(normalize_posix_path("relative/x"), None);
        // 9;9 routes by namespace; posix ignores OSC 7 (bash reports via 9;9).
        assert_eq!(
            parse_osc_cwd(b"9;9;/tmp/", PathNamespace::Posix).as_deref(),
            Some(Path::new("/tmp"))
        );
        assert_eq!(
            parse_osc_cwd(b"9;9;C:\\", PathNamespace::Win).as_deref(),
            Some(Path::new("C:\\"))
        );
        assert_eq!(parse_osc_cwd(b"7;file://host/x", PathNamespace::Posix), None);
    }
}
