//! Grid → VT-stream serialization (the tmux/xterm.js-serialize model).
//!
//! Attach used to replay raw journal bytes. Raw history carries absolute
//! cursor addressing from dead sessions, which is only correct at the grid
//! height it was written for — the source of every restore-seam artifact
//! (blank voids, snapped prompts, overwritten history). Instead, this walks
//! the daemon's authoritative grid and emits it as literal lines: SGR runs,
//! soft-wrap continuations, a relative cursor placement, and the terminal
//! modes. The output reconstructs the same screen at ANY client height.
//!
//! Blank-line runs in scrollback are capped at 2 — restore pads carry no
//! information, so seams collapse to `old output / marker / prompt`.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Flags, LineLength};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

use super::session::EventProxy;

/// Resize a Term with CONHOST's row-growth semantics: on a rows GROW the
/// content stays put and blank rows open BELOW. `Term::resize` bottom-anchors
/// instead — it pulls `min(history, added)` rows out of scrollback onto the
/// screen and drags the cursor down; conhost never pulls, and the repaint it
/// sends after the matching `ResizePseudoConsole` assumes the no-pull layout,
/// so letting the pull stand (a) BLANKS the pulled rows when the repaint
/// arrives (scrollback content destroyed from the grid — the daemon-mirror
/// twin of the GUI's "restore truncated my ls" loss) and (b) leaves the
/// cursor rows below where conhost has it until the repaint heals it. Column
/// changes keep full reflow semantics. Mirrors `TermBackend::grow_rows_conhost`
/// (GUI side) — every Term that shadows a conhost must resize the same way.
pub fn resize_conhost<L: EventListener>(term: &mut Term<L>, cols: usize, rows: usize) {
    let (cols, rows) = (cols.clamp(2, 1000), rows.clamp(2, 1000));
    if rows > term.screen_lines() {
        // Rows-grow as its own step, then the pull is undone: the pulled
        // count is exactly the cursor's downward displacement.
        let cur_cols = term.columns();
        let before = term.grid().cursor.point.line.0;
        term.resize(TermSize::new(cur_cols, rows));
        let pulled = (term.grid().cursor.point.line.0 - before).max(0) as usize;
        if pulled > 0 {
            let grid = term.grid_mut();
            grid.scroll_up::<Color>(&(Line(0)..Line(rows as i32)), pulled);
            grid.cursor.point.line -= pulled;
            grid.saved_cursor.point.line -= pulled;
        }
    }
    term.resize(TermSize::new(cols, rows));
}

/// Longest run of consecutive blank scrollback lines that survives
/// serialization. Real output rarely exceeds one blank separator line;
/// restore pads are dozens.
const MAX_BLANK_RUN: usize = 2;

/// Invisible restore-seam sentinel: written (concealed) into the daemon
/// Term's stream at every restore, never emitted to clients. Lets the
/// serializer erase the seam completely — drop the surrounding pad blanks and
/// dedupe the dead session's dangling prompt against the new session's first
/// one — so a reopen reads as if the process never went away.
pub const SEAM_SENTINEL: &str = "\u{27e6}tc:seam:e5b3\u{27e7}";

/// True when the session is on the alternate screen — the primary grid is
/// inaccessible then, so the caller must fall back to raw-journal replay.
pub fn is_alt_screen(term: &Term<EventProxy>) -> bool {
    term.mode().contains(TermMode::ALT_SCREEN)
}

/// One grid row, pre-rendered.
struct LineRec {
    /// SGR + text bytes, no line ending.
    bytes: Vec<u8>,
    /// Plain trimmed text (for seam dedupe comparisons).
    plain: String,
    blank: bool,
    wrapped: bool,
    in_history: bool,
    /// Erased by seam processing.
    drop: bool,
    seam: bool,
}

/// Old sessions' content, pre-rendered for attach-time prepending. Kept OUT
/// of the live mirror Term: the mirror must contain exactly what conhost has
/// seen and nothing else, or their coordinate systems diverge on resize
/// (alacritty pulls scrollback rows that conhost doesn't have — the
/// "typing echoes rows away from the prompt" bug).
#[derive(Default, Clone)]
pub struct Preface {
    /// Joined lines, every one ending in `SGR-reset CRLF`.
    pub bytes: Vec<u8>,
    /// Byte offset where the final content line starts — truncating here
    /// removes the dead session's dangling prompt when it matches the live
    /// session's first line.
    pub last_line_start: usize,
    /// Plain trimmed text of that final line.
    pub last_line_plain: String,
    /// The final session's opening block (its banner/MOTD: lines printed
    /// before the first prompt-sigil row), recorded as byte ranges into
    /// `bytes` — the live-attach half of the seam banner dedupe:
    /// `serialize_term` splices the block out when the live session's
    /// leading rows reprint the same text. The restore-time render can only
    /// collapse copies that are BOTH already in the journal; the newest
    /// journal copy vs the fresh spawn's re-print meets at attach time.
    pub opening: Vec<OpeningLine>,
}

/// One recorded opening-block line: its byte range in `Preface::bytes` plus
/// the rendered row's comparison keys (matching `LineRec`'s).
#[derive(Clone)]
pub struct OpeningLine {
    pub start: usize,
    pub end: usize,
    pub plain: String,
    pub blank: bool,
    pub wrapped: bool,
}

impl Preface {
    /// Append a daemon-authored informational line (visible, plain gray).
    pub fn push_info_line(&mut self, text: &str) {
        self.last_line_start = self.bytes.len();
        self.last_line_plain = text.trim().to_string();
        self.bytes
            .extend_from_slice(format!("\x1b[0m\x1b[90m{text}\x1b[0m\r\n").as_bytes());
    }
}

/// Render + seam-erase a term's full grid (history + visible screen) into a
/// Preface: trailing blank lines dropped, every kept line CRLF-terminated.
pub fn content_preface(term: &Term<EventProxy>) -> Preface {
    let mut recs = render_lines(term);
    // The dead screen's trailing blanks are noise in a preface.
    while recs.last().is_some_and(|r| r.blank || r.drop) {
        recs.pop();
    }
    // The final session's opening block (post-last-boundary lines before its
    // first prompt-sigil row): recorded as byte ranges so the attach-time
    // serialization can splice it out when the NEW session reprints the same
    // banner/MOTD — this build can't see the future spawn's output, the
    // record lets `serialize_term` finish the dedupe.
    let last_opening = session_openings(&recs).pop().unwrap_or_default();
    let mut opening: Vec<OpeningLine> = Vec::new();
    let mut bytes: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut last_line_start = 0usize;
    let mut last_line_plain = String::new();
    let mut blank_run = 0usize;
    let mut line_start = 0usize;
    for (i, rec) in recs.iter().enumerate() {
        if rec.drop {
            continue;
        }
        if rec.blank {
            blank_run += 1;
            if blank_run > MAX_BLANK_RUN {
                continue;
            }
        } else {
            blank_run = 0;
        }
        line_start = bytes.len();
        bytes.extend_from_slice(&rec.bytes);
        if !rec.wrapped {
            bytes.extend_from_slice(b"\x1b[0m\r\n");
            if !rec.blank {
                last_line_start = line_start;
                last_line_plain = rec.plain.clone();
            }
        }
        if last_opening.binary_search(&i).is_ok() {
            opening.push(OpeningLine {
                start: line_start,
                end: bytes.len(),
                plain: rec.plain.clone(),
                blank: rec.blank,
                wrapped: rec.wrapped,
            });
        }
    }
    let _ = line_start;
    // Never let the opening reach the final content line — that row is the
    // dangling-prompt dedupe's territory (the two cuts must not overlap).
    opening.retain(|l| l.end <= last_line_start);
    Preface {
        bytes,
        last_line_start,
        last_line_plain,
        opening,
    }
}

/// Restore-time preface build + the ALT-CLOSURE JOURNAL FIX (the "sleeping a
/// claude wipes the history" bug). When the tail ends still inside the
/// alternate screen (an alt-screen app was killed — sleep, daemon restart,
/// crash), returns the byte fix the caller must APPEND TO THE JOURNAL before
/// its seam: `?1049l` (closing the otherwise-never-exited alt region — one
/// unexited ENTER makes every later session's bytes parse INTO the alt grid,
/// and the field claude journal held 112 enters with ZERO exits, so every
/// re-parse reconstructed the pre-first-claude primary = an empty preface)
/// followed by the final alt FRAME re-printed as literal primary lines (the
/// conversation the user was looking at survives as real scrollback instead
/// of dying with the alt grid). The returned preface already includes the
/// fix — it is built from the same parse the fix was fed through.
pub fn preface_with_alt_fix(raw: &[u8], cols: u16, rows: u16) -> (Preface, Vec<u8>) {
    if raw.is_empty() {
        return (Preface::default(), Vec::new());
    }
    let (term, fix) = scratch_term_with_fix(raw, cols, rows);
    (content_preface(&term), fix)
}

/// Byte offsets of every restore-seam sentinel in `raw` (the offset of the
/// sentinel text itself). Session boundaries for the scratch/mapping parses:
/// a session that died inside the alternate screen never emitted the EXIT,
/// so the parse must close the region at the seam or every following
/// session's bytes render into the frozen alt grid (content swallowed from
/// every reconstruction). Shared with daemon::anchors — the mapping parse
/// must follow the exact same alt closures to land on the same rows.
pub(super) fn seam_offsets(raw: &[u8]) -> Vec<usize> {
    let needle = SEAM_SENTINEL.as_bytes();
    memchr::memmem::find_iter(raw, needle).collect()
}

/// Close a still-open alt region at a session boundary: the seam bytes that
/// follow (pad + `ESC[H`) were written for the PRIMARY grid. No frame
/// re-print here — historical regions already lost their frames (pre-fix
/// journals), and stacking N dead TUI frames into the preface would be
/// noise; the FINAL region's frame is preserved by `alt_frame_fix`.
/// Shared with daemon::anchors (same closure points ⇒ same rows).
pub(super) fn exit_alt_at_seam<L: EventListener>(
    term: &mut Term<L>,
    parser: &mut super::ImmediateProcessor,
) {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        parser.advance(term, b"\x1b[?1049l");
    }
}

/// The end-of-stream alt closure: exit the alternate screen AND re-print its
/// final frame as literal primary lines — what a real TUI leaves behind is
/// its exit, but this session was KILLED mid-draw, and the frame is the only
/// surviving witness of what the user was looking at (for a claude terminal:
/// the conversation). Returns the exact bytes fed through the parser so the
/// caller can persist them (launch() appends them to the journal — future
/// re-parses then see a closed region + real scrollback, no synthesis).
pub(super) fn alt_frame_fix(
    term: &mut Term<EventProxy>,
    parser: &mut super::ImmediateProcessor,
) -> Vec<u8> {
    // Render the alt grid BEFORE exiting (the exit restores the primary).
    let mut recs = render_lines(term);
    while recs.last().is_some_and(|r| r.blank) {
        recs.pop();
    }
    let mut fix: Vec<u8> = Vec::with_capacity(4096);
    fix.extend_from_slice(b"\x1b[?1049l\x1b[0m\r\n");
    for rec in &recs {
        fix.extend_from_slice(&rec.bytes);
        if !rec.wrapped {
            fix.extend_from_slice(b"\x1b[0m\r\n");
        }
    }
    parser.advance(term, &fix);
    fix
}

/// SLEEP freeze-frame capture: render the live mirror's ALT grid as bytes
/// replayable INSIDE an already-entered alternate screen (the dead-attach arm
/// emits `?1049h` first, then this). Unlike `alt_frame_fix` (which flattens
/// the frame into primary scrollback), this reproduces the frame ON the alt
/// grid with live-TUI semantics — and it must be captured BEFORE the sleep
/// kill: claude's graceful exit handler wipes the alt screen into the journal
/// on console-close, so the mirror between drain and kill is the frame's only
/// witness (see daemon::frame).
///
/// Every non-blank row is positioned ABSOLUTELY (CUP) with auto-wrap off:
/// replayed at the captured size this is exact; a SMALLER attacher clips
/// rows/columns at its edges (never re-flows — the clip-on-resize policy,
/// matching what a live TUI's raw frames would do before a repaint); a larger
/// one leaves background beyond the captured geometry. The cursor is restored
/// last, with its visibility (TUIs usually hide it) and the mirror's wrap
/// mode re-asserted.
pub fn capture_alt_frame(term: &Term<EventProxy>) -> Vec<u8> {
    let recs = render_lines(term);
    let mut out: Vec<u8> = Vec::with_capacity(8 * 1024);
    // The alt grid has no scrollback, so recs are exactly the screen rows.
    out.extend_from_slice(b"\x1b[0m\x1b[?7l");
    for (i, rec) in recs.iter().enumerate() {
        if rec.blank {
            continue; // a fresh alt grid is already blank
        }
        out.extend_from_slice(format!("\x1b[{};1H", i + 1).as_bytes());
        out.extend_from_slice(&rec.bytes);
        out.extend_from_slice(b"\x1b[0m");
    }
    let cur = term.grid().cursor.point;
    out.extend_from_slice(
        format!("\x1b[{};{}H", cur.line.0.max(0) + 1, cur.column.0 + 1).as_bytes(),
    );
    if term.mode().contains(TermMode::LINE_WRAP) {
        out.extend_from_slice(b"\x1b[?7h");
    }
    if !term.mode().contains(TermMode::SHOW_CURSOR) {
        out.extend_from_slice(b"\x1b[?25l");
    }
    out
}

