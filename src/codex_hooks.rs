//! Codex-session attribution hook engine (task #30 — the codex mirror of the
//! claude attribution layers).
//!
//! OpenAI Codex has NO live pid registry (unlike claude ≥2.1.200), so its
//! backbone is birth-correlation of the rollout/threads store (tracker.rs
//! `codex_extract`) which fixes LAUNCH identity only. The missing 100% —
//! following an in-TUI `/resume`/`/new`/`/fork` switch and breaking a
//! parallel same-cwd tie — comes from codex's own SessionStart hook, which
//! fires at the first prompt (source=startup) and on every in-TUI switch
//! (source=resume|clear|compact), carrying the live `session_id`.
//!
//! Codex hooks are TRUST-GATED: an untrusted `~/.codex/hooks.json` silently
//! does not run (no interactive prompt in `codex exec`; a review banner in
//! the TUI). TC pre-seeds the exact `trusted_hash` codex would compute into
//! `~/.codex/config.toml [hooks.state]`, so the hook fires from the first
//! prompt with ZERO user friction — the same "no surprises" bar as claude's
//! `--settings` injection.
//!
//! Transport per lane (mirrors claude L2/L3):
//!   - Windows-native: hook command = `"<tc.exe>" __codex-hook SessionStart`.
//!     Codex passes its full env to hook children, so the hook inherits
//!     `TC_SESSION_ID` (the terminal id) and reads `session_id` off stdin,
//!     then posts `ReportCliSession{adapter:"codex"}` to the daemon. No tty
//!     needed — the native-Windows hook child has no console anyway.
//!   - WSL / ssh: hook command = `~/.tc/codex-hook.sh SessionStart`, a POSIX
//!     beacon that prints `ESC ] 7717 ; tcbeacon ; codex ; <event> ;
//!     <source> ; <sid> BEL` to /dev/tty — the exact byte path a session's
//!     own pty carries into the TC journal, where the BlockScanner's
//!     `tcbeacon` verb hands it to `Core::on_beacon`.
//!
//! SANCTIONED WRITES: `~/.codex/hooks.json` (merged, never clobbered),
//! `~/.codex/config.toml [hooks.state.'<key>']` (format-preserving insert of
//! ONE trust entry via toml_edit — the user's comments/order/other tables are
//! untouched), and the POSIX `~/.tc/codex-hook.sh` script (WSL/ssh only).

use std::path::{Path, PathBuf};

/// The event we install. Codex has NO SessionEnd (unlike claude); SessionStart
/// with source ∈ {startup, resume, clear, compact} covers new/resume/fork and
/// every in-TUI switch, so one handler is the whole story.
pub const EVENT_SNAKE: &str = "session_start";
pub const EVENT_JSON: &str = "SessionStart";

/// The tc.exe subcommand the Windows-lane hook invokes.
pub const TC_HOOK_SUBCOMMAND: &str = "__codex-hook";

