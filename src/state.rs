//! Persistent data model shared by daemon and GUI.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Root data directory: %LOCALAPPDATA%\Pulse.
///
/// `TC_DATA_DIR` overrides it wholesale (daemon.json, lock, journals,
/// bootstrap, state, logs), giving a fully isolated daemon+probe universe on
/// its own port/token. This exists for measurement: probes against the
/// installed daemon are contaminated — the user's GUI auto-attaches to every
/// terminal in the snapshot (probe terminals included), so a flood pays the
/// per-attached-client fanout cost and competes with live sessions. Set the
/// same value for the daemon process and every probe run that should talk to
/// it; never combine with `--install` (it would install into the override).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TC_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("Pulse")
}

/// True when `TC_DATA_DIR` redirects the data dir (isolated probe/dev
/// daemon). Gates side effects that must never leak out of the sandbox —
/// most importantly the autostart Run key.
pub fn data_dir_overridden() -> bool {
    std::env::var_os("TC_DATA_DIR").is_some_and(|d| !d.is_empty())
}

pub fn journals_dir() -> PathBuf {
    data_dir().join("journals")
}

/// Remote CLI-resume probe sidecars (`probes\<terminal-id>.json`): the
/// snapshot-leg store listing persisted at bare-CLI block open, consumed by
/// the correlate leg at the next restore-class launch (remote-cli-resume-spec
/// D3). Beside the journals helper; NOTHING persisted-schema changes.
pub fn data_probes_dir() -> PathBuf {
    data_dir().join("probes")
}

pub fn state_path() -> PathBuf {
    data_dir().join("state.json")
}

pub fn daemon_info_path() -> PathBuf {
    data_dir().join("daemon.json")
}

pub fn daemon_log_path() -> PathBuf {
    data_dir().join("daemon.log")
}

pub fn gui_log_path() -> PathBuf {
    data_dir().join("gui.log")
}

/// Cap a log at process start (R3-1): past the cap it is renamed to
/// `<name>.log.old` (rename-replace — exactly one prior generation kept) and
/// the caller opens a fresh file. Called BEFORE the logger is initialized, so
/// in-run byte offsets (probes tail the daemon log by offset) never move.
pub fn rotate_log_at_startup(path: &Path) {
    const LOG_ROTATE_CAP: u64 = 4 * 1024 * 1024;
    let Ok(meta) = std::fs::metadata(path) else {
        return; // no log yet
    };
    if meta.len() <= LOG_ROTATE_CAP {
        return;
    }
    let old = path.with_extension("log.old");
    let _ = std::fs::rename(path, old);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TermStatus {
    Running,
    /// Process exited (or daemon restarted). Journal retained; restorable.
    Dead,
}

/// What kind of program lives in the terminal — this decides the resume
/// strategy after an exit, app restart, or reboot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TermKind {
    /// Claude Code with a pinned session id: spawned as
    /// `claude --session-id <id>` the first time, `claude --resume <id>`
    /// forever after. Deterministic — never guesses "most recent".
    Claude {
        session_id: Uuid,
        /// Extra CLI args appended at every launch (e.g. --model, --effort).
        extra_args: Vec<String>,
    },
    /// Plain shell. Restore = relaunch in the saved cwd; the journal keeps
    /// the old scrollback visible above a restore marker.
    Shell,
    /// Arbitrary command line. Restore = run it again in the saved cwd.
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub order: i64,
    /// Curated sidebar color tag: index into the GUI's swatch table; None =
    /// untagged (rows render exactly as before the field existed). APPENDED
    /// LAST (task #22): Folder rides the bincode Snapshot, so field order is
    /// wire order — same-exe GUI+daemon rule, same skew class as
    /// `TerminalMeta.shell_cfg` (the install copy-race); proto 7→8 rides the
    /// C2D::SetColorTag append anyway.
    #[serde(default)]
    pub color_tag: Option<u8>,
}

/// Which shell actually runs in a terminal — DERIVED from the meta the user
/// already provides (kind + program + args), never persisted (P6 D1: the
/// program+args ARE the identity, and deriving can never disagree with what
/// actually spawns). Decides hook delivery, cwd namespace, and restore
/// synthesis per family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellFamily {
    /// powershell.exe / pwsh.exe, TermKind::Shell — the fully-hooked path.
    Pwsh,
    /// wsl.exe with an explicit inner shell we control (bash in P6a).
    /// `distro`: value after -d/--distribution; None = the default distro.
    WslShell { distro: Option<String> },
    /// cmd.exe as the terminal's shell (P6b — classified now, hooked later).
    Cmd,
    /// ssh.exe (P6c — classified now, hooked later).
    Ssh { host: String },
    /// Everything else (claude-kind, Custom commands, unknown shells) —
    /// exactly the pre-P6 unhooked behavior.
    Other,
}

/// Which world a session's paths live in. WSL/ssh shells see POSIX paths;
/// their cwds are stored VERBATIM (P6 D5) and must never round-trip through
/// Windows path normalization or `.is_dir()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathNamespace {
    Win,
    Posix,
}

pub fn path_namespace(family: &ShellFamily) -> PathNamespace {
    match family {
        ShellFamily::WslShell { .. } | ShellFamily::Ssh { .. } => PathNamespace::Posix,
        _ => PathNamespace::Win,
    }
}

/// Drive-letter translation of a Windows path into WSL's automount namespace:
/// `C:\Users\z\x.bashrc` → `/mnt/c/Users/z/x.bashrc` (lowercase drive,
/// backslashes→slashes). None for UNC/relative paths (untranslatable — the
/// caller degrades: hookless spawn for the bootstrap, `~` for display).
/// Lives here (not daemon::bootstrap) since v0.1.1: `display_cwd` needs it
/// and state.rs also compiles into the pulse-ctl bin, which has no daemon
/// module. Re-exported by daemon::bootstrap for its historical call sites.
pub fn wsl_mnt_path(p: &Path) -> Option<String> {
    let s = p.to_str()?;
    let b = s.as_bytes();
    if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || (b[2] != b'\\' && b[2] != b'/')
    {
        return None;
    }
    let drive = (b[0] as char).to_ascii_lowercase();
    let rest = s[3..].replace('\\', "/");
    Some(format!("/mnt/{drive}/{rest}"))
}

