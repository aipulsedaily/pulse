# P4 "Composer v2 + clickable history" — Implementation Spec (final, implementation-ready)

Target: C:\Terminal Control (egui 0.35 GUI + daemon). P4 is the final roadmap phase. It layers
on **P3 "Composer v1"** (docs\p3-composer-v1-spec.md — the strip, gate, `prompt_end` capture,
`activate()`, recall, submission path) which itself layers on **P2 "Blocks UI"** (merged:
`BlockFeed`/anchors in term_backend.rs, `BlockList` store + `can_rerun` in gui/mod.rs, blocks
panel, `StreamPos`/`BlockText` at proto=2). **P4 depends on P3 being merged first.** At merge
time, verify P3's shipped signatures against this spec's references (`prompt_end`,
`cursor_at_prompt_end`, `ComposerState`, `ComposerOutput`, `gate()`, strip states) and adapt
names — the behavior contract here is binding, the identifiers follow whatever P3 landed.

Two features:

1. **Typeahead reclaim** — P3 v1 refuses to auto-arm over stray typed text and its manual
   activation DISCARDS that text (click-gated Ctrl+C). V2 upgrades the same click to PULL the
   already-typed text into the composer draft before clearing the prompt line, so the user
   keeps what they typed with native editing.
2. **Clickable cross-session history** — a browsable, searchable popup of commands across ALL
   terminals and past epochs, click-to-insert into the current composer draft, click-to-run
   gated by P3's gate.

Ordered as the implementation plan: invariants → decisions → reclaim (extraction → activation
→ failure modes) → history (data → index → popup → focus) → protocol (none) → file-by-file
changes with Rust signatures → perf → degraded modes → probes → unit tests → checklist → open
questions → DO-NOTs → order. Every decision carries a one-line justification.

---

## 0. Non-negotiable invariants (violating any is a bug)

1. **Mirror/parser purity**: P4 injects nothing into any VT parser stream. Reclaim is a pure
   READ of the GUI grid; the only PTY bytes it ever causes are P3's already-sanctioned
   click-gated clear chord and the normal composer submission.
2. **Zero wire changes**: no new protocol variants, no field additions, no `proto` bump. P5
   owns proto=3 (`HelloCtl`/`Ctl` appended after `BlockText`); P4 appends NOTHING, so it can
   land before, after, or interleaved with P5 with zero collision surface.
3. **Sidecar ownership**: the GUI never opens `journals/<id>.blocks.json`. Its only source of
   block records is `D2C::Blocks` frames — which it already receives for EVERY terminal
   (§3.1). Daemon files stay daemon-owned.
4. **Hookless sessions cost zero**: no strip (P3) ⇒ no history button ⇒ no popup ⇒ no index.
   A claude/cmd/custom tab renders at exactly today's cost; the history index is built only
   when the popup is opened on a hooked tab.
5. **Drafts are never destroyed** (P3 D8 extended): reclaim APPENDS to an existing draft;
   history insert STASHES the displaced draft and ArrowDown restores it.
6. **Refuse over guess**: reclaim returns text only when it is exactly recoverable from the
   grid; every ambiguous case (multi-line buffer, cursor mid-line, missing capture) falls
   back to P3 v1's discard with an honest label. Wrong reclaimed text is worse than none —
   the user will SUBMIT what we reclaim.
7. **No new repaint loops**: the popup repaints on input events only; the strip's reclaim
   label is computed per frame only in the one strip state that shows it, bounded (§2.5).
8. **bincode append-only discipline** still binds any FUTURE change: if an implementer is
   tempted to add a wire query after all (rejected in §5.1), it must append after P5's
   variants and bump proto to 4 — never reorder, never insert.

---

## 1. Headline decisions

