//! Hover-peek for the collapsed sidebar rail.
//!
//! While the sidebar is collapsed to its 44px rail, resting the pointer on
//! the rail grows the rail into the full sidebar — over the terminal — and
//! leaving folds it back. The flyout is an egui `Area` OVER the terminal, never a
//! panel beside it: the rail panel stays exactly `RAIL_W` wide, so the
//! central panel — and therefore the grid's cols/rows and the PTY size — is
//! identical with the flyout open, opening, or closed. A mouse pass must
//! never reflow a running program (pinned by `flyout_never_moves_the_grid`).
//!
//! Model (one rule each, explicit toggle always wins):
//! - **Pinned open** (`sidebar_collapsed == false`): the real 240px panel;
//!   the peek is inert and reset.
//! - **Collapsed**: pointer dwells `OPEN_DELAY` on the rail → open. Pointer
//!   away from rail ∪ flyout for `CLOSE_DELAY` → close. A press outside both
//!   closes at once. A drag, an inline rename, or an open popup (row context
//!   menu) holds it open.
//! - **Just collapsed**: no peek until the pointer has left the old 240px
//!   footprint once, so collapsing never immediately re-opens under the
//!   pointer.
//!
//! Motion is ONE scalar timeline `t` (0 = closed, 1 = open): the flyout's
//! width occupies `WIDTH_SPAN`, the labels' fade `LABEL_SPAN`. Opening runs
//! it forward (width first, labels after); closing runs the same timeline
//! backward, so labels fade out before the width folds — symmetric by
//! construction, no separate close choreography to drift.
//!
//! Pure state machine: time comes in as `PeekInput::now` (egui's input
//! clock), so every rule is unit-tested without a window.

use egui::{Color32, Id, Order, Pos2, Rect, UiBuilder, Vec2};

/// Collapsed rail width (the sidebar panel's width while railed).
pub(super) const RAIL_W: f32 = 44.0;
/// Pinned sidebar width; also the open flyout's width (from the rail's left
/// edge), so the tree inside it lays out at exactly the pinned geometry —
/// the flyout IS the sidebar, and pinning from it changes no pixel of it.
pub(super) const PANEL_W: f32 = 240.0;

/// Dwell before the flyout opens. Crossing a 44px rail at an ordinary
/// pointer speed (~600-1500 px/s) takes 30-75ms, so 150ms rejects every
/// pass-through on the way to the window edge or titlebar while still
/// reading as immediate when the pointer is put there on purpose (below the
/// ~200ms point where a hover response starts to feel laggy).
pub(super) const OPEN_DELAY: f64 = 0.15;
/// Grace before it closes once the pointer is away from rail ∪ flyout. Twice
/// the open delay: an overshoot past the flyout edge while reaching for a
/// row's ✕/✏ cluster, or a diagonal swing across the corner, comes back well
/// inside 300ms, and the flyout must not fold under it.
pub(super) const CLOSE_DELAY: f64 = 0.30;
/// Timeline durations. Opening is unhurried enough to read as motion;
/// closing is quicker — an exit should get out of the way.
pub(super) const OPEN_DUR: f32 = 0.20;
pub(super) const CLOSE_DUR: f32 = 0.16;
/// Forgiveness band beyond the flyout's right edge that still counts as
/// "on the flyout" (the row action cluster sits 2px from that edge).
pub(super) const EDGE_SLOP: f32 = 12.0;
/// Left window edge band that belongs to the OS resize grip: it never arms
/// the peek (reaching for the resize edge crosses — and stops on — the rail).
pub(super) const RESIZE_EDGE: f32 = 6.0;

/// A delay with less than this left counts as elapsed: the wakeup scheduled
/// for a deadline can land a float-rounding hair before it, and must not
/// re-arm a zero-length wakeup.
const TIMER_EPS: f64 = 1e-4;

/// Width occupies the first 60% of the timeline: 120ms of a 200ms open.
const WIDTH_SPAN: (f32, f32) = (0.0, 0.6);
/// Labels fade over the last 55%: they start as the width lands (overlap
/// 0.45..0.6 keeps the hand-off continuous) and, run backward, are almost
/// gone (≈18%) by the time the width starts to fold.
const LABEL_SPAN: (f32, f32) = (0.45, 1.0);

