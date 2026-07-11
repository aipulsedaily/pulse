//! The GUI shell: folder sidebar + terminal area, all state served by the daemon.

mod bindings;
/// Public within the crate: the gate-replay probe drives `ComposerState` +
/// `gate()` against real session bytes (pure logic, GUI-free).
pub mod complete;
pub mod composer;
/// QOL §4: pure drop translation/quoting/routing. ssh-drop (#26) shares its
/// `bash_single_quote` and replaces the one Ssh arm in `route_file_drop`.
pub mod drop;
mod glyph_cache;
mod highlight;
/// Public within the crate: the history_cross_session probe feeds captured
/// Blocks lists through the same build_index/filter the popup uses.
pub mod history;
mod import;
/// Public within the crate: the launcher_claude_cwd probe composes its
/// Claude-kind create through the REAL `claude_dir_spec` path.
pub mod launcher;
pub mod shells;
/// ssh-drop (#26): sftp transport, upload queue/workers, and the pure
/// argv/parser/classifier half (golden-tested in-module).
pub mod ssh_drop;
pub mod ipc;
pub mod term_backend;
mod term_view;
mod theme;
/// The app's first toast surface (#26; the #25 attention toast reuses it).
pub mod toast;

// Zero-behavior splits of this file (round-3 cleanup): same `gui` scope,
// `pub(super)` items, glob-reimported below so sibling modules and tests keep
// their `super::` paths.
mod central;
mod consent;
mod icons;
/// #34 lifecycle chrome: branded updating/uninstall helper windows (run
/// standalone via `--updating-ui`/`--uninstall-ui`) + the first-run card.
pub mod lifecycle_ui;
mod modals;
mod settings;
mod sidebar;
/// #34: Velopack update engine + sidebar update surface + backups.
mod update;

use consent::*;
use icons::*;
use modals::*;
use settings::*;
use update::*;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::TermMode;
use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Id, Layout, Margin, Pos2,
    Rect, RichText, Sense, Stroke, StrokeKind, UiBuilder, Vec2,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::{C2D, D2C};
use crate::state::{
    presented_status, shell_family, BlockRec, NewTerminal, PresentedStatus, SharedState,
    ShellFamily, TermKind, TermStatus, TerminalMeta,
};
use bindings::BindingsLayout;
use composer::{ComposerMode, ComposerState};
use ipc::IpcClient;
use launcher::{LauncherState, SpawnSpec};
use term_backend::{GridSize, TermBackend};

// ─────────────────── Warp design tokens (D4–D12) ───────────────────
// Elevation is expressed as surface steps, not shadows (D1). One accent (D2).
// Hover/selection are translucent overlays; borders are hairline (D3).

// Surfaces (D4)
const BG: Color32 = Color32::from_rgb(0x0B, 0x0D, 0x12); // app root
const BG_SIDEBAR: Color32 = Color32::from_rgb(0x0D, 0x0F, 0x15);
/// Sidebar boundary (user: "we need divider for sidebar maybe slight color
/// shift"): a clearly lifted sidebar surface so the sidebar-vs-terminal-area
/// boundary reads as two surfaces meeting — no line (candidate A, chosen over
/// the staged 1px edge-fill variant).
const BG_SIDEBAR_LIFT: Color32 = Color32::from_rgb(0x12, 0x15, 0x1D);
const SURFACE: Color32 = Color32::from_rgb(0x14, 0x17, 0x1F); // header/card/modal base
const SURFACE_2: Color32 = Color32::from_rgb(0x1B, 0x1F, 0x2A); // hover/input/selected
const SURFACE_3: Color32 = Color32::from_rgb(0x22, 0x26, 0x34); // modal fill/pressed
/// One step above SURFACE_3: the hover fill for anything RESTING on
/// SURFACE_3 (egui-native menu items, buttons in modals). The doctrine
/// de-stroke pass left those hovers painting SURFACE_3-on-SURFACE_3 —
/// invisible; hover must always read as a fill shift (never a stroke).
const SURFACE_4: Color32 = Color32::from_rgb(0x2C, 0x31, 0x41);

// Overlays (D5)
const OV_HOVER: Color32 = Color32::from_rgba_premultiplied(10, 10, 10, 10);
const OV_PRESSED: Color32 = Color32::from_rgba_premultiplied(18, 18, 18, 18);

// ── Merged top bar (task #21) ──
/// One slim strip carries window chrome + the old terminal header.
/// 36px keeps standard caption-button hit targets on a frameless window.
const TITLEBAR_H: f32 = 36.0;
/// Pixels the inline scrollback-search cluster consumes when open (field +
/// count + 3 icon buttons); pre-reserved out of the name/cwd text budget.
const SEARCH_CLUSTER_W: f32 = 330.0;
/// Drag-region goal: the free middle of the bar must stay a generous drag
/// handle (frameless window — dragging is load-bearing).
const DRAG_FRACTION: f32 = 0.40;
/// Hard drag floor below typical window widths.
const MIN_DRAG_PX: f32 = 120.0;
/// The terminal name never ellipsizes below this (readability floor).
const MIN_NAME_PX: f32 = 90.0;
/// Gap between the name and the dimmed cwd.
const NAME_CWD_GAP: f32 = 8.0;

// Borders (D6)
// BORDER/BORDER_STRONG deleted in the doctrine stroke sweep (2026-07-03):
// no hairlines/borders anywhere — structure is spacing, background shifts,
// and shadows.

// Text (D7) — never pure white.
const TEXT: Color32 = Color32::from_rgb(0xE7, 0xE9, 0xEF);
const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xA9, 0xAF, 0xC0);
const TEXT_MUTED: Color32 = Color32::from_rgb(0x6B, 0x71, 0x85);
const TEXT_FAINT: Color32 = Color32::from_rgb(0x4A, 0x4F, 0x60);

// Accent (D8)
const ACCENT: Color32 = Color32::from_rgb(0x7C, 0x83, 0xFF);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x90, 0x96, 0xFF);
const ACCENT_PRESSED: Color32 = Color32::from_rgb(0x66, 0x6D, 0xF0);
const ON_ACCENT: Color32 = Color32::from_rgb(0x0B, 0x0D, 0x12);
const ACCENT_SUBTLE: Color32 = Color32::from_rgba_premultiplied(15, 15, 30, 30);

// Status (D9/D10)
const SUCCESS: Color32 = Color32::from_rgb(0x4A, 0xDE, 0x80);
const DANGER: Color32 = Color32::from_rgb(0xFF, 0x5C, 0x6C);
const DANGER_HOVER: Color32 = Color32::from_rgb(0xFF, 0x74, 0x82);

// Terminal surface (D12).
const TERM_BG: Color32 = Color32::from_rgb(0x0C, 0x0E, 0x13);

// Attention amber (V-A / V-D): the single non-token colour, for NeedsYou.
const ATTENTION: Color32 = Color32::from_rgb(0xF5, 0xA6, 0x23);

// Named for a few kept call sites.
const RED: Color32 = DANGER;

/// Curated row color tags (task #22): 8 hues designed against the dark
/// theme — the status hues (DANGER red / SUCCESS green / ACCENT indigo /
/// muted gold from the terminal palette) plus four extras picked to read at
/// 3px-bar size on BG_SIDEBAR_LIFT. The orange is deliberately dimmer and
/// browner than the ATTENTION amber signal so a tag never reads as
/// NeedsYou. Index = the persisted `color_tag` value; the label names the
/// context-menu row.
const TAG_COLORS: [(Color32, &str); 8] = [
    (Color32::from_rgb(0xFF, 0x5C, 0x6C), "Red"),
    (Color32::from_rgb(0xE0, 0x82, 0x4C), "Orange"),
    (Color32::from_rgb(0xE5, 0xC0, 0x7B), "Gold"),
    (Color32::from_rgb(0x4A, 0xDE, 0x80), "Green"),
    (Color32::from_rgb(0x4F, 0xD1, 0xC5), "Teal"),
    (Color32::from_rgb(0x7C, 0x83, 0xFF), "Indigo"),
    (Color32::from_rgb(0xB7, 0x8A, 0xF7), "Violet"),
    (Color32::from_rgb(0xF2, 0x72, 0xB6), "Rose"),
];

/// The swatch for a persisted tag. Out-of-range indices (a future table
/// growth read by an older build) render as untagged, never panic.
fn tag_color(tag: Option<u8>) -> Option<Color32> {
    tag.and_then(|i| TAG_COLORS.get(i as usize)).map(|&(c, _)| c)
}

/// Premultiplied src-over compositing (exactly what rect_filled does), for
/// computing the EFFECTIVE fill under the moon glyph's bite circle. `bg` is
/// assumed opaque (every surface token is).
fn composite_over(bg: Color32, fg: Color32) -> Color32 {
    let inv = 255 - fg.a() as u32;
    Color32::from_rgba_premultiplied(
        (fg.r() as u32 + bg.r() as u32 * inv / 255).min(255) as u8,
        (fg.g() as u32 + bg.g() as u32 * inv / 255).min(255) as u8,
        (fg.b() as u32 + bg.b() as u32 * inv / 255).min(255) as u8,
        255,
    )
}

/// SLEEP S14: the crescent-moon glyph for Asleep/Sleeping — PAINTER-drawn
/// (a font ☾ risks glyph-atlas fallback holes): a filled circle with a
/// `bg`-colored bite circle offset toward the upper right. `bg` must be the
/// effective fill the glyph sits on (hover-lerped row fill, card fill, …) or
/// the bite reads as a smudge. Distinct at a glance from BOTH the Dead
/// hollow ring and every filled status dot (DO-NOT 10).
pub(crate) fn draw_moon(painter: &egui::Painter, c: Pos2, r: f32, color: Color32, bg: Color32) {
    painter.circle_filled(c, r, color);
    painter.circle_filled(c + Vec2::new(r * 0.55, -r * 0.35), r * 0.85, bg);
}

/// Live-output window that reads as "Working" for every terminal (V-A).
const WORKING_WINDOW: Duration = Duration::from_millis(800);
/// task #22 CLI attention: a CLI terminal (claude-kind, or a shell running a
/// tracked inner CLI) whose stream has been quiet this long is DONE, not
/// paused — the dot latches NeedsYou (amber) until viewed. 3s rides out
/// claude's inter-tool-call pauses (the 1–2s class); shorter thresholds flap
/// mid-response. Until the latch lands the dot keeps its Working pulse (the
/// bridge in `derive_activity`) so a pause never reads as a gray flicker.
const CLI_ATTENTION_QUIET: Duration = Duration::from_secs(3);
/// Output within this window after attach/restore never arms the CLI
/// attention episode: attach-resize conhost repaints and respawn banners
/// would otherwise light every reopened CLI terminal amber at boot.
const CLI_ATTACH_SUPPRESS: Duration = Duration::from_secs(5);
/// Unread ack machine: a live-output episode on an unselected terminal only
/// QUEUES an unread check; the verdict runs once the stream has been quiet
/// this long, by comparing the grid against the acked baseline
/// (`unread_should_mark`). Claude Code's idle status-row chrome (updater
/// "Checking for updates" paint/erase, statusline timer digit ticks) repaints
/// well inside this window, so a whole chrome burst is judged as ONE settled
/// state — which restores the acked text and never marks.
const UNREAD_SETTLE: Duration = Duration::from_secs(2);
/// Rows of changed text required for a settled episode to mark unread.
/// Status chrome (Claude's updater line, prompt-refreshers, progress bars)
/// rewrites ONE row in place; user-relevant output lands on ≥2 rows (output
/// line(s) + the next prompt) or scrolls rows into history (which marks via
/// `GridDigest::history` growth regardless of this floor).
const UNREAD_MIN_ROWS: usize = 2;
/// SLEEP S7 (GUI mirror of the daemon's SLEEP_QUIET_MS): output within this
/// window is busy evidence — the Sleep click gets a confirm modal instead
/// of firing. Quiet alt-screen never gates (the idle claude REPL is the
/// headline sleep target).
const SLEEP_QUIET_WINDOW: Duration = Duration::from_secs(3);

/// What the GUI-side sleep gate found (S8): the modal names it.
enum SleepEvidence {
    /// An open block — the command line, shown in the confirm copy.
    OpenBlock(String),
    /// No open block, but output within SLEEP_QUIET_WINDOW.
    OutputFlowing,
}

/// SLEEP §7.1: the per-presented-status lifecycle context-menu item, pure so
/// the table is unit-pinned. None = no lifecycle action (the Sleeping
/// transient — the drain resolves in under a second; offering Wake there
/// would race the kill). Running additionally keeps its "Kill process" row
/// (built at the call site — kill and sleep are different intents).
fn lifecycle_menu_label(p: PresentedStatus) -> Option<&'static str> {
    match p {
        PresentedStatus::Running => Some("Sleep"),
        PresentedStatus::Asleep => Some("Wake"),
        PresentedStatus::Dead => Some("Restore"),
        PresentedStatus::Sleeping => None,
    }
}

/// QOL §3.2: enabled-state table for the grid context menu (pure, tested).
/// Items DIM rather than vanish (R5); Copy's gate (a live selection) is read
/// at render time. Asleep/Sleeping rows take the dead-row column — Paste and
/// Rerun dim so no input path can wake implicitly (sleep inv. 5); Clear stays
/// enabled (view state works on a frozen grid).
#[derive(Debug, PartialEq, Eq)]
struct MenuGates {
    paste: bool,
    open_cwd: bool,
    rerun: bool,
    clear: bool,
}

fn menu_gates(
    presented: PresentedStatus,
    can_rerun: bool,
    has_last_closed: bool,
    has_local_cwd: bool,
    history: usize,
) -> MenuGates {
    let running = presented == PresentedStatus::Running;
    MenuGates {
        paste: running,
        open_cwd: has_local_cwd,
        rerun: running && can_rerun && has_last_closed,
        clear: history > 0,
    }
}

/// QOL §3.3: the local directory a terminal's cwd maps to (pure, tested).
/// Win-namespace shells/CLIs: live_cwd else the persisted cwd. WSL: posix
/// `/mnt/<drive>/…` translates back to the drive form (nicer than UNC);
/// anything else in a NAMED distro becomes `\\wsl.localhost\<distro>\…`
/// (Explorer opens WSL UNC natively); a default-distro terminal has no name
/// to build the UNC with ⇒ None. Ssh: None — no local directory exists.
fn local_cwd_for(
    family: &ShellFamily,
    live_cwd: Option<&std::path::Path>,
    meta_cwd: &std::path::Path,
) -> Option<PathBuf> {
    fn is_win_shaped(s: &str) -> bool {
        let b = s.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    }
    // Shared with Tab completion (#24): ONE posix→local mapping.
    use complete::posix_to_local;
    match family {
        ShellFamily::Ssh { .. } => None,
        ShellFamily::WslShell { distro } => {
            let cwd = live_cwd.unwrap_or(meta_cwd);
            let s = cwd.to_str()?;
            if is_win_shaped(s) {
                // The pre-first-cd persisted cwd may still be Windows-shaped.
                return Some(cwd.to_path_buf());
            }
            posix_to_local(s, distro.as_deref())
        }
        _ => Some(
            live_cwd
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| meta_cwd.to_path_buf()),
        ),
    }
}

/// QOL §7.1: the Duplicate spec (pure, tested). Claude terminals mint a
/// FRESH session id — NEVER the pinned one (two terminals resuming one id is
/// the wrong-session corruption class the tracker refuses to guess about,
/// DO-NOT 10). cwd = where the terminal is NOW (live_cwd, posix verbatim for
/// WSL — launch `--cd` accepts it); ssh duplicates land in the remote $HOME
/// (empty cwd, the launcher's own ssh convention).
fn duplicate_spec(t: &TerminalMeta, taken: &[&str]) -> NewTerminal {
    let kind = match &t.kind {
        TermKind::Claude { extra_args, .. } => TermKind::Claude {
            session_id: Uuid::new_v4(),
            extra_args: extra_args.clone(),
        },
        k => k.clone(),
    };
    let cwd = match shell_family(&t.kind, &t.program, &t.args) {
        ShellFamily::Ssh { .. } => PathBuf::new(),
        _ => t.live_cwd.clone().unwrap_or_else(|| t.cwd.clone()),
    };
    NewTerminal {
        name: launcher::uniquify_name(&t.name, taken),
        folder: t.folder,
        kind,
        program: t.program.clone(),
        args: t.args.clone(),
        cwd,
        already_launched: false,
        shell_cfg: t.shell_cfg.clone(),
    }
}

/// QOL §6.1: read CF_UNICODETEXT off the Win32 clipboard. egui 0.35 only
/// surfaces the clipboard as Paste EVENTS (Ctrl+V), so the menu Paste row
/// and middle-click paste need their own read — windows crate only, no new
/// dependency (the Q3 bar: never add a clipboard crate for this).
fn clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    unsafe {
        OpenClipboard(None).ok()?;
        let mut text = None;
        if let Ok(h) = GetClipboardData(CF_UNICODETEXT) {
            let hg = HGLOBAL(h.0);
            let p = GlobalLock(hg) as *const u16;
            if !p.is_null() {
                let cap = GlobalSize(hg) / 2;
                let mut len = 0usize;
                while len < cap && *p.add(len) != 0 {
                    len += 1;
                }
                text = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
                let _ = GlobalUnlock(hg);
            }
        }
        let _ = CloseClipboard();
        text.filter(|s| !s.is_empty())
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Prefs {
    font_size: f32,
    /// Legacy form-modal directory. Kept only to seed the first
    /// `last_spawn`'s cwd (selector spec §9); nothing writes it anymore.
    last_cwd: String,
    /// Sidebar row density: false = comfortable two-line rows (default), true =
    /// compact single-line rows (V-B).
    #[serde(default)]
    compact: bool,
    /// Sidebar collapsed to a slim status-dot rail.
    #[serde(default)]
    sidebar_collapsed: bool,
    /// Sticky instant-create choice: what the titlebar + spawns (§3.1).
    /// Overwritten by every successful create, instant or launcher.
    #[serde(default)]
    last_spawn: Option<SpawnSpec>,
    /// MRU ring of 8 distinct (kind_tag, cwd) spawn combos — the launcher's
    /// Suggested section (§4.3/§10).
    #[serde(default)]
    recent_spawns: Vec<SpawnSpec>,
    /// QOL §6.2: copy the selection at every selection-commit edge (default
    /// OFF — copy stays an explicit act unless opted in). Visible entry
    /// point = the grid context menu's toggle row.
    #[serde(default)]
    copy_on_select: bool,
    /// QOL §5: the raw-path paste-safety gate. Default ON; the ConfirmPaste
    /// modal's "Don't warn again" clears it.
    #[serde(default = "default_true_pref")]
    paste_warn: bool,
    /// ssh-drop §4 (T12): "Never show this again" on the upload consent
    /// dialog — GLOBAL (the dialog teaches one semantic; every progress
    /// toast restates the host anyway).
    #[serde(default)]
    ssh_drop_skip_consent: bool,
    /// Attribution Layer 3: per-host verdicts for the remote claude-session
    /// tracker hook (key = the ssh destination string verbatim; true =
    /// install, false = never ask again for this host).
    #[serde(default)]
    claude_hook_hosts: std::collections::BTreeMap<String, bool>,
    /// The "Always — don't show again" checkbox: the SAME answer applied to
    /// every future host (Some(true)=always install, Some(false)=never ask;
    /// None=ask per host). Per-host verdicts already recorded keep priority.
    #[serde(default)]
    claude_hook_all: Option<bool>,
    /// Codex attribution (task #30): the Windows-native ~/.codex install
    /// consent — Some(true)=install+heal idempotently, Some(false)=never,
    /// None=ask on the first local codex use. R4-F6: this covers ONLY the
    /// Windows lane; WSL distros have their own verdicts below (the consent
    /// dialog's writes must match its label exactly).
    #[serde(default)]
    codex_hook_local: Option<bool>,
    /// R4-F6: per-WSL-distro codex-hook verdicts (key = resolved distro
    /// name), asked on that distro's first codex use.
    #[serde(default)]
    codex_hook_wsl_distros: std::collections::BTreeMap<String, bool>,
    /// R4-F6: the all-WSL verdict — set ONLY by the consent checkbox
    /// ("enable for WSL distros too" on the Windows ask, "apply to all WSL
    /// distros" on a WSL ask). Per-distro verdicts keep priority.
    #[serde(default)]
    codex_hook_wsl: Option<bool>,
    /// Codex attribution: per-host verdicts for the remote codex-session
    /// beacon (ssh), same shape as `claude_hook_hosts`.
    #[serde(default)]
    codex_hook_hosts: std::collections::BTreeMap<String, bool>,
    /// Codex attribution: the ssh "Always" checkbox (mirrors claude_hook_all).
    #[serde(default)]
    codex_hook_all: Option<bool>,
    /// r2-M2: per-terminal scrollback depth (grid lines). The GUI's dominant
    /// memory knob — a saturated 158-col grid costs ~3.9KB/line — applied
    /// when a backend is (re)constructed (attach/Reset), not retroactively.
    /// UI entry point: the settings page's Terminal section (#33).
    #[serde(default = "default_scrollback_lines")]
    scrollback_lines: usize,
    /// Updates (#33 settings / #34 velopack): background update checks
    /// (GUI boot + periodic). The stub build ignores it; #34 consumes it.
    #[serde(default = "default_true_pref")]
    update_auto_check: bool,
    /// Updates: silently download+stage once a check finds one (updates
    /// still apply only on an explicit restart).
    #[serde(default = "default_true_pref")]
    update_auto_download: bool,
    /// Updates: "Skip this version" — this exact version is never offered;
    /// the checker clears it when a newer version appears (#34).
    #[serde(default)]
    update_skip_version: Option<String>,
    /// Updates: default state of the popover's "back up layout first"
    /// checkbox (and the settings toggle that seeds it).
    #[serde(default = "default_true_pref")]
    update_backup_default: bool,
    /// #34 Axis 7: the version this GUI last booted as. A boot where it
    /// differs from CARGO_PKG_VERSION is a post-update boot → one quiet
    /// "Updated to vX" toast + the 15s daemon health check. None = the
    /// pre-#34 era (or first run) — no toast.
    #[serde(default)]
    last_run_version: Option<String>,
    /// v0.1.2: show the distro welcome message (motd) on every NEW WSL
    /// terminal (default ON). Stamped into `ShellCfg.wsl_motd` at create
    /// time; the daemon's fresh-spawn prelude prints the motd without ever
    /// touching the distro's once-a-day `~/.motd_shown` stamp, and restores
    /// never show nor consume it. Settings → Terminal owns the toggle.
    #[serde(default = "default_true_pref")]
    wsl_welcome_banner: bool,
}

fn default_true_pref() -> bool {
    true
}

fn default_scrollback_lines() -> usize {
    10_000
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            font_size: 13.0,
            last_cwd: "C:\\".into(),
            compact: false,
            sidebar_collapsed: false,
            last_spawn: None,
            recent_spawns: Vec::new(),
            copy_on_select: false,
            paste_warn: true,
            ssh_drop_skip_consent: false,
            claude_hook_hosts: std::collections::BTreeMap::new(),
            claude_hook_all: None,
            codex_hook_local: None,
            codex_hook_wsl_distros: std::collections::BTreeMap::new(),
            codex_hook_wsl: None,
            codex_hook_hosts: std::collections::BTreeMap::new(),
            codex_hook_all: None,
            scrollback_lines: default_scrollback_lines(),
            update_auto_check: true,
            update_auto_download: true,
            update_skip_version: None,
            update_backup_default: true,
            last_run_version: None,
            wsl_welcome_banner: true,
        }
    }
}

/// See `App::sidebar_rows`: the sidebar's presentation order, cached per
/// Snapshot generation. `groups[i]` holds `folders[i]`'s members.
#[derive(Default)]
struct SidebarRows {
    /// The `state_gen` this was built from (0 = never built).
    gen: u64,
    folders: Vec<crate::state::Folder>,
    groups: Vec<Vec<crate::state::TerminalMeta>>,
    /// Folderless (or dangling-folder) terminals.
    loose: Vec<crate::state::TerminalMeta>,
}

impl SidebarRows {
    /// Every terminal in sidebar presentation order (grouped, then loose).
    fn iter(&self) -> impl Iterator<Item = &crate::state::TerminalMeta> {
        self.groups.iter().flatten().chain(self.loose.iter())
    }
}

/// Pure builder for the sidebar row cache. Sort keys are `order` ALONE (D6:
/// the NeedsYou signal stays amber dot/bar/badge/pill, never a row jump) —
/// presentation order is byte-identical to the per-frame sort this replaced.
fn build_sidebar_rows(state: &SharedState, gen: u64) -> SidebarRows {
    let mut folders = state.folders.clone();
    folders.sort_by_key(|f| f.order);
    let groups: Vec<Vec<crate::state::TerminalMeta>> = folders
        .iter()
        .map(|f| {
            let mut terms: Vec<_> = state
                .terminals
                .iter()
                .filter(|t| t.folder == Some(f.id))
                .cloned()
                .collect();
            terms.sort_by_key(|t| t.order);
            terms
        })
        .collect();
    let folder_ids: HashSet<Uuid> = folders.iter().map(|f| f.id).collect();
    let mut loose: Vec<_> = state
        .terminals
        .iter()
        .filter(|t| t.folder.is_none_or(|fid| !folder_ids.contains(&fid)))
        .cloned()
        .collect();
    loose.sort_by_key(|t| t.order);
    SidebarRows {
        gen,
        folders,
        groups,
        loose,
    }
}

/// See `App::previews`.
#[derive(Default)]
struct PreviewCache {
    key: Option<(u64, u16, u16, usize)>,
    text: String,
    /// Laid-out preview text (r4 perf-gui L2): `painter.text` re-CLONES and
    /// re-hashes the ~6-line String per card per frame (egui caches the
    /// galley, not the `impl ToString` copy). Rebuilt on key drift or a
    /// pixels_per_point change (f32 bits — galleys bake the ppp in).
    galley: Option<(u32, std::sync::Arc<egui::Galley>)>,
}

/// The dashboard preview cache key. `feed_gen` covers consumed bytes but
/// does NOT bump on `resize_to` — the grid dims must ride the key too, or a
/// resized-but-quiet card would keep painting the stale wrap. `max_chars`
/// covers the card's own width budget.
fn preview_key(backend: &TermBackend, max_chars: usize) -> (u64, u16, u16, usize) {
    (
        backend.feed_gen,
        backend.size.cols,
        backend.size.rows,
        max_chars,
    )
}

/// Derived per-terminal activity, recomputed every frame from `ActivityState`
/// plus the terminal's status (V-A).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Activity {
    /// Output within the last ~800ms.
    Working,
    /// Running but quiet.
    Idle,
    /// Bell rang or an interactive prompt is waiting; latched until viewed.
    NeedsYou,
    /// SLEEP §7.4: the user shelved this terminal (presented Asleep or the
    /// sub-second Sleeping transient). Renders as the moon glyph; never
    /// latches attention (S13); excluded from the attention pill's waiting
    /// set.
    Asleep,
    /// Process exited.
    Dead,
}

/// Mutable per-terminal signal bookkeeping, updated as output arrives and each
/// frame (V-A).
struct ActivityState {
    /// When the last live output chunk arrived (drives Working/Idle).
    last_output: Instant,
    /// Unread output bursts since the terminal was last viewed (badge count).
    bursts: u32,
    /// Latched NeedsYou flag: set by a bell or prompt signature, cleared only
    /// when the terminal is selected AND the window is focused.
    needs_you: bool,
    /// `TermBackend::feed_gen` at the last prompt-signature scan, plus the
    /// cached verdict. The scan is O(rows) grid work per terminal — gated on
    /// consumed bytes so 20 idle terminals cost nothing per typing frame
    /// (UX HIGH-3). Semantics unchanged: the cached verdict re-latches
    /// NeedsYou each frame exactly like the live scan did.
    scanned_gen: u64,
    prompt_sig: bool,
    /// task #22 CLI attention: a live streaming episode is in flight for a
    /// CLI-kind terminal. Armed by live Output (past the attach suppression
    /// window), consumed when the quiet latch fires or the terminal is
    /// viewed. Meaningless for plain shells (never read).
    cli_stream: bool,
    /// Output before this instant never arms `cli_stream` — the post-attach
    /// conhost repaint / respawn banner window (CLI_ATTACH_SUPPRESS).
    cli_suppress_until: Instant,
    /// Bug A attention ack: true ⇒ the next waiting signal (bell / prompt
    /// signature / CLI quiet) may latch NeedsYou. Viewing the terminal (or
    /// clicking the attention pill) DISARMS persistently — a still-painted
    /// permission prompt or an idle REPL repaint can never re-fire amber.
    /// Only user-authored PTY input re-arms (`rearm_attention`), plus
    /// session Reset and sleep/wake (both deliberate user acts). Starts
    /// armed so a genuinely-waiting prompt alerts once at boot.
    armed: bool,
    /// Unread ack machine (accent-dot cousin of Bug A): live output on an
    /// unselected terminal sets this instead of marking unread directly;
    /// once the stream settles (UNREAD_SETTLE) the grid is compared against
    /// `unread_base` and only a real text change marks. Invisible/chrome
    /// bytes (title-only OSCs, cursor churn, paint-then-erase status lines)
    /// therefore never light the dot.
    unread_pending: bool,
    /// Output arrived inside the attach/Reset suppression window (conhost
    /// attach-resize repaints, respawn banners): the next settle ADOPTS the
    /// grid as the new baseline without a verdict. Without this, the
    /// suppressed repaint silently drifts the grid away from the
    /// replay-seeded baseline and the FIRST real episode after boot would
    /// inherit that diff and false-mark.
    unread_adopt: bool,
    /// The acked visible-content baseline: what the user last saw (captured
    /// at the deselection edge, pw2-L1) or the last settled state already
    /// judged (refreshed after every settle verdict, marking or not — chrome
    /// drift like a ticking status timer never accumulates into a mark).
    /// None until the attach Replay seeds it; a None baseline ADOPTS the
    /// current grid without marking, so restore/Reset replay comes up quiet.
    unread_base: Option<term_backend::GridDigest>,
    /// `TermBackend::feed_gen` at the last baseline refresh — lets the
    /// deselection-edge capture skip the O(rows×cols) digest when nothing
    /// was consumed since the baseline was taken.
    unread_base_gen: u64,
}

impl ActivityState {
    fn new() -> Self {
        Self {
            // Backdate so a freshly-attached terminal isn't briefly "Working".
            last_output: Instant::now() - Duration::from_secs(10),
            bursts: 0,
            needs_you: false,
            // u64::MAX ⇒ first frame always scans (feed_gen starts at 0).
            scanned_gen: u64::MAX,
            prompt_sig: false,
            cli_stream: false,
            // Entries are created at attach (update_activity's first pass
            // over a fresh backend), so "now" is attach time.
            cli_suppress_until: Instant::now() + CLI_ATTACH_SUPPRESS,
            armed: true,
            unread_pending: false,
            unread_adopt: false,
            unread_base: None,
            unread_base_gen: u64::MAX,
        }
    }
}

/// Pure unread verdict (unit-tested): does the settled grid differ from the
/// acked baseline in a USER-RELEVANT way? Marks on scrollback growth (rows
/// scrolled off = new content the user hasn't seen) or on ≥ UNREAD_MIN_ROWS
/// rows of changed text. Never marks with no baseline (post-attach/Reset
/// adoption — boot comes up quiet) or across a resize (row-count mismatch:
/// reflow moves every hash without any new output; the caller adopts).
/// pw2-L1: re-base a terminal's unread baseline to its CURRENT grid — "the
/// user saw exactly this". Called synchronously at the deselection edge
/// (`select_terminal`), BEFORE `selected` is overwritten; see that call site
/// for why the capture must be same-frame (a next-frame edge capture bakes
/// one drain of never-painted bytes into the baseline and can permanently
/// miss an unread mark).
fn unread_rebase_on_deselect(st: &mut ActivityState, backend: &TermBackend) {
    // Gen match ⇒ nothing was consumed since the last capture — the
    // baseline already reflects this exact grid content, skip the digest.
    if st.unread_base.is_some() && st.unread_base_gen == backend.feed_gen {
        return;
    }
    st.unread_base = Some(backend.grid_digest());
    st.unread_base_gen = backend.feed_gen;
}

fn unread_should_mark(
    base: Option<&term_backend::GridDigest>,
    cur: &term_backend::GridDigest,
) -> bool {
    let Some(base) = base else { return false };
    if base.rows.len() != cur.rows.len() {
        return false;
    }
    if cur.history > base.history {
        return true;
    }
    let changed = base
        .rows
        .iter()
        .zip(&cur.rows)
        .filter(|(a, b)| a != b)
        .count();
    changed >= UNREAD_MIN_ROWS
}

/// Pure per-frame dot rule (task #22, unit-tested): status + signals →
/// Activity. `is_cli`/`cli_stream` add the CLI bridge — a streaming CLI
/// keeps the Working pulse through its short inter-tool-call pauses so the
/// dot never flaps gray before the attention latch fires at
/// CLI_ATTENTION_QUIET (the latch itself lives in `update_activity` and
/// arrives here as `needs_you`).
fn derive_activity(
    dead: bool,
    asleep: bool,
    needs_you: bool,
    quiet: Duration,
    is_cli: bool,
    cli_stream: bool,
) -> Activity {
    // SLEEP: the shelved presentation wins outright — a sleeping terminal
    // can't need you (S13 clears the latch and no output can arrive), and
    // Dead must not claim a merely-shelved terminal (DO-NOT 10).
    if asleep {
        return Activity::Asleep;
    }
    if dead {
        return Activity::Dead;
    }
    if needs_you {
        return Activity::NeedsYou;
    }
    if quiet < WORKING_WINDOW {
        return Activity::Working;
    }
    if is_cli && cli_stream && quiet < CLI_ATTENTION_QUIET {
        return Activity::Working;
    }
    Activity::Idle
}

/// The activity decision shared by `activity_from_meta` (meta in hand — the hot
/// sidebar/dashboard path) and `activity_of` (id-only lookup): read the
/// dot-relevant fields off the meta plus the O(1) signal-map entry, then defer
/// to `derive_activity`. A missing meta is neither dead, asleep, nor a CLI (the
/// id-only not-found fallback). Both callers route through here so the two paths
/// cannot drift — asserted in `activity_meta_matches_lookup`.
fn activity_for(meta: Option<&TerminalMeta>, sig: Option<&ActivityState>) -> Activity {
    let dead = meta.is_some_and(|t| t.status == TermStatus::Dead);
    let asleep = meta.is_some_and(|t| t.asleep);
    let is_cli = meta.is_some_and(|t| {
        matches!(t.kind, crate::state::TermKind::Claude { .. }) || t.inner_cli.is_some()
    });
    match sig {
        Some(s) => derive_activity(
            dead,
            asleep,
            s.needs_you,
            s.last_output.elapsed(),
            is_cli,
            s.cli_stream,
        ),
        None if asleep => Activity::Asleep,
        None if dead => Activity::Dead,
        None => Activity::Idle,
    }
}

/// Pure per-frame attention latch step (Bug A ack machine, unit-tested):
/// the three latch sources (bell, prompt signature, CLI quiet) fire only
/// while `st.armed`; viewing the terminal clears AND disarms, so nothing
/// re-latches until user input re-arms (`rearm_attention`). Loop killers:
/// a level-triggered `prompt_sig` that stays painted after the ack, and
/// idle REPL repaints re-arming `cli_stream`, are both dead ends while
/// disarmed. `prompt_sig` is passed (not read from `st`) so the caller's
/// scan-refresh stays visible at the call site.
fn step_attention(
    st: &mut ActivityState,
    bell: bool,
    prompt_sig: bool,
    is_cli: bool,
    quiet: Duration,
    viewing: bool,
) {
    if st.armed {
        if bell {
            st.needs_you = true;
        }
        if prompt_sig {
            st.needs_you = true;
        }
        // task #22: a CLI streaming episode that has gone quiet past the
        // threshold is DONE — latch NeedsYou (amber dot / left bar /
        // titlebar pill / taskbar flash, the whole existing signal path)
        // until the terminal is viewed. One latch per episode.
        if is_cli && st.cli_stream && quiet >= CLI_ATTENTION_QUIET {
            st.needs_you = true;
            st.cli_stream = false;
        }
    }
    if viewing {
        st.needs_you = false;
        st.bursts = 0;
        // Viewing consumes any in-flight CLI episode too — typing
        // (input) requires selection+focus, so a mid-stream glance
        // means completion never alerts unless input follows.
        st.cli_stream = false;
        // Persistent ack: amber stays off until the user sends input.
        st.armed = false;
    }
}

/// Inline scrollback search over the selected terminal (V4).
struct SearchState {
    query: String,
    regex: Option<RegexSearch>,
    matches: Vec<Match>,
    current: usize,
    /// history_size at the last recompute (F5c): stored match Points name
    /// grid lines, and rows rotating into history shift every line under
    /// them — a stale current-match would paint its strong highlight on
    /// arbitrary rows while output streams. Drift ⇒ debounced recompute;
    /// until it runs the current-match highlight is withheld.
    matches_history: usize,
    last_build: Instant,
    /// Last user-driven search interaction (query edit / match step): keys
    /// the adaptive drift debounce — engaged users get the snappy rebuild,
    /// pure output drift waits longer (the rebuild is a full-scrollback
    /// regex walk on the paint thread).
    last_user: Instant,
}

impl SearchState {
    fn new() -> Self {
        Self {
            query: String::new(),
            regex: None,
            matches: Vec::new(),
            current: 0,
            matches_history: 0,
            last_build: Instant::now(),
            last_user: Instant::now(),
        }
    }
}

