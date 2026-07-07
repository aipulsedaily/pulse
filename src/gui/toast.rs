//! Bottom-right toast stack — the app's FIRST toast surface (ssh-drop #26),
//! deliberately sized for #26 plus the #25 attention-toast seam and nothing
//! else (spec §5). Doctrine: zero strokes (SURFACE fill + shadow carry the
//! depth), fades ≤120ms, auto-dismiss pauses while hovered, and the stack
//! NEVER steals focus — no focusable widgets, just painter glyphs and
//! invisible interact rects (the blocks-toolbar pattern; an egui Area takes
//! no keyboard focus by itself).
//!
//! Ownership split: `Toasts` is pure bookkeeping (push/update/dismiss/tick —
//! unit-tested without a Context); `show` renders and returns the one action
//! a click requested, which the App dispatches (§5.1).

use std::time::{Duration, Instant};

use egui::{
    Align2, Color32, CornerRadius, Id, Margin, Pos2, Rect, RichText, Sense, Shape, Stroke, Vec2,
};

// Local copies of the app tokens (the term_view precedent — theme consts are
// module-private in mod.rs).
const SURFACE: Color32 = Color32::from_rgb(0x14, 0x17, 0x1F);
const TEXT: Color32 = Color32::from_rgb(0xE7, 0xE9, 0xEF);
const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xA9, 0xAF, 0xC0);
const TEXT_MUTED: Color32 = Color32::from_rgb(0x6B, 0x71, 0x85);
const ACCENT: Color32 = Color32::from_rgb(0x7C, 0x83, 0xFF);
const DANGER: Color32 = Color32::from_rgb(0xFF, 0x5C, 0x6C);

/// At most this many toasts are visible (non-fading); pushing past the cap
/// evicts the oldest non-Progress toast first (§5.1) — a Progress toast is a
/// live upload's only cancel affordance, so it survives longest.
const CAP: usize = 4;
const WIDTH: f32 = 320.0;
const GAP: f32 = 8.0;
const MARGIN: f32 = 12.0;
const FADE: f32 = 0.12;

pub type ToastId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    /// Sticky (ttl None), spinner glyph, ✕ = its action (cancel).
    Progress,
    Error,
    Info,
}

/// Returned from `show()`; the consumer dispatches (§5.1). One variant
/// today; the deferred #25 attention toast adds its focus-terminal action
/// here when it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastAction {
    /// #26: cancel the upload job.
    CancelUpload(u64),
}

pub struct Toast {
    pub kind: ToastKind,
    /// 13px TEXT, single line, ellipsized by the renderer.
    pub title: String,
    /// 12px TEXT_SECONDARY lines (per-file rows).
    pub detail: Vec<String>,
    /// None = sticky (Progress); Some = auto-dismiss after this long.
    pub ttl: Option<Duration>,
    /// Progress: ✕ triggers it. Info: a body click triggers it.
    pub action: Option<ToastAction>,
}

struct Item {
    id: ToastId,
    t: Toast,
    /// Auto-dismiss budget left; counts down only in un-hovered frames
    /// (hover-to-hold). None = sticky.
    remaining: Option<Duration>,
    /// Fade-out in progress (dismissed/expired/evicted); the item is removed
    /// once the fade lands at zero.
    closing: bool,
    /// The fade-in animation was seeded at 0 on the first rendered frame
    /// (egui initializes an unseen animate id AT the target — without the
    /// seed a new toast would pop in rather than fade).
    seeded: bool,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Item>,
    next_id: ToastId,
    last_tick: Option<Instant>,
    /// The toast the pointer rested on last frame (render-time observation;
    /// one frame of hover-pause lag is imperceptible).
    hovered: Option<ToastId>,
}

