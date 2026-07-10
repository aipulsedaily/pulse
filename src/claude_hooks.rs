//! Attribution Layer 3: the per-host opt-in claude-session beacon for ssh
//! terminals. With the user's consent (a one-time popup per host), TC
//! installs two things on the remote host over the EXISTING sftp transport:
//!
//!   1. `~/.tc/claude-hook.sh` — a ~15-line POSIX beacon script that prints
//!      `ESC ] 7717;tcbeacon;<event>;<source>;<session-id> BEL` to /dev/tty
//!      (fail-silent, always exit 0 — claude must never see a failing hook);
//!   2. SessionStart/SessionEnd hook entries MERGED into the remote
//!      `~/.claude/settings.json` — non-destructively (user hooks preserved,
//!      invalid JSON refused rather than clobbered), idempotently (re-runs
//!      detect our entry), and atomically (put to a temp name + posix-rename
//!      over the original).
//!
//! The beacon then rides the session's own pty into the terminal's journal,
//! where the BlockScanner's `tcbeacon` verb hands it to `Core::on_beacon`
//! (advisory-trust gates) — exact conversation restore for remote claudes,
//! /clear and in-TUI /resume included, with zero probes.
//!
//! SANCTIONED REMOTE WRITES: `~/.tc/claude-hook.sh` (+ the `~/.tc` mkdir)
//! and the consented `~/.claude/settings.json` merge are — alongside
//! ssh-drop's `~/.tc-drops` and the codex mirror's `~/.tc/codex-hook.sh` +
//! `~/.codex/config.toml [hooks.state]` installs (task #30, see
//! `codex_hooks.rs`) — the only remote writes TC ever performs, all strictly
//! behind their per-host consent dialogs (Prefs.claude_hook_hosts /
//! codex_hook_hosts).

use std::time::Duration;

/// The hook command written into the remote settings.json. `~` expands in
/// the bash -c claude wraps hook commands in; the event name rides argv so
/// the script works even if a future claude drops hook_event_name from
/// stdin.
pub const REMOTE_HOOK_CMD_BASE: &str = "~/.tc/claude-hook.sh";

/// The beacon script body (LF endings; POSIX sh only — the remote login
/// shell is unknown). `sed` extraction is deliberately lax: the stdin JSON
/// is single-line with unique keys, and every value we pull (uuid, claude
/// enum words) is quote-free.
pub const BEACON_SCRIPT: &str = r#"#!/bin/sh
# Pulse claude-session beacon.
# Installed with your consent by Pulse; safe to delete (sessions
# then fall back to probe-based correlation). Reports the live claude
# session id to the hosting terminal via a private OSC on /dev/tty.
# Always silent, always exit 0 - claude must never see a failing hook.
ev="${1:-SessionStart}"
in=$(cat 2>/dev/null || true)
sid="${CLAUDE_CODE_SESSION_ID:-}"
if [ -z "$sid" ]; then
  sid=$(printf '%s' "$in" | sed -n 's/.*"session_id"[^"]*"\([^"]*\)".*/\1/p' | head -n 1)
fi
src=$(printf '%s' "$in" | sed -n 's/.*"source"[^"]*"\([^"]*\)".*/\1/p' | head -n 1)
if [ -z "$src" ]; then
  src=$(printf '%s' "$in" | sed -n 's/.*"reason"[^"]*"\([^"]*\)".*/\1/p' | head -n 1)
fi
cwd=$(printf '%s' "$in" | sed -n 's/.*"cwd"[^"]*"\([^"]*\)".*/\1/p' | head -n 1)
cwdhex=$(printf '%s' "$cwd" | od -An -v -tx1 2>/dev/null | tr -d ' \n')
[ -n "$sid" ] || exit 0
{ printf '\033]7717;tcbeacon;%s;%s;%s;%s\007' "$ev" "${src:-unknown}" "$sid" "$cwdhex" > /dev/tty; } 2>/dev/null || true
exit 0
"#;

