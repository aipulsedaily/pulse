# P2 "Blocks UI" — Implementation Spec (final, implementation-ready)

Target: C:\Terminal Control (egui 0.35 GUI + daemon). P1 shipped daemon-side block records
(`BlockRec` keyed to ABSOLUTE journal offsets, `D2C::Blocks` full-on-attach + incremental,
GUI store-only). P2 makes them visible and actionable, mouse-first, Warp-clean.

Everything below is ordered as the implementation plan: protocol → daemon → GUI backend
(anchoring) → GUI store/actions → term_view chrome → header/panel → tests/probes → checklist.
Each decision carries a one-line justification. Open questions are at the end with defaults.

---

## 0. Non-negotiable invariants (violating any of these is a bug)

1. **Mirror purity / parser purity**: nothing is ever injected into any VT parser stream —
   GUI anchoring is *observation only* (scan + split-feed the same bytes, byte-identical
   parse result).
2. **bincode compat**: new protocol variants appended at the very END of `C2D`/`D2C` only;
   no fields added to existing variants. `DaemonInfo.proto` bumps 1 → 2.
3. **Hookless sessions cost zero**: a session with no block records must render at exactly
   today's cost — every new code path is behind an `Option` that is `None` for them.
4. **Alt-screen ⇒ no block chrome of any kind** (separators, gutter, chips, toolbar).
5. **No extra repaints**: block chrome never calls `request_repaint*` except via
   `animate_value_with_time` for the (optional) jump flash; never during sync-deferred frames.
6. **Hot cell loop untouched**: the per-cell render loop in `term_view::render` gains zero
   new work; all chrome is drawn from a sorted anchor index, O(log n + visible blocks).

---

## 1. Protocol (src/protocol.rs)

### 1.1 New variants — appended at enum END (bincode is positional)

```rust
// C2D — append AFTER `DebugDump` (current last variant is DebugDump; Shutdown/Ping are
// mid-enum — order in source == wire index, do not reorder anything):
    /// Ask for one block's output text: journal bytes start_off..end_off (or ..head for an
    /// open block), ANSI/OSC-stripped, size-capped. Answered with D2C::BlockText to the
    /// requesting client only. Unknown start_off is logged and silently dropped.
    BlockText { id: Uuid, start_off: u64 },
```

```rust
// D2C — append AFTER `Blocks`, in this order:
    /// Sent immediately after EVERY Replay (attach and restore-resync): the absolute
    /// journal stream offset at which live Output frames resume. The GUI anchors block
    /// records to grid rows by counting Output bytes from this base. Captured under the
    /// same journal lock as the Replay snapshot, so it is exact.
    StreamPos { id: Uuid, off: u64 },
    /// Reply to C2D::BlockText.
    BlockText { id: Uuid, start_off: u64, text: String, truncated: bool },
```

Why `StreamPos` and not offsets on `Output`: adding a field to `Output` breaks bincode
positional decoding of every existing frame; one absolute base + contiguity is sufficient
because `fanout` is only ever called under the journal lock with exactly the appended bytes
(ingest + emit_output), so Output frames are a gapless suffix of the journal stream.
Known divergence (accepted): a failed journal append (disk-full) still fans out — offsets
drift only in an already-reported disk-full state; cosmetic.

Why per-block `BlockText` pull (not pushing output with recs): block output can be MBs;
records must stay tiny (500 recs × full sync on every attach).

### 1.2 Version gate

- `DaemonInfo { proto: 2 }` (write site: `daemon::run()` where daemon.json is produced).
- `ipc.rs`: plumb `proto` into `IpcClient` (`pub proto: u32`, from the parsed `DaemonInfo`
  in `try_connect`); update the skew warning text ("predates blocks UI (P2); restart the
  daemon from this build").
- GUI behavior on `proto < 2`: never send `BlockText`; anchors never form (no `StreamPos`
  arrives) so in-grid chrome self-degrades; the blocks panel still lists records with
  Copy-command and Re-run (both need no new protocol). Justification: single-exe ships both
  roles, skew is transient; degrade must be silent, not broken (an old daemon would DROP the
  client on an undecodable C2D frame).

---

## 2. Daemon changes (src/daemon/mod.rs, journal.rs, blocks.rs, + new src/strip.rs)

### 2.1 `pub mod blocks;`

Change `mod blocks;` → `pub mod blocks;` in daemon/mod.rs (and `mod daemon;` in main.rs stays
as-is — root-declared modules are crate-visible). Why: the GUI reuses `BlockScanner` +
`HookVerb` verbatim; a second scanner implementation would drift.

### 2.2 Send `StreamPos` after every Replay (2 sites)

**Site A — `C2D::Attach` handler** (daemon/mod.rs ~line 861): inside the `let j = journal.lock();`
block, capture `let stream_off = j.absolute_len();` right where `bytes` is built (before any
enqueue), then after enqueueing the Replay frame enqueue
`D2C::StreamPos { id, off: stream_off }`. Order on the client's queue: Replay → StreamPos →
(existing) Blocks full → live Output. Why under the lock: ingest holds the same lock across
append+fanout, so `absolute_len` at this point is exactly where the first post-Replay Output
frame begins.

**Site B — restore resync in `launch()`** (~lines 612–649): same pattern — capture
`stream_off = j.absolute_len()` while `j` is held, and for each suspended client enqueue
Reset → Replay → StreamPos.

**Site B addendum — Blocks full sync after resync.** After the resync loop (journal lock
dropped), read the store snapshot (leaf `blocks` lock: `(epoch, recs.clone())`) and enqueue
`D2C::Blocks { full: true, .. }` to each suspended client. This fixes a real P1 gap: `launch()`
calls `close_dangling` BEFORE clients are suspended and never notifies, so a reconnected GUI
kept a stale open record forever — which would wrongly disable Re-run. (New attaches already
get the full sync.)

### 2.3 `Journal::read_range` (journal.rs)

```rust
/// Bytes for the absolute-offset range [abs_start, abs_end), clamped to what the file
/// still holds (compaction may have cut the head) and to `max` bytes.
/// Returns (bytes, clipped) where clipped = head was cut or `max` was hit.
pub fn read_range(&self, abs_start: u64, abs_end: u64, max: usize) -> (Vec<u8>, bool)
```

Implementation: open a FRESH `File` (same pattern as `tail()` — never seek the append handle);
file range = `abs.saturating_sub(self.base)` clamped to `[0, self.len]`; `clipped` true when
`abs_start < self.base` or the range exceeded `max`. Why fresh handle: the append handle's
position/mode must never be disturbed under concurrent appends.

### 2.4 `C2D::BlockText` handler (daemon/mod.rs, in `handle_message`)

```rust
C2D::BlockText { id, start_off } => {
    // Leaf blocks lock: find the record; clone what we need; drop the lock.
    let rec = self.blocks.lock().get(&id)
        .and_then(|s| s.recs.iter().find(|r| r.start_off == start_off).cloned());
    let Some(rec) = rec else { log::debug!("BlockText: unknown block"); return; };
    let Ok(journal) = self.journal(id) else { return };
    let (raw, clipped) = {
        let j = journal.lock();
        let end = rec.end_off.unwrap_or_else(|| j.absolute_len()); // open block = to head
        j.read_range(rec.start_off, end, BLOCK_TEXT_RAW_CAP)       // 4 MiB
    };
    let mut text = String::new();
    let mut stripper = crate::strip::AnsiStripper::default();
    stripper.feed(&raw, &mut text);
    let mut truncated = clipped || rec.truncated;
    if text.len() > BLOCK_TEXT_CAP { // 1 MiB, cut at a char boundary
        let mut cut = BLOCK_TEXT_CAP;
        while !text.is_char_boundary(cut) { cut -= 1; }
        text.truncate(cut);
        truncated = true;
    }
    if let Some(f) = frame_bytes(&D2C::BlockText { id, start_off, text, truncated }) {
        client.enqueue(&f); // requester only — this is a reply, not a broadcast
    }
}
```

Constants next to `MAX_FRAME`-style consts: `BLOCK_TEXT_RAW_CAP: usize = 4 * 1024 * 1024`,
`BLOCK_TEXT_CAP: usize = 1024 * 1024`. Why caps: MAX_FRAME is 32 MiB but a clipboard copy of
more than ~1 MiB of text has no use and stalls the client queue.

Range content note (why this is clean without heuristics): `start_off` is just after the exec
hook's OSC terminator — i.e. AFTER the command echo and its newline; `end_off` is just after
the closing `pre` hook's terminator, which the bootstrap emits BEFORE any prompt text. So the
range is pure command output (plus SGR noise, which the stripper removes).

### 2.5 Shared ANSI stripper — new `src/strip.rs`

Move `AnsiStripper` + `StripState` from probe.rs verbatim into `src/strip.rs`
(`pub struct AnsiStripper`, `pub fn feed(&mut self, bytes: &[u8], out: &mut String)`), add
`mod strip;` to main.rs, and re-point probe.rs to `crate::strip::AnsiStripper`. Why: the
daemon needs exactly the probe's chunk-safe streaming semantics (it already strips OSC bodies
— including 7717 hooks — and carries state across chunks); duplicating it invites drift.
Pure move, zero behavior change (probe stays green as the regression check).