| # | Decision | One-line justification |
|---|---|---|
| D1 | Reclaim is **click-gated only**, riding P3's existing manual-activation gesture (the strip / `❯ Compose` click) — never automatic, never on a keystroke | Mouse-first doctrine + the PSReadLine-blind contract; auto-pulling text mid-typing is a focus steal (P3 inv. 4 class) |
| D2 | Reclaim reads the **grid** (prompt_end → cursor), not byte counts or a key log | The grid is post-echo truth: it sees type-ahead, backspaces, other clients' keystrokes, and PSReadLine's own rendering exactly (P3 D5's argument, extended to content) |
| D3 | **Refuse over guess**: multi-line buffers (continuation prompts), non-ghost cells right of the cursor, and missing/stale `prompt_end` all fall back to v1 discard, with the strip label saying which will happen BEFORE the click | PSReadLine's `ContinuationPrompt` string is user-configurable — stripping a guessed `>> ` corrupts commands; honesty at the affordance is the mouse-first contract |
| D4 | Prediction **ghost text** = cells right of the cursor whose flags intersect `DIM \| ITALIC` (PSReadLine's default `InlinePredictionColor` is `\e[97;2;3m` — SGR 2 dim + 3 italic); customized prediction colors fail toward refusal | The heuristic can only lose convenience, never correctness: mistaking ghost for real text refuses reclaim; typed echo never renders dim/italic under PSReadLine defaults |
| D5 | History = **GUI-side aggregation** over the `BlockList` stores the App already holds; **zero protocol change** | `apply_snapshot` Attaches EVERY terminal in `SharedState` (gui/mod.rs ~614-627) and every Attach already ships a full `Blocks` sync — the daemon loads the sidecar on first journal touch even for dead terminals (daemon/mod.rs `journal()` → `block_store_base`) — so the data is ALREADY on the client; a wire query would duplicate a delivery path that exists |
| D6 | Deleted terminals' history **dies with Delete** | `DeleteTerminal` already destroys the journal + sidecar irrecoverably (P5 calls it "the one irrecoverable verb"); a GUI-side graveyard would contradict daemon ownership (inv. 3) and resurrect the deleted-artifact bug class |
| D7 | Ordering = **recency with exact-cmd dedupe** (most recent instance represents; ×N count badge); no blended frequency score | Predictable order beats clever ranking (P2's "navigation must be predictable" doctrine); dedupe IS the frequency compression — a command used 50× shows once, at its freshest position |
| D8 | Search = **tokenized AND-substring** over cmd + cwd, case-insensitive; no regex, no fuzzy scoring | This is command recall, not text search (P2 §6.2 precedent — scrollback search owns regex); multi-token AND ("git push" matches `git commit && git push`) covers the fuzzy need without unpredictable ranking |
| D9 | The popup lives **above the composer strip**, opened by a History button IN the strip; no header entry, no second surface | Insertion targets the composer directly below it (spatial adjacency); hookless tabs have no composer to insert into, and they have no strip — entry point and capability disappear together |
| D10 | Row click = **insert into draft** (stashing any displaced draft via the P3 recall mechanism); **Run** is a separate hover action enabled only when `mode == Compose` or `gate() == AutoArm` | Insert is always safe; Run reuses the exact P3 submit path — no second submission encoder, no way to type into a dirty prompt |
| D11 | Index built **lazily on popup open**, cached in the popup state, invalidated by a Blocks-frame stamp; rows rendered with `show_rows` | Zero cost while closed (inv. 4); a ≤10k-rec sort is open-click work (~ms once), never frame work |
| D12 | ArrowUp recall (P3 D9) stays per-terminal and unchanged; the popup is the cross-terminal surface | Quiet accelerator vs. browsable surface are different gestures; overloading ArrowUp to cross terminals would make recall unpredictable |

---

## 2. Feature A — Typeahead reclaim

### 2.1 UX (what changes vs P3 v1, exactly)

P3's manual-activation gesture is unchanged: at a prompt whose cursor sits past `prompt_end`
(stray typed text / landed type-ahead), auto-arm is refused (`ManualOnly`) and the strip shows
the `❯ Compose` ghost button. V2 changes what the click DOES and what the strip SAYS:

| Prompt state (gate core passes) | Strip left label (12px, TEXT_SECONDARY) | `❯ Compose` click does |
|---|---|---|
| clean (`cursor_clean`) | (P3 unchanged — auto-arm usually got here first) | arm, no bytes |
| dirty + `Reclaim::Text(t)`, t non-empty | `Typed text at the prompt — Compose keeps it` | draft ← draft ⊕ t (§2.4), send clear chord, arm, focus editor with caret at end |
| dirty + `Text("")` (only whitespace typed) | P3 v1 label | send clear chord, arm (nothing worth keeping) |
| dirty + `MultiLine` / `CursorMidLine` / `Unavailable` | `Typed text at the prompt — Compose clears it` | v1 behavior: clear chord, arm, draft untouched |

- The label IS the affordance the roadmap requires: visible, honest, and it changes before
  the click so the user is never surprised. Hover tooltip on the button repeats it with the
  mechanism ("Moves what you've typed into the editor" / "Cancels the typed line (Ctrl+C)").
- The whole strip stays click-to-activate (P3 §6.3) with the same v2 semantics — biggest
  target for the primary action.
- Reclaim is NEVER offered as a keyboard action and never fires from focus changes: the only
  trigger is the user's explicit activation click (D1).

### 2.2 Extraction — the pure function (src/gui/term_backend.rs)

```rust
/// What the grid holds between the prompt end and the cursor.
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

/// Free function, generic over the event listener so the PROBE can run it
/// against a Term it rebuilt from captured session bytes (same pattern as
/// `walk_to_logical_start` staying private but this one pub).
pub fn extract_input<L: alacritty_terminal::event::EventListener>(
    term: &Term<L>,
    prompt_end: (i32, usize), // (grid line — may be negative/history, col)
) -> Reclaim
```

Algorithm (each step one line, each load-bearing):

1. `cur = term.grid().cursor.point` (screen coords, `cur.line.0 ≥ 0`); `(pl, pc) = prompt_end`.
2. **Plausibility**: `pl > cur.line.0` (cursor above the capture — screen rewritten),
   `cur.line.0 - pl > RECLAIM_ROW_CAP`, or `pl < -(history_size as i32)` ⇒ `Unavailable`.
3. `pl == cur.line.0 && pc == cur.column.0` ⇒ `Text(String::new())` (clean; callers normally
   gate on dirty first — totality only).
4. `pl == cur.line.0 && pc > cur.column.0` ⇒ `Unavailable` (cursor LEFT of the prompt end on
   the same row: the prompt was re-rendered shorter; the capture lies).
5. **Wrap-chain check** — for every row `r` in `pl .. cur.line.0` (exclusive of the cursor
   row): `grid[Line(r)][Column(cols-1)].flags.contains(Flags::WRAPLINE)` must hold, else
   `MultiLine`. A hard newline inside the span means PSReadLine rendered a continuation
   prompt at the start of the next row (Shift+Enter / incomplete syntax); its text is the
   user-configurable `ContinuationPrompt` — refusal is the only correct move (D3).
6. **Cursor-row trailing check**:
   - `grid[Line(cur.line.0)][Column(cols-1)].flags.contains(Flags::WRAPLINE)` ⇒ the buffer
     continues BELOW the cursor ⇒ `CursorMidLine`.
   - Scan cells `cur.column.0 .. cols` on the cursor row: any cell with `c != ' '` whose
     flags do NOT intersect `Flags::DIM | Flags::ITALIC` ⇒ `CursorMidLine` (real text right
     of the caret — the user pressed Home/arrows). Dim-or-italic cells are PSReadLine
     prediction ghost text (D4) and are ignored. On PSReadLine 2.0 (inbox PS 5.1) predictions
     don't exist, so there ANY non-space right of the cursor is real ⇒ refuse — correct.
7. **Collect** — first row: columns `pc ..` ; interior rows: `0 ..` ; cursor row:
   `0 .. cur.column.0` (exclusive — the cursor sits after the last typed char); single-row
   span: `pc .. cur.column.0`. Per cell: skip `Flags::WIDE_CHAR_SPACER` and
   `Flags::LEADING_WIDE_CHAR_SPACER` (CJK spacers), push `cell.c` otherwise.
8. `Text(s.trim_end().to_string())` — trailing whitespace is submission-meaningless, and
   trimming makes the ghost-text boundary exact when the user typed a trailing space.

Cost: at most `RECLAIM_ROW_CAP × cols` cell reads — a few hundred cells in practice (a 1–2
row command), worst ~64k on a pathological 10k-char line; only ever runs on the selected
terminal in one strip state (§2.5) and at click time.

### 2.3 `TermBackend::reclaim_text` (the impure wrapper)

```rust
impl TermBackend {
    /// Reclaimable typed input at the current prompt, or why not. Pure read.
    pub fn reclaim_text(&self) -> Reclaim {
        let Some(bf) = &self.block_feed else { return Reclaim::Unavailable };
        if bf.stale { return Reclaim::Unavailable; }
        let Some(pe) = bf.prompt_end else { return Reclaim::Unavailable };
        // A pending DECSET-2026 sync block means the grid lags the stream —
        // the same "the cursor would lie" guard as P3's PromptEnd capture.
        if self.parser.sync_timeout().sync_timeout().is_some() {
            return Reclaim::Unavailable;
        }
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return Reclaim::Unavailable; // belt: gate already blocks alt
        }
        extract_input(&self.term, pe)
    }
}
```

`prompt_end` is P3's capture: shifted with history by `track_scroll` alongside anchors,
invalidated on ED3-below-it / saturation / resize (P3 §5.2) — every invalidation path lands
here as `Unavailable` ⇒ discard fallback. No new invalidation logic is needed.

### 2.4 Composer activation v2 (src/gui/composer.rs)

P3's `activate(&mut self, backend: &TermBackend) -> Vec<u8>` keeps its signature; its body
gains the reclaim branch:

```rust
pub fn activate(&mut self, backend: &TermBackend) -> Vec<u8> {
    let clean = backend.cursor_at_prompt_end(); // P3 accessor
    self.arm_manual(); // mode = Compose, want_focus = true (P3 semantics)
    if clean {
        return Vec::new();
    }
    if let Reclaim::Text(t) = backend.reclaim_text() {
        if !t.is_empty() {
            self.push_reclaimed(&t);
        }
    }
    clear_chord(backend) // P3 §4.2, unchanged: win32-encoded Ctrl+C / 0x03
}

/// Merge reclaimed prompt text into the draft. Never destroys either side.
fn push_reclaimed(&mut self, t: &str) {
    if self.draft.is_empty() {
        self.draft = t.to_string();
    } else {
        // Newline keeps the two fragments visually distinct and trivially
        // editable; the user reviews before submitting (and on PS 5.1 would
        // see two lines = two submissions — visible, not silent).
        self.draft.push('\n');
        self.draft.push_str(t);
    }
    self.caret_to_end = true; // one-frame flag; §7.3 applies it via TextEditState
    self.recall = None;       // an edit-equivalent: recall walk resets (P3 rule)
}
```

Ordering note (same as P3 v1): the chord bytes ship this frame; PSReadLine cancels the line
and re-renders a prompt ~ms later, which fires a fresh `pre` + `133;B` ⇒ `prompt_end`
recaptures and `cursor_clean` turns true — self-consistent. Keystrokes in flight between the
grid read and the chord landing die with the CancelLine — accepted, click-bounded, and the
reclaimed draft is a superset of what the user had at click time (P3 §4.2 residual class).

### 2.5 The strip label (per-frame preview, bounded)

`composer::show` computes the label for the ManualOnly-dirty state only:

```rust
/// Only called when: terminal selected, strip visible, gate() == ManualOnly,
/// !cursor_clean. One bounded grid read per frame in exactly that state.
fn activation_preview(backend: &TermBackend) -> ActPreview {
    match backend.reclaim_text() {
        Reclaim::Text(t) if !t.is_empty() => ActPreview::Keeps,
        _ => ActPreview::Clears,
    }
}
```

No caching: the read is a few hundred cells in the realistic case (µs against the measured
~240µs p50 frame), and a cache keyed on cursor position would go stale on Delete-key edits
(cursor doesn't move) — wrong labels are worse than µs. The `RECLAIM_ROW_CAP` bound keeps the
pathological case finite.

### 2.6 Reclaim failure modes (the honest contract)

| Situation | Result | User sees |
|---|---|---|
| Plain stray text, single logical line (incl. wrapped) | `Text` — reclaimed | "Compose keeps it"; draft contains the text, prompt clears |
| Type-ahead that landed at the fresh prompt | `Text` — reclaimed | same |
| Multi-line buffer (Shift+Enter / incomplete syntax, `>>` continuations) | `MultiLine` → discard | "Compose clears it" |
| Cursor moved left mid-line (Home/arrows) | `CursorMidLine` → discard | "Compose clears it" (CancelLine kills the whole buffer incl. post-cursor text — nothing partial) |
| PSReadLine prediction ghost after the typed text (PS7 defaults) | ghost ignored; typed part reclaimed | draft = typed text only, no ghost |
| Custom `InlinePredictionColor` without dim/italic | `CursorMidLine` → discard | convenience lost, never wrong text (D4) |
| Cold attach (no `prompt_end` yet — P3 §2.2) | `Unavailable` → discard | v1 behavior exactly |
| Resize since the prompt rendered (`prompt_end` invalidated) | `Unavailable` → discard | v1 behavior |
| Sync block pending (DECSET 2026) | `Unavailable` → discard | v1 behavior |
| IME composition in progress | nothing special — preedit never reaches the PTY/grid; only committed text echoes and reclaims | correct by construction |
| Selection highlight / syntax colors on the input | irrelevant — extraction reads `cell.c`, not colors | correct |
| Another client typing in the same frame | same residual as P3 §4.2 — click-bounded | rare, bounded |
| RPROMPT-style right-aligned prompt text (exotic custom prompts) | glued into the reclaimed text | documented residual; open question 3 |

---

## 3. Feature B — Clickable cross-session history

### 3.1 Data source: the GUI already has everything (why no protocol change)

Chain of existing facts, each verifiable in the tree today:

1. `apply_snapshot` (gui/mod.rs ~614-627) creates a backend and sends `C2D::Attach` for
   EVERY terminal in `SharedState` — running or Dead — on every snapshot, and after every
   reconnect (`terms.clear()` forces re-attach).
2. The daemon's Attach handler (daemon/mod.rs ~877-955) calls `self.journal(id)`, whose first
   touch loads the block sidecar (`block_store_base` → `BlockStore::load`), then enqueues a
   full `D2C::Blocks` sync under the same journal lock — **including for dead terminals**
   (the journal-tail fallback path still runs the full-sync block).
3. The GUI's `D2C::Blocks` arm upserts into `self.blocks: HashMap<Uuid, BlockList>`, which
   `apply_snapshot` prunes only for deleted terminals.

Therefore `self.blocks` is, at all times, the union of every existing terminal's sidecar-
persisted records (≤500 each, all epochs) — precisely the cross-session corpus. A daemon
aggregation query would re-deliver data the client already holds, add a wire variant that
must be sequenced against P5's appends (proto=4 coordination), and create a second source of
truth. **Decision: GUI aggregation, zero wire changes** (D5). The rejected-alternative is
recorded so nobody re-litigates it: if some future phase stops attaching all terminals (e.g.
lazy attach for 500-terminal workspaces), THAT phase adds the query — appended after P5's
variants, proto=4.

Retention: dead terminals appear (their sidecars persist and load on attach); deleted
terminals vanish with their sidecar (D6). Names come from `SharedState` at index-build time —
dead terminals still have metas, so attribution always resolves.

### 3.2 The index (new file: src/gui/history.rs)

```rust
use std::path::PathBuf;
use uuid::Uuid;
use crate::state::BlockRec;

/// Hard cap on index entries after dedupe (oldest dropped). 20 terminals ×
/// 500 recs = 10k worst-case input; 5k deduped entries ≈ a fraction of a MB.
pub const MAX_HISTORY: usize = 5000;

pub struct HistEntry {
    pub cmd: String,        // trimmed, as recorded (may contain \n on PSRL ≥2.2)
    cmd_lc: String,         // lowercase, for filtering
    pub term: Uuid,         // most-recent user's terminal
    pub term_name: String,  // resolved at build (dead terms still have metas)
    pub term_dead: bool,
    pub cwd: Option<PathBuf>,
    cwd_lc: String,         // lowercase display string, for filtering
    pub last_ms: u64,       // started_ms of the most recent use
    pub exit: Option<i64>,  // of the most recent use (None = open/never closed)
    pub open: bool,         // most recent use still running
    pub count: u32,         // total occurrences across all terminals/epochs
}

/// Aggregate + dedupe + order. Pure: unit-testable, probe-drivable.
/// `lists` = (terminal id, name, dead, recs sorted by start_off) per terminal.
/// Dedupe key = exact trimmed cmd string; the MOST RECENT instance (max
/// started_ms) is the representative; count accumulates. Sort: last_ms desc,
/// then count desc, then cmd asc (total order ⇒ stable UI).
pub fn build_index(lists: &[(Uuid, String, bool, &[BlockRec])]) -> Vec<HistEntry>

/// Tokenized AND-substring filter (D8): query split on whitespace; every
/// token must appear in cmd_lc OR cwd_lc. Empty query = identity. Returns
/// indices into `entries` (order preserved = recency).
pub fn filter(entries: &[HistEntry], query: &str) -> Vec<u32>
```

Build notes:
- Skip recs with blank `cmd` (the bootstrap already skips blank lines; belt only).
- `open` recs (end_off None) are included — a currently-running command is legitimate recall.
- `build_index` clones cmd strings; no borrows into `App.blocks` may be stored across frames
  (borrow discipline — the popup outlives any single borrow of the store).
- Truncate to `MAX_HISTORY` after sorting (drops the oldest tail).

Invalidation: `App.blocks_stamp: u64` — incremented in the `D2C::Blocks` drain arm, in
`apply_snapshot` when `blocks.retain` removed anything, and on reconnect (`blocks.clear()`).
The popup rebuilds when its `built` stamp differs (§3.3). One integer bump per Blocks frame —
free.

### 3.3 Popup state + UI (App-level, mirrors `blocks_panel_ui`)

```rust
/// gui/mod.rs
struct HistoryPopup {
    query: String,
    /// Keyboard-selected row: index into `hits`.
    sel: usize,
    /// Filtered indices into `entries` (recomputed on query change + rebuild).
    hits: Vec<u32>,
    entries: Vec<history::HistEntry>,
    built: u64, // blocks_stamp at build; drift ⇒ rebuild + re-filter
}

// App gains:
history: Option<HistoryPopup>,     // None ⇒ zero cost, no memory
history_btn_rect: Option<Rect>,    // click-outside exemption (blocks-panel pattern)
blocks_stamp: u64,
```

`fn history_popup_ui(&mut self, ctx: &egui::Context, strip_rect: Rect, id: Uuid)` — called
from `terminal_card` right after `composer::show` (it needs the strip rect for anchoring);
same deferred-action pattern as `blocks_panel_ui` (collect `Act`s, apply after the borrow).

Geometry & chrome:
- `egui::Area::new(Id::new(("history_popup", id))).order(Order::Foreground)`
  `.fixed_pos(pos2(strip_rect.left(), strip_rect.top() - 6.0)).pivot(Align2::LEFT_BOTTOM)` —
  grows UPWARD from the strip, hugging it at any content height.
- Width `min(640.0, strip_rect.width())`; `Frame` styled exactly like the blocks panel
  (SURFACE fill, BORDER stroke, radius 8, the same shadow recipe).
- Header row: search `TextEdit` (hint `Search command history`, `te.request_focus()` every
  frame — the blocks-panel/search precedent), right-aligned entry count caption
  (`{hits} of {entries}` in TEXT_MUTED 10px).
- Body: `ScrollArea::vertical().max_height(420.0).show_rows(ui, 44.0, hits.len(), …)` —
  O(visible) render, two-line rows:
  - line 1: status glyph (open ⇒ ACCENT dot; failed ⇒ `✕ {code}` DANGER 10px; success/None ⇒
    nothing — P2's "absence of red IS success"), `cmd` single-line-ified
    (`replace(['\r','\n'], " ")`) mono 12px ellipsized into its lane, right-aligned
    `time_ago_ms(last_ms)` TEXT_MUTED 10px.
  - line 2 (11px TEXT_MUTED): `{term_name} · {cwd middle-ellipsized 36}` (name in TEXT_FAINT
    when `term_dead`), plus `×{count}` badge when count > 1.
  - keyboard-selected row: ACCENT_SUBTLE fill; hovered row: SURFACE_2 (hover wins visually,
    keyboard sel persists).
- Hover action cluster (right side, mirrors the blocks panel rows): `[Copy]` (Icon::Copy —
  `ctx.copy_text(cmd)`), `[Run]` (Icon::Rerun — enabled per D10; disabled = TEXT_FAINT with
  tooltip "Shell is busy" / "Prompt has typed text — Compose first" / "No prompt yet").
- Row primary click (and Enter): **Insert** — §3.4.
- Empty states: no entries ⇒ centered TEXT_FAINT "No commands yet — hooked terminals record
  their history here"; no hits ⇒ "No matches".

Rebuild/refilter rules (start of `history_popup_ui`):
- `built != blocks_stamp` ⇒ rebuild `entries` from `self.blocks` + `self.state` (assemble the
  `lists` slice; O(total recs)), re-run `filter`, clamp `sel`.
- `te.changed()` ⇒ re-run `filter`, `sel = 0`.

### 3.4 Insert / Run semantics

```rust
// Deferred actions out of the popup closure:
enum HistAct { Insert(u32), Run(u32), Copy(u32) }
```

**Insert** (`row click` / `Enter`):
1. `let cmd = entries[i].cmd.clone();`
2. On the selected terminal's `ComposerState`: `insert_history(&cmd)` —

```rust
/// composer.rs. Replaces the visible draft with `cmd`, stashing the previous
/// draft in the recall slot so ArrowDown-past-newest restores it (the P3
/// recall gesture, one mechanism for both). Any edit drops the stash (P3 rule).
pub fn insert_history(&mut self, cmd: &str) {
    if !self.draft.is_empty() && self.draft != cmd {
        self.recall = Some((RecallSrc::History, self.draft.clone())); // §7.2 type change
    }
    self.draft = cmd.to_string();
    self.caret_to_end = true;
    self.want_focus = true;
}
```

3. Close the popup; composer takes focus (mode unchanged — if the gate is Blocked the draft
   simply waits; drafts survive everything, P3 D8).

**Run** (hover button; enabled iff `mode == Compose || gate() == AutoArm` for the SELECTED
terminal): `insert_history(&cmd)` then the composer's `submit(backend)` — the exact P3 path
(`submission_bytes` incl. BRACKETED_PASTE check, `Raw(PostSubmit)`, `episode_used`,
scroll-to-bottom). Then close the popup. Never bypasses the gate, never force-clears.

**Copy**: clipboard only, popup stays open (comparison shopping is a copy use case).

### 3.5 Entry point (strip button) + mutual exclusion

- `composer::show` gains a **History icon button** (new `Icon::History` — painter-drawn
  circle + two clock hands, 1.5px stroke, matching the icon language) in the strip's right
  cluster, visible in the states where an insertion target is visible or one click away:
  **Compose** (both focused/unfocused) and **ManualOnly/latent** (`❯ Compose` present).
  Hidden in Busy/AltScreen/Dead/NoPrompt-blocked (the editor isn't available — a button that
  opens a popup whose primary action can't land is a dead-end lie; open question 1).
- `ComposerOutput` gains `pub toggle_history: bool` and `pub history_btn: Option<Rect>`;
  `terminal_card` flips `self.history` on toggle and stores the rect in
  `self.history_btn_rect`.
- Mutual exclusion: opening the history popup sets `self.blocks_panel = None`; opening the
  blocks panel sets `self.history = None`; the search toggle (magnifier) also closes the
  history popup. One floating surface at a time — no z-fights, no double-Esc ambiguity.
- `select_terminal` closes it (`self.history = None`) alongside search/blocks panel — history
  content is cross-terminal but insertion targets the selected composer; reopening is one
  click and re-anchors to the new strip.

### 3.6 Focus routing, keyboard nav, Escape chain

Priority order (extends P3 §3 by one tier): **modal > search > blocks panel = history popup >
composer > grid** (the two popups never coexist, §3.5).

- Grid focus flag (`terminal_card`): `self.modal.is_none() && self.search.is_none() &&
  self.blocks_panel.is_none() && self.history.is_none() && !composer_focused` — the exact
  mechanism P2/P3 use, extended one term. The composer editor also must NOT `request_focus`
  while the popup is open: `composer::show` receives `overlay_open: bool` (true when either
  popup is open) and suppresses `want_focus` consumption for that frame (the flag persists,
  so focus lands when the popup closes).
- While open, the popup's search field holds egui focus (`request_focus` per frame). BEFORE
  adding the TextEdit, consume nav keys (P3's consume-before-show pattern):

```rust
let (up, down, enter) = ctx.input_mut(|i| (
    i.consume_key(Modifiers::NONE, Key::ArrowUp),
    i.consume_key(Modifiers::NONE, Key::ArrowDown),
    i.consume_key(Modifiers::NONE, Key::Enter),
));
```

  Up/Down move `sel` (clamped, no wrap — predictable at the ends), scrolling the selected row
  into view via `rowresp.scroll_to_me(Some(Align::Center))` on keyboard-driven changes only;
  Enter = Insert on `hits[sel]` (no-op when `hits` is empty). These arrows never reach the
  composer's recall — the popup is above it in the chain, and P3 recall only runs when the
  composer editor itself has focus.
- **Escape**: if `self.search.is_some()` the header's search-Esc wins (existing code) and the
  popup ignores the press (guard: `search.is_none()`); else the popup closes and, if the
  composer is armed, `want_focus` re-fires (one Esc = one layer, the P3 chain below is
  untouched: next Esc = composer→raw, grid Esc = byte to the shell).
- **Click-outside-closes**: primary press whose origin is outside the popup rect AND outside
  `history_btn_rect` closes it (the blocks-panel pattern verbatim, incl. the
  press-origin-not-release rule so drags out of the popup don't reopen).
- Mouse wheel over the popup scrolls the popup (Foreground-order Area — egui layer
  hit-testing already keeps wheel/clicks off the grid's raw handler; the blocks panel proves
  the pattern).

---

## 4. Protocol

**None.** No new variants, no proto bump; `DaemonInfo.proto` stays whatever the tree ships
(2 today, 3 after P5). P4 is GUI + probe only — it cannot collide with P5's appends by
construction. The only daemon-adjacent statement P4 makes is a compile-time visibility change
in the GUI module tree (§5).

---

## 5. File-by-file changes (with signatures)

### 5.1 src/gui/mod.rs (module decls + App wiring)

1. `mod term_backend;` → `pub mod term_backend;` and add `pub mod history;` — the probe
   (crate-root sibling) needs `crate::gui::term_backend::{TermBackend, Reclaim}` and
   `crate::gui::history::build_index` for §9's cases. Precedent: P2 made `daemon::blocks`
   pub for exactly this cross-module reuse; visibility only, zero behavior.
2. App fields: `history`, `history_btn_rect`, `blocks_stamp` (§3.3).
3. `drain_ipc` `D2C::Blocks` arm: `self.blocks_stamp += 1;` (one line, after the upsert).
4. `apply_snapshot`: bump `blocks_stamp` when the `blocks.retain` pass removed entries; also
   `self.history = None` when the SELECTED terminal was deleted (the popup's anchor died).
5. `reconnect_if_needed`: `self.history = None; self.blocks_stamp += 1;` next to
   `blocks.clear()`.
6. `select_terminal`: `self.history = None;` (alongside `search`/`blocks_panel`).
7. Search toggle (header magnifier): closing/opening search sets `self.history = None`.
8. Blocks-button click: opening the blocks panel sets `self.history = None`; the history
   toggle path sets `self.blocks_panel = None`.
9. Grid focus flag: add `&& self.history.is_none()` (§3.6).
10. `fn history_popup_ui(&mut self, ctx, strip_rect: Rect, id: Uuid)` (§3.3-3.4), called from
    `terminal_card` after `composer::show`; `Icon::History` added to the `Icon` enum +
    `draw_icon` arm.

### 5.2 src/gui/term_backend.rs

- `pub enum Reclaim` + `pub fn extract_input<L: EventListener>(…)` + `RECLAIM_ROW_CAP`
  (§2.2).
- `impl TermBackend { pub fn reclaim_text(&self) -> Reclaim }` (§2.3).
- Nothing else — `prompt_end` capture/shift/invalidate is P3's, untouched.

### 5.3 src/gui/composer.rs (P3's file, extended)

- `activate()` body gains the reclaim branch + `push_reclaimed` (§2.4). Signature unchanged.
- `ComposerState` gains `caret_to_end: bool` (one-frame flag; applied by loading
  `egui::text_edit::TextEditState`, setting the ccursor to the end, and storing it back
  before the TextEdit shows — the standard egui pattern) and the recall-source refinement:

```rust
pub enum RecallSrc { Recs(usize), History }
// P3's `recall: Option<(usize, String)>` becomes `Option<(RecallSrc, String)>`;
// recall_prev from History starts the walk at the newest rec (Recs(len-1));
// recall_next past-newest restores the saved string for BOTH sources.
```

- `pub fn insert_history(&mut self, cmd: &str)` (§3.4).
- `fn activation_preview(backend: &TermBackend) -> ActPreview` + the two strip labels (§2.1,
  §2.5).
- `composer::show` params: `overlay_open: bool` (§3.6); output gains `toggle_history` +
  `history_btn` (§3.5); History button drawn in the states of §3.5.

### 5.4 src/gui/history.rs (new)

`HistEntry`, `MAX_HISTORY`, `build_index`, `filter` (§3.2). Pure logic + unit tests; no egui
imports (the popup UI lives in mod.rs where App state is).

### 5.5 Untouched (explicitly)

protocol.rs, daemon/* (all), state.rs, strip.rs, win32_input.rs, ipc.rs, term_view.rs,
bindings.rs, glyph_cache.rs, theme.rs, journal/serialize/bootstrap/session/tracker. P4's
whole footprint is gui/{mod, term_backend, composer, history}.rs + probe.rs.

---

## 6. Performance budget (explicit)

| Cost | When | Bound |
|---|---|---|
| Reclaim preview (`activation_preview`) | per frame, ONLY selected terminal in ManualOnly-dirty strip state | ≤ RECLAIM_ROW_CAP×cols cell reads; realistic 1–2 rows (µs vs 240µs p50 frame) |
| Reclaim extraction at click | once per activation click | same bound |
| Index build | popup open + on stamp drift while open | O(total recs ≤ 10k) + sort; ~ms, click-time not frame-time |
| Filter | query change / rebuild only | O(entries × tokens) substring over pre-lowered strings |
| Popup render | per frame while open | `show_rows` O(visible ≈ 10 rows); galley cache absorbs text layout |
| `blocks_stamp` maintenance | per Blocks frame | one u64 increment |
| Popup closed / hookless tab | always | zero — `history` is None, no fields touched, no button drawn |
| Repaints | — | none self-scheduled; input-event driven only (inv. 7) |

---

## 7. Degraded modes — the honest contract

| Situation | Reclaim | History popup |
|---|---|---|
| Hookless-only workspace (claude/cmd tabs only) | n/a (no strip) | no entry point anywhere; index would be empty anyway (no Blocks recs) |
| Mixed workspace, viewing a hookless tab | n/a | no strip on THIS tab ⇒ no entry point; switch to a hooked tab to browse (documented; D9) |
| proto=1 daemon (P1: Blocks frames, no StreamPos) | no `prompt_end` (P3 needs the GUI scanner which works proto-free, but cold prompts only) ⇒ discard fallback | works — Blocks frames are proto 1 |
| proto=0 daemon | no Blocks frames ⇒ epoch 0 ⇒ no strip | no entry point, empty corpus |
| Dead terminal | n/a (gate Dead) | its commands listed (sidecar loaded on attach), name from meta, dimmed |
| Deleted terminal | — | rows vanish at the next snapshot (stamp bump ⇒ rebuild) — history dies with Delete (D6) |
| Terminal restored across epochs | reclaim unaffected | old-epoch recs persist (sidecar) and list normally |
| Huge sidecars (500-cap × many terminals) | — | 10k-rec build on open; MAX_HISTORY truncates the deduped tail |
| Journal-compaction-truncated recs | — | listed normally (cmd text is in the rec; only output was cut) — Run/Insert unaffected |
| PSReadLine ≥2.2 multi-line rec (embedded \n) | — | displayed single-line-ified; Insert preserves real newlines; Run inherits P3 paste semantics |
| Blocks panel open | — | opening history closes it (and vice versa) |
| GUI restart | cold-attach reclaim = Unavailable until first live prompt (P3 §2.2) | full corpus back after attach full-syncs (frames arrive with the replays) |

---

## 8. Probes (src/probe.rs — extend the suite; headless, no GUI attached)

Names are the contract, not the count (P5's rule). Both cases require P3's `PromptEnd`
scanner verb + `prompt_end` capture to be merged.

### 8.1 `reclaim_extract` — extraction against real PSReadLine echo bytes

1. Hooked pwsh terminal; `Attach { cols: 120, rows: 30 }`; record the `Replay` bytes, the
   `StreamPos` offset, and every subsequent `Output` frame in order (extend `Conn` with a
   small capture helper reusing `await_output`'s loop).
2. Await the first prompt; send `Input "RECLAIM_XYZ_77"` (NO enter); await its echo.
3. **Offline reconstruction** (this is the money move — the probe exercises the REAL GUI
   path): build a `crate::gui::term_backend::TermBackend` at 120×30, feed the Replay via
   `advance()`, then `set_stream_pos(off)`, `enable_block_scan()`, then the captured Output
   frames via `advance_live()` in order. Assert
   `backend.reclaim_text() == Reclaim::Text("RECLAIM_XYZ_77")`.
4. Chunk-invariance leg: a second backend fed the same Output bytes re-chunked at 7 bytes —
   identical result (ModeScanner ethos).
5. Wrapped leg: send a 150-char marker (`"RQ_" + "x"*147` — wraps at 120 cols); re-capture;
   assert the full 150-char string extracts (WRAPLINE walk).
6. Multi-line leg: send `Input "echo 'RECLAIM_ML"` then `\r` (incomplete string ⇒ PSReadLine
   goes into continuation mode and renders its continuation prompt); re-capture; assert
   `Reclaim::MultiLine`. Then win32-encoded Ctrl+C (reuse the `keys` encoding) to clean up.
7. Clear leg: after the Ctrl+C and the fresh prompt, re-capture; assert
   `reclaim_text() == Text("")` (clean prompt extracts empty).

### 8.2 `history_cross_session` — corpus correctness across terminals + epochs

1. Create hooked terminals A and B; in A run `echo HIST_A_1`, in B run `echo HIST_B_1`
   (`await_blocks` for the closed recs).
2. `KillTerminal A`; await Dead (snapshot pred); `RestartTerminal A` (epoch bump); await the
   post-resync full Blocks frame; run `echo HIST_A_2`.
3. Open a SECOND fresh `Conn` (simulating a GUI restart); Attach A and B; via `await_blocks`
   collect each terminal's full list. Assert: A's list contains `echo HIST_A_1` (old epoch)
   AND `echo HIST_A_2` (new epoch) with `A1.epoch < A2.epoch`; B's contains `echo HIST_B_1`.
   This proves the "cross-session corpus is already client-side" claim of §3.1 end-to-end,
   including the dead→restored sidecar path.
4. Feed the collected lists into `crate::gui::history::build_index` with fabricated
   names/dead flags; assert: three entries; order `HIST_A_2` first (recency); every entry
   carries the right terminal id.
5. Dedupe leg: run `echo HIST_B_1` in B again; re-collect; rebuild; assert ONE `HIST_B_1`
   entry with `count == 2` and `last_ms` updated (representative = newest).
6. Filter leg (pure): `filter(entries, "hist_a")` hits exactly the two A entries;
   `filter(entries, "echo a_2")` — tokenized AND — hits exactly `HIST_A_2`;
   `filter(entries, "")` is identity.

Register both in the `CASES` table after the blocks cases.

---

## 9. Unit tests (cargo test)

term_backend.rs (build streams with the existing `hook()` helper + a `\x1b]133;B\x07`
marker; all through `advance_live` so the real capture path runs):

- `reclaim_simple`: prompt + 133;B + echo `dir` ⇒ `Text("dir")`.
- `reclaim_wrapped`: 100-char input at 40 cols (WRAPLINE chain) ⇒ full string.
- `reclaim_wide_chars`: echo `漢字` (wide cells + spacers) ⇒ `Text("漢字")`, no spacer chars.
- `reclaim_multiline_refused`: echo + `\r\n>> more` (hard newline in the span) ⇒ `MultiLine`.
- `reclaim_ghost_ignored`: typed `git` + `\x1b[97;2;3m status\x1b[0m` + cursor repositioned
  to after `git` (CUB) ⇒ `Text("git")`.
- `reclaim_cursor_midline_refused`: typed `abcdef` + `\x1b[3D` (plain cells right of cursor)
  ⇒ `CursorMidLine`.
- `reclaim_unavailable_matrix`: no prompt_end / stale feed / mid-sync-block (`?2026h` open) /
  prompt_end below cursor ⇒ `Unavailable`.
- `reclaim_clean_is_empty_text`: cursor exactly at prompt_end ⇒ `Text("")`.

composer.rs:

- `activate_reclaims_into_empty_draft`: backend with staged dirty prompt ⇒ draft == text,
  return == clear chord bytes, mode == Compose.
- `activate_appends_below_existing_draft`: draft `"a"` + reclaim `"b"` ⇒ `"a\nb"`, caret-end
  flag set, recall cleared.
- `activate_falls_back_to_discard`: MultiLine-staged grid ⇒ draft untouched, chord returned.
- `insert_history_stashes_and_restores`: non-empty draft + insert ⇒ draft replaced, stash
  set; `recall_next` at newest ⇒ stash restored; an edit drops the stash (P3 rule).
- `run_enable_rule`: enabled iff `mode == Compose || gate() == AutoArm` — table over the P3
  verdicts.

history.rs:

- `index_dedupes_and_orders`: two terminals, duplicate cmds across epochs ⇒ one entry,
  count right, recency order, tiebreaks (count desc, cmd asc) exercised.
- `filter_tokens_and_case`: multi-token AND over cmd+cwd, case-insensitive, empty-query
  identity, no-hit case.
- `index_caps_at_max`: MAX_HISTORY+k deduped entries ⇒ oldest k dropped.
- `open_and_failed_flags`: open rec ⇒ `open`, failed rec ⇒ `exit` carried.

---

## 10. Interactive checklist (screenshot-verified; never run a second GUI instance while the
user's is open; never inject input while the user is active)

1. Hooked pwsh, composer armed: click into the grid, type `dir /w` raw (echo at the prompt).
   Strip shows "Typed text at the prompt — Compose keeps it". Click Compose: editor opens
   containing exactly `dir /w`, the prompt line visibly clears (one re-render), Enter runs it.
2. Type-ahead: run `ping -n 3 127.0.0.1`, type `cls` while it runs. At the next prompt no
   auto-arm (P3 unchanged); the strip offers "keeps it"; click ⇒ `cls` is in the draft.
3. On pwsh 7 with predictions: type `git` so ghost text renders; click Compose ⇒ draft is
   exactly `git` — no ghost text reclaimed.
4. Multi-line: type `echo 'abc` + Enter (continuation `>>`); strip says "clears it"; click ⇒
   line cancelled, draft empty (or prior draft intact).
5. History button appears in the strip (Compose/ManualOnly states only); click opens the
   popup ABOVE the strip; rows show cmd, terminal name, cwd, time, ×N badges; typing filters
   live; Up/Down moves the selection; Enter inserts and focuses the composer; Esc closes back
   to the composer.
6. Cross-terminal: a command run in terminal B appears in A's popup attributed to B; click
   inserts into A's composer; Run executes in A when A is at a clean prompt and is disabled
   with "Shell is busy" while A runs something.
7. Kill + Restore a terminal: its pre-restart commands still list (old epoch); a dead
   terminal's commands list with a dimmed name.
8. GUI restart: the popup shows the full corpus immediately after reconnect/attach.
9. Draft stash: half-typed draft + history insert ⇒ draft replaced; ArrowDown at the last
   line restores the stashed draft; editing after insert drops the stash.
10. Claude tab: no strip, no history button, zero render change (P3/P2 gates).
11. Blocks panel and history popup never coexist; search open closes the popup; Esc order is
    one-layer-at-a-time; click-outside closes without re-opening via the toggle button.
12. Selection drag over the grid while the popup is open still works (Foreground Area layer
    isolation — same as the blocks panel).

---

## 11. Open questions — each with the default the implementer should take

1. **History button in Raw(Busy)** (browse/copy while a command runs): default HIDDEN — the
   insertion target (editor) isn't visible, and a popup whose primary action can't land is a
   dead-end; revisit if the user asks for copy-while-busy.
2. **Enter-in-popup = Run instead of Insert when the gate is AutoArm**: default NO — Enter
   always inserts; running is a deliberate second action (hover button). Predictability over
   keystroke economy.
3. **RPROMPT-style custom prompts glue right-aligned text into reclaim**: default accept as a
   documented residual (exotic; the user sees the draft before submitting). A refusal
   heuristic (gap of ≥8 spaces?) would misfire on aligned typed text.
4. **Reclaim merge separator when the draft is non-empty**: default `\n` (visually distinct,
   trivially deletable; PS 5.1 two-submission consequence is visible in the editor).
   Alternative single space rejected: silently fuses two fragments into one wrong command.
5. **MAX_HISTORY = 5000**: default as spec'd; it is a memory/paint bound, not UX — bump
   freely if 20+ terminals × 500 recs of unique commands ever materializes.
6. **Ghost-text flag heuristic breadth** (`intersects(DIM|ITALIC)` vs `contains(DIM|ITALIC)`):
   default `intersects` — customized prediction colors that keep EITHER attribute still
   classify as ghost; full-custom colors refuse (safe side).
7. **Persist a global history file for deleted terminals**: default NO (D6). If demanded
   later it is a daemon-side feature (sidecar graveyard or global append log), not a GUI one.
8. **Popup stays open after Insert for multi-insert workflows**: default CLOSE (single-purpose
   gesture, focus returns to the editor where the user's eyes go). Copy keeps it open.

---

## 12. Explicit DO-NOTs (each traces to an invariant or past incident)

- Do NOT add protocol variants or bump `proto` — P5 owns proto=3; P4 ships zero wire changes
  (inv. 2). If a future query is ever added, append AFTER P5's variants, proto=4.
- Do NOT read `journals/*.blocks.json` (or any daemon file) from the GUI — Blocks frames are
  the interface; sidecars are daemon-owned (inv. 3; delete/resurrection incident class).
- Do NOT reclaim automatically, on a timer, or on keystroke — click-gated only (mouse-first
  doctrine; PSReadLine-blind contract; P3 D4's blast-radius argument).
- Do NOT guess-strip continuation prompts (`>>` is user-configurable `ContinuationPrompt`) —
  refuse multi-line (inv. 6).
- Do NOT send any PTY byte from reclaim except P3's click-gated clear chord — extraction is a
  pure grid read (inv. 1).
- Do NOT trust `prompt_end` without the staleness checks (stale feed / sync-pending /
  plausibility) — a wrong span reclaims garbage the user will submit (drop-don't-drift, P2
  §3.4 doctrine).
- Do NOT destroy drafts: reclaim appends, history insert stashes (inv. 5; P3 D8).
- Do NOT hold borrows into `App.blocks` inside the popup state — clone into `HistEntry`
  (borrow discipline; the popup outlives the borrow).
- Do NOT rebuild the index per frame — stamp-gated on open only (inv. 7 / D11).
- Do NOT let popup keys leak: consume Up/Down/Enter before the TextEdit, guard Esc on
  `search.is_none()`, and extend the grid-focus flag — a leaked arrow reaching the composer
  recall or a leaked Enter reaching submit is a keystroke-loss-class bug (P3 inv. 4).
- Do NOT let Run bypass the gate or fire from ManualOnly (stray text would prefix the
  command) — `mode == Compose || AutoArm` only (D10).
- Do NOT show the History button (or popup) on hookless tabs — no strip, no entry point, zero
  cost (inv. 4; the load-bearing epoch==0 gate).
- Do NOT anchor the popup anywhere but the strip (no header entry) — insertion adjacency is
  the design (D9).

---

## 13. Suggested implementation order (compile-green at each step)

1. **term_backend.rs**: `Reclaim` + `extract_input` + `reclaim_text` + §9 unit tests;
   `pub mod term_backend;` + (empty) `pub mod history;` decls. Pure additions, nothing calls
   them yet.
2. **history.rs**: `HistEntry` + `build_index` + `filter` + unit tests. Still inert.
3. **composer.rs**: `activate()` v2 + `push_reclaimed` + `caret_to_end` + `RecallSrc` +
   `insert_history` + `activation_preview` + strip labels + unit tests. The reclaim feature
   is now live end-to-end — run interactive checks 1–4.
4. **mod.rs**: `blocks_stamp` + App fields + strip History button plumb (`ComposerOutput`
   additions) + `history_popup_ui` + focus-flag/Escape/exclusion wiring (§5.1 items 2–10).
   Interactive checks 5–12.
5. **probe.rs**: `reclaim_extract` + `history_cross_session`; run the FULL suite against an
   installed daemon (Start-Process -Wait pattern; installed-vs-direct daemons have diverged
   before — CREATE_NEW_PROCESS_GROUP incident).
6. Interactive checklist with screenshots last (PowerShell CopyFromScreen; never a second GUI
   instance while the user's is open).
