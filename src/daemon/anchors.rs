//! Restored-history anchors (history-parity, proto 7): map persisted block
//! records and superseded bare-prompt rows to REPLAY rows at attach time, so
//! a reopened GUI can re-mint the `❯` history covers and block chrome its
//! previous session minted live. Without this, covers/anchors were GUI-session
//! state: close + reopen rendered all pre-reopen history as raw `PS …>` rows
//! (the user's "works then I close and reopen boom all previous is missing").
//!
//! The mapping runs in three stages, all attach-time, none touching the hot
//! ingest path:
//!
//!  1. HOOK SCAN — `BlockScanner` over the journal tail collects every
//!     prompt-end (OSC 133;B), exec and pre hook with its absolute stream
//!     offset. Each 133;B opens a "prompt slot": a slot owning one or more
//!     BlockRecs (`start_off` ∈ [this 133;B, next 133;B)) is a block prompt
//!     row — this covers exec-hooked shells AND cmd-family synthetic records
//!     (whose start_off is the pre-write journal head, always after the
//!     prompt rendered); a slot with a following `pre` and no exec and no rec
//!     is a superseded bare prompt (empty Enter / Ctrl+C) — a spacer.
//!
//!  2. MAPPING PARSE — the tail is re-parsed through a scratch Term that
//!     follows the stream's own geometry (`serialize::scratch_segments`,
//!     the exact rules the replay reconstruction uses), pausing at every
//!     slot's 133;B to capture the cursor row/column and the prompt prefix
//!     text. Rows are then tracked through the rest of the parse with an
//!     EXACT scroll odometer: the grid's `display_offset` is pinned at 1, and
//!     alacritty's `scroll_up` increments a non-zero display offset on every
//!     scroll — including through ring saturation, where the history-delta
//!     trick every other tracker uses goes blind. ED3/RIS reset the offset
//!     to 0, which is detected and drops everything pending (the history is
//!     gone anyway). The odometer's only overcount source is a primary-screen
//!     DECSTBM sub-region scroll (rare in shell streams); overcounts can only
//!     over-prune — a missing hint, never a wrong one.
//!
//!  3. REPLAY PLACEMENT — the actual Replay bytes (the exact stream the
//!     client will parse) are parsed into a second scratch Term, and each
//!     surviving checkpoint's replay row is COMPUTED from the serializer's
//!     own emit decisions (`serialize::emitted_grid_rows`, bottom-anchored
//!     on the relative-cursor contract), then verified by text inside a
//!     ±ROW_TOL window: the row must render the captured prompt prefix AND
//!     the record's command (blocks) or a blank input area (spacers). Rows
//!     that fail are simply absent from the hints — drop, never guess; the
//!     GUI re-verifies once more against its own grid before minting. A
//!     final replay-truth sweep then spacer-hints any unclaimed row that
//!     still renders a known bare-prompt shape above the final prompt row
//!     (conhost repaint doubles, reflow-dropped checkpoints) — those rows
//!     are blanked live, so reopen parity blanks them too.

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Term, TermMode};

use crate::protocol::{AnchorHint, ANCHOR_BLOCK, ANCHOR_SPACER};
use crate::state::BlockRec;

use super::blocks::{BlockScanner, HookVerb};
use super::serialize;
use super::session::EventProxy;
use super::ImmediateProcessor;

/// Mapping-parse scrollback depth. Must cover the deepest row a replay can
/// carry (preface ≤ ~2050 lines + mirror history 2000 + screen); rows deeper
/// than the replay window can never match anyway, and the pinned odometer
/// keeps counting through saturation, so a deeper tail only evicts rows that
/// were already unmatchable.
const MAP_HISTORY: usize = 5000;
/// Spacer hints kept per attach (newest win). Blocks are already capped at
/// 500 by the store.
const MAX_SPACERS: usize = 500;
/// Feed granularity while the odometer is not yet pinned (history still 0):
/// a chunk this small cannot scroll ambiguously far in one step.
const PREPIN_CHUNK: usize = 4 * 1024;
/// Feed granularity once pinned (matches the ingest slice size).
const CHUNK: usize = 64 * 1024;
/// How far a checkpoint's COMPUTED replay row may be corrected by the text
/// verification (±). The computation is exact for a dead reconstruction; on
/// a live attach the replay is preface + mirror serialization while the
/// mapping grid is one whole-tail parse, and their only divergence is the
/// preface's trailing-blank pop / pre-prompt blank cap at the boundary —
/// bounded by MAX_BLANK_RUN rows. Kept deliberately tiny: candidate rows in
/// the field are often IDENTICAL (bare-prompt runs, repeated `ls`), so any
/// wide window re-opens the wrong-join door this replaced (see stage 3).
const ROW_TOL: i32 = 2;

/// One prompt slot derived from the hook scan (stage 1).
struct Slot {
    /// Absolute stream offset just past the 133;B terminator — the mapping
    /// checkpoint position, and the spacer hint's identity.
    pe_off: u64,
    /// Indices into `recs` anchored at this prompt (empty ⇒ not a block row).
    recs: std::ops::Range<usize>,
    /// Bare prompt superseded without an exec: a spacer candidate.
    spacer: bool,
}

/// One recorded mapping checkpoint (stage 2), tracked to the end of the parse.
struct Cp {
    slot: usize,
    /// Current mapping-grid row (shifted per feed slice by the odometer).
    line: i32,
    /// Cursor column at the 133;B = the prompt end.
    col: usize,
    /// Row text left of `col` at capture, trim-end — the prompt prefix.
    prefix: String,
}