// ───────────────────────────── SHA-256 (hand-rolled) ─────────────────────────
//
// No sha2 in the dependency tree; the codebase hand-rolls its primitives
// (base64 in bootstrap.rs, hex in blocks.rs). This is a straight FIPS-180-4
// SHA-256, pinned by NIST vectors AND by the empirically-observed codex hash
// in the unit tests below — a single-byte drift there fails the build.

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // Pad: message + 0x80 + zeros + 64-bit big-endian bit length.
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let j = i * 4;
            *word = u32::from_be_bytes([chunk[j], chunk[j + 1], chunk[j + 2], chunk[j + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

use crate::strip::hex_lower;

// ───────────────────────────── trust hash ─────────────────────────────

/// The `trusted_hash` codex computes for a single command handler, reproduced
/// byte-for-byte (verified live against codex-cli 0.142.5). Source:
/// `codex-rs/config/src/fingerprint.rs::version_for_toml` +
/// `hooks/src/engine/discovery.rs::command_hook_hash`.
///
/// Codex normalizes the identity to `NormalizedHookIdentity { event_name,
/// #[flatten] MatcherGroup { matcher, hooks: [normalized_handler] } }`,
/// converts struct→TOML→JSON, RECURSIVELY sorts object keys, serializes
/// compact, and SHA-256s that. The normalized handler is always
/// `{type:"command", command:<final>, timeout:600, async:false}` with
/// commandWindows/statusMessage dropped when None (TOML omits None), so the
/// canonical JSON is fully determined by the final command + the matcher.
///
/// `matcher`: SessionStart passes the group matcher through, so our `""`
/// stays as an empty string (PRESENT). UserPromptSubmit/Stop force None (the
/// key is OMITTED). `command` is the FINAL command after codex's Windows
/// `commandWindows` substitution — we set only `command`, so it is that.
pub fn trusted_hash(command: &str, event_snake: &str, matcher: Option<&str>) -> String {
    // serde_json escapes the string values exactly as codex's serializer does
    // (it is the same serde_json). Keys are assembled in sorted order (codex's
    // canonical_json sorts recursively): identity => event_name < hooks <
    // matcher; handler => async < command < timeout < type.
    let cmd_lit = serde_json::to_string(command).unwrap_or_else(|_| "\"\"".into());
    let handler = format!(r#"{{"async":false,"command":{cmd_lit},"timeout":600,"type":"command"}}"#);
    let canonical = match matcher {
        Some(m) => {
            let m_lit = serde_json::to_string(m).unwrap_or_else(|_| "\"\"".into());
            format!(r#"{{"event_name":"{event_snake}","hooks":[{handler}],"matcher":{m_lit}}}"#)
        }
        None => format!(r#"{{"event_name":"{event_snake}","hooks":[{handler}]}}"#),
    };
    format!("sha256:{}", hex_lower(&sha256(canonical.as_bytes())))
}

/// The `[hooks.state]` sub-table key for our handler:
/// `<hooks.json path>:<event_snake>:<group_idx>:<handler_idx>`. The path is
/// codex's OWN view of the file (Windows: canonical backslash CODEX_HOME;
/// POSIX: the remote/WSL home path) — NOT TC's access path (which may be a
/// `\\wsl$` UNC or an sftp temp). Handler index is 0 (our group is
/// single-handler); group index is where our group lands after the merge.
pub fn state_key(hooks_json_path: &str, event_snake: &str, group_idx: usize) -> String {
    format!("{hooks_json_path}:{event_snake}:{group_idx}:0")
}

// ───────────────────────────── hooks.json merge ─────────────────────────────

/// True if a handler command is one of ours (`__codex-hook` for the Windows
/// lane, `codex-hook.sh` for the POSIX beacon). Used for idempotent re-runs
/// and to locate our group index for the trust key.
fn is_our_command(cmd: &str) -> bool {
    cmd.contains(TC_HOOK_SUBCOMMAND) || cmd.contains("codex-hook.sh")
}

/// Result of a hooks.json merge: the merged pretty JSON, the group index of
/// OUR SessionStart handler (drives the trust key), and whether the file
/// changed (false = already correct, idempotent).
pub struct HooksMerge {
    pub body: String,
    pub group_idx: usize,
    pub changed: bool,
}

/// Merge our SessionStart handler into a codex hooks.json body, non-destructively.
///
/// - `existing` None / empty ⇒ start from `{}`.
/// - Err ⇒ the file is not a JSON object, or hooks / hooks.SessionStart is the
///   wrong shape — REFUSED, never clobbered (the same doctrine as claude's
///   settings merge: a broken user file is surfaced, not overwritten).
/// - A pre-existing user group at index 0 shifts OUR group to index 1 — the
///   returned `group_idx` reflects the merged position so the trust key is
///   computed against reality (the task's hard requirement).
/// - Idempotent: our own group is detected; if its command drifted (tc.exe
///   moved) it is UPDATED in place and `changed` is true.
/// - Only a group whose handlers are ALL ours is ever replaced. A mixed
///   group (user handlers sharing a group with one of our commands) is left
///   verbatim and a fresh group of our own is appended — `group_idx` (and
///   thus the trust key) always names the group this merge actually wrote.
pub fn merge_hooks_json(existing: Option<&str>, command: &str) -> Result<HooksMerge, String> {
    let mut root: serde_json::Value = match existing {
        None => serde_json::json!({}),
        Some(s) if s.trim().is_empty() => serde_json::json!({}),
        Some(s) => serde_json::from_str(s)
            .map_err(|e| format!("~/.codex/hooks.json is not valid JSON ({e}); not touching it"))?,
    };
    if !root.is_object() {
        return Err("~/.codex/hooks.json is not a JSON object; not touching it".into());
    }
    let hooks = root
        .as_object_mut()
        .expect("checked object")
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        return Err("~/.codex/hooks.json has a non-object \"hooks\"; not touching it".into());
    }
    let arr = hooks
        .as_object_mut()
        .expect("checked object")
        .entry(EVENT_JSON)
        .or_insert_with(|| serde_json::json!([]));
    let Some(list) = arr.as_array_mut() else {
        return Err(format!(
            "~/.codex/hooks.json has a non-array hooks.{EVENT_JSON}; not touching it"
        ));
    };

    // Locate OUR group: one whose handlers are ALL ours (`__codex-hook`/
    // `codex-hook.sh`). Codex indexes handlers by (group_idx, handler_idx);
    // we always occupy a single-handler group of our own at handler_idx 0.
    // A MIXED group — a user added their own handlers next to (a copy of)
    // ours — is user content and is NEVER replaced: replacing the whole
    // group would silently delete their handlers. Our stray handler inside
    // such a group stays untrusted (trust keys are per-handler) and inert.
    let fully_ours = list.iter().position(|g| {
        g["hooks"].as_array().is_some_and(|inner| {
            !inner.is_empty()
                && inner
                    .iter()
                    .all(|h| h["command"].as_str().is_some_and(is_our_command))
        })
    });

    let our_entry = serde_json::json!({
        "matcher": "",
        "hooks": [{ "type": "command", "command": command }]
    });

    let (group_idx, changed) = match fully_ours {
        Some(idx) => {
            if list[idx] == our_entry {
                (idx, false) // already exactly right — idempotent no-op
            } else {
                list[idx] = our_entry; // tc.exe moved / shape drift ⇒ refresh
                (idx, true)
            }
        }
        None => {
            list.push(our_entry);
            (list.len() - 1, true)
        }
    };

    let body = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    Ok(HooksMerge {
        body,
        group_idx,
        changed,
    })
}

// ───────────────────────────── config.toml trust merge ─────────────────────────────

/// Insert-or-update exactly ONE `[hooks.state.'<key>']` trust entry in a
/// config.toml body, preserving everything else (comments, key order, other
/// tables) via toml_edit. Returns the new body + whether it changed.
///
/// Err ⇒ the config isn't parseable TOML (never clobber a broken/locked file;
/// codex itself would refuse to start on it, so surfacing is correct).
pub fn merge_config_trust(existing: &str, key: &str, hash: &str) -> Result<(String, bool), String> {
    use toml_edit::{DocumentMut, Item, Table, Value};

    let mut doc: DocumentMut = existing
        .parse()
        .map_err(|e| format!("~/.codex/config.toml is not valid TOML ({e}); not touching it"))?;

    // Ensure [hooks] and [hooks.state] exist as tables (dotted, so a `[hooks]`
    // the user already opened for other keys is reused, not duplicated).
    let hooks = doc
        .entry("hooks")
        .or_insert_with(|| Item::Table(Table::new()));
    let Some(hooks_tbl) = hooks.as_table_mut() else {
        return Err("~/.codex/config.toml has a non-table [hooks]; not touching it".into());
    };
    // Render nested tables with their full header path (`[hooks.state.'key']`)
    // rather than inline, matching codex's own writes. Set on [hooks] before
    // taking the nested [hooks.state] borrow.
    hooks_tbl.set_implicit(true);
    let state = hooks_tbl
        .entry("state")
        .or_insert_with(|| Item::Table(Table::new()));
    let Some(state_tbl) = state.as_table_mut() else {
        return Err("~/.codex/config.toml has a non-table [hooks.state]; not touching it".into());
    };
    state_tbl.set_implicit(true);

    // The entry sub-table for our key.
    let entry = state_tbl
        .entry(key)
        .or_insert_with(|| Item::Table(Table::new()));
    let Some(entry_tbl) = entry.as_table_mut() else {
        return Err("~/.codex/config.toml has a non-table hook-state entry; not touching it".into());
    };

    let already = entry_tbl
        .get("trusted_hash")
        .and_then(|i| i.as_str())
        .map(|s| s == hash)
        .unwrap_or(false);
    if already {
        return Ok((doc.to_string(), false));
    }
    entry_tbl.insert("trusted_hash", Item::Value(Value::from(hash)));
    Ok((doc.to_string(), true))
}

// ───────────────────────────── local install (Win / WSL via a filesystem) ─────────────────────────────

/// One codex-home install target: `access` = where TC writes (a local path or
/// a `\\wsl$` UNC); `codex_path` = the path codex ITSELF sees for hooks.json
/// (drives the trust key). For a native-Windows home these are identical.
pub struct LocalTarget {
    /// Directory TC writes to (the `.codex` dir, via whatever mount).
    pub access_home: PathBuf,
    /// hooks.json path as CODEX sees it (Windows backslash / POSIX slash).
    pub codex_hooks_path: String,
    /// The final hook command string for this lane.
    pub command: String,
    /// WSL/local-POSIX lane: the beacon script to drop, as (write path, body).
    /// None for the native-Windows lane (the `tc __codex-hook` command needs
    /// no script). The write path is the TC-access path to `~/.tc/codex-hook.sh`
    /// (e.g. a `\\wsl$` UNC).
    pub script: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// hooks.json and/or the trust entry were written this run.
    Installed,
    /// Everything was already in place (idempotent re-run).
    AlreadyInstalled,
}

/// Install (or heal, idempotently) the codex SessionStart hook + trust entry
/// into a codex home reachable on the local filesystem. Blocking file IO —
/// call off the GUI thread. Never clobbers: hooks.json is merged, config.toml
/// is toml_edit-spliced.
pub fn install_local(t: &LocalTarget) -> Result<Outcome, String> {
    let hooks_file = t.access_home.join("hooks.json");
    let config_file = t.access_home.join("config.toml");
    std::fs::create_dir_all(&t.access_home).map_err(|e| format!("mkdir .codex: {e}"))?;

    // Only a provable "file does not exist" may start the merge from empty —
    // an unreadable-but-existing file (permission, lock, transient \\wsl$
    // error) must refuse, or the atomic rename below would replace the
    // user's real config with a hooks-only one.
    let existing_hooks = match std::fs::read_to_string(&hooks_file) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("read {}: {e}; not touching it", hooks_file.display())),
    };
    let merge = merge_hooks_json(existing_hooks.as_deref(), &t.command)?;
    let key = state_key(&t.codex_hooks_path, EVENT_SNAKE, merge.group_idx);
    let hash = trusted_hash(&t.command, EVENT_SNAKE, Some(""));

    // A brand-new config.toml is valid empty TOML.
    let existing_config = match std::fs::read_to_string(&config_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}; not touching it", config_file.display())),
    };
    let (config_body, config_changed) = merge_config_trust(&existing_config, &key, &hash)?;

    // Drop the beacon script first where the POSIX lane needs one (heals a
    // deleted script; harmless to rewrite). Its content is stable, so this
    // never flips the Outcome on its own.
    if let Some(script_path) = &t.script {
        if let Some(parent) = script_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir .tc: {e}"))?;
        }
        write_atomic(script_path, BEACON_SCRIPT.as_bytes())?;
    }
    // Write hooks.json first (the hook is inert until trusted anyway), then the
    // trust entry — so a crash between the two never leaves a TRUSTED hash for
    // a hooks.json that doesn't exist. Atomic temp+rename each.
    if merge.changed || existing_hooks.is_none() {
        write_atomic(&hooks_file, merge.body.as_bytes())?;
    }
    if config_changed {
        write_atomic(&config_file, config_body.as_bytes())?;
    }
    Ok(if merge.changed || config_changed {
        Outcome::Installed
    } else {
        Outcome::AlreadyInstalled
    })
}

/// Temp-name + rename write (atomic on a single volume) so a mid-write death
/// never truncates the user's real config.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!(
        "tc-new-{}",
        std::process::id()
    ));
    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename into {}: {e}", path.display())
    })
}