impl Toasts {
    pub fn push(&mut self, t: Toast) -> ToastId {
        while self.items.iter().filter(|i| !i.closing).count() >= CAP {
            let victim = self
                .items
                .iter()
                .find(|i| !i.closing && i.t.kind != ToastKind::Progress)
                .map(|i| i.id)
                .or_else(|| self.items.iter().find(|i| !i.closing).map(|i| i.id));
            match victim {
                Some(v) => self.dismiss(v),
                None => break,
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let remaining = t.ttl;
        self.items.push(Item {
            id,
            t,
            remaining,
            closing: false,
            seeded: false,
        });
        id
    }

    /// Morph a toast in place (Progress → Error/Info verdicts, queued-title
    /// swap). Re-seeds the auto-dismiss countdown from the new ttl. No-op on
    /// an unknown/dismissed id.
    pub fn update(&mut self, id: ToastId, f: impl FnOnce(&mut Toast)) {
        if let Some(it) = self.items.iter_mut().find(|i| i.id == id) {
            f(&mut it.t);
            it.remaining = it.t.ttl;
        }
    }

    pub fn dismiss(&mut self, id: ToastId) {
        if let Some(it) = self.items.iter_mut().find(|i| i.id == id) {
            it.closing = true;
        }
    }

    /// Pure countdown bookkeeping: `dt` elapses on every non-hovered,
    /// non-closing toast with a ttl; returns the ids that just expired
    /// (marked closing). Unit-tested without a Context.
    fn tick(&mut self, dt: Duration, hovered: Option<ToastId>) -> Vec<ToastId> {
        let mut expired = Vec::new();
        for it in &mut self.items {
            if it.closing || hovered == Some(it.id) {
                continue;
            }
            if let Some(rem) = &mut it.remaining {
                *rem = rem.saturating_sub(dt);
                if rem.is_zero() {
                    it.closing = true;
                    expired.push(it.id);
                }
            }
        }
        expired
    }

    /// Render the stack bottom-right inside `anchor` (already lifted over the
    /// composer strip by the caller) and return the one action a click
    /// requested. `interactive: false` (a modal is open) renders but ignores
    /// the pointer — belt on top of egui's own modal-layer blocking.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        anchor: Rect,
        interactive: bool,
    ) -> Option<ToastAction> {
        if self.items.is_empty() {
            self.last_tick = None;
            self.hovered = None;
            return None;
        }
        let now = Instant::now();
        let dt = self.last_tick.map(|t| now - t).unwrap_or_default();
        self.last_tick = Some(now);
        let hovered_last = self.hovered.take();
        let _ = self.tick(dt, hovered_last);

        let width = WIDTH.min(anchor.width() - 2.0 * MARGIN);
        if width < 120.0 {
            return None;
        }
        let right = anchor.max.x - MARGIN;
        let mut bottom = anchor.max.y - MARGIN;
        let mut action = None;
        let mut gone: Vec<ToastId> = Vec::new();

        // Newest sits nearest the corner; older toasts stack upward.
        for it in self.items.iter_mut().rev() {
            let anim_id = Id::new(("toast-fade", it.id));
            if !it.seeded {
                // Force the animation to start from 0 so the first frames
                // fade in instead of popping.
                ctx.animate_bool_with_time(anim_id, false, 0.0);
                it.seeded = true;
            }
            let alpha = ctx.animate_bool_with_time(anim_id, !it.closing, FADE);
            if it.closing && alpha <= 0.0 {
                gone.push(it.id);
                continue;
            }

            // (frame rect, hovered, ✕ clicked, body clicked) — interaction
            // happens INSIDE the area on its own layer, so egui's modal-layer
            // blocking applies and nothing here can take keyboard focus.
            let tid = it.id;
            let kind = it.t.kind;
            let title = it.t.title.clone();
            let detail = it.t.detail.clone();
            let closing = it.closing;
            let aresp = egui::Area::new(Id::new(("toast", it.id)))
                .order(egui::Order::Foreground)
                .pivot(Align2::RIGHT_BOTTOM)
                .fixed_pos(Pos2::new(right, bottom))
                .show(ctx, |ui| {
                    ui.multiply_opacity(alpha);
                    let fr = egui::Frame::new()
                        .fill(SURFACE)
                        .corner_radius(CornerRadius::same(10))
                        .shadow(egui::epaint::Shadow {
                            offset: [0, 6],
                            blur: 28,
                            spread: 0,
                            color: Color32::from_black_alpha(150),
                        })
                        .inner_margin(Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.set_width(width - 24.0);
                            ui.horizontal_top(|ui| {
                                // Leading glyph, painter-drawn (D35): signal,
                                // not decoration.
                                let (grect, _) = ui
                                    .allocate_exact_size(Vec2::new(16.0, 18.0), Sense::hover());
                                let c = grect.center();
                                let p = ui.painter();
                                match kind {
                                    ToastKind::Progress => {
                                        let t = ui.input(|i| i.time);
                                        spinner(p, c, 5.0, t, ACCENT);
                                    }
                                    ToastKind::Error => {
                                        p.circle_filled(c, 3.0, DANGER);
                                    }
                                    ToastKind::Info => {
                                        p.circle_filled(c, 3.0, ACCENT);
                                    }
                                }
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    // Reserve the ✕ zone so the ghost never
                                    // sits over glyphs.
                                    ui.set_width(width - 24.0 - 16.0 - 18.0);
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&title).size(13.0).color(TEXT),
                                        )
                                        .truncate(),
                                    );
                                    for d in &detail {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(d)
                                                    .size(12.0)
                                                    .color(TEXT_SECONDARY),
                                            )
                                            .truncate(),
                                        );
                                    }
                                });
                            });
                        });
                    let frect = fr.response.rect;
                    let mut hovered_now = false;
                    let mut x_clicked = false;
                    let mut body_clicked = false;
                    if interactive && !closing {
                        let body = ui.interact(
                            frect,
                            Id::new(("toast-hit", tid)),
                            Sense::click(),
                        );
                        hovered_now = body.hovered();
                        let xc = Pos2::new(frect.max.x - 14.0, frect.min.y + 14.0);
                        let xrect = Rect::from_center_size(xc, Vec2::splat(16.0));
                        if hovered_now {
                            // Ghost ✕, hover-revealed: dismiss (Error/Info)
                            // or the Progress action (cancel).
                            let over_x = ui
                                .ctx()
                                .pointer_hover_pos()
                                .is_some_and(|p| xrect.contains(p));
                            close_glyph(
                                ui.painter(),
                                xc,
                                3.5,
                                if over_x { TEXT } else { TEXT_MUTED },
                            );
                        }
                        if body.clicked() {
                            if body.interact_pointer_pos().is_some_and(|p| xrect.contains(p)) {
                                x_clicked = true;
                            } else {
                                body_clicked = true;
                            }
                        }
                    }
                    (frect, hovered_now, x_clicked, body_clicked)
                });
            let (frect, hovered_now, x_clicked, body_clicked) = aresp.inner;

            if hovered_now {
                self.hovered = Some(it.id);
            }
            if x_clicked {
                match (it.t.kind, it.t.action) {
                    (ToastKind::Progress, Some(a)) => action = Some(a),
                    _ => it.closing = true,
                }
            } else if body_clicked && it.t.kind == ToastKind::Info {
                if let Some(a) = it.t.action {
                    action = Some(a);
                    it.closing = true;
                }
            }

            bottom = frect.min.y - GAP;
            if bottom < anchor.min.y + 40.0 {
                break; // never stack past the top of the central area
            }
        }

        if !gone.is_empty() {
            self.items.retain(|i| !gone.contains(&i.id));
        }

        // Wakeups: spinners animate at the chrome cadence; ttl expiries need
        // a frame at the deadline (fades self-schedule via ctx.animate_*).
        if self
            .items
            .iter()
            .any(|i| !i.closing && i.t.kind == ToastKind::Progress)
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if let Some(min) = self
            .items
            .iter()
            .filter(|i| !i.closing)
            .filter_map(|i| i.remaining)
            .min()
        {
            ctx.request_repaint_after(min + Duration::from_millis(10));
        }
        action
    }
}