---

## 3. GUI backend: offset→row anchoring (src/gui/term_backend.rs)

This is the heart of P2. Mechanism: **feed-time capture, delta maintenance, honest decay**.

### 3.1 The mechanism in one paragraph

The GUI runs the same chunk-safe `BlockScanner` over live Output bytes. When a hook is found,
`advance` is split at the hook boundary so the grid state at capture time is exactly the
state after parsing up to that hook — the cursor row IS the block boundary row. The anchor is
stored as a plain grid `Line` index (viewport space, negative = scrollback) and mutated in
place as the grid scrolls: lines only move when rows enter history, and that is exactly
observable as `history_size()` growth. When exactness is unattainable — history ring
saturated at 10k, history shrank (RIS/ED3), resize while alt-screen — anchors are dropped
and chrome silently disappears rather than drift (wrong chrome is worse than none). Matching
anchors to `BlockRec`s uses the absolute journal offset: `D2C::StreamPos` gives the base and
Output bytes are counted from there, so `anchor.start_off == rec.start_off` exactly
(probe-verified, §8.1). `start_off` alone is a sufficient key: journal offsets are monotonic
per terminal, so no two records — even across epochs — share one.

### 3.2 Types

```rust
/// One in-grid anchor for a block, joined to its BlockRec by start_off.
#[derive(Clone, Copy, Debug)]
pub struct BlockAnchor {
    pub start_off: u64,
    /// Grid line (Term coordinate space: 0-based screen, negative = history) of the
    /// LOGICAL START row of the command line (wrap chain already walked at capture).
    pub line: i32,
    /// Grid line of the closing prompt row (set when the matching `pre` hook lands).
    pub end_line: Option<i32>,
}

/// Per-backend block-anchoring state. None ⇒ this session pays zero cost.
pub struct BlockFeed {
    scanner: crate::daemon::blocks::BlockScanner,
    /// Absolute journal offset of the next live Output byte (from D2C::StreamPos).
    next_off: Option<u64>,
    /// Scanning active (set when a Blocks frame shows epoch > 0 — i.e. hooked spawn).
    enabled: bool,
    /// Sorted by start_off; lines are strictly increasing too (later block = lower row).
    pub anchors: Vec<BlockAnchor>,
    last_history: usize,
    was_alt: bool,
    /// Anchors can no longer be maintained exactly; in-grid chrome is suppressed
    /// (panel/actions unaffected). Set by: ring saturation, history shrink, alt resize.
    pub stale: bool,
}

pub struct TermBackend {
    // …existing fields…
    pub block_feed: Option<BlockFeed>,
}
```

Why `line: i32` mutated in place rather than a monotonic "absolute row" space: ≤500 anchors
× one add per scrolling chunk is trivially cheap, and it keeps exactly ONE coordinate space —
the same one `selection_point`, the continuity fill, and the render pass already use.

### 3.3 Feed path

Rename the current `advance` internals into two public entries (both still run `mode_scan` —
win32-input detection must see replay bytes):

```rust
/// Replay/reconstruction bytes and tests: parse only. Never scanned, never counted —
/// a raw-tail replay (alt-screen/dead fallback) CONTAINS hook OSCs, and counting a
/// reconstruction would corrupt stream offsets.
pub fn advance(&mut self, bytes: &[u8])

/// Live Output frames: parse + scan + anchor.
pub fn advance_live(&mut self, bytes: &[u8])
```

`advance_live`:

