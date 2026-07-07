//! Terminal selector launcher (selector-ui-spec §3/§4): the split-+ instant
//! spawn and the launcher palette that replace the NewTerminal form modal and
//! the Import modal.
//!
//! Layout of this file: an egui-free candidate core first (`SpawnSpec`,
//! `Candidate`, `build`/`filter`/`uniquify_name`/`spec_for` — unit-testable,
//! spec §12.1) and the egui view (`view`) after it. App wiring (open/close,
//! focus routing, CreateTerminal sends) lives in gui/mod.rs.
//!
//! Doctrine: refuse-over-guess everywhere — an unknown `kind_tag` produces no
//! suggestion row and no spawn; a nonexistent typed path produces no row and
//! no error chrome. Candidates are built at open time only (plus a debounced
//! rebuild on blocks_stamp drift); the per-frame work is grouping ≤~30 cached
//! rows, never a rebuild and never a filesystem touch (§1 inv. 4/7).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::import::FoundSession;
use crate::state::{BlockRec, NewTerminal, SharedState, TermKind};

// ─────────────────────────── core types (§4.3/§10) ───────────────────────────

/// Sticky spawn choice persisted in Prefs (§10). `kind_tag` is a STRING —
/// P6 kinds ("wsl:<distro>", "ssh:<host>") deserialize without lockstep
/// releases; unknown tags are refused at spec time, never guessed into a
/// program (§14.9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub kind_tag: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

/// One launchable shell (P6's `gui/shells.rs::ShellChoice` will replace this
/// shape; until then the §4.8 degraded set fills it — the section is
/// data-driven either way).
#[derive(Debug, Clone)]
pub struct ShellChoice {
    pub kind_tag: String,
    pub label: String,
    pub detail: String,
}

/// The §4.8 degraded shell set (the environment-independent prefix of
/// `gui::shells::shell_choices()`): PowerShell + cmd. "New Claude session"
/// rides the same section as its own `CandKind::ClaudeNew` row. Test-only
/// since P6a — the app path enumerates through gui/shells.rs.
#[cfg(test)]
pub fn degraded_shells() -> Vec<ShellChoice> {
    vec![
        ShellChoice {
            kind_tag: "powershell".into(),
            label: "PowerShell".into(),
            detail: String::new(),
        },
        ShellChoice {
            kind_tag: "cmd".into(),
            label: "cmd".into(),
            detail: String::new(),
        },
    ]
}

#[derive(Debug, Clone)]
pub enum CandKind {
    Shell(ShellChoice),
    /// Spawn the last-used shell in this directory. (§4.5 typed paths are a
    /// view-level row — `RowRef::Typed` — activated through `dir_spec`, so
    /// they never enter the candidate index.)
    RecentDir { cwd: PathBuf },
    ClaudeSession {
        session_id: Uuid,
        cwd: PathBuf,
        preview: String,
        project: String,
    },
    /// "New Claude session" in last_spawn's cwd.
    ClaudeNew,
    /// Expands the inline ssh editor (P6c §9): freeform host + the
    /// remote-hooks toggle.
    SshTo,
    /// Expands the inline custom editor (§4.6).
    Custom,
    /// A recent (kind+cwd) combo from `Prefs.recent_spawns`.
    Suggestion { spec: SpawnSpec },
}

/// Palette sections, in display order (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Suggested,
    Shells,
    RecentDirs,
    ClaudeSessions,
    Custom,
}

pub const SECTION_ORDER: [Section; 5] = [
    Section::Suggested,
    Section::Shells,
    Section::RecentDirs,
    Section::ClaudeSessions,
    Section::Custom,
];

pub fn section_of(kind: &CandKind) -> Section {
    match kind {
        CandKind::Suggestion { .. } => Section::Suggested,
        CandKind::Shell(_) | CandKind::SshTo | CandKind::ClaudeNew => Section::Shells,
        CandKind::RecentDir { .. } => Section::RecentDirs,
        CandKind::ClaudeSession { .. } => Section::ClaudeSessions,
        CandKind::Custom => Section::Custom,
    }
}

fn section_label(s: Section) -> Option<&'static str> {
    match s {
        Section::Suggested => Some("SUGGESTED"),
        Section::Shells => Some("SHELLS"),
        Section::RecentDirs => Some("RECENT DIRECTORIES"),
        Section::ClaudeSessions => Some("CLAUDE SESSIONS"),
        Section::Custom => None, // the row is its own label
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub kind: CandKind,
    pub label: String,
    pub secondary: String,
    pub label_lc: String,
    pub secondary_lc: String,
}

impl Candidate {
    fn new(kind: CandKind, label: String, secondary: String) -> Self {
        // Single-line-ify: claude previews can carry newlines, and a
        // multi-line galley would bleed outside the 34px row.
        let label = label.replace(['\r', '\n'], " ");
        let secondary = secondary.replace(['\r', '\n'], " ");
        let label_lc = label.to_lowercase();
        let secondary_lc = secondary.to_lowercase();
        Self { kind, label, secondary, label_lc, secondary_lc }
    }
}

// ─────────────────────────── tag mapping ───────────────────────────

/// Tags `spec_from_spawn` can turn into a real spawn today. P6a added
/// "wsl:<distro>"; P6c adds "ssh:<host>".
pub fn known_tag(tag: &str) -> bool {
    matches!(tag, "powershell" | "pwsh" | "cmd" | "claude" | "custom")
        || wsl_tag_distro(tag).is_some()
        || ssh_tag_host(tag).is_some()
}

/// "wsl:<distro>" → Some(distro); anything else (including a bare/empty
/// "wsl:") → None. The distro rides the tag so recent/sticky spawns stay
/// self-describing strings (§10).
pub fn wsl_tag_distro(tag: &str) -> Option<&str> {
    tag.strip_prefix("wsl:").filter(|d| !d.is_empty())
}

/// "ssh:<host>" → Some(host) — same self-describing-tag grammar as wsl.
pub fn ssh_tag_host(tag: &str) -> Option<&str> {
    tag.strip_prefix("ssh:").filter(|h| !h.is_empty())
}

/// Human name for a spawn kind, for suggestion rows and the instant-create
/// tooltip preview ("PowerShell · C:\dir").
pub fn display_kind(tag: &str, program: &str) -> String {
    if let Some(distro) = wsl_tag_distro(tag) {
        return format!("{distro} (WSL)");
    }
    if let Some(host) = ssh_tag_host(tag) {
        return format!("{host} (ssh)");
    }
    match tag {
        "powershell" => "PowerShell".into(),
        "pwsh" => "PowerShell 7".into(),
        "cmd" => "cmd".into(),
        "claude" => "Claude".into(),
        "custom" => Path::new(program)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| program.to_string()),
        other => other.to_string(),
    }
}

fn dir_label(cwd: &Path) -> String {
    cwd.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.to_string_lossy().to_string())
}

/// Auto-name grammar (D5, mirrors the old `default_name`): "Shell · dir",
/// "Claude · dir", "cmd · dir", bare dir for custom commands. An EMPTY dir
/// (a directory-less spec — shouldn't reach here since `nonempty_cwd` heals
/// spawn cwds, but poisoned persisted specs exist) drops the " · " suffix
/// instead of rendering a dangling "Ubuntu-24.04 · " header.
pub fn auto_name(tag: &str, cwd: &Path) -> String {
    let dir = dir_label(cwd);
    if let Some(distro) = wsl_tag_distro(tag) {
        return if dir.is_empty() {
            distro.to_string()
        } else {
            format!("{distro} · {dir}")
        };
    }
    if let Some(host) = ssh_tag_host(tag) {
        // ssh terminals have no meaningful Windows cwd — the host IS the
        // identity (the remote cwd arrives later via the hooks).
        return host.to_string();
    }
    if dir.is_empty() {
        return match tag {
            "claude" => "Claude".into(),
            "cmd" => "cmd".into(),
            "custom" => "Custom".into(),
            _ => "Shell".into(),
        };
    }
    match tag {
        "claude" => format!("Claude · {dir}"),
        "cmd" => format!("cmd · {dir}"),
        "custom" => dir,
        _ => format!("Shell · {dir}"),
    }
}

/// Heal a directory-less cwd before it becomes a Windows/WSL spawn: an ssh
/// terminal's SpawnSpec carries an EMPTY cwd by design (P6c — no Windows-side
/// directory), and anything inheriting it verbatim (`last_spawn` after an ssh
/// create, a recent recorded in that state) spawned `wsl --cd ""` ⇒
/// Wsl/E_INVALIDARG instant session death (2026-07-04 regression). Empty ⇒
/// the user's home, mirroring `default_spawn`; non-empty passes through
/// verbatim (POSIX restore cwds included).
fn nonempty_cwd(cwd: &Path) -> PathBuf {
    if cwd.as_os_str().is_empty() {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("C:\\"))
    } else {
        cwd.to_path_buf()
    }
}

/// Namespace-aware cwd heal (v0.1.1, the "WSL starts in /mnt/c/Users/<u>"
/// fix): ssh keeps its by-design empty cwd; a WSL tag heals empty to `~`
/// (the LINUX home — wsl.exe resolves `--cd ~` in-distro, any version, any
/// default user), never to the WINDOWS home like every other family.
/// Explicit directories still pass through verbatim for every tag ("open
/// this Windows project in WSL" via /mnt is a feature).
fn heal_cwd(kind_tag: &str, cwd: &Path) -> PathBuf {
    if ssh_tag_host(kind_tag).is_some() {
        return cwd.to_path_buf();
    }
    if wsl_tag_distro(kind_tag).is_some() {
        return if cwd.as_os_str().is_empty() {
            PathBuf::from("~")
        } else {
            cwd.to_path_buf()
        };
    }
    nonempty_cwd(cwd)
}

/// First free " 2"/" 3"/… suffix among `taken` (case-exact, D5/§12.1-1).
pub fn uniquify_name(base: &str, taken: &[&str]) -> String {
    if !taken.contains(&base) {
        return base.to_string();
    }
    let mut n = 2u32;
    loop {
        let cand = format!("{base} {n}");
        if !taken.contains(&cand.as_str()) {
            return cand;
        }
        n += 1;
    }
}

