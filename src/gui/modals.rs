//! Modal dialogs: the Modal enum + show_modal (+ their two private
//! helpers, show_dialog / dialog_input — all consumers live here).
//! Zero-behavior split from gui/mod.rs.

use super::*;

/// Centered dialog with dimmed, input-blocking backdrop. Esc or a click
/// outside closes it (via `should_close`).
fn show_dialog<R>(
    ctx: &egui::Context,
    title: &str,
    width: f32,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::ModalResponse<R> {
    egui::Modal::new(egui::Id::new("tc-dialog")).show(ctx, |ui| {
        // Scale-in / fade-in (V6): alpha ramp over ~120ms.
        let t = ui
            .ctx()
            .animate_bool_with_time(Id::new("tc-dialog-fade"), true, 0.12);
        ui.multiply_opacity(t);
        if t < 1.0 {
            ui.ctx().request_repaint();
        }
        ui.set_width(width);
        ui.add_space(4.0);
        ui.label(RichText::new(title).font(semibold(15.0)).color(TEXT));
        ui.add_space(12.0);
        content(ui)
    })
}

/// Styled single-line dialog input (D29). Returns whether Enter was pressed.
fn dialog_input(ui: &mut egui::Ui, text: &mut String, hint: &str) -> bool {
    let resp = ui.add(
        egui::TextEdit::singleline(text)
            .hint_text(hint)
            .desired_width(f32::INFINITY)
            .font(FontId::proportional(13.0))
            .margin(Margin::symmetric(10, 8)),
    );
    resp.request_focus();
    ui.input(|i| i.key_pressed(egui::Key::Enter))
}

// The NewTerminal form modal and the Import modal are DELETED (selector spec
// §4.7/D3): creation is the split-+ instant spawn or the launcher palette;
// claude-session import is a launcher section.
// The rename modals are DELETED too (task #22 / §5.4): renaming is inline —
// a borderless TextEdit in place of the name galley (sidebar row or top-bar
// title), Enter/blur commits, Esc/empty cancels.
pub(super) enum Modal {
    ConfirmDeleteTerminal(Uuid),
    ConfirmDeleteFolder(Uuid),
    NewFolder(String),
    /// SLEEP S8: single-terminal sleep whose gate tripped — the modal names
    /// the open block / flowing output. Idle sleeps never see a modal.
    ConfirmSleep(Uuid),
    /// SLEEP S8: folder sleep is ALWAYS confirmed (the blind bulk act) —
    /// the modal lists every presented-Running member by name + what it's
    /// doing. Confirming carries force semantics (§8.1).
    ConfirmSleepFolder(Uuid),
    /// QOL §5: a raw-path paste tripped the safety gate (multi-line/huge
    /// into a non-bracketed shell). Confirm re-encodes at send time;
    /// the checkbox clears `Prefs.paste_warn`.
    ConfirmPaste {
        id: Uuid,
        text: String,
        dont_warn: bool,
    },
    /// ssh-drop §4: first-ever ssh drop — consent before anything connects.
    /// The batch itself waits in `App.pending_ssh_drop`.
    SshDropConsent,
    /// Attribution Layer 3: per-host opt-in for the remote claude-session
    /// tracker hook, shown on the FIRST claude use in an ssh terminal to a
    /// host with no recorded verdict. The pending host/terminal waits in
    /// `App.pending_claude_hook`.
    ClaudeHookConsent,
    /// Codex attribution (task #30): consent for the codex session hook —
    /// local (Windows/WSL ~/.codex) or a remote ssh host. The pending lane
    /// waits in `App.pending_codex_hook`.
    CodexHookConsent,
}

impl App {
    pub(super) fn show_modal(&mut self, ctx: &egui::Context) {
        // Settings-spec G9: one overlay at a time. While the settings dialog
        // is open a queued modal simply waits (the consent scans gate on
        // `modal.is_some()`, so nothing re-queues meanwhile); it shows the
        // frame after settings closes.
        if self.settings.is_some() {
            ctx.animate_bool_with_time(Id::new("tc-dialog-fade"), false, 0.0);
            return;
        }
        let Some(mut modal) = self.modal.take() else {
            // No modal: keep the fade animation reset so the next one scales in.
            ctx.animate_bool_with_time(Id::new("tc-dialog-fade"), false, 0.0);
            return;
        };
        let mut keep = true;
        let mut actions: Vec<C2D> = Vec::new();

        match &mut modal {
            Modal::NewFolder(name) => {
                let mr = show_dialog(ctx, "New folder", 440.0, |ui| {
                    let enter = dialog_input(ui, name, "Folder name");
                    ui.add_space(16.0);
                    let valid = !name.trim().is_empty();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_button(ui, "Create", false, valid).clicked() || (enter && valid) {
                            actions.push(C2D::CreateFolder {
                                name: name.trim().to_string(),
                            });
                            keep = false;
                        }
                        ui.add_space(8.0);
                        if ghost_button_auto(ui, "Cancel", TEXT_SECONDARY).clicked() {
                            keep = false;
                        }
                    });
                });
                if mr.should_close() {
                    keep = false;
                }
            }
            Modal::ConfirmDeleteTerminal(id) => {
                let id = *id;
                let name = self
                    .state
                    .terminal(id)
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                let mr = show_dialog(ctx, "Delete terminal", 440.0, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "Delete \u{201C}{name}\u{201D}? The process is killed and its saved scrollback is erased."
                        ))
                        .size(13.0)
                        .color(TEXT_SECONDARY),
                    );
                    ui.add_space(20.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_button(ui, "Delete", true, true).clicked() {
                            // Move selection to a neighbor before it vanishes so
                            // the user isn't dumped to the empty state. This
                            // deliberately bypasses select_terminal (to dodge
                            // its other side effects), so the two cross-terminal
                            // panels it would drop must be cleared inline (B1):
                            // stale search matches otherwise drive the counter,
                            // Enter-stepping, and the current-match highlight
                            // on the WRONG terminal.
                            if self.selected == Some(id) {
                                self.selected = self.neighbor_of(id);
                                self.search = None;
                                self.blocks_panel = None;
                            }
                            actions.push(C2D::DeleteTerminal { id });
                            keep = false;
                        }
                        ui.add_space(8.0);
                        if ghost_button_auto(ui, "Cancel", TEXT_SECONDARY).clicked() {
                            keep = false;
                        }
                    });
                });
                if mr.should_close() {
                    keep = false;
                }
            }
            Modal::ConfirmSleep(id) => {
                let id = *id;
                let name = self
                    .state
                    .terminal(id)
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                // Re-read the evidence at paint time — the block may have
                // closed while the modal sat open; the copy stays honest.
                let evidence = match self.sleep_gate_evidence(id) {
                    Some(SleepEvidence::OpenBlock(cmd)) => {
                        format!("\u{201C}{}\u{201D} is still running.", middle_ellipsize(&cmd, 48))
                    }
                    Some(SleepEvidence::OutputFlowing) => "Output is still flowing.".to_string(),
                    None => String::new(),
                };
                let mr = show_dialog(ctx, "Sleep terminal", 460.0, |ui| {
                    let body = if evidence.is_empty() {
                        format!(
                            "Sleep \u{201C}{name}\u{201D}? Its processes are killed to free memory; \
                             scrollback, blocks and resume identity are kept, and Wake restores it."
                        )
                    } else {
                        format!(
                            "{evidence} Sleeping kills the process tree mid-command — the journal \
                             keeps what has streamed (a claude mid-response resumes from its last \
                             saved message), and Wake restores \u{201C}{name}\u{201D}."
                        )
                    };
                    ui.label(RichText::new(body).size(13.0).color(TEXT_SECONDARY));
                    ui.add_space(20.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_button(ui, "Sleep", false, true).clicked() {
                            actions.push(C2D::SleepTerminal { id });
                            keep = false;
                        }
                        ui.add_space(8.0);
                        if ghost_button_auto(ui, "Cancel", TEXT_SECONDARY).clicked() {
                            keep = false;
                        }
                    });
                });
                if mr.should_close() {
                    keep = false;
                }
            }
            Modal::ConfirmSleepFolder(fid) => {
                let fid = *fid;
                let fname = self
                    .state
                    .folders
                    .iter()
                    .find(|f| f.id == fid)
                    .map(|f| f.name.clone())
                    .unwrap_or_default();
                // The target list: every presented-Running member (S16) —
                // name + what it's doing (open-block cmd in danger red, else
                // the OSC title, muted). Moon-marked members are omitted.
                let members: Vec<(String, Option<String>, bool)> = self
                    .state
                    .terminals
                    .iter()
                    .filter(|t| {
                        t.folder == Some(fid)
                            && presented_status(t.status, t.asleep) == PresentedStatus::Running
                    })
                    .map(|t| {
                        let open = self
                            .blocks
                            .get(&t.id)
                            .and_then(|b| b.recs.iter().rev().find(|r| r.end_off.is_none()))
                            .map(|r| r.cmd.clone());
                        let busy = open.is_some();
                        let doing = open.or_else(|| {
                            self.terms
                                .get(&t.id)
                                .and_then(|b| b.title.clone())
                                .filter(|s| !s.is_empty())
                        });
                        (t.name.clone(), doing, busy)
                    })
                    .collect();
                let n = members.len();
                let mr = show_dialog(ctx, "Sleep folder", 480.0, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "Sleep every running terminal in \u{201C}{fname}\u{201D}? Processes are \
                             killed to free memory; scrollback, blocks and resume identity are \
                             kept, and Wake all restores them."
                        ))
                        .size(13.0)
                        .color(TEXT_SECONDARY),
                    );
                    ui.add_space(12.0);
                    for (name, doing, busy) in &members {
                        ui.horizontal(|ui| {
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(middle_ellipsize(name, 28)).size(13.0).color(TEXT),
                            );
                            if let Some(d) = doing {
                                ui.label(
                                    RichText::new(middle_ellipsize(d, 36))
                                        .font(FontId::monospace(12.0))
                                        .color(if *busy { DANGER } else { TEXT_MUTED }),
                                );
                            }
                        });
                    }
                    if members.is_empty() {
                        ui.label(
                            RichText::new("No running terminals in this folder.")
                                .size(12.0)
                                .color(TEXT_MUTED),
                        );
                    }
                    ui.add_space(20.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_button(
                            ui,
                            &format!("Sleep {n} terminal{}", if n == 1 { "" } else { "s" }),
                            false,
                            n > 0,
                        )
                        .clicked()
                        {
                            actions.push(C2D::SleepFolder { folder: fid });
                            keep = false;
                        }
                        ui.add_space(8.0);
                        if ghost_button_auto(ui, "Cancel", TEXT_SECONDARY).clicked() {
                            keep = false;
                        }
                    });
                });
                if mr.should_close() {
                    keep = false;
                }
            }
            Modal::ConfirmDeleteFolder(id) => {
                let id = *id;
                let name = self
                    .state
                    .folders
                    .iter()
                    .find(|f| f.id == id)
                    .map(|f| f.name.clone())
                    .unwrap_or_default();
                let mr = show_dialog(ctx, "Delete folder", 440.0, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "Delete folder \u{201C}{name}\u{201D}? Its terminals are kept and moved to Ungrouped."
                        ))
                        .size(13.0)
                        .color(TEXT_SECONDARY),
                    );
                    ui.add_space(20.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_button(ui, "Delete", true, true).clicked() {
                            actions.push(C2D::DeleteFolder { id });
                            keep = false;
                        }
                        ui.add_space(8.0);
                        if ghost_button_auto(ui, "Cancel", TEXT_SECONDARY).clicked() {
                            keep = false;
                        }
                    });
                });
                if mr.should_close() {
                    keep = false;
                }
            }
            Modal::ConfirmPaste { id, text, dont_warn } => {
                // QOL §5: the raw-path paste gate tripped — this shell has no
                // bracketed paste, so every line RUNS as it arrives. Preview
                // first, confirm explicitly; the encoding decision is remade
                // at send time (P5 — the mode may have flipped meanwhile).
                let id = *id;
                let lines: Vec<&str> = text.lines().collect();
                let n = lines.len().max(1);
                let more = lines.len().saturating_sub(3);
                let bytes = text.len();
                let mut confirm = false;
                let mr = show_dialog(ctx, "Paste into terminal", 460.0, |ui| {
                    ui.label(
                        RichText::new(if n > 1 {
                            format!(
                                "This shell runs each line the moment it arrives \u{2014} \
                                 paste {n} lines?"
                            )
                        } else {
                            format!("Paste {bytes} characters into the shell?")
                        })
                        .size(13.0)
                        .color(TEXT_SECONDARY),
                    );
                    ui.add_space(10.0);
                    for l in lines.iter().take(3) {
                        ui.label(
                            RichText::new(middle_ellipsize(l, 58))
                                .font(FontId::monospace(12.0))
                                .color(TEXT),
                        );
                    }
                    if more > 0 {
                        ui.label(
                            RichText::new(format!(
                                "+{more} more line{}",
                                if more == 1 { "" } else { "s" }
                            ))
                            .size(12.0)
                            .color(TEXT_MUTED),
                        );
                    }
                    ui.add_space(12.0);
                    ui.checkbox(dont_warn, "Don't warn again");
                    ui.add_space(16.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_button(ui, "Paste", false, true).clicked() {
                            confirm = true;
                            keep = false;
                        }
                        ui.add_space(8.0);
                        if ghost_button_auto(ui, "Cancel", TEXT_SECONDARY).clicked() {
                            keep = false;
                        }
                    });
                });
                if mr.should_close() {
                    keep = false;
                }
                if confirm {
                    if *dont_warn {
                        self.prefs.paste_warn = false;
                        self.save_prefs();
                    }
                    let t = text.clone();
                    self.send_paste(id, &t);
                }
            }
            Modal::SshDropConsent => {
                // ssh-drop §4: consent BEFORE anything connects. The batch
                // waits in pending_ssh_drop; the host is re-read at paint
                // time (program/args are static per terminal, P6 D1).
                let host = self.pending_ssh_drop.as_ref().and_then(|p| {
                    self.state.terminal(p.terminal).and_then(|t| {
                        match shell_family(&t.kind, &t.program, &t.args) {
                            ShellFamily::Ssh { host } => Some(host),
                            _ => None,
                        }
                    })
                });
                match (host, self.pending_ssh_drop.take()) {
                    (Some(host), Some(mut pending)) => {
                        let names: Vec<String> = pending
                            .paths
                            .iter()
                            .map(|p| {
                                p.file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            })
                            .collect();
                        let n = names.len();
                        let mut go = false;
                        let mr = show_dialog(ctx, &format!("Upload to {host}?"), 460.0, |ui| {
                            let body = if n == 1 {
                                format!(
                                    "This will copy \u{201C}{}\u{201D} to {host} over SFTP, \
                                     into ~/.tc-drops on that host, then paste the remote \
                                     path into the terminal. Continue?",
                                    names[0]
                                )
                            } else {
                                format!(
                                    "This will copy {n} files to {host} over SFTP, into \
                                     ~/.tc-drops on that host, then paste the remote paths \
                                     into the terminal. Continue?"
                                )
                            };
                            ui.label(RichText::new(body).size(13.0).color(TEXT_SECONDARY));
                            if n > 1 {
                                ui.add_space(10.0);
                                for name in names.iter().take(5) {
                                    ui.label(
                                        RichText::new(middle_ellipsize(name, 44))
                                            .size(12.0)
                                            .color(TEXT_MUTED),
                                    );
                                }
                                if n > 5 {
                                    ui.label(
                                        RichText::new(format!("+ {} more", n - 5))
                                            .size(12.0)
                                            .color(TEXT_MUTED),
                                    );
                                }
                            }
                            ui.add_space(12.0);
                            ui.checkbox(&mut pending.dont_ask_again, "Never show this again");
                            ui.add_space(16.0);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if primary_button(ui, "Continue", false, true).clicked() {
                                    go = true;
                                    keep = false;
                                }
                                ui.add_space(8.0);
                                if ghost_button_auto(ui, "Cancel", TEXT_SECONDARY).clicked() {
                                    keep = false;
                                }
                            });
                        });
                        if mr.should_close() {
                            keep = false;
                        }
                        if go {
                            if pending.dont_ask_again {
                                self.prefs.ssh_drop_skip_consent = true;
                                self.save_prefs();
                            }
                            self.enqueue_ssh_drop(pending.terminal, pending.paths);
                        } else if keep {
                            self.pending_ssh_drop = Some(pending);
                        }
                        // Cancel/Esc: the pending batch stays dropped —
                        // no upload, nothing pasted, no toast (§4: the
                        // user just said no; a toast would nag).
                    }
                    _ => {
                        // Terminal vanished / nothing pending: close silently.
                        keep = false;
                    }
                }
            }
            Modal::ClaudeHookConsent => {
                // Attribution Layer 3 §UX: per-host opt-in, asked once on
                // the first claude use in an ssh terminal to this host.
                // Yes/No persist the verdict; the checkbox applies the SAME
                // answer to all future hosts. Esc persists nothing (asked
                // again next run, not this one).
                match self.pending_claude_hook.take() {
                    Some(mut pending) => {
                        let host = pending.host.clone();
                        let terminal = pending.terminal;
                        let mut verdict: Option<bool> = None;
                        let mr = show_dialog(
                            ctx,
                            &format!("Add Claude Code session tracker to {host}?"),
                            460.0,
                            |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "This installs a small hook script on {host} \
                                         (~/.tc/claude-hook.sh, plus two entries merged \
                                         into ~/.claude/settings.json) so Claude Code \
                                         reports its live session id to this terminal. \
                                         Enables exact conversation restore — /clear and \
                                         /resume included — for claude sessions on this \
                                         host."
                                    ))
                                    .size(13.0)
                                    .color(TEXT_SECONDARY),
                                );
                                ui.add_space(12.0);
                                ui.checkbox(
                                    &mut pending.always,
                                    "Always \u{2014} don't show again (applies to all future hosts)",
                                );
                                ui.add_space(16.0);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if primary_button(ui, "Yes, add it", false, true).clicked() {
                                        verdict = Some(true);
                                        keep = false;
                                    }
                                    ui.add_space(8.0);
                                    if ghost_button_auto(ui, "No", TEXT_SECONDARY).clicked() {
                                        verdict = Some(false);
                                        keep = false;
                                    }
                                });
                            },
                        );
                        if mr.should_close() {
                            keep = false;
                        }
                        match verdict {
                            Some(yes) => {
                                self.prefs.claude_hook_hosts.insert(host.clone(), yes);
                                if pending.always {
                                    self.prefs.claude_hook_all = Some(yes);
                                }
                                self.save_prefs();
                                if yes {
                                    self.start_claude_hook_install(ctx, terminal, host);
                                } else {
                                    self.claude_hook_dismissed.insert(host);
                                }
                            }
                            None if keep => {
                                self.pending_claude_hook = Some(pending);
                            }
                            None => {
                                // Esc/close: no verdict recorded — don't
                                // re-open this run, ask again next run.
                                self.claude_hook_dismissed.insert(host);
                            }
                        }
                    }
                    None => {
                        keep = false;
                    }
                }
            }
            Modal::CodexHookConsent => {
                // Codex attribution §UX: asked once per lane on the first
                // codex use. R4-F6: the Windows lane persists its own verdict
                // (codex_hook_local); each WSL distro persists a per-distro
                // verdict, with the checkbox widening to all-WSL
                // (codex_hook_wsl); ssh lanes are per-host with the checkbox
                // widening to all-hosts. Writes mirror labels exactly.
                match self.pending_codex_hook.take() {
                    Some(mut pending) => {
                        let lane = pending.lane.clone();
                        let terminal = pending.terminal;
                        let key = lane.key();
                        let label = codex_key_label(&key);
                        let title = if lane.is_local() {
                            format!("Enable Codex session tracking on {label}?")
                        } else {
                            format!("Add Codex session tracker to {label}?")
                        };
                        let (target, always_label) = match &lane {
                            CodexLane::LocalWindows => (
                                "~/.codex/hooks.json (plus one trust entry in \
                                 ~/.codex/config.toml)",
                                "Always \u{2014} enable for WSL distros too",
                            ),
                            CodexLane::LocalWsl { .. } => (
                                "~/.codex/hooks.json + trust entry, plus a small hook \
                                 script (~/.tc/codex-hook.sh) in that distro",
                                "Always \u{2014} apply to all WSL distros",
                            ),
                            CodexLane::Ssh { .. } => (
                                "a small hook script (~/.tc/codex-hook.sh) plus a merged \
                                 ~/.codex/hooks.json + trust entry on that host",
                                "Always \u{2014} don't show again (applies to all future hosts)",
                            ),
                        };
                        let mut verdict: Option<bool> = None;
                        let mr = show_dialog(ctx, &title, 470.0, |ui| {
                            ui.label(
                                RichText::new(format!(
                                    "This adds {target} so Codex reports its live session id \
                                     to this terminal. Enables exact session restore across \
                                     close/reboot — in-TUI /resume, /new and /fork included."
                                ))
                                .size(13.0)
                                .color(TEXT_SECONDARY),
                            );
                            ui.add_space(12.0);
                            ui.checkbox(&mut pending.always, always_label);
                            ui.add_space(16.0);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if primary_button(ui, "Yes, enable it", false, true).clicked() {
                                    verdict = Some(true);
                                    keep = false;
                                }
                                ui.add_space(8.0);
                                if ghost_button_auto(ui, "No", TEXT_SECONDARY).clicked() {
                                    verdict = Some(false);
                                    keep = false;
                                }
                            });
                        });
                        if mr.should_close() {
                            keep = false;
                        }
                        match verdict {
                            Some(yes) => {
                                // R4-F6: the writes must mirror the labels
                                // exactly — a consent dialog's checkbox is
                                // the ONLY thing that widens the answer's
                                // scope beyond the asked lane.
                                match &lane {
                                    CodexLane::Ssh { host } => {
                                        self.prefs.codex_hook_hosts.insert(host.clone(), yes);
                                        if pending.always {
                                            self.prefs.codex_hook_all = Some(yes);
                                        }
                                    }
                                    CodexLane::LocalWindows => {
                                        self.prefs.codex_hook_local = Some(yes);
                                        // "enable for WSL distros too"
                                        if pending.always {
                                            self.prefs.codex_hook_wsl = Some(yes);
                                        }
                                    }
                                    CodexLane::LocalWsl { distro } => {
                                        self.prefs
                                            .codex_hook_wsl_distros
                                            .insert(distro.clone(), yes);
                                        // "apply to all WSL distros"
                                        if pending.always {
                                            self.prefs.codex_hook_wsl = Some(yes);
                                        }
                                    }
                                }
                                self.save_prefs();
                                if yes {
                                    self.start_codex_hook_install(ctx, terminal, lane);
                                } else {
                                    self.codex_hook_dismissed.insert(key);
                                }
                            }
                            None if keep => {
                                self.pending_codex_hook = Some(pending);
                            }
                            None => {
                                self.codex_hook_dismissed.insert(key);
                            }
                        }
                    }
                    None => {
                        keep = false;
                    }
                }
            }
        }

        for a in actions {
            self.send(a);
        }
        if keep {
            self.modal = Some(modal);
        }
    }
}
