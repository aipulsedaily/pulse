//! A terminal emulator state machine fed by bytes from the daemon.
//!
//! Unlike egui_term (which this is adapted from, MIT), there is no in-process
//! PTY: the daemon owns the process; we own the VT parser + grid + selection,
//! and everything the terminal wants to write back (DA/DSR/OSC responses)
//! is collected into an out-buffer the app ships to the daemon as Input.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::index::Direction;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{self, test::TermSize, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use egui::Modifiers;
use std::sync::mpsc;

use super::theme::TerminalTheme;
use crate::daemon::blocks::{BlockScanner, HookVerb};
use crate::win32_input::ModeScanner;

/// Scrollback depth of every GUI grid — shared between `TermBackend::new`'s
/// Term config and the block-anchor saturation check (once the ring is full,
/// history_size() stops growing while rows still evict, so anchor tracking
/// goes blind and must drop out rather than drift).
const GUI_SCROLLBACK: usize = 10_000;

/// Max superseded-prompt spacer rows tracked per backend (P3 "more lines"
/// gesture). Bounded like anchors; oldest drops first.
const SPACER_CAP: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: f32,
    pub cell_height: f32,
}

impl Default for GridSize {
    fn default() -> Self {
        Self {
            cols: 160,
            rows: 42,
            cell_width: 8.0,
            cell_height: 16.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MouseButton {
    Left = 0,
    /// xterm button code 1 (QOL §3.4): forwarded to MOUSE_MODE apps.
    Middle = 1,
    /// xterm button code 2 (QOL §3.4): forwarded to MOUSE_MODE apps.
    Right = 2,
    // LeftMove (code 32) is gone: drag MOTION is never forwarded — a drag
    // over a MOUSE_MODE app is BY DESIGN a local selection (the mouse-first
    // copy path), and the old branch could only ever fire for a press the
    // app never received.
    /// xterm wheel-up (button 4 ⇒ code 64): press-only events shipped to
    /// MOUSE_MODE apps — claude's transcript scrolling rides these.
    WheelUp = 64,
    /// xterm wheel-down (button 5 ⇒ code 65).
    WheelDown = 65,
}

/// Where a mouse-wheel gesture over the grid must go. Pure — unit-tested as
/// a decision table (the "wheel sends arrow keys into claude" field bug).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelRoute {
    /// The app claimed the mouse (DECSET 1000/1002/1003): ship xterm wheel
    /// button events (64/65) and let the app scroll itself. claude keeps
    /// ?1003h any-event tracking on and scrolls its own transcript this way
    /// — alacritty and Windows Terminal both route mouse-mode FIRST.
    Report,
    /// True full-screen alt-screen app that did NOT claim the mouse, with
    /// alternate-scroll on (DECSET 1007, default-on): wheel becomes arrow
    /// keys (htop without mouse mode, less). The alt grid has no scrollback
    /// by construction, so there is never a viewport to scroll here.
    Arrows,
    /// Scroll Pulse's own scrollback locally; the app sees nothing.
    Viewport,
}

/// Decision table for wheel routing — the exact precedence alacritty
/// (input.rs `scroll_terminal`) and Windows Terminal use:
/// mouse-report wins, then the alt-screen arrows fallback, else the local
/// viewport. Shift is the universal "scroll locally" override on both
/// forwarding branches.
pub fn wheel_route(mode: TermMode, shift: bool) -> WheelRoute {
    if mode.intersects(TermMode::MOUSE_MODE) && !shift {
        WheelRoute::Report
    } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) && !shift {
        WheelRoute::Arrows
    } else {
        WheelRoute::Viewport
    }
}

pub struct EventProxy(mpsc::Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

/// One in-grid anchor for a block, joined to its BlockRec by `start_off`
/// (journal offsets are monotonic per terminal, so no two records — even
/// across epochs — share one).
#[derive(Clone, Copy, Debug)]
pub struct BlockAnchor {
    pub start_off: u64,
    /// Grid line (Term coordinate space: 0-based screen, negative = history)
    /// of the LOGICAL START row of the prompt+command line (wrap chain
    /// already walked at capture).
    pub line: i32,
    /// Grid line of the closing prompt row (set when the matching `pre` hook
    /// lands).
    pub end_line: Option<i32>,
}

/// Quiescence window for the pending prompt-end upgrade (v0.1.1): conhost
/// forwards OSCs through the ConPTY pipe immediately but renders text on an
/// async frame (~15ms), so a 133;B can beat its own prompt text into the
/// stream — the immediate capture then lands at the row start and the
/// composer reads the prompt string as typed input (the field typed-text/^C
/// loop). The reordered text frame lands well within this window on both
/// fast and slow relay paths.
const PROMPT_END_QUIESCE: std::time::Duration = std::time::Duration::from_millis(40);

/// Per-backend block-anchoring state: feed-time capture, delta maintenance,
/// honest decay. `None` on the backend ⇒ that session pays zero cost.
pub struct BlockFeed {
    scanner: BlockScanner,
    /// Absolute journal offset of the next live Output byte (from
    /// D2C::StreamPos). None ⇒ no base yet (e.g. proto < 2 daemon): offsets
    /// can't be computed, so no anchors ever form.
    next_off: Option<u64>,
    /// Scanning active (set when a Blocks frame shows epoch > 0 — i.e. this
    /// terminal spawns hooked).
    enabled: bool,
    /// Sorted by start_off; lines are non-decreasing too (later block =
    /// lower row, and older anchors only ever move UP into history). Binary
    /// search works on either key; no sort is ever needed.
    pub anchors: Vec<BlockAnchor>,
    last_history: usize,
    was_alt: bool,
    /// Anchors can no longer be maintained exactly; in-grid chrome is
    /// suppressed (panel/actions unaffected). Set by: ring saturation,
    /// resize while alt-screen. Wrong chrome is worse than none.
    pub stale: bool,
    /// Monotonic feed-time counters for `pre` / `exec` hooks (P3 composer).
    /// The composer diffs them per frame to see events without a callback
    /// plumb; they bump even inside a deferred sync block (the latch is
    /// stream truth, not grid truth).
    pub pre_seen: u64,
    pub exec_seen: u64,
    /// Cursor cell captured at the last `PromptEnd` (OSC 133;B), in the same
    /// grid space as anchors: (line, col). Shifted with history like anchors;
    /// dropped (never remapped) on reflow/saturation/alt-resize — a wrong
    /// prompt-end is worse than a missing one (P3 §5.2).
    pub prompt_end: Option<(i32, usize)>,
    /// v0.1.1 (the ConPTY OSC-vs-text frame race, field: every 133;B on the
    /// laptop captured the WRONG cell and the composer read the prompt
    /// string as typed text): the immediate capture above is provisional —
    /// while this flag is up, the capture is UPGRADED to the live cursor
    /// cell on the first quiescent moment (no output for
    /// `PROMPT_END_QUIESCE`), because the reordered prompt-text frame lands
    /// within that window. Local input sent while pending FREEZES the
    /// immediate capture instead (waiting past the echo would fold typed
    /// text into the "prompt" — the dangerous direction). Cleared by
    /// exec/pre/alt/stale like the capture itself.
    pending_prompt_end: bool,
    /// Presentational covers riding the grid (P3 seamless): superseded bare
    /// prompt rows blanked by the empty-Enter "more lines" gesture, and
    /// composer-submitted command rows re-styled as `❯ cmd` history so the
    /// input surface never blinks through the raw `PS …>` between prompts.
    /// Grid-line space; shifted with history + pruned on the SAME
    /// drop-don't-drift rules as anchors (a dropped cover just shows the raw
    /// row — cosmetic, honest). Self-heal + selection/search suppression are
    /// applied at paint time.
    pub covers: Vec<PresCover>,
    /// True once an exec hook has fired since the last prompt (`pre`). A
    /// `pre` arriving with this still false ⇒ the previous prompt was
    /// superseded with no command run: a spacer-row candidate (empty Enter,
    /// Ctrl+C at prompt, …). The paint-time self-heal decides whether it is
    /// actually blank.
    saw_exec_since_pre: bool,
    /// The grid row a fresh prompt is ABOUT to render on: captured at the
    /// `pre` scan (the bootstrap's flush-sleep means the cursor sits at the
    /// incoming prompt row's start when the OSC lands), cleared by the
    /// 133;B that ends the render window (prompt_end takes over) and by any
    /// exec. This is the certainty source for the submit-flash blank: the
    /// structural ConPTY window where the fresh raw `PS …>` text renders
    /// BEFORE its 133;B used to flash for a frame at the bottom of the
    /// screen on every submit. Shifted with history like covers; dropped —
    /// never remapped — on reflow/alt/shrink-below-screen (drop-don't-drift:
    /// a wrong blank is worse than the flash).
    incoming_prompt: Option<i32>,
    /// Cursor grid line at the end of the previous feed — the UP-MOVE
    /// detector for the stale-cover prune (F2). None while alt-screen is
    /// active (the visible cursor belongs to the alt grid) so alt exit never
    /// fakes an upward jump.
    last_cursor_line: Option<i32>,
    /// History size right after the attach Replay was parsed (captured at
    /// StreamPos — the frame the daemon sends immediately after Replay,
    /// before any live Output). ReplayAnchors rows are in replay coordinate
    /// space; live output landing before the hints frame shifts the grid, so
    /// hint rows are re-based by `history_now - replay_base_history`.
    replay_base_history: usize,
    /// Grid size at StreamPos: a resize between the Replay and the hints
    /// frame reflows rows unmappably — hints are dropped whole (drop, never
    /// drift).
    replay_size: (u16, u16),
    /// Freshest FEED-TIME cwd from the last `pre` hook whose payload carried
    /// one (bash/pwsh `d` field; cmd's static PROMPT sends an empty payload
    /// and stays None). The composer lane label prefers this over the
    /// Snapshot meta's live_cwd — zero round-trip: the label updates the
    /// same frame the fresh prompt renders (the stale-lane bug where
    /// `cd`'s new cwd waited for an unrelated Snapshot broadcast).
    live_cwd: Option<String>,
}

/// One presentational cover row (P3). `cmd == None` ⇒ blank spacer;
/// `cmd == Some` ⇒ history cover painting `❯ cwd cmd` over the raw row.
#[derive(Clone, Debug)]
pub struct PresCover {
    /// Grid line (0-based screen, negative = history), shifted with history.
    pub line: i32,
    /// Grid column where the shell's input area began (prompt end): the
    /// self-heal check reads the raw row from here.
    pub col: usize,
    /// Dimmed cwd for history covers (matches the armed/hold prefix exactly
    /// so the hold→history swap is pixel-zero). Unused for spacers.
    pub cwd: Option<String>,
    /// The submitted command for history covers; `None` for blank spacers.
    pub cmd: Option<String>,
    /// SPACERS ONLY: the prompt text left of `col` at capture time
    /// (trim-end). The self-heal blanks the row only while it still shows
    /// EXACTLY this bare prompt — a row erased in place (cls, conhost resize
    /// repaint) and rewritten by short output would otherwise be blanked by
    /// a stale spacer (background painted over legitimate text: the
    /// empty-rectangle artifact class). History covers verify via `cmd`.
    pub sig: Option<String>,
}

impl BlockFeed {
    fn new(history: usize) -> Self {
        Self {
            scanner: BlockScanner::new(),
            next_off: None,
            enabled: false,
            anchors: Vec::new(),
            last_history: history,
            was_alt: false,
            stale: false,
            pre_seen: 0,
            exec_seen: 0,
            prompt_end: None,
            pending_prompt_end: false,
            covers: Vec::new(),
            saw_exec_since_pre: false,
            incoming_prompt: None,
            last_cursor_line: None,
            replay_base_history: history,
            replay_size: (0, 0),
            live_cwd: None,
        }
    }
}

/// One restored-history hint after the App joined it to its BlockRec (see
/// D2C::ReplayAnchors). `cmd == None` ⇒ spacer.
#[derive(Debug, Clone)]
pub struct ReplayHint {
    pub start_off: u64,
    /// Replay-space grid row (re-based against history growth on apply).
    pub row: i32,
    /// Grid column where the input area begins (prompt end).
    pub col: usize,
    /// The joined record's command (blocks) or None (spacer).
    pub cmd: Option<String>,
    /// The joined record's cwd, display-formatted (blocks only).
    pub cwd: Option<String>,
}

pub struct TermBackend {
    pub term: Term<EventProxy>,
    parser: Processor,
    events: mpsc::Receiver<Event>,
    pub size: GridSize,
    pub theme: TerminalTheme,
    pub title: Option<String>,
    /// Set true when the VT stream rings the bell; the app drains it once per
    /// frame to latch a NeedsYou activity state (V-A).
    pub bell: bool,
    /// True while the session's conhost has win32-input-mode (DECSET 9001)
    /// requested: keys are then encoded as full Win32 key events instead of
    /// VT bytes (see `crate::win32_input`). vte ignores private mode 9001,
    /// so a raw scan over the same byte stream tracks it. The daemon
    /// re-asserts the mode at the end of every attach replay.
    pub win32_input: bool,
    mode_scan: ModeScanner,
    /// Journal-block anchoring (P2). None ⇒ this session pays zero cost —
    /// created lazily by the first D2C::StreamPos.
    pub block_feed: Option<BlockFeed>,
    /// A block-jump just happened: (start_off, when). The view draws a short
    /// flash over the block's first rows and clears it.
    pub jump_flash: Option<(u64, std::time::Instant)>,
    /// The Term's configured scrollback depth (10k normally; small in tests
    /// so saturation is exercisable).
    history_cap: usize,
    /// Instant of the last LIVE output frame (advance_live) — the quiescence
    /// clock for the pending prompt-end upgrade (v0.1.1). None until the
    /// first live frame.
    last_output_at: Option<std::time::Instant>,
    /// Monotonic generation counter, bumped whenever the parser consumed
    /// bytes (advance / advance_live) or a stuck sync block was force-flushed
    /// (pump_sync). Cheap change-detection for per-frame grid scans: the
    /// activity prompt-signature rescan is gated on this so 20 idle terminals
    /// cost zero grid walks per typing frame (UX HIGH-3).
    pub feed_gen: u64,
    /// The grid line term_view actually BLANKED with the current-prompt
    /// cover last frame (the static-input architecture: the shell's latched
    /// prompt row renders as whitespace while the composer's strip editor is
    /// the one prompt). Set by the app after term_view returns — copy
    /// synthesis (`selection_text`) must match what was PAINTED, one frame
    /// stale at worst (the Copy event is processed before this frame's
    /// render).
    pub cur_blank_line: Option<i32>,
}

/// The single Term::Config constructor — `Term::set_options` replaces the
/// WHOLE config, so every custom field must live here or a runtime history
/// change (shrink_history_for_idle) would silently reset it.
fn term_config(history: usize) -> term::Config {
    term::Config {
        scrolling_history: history,
        // QOL §6.4: the alacritty default word boundaries MINUS `:` —
        // double-click selects `C:\Users\…\shot.png` and `https://…`
        // whole (both split at the colon under the default). Cost:
        // `key: value` word-select grabs the trailing colon — trivially
        // edited; the path/URL win is the daily gesture. `│`/quotes/
        // brackets stay (box-drawing walls and real word delimiters).
        semantic_escape_chars: ",\u{2502}`|\"' ()[]{}<>\t".into(),
        ..term::Config::default()
    }
}

impl TermBackend {
    pub fn new(size: GridSize) -> Self {
        Self::new_with_history(size, GUI_SCROLLBACK)
    }

    /// r2-M2: the scrollback depth is a stored pref (gui.json
    /// `scrollback_lines`, default = GUI_SCROLLBACK) — the GUI's dominant
    /// per-terminal memory cost (~3.9KB/row at 158 cols when saturated).
    /// Applied at construction only: runtime growth is not supported (the
    /// shrink path exists solely for idle grids, see
    /// `shrink_history_for_idle`).
    pub fn with_scrollback(size: GridSize, lines: usize) -> Self {
        Self::new_with_history(size, lines.clamp(200, 100_000))
    }

    fn new_with_history(size: GridSize, history: usize) -> Self {
        let (tx, rx) = mpsc::channel();
        let config = term_config(history);
        let term = Term::new(
            config,
            &TermSize::new(size.cols as usize, size.rows as usize),
            EventProxy(tx),
        );
        Self {
            term,
            parser: Processor::new(),
            events: rx,
            size,
            theme: TerminalTheme::default(),
            title: None,
            bell: false,
            win32_input: false,
            mode_scan: ModeScanner::new(),
            block_feed: None,
            jump_flash: None,
            history_cap: history,
            last_output_at: None,
            feed_gen: 0,
            cur_blank_line: None,
        }
    }

    /// r2-M1: an asleep/dead terminal keeps a full 10k-line grid that wake
    /// discards anyway — the Reset arm REPLACES the backend wholesale,
    /// rebuilt from a ≤2MB replay (≈ ≤4k lines). Shrink to the replay
    /// ceiling and truly free the rows (alacritty `set_options` →
    /// `update_history` → `shrink_lines`; measured up to ~35MB per
    /// saturated 158-col terminal). A history shrink moves every
    /// history-relative coordinate, so the drop-don't-drift doctrine
    /// applies — the same clear set as ring saturation: anchors, covers,
    /// prompt_end, incoming_prompt dropped, chrome degraded via `stale`
    /// (the next Reset/Replay rebuilds all of it from hints). Scrollback
    /// beyond the kept tail is lost — exactly what wake already does.
    /// Returns true when it actually shrank (idempotent; the caller then
    /// invalidates search).
    pub fn shrink_history_for_idle(&mut self) -> bool {
        const IDLE_HISTORY: usize = 2_000;
        if self.history_cap <= IDLE_HISTORY {
            return false;
        }
        self.history_cap = IDLE_HISTORY;
        // Selection coordinates die with the rows under them.
        self.term.selection = None;
        self.term.set_options(term_config(IDLE_HISTORY));
        if let Some(bf) = self.block_feed.as_mut() {
            bf.stale = true;
            bf.anchors.clear();
            bf.prompt_end = None;
            bf.incoming_prompt = None;
            bf.covers.clear();
        }
        let h = self.history_size();
        if let Some(bf) = self.block_feed.as_mut() {
            bf.last_history = h;
        }
        // Re-key preview/paint caches (they key on feed_gen).
        self.feed_gen = self.feed_gen.wrapping_add(1);
        true
    }

    /// Feed daemon output through the VT parser.
    ///
    /// Query responses (cursor reports, color queries, …) are DISCARDED here:
    /// the daemon's headless parser is the single authoritative responder.
    /// Forwarding ours too would deliver duplicate reports that leak into the
    /// shell as garbage keystrokes.
    ///
    /// DECSET 2026 (synchronized output) is honored by vte itself: bytes
    /// between BSU (`ESC[?2026h`) and ESU (`ESC[?2026l`) are held in the
    /// parser and applied to the grid in one go at ESU — TUI frame updates
    /// present atomically instead of flickering half-drawn. vte caps the
    /// deferral at 2MiB but its 150ms cap is embedder-enforced: the app must
    /// call `pump_sync` every frame or a stuck BSU freezes the grid.
    ///
    /// This entry is for REPLAY/reconstruction bytes (and tests): parse only.
    /// Never block-scanned, never offset-counted — a raw-tail replay
    /// (alt-screen/dead fallback) CONTAINS hook OSCs, and counting a
    /// reconstruction would corrupt stream offsets. Live Output frames go
    /// through `advance_live`.
    pub fn advance(&mut self, bytes: &[u8]) {
        // A Replay REBUILDS this grid's world: any selection made against the
        // previous content is a set of coordinates into text that no longer
        // exists — left alive it paints permanent full-width tint slabs over
        // whatever the reconstruction puts on those rows (the restored-session
        // "navy rectangles" class). Every Replay today lands in a fresh
        // backend, so this is structural insurance, not a hot path.
        self.term.selection = None;
        self.feed_gen = self.feed_gen.wrapping_add(1);
        if let Some(on) = self.mode_scan.feed(bytes) {
            self.win32_input = on;
        }
        self.parser.advance(&mut self.term, bytes);
        self.drain_events();
    }

    /// Live Output frames: parse + block-scan + anchor (P2). Identical parse
    /// result to `advance` — vte is incremental, so split-feeding at hook
    /// boundaries is byte-identical; the scanner READS only (mirror purity).
    pub fn advance_live(&mut self, bytes: &[u8]) {
        // Pending prompt-end upgrade (v0.1.1): if the stream was quiet for
        // the whole quiescence window, the PRE-parse cursor is the settled
        // one — resolve before these new bytes move it (output after a
        // settled prompt is either the next command's world or type-ahead
        // echo; neither belongs in the prompt-end cell).
        let now = std::time::Instant::now();
        if self.block_feed.as_ref().is_some_and(|f| f.pending_prompt_end)
            && self
                .last_output_at
                .is_some_and(|t| now.duration_since(t) >= PROMPT_END_QUIESCE)
        {
            self.resolve_pending_prompt_end();
        }
        self.last_output_at = Some(now);
        self.feed_gen = self.feed_gen.wrapping_add(1);
        if let Some(on) = self.mode_scan.feed(bytes) {
            self.win32_input = on;
        }
        match &self.block_feed {
            None => self.parser.advance(&mut self.term, bytes),
            Some(_) => self.advance_scanned(bytes),
        }
        self.drain_events();
        self.clear_live_selection_on_output();
    }

    /// Commit the pending prompt-end upgrade: the live cursor cell IS the
    /// settled prompt end (v0.1.1 — see `BlockFeed::pending_prompt_end`).
    /// No-op (flag cleared) when the feed went stale or an alt screen owns
    /// the cursor.
    fn resolve_pending_prompt_end(&mut self) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        let cur = self.term.grid().cursor.point;
        if let Some(bf) = self.block_feed.as_mut() {
            if !bf.pending_prompt_end {
                return;
            }
            bf.pending_prompt_end = false;
            if bf.stale || alt {
                return;
            }
            bf.prompt_end = Some((cur.line.0, cur.column.0));
            bf.incoming_prompt = None;
        }
    }

    /// Per-frame poll for the pending prompt-end upgrade (v0.1.1): resolves
    /// once the output has been quiet for `PROMPT_END_QUIESCE`, returns the
    /// wakeup deadline while still pending (the caller schedules a repaint —
    /// an idle terminal produces no output frames to resolve on).
    pub fn poll_pending_prompt_end(&mut self, now: std::time::Instant) -> Option<std::time::Instant> {
        if !self
            .block_feed
            .as_ref()
            .is_some_and(|f| f.pending_prompt_end)
        {
            return None;
        }
        let deadline = self.last_output_at.unwrap_or(now) + PROMPT_END_QUIESCE;
        if now >= deadline {
            self.resolve_pending_prompt_end();
            None
        } else {
            Some(deadline)
        }
    }

    /// Local input is about to ship to this terminal's PTY (v0.1.1): a
    /// pending prompt-end upgrade FREEZES on the immediate capture — the
    /// keystroke's echo arrives as output and would otherwise restart the
    /// quiescence clock until the "settled" cursor sat AFTER the echoed
    /// text, folding typed input into the prompt (the fused-submit hazard).
    /// The immediate capture is exactly today's semantics: correct on
    /// non-racing machines, honestly dirty on racing ones.
    pub fn note_input(&mut self) {
        if let Some(bf) = self.block_feed.as_mut() {
            bf.pending_prompt_end = false;
        }
    }

    /// Selection lifecycle (repro bug 2c, "immortal staircase"): a selection
    /// used to persist until the next click — surviving submits, scrolls and
    /// covers as a floating tint band over rows whose text had long rotated
    /// away. Policy: new output while the viewport is AT THE BOTTOM clears a
    /// selection that touches the live region (line ≥ 0) — predictable, and
    /// exactly when its visual context is being rewritten. A scrolled-up
    /// selection (reading/copying history) is never touched.
    fn clear_live_selection_on_output(&mut self) {
        // Alt-screen TUIs (claude, any full-screen app) repaint IN PLACE —
        // spinner/stream redraws arrive as output frames several times a
        // second, and this policy used to destroy a selection within one
        // frame (even MID-DRAG: update_selection no-ops once term.selection
        // is None), which made copying text out of claude impossible: by the
        // time Ctrl+C or the menu arrived there was no selection, so Ctrl+C
        // INTERRUPTED the app instead. The staircase hazard this policy
        // exists for is scrollback MOTION under the selection — the alt grid
        // has no scrollback and rows never rotate away, so alt selections
        // survive output (WT parity: the tint stays put while the app
        // repaints under it); a press still clears them.
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        if self.term.grid().display_offset() != 0 {
            return;
        }
        let touches_live = self
            .term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term))
            .is_some_and(|r| r.end.line.0 >= 0);
        if touches_live {
            self.term.selection = None;
        }
    }

    /// The block-feed path: count stream offsets, and when scanning is
    /// enabled split the parse at each hook boundary so the grid state at
    /// capture time is exactly the state after parsing up to that hook — the
    /// cursor row IS the block boundary row.
    fn advance_scanned(&mut self, bytes: &[u8]) {
        let (base, enabled) = {
            let bf = self.block_feed.as_mut().unwrap();
            let base = bf.next_off;
            if let Some(o) = bf.next_off.as_mut() {
                *o += bytes.len() as u64;
            }
            (base, bf.enabled)
        };
        if !enabled {
            self.parser.advance(&mut self.term, bytes);
            return;
        }
        let events = self.block_feed.as_mut().unwrap().scanner.feed(bytes);
        let mut done = 0usize;
        for ev in events {
            let end = ev.offset_in_chunk; // byte AFTER the OSC terminator
            self.parser.advance(&mut self.term, &bytes[done..end]);
            done = end;
            self.track_scroll(); // shift anchors before reading the cursor
            // Feed-time event counters (P3): bumped BEFORE the sync-pending
            // skip — the events are real regardless of grid deferral (the
            // composer's prompt latch is stream truth, not grid truth).
            {
                let bf = self.block_feed.as_mut().unwrap();
                match &ev.verb {
                    HookVerb::Pre { cwd, .. } => {
                        bf.pre_seen += 1;
                        // Feed-time cwd for the lane label (stream truth —
                        // like the counters, valid even under sync deferral).
                        if !cwd.is_empty() {
                            bf.live_cwd = Some(cwd.clone());
                        }
                    }
                    HookVerb::Exec { .. } => bf.exec_seen += 1,
                    _ => {}
                }
            }
            // Skip capture while a DECSET-2026 sync block is pending: the
            // grid hasn't applied these bytes yet, so the cursor would lie.
            // (Hooks inside sync blocks don't happen at real prompts; safe
            // to leave such a block unanchored.)
            if self.parser.sync_timeout().sync_timeout().is_some() {
                continue;
            }
            match ev.verb {
                HookVerb::Exec { .. } => {
                    if let Some(b) = base {
                        self.capture_exec(b + end as u64);
                    }
                }
                HookVerb::Pre { .. } => self.capture_pre(),
                HookVerb::PromptEnd => self.capture_prompt_end(),
                HookVerb::Init { .. } | HookVerb::Beacon { .. } => {}
            }
        }
        self.parser.advance(&mut self.term, &bytes[done..]);
        self.track_scroll();
        self.prune_stale_covers();
    }

    /// Feed-time stale-cover prune (F2): covers strictly BELOW the cursor
    /// are always wrong (covers are minted at-or-above it — content below
    /// the cursor means the screen was rewritten under them), and an UPWARD
    /// cursor jump (cls/ED2, CUP redraws, the conhost resize repaint)
    /// invalidates covers at/below the new cursor row. The sig/cmd self-heal
    /// alone re-matched coincident rewrites — a FRESH bare prompt landing on
    /// a dead spacer's row passed the heal and the spacer blanked the LIVE
    /// prompt (the vanishing-prompt / gap-rows class). Normal downward
    /// output never prunes; scrollback rows are immutable so history covers
    /// are safe by construction. This also enforces the bottom-pin invariant
    /// INV-PIN's precondition: no cover can survive below the cursor, so the
    /// continuity fill's blank_tail can never stall on a covered row (F1).
    fn prune_stale_covers(&mut self) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        let cur = self.term.grid().cursor.point.line.0;
        let Some(bf) = self.block_feed.as_mut() else { return };
        if alt {
            // The visible cursor belongs to the alt grid; the primary grid
            // (and its covers) are frozen. Forget the tracking point so alt
            // exit can never read as an upward jump.
            bf.last_cursor_line = None;
            return;
        }
        if !bf.enabled || bf.stale {
            bf.last_cursor_line = Some(cur);
            return;
        }
        let up_move = bf.last_cursor_line.is_some_and(|p| cur < p);
        bf.covers
            .retain(|c| c.line < cur || (!up_move && c.line == cur));
        bf.last_cursor_line = Some(cur);
    }

    /// Maintain anchors against grid movement: lines only move when rows
    /// enter history (observable as history_size growth); when exactness is
    /// unattainable — ring saturated, alt resize — anchors are dropped and
    /// chrome silently disappears rather than drift.
    fn track_scroll(&mut self) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        let h = self.term.grid().history_size();
        let cap = self.history_cap;
        let Some(bf) = self.block_feed.as_mut() else { return };
        if !bf.enabled || bf.stale {
            return;
        }
        if alt {
            // The active grid is the alt grid (history_size()==0) — reading
            // it would look like a huge shrink. The primary grid is frozen
            // while alt is active, so anchors need no updates. A pending
            // prompt-render window is definitionally over (a full-screen app
            // owns the screen now) — drop, never blank an alt row.
            bf.incoming_prompt = None;
            bf.was_alt = true;
            return;
        }
        if bf.was_alt {
            // Leaving alt: resync, no shift — primary history can't have
            // changed while alt was active.
            bf.was_alt = false;
            bf.last_history = h;
            return;
        }
        if h >= cap {
            // Saturated ring: further scrolling is unobservable (history
            // delta pins at 0 while rows still evict) — anchors would drift.
            // Honest degraded mode for the rest of this attach.
            bf.stale = true;
            bf.anchors.clear();
            bf.prompt_end = None;
            bf.incoming_prompt = None;
            bf.covers.clear();
            return;
        }
        if h > bf.last_history {
            let d = (h - bf.last_history) as i32;
            for a in bf.anchors.iter_mut() {
                a.line -= d;
                if let Some(e) = a.end_line.as_mut() {
                    *e -= d;
                }
            }
            // Fell off the ring.
            bf.anchors.retain(|a| a.line >= -(h as i32));
            if let Some(pe) = bf.prompt_end.as_mut() {
                pe.0 -= d;
                // The CURRENT prompt's end can never live in scrollback: if
                // its row scrolled off-screen, that prompt is over (hookless
                // output — a conhost resize repaint, raw Enters — pushed it
                // up without an exec to retire it). A negative prompt_end is
                // definitionally stale: it made `cursor_at_prompt_end()`
                // false forever on idle restored sessions AND pointed the
                // reclaim/activation-preview at scrollback text (the field
                // "Typed text at the prompt" label over a clean prompt).
                // Drop-don't-drift (restored-render fix).
                if pe.0 < 0 {
                    bf.prompt_end = None;
                }
            }
            if let Some(ip) = bf.incoming_prompt.as_mut() {
                // Rides the scroll like prompt_end; an incoming prompt can
                // never be in scrollback (the paint gate's cursor-on-row
                // check would fail anyway) — drop, don't drift.
                *ip -= d;
                if *ip < 0 {
                    bf.incoming_prompt = None;
                }
            }
            for c in bf.covers.iter_mut() {
                c.line -= d;
            }
            bf.covers.retain(|c| c.line >= -(h as i32));
            bf.last_history = h;
        } else if h < bf.last_history {
            // History shrank (ED3/RIS/clear-scrollback): scrollback rows are
            // gone — prune their anchors; screen rows and theirs are intact.
            bf.anchors.retain(|a| a.line >= 0);
            if bf.prompt_end.is_some_and(|(l, _)| l < 0) {
                bf.prompt_end = None;
            }
            if bf.incoming_prompt.is_some_and(|l| l < 0) {
                bf.incoming_prompt = None;
            }
            bf.covers.retain(|c| c.line >= 0);
            bf.last_history = h;
        }
    }

    /// An exec hook landed: the cursor sits at col 0 of the first OUTPUT row
    /// (PSReadLine echoes the accept-newline before ReadLine returns), so the
    /// command's last row is one above; a hook landing mid-line means that
    /// line IS the command row. Normalize to the LOGICAL start of the
    /// (possibly wrapped) prompt+command line.
    fn capture_exec(&mut self, start_off: u64) {
        if self.block_feed.as_ref().is_none_or(|f| f.stale) {
            return;
        }
        let cur = self.term.grid().cursor.point;
        let hist = self.term.grid().history_size() as i32;
        let cmd_last = if cur.column.0 == 0 {
            cur.line.0 - 1
        } else {
            cur.line.0
        }
        .max(-hist);
        let line = walk_to_logical_start(&self.term, cmd_last, 64);
        let bf = self.block_feed.as_mut().unwrap();
        bf.saw_exec_since_pre = true;
        // A command is now running: the captured prompt end is DEAD until the
        // next prompt's 133;B re-captures it. Leaving the stale cell live let
        // `cursor_at_prompt_end()` spuriously match mid-render (the cursor
        // sweeps the whole grid while output draws) and the composer armed its
        // cover over an arbitrary row — the "cover at a wrong/transient row"
        // submit artifact. Drop-don't-drift.
        bf.prompt_end = None;
        bf.pending_prompt_end = false;
        bf.incoming_prompt = None;
        // Defensive ordering: the daemon dedupes and offsets are monotonic —
        // a duplicate/late offset can only come from a replayed spoof.
        bf.anchors.retain(|a| a.start_off < start_off);
        bf.anchors.push(BlockAnchor {
            start_off,
            line,
            end_line: None,
        });
    }

    /// A PromptEnd (OSC 133;B) landed: the bootstrap emits it after the
    /// prompt text has drained (frame-flush sleep), so the cursor cell at
    /// this split-feed point IS where the prompt string ends and PSReadLine's
    /// input area begins (P3 §5.2).
    fn capture_prompt_end(&mut self) {
        let cur = self.term.grid().cursor.point;
        // v0.1.1 (H1 fix): the capture is provisional ONLY when it carries the
        // ConPTY-reorder SIGNATURE — the 133;B arrived before its prompt text,
        // so the cursor sits at the row start with a blank prefix to its left.
        // A capture with real prompt text already rendered to its left is the
        // correct, final cell and must NEVER be upgraded: leaving the pending
        // flag on a correct capture let async output (a backgrounded job's log
        // line) move the cursor and the 40ms-quiet upgrade then re-pointed
        // prompt_end at the post-output cell — converting honest-dirty into
        // wrongly-clean (armed cover over async output). The upgrade now heals
        // only the race it was built for.
        let suspicious = self
            .row_prefix_text(cur.line.0, cur.column.0)
            .trim()
            .is_empty();
        if let Some(bf) = self.block_feed.as_mut() {
            if !bf.stale {
                bf.prompt_end = Some((cur.line.0, cur.column.0));
                // The render window is over: the armed cover (certainty:
                // cursor exactly at this captured cell) takes over from the
                // incoming-prompt blank on the same row, same frame.
                bf.incoming_prompt = None;
                // Pend the quiescence upgrade only on the race signature (see
                // above); a trusted capture is left final. On non-racing
                // machines this is a no-op — the capture was already correct.
                bf.pending_prompt_end = suspicious;
            }
        }
    }

    /// A pre hook fires before any prompt text renders: the cursor row is the
    /// closing prompt row of the still-open block. Also the spacer-row
    /// detector: if no exec ran since the last prompt, that prompt was
    /// superseded with no command — a blank-spacer candidate (the empty-Enter
    /// "more lines" gesture, or Ctrl+C at a prompt). The previous prompt's row
    /// is `prompt_end` (still the OLD value here — 133;B overwrites it only
    /// after the new prompt text renders, which is after this hook). Whether
    /// it is actually blank is decided by the paint-time self-heal, so a
    /// cancelled line that still holds typed text renders raw.
    fn capture_pre(&mut self) {
        let prompt_line = self.term.grid().cursor.point.line.0;
        let cur_col = self.term.grid().cursor.point.column.0;
        let spacer = match self.block_feed.as_ref() {
            Some(bf) if !bf.stale && !bf.saw_exec_since_pre => {
                bf.prompt_end.map(|(line, col)| {
                    (line, col, self.row_prefix_text(line, col))
                })
            }
            _ => None,
        };
        if let Some(bf) = self.block_feed.as_mut() {
            if bf.stale {
                return;
            }
            if let Some((line, col, sig)) = spacer {
                push_cover(
                    &mut bf.covers,
                    PresCover { line, col, cwd: None, cmd: None, sig: Some(sig) },
                );
            }
            bf.saw_exec_since_pre = false;
            // A NEW prompt is about to render: the previous prompt's captured
            // end cell is obsolete the moment the pre hook fires (spacer use
            // above is its last legitimate read). ConPTY delivers this OSC
            // ahead of the asynchronously-rendered text, so for ~15ms the
            // cursor often still SITS at the old prompt end — a live stale
            // capture made `cursor_at_prompt_end()` spuriously true there and
            // the composer auto-armed its cover on the OLD row, then dropped
            // to the strip lane, then re-armed on the new row when 133;B
            // landed: the user-reported "input box drops DOWN then flies up"
            // submit flicker. The next 133;B re-captures ~15ms later.
            bf.prompt_end = None;
            // v0.1.1: a pending upgrade for the SUPERSEDED prompt must never
            // resolve against the fresh prompt's cursor.
            bf.pending_prompt_end = false;
            // …and the row the fresh prompt will render on IS the cursor row
            // at this scan (the bootstrap's pre-hook flush-sleep drained the
            // previous command's text frames first). The composer blanks it
            // through the render window so the raw prompt never flashes; the
            // paint gate re-checks the cursor is still ON this row every
            // frame (a late output tail — the reorder residual — moves it
            // off and the blank drops, honest raw). COLUMN 0 is required at
            // capture: a fresh prompt renders from the row start — a pre
            // scanned with the cursor parked mid-row (the echo/newline of
            // the previous command hasn't rendered yet, the ConPTY reorder)
            // is NOT sitting on the incoming prompt row, and blanking there
            // would blank the OLD prompt/echo row. Drop-don't-drift.
            bf.incoming_prompt = (cur_col == 0).then_some(prompt_line);
            if let Some(a) = bf.anchors.last_mut() {
                if a.end_line.is_none() {
                    a.end_line = Some(prompt_line);
                }
            }
        }
    }

    /// A live (non-stale) prompt-end capture exists. The strip's dirty-prompt
    /// label keys off this: with NO capture there is nothing "typed at the
    /// prompt" to speak about — during the pre→133;B render window (prompt_end
    /// deliberately invalidated) the strip must show the neutral label, not
    /// flash "Typed text at the prompt" on every submit transition.
    pub fn has_prompt_end(&self) -> bool {
        self.block_feed
            .as_ref()
            .is_some_and(|f| !f.stale && f.prompt_end.is_some())
    }

    /// True when the live grid cursor sits exactly at the captured prompt
    /// end — i.e. PSReadLine's input buffer is visibly empty (P3 §5.2). Two
    /// integer compares; hookless sessions (`block_feed` None) return false.
    pub fn cursor_at_prompt_end(&self) -> bool {
        let Some(bf) = &self.block_feed else { return false };
        let Some((line, col)) = bf.prompt_end else { return false };
        if bf.stale {
            return false;
        }
        let cur = self.term.grid().cursor.point;
        cur.line.0 == line && cur.column.0 == col
    }

    /// The cursor row's text LEFT of the cursor, trim-end'd (v0.1.1): the
    /// pre-shell auth-prompt detector's input — `user@host's password:` /
    /// `(yes/no/[fingerprint])?` sit exactly there while ssh is asking.
    /// Bounded O(cols); render-only consumer (a miss is just a generic
    /// label).
    pub fn cursor_row_text(&self) -> String {
        let cur = self.term.grid().cursor.point;
        self.row_prefix_text(cur.line.0, cur.column.0)
    }

    /// D2 heuristic prompt detection: the cursor row's prefix (same contract
    /// as `cursor_row_text`) plus the COLUMN GAP — how many cells the cursor
    /// sits past the trimmed text (a prompt's trailing space ⇒ 1; 0 for
    /// no-space prompts; large ⇒ the cursor was parked away from the text by
    /// a full-screen paint / column alignment, not a prompt tail). Sibling of
    /// `row_prefix_text`: NULs read as spaces, wide-char spacers skipped, so
    /// the gap counts rendered cells of trailing whitespace.
    pub fn cursor_prefix_gap(&self) -> (String, usize) {
        let cur = self.term.grid().cursor.point;
        let grid = self.term.grid();
        let cols = self.size.cols as usize;
        let row = &grid[Line(cur.line.0)];
        let mut s = String::with_capacity(cur.column.0.min(cols));
        for c in 0..cur.column.0.min(cols) {
            let cell = &row[Column(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            s.push(if cell.c == '\0' { ' ' } else { cell.c });
        }
        let total = s.chars().count();
        let t = s.trim_end();
        let gap = total - t.chars().count();
        (t.to_string(), gap)
    }

    /// The cursor's grid column (cell space). D2: the heuristic prompt
    /// latch's anchor — with no 133;B in a markerless nested shell, the
    /// cursor cell itself IS the prompt-end surrogate.
    pub fn cursor_col(&self) -> usize {
        self.term.grid().cursor.point.column.0
    }

    /// Instant of the last LIVE output frame (advance_live), if any — the
    /// D2 heuristic latch's output-quiet clock (300ms of silence before a
    /// prompt-shaped cursor row may arm). Replay/reconstruction feeds
    /// (`advance`) deliberately don't stamp it: quiet is a LIVE property.
    pub fn last_output_at(&self) -> Option<std::time::Instant> {
        self.last_output_at
    }

    /// Freshest feed-time cwd (the last `pre` hook payload that carried
    /// one). The lane label prefers this over Snapshot meta — it updates the
    /// frame the fresh prompt renders instead of waiting for a broadcast.
    pub fn feed_cwd(&self) -> Option<&str> {
        self.block_feed
            .as_ref()
            .and_then(|f| f.live_cwd.as_deref())
    }

    /// A live hook feed exists (scanning enabled, not stale). The composer's
    /// prompt-render-window queue guard keys off this: only a hook-fed shell
    /// has a pre→133;B window to wait out.
    pub fn feed_live(&self) -> bool {
        self.block_feed
            .as_ref()
            .is_some_and(|f| f.enabled && !f.stale)
    }

    /// The grid row a fresh prompt is provably rendering on RIGHT NOW —
    /// i.e. the pre→133;B render window is open and the cursor still sits on
    /// the row captured at the `pre` scan. The composer blanks this row
    /// (submit-flash fix): the structural ConPTY window where the raw fresh
    /// `PS …>` paints before its 133;B used to flash for one frame on every
    /// submit. Certainty gates, ALL required (drop-don't-drift):
    ///   - feed live (enabled, not stale), primary screen;
    ///   - a `pre` was scanned with no exec since (a prompt IS coming);
    ///   - 133;B has not landed yet (prompt_end would own the row);
    ///   - the live cursor is ON the captured row (a late output tail — the
    ///     ConPTY reorder residual — moves it off; raw is honest there).
    pub fn incoming_prompt_row(&self) -> Option<i32> {
        let bf = self.block_feed.as_ref()?;
        if !bf.enabled || bf.stale || bf.saw_exec_since_pre || bf.prompt_end.is_some() {
            return None;
        }
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let row = bf.incoming_prompt?;
        (self.term.grid().cursor.point.line.0 == row).then_some(row)
    }

    /// Mark the CURRENT prompt row as a blank spacer (the composer's own
    /// empty-Enter gesture): the armed prompt at submit time becomes "more
    /// lines" whitespace the same frame the cover drops, so no raw `PS …>`
    /// flashes on it during the re-prompt round-trip. Uses `prompt_end`
    /// (identical source to the feed-time capture); the paint-time self-heal
    /// still decides whether it renders blank. Idempotent via `push_cover`.
    pub fn mark_prompt_spacer(&mut self) {
        let target = match self.block_feed.as_ref() {
            Some(bf) if !bf.stale => bf
                .prompt_end
                .map(|(line, col)| (line, col, self.row_prefix_text(line, col))),
            _ => None,
        };
        if let (Some(bf), Some((line, col, sig))) = (self.block_feed.as_mut(), target) {
            push_cover(
                &mut bf.covers,
                PresCover { line, col, cwd: None, cmd: None, sig: Some(sig) },
            );
        }
    }

    /// The row's text LEFT of `col` (the prompt region), trim-end, NULs as
    /// spaces, wide-char spacers skipped — the spacer signature source, read
    /// identically at capture and at the paint-time self-heal.
    fn row_prefix_text(&self, line: i32, col: usize) -> String {
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let rows = self.size.rows as i32;
        if line < -history || line >= rows {
            return String::new();
        }
        let cols = self.size.cols as usize;
        let row = &grid[Line(line)];
        let mut s = String::with_capacity(col.min(cols));
        for c in 0..col.min(cols) {
            let cell = &row[Column(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            s.push(if cell.c == '\0' { ' ' } else { cell.c });
        }
        s.trim_end().to_string()
    }

    /// LOW-6: `row_prefix_text(line, col) == sig`, without building the
    /// String — the per-frame spacer heal runs this for every on-screen
    /// spacer cover. Provably equivalent: `str::trim_end` strips exactly
    /// `char::is_whitespace()`, so a produced char past the end of `sig`
    /// matches iff it is whitespace (the trailing run the String form
    /// trimmed away), and `sig` itself is already trim-end'd at capture.
    fn row_prefix_matches(&self, line: i32, col: usize, sig: &str) -> bool {
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let rows = self.size.rows as i32;
        if line < -history || line >= rows {
            return sig.is_empty(); // out-of-range prefix is ""
        }
        let cols = self.size.cols as usize;
        let row = &grid[Line(line)];
        let mut sig_it = sig.chars();
        for c in 0..col.min(cols) {
            let cell = &row[Column(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let ch = if cell.c == '\0' { ' ' } else { cell.c };
            match sig_it.next() {
                Some(w) if w == ch => {}
                Some(_) => return false,
                None => {
                    if !ch.is_whitespace() {
                        return false; // extra non-trailing content
                    }
                }
            }
        }
        sig_it.next().is_none() // sig must be fully consumed
    }

    /// Convert a just-submitted (and grid-verified) command row into a
    /// permanent history cover: from now on the row paints `❯ cwd cmd` in the
    /// armed style instead of ever showing the raw `PS …> cmd`, so the input
    /// surface never blinks through the default prompt between commands (P3).
    /// The caller (composer release path) has already confirmed the echo
    /// landed on this row via `row_has_text_at`.
    pub fn add_history_cover(&mut self, line: i32, col: usize, cwd: Option<String>, cmd: String) {
        if let Some(bf) = self.block_feed.as_mut() {
            if bf.stale {
                return;
            }
            push_cover(
                &mut bf.covers,
                PresCover { line, col, cwd, cmd: Some(cmd), sig: None },
            );
        }
    }

    /// The presentational covers that survive the paint-time self-heal AND
    /// lie inside the visible grid-line window `[lo, hi]` (inclusive, grid
    /// space — the caller maps to screen rows via display offset and applies
    /// selection/search suppression). Returns INDICES into the feed's covers:
    /// history covers are permanent and accumulate over a long session, so
    /// per-frame healing must probe only the on-screen ones and never clone
    /// their Strings (UX MEDIUM-5). A blank spacer heals only while its input
    /// area is still blank; a history cover heals only while the raw row
    /// still carries the submitted command at the captured column — a
    /// cls/redraw drops it back to the honest raw row.
    pub fn healthy_covers_in(&self, lo: i32, hi: i32) -> Vec<usize> {
        let Some(bf) = &self.block_feed else { return Vec::new() };
        if bf.stale || bf.covers.is_empty() {
            return Vec::new();
        }
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let rows = self.size.rows as i32;
        let cols = self.size.cols as usize;
        let lo = lo.max(-history);
        let hi = hi.min(rows - 1);
        bf.covers
            .iter()
            .enumerate()
            .filter(|(_, c)| c.line >= lo && c.line <= hi && c.col <= cols)
            .filter(|(_, c)| match &c.cmd {
                // History cover: the raw row must still show the command.
                Some(cmd) => self.row_has_text_at(c.line, c.col, cmd.lines().next().unwrap_or("")),
                // Blank spacer: the input area must still be empty AND the
                // prompt region must still show EXACTLY the bare prompt the
                // spacer was minted for — a row erased in place (cls / the
                // conhost resize repaint) and rewritten by short output must
                // render raw, never be blanked by a stale spacer.
                None => {
                    let row = &grid[Line(c.line)];
                    let input_blank = (c.col.min(cols)..cols).all(|x| {
                        let ch = row[Column(x)].c;
                        ch == ' ' || ch == '\0'
                    });
                    input_blank
                        && c.sig
                            .as_deref()
                            .is_some_and(|sig| self.row_prefix_matches(c.line, c.col, sig))
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Test-compat view of `healthy_covers_in` over the whole grid, cloning
    /// the surviving covers (fine for tests; production paint uses the
    /// windowed index form).
    #[cfg(test)]
    pub fn healthy_covers(&self) -> Vec<PresCover> {
        let bf = match &self.block_feed {
            Some(bf) => bf,
            None => return Vec::new(),
        };
        self.healthy_covers_in(i32::MIN, i32::MAX)
            .into_iter()
            .map(|i| bf.covers[i].clone())
            .collect()
    }

    /// Cold-attach seed (task #15): the daemon certified this session is at a
    /// clean interactive prompt; seed `prompt_end` from the replay-space cell
    /// so the composer arms with the cover on at app open, exactly as a live
    /// 133;B scan would. No-op without a feed (hookless session).
    pub fn seed_prompt_end(&mut self, line: i32, col: usize) {
        if let Some(bf) = self.block_feed.as_mut() {
            if !bf.stale {
                bf.prompt_end = Some((line, col));
                // L6 belt: a cold-attach seed is authoritative — never let a
                // stray outstanding upgrade clobber it at the next quiet gap.
                bf.pending_prompt_end = false;
            }
        }
    }

    /// Reclaimable typed input at the current prompt, or why not (P4 §2.3).
    /// Pure grid read — the impure wrapper over `extract_input` adding the
    /// staleness gates: no feed / stale feed / no capture / pending
    /// DECSET-2026 sync block (the grid lags the stream — the cursor would
    /// lie) / alt-screen (belt: the gate already blocks alt).
    pub fn reclaim_text(&self) -> Reclaim {
        let Some(bf) = &self.block_feed else {
            return Reclaim::Unavailable;
        };
        if bf.stale {
            return Reclaim::Unavailable;
        }
        let Some(pe) = bf.prompt_end else {
            return Reclaim::Unavailable;
        };
        if self.parser.sync_timeout().sync_timeout().is_some() {
            return Reclaim::Unavailable;
        }
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return Reclaim::Unavailable;
        }
        extract_input(&self.term, pe)
    }

    /// D2C::StreamPos: create the feed lazily; scanning stays off until a
    /// Blocks frame with epoch > 0 enables it. Also the replay-coordinate
    /// baseline for ReplayAnchors: StreamPos arrives immediately after the
    /// Replay and before any live Output, so the grid state HERE is the
    /// replay coordinate space the daemon's hints are expressed in.
    pub fn set_stream_pos(&mut self, off: u64) {
        let h = self.term.grid().history_size();
        let size = (self.size.cols, self.size.rows);
        let bf = self.block_feed.get_or_insert_with(|| BlockFeed::new(h));
        bf.next_off = Some(off);
        bf.replay_base_history = h;
        bf.replay_size = size;
    }

    /// Mint history covers + block anchors from restored-history hints
    /// (D2C::ReplayAnchors) — the reopen-parity core: every pre-attach block
    /// row gets the same `❯ cwd cmd` cover and chrome anchor a live composer
    /// session would have minted, and superseded bare prompts get their blank
    /// spacer covers back. Every hint is re-verified against THIS grid before
    /// minting (the daemon already verified against its own replay parse;
    /// wrong covers are worse than raw rows): a block row must still render
    /// the command at the hinted column, a spacer row must still show a bare
    /// prompt with a blank input area. Rows are re-based by the history
    /// growth since the Replay parse (live output may precede this frame);
    /// stale/alt/resized-since-replay states drop the batch whole.
    pub fn apply_replay_hints(&mut self, mut hints: Vec<ReplayHint>) {
        if hints.is_empty() {
            return;
        }
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        let hist_now = self.term.grid().history_size();
        let size_now = (self.size.cols, self.size.rows);
        let rows = self.size.rows as i32;
        let delta = {
            let Some(bf) = self.block_feed.as_ref() else { return };
            if !bf.enabled || bf.stale || alt || bf.replay_size != size_now {
                return;
            }
            match hist_now.checked_sub(bf.replay_base_history) {
                Some(d) => d as i32,
                None => return, // history shrank since replay: unmappable
            }
        };
        hints.sort_by_key(|h| (h.row, h.start_off));
        // Verify pass (immutable grid reads), collecting what to mint.
        let hist = hist_now as i32;
        let mut anchors: Vec<BlockAnchor> = Vec::new();
        // Each block anchor's COVER row (the prompt row, not the walked
        // logical start) — the reference point for close bounds.
        let mut anchor_rows: Vec<i32> = Vec::new();
        let mut covers: Vec<PresCover> = Vec::new();
        let mut verified_rows: Vec<i32> = Vec::new();
        for h in &hints {
            let line = h.row - delta;
            if line < -hist || line >= rows {
                continue;
            }
            match &h.cmd {
                Some(cmd) => {
                    let first = cmd.lines().next().unwrap_or("");
                    if !self.row_has_text_at(line, h.col, first) {
                        continue;
                    }
                    anchors.push(BlockAnchor {
                        start_off: h.start_off,
                        line: walk_to_logical_start(&self.term, line, 64),
                        end_line: None, // second pass: the next verified row
                    });
                    anchor_rows.push(line);
                    covers.push(PresCover {
                        line,
                        col: h.col,
                        cwd: h.cwd.clone(),
                        cmd: Some(first.to_string()),
                        sig: None,
                    });
                    verified_rows.push(line);
                }
                None => {
                    // Spacer: bare prompt, blank input area — the same rule
                    // its paint-time self-heal will keep applying.
                    let sig = self.row_prefix_text(line, h.col);
                    if sig.is_empty() {
                        continue;
                    }
                    let grid = self.term.grid();
                    let cols = self.size.cols as usize;
                    let row_ref = &grid[Line(line)];
                    let blank = (h.col.min(cols)..cols).all(|x| {
                        let ch = row_ref[Column(x)].c;
                        ch == ' ' || ch == '\0'
                    });
                    if !blank {
                        continue;
                    }
                    covers.push(PresCover {
                        line,
                        col: h.col,
                        cwd: None,
                        cmd: None,
                        sig: Some(sig),
                    });
                    verified_rows.push(line);
                }
            }
        }
        if verified_rows.is_empty() {
            return;
        }
        // Close bounds: a restored block ends where the next verified row
        // (block or spacer — the next prompt) begins; the last one falls back
        // to the seeded prompt end (the live prompt row) when it lies below.
        let seeded_pe = self
            .block_feed
            .as_ref()
            .and_then(|bf| bf.prompt_end)
            .map(|(l, _)| l);
        for (a, &arow) in anchors.iter_mut().zip(&anchor_rows) {
            a.end_line = verified_rows
                .iter()
                .find(|&&r| r > arow)
                .copied()
                .or(seeded_pe.filter(|&l| l > arow));
        }
        let bf = self.block_feed.as_mut().unwrap();
        // Merge: hints are strictly pre-replay offsets, live-captured anchors
        // (output that arrived before this frame) strictly post-replay — the
        // sorted-by-start_off / non-decreasing-lines invariant holds with the
        // hints prepended. Defensive dedupe by start_off keeps a replayed
        // spoof from doubling an anchor.
        let hinted: std::collections::HashSet<u64> =
            anchors.iter().map(|a| a.start_off).collect();
        let live: Vec<BlockAnchor> = bf
            .anchors
            .iter()
            .filter(|a| !hinted.contains(&a.start_off))
            .copied()
            .collect();
        anchors.extend(live);
        bf.anchors = anchors;
        for c in covers {
            push_cover(&mut bf.covers, c);
        }
    }

    /// First Blocks frame with epoch > 0 (a hooked spawn exists): start
    /// scanning. epoch>0 — not TermKind — is the signal because the CLI
    /// restore wrapper spawns hooked while its kind stays Shell/Custom.
    pub fn enable_block_scan(&mut self) {
        let h = self.term.grid().history_size();
        let bf = self.block_feed.get_or_insert_with(|| BlockFeed::new(h));
        bf.enabled = true;
    }

    /// Scroll so grid line `line` sits ~2 rows below the viewport top (same
    /// math family as scroll_to_match). View-only.
    pub fn jump_to_line(&mut self, line: i32) {
        let history = self.term.grid().history_size() as i32;
        let desired = (2 - line).clamp(0, history);
        let cur = self.term.grid().display_offset() as i32;
        let delta = desired - cur;
        if delta != 0 {
            self.term.grid_mut().scroll_display(Scroll::Delta(delta));
        }
    }

    /// Enforce the synchronized-output time cap. vte's `StdSyncHandler` only
    /// records the 150ms deadline — nothing inside `advance` ever expires it,
    /// so an app that crashes (or stalls) mid-sync-block would leave its last
    /// frame invisibly buffered forever. Returns the pending deadline so the
    /// caller can schedule a wakeup for it; flushes and returns `None` once
    /// the deadline has passed (or none is pending).
    pub fn pump_sync(&mut self) -> Option<std::time::Instant> {
        let deadline = self.parser.sync_timeout().sync_timeout()?;
        if std::time::Instant::now() >= deadline {
            self.parser.stop_sync(&mut self.term);
            // Deferred bytes just hit the grid: change-detectors keyed on
            // feed_gen (activity rescan) must observe this flush too.
            self.feed_gen = self.feed_gen.wrapping_add(1);
            // The flush may have scrolled the grid (history growth) — the
            // anchors/covers/prompt_end shift must observe it NOW, not at the
            // next feed, or they paint one-frame-stale rows.
            self.track_scroll();
            self.prune_stale_covers();
            self.drain_events();
            self.clear_live_selection_on_output();
            None
        } else {
            Some(deadline)
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Title(t) => self.title = Some(t),
                Event::ResetTitle => self.title = None,
                // A bell is the terminal's own "look at me" signal — latch it as
                // a NeedsYou candidate (V-A). Previously dropped.
                Event::Bell => self.bell = true,
                _ => {}
            }
        }
    }

    /// The bottom-most `max` non-blank screen rows, top-to-bottom, as plain
    /// text. Used for the dashboard preview and prompt-signature detection.
    /// Reads the live screen region (Line 0..rows), independent of scrollback.
    pub fn preview_lines(&self, max: usize) -> Vec<String> {
        let grid = self.term.grid();
        let cols = self.size.cols as usize;
        let rows = self.size.rows as i32;
        let mut out: Vec<String> = Vec::new();
        for line in (0..rows).rev() {
            if out.len() >= max {
                break;
            }
            let row = &grid[Line(line)];
            let mut s = String::with_capacity(cols);
            for col in 0..cols {
                s.push(row[Column(col)].c);
            }
            let trimmed = s.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            out.push(trimmed.to_string());
        }
        out.reverse();
        out
    }

    /// Conservative, high-precision check for an interactive prompt that is
    /// waiting on the user (V-A NeedsYou). The signature table is deliberately
    /// tiny — a false positive ("needs you" when it doesn't) is worse than a
    /// miss, so only Claude Code's permission-prompt phrasing is matched.
    ///
    /// Allocation-free (UX HIGH-3): the caller runs this across the whole
    /// terminal fleet, so it walks cells directly instead of building per-row
    /// Strings; same semantics as matching against `preview_lines(8)`.
    pub fn looks_like_prompt(&self) -> bool {
        const SIGS: &[&str] = &["Do you want", "\u{276f} 1. Yes"];
        let title = self.title.as_deref().unwrap_or("");
        if SIGS.iter().any(|s| title.contains(s)) {
            return true;
        }
        let grid = self.term.grid();
        let cols = self.size.cols as usize;
        let rows = self.size.rows as i32;
        let mut seen = 0usize;
        for line in (0..rows).rev() {
            if seen >= 8 {
                break;
            }
            let row = &grid[Line(line)];
            // Trailing-trimmed length: the row is "blank" (skipped, not
            // counted toward the 8) when every cell is whitespace/NUL.
            let len = (0..cols)
                .rev()
                .find(|&c| {
                    let ch = row[Column(c)].c;
                    !(ch == '\0' || ch.is_whitespace())
                })
                .map(|c| c + 1)
                .unwrap_or(0);
            if len == 0 {
                continue;
            }
            seen += 1;
            if SIGS.iter().any(|sig| row_contains(row, len, sig)) {
                return true;
            }
        }
        false
    }

    /// Set the viewport's absolute display offset (0 = live bottom,
    /// history_size = oldest), clamped. View-only — used by the scrollbar
    /// thumb drag (UX MEDIUM-7), which maps thumb pixels to an absolute
    /// offset rather than accumulating deltas.
    pub fn set_display_offset(&mut self, target: i32) {
        let history = self.term.grid().history_size() as i32;
        let target = target.clamp(0, history);
        let cur = self.term.grid().display_offset() as i32;
        let delta = target - cur;
        if delta != 0 {
            self.term.grid_mut().scroll_display(Scroll::Delta(delta));
        }
    }

    /// Every match of `regex` across the whole scrollback, top-to-bottom.
    /// Called once per query change (not per frame) to size the "3/17" counter
    /// and drive prev/next navigation (V4 search).
    pub fn all_matches(&self, regex: &mut RegexSearch) -> Vec<Match> {
        let term = &self.term;
        let history = term.grid().history_size() as i32;
        let start = Point::new(Line(-history), Column(0));
        let end = Point::new(
            term.bottommost_line(),
            Column(self.size.cols.saturating_sub(1) as usize),
        );
        RegexIter::new(start, end, Direction::Right, term, regex).collect()
    }

    /// Scroll the local viewport so `m` sits about a third of the way down.
    /// View-only: the daemon and PTY are uninvolved (V4 search navigation).
    pub fn scroll_to_match(&mut self, m: &Match) {
        let line = m.start().line.0;
        let rows = self.size.rows as i32;
        let history = self.term.grid().history_size() as i32;
        let desired = (-line + rows / 3).clamp(0, history);
        let cur = self.term.grid().display_offset() as i32;
        let delta = desired - cur;
        if delta != 0 {
            self.term.grid_mut().scroll_display(Scroll::Delta(delta));
        }
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Scrollback rows currently stored (grid history). Grows exactly when
    /// screen rows scroll off the top — the composer's SubmitHold uses a
    /// delta on this as "the grid moved under the pinned cover row".
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// The cursor's grid line (screen space, 0-based).
    pub fn cursor_line(&self) -> i32 {
        self.term.grid().cursor.point.line.0
    }

    /// True when grid row `line` contains the characters of `expect` starting
    /// at cell `col` (P3 SubmitHold echo-landed check). Wide chars occupy a
    /// lead cell plus a spacer cell — spacers are skipped so CJK commands
    /// compare 1:1 with their source string. Running off the row's right edge
    /// with every compared cell matching counts as a match (a long command
    /// wraps; the visible part of THIS row shows no less than the user
    /// submitted). An empty `expect` never matches — there is nothing whose
    /// arrival could be confirmed.
    pub fn row_has_text_at(&self, line: i32, col: usize, expect: &str) -> bool {
        if expect.is_empty() {
            return false;
        }
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let rows = self.size.rows as i32;
        let cols = self.size.cols as usize;
        if line < -history || line >= rows || col >= cols {
            return false;
        }
        let row = &grid[Line(line)];
        let mut row_chars = (col..cols).filter_map(|c| {
            let cell = &row[Column(c)];
            (!cell.flags.contains(Flags::WIDE_CHAR_SPACER)).then_some(cell.c)
        });
        for want in expect.chars() {
            match row_chars.next() {
                Some(have) if have == want => {}
                None => return true, // row edge: everything that fit matched
                Some(_) => return false,
            }
        }
        true
    }

    /// Resize grid to fill `layout` px with `cell` px glyphs.
    /// Returns Some((cols, rows)) when the grid actually changed — the caller
    /// must forward that to the daemon so the PTY matches.
    pub fn resize_to(&mut self, layout: egui::Vec2, cell: egui::Vec2) -> Option<(u16, u16)> {
        let cols = (layout.x / cell.x.max(1.0)).floor() as u16;
        let rows = (layout.y / cell.y.max(1.0)).floor() as u16;
        if cols < 2 || rows < 2 {
            return None;
        }
        let cell_changed =
            self.size.cell_width != cell.x || self.size.cell_height != cell.y;
        if cols == self.size.cols && rows == self.size.rows {
            if cell_changed {
                self.size.cell_width = cell.x;
                self.size.cell_height = cell.y;
            }
            return None;
        }
        self.size = GridSize {
            cols,
            rows,
            cell_width: cell.x,
            cell_height: cell.y,
        };
        // A CLEAN prompt (cursor exactly at the captured prompt end, empty
        // input) has an invariant the reflow preserves: prompt_end IS the
        // cursor cell. alacritty carries the cursor to its reflowed position,
        // so re-deriving prompt_end from the post-resize cursor keeps it exact
        // across the resize — critical for the cold-attach SEED (no live 133;B
        // will ever re-capture it; clearing it wiped the boot cover on the
        // corrective strip-resize, R1) and a free win for live prompts too. A
        // DIRTY prompt (cursor past the end / stale feed) is dropped as before
        // — a wrong prompt-end is worse than a missing one.
        let clean_prompt = self.cursor_at_prompt_end();
        let pre = self.pre_resize_ordinals();
        let pre_covers = self.pre_resize_cover_ordinals();
        // Row GROWTH runs with alacritty's history-pull UNDONE
        // (serialize::resize_conhost — ONE implementation shared with the
        // daemon mirror and the restore/dead-attach scratch parses): alacritty
        // bottom-anchors a taller viewport by pulling rows out of scrollback
        // and dragging the cursor down; conhost NEVER does — its content
        // stays put and blank rows appear below, and the repaint it sends
        // after the PTY resize rewrites our screen to that layout. Letting
        // the pull stand meant the repaint BLANKED the pulled rows (for a
        // restored session those are the dead session's last screenful — the
        // "restore truncated my ls" loss) and stranded prompt_end rows below
        // the repainted cursor (the boot cover-never-paints bug). Column
        // changes keep full reflow semantics.
        crate::daemon::serialize::resize_conhost(
            &mut self.term,
            cols as usize,
            rows as usize,
        );
        self.apply_resize_ordinals(pre);
        // Presentational covers ride the SAME ordinal remap as anchors (the
        // resize-drops-covers smoking gun: the user resizes constantly, and
        // every reflow used to wipe every history cover + spacer — stacked
        // raw `PS …>` prompts resurfacing minutes after each fix pass).
        // Each remapped cover is re-verified against the reflowed grid
        // (history: the command still on the row at the captured col;
        // spacer: the bare-prompt sig still there and the input area blank)
        // — the genuinely un-remappable drop to the honest raw row.
        self.apply_resize_cover_ordinals(pre_covers);
        let cur = self.term.grid().cursor.point;
        if let Some(bf) = self.block_feed.as_mut() {
            bf.prompt_end = clean_prompt.then_some((cur.line.0, cur.column.0));
            // A prompt-render window can't survive a reflow (the incoming
            // row is unmappable mid-render) — drop, the ~15ms flash is
            // honest under a resize.
            bf.incoming_prompt = None;
            // Reflow moved the cursor row: re-baseline the up-move detector
            // so the next feed can't read the jump as a screen rewrite.
            bf.last_cursor_line = Some(cur.line.0);
        }
        Some((cols, rows))
    }

    /// PRE-resize cover ordinals: covers are single rows that are always
    /// logical starts (bare prompt rows / prompt+command rows), so they ride
    /// the same bottom-up logical-line ordinal walk as anchors. Rows that
    /// are NOT logical starts (impossible in normal operation) are dropped.
    fn pre_resize_cover_ordinals(&mut self) -> Option<Vec<(u32, PresCover)>> {
        let live = self
            .block_feed
            .as_ref()
            .is_some_and(|f| f.enabled && !f.stale && !f.covers.is_empty());
        if !live {
            return None;
        }
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            // pre_resize_ordinals already staled + cleared everything.
            return None;
        }
        let bottom = self.term.bottommost_line().0;
        let covers = &self.block_feed.as_ref().unwrap().covers;
        let top_needed = covers
            .iter()
            .map(|c| c.line)
            .min()
            .unwrap_or(bottom)
            .min(bottom);
        let mut count = 0u32;
        let mut ord = std::collections::HashMap::new();
        for r in (top_needed..=bottom).rev() {
            if is_countable_start(&self.term, r) {
                count += 1;
                ord.insert(r, count);
            }
        }
        let out: Vec<(u32, PresCover)> = covers
            .iter()
            .filter(|c| c.line <= bottom)
            .filter_map(|c| ord.get(&c.line).map(|&o| (o, c.clone())))
            .collect();
        (!out.is_empty()).then_some(out)
    }

    /// POST-resize: place each cover at its ordinal's new row, then verify
    /// it against the reflowed grid exactly like the paint-time self-heal —
    /// remap where exact, drop the rest (drop-don't-drift).
    fn apply_resize_cover_ordinals(&mut self, pre: Option<Vec<(u32, PresCover)>>) {
        let Some(bf) = self.block_feed.as_mut() else { return };
        bf.covers.clear();
        let Some(pre) = pre else { return };
        if bf.stale {
            return;
        }
        let max_ord = pre.iter().map(|(o, _)| *o).max().unwrap_or(0);
        let bottom = self.term.bottommost_line().0;
        let history = self.term.grid().history_size() as i32;
        let mut row_of_ord: Vec<Option<i32>> = vec![None; max_ord as usize + 1];
        let mut count = 0u32;
        let mut r = bottom;
        while r >= -history && count < max_ord {
            if is_countable_start(&self.term, r) {
                count += 1;
                row_of_ord[count as usize] = Some(r);
            }
            r -= 1;
        }
        let cols = self.size.cols as usize;
        let mut remapped: Vec<PresCover> = Vec::with_capacity(pre.len());
        for (o, mut c) in pre {
            let Some(line) = row_of_ord.get(o as usize).copied().flatten() else {
                continue;
            };
            c.line = line;
            if c.col >= cols {
                continue; // the prompt region no longer fits this width
            }
            remapped.push(c);
        }
        // Verify against the reflowed grid (needs &self grid reads, so the
        // block_feed borrow ends first).
        let verified: Vec<PresCover> = remapped
            .into_iter()
            .filter(|c| match &c.cmd {
                Some(cmd) => {
                    self.row_has_text_at(c.line, c.col, cmd.lines().next().unwrap_or(""))
                }
                None => {
                    let grid = self.term.grid();
                    let row = &grid[Line(c.line)];
                    let input_blank = (c.col.min(cols)..cols).all(|x| {
                        let ch = row[Column(x)].c;
                        ch == ' ' || ch == '\0'
                    });
                    input_blank
                        && c.sig
                            .as_deref()
                            .is_some_and(|sig| self.row_prefix_matches(c.line, c.col, sig))
                }
            })
            .collect();
        if let Some(bf) = self.block_feed.as_mut() {
            bf.covers = verified;
        }
    }

    /// PRE-resize: one bottom-up walk assigning each anchor its logical
    /// ordinal — ordinal(row) = number of logical-start rows in
    /// [row ..= bottommost_line]. Returns None when there is nothing to
    /// remap. Resizing while ALT_SCREEN makes the primary grid inaccessible
    /// (`inactive_grid` is private — known from the serializer work), so the
    /// walk cannot run: anchors go stale instead of garbage.
    fn pre_resize_ordinals(&mut self) -> Option<Vec<(u64, u32, Option<u32>)>> {
        let live = self
            .block_feed
            .as_ref()
            .is_some_and(|f| f.enabled && !f.stale && !f.anchors.is_empty());
        if !live {
            return None;
        }
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            let bf = self.block_feed.as_mut().unwrap();
            bf.stale = true;
            bf.anchors.clear();
            bf.prompt_end = None;
            bf.covers.clear();
            return None;
        }
        let bottom = self.term.bottommost_line().0;
        let top_needed = self
            .block_feed
            .as_ref()
            .unwrap()
            .anchors
            .iter()
            .map(|a| a.line)
            .min()
            .unwrap_or(bottom)
            .min(bottom);
        let mut count = 0u32;
        let mut ord = std::collections::HashMap::new();
        for r in (top_needed..=bottom).rev() {
            if is_countable_start(&self.term, r) {
                count += 1;
            }
            ord.insert(r, count);
        }
        let bf = self.block_feed.as_ref().unwrap();
        Some(
            bf.anchors
                .iter()
                .map(|a| {
                    (
                        a.start_off,
                        ord.get(&a.line).copied().unwrap_or(0),
                        a.end_line.and_then(|e| ord.get(&e).copied()),
                    )
                })
                .collect(),
        )
    }

    /// POST-resize: walk bottom-up again counting logical starts; when the
    /// count reaches an ordinal, that row is the anchor's new line. Anchors
    /// whose ordinal is never reached (reflow pushed them off the ring) are
    /// pruned; a pruned end_line falls back to None (render then bounds the
    /// block by the next anchor).
    fn apply_resize_ordinals(&mut self, pre: Option<Vec<(u64, u32, Option<u32>)>>) {
        let Some(pre) = pre else { return };
        let max_ord = pre
            .iter()
            .map(|(_, o, e)| (*o).max(e.unwrap_or(0)))
            .max()
            .unwrap_or(0);
        let bottom = self.term.bottommost_line().0;
        let history = self.term.grid().history_size() as i32;
        let mut row_of_ord: Vec<Option<i32>> = vec![None; max_ord as usize + 1];
        let mut count = 0u32;
        let mut r = bottom;
        while r >= -history && count < max_ord {
            if is_countable_start(&self.term, r) {
                count += 1;
                row_of_ord[count as usize] = Some(r);
            }
            r -= 1;
        }
        let h = self.term.grid().history_size();
        let bf = self.block_feed.as_mut().unwrap();
        bf.anchors.clear();
        for (start_off, o, e) in pre {
            let Some(line) = row_of_ord.get(o as usize).copied().flatten() else {
                continue;
            };
            let end_line = e.and_then(|k| row_of_ord.get(k as usize).copied().flatten());
            bf.anchors.push(BlockAnchor {
                start_off,
                line,
                end_line,
            });
        }
        bf.last_history = h;
    }

    /// Scroll the LOCAL viewport only (Shift+PageUp/PageDown paging — the
    /// caller gates alt-screen). Wheel gestures route through `wheel()`,
    /// which owns the mouse-report / arrow-key forwarding decision.
    pub fn scroll(&mut self, delta: i32, _out: &mut Vec<u8>) {
        if delta != 0 {
            self.term.grid_mut().scroll_display(Scroll::Delta(delta));
        }
    }

    /// A wheel gesture of `delta` lines (positive = up) at grid point `at`,
    /// routed per `wheel_route`. Field bug this fixes: claude (alt-screen +
    /// ?1003h any-event tracking, no wheel-report path here) fell through to
    /// the alternate-scroll branch and received UP/DOWN ARROWS instead of
    /// the wheel events it scrolls its transcript with.
    pub fn wheel(&mut self, delta: i32, modifiers: Modifiers, at: Point, out: &mut Vec<u8>) {
        if delta == 0 {
            return;
        }
        match wheel_route(self.mode(), modifiers.shift) {
            WheelRoute::Report => {
                // Press-only, one event per line (xterm wheel semantics —
                // wheel buttons never emit a release).
                let btn = if delta > 0 {
                    MouseButton::WheelUp
                } else {
                    MouseButton::WheelDown
                };
                for _ in 0..delta.abs() {
                    self.mouse_report(btn, modifiers, at, true, out);
                }
            }
            WheelRoute::Arrows => {
                let cmd = if delta > 0 { b'A' } else { b'B' };
                for _ in 0..delta.abs() {
                    out.extend_from_slice(&[0x1b, b'O', cmd]);
                }
            }
            WheelRoute::Viewport => {
                self.term.grid_mut().scroll_display(Scroll::Delta(delta));
            }
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Map a pixel position (relative to the CONTENT rect) to a grid point.
    /// `y` may be NEGATIVE: the continuity fill draws scrollback above the
    /// live viewport after a restore, and that text must be selectable —
    /// negative rows resolve to history lines (clamped at the oldest).
    /// Column math is FLOAT division: cell_width is fractional whenever the
    /// physical-pixel snap lands off the point grid (13px at ppp=1.5 =
    /// 8.667pt cells — the common case). The old integer division truncated
    /// the DIVISOR (x/8 instead of x/8.667), so the mapped column overshot
    /// the pointer by ~8% of x — the "drag over-selects in claude" bug: the
    /// highlight ran cells past the dragged cell, worse the further right
    /// the pointer was (and forwarded SGR mouse reports carried the same
    /// wrong column). The renderer places cells at fractional cell_w; the
    /// hit-test must divide by the same number.
    pub fn selection_point(&self, x: f32, y: f32) -> Point {
        let col = (x.max(0.0) / self.size.cell_width.max(1.0)) as usize;
        let col = Column(col.min(self.size.cols as usize - 1));
        let row = (y / self.size.cell_height.max(1.0)).floor() as i32;
        let row = row.min(self.size.rows as i32 - 1);
        let offset = self.term.grid().display_offset() as i32;
        let history = self.term.grid().history_size() as i32;
        let line = (row - offset).max(-history);
        Point::new(Line(line), col)
    }

    fn selection_side(&self, x: f32) -> Side {
        // Same float rule as selection_point (the old `usize %` truncated the
        // cell width and drifted the midpoint test rightward with x).
        let frac = (x.max(0.0) / self.size.cell_width.max(1.0)).fract();
        if frac > 0.5 {
            Side::Right
        } else {
            Side::Left
        }
    }

    pub fn start_selection(&mut self, ty: SelectionType, x: f32, y: f32) {
        let point = self.selection_point(x, y);
        self.term.selection = Some(Selection::new(ty, point, self.selection_side(x)));
    }

    pub fn update_selection(&mut self, x: f32, y: f32) {
        let point = self.selection_point(x, y);
        let side = self.selection_side(x);
        if let Some(selection) = &mut self.term.selection {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Select the whole buffer, oldest history line → bottom-right of the
    /// screen (QOL §3.2). Copy stays a second, deliberate click (WT parity —
    /// DO-NOT 2: no surprise clipboard writes); the copy itself then rides
    /// `selection_text`'s display-stable synthesis like every copy surface.
    pub fn select_all(&mut self) {
        let history = self.term.grid().history_size() as i32;
        let mut sel = Selection::new(
            SelectionType::Lines,
            Point::new(Line(-history), Column(0)),
            Side::Left,
        );
        sel.update(
            Point::new(
                self.term.bottommost_line(),
                Column((self.size.cols as usize).saturating_sub(1)),
            ),
            Side::Right,
        );
        self.term.selection = Some(sel);
    }

    /// Clear the LOCAL scrollback ring — a VIEW gesture (QOL §7.2), never
    /// data destruction: the daemon mirror, journal, and blocks sidecar are
    /// untouched (mirror purity, DO-NOT 9), so a reattach resurrects the
    /// history via serialized replay + ReplayAnchors. GUI anchoring state is
    /// pruned exactly like the ED3 rule in `track_scroll` (drop-don't-drift):
    /// history rows died, screen rows live on.
    pub fn clear_scrollback_view(&mut self) {
        self.scroll_to_bottom();
        self.term.grid_mut().clear_history();
        self.clear_selection(); // its coordinates just died
        self.jump_flash = None;
        self.feed_gen += 1; // grid changed: activity/prompt rescans re-run
        if let Some(bf) = self.block_feed.as_mut() {
            bf.anchors.retain(|a| a.line >= 0);
            if bf.prompt_end.is_some_and(|(l, _)| l < 0) {
                bf.prompt_end = None;
            }
            if bf.incoming_prompt.is_some_and(|l| l < 0) {
                bf.incoming_prompt = None;
            }
            bf.covers.retain(|c| c.line >= 0);
            // Keep the delta tracker honest so the next feed doesn't read the
            // shrink as a second ED3.
            bf.last_history = self.term.grid().history_size();
        }
    }

    /// Selection → clipboard text, DISPLAY-STABLE (§6): covered rows
    /// contribute the text the user SEES, not the raw grid underneath.
    /// - history cover ⇒ `❯ {cwd} {cmd}` (full cwd — copy is the un-elided
    ///   displayed line), spacer / blanked current-prompt row ⇒ "".
    /// - WHOLE-ROW rule: a covered line contributes its full synthesized
    ///   text whenever ANY selected cell touches it (the cover is an atomic
    ///   presentational unit; grid columns mean nothing against painted
    ///   galleys). Partial column ranges apply to RAW lines only.
    /// - Raw wrapped rows join without '\n'; a cover always forces a '\n'
    ///   boundary on both sides (an interrupted wrap chain must not fuse).
    ///
    /// The classification predicate is `healthy_covers_in` — copy matches
    /// paint by construction; an unhealthy cover copies raw.
    pub fn selection_text(&self) -> Option<String> {
        let range = self.term.selection.as_ref()?.to_range(&self.term)?;
        let (lo, hi) = (range.start.line.0, range.end.line.0);
        let mut covered: std::collections::HashMap<i32, String> =
            std::collections::HashMap::new();
        for i in self.healthy_covers_in(lo, hi) {
            let Some(c) = self.block_feed.as_ref().and_then(|f| f.covers.get(i)) else {
                continue;
            };
            let text = match (&c.cmd, &c.cwd) {
                (Some(cmd), Some(cwd)) => format!("\u{276f} {cwd} {cmd}"),
                (Some(cmd), None) => format!("\u{276f} {cmd}"),
                (None, _) => String::new(),
            };
            covered.insert(c.line, text);
        }
        if let Some(l) = self.cur_blank_line {
            if l >= lo && l <= hi {
                covered.insert(l, String::new());
            }
        }
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let rows = self.size.rows as i32;
        let cols = self.size.cols as usize;
        let mut out = String::new();
        for line in lo..=hi {
            let sep = line < hi;
            if let Some(synth) = covered.get(&line) {
                out.push_str(synth);
                if sep {
                    out.push('\n');
                }
                continue;
            }
            if line < -history || line >= rows {
                if sep {
                    out.push('\n');
                }
                continue;
            }
            let row = &grid[Line(line)];
            let c0 = if line == lo { range.start.column.0 } else { 0 };
            let c1 = if line == hi {
                range.end.column.0.min(cols - 1)
            } else {
                cols - 1
            };
            let mut s = String::new();
            for c in c0..=c1 {
                let cell = &row[Column(c)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                // SGR 8 conceal renders blank — copy what is displayed (§6).
                s.push(if cell.c == '\0' || cell.flags.contains(Flags::HIDDEN) {
                    ' '
                } else {
                    cell.c
                });
            }
            // Wrap-aware join: only when the selection reaches the row's
            // right edge, the row wraps, and the next line is raw.
            let wraps = c1 == cols - 1
                && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE)
                && sep
                && !covered.contains_key(&(line + 1));
            if wraps {
                out.push_str(&s);
            } else {
                out.push_str(s.trim_end());
                if sep {
                    out.push('\n');
                }
            }
        }
        Some(out)
    }

    pub fn mouse_report(
        &self,
        button: MouseButton,
        modifiers: Modifiers,
        point: Point,
        pressed: bool,
        out: &mut Vec<u8>,
    ) {
        let mut mods = 0u8;
        if modifiers.contains(Modifiers::SHIFT) {
            mods += 4;
        }
        if modifiers.contains(Modifiers::ALT) {
            mods += 8;
        }
        if modifiers.contains(Modifiers::COMMAND) {
            mods += 16;
        }
        let mode = self.mode();
        let code = button as u8 + mods;
        // Reports are viewport-space: a point in the continuity-fill history
        // region clamps to the top row (apps can't address scrollback).
        let report_line = point.line.0.max(0);
        if mode.contains(TermMode::SGR_MOUSE) {
            let c = if pressed { 'M' } else { 'm' };
            out.extend_from_slice(
                format!("\x1b[<{};{};{}{}", code, point.column.0 + 1, report_line + 1, c)
                    .as_bytes(),
            );
        } else {
            let code = if pressed { code } else { 3 + mods };
            let col = point.column.0;
            let line = point.line.0.max(0) as usize;
            if col < 223 && line < 223 {
                out.extend_from_slice(&[
                    0x1b,
                    b'[',
                    b'M',
                    32 + code,
                    (32 + 1 + col) as u8,
                    (32 + 1 + line) as u8,
                ]);
            }
        }
    }
}

/// True when the row's first `len` cells contain `sig` as a contiguous char
/// sequence — the cell-walk equivalent of building the row's String and
/// calling `contains` (allocation-free; UX HIGH-3).
fn row_contains(
    row: &alacritty_terminal::grid::Row<alacritty_terminal::term::cell::Cell>,
    len: usize,
    sig: &str,
) -> bool {
    let m = sig.chars().count();
    if m == 0 || m > len {
        return false;
    }
    for start in 0..=(len - m) {
        if sig
            .chars()
            .enumerate()
            .all(|(k, want)| row[Column(start + k)].c == want)
        {
            return true;
        }
    }
    false
}

/// Insert a presentational cover, deduped by grid line (a later cover for the
/// same row REPLACES an earlier one — e.g. a history cover supersedes a stale
/// spacer that shifted onto the same line), bounded to `SPACER_CAP` with the
/// oldest dropped first (drop-don't-drift).
fn push_cover(covers: &mut Vec<PresCover>, cover: PresCover) {
    covers.retain(|c| c.line != cover.line);
    covers.push(cover);
    if covers.len() > SPACER_CAP {
        covers.remove(0);
    }
}

/// What the grid holds between the prompt end and the cursor (P4 typeahead
/// reclaim). `Text` is EXACTLY recoverable input; every ambiguous shape is a
/// refusal variant and the caller falls back to the v1 discard — refuse over
/// guess, because the user will SUBMIT whatever we reclaim.
#[derive(Debug, Clone, PartialEq)]
pub enum Reclaim {
    /// Exactly recoverable single-logical-line input (may be empty after
    /// trailing-whitespace trim).
    Text(String),
    /// A non-wrapped row boundary inside the span: PSReadLine rendered a
    /// multi-line buffer with continuation prompts (user-configurable text —
    /// never guess-strip it).
    MultiLine,
    /// Real (non-ghost) cells right of the cursor, or the cursor row wraps
    /// onward: the caret is mid-buffer and the span misses text.
    CursorMidLine,
    /// No prompt_end capture / stale feed / pending sync block / span
    /// implausible. Nothing readable.
    Unavailable,
}

/// Bounded walk cap: 64 rows ≥ the shell's own 2000-char cmd truncation at
/// any sane width; a longer span means the capture is stale.
const RECLAIM_ROW_CAP: i32 = 64;

/// Pure extraction of the typed input between `prompt_end` and the cursor
/// (P4 §2.2). Free function, generic over the event listener so the probe
/// can run it against a Term it rebuilt from captured session bytes.
///
/// Ghost text: cells right of the cursor whose flags intersect DIM|ITALIC
/// are PSReadLine prediction ghosts (default `InlinePredictionColor` is
/// `\e[97;2;3m`) and are ignored; any OTHER non-space cell right of the
/// caret is real text (Home/arrows) ⇒ refuse. Customized prediction colors
/// without either attribute fail toward refusal — convenience lost, never
/// wrong text.
pub fn extract_input<L: EventListener>(term: &Term<L>, prompt_end: (i32, usize)) -> Reclaim {
    let grid = term.grid();
    let cur = grid.cursor.point;
    let (pl, pc) = prompt_end;
    let history = grid.history_size() as i32;
    let cols = term.columns();
    // Plausibility: cursor above the capture (screen rewritten), an absurd
    // span, or a capture that fell off the ring ⇒ nothing readable.
    if pl > cur.line.0 || cur.line.0 - pl > RECLAIM_ROW_CAP || pl < -history {
        return Reclaim::Unavailable;
    }
    if pl == cur.line.0 {
        if pc == cur.column.0 {
            return Reclaim::Text(String::new()); // clean; totality only
        }
        if pc > cur.column.0 {
            // Cursor LEFT of the prompt end on the same row: the prompt was
            // re-rendered shorter; the capture lies.
            return Reclaim::Unavailable;
        }
    }
    // Wrap-chain check for every row above the cursor row: a hard newline in
    // the span means a rendered continuation prompt (Shift+Enter/incomplete
    // syntax) whose text is user-configurable — refusal is the only correct
    // move.
    for r in pl..cur.line.0 {
        if !grid[Line(r)][Column(cols - 1)]
            .flags
            .contains(Flags::WRAPLINE)
        {
            return Reclaim::MultiLine;
        }
    }
    // Cursor-row trailing checks: buffer continuing BELOW the cursor, or
    // real (non-ghost) text right of the caret ⇒ the span misses text.
    if grid[Line(cur.line.0)][Column(cols - 1)]
        .flags
        .contains(Flags::WRAPLINE)
    {
        return Reclaim::CursorMidLine;
    }
    {
        let row = &grid[Line(cur.line.0)];
        for c in cur.column.0..cols {
            let cell = &row[Column(c)];
            if cell.c != ' '
                && cell.c != '\0'
                && !cell.flags.intersects(Flags::DIM | Flags::ITALIC)
            {
                return Reclaim::CursorMidLine;
            }
        }
    }
    // Collect: first row from the prompt end, interior rows whole, cursor
    // row up to (exclusive of) the caret. CJK spacer cells are skipped so
    // wide chars compare 1:1 with their source string.
    let mut s = String::new();
    for r in pl..=cur.line.0 {
        let row = &grid[Line(r)];
        let c0 = if r == pl { pc } else { 0 };
        let c1 = if r == cur.line.0 { cur.column.0 } else { cols };
        for c in c0..c1.min(cols) {
            let cell = &row[Column(c)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            // Never-written cells read '\0'; a NUL must not reach the draft.
            s.push(if cell.c == '\0' { ' ' } else { cell.c });
        }
    }
    Reclaim::Text(s.trim_end().to_string())
}

/// Walk up the WRAPLINE chain: row r's logical start is the topmost row s ≤ r
/// such that row s-1 does not wrap into s. Bounded (64 rows ≈ a 10k-char
/// command at 160 cols).
fn walk_to_logical_start(term: &Term<EventProxy>, mut r: i32, cap: usize) -> i32 {
    let grid = term.grid();
    let history = grid.history_size() as i32;
    let cols = term.columns();
    for _ in 0..cap {
        let above = r - 1;
        if above < -history {
            break;
        }
        if !grid[Line(above)][Column(cols - 1)]
            .flags
            .contains(Flags::WRAPLINE)
        {
            break;
        }
        r = above;
    }
    r
}

/// A logical start that participates in resize-ordinal counting: BLANK rows
/// are excluded. A rows-GROW opens blank rows at the screen bottom (the
/// conhost-parity grow keeps content put and blanks appear below) — counting
/// those as starts shifted every ordinal by exactly the growth and remapped
/// anchors/covers onto wrong rows (the storm repro). Content rows are stable
/// ordinal units across reflow; viewport-artifact blanks are not.
fn is_countable_start(term: &Term<EventProxy>, r: i32) -> bool {
    use alacritty_terminal::term::cell::LineLength as _;
    is_logical_start(term, r) && term.grid()[Line(r)].line_length().0 > 0
}

/// True when row `r` starts a logical line (the row above doesn't wrap into
/// it, or there is no row above).
fn is_logical_start(term: &Term<EventProxy>, r: i32) -> bool {
    let grid = term.grid();
    let history = grid.history_size() as i32;
    let above = r - 1;
    if above < -history {
        return true;
    }
    let cols = term.columns();
    !grid[Line(above)][Column(cols - 1)]
        .flags
        .contains(Flags::WRAPLINE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// UX HIGH-3 activity-rescan gate: `feed_gen` advances exactly when the
    /// parser consumed bytes (advance / advance_live), never on read-only
    /// per-frame work — so `update_activity` can skip the prompt-signature
    /// grid walk for terminals with no new output.
    #[test]
    fn feed_gen_tracks_consumption_only() {
        let mut b = TermBackend::new(GridSize::default());
        let g0 = b.feed_gen;
        // Read-only per-frame work must not bump it.
        let _ = b.looks_like_prompt();
        let _ = b.healthy_covers_in(i32::MIN, i32::MAX);
        let _ = b.preview_lines(8);
        assert_eq!(b.feed_gen, g0, "reads must not advance feed_gen");
        b.advance(b"hello");
        assert_eq!(b.feed_gen, g0 + 1, "replay path consumes");
        b.advance_live(b"world");
        assert_eq!(b.feed_gen, g0 + 2, "live path consumes");
        let _ = b.looks_like_prompt();
        assert_eq!(b.feed_gen, g0 + 2);
    }

    /// The allocation-free `looks_like_prompt` cell walk matches the old
    /// preview_lines(8)-based semantics: signature anywhere in the bottom 8
    /// NON-BLANK rows (or the title) latches; other text does not; a
    /// signature pushed above that window stops matching.
    #[test]
    fn looks_like_prompt_cell_walk_parity() {
        let mut b = TermBackend::new(GridSize::default());
        assert!(!b.looks_like_prompt(), "empty grid is quiet");
        b.advance(b"PS C:\\> ");
        assert!(!b.looks_like_prompt(), "a plain prompt is not NeedsYou");
        b.advance(b"\r\nDo you want to allow this?\r\n");
        assert!(b.looks_like_prompt(), "signature row matches");
        // Blank rows between content don't consume the 8-row budget.
        b.advance(b"\r\n\r\n\r\nmore\r\n");
        assert!(b.looks_like_prompt(), "blank rows are skipped, not counted");
        // 8 fresh non-blank rows push the signature out of the window.
        for i in 0..8 {
            b.advance(format!("filler {i}\r\n").as_bytes());
        }
        assert!(
            !b.looks_like_prompt(),
            "signature above the bottom-8 window no longer latches"
        );
        // Title signature still matches.
        b.title = Some("Do you want to proceed?".into());
        assert!(b.looks_like_prompt());
    }

    #[test]
    fn esu_applies_sync_block_atomically() {
        let mut b = TermBackend::new(GridSize::default());
        b.advance(b"\x1b[?2026hATOMIC");
        assert!(
            b.preview_lines(1).is_empty(),
            "sync-block content leaked to the grid before ESU"
        );
        b.advance(b"\x1b[?2026l");
        assert_eq!(b.preview_lines(1), vec!["ATOMIC".to_string()]);
        assert!(b.pump_sync().is_none(), "no deadline may linger after ESU");
    }

    // ── Journal-block anchoring (P2) ────────────────────────────────

    fn hook(verb: &str, json: &str) -> Vec<u8> {
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        format!("\x1b]7717;0123456789abcdef;{verb};{hex}\x07").into_bytes()
    }

    fn exec_hook(cmd: &str) -> Vec<u8> {
        hook("exec", &format!(r#"{{"c":"{cmd}"}}"#))
    }

    fn bhist(cols: u16, rows: u16, hist: usize) -> TermBackend {
        let mut b = TermBackend::new_with_history(
            GridSize {
                cols,
                rows,
                cell_width: 8.0,
                cell_height: 16.0,
            },
            hist,
        );
        b.set_stream_pos(0);
        b.enable_block_scan();
        b
    }

    fn row_text(b: &TermBackend, line: i32) -> String {
        let grid = b.term.grid();
        let cols = b.size.cols as usize;
        let row = &grid[Line(line)];
        (0..cols)
            .map(|c| row[Column(c)].c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn anchors(b: &TermBackend) -> &[BlockAnchor] {
        &b.block_feed.as_ref().unwrap().anchors
    }

    /// Bug 2 (stale lane cwd): the pre hook's `d` payload is captured at
    /// FEED time — the lane label reads it the same frame the fresh prompt
    /// renders. Empty payloads (cmd's static PROMPT) leave it untouched so
    /// the Snapshot-meta fallback applies.
    #[test]
    fn feed_time_cwd_captured_from_pre_hook() {
        let mut b = bhist(80, 24, 100);
        assert_eq!(b.feed_cwd(), None);
        b.advance_live(&hook("pre", r#"{"e":0,"n":1,"d":"C:\\proj"}"#));
        assert_eq!(b.feed_cwd(), Some("C:\\proj"), "pre cwd lands feed-time");
        // Empty payload (cmd family): keep the previous value.
        b.advance_live(&hook("pre", r#"{"e":null,"n":0}"#));
        assert_eq!(b.feed_cwd(), Some("C:\\proj"));
        // A later cd updates it.
        b.advance_live(&hook("pre", r#"{"e":0,"n":2,"d":"C:\\proj\\sub"}"#));
        assert_eq!(b.feed_cwd(), Some("C:\\proj\\sub"));
    }

    /// v0.1.1 quiescence-resolved 133;B (the ConPTY OSC-vs-text reorder,
    /// field: every capture on the laptop landed at the row start and the
    /// composer read the prompt string as typed text): the immediate
    /// capture is provisional; once output quiesces it upgrades to the
    /// settled cursor — the true prompt end.
    #[test]
    fn prompt_end_upgrades_after_reorder() {
        let mut b = bhist(80, 24, 100);
        // RACE order: the OSC beats the prompt text into the stream. One
        // feed — split-feed captures at the OSC boundary internally, so the
        // immediate capture is the pre-text cursor even though the text is
        // in the same chunk (and the test can't be flaked by a wall-clock
        // stall between two calls).
        b.advance_live(&hook("pre", r#"{"e":0,"n":1,"d":"/home/z"}"#));
        b.advance_live(b"\x1b]133;B\x07zany@MSI:~$ ");
        assert_eq!(
            b.block_feed.as_ref().unwrap().prompt_end,
            Some((0, 0)),
            "immediate capture = the wrong cell (row start)"
        );
        assert!(
            !b.cursor_at_prompt_end(),
            "pre-upgrade the wrong capture reads honestly dirty"
        );
        // Quiescence: the upgrade re-reads the settled cursor.
        let later = std::time::Instant::now() + Duration::from_millis(200);
        assert_eq!(b.poll_pending_prompt_end(later), None);
        assert_eq!(b.block_feed.as_ref().unwrap().prompt_end, Some((0, 12)));
        assert!(b.cursor_at_prompt_end(), "upgraded capture = the true end");
        // Resolved: later polls are free (no wakeup churn).
        assert_eq!(b.poll_pending_prompt_end(later), None);
    }

    /// v0.1.1: a correct-order capture (real prompt text to the cursor's
    /// left) is NEVER pended — the upgrade only ever heals the reorder race.
    #[test]
    fn prompt_end_not_pended_on_correct_order() {
        let mut b = bhist(80, 24, 100);
        b.advance_live(&hook("pre", r#"{"e":0,"n":1,"d":"/home/z"}"#));
        b.advance_live(b"zany@MSI:~$ \x1b]133;B\x07");
        assert!(b.cursor_at_prompt_end());
        assert!(
            !b.block_feed.as_ref().unwrap().pending_prompt_end,
            "a capture with prompt text to its left is final, not pending"
        );
        let pe = b.block_feed.as_ref().unwrap().prompt_end;
        let later = std::time::Instant::now() + Duration::from_millis(200);
        assert_eq!(b.poll_pending_prompt_end(later), None);
        assert_eq!(b.block_feed.as_ref().unwrap().prompt_end, pe);
        assert!(b.cursor_at_prompt_end());
    }

    /// v0.1.1 H1 regression: a CORRECT capture followed by ASYNC output (a
    /// backgrounded job's log line — NO input, so note_input never fires)
    /// must leave prompt_end put. The old unconditional pend re-pointed it at
    /// the post-output cursor at the 40ms-quiet moment, turning honest-dirty
    /// into WRONGLY-CLEAN (armed cover over async output). Now: not pended ⇒
    /// the async output just moves the cursor off the true end ⇒ dirty, like
    /// v0.1.0.
    #[test]
    fn correct_capture_survives_async_output() {
        let mut b = bhist(80, 24, 100);
        b.advance_live(&hook("pre", r#"{"e":0,"n":1,"d":"/home/z"}"#));
        b.advance_live(b"zany@MSI:~$ \x1b]133;B\x07");
        assert!(b.cursor_at_prompt_end(), "clean at the true prompt end");
        let pe = b.block_feed.as_ref().unwrap().prompt_end;
        // A backgrounded job prints to the tty ~15ms later (no input).
        b.advance_live(b"Listening on :8080\r\n");
        // 40ms+ of quiet, then the per-frame poll: it must NOT move prompt_end
        // onto the post-output cursor.
        let later = std::time::Instant::now() + Duration::from_millis(200);
        assert_eq!(b.poll_pending_prompt_end(later), None);
        assert_eq!(
            b.block_feed.as_ref().unwrap().prompt_end,
            pe,
            "async output must never migrate a correct prompt-end"
        );
        assert!(
            !b.cursor_at_prompt_end(),
            "the cursor left the end ⇒ dirty (honest), never wrongly-clean"
        );
    }

    /// v0.1.1: on a RACE capture (col 0, pended), local input FREEZES the
    /// upgrade on the immediate capture — resolving past the keystroke echo
    /// would fold typed text into the "prompt" and read a dirty line as clean
    /// (the fused-submit hazard, worse than today's dirty ManualOnly).
    #[test]
    fn prompt_end_upgrade_freezes_on_input() {
        let mut b = bhist(80, 24, 100);
        b.advance_live(&hook("pre", r#"{"e":0,"n":1,"d":"/home/z"}"#));
        // Race order: OSC before its prompt text ⇒ col-0 capture, pended.
        b.advance_live(b"\x1b]133;B\x07");
        assert!(b.block_feed.as_ref().unwrap().pending_prompt_end);
        // The user types inside the window; the echo follows as output.
        b.note_input();
        b.advance_live(b"ls");
        let later = std::time::Instant::now() + Duration::from_millis(200);
        assert_eq!(b.poll_pending_prompt_end(later), None);
        assert_eq!(
            b.block_feed.as_ref().unwrap().prompt_end,
            Some((0, 0)),
            "the immediate (race) capture stands frozen; the echo never joins it"
        );
        assert!(!b.cursor_at_prompt_end(), "typed text reads dirty, as before");
    }

    /// v0.1.1: a fresh `pre` (a new prompt is coming) drops a still-pending
    /// upgrade for the superseded prompt — it must never resolve against
    /// the NEXT prompt's cursor.
    #[test]
    fn prompt_end_pending_dropped_by_next_pre() {
        let mut b = bhist(80, 24, 100);
        b.advance_live(&hook("pre", r#"{"e":0,"n":1,"d":"/home/z"}"#));
        // Race capture (pending) + text + newline + the NEXT pre, one feed.
        let mut data = b"\x1b]133;B\x07zany@MSI:~$ \r\n".to_vec();
        data.extend(hook("pre", r#"{"e":0,"n":2,"d":"/home/z"}"#));
        b.advance_live(&data);
        // The old pending must be gone: resolving now would blame prompt 2's
        // world for prompt 1's cell.
        let later = std::time::Instant::now() + Duration::from_millis(200);
        assert_eq!(b.poll_pending_prompt_end(later), None);
        assert_eq!(
            b.block_feed.as_ref().unwrap().prompt_end,
            None,
            "no capture may exist between a pre and its own 133;B"
        );
    }

    /// v0.1.1 item 7 (the floating duplicated-prompt-rows band): six rapid
    /// empty-Enter prompt cycles over IDENTICAL bare prompt rows, fed in
    /// the RACE order (OSC before text — the field shape) with scroll in
    /// between. Every superseded prompt row must carry its spacer cover AT
    /// ITS OWN ROW (sig verified at the exact row), exactly once, and the
    /// live prompt row is never covered. Fails without the quiescence
    /// upgrade (spacers then mint at col-0 cells whose sig is empty and
    /// never heal).
    #[test]
    fn six_identical_prompt_rows_covers_stay_glued() {
        let mut b = bhist(80, 4, 100); // 4 rows: the cycles scroll history
        let cycle = |b: &mut TermBackend, n: usize| {
            b.advance_live(&hook(
                "pre",
                &format!(r#"{{"e":0,"n":{n},"d":"/home/z"}}"#),
            ));
            // Reorder: the OSC precedes its prompt text (one feed — the
            // split-feed capture still sees the pre-text cursor).
            b.advance_live(b"\x1b]133;B\x07zany@MSI:~$ ");
            let later = std::time::Instant::now() + Duration::from_millis(100);
            let _ = b.poll_pending_prompt_end(later); // quiescence resolve
        };
        cycle(&mut b, 1);
        for n in 2..=7 {
            b.advance_live(b"\r\n"); // empty Enter: the prompt is superseded
            cycle(&mut b, n);
        }
        let cur = b.term.grid().cursor.point.line.0;
        assert!(b.cursor_at_prompt_end(), "the live prompt is cleanly captured");
        let healthy = b.healthy_covers();
        assert_eq!(healthy.len(), 6, "one spacer per superseded prompt");
        let mut lines: Vec<i32> = healthy.iter().map(|c| c.line).collect();
        lines.sort_unstable();
        lines.dedup();
        assert_eq!(lines.len(), 6, "no two covers on one row");
        assert_eq!(
            lines,
            (cur - 6..cur).collect::<Vec<_>>(),
            "covers glued to the six superseded rows, live row uncovered"
        );
        for c in &healthy {
            assert!(c.line < cur, "the live prompt row is never covered");
            assert_eq!(
                c.sig.as_deref(),
                Some("zany@MSI:~$"),
                "sig = the bare prompt captured at the TRUE prompt end"
            );
            assert_eq!(
                row_text(&b, c.line),
                "zany@MSI:~$",
                "the row under the cover still shows exactly its sig"
            );
        }
    }

    #[test]
    fn anchor_capture_splits_at_hook() {
        let mut b = bhist(80, 24, 100);
        let mut data = b"PS> echo hi\r\n".to_vec();
        data.extend(exec_hook("echo hi"));
        let hook_end = data.len(); // byte after the OSC terminator
        data.extend_from_slice(b"out\r\n");
        b.advance_live(&data); // ONE call: split-feed happens internally
        assert_eq!(anchors(&b).len(), 1);
        assert_eq!(anchors(&b)[0].line, 0, "anchor sits on the command row");
        assert_eq!(
            anchors(&b)[0].start_off,
            hook_end as u64,
            "start_off = stream base + byte after the hook terminator"
        );
        // The pre hook fires before any prompt text: cursor row (2, after
        // "out\r\n") becomes the block's end_line.
        b.advance_live(&hook("pre", r#"{"e":0,"n":1,"d":"C:"}"#));
        assert_eq!(anchors(&b)[0].end_line, Some(2));
    }

    #[test]
    fn anchor_shifts_with_history_and_prunes() {
        let mut b = bhist(80, 5, 100);
        let mut data = b"cmd\r\n".to_vec();
        data.extend(exec_hook("cmd"));
        b.advance_live(&data);
        assert_eq!(anchors(&b)[0].line, 0);
        for i in 0..10 {
            b.advance_live(format!("l{i}\r\n").as_bytes());
        }
        let h = b.term.grid().history_size() as i32;
        assert!(h > 0, "test must scroll rows into history");
        assert_eq!(
            anchors(&b)[0].line,
            -h,
            "anchor shifted by exactly the history delta"
        );
        // Scroll past what the ring can track: the anchor must disappear
        // (never drift) — saturation clears + marks stale.
        for i in 0..120 {
            b.advance_live(format!("m{i}\r\n").as_bytes());
        }
        let f = b.block_feed.as_ref().unwrap();
        assert!(f.anchors.is_empty(), "anchor survived past the ring");
        assert!(f.stale);
    }

    /// r2-M1: the idle shrink truly frees history rows and clears every
    /// history-relative coordinate (drop-don't-drift — the same set ring
    /// saturation clears); idempotent; the backend keeps working at the
    /// reduced cap.
    #[test]
    fn shrink_history_for_idle_frees_and_clears() {
        let mut b = TermBackend::new(GridSize {
            cols: 40,
            rows: 5,
            cell_width: 8.0,
            cell_height: 16.0,
        });
        b.set_stream_pos(0);
        b.enable_block_scan();
        let mut data = b"cmd\r\n".to_vec();
        data.extend(exec_hook("cmd"));
        b.advance_live(&data);
        for i in 0..3000 {
            b.advance_live(format!("l{i}\r\n").as_bytes());
        }
        assert!(b.history_size() > 2_000, "test must exceed the idle ceiling");
        assert!(!b.block_feed.as_ref().unwrap().anchors.is_empty());
        let gen_before = b.feed_gen;
        assert!(b.shrink_history_for_idle(), "first shrink reports work done");
        assert!(b.history_size() <= 2_000, "rows past the ceiling are freed");
        let f = b.block_feed.as_ref().unwrap();
        assert!(f.anchors.is_empty() && f.covers.is_empty());
        assert!(f.prompt_end.is_none());
        assert!(f.stale, "in-grid chrome degrades honestly until the next Reset");
        assert_ne!(b.feed_gen, gen_before, "paint/preview caches re-key");
        assert!(!b.shrink_history_for_idle(), "idempotent");
        // The grid still parses fine at the reduced cap.
        b.advance_live(b"still alive\r\n");
    }

    /// Restored-history hints (D2C::ReplayAnchors): verified hints mint
    /// history covers + block anchors on the replayed rows; a spacer hint
    /// blanks its bare-prompt row; mismatched hints (row no longer shows the
    /// command) vanish without a trace.
    #[test]
    fn replay_hints_mint_covers_anchors_and_drop_mismatches() {
        let mut b = TermBackend::new_with_history(
            GridSize { cols: 40, rows: 10, cell_width: 8.0, cell_height: 16.0 },
            100,
        );
        // The attach Replay (reconstruction path — advance, never scanned).
        b.advance(
            b"PS C:\\> echo hi\r\nhi\r\nPS C:\\>\r\nPS C:\\> dir\r\nf.txt\r\nPS C:\\> ",
        );
        b.set_stream_pos(500);
        b.enable_block_scan();
        b.seed_prompt_end(5, 8); // PromptState: live prompt on row 5
        b.apply_replay_hints(vec![
            ReplayHint {
                start_off: 100,
                row: 0,
                col: 8,
                cmd: Some("echo hi".into()),
                cwd: Some("C:\\".into()),
            },
            ReplayHint { start_off: 150, row: 2, col: 8, cmd: None, cwd: None },
            ReplayHint {
                start_off: 200,
                row: 3,
                col: 8,
                cmd: Some("dir".into()),
                cwd: Some("C:\\".into()),
            },
            // Mismatch: row 4 shows "f.txt", not this command — dropped.
            ReplayHint {
                start_off: 300,
                row: 4,
                col: 8,
                cmd: Some("ghost cmd".into()),
                cwd: None,
            },
        ]);
        let f = b.block_feed.as_ref().unwrap();
        assert_eq!(f.anchors.len(), 2, "two verified block anchors");
        assert_eq!(f.anchors[0].start_off, 100);
        assert_eq!(f.anchors[0].line, 0);
        assert_eq!(
            f.anchors[0].end_line,
            Some(2),
            "block closes at the next verified row (the spacer prompt)"
        );
        assert_eq!(f.anchors[1].start_off, 200);
        assert_eq!(f.anchors[1].line, 3);
        assert_eq!(
            f.anchors[1].end_line,
            Some(5),
            "last block closes at the seeded live prompt row"
        );
        let covers = b.healthy_covers();
        assert_eq!(covers.len(), 3, "2 history + 1 spacer: {covers:?}");
        assert!(covers
            .iter()
            .any(|c| c.line == 0 && c.cmd.as_deref() == Some("echo hi")));
        assert!(covers
            .iter()
            .any(|c| c.line == 2 && c.cmd.is_none() && c.sig.as_deref() == Some("PS C:\\>")));
        assert!(covers
            .iter()
            .any(|c| c.line == 3 && c.cmd.as_deref() == Some("dir")));
        assert!(
            !covers.iter().any(|c| c.line == 4),
            "the mismatched hint minted nothing"
        );
    }

    /// Live Output frames landing between the Replay and the hints frame
    /// shift the grid; hint rows re-base by exactly the history growth. A
    /// resize in that window drops the batch whole (drop, never drift).
    #[test]
    fn replay_hints_rebase_against_interleaved_output() {
        let mut b = TermBackend::new_with_history(
            GridSize { cols: 40, rows: 4, cell_width: 8.0, cell_height: 16.0 },
            100,
        );
        b.advance(b"PS C:\\> echo hi\r\nhi\r\nPS C:\\> ");
        b.set_stream_pos(500);
        b.enable_block_scan();
        // Live output before the hints frame: 3 rows scroll into history.
        b.advance_live(b"x1\r\nx2\r\nx3\r\n");
        let d = b.term.grid().history_size() as i32;
        assert!(d > 0, "output must have scrolled");
        b.apply_replay_hints(vec![ReplayHint {
            start_off: 100,
            row: 0, // replay coordinates: the echo row was row 0 at replay
            col: 8,
            cmd: Some("echo hi".into()),
            cwd: None,
        }]);
        let f = b.block_feed.as_ref().unwrap();
        assert_eq!(f.anchors.len(), 1, "re-based hint verified and minted");
        assert_eq!(f.anchors[0].line, -d, "row re-based by the history growth");

        // Resize between replay and hints ⇒ batch dropped.
        let mut b2 = TermBackend::new_with_history(
            GridSize { cols: 40, rows: 4, cell_width: 8.0, cell_height: 16.0 },
            100,
        );
        b2.advance(b"PS C:\\> echo hi\r\nhi\r\nPS C:\\> ");
        b2.set_stream_pos(500);
        b2.enable_block_scan();
        let changed = b2.resize_to(
            egui::Vec2::new(60.0 * 8.0, 6.0 * 16.0),
            egui::Vec2::new(8.0, 16.0),
        );
        assert!(changed.is_some(), "grid must actually resize");
        b2.apply_replay_hints(vec![ReplayHint {
            start_off: 100,
            row: 0,
            col: 8,
            cmd: Some("echo hi".into()),
            cwd: None,
        }]);
        assert!(
            b2.block_feed.as_ref().unwrap().anchors.is_empty(),
            "hints against a resized grid are dropped whole"
        );
    }

    #[test]
    fn saturation_sets_stale_and_clears() {
        let mut b = bhist(80, 5, 50);
        let mut data = b"cmd\r\n".to_vec();
        data.extend(exec_hook("cmd"));
        b.advance_live(&data);
        assert_eq!(anchors(&b).len(), 1);
        for i in 0..80 {
            b.advance_live(format!("s{i}\r\n").as_bytes());
        }
        let f = b.block_feed.as_ref().unwrap();
        assert!(f.stale, "saturated ring must set stale");
        assert!(f.anchors.is_empty());
    }

    #[test]
    fn alt_screen_freezes_tracking() {
        let mut b = bhist(80, 5, 100);
        b.advance_live(b"a\r\nb\r\ncmd\r\n");
        b.advance_live(&exec_hook("cmd"));
        let line0 = anchors(&b)[0].line;
        assert_eq!(line0, 2);
        // Alt-screen round trip: the primary grid is frozen; reading the alt
        // grid would look like a huge history shrink (phantom prune).
        b.advance_live(b"\x1b[?1049h");
        b.advance_live("junk\r\n".repeat(20).as_bytes());
        b.advance_live(b"\x1b[?1049l");
        {
            let f = b.block_feed.as_ref().unwrap();
            assert!(!f.stale);
            assert_eq!(f.anchors[0].line, line0, "anchor unchanged across alt");
            assert_eq!(
                f.last_history,
                b.term.grid().history_size(),
                "last_history resynced on alt exit"
            );
        }
        // Post-alt output shifts by exactly the real history delta.
        let h0 = b.term.grid().history_size() as i32;
        for i in 0..3 {
            b.advance_live(format!("p{i}\r\n").as_bytes());
        }
        let d = b.term.grid().history_size() as i32 - h0;
        assert!(d > 0);
        assert_eq!(anchors(&b)[0].line, line0 - d);
    }

    #[test]
    fn resize_remap_follows_logical_line() {
        let mut b = bhist(80, 10, 100);
        b.advance_live(b"one\r\ntwo\r\n");
        let cmd = format!("PS> {}", "X".repeat(100)); // wraps at 80 cols
        b.advance_live(cmd.as_bytes());
        b.advance_live(b"\r\n");
        b.advance_live(&exec_hook("X"));
        let line0 = anchors(&b)[0].line;
        assert!(
            row_text(&b, line0).starts_with("PS> XXX"),
            "anchor starts at the logical start of the wrapped command"
        );
        // Shrink so the wrap points move: 104 chars now span 3 rows.
        b.resize_to(egui::vec2(40.0 * 8.0, 10.0 * 16.0), egui::vec2(8.0, 16.0));
        let f = b.block_feed.as_ref().unwrap();
        assert_eq!(f.anchors.len(), 1, "anchor survived reflow");
        let l = f.anchors[0].line;
        assert!(
            row_text(&b, l).starts_with("PS> XXX"),
            "anchor follows its logical line through reflow (row now {:?})",
            row_text(&b, l)
        );
    }

    #[test]
    fn history_shrink_prunes_scrollback_anchors_only() {
        let mut b = bhist(80, 5, 100);
        b.advance_live(b"old\r\n");
        b.advance_live(&exec_hook("old"));
        for i in 0..8 {
            b.advance_live(format!("f{i}\r\n").as_bytes());
        }
        assert!(anchors(&b)[0].line < 0, "first anchor must be in scrollback");
        b.advance_live(b"newcmd\r\n");
        b.advance_live(&exec_hook("newcmd"));
        assert_eq!(anchors(&b).len(), 2);
        let on_screen = anchors(&b)[1].line;
        assert!(on_screen >= 0);
        // ED3 erases scrollback only: history anchors die, screen ones live.
        b.advance_live(b"\x1b[3J");
        let f = b.block_feed.as_ref().unwrap();
        assert_eq!(f.anchors.len(), 1, "scrollback anchor pruned");
        assert_eq!(f.anchors[0].line, on_screen, "screen anchor intact");
        assert!(!f.stale);
        assert_eq!(f.last_history, 0);
    }

    // ── PromptEnd capture (P3) ──────────────────────────────────────

    const PROMPT_END: &[u8] = b"\x1b]133;B\x07";

    #[test]
    fn prompt_end_captured_at_marker() {
        let mut b = bhist(80, 24, 100);
        let mut data = hook("pre", r#"{"e":0,"n":1,"d":"C:"}"#);
        data.extend_from_slice(b"PS C:\\> ");
        data.extend_from_slice(PROMPT_END);
        b.advance_live(&data); // one call: split-feed captures at the marker
        assert_eq!(
            b.block_feed.as_ref().unwrap().prompt_end,
            Some((0, "PS C:\\> ".len())),
            "prompt_end = cursor cell right after the prompt text"
        );
        assert!(b.cursor_at_prompt_end());
        // Typed echo moves the cursor past the prompt end.
        b.advance_live(b"dir");
        assert!(!b.cursor_at_prompt_end());
        // Shell-echo backspaces (BS SP BS) erase it again.
        b.advance_live(b"\x08 \x08\x08 \x08\x08 \x08");
        assert!(b.cursor_at_prompt_end());
    }

    #[test]
    fn prompt_end_shifts_with_history_and_invalidates() {
        let mut b = bhist(80, 5, 200);
        b.advance_live(b"PS> ");
        b.advance_live(PROMPT_END);
        assert_eq!(b.block_feed.as_ref().unwrap().prompt_end, Some((0, 4)));
        for i in 0..8 {
            b.advance_live(format!("\r\nline{i}").as_bytes());
        }
        let h = b.term.grid().history_size() as i32;
        assert!(h > 0, "test must scroll rows into history");
        // SUPERSEDED (restored-render fix): prompt_end used to shift into
        // scrollback (Some((-h, 4))) and survive there. A CURRENT prompt's
        // end in scrollback is definitionally stale — hookless output (a
        // conhost resize repaint, raw Enters) pushed the prompt row away
        // without an exec to retire it, and the stale cell then held
        // `cursor_at_prompt_end()` false forever on idle restored sessions
        // (armed-hint-over-raw-prompt field bug) while pointing the
        // activation preview at history text. It now DROPS the moment it
        // leaves the screen. (Cost: reclaim of a >full-screen wrapped
        // type-ahead line degrades to the discard-chord path — announced by
        // the "clears it" label — an extreme rarity against a defect every
        // restored session could hit.)
        assert_eq!(
            b.block_feed.as_ref().unwrap().prompt_end,
            None,
            "prompt_end dropped once its row scrolled off-screen"
        );
        // ED3 erases scrollback: still no prompt_end.
        b.advance_live(b"\x1b[3J");
        assert_eq!(b.block_feed.as_ref().unwrap().prompt_end, None);
        // Re-capture on screen. A CLEAN prompt now SURVIVES resize —
        // re-derived to the reflowed cursor (cold-attach seed survival, R1) —
        // whereas a DIRTY prompt is still dropped (never remapped).
        b.advance_live(PROMPT_END);
        assert!(b.cursor_at_prompt_end());
        b.resize_to(egui::vec2(40.0 * 8.0, 5.0 * 16.0), egui::vec2(8.0, 16.0));
        let cur = b.term.grid().cursor.point;
        assert_eq!(
            b.block_feed.as_ref().unwrap().prompt_end,
            Some((cur.line.0, cur.column.0)),
            "a clean prompt_end survives resize (re-derived to the reflowed cursor)"
        );
        b.advance_live(b"xy"); // typed input ⇒ dirty
        assert!(!b.cursor_at_prompt_end());
        b.resize_to(egui::vec2(38.0 * 8.0, 5.0 * 16.0), egui::vec2(8.0, 16.0));
        assert_eq!(
            b.block_feed.as_ref().unwrap().prompt_end,
            None,
            "a dirty prompt_end is dropped on resize"
        );
        // Saturation clears it too (drop-don't-drift).
        b.advance_live(PROMPT_END);
        assert!(b.block_feed.as_ref().unwrap().prompt_end.is_some());
        for i in 0..260 {
            b.advance_live(format!("s{i}\r\n").as_bytes());
        }
        let f = b.block_feed.as_ref().unwrap();
        assert!(f.stale);
        assert_eq!(f.prompt_end, None);
    }

    /// Row GROWTH matches conhost, not alacritty (the boot-cover + restore-
    /// truncation fix): content stays top-anchored — scrollback rows are NOT
    /// pulled onto the screen (the conhost repaint that follows the matching
    /// PTY resize would blank them, destroying a restored session's last
    /// screenful — the field "restore truncated my ls" loss) — the cursor
    /// keeps its content-relative row, and a clean prompt_end rides it, so
    /// the composer cover survives the boot corrective resize.
    #[test]
    fn grow_rows_keeps_history_and_prompt_row_conhost_style() {
        let mut b = bhist(80, 10, 200);
        for i in 0..30 {
            b.advance_live(format!("fill{i}\r\n").as_bytes());
        }
        b.advance_live(b"PS> ");
        b.advance_live(PROMPT_END);
        let hist_before = b.history_size();
        assert!(hist_before >= 15, "test needs real scrollback");
        let cur_before = b.term.grid().cursor.point;
        assert!(b.cursor_at_prompt_end());
        // Grow 10 → 24 rows (the boot corrective resize, grow direction).
        b.resize_to(egui::vec2(80.0 * 8.0, 24.0 * 16.0), egui::vec2(8.0, 16.0));
        let cur = b.term.grid().cursor.point;
        assert_eq!(
            cur.line.0, cur_before.line.0,
            "cursor row unchanged — no history pull"
        );
        assert_eq!(b.history_size(), hist_before, "scrollback fully intact");
        assert!(
            b.cursor_at_prompt_end(),
            "clean prompt_end rides the unchanged row (cover survives)"
        );
        assert!(row_text(&b, cur.line.0).starts_with("PS>"));
        for l in (cur.line.0 + 1)..24 {
            assert_eq!(row_text(&b, l), "", "blank rows open BELOW, conhost-style");
        }
        assert_eq!(
            row_text(&b, cur.line.0 - 1),
            "fill29",
            "the previous output still sits directly above the prompt"
        );
    }

    #[test]
    fn pre_exec_counters_bump_even_inside_sync_block() {
        let mut b = bhist(80, 24, 100);
        let mut data = b"\x1b[?2026h".to_vec();
        data.extend(hook("pre", r#"{"e":0,"n":1,"d":"C:"}"#));
        data.extend_from_slice(b"PS> ");
        data.extend_from_slice(PROMPT_END);
        data.extend(exec_hook("echo hi"));
        b.advance_live(&data);
        let f = b.block_feed.as_ref().unwrap();
        assert_eq!(f.pre_seen, 1, "pre counted despite grid deferral");
        assert_eq!(f.exec_seen, 1, "exec counted despite grid deferral");
        assert_eq!(
            f.prompt_end, None,
            "cursor capture skipped while a sync block is pending"
        );
        // ESU applies the block; the NEXT prompt render recaptures normally.
        b.advance_live(b"\x1b[?2026l");
        b.advance_live(PROMPT_END);
        assert!(b.block_feed.as_ref().unwrap().prompt_end.is_some());
    }

    #[test]
    fn row_prefix_matches_equals_the_string_form() {
        // LOW-6: the allocation-free comparator must agree with
        // `row_prefix_text(...) == sig` on every produced-vs-sig shape.
        let mut b = bhist(20, 4, 50);
        b.advance_live(b"PS C:\\>   "); // trailing spaces after the prompt
        let col = 9usize;
        let produced = b.row_prefix_text(0, col);
        // Exact match (sig captured is trim-end'd — no trailing ws).
        assert!(b.row_prefix_matches(0, col, &produced));
        assert!(b.row_prefix_matches(0, col, produced.trim_end()));
        // Interior whitespace preserved.
        assert!(!b.row_prefix_matches(0, col, "PS C:>"));
        // Extra trailing content in the row breaks a shorter sig.
        assert!(!b.row_prefix_matches(0, col, "PS"));
        // Longer sig than the row provides.
        assert!(!b.row_prefix_matches(0, col, "PS C:\\> more"));
        // Empty sig only matches an all-whitespace / empty prefix.
        let mut blank = bhist(20, 4, 50);
        blank.advance_live(b"   ");
        assert!(blank.row_prefix_matches(0, 3, ""));
        assert!(!b.row_prefix_matches(0, col, ""));
        // Cross-check the general shape against the string form directly.
        for c in 0..=12usize {
            let s = b.row_prefix_text(0, c);
            assert_eq!(
                b.row_prefix_matches(0, c, &s),
                b.row_prefix_text(0, c) == s,
                "mismatch at col {c}"
            );
        }
    }

    #[test]
    fn row_has_text_at_matches_cells() {
        let mut b = bhist(20, 4, 50);
        b.advance_live(b"PS> ");
        assert!(
            !b.row_has_text_at(0, 4, "echo hi"),
            "bare prompt row must not match before the echo renders"
        );
        b.advance_live(b"ec");
        assert!(
            !b.row_has_text_at(0, 4, "echo hi"),
            "a partially rendered echo is LESS text than submitted"
        );
        b.advance_live(b"ho hi");
        assert!(b.row_has_text_at(0, 4, "echo hi"));
        assert!(!b.row_has_text_at(0, 4, "echo hx"), "mismatch stays false");
        assert!(!b.row_has_text_at(0, 4, ""), "empty expectation never matches");
        assert!(!b.row_has_text_at(5, 0, "x"), "off-grid row is false");
        assert!(!b.row_has_text_at(0, 99, "x"), "off-grid col is false");
        // Wide chars: lead cell + spacer cell per glyph — the spacer is
        // skipped so the comparison is 1:1 with the source string.
        b.advance_live(b"\r\n$ ");
        b.advance_live("漢字ok".as_bytes());
        assert!(b.row_has_text_at(1, 2, "漢字ok"));
        // Row-edge overflow: everything that fits matches ⇒ true (wrap).
        b.advance_live(b"\r\n");
        b.advance_live(b"cmd ");
        b.advance_live(&[b'x'; 16]); // fills row 2 to the right edge
        assert!(b.row_has_text_at(2, 0, &format!("cmd {}", "x".repeat(30))));
    }

    // ── Presentational covers: spacers + history covers (P3) ─────────

    fn prompt_frame(n: u32) -> Vec<u8> {
        let mut d = hook("pre", &format!(r#"{{"e":0,"n":{n},"d":"C:"}}"#));
        d.extend_from_slice(b"PS C:\\> ");
        d.extend_from_slice(PROMPT_END);
        d
    }

    #[test]
    fn spacer_captured_on_pre_without_exec() {
        let mut b = bhist(40, 6, 100);
        b.advance_live(&prompt_frame(1)); // first prompt, row 0
        assert!(b.healthy_covers().is_empty(), "no spacer at the first prompt");
        // Accept newline + re-prompt with NO command run (empty Enter).
        let mut d = b"\r\n".to_vec();
        d.extend(prompt_frame(2));
        b.advance_live(&d);
        let covers = b.healthy_covers();
        assert_eq!(covers.len(), 1, "the superseded empty prompt is a spacer");
        assert_eq!(covers[0].line, 0);
        assert!(covers[0].cmd.is_none(), "spacer = blank cover");
        // A real command between prompts is NOT a spacer.
        let mut d = b"\r\n".to_vec();
        d.extend(exec_hook("ls"));
        d.extend_from_slice(b"out\r\n");
        d.extend(prompt_frame(3));
        b.advance_live(&d);
        let blanks = b.healthy_covers().iter().filter(|c| c.cmd.is_none()).count();
        assert_eq!(blanks, 1, "a prompt that ran a command is not a spacer");
    }

    #[test]
    fn spacer_self_heals_when_content_appears() {
        let mut b = bhist(40, 6, 100);
        b.advance_live(&prompt_frame(1));
        b.mark_prompt_spacer(); // row 0 is a blank spacer
        assert_eq!(b.healthy_covers().len(), 1);
        // Content written into the input area ⇒ no longer a bare prompt.
        b.advance_live(b"\x1b[1;9Hxyz"); // CUP row1 col9 (1-based) == grid (0,8)
        assert!(
            b.healthy_covers().iter().all(|c| c.line != 0),
            "a spacer with content in its input area self-heals away"
        );
    }

    /// The stale-spacer hardening: a spacer only ever blanks the EXACT bare
    /// prompt it was minted for. A row erased in place (cls / the conhost
    /// resize repaint) and rewritten with SHORT output — text entirely left
    /// of the captured prompt-end col, input area still blank — used to pass
    /// the input-area-only health check, and the spacer painted background
    /// over legitimate output (the empty-rectangle artifact class).
    #[test]
    fn stale_spacer_never_blanks_rewritten_rows() {
        let mut b = bhist(40, 6, 100);
        b.advance_live(&prompt_frame(1)); // bare "PS C:\> " on row 0
        b.mark_prompt_spacer();
        assert_eq!(b.healthy_covers().len(), 1, "fresh spacer is healthy");
        // Erase the row in place and rewrite it with short output ("done."
        // ends well left of col 8; the input area stays blank).
        b.advance_live(b"\x1b[1;1H\x1b[2Kdone.");
        assert!(
            b.healthy_covers().is_empty(),
            "a rewritten row must render raw — stale spacers may not blank it"
        );
    }

    #[test]
    fn history_cover_verifies_command_and_self_heals() {
        let mut b = bhist(40, 6, 100);
        b.advance_live(&prompt_frame(1));
        b.advance_live(b"ls"); // command echoed at the prompt-end col
        b.add_history_cover(0, 8, Some("C:\\".into()), "ls".into());
        let covers = b.healthy_covers();
        assert_eq!(covers.len(), 1);
        assert_eq!(covers[0].cmd.as_deref(), Some("ls"));
        // A redraw that clears the row drops the cover (honest raw row).
        b.advance_live(b"\x1b[1;1H\x1b[2K");
        assert!(
            b.healthy_covers().is_empty(),
            "a cleared row drops its history cover"
        );
    }

    #[test]
    fn covers_shift_with_history_and_saturation_clears() {
        let mut b = bhist(40, 4, 40);
        b.advance_live(&prompt_frame(1));
        b.advance_live(b"ls");
        b.add_history_cover(0, 8, None, "ls".into());
        for i in 0..6 {
            b.advance_live(format!("\r\nout{i}").as_bytes());
        }
        let mid = b.healthy_covers();
        assert_eq!(mid.len(), 1, "cover shifted with content, still verified");
        assert_eq!(mid[0].cmd.as_deref(), Some("ls"));
        assert!(mid[0].line < 0, "the submitted row scrolled into history");
        // Saturate the ring: covers drop (drop-don't-drift, stale).
        for i in 0..60 {
            b.advance_live(format!("\r\ns{i}").as_bytes());
        }
        assert!(b.healthy_covers().is_empty(), "saturation clears covers");
    }

    #[test]
    fn resize_remaps_surviving_spacer() {
        let mut b = bhist(40, 6, 100);
        b.advance_live(&prompt_frame(1));
        b.mark_prompt_spacer();
        assert_eq!(b.healthy_covers().len(), 1);
        b.resize_to(egui::vec2(30.0 * 8.0, 6.0 * 16.0), egui::vec2(8.0, 16.0));
        assert_eq!(
            b.healthy_covers().len(),
            1,
            "a spacer whose row survives reflow intact rides the ordinal remap"
        );
    }

    /// THE resize smoking gun (repro bug 2d): every window resize used to
    /// wipe every history cover and spacer — on a resize-happy box the raw
    /// `PS …>` rows resurfaced minutes after each fix pass. Covers now ride
    /// the same bottom-up logical-line ordinal remap as anchors, verified
    /// against the reflowed grid; only the genuinely un-remappable drop.
    #[test]
    fn covers_remap_across_resize() {
        let mut b = bhist(60, 8, 200);
        // History cover: `PS C:\> ls` echoed at the prompt-end col; the exec
        // hook runs like the real flow (it clears prompt_end, so the next
        // pre mints no phantom spacer over the echo row).
        b.advance_live(&prompt_frame(1));
        b.advance_live(b"ls");
        b.advance_live(&exec_hook("ls"));
        b.add_history_cover(0, 8, Some("C:\\".into()), "ls".into());
        // Scroll it up, mint a spacer at the next prompt.
        let mut d = b"\r\nout1\r\nout2\r\n".to_vec();
        d.extend(prompt_frame(2));
        b.advance_live(&d);
        b.mark_prompt_spacer();
        let before = b.healthy_covers();
        assert_eq!(before.len(), 2, "history cover + spacer before the resize");
        // Cols shrink (still wide enough that neither covered row re-wraps).
        b.resize_to(egui::vec2(40.0 * 8.0, 8.0 * 16.0), egui::vec2(8.0, 16.0));
        let after = b.healthy_covers();
        assert_eq!(after.len(), 2, "both covers survive the reflow");
        let hist = after.iter().find(|c| c.cmd.is_some()).unwrap();
        assert!(
            b.row_has_text_at(hist.line, hist.col, "ls"),
            "remapped history cover still sits on its echo row"
        );
        // A shrink that reflows the covered row's prompt (cols < prompt end)
        // drops the un-remappable cover instead of drifting.
        b.resize_to(egui::vec2(6.0 * 8.0, 8.0 * 16.0), egui::vec2(8.0, 16.0));
        assert!(
            b.healthy_covers().is_empty(),
            "a reflow the verify step can't confirm drops the covers"
        );
    }

    /// F2: the stale-cover prune. A cover the cursor jumps back UP to (cls,
    /// CUP redraw, conhost repaint) is pruned even when the rewritten row
    /// carries an identical bare prompt (the sig heal alone re-matched it
    /// and blanked the LIVE prompt); covers strictly below the cursor are
    /// always pruned; normal downward output never prunes.
    #[test]
    fn stale_covers_pruned_on_cursor_up_move() {
        let mut b = bhist(40, 6, 100);
        b.advance_live(&prompt_frame(1)); // bare "PS C:\> " row 0
        b.mark_prompt_spacer();
        assert_eq!(b.healthy_covers().len(), 1);
        // Downward output: the spacer (above the cursor) survives.
        b.advance_live(b"\r\nout\r\nmore\r\n");
        assert_eq!(b.healthy_covers().len(), 1, "downward motion never prunes");
        // In-place rewrite (conhost repaint / CUP redraw): cursor jumps UP
        // and row 0 is erased + rewritten with an IDENTICAL bare prompt —
        // the sig heal alone would re-match it and blank the LIVE prompt.
        b.advance_live(b"\x1b[1;1H\x1b[2KPS C:\\> ");
        assert!(
            b.healthy_covers().is_empty(),
            "an up-move prunes the coincident-row spacer — the live prompt must render raw"
        );
    }

    #[test]
    fn covers_below_cursor_always_pruned() {
        let mut b = bhist(40, 8, 100);
        b.advance_live(&prompt_frame(1));
        b.advance_live(b"ls");
        b.add_history_cover(0, 8, None, "ls".into());
        b.advance_live(b"\r\nout\r\n"); // cursor row 2, cover row 0: fine
        assert_eq!(b.healthy_covers().len(), 1);
        // Cursor jumps ABOVE the cover row (row 0 < cover's current row?
        // no — jump the cursor up past everything, then write BELOW the
        // cover: any cover at/below the cursor's new row goes).
        b.advance_live(b"\x1b[1;1H"); // CUP row 0 (up-move): prunes ≥ 0
        assert!(
            b.healthy_covers().is_empty(),
            "no cover may survive at/below an upward-jumped cursor"
        );
    }

    // ── Display-stable selection + copy (§6) ─────────────────────────

    /// Drag over-select fix: the pixel→column map divides by the FRACTIONAL
    /// cell width the renderer uses (13 physical px at ppp=1.5 = 8.667pt),
    /// not its integer truncation. The old `usize / usize` mapped the middle
    /// of column 60 to column 65 — the highlight ran past the pointer,
    /// worse the further right the drag ended (user screenshot: claude TUI,
    /// trailing text the pointer never crossed).
    #[test]
    fn selection_point_fractional_cell_width_exact() {
        let cw = 13.0f32 / 1.5; // 8.6667pt, the physical-pixel snap shape
        let mut b = TermBackend::new(GridSize {
            cols: 120,
            rows: 10,
            cell_width: cw,
            cell_height: 28.0 / 1.5,
        });
        // Pointer in the MIDDLE of column 60: must map to 60, not 65.
        let x = (60.0 + 0.5) * cw;
        assert_eq!(b.selection_point(x, 0.0).column.0, 60);
        // Exact drag span: press mid-col 2, release mid-col 60 ⇒ the
        // selection ends exactly at the dragged cell.
        b.advance(b"\x1b[?1049h"); // alt grid (the claude case) — same math
        b.start_selection(SelectionType::Simple, 2.5 * cw, 1.0);
        b.update_selection((60.0 + 0.5) * cw, 1.0);
        let r = b.term.selection.as_ref().unwrap().to_range(&b.term).unwrap();
        assert_eq!((r.start.column.0, r.end.column.0), (2, 60));
    }

    /// selection_side follows the same float rule: left half ⇒ Left, right
    /// half ⇒ Right, at ANY column (the truncated `%` drifted the midpoint
    /// rightward with x).
    #[test]
    fn selection_side_fractional_cell_width() {
        let cw = 13.0f32 / 1.5;
        let b = TermBackend::new(GridSize {
            cols: 120,
            rows: 10,
            cell_width: cw,
            cell_height: 28.0 / 1.5,
        });
        assert_eq!(b.selection_side(100.0 * cw + 0.2 * cw), Side::Left);
        assert_eq!(b.selection_side(100.0 * cw + 0.8 * cw), Side::Right);
    }

    /// Select rows given as (start_line, start_colpx…) via the pixel API.
    fn select_lines(b: &mut TermBackend, l0: i32, l1: i32) {
        // display_offset 0: y = line * cell_h (16), x in cell px (8).
        b.start_selection(SelectionType::Simple, 0.0, l0 as f32 * 16.0);
        b.update_selection(39.0 * 8.0, l1 as f32 * 16.0 + 1.0);
    }

    #[test]
    fn copy_synthesizes_covered_rows() {
        let mut b = bhist(40, 8, 100);
        b.advance_live(&prompt_frame(1)); // row 0: "PS C:\> "
        b.advance_live(b"ls");
        b.add_history_cover(0, 8, Some("C:\\proj".into()), "ls".into());
        b.advance_live(b"\r\nout1\r\nout2");
        // Raw + covered + raw span: the covered line contributes its FULL
        // displayed text (whole-row rule); raw lines stay exact.
        select_lines(&mut b, 0, 2);
        assert_eq!(
            b.selection_text().as_deref(),
            Some("\u{276f} C:\\proj ls\nout1\nout2"),
            "copy == displayed: history cover synthesizes, raw rows exact"
        );
        // Selection entirely inside the covered row ⇒ the full synth line.
        b.start_selection(SelectionType::Simple, 2.0 * 8.0, 0.0);
        b.update_selection(5.0 * 8.0, 1.0);
        assert_eq!(b.selection_text().as_deref(), Some("\u{276f} C:\\proj ls"));
        // Unhealthy cover (row cleared) ⇒ copies raw (matches paint).
        b.advance_live(b"\x1b[1;1H\x1b[2Kraw text");
        // (the up-move prune also killed it — either way: raw)
        select_lines(&mut b, 0, 0);
        assert_eq!(b.selection_text().as_deref(), Some("raw text"));
    }

    #[test]
    fn copy_spacer_and_blanked_prompt_are_empty() {
        let mut b = bhist(40, 8, 100);
        b.advance_live(&prompt_frame(1));
        b.mark_prompt_spacer(); // row 0 spacer
        let mut d = b"\r\n".to_vec();
        d.extend(prompt_frame(2)); // fresh prompt row 1
        b.advance_live(&d);
        b.cur_blank_line = Some(1); // the current-prompt cover blanked row 1
        select_lines(&mut b, 0, 1);
        assert_eq!(
            b.selection_text().as_deref(),
            Some("\n"),
            "spacer and the blanked current-prompt row both copy as empty"
        );
    }

    #[test]
    fn copy_wrap_chain_joins_only_raw_rows() {
        let mut b = bhist(20, 8, 100);
        b.advance_live(b"PS> ");
        let long = "x".repeat(30); // wraps 20-col row 0 into row 1
        b.advance_live(long.as_bytes());
        select_lines(&mut b, 0, 1);
        assert_eq!(
            b.selection_text().as_deref(),
            Some("PS> xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
            "a raw wrap chain joins without a newline"
        );
        // A cover interrupting the chain forces the '\n' boundary.
        let mut b = bhist(20, 8, 100);
        b.advance_live(b"PS> ");
        b.advance_live("y".repeat(30).as_bytes());
        b.cur_blank_line = Some(1); // pretend row 1 is blanked
        select_lines(&mut b, 0, 1);
        assert_eq!(
            b.selection_text().as_deref(),
            Some("PS> yyyyyyyyyyyyyyyy\n"),
            "a covered row never fuses into a wrap chain"
        );
    }

    /// The immortal-selection policy (repro bug 2c): output at the live
    /// bottom clears a selection touching the live region; a scrolled-up
    /// selection (reading history) survives.
    #[test]
    fn live_selection_cleared_by_output_scrolled_kept() {
        let mut b = bhist(40, 4, 100);
        for i in 0..8 {
            b.advance_live(format!("l{i}\r\n").as_bytes());
        }
        // Selection on live rows at the bottom viewport.
        select_lines(&mut b, 1, 2);
        assert!(b.selection_text().is_some());
        b.advance_live(b"more\r\n");
        assert!(
            b.term.selection.is_none(),
            "new output at display_offset==0 clears a live-region selection"
        );
        // Scrolled up: the same gesture survives output.
        select_lines(&mut b, 1, 2);
        b.scroll(2, &mut Vec::new()); // display_offset > 0
        assert!(b.term.grid().display_offset() > 0);
        b.advance_live(b"even more\r\n");
        assert!(
            b.term.selection.is_some(),
            "a scrolled-up selection is never touched by output"
        );
    }

    /// The alt-screen exemption (the copy-out-of-claude fix): a full-screen
    /// TUI repaints IN PLACE — its spinner/stream redraws must NOT clear a
    /// live selection (there is no scrollback motion on the alt grid for the
    /// staircase policy to guard against). Claude keeps painting several
    /// frames a second, so without this every selection died within one
    /// output frame — even mid-drag.
    #[test]
    fn alt_screen_selection_survives_output() {
        let mut b = bhist(40, 8, 100);
        b.advance_live(b"\x1b[?1049h\x1b[2J\x1b[Halt frame text\r\n");
        assert!(b.mode().contains(TermMode::ALT_SCREEN));
        select_lines(&mut b, 0, 0);
        assert!(b.selection_text().is_some());
        // A spinner/status redraw on some OTHER row (claude's steady-state
        // cadence) must not clear it — this was the whole bug.
        b.advance_live(b"\x1b[3;1H\x1b[Kspinner *");
        assert!(
            b.term.selection.is_some(),
            "alt-screen output elsewhere must not clear the selection"
        );
        // Rewriting the SELECTED row is alacritty's own content-aware
        // invalidation (EL intersects the range) — honest, and exactly when
        // the selected text really changed. Not our policy, pinned so a vte
        // upgrade that drops it gets noticed.
        b.advance_live(b"\x1b[1;1H\x1b[Kalt frame text *");
        assert!(
            b.term.selection.is_none(),
            "rewriting the selected row itself invalidates the selection"
        );
        // Leaving the alt screen re-arms the ordinary staircase policy.
        b.advance_live(b"\x1b[?1049l");
        select_lines(&mut b, 0, 0);
        b.advance_live(b"more\r\n");
        assert!(
            b.term.selection.is_none(),
            "primary-screen output keeps the staircase policy"
        );
    }

    /// Bug C geometry pin (R3 — the hide is RENDER-ONLY): alt-screen
    /// entry/exit is not an input to any geometry function. With the hooked
    /// layout unchanged, `resize_to` returns None across a real DECSET 1049
    /// round-trip — no PTY resize may ever ride the strip hide/show (a
    /// resize under alt marks the block feed stale and wipes anchors,
    /// prompt_end and covers: see `pre_resize_ordinals`).
    #[test]
    fn alt_screen_never_resizes_unchanged_layout() {
        let mut b = bhist(40, 8, 100);
        let cell = egui::Vec2::new(8.0, 16.0);
        let layout = egui::Vec2::new(40.0 * 8.0, 8.0 * 16.0);
        assert_eq!(b.resize_to(layout, cell), None, "same grid ⇒ no resize");
        b.advance_live(b"\x1b[?1049h\x1b[2J\x1b[Htui frame");
        assert!(b.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(
            b.resize_to(layout, cell),
            None,
            "alt entry must not produce a PTY resize"
        );
        b.advance_live(b"\x1b[?1049l");
        assert_eq!(
            b.resize_to(layout, cell),
            None,
            "alt exit must not produce a PTY resize"
        );
        assert_eq!((b.size.cols, b.size.rows), (40, 8), "grid untouched");
    }

    /// Restored-render fix: a Replay is a full grid reconstruction — any
    /// selection made against the pre-replay content is meaningless
    /// coordinates and must die with the old world (the immortal "navy
    /// rectangles" over restored scrollback). Reset already rebuilds the
    /// backend today; this pins the invariant at the parse entry itself so
    /// no future Replay consumer can leak a stale range.
    #[test]
    fn replay_clears_stale_selection() {
        let mut b = bhist(40, 6, 100);
        for i in 0..10 {
            b.advance_live(format!("old{i}\r\n").as_bytes());
        }
        select_lines(&mut b, 1, 3);
        assert!(b.selection_text().is_some(), "test needs a live selection");
        // A reconstruction lands (attach/resync replay path = advance()).
        b.advance(b"\x1b[2J\x1b[Hrebuilt world\r\nPS C:\\> ");
        assert!(
            b.term.selection.is_none(),
            "replay must clear any selection made against the old content"
        );
    }

    /// The live staging repro: covers deep in HISTORY must ride a multi-step
    /// window resize storm (cols AND rows changing) - the field failure mode
    /// where every fix pass's covers died minutes later on a resize-happy
    /// box.
    #[test]
    fn covers_survive_resize_storm_with_deep_history() {
        let mut b = bhist(80, 10, 2000);
        for i in 0..100 {
            b.advance_live(format!("fill{i} line of output text\r\n").as_bytes());
        }
        b.advance_live(&prompt_frame(1));
        b.advance_live(b"ls");
        b.advance_live(&exec_hook("ls"));
        let echo_row = b.term.grid().cursor.point.line.0;
        b.add_history_cover(echo_row, 8, Some("C:\\".into()), "ls".into());
        let mut d = b"\r\nout\r\nout2\r\n".to_vec();
        d.extend(prompt_frame(2));
        b.advance_live(&d);
        b.mark_prompt_spacer();
        assert_eq!(b.healthy_covers().len(), 2, "history cover + spacer minted");
        // Three-step storm, cols and rows both moving, then back.
        b.resize_to(egui::vec2(76.0 * 8.0, 9.0 * 16.0), egui::vec2(8.0, 16.0));
        b.resize_to(egui::vec2(72.0 * 8.0, 8.0 * 16.0), egui::vec2(8.0, 16.0));
        b.resize_to(egui::vec2(80.0 * 8.0, 10.0 * 16.0), egui::vec2(8.0, 16.0));
        let covers = b.healthy_covers();
        let hist = covers.iter().find(|c| c.cmd.is_some());
        assert!(
            hist.is_some(),
            "the history cover must survive the storm (got {covers:?})"
        );
        let hist = hist.unwrap();
        assert!(
            b.row_has_text_at(hist.line, hist.col, "ls"),
            "and still sit on its true echo row"
        );
    }

    #[test]
    fn seed_prompt_end_arms_cursor_at_prompt_end() {
        let mut b = bhist(40, 6, 100);
        b.advance_live(b"PS C:\\> "); // cursor at (0,8), but no 133;B yet
        assert!(!b.cursor_at_prompt_end(), "no prompt_end without a 133;B");
        b.seed_prompt_end(0, 8);
        assert!(
            b.cursor_at_prompt_end(),
            "a cold-attach seed arms cursor_at_prompt_end"
        );
    }

    #[test]
    fn cold_attach_fabricates_no_covers() {
        // A fresh cold-attach backend (replayed restored screen + seed, no
        // submits, no live pre-without-exec) must have NO presentational
        // covers — else stale/seeded coordinates could blank legitimate
        // restored content rows (the R1 truncated-scrollback suspicion).
        let mut b = TermBackend::new(GridSize::default());
        b.set_stream_pos(0);
        b.enable_block_scan();
        b.advance(b"Directory\r\ndocs\r\nsrc\r\ntarget\r\nCargo.toml\r\nPS C:\\> ");
        b.seed_prompt_end(5, 8);
        assert!(
            b.healthy_covers().is_empty(),
            "cold attach must not fabricate covers that could blank restored rows"
        );
    }

    /// The click-gated reclaim (`activate` path) recovers raw text echoed at
    /// the prompt exactly and refuses the ambiguous shapes — the coverage
    /// that used to live on the deleted `typed_after_prompt_end` wrapper
    /// (its auto-chord consumer is gone: post-submit typing buffers in the
    /// composer and never reaches the shell).
    #[test]
    fn reclaim_recovers_race_echo() {
        let mut b = bhist(40, 6, 100);
        b.advance_live(&prompt_frame(1)); // prompt_end at (0, 8)
        assert_eq!(b.reclaim_text(), Reclaim::Text(String::new()), "nothing typed yet");
        b.advance_live(b"ls"); // echoed raw keystrokes
        assert_eq!(b.reclaim_text(), Reclaim::Text("ls".into()));
        // Cursor moved to another row (multi-row edit) ⇒ refuse (ambiguous).
        b.advance_live(b"\r\nmore");
        assert_ne!(
            b.reclaim_text(),
            Reclaim::Text("ls".into()),
            "a cursor off the prompt row refuses exact recovery"
        );
    }

    // ── Typeahead reclaim extraction (P4 §2) ─────────────────────────

    /// Backend parked at a live captured prompt end (0-based col 8).
    fn reclaim_backend(cols: u16) -> TermBackend {
        let mut b = bhist(cols, 10, 200);
        b.advance_live(&prompt_frame(1)); // "PS C:\> " + 133;B ⇒ prompt_end (0, 8)
        assert!(b.cursor_at_prompt_end());
        b
    }

    #[test]
    fn reclaim_simple() {
        let mut b = reclaim_backend(80);
        b.advance_live(b"dir");
        assert_eq!(b.reclaim_text(), Reclaim::Text("dir".into()));
    }

    #[test]
    fn reclaim_wrapped() {
        let mut b = reclaim_backend(40);
        let input = "x".repeat(100); // wraps twice at 40 cols (prompt takes 8)
        b.advance_live(input.as_bytes());
        assert!(
            b.term.grid().cursor.point.line.0 > 0,
            "test must actually wrap"
        );
        assert_eq!(b.reclaim_text(), Reclaim::Text(input));
    }

    #[test]
    fn reclaim_wide_chars() {
        let mut b = reclaim_backend(80);
        b.advance_live("漢字".as_bytes()); // wide cells + spacer cells
        assert_eq!(
            b.reclaim_text(),
            Reclaim::Text("漢字".into()),
            "spacer cells must not leak into the reclaimed text"
        );
    }

    #[test]
    fn reclaim_multiline_refused() {
        let mut b = reclaim_backend(80);
        b.advance_live(b"echo 'abc\r\n>> more"); // hard newline in the span
        assert_eq!(b.reclaim_text(), Reclaim::MultiLine);
    }

    #[test]
    fn reclaim_ghost_ignored() {
        let mut b = reclaim_backend(80);
        // Typed "git", PSReadLine prediction ghost " status" in dim+italic,
        // cursor repositioned back to just after "git" (CUB 7).
        b.advance_live(b"git\x1b[97;2;3m status\x1b[0m\x1b[7D");
        assert_eq!(
            b.reclaim_text(),
            Reclaim::Text("git".into()),
            "dim/italic prediction ghost right of the caret is ignored"
        );
        // A custom prediction color WITHOUT dim/italic reads as real text ⇒
        // refuse (never reclaim wrong text).
        let mut b = reclaim_backend(80);
        b.advance_live(b"git\x1b[90m status\x1b[0m\x1b[7D");
        assert_eq!(b.reclaim_text(), Reclaim::CursorMidLine);
    }

    #[test]
    fn reclaim_cursor_midline_refused() {
        let mut b = reclaim_backend(80);
        b.advance_live(b"abcdef\x1b[3D"); // plain cells right of the caret
        assert_eq!(b.reclaim_text(), Reclaim::CursorMidLine);
        // Cursor row wrapping onward (buffer continues below) also refuses.
        let mut b = reclaim_backend(40);
        b.advance_live("y".repeat(60).as_bytes()); // wraps to row 1
        b.advance_live(b"\x1b[1;20H"); // caret back onto the wrapping row 0
        assert_eq!(b.reclaim_text(), Reclaim::CursorMidLine);
    }

    #[test]
    fn reclaim_unavailable_matrix() {
        // No prompt_end capture at all.
        let mut b = bhist(80, 10, 200);
        b.advance_live(b"PS C:\\> dir");
        assert_eq!(b.reclaim_text(), Reclaim::Unavailable);
        // Hookless backend (no feed).
        let b = TermBackend::new(GridSize::default());
        assert_eq!(b.reclaim_text(), Reclaim::Unavailable);
        // Pending DECSET-2026 sync block: the grid lags the stream.
        let mut b = reclaim_backend(80);
        b.advance_live(b"dir");
        b.advance_live(b"\x1b[?2026h");
        assert_eq!(b.reclaim_text(), Reclaim::Unavailable);
        // prompt_end BELOW the cursor (screen rewritten upward).
        let mut b = bhist(80, 10, 200);
        b.advance_live(b"one\r\ntwo\r\n");
        b.advance_live(&prompt_frame(1)); // prompt_end on row 2
        b.advance_live(b"\x1b[1;1H"); // cursor to row 0
        assert_eq!(b.reclaim_text(), Reclaim::Unavailable);
        // Stale feed (saturated ring).
        let mut b = bhist(80, 5, 40);
        b.advance_live(&prompt_frame(1));
        for i in 0..60 {
            b.advance_live(format!("s{i}\r\n").as_bytes());
        }
        assert!(b.block_feed.as_ref().unwrap().stale);
        assert_eq!(b.reclaim_text(), Reclaim::Unavailable);
    }

    #[test]
    fn reclaim_clean_is_empty_text() {
        let b = reclaim_backend(80);
        assert_eq!(b.reclaim_text(), Reclaim::Text(String::new()));
        // Only whitespace typed ⇒ trimmed to empty too.
        let mut b = reclaim_backend(80);
        b.advance_live(b"   ");
        assert_eq!(b.reclaim_text(), Reclaim::Text(String::new()));
    }

    #[test]
    fn pump_sync_flushes_a_stuck_sync_block() {
        // An app that dies mid-sync-block never sends ESU; the 150ms cap
        // (enforced by pump_sync, vte only records the deadline) must flush
        // the buffered bytes so the last frame still becomes visible.
        //
        // Deflaked (r2 roll): vte's deadline is WALL-CLOCK — a loaded
        // machine stalling this thread >150ms between `advance` and the
        // first pump observes the expired phase immediately, which is the
        // feature working, not a failure. Assert phase 1 only when it is
        // observable, and wait for expiry with a deadline-bounded poll
        // instead of one fixed oversleep.
        let mut b = TermBackend::new(GridSize::default());
        b.advance(b"\x1b[?2026hSTUCK");
        assert!(b.preview_lines(1).is_empty(), "sync block buffers its bytes");
        if b.pump_sync().is_some() {
            // Unexpired: bytes still held; poll until the cap fires.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while b.pump_sync().is_some() {
                assert!(std::time::Instant::now() < deadline, "sync block never expired");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(b.pump_sync().is_none(), "expired sync block stays flushed");
        assert_eq!(b.preview_lines(1), vec!["STUCK".to_string()]);
    }

    /// Prompt-render-window tracking (Bug 2, the 1-frame `PS C:\>` submit
    /// flash): the row is pinned at the `pre` scan ONLY with the cursor at
    /// col 0 (a mid-row pre is the ConPTY reorder residual — the OLD prompt
    /// row), survives prompt text rendering along the row, rides scrolls
    /// like covers, and dies on 133;B / exec / cursor leaving the row.
    #[test]
    fn incoming_prompt_row_certainty_gates() {
        let mut b = TermBackend::new(GridSize {
            cols: 40,
            rows: 6,
            cell_width: 8.0,
            cell_height: 16.0,
        });
        b.set_stream_pos(0);
        b.enable_block_scan();
        b.advance_live(b"l0\r\nl1\r\nl2\r\nl3\r\nl4\r\n"); // cursor (5, 0)
        assert_eq!(b.incoming_prompt_row(), None, "no pre yet");

        // pre at col 0 of the bottom row ⇒ pinned there.
        b.advance_live(&hook("pre", r#"{"e":0,"n":1,"d":"C:"}"#));
        assert_eq!(b.incoming_prompt_row(), Some(5));

        // Prompt text renders along the row: still certain (cursor on row).
        b.advance_live(b"PS C:\\> ");
        assert_eq!(b.incoming_prompt_row(), Some(5));

        // 133;B ends the window; prompt_end owns the row now.
        b.advance_live(b"\x1b]133;B\x07");
        assert_eq!(b.incoming_prompt_row(), None);
        assert!(b.has_prompt_end());

        // Reorder residual: a pre scanned with the cursor parked MID-ROW
        // (old prompt end — the newline echo hasn't rendered) never pins.
        b.advance_live(&hook("pre", r#"{"e":0,"n":2,"d":"C:"}"#));
        assert_eq!(
            b.incoming_prompt_row(),
            None,
            "mid-row pre is the OLD prompt row — blanking there is wrong"
        );

        // Fresh cycle at col 0, then a late output tail scrolls the grid:
        // the pinned row shifts up while the cursor stays on the bottom row
        // ⇒ certainty gone, the blank drops (raw is honest).
        b.advance_live(b"\r\n"); // newline echo: cursor (5, 0) again
        b.advance_live(&hook("pre", r#"{"e":0,"n":3,"d":"C:"}"#));
        assert_eq!(b.incoming_prompt_row(), Some(5));
        b.advance_live(b"tail\r\n"); // late tail: scrolls, cursor leaves the pin
        assert_eq!(b.incoming_prompt_row(), None, "cursor left the pinned row");
    }

    // ── QOL: right/middle mouse-report goldens (§3.4) ─────────────────

    #[test]
    fn mouse_report_middle_and_right_button_goldens() {
        let mut b = TermBackend::new(GridSize::default());
        let at = Point::new(Line(0), Column(0));
        let mods = Modifiers::NONE;
        // Legacy (no SGR): press = 32+code, release = 32+3 for ANY button.
        let mut out = Vec::new();
        b.mouse_report(MouseButton::Middle, mods, at, true, &mut out);
        assert_eq!(out, vec![0x1b, b'[', b'M', 32 + 1, 33, 33]);
        out.clear();
        b.mouse_report(MouseButton::Right, mods, at, true, &mut out);
        assert_eq!(out, vec![0x1b, b'[', b'M', 32 + 2, 33, 33]);
        out.clear();
        b.mouse_report(MouseButton::Right, mods, at, false, &mut out);
        assert_eq!(out, vec![0x1b, b'[', b'M', 32 + 3, 33, 33], "legacy release is code 3");
        // SGR (DECSET 1006): '<code;col;row' + M/m press/release.
        b.advance(b"\x1b[?1006h");
        out.clear();
        b.mouse_report(MouseButton::Right, mods, at, true, &mut out);
        assert_eq!(out, b"\x1b[<2;1;1M");
        out.clear();
        b.mouse_report(MouseButton::Right, mods, at, false, &mut out);
        assert_eq!(out, b"\x1b[<2;1;1m", "SGR release keeps the button code");
        out.clear();
        b.mouse_report(MouseButton::Middle, mods, at, true, &mut out);
        assert_eq!(out, b"\x1b[<1;1;1M");
    }

    // ── wheel routing (the "wheel sends arrows into claude" field fix) ──

    /// Decision table: mouse-report wins, then the alt-screen arrows
    /// fallback, else the local viewport — alacritty's (input.rs
    /// `scroll_terminal`) and Windows Terminal's exact precedence. Shift is
    /// the universal local-scroll override on both forwarding branches.
    #[test]
    fn wheel_route_decision_table() {
        use TermMode as M;
        let base = M::default();
        assert!(
            base.contains(M::ALTERNATE_SCROLL),
            "alacritty default has alternate-scroll ON — the arrows branch \
             needs no app opt-in, which is why every alt-screen app hit it"
        );
        let table: &[(TermMode, bool, WheelRoute)] = &[
            // Plain shell, no claims: local scrollback.
            (base, false, WheelRoute::Viewport),
            // Full-screen TUI without mouse mode (htop default, less):
            // arrows — the alt grid has no scrollback to scroll.
            (base | M::ALT_SCREEN, false, WheelRoute::Arrows),
            // Alt-screen app that RESET alternate scroll (?1007l): silence,
            // never arrows.
            (
                (base - M::ALTERNATE_SCROLL) | M::ALT_SCREEN,
                false,
                WheelRoute::Viewport,
            ),
            // The claude shape: alt-screen + ?1003h any-event tracking
            // (+?1006 SGR, irrelevant to routing) — the app claimed the
            // mouse, wheel events go to it, NOT arrows.
            (
                base | M::ALT_SCREEN | M::MOUSE_MOTION | M::SGR_MOUSE,
                false,
                WheelRoute::Report,
            ),
            // Any MOUSE_MODE flavor claims the wheel, alt-screen or not
            // (vim mouse=a is DRAG; click-only apps are REPORT_CLICK).
            (base | M::MOUSE_REPORT_CLICK, false, WheelRoute::Report),
            (base | M::ALT_SCREEN | M::MOUSE_DRAG, false, WheelRoute::Report),
            // Shift overrides BOTH forwarding branches back to local.
            (
                base | M::ALT_SCREEN | M::MOUSE_MOTION | M::SGR_MOUSE,
                true,
                WheelRoute::Viewport,
            ),
            (base | M::ALT_SCREEN, true, WheelRoute::Viewport),
            (base, true, WheelRoute::Viewport),
        ];
        for &(mode, shift, want) in table {
            assert_eq!(
                wheel_route(mode, shift),
                want,
                "mode={mode:?} shift={shift}"
            );
        }
    }

    /// Byte-level goldens through `TermBackend::wheel` with modes set by
    /// real DECSET sequences (not hand-built flags).
    #[test]
    fn wheel_goldens_by_session_shape() {
        let at = Point::new(Line(0), Column(0));
        let none = Modifiers::NONE;
        let shift = Modifiers::SHIFT;

        // claude: ?1049h alt + ?1003h any-event + ?1006h SGR. Wheel up/down
        // = SGR wheel buttons 64/65, press-only, one per line; the local
        // viewport never moves; arrows never appear.
        let mut b = TermBackend::new(GridSize::default());
        b.advance(b"\x1b[?1049h\x1b[?1003h\x1b[?1006h");
        let mut out = Vec::new();
        b.wheel(1, none, at, &mut out);
        assert_eq!(out, b"\x1b[<64;1;1M", "wheel-up = SGR button 64 press");
        out.clear();
        b.wheel(-2, none, at, &mut out);
        assert_eq!(
            out, b"\x1b[<65;1;1M\x1b[<65;1;1M",
            "wheel-down = one button-65 event per line"
        );
        assert_eq!(b.term.grid().display_offset(), 0);
        // Shift+wheel goes local (silent no-op on the historyless alt grid).
        out.clear();
        b.wheel(3, shift, at, &mut out);
        assert!(out.is_empty(), "shift-wheel never reaches a mouse-mode app");

        // Legacy mouse app (?1000h, no SGR): X10-encoded wheel bytes.
        let mut b = TermBackend::new(GridSize::default());
        b.advance(b"\x1b[?1000h");
        let mut out = Vec::new();
        b.wheel(1, none, at, &mut out);
        assert_eq!(
            out,
            vec![0x1b, b'[', b'M', 32 + 64, 33, 33],
            "legacy wheel-up is code 96 at the hovered cell"
        );

        // htop/less shape: alt-screen, NO mouse mode — the arrows fallback
        // stays byte-identical to the old behavior.
        let mut b = TermBackend::new(GridSize::default());
        b.advance(b"\x1b[?1049h");
        let mut out = Vec::new();
        b.wheel(2, none, at, &mut out);
        assert_eq!(out, b"\x1bOA\x1bOA");
        out.clear();
        b.wheel(-1, none, at, &mut out);
        assert_eq!(out, b"\x1bOB");

        // Alt-screen app that reset alternate scroll (?1007l): silence.
        let mut b = TermBackend::new(GridSize::default());
        b.advance(b"\x1b[?1049h\x1b[?1007l");
        let mut out = Vec::new();
        b.wheel(1, none, at, &mut out);
        assert!(out.is_empty(), "?1007l means no arrows");
        assert_eq!(b.term.grid().display_offset(), 0, "alt grid has no history");

        // Plain shell with history: wheel scrolls Pulse's scrollback, the
        // app sees nothing (bytes empty), and wheel-down comes back.
        let mut b = bhist(40, 4, 100);
        for i in 0..10 {
            b.advance_live(format!("line{i}\r\n").as_bytes());
        }
        let mut out = Vec::new();
        b.wheel(2, none, at, &mut out);
        assert!(out.is_empty(), "local scrolling writes no PTY bytes");
        assert_eq!(b.term.grid().display_offset(), 2);
        b.wheel(-1, none, at, &mut out);
        assert!(out.is_empty());
        assert_eq!(b.term.grid().display_offset(), 1);
    }

    // ── QOL: Select all (§3.2) ────────────────────────────────────────

    #[test]
    fn select_all_spans_history_and_synthesizes_covers() {
        let mut b = bhist(40, 4, 100);
        b.advance_live(&prompt_frame(1)); // row 0: "PS C:\> "
        b.advance_live(b"ls");
        b.add_history_cover(0, 8, Some("C:\\proj".into()), "ls".into());
        // Push the covered row into history (4-row grid).
        b.advance_live(b"\r\nAAA\r\nBBB\r\nCCC\r\nDDD");
        assert!(b.term.grid().history_size() > 0, "test needs scrollback");
        b.select_all();
        let text = b.selection_text().expect("select_all built a selection");
        assert!(
            text.contains("\u{276f} C:\\proj ls"),
            "history-cover row synthesizes in the select-all copy: {text:?}"
        );
        assert!(text.contains("AAA") && text.contains("DDD"), "{text:?}");
        assert!(
            text.find("\u{276f}").unwrap() < text.find("AAA").unwrap(),
            "oldest (history) rows come first"
        );
    }

    // ── QOL: view-only clear scrollback (§7.2) — the ED3-style prune ──

    #[test]
    fn clear_scrollback_prune_table() {
        let mut b = bhist(40, 4, 100);
        b.advance_live(&prompt_frame(1));
        b.advance_live(b"ls");
        b.advance_live(&exec_hook("ls"));
        // Scroll the block anchor + a history cover into scrollback, then
        // land a fresh prompt (prompt_end on-screen).
        b.add_history_cover(0, 8, Some("C:\\".into()), "ls".into());
        b.advance_live(b"\r\nout1\r\nout2\r\nout3\r\nout4\r\n");
        b.advance_live(&prompt_frame(2));
        let hist_before = b.term.grid().history_size();
        assert!(hist_before > 0);
        let feed = b.block_feed.as_ref().unwrap();
        assert!(
            feed.anchors.iter().any(|a| a.line < 0) || feed.covers.iter().any(|c| c.line < 0),
            "test needs anchoring state in history"
        );
        let pe_before = feed.prompt_end;
        assert!(pe_before.is_some_and(|(l, _)| l >= 0), "fresh prompt_end is on-screen");
        b.start_selection(SelectionType::Simple, 0.0, 0.0);
        b.update_selection(100.0, 30.0);
        b.jump_flash = Some((0, std::time::Instant::now()));

        b.clear_scrollback_view();

        assert_eq!(b.term.grid().history_size(), 0, "ring emptied");
        assert_eq!(b.term.grid().display_offset(), 0);
        assert!(b.term.selection.is_none(), "selection coordinates died");
        assert!(b.jump_flash.is_none());
        let feed = b.block_feed.as_ref().unwrap();
        assert!(
            feed.anchors.iter().all(|a| a.line >= 0),
            "history anchors pruned, screen anchors live"
        );
        assert!(feed.covers.iter().all(|c| c.line >= 0));
        assert_eq!(feed.prompt_end, pe_before, "on-screen prompt_end survives");
        assert_eq!(feed.last_history, 0, "delta tracker rebased");
        assert!(!feed.stale, "a view clear is not a staleness event");
    }

    // ── QOL: `:` removed from word boundaries (§6.4) ──────────────────

    #[test]
    fn semantic_word_select_spans_colon_paths_and_urls() {
        let mut b = TermBackend::new(GridSize {
            cols: 80,
            rows: 4,
            cell_width: 8.0,
            cell_height: 16.0,
        });
        b.advance(b"PS C:\\> C:\\Users\\alice\\shot.png https://x.dev/a?b=1");
        // Double-click inside the path (col 12): the whole drive path selects.
        b.start_selection(SelectionType::Semantic, 12.0 * 8.0 + 2.0, 2.0);
        assert_eq!(
            b.selection_text().as_deref(),
            Some("C:\\Users\\alice\\shot.png"),
            "the colon no longer splits the drive path"
        );
        // Double-click inside the URL: scheme + host + query select whole.
        b.start_selection(SelectionType::Semantic, 35.0 * 8.0 + 2.0, 2.0);
        assert_eq!(
            b.selection_text().as_deref(),
            Some("https://x.dev/a?b=1"),
            "the colon no longer splits the URL at the scheme"
        );
    }

    // ── QOL: copy-on-select commit edges (§6.2) — the empty-click guard ──

    /// The term_view commit edges copy iff `!selection.is_empty()`. The
    /// load-bearing case: a zero-travel Primary click leaves an EMPTY Simple
    /// selection — never copied (no surprise clipboard clobber on plain
    /// clicks); a drag release and a double-click Semantic press both commit
    /// real text. Pins the predicate the three §6.2 edges gate on.
    #[test]
    fn copy_on_select_commit_edge_walk() {
        let mut b = TermBackend::new(GridSize {
            cols: 40,
            rows: 4,
            cell_width: 8.0,
            cell_height: 16.0,
        });
        b.advance(b"alpha beta");
        // Zero-travel click: the press edge starts a Simple selection that
        // never gets an update — empty, so the release edge must not copy.
        b.start_selection(SelectionType::Simple, 2.0, 2.0);
        assert!(
            b.term.selection.as_ref().is_some_and(|s| s.is_empty()),
            "a plain click's selection is empty"
        );
        assert_eq!(b.selection_text(), None, "empty selection has no copy text");
        // Drag commit: the release edge sees a non-empty selection ⇒ copies.
        b.update_selection(8.0 * 8.0 + 2.0, 2.0);
        assert!(b.term.selection.as_ref().is_some_and(|s| !s.is_empty()));
        let dragged = b.selection_text().expect("dragged selection copies");
        assert!(dragged.starts_with("alpha"), "{dragged:?}");
        // Double/triple-click commit: Semantic/Lines are non-empty at the
        // press edge ⇒ the word/line copies immediately.
        b.start_selection(SelectionType::Semantic, 2.0, 2.0);
        assert!(b.term.selection.as_ref().is_some_and(|s| !s.is_empty()));
        assert_eq!(b.selection_text().as_deref(), Some("alpha"));
    }
}