/// DIAGNOSTIC (staging rigs): `TC_DIAG_RAIL_PEEK=open` holds a virtual
/// pointer on the rail; `=cycle` alternates on/off every `DIAG_HALF` seconds
/// (frame-cost measurement of the animation). Display-detached rigs deliver
/// no hover, so scripted staging needs a product-side hook — the
/// TC_DIAG_OPEN_LAUNCHER_MS precedent. Read once. Never set in normal use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum DiagPeek {
    Open,
    Cycle,
}
pub(super) const DIAG_HALF: f64 = 0.7;

pub(super) fn diag_peek() -> Option<DiagPeek> {
    static D: std::sync::OnceLock<Option<DiagPeek>> = std::sync::OnceLock::new();
    *D.get_or_init(|| match std::env::var("TC_DIAG_RAIL_PEEK").ok().as_deref() {
        Some("open") => Some(DiagPeek::Open),
        Some("cycle") => Some(DiagPeek::Cycle),
        _ => None,
    })
}

/// One frame of input to the peek state machine.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct PeekInput {
    /// egui input clock, seconds.
    pub now: f64,
    /// Sidebar explicitly pinned open: the peek is irrelevant.
    pub pinned: bool,
    /// Pointer inside the rail's arming zone (rail minus the resize edge).
    pub in_arm_zone: bool,
    /// Pointer anywhere on the rail column (keeps an open flyout open).
    pub in_rail: bool,
    /// Pointer over the flyout's currently visible rect (+ `EDGE_SLOP`).
    pub in_flyout: bool,
    /// Pointer beyond the old pinned footprint (x past `PANEL_W`), or gone
    /// from the window — clears the just-collapsed re-arm guard.
    pub beyond_footprint: bool,
    /// Something that must not lose the flyout under it: an armed drag, an
    /// inline rename, an open context menu, or a press that began on it.
    pub hold: bool,
    /// A pointer press landed outside rail ∪ flyout this frame.
    pub pressed_outside: bool,
    /// Any pointer button is down (never ARM mid-gesture, e.g. a text
    /// selection sweeping left across the rail).
    pub buttons_down: bool,
}

/// What the frame should draw and schedule.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct PeekFrame {
    /// Flyout width progress, eased, 0..1 (× `FLYOUT_W`).
    pub width_t: f32,
    /// Label/content opacity, eased, 0..1.
    pub label_t: f32,
    /// Animation in flight: repaint next frame.
    pub animating: bool,
    /// A delay timer is pending: repaint after this many seconds (the
    /// pointer may be resting perfectly still, producing no events).
    pub wake_in: Option<f64>,
}