/// One terminal's mirrored block records (P2).
#[derive(Default)]
struct BlockList {
    /// Sorted by start_off (the daemon appends in order; upserts never
    /// reorder). Binary-search upserts keep it that way.
    recs: Vec<BlockRec>,
    /// Latest epoch seen in any Blocks frame; > 0 ⇔ this terminal spawns
    /// hooked (launch() bumps epoch only for hooked spawns — including the
    /// CLI-restore pwsh wrapper whose TermKind stays Shell/Custom).
    epoch: u32,
    /// Cached failed-block count needs recompute.
    dirty: bool,
    failed: usize,
}

impl BlockList {
    /// Count of completed failures (exit Some(≠0)) for the header badge and
    /// the panel's failure navigation; cached, recomputed on change.
    fn failed_count(&mut self) -> usize {
        if self.dirty {
            self.failed = self
                .recs
                .iter()
                .filter(|r| r.end_off.is_some() && r.exit.is_some_and(|e| e != 0))
                .count();
            self.dirty = false;
        }
        self.failed
    }
}

/// The Re-run gate's record leg, pure so the truth table is unit-testable:
/// "no open block" IS cursor-at-prompt for hooked shells — every accepted
/// line opens a block (exec hook) and only the next prompt render closes it
/// (pre hook), so an open block covers both "command still running" and
/// "user launched a TUI from the prompt".
fn rerun_recs_ready(recs: &[BlockRec]) -> bool {
    !recs.is_empty() && recs.iter().all(|r| r.end_off.is_some())
}

/// Blocks recall panel state (P2, §6).
struct BlocksPanel {
    filter: String,
    failed_only: bool,
    /// Cached filtered record indices (newest first) + the key they were
    /// computed for: (filter, failed_only, blocks_stamp). Recomputed only on
    /// query/toggle change or a Blocks frame — never per frame, so an open
    /// panel stops lowercasing all 500 recs at 60fps (LOW-12).
    cache_key: (String, bool, u64),
    rows: Vec<usize>,
}

impl BlocksPanel {
    fn new() -> Self {
        Self {
            filter: String::new(),
            failed_only: false,
            // Impossible stamp ⇒ first frame always computes.
            cache_key: (String::new(), false, u64::MAX),
            rows: Vec::new(),
        }
    }
}

/// Cross-session history popup state (P4 §3.3), anchored above the composer
/// strip. Exists only while open — closed costs zero memory and zero work.
struct HistoryPopup {
    query: String,
    /// Keyboard-selected row: index into `hits`.
    sel: usize,
    /// Filtered indices into `entries` (recomputed on query change/rebuild).
    hits: Vec<u32>,
    entries: Vec<history::HistEntry>,
    /// blocks_stamp at build; drift ⇒ rebuild + re-filter.
    built: u64,
    /// When the index was last rebuilt: stamp-drift rebuilds are debounced to
    /// one per 500ms so a busy terminal closing blocks every second can't
    /// force O(total recs) work under the user's pointer while they read the
    /// list (LOW-13). The first build (built == u64::MAX) is never delayed.
    built_at: Instant,
    /// A keyboard move this frame: nudge the scroll so `sel` stays visible.
    kb_moved: bool,
}

impl HistoryPopup {
    fn new() -> Self {
        Self {
            query: String::new(),
            sel: 0,
            hits: Vec::new(),
            entries: Vec::new(),
            built: u64::MAX, // != any stamp ⇒ first frame builds
            built_at: Instant::now(),
            kb_moved: false,
        }
    }
}

/// What the central panel is showing (V-C).
#[derive(Clone, Copy, PartialEq)]
enum CentralView {
    Terminal,
    /// A card dashboard scoped to one folder, or all terminals when `None`.
    Dashboard(Option<Uuid>),
}

/// What a dashboard-card click asked for (§6.2).
enum CardAction {
    Select,
    /// The dead card's hover `↻ Restore` ghost button.
    Restore,
}

/// Which zone of a folder header was clicked (V-C).
enum FolderAction {
    None,
    ToggleCollapse,
    Dashboard,
    Delete,
    /// Hover ✏ — inline rename (§5.4).
    Rename,
}

/// What an inline rename edits (§5.4).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RenameTarget {
    Term(Uuid),
    Folder(Uuid),
}

/// Which surface hosts the rename TextEdit — exactly one host renders it
/// per frame (a terminal can be visible in the sidebar AND the top bar).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RenameHost {
    Row,
    Bar,
}

/// Inline rename in flight (§5.4). Transient, never persisted.
struct RenameState {
    target: RenameTarget,
    value: String,
    host: RenameHost,
    /// Open-frame work pending: focus grab + select-all (LOW-9 pattern).
    /// Also marks "the editor has not rendered yet", so the end-of-frame
    /// not-rendered check (host vanished ⇒ blur-commit) skips the frames
    /// between start and first paint.
    focus_pending: bool,
    /// The editor confirmed egui focus at least once — gates the
    /// lost_focus ⇒ commit rule against the open-frame race.
    had_focus: bool,
    /// A host rendered the editor this frame (reset each frame in `ui`).
    rendered: bool,
}

/// The pure commit rule (§5.4, unit-tested): trimmed, empty ⇒ cancel
/// (None), else the rename verb for the target.
fn rename_commit(target: RenameTarget, value: &str) -> Option<C2D> {
    let name = value.trim();
    if name.is_empty() {
        return None;
    }
    Some(match target {
        RenameTarget::Term(id) => C2D::RenameTerminal { id, name: name.to_string() },
        RenameTarget::Folder(id) => C2D::RenameFolder { id, name: name.to_string() },
    })
}

/// A drag-to-reorder in flight (§5.5). Created at egui's decided-drag
/// threshold (~6px — clicks and context menus unaffected), transient.
struct DragState {
    /// What is being dragged — terminal rows and folder rows arm drags.
    payload: DragPayload,
    /// Ghost content, captured at drag start (name + activity dot color;
    /// folder ghosts carry the member count and no dot).
    name: String,
    dot: Color32,
    /// Pointer-lock offset: grab point relative to the row's origin, so the
    /// ghost rides the pointer exactly where the row was grabbed.
    grab: Vec2,
}

/// The dragged subject (Bug B): terminals move/reorder, folders reorder.
enum DragPayload {
    Term {
        id: Uuid,
        /// The dragged terminal's folder at drag start (drop into the same
        /// group sends no MoveTerminal).
        from: Option<Uuid>,
    },
    Folder { id: Uuid },
}

/// One row painted by this frame's sidebar tree, recorded while a drag is
/// armed — the §5.5 drop-slot map (hit-test against LAST frame's rows is
/// at most one frame stale, invisible at pointer speeds).
enum DropRow {
    Term {
        rect: Rect,
        folder: Option<Uuid>,
        /// Index of this row within its painted group (order-sorted).
        idx: usize,
    },
    Folder {
        rect: Rect,
        id: Uuid,
        /// Painted folder ordinal (order-sorted) — folder-reorder drops.
        idx: usize,
    },
    /// The UNGROUPED header / empty tail of the tree — always revealed
    /// while a terminal drag is armed so a terminal can ALWAYS be dragged
    /// out of every folder (Bug B: previously a silent dead zone).
    LooseZone { rect: Rect },
}

/// A resolved drop position (visual marker + wire semantics).
enum SlotHit {
    /// Insert into `folder`'s group before painted row `idx` (idx == group
    /// len ⇒ append). `y`/`x` place the 2px accent insertion bar.
    Insert {
        folder: Option<Uuid>,
        idx: usize,
        y: f32,
        x: egui::Rangef,
    },
    /// Move into a folder (append) — the folder row highlights.
    IntoFolder { id: Uuid, rect: Rect },
    /// Reorder a folder to painted ordinal `idx` — the bar sits in the
    /// inter-group gap (`y` is below the target group / above the next).
    FolderInsert {
        idx: usize,
        y: f32,
        x: egui::Rangef,
    },
    /// Move a terminal out of all folders (append to the loose group).
    LooseAppend { y: f32, x: egui::Rangef },
}

/// Pure §5.5 drop math (unit-tested): the `ReorderTerminal` delta that
/// lands `id` at the painted insertion index `idx` (None = append).
/// `group` replicates the DAEMON's post-move group — terminals filtered by
/// destination folder in snapshot vec order, stable-sorted by `order`,
/// INCLUDING the dragged terminal. `same_group` says whether the painted
/// rows included the dragged row (same-group drags do; cross-group drags
/// paint the destination group without it), which shifts the target index
/// when inserting below the source. The daemon's remove+insert semantics
/// make final_index == cur + delta (clamped), so delta = final − cur.
fn drop_reorder_delta(group: &[Uuid], id: Uuid, idx: Option<usize>, same_group: bool) -> i32 {
    let Some(cur) = group.iter().position(|&g| g == id) else {
        return 0;
    };
    let last = group.len() - 1;
    let fin = match idx {
        None => last,
        Some(i) if same_group && i > cur => (i - 1).min(last),
        Some(i) => i.min(last),
    };
    fin as i32 - cur as i32
}

/// Pure §5.5 hit-test (unit-tested), payload-aware. Bands are the painted
/// row rects widened by ±8px horizontally and ±4px vertically (half the
/// worst inter-row gap — no dead strip between adjacent rows; overlapping
/// bands resolve first-in-paint-order, which yields the same insert index
/// either way). Outside every band ⇒ None (release there cancels).
///
/// Terminal payload: terminal rows split at their midline into
/// insert-above / insert-below (the last row of each group gets 20px of
/// grace below for end-of-list drops); folder rows are move-into targets
/// over their whole height; LooseZone bands append to the loose group.
///
/// Folder payload: folder rows split at their midline into
/// reorder-above / reorder-below; a folder's open member rows resolve to
/// that folder's below-slot so the whole visual group reads as one target
/// (the bar always sits in the inter-group gap). Loose terminal rows and
/// LooseZone bands are ignored (a folder can't nest or go loose).
fn slot_hit(rows: &[DropRow], payload: &DragPayload, p: Pos2) -> Option<SlotHit> {
    const SLOP_X: f32 = 8.0;
    const SLOP_Y: f32 = 4.0;
    let in_band = |rect: Rect, grace: f32| {
        p.x >= rect.min.x - SLOP_X
            && p.x <= rect.max.x + SLOP_X
            && p.y >= rect.min.y - SLOP_Y
            && p.y <= rect.max.y + SLOP_Y + grace
    };
    // The inter-group gap below folder `fid`'s painted group: the bottom of
    // its last visible member row (or the folder row itself when collapsed
    // or empty) — where a FolderInsert below-bar sits.
    let group_bottom = |fid: Uuid| {
        rows.iter()
            .filter_map(|r| match r {
                DropRow::Folder { rect, id, .. } if *id == fid => Some(rect.max.y),
                DropRow::Term { rect, folder, .. } if *folder == Some(fid) => Some(rect.max.y),
                _ => None,
            })
            .fold(f32::MIN, f32::max)
    };
    for row in rows {
        match (payload, row) {
            (DragPayload::Term { .. }, DropRow::Term { rect, folder, idx }) => {
                // Grace below the group's last painted row.
                let last = !rows.iter().any(|r| {
                    matches!(r, DropRow::Term { folder: f, idx: i, .. }
                        if f == folder && *i == idx + 1)
                });
                if !in_band(*rect, if last { 20.0 } else { 0.0 }) {
                    continue;
                }
                let below = p.y >= rect.center().y;
                return Some(SlotHit::Insert {
                    folder: *folder,
                    idx: if below { idx + 1 } else { *idx },
                    y: if below { rect.max.y + 2.0 } else { rect.min.y - 2.0 },
                    x: rect.x_range(),
                });
            }
            (DragPayload::Term { .. }, DropRow::Folder { rect, id, .. }) => {
                if !in_band(*rect, 0.0) {
                    continue;
                }
                return Some(SlotHit::IntoFolder { id: *id, rect: *rect });
            }
            (DragPayload::Term { .. }, DropRow::LooseZone { rect }) => {
                if !in_band(*rect, 0.0) {
                    continue;
                }
                return Some(SlotHit::LooseAppend {
                    y: rect.min.y + 1.0,
                    x: rect.x_range(),
                });
            }
            (DragPayload::Folder { .. }, DropRow::Folder { rect, id, idx }) => {
                if !in_band(*rect, 0.0) {
                    continue;
                }
                let below = p.y >= rect.center().y;
                return Some(SlotHit::FolderInsert {
                    idx: if below { idx + 1 } else { *idx },
                    y: if below { group_bottom(*id) + 2.0 } else { rect.min.y - 2.0 },
                    x: rect.x_range(),
                });
            }
            (DragPayload::Folder { .. }, DropRow::Term { rect, folder: Some(fid), .. }) => {
                // A foldered member row: the whole open group reads as its
                // folder's below-slot.
                if !in_band(*rect, 0.0) {
                    continue;
                }
                let fidx = rows.iter().find_map(|r| match r {
                    DropRow::Folder { id, idx, .. } if id == fid => Some(*idx),
                    _ => None,
                })?;
                let frect = rows.iter().find_map(|r| match r {
                    DropRow::Folder { rect, id, .. } if id == fid => Some(*rect),
                    _ => None,
                })?;
                return Some(SlotHit::FolderInsert {
                    idx: fidx + 1,
                    y: group_bottom(*fid) + 2.0,
                    x: frect.x_range(),
                });
            }
            // A folder can't nest into loose rows or the loose zone.
            (DragPayload::Folder { .. }, _) => continue,
        }
    }
    None
}

fn prefs_path() -> PathBuf {
    crate::state::data_dir().join("gui.json")
}

/// Load Prefs from `path`. A PRESENT-but-unparseable file is renamed to
/// `gui.json.corrupt` (state.json parity) before defaulting: Prefs carries
/// consent state (hook-host verdicts, paste_warn, "never ask again"), and
/// silently defaulting over a corrupt file would re-prompt for everything
/// AND destroy the evidence on the next save.
fn load_prefs(path: &std::path::Path) -> Prefs {
    let Ok(bytes) = std::fs::read(path) else {
        return Prefs::default(); // no file yet — first run
    };
    match serde_json::from_slice::<Prefs>(&bytes) {
        Ok(mut p) => {
            // v0.1.2 field fix: heal pre-v0.1.1 WSL spawn-row poison at the
            // persistence boundary (covers instant-create, Suggested rows,
            // the sidebar preview, AND backup restores — settings.rs reloads
            // through here). Idempotent; persisted on the next save_prefs.
            if heal_wsl_profile_cwds(&mut p) {
                log::info!("gui.json: healed WSL spawn rows pinned to the Windows profile dir (\u{2192} ~)");
            }
            p
        }
        Err(e) => {
            log::error!("gui.json corrupt ({e}); starting from defaults, old file backed up");
            let _ = std::fs::rename(path, path.with_extension("json.corrupt"));
            Prefs::default()
        }
    }
}

/// v0.1.2: is `cwd` the WINDOWS profile dir a pre-v0.1.1 default recorded
/// into WSL spawn rows? True for the current user's home (case-insensitive,
/// separator-normalized, trailing-separator-tolerant) and for any
/// `<drive>:\Users\<name>` profile ROOT (the same poison from another
/// account/machine riding a copied gui.json). Deliberately NOT true for
/// anything deeper (`C:\Users\zany\projects` is a real "open this Windows
/// project in WSL" pick — a feature).
fn is_windows_profile_dir(cwd: &Path, home: Option<&Path>) -> bool {
    fn norm(p: &Path) -> String {
        let mut s = p.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
        while s.ends_with('\\') {
            s.pop();
        }
        s
    }
    let c = norm(cwd);
    if c.is_empty() {
        return false; // empty is heal_cwd's job (spawn-time → ~ already)
    }
    if home.is_some_and(|h| norm(h) == c) {
        return true;
    }
    let parts: Vec<&str> = c.split('\\').collect();
    parts.len() == 3
        && parts[0].len() == 2
        && parts[0].as_bytes()[0].is_ascii_alphabetic()
        && parts[0].as_bytes()[1] == b':'
        && parts[1] == "users"
        && !parts[2].is_empty()
}

/// One-time (idempotent, every load) heal of persisted WSL spawn rows whose
/// cwd is the WINDOWS profile dir: the pre-v0.1.1 default recorded
/// `C:\Users\<u>` into `last_spawn`/`recent_spawns` for WSL tags, and
/// v0.1.1's `heal_cwd` only heals EMPTY cwds — so every instant-create and
/// Suggested-row spawn kept replaying `/mnt/c/Users/<u>` forever
/// (self-reinforcing via note_spawn; survives uninstall/reinstall because
/// the data dir is deliberately kept). Rewrites those rows to `~` (the
/// LINUX home, resolved in-distro by `wsl --cd ~`). Runtime explicit
/// directory picks are untouched — this runs only on rows loaded from disk.
fn heal_wsl_profile_cwds(prefs: &mut Prefs) -> bool {
    let home = dirs::home_dir();
    let mut changed = false;
    {
        let mut heal = |s: &mut SpawnSpec| {
            if launcher::wsl_tag_distro(&s.kind_tag).is_some()
                && is_windows_profile_dir(&s.cwd, home.as_deref())
            {
                s.cwd = PathBuf::from("~");
                changed = true;
            }
        };
        if let Some(last) = prefs.last_spawn.as_mut() {
            heal(last);
        }
        for recent in prefs.recent_spawns.iter_mut() {
            heal(recent);
        }
    }
    changed
}

/// ssh-drop §4: the drop batch a consent dialog covers. Continue enqueues
/// it; Cancel/Esc drops it silently (the user just said no — no toast).
struct PendingSshDrop {
    terminal: Uuid,
    paths: Vec<PathBuf>,
    dont_ask_again: bool,
}

pub struct App {
    ipc: Option<IpcClient>,
    last_connect: Instant,
    /// Idle heartbeat bookkeeping (R7) and daemon-restart detection (R8a).
    last_ping: Instant,
    last_daemon_pid: Option<u32>,
    /// Transient restart notice and throttled error banner (R8a / R4).
    notice: Option<(String, Instant)>,
    last_error: Option<(String, Instant)>,
    state: SharedState,
    terms: HashMap<Uuid, TermBackend>,
    /// Journal Blocks per terminal, mirrored from D2C::Blocks (full sync on
    /// attach, upserts keyed by start_off live — journal offsets are
    /// monotonic per terminal, so start_off is unique even across epochs).
    blocks: HashMap<Uuid, BlockList>,
    /// Per-terminal composer state (P3). Created lazily on the first Blocks
    /// frame with epoch > 0 — hookless terminals (claude, cmd) never allocate
    /// one and pay zero cost anywhere in the composer path.
    composers: HashMap<Uuid, ComposerState>,
    /// The blocks recall panel (P2), open for the selected terminal only.
    blocks_panel: Option<BlocksPanel>,
    /// This frame's header Blocks-button rect: click-outside panel closing
    /// must exempt it, or the toggle would close-on-press + reopen-on-release.
    blocks_btn_rect: Option<Rect>,
    /// Cross-session history popup (P4), open for the selected terminal only.
    /// None ⇒ zero cost, no index, no memory.
    history: Option<HistoryPopup>,
    /// This frame's strip History-button rect (click-outside exemption —
    /// the blocks-panel pattern).
    history_btn_rect: Option<Rect>,
    /// Bumped on every Blocks frame / store prune / reconnect: the popup's
    /// lazily-built index rebuilds when its `built` stamp drifts (D11).
    blocks_stamp: u64,
    /// The settings dialog (task #33). None ⇒ zero cost (launcher/history
    /// pattern); runtime-only, never persisted.
    settings: Option<SettingsState>,
    /// The update backend (#34): the settings Updates section and the
    /// sidebar update row/popover render from `state()`; Velopack engine
    /// behind it (update.rs). Dev builds degrade to Unsupported (hidden).
    updates: Box<dyn UpdateProvider>,
    /// #34: the anchored update popover (Axis 5). None = closed, zero cost.
    update_popover: Option<UpdatePopover>,
    /// #34 Axis 7: post-update daemon health check — one toast if the daemon
    /// still isn't back when this deadline passes on an updated boot.
    update_health_due: Option<Instant>,
    /// #34 lifecycle: the one-time first-run welcome card (Velopack
    /// on_first_run latch). Dismiss = gone forever.
    welcome_card: bool,
    selected: Option<Uuid>,
    unread: HashSet<Uuid>,
    /// Per-terminal activity bookkeeping (V-A).
    activity: HashMap<Uuid, ActivityState>,
    /// Terminals we have already flashed the taskbar for while unfocused, so a
    /// single NeedsYou event fires RequestUserAttention exactly once (V-D).
    attention_flashed: HashSet<Uuid>,
    /// Snapshot generation: bumped whenever `self.state` is replaced —
    /// `apply_snapshot` is the ONLY mutation site for state.terminals /
    /// state.folders. Keys the sidebar row cache below.
    state_gen: u64,
    /// Sidebar presentation rows, rebuilt only on `state_gen` drift.
    /// Renames/color tags/asleep all arrive via Snapshot, so the cache stays
    /// truthful; Rc so the paint pass can iterate rows while `&mut self` row
    /// methods run. Replaces ~10 heap clones per terminal per painted frame.
    sidebar_rows: std::rc::Rc<SidebarRows>,
    /// Fleet aggregates, computed once per logic frame at the end of
    /// `update_activity`'s pass: whether anything is Working (drives the
    /// 100ms pulse repaint) and the NeedsYou set in sidebar order (drives
    /// the attention pill and its cycle-next order).
    any_working: bool,
    waiting: Vec<Uuid>,
    /// Dashboard card preview text, cached per terminal keyed on
    /// `preview_key` — rebuilding it walked up to rows×cols cells per card
    /// per frame while the dashboard repainted at ≥10fps.
    previews: HashMap<Uuid, PreviewCache>,
    /// Central panel mode: a terminal or the card dashboard (V-C).
    central_view: CentralView,
    /// Inline scrollback search state for the selected terminal (V4).
    search: Option<SearchState>,
    modal: Option<Modal>,
    /// The launcher palette (selector §4) — overlay when opened from the +
    /// chevron, or the §6.1 empty-state embed (`embedded` flag). None ⇒ zero
    /// cost, no candidate index.
    launcher: Option<LauncherState>,
    /// This frame's split-+ button rect: launcher click-outside closing must
    /// exempt it (the blocks-panel press/release pattern).
    launcher_btn_rect: Option<Rect>,
    /// Auto-select on create (§3.2): the name we just asked the daemon for,
    /// stamped. Resolved in `apply_snapshot`, 5s expiry, cancelled by any
    /// manual selection.
    pending_create: Option<(String, Instant)>,
    /// Inline rename in flight (§5.4). While Some, overlay_open is true for
    /// the selected terminal's card (composer/grid stand down).
    renaming: Option<RenameState>,
    /// Drag-to-reorder in flight (§5.5).
    drag: Option<DragState>,
    /// The sidebar rows painted last frame while a drag was armed — the
    /// drop-slot map (§5.5). Rebuilt every armed frame.
    drop_rows: Vec<DropRow>,
    /// Last frame's central-panel rect — anchors the launcher overlay.
    central_rect: Option<Rect>,
    prefs: Prefs,
    bindings: BindingsLayout,
    url_regex: RegexSearch,
    /// Per-glyph galley cache shared across all terminals.
    glyphs: glyph_cache::GlyphCache,
    /// Persistent shape buffers for term_view::render (drained every frame;
    /// capacity survives so streaming frames stop regrowing seven Vecs).
    render_scratch: term_view::RenderScratch,
    /// Committed (inner grid size, cell) of the terminal area, in points.
    last_grid: Option<(Vec2, Vec2)>,
    /// Candidate grid waiting out the resize throttle/stability window.
    pending_grid: Option<(Vec2, Vec2, Instant)>,
    /// When the last PTY resize was committed (rate-limits live drag resizes).
    last_resize_commit: Instant,
    /// Echo-path latency tracer, enabled with TC_TRACE_LATENCY=1 (T-LAT).
    trace: Option<LatTrace>,
    /// Start of the current frame's `logic()`, consumed by `ui()`'s tail to
    /// clock the whole frame. Tracing only.
    frame_t0: Option<Instant>,
    /// DIAGNOSTIC (perf-wave-2): TC_DIAG_SPIN=1 requests a repaint every frame
    /// (pins the pipeline at vsync rate with zero content change) and
    /// TC_DIAG_EMPTY_UI=1 short-circuits `ui()` to a bare background fill.
    /// Together they attribute per-frame CPU: spin+empty = eframe/egui fixed
    /// pipeline cost, spin+full = fixed + App paint. Never set in normal use.
    diag_spin: bool,
    diag_empty_ui: bool,
    /// DIAGNOSTIC (C2 staging): TC_DIAG_ASSUME_FOCUS=1 treats the window as
    /// focused for the PTY-resize gate. Display-detached RDP rigs never
    /// deliver WM_SETFOCUS to the winit window, so `RawInput::focused` stays
    /// false and NO resize (boot commit, corrective heal, strip collapse)
    /// can ever flow — this knob lets scripted staging exercise the real
    /// resize pipeline there. Single-window rigs only (the focused-only rule
    /// exists so two windows can't fight over PTY grids). Never set in
    /// normal use.
    diag_assume_focus: bool,
    /// DIAGNOSTIC (launcher staging): TC_DIAG_OPEN_LAUNCHER_MS=<ms> invokes
    /// `open_launcher()` once, N ms after boot — the same method every real
    /// entry point (titlebar chevron, sidebar +, folder "New terminal
    /// here…") funnels into — and TC_DIAG_LAUNCHER_QUERY presets the query
    /// at that invocation. TC_DIAG_START_VIEW=dashboard boots into the card
    /// dashboard. Display-detached RDP rigs deliver no synthetic clicks or
    /// keys (same family as TC_DIAG_ASSUME_FOCUS), so scripted staging needs
    /// product-side hooks to exercise the launcher entry points. Never set
    /// in normal use.
    diag_open_launcher: Option<Instant>,
    diag_launcher_query: Option<String>,
    /// Startup/attach lifecycle stage tracker (perf-wave-3), enabled with
    /// TC_PERF_STAGES=1 — the daemon's stage-timer knob. Log-only; None when
    /// off. gui.log gets `[perf] gui …` lines for cold start (window+wgpu →
    /// connected → snapshot → replays parsed → painted) and for every attach
    /// cycle (initial attach-all AND reconnect re-attach after a daemon
    /// restart — both go through the same snapshot→Attach→Replay path).
    perf3: Option<GuiPerf>,
    /// Font-change stage tracker (TC_PERF_STAGES): one record per explicit
    /// font-size step, logged as `[perf] fontstep …` lines
    /// (click → commit → settled). Log-only; None while idle or when the
    /// knob is off.
    font_perf: Option<FontPerf>,
    /// Stamped at every explicit font-size step (footer stepper / Ctrl+wheel
    /// zoom). A cell-metric change with a recent step is a deliberate user
    /// action, not a DPI/monitor-hop flap.
    font_step_t0: Option<Instant>,
    /// R3-5: pending debounced prefs save (font_size only — a zoom gesture is
    /// 5-20 wheel notches and each save_prefs is an fsync on the paint
    /// thread). Flushed by logic() once due, and on exit. Consent answers
    /// keep their immediate save_prefs().
    prefs_save_due: Option<Instant>,
    /// ssh-drop (#26): the toast stack — bottom-right of the central area,
    /// shown at the end of `ui()`. The app's first toast surface (§5).
    toasts: toast::Toasts,
    /// ssh-drop (#26): per-terminal upload queues + worker threads (§6).
    uploads: ssh_drop::Uploads,
    /// ssh-drop §4: the drop batch the consent modal covers. While ANY
    /// modal is open new ssh drops no-op, so at most one exists.
    pending_ssh_drop: Option<PendingSshDrop>,
    /// Attribution Layer 3: the host the ClaudeHookConsent modal covers.
    pending_claude_hook: Option<PendingClaudeHook>,
    /// Hosts whose consent question was dismissed (Esc) or answered THIS
    /// run — never re-prompt within a GUI session. Runtime-only.
    claude_hook_dismissed: HashSet<String>,
    /// Hosts with a beacon install running or finished this run (yes-hosts
    /// re-verify once per GUI run — the install is idempotent and heals a
    /// remotely-deleted script).
    claude_hook_done: HashSet<String>,
    /// r4 perf-gui L3: terminals whose claude consent lane is fully settled —
    /// skipped by the per-frame scan without rebuilding host Strings.
    /// Cleared on snapshot apply and install failure.
    claude_consent_settled: HashSet<Uuid>,
    /// Install worker results: (host, outcome) → toast.
    claude_hook_rx: std::sync::mpsc::Receiver<(String, Result<crate::claude_hooks::Outcome, String>)>,
    claude_hook_tx: std::sync::mpsc::Sender<(String, Result<crate::claude_hooks::Outcome, String>)>,
    /// Codex attribution: the lane the CodexHookConsent modal covers.
    pending_codex_hook: Option<PendingCodexHook>,
    /// Codex-hook lanes dismissed (Esc) or answered this GUI run.
    codex_hook_dismissed: HashSet<String>,
    /// Codex-hook lanes with an install running/finished this run (idempotent
    /// re-verify per run, heals a deleted script/hook).
    codex_hook_done: HashSet<String>,
    /// r4 perf-gui L3: codex sibling of `claude_consent_settled`.
    codex_consent_settled: HashSet<Uuid>,
    /// Codex install worker results: (lane key + human label, outcome) → toast.
    codex_hook_rx: std::sync::mpsc::Receiver<(String, Result<crate::codex_hooks::Outcome, String>)>,
    codex_hook_tx: std::sync::mpsc::Sender<(String, Result<crate::codex_hooks::Outcome, String>)>,
}

/// See `App::font_perf`. Clocks one font-size step end to end:
/// `t0` = the click; `committed` = the resize-commit frame; `last_activity`
/// = the most recent resize-driven event (commit itself, or an Output frame
/// parsed after it — conhost's post-resize repaint). Settled = activity
/// quiet for 700ms; `settle_ms` in the log is last_activity − t0.
struct FontPerf {
    t0: Instant,
    committed: Option<Instant>,
    last_activity: Instant,
    /// Start of the current frame's logic() while this tracker is live.
    frame_t0: Option<Instant>,
    frames_gt16: u32,
    frames_gt33: u32,
    max_frame_us: u64,
}

impl FontPerf {
    fn new(now: Instant) -> Self {
        Self {
            t0: now,
            committed: None,
            last_activity: now,
            frame_t0: None,
            frames_gt16: 0,
            frames_gt33: 0,
            max_frame_us: 0,
        }
    }
}

/// See `App::perf3`. `ms=` in every line is milliseconds since `gui::run`
/// (gui.log timestamps are 1s resolution); `cycle_ms=` is since the snapshot
/// that opened the current attach cycle.
#[derive(Default)]
struct GuiPerf {
    /// Set when a snapshot announces new attaches while none were pending.
    cycle_t0: Option<Instant>,
    /// Terminals whose attach Replay hasn't been parsed yet this cycle.
    pending: HashSet<Uuid>,
    /// Cumulative Replay vte-parse time this cycle.
    parse_us: u64,
    /// Log a stage line at the end of the next `ui()` paint.
    paint_selected: bool,
    paint_all: bool,
    first_paint_done: bool,
}

/// Milliseconds since `gui::run` began (0 if it never marked — tests).
fn gui_ms() -> u64 {
    GUI_T0
        .get()
        .map(|t0| t0.elapsed().as_millis() as u64)
        .unwrap_or(0)
}
static GUI_T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Measures the two GUI-side legs of keystroke echo, logged to gui.log every
/// 2s while enabled (TC_TRACE_LATENCY=1): `sched` is socket-arrival → drain
/// (how long a daemon frame waited for a repaint to pick it up) and `frame`
/// is logic-start → ui-end (how long a repaint takes once it runs). Costs
/// nothing when disabled — the App field is None.
struct LatTrace {
    sched_us: Vec<u64>,
    frame_us: Vec<u64>,
    last_report: Instant,
    /// egui cumulative counters at the last report: painted frames vs UI
    /// passes (pass surplus over frames = request_discard reruns).
    last_frame_nr: u64,
    last_pass_nr: u64,
}

impl LatTrace {
    fn enabled() -> Option<Self> {
        (std::env::var("TC_TRACE_LATENCY").ok().as_deref() == Some("1")).then(|| Self {
            sched_us: Vec::new(),
            frame_us: Vec::new(),
            last_report: Instant::now(),
            last_frame_nr: 0,
            last_pass_nr: 0,
        })
    }