/// Merge our hook entries into a remote settings.json body. Returns the
/// merged pretty JSON + whether anything changed (false = already
/// installed, idempotent re-run). `existing` = the fetched file, None when
/// the host has no settings.json yet. Err = the existing file isn't a JSON
/// object — REFUSED, never clobbered (the user's file is broken; overwriting
/// hides that from them and destroys whatever they meant).
pub fn merge_settings(existing: Option<&str>) -> Result<(String, bool), String> {
    let mut root: serde_json::Value = match existing {
        None => serde_json::json!({}),
        Some(s) if s.trim().is_empty() => serde_json::json!({}),
        Some(s) => serde_json::from_str(s)
            .map_err(|e| format!("remote settings.json is not valid JSON ({e}); not touching it"))?,
    };
    if !root.is_object() {
        return Err("remote settings.json is not a JSON object; not touching it".into());
    }
    let hooks = root
        .as_object_mut()
        .expect("checked object")
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        return Err("remote settings.json has a non-object \"hooks\"; not touching it".into());
    }
    let mut changed = false;
    for event in ["SessionStart", "SessionEnd"] {
        let arr = hooks
            .as_object_mut()
            .expect("checked object")
            .entry(event)
            .or_insert_with(|| serde_json::json!([]));
        let Some(list) = arr.as_array_mut() else {
            return Err(format!(
                "remote settings.json has a non-array hooks.{event}; not touching it"
            ));
        };
        let ours = |v: &serde_json::Value| {
            v["hooks"].as_array().is_some_and(|inner| {
                inner.iter().any(|h| {
                    h["command"]
                        .as_str()
                        .is_some_and(|c| c.contains(".tc/claude-hook.sh"))
                })
            })
        };
        if list.iter().any(ours) {
            continue; // idempotent: our entry is already there
        }
        list.push(serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": format!("{REMOTE_HOOK_CMD_BASE} {event}"),
            }]
        }));
        changed = true;
    }
    let body = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    Ok((body, changed))
}

/// Install result, for the toast copy (naming aligned with
/// `codex_hooks::Outcome`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Script uploaded + settings.json gained our hooks.
    Installed,
    /// Script (re)uploaded; settings.json already carried our hooks.
    AlreadyInstalled,
}

const INSTALL_DEADLINE: Duration = Duration::from_secs(30);

fn run_batch(
    sftp: &std::path::Path,
    meta_args: &[String],
    tag: &str,
    body: &str,
) -> Result<std::process::Output, String> {
    crate::ssh_transport::run_install_batch(sftp, meta_args, tag, body, INSTALL_DEADLINE)
}

