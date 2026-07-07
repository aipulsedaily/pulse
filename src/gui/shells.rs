//! Shell catalog for the launcher palette (P6 §9): enumerators that turn the
//! machine's installed shells into `ShellChoice` rows. Enumeration happens on
//! dialog open only (registry read + one ssh-config file read; never a
//! process spawn, never polled — P6 inv. 7/D16: `wsl -l` emits localized
//! UTF-16LE and costs a wsl.exe launch; the Lxss registry is structured,
//! instant, and works while WSL is stopped).

use std::path::{Path, PathBuf};

use super::launcher::ShellChoice;

/// One installed WSL distribution, from the Lxss registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslDistro {
    pub name: String,
    pub is_default: bool,
}

/// Raw per-distro registry values, decoupled from winreg for fixture tests
/// (§12 U5): (DistributionName, State) per Lxss subkey.
type LxssEntry = (Option<String>, Option<u32>);

/// Pure classification of Lxss registry data: keep State==1 (installed;
/// installing/uninstalling are transient), skip Docker Desktop's utility
/// distros (busybox worlds without bash — a shell row for them would only
/// ever produce a fast-exit warning), flag the default. `default_guid` is the
/// Lxss root's DefaultDistribution value; `entries` carry their subkey GUID.
fn distros_from(entries: &[(String, LxssEntry)], default_guid: Option<&str>) -> Vec<WslDistro> {
    let mut out = Vec::new();
    for (guid, (name, state)) in entries {
        let Some(name) = name else { continue };
        if state.unwrap_or(0) != 1 {
            continue;
        }
        if name.to_ascii_lowercase().starts_with("docker-desktop") {
            continue;
        }
        out.push(WslDistro {
            name: name.clone(),
            is_default: default_guid.is_some_and(|d| d.eq_ignore_ascii_case(guid)),
        });
    }
    // Default distro first, then registry order — the row a user most likely
    // wants sits on top of the WSL cluster.
    out.sort_by_key(|d| !d.is_default);
    out
}

/// Installed WSL distros from HKCU\...\Lxss. Registry absent (WSL never
/// installed) ⇒ empty — no WSL rows, no error chrome.
pub fn wsl_distros() -> Vec<WslDistro> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let Ok(lxss) = hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Lxss") else {
        return Vec::new();
    };
    let default_guid: Option<String> = lxss.get_value("DefaultDistribution").ok();
    let mut entries: Vec<(String, LxssEntry)> = Vec::new();
    for key in lxss.enum_keys().flatten() {
        let Ok(sub) = lxss.open_subkey(&key) else { continue };
        let name: Option<String> = sub.get_value("DistributionName").ok();
        let state: Option<u32> = sub.get_value("State").ok();
        entries.push((key, (name, state)));
    }
    distros_from(&entries, default_guid.as_deref())
}

