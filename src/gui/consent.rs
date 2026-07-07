//! Hook-install consent lanes (claude Attribution Layer 3 + codex task #30):
//! pending state, lane plumbing, and the scan/start/drain cycle. Zero-behavior
//! split from gui/mod.rs.

use super::*;

/// Attribution Layer 3: the host a ClaudeHookConsent dialog covers.
/// Yes/No persist a per-host verdict in Prefs; the `always` checkbox
/// applies the SAME answer to every future host (Prefs.claude_hook_all).
/// Esc/close-without-answer persists nothing — re-asked next GUI run only
/// (`claude_hook_dismissed` suppresses it for this run).
pub(super) struct PendingClaudeHook {
    pub(super) terminal: Uuid,
    pub(super) host: String,
    pub(super) always: bool,
}

/// Codex attribution: which lane a codex-hook consent/install covers.
/// R4-F6: every lane records its own verdict — `LocalWindows` (native
/// ~/.codex), `LocalWsl` (per-distro, with the all-WSL checkbox as
/// fallback), `Ssh` (per-host). `key()` is the dedupe/dismiss identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CodexLane {
    /// Native Windows codex home (~/.codex).
    LocalWindows,
    /// A WSL distro's codex home, reached over `\\wsl$\<distro>`.
    LocalWsl { distro: String },
    /// A remote host over the terminal's ssh transport.
    Ssh { host: String },
}

impl CodexLane {
    /// Stable per-lane identity for dismiss/done sets and per-host prefs.
    pub(super) fn key(&self) -> String {
        match self {
            CodexLane::LocalWindows => "local:windows".into(),
            CodexLane::LocalWsl { distro } => format!("local:wsl:{distro}"),
            CodexLane::Ssh { host } => format!("ssh:{host}"),
        }
    }
    pub(super) fn is_local(&self) -> bool {
        !matches!(self, CodexLane::Ssh { .. })
    }
}

/// The codex-hook lane a `CodexHookConsent` dialog covers (mirrors
/// `PendingClaudeHook`).
pub(super) struct PendingCodexHook {
    pub(super) terminal: Uuid,
    pub(super) lane: CodexLane,
    pub(super) always: bool,
}

/// Human label for a codex lane key (toast copy). Keys are stable identifiers;
/// this is display-only.
pub(super) fn codex_key_label(key: &str) -> String {
    if key == "local:windows" {
        "this PC".into()
    } else if let Some(d) = key.strip_prefix("local:wsl:") {
        format!("WSL {d}")
    } else if let Some(h) = key.strip_prefix("ssh:") {
        h.to_string()
    } else {
        key.to_string()
    }
}

/// Resolve a WSL distro's `$HOME` (POSIX), for the `\\wsl$` write paths and the
/// codex-visible trust key. Blocking (a single `wsl.exe` call) — worker-thread
/// only. None ⇒ the distro is unreachable / returned nothing.
pub(super) fn wsl_home(distro: &str) -> Option<String> {
    let out = std::process::Command::new("wsl.exe")
        .args(["-d", distro, "--", "sh", "-lc", "printf %s \"$HOME\""])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let home = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (home.starts_with('/')).then_some(home)
}

