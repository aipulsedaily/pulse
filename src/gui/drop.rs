//! Local drag-drop → quoted-path insertion (QOL §4): the pure half.
//!
//! Everything here is side-effect-free and golden-tested — translation,
//! quoting, routing verdicts, and the hover-label text. `App::route_file_drop`
//! (mod.rs) owns the impure half: selected-terminal resolution, the composer
//! draft vs PTY landing, and the refusal bookkeeping.
//!
//! Contract with ssh-drop (#26): `bash_single_quote` is the SHARED posix
//! quoting helper (its paste-after-success builds `'{home}/.tc-drops/{name}'`
//! with it), and the Ssh arm in `route_file_drop` is the exactly-one seam the
//! upload pipeline replaces. Nothing else may branch on Ssh.

use std::path::{Path, PathBuf};

use crate::state::ShellFamily;

/// The translation/quoting family a drop targets. Derived from `ShellFamily`
/// (never persisted); owning the distro String keeps this free of borrows so
/// the router can hold it across `&mut self` calls.
#[derive(Debug, Clone, PartialEq)]
pub enum DropFamily {
    Pwsh,
    Cmd,
    Wsl { distro: Option<String> },
    /// ssh: drops UPLOAD (#26) — `host` = destination verbatim, for the
    /// hover label and the upload pipeline's consent/toast copy.
    Ssh { host: String },
    /// claude-kind, Custom, hookless — WT-style bare-or-quoted.
    Other,
}

pub fn drop_family(f: &ShellFamily) -> DropFamily {
    match f {
        ShellFamily::Pwsh => DropFamily::Pwsh,
        ShellFamily::Cmd => DropFamily::Cmd,
        ShellFamily::WslShell { distro } => DropFamily::Wsl {
            distro: distro.clone(),
        },
        ShellFamily::Ssh { host } => DropFamily::Ssh { host: host.clone() },
        ShellFamily::Other => DropFamily::Other,
    }
}

/// Where a drop (or routed paste) lands (§4.3). `Refuse` inserts nothing —
/// the hover label already explained why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteVerdict {
    /// Armed composer: append to the draft (pointer act — episode untouched).
    Draft,
    /// Paste-semantics bytes to the PTY (real input, on_raw_input fires).
    Pty,
    /// ssh (#26): upload to the remote, then paste the remote paths. The
    /// consent gate + queue live in mod.rs's one Ssh arm.
    SshUpload,
    /// A not-running terminal (sleep inv. 5: nothing wakes).
    Refuse,
}

pub fn route_verdict(fam: &DropFamily, compose: bool, running: bool) -> RouteVerdict {
    if !running {
        return RouteVerdict::Refuse;
    }
    if matches!(fam, DropFamily::Ssh { .. }) {
        return RouteVerdict::SshUpload;
    }
    if compose {
        RouteVerdict::Draft
    } else {
        RouteVerdict::Pty
    }
}

/// POSIX single-quote: `'` → `'\''`, everything else literal — the only inert
/// quoting form across bash/zsh/fish/dash. SHARED with ssh-drop (#26): its
/// paste-after-success quotes remote paths with exactly this.
pub fn bash_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// PowerShell: single quotes are the only inert form (`"` interpolates `$`,
/// backtick escapes); internal `'` doubles. Shared with Tab completion
/// (#24 — complete.rs quotes completed tokens with exactly these helpers).
pub(crate) fn pwsh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// cmd.exe: wrap in `"…"` only when the path carries cmd-special characters;
/// `"` is illegal in Windows paths so no escaping is ever needed.
pub(crate) fn cmd_quote(s: &str) -> String {
    const SPECIAL: &[char] = &[
        ' ', '\t', '&', '^', '%', '(', ')', ';', ',', '=', '!', '\'', '+', '`', '~', '[', ']',
        '{', '}',
    ];
    if s.contains(SPECIAL) {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

/// WT-style for arbitrary CLIs (claude tokenizes both): bare when the path is
/// plain `[A-Za-z0-9_\-.:\\/]+`, else `"…"`.
pub(crate) fn other_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '\\' | '/')
        });
    if plain {
        s.to_string()
    } else {
        format!("\"{s}\"")
    }
}