// ───────────────────────────── the Windows-native hook command ─────────────────────────────

/// The Windows-lane hook command: `<pulse-ctl.exe> __codex-hook SessionStart`.
/// pulse-ctl.exe is resolved as the sibling of the CURRENT exe (survives
/// install-dir moves; the trust hash re-derives if the path changes).
///
/// QUOTING: codex runs the command via Rust's `Command::arg` → `cmd.exe /C`.
/// Rust wraps any space-containing arg in quotes and BACKSLASH-escapes inner
/// quotes — which cmd.exe mis-parses (`\"` is not cmd syntax), so a quoted
/// program path silently fails ("hook Failed", proven live). The only form
/// that round-trips is a SPACE-FREE program path used unquoted: Rust wraps the
/// whole string once, cmd /C strips that one pair, and runs the bare path. So
/// a pulse-ctl.exe path with spaces is reduced to its 8.3 SHORT path
/// (space-free); the common install path (`%LOCALAPPDATA%\Pulse\bin`) has no
/// spaces and is used as-is. None ⇒ no sibling pulse-ctl.exe (dev single-bin
/// build — birth-correlation still covers the lane) or no space-free form
/// available (8.3 disabled on the volume — rare; degrade to correlation).
pub fn windows_hook_command() -> Option<String> {
    let ctl = std::env::current_exe().ok()?.parent()?.join("pulse-ctl.exe");
    windows_hook_command_for_exe(&ctl)
}