/// Do the actual codex-hook install for one lane (worker thread). `ssh_ident`
/// is the terminal's (program, args) — required for the Ssh lane only.
pub(super) fn codex_install_lane(
    lane: &CodexLane,
    ssh_ident: Option<&(String, Vec<String>)>,
) -> Result<crate::codex_hooks::Outcome, String> {
    use crate::codex_hooks::{install_local, install_remote, LocalTarget, POSIX_HOOK_COMMAND};
    match lane {
        CodexLane::LocalWindows => {
            let home = dirs::home_dir()
                .ok_or("no home dir")?
                .join(".codex");
            let command = crate::codex_hooks::windows_hook_command()
                .ok_or("no sibling pulse-ctl.exe (birth-correlation still covers codex here)")?;
            let codex_hooks_path = home.join("hooks.json").to_string_lossy().into_owned();
            install_local(&LocalTarget {
                access_home: home,
                codex_hooks_path,
                command,
                script: None,
            })
        }
        CodexLane::LocalWsl { distro } => {
            let home = wsl_home(distro)
                .ok_or_else(|| format!("could not resolve $HOME in WSL {distro}"))?;
            // `\\wsl$\<distro>` + a POSIX path as a Windows UNC.
            let unc = |posix: &str| {
                PathBuf::from(format!(
                    "\\\\wsl$\\{distro}{}",
                    posix.replace('/', "\\")
                ))
            };
            install_local(&LocalTarget {
                access_home: unc(&format!("{home}/.codex")),
                codex_hooks_path: format!("{home}/.codex/hooks.json"),
                command: POSIX_HOOK_COMMAND.to_string(),
                script: Some(unc(&format!("{home}/.tc/codex-hook.sh"))),
            })
        }
        CodexLane::Ssh { .. } => {
            let (program, args) = ssh_ident.ok_or("no ssh transport identity")?;
            install_remote(program, args)
        }
    }
}

impl App {
    /// Attribution Layer 3 trigger: FIRST claude use in an ssh terminal to a
    /// host without a recorded verdict ⇒ the consent popup. Runs every
    /// frame; the scan is a linear pass over the snapshot (a handful of
    /// terminals) gated to nothing-pending. Yes-hosts (per-host or global)
    /// re-verify the install once per GUI run (idempotent; heals a deleted
    /// remote script); no-hosts are suppressed forever.
    pub(super) fn scan_claude_hook_consent(&mut self, ctx: &egui::Context) {
        if self.modal.is_some() || self.pending_claude_hook.is_some() {
            return;
        }
        let mut install: Option<(Uuid, String)> = None;
        let mut ask: Option<(Uuid, String)> = None;
        let mut settle: Vec<Uuid> = Vec::new();
        for t in &self.state.terminals {
            // r4 perf-gui L3: skip terminals whose host verdict is already
            // settled without re-running shell_family (its Ssh arm clones
            // the host String every frame otherwise). Cleared on snapshot
            // apply and on install failure (retry re-opens the host).
            if self.claude_consent_settled.contains(&t.id) {
                continue;
            }
            let Some(cli) = &t.inner_cli else { continue };
            if cli.adapter != "claude" {
                continue;
            }
            let ShellFamily::Ssh { host } = shell_family(&t.kind, &t.program, &t.args) else {
                continue;
            };
            if self.claude_hook_dismissed.contains(&host)
                || self.claude_hook_done.contains(&host)
            {
                settle.push(t.id);
                continue;
            }
            let verdict = self
                .prefs
                .claude_hook_hosts
                .get(&host)
                .copied()
                .or(self.prefs.claude_hook_all);
            match verdict {
                Some(true) => {
                    install = Some((t.id, host));
                    break;
                }
                Some(false) => {
                    self.claude_hook_dismissed.insert(host);
                    settle.push(t.id);
                }
                None => {
                    ask = Some((t.id, host));
                    break;
                }
            }
        }
        self.claude_consent_settled.extend(settle);
        if let Some((id, host)) = install {
            self.start_claude_hook_install(ctx, id, host);
        } else if let Some((terminal, host)) = ask {
            self.pending_claude_hook = Some(PendingClaudeHook {
                terminal,
                host,
                always: false,
            });
            self.modal = Some(Modal::ClaudeHookConsent);
        }
    }

    /// Kick one beacon install for `host` over `terminal`'s transport
    /// identity, on a worker thread (three bounded sftp connections — never
    /// on the GUI thread). Result lands via `claude_hook_rx` → toast.
    pub(super) fn start_claude_hook_install(&mut self, ctx: &egui::Context, terminal: Uuid, host: String) {
        let Some(t) = self.state.terminal(terminal) else { return };
        let program = t.program.clone();
        let args = t.args.clone();
        self.claude_hook_done.insert(host.clone());
        let tx = self.claude_hook_tx.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("claude-hook-install".into())
            .spawn(move || {
                let r = crate::claude_hooks::install_remote(&program, &args);
                let _ = tx.send((host, r));
                ctx.request_repaint();
            })
            .ok();
    }

