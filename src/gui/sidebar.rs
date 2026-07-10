//! Sidebar: rail, footer, tree, folder and terminal rows. Zero-behavior
//! split from gui/mod.rs.

use super::*;

impl App {
    pub(super) fn sidebar(&mut self, root: &mut egui::Ui) {
        // Animated collapse: full tree at 240px, slim status-dot rail at 44px.
        let target = if self.prefs.sidebar_collapsed { 44.0 } else { 240.0 };
        let width = root
            .ctx()
            .animate_value_with_time(Id::new("sidebar-width"), target, 0.15);
        let railed = self.prefs.sidebar_collapsed;
        // Boundary treatment (user: "we need divider for sidebar … line or
        // color shift"): the sidebar surface is LIFTED a step above the
        // terminal area's BG so the boundary reads as two surfaces meeting.
        let fill = BG_SIDEBAR_LIFT;

        egui::Panel::left("sidebar")
            .resizable(false)
            .default_size(width)
            .min_size(width)
            .max_size(width)
            .frame(
                egui::Frame::new()
                    .fill(fill)
                    .inner_margin(if railed { Margin::same(4) } else { Margin::same(8) })
                    .stroke(Stroke::NONE),
            )
            .show(root, |ui| {
                if railed {
                    // The rail accepts no drops (§5.5) and hosts no rows to
                    // drag from — collapse any in-flight drag.
                    self.drag = None;
                    self.drop_rows.clear();
                    // Rail footer: just the daemon dot, centered.
                    let connected = self.ipc.as_ref().is_some_and(|c| c.is_connected());
                    egui::Panel::bottom("sidebar-footer")
                        .frame(egui::Frame::new().fill(fill).inner_margin(Margin::same(2)))
                        .show(ui, |ui| {
                            // Settings glyph stacked above the dot (task #33
                            // §2.2 — the rail's whisper-quiet entry point);
                            // the #34 update glyph (accent ↑, absent while
                            // idle) stacks above that.
                            ui.with_layout(
                                Layout::top_down(Align::Center),
                                |ui| {
                                    self.rail_update_glyph(ui);
                                    if footer_glyph(ui, Icon::Sliders)
                                        .on_hover_text("Settings (Ctrl+,)")
                                        .clicked()
                                    {
                                        self.open_settings();
                                    }
                                },
                            );
                            let (dr, dresp) = ui
                                .allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
                            ui.painter().circle_filled(
                                Pos2::new(dr.center().x, dr.center().y),
                                4.0,
                                if connected { SUCCESS } else { DANGER },
                            );
                            dresp.on_hover_text(if connected {
                                "Daemon connected"
                            } else {
                                "Daemon unreachable"
                            });
                        });
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add_space(4.0);
                            self.sidebar_rail(ui);
                        });
                    return;
                }

                // Whisper-quiet footer pinned to the bottom (V-B). The primary
                // actions (New terminal / folder / import) now live in the
                // titlebar strip, so the sidebar body is pure tree + footer.
                egui::Panel::bottom("sidebar-footer")
                    .frame(
                        egui::Frame::new()
                            .fill(fill)
                            .inner_margin(Margin { left: 2, right: 2, top: 4, bottom: 2 }),
                    )
                    .show(ui, |ui| {
                        // #34 Axis 5: the quiet update row directly above the
                        // footer cluster — absent entirely while idle.
                        self.sidebar_update_row(ui);
                        self.sidebar_footer(ui);
                    });

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        self.sidebar_tree(ui);
                        // Bug B: edge-band autoscroll keeps off-screen drop
                        // targets reachable while a drag is armed.
                        self.drag_autoscroll(ui);
                    });
            });

    }

    /// Collapsed sidebar: one activity dot per terminal, folder groups separated
    /// by small gaps. Tooltip carries the name; click selects; the selected
    /// terminal gets a slim accent bar on the rail edge.
    pub(super) fn sidebar_rail(&mut self, ui: &mut egui::Ui) {
        // Rail header, one-column doctrine: everything centers on the same
        // vertical axis as the dot rows. The expanded titlebar cluster keeps
        // its toggle at x=36 — past a 44px rail's useful span — so while
        // railed the toggle stacks HERE, centered under the titlebar's app
        // mark, and returns to the titlebar on expand (same flag flips both,
        // so no frame draws two toggles mid-animation).
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            if footer_glyph(ui, Icon::Sidebar)
                .on_hover_text("Expand sidebar")
                .clicked()
            {
                self.prefs.sidebar_collapsed = false;
                self.save_prefs();
            }
            ui.add_space(6.0);
            // Rail + (§5.6): instant create pinned at the rail's top, above
            // the first dot. The launcher needs width, so the rail's + never
            // opens it — the tooltip carries the sticky-spawn preview
            // instead, built LAZILY inside on_hover_ui (r4 perf-gui M1: the
            // SpawnSpec clone + format! must not run on every painted rail
            // frame).
            let plus = footer_glyph(ui, Icon::Plus).on_hover_ui(|ui| {
                ui.label(format!(
                    "New terminal \u{2014} {}",
                    launcher::spawn_preview(&self.effective_last_spawn())
                ));
            });
            if plus.clicked() {
                self.instant_create(ui.ctx(), None);
            }
        });
        ui.add_space(6.0);

        let time = ui.input(|i| i.time);
        // r4 perf-gui M1: ride the state_gen-keyed row cache like the tree
        // (round-1 HIGH-1). `rows.iter()` (groups flattened, then loose) is
        // byte-identical to the old sorted_terminal_ids() order by
        // construction — same sort key `order` alone (D6), same
        // dangling-folder rule — without the per-frame folder clone-sort,
        // O(N²) meta finds, and per-row TerminalMeta deep clones.
        let rows = self.sidebar_rows_current();
        let mut prev_folder: Option<Option<Uuid>> = None;
        for t in rows.iter() {
            let id = t.id;
            // Gap between folder groups.
            if prev_folder.is_some() && prev_folder != Some(t.folder) {
                ui.add_space(10.0);
            }
            prev_folder = Some(t.folder);

            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::click());
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            let selected = self.selected == Some(id);
            let hover_t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
            let painter = ui.painter();
            if hover_t > 0.02 {
                painter.rect_filled(
                    rect,
                    CornerRadius::same(6),
                    Color32::from_rgba_unmultiplied(255, 255, 255, (10.0 * hover_t) as u8),
                );
            }
            if selected {
                let bar = Rect::from_min_max(
                    Pos2::new(rect.min.x + 1.0, rect.min.y + 6.0),
                    Pos2::new(rect.min.x + 3.0, rect.max.y - 6.0),
                );
                painter.rect_filled(bar, CornerRadius::same(1), ACCENT);
            }
            let dot_c = rect.center();
            match self.activity_from_meta(t) {
                Activity::Working => {
                    let pulse = 0.75 + 0.25 * (time as f32 * std::f32::consts::TAU).sin();
                    painter.circle_filled(dot_c, 7.0, ACCENT.gamma_multiply(0.20 * pulse));
                    painter.circle_filled(dot_c, 4.5, ACCENT.gamma_multiply(pulse));
                }
                Activity::Idle => {
                    painter.circle_filled(dot_c, 4.5, TEXT_MUTED);
                }
                Activity::NeedsYou => {
                    painter.circle_filled(dot_c, 7.0, ATTENTION.gamma_multiply(0.22));
                    painter.circle_filled(dot_c, 4.5, ATTENTION);
                }
                Activity::Asleep => {
                    // Rail rows: moon over the hover-lerped rail fill (S14).
                    let row_bg = composite_over(
                        BG_SIDEBAR_LIFT,
                        Color32::from_rgba_unmultiplied(
                            255,
                            255,
                            255,
                            (10.0 * hover_t) as u8,
                        ),
                    );
                    draw_moon(painter, dot_c, 5.0, TEXT_MUTED, row_bg);
                }
                Activity::Dead => {
                    painter.circle_stroke(dot_c, 4.0, Stroke::new(1.5, TEXT_MUTED));
                }
            }
            // Unread tick, top-right of the dot cell.
            if self.unread.contains(&id) && !selected {
                painter.circle_filled(Pos2::new(rect.max.x - 8.0, rect.min.y + 8.0), 2.5, ACCENT);
            }

            let resp = resp.on_hover_text(&t.name);
            if resp.clicked() {
                self.select_terminal(id);
            }
        }
    }

    /// Bottom-of-sidebar status cluster (V-B): daemon dot, font steppers,
    /// density toggle, version. 24px tall, muted 11px, no borders.
    pub(super) fn sidebar_footer(&mut self, ui: &mut egui::Ui) {
        let connected = self.ipc.as_ref().is_some_and(|c| c.is_connected());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.set_height(24.0);

            // Daemon status dot.
            let (r, dot_resp) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
            let dot = if connected { SUCCESS } else { DANGER };
            ui.painter().circle_filled(r.center(), 4.0, dot);
            dot_resp.on_hover_text(if connected {
                "Daemon connected"
            } else {
                "Daemon unreachable"
            });

            // Font size steppers.
            if footer_glyph(ui, Icon::Minus)
                .on_hover_text("Smaller font")
                .clicked()
            {
                self.font_step(-1.0);
            }
            ui.label(
                RichText::new(format!("{}px", self.prefs.font_size as i32))
                    .size(11.0)
                    .color(TEXT_MUTED),
            );
            if footer_glyph(ui, Icon::Plus)
                .on_hover_text("Larger font")
                .clicked()
            {
                self.font_step(1.0);
            }

            // Density toggle.
            let dens_icon = if self.prefs.compact {
                Icon::DensityCompact
            } else {
                Icon::DensityComfortable
            };
            if footer_glyph(ui, dens_icon)
                .on_hover_text(if self.prefs.compact {
                    "Compact rows \u{2014} click for comfortable"
                } else {
                    "Comfortable rows \u{2014} click for compact"
                })
                .clicked()
            {
                self.prefs.compact = !self.prefs.compact;
                self.save_prefs();
            }

            // Version + settings glyph, right-aligned (first added =
            // rightmost; the glyph sits visually left of the version).
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .size(11.0)
                        .color(TEXT_FAINT),
                );
                if footer_glyph(ui, Icon::Sliders)
                    .on_hover_text("Settings (Ctrl+,)")
                    .clicked()
                {
                    self.open_settings();
                }
            });
        });
    }

    pub(super) fn sidebar_tree(&mut self, ui: &mut egui::Ui) {
        // Drag lifecycle first (§5.5): resolve Esc/release against LAST
        // frame's slot map, then rebuild it while painting this frame.
        self.drag_lifecycle(ui.ctx());
        self.drop_rows.clear();

        // Snapshot-generation-keyed rows (see build_sidebar_rows for the D6
        // ordering contract) — no per-frame clones or sorts.
        let rows = self.sidebar_rows_current();

        for (gi, folder) in rows.folders.iter().enumerate() {
            let terms = &rows.groups[gi];

            // SLEEP: presented member states drive the folder badge + the
            // Sleep all / Wake all menu rows (S16).
            let asleep_n = terms
                .iter()
                .filter(|t| t.asleep)
                .count();
            let any_running = terms
                .iter()
                .any(|t| presented_status(t.status, t.asleep) == PresentedStatus::Running);
            let any_asleep = terms
                .iter()
                .any(|t| presented_status(t.status, t.asleep) == PresentedStatus::Asleep);
            let (resp, action) = self.folder_row(ui, folder, terms.len(), asleep_n, gi);
            match action {
                FolderAction::Delete => {
                    self.modal = Some(Modal::ConfirmDeleteFolder(folder.id));
                }
                FolderAction::ToggleCollapse => {
                    self.send(C2D::SetFolderCollapsed {
                        id: folder.id,
                        collapsed: !folder.collapsed,
                    });
                }
                FolderAction::Dashboard => {
                    self.enter_dashboard(Some(folder.id));
                }
                FolderAction::Rename => {
                    self.start_rename(
                        RenameTarget::Folder(folder.id),
                        folder.name.clone(),
                        RenameHost::Row,
                    );
                }
                FolderAction::None => {}
            }
            let colors_ok = self.color_tags_supported();
            resp.context_menu(|ui| {
                menu_item_style(ui);
                if ui.button("New terminal here\u{2026}").clicked() {
                    // Launcher with the footer folder chip preset (§5.3/§4.2).
                    self.open_launcher(ui.ctx(), Some(folder.id));
                    ui.close();
                }
                if ui.button("Rename").clicked() {
                    self.start_rename(
                        RenameTarget::Folder(folder.id),
                        folder.name.clone(),
                        RenameHost::Row,
                    );
                    ui.close();
                }
                if colors_ok {
                    if let Some(pick) = color_tag_menu(ui, folder.color_tag) {
                        self.send(C2D::SetFolderColor { id: folder.id, tag: pick });
                        ui.close();
                    }
                }
                if ui.button("Move up").clicked() {
                    self.send(C2D::MoveFolder { id: folder.id, delta: -1 });
                    ui.close();
                }
                if ui.button("Move down").clicked() {
                    self.send(C2D::MoveFolder { id: folder.id, delta: 1 });
                    ui.close();
                }
                // SLEEP §7.1/§8: folder sleep is ALWAYS a confirm modal (the
                // blind bulk act — it lists every target); folder wake is
                // additive (nothing can be lost) so it fires directly.
                if self.sleep_supported() && (any_running || any_asleep) {
                    ui.separator();
                    if any_running && ui.button("Sleep all").clicked() {
                        self.modal = Some(Modal::ConfirmSleepFolder(folder.id));
                        ui.close();
                    }
                    if any_asleep && ui.button("Wake all").clicked() {
                        self.send(C2D::WakeFolder { folder: folder.id });
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button(RichText::new("Delete folder").color(RED)).clicked() {
                    self.modal = Some(Modal::ConfirmDeleteFolder(folder.id));
                    ui.close();
                }
            });

            if !folder.collapsed {
                for (i, t) in terms.iter().enumerate() {
                    self.terminal_row(ui, t, Some(folder.id), i);
                }
            }
            ui.add_space(2.0);
        }

        // Bug B: while a TERMINAL drag is armed the UNGROUPED section is
        // always revealed (TEXT_FAINT when it holds no rows — signal, not
        // decoration) and registers as a drop band, so a terminal can
        // always be dragged out of every folder.
        let term_drag = matches!(
            &self.drag,
            Some(d) if matches!(d.payload, DragPayload::Term { .. })
        );
        if !rows.loose.is_empty() || (term_drag && !rows.folders.is_empty()) {
            if !rows.folders.is_empty() {
                let col = if rows.loose.is_empty() { TEXT_FAINT } else { TEXT_MUTED };
                let hdr = section_header_col(ui, "UNGROUPED", col);
                if term_drag {
                    self.drop_rows.push(DropRow::LooseZone { rect: hdr });
                }
            }
            for (i, t) in rows.loose.iter().enumerate() {
                self.terminal_row(ui, t, None, i);
            }
        }
        // Bug B: the empty tail below the last row is a "move out of
        // folders" band too — a release there used to cancel silently.
        if term_drag {
            let top = ui.cursor().top();
            let bottom = ui.clip_rect().bottom();
            if bottom - top > 6.0 {
                let x = ui.max_rect().x_range();
                self.drop_rows.push(DropRow::LooseZone {
                    rect: Rect::from_min_max(
                        Pos2::new(x.min, top),
                        Pos2::new(x.max, bottom),
                    ),
                });
            }
        }

        // Armed drag: hovered-slot marker + pointer ghost, over the rows.
        if self.drag.is_some() {
            self.paint_drag_feedback(ui);
        }

        if self.state.terminals.is_empty() && self.state.folders.is_empty() {
            // §6.1: one clickable, zero prose — the button focuses the
            // embedded launcher's query in the central panel.
            ui.add_space(12.0);
            if ghost_button(ui, ui.available_width(), "New terminal", Some(Icon::Plus)).clicked()
            {
                ui.ctx()
                    .memory_mut(|m| m.request_focus(Id::new("launcher_q")));
            }
        }
    }

    /// Folder row (D21): painted rotating chevron, name, count. No status dot.
    /// Clicking the chevron zone collapses; clicking the name opens the folder
    /// dashboard (V-C); hover reveals ✕ (delete) and ✏ (inline rename, §5.4).
    /// A terminal drag hovering the row is a move-into target (§5.5); the row
    /// itself arms a folder-reorder drag (Bug B). `gi` is the painted folder
    /// ordinal (order-sorted).
    pub(super) fn folder_row(
        &mut self,
        ui: &mut egui::Ui,
        folder: &crate::state::Folder,
        count: usize,
        asleep_n: usize,
        gi: usize,
    ) -> (egui::Response, FolderAction) {
        let (rect, resp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), 30.0),
            Sense::click_and_drag(),
        );
        // Drag arming (Bug B, §5.5 grammar): egui's decided-drag threshold
        // (~6px of travel) keeps clicks, the chevron/✕/✏ zones, and context
        // menus unaffected. Ghost = dot-less "name · count".
        if resp.drag_started() && self.drag.is_none() && self.renaming.is_none() {
            let grab = resp
                .interact_pointer_pos()
                .map(|p| p - rect.min)
                .unwrap_or(Vec2::new(20.0, 15.0));
            self.drag = Some(DragState {
                payload: DragPayload::Folder { id: folder.id },
                name: format!("{} \u{b7} {}", folder.name, count),
                dot: TEXT_MUTED,
                grab,
            });
        }
        let dragging = self.drag.is_some();
        let drag_source = matches!(
            &self.drag,
            Some(DragState { payload: DragPayload::Folder { id }, .. }) if *id == folder.id
        );
        let t = ui.ctx().animate_bool_with_time(
            resp.id,
            resp.hovered() && !dragging,
            0.12,
        );
        let close_rect =
            Rect::from_center_size(Pos2::new(rect.max.x - 13.0, rect.center().y), Vec2::splat(22.0));
        let pencil_rect = close_rect.translate(Vec2::new(-22.0, 0.0));
        let pointer = ui.ctx().pointer_latest_pos();
        let over_close = resp.hovered() && pointer.is_some_and(|p| close_rect.contains(p));
        let over_pencil = resp.hovered() && pointer.is_some_and(|p| pencil_rect.contains(p));
        // Owned painter: the rename editor below needs `&mut ui`.
        let painter = ui.painter().clone();
        if t > 0.0 {
            painter.rect_filled(rect, CornerRadius::same(6), OV_HOVER.gamma_multiply(t.min(1.0)));
        }
        // Color tag (task #22): a 3px bar hugging the left edge — the
        // selection-bar grammar. Untagged rows are pixel-identical.
        if let Some(col) = tag_color(folder.color_tag) {
            let bar = Rect::from_min_max(
                Pos2::new(rect.min.x, rect.min.y + 4.0),
                Pos2::new(rect.min.x + 3.0, rect.max.y - 4.0),
            );
            painter.rect_filled(bar, CornerRadius::same(1), col);
        }
        // Chevron: right when collapsed, down when open.
        let cc = Pos2::new(rect.min.x + 12.0, rect.center().y);
        let stroke = Stroke::new(1.5, TEXT_MUTED);
        if folder.collapsed {
            painter.line_segment([cc + Vec2::new(-2.0, -4.0), cc + Vec2::new(3.0, 0.0)], stroke);
            painter.line_segment([cc + Vec2::new(3.0, 0.0), cc + Vec2::new(-2.0, 4.0)], stroke);
        } else {
            painter.line_segment([cc + Vec2::new(-4.0, -2.0), cc + Vec2::new(0.0, 3.0)], stroke);
            painter.line_segment([cc + Vec2::new(0.0, 3.0), cc + Vec2::new(4.0, -2.0)], stroke);
        }
        let renaming_here = matches!(
            &self.renaming,
            Some(rn) if rn.target == RenameTarget::Folder(folder.id) && rn.host == RenameHost::Row
        );
        if renaming_here {
            let er = Rect::from_min_max(
                Pos2::new(rect.min.x + 24.0, rect.center().y - 11.0),
                Pos2::new(rect.max.x - 48.0, rect.center().y + 11.0),
            );
            self.rename_editor(ui, er, semibold(13.0));
        } else {
            painter.text(
                Pos2::new(rect.min.x + 26.0, rect.center().y),
                Align2::LEFT_CENTER,
                &folder.name,
                semibold(13.0),
                TEXT,
            );
        }
        // Right side on hover: ✕ (delete) and ✏ (rename); the terminal count
        // at rest. Suppressed while a drag is armed (the row is a target).
        if t > 0.02 && !dragging {
            if over_close {
                painter.rect_filled(close_rect, CornerRadius::same(6), danger_wash(t));
            }
            let col = if over_close { DANGER_HOVER } else { TEXT_MUTED };
            draw_icon(
                &painter,
                close_rect.shrink(7.0),
                Icon::Close,
                col.gamma_multiply(t.min(1.0)),
            );
            let pcol = if over_pencil { TEXT } else { TEXT_MUTED };
            draw_icon(
                &painter,
                pencil_rect.shrink(7.0),
                Icon::Pencil,
                pcol.gamma_multiply(t.min(1.0)),
            );
        } else if !dragging {
            if asleep_n > 0 {
                // SLEEP §7.1: the count slot becomes a `☾ n` badge while any
                // member sleeps (n = asleep members; full-sleep shows the
                // member count). Painter moon — never a font glyph (S14).
                let ng = painter.layout_no_wrap(
                    asleep_n.to_string(),
                    FontId::proportional(11.0),
                    TEXT_MUTED,
                );
                let nw = ng.size().x;
                painter.galley(
                    Pos2::new(rect.max.x - 8.0 - nw, rect.center().y - ng.size().y / 2.0),
                    ng,
                    TEXT_MUTED,
                );
                let row_bg =
                    composite_over(BG_SIDEBAR_LIFT, OV_HOVER.gamma_multiply(t.min(1.0)));
                draw_moon(
                    &painter,
                    Pos2::new(rect.max.x - 14.0 - nw, rect.center().y),
                    4.0,
                    TEXT_MUTED,
                    row_bg,
                );
            } else {
                painter.text(
                    Pos2::new(rect.max.x - 8.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    count.to_string(),
                    FontId::proportional(11.0),
                    TEXT_MUTED,
                );
            }
        }
        // Drop-slot map (§5.5): for terminal drags the whole folder row is a
        // move-into target (collapsed or not); for folder drags it splits at
        // the midline into reorder slots (Bug B) — `slot_hit` picks per
        // payload.
        if dragging {
            self.drop_rows.push(DropRow::Folder { rect, id: folder.id, idx: gi });
        }
        // Source folder dims like source terminal rows while its ghost
        // rides the pointer (Bug B).
        if drag_source {
            painter.rect_filled(rect, CornerRadius::same(6), BG_SIDEBAR_LIFT.gamma_multiply(0.6));
        }
        let action = if resp.clicked() {
            match resp.interact_pointer_pos() {
                Some(p) if close_rect.contains(p) => FolderAction::Delete,
                Some(p) if pencil_rect.contains(p) => FolderAction::Rename,
                // Left ~24px is the chevron affordance; the rest is the name.
                Some(p) if p.x < rect.min.x + 24.0 => FolderAction::ToggleCollapse,
                Some(_) => FolderAction::Dashboard,
                None => FolderAction::None,
            }
        } else {
            FolderAction::None
        };
        (resp, action)
    }

    pub(super) fn terminal_row(
        &mut self,
        ui: &mut egui::Ui,
        t: &crate::state::TerminalMeta,
        group: Option<Uuid>,
        idx: usize,
    ) {
        let indent = group.is_some();
        let selected = self.selected == Some(t.id);
        let unread = self.unread.contains(&t.id);

        let act = self.activity_from_meta(t);
        let bursts = self.activity.get(&t.id).map(|s| s.bursts).unwrap_or(0);
        // LOW-7: one clone of the OSC title (it was cloned here and cloned
        // AGAIN at the sub-line below). An empty title is normalized to None
        // up front so the display arm neither re-filters nor re-clones.
        let title: Option<String> = self
            .terms
            .get(&t.id)
            .and_then(|b| b.title.clone())
            .filter(|s| !s.is_empty());
        let compact = self.prefs.compact;
        let row_h = if compact { 30.0 } else { 46.0 };
        let time = ui.input(|i| i.time);

        let (rect, resp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), row_h),
            Sense::click_and_drag(),
        );
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);

        // Drag arming (§5.5): egui's decided-drag threshold (~6px of travel)
        // keeps plain clicks and context menus unaffected.
        if resp.drag_started() && self.drag.is_none() && self.renaming.is_none() {
            let grab = resp
                .interact_pointer_pos()
                .map(|p| p - rect.min)
                .unwrap_or(Vec2::new(20.0, 14.0));
            let dot = match act {
                Activity::Working => ACCENT,
                Activity::NeedsYou => ATTENTION,
                Activity::Idle | Activity::Asleep | Activity::Dead => TEXT_MUTED,
            };
            self.drag = Some(DragState {
                payload: DragPayload::Term { id: t.id, from: t.folder },
                name: t.name.clone(),
                dot,
                grab,
            });
        }
        let dragging = self.drag.is_some();
        let drag_source = matches!(
            &self.drag,
            Some(DragState { payload: DragPayload::Term { id, .. }, .. }) if *id == t.id
        );

        let hover_t = ui.ctx().animate_bool_with_time(
            resp.id,
            resp.hovered() && !dragging,
            0.12,
        );
        let sel_t = ui.ctx().animate_bool_with_time(
            resp.id.with("sel"),
            selected,
            0.12,
        );
        // Owned painter: the rename editor below needs `&mut ui`.
        let painter = ui.painter().clone();

        if sel_t > 0.0 {
            painter.rect_filled(rect, CornerRadius::same(6), ACCENT_SUBTLE.gamma_multiply(sel_t));
            // 2px accent vertical bar at the left, inset 3px, rounded (D19/D38).
            let bar = Rect::from_min_max(
                Pos2::new(rect.min.x + 3.0, rect.min.y + 4.0),
                Pos2::new(rect.min.x + 5.0, rect.max.y - 4.0),
            );
            painter.rect_filled(bar, CornerRadius::same(1), ACCENT.gamma_multiply(sel_t));
        } else if hover_t > 0.0 {
            painter.rect_filled(rect, CornerRadius::same(6), OV_HOVER.gamma_multiply(hover_t.min(1.0)));
        }
        // NeedsYou: a faint amber indicator bar hugging the very left edge
        // (V-A) — the signal wins the edge over a color tag while latched.
        if act == Activity::NeedsYou {
            let bar = Rect::from_min_max(
                Pos2::new(rect.min.x, rect.min.y + 4.0),
                Pos2::new(rect.min.x + 2.0, rect.max.y - 4.0),
            );
            painter.rect_filled(bar, CornerRadius::same(1), ATTENTION.gamma_multiply(0.55));
        } else if let Some(col) = tag_color(t.color_tag) {
            // Color tag (task #22): 3px identity bar, selection-bar grammar.
            // Untagged rows are pixel-identical to before the feature.
            let bar = Rect::from_min_max(
                Pos2::new(rect.min.x, rect.min.y + 4.0),
                Pos2::new(rect.min.x + 3.0, rect.max.y - 4.0),
            );
            painter.rect_filled(bar, CornerRadius::same(1), col);
        }

        let x0 = rect.min.x + 8.0 + if indent { 12.0 } else { 0.0 };
        // Vertical anchors: single centred line when compact, else two lines.
        let (line1_y, line2_y) = if compact {
            (rect.center().y, rect.center().y)
        } else {
            (rect.min.y + 16.0, rect.min.y + 33.0)
        };

        // State dot (V-A): Working pulses, Idle is steady muted, NeedsYou is
        // amber, Asleep is the crescent moon (SLEEP S14 — painter-drawn, its
        // bite filled with THIS row's current computed fill so it stays a
        // crescent under hover/selection), Dead is a hollow ring.
        let dot_c = Pos2::new(x0 + 4.0, line1_y);
        match act {
            Activity::Working => {
                let pulse = 0.75 + 0.25 * (time as f32 * std::f32::consts::TAU).sin();
                painter.circle_filled(dot_c, 6.0, ACCENT.gamma_multiply(0.20 * pulse));
                painter.circle_filled(dot_c, 4.0, ACCENT.gamma_multiply(pulse));
            }
            Activity::Idle => {
                painter.circle_filled(dot_c, 4.0, TEXT_MUTED);
            }
            Activity::NeedsYou => {
                painter.circle_filled(dot_c, 6.0, ATTENTION.gamma_multiply(0.22));
                painter.circle_filled(dot_c, 4.0, ATTENTION);
            }
            Activity::Asleep => {
                let row_bg = if sel_t > 0.0 {
                    composite_over(BG_SIDEBAR_LIFT, ACCENT_SUBTLE.gamma_multiply(sel_t))
                } else if hover_t > 0.0 {
                    composite_over(BG_SIDEBAR_LIFT, OV_HOVER.gamma_multiply(hover_t.min(1.0)))
                } else {
                    BG_SIDEBAR_LIFT
                };
                draw_moon(&painter, dot_c, 4.5, TEXT_MUTED, row_bg);
            }
            Activity::Dead => {
                painter.circle_stroke(dot_c, 3.5, Stroke::new(1.5, TEXT_MUTED));
            }
        }

        let presented = presented_status(t.status, t.asleep);
        let asleep_row = matches!(
            presented,
            PresentedStatus::Asleep | PresentedStatus::Sleeping
        );
        let dead = t.status == TermStatus::Dead;

        // Hover action cluster geometry (§5.2), right-to-left: ✕ (delete,
        // confirm), ✏ (inline rename), and for dead rows ↻ (Restore) in the
        // slot nearest the text — revival is the easy target. 22px hit
        // rects, adjacent cells.
        let close_rect =
            Rect::from_center_size(Pos2::new(rect.max.x - 13.0, line1_y), Vec2::splat(22.0));
        let pencil_rect = close_rect.translate(Vec2::new(-22.0, 0.0));
        let restore_rect = pencil_rect.translate(Vec2::new(-22.0, 0.0));
        let cluster_left = if dead { restore_rect.min.x } else { pencil_rect.min.x };

        let renaming_here = matches!(
            &self.renaming,
            Some(rn) if rn.target == RenameTarget::Term(t.id) && rn.host == RenameHost::Row
        );
        if renaming_here {
            let er = Rect::from_min_max(
                Pos2::new(x0 + 14.0, line1_y - 11.0),
                Pos2::new(rect.max.x - 8.0, line1_y + 11.0),
            );
            self.rename_editor(ui, er, FontId::proportional(13.0));
        } else {
            // SLEEP §7.1: asleep rows read dormant — name drops to muted
            // (hover lifts it only to secondary, never full brightness).
            let (name_col, hover_col) = if asleep_row {
                (TEXT_MUTED, TEXT_SECONDARY)
            } else if selected {
                (TEXT, TEXT)
            } else {
                (TEXT_SECONDARY, TEXT)
            };
            painter.text(
                Pos2::new(x0 + 16.0, line1_y),
                Align2::LEFT_CENTER,
                &t.name,
                FontId::proportional(13.0),
                lerp_col(name_col, hover_col, hover_t),
            );
        }

        // Second line (comfortable only): what the terminal says it's doing
        // (OSC title), else how long it's been quiet (V-B). Dead rows say a
        // static, honest "exited" — no persisted death-time exists and the
        // idle timer was lying (F13). Asleep rows say so (S14 — dormant, not
        // died; the Sleeping transient names the drain).
        if !compact {
            let sub = match presented {
                PresentedStatus::Asleep => "asleep".to_string(),
                PresentedStatus::Sleeping => "sleeping\u{2026}".to_string(),
                // SSH auto-reconnect supervision in progress: the row says
                // so through the between-attempt Dead transients AND the
                // in-flight attempt (status flaps; the flag doesn't).
                PresentedStatus::Dead if t.reconnecting => "reconnecting\u{2026}".to_string(),
                PresentedStatus::Dead => "exited".to_string(),
                PresentedStatus::Running if t.reconnecting => "reconnecting\u{2026}".to_string(),
                PresentedStatus::Running => {
                    title.unwrap_or_else(|| self.idle_label(t.id))
                }
            };
            let sub = middle_ellipsize(&sub, 34);
            painter.text(
                Pos2::new(x0 + 16.0, line2_y),
                Align2::LEFT_CENTER,
                sub,
                FontId::proportional(11.0),
                TEXT_MUTED,
            );
        }

        // Right side: hover action cluster, else the burst badge / unread
        // dot (the badge yields while hovered — exactly the old rule).
        let pointer = ui.ctx().pointer_latest_pos();
        let over_close = resp.hovered() && pointer.is_some_and(|p| close_rect.contains(p));
        let over_pencil = resp.hovered() && pointer.is_some_and(|p| pencil_rect.contains(p));
        let over_restore =
            dead && resp.hovered() && pointer.is_some_and(|p| restore_rect.contains(p));
        if hover_t > 0.02 && !dragging {
            if over_close {
                painter.rect_filled(close_rect, CornerRadius::same(6), danger_wash(hover_t));
            }
            let col = if over_close { DANGER_HOVER } else { TEXT_MUTED };
            draw_icon(
                &painter,
                close_rect.shrink(7.0),
                Icon::Close,
                col.gamma_multiply(hover_t.min(1.0)),
            );
            let pcol = if over_pencil { TEXT } else { TEXT_MUTED };
            draw_icon(
                &painter,
                pencil_rect.shrink(7.0),
                Icon::Pencil,
                pcol.gamma_multiply(hover_t.min(1.0)),
            );
            if dead {
                let rcol = if over_restore { TEXT } else { TEXT_MUTED };
                draw_icon(
                    &painter,
                    restore_rect.shrink(7.0),
                    Icon::Rerun,
                    rcol.gamma_multiply(hover_t.min(1.0)),
                );
            }
        } else if !compact && bursts > 0 {
            // Burst-count pill (V-B): count of unread output bursts, capped 9+.
            let label = if bursts > 9 { "9+".to_string() } else { bursts.to_string() };
            let is_amber = act == Activity::NeedsYou;
            burst_badge(&painter, Pos2::new(rect.max.x - 10.0, line1_y), &label, is_amber);
        } else if compact && unread {
            painter.circle_filled(Pos2::new(rect.max.x - 10.0, line1_y), 3.0, ACCENT);
        }

        if resp.clicked() {
            let pos = resp.interact_pointer_pos();
            if pos.is_some_and(|p| close_rect.contains(p)) {
                self.modal = Some(Modal::ConfirmDeleteTerminal(t.id));
            } else if pos.is_some_and(|p| pencil_rect.contains(p)) {
                self.start_rename(RenameTarget::Term(t.id), t.name.clone(), RenameHost::Row);
            } else if dead && pos.is_some_and(|p| restore_rect.contains(p)) {
                self.send(C2D::RestartTerminal { id: t.id });
            } else {
                self.select_terminal(t.id);
            }
        }
        // Double-click the name = inline rename (§5.4); the pair's first
        // click already selected the row.
        if !renaming_here
            && resp.double_clicked()
            && resp
                .interact_pointer_pos()
                .is_some_and(|p| p.x >= x0 && p.x < cluster_left)
        {
            self.start_rename(RenameTarget::Term(t.id), t.name.clone(), RenameHost::Row);
        }

        // Drop-slot map (§5.5): this row's painted position in its group.
        if dragging {
            self.drop_rows.push(DropRow::Term { rect, folder: group, idx });
        }
        // Source row dims to ~40% while its ghost rides the pointer: the
        // sidebar surface composited over it at 60% (fill shift, no stroke).
        if drag_source {
            painter.rect_filled(rect, CornerRadius::same(6), BG_SIDEBAR_LIFT.gamma_multiply(0.6));
        }

        {
            let colors_ok = self.color_tags_supported();
            resp.context_menu(|ui| {
                menu_item_style(ui);
                if ui.button("Rename").clicked() {
                    self.start_rename(
                        RenameTarget::Term(t.id),
                        t.name.clone(),
                        RenameHost::Row,
                    );
                    ui.close();
                }
                // QOL §7.1: same program in the terminal's CURRENT cwd —
                // covers "new terminal in same directory" with one obvious
                // gesture. Claude duplicates mint a FRESH session id.
                if ui.button("Duplicate").clicked() {
                    self.duplicate_terminal(t);
                    ui.close();
                }
                if colors_ok {
                    if let Some(pick) = color_tag_menu(ui, t.color_tag) {
                        self.send(C2D::SetColorTag { id: t.id, tag: pick });
                        ui.close();
                    }
                }
                ui.menu_button("Move to", |ui| {
                    menu_item_style(ui);
                    if ui.button("(no folder)").clicked() {
                        self.send(C2D::MoveTerminal { id: t.id, folder: None });
                        ui.close();
                    }
                    let mut folders = self.state.folders.clone();
                    folders.sort_by_key(|f| f.order);
                    for f in folders {
                        if ui.button(&f.name).clicked() {
                            self.send(C2D::MoveTerminal {
                                id: t.id,
                                folder: Some(f.id),
                            });
                            ui.close();
                        }
                    }
                });
                if ui.button("Move up").clicked() {
                    self.send(C2D::ReorderTerminal { id: t.id, delta: -1 });
                    ui.close();
                }
                if ui.button("Move down").clicked() {
                    self.send(C2D::ReorderTerminal { id: t.id, delta: 1 });
                    ui.close();
                }
                ui.separator();
                // SLEEP §7.1: the lifecycle rows follow the PRESENTED status
                // (lifecycle_menu_label, unit-pinned): Running → Sleep (+ the
                // existing Kill), Asleep → Wake, Dead → Restore, Sleeping →
                // nothing (the drain resolves in under a second). Sleep entry
                // points hide against a pre-proto-9 daemon (skew window).
                match lifecycle_menu_label(presented) {
                    Some("Sleep") => {
                        if self.sleep_supported() && ui.button("Sleep").clicked() {
                            // Gate GUI-side (S8): idle sleeps are instant;
                            // busy evidence gets the confirm modal naming it.
                            match self.sleep_gate_evidence(t.id) {
                                None => {
                                    // C2: un-collapse + resize back BEFORE
                                    // the sleep so the freeze-frame captures
                                    // the reserved-size grid (wake geometry
                                    // consistency).
                                    self.prepare_sleep_geometry(t.id);
                                    self.send(C2D::SleepTerminal { id: t.id });
                                }
                                Some(_) => self.modal = Some(Modal::ConfirmSleep(t.id)),
                            }
                            ui.close();
                        }
                        if ui.button("Kill process").clicked() {
                            self.send(C2D::KillTerminal { id: t.id });
                            ui.close();
                        }
                    }
                    Some("Wake") => {
                        if ui.button("Wake").clicked() {
                            // RestartTerminal IS wake: launch() clears the
                            // asleep flag in its success mutate (S3/S5).
                            self.send(C2D::RestartTerminal { id: t.id });
                            ui.close();
                        }
                    }
                    Some(_) => {
                        if ui.button("Restore").clicked() {
                            self.send(C2D::RestartTerminal { id: t.id });
                            ui.close();
                        }
                    }
                    None => {
                        ui.label(
                            RichText::new("sleeping\u{2026}").size(12.0).color(TEXT_MUTED),
                        );
                    }
                }
                let mut auto = t.auto_restore;
                if ui.checkbox(&mut auto, "Restore after reboot").changed() {
                    self.send(C2D::SetAutoRestore { id: t.id, auto });
                }
                // SSH only: the auto-reconnect opt-out (ShellCfg field;
                // default ON). Visible entry point per the mouse-first
                // doctrine; hidden against a pre-proto-10 daemon.
                if self.reconnect_supported()
                    && matches!(
                        crate::state::shell_family(&t.kind, &t.program, &t.args),
                        crate::state::ShellFamily::Ssh { .. }
                    )
                {
                    let mut rc = t.shell_cfg.as_ref().is_none_or(|c| c.auto_reconnect);
                    if ui.checkbox(&mut rc, "Auto-reconnect").changed() {
                        self.send(C2D::SetAutoReconnect { id: t.id, on: rc });
                    }
                }
                ui.separator();
                if ui.button(RichText::new("Delete").color(RED)).clicked() {
                    self.modal = Some(Modal::ConfirmDeleteTerminal(t.id));
                    ui.close();
                }
            });
        }
    }
}