/// `windows_hook_command` for an explicit controller-CLI path (also used by
/// the rebrand migration's hook repair, where the CLI is the freshly deployed
/// bin\pulse-ctl.exe rather than a sibling of the current exe).
pub fn windows_hook_command_for_exe(ctl: &Path) -> Option<String> {
    if !ctl.is_file() {
        return None;
    }
    let path = ctl.to_string_lossy().into_owned();
    let path = if path.contains(' ') {
        short_path(ctl)?
    } else {
        path
    };
    windows_hook_command_for(&path)
}

/// Testable core: the command string for a given controller-CLI path. The
/// path must be space-free and quote-free for the `cmd /C` round-trip (see
/// `windows_hook_command`); anything else ⇒ None.
pub fn windows_hook_command_for(tc_path: &str) -> Option<String> {
    if tc_path.contains(' ') || tc_path.contains('"') {
        return None;
    }
    Some(format!("{tc_path} {TC_HOOK_SUBCOMMAND} {EVENT_JSON}"))
}

/// The 8.3 short (space-free) form of a path via GetShortPathNameW. None if the
/// call fails or the result still contains a space (8.3 generation disabled on
/// the volume).
#[cfg(windows)]
fn short_path(p: &Path) -> Option<String> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // First call sizes the buffer, second fills it.
    let need =
        unsafe { windows::Win32::Storage::FileSystem::GetShortPathNameW(windows::core::PCWSTR(wide.as_ptr()), None) };
    if need == 0 {
        return None;
    }
    let mut buf = vec![0u16; need as usize];
    let got = unsafe {
        windows::Win32::Storage::FileSystem::GetShortPathNameW(
            windows::core::PCWSTR(wide.as_ptr()),
            Some(&mut buf),
        )
    };
    if got == 0 || got as usize > buf.len() {
        return None;
    }
    let s = std::ffi::OsString::from_wide(&buf[..got as usize])
        .to_string_lossy()
        .into_owned();
    (!s.contains(' ')).then_some(s)
}

#[cfg(not(windows))]
fn short_path(_p: &Path) -> Option<String> {
    None
}

// ───────────────────────────── POSIX beacon (WSL / ssh) ─────────────────────────────

/// The POSIX beacon script body (LF; POSIX sh only). Reports the live codex
/// session id to the hosting terminal via a private OSC on /dev/tty. Mirrors
/// claude's `claude-hook.sh`, but the OSC carries the `codex` adapter in the
/// slot after `tcbeacon` so `Core::on_beacon` folds it into a codex inner_cli.
/// The event rides argv; sid + source come from the hook's stdin JSON. Always
/// silent, always exit 0 — codex must never see a failing hook.
pub const BEACON_SCRIPT: &str = r#"#!/bin/sh
# Pulse codex-session beacon.
# Installed by Pulse; safe to delete (codex sessions then fall back
# to birth-time correlation). Reports the live codex session id to the hosting
# terminal via a private OSC on /dev/tty. Always silent, always exit 0.
ev="${1:-SessionStart}"
in=$(cat 2>/dev/null || true)
sid=$(printf '%s' "$in" | sed -n 's/.*"session_id"[^"]*"\([^"]*\)".*/\1/p' | head -n 1)
src=$(printf '%s' "$in" | sed -n 's/.*"source"[^"]*"\([^"]*\)".*/\1/p' | head -n 1)
[ -n "$sid" ] || exit 0
{ printf '\033]7717;tcbeacon;codex;%s;%s;%s\007' "$ev" "${src:-unknown}" "$sid" > /dev/tty; } 2>/dev/null || true
exit 0
"#;

