//! Velopack-backed auto-update (task #34) — the real `UpdateProvider` behind
//! the settings Updates section, plus the bottom-of-sidebar update row, the
//! collapsed-rail glyph, the anchored popover, the pre-update backup, and
//! the apply orchestration (update-plan Axes 3-7).
//!
//! Shape: one background "update-engine" thread owns the Velopack
//! `UpdateManager` (checks on boot +30s, every 6h, and on demand; silent
//! auto-download when the pref allows; the full apply pipeline). The GUI
//! reads a small `Mutex`-shared presentation state — never the paint thread
//! doing network or file IO. The daemon stays 100% update-agnostic.
//!
//! Gating (Axis 3): the engine goes inert (state stays `Unsupported`, zero
//! UI) unless this exe runs from a Velopack install; a `TC_DATA_DIR` sandbox
//! stays inert too unless `TC_UPDATE_FEED` names a test feed — the staging
//! hook. With no feed override, updates also stay inert until the public
//! releases repo is configured (RELEASES_REPO_URL below).

use super::*;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
// parking_lot (the codebase standard): no poisoning, so an engine-thread
// panic can never take the paint thread down with it (`state()` runs every
// frame).
use parking_lot::Mutex;
use std::sync::Arc;

use velopack::{sources, UpdateCheck, UpdateInfo, UpdateManager};

/// GitHub repo whose Releases feed carries the Velopack packages
/// (`GithubSource`, unauthenticated — 4 checks/day is nothing against the
/// 60 req/h/IP limit). Set to the real public repo, which ACTIVATES the
/// GitHub update path for installed builds; a value containing "TODO"
/// would disable it again (the pre-release gate, kept for reuse).
const RELEASES_REPO_URL: &str = "https://github.com/aipulsedaily/pulse";

/// Feed override for staging/tests: a local dir (or URL) containing the
/// Velopack releases feed, consumed via `AutoSource`. Load-bearing for the
/// staging proof (plan §12); acceptable exposure — an attacker who can set
/// user env vars already owns HKCU.
const FEED_ENV: &str = "TC_UPDATE_FEED";

/// Boot check is deferred so GUI boot stays instant (Axis 3).
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// After this many consecutive background download failures, stop trying
/// until the next boot or a manual "Check now" (Axis 4).
const MAX_DL_FAILURES: u32 = 3;
/// Pre-update backups kept (Axis 6); older `pre-update-*` dirs are pruned.
const BACKUP_KEEP: usize = 5;

// ─────────────────────────── engine plumbing ───────────────────────────

enum EngineCmd {
    CheckNow,
    /// Popover primary: download if not staged, back up if asked, quiesce
    /// the daemon, hand off to Update.exe (Axis 7).
    Apply { backup: bool },
}

/// Mirror of the update prefs, written by the App each logic tick (the
/// checker must honor live settings changes without a restart).
#[derive(Clone, PartialEq)]
struct PrefsView {
    auto_check: bool,
    auto_download: bool,
    skip: Option<String>,
}

impl Default for PrefsView {
    fn default() -> Self {
        Self { auto_check: true, auto_download: true, skip: None }
    }
}

struct EngineShared {
    ui: UpdateUiState,
    /// The offered/staged update — engine-internal apply input.
    pending: Option<UpdateInfo>,
    /// "What's new" target for the offered version.
    notes_url: Option<String>,
    /// One-shot messages for the toast surface: (is_error, title, detail).
    msgs: Vec<(bool, String, String)>,
    /// The checker saw a NEWER version than the recorded skip — the App
    /// clears the pref (Axis 3: any newer version clears the skip).
    clear_skip: bool,
    prefs: PrefsView,
    dl_failures: u32,
}

impl Default for EngineShared {
    fn default() -> Self {
        Self {
            ui: UpdateUiState::Unsupported,
            pending: None,
            notes_url: None,
            msgs: Vec::new(),
            clear_skip: false,
            prefs: PrefsView::default(),
            dl_failures: 0,
        }
    }
}

/// The real update backend (#34): swapped in for `StubUpdateProvider` in
/// `App::new`. All verbs are non-blocking except `restore_backup` (a rare,
/// deliberate, bounded settings action — see there).
pub(super) struct VelopackUpdateProvider {
    shared: Arc<Mutex<EngineShared>>,
    tx: Sender<EngineCmd>,
}

impl VelopackUpdateProvider {
    pub(super) fn new(ctx: egui::Context) -> Self {
        let shared = Arc::new(Mutex::new(EngineShared::default()));
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_shared = shared.clone();
        // Engine construction (UpdateManager, locator IO) happens on the
        // thread — App::new stays instant. Spawn failure leaves the stub
        // behavior (Unsupported) — never a crash path.
        let _ = std::thread::Builder::new()
            .name("update-engine".into())
            .spawn(move || engine_loop(thread_shared, rx, ctx));
        Self { shared, tx }
    }
}

impl UpdateProvider for VelopackUpdateProvider {
    fn state(&self) -> UpdateUiState {
        self.shared.lock().ui.clone()
    }

    fn check_now(&mut self) {
        let _ = self.tx.send(EngineCmd::CheckNow);
    }