    fn report(&mut self, ctx: &egui::Context) {
        if self.last_report.elapsed() < Duration::from_secs(2)
            || (self.sched_us.is_empty() && self.frame_us.is_empty())
        {
            return;
        }
        fn pct(sorted: &[u64], p: usize) -> u64 {
            if sorted.is_empty() {
                return 0;
            }
            sorted[(sorted.len() * p / 100).min(sorted.len() - 1)]
        }
        self.sched_us.sort_unstable();
        self.frame_us.sort_unstable();
        let frame_nr = ctx.cumulative_frame_nr();
        let pass_nr = ctx.cumulative_pass_nr();
        log::info!(
            "[lat] sched n={} p50={}us p95={}us max={}us | frame n={} p50={}us p95={}us max={}us | painted={} passes={}",
            self.sched_us.len(),
            pct(&self.sched_us, 50),
            pct(&self.sched_us, 95),
            self.sched_us.last().copied().unwrap_or(0),
            self.frame_us.len(),
            pct(&self.frame_us, 50),
            pct(&self.frame_us, 95),
            self.frame_us.last().copied().unwrap_or(0),
            frame_nr - self.last_frame_nr,
            pass_nr - self.last_pass_nr,
        );
        self.last_frame_nr = frame_nr;
        self.last_pass_nr = pass_nr;
        self.sched_us.clear();
        self.frame_us.clear();
        self.last_report = Instant::now();
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let perf3 = (std::env::var("TC_PERF_STAGES").ok().as_deref() == Some("1"))
            .then(GuiPerf::default);
        if perf3.is_some() {
            // eframe has finished window + wgpu adapter/device init by the
            // time it calls the app factory: this minus `gui start` is the
            // graphics-stack share of cold start.
            log::info!("[perf] gui window_ready ms={}", gui_ms());
        }
        install_fonts(&cc.egui_ctx);
        style(&cc.egui_ctx);

        let mut prefs = load_prefs(&prefs_path());
        // #34 Axis 7: post-update boot detection. `updated_from` is Some only
        // when a PREVIOUS version ran here (never on first run or the
        // pre-updater-era gui.json shape).
        let current_version = env!("CARGO_PKG_VERSION");
        let updated_from = prefs
            .last_run_version
            .clone()
            .filter(|v| v != current_version);
        let version_changed = prefs.last_run_version.as_deref() != Some(current_version);
        if version_changed {
            prefs.last_run_version = Some(current_version.to_string());
        }

        let ipc = ipc::connect_or_spawn(cc.egui_ctx.clone()).ok();
        let last_daemon_pid = ipc.as_ref().map(|c| c.pid);
        if perf3.is_some() {
            log::info!(
                "[perf] gui connected ok={} ms={}",
                ipc.is_some(),
                gui_ms()
            );
        }
        let (claude_hook_tx, claude_hook_rx) = std::sync::mpsc::channel();
        let (codex_hook_tx, codex_hook_rx) = std::sync::mpsc::channel();

        let mut app = Self {
            ipc,
            last_connect: Instant::now(),
            last_ping: Instant::now(),
            last_daemon_pid,
            notice: None,
            last_error: None,
            state: SharedState::default(),
            terms: HashMap::new(),
            blocks: HashMap::new(),
            composers: HashMap::new(),
            blocks_panel: None,
            blocks_btn_rect: None,
            history: None,
            history_btn_rect: None,
            blocks_stamp: 0,
            settings: None,
            updates: Box::new(VelopackUpdateProvider::new(cc.egui_ctx.clone())),
            update_popover: None,
            update_health_due: None,
            welcome_card: crate::FIRST_RUN.load(std::sync::atomic::Ordering::Relaxed),
            selected: None,
            unread: HashSet::new(),
            activity: HashMap::new(),
            attention_flashed: HashSet::new(),
            // Starts at 1 against the cache's gen 0, so the first frame
            // builds rows even before the first Snapshot lands.
            state_gen: 1,
            sidebar_rows: Default::default(),
            any_working: false,
            waiting: Vec::new(),
            previews: HashMap::new(),
            central_view: if std::env::var("TC_DIAG_START_VIEW").ok().as_deref()
                == Some("dashboard")
            {
                CentralView::Dashboard(None)
            } else {
                CentralView::Terminal
            },
            search: None,
            modal: None,
            launcher: None,
            launcher_btn_rect: None,
            pending_create: None,
            renaming: None,
            drag: None,
            drop_rows: Vec::new(),
            central_rect: None,
            prefs,
            bindings: BindingsLayout::new(),
            url_regex: RegexSearch::new(term_view::URL_REGEX).expect("static regex"),
            glyphs: glyph_cache::GlyphCache::default(),
            render_scratch: term_view::RenderScratch::default(),
            last_grid: None,
            pending_grid: None,
            last_resize_commit: Instant::now(),
            trace: LatTrace::enabled(),
            frame_t0: None,
            diag_spin: std::env::var("TC_DIAG_SPIN").ok().as_deref() == Some("1"),
            diag_empty_ui: std::env::var("TC_DIAG_EMPTY_UI").ok().as_deref() == Some("1"),
            diag_assume_focus: std::env::var("TC_DIAG_ASSUME_FOCUS").ok().as_deref()
                == Some("1"),
            diag_open_launcher: std::env::var("TC_DIAG_OPEN_LAUNCHER_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(|ms| Instant::now() + Duration::from_millis(ms)),
            diag_launcher_query: std::env::var("TC_DIAG_LAUNCHER_QUERY").ok(),
            perf3,
            font_perf: None,
            font_step_t0: None,
            prefs_save_due: None,
            toasts: toast::Toasts::default(),
            uploads: ssh_drop::Uploads::new(cc.egui_ctx.clone()),
            pending_ssh_drop: None,
            pending_claude_hook: None,
            claude_hook_dismissed: HashSet::new(),
            claude_hook_done: HashSet::new(),
            claude_consent_settled: HashSet::new(),
            claude_hook_rx,
            claude_hook_tx,
            pending_codex_hook: None,
            codex_hook_dismissed: HashSet::new(),
            codex_hook_done: HashSet::new(),
            codex_consent_settled: HashSet::new(),
            codex_hook_rx,
            codex_hook_tx,
        };
        if version_changed {
            // Persist immediately: a crash-loop must not re-toast every boot.
            app.save_prefs();
            if let Some(from) = updated_from {
                log::info!("post-update boot: v{from} -> v{current_version}");
                app.toasts.push(toast::Toast {
                    kind: toast::ToastKind::Info,
                    title: format!("Updated to v{current_version}"),
                    detail: Vec::new(),
                    ttl: Some(Duration::from_secs(6)),
                    action: None,
                });
                app.update_health_due = Some(Instant::now() + Duration::from_secs(15));
            }
        }
        app
    }

    /// One explicit font-size step (footer stepper / Ctrl+wheel zoom /
    /// settings row / demo knob) — the single entry point so every caller
    /// gets identical prefs/persist/perf treatment.
    fn font_step(&mut self, delta: f32) {
        let new = (self.prefs.font_size + delta).clamp(8.0, 28.0);
        if new == self.prefs.font_size {
            return;
        }
        self.prefs.font_size = new;
        // Debounced (R3-5): persist 500ms after the LAST step of the gesture.
        // A power cut inside the window loses only the final zoom level.
        self.prefs_save_due = Some(Instant::now() + Duration::from_millis(500));
        let now = Instant::now();
        self.font_step_t0 = Some(now);
        if self.perf3.is_some() {
            log::info!("[perf] fontstep click size={new} ms={}", gui_ms());
            self.font_perf = Some(FontPerf::new(now));
        }
    }

    fn save_prefs(&self) {
        let Ok(data) = serde_json::to_vec_pretty(&self.prefs) else {
            return;
        };
        // Atomic write: fsync a temp file, then rename over the old prefs so a
        // power cut can never leave a truncated gui.json. C4 honesty: a
        // silent failure loses consent state ("never ask again" answers) —
        // the user gets re-prompted with no clue why; log it.
        let path = prefs_path();
        let tmp = path.with_extension("json.tmp");
        let write_tmp = || -> std::io::Result<()> {
            use std::io::Write;
            std::fs::create_dir_all(crate::state::data_dir())?;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        };
        if let Err(e) = write_tmp() {
            log::error!("gui.json save failed (consent/prefs may re-prompt): {e}");
        }
    }

    fn send(&self, msg: C2D) {
        if let Some(ipc) = &self.ipc {
            ipc.send(&msg);
        }
    }

    /// C2D::Input with large payloads split into 64KiB frames: a keystroke
    /// stays one frame, while a multi-MB paste can neither wedge the daemon
    /// behind one giant pipe write nor trip the 32MB frame cap (which kills
    /// the connection). Frames are FIFO per connection, so chunk order —
    /// including bracketed-paste markers landing in the first/last chunk —
    /// is preserved end-to-end.
    fn send_input(&self, id: Uuid, bytes: Vec<u8>) {
        const INPUT_CHUNK: usize = 64 * 1024;
        if bytes.len() <= INPUT_CHUNK {
            self.send(C2D::Input { id, bytes });
            return;
        }
        for chunk in bytes.chunks(INPUT_CHUNK) {
            self.send(C2D::Input {
                id,
                bytes: chunk.to_vec(),
            });
        }
    }

    /// task #22: the connected daemon understands C2D::SetColorTag /
    /// SetFolderColor (proto 8). An older daemon would fail to decode the
    /// frame and drop the connection, so the Color submenu hides entirely
    /// during the install copy-race skew window.
    fn color_tags_supported(&self) -> bool {
        self.ipc.as_ref().is_some_and(|c| c.proto >= 8)
    }

    /// SLEEP: the connected daemon understands the proto-9 sleep verbs. An
    /// older daemon would fail to decode the frame and drop the connection,
    /// so every sleep entry point hides during the install copy-race skew
    /// window (the color-tag pattern).
    fn sleep_supported(&self) -> bool {
        self.ipc.as_ref().is_some_and(|c| c.proto >= 9)
    }

    /// SSH auto-reconnect: the connected daemon understands proto-10
    /// C2D::CancelReconnect (same skew-window pattern as sleep).
    fn reconnect_supported(&self) -> bool {
        self.ipc.as_ref().is_some_and(|c| c.proto >= 10)
    }

    /// The presented lifecycle state of a terminal (SLEEP S1).
    fn presented(&self, id: Uuid) -> PresentedStatus {
        self.state
            .terminal(id)
            .map(|t| presented_status(t.status, t.asleep))
            .unwrap_or(PresentedStatus::Dead)
    }

    /// SLEEP S7, GUI side: the busy evidence the confirm modal names — an
    /// open block's command, else output within the quiet window. Computed
    /// from the GUI's own mirrored state (blocks list + activity stamp);
    /// None = the sleep is instant and friction-free.
    fn sleep_gate_evidence(&self, id: Uuid) -> Option<SleepEvidence> {
        let quiet = self
            .activity
            .get(&id)
            .is_none_or(|s| s.last_output.elapsed() >= SLEEP_QUIET_WINDOW);
        if let Some(cmd) = self
            .blocks
            .get(&id)
            .and_then(|b| b.recs.iter().rev().find(|r| r.end_off.is_none()))
            .map(|r| r.cmd.clone())
        {
            // Sleep-freeze S7 refinement (mirrors the daemon's
            // sleep_busy_evidence): an open block whose grid sits QUIET on
            // the ALT SCREEN is an idle TUI at rest (claude typed at a
            // prompt, vim, htop) — the spec's own friction-free pass row.
            // The block only exists because of HOW the TUI was launched.
            let alt_quiet = quiet
                && self
                    .terms
                    .get(&id)
                    .is_some_and(|b| b.mode().contains(TermMode::ALT_SCREEN));
            if !alt_quiet {
                return Some(SleepEvidence::OpenBlock(cmd));
            }
        }
        if !quiet {
            return Some(SleepEvidence::OutputFlowing);
        }
        None
    }

    /// P6b: this terminal's derived shell family is Cmd — routes composer
    /// submissions through the SubmitCommand ledger, swaps the dirty-prompt
    /// clear chord to ESC, and refuses multi-line submission. Derived from
    /// the persisted program+args (static per terminal, D1).
    fn family_is_cmd(&self, id: Uuid) -> bool {
        self.state.terminal(id).is_some_and(|t| {
            matches!(
                crate::state::shell_family(&t.kind, &t.program, &t.args),
                crate::state::ShellFamily::Cmd
            )
        })
    }

    /// v0.1.1: this terminal's derived shell family is Ssh — gates the
    /// composer's pre-shell raw-conversation labels (password lock line,
    /// host-key line). Derived like `family_is_cmd`, stamped at composer
    /// creation.
    fn family_is_ssh(&self, id: Uuid) -> bool {
        self.state.terminal(id).is_some_and(|t| {
            matches!(
                crate::state::shell_family(&t.kind, &t.program, &t.args),
                crate::state::ShellFamily::Ssh { .. }
            )
        })
    }

    /// Tab completion (#24): the terminal's completion family (path
    /// namespace + quoting rules), derived like `family_is_cmd` and stamped
    /// on the composer at creation. Owns the distro for WSL posix↔local
    /// mapping.
    fn family_complete(&self, id: Uuid) -> complete::Family {
        self.state
            .terminal(id)
            .map(|t| complete::family_for(&shell_family(&t.kind, &t.program, &t.args)))
            .unwrap_or(complete::Family::Pwsh)
    }

    /// P6b §5.2: ship a Cmd-family submission. A proto ≥ 6 daemon gets the
    /// SubmitCommand ledger verb (it computes the submission bytes from its
    /// mirror, writes them, AND opens the synthetic block); an older daemon
    /// (the install copy-race window — it would drop the connection on an
    /// undecodable C2D variant) gets the plain P3 byte path: the command
    /// still runs, just unrecorded until the daemon is restarted from this
    /// build.
    fn send_cmd_submission(&mut self, id: Uuid, cmd: String) {
        if let Some(b) = self.terms.get_mut(&id) {
            b.scroll_to_bottom();
            b.note_input(); // v0.1.1: freeze a pending prompt-end upgrade
        }
        self.rearm_attention(id); // Bug A: user input re-arms amber
        if self.ipc.as_ref().is_some_and(|c| c.proto >= 6) {
            self.send(C2D::SubmitCommand {
                id,
                cmd,
                write: true,
            });
        } else if let Some(b) = self.terms.get(&id) {
            let bytes = composer::submission_bytes(b, &cmd);
            self.send(C2D::Input { id, bytes });
        }
    }

    // ───────────────────── QOL: drops, routed pastes, menu verbs ─────────────────────

    /// QOL §4.3: THE single drop router. Routes to the SELECTED terminal
    /// only (winit delivers no drop position on Windows — DO-NOT 6), only
    /// while the terminal view is showing. Family-aware translation +
    /// quoting; ssh refuses (the exactly-one #26 seam — its upload pipeline
    /// replaces that arm's body and inherits everything else here).
    fn route_file_drop(&mut self, paths: Vec<PathBuf>) {
        if self.central_view != CentralView::Terminal {
            return;
        }
        let Some(id) = self.selected else { return };
        let Some(t) = self.state.terminal(id) else { return };
        let running = presented_status(t.status, t.asleep) == PresentedStatus::Running;
        let fam = drop::drop_family(&shell_family(&t.kind, &t.program, &t.args));
        let compose = self
            .composers
            .get(&id)
            .is_some_and(|c| c.mode == ComposerMode::Compose);
        match drop::route_verdict(&fam, compose, running) {
            drop::RouteVerdict::Refuse => {
                // Dead/Asleep/Sleeping: zero bytes, zero spawns (sleep
                // inv. 5 — no input path may wake); the hover label
                // pre-explained.
            }
            // ssh-drop (#26): consent gate → upload queue. The paste routes
            // by composer mode at COMPLETION time (§7.1), not now.
            drop::RouteVerdict::SshUpload => {
                // §4: while ANY modal is open, new ssh drops no-op (the
                // consent modal covers exactly one pending batch).
                if self.modal.is_some() {
                    return;
                }
                // §3.3 pre-flight: directories + non-Unicode names refuse
                // with their own toast lines BEFORE consent — never consent
                // to something we'd then refuse.
                let (files, refused) = ssh_drop::preflight_partition(paths);
                if !refused.is_empty() {
                    let title = if files.is_empty() && refused.len() == 1 {
                        "nothing to upload".to_string()
                    } else {
                        format!(
                            "{} of {} won't upload",
                            refused.len(),
                            files.len() + refused.len()
                        )
                    };
                    self.toasts.push(toast::Toast {
                        kind: toast::ToastKind::Error,
                        title,
                        detail: refused,
                        ttl: Some(Duration::from_secs(8)),
                        action: None,
                    });
                }
                if files.is_empty() {
                    return;
                }
                if self.prefs.ssh_drop_skip_consent {
                    self.enqueue_ssh_drop(id, files);
                } else {
                    self.pending_ssh_drop = Some(PendingSshDrop {
                        terminal: id,
                        paths: files,
                        dont_ask_again: false,
                    });
                    self.modal = Some(Modal::SshDropConsent);
                }
            }
            // D8: drops are exempt from the §5 paste gate (paths never
            // carry newlines; a drop is a deliberate single-line insert).
            // Untranslatable paths (WSL foreign-UNC etc.) skip — the hover
            // label already counted them; None ⇒ nothing translated.
            drop::RouteVerdict::Draft => {
                if let Some(text) = drop::build_insert(&paths, &fam) {
                    if let Some(st) = self.composers.get_mut(&id) {
                        st.insert_dropped_text(&text);
                    }
                }
            }
            drop::RouteVerdict::Pty => {
                if let Some(text) = drop::build_insert(&paths, &fam) {
                    self.send_paste(id, &text);
                }
            }
        }
    }

    /// QOL §4.3 landing / the ssh-drop (#26) SEAM: text that already passed
    /// its gate lands where typing would land — an armed composer's DRAFT
    /// (pointer act: episode untouched, mode unchanged), else the PTY as
    /// paste-semantics bytes (real input: `on_raw_input` fires in
    /// `send_paste`, so the routing is truthful).
    fn insert_text_routed(&mut self, id: Uuid, text: &str) {
        if self
            .composers
            .get(&id)
            .is_some_and(|c| c.mode == ComposerMode::Compose)
        {
            if let Some(st) = self.composers.get_mut(&id) {
                st.insert_dropped_text(text);
            }
            return;
        }
        self.send_paste(id, text);
    }

    /// QOL §3.2/§6.1: menu Paste + middle-click paste — the gated route
    /// (P4: one gate, no bypass surface). Compose ⇒ draft (structurally
    /// safe: multi-line buffers visibly); raw ⇒ the §5 gate, then the PTY.
    fn route_paste(&mut self, id: Uuid, text: &str) {
        if self.presented(id) != PresentedStatus::Running {
            return; // dim states / sleep inv. 5 — input never wakes
        }
        let compose = self
            .composers
            .get(&id)
            .is_some_and(|c| c.mode == ComposerMode::Compose);
        if !compose {
            let bracketed = self
                .terms
                .get(&id)
                .is_some_and(|b| b.mode().contains(TermMode::BRACKETED_PASTE));
            if term_view::paste_needs_confirm(text, bracketed, self.prefs.paste_warn) {
                self.modal = Some(Modal::ConfirmPaste {
                    id,
                    text: text.to_string(),
                    dont_warn: false,
                });
                return;
            }
        }
        self.insert_text_routed(id, text);
    }

    /// Ship paste-semantics bytes to the PTY, encoding decided AT SEND TIME
    /// (§5 P5 — the mode may have flipped while a modal sat open). These are
    /// real PTY bytes: the composer episode is consumed exactly like any
    /// grid write (invariant 3's sanctioned asymmetry).
    fn send_paste(&mut self, id: Uuid, text: &str) {
        let Some(b) = self.terms.get_mut(&id) else { return };
        let bracketed = b.mode().contains(TermMode::BRACKETED_PASTE);
        b.scroll_to_bottom();
        b.note_input(); // v0.1.1: freeze a pending prompt-end upgrade
        let bytes = term_view::paste_bytes(bracketed, text);
        if let Some(st) = self.composers.get_mut(&id) {
            st.on_raw_input(Instant::now());
        }
        self.rearm_attention(id); // Bug A: user input re-arms amber
        self.send_input(id, bytes);
    }

    // ───────────────────── ssh-drop (#26): upload → paste ─────────────────────

    /// Queue a consented (or consent-exempt) drop batch (§6.2): one job, one
    /// sticky Progress toast whose ✕ cancels. Sequential per terminal (T13 —
    /// paste order == drop order), parallel across terminals.
    fn enqueue_ssh_drop(&mut self, id: Uuid, files: Vec<PathBuf>) {
        let Some(t) = self.state.terminal(id) else { return };
        let ShellFamily::Ssh { host } = shell_family(&t.kind, &t.program, &t.args) else {
            return;
        };
        let program = t.program.clone();
        let args = t.args.clone();
        let job_id = self.uploads.alloc_job();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
            .collect();
        let n = names.len();
        let mut title = if n == 1 {
            format!("uploading {} to {host}\u{2026}", names[0])
        } else {
            format!("uploading {n} files to {host}\u{2026}")
        };
        if self.uploads.busy(id) {
            title = format!("queued \u{2014} {title}");
        }
        let mut detail: Vec<String> = names.iter().take(4).cloned().collect();
        if n > 4 {
            detail.push(format!("+{} more", n - 4));
        }
        // A single filename already lives in the title; don't repeat it.
        if n == 1 {
            detail.clear();
        }
        let toast_id = self.toasts.push(toast::Toast {
            kind: toast::ToastKind::Progress,
            title,
            detail,
            ttl: None,
            action: Some(toast::ToastAction::CancelUpload(job_id)),
        });
        self.uploads.enqueue(ssh_drop::Job {
            job_id,
            terminal: id,
            host,
            program,
            args,
            files,
            toast: toast_id,
        });
    }

    /// §6.4: the worker→GUI event drain (runs in `logic()` beside the ipc
    /// drain). ALL toast morphs and the completion-time paste happen here,
    /// on the GUI thread.
    fn drain_uploads(&mut self, ctx: &egui::Context) {
        for ev in self.uploads.drain() {
            match ev {
                ssh_drop::Event::Done {
                    terminal,
                    job_id,
                    home,
                    verdicts,
                } => {
                    let Some(job) = self.uploads.finish(terminal, job_id) else {
                        continue;
                    };
                    let host = job.host.clone();
                    let total = verdicts.len();
                    let ok: Vec<String> = verdicts
                        .iter()
                        .filter_map(|(_, v)| v.as_ref().ok().cloned())
                        .collect();
                    let failed: Vec<(String, ssh_drop::FileErr)> = verdicts
                        .iter()
                        .filter_map(|(p, v)| {
                            v.as_ref().err().map(|e| {
                                (
                                    p.file_name()
                                        .map(|s| s.to_string_lossy().into_owned())
                                        .unwrap_or_default(),
                                    *e,
                                )
                            })
                        })
                        .collect();
                    if failed.is_empty() {
                        // All verified: the pasted path IS the feedback
                        // (inv. 7) — no success toast.
                        self.toasts.dismiss(job.toast);
                    } else if total == 1 {
                        // One file, one §7 row: its exact title + detail.
                        let (name, err) = &failed[0];
                        let (title, detail) = ssh_drop::file_err_toast(name, err, &host);
                        self.toasts.update(job.toast, |t| {
                            t.kind = toast::ToastKind::Error;
                            t.title = title;
                            t.detail = detail;
                            t.ttl = Some(Duration::from_secs(8));
                            t.action = None;
                        });
                    } else {
                        // Partial batch (T11): paste the successes, itemize
                        // the failures.
                        let title =
                            format!("{} of {} uploaded to {}", ok.len(), total, host);
                        let detail: Vec<String> = failed
                            .iter()
                            .take(5)
                            .map(|(name, err)| ssh_drop::file_err_line(name, err, &host))
                            .collect();
                        self.toasts.update(job.toast, |t| {
                            t.kind = toast::ToastKind::Error;
                            t.title = title;
                            t.detail = detail;
                            t.ttl = Some(Duration::from_secs(8));
                            t.action = None;
                        });
                    }
                    if !ok.is_empty() {
                        self.deliver_remote_paths(ctx, terminal, &home, &ok);
                    }
                    self.start_next_upload(terminal);
                }
                ssh_drop::Event::ConnFailed {
                    terminal,
                    job_id,
                    err,
                } => {
                    let Some(job) = self.uploads.finish(terminal, job_id) else {
                        continue;
                    };
                    let names: Vec<String> = job
                        .files
                        .iter()
                        .map(|p| {
                            p.file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        })
                        .collect();
                    let (title, detail) = ssh_drop::conn_err_toast(&err, &job.host, &names);
                    self.toasts.update(job.toast, |t| {
                        t.kind = toast::ToastKind::Error;
                        t.title = title;
                        t.detail = detail;
                        t.ttl = Some(Duration::from_secs(8));
                        t.action = None;
                    });
                    self.start_next_upload(terminal);
                }
                ssh_drop::Event::Cancelled { terminal, job_id } => {
                    let Some(job) = self.uploads.finish(terminal, job_id) else {
                        continue;
                    };
                    self.cancelled_toast(job.toast);
                    self.start_next_upload(terminal);
                }
            }
        }
    }

    /// §7 row 12 (T15): cancelled ⇒ zero observable side effects.
    fn cancelled_toast(&mut self, id: toast::ToastId) {
        self.toasts.update(id, |t| {
            t.kind = toast::ToastKind::Info;
            t.title = "upload cancelled".into();
            t.detail = vec!["nothing was pasted".into()];
            t.ttl = Some(Duration::from_secs(5));
            t.action = None;
        });
    }

    /// §7.1/§6.9: paste the verified remote paths at COMPLETION time through
    /// the qol router (Compose ⇒ draft, raw ⇒ paste bytes). A terminal that
    /// died/slept/vanished mid-upload gets the clipboard fallback — input
    /// never wakes anything (sleep inv. 5).
    fn deliver_remote_paths(
        &mut self,
        ctx: &egui::Context,
        terminal: Uuid,
        home: &str,
        names: &[String],
    ) {
        let text = ssh_drop::paste_text(home, names);
        if self.presented(terminal) == PresentedStatus::Running {
            self.insert_text_routed(terminal, &text);
        } else {
            ctx.copy_text(text);
            self.toasts.push(toast::Toast {
                kind: toast::ToastKind::Info,
                title: "uploaded \u{2014} terminal closed".into(),
                detail: vec!["remote paths copied to clipboard".into()],
                ttl: Some(Duration::from_secs(5)),
                action: None,
            });
        }
    }

    /// Start the terminal's next queued job and un-queue its toast title.
    fn start_next_upload(&mut self, terminal: Uuid) {
        if let Some(tid) = self.uploads.start_next(terminal) {
            self.toasts.update(tid, |t| {
                if let Some(rest) = t.title.strip_prefix("queued \u{2014} ") {
                    t.title = rest.to_string();
                }
            });
        }
    }

    /// End-of-frame toast stack (§5): bottom-right of the central area,
    /// lifted over the composer strip when the shown terminal reserves it.
    /// Dispatches the one action a click requested.
    fn toasts_ui(&mut self, ctx: &egui::Context) {
        let Some(mut anchor) = self.central_rect else { return };
        if self.central_view == CentralView::Terminal
            && self.selected.is_some_and(|id| self.hooked(id))
        {
            anchor.max.y -= composer::STRIP_H;
        }
        let interactive = self.modal.is_none();
        match self.toasts.show(ctx, anchor, interactive) {
            Some(toast::ToastAction::CancelUpload(job)) => {
                // A queued job dies right here (no worker to speak for it);
                // a running one is killed and its worker reports Cancelled.
                if let Some(tid) = self.uploads.cancel(job) {
                    self.cancelled_toast(tid);
                }
            }
            None => {}
        }
    }

    /// QOL §3.2: the Find row / titlebar toggle body — open the scrollback
    /// search (one floating surface at a time).
    fn open_search(&mut self) {
        self.search = Some(SearchState::new());
        self.history = None;
        self.launcher = None;
    }

    /// QOL §7.2: view-only clear — the local ring only; daemon mirror,
    /// journal and blocks sidecar never hear about it (a reattach restores
    /// the history — the deliberate v1 contract). Search matches invalidate
    /// through the existing history-drift path (matches_history mismatch).
    fn clear_scrollback(&mut self, id: Uuid) {
        if let Some(b) = self.terms.get_mut(&id) {
            b.clear_scrollback_view();
        }
    }

    /// QOL §3.3: the local directory a terminal's cwd maps to, for Explorer
    /// and gating the menu row. None ⇒ no local directory exists (ssh).
    fn resolve_local_cwd(&self, id: Uuid) -> Option<PathBuf> {
        let t = self.state.terminal(id)?;
        local_cwd_for(
            &shell_family(&t.kind, &t.program, &t.args),
            t.live_cwd.as_deref(),
            &t.cwd,
        )
    }

    /// QOL §7.1: Duplicate terminal — build a fresh spec from the row's meta
    /// and create it (auto-selected via the launcher's pending_create
    /// machinery). NOT a launcher choice: sticky spawn prefs untouched.
    fn duplicate_terminal(&mut self, t: &TerminalMeta) {
        let mut nt = {
            let taken: Vec<&str> =
                self.state.terminals.iter().map(|s| s.name.as_str()).collect();
            duplicate_spec(t, &taken)
        };
        self.stamp_wsl_banner(&mut nt);
        let name = nt.name.clone();
        self.send(C2D::CreateTerminal { spec: nt });
        self.pending_create = Some((name, Instant::now()));
    }

    fn reconnect_if_needed(&mut self, ctx: &egui::Context) {
        // #34 Axis 7 step 2 (CRITICAL): while an update apply is in flight
        // (backup → quiesce → Update.exe handoff) this loop must NOT
        // resurrect the freshly-quiesced OLD daemon — the new GUI's bin-sync
        // deploys and the normal path respawns it after the swap.
        if self.updates.applying() {
            return;
        }
        // Alive means connected AND recently heard from: a half-open socket that
        // has gone quiet for >30s is treated as dead so we force a reconnect (R7).
        let alive = self
            .ipc
            .as_ref()
            .is_some_and(|c| c.is_connected() && c.silent_secs() <= 30);
        if alive {
            return;
        }
        if self.last_connect.elapsed() < Duration::from_secs(2) {
            ctx.request_repaint_after(Duration::from_millis(500));
            return;
        }
        self.last_connect = Instant::now();
        match ipc::connect_or_spawn(ctx.clone()) {
            Ok(client) => {
                // A different pid means the daemon was restarted; its terminals
                // were restored from journals (R8a).
                if let Some(prev) = self.last_daemon_pid {
                    if prev != client.pid {
                        self.notice = Some((
                            "Daemon restarted \u{2014} sessions restored from journal.".into(),
                            Instant::now(),
                        ));
                    }
                }
                self.last_daemon_pid = Some(client.pid);
                self.ipc = Some(client);
                // Fresh connection: rebuild every screen from journal replay.
                self.terms.clear();
                self.blocks.clear();
                self.history = None;
                self.blocks_stamp = self.blocks_stamp.wrapping_add(1);
                self.unread.clear();
                self.activity.clear();
                self.attention_flashed.clear();
                // Composers are NOT cleared — drafts survive a reconnect
                // (D8) — but every latch resets so arming waits for live
                // hooks from the new connection.
                for st in self.composers.values_mut() {
                    st.on_reset();
                }
                if let Some(p) = &mut self.perf3 {
                    // New connection ⇒ new attach cycle; stale pending ids
                    // from the dead connection would poison replays_done.
                    p.pending.clear();
                    p.cycle_t0 = None;
                    log::info!("[perf] gui reconnected ms={}", gui_ms());
                }
            }
            Err(_) => {
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }
    }

    /// Apply pending daemon frames, bounded per UI frame: a flood can queue
    /// tens of MB while the GUI is occluded (logic ticks at ~1Hz there), and
    /// parsing it all at once would stall the frame for hundreds of ms.
    /// Order is preserved — when the budget runs out we simply stop and
    /// request another repaint, so the queue always drains.
    fn drain_ipc(&mut self, ctx: &egui::Context) {
        const PARSE_BUDGET: usize = 2 * 1024 * 1024;
        // The budget is bytes AND time: parse rate is ~13ms/MB on a fast CPU
        // (worse on a mid one), so a full 2MiB budget could spend 26-50ms in
        // one frame during attach/resync storms or an un-minimize drain. The
        // exhaustion path below already defers losslessly (request_repaint +
        // FIFO carry-over), so slicing at ~6ms turns one heavy frame into a
        // few smooth ones — same bytes, same order.
        //
        // BOOT-CYCLE PACING (perf-wave-4, diagnosed but deliberately NOT
        // "fixed" here): during a GUI cold start the attach replays sit
        // parsed-ready in this channel while drain frames land ~55-88ms
        // apart — the daemon delivers all N replays in 12ms (N=10) / 36ms
        // (N=30) and total parse CPU is 27/85ms, yet the cycle takes
        // 0.2-0.9s. That stall is NOT this slice policy and NOT
        // repaint-request starvation (TC_DIAG_SPIN=1 requests a repaint
        // every frame and changes nothing): a minimal eframe 0.35/DX12/
        // LOW_LATENCY app with ZERO app logic shows the same ~88ms
        // inter-frame present cadence when visible-but-unfocused vs ~31ms
        // when activated — eframe/winit/DWM present pacing of
        // non-foreground windows (AutoVsync + max_frame_latency=1) is the
        // suspect. Agent rigs cannot hold real user foreground focus, so
        // whether a genuinely-clicked-open window collapses to vsync pacing
        // is the open question: diagnose on a real focused cold start (the
        // [perf] replay lines ship in the binary) BEFORE touching anything.
        // If gaps survive real focus, the sanctioned fallback is a larger
        // slice (≤12ms) gated STRICTLY on the boot/reconnect pending set —
        // steady-state must keep 6ms (r3-2), and present-mode experiments
        // stay behind an env knob, never default. The selected terminal is
        // unaffected either way (painted at snapshot+38ms, N-invariant).
        const PARSE_SLICE: Duration = Duration::from_millis(6);
        let t0 = Instant::now();
        let mut parsed = 0usize;
        loop {
            if parsed >= PARSE_BUDGET || t0.elapsed() >= PARSE_SLICE {
                ctx.request_repaint();
                break;
            }
            let (arrived, msg) = {
                let Some(ipc) = &self.ipc else { return };
                match ipc.rx.try_recv() {
                    Ok(pair) => {
                        ipc.note_drained(&pair.1); // r2-M3 byte accounting
                        pair
                    }
                    Err(_) => break,
                }
            };
            // Trace only echo-sized frames: bulk (attach replays, floods) is
            // intentionally throttled and would drown the typing signal.
            if let (Some(t), D2C::Output { bytes, .. }) = (&mut self.trace, &msg) {
                if bytes.len() <= 1024 {
                    t.sched_us
                        .push(arrived.elapsed().as_micros().min(u64::MAX as u128) as u64);
                }
            }
            match msg {
                D2C::Snapshot { state } => self.apply_snapshot(state),
                D2C::Replay { id, bytes } => {
                    // Journal tail: replay it into the grid but don't count it as
                    // live activity (it's historical, delivered on attach).
                    parsed += bytes.len();
                    if let Some(backend) = self.terms.get_mut(&id) {
                        let t0 = self.perf3.is_some().then(Instant::now);
                        backend.advance(&bytes);
                        // Unread baseline: the reconstruction IS the acked
                        // state — restore/boot replay must come up quiet, and
                        // the first LIVE change afterwards is judged against
                        // this real content instead of a blind adoption.
                        let digest = backend.grid_digest();
                        let gen = backend.feed_gen;
                        let ast = self.activity.entry(id).or_insert_with(ActivityState::new);
                        ast.unread_base = Some(digest);
                        ast.unread_base_gen = gen;
                        ast.unread_pending = false;
                        ast.unread_adopt = false;
                        if let (Some(p), Some(t0)) = (&mut self.perf3, t0) {
                            let us = t0.elapsed().as_micros() as u64;
                            p.parse_us += us;
                            log::info!(
                                "[perf] gui replay id={id} bytes={} parse_us={us} ms={}",
                                bytes.len(),
                                gui_ms()
                            );
                            if Some(id) == self.selected {
                                p.paint_selected = true;
                            }
                            if p.pending.remove(&id) && p.pending.is_empty() {
                                p.paint_all = true;
                                log::info!(
                                    "[perf] gui replays_done parse_us_total={} cycle_ms={} ms={}",
                                    p.parse_us,
                                    p.cycle_t0.map(|t| t.elapsed().as_millis()).unwrap_or(0),
                                    gui_ms()
                                );
                            }
                        }
                    }
                }
                D2C::Output { id, bytes } => {
                    parsed += bytes.len();
                    // Font-step settle tracking: output arriving after the
                    // commit is (in a quiet staging corpus) the conhost
                    // post-resize repaint — the last visual change of the
                    // transition.
                    if let Some(fp) = &mut self.font_perf {
                        if fp.committed.is_some() {
                            fp.last_activity = Instant::now();
                        }
                    }
                    let selected = self.selected;
                    let counters = if let Some(backend) = self.terms.get_mut(&id) {
                        // Live path: parse + block-scan + offset-count (P2).
                        // Replay stays advance() — a reconstruction contains
                        // hook OSCs but is NOT journal bytes.
                        backend.advance_live(&bytes);
                        backend
                            .block_feed
                            .as_ref()
                            .map(|f| (f.pre_seen, f.exec_seen))
                    } else {
                        continue;
                    };
                    // Composer latch pump (P3): counter diffs drive the
                    // prompt latch / dismissal for EVERY terminal, selected
                    // or not — O(events), not per-frame.
                    if let (Some((pre, exec)), Some(st)) =
                        (counters, self.composers.get_mut(&id))
                    {
                        st.on_stream_events(pre, exec, Instant::now());
                    }
                    // V-A / unread ack: live output on an unwatched terminal
                    // only QUEUES an unread check — update_activity runs the
                    // verdict once the stream settles (UNREAD_SETTLE) by
                    // diffing the grid against the acked baseline, so chrome
                    // bytes (Claude's updater status paint/erase, statusline
                    // timer ticks, title-only OSCs, cursor churn) never light
                    // the dot. Post-attach/Reset repaints share the CLI
                    // suppression window: a boot repaint never even queues.
                    let now = Instant::now();
                    let st = self.activity.entry(id).or_insert_with(ActivityState::new);
                    if selected != Some(id) {
                        if now >= st.cli_suppress_until {
                            st.unread_pending = true;
                        } else {
                            st.unread_adopt = true;
                        }
                    }
                    st.last_output = now;
                    // task #22: live output arms a CLI streaming episode —
                    // except inside the post-attach window, where the
                    // attach-resize conhost repaint would arm a false
                    // "needs you" on every boot. Viewing consumes it.
                    if now >= st.cli_suppress_until {
                        st.cli_stream = true;
                    }
                }
                D2C::Reset { id } => {
                    // The daemon rewrote this terminal's world (restore); a
                    // fresh serialized Replay follows — via our own re-Attach
                    // below (proto ≥ 12), or the legacy daemon push. Start
                    // from a blank grid sized like the others (per-terminal:
                    // hooked terminals reserve the composer strip). With no
                    // committed layout
                    // (occluded/boot GUI) fall back to the terminal's last
                    // known PTY size, never the 160×42 default (Bug B: the
                    // default-size flap resized real PTYs on every boot).
                    let boot_size = self
                        .state
                        .terminals
                        .iter()
                        .find(|t| t.id == id)
                        .filter(|t| t.last_cols >= 2 && t.last_rows >= 2)
                        .map(|t| GridSize {
                            cols: t.last_cols.min(1000),
                            rows: t.last_rows.min(1000),
                            ..GridSize::default()
                        })
                        .unwrap_or_default();
                    let mut backend =
                        TermBackend::with_scrollback(boot_size, self.prefs.scrollback_lines);
                    if let Some((layout, cell)) = self.last_grid {
                        let _ = backend.resize_to(self.layout_for(id, layout), cell);
                    }
                    let (cols, rows) = (backend.size.cols, backend.size.rows);
                    self.terms.insert(id, backend);
                    // WIDTH-MISMATCH GARBLE FIX: re-announce OUR real grid by
                    // re-attaching (the sleep-Exited arm's proven pattern) —
                    // a proto ≥ 12 daemon suppressed its blind-size Replay
                    // push for us and its attach arm resizes ConPTY to these
                    // dims BEFORE serializing, so the replay parses
                    // width-correct and the PTY is never stranded at the
                    // pre-restore width (the restored-claude field garble:
                    // 175-col replay bytes parsed into this 147-col grid,
                    // silently, with the show-heal no-oping forever after).
                    // An older daemon still pushes its own Replay; a
                    // re-attach would then DOUBLE the scrollback, so this
                    // gates on the daemon generation.
                    if self.ipc.as_ref().is_some_and(|c| c.proto >= 12) {
                        // Search matches indexed the replaced grid — drop
                        // honestly (the r2-M1 shrink's rule).
                        if self.selected == Some(id) {
                            self.search = None;
                        }
                        self.send(C2D::Attach { id, cols, rows });
                    }
                    // Composer: fresh session — draft kept, latches cleared;
                    // the new session's first live `pre` re-arms (§2.4).
                    if let Some(st) = self.composers.get_mut(&id) {
                        st.on_reset();
                    }
                    // task #22: the respawn's banner/prompt render is not a
                    // CLI streaming episode — re-open the suppression window.
                    let ast = self.activity.entry(id).or_insert_with(ActivityState::new);
                    ast.cli_suppress_until = Instant::now() + CLI_ATTACH_SUPPRESS;
                    ast.cli_stream = false;
                    // Unread: the daemon rewrote this terminal's world — the
                    // stale baseline names dead content. Drop it; the fresh
                    // serialized Replay (following this Reset) re-seeds it,
                    // so the restore never marks unread (post-update boots
                    // come up with zero dots).
                    ast.unread_base = None;
                    ast.unread_pending = false;
                    ast.unread_adopt = false;
                    // Bug A: a fresh session may alert once (respawn is a
                    // deliberate user act, like boot).
                    ast.armed = true;
                }
                D2C::Error { message } => {
                    self.last_error = Some((message, Instant::now()));
                }
                D2C::Blocks { id, epoch, full, recs } => {
                    // full replaces the list; incrementals binary-search
                    // upsert by start_off (unique across epochs — offsets
                    // are monotonic per terminal).
                    let list = self.blocks.entry(id).or_default();
                    list.epoch = list.epoch.max(epoch);
                    if full {
                        list.recs = recs;
                    } else {
                        for r in recs {
                            match list
                                .recs
                                .binary_search_by_key(&r.start_off, |x| x.start_off)
                            {
                                Ok(i) => list.recs[i] = r,
                                Err(i) => list.recs.insert(i, r),
                            }
                        }
                    }
                    list.dirty = true;
                    // History-index invalidation (P4 D11): one integer bump
                    // per Blocks frame — the popup rebuilds lazily on drift.
                    self.blocks_stamp = self.blocks_stamp.wrapping_add(1);
                    // epoch > 0 ⇔ a hooked spawn exists: turn the backend's
                    // scanner on. The full sync rides the same client queue
                    // as Replay/StreamPos, so this lands before the first
                    // live hook byte.
                    if list.epoch > 0 {
                        if let Some(b) = self.terms.get_mut(&id) {
                            b.enable_block_scan();
                        }
                        // Hooked spawn exists ⇒ this terminal gets a composer
                        // (P3). Hookless terminals never reach this line.
                        // P6b: stamp the family verdict — Cmd routes its
                        // submissions through the SubmitCommand ledger and
                        // swaps the clear-chord/multi-line rules. Idempotent
                        // (family derives from persisted program+args).
                        // SLEEP: stamp the asleep flag too, so a composer
                        // created by an attach to an already-asleep terminal
                        // gates Blocked(Asleep) from its first tick.
                        let is_cmd = self.family_is_cmd(id);
                        let is_ssh = self.family_is_ssh(id);
                        let fam = self.family_complete(id);
                        let (asleep, reconnecting) = self
                            .state
                            .terminal(id)
                            .map(|t| (t.asleep, t.reconnecting))
                            .unwrap_or((false, false));
                        let st = self.composers.entry(id).or_default();
                        st.is_cmd = is_cmd;
                        st.is_ssh = is_ssh;
                        st.fam = fam;
                        st.asleep = asleep;
                        st.reconnecting = reconnecting;
                    }
                }
                D2C::StreamPos { id, off } => {
                    // Absolute journal offset where live Output resumes:
                    // the base for anchor↔record joins (P2 §3).
                    if let Some(b) = self.terms.get_mut(&id) {
                        b.set_stream_pos(off);
                    }
                }
                D2C::BlockText {
                    id: _,
                    start_off: _,
                    text,
                    truncated,
                } => {
                    // Reply to our Copy-output request: straight to clipboard.
                    ctx.copy_text(text);
                    self.notice = Some((
                        if truncated {
                            "Block output copied (truncated).".into()
                        } else {
                            "Block output copied.".into()
                        },
                        Instant::now(),
                    ));
                }
                D2C::Exited { id, .. } => {
                    // SLEEP §7.3: the flag Snapshot precedes the kill's
                    // Exited on the same queue, so the meta is already
                    // truthful here — re-stamp (belt) and let on_exited pick
                    // Raw(Asleep) over Raw(Dead). Draft kept either way.
                    let asleep = self.state.terminal(id).is_some_and(|t| t.asleep);
                    if let Some(st) = self.composers.get_mut(&id) {
                        st.asleep = asleep;
                        st.on_exited();
                    }
                    // SLEEP freeze-frame: a sleep's Exited means the daemon
                    // world (journal + frame sidecar) is now the durable
                    // asleep view — re-attach through the dead-attach path,
                    // swapping the live-parsed grid for the serialized
                    // scrollback underlay + the frozen alt frame. Fanout was
                    // muted at the kill, so the old grid already shows the
                    // pre-wipe frame; the swap is content-equivalent (and it
                    // heals any pre-mute drain tail) — one message, zero new
                    // wire variants, every GUI re-attaches itself. Fresh
                    // backend exactly like the D2C::Reset arm (atomic swap);
                    // composer latches were gated by on_exited above.
                    if asleep && self.terms.contains_key(&id) {
                        let boot_size = self
                            .state
                            .terminals
                            .iter()
                            .find(|t| t.id == id)
                            .filter(|t| t.last_cols >= 2 && t.last_rows >= 2)
                            .map(|t| GridSize {
                                cols: t.last_cols.min(1000),
                                rows: t.last_rows.min(1000),
                                ..GridSize::default()
                            })
                            .unwrap_or_default();
                        let mut backend =
                            TermBackend::with_scrollback(boot_size, self.prefs.scrollback_lines);
                        if let Some((layout, cell)) = self.last_grid {
                            let _ = backend.resize_to(self.layout_for(id, layout), cell);
                        }
                        let (cols, rows) = (backend.size.cols, backend.size.rows);
                        self.terms.insert(id, backend);
                        // Search matches indexed the replaced grid — drop
                        // honestly (the r2-M1 shrink's rule).
                        if self.selected == Some(id) {
                            self.search = None;
                        }
                        self.send(C2D::Attach { id, cols, rows });
                    }
                }
                D2C::ReplayAnchors { id, items } => {
                    // Restored-history hints (proto 7): join block hints to
                    // their records by start_off (spoofed/stale offsets match
                    // nothing and vanish), then let the backend re-verify each
                    // row against its own grid and mint the covers + anchors.
                    let hints: Vec<term_backend::ReplayHint> = {
                        let recs = self
                            .blocks
                            .get(&id)
                            .map(|b| b.recs.as_slice())
                            .unwrap_or(&[]);
                        items
                            .into_iter()
                            .filter_map(|it| match it.kind {
                                crate::protocol::ANCHOR_BLOCK => {
                                    let ri = recs
                                        .binary_search_by_key(&it.start_off, |r| r.start_off)
                                        .ok()?;
                                    let rec = &recs[ri];
                                    Some(term_backend::ReplayHint {
                                        start_off: it.start_off,
                                        row: it.row,
                                        col: it.col as usize,
                                        cmd: Some(rec.cmd.clone()),
                                        cwd: rec
                                            .cwd
                                            .as_ref()
                                            .map(|p| p.to_string_lossy().into_owned()),
                                    })
                                }
                                crate::protocol::ANCHOR_SPACER => {
                                    Some(term_backend::ReplayHint {
                                        start_off: it.start_off,
                                        row: it.row,
                                        col: it.col as usize,
                                        cmd: None,
                                        cwd: None,
                                    })
                                }
                                _ => None,
                            })
                            .collect()
                    };
                    if let Some(b) = self.terms.get_mut(&id) {
                        b.apply_replay_hints(hints);
                    }
                }
                D2C::PromptState {
                    id,
                    at_prompt,
                    line,
                    col,
                    clean: _,
                } => {
                    // Cold-attach arm (task #15): the daemon certified this
                    // session's prompt state at attach. Seed the backend's
                    // prompt_end from the replay-space cell and latch the
                    // composer as a live `pre` would — the gate then auto-arms
                    // with the cover on IF the replayed cursor sits exactly at
                    // the seeded prompt end (its own clean check, which is
                    // strictly truer than the daemon's `clean` after our own
                    // resize), else ManualOnly. Arrives after Blocks, so the
                    // feed exists and scanning is on.
                    if at_prompt {
                        if let Some(b) = self.terms.get_mut(&id) {
                            b.seed_prompt_end(line, col as usize);
                        }
                        if let Some(st) = self.composers.get_mut(&id) {
                            st.on_attach_prompt(Instant::now());
                        }
                    }
                }
                D2C::Pong => {}
                // P5 controller-channel replies are addressed to warpctrl
                // clients, not the terminal view — the GUI ignores them.
                D2C::Ctl { .. } => {}
            }
        }
    }

    fn apply_snapshot(&mut self, state: SharedState) {
        self.state = state;
        self.state_gen += 1;
        let ids: HashSet<Uuid> = self.state.terminals.iter().map(|t| t.id).collect();

        // Attach anything new, announcing our grid size so the daemon brings
        // the session to it BEFORE serializing (exact cursor placement) — no
        // separate Resize needed.
        //
        // r2 boot-perf 2: the order used to be HashSet-random, so the
        // selected tab could paint LAST in the cycle (measured up to ~800ms
        // after snapshot at 25 terminals). Attach the to-be-selected
        // terminal FIRST — reconnect: the current selection; cold boot: the
        // sidebar's first (exactly the selection default applied below) —
        // then the rest in sidebar order for a top-down fill.
        let ordered = {
            let mut ordered = self.sorted_terminal_ids();
            let first = self
                .selected
                .filter(|s| ids.contains(s))
                .or_else(|| ordered.first().copied());
            if let Some(first) = first {
                if let Some(pos) = ordered.iter().position(|&x| x == first) {
                    let f = ordered.remove(pos);
                    ordered.insert(0, f);
                }
            }
            ordered
        };
        let mut to_attach = Vec::new();
        for id in &ordered {
            if !self.terms.contains_key(id) {
                // Cold boot (no layout committed yet): start the backend at
                // the terminal's LAST KNOWN PTY size from the snapshot meta,
                // NOT GridSize::default. Attaching at the 160×42 default made
                // every GUI start resize every PTY to 42 rows and back
                // seconds later (journal-proven): conhost repaint storms,
                // stale-stamped repaints, mirror grow churn — the raw
                // material of the Bug-B grid corruption. At the meta size the
                // attach-resize is a NO-OP; the one corrective resize follows
                // when the real window layout lands.
                let meta = self.state.terminals.iter().find(|t| t.id == *id);
                let boot_size = meta
                    .filter(|t| t.last_cols >= 2 && t.last_rows >= 2)
                    .map(|t| GridSize {
                        cols: t.last_cols.min(1000),
                        rows: t.last_rows.min(1000),
                        ..GridSize::default()
                    })
                    .unwrap_or_default();
                // Tripwire (field 160×42 attach-yank forensics): the daemon
                // now writes the spawn size into meta at launch, so an
                // unknown grid for a running session should be impossible —
                // if this ever fires again, the log names the culprit meta.
                if meta.is_none_or(|t| t.last_cols < 2 || t.last_rows < 2) {
                    log::warn!(
                        "[attach] {id} snapshot meta grid unknown (cols={} rows={}) — attaching at the {}x{} default",
                        meta.map(|t| t.last_cols).unwrap_or(0),
                        meta.map(|t| t.last_rows).unwrap_or(0),
                        boot_size.cols,
                        boot_size.rows,
                    );
                }
                let mut backend =
                    TermBackend::with_scrollback(boot_size, self.prefs.scrollback_lines);
                if let Some((layout, cell)) = self.last_grid {
                    // Per-terminal geometry (P3 §7): with meta.hooked (proto
                    // 5) the strip reservation is known here, so the first
                    // attach already announces the shrunk grid and the
                    // corrective Resize is a no-op (pre-proto-5 terminals
                    // still take the one corrective flip).
                    let _ = backend.resize_to(self.layout_for(*id, layout), cell);
                }
                to_attach.push((*id, backend.size.cols, backend.size.rows));
                self.terms.insert(*id, backend);
            }
        }
        for &(id, cols, rows) in &to_attach {
            self.send(C2D::Attach { id, cols, rows });
        }
        if let Some(p) = &mut self.perf3 {
            if !to_attach.is_empty() {
                if p.pending.is_empty() {
                    p.cycle_t0 = Some(Instant::now());
                    p.parse_us = 0;
                }
                p.pending.extend(to_attach.iter().map(|&(id, ..)| id));
                log::info!(
                    "[perf] gui snapshot terms={} new_attach={} ms={}",
                    ids.len(),
                    to_attach.len(),
                    gui_ms()
                );
            }
        }

        // Drop anything deleted.
        self.terms.retain(|id, _| ids.contains(id));
        self.previews.retain(|id, _| ids.contains(id));
        // r2-M4: these two were never pruned against snapshot ids (only
        // wholesale-cleared on reconnect) — a deletion leaked their entries
        // for the GUI's lifetime.
        self.activity.retain(|id, _| ids.contains(id));
        self.attention_flashed.retain(|id| ids.contains(id));
        // r4 perf-gui L3: a snapshot can change a terminal's program/args/
        // inner_cli (its consent lane) — drop the settled skip cache and let
        // the next scan re-derive it (one linear pass, snapshots are rare).
        self.claude_consent_settled.clear();
        self.codex_consent_settled.clear();
        let blocks_before = self.blocks.len();
        self.blocks.retain(|id, _| ids.contains(id));
        if self.blocks.len() != blocks_before {
            // Deleted terminals' history dies with Delete (P4 D6): the next
            // popup frame rebuilds without their rows.
            self.blocks_stamp = self.blocks_stamp.wrapping_add(1);
        }
        self.composers.retain(|id, _| ids.contains(id));
        // r2-M1: asleep/dead grids keep a full 10k-line history that the
        // wake/restart Reset discards anyway (fresh backend, replay-rebuilt)
        // — shrink them to the replay ceiling and truly free the rows
        // (up to ~35MB per saturated 158-col terminal). Idempotent; fires
        // once on the transition.
        for t in &self.state.terminals {
            if t.asleep || t.status == crate::state::TermStatus::Dead {
                if let Some(b) = self.terms.get_mut(&t.id) {
                    if b.shrink_history_for_idle() && self.selected == Some(t.id) {
                        // Search matches index rows that may just have been
                        // freed — drop the search, honestly.
                        self.search = None;
                    }
                }
            }
        }
        // SLEEP: every Snapshot refreshes the composers' asleep stamp (the
        // capture-on-change flag rides Snapshot — multi-GUI coherent); the
        // gate then blocks/unblocks on the next tick.
        {
            let asleep_ids: HashSet<Uuid> = self
                .state
                .terminals
                .iter()
                .filter(|t| t.asleep)
                .map(|t| t.id)
                .collect();
            let reconnecting_ids: HashSet<Uuid> = self
                .state
                .terminals
                .iter()
                .filter(|t| t.reconnecting)
                .map(|t| t.id)
                .collect();
            for (id, st) in self.composers.iter_mut() {
                st.asleep = asleep_ids.contains(id);
                st.reconnecting = reconnecting_ids.contains(id);
            }
        }
        self.unread.retain(|id| ids.contains(id));
        // Inline-rename / drag targets that vanished with this snapshot die
        // with it (task #22).
        if let Some(rn) = &self.renaming {
            let alive = match rn.target {
                RenameTarget::Term(id) => ids.contains(&id),
                RenameTarget::Folder(id) => self.state.folders.iter().any(|f| f.id == id),
            };
            if !alive {
                self.renaming = None;
            }
        }
        let drag_alive = match self.drag.as_ref().map(|d| &d.payload) {
            Some(DragPayload::Term { id, .. }) => ids.contains(id),
            Some(DragPayload::Folder { id }) => {
                self.state.folders.iter().any(|f| f.id == *id)
            }
            None => true,
        };
        if !drag_alive {
            self.drag = None;
        }
        if self.selected.is_some_and(|id| !ids.contains(&id)) {
            self.selected = None;
            // The popup's anchor (the selected terminal's strip) died.
            self.history = None;
            // B1: the auto-select below bypasses select_terminal, so the
            // cross-terminal panels it would drop are cleared here — search
            // matches never carry across terminals (a stale list drives the
            // counter/step/current-highlight against the new grid whenever
            // history sizes coincide, e.g. two capped terminals).
            self.search = None;
            self.blocks_panel = None;
        }
        // The §6.1 empty-state embed dies with the empty state.
        if !self.state.terminals.is_empty()
            && self.launcher.as_ref().is_some_and(|l| l.embedded)
        {
            self.launcher = None;
        }
        // Auto-select our own pending create (D4/§3.2): join by (new id,
        // exact name), newest order wins; 5s expiry covers a refused create
        // or a raced rename — then we silently stop retargeting.
        if let Some((name, t0)) = self.pending_create.clone() {
            if t0.elapsed() > launcher::PENDING_EXPIRY {
                self.pending_create = None;
            } else {
                let hit = {
                    let newly: Vec<(Uuid, &str, i64)> = self
                        .state
                        .terminals
                        .iter()
                        .filter(|t| to_attach.iter().any(|&(id, ..)| id == t.id))
                        .map(|t| (t.id, t.name.as_str(), t.order))
                        .collect();
                    launcher::resolve_pending(&name, &newly)
                };
                if let Some(id) = hit {
                    self.pending_create = None;
                    self.select_terminal(id);
                }
            }
        }
        if self.selected.is_none() {
            self.selected = self.sorted_terminal_ids().first().copied();
        }
    }

    /// The terminal to select after `id` is deleted: the next one, else the
    /// previous, else none.
    fn neighbor_of(&self, id: Uuid) -> Option<Uuid> {
        let ids = self.sorted_terminal_ids();
        let pos = ids.iter().position(|&x| x == id)?;
        if pos + 1 < ids.len() {
            Some(ids[pos + 1])
        } else if pos > 0 {
            Some(ids[pos - 1])
        } else {
            None
        }
    }

    /// The sidebar row cache, rebuilt only when a Snapshot replaced `state`.
    /// Returning the Rc lets callers iterate rows while `&mut self` methods
    /// run (the reason the old code deep-cloned every meta per frame).
    fn sidebar_rows_current(&mut self) -> std::rc::Rc<SidebarRows> {
        if self.sidebar_rows.gen != self.state_gen {
            self.sidebar_rows = std::rc::Rc::new(build_sidebar_rows(&self.state, self.state_gen));
        }
        self.sidebar_rows.clone()
    }

    fn sorted_terminal_ids(&self) -> Vec<Uuid> {
        let mut folders = self.state.folders.clone();
        folders.sort_by_key(|f| f.order);
        let mut ids = Vec::new();
        for f in &folders {
            let mut in_folder: Vec<_> = self
                .state
                .terminals
                .iter()
                .filter(|t| t.folder == Some(f.id))
                .collect();
            in_folder.sort_by_key(|t| t.order);
            ids.extend(in_folder.iter().map(|t| t.id));
        }
        let folder_ids: HashSet<Uuid> = folders.iter().map(|f| f.id).collect();
        let mut loose: Vec<_> = self
            .state
            .terminals
            .iter()
            .filter(|t| t.folder.is_none_or(|fid| !folder_ids.contains(&fid)))
            .collect();
        loose.sort_by_key(|t| t.order);
        ids.extend(loose.iter().map(|t| t.id));
        ids
    }

    // ─────────────────────────── activity (V-A) ───────────────────────────

    /// Derive the current activity from a meta already in hand — the hot path
    /// for the sidebar rows and dashboard cards, which loop over metas they hold
    /// and would otherwise re-scan `state.terminals` (O(N)) per row for fields
    /// already on the stack.
    fn activity_from_meta(&self, t: &TerminalMeta) -> Activity {
        activity_for(Some(t), self.activity.get(&t.id))
    }

    /// Derive the current activity for a terminal by id (the rare id-only
    /// callers). Prefer `activity_from_meta` when the meta is already in hand.
    fn activity_of(&self, id: Uuid) -> Activity {
        activity_for(self.state.terminal(id), self.activity.get(&id))
    }

    /// Drain per-frame signals: bell + prompt detection latch NeedsYou; viewing
    /// a terminal (selected AND focused) clears its latch and burst count AND
    /// disarms further latching until user input (Bug A ack, `step_attention`);
    /// a newly-latched terminal flashes the taskbar once while unfocused (V-D).
    fn update_activity(&mut self, ctx: &egui::Context, focused: bool) {
        let selected = self.selected;
        let mut flash = false;
        // ONE fleet pass, in cached sidebar order: latch updates first, then
        // the per-terminal verdict feeds the frame's aggregates (any_working,
        // waiting) — the old per-use-site derivations re-found every meta
        // linearly and allocated two HashSets per logic frame. The meta
        // carries asleep/kind/inner_cli directly.
        let rows = self.sidebar_rows_current();
        let mut any_working = false;
        let mut waiting = std::mem::take(&mut self.waiting);
        waiting.clear();
        for t in rows.iter() {
            let id = t.id;
            let Some(backend) = self.terms.get_mut(&id) else {
                continue; // meta⇔backend sync happens in apply_snapshot
            };
            let st = self.activity.entry(id).or_insert_with(ActivityState::new);
            // SLEEP S13: sleeping is the user's explicit "not now" — the
            // whole attention surface resets while flagged (NeedsYou latch,
            // bursts, unread dot, taskbar-flash eligibility, and the
            // task-#22 CLI episode). Idempotent per frame, so a latch racing
            // the sleep Snapshot clears on the next frame; nothing
            // re-latches while asleep because a dead PTY produces no
            // output/bell. (Asleep is neither Working nor waiting.)
            if t.asleep {
                st.needs_you = false;
                st.bursts = 0;
                st.cli_stream = false;
                // Unread queue dies with the rest of the attention surface;
                // the baseline may go stale — wake respawns through Reset,
                // which drops and re-seeds it.
                st.unread_pending = false;
                st.unread_adopt = false;
                // Bug A: wake requires a deliberate user act, so a woken
                // session behaves like a fresh one — may alert once.
                st.armed = true;
                let _ = std::mem::take(&mut backend.bell);
                self.unread.remove(&id);
                self.attention_flashed.remove(&id);
                continue;
            }
            // task #22 CLI attention: claude-kind terminals and shells whose
            // tracker reports a known inner CLI.
            let is_cli = matches!(t.kind, crate::state::TermKind::Claude { .. })
                || t.inner_cli.is_some();
            let was = st.needs_you;
            // Drain the bell unconditionally (it must not pile up while
            // disarmed); step_attention decides whether it latches.
            let bell = std::mem::take(&mut backend.bell);
            // Prompt-signature scan only when the backend consumed bytes
            // since the last scan (UX HIGH-3); the cached verdict keeps the
            // exact per-frame latch semantics of the unconditional scan.
            if st.scanned_gen != backend.feed_gen {
                st.scanned_gen = backend.feed_gen;
                st.prompt_sig = backend.looks_like_prompt();
            }
            // Bug A: latch + view-ack live in the pure step (unit-tested);
            // all three sources are gated on `st.armed` so an acked terminal
            // stays quiet until the user sends input (`rearm_attention`).
            let viewing = selected == Some(id) && focused;
            let sig = st.prompt_sig;
            let quiet = st.last_output.elapsed();
            step_attention(st, bell, sig, is_cli, quiet, viewing);
            if st.needs_you && !was && !focused && !self.attention_flashed.contains(&id) {
                flash = true;
                self.attention_flashed.insert(id);
            }
            if !st.needs_you {
                self.attention_flashed.remove(&id);
            }
            // Unread ack machine (accent dot, Bug A's step_attention pattern):
            // the deselection edge re-bases the digest (select_terminal,
            // pw2-L1) — "I saw this" — so
            // the ack survives any later chrome cycle; an unselected terminal
            // with a queued episode gets ONE verdict once the stream settles.
            // The baseline refreshes after every verdict, marking or not:
            // chrome drift (a ticking status timer, paint/erase status text)
            // never accumulates into a mark, and a real change marks exactly
            // once until the user views it.
            if selected == Some(id) {
                st.unread_pending = false;
                st.unread_adopt = false;
                // Selection alone drops the flag (select_terminal's rule) —
                // this also covers the apply_snapshot auto-select path that
                // bypasses select_terminal (B1).
                self.unread.remove(&id);
                // Baseline maintenance lives at the DESELECTION edge now
                // (select_terminal, pw2-L1); only a missing baseline seeds
                // here (fresh terminal selected before its Replay, post-
                // Reset gap) so a never-deselected terminal still has one
                // the moment it needs one.
                if st.unread_base.is_none() {
                    st.unread_base = Some(backend.grid_digest());
                    st.unread_base_gen = backend.feed_gen;
                }
            } else if (st.unread_pending || st.unread_adopt) && quiet >= UNREAD_SETTLE {
                // Suppressed-window output (attach repaint / respawn banner)
                // re-bases without a verdict, even when later bytes also
                // queued one — boot noise never marks (task-#22 precedent).
                let adopt = st.unread_adopt;
                st.unread_pending = false;
                st.unread_adopt = false;
                let cur = backend.grid_digest();
                if !adopt && unread_should_mark(st.unread_base.as_ref(), &cur) {
                    self.unread.insert(id);
                    // Burst badge counts CONFIRMED content bursts only —
                    // consistent with the dot it annotates.
                    st.bursts = st.bursts.saturating_add(1);
                }
                st.unread_base = Some(cur);
                st.unread_base_gen = backend.feed_gen;
            }
            match derive_activity(
                t.status == TermStatus::Dead,
                false, // asleep handled above
                st.needs_you,
                st.last_output.elapsed(),
                is_cli,
                st.cli_stream,
            ) {
                Activity::Working => any_working = true,
                Activity::NeedsYou => waiting.push(id),
                _ => {}
            }
        }
        self.any_working = any_working;
        self.waiting = waiting;
        if flash {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
    }

    /// Bug A: user-authored PTY input re-arms the attention latch — the NEXT
    /// waiting signal (bell / prompt signature / CLI quiet) may fire amber
    /// again. Also drops any stale CLI episode so the RESPONSE stream defines
    /// the next one (an idle repaint before the input must not count). Call
    /// beside every user-input `note_input` site; never from synthetic or
    /// automatic writes. Typing requires selection+focus, so the same-frame
    /// view-ack keeps re-arming itself invisible — amber only returns once
    /// the CLI later goes quiet or prompts. Missing entry ⇒ nothing to do
    /// (ActivityState::new already starts armed).
    fn rearm_attention(&mut self, id: Uuid) {
        if let Some(st) = self.activity.get_mut(&id) {
            st.armed = true;
            st.cli_stream = false;
        }
    }

    /// Relative idle time for a terminal's second sidebar line, e.g. "2m".
    fn idle_label(&self, id: Uuid) -> String {
        let secs = self
            .activity
            .get(&id)
            .map(|s| s.last_output.elapsed().as_secs())
            .unwrap_or(0);
        match secs {
            0..=9 => "active".into(),
            10..=59 => format!("{secs}s"),
            60..=3599 => format!("{}m", secs / 60),
            3600..=86399 => format!("{}h", secs / 3600),
            _ => format!("{}d", secs / 86400),
        }
    }

    /// Select a terminal: clear its unread/burst signals, leave any dashboard,
    /// and drop the scrollback search (matches don't carry across terminals).
    fn select_terminal(&mut self, id: Uuid) {
        // Unread ack machine (pw2-L1): capture the OUTGOING terminal's
        // "what the user last saw" baseline here, synchronously at the
        // deselection edge. The only consumer of a selected terminal's
        // baseline is the verdict taken after it becomes unselected, so the
        // per-frame re-digest this replaced (grid_digest on every consumed
        // frame of a streaming selected terminal, 3-22µs each) was pure
        // waste. Synchronous capture is load-bearing: the click lands in
        // ui(), AFTER this frame's drain_ipc and update_activity, so zero
        // bytes have drained since the last selected-arm pass and the
        // captured baseline is byte-identical to the continuously
        // maintained one. A next-frame edge capture would run after the
        // NEXT frame's drain advanced this grid — one batch of
        // never-painted bytes baked into the baseline; a stream settling
        // inside that batch (claude's final chunk landing as the user
        // clicks away) would then never light the unread dot. Census of
        // `self.selected` writers: this is the only prev-alive deselection
        // edge — the delete paths (apply_snapshot prune, delete-modal
        // neighbor hop) lose the backend anyway, and the auto-select
        // fallback has no previous terminal.
        if let Some(prev) = self.selected.filter(|&p| p != id) {
            if let (Some(backend), Some(st)) =
                (self.terms.get(&prev), self.activity.get_mut(&prev))
            {
                unread_rebase_on_deselect(st, backend);
            }
        }
        self.selected = Some(id);
        self.unread.remove(&id);
        if let Some(st) = self.activity.get_mut(&id) {
            st.bursts = 0;
        }
        self.central_view = CentralView::Terminal;
        self.search = None;
        self.blocks_panel = None;
        // History content is cross-terminal but insertion targets the
        // selected composer — close and let one click re-anchor (P4 §3.5).
        self.history = None;
        // Selection races resolve in the user's favor (§3.2): any manual
        // selection cancels create-retargeting. The launcher closes too —
        // its folder chip / target assumptions are stale (§8).
        self.pending_create = None;
        if self.launcher.as_ref().is_some_and(|l| !l.embedded) {
            self.launcher = None;
        }
        // Tab-switch consistency (P3 §3): an armed prompt means typing
        // composes — the target's armed composer takes focus.
        if let Some(st) = self.composers.get_mut(&id) {
            if st.mode == ComposerMode::Compose {
                st.want_focus = true;
            }
        }
    }

    // ─────────────────────── inline rename (§5.4) ───────────────────────

    /// Begin an inline rename. Entry points: hover ✏ (terminal + folder
    /// rows), double-click of the row name, context-menu Rename, and the
    /// top-bar title click. Mouse-first; no hotkey.
    fn start_rename(&mut self, target: RenameTarget, current: String, host: RenameHost) {
        self.renaming = Some(RenameState {
            target,
            value: current,
            host,
            focus_pending: true,
            had_focus: false,
            rendered: false,
        });
    }

    /// Commit (Enter/blur: trimmed, empty ⇒ cancel) or cancel (Esc) — ends
    /// the rename either way.
    fn finish_rename(&mut self, commit: bool) {
        let Some(rn) = self.renaming.take() else { return };
        if commit {
            if let Some(msg) = rename_commit(rn.target, &rn.value) {
                self.send(msg);
            }
        }
    }

    /// The one inline-rename editor (§5.4): a borderless 13px TextEdit over
    /// `rect` with a SURFACE_2 fill rounded 4 behind the text only — hosted
    /// by whichever surface owns the name galley this frame (sidebar row or
    /// top-bar title). Open frame: focus grab + select-all (LOW-9). Enter →
    /// commit, Esc → cancel, blur → commit.
    fn rename_editor(&mut self, ui: &mut egui::Ui, rect: Rect, font: FontId) {
        let ed_id = Id::new("inline-rename");
        let ctx = ui.ctx().clone();
        {
            let Some(rn) = self.renaming.as_mut() else { return };
            rn.rendered = true;
            if rn.focus_pending {
                rn.focus_pending = false;
                ctx.memory_mut(|m| m.request_focus(ed_id));
                // Select-all before the TextEdit shows (the composer's
                // TextEditState pattern): typing replaces the whole name.
                let end = egui::text::CCursor::new(rn.value.chars().count());
                let mut st = egui::text_edit::TextEditState::load(&ctx, ed_id)
                    .unwrap_or_default();
                st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    end,
                )));
                st.store(&ctx, ed_id);
            }
        }
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), SURFACE_2);
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        // Read Enter/Esc BEFORE the field consumes them (the inline-search
        // pattern).
        let (esc, enter) = child.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Enter),
            )
        });
        let resp = {
            let Some(rn) = self.renaming.as_mut() else { return };
            let resp = child.add(
                egui::TextEdit::singleline(&mut rn.value)
                    .id(ed_id)
                    .font(font)
                    .text_color(TEXT)
                    .frame(egui::Frame::NONE)
                    .margin(Margin::symmetric(4, 2))
                    .desired_width(rect.width() - 8.0),
            );
            if resp.has_focus() {
                rn.had_focus = true;
            }
            resp
        };
        let had_focus = self.renaming.as_ref().is_some_and(|r| r.had_focus);
        if esc {
            self.finish_rename(false);
        } else if had_focus && (enter || resp.lost_focus()) {
            self.finish_rename(true);
        }
    }

    // ───────────────────── drag to reorder (§5.5) ─────────────────────

    /// Per-frame drag bookkeeping, run before the tree rebuilds the slot
    /// map: Esc cancels (consumed), a release resolves against LAST frame's
    /// rows (one frame stale at worst), and a lost release (alt-tab
    /// mid-drag — the term_view incident class) never leaves a latched drag.
    fn drag_lifecycle(&mut self, ctx: &egui::Context) {
        if self.drag.is_none() {
            return;
        }
        let esc = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if esc {
            self.drag = None;
            return;
        }
        let (released, down, pos) = ctx.input(|i| {
            (
                i.pointer.any_released(),
                i.pointer.primary_down(),
                i.pointer.latest_pos(),
            )
        });
        if released {
            if let Some(hit) = pos.and_then(|p| self.slot_at(p)) {
                self.perform_drop(hit);
            }
            self.drag = None;
        } else if !down {
            self.drag = None;
        }
    }

    /// Hit-test the pointer against LAST frame's slot map for the armed
    /// payload (see `slot_hit` for the band rules). None while no drag.
    fn slot_at(&self, p: Pos2) -> Option<SlotHit> {
        let d = self.drag.as_ref()?;
        slot_hit(&self.drop_rows, &d.payload, p)
    }

    /// Wire a resolved drop (§5.5): for terminals, MoveTerminal when the
    /// folder changed, then ReorderTerminal with the client-computed delta —
    /// two messages, same daemon thread, processed in order. For folders,
    /// MoveFolder with the same delta math. Nothing is applied
    /// optimistically; the snapshot round-trip repaints the truth.
    fn perform_drop(&mut self, hit: SlotHit) {
        let Some(d) = self.drag.take() else { return };
        match (d.payload, hit) {
            (DragPayload::Term { id, from }, hit) => self.perform_term_drop(id, from, hit),
            (DragPayload::Folder { id }, SlotHit::FolderInsert { idx, .. }) => {
                // Replicate the daemon's MoveFolder semantics: folders
                // sorted by `order` alone (D6), remove+insert+clamp — the
                // exact math drop_reorder_delta encodes.
                let mut fs: Vec<(Uuid, i64)> = self
                    .state
                    .folders
                    .iter()
                    .map(|f| (f.id, f.order))
                    .collect();
                fs.sort_by_key(|&(_, o)| o);
                let folders: Vec<Uuid> = fs.into_iter().map(|(i, _)| i).collect();
                let delta = drop_reorder_delta(&folders, id, Some(idx), true);
                if delta != 0 {
                    self.send(C2D::MoveFolder { id, delta });
                }
            }
            // slot_hit filters targets by payload, so a folder payload can
            // only resolve to FolderInsert — defensively drop anything else.
            (DragPayload::Folder { .. }, _) => {}
        }
    }

    /// The terminal half of `perform_drop` (§5.5).
    fn perform_term_drop(&mut self, id: Uuid, from: Option<Uuid>, hit: SlotHit) {
        let (dest, idx) = match hit {
            SlotHit::IntoFolder { id: fid, .. } => {
                if from == Some(fid) {
                    return; // already in that folder — nothing to do
                }
                (Some(fid), None)
            }
            SlotHit::Insert { folder, idx, .. } => (folder, Some(idx)),
            // Bug B: the UNGROUPED zone appends to the loose group.
            SlotHit::LooseAppend { .. } => (None, None),
            SlotHit::FolderInsert { .. } => return, // not a terminal target
        };
        if from != dest {
            self.send(C2D::MoveTerminal { id, folder: dest });
        }
        // Replicate the daemon's post-move group EXACTLY (its filter walks
        // the snapshot vec in order, then stable-sorts by `order` — same
        // vec, same sort here; the dragged terminal's folder is overridden
        // to the destination since the daemon reorders after the move).
        let mut g: Vec<(Uuid, i64)> = self
            .state
            .terminals
            .iter()
            .filter(|t| {
                let f = if t.id == id { dest } else { t.folder };
                f == dest
            })
            .map(|t| (t.id, t.order))
            .collect();
        g.sort_by_key(|&(_, o)| o);
        let group: Vec<Uuid> = g.into_iter().map(|(i, _)| i).collect();
        let delta = drop_reorder_delta(&group, id, idx, from == dest);
        if delta != 0 {
            self.send(C2D::ReorderTerminal { id, delta });
        }
    }

    /// The armed drag's visual layer: insertion bar / folder highlight at
    /// the hovered slot (painted over the tree, this frame's rows), plus the
    /// pointer-locked ghost. Zero animation by spec.
    fn paint_drag_feedback(&self, ui: &mut egui::Ui) {
        let Some(d) = &self.drag else { return };
        let Some(p) = ui.ctx().pointer_latest_pos() else { return };
        let hit = self.slot_at(p);
        match &hit {
            Some(
                SlotHit::Insert { y, x, .. }
                | SlotHit::FolderInsert { y, x, .. }
                | SlotHit::LooseAppend { y, x },
            ) => {
                let bar = Rect::from_min_max(
                    Pos2::new(x.min + 4.0, y - 1.0),
                    Pos2::new(x.max - 4.0, y + 1.0),
                );
                ui.painter().rect_filled(bar, CornerRadius::same(1), ACCENT);
            }
            Some(SlotHit::IntoFolder { rect, .. }) => {
                ui.painter()
                    .rect_filled(*rect, CornerRadius::same(6), ACCENT_SUBTLE);
            }
            None => {}
        }
        // Dead zone (Bug B): a release here cancels — say so instead of
        // failing silently. The ghost dims and the cursor flips to no-drop
        // (reveal, never outline).
        let live = hit.is_some();
        let alpha = if live { 0.8 } else { 0.45 };
        // Ghost: name (+ activity dot for terminals; folder ghosts are
        // dot-less name · count) on SURFACE_2 r6 + soft shadow, riding the
        // pointer at the grab offset. Painted on the tooltip layer so it
        // clears every panel.
        let is_folder = matches!(d.payload, DragPayload::Folder { .. });
        let painter = ui
            .ctx()
            .layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::new("drag-ghost")));
        let galley =
            painter.layout_no_wrap(d.name.clone(), FontId::proportional(13.0), TEXT);
        let (text_x, pad) = if is_folder { (14.0, 26.0) } else { (26.0, 36.0) };
        let rect = Rect::from_min_size(
            p - d.grab,
            Vec2::new(galley.size().x + pad, 28.0),
        );
        painter.add(
            egui::epaint::Shadow {
                offset: [0, 2],
                blur: 12,
                spread: 0,
                color: Color32::from_black_alpha(64),
            }
            .as_shape(rect, CornerRadius::same(6)),
        );
        painter.rect_filled(rect, CornerRadius::same(6), SURFACE_2.gamma_multiply(alpha));
        if !is_folder {
            painter.circle_filled(
                Pos2::new(rect.min.x + 14.0, rect.center().y),
                4.0,
                d.dot.gamma_multiply(alpha),
            );
        }
        let ty = rect.center().y - galley.size().y / 2.0;
        painter.galley(
            Pos2::new(rect.min.x + text_x, ty),
            galley,
            TEXT.gamma_multiply(alpha),
        );
        ui.ctx().set_cursor_icon(if live {
            CursorIcon::Grabbing
        } else {
            CursorIcon::NoDrop
        });
    }

    /// Bug B: edge-band autoscroll while a drag is armed, run inside the
    /// sidebar's ScrollArea closure. Pointer within 24px of the viewport's
    /// top/bottom feeds a per-frame scroll delta proportional to edge
    /// proximity, so off-screen drop targets are reachable mid-drag.
    fn drag_autoscroll(&self, ui: &mut egui::Ui) {
        if self.drag.is_none() {
            return;
        }
        let Some(p) = ui.ctx().pointer_latest_pos() else { return };
        let vp = ui.clip_rect();
        if p.x < vp.min.x || p.x > vp.max.x {
            return; // pointer left the sidebar column
        }
        const BAND: f32 = 24.0;
        let dt = ui.input(|i| i.stable_dt).min(0.1);
        // Positive scroll delta moves the view up (content down).
        let dy = if p.y < vp.min.y + BAND {
            (vp.min.y + BAND - p.y) * 8.0 * dt
        } else if p.y > vp.max.y - BAND {
            -((p.y - (vp.max.y - BAND)) * 8.0 * dt)
        } else {
            return;
        };
        ui.scroll_with_delta(Vec2::new(0.0, dy));
        ui.ctx().request_repaint();
    }

    // ─────────────────────────── composer (P3) ───────────────────────────

    /// This terminal spawns with the block-hook bootstrap (P3's load-bearing
    /// hookless gate). epoch > 0 is the live signal (P2's scanner enable);
    /// the persisted meta flag (proto 5) makes the verdict available at the
    /// very first attach — before the Blocks sync lands — so the strip
    /// reservation is in the attach-at-size announcement and the corrective
    /// resize becomes a no-op (the 49↔52 boot flip).
    fn hooked(&self, id: Uuid) -> bool {
        self.blocks.get(&id).is_some_and(|b| b.epoch > 0)
            || self.state.terminal(id).is_some_and(|t| t.hooked)
    }

    /// Per-terminal grid geometry (P3 §7): hooked terminals reserve a
    /// strip at the card's bottom edge; hookless terminals keep the full
    /// card. Each terminal owns a stable geometry — tab switches change
    /// nothing (a shared layout would flip grid sizes between hooked and
    /// hookless tabs: the resize-storm incident class).
    ///
    /// C2 exception — the ONE sanctioned dynamic edge: while the strip is
    /// collapsed under a stable full-screen app (`strip_collapsed`, the same
    /// `strip_hidden` predicate that gates the strip paint — single source,
    /// paint and PTY size can never disagree) the terminal gets the FULL
    /// card: the reclaimed rows reach the TUI through the ordinary debounced
    /// resize paths (commit loop / corrective heal), one +rows resize at
    /// collapse and one −rows at the instant return. The HIDE_AFTER
    /// hysteresis inside the predicate is the flap debounce.
    fn layout_for(&self, id: Uuid, base: Vec2) -> Vec2 {
        if self.hooked(id) && !self.strip_collapsed(id) {
            Vec2::new(base.x, (base.y - composer::STRIP_H).max(0.0))
        } else {
            base
        }
    }

    /// C2: is this terminal's strip collapsed (rows reclaimed by the grid)
    /// right now? Thin app-side binding of `composer::strip_hidden` — it
    /// derives every input live (backend ALT_SCREEN flag, Running status,
    /// open recs, the composer's hysteresis clock) so EVERY caller —
    /// layout_for (and through it all resize sites), central's card split,
    /// the sleep pre-pass — asks the identical question. Terminals without
    /// a composer/backend yet (cold attach, hookless) are never collapsed.
    fn strip_collapsed(&self, id: Uuid) -> bool {
        let Some(st) = self.composers.get(&id) else {
            return false;
        };
        let Some(b) = self.terms.get(&id) else {
            return false;
        };
        let running =
            self.state.terminal(id).map(|t| t.status) == Some(TermStatus::Running);
        let alt = b.mode().contains(TermMode::ALT_SCREEN);
        let open_rec = self
            .blocks
            .get(&id)
            .is_some_and(|bl| bl.recs.iter().any(|r| r.end_off.is_none()));
        composer::strip_hidden(st, running, alt, open_rec, Instant::now())
    }

    /// C2 sleep pre-pass: the daemon's freeze-frame capture and the asleep
    /// presentation must share ONE geometry. Sleeping while collapsed would
    /// freeze a FULL-size frame that wake then presents in a reserved-size
    /// grid (the asleep lane holds the strip visible by precedence). So:
    /// restart the hide clock (the strip un-collapses this instant and
    /// cannot re-collapse for HIDE_AFTER — ample cover for the sleep
    /// round-trip) and resize back BEFORE the sleep verb ships; the daemon
    /// processes messages in order, so the mirror + PTY are reserved-size
    /// when the kill captures the frame. No-op unless actually collapsed.
    fn prepare_sleep_geometry(&mut self, id: Uuid) {
        if !self.strip_collapsed(id) {
            return;
        }
        if let Some(st) = self.composers.get_mut(&id) {
            st.restart_hide_clock();
        }
        if let Some((base, cell)) = self.last_grid {
            let l = self.layout_for(id, base);
            if let Some(b) = self.terms.get_mut(&id) {
                if let Some((cols, rows)) = b.resize_to(l, cell) {
                    self.send(C2D::Resize { id, cols, rows });
                }
            }
        }
    }

    /// C2: the folder-sleep pre-pass — every member gets the same
    /// freeze-geometry treatment before the shared SleepFolder verb.
    fn prepare_sleep_geometry_folder(&mut self, folder: Uuid) {
        let ids: Vec<Uuid> = self
            .state
            .terminals
            .iter()
            .filter(|t| t.folder == Some(folder))
            .map(|t| t.id)
            .collect();
        for id in ids {
            self.prepare_sleep_geometry(id);
        }
    }

    // ─────────────────────────── blocks (P2) ───────────────────────────

    /// Mouse-first Re-run is allowed only when the shell is demonstrably at
    /// an interactive prompt: the session is Running, no block record is
    /// open, and the terminal is not in alt-screen. Accepted residual risk
    /// (matches Warp): text typed-but-unsubmitted at the prompt gets the
    /// re-run appended after it — clearing it blind is PSReadLine-mode
    /// dependent, and P3 Composer owns line editing.
    fn can_rerun(&self, id: Uuid) -> bool {
        let running = self
            .state
            .terminal(id)
            .is_some_and(|t| t.status == TermStatus::Running);
        let no_open = self
            .blocks
            .get(&id)
            .is_some_and(|b| rerun_recs_ready(&b.recs));
        let not_alt = self
            .terms
            .get(&id)
            .is_some_and(|t| !t.mode().contains(TermMode::ALT_SCREEN));
        // B2: the composer's v0.1.1 PRE-SHELL veto, same signals. A restored
        // password-auth ssh terminal replays only CLOSED records (on_exit
        // closed the previous lifetime's dangling block), so the record leg
        // passes while ssh sits at `password:` / the host-key question — one
        // Re-run click would type the command + Enter into the auth
        // conversation (and a command starting with `yes` would TRUST the
        // unverified host key). No hook event in the CURRENT lifetime and no
        // live prompt latch ⇒ whatever is talking is not the shell ⇒ no
        // Re-run. On a healthy restored local shell the first prompt render
        // fires the pre hook (pre_seen > 0) and Re-run re-enables at once.
        let pre_shell_now = composer::pre_shell(
            running,
            !not_alt,
            self.terms
                .get(&id)
                .and_then(|t| t.block_feed.as_ref())
                .map(|f| f.pre_seen)
                .unwrap_or(0),
            self.terms
                .get(&id)
                .and_then(|t| t.block_feed.as_ref())
                .map(|f| f.exec_seen)
                .unwrap_or(0),
            self.composers
                .get(&id)
                .is_some_and(|c| c.at_prompt_latched()),
        );
        running && no_open && not_alt && !pre_shell_now
    }

    /// Type the recorded command + Enter into the shell. UTF-8 passthrough
    /// is valid under win32-input-mode (it is Windows Terminal's paste path).
    fn rerun_block(&mut self, id: Uuid, start_off: u64) {
        if !self.can_rerun(id) {
            return;
        }
        let Some(cmd) = self
            .blocks
            .get(&id)
            .and_then(|b| b.recs.iter().find(|r| r.start_off == start_off))
            .map(|r| r.cmd.clone())
        else {
            return;
        };
        let mut bytes = cmd.into_bytes();
        bytes.push(b'\r');
        self.send(C2D::Input { id, bytes });
        if let Some(b) = self.terms.get_mut(&id) {
            b.scroll_to_bottom();
            b.note_input(); // v0.1.1: freeze a pending prompt-end upgrade
        }
        self.rearm_attention(id); // Bug A: user input re-arms amber
    }

    /// Ask the daemon for the block's stripped output text; the D2C reply
    /// lands it on the clipboard (fire-and-forget — loopback replies arrive
    /// in ms, and a dead daemon surfaces via the reconnect UX). An old
    /// daemon would DROP the client on an undecodable C2D frame, so gate on
    /// its protocol generation.
    fn copy_block_output(&mut self, id: Uuid, start_off: u64) {
        if self.ipc.as_ref().is_some_and(|c| c.proto >= 2) {
            self.send(C2D::BlockText { id, start_off });
        } else {
            self.notice = Some((
                "Restart the daemon from this build to copy block output.".into(),
                Instant::now(),
            ));
        }
    }

    /// Blocks recall panel (P2 §6): filter + failure navigation + one row per
    /// record, newest first. Real egui widgets are safe here (unlike the
    /// in-grid chrome): the panel is a Foreground-order Area, so egui's layer
    /// hit-testing keeps its clicks and wheel away from the grid's raw-event
    /// handler underneath. Rows without an anchor render dimmed — the honest
    /// degraded mode for pre-attach/stale history: everything but the in-grid
    /// jump still works from the record + journal.
    fn blocks_panel_ui(&mut self, ctx: &egui::Context, central: Rect, id: Uuid) {
        if self.blocks_panel.is_none() {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.blocks_panel = None;
            return;
        }
        enum Act {
            CopyOut(u64),
            Rerun(u64),
            Jump(u64, i32),
        }
        let mut acts: Vec<Act> = Vec::new();
        let mut close = false;
        let can_rerun = self.can_rerun(id);
        let top_line = self
            .terms
            .get(&id)
            .map(|b| -(b.term.grid().display_offset() as i32))
            .unwrap_or(0);
        let btn_rect = self.blocks_btn_rect;
        let width = 440.0;
        let pos = Pos2::new(central.max.x - width - 12.0, central.min.y + 4.0);
        {
            let Some(panel) = self.blocks_panel.as_mut() else { return };
            let Some(list) = self.blocks.get_mut(&id) else { return };
            let failed_total = list.failed_count();
            let feed = self.terms.get(&id).and_then(|b| b.block_feed.as_ref());
            let anchors: &[term_backend::BlockAnchor] =
                feed.map(|f| f.anchors.as_slice()).unwrap_or(&[]);
            let in_grid = feed.is_some_and(|f| !f.stale);
            // Anchored failures, by grid line — the prev/next working set.
            // Skipping unanchored failures (still listed in the panel) beats
            // jumping to a wrong row: navigation must be predictable from
            // what the user can see.
            let mut fails: Vec<(u64, i32)> = list
                .recs
                .iter()
                .filter(|r| r.end_off.is_some() && r.exit.is_some_and(|e| e != 0))
                .filter_map(|r| {
                    (in_grid)
                        .then(|| {
                            anchors
                                .binary_search_by_key(&r.start_off, |a| a.start_off)
                                .ok()
                                .map(|ai| (r.start_off, anchors[ai].line))
                        })
                        .flatten()
                })
                .collect();
            fails.sort_by_key(|&(_, l)| l);
            let prev_fail = fails.iter().filter(|&&(_, l)| l < top_line).max_by_key(|&&(_, l)| l).copied();
            let next_fail = fails.iter().filter(|&&(_, l)| l > top_line).min_by_key(|&&(_, l)| l).copied();

            // Filtered record indices, newest first (recency is what command
            // recall wants). Plain case-insensitive substring — command
            // recall, not text search; scrollback search owns regex. Cached
            // by (filter, failed_only, blocks_stamp): recomputed on query
            // change or new Blocks frames only (LOW-12).
            let key = (panel.filter.clone(), panel.failed_only, self.blocks_stamp);
            if panel.cache_key != key {
                let filter_lc = panel.filter.to_lowercase();
                panel.rows = list
                    .recs
                    .iter()
                    .enumerate()
                    .rev()
                    .filter(|(_, r)| {
                        (!panel.failed_only
                            || (r.end_off.is_some() && r.exit.is_some_and(|e| e != 0)))
                            && (filter_lc.is_empty()
                                || r.cmd.to_lowercase().contains(&filter_lc))
                    })
                    .map(|(i, _)| i)
                    .collect();
                panel.cache_key = key;
            }
            // Cheap clone of ≤500 indices so the TextEdit below can borrow
            // `panel.filter` mutably while the rows render.
            let rows: Vec<usize> = panel.rows.clone();

            let area = egui::Area::new(egui::Id::new(("blocks_panel", id)))
                .order(egui::Order::Foreground)
                .fixed_pos(pos);
            let aresp = area.show(ctx, |ui| {
                // Depth by shadow alone — no border stroke (seamless doctrine).
                egui::Frame::new()
                    .fill(SURFACE)
                    .corner_radius(CornerRadius::same(8))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 6],
                        blur: 24,
                        spread: 0,
                        color: Color32::from_black_alpha(140),
                    })
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(width - 20.0);
                        ui.horizontal(|ui| {
                            let te = ui.add(
                                egui::TextEdit::singleline(&mut panel.filter)
                                    .desired_width(170.0)
                                    .hint_text("Filter commands")
                                    .font(FontId::proportional(12.0))
                                    .margin(Margin::symmetric(8, 5)),
                            );
                            te.request_focus();
                            let chip_label = if failed_total > 0 {
                                format!("Failures {failed_total}")
                            } else {
                                "Failures".into()
                            };
                            if ui
                                .selectable_label(
                                    panel.failed_only,
                                    RichText::new(chip_label).size(11.0),
                                )
                                .clicked()
                            {
                                panel.failed_only = !panel.failed_only;
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if nav_icon_button(
                                    ui,
                                    Icon::ChevronDown,
                                    next_fail.is_some(),
                                    "Next failed command",
                                    "No failed commands in view history",
                                ) {
                                    if let Some((off, l)) = next_fail {
                                        acts.push(Act::Jump(off, l));
                                    }
                                }
                                if nav_icon_button(
                                    ui,
                                    Icon::ChevronUp,
                                    prev_fail.is_some(),
                                    "Previous failed command",
                                    "No failed commands in view history",
                                ) {
                                    if let Some((off, l)) = prev_fail {
                                        acts.push(Act::Jump(off, l));
                                    }
                                }
                            });
                        });
                        ui.add_space(8.0);
                        egui::ScrollArea::vertical().max_height(360.0).show_rows(
                            ui,
                            34.0,
                            rows.len(),
                            |ui, range| {
                                for &ri in &rows[range] {
                                    let r = &list.recs[ri];
                                    let anchor_line = in_grid
                                        .then(|| {
                                            anchors
                                                .binary_search_by_key(&r.start_off, |a| {
                                                    a.start_off
                                                })
                                                .ok()
                                                .map(|ai| anchors[ai].line)
                                        })
                                        .flatten();
                                    let (rect, rowresp) = ui.allocate_exact_size(
                                        Vec2::new(ui.available_width(), 34.0),
                                        Sense::click(),
                                    );
                                    let hovered = rowresp.hovered();
                                    let p = ui.painter().clone();
                                    if hovered {
                                        p.rect_filled(rect, CornerRadius::same(6), SURFACE_2);
                                    }
                                    let failed = r.end_off.is_some()
                                        && r.exit.is_some_and(|e| e != 0);
                                    let open = r.end_off.is_none();
                                    // Status glyph: quiet for success — the
                                    // absence of red IS the success state.
                                    if open {
                                        p.circle_filled(
                                            Pos2::new(rect.min.x + 16.0, rect.center().y),
                                            3.0,
                                            ACCENT,
                                        );
                                    } else if failed {
                                        p.text(
                                            Pos2::new(rect.min.x + 8.0, rect.center().y),
                                            Align2::LEFT_CENTER,
                                            format!("\u{2715} {}", r.exit.unwrap_or(0)),
                                            FontId::proportional(10.0),
                                            DANGER,
                                        );
                                    } else if r.exit.is_none() {
                                        p.text(
                                            Pos2::new(rect.min.x + 12.0, rect.center().y),
                                            Align2::LEFT_CENTER,
                                            "\u{2014}",
                                            FontId::proportional(10.0),
                                            TEXT_FAINT,
                                        );
                                    }
                                    // Command, ellipsized into its lane.
                                    let cmd_col =
                                        if anchor_line.is_some() { TEXT } else { TEXT_MUTED };
                                    let cmd_one = r.cmd.replace(['\r', '\n'], " ");
                                    let lane_w = rect.width() - 40.0 - 130.0;
                                    let galley = p.layout_no_wrap(
                                        cmd_one,
                                        FontId::monospace(12.0),
                                        cmd_col,
                                    );
                                    let cp = p.with_clip_rect(Rect::from_min_max(
                                        Pos2::new(rect.min.x + 40.0, rect.min.y),
                                        Pos2::new(rect.min.x + 40.0 + lane_w, rect.max.y),
                                    ));
                                    cp.galley(
                                        Pos2::new(
                                            rect.min.x + 40.0,
                                            rect.center().y - galley.size().y / 2.0,
                                        ),
                                        galley,
                                        cmd_col,
                                    );
                                    if hovered {
                                        // Mini action cluster, mirroring the
                                        // in-grid toolbar.
                                        let kinds = [
                                            (Icon::Copy, "Copy command"),
                                            (Icon::CopyLines, "Copy output"),
                                            (Icon::Rerun, if can_rerun { "Run again" } else { "Shell is busy" }),
                                        ];
                                        for (k, (icon, tip)) in kinds.into_iter().enumerate() {
                                            let bx = rect.max.x
                                                - 8.0
                                                - (3 - k) as f32 * 22.0;
                                            let brect = Rect::from_min_size(
                                                Pos2::new(bx, rect.center().y - 9.0),
                                                Vec2::splat(18.0),
                                            );
                                            let bresp = ui.interact(
                                                brect,
                                                ui.id().with(("blkrow", r.start_off, k)),
                                                Sense::click(),
                                            );
                                            let dim = k == 2 && !can_rerun;
                                            if bresp.hovered() && !dim {
                                                p.rect_filled(
                                                    brect,
                                                    CornerRadius::same(4),
                                                    OV_HOVER,
                                                );
                                            }
                                            draw_icon(
                                                &p,
                                                brect.shrink(2.0),
                                                icon,
                                                if dim {
                                                    TEXT_FAINT
                                                } else if bresp.hovered() {
                                                    TEXT
                                                } else {
                                                    TEXT_SECONDARY
                                                },
                                            );
                                            if bresp.on_hover_text(tip).clicked() {
                                                match k {
                                                    0 => ui.ctx().copy_text(r.cmd.clone()),
                                                    1 => acts.push(Act::CopyOut(r.start_off)),
                                                    _ if !dim => {
                                                        acts.push(Act::Rerun(r.start_off))
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                    } else {
                                        let dur = r
                                            .ended_ms
                                            .map(|e| {
                                                term_view::fmt_duration(
                                                    e.saturating_sub(r.started_ms),
                                                )
                                            })
                                            .unwrap_or_else(|| "running\u{2026}".into());
                                        let right =
                                            format!("{dur} · {}", time_ago_ms(r.started_ms));
                                        let rg = p.layout_no_wrap(
                                            right,
                                            FontId::proportional(10.0),
                                            TEXT_MUTED,
                                        );
                                        let rx = rect.max.x - 8.0 - rg.size().x;
                                        p.galley(
                                            Pos2::new(
                                                rx,
                                                rect.center().y - rg.size().y / 2.0,
                                            ),
                                            rg,
                                            TEXT_MUTED,
                                        );
                                        if r.truncated {
                                            // Journal compaction cut this
                                            // block's output; Copy output
                                            // will be partial.
                                            p.text(
                                                Pos2::new(rx - 6.0, rect.center().y),
                                                Align2::RIGHT_CENTER,
                                                "trimmed",
                                                FontId::proportional(10.0),
                                                TEXT_FAINT,
                                            );
                                        }
                                    }
                                    if let Some(line) = anchor_line {
                                        if rowresp.clicked() {
                                            acts.push(Act::Jump(r.start_off, line));
                                        }
                                    } else {
                                        rowresp.on_hover_text(
                                            "Not in view \u{2014} ran before this window attached (or scrolled past tracking)",
                                        );
                                    }
                                }
                            },
                        );
                    });
            });
            // A primary press outside the panel (and off the header toggle)
            // closes it.
            let prect = aresp.response.rect;
            if ctx.input(|i| {
                i.pointer.primary_pressed()
                    && i.pointer.press_origin().is_some_and(|p| {
                        !prect.contains(p) && !btn_rect.is_some_and(|b| b.contains(p))
                    })
            }) {
                close = true;
            }
        }
        for act in acts {
            match act {
                Act::CopyOut(off) => self.copy_block_output(id, off),
                Act::Rerun(off) => self.rerun_block(id, off),
                Act::Jump(off, line) => {
                    if let Some(b) = self.terms.get_mut(&id) {
                        b.jump_to_line(line);
                        b.jump_flash = Some((off, Instant::now()));
                    }
                }
            }
        }
        if close {
            self.blocks_panel = None;
        }
    }

    /// Cross-session history popup (P4 §3.3): every command across ALL
    /// terminals and past epochs, aggregated GUI-side from the BlockList
    /// stores (zero wire changes — the sidecars are already client-side via
    /// the per-attach full Blocks syncs). Anchored ABOVE the composer strip,
    /// growing upward; recency order with exact-cmd dedupe; tokenized
    /// AND-substring filter; Up/Down/Enter keyboard nav; hover Copy/Run.
    /// Seamless doctrine: SURFACE fill + shadow + spacing, zero strokes.
    fn history_popup_ui(
        &mut self,
        ctx: &egui::Context,
        strip_rect: Rect,
        id: Uuid,
        prompt_cwd: Option<&str>,
    ) {
        if self.history.is_none() {
            return;
        }
        // Escape closes one layer (P4 §3.6). Search never coexists with the
        // popup (§3.5 exclusion), but guard anyway: search-Esc wins.
        if self.search.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.history = None;
            if let Some(st) = self.composers.get_mut(&id) {
                if st.mode == ComposerMode::Compose {
                    st.want_focus = true; // focus returns to the composer
                }
            }
            return;
        }

        // Rebuild the index on stamp drift (open frame, Blocks frames while
        // open, deletions): O(total recs) + sort — click-time work, never
        // steady frame work. Drift rebuilds are debounced to 500ms (LOW-13);
        // the deferred rebuild lands on a later frame because the stamp still
        // differs (repaints keep coming while a terminal streams).
        if self.history.as_ref().is_some_and(|h| {
            h.built != self.blocks_stamp
                && (h.built == u64::MAX || h.built_at.elapsed() >= Duration::from_millis(500))
        }) {
            let entries = {
                let mut lists: Vec<(Uuid, String, bool, &[BlockRec])> = Vec::new();
                for tid in self.sorted_terminal_ids() {
                    let Some(bl) = self.blocks.get(&tid) else { continue };
                    if bl.recs.is_empty() {
                        continue;
                    }
                    let Some(meta) = self.state.terminal(tid) else { continue };
                    lists.push((
                        tid,
                        meta.name.clone(),
                        meta.status == TermStatus::Dead,
                        bl.recs.as_slice(),
                    ));
                }
                history::build_index(&lists)
            };
            let stamp = self.blocks_stamp;
            if let Some(h) = self.history.as_mut() {
                h.entries = entries;
                h.built = stamp;
                h.built_at = Instant::now();
                h.hits = history::filter(&h.entries, &h.query);
                h.sel = h.sel.min(h.hits.len().saturating_sub(1));
            }
        }

        // Consume nav keys BEFORE the search TextEdit shows (the P3
        // consume-before-show pattern): a leaked arrow reaching the composer
        // recall or a leaked Enter reaching submit is a keystroke-loss bug.
        let (up, down, enter) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            )
        });

        // Run gate for the SELECTED terminal (D10) + its disabled reason.
        let running =
            self.state.terminal(id).map(|t| t.status) == Some(TermStatus::Running);
        let now = Instant::now();
        let run_ok = match (self.composers.get(&id), self.terms.get(&id), self.blocks.get(&id))
        {
            (Some(st), Some(b), Some(bl)) => {
                st.history_run_allowed(b, &bl.recs, running, now)
            }
            _ => false,
        };
        let run_tip: &str = if run_ok {
            "Run in this terminal"
        } else if !running {
            "Session ended"
        } else if self
            .blocks
            .get(&id)
            .is_some_and(|b| b.recs.iter().any(|r| r.end_off.is_none()))
        {
            "Shell is busy"
        } else if self
            .terms
            .get(&id)
            .is_some_and(|b| !b.cursor_at_prompt_end())
        {
            "Prompt has typed text \u{2014} Compose first"
        } else {
            "No prompt yet"
        };

        enum HistAct {
            Insert(u32),
            Run(u32),
            Copy(u32),
        }
        let mut acts: Vec<HistAct> = Vec::new();
        let mut close = false;
        let btn_rect = self.history_btn_rect;
        let width = 640.0_f32.min(strip_rect.width());

        {
            let Some(popup) = self.history.as_mut() else { return };
            let kb_moved = {
                let mut moved = false;
                if up && popup.sel > 0 {
                    popup.sel -= 1;
                    moved = true;
                }
                if down && popup.sel + 1 < popup.hits.len() {
                    popup.sel += 1;
                    moved = true;
                }
                moved
            };
            popup.kb_moved = kb_moved;
            if enter {
                if let Some(&hit) = popup.hits.get(popup.sel) {
                    acts.push(HistAct::Insert(hit));
                }
            }

            let area = egui::Area::new(Id::new(("history_popup", id)))
                .order(egui::Order::Foreground)
                .pivot(Align2::LEFT_BOTTOM)
                .fixed_pos(Pos2::new(strip_rect.left(), strip_rect.top() - 6.0));
            let aresp = area.show(ctx, |ui| {
                // Depth by shadow + surface, never stroke (seamless doctrine).
                egui::Frame::new()
                    .fill(SURFACE)
                    .corner_radius(CornerRadius::same(8))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 6],
                        blur: 28,
                        spread: 0,
                        color: Color32::from_black_alpha(150),
                    })
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(width - 20.0);
                        // Header: search field + entry count.
                        ui.horizontal(|ui| {
                            let te = ui.add(
                                egui::TextEdit::singleline(&mut popup.query)
                                    // Stable id so the open-frame focus grab
                                    // (LOW-9) can target it before this
                                    // widget exists.
                                    .id(Id::new(("history_query", id)))
                                    .desired_width(240.0)
                                    .hint_text("Search command history")
                                    .font(FontId::proportional(12.0))
                                    .margin(Margin::symmetric(8, 5)),
                            );
                            te.request_focus();
                            if te.changed() {
                                popup.hits = history::filter(&popup.entries, &popup.query);
                                popup.sel = 0;
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{} of {}",
                                        popup.hits.len(),
                                        popup.entries.len()
                                    ))
                                    .size(10.0)
                                    .color(TEXT_MUTED),
                                );
                            });
                        });
                        ui.add_space(6.0);
                        if popup.entries.is_empty() {
                            ui.add_space(18.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(
                                        "No commands yet \u{2014} hooked terminals record their history here",
                                    )
                                    .size(12.0)
                                    .color(TEXT_FAINT),
                                );
                            });
                            ui.add_space(18.0);
                            return;
                        }
                        if popup.hits.is_empty() {
                            ui.add_space(18.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new("No matches")
                                        .size(12.0)
                                        .color(TEXT_FAINT),
                                );
                            });
                            ui.add_space(18.0);
                            return;
                        }
                        const ROW_H: f32 = 44.0;
                        let mut scroll = egui::ScrollArea::vertical().max_height(420.0);
                        if popup.kb_moved {
                            // Keep the keyboard selection in view: show_rows
                            // renders only visible rows, so an off-screen sel
                            // can't scroll_to_me — steer the offset instead.
                            let target =
                                (popup.sel as f32 * ROW_H - 180.0).max(0.0);
                            scroll = scroll.vertical_scroll_offset(target);
                        }
                        scroll.show_rows(ui, ROW_H, popup.hits.len(), |ui, range| {
                            for ri in range {
                                let e = &popup.entries[popup.hits[ri] as usize];
                                let (rect, rowresp) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), ROW_H),
                                    Sense::click(),
                                );
                                let hovered = rowresp.hovered();
                                let p = ui.painter().clone();
                                // Selection/hover = background shift only.
                                if ri == popup.sel {
                                    p.rect_filled(rect, CornerRadius::same(6), ACCENT_SUBTLE);
                                }
                                if hovered {
                                    p.rect_filled(rect, CornerRadius::same(6), SURFACE_2);
                                }
                                let line1_y = rect.min.y + 14.0;
                                let line2_y = rect.max.y - 12.0;
                                // Status glyph: open ⇒ accent dot; failed ⇒
                                // ✕ code; success/None ⇒ nothing (absence of
                                // red IS success).
                                if e.open {
                                    p.circle_filled(
                                        Pos2::new(rect.min.x + 14.0, line1_y),
                                        3.0,
                                        ACCENT,
                                    );
                                } else if e.exit.is_some_and(|x| x != 0) {
                                    // Wide codes (e.g. NTSTATUS -1073741510,
                                    // Ctrl+C) overflow the glyph lane into
                                    // the command text — cap to a bare ✕.
                                    let code = e.exit.unwrap_or(0);
                                    let txt = if (-99..=999).contains(&code) {
                                        format!("\u{2715} {code}")
                                    } else {
                                        "\u{2715}".to_string()
                                    };
                                    p.text(
                                        Pos2::new(rect.min.x + 6.0, line1_y),
                                        Align2::LEFT_CENTER,
                                        txt,
                                        FontId::proportional(10.0),
                                        DANGER,
                                    );
                                }
                                // Command, single-line-ified, clipped lane.
                                let cmd_one = e.cmd.replace(['\r', '\n'], " ");
                                let lane_r = rect.max.x - if hovered { 60.0 } else { 76.0 };
                                let galley = p.layout_no_wrap(
                                    cmd_one,
                                    FontId::monospace(12.0),
                                    TEXT,
                                );
                                let cp = p.with_clip_rect(Rect::from_min_max(
                                    Pos2::new(rect.min.x + 32.0, rect.min.y),
                                    Pos2::new(lane_r, rect.max.y),
                                ));
                                cp.galley(
                                    Pos2::new(
                                        rect.min.x + 32.0,
                                        line1_y - galley.size().y / 2.0,
                                    ),
                                    galley,
                                    TEXT,
                                );
                                // Line 2: terminal (dimmed when dead) · cwd,
                                // ×N badge when deduped.
                                let name_col =
                                    if e.term_dead { TEXT_FAINT } else { TEXT_MUTED };
                                let mut sub = e.term_name.clone();
                                if let Some(cwd) = &e.cwd {
                                    sub.push_str(" \u{00b7} ");
                                    sub.push_str(&middle_ellipsize(
                                        &cwd.to_string_lossy(),
                                        36,
                                    ));
                                }
                                let sg = p.layout_no_wrap(
                                    sub,
                                    FontId::proportional(11.0),
                                    name_col,
                                );
                                let sgw = sg.size().x;
                                p.galley(
                                    Pos2::new(
                                        rect.min.x + 32.0,
                                        line2_y - sg.size().y / 2.0,
                                    ),
                                    sg,
                                    name_col,
                                );
                                if e.count > 1 {
                                    p.text(
                                        Pos2::new(rect.min.x + 40.0 + sgw, line2_y),
                                        Align2::LEFT_CENTER,
                                        format!("\u{00d7}{}", e.count),
                                        FontId::proportional(10.0),
                                        TEXT_MUTED,
                                    );
                                }
                                if hovered {
                                    // Hover action cluster: Copy + Run.
                                    for (k, (icon, tip)) in [
                                        (Icon::Copy, "Copy command"),
                                        (Icon::Rerun, run_tip),
                                    ]
                                    .into_iter()
                                    .enumerate()
                                    {
                                        let bx = rect.max.x - 8.0 - (2 - k) as f32 * 24.0;
                                        let brect = Rect::from_min_size(
                                            Pos2::new(bx, rect.center().y - 9.0),
                                            Vec2::splat(18.0),
                                        );
                                        let bresp = ui.interact(
                                            brect,
                                            ui.id().with(("histrow", ri, k)),
                                            Sense::click(),
                                        );
                                        let dim = k == 1 && !run_ok;
                                        if bresp.hovered() && !dim {
                                            p.rect_filled(
                                                brect,
                                                CornerRadius::same(4),
                                                OV_HOVER,
                                            );
                                        }
                                        draw_icon(
                                            &p,
                                            brect.shrink(2.0),
                                            icon,
                                            if dim {
                                                TEXT_FAINT
                                            } else if bresp.hovered() {
                                                TEXT
                                            } else {
                                                TEXT_SECONDARY
                                            },
                                        );
                                        if bresp.on_hover_text(tip).clicked() {
                                            match k {
                                                0 => acts.push(HistAct::Copy(popup.hits[ri])),
                                                _ if !dim => {
                                                    acts.push(HistAct::Run(popup.hits[ri]))
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                } else {
                                    // At rest: relative time, right-aligned.
                                    p.text(
                                        Pos2::new(rect.max.x - 8.0, line1_y),
                                        Align2::RIGHT_CENTER,
                                        time_ago_ms(e.last_ms),
                                        FontId::proportional(10.0),
                                        TEXT_MUTED,
                                    );
                                }
                                if rowresp.clicked() {
                                    acts.push(HistAct::Insert(popup.hits[ri]));
                                }
                                if popup.kb_moved && ri == popup.sel {
                                    rowresp.scroll_to_me(Some(Align::Center));
                                }
                            }
                        });
                    });
            });
            // Primary press outside the popup AND off the strip's History
            // button closes it (press origin, not release — drags out of the
            // popup must not re-toggle).
            let prect = aresp.response.rect;
            if ctx.input(|i| {
                i.pointer.primary_pressed()
                    && i.pointer.press_origin().is_some_and(|p| {
                        !prect.contains(p) && !btn_rect.is_some_and(|b| b.contains(p))
                    })
            }) {
                close = true;
            }
        }

        for act in acts {
            match act {
                HistAct::Copy(i) => {
                    if let Some(cmd) = self
                        .history
                        .as_ref()
                        .and_then(|h| h.entries.get(i as usize))
                        .map(|e| e.cmd.clone())
                    {
                        ctx.copy_text(cmd);
                        // Copy keeps the popup open (comparison shopping).
                    }
                }
                HistAct::Insert(i) => {
                    if let Some(cmd) = self
                        .history
                        .as_ref()
                        .and_then(|h| h.entries.get(i as usize))
                        .map(|e| e.cmd.clone())
                    {
                        if let Some(st) = self.composers.get_mut(&id) {
                            st.insert_history(&cmd);
                        }
                        close = true; // focus returns via want_focus
                    }
                }
                HistAct::Run(i) => {
                    if !run_ok {
                        continue;
                    }
                    let Some(cmd) = self
                        .history
                        .as_ref()
                        .and_then(|h| h.entries.get(i as usize))
                        .map(|e| e.cmd.clone())
                    else {
                        continue;
                    };
                    // The EXACT P3 submit path (D10): insert (stashing any
                    // displaced draft), then submit with the cover pinned so
                    // the SubmitHold ghost bridges the echo — never a second
                    // submission encoder, never a gate bypass.
                    let mut bytes = Vec::new();
                    let mut cmd_submit = None;
                    if let (Some(st), Some(b)) =
                        (self.composers.get_mut(&id), self.terms.get(&id))
                    {
                        st.insert_history(&cmd);
                        let cl = composer::cover_line_for(st, b, true, now);
                        let (w, _) = st.submit(b, cl, prompt_cwd);
                        bytes = w;
                        // P6b: Cmd-family Runs ride the ledger verb (a
                        // multi-line history command was refused by the
                        // dispatch belt — it sits in the draft instead).
                        cmd_submit = st.take_submit_cmd();
                    }
                    if !bytes.is_empty() {
                        if let Some(b) = self.terms.get_mut(&id) {
                            b.scroll_to_bottom();
                            b.note_input(); // v0.1.1: freeze a pending capture
                        }
                        self.rearm_attention(id); // Bug A: user input re-arms
                        self.send(C2D::Input { id, bytes });
                    }
                    if let Some(cmdline) = cmd_submit {
                        self.send_cmd_submission(id, cmdline);
                    }
                    close = true;
                }
            }
        }
        if close {
            self.history = None;
        }
    }

    /// Open the card dashboard scoped to `folder` (None = all terminals, V-C).
    fn enter_dashboard(&mut self, folder: Option<Uuid>) {
        self.central_view = CentralView::Dashboard(folder);
        self.search = None;
    }

    /// Rebuild the search regex + full-scrollback match list for `id` after the
    /// query changes (V4). Plain-text search: metacharacters are escaped.
    fn recompute_search(&mut self, id: Uuid) {
        let query = match &self.search {
            Some(s) => s.query.clone(),
            None => return,
        };
        let (regex, matches) = if query.is_empty() {
            (None, Vec::new())
        } else {
            match RegexSearch::new(&regex_escape(&query)) {
                Ok(mut re) => {
                    let m = self
                        .terms
                        .get(&id)
                        .map(|b| b.all_matches(&mut re))
                        .unwrap_or_default();
                    (Some(re), m)
                }
                Err(_) => (None, Vec::new()),
            }
        };
        let hist = self.terms.get(&id).map(|b| b.history_size()).unwrap_or(0);
        if let Some(s) = self.search.as_mut() {
            s.regex = regex;
            s.matches = matches;
            s.current = 0;
            s.matches_history = hist;
            s.last_build = Instant::now();
        }
    }

    /// Move to the next/previous match and scroll it into view (V4).
    fn search_step(&mut self, id: Uuid, forward: bool) {
        let m = {
            let Some(s) = self.search.as_mut() else { return };
            if s.matches.is_empty() {
                return;
            }
            let n = s.matches.len();
            s.current = if forward {
                (s.current + 1) % n
            } else {
                (s.current + n - 1) % n
            };
            s.last_user = Instant::now();
            s.matches[s.current].clone()
        };
        if let Some(b) = self.terms.get_mut(&id) {
            b.scroll_to_match(&m);
        }
    }

    // ────────────────────────────── UI ──────────────────────────────

    /// Merged single top bar (task #21): window chrome + the old terminal
    /// header in ONE ~36px strip. Left: app mark (glyph only — the wordmark
    /// died with the merge), sidebar toggle, activity dot + terminal name
    /// (click = inline rename, §5.4) + dimmed cwd (+ the inline scrollback
    /// search when open). Right: the read surfaces only — blocks + search —
    /// then the split-+ and dashboard buttons before the window caption
    /// buttons. Kill/Restore/Delete left the bar (task #22): lifecycle lives
    /// on the sidebar row (context menu + §5.2 hover cluster) so destructive
    /// targets never sit near window-close. The whole strip is a drag
    /// handle; widgets allocated on top capture their own clicks. Zero
    /// strokes: the SURFACE fill against the content below is the only
    /// boundary (seamless doctrine).
    fn titlebar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, maximized: bool) {
        egui::Panel::top("titlebar")
            .frame(egui::Frame::new().fill(SURFACE).inner_margin(Margin::ZERO))
            .show(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), TITLEBAR_H),
                    Sense::hover(),
                );

                // Whole strip is a drag handle; buttons allocated afterwards sit
                // on top and capture their own clicks. Double-click toggles max.
                let drag = ui.interact(rect, Id::new("titlebar-drag"), Sense::click_and_drag());
                if drag.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }

                // App mark (glyph only; it carries the brand by itself and the
                // row gets the wordmark's space back). Railed, it centers
                // exactly over the 44px rail's glyph axis (x=22) so the
                // collapsed column reads as one centered stack.
                let mark_x = if self.prefs.sidebar_collapsed { 13.0 } else { 12.0 };
                let gr = Rect::from_min_size(
                    Pos2::new(rect.min.x + mark_x, rect.center().y - 9.0),
                    Vec2::splat(18.0),
                );
                draw_icon(ui.painter(), gr, Icon::Terminal, ACCENT);

                // Caption buttons (right): minimize, maximize/restore, close.
                // Standard 46px hit targets at full bar height (edge snap and
                // Fitts-friendly corners survive the slimmer bar).
                let btn_w = 46.0;
                let close_rect = Rect::from_min_max(
                    Pos2::new(rect.max.x - btn_w, rect.min.y),
                    Pos2::new(rect.max.x, rect.max.y),
                );
                let max_rect = close_rect.translate(Vec2::new(-btn_w, 0.0));
                let min_rect = max_rect.translate(Vec2::new(-btn_w, 0.0));
                if caption_button(ui, min_rect, Icon::WinMin, false).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                let max_icon = if maximized { Icon::WinRestore } else { Icon::WinMax };
                if caption_button(ui, max_rect, max_icon, false).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                if caption_button(ui, close_rect, Icon::Close, true).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }

                // Terminal context: the selected terminal, in Terminal view
                // only. The dashboard has its own header strip, and terminal
                // actions there would target something invisible.
                // LOW-8: clone only the four display fields the titlebar
                // reads, not the whole TerminalMeta every frame. v0.1.1: the
                // cwd field is the SHARED display rule's output
                // (display_cwd — live_cwd first, POSIX-honest fallbacks),
                // not the raw launch cwd: the old `m.cwd` clone showed
                // `C:\Users\zany` forever for a Linux session.
                let term_meta = match self.central_view {
                    CentralView::Terminal => self.selected.and_then(|id| {
                        self.state
                            .terminal(id)
                            .map(|m| (id, m.status, m.asleep, m.display_cwd(), m.name.clone()))
                    }),
                    CentralView::Dashboard(_) => None,
                };

                // ── Right cluster (right-to-left, hugging the captions):
                // dashboard, split-+, attention pill, then the terminal
                // actions. The split-+/dashboard pair deliberately separates
                // the terminal ✕ from the window close (misclick hazard).
                self.blocks_btn_rect = None;
                let rr = Rect::from_min_max(
                    Pos2::new(rect.min.x + 120.0, rect.min.y),
                    Pos2::new(min_rect.min.x - 6.0, rect.max.y),
                );
                let mut rui = ui.new_child(
                    UiBuilder::new()
                        .max_rect(rr)
                        .layout(Layout::right_to_left(Align::Center)),
                );
                rui.spacing_mut().item_spacing.x = 4.0;
                if icon_button(&mut rui, Icon::Grid, false)
                    .on_hover_text("Dashboard \u{2014} all terminals")
                    .clicked()
                {
                    self.enter_dashboard(None);
                }
                self.split_plus_button(&mut rui, ctx);
                self.attention_pill(&mut rui);
                if let Some((id, ..)) = &term_meta {
                    let id = *id;
                    rui.add_space(6.0);
                    // Kill/Restore/Delete left the bar (task #22): lifecycle
                    // actions live on the sidebar row — its context menu
                    // (Kill/Restart + Delete) and the §5.2 hover cluster
                    // (✕ confirm, dead-row ↻). The bar keeps the read/search
                    // surfaces only.
                    // Magnifier toggle (V4): the visible entry point.
                    if icon_button(&mut rui, Icon::Search, false)
                        .on_hover_text("Search scrollback")
                        .clicked()
                    {
                        if self.search.is_some() {
                            self.search = None;
                        } else {
                            // Extracted body (QOL §3.2): the menu's Find row
                            // shares it. Mutual exclusion lives inside.
                            self.open_search();
                        }
                    }
                    // Blocks recall panel toggle (P2): shown only when
                    // records exist — a claude/cmd tab shows zero block
                    // chrome anywhere. A corner dot flags failures.
                    if self.blocks.get(&id).is_some_and(|b| !b.recs.is_empty()) {
                        let failed = self
                            .blocks
                            .get_mut(&id)
                            .map(|b| b.failed_count())
                            .unwrap_or(0);
                        let resp = icon_button(&mut rui, Icon::Blocks, false)
                            .on_hover_text("Command blocks");
                        if failed > 0 {
                            rui.painter().circle_filled(
                                Pos2::new(resp.rect.max.x - 7.0, resp.rect.min.y + 7.0),
                                3.0,
                                DANGER,
                            );
                        }
                        self.blocks_btn_rect = Some(resp.rect);
                        if resp.clicked() {
                            self.blocks_panel = if self.blocks_panel.is_some() {
                                None
                            } else {
                                // Mutual exclusion with the history popup
                                // (P4 §3.5) and the launcher (selector §1.3).
                                self.history = None;
                                self.launcher = None;
                                Some(BlocksPanel::new())
                            };
                        }
                    }
                    // SLEEP S15/§7.2: the bar action slot — accent "Wake"
                    // ghost for a presented-Asleep terminal (the old
                    // Restore-slot pattern; task #22 moved Restore to the
                    // row, but Wake is the return-to-work affordance and
                    // earns the bar), dim "sleeping…" during the transient.
                    match self.presented(id) {
                        PresentedStatus::Asleep => {
                            if ghost_button_auto(&mut rui, "Wake", ACCENT).clicked() {
                                self.send(C2D::RestartTerminal { id });
                            }
                        }
                        PresentedStatus::Sleeping => {
                            rui.label(
                                RichText::new("sleeping\u{2026}")
                                    .size(12.0)
                                    .color(TEXT_MUTED),
                            );
                        }
                        _ => {}
                    }
                }
                let right_edge = rui.min_rect().min.x;

                // ── Left cluster: logo + sidebar toggle live in the
                // sidebar's span of the bar.
                let lr = Rect::from_min_max(
                    Pos2::new(rect.min.x + 36.0, rect.min.y),
                    Pos2::new(right_edge - 4.0, rect.max.y),
                );
                let mut lui = ui.new_child(
                    UiBuilder::new()
                        .max_rect(lr)
                        .layout(Layout::left_to_right(Align::Center)),
                );
                lui.spacing_mut().item_spacing.x = 4.0;
                // The toggle's x=36 slot straddles the 44px rail boundary, so
                // while railed it lives in the rail itself (sidebar_rail's
                // centered header stack); the titlebar hosts it only at full
                // width. Same flag flips both sites — never two toggles.
                if !self.prefs.sidebar_collapsed
                    && icon_button(&mut lui, Icon::Sidebar, false)
                        .on_hover_text("Collapse sidebar")
                        .clicked()
                {
                    self.prefs.sidebar_collapsed = true;
                    self.save_prefs();
                }

                // Terminal identity (dot + name + dimmed cwd + inline search)
                // starts at the TERMINAL column — right of the sidebar
                // boundary — so the name reads as a title above the terminal
                // content, not as window chrome (user-directed). Mirrors the
                // sidebar panel's animated width (same id + target ⇒ same
                // value this frame), so the name slides with collapse; the
                // rail leaves no room, so clamp right of the toggle there.
                let sb_target = if self.prefs.sidebar_collapsed { 44.0 } else { 240.0 };
                let sidebar_w = ctx.animate_value_with_time(
                    Id::new("sidebar-width"),
                    sb_target,
                    0.15,
                );
                let name_x = (rect.min.x + sidebar_w + 12.0).max(lui.min_rect().max.x + 8.0);
                if let Some((id, status, asleep, cwd, name)) = &term_meta {
                    let id = *id;
                    let nr = Rect::from_min_max(
                        Pos2::new(name_x, rect.min.y),
                        Pos2::new(right_edge - 4.0, rect.max.y),
                    );
                    let mut lui = ui.new_child(
                        UiBuilder::new()
                            .max_rect(nr)
                            .layout(Layout::left_to_right(Align::Center)),
                    );
                    lui.spacing_mut().item_spacing.x = 4.0;
                    // Activity dot (the old header's status dot); asleep
                    // presents the moon (SLEEP S14 — bar surface bite).
                    let bar_presented = presented_status(*status, *asleep);
                    let (r, _) = lui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    match bar_presented {
                        PresentedStatus::Running => {
                            lui.painter()
                                .circle_filled(r.center(), 6.0, SUCCESS.gamma_multiply(0.22));
                            lui.painter().circle_filled(r.center(), 4.0, SUCCESS);
                        }
                        PresentedStatus::Asleep | PresentedStatus::Sleeping => {
                            draw_moon(lui.painter(), r.center(), 4.5, TEXT_MUTED, SURFACE);
                        }
                        PresentedStatus::Dead => {
                            lui.painter().circle_filled(r.center(), 4.0, TEXT_MUTED);
                        }
                    }
                    lui.add_space(2.0);

                    // Name + cwd under a pixel budget that preserves the drag
                    // region (bar_text_budget: name middle-ellipsizes first,
                    // cwd hides second — unit-tested ordering).
                    let search_w = if self.search.is_some() { SEARCH_CLUSTER_W } else { 0.0 };
                    let span = (right_edge - 4.0 - lui.cursor().min.x - search_w).max(0.0);
                    let name_font = semibold(13.0);
                    let cwd_font = FontId::monospace(11.0);
                    let cwd_full = middle_ellipsize(cwd, 52);
                    let name_g = lui.painter().layout_no_wrap(
                        name.clone(),
                        name_font.clone(),
                        TEXT,
                    );
                    let cwd_g = lui.painter().layout_no_wrap(
                        cwd_full.clone(),
                        cwd_font.clone(),
                        TEXT_MUTED,
                    );
                    let (name_px, show_cwd) =
                        bar_text_budget(rect.width(), span, name_g.size().x, cwd_g.size().x);
                    let name_txt =
                        ellipsize_to_px(lui.painter(), name, &name_font, name_px);
                    // The bar title is a rename entry point (§5.4): click =
                    // inline rename in place, hover = fill-shift pill +
                    // tooltip (mouse-first, doctrine hover grammar).
                    let renaming_bar = matches!(
                        &self.renaming,
                        Some(rn) if rn.target == RenameTarget::Term(id)
                            && rn.host == RenameHost::Bar
                    );
                    if renaming_bar {
                        let w = 240.0f32.min(span.max(120.0));
                        let (er, _) =
                            lui.allocate_exact_size(Vec2::new(w, 24.0), Sense::hover());
                        self.rename_editor(&mut lui, er, name_font.clone());
                    } else {
                        let ng = lui.painter().layout_no_wrap(
                            name_txt,
                            name_font.clone(),
                            TEXT,
                        );
                        let (nr2, nresp) = lui.allocate_exact_size(
                            ng.size() + Vec2::new(8.0, 6.0),
                            Sense::click(),
                        );
                        let nresp = nresp
                            .on_hover_cursor(egui::CursorIcon::Text)
                            .on_hover_text("Rename");
                        if nresp.hovered() {
                            lui.painter().rect_filled(
                                nr2,
                                CornerRadius::same(4),
                                SURFACE_2,
                            );
                        }
                        let tp = Pos2::new(
                            nr2.min.x + 4.0,
                            nr2.center().y - ng.size().y / 2.0,
                        );
                        lui.painter().galley(tp, ng, TEXT);
                        if nresp.clicked() {
                            self.start_rename(
                                RenameTarget::Term(id),
                                name.clone(),
                                RenameHost::Bar,
                            );
                        }
                    }
                    if show_cwd {
                        lui.add_space(4.0);
                        lui.label(RichText::new(cwd_full).font(cwd_font).color(TEXT_MUTED));
                    }

                    // Inline scrollback search (V4), shown when the magnifier
                    // is toggled. Lives in the bar's left flow, after the
                    // identity cluster (its width is pre-reserved above).
                    if self.search.is_some() {
                        lui.add_space(8.0);
                        // Read Enter/Shift/Esc before the field consumes them.
                        let (enter, shift, esc) = lui.input(|i| {
                            (
                                i.key_pressed(egui::Key::Enter),
                                i.modifiers.shift,
                                i.key_pressed(egui::Key::Escape),
                            )
                        });
                        let mut q =
                            self.search.as_ref().map(|s| s.query.clone()).unwrap_or_default();
                        let te = lui.add(
                            egui::TextEdit::singleline(&mut q)
                                .desired_width(180.0)
                                .hint_text("Search scrollback")
                                .font(FontId::proportional(13.0))
                                .margin(Margin::symmetric(8, 6)),
                        );
                        te.request_focus();
                        if te.changed() {
                            if let Some(s) = self.search.as_mut() {
                                s.query = q;
                                s.last_user = Instant::now();
                            }
                            self.recompute_search(id);
                        }
                        let (cur, total) = self
                            .search
                            .as_ref()
                            .map(|s| (s.current + 1, s.matches.len()))
                            .unwrap_or((0, 0));
                        let count = if total == 0 {
                            "0/0".to_string()
                        } else {
                            format!("{cur}/{total}")
                        };
                        lui.label(RichText::new(count).size(11.0).color(TEXT_MUTED));
                        let prev = icon_button(&mut lui, Icon::ChevronUp, false)
                            .on_hover_text("Previous match");
                        let next = icon_button(&mut lui, Icon::ChevronDown, false)
                            .on_hover_text("Next match");
                        let close =
                            icon_button(&mut lui, Icon::Close, false).on_hover_text("Close search");
                        if next.clicked() || (enter && !shift) {
                            self.search_step(id, true);
                        }
                        if prev.clicked() || (enter && shift) {
                            self.search_step(id, false);
                        }
                        if close.clicked() || esc {
                            self.search = None;
                        }
                    }
                }
            });
    }

    /// Compact split-+ (D1/§3.1, merged-bar form): icon + chevron, no text
    /// label — the main-zone tooltip carries the create preview instead. Main
    /// zone = instant spawn from `last_spawn`; chevron zone / right-click =
    /// the launcher palette. The chevron stays visible at rest (it IS the
    /// launcher affordance now that the label is gone).
    fn split_plus_button(&mut self, aui: &mut egui::Ui, ctx: &egui::Context) {
        let (rect, _) = aui.allocate_exact_size(Vec2::new(54.0, 28.0), Sense::hover());
        let chev_zone = Rect::from_min_max(Pos2::new(rect.max.x - 22.0, rect.min.y), rect.max);
        let main_zone = Rect::from_min_max(rect.min, Pos2::new(rect.max.x - 22.0, rect.max.y));
        let main = aui.interact(main_zone, Id::new("newterm-main"), Sense::click());
        let chev = aui.interact(chev_zone, Id::new("newterm-chev"), Sense::click());
        let t = aui.ctx().animate_bool_with_time(
            Id::new("newterm-hover"),
            main.hovered() || chev.hovered(),
            0.12,
        );
        let painter = aui.painter();
        if main.is_pointer_button_down_on() || chev.is_pointer_button_down_on() {
            painter.rect_filled(rect, CornerRadius::same(8), OV_PRESSED);
        } else if t > 0.0 {
            painter.rect_filled(rect, CornerRadius::same(8), SURFACE_2.gamma_multiply(t));
        }
        let fg = lerp_col(TEXT_SECONDARY, TEXT, t);
        let ir = Rect::from_center_size(
            Pos2::new(main_zone.center().x + 2.0, rect.center().y),
            Vec2::splat(14.0),
        );
        draw_icon(painter, ir, Icon::Plus, fg);
        let ct = aui
            .ctx()
            .animate_bool_with_time(chev.id.with("hover"), chev.hovered(), 0.12);
        let cr = Rect::from_center_size(
            Pos2::new(chev_zone.center().x - 2.0, chev_zone.center().y),
            Vec2::splat(12.0),
        );
        draw_icon(painter, cr, Icon::ChevronDown, lerp_col(TEXT_MUTED, TEXT, ct));
        self.launcher_btn_rect = Some(rect);
        // The tooltip IS the create preview (§3.1, fixes F6) — built LAZILY
        // inside on_hover_ui (r4 perf-gui L4: the titlebar paints every
        // frame; the SpawnSpec clone + format!s must only run on hover).
        let main = main
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_ui(|ui| {
                ui.label(format!(
                    "New terminal \u{2014} {}",
                    launcher::spawn_preview(&self.effective_last_spawn())
                ));
            });
        let chev = chev
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Choose what to launch\u{2026}");
        if main.clicked() {
            self.instant_create(ctx, None);
        }
        if chev.clicked() || main.secondary_clicked() || chev.secondary_clicked() {
            if self.launcher.as_ref().is_some_and(|l| !l.embedded) {
                self.close_launcher();
            } else {
                self.open_launcher(ctx, None);
            }
        }
    }

    /// Amber "N waiting" pill (V-D), inline in the merged bar's right cluster:
    /// visible only when a terminal is NeedsYou. Clicking cycles selection
    /// through waiting terminals, clearing each latch.
    fn attention_pill(&mut self, ui: &mut egui::Ui) {
        // The NeedsYou set in sidebar order, from update_activity's fleet
        // pass this frame (the clone only happens while the pill shows).
        if self.waiting.is_empty() {
            return;
        }
        let waiting = self.waiting.clone();
        let label = format!("{} waiting", waiting.len());
        let galley =
            ui.painter()
                .layout_no_wrap(label, FontId::proportional(12.0), ATTENTION);
        let w = galley.size().x + 34.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::click());
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let fill = if resp.hovered() { SURFACE_3 } else { SURFACE_2 };
        ui.painter().rect_filled(rect, CornerRadius::same(12), fill);
        // Amber dot + text.
        let dot_c = Pos2::new(rect.min.x + 13.0, rect.center().y);
        ui.painter().circle_filled(dot_c, 4.0, ATTENTION);
        ui.painter().galley(
            Pos2::new(rect.min.x + 22.0, rect.center().y - galley.size().y / 2.0),
            galley,
            ATTENTION,
        );
        if resp.clicked() {
            // Cycle to the next waiting terminal after the current selection.
            let next = self
                .selected
                .and_then(|cur| waiting.iter().position(|&id| id == cur))
                .map(|pos| waiting[(pos + 1) % waiting.len()])
                .unwrap_or(waiting[0]);
            self.select_terminal(next);
            if let Some(st) = self.activity.get_mut(&next) {
                st.needs_you = false;
                // Bug A: the click IS the ack — disarm persistently, same as
                // the view-ack in step_attention (which also lands next
                // frame, but only if the window is focused).
                st.armed = false;
            }
            self.attention_flashed.remove(&next);
        }
    }

    /// 6px resize strips on every edge/corner of an undecorated window (V1).
    /// egui-winit maps `BeginResize` to a real OS resize loop, so Aero snap and
    /// edge-drag keep working. Hosted in a foreground layer so they sit above
    /// the panels.
    fn resize_handles(&self, ui: &egui::Ui, ctx: &egui::Context, full: Rect) {
        use egui::ResizeDirection as RD;
        const T: f32 = 6.0; // edge thickness
        const C: f32 = 12.0; // corner square
        let (l, r, t, b) = (full.min.x, full.max.x, full.min.y, full.max.y);
        let handles: [(Rect, RD, CursorIcon); 8] = [
            // Corners first (they win over edges where they overlap).
            (Rect::from_min_max(Pos2::new(l, t), Pos2::new(l + C, t + C)), RD::NorthWest, CursorIcon::ResizeNwSe),
            (Rect::from_min_max(Pos2::new(r - C, t), Pos2::new(r, t + C)), RD::NorthEast, CursorIcon::ResizeNeSw),
            (Rect::from_min_max(Pos2::new(l, b - C), Pos2::new(l + C, b)), RD::SouthWest, CursorIcon::ResizeNeSw),
            (Rect::from_min_max(Pos2::new(r - C, b - C), Pos2::new(r, b)), RD::SouthEast, CursorIcon::ResizeNwSe),
            // Edges (between the corners).
            (Rect::from_min_max(Pos2::new(l, t + C), Pos2::new(l + T, b - C)), RD::West, CursorIcon::ResizeHorizontal),
            (Rect::from_min_max(Pos2::new(r - T, t + C), Pos2::new(r, b - C)), RD::East, CursorIcon::ResizeHorizontal),
            (Rect::from_min_max(Pos2::new(l + C, t), Pos2::new(r - C, t + T)), RD::North, CursorIcon::ResizeVertical),
            (Rect::from_min_max(Pos2::new(l + C, b - T), Pos2::new(r - C, b)), RD::South, CursorIcon::ResizeVertical),
        ];
        for (i, (hr, dir, cursor)) in handles.iter().enumerate() {
            let resp = ui
                .interact(*hr, Id::new(("resize", i)), Sense::drag())
                .on_hover_cursor(*cursor);
            if resp.hovered() || resp.dragged() {
                ctx.set_cursor_icon(*cursor);
            }
            if resp.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(*dir));
            }
        }
    }

    fn disconnected_banner(&mut self, root: &mut egui::Ui) {
        let connected = self.ipc.as_ref().is_some_and(|c| c.is_connected());
        if connected {
            return;
        }
        egui::Panel::top("disconnected")
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .inner_margin(Margin::symmetric(12, 8)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, DANGER);
                    ui.label(
                        RichText::new("Daemon unreachable \u{2014} reconnecting\u{2026}")
                            .size(13.0)
                            .color(TEXT_SECONDARY),
                    );
                });
                // Danger hairline under the banner.
                let rect = ui.max_rect();
                ui.painter().line_segment(
                    [
                        Pos2::new(rect.min.x - 12.0, rect.max.y + 8.0),
                        Pos2::new(rect.max.x + 12.0, rect.max.y + 8.0),
                    ],
                    Stroke::new(1.0, DANGER.gamma_multiply(0.5)),
                );
            });
    }

    /// Transient restart notice (R8a) and dismissable error banner (R4).
    fn banners(&mut self, root: &mut egui::Ui) {
        if let Some((msg, t)) = self.notice.clone() {
            if t.elapsed() > Duration::from_secs(6) {
                self.notice = None;
            } else {
                egui::Panel::top("notice")
                    .frame(egui::Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(12, 8)))
                    .show(root, |ui| {
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                            ui.painter().circle_filled(r.center(), 4.0, ACCENT);
                            ui.label(RichText::new(msg).size(13.0).color(TEXT_SECONDARY));
                        });
                    });
            }
        }
        if let Some((msg, t)) = self.last_error.clone() {
            if t.elapsed() > Duration::from_secs(10) {
                self.last_error = None;
            } else {
                let mut dismiss = false;
                egui::Panel::top("error")
                    .frame(egui::Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(12, 8)))
                    .show(root, |ui| {
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                            ui.painter().circle_filled(r.center(), 4.0, DANGER);
                            ui.label(RichText::new(msg).size(13.0).color(DANGER_HOVER));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ghost_button_auto(ui, "Dismiss", TEXT_SECONDARY).clicked() {
                                    dismiss = true;
                                }
                            });
                        });
                    });
                if dismiss {
                    self.last_error = None;
                }
            }
        }
    }

    // ───────────────────── launcher + instant create (selector spec) ─────────────────────

    /// The effective sticky spawn (§3.1): `Prefs.last_spawn`, else PowerShell
    /// in the user's home dir (the legacy `last_cwd` seeds the cwd if it was
    /// ever customized).
    fn effective_last_spawn(&self) -> SpawnSpec {
        self.prefs
            .last_spawn
            .clone()
            .unwrap_or_else(|| launcher::default_spawn(&self.prefs.last_cwd))
    }

    /// v0.1.2: stamp the create-time WSL welcome-banner choice (Settings →
    /// Terminal pref, default ON) onto a spec about to be sent. Lives on
    /// `ShellCfg` so the daemon — which owns the spawn — sees it; ctl/probe
    /// creates never pass through here and stay banner-free (the serde
    /// default). Non-WSL specs are untouched.
    fn stamp_wsl_banner(&self, nt: &mut crate::state::NewTerminal) {
        if matches!(
            crate::state::shell_family(&nt.kind, &nt.program, &nt.args),
            crate::state::ShellFamily::WslShell { .. }
        ) {
            nt.shell_cfg.get_or_insert_with(Default::default).wsl_motd =
                self.prefs.wsl_welcome_banner;
        }
    }

    /// Record a successful create into the sticky prefs: `last_spawn`
    /// overwritten, `recent_spawns` MRU ring of 8 deduped by (kind_tag, cwd).
    fn note_spawn(&mut self, spec: SpawnSpec) {
        self.prefs
            .recent_spawns
            .retain(|s| !(s.kind_tag == spec.kind_tag && s.cwd == spec.cwd));
        self.prefs.recent_spawns.insert(0, spec.clone());
        self.prefs.recent_spawns.truncate(8);
        self.prefs.last_spawn = Some(spec);
        self.save_prefs();
    }

    /// Instant create (D1/§3.1): deterministic spawn from the sticky spec,
    /// auto-named + uniquified, auto-selected via `pending_create`. On a TRUE
    /// first run (no recents, no terminals) it opens the launcher instead —
    /// a first-time user should see their options once (Q1).
    fn instant_create(&mut self, ctx: &egui::Context, folder: Option<Uuid>) {
        if self.prefs.recent_spawns.is_empty() && self.state.terminals.is_empty() {
            self.open_launcher(ctx, folder);
            return;
        }
        let last = self.effective_last_spawn();
        let nt = {
            let taken: Vec<&str> =
                self.state.terminals.iter().map(|t| t.name.as_str()).collect();
            launcher::spec_from_spawn(&last, folder, &taken)
        };
        let Some(mut nt) = nt else { return };
        self.stamp_wsl_banner(&mut nt);
        let name = nt.name.clone();
        self.send(C2D::CreateTerminal { spec: nt });
        self.pending_create = Some((name, Instant::now()));
        self.note_spawn(last);
    }

    /// Open the launcher palette (§4.1): mutual exclusion with every other
    /// floating surface, candidates built now (click-time cost), open-frame
    /// focus grab so the first fast keystroke lands in the query (LOW-9).
    /// While the §6.1 empty-state embed is SHOWING, the embed IS the launcher
    /// — just target its query (and preset its folder chip). The embed only
    /// renders in the Terminal central view: on the DASHBOARD with zero
    /// terminals every entry point (titlebar chevron, sidebar +, folder
    /// "New terminal here…") used to mutate the invisible embed and read as
    /// a dead control (field report #2) — those now open the overlay.
    fn open_launcher(&mut self, ctx: &egui::Context, folder: Option<Uuid>) {
        self.search = None;
        self.blocks_panel = None;
        self.history = None;
        match launcher::open_target(
            self.state.terminals.is_empty(),
            matches!(self.central_view, CentralView::Dashboard(_)),
        ) {
            launcher::OpenTarget::Embed => {
                if let Some(l) = self.launcher.as_mut() {
                    l.folder = folder.or(l.folder);
                } else {
                    // First frame after a snapshot emptied the list: the
                    // embed renders next frame either way — creating it now
                    // just makes the focus grab land.
                    self.launcher = Some(self.fresh_launcher(folder, true));
                }
            }
            launcher::OpenTarget::Overlay => {
                self.launcher = Some(self.fresh_launcher(folder, false));
            }
        }
        ctx.memory_mut(|m| m.request_focus(Id::new("launcher_q")));
    }

    fn fresh_launcher(&self, folder: Option<Uuid>, embedded: bool) -> LauncherState {
        // One synchronous scan per open (Q2: head-reads only, <50ms at dozens
        // of sessions); drift rebuilds reuse this list — never a rescan.
        // Shell rows likewise: one Lxss registry read per open (P6 inv. 7).
        let sessions = import::scan();
        let mut st = LauncherState::new(folder, embedded, shells::shell_choices(), sessions);
        self.rebuild_launcher(&mut st);
        st
    }

    /// (Re)build the candidate index from current client-side state. Called
    /// at open and on debounced blocks_stamp drift — never per frame (§1.7).
    fn rebuild_launcher(&self, st: &mut LauncherState) {
        let lists: Vec<(Uuid, &[BlockRec])> = self
            .blocks
            .iter()
            .map(|(id, b)| (*id, b.recs.as_slice()))
            .collect();
        st.cands = launcher::build(
            &self.state,
            &lists,
            &st.shells,
            &st.sessions,
            &self.prefs.recent_spawns,
            &self.effective_last_spawn().cwd,
        );
        st.hits = launcher::filter(&st.cands, &st.query);
        st.sel = st.sel.min(st.hits.len().saturating_sub(1));
        st.built = self.blocks_stamp;
        st.built_at = Instant::now();
        if std::env::var("TC_TRACE_LAUNCHER").ok().as_deref() == Some("1") {
            log::info!("[launcher] build cands={}", st.cands.len());
        }
    }

    /// Debounced drift rebuild (history-popup pattern, §9): Blocks frames /
    /// prunes / reconnects bump `blocks_stamp`; an open launcher follows at
    /// most twice per second.
    fn launcher_drift_rebuild(&mut self) {
        let need = self.launcher.as_ref().is_some_and(|l| {
            l.built != self.blocks_stamp
                && (l.built == u64::MAX || l.built_at.elapsed() >= Duration::from_millis(500))
        });
        if need {
            let mut st = self.launcher.take().expect("checked");
            self.rebuild_launcher(&mut st);
            self.launcher = Some(st);
        }
    }

    /// One activation ⇒ one `CreateTerminal` + pending auto-select + sticky
    /// prefs update (§4.4). Refuse-over-guess: an unmappable candidate does
    /// nothing.
    fn launcher_activate(&mut self, act: launcher::Activation) {
        let Some(l) = self.launcher.as_ref() else { return };
        let folder = l.folder;
        let last = self.effective_last_spawn();
        let built = {
            let taken: Vec<&str> =
                self.state.terminals.iter().map(|t| t.name.as_str()).collect();
            match act {
                launcher::Activation::Cand(i) => l
                    .cands
                    .get(i as usize)
                    .and_then(|c| launcher::spec_for(c, &last, folder, &taken)),
                // The typed SHELL row spawns what it names — a claude-kind
                // sticky spawn heals to the PowerShell default (the claude
                // choice is its own explicit row below).
                launcher::Activation::Typed(p) => {
                    launcher::dir_spec(&p, &launcher::shell_last(&last), folder, &taken)
                }
                launcher::Activation::TypedClaude(p) => {
                    launcher::claude_dir_spec(&p, folder, &taken)
                }
                launcher::Activation::Custom { prog, args } => {
                    launcher::custom_spec(&prog, &args, &last.cwd, folder, &taken)
                }
                // P6c freeform ssh: host line + the remote-hooks opt-in.
                launcher::Activation::Ssh { host_line, remote_hooks } => {
                    launcher::ssh_spec(&host_line, remote_hooks, folder, &taken)
                }
            }
        };
        let Some((mut nt, spawn)) = built else { return };
        self.stamp_wsl_banner(&mut nt);
        let name = nt.name.clone();
        self.send(C2D::CreateTerminal { spec: nt });
        self.pending_create = Some((name, Instant::now()));
        self.note_spawn(spawn);
    }

    /// Close the launcher; an armed composer regains focus (the history
    /// popup's exact Esc contract, §8).
    fn close_launcher(&mut self) {
        self.launcher = None;
        if let Some(id) = self.selected {
            if let Some(st) = self.composers.get_mut(&id) {
                if st.mode == ComposerMode::Compose {
                    st.want_focus = true;
                }
            }
        }
    }

    /// Apply a launcher view result (shared by the overlay and the embed).
    fn handle_launcher_out(&mut self, out: launcher::LauncherOut) {
        if out.new_folder {
            self.modal = Some(Modal::NewFolder(String::new()));
        }
        if let Some(act) = out.activate {
            self.launcher_activate(act);
            // Activation closes the overlay (§4.4); the embed persists until
            // the created terminal's snapshot replaces the empty state.
            if self.launcher.as_ref().is_some_and(|l| !l.embedded) {
                self.close_launcher();
            }
        } else if out.close {
            self.close_launcher();
        }
    }

    /// The launcher overlay Area (§4.1): anchored under the titlebar,
    /// centered on the central panel, 90ms opacity fade (the tc-dialog-fade
    /// mechanism), click-outside closes (split-+ rect exempt).
    fn launcher_overlay_ui(&mut self, ctx: &egui::Context) {
        let open = self.launcher.as_ref().is_some_and(|l| !l.embedded);
        if !open {
            ctx.animate_bool_with_time(Id::new("launcher-fade"), false, 0.0);
            return;
        }
        self.launcher_drift_rebuild();
        let Some(central) = self.central_rect else { return };
        let width = 560.0f32.min(central.width() - 32.0).max(200.0);
        let max_h = central.height() * 0.62;
        let t = ctx.animate_bool_with_time(Id::new("launcher-fade"), true, 0.09);
        if t < 1.0 {
            ctx.request_repaint();
        }

        let mut st = self.launcher.take().expect("checked open");
        let keys_enabled = self.modal.is_none();
        let out = {
            let vc = launcher::ViewCtx {
                folders: &self.state.folders,
                keys_enabled,
                embedded: false,
                width,
                max_h,
            };
            let area = egui::Area::new(Id::new("launcher"))
                .order(egui::Order::Foreground)
                .pivot(Align2::CENTER_TOP)
                .fixed_pos(Pos2::new(central.center().x, central.min.y + 8.0));
            let aresp = area.show(ctx, |ui| {
                ui.multiply_opacity(t);
                egui::Frame::new()
                    .fill(SURFACE)
                    .corner_radius(CornerRadius::same(10))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 6],
                        blur: 28,
                        spread: 0,
                        color: Color32::from_black_alpha(150),
                    })
                    .inner_margin(Margin::same(6))
                    .show(ui, |ui| launcher::view(ui, &mut st, &vc))
                    .inner
            });
            let mut out = aresp.inner;
            // A primary press outside the palette (and off the split-+
            // button) closes it — press origin, so drags out don't re-toggle.
            let prect = aresp.response.rect;
            let btn = self.launcher_btn_rect;
            if keys_enabled
                && ctx.input(|i| {
                    i.pointer.primary_pressed()
                        && i.pointer.press_origin().is_some_and(|p| {
                            !prect.contains(p) && !btn.is_some_and(|b| b.contains(p))
                        })
                })
            {
                out.close = true;
            }
            out
        };
        self.launcher = Some(st);
        self.handle_launcher_out(out);
    }
}

