//! Settings page (task #33): a centered overlay dialog (egui::Modal,
//! id "tc-settings") — pure presentation over existing `Prefs` state plus
//! the append-only Updates prefs (#34 velopack backend plugs in behind
//! `UpdateProvider`). Write-through on commit (every change saves
//! immediately); no Apply/OK/Cancel. Open-state is runtime-only.
//!
//! Sections: Appearance · Terminal · Permissions & consent · Updates · About.
//!
//! Non-goals (v1, deliberate): keybinding editor (bindings.rs is a static VT
//! table, not user config), theme picker (one designed theme is the product),
//! default-shell picker (the launcher's `last_spawn` owns it), any daemon-side
//! knob (journal caps, restore lanes — no protocol traffic from this page,
//! ever), keepalive intervals, sleep behavior, toast preferences.

use super::*;
use std::path::Path;

// ─────────────────────────── state ───────────────────────────

/// Runtime-only settings-dialog state — never persisted (inv. 7: reopening
/// the app never boots into settings).
pub(super) struct SettingsState {
    /// G8: when the reset-consents button was first clicked; armed for 3s.
    reset_armed: Option<Instant>,
    /// Backups list: which entry's Restore is armed (index + click time).
    restore_armed: Option<(usize, Instant)>,
    /// Autostart fact, read ONCE at settings-open (registry IO is not
    /// per-frame work).
    autostart: AutostartUi,
    /// backups\ dir listing, read once at settings-open. Newest first.
    backups: Vec<BackupEntry>,
}

/// How the "Starts with Windows" row presents.
pub(super) enum AutostartUi {
    /// `TC_DATA_DIR` sandbox: autostart is deliberately untouched here.
    SandboxNa,
    Status(AutostartStatus),
}

impl SettingsState {
    /// Gather the open-time facts (one registry read + one dir listing —
    /// click-time cost, never per frame).
    pub(super) fn gather() -> Self {
        let autostart = if crate::state::data_dir_overridden() {
            AutostartUi::SandboxNa
        } else {
            let value = read_autostart_run_value();
            let status = match crate::installed_exe_path() {
                Some(exe) => autostart_status(value.as_deref(), &exe),
                None => AutostartStatus::NotRegistered,
            };
            AutostartUi::Status(status)
        };
        Self {
            reset_armed: None,
            restore_armed: None,
            autostart,
            backups: list_backups(&backups_dir()),
        }
    }
}

/// How long a two-click confirm stays armed before quietly disarming.
const ARM_WINDOW: Duration = Duration::from_secs(3);

// ─────────────────────── update provider (#34 seam) ───────────────────────

/// Presentation state for the Updates section AND the sidebar update
/// row/popover (#34). Cheap to produce; read per painted frame.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum UpdateUiState {
    /// No update machinery in this build (dev/portable/stub) — the section
    /// says so quietly; never an error.
    Unsupported,
    /// Supported, nothing checked yet.
    Idle,
    Checking,
    UpToDate,
    /// A MANUAL "Check now" couldn't reach the feed (background failures
    /// stay a log line only — Axis 3).
    CheckFailed,
    Available { version: String },
    Downloading { percent: u8 },
    Ready { version: String },
    /// Apply in flight (#34 Axis 7): backup → quiesce → Update.exe handoff.
    /// While this is up, reconnect_if_needed must NOT respawn the daemon.
    Applying { stage: String },
}

/// The update backend seam (task #34, Velopack — `update.rs`). The settings
/// UI and the sidebar row/popover render purely from `state()`; the stub
/// keeps every default. Verbs never block the paint thread except
/// `restore_backup` (bounded, explicit, armed settings action).
pub(super) trait UpdateProvider {
    /// Current presentation state.
    fn state(&self) -> UpdateUiState;
    /// Manual "Check now" (settings). Must never block the paint thread.
    fn check_now(&mut self);
    /// Daemon-coordinated restore of one backup dir. Err(msg) when this
    /// build can't (stub) or the restore failed — caller toasts it quietly.
    fn restore_backup(&mut self, backup_dir: &Path) -> Result<(), String>;
    /// #34: true while an apply is in flight — gates `reconnect_if_needed`
    /// so the 2s loop can't resurrect the daemon between quiesce and exit.
    fn applying(&self) -> bool {
        matches!(self.state(), UpdateUiState::Applying { .. })
    }
    /// #34: the "What's new" link target for the offered version.
    fn release_notes_url(&self) -> Option<String> {
        None
    }
    /// #34: popover primary — download (if unstaged), optionally back up,
    /// quiesce, hand off to the updater. Runs on the engine thread.
    fn begin_apply(&mut self, _backup: bool) {}
    /// #34: mirror the live update prefs into the background checker.
    fn sync_prefs(&mut self, _auto_check: bool, _auto_download: bool, _skip: Option<&str>) {}
    /// #34: one-shot messages for the toast surface: (is_error, title, detail).
    fn take_messages(&mut self) -> Vec<(bool, String, String)> {
        Vec::new()
    }
    /// #34: the checker saw a version NEWER than a recorded skip — the
    /// caller clears `update_skip_version`.
    fn take_clear_skip(&mut self) -> bool {
        false
    }
}

/// The pre-#34 stub, kept as the trait's reference implementation and the
/// default-verb test double (production always constructs the Velopack
/// provider, which itself degrades to `Unsupported` on non-installed
/// builds — so the stub is test-only code now).
#[cfg(test)]
#[derive(Default)]
pub(super) struct StubUpdateProvider;

#[cfg(test)]
impl UpdateProvider for StubUpdateProvider {
    fn state(&self) -> UpdateUiState {
        UpdateUiState::Unsupported
    }
    fn check_now(&mut self) {}
    fn restore_backup(&mut self, _backup_dir: &Path) -> Result<(), String> {
        Err("restoring a backup needs the installed build (arrives with the updater)".into())
    }
}

/// Status line under the Updates section's version row. `(text, muted)` —
/// muted rows use TEXT_MUTED, the rest TEXT_SECONDARY. None ⇒ no line.
pub(super) fn update_status_text(state: &UpdateUiState) -> Option<(String, bool)> {
    match state {
        UpdateUiState::Unsupported => {
            Some(("Updates aren't available in this build.".into(), true))
        }
        UpdateUiState::Idle => None,
        UpdateUiState::Checking => Some(("Checking\u{2026}".into(), true)),
        UpdateUiState::UpToDate => Some(("Up to date.".into(), true)),
        UpdateUiState::CheckFailed => {
            Some(("Couldn't reach the update feed.".into(), true))
        }
        UpdateUiState::Available { version } => Some((format!("v{version} is available."), false)),
        UpdateUiState::Downloading { percent } => {
            Some((format!("Downloading\u{2026} {percent}%"), true))
        }
        UpdateUiState::Ready { version } => {
            Some((format!("v{version} is ready \u{2014} restart to update."), false))
        }
        UpdateUiState::Applying { stage } => Some((stage.clone(), true)),
    }
}

// ─────────────────────────── pure helpers ───────────────────────────