/// Classify what actually spawns. Only `TermKind::Shell` can be a first-class
/// shell family (Custom keeps full degraded freedom; Claude-kind untouched).
/// Stem-matched case-insensitively like the historical pwsh check.
pub fn shell_family(kind: &TermKind, program: &str, args: &[String]) -> ShellFamily {
    if !matches!(kind, TermKind::Shell) {
        return ShellFamily::Other;
    }
    let stem = Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match stem.as_str() {
        "powershell" | "pwsh" => ShellFamily::Pwsh,
        "wsl" => wsl_family(args),
        "cmd" => ShellFamily::Cmd,
        "ssh" => match ssh_destination(args) {
            Some(host) => ShellFamily::Ssh { host: host.to_string() },
            None => ShellFamily::Other,
        },
        _ => ShellFamily::Other,
    }
}

/// WSL classification: we only hook argv shapes WE synthesize (a bare spawn
/// or `-d <distro>`); hand-built exotic wsl args (`--system`, `-e`, a command
/// tail, …) classify Other — never guess-hook a world we didn't set up. The
/// synthesized `--cd/--exec` tail is added per-spawn and never persisted in
/// meta.args, so it never reaches this classifier.
fn wsl_family(args: &[String]) -> ShellFamily {
    match args {
        [] => ShellFamily::WslShell { distro: None },
        [flag, distro] if flag == "-d" || flag == "--distribution" => ShellFamily::WslShell {
            distro: Some(distro.clone()),
        },
        _ => ShellFamily::Other,
    }
}

/// The ssh destination (host) an argv addresses — the first non-flag arg,
/// skipping OpenSSH's value-taking flags — and it must be the LAST arg: any
/// token after the destination is a REMOTE COMMAND in ssh's grammar (the
/// session runs it and exits — not an interactive shell), so such shapes
/// classify Other and are never hooked (the wsl exotic-argv doctrine).
/// Shared by the classifier and the launcher's freeform-host validation.
pub fn ssh_destination(args: &[String]) -> Option<&str> {
    // OpenSSH value-taking flags (`man ssh` synopsis, 9.x):
    // -B -b -c -D -E -e -F -I -i -J -L -l -m -O -o -p -Q -R -S -W -w.
    const VALUE_FLAGS: &[&str] = &[
        "-B", "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O",
        "-o", "-p", "-Q", "-R", "-S", "-W", "-w",
    ];
    let mut it = args.iter().enumerate();
    while let Some((idx, a)) = it.next() {
        if VALUE_FLAGS.contains(&a.as_str()) {
            let _ = it.next(); // consume the flag's value
            continue;
        }
        if a.starts_with('-') {
            continue; // boolean flag (or -p2222 glued form: value rides along)
        }
        // Destination found; refuse when anything trails it (remote command).
        return (idx == args.len() - 1).then_some(a.as_str());
    }
    None
}

/// Per-family user choices the classifier can't derive (P6 §2). Appended to
/// TerminalMeta/NewTerminal with serde-default; fields are append-only-with-
/// serde-default forever. Dormant in P6a (bash is the only WSL shell and ssh
/// hasn't landed); carried now so the create path doesn't change shape later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellCfg {
    /// WSL inner shell: None/"bash" (default) | "zsh" | "fish" (P6a.2).
    #[serde(default)]
    pub shell: Option<String>,
    /// Ssh: inject the one-shot remote bootstrap (default true; P6c).
    #[serde(default = "default_true")]
    pub remote_hooks: bool,
    /// Ssh: automatically reconnect after an UNEXPECTED link death (default
    /// true — the opt-out). Only ever acts when the dying connection had
    /// successfully hooked (bootstrap ran ⇒ auth completed without
    /// interaction) and the exit was not user-initiated or a clean remote
    /// `exit`; see Core::maybe_schedule_reconnect. Appended with
    /// serde-default (fields are append-only-with-serde-default forever).
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    /// WSL (v0.1.2): print the distro welcome message (motd) on this
    /// terminal's FIRST spawn — the pam_motd-emulation prelude that never
    /// touches the distro's once-a-day `~/.motd_shown` stamp. Stamped at
    /// create time by the GUI from the "Show welcome message on new
    /// terminals" pref (default ON there); serde-default FALSE so raw-API /
    /// ctl / probe creates and pre-v0.1.2 rows stay banner-free (automation
    /// streams must not grow motd bytes uninvited). Restores ignore it.
    #[serde(default)]
    pub wsl_motd: bool,
}

impl Default for ShellCfg {
    fn default() -> Self {
        Self {
            shell: None,
            remote_hooks: true,
            auto_reconnect: true,
            wsl_motd: false,
        }
    }
}

/// How confident the tracker is in a detected inner CLI's resume identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CliConfidence {
    /// The resume id was read straight from the process's argv — or from the
    /// CLI's own self-report (claude's pid registry / an injected
    /// SessionStart hook / the remote tcbeacon), which is the same trust
    /// class: the tool itself named the session.
    Explicit,
    /// Inferred by correlating process/journal/session timestamps.
    Correlated,
    /// Multiple plausible sessions — never guessed; the user is offered a choice.
    Ambiguous,
}