    /// Settings "Restore layout…": parse-verify the backup FIRST (nothing is
    /// touched if it wouldn't restore), then quiesce the daemon and copy the
    /// small-config set back. Runs on the paint thread — bounded at the
    /// quiesce wait (~8s worst case) and only ever triggered by an explicit,
    /// armed two-click settings action. The daemon respawns from bin\ via
    /// the existing reconnect loop and reloads the restored state.json.
    fn restore_backup(&mut self, backup_dir: &Path) -> Result<(), String> {
        verify_backup_parses(backup_dir)?;
        if !crate::quiesce_daemon() {
            return Err("the daemon didn't stop — nothing was changed".into());
        }
        copy_backup_into(backup_dir, &crate::state::data_dir())
    }

    fn release_notes_url(&self) -> Option<String> {
        self.shared.lock().notes_url.clone()
    }

    fn begin_apply(&mut self, backup: bool) {
        let _ = self.tx.send(EngineCmd::Apply { backup });
    }

    fn sync_prefs(&mut self, auto_check: bool, auto_download: bool, skip: Option<&str>) {
        let view = PrefsView {
            auto_check,
            auto_download,
            skip: skip.map(str::to_string),
        };
        let mut s = self.shared.lock();
        if s.prefs == view {
            return;
        }
        s.prefs = view;
        // A skip recorded against the currently offered version silences the
        // surface immediately (the engine only re-filters on its next check).
        if let Some(skip) = s.prefs.skip.clone() {
            let offered = match &s.ui {
                UpdateUiState::Available { version } | UpdateUiState::Ready { version } => {
                    Some(version.clone())
                }
                _ => None,
            };
            if offered.as_deref() == Some(skip.as_str()) {
                s.ui = UpdateUiState::UpToDate;
                s.pending = None;
                s.notes_url = None;
            }
        }
    }

    fn take_messages(&mut self) -> Vec<(bool, String, String)> {
        std::mem::take(&mut self.shared.lock().msgs)
    }

    fn take_clear_skip(&mut self) -> bool {
        std::mem::take(&mut self.shared.lock().clear_skip)
    }
}

fn set_state(shared: &Arc<Mutex<EngineShared>>, ctx: &egui::Context, ui: UpdateUiState) {
    shared.lock().ui = ui;
    ctx.request_repaint();
}

fn push_msg(shared: &Arc<Mutex<EngineShared>>, ctx: &egui::Context, error: bool, title: &str, detail: String) {
    shared.lock().msgs.push((error, title.to_string(), detail));
    ctx.request_repaint();
}

fn build_manager(feed: Option<&str>) -> Option<UpdateManager> {
    let result = match feed {
        Some(feed) => UpdateManager::new(sources::AutoSource::new(feed), None, None),
        None => {
            if RELEASES_REPO_URL.contains("TODO") {
                log::info!("updates disabled: releases repo not configured yet");
                return None;
            }
            UpdateManager::new(sources::GithubSource::new(RELEASES_REPO_URL, None, false), None, None)
        }
    };
    match result {
        Ok(um) => Some(um),
        Err(e) => {
            // Dev/portable build: the entire update feature is inert and
            // hidden (Axis 3) — an info line, never an error.
            log::info!("updates disabled (not a Velopack install): {e}");
            None
        }
    }
}