impl eframe::App for App {
    /// ssh-drop §6.8: GUI exit kills running upload children — an orphaned
    /// hidden sftp.exe uploading forever is worse than a truncated partial
    /// (no resume-on-relaunch in v1; documented, honest).
    fn on_exit(&mut self) {
        // Flush a pending debounced prefs save (R3-5) before the workers die.
        if self.prefs_save_due.take().is_some() {
            self.save_prefs();
        }
        self.uploads.shutdown();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Opaque app background (no window transparency). A maximized undecorated
        // window's ~8px work-area overhang shows this fill, and the content is
        // inset to keep it inside the work area.
        BG.to_normalized_gamma_f32()
    }

    /// Background work that must run even while the window is hidden/occluded.
    /// eframe calls `logic` every frame AND on hidden repaints, but skips `ui`
    /// when the window isn't visible — so reconnect, output draining, activity
    /// derivation, and the heartbeat live here, not in `ui`. A backgrounded GUI
    /// therefore still notices a daemon restart and reconnects before refocus.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.trace.is_some() {
            self.frame_t0 = Some(Instant::now());
        }
        // Font-step tracker: frame clock + settled detection.
        if let Some(fp) = &mut self.font_perf {
            fp.frame_t0 = Some(Instant::now());
            let done = fp.committed.is_some()
                && fp.last_activity.elapsed() >= Duration::from_millis(700);
            if done {
                log::info!(
                    "[perf] fontstep settled settle_ms={} commit_ms={} frames_gt16={} \
                     frames_gt33={} max_frame_us={} ms={}",
                    (fp.last_activity - fp.t0).as_millis(),
                    fp.committed.map(|c| (c - fp.t0).as_millis()).unwrap_or(0),
                    fp.frames_gt16,
                    fp.frames_gt33,
                    fp.max_frame_us,
                    gui_ms()
                );
                self.font_perf = None;
            } else {
                // Keep frames coming so pending commits and the settled
                // check make progress even on an otherwise idle GUI.
                ctx.request_repaint_after(Duration::from_millis(150));
            }
        }
        // R3-5: flush the debounced font-size prefs save once the gesture
        // settles (the 1Hz heartbeat bounds the idle-GUI flush latency).
        if self.prefs_save_due.is_some_and(|t| Instant::now() >= t) {
            self.prefs_save_due = None;
            self.save_prefs();
        }
        self.reconnect_if_needed(ctx);
        // Mirror the selection for the IPC reader's repaint coalescing
        // (background-terminal Output defers to the 100ms chrome cadence).
        // Selection changes are input-driven and input repaints immediately,
        // so this is never more than one frame stale.
        if let Some(ipc) = &self.ipc {
            ipc.set_selected(self.selected);
        }
        self.drain_ipc(ctx);
        // ssh-drop (#26): upload worker events — toast morphs + the
        // completion-time paste (§6.4). Runs while occluded too, so an
        // upload finishing behind another window still pastes/toasts.
        self.drain_uploads(ctx);
        // Attribution Layer 3: install-result toasts + the per-host consent
        // trigger (first claude use in an ssh terminal).
        self.drain_claude_hooks();
        self.scan_claude_hook_consent(ctx);
        // Codex attribution (task #30): install-result toasts + the first-use
        // consent trigger (local ~/.codex and per-host ssh).
        self.drain_codex_hooks();
        self.scan_codex_hook_consent(ctx);
        // #34: update-engine plumbing — prefs mirror, skip-clear writeback,
        // engine toasts, post-update daemon health check.
        self.pump_updates();
        if let Some(t) = &mut self.trace {
            t.report(ctx);
        }

        // Synchronized output (DECSET 2026): vte defers a sync block inside
        // the parser until ESU, but its 150ms time cap is ours to enforce —
        // flush any expired block and schedule a wakeup for pending ones so a
        // stuck BSU can never freeze a grid.
        let mut next_sync: Option<Instant> = None;
        for backend in self.terms.values_mut() {
            if let Some(deadline) = backend.pump_sync() {
                next_sync = Some(next_sync.map_or(deadline, |d| d.min(deadline)));
            }
        }
        if let Some(deadline) = next_sync {
            ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
        }

        // TC_DIAG_ASSUME_FOCUS (staging rigs): display-detached sessions
        // never deliver WM_SETFOCUS, so the view-ack (selected AND focused)
        // would be unreachable there — the knob stands in for focus exactly
        // like it does for the PTY-resize gate. Never set in normal use.
        let focused = ctx.input(|i| i.focused) || self.diag_assume_focus;
        self.update_activity(ctx, focused);

        // Idle heartbeat (R7): ping periodically to notice a dead daemon.
        if self.last_ping.elapsed() > Duration::from_secs(10) {
            self.send(C2D::Ping);
            self.last_ping = Instant::now();
        }
        // Keep `logic` firing on a steady cadence even while hidden, so the
        // reconnect/drain/heartbeat above continue to run in the background.
        ctx.request_repaint_after(Duration::from_secs(1));
        if self.diag_spin {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if self.diag_empty_ui {
            ui.painter().rect_filled(ui.max_rect(), CornerRadius::ZERO, BG);
            if let (Some(t), Some(t0)) = (&mut self.trace, self.frame_t0.take()) {
                t.frame_us
                    .push(t0.elapsed().as_micros().min(u64::MAX as u128) as u64);
            }
            return;
        }

        // DIAGNOSTIC one-shot (see the field docs): scripted launcher open
        // for display-detached staging rigs — no clicks or keys arrive there.
        if self.diag_open_launcher.is_some_and(|t| Instant::now() >= t) {
            self.diag_open_launcher = None;
            self.open_launcher(&ctx, None);
            if let Some(q) = self.diag_launcher_query.take() {
                if let Some(l) = self.launcher.as_mut() {
                    l.query = q;
                    l.hits = launcher::filter(&l.cands, &l.query);
                    l.sel = 0;
                }
            }
        }
        if self.diag_open_launcher.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // Keep pulsing dots animating without input while anything is Working
        // (aggregate computed by update_activity's fleet pass this frame).
        if self.any_working {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // Ctrl+wheel font zoom.
        let zoom = ctx.input(|i| {
            if i.modifiers.command_only() {
                i.zoom_delta()
            } else {
                1.0
            }
        });
        if zoom != 1.0 {
            self.font_step((zoom - 1.0).signum());
        }

        // Ctrl+, — the silent settings accelerator (G3: the footer glyph is
        // the discoverable entry; this is convention only). Consumed here so
        // it never reaches the terminal.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            if self.settings.is_some() {
                self.settings = None;
            } else {
                self.open_settings();
            }
        }

        // QOL §4.2: local drag-drop intake — runs only when the OS actually
        // dropped files on the window (egui clears the vec next frame;
        // multiple files arrive in one frame). Routing = the single router
        // (selected terminal only; winit gives no drop position — DO-NOT 6).
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.route_file_drop(dropped);
        }

        // Undecorated window: an OS-maximized borderless window overhangs the
        // work area by ~8px, so inset the whole app then paint its background.
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let full = ui.max_rect();
        let inset = if maximized { 8.0 } else { 0.0 };
        let content = full.shrink(inset);
        ui.painter().rect_filled(content, CornerRadius::ZERO, BG);

        let mut cui = ui.new_child(UiBuilder::new().max_rect(content).layout(*ui.layout()));
        self.titlebar(&mut cui, &ctx, maximized);
        self.disconnected_banner(&mut cui);
        self.banners(&mut cui);
        self.sidebar(&mut cui);
        self.central(&mut cui);

        // Launcher palette overlay (selector §4) — above the panels, below
        // the modal (D14: modal > launcher > everything else).
        self.launcher_overlay_ui(&ctx);

        // #34: the anchored update popover (Axis 5) — non-modal, above the
        // panels beside the launcher layer.
        self.update_popover_ui(&ctx);

        // #34 lifecycle: the one-time first-run welcome card.
        self.welcome_card_ui(&ctx);

        // ssh-drop (#26): the toast stack paints over the panels; an open
        // modal blocks its clicks (egui modal layer + the interactive gate).
        self.toasts_ui(&ctx);

        // Settings dialog (task #33) — immediately before show_modal, which
        // early-returns while it is open (G9: one overlay at a time).
        self.show_settings(&ctx);

        self.show_modal(&ctx);

        // Inline rename host check (§5.4): if no surface rendered the editor
        // this frame (row scrolled away, sidebar railed, view switched), that
        // is a blur — commit. `focus_pending` covers the start→first-paint
        // gap so a rename begun this frame isn't instantly committed.
        if let Some(rn) = &mut self.renaming {
            if rn.rendered {
                rn.rendered = false;
            } else if !rn.focus_pending {
                self.finish_rename(true);
            }
        }

        // Resize borders sit above everything; useless when maximized.
        if !maximized {
            self.resize_handles(ui, &ctx, full);
        }

        if let (Some(t), Some(t0)) = (&mut self.trace, self.frame_t0.take()) {
            t.frame_us
                .push(t0.elapsed().as_micros().min(u64::MAX as u128) as u64);
        }

        // Font-step tracker: close this frame's clock (logic start → ui end,
        // same span as the [lat] frame metric).
        if let Some(fp) = &mut self.font_perf {
            if let Some(t0) = fp.frame_t0.take() {
                let us = t0.elapsed().as_micros() as u64;
                if us > 16_000 {
                    fp.frames_gt16 += 1;
                }
                if us > 33_000 {
                    fp.frames_gt33 += 1;
                }
                fp.max_frame_us = fp.max_frame_us.max(us);
            }
        }

        if let Some(p) = &mut self.perf3 {
            if !p.first_paint_done {
                p.first_paint_done = true;
                log::info!("[perf] gui first_paint ms={}", gui_ms());
            }
            if p.paint_selected {
                p.paint_selected = false;
                log::info!("[perf] gui paint_selected ms={}", gui_ms());
            }
            if p.paint_all {
                p.paint_all = false;
                log::info!(
                    "[perf] gui paint_all cycle_ms={} ms={}",
                    p.cycle_t0.take().map(|t| t.elapsed().as_millis()).unwrap_or(0),
                    gui_ms()
                );
            }
        }
    }
}