/// Tri-state consent mapping: None→0 (Ask), Some(true)→1, Some(false)→2.
pub(super) fn tri_index(v: Option<bool>) -> usize {
    match v {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    }
}

/// Inverse of `tri_index`.
pub(super) fn tri_value(i: usize) -> Option<bool> {
    match i {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// G6: the scrollback preset chips, bracketing `with_scrollback`'s
/// 200..=100_000 clamp (term_backend.rs).
pub(super) const SCROLLBACK_PRESETS: [(usize, &str); 6] = [
    (1_000, "1k"),
    (5_000, "5k"),
    (10_000, "10k"),
    (25_000, "25k"),
    (50_000, "50k"),
    (100_000, "100k"),
];

/// Index of `lines` among the presets, or None (⇒ render a literal 7th chip
/// for the hand-edited value — never silently re-mapped).
pub(super) fn preset_index(lines: usize) -> Option<usize> {
    SCROLLBACK_PRESETS.iter().position(|(v, _)| *v == lines)
}

/// The "Starts with Windows" fact (About §4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AutostartStatus {
    Registered,
    NotRegistered,
    /// The Run key exists but points at a different exe (or lost its
    /// `--daemon` argument) — e.g. an old install location.
    PointsElsewhere,
}

/// Classify the HKCU Run value against the installed exe. The writer is
/// `main.rs::install()`: `"\"{exe}\" --daemon"` (quoted path + ` --daemon`);
/// paths compare case-insensitively (Windows). Any unparseable/missing value
/// presents as NotRegistered (§9: never a scary red — the daemon may
/// legitimately be a dev build).
pub(super) fn autostart_status(run_value: Option<&str>, installed_exe: &Path) -> AutostartStatus {
    let Some(value) = run_value else {
        return AutostartStatus::NotRegistered;
    };
    let value = value.trim();
    if value.is_empty() {
        return AutostartStatus::NotRegistered;
    }
    // Extract the command path: quoted → up to the closing quote; bare → up
    // to the first space (or the whole string).
    let (path, rest) = if let Some(inner) = value.strip_prefix('"') {
        match inner.split_once('"') {
            Some((p, r)) => (p, r),
            None => (inner, ""),
        }
    } else {
        match value.split_once(' ') {
            Some((p, r)) => (p, r),
            None => (value, ""),
        }
    };
    let expected = installed_exe.to_string_lossy();
    let same_exe = path.eq_ignore_ascii_case(expected.as_ref());
    if same_exe
        && rest
            .split_whitespace()
            .any(|a| a.eq_ignore_ascii_case("--daemon"))
    {
        AutostartStatus::Registered
    } else {
        AutostartStatus::PointsElsewhere
    }
}

/// Read the autostart Run value (read-only; the value name matches what
/// `main.rs::install()` writes). Any failure ⇒ None.
fn read_autostart_run_value() -> Option<String> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .ok()?
        .get_value::<String, _>(crate::RUN_KEY_VALUE)
        .ok()
}

/// Where pre-update/manual backups live (update-plan Axis 6): inside the data
/// dir, so they survive an app uninstall.
pub(super) fn backups_dir() -> PathBuf {
    crate::state::data_dir().join("backups")
}

/// One row of the Backups list.
pub(super) struct BackupEntry {
    pub(super) dir: PathBuf,
    /// "v0.1.0 → v0.1.1", "manual", or the raw dir name when unrecognized.
    pub(super) label: String,
    /// "2026-07-06 14:25", or empty when unrecognized.
    pub(super) ts: String,
}

/// Parse a backup dir name into (label, display timestamp). Recognized
/// shapes (update-plan Axis 6): `pre-update-v<from>-to-v<to>-<yyyyMMdd-HHmmss>`
/// and `manual-<yyyyMMdd-HHmmss>`. Anything else ⇒ None (listed raw).
pub(super) fn parse_backup_name(name: &str) -> Option<(String, String)> {
    if let Some(rest) = name.strip_prefix("pre-update-v") {
        let (from, rest) = rest.split_once("-to-v")?;
        let split = rest.len().checked_sub(15)?;
        // `.get` (not `split_at`): a user-created dir name with multi-byte
        // UTF-8 at the boundary must list raw, not panic the paint thread.
        let to_dash = rest.get(..split)?;
        let ts = rest.get(split..)?;
        let to = to_dash.strip_suffix('-')?;
        if from.is_empty() || to.is_empty() {
            return None;
        }
        return Some((format!("v{from} \u{2192} v{to}"), format_backup_ts(ts)?));
    }
    if let Some(ts) = name.strip_prefix("manual-") {
        return Some(("manual".into(), format_backup_ts(ts)?));
    }
    None
}

/// "20260706-142530" → "2026-07-06 14:25" (seconds dropped for display).
fn format_backup_ts(ts: &str) -> Option<String> {
    // Validate on raw bytes only — no &str slicing until the name is proven
    // all-ASCII, so arbitrary multi-byte dir names can never hit a
    // char-boundary panic.
    let b = ts.as_bytes();
    let ok = b.len() == 15
        && b[8] == b'-'
        && b[..8].iter().all(|c| c.is_ascii_digit())
        && b[9..].iter().all(|c| c.is_ascii_digit());
    if !ok {
        return None;
    }
    Some(format!(
        "{}-{}-{} {}:{}",
        &ts[..4],
        &ts[4..6],
        &ts[6..8],
        &ts[9..11],
        &ts[11..13]
    ))
}

/// List backup dirs, newest first (recognized timestamps sort descending;
/// unrecognized dirs trail, alphabetical). Missing dir ⇒ empty (quiet state).
pub(super) fn list_backups(dir: &Path) -> Vec<BackupEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let (label, ts) = parse_backup_name(&name).unwrap_or_else(|| (name.clone(), String::new()));
        out.push(BackupEntry { dir: e.path(), label, ts });
    }
    out.sort_by(|a, b| match (a.ts.is_empty(), b.ts.is_empty()) {
        (false, false) => b.ts.cmp(&a.ts).then_with(|| a.label.cmp(&b.label)),
        (true, true) => a.label.cmp(&b.label),
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
    });
    out
}

/// Forget one recorded verdict so the consent question is asked again on the
/// next use — THIS run (inv. 5): removes the map entry AND the lane's
/// runtime dismissed/done marks. `map_key` is the map's key (host / distro);
/// `lane_key` is the runtime sets' identity (claude: the host verbatim;
/// codex: `CodexLane::key()` form). Absent entries are a no-op.
pub(super) fn forget_verdict(
    map: &mut std::collections::BTreeMap<String, bool>,
    dismissed: &mut HashSet<String>,
    done: &mut HashSet<String>,
    map_key: &str,
    lane_key: &str,
) {
    map.remove(map_key);
    dismissed.remove(lane_key);
    done.remove(lane_key);
}

