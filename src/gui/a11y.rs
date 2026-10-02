//! Accessibility-tree safety: keep egui's focused widget id and the AccessKit
//! node list in agreement.
//!
//! ## Why this module exists (field crash, v0.1.18)
//!
//! `eframe` is built with the `accesskit` feature, so every frame egui hands
//! the platform an `accesskit::TreeUpdate` whose `focus` field is
//! `memory.focused()` and whose `nodes` are only the widgets that were
//! actually *painted* this pass (egui `context.rs`, the `accesskit` block in
//! `Context::end_pass`). `accesskit_consumer::tree::State::validate_global`
//! then asserts `nodes.contains_key(&focus)` and **panics** otherwise:
//!
//! ```text
//! GUI PANIC at accesskit_consumer-0.35.0/src/tree.rs:71:13:
//!   Focused ID #5772719745776736982 is not in the node list
//! ```
//!
//! egui *does* have a dead-man's switch for a focused widget that disappears
//! (`memory/mod.rs`, `Focus::end_pass`), but it deliberately does not fire on
//! the frame focus was requested:
//!
//! ```ignore
//! let recently_gained_focus = self.id_previous_frame != Some(focused_widget.id);
//! if !recently_gained_focus && !used_ids.contains_key(&focused_widget.id) {
//!     self.focused_widget = None; // only when it is NOT fresh
//! }
//! ```
//!
//! That exemption exists so `request_focus` may be called one frame and the
//! widget added the next. The cost is that a `request_focus(id)` for a widget
//! which is **not painted in that same pass** survives into the AccessKit
//! update as a focus id with no node — an immediate panic, and the whole app
//! dies. The crash is latent in every build; it only becomes visible once an
//! assistive-technology client attaches (the AccessKit Windows adapter stays
//! inert, and skips validation, until something sends `WM_GETOBJECT`), which
//! is why the field log shows it twice in a row on one machine and never on
//! others.
//!
//! ## The guard
//!
//! [`seal_focus`] runs once per frame, immediately before `Context::end_pass`
//! builds the tree, and drops focus when the focused id was not painted in
//! this pass. It is the same dead-man's switch egui already has, minus the
//! `recently_gained_focus` exemption, which we cannot honour: by the time the
//! exemption would pay off (next frame) the app has already crashed.
//!
//! It is *not* a `catch_unwind` around the panic: the invariant is restored at
//! the source, so AccessKit is handed a tree that satisfies its contract.
//!
//! Fixes at the call sites (focus is no longer requested for widgets that are
//! not painted) remove the individual triggers; this guard makes the class
//! unreachable, including for triggers we have not found.

use egui::{Context, Id};

/// Did `id` get painted in the pass that is currently open?
///
/// egui keeps this pass's AccessKit node map private (`Context::pass_state` is
/// `pub(crate)`), but it exposes `Context::accesskit_node_builder`, which
/// hands out the node for an id — *creating an empty one if absent*. Every
/// widget that actually produced a `Response` this pass had
/// `Response::fill_accesskit_node_common` run on its node, and that always
/// sets `bounds` (egui `response.rs:892`). A node AccessKit never saw is
/// `accesskit::Node::default()`, whose `bounds()` is `None` (asserted by
/// accesskit's own test suite). So `bounds.is_some()` is exactly "painted this
/// pass".
///
/// Returns `None` when AccessKit is disabled — then there is no tree to keep
/// consistent and nothing for a caller to do.
///
/// The probe may leave behind one inert `Role::Unknown` node parented at the
/// tree root. That is deliberate belt-and-braces: even if a caller ignores the
/// answer, the focused node now *exists*, so AccessKit's invariant holds.
fn painted_this_pass(ctx: &Context, id: Id) -> Option<bool> {
    ctx.accesskit_node_builder(id, |node| node.bounds().is_some())
}

/// Drop `memory.focused()` when that widget was not painted in the pass that
/// is about to end, so the AccessKit `TreeUpdate` cannot name a focus id that
/// is absent from its node list.
///
/// Call once per frame, as late as possible — after all UI has been laid out
/// and before `Context::end_pass` runs. Returns the id that was cleared, for
/// tests and tracing.
pub fn seal_focus(ctx: &Context) -> Option<Id> {
    let focused = ctx.memory(|m| m.focused())?;
    // AccessKit off ⇒ no tree, no invariant to protect: leave focus alone.
    if painted_this_pass(ctx, focused) != Some(false) {
        return None;
    }
    ctx.memory_mut(|m| m.surrender_focus(focused));
    log::debug!("a11y: dropped focus on unpainted widget {focused:?}");
    Some(focused)
}