/// A CLI a user launched by hand inside a plain shell (e.g. `claude`), tracked
/// so a restart can resume it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InnerCli {
    /// Adapter key, e.g. "claude".
    pub adapter: String,
    /// The resume token (session id) if known.
    pub resume_token: Option<String>,
    pub confidence: CliConfidence,
    /// The cwd the CLI was running in.
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalMeta {
    pub id: Uuid,
    pub name: String,
    pub folder: Option<Uuid>,
    pub kind: TermKind,
    /// Program to launch (e.g. "claude", "powershell.exe").
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub order: i64,
    /// Relaunch automatically when the daemon starts (i.e. after login/reboot).
    #[serde(default = "default_true")]
    pub auto_restore: bool,
    /// The process has been spawned at least once (drives --session-id vs --resume).
    #[serde(default)]
    pub launched_once: bool,
    #[serde(default = "default_dead")]
    pub status: TermStatus,
    /// Last grid size a client showed this terminal at (0 = unknown).
    /// Respawns and boot restores use it so wrapping matches the window.
    #[serde(default)]
    pub last_cols: u16,
    #[serde(default)]
    pub last_rows: u16,
    /// Live cwd of the terminal's shell (Shell/Custom), tracked so a restart
    /// resumes in the right directory even after the user cd'd.
    #[serde(default)]
    pub live_cwd: Option<PathBuf>,
    /// A hand-run CLI detected inside a plain shell, to be resumed on restart.
    #[serde(default)]
    pub inner_cli: Option<InnerCli>,
    /// This terminal's spawns dot-source the block-hook bootstrap (written by
    /// launch()): the GUI reserves the composer strip from the FIRST attach
    /// instead of waiting for the Blocks/epoch round-trip — the 49↔52 boot
    /// resize flip every hooked terminal used to pay. serde-default for old
    /// state.json files. APPENDED LAST deliberately: TerminalMeta rides the
    /// bincode Snapshot, so field order is wire order (proto 4→5 bump).
    #[serde(default)]
    pub hooked: bool,
    /// Per-family shell options (P6 §10.2). APPENDED after `hooked` — bincode
    /// Snapshot field order is wire order; GUI+daemon are the same exe (always
    /// version-matched) and tc.exe never decodes Snapshot payloads (CtlTerm is
    /// deliberately decoupled), so no proto bump is needed for this append —
    /// the skew window is the install copy-race that already exists.
    #[serde(default)]
    pub shell_cfg: Option<ShellCfg>,
    /// Curated sidebar color tag (task #22): index into the GUI's swatch
    /// table (0-7 today; out-of-range values render as untagged so a future
    /// table growth stays backward-safe); None = untagged. APPENDED after
    /// `shell_cfg` — bincode Snapshot field order is wire order (same-exe
    /// GUI+daemon rule; tc.exe never decodes Snapshot). Set via
    /// C2D::SetColorTag (proto 8).
    #[serde(default)]
    pub color_tag: Option<u8>,
    /// SLEEP (proto 9, S1): the user put this terminal to sleep — its process
    /// tree was torn down, everything else (journal, blocks sidecar, meta,
    /// pinned CLI identity) stays persisted exactly as a daemon shutdown
    /// leaves it. Survives reboots: boot auto-restore SKIPS asleep terminals
    /// until an explicit wake (`asleep` is the stronger "not until I say so"
    /// intent and wins over `auto_restore` while set). Cleared by launch()
    /// in the same mutate that sets Running, so (Running, true) exists only
    /// as the sub-second "Sleeping" drain transient. NOT a TermStatus
    /// variant: `SharedState::load()` force-resets status to Dead at boot,
    /// which would erase a persisted Asleep — the flag survives load
    /// untouched and keeps on_exit unchanged. APPENDED after `color_tag`
    /// (bincode Snapshot field order is wire order; same-exe rule).
    #[serde(default)]
    pub asleep: bool,
    /// SSH auto-reconnect supervision is ACTIVE for this terminal (proto 10):
    /// an unexpected link death qualified and the daemon is retrying with
    /// bounded backoff. Presentation-only state that happens to ride the
    /// Snapshot (the GUI lane shows "reconnecting…" + Cancel); runtime truth
    /// lives in Core::reconnects. Force-reset by `SharedState::load()` like
    /// `status` — a persisted value from a power loss is meaningless.
    /// APPENDED after `asleep` (bincode Snapshot field order is wire order;
    /// same-exe rule; proto 9 → 10).
    #[serde(default)]
    pub reconnecting: bool,
}

/// Derived presentation of (status, asleep) — NEVER persisted, NEVER on the
/// wire as an enum (S1). "Sleeping" exists only inside the sleep drain
/// window (flag saved → exit lands, ≤2s cap); a power loss inside it reloads
/// as (Dead, true) = Asleep, the intended outcome. Wake can never produce
/// (Running, true): launch() clears the flag in the same mutate that sets
/// Running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentedStatus {
    Running,
    /// Transient: flagged asleep, process still exiting (drain window).
    Sleeping,
    /// Flagged asleep and the process is gone — the resting sleep state.
    Asleep,
    /// Died on its own (not shelved) — keeps the hollow-ring semantics.
    Dead,
}

pub fn presented_status(status: TermStatus, asleep: bool) -> PresentedStatus {
    match (status, asleep) {
        (TermStatus::Running, false) => PresentedStatus::Running,
        (TermStatus::Running, true) => PresentedStatus::Sleeping,
        (TermStatus::Dead, true) => PresentedStatus::Asleep,
        (TermStatus::Dead, false) => PresentedStatus::Dead,
    }
}

fn default_true() -> bool {
    true
}
fn default_dead() -> TermStatus {
    TermStatus::Dead
}