/// Install the beacon on one host, over the terminal's own transport
/// identity (`meta_args` = the persisted ssh argv; `program` = the ssh exe,
/// for sibling-sftp resolution). Blocking — run on a worker thread.
///
/// Three bounded sftp connections: fetch settings.json (missing file ok) →
/// merge locally → upload script + merged settings (temp name +
/// posix-rename, atomic on OpenSSH servers).
pub fn install_remote(program: &str, meta_args: &[String]) -> Result<Outcome, String> {
    use crate::ssh_transport::fwd;
    let sftp = crate::ssh_transport::resolve_sftp(program)
        .map_err(|looked| format!("no sftp.exe (looked in {looked})"))?;
    let dir = crate::state::data_dir().join("tmp");
    let _ = std::fs::create_dir_all(&dir);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let local_settings = dir.join(format!("claude-hook-fetch-{nonce}.json"));
    let local_script = dir.join(format!("claude-hook-script-{nonce}.sh"));
    let local_merged = dir.join(format!("claude-hook-merged-{nonce}.json"));
    let cleanup = || {
        let _ = std::fs::remove_file(&local_settings);
        let _ = std::fs::remove_file(&local_script);
        let _ = std::fs::remove_file(&local_merged);
    };

    // 1) Fetch the existing settings.json. `-get` (leading dash) keeps the
    //    batch alive when the file simply doesn't exist; a transport-class
    //    failure still exits non-zero and is classified honestly. The paired
    //    `-ls -l` makes absence PROVABLE and the fetched size CHECKABLE
    //    (R4-F3): a fetch that failed for any reason other than "not found"
    //    (root-owned file, dangling symlink, mid-transfer death leaving a
    //    partial local file) must refuse, or the merge would run against an
    //    empty/truncated body and atomically replace the user's real file.
    const REMOTE_SETTINGS: &str = ".claude/settings.json";
    let _ = std::fs::remove_file(&local_settings);
    let fetch = format!(
        "-ls -l \"{REMOTE_SETTINGS}\"\n-get \"{REMOTE_SETTINGS}\" \"{}\"\n",
        fwd(&local_settings)
    );
    let out = run_batch(&sftp, meta_args, "claude-hook-fetch", &fetch).inspect_err(|_| cleanup())?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        cleanup();
        return Err(format!(
            "connection failed: {:?}",
            crate::ssh_transport::classify_conn(&stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let existing = match std::fs::read_to_string(&local_settings) {
        Ok(s) => {
            // R4-F3: a partial fetch must not masquerade as the whole file
            // (truncated JSON virtually always refuses at parse, but the
            // size proof closes the parseable-prefix window for free).
            if !crate::ssh_transport::fetched_len_matches(REMOTE_SETTINGS, &stdout, s.len() as u64)
            {
                cleanup();
                return Err(format!(
                    "remote ~/{REMOTE_SETTINGS} was only partially fetched; not touching it"
                ));
            }
            Some(s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !crate::ssh_transport::remote_file_absent(REMOTE_SETTINGS, &stdout, &stderr) {
                cleanup();
                return Err(format!(
                    "remote ~/{REMOTE_SETTINGS} exists but could not be fetched; not touching it"
                ));
            }
            None
        }
        Err(e) => {
            cleanup();
            return Err(format!("read fetched settings.json: {e}"));
        }
    };

    // 2) Merge locally (refuses to touch invalid/unexpected shapes).
    let (merged, changed) = match merge_settings(existing.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            cleanup();
            return Err(e);
        }
    };

    // 3) Upload: the script always (heals a deleted/stale script), the
    //    merged settings only when the merge added something. Temp name +
    //    rename keeps a mid-upload death from ever truncating the user's
    //    settings; the stale-temp `-rm`s make re-runs self-healing.
    if let Err(e) = std::fs::write(&local_script, BEACON_SCRIPT) {
        cleanup();
        return Err(format!("local script write: {e}"));
    }
    let mut batch = String::new();
    batch.push_str("-mkdir .tc\n");
    batch.push_str("-rm .tc/claude-hook.sh.tc-new\n");
    batch.push_str(&format!(
        "put \"{}\" .tc/claude-hook.sh.tc-new\n",
        fwd(&local_script)
    ));
    batch.push_str("rename .tc/claude-hook.sh.tc-new .tc/claude-hook.sh\n");
    batch.push_str("chmod 755 .tc/claude-hook.sh\n");
    if changed {
        if let Err(e) = std::fs::write(&local_merged, &merged) {
            cleanup();
            return Err(format!("local merge write: {e}"));
        }
        batch.push_str("-mkdir .claude\n");
        batch.push_str("-rm .claude/settings.json.tc-new\n");
        batch.push_str(&format!(
            "put \"{}\" .claude/settings.json.tc-new\n",
            fwd(&local_merged)
        ));
        batch.push_str("rename .claude/settings.json.tc-new .claude/settings.json\n");
    }
    let out = run_batch(&sftp, meta_args, "claude-hook-put", &batch).inspect_err(|_| cleanup())?;
    cleanup();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "install failed: {:?}",
            crate::ssh_transport::classify_conn(&stderr)
        ));
    }
    Ok(if changed {
        Outcome::Installed
    } else {
        Outcome::AlreadyInstalled
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh host (no settings.json): both events land, valid pretty JSON.
    #[test]
    fn merge_into_empty() {
        let (body, changed) = merge_settings(None).unwrap();
        assert!(changed);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        for event in ["SessionStart", "SessionEnd"] {
            let cmd = v["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(cmd, format!("~/.tc/claude-hook.sh {event}"));
            assert_eq!(v["hooks"][event][0]["hooks"][0]["type"], "command");
        }
        // Whitespace-only file counts as empty, not invalid.
        let (_, changed) = merge_settings(Some("  \n")).unwrap();
        assert!(changed);
    }

    /// PRESERVATION golden: user settings + user hooks survive byte-exact
    /// (values compared post-parse; key content untouched), our entries
    /// APPEND after theirs.
    #[test]
    fn merge_preserves_user_content() {
        let user = r#"{
            "model": "opus",
            "permissions": {"defaultMode": "acceptEdits"},
            "hooks": {
                "SessionStart": [
                    {"matcher": "startup", "hooks": [{"type": "command", "command": "echo hi"}]}
                ],
                "PreToolUse": [
                    {"hooks": [{"type": "command", "command": "/usr/local/bin/lint"}]}
                ]
            }
        }"#;
        let (body, changed) = merge_settings(Some(user)).unwrap();
        assert!(changed);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(v["permissions"]["defaultMode"], "acceptEdits");
        // User's SessionStart hook still first; ours appended.
        let ss = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 2);
        assert_eq!(ss[0]["hooks"][0]["command"], "echo hi");
        assert_eq!(
            ss[1]["hooks"][0]["command"],
            "~/.tc/claude-hook.sh SessionStart"
        );
        // Foreign hook classes untouched.
        assert_eq!(
            v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "/usr/local/bin/lint"
        );
        // SessionEnd created fresh.
        assert_eq!(
            v["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
            "~/.tc/claude-hook.sh SessionEnd"
        );
    }

    /// IDEMPOTENCE golden: merging our own output changes nothing — and a
    /// half-installed file (one event present) only gains the missing one.
    #[test]
    fn merge_is_idempotent() {
        let (first, changed) = merge_settings(None).unwrap();
        assert!(changed);
        let (second, changed) = merge_settings(Some(&first)).unwrap();
        assert!(!changed, "re-merge must be a no-op");
        assert_eq!(first, second);
        // Half-installed: SessionStart there, SessionEnd missing.
        let half = r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"~/.tc/claude-hook.sh SessionStart"}]}]}}"#;
        let (body, changed) = merge_settings(Some(half)).unwrap();
        assert!(changed);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(v["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);
    }

    /// REFUSAL golden: invalid JSON / non-object shapes are never clobbered.
    #[test]
    fn merge_refuses_unmergeable() {
        assert!(merge_settings(Some("{ truncated")).is_err());
        assert!(merge_settings(Some("[1,2,3]")).is_err());
        assert!(merge_settings(Some(r#"{"hooks": "what"}"#)).is_err());
        assert!(merge_settings(Some(r#"{"hooks": {"SessionStart": {}}}"#)).is_err());
    }

    /// The beacon script's load-bearing properties: POSIX sh (no bashisms
    /// in the shebang), the exact OSC shape the BlockScanner parses (F1
    /// beacon v2: 4th payload field = hex(cwd), hex via `od` so `;`/BEL/ESC
    /// in a path can never split or terminate the OSC), LF endings,
    /// fail-silent exit 0 everywhere.
    #[test]
    fn beacon_script_shape() {
        assert!(BEACON_SCRIPT.starts_with("#!/bin/sh\n"));
        assert!(!BEACON_SCRIPT.contains('\r'), "LF only — sh rejects CRLF");
        assert!(BEACON_SCRIPT
            .contains(r"printf '\033]7717;tcbeacon;%s;%s;%s;%s\007'"));
        assert!(BEACON_SCRIPT.contains("> /dev/tty"));
        assert!(BEACON_SCRIPT.trim_end().ends_with("exit 0"));
        assert!(BEACON_SCRIPT.contains(r#"[ -n "$sid" ] || exit 0"#));
        // v2 cwd leg: extracted from the hook stdin JSON, hex-encoded with
        // POSIX od (busybox-safe flags), errors swallowed (a cwd-less or
        // od-less host degrades to an empty field = legacy 3-field behavior
        // at the parser).
        assert!(BEACON_SCRIPT.contains(r#"sed -n 's/.*"cwd"[^"]*"\([^"]*\)".*/\1/p'"#));
        assert!(BEACON_SCRIPT.contains(r"od -An -v -tx1 2>/dev/null | tr -d ' \n'"));
    }
}