/// Compute the replay-row hints for one attach. `tail` is the journal tail
/// snapshot the replay was built from (live: the same lock-held snapshot;
/// dead: serialize_dead's input), `tail_base` its absolute stream offset,
/// `replay` the exact bytes the client will parse, `recs` the block records
/// the client got in the Blocks full sync (sorted by start_off), and
/// `cols`/`rows` the attacher's grid. Pure; call OUTSIDE the journal lock.
pub fn compute_hints(
    tail: &[u8],
    tail_base: u64,
    replay: &[u8],
    recs: &[BlockRec],
    cols: u16,
    rows: u16,
) -> Vec<AnchorHint> {
    if tail.is_empty() || replay.is_empty() {
        return Vec::new();
    }
    // Alt-cut safety: identical trim to the reconstruction's scratch parse.
    let (alt_start, _) = serialize::alt_cut_scan(tail);
    let region = &tail[alt_start..];
    let region_base = tail_base + alt_start as u64;

    // ── stage 1: hook scan + slots ──────────────────────────────────────
    let events = scan_events(region, region_base);
    let slots = build_slots(&events, recs);
    if slots.is_empty() {
        return Vec::new();
    }
    // Checkpoints in stream order: (region-relative offset, slot index).
    let cps: Vec<(usize, usize)> = slots
        .iter()
        .enumerate()
        .map(|(i, s)| ((s.pe_off - region_base) as usize, i))
        .collect();

    // ── stage 2: mapping parse with the pinned-offset odometer ─────────
    let segs = serialize::scratch_segments(region, cols, rows);
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut term = Term::new(
        alacritty_terminal::term::Config {
            scrolling_history: MAP_HISTORY,
            ..Default::default()
        },
        &TermSize::new(segs[0].2, segs[0].3),
        EventProxy::new(tx),
    );
    let mut parser = ImmediateProcessor::new();
    let mut pending: Vec<Cp> = Vec::new();
    let mut pinned = false;
    let mut last_history = 0usize;
    let mut cpi = 0usize;
    // Session-boundary alt closures: EXACTLY the seams the reconstruction's
    // scratch parse closes (serialize::seam_offsets) — a dead-in-alt region
    // otherwise swallows every later session into the frozen alt grid on
    // one side only, and every row downstream diverges.
    let seams = serialize::seam_offsets(region);
    let mut smi = 0usize;
    for (si, &(start, end, segc, segr)) in segs.iter().enumerate() {
        if si > 0 {
            // Segment boundary = a WINSZ resize. Row shifts across a resize
            // are only history-delta observable — a cols reflow additionally
            // moves rows relative to each other by re-wrap deltas, which is
            // invisible here; affected checkpoints end up outside ROW_TOL of
            // their computed replay row and drop at verification (honest
            // miss; the spacer sweep still covers their bare-prompt rows).
            let h0 = term.grid().history_size();
            serialize::resize_conhost(&mut term, segc, segr);
            let h1 = term.grid().history_size();
            let d = h1 as i32 - h0 as i32;
            if d != 0 {
                for c in pending.iter_mut() {
                    c.line -= d;
                }
            }
            pending.retain(|c| c.line >= -(h1 as i32));
            pinned = repin(&mut term);
            last_history = h1;
        }
        let mut pos = start;
        while pos < end {
            let mut target = end.min(pos + if pinned { CHUNK } else { PREPIN_CHUNK });
            // Defensive: skip checkpoints the position already passed (can
            // only happen if a hook OSC straddled a segment boundary).
            while cpi < cps.len() && cps[cpi].0 < pos {
                cpi += 1;
            }
            if cpi < cps.len() && cps[cpi].0 > pos {
                target = target.min(cps[cpi].0);
            }
            while smi < seams.len() && seams[smi] <= pos {
                smi += 1;
            }
            if smi < seams.len() && seams[smi] > pos {
                target = target.min(seams[smi]);
            }
            parser.advance(&mut term, &region[pos..target]);
            pos = target;
            if seams.get(smi) == Some(&pos) {
                serialize::exit_alt_at_seam(&mut term, &mut parser);
                smi += 1;
            }
            settle(&mut term, &mut pending, &mut pinned, &mut last_history);
            while cpi < cps.len() && cps[cpi].0 == pos {
                record(&term, cps[cpi].1, &mut pending);
                cpi += 1;
            }
        }
    }
    // End-of-stream alt closure, mirroring scratch_term_with_fix: the replay
    // reconstruction exits a still-open alt region and re-prints its frame
    // as primary lines — the mapping grid must hold the SAME final content
    // or the content-to-content anchor (and every expected row) diverges.
    if term.mode().contains(alacritty_terminal::term::TermMode::ALT_SCREEN) {
        let _ = serialize::alt_frame_fix(&mut term, &mut parser);
        settle(&mut term, &mut pending, &mut pinned, &mut last_history);
    }

    // ── stage 3: place each checkpoint at its COMPUTED replay row ────────
    // The mapping grid and the replay reconstruction hold the SAME final
    // content — the replay is the serializer's emission of the same parse —
    // so a checkpoint's replay row is computed, not searched:
    //
    //     expected = final_row − (non-trailing rows emitted BELOW cp.line)
    //
    // Anchored CONTENT-TO-CONTENT: `final_row` is the deepest non-blank
    // replay row, and distances are measured to the mapping grid's last
    // non-blank emitted line, ignoring both grids' trailing blanks. That
    // makes the mapping invariant to every geometry asymmetry between the
    // two sides: the live mirror is resized to the ATTACHER's grid before
    // serializing while the mapping term follows the journal's last-known
    // geometry (the resize repaint lands later), so their trailing
    // blank-line counts differ by the rows delta — an anchor on the last
    // emitted line or on line counts would shift EVERY hint by that delta.
    // Extra top rows (the mapping ring outlives the replay source's) can't
    // shift it either, and neither can the replay CURSOR's CUU clamp at a
    // shorter client's screen top (field cmd journal: a 51-row grid into a
    // 42-row client pinned the cursor at row 0, 6 rows off).
    //
    // `serialize::emitted_grid_rows` provides the emit decisions (single
    // rule set shared with serialize_term — the two can never drift). Text
    // verification inside ±ROW_TOL then decides; misses DROP, never guess.
    //
    // This replaces a greedy in-order text SEARCH (window up to 400 rows
    // below the tracked line, unbounded for reflow-loosened checkpoints).
    // On the field PS journal that search collapsed wholesale: resize storms
    // loosened nearly every checkpoint, Enter-spam runs and conhost repaint
    // doubles made dozens of rows IDENTICAL ("PS C:\>", "PS C:\> ls"), and
    // first-fit matching let deep old checkpoints steal shallow rows while
    // the monotone search cursor disenfranchised entire spam clusters — the
    // "dead PS sessions' bare prompt rows survive every reopen" bug (cmd
    // journals were short and storm-free, so cmd appeared fixed). One wrong
    // join even spacer-blanked the LIVE prompt row.
    //
    // TC_TRACE_ANCHORS=1: per-checkpoint decision lines on stderr (field
    // diagnosis; never set in production). DEV-ONLY BY DESIGN (N5): the
    // release daemon is windows_subsystem="windows", so stderr goes nowhere
    // there — this output exists only under `cargo test`/console builds,
    // which is exactly where anchor decisions get diagnosed.
    let trace = std::env::var_os("TC_TRACE_ANCHORS").is_some();
    let rterm = replay_term(replay, cols, rows);
    let r_hist = rterm.grid().history_size() as i32;
    let r_rows = rterm.screen_lines() as i32;
    // Trim the emit list at the mapping grid's last non-blank line: the
    // content-to-content anchor (see above).
    let mut emitted = serialize::emitted_grid_rows(&term);
    match emitted.iter().rposition(|&(_, blank)| !blank) {
        Some(lc) => emitted.truncate(lc + 1),
        None => return Vec::new(),
    }
    // The final prompt row: the deepest non-blank replay row (the live
    // prompt, or a dead session's honest last row) — the anchor, and a row
    // hints never claim: a spacer would blank the live prompt, a block
    // cover would swallow re-typed text at it. NOT the cursor row: CUU
    // clamping (see above) can pin the replay cursor rows away from it.
    let final_row = (-r_hist..r_rows)
        .rev()
        .find(|&r| !row_input_blank(&rterm, r, 0))
        .unwrap_or(-r_hist);
    let anchor = final_row;
    let last_idx = emitted.len() as i32 - 1;
    let mut out: Vec<AnchorHint> = Vec::new();
    let mut spacers = 0usize;
    // Monotone floor: stream order ⇒ replay-row order; a tolerance-corrected
    // match may never claim a row at or above an earlier claim.
    let mut last_row = -r_hist - 1;
    if trace {
        eprintln!(
            "[anchors] slots={} pending={} emitted={} anchor={anchor} \
             replay hist={r_hist} rows={r_rows} final_row={final_row}",
            slots.len(),
            pending.len(),
            emitted.len()
        );
    }
    for cp in &pending {
        let slot = &slots[cp.slot];
        // A checkpoint row the serializer dropped (seam-deduped dangling
        // prompt) has no replay row at all; one past the last content line
        // (trailing blanks) can't be a prompt row either.
        let Ok(k) = emitted.binary_search_by_key(&cp.line, |&(r, _)| r) else {
            if trace {
                eprintln!(
                    "[anchors] cp pe={} line={} col={} prefix={:?} -> DROPPED ROW",
                    slot.pe_off, cp.line, cp.col, cp.prefix
                );
            }
            continue;
        };
        let expected = anchor - (last_idx - k as i32);
        let lo = (expected - ROW_TOL).max(-r_hist).max(last_row + 1);
        let hi = (expected + ROW_TOL).min(r_rows - 1);
        let mut r = lo;
        let mut matched = false;
        while r <= hi {
            if row_prefix(&rterm, r, cp.col).as_deref() == Some(cp.prefix.as_str()) {
                if slot.recs.is_empty() {
                    // Spacer: the input area must still be blank (a cancelled
                    // line that kept typed text renders raw — honest), and
                    // the final prompt row is never a spacer — it is the
                    // live prompt or the dead session's honest last row.
                    if !cp.prefix.is_empty() && r < final_row && row_input_blank(&rterm, r, cp.col)
                    {
                        if spacers < MAX_SPACERS {
                            out.push(AnchorHint {
                                start_off: slot.pe_off,
                                row: r,
                                col: cp.col as u32,
                                kind: ANCHOR_SPACER,
                            });
                            spacers += 1;
                        }
                        last_row = r;
                        matched = true;
                        break;
                    }
                } else if r < final_row {
                    // Block: the row must render the recorded command at the
                    // prompt end. Several records can share one prompt row
                    // (exec-exec doubles); each matching one gets a hint.
                    // Never the final row: typed-again text at the live
                    // prompt must not be covered as history.
                    let mut any = false;
                    for ri in slot.recs.clone() {
                        let first = recs[ri].cmd.lines().next().unwrap_or("");
                        if row_text_at(&rterm, r, cp.col, first) {
                            out.push(AnchorHint {
                                start_off: recs[ri].start_off,
                                row: r,
                                col: cp.col as u32,
                                kind: ANCHOR_BLOCK,
                            });
                            any = true;
                        }
                    }
                    if any {
                        last_row = r;
                        matched = true;
                        break;
                    }
                }
            }
            r += 1;
        }
        if trace {
            eprintln!(
                "[anchors] cp pe={} line={} col={} prefix={:?} recs={:?} \
                 expected={expected} window=[{lo},{hi}] -> {}",
                slot.pe_off,
                cp.line,
                cp.col,
                cp.prefix,
                slot.recs,
                if matched {
                    format!("MATCH row {r}")
                } else {
                    "MISS".to_string()
                }
            );
        }
    }

    // ── stage 3b: replay-truth spacer sweep ──────────────────────────────
    // A row that RENDERS as a superseded bare prompt is blanked live by a
    // spacer cover; reopen parity demands the same regardless of whether a
    // checkpoint could place it: conhost repaint doubles carry no 133;B of
    // their own, and reflow-crossed checkpoints drop honestly above. A row
    // qualifies when it shows a KNOWN prompt shape (prefix + column seen at
    // a real 133;B in this tail) with a blank input area, sits above the
    // final prompt row, and no hint claimed it. Hint keys are recycled from
    // spacer slots that produced no hint, then synthesized downward from
    // u64::MAX (unreachable by any real journal offset) — spacer start_offs
    // are opaque everywhere: blocks join records by start_off, spacers
    // never join anything (GUI mints a sig-healed blank cover from row+col
    // alone).
    let shapes: Vec<(&str, usize)> = {
        let mut v: Vec<(&str, usize)> = Vec::new();
        for cp in &pending {
            if !cp.prefix.is_empty() && !v.iter().any(|s| s.0 == cp.prefix && s.1 == cp.col) {
                v.push((cp.prefix.as_str(), cp.col));
            }
        }
        v
    };
    let hinted_spacer_offs: std::collections::HashSet<u64> = out
        .iter()
        .filter(|h| h.kind == ANCHOR_SPACER)
        .map(|h| h.start_off)
        .collect();
    let mut spare_keys = slots
        .iter()
        .filter(|s| s.spacer && !hinted_spacer_offs.contains(&s.pe_off))
        .map(|s| s.pe_off);
    if !shapes.is_empty() {
        let claimed: std::collections::HashSet<i32> = out.iter().map(|h| h.row).collect();
        let sweep_hi = final_row.min(r_rows);
        let mut synth_key = u64::MAX;
        let mut r = -r_hist;
        while r < sweep_hi && spacers < MAX_SPACERS {
            if !claimed.contains(&r) {
                for &(prefix, col) in &shapes {
                    if row_prefix(&rterm, r, col).as_deref() == Some(prefix)
                        && row_input_blank(&rterm, r, col)
                    {
                        let key = spare_keys.next().unwrap_or_else(|| {
                            synth_key -= 1;
                            synth_key
                        });
                        if trace {
                            eprintln!("[anchors] sweep spacer row {r} col {col} {prefix:?}");
                        }
                        out.push(AnchorHint {
                            start_off: key,
                            row: r,
                            col: col as u32,
                            kind: ANCHOR_SPACER,
                        });
                        spacers += 1;
                        break;
                    }
                }
            }
            r += 1;
        }
    }
    out.sort_by_key(|a| (a.row, a.start_off));
    out
}