fn engine_loop(shared: Arc<Mutex<EngineShared>>, rx: Receiver<EngineCmd>, ctx: egui::Context) {
    let feed = std::env::var(FEED_ENV).ok().filter(|s| !s.is_empty());
    // Sandbox discipline (Axis 3): a TC_DATA_DIR universe never runs the
    // real updater — unless the test-feed hook names a feed explicitly
    // (the staging proof's entry point).
    if crate::state::data_dir_overridden() && feed.is_none() {
        return;
    }
    let Some(um) = build_manager(feed.as_deref()) else {
        return; // state stays Unsupported; every verb is a quiet no-op
    };
    if feed.is_some() {
        log::info!("update engine: using feed override from {FEED_ENV}");
    }
    set_state(&shared, &ctx, UpdateUiState::Idle);

    let mut next_check = Instant::now() + FIRST_CHECK_DELAY;
    loop {
        let wait = next_check.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(EngineCmd::CheckNow) => {
                shared.lock().dl_failures = 0; // manual check re-arms downloads
                check(&um, &shared, &ctx, true);
                next_check = Instant::now() + CHECK_INTERVAL;
            }
            Ok(EngineCmd::Apply { backup }) => apply(&um, &shared, &ctx, backup),
            Err(RecvTimeoutError::Timeout) => {
                if shared.lock().prefs.auto_check {
                    check(&um, &shared, &ctx, false);
                }
                next_check = Instant::now() + CHECK_INTERVAL;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// One check cycle. Background failures are a log line only; a manual
/// "Check now" failure may say so inline via `CheckFailed` (Axis 3).
fn check(um: &UpdateManager, shared: &Arc<Mutex<EngineShared>>, ctx: &egui::Context, manual: bool) {
    let prev = shared.lock().ui.clone();
    set_state(shared, ctx, UpdateUiState::Checking);
    match um.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(info)) => {
            let version = info.TargetFullRelease.Version.clone();
            let (skip, auto_dl) = {
                let s = shared.lock();
                (s.prefs.skip.clone(), s.prefs.auto_download)
            };
            match skip_filter(&version, skip.as_deref()) {
                SkipVerdict::Suppress => {
                    let mut s = shared.lock();
                    s.ui = UpdateUiState::UpToDate;
                    s.pending = None;
                    s.notes_url = None;
                    ctx.request_repaint();
                    return;
                }
                SkipVerdict::OfferAndClear => shared.lock().clear_skip = true,
                SkipVerdict::Offer => {}
            }
            {
                let mut s = shared.lock();
                s.pending = Some(*info.clone());
                s.notes_url = release_notes_url_for(&version);
            }
            let blocked = shared.lock().dl_failures >= MAX_DL_FAILURES;
            if auto_dl && !blocked {
                download(um, shared, ctx, &info, &version);
            } else {
                set_state(shared, ctx, UpdateUiState::Available { version });
            }
        }
        Ok(_) => set_state(shared, ctx, UpdateUiState::UpToDate),
        Err(e) => {
            log::warn!("update check failed: {e}");
            set_state(shared, ctx, if manual { UpdateUiState::CheckFailed } else { prev });
        }
    }
}

/// Download + stage (delta preferred — Velopack picks). Progress rides a
/// forwarder thread into the shared state; a failure resets to Available
/// and the next 6h cycle (or a manual check) retries.
fn download(
    um: &UpdateManager,
    shared: &Arc<Mutex<EngineShared>>,
    ctx: &egui::Context,
    info: &UpdateInfo,
    version: &str,
) -> bool {
    set_state(shared, ctx, UpdateUiState::Downloading { percent: 0 });
    let (ptx, prx) = std::sync::mpsc::channel::<i16>();
    let fwd_shared = shared.clone();
    let fwd_ctx = ctx.clone();
    let forwarder = std::thread::spawn(move || {
        while let Ok(p) = prx.recv() {
            let mut s = fwd_shared.lock();
            // Only while still downloading — a late progress tick must not
            // clobber the Ready/Available verdict set by the engine.
            if matches!(s.ui, UpdateUiState::Downloading { .. }) {
                s.ui = UpdateUiState::Downloading { percent: p.clamp(0, 100) as u8 };
                fwd_ctx.request_repaint();
            }
        }
    });
    let result = um.download_updates(info, Some(ptx));
    let _ = forwarder.join();
    match result {
        Ok(()) => {
            shared.lock().dl_failures = 0;
            set_state(shared, ctx, UpdateUiState::Ready { version: version.to_string() });
            log::info!("update v{version} downloaded and staged");
            true
        }
        Err(e) => {
            let mut s = shared.lock();
            s.dl_failures += 1;
            log::warn!("update download failed ({}): {e}", s.dl_failures);
            drop(s);
            set_state(shared, ctx, UpdateUiState::Available { version: version.to_string() });
            false
        }
    }
}

/// The apply pipeline (Axis 7): [download if unstaged] → backup+verify →
/// quiesce daemon → Update.exe handoff (this process exits; Update.exe swaps
/// `current\` and relaunches the new GUI, whose bin-sync deploys the daemon).
/// Every failure resets to a live state and the daemon respawns via the
/// GUI's reconnect loop the moment `Applying` clears.
fn apply(um: &UpdateManager, shared: &Arc<Mutex<EngineShared>>, ctx: &egui::Context, backup: bool) {
    let (pending, ui) = {
        let s = shared.lock();
        (s.pending.clone(), s.ui.clone())
    };
    let Some(info) = pending else {
        push_msg(shared, ctx, true, "No update staged", String::new());
        return;
    };
    let version = info.TargetFullRelease.Version.clone();

    // Auto-download off (or a previous failure): the popover's primary is
    // "Download & restart" — stage it now, visibly.
    if !matches!(ui, UpdateUiState::Ready { .. }) && !download(um, shared, ctx, &info, &version) {
        push_msg(shared, ctx, true, "Download failed — update cancelled", "the next check will retry".into());
        return;
    }

    if backup {
        set_state(shared, ctx, UpdateUiState::Applying { stage: "Backing up\u{2026}".into() });
        let from = env!("CARGO_PKG_VERSION");
        match create_pre_update_backup(from, &version) {
            Ok(dir) => log::info!("pre-update backup at {}", dir.display()),
            Err(e) => {
                // A backup that would not restore is worse than none —
                // abort the whole update (Axis 6).
                push_msg(shared, ctx, true, "Backup failed — update cancelled", e);
                set_state(shared, ctx, UpdateUiState::Ready { version });
                return;
            }
        }
    }

    set_state(shared, ctx, UpdateUiState::Applying { stage: "Restarting\u{2026}".into() });
    // Branded transition window (#34 lifecycle UI): covers the gap between
    // this GUI exiting and the new one appearing. It watches bin\.version
    // flip to the target and closes itself; on an apply failure it times out
    // quietly (the toast below owns the failure story).
    crate::spawn_lifecycle_helper(&["--updating-ui", env!("CARGO_PKG_VERSION"), &version]);
    // Let the paint thread observe `Applying` before the daemon goes down —
    // reconnect_if_needed early-returns from that point on (the planner-found
    // race: the 2s loop would resurrect the OLD daemon between quiesce and
    // process exit).
    std::thread::sleep(Duration::from_millis(200));
    if !crate::quiesce_daemon() {
        push_msg(shared, ctx, true, "Couldn't stop the daemon — update cancelled", "sessions are untouched".into());
        set_state(shared, ctx, UpdateUiState::Ready { version });
        return;
    }
    log::info!("update: daemon quiesced; handing off to Update.exe (target v{version})");
    if let Err(e) = um.apply_updates_and_restart(&info) {
        // The daemon respawns via the reconnect loop once Applying clears.
        push_msg(shared, ctx, true, "Update failed", e.to_string());
        set_state(shared, ctx, UpdateUiState::Ready { version });
    }
    // On success this process has already exited inside the call.
}

fn release_notes_url_for(version: &str) -> Option<String> {
    if RELEASES_REPO_URL.contains("TODO") {
        return None;
    }
    Some(format!("{RELEASES_REPO_URL}/releases/tag/v{version}"))
}

// ─────────────────────────── skip filter (Axis 3) ───────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SkipVerdict {
    Offer,
    /// The offered version is exactly the recorded skip.
    Suppress,
    /// A different (newer — offers are always newer than current) version
    /// appeared: offer it AND clear the stale skip pref.
    OfferAndClear,
}