/// The instant-create default when no `last_spawn` exists yet: PowerShell in
/// the user's home dir (§3.1), unless the legacy `Prefs.last_cwd` carries a
/// real customized directory (§9: last_cwd seeds the first spawn's cwd).
pub fn default_spawn(seed_cwd: &str) -> SpawnSpec {
    let seed = seed_cwd.trim();
    let cwd = if !seed.is_empty() && seed != "C:\\" {
        PathBuf::from(seed)
    } else {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("C:\\"))
    };
    SpawnSpec {
        kind_tag: "powershell".into(),
        program: "powershell.exe".into(),
        args: Vec::new(),
        cwd,
    }
}

/// Tooltip preview of what a spawn spec launches (§3.1: the tooltip IS the
/// name/kind feedback).
pub fn spawn_preview(s: &SpawnSpec) -> String {
    format!("{} · {}", display_kind(&s.kind_tag, &s.program), s.cwd.display())
}

/// Map a `SpawnSpec` to a concrete `NewTerminal` (auto-named + uniquified).
/// Unknown tags ⇒ None — refuse-over-guess (§14.9): a wrong spawn is worse
/// than a missing one.
pub fn spec_from_spawn(
    spec: &SpawnSpec,
    folder: Option<Uuid>,
    taken: &[&str],
) -> Option<NewTerminal> {
    // P6c: hooked ssh — persisted args are `[user flags…, host]` only; the
    // daemon synthesizes the keepalives + one-shot remote bootstrap tail
    // per-spawn (token rotation), exactly like the wsl --cd/--exec tail. The
    // cwd is EMPTY: an ssh terminal has no Windows-side directory, and the
    // daemon treats a non-POSIX cwd as "remote default $HOME" (no cd).
    if let Some(host) = ssh_tag_host(&spec.kind_tag) {
        let args = if spec.args.is_empty() {
            vec![host.to_string()]
        } else {
            spec.args.clone()
        };
        let name = uniquify_name(&auto_name(&spec.kind_tag, &spec.cwd), taken);
        return Some(NewTerminal {
            name,
            folder,
            kind: TermKind::Shell,
            program: "ssh.exe".into(),
            args,
            cwd: PathBuf::new(),
            already_launched: false,
            shell_cfg: None, // remote_hooks default ON; ssh_spec opts out
        });
    }
    let (kind, program, args) = match spec.kind_tag.as_str() {
        "powershell" => (TermKind::Shell, "powershell.exe".to_string(), Vec::new()),
        "pwsh" => (TermKind::Shell, "pwsh.exe".to_string(), Vec::new()),
        // P6b: cmd is a first-class hooked Shell — the daemon injects the
        // PROMPT-env hooks per family (Custom would classify ShellFamily::
        // Other and spawn hookless).
        "cmd" => (TermKind::Shell, "cmd.exe".to_string(), Vec::new()),
        // P6a: hooked WSL bash in the picked distro. Only `-d <distro>` is
        // persisted — the daemon's spawn synthesizes the --cd/--exec tail
        // per-spawn (token rotation), exactly like the pwsh dot-source.
        tag if wsl_tag_distro(tag).is_some() => {
            let distro = wsl_tag_distro(tag).unwrap();
            (
                TermKind::Shell,
                "wsl.exe".to_string(),
                vec!["-d".to_string(), distro.to_string()],
            )
        }
        "claude" => (
            TermKind::Claude { session_id: Uuid::new_v4(), extra_args: spec.args.clone() },
            "claude".to_string(),
            Vec::new(),
        ),
        "custom" => (TermKind::Custom, spec.program.clone(), spec.args.clone()),
        _ => return None,
    };
    // Directory-full spawns only past this point (ssh returned above): an
    // empty persisted cwd (instant-create replaying an ssh-era last_spawn, a
    // poisoned recent) heals to home instead of reaching wsl.exe as --cd ""
    // — v0.1.1: WSL tags heal to the LINUX home (`~`), never the Windows one.
    let cwd = heal_cwd(&spec.kind_tag, &spec.cwd);
    let name = uniquify_name(&auto_name(&spec.kind_tag, &cwd), taken);
    Some(NewTerminal {
        name,
        folder,
        kind,
        program,
        args,
        cwd,
        already_launched: false,
        shell_cfg: None, // P6a defaults (bash); zsh/fish selection is P6a.2
    })
}

/// Directory rows (recents + typed paths): the row's dir with last_spawn's
/// kind (§4.4 "directory rows use the row's dir with last_spawn's shell").
pub fn dir_spec(
    cwd: &Path,
    last: &SpawnSpec,
    folder: Option<Uuid>,
    taken: &[&str],
) -> Option<(NewTerminal, SpawnSpec)> {
    let s = SpawnSpec {
        kind_tag: last.kind_tag.clone(),
        program: last.program.clone(),
        args: last.args.clone(),
        cwd: cwd.to_path_buf(),
    };
    let nt = spec_from_spawn(&s, folder, taken)?;
    Some((nt, s))
}

/// Inline custom-command creation (§4.6). The program field's first token is
/// the program; remaining tokens (both fields, quote-aware) are args.
pub fn custom_spec(
    prog_line: &str,
    args_line: &str,
    cwd: &Path,
    folder: Option<Uuid>,
    taken: &[&str],
) -> Option<(NewTerminal, SpawnSpec)> {
    let mut tokens = split_args(prog_line);
    tokens.extend(split_args(args_line));
    if tokens.is_empty() {
        return None;
    }
    let program = tokens.remove(0);
    let s = SpawnSpec {
        kind_tag: "custom".into(),
        program,
        args: tokens,
        // The caller passes last_spawn's cwd — heal an ssh-era empty one.
        cwd: nonempty_cwd(cwd),
    };
    let nt = spec_from_spawn(&s, folder, taken)?;
    Some((nt, s))
}

/// Freeform ssh creation (P6c §9): the "ssh to…" expansion's host line is
/// quote-aware-split into `[user flags…, host]` and validated through the
/// SAME destination rule the daemon's classifier applies (host must exist and
/// be last — a trailing remote command would classify Other and never hook).
/// `remote_hooks:false` persists the per-terminal opt-out in ShellCfg; the
/// recorded SpawnSpec carries the tag+args only, so a recent-spawn re-create
/// defaults back to hooks-on (the opt-out lives on the terminal, not the
/// recents ring).
pub fn ssh_spec(
    host_line: &str,
    remote_hooks: bool,
    folder: Option<Uuid>,
    taken: &[&str],
) -> Option<(NewTerminal, SpawnSpec)> {
    let tokens = split_args(host_line);
    let host = crate::state::ssh_destination(&tokens)?.to_string();
    let s = SpawnSpec {
        kind_tag: format!("ssh:{host}"),
        program: "ssh.exe".into(),
        args: tokens,
        cwd: PathBuf::new(),
    };
    let mut nt = spec_from_spawn(&s, folder, taken)?;
    if !remote_hooks {
        nt.shell_cfg = Some(crate::state::ShellCfg {
            remote_hooks: false,
            ..Default::default()
        });
    }
    Some((nt, s))
}

/// Activation mapping (§4.4). Returns the `NewTerminal` to send AND the
/// `SpawnSpec` to record into `last_spawn`/`recent_spawns`. `last` is the
/// effective sticky spawn (caller resolves the first-run default). None for
/// `Custom`/`SshTo` (their expansions have their own paths) and for unknown
/// tags.
pub fn spec_for(
    cand: &Candidate,
    last: &SpawnSpec,
    folder: Option<Uuid>,
    taken: &[&str],
) -> Option<(NewTerminal, SpawnSpec)> {
    match &cand.kind {
        CandKind::Shell(ch) => {
            let mut s = SpawnSpec {
                kind_tag: ch.kind_tag.clone(),
                program: String::new(),
                args: Vec::new(),
                // ssh rows carry no Windows cwd (recents dedupe by
                // (kind_tag, cwd) — keep it stable/empty per host). WSL rows
                // default to the LINUX home (v0.1.1: `~` — wsl.exe resolves
                // it in-distro; the old last_spawn inheritance landed every
                // distro in /mnt/c/Users/<u>, a Windows dir posing as a
                // Linux world). Every OTHER shell inherits last_spawn's dir
                // — healed: an ssh last_spawn's empty cwd must never ride
                // into a pwsh/cmd spawn OR its recorded spec (the --cd ""
                // regression). Explicit-directory rows (RecentDir/typed
                // paths) are untouched — "open this Windows project in WSL"
                // stays a feature.
                cwd: if ssh_tag_host(&ch.kind_tag).is_some() {
                    PathBuf::new()
                } else if wsl_tag_distro(&ch.kind_tag).is_some() {
                    PathBuf::from("~")
                } else {
                    nonempty_cwd(&last.cwd)
                },
            };
            let nt = spec_from_spawn(&s, folder, taken)?;
            s.program = nt.program.clone();
            Some((nt, s))
        }
        CandKind::Suggestion { spec } => {
            let nt = spec_from_spawn(spec, folder, taken)?;
            // Re-record the healed cwd (not the persisted one): activating a
            // poisoned empty-cwd recent converges the ring instead of
            // re-poisoning last_spawn. v0.1.1: namespace-aware — a WSL
            // recent's empty cwd heals to `~`, never the Windows home.
            let mut s = spec.clone();
            s.cwd = heal_cwd(&s.kind_tag, &s.cwd);
            Some((nt, s))
        }
        CandKind::RecentDir { cwd } => dir_spec(cwd, last, folder, taken),
        CandKind::ClaudeNew => {
            let s = SpawnSpec {
                kind_tag: "claude".into(),
                program: "claude".into(),
                args: Vec::new(),
                cwd: nonempty_cwd(&last.cwd),
            };
            let nt = spec_from_spawn(&s, folder, taken)?;
            Some((nt, s))
        }
        CandKind::ClaudeSession { session_id, cwd, preview, project } => {
            // Exactly the old Import spec per session (§4.4/F7), plus name
            // uniquification so pending_create's name join stays exact.
            let base: String = if preview.is_empty() {
                project.clone()
            } else {
                preview.chars().take(28).collect()
            };
            let nt = NewTerminal {
                name: uniquify_name(&base, taken),
                folder,
                kind: TermKind::Claude { session_id: *session_id, extra_args: Vec::new() },
                program: "claude".into(),
                args: Vec::new(),
                cwd: cwd.clone(),
                already_launched: true,
                shell_cfg: None,
            };
            let s = SpawnSpec {
                kind_tag: "claude".into(),
                program: "claude".into(),
                args: Vec::new(),
                cwd: cwd.clone(),
            };
            Some((nt, s))
        }
        CandKind::SshTo | CandKind::Custom => None,
    }
}