/// G8: restore every consent-class pref to its default and clear all four
/// runtime dismissed/done sets, so every question asks again — this run.
/// Unrelated prefs (font, density, scrollback, spawn history, update
/// settings) are untouched.
pub(super) fn reset_all_consents(
    prefs: &mut Prefs,
    claude_dismissed: &mut HashSet<String>,
    claude_done: &mut HashSet<String>,
    codex_dismissed: &mut HashSet<String>,
    codex_done: &mut HashSet<String>,
) {
    prefs.paste_warn = true;
    prefs.ssh_drop_skip_consent = false;
    prefs.claude_hook_hosts.clear();
    prefs.claude_hook_all = None;
    prefs.codex_hook_local = None;
    prefs.codex_hook_wsl = None;
    prefs.codex_hook_wsl_distros.clear();
    prefs.codex_hook_hosts.clear();
    prefs.codex_hook_all = None;
    claude_dismissed.clear();
    claude_done.clear();
    codex_dismissed.clear();
    codex_done.clear();
}

// ─────────────────────────── widget grammar ───────────────────────────
// Doctrine-derived: zero widget strokes; hover is a fill shift; every action
// is a visible clickable control.

/// Section header: sentence case, 12px semibold TEXT_SECONDARY, spacing as
/// structure (18 above, 6 below) — never a rule.
fn section_header(ui: &mut egui::Ui, label: &str) {
    ui.add_space(18.0);
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.label(RichText::new(label).font(semibold(12.0)).color(TEXT_SECONDARY));
    });
    ui.add_space(6.0);
}

/// Full-width setting row: label (13px TEXT) + optional wrapped sub-copy
/// (11px TEXT_MUTED) left, right-aligned control (rendered by `control`,
/// which reports whether it changed). Row hover paints a soft SURFACE_4
/// fill. `control_w` reserves the control's width so the sub-copy wraps
/// around it instead of underneath it.
pub(super) fn setting_row(
    ui: &mut egui::Ui,
    label: &str,
    sub: Option<&str>,
    control_w: f32,
    control: impl FnOnce(&mut egui::Ui) -> bool,
) -> bool {
    let avail = ui.available_width();
    let text_w = (avail - control_w - 30.0).max(80.0);
    let sub_galley = sub.map(|s| {
        ui.painter()
            .layout(s.to_string(), FontId::proportional(11.0), TEXT_MUTED, text_w)
    });
    let sub_h = sub_galley.as_ref().map_or(0.0, |g| g.size().y + 3.0);
    let h = (16.0 + 18.0 + sub_h).max(36.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(avail, h), Sense::hover());
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let painter = ui.painter();
    if t > 0.0 {
        painter.rect_filled(rect, CornerRadius::same(6), SURFACE_4.gamma_multiply(t));
    }
    let text_x = rect.min.x + 10.0;
    if let Some(g) = sub_galley {
        painter.text(
            Pos2::new(text_x, rect.min.y + 8.0),
            Align2::LEFT_TOP,
            label,
            FontId::proportional(13.0),
            TEXT,
        );
        painter.galley(Pos2::new(text_x, rect.min.y + 27.0), g, TEXT_MUTED);
    } else {
        painter.text(
            Pos2::new(text_x, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(13.0),
            TEXT,
        );
    }
    let crect = Rect::from_min_max(
        Pos2::new(rect.max.x - control_w - 10.0, rect.min.y),
        Pos2::new(rect.max.x - 10.0, rect.max.y),
    );
    let mut cui = ui.new_child(
        UiBuilder::new()
            .max_rect(crect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    control(&mut cui)
}

/// 34×18 toggle pill (§5.2): track lerps SURFACE_4 → ACCENT, 14px thumb
/// slides, no strokes. Click toggles; the response reports `changed()`.
pub(super) fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(34.0, 18.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.12);
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.12);
    let off_track = lerp_col(SURFACE_4, Color32::from_rgb(0x39, 0x3F, 0x52), hov);
    let on_track = lerp_col(ACCENT, ACCENT_HOVER, hov);
    let track = lerp_col(off_track, on_track, t);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(9), track);
    let x = rect.min.x + 9.0 + (rect.width() - 18.0) * t;
    painter.circle_filled(
        Pos2::new(x, rect.center().y),
        7.0,
        lerp_col(TEXT_SECONDARY, ON_ACCENT, t),
    );
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Toggle row over `setting_row`: returns Some(new value) when clicked.
fn toggle_row(ui: &mut egui::Ui, label: &str, sub: Option<&str>, value: bool) -> Option<bool> {
    let mut on = value;
    let changed = setting_row(ui, label, sub, 44.0, |ui| toggle_switch(ui, &mut on).changed());
    changed.then_some(on)
}

/// Segmented control (§5.3): container pill SURFACE_2, selected segment
/// SURFACE_4 + TEXT, unselected TEXT_MUTED (hover TEXT_SECONDARY over a
/// faint OV_HOVER fill). Also serves the scrollback chips. Returns
/// Some(new index) on click.
pub(super) fn segmented(
    ui: &mut egui::Ui,
    id_salt: &str,
    options: &[&str],
    selected: usize,
) -> Option<usize> {
    let font = FontId::proportional(12.0);
    let widths: Vec<f32> = options
        .iter()
        .map(|o| {
            ui.painter()
                .layout_no_wrap(o.to_string(), font.clone(), TEXT)
                .size()
                .x
                + 16.0
        })
        .collect();
    let total: f32 = widths.iter().sum();
    let h = 24.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total, h), Sense::hover());
    let painter = ui.painter().clone();
    painter.rect_filled(rect, CornerRadius::same(6), SURFACE_2);
    let mut x = rect.min.x;
    let mut picked = None;
    for (i, (opt, w)) in options.iter().zip(&widths).enumerate() {
        let seg = Rect::from_min_max(Pos2::new(x, rect.min.y), Pos2::new(x + w, rect.max.y));
        let resp = ui
            .interact(seg, ui.id().with((id_salt, i)), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        let col = if i == selected {
            painter.rect_filled(seg, CornerRadius::same(6), SURFACE_4);
            TEXT
        } else if resp.hovered() {
            painter.rect_filled(seg, CornerRadius::same(6), OV_HOVER);
            TEXT_SECONDARY
        } else {
            TEXT_MUTED
        };
        painter.text(seg.center(), Align2::CENTER_CENTER, opt, font.clone(), col);
        if resp.clicked() && i != selected {
            picked = Some(i);
        }
        x += w;
    }
    picked
}