/// Scratch-parse raw journal bytes into a throwaway Term.
///
/// GEOMETRY FIDELITY (the "prompt overwriting a mid-table row" corruption):
/// absolute cursor addressing in the stream is only correct at the grid size
/// it was written for, and one journal tail can span several PTY sizes
/// (attach/boot resize storms). Conhost stamps every resize repaint with an
/// XTWINOPS window-size report `ESC[8;<rows>;<cols>t`, so the scratch Term
/// follows the tail's own reports: it STARTS at the first report's size when
/// one exists (bytes before it were written at that size or earlier) and
/// resizes at each subsequent report. Re-parsing the whole tail at one fixed
/// smaller size CLAMPED every row below it — piling table rows onto the
/// bottom row and writing the next session's prompt over their left half.
/// The field screenshot (`PS C:\…>   12:43 AM   1445 Cargo.toml` fused on one
/// row) was manufactured exactly this way at restore-preface build time; the
/// bytes in the journal were internally consistent.
fn scratch_term(raw: &[u8], cols: u16, rows: u16) -> Term<EventProxy> {
    scratch_term_with_fix(raw, cols, rows).0
}

/// The scratch parse plus the end-of-stream alt-closure bytes (empty when
/// the tail already ends on the primary screen). See `preface_with_alt_fix`.
fn scratch_term_with_fix(raw: &[u8], cols: u16, rows: u16) -> (Term<EventProxy>, Vec<u8>) {
    let (mut scratch, mut parser) = scratch_parse(raw, cols, rows);
    // A tail that ENDS inside the alternate screen (session died mid-TUI —
    // sleep/reboot/daemon kill while claude drew) leaves the ACTIVE grid
    // holding the TUI's dead frame and hides the primary grid + all
    // scrollback. A real terminal leaving a TUI restores the primary screen;
    // do exactly that — and re-print the killed frame as literal lines so
    // the last thing the user saw survives as scrollback (the fix bytes are
    // returned for the journal so future parses see real closed bytes).
    let fix = if is_alt_screen(&scratch) {
        alt_frame_fix(&mut scratch, &mut parser)
    } else {
        Vec::new()
    };
    (scratch, fix)
}

/// The shared scratch parse: follows the stream's own geometry reports
/// (`scratch_segments`), the alt-cut trim, and the seam alt-closures —
/// WITHOUT the end-of-stream alt closure. A tail that ends inside the
/// alternate screen leaves the returned Term ON the alt grid; callers
/// decide what that means (`scratch_term_with_fix` flattens the killed
/// frame into primary scrollback for dead sessions; `serialize_live_alt`
/// captures it as a live frame overlay).
fn scratch_parse(raw: &[u8], cols: u16, rows: u16) -> (Term<EventProxy>, super::ImmediateProcessor) {
    use alacritty_terminal::term;
    // ALT-SCREEN CUT SAFETY (the "claude fragments fused with prompts"
    // artifact): a tail whose first alt marker is an EXIT was cut INSIDE an
    // alt-screen region — the bytes before that exit are TUI frame traffic
    // whose ENTER lies before the cut. Parsed at primary they paint TUI
    // fragments over the primary grid, the orphan exit is then a no-op, and
    // the next session's prompts render interleaved with the remnants.
    // Start after the orphan exit instead.
    let (alt_start, _) = alt_cut_scan(raw);
    let raw = &raw[alt_start..];
    let effective = scratch_segments(raw, cols, rows);
    // Session boundaries: close a dead-in-alt region at its seam or every
    // later session parses into the frozen alt grid (the "sleeping claude
    // wipes history" mechanism — see preface_with_alt_fix).
    let seams = seam_offsets(raw);
    let mut si = 0usize;
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut scratch = Term::new(
        term::Config {
            scrolling_history: 2000,
            ..term::Config::default()
        },
        &TermSize::new(effective[0].2, effective[0].3),
        super::session::EventProxy::new(tx),
    );
    // ImmediateProcessor: a journal tail can end inside a DECSET 2026 sync
    // block (the app never got to its ESU before dying); the default parser
    // would hold those bytes in its deferral buffer and the tail would
    // silently be lost. The daemon never defers.
    let mut parser = super::ImmediateProcessor::new();
    for &(start, end, c, r) in &effective {
        resize_conhost(&mut scratch, c, r);
        let mut pos = start;
        while pos < end {
            while si < seams.len() && seams[si] <= pos {
                si += 1;
            }
            let target = match seams.get(si) {
                Some(&s) if s < end => s,
                _ => end,
            };
            parser.advance(&mut scratch, &raw[pos..target]);
            pos = target;
            if seams.get(si) == Some(&pos) {
                exit_alt_at_seam(&mut scratch, &mut parser);
                si += 1;
            }
        }
    }
    (scratch, parser)
}

/// Geometry segment table for a raw journal tail: (start, end, cols, rows).
/// The stream's own XTWINOPS reports segment it — bytes before the first
/// report parse at that report's size (the closest known geometry), each
/// report re-sizes from its offset on, no reports at all ⇒ the caller's
/// size. Rows are WIDENED to the deepest absolutely-addressed row in each
/// segment: a resize-storm race can stamp a repaint with a STALE size while
/// the content that follows addresses the true (taller) viewport
/// (field-observed — epoch-19's `[8;42;160t` repaint followed by CUPs to
/// row 46). Growing reconstructs the writer's layout and costs only
/// trailing blank rows (trimmed/capped downstream); clamping fuses rows.
/// Shared by `scratch_term` and the anchor-mapping parse (daemon::anchors),
/// which must follow the exact same geometry to land on the same rows.
/// The caller has already applied the alt-cut trim (`alt_cut_scan`).
pub(super) fn scratch_segments(
    raw: &[u8],
    cols: u16,
    rows: u16,
) -> Vec<(usize, usize, usize, usize)> {
    let reports = winsz_reports(raw);
    let first = reports
        .first()
        .map(|&(_, r, c)| (c, r))
        .unwrap_or((cols.clamp(2, 1000) as usize, rows.clamp(2, 1000) as usize));
    let mut segs: Vec<(usize, usize, usize)> = vec![(0, first.0, first.1)];
    for &(off, r, c) in &reports {
        if off == 0 {
            segs[0] = (0, c, r);
        } else {
            segs.push((off, c, r));
        }
    }
    segs.iter()
        .enumerate()
        .map(|(i, &(start, c, r))| {
            let end = segs.get(i + 1).map(|s| s.0).unwrap_or(raw.len());
            let deep = max_addressed_row(&raw[start..end]);
            (start, end, c, r.max(deep).clamp(2, 1000))
        })
        .collect()
}

/// Alt-screen ENTER/EXIT markers (DECSET/DECRST 47, 1047, 1049) in stream
/// order: (offset past the sequence, is_enter). Multi-parameter DECSET lines
/// (`CSI ? 1049;2004 h`) count when ANY parameter is an alt-screen mode.
fn alt_markers(raw: &[u8]) -> Vec<(usize, bool)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < raw.len() {
        let Some(p) = memchr::memchr(0x1b, &raw[i..]) else { break };
        let at = i + p;
        let rest = &raw[at..];
        i = at + 1;
        if rest.len() < 5 || rest[1] != b'[' || rest[2] != b'?' {
            continue;
        }
        let mut j = 3usize;
        while j < rest.len() && (rest[j].is_ascii_digit() || rest[j] == b';') {
            j += 1;
        }
        if j == 3 || j >= rest.len() || j > 32 {
            continue;
        }
        let fin = rest[j];
        if fin != b'h' && fin != b'l' {
            continue;
        }
        let params = &rest[3..j];
        let is_alt = params
            .split(|&b| b == b';')
            .filter_map(|p| std::str::from_utf8(p).ok()?.parse::<u32>().ok())
            .any(|n| matches!(n, 47 | 1047 | 1049));
        if is_alt {
            out.push((at + j + 1, fin == b'h'));
        }
    }
    out
}

/// Where a raw journal-tail parse should START, plus whether the bytes from
/// there leave the stream inside the alternate screen. A journal tail is a
/// byte SUFFIX: the cut can land inside an alt-screen region, and parsing
/// those bytes at the primary grid manufactures TUI-fragment corruption.
/// - `start`: past a leading orphan EXIT (a first marker that is an exit ⇒
///   everything before it belongs to an alt screen entered before the cut).
/// - `Some(true)` ⇒ an ENTER after `start` has no matching EXIT (tail ends
///   inside the alt screen); `Some(false)` ⇒ ends on the primary screen;
///   `None` ⇒ no markers survive (undecidable from the tail alone).
pub fn alt_cut_scan(raw: &[u8]) -> (usize, Option<bool>) {
    let ms = alt_markers(raw);
    let (start, rest) = match ms.split_first() {
        Some((&(end, false), rest)) => (end, rest),
        _ => (0usize, &ms[..]),
    };
    (start, rest.last().map(|&(_, enter)| enter))
}

/// Attach/resync replay for a session LIVE on the alternate screen — the
/// WIDTH-MISMATCH GARBLE FIX. This replaced `alt_tail_for_live`'s raw-tail
/// replay: raw journal bytes are width-honest only at the geometry they
/// were recorded at — parsed into a client grid of a different width,
/// wide rows wrap early (shifting every following row) while the TUI's
/// absolute CUPs clamp onto the wrong rows and overwrite mid-row: old and
/// new text interleave (the restored-claude field garble, healed only by a
/// manual resize forcing a full TUI repaint).
///
/// Instead: scratch-parse the tail at its own recorded geometry
/// (`scratch_segments` follows the stream's XTWINOPS reports) and emit
/// - `serialize_term` of the PRIMARY grid (scrollback underlay —
///   reconstructs at ANY client size; also re-asserts the live modes), then
/// - `\x1b[?1049h` + `capture_alt_frame` of the alt grid (the sleep
///   freeze-frame renderer: absolute rows, clip semantics at a smaller
///   attacher, never re-flows) — the live TUI's next repaint refreshes it.
///
/// When the tail's parse does NOT end inside the alt screen (journal/mirror
/// divergence — the caller's mirror is the authority that the session is on
/// the alt screen NOW), the underlay is still emitted and the alt grid is
/// entered blank; the TUI's next repaint fills it.
pub fn serialize_live_alt(raw: &[u8], cols: u16, rows: u16) -> Vec<u8> {
    let (mut scratch, mut parser) = scratch_parse(raw, cols, rows);
    let frame = if is_alt_screen(&scratch) {
        // Capture the frame BEFORE exiting (the exit restores the primary
        // grid); the exit gives serialize_term the primary + scrollback.
        let f = capture_alt_frame(&scratch);
        parser.advance(&mut scratch, b"\x1b[?1049l");
        f
    } else {
        Vec::new()
    };
    let mut out = serialize_term(&scratch, None);
    out.extend_from_slice(b"\x1b[?1049h");
    out.extend_from_slice(&frame);
    out
}

/// The deepest 1-based row absolutely addressed in `seg` by CUP (`CSI r;cH`
/// / `CSI r;cf`) or VPA (`CSI rd`). memchr-gated; garbage-length params are
/// ignored. 0 = nothing addressed.
fn max_addressed_row(seg: &[u8]) -> usize {
    let mut max = 0usize;
    let mut i = 0usize;
    while let Some(p) = memchr::memchr(0x1b, &seg[i..]) {
        let esc = i + p;
        i = esc + 1;
        let rest = &seg[esc..];
        if rest.len() < 4 || rest[1] != b'[' || !rest[2].is_ascii_digit() {
            continue;
        }
        let mut k = 2usize;
        let mut row = 0usize;
        while k < rest.len() && rest[k].is_ascii_digit() && k < 6 {
            row = row * 10 + (rest[k] - b'0') as usize;
            k += 1;
        }
        if k >= rest.len() || row == 0 || row > 1000 {
            continue;
        }
        // The final byte decides: `d` right here (VPA), or `H`/`f` either
        // right here (row-only CUP) or after a `;cols` second param.
        let fin = match rest[k] {
            b'H' | b'f' | b'd' => Some(rest[k]),
            b';' => {
                let mut j = k + 1;
                while j < rest.len() && rest[j].is_ascii_digit() && j < k + 7 {
                    j += 1;
                }
                (j < rest.len() && matches!(rest[j], b'H' | b'f')).then(|| rest[j])
            }
            _ => None,
        };
        if fin.is_some() {
            max = max.max(row);
        }
    }
    max
}