/// Install [`seal_focus`] as an egui end-of-pass plugin. Call once, on the
/// context, before the first frame.
///
/// egui runs `on_end_pass` callbacks after the app's UI closure and before
/// `Context::end_pass` builds the AccessKit `TreeUpdate` (egui
/// `context.rs::run_ui_dyn`), and it runs them for *every* pass of a
/// multi-pass frame. That is the only place a single hook can cover the whole
/// app — including passes where `eframe` skips `App::ui` (hidden window) and
/// the extra passes egui runs after a `request_discard`.
pub fn install(ctx: &Context) {
    ctx.on_end_pass(
        "pulse_a11y_seal_focus",
        std::sync::Arc::new(|ui: &mut egui::Ui| {
            seal_focus(ui.ctx());
        }),
    );
}

/// A focus request that is replayed until the target widget actually paints.
///
/// The pattern this replaces is "toggle a panel open and immediately
/// `request_focus` the text field inside it". On that frame the field has not
/// been laid out yet (the toggle is handled *after* the panel body for the
/// current frame), so the request names an unpainted id. Storing the intent
/// and letting the widget claim it with `Response::request_focus()` on the
/// frame it is painted is both crash-free and what the user wants: the caret
/// lands in the field as soon as it is on screen.
///
/// `take_if` is the whole API: the widget asks "was focus meant for me?" on
/// the frame it paints, and the request is consumed only then. A request that
/// is never claimed is dropped after [`Self::EXPIRY_FRAMES`] frames so a
/// closed panel does not steal focus later.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FocusRequest {
    target: Option<Id>,
    age: u32,
}

impl FocusRequest {
    /// A pending request is abandoned after this many frames without the
    /// target painting. Two frames is enough for "open panel now, body paints
    /// next frame" while being short enough that a request cannot survive a
    /// surface being closed.
    pub const EXPIRY_FRAMES: u32 = 2;

    /// Ask for `id` to receive focus on the next frame it is painted.
    pub fn arm(&mut self, id: Id) {
        self.target = Some(id);
        self.age = 0;
    }

    /// Forget any pending request. Called when the surface that owns the
    /// target closes, so focus is never handed to a widget that is gone.
    pub fn clear(&mut self) {
        self.target = None;
        self.age = 0;
    }

    /// True while a request is outstanding.
    pub fn is_armed(&self) -> bool {
        self.target.is_some()
    }

    /// Consume the request if it was meant for `id`. Call this from the
    /// widget's own paint site and feed the result to
    /// `Response::request_focus()`.
    pub fn take_if(&mut self, id: Id) -> bool {
        if self.target == Some(id) {
            self.clear();
            true
        } else {
            false
        }
    }