impl TerminalMeta {
    /// The ONE effective-cwd display rule (v0.1.1, the "titlebar shows
    /// `C:\Users\zany` for a Linux session" fix) — every cwd-displaying
    /// surface (titlebar, dashboard card, `pulse-ctl list`, composer lane
    /// fallback) goes through here:
    ///   - Win namespace: tracked `live_cwd`, else the launch cwd (exactly
    ///     the rule those surfaces already applied);
    ///   - Posix namespace (WSL/ssh): `live_cwd` verbatim when the hooks
    ///     have reported one; else an EXPLICIT Windows start dir shows as
    ///     its automount translation (`/mnt/c/…` — true by wsl.exe `--cd`
    ///     semantics even for a hookless spawn); else `~` (exactly where
    ///     `--cd ~` starts the session). NEVER a `C:\` string for a
    ///     Posix-namespace session.
    pub fn display_cwd(&self) -> String {
        let family = shell_family(&self.kind, &self.program, &self.args);
        if let Some(live) = &self.live_cwd {
            return live.to_string_lossy().into_owned();
        }
        match path_namespace(&family) {
            PathNamespace::Win => self.cwd.to_string_lossy().into_owned(),
            PathNamespace::Posix => {
                let s = self.cwd.to_string_lossy();
                let t = s.trim();
                if t.is_empty() || t == "~" {
                    return "~".into();
                }
                if t.starts_with('/') {
                    return t.to_string(); // POSIX restore cwd, verbatim
                }
                // Explicit Windows start dir: only WSL can honor it (wsl.exe
                // resolves --cd through the automount) — for ssh a Windows
                // path has no remote meaning, so the honest placeholder is
                // the remote home.
                match family {
                    ShellFamily::WslShell { .. } => {
                        wsl_mnt_path(Path::new(t)).unwrap_or_else(|| "~".into())
                    }
                    _ => "~".into(),
                }
            }
        }
    }

    /// The command line to use for the next launch, applying the resume adapter.
    pub fn launch_command(&self) -> (String, Vec<String>) {
        match &self.kind {
            TermKind::Claude {
                session_id,
                extra_args,
            } => {
                let mut args = if self.launched_once && claude_session_file_exists(&self.cwd, session_id) {
                    vec!["--resume".to_string(), session_id.to_string()]
                } else {
                    vec!["--session-id".to_string(), session_id.to_string()]
                };
                args.extend(extra_args.iter().cloned());
                // Attribution Layer 2: inject SessionStart/SessionEnd hooks
                // via a `--settings` JSON STRING (argv-only — no files
                // written, zero approval prompts; hooks from --settings are
                // ADDITIVE over the user's own settings). The hook command
                // is `pulse-ctl.exe __claude-hook <event>`, which posts the
                // live session id to the daemon within ~200ms of an in-TUI
                // /clear or /resume switch. Skipped when the user passed
                // their own --settings (never clobber intent) or when no
                // sibling pulse-ctl.exe exists (single-bin dev build —
                // Layer 1's registry read still covers these terminals).
                if !extra_args.iter().any(|a| a.starts_with("--settings")) {
                    if let Some(json) = claude_hook_settings_json() {
                        args.push("--settings".to_string());
                        args.push(json);
                    }
                }
                (self.program.clone(), args)
            }
            TermKind::Shell | TermKind::Custom => (self.program.clone(), self.args.clone()),
        }
    }
}

/// The `--settings` JSON for Layer-2 hook injection, naming the sibling
/// pulse-ctl.exe of the CURRENT exe (the daemon; hooks must survive
/// install-dir moves, so the path is resolved per launch, not persisted).
/// None when no pulse-ctl.exe exists or its path can't be expressed safely.
fn claude_hook_settings_json() -> Option<String> {
    let ctl = std::env::current_exe().ok()?.parent()?.join("pulse-ctl.exe");
    if !ctl.is_file() {
        return None;
    }
    claude_hook_settings_json_for(&ctl.to_string_lossy())
}

/// Testable core: the settings JSON for a given pulse-ctl.exe path. Windows hooks
/// run under git-bash, which EATS backslashes in command strings — forward
/// slashes are mandatory — and the path is single-quoted for bash (spaces:
/// "C:/Program Files/…"). A path containing a single quote can't be quoted
/// safely ⇒ None (degrade to hookless; Layer 1 still covers).
pub fn claude_hook_settings_json_for(tc_path: &str) -> Option<String> {
    let fwd = tc_path.replace('\\', "/");
    if fwd.contains('\'') {
        return None;
    }
    let hook = |event: &str| {
        serde_json::json!([{
            "hooks": [{
                "type": "command",
                "command": format!("'{fwd}' __claude-hook {event}"),
            }]
        }])
    };
    let v = serde_json::json!({
        "hooks": {
            "SessionStart": hook("SessionStart"),
            "SessionEnd": hook("SessionEnd"),
        }
    });
    serde_json::to_string(&v).ok()
}

/// Claude Code munges a project cwd into a directory name under
/// ~/.claude/projects by replacing every non-alphanumeric char with '-'
/// (e.g. "C:\Terminal Control" -> "C--Terminal-Control").
pub fn claude_project_dir_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn claude_session_file(cwd: &Path, session_id: &Uuid) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".claude")
            .join("projects")
            .join(claude_project_dir_name(cwd))
            .join(format!("{session_id}.jsonl")),
    )
}

pub fn claude_session_file_exists(cwd: &Path, session_id: &Uuid) -> bool {
    claude_session_file(cwd, session_id).is_some_and(|p| p.exists())
}

/// One shell command's journal-anchored record (a "block"): announced by the
/// injected PowerShell bootstrap via private OSC hooks, scanned out of the
/// output stream by the daemon, and keyed to ABSOLUTE journal offsets so it
/// survives compaction, restarts, and reboots. Shared daemon/GUI type, but
/// deliberately NOT part of `SharedState` (which is re-broadcast and fsynced
/// on every mutation) — blocks persist in their own per-terminal sidecar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockRec {
    /// Spawn generation the block belongs to (bumped every launch()).
    pub epoch: u32,
    /// The shell's prompt counter at close time (0 while open).
    pub n: u32,
    /// The command line as typed (shell-truncated to 2000 chars).
    pub cmd: String,
    /// cwd the command started in (from the preceding prompt hook).
    pub cwd: Option<PathBuf>,
    /// Exit code reported by the closing prompt hook; None = never closed
    /// cleanly (session died / next command started first). Best-effort for
    /// cmdlet-only pipelines (PowerShell only sets $LASTEXITCODE for native
    /// commands; $? is folded to 0/1 otherwise).
    pub exit: Option<i64>,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
    /// Absolute journal offset where the block's output begins (just after
    /// the exec hook's terminator).
    pub start_off: u64,
    /// Absolute journal offset just after the closing prompt hook.
    pub end_off: Option<u64>,
    /// Compaction cut into this block's output (start_off < base < end_off).
    #[serde(default)]
    pub truncated: bool,
}