/// Every `ESC[8;<rows>;<cols>t` in `raw`: (byte offset of the ESC, rows,
/// cols), values clamped to the sane grid range. memchr-gated — plain text
/// pays one SIMD skip per ESC.
fn winsz_reports(raw: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(p) = memchr::memchr(0x1b, &raw[i..]) {
        let esc = i + p;
        i = esc + 1;
        let rest = &raw[esc..];
        if rest.len() < 6 || rest[1] != b'[' || rest[2] != b'8' || rest[3] != b';' {
            continue;
        }
        let mut k = 4usize;
        let mut rows = 0usize;
        while k < rest.len() && rest[k].is_ascii_digit() && k < 10 {
            rows = rows * 10 + (rest[k] - b'0') as usize;
            k += 1;
        }
        if k >= rest.len() || rest[k] != b';' {
            continue;
        }
        k += 1;
        let mut cols = 0usize;
        let col_start = k;
        while k < rest.len() && rest[k].is_ascii_digit() && k < col_start + 6 {
            cols = cols * 10 + (rest[k] - b'0') as usize;
            k += 1;
        }
        if k >= rest.len() || rest[k] != b't' || rows == 0 || cols == 0 {
            continue;
        }
        out.push((esc, rows.clamp(2, 1000), cols.clamp(2, 1000)));
        i = esc + k + 1;
    }
    out
}

/// Attach replay for a DEAD terminal: scratch-parse the raw journal tail at
/// the attacher's grid and serialize the result, exactly like a live mirror.
///
/// Perf-wave-3: shipping the raw ≤2MB tail per dead attach made a daemon
/// restart with an attached GUI a ~35MB replay storm (20 not-yet-restored
/// terminals × 2MB), all of it discarded seconds later when each restore's
/// Reset+resync replaced the world. The serialized form is ~10× smaller,
/// visually identical (same render/seam-erasure rules the restore preface
/// uses on the same bytes), and free of the dead session's absolute cursor
/// addressing at foreign sizes. A tail that ends inside the alternate
/// screen (session died mid-TUI) now reconstructs the RESTORED PRIMARY grid
/// — `scratch_term` exits the alt screen after the parse, exactly like the
/// TUI exiting would (render-bugs pass, the claude-fragment fusion class) —
/// so `None` (raw-tail fallback) is a belt that should never engage.
pub fn serialize_dead(raw: &[u8], cols: u16, rows: u16) -> Option<Vec<u8>> {
    if raw.is_empty() {
        return Some(Vec::new());
    }
    let scratch = scratch_term(raw, cols, rows);
    if is_alt_screen(&scratch) {
        return None;
    }
    Some(serialize_term(&scratch, None))
}

/// How many content lines past a seam (or into the live grid) the
/// dangling-prompt dedupe searches for the new session's first prompt.
/// Restored cmd/WSL sessions print a banner/MOTD before their first prompt,
/// so "the first content line" never matched and the dead prompt survived
/// forever (the restored-render-fix field bug). 40 covers every banner seen
/// in the field (Windows copyright = 2 lines, Ubuntu MOTD ≈ 10-25) with
/// margin; the prompt text itself is specific enough that a wider window
/// costs nothing.
const DEDUPE_WINDOW: usize = 40;

/// The dangling-prompt comparison: the dead session's trailing line `prev`
/// is redundant against a later line `next` when they are identical, OR when
/// `next` begins with `prev` at a prompt boundary. Exact equality alone
/// never fired in the field: by re-parse time the user had typed at the new
/// session's first prompt (`PS C:\>` vs `PS C:\> ls`) or a conhost resize
/// repaint had doubled it (`PS C:\> PS C:\>`) — journal-evidenced, both.
/// The boundary guard keeps "Compiling x" from merging into "Compiling xyz":
/// the continuation must start with a space (typed-command boundary) or
/// `prev` must end in a shell prompt terminator (cmd renders `C:\dir>` with
/// no trailing space; bash `$`/`#`, PS `>`).
fn dangling_prompt_match(prev: &str, next: &str) -> bool {
    if prev.is_empty() {
        return false;
    }
    if prev == next {
        return true;
    }
    match next.strip_prefix(prev) {
        Some(rest) => rest.starts_with(' ') || prev.ends_with(['>', '$', '#', '%', ':']),
        None => false,
    }
}

/// Seam-adjacent banner dedupe (the "15 stacked `Microsoft Windows
/// [Version …]` banners after restarts" fix). cmd prints its banner on every
/// REAL spawn (no off switch exists) and ssh sessions re-print their MOTD
/// per login — so every restore appends one more identical copy to the
/// journal, and reconstructions showed them all stacked at the seams. The
/// pwsh bootstrap used to reproduce the `-Command`-suppressed logo per spawn
/// too; since the respawn-banner fix it only does so on the FIRST-EVER spawn
/// (`pwsh_banner_for_spawn`), but this pass stays load-bearing for pwsh as
/// well: journals written before that fix carry one banner copy per
/// lifetime, and they must keep collapsing to one on every future restore.
///
/// Rule: a session's OPENING BLOCK — the lines it printed after its boundary
/// (a restore seam, or the tail start) BEFORE its first prompt-sigil line —
/// is redundant when the NEXT session's opening reprints the same text; the
/// EARLIER copy drops (keep-newest: the newest copy can sit on the visible
/// screen where drops are forbidden; older copies are history).
///
/// The sigil stop is the safety invariant: a line containing '>', '$' or '#'
/// (every prompt shape across the shell families, bare or typed-on) is never
/// part of an opening, so this can never eat a prompt row or a typed command
/// — only pre-first-prompt spawn output (banner/MOTD class) can match, and
/// only when the following session reprints it line-for-line from its own
/// boundary. The live-attach half lives in `serialize_term` (the
/// `Preface::opening` splice); the two must agree — anchors' mapping parse
/// applies THIS rule across the same boundaries (via `render_lines` /
/// `emitted_grid_rows`) and a one-sided drop shifts every computed hint row
/// above the banner (same contract as the dangling-prompt dedupe).
fn opening_sigil_stop(plain: &str) -> bool {
    plain.contains(['>', '$', '#'])
}

/// Per session boundary (tail start + each seam), the indices of the
/// boundary's opening block: surviving recs in order, stopping at the next
/// seam, the first sigil line, or DEDUPE_WINDOW rows.
fn session_openings(recs: &[LineRec]) -> Vec<Vec<usize>> {
    let mut bounds: Vec<usize> = vec![0];
    for (i, r) in recs.iter().enumerate() {
        if r.seam {
            bounds.push(i + 1);
        }
    }
    bounds
        .into_iter()
        .map(|b| {
            let mut out = Vec::new();
            for (i, r) in recs.iter().enumerate().skip(b) {
                if r.seam || out.len() >= DEDUPE_WINDOW {
                    break;
                }
                if r.drop {
                    continue;
                }
                if opening_sigil_stop(&r.plain) {
                    break;
                }
                out.push(i);
            }
            out
        })
        .collect()
}

/// Pass 1+2: render every grid line and mark seams + drops.
fn render_lines(term: &Term<EventProxy>) -> Vec<LineRec> {
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines() as i32;
    let history = grid.history_size() as i32;

    // Pass 1: render every line.
    let mut recs: Vec<LineRec> = Vec::with_capacity((history + rows) as usize);
    for line in -history..rows {
        let row = &grid[Line(line)];
        let len = row.line_length().0.min(cols);
        let mut bytes: Vec<u8> = Vec::new();
        let mut plain = String::new();
        let mut attrs = AttrState::default();
        for col in 0..len {
            let cell = &row[Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            attrs.apply(cell.fg, cell.bg, cell.flags, &mut bytes);
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
            plain.push(cell.c);
            if let Some(extra) = cell.zerowidth() {
                for z in extra {
                    bytes.extend_from_slice(z.encode_utf8(&mut buf).as_bytes());
                    plain.push(*z);
                }
            }
        }
        let plain = plain.trim().to_string();
        let wrapped =
            len == cols && cols > 0 && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        // Legacy visible markers from older builds are seam text too — erasing
        // them retroactively cleans scrollback written before the sentinel
        // existed. (The crash-loop warning is NOT matched; that one is
        // information.)
        let legacy_marker = plain.starts_with("── ")
            && plain.ends_with(" ──")
            && (plain.contains("restored") || plain.contains("process exited"));
        recs.push(LineRec {
            bytes,
            seam: plain == SEAM_SENTINEL || legacy_marker,
            blank: len == 0,
            wrapped,
            in_history: line < 0,
            drop: false,
            plain,
        });
    }

    // Pass 2: erase seams. For each sentinel: drop it and every adjacent
    // blank (restore padding), and if the last surviving content line BEFORE
    // it is textually identical to the first content line AFTER it (the dead
    // session's dangling prompt vs. the new session's fresh one), drop the
    // stale copy. Only history lines are ever dropped — visible rows anchor
    // the cursor/coordinate math.
    for i in 0..recs.len() {
        if !recs[i].seam {
            continue;
        }
        recs[i].drop = true;
        let mut lo = i;
        while lo > 0 && recs[lo - 1].blank && recs[lo - 1].in_history && !recs[lo - 1].drop {
            lo -= 1;
            recs[lo].drop = true;
        }
        let mut hi = i;
        while hi + 1 < recs.len() && recs[hi + 1].blank && recs[hi + 1].in_history {
            hi += 1;
            recs[hi].drop = true;
        }
        let prev = (0..lo).rev().find(|&k| !recs[k].drop && !recs[k].blank);
        if let Some(p) = prev {
            if recs[p].in_history && !recs[p].wrapped && !recs[p].plain.is_empty() {
                // Search a WINDOW of content lines past the seam, not just the
                // first: restored cmd/WSL sessions print a banner/MOTD before
                // their first prompt, and the first prompt row itself may
                // carry typed text or a repaint double by re-parse time —
                // exact first-line equality never engaged in the field (the
                // "dead sessions' final bare PS> rows survive every restore"
                // bug). Candidates may be wrapped (a long first command wraps
                // the prompt row); prefix matching is row-text based either
                // way.
                let hit = (hi + 1..recs.len())
                    .filter(|&k| !recs[k].drop && !recs[k].blank && !recs[k].seam)
                    .take(DEDUPE_WINDOW)
                    .any(|k| dangling_prompt_match(&recs[p].plain, &recs[k].plain));
                if hit {
                    recs[p].drop = true;
                }
            }
        }
    }

    // Pass 3: seam-adjacent banner dedupe (see `opening_sigil_stop`). For
    // each pair of consecutive session boundaries, pairwise-match their
    // opening blocks (blank/wrap flags + trimmed text); when at least one
    // non-blank line matched, drop the EARLIER opening's matched lines up to
    // the last non-blank match. Only history rows ever drop — visible rows
    // anchor the cursor/coordinate math.
    let openings = session_openings(&recs);
    for w in openings.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let mut last_nonblank: Option<usize> = None;
        for k in 0..a.len().min(b.len()) {
            let (ra, rb) = (&recs[a[k]], &recs[b[k]]);
            if ra.blank != rb.blank || ra.wrapped != rb.wrapped || ra.plain != rb.plain {
                break;
            }
            if !ra.blank {
                last_nonblank = Some(k);
            }
        }
        if let Some(last) = last_nonblank {
            for &i in &a[..=last] {
                if recs[i].in_history {
                    recs[i].drop = true;
                }
            }
        }
    }

    // Pass 3.5: seam-TRAILING banner orphans. Conhost resize repaints can
    // OVERWRITE an earlier seam's sentinel row and re-render the banner
    // mid-region (field cmd journal: two `[8;…t` repaints erased the first
    // two seams, leaving one orphan banner render fused into the first
    // region) — pass 3 never sees a boundary there. The orphan still ends
    // up directly ABOVE a surviving seam whose next session reprints the
    // same block, so: for each seam, when the last surviving pre-seam lines
    // EQUAL the post-seam opening (same order, same blank pattern, whole
    // block), the pre-seam copy drops. Equality against a sigil-free
    // opening keeps the safety invariant (a prompt/command row can never
    // match), and `ver`-style near-banners fail the whole-block equality.
    let seam_positions: Vec<usize> = recs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.seam)
        .map(|(i, _)| i)
        .collect();
    for (k, &si) in seam_positions.iter().enumerate() {
        // openings[0] is the tail start; openings[k+1] follows seam k.
        // Compared UNTRIMMED (interior and trailing blanks included) — the
        // pre-seam copy of the block carries the same blank pattern, and a
        // stricter shape means fewer chances to eat real output.
        let o = &openings[k + 1][..];
        if !o.iter().any(|&i| !recs[i].blank) {
            continue;
        }
        // The last |o| surviving recs strictly before the seam.
        let mut before: Vec<usize> = Vec::with_capacity(o.len());
        let mut i = si;
        while before.len() < o.len() && i > 0 {
            i -= 1;
            if recs[i].drop || recs[i].seam {
                continue;
            }
            before.push(i);
        }
        if before.len() < o.len() {
            continue;
        }
        before.reverse();
        let all_eq = before.iter().zip(o.iter()).all(|(&b, &a)| {
            recs[b].blank == recs[a].blank
                && recs[b].wrapped == recs[a].wrapped
                && recs[b].plain == recs[a].plain
        });
        if all_eq && before.iter().all(|&b| recs[b].in_history) {
            for &b in &before {
                recs[b].drop = true;
            }
        }
    }
    recs
}