    /// Install-result drain (runs in `logic()` beside the upload drain).
    pub(super) fn drain_claude_hooks(&mut self) {
        while let Ok((host, result)) = self.claude_hook_rx.try_recv() {
            match result {
                Ok(crate::claude_hooks::Outcome::Installed) => {
                    log::info!("claude hook installed on {host}");
                    self.toasts.push(toast::Toast {
                        kind: toast::ToastKind::Info,
                        // N3: capitalized like the codex drain's toast copy.
                        title: format!("Claude session tracker added to {host}"),
                        detail: vec![
                            "exact conversation restore is on for this host".into()
                        ],
                        ttl: Some(Duration::from_secs(6)),
                        action: None,
                    });
                }
                Ok(crate::claude_hooks::Outcome::AlreadyInstalled) => {
                    // Re-verify pass found everything in place — silent
                    // (the feature already works; a toast would nag).
                    log::info!("claude hook already present on {host}");
                }
                Err(e) => {
                    log::warn!("claude hook install on {host} failed: {e}");
                    // Allow a retry on the next claude use this run.
                    self.claude_hook_done.remove(&host);
                    self.claude_hook_dismissed.insert(host.clone());
                    // The settled-terminal skip cache (perf L3) may hold ids
                    // for this host — re-open them for the retry.
                    self.claude_consent_settled.clear();
                    self.toasts.push(toast::Toast {
                        kind: toast::ToastKind::Error,
                        title: format!("Claude session tracker install on {host} failed"),
                        detail: vec![e],
                        ttl: Some(Duration::from_secs(8)),
                        action: None,
                    });
                }
            }
        }
    }

    /// Codex attribution (task #30) trigger: the FIRST codex use in a terminal
    /// whose lane (local Windows / a WSL distro / an ssh host) has no recorded
    /// verdict ⇒ the consent popup. Once accepted, the install runs once per
    /// GUI run per lane (idempotent — heals a deleted hooks.json/trust/script).
    /// Runs every frame; a linear pass over a handful of terminals gated to
    /// nothing-pending. R4-F6: each lane class has its own verdict —
    /// `codex_hook_local` covers ONLY Windows-native; WSL distros use
    /// per-distro verdicts with the all-WSL checkbox (`codex_hook_wsl`) as
    /// fallback; ssh lanes are per-host.
    pub(super) fn scan_codex_hook_consent(&mut self, ctx: &egui::Context) {
        if self.modal.is_some() || self.pending_codex_hook.is_some() {
            return;
        }
        let mut install: Option<(Uuid, CodexLane)> = None;
        let mut ask: Option<(Uuid, CodexLane)> = None;
        let mut settle: Vec<Uuid> = Vec::new();
        for t in &self.state.terminals {
            // r4 perf-gui L3: terminals whose lane already reached a
            // dismissed/done/refused verdict are remembered by id — the
            // shell_family + key() Strings must not be rebuilt every logic
            // frame forever. Cleared on snapshot apply and on install
            // failure (retry re-opens the lane).
            if self.codex_consent_settled.contains(&t.id) {
                continue;
            }
            let Some(cli) = &t.inner_cli else { continue };
            if cli.adapter != "codex" {
                continue;
            }
            let lane = match shell_family(&t.kind, &t.program, &t.args) {
                ShellFamily::Ssh { host } => CodexLane::Ssh { host },
                // Shared pure helper; lives daemon-side because the tracker
                // owns the distro-registry cache (N4 — deliberate layering).
                ShellFamily::WslShell { distro } => match crate::daemon::claude_registry::resolve_distro(distro.as_deref()) {
                    Some(d) => CodexLane::LocalWsl { distro: d },
                    None => continue, // unknown distro — can't reach its ~/.codex
                },
                // Pwsh / Cmd / Other run codex in the native Windows namespace.
                ShellFamily::Pwsh | ShellFamily::Cmd | ShellFamily::Other => {
                    CodexLane::LocalWindows
                }
            };
            let key = lane.key();
            if self.codex_hook_dismissed.contains(&key) || self.codex_hook_done.contains(&key) {
                settle.push(t.id);
                continue;
            }
            // R4-F6: verdict per lane class — the consent dialog's writes
            // (modals.rs) mirror this read exactly.
            let verdict = match &lane {
                CodexLane::LocalWindows => self.prefs.codex_hook_local,
                CodexLane::LocalWsl { distro } => self
                    .prefs
                    .codex_hook_wsl_distros
                    .get(distro)
                    .copied()
                    .or(self.prefs.codex_hook_wsl),
                CodexLane::Ssh { host } => self
                    .prefs
                    .codex_hook_hosts
                    .get(host)
                    .copied()
                    .or(self.prefs.codex_hook_all),
            };
            match verdict {
                Some(true) => {
                    install = Some((t.id, lane));
                    break;
                }
                Some(false) => {
                    self.codex_hook_dismissed.insert(key);
                    settle.push(t.id);
                }
                None => {
                    ask = Some((t.id, lane));
                    break;
                }
            }
        }
        self.codex_consent_settled.extend(settle);
        if let Some((id, lane)) = install {
            self.start_codex_hook_install(ctx, id, lane);
        } else if let Some((terminal, lane)) = ask {
            self.pending_codex_hook = Some(PendingCodexHook {
                terminal,
                lane,
                always: false,
            });
            self.modal = Some(Modal::CodexHookConsent);
        }
    }

