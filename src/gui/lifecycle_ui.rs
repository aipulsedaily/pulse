//! #34 lifecycle chrome: the custom-branded install/update/uninstall
//! moments that live OUTSIDE the main window's normal life.
//!
//! Three surfaces, one visual grammar (near-black card, accent mark, quiet
//! copy — theme tokens from gui/mod.rs):
//! - `run_updating(from, to)`: a small standalone window covering the
//!   apply gap — old GUI exits, Update.exe swaps `current\`, new GUI boots.
//!   Spawned from a %TEMP% copy of the exe (`crate::spawn_lifecycle_helper`)
//!   so it never image-locks the dir being swapped. It watches the
//!   `bin\.version` sidecar flip to `to` (bin-sync completion = the update
//!   truly landed) and then closes itself after a short "Updated" beat.
//! - `run_uninstall()`: "Uninstalling… → Uninstalled" window with the
//!   data-kept note and an armed opt-in "Delete my data too". Also runs from
//!   a %TEMP% copy (the install dir is deleted underneath it).
//! - `App::welcome_card_ui`: the one-time in-app first-run card (Velopack's
//!   on_first_run hook), rendered as a quiet overlay — one panel, one
//!   button, no wizard.
//!
//! Doctrine: these are the ONLY places the brand may animate (a live
//! progress spinner is activity, not idle chrome). Helpers self-delete
//! their temp copy on exit.

use super::*;