pub(super) fn skip_filter(offered: &str, skip: Option<&str>) -> SkipVerdict {
    match skip {
        None => SkipVerdict::Offer,
        Some(s) if s == offered => SkipVerdict::Suppress,
        Some(_) => SkipVerdict::OfferAndClear,
    }
}

// ─────────────────────────── backups (Axis 6) ───────────────────────────

/// Local wall-clock stamp for backup dir names — matches the
/// `yyyyMMdd-HHmmss` shape `settings::parse_backup_name` displays.
fn local_timestamp() -> String {
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

fn create_pre_update_backup(from: &str, to: &str) -> Result<PathBuf, String> {
    let dir = create_backup_in(
        &crate::state::data_dir(),
        &backups_dir(),
        from,
        to,
        &local_timestamp(),
    )?;
    let _ = prune_pre_update_backups(&backups_dir(), BACKUP_KEEP);
    Ok(dir)
}

/// Copy the SMALL-CONFIG set (state.json + gui.json + probes\*.json — NEVER
/// daemon.json/daemon.lock, logs, journals) into a fresh
/// `pre-update-v<from>-to-v<to>-<ts>` dir, verify every copy byte-for-byte,
/// serde-parse state.json/gui.json, and write a manifest. ANY failure
/// removes the partial dir and returns Err — the caller aborts the update.
/// Optional serde gate a copied file must pass before the backup counts.
type ParseCheck = Option<fn(&[u8]) -> Result<(), String>>;

pub(super) fn create_backup_in(
    data_dir: &Path,
    backups_dir: &Path,
    from: &str,
    to: &str,
    ts: &str,
) -> Result<PathBuf, String> {
    let dir = backups_dir.join(format!("pre-update-v{from}-to-v{to}-{ts}"));
    let run = || -> Result<Vec<(String, u64)>, String> {
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let mut files: Vec<(String, u64)> = Vec::new();
        let mut copy_verified = |src: &Path, dst: &Path, name: &str, parse: ParseCheck| -> Result<(), String> {
            let bytes = std::fs::read(src).map_err(|e| format!("read {name}: {e}"))?;
            std::fs::write(dst, &bytes).map_err(|e| format!("write {name}: {e}"))?;
            let back = std::fs::read(dst).map_err(|e| format!("verify-read {name}: {e}"))?;
            if back != bytes {
                return Err(format!("verify {name}: copied bytes differ"));
            }
            if let Some(parse) = parse {
                parse(&back).map_err(|e| format!("verify-parse {name}: {e}"))?;
            }
            files.push((name.to_string(), back.len() as u64));
            Ok(())
        };
        let parse_state = |b: &[u8]| -> Result<(), String> {
            serde_json::from_slice::<crate::state::SharedState>(b)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        let parse_prefs = |b: &[u8]| -> Result<(), String> {
            serde_json::from_slice::<Prefs>(b).map(|_| ()).map_err(|e| e.to_string())
        };
        let state_src = data_dir.join("state.json");
        if state_src.exists() {
            copy_verified(&state_src, &dir.join("state.json"), "state.json", Some(parse_state))?;
        }
        let gui_src = data_dir.join("gui.json");
        if gui_src.exists() {
            copy_verified(&gui_src, &dir.join("gui.json"), "gui.json", Some(parse_prefs))?;
        }
        let probes_src = data_dir.join("probes");
        if let Ok(rd) = std::fs::read_dir(&probes_src) {
            let probes_dst = dir.join("probes");
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "json") && p.is_file() {
                    if !probes_dst.exists() {
                        std::fs::create_dir_all(&probes_dst)
                            .map_err(|e| format!("create probes dir: {e}"))?;
                    }
                    let fname = e.file_name().to_string_lossy().into_owned();
                    let rel = format!("probes/{fname}");
                    copy_verified(&p, &probes_dst.join(&fname), &rel, None)?;
                }
            }
        }
        Ok(files)
    };
    match run() {
        Ok(files) => {
            let manifest = serde_json::json!({
                "from": from,
                "to": to,
                "files": files
                    .iter()
                    .map(|(n, b)| serde_json::json!({ "name": n, "bytes": b }))
                    .collect::<Vec<_>>(),
            });
            let pretty = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
            std::fs::write(dir.join("manifest.json"), pretty)
                .map_err(|e| format!("write manifest: {e}"))?;
            Ok(dir)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir); // never leave a lying partial backup
            Err(e)
        }
    }
}