// ─────────────────────────── build + filter (§4.3) ───────────────────────────

/// Build the candidate index. Called at open time (and on debounced
/// blocks_stamp drift while open) — NEVER per frame (§1 inv. 7).
/// `blocks` = per-terminal block records (the history corpus already
/// client-side); `sessions` = the open-time `import::scan` result (this fn
/// re-filters already-imported ids against `state` so an import de-lists on
/// the next rebuild without rescanning the filesystem).
pub fn build(
    state: &SharedState,
    blocks: &[(Uuid, &[BlockRec])],
    shells: &[ShellChoice],
    sessions: &[FoundSession],
    recents: &[SpawnSpec],
) -> Vec<Candidate> {
    let mut out = Vec::new();

    // Suggested: last 3 distinct (kind_tag, cwd) combos, MRU-first (§4.3).
    let mut seen: Vec<(&str, &Path)> = Vec::new();
    for s in recents {
        if !known_tag(&s.kind_tag) {
            continue; // refuse-over-guess: no row for tags we can't spawn
        }
        if seen
            .iter()
            .any(|(t, c)| *t == s.kind_tag.as_str() && *c == s.cwd.as_path())
        {
            continue;
        }
        seen.push((s.kind_tag.as_str(), s.cwd.as_path()));
        out.push(Candidate::new(
            CandKind::Suggestion { spec: s.clone() },
            display_kind(&s.kind_tag, &s.program),
            s.cwd.to_string_lossy().into_owned(),
        ));
        if seen.len() == 3 {
            break;
        }
    }

    // Shells: the provided list verbatim (§4.8 degraded set until P6), plus
    // the New-Claude row.
    for ch in shells {
        out.push(Candidate::new(
            CandKind::Shell(ch.clone()),
            ch.label.clone(),
            ch.detail.clone(),
        ));
    }
    // Freeform ssh (P6c §9): always available — config hosts above are a
    // convenience, not a requirement.
    out.push(Candidate::new(
        CandKind::SshTo,
        "ssh to\u{2026}".into(),
        String::new(),
    ));
    out.push(Candidate::new(
        CandKind::ClaudeNew,
        "New Claude session".into(),
        String::new(),
    ));

    // Recent directories: union of terminal cwd/live_cwd + block-rec cwds,
    // case-insensitive dedupe, MRU by block started_ms, cap 12 (§4.3).
    let key = |p: &Path| p.to_string_lossy().to_lowercase();
    let mut dirs: HashMap<String, (PathBuf, u64)> = HashMap::new();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for t in &state.terminals {
        let eff = t.live_cwd.as_ref().unwrap_or(&t.cwd);
        *counts.entry(key(eff)).or_default() += 1;
        for p in [Some(&t.cwd), t.live_cwd.as_ref()].into_iter().flatten() {
            dirs.entry(key(p)).or_insert_with(|| (p.clone(), 0));
        }
    }
    for (_, recs) in blocks {
        for r in recs.iter() {
            if let Some(c) = &r.cwd {
                let e = dirs.entry(key(c)).or_insert_with(|| (c.clone(), 0));
                e.1 = e.1.max(r.started_ms);
            }
        }
    }
    let mut dir_rows: Vec<(String, PathBuf, u64)> =
        dirs.into_iter().map(|(k, (p, m))| (k, p, m)).collect();
    dir_rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    dir_rows.truncate(12);
    for (k, p, _) in dir_rows {
        let n = counts.get(&k).copied().unwrap_or(0);
        let secondary = if n > 1 { format!("{n} terminals") } else { String::new() };
        out.push(Candidate::new(
            CandKind::RecentDir { cwd: p.clone() },
            p.to_string_lossy().into_owned(),
            secondary,
        ));
    }

    // Claude sessions: minus already-imported ids, top 8 by mtime (`scan`
    // already sorts newest-first).
    let imported: HashSet<Uuid> = state
        .terminals
        .iter()
        .filter_map(|t| match &t.kind {
            TermKind::Claude { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .collect();
    for s in sessions
        .iter()
        .filter(|s| !imported.contains(&s.session_id))
        .take(8)
    {
        let label = if s.preview.is_empty() { s.project.clone() } else { s.preview.clone() };
        let secondary = format!("{} · {}", s.cwd.to_string_lossy(), short_age(s.modified));
        out.push(Candidate::new(
            CandKind::ClaudeSession {
                session_id: s.session_id,
                cwd: s.cwd.clone(),
                preview: s.preview.clone(),
                project: s.project.clone(),
            },
            label,
            secondary,
        ));
    }

    // Custom command… — always last (§4.3).
    out.push(Candidate::new(
        CandKind::Custom,
        "Custom command\u{2026}".into(),
        String::new(),
    ));
    out
}

/// history::filter's exact rules over label+secondary (§4.3): tokenized
/// AND-substring, case-insensitive, no fuzz. Empty query = identity.
pub fn filter(cands: &[Candidate], query: &str) -> Vec<u32> {
    let toks: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    cands
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            toks.iter()
                .all(|t| c.label_lc.contains(t.as_str()) || c.secondary_lc.contains(t.as_str()))
        })
        .map(|(i, _)| i as u32)
        .collect()
}

/// Relative age, launcher-secondary style ("2h", not "2h ago" — lane width).
fn short_age(t: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(t).unwrap_or_default().as_secs();
    match secs {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// Quote-aware command line splitter (moved from gui/mod.rs with the form
/// modal's deletion — the custom expansion is now its only user).
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.trim().chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ' ' if !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ─────────────────────────── pending create (§3.2) ───────────────────────────

/// How long a pending create keeps retargeting selection before it silently
/// gives up (daemon refused / raced rename).
pub const PENDING_EXPIRY: Duration = Duration::from_secs(5);

/// Resolve a pending create against this snapshot's NEW terminals:
/// exact-name match, newest `order` wins (§3.2).
pub fn resolve_pending(name: &str, newly: &[(Uuid, &str, i64)]) -> Option<Uuid> {
    newly
        .iter()
        .filter(|(_, n, _)| *n == name)
        .max_by_key(|(_, _, o)| *o)
        .map(|(id, ..)| *id)
}

// ─────────────────────────── key plan (§8, unit-tested) ───────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum EscAct {
    CloseFolderMenu,
    CollapseCustom,
    CollapseSsh,
    Close,
}

/// Esc peels one layer at a time (§4.6/§8): folder menu, then whichever
/// inline expansion is open (custom / ssh — mutually exclusive), then the
/// palette itself.
pub fn esc_act(folder_menu_open: bool, custom_open: bool, ssh_open: bool) -> EscAct {
    if folder_menu_open {
        EscAct::CloseFolderMenu
    } else if custom_open {
        EscAct::CollapseCustom
    } else if ssh_open {
        EscAct::CollapseSsh
    } else {
        EscAct::Close
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum EnterAct {
    CreateCustom,
    CreateSsh,
    ActivateSel,
}

/// Enter creates from the open expansion's fields (custom / ssh); otherwise
/// it activates the keyboard-selected row.
pub fn enter_act(custom_open: bool, ssh_open: bool) -> EnterAct {
    if custom_open {
        EnterAct::CreateCustom
    } else if ssh_open {
        EnterAct::CreateSsh
    } else {
        EnterAct::ActivateSel
    }
}

/// Does the query parse as an absolute path (§4.5)?
pub fn query_is_pathish(q: &str) -> bool {
    let q = q.trim();
    let bytes = q.as_bytes();
    (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\')
        || q.starts_with("\\\\")
}

// ─────────────────────────── state ───────────────────────────

/// §4.5 typed-path probe: one debounced `is_dir` per settled query, never a
/// scan per keystroke (§14.7).
pub struct TypedDir {
    pub q: String,
    pub at: Instant,
    pub checked: bool,
    pub dir: Option<PathBuf>,
}

pub struct LauncherState {
    pub query: String,
    /// Keyboard-selected index into the current display rows.
    pub sel: usize,
    pub cands: Vec<Candidate>,
    pub hits: Vec<u32>,
    /// Footer folder chip target (preset by "New terminal here…").
    pub folder: Option<Uuid>,
    pub custom_open: bool,
    /// One-frame flag: scroll the freshly-opened expansion into view (the
    /// Custom row is last — the fields would otherwise open below the fold).
    pub custom_reveal: bool,
    pub custom_prog: String,
    pub custom_args: String,
    /// P6c freeform-ssh expansion (mutually exclusive with custom_open).
    pub ssh_open: bool,
    pub ssh_reveal: bool,
    pub ssh_host: String,
    /// The per-terminal remote-hooks opt-in (default ON, spec §3.4.1).
    pub ssh_hooks: bool,
    pub typed: Option<TypedDir>,
    pub folder_menu: bool,
    /// Open-time scan results, cached so drift rebuilds never rescan the
    /// filesystem; `build` re-filters imported ids each rebuild.
    pub sessions: Vec<FoundSession>,
    pub shells: Vec<ShellChoice>,
    /// blocks_stamp at build; drift ⇒ debounced rebuild (history pattern).
    pub built: u64,
    pub built_at: Instant,
    /// §6.1 empty-state embed (inline content, no Area, no close).
    pub embedded: bool,
}

impl LauncherState {
    pub fn new(
        folder: Option<Uuid>,
        embedded: bool,
        shells: Vec<ShellChoice>,
        sessions: Vec<FoundSession>,
    ) -> Self {
        Self {
            query: String::new(),
            sel: 0,
            cands: Vec::new(),
            hits: Vec::new(),
            folder,
            custom_open: false,
            custom_reveal: false,
            custom_prog: String::new(),
            custom_args: String::new(),
            ssh_open: false,
            ssh_reveal: false,
            ssh_host: String::new(),
            ssh_hooks: true,
            typed: None,
            folder_menu: false,
            sessions,
            shells,
            built: u64::MAX,
            built_at: Instant::now(),
            embedded,
        }
    }
}

// ─────────────────────────── egui view (§4.2/§7) ───────────────────────────

use egui::{Align2, CornerRadius, FontId, Id, Key, Modifiers, Pos2, Rect, Sense, UiBuilder, Vec2};

use super::{draw_icon, lerp_col, semibold, Icon};
use super::{ACCENT, ACCENT_HOVER, OV_HOVER, SURFACE_2, SURFACE_3, SURFACE_4, TEXT, TEXT_FAINT, TEXT_MUTED, TEXT_SECONDARY};

pub struct ViewCtx<'a> {
    pub folders: &'a [crate::state::Folder],
    /// False while a modal is open — the modal owns keys and focus (§8).
    pub keys_enabled: bool,
    pub embedded: bool,
    pub width: f32,
    pub max_h: f32,
}

pub enum Activation {
    Cand(u32),
    Typed(PathBuf),
    Custom { prog: String, args: String },
    /// Freeform ssh from the "ssh to…" expansion (P6c).
    Ssh { host_line: String, remote_hooks: bool },
}

#[derive(Default)]
pub struct LauncherOut {
    pub activate: Option<Activation>,
    /// Esc past the last layer (overlay only; the embed never closes).
    pub close: bool,
    /// Footer chip "New folder…" — App opens Modal::NewFolder.
    pub new_folder: bool,
}

enum RowRef {
    Typed,
    Cand(u32),
}

enum Item {
    Label(&'static str),
    Row(RowRef),
}

const ROW_H: f32 = 34.0;
const LABEL_TOP: f32 = 12.0;
const LABEL_H: f32 = 16.0;
const LABEL_BOTTOM: f32 = 4.0;

fn glyph_for(kind: &CandKind) -> Icon {
    match kind {
        CandKind::Shell(_) | CandKind::SshTo => Icon::Shell,
        CandKind::Suggestion { spec } => match spec.kind_tag.as_str() {
            "claude" => Icon::ClaudeSpark,
            "custom" => Icon::Terminal,
            _ => Icon::Shell,
        },
        CandKind::RecentDir { .. } => Icon::Folder,
        CandKind::ClaudeNew | CandKind::ClaudeSession { .. } => Icon::ClaudeSpark,
        CandKind::Custom => Icon::Terminal,
    }
}

/// Display plan: typed row first, then filtered rows grouped by section in
/// order, section labels only for non-empty sections; empty-query caps
/// (Recent 5 / Claude 3, §4.3); the Custom row always present.
fn build_plan(st: &LauncherState) -> Vec<Item> {
    let mut items = Vec::new();
    if st.typed.as_ref().is_some_and(|t| t.dir.is_some()) {
        items.push(Item::Row(RowRef::Typed));
    }
    let empty_q = st.query.trim().is_empty();
    let mut custom_seen = false;
    for sec in SECTION_ORDER {
        let cap = if empty_q {
            match sec {
                Section::RecentDirs => 5,
                Section::ClaudeSessions => 3,
                _ => usize::MAX,
            }
        } else {
            usize::MAX
        };
        let rows: Vec<u32> = st
            .hits
            .iter()
            .copied()
            .filter(|&i| section_of(&st.cands[i as usize].kind) == sec)
            .take(cap)
            .collect();
        if rows.is_empty() {
            continue;
        }
        if let Some(lbl) = section_label(sec) {
            items.push(Item::Label(lbl));
        }
        for i in rows {
            if sec == Section::Custom {
                custom_seen = true;
            }
            items.push(Item::Row(RowRef::Cand(i)));
        }
    }
    if !custom_seen {
        // The escape hatch never filters away.
        if let Some(ci) = st.cands.iter().position(|c| matches!(c.kind, CandKind::Custom)) {
            items.push(Item::Row(RowRef::Cand(ci as u32)));
        }
    }
    items
}

/// The palette body: query row, sections, custom expansion, footer. Hosted in
/// the overlay Area (gui/mod.rs) or inline in the empty-state central (§6.1).
/// Zero strokes anywhere; structure is spacing + fills (§7).
pub fn view(ui: &mut egui::Ui, st: &mut LauncherState, vc: &ViewCtx) -> LauncherOut {
    let mut out = LauncherOut::default();
    ui.set_width(vc.width);
    ui.spacing_mut().item_spacing = Vec2::ZERO;

    // ── typed-path probe (§4.5): debounce 250ms, then ONE is_dir ──
    if query_is_pathish(&st.query) {
        let stale = st.typed.as_ref().is_none_or(|t| t.q != st.query);
        if stale {
            st.typed = Some(TypedDir {
                q: st.query.clone(),
                at: Instant::now(),
                checked: false,
                dir: None,
            });
        }
        if let Some(td) = &mut st.typed {
            if !td.checked {
                let elapsed = td.at.elapsed();
                if elapsed >= Duration::from_millis(250) {
                    td.checked = true;
                    let p = PathBuf::from(td.q.trim());
                    td.dir = p.is_dir().then_some(p);
                } else {
                    ui.ctx()
                        .request_repaint_after(Duration::from_millis(250) - elapsed);
                }
            }
        }
    } else {
        st.typed = None;
    }

    // ── consume nav keys BEFORE any TextEdit shows (§8, P3/P4 pattern) ──
    let (up, down, enter) = if vc.keys_enabled {
        ui.ctx().input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Enter),
            )
        })
    } else {
        (false, false, false)
    };
    let esc = vc.keys_enabled
        && (st.folder_menu || st.custom_open || st.ssh_open || !vc.embedded)
        && ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));

    // ── query row (36px): ❯ + borderless TextEdit, no input well ──
    let (qrect, _) = ui.allocate_exact_size(Vec2::new(vc.width, 36.0), Sense::hover());
    ui.painter().text(
        Pos2::new(qrect.min.x + 12.0, qrect.center().y),
        Align2::LEFT_CENTER,
        "\u{276f}",
        FontId::monospace(13.0),
        ACCENT,
    );
    let te_rect = Rect::from_min_max(
        Pos2::new(qrect.min.x + 30.0, qrect.min.y + 7.0),
        Pos2::new(qrect.max.x - 10.0, qrect.max.y - 7.0),
    );
    let mut tui = ui.new_child(
        UiBuilder::new()
            .max_rect(te_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let te = tui.add(
        egui::TextEdit::singleline(&mut st.query)
            .id(Id::new("launcher_q"))
            .hint_text("type to filter \u{2014} shells, folders, paths, sessions")
            .font(FontId::proportional(13.0))
            .frame(egui::Frame::NONE)
            .desired_width(te_rect.width()),
    );
    if vc.keys_enabled && !st.custom_open && !st.ssh_open && !st.folder_menu {
        te.request_focus();
    }
    if te.changed() {
        st.hits = filter(&st.cands, &st.query);
        st.sel = 0;
    }

    // ── display plan + keyboard application ──
    let plan = build_plan(st);
    let sel_count = plan
        .iter()
        .filter(|i| matches!(i, Item::Row(_)))
        .count();
    st.sel = st.sel.min(sel_count.saturating_sub(1));
    let mut kb_moved = false;
    if !st.custom_open && !st.ssh_open && !st.folder_menu {
        if up && st.sel > 0 {
            st.sel -= 1;
            kb_moved = true;
        }
        if down && st.sel + 1 < sel_count {
            st.sel += 1;
            kb_moved = true;
        }
    }
    if esc {
        match esc_act(st.folder_menu, st.custom_open, st.ssh_open) {
            EscAct::CloseFolderMenu => st.folder_menu = false,
            EscAct::CollapseCustom => st.custom_open = false,
            EscAct::CollapseSsh => st.ssh_open = false,
            EscAct::Close => {
                if !vc.embedded {
                    out.close = true;
                }
            }
        }
    }
    let custom_ready = !split_args(&st.custom_prog).is_empty();
    // Same validation the daemon's classifier applies: a destination exists
    // and is the last token (anything after it would be a remote command).
    let ssh_ready = crate::state::ssh_destination(&split_args(&st.ssh_host)).is_some();
    let mut toggle_custom = false;
    let mut toggle_ssh = false;
    if enter {
        match enter_act(st.custom_open, st.ssh_open) {
            EnterAct::CreateCustom => {
                if custom_ready {
                    out.activate = Some(Activation::Custom {
                        prog: st.custom_prog.clone(),
                        args: st.custom_args.clone(),
                    });
                }
            }
            EnterAct::CreateSsh => {
                if ssh_ready {
                    out.activate = Some(Activation::Ssh {
                        host_line: st.ssh_host.clone(),
                        remote_hooks: st.ssh_hooks,
                    });
                }
            }
            EnterAct::ActivateSel => {
                if let Some(item) = plan
                    .iter()
                    .filter_map(|i| match i {
                        Item::Row(r) => Some(r),
                        _ => None,
                    })
                    .nth(st.sel)
                {
                    match item {
                        RowRef::Typed => {
                            if let Some(p) =
                                st.typed.as_ref().and_then(|t| t.dir.clone())
                            {
                                out.activate = Some(Activation::Typed(p));
                            }
                        }
                        RowRef::Cand(i) => match st.cands[*i as usize].kind {
                            CandKind::Custom => toggle_custom = true,
                            CandKind::SshTo => toggle_ssh = true,
                            _ => out.activate = Some(Activation::Cand(*i)),
                        },
                    }
                }
            }
        }
    }

    // ── rows ──
    let width = vc.width;
    egui::ScrollArea::vertical()
        .max_height(vc.max_h)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            let mut sel_i = 0usize;
            for item in &plan {
                match item {
                    Item::Label(lbl) => {
                        ui.add_space(LABEL_TOP);
                        let (lrect, _) = ui
                            .allocate_exact_size(Vec2::new(width, LABEL_H), Sense::hover());
                        ui.painter().text(
                            Pos2::new(lrect.min.x + 12.0, lrect.center().y),
                            Align2::LEFT_CENTER,
                            *lbl,
                            semibold(11.0),
                            TEXT_FAINT,
                        );
                        ui.add_space(LABEL_BOTTOM);
                    }
                    Item::Row(rref) => {
                        let this_sel = sel_i;
                        sel_i += 1;
                        let selected = this_sel == st.sel;
                        let (glyph, label, secondary, is_custom, is_ssh_to) = match rref {
                            RowRef::Typed => {
                                let p = st
                                    .typed
                                    .as_ref()
                                    .and_then(|t| t.dir.as_ref())
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_default();
                                (Icon::Folder, format!("Open shell in {p}"), String::new(), false, false)
                            }
                            RowRef::Cand(i) => {
                                let c = &st.cands[*i as usize];
                                (
                                    glyph_for(&c.kind),
                                    c.label.clone(),
                                    c.secondary.clone(),
                                    matches!(c.kind, CandKind::Custom),
                                    matches!(c.kind, CandKind::SshTo),
                                )
                            }
                        };
                        let (rect, resp) = ui
                            .allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
                        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        let p = ui.painter().clone();
                        if selected {
                            // Same selection grammar as the sidebar (§4.2).
                            p.rect_filled(rect, CornerRadius::same(6), super::ACCENT_SUBTLE);
                            let bar = Rect::from_min_max(
                                Pos2::new(rect.min.x + 3.0, rect.min.y + 4.0),
                                Pos2::new(rect.min.x + 5.0, rect.max.y - 4.0),
                            );
                            p.rect_filled(bar, CornerRadius::same(1), ACCENT);
                        }
                        if resp.hovered() {
                            p.rect_filled(rect, CornerRadius::same(6), OV_HOVER);
                        }
                        let grect = Rect::from_center_size(
                            Pos2::new(rect.min.x + 20.0, rect.center().y),
                            Vec2::splat(16.0),
                        );
                        draw_icon(&p, grect.shrink(1.0), glyph, TEXT_MUTED);
                        // Secondary, right-aligned; the ↩ hint takes the far
                        // right on the kb-selected row (silent accelerator).
                        let mut right_x = rect.max.x - 12.0;
                        if selected {
                            p.text(
                                Pos2::new(rect.max.x - 12.0, rect.center().y),
                                Align2::RIGHT_CENTER,
                                "\u{21a9}",
                                FontId::proportional(11.0),
                                TEXT_FAINT,
                            );
                            right_x -= 20.0;
                        }
                        let mut sec_w = 0.0;
                        if !secondary.is_empty() {
                            let sg = p.layout_no_wrap(
                                super::middle_ellipsize(&secondary, 44),
                                FontId::proportional(11.0),
                                TEXT_MUTED,
                            );
                            sec_w = sg.size().x;
                            p.galley(
                                Pos2::new(right_x - sec_w, rect.center().y - sg.size().y / 2.0),
                                sg,
                                TEXT_MUTED,
                            );
                        }
                        // Label, clipped to its lane.
                        let lane_end = right_x - sec_w - 8.0;
                        let lg = p.layout_no_wrap(label, FontId::proportional(13.0), TEXT);
                        let cp = p.with_clip_rect(Rect::from_min_max(
                            Pos2::new(rect.min.x + 36.0, rect.min.y),
                            Pos2::new(lane_end.max(rect.min.x + 40.0), rect.max.y),
                        ));
                        cp.galley(
                            Pos2::new(rect.min.x + 36.0, rect.center().y - lg.size().y / 2.0),
                            lg,
                            TEXT,
                        );
                        if kb_moved && selected {
                            resp.scroll_to_me(None);
                        }
                        if resp.clicked() {
                            st.sel = this_sel;
                            match rref {
                                RowRef::Typed => {
                                    if let Some(pth) =
                                        st.typed.as_ref().and_then(|t| t.dir.clone())
                                    {
                                        out.activate = Some(Activation::Typed(pth));
                                    }
                                }
                                RowRef::Cand(i) => {
                                    if is_custom {
                                        toggle_custom = true;
                                    } else if is_ssh_to {
                                        toggle_ssh = true;
                                    } else {
                                        out.activate = Some(Activation::Cand(*i));
                                    }
                                }
                            }
                        }
                        // §4.6: the expansion grows in place below the row.
                        if is_custom && st.custom_open {
                            custom_expansion(ui, st, width, custom_ready, &mut out);
                        }
                        if is_ssh_to && st.ssh_open {
                            ssh_expansion(ui, st, width, ssh_ready, &mut out);
                        }
                    }
                }
            }
        });

    if toggle_custom {
        st.custom_open = !st.custom_open;
        if st.custom_open {
            st.ssh_open = false; // one inline expansion at a time
            st.custom_reveal = true;
            ui.ctx()
                .memory_mut(|m| m.request_focus(Id::new("launcher_custom_prog")));
        }
    }
    if toggle_ssh {
        st.ssh_open = !st.ssh_open;
        if st.ssh_open {
            st.custom_open = false;
            st.ssh_reveal = true;
            ui.ctx()
                .memory_mut(|m| m.request_focus(Id::new("launcher_ssh_host")));
        }
    }

    // ── footer lane (28px, §4.2) — overlay only; the embed has no folders UI ──
    if !vc.embedded {
        ui.add_space(4.0);
        let (frect, _) = ui.allocate_exact_size(Vec2::new(width, 28.0), Sense::hover());
        let p = ui.painter().clone();
        let fname = st
            .folder
            .and_then(|id| vc.folders.iter().find(|f| f.id == id))
            .map(|f| f.name.clone())
            .unwrap_or_else(|| "(no folder)".into());
        let chip_txt = format!("in: {fname} \u{25be}");
        let cg = p.layout_no_wrap(chip_txt, FontId::proportional(12.0), TEXT_SECONDARY);
        let chip_rect = Rect::from_min_size(
            Pos2::new(frect.min.x + 12.0, frect.center().y - 10.0),
            Vec2::new(cg.size().x + 8.0, 20.0),
        );
        let chip = ui.interact(chip_rect, Id::new("launcher_folder_chip"), Sense::click());
        let ct = ui
            .ctx()
            .animate_bool_with_time(chip.id, chip.hovered(), 0.12);
        p.galley(
            Pos2::new(chip_rect.min.x + 4.0, chip_rect.center().y - cg.size().y / 2.0),
            cg,
            lerp_col(TEXT_SECONDARY, TEXT, ct),
        );
        if chip
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            st.folder_menu = !st.folder_menu;
        }
        p.text(
            Pos2::new(frect.max.x - 12.0, frect.center().y),
            Align2::RIGHT_CENTER,
            "Enter creates  \u{00b7}  Esc",
            FontId::proportional(11.0),
            TEXT_FAINT,
        );

        // Folder chip popup: strokeless surface, same grammar as the panels.
        if st.folder_menu {
            folder_menu(ui.ctx(), st, vc, chip_rect, &mut out);
        }
    }
    ui.add_space(6.0);
    out
}