impl PeekFrame {
    pub fn visible(&self) -> bool {
        self.width_t > 0.0
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct RailPeek {
    /// Intent: true once the open delay elapsed, until a close rule fires.
    open: bool,
    /// Timeline position, linear 0..1 (eased on output).
    t: f32,
    /// Dwell start while arming.
    enter_at: Option<f64>,
    /// Away-since while open.
    leave_at: Option<f64>,
    /// Previous step's clock.
    last: Option<f64>,
    /// Whether the previous step was mid-animation (dt is trusted only then).
    was_animating: bool,
    /// Just-collapsed guard (see module docs).
    rearm: bool,
}

impl RailPeek {
    /// The user collapsed the sidebar: no peek until the pointer has left the
    /// old footprint once.
    pub fn require_rearm(&mut self) {
        *self = RailPeek {
            rearm: true,
            ..RailPeek::default()
        };
    }

    /// Drop the flyout instantly (the user pinned the sidebar from it — the
    /// real panel takes its place this frame).
    pub fn dismiss(&mut self) {
        *self = RailPeek::default();
    }

    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Current eased width progress (last step's), for hit-testing the
    /// visible flyout before this frame's step.
    pub fn width_t(&self) -> f32 {
        ease_out_cubic(span(self.t, WIDTH_SPAN))
    }

    pub fn step(&mut self, i: PeekInput) -> PeekFrame {
        if i.pinned {
            *self = RailPeek::default();
            return PeekFrame::default();
        }
        // dt: a frame that starts an animation from rest advances by at most
        // one nominal frame — the clock gap since the last (idle) frame is
        // not animation time, or the first painted frame would jump.
        let raw_dt = self.last.map_or(0.0, |l| (i.now - l).max(0.0)) as f32;
        let dt = if self.was_animating {
            raw_dt.min(0.05)
        } else {
            raw_dt.min(1.0 / 60.0)
        };
        self.last = Some(i.now);

        if self.rearm && i.beyond_footprint {
            self.rearm = false;
        }

        let mut wake_in = None;
        if self.open {
            self.enter_at = None;
            if i.hold || i.in_rail || i.in_flyout {
                self.leave_at = None;
            } else if i.pressed_outside {
                self.open = false;
                self.leave_at = None;
            } else {
                let since = *self.leave_at.get_or_insert(i.now);
                let left = CLOSE_DELAY - (i.now - since);
                if left <= TIMER_EPS {
                    self.open = false;
                    self.leave_at = None;
                } else {
                    wake_in = Some(left);
                }
            }
        } else {
            self.leave_at = None;
            if self.t > 0.0 && i.in_flyout && !i.pressed_outside {
                // Caught mid-fold: the pointer came back onto the visible
                // flyout — reverse at once, no second dwell.
                self.open = true;
                self.enter_at = None;
            } else if i.in_arm_zone && !self.rearm && !i.buttons_down {
                let since = *self.enter_at.get_or_insert(i.now);
                let left = OPEN_DELAY - (i.now - since);
                if left <= TIMER_EPS {
                    self.open = true;
                    self.enter_at = None;
                } else {
                    wake_in = Some(left);
                }
            } else {
                self.enter_at = None;
            }
        }

        let target = if self.open { 1.0 } else { 0.0 };
        if self.t < target {
            self.t = (self.t + dt / OPEN_DUR).min(1.0);
        } else if self.t > target {
            self.t = (self.t - dt / CLOSE_DUR).max(0.0);
        }
        // Still short of the target after this step ⇒ keep frames coming.
        // (A step that STARTS an animation with dt=0 lands here too.)
        let animating = self.t != target;
        self.was_animating = animating;

        PeekFrame {
            width_t: ease_out_cubic(span(self.t, WIDTH_SPAN)),
            label_t: smoothstep(span(self.t, LABEL_SPAN)),
            animating,
            wake_in,
        }
    }
}

fn span(t: f32, (a, b): (f32, f32)) -> f32 {
    ((t - a) / (b - a)).clamp(0.0, 1.0)
}

/// Decelerating entrance; run backward it accelerates away (the exit).
fn ease_out_cubic(x: f32) -> f32 {
    let u = 1.0 - x;
    1.0 - u * u * u
}

fn smoothstep(x: f32) -> f32 {
    x * x * (3.0 - 2.0 * x)
}

/// The flyout's full (fully open) rect: exactly the pinned sidebar's
/// footprint, starting at the rail's left edge — the rail GROWS into the
/// sidebar rather than a second panel appearing beside it.
pub(super) fn flyout_rect(rail: Rect) -> Rect {
    Rect::from_min_size(rail.min, Vec2::new(PANEL_W, rail.height()))
}

/// The flyout's currently visible rect: the rail's own column plus the grown
/// share of the remaining `PANEL_W - RAIL_W`.
pub(super) fn visible_rect(rail: Rect, width_t: f32) -> Rect {
    let grow = ((PANEL_W - RAIL_W) * width_t).round();
    Rect::from_min_max(rail.min, Pos2::new(rail.max.x + grow, rail.max.y))
}

/// Depth cue at the flyout's right edge: a gradient quad strip, black at
/// `SHADE_ALPHA` on the edge, ~35% of that at `SHADE_MID`, gone at
/// `SHADE_W`. It reads as a surface casting onto the terminal (the text
/// dims INTO the edge instead of being sliced off mid-glyph) — a tonal
/// falloff, never a line, and no blur pass: 6 vertices, 4 triangles.
const SHADE_W: f32 = 22.0;
const SHADE_MID: f32 = 7.0;
const SHADE_ALPHA: f32 = 120.0;

fn edge_shade(painter: &egui::Painter, vis: Rect, strength: f32) {
    let a0 = (SHADE_ALPHA * strength) as u8;
    let a1 = (SHADE_ALPHA * 0.35 * strength) as u8;
    if a0 == 0 {
        return;
    }
    let (x0, y0, y1) = (vis.max.x, vis.min.y, vis.max.y);
    let mut mesh = egui::Mesh::default();
    let cols = [
        Color32::from_black_alpha(a0),
        Color32::from_black_alpha(a1),
        Color32::TRANSPARENT,
    ];
    for (x, c) in [x0, x0 + SHADE_MID, x0 + SHADE_W].into_iter().zip(cols) {
        mesh.colored_vertex(Pos2::new(x, y0), c);
        mesh.colored_vertex(Pos2::new(x, y1), c);
    }
    for k in 0..2u32 {
        let i = 2 * k;
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 1, i + 3, i + 2);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Paint the flyout over the terminal and run `add_contents` inside it.
///
/// An `Area` (Order::Middle): above the panels, below popups/modals. It
/// covers the rail's column too, but paints NO fill there — the rail panel
/// underneath is the same surface, and its dots stay visible while the
/// width grows, then cross-fade out (the caller fades the rail by
/// `1 - label_t`) as the tree's own rows fade in at `label_t`. One column
/// of dots at any moment, never two lists side by side.
///
/// The contents are laid out at the FULL width from the first frame — the
/// visible width only clips them, so nothing re-flows while it grows — and
/// the clip also bounds interaction (a clipped-away row is not clickable).
/// The Area's own extent is the visible rect, so a half-folded flyout never
/// swallows clicks meant for the terminal beside it.
pub(super) fn show_flyout(
    ctx: &egui::Context,
    rail: Rect,
    frame: &PeekFrame,
    fill: Color32,
    add_contents: impl FnOnce(&mut egui::Ui),
) -> Rect {
    let full = flyout_rect(rail);
    let vis = visible_rect(rail, frame.width_t);
    egui::Area::new(Id::new("sidebar-peek"))
        .order(Order::Middle)
        .fixed_pos(full.min)
        .constrain(false)
        .fade_in(false)
        .movable(false)
        .show(ctx, |ui| {
            let grown = Rect::from_min_max(Pos2::new(rail.max.x, vis.min.y), vis.max);
            if grown.width() > 0.5 {
                ui.painter()
                    .rect_filled(grown, egui::CornerRadius::ZERO, fill);
                edge_shade(ui.painter(), vis, frame.width_t);
            }
            // Contents are invisible until the labels start (the width-only
            // first ~90ms of an open, the last of a close): skip them then.
            if frame.label_t > 0.0 {
                let mut inner = ui.new_child(UiBuilder::new().max_rect(full));
                inner.set_clip_rect(vis);
                inner.multiply_opacity(frame.label_t);
                add_contents(&mut inner);
            }
            // The Area's extent (hit-testing + layer hover) = visible rect.
            ui.advance_cursor_after_rect(vis);
        });
    vis
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: f64 = 1.0 / 60.0;

    fn idle(now: f64) -> PeekInput {
        PeekInput {
            now,
            beyond_footprint: true,
            ..Default::default()
        }
    }
    fn on_rail(now: f64) -> PeekInput {
        PeekInput {
            now,
            in_arm_zone: true,
            in_rail: true,
            ..Default::default()
        }
    }
    fn on_flyout(now: f64) -> PeekInput {
        PeekInput {
            now,
            in_flyout: true,
            ..Default::default()
        }
    }

    /// Drive `f(now)` at 60fps from `t0` for `secs`; returns the last frame.
    fn run(p: &mut RailPeek, t0: f64, secs: f64, f: impl Fn(f64) -> PeekInput) -> PeekFrame {
        let mut now = t0;
        let mut last = PeekFrame::default();
        while now <= t0 + secs + 1e-9 {
            last = p.step(f(now));
            now += FRAME;
        }
        last
    }

    #[test]
    fn crossing_the_rail_never_opens() {
        let mut p = RailPeek::default();
        // 100ms on the rail (a slow pass-through) then gone.
        let f = run(&mut p, 0.0, 0.10, on_rail);
        assert!(!p.is_open());
        assert_eq!(f.width_t, 0.0);
        assert!(f.wake_in.is_some(), "dwell timer must schedule a wakeup");
        let f = run(&mut p, 0.11, 1.0, idle);
        assert!(!p.is_open() && f.width_t == 0.0 && !f.animating);
    }

    #[test]
    fn dwell_opens_after_open_delay() {
        let mut p = RailPeek::default();
        let f = p.step(on_rail(0.0));
        assert_eq!(f.wake_in, Some(OPEN_DELAY));
        assert!(!p.step(on_rail(OPEN_DELAY - 0.01)).visible());
        assert!(!p.is_open());
        p.step(on_rail(OPEN_DELAY));
        assert!(p.is_open(), "opens exactly at the dwell threshold");
        // Pointer resting still: the wakeup lands on the deadline even with
        // no events in between (the clock jump is not animation time).
        let mut p = RailPeek::default();
        p.step(on_rail(0.0));
        let f = p.step(on_rail(OPEN_DELAY));
        assert!(p.is_open() && f.animating);
        // The 150ms idle gap is NOT animation time: one nominal frame only.
        let one_frame = ease_out_cubic(span((1.0 / 60.0) / OPEN_DUR, WIDTH_SPAN));
        assert!(
            f.width_t <= one_frame + 1e-6,
            "first frame advances ≤1 frame ({one_frame}), got {}",
            f.width_t
        );
    }

    #[test]
    fn width_leads_labels_on_open_and_trails_on_close() {
        let mut p = RailPeek::default();
        p.step(on_rail(0.0));
        let mut now = OPEN_DELAY;
        let mut saw_width_before_labels = false;
        loop {
            let f = p.step(on_rail(now));
            if f.width_t > 0.5 && f.label_t == 0.0 {
                saw_width_before_labels = true;
            }
            assert!(f.label_t <= f.width_t + 1e-6, "labels never lead the width");
            if !f.animating {
                assert_eq!((f.width_t, f.label_t), (1.0, 1.0));
                break;
            }
            now += FRAME;
        }
        assert!(saw_width_before_labels);
        // Settles inside OPEN_DUR (+1 frame of start-from-rest).
        assert!(now - OPEN_DELAY <= OPEN_DUR as f64 + 2.0 * FRAME);

        // Close: press outside → labels gone before width starts folding.
        let mut f = p.step(PeekInput {
            pressed_outside: true,
            ..idle(now + FRAME)
        });
        now += FRAME;
        let mut saw_labels_gone_wide = false;
        while f.animating {
            now += FRAME;
            f = p.step(idle(now));
            if f.label_t < 0.05 && f.width_t > 0.9 {
                saw_labels_gone_wide = true;
            }
        }
        assert!(saw_labels_gone_wide, "labels fade out before the width folds");
        assert_eq!((f.width_t, f.label_t), (0.0, 0.0));
    }

    #[test]
    fn close_is_forgiving_then_folds() {
        let mut p = RailPeek::default();
        run(&mut p, 0.0, 0.6, on_rail);
        assert!(p.is_open());
        // A 250ms excursion off the flyout (overshoot) then back: stays.
        run(&mut p, 0.61, 0.25, idle);
        assert!(p.is_open(), "excursion shorter than CLOSE_DELAY keeps it open");
        run(&mut p, 0.87, 0.10, on_flyout);
        assert!(p.is_open());
        // Leaving for good: closed once CLOSE_DELAY has elapsed, not before.
        let t_leave = 0.98;
        run(&mut p, t_leave, CLOSE_DELAY - 0.05, idle);
        assert!(p.is_open());
        run(&mut p, t_leave + CLOSE_DELAY - 0.05 + FRAME, 0.1, idle);
        assert!(!p.is_open());
        let f = run(&mut p, t_leave + 0.5, 0.5, idle);
        assert!(!f.visible() && !f.animating);
    }

    #[test]
    fn press_outside_closes_immediately_but_hold_wins() {
        let mut p = RailPeek::default();
        run(&mut p, 0.0, 0.6, on_rail);
        // Held (context menu open): a press outside — on the menu — keeps it.
        p.step(PeekInput {
            hold: true,
            pressed_outside: true,
            ..idle(0.62)
        });
        assert!(p.is_open());
        // Hold also suspends the close timer indefinitely.
        run(&mut p, 0.63, 2.0, |now| PeekInput { hold: true, ..idle(now) });
        assert!(p.is_open());
        // Unheld press on the terminal: closes this frame.
        p.step(PeekInput {
            pressed_outside: true,
            ..idle(2.7)
        });
        assert!(!p.is_open());
    }

    #[test]
    fn pinned_always_wins() {
        let mut p = RailPeek::default();
        run(&mut p, 0.0, 0.6, on_rail);
        assert!(p.is_open());
        let f = p.step(PeekInput {
            pinned: true,
            ..on_rail(0.62)
        });
        assert_eq!(f, PeekFrame::default(), "pinned ⇒ no flyout, no timers");
        assert!(!p.is_open());
        // Hovering the (absent) rail while pinned never arms anything.
        let f = run(&mut p, 0.63, 1.0, |now| PeekInput { pinned: true, ..on_rail(now) });
        assert_eq!(f, PeekFrame::default());
    }

    #[test]
    fn just_collapsed_needs_rearm() {
        let mut p = RailPeek::default();
        p.require_rearm();
        let f = run(&mut p, 0.0, 1.0, on_rail);
        assert!(!p.is_open() && !f.visible() && f.wake_in.is_none());
        // Out to the terminal and back: peeks normally.
        p.step(idle(1.1));
        run(&mut p, 1.2, 0.3, on_rail);
        assert!(p.is_open());
    }

    #[test]
    fn no_arming_mid_gesture() {
        let mut p = RailPeek::default();
        run(&mut p, 0.0, 1.0, |now| PeekInput {
            buttons_down: true,
            ..on_rail(now)
        });
        assert!(!p.is_open());
    }

    #[test]
    fn resize_edge_does_not_arm_but_keeps_open() {
        // in_rail without in_arm_zone = the OS resize band.
        let mut p = RailPeek::default();
        run(&mut p, 0.0, 1.0, |now| PeekInput {
            in_rail: true,
            ..idle(now)
        });
        assert!(!p.is_open());
        run(&mut p, 1.1, 0.4, on_rail);
        assert!(p.is_open());
        run(&mut p, 1.6, 1.0, |now| PeekInput {
            in_rail: true,
            ..idle(now)
        });
        assert!(p.is_open(), "the resize band still counts as on the rail");
    }

    #[test]
    fn caught_mid_fold_reopens_without_second_dwell() {
        let mut p = RailPeek::default();
        run(&mut p, 0.0, 0.6, on_rail);
        p.step(PeekInput {
            pressed_outside: true,
            ..idle(0.61)
        });
        let f = p.step(idle(0.61 + FRAME));
        assert!(f.visible() && !p.is_open());
        p.step(on_flyout(0.61 + 2.0 * FRAME));
        assert!(p.is_open(), "pointer back on the visible flyout reverses the fold");
    }

    #[test]
    fn settled_states_request_no_frames() {
        // No repaint storm: open-and-still and closed-and-away both settle to
        // zero scheduled work.
        let mut p = RailPeek::default();
        let f = run(&mut p, 0.0, 1.0, on_rail);
        assert!(!f.animating && f.wake_in.is_none());
        let f = run(&mut p, 1.1, 1.0, idle);
        assert!(!f.animating && f.wake_in.is_none());
    }

    /// Frame-driven: the app only paints when the machine asks (animation
    /// ⇒ next frame, timer ⇒ at its deadline). Twelve full open/close cycles
    /// at 60Hz must cost exactly the animation frames plus a couple of timer
    /// wakeups each — no storm, and no stall: a wakeup landing a float hair
    /// before its deadline must still fire (it once re-armed a 5e-17s wakeup
    /// forever).
    #[test]
    fn frames_are_bounded_by_the_animation() {
        let mut p = RailPeek::default();
        let half = 0.7;
        let mut now = 0.0f64;
        let mut frames = 0u32;
        while now < 12.0 * 2.0 * half {
            let phase = now % (2.0 * half);
            let on = phase < half;
            let f = p.step(PeekInput {
                now,
                in_arm_zone: on,
                in_rail: on,
                ..Default::default()
            });
            frames += 1;
            assert!(frames < 10_000, "stalled at now={now}: {f:?}");
            // External driver: the phase flip itself is one wakeup.
            let mut next = half - phase % half + 1e-3;
            if f.animating {
                next = next.min(FRAME);
            }
            if let Some(w) = f.wake_in {
                next = next.min(w);
            }
            now += next;
        }
        let per_cycle = frames as f64 / 12.0;
        let anim = ((OPEN_DUR + CLOSE_DUR) as f64 / FRAME).ceil();
        assert!(
            per_cycle <= anim + 8.0,
            "{per_cycle:.1} frames/cycle vs {anim} animation frames"
        );
    }

    /// The rail becomes the sidebar: the flyout starts as exactly the rail
    /// column and, fully open, is exactly the pinned panel's footprint — so
    /// pinning from it moves no pixel of the sidebar.
    #[test]
    fn flyout_grows_from_the_rail_into_the_pinned_footprint() {
        let rail = Rect::from_min_size(Pos2::new(0.0, 36.0), Vec2::new(RAIL_W, 700.0));
        assert_eq!(visible_rect(rail, 0.0), rail);
        let open = visible_rect(rail, 1.0);
        assert_eq!(open, Rect::from_min_size(rail.min, Vec2::new(PANEL_W, 700.0)));
        assert_eq!(open, flyout_rect(rail));
        let mid = visible_rect(rail, 0.5);
        assert!(mid.min == rail.min && mid.max.x > rail.max.x && mid.max.x < open.max.x);
    }

    /// THE non-negotiable: the flyout overlays the terminal, it never
    /// displaces it. Real egui panels in the app's order (titlebar, rail
    /// panel at `RAIL_W`, central card) with the real `show_flyout` drawn at
    /// every point of its timeline; the central panel's grid area — and the
    /// cols/rows a real `TermBackend::resize_to` derives from it — must be
    /// identical throughout, and `resize_to` must report no change (i.e. no
    /// PTY resize would be sent) after the initial sizing.
    #[test]
    fn flyout_never_moves_the_grid() {
        use crate::gui::term_backend::{GridSize, TermBackend};
        use crate::gui::term_view;

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0));
        let cell = Vec2::new(8.0, 17.0);
        let mut backend = TermBackend::new(GridSize::default());
        type Out = (Rect, Option<(u16, u16)>, Rect, (u16, u16));
        // `as_panel` = the REJECTED design (the flyout as a growing panel):
        // the negative control proving this harness can see displacement.
        let mut frame_at = |p: &PeekFrame, now: f64, as_panel: bool| -> Out {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                time: Some(now),
                ..Default::default()
            };
            let mut central = Rect::NOTHING;
            let mut rail = Rect::NOTHING;
            let _ = ctx.run_ui(raw, |ui| {
                let mut cui = ui.new_child(UiBuilder::new().max_rect(screen));
                egui::Panel::top("t")
                    .exact_size(36.0)
                    .show(&mut cui, |_| {});
                let w = if as_panel {
                    RAIL_W + (PANEL_W - RAIL_W) * p.width_t
                } else {
                    RAIL_W
                };
                let r = egui::Panel::left("sidebar")
                    .resizable(false)
                    .default_size(w)
                    .min_size(w)
                    .max_size(w)
                    .show(&mut cui, |_| {});
                rail = r.response.rect;
                if p.visible() && !as_panel {
                    show_flyout(ui.ctx(), rail, p, Color32::BLACK, |ui| {
                        for _ in 0..30 {
                            ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), 46.0),
                                egui::Sense::click(),
                            );
                        }
                    });
                }
                egui::CentralPanel::default().show(&mut cui, |ui| {
                    central = ui.available_rect_before_wrap();
                });
            });
            let layout = term_view::grid_inner_size(central.size());
            let resized = backend.resize_to(layout, cell);
            (central, resized, rail, (backend.size.cols, backend.size.rows))
        };

        let (c0, first, rail, dims) = frame_at(&PeekFrame::default(), 0.0, false);
        assert!(first.is_some(), "initial sizing commits once");
        assert!((rail.width() - RAIL_W).abs() < 0.5, "rail panel is RAIL_W");

        // A full open → close cycle through the real state machine.
        let mut peek = RailPeek::default();
        let mut now = 0.0;
        let mut max_w = 0.0f32;
        for i in 0..120 {
            now += FRAME;
            let inp = if i < 60 { on_rail(now) } else { idle(now) };
            let f = peek.step(inp);
            max_w = max_w.max(f.width_t);
            let (c, resized, _, d) = frame_at(&f, now, false);
            assert_eq!(c, c0, "central rect moved at frame {i} (width_t {})", f.width_t);
            assert_eq!(resized, None, "a PTY resize would fire at frame {i}");
            assert_eq!(d, dims);
        }
        assert!(max_w > 0.99, "the cycle really opened the flyout");
        assert!(!peek.is_open());

        // Negative control: the same open frame as a PANEL moves the grid
        // and fires a resize — so the equalities above are not vacuous.
        let open = PeekFrame {
            width_t: 1.0,
            label_t: 1.0,
            ..Default::default()
        };
        let (c, resized, _, d) = frame_at(&open, now + FRAME, true);
        assert_ne!(c, c0);
        assert!(resized.is_some() && d != dims);
    }
}