/// Helper-window shell shared by the two standalone surfaces.
fn helper_options() -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([430.0, 190.0])
        .with_decorations(false)
        .with_resizable(false)
        .with_always_on_top()
        .with_title("Pulse");
    if let Some(icon) = super::app_window_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    eframe::NativeOptions {
        viewport,
        centered: true,
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            wgpu_setup: {
                use eframe::egui_wgpu::wgpu;
                let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
                setup.instance_descriptor.backends =
                    wgpu::Backends::from_env().unwrap_or(wgpu::Backends::DX12);
                setup.into()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Self-delete the %TEMP% helper copy after the process exits (cmd ping
/// delay releases the image lock first). No-op when this exe does not live
/// in the temp dir — a dev binary invoked by hand must never delete itself.
fn schedule_self_delete() {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    if !me.starts_with(std::env::temp_dir()) {
        return;
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("cmd")
        .arg("/C")
        .arg(format!(
            "ping -n 3 127.0.0.1 >nul & del /f \"{}\"",
            me.display()
        ))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// The real app mark (Logo A), rendered from the committed 48x48 RGBA blob
/// (assets/window-icon-48.rgba — the full frame+chevron+spark mark on its
/// rounded near-black plate, transparent corners). The texture is memoized
/// in egui's temp store so it is uploaded once per process, not per frame.
/// Shared by the lifecycle windows and the first-run welcome card.
pub(super) fn draw_mark(painter: &egui::Painter, center: Pos2, size: f32) {
    let ctx = painter.ctx();
    let id = egui::Id::new("tc-app-mark-tex");
    let tex = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id));
    let tex = tex.unwrap_or_else(|| {
        const RGBA: &[u8] = include_bytes!("../../assets/window-icon-48.rgba");
        // Non-premultiplied RGBA straight from the generator (PIL tobytes).
        let img = egui::ColorImage::from_rgba_unmultiplied([48, 48], RGBA);
        let handle = ctx.load_texture("tc-app-mark", img, egui::TextureOptions::LINEAR);
        ctx.data_mut(|d| d.insert_temp(id, handle.clone()));
        handle
    });
    let rect = Rect::from_center_size(center, Vec2::splat(size));
    painter.image(
        tex.id(),
        rect,
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );
}

/// Shared card chrome: background, mark, title/sub, drag-to-move, ✕.
/// Returns true when the ✕ was clicked.
fn card_chrome(ui: &mut egui::Ui, title: &str, sub: &str, spinning: bool) -> bool {
    let rect = ui.max_rect();
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::ZERO, BG);
    // Mark, left.
    draw_mark(painter, Pos2::new(rect.min.x + 46.0, rect.min.y + 52.0), 34.0);
    // Copy block.
    let tx = rect.min.x + 84.0;
    painter.text(
        Pos2::new(tx, rect.min.y + 40.0),
        Align2::LEFT_CENTER,
        title,
        semibold(15.0),
        TEXT,
    );
    let sub_galley = painter.layout(
        sub.to_string(),
        FontId::proportional(12.0),
        TEXT_MUTED,
        rect.max.x - tx - 18.0,
    );
    painter.galley(Pos2::new(tx, rect.min.y + 56.0), sub_galley, TEXT_MUTED);
    // Spinner: a rotating accent arc beside the title while active. This is
    // live progress — the one sanctioned animation in the brand.
    if spinning {
        let c = Pos2::new(rect.max.x - 60.0, rect.min.y + 40.0);
        let t = ui.input(|i| i.time) as f32;
        let r = 7.0;
        let start = t * 3.4;
        let pts: Vec<Pos2> = (0..=24)
            .map(|i| {
                let a = start + i as f32 * 0.09;
                Pos2::new(c.x + r * a.cos(), c.y + r * a.sin())
            })
            .collect();
        ui.painter()
            .add(egui::Shape::line(pts, Stroke::new(2.0, ACCENT)));
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }
    // Drag anywhere (frameless window).
    let bg_resp = ui.interact(rect, Id::new("lc-drag"), Sense::click_and_drag());
    if bg_resp.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    // ✕ close, top-right, hover-revealed like the app chrome.
    let close = Rect::from_center_size(
        Pos2::new(rect.max.x - 16.0, rect.min.y + 16.0),
        Vec2::splat(22.0),
    );
    let cresp = ui.interact(close, Id::new("lc-close"), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(cresp.id, cresp.hovered(), 0.12);
    draw_icon(
        ui.painter(),
        close.shrink(7.0),
        Icon::Close,
        lerp_col(TEXT_MUTED, TEXT, hov),
    );
    cresp.clicked()
}

// ─────────────────────────── updating window ───────────────────────────

struct UpdatingUi {
    from: String,
    to: String,
    started: Instant,
    /// Set the moment bin\.version flips to `to` — the visible "Updated"
    /// beat runs briefly after, then the window closes itself.
    landed: Option<Instant>,
    last_poll: Instant,
}

impl eframe::App for UpdatingUi {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        // Poll the sidecar at 4Hz — flip means the new GUI booted and
        // bin-sync deployed, i.e. the update fully landed.
        if self.landed.is_none() && self.last_poll.elapsed() > Duration::from_millis(250) {
            self.last_poll = Instant::now();
            let sidecar = crate::state::data_dir().join("bin").join(".version");
            let deployed = std::fs::read_to_string(sidecar).unwrap_or_default();
            if deployed.trim() == self.to {
                self.landed = Some(Instant::now());
            }
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let done_beat = self
            .landed
            .is_some_and(|t| t.elapsed() > Duration::from_millis(1100));
        // Quiet cap: if the update never lands (apply failure — the main
        // GUI's toast owns that story), don't haunt the desktop.
        let timed_out = self.started.elapsed() > Duration::from_secs(90);
        if done_beat || timed_out {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let (title, sub, spinning) = if self.landed.is_some() {
            (
                "Updated".to_string(),
                format!("Pulse v{} is ready.", self.to),
                false,
            )
        } else {
            (
                "Updating Pulse".to_string(),
                format!(
                    "v{} \u{2192} v{} \u{2014} your terminals will be right back.",
                    self.from, self.to
                ),
                true,
            )
        };
        if card_chrome(ui, &title, &sub, spinning) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.landed.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }
}

/// Standalone branded window covering the apply→relaunch gap. Spawned by the
/// update engine right before the daemon quiesce (update.rs::apply).
pub fn run_updating(from: String, to: String) -> anyhow::Result<()> {
    let app = UpdatingUi {
        from: if from.is_empty() { "?".into() } else { from },
        to: if to.is_empty() { "?".into() } else { to },
        started: Instant::now(),
        landed: None,
        last_poll: Instant::now() - Duration::from_secs(1),
    };
    let result = eframe::run_native(
        "tc-updating",
        helper_options(),
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            style(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );
    schedule_self_delete();
    result.map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

// ─────────────────────────── uninstall window ───────────────────────────

struct UninstallUi {
    started: Instant,
    /// Two-click confirm for "Delete my data too" (3s arm window).
    delete_armed: Option<Instant>,
    data_deleted: bool,
    /// The confirmed delete failed and the dir is still there (files in
    /// use) — surface it instead of appearing to do nothing.
    delete_failed: bool,
}

impl UninstallUi {
    /// Uninstall is finished once Velopack's install dir is gone (checked
    /// live), with a time cap so a stuck Update.exe can't wedge the window.
    fn uninstall_done(&self) -> bool {
        if self.started.elapsed() > Duration::from_secs(30) {
            return true;
        }
        let base = dirs::data_local_dir().unwrap_or_default();
        !base.join("AIPulseDaily.Pulse").exists()
    }
}

impl eframe::App for UninstallUi {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        let done = self.uninstall_done();
        let data_dir = crate::state::data_dir();
        let (title, sub) = if !done {
            (
                "Uninstalling Pulse\u{2026}".to_string(),
                "Stopping the background daemon and removing the app.".to_string(),
            )
        } else if self.data_deleted {
            (
                "Uninstalled".to_string(),
                "The app and its data are gone. Thanks for trying Pulse.".to_string(),
            )
        } else if self.delete_failed {
            (
                "Uninstalled".to_string(),
                "Some files are still in use \u{2014} close Pulse and try again."
                    .to_string(),
            )
        } else {
            (
                "Uninstalled".to_string(),
                format!(
                    "Your sessions and settings were kept at {} \u{2014} reinstalling brings every terminal back.",
                    data_dir.display()
                ),
            )
        };
        // Disarm quietly after the window lapses.
        if self
            .delete_armed
            .is_some_and(|t| t.elapsed() > Duration::from_secs(3))
        {
            self.delete_armed = None;
        }
        let mut close = false;
        if card_chrome(ui, &title, &sub, !done) {
            close = true;
        }
        if done {
            let rect = ui.max_rect();
            // Buttons bottom-right: primary Close, ghost armed
            // delete-my-data (opt-in, never the default).
            let brow = Rect::from_min_max(
                Pos2::new(rect.min.x + 84.0, rect.max.y - 46.0),
                Pos2::new(rect.max.x - 14.0, rect.max.y - 14.0),
            );
            let mut bui = ui.new_child(
                UiBuilder::new()
                    .max_rect(brow)
                    .layout(Layout::right_to_left(Align::Center)),
            );
            let pg = bui
                .painter()
                .layout_no_wrap("Close".into(), semibold(12.0), ON_ACCENT);
            let (prect, presp) =
                bui.allocate_exact_size(Vec2::new(pg.size().x + 26.0, 26.0), Sense::click());
            let pfill = if presp.hovered() { ACCENT_HOVER } else { ACCENT };
            bui.painter().rect_filled(prect, CornerRadius::same(7), pfill);
            bui.painter().text(
                prect.center(),
                Align2::CENTER_CENTER,
                "Close",
                semibold(12.0),
                ON_ACCENT,
            );
            if presp.clicked() {
                close = true;
            }
            if !self.data_deleted {
                let armed = self.delete_armed.is_some();
                let label = if armed {
                    "Click again to delete everything"
                } else {
                    "Delete my data too\u{2026}"
                };
                let color = if armed { DANGER } else { TEXT_MUTED };
                if row_ghost_button(&mut bui, label, color).clicked() {
                    if armed {
                        self.delete_armed = None;
                        // The daemon was quiesced by the uninstall hook;
                        // the data dir is free to go.
                        if std::fs::remove_dir_all(&data_dir).is_ok() || !data_dir.exists() {
                            self.data_deleted = true;
                            self.delete_failed = false;
                        } else {
                            // Something (a still-open gui.log, an Explorer
                            // window) holds the dir — say so; retry stays
                            // available.
                            self.delete_failed = true;
                        }
                    } else {
                        self.delete_armed = Some(Instant::now());
                    }
                }
                if armed {
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
            }
        }
        if close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if !done {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// Standalone branded uninstall window, spawned by the `--veloapp-uninstall`
/// hook (main.rs::uninstall_cleanup) from a %TEMP% copy.
pub fn run_uninstall() -> anyhow::Result<()> {
    let app = UninstallUi {
        started: Instant::now(),
        delete_armed: None,
        data_deleted: false,
        delete_failed: false,
    };
    let result = eframe::run_native(
        "tc-uninstalling",
        helper_options(),
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            style(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );
    schedule_self_delete();
    result.map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

// ─────────────────────────── first-run welcome card ───────────────────────────

impl App {
    /// One-time branded welcome overlay on the very first launch after
    /// install (Velopack on_first_run). One panel, one button, dismissed by
    /// Esc/click-away/button — never seen again (the latch is run-once).
    pub(super) fn welcome_card_ui(&mut self, ctx: &egui::Context) {
        if !self.welcome_card {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.welcome_card = false;
            return;
        }
        let content = ctx.content_rect();
        const W: f32 = 400.0;
        const H: f32 = 150.0;
        let pos = content.center() - Vec2::new(W / 2.0, H / 2.0 + 40.0);
        let mut dismiss = false;
        let area = egui::Area::new(egui::Id::new("welcome-card"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos);
        let aresp = area.show(ctx, |ui| {
            egui::Frame::new()
                .fill(SURFACE)
                .corner_radius(CornerRadius::same(12))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 8],
                    blur: 32,
                    spread: 0,
                    color: Color32::from_black_alpha(160),
                })
                .inner_margin(Margin::same(18))
                .show(ui, |ui| {
                    ui.set_width(W - 36.0);
                    ui.horizontal(|ui| {
                        let (mrect, _) =
                            ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                        draw_mark(ui.painter(), mrect.center(), 30.0);
                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new("Welcome to Pulse")
                                    .font(semibold(14.0))
                                    .color(TEXT),
                            );
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new(
                                    "Installed and ready. Terminals you create here persist \u{2014} close everything, come back anytime.",
                                )
                                .size(12.0)
                                .color(TEXT_MUTED),
                            );
                        });
                    });
                    ui.add_space(12.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let pg = ui.painter().layout_no_wrap(
                            "Get started".into(),
                            semibold(12.0),
                            ON_ACCENT,
                        );
                        let (prect, presp) = ui.allocate_exact_size(
                            Vec2::new(pg.size().x + 26.0, 27.0),
                            Sense::click(),
                        );
                        let presp = presp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        let pfill = if presp.hovered() { ACCENT_HOVER } else { ACCENT };
                        ui.painter().rect_filled(prect, CornerRadius::same(7), pfill);
                        ui.painter().text(
                            prect.center(),
                            Align2::CENTER_CENTER,
                            "Get started",
                            semibold(12.0),
                            ON_ACCENT,
                        );
                        if presp.clicked() {
                            dismiss = true;
                        }
                    });
                });
        });
        let painted = aresp.response.rect;
        let pressed_outside = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|p| !painted.contains(p))
        });
        if dismiss || pressed_outside {
            self.welcome_card = false;
        }
    }
}