/// One borderless expansion field on a SURFACE_2 well (shared by the custom
/// and ssh expansions).
fn expansion_field(ui: &mut egui::Ui, width: f32, id: &str, text: &mut String, hint: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 28.0), Sense::hover());
    let well = Rect::from_min_max(
        Pos2::new(rect.min.x + 36.0, rect.min.y + 2.0),
        Pos2::new(rect.max.x - 12.0, rect.max.y - 2.0),
    );
    ui.painter().rect_filled(well, CornerRadius::same(4), SURFACE_2);
    let mut fui = ui.new_child(
        UiBuilder::new()
            .max_rect(well.shrink2(Vec2::new(8.0, 3.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    fui.add(
        egui::TextEdit::singleline(text)
            .id(Id::new(id))
            .hint_text(hint)
            .font(FontId::proportional(13.0))
            .frame(egui::Frame::NONE)
            .desired_width(well.width() - 16.0),
    );
    ui.add_space(2.0);
}

/// The accent "Create" text-button row (Run ▸ grammar). Returns true when a
/// ready click landed.
fn expansion_create(ui: &mut egui::Ui, width: f32, id: &str, ready: bool) -> bool {
    let (crect, _) = ui.allocate_exact_size(Vec2::new(width, 22.0), Sense::hover());
    let p = ui.painter().clone();
    let cg = p.layout_no_wrap("Create".into(), FontId::proportional(12.0), ACCENT);
    let brect = Rect::from_min_size(
        Pos2::new(crect.max.x - 12.0 - cg.size().x - 8.0, crect.min.y),
        Vec2::new(cg.size().x + 8.0, 22.0),
    );
    let bresp = ui.interact(brect, Id::new(id), Sense::click());
    let col = if !ready {
        TEXT_FAINT
    } else if bresp.hovered() {
        ACCENT_HOVER
    } else {
        ACCENT
    };
    p.galley(
        Pos2::new(brect.min.x + 4.0, brect.center().y - cg.size().y / 2.0),
        cg,
        col,
    );
    bresp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
        && ready
}

/// The §4.6 inline custom-command editor: two borderless fields on SURFACE_2
/// wells + an accent "Create" text-button (Run ▸ grammar).
fn custom_expansion(
    ui: &mut egui::Ui,
    st: &mut LauncherState,
    width: f32,
    ready: bool,
    out: &mut LauncherOut,
) {
    ui.add_space(4.0);
    expansion_field(ui, width, "launcher_custom_prog", &mut st.custom_prog, "program");
    expansion_field(ui, width, "launcher_custom_args", &mut st.custom_args, "arguments");
    if expansion_create(ui, width, "launcher_custom_create", ready) {
        out.activate = Some(Activation::Custom {
            prog: st.custom_prog.clone(),
            args: st.custom_args.clone(),
        });
    }
    ui.add_space(4.0);
    if std::mem::take(&mut st.custom_reveal) {
        // The Custom row is last: without this the fields open below the
        // scroll fold.
        ui.scroll_to_cursor(None);
    }
}

/// The P6c inline ssh editor (§9): one host field (flags allowed — the same
/// destination rule the daemon classifies by), the remote-hooks toggle
/// (default ON) with its one-line explanation, and Create. Same strokeless
/// grammar as the custom expansion.
fn ssh_expansion(
    ui: &mut egui::Ui,
    st: &mut LauncherState,
    width: f32,
    ready: bool,
    out: &mut LauncherOut,
) {
    ui.add_space(4.0);
    expansion_field(
        ui,
        width,
        "launcher_ssh_host",
        &mut st.ssh_host,
        "user@host  (ssh flags ok, host last)",
    );
    // Remote-hooks toggle: accent text affordance, no box (doctrine §7).
    let (trect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    let p = ui.painter().clone();
    let label = if st.ssh_hooks {
        "\u{2713} remote hooks"
    } else {
        "\u{00b7} remote hooks off"
    };
    let tg = p.layout_no_wrap(label.into(), FontId::proportional(12.0), ACCENT);
    let brect = Rect::from_min_size(
        Pos2::new(trect.min.x + 36.0, trect.min.y),
        Vec2::new(tg.size().x + 8.0, 18.0),
    );
    let bresp = ui.interact(brect, Id::new("launcher_ssh_hooks"), Sense::click());
    let col = match (st.ssh_hooks, bresp.hovered()) {
        (true, true) => ACCENT_HOVER,
        (true, false) => ACCENT,
        (false, true) => TEXT_SECONDARY,
        (false, false) => TEXT_MUTED,
    };
    p.galley(
        Pos2::new(brect.min.x, brect.center().y - tg.size().y / 2.0),
        tg,
        col,
    );
    if bresp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        st.ssh_hooks = !st.ssh_hooks;
    }
    // The one-line explanation (§9 RemoteHooks field contract).
    let (erect, _) = ui.allocate_exact_size(Vec2::new(width, 16.0), Sense::hover());
    p.text(
        Pos2::new(erect.min.x + 36.0, erect.center().y),
        Align2::LEFT_CENTER,
        if st.ssh_hooks {
            "blocks + composer via a one-shot bash bootstrap on the remote; nothing persists there"
        } else {
            "plain ssh \u{2014} no blocks, no composer, no remote cwd tracking"
        },
        FontId::proportional(11.0),
        TEXT_FAINT,
    );
    if expansion_create(ui, width, "launcher_ssh_create", ready) {
        out.activate = Some(Activation::Ssh {
            host_line: st.ssh_host.clone(),
            remote_hooks: st.ssh_hooks,
        });
    }
    ui.add_space(4.0);
    if std::mem::take(&mut st.ssh_reveal) {
        ui.scroll_to_cursor(None);
    }
}

/// Footer folder-target popup: "(no folder)", every folder, "New folder…".
/// Manual Area so the styling is exactly the strokeless panel grammar (§14.8).
fn folder_menu(
    ctx: &egui::Context,
    st: &mut LauncherState,
    vc: &ViewCtx,
    chip_rect: Rect,
    out: &mut LauncherOut,
) {
    let mut folders: Vec<&crate::state::Folder> = vc.folders.iter().collect();
    folders.sort_by_key(|f| f.order);
    let area = egui::Area::new(Id::new("launcher_folder_menu"))
        .order(egui::Order::Tooltip)
        .pivot(Align2::LEFT_BOTTOM)
        .fixed_pos(Pos2::new(chip_rect.left(), chip_rect.top() - 4.0));
    let aresp = area.show(ctx, |ui| {
        egui::Frame::new()
            .fill(SURFACE_3)
            .corner_radius(CornerRadius::same(8))
            .shadow(egui::epaint::Shadow {
                offset: [0, 4],
                blur: 18,
                spread: 0,
                color: egui::Color32::from_black_alpha(130),
            })
            .inner_margin(egui::Margin::same(4))
            .show(ui, |ui| {
                ui.set_width(200.0);
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                enum Pick {
                    None_,
                    Folder(Uuid),
                    New,
                }
                let mut pick: Option<Pick> = None;
                let row = |ui: &mut egui::Ui, key: usize, label: &str, dim: bool| -> bool {
                    let (rect, resp) = ui
                        .allocate_exact_size(Vec2::new(200.0, 24.0), Sense::click());
                    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.hovered() {
                        // SURFACE_4, not OV_HOVER: on the SURFACE_3 popup the
                        // faint overlay read as "no hover feedback" (task-2
                        // fix — same token as the egui-native menu items).
                        ui.painter()
                            .rect_filled(rect, CornerRadius::same(4), SURFACE_4);
                    }
                    ui.painter().text(
                        Pos2::new(rect.min.x + 8.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        label,
                        FontId::proportional(12.0),
                        if dim { TEXT_SECONDARY } else { TEXT },
                    );
                    let _ = key;
                    resp.clicked()
                };
                if row(ui, 0, "(no folder)", st.folder.is_some()) {
                    pick = Some(Pick::None_);
                }
                for (i, f) in folders.iter().enumerate() {
                    if row(ui, i + 1, &f.name, false) {
                        pick = Some(Pick::Folder(f.id));
                    }
                }
                if row(ui, usize::MAX, "New folder\u{2026}", true) {
                    pick = Some(Pick::New);
                }
                match pick {
                    Some(Pick::None_) => {
                        st.folder = None;
                        st.folder_menu = false;
                    }
                    Some(Pick::Folder(id)) => {
                        st.folder = Some(id);
                        st.folder_menu = false;
                    }
                    Some(Pick::New) => {
                        out.new_folder = true;
                        st.folder_menu = false;
                    }
                    None => {}
                }
            });
    });
    // Press outside the menu and off the chip closes it (press origin —
    // the blocks-panel pattern).
    let mrect = aresp.response.rect;
    if ctx.input(|i| {
        i.pointer.primary_pressed()
            && i.pointer
                .press_origin()
                .is_some_and(|p| !mrect.contains(p) && !chip_rect.contains(p))
    }) {
        st.folder_menu = false;
    }
}

// ─────────────────────────── tests (§12.1) ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TerminalMeta;

    fn spawn(tag: &str, cwd: &str) -> SpawnSpec {
        SpawnSpec {
            kind_tag: tag.into(),
            program: match tag {
                "powershell" => "powershell.exe".into(),
                "cmd" => "cmd.exe".into(),
                "claude" => "claude".into(),
                _ => "x.exe".into(),
            },
            args: Vec::new(),
            cwd: PathBuf::from(cwd),
        }
    }

    fn term(name: &str, cwd: &str, kind: TermKind) -> TerminalMeta {
        TerminalMeta {
            id: Uuid::new_v4(),
            name: name.into(),
            folder: None,
            kind,
            program: "powershell.exe".into(),
            args: Vec::new(),
            cwd: PathBuf::from(cwd),
            order: 0,
            auto_restore: true,
            launched_once: false,
            status: crate::state::TermStatus::Running,
            last_cols: 0,
            last_rows: 0,
            live_cwd: None,
            inner_cli: None,
            hooked: false,
            shell_cfg: None,
            color_tag: None,
            asleep: false,
            reconnecting: false,
        }
    }

    fn rec(cwd: &str, started_ms: u64) -> BlockRec {
        BlockRec {
            epoch: 1,
            n: 0,
            cmd: "echo x".into(),
            cwd: Some(PathBuf::from(cwd)),
            exit: Some(0),
            started_ms,
            ended_ms: Some(started_ms + 1),
            start_off: started_ms,
            end_off: Some(started_ms + 10),
            truncated: false,
        }
    }

    fn session(id: Uuid, cwd: &str, preview: &str) -> FoundSession {
        FoundSession {
            session_id: id,
            cwd: PathBuf::from(cwd),
            project: "proj".into(),
            modified: SystemTime::now(),
            preview: preview.into(),
        }
    }

    /// P6a: the "wsl:<distro>" tag grammar — known, display/auto-name forms,
    /// and the spawn mapping (TermKind::Shell + wsl.exe + ONLY `-d <distro>`
    /// persisted; the daemon synthesizes the --cd/--exec tail per spawn).
    #[test]
    fn wsl_tag_mapping() {
        assert!(known_tag("wsl:Ubuntu-24.04"));
        assert!(!known_tag("wsl:"), "empty distro is refused");
        assert!(!known_tag("wsl"), "bare wsl is not a launcher tag");
        assert!(known_tag("ssh:devbox"), "ssh tags are known since P6c");
        assert!(!known_tag("ssh:"), "empty host is refused");
        assert!(!known_tag("ssh"), "bare ssh is not a launcher tag");
        assert_eq!(display_kind("wsl:Ubuntu", "wsl.exe"), "Ubuntu (WSL)");
        assert_eq!(
            auto_name("wsl:Ubuntu", Path::new("C:\\proj")),
            "Ubuntu · proj"
        );

        let nt = spec_from_spawn(&spawn("wsl:Ubuntu-24.04", "C:\\proj"), None, &[]).unwrap();
        assert_eq!(nt.kind, TermKind::Shell);
        assert_eq!(nt.program, "wsl.exe");
        assert_eq!(nt.args, vec!["-d".to_string(), "Ubuntu-24.04".to_string()]);
        assert_eq!(nt.cwd, PathBuf::from("C:\\proj"));
        assert_eq!(nt.name, "Ubuntu-24.04 · proj");
        assert!(nt.shell_cfg.is_none());
        // The daemon-side classifier agrees with the launcher mapping.
        assert_eq!(
            crate::state::shell_family(&nt.kind, &nt.program, &nt.args),
            crate::state::ShellFamily::WslShell { distro: Some("Ubuntu-24.04".into()) }
        );
        // Refused tags produce no spawn.
        assert!(spec_from_spawn(&spawn("wsl:", "C:\\x"), None, &[]).is_none());
        assert!(spec_from_spawn(&spawn("ssh:", "C:\\x"), None, &[]).is_none());
        assert!(spec_from_spawn(&spawn("zsh:foo", "C:\\x"), None, &[]).is_none());
    }

    /// P6c: the "ssh:<host>" tag grammar — known, display/auto-name forms,
    /// the spawn mapping (TermKind::Shell + ssh.exe + `[user flags…, host]`
    /// persisted, EMPTY cwd; the daemon synthesizes keepalives + the one-shot
    /// remote bootstrap per spawn), and the freeform `ssh_spec` path with the
    /// remote-hooks opt-out landing in ShellCfg.
    #[test]
    fn ssh_tag_mapping() {
        assert_eq!(display_kind("ssh:alice@devbox", "ssh.exe"), "alice@devbox (ssh)");
        assert_eq!(auto_name("ssh:alice@devbox", Path::new("")), "alice@devbox");

        let nt = spec_from_spawn(&spawn("ssh:devbox", "C:\\proj"), None, &[]).unwrap();
        assert_eq!(nt.kind, TermKind::Shell);
        assert_eq!(nt.program, "ssh.exe");
        assert_eq!(nt.args, vec!["devbox".to_string()]);
        assert_eq!(nt.cwd, PathBuf::new(), "ssh terminals carry no Windows cwd");
        assert_eq!(nt.name, "devbox");
        assert!(nt.shell_cfg.is_none(), "hooks default ON = no cfg needed");
        // The daemon-side classifier agrees with the launcher mapping.
        assert_eq!(
            crate::state::shell_family(&nt.kind, &nt.program, &nt.args),
            crate::state::ShellFamily::Ssh { host: "devbox".into() }
        );

        // Freeform with flags: destination must be last; SpawnSpec carries
        // the full args so recents re-create the same connection.
        let (nt, sp) = ssh_spec("-p 2222 alice@devbox", true, None, &[]).unwrap();
        assert_eq!(nt.args, vec!["-p".to_string(), "2222".into(), "alice@devbox".into()]);
        assert_eq!(sp.kind_tag, "ssh:alice@devbox");
        assert!(nt.shell_cfg.is_none());
        assert_eq!(
            crate::state::shell_family(&nt.kind, &nt.program, &nt.args),
            crate::state::ShellFamily::Ssh { host: "alice@devbox".into() }
        );
        // Hooks opt-out persists on the terminal (per-host, spec §3.4.1).
        let (nt, _) = ssh_spec("devbox", false, None, &[]).unwrap();
        assert_eq!(
            nt.shell_cfg,
            Some(crate::state::ShellCfg { remote_hooks: false, ..Default::default() })
        );
        // Invalid host lines are refused (trailing remote command, empty).
        assert!(ssh_spec("devbox uptime", true, None, &[]).is_none());
        assert!(ssh_spec("", true, None, &[]).is_none());
        assert!(ssh_spec("-p 2222", true, None, &[]).is_none());
    }

    /// P6b: the cmd row spawns a first-class hooked Shell (it used to map to
    /// TermKind::Custom, which classifies ShellFamily::Other and spawned
    /// hookless — no PROMPT hooks, no blocks, no composer strip).
    #[test]
    fn cmd_tag_maps_to_first_class_shell() {
        let nt = spec_from_spawn(&spawn("cmd", "C:\\proj"), None, &[]).unwrap();
        assert_eq!(nt.kind, TermKind::Shell);
        assert_eq!(nt.program, "cmd.exe");
        assert!(nt.args.is_empty());
        assert_eq!(nt.name, "cmd · proj");
        // The daemon-side classifier agrees with the launcher mapping.
        assert_eq!(
            crate::state::shell_family(&nt.kind, &nt.program, &nt.args),
            crate::state::ShellFamily::Cmd
        );
    }

    // §12.1-1
    #[test]
    fn uniquify_name_suffixes() {
        assert_eq!(uniquify_name("Shell · dir", &[]), "Shell · dir");
        assert_eq!(uniquify_name("Shell · dir", &["Shell · dir"]), "Shell · dir 2");
        assert_eq!(
            uniquify_name("Shell · dir", &["Shell · dir", "Shell · dir 2"]),
            "Shell · dir 3"
        );
        // Case-exact: a different case is a different name.
        assert_eq!(uniquify_name("shell", &["Shell"]), "shell");
    }

    // §12.1-2
    #[test]
    fn build_sections() {
        let mut state = SharedState::default();
        let imported_id = Uuid::new_v4();
        let mut t1 = term("A", "C:\\One", TermKind::Shell);
        t1.live_cwd = Some(PathBuf::from("C:\\Live"));
        let t2 = term("B", "C:\\One", TermKind::Shell);
        let t3 = term(
            "C",
            "C:\\Claude",
            TermKind::Claude { session_id: imported_id, extra_args: Vec::new() },
        );
        let (id1, id2) = (t1.id, t2.id);
        state.terminals = vec![t1, t2, t3];

        let recs1 = vec![rec("C:\\Blocks", 500), rec("C:\\One", 900)];
        let recs2 = vec![rec("C:\\Blocks", 700)];
        let blocks: Vec<(Uuid, &[BlockRec])> =
            vec![(id1, recs1.as_slice()), (id2, recs2.as_slice())];

        // Recents: MRU-first ring, one dup (same tag+cwd), one unknown tag.
        let recents = vec![
            spawn("powershell", "C:\\One"),
            spawn("zsh:foo", "C:\\One"), // unknown tag ⇒ refused, no row
            spawn("claude", "C:\\Two"),
            spawn("powershell", "C:\\One"), // dup of head ⇒ deduped
            spawn("cmd", "C:\\Three"),
            spawn("powershell", "C:\\Four"), // 4th distinct ⇒ beyond cap 3
        ];

        let sessions = vec![
            session(imported_id, "C:\\Claude", "already imported"),
            session(Uuid::new_v4(), "C:\\Fresh", "fix the overlay bug"),
        ];

        let cands = build(&state, &blocks, &degraded_shells(), &sessions, &recents);

        // Suggested: 3 distinct known combos, MRU-first == recents head.
        let sugg: Vec<&Candidate> = cands
            .iter()
            .filter(|c| matches!(c.kind, CandKind::Suggestion { .. }))
            .collect();
        assert_eq!(sugg.len(), 3);
        assert_eq!(sugg[0].label, "PowerShell");
        assert_eq!(sugg[0].secondary, "C:\\One");
        assert_eq!(sugg[1].label, "Claude");
        assert_eq!(sugg[2].label, "cmd");

        // Shells: degraded set + New Claude session.
        let shell_labels: Vec<&str> = cands
            .iter()
            .filter(|c| section_of(&c.kind) == Section::Shells)
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(
            shell_labels,
            vec!["PowerShell", "cmd", "ssh to\u{2026}", "New Claude session"]
        );

        // Recent dirs: union of cwd/live_cwd/block cwds, MRU by started_ms.
        let dirs: Vec<&Candidate> = cands
            .iter()
            .filter(|c| matches!(c.kind, CandKind::RecentDir { .. }))
            .collect();
        let labels: Vec<&str> = dirs.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"C:\\Live"), "live_cwd in the union");
        assert_eq!(labels[0], "C:\\One", "max block started_ms (900) first");
        assert_eq!(labels[1], "C:\\Blocks", "next recency (700)");
        // Two terminals live in C:\One (t1's live_cwd moved away, t2's cwd
        // counts; t1's plain cwd union entry remains).
        let one = dirs.iter().find(|c| c.label == "C:\\One").unwrap();
        assert_eq!(one.secondary, "", "only t2 LIVES there (t1 cd'd away)");

        // Claude sessions: imported id filtered out.
        let claude: Vec<&Candidate> = cands
            .iter()
            .filter(|c| matches!(c.kind, CandKind::ClaudeSession { .. }))
            .collect();
        assert_eq!(claude.len(), 1);
        assert_eq!(claude[0].label, "fix the overlay bug");

        // Custom last; section order stable/monotonic.
        assert!(matches!(cands.last().unwrap().kind, CandKind::Custom));
        let mut last = 0usize;
        for c in &cands {
            let pos = SECTION_ORDER
                .iter()
                .position(|s| *s == section_of(&c.kind))
                .unwrap();
            assert!(pos >= last, "sections in order");
            last = pos;
        }
    }

    #[test]
    fn build_counts_multi_terminal_dirs() {
        let state = SharedState {
            terminals: vec![
                term("A", "C:\\Same", TermKind::Shell),
                term("B", "C:\\Same", TermKind::Shell),
            ],
            ..Default::default()
        };
        let cands = build(&state, &[], &degraded_shells(), &[], &[]);
        let dir = cands
            .iter()
            .find(|c| matches!(c.kind, CandKind::RecentDir { .. }))
            .unwrap();
        assert_eq!(dir.secondary, "2 terminals");
    }

    #[test]
    fn build_caps_recent_dirs_at_12() {
        let mut state = SharedState::default();
        for i in 0..20 {
            state.terminals.push(term("t", &format!("C:\\D{i:02}"), TermKind::Shell));
        }
        let cands = build(&state, &[], &degraded_shells(), &[], &[]);
        let n = cands
            .iter()
            .filter(|c| matches!(c.kind, CandKind::RecentDir { .. }))
            .count();
        assert_eq!(n, 12);
    }

    // §12.1-3 (mirror of history::filter tests)
    #[test]
    fn filter_tokens_and_case() {
        let cands = vec![
            Candidate::new(CandKind::ClaudeNew, "New Claude session".into(), String::new()),
            Candidate::new(
                CandKind::RecentDir { cwd: PathBuf::from("C:\\Proj") },
                "C:\\Proj".into(),
                "3 terminals".into(),
            ),
            Candidate::new(CandKind::Custom, "Custom command\u{2026}".into(), String::new()),
        ];
        // Multi-token AND, case-insensitive.
        let hits = filter(&cands, "new CLAUDE");
        assert_eq!(hits, vec![0]);
        // Secondary matches too.
        assert_eq!(filter(&cands, "terminals"), vec![1]);
        // Empty query = identity, order preserved.
        assert_eq!(filter(&cands, ""), vec![0, 1, 2]);
        // No hits.
        assert!(filter(&cands, "zzz qqq").is_empty());
    }

    // §12.1-4
    #[test]
    fn spec_for_mapping() {
        let last = spawn("powershell", "C:\\Sticky");
        let taken: Vec<&str> = vec![];

        // Shell row uses last_spawn's cwd.
        let shell = Candidate::new(
            CandKind::Shell(degraded_shells()[0].clone()),
            "PowerShell".into(),
            String::new(),
        );
        let (nt, sp) = spec_for(&shell, &last, None, &taken).unwrap();
        assert_eq!(nt.cwd, PathBuf::from("C:\\Sticky"));
        assert_eq!(nt.program, "powershell.exe");
        assert!(matches!(nt.kind, TermKind::Shell));
        assert!(!nt.already_launched);
        assert_eq!(nt.name, "Shell · Sticky");
        assert_eq!(sp.kind_tag, "powershell");

        // Dir row uses the row's dir + last_spawn's shell.
        let dir = Candidate::new(
            CandKind::RecentDir { cwd: PathBuf::from("C:\\Elsewhere") },
            "C:\\Elsewhere".into(),
            String::new(),
        );
        let (nt, sp) = spec_for(&dir, &last, None, &taken).unwrap();
        assert_eq!(nt.cwd, PathBuf::from("C:\\Elsewhere"));
        assert!(matches!(nt.kind, TermKind::Shell), "last_spawn's shell kind");
        assert_eq!(sp.cwd, PathBuf::from("C:\\Elsewhere"));

        // Claude session ⇒ already_launched + the session's id.
        let sid = Uuid::new_v4();
        let cs = Candidate::new(
            CandKind::ClaudeSession {
                session_id: sid,
                cwd: PathBuf::from("C:\\Proj"),
                preview: "fix the bug in the overlay code before the deadline".into(),
                project: "proj".into(),
            },
            "fix the bug".into(),
            String::new(),
        );
        let (nt, _) = spec_for(&cs, &last, Some(Uuid::nil()), &taken).unwrap();
        assert!(nt.already_launched);
        assert_eq!(nt.folder, Some(Uuid::nil()));
        match nt.kind {
            TermKind::Claude { session_id, .. } => assert_eq!(session_id, sid),
            _ => panic!("claude kind"),
        }
        assert_eq!(nt.name.chars().count(), 28, "import name = preview take(28)");

        // Unknown kind_tag ⇒ None (refuse-over-guess). "wsl:<distro>" became
        // known in P6a, "ssh:<host>" in P6c.
        let unk = Candidate::new(
            CandKind::Suggestion { spec: spawn("zsh:foo", "C:\\X") },
            "foo".into(),
            String::new(),
        );
        assert!(spec_for(&unk, &last, None, &taken).is_none());

        // An ssh SHELL row (config host) spawns with an EMPTY cwd regardless
        // of last_spawn's directory.
        let sshrow = Candidate::new(
            CandKind::Shell(ShellChoice {
                kind_tag: "ssh:devbox".into(),
                label: "devbox (ssh)".into(),
                detail: "~/.ssh/config".into(),
            }),
            "devbox (ssh)".into(),
            String::new(),
        );
        let (nt, sp) = spec_for(&sshrow, &last, None, &taken).unwrap();
        assert_eq!(nt.cwd, PathBuf::new());
        assert_eq!(sp.cwd, PathBuf::new());
        assert_eq!(nt.args, vec!["devbox".to_string()]);

        // Custom/SshTo rows never spec directly (their expansions own it).
        let custom = Candidate::new(CandKind::Custom, "Custom command\u{2026}".into(), String::new());
        assert!(spec_for(&custom, &last, None, &taken).is_none());
        let sshto = Candidate::new(CandKind::SshTo, "ssh to\u{2026}".into(), String::new());
        assert!(spec_for(&sshto, &last, None, &taken).is_none());
    }

    /// THE 2026-07-04 regression: create an ssh terminal (its SpawnSpec
    /// carries an EMPTY cwd by design), then click a WSL/shell row — the row
    /// inherited last_spawn's empty cwd verbatim, the daemon passed it to
    /// `wsl --cd ""`, and wsl.exe died with Wsl/E_INVALIDARG. Every consumer
    /// of an inherited/persisted cwd now heals empty → home.
    #[test]
    fn empty_ssh_cwd_never_inherited() {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("C:\\"));
        // last_spawn as recorded after an ssh create: empty cwd.
        let last = spawn("ssh:192.0.2.14", "");
        assert_eq!(last.cwd, PathBuf::new(), "precondition: ssh spec cwd empty");

        // WSL shell row right after: the LINUX home (v0.1.1 — `~`, resolved
        // in-distro by wsl.exe), never the Windows home posing as a Linux
        // dir, and never the empty ssh cwd.
        let wslrow = Candidate::new(
            CandKind::Shell(ShellChoice {
                kind_tag: "wsl:Ubuntu-24.04".into(),
                label: "Ubuntu-24.04".into(),
                detail: "WSL".into(),
            }),
            "Ubuntu-24.04".into(),
            String::new(),
        );
        let (nt, sp) = spec_for(&wslrow, &last, None, &[]).unwrap();
        assert_eq!(nt.cwd, PathBuf::from("~"), "WSL rows default to the Linux home");
        assert_eq!(sp.cwd, PathBuf::from("~"), "recorded spec matches (recents dedupe on ~)");
        assert_eq!(nt.name, "Ubuntu-24.04 · ~", "honest auto-name");

        // PowerShell row: same heal (empty cwd would otherwise spawn the
        // shell in the daemon's own directory).
        let psrow = Candidate::new(
            CandKind::Shell(ShellChoice {
                kind_tag: "powershell".into(),
                label: "PowerShell".into(),
                detail: String::new(),
            }),
            "PowerShell".into(),
            String::new(),
        );
        let (nt, sp) = spec_for(&psrow, &last, None, &[]).unwrap();
        assert_eq!(nt.cwd, home);
        assert_eq!(sp.cwd, home);

        // New-Claude row: same heal.
        let claude = Candidate::new(CandKind::ClaudeNew, "New Claude session".into(), String::new());
        let (nt, sp) = spec_for(&claude, &last, None, &[]).unwrap();
        assert_eq!(nt.cwd, home);
        assert_eq!(sp.cwd, home);

        // Instant-create / suggestion replay of a POISONED persisted wsl spec
        // (recorded while the bug was live): spawns in the LINUX home
        // (v0.1.1), and activating it re-records the healed cwd so the ring
        // converges. An explicit dir still passes through verbatim.
        let poisoned = spawn("wsl:Ubuntu-24.04", "");
        let nt = spec_from_spawn(&poisoned, None, &[]).unwrap();
        assert_eq!(nt.cwd, PathBuf::from("~"));
        let sugg = Candidate::new(
            CandKind::Suggestion { spec: poisoned },
            "Ubuntu-24.04 (WSL)".into(),
            String::new(),
        );
        let (_, sp) = spec_for(&sugg, &last, None, &[]).unwrap();
        assert_eq!(sp.cwd, PathBuf::from("~"), "suggestion re-record heals the ring");
        let explicit = spawn("wsl:Ubuntu-24.04", "C:\\proj");
        let nt = spec_from_spawn(&explicit, None, &[]).unwrap();
        assert_eq!(
            nt.cwd,
            PathBuf::from("C:\\proj"),
            "explicit Windows dirs keep riding /mnt (a feature, not a heal target)"
        );

        // ssh rows themselves keep their by-design empty cwd (the daemon
        // never --cd's an ssh spawn; recents dedupe by (kind_tag, "")).
        let sshrow = Candidate::new(
            CandKind::Shell(ShellChoice {
                kind_tag: "ssh:devbox".into(),
                label: "devbox (ssh)".into(),
                detail: String::new(),
            }),
            "devbox (ssh)".into(),
            String::new(),
        );
        let (nt, sp) = spec_for(&sshrow, &last, None, &[]).unwrap();
        assert_eq!(nt.cwd, PathBuf::new());
        assert_eq!(sp.cwd, PathBuf::new());

        // Header honesty for any straggler empty-dir spec.
        assert_eq!(auto_name("wsl:Ubuntu-24.04", Path::new("")), "Ubuntu-24.04");
        assert_eq!(auto_name("powershell", Path::new("")), "Shell");
        assert_eq!(auto_name("claude", Path::new("")), "Claude");
    }

    #[test]
    fn spec_for_uniquifies_against_taken() {
        let last = spawn("powershell", "C:\\Sticky");
        let shell = Candidate::new(
            CandKind::Shell(degraded_shells()[0].clone()),
            "PowerShell".into(),
            String::new(),
        );
        let (nt, _) = spec_for(&shell, &last, None, &["Shell · Sticky"]).unwrap();
        assert_eq!(nt.name, "Shell · Sticky 2");
    }

    #[test]
    fn custom_spec_tokenizes() {
        let (nt, sp) = custom_spec(
            "ping",
            "-t 127.0.0.1",
            Path::new("C:\\Here"),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(nt.program, "ping");
        assert_eq!(nt.args, vec!["-t", "127.0.0.1"]);
        assert!(matches!(nt.kind, TermKind::Custom));
        assert_eq!(nt.name, "Here");
        assert_eq!(sp.kind_tag, "custom");
        assert!(custom_spec("", "", Path::new("C:\\"), None, &[]).is_none());
        // Quoted program with spaces stays one token.
        let (nt, _) = custom_spec(
            "\"C:\\Program Files\\x\\tool.exe\" run",
            "",
            Path::new("C:\\Here"),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(nt.program, "C:\\Program Files\\x\\tool.exe");
        assert_eq!(nt.args, vec!["run"]);
    }

    // §12.1-6
    #[test]
    fn pending_resolution() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        // Exact-name match; newest order wins on a name collision.
        let newly = vec![(a, "Shell · dir", 5i64), (b, "Shell · dir", 9i64)];
        assert_eq!(resolve_pending("Shell · dir", &newly), Some(b));
        assert_eq!(resolve_pending("Other", &newly), None);
        assert!(resolve_pending("x", &[]).is_none());
        // Expiry window (the App checks elapsed() against this constant).
        assert_eq!(PENDING_EXPIRY, Duration::from_secs(5));
    }

    // §12.1-7: key consumption order truth table.
    #[test]
    fn key_plan_truth_table() {
        // Esc peels: folder menu > custom expansion > ssh expansion > close.
        assert_eq!(esc_act(true, true, false), EscAct::CloseFolderMenu);
        assert_eq!(esc_act(true, false, true), EscAct::CloseFolderMenu);
        assert_eq!(esc_act(false, true, false), EscAct::CollapseCustom);
        assert_eq!(esc_act(false, false, true), EscAct::CollapseSsh);
        assert_eq!(esc_act(false, false, false), EscAct::Close);
        // Enter creates from the open expansion's fields, else activates.
        assert_eq!(enter_act(true, false), EnterAct::CreateCustom);
        assert_eq!(enter_act(false, true), EnterAct::CreateSsh);
        assert_eq!(enter_act(false, false), EnterAct::ActivateSel);
    }

    #[test]
    fn pathish_queries() {
        assert!(query_is_pathish("C:\\Terminal Control"));
        assert!(query_is_pathish("d:\\x"));
        assert!(query_is_pathish("\\\\server\\share"));
        assert!(!query_is_pathish("terminal"));
        assert!(!query_is_pathish("C:"));
        assert!(!query_is_pathish("C:relative"));
    }

    #[test]
    fn default_spawn_seeds() {
        // Legacy last_cwd seeds the first spawn's cwd when customized.
        assert_eq!(default_spawn("C:\\Work").cwd, PathBuf::from("C:\\Work"));
        // The untouched default falls back to home.
        let d = default_spawn("C:\\");
        assert_eq!(d.kind_tag, "powershell");
        assert_ne!(d.cwd, PathBuf::from("C:\\"));
    }
}