/// Windows path → in-distro POSIX path for a WSL terminal (§4.4):
///  • `C:\…` → `/mnt/c/…` (the existing golden-tested `wsl_mnt_path`);
///  • `\\wsl.localhost\<distro>\rest` / `\\wsl$\<distro>\rest` — distro
///    matched case-insensitively against THIS terminal's distro — → `/rest`;
///  • anything else (other UNC, mismatched/unknown distro, relative) → None:
///    skipped + counted, never silently mangled (never-guess).
pub fn translate_wsl(path: &str, distro: Option<&str>) -> Option<String> {
    if let Some(m) = crate::daemon::bootstrap::wsl_mnt_path(Path::new(path)) {
        // Root form "C:\" yields a trailing slash; tidy it.
        let t = m.trim_end_matches('/');
        return Some(if t.is_empty() { "/".into() } else { t.to_string() });
    }
    // UNC into the SAME distro. A default-distro terminal (distro None) has
    // no name to match against — untranslatable (refuse over guess).
    let d = distro?;
    let lower = path.to_ascii_lowercase();
    let dl = d.to_ascii_lowercase();
    for prefix in [
        format!("\\\\wsl.localhost\\{dl}\\"),
        format!("\\\\wsl$\\{dl}\\"),
    ] {
        if lower.starts_with(&prefix) {
            let rest = &path[prefix.len()..];
            return Some(format!("/{}", rest.replace('\\', "/")));
        }
    }
    None
}

/// Translate + quote ONE dropped path for a family. None ⇒ untranslatable
/// (WSL-only today) or refused family — the caller skips and counts it.
pub fn quote_path(path: &Path, fam: &DropFamily) -> Option<String> {
    let s = path.to_string_lossy();
    match fam {
        DropFamily::Pwsh => Some(pwsh_quote(&s)),
        DropFamily::Cmd => Some(cmd_quote(&s)),
        DropFamily::Wsl { distro } => {
            translate_wsl(&s, distro.as_deref()).map(|p| bash_single_quote(&p))
        }
        DropFamily::Ssh { .. } => None, // uploads, never inserts local paths (#26); belt here
        DropFamily::Other => Some(other_quote(&s)),
    }
}

/// Multi-file insertion text (D7): space-separated, ONE trailing space so the
/// user keeps typing. None when nothing translated (all skipped).
pub fn build_insert(paths: &[PathBuf], fam: &DropFamily) -> Option<String> {
    let quoted: Vec<String> = paths.iter().filter_map(|p| quote_path(p, fam)).collect();
    if quoted.is_empty() {
        return None;
    }
    Some(format!("{} ", quoted.join(" ")))
}