/// The serializer's per-row emit decision over rendered lines: seam-dropped
/// rows are out (`rec.drop`), HISTORY blank runs are capped at MAX_BLANK_RUN,
/// and the history blank run immediately preceding the final content line
/// (the prompt the eye rests on) is capped at ONE row — a restored seam's pad
/// plus PS 5.1's own pre-prompt blank otherwise stack a 2-3 row void directly
/// above the prompt (pipeline audit §3 top-gap accounting). SCREEN blanks are
/// grid truth and always survive — dropping them would shift the relative
/// cursor placement. Extracted from `serialize_term` so daemon::anchors can
/// compute, for any grid row, exactly which replay line it becomes (the two
/// must never drift apart — a hint mapped through a different rule set lands
/// on the wrong row).
fn emit_mask(recs: &[LineRec]) -> Vec<bool> {
    let mut mask = vec![false; recs.len()];
    let kept: Vec<usize> = (0..recs.len()).filter(|&i| !recs[i].drop).collect();
    let pre_prompt_skip: Option<(usize, usize)> = kept
        .iter()
        .rposition(|&i| !recs[i].blank)
        .and_then(|last| {
            let mut i = last;
            while i > 0 && recs[kept[i - 1]].blank && recs[kept[i - 1]].in_history {
                i -= 1;
            }
            // Keep the first blank of the run (i); skip (i+1 .. last).
            (last - i > 1).then_some((i + 1, last))
        });
    let mut blank_run = 0usize;
    for (pos, &i) in kept.iter().enumerate() {
        let rec = &recs[i];
        if pre_prompt_skip.is_some_and(|(a, b)| pos >= a && pos < b && rec.in_history && rec.blank)
        {
            continue;
        }
        if rec.in_history && rec.blank {
            blank_run += 1;
            if blank_run > MAX_BLANK_RUN {
                continue;
            }
        } else {
            blank_run = 0;
        }
        mask[i] = true;
    }
    mask
}

/// Grid rows (Line index space: -history .. screen_lines) that
/// `serialize_term` emits as literal lines, in order, each with its
/// blankness — the exact grid-row → replay-line mapping. daemon::anchors
/// uses it to COMPUTE a checkpoint's replay row (anchored content-to-content
/// on the last non-blank line) instead of searching for it by text.
pub(super) fn emitted_grid_rows(term: &Term<EventProxy>) -> Vec<(i32, bool)> {
    let recs = render_lines(term);
    let mask = emit_mask(&recs);
    let history = term.grid().history_size() as i32;
    (0..recs.len())
        .filter(|&i| mask[i])
        .map(|i| (i as i32 - history, recs[i].blank))
        .collect()
}

/// Serialize the live mirror term, prepending `preface` (older sessions'
/// content). The dead session's dangling prompt at the preface tail is
/// truncated when it textually equals the live session's first content line.
pub fn serialize_term(term: &Term<EventProxy>, preface: Option<&Preface>) -> Vec<u8> {
    let grid = term.grid();
    let rows = grid.screen_lines() as i32;
    let recs = render_lines(term);

    let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
    out.extend_from_slice(b"\x1b[0m");

    if let Some(pre) = preface {
        // Same windowed dangling-prompt rule as the seam pass in
        // render_lines: the live session's banner (cmd/WSL restores) and any
        // text the user already typed at its first prompt defeated exact
        // first-line equality, so the preface's dead trailing prompt stacked
        // above the fresh one on every attach.
        let hit = !pre.last_line_plain.is_empty()
            && recs
                .iter()
                .filter(|r| !r.drop && !r.blank)
                .take(DEDUPE_WINDOW)
                .any(|r| dangling_prompt_match(&pre.last_line_plain, &r.plain));
        let cut = if hit { pre.last_line_start } else { pre.bytes.len() };
        // Banner dedupe, live-attach half (`opening_sigil_stop` has the
        // rule): the preface's final-session opening block is spliced out
        // when the live session's leading rows reprint the same text —
        // cmd/pwsh banners, WSL/ssh MOTDs. Pass 3 in render_lines can only
        // collapse copies that are both in the journal; the newest journal
        // copy vs the live mirror's fresh print meets HERE, and the anchors
        // mapping parse applies pass 3 across this same boundary, so the two
        // decisions agree (a one-sided keep would shift every computed hint
        // row above the banner).
        let splice: Option<(usize, usize)> = {
            let mut last_hit: Option<usize> = None;
            for (k, ol) in pre.opening.iter().enumerate() {
                match recs.get(k) {
                    Some(r)
                        if !r.drop
                            && r.blank == ol.blank
                            && r.wrapped == ol.wrapped
                            && r.plain == ol.plain =>
                    {
                        if !r.blank {
                            last_hit = Some(k);
                        }
                    }
                    _ => break,
                }
            }
            last_hit.and_then(|k| {
                let (s, e) = (pre.opening[0].start, pre.opening[k].end);
                (e <= cut).then_some((s, e))
            })
        };
        match splice {
            Some((s, e)) => {
                out.extend_from_slice(&pre.bytes[..s]);
                out.extend_from_slice(&pre.bytes[e..cut]);
            }
            None => out.extend_from_slice(&pre.bytes[..cut]),
        }
    }

    // Join the live lines, capping genuine blank runs in scrollback.
    let mask = emit_mask(&recs);
    let last_emit = mask.iter().rposition(|&e| e);
    for (i, rec) in recs.iter().enumerate() {
        if !mask[i] {
            continue;
        }
        out.extend_from_slice(&rec.bytes);
        let last = Some(i) == last_emit;
        if !rec.wrapped && !last {
            out.extend_from_slice(b"\x1b[0m\r\n");
        }
    }
    out.extend_from_slice(b"\x1b[0m");

    // Cursor: relative from the bottom visible row — height-independent.
    let cur = grid.cursor.point;
    let up = (rows - 1 - cur.line.0).max(0);
    out.extend_from_slice(b"\r");
    if up > 0 {
        out.extend_from_slice(format!("\x1b[{up}A").as_bytes());
    }
    if cur.column.0 > 0 {
        out.extend_from_slice(format!("\x1b[{}C", cur.column.0).as_bytes());
    }

    // Modes (only non-defaults need re-asserting).
    let mode = *term.mode();
    let mut m = |s: &str| out.extend_from_slice(s.as_bytes());
    if mode.contains(TermMode::APP_CURSOR) {
        m("\x1b[?1h");
    }
    if mode.contains(TermMode::APP_KEYPAD) {
        m("\x1b=");
    }
    if mode.contains(TermMode::BRACKETED_PASTE) {
        m("\x1b[?2004h");
    }
    if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
        m("\x1b[?1000h");
    }
    if mode.contains(TermMode::MOUSE_DRAG) {
        m("\x1b[?1002h");
    }
    if mode.contains(TermMode::MOUSE_MOTION) {
        m("\x1b[?1003h");
    }
    if mode.contains(TermMode::SGR_MOUSE) {
        m("\x1b[?1006h");
    }
    if mode.contains(TermMode::UTF8_MOUSE) {
        m("\x1b[?1005h");
    }
    if mode.contains(TermMode::ALTERNATE_SCROLL) {
        m("\x1b[?1007h");
    }
    if mode.contains(TermMode::FOCUS_IN_OUT) {
        m("\x1b[?1004h");
    }
    if mode.contains(TermMode::ORIGIN) {
        m("\x1b[?6h");
    }
    if mode.contains(TermMode::INSERT) {
        m("\x1b[4h");
    }
    if !mode.contains(TermMode::LINE_WRAP) {
        m("\x1b[?7l");
    }
    if !mode.contains(TermMode::SHOW_CURSOR) {
        m("\x1b[?25l");
    }

    out
}

/// Tracks the emitted SGR state; on any difference emits a full reset + set.
/// (Reset-and-set is a few bytes more than minimal diffing but immune to
/// attribute-interaction bugs.)
#[derive(PartialEq, Clone, Copy)]
struct AttrState {
    fg: Option<Color>,
    bg: Option<Color>,
    flags: Flags,
}

impl Default for AttrState {
    fn default() -> Self {
        Self {
            fg: None,
            bg: None,
            flags: Flags::empty(),
        }
    }
}

impl AttrState {
    fn apply(&mut self, fg: Color, bg: Color, cell_flags: Flags, out: &mut Vec<u8>) {
        let style = cell_flags
            & (Flags::BOLD
                | Flags::DIM
                | Flags::ITALIC
                | Flags::UNDERLINE
                | Flags::DOUBLE_UNDERLINE
                | Flags::INVERSE
                | Flags::HIDDEN
                | Flags::STRIKEOUT);
        let next = AttrState {
            fg: Some(fg),
            bg: Some(bg),
            flags: style,
        };
        if *self == next {
            return;
        }
        *self = next;

        let mut seq = String::from("\x1b[0");
        if style.contains(Flags::BOLD) {
            seq.push_str(";1");
        }
        if style.contains(Flags::DIM) {
            seq.push_str(";2");
        }
        if style.contains(Flags::ITALIC) {
            seq.push_str(";3");
        }
        if style.intersects(Flags::UNDERLINE | Flags::DOUBLE_UNDERLINE) {
            seq.push_str(";4");
        }
        if style.contains(Flags::INVERSE) {
            seq.push_str(";7");
        }
        if style.contains(Flags::HIDDEN) {
            seq.push_str(";8");
        }
        if style.contains(Flags::STRIKEOUT) {
            seq.push_str(";9");
        }
        push_color(&mut seq, fg, false);
        push_color(&mut seq, bg, true);
        seq.push('m');
        out.extend_from_slice(seq.as_bytes());
    }
}