/// The POSIX hook command that runs the beacon script (installed at
/// `~/.tc/codex-hook.sh`). Invoked as `sh <script>` so the file needs no
/// execute bit — a `\\wsl$` write can't chmod, and an sftp upload need not.
/// `~` expands in the `/bin/sh -lc` codex wraps hook commands in.
pub const POSIX_HOOK_COMMAND: &str = "sh ~/.tc/codex-hook.sh SessionStart";

// ───────────────────────────── ssh remote install ─────────────────────────────

const REMOTE_INSTALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

fn tmp_dir() -> PathBuf {
    crate::state::data_dir().join("tmp")
}

fn run_batch(
    sftp: &Path,
    meta_args: &[String],
    tag: &str,
    body: &str,
) -> Result<std::process::Output, String> {
    crate::ssh_transport::run_install_batch(sftp, meta_args, tag, body, REMOTE_INSTALL_DEADLINE)
}

/// Install (or heal) the codex SessionStart beacon on one ssh host, over the
/// terminal's own transport identity — the consent-gated remote write.
/// Mirrors claude's `claude_hooks::install_remote` but for codex: the beacon
/// script + a merged `~/.codex/hooks.json` + the computed `trusted_hash` in
/// `~/.codex/config.toml [hooks.state]` (codex hooks are trust-gated; claude's
/// settings.json hooks are not). Blocking — run on a worker thread.
///
/// Connection 1 resolves the remote home (`pwd`) and fetches the existing
/// config.toml + hooks.json (missing ok). Merge is local (never clobber).
/// Connection 2 uploads all three via temp-name + posix-rename (atomic).
pub fn install_remote(program: &str, meta_args: &[String]) -> Result<Outcome, String> {
    use crate::ssh_transport::fwd;
    let sftp = crate::ssh_transport::resolve_sftp(program)
        .map_err(|looked| format!("no sftp.exe (looked in {looked})"))?;
    let dir = tmp_dir();
    let _ = std::fs::create_dir_all(&dir);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let local_cfg = dir.join(format!("codex-cfg-{nonce}.toml"));
    let local_hooks = dir.join(format!("codex-hooks-{nonce}.json"));
    let local_script = dir.join(format!("codex-script-{nonce}.sh"));
    let local_cfg_out = dir.join(format!("codex-cfg-out-{nonce}.toml"));
    let local_hooks_out = dir.join(format!("codex-hooks-out-{nonce}.json"));
    let cleanup = || {
        for p in [&local_cfg, &local_hooks, &local_script, &local_cfg_out, &local_hooks_out] {
            let _ = std::fs::remove_file(p);
        }
    };

    // 1) Resolve $HOME (pwd) + fetch config.toml + hooks.json (missing ok).
    //    Each `-get` is paired with a `-ls -l` so absence is PROVABLE and
    //    the fetched size is CHECKABLE (R4-F3): a fetch that failed for any
    //    reason other than "not found" must refuse — including a -get that
    //    died mid-transfer leaving a partial (but often still parseable)
    //    local file — or the merge would run against a truncated body and
    //    the atomic rename in step 3 would replace the user's real file.
    const REMOTE_CFG: &str = ".codex/config.toml";
    const REMOTE_HOOKS: &str = ".codex/hooks.json";
    let _ = std::fs::remove_file(&local_cfg);
    let _ = std::fs::remove_file(&local_hooks);
    let fetch = format!(
        "pwd\n-ls -l \"{REMOTE_CFG}\"\n-ls -l \"{REMOTE_HOOKS}\"\n-get \"{REMOTE_CFG}\" \"{}\"\n-get \"{REMOTE_HOOKS}\" \"{}\"\n",
        fwd(&local_cfg),
        fwd(&local_hooks)
    );
    let out = run_batch(&sftp, meta_args, "codex-hook-fetch", &fetch).inspect_err(|_| cleanup())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        cleanup();
        return Err(format!(
            "connection failed: {:?}",
            crate::ssh_transport::classify_conn(&stderr)
        ));
    }
    let Some(home) = crate::ssh_transport::parse_pwd(&stdout) else {
        cleanup();
        return Err("could not resolve remote home (no pwd)".into());
    };
    let home = home.trim_end_matches('/').to_string();
    let remote_hooks_path = format!("{home}/.codex/hooks.json");

    // 2) Merge hooks.json + config.toml LOCALLY (never clobber).
    let existing_hooks = match std::fs::read_to_string(&local_hooks) {
        Ok(s) => {
            // R4-F3: a partial fetch must not masquerade as the whole file.
            if !crate::ssh_transport::fetched_len_matches(REMOTE_HOOKS, &stdout, s.len() as u64) {
                cleanup();
                return Err(format!(
                    "remote ~/{REMOTE_HOOKS} was only partially fetched; not touching it"
                ));
            }
            Some(s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !crate::ssh_transport::remote_file_absent(REMOTE_HOOKS, &stdout, &stderr) {
                cleanup();
                return Err(format!(
                    "remote ~/{REMOTE_HOOKS} exists but could not be fetched; not touching it"
                ));
            }
            None
        }
        Err(e) => {
            cleanup();
            return Err(format!("read fetched hooks.json: {e}"));
        }
    };
    let merge = match merge_hooks_json(existing_hooks.as_deref(), POSIX_HOOK_COMMAND) {
        Ok(m) => m,
        Err(e) => {
            cleanup();
            return Err(e);
        }
    };
    let key = state_key(&remote_hooks_path, EVENT_SNAKE, merge.group_idx);
    let hash = trusted_hash(POSIX_HOOK_COMMAND, EVENT_SNAKE, Some(""));
    let existing_cfg = match std::fs::read_to_string(&local_cfg) {
        Ok(s) => {
            // R4-F3: TOML truncated at a line boundary still parses — the
            // size proof is the only thing standing between a mid-transfer
            // failure and merging away the user's config tail.
            if !crate::ssh_transport::fetched_len_matches(REMOTE_CFG, &stdout, s.len() as u64) {
                cleanup();
                return Err(format!(
                    "remote ~/{REMOTE_CFG} was only partially fetched; not touching it"
                ));
            }
            s
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !crate::ssh_transport::remote_file_absent(REMOTE_CFG, &stdout, &stderr) {
                cleanup();
                return Err(format!(
                    "remote ~/{REMOTE_CFG} exists but could not be fetched; not touching it"
                ));
            }
            String::new()
        }
        Err(e) => {
            cleanup();
            return Err(format!("read fetched config.toml: {e}"));
        }
    };
    let (cfg_body, cfg_changed) = match merge_config_trust(&existing_cfg, &key, &hash) {
        Ok(v) => v,
        Err(e) => {
            cleanup();
            return Err(e);
        }
    };

    // 3) Upload: script always (heals a deleted one), hooks.json when the merge
    //    added/changed our entry, config.toml when the trust entry changed. Temp
    //    name + rename keeps a mid-upload death from truncating the user's files.
    if let Err(e) = std::fs::write(&local_script, BEACON_SCRIPT) {
        cleanup();
        return Err(format!("local script write: {e}"));
    }
    let mut batch = String::new();
    batch.push_str("-mkdir .tc\n");
    batch.push_str("-rm .tc/codex-hook.sh.tc-new\n");
    batch.push_str(&format!("put \"{}\" .tc/codex-hook.sh.tc-new\n", fwd(&local_script)));
    batch.push_str("rename .tc/codex-hook.sh.tc-new .tc/codex-hook.sh\n");
    batch.push_str("-mkdir .codex\n");
    if merge.changed || existing_hooks.is_none() {
        if let Err(e) = std::fs::write(&local_hooks_out, &merge.body) {
            cleanup();
            return Err(format!("local hooks write: {e}"));
        }
        batch.push_str("-rm .codex/hooks.json.tc-new\n");
        batch.push_str(&format!("put \"{}\" .codex/hooks.json.tc-new\n", fwd(&local_hooks_out)));
        batch.push_str("rename .codex/hooks.json.tc-new .codex/hooks.json\n");
    }
    if cfg_changed {
        if let Err(e) = std::fs::write(&local_cfg_out, &cfg_body) {
            cleanup();
            return Err(format!("local config write: {e}"));
        }
        batch.push_str("-rm .codex/config.toml.tc-new\n");
        batch.push_str(&format!("put \"{}\" .codex/config.toml.tc-new\n", fwd(&local_cfg_out)));
        batch.push_str("rename .codex/config.toml.tc-new .codex/config.toml\n");
    }
    let out = run_batch(&sftp, meta_args, "codex-hook-put", &batch).inspect_err(|_| cleanup())?;
    cleanup();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "install failed: {:?}",
            crate::ssh_transport::classify_conn(&stderr)
        ));
    }
    Ok(if merge.changed || cfg_changed {
        Outcome::Installed
    } else {
        Outcome::AlreadyInstalled
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 NIST vectors — a drift here is a build failure.
    #[test]
    fn sha256_nist_vectors() {
        assert_eq!(
            hex_lower(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_lower(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex_lower(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// Full-command pinned vector. Provenance: this algorithm's output was
    /// verified live against codex-cli 0.142.5 on 2026-07-06 (a scratch
    /// CODEX_HOME accepted the hash we computed for the real staging path)
    /// before the fixture was anonymized for release; the neutral vector
    /// below was computed independently with python hashlib over the
    /// documented canonical form, same method as
    /// `trusted_hash_non_ascii_command_pinned`.
    #[test]
    fn trusted_hash_pinned_independent() {
        // Same shape as the live-verified command (note the deliberate single
        // forward slash before `codexhome` — the staging path as it was
        // passed to codex mixed separators).
        let cmd = "cmd /c echo done> \"C:\\Users\\alice\\AppData\\Local\\Temp\\claude\\C--my-proj\\00000000-0000-4000-8000-000000000000\\scratchpad/codexhome\\tc-sess.txt\"";
        assert_eq!(
            trusted_hash(cmd, "session_start", Some("")),
            "sha256:fbec6a0d939fa0b4fe52900b32d577a86ba68aaaf6b41f45918b9f348e36f6f6"
        );
    }

    /// Non-ASCII + control bytes in the command: serde_json's escaping rules
    /// (control chars escaped `\t`-style, non-ASCII passed through as UTF-8)
    /// are part of the hash identity — codex hashes with the same serde_json.
    /// Expected value computed INDEPENDENTLY (python hashlib over the
    /// documented canonical string), so an escaping change in either
    /// serializer fails this vector.
    #[test]
    fn trusted_hash_non_ascii_command_pinned() {
        assert_eq!(
            trusted_hash("echo héllo — ünïcode\ttab", "session_start", Some("")),
            "sha256:d52c3d72b73599590471907aa80daa53e3b936a6760599f7e6e443d93dc117a3"
        );
    }

    /// Matcher handling: session_start keeps `""` (present); an event that
    /// forces None omits the key entirely — the canonical JSON differs.
    #[test]
    fn matcher_presence_changes_hash() {
        let with = trusted_hash("x", "session_start", Some(""));
        let without = trusted_hash("x", "user_prompt_submit", None);
        assert_ne!(with, without);
        // Hand-computed canonical strings (documented shape) round-trip:
        assert!(with.starts_with("sha256:"));
        assert!(without.starts_with("sha256:"));
    }

    #[test]
    fn state_key_shape() {
        assert_eq!(
            state_key(r"C:\Users\alice\.codex\hooks.json", "session_start", 0),
            r"C:\Users\alice\.codex\hooks.json:session_start:0:0"
        );
        // A pre-existing user group shifts us to group 1.
        assert_eq!(
            state_key("/home/alice/.codex/hooks.json", "session_start", 1),
            "/home/alice/.codex/hooks.json:session_start:1:0"
        );
    }

    /// Fresh hooks.json: our SessionStart group lands at index 0.
    #[test]
    fn merge_into_empty() {
        let m = merge_hooks_json(None, "cmd X").unwrap();
        assert!(m.changed);
        assert_eq!(m.group_idx, 0);
        let v: serde_json::Value = serde_json::from_str(&m.body).unwrap();
        assert_eq!(v["hooks"]["SessionStart"][0]["matcher"], "");
        assert_eq!(
            v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "cmd X"
        );
        assert_eq!(v["hooks"]["SessionStart"][0]["hooks"][0]["type"], "command");
        // Whitespace-only counts as empty.
        assert!(merge_hooks_json(Some("  \n"), "cmd X").unwrap().changed);
    }

    /// Preservation + INDEX SHIFT: a user's own SessionStart group stays at 0,
    /// ours appends at 1, foreign events untouched.
    #[test]
    fn merge_preserves_and_shifts_index() {
        let user = r#"{
            "hooks": {
                "SessionStart": [
                    {"matcher": "", "hooks": [{"type": "command", "command": "user-thing"}]}
                ],
                "Stop": [
                    {"hooks": [{"type": "command", "command": "user-stop"}]}
                ]
            }
        }"#;
        let m = merge_hooks_json(Some(user), "cmd X").unwrap();
        assert!(m.changed);
        assert_eq!(m.group_idx, 1, "our group must land AFTER the user's");
        let v: serde_json::Value = serde_json::from_str(&m.body).unwrap();
        let ss = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 2);
        assert_eq!(ss[0]["hooks"][0]["command"], "user-thing");
        assert_eq!(ss[1]["hooks"][0]["command"], "cmd X");
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "user-stop");
    }

    /// Idempotence + drift-repair: re-merging our own output is a no-op; a
    /// changed command (tc.exe moved) updates in place at the SAME index.
    #[test]
    fn merge_idempotent_and_repairs_drift() {
        let first = merge_hooks_json(None, "\"C:/old/tc.exe\" __codex-hook SessionStart").unwrap();
        let first_body = first.body.clone();
        let second =
            merge_hooks_json(Some(&first_body), "\"C:/old/tc.exe\" __codex-hook SessionStart")
                .unwrap();
        assert!(!second.changed, "re-merge of identical output must no-op");
        assert_eq!(first.body, second.body);
        // tc.exe moved ⇒ same group index, updated command, changed=true.
        let moved = merge_hooks_json(
            Some(&first.body),
            "\"C:/new/tc.exe\" __codex-hook SessionStart",
        )
        .unwrap();
        assert!(moved.changed);
        assert_eq!(moved.group_idx, first.group_idx);
        let v: serde_json::Value = serde_json::from_str(&moved.body).unwrap();
        assert_eq!(
            v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "\"C:/new/tc.exe\" __codex-hook SessionStart"
        );
    }

    /// DOCTRINE: a user group that carries their own handlers NEXT TO one of
    /// our commands is never replaced (that would delete their handlers) —
    /// ours appends as a fresh group, the trust key follows the appended
    /// index, and a re-run of the result is a no-op (no group pile-up).
    #[test]
    fn merge_never_replaces_mixed_group() {
        let mixed = r#"{
            "hooks": {
                "SessionStart": [
                    {"matcher": "", "hooks": [
                        {"type": "command", "command": "user-thing"},
                        {"type": "command", "command": "old-tc __codex-hook SessionStart"}
                    ]}
                ]
            }
        }"#;
        let m = merge_hooks_json(Some(mixed), "tc.exe __codex-hook SessionStart").unwrap();
        assert!(m.changed);
        assert_eq!(m.group_idx, 1, "ours must append, not replace the mixed group");
        let v: serde_json::Value = serde_json::from_str(&m.body).unwrap();
        let ss = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 2);
        // User's mixed group survives verbatim (both handlers).
        assert_eq!(ss[0]["hooks"][0]["command"], "user-thing");
        assert_eq!(
            ss[0]["hooks"][1]["command"],
            "old-tc __codex-hook SessionStart"
        );
        assert_eq!(ss[1]["hooks"][0]["command"], "tc.exe __codex-hook SessionStart");
        // Idempotent against the merged output: the fully-ours group is
        // found and kept; no third group ever appears.
        let again = merge_hooks_json(Some(&m.body), "tc.exe __codex-hook SessionStart").unwrap();
        assert!(!again.changed);
        assert_eq!(again.group_idx, 1);
        assert_eq!(again.body, m.body);
    }

    /// Refusal: broken/unexpected shapes are never clobbered.
    #[test]
    fn merge_refuses_unmergeable() {
        assert!(merge_hooks_json(Some("{ truncated"), "x").is_err());
        assert!(merge_hooks_json(Some("[1,2,3]"), "x").is_err());
        assert!(merge_hooks_json(Some(r#"{"hooks":"what"}"#), "x").is_err());
        assert!(merge_hooks_json(Some(r#"{"hooks":{"SessionStart":{}}}"#), "x").is_err());
    }

    /// Config trust merge: format-preserving insert into a config that already
    /// has comments, other tables, AND a populated [hooks.state] — the user's
    /// content survives, our key is added, re-run is a no-op.
    #[test]
    fn config_merge_preserves_and_is_idempotent() {
        let user = "\
# my codex config
model = \"gpt-5.5\"

[projects.'C:\\proj']
trust_level = \"trusted\"

[hooks.state]

[hooks.state.'C:\\Users\\alice\\.codex\\hooks.json:stop:0:0']
trusted_hash = \"sha256:deadbeef\"
";
        let key = r"C:\Users\alice\.codex\hooks.json:session_start:0:0";
        let hash = "sha256:abc123";
        let (body, changed) = merge_config_trust(user, key, hash).unwrap();
        assert!(changed);
        // User content intact.
        assert!(body.contains("# my codex config"));
        assert!(body.contains("model = \"gpt-5.5\""));
        assert!(body.contains("trust_level = \"trusted\""));
        assert!(body.contains("sha256:deadbeef"), "existing trust entry kept");
        // Ours present and re-parseable as the same key.
        let doc: toml_edit::DocumentMut = body.parse().unwrap();
        assert_eq!(
            doc["hooks"]["state"][key]["trusted_hash"].as_str(),
            Some(hash)
        );
        // Re-run: no-op.
        let (again, changed2) = merge_config_trust(&body, key, hash).unwrap();
        assert!(!changed2);
        assert_eq!(again, body);
    }

    /// Config trust merge into an EMPTY config (fresh ~/.codex) is valid.
    #[test]
    fn config_merge_into_empty() {
        let (body, changed) = merge_config_trust("", "k:session_start:0:0", "sha256:z").unwrap();
        assert!(changed);
        let doc: toml_edit::DocumentMut = body.parse().unwrap();
        assert_eq!(
            doc["hooks"]["state"]["k:session_start:0:0"]["trusted_hash"].as_str(),
            Some("sha256:z")
        );
    }

    #[test]
    fn config_merge_refuses_broken_toml() {
        assert!(merge_config_trust("this = = broken", "k", "h").is_err());
    }

    #[test]
    fn windows_command_quoting() {
        // Space-free path: bare (unquoted) command — the only form that
        // round-trips Rust's arg-quoting through `cmd /C` (proven live).
        assert_eq!(
            windows_hook_command_for("C:\\Users\\z\\AppData\\Local\\Pulse\\bin\\pulse-ctl.exe")
                .unwrap(),
            "C:\\Users\\z\\AppData\\Local\\Pulse\\bin\\pulse-ctl.exe __codex-hook SessionStart"
        );
        // A space or a quote in the path can't round-trip ⇒ None (the real
        // caller reduces spaced paths to their 8.3 short form first; a quote
        // or an 8.3-disabled volume degrades to birth-correlation).
        assert!(windows_hook_command_for("C:\\Program Files\\Pulse\\pulse-ctl.exe").is_none());
        assert!(windows_hook_command_for("C:\\a\"b\\pulse-ctl.exe").is_none());
    }

    #[test]
    fn beacon_script_shape() {
        assert!(BEACON_SCRIPT.starts_with("#!/bin/sh\n"));
        assert!(!BEACON_SCRIPT.contains('\r'), "LF only");
        assert!(BEACON_SCRIPT.contains(r"printf '\033]7717;tcbeacon;codex;%s;%s;%s\007'"));
        assert!(BEACON_SCRIPT.contains("> /dev/tty"));
        assert!(BEACON_SCRIPT.trim_end().ends_with("exit 0"));
    }
}