    /// Age the pending request one frame; drops it once it expires. Call once
    /// per frame after the UI has had its chance to claim it.
    pub fn tick(&mut self) {
        if self.target.is_some() {
            self.age += 1;
            if self.age > Self::EXPIRY_FRAMES {
                log::debug!("a11y: focus request {:?} expired unclaimed", self.target);
                self.clear();
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use egui::accesskit;

    /// Run one headless egui frame with AccessKit on and return the tree update
    /// egui would hand the platform. `body` is the whole frame, exactly as
    /// `eframe` would call `App::update`; `seal_focus` is called by the body
    /// where the app calls it (last, before the pass ends).
    fn frame(ctx: &Context, mut body: impl FnMut(&mut egui::Ui)) -> accesskit::TreeUpdate {
        ctx.enable_accesskit();
        let out = ctx.run_ui(Default::default(), |ui| body(ui));
        out.platform_output
            .accesskit_update
            .expect("accesskit enabled ⇒ an update every pass")
    }

    /// The invariant `accesskit_consumer::tree::State::validate_global`
    /// enforces, checked without pulling the consumer crate in as a
    /// dependency: the field panic *is* this assertion failing.
    fn assert_focus_present(u: &accesskit::TreeUpdate) {
        let present = u.nodes.iter().any(|(id, _)| *id == u.focus);
        assert!(
            present,
            "Focused ID {:?} is not in the node list ({} nodes) \
             — this is the accesskit_consumer tree.rs:71 panic",
            u.focus,
            u.nodes.len()
        );
    }

    fn focus_is_absent(u: &accesskit::TreeUpdate) -> bool {
        !u.nodes.iter().any(|(id, _)| *id == u.focus)
    }

    /// REGRESSION (the v0.1.18 field crash): request focus for a widget that is
    /// not painted and egui emits an AccessKit update whose `focus` has no
    /// node. egui's own dead-man's switch (`memory/mod.rs` `Focus::end_pass`)
    /// does not fire, because the request counts as "recently gained".
    ///
    /// This test asserts the *bug* still exists in egui, so the guard below is
    /// known to be load-bearing rather than dead code. If a future egui fixes
    /// it, this test fails and the guard can be reconsidered.
    #[test]
    fn egui_propagates_focus_for_an_unpainted_widget() {
        let ctx = Context::default();
        let ghost = Id::new("a-widget-that-is-never-painted");
        let u = frame(&ctx, |ui| {
            ui.label("something, so the tree is not empty");
            ui.ctx().memory_mut(|m| m.request_focus(ghost));
        });
        assert_eq!(
            u.focus,
            ghost.accesskit_id(),
            "egui forwards the stale focus id verbatim"
        );
        assert!(focus_is_absent(&u), "…and the node is genuinely missing");
    }

    /// The fix: the same frame, with `seal_focus` as the last thing before the
    /// pass ends, yields a tree that satisfies AccessKit.
    #[test]
    fn seal_focus_repairs_the_unpainted_focus_target() {
        let ctx = Context::default();
        let ghost = Id::new("a-widget-that-is-never-painted");
        let u = frame(&ctx, |ui| {
            ui.label("something, so the tree is not empty");
            ui.ctx().memory_mut(|m| m.request_focus(ghost));
            assert_eq!(
                seal_focus(ui.ctx()),
                Some(ghost),
                "the guard must notice the ghost"
            );
        });
        assert_focus_present(&u);
    }

    /// The real shape of the crash: a text field is painted and focused, then
    /// the surface holding it stops being painted on a later frame while the
    /// focus request still fires (`launcher.rs` toggles, `central.rs` holds).
    #[test]
    fn surface_torn_down_while_focus_is_re_requested() {
        let ctx = Context::default();
        let field = Id::new("launcher_ssh_host");
        let mut buf = String::new();

        // Frame 1: the field exists and takes focus.
        let u1 = frame(&ctx, |ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut buf).id(field));
            r.request_focus();
            seal_focus(ui.ctx());
        });
        assert_focus_present(&u1);

        // Frame 2: the launcher is gone, yet focus is requested for its field.
        let u2 = frame(&ctx, |ui| {
            ui.label("launcher closed; a terminal was created");
            ui.ctx().memory_mut(|m| m.request_focus(field));
            seal_focus(ui.ctx());
        });
        assert_focus_present(&u2);
    }

    /// Where egui's dead-man's switch DOES save us, documented so the guard's
    /// scope is honest: a widget that merely *stops* being painted while it
    /// already held focus is cleared by egui itself (`id_previous_frame ==
    /// Some(id)` ⇒ not "recently gained" ⇒ the switch fires). Only a FRESH
    /// focus request for an unpainted id slips through — which is exactly the
    /// launcher-row click below.
    #[test]
    fn egui_itself_handles_a_focused_widget_that_merely_disappears() {
        let ctx = Context::default();
        let field = Id::new("launcher_ssh_host");
        let mut buf = String::new();
        frame(&ctx, |ui| {
            ui.add(egui::TextEdit::singleline(&mut buf).id(field))
                .request_focus();
        });
        let u2 = frame(&ctx, |ui| {
            ui.label("launcher closed");
        });
        assert_focus_present(&u2);
        assert_ne!(u2.focus, field.accesskit_id(), "egui dropped it by itself");
    }