/// §4.7 hover-label text — the drop preview AND the refusal surface, decided
/// per the state table. `n` = hovered-file count (hover events may omit
/// paths; `paths` holds the known ones for the WSL translatability count).
pub fn hover_label(
    fam: &DropFamily,
    compose: bool,
    running: bool,
    paths: &[PathBuf],
    n: usize,
) -> String {
    if !running {
        return "terminal is not running".into();
    }
    if let DropFamily::Ssh { host } = fam {
        // #26: the drop preview IS the upload promise.
        return format!(
            "upload to {host} \u{2014} {n} file{}",
            if n == 1 { "" } else { "s" }
        );
    }
    if let DropFamily::Wsl { distro } = fam {
        let translatable = paths
            .iter()
            .filter(|p| translate_wsl(&p.to_string_lossy(), distro.as_deref()).is_some())
            .count();
        if !paths.is_empty() && translatable < paths.len() {
            return format!("{translatable} of {} translate to /mnt/\u{2026}", paths.len());
        }
        return if n > 1 {
            "insert as /mnt/\u{2026} paths".into()
        } else {
            "insert as /mnt/\u{2026} path".into()
        };
    }
    if compose {
        return if n > 1 {
            format!("add to command \u{2014} {n} files")
        } else {
            "add to command".into()
        };
    }
    if n > 1 {
        format!("insert {n} paths")
    } else {
        "insert path".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wsl(distro: Option<&str>) -> DropFamily {
        DropFamily::Wsl {
            distro: distro.map(str::to_string),
        }
    }

    // ── quoting goldens per family (§4.4) ───────────────────────────────

    #[test]
    fn pwsh_always_single_quotes_and_doubles_internal_quotes() {
        assert_eq!(
            quote_path(Path::new(r"C:\Users\alice\shot.png"), &DropFamily::Pwsh).unwrap(),
            r"'C:\Users\alice\shot.png'"
        );
        assert_eq!(
            quote_path(Path::new(r"C:\it's here\a.png"), &DropFamily::Pwsh).unwrap(),
            r"'C:\it''s here\a.png'"
        );
    }

    #[test]
    fn cmd_quotes_only_when_special() {
        assert_eq!(
            quote_path(Path::new(r"C:\tools\x.exe"), &DropFamily::Cmd).unwrap(),
            r"C:\tools\x.exe"
        );
        assert_eq!(
            quote_path(Path::new(r"C:\Program Files (x86)\a.txt"), &DropFamily::Cmd).unwrap(),
            "\"C:\\Program Files (x86)\\a.txt\""
        );
        assert_eq!(
            quote_path(Path::new(r"C:\a&b\c.txt"), &DropFamily::Cmd).unwrap(),
            "\"C:\\a&b\\c.txt\""
        );
    }

    #[test]
    fn bash_quote_escapes_internal_quote_the_posix_way() {
        assert_eq!(bash_single_quote("/tmp/it's"), r"'/tmp/it'\''s'");
        assert_eq!(bash_single_quote("/plain"), "'/plain'");
    }

    #[test]
    fn other_family_bare_vs_quoted() {
        assert_eq!(
            quote_path(Path::new(r"C:\Users\alice\shot.png"), &DropFamily::Other).unwrap(),
            r"C:\Users\alice\shot.png"
        );
        assert_eq!(
            quote_path(Path::new(r"C:\My Shots\shot 1.png"), &DropFamily::Other).unwrap(),
            "\"C:\\My Shots\\shot 1.png\""
        );
    }

    // ── WSL translation (§4.4) ──────────────────────────────────────────

    #[test]
    fn wsl_drive_paths_translate_to_mnt() {
        assert_eq!(
            translate_wsl(r"C:\Users\alice\shot.png", Some("Ubuntu-24.04")).unwrap(),
            "/mnt/c/Users/alice/shot.png"
        );
        // Default-distro terminals still translate drive paths.
        assert_eq!(translate_wsl(r"D:\x y\z", None).unwrap(), "/mnt/d/x y/z");
        assert_eq!(translate_wsl(r"C:\", None).unwrap(), "/mnt/c");
    }

    #[test]
    fn wsl_unc_translates_only_for_matching_distro() {
        assert_eq!(
            translate_wsl(r"\\wsl.localhost\Ubuntu-24.04\home\z\f.png", Some("ubuntu-24.04"))
                .unwrap(),
            "/home/z/f.png"
        );
        assert_eq!(
            translate_wsl(r"\\wsl$\Ubuntu-24.04\tmp\f", Some("Ubuntu-24.04")).unwrap(),
            "/tmp/f"
        );
        // Mismatched distro / unknown default / foreign UNC ⇒ None.
        assert_eq!(translate_wsl(r"\\wsl$\Debian\tmp\f", Some("Ubuntu-24.04")), None);
        assert_eq!(translate_wsl(r"\\wsl$\Debian\tmp\f", None), None);
        assert_eq!(translate_wsl(r"\\server\share\f.png", Some("Ubuntu-24.04")), None);
        assert_eq!(translate_wsl("relative\\path", Some("Ubuntu-24.04")), None);
    }

    #[test]
    fn wsl_quote_wraps_translated_path_with_posix_quotes() {
        assert_eq!(
            quote_path(Path::new(r"C:\it's\a b.png"), &wsl(Some("Ubuntu"))).unwrap(),
            r"'/mnt/c/it'\''s/a b.png'"
        );
        assert_eq!(quote_path(Path::new(r"\\server\x"), &wsl(Some("Ubuntu"))), None);
    }

    // ── multi-file join (D7) + mixed-translatability skip ───────────────

    #[test]
    fn build_insert_joins_with_one_trailing_space() {
        let paths = vec![PathBuf::from(r"C:\a.png"), PathBuf::from(r"C:\b c.png")];
        assert_eq!(
            build_insert(&paths, &DropFamily::Pwsh).unwrap(),
            r"'C:\a.png' 'C:\b c.png' "
        );
    }

    #[test]
    fn build_insert_skips_untranslatable_and_refuses_when_all_skip() {
        let paths = vec![
            PathBuf::from(r"C:\ok.png"),
            PathBuf::from(r"\\server\no.png"),
        ];
        assert_eq!(
            build_insert(&paths, &wsl(Some("Ubuntu"))).unwrap(),
            "'/mnt/c/ok.png' "
        );
        assert_eq!(
            build_insert(&[PathBuf::from(r"\\server\no.png")], &wsl(Some("Ubuntu"))),
            None
        );
    }

    // ── router table (§4.3): family × mode × status ─────────────────────

    #[test]
    fn route_verdict_table() {
        use RouteVerdict::*;
        let ssh = DropFamily::Ssh { host: "h".into() };
        let fams = [
            (DropFamily::Pwsh, false),
            (DropFamily::Cmd, false),
            (wsl(Some("U")), false),
            (DropFamily::Other, false),
            (ssh, true),
        ];
        for (fam, is_ssh) in fams {
            // Not running ⇒ always refuse (sleep inv. 5: nothing wakes).
            assert_eq!(route_verdict(&fam, false, false), Refuse);
            assert_eq!(route_verdict(&fam, true, false), Refuse);
            if is_ssh {
                // The one #26 seam: uploads regardless of composer mode
                // (the paste routes by mode at COMPLETION time, not now).
                assert_eq!(route_verdict(&fam, true, true), SshUpload);
                assert_eq!(route_verdict(&fam, false, true), SshUpload);
            } else {
                assert_eq!(route_verdict(&fam, true, true), Draft);
                assert_eq!(route_verdict(&fam, false, true), Pty);
            }
        }
    }

    // ── §4.7 label states ────────────────────────────────────────────────

    #[test]
    fn hover_label_state_table() {
        let one = vec![PathBuf::from(r"C:\a.png")];
        let two = vec![PathBuf::from(r"C:\a.png"), PathBuf::from(r"C:\b.png")];
        let mixed = vec![PathBuf::from(r"C:\a.png"), PathBuf::from(r"\\server\b.png")];
        assert_eq!(
            hover_label(&DropFamily::Pwsh, true, true, &two, 2),
            "add to command \u{2014} 2 files"
        );
        assert_eq!(hover_label(&DropFamily::Pwsh, true, true, &one, 1), "add to command");
        assert_eq!(hover_label(&DropFamily::Other, false, true, &one, 1), "insert path");
        assert_eq!(hover_label(&DropFamily::Other, false, true, &two, 2), "insert 2 paths");
        assert_eq!(
            hover_label(&wsl(Some("U")), false, true, &two, 2),
            "insert as /mnt/\u{2026} paths"
        );
        assert_eq!(
            hover_label(&wsl(Some("U")), false, true, &mixed, 2),
            "1 of 2 translate to /mnt/\u{2026}"
        );
        // #26: the drop preview announces the upload, host + count.
        let ssh = DropFamily::Ssh { host: "192.0.2.14".into() };
        assert_eq!(
            hover_label(&ssh, false, true, &one, 1),
            "upload to 192.0.2.14 \u{2014} 1 file"
        );
        assert_eq!(
            hover_label(&ssh, true, true, &two, 2),
            "upload to 192.0.2.14 \u{2014} 2 files"
        );
        assert_eq!(
            hover_label(&DropFamily::Pwsh, false, false, &one, 1),
            "terminal is not running"
        );
    }
}