/// Scan the region for hook events: (absolute offset past the terminator,
/// verb class). Token contents are irrelevant here — block hints only emit
/// where a PERSISTED record (token-checked at ingest) joins the slot, and
/// spacers are re-verified against the replay text, the same trust stance as
/// the GUI's own live scan.
enum EvKind {
    Pe,
    Exec,
    Pre,
}

fn scan_events(region: &[u8], region_base: u64) -> Vec<(u64, EvKind)> {
    let mut sc = BlockScanner::new();
    let mut events = Vec::new();
    let mut abs = region_base;
    for chunk in region.chunks(CHUNK) {
        for ev in sc.feed(chunk) {
            let kind = match ev.verb {
                HookVerb::PromptEnd => EvKind::Pe,
                HookVerb::Exec { .. } => EvKind::Exec,
                HookVerb::Pre { .. } => EvKind::Pre,
                // D* 133;A is a live-close anchor only; restore slots stay
                // keyed on the 133;B prompt-end rows.
                HookVerb::Init { .. }
                | HookVerb::Beacon { .. }
                | HookVerb::PromptStart => continue,
            };
            events.push((abs + ev.offset_in_chunk as u64, kind));
        }
        abs += chunk.len() as u64;
    }
    events
}

/// Build prompt slots from the event stream (stage 1 classification).
fn build_slots(events: &[(u64, EvKind)], recs: &[BlockRec]) -> Vec<Slot> {
    let mut slots = Vec::new();
    for (i, (off, kind)) in events.iter().enumerate() {
        if !matches!(kind, EvKind::Pe) {
            continue;
        }
        let next_pe = events[i + 1..]
            .iter()
            .find(|(_, k)| matches!(k, EvKind::Pe))
            .map(|(o, _)| *o)
            .unwrap_or(u64::MAX);
        let lo = recs.partition_point(|r| r.start_off < *off);
        let hi = recs.partition_point(|r| r.start_off < next_pe);
        let mut has_pre = false;
        let mut has_exec = false;
        for (o, k) in &events[i + 1..] {
            if *o >= next_pe {
                break;
            }
            match k {
                EvKind::Pre => has_pre = true,
                EvKind::Exec => has_exec = true,
                EvKind::Pe => break,
            }
        }
        if lo < hi {
            slots.push(Slot {
                pe_off: *off,
                recs: lo..hi,
                spacer: false,
            });
        } else if has_pre && !has_exec {
            // Superseded bare prompt: a fresh prompt replaced it with no
            // command run (empty Enter, Ctrl+C at a prompt). The FINAL
            // prompt (no following pre) is never a spacer — it is the live
            // prompt (composer/PromptState territory) or the dead session's
            // honest last row (dedupe territory).
            slots.push(Slot {
                pe_off: *off,
                recs: 0..0,
                spacer: true,
            });
        }
    }
    // Keep the newest MAX_SPACERS spacer slots (drop oldest extras).
    let spacer_total = slots.iter().filter(|s| s.spacer).count();
    if spacer_total > MAX_SPACERS {
        let mut drop_n = spacer_total - MAX_SPACERS;
        slots.retain(|s| {
            if s.spacer && drop_n > 0 {
                drop_n -= 1;
                false
            } else {
                true
            }
        });
    }
    slots
}