/// Pure ssh-config Host extraction (P6c §9/D16, fixture-tested §12 U5):
/// `Host` lines contribute each pattern token that is a CONCRETE host —
/// wildcard/negated patterns (`*?!`) are config machinery, not launchable
/// destinations. `Match` blocks and every other keyword are ignored. One
/// level of `Include` is expanded through `read_include` (paths resolve
/// relative to ~/.ssh; nested Includes inside included files are NOT
/// followed — one level per spec). Order preserved, first occurrence wins.
fn hosts_from_config(
    text: &str,
    read_include: &mut dyn FnMut(&str) -> Vec<String>,
    depth: u32,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Keyword arguments split on whitespace or '=' (ssh_config grammar;
        // "Key = value" leaves a dangling '=' in the remainder — strip it).
        let (kw, rest) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, r)) => (k, r.trim().trim_start_matches('=').trim()),
            None => continue,
        };
        if kw.eq_ignore_ascii_case("host") {
            for tok in rest.split_whitespace() {
                let tok = tok.trim_matches('"');
                if tok.is_empty() || tok.contains(['*', '?', '!']) {
                    continue;
                }
                if !out.iter().any(|h| h == tok) {
                    out.push(tok.to_string());
                }
            }
        } else if kw.eq_ignore_ascii_case("include") && depth == 0 {
            for inc in rest.split_whitespace() {
                let inc = inc.trim_matches('"');
                for body in read_include(inc) {
                    for h in hosts_from_config(&body, &mut |_| Vec::new(), 1) {
                        if !out.contains(&h) {
                            out.push(h);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Resolve one `Include` value against ~/.ssh: absolute paths verbatim,
/// relative ones under the ssh dir; a single trailing `*` component globs via
/// one read_dir (the common `Include config.d/*` shape). Unreadable ⇒ empty.
fn read_include_files(ssh_dir: &Path, value: &str) -> Vec<String> {
    let expand = |p: PathBuf| -> Vec<PathBuf> {
        let s = p.to_string_lossy().into_owned();
        match s.strip_suffix('*') {
            Some(prefix) if !prefix.contains('*') => {
                let dir = Path::new(prefix)
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                let stem = Path::new(prefix)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| e.path())
                            .filter(|f| {
                                f.is_file()
                                    && f.file_name()
                                        .is_some_and(|n| n.to_string_lossy().starts_with(&stem))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                files.sort();
                files
            }
            _ if s.contains(['*', '?']) => Vec::new(), // richer globs: skip
            _ => vec![p],
        }
    };
    let base = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        ssh_dir.join(value)
    };
    expand(base)
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect()
}

/// Concrete hosts from %USERPROFILE%\.ssh\config (+ one Include level).
/// Missing file ⇒ empty — the launcher still offers the freeform "ssh to…"
/// row.
pub fn ssh_hosts() -> Vec<String> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let ssh_dir = home.join(".ssh");
    let Ok(text) = std::fs::read_to_string(ssh_dir.join("config")) else {
        return Vec::new();
    };
    hosts_from_config(&text, &mut |v| read_include_files(&ssh_dir, v), 0)
}

/// The launcher's SHELLS section (P6a/P6c): PowerShell + cmd (the pre-P6
/// set), one row per installed WSL distro, then one row per concrete
/// ~/.ssh/config host. Called at launcher open, cached for the palette's
/// lifetime by LauncherState.
pub fn shell_choices() -> Vec<ShellChoice> {
    let mut out = vec![
        ShellChoice {
            kind_tag: "powershell".into(),
            label: "PowerShell".into(),
            detail: String::new(),
        },
        ShellChoice {
            kind_tag: "cmd".into(),
            label: "cmd".into(),
            // P6b degraded note (§13, stated honestly): cmd's PROMPT hooks
            // carry no exec and cannot render ERRORLEVEL, so records show
            // duration + cwd but never exit codes.
            detail: "no exit codes".into(),
        },
    ];
    for d in wsl_distros() {
        out.push(ShellChoice {
            kind_tag: format!("wsl:{}", d.name),
            label: format!("{} (WSL)", d.name),
            detail: if d.is_default {
                "default distro".into()
            } else {
                String::new()
            },
        });
    }
    for h in ssh_hosts() {
        out.push(ShellChoice {
            kind_tag: format!("ssh:{h}"),
            label: format!("{h} (ssh)"),
            detail: "~/.ssh/config".into(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(guid: &str, name: Option<&str>, state: Option<u32>) -> (String, LxssEntry) {
        (guid.to_string(), (name.map(String::from), state))
    }

    /// U5: the Lxss fixture — State gate, docker-desktop skip, default flag
    /// (GUID compared case-insensitively), default-first ordering, and
    /// tolerance for value-less subkeys.
    #[test]
    fn lxss_fixture_classification() {
        let entries = vec![
            e("{AAA}", Some("docker-desktop"), Some(1)),
            e("{BBB}", Some("Ubuntu-24.04"), Some(1)),
            e("{CCC}", Some("Debian"), Some(3)), // installing — skipped
            e("{DDD}", Some("Ubuntu"), Some(1)),
            e("{EEE}", None, Some(1)),   // value-less subkey — skipped
            e("{FFF}", Some("docker-desktop-data"), Some(1)),
        ];
        let got = distros_from(&entries, Some("{bbb}"));
        assert_eq!(
            got,
            vec![
                WslDistro { name: "Ubuntu-24.04".into(), is_default: true },
                WslDistro { name: "Ubuntu".into(), is_default: false },
            ]
        );
        // No default value ⇒ nothing flagged; registry absent ⇒ empty.
        let got = distros_from(&entries, None);
        assert!(got.iter().all(|d| !d.is_default));
        assert!(distros_from(&[], Some("{BBB}")).is_empty());
    }

    #[test]
    fn shell_choices_always_lead_with_the_degraded_set() {
        // Environment-shaped beyond the first two rows; the invariant is the
        // stable prefix + the wsl:/ssh: tag grammar for whatever follows
        // (wsl cluster first, then the ssh-config hosts).
        let rows = shell_choices();
        assert_eq!(rows[0].kind_tag, "powershell");
        assert_eq!(rows[1].kind_tag, "cmd");
        let mut seen_ssh = false;
        for r in &rows[2..] {
            if r.kind_tag.starts_with("ssh:") {
                seen_ssh = true;
                assert!(r.label.ends_with(" (ssh)"));
                continue;
            }
            assert!(r.kind_tag.starts_with("wsl:"), "unexpected row {:?}", r.kind_tag);
            assert!(r.label.ends_with(" (WSL)"));
            assert!(!seen_ssh, "wsl rows must precede ssh rows");
        }
    }

    /// U5 (ssh half): the config fixture — Host token extraction, wildcard/
    /// negation skips, `=` separators, quoted tokens, Match/other keywords
    /// ignored, one Include level (nested Includes not followed), dedupe +
    /// order preservation.
    #[test]
    fn ssh_config_fixture_classification() {
        let cfg = r#"
# comment
Host devbox
  HostName devbox.internal
  User alice

Host *.internal ??-probe !prod-*
Host staging prod "web-01"
Match host devbox
  Port 2222
Host=edgebox
Include extra.conf nested.conf
"#;
        let mut reads: Vec<String> = Vec::new();
        let mut inc = |v: &str| -> Vec<String> {
            reads.push(v.to_string());
            match v {
                "extra.conf" => vec!["Host bastion devbox\nInclude deeper.conf\n".to_string()],
                "nested.conf" => vec!["Host tunnel-*\nHost edge\n".to_string()],
                _ => Vec::new(),
            }
        };
        let hosts = hosts_from_config(cfg, &mut inc, 0);
        assert_eq!(
            hosts,
            vec!["devbox", "staging", "prod", "web-01", "edgebox", "bastion", "edge"]
        );
        // One Include level only: deeper.conf was never requested.
        assert_eq!(reads, vec!["extra.conf", "nested.conf"]);
        // Missing config shape: no rows, no error.
        assert!(hosts_from_config("", &mut |_| Vec::new(), 0).is_empty());
    }
}