/// The "Color" context-submenu (task #22): the 8 curated swatches + None,
/// mouse-first swatch rows. Returns Some(pick) when a row was clicked —
/// Some(Some(i)) tags, Some(None) clears.
fn color_tag_menu(ui: &mut egui::Ui, current: Option<u8>) -> Option<Option<u8>> {
    let mut pick = None;
    ui.menu_button("Color", |ui| {
        menu_item_style(ui);
        for (i, &(col, label)) in TAG_COLORS.iter().enumerate() {
            if color_swatch_row(ui, Some(col), label, current == Some(i as u8)) {
                pick = Some(Some(i as u8));
                ui.close();
            }
        }
        if color_swatch_row(ui, None, "None", current.is_none()) {
            pick = Some(None);
            ui.close();
        }
    });
    pick
}

/// One swatch row: filled circle (or a hollow ring for None) + label, menu
/// hover grammar (SURFACE_4 fill). The current pick wears a subtle outer
/// ring so the menu reads state at a glance.
fn color_swatch_row(
    ui: &mut egui::Ui,
    color: Option<Color32>,
    label: &str,
    current: bool,
) -> bool {
    let w = ui.available_width().max(120.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::click());
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(5), SURFACE_4);
    }
    let c = Pos2::new(rect.min.x + 14.0, rect.center().y);
    match color {
        Some(col) => {
            painter.circle_filled(c, 5.0, col);
        }
        None => {
            painter.circle_stroke(c, 4.5, Stroke::new(1.5, TEXT_MUTED));
        }
    }
    if current {
        painter.circle_stroke(c, 8.0, Stroke::new(1.0, TEXT_SECONDARY));
    }
    painter.text(
        Pos2::new(rect.min.x + 28.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        if resp.hovered() { TEXT } else { TEXT_SECONDARY },
    );
    resp.clicked()
}