/// Spec for creating a terminal, sent by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTerminal {
    pub name: String,
    pub folder: Option<Uuid>,
    pub kind: TermKind,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// When importing an existing Claude session, it already ran elsewhere,
    /// so the first launch must use --resume, not --session-id.
    pub already_launched: bool,
    /// Per-family shell options (P6 §10.2). APPENDED — NewTerminal rides
    /// bincode (C2D::CreateTerminal + CtlRequest::CreateTerminal), field
    /// order is wire order. GUI+daemon are the same exe; a stale tc.exe's
    /// shorter struct fails decode only across the install copy-race window
    /// (both exes are copied together).
    #[serde(default)]
    pub shell_cfg: Option<ShellCfg>,
}

/// The full state broadcast to clients and persisted to state.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SharedState {
    pub folders: Vec<Folder>,
    pub terminals: Vec<TerminalMeta>,
    #[serde(default)]
    pub next_order: i64,
}

impl SharedState {
    /// Load state.json. The `bool` is the health flag: `true` ONLY when the
    /// file existed AND parsed — i.e. the returned terminal list is the real
    /// one. `false` covers fresh installs (NotFound), corruption, and
    /// transient read errors (AV lock, sharing violation, EACCES). Destructive
    /// boot maintenance — the orphan artifact reap in `daemon::run()` — must
    /// run only on a healthy load: against a defaulted (empty) state it would
    /// classify EVERY journal/sidecar/probe file as orphaned and delete them.
    pub fn load() -> (Self, bool) {
        Self::load_from(&state_path())
    }

    fn load_from(path: &Path) -> (Self, bool) {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<SharedState>(&bytes) {
                Ok(mut s) => {
                    // A freshly loaded daemon has no live PTYs, and no
                    // reconnect supervision survives a restart (runtime
                    // truth lives in Core::reconnects).
                    for t in &mut s.terminals {
                        t.status = TermStatus::Dead;
                        t.reconnecting = false;
                    }
                    (s, true)
                }
                Err(e) => {
                    log::error!("state.json corrupt ({e}); starting fresh, old file backed up");
                    Self::backup_bad_state(path);
                    (SharedState::default(), false)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Genuinely absent: first run (or manual reset). Defaulted,
                // and NOT healthy — there is no proven terminal list.
                (SharedState::default(), false)
            }
            Err(e) => {
                // Exists but unreadable (sharing violation / AV lock /
                // EACCES): treat like corruption — log + best-effort backup —
                // because running on a defaulted state would otherwise SAVE
                // the empty state over the real file on the first mutation.
                // (If the rename also fails, e.g. the file is locked, the
                // original survives in place for the next boot.)
                log::error!("state.json unreadable ({e}); starting fresh, backup attempted");
                Self::backup_bad_state(path);
                (SharedState::default(), false)
            }
        }
    }