/// Per-host/lane verdict row (§5.4): indented, monospace identity, verdict
/// chip text, hover-reveal ✕ ("forget — ask again next use"). Returns true
/// when ✕ was clicked.
pub(super) fn host_row(ui: &mut egui::Ui, host: &str, installs: bool) -> bool {
    let avail = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(avail, 26.0), Sense::hover());
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let painter = ui.painter();
    if t > 0.0 {
        painter.rect_filled(rect, CornerRadius::same(5), OV_HOVER.gamma_multiply(t));
    }
    let name = middle_ellipsize(host, 36);
    let ng = painter.layout_no_wrap(name, FontId::monospace(12.0), TEXT_SECONDARY);
    let nx = rect.min.x + 22.0;
    painter.galley(
        Pos2::new(nx, rect.center().y - ng.size().y / 2.0),
        ng.clone(),
        TEXT_SECONDARY,
    );
    let (chip, chip_col) = if installs {
        ("installs", ACCENT)
    } else {
        ("never", TEXT_MUTED)
    };
    painter.text(
        Pos2::new(nx + ng.size().x + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        chip,
        FontId::proportional(11.0),
        chip_col,
    );
    // Hover-reveal ✕ (sidebar-row grammar): forget = ask again next use.
    let mut forget = false;
    if resp.hovered() {
        let xr = Rect::from_center_size(
            Pos2::new(rect.max.x - 18.0, rect.center().y),
            Vec2::splat(16.0),
        );
        let xresp = ui
            .interact(xr, resp.id.with("forget"), Sense::click())
            .on_hover_text("Forget \u{2014} ask again on next use")
            .on_hover_cursor(CursorIcon::PointingHand);
        let col = if xresp.hovered() { DANGER } else { TEXT_MUTED };
        draw_icon(
            painter,
            Rect::from_center_size(xr.center(), Vec2::splat(12.0)),
            Icon::Close,
            col,
        );
        forget = xresp.clicked();
    }
    forget
}