    /// THE FIELD CRASH, exactly as it happened (gui.log 1607 & 1655, focus id
    /// 5772719745776736982 = `Id::new("launcher_ssh_host")`).
    ///
    /// The launcher's "SSH to…" row click is *collected* inside the ScrollArea
    /// body and *applied* after it, so the ssh expansion — and its host field
    /// — cannot have been painted on that frame. The old code called
    /// `request_focus(Id::new("launcher_ssh_host"))` right there: a fresh
    /// request for an id with no node. Reproduced here without the guard.
    #[test]
    fn launcher_ssh_row_click_is_the_field_crash() {
        let ctx = Context::default();
        let query = Id::new("launcher_q");
        let host = Id::new("launcher_ssh_host");
        assert_eq!(
            host.value(),
            5_772_719_745_776_736_982,
            "the id in the field panic"
        );
        let mut q = String::new();

        // Frame 1: launcher open, the query field holds focus.
        let u1 = frame(&ctx, |ui| {
            ui.add(egui::TextEdit::singleline(&mut q).id(query))
                .request_focus();
        });
        assert_focus_present(&u1);

        // Frame 2: the row is clicked. `ssh_open` flips AFTER the body, so the
        // host field is not painted; v0.1.18 focused it anyway.
        let u2 = frame(&ctx, |ui| {
            ui.add(egui::TextEdit::singleline(&mut q).id(query));
            ui.ctx().memory_mut(|m| m.request_focus(host));
        });
        assert_eq!(u2.focus, host.accesskit_id());
        assert!(
            focus_is_absent(&u2),
            "reproduces accesskit_consumer tree.rs:71"
        );
    }

    /// The same click with the guard in place: no violation.
    #[test]
    fn launcher_ssh_row_click_is_safe_with_the_guard() {
        let ctx = Context::default();
        let query = Id::new("launcher_q");
        let host = Id::new("launcher_ssh_host");
        let mut q = String::new();
        frame(&ctx, |ui| {
            ui.add(egui::TextEdit::singleline(&mut q).id(query))
                .request_focus();
            seal_focus(ui.ctx());
        });
        let u2 = frame(&ctx, |ui| {
            ui.add(egui::TextEdit::singleline(&mut q).id(query));
            ui.ctx().memory_mut(|m| m.request_focus(host));
            assert_eq!(seal_focus(ui.ctx()), Some(host));
        });
        assert_focus_present(&u2);
    }

    /// And the same click done the NEW way — armed, then claimed by the field
    /// when the expansion really paints — needs no guard at all and still puts
    /// the caret where the user expects it.
    #[test]
    fn launcher_ssh_row_click_armed_lands_in_the_field() {
        let ctx = Context::default();
        let query = Id::new("launcher_q");
        let host = Id::new("launcher_ssh_host");
        let mut req = FocusRequest::default();
        let mut q = String::new();
        let mut h = String::new();
        let mut ssh_open = false;

        // Frame 1: row clicked. Nothing is focused that does not exist.
        let u1 = frame(&ctx, |ui| {
            let te = ui.add(egui::TextEdit::singleline(&mut q).id(query));
            if ssh_open {
                let r = ui.add(egui::TextEdit::singleline(&mut h).id(host));
                if req.take_if(host) {
                    r.request_focus();
                }
            } else if !req.is_armed() {
                te.request_focus();
            }
            ssh_open = true;
            req.arm(host);
            req.tick();
        });
        assert_focus_present(&u1);

        // Frame 2: the expansion paints and claims the request.
        let u2 = frame(&ctx, |ui| {
            ui.add(egui::TextEdit::singleline(&mut q).id(query));
            let r = ui.add(egui::TextEdit::singleline(&mut h).id(host));
            if req.take_if(host) {
                r.request_focus();
            }
            req.tick();
        });
        assert_focus_present(&u2);
        assert_eq!(u2.focus, host.accesskit_id(), "caret lands in the host field");
    }