fn push_color(seq: &mut String, color: Color, is_bg: bool) {
    let base = if is_bg { 40 } else { 30 };
    let bright = if is_bg { 100 } else { 90 };
    match color {
        Color::Named(named) => {
            let code: i32 = match named {
                NamedColor::Black => base,
                NamedColor::Red => base + 1,
                NamedColor::Green => base + 2,
                NamedColor::Yellow => base + 3,
                NamedColor::Blue => base + 4,
                NamedColor::Magenta => base + 5,
                NamedColor::Cyan => base + 6,
                NamedColor::White => base + 7,
                NamedColor::BrightBlack => bright,
                NamedColor::BrightRed => bright + 1,
                NamedColor::BrightGreen => bright + 2,
                NamedColor::BrightYellow => bright + 3,
                NamedColor::BrightBlue => bright + 4,
                NamedColor::BrightMagenta => bright + 5,
                NamedColor::BrightCyan => bright + 6,
                NamedColor::BrightWhite => bright + 7,
                // Foreground/Background and the dim variants: default.
                _ => {
                    if is_bg {
                        49
                    } else {
                        39
                    }
                }
            };
            // Default colors need no code (SGR 0 already reset them).
            if code != 39 && code != 49 {
                seq.push_str(&format!(";{code}"));
            }
        }
        Color::Indexed(i) => {
            seq.push_str(&format!(";{};5;{}", if is_bg { 48 } else { 38 }, i));
        }
        Color::Spec(rgb) => {
            seq.push_str(&format!(
                ";{};2;{};{};{}",
                if is_bg { 48 } else { 38 },
                rgb.r,
                rgb.g,
                rgb.b
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dead-terminal attach reconstruction (perf-wave-3): a primary-screen
    /// journal tail serializes compactly (content preserved, seam sentinel
    /// erased — the same render rules the restore preface applies to the
    /// same bytes), while a tail that dies inside the alternate screen
    /// refuses so the caller falls back to the raw tail.
    #[test]
    fn serialize_dead_reconstructs_primary_and_refuses_alt() {
        let mut raw = Vec::new();
        raw.extend_from_slice(b"hello world\r\n");
        raw.extend_from_slice(format!("\x1b[8m{SEAM_SENTINEL}\x1b[28m\r\n").as_bytes());
        raw.extend_from_slice(b"PS C:\\> ");
        let out = serialize_dead(&raw, 80, 24).expect("primary-screen tail must serialize");
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.contains("hello world"), "content lost: {text:?}");
        assert!(text.contains("PS C:\\>"), "prompt lost: {text:?}");
        assert!(
            !text.contains(SEAM_SENTINEL),
            "seam sentinel must be erased from the reconstruction"
        );

        // A tail that dies inside the alternate screen reconstructs the
        // RESTORED PRIMARY grid *plus the killed frame re-printed as literal
        // scrollback lines* (the "sleeping claude wipes the history" fix —
        // supersedes the render-bugs contract that dropped the frame): the
        // frame is the only witness of what the user was looking at, and it
        // lands as ordinary lines AFTER the pre-alt primary content, never
        // as raw absolute-addressed TUI traffic.
        let alt = b"before\x1b[?1049h\x1b[HTUI SCREEN".to_vec();
        let out = serialize_dead(&alt, 80, 24).expect("alt tail reconstructs primary");
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.contains("before"), "primary content restored: {text:?}");
        let b = text.find("before").unwrap();
        let f = text
            .find("TUI SCREEN")
            .expect("the killed frame is preserved as scrollback");
        assert!(f > b, "frame lines land after the primary content: {text:?}");

        assert_eq!(serialize_dead(&[], 80, 24), Some(Vec::new()));
    }

    /// The alt-closure journal fix (Bug 1, "sleeping claude wipes history"):
    /// (a) a tail ending inside the alt screen yields fix bytes = `?1049l` +
    /// the frame as literal lines, and the preface already contains them;
    /// (b) a MID-TAIL unexited alt region (the field claude journal held 112
    /// enters and ZERO exits) is closed at its seam so later sessions'
    /// content survives into the preface; (c) re-parsing tail+fix yields no
    /// further fix (idempotent — the journaled bytes close the region).
    #[test]
    fn alt_closure_fix_preserves_frame_and_later_sessions() {
        // (a) end-in-alt: frame preserved, fix returned.
        let mut raw = b"primary line\r\n".to_vec();
        raw.extend_from_slice(b"\x1b[?1049h\x1b[HCONVERSATION FRAME");
        let (preface, fix) = preface_with_alt_fix(&raw, 80, 24);
        assert!(!fix.is_empty(), "end-in-alt must produce the journal fix");
        assert!(fix.starts_with(b"\x1b[?1049l"), "fix closes the region first");
        let text = String::from_utf8_lossy(&preface.bytes).into_owned();
        assert!(text.contains("primary line"), "{text:?}");
        assert!(
            text.contains("CONVERSATION FRAME"),
            "the killed frame survives into the preface: {text:?}"
        );

        // (c) idempotence: the fix appended = a closed region; no new fix.
        let mut fixed = raw.clone();
        fixed.extend_from_slice(&fix);
        let (p2, fix2) = preface_with_alt_fix(&fixed, 80, 24);
        assert!(fix2.is_empty(), "journaled fix closes the region for good");
        let t2 = String::from_utf8_lossy(&p2.bytes).into_owned();
        assert_eq!(
            t2.matches("CONVERSATION FRAME").count(),
            1,
            "no double frame: {t2:?}"
        );

        // (b) mid-tail unexited region + seam + a later session: the later
        // session's content must survive (the old parse painted it into the
        // frozen alt grid and the preface reconstructed pre-alt emptiness).
        let mut journal = b"session one\r\n".to_vec();
        journal.extend_from_slice(b"\x1b[?1049h\x1b[HDEAD TUI ONE");
        // The REAL seam shape launch() writes: sentinel + rows×CRLF pad +
        // home (the pad scrolls old content into history before the fresh
        // session's absolute addressing reuses the viewport).
        journal.extend_from_slice(format!("\r\n\x1b[8m{SEAM_SENTINEL}\x1b[28m").as_bytes());
        journal.extend_from_slice(&b"\r\n".repeat(24));
        journal.extend_from_slice(b"\x1b[H");
        journal.extend_from_slice(b"session two prompt\r\n");
        let (p3, _) = preface_with_alt_fix(&journal, 80, 24);
        let t3 = String::from_utf8_lossy(&p3.bytes).into_owned();
        assert!(
            t3.contains("session two prompt"),
            "later session swallowed by the unexited alt region: {t3:?}"
        );
        assert!(t3.contains("session one"), "{t3:?}");
    }

    /// SLEEP freeze-frame: `capture_alt_frame` replayed after `?1049h`
    /// reproduces the alt grid at the captured size (rows on their absolute
    /// lines, cursor + visibility restored) and CLIPS at a smaller attacher —
    /// no re-flow, no wrap, no panic (the clip-on-resize policy).
    #[test]
    fn capture_alt_frame_roundtrip_and_clip() {
        use alacritty_terminal::term;
        let mut raw = b"PS C:\\> claude\r\n".to_vec();
        raw.extend_from_slice(b"\x1b[?1049h\x1b[?25l");
        raw.extend_from_slice(b"\x1b[3;5H\x1b[1mHELLO CONVERSATION\x1b[0m");
        // A row wider than the small client below (col-clip fixture).
        raw.extend_from_slice(
            format!("\x1b[5;1H{}", "W".repeat(60)).as_bytes(),
        );
        raw.extend_from_slice(b"\x1b[24;1H> input box");
        // A live-mirror stand-in: raw parse, NO alt closure (scratch_term
        // would exit the alt screen — that is its job, not this test's).
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut mirror = Term::new(
            term::Config::default(),
            &TermSize::new(80, 24),
            super::super::session::EventProxy::new(tx),
        );
        let mut parser = super::super::ImmediateProcessor::new();
        parser.advance(&mut mirror, &raw);
        assert!(is_alt_screen(&mirror), "fixture must end on the alt screen");
        let frame = capture_alt_frame(&mirror);
        assert!(
            frame.ends_with(b"\x1b[?25l"),
            "hidden cursor must be re-asserted"
        );

        // Same-size replay over a serialized underlay.
        let mut replay = b"underlay scrollback row\r\n\x1b[?1049h".to_vec();
        replay.extend_from_slice(&frame);
        let (screen, _, _, _) = client_grid(&replay, 80, 24);
        assert!(
            screen[2].contains("HELLO CONVERSATION"),
            "row 3 content: {screen:?}"
        );
        assert!(screen[23].contains("> input box"), "bottom row: {screen:?}");

        // Smaller attacher: rows/cols clip in place — the deep row clamps to
        // the bottom, the wide row truncates at the right edge instead of
        // wrapping onto the next row.
        let (small, _, _, _) = client_grid(&replay, 40, 10);
        assert!(
            small[2].contains("HELLO CONVERS"),
            "top rows keep their lines: {small:?}"
        );
        assert!(
            small[4].starts_with(&"W".repeat(40)) && small[4].len() <= 40,
            "wide row clips at the edge: {:?}",
            small[4]
        );
        assert!(
            !small[5].contains('W'),
            "auto-wrap must be off during the frame (no reflow): {:?}",
            small[5]
        );
        assert!(
            small[9].contains("> input box"),
            "deep rows clamp to the bottom row: {small:?}"
        );
    }

    /// Alt-screen cut safety (Bug 4, the "claude fragments fused with shell
    /// prompts" artifact): the marker scan and the orphan-exit skip.
    #[test]
    fn alt_cut_scan_and_live_tail() {
        // No markers: undecidable; start at 0.
        assert_eq!(alt_cut_scan(b"plain shell output\r\n"), (0, None));
        // Balanced enter/exit: primary at end.
        let balanced = b"a\x1b[?1049hFRAME\x1b[?1049lb".to_vec();
        assert_eq!(alt_cut_scan(&balanced), (0, Some(false)));
        // Ends inside alt.
        let inside = b"a\x1b[?1049hFRAME".to_vec();
        assert_eq!(alt_cut_scan(&inside), (0, Some(true)));
        // Cut inside an alt region: first marker is the orphan EXIT — the
        // parse starts after it.
        let orphan = b"TUI JUNK\x1b[?1049lreal shell".to_vec();
        let (start, state) = alt_cut_scan(&orphan);
        assert_eq!(&orphan[start..], b"real shell");
        assert_eq!(state, None);
        // Multi-parameter DECSET counts; 2004 alone does not.
        assert_eq!(alt_cut_scan(b"\x1b[?1049;2004hX").1, Some(true));
        assert_eq!(alt_cut_scan(b"\x1b[?2004hX"), (0, None));
        // 47 / 1047 variants count.
        assert_eq!(alt_cut_scan(b"\x1b[?47hX").1, Some(true));
        assert!(alt_cut_scan(b"\x1b[?1047lX").0 > 0);
    }

    /// THE WIDTH-MISMATCH GARBLE FIX (the 2026-07-09 restored-claude field
    /// bug): a live alt-screen session recorded at 175 cols, replayed to a
    /// client attaching at 147, must stay readable. The old raw-tail replay
    /// (`alt_tail_for_live`) shipped bytes that are width-honest only at
    /// their recorded geometry: at 147, the 175-col row wraps early and its
    /// spill fuses with the next absolutely-addressed row (reproduced below
    /// as the A/B control — the exact field mechanism, `scratchpad\
    /// garble-evidence\repro-attach-state.txt`). `serialize_live_alt`
    /// scratch-parses the tail at its own XTWINOPS-reported geometry and
    /// re-emits it as a frozen alt frame over a serialized primary
    /// underlay — width-correct by construction at BOTH widths.
    #[test]
    fn serialize_live_alt_is_width_honest() {
        // The field journal's shape, reduced: one XTWINOPS report pins the
        // recorded geometry at 175×49; primary scrollback; alt enter; a TUI
        // frame with a full-width row directly above an absolutely-addressed
        // row (the fusion pair), plus a CUP to a column beyond 147.
        let mut raw = Vec::new();
        raw.extend_from_slice(b"\x1b[8;49;175t");
        raw.extend_from_slice(b"PS C:\\quipshot-hub> claude\r\nHISTLINE ONE\r\n");
        raw.extend_from_slice(b"\x1b[?1049h\x1b[?25l");
        raw.extend_from_slice(format!("\x1b[5;1H{}", "A".repeat(170)).as_bytes());
        raw.extend_from_slice(b"\x1b[6;30HWMROW_SIX_TEXT");
        raw.extend_from_slice(b"\x1b[7;160HDEEPCOL");

        // A/B control — the OLD path (raw tail, enter survives ⇒ shipped
        // verbatim) parsed at the mismatched width manufactures the fusion:
        // the wrapped spill of row 5 shares row 6 with WMROW_SIX_TEXT.
        let (screen, _, _, _) = client_grid(&raw, 147, 49);
        let fused = &screen[5];
        assert!(
            fused.contains('A') && fused.contains("WMROW_SIX_TEXT"),
            "control failed to reproduce the field fusion (raw tail at 147): {fused:?}"
        );

        // The fix, at the RECORDED width: frame rows land exactly where the
        // TUI drew them, nothing fused.
        let out = serialize_live_alt(&raw, 175, 49);
        let (s175, _, _, _) = client_grid(&out, 175, 49);
        assert_eq!(s175[4], "A".repeat(170), "recorded-width row intact");
        assert!(
            s175[5].trim_start().starts_with("WMROW_SIX_TEXT") && !s175[5].contains('A'),
            "row 6 must hold only its own text at 175: {:?}",
            s175[5]
        );
        assert!(s175[6].contains("DEEPCOL"), "deep-column row at 175: {:?}", s175[6]);

        // The fix, at the FOREIGN width (the field's 147): the wide row
        // CLIPS at the edge instead of wrapping (capture_alt_frame's ?7l),
        // so row 6 is readable and free of row-5 spill; the >147 CUP text
        // clips away rather than landing mid-row.
        let (s147, _, _, _) = client_grid(&out, 147, 49);
        assert!(
            s147[4].starts_with(&"A".repeat(147)) && s147[4].len() <= 147,
            "wide row clips at the client edge: {:?}",
            s147[4]
        );
        assert!(
            s147[5].trim_start().starts_with("WMROW_SIX_TEXT") && !s147[5].contains('A'),
            "the field fusion is gone at 147: {:?}",
            s147[5]
        );

        // Both replays end INSIDE the alt screen (live-TUI semantics) and
        // the primary underlay beneath carries the pre-alt history.
        let mut replay_term = {
            use alacritty_terminal::term;
            let (tx, _rx) = std::sync::mpsc::channel();
            let mut t = Term::new(
                term::Config::default(),
                &TermSize::new(147, 49),
                super::super::session::EventProxy::new(tx),
            );
            let mut p = super::super::ImmediateProcessor::new();
            p.advance(&mut t, &out);
            t
        };
        assert!(
            replay_term.mode().contains(TermMode::ALT_SCREEN),
            "replay must leave the client on the alt grid"
        );
        {
            let mut p = super::super::ImmediateProcessor::new();
            p.advance(&mut replay_term, b"\x1b[?1049l");
        }
        let grid = replay_term.grid();
        let hist = grid.history_size() as i32;
        let all: String = (-hist..49)
            .map(|l| {
                let row = &grid[Line(l)];
                let len = row.line_length().0.min(147);
                (0..len).map(|c| row[Column(c)].c).collect::<String>() + "\n"
            })
            .collect();
        assert!(
            all.contains("HISTLINE ONE"),
            "primary underlay must carry the pre-alt scrollback: {all:?}"
        );

        // Divergence belt: a tail whose parse ends PRIMARY still enters a
        // blank alt grid (the mirror said alt) over the serialized underlay.
        let primary_tail = b"\x1b[8;24;80tPS> done\r\nPS> ".to_vec();
        let out2 = serialize_live_alt(&primary_tail, 80, 24);
        assert!(out2.ends_with(b"\x1b[?1049h"), "blank alt entry appended");
        let (s2, _, _, _) = client_grid(&out2, 80, 24);
        assert!(
            s2.iter().all(|r| !r.contains("PS> done")),
            "alt grid starts blank: {s2:?}"
        );
    }

    /// The fused-region regression, end to end at the scratch-parse level: a
    /// journal tail cut INSIDE a completed alt region (TUI frame bytes, the
    /// orphan exit, then the shell's next prompts) must reconstruct ONLY the
    /// shell content — the old parse painted the frame bytes onto the
    /// primary grid where the exit could not undo them.
    #[test]
    fn cut_inside_alt_region_never_fuses_tui_fragments() {
        let mut raw = Vec::new();
        // TUI frame traffic whose ENTER lies before the cut: absolute CUPs
        // + box-drawing rows (claude-shaped).
        raw.extend_from_slice(b"\x1b[3;1H\xe2\x95\xad\xe2\x94\x80 CLAUDE BOX \xe2\x95\xae");
        raw.extend_from_slice(b"\x1b[4;1H\xe2\x94\x82 remnant \xe2\x94\x82");
        // The TUI exits (orphan exit from this tail's perspective)…
        raw.extend_from_slice(b"\x1b[?1049l");
        // …and the shell renders its prompts on the restored primary.
        raw.extend_from_slice(b"PS C:\\> claude\r\nPS C:\\> ");
        let out = serialize_dead(&raw, 80, 24).expect("primary tail");
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.contains("PS C:\\> claude"), "shell content kept: {text:?}");
        assert!(
            !text.contains("CLAUDE BOX") && !text.contains("remnant"),
            "TUI fragments must not fuse into the reconstruction: {text:?}"
        );
    }

    /// Plain lines of a reconstruction, SGR stripped, CRLF split.
    fn plain_lines(bytes: &[u8]) -> Vec<String> {
        let mut strip = crate::strip::AnsiStripper::default();
        let mut raw = Vec::new();
        strip.feed_bytes(bytes, &mut raw);
        String::from_utf8_lossy(&raw)
            .split(['\r', '\n'])
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The HISTORY blank run immediately above the final prompt is capped at
    /// ONE row (pipeline audit §3 top-gap accounting): a restored seam's pad
    /// plus PS 5.1's pre-prompt blank otherwise stack a 2-3 row void over
    /// the one place the eye rests. Screen blanks are grid truth and are
    /// never dropped (cursor math).
    #[test]
    fn pre_prompt_history_blank_run_capped_at_one() {
        let mut raw = Vec::new();
        for i in 1..=4 {
            raw.extend_from_slice(format!("l{i}\r\n").as_bytes());
        }
        raw.extend_from_slice(b"\r\n\r\n\r\n\r\n\r\n"); // blanks scroll into history
        raw.extend_from_slice(b"\x1b[HPS> "); // prompt repainted at screen top
        let term = scratch_term(&raw, 40, 3);
        let out = serialize_term(&term, None);
        let mut strip = crate::strip::AnsiStripper::default();
        let mut plain = Vec::new();
        strip.feed_bytes(&out, &mut plain);
        let text = String::from_utf8_lossy(&plain).into_owned();
        let lines: Vec<&str> = text.split("\r\n").collect();
        let l4 = lines.iter().position(|l| l.trim_end() == "l4").expect("l4");
        let ps = lines
            .iter()
            .position(|l| l.trim_end().starts_with("PS>"))
            .expect("prompt");
        assert!(
            ps > l4,
            "prompt must follow the content: l4 at {l4}, prompt at {ps}"
        );
        assert_eq!(
            ps - l4 - 1,
            1,
            "exactly ONE breathing row between the last output and the prompt: {lines:?}"
        );
    }

    /// Conhost-parity grow: rows-growth must NOT pull scrollback onto the
    /// screen or move the cursor — content stays put, blanks open below
    /// (the daemon-mirror twin of the GUI's grow_rows_conhost contract).
    #[test]
    fn resize_conhost_grow_never_pulls_history() {
        let raw = {
            let mut r = Vec::new();
            for i in 0..30 {
                r.extend_from_slice(format!("fill{i}\r\n").as_bytes());
            }
            r.extend_from_slice(b"PS> ");
            r
        };
        let mut term = scratch_term(&raw, 40, 10);
        let hist_before = term.grid().history_size();
        let cur_before = term.grid().cursor.point;
        assert!(hist_before >= 15, "test needs real scrollback");
        resize_conhost(&mut term, 40, 24);
        assert_eq!(
            term.grid().cursor.point.line.0, cur_before.line.0,
            "cursor row unchanged — no pull"
        );
        assert_eq!(
            term.grid().history_size(),
            hist_before,
            "scrollback fully intact across the grow"
        );
    }

    /// THE Bug-B geometry-fidelity regression (field journal shape): a
    /// resize repaint stamped `[8;5;40t` (STALE — the true viewport had
    /// grown) followed by ls-style absolute addressing down to row 9. A
    /// fixed-size re-parse at the caller's 5 rows CLAMPED rows 6..9 onto the
    /// bottom row — table rows piled up and the next prompt overwrote their
    /// left half (`PS C:\…>   12:43 AM   1445 Cargo.toml` fused on one row,
    /// the user's screenshot). The scratch parse must follow the stream's
    /// own geometry (reports + deepest addressed row) so every line keeps
    /// its own row.
    #[test]
    fn scratch_parse_follows_stream_geometry() {
        let mut raw: Vec<u8> = Vec::new();
        // Conhost resize repaint at 5 rows: prompt at [H, [K-blanked rows.
        raw.extend_from_slice(b"\x1b[8;5;40t\x1b[HPS> \x1b[K");
        for _ in 0..4 {
            raw.extend_from_slice(b"\r\n\x1b[K");
        }
        raw.extend_from_slice(b"\x1b[1;5H");
        // The command + output addressing BEYOND the stamped height (the
        // true viewport was taller): the ls table shape.
        raw.extend_from_slice(b"ls");
        raw.extend_from_slice(b"\x1b[3;1HtableHeader");
        raw.extend_from_slice(b"\x1b[8;1HCargoLock-row");
        raw.extend_from_slice(b"\x1b[9;1HCargoToml-row");
        raw.extend_from_slice(b"\x1b[10;1H");
        raw.extend_from_slice(b"PS> ");
        let out = serialize_dead(&raw, 40, 5).expect("primary tail serializes");
        let lines = plain_lines(&out);
        // Every row on its own line, in order — nothing fused, nothing lost.
        let idx = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} missing from {lines:?}"))
        };
        let (h, lock, toml) = (idx("tableHeader"), idx("CargoLock-row"), idx("CargoToml-row"));
        assert!(h < lock && lock < toml, "rows in stream order: {lines:?}");
        for l in &lines {
            let fused = l.contains("CargoToml-row") && l.contains("PS>");
            assert!(!fused, "prompt fused onto a table row (the field corruption): {l:?}");
        }
        assert!(
            lines.iter().any(|l| l.trim() == "PS>"),
            "the final prompt survives on its own row: {lines:?}"
        );
    }

    /// Multi-size tails: a later WINSZ report re-sizes the scratch term, so
    /// absolute addressing after a legitimate resize stays exact too.
    #[test]
    fn scratch_parse_resizes_at_reports() {
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(b"\x1b[8;4;40t\x1b[Hsmall-screen\x1b[K");
        raw.extend_from_slice(b"\x1b[8;12;40t\x1b[Hbig-screen\x1b[K");
        raw.extend_from_slice(b"\x1b[11;1Hdeep-row");
        raw.extend_from_slice(b"\x1b[12;1HPS> ");
        let out = serialize_dead(&raw, 40, 4).expect("primary tail serializes");
        let lines = plain_lines(&out);
        assert!(
            lines.iter().any(|l| l.contains("deep-row")),
            "row 11 must exist after the 12-row report: {lines:?}"
        );
        let deep = lines.iter().position(|l| l.contains("deep-row")).unwrap();
        let prompt = lines.iter().rposition(|l| l.contains("PS>")).unwrap();
        assert!(deep < prompt, "deep row above the prompt: {lines:?}");
        assert!(
            !lines[deep].contains("PS>"),
            "no fusing at the taller size: {lines:?}"
        );
    }

    /// Field-journal harness (env-gated, no-op in CI): point TC_BUGB_JOURNAL
    /// at a real journal file to compare the OLD fixed-size scratch parse
    /// (rows piled onto the clamped bottom row → the fused
    /// `PS …>  …Cargo.toml` corruption) against the geometry-following one.
    /// Run with `--nocapture` for the line dumps.
    #[test]
    fn field_journal_reconstruction_no_fused_rows() {
        let Ok(path) = std::env::var("TC_BUGB_JOURNAL") else {
            return;
        };
        let raw = std::fs::read(&path).expect("journal readable");
        // OLD behavior: one fixed-size parse of the whole tail.
        let old = {
            use alacritty_terminal::term;
            let (tx, _rx) = std::sync::mpsc::channel();
            let mut t = Term::new(
                term::Config {
                    scrolling_history: 2000,
                    ..term::Config::default()
                },
                &TermSize::new(160, 42),
                super::super::session::EventProxy::new(tx),
            );
            let mut parser = super::super::ImmediateProcessor::new();
            parser.advance(&mut t, &raw);
            serialize_term(&t, None)
        };
        let new = serialize_dead(&raw, 160, 42).expect("primary tail");
        let fused = |lines: &[String]| {
            lines
                .iter()
                .filter(|l| {
                    l.contains("PS C:\\Terminal Control>")
                        && (l.contains("Cargo.toml") || l.contains("Cargo.lock"))
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let old_fused = fused(&plain_lines(&old));
        let new_fused = fused(&plain_lines(&new));
        println!("OLD fixed-size parse fused rows: {old_fused:#?}");
        println!("NEW geometry-following parse fused rows: {new_fused:#?}");
        assert!(
            new_fused.is_empty(),
            "geometry-following parse must not fuse prompt onto table rows"
        );
    }

    /// Diagnostic dumper (env-gated, no-op in CI): TC_DUMP_JOURNAL=<path> ⇒
    /// print the geometry-following dead-reconstruction of a real journal,
    /// line by line (what a restore preface / dead attach would replay).
    /// Report-only — pair with `--nocapture`.
    #[test]
    fn field_journal_dead_reconstruction_dump() {
        let Ok(path) = std::env::var("TC_DUMP_JOURNAL") else {
            return;
        };
        let raw = std::fs::read(&path).expect("journal readable");
        match serialize_dead(&raw, 160, 42) {
            Some(s) => {
                for (i, l) in plain_lines(&s).iter().enumerate() {
                    if !l.trim().is_empty() {
                        println!("{i:4}: {l}");
                    }
                }
            }
            None => println!("(alt-screen tail: raw-tail fallback would be used)"),
        }
    }

    /// Seam-decision tracer (env-gated, no-op in CI): TC_SEAM_TRACE=<path> ⇒
    /// scratch-parse a real journal and print, for every seam the render
    /// pass found, the surviving/dropped lines around it and the window the
    /// dangling-prompt dedupe searched. Report-only — pair with
    /// `--nocapture`.
    #[test]
    fn field_journal_seam_trace() {
        let Ok(path) = std::env::var("TC_SEAM_TRACE") else {
            return;
        };
        let raw = std::fs::read(&path).expect("journal readable");
        let term = scratch_term(&raw, 160, 42);
        let recs = render_lines(&term);
        let flag = |r: &LineRec| {
            format!(
                "{}{}{}{}",
                if r.drop { "D" } else { "-" },
                if r.blank { "b" } else { "-" },
                if r.in_history { "h" } else { "s" },
                if r.wrapped { "w" } else { "-" },
            )
        };
        let clip = |s: &str| s.chars().take(60).collect::<String>();
        for i in 0..recs.len() {
            if !recs[i].seam {
                continue;
            }
            println!("== seam at rec {i} ==");
            let lo = i.saturating_sub(4);
            for (k, r) in recs.iter().enumerate().take(i).skip(lo) {
                println!("  before {k:5} [{}] {}", flag(r), clip(&r.plain));
            }
            let mut shown = 0usize;
            for (k, r) in recs.iter().enumerate().skip(i + 1) {
                if r.blank {
                    continue;
                }
                println!("  after  {k:5} [{}] {}", flag(r), clip(&r.plain));
                shown += 1;
                if shown >= 8 {
                    break;
                }
            }
        }
        let victims = recs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.drop && !r.blank && !r.seam)
            .map(|(k, r)| format!("{k}: {}", clip(&r.plain)))
            .collect::<Vec<_>>();
        println!("dedupe victims ({}):", victims.len());
        for v in &victims {
            println!("  {v}");
        }
    }

    /// The dangling-prompt matcher: equality, typed-at-prompt continuation,
    /// repaint doubling, cmd's space-less prompt — and the boundary guard
    /// that keeps ordinary output lines from merging.
    #[test]
    fn dangling_prompt_match_matrix() {
        // Exact (the original rule).
        assert!(dangling_prompt_match("PS C:\\>", "PS C:\\>"));
        // The user typed at the new session's first prompt (field journal).
        assert!(dangling_prompt_match("PS C:\\>", "PS C:\\> ls"));
        // Conhost resize-repaint doubling (field journal).
        assert!(dangling_prompt_match("PS C:\\>", "PS C:\\> PS C:\\>"));
        // cmd prompts have no trailing space before typed text.
        assert!(dangling_prompt_match("C:\\dir>", "C:\\dir>dir /b"));
        // bash.
        assert!(dangling_prompt_match("user@host:~$", "user@host:~$ make"));
        // Boundary guard: plain output lines never merge.
        assert!(!dangling_prompt_match("Compiling x", "Compiling xyz"));
        assert!(!dangling_prompt_match("", "anything"));
        assert!(!dangling_prompt_match("PS C:\\>", "PS D:\\>"));
    }

    /// THE restored-render-fix field bug: the dead session's final bare
    /// prompt survived every restore because (a) the user had typed at the
    /// new session's first prompt by re-parse time and (b) restored cmd/WSL
    /// sessions print a banner before their first prompt. Both defeat exact
    /// first-content-line equality; the windowed prefix rule engages.
    #[test]
    fn dangling_prompt_dedupes_across_typed_text_and_banners() {
        // Enough filler that the dead session's trailing prompt lands in
        // HISTORY at re-parse time: only history rows are ever dropped
        // (screen rows anchor the cursor/coordinate math) — exactly the
        // field shape, where the dead content sits deep in scrollback.
        let filler = |raw: &mut Vec<u8>| {
            for i in 0..30 {
                raw.extend_from_slice(format!("fill {i}\r\n").as_bytes());
            }
        };

        // (a) typed-at-first-prompt: [dead: bare prompt][seam][new: prompt+cmd]
        let mut raw = Vec::new();
        raw.extend_from_slice(b"old output\r\nPS C:\\> \r\n");
        raw.extend_from_slice(format!("\x1b[8m{SEAM_SENTINEL}\x1b[28m\r\n").as_bytes());
        raw.extend_from_slice(b"PS C:\\> ls\r\nfile.txt\r\n");
        filler(&mut raw);
        raw.extend_from_slice(b"PS C:\\> ");
        let out = serialize_dead(&raw, 80, 12).expect("primary tail");
        let lines = plain_lines(&out);
        let bare = lines.iter().filter(|l| l.trim_end() == "PS C:\\>").count();
        assert_eq!(
            bare, 1,
            "the dead session's dangling bare prompt must be deduped; only the \
             final live prompt survives: {lines:?}"
        );

        // (b) banner between the seam and the first prompt (cmd restore
        // shape): the window search crosses it.
        let mut raw = Vec::new();
        raw.extend_from_slice(b"old output\r\nC:\\work>\r\n");
        raw.extend_from_slice(format!("\x1b[8m{SEAM_SENTINEL}\x1b[28m\r\n").as_bytes());
        raw.extend_from_slice(b"Microsoft Windows [Version 10.0.26200]\r\n");
        raw.extend_from_slice(b"(c) Microsoft Corporation. All rights reserved.\r\n\r\n");
        raw.extend_from_slice(b"C:\\work>echo hi\r\nhi\r\n");
        filler(&mut raw);
        raw.extend_from_slice(b"C:\\work>");
        let out = serialize_dead(&raw, 80, 12).expect("primary tail");
        let lines = plain_lines(&out);
        let bare = lines.iter().filter(|l| l.trim_end() == "C:\\work>").count();
        assert_eq!(
            bare, 1,
            "dangling cmd prompt deduped across the banner window: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Microsoft Windows")),
            "the banner itself is real content and must survive: {lines:?}"
        );

        // Negative: a session that died mid-output must NOT lose its last
        // line to a coincidental continuation in the next session.
        let mut raw = Vec::new();
        raw.extend_from_slice(b"building step\r\n");
        raw.extend_from_slice(format!("\x1b[8m{SEAM_SENTINEL}\x1b[28m\r\n").as_bytes());
        raw.extend_from_slice(b"building steps done\r\n");
        filler(&mut raw);
        raw.extend_from_slice(b"PS C:\\> ");
        let out = serialize_dead(&raw, 80, 12).expect("primary tail");
        let lines = plain_lines(&out);
        assert!(
            lines.iter().any(|l| l.trim_end() == "building step"),
            "non-prompt trailing line must survive (boundary guard): {lines:?}"
        );
    }

    /// The daemon's journal-only seam write, exactly as mod.rs launch()
    /// composes it: CRLF + concealed sentinel + a screenful of pad + home.
    fn seam_bytes(rows: usize) -> Vec<u8> {
        let mut s = format!("\r\n\x1b[8m{SEAM_SENTINEL}\x1b[28m").into_bytes();
        s.extend(std::iter::repeat_n(b"\r\n".as_slice(), rows).flatten());
        s.extend_from_slice(b"\x1b[H");
        s
    }

    /// Seam-adjacent banner dedupe (the "~15 stacked `Microsoft Windows
    /// [Version …]` banners across restarts" field bug): every real cmd
    /// spawn prints the same banner, one copy accumulates per lifetime in
    /// the journal, and reconstructions showed them all. Rule R keeps the
    /// NEWEST copy; prior lifetimes' copies drop at their seams; real
    /// output around them survives.
    #[test]
    fn seam_banner_dedupe_keeps_newest_copy() {
        const BANNER: &[u8] = b"Microsoft Windows [Version 10.0.26200.8655]\r\n(c) Microsoft Corporation. All rights reserved.\r\n\r\n";
        let mut raw = Vec::new();
        raw.extend_from_slice(BANNER);
        raw.extend_from_slice(b"C:\\>echo OLD_OUTPUT_1\r\nOLD_OUTPUT_1\r\n\r\nC:\\>");
        raw.extend(seam_bytes(12));
        raw.extend_from_slice(BANNER);
        raw.extend_from_slice(b"C:\\>echo OLD_OUTPUT_2\r\nOLD_OUTPUT_2\r\n\r\nC:\\>");
        raw.extend(seam_bytes(12));
        raw.extend_from_slice(BANNER);
        raw.extend_from_slice(b"C:\\>");
        let out = serialize_dead(&raw, 80, 12).expect("primary tail");
        let lines = plain_lines(&out);
        let banners = lines
            .iter()
            .filter(|l| l.contains("Microsoft Windows [Version"))
            .count();
        assert_eq!(banners, 1, "exactly one banner survives: {lines:?}");
        // The survivor is the NEWEST copy — it renders below prior content.
        let b = lines
            .iter()
            .position(|l| l.contains("Microsoft Windows [Version"))
            .unwrap();
        let o2 = lines
            .iter()
            .position(|l| l.trim_end() == "OLD_OUTPUT_2")
            .expect("old output kept");
        assert!(b > o2, "newest banner sits below prior content: {lines:?}");
        assert!(
            lines.iter().any(|l| l.trim_end() == "OLD_OUTPUT_1"),
            "first lifetime's output survives: {lines:?}"
        );
    }

    /// Rule R's safety invariant: openings stop at the first prompt-sigil
    /// line ('>', '$', '#'), so identical prompt/command rows across
    /// lifetimes — a user's habitual first command — are NEVER deduped.
    /// Pre-banner-fix pwsh journals have exactly this shape.
    #[test]
    fn seam_banner_dedupe_never_eats_prompt_shaped_rows() {
        let mut raw = Vec::new();
        raw.extend_from_slice(b"PS C:\\> claude\r\nclaude-output-a\r\nPS C:\\> ");
        raw.extend(seam_bytes(12));
        raw.extend_from_slice(b"PS C:\\> claude\r\nclaude-output-b\r\nPS C:\\> ");
        let out = serialize_dead(&raw, 80, 12).expect("primary tail");
        let lines = plain_lines(&out);
        let invocations = lines
            .iter()
            .filter(|l| l.trim_end() == "PS C:\\> claude")
            .count();
        assert_eq!(
            invocations, 2,
            "identical typed commands are real content, never banner-deduped: {lines:?}"
        );
    }

    /// T3a boundary: a WIDTH CHANGE between lifetimes re-wraps the banner,
    /// so the two copies' row texts differ and the pairwise match breaks at
    /// row 0 — BOTH copies survive. This freezes the FAIL-SAFE direction:
    /// when in doubt the dedupe must keep, never eat (a lost real row is
    /// unrecoverable history; a doubled banner is cosmetic).
    #[test]
    fn seam_banner_dedupe_width_change_keeps_both() {
        let mut raw = Vec::new();
        // Old lifetime rendered at a narrower width: the version line landed
        // as two physical rows in the journal.
        raw.extend_from_slice(
            b"Microsoft Windows [Version 10.0\r\n.26200.8655]\r\n(c) Microsoft Corporation. All rights reserved.\r\n\r\nC:\\>",
        );
        raw.extend(seam_bytes(12));
        raw.extend_from_slice(
            b"Microsoft Windows [Version 10.0.26200.8655]\r\n(c) Microsoft Corporation. All rights reserved.\r\n\r\nC:\\>",
        );
        let out = serialize_dead(&raw, 80, 12).expect("primary tail");
        let lines = plain_lines(&out);
        let copyrights = lines
            .iter()
            .filter(|l| l.contains("(c) Microsoft Corporation"))
            .count();
        assert_eq!(
            copyrights, 2,
            "re-wrapped copies never match — both survive (fail-safe): {lines:?}"
        );
    }

    /// T3b boundary: an opening block LONGER than DEDUPE_WINDOW (a giant
    /// MOTD). Both openings cap at 40 rows, so the earlier copy's first 40
    /// rows dedupe and its tail rows past the window survive as an orphan —
    /// documented degradation: leftovers are cosmetic, nothing real is ever
    /// dropped, and the new session's copy stays complete.
    #[test]
    fn seam_banner_dedupe_giant_motd_caps_at_window() {
        let motd: Vec<u8> = (1..=45)
            .flat_map(|n| format!("motd line {n:02} lorem ipsum\r\n").into_bytes())
            .collect();
        let mut raw = Vec::new();
        raw.extend_from_slice(&motd);
        raw.extend_from_slice(b"C:\\>");
        raw.extend(seam_bytes(12));
        raw.extend_from_slice(&motd);
        raw.extend_from_slice(b"C:\\>");
        let out = serialize_dead(&raw, 80, 12).expect("primary tail");
        let lines = plain_lines(&out);
        let count = |needle: &str| {
            lines
                .iter()
                .filter(|l| l.trim_end() == needle)
                .count()
        };
        // Inside the window: old copy deduped, one survivor.
        assert_eq!(
            count("motd line 10 lorem ipsum"),
            1,
            "rows inside DEDUPE_WINDOW dedupe: {lines:?}"
        );
        assert_eq!(count("motd line 40 lorem ipsum"), 1);
        // Past the window: the old copy's tail orphans (kept — cosmetic),
        // and the new copy is complete.
        assert_eq!(
            count("motd line 42 lorem ipsum"),
            2,
            "rows past DEDUPE_WINDOW stay as a kept orphan tail: {lines:?}"
        );
        assert_eq!(count("motd line 45 lorem ipsum"), 2);
    }

    /// Pass 3.5 (field cmd journal shape): a conhost resize repaint erased
    /// an earlier seam's sentinel row and re-rendered the banner mid-region
    /// — no boundary for pass 3 — but the orphan sits directly above a
    /// surviving seam whose next session reprints the same block, so it
    /// drops; `ver`-style output (version line only, not the whole block)
    /// stays.
    #[test]
    fn seam_trailing_banner_orphan_drops() {
        const BANNER: &[u8] = b"Microsoft Windows [Version 10.0.26200.8655]\r\n(c) Microsoft Corporation. All rights reserved.\r\n\r\n";
        let mut raw = Vec::new();
        raw.extend_from_slice(b"C:\\>dir\r\n real-output-1\r\n real-output-2\r\n");
        // The repaint-orphaned banner copy (its seam/sentinel was overwritten).
        raw.extend_from_slice(BANNER);
        raw.extend_from_slice(b"C:\\>");
        raw.extend(seam_bytes(12));
        raw.extend_from_slice(BANNER);
        raw.extend_from_slice(b"C:\\>");
        let out = serialize_dead(&raw, 80, 12).expect("primary tail");
        let lines = plain_lines(&out);
        let banners = lines
            .iter()
            .filter(|l| l.contains("Microsoft Windows [Version"))
            .count();
        assert_eq!(banners, 1, "orphan collapsed, newest kept: {lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("real-output-1")),
            "real content survives: {lines:?}"
        );

        // `ver` output = the version line ALONE — fails whole-block equality.
        let mut raw = Vec::new();
        raw.extend_from_slice(b"C:\\>ver\r\n\r\nMicrosoft Windows [Version 10.0.26200.8655]\r\n\r\nC:\\>");
        raw.extend(seam_bytes(12));
        raw.extend_from_slice(BANNER);
        raw.extend_from_slice(b"C:\\>");
        let out = serialize_dead(&raw, 80, 12).expect("primary tail");
        let lines = plain_lines(&out);
        let vers = lines
            .iter()
            .filter(|l| l.contains("Microsoft Windows [Version"))
            .count();
        assert_eq!(vers, 2, "ver output is real content, never deduped: {lines:?}");
    }

    /// The live-attach half (Preface::opening splice): the preface's
    /// final-session banner is spliced out when the live mirror reprints it
    /// — the FIRST restart already shows one banner, not two — while a live
    /// session that does NOT reprint (WSL same-day MOTD) keeps the preface
    /// copy as the newest visible one.
    #[test]
    fn preface_banner_splices_against_live_reprint() {
        const BANNER: &[u8] = b"Windows PowerShell\r\nCopyright (C) Microsoft Corporation. All rights reserved.\r\n\r\nInstall the latest PowerShell for new features and improvements! https://aka.ms/PSWindows\r\n\r\n";
        let mut raw = Vec::new();
        raw.extend_from_slice(BANNER);
        raw.extend_from_slice(b"PS C:\\> echo OLD1\r\nOLD1\r\nPS C:\\> ");
        let (preface, _) = preface_with_alt_fix(&raw, 80, 24);
        assert!(
            !preface.opening.is_empty(),
            "the final session's opening block must be recorded"
        );

        // Live mirror reprints the banner, then its fresh prompt.
        let mut live = Vec::new();
        live.extend_from_slice(BANNER);
        live.extend_from_slice(b"PS C:\\> ");
        let term = scratch_term(&live, 80, 24);
        let out = serialize_term(&term, Some(&preface));
        let lines = plain_lines(&out);
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.trim_end() == "Windows PowerShell")
                .count(),
            1,
            "preface banner spliced against the live reprint: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.trim_end() == "OLD1"),
            "preface content kept: {lines:?}"
        );
        // The dangling-prompt dedupe still composes with the splice.
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.trim_end() == "PS C:\\>")
                .count(),
            1,
            "exactly one bare prompt: {lines:?}"
        );

        // No reprint ⇒ the preface copy stays (honest newest copy).
        let term2 = scratch_term(b"PS C:\\> ", 80, 24);
        let out2 = serialize_term(&term2, Some(&preface));
        let lines2 = plain_lines(&out2);
        assert_eq!(
            lines2
                .iter()
                .filter(|l| l.trim_end() == "Windows PowerShell")
                .count(),
            1,
            "no live reprint ⇒ preface banner kept: {lines2:?}"
        );
    }

    /// The exact PS restore byte shape from the user's field journal
    /// (2026-07-04): the dead session's final prompt row is hook-wrapped
    /// (OSC 7717 pre + OSC 9;9 + 133;A + `PS C:\> ` + 133;B), the daemon's
    /// journal-only seam follows (CRLF + concealed sentinel + pad + home),
    /// and the restored session boots with a title OSC, ?9001h, init/pre
    /// hooks, its prompt, then the conhost resize repaint (XTWINOPS stamp +
    /// home + prompt rewrite + per-row ESC[K). The dangling dead prompt must
    /// dedupe against the repainted fresh one — the OSC/hook dressing and
    /// the repaint doubling must not defeat the comparison.
    #[test]
    fn field_ps_restore_shape_dedupes() {
        let mut raw = Vec::new();
        raw.extend_from_slice(b"old output\r\n");
        for i in 0..30 {
            raw.extend_from_slice(format!("fill {i}\r\n").as_bytes());
        }
        // Dead session's final prompt row, exactly as journaled.
        raw.extend_from_slice(
            b"\x1b]7717;554cc51df29d8ce9;pre;7b7d\x07\x1b]9;9;C:\\\x07\
              \x1b]133;A\x07PS C:\\> \x1b]133;B\x07\r\n",
        );
        // The daemon seam write (mod.rs launch): CRLF + sentinel + pad + home.
        raw.extend_from_slice(format!("\r\n\x1b[8m{SEAM_SENTINEL}\x1b[28m").as_bytes());
        raw.extend(std::iter::repeat_n(b"\r\n".as_slice(), 12).flatten());
        raw.extend_from_slice(b"\x1b[H");
        // Restored session boot bytes (journal-verbatim shapes).
        raw.extend_from_slice(
            b"\x1b[6n\x1b[?9001h\x1b[?1004h\x1b[m\
              \x1b]0;C:\\WINDOWS\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\x07\x1b[?25h\
              \x1b]7717;bc488cc35a604812;init;7b7d\x07\
              \x1b]7717;bc488cc35a604812;pre;7b7d\x07\x1b]9;9;C:\\\x07\
              \x1b]133;A\x07PS C:\\> \x1b]133;B\x07",
        );
        // Conhost resize repaint: stamp + home + prompt rewrite + ESC[K rows.
        raw.extend_from_slice(b"\x1b[?25l\x1b[8;12;80t\x1b[HPS C:\\>\x1b[K\r\n");
        for _ in 0..10 {
            raw.extend_from_slice(b"\x1b[K\r\n");
        }
        raw.extend_from_slice(b"\x1b[K\x1b[1;9H\x1b[?25h");

        let out = serialize_dead(&raw, 80, 12).expect("primary tail");
        let lines = plain_lines(&out);
        let bare = lines
            .iter()
            .filter(|l| l.trim_end() == "PS C:\\>")
            .count();
        assert_eq!(
            bare, 1,
            "exactly the live repainted prompt survives; the dead session's \
             dangler dedupes through the hook/OSC dressing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.trim_end() == "fill 29"),
            "real content above the seam survives: {lines:?}"
        );
    }

    /// Parse a reconstruction into a client-sized Term and return
    /// (trimmed screen-row texts, history blank/non-blank counts, cursor).
    fn client_grid(bytes: &[u8], cols: usize, rows: usize) -> (Vec<String>, usize, usize, (i32, usize)) {
        use alacritty_terminal::term;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut client = Term::new(
            term::Config {
                scrolling_history: 5000,
                ..term::Config::default()
            },
            &TermSize::new(cols, rows),
            super::super::session::EventProxy::new(tx),
        );
        let mut parser = super::super::ImmediateProcessor::new();
        parser.advance(&mut client, bytes);
        let grid = client.grid();
        let hist = grid.history_size() as i32;
        let row_text = |line: i32| {
            let row = &grid[Line(line)];
            let len = row.line_length().0.min(cols);
            (0..len).map(|c| row[Column(c)].c).collect::<String>().trim_end().to_string()
        };
        let screen: Vec<String> = (0..rows as i32).map(row_text).collect();
        let (mut hb, mut hc) = (0usize, 0usize);
        for line in -hist..0 {
            if row_text(line).is_empty() {
                hb += 1;
            } else {
                hc += 1;
            }
        }
        let cur = grid.cursor.point;
        (screen, hb, hc, (cur.line.0, cur.column.0))
    }

    /// THE 2026-07-06 field restore journals, end to end (byte-exact shape,
    /// anonymized). Terminal "Shell · alice 2": a PS session restored 7×,
    /// journal = boot paint + [banner+prompt] per lifetime, each joined by
    /// the launch() seam (sentinel + 51-row pad + home), ending in the
    /// attach-resize conhost repaint. Terminal "edgebox": the ssh twin whose
    /// every lifetime is ONE bare prompt. The reconstruction fed to a client
    /// at the journal's own geometry must be CONTIGUOUS: one banner at the
    /// screen top, the prompt directly under it (one blank between the
    /// install line and the prompt — the shell's own spacing), cursor ON the
    /// prompt row, and at most MAX_BLANK_RUN residual history blanks. The
    /// field bug rendered banner / screen-sized void / prompt-at-bottom; the
    /// render half of that fix lives in gui::term_view (content_y_offset
    /// uncapped fill) — THIS pins the serialize/replay half.
    #[test]
    fn field_restore_journal_reconstruction_is_contiguous() {
        const ROWS: usize = 51;
        const COLS: usize = 158;
        let boot = |title: &str| {
            let mut b = format!(
                "\x1b[6n\x1b[?9001h\x1b[?1004h\x1b[m\x1b]0;{title}\x07\x1b[?25h\x1b[?25l\x1b[8;{ROWS};{COLS}t"
            )
            .into_bytes();
            for _ in 0..ROWS {
                b.extend_from_slice(b"\x1b[K\r\n");
            }
            b.extend_from_slice(b"\x1b[K\x1b[H");
            b
        };
        let relaunch = |title: &str| {
            format!("\x1b[6n\x1b[?9001h\x1b[?1004h\x1b[m\x1b]0;{title}\x07\x1b[?25h").into_bytes()
        };
        const PS_BANNER: &str = "Windows PowerShell\r\nCopyright (C) Microsoft Corporation. All rights reserved.\x1b[4;1HInstall the latest PowerShell for new features and improvements! https://aka.ms/PSWindows\x1b[6;1H";
        let ps_prompt = "\x1b]7717;0011223344556677;init;7b7d\x07\x1b]7717;0011223344556677;pre;7b7d\x07\x1b]9;9;C:\\Users\\alice\x07\x1b]133;A\x07PS C:\\Users\\alice> \x1b]133;B\x07";

        // ── The PS journal.
        let mut raw = boot("C:\\WINDOWS\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
        raw.extend_from_slice(ps_prompt.as_bytes()); // lifetime 1: bare prompt
        for _ in 0..7 {
            raw.extend(seam_bytes(ROWS));
            raw.extend(relaunch("C:\\WINDOWS\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"));
            raw.extend_from_slice(b"\x1b[?25l");
            raw.extend_from_slice(PS_BANNER.as_bytes());
            raw.extend_from_slice(b"\x1b[?25h");
            raw.extend_from_slice(ps_prompt.as_bytes());
        }
        // The attach-resize conhost repaint (journal-verbatim shape).
        raw.extend_from_slice(
            format!("\x1b[?25l\x1b[8;{ROWS};{COLS}t\x1b[HWindows PowerShell\x1b[K\r\nCopyright (C) Microsoft Corporation. All rights reserved.\x1b[K\r\n\x1b[K\r\nInstall the latest PowerShell for new features and improvements! https://aka.ms/PSWindows\x1b[K\r\n\x1b[K\r\nPS C:\\Users\\alice>\x1b[K\r\n").as_bytes(),
        );
        for _ in 0..44 {
            raw.extend_from_slice(b"\x1b[K\r\n");
        }
        raw.extend_from_slice(b"\x1b[K\x1b[6;21H\x1b[?25h");

        let out = serialize_dead(&raw, COLS as u16, ROWS as u16).expect("primary tail");
        let (screen, hist_blank, hist_content, cursor) = client_grid(&out, COLS, ROWS);
        assert_eq!(screen[0], "Windows PowerShell", "banner at the screen top: {screen:?}");
        assert_eq!(
            screen.iter().filter(|l| l.as_str() == "Windows PowerShell").count() + hist_content,
            1,
            "exactly ONE banner copy anywhere (dedupe): screen={screen:?} hist_content={hist_content}"
        );
        let install = screen.iter().position(|l| l.starts_with("Install the latest")).expect("install line");
        let prompt = screen.iter().position(|l| l.starts_with("PS C:\\Users\\alice>")).expect("prompt row");
        assert_eq!(
            prompt,
            install + 2,
            "CONTIGUOUS: prompt directly under the banner (one shell blank), never a void: {screen:?}"
        );
        assert_eq!(cursor.0 as usize, prompt, "cursor sits on the prompt row");
        assert!(
            hist_blank <= MAX_BLANK_RUN,
            "residual history blanks capped: {hist_blank}"
        );

        // ── The ssh journal (every lifetime a single bare prompt).
        let ssh_prompt = "\x1b]7717;8899aabbccddeeff;init;7b7d\x07\x1b]7717;8899aabbccddeeff;pre;7b7d\x07\x1b]9;9;/home/alice\x07\x1b[?2004h\x1b]133;A\x07\x1b[32m\x1b[1malice@edgebox\x1b[m:\x1b[34m\x1b[1m~\x1b[m$ \x1b]0;alice@edgebox: ~\x07\x1b]133;B\x07";
        let mut raw = boot("C:\\WINDOWS\\System32\\OpenSSH\\ssh.exe");
        raw.extend_from_slice(ssh_prompt.as_bytes());
        for _ in 0..8 {
            raw.extend(seam_bytes(ROWS));
            raw.extend(relaunch("C:\\WINDOWS\\System32\\OpenSSH\\ssh.exe"));
            raw.extend_from_slice(ssh_prompt.as_bytes());
        }
        // Resize repaint + bash's own prompt redraw (journal-verbatim shape).
        raw.extend_from_slice(
            format!("\x1b[?25l\x1b[8;{ROWS};{COLS}t\x1b[32m\x1b[1m\x1b[Halice@edgebox\x1b[m:\x1b[34m\x1b[1m~\x1b[m$\x1b[K\r\n").as_bytes(),
        );
        for _ in 0..49 {
            raw.extend_from_slice(b"\x1b[K\r\n");
        }
        raw.extend_from_slice(b"\x1b[K\x1b[1;14H\x1b[?25h\x1b[?25l\x1b[H\x1b[K\x1b[?25h\x1b]133;A\x07\x1b[32m\x1b[1malice@edgebox\x1b[m:\x1b[34m\x1b[1m~\x1b[m$ \x1b]133;B\x07");

        let out = serialize_dead(&raw, COLS as u16, ROWS as u16).expect("primary tail");
        let (screen, hist_blank, hist_content, cursor) = client_grid(&out, COLS, ROWS);
        assert!(
            screen[0].starts_with("alice@edgebox:~$"),
            "the one live prompt at the screen top: {screen:?}"
        );
        assert_eq!(
            screen.iter().filter(|l| !l.is_empty()).count(),
            1,
            "nothing but the live prompt (all dead bare prompts deduped): {screen:?}"
        );
        assert_eq!(hist_content, 0, "no stacked dead prompts in history");
        assert!(hist_blank <= MAX_BLANK_RUN);
        assert_eq!(cursor.0, 0, "cursor on the prompt row");
    }

    /// The report scanner: offsets/values parsed exactly; garbage shapes
    /// (missing digits, wrong final, absurd values) are skipped.
    #[test]
    fn winsz_scanner_matrix() {
        let raw = b"text\x1b[8;42;160tmid\x1b[8;49;145t\x1b[8;;5t\x1b[8;9xtail";
        let r = winsz_reports(raw);
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].1, r[0].2), (42, 160));
        assert_eq!((r[1].1, r[1].2), (49, 145));
        assert_eq!(r[0].0, 4, "offset of the ESC");
        assert_eq!(winsz_reports(b"no reports here"), Vec::new());
        // Deep-row scan: CUP, CUP-row-only, VPA count; SGR/other finals don't.
        assert_eq!(max_addressed_row(b"\x1b[46;1H\x1b[9d\x1b[50m\x1b[8;99;10t"), 46);
        assert_eq!(max_addressed_row(b"\x1b[7H"), 7);
        assert_eq!(max_addressed_row(b"plain"), 0);
    }
}