/// Relative wall-clock age of a block's start (epoch millis), for panel rows.
fn time_ago_ms(started_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let s = now.saturating_sub(started_ms) / 1000;
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// Item styling inside egui-native menus/popups (the context-menu hover fix):
/// comfortable padding and a rounded-5 hover fill. egui's menu_style() keeps
/// item rest-fills transparent, so the SURFACE_4 hover fill from the global
/// visuals is the entire affordance — fills, never strokes (doctrine). Call
/// at the top of every context-menu / submenu closure (submenus build their
/// style from the ctx, not the parent menu Ui, so each closure needs it).
fn menu_item_style(ui: &mut egui::Ui) {
    ui.spacing_mut().button_padding = Vec2::new(8.0, 5.0);
    ui.spacing_mut().item_spacing.y = 2.0;
    let w = &mut ui.style_mut().visuals.widgets;
    for s in [&mut w.hovered, &mut w.active, &mut w.open] {
        s.corner_radius = CornerRadius::same(5);
    }
}

/// Middle-ellipsize a string to at most `max` chars, keeping head and tail (D26).
/// Pure truncation budget for the merged bar's identity cluster (task #21,
/// unit-tested): given the bar width, the pixel span from the name's left
/// edge to the right cluster, and the natural name/cwd widths, returns
/// (pixels the name may render in, whether the cwd shows).
///
/// Narrow-window order: the name middle-ellipsizes first (down to
/// MIN_NAME_PX), then the cwd hides — and the drag-region reservation
/// (DRAG_FRACTION of the bar at typical widths, a MIN_DRAG_PX hard floor
/// below them) is taken off the top before any text is placed.
fn bar_text_budget(bar_w: f32, span: f32, name_w: f32, cwd_w: f32) -> (f32, bool) {
    let reserve = (bar_w * DRAG_FRACTION).max(MIN_DRAG_PX);
    let mut avail = span - reserve;
    if avail < MIN_NAME_PX {
        // Below typical widths: keep a readable name and the hard drag floor
        // instead of the 40% goal.
        avail = MIN_NAME_PX.min(span - MIN_DRAG_PX);
    }
    if avail <= 0.0 {
        return (0.0, false);
    }
    if name_w + NAME_CWD_GAP + cwd_w <= avail {
        return (name_w, true);
    }
    let name_room = avail - NAME_CWD_GAP - cwd_w;
    if name_room >= MIN_NAME_PX {
        return (name_room, true);
    }
    (name_w.min(avail), false)
}

/// Middle-ellipsize `s` until it lays out within `budget` pixels (the merged
/// bar's name lane). Returns the string unchanged when it already fits.
fn ellipsize_to_px(painter: &egui::Painter, s: &str, font: &FontId, budget: f32) -> String {
    let full = painter.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE);
    if full.size().x <= budget {
        return s.to_string();
    }
    let total = s.chars().count();
    let mut keep = ((budget / full.size().x) * total as f32).floor() as usize;
    loop {
        let cand = middle_ellipsize(s, keep.max(3));
        let g = painter.layout_no_wrap(cand.clone(), font.clone(), Color32::WHITE);
        if g.size().x <= budget || keep <= 3 {
            return cand;
        }
        keep -= 1;
    }
}

