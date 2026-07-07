//! Pulse — persistent terminal manager.
//!
//! One binary, several roles:
//!   pulse.exe            → GUI (spawns the daemon if needed)
//!   pulse.exe --daemon   → PTY broker daemon (owns all terminals)
//!   pulse.exe --install  → copy to a stable path + register autostart
//!   pulse.exe --probe    → end-to-end self test

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod claude_hooks;
mod codex_hooks;
mod ctl;
mod daemon;
mod gui;
mod palette;
mod probe;
mod protocol;
mod ssh_transport;
mod state;
mod strip;
mod win32_input;

use std::path::PathBuf;

/// Stable install location, independent of where the binary is first run from:
/// %LOCALAPPDATA%\Pulse\bin\pulse.exe. Autostart and daemon spawns prefer
/// this so a moved/deleted build directory can't orphan the daemon.
pub fn installed_exe_path() -> Option<PathBuf> {
    Some(state::data_dir().join("bin").join("pulse.exe"))
}

/// The HKCU Run value name that autostarts the daemon. The pre-rebrand value
/// name ("TerminalControlDaemon") is deleted by the one-time data-dir
/// migration below and by uninstall_cleanup.
pub const RUN_KEY_VALUE: &str = "Pulse";
const LEGACY_RUN_KEY_VALUE: &str = "TerminalControlDaemon";
/// Pre-rebrand data-dir name (%LOCALAPPDATA%\TerminalControl); referenced
/// only by the one-time migration.
const LEGACY_DATA_DIR_NAME: &str = "TerminalControl";

/// Set by Velopack's first-run hook (the very first launch after Setup —
/// signalled via the VELOPACK_FIRSTRUN env Update.exe/Setup set on the
/// process they launch). Consumed by the GUI to show the one-time branded
/// welcome card (#34 lifecycle UI). Never set for dev builds.
pub static FIRST_RUN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn main() -> anyhow::Result<()> {
    // Velopack hook handler (#34) — MUST be the first statement of main():
    // during install/update/uninstall, Update.exe invokes this exe with
    // `--veloapp-*` args and run() dispatches the matching hook then EXITS
    // the process. Every normal arg (--daemon / --install / --probe / ctl)
    // falls through unmolested; in a non-Velopack context (dev build,
    // bin\ daemon) run() fails to locate a manifest and returns immediately.
    // Auto-apply-on-startup is OFF: update-plan Axis 4 picked the persistent
    // "restart to update" affordance — updates apply only when the user asks.
    velopack::VelopackApp::build()
        .set_auto_apply_on_startup(false)
        .on_first_run(|_| FIRST_RUN.store(true, std::sync::atomic::Ordering::Relaxed))
        .on_before_uninstall_fast_callback(|_| uninstall_cleanup())
        .run();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--daemon") => daemon::run(),
        Some("--install") => install(),
        Some("--probe") => probe::run(args.get(2).map(String::as_str)),
        // #34 lifecycle UI: the branded uninstall / update-transition
        // windows, run from a %TEMP% copy of this exe (the install dir is
        // being deleted/swapped underneath — a process running from
        // `current\` would image-lock it against Update.exe).
        Some("--uninstall-ui") => gui::lifecycle_ui::run_uninstall(),
        Some("--updating-ui") => gui::lifecycle_ui::run_updating(
            args.get(2).cloned().unwrap_or_default(),
            args.get(3).cloned().unwrap_or_default(),
        ),
        // Controller CLI through the main exe (debug/redirected-output use;
        // `pulse-ctl.exe` — a real console binary — is the documented interface).
        Some("ctl") => std::process::exit(ctl::run(args[2..].to_vec())),
        _ => gui::run(),
    }
}

/// Ask a running daemon to exit and wait (bounded, ~5s + the 3s bounded
/// shutdown read) until the single-instance `daemon.lock` is releasable —
/// the reliable "daemon fully exited, binary replaceable" signal. Returns
/// `true` when the daemon is gone (or was never running). Shared by
/// `install()`, the updater's apply path (gui::update), backup restore, and
/// the Velopack uninstall hook. `request_shutdown` keeps its
/// read-until-close discipline — dropping the socket early RSTs the
/// Shutdown frame away (documented live incident, gui/ipc.rs).
pub fn quiesce_daemon() -> bool {
    quiesce_daemon_in(&state::data_dir())
}