```rust
pub fn advance_live(&mut self, bytes: &[u8]) {
    if let Some(on) = self.mode_scan.feed(bytes) { self.win32_input = on; }
    match &mut self.block_feed {
        None => self.parser.advance(&mut self.term, bytes),
        Some(_) => self.advance_scanned(bytes),
    }
    self.drain_events();
}

fn advance_scanned(&mut self, bytes: &[u8]) {
    let bf = self.block_feed.as_mut().unwrap();
    let base = bf.next_off;
    if let Some(o) = bf.next_off.as_mut() { *o += bytes.len() as u64; }
    if !bf.enabled {
        self.parser.advance(&mut self.term, bytes);
        return;
    }
    let events = bf.scanner.feed(bytes); // offsets relative to `bytes`
    let mut done = 0usize;
    for ev in events {
        let end = ev.offset_in_chunk;                       // byte AFTER the OSC terminator
        self.parser.advance(&mut self.term, &bytes[done..end]);
        done = end;
        self.track_scroll();                                // shift anchors before reading cursor
        // Skip capture while a DECSET-2026 sync block is pending: the grid hasn't
        // applied these bytes yet, so the cursor would lie. (Hooks inside sync blocks
        // don't happen at real prompts; safe to leave such a block unanchored.)
        if self.parser.sync_timeout().sync_timeout().is_some() { continue; }
        match ev.verb {
            HookVerb::Exec { .. } => {
                if let Some(b) = base { self.capture_exec(b + end as u64); }
            }
            HookVerb::Pre { .. } => self.capture_pre(),
            HookVerb::Init { .. } => {}
        }
    }
    self.parser.advance(&mut self.term, &bytes[done..]);
    self.track_scroll();
}
```

Notes:
- `abs_off` for exec = `base + end` — the byte after the hook terminator — which is exactly
  what the daemon stored as `rec.start_off` (see ingest/on_block_event). Bit-for-bit equality
  is what probe §8.1 asserts.
- Split-feeding is byte-identical parsing (vte is incremental); the ModeScanner-style tests
  in blocks.rs already prove scanner chunk-invariance.
- The GUI cannot check hook tokens (by design — token is an anti-spoof for the *record*
  store). A spoofed hook creates an anchor whose `start_off` matches no record ⇒ it is inert
  (render joins by start_off) and gets pruned by the eviction/GC rules below.

### 3.4 Scroll tracking, saturation, alt-screen

```rust
fn track_scroll(&mut self) { /* on &mut self to read term + mutate feed */ }
```

Logic (runs only when `block_feed.is_some() && enabled && !stale`):