fn middle_ellipsize(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max || max < 3 {
        return s.to_string();
    }
    let head = (max - 1) / 2;
    let tail = max - 1 - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('\u{2026}');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// Uppercase section header (D20): 11px muted, 22px tall, 12px above / 4
/// below. Color is caller-chosen (Bug B: the UNGROUPED header revealed
/// mid-drag whispers at TEXT_FAINT); returns the header rect so it can
/// register as a drop band.
fn section_header_col(ui: &mut egui::Ui, label: &str, col: Color32) -> Rect {
    ui.add_space(12.0);
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::hover());
    ui.painter().text(
        Pos2::new(rect.min.x + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        label.to_uppercase(),
        FontId::proportional(11.0),
        col,
    );
    ui.add_space(4.0);
    rect
}

/// Escape regex metacharacters so a search query matches as literal text (V4).
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Semibold family used for titles when a real semibold face is present
/// (D14/D16). Falls back to the regular UI font otherwise.
const UI_SEMIBOLD: &str = "ui_semibold";

fn install_fonts(ctx: &egui::Context) {
    use std::sync::Arc;
    let mut fonts = egui::FontDefinitions::default();
    let read = |name: &str| std::fs::read(format!("C:\\Windows\\Fonts\\{name}")).ok();

    // Monospace grid font: Cascadia Mono is variable, so bold is real wght 700.
    for candidate in ["CascadiaMono.ttf", "CascadiaCode.ttf", "consola.ttf"] {
        if let Some(bytes) = read(candidate) {
            fonts
                .font_data
                .insert("term-mono".into(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts
                .families
                .get_mut(&FontFamily::Monospace)
                .unwrap()
                .insert(0, "term-mono".into());
            break;
        }
    }

    // UI proportional font (D13). No network downloads — Windows Segoe UI.
    for candidate in ["SegoeUIVariable.ttf", "segoeui.ttf"] {
        if let Some(bytes) = read(candidate) {
            fonts
                .font_data
                .insert("ui".into(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts
                .families
                .get_mut(&FontFamily::Proportional)
                .unwrap()
                .insert(0, "ui".into());
            break;
        }
    }

    // Semibold for titles (D14/D16). Always define the family so Name(..) always
    // resolves; back it with a real semibold face if one exists, else the
    // regular UI/proportional stack.
    let semibold_backing = ["seguisb.ttf", "segoeuisb.ttf"].into_iter().find_map(|c| {
        read(c).map(|bytes| {
            fonts
                .font_data
                .insert("ui-semibold".into(), Arc::new(egui::FontData::from_owned(bytes)));
            "ui-semibold".to_string()
        })
    });
    let mut stack = Vec::new();
    if let Some(name) = semibold_backing {
        stack.push(name);
    }
    stack.push("ui".into());
    stack.push("Hack".into()); // egui's default proportional fallback
    fonts
        .families
        .insert(FontFamily::Name(UI_SEMIBOLD.into()), stack);

    ctx.set_fonts(fonts);
}

fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(UI_SEMIBOLD.into()))
}

/// Global egui visuals (D39). Field names verified against egui 0.35.
fn style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG_SIDEBAR;
    v.window_fill = SURFACE_3;
    // Seamless doctrine: windows/dialogs carry depth via their shadow, never
    // a border stroke.
    v.window_stroke = Stroke::NONE;
    v.window_corner_radius = CornerRadius::same(12);
    v.window_shadow = egui::epaint::Shadow {
        offset: [0, 16],
        blur: 48,
        spread: 0,
        color: Color32::from_black_alpha(150),
    };
    v.popup_shadow = v.window_shadow;
    v.extreme_bg_color = Color32::from_rgb(0x0E, 0x10, 0x16); // inputs (D29)
    v.faint_bg_color = SURFACE_2;
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(124, 131, 255, 77); // accent@30 (D11)
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;

    let radius = CornerRadius::same(8);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = radius;
        w.bg_stroke = Stroke::NONE;
    }
    // All widget bg_strokes stay Stroke::NONE (the loop above): egui-native
    // widgets — context menus, checkboxes, TextEdits, separators — must not
    // leak hairlines into the app (seamless doctrine). fg_stroke is TEXT/icon
    // color, not a border. Hover states read as a background shift instead.
    //
    // Hover/active fills are SURFACE_4, a step ABOVE window_fill (SURFACE_3):
    // egui menus (Frame::menu fills with window_fill and menu_style() resets
    // item rest-fill to transparent) paint hover with widgets.hovered — while
    // that equaled SURFACE_3 every context-menu item hovered invisibly, and
    // native buttons resting on a SURFACE_3 modal melted into it on hover.
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_SECONDARY);
    v.widgets.inactive.weak_bg_fill = SURFACE_2;
    v.widgets.inactive.bg_fill = SURFACE_2;
    v.widgets.hovered.weak_bg_fill = SURFACE_4;
    v.widgets.hovered.bg_fill = SURFACE_4;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.active.weak_bg_fill = SURFACE_4;
    v.widgets.active.bg_fill = SURFACE_4;
    v.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.open.weak_bg_fill = SURFACE_4;
    v.widgets.open.bg_fill = SURFACE_4;

    ctx.set_visuals(v);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(6.0, 4.0); // (D17)
        style.spacing.button_padding = Vec2::new(10.0, 4.0);
        // Floating scrollbars, 6px→10px, translucent thumb (D32).
        let mut scroll = egui::style::ScrollStyle::floating();
        scroll.floating_width = 6.0;
        scroll.bar_width = 10.0;
        style.spacing.scroll = scroll;
    });
}

/// Log GUI panics to gui.log with location + payload before the process dies.
/// Release builds have no console, so without this a crash leaves no trace.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".into());
        log::error!(
            "GUI PANIC at {loc}: {msg}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        prev(info);
    }));
}

/// The eframe window icon (#34 phase 2): raw 48x48 RGBA of the full app
/// mark, generated by assets/gen-icons.ps1 and committed, so no image-decode
/// dependency is pulled in for one icon. None if the asset is somehow the
/// wrong size (defensive — a build with a stale/absent blob just runs
/// iconless rather than panicking). Shared by the main window and the
/// lifecycle helper windows.
pub(crate) fn app_window_icon() -> Option<egui::IconData> {
    const RGBA: &[u8] = include_bytes!("../../assets/window-icon-48.rgba");
    const DIM: usize = 48;
    if RGBA.len() != DIM * DIM * 4 {
        return None;
    }
    Some(egui::IconData {
        rgba: RGBA.to_vec(),
        width: DIM as u32,
        height: DIM as u32,
    })
}

