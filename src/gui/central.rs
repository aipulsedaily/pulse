//! Central area: terminal card, dashboard, empty state. Zero-behavior
//! split from gui/mod.rs.

use super::*;

impl App {
    pub(super) fn central(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG))
            .show(root, |ui| {
                // Anchor for the launcher overlay (under the titlebar,
                // centered on this panel).
                self.central_rect = Some(ui.max_rect());
                if let CentralView::Dashboard(folder) = self.central_view {
                    self.dashboard(ui, folder);
                    return;
                }
                // Keep the dashboard fade reset so it scales in on next entry.
                ui.ctx().animate_bool_with_time(Id::new("dashboard-fade"), false, 0.0);
                let Some(id) = self.selected else {
                    self.empty_state(ui);
                    return;
                };
                // Deleted-but-still-selected (one frame): render nothing.
                if self.state.terminal(id).is_none() {
                    return;
                }
                let central_rect = ui.max_rect();

                // Terminal card (D30): grid inside a rounded surface, inset
                // 8px all around now that the old header strip is merged into
                // the titlebar (task #21). No border stroke — the
                // TERM_BG-vs-app-background shift is the structure (seamless
                // doctrine: depth by background shift/shadow, never lines).
                let card = egui::Frame::new()
                    .fill(TERM_BG)
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::same(0))
                    .outer_margin(Margin {
                        left: 8,
                        right: 8,
                        top: 8,
                        bottom: 8,
                    });
                card.show(ui, |ui| {
                    self.terminal_card(ui, id);
                });

                // Blocks recall panel (P2): floats under the header's right
                // edge, above the card.
                self.blocks_panel_ui(ui.ctx(), central_rect, id);
            });
    }

    /// The character grid inside the terminal card, with debounced PTY resize.
    pub(super) fn terminal_card(&mut self, ui: &mut egui::Ui, id: Uuid) {
        // Debounced grid sizing: only resize the PTYs once the window geometry
        // has been stable for a moment. Monitor hops and drag-resizes otherwise
        // storm ConPTY with intermediate sizes, which clears the screen.
        let ppp = ui.ctx().pixels_per_point();
        let font = FontId::monospace(self.prefs.font_size);
        let (raw_w, raw_h) = ui
            .ctx()
            .fonts_mut(|f| (f.glyph_width(&font, 'm'), f.row_height(&font)));
        // Snap cell metrics to whole physical pixels for crisp columns (T2).
        let cell = egui::vec2(
            (raw_w * ppp).round().max(1.0) / ppp,
            (raw_h * ppp).round().max(1.0) / ppp,
        );
        let layout = term_view::grid_inner_size(ui.available_size());
        let target = (layout, cell);
        // Only the FOCUSED window may drive PTY resizes. Otherwise two open
        // windows of different sizes fight over each terminal's grid and thrash
        // ConPTY continuously. An unfocused window renders at whatever grid was
        // last committed; clearing any pending candidate stops it from
        // committing with stale timing when focus returns, where the normal
        // 250ms debounce re-evaluates fresh.
        let win_focused = ui.ctx().input(|i| i.focused);
        if !win_focused {
            // Only the FOCUSED window drives PTY resizes; otherwise two windows
            // of different sizes fight over each terminal's grid. Drop any
            // pending candidate so it can't commit with stale timing on refocus.
            self.pending_grid = None;
        } else if self.last_grid != Some(target) {
            let first = self.last_grid.is_none();
            let cell_changed = self.last_grid.map(|(_, c)| c).is_some_and(|pc| pc != cell);
            // A cell-metric change with a recent explicit font step is a
            // DELIBERATE click (footer stepper / Ctrl+wheel), not a
            // DPI/monitor-hop flap — it takes the live-drag regime so the
            // click lands within one throttle window instead of the 500ms
            // hysteresis (the perceived "font change takes ages" floor).
            let font_step_recent = self
                .font_step_t0
                .is_some_and(|t| t.elapsed() < Duration::from_secs(5));
            // Two regimes:
            //  • Cell-metric change WITHOUT a font step (a ppp flap, e.g. a
            //    1080p↔4K monitor hop): each commit forces a destructive
            //    conhost repaint, so wait for 500ms of stability before
            //    committing.
            //  • Layout-only change (plain drag-resize) and explicit font
            //    steps: resize live so the grid re-wraps with no dead margins
            //    — commit on the leading edge, then at most once per 120ms,
            //    plus a trailing commit when the size settles.
            let do_commit = if first {
                true
            } else if cell_changed && !font_step_recent {
                matches!(
                    self.pending_grid,
                    Some((l, c, t0)) if l == layout && c == cell
                        && t0.elapsed() >= Duration::from_millis(500)
                )
            } else {
                self.last_resize_commit.elapsed() >= Duration::from_millis(120)
            };
            if do_commit {
                self.last_grid = Some(target);
                self.pending_grid = None;
                self.last_resize_commit = Instant::now();
                // LAZY RESIZE (font-perf fix): resize ONLY the shown terminal
                // here — its local grid and its PTY together, so the rendered
                // grid and ConPTY never diverge. Background terminals keep
                // their old geometry (a background PTY doesn't need new dims
                // until viewed) and heal through the corrective per-terminal
                // resize below the moment they are shown — the pre-existing
                // resize-on-select path (P3 §7 epoch flips ride the same
                // block). The old all-terminals loop reflowed EVERY backend's
                // scrollback on the paint thread (~0.4s frame stall at 20
                // streaming terminals, measured) and stormed 20 PTY resizes
                // → 20 conhost repaints per step, all of which the GUI then
                // parsed. Geometry stays PER-TERMINAL via layout_for (P3 §7).
                let perf_t0 = self.perf3.is_some().then(Instant::now);
                let l = self.layout_for(id, layout);
                let mut sent = false;
                if let Some(b) = self.terms.get_mut(&id) {
                    if let Some((cols, rows)) = b.resize_to(l, cell) {
                        self.send(C2D::Resize { id, cols, rows });
                        sent = true;
                    }
                }
                if let Some(t0) = perf_t0 {
                    log::info!(
                        "[perf] fontstep commit cell_changed={cell_changed} \
                         font_step={font_step_recent} sent={sent} resize_us={} \
                         wait_ms={} ms={}",
                        t0.elapsed().as_micros(),
                        self.font_perf
                            .as_ref()
                            .map(|f| f.t0.elapsed().as_millis() as u64)
                            .unwrap_or(0),
                        gui_ms()
                    );
                }
                if let Some(fp) = &mut self.font_perf {
                    let now = Instant::now();
                    fp.committed = Some(now);
                    fp.last_activity = now;
                }
            } else {
                // Remember the latest target and schedule the trailing commit.
                match self.pending_grid {
                    Some((l, c, _)) if l == layout && c == cell => {}
                    _ => self.pending_grid = Some((layout, cell, Instant::now())),
                }
                let delay = if cell_changed && !font_step_recent { 80 } else { 120 };
                ui.ctx().request_repaint_after(Duration::from_millis(delay));
            }
        }

        // Corrective per-terminal resize (P3 §7) — ALSO the lazy-resize heal
        // (font-perf fix): a hooked terminal's strip reservation can postdate
        // its attach (the epoch arrives with the Blocks sync one drain
        // later), and background terminals are deliberately left at their old
        // geometry by the commit above — they pick up the current
        // layout/cell HERE, the first frame they are shown (one reflow + one
        // PTY resize + one conhost repaint, for exactly the terminal being
        // viewed). resize_to is a no-op when nothing changed, so this fires
        // at most once per epoch flip / stale show.
        if win_focused {
            if let Some((base, cell)) = self.last_grid {
                let l = self.layout_for(id, base);
                if let Some(b) = self.terms.get_mut(&id) {
                    if let Some((cols, rows)) = b.resize_to(l, cell) {
                        self.send(C2D::Resize { id, cols, rows });
                    }
                }
            }
        }

        let hooked = self.hooked(id);
        let running =
            self.state.terminal(id).map(|t| t.status) == Some(TermStatus::Running);
        let overlay_open = self.modal.is_some()
            || self.search.is_some()
            || self.blocks_panel.is_some()
            || self.history.is_some()
            || self.launcher.is_some()
            // Inline rename owns the keyboard while active (§5.4/§8): the
            // composer's every-frame focus re-request and the grid both
            // stand down through this one flag.
            || self.renaming.is_some();

        // Composer signal pump for the selected terminal (P3 §2.4): gate
        // eval + settle/hold wakeup (the sanctioned self-scheduled repaints).
        let mut comp_active = false;
        let now = Instant::now();
        if hooked {
            // Pending prompt-end upgrade (v0.1.1 quiescence capture): must
            // resolve BEFORE the tick/gate read it; an idle terminal needs
            // the scheduled wakeup (no output frame will arrive to resolve
            // on).
            if let Some(b) = self.terms.get_mut(&id) {
                if let Some(deadline) = b.poll_pending_prompt_end(now) {
                    ui.ctx()
                        .request_repaint_after(deadline.saturating_duration_since(now));
                }
            }
            let comp_had_focus = self.composers.get(&id).is_some_and(|c| c.has_focus);
            let grid_focused = win_focused && !overlay_open && !comp_had_focus;
            let wake = match (
                self.composers.get_mut(&id),
                self.terms.get(&id),
                self.blocks.get(&id),
            ) {
                (Some(st), Some(backend), Some(bl)) => {
                    let w = st.tick(backend, &bl.recs, running, grid_focused, now);
                    comp_active = st.mode == ComposerMode::Compose;
                    w
                }
                _ => None,
            };
            if let Some(deadline) = wake {
                ui.ctx()
                    .request_repaint_after(deadline.saturating_duration_since(now));
            }
            // Establish composer focus BEFORE term_view reads events (Bug 1
            // focus-handoff race): request the editor's egui focus for THIS
            // frame so the grid — which may still hold focus from last frame
            // — reports !has_focus and ignores the keystroke, leaving it for
            // the editor created later this frame. Without it, a key in the
            // arm frame lands in the grid (raw → episode used → the arm is
            // dismissed): the confirmed "typing goes raw at a fresh prompt"
            // cause. Held every frame while armed (want_focus set on the
            // transition, then has_focus) so no key can slip through; the
            // §13.1 alt-tab non-seize is preserved because want_focus is only
            // set when the grid held focus and is cleared once confirmed.
            if comp_active && win_focused && !overlay_open {
                let hold = self
                    .composers
                    .get(&id)
                    .is_some_and(|c| c.want_focus || c.has_focus);
                if hold {
                    let ed_id = Id::new(("composer", id));
                    ui.ctx().memory_mut(|m| m.request_focus(ed_id));
                }
            }
            // A released SubmitHold whose command is grid-verified becomes a
            // PERMANENT history cover (the flicker fix): the submitted row
            // keeps `❯ cmd` styling instead of ever reverting to raw `PS …>`.
            // The backend owns the covers, so drain the composer's outbox here
            // (mutable-backend context) right after tick.
            if let Some((line, col, cwd, cmd)) = self
                .composers
                .get_mut(&id)
                .and_then(|c| c.take_pending_history_cover())
            {
                if let Some(b) = self.terms.get_mut(&id) {
                    b.add_history_cover(line, col, cwd, cmd);
                }
            }
            // Raced-arm reclaim (P4): send the clear chord that empties the
            // shell line whose type-ahead the composer just pulled into the
            // draft. Sanctioned send path; the reclaimed text is now the
            // draft, so the shell line must be cancelled to avoid a double.
            if let Some(bytes) = self
                .composers
                .get_mut(&id)
                .and_then(|c| c.take_pending_clear())
            {
                self.send(C2D::Input { id, bytes });
            }
        }

        // Don't let the grid grab keyboard focus while the search field,
        // the blocks panel, the history popup, or the launcher is open
        // (typing must land in their filters), or while the composer owns
        // the prompt episode (P3 §3 extended by P4 §3.6 and selector D14:
        // modal > launcher > search > blocks panel = history popup >
        // composer > grid).
        let focused = self.modal.is_none()
            && self.search.is_none()
            && self.blocks_panel.is_none()
            && self.history.is_none()
            && self.launcher.is_none()
            && self.renaming.is_none()
            && !comp_active;
        self.glyphs.sync(font.clone(), ppp);
        // Borrow order: evaluate the Re-run gate first (immutable, ends),
        // then hold `search`, `blocks` and `terms` together — disjoint fields.
        let can_rerun = self.can_rerun(id);
        // F5c: rows rotating into history invalidate the stored match Points.
        // Recompute on drift (debounced 250ms — the all-matches pass walks
        // the whole scrollback); until then the current-match highlight is
        // withheld below rather than painted on arbitrary rows.
        let cur_hist = self.terms.get(&id).map(|b| b.history_size()).unwrap_or(0);
        if self.search.as_ref().is_some_and(|s| {
            // Adaptive drift debounce: while the user is driving the search
            // (query edit / step within ~2s) the 250ms cadence keeps the
            // counter fresh; PURE output drift rebuilds at 1s — the F5c
            // staleness gate below withholds the current-match highlight
            // either way until the rebuild lands, so nothing stale paints.
            let debounce = if s.last_user.elapsed() < Duration::from_secs(2) {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(1)
            };
            !s.query.is_empty()
                && s.matches_history != cur_hist
                && s.last_build.elapsed() >= debounce
        }) {
            self.recompute_search(id);
        }
        let (sre, curm) = match &mut self.search {
            Some(s) => (
                s.regex.as_mut(),
                (s.matches_history == cur_hist)
                    .then(|| s.matches.get(s.current).cloned())
                    .flatten(),
            ),
            None => (None, None),
        };
        let mut write = Vec::new();
        let mut block_action = None;
        let mut grid_clicked = false;
        let mut grid_click_pos = None;
        let mut cover_rect = None;
        let mut grid_resp: Option<egui::Response> = None;
        let mut rclick_forwarded = false;
        let mut middle_paste = false;
        let mut paste_pending: Option<String> = None;
        // Composer prompt-row cover intent (P3, seamless redesign): when the
        // armed composer provably owns a clean at-rest prompt, the grid
        // paints its background over the shell's prompt row and the composer
        // renders THE one prompt there. Certainty gates: armed + live latch
        // + cursor exactly at the captured prompt end; term_view adds the
        // at-rest viewport + on-screen row checks. Any failure ⇒ no cover,
        // raw rendering. A live SubmitHold pins the cover to its recorded row
        // through the handoff (Bug 3) regardless of mode and cursor — the
        // single-source gate lives in composer::cover_line_for (unit-tested).
        let cover_line = match (self.composers.get(&id), self.terms.get(&id)) {
            (Some(st), Some(b)) => composer::cover_line_for(st, b, comp_active, now),
            _ => None,
        };
        // QOL §4.7: drop-target hover wash + label (runs only while an OS
        // drag hovers the window). Field-level reads only — the (sre, curm)
        // search borrow is still live here. (The v1 ssh-refusal linger died
        // with #26: ssh drops now upload, and the consent dialog / progress
        // toast is the landing feedback.)
        let presented_now = self
            .state
            .terminal(id)
            .map(|t| presented_status(t.status, t.asleep))
            .unwrap_or(PresentedStatus::Dead);
        let drop_label = {
            let (hover_files, hover_n) = ui.ctx().input(|i| {
                (
                    i.raw
                        .hovered_files
                        .iter()
                        .filter_map(|f| f.path.clone())
                        .collect::<Vec<PathBuf>>(),
                    i.raw.hovered_files.len(),
                )
            });
            if hover_n > 0 {
                let fam = self
                    .state
                    .terminal(id)
                    .map(|t| drop::drop_family(&shell_family(&t.kind, &t.program, &t.args)))
                    .unwrap_or(drop::DropFamily::Other);
                Some(drop::hover_label(
                    &fam,
                    comp_active,
                    presented_now == PresentedStatus::Running,
                    &hover_files,
                    hover_n,
                ))
            } else {
                None
            }
        };
        let view_opts = term_view::ViewOpts {
            copy_on_select: self.prefs.copy_on_select,
            paste_warn: self.prefs.paste_warn,
            drop_label,
        };
        // Card split (P3 §7): hooked terminals draw the grid above a
        // CONSTANT 36px composer strip; hookless terminals keep today's
        // single call byte-for-byte.
        let full = ui.available_rect_before_wrap();
        let grid_area = if hooked {
            Rect::from_min_max(
                full.min,
                Pos2::new(full.max.x, full.max.y - composer::STRIP_H),
            )
        } else {
            full
        };
        {
            let bctx = self.blocks.get(&id).and_then(|b| {
                (!b.recs.is_empty()).then_some(term_view::BlockViewCtx {
                    recs: &b.recs,
                    can_rerun,
                })
            });
            if let Some(backend) = self.terms.get_mut(&id) {
                let (resp, out) = if hooked {
                    let mut child = ui.new_child(
                        UiBuilder::new().max_rect(grid_area).layout(*ui.layout()),
                    );
                    term_view::show(
                        &mut child,
                        backend,
                        &self.bindings,
                        font.clone(),
                        focused,
                        &mut self.url_regex,
                        &mut self.glyphs,
                        &mut self.render_scratch,
                        ppp,
                        sre,
                        curm,
                        bctx,
                        cover_line,
                        view_opts,
                    )
                } else {
                    term_view::show(
                        ui,
                        backend,
                        &self.bindings,
                        font.clone(),
                        focused,
                        &mut self.url_regex,
                        &mut self.glyphs,
                        &mut self.render_scratch,
                        ppp,
                        sre,
                        curm,
                        bctx,
                        None,
                        view_opts,
                    )
                };
                grid_clicked = resp.clicked();
                grid_click_pos = resp.interact_pointer_pos();
                grid_resp = Some(resp);
                cover_rect = out.cover;
                rclick_forwarded = out.rclick_forwarded;
                middle_paste = out.middle_paste;
                paste_pending = out.paste_pending;
                // Copy synthesis must match paint (§6): record the row the
                // current-prompt cover actually BLANKED this frame so
                // selection_text can substitute "" for it.
                backend.cur_blank_line = if out.cover.is_some() { cover_line } else { None };
                write = out.write;
                block_action = out.block;
            }
        }
        if !write.is_empty() {
            // D7: ANY raw bytes headed for this PTY (keys, grid paste,
            // wheel-to-arrows, mouse reports) use the prompt episode and
            // dismiss an armed composer — before the send, so this frame's
            // routing is already truthful.
            if let Some(st) = self.composers.get_mut(&id) {
                st.on_raw_input(Instant::now());
            }
            // v0.1.1: freeze a pending prompt-end upgrade — the echo of
            // these bytes must never be folded into the captured prompt.
            if let Some(b) = self.terms.get_mut(&id) {
                b.note_input();
            }
            // P6b §5.2 observed-raw capture (the deliberate-yield path — with
            // post-submit typeahead, composed keys never reach here): a raw
            // Enter at a hooked cmd prompt is the only executions witness cmd
            // has (no exec hook; echo bytes are editing-key soup). Read the
            // FINAL rendered input row (reclaim_text — exact regardless of
            // editing keys) and record it write:false BEFORE the Enter ships,
            // so the synthetic block opens at the pre-echo journal head.
            // Best-effort by design: multi-line/mid-line/stale reclaims and
            // pre-P6b daemons skip the record, never the keystrokes.
            if self.family_is_cmd(id) && self.ipc.as_ref().is_some_and(|c| c.proto >= 6) {
                let observed = self.terms.get(&id).and_then(|b| {
                    let latched = b
                        .block_feed
                        .as_ref()
                        .is_some_and(|f| f.prompt_end.is_some());
                    if !latched || !composer::bytes_contain_enter(&write, b.win32_input) {
                        return None;
                    }
                    match b.reclaim_text() {
                        term_backend::Reclaim::Text(t) if !t.trim().is_empty() => {
                            Some(t.trim().to_string())
                        }
                        _ => None,
                    }
                });
                if let Some(cmdline) = observed {
                    self.send(C2D::SubmitCommand {
                        id,
                        cmd: cmdline,
                        write: false,
                    });
                }
            }
            // Chunked: `write` carries grid pastes (Ctrl+V on the raw grid),
            // which can be arbitrarily large.
            self.send_input(id, write);
        }
        if grid_clicked {
            // A click on the covered prompt row is a click on OUR prompt —
            // focus the editor. Any OTHER pointer interaction with the grid
            // (click, drag-select, wheel) leaves the composer exactly as it
            // was: pointer events never disarm (Bug 4 — selecting scrollback
            // used to blur to Raw(UserRaw) and consume the episode). A
            // pointer produces no PTY bytes at an idle prompt (MOUSE_MODE is
            // gate-blocked while armed), so yield-to-raw remains exclusively
            // the ⌨ toggle, Esc, and actual raw bytes headed for the PTY.
            let on_cover = cover_rect
                .zip(grid_click_pos)
                .is_some_and(|(r, p)| r.contains(p));
            if on_cover {
                if let Some(st) = self.composers.get_mut(&id) {
                    st.want_focus = true;
                }
            }
        }
        // QOL §6.1: middle-click release pastes the clipboard through the
        // mode router — Compose ⇒ draft, raw ⇒ the §5 gate then the PTY
        // (one gate, no bypass surface).
        if middle_paste {
            if let Some(text) = clipboard_text() {
                self.route_paste(id, &text);
            }
        }
        // QOL §5: a raw grid paste tripped the safety gate — nothing was
        // written; confirm first (re-encoded at confirm time).
        if let Some(text) = paste_pending {
            self.modal = Some(Modal::ConfirmPaste {
                id,
                text,
                dont_warn: false,
            });
        }

        // ── QOL §3: the terminal-area context menu, attached to the grid
        // response. A right-click forwarded to a MOUSE_MODE app never opens
        // it (R3 — gating the CALL means egui never sees that click);
        // Shift+right-click always menus. Menu widgets live in an egui menu
        // layer, so their clicks never reach the grid's raw-event input path
        // (blocks-panel precedent), and opening/clicking is pointer-only —
        // invariant 3: the composer episode is never consumed here. The menu
        // is content-scoped: lifecycle verbs stay on the sidebar row.
        if let Some(resp) = &grid_resp {
            if !rclick_forwarded {
                resp.context_menu(|ui| {
                    menu_item_style(ui);
                    ui.set_min_width(200.0);
                    let presented = self.presented(id);
                    let has_sel = self
                        .terms
                        .get(&id)
                        .is_some_and(|b| b.selection_text().is_some());
                    let last_closed = self.blocks.get(&id).and_then(|b| {
                        b.recs
                            .iter()
                            .rev()
                            .find(|r| r.end_off.is_some())
                            .map(|r| (r.start_off, r.cmd.clone()))
                    });
                    let gates = menu_gates(
                        presented,
                        self.can_rerun(id),
                        last_closed.is_some(),
                        self.resolve_local_cwd(id).is_some(),
                        self.terms.get(&id).map(|b| b.history_size()).unwrap_or(0),
                    );
                    // Copy — the §6 synthesized copy (menu Copy must match
                    // paint exactly like Ctrl+C does; DO-NOT 4).
                    if ui.add_enabled(has_sel, egui::Button::new("Copy")).clicked() {
                        if let Some(text) =
                            self.terms.get(&id).and_then(|b| b.selection_text())
                        {
                            ui.ctx().copy_text(text);
                        }
                        ui.close();
                    }
                    if ui.add_enabled(gates.paste, egui::Button::new("Paste")).clicked() {
                        if let Some(text) = clipboard_text() {
                            self.route_paste(id, &text);
                        }
                        ui.close();
                    }
                    // Copy stays a second, deliberate click (WT parity —
                    // DO-NOT 2: never auto-copy the Select all).
                    if ui.button("Select all").clicked() {
                        if let Some(b) = self.terms.get_mut(&id) {
                            b.select_all();
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Find").clicked() {
                        self.open_search();
                        ui.close();
                    }
                    if ui
                        .add_enabled(gates.open_cwd, egui::Button::new("Open cwd in Explorer"))
                        .clicked()
                    {
                        if let Some(dir) = self.resolve_local_cwd(id) {
                            // C6: a stale/removed dir clicks into nothing —
                            // say so instead.
                            let fail = if !dir.exists() {
                                Some(format!("Folder no longer exists: {}", dir.display()))
                            } else {
                                open::that_detached(&dir)
                                    .err()
                                    .map(|e| format!("Could not open {}: {e}", dir.display()))
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
                        ui.close();
                    }
                    let rerun_label = match &last_closed {
                        Some((_, cmd)) => {
                            format!("Rerun last: {}", middle_ellipsize(cmd, 32))
                        }
                        None => "Rerun last".to_string(),
                    };
                    // Extend, never wrap: a wrapped menu row reads as two
                    // items (the ellipsize already bounds the width).
                    if ui
                        .add_enabled(
                            gates.rerun,
                            egui::Button::new(rerun_label)
                                .wrap_mode(egui::TextWrapMode::Extend),
                        )
                        .clicked()
                    {
                        if let Some((off, _)) = last_closed {
                            self.rerun_block(id, off);
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(gates.clear, egui::Button::new("Clear scrollback"))
                        .clicked()
                    {
                        self.clear_scrollback(id);
                        ui.close();
                    }
                    // §6.2: the one visible entry point a settings-UI-free
                    // app has for the copy-on-select pref.
                    let mut cos = self.prefs.copy_on_select;
                    if ui.checkbox(&mut cos, "Copy on select").changed() {
                        self.prefs.copy_on_select = cos;
                        self.save_prefs();
                    }
                });
            }
        }

        // Composer strip (P3 §6.3). Present for hooked terminals in EVERY
        // state (alt-screen, Dead, busy) — the reservation is constant.
        if hooked {
            let strip_rect = Rect::from_min_max(
                Pos2::new(full.min.x, full.max.y - composer::STRIP_H),
                full.max,
            );
            // Lane cwd: feed-time hook cwd first (updates the frame the
            // fresh prompt renders — the stale-`cd` lane bug), Snapshot meta
            // as the fallback (cmd's static PROMPT carries no cwd payload;
            // its meta rides the daemon's prompt-time broadcast instead).
            let prompt_cwd = self
                .terms
                .get(&id)
                .and_then(|b| b.feed_cwd().map(str::to_owned))
                .or_else(|| {
                    // v0.1.1: the shared display rule — a WSL/ssh lane chip
                    // must never fall back to a `C:\` string.
                    self.state.terminal(id).map(|t| t.display_cwd())
                });
            let mut comp_write = Vec::new();
            let mut spacer_gesture = false;
            let mut toggle_history = false;
            let mut wake_clicked = false;
            let mut restore_clicked = false;
            let mut cancel_reconnect_clicked = false;
            self.history_btn_rect = None;
            if let (Some(st), Some(backend), Some(bl)) = (
                self.composers.get_mut(&id),
                self.terms.get(&id),
                self.blocks.get(&id),
            ) {
                let out = composer::show(
                    ui,
                    strip_rect,
                    grid_area,
                    id,
                    st,
                    backend,
                    &bl.recs,
                    bl.epoch,
                    running,
                    overlay_open,
                    font,
                    cover_line,
                    prompt_cwd.as_deref(),
                );
                st.has_focus = out.has_focus;
                comp_write = out.write;
                spacer_gesture = out.spacer_gesture;
                toggle_history = out.toggle_history;
                wake_clicked = out.wake;
                restore_clicked = out.restore;
                cancel_reconnect_clicked = out.cancel_reconnect;
                self.history_btn_rect = out.history_btn;
            }
            if wake_clicked || restore_clicked {
                // SLEEP: the strip's Wake ▸ — RestartTerminal IS wake (S5).
                // Dead lane's Restore ▸ is the same verb (restore = launch).
                self.send(C2D::RestartTerminal { id });
            }
            if cancel_reconnect_clicked && self.reconnect_supported() {
                self.send(C2D::CancelReconnect { id });
            }
            if toggle_history {
                if self.history.is_some() {
                    self.history = None;
                } else {
                    // One floating surface at a time (P4 §3.5 + selector §1.3).
                    self.search = None;
                    self.blocks_panel = None;
                    self.launcher = None;
                    self.history = Some(HistoryPopup::new());
                    // LOW-9: move egui focus to the popup's query field the
                    // instant it opens (the composer's arm-frame fix, same
                    // mechanism) — otherwise the grid/composer editor keeps
                    // focus until the popup's own request_focus runs at the
                    // END of this frame, and a fast keystroke leaks to the
                    // PTY or into the draft.
                    ui.ctx().memory_mut(|m| {
                        m.request_focus(Id::new(("history_query", id)))
                    });
                }
            }
            // The popup anchors above the strip; drawn after the strip so it
            // floats over the grid (Foreground order handles z).
            self.history_popup_ui(ui.ctx(), strip_rect, id, prompt_cwd.as_deref());
            if !comp_write.is_empty() {
                // Same post-send behavior as write_and_pin: submissions jump
                // the viewport back to the live bottom.
                if let Some(b) = self.terms.get_mut(&id) {
                    b.scroll_to_bottom();
                    // v0.1.1: composer-authored bytes (submission / chord)
                    // freeze a pending prompt-end upgrade like any input.
                    b.note_input();
                    // Empty-Enter "more lines" gesture: mark the row the
                    // composer just left a blank spacer cover the same frame
                    // the armed cover drops, so no raw `PS …>` flashes on it
                    // during the re-prompt round-trip.
                    if spacer_gesture {
                        b.mark_prompt_spacer();
                    }
                }
                // Chunked: a composer submission can carry a large paste.
                self.send_input(id, comp_write);
            }
            // P6b §5.2: a Cmd-family submission produced no bytes — it rides
            // the SubmitCommand ledger verb instead (the daemon writes AND
            // records). Exactly one of comp_write / the outbox is populated
            // per dispatch; SubmitHold/cover mechanics already ran in show().
            if let Some(cmdline) = self
                .composers
                .get_mut(&id)
                .and_then(|c| c.take_submit_cmd())
            {
                self.send_cmd_submission(id, cmdline);
            }
            ui.advance_cursor_after_rect(full);
        }
        match block_action {
            Some(term_view::BlockAction::CopyOutput(off)) => self.copy_block_output(id, off),
            Some(term_view::BlockAction::Rerun(off)) => self.rerun_block(id, off),
            None => {}
        }
    }

    /// Empty central panel. First run / zero terminals: the LAUNCHER CONTENT
    /// embeds inline (§6.1/D11) — no modal, no dead text; the selector is the
    /// first thing a new user sees. Terminals-exist-but-none-selected (rare
    /// transient): the old icon + text, button opens the launcher.
    pub(super) fn empty_state(&mut self, ui: &mut egui::Ui) {
        if self.state.terminals.is_empty() {
            if !self.launcher.as_ref().is_some_and(|l| l.embedded) {
                self.launcher = Some(self.fresh_launcher(None, true));
            }
            self.launcher_drift_rebuild();
            let avail = ui.available_size();
            let w = 560.0f32.min(avail.x - 32.0).max(200.0);
            ui.add_space(avail.y * 0.24);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Start a terminal")
                        .font(semibold(15.0))
                        .color(TEXT),
                );
            });
            ui.add_space(12.0);
            let full = ui.available_rect_before_wrap();
            let body = Rect::from_min_max(
                Pos2::new(full.center().x - w / 2.0, full.min.y),
                Pos2::new(full.center().x + w / 2.0, full.max.y - 12.0),
            );
            let mut st = self.launcher.take().expect("just ensured");
            let keys_enabled = self.modal.is_none();
            let out = {
                let vc = launcher::ViewCtx {
                    folders: &self.state.folders,
                    keys_enabled,
                    embedded: true,
                    width: w,
                    max_h: body.height() - 60.0,
                };
                let mut child = ui.new_child(
                    UiBuilder::new()
                        .max_rect(body)
                        .layout(egui::Layout::top_down(Align::Min)),
                );
                launcher::view(&mut child, &mut st, &vc)
            };
            self.launcher = Some(st);
            self.handle_launcher_out(out);
            return;
        }
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.32);
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::hover());
            draw_icon(ui.painter(), rect, Icon::Terminal, TEXT_FAINT);
            ui.add_space(14.0);
            ui.label(
                RichText::new("No terminal selected")
                    .font(semibold(15.0))
                    .color(TEXT_SECONDARY),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("Create a terminal, or pick one from the sidebar.")
                    .size(13.0)
                    .color(TEXT_MUTED),
            );
            ui.add_space(16.0);
            if primary_button(ui, "New terminal", false, true).clicked() {
                self.open_launcher(ui.ctx(), None);
            }
        });
    }

    /// Card dashboard (V-C): a responsive grid of live terminal previews,
    /// folder-scoped or all. Clicking a card selects that terminal.
    pub(super) fn dashboard(&mut self, ui: &mut egui::Ui, folder: Option<Uuid>) {
        // Scale-in / fade-in for the whole surface (V6).
        let anim = ui
            .ctx()
            .animate_bool_with_time(Id::new("dashboard-fade"), true, 0.12);
        ui.multiply_opacity(anim);
        if anim < 1.0 {
            ui.ctx().request_repaint();
        }

        // r4 perf-gui L2: derive the card order from the state_gen-keyed row
        // cache — same order as the old sorted_terminal_ids() by
        // construction — instead of a clone-sort + O(N²) meta finds per
        // painted frame.
        let ids: Vec<Uuid> = self
            .sidebar_rows_current()
            .iter()
            .filter(|t| match folder {
                Some(f) => t.folder == Some(f),
                None => true,
            })
            .map(|t| t.id)
            .collect();

        let scope_name = match folder {
            Some(f) => self
                .state
                .folders
                .iter()
                .find(|x| x.id == f)
                .map(|x| x.name.clone())
                .unwrap_or_else(|| "Folder".into()),
            None => "All terminals".into(),
        };

        // Header strip: Back + title + count. Spans the full central width
        // (§6.2 — the shrink-to-content chip read as floating debris).
        let hr = egui::Frame::new()
            .fill(SURFACE)
            .inner_margin(Margin { left: 8, right: 8, top: 0, bottom: 0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_height(40.0);
                ui.horizontal_centered(|ui| {
                    if ghost_button_auto(ui, "\u{2190} Back", TEXT_SECONDARY).clicked() {
                        self.central_view = CentralView::Terminal;
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new(&scope_name).font(semibold(15.0)).color(TEXT));
                    ui.label(
                        RichText::new(format!("\u{B7} {}", ids.len()))
                            .size(12.0)
                            .color(TEXT_MUTED),
                    );
                });
            });
        // No divider under the dashboard header (seamless doctrine).
        let _ = hr;

        let time = ui.input(|i| i.time);
        let mut clicked: Option<Uuid> = None;
        let mut restore: Option<Uuid> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(12.0);
                if ids.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(
                            RichText::new("No terminals in this view.")
                                .size(13.0)
                                .color(TEXT_MUTED),
                        );
                    });
                    return;
                }
                let gap = 12.0;
                let avail = (ui.available_width() - 2.0 * gap).max(200.0);
                let min_card = 300.0;
                let cols = (((avail + gap) / (min_card + gap)).floor() as usize).max(1);
                let card_w = (avail - gap * (cols as f32 - 1.0)) / cols as f32;
                let card_h = 168.0;
                let max_chars = ((card_w - 28.0) / 6.6) as usize;

                ui.horizontal(|ui| {
                    ui.add_space(gap);
                    ui.vertical(|ui| {
                        for chunk in ids.chunks(cols) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = gap;
                                for &id in chunk {
                                    match self.dashboard_card(ui, id, card_w, card_h, max_chars, time) {
                                        Some(CardAction::Select) => clicked = Some(id),
                                        Some(CardAction::Restore) => restore = Some(id),
                                        None => {}
                                    }
                                }
                            });
                            ui.add_space(gap);
                        }
                    });
                });
            });

        if let Some(id) = restore {
            self.send(C2D::RestartTerminal { id });
        }
        if let Some(id) = clicked {
            self.select_terminal(id);
        }
    }

    /// One dashboard card. Returns what the click asked for, if anything.
    /// `&mut self` only for the preview cache write.
    pub(super) fn dashboard_card(
        &mut self,
        ui: &mut egui::Ui,
        id: Uuid,
        w: f32,
        h: f32,
        max_chars: usize,
        time: f64,
    ) -> Option<CardAction> {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
        let hover_t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        let painter = ui.painter();
        // Card body: background shift only (hover brightens it) — no border
        // stroke (seamless doctrine).
        let fill = lerp_col(SURFACE, SURFACE_2, hover_t);
        painter.rect_filled(rect, CornerRadius::same(10), fill);

        let meta = self.state.terminal(id);
        let name = meta.map(|m| m.name.as_str()).unwrap_or("(gone)");
        let act = self.activity_of(id);
        let dead = meta.map(|m| m.status) == Some(TermStatus::Dead);
        let asleep = meta.is_some_and(|m| m.asleep);

        // Header row: activity dot + name (asleep = the moon, S14; its bite
        // takes the card's hover-lerped fill).
        let dot_c = Pos2::new(rect.min.x + 16.0, rect.min.y + 18.0);
        match act {
            Activity::Working => {
                let pulse = 0.75 + 0.25 * (time as f32 * std::f32::consts::TAU).sin();
                painter.circle_filled(dot_c, 4.0, ACCENT.gamma_multiply(pulse));
            }
            Activity::Idle => {
                painter.circle_filled(dot_c, 4.0, TEXT_MUTED);
            }
            Activity::NeedsYou => {
                painter.circle_filled(dot_c, 4.0, ATTENTION);
            }
            Activity::Asleep => {
                draw_moon(painter, dot_c, 4.5, TEXT_MUTED, fill);
            }
            Activity::Dead => {
                painter.circle_stroke(dot_c, 3.5, Stroke::new(1.5, TEXT_MUTED));
            }
        }
        painter.text(
            Pos2::new(rect.min.x + 28.0, rect.min.y + 18.0),
            Align2::LEFT_CENTER,
            middle_ellipsize(name, max_chars),
            semibold(13.0),
            TEXT,
        );

        // Meta line (§6.2): `cwd · last command` — the card names where it
        // is and what it last did. Omitted entirely when both are unknown.
        let mut line_y = rect.min.y + 36.0;
        let meta_line = {
            // v0.1.1: the shared display rule (display_cwd) — POSIX-namespace
            // sessions never show a `C:\` string.
            let cwd = meta.map(|m| m.display_cwd());
            let last_cmd = self
                .blocks
                .get(&id)
                .and_then(|b| b.recs.iter().rev().find(|r| r.end_off.is_some()))
                .map(|r| r.cmd.clone());
            match (cwd.filter(|c| !c.is_empty()), last_cmd) {
                (Some(c), Some(cmd)) => Some(format!(
                    "{} \u{B7} {}",
                    middle_ellipsize(&c, max_chars / 2),
                    cmd
                )),
                (Some(c), None) => Some(middle_ellipsize(&c, max_chars)),
                (None, Some(cmd)) => Some(cmd),
                (None, None) => None,
            }
        };
        if let Some(m) = meta_line {
            painter.text(
                Pos2::new(rect.min.x + 16.0, line_y),
                Align2::LEFT_TOP,
                middle_ellipsize(&m, max_chars),
                FontId::proportional(11.0),
                TEXT_FAINT,
            );
            line_y += 14.0;
        }

        // OSC title line (slides down when the meta line is present).
        let title = self.terms.get(&id).and_then(|b| b.title.clone());
        if let Some(t) = title.filter(|s| !s.is_empty()) {
            painter.text(
                Pos2::new(rect.min.x + 16.0, line_y),
                Align2::LEFT_TOP,
                middle_ellipsize(&t, max_chars),
                FontId::proportional(11.0),
                TEXT_SECONDARY,
            );
            line_y += 14.0;
        }

        // Preview: last non-blank grid rows, mono 11px. Composed text is
        // cached per card (see preview_key) — trim semantics identical:
        // trim_end + blank-row skip live in preview_lines, the per-line
        // max_chars cut here.
        if let Some(backend) = self.terms.get(&id) {
            let key = preview_key(backend, max_chars);
            let ppp = ui.ctx().pixels_per_point().to_bits();
            let cache = self.previews.entry(id).or_default();
            if cache.key != Some(key) {
                cache.text = backend
                    .preview_lines(6)
                    .iter()
                    .map(|l| l.chars().take(max_chars).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n");
                cache.key = Some(key);
                cache.galley = None;
            }
            // r4 perf-gui L2: paint the stored galley — `painter.text` would
            // re-clone + re-hash the whole preview String per card per frame.
            // LEFT_TOP anchor ⇒ galley at pos is byte-identical placement.
            let galley = match &cache.galley {
                Some((p, g)) if *p == ppp => g.clone(),
                _ => {
                    let g = painter.layout_no_wrap(
                        cache.text.clone(),
                        FontId::monospace(11.0),
                        TEXT_MUTED,
                    );
                    cache.galley = Some((ppp, g.clone()));
                    g
                }
            };
            painter.galley(
                Pos2::new(rect.min.x + 16.0, line_y.max(rect.min.y + 54.0)),
                galley,
                TEXT_MUTED,
            );
        }

        // Dead card: hover reveals a ↻ Restore ghost text-button
        // bottom-right (§6.2) — text that brightens, no box. An asleep card
        // (status Dead + flag) words it as the wake it is; the wire verb is
        // the same RestartTerminal either way (S3).
        let mut over_restore = false;
        if dead && hover_t > 0.02 {
            let galley = painter.layout_no_wrap(
                if asleep { "\u{21BB} Wake" } else { "\u{21BB} Restore" }.to_string(),
                FontId::proportional(12.0),
                ACCENT,
            );
            let rr = Rect::from_min_size(
                Pos2::new(
                    rect.max.x - galley.size().x - 16.0,
                    rect.max.y - galley.size().y - 12.0,
                ),
                galley.size(),
            );
            over_restore = ui
                .ctx()
                .pointer_latest_pos()
                .is_some_and(|p| rr.expand(6.0).contains(p));
            let col = if over_restore { ACCENT_HOVER } else { ACCENT };
            let col = col.gamma_multiply((hover_t * 1.2).min(1.0));
            painter.galley(rr.min, galley, col);
        }

        if resp.clicked() {
            if over_restore {
                return Some(CardAction::Restore);
            }
            return Some(CardAction::Select);
        }
        None
    }
}