    /// A focused widget that keeps painting must not be disturbed — the guard
    /// may not break the composer's per-frame focus hold.
    #[test]
    fn seal_focus_leaves_a_live_focused_widget_alone() {
        let ctx = Context::default();
        let field = Id::new("composer-ish");
        let mut buf = String::new();
        for pass in 0..3 {
            let u = frame(&ctx, |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut buf).id(field));
                if !r.has_focus() {
                    r.request_focus();
                }
                assert_eq!(
                    seal_focus(ui.ctx()),
                    None,
                    "pass {pass}: the guard must not steal focus from a live widget"
                );
            });
            assert_focus_present(&u);
            if pass > 0 {
                assert_eq!(
                    u.focus,
                    field.accesskit_id(),
                    "pass {pass}: focus is held across frames"
                );
            }
        }
    }

    /// An idle frame (no focus at all) is fine: egui falls back to the root.
    #[test]
    fn no_focus_is_not_a_violation() {
        let ctx = Context::default();
        let u = frame(&ctx, |ui| {
            ui.label("idle");
            assert_eq!(seal_focus(ui.ctx()), None);
        });
        assert_focus_present(&u);
    }

    /// Every `Id::new(..)` the GUI focuses by hand, run through the crash
    /// shape: focus it on a frame where it is not painted, and the tree must
    /// still be valid. This is the blanket guarantee — a new focus site added
    /// later is covered by the guard whether or not anyone adds a test.
    #[test]
    fn every_hand_focused_id_is_safe_when_absent() {
        for name in [
            "launcher_q",
            "launcher_ssh_host",
            "launcher_custom_prog",
            "launcher_ssh_hooks",
            "launcher_folder_chip",
            "launcher_folder_menu",
            "inline-rename",
            "tc-settings",
            "tc-dialog",
            "update-popover",
        ] {
            let ctx = Context::default();
            let id = Id::new(name);
            let u = frame(&ctx, |ui| {
                ui.label("a frame without that widget");
                ui.ctx().memory_mut(|m| m.request_focus(id));
                seal_focus(ui.ctx());
            });
            assert_focus_present(&u);
        }
    }

    /// With AccessKit disabled there is no tree and the guard must be inert —
    /// it must not change focus behaviour for a build without the feature.
    #[test]
    fn guard_is_inert_without_accesskit() {
        let ctx = Context::default();
        let ghost = Id::new("ghost");
        let _ = ctx.run_ui(Default::default(), |ui| {
            ui.label("no accesskit");
            ui.ctx().memory_mut(|m| m.request_focus(ghost));
            assert_eq!(seal_focus(ui.ctx()), None, "nothing to protect");
            assert_eq!(
                ui.ctx().memory(|m| m.focused()),
                Some(ghost),
                "focus behaviour is untouched without accesskit"
            );
        });
    }

    /// The deferred request lands on the frame the widget first paints, which
    /// is what the launcher's "open the panel, focus its field" wants.
    #[test]
    fn focus_request_is_claimed_on_the_frame_the_widget_appears() {
        let mut req = FocusRequest::default();
        let field = Id::new("launcher_ssh_host");
        req.arm(field);
        assert!(
            !req.take_if(Id::new("launcher_q")),
            "a different widget cannot take it"
        );
        req.tick();
        assert!(req.is_armed(), "still pending for the field");
        assert!(req.take_if(field));
        assert!(!req.is_armed(), "consumed exactly once");
        assert!(!req.take_if(field), "not replayed forever");
    }

    /// An unclaimed request expires instead of ambushing a later surface.
    #[test]
    fn focus_request_expires_when_never_claimed() {
        let mut req = FocusRequest::default();
        req.arm(Id::new("gone"));
        for _ in 0..=FocusRequest::EXPIRY_FRAMES {
            req.tick();
        }
        assert!(
            !req.is_armed(),
            "expired after {} frames",
            FocusRequest::EXPIRY_FRAMES
        );
    }

    /// Closing the surface clears the pending request.
    #[test]
    fn focus_request_cleared_on_surface_close() {
        let mut req = FocusRequest::default();
        req.arm(Id::new("launcher_custom_prog"));
        req.clear();
        assert!(!req.is_armed());
        assert!(!req.take_if(Id::new("launcher_custom_prog")));
    }

    /// A deferred request, driven end-to-end through two real egui frames: the
    /// panel opens on frame 1 (field not painted yet), the field paints on
    /// frame 2 and claims focus. The tree is valid on both.
    #[test]
    fn deferred_request_drives_a_real_two_frame_open() {
        let ctx = Context::default();
        let field = Id::new("launcher_ssh_host");
        let mut req = FocusRequest::default();
        let mut open = false;
        let mut buf = String::new();

        let u1 = frame(&ctx, |ui| {
            // The toggle is handled after the body, as in launcher.rs.
            if open {
                let r = ui.add(egui::TextEdit::singleline(&mut buf).id(field));
                if req.take_if(field) {
                    r.request_focus();
                }
            }
            open = true;
            req.arm(field);
            req.tick();
            seal_focus(ui.ctx());
        });
        assert_focus_present(&u1);
        assert!(req.is_armed(), "frame 1: field not painted, still pending");

        let u2 = frame(&ctx, |ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut buf).id(field));
            if req.take_if(field) {
                r.request_focus();
            }
            req.tick();
            seal_focus(ui.ctx());
        });
        assert_focus_present(&u2);
        assert_eq!(u2.focus, field.accesskit_id(), "frame 2: the field has it");
        assert!(!req.is_armed(), "claimed");
    }
}