pub fn run() -> anyhow::Result<()> {
    // GUI log + panic hook so a background death is diagnosable (release builds
    // have no console). Mirrors the daemon's WriteLogger, including the R3-1
    // startup rotation cap — but rotate ONLY when no other process holds
    // gui.log open (R4, perf-daemon M1 sibling): GUIs have no instance lock,
    // and a second GUI start renaming the live GUI's log would silently move
    // its logger handle to .log.old and defeat the cap. An exclusive
    // (share_mode 0) probe open fails with a sharing violation whenever any
    // handle exists; skipping just defers rotation to the next solo start.
    let _ = std::fs::create_dir_all(crate::state::data_dir());
    // v0.1.2 field bug A (defense in depth): shortcut launches and
    // Update.exe relaunches start the GUI with CWD=<velopack-root>\current\
    // — an open CWD handle inside the dir Velopack must rename/delete.
    // Re-home to the data dir so neither this process nor anything it
    // spawns (helper/daemon are also pinned individually) can block an
    // update swap or uninstall. No GUI path resolves relative to CWD.
    let _ = std::env::set_current_dir(crate::state::data_dir());
    {
        use std::os::windows::fs::OpenOptionsExt;
        let sole_holder = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(crate::state::gui_log_path())
            .is_ok(); // probe handle drops here
        if sole_holder {
            crate::state::rotate_log_at_startup(&crate::state::gui_log_path());
        }
    }
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::state::gui_log_path())
    {
        let _ = simplelog::WriteLogger::init(
            simplelog::LevelFilter::Info,
            simplelog::Config::default(),
            f,
        );
    }
    install_panic_hook();
    log::info!("gui starting (pid {})", std::process::id());
    // #34 bin-sync (Axis 0-C), BEFORE the first daemon connect: in a
    // Velopack-installed context, deploy this build into bin\ when the
    // sidecar version differs (first install, post-update, repair). The
    // daemon the reconnect loop spawns below then already IS this version.
    crate::sync_bin_install();
    let _ = GUI_T0.set(Instant::now());
    if std::env::var("TC_PERF_STAGES").ok().as_deref() == Some("1") {
        log::info!("[perf] gui start pid={}", std::process::id());
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size([800.0, 500.0])
        .with_title("Pulse")
        // Frameless custom titlebar (V1): own our chrome, keep OS resize.
        // NOT transparent: Phase A paints an opaque background, and a
        // transparent wgpu surface was the prime suspect in silent
        // background exits. Mica/acrylic can re-add transparency in Phase B
        // with proper soak testing.
        .with_decorations(false)
        .with_resizable(true);
    // #34 phase 2: the app mark on the taskbar / alt-tab / window corner.
    if let Some(icon) = app_window_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            // Default is HIGH_THROUGHPUT: a 2-frame presentation queue, i.e.
            // up to ~2 extra vsyncs between painting a keystroke's echo and
            // it reaching glass. Our GPU work is trivial textured quads —
            // LOW_LATENCY (AutoVsync + max frame latency 1) trades nothing
            // we can measure and cuts a full frame off typing latency.
            surface: eframe::egui_wgpu::SurfaceConfig::LOW_LATENCY,
            // Backend: DX12 only, never Vulkan (perf-wave-2). The egui-wgpu
            // default (PRIMARY | GL) enumerates Vulkan first, and NVIDIA's
            // Vulkan submit/present path burns ~3.6ms of main-thread CPU per
            // painted frame on this class of machine (driver 591.86, RTX 3070)
            // vs ~0.9ms on DX12 — measured with an empty UI, window-size
            // independent, present-mode independent. At the 60fps a streaming
            // terminal paints, that difference alone was ~70% of the GUI's
            // flood-time CPU. Wave 3 dropped the GL fallback from the default
            // set too: initializing the GL/EGL stack cost ~80ms of every cold
            // start (window_ready 300-335ms → 219-242ms measured, 20-session
            // corpus) and buys nothing on Windows — the DX12 backend always
            // has an adapter (WARP software rasterizer at worst), which is
            // exactly the exotic-setup story GL was kept for. WGPU_BACKEND
            // still overrides for diagnosis (e.g. WGPU_BACKEND=gl).
            wgpu_setup: {
                use eframe::egui_wgpu::wgpu;
                let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
                setup.instance_descriptor.backends =
                    wgpu::Backends::from_env().unwrap_or(wgpu::Backends::DX12);
                // Memory hints (perf-wave MEM-1): wgpu's default
                // MemoryHints::Performance maps (DX12/gpu-allocator) to
                // 128-256MB device + 64-128MB host minimum blocks, committed
                // up-front per heap type — ~120MB dedicated VRAM + ~60MB
                // shared + ~180MB private RAM the GUI never touches (the
                // zero-terminal floor was dominated by it). MemoryUsage
                // grows in 8MB blocks instead; growth is event-driven
                // (glyph-atlas upload, vertex-buffer growth, window enlarge —
                // all already reconfigure-class events), never per-frame:
                // A/B-measured under simultaneous flood + atlas churn +
                // resize churn with NO P99.9 frame-time change, steady or
                // resize-adjacent, while the savings held (−120MB ded /
                // −60MB shared / −179MB priv). Everything else in this
                // closure replicates egui-wgpu 0.35's default
                // device_descriptor (setup.rs, without_display_handle)
                // verbatim: the GL branch matters because WGPU_BACKEND=gl is
                // the documented diagnosis path above — full default limits
                // can exceed a GL adapter's and request_device would fail
                // (GUI won't boot exactly when the fallback is needed).
                setup.device_descriptor = std::sync::Arc::new(|adapter| {
                    let base_limits = if adapter.get_info().backend == wgpu::Backend::Gl {
                        wgpu::Limits::downlevel_webgl2_defaults()
                    } else {
                        wgpu::Limits::default()
                    };
                    wgpu::DeviceDescriptor {
                        label: Some("egui wgpu device"),
                        required_limits: wgpu::Limits {
                            // Depth texture must cover the whole surface on
                            // 4k+ displays (egui's 8192 overlay, kept).
                            max_texture_dimension_2d: 8192,
                            ..base_limits
                        },
                        // The ONE deliberate change vs egui's default.
                        memory_hints: wgpu::MemoryHints::MemoryUsage,
                        ..Default::default()
                    }
                });
                setup.into()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let result = eframe::run_native(
        "Pulse",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    );
    match &result {
        Ok(()) => log::info!("gui event loop exited cleanly (pid {})", std::process::id()),
        Err(e) => log::error!("gui event loop exited with error: {e}"),
    }
    result.map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// QOL §3.2/§3.5: the context-menu enabled-state table. Asleep and
    /// Sleeping take the dead-row column for input verbs (sleep inv. 5 —
    /// nothing wakes implicitly); Clear is view state and follows history
    /// alone.
    #[test]
    fn menu_gates_table() {
        use PresentedStatus::*;
        // Running, everything available.
        assert_eq!(
            menu_gates(Running, true, true, true, 100),
            MenuGates { paste: true, open_cwd: true, rerun: true, clear: true }
        );
        // Rerun needs the CAN gate and a closed record both.
        assert!(!menu_gates(Running, false, true, true, 1).rerun);
        assert!(!menu_gates(Running, true, false, true, 1).rerun);
        // Dead: Paste/Rerun dim; Open cwd + Clear work.
        assert_eq!(
            menu_gates(Dead, true, true, true, 100),
            MenuGates { paste: false, open_cwd: true, rerun: false, clear: true }
        );
        // Asleep/Sleeping: identical to dead for the input verbs.
        for p in [Asleep, Sleeping] {
            let g = menu_gates(p, true, true, true, 100);
            assert!(!g.paste && !g.rerun, "{p:?} must not accept input");
            assert!(g.clear, "view-state clear stays available");
        }
        // No scrollback ⇒ nothing to clear; no local cwd ⇒ dim Explorer.
        assert!(!menu_gates(Running, true, true, true, 0).clear);
        assert!(!menu_gates(Running, true, true, false, 1).open_cwd);
    }

    /// QOL §3.3: the local-cwd resolution table (Explorer row).
    #[test]
    fn resolve_local_cwd_table() {
        let win = ShellFamily::Pwsh;
        let live = Path::new(r"C:\proj\sub");
        let meta = Path::new(r"C:\proj");
        // Win namespace: live_cwd wins, meta.cwd is the fallback.
        assert_eq!(local_cwd_for(&win, Some(live), meta), Some(live.to_path_buf()));
        assert_eq!(local_cwd_for(&win, None, meta), Some(meta.to_path_buf()));
        // WSL: /mnt/<drive> translates back to the drive form.
        let wsl = ShellFamily::WslShell { distro: Some("Ubuntu-24.04".into()) };
        assert_eq!(
            local_cwd_for(&wsl, Some(Path::new("/mnt/c/Users/z")), meta),
            Some(PathBuf::from(r"C:\Users\z"))
        );
        assert_eq!(
            local_cwd_for(&wsl, Some(Path::new("/mnt/c")), meta),
            Some(PathBuf::from(r"C:\"))
        );
        // WSL in-distro paths become the Explorer-native UNC.
        assert_eq!(
            local_cwd_for(&wsl, Some(Path::new("/home/z")), meta),
            Some(PathBuf::from(r"\\wsl.localhost\Ubuntu-24.04\home\z"))
        );
        // Default distro has no name to build the UNC ⇒ dim (never guess).
        let wsl_default = ShellFamily::WslShell { distro: None };
        assert_eq!(local_cwd_for(&wsl_default, Some(Path::new("/home/z")), meta), None);
        // …but its /mnt paths still translate.
        assert_eq!(
            local_cwd_for(&wsl_default, Some(Path::new("/mnt/d/x")), meta),
            Some(PathBuf::from(r"D:\x"))
        );
        // Pre-first-cd WSL rows may still hold a Windows-shaped cwd.
        assert_eq!(local_cwd_for(&wsl, None, meta), Some(meta.to_path_buf()));
        // Ssh: no local directory exists.
        let ssh = ShellFamily::Ssh { host: "h".into() };
        assert_eq!(local_cwd_for(&ssh, Some(Path::new("/home/z")), meta), None);
    }

    /// T1: Prefs carries consent state (hook-host verdicts, paste_warn,
    /// "never ask again") — a serde regression here silently re-prompts or
    /// forgets. (a) A LEGACY minimal gui.json (the pre-#[serde(default)]
    /// era shape) loads with every newer field at its documented default —
    /// paste_warn defaults TRUE, everything else falsy. (b) A fully
    /// populated Prefs round-trips exactly.
    #[test]
    fn prefs_migration_and_round_trip() {
        // (a) Legacy shape: only the two original fields.
        let legacy = r#"{"font_size": 14.5, "last_cwd": "D:\\work"}"#;
        let p: Prefs = serde_json::from_str(legacy).expect("legacy prefs must load");
        assert_eq!(p.font_size, 14.5);
        assert_eq!(p.last_cwd, "D:\\work");
        assert!(p.paste_warn, "paste_warn defaults ON");
        assert!(!p.compact && !p.sidebar_collapsed && !p.copy_on_select);
        assert!(!p.ssh_drop_skip_consent);
        assert!(p.last_spawn.is_none() && p.recent_spawns.is_empty());
        assert!(p.claude_hook_hosts.is_empty() && p.claude_hook_all.is_none());
        assert!(p.codex_hook_local.is_none());
        assert!(p.codex_hook_wsl_distros.is_empty() && p.codex_hook_wsl.is_none());
        assert!(p.codex_hook_hosts.is_empty() && p.codex_hook_all.is_none());
        assert_eq!(p.scrollback_lines, 10_000, "r2-M2 pref defaults to 10k");
        // #33 Updates prefs: auto-check/auto-download/backup default ON,
        // no skip recorded — an old gui.json must load exactly like this.
        assert!(p.update_auto_check && p.update_auto_download && p.update_backup_default);
        assert!(p.update_skip_version.is_none());
        // #34: pre-updater gui.json has no last_run_version — that shape
        // must NOT mint an "Updated to…" toast (None, not Some).
        assert!(p.last_run_version.is_none());
        // v0.1.2: the WSL welcome banner defaults ON.
        assert!(p.wsl_welcome_banner);

        // (b) Every field non-default → byte-exact round trip.
        let full = Prefs {
            font_size: 16.0,
            last_cwd: "E:\\x".into(),
            compact: true,
            sidebar_collapsed: true,
            last_spawn: Some(SpawnSpec {
                kind_tag: "pwsh".into(),
                program: "pwsh.exe".into(),
                args: vec!["-NoLogo".into()],
                cwd: PathBuf::from("C:\\proj"),
            }),
            recent_spawns: vec![SpawnSpec {
                kind_tag: "wsl".into(),
                program: "wsl.exe".into(),
                args: vec![],
                cwd: PathBuf::new(),
            }],
            copy_on_select: true,
            paste_warn: false,
            ssh_drop_skip_consent: true,
            claude_hook_hosts: [("devbox".to_string(), true), ("other".to_string(), false)]
                .into_iter()
                .collect(),
            claude_hook_all: Some(false),
            codex_hook_local: Some(true),
            codex_hook_wsl_distros: [("Ubuntu".to_string(), false)].into_iter().collect(),
            codex_hook_wsl: Some(false),
            codex_hook_hosts: [("devbox".to_string(), true)].into_iter().collect(),
            codex_hook_all: Some(true),
            scrollback_lines: 5_000,
            update_auto_check: false,
            update_auto_download: false,
            update_skip_version: Some("0.2.0".into()),
            update_backup_default: false,
            last_run_version: Some("0.1.0".into()),
            wsl_welcome_banner: false,
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: Prefs = serde_json::from_str(&json).unwrap();
        assert_eq!(back, full, "prefs must round-trip losslessly");
    }

    /// v0.1.2 persisted-row heal: pre-v0.1.1 WSL rows carry the WINDOWS
    /// profile dir (the old default), which v0.1.1's empty-only heal_cwd
    /// deliberately preserved — so instant-create and Suggested rows kept
    /// spawning /mnt/c/Users/<u> forever (the field bug, self-reinforcing
    /// via note_spawn and surviving reinstalls with the kept data dir).
    /// The load-boundary heal rewrites exactly those rows to `~`.
    #[test]
    fn wsl_profile_cwd_heal() {
        let wsl = |cwd: &str| SpawnSpec {
            kind_tag: "wsl:Ubuntu-24.04".into(),
            program: "wsl.exe".into(),
            args: vec!["-d".into(), "Ubuntu-24.04".into()],
            cwd: PathBuf::from(cwd),
        };
        let ps = |cwd: &str| SpawnSpec {
            kind_tag: "powershell".into(),
            program: "powershell.exe".into(),
            args: vec![],
            cwd: PathBuf::from(cwd),
        };
        let mut p = Prefs {
            last_spawn: Some(wsl("C:\\Users\\zany")), // ← the field poison
            recent_spawns: vec![
                wsl("C:\\Users\\zany"),          // poison (MRU head)
                wsl("~"),                        // healthy v0.1.1 row
                wsl("D:/Users/bob/"),            // poison: other user/drive, fwd slashes, trailing sep
                wsl("C:\\Users\\zany\\projects"), // explicit project dir — a FEATURE, keep
                ps("C:\\Users\\zany"),           // Windows shell in the profile dir — legit
                wsl(""),                         // pre-heal empty era — spawn-time heal owns it
            ],
            ..Default::default()
        };
        assert!(heal_wsl_profile_cwds(&mut p), "poisoned rows must report a change");
        assert_eq!(p.last_spawn.as_ref().unwrap().cwd, PathBuf::from("~"));
        let cwds: Vec<String> = p
            .recent_spawns
            .iter()
            .map(|s| s.cwd.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            cwds,
            [
                "~",
                "~",
                "~",
                "C:\\Users\\zany\\projects",
                "C:\\Users\\zany",
                "",
            ]
        );
        // Idempotent: the healed shape is a fixed point (second load no-ops).
        assert!(!heal_wsl_profile_cwds(&mut p), "healed prefs must not re-report");

        // Nothing to heal ⇒ untouched and unreported.
        let mut clean = Prefs {
            last_spawn: Some(ps("C:\\Users\\zany")),
            ..Default::default()
        };
        assert!(!heal_wsl_profile_cwds(&mut clean));
        assert_eq!(clean.last_spawn.as_ref().unwrap().cwd, PathBuf::from("C:\\Users\\zany"));
    }

    /// The profile-dir shape test itself: home-dir equality is
    /// case/separator/trailing-insensitive; any `<drive>:\Users\<name>`
    /// profile ROOT matches; deeper paths and non-profile dirs never do.
    #[test]
    fn windows_profile_dir_shapes() {
        let home = Path::new("C:\\Users\\Zany");
        let is = |p: &str| is_windows_profile_dir(Path::new(p), Some(home));
        assert!(is("C:\\Users\\Zany"));
        assert!(is("c:\\users\\zany"));
        assert!(is("C:/Users/zany/"));
        assert!(is("C:\\Users\\zany\\"));
        // Profile-shaped for OTHER accounts/drives (copied gui.json poison).
        assert!(is("D:\\Users\\bob"));
        assert!(is("c:/users/someone.else"));
        // NOT profile-shaped: deeper paths, roots, POSIX homes, ~.
        assert!(!is("C:\\Users\\zany\\projects"));
        assert!(!is("C:\\Users"));
        assert!(!is("C:\\"));
        assert!(!is("~"));
        assert!(!is("/home/zany"));
        assert!(!is(""));
        // Home equality works even when home is NOT under \Users (domain
        // profiles, relocated homes).
        assert!(is_windows_profile_dir(
            Path::new("e:/profiles/zany/"),
            Some(Path::new("E:\\Profiles\\Zany"))
        ));
        assert!(!is_windows_profile_dir(Path::new("E:\\Profiles\\Zany"), None));
    }

    /// T1: a corrupt gui.json backs up as gui.json.corrupt (state.json
    /// parity) instead of being silently replaced by defaults on the next
    /// save; a missing file is just first-run defaults, no backup minted.
    #[test]
    fn prefs_corrupt_file_backs_up() {
        let dir = std::env::temp_dir().join(format!("tc-prefs-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gui.json");
        let backup = dir.join("gui.json.corrupt");

        // Missing file: defaults, no backup.
        assert_eq!(load_prefs(&path), Prefs::default());
        assert!(!backup.exists());

        // Corrupt file: defaults + the original preserved as .corrupt.
        std::fs::write(&path, b"{ definitely not json").unwrap();
        assert_eq!(load_prefs(&path), Prefs::default());
        assert!(!path.exists(), "corrupt original renamed away");
        assert_eq!(
            std::fs::read(&backup).unwrap(),
            b"{ definitely not json",
            "the evidence survives byte-exact"
        );

        // Valid file loads normally (and never touches the backup).
        std::fs::write(&path, serde_json::to_vec(&Prefs::default()).unwrap()).unwrap();
        assert_eq!(load_prefs(&path), Prefs::default());
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// HIGH-1 sidebar row cache: the builder's grouping/ordering contract
    /// (folders by `order`, members by `t.order` ALONE, dangling-folder
    /// terminals in loose) and the generation key — a rebuild against a
    /// mutated state with a bumped gen reflects the change; the cache-hit
    /// comparison (`rows.gen == state_gen`) is what sidebar_rows_current
    /// keys on.
    #[test]
    fn sidebar_rows_build_and_generation_key() {
        let f_a = Uuid::new_v4();
        let f_b = Uuid::new_v4();
        let dangling = Uuid::new_v4();
        let folder = |id, name: &str, order| crate::state::Folder {
            id,
            name: name.into(),
            collapsed: false,
            order,
            color_tag: None,
        };
        let term = |name: &str, folder, order| TerminalMeta {
            id: Uuid::new_v4(),
            name: name.into(),
            folder,
            kind: TermKind::Shell,
            program: "powershell.exe".into(),
            args: vec![],
            cwd: PathBuf::new(),
            order,
            auto_restore: false,
            launched_once: false,
            status: TermStatus::Running,
            last_cols: 80,
            last_rows: 24,
            live_cwd: None,
            inner_cli: None,
            hooked: false,
            shell_cfg: None,
            color_tag: None,
            asleep: false,
            reconnecting: false,
            nested_chain: None,
        };
        let mut state = SharedState {
            folders: vec![folder(f_b, "B", 2), folder(f_a, "A", 1)],
            terminals: vec![
                term("b2", Some(f_b), 2),
                term("a1", Some(f_a), 1),
                term("loose2", None, 2),
                term("dangling", Some(dangling), 0),
                term("a0", Some(f_a), 0),
                term("loose1", None, 1),
            ],
            ..Default::default()
        };
        let rows = build_sidebar_rows(&state, 7);
        assert_eq!(rows.gen, 7);
        let names: Vec<&str> = rows.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["A", "B"], "folders by order");
        let a: Vec<&str> = rows.groups[0].iter().map(|t| t.name.as_str()).collect();
        assert_eq!(a, ["a0", "a1"], "members by t.order alone (D6)");
        assert_eq!(rows.groups[1].len(), 1);
        let loose: Vec<&str> = rows.loose.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            loose,
            ["dangling", "loose1", "loose2"],
            "folderless AND dangling-folder terminals, by order"
        );
        // Full presentation order via iter().
        let all: Vec<&str> = rows.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(all, ["a0", "a1", "b2", "dangling", "loose1", "loose2"]);
        // Generation drift: a rename in a NEW snapshot generation rebuilds to
        // the new content; the stale cache would have kept the old name.
        state.terminals[1].name = "a1-renamed".into();
        let rows2 = build_sidebar_rows(&state, 8);
        assert_ne!(rows.gen, rows2.gen, "a new snapshot must never reuse the old key");
        assert_eq!(rows2.groups[0][1].name, "a1-renamed");
        assert_eq!(rows.groups[0][1].name, "a1", "old build is immutable");
    }

    /// HIGH-2 dashboard preview cache key: consumed bytes invalidate via
    /// feed_gen; a resize does NOT bump feed_gen (the documented gotcha), so
    /// the grid dims must ride the key; the card's max_chars budget too.
    #[test]
    fn preview_cache_key_tracks_feed_resize_and_width() {
        let mut b = TermBackend::new(GridSize::default());
        let k0 = preview_key(&b, 80);
        b.advance_live(b"hello");
        assert_ne!(preview_key(&b, 80), k0, "consumed bytes must invalidate");
        let k1 = preview_key(&b, 80);
        let gen = b.feed_gen;
        let resized = b.resize_to(Vec2::new(880.0, 480.0), Vec2::new(8.0, 16.0));
        assert!(resized.is_some(), "test resize must land");
        assert_eq!(b.feed_gen, gen, "the gotcha: resize alone never bumps feed_gen");
        assert_ne!(preview_key(&b, 80), k1, "…so the dims must ride the key");
        assert_ne!(
            preview_key(&b, 40),
            preview_key(&b, 80),
            "the card's width budget rides the key"
        );
    }

    /// QOL §7.1: the Duplicate builder. THE assert: a Claude duplicate mints
    /// a FRESH session id — never the pinned one (DO-NOT 10).
    #[test]
    fn duplicate_spec_builder() {
        let pinned = Uuid::new_v4();
        let t = TerminalMeta {
            id: Uuid::new_v4(),
            name: "claude".into(),
            folder: Some(Uuid::new_v4()),
            kind: TermKind::Claude {
                session_id: pinned,
                extra_args: vec!["--model".into(), "opus".into()],
            },
            program: "claude".into(),
            args: vec![],
            cwd: PathBuf::from(r"C:\proj"),
            order: 3,
            auto_restore: true,
            launched_once: true,
            status: TermStatus::Running,
            last_cols: 120,
            last_rows: 30,
            live_cwd: Some(PathBuf::from(r"C:\proj\deeper")),
            inner_cli: None,
            hooked: false,
            shell_cfg: Some(crate::state::ShellCfg::default()),
            color_tag: Some(3),
            asleep: false,
            reconnecting: false,
            nested_chain: None,
        };
        let nt = duplicate_spec(&t, &["claude"]);
        match &nt.kind {
            TermKind::Claude { session_id, extra_args } => {
                assert_ne!(*session_id, pinned, "NEVER copy the pinned session id");
                assert_eq!(extra_args, &vec!["--model".to_string(), "opus".to_string()]);
            }
            k => panic!("kind must stay claude, got {k:?}"),
        }
        assert_eq!(nt.name, "claude 2", "name uniquified against taken");
        assert_eq!(nt.folder, t.folder, "same folder");
        assert_eq!(nt.cwd, PathBuf::from(r"C:\proj\deeper"), "duplicate = where it is NOW");
        assert!(!nt.already_launched);
        assert_eq!(nt.shell_cfg, t.shell_cfg, "shell_cfg cloned (remote-hooks opt-out rides)");

        // Shell family: cwd falls back to meta.cwd without a live one; ssh
        // duplicates land in the remote $HOME (empty cwd, launcher parity).
        let sh = TerminalMeta {
            kind: TermKind::Shell,
            program: "powershell.exe".into(),
            live_cwd: None,
            ..t.clone()
        };
        assert_eq!(duplicate_spec(&sh, &[]).cwd, PathBuf::from(r"C:\proj"));
        let ssh = TerminalMeta {
            kind: TermKind::Shell,
            program: "ssh".into(),
            args: vec!["dev-box".into()],
            live_cwd: Some(PathBuf::from("/home/z")),
            ..t.clone()
        };
        assert_eq!(duplicate_spec(&ssh, &[]).cwd, PathBuf::new(), "ssh ⇒ remote $HOME");
    }

    /// Merged-bar truncation ordering (task #21 narrow-window rule): the
    /// name middle-ellipsizes first, the cwd hides second, and the name
    /// never drops below its readability floor while a cwd is shown.
    #[test]
    fn bar_truncation_name_ellipsizes_first_cwd_hides_second() {
        // Roomy: both render untouched.
        assert_eq!(bar_text_budget(1440.0, 970.0, 150.0, 200.0), (150.0, true));
        // Tighter: the name gives way (>= MIN_NAME_PX) while the cwd stays.
        let (w, cwd) = bar_text_budget(1000.0, 700.0, 150.0, 200.0);
        assert!(cwd, "cwd must survive while the name can still shrink");
        assert!((MIN_NAME_PX..150.0).contains(&w), "name shrinks to {w}");
        // Tighter still: the cwd hides before the name goes below its floor.
        let (w, cwd) = bar_text_budget(1000.0, 640.0, 150.0, 200.0);
        assert!(!cwd, "cwd hides once the name would fall below MIN_NAME_PX");
        assert!(w >= MIN_NAME_PX, "hiding the cwd gives the name room back");
        // Minimum window: name keeps its floor, cwd gone, no panic.
        let (w, cwd) = bar_text_budget(800.0, 330.0, 150.0, 200.0);
        assert!(!cwd && (w - MIN_NAME_PX).abs() < f32::EPSILON);
        // Degenerate span never goes negative.
        assert_eq!(bar_text_budget(800.0, 100.0, 150.0, 200.0), (0.0, false));
    }

    /// The drag region keeps >= DRAG_FRACTION of the bar at typical sizes:
    /// whatever the budget grants, the leftover span (the free middle the
    /// drag handle owns) stays above the reservation.
    #[test]
    fn bar_drag_region_reserved_at_typical_widths() {
        for (bar_w, span) in [(1200.0, 760.0), (1440.0, 970.0), (1920.0, 1400.0)] {
            let (w, cwd) = bar_text_budget(bar_w, span, 180.0, 260.0);
            let used = w + if cwd { NAME_CWD_GAP + 260.0 } else { 0.0 };
            assert!(
                span - used >= bar_w * DRAG_FRACTION,
                "drag region shrank below 40% at bar_w={bar_w}: span={span} used={used}"
            );
        }
    }

    fn rec(start_off: u64, end_off: Option<u64>, exit: Option<i64>) -> BlockRec {
        BlockRec {
            epoch: 1,
            n: 0,
            cmd: "echo x".into(),
            cwd: None,
            exit,
            started_ms: 0,
            ended_ms: end_off.map(|_| 1),
            start_off,
            end_off,
            truncated: false,
        }
    }

    /// The Re-run gate's record leg (§4.2): "no open block" IS
    /// cursor-at-prompt for hooked shells. Empty ⇒ not ready (nothing proves
    /// a prompt was ever hooked); any open record ⇒ busy; all closed ⇒ ready
    /// regardless of exit codes.
    #[test]
    fn rerun_gate_truth_table() {
        assert!(!rerun_recs_ready(&[]), "empty recs must gate re-run off");
        assert!(
            !rerun_recs_ready(&[rec(10, Some(20), Some(0)), rec(30, None, None)]),
            "an open block (running command / live TUI) must gate re-run off"
        );
        assert!(
            rerun_recs_ready(&[rec(10, Some(20), Some(0)), rec(30, Some(40), Some(3))]),
            "all-closed records mean the shell is back at a prompt"
        );
        assert!(
            rerun_recs_ready(&[rec(10, Some(20), None)]),
            "exit=None (dangling close) still counts as closed"
        );
    }

    /// task #22: the inline-rename commit rule — trimmed, empty cancels,
    /// target picks the verb.
    #[test]
    fn rename_commit_cancel_empty_table() {
        let id = Uuid::new_v4();
        // Commit: trimmed name, terminal verb.
        match rename_commit(RenameTarget::Term(id), "  build box  ") {
            Some(C2D::RenameTerminal { id: i, name }) => {
                assert_eq!(i, id);
                assert_eq!(name, "build box");
            }
            other => panic!("expected RenameTerminal, got {other:?}"),
        }
        // Folder target picks the folder verb.
        match rename_commit(RenameTarget::Folder(id), "work") {
            Some(C2D::RenameFolder { id: i, name }) => {
                assert_eq!(i, id);
                assert_eq!(name, "work");
            }
            other => panic!("expected RenameFolder, got {other:?}"),
        }
        // Empty / whitespace-only ⇒ cancel (no message).
        assert!(rename_commit(RenameTarget::Term(id), "").is_none());
        assert!(rename_commit(RenameTarget::Term(id), "   \t ").is_none());
        assert!(rename_commit(RenameTarget::Folder(id), " ").is_none());
    }

    /// task #22: swatch lookup clamps out-of-range persisted values (a
    /// future table growth read by an older build) to untagged.
    #[test]
    fn tag_color_clamps_unknown_indices() {
        assert_eq!(tag_color(None), None);
        for i in 0..TAG_COLORS.len() as u8 {
            assert_eq!(tag_color(Some(i)), Some(TAG_COLORS[i as usize].0));
        }
        assert_eq!(tag_color(Some(TAG_COLORS.len() as u8)), None);
        assert_eq!(tag_color(Some(255)), None);
    }

    /// task #22 §5.5: the drop-slot delta replicates the daemon's
    /// remove+insert semantics (final index = cur + delta) for same-group
    /// and cross-group drops, insertion and append.
    #[test]
    fn drop_reorder_delta_table() {
        let ids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
        let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);
        let group = vec![a, b, c, d];

        // Same group: painted rows include the dragged row.
        // Drag a to before c (painted idx 2) ⇒ [b,a,c,d] ⇒ delta +1.
        assert_eq!(drop_reorder_delta(&group, a, Some(2), true), 1);
        // Drag a to before a (its own slot) ⇒ no-op.
        assert_eq!(drop_reorder_delta(&group, a, Some(0), true), 0);
        assert_eq!(drop_reorder_delta(&group, a, Some(1), true), 0);
        // Drag d to the top ⇒ delta −3.
        assert_eq!(drop_reorder_delta(&group, d, Some(0), true), -3);
        // Drag b below the end (painted idx 4 = past last) ⇒ delta +2.
        assert_eq!(drop_reorder_delta(&group, b, Some(4), true), 2);
        // Append (folder-header drop) ⇒ end of group.
        assert_eq!(drop_reorder_delta(&group, a, None, true), 3);
        assert_eq!(drop_reorder_delta(&group, d, None, true), 0);

        // Cross group: painted rows EXCLUDE the dragged row; `group` here is
        // the post-move replica (dragged included, sorted by order). Say the
        // dragged terminal x sorts to position 1 of [b, x, c].
        let x = Uuid::new_v4();
        let cross = vec![b, x, c];
        // Insert at painted top (idx 0 of [b, c]) ⇒ final 0 ⇒ delta −1.
        assert_eq!(drop_reorder_delta(&cross, x, Some(0), false), -1);
        // Insert before painted c (idx 1) ⇒ final 1 == cur ⇒ no-op.
        assert_eq!(drop_reorder_delta(&cross, x, Some(1), false), 0);
        // Insert at painted end (idx 2) ⇒ final 2 ⇒ delta +1.
        assert_eq!(drop_reorder_delta(&cross, x, Some(2), false), 1);
        // Append ⇒ same as end.
        assert_eq!(drop_reorder_delta(&cross, x, None, false), 1);

        // Unknown id ⇒ 0 (defensive; the daemon would ignore it anyway).
        assert_eq!(drop_reorder_delta(&group, Uuid::new_v4(), Some(0), true), 0);

        // Bug B: folder reorder rides the SAME fn — the daemon's MoveFolder
        // has identical remove+insert+clamp semantics. Folder id lists,
        // always same_group (the painted folders include the dragged one).
        let f: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        // Drag folder 0 below folder 1 (painted idx 2) ⇒ delta +1.
        assert_eq!(drop_reorder_delta(&f, f[0], Some(2), true), 1);
        // Drag the last folder to the top ⇒ delta −2.
        assert_eq!(drop_reorder_delta(&f, f[2], Some(0), true), -2);
        // Below the end (idx past last) ⇒ clamped append.
        assert_eq!(drop_reorder_delta(&f, f[1], Some(3), true), 1);
        // Own slot (above or below itself) ⇒ no-op, nothing is sent.
        assert_eq!(drop_reorder_delta(&f, f[1], Some(1), true), 0);
        assert_eq!(drop_reorder_delta(&f, f[1], Some(2), true), 0);
    }

    /// Bug B §5.5: the pure payload-aware hit-test. Layout (x 0..200):
    /// folder A (0..30) with members a0 (33..63) and a1 (66..96, last —
    /// 20px grace), collapsed folder B (101..131), UNGROUPED header zone
    /// (143..165), loose term l0 (169..199), tail zone (202..400).
    #[test]
    fn slot_hit_table() {
        let (fa, fb) = (Uuid::new_v4(), Uuid::new_v4());
        let r = |y0: f32, y1: f32| {
            Rect::from_min_max(Pos2::new(0.0, y0), Pos2::new(200.0, y1))
        };
        let rows = vec![
            DropRow::Folder { rect: r(0.0, 30.0), id: fa, idx: 0 },
            DropRow::Term { rect: r(33.0, 63.0), folder: Some(fa), idx: 0 },
            DropRow::Term { rect: r(66.0, 96.0), folder: Some(fa), idx: 1 },
            DropRow::Folder { rect: r(101.0, 131.0), id: fb, idx: 1 },
            DropRow::LooseZone { rect: r(143.0, 165.0) },
            DropRow::Term { rect: r(169.0, 199.0), folder: None, idx: 0 },
            DropRow::LooseZone { rect: r(202.0, 400.0) },
        ];
        let term = DragPayload::Term { id: Uuid::new_v4(), from: None };
        let hit = |pl: &DragPayload, x: f32, y: f32| slot_hit(&rows, pl, Pos2::new(x, y));

        // Terminal payload — midline split on member rows.
        assert!(matches!(hit(&term, 100.0, 40.0),
            Some(SlotHit::Insert { folder: Some(f), idx: 0, .. }) if f == fa));
        assert!(matches!(hit(&term, 100.0, 55.0),
            Some(SlotHit::Insert { folder: Some(f), idx: 1, .. }) if f == fa));
        // Group-end grace: 20px below the last member still appends there.
        assert!(matches!(hit(&term, 100.0, 105.0),
            Some(SlotHit::Insert { folder: Some(f), idx: 2, .. }) if f == fa));
        // Folder rows are move-into targets over their whole height.
        assert!(matches!(hit(&term, 100.0, 15.0),
            Some(SlotHit::IntoFolder { id, .. }) if id == fa));
        assert!(matches!(hit(&term, 100.0, 125.0),
            Some(SlotHit::IntoFolder { id, .. }) if id == fb));
        // The UNGROUPED header and the empty tail append to loose.
        assert!(matches!(hit(&term, 100.0, 150.0), Some(SlotHit::LooseAppend { .. })));
        assert!(matches!(hit(&term, 100.0, 300.0), Some(SlotHit::LooseAppend { .. })));
        // Dead zone (the gap between B and the header, past ±4 slop) ⇒ None.
        assert!(hit(&term, 100.0, 137.0).is_none());
        // Horizontal slop is ±8.
        assert!(hit(&term, -9.0, 15.0).is_none());
        assert!(matches!(hit(&term, -7.0, 15.0), Some(SlotHit::IntoFolder { .. })));

        // Folder payload — folder rows split at the midline into reorder
        // slots; the below-bar sits at the group's bottom (a1 = 96).
        let fol = DragPayload::Folder { id: fb };
        assert!(matches!(hit(&fol, 100.0, 10.0),
            Some(SlotHit::FolderInsert { idx: 0, .. })));
        assert!(matches!(hit(&fol, 100.0, 25.0),
            Some(SlotHit::FolderInsert { idx: 1, y, .. }) if y == 98.0));
        // A's open member rows read as A's below-slot (one visual group).
        assert!(matches!(hit(&fol, 100.0, 45.0),
            Some(SlotHit::FolderInsert { idx: 1, y, .. }) if y == 98.0));
        assert!(matches!(hit(&fol, 100.0, 80.0),
            Some(SlotHit::FolderInsert { idx: 1, .. })));
        // Loose terminals and the loose zones are not folder targets.
        assert!(hit(&fol, 100.0, 150.0).is_none());
        assert!(hit(&fol, 100.0, 184.0).is_none());
        assert!(hit(&fol, 100.0, 300.0).is_none());
    }

    /// task #22: the CLI attention dot state table. Working <800ms for
    /// everyone; a CLI with a live episode bridges the pulse through pauses
    /// up to the quiet threshold (no gray flap); the latch itself arrives as
    /// needs_you and wins over everything but Dead; plain shells are
    /// untouched by the bridge.
    #[test]
    fn cli_attention_state_table() {
        use std::time::Duration as D;
        let ms = D::from_millis;
        // Plain shell: Working → Idle at 800ms, no bridge.
        assert_eq!(derive_activity(false, false, false, ms(100), false, false), Activity::Working);
        assert_eq!(derive_activity(false, false, false, ms(900), false, false), Activity::Idle);
        assert_eq!(derive_activity(false, false, false, ms(2000), false, true), Activity::Idle);
        // CLI mid-stream: pulses through the pause window…
        assert_eq!(derive_activity(false, false, false, ms(100), true, true), Activity::Working);
        assert_eq!(derive_activity(false, false, false, ms(1500), true, true), Activity::Working);
        assert_eq!(derive_activity(false, false, false, ms(2999), true, true), Activity::Working);
        // …and past the threshold reads Idle until the update_activity latch
        // lands (needs_you) — derive itself never invents the latch.
        assert_eq!(derive_activity(false, false, false, ms(3000), true, true), Activity::Idle);
        // The latch: amber wins over Working and Idle.
        assert_eq!(derive_activity(false, false, true, ms(100), true, false), Activity::NeedsYou);
        assert_eq!(derive_activity(false, false, true, ms(9000), true, false), Activity::NeedsYou);
        // CLI with no live episode: standard rules.
        assert_eq!(derive_activity(false, false, false, ms(1500), true, false), Activity::Idle);
        // Dead wins over everything.
        assert_eq!(derive_activity(true, false, true, ms(100), true, true), Activity::Dead);
        // SLEEP: the shelved presentation wins over Dead, the latch, and
        // any streaming state (S13 — never nag about a shelved world).
        assert_eq!(derive_activity(true, true, true, ms(100), true, true), Activity::Asleep);
        assert_eq!(derive_activity(false, true, false, ms(100), false, false), Activity::Asleep);
        // The latch condition itself (update_activity's rule).
        let quiet_enough = |q: D, stream: bool| stream && q >= CLI_ATTENTION_QUIET;
        assert!(!quiet_enough(ms(2999), true));
        assert!(quiet_enough(ms(3000), true));
        assert!(!quiet_enough(ms(10_000), false));
    }

    /// perf wave 2 F1: `activity_from_meta` (meta in hand — the sidebar/dashboard
    /// hot path) and `activity_of` (id lookup) both route through `activity_for`,
    /// so a dot derived from a rows-cache meta is byte-identical to one derived
    /// by re-scanning `state`. Asserts that equivalence across a seeded fleet
    /// spanning every dot variant plus the has-signal / no-signal split.
    #[test]
    fn activity_meta_matches_lookup() {
        use std::time::Duration;
        let mk = |kind: TermKind, status: TermStatus, asleep: bool| TerminalMeta {
            id: Uuid::new_v4(),
            name: "t".into(),
            folder: None,
            kind,
            program: "p".into(),
            args: vec![],
            cwd: PathBuf::from(r"C:\"),
            order: 0,
            auto_restore: true,
            launched_once: true,
            status,
            last_cols: 80,
            last_rows: 24,
            live_cwd: None,
            inner_cli: None,
            hooked: false,
            shell_cfg: None,
            color_tag: None,
            asleep,
            reconnecting: false,
            nested_chain: None,
        };
        let sig = |needs_you: bool, quiet_ms: u64, cli_stream: bool| {
            let mut s = ActivityState::new();
            s.needs_you = needs_you;
            s.last_output = Instant::now() - Duration::from_millis(quiet_ms);
            s.cli_stream = cli_stream;
            s
        };
        let claude = || TermKind::Claude {
            session_id: Uuid::new_v4(),
            extra_args: vec![],
        };

        // A fleet spanning every branch: working shell, idle shell, dead,
        // asleep, a streaming CLI, and a latched CLI.
        let fleet = vec![
            mk(TermKind::Shell, TermStatus::Running, false),
            mk(TermKind::Shell, TermStatus::Running, false),
            mk(TermKind::Shell, TermStatus::Dead, false),
            mk(TermKind::Shell, TermStatus::Running, true),
            mk(claude(), TermStatus::Running, false),
            mk(claude(), TermStatus::Running, false),
        ];
        let mut acts: HashMap<Uuid, ActivityState> = HashMap::new();
        acts.insert(fleet[0].id, sig(false, 100, false)); // Working
        acts.insert(fleet[1].id, sig(false, 5_000, false)); // Idle
        acts.insert(fleet[2].id, sig(false, 5_000, false)); // Dead wins
        acts.insert(fleet[3].id, sig(false, 100, false)); // Asleep wins
        acts.insert(fleet[4].id, sig(false, 100, true)); // CLI streaming → Working
        acts.insert(fleet[5].id, sig(true, 9_000, false)); // latched → NeedsYou

        // The equivalence: meta in hand == re-scan by id (what `state.terminal`
        // does), for every terminal — both with and without a signal entry.
        for t in &fleet {
            let by_id = fleet.iter().find(|x| x.id == t.id);
            assert_eq!(
                activity_for(Some(t), acts.get(&t.id)),
                activity_for(by_id, acts.get(&t.id)),
                "meta vs lookup diverged for {:?}",
                t.kind
            );
            assert_eq!(activity_for(Some(t), None), activity_for(by_id, None));
        }

        // The fleet genuinely spans the variants (not all collapsed to Idle).
        let variants: Vec<Activity> = fleet
            .iter()
            .map(|t| activity_for(Some(t), acts.get(&t.id)))
            .collect();
        for want in [
            Activity::Working,
            Activity::Idle,
            Activity::Dead,
            Activity::Asleep,
            Activity::NeedsYou,
        ] {
            assert!(variants.contains(&want), "fleet missing {want:?}");
        }

        // A stale id (meta gone) resolves via the None-meta fallback: with no
        // signal entry it is Idle, exactly as `activity_of` on an unknown id.
        let ghost = Uuid::new_v4();
        assert_eq!(
            activity_for(fleet.iter().find(|x| x.id == ghost), acts.get(&ghost)),
            Activity::Idle,
        );
    }

    /// Bug A: the attention ack state machine (`step_attention`). Amber fires
    /// once per user turn: viewing clears AND disarms persistently, so a
    /// still-painted prompt signature (loop a) and idle-REPL repaint episodes
    /// (loop b) never re-fire; only user input re-arms; Reset/wake re-arm.
    #[test]
    fn attention_ack_state_table() {
        use std::time::Duration as D;
        let ms = D::from_millis;
        let mut st = ActivityState::new();
        assert!(st.armed, "starts armed: a waiting prompt alerts once at boot");

        // 1. Armed + prompt signature ⇒ latch (baseline preserved).
        step_attention(&mut st, false, true, false, ms(0), false);
        assert!(st.needs_you, "armed prompt_sig latches");
        assert!(st.armed, "latching does not disarm (taskbar flash/pill live)");

        // 2. View ⇒ clear + disarm; the signature still painted across
        //    deselect/reselect frames never re-latches (kills loop a).
        step_attention(&mut st, false, true, false, ms(0), true);
        assert!(!st.needs_you && !st.armed, "view acks: cleared + disarmed");
        for _ in 0..50 {
            step_attention(&mut st, false, true, false, ms(0), false);
            assert!(!st.needs_you, "level-triggered prompt_sig stays acked");
        }

        // 3. Disarmed idle-repaint episode: output re-armed cli_stream, then
        //    quiet past the threshold ⇒ no latch (kills loop b).
        st.cli_stream = true;
        step_attention(&mut st, false, false, true, ms(3500), false);
        assert!(!st.needs_you, "idle repaint + quiet never re-fires while acked");

        // 4. User input re-arms (rearm_attention: armed=true, cli_stream=false);
        //    the response stream then defines the next episode, which fires
        //    exactly once at the quiet threshold.
        st.armed = true;
        st.cli_stream = false; // rearm_attention drops the stale episode…
        st.cli_stream = true; // …and the response bytes arm a fresh one
        step_attention(&mut st, false, false, true, ms(2999), false);
        assert!(!st.needs_you, "not quiet long enough yet");
        step_attention(&mut st, false, false, true, ms(3000), false);
        assert!(st.needs_you, "fires again on the next waiting transition");
        assert!(!st.cli_stream, "episode consumed on latch — one alert per turn");
        step_attention(&mut st, false, false, true, ms(5000), true); // view
        assert!(!st.needs_you && !st.armed);

        // 5. Bell while disarmed ⇒ drained upstream, no latch; armed ⇒ latch.
        step_attention(&mut st, true, false, false, ms(0), false);
        assert!(!st.needs_you, "acked bell stays quiet");
        st.armed = true;
        step_attention(&mut st, true, false, false, ms(0), false);
        assert!(st.needs_you, "armed bell latches");

        // 6. Reset/wake re-arm: the fresh session's boot prompt alerts once.
        step_attention(&mut st, false, false, false, ms(0), true); // ack
        assert!(!st.armed);
        st.armed = true; // D2C::Reset / sleep-wake site
        step_attention(&mut st, false, true, false, ms(0), false);
        assert!(st.needs_you, "fresh session may alert once");

        // 7. Pill click acks persistently (needs_you=false AND armed=false):
        //    the still-painted signature must not re-light next frame.
        st.needs_you = false;
        st.armed = false;
        step_attention(&mut st, false, true, false, ms(0), false);
        assert!(!st.needs_you, "pill ack is durable, not a one-frame clear");
    }

    /// Unread ack machine: the pure verdict table (`unread_should_mark`).
    /// No baseline adopts quietly (restore/Reset boots), identical grids and
    /// single-row chrome drift never mark, ≥2 changed rows or scrollback
    /// growth mark, resize (row-count mismatch) and scrollback clears adopt.
    #[test]
    fn unread_verdict_state_table() {
        let d = |history, rows: Vec<u64>| term_backend::GridDigest { history, rows };
        // Restore/Reset replay lands on a None baseline: adopt, never mark.
        assert!(!unread_should_mark(None, &d(0, vec![1, 2, 3])));
        // Identical settled grid (title-only OSC, cursor churn, keepalives,
        // paint-then-erase status chrome): no mark.
        assert!(!unread_should_mark(Some(&d(0, vec![1, 2, 3])), &d(0, vec![1, 2, 3])));
        // One row of drift = status chrome (updater line, timer digits,
        // progress bars, prompt refreshers): no mark.
        assert!(!unread_should_mark(Some(&d(0, vec![1, 2, 3])), &d(0, vec![9, 2, 3])));
        // Two rows of changed text = user-relevant output: mark.
        assert!(unread_should_mark(Some(&d(0, vec![1, 2, 3])), &d(0, vec![9, 8, 3])));
        // Scrollback growth = content the user never saw, even if the
        // visible rows ended up identical: mark.
        assert!(unread_should_mark(Some(&d(0, vec![1, 2, 3])), &d(5, vec![1, 2, 3])));
        // Scrollback SHRINK (clear-scrollback / idle shrink): not new output.
        assert!(!unread_should_mark(Some(&d(9, vec![1, 2, 3])), &d(3, vec![1, 2, 3])));
        // Resize reflow moves every hash without any new output: adopt.
        assert!(!unread_should_mark(Some(&d(0, vec![1, 2, 3])), &d(0, vec![9, 8, 7, 6])));
    }

    /// The Q1 root-cause bytes, replayed verbatim: idle Claude Code terminals
    /// emit status-row chrome — auto-updater "Checking for updates ·"
    /// paint/erase cycles and per-second statusline timer digit ticks
    /// (captured from live journals, rows 47/49 of a 160-col session). A
    /// whole settled chrome burst must never mark unread, ack must survive
    /// further cycles, and REAL output must still mark.
    #[test]
    fn unread_chrome_replay_state_table() {
        const CHROME_UPDATES: &[u8] = include_bytes!("testdata/chrome_updates.bin");
        const CHROME_GOAL: &[u8] = include_bytes!("testdata/chrome_goal_tick.bin");
        // Tall enough that the captured CUPs (rows 47-51 of the live
        // sessions) land on their true rows instead of clamping onto each
        // other at the grid's bottom edge.
        let size = GridSize {
            cols: 160,
            rows: 60,
            ..GridSize::default()
        };
        let mut b = TermBackend::new(size);
        // A lived-in screen: prompt + prior output, plus ONE chrome cycle so
        // the persistent statusline furniture ("⏵⏵ auto mode", the goal
        // widget) is already painted — exactly what the user's acked screen
        // shows before the next periodic burst arrives.
        b.advance_live(b"PS C:\\Users\\zany> claude\r\n\r\nWelcome back.\r\n");
        b.advance_live(CHROME_UPDATES);
        b.advance_live(CHROME_GOAL);
        let base = b.grid_digest();

        // Updater chrome burst settles ⇒ no mark.
        b.advance_live(CHROME_UPDATES);
        assert!(
            !unread_should_mark(Some(&base), &b.grid_digest()),
            "updater status paint/erase must not mark unread"
        );
        // Statusline timer digit ticks ⇒ still no mark (same status row).
        b.advance_live(CHROME_GOAL);
        assert!(
            !unread_should_mark(Some(&base), &b.grid_digest()),
            "statusline timer ticks must not mark unread"
        );

        // Select clears durably: the view-ack re-bases, and ANOTHER full
        // periodic cycle after the ack stays dark.
        let acked = b.grid_digest();
        b.advance_live(CHROME_UPDATES);
        b.advance_live(CHROME_GOAL);
        assert!(
            !unread_should_mark(Some(&acked), &b.grid_digest()),
            "ack must survive the next periodic chrome cycle"
        );

        // Real output (new text on ≥2 rows) marks.
        let before = b.grid_digest();
        b.advance_live(b"\x1b[10;1Herror: build failed\r\n\x1b[11;1H1 error emitted\r\n");
        assert!(
            unread_should_mark(Some(&before), &b.grid_digest()),
            "real multi-row output must mark unread"
        );

        // Replay-style reconstruction into a fresh backend: baseline is
        // seeded FROM the replayed grid (drain_ipc Replay arm), so the
        // reconstruction itself never marks — post-update boots are quiet.
        let mut fresh = TermBackend::new(size);
        fresh.advance(b"PS C:\\Users\\zany> claude\r\n\r\nWelcome back.\r\n");
        let seeded = fresh.grid_digest();
        assert!(!unread_should_mark(Some(&seeded), &fresh.grid_digest()));
    }

    /// pw2-L1: capture-at-deselect ≡ the continuous re-base it replaced —
    /// and explicitly NOT next-frame-edge semantics. Walks select → stream
    /// (viewed) → deselect (capture via the exact helper select_terminal
    /// calls) → stream more → settle. Bytes the user saw before clicking
    /// away must NOT mark; bytes drained AFTER the deselection MUST mark —
    /// including when the stream settles inside the very first
    /// post-deselect drain batch (the regression a next-frame edge capture
    /// would introduce: one drain of never-painted bytes baked into the
    /// baseline, dot permanently missed).
    #[test]
    fn unread_capture_at_deselect_equivalence() {
        let size = GridSize {
            cols: 80,
            rows: 24,
            ..GridSize::default()
        };
        let mut b = TermBackend::new(size);
        let mut st = ActivityState::new();

        // Selected: the user watches this stream in. The continuous
        // re-base kept the baseline at "what is on glass"; the edge capture
        // below must land on the identical digest.
        b.advance_live(b"PS C:\\> build\r\nstep 1 ok\r\nstep 2 ok\r\n");
        let continuous = b.grid_digest(); // what per-frame re-base held

        // Deselection edge: select_terminal's capture, same frame as the
        // click (no bytes drain between update_activity and the click).
        unread_rebase_on_deselect(&mut st, &b);
        let base_at_click = st.unread_base.clone().expect("captured");
        assert_eq!(
            base_at_click, continuous,
            "capture-at-deselect must equal the continuously-maintained baseline"
        );
        // Bytes seen BEFORE the click never mark.
        assert!(!unread_should_mark(Some(&base_at_click), &b.grid_digest()));

        // Settle-in-drained-batch regression case: the stream settles
        // inside the FIRST post-deselect batch (claude prints its final
        // answer chunk just as the user clicks away). Those bytes were
        // never painted for the user — the settle verdict MUST mark.
        b.advance_live(b"error: build failed\r\nnote: 2 errors emitted\r\n");
        let settled = b.grid_digest();
        assert!(
            unread_should_mark(Some(&base_at_click), &settled),
            "bytes drained after the deselection edge must mark unread"
        );
        // And explicitly NOT next-frame-edge semantics: a baseline captured
        // after that drain compares equal to the settled grid — that
        // variant would never light the dot.
        let next_frame_edge = b.grid_digest();
        assert!(
            !unread_should_mark(Some(&next_frame_edge), &settled),
            "sanity: a next-frame-edge baseline is blind to the settling batch"
        );

        // Gen-skip guard: nothing consumed since the capture ⇒ a second
        // deselection edge (reselect + immediate click-away) skips the
        // digest but keeps the same verdict surface.
        let mut st2 = ActivityState::new();
        unread_rebase_on_deselect(&mut st2, &b);
        let first = st2.unread_base.clone();
        unread_rebase_on_deselect(&mut st2, &b);
        assert_eq!(st2.unread_base, first, "no-consumption re-capture is a no-op");
    }

    /// SLEEP §7.1: the presented-status → lifecycle-menu table. Running gets
    /// Sleep (Kill rides along at the call site), Asleep gets Wake, Dead
    /// keeps Restore, and the sub-second Sleeping transient offers nothing.
    #[test]
    fn lifecycle_menu_table() {
        assert_eq!(lifecycle_menu_label(PresentedStatus::Running), Some("Sleep"));
        assert_eq!(lifecycle_menu_label(PresentedStatus::Asleep), Some("Wake"));
        assert_eq!(lifecycle_menu_label(PresentedStatus::Dead), Some("Restore"));
        assert_eq!(lifecycle_menu_label(PresentedStatus::Sleeping), None);
    }
}