/// Prune old `pre-update-*` backups beyond `keep`, newest (by the trailing
/// timestamp in the name) kept. Manual and unrecognized dirs are never
/// touched. Returns the removed dirs (test observability).
pub(super) fn prune_pre_update_backups(backups_dir: &Path, keep: usize) -> Vec<PathBuf> {
    let mut pre: Vec<(String, PathBuf)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(backups_dir) else {
        return Vec::new();
    };
    for e in rd.flatten() {
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("pre-update-v") && name.len() >= 15 {
            // `.get` (not a direct slice): a hand-made dir name with
            // multi-byte UTF-8 at the boundary is "unrecognized — never
            // touched", not an engine-thread panic.
            let Some(ts) = name.get(name.len() - 15..) else {
                continue;
            };
            pre.push((ts.to_string(), e.path()));
        }
    }
    if pre.len() <= keep {
        return Vec::new();
    }
    pre.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    let mut removed = Vec::new();
    for (_, path) in pre.into_iter().skip(keep) {
        if std::fs::remove_dir_all(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

/// Parse-verify a backup BEFORE restoring: state.json/gui.json (when
/// present) must deserialize; an empty backup is refused.
pub(super) fn verify_backup_parses(backup: &Path) -> Result<(), String> {
    let state = backup.join("state.json");
    let gui = backup.join("gui.json");
    if !state.exists() && !gui.exists() {
        return Err("backup contains no state.json or gui.json".into());
    }
    if state.exists() {
        let bytes = std::fs::read(&state).map_err(|e| format!("read state.json: {e}"))?;
        serde_json::from_slice::<crate::state::SharedState>(&bytes)
            .map_err(|e| format!("state.json won't parse: {e}"))?;
    }
    if gui.exists() {
        let bytes = std::fs::read(&gui).map_err(|e| format!("read gui.json: {e}"))?;
        serde_json::from_slice::<Prefs>(&bytes).map_err(|e| format!("gui.json won't parse: {e}"))?;
    }
    Ok(())
}

/// Copy the backed-up small-config set back into the data dir. The caller
/// has verified the backup and quiesced the daemon.
pub(super) fn copy_backup_into(backup: &Path, data_dir: &Path) -> Result<(), String> {
    for name in ["state.json", "gui.json"] {
        let src = backup.join(name);
        if src.exists() {
            std::fs::copy(&src, data_dir.join(name)).map_err(|e| format!("restore {name}: {e}"))?;
        }
    }
    let probes_src = backup.join("probes");
    if let Ok(rd) = std::fs::read_dir(&probes_src) {
        let probes_dst = data_dir.join("probes");
        std::fs::create_dir_all(&probes_dst).map_err(|e| format!("create probes dir: {e}"))?;
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                std::fs::copy(&p, probes_dst.join(e.file_name()))
                    .map_err(|e| format!("restore probe: {e}"))?;
            }
        }
    }
    Ok(())
}

// ─────────────────────────── sidebar surface (Axis 5) ───────────────────────────

/// What the bottom-of-sidebar row shows: `(label, interactive)`. None = the
/// row is absent entirely (zero pixels while idle). The pill appears once an
/// update is READY (auto-download on), or already at AVAILABLE when
/// auto-download is off (the popover primary then downloads first); the
/// mid-flight states show quiet stage text.
pub(super) fn update_row_label(state: &UpdateUiState, auto_download: bool) -> Option<(String, bool)> {
    match state {
        UpdateUiState::Ready { version } => {
            Some((format!("Update ready \u{00b7} v{version}"), true))
        }
        UpdateUiState::Available { version } if !auto_download => {
            Some((format!("Update available \u{00b7} v{version}"), true))
        }
        UpdateUiState::Downloading { percent } if !auto_download => {
            Some((format!("Downloading\u{2026} {percent}%"), false))
        }
        UpdateUiState::Applying { stage } => Some((stage.clone(), false)),
        _ => None,
    }
}

/// Runtime state of the update popover (never persisted).
pub(super) struct UpdatePopover {
    /// The launching row/glyph rect — the popover anchors above it and the
    /// rect is exempt from click-away (press/release toggle pattern).
    anchor: Rect,
    /// Seeded from `update_backup_default` at open.
    backup: bool,
    /// Painted popover rect last frame (click-away hit test).
    rect: Option<Rect>,
}

impl App {
    /// Per-logic-tick update plumbing (#34): prefs mirror into the engine,
    /// skip-clear writeback, engine toasts, the post-update daemon health
    /// check, and popover/settings mutual exclusion (G9).
    pub(super) fn pump_updates(&mut self) {
        self.updates.sync_prefs(
            self.prefs.update_auto_check,
            self.prefs.update_auto_download,
            self.prefs.update_skip_version.as_deref(),
        );
        if self.updates.take_clear_skip() && self.prefs.update_skip_version.is_some() {
            self.prefs.update_skip_version = None;
            self.save_prefs();
        }
        for (error, title, detail) in self.updates.take_messages() {
            self.toasts.push(toast::Toast {
                kind: if error { toast::ToastKind::Error } else { toast::ToastKind::Info },
                title,
                detail: if detail.is_empty() { Vec::new() } else { vec![detail] },
                ttl: Some(Duration::from_secs(8)),
                action: None,
            });
        }
        // Post-update health check (Axis 7): if the daemon still isn't back
        // 15s after an updated boot, say so ONCE and point at the backups.
        if let Some(due) = self.update_health_due {
            if self.ipc.as_ref().is_some_and(|c| c.is_connected()) {
                self.update_health_due = None;
            } else if Instant::now() >= due {
                self.update_health_due = None;
                self.toasts.push(toast::Toast {
                    kind: toast::ToastKind::Error,
                    title: "Daemon didn't come back after the update".into(),
                    detail: vec!["Your layout backup is in Settings \u{2192} Backups.".into()],
                    ttl: None,
                    action: None,
                });
            }
        }
        if self.settings.is_some() {
            self.update_popover = None;
        }
    }

    fn toggle_update_popover(&mut self, anchor: Rect) {
        if self.update_popover.is_some() {
            self.update_popover = None;
        } else {
            self.update_popover = Some(UpdatePopover {
                anchor,
                backup: self.prefs.update_backup_default,
                rect: None,
            });
        }
    }

    /// Expanded sidebar: the quiet update row directly above the footer
    /// (Axis 5a). Absent = zero pixels. Base state reads as a label; hover
    /// paints the standard hover fill revealing it as clickable. Never
    /// pulses, never animates while idle.
    pub(super) fn sidebar_update_row(&mut self, ui: &mut egui::Ui) {
        let Some((label, interactive)) =
            update_row_label(&self.updates.state(), self.prefs.update_auto_download)
        else {
            return;
        };
        let sense = if interactive { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), sense);
        let resp = if interactive {
            resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        } else {
            resp
        };
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id, interactive && resp.hovered(), 0.12);
        let painter = ui.painter();
        if t > 0.0 {
            painter.rect_filled(rect, CornerRadius::same(6), OV_HOVER.gamma_multiply(t.min(1.0)));
        }
        let color = if interactive { ACCENT } else { TEXT_MUTED };
        let glyph = Rect::from_center_size(
            Pos2::new(rect.min.x + 14.0, rect.center().y),
            Vec2::splat(11.0),
        );
        draw_icon(painter, glyph, Icon::UpdateArrow, color);
        painter.text(
            Pos2::new(rect.min.x + 24.0, rect.center().y),
            Align2::LEFT_CENTER,
            &label,
            FontId::proportional(12.0),
            if interactive { lerp_col(color, ACCENT_HOVER, t) } else { color },
        );
        if interactive {
            // Keep the popover anchored to the row's live position.
            if let Some(p) = &mut self.update_popover {
                p.anchor = rect;
            }
            if resp.clicked() {
                self.toggle_update_popover(rect);
            }
        }
    }

    /// Collapsed rail: a single accent up-arrow glyph in the rail footer,
    /// tooltip carrying the row copy, click opens the same popover (Axis 5).
    pub(super) fn rail_update_glyph(&mut self, ui: &mut egui::Ui) {
        let Some((label, interactive)) =
            update_row_label(&self.updates.state(), self.prefs.update_auto_download)
        else {
            return;
        };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), if interactive { Sense::click() } else { Sense::hover() });
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id, interactive && resp.hovered(), 0.12);
        let painter = ui.painter();
        let glyph = Rect::from_center_size(rect.center(), Vec2::splat(12.0));
        let color = if interactive {
            lerp_col(ACCENT, ACCENT_HOVER, t)
        } else {
            TEXT_MUTED
        };
        draw_icon(painter, glyph, Icon::UpdateArrow, color);
        let resp = resp.on_hover_text(label);
        if interactive {
            if let Some(p) = &mut self.update_popover {
                p.anchor = rect;
            }
            if resp.clicked() {
                self.toggle_update_popover(rect);
            }
            if resp.hovered() {
                resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            }
        }
    }

    /// The anchored popover (Axis 5): NOT a modal — no backdrop, no focus
    /// steal; Esc and click-away close it. Two choices + one checkbox.
    pub(super) fn update_popover_ui(&mut self, ctx: &egui::Context) {
        let Some(popover) = &self.update_popover else {
            return;
        };
        // Only meaningful against a clickable state; mid-apply the row
        // itself carries the stage text.
        let state = self.updates.state();
        let version = match &state {
            UpdateUiState::Available { version } | UpdateUiState::Ready { version } => {
                version.clone()
            }
            _ => {
                self.update_popover = None;
                return;
            }
        };
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.update_popover = None;
            return;
        }
        let anchor = popover.anchor;
        let mut backup = popover.backup;
        let staged = matches!(state, UpdateUiState::Ready { .. });
        let notes = self.updates.release_notes_url();

        const W: f32 = 290.0;
        let est_h = 128.0;
        let content = ctx.content_rect();
        let pos = Pos2::new(
            anchor.min.x.clamp(content.min.x + 8.0, (content.max.x - W - 8.0).max(content.min.x + 8.0)),
            (anchor.min.y - est_h - 8.0).max(content.min.y + 8.0),
        );

        let mut apply_clicked = false;
        let mut later_clicked = false;
        let area = egui::Area::new(egui::Id::new("update-popover"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos);
        let aresp = area.show(ctx, |ui| {
            egui::Frame::new()
                .fill(SURFACE)
                .corner_radius(CornerRadius::same(10))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 6],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(140),
                })
                .inner_margin(Margin::same(14))
                .show(ui, |ui| {
                    ui.set_width(W - 28.0);
                    ui.label(
                        RichText::new(format!("Update to v{version}?"))
                            .font(semibold(13.0))
                            .color(TEXT),
                    );
                    ui.add_space(3.0);
                    // Honest copy (Axis 5 overrule of spec §3): the daemon
                    // restart DOES kill live foreground processes.
                    ui.label(
                        RichText::new(
                            "Terminals restore with their history; running commands restart.",
                        )
                        .size(11.0)
                        .color(TEXT_MUTED),
                    );
                    ui.add_space(8.0);
                    // Backup checkbox — a quiet painter checkbox, seeded from
                    // update_backup_default at open time.
                    let (crect, cresp) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 20.0),
                        Sense::click(),
                    );
                    let cresp = cresp.on_hover_cursor(egui::CursorIcon::PointingHand);
                    if cresp.clicked() {
                        backup = !backup;
                    }
                    let box_rect = Rect::from_center_size(
                        Pos2::new(crect.min.x + 8.0, crect.center().y),
                        Vec2::splat(15.0),
                    );
                    let cp = ui.painter();
                    if backup {
                        cp.rect_filled(box_rect, CornerRadius::same(4), ACCENT);
                        let c = box_rect.center();
                        let s = Stroke::new(1.8, ON_ACCENT);
                        cp.line_segment([c + Vec2::new(-3.5, 0.0), c + Vec2::new(-1.0, 2.6)], s);
                        cp.line_segment([c + Vec2::new(-1.0, 2.6), c + Vec2::new(3.6, -2.8)], s);
                    } else {
                        cp.rect_filled(box_rect, CornerRadius::same(4), SURFACE_4);
                    }
                    cp.text(
                        Pos2::new(box_rect.max.x + 8.0, crect.center().y),
                        Align2::LEFT_CENTER,
                        "Back up current layout first",
                        FontId::proportional(12.0),
                        TEXT_SECONDARY,
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if let Some(url) = &notes {
                            if row_ghost_button(ui, "What's new", TEXT_MUTED).clicked() {
                                let _ = open::that_detached(url);
                            }
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            // First added = rightmost: primary, then Later.
                            let primary = if staged { "Update & restart" } else { "Download & restart" };
                            let pg = ui.painter().layout_no_wrap(
                                primary.to_string(),
                                semibold(12.0),
                                ON_ACCENT,
                            );
                            let (prect, presp) = ui.allocate_exact_size(
                                Vec2::new(pg.size().x + 22.0, 26.0),
                                Sense::click(),
                            );
                            let presp = presp.on_hover_cursor(egui::CursorIcon::PointingHand);
                            let pfill = if presp.hovered() { ACCENT_HOVER } else { ACCENT };
                            ui.painter().rect_filled(prect, CornerRadius::same(7), pfill);
                            ui.painter().text(
                                prect.center(),
                                Align2::CENTER_CENTER,
                                primary,
                                semibold(12.0),
                                ON_ACCENT,
                            );
                            if presp.clicked() {
                                apply_clicked = true;
                            }
                            if row_ghost_button(ui, "Later", TEXT_SECONDARY).clicked() {
                                later_clicked = true;
                            }
                        });
                    });
                });
        });
        let painted = aresp.response.rect;
        if let Some(p) = &mut self.update_popover {
            p.backup = backup;
            p.rect = Some(painted);
        }
        if apply_clicked {
            self.updates.begin_apply(backup);
            self.update_popover = None;
            return;
        }
        if later_clicked {
            self.update_popover = None;
            return;
        }
        // Click-away (press anywhere outside the popover and its anchor).
        let pressed_outside = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !painted.contains(p) && !anchor.contains(p))
        });
        if pressed_outside {
            self.update_popover = None;
        }
    }
}