/// Ghost text button that arms into a danger confirm (G8: two-click with the
/// armed state VISIBLE — the relabeled button IS the state).
fn danger_ghost_button(ui: &mut egui::Ui, label: &str, armed: bool) -> egui::Response {
    let color = if armed { DANGER } else { TEXT_MUTED };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), FontId::proportional(13.0), color);
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(galley.size().x + 24.0, 30.0),
        Sense::click(),
    );
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let painter = ui.painter();
    if t > 0.0 {
        painter.rect_filled(rect, CornerRadius::same(8), danger_wash(t));
    }
    let col = if armed {
        DANGER
    } else {
        lerp_col(TEXT_MUTED, DANGER_HOVER, t)
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(13.0),
        col,
    );
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Small right-sized ghost button (row controls): label-sized, quiet hover
/// fill, 24px tall — the dialog-footer ghost shrunk to row scale.
pub(super) fn row_ghost_button(ui: &mut egui::Ui, label: &str, color: Color32) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), FontId::proportional(12.0), color);
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 18.0, 24.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let painter = ui.painter();
    if t > 0.0 {
        painter.rect_filled(rect, CornerRadius::same(6), SURFACE_4.gamma_multiply(t));
    }
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(12.0),
        lerp_col(color, TEXT, t * 0.6),
    );
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Read-only About row: label column left (TEXT_SECONDARY 12px), value
/// right-aligned via the closure (first-added = rightmost).
fn about_row(ui: &mut egui::Ui, label: &str, value: impl FnOnce(&mut egui::Ui)) {
    let avail = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(avail, 26.0), Sense::hover());
    ui.painter().text(
        Pos2::new(rect.min.x + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        TEXT_SECONDARY,
    );
    let vrect = Rect::from_min_max(
        Pos2::new(rect.min.x + 130.0, rect.min.y),
        Pos2::new(rect.max.x - 10.0, rect.max.y),
    );
    let mut vui = ui.new_child(
        UiBuilder::new()
            .max_rect(vrect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    value(&mut vui);
}

// ─────────────────────────── the page ───────────────────────────

impl App {
    /// Open the settings dialog: mutual exclusion with every other floating
    /// surface (G9); no-op while a confirm modal is up.
    pub(super) fn open_settings(&mut self) {
        if self.modal.is_some() {
            return;
        }
        self.launcher = None;
        self.history = None;
        self.blocks_panel = None;
        self.settings = Some(SettingsState::gather());
    }

    /// The settings overlay dialog (G1): centered egui::Modal over the
    /// terminal, 560px, internal scroll. Runs immediately before
    /// `show_modal` in `ui()`; `show_modal` early-returns while this is open.
    pub(super) fn show_settings(&mut self, ctx: &egui::Context) {
        let Some(mut st) = self.settings.take() else {
            // Closed: keep the fade reset so the next open animates.
            ctx.animate_bool_with_time(Id::new("tc-settings-fade"), false, 0.0);
            return;
        };
        // Disarm expired two-click confirms before painting.
        if st.reset_armed.is_some_and(|t| t.elapsed() >= ARM_WINDOW) {
            st.reset_armed = None;
        }
        if st.restore_armed.is_some_and(|(_, t)| t.elapsed() >= ARM_WINDOW) {
            st.restore_armed = None;
        }
        let mut keep = true;
        let max_h = self
            .central_rect
            .map(|r| r.height())
            .unwrap_or_else(|| ctx.content_rect().height())
            * 0.72;
        let mr = egui::Modal::new(Id::new("tc-settings")).show(ctx, |ui| {
            let t = ui
                .ctx()
                .animate_bool_with_time(Id::new("tc-settings-fade"), true, 0.12);
            ui.multiply_opacity(t);
            if t < 1.0 {
                ui.ctx().request_repaint();
            }
            ui.set_width(560.0);
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(RichText::new("Settings").font(semibold(15.0)).color(TEXT));
                // Mouse-first: a visible close control (Esc/click-outside
                // also close).
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_button(ui, Icon::Close, false)
                        .on_hover_text("Close")
                        .clicked()
                    {
                        keep = false;
                    }
                });
            });
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .max_height(max_h)
                .show(ui, |ui| {
                    self.settings_appearance(ui);
                    self.settings_terminal(ui);
                    self.settings_consent(ui, &mut st);
                    self.settings_updates(ui, &mut st);
                    self.settings_about(ui, &mut st);
                    ui.add_space(10.0);
                });
        });
        if mr.should_close() {
            keep = false;
        }
        if keep {
            // Two-click confirms disarm on their own clock — keep frames
            // coming while one is armed.
            if st.reset_armed.is_some() || st.restore_armed.is_some() {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            self.settings = Some(st);
        } else {
            ctx.animate_bool_with_time(Id::new("tc-settings-fade"), false, 0.0);
        }
    }

    fn settings_appearance(&mut self, ui: &mut egui::Ui) {
        section_header(ui, "Appearance");

        // Font size: the footer stepper's twin — same single entry point
        // (App::font_step: clamp 8–28, debounced save, perf tracking).
        let px = self.prefs.font_size as i32;
        let mut delta = 0.0f32;
        setting_row(
            ui,
            "Font size",
            Some("Ctrl + scroll wheel zooms too."),
            96.0,
            |ui| {
                if footer_glyph(ui, Icon::Plus).on_hover_text("Larger").clicked() {
                    delta = 1.0;
                }
                ui.label(
                    RichText::new(format!("{px}px"))
                        .size(12.0)
                        .color(TEXT_SECONDARY),
                );
                if footer_glyph(ui, Icon::Minus)
                    .on_hover_text("Smaller")
                    .clicked()
                {
                    delta = -1.0;
                }
                delta != 0.0
            },
        );
        if delta != 0.0 {
            self.font_step(delta);
        }

        // Sidebar row density (the footer toggle's canonical twin).
        let sel = usize::from(self.prefs.compact);
        let mut pick = None;
        setting_row(ui, "Sidebar rows", None, 170.0, |ui| {
            pick = segmented(ui, "tc-set-density", &["Comfortable", "Compact"], sel);
            pick.is_some()
        });
        if let Some(i) = pick {
            self.prefs.compact = i == 1;
            self.save_prefs();
        }
    }

    fn settings_terminal(&mut self, ui: &mut egui::Ui) {
        section_header(ui, "Terminal");

        // Scrollback: preset chips (G6). A hand-edited non-preset value
        // renders as its own selected chip — the UI never lies.
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.label(RichText::new("Scrollback").size(13.0).color(TEXT));
        });
        ui.add_space(6.0);
        let current = self.prefs.scrollback_lines;
        let mut labels: Vec<String> = SCROLLBACK_PRESETS
            .iter()
            .map(|(_, l)| (*l).to_string())
            .collect();
        let sel = match preset_index(current) {
            Some(i) => i,
            None => {
                labels.push(current.to_string());
                labels.len() - 1
            }
        };
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut pick = None;
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            pick = segmented(ui, "tc-set-scrollback", &refs, sel);
        });
        if let Some(i) = pick {
            if let Some((lines, _)) = SCROLLBACK_PRESETS.get(i) {
                self.prefs.scrollback_lines = *lines;
                self.save_prefs();
            }
        }
        ui.add_space(5.0);
        // Honesty (inv. 4): scrollback applies at backend construction only —
        // never reconstruct live terminals from here.
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.label(
                RichText::new(
                    "Applies to terminals as they reconnect or restore \u{2014} open \
                     terminals keep their current depth.",
                )
                .size(11.0)
                .color(TEXT_MUTED),
            );
        });
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.label(
                RichText::new("\u{2248} 0.4 MB per 1,000 lines per busy terminal.")
                    .size(11.0)
                    .color(TEXT_FAINT),
            );
        });
        ui.add_space(6.0);

        // Copy on select (canonical entry; the grid context-menu row stays).
        if let Some(v) = toggle_row(
            ui,
            "Copy on select",
            Some("Selecting text copies it immediately."),
            self.prefs.copy_on_select,
        ) {
            self.prefs.copy_on_select = v;
            self.save_prefs();
        }

        // The way back from ConfirmPaste's "Don't warn again".
        if let Some(v) = toggle_row(
            ui,
            "Confirm risky pastes",
            Some("Warn before multi-line pastes into shells that run each line on arrival."),
            self.prefs.paste_warn,
        ) {
            self.prefs.paste_warn = v;
            self.save_prefs();
        }
    }

    fn settings_consent(&mut self, ui: &mut egui::Ui, st: &mut SettingsState) {
        section_header(ui, "Permissions & consent");

        // Inverted row (states the protective behavior): the way back from
        // the ssh-drop dialog's "Never show this again".
        if let Some(v) = toggle_row(
            ui,
            "Confirm SSH file uploads",
            Some("Ask before copying dropped files to a remote host."),
            !self.prefs.ssh_drop_skip_consent,
        ) {
            self.prefs.ssh_drop_skip_consent = !v;
            self.save_prefs();
        }

        // Claude session tracker on SSH hosts (tri-state + per-host list).
        let sel = tri_index(self.prefs.claude_hook_all);
        let mut pick = None;
        setting_row(
            ui,
            "Claude session tracker on SSH hosts",
            Some("Installs a small hook on the remote host so Claude Code sessions restore exactly."),
            160.0,
            |ui| {
                pick = segmented(ui, "tc-set-claude-all", &["Ask", "Always", "Never"], sel);
                pick.is_some()
            },
        );
        if let Some(i) = pick {
            self.prefs.claude_hook_all = tri_value(i);
            self.save_prefs();
        }
        // Per-host verdicts (kept visible under any segment: recorded
        // verdicts keep priority over the *_all answer — documented
        // semantics, not a bug).
        let mut forget: Option<String> = None;
        for (host, yes) in &self.prefs.claude_hook_hosts {
            if host_row(ui, host, *yes) {
                forget = Some(host.clone());
            }
        }
        if let Some(host) = forget {
            forget_verdict(
                &mut self.prefs.claude_hook_hosts,
                &mut self.claude_hook_dismissed,
                &mut self.claude_hook_done,
                &host,
                &host,
            );
            // inv. 5: re-asking must work THIS run — the per-terminal
            // settled cache would otherwise skip the scan until the next
            // snapshot.
            self.claude_consent_settled.clear();
            self.save_prefs();
        }

        // Codex session tracking, local lanes (this PC & WSL). The row
        // governs both fields its label names.
        let sel = tri_index(self.prefs.codex_hook_local);
        let mut pick = None;
        setting_row(
            ui,
            "Codex session tracking (this PC & WSL)",
            Some("Records Codex session ids so sessions restore exactly across close and reboot."),
            140.0,
            |ui| {
                pick = segmented(ui, "tc-set-codex-local", &["Ask", "On", "Off"], sel);
                pick.is_some()
            },
        );
        if let Some(i) = pick {
            let v = tri_value(i);
            self.prefs.codex_hook_local = v;
            self.prefs.codex_hook_wsl = v;
            if v.is_none() {
                // Flipping to Ask: clear the local runtime lanes so the
                // question re-asks this run.
                self.codex_hook_dismissed.retain(|k| !k.starts_with("local:"));
                self.codex_hook_done.retain(|k| !k.starts_with("local:"));
                self.codex_consent_settled.clear();
            }
            self.save_prefs();
        }
        // Per-WSL-distro verdicts (inv. 5: the page is the way back from
        // every recorded answer, the per-distro ones included).
        let mut forget: Option<String> = None;
        for (distro, yes) in &self.prefs.codex_hook_wsl_distros {
            if host_row(ui, &format!("WSL {distro}"), *yes) {
                forget = Some(distro.clone());
            }
        }
        if let Some(distro) = forget {
            let lane_key = format!("local:wsl:{distro}");
            forget_verdict(
                &mut self.prefs.codex_hook_wsl_distros,
                &mut self.codex_hook_dismissed,
                &mut self.codex_hook_done,
                &distro,
                &lane_key,
            );
            self.codex_consent_settled.clear();
            self.save_prefs();
        }

        // Codex tracker on SSH hosts — identical grammar to claude's.
        let sel = tri_index(self.prefs.codex_hook_all);
        let mut pick = None;
        setting_row(
            ui,
            "Codex tracker on SSH hosts",
            Some("Installs a small hook on the remote host so Codex sessions restore exactly."),
            160.0,
            |ui| {
                pick = segmented(ui, "tc-set-codex-all", &["Ask", "Always", "Never"], sel);
                pick.is_some()
            },
        );
        if let Some(i) = pick {
            self.prefs.codex_hook_all = tri_value(i);
            self.save_prefs();
        }
        let mut forget: Option<String> = None;
        for (host, yes) in &self.prefs.codex_hook_hosts {
            if host_row(ui, host, *yes) {
                forget = Some(host.clone());
            }
        }
        if let Some(host) = forget {
            let lane_key = format!("ssh:{host}");
            forget_verdict(
                &mut self.prefs.codex_hook_hosts,
                &mut self.codex_hook_dismissed,
                &mut self.codex_hook_done,
                &host,
                &lane_key,
            );
            self.codex_consent_settled.clear();
            self.save_prefs();
        }

        // Reset all consent choices — G8 two-click inline confirm (armed
        // state is the relabeled button itself; disarms after 3s untouched).
        ui.add_space(6.0);
        let armed = st.reset_armed.is_some();
        let label = if armed {
            "Click again to reset"
        } else {
            "Reset all consent choices"
        };
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            if danger_ghost_button(ui, label, armed).clicked() {
                if armed {
                    st.reset_armed = None;
                    reset_all_consents(
                        &mut self.prefs,
                        &mut self.claude_hook_dismissed,
                        &mut self.claude_hook_done,
                        &mut self.codex_hook_dismissed,
                        &mut self.codex_hook_done,
                    );
                    self.claude_consent_settled.clear();
                    self.codex_consent_settled.clear();
                    self.save_prefs();
                } else {
                    st.reset_armed = Some(Instant::now());
                }
            }
        });
    }

    fn settings_updates(&mut self, ui: &mut egui::Ui, st: &mut SettingsState) {
        section_header(ui, "Updates");

        // Version + status + Check now. The stub provider reports
        // Unsupported — the status line says so quietly, never an error.
        let state = self.updates.state();
        let status = update_status_text(&state);
        let mut check = false;
        setting_row(
            ui,
            &format!("Pulse v{}", env!("CARGO_PKG_VERSION")),
            status.as_ref().map(|(s, _)| s.as_str()),
            96.0,
            |ui| {
                if row_ghost_button(ui, "Check now", TEXT_SECONDARY).clicked() {
                    check = true;
                }
                false
            },
        );
        if check {
            self.updates.check_now();
        }

        if let Some(v) = toggle_row(
            ui,
            "Check for updates automatically",
            Some("Quietly, on start and every few hours while running."),
            self.prefs.update_auto_check,
        ) {
            self.prefs.update_auto_check = v;
            self.save_prefs();
        }
        if let Some(v) = toggle_row(
            ui,
            "Download updates in the background",
            Some("Updates apply only when you choose to restart."),
            self.prefs.update_auto_download,
        ) {
            self.prefs.update_auto_download = v;
            self.save_prefs();
        }
        if let Some(v) = toggle_row(
            ui,
            "Back up layout before updating",
            Some("Saves terminals, layout and preferences to the backups folder first."),
            self.prefs.update_backup_default,
        ) {
            self.prefs.update_backup_default = v;
            self.save_prefs();
        }

        // Skip-this-version: shown while a skip is recorded (with the way
        // back), or while an update is available to skip. Otherwise the row
        // is absent — quiet.
        if let Some(skipped) = self.prefs.update_skip_version.clone() {
            let mut clear = false;
            setting_row(
                ui,
                &format!("Skipping v{skipped}"),
                Some("This version won't be offered; a newer one will."),
                72.0,
                |ui| {
                    clear = row_ghost_button(ui, "Clear", TEXT_SECONDARY).clicked();
                    clear
                },
            );
            if clear {
                self.prefs.update_skip_version = None;
                self.save_prefs();
            }
        } else if let UpdateUiState::Available { version } | UpdateUiState::Ready { version } =
            &state
        {
            let version = version.clone();
            let mut skip = false;
            setting_row(ui, "Skip this version", None, 110.0, |ui| {
                skip = row_ghost_button(ui, &format!("Skip v{version}"), TEXT_SECONDARY)
                    .clicked();
                skip
            });
            if skip {
                self.prefs.update_skip_version = Some(version);
                self.save_prefs();
            }
        }

        // Backups list (Axis 6): read from the real backups\ dir at open.
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.label(RichText::new("Backups").size(12.0).color(TEXT_SECONDARY));
        });
        ui.add_space(4.0);
        if st.backups.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(
                        "No backups yet \u{2014} one is saved before each update.",
                    )
                    .size(11.0)
                    .color(TEXT_MUTED),
                );
            });
        }
        let mut restore: Option<usize> = None;
        let mut arm: Option<usize> = None;
        for (i, b) in st.backups.iter().enumerate() {
            let avail = ui.available_width();
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(avail, 28.0), Sense::hover());
            let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
            let painter = ui.painter();
            if t > 0.0 {
                painter.rect_filled(rect, CornerRadius::same(6), SURFACE_4.gamma_multiply(t));
            }
            let x = rect.min.x + 10.0;
            let shown = if b.ts.is_empty() { &b.label } else { &b.ts };
            let g = painter.layout_no_wrap(shown.clone(), FontId::monospace(12.0), TEXT);
            painter.galley(Pos2::new(x, rect.center().y - g.size().y / 2.0), g.clone(), TEXT);
            if !b.ts.is_empty() {
                painter.text(
                    Pos2::new(x + g.size().x + 10.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    &b.label,
                    FontId::proportional(11.0),
                    TEXT_MUTED,
                );
            }
            let armed = st.restore_armed.is_some_and(|(ai, _)| ai == i);
            let brect = Rect::from_min_max(
                Pos2::new(rect.max.x - 170.0, rect.min.y),
                Pos2::new(rect.max.x - 6.0, rect.max.y),
            );
            let mut bui = ui.new_child(
                UiBuilder::new()
                    .max_rect(brect)
                    .layout(Layout::right_to_left(Align::Center)),
            );
            let blabel = if armed {
                "Click again to restore"
            } else {
                "Restore layout\u{2026}"
            };
            let bcol = if armed { DANGER } else { TEXT_SECONDARY };
            if row_ghost_button(&mut bui, blabel, bcol).clicked() {
                if armed {
                    restore = Some(i);
                } else {
                    arm = Some(i);
                }
            }
        }
        if let Some(i) = arm {
            st.restore_armed = Some((i, Instant::now()));
        }
        if let Some(i) = restore {
            st.restore_armed = None;
            let dir = st.backups[i].dir.clone();
            let toast = match self.updates.restore_backup(&dir) {
                Ok(()) => {
                    // #34: the restored gui.json is the truth now — reload it
                    // so the in-memory prefs (and this open dialog) match;
                    // the daemon respawns from the restored state.json via
                    // the reconnect loop.
                    self.prefs = load_prefs(&prefs_path());
                    toast::Toast {
                        kind: toast::ToastKind::Info,
                        title: "Layout restored".into(),
                        detail: vec!["terminals reload from the backup".into()],
                        ttl: Some(Duration::from_secs(6)),
                        action: None,
                    }
                }
                Err(e) => toast::Toast {
                    kind: toast::ToastKind::Info,
                    title: "Couldn't restore this backup".into(),
                    detail: vec![e],
                    ttl: Some(Duration::from_secs(6)),
                    action: None,
                },
            };
            self.toasts.push(toast);
        }
    }

    fn settings_about(&mut self, ui: &mut egui::Ui, st: &mut SettingsState) {
        section_header(ui, "About");

        about_row(ui, "Version", |ui| {
            ui.label(
                RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(12.0)
                    .color(TEXT),
            );
        });

        // Daemon status — read per frame (flips live across a restart).
        let connected = self.ipc.as_ref().is_some_and(|c| c.is_connected());
        about_row(ui, "Daemon", |ui| {
            ui.label(
                RichText::new(if connected { "connected" } else { "unreachable" })
                    .size(12.0)
                    .color(TEXT),
            );
            let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
            ui.painter()
                .circle_filled(r.center(), 4.0, if connected { SUCCESS } else { DANGER });
        });

        about_row(ui, "Starts with Windows", |ui| {
            let (text, col) = match st.autostart {
                AutostartUi::SandboxNa => ("dev sandbox \u{2014} n/a", TEXT_MUTED),
                AutostartUi::Status(AutostartStatus::Registered) => ("Registered", TEXT_MUTED),
                AutostartUi::Status(AutostartStatus::NotRegistered) => {
                    ("Not registered", ATTENTION)
                }
                AutostartUi::Status(AutostartStatus::PointsElsewhere) => {
                    ("Points at another copy", ATTENTION)
                }
            };
            ui.label(RichText::new(text).size(12.0).color(col));
        });

        let data_dir = crate::state::data_dir();
        let mut open_dir = false;
        about_row(ui, "Data folder", |ui| {
            open_dir = row_ghost_button(ui, "Open", TEXT_SECONDARY).clicked();
            ui.add_space(4.0);
            ui.label(
                RichText::new(middle_ellipsize(&data_dir.display().to_string(), 42))
                    .font(FontId::monospace(11.0))
                    .color(TEXT),
            );
        });
        if open_dir {
            self.open_support_path(&data_dir, "data folder");
        }

        let mut open_gui_log = false;
        let mut open_daemon_log = false;
        about_row(ui, "Logs", |ui| {
            open_daemon_log = row_ghost_button(ui, "Daemon log", TEXT_SECONDARY).clicked();
            ui.add_space(2.0);
            open_gui_log = row_ghost_button(ui, "GUI log", TEXT_SECONDARY).clicked();
        });
        if open_gui_log {
            self.open_support_path(&crate::state::gui_log_path(), "GUI log");
        }
        if open_daemon_log {
            self.open_support_path(&crate::state::daemon_log_path(), "daemon log");
        }
    }

    /// Open a support path in the OS handler; a missing path gets the
    /// central.rs stale-dir treatment (error toast) instead of a dead click.
    fn open_support_path(&mut self, path: &Path, what: &str) {
        let fail = if !path.exists() {
            Some(format!("No {what} yet: {}", path.display()))
        } else {
            open::that_detached(path)
                .err()
                .map(|e| format!("Could not open {}: {e}", path.display()))
        };
        if let Some(title) = fail {
            self.toasts.push(toast::Toast {
                kind: toast::ToastKind::Error,
                title,
                detail: Vec::new(),
                ttl: Some(Duration::from_secs(6)),
                action: None,
            });
        }
    }
}