    /// Kick one codex-hook install for a lane on a worker thread (file IO / a
    /// couple of bounded sftp connections — never on the GUI thread). Result
    /// lands via `codex_hook_rx` → toast.
    pub(super) fn start_codex_hook_install(&mut self, ctx: &egui::Context, terminal: Uuid, lane: CodexLane) {
        let key = lane.key();
        self.codex_hook_done.insert(key.clone());
        // ssh needs the terminal's transport identity (program+args).
        let ssh_ident = self
            .state
            .terminal(terminal)
            .map(|t| (t.program.clone(), t.args.clone()));
        let tx = self.codex_hook_tx.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("codex-hook-install".into())
            .spawn(move || {
                let r = codex_install_lane(&lane, ssh_ident.as_ref());
                let _ = tx.send((lane.key(), r));
                ctx.request_repaint();
            })
            .ok();
    }

    /// Codex install-result drain (runs in `logic()` beside the claude drain).
    pub(super) fn drain_codex_hooks(&mut self) {
        while let Ok((key, result)) = self.codex_hook_rx.try_recv() {
            let label = codex_key_label(&key);
            match result {
                Ok(crate::codex_hooks::Outcome::Installed) => {
                    log::info!("codex hook installed for {key}");
                    self.toasts.push(toast::Toast {
                        kind: toast::ToastKind::Info,
                        title: format!("Codex session tracker added ({label})"),
                        detail: vec!["exact session restore is on for codex here".into()],
                        ttl: Some(Duration::from_secs(6)),
                        action: None,
                    });
                }
                Ok(crate::codex_hooks::Outcome::AlreadyInstalled) => {
                    log::info!("codex hook already present for {key}");
                }
                Err(e) => {
                    log::warn!("codex hook install for {key} failed: {e}");
                    // Allow a retry on the next codex use this run.
                    self.codex_hook_done.remove(&key);
                    self.codex_hook_dismissed.insert(key);
                    // The settled-terminal skip cache (perf L3) may hold ids
                    // for this lane — re-open them for the retry.
                    self.codex_consent_settled.clear();
                    self.toasts.push(toast::Toast {
                        kind: toast::ToastKind::Error,
                        title: format!("Codex session tracker install failed ({label})"),
                        detail: vec![e],
                        ttl: Some(Duration::from_secs(8)),
                        action: None,
                    });
                }
            }
        }
    }
}