1. `let alt = self.term.mode().contains(TermMode::ALT_SCREEN);`
   - Entering alt (`!was_alt && alt`): set `was_alt = true` and RETURN. The active grid is
     the alt grid (`history_size()==0`) — reading it would look like a huge shrink. The
     primary grid is frozen while alt is active, so anchors need no updates.
   - Leaving alt: `was_alt = false; last_history = history_size(); return;` (resync, no shift
     — primary history can't have changed while alt was active).
   - While alt: return.
2. `let h = self.term.grid().history_size();`
   - `h > last_history`: `let d = (h - last_history) as i32;` subtract `d` from every
     `anchor.line` and `end_line`; prune anchors with `line < -(h as i32)` (fell off the
     ring). `last_history = h`.
   - `h < last_history`: history shrank (ED3/RIS/clear-scrollback). Prune anchors with
     `line < 0` (their rows are gone); keep on-screen ones; `last_history = h`.
     Why not full drop: ED3 erases scrollback only; screen rows and their anchors are intact.
   - `h == GUI_SCROLLBACK (10_000)`: the ring is saturated — further scrolling is
     unobservable (history delta pins at 0 while rows still evict), so anchors would drift.
     Set `stale = true; anchors.clear();`. Honest degraded mode: panel keeps working,
     in-grid chrome disappears for the rest of this attach. (`const GUI_SCROLLBACK: usize =
     10_000;` shared with `TermBackend::new`'s config.)

Why drift is unacceptable but disappearance is fine: a separator on the wrong command is
actively misleading; absence just means "scrolled past what we can track", and the panel
still names every block.

### 3.5 Capture

```rust
fn capture_exec(&mut self, start_off: u64) {
    let grid = self.term.grid();
    let cur = grid.cursor.point;
    // PSReadLine echoes the accept-newline before ReadLine returns, so the cursor sits at
    // col 0 of the first OUTPUT row; the command's last row is one above. If a hook ever
    // lands with the cursor mid-line, that line IS the command row.
    let cmd_last = if cur.column.0 == 0 { cur.line.0 - 1 } else { cur.line.0 };
    // Normalize to the LOGICAL START of the (possibly wrapped) prompt+command line, so the
    // separator sits above the whole command and resize remap can key on logical lines.
    let line = walk_to_logical_start(&self.term, cmd_last, 64);
    let bf = self.block_feed.as_mut().unwrap();
    if bf.stale { return; }
    // Defensive ordering: drop any anchor with start_off >= this one (daemon dedupes;
    // the GUI store keyed the same way — duplicates can only come from a replayed spoof).
    bf.anchors.retain(|a| a.start_off < start_off);
    bf.anchors.push(BlockAnchor { start_off, line, end_line: None });
}

fn capture_pre(&mut self) {
    let prompt_line = self.term.grid().cursor.point.line.0; // pre fires before prompt text
    if let Some(bf) = self.block_feed.as_mut() {
        if let Some(a) = bf.anchors.last_mut() {
            if a.end_line.is_none() { a.end_line = Some(prompt_line); }
        }
    }
}

/// Walk up the WRAPLINE chain: row r's logical start is the topmost row s ≤ r such that
/// row s-1 does not wrap into s. Bounded (64 rows ≈ a 10k-char command at 160 cols).
fn walk_to_logical_start(term: &Term<EventProxy>, mut r: i32, cap: usize) -> i32
// impl: for _ in 0..cap { let above = r - 1; if above < -(history as i32) { break; }
//   if !term.grid()[Line(above)][Column(cols-1)].flags.contains(Flags::WRAPLINE) { break; }
//   r = above; }  return r;
```

Anchor ordering invariant (used for binary search): pushed in start_off order, and lines are
non-decreasing because every exec/pre pair moves the cursor at least one row between captures
while older anchors only ever move DOWN (negative). No sort ever needed.

### 3.6 Resize reflow remap

alacritty reflows on resize (wrapped lines merge/split) — visual line indices shift
unpredictably, but LOGICAL lines (explicit newlines) are preserved as units. Anchors are
stored at logical-start rows (§3.5), so remap = "same logical line, new visual row", counted
from the BOTTOM (the bottom is the stable end across reflow; counting from the top would
shift when reflow-overflow evicts head rows).

In `resize_to`, when `block_feed` has anchors and `!stale`:

```rust
// PRE-resize: one bottom-up walk assigning each anchor its logical ordinal.
// ordinal(a) = number of logical-start rows in [a.line ..= bottommost_line()].
// Single pass from bottommost down to the topmost anchor; O(rows spanned), only on
// debounced commits (120ms live / 500ms cell-change), never per frame.
let ordinals: Vec<(u64, u32, Option<u32>)> = …; // (start_off, ord(line), ord(end_line))

self.term.resize(TermSize::new(cols as usize, rows as usize));

// POST-resize: walk bottom-up again counting logical starts; when the count reaches an
// ordinal, that row is the anchor's new line. Anchors whose ordinal is never reached
// (reflow pushed them off the ring) are pruned. Then last_history = history_size().
```

Special cases:
- Resize while `ALT_SCREEN`: the active grid is the alt grid and the primary grid is
  inaccessible (`inactive_grid` is private — known from the serializer work), so the walk
  cannot run. Set `stale = true`, clear anchors. Justification: rare combo (resizing while
  inside vim), and wrong anchors after reflow would be garbage.
- `end_line` remaps by its own ordinal; if pruned, set to `None` (render then bounds the
  block by the next anchor).

### 3.7 Small API additions

```rust
impl TermBackend {
    /// D2C::StreamPos: create the feed lazily; scanning stays off until enabled.
    pub fn set_stream_pos(&mut self, off: u64)
    /// First Blocks frame with epoch > 0 (a hooked spawn exists): start scanning.
    pub fn enable_block_scan(&mut self)
    /// Scroll so grid line `line` sits ~2 rows below the viewport top (same math family
    /// as scroll_to_match): display_offset = clamp(2 - line, 0, history).
    pub fn jump_to_line(&mut self, line: i32)
}
```

Why epoch>0 as the enable signal (not TermKind heuristics): `launch()` bumps epoch ONLY for
hooked spawns, including the CLI-restore pwsh wrapper whose state-side `kind` stays
Shell/Custom — a kind check would miss it. The full-sync Blocks frame (which carries epoch)
arrives after Replay/StreamPos and before any live Output, so the scanner is on before the
first hook byte. Cost when enabled but no hooks flow: one DFA pass over output bytes — the
daemon already does the identical scan on the same stream.

---

## 4. GUI store + actions (src/gui/mod.rs)

### 4.1 Store

Replace `blocks: HashMap<Uuid, Vec<BlockRec>>` with:

```rust
struct BlockList {
    /// Sorted by start_off (monotonic per terminal — the daemon appends in order and
    /// upserts never reorder). Binary-search upserts keep it that way.
    recs: Vec<BlockRec>,
    /// Latest epoch seen in any Blocks frame; > 0 ⇔ this terminal spawns hooked.
    epoch: u32,
    /// Derived data (failed count) needs recompute.
    dirty: bool,
    /// Cached count of failed blocks (exit Some(≠0)) for the panel badge / nav buttons.
    failed: usize,
}
blocks: HashMap<Uuid, BlockList>,
```

`drain_ipc`:

```rust
D2C::Blocks { id, epoch, full, recs } => {
    let list = self.blocks.entry(id).or_default();
    list.epoch = list.epoch.max(epoch);
    if full { list.recs = recs; } else {
        for r in recs {
            match list.recs.binary_search_by_key(&r.start_off, |x| x.start_off) {
                Ok(i) => list.recs[i] = r,
                Err(i) => list.recs.insert(i, r),
            }
        }
    }
    list.dirty = true;
    if list.epoch > 0 {
        if let Some(b) = self.terms.get_mut(&id) { b.enable_block_scan(); }
    }
}
D2C::StreamPos { id, off } => {
    if let Some(b) = self.terms.get_mut(&id) { b.set_stream_pos(off); }
}
D2C::BlockText { id: _, start_off: _, text, truncated } => {
    ctx.copy_text(text);
    self.notice = Some((
        if truncated { "Block output copied (truncated).".into() }
        else { "Block output copied.".into() },
        Instant::now(),
    ));
}
```

Why key upserts by `start_off` alone (P1 keyed by `(epoch, start_off)`): journal offsets are
monotonic per terminal, so start_off is already unique across epochs; a single sorted key
enables binary search. Keep reading `epoch` for the hooked signal. `Output` frames switch to
`backend.advance_live(&bytes)`; `Replay` stays `backend.advance(&bytes)`.

Reconnect (`reconnect_if_needed`) already clears `self.blocks` — unchanged; `D2C::Reset`
creates a fresh backend, and the post-resync full sync (§2.2) re-delivers epoch → re-enables
scanning on the new backend. `apply_snapshot` retention of `self.blocks` — unchanged.

### 4.2 Re-run gate

```rust
/// Mouse-first Re-run is allowed only when the shell is demonstrably at an interactive
/// prompt. Signals (all existing): the session is Running; the terminal is not in
/// alt-screen; and no block record is open (end_off == None). "No open block" IS
/// cursor-on-prompt for hooked shells: every accepted line opens a block (exec hook) and
/// only the next prompt render closes it (pre hook) — so an open block covers both
/// "command still running" and "user launched a TUI (claude) from the prompt".
fn can_rerun(&self, id: Uuid) -> bool {
    let running = self.state.terminal(id).is_some_and(|t| t.status == TermStatus::Running);
    let no_open = self.blocks.get(&id)
        .is_some_and(|b| !b.recs.is_empty() && b.recs.iter().all(|r| r.end_off.is_some()));
    let not_alt = self.terms.get(&id)
        .is_some_and(|t| !t.mode().contains(TermMode::ALT_SCREEN));
    running && no_open && not_alt
}
```

Accepted residual risk (documented, matches Warp): text already typed-but-unsubmitted at the
prompt gets the re-run appended after it. Default: do NOT try to clear the line first
(sending ESC/kill-chords blind into PSReadLine risks mode-dependent behavior; P3 Composer
owns line-editing). Execution path:

```rust
fn rerun_block(&mut self, id: Uuid, start_off: u64) {
    if !self.can_rerun(id) { return; }
    let Some(cmd) = self.blocks.get(&id)
        .and_then(|b| b.recs.iter().find(|r| r.start_off == start_off))
        .map(|r| r.cmd.clone()) else { return };
    let mut bytes = cmd.into_bytes();
    bytes.push(b'\r');
    self.send(C2D::Input { id, bytes });      // typed-text path: UTF-8 passthrough is
    if let Some(b) = self.terms.get_mut(&id) { // valid under win32-input-mode (WT's paste path)
        b.scroll_to_bottom();
    }
}
```

### 4.3 Copy output

```rust
fn copy_block_output(&mut self, id: Uuid, start_off: u64) {
    if self.ipc.as_ref().is_some_and(|c| c.proto >= 2) {
        self.send(C2D::BlockText { id, start_off });
    } else {
        self.notice = Some(("Restart the daemon from this build to copy block output.".into(), Instant::now()));
    }
}
```

Fire-and-forget; the reply path is §4.1. Why no pending-state machinery: replies arrive in
milliseconds on loopback; a lost reply (daemon died) surfaces via the existing reconnect UX.

---

## 5. term_view chrome (src/gui/term_view.rs)

### 5.1 Data in

```rust
/// Everything the widget needs to draw + interact with blocks. None ⇒ zero block work.
pub struct BlockViewCtx<'a> {
    pub recs: &'a [BlockRec],   // sorted by start_off
    pub can_rerun: bool,        // App-evaluated gate (§4.2)
}

pub enum BlockAction { CopyOutput(u64), Rerun(u64) } // start_off; CopyCmd+Jump handled in-widget

pub struct TermViewOutput {
    pub write: Vec<u8>,
    pub block: Option<BlockAction>,
}

pub fn show(…existing params…, blocks: Option<BlockViewCtx<'_>>) -> (Response, TermViewOutput)
```

`terminal_card` builds it (borrow-order: compute `can_rerun` first, then take
`self.blocks.get(&id)` and `self.terms.get_mut(&id)` — disjoint fields):

```rust
let can_rerun = self.can_rerun(id);
let bctx = self.blocks.get(&id).and_then(|b| {
    (!b.recs.is_empty()).then_some(term_view::BlockViewCtx { recs: &b.recs, can_rerun })
});
```

Suppression gate inside `show`/`render` (ONE check, before anything block-related):

```rust
let blocks_active = blocks.is_some()
    && !backend.mode().contains(TermMode::ALT_SCREEN)
    && backend.block_feed.as_ref().is_some_and(|f| !f.stale && !f.anchors.is_empty());
```

This single boolean guarantees: hookless sessions (blocks=None), alt-screen sessions, stale
sessions, and Claude/cmd sessions all take exactly today's render path.

### 5.2 Theme tokens (add next to the existing ACCENT/SURFACE_2/BORDER consts — the file
already duplicates app tokens by convention)

```rust
const DANGER: Color32 = Color32::from_rgb(0xFF, 0x5C, 0x6C);
const SEPARATOR: Color32 = Color32::from_rgba_unmultiplied(0x2C, 0x32, 0x42, 96); // BORDER_STRONG @ ~38%
const DANGER_GUTTER: Color32 = Color32::from_rgba_unmultiplied(0xFF, 0x5C, 0x6C, 140);
const CHIP_BG: Color32 = Color32::from_rgba_unmultiplied(0xFF, 0x5C, 0x6C, 26);
```

Justification: separators must read as texture, not lines (user: "clean UX not slop");
success stays completely quiet (no green — the absence of red IS the success state, Warp-style).

### 5.3 Geometry — ONE formula everywhere

Grid line → pixel y (identical to both existing passes, so chrome lands correctly in the
continuity-fill region too): `y(line) = px(origin.y + cell_h * (line + display_offset))`
where `origin = content_rect.min` (the shifted rect). All chrome y-coords derive from this;
nothing else may be invented.

Clip: chrome uses `painter.with_clip_rect(Rect::from_min_max(pos2(grid_rect.min.x - PAD_L + 3.0,
grid_rect.min.y), grid_rect.max))` — same vertical clip as text, widened left so the failure
gutter can live in the padding without touching column 0 glyphs.

### 5.4 What is drawn (per visible anchored block, joined rec required)

Visible set: binary search `anchors` (`partition_point` on `line`) for
`line + display_offset ∈ [-(fill rows above), rows]`; for each, binary search `recs` by
`start_off`. No allocation; skip anchors with no rec (spoof orphans).

Per block, with `sep_y = y(anchor.line) - 0.5` (on the row boundary):

1. **Separator** (all completed + open blocks): 1px `SEPARATOR` hairline from
   `grid_rect.min.x` to `grid_rect.max.x - 16` (stops short of the scrollbar lane). Pushed
   into `bg_shapes` (under everything; it lives in the row gap so z barely matters).
   Skip when `y < grid_rect.min.y + 1` (clipped anyway). **Self-healing guard**: skip the
   whole block's chrome if the anchor row is now blank (`grid[Line(anchor.line)]
   .line_length().0 == 0`) — covers `cls` (ED2 erases rows in place; anchors would
   otherwise mark empty screen rows).
2. **Failure chrome** (only `rec.exit.is_some_and(|e| e != 0)`):
   - Gutter strip: 3px wide rect at `x ∈ [grid_rect.min.x - PAD_L + 3, grid_rect.min.x - PAD_L + 6]`,
     from `sep_y` to `y(end_bound)` (end_bound = `anchor.end_line` else next anchor's line
     else cursor line), color `DANGER_GUTTER`, into `bg_shapes`.
   - Exit chip: right-aligned, vertically centered ON the separator line (avoids glyph
     collision): rounded rect `CHIP_BG` + text `exit {code}` 10px `DANGER`
     (`painter.layout_no_wrap` — egui's internal galley cache makes the per-frame call a
     hash lookup, no layout churn). Chip bg → `deco_shapes`, text → `text_shapes`.
3. **Open blocks** (`rec.end_off.is_none()`): separator ONLY. No gutter, no chip, no hover
   toolbar — an open block may be a live TUI (claude at the prompt); requirement: draw
   nothing over the live app. Its actions live in the blocks panel.
4. **Hover toolbar** (completed blocks only): see §5.5.
5. **Jump flash** (after a jump): translucent `ACCENT_SUBTLE` full-width rect over the
   block's first 2 rows, alpha animated 1→0 over 0.5s via
   `ctx.animate_value_with_time(widget_id.with(("blk_flash", start_off)), …)`; driven by a
   `(start_off, Instant)` in `ViewState`. Cheap, bounded, self-repainting via animate_value.

Duration/cwd are NOT drawn per-block statically — they appear in the hover toolbar only
(requirement: "duration + cwd available on hover"; keeps resting state quiet).

### 5.5 Hover toolbar — layout, hit-testing, z-order

Manual painter chrome + manual hit-testing, exactly like the jump pill — NOT egui child
widgets. Why: `process_input` consumes RAW `ctx.input` events, so egui's widget hit-order
cannot protect selection/mouse-report anyway; and real Buttons would steal the grid's
keyboard focus for a frame per click.

```rust
#[derive(Clone, Copy, PartialEq)]
enum BlockBtn { CopyCmd, CopyOutput, Rerun, Jump }

struct HoveredBlock {
    start_off: u64,
    toolbar: Rect,
    btns: [(Rect, BlockBtn); 4],
    rerun_enabled: bool,
}

/// Pure layout: pointer row (via backend.selection_point — the SAME mapping drag-select
/// uses, including negative-y history rows) → binary search for the block whose
/// [line, end_bound) contains it → toolbar geometry. Returns None unless blocks_active,
/// pointer inside grid_rect, no drag in progress, and the block is completed.
fn hovered_block_layout(backend:&TermBackend, blocks:&BlockViewCtx, grid_rect:Rect,
                        content_rect:Rect, pointer:Option<Pos2>, dragging:bool,
                        mode:TermMode) -> Option<HoveredBlock>
```

Rules (each one line, each load-bearing):
- Suppressed when `mode.intersects(TermMode::MOUSE_MODE)` — a primary-screen app is
  consuming the mouse; hover chrome over it would fight the app.
- Suppressed while `vs.dragging` — a selection drag sweeping through must never pop UI.
- Position: right-aligned pill, `height 24`, at `y = sep_y + 4`, `x right = grid_rect.max.x - 12`;
  clamped to `y ≥ grid_rect.min.y + 4` (block header scrolled off ⇒ toolbar docks to the top
  edge while the pointer is inside the block — Warp's sticky-header affordance).
- Contents left→right: `"{duration} · {cwd}"` caption (11px, TEXT_SECONDARY; cwd
  middle-ellipsized to 28 chars; duration `fmt_duration(ended_ms - started_ms)`), then 4
  icon buttons 18×18: CopyCmd, CopyOutput, Rerun, Jump. Rerun drawn `TEXT_FAINT` when
  `!can_rerun` (visibly disabled, still discoverable).
- Visuals: `SURFACE_2` fill, `BORDER` 1px stroke, radius 6, same shadow recipe as the jump
  pill. Buttons get an `OV_HOVER` overlay when the pointer is inside their rect. Labels via
  `egui::containers::show_tooltip_at` after 0.3s hover ("Copy command", "Copy output",
  "Run again" / "Shell is busy", "Jump to start").
- Drawn LAST (pushed to the end of `text_shapes` after compose — same painter, same clip),
  so it floats above text like the jump pill. Because it exists only under the pointer,
  transient occlusion of one text row is acceptable and standard.

Call order inside `show()` (this is the interaction-correctness crux):

```rust
let chrome = hovered_block_layout(…);              // BEFORE process_input
process_input(…, chrome.as_ref());                 // may consume clicks
render(…, blocks.as_ref(), chrome.as_ref(), …);    // draws chrome
```

`process_input` changes — precise and minimal:
- `PointerButton { pressed: true, pos, .. }`: if `chrome.is_some_and(|c| c.toolbar.contains(pos))`
  → record `vs.toolbar_press = Some(pos)` and `continue` (do NOT start selection, do NOT
  mouse_report — mouse-report can't fire here anyway since MOUSE_MODE suppresses the toolbar,
  but the selection skip is essential).
- `PointerButton { pressed: false, pos, .. }`: if `vs.toolbar_press.take()` was Some and a
  button rect contains `pos` → emit the action: CopyCmd → `ctx.copy_text(rec.cmd)` in-widget;
  Jump → `backend.jump_to_line(anchor.line)` + set flash, in-widget; CopyOutput/Rerun →
  `out.block = Some(…)` for the App. Then `continue`.
- Everything else — wheel, drag update, negative-y selection, focus lock, IME, key paths —
  untouched. A drag that *enters* the toolbar rect mid-drag keeps selecting (only presses
  that BEGIN inside the rect are consumed).
- Add `hovered_block: Option<u64>` + `toolbar_press: Option<Pos2>` + `flash: Option<(u64, f64)>`
  to `ViewState` (it's Clone+Default temp memory already).

New icons in mod.rs `Icon` enum + `draw_icon`: `Copy` (two offset rounded rects),
`CopyLines` (rect + three hlines), `Rerun` (¾ circle arc + arrowhead), reuse existing
`ChevronUp/ChevronDown` for the panel nav. Keep strokes 1.5px, TEXT_SECONDARY, matching the
existing icon language.

### 5.6 Perf compliance (the budget, explicitly)

| Cost | When | Bound |
|---|---|---|
| Scanner DFA | per live Output chunk, hooked sessions only | ~1 branch/byte (daemon already pays the same) |
| Anchor shift | per scrolling chunk | ≤500 int subs |
| Resize remap | per debounced resize commit | 2 × O(rows spanned by anchors) walks |
| Render lookup | per frame, blocks_active only | 2 binary searches + O(visible blocks ≤ rows) |
| Shapes | per frame | pushed into EXISTING vecs (bg/deco/text) — zero new Vec allocs |
| Galleys | chip/toolbar text | egui Fonts galley cache — hash lookups after first frame |
| No blocks / alt-screen / stale | per frame | one boolean check (`blocks_active`) |

No `request_repaint` from any chrome path; hover changes repaint via input events as always;
`pump_sync` cadence untouched.

---

## 6. Header + blocks panel + navigation (src/gui/mod.rs)

### 6.1 Entry point (mouse-first, per UX doctrine)

In `header_bar`'s right-to-left cluster, between the search magnifier and the cwd label, add
a **Blocks icon button** (`Icon::CopyLines` reused or a dedicated `Icon::Blocks` — three
stacked bars) — shown ONLY when `self.blocks.get(&id).is_some_and(|b| !b.recs.is_empty())`.
Why hidden otherwise: a claude/cmd tab must show zero block chrome anywhere. When failures
exist, draw a small `DANGER` dot on the icon's corner (reuse the burst_badge dot recipe) —
the visible entry point for "something failed".

Clicking toggles `self.blocks_panel: Option<BlocksPanel>`:

```rust
struct BlocksPanel { filter: String, failed_only: bool }
```

Panel closes on: toggle click, Esc, terminal switch (`select_terminal` sets it to None), or
a primary click outside its rect (check `ctx.input clicks` vs the panel rect).

### 6.2 Panel UI

`egui::Area` (id `("blocks_panel", terminal_id)`, `Order::Foreground`) anchored under the
header's right edge; `Frame` styled like the modal surface (SURFACE, BORDER stroke, radius 8,
shadow); width 440, max height 360 with `ScrollArea::vertical().show_rows` (recs ≤ 500,
row height fixed 34 — show_rows keeps it O(visible)).

Header row: filter `TextEdit` (hint "Filter commands", plain case-insensitive substring over
`cmd` — no regex: this is command recall, not text search, and the scrollback search already
owns regex), a "Failures" toggle chip, and two icon buttons `ChevronUp`/`ChevronDown` =
**previous/next failed command** with hover text. While the panel is open, the grid's
`focused` flag goes false (same mechanism as search: `self.search.is_none() &&
self.blocks_panel.is_none()`), so typing lands in the filter, and the grid stops
`request_focus()`-stealing it.

Each row (newest at top — recency is what recall wants): status glyph (nothing for exit 0,
`DANGER` "✕ {code}" for failures, spinner-dot for open, `TEXT_FAINT` "—" for exit=None),
`cmd` in 12px mono ellipsized, right-aligned `fmt_duration` + `time_ago(started_ms)` in
TEXT_MUTED. Truncated blocks get a `TEXT_FAINT` "trimmed" tag (journal compaction cut their
output; Copy output will be partial — matches `truncated` flag honesty).

Row interactions (hover reveals a mini action cluster, mirroring the toolbar): click row =
jump when anchored; rows without an anchor render slightly dimmed with hover text
"Not in view — ran before this window attached (or scrolled past tracking)". Why still
listed: the panel is the honest degraded mode for pre-attach/stale history — commands,
exit codes, durations, cwd, Copy command, Copy output, and Re-run all still work from the
record + journal; only the in-grid jump needs an anchor.

Anchored check: `terms[id].block_feed` anchors binary-search by `start_off`.

### 6.3 Prev/next failed semantics

Working set: failed recs (`exit.is_some_and(|e| e != 0)`) that have anchors, ordered by
`start_off`. Current position = the topmost visible grid line (`-display_offset`). "Prev" =
greatest anchored failure with `line < top_line` (older, above); "next" = smallest with
`line > top_line`. Jump via `backend.jump_to_line` + flash. Buttons disabled (TEXT_FAINT)
when the set is empty; hover text explains ("No failed commands in view history").
Justification: navigation must be strictly predictable from what the user can see; skipping
unanchored failures silently (they're listed in the panel) beats jumping to a wrong row.

---

## 7. Degraded modes — the honest contract (user-visible truth table)

| Situation | In-grid chrome | Panel | Copy output | Re-run |
|---|---|---|---|---|
| Hooked, live, command ran while attached | full | yes | yes | gated |
| Command ran before this attach (replay is a reconstruction — journal offsets ≠ replayed bytes, so no exact row exists BY CONSTRUCTION) | none | yes (dimmed row) | yes | gated |
| Scrollback ring saturated (>10k lines this attach) | none (dropped, never drifting) | yes | yes | gated |
| Block truncated by journal compaction | normal + "trimmed" in panel | yes | partial (`truncated: true`) | gated |
| Open block (incl. claude TUI at prompt) | separator at start only | yes ("running…") | yes (partial-so-far) | no (open block fails gate) |
| Alt-screen active | none (all suppressed) | yes | yes | no (gate) |
| Hookless session (claude tab, cmd, custom) | none — and no Blocks header button at all | n/a | n/a | n/a |
| Old daemon (proto < 2) | none (no StreamPos ⇒ no anchors) | yes | notice: restart daemon | gated |

---

## 8. Probes (src/probe.rs — extend the existing 18-case suite)

All headless, no GUI, run with no GUI attached. Reuse `Conn`, `create_probe_terminal`,
`await_blocks`, `await_output`.

### 8.1 `blocks_stream_pos` — proves the GUI anchoring math end-to-end

1. Create hooked pwsh terminal; `Attach { cols: 120, rows: 30 }`.
2. Assert a `D2C::StreamPos { off }` frame arrives after the `Replay` frame and before any
   `Output` frame (drain in order; record `off`).
3. Send `Input "echo POSMARK_1\r"`.
4. Collect all `Output` bytes from attach onward into a buffer; run
   `daemon::blocks::BlockScanner` over it (chunk it at 7 bytes to exercise carry); for the
   `Exec { cmd: "echo POSMARK_1" }` event compute `abs = off + event_offset_in_buffer`.
5. `await_blocks` until a rec with `cmd == "echo POSMARK_1"` exists; assert
   `rec.start_off == abs` EXACTLY. This is the money assertion: GUI-side offset arithmetic
   (StreamPos + Output byte counting + scanner offsets) reproduces daemon record keys
   bit-for-bit — which is the entire basis of anchor↔record joins.
6. Also assert a second `StreamPos` arrives after a `RestartTerminal` resync
   (`Reset` → `Replay` → `StreamPos` order), plus a full `Blocks` frame after it (§2.2 fix).

### 8.2 `blocks_text` — BlockText round-trip

1. Hooked terminal; run `echo BLK_OUT_alpha; echo BLK_OUT_beta`.
2. `await_blocks` for the closed rec; send `C2D::BlockText { id, start_off }`.
3. Await `D2C::BlockText`: assert text contains `BLK_OUT_alpha` and `BLK_OUT_beta`, contains
   NO 0x1b and NO 0x07 bytes, does NOT contain `"7717"` (hook OSCs stripped), does NOT
   contain the next prompt (`"PS "` must not appear after the last BLK_OUT line — proves the
   start/end offsets exclude echo and prompt), and `truncated == false`.
4. Open-block variant: start `ping -t 127.0.0.1`, await the open rec, request BlockText,
   assert it contains `Reply from` (partial output of an open block works), then Ctrl+C.

### 8.3 `blocks_rerun_gate` — gate truth through a real shell

1. Hooked terminal; run `echo RERUN_TAG`; await closed rec N1.
2. Evaluate the gate exactly as the GUI will (`recs non-empty && all end_off.is_some()`):
   assert TRUE. Send the re-run bytes (`"echo RERUN_TAG\r"` via `Input`).
3. `await_blocks` for a SECOND rec with the same cmd and `exit == Some(0)` — proves an
   injected re-run is accepted at a real PSReadLine prompt and re-captured by the hooks.
4. Start `ping -t 127.0.0.1`; await its OPEN rec; assert gate FALSE (open block present).
5. Interrupt (reuse the `keys` case's win32 Ctrl+C encoding); await the rec closing; assert
   gate TRUE again. (Alt-screen leg is GUI-side TermMode and is covered by unit test §9.)

### 8.4 `blocks_hookless_silent` — no chrome inputs exist for unhooked terminals

1. Create a terminal with `kind: Custom, program: "cmd.exe", args: ["/q"]`; attach.
2. Assert the attach-time full `Blocks` frame (if any) has `epoch == 0` and empty `recs`
   (epoch 0 = never a hooked spawn ⇒ the GUI never enables scanning — the load-bearing gate).
3. Run `echo hi` via Input; bounded negative wait (2s) asserting NO incremental Blocks frame
   and no `journals/<id>.blocks.json` sidecar exists.

Register all four in the `CASES` table; suite grows 18 → 22.

## 9. Unit tests (cargo test)

term_backend.rs (build streams with the same `hook()` helper style as blocks.rs tests;
add a test-only `TermBackend::new_with_history(size, hist)` so saturation is testable with
hist=50):

- `anchor_capture_splits_at_hook`: feed `"cmd echo\r\n" + exec_hook + "out\r\n"` in one
  `advance_live` call; assert anchor.line == the command row (cursor col-0 rule + wrap walk).
- `anchor_shifts_with_history_and_prunes`: fill screen, scroll k lines, assert `line -= k`;
  scroll past ring, assert anchor pruned.
- `saturation_sets_stale_and_clears`: hist=50 backend, overflow it, assert
  `stale && anchors.is_empty()`.
- `alt_screen_freezes_tracking`: capture anchor → `ESC[?1049h` + junk + `ESC[?1049l` →
  assert anchor unchanged and `last_history` resynced (no phantom shrink).
- `resize_remap_follows_logical_line`: wrapped content, capture, shrink cols so wraps
  change, assert the anchor row's text still starts with the command text.
- `history_shrink_prunes_scrollback_anchors_only` (ED3).
- `rerun gate` truth table as a pure function test (empty recs / open rec / all closed).
- strip.rs move: keep probe compiling green + one direct test that a 7717 hook OSC and SGR
  are stripped and BEL never lands in output.

## 10. Interactive checklist (must be verified by screenshot — PowerShell CopyFromScreen; never
run a second GUI instance while the user's is open; never inject input while user is active)

1. Hooked pwsh tab: run `echo ok`, `cmd /c exit 3`, `Get-ChildItem` — separators hairline-subtle
   at rest; ONLY the `exit 3` block shows the red gutter + `exit 3` chip; success rows quiet.
2. Hover a completed block: toolbar appears top-right with duration · cwd + 4 icons; moving
   along the block keeps it; scrolling a long block docks it at the viewport top; leaving hides it.
3. Click-drag a selection STARTING under the toolbar's block but outside the pill — selection
   works; press starting ON the pill never selects; drag entering the pill keeps selecting.
4. Re-run from toolbar at an idle prompt: command types + executes + new block appears;
   while `ping -t` runs the Rerun icon is dimmed with "Shell is busy".
5. Copy command / Copy output on a block with colored output — clipboard is clean text, no
   escapes, no prompt; notice appears for output copy.
6. Blocks panel: filter narrows; Failures toggle; prev/next failed jumps with flash; rows for
   pre-attach history are dimmed and un-jumpable but copy/re-run work.
7. Claude tab (TermKind::Claude): zero chrome, no Blocks header button, no measurable
   render change.
8. In the hooked tab run `claude` (open block): nothing draws over the live claude UI except
   the historical separator above it; alt-screen app (e.g. `vim` via git) suppresses ALL chrome.
9. Resize drag while scrolled up in history with visible separators: separators stay glued to
   their command rows through reflow commits (120ms live cadence), no drift, no jumps.
10. `cls` then scroll up: no stray separators on blank rows (self-healing guard).
11. Scrollback search (V4) open + blocks visible: highlights and chrome coexist; search focus
    unaffected by block chrome; jump pill and toolbar don't collide (toolbar is top-anchored,
    pill bottom-center).
12. 4K↔1080p monitor hop (ppp flap): chrome stays pixel-aligned with rows after the 500ms
    cell-metric commit.

## 11. Open questions — each with the default the implementer should take

1. **Exit chip overlap with a long wrapped command row**: chip rides ON the separator line;
   worst case it overlaps trailing glyphs of the previous block's last row. Default: accept
   (failures only; Warp does the same); alternative (clip text under chip) is not worth a pass.
2. **Clearing typed-but-unsubmitted prompt text before Re-run**: default NO (blind ESC into
   PSReadLine is mode-dependent); revisit in P3 Composer where we own the line editor.
3. **Anchor recovery after ring saturation / for pre-attach blocks via content matching**
   (search grid rows for `rec.cmd` echoes): default OUT OF SCOPE for P2 — it reintroduces
   drift risk for a cosmetic win; the panel covers recall.
4. **`Icon::Blocks` glyph**: default three stacked horizontal bars, 1.5px stroke; any clean
   variant is fine as long as it reads at 16px.
5. **Filter semantics in panel**: default plain case-insensitive substring on `cmd` only
   (not cwd); add cwd matching only if it costs one `contains`.
6. **Duration format**: default `<1s → "412 ms"`, `<60s → "3.4 s"`, else `"2m 05s"`.
7. **Blocks panel while terminal Dead**: default panel still opens (records persist via
   sidecar; Copy output still works from the journal); Re-run disabled by the Running gate.

## 12. Explicit DO-NOTs (each traces to an invariant or past incident)

- Do NOT feed anything into any parser that didn't come from the daemon (mirror-purity class
  of bugs: coordinate divergence).
- Do NOT add fields to existing protocol variants or reorder enums (bincode is positional).
- Do NOT compute chrome per cell in the render loop; only the sorted-anchor path.
- Do NOT request repaints from chrome; do NOT touch `pump_sync` cadence or force repaints
  while a sync block is pending.
- Do NOT let toolbar presses reach selection/mouse-report; do NOT break negative-y history
  selection (reuse `selection_point` for hover mapping — never a second formula).
- Do NOT show any block UI for `epoch == 0` terminals (the load-bearing hookless gate).
- Do NOT draw gutter/toolbar for open blocks (live TUI safety), and suppress everything under
  `ALT_SCREEN`.
- Do NOT keep anchors that might be wrong — stale/drift states drop chrome, never guess.

## 13. Suggested implementation order (compile-green at each step)

1. `src/strip.rs` move + probe re-point (pure refactor, run probe `blocks_roundtrip`).
2. protocol.rs variants + proto=2 + ipc.rs plumb.
3. journal.rs `read_range` + daemon `BlockText` handler + `StreamPos` at both Replay sites +
   resync full-Blocks fix. Probes 8.1/8.2 written and green here (they need no GUI code).
4. `pub mod blocks;` + term_backend BlockFeed (capture/track/remap) + unit tests.
5. mod.rs store rework (BlockList) + drain_ipc arms + gate + actions.
6. term_view chrome (separator/gutter/chip → toolbar → jump/flash), `blocks_active` gating.
7. Header button + panel + failed nav. Probes 8.3/8.4. Interactive checklist last.