/// `quiesce_daemon` against an explicit data dir. Exists for the one-time
/// legacy-dir migration, which must hand-shake the OLD daemon (whose
/// daemon.json / daemon.lock live in the old dir) while `state::data_dir()`
/// already resolves to the new location.
fn quiesce_daemon_in(dir: &std::path::Path) -> bool {
    let _ = gui::ipc::request_shutdown_at(&dir.join("daemon.json")); // may simply not be running
    let lock_path = dir.join("daemon.lock");
    for _ in 0..50 {
        if !lock_path.exists() || std::fs::remove_file(&lock_path).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// One-time rebrand migration: move the ENTIRE pre-rebrand data dir
/// (`<parent>\TerminalControl` — journals, state.json, gui.json, backups,
/// probes, ctl-tokens, logs) to the new location (`state::data_dir()`,
/// `<parent>\Pulse`), preserving every byte via a same-volume rename.
///
/// Invariants:
/// - Runs ONLY when the old dir exists, looks like ours (state.json /
///   journals\ / bin\ marker), and the new dir does not exist yet — a second
///   run is a provable no-op, and a fresh install (no old dir) never enters.
/// - The OLD daemon is quiesced first via its own daemon.json/daemon.lock
///   (the existing shutdown handshake), so the move never races live journal
///   writes; its sessions relaunch from journals under the new dir exactly
///   like a reboot restore.
/// - Old-named binaries (terminal-control.exe / tc.exe) and the bin-sync
///   `.version` sidecar are removed from the moved `bin\` (bounded retry —
///   the old daemon's exe image lock outlives the lock file by milliseconds);
///   the caller then deploys pulse.exe / pulse-ctl.exe into the same bin\.
/// - The legacy HKCU Run value is deleted (real profile only, never from a
///   TC_DATA_DIR sandbox); the caller registers the new "Pulse" value.
///
/// The old dir is derived as a SIBLING of the new one, so a TC_DATA_DIR
/// sandbox exercises the whole path (old = `<sandbox-parent>\TerminalControl`)
/// without ever touching %LOCALAPPDATA%.
///
/// Returns true when a migration happened.
pub fn migrate_legacy_data_dir() -> bool {
    let new = state::data_dir();
    let Some(parent) = new.parent().map(std::path::Path::to_path_buf) else {
        return false;
    };
    let old = parent.join(LEGACY_DATA_DIR_NAME);
    if !old.is_dir() || new.exists() {
        return false;
    }
    // Only migrate a dir that is provably ours — never relocate a stranger's
    // directory that merely shares the old name.
    let looks_like_ours = old.join("state.json").is_file()
        || old.join("journals").is_dir()
        || old.join("bin").is_dir();
    if !looks_like_ours {
        log::warn!(
            "legacy dir {} exists but has no state.json/journals/bin — not migrating it",
            old.display()
        );
        return false;
    }
    log::info!("migrating data dir {} -> {}", old.display(), new.display());
    println!("Migrating data from {} to {} ...", old.display(), new.display());
    if !quiesce_daemon_in(&old) {
        // The rename below fails on any open handle; warn-and-try matches the
        // install() copy discipline (the rename is the honest failure point).
        println!("warning: old daemon still holds daemon.lock; move may fail");
    }
    // Same-volume rename: atomic for the tree, byte-identical journals, no
    // copy window. Bounded retry — the old daemon's handles (gui.log via a
    // lingering GUI, the exe image in bin\) can outlive the lock release.
    let mut moved = false;
    for _ in 0..50 {
        match std::fs::rename(&old, &new) {
            Ok(()) => {
                moved = true;
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    if !moved {
        // Nothing was harmed: rename either fully succeeds or leaves the old
        // tree in place. Say so and let the caller proceed as a fresh
        // install; the next --install retries the migration.
        println!(
            "ERROR: could not move {} (files in use?). Close anything using it and re-run --install.",
            old.display()
        );
        log::error!("data-dir migration rename failed; old dir left untouched");
        return false;
    }
    // The moved bin\ still holds the old-named binaries and the bin-sync
    // sidecar. Drop them: the sidecar would otherwise tell a same-version
    // bin-sync "already deployed" while pulse.exe does not exist yet, and
    // the old exes are dead weight the moment the Run key flips.
    for stale in ["terminal-control.exe", "tc.exe", ".version"] {
        let p = new.join("bin").join(stale);
        if !p.exists() {
            continue;
        }
        for _ in 0..30 {
            if std::fs::remove_file(&p).is_ok() || !p.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    // Retire the legacy autostart value (real profile only — a sandbox never
    // wrote one). The caller writes the new "Pulse" value.
    if !state::data_dir_overridden() {
        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        if let Ok(key) = hkcu.open_subkey_with_flags(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            winreg::enums::KEY_SET_VALUE,
        ) {
            let _ = key.delete_value(LEGACY_RUN_KEY_VALUE);
        }
    }
    println!("Migrated. Sessions and journals moved to {}", new.display());
    log::info!("data-dir migration complete");
    true
}

/// Bin-sync (#34, update-plan Axis 0-C: "Velopack delivers, --install
/// deploys"). Velopack owns `%LOCALAPPDATA%\AIPulseDaily.Pulse\current\`;
/// the load-bearing runtime path stays `installed_exe_path()` (bin\) — the
/// GUI's daemon-spawn preference, the daemon's Run-key self-heal, and the
/// ~/.codex hook entries all point there, and the daemon never
/// runs from Velopack's dir (so an update apply never contends with the
/// daemon's image lock).
///
/// Called at the top of every GUI boot in a Velopack-installed context:
/// when the `bin\.version` sidecar differs from this build, quiesce the
/// daemon, copy pulse.exe + pulse-ctl.exe from `current\` into `bin\`,
/// and write the sidecar. First install, updates and repairs all funnel
/// through this ONE deploy path — the proven `--install` dance.
///
/// Under `TC_DATA_DIR` the whole operation is contained by construction:
/// data_dir()/bin, daemon.json and daemon.lock all resolve inside the
/// override, and no Run key is written here (the daemon's own
/// install_autostart carries the TC_DATA_DIR gate). The staging proof
/// depends on that containment.
pub fn sync_bin_install() {
    use velopack::locator::{auto_locate_app_manifest, LocationContext};
    if auto_locate_app_manifest(LocationContext::FromCurrentExe).is_err() {
        return; // not a Velopack install (dev/portable build) — bin\ is not ours to manage
    }
    // Rebrand migration first (no-op unless the legacy TerminalControl dir
    // exists and the new one doesn't): a Velopack Setup.exe run on a machine
    // with a pre-rebrand --install must inherit its sessions, not orphan
    // them. The Velopack-context gate above keeps dev/portable runs inert.
    let migrated = migrate_legacy_data_dir();
    let Some(target) = installed_exe_path() else {
        return;
    };
    let sidecar = target.with_file_name(".version");
    let deployed = std::fs::read_to_string(&sidecar).unwrap_or_default();
    if deployed.trim() == env!("CARGO_PKG_VERSION") {
        return;
    }
    log::info!(
        "bin-sync: deployed '{}' != packaged '{}' — deploying into {}",
        deployed.trim(),
        env!("CARGO_PKG_VERSION"),
        target.display()
    );
    if !quiesce_daemon() {
        // Copy below will fail on a locked file; warn-and-continue matches
        // install(). The next boot retries (sidecar was not written).
        log::warn!("bin-sync: daemon still holds daemon.lock; copy may fail");
    }
    let copy = || -> std::io::Result<()> {
        let current = std::env::current_exe()?;
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if current != target {
            std::fs::copy(&current, &target)?;
        }
        let ctl_source = current.with_file_name("pulse-ctl.exe");
        let ctl_target = target.with_file_name("pulse-ctl.exe");
        if ctl_source.exists() && ctl_source != ctl_target {
            std::fs::copy(&ctl_source, &ctl_target)?;
        } else if !ctl_source.exists() {
            log::warn!("bin-sync: no sibling pulse-ctl.exe; controller CLI not deployed");
        }
        Ok(())
    };
    match copy() {
        Ok(()) => {
            // Sidecar written LAST, only after a successful copy — a failed
            // sync must retry next boot, never lie about the deployed version.
            if let Err(e) = std::fs::write(&sidecar, env!("CARGO_PKG_VERSION")) {
                log::error!("bin-sync: sidecar write failed: {e}");
            } else {
                log::info!("bin-sync: deployed v{}", env!("CARGO_PKG_VERSION"));
            }
            if migrated {
                // A migration just retired <old>\bin\tc.exe — the user's
                // consented ~/.codex hook entry (if any) still names it.
                // Point it at the freshly deployed pulse-ctl.exe.
                repair_codex_hook_command(&target.with_file_name("pulse-ctl.exe"));
            }
        }
        Err(e) => log::error!("bin-sync: copy failed ({e}); will retry next boot"),
    }
}

/// Rebrand follow-up: the native-Windows codex hook entry the user consented
/// to (task #30) PERSISTS the controller-CLI path inside `~/.codex/hooks.json`
/// (plus a trust hash over that command in config.toml). After the data-dir
/// migration that path names the retired `...\TerminalControl\bin\tc.exe`, so
/// the hook would silently die. Re-run the existing never-clobber merge with
/// the new pulse-ctl.exe path — it updates OUR group in place and re-derives
/// the trust hash, leaving every user entry untouched.
///
/// Strictly repair-only: if hooks.json is absent or carries no `__codex-hook`
/// entry, consent was never given and NOTHING is written. WSL/ssh codex lanes
/// and the claude lanes embed no Windows exe path (POSIX `~/.tc` scripts /
/// per-launch argv injection) — nothing to repair there.
///
/// `TC_HOOK_HOME` overrides the home dir for sandboxed staging proofs (same
/// spirit as TC_DATA_DIR; dirs::home_dir ignores USERPROFILE on Windows).
fn repair_codex_hook_command(ctl_exe: &std::path::Path) {
    let home = std::env::var_os("TC_HOOK_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir);
    let Some(home) = home else { return };
    let codex_home = home.join(".codex");
    let hooks_file = codex_home.join("hooks.json");
    let Ok(body) = std::fs::read_to_string(&hooks_file) else {
        return; // no codex hooks installed — nothing to repair
    };
    if !body.contains(codex_hooks::TC_HOOK_SUBCOMMAND) {
        return; // no entry of ours — consent was never given; never install unasked
    }
    if !ctl_exe.is_file() {
        return;
    }
    let Some(command) = codex_hooks::windows_hook_command_for_exe(ctl_exe) else {
        return; // path not expressible for cmd /C — leave the old entry; the
                // GUI's consent lane reports install problems interactively
    };
    let target = codex_hooks::LocalTarget {
        access_home: codex_home,
        codex_hooks_path: hooks_file.to_string_lossy().into_owned(),
        command,
        script: None,
    };
    match codex_hooks::install_local(&target) {
        Ok(outcome) => log::info!("codex hook path repair: {outcome:?}"),
        Err(e) => log::warn!("codex hook path repair failed: {e}"),
    }
}

/// Velopack `--veloapp-uninstall` fast hook (#34): remove the artifacts this
/// app manages OUTSIDE Velopack's own directory — the bin\ deploy target and
/// the HKCU Run key — after quiescing the daemon. The DATA DIR (state.json,
/// journals\, gui.json, backups\) is deliberately KEPT: a reinstall restores
/// every session; full purge is documented as "delete
/// %LOCALAPPDATA%\Pulse". Hook entries in ~/.codex are
/// left in place (inert without pulse-ctl.exe; user-owned files). Velopack
/// terminates the hook after 30s — quiesce is bounded well under that.
fn uninstall_cleanup() {
    // Branded "Uninstalling… → Uninstalled" window (#34 lifecycle UI),
    // spawned FIRST so it covers the quiesce; it runs from a %TEMP% copy so
    // it survives (and never image-locks) the install-dir deletion.
    spawn_lifecycle_helper(&["--uninstall-ui"]);
    let _ = quiesce_daemon();
    // The Run key is a real-profile global: a TC_DATA_DIR sandbox never
    // wrote one (install_autostart is gated), so never delete it from one.
    if !state::data_dir_overridden() {
        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        if let Ok(key) = hkcu.open_subkey_with_flags(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            winreg::enums::KEY_SET_VALUE,
        ) {
            let _ = key.delete_value(RUN_KEY_VALUE);
            // Pre-rebrand value, in case an ancient install never migrated.
            let _ = key.delete_value(LEGACY_RUN_KEY_VALUE);
        }
    }
    // Bounded retry (staging-proof finding): quiesce_daemon() returns the
    // instant daemon.lock is releasable, but the daemon's exe IMAGE lock
    // lives until the process fully terminates — a single remove_dir_all
    // fired in that window deletes pulse-ctl.exe/.version and then fails on
    // pulse.exe, stranding a half-deleted bin\.
    let bin = state::data_dir().join("bin");
    for _ in 0..30 {
        if std::fs::remove_dir_all(&bin).is_ok() || !bin.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Copy this exe to %TEMP% and run it detached with `args` (#34 lifecycle
/// UI). The copy is what lets the window outlive — and never image-lock —
/// the Velopack install dir while Update.exe swaps or deletes it; the helper
/// self-deletes its temp copy on exit (lifecycle_ui). Best-effort: lifecycle
/// chrome must never block the actual install/update/uninstall work.
pub fn spawn_lifecycle_helper(args: &[&str]) {
    let run = || -> std::io::Result<()> {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        let me = std::env::current_exe()?;
        let dst = std::env::temp_dir().join(format!("pulse-lifecycle-{}.exe", std::process::id()));
        std::fs::copy(&me, &dst)?;
        std::process::Command::new(&dst)
            .args(args)
            .creation_flags(DETACHED_PROCESS)
            .spawn()?;
        Ok(())
    };
    if let Err(e) = run() {
        log::warn!("lifecycle helper spawn failed: {e}");
    }
}

/// Copy this binary to the stable path, point autostart at it, and start the
/// daemon from there. Any daemon already running from an old path is asked to
/// shut down first so the single-instance lock is released.
fn install() -> anyhow::Result<()> {
    let Some(target) = installed_exe_path() else {
        anyhow::bail!("could not resolve install path");
    };
    let current = std::env::current_exe()?;

    // One-time rebrand migration (no-op unless the legacy TerminalControl
    // data dir exists and the new one doesn't). Runs BEFORE the quiesce
    // below: the old daemon lives in the old dir and must be hand-shaken
    // there; everything after this line operates on the new layout.
    let migrated = migrate_legacy_data_dir();

    // Ask a running daemon to exit so we can replace the binary and re-own
    // the lock (shared helper, #34). Warn-and-continue: the copy below is
    // the honest failure point.
    if !quiesce_daemon() {
        println!("warning: daemon still holds daemon.lock; copy may fail");
    }

    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Bounded-retry copies: quiesce_daemon() returns the instant daemon.lock
    // is releasable, but the quiesced daemon's exe IMAGE lock lives until the
    // process fully terminates — a re-run of --install (target exe = the
    // running daemon's image) raced it and failed with "file in use" (staging
    // finding; same race the uninstall hook retries around).
    let copy_retry = |src: &std::path::Path, dst: &std::path::Path| -> std::io::Result<u64> {
        let mut last = None;
        for _ in 0..30 {
            match std::fs::copy(src, dst) {
                Ok(n) => return Ok(n),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err(last.expect("at least one attempt"))
    };
    if current != target {
        copy_retry(&current, &target)?;
    }

    // The controller CLI ships alongside: copy a sibling pulse-ctl.exe when
    // the build produced one (missing = a single-bin `cargo run` build — warn
    // and continue, the main exe still works).
    let ctl_target = target.with_file_name("pulse-ctl.exe");
    let ctl_source = current.with_file_name("pulse-ctl.exe");
    if ctl_source.exists() && ctl_source != ctl_target {
        match copy_retry(&ctl_source, &ctl_target) {
            Ok(_) => println!("Installed controller CLI to {}", ctl_target.display()),
            Err(e) => println!("warning: pulse-ctl.exe copy failed: {e}"),
        }
    } else if !ctl_source.exists() {
        println!(
            "warning: no sibling pulse-ctl.exe next to this binary; controller CLI not installed"
        );
    }

    // A migration retired the old-named controller the user's consented
    // ~/.codex hook entry points at — repoint it at the CLI just deployed.
    if migrated {
        repair_codex_hook_command(&ctl_target);
    }

    // Register autostart pointing at the installed copy. The Run key is a
    // real-profile global: a TC_DATA_DIR sandbox install must never repoint
    // the machine's actual autostart at a scratch binary (matches the
    // daemon's gated self-heal and uninstall_cleanup's assumption above).
    if !state::data_dir_overridden() {
        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        if let Ok((key, _)) =
            hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        {
            key.set_value(
                RUN_KEY_VALUE,
                &format!("\"{}\" --daemon", target.display()),
            )?;
        }
    } else {
        println!("sandbox (TC_DATA_DIR): autostart not registered");
    }

    // Launch the daemon from the installed path. DETACHED_PROCESS only —
    // CREATE_NEW_PROCESS_GROUP would start the daemon with Ctrl+C disabled,
    // and ConPTY children inherit that: native commands in every terminal
    // would ignore Ctrl+C (see gui::ipc::spawn_daemon).
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        std::process::Command::new(&target)
            .arg("--daemon")
            .creation_flags(DETACHED_PROCESS)
            .spawn()?;
    }

    println!("Installed to {}", target.display());
    if state::data_dir_overridden() {
        println!("Daemon launched.");
    } else {
        println!("Autostart registered; daemon launched.");
    }
    Ok(())
}