// ─────────────────────────── tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// §8.1: tri-state mapping round-trips over all three states.
    #[test]
    fn tri_state_round_trip() {
        for v in [None, Some(true), Some(false)] {
            assert_eq!(tri_value(tri_index(v)), v);
        }
        assert_eq!(tri_index(None), 0);
        assert_eq!(tri_index(Some(true)), 1);
        assert_eq!(tri_index(Some(false)), 2);
        // Out-of-range indices degrade to Ask (defensive).
        assert_eq!(tri_value(7), None);
    }

    /// §8.2: preset table — each preset maps to its index; the serde default
    /// (10k) is a preset; non-presets (incl. the sub-clamp 200 hand-edit)
    /// render as their own chip.
    #[test]
    fn scrollback_preset_index_table() {
        for (i, (v, _)) in SCROLLBACK_PRESETS.iter().enumerate() {
            assert_eq!(preset_index(*v), Some(i));
        }
        assert_eq!(preset_index(10_000), Some(2), "serde default is a preset");
        assert_eq!(preset_index(100_000), Some(5), "clamp ceiling is a preset");
        assert_eq!(preset_index(12_345), None);
        assert_eq!(preset_index(200), None, "clamp floor renders literal");
        assert_eq!(preset_index(0), None);
    }

    /// §8.3: autostart classification against main.rs::install()'s exact
    /// write format (`"\"{exe}\" --daemon"`, quoted path + ` --daemon`).
    #[test]
    fn autostart_status_table() {
        let exe = Path::new(r"C:\Users\z\AppData\Local\Pulse\bin\pulse.exe");
        let written = format!("\"{}\" --daemon", exe.display());
        assert_eq!(autostart_status(Some(&written), exe), AutostartStatus::Registered);
        // Case variance (Windows paths are case-insensitive).
        assert_eq!(
            autostart_status(Some(&written.to_uppercase()), exe),
            AutostartStatus::Registered
        );
        // Unquoted-but-correct also registers (defensive tolerance).
        let bare = format!("{} --daemon", exe.display());
        assert_eq!(autostart_status(Some(&bare), exe), AutostartStatus::Registered);
        // Missing value ⇒ NotRegistered.
        assert_eq!(autostart_status(None, exe), AutostartStatus::NotRegistered);
        assert_eq!(autostart_status(Some(""), exe), AutostartStatus::NotRegistered);
        // Different path ⇒ PointsElsewhere.
        assert_eq!(
            autostart_status(Some("\"D:\\old\\pulse.exe\" --daemon"), exe),
            AutostartStatus::PointsElsewhere
        );
        // Right exe, missing --daemon ⇒ not a working registration.
        assert_eq!(
            autostart_status(Some(&format!("\"{}\"", exe.display())), exe),
            AutostartStatus::PointsElsewhere
        );
    }

    /// §8.4: reset restores consent defaults + clears all four runtime sets;
    /// unrelated prefs untouched.
    #[test]
    fn reset_all_consents_post_state() {
        let mut p = Prefs {
            font_size: 17.0,
            compact: true,
            scrollback_lines: 50_000,
            copy_on_select: true,
            paste_warn: false,
            ssh_drop_skip_consent: true,
            claude_hook_all: Some(true),
            codex_hook_local: Some(false),
            codex_hook_wsl: Some(true),
            codex_hook_all: Some(false),
            update_auto_check: false,
            update_skip_version: Some("9.9.9".into()),
            ..Prefs::default()
        };
        p.claude_hook_hosts.insert("devbox".into(), true);
        p.codex_hook_wsl_distros.insert("Ubuntu".into(), false);
        p.codex_hook_hosts.insert("devbox".into(), false);
        let mut cd: HashSet<String> = ["devbox".into()].into();
        let mut cdo: HashSet<String> = ["devbox".into()].into();
        let mut xd: HashSet<String> = ["ssh:devbox".into(), "local:windows".into()].into();
        let mut xdo: HashSet<String> = ["local:wsl:Ubuntu".into()].into();
        reset_all_consents(&mut p, &mut cd, &mut cdo, &mut xd, &mut xdo);
        // Consent defaults restored.
        assert!(p.paste_warn && !p.ssh_drop_skip_consent);
        assert!(p.claude_hook_hosts.is_empty() && p.claude_hook_all.is_none());
        assert!(p.codex_hook_local.is_none() && p.codex_hook_wsl.is_none());
        assert!(p.codex_hook_wsl_distros.is_empty());
        assert!(p.codex_hook_hosts.is_empty() && p.codex_hook_all.is_none());
        // Runtime sets emptied (re-asks work this run).
        assert!(cd.is_empty() && cdo.is_empty() && xd.is_empty() && xdo.is_empty());
        // Unrelated prefs untouched.
        assert_eq!(p.font_size, 17.0);
        assert!(p.compact && p.copy_on_select);
        assert_eq!(p.scrollback_lines, 50_000);
        assert!(!p.update_auto_check, "update prefs are not consent state");
        assert_eq!(p.update_skip_version.as_deref(), Some("9.9.9"));
    }

    /// §8.5: forget-host removes the verdict + both runtime marks; an absent
    /// host is a no-op; other entries survive.
    #[test]
    fn forget_verdict_table() {
        let mut map: std::collections::BTreeMap<String, bool> =
            [("devbox".to_string(), true), ("other".to_string(), false)]
                .into_iter()
                .collect();
        let mut dismissed: HashSet<String> = ["ssh:devbox".into(), "ssh:other".into()].into();
        let mut done: HashSet<String> = ["ssh:devbox".into()].into();
        forget_verdict(&mut map, &mut dismissed, &mut done, "devbox", "ssh:devbox");
        assert!(!map.contains_key("devbox"));
        assert!(map.contains_key("other"), "other verdicts survive");
        assert!(!dismissed.contains("ssh:devbox"));
        assert!(dismissed.contains("ssh:other"));
        assert!(done.is_empty());
        // Absent host: no-op, no panic.
        forget_verdict(&mut map, &mut dismissed, &mut done, "ghost", "ssh:ghost");
        assert_eq!(map.len(), 1);
    }

    /// Backup dir-name parsing (update-plan Axis 6 layout) + list ordering.
    #[test]
    fn backup_name_parse_table() {
        assert_eq!(
            parse_backup_name("pre-update-v0.1.0-to-v0.2.0-20260706-142530"),
            Some(("v0.1.0 \u{2192} v0.2.0".into(), "2026-07-06 14:25".into()))
        );
        assert_eq!(
            parse_backup_name("manual-20261231-235959"),
            Some(("manual".into(), "2026-12-31 23:59".into()))
        );
        // Malformed shapes refuse instead of guessing.
        assert_eq!(parse_backup_name("pre-update-v0.1.0-20260706-142530"), None);
        assert_eq!(parse_backup_name("pre-update-v-to-v2-20260706-142530"), None);
        assert_eq!(parse_backup_name("manual-2026-07-06"), None);
        assert_eq!(parse_backup_name("manual-2026070a-142530"), None);
        assert_eq!(parse_backup_name("random-junk"), None);
        assert_eq!(parse_backup_name(""), None);
        // Multi-byte UTF-8 straddling the timestamp boundary must list raw
        // (None), not panic the paint thread (é spans the len-15 split).
        assert_eq!(parse_backup_name("pre-update-v1-to-vx\u{e9}0260706-142530"), None);
        assert_eq!(parse_backup_name("manual-2026070\u{e9}-142530"), None);
    }

    /// list_backups: newest first, unknown names trail, missing dir = empty.
    #[test]
    fn list_backups_orders_newest_first() {
        let dir = std::env::temp_dir().join(format!("tc-backups-test-{}", uuid::Uuid::new_v4()));
        assert!(list_backups(&dir).is_empty(), "missing dir is a quiet empty");
        std::fs::create_dir_all(dir.join("pre-update-v0.1.0-to-v0.1.1-20260101-090000")).unwrap();
        std::fs::create_dir_all(dir.join("manual-20260615-120000")).unwrap();
        std::fs::create_dir_all(dir.join("strange-dir")).unwrap();
        std::fs::write(dir.join("stray-file.txt"), b"x").unwrap(); // files ignored
        let got = list_backups(&dir);
        let labels: Vec<&str> = got.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["manual", "v0.1.0 \u{2192} v0.1.1", "strange-dir"]);
        assert_eq!(got[0].ts, "2026-06-15 12:00");
        assert!(got[2].ts.is_empty(), "unknown dirs list raw, no invented time");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Updates status line covers every provider state (and the stub is
    /// quiet, never an error).
    #[test]
    fn update_status_text_table() {
        let (s, muted) = update_status_text(&UpdateUiState::Unsupported).unwrap();
        assert!(s.contains("aren't available"), "stub copy is quiet: {s}");
        assert!(muted);
        assert_eq!(update_status_text(&UpdateUiState::Idle), None);
        assert!(update_status_text(&UpdateUiState::Checking).is_some());
        assert!(update_status_text(&UpdateUiState::UpToDate).is_some());
        let (s, _) = update_status_text(&UpdateUiState::Available { version: "0.2.0".into() })
            .unwrap();
        assert!(s.contains("v0.2.0"));
        let (s, _) =
            update_status_text(&UpdateUiState::Downloading { percent: 43 }).unwrap();
        assert!(s.contains("43%"));
        let (s, _) =
            update_status_text(&UpdateUiState::Ready { version: "0.2.0".into() }).unwrap();
        assert!(s.contains("restart"));
        // #34 additions: manual-check failure is muted honesty, never scary;
        // Applying echoes its stage text.
        let (s, muted) = update_status_text(&UpdateUiState::CheckFailed).unwrap();
        assert!(s.contains("Couldn't reach"), "manual-failure copy: {s}");
        assert!(muted);
        let (s, _) =
            update_status_text(&UpdateUiState::Applying { stage: "Backing up\u{2026}".into() })
                .unwrap();
        assert_eq!(s, "Backing up\u{2026}");
    }

    /// The stub provider: Unsupported, check is a no-op, restore refuses
    /// with a message (the UI shows it as an info toast, not an error), and
    /// every #34 default verb stays inert.
    #[test]
    fn stub_provider_behavior() {
        let mut p = StubUpdateProvider;
        assert_eq!(p.state(), UpdateUiState::Unsupported);
        p.check_now();
        assert_eq!(p.state(), UpdateUiState::Unsupported);
        assert!(p.restore_backup(Path::new(r"C:\nope")).is_err());
        assert!(!p.applying());
        assert!(p.release_notes_url().is_none());
        p.begin_apply(true);
        p.sync_prefs(true, true, Some("0.2.0"));
        assert!(p.take_messages().is_empty());
        assert!(!p.take_clear_skip());
    }
}