/// 12px indeterminate arc spinner: ~¾ turn, rotating with wall time.
/// pub(crate): the composer's `reconnecting…` lane reuses it.
pub(crate) fn spinner(p: &egui::Painter, center: Pos2, r: f32, time: f64, color: Color32) {
    const N: usize = 20;
    let start = (time * 2.4) % std::f64::consts::TAU;
    let sweep = 0.72 * std::f64::consts::TAU;
    let pts: Vec<Pos2> = (0..=N)
        .map(|i| {
            let a = start + sweep * (i as f64 / N as f64);
            center + Vec2::new(a.cos() as f32, a.sin() as f32) * r
        })
        .collect();
    p.add(Shape::line(pts, Stroke::new(2.0, color)));
}

fn close_glyph(p: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    let s = Stroke::new(1.4, color);
    p.line_segment([c + Vec2::new(-r, -r), c + Vec2::new(r, r)], s);
    p.line_segment([c + Vec2::new(-r, r), c + Vec2::new(r, -r)], s);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toast(kind: ToastKind, ttl: Option<Duration>) -> Toast {
        Toast {
            kind,
            title: "t".into(),
            detail: vec![],
            ttl,
            action: None,
        }
    }

    fn visible(ts: &Toasts) -> Vec<ToastId> {
        ts.items.iter().filter(|i| !i.closing).map(|i| i.id).collect()
    }

    /// §5.1 cap: pushing past 4 evicts the oldest non-Progress first; an
    /// all-Progress stack falls back to the oldest Progress.
    #[test]
    fn eviction_cap_prefers_non_progress() {
        let mut ts = Toasts::default();
        let p1 = ts.push(toast(ToastKind::Progress, None));
        let e1 = ts.push(toast(ToastKind::Error, Some(Duration::from_secs(8))));
        let e2 = ts.push(toast(ToastKind::Error, Some(Duration::from_secs(8))));
        let p2 = ts.push(toast(ToastKind::Progress, None));
        let e3 = ts.push(toast(ToastKind::Error, Some(Duration::from_secs(8))));
        // e1 (oldest non-Progress) was evicted, both Progress survive.
        assert_eq!(visible(&ts), vec![p1, e2, p2, e3]);
        assert!(ts.items.iter().any(|i| i.id == e1 && i.closing));

        let mut ts = Toasts::default();
        let ps: Vec<_> = (0..4)
            .map(|_| ts.push(toast(ToastKind::Progress, None)))
            .collect();
        let p5 = ts.push(toast(ToastKind::Progress, None));
        assert_eq!(visible(&ts), vec![ps[1], ps[2], ps[3], p5]);
    }

    /// Hover pauses the countdown; un-hovered time elapses it; expiry marks
    /// closing exactly once.
    #[test]
    fn ttl_pause_math() {
        let mut ts = Toasts::default();
        let id = ts.push(toast(ToastKind::Info, Some(Duration::from_secs(5))));
        assert!(ts.tick(Duration::from_secs(2), None).is_empty());
        assert_eq!(ts.items[0].remaining, Some(Duration::from_secs(3)));
        // Hovered frames hold the budget.
        assert!(ts.tick(Duration::from_secs(60), Some(id)).is_empty());
        assert_eq!(ts.items[0].remaining, Some(Duration::from_secs(3)));
        let expired = ts.tick(Duration::from_secs(4), None);
        assert_eq!(expired, vec![id]);
        assert!(ts.items[0].closing);
        // Already-closing items never re-expire.
        assert!(ts.tick(Duration::from_secs(4), None).is_empty());
    }

    /// Sticky Progress never counts down; a morph to Error re-seeds the
    /// countdown from the new ttl; dismiss marks closing.
    #[test]
    fn update_morph_and_dismiss() {
        let mut ts = Toasts::default();
        let id = ts.push(toast(ToastKind::Progress, None));
        assert!(ts.tick(Duration::from_secs(3600), None).is_empty());
        ts.update(id, |t| {
            t.kind = ToastKind::Error;
            t.title = "failed".into();
            t.ttl = Some(Duration::from_secs(8));
        });
        assert_eq!(ts.items[0].remaining, Some(Duration::from_secs(8)));
        assert_eq!(ts.items[0].t.title, "failed");
        ts.update(999, |t| t.title = "never".into()); // unknown id no-ops
        ts.dismiss(id);
        assert!(ts.items[0].closing);
    }
}