// Rail geometry pins (collapsed-rail layout fix). egui lays the rail out at
// runtime, so these pin the arithmetic the layout relies on: every centered
// element must fit the 44px column, and header glyphs must share the dot
// rows' center axis. The literals mirror sidebar():9 (44.0 rail width),
// sidebar():27 (Margin::same(4) rail body), and icons::footer_glyph (18px).
#[cfg(test)]
mod rail_layout_pins {
    const RAIL_W: f32 = 44.0;
    const RAIL_MARGIN: f32 = 4.0;
    const GLYPH: f32 = 18.0;
    const DOT_ROW_H: f32 = 28.0;

    /// A centered 18px glyph fits the rail's inner span with room to spare —
    /// no clipping at the 44px width regardless of DPI scale (egui units are
    /// logical points; the proportions hold at any pixels_per_point).
    #[test]
    fn rail_glyph_fits_one_column() {
        let inner = RAIL_W - 2.0 * RAIL_MARGIN;
        assert!(GLYPH <= inner, "18px glyph must fit {inner}px inner rail");
    }

    /// Centered header glyphs and full-width dot rows share one axis: the
    /// center of a centered glyph equals the center of a full-width row.
    #[test]
    fn rail_header_and_dots_share_center_axis() {
        let inner = RAIL_W - 2.0 * RAIL_MARGIN;
        let glyph_center = RAIL_MARGIN + (inner - GLYPH) / 2.0 + GLYPH / 2.0;
        let dot_center = RAIL_MARGIN + inner / 2.0;
        assert!((glyph_center - dot_center).abs() < f32::EPSILON);
        // And the dot row is tall enough to host its 7px-radius halo dot.
        let row_h = DOT_ROW_H;
        let halo_d = 2.0 * 7.0;
        assert!(row_h >= halo_d, "28px row must host a 14px halo");
    }
}