    /// Move a corrupt/unreadable state.json aside. Keep the FIRST bad copy
    /// (best forensics): a second one must not clobber it — fall back to a
    /// timestamped name.
    fn backup_bad_state(path: &Path) {
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let backup = dir.join("state.json.corrupt");
        let backup = if backup.exists() {
            dir.join(format!(
                "state.json.corrupt.{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ))
        } else {
            backup
        };
        let _ = std::fs::rename(path, backup);
    }

    /// Atomic save: write + fsync a temp file, then rename over the old one.
    /// `rename` maps to MoveFileExW(REPLACE_EXISTING) on Windows, so it swaps
    /// atomically and there is never a window with no state.json.
    pub fn save(&self) -> anyhow::Result<()> {
        use std::io::Write;
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join("state.json.tmp");
        let data = serde_json::to_vec_pretty(self)?;
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, state_path())?;
        Ok(())
    }

    /// `save()` with the failure LOGGED (C1 honesty doctrine): a silently
    /// dropped state save loses resume identities, live cwds, claude pins —
    /// the "come back at any time" promise — with no trace. Call sites that
    /// need no special handling use this instead of `let _ = save()`; the
    /// daemon's disk-full banner is owned by the journal append path.
    pub fn save_logged(&self, what: &str) {
        if let Err(e) = self.save() {
            log::error!("state.json save failed ({what}): {e}");
        }
    }

    pub fn terminal(&self, id: Uuid) -> Option<&TerminalMeta> {
        self.terminals.iter().find(|t| t.id == id)
    }

    pub fn terminal_mut(&mut self, id: Uuid) -> Option<&mut TerminalMeta> {
        self.terminals.iter_mut().find(|t| t.id == id)
    }

    pub fn alloc_order(&mut self) -> i64 {
        self.next_order += 1;
        self.next_order
    }
}

#[cfg(test)]
mod shell_family_tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// U1: the classifier table — families derive from kind+program+args,
    /// exotic wsl args refuse to classify, ssh flag skipping finds the host.
    #[test]
    fn classifier_table() {
        use ShellFamily::*;
        let sh = TermKind::Shell;
        assert_eq!(shell_family(&sh, "powershell.exe", &[]), Pwsh);
        assert_eq!(shell_family(&sh, "PWSH.EXE", &[]), Pwsh);
        assert_eq!(shell_family(&sh, "C:\\Program Files\\PowerShell\\7\\pwsh.exe", &[]), Pwsh);
        assert_eq!(shell_family(&sh, "wsl.exe", &[]), WslShell { distro: None });
        assert_eq!(
            shell_family(&sh, "wsl", &s(&["-d", "Ubuntu-24.04"])),
            WslShell { distro: Some("Ubuntu-24.04".into()) }
        );
        assert_eq!(
            shell_family(&sh, "WSL.exe", &s(&["--distribution", "Debian"])),
            WslShell { distro: Some("Debian".into()) }
        );
        // Exotic hand-built wsl argv shapes are never hooked (refuse-over-guess).
        assert_eq!(shell_family(&sh, "wsl.exe", &s(&["--system"])), Other);
        assert_eq!(shell_family(&sh, "wsl.exe", &s(&["-d", "Ubuntu", "-e", "htop"])), Other);
        assert_eq!(shell_family(&sh, "wsl.exe", &s(&["-e", "bash"])), Other);
        assert_eq!(shell_family(&sh, "cmd.exe", &[]), Cmd);
        assert_eq!(
            shell_family(&sh, "ssh.exe", &s(&["devbox"])),
            Ssh { host: "devbox".into() }
        );
        // Value-taking flags are skipped with their values; -v is boolean.
        assert_eq!(
            shell_family(&sh, "ssh", &s(&["-p", "2222", "-i", "k.pem", "-v", "alice@devbox"])),
            Ssh { host: "alice@devbox".into() }
        );
        assert_eq!(shell_family(&sh, "ssh.exe", &s(&["-p", "2222"])), Other);
        // The full OpenSSH value-flag set is skipped (capital forms too).
        assert_eq!(
            shell_family(
                &sh,
                "ssh",
                &s(&["-J", "jump@bastion", "-o", "ServerAliveInterval=60", "-E", "log", "devbox"])
            ),
            Ssh { host: "devbox".into() }
        );
        // Anything AFTER the destination is a remote command — ssh runs it
        // and exits (not an interactive shell): refuse-over-guess ⇒ Other.
        assert_eq!(
            shell_family(&sh, "ssh.exe", &s(&["devbox", "uptime"])),
            Other
        );
        assert_eq!(
            shell_family(&sh, "ssh", &s(&["devbox", "-v"])),
            Other,
            "post-destination tokens are remote-command words to ssh"
        );
        // Only TermKind::Shell classifies; Custom/Claude keep degraded freedom.
        assert_eq!(shell_family(&TermKind::Custom, "wsl.exe", &[]), Other);
        assert_eq!(shell_family(&TermKind::Custom, "cmd.exe", &[]), Other);
        assert_eq!(
            shell_family(
                &TermKind::Claude { session_id: Uuid::nil(), extra_args: vec![] },
                "claude",
                &[]
            ),
            Other
        );
        assert_eq!(shell_family(&sh, "claude", &[]), Other);
    }

    #[test]
    fn namespaces_follow_family() {
        assert_eq!(path_namespace(&ShellFamily::Pwsh), PathNamespace::Win);
        assert_eq!(path_namespace(&ShellFamily::Cmd), PathNamespace::Win);
        assert_eq!(path_namespace(&ShellFamily::Other), PathNamespace::Win);
        assert_eq!(
            path_namespace(&ShellFamily::WslShell { distro: None }),
            PathNamespace::Posix
        );
        assert_eq!(
            path_namespace(&ShellFamily::Ssh { host: "h".into() }),
            PathNamespace::Posix
        );
    }

    /// ShellCfg's serde default and Default::default agree (remote_hooks=true,
    /// auto_reconnect=true), and an old state.json without the fields loads
    /// with them on (the reconnect opt-out is append-only serde-default).
    #[test]
    fn shell_cfg_defaults() {
        let d = ShellCfg::default();
        assert!(d.remote_hooks);
        assert!(d.auto_reconnect, "reconnect defaults ON (opt-out field)");
        assert!(
            !d.wsl_motd,
            "v0.1.2: wsl_motd defaults OFF at this layer — the GUI stamps the \
             pref at create time; ctl/probe creates stay banner-free"
        );
        let parsed: ShellCfg = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, d);
        let old: ShellCfg = serde_json::from_str(r#"{"remote_hooks":false}"#).unwrap();
        assert!(!old.remote_hooks);
        assert!(old.auto_reconnect, "pre-proto-10 cfg keeps reconnect on");
        assert!(!old.wsl_motd, "pre-v0.1.2 cfg loads with the banner opt-in off");
    }

    /// v0.1.1: the ONE effective-cwd display rule — POSIX-namespace
    /// sessions (WSL/ssh) never render a `C:\` string; Win namespace keeps
    /// the live_cwd-else-cwd rule every surface already had.
    #[test]
    fn display_cwd_rule() {
        let mut meta = TerminalMeta {
            id: Uuid::new_v4(),
            name: "t".into(),
            folder: None,
            kind: TermKind::Shell,
            program: "wsl.exe".into(),
            args: vec!["-d".into(), "Ubuntu-24.04".into()],
            cwd: PathBuf::from("~"),
            order: 0,
            auto_restore: true,
            launched_once: false,
            status: TermStatus::Dead,
            last_cols: 0,
            last_rows: 0,
            live_cwd: None,
            inner_cli: None,
            hooked: true,
            shell_cfg: None,
            color_tag: None,
            asleep: false,
            reconnecting: false,
        };
        // WSL, pre-first-hook: the `--cd ~` truth.
        assert_eq!(meta.display_cwd(), "~");
        // Hooks reported: live POSIX cwd verbatim.
        meta.live_cwd = Some(PathBuf::from("/home/zany/proj"));
        assert_eq!(meta.display_cwd(), "/home/zany/proj");
        // Explicit Windows start dir, hookless: the automount translation —
        // true by wsl.exe --cd semantics — never the raw C:\ string.
        meta.live_cwd = None;
        meta.cwd = PathBuf::from("C:\\Users\\zany\\proj");
        assert_eq!(meta.display_cwd(), "/mnt/c/Users/zany/proj");
        // Untranslatable (UNC) degrades to ~, never a Windows path.
        meta.cwd = PathBuf::from("\\\\server\\share");
        assert_eq!(meta.display_cwd(), "~");
        // POSIX restore cwd rides verbatim.
        meta.cwd = PathBuf::from("/tmp");
        assert_eq!(meta.display_cwd(), "/tmp");
        // ssh: empty cwd (by design) presents as the remote home; a stray
        // Windows cwd (raw-API create) does too — /mnt is WSL semantics and
        // means nothing on a remote host.
        meta.program = "ssh.exe".into();
        meta.args = vec!["alice@devbox".into()];
        meta.cwd = PathBuf::new();
        assert_eq!(meta.display_cwd(), "~");
        meta.cwd = PathBuf::from("C:\\Terminal Control");
        assert_eq!(meta.display_cwd(), "~", "no /mnt story for a remote host");
        meta.cwd = PathBuf::new();
        meta.live_cwd = Some(PathBuf::from("/srv/app"));
        assert_eq!(meta.display_cwd(), "/srv/app");
        // Win namespace: live_cwd else cwd (the pre-existing rule).
        meta.program = "powershell.exe".into();
        meta.args = vec![];
        meta.live_cwd = None;
        meta.cwd = PathBuf::from("C:\\proj");
        assert_eq!(meta.display_cwd(), "C:\\proj");
        meta.live_cwd = Some(PathBuf::from("C:\\proj\\sub"));
        assert_eq!(meta.display_cwd(), "C:\\proj\\sub");
    }