// ─────────────────────────── tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tc-update-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Axis 3: the skip-version filter table — the exact recorded version is
    /// suppressed; any OTHER offered version is offered and clears the skip.
    #[test]
    fn skip_filter_table() {
        use SkipVerdict::*;
        assert_eq!(skip_filter("0.2.0", None), Offer);
        assert_eq!(skip_filter("0.2.0", Some("0.2.0")), Suppress);
        assert_eq!(skip_filter("0.3.0", Some("0.2.0")), OfferAndClear);
        assert_eq!(skip_filter("0.2.0", Some("0.2.1")), OfferAndClear);
    }

    /// Axis 5: the sidebar row presentation table — hidden while idle,
    /// "ready" once staged, "available" only when auto-download is off,
    /// stage text (non-interactive) mid-flight.
    #[test]
    fn update_row_label_table() {
        use UpdateUiState::*;
        for auto in [true, false] {
            assert_eq!(update_row_label(&Unsupported, auto), None);
            assert_eq!(update_row_label(&Idle, auto), None);
            assert_eq!(update_row_label(&Checking, auto), None);
            assert_eq!(update_row_label(&UpToDate, auto), None);
            assert_eq!(update_row_label(&CheckFailed, auto), None);
            assert_eq!(
                update_row_label(&Ready { version: "0.2.0".into() }, auto),
                Some(("Update ready \u{00b7} v0.2.0".into(), true))
            );
            assert_eq!(
                update_row_label(&Applying { stage: "Backing up\u{2026}".into() }, auto),
                Some(("Backing up\u{2026}".into(), false))
            );
        }
        // Available/Downloading surface only on the manual-download path.
        assert_eq!(update_row_label(&Available { version: "0.2.0".into() }, true), None);
        assert_eq!(
            update_row_label(&Available { version: "0.2.0".into() }, false),
            Some(("Update available \u{00b7} v0.2.0".into(), true))
        );
        assert_eq!(update_row_label(&Downloading { percent: 43 }, true), None);
        assert_eq!(
            update_row_label(&Downloading { percent: 43 }, false),
            Some(("Downloading\u{2026} 43%".into(), false))
        );
    }

    /// Axis 6: happy-path backup — small-config set copied + verified,
    /// manifest written, probes ride along, daemon.json/journals excluded,
    /// and the name parses with the settings-list parser.
    #[test]
    fn backup_create_and_manifest() {
        let data = tdir("data");
        let backups = tdir("backups");
        let state = crate::state::SharedState::default();
        std::fs::write(data.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::write(
            data.join("gui.json"),
            serde_json::to_vec(&Prefs::default()).unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(data.join("probes")).unwrap();
        std::fs::write(data.join("probes").join("t1.json"), b"{\"x\":1}").unwrap();
        // Excluded-by-design files must not be picked up.
        std::fs::write(data.join("daemon.json"), b"{}").unwrap();

        let dir = create_backup_in(&data, &backups, "0.1.0", "0.1.1", "20260706-120000").unwrap();
        assert!(dir.join("state.json").exists());
        assert!(dir.join("gui.json").exists());
        assert!(dir.join("probes").join("t1.json").exists());
        assert!(!dir.join("daemon.json").exists(), "daemon.json is NEVER backed up");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["from"], "0.1.0");
        assert_eq!(manifest["to"], "0.1.1");
        assert_eq!(manifest["files"].as_array().unwrap().len(), 3);
        // The dir name parses in the settings Backups list.
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let (label, ts) = parse_backup_name(&name).expect("settings parser accepts the name");
        assert_eq!(label, "v0.1.0 \u{2192} v0.1.1");
        assert_eq!(ts, "2026-07-06 12:00");
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&backups);
    }

    /// Axis 6: a corrupt state.json ABORTS the backup (a backup that would
    /// not restore is worse than none) and removes the partial dir.
    #[test]
    fn backup_aborts_on_corrupt_source() {
        let data = tdir("data-bad");
        let backups = tdir("backups-bad");
        std::fs::write(data.join("state.json"), b"{ not json").unwrap();
        let err = create_backup_in(&data, &backups, "0.1.0", "0.1.1", "20260706-120000")
            .expect_err("corrupt source must abort");
        assert!(err.contains("state.json"), "error names the culprit: {err}");
        assert_eq!(
            std::fs::read_dir(&backups).unwrap().count(),
            0,
            "no partial backup dir may survive an abort"
        );
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&backups);
    }

    /// Axis 6: retention — keep the newest 5 pre-update backups, never touch
    /// manual/unrecognized dirs.
    #[test]
    fn backup_prune_keeps_newest_five() {
        let backups = tdir("prune");
        for i in 0..7 {
            std::fs::create_dir_all(
                backups.join(format!("pre-update-v0.1.{i}-to-v0.1.{}-2026070{}-120000", i + 1, i + 1)),
            )
            .unwrap();
        }
        std::fs::create_dir_all(backups.join("manual-20260101-000000")).unwrap();
        std::fs::create_dir_all(backups.join("keep-me")).unwrap();
        // Multi-byte UTF-8 straddling the trailing-15-byte boundary: must be
        // treated as unrecognized (kept), never an engine-thread panic.
        std::fs::create_dir_all(backups.join("pre-update-vx\u{e9}0260706-142530")).unwrap();
        let removed = prune_pre_update_backups(&backups, 5);
        assert_eq!(removed.len(), 2);
        // The two OLDEST timestamps went; manual + unrecognized survive.
        assert!(!backups.join("pre-update-v0.1.0-to-v0.1.1-20260701-120000").exists());
        assert!(!backups.join("pre-update-v0.1.1-to-v0.1.2-20260702-120000").exists());
        assert!(backups.join("pre-update-v0.1.6-to-v0.1.7-20260707-120000").exists());
        assert!(backups.join("manual-20260101-000000").exists());
        assert!(backups.join("keep-me").exists());
        assert!(backups.join("pre-update-vx\u{e9}0260706-142530").exists());
        let _ = std::fs::remove_dir_all(&backups);
    }

    /// Axis 6 restore: verification refuses an empty or corrupt backup;
    /// a good one copies the set back.
    #[test]
    fn restore_verify_and_copy() {
        let backup = tdir("restore-src");
        let data = tdir("restore-dst");
        assert!(verify_backup_parses(&backup).is_err(), "empty backup refused");
        std::fs::write(backup.join("state.json"), b"{ nope").unwrap();
        assert!(verify_backup_parses(&backup).is_err(), "corrupt backup refused");
        let state = crate::state::SharedState::default();
        std::fs::write(backup.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::write(
            backup.join("gui.json"),
            serde_json::to_vec(&Prefs::default()).unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(backup.join("probes")).unwrap();
        std::fs::write(backup.join("probes").join("p.json"), b"{}").unwrap();
        verify_backup_parses(&backup).expect("good backup verifies");
        copy_backup_into(&backup, &data).expect("copy back succeeds");
        assert!(data.join("state.json").exists());
        assert!(data.join("gui.json").exists());
        assert!(data.join("probes").join("p.json").exists());
        let _ = std::fs::remove_dir_all(&backup);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// The backup timestamp shape matches what parse_backup_name displays.
    #[test]
    fn local_timestamp_shape() {
        let ts = local_timestamp();
        assert_eq!(ts.len(), 15);
        assert_eq!(ts.as_bytes()[8], b'-');
        assert!(ts[..8].bytes().all(|c| c.is_ascii_digit()));
        assert!(ts[9..].bytes().all(|c| c.is_ascii_digit()));
    }
}