/// Pin the odometer: display offset exactly 1 (requires ≥1 history row).
fn repin(term: &mut Term<EventProxy>) -> bool {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        return false;
    }
    if term.grid().history_size() == 0 {
        return false;
    }
    let cur = term.grid().display_offset() as i32;
    term.grid_mut().scroll_display(Scroll::Delta(1 - cur));
    true
}

/// Post-slice odometer accounting: shift every pending checkpoint by exactly
/// the rows that scrolled during the slice.
fn settle(
    term: &mut Term<EventProxy>,
    pending: &mut Vec<Cp>,
    pinned: &mut bool,
    last_history: &mut usize,
) {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        // The primary grid is frozen while alt is active, and display_offset
        // would read the ALT grid's — defer to the next primary slice (the
        // primary offset accumulates across the alt region untouched).
        return;
    }
    let h = term.grid().history_size();
    if !*pinned {
        // Pre-pin phase: history delta is exact (PREPIN_CHUNK bounds a
        // slice's scroll far below the ring cap).
        if h < *last_history {
            // ED3-class shrink: scrollback gone; screen rows keep their
            // coordinates.
            pending.retain(|c| c.line >= 0);
        } else if h > *last_history {
            let d = (h - *last_history) as i32;
            for c in pending.iter_mut() {
                c.line -= d;
            }
        }
        *last_history = h;
        *pinned = repin(term);
    } else {
        let d = term.grid().display_offset();
        if d == 0 {
            // ED3/RIS reset the offset: the shift since the last settle is
            // unknowable and the history it tracked is gone — drop all.
            pending.clear();
            *last_history = h;
            *pinned = repin(term);
        } else {
            let shift = d.saturating_sub(1);
            if d >= MAP_HISTORY {
                // Odometer cap: shift under-counted, but everything pending
                // is ≥ cap rows deep — far beyond any replay window.
                pending.clear();
            } else if shift > 0 {
                for c in pending.iter_mut() {
                    c.line -= shift as i32;
                }
            }
            if shift > 0 {
                term.grid_mut().scroll_display(Scroll::Delta(-(shift as i32)));
            }
            *last_history = h;
        }
    }
    let h = term.grid().history_size() as i32;
    pending.retain(|c| c.line >= -h);
}

/// Record a checkpoint at the current cursor (the feed is paused exactly at
/// the 133;B, so the cursor sits at the prompt end — the same contract
/// PromptState and the GUI's capture_prompt_end rely on).
fn record(term: &Term<EventProxy>, slot_idx: usize, pending: &mut Vec<Cp>) {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        return; // a hook inside a TUI frame is not a prompt row
    }
    let cur = term.grid().cursor.point;
    let col = cur.column.0;
    let prefix = row_prefix(term, cur.line.0, col).unwrap_or_default();
    pending.push(Cp {
        slot: slot_idx,
        line: cur.line.0,
        col,
        prefix,
    });
}

/// Parse the replay bytes into the exact grid the client will reconstruct.
fn replay_term(replay: &[u8], cols: u16, rows: u16) -> Term<EventProxy> {
    let nl = memchr::memchr_iter(b'\n', replay).count();
    let hist = (nl + rows as usize + 8).min(12_000);
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut term = Term::new(
        alacritty_terminal::term::Config {
            scrolling_history: hist,
            ..Default::default()
        },
        &TermSize::new(cols.clamp(2, 1000) as usize, rows.clamp(2, 1000) as usize),
        EventProxy::new(tx),
    );
    let mut parser = ImmediateProcessor::new();
    parser.advance(&mut term, replay);
    term
}