    /// Bug 1: the claude launch-command resume-branch selection. A pinned
    /// terminal uses `--resume <id>` ONLY when (a) it has launched before AND
    /// (b) the session transcript exists on disk at claude's own layout
    /// (~/.claude/projects/<munged-cwd>/<id>.jsonl) — a `--resume` for a
    /// transcript-less id makes claude print "No conversation found" and
    /// exit (live-verified against 2.1.200), so `--session-id` (fresh) is
    /// the honest branch there. Uses a real fixture under the REAL home
    /// layout (unique per-pid cwd), removed after.
    #[test]
    fn claude_launch_command_resume_branch() {
        let sid = Uuid::new_v4();
        let cwd = PathBuf::from(format!("C:\\tc-launchcmd-{}", std::process::id()));
        let mut meta = TerminalMeta {
            id: Uuid::new_v4(),
            name: "c".into(),
            folder: None,
            kind: TermKind::Claude {
                session_id: sid,
                extra_args: vec![],
            },
            program: "claude".into(),
            args: vec![],
            cwd: cwd.clone(),
            order: 0,
            auto_restore: true,
            launched_once: false,
            status: TermStatus::Dead,
            last_cols: 0,
            last_rows: 0,
            live_cwd: None,
            inner_cli: None,
            hooked: false,
            shell_cfg: None,
            color_tag: None,
            asleep: false,
            reconnecting: false,
        };
        // Never launched: --session-id regardless of disk state.
        let (_, args) = meta.launch_command();
        assert_eq!(args[0], "--session-id");
        // Launched but no transcript on disk: still --session-id (fresh).
        meta.launched_once = true;
        let (_, args) = meta.launch_command();
        assert_eq!(args[0], "--session-id");
        // Transcript exists: --resume fires. (Only the resume pair is
        // pinned: a sibling pulse-ctl.exe next to the TEST binary would
        // legally append the Layer-2 `--settings` injection.)
        let file = claude_session_file(&cwd, &sid).expect("home dir");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"{}\n").unwrap();
        let (_, args) = meta.launch_command();
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(file.parent().unwrap());
        assert_eq!(&args[..2], &["--resume".to_string(), sid.to_string()]);
    }

    /// Attribution Layer 2: the injected --settings JSON — forward slashes
    /// (git-bash eats backslashes in hook command strings), single-quoted
    /// path (spaces survive bash), both events wired to `__claude-hook`,
    /// and an unquotable path degrades to None rather than mis-quoting.
    #[test]
    fn claude_hook_settings_json_shape() {
        let j =
            claude_hook_settings_json_for("C:\\Program Files\\Pulse\\bin\\pulse-ctl.exe")
                .unwrap();
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        let start = v["hooks"]["SessionStart"][0]["hooks"][0].clone();
        assert_eq!(start["type"], "command");
        assert_eq!(
            start["command"],
            "'C:/Program Files/Pulse/bin/pulse-ctl.exe' __claude-hook SessionStart"
        );
        let end = v["hooks"]["SessionEnd"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(end.ends_with("__claude-hook SessionEnd"));
        assert!(
            !j.contains('\\'),
            "backslashes must never reach a git-bash hook command"
        );
        // A single quote in the path can't be bash-quoted safely here.
        assert!(claude_hook_settings_json_for("C:/o'brien/pulse-ctl.exe").is_none());
    }

    /// SLEEP S1: the presented-status derivation table, plus the append-only
    /// serde rules for the `asleep` flag (old JSON ⇒ false; set value
    /// survives serde_json AND bincode round-trips like color_tag).
    #[test]
    fn presented_status_table() {
        use PresentedStatus::*;
        assert_eq!(presented_status(TermStatus::Running, false), Running);
        assert_eq!(presented_status(TermStatus::Running, true), Sleeping);
        assert_eq!(presented_status(TermStatus::Dead, true), Asleep);
        assert_eq!(presented_status(TermStatus::Dead, false), Dead);

        let old_term = r#"{
            "id": "6c0f7ee1-3f34-4b2e-9d5c-333333333333",
            "name": "t", "folder": null, "kind": "Shell",
            "program": "powershell.exe", "args": [], "cwd": "C:\\"
        }"#;
        let t: TerminalMeta = serde_json::from_str(old_term).unwrap();
        assert!(!t.asleep, "pre-sleep state.json loads awake");
        let mut t2 = t.clone();
        t2.asleep = true;
        let tj: TerminalMeta =
            serde_json::from_slice(&serde_json::to_vec(&t2).unwrap()).unwrap();
        assert!(tj.asleep);
        let tb: TerminalMeta =
            bincode::deserialize(&bincode::serialize(&t2).unwrap()).unwrap();
        assert!(tb.asleep, "asleep survives the bincode Snapshot wire");
    }

    /// Color-tag persistence round-trip (task #22): a pre-tag state.json
    /// loads with `color_tag: None` on terminals AND folders (append-only
    /// serde-default rule), and a tagged value survives save→load through
    /// both serde_json (state.json) and bincode (Snapshot wire).
    #[test]
    fn color_tag_round_trip() {
        // Old JSON without the field ⇒ None (both structs).
        let old_term = r#"{
            "id": "6c0f7ee1-3f34-4b2e-9d5c-111111111111",
            "name": "t", "folder": null, "kind": "Shell",
            "program": "powershell.exe", "args": [], "cwd": "C:\\"
        }"#;
        let t: TerminalMeta = serde_json::from_str(old_term).unwrap();
        assert_eq!(t.color_tag, None);
        let old_folder = r#"{
            "id": "6c0f7ee1-3f34-4b2e-9d5c-222222222222", "name": "f"
        }"#;
        let f: Folder = serde_json::from_str(old_folder).unwrap();
        assert_eq!(f.color_tag, None);

        // Tagged values survive both serializers.
        let mut t2 = t.clone();
        t2.color_tag = Some(5);
        let mut f2 = f.clone();
        f2.color_tag = Some(2);
        let tj: TerminalMeta =
            serde_json::from_slice(&serde_json::to_vec(&t2).unwrap()).unwrap();
        assert_eq!(tj.color_tag, Some(5));
        let fj: Folder = serde_json::from_slice(&serde_json::to_vec(&f2).unwrap()).unwrap();
        assert_eq!(fj.color_tag, Some(2));
        let tb: TerminalMeta =
            bincode::deserialize(&bincode::serialize(&t2).unwrap()).unwrap();
        assert_eq!(tb.color_tag, Some(5));
        let fb: Folder = bincode::deserialize(&bincode::serialize(&f2).unwrap()).unwrap();
        assert_eq!(fb.color_tag, Some(2));
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    fn tdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tc-state-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// R4-T1: rotate_log_at_startup semantics, pinned alongside moving the
    /// daemon call site behind the instance lock (r4 perf-daemon MEDIUM-1):
    /// missing ⇒ no-op; at/under the 4MB cap ⇒ untouched; over ⇒ renamed to
    /// `<name>.log.old` replacing (not appending to) the prior generation.
    #[test]
    fn log_rotation_at_startup() {
        const CAP: u64 = 4 * 1024 * 1024;
        let d = tdir("rot");
        let log = d.join("daemon.log");
        let old = d.join("daemon.log.old"); // the with_extension("log.old") math

        // Missing file ⇒ no-op, no .old minted.
        rotate_log_at_startup(&log);
        assert!(!log.exists() && !old.exists());

        // At/below the cap ⇒ untouched.
        std::fs::write(&log, vec![b'x'; CAP as usize]).unwrap();
        rotate_log_at_startup(&log);
        assert!(log.exists() && !old.exists(), "at-cap log must not rotate");

        // Above the cap ⇒ rename-replace: exactly one prior generation kept.
        std::fs::write(&old, b"prior-generation").unwrap();
        std::fs::write(&log, vec![b'y'; CAP as usize + 1]).unwrap();
        rotate_log_at_startup(&log);
        assert!(!log.exists(), "over-cap log must rotate away");
        assert_eq!(
            std::fs::metadata(&old).unwrap().len(),
            CAP + 1,
            "rename must REPLACE the prior generation"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// R4-F1: the load health flag. `true` ONLY for existed-and-parsed (the
    /// boot orphan reap is gated on it — a defaulted state would classify
    /// every journal as orphaned and delete it). Absent and corrupt both
    /// default UNHEALTHY; a corrupt file is moved aside (forensics), and a
    /// healthy load force-resets status to Dead.
    #[test]
    fn load_health_flag_and_corrupt_backup() {
        let d = tdir("load");
        let path = d.join("state.json");

        // Absent ⇒ default, unhealthy.
        let (s, healthy) = SharedState::load_from(&path);
        assert!(s.terminals.is_empty());
        assert!(!healthy, "absent state must be unhealthy (reap must not run)");

        // Healthy round-trip.
        let t: TerminalMeta = serde_json::from_str(
            r#"{
                "id": "6c0f7ee1-3f34-4b2e-9d5c-111111111111",
                "name": "t", "folder": null, "kind": "Shell",
                "program": "powershell.exe", "args": [], "cwd": "C:\\",
                "status": "Running", "reconnecting": true
            }"#,
        )
        .unwrap();
        let real = SharedState { folders: vec![], terminals: vec![t], next_order: 3 };
        std::fs::write(&path, serde_json::to_vec(&real).unwrap()).unwrap();
        let (s, healthy) = SharedState::load_from(&path);
        assert!(healthy);
        assert_eq!(s.terminals.len(), 1);
        assert_eq!(s.terminals[0].status, TermStatus::Dead, "boot force-reset");
        assert!(!s.terminals[0].reconnecting, "boot force-reset");

        // Corrupt ⇒ default, unhealthy, original preserved as .corrupt.
        std::fs::write(&path, b"{ not json").unwrap();
        let (s, healthy) = SharedState::load_from(&path);
        assert!(s.terminals.is_empty());
        assert!(!healthy, "corrupt state must be unhealthy (reap must not run)");
        assert!(!path.exists(), "corrupt file must be moved aside");
        assert_eq!(
            std::fs::read(d.join("state.json.corrupt")).unwrap(),
            b"{ not json",
            "first bad copy preserved for forensics"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