/// Row text left of `col`, trim-end, NULs as spaces, wide-char spacers
/// skipped — identical semantics to the GUI's `row_prefix_text`, so the
/// prefix captured in the mapping grid compares 1:1 against the replay grid
/// AND against what the GUI's own self-heal will read.
fn row_prefix(term: &Term<EventProxy>, line: i32, col: usize) -> Option<String> {
    let grid = term.grid();
    let history = grid.history_size() as i32;
    let rows = grid.screen_lines() as i32;
    if line < -history || line >= rows {
        return None;
    }
    let cols = grid.columns();
    let row = &grid[Line(line)];
    let mut s = String::with_capacity(col.min(cols));
    for c in 0..col.min(cols) {
        let cell = &row[Column(c)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        s.push(if cell.c == '\0' { ' ' } else { cell.c });
    }
    Some(s.trim_end().to_string())
}

/// GUI `row_has_text_at` semantics: `expect` must render from `col`; running
/// off the row's right edge with every compared cell matching counts (a long
/// command wraps — the visible part shows no less than recorded). Empty
/// `expect` never matches.
fn row_text_at(term: &Term<EventProxy>, line: i32, col: usize, expect: &str) -> bool {
    if expect.is_empty() {
        return false;
    }
    let grid = term.grid();
    let history = grid.history_size() as i32;
    let rows = grid.screen_lines() as i32;
    let cols = grid.columns();
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
            None => return true,
            Some(_) => return false,
        }
    }
    true
}

/// The input area right of `col` holds nothing (spaces/NULs only).
fn row_input_blank(term: &Term<EventProxy>, line: i32, col: usize) -> bool {
    let grid = term.grid();
    let history = grid.history_size() as i32;
    let rows = grid.screen_lines() as i32;
    if line < -history || line >= rows {
        return false;
    }
    let cols = grid.columns();
    let row = &grid[Line(line)];
    (col.min(cols)..cols).all(|c| {
        let ch = row[Column(c)].c;
        ch == ' ' || ch == '\0'
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOK: &str = "0123456789abcdef";

    fn hook(verb: &str, json: &str) -> Vec<u8> {
        let hex = crate::strip::hex_lower(json.as_bytes());
        format!("\x1b]7717;{TOK};{verb};{hex}\x07").into_bytes()
    }

    fn pe() -> Vec<u8> {
        b"\x1b]133;B\x07".to_vec()
    }

    fn rec(start: u64, end: u64, cmd: &str) -> BlockRec {
        BlockRec {
            epoch: 1,
            n: 0,
            cmd: cmd.into(),
            cwd: Some(std::path::PathBuf::from("C:\\")),
            exit: Some(0),
            started_ms: 0,
            ended_ms: Some(1),
            start_off: start,
            end_off: Some(end),
            truncated: false,
        }
    }

    /// A synthetic hooked-shell stream builder: prompts render `PS C:\> `,
    /// commands echo at the prompt end, exec/pre hooks bracket output —
    /// journal-faithful ordering. Returns (stream, recs).
    struct Shell {
        s: Vec<u8>,
        recs: Vec<BlockRec>,
    }

    impl Shell {
        fn new() -> Self {
            Self { s: Vec::new(), recs: Vec::new() }
        }

        fn prompt(&mut self) -> &mut Self {
            self.s.extend(hook("pre", r#"{"e":0,"n":1,"d":"C:\\"}"#));
            self.s.extend_from_slice(b"\x1b]133;A\x07PS C:\\> ");
            self.s.extend(pe());
            self
        }

        /// Type + run a command through the exec hook (pwsh shape).
        fn run(&mut self, cmd: &str, output: &str) -> &mut Self {
            self.s.extend_from_slice(cmd.as_bytes());
            self.s.extend_from_slice(b"\r\n");
            let start = self.s.len() as u64;
            self.s
                .extend(hook("exec", &format!(r#"{{"c":"{cmd}"}}"#)));
            // exec offset points past its terminator == where we snapshot.
            let start_off = self.s.len() as u64;
            let _ = start;
            self.s.extend_from_slice(output.as_bytes());
            let end_off = self.s.len() as u64;
            self.recs
                .push(rec(start_off, end_off, cmd));
            self
        }

        /// Empty Enter at the prompt: newline, then the next prompt's pre
        /// supersedes the bare one (the spacer shape).
        fn empty_enter(&mut self) -> &mut Self {
            self.s.extend_from_slice(b"\r\n");
            self
        }

        fn flood(&mut self, lines: usize) -> &mut Self {
            for i in 0..lines {
                self.s
                    .extend_from_slice(format!("flood line {i}\r\n").as_bytes());
            }
            self
        }
    }

    fn hints_for(shell: &Shell, cols: u16, rows: u16) -> (Vec<AnchorHint>, Term<EventProxy>) {
        let replay =
            serialize::serialize_dead(&shell.s, cols, rows).expect("primary tail serializes");
        let hints = compute_hints(&shell.s, 0, &replay, &shell.recs, cols, rows);
        (hints, replay_term(&replay, cols, rows))
    }

    fn row_string(term: &Term<EventProxy>, line: i32) -> String {
        let grid = term.grid();
        let cols = grid.columns();
        let row = &grid[Line(line)];
        let mut s = String::new();
        for c in 0..cols {
            let cell = &row[Column(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            s.push(if cell.c == '\0' { ' ' } else { cell.c });
        }
        s.trim_end().to_string()
    }

    /// The core contract: every record's hint row renders `PS C:\> <cmd>` in
    /// the replay grid at the hinted column, and a superseded bare prompt
    /// yields a spacer hint on a bare-prompt row.
    #[test]
    fn blocks_and_spacers_map_to_replay_rows() {
        let mut sh = Shell::new();
        sh.prompt().run("echo hi", "hi\r\n");
        sh.prompt().empty_enter();
        sh.prompt().run("dir", "file-a\r\nfile-b\r\n");
        sh.prompt(); // final live prompt: never hinted
        let (hints, rt) = hints_for(&sh, 60, 12);

        let blocks: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_BLOCK).collect();
        assert_eq!(blocks.len(), 2, "both records hinted: {hints:?}");
        for (h, cmd) in blocks.iter().zip(["echo hi", "dir"]) {
            let text = row_string(&rt, h.row);
            assert_eq!(
                text,
                format!("PS C:\\> {cmd}"),
                "hint row must render the prompt+command"
            );
            assert_eq!(h.col, 8, "prompt end column");
            assert!(row_text_at(&rt, h.row, h.col as usize, cmd));
        }
        // Rec join keys are the record start_offs.
        assert_eq!(blocks[0].start_off, sh.recs[0].start_off);
        assert_eq!(blocks[1].start_off, sh.recs[1].start_off);

        let spacers: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_SPACER).collect();
        assert_eq!(spacers.len(), 1, "one superseded bare prompt: {hints:?}");
        let s = spacers[0];
        assert_eq!(row_string(&rt, s.row), "PS C:\\>");
        assert!(row_input_blank(&rt, s.row, s.col as usize));
        // The final live prompt is NOT a spacer — its row is below the last
        // spacer and carries no hint.
        assert!(hints.iter().all(|h| h.row < rt.grid().cursor.point.line.0));
        // Rows are strictly ordered like the stream.
        let mut rows: Vec<i32> = hints.iter().map(|h| h.row).collect();
        let sorted = {
            let mut r = rows.clone();
            r.sort();
            r
        };
        rows.sort();
        assert_eq!(rows, sorted);
    }

    /// cmd-family shape: no exec hook ever; the synthetic record's start_off
    /// is the journal head at submission (after the prompt rendered, before
    /// the echo). The slot rule (start_off ∈ [133;B, next 133;B)) must anchor
    /// it to the right prompt row, and a typed row must NOT be classified as
    /// a spacer just because the family has no exec events.
    #[test]
    fn cmd_family_synthetic_records_map_without_exec() {
        let mut s = Vec::new();
        let mut recs = Vec::new();
        // prompt 1 (cmd's PROMPT macro: pre then prompt text then 133;B).
        s.extend(hook("pre", r#"{"e":null,"n":0,"d":"C:\\work"}"#));
        s.extend_from_slice(b"C:\\work>");
        s.extend(pe());
        // SubmitCommand head: before the echo.
        let start_off = s.len() as u64;
        s.extend_from_slice(b"dir /b\r\nCargo.toml\r\n");
        let end_off = s.len() as u64;
        let mut r = rec(start_off, end_off, "dir /b");
        r.exit = None; // D7: cmd never reports exit codes
        recs.push(r);
        // prompt 2 closes it.
        s.extend(hook("pre", r#"{"e":null,"n":0,"d":"C:\\work"}"#));
        s.extend_from_slice(b"C:\\work>");
        s.extend(pe());

        let replay = serialize::serialize_dead(&s, 60, 10).expect("serializes");
        let hints = compute_hints(&s, 0, &replay, &recs, 60, 10);
        let rt = replay_term(&replay, 60, 10);
        assert_eq!(hints.len(), 1, "one block hint, no spacers: {hints:?}");
        let h = &hints[0];
        assert_eq!(h.kind, ANCHOR_BLOCK);
        assert_eq!(h.start_off, start_off);
        assert_eq!(h.col, 8, "cmd prompt has no trailing space: col 8");
        assert_eq!(row_string(&rt, h.row), "C:\\work>dir /b");
    }

    /// The odometer through ring saturation: a flood longer than MAP_HISTORY
    /// between two blocks evicts the first block's row from every window;
    /// the second block still maps EXACTLY (the pinned display_offset kept
    /// counting where the history-delta trick goes blind).
    #[test]
    fn saturation_keeps_late_blocks_exact_and_drops_evicted() {
        let mut sh = Shell::new();
        sh.prompt().run("echo before", "gone\r\n");
        sh.flood(MAP_HISTORY + 500);
        sh.prompt().run("echo after", "kept\r\n");
        sh.prompt();
        let (hints, rt) = hints_for(&sh, 60, 12);
        let blocks: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_BLOCK).collect();
        assert_eq!(blocks.len(), 1, "pre-flood block evicted: {hints:?}");
        let h = blocks[0];
        assert_eq!(h.start_off, sh.recs[1].start_off, "the post-flood record");
        assert_eq!(row_string(&rt, h.row), "PS C:\\> echo after");
    }

    /// ED3 (clear-scrollback) between blocks: everything recorded before it
    /// is untrackable AND gone from the grid — dropped; blocks after map.
    #[test]
    fn ed3_drops_pre_clear_hints() {
        let mut sh = Shell::new();
        sh.prompt().run("echo old", "x\r\n");
        sh.flood(40); // push the old block into scrollback
        sh.s.extend_from_slice(b"\x1b[H\x1b[2J\x1b[3J"); // cls + clear scrollback
        sh.prompt().run("echo new", "y\r\n");
        sh.prompt();
        let (hints, rt) = hints_for(&sh, 60, 12);
        let blocks: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_BLOCK).collect();
        assert_eq!(blocks.len(), 1, "pre-ED3 block dropped: {hints:?}");
        assert_eq!(blocks[0].start_off, sh.recs[1].start_off);
        assert_eq!(row_string(&rt, blocks[0].row), "PS C:\\> echo new");
    }

    /// A mid-tail WINSZ resize (cols reflow): short rows survive the reflow
    /// unwrapped, so checkpoints before it still land (history-delta shift
    /// is exact when nothing re-wraps); checkpoints after stay exact. No
    /// hint may point at a row that fails its own verification (the §8.1
    /// bar, enforced here by re-reading every hinted row).
    #[test]
    fn winsz_resize_mid_tail_keeps_hints_verifiable() {
        let mut sh = Shell::new();
        sh.prompt().run("echo one", "a\r\n");
        // Conhost resize repaint stamp: same rows, narrower cols (no wrap of
        // our short rows — content survives reflow).
        sh.s.extend_from_slice(b"\x1b[8;12;50t");
        sh.prompt().run("echo two", "b\r\n");
        sh.prompt();
        let (hints, rt) = hints_for(&sh, 50, 12);
        let blocks: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_BLOCK).collect();
        assert!(
            blocks.iter().any(|h| h.start_off == sh.recs[1].start_off),
            "post-resize block must map: {hints:?}"
        );
        for h in &hints {
            if h.kind == ANCHOR_BLOCK {
                let ri = sh
                    .recs
                    .iter()
                    .position(|r| r.start_off == h.start_off)
                    .unwrap();
                assert!(
                    row_text_at(&rt, h.row, h.col as usize, &sh.recs[ri].cmd),
                    "every emitted hint must verify on its replay row"
                );
            }
        }
    }

    /// Identical commands stay distinct: three `echo hi` blocks get three
    /// hints on three different rows, in order, each verifying.
    #[test]
    fn identical_commands_map_to_distinct_rows() {
        let mut sh = Shell::new();
        for _ in 0..3 {
            sh.prompt().run("echo hi", "hi\r\n");
        }
        sh.prompt();
        let (hints, rt) = hints_for(&sh, 60, 20);
        let blocks: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_BLOCK).collect();
        assert_eq!(blocks.len(), 3);
        let mut seen = std::collections::HashSet::new();
        for (h, r) in blocks.iter().zip(&sh.recs) {
            assert_eq!(h.start_off, r.start_off, "in-order join");
            assert!(seen.insert(h.row), "distinct rows");
            assert_eq!(row_string(&rt, h.row), "PS C:\\> echo hi");
        }
    }

    /// THE field PS bug (2026-07-04, user journal): Enter-spam runs and
    /// conhost repaint doubles make dozens of replay rows IDENTICAL bare
    /// prompts; the old greedy in-order text search let one session's
    /// checkpoints steal another session's rows and then disenfranchised
    /// whole runs — reopened GUIs showed dead PS sessions' bare `PS C:\>`
    /// rows raw. Computed placement + the replay-truth sweep must cover
    /// EVERY superseded bare-prompt row across sessions, join every block
    /// in stream order, and never touch the final prompt row.
    #[test]
    fn spam_runs_and_repaint_doubles_cover_without_cross_session_steals() {
        let mut sh = Shell::new();
        // Session 1: identical commands around an Enter-spam run.
        sh.prompt().run("ls", "f1\r\nf2\r\n");
        for _ in 0..6 {
            sh.prompt().empty_enter();
        }
        sh.prompt().run("ls", "f1\r\nf2\r\n");
        // A conhost repaint DOUBLE of a bare prompt: rendered cells only —
        // repaints re-emit screen text, never the OSC hooks.
        sh.s.extend_from_slice(b"PS C:\\> \r\n");
        // Dead session's dangling prompt + the daemon's journal-only seam
        // (mod.rs launch: CRLF + concealed sentinel + pad(rows) + home).
        sh.prompt();
        sh.s.extend_from_slice(
            format!("\r\n\x1b[8m{}\x1b[28m", serialize::SEAM_SENTINEL).as_bytes(),
        );
        sh.s.extend(std::iter::repeat_n(b"\r\n".as_slice(), 12).flatten());
        sh.s.extend_from_slice(b"\x1b[H");
        // Session 2: its own spam run + the same command text again.
        sh.prompt().empty_enter();
        sh.prompt().empty_enter();
        sh.prompt().run("ls", "f1\r\nf2\r\n");
        sh.prompt(); // live prompt
        let (hints, rt) = hints_for(&sh, 60, 12);

        // Every rendered bare-prompt row above the final prompt row must be
        // spacer-hinted (checkpoint or sweep); the final prompt row must
        // carry no hint of any kind.
        let hist = rt.grid().history_size() as i32;
        let rows = rt.screen_lines() as i32;
        let final_row = (-hist..rows)
            .rev()
            .find(|&r| !row_string(&rt, r).is_empty())
            .expect("content exists");
        assert_eq!(row_string(&rt, final_row), "PS C:\\>", "live prompt is last");
        let hinted: std::collections::HashSet<i32> = hints.iter().map(|h| h.row).collect();
        for r in -hist..final_row {
            if row_string(&rt, r) == "PS C:\\>" {
                assert!(
                    hinted.contains(&r),
                    "bare prompt row {r} left raw: {hints:?}"
                );
            }
        }
        assert!(
            hints.iter().all(|h| h.row != final_row),
            "final prompt row must never be hinted: {hints:?}"
        );

        // All three identical-command records join in stream order, each on
        // a distinct verifying row.
        let blocks: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_BLOCK).collect();
        assert_eq!(blocks.len(), 3, "{hints:?}");
        for (h, r) in blocks.iter().zip(&sh.recs) {
            assert_eq!(h.start_off, r.start_off, "stream-order join");
            assert_eq!(row_string(&rt, h.row), "PS C:\\> ls");
        }
    }

    /// The field cmd-journal anchor trap: the stream carries its own TALLER
    /// geometry (XTWINOPS stamp), so the serialized CUU cursor trailer
    /// clamps at a shorter client's screen top — anchoring on the replay
    /// cursor lands rows off. The line-count anchor must still place blocks
    /// exactly.
    #[test]
    fn taller_stream_geometry_clamped_client_still_anchors() {
        let mut sh = Shell::new();
        // Conhost repaint stamp: 30 rows; content stays near the top.
        sh.s.extend_from_slice(b"\x1b[8;30;60t");
        sh.prompt().run("echo deep", "out\r\n");
        sh.prompt(); // live prompt on row ~2 of a 30-row screen
        let (hints, rt) = hints_for(&sh, 60, 12); // 12-row client clamps CUU
        let blocks: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_BLOCK).collect();
        assert_eq!(blocks.len(), 1, "{hints:?}");
        assert_eq!(blocks[0].start_off, sh.recs[0].start_off);
        assert_eq!(row_string(&rt, blocks[0].row), "PS C:\\> echo deep");
    }

    /// Live-attach geometry asymmetry: the mirror is resized to the
    /// ATTACHER's grid before serializing, but the mapping term follows the
    /// journal's last-known geometry (the resize repaint lands later) — the
    /// two sides' trailing blank-line counts differ by the rows delta (8
    /// here, far past ROW_TOL). The content-to-content anchor must keep
    /// every hint exact regardless.
    #[test]
    fn replay_from_attacher_resized_mirror_still_maps() {
        let mut sh = Shell::new();
        // The journal's own geometry: a 12-row conhost stamp up front.
        sh.s.extend_from_slice(b"\x1b[8;12;60t");
        sh.prompt().run("echo asym", "one\r\n");
        sh.prompt().empty_enter();
        sh.prompt().run("echo more", "two\r\n");
        sh.prompt();
        // The live mirror: parsed at journal geometry, then attach-resized
        // to the attacher's 20 rows BEFORE serializing (daemon do_resize);
        // the resize repaint never reaches the journal snapshot.
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut mirror = Term::new(
            alacritty_terminal::term::Config {
                scrolling_history: 2000,
                ..Default::default()
            },
            &TermSize::new(60, 12),
            EventProxy::new(tx),
        );
        let mut parser = ImmediateProcessor::new();
        parser.advance(&mut mirror, &sh.s);
        serialize::resize_conhost(&mut mirror, 60, 20);
        let replay = serialize::serialize_term(&mirror, None);
        let hints = compute_hints(&sh.s, 0, &replay, &sh.recs, 60, 20);
        let rt = replay_term(&replay, 60, 20);
        let blocks: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_BLOCK).collect();
        assert_eq!(blocks.len(), 2, "{hints:?}");
        assert_eq!(row_string(&rt, blocks[0].row), "PS C:\\> echo asym");
        assert_eq!(row_string(&rt, blocks[1].row), "PS C:\\> echo more");
        let spacers: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_SPACER).collect();
        assert_eq!(spacers.len(), 1, "{hints:?}");
        assert_eq!(row_string(&rt, spacers[0].row), "PS C:\\>");
    }

    /// Hooks inside an alt-screen region never produce hints, and the alt
    /// region does not corrupt the primary-row mapping around it.
    #[test]
    fn alt_screen_hooks_are_skipped() {
        let mut sh = Shell::new();
        sh.prompt().run("echo pre-alt", "x\r\n");
        // A TUI runs: enters alt, emits a stray 133;B + pre inside, exits.
        sh.s.extend_from_slice(b"\x1b[?1049h\x1b[HTUI FRAME");
        sh.s.extend(pe());
        sh.s.extend(hook("pre", r#"{"e":0,"n":9,"d":"C:\\"}"#));
        sh.s.extend_from_slice(b"\x1b[?1049l");
        sh.prompt().run("echo post-alt", "y\r\n");
        sh.prompt();
        let (hints, rt) = hints_for(&sh, 60, 12);
        let blocks: Vec<_> = hints.iter().filter(|h| h.kind == ANCHOR_BLOCK).collect();
        assert_eq!(blocks.len(), 2, "{hints:?}");
        assert_eq!(row_string(&rt, blocks[0].row), "PS C:\\> echo pre-alt");
        assert_eq!(row_string(&rt, blocks[1].row), "PS C:\\> echo post-alt");
        // The stray in-alt 133;B produced nothing (the pre after it belongs
        // to the alt region; no spacer may exist here).
        assert!(hints.iter().all(|h| h.kind == ANCHOR_BLOCK), "{hints:?}");
    }

    /// Field-journal harness (env-gated, no-op in CI): point TC_HP_JOURNAL
    /// at a real journal file (its `<id>.blocks.json` sidecar next to it) to
    /// compute hints exactly like a dead attach would and verify every
    /// emitted hint's row against the replay reconstruction. Report-only for
    /// coverage; hard-asserts the §8.1 bar for what IS emitted. Run with
    /// `--nocapture`.
    #[test]
    fn field_journal_hints_verify() {
        let Ok(path) = std::env::var("TC_HP_JOURNAL") else {
            return;
        };
        let raw = std::fs::read(&path).expect("journal readable");
        let side = std::path::Path::new(&path).with_extension("blocks.json");
        #[derive(serde::Deserialize)]
        struct Sidecar {
            #[allow(dead_code)]
            epoch: u32,
            base: u64,
            recs: Vec<BlockRec>,
        }
        let side: Sidecar = serde_json::from_slice(
            &std::fs::read(&side).expect("sidecar next to the journal"),
        )
        .expect("sidecar parses");
        // Dead-attach shape: tail = the whole file here; its first byte's
        // absolute offset is the sidecar's compaction base.
        let (cols, rows) = (160u16, 42u16);
        let replay = serialize::serialize_dead(&raw, cols, rows).expect("primary tail");
        let hints = compute_hints(&raw, side.base, &replay, &side.recs, cols, rows);
        let rt = replay_term(&replay, cols, rows);
        let blocks = hints.iter().filter(|h| h.kind == ANCHOR_BLOCK).count();
        let spacers = hints.len() - blocks;
        println!(
            "recs={} hints: blocks={blocks} spacers={spacers}",
            side.recs.len()
        );
        for h in &hints {
            let text = row_string(&rt, h.row);
            if h.kind == ANCHOR_BLOCK {
                let rec = side
                    .recs
                    .iter()
                    .find(|r| r.start_off == h.start_off)
                    .expect("block hint joins a rec");
                println!(
                    "  block row {:>5} col {:>3} | {:?} <- {:?}",
                    h.row, h.col, text, rec.cmd
                );
                assert!(
                    row_text_at(&rt, h.row, h.col as usize, rec.cmd.lines().next().unwrap_or("")),
                    "hinted row must render the command: row {} {text:?} cmd {:?}",
                    h.row,
                    rec.cmd
                );
            } else {
                println!("  spacer row {:>5} col {:>3} | {text:?}", h.row, h.col);
                assert!(
                    row_input_blank(&rt, h.row, h.col as usize),
                    "spacer row must have a blank input area: {text:?}"
                );
            }
        }
    }

    /// Bare-prompt coverage tracer (env-gated, no-op in CI): TC_HP_COVERAGE=
    /// <journal path> ⇒ list every replay-grid row that renders as a bare
    /// prompt (promptish text, blank input area) and whether a spacer/block
    /// hint targets it. Unhinted bare-prompt rows are exactly what a reopened
    /// GUI shows raw. Report-only — pair with `--nocapture`.
    #[test]
    fn field_journal_bare_prompt_coverage() {
        let Ok(path) = std::env::var("TC_HP_COVERAGE") else {
            return;
        };
        let raw = std::fs::read(&path).expect("journal readable");
        let side = std::path::Path::new(&path).with_extension("blocks.json");
        #[derive(serde::Deserialize)]
        struct Sidecar {
            #[allow(dead_code)]
            epoch: u32,
            base: u64,
            recs: Vec<BlockRec>,
        }
        let side: Sidecar = serde_json::from_slice(
            &std::fs::read(&side).expect("sidecar next to the journal"),
        )
        .expect("sidecar parses");
        let (cols, rows) = (160u16, 42u16);
        let replay = serialize::serialize_dead(&raw, cols, rows).expect("primary tail");
        let hints = compute_hints(&raw, side.base, &replay, &side.recs, cols, rows);
        let rt = replay_term(&replay, cols, rows);
        let hinted: std::collections::HashSet<i32> = hints.iter().map(|h| h.row).collect();
        let hist = rt.grid().history_size() as i32;
        let screen = rt.screen_lines() as i32;
        let mut unhinted = 0usize;
        for line in -hist..screen {
            let text = row_string(&rt, line);
            let t = text.trim_end();
            // Bare-prompt shape: short promptish row, nothing typed after it.
            let bare = !t.is_empty()
                && t.len() < 100
                && t.ends_with(['>', '$', '#', '%'])
                && (t.starts_with("PS ") || !t.contains(' '));
            if !bare {
                continue;
            }
            let mark = if hinted.contains(&line) {
                "HINTED  "
            } else {
                unhinted += 1;
                "UNHINTED"
            };
            println!("  {mark} row {line:>5} | {t:?}");
        }
        println!(
            "bare prompt rows unhinted: {unhinted} (hints total: {}, recs: {})",
            hints.len(),
            side.recs.len()
        );
    }

    /// Empty inputs are cheap no-ops.
    #[test]
    fn empty_inputs_yield_nothing() {
        assert!(compute_hints(b"", 0, b"x", &[], 80, 24).is_empty());
        assert!(compute_hints(b"x", 0, b"", &[], 80, 24).is_empty());
        assert!(compute_hints(b"plain text no hooks", 0, b"plain", &[], 80, 24).is_empty());
    }
}
