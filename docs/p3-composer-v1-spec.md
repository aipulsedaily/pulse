# P3 "Composer v1" — Implementation Spec (final, implementation-ready)

> **As-of-writing note (historical plan).** Three areas below were superseded by later work
> and now live in the code, not this spec: (1) the **50ms settle** window is `SETTLE =
> Duration::ZERO` in `composer.rs` — any delay lost the race to the grid (episode_used ⇒
> ManualOnly); the code comment there has the root cause. (2) **D2's "prompt row stays bare,
> never replaced"** is superseded by the v2 **cover system** — the composer now covers the
> prompt row (SUBMIT_HOLD / INCOMING_COVER machinery, UX doctrine). (3) `RawReason` gained
> **`Asleep`** (sleep-spec). The gate/state-machine logic is otherwise live.

Target: C:\Terminal Control (egui 0.35 GUI + daemon). Builds directly on P2 "Blocks UI"
(docs\p2-blocks-ui-spec.md): the GUI-side `BlockFeed` scanner (§P2-3), the `BlockList` store +
`can_rerun` gate (§P2-4), and the P1 hook protocol (bootstrap `pre`/`exec` OSCs). **P3 depends on
P2 being merged first** — it extends `BlockFeed` and reuses `advance_scanned`'s split-feed.

The Composer is a native prompt editor drawn by the GUI at the bottom of the terminal view,
active only when the shell is demonstrably at an interactive prompt. While composing, ZERO bytes
reach the PTY per keystroke; typing latency is structurally zero. When the gate fails — TUI
running, alt-screen, hookless shell, output streaming — every key goes raw to the PTY exactly as
today.

Ordered as the implementation plan: decisions → gate/state machine → input routing → submission
→ backend changes → composer module → layout → mod.rs wiring → degraded table → probes → tests →
checklist. Each decision carries a one-line justification. Open questions at the end with
defaults.

---

## 0. Non-negotiable invariants (violating any is a bug)

1. **Mirror/parser purity**: the composer never injects a byte into any VT parser stream. All
   prompt detection is *observation* (the same scanner pass P2 already runs). The only bytes it
   ever writes to the PTY are user-intended input: the submission, and (on explicit click only)
   one clear chord.
2. **Hookless sessions cost zero**: a terminal whose `BlockList.epoch == 0` (claude tabs, cmd,
   custom) renders at exactly today's cost — no strip, no scanner state, no composer code path.
   The load-bearing gate is the same `epoch > 0` signal P2 uses.
3. **Raw mode is byte-identical to today**: when the composer is not focused/armed, the key path
   through `term_view::process_input` is untouched — same win32-input encoding, same VT fallback,
   same IME/paste handling.
4. **No keystroke is ever lost or doubled at a mode boundary**: egui has a single focus owner;
   keys go to the composer TextEdit XOR the grid, decided per frame by explicit rules (§3). Mode
   transitions never move focus away from an actively-typing user except as the direct result of
   the user's own action (submit / Esc / click).
5. **Grid geometry never depends on transient shell state**: the composer strip is a 36px
   reservation per hooked terminal — it does not appear/disappear with prompts or death.
   PTY resize events tied to composer behavior: zero (past incident class: resize storms wipe
   conhost). C2 amendment — the ONE sanctioned exception: a STABLE alt screen (held ≥ 400ms)
   collapses the strip and the terminal reclaims the rows, ±1 debounced resize per real TUI
   enter/exit (§7 "C2 refinement" — hysteresis, single-source predicate, recoverable feed).
6. **bincode compat**: **no protocol change at all.** Submission = existing `C2D::Input`; gate =
   existing `D2C::Blocks` + the GUI's own scanner; history = existing `BlockRec.cmd`.
   `DaemonInfo.proto` stays **2** (P2's value) — reuse, don't bump, because nothing on the wire
   changes.
7. **No repaint loops**: the only self-scheduled wakeup is one `request_repaint_after` for the
   50ms settle window; egui's own caret blink while the TextEdit is focused is accepted (it is
   egui-native and stops when unfocused).

---

## 1. Headline decisions (the shape of the feature)

| # | Decision | One-line justification |
|---|---|---|
| D1 | Composer lives in a **permanent 36px bottom strip** inside the terminal card, reserved for hooked terminals only; the editor **grows upward as an overlay** over the grid when multi-line | Constant reservation = zero PTY resizes tied to shell state (inv. 5); upward overlay growth means typing a 5-line script never re-wraps conhost |
| D2 | The shell's prompt row **stays visible and bare** in the grid above the strip; the composer never visually replaces it | Honest and simple: the real prompt (user's own customizations) remains the source of truth, and no synthetic bytes are needed (inv. 1) |
| D3 | Submission is **byte-identical to paste-then-Enter**: `term_view::paste()` encoding (bracketed iff `TermMode::BRACKETED_PASTE`) + `\r` | It is the one input path PSReadLine, conhost, and this codebase already handle everywhere (WT's own paste path, probe-verified under win32-input-mode) |
| D4 | **No blind clear at submit.** A clear chord (`Ctrl+C`, win32-encoded) is sent ONLY on explicit user click into a composer whose prompt provably holds stray text | Ctrl+C = `CancelLine` is the only edit-mode-independent line-kill in PSReadLine; firing it only on a deliberate click bounds the blast radius to user intent |
| D5 | Stray-text detection is **grid observation, not byte counting**: capture the cursor cell at the bootstrap's `OSC 133;B` (prompt end) and compare with the live cursor | Byte counting can't distinguish TUI-consumed keys from type-ahead queued for the next prompt; the grid cursor can, exactly |
| D6 | After submit the composer yields to raw **synchronously, same frame** — before the exec hook round-trips | Guarantees `claude` + Enter + immediate typing goes raw with zero lost/doubled keys (the user's explicit worry) |
| D7 | Raw typing at a prompt always wins: any raw key at an armed prompt dismisses the composer **for that prompt episode** (until the next `pre` hook) | The last input target the user chose is authoritative; no focus tug-of-war, no surprise focus steals mid-word |
| D8 | Per-terminal **draft survives everything except submit** (gate failures, tab switches, restores, reconnects) | Losing a half-written command to a background event is the cardinal editor sin |
| D9 | History recall (ArrowUp/Down at buffer edges) reads `BlockList.recs` — per-session, consecutive-dedupe; cross-session history is P4 | The records already exist, persist across epochs via the sidecar, and carry exactly the accepted command text |
| D10 | No new protocol, no daemon changes except one shared-scanner verb (§5.1) | The entire feature is observable client-side from what P1/P2 already ship |

---

## 2. Gate + state machine

### 2.1 Signals (all existing or feed-time observable)

| Signal | Source | Meaning |
|---|---|---|
| `hooked` | `BlockList.epoch > 0` (P2 §4.1) | this terminal spawns with the bootstrap; hooks exist |
| `running` | `TerminalMeta.status == Running` | process alive |
| `alt` | `backend.mode().contains(ALT_SCREEN)` | full-screen app owns the screen |
| `mouse` | `backend.mode().intersects(MOUSE_MODE)` | a primary-screen app is consuming input — not a prompt |
| `open_block` | any `BlockList` rec with `end_off == None` | a command (or TUI: claude, ssh, nested cmd/pwsh) is running |
| `at_prompt` | latched by a **live GUI-scanned `pre` hook** (feed-time, §5.2); cleared by `exec` hook / `Reset` / `Exited` / reconnect | the shell rendered a prompt and PSReadLine is reading |
| `prompt_end` | cursor cell captured at the GUI-scanned `OSC 133;B` (feed-time split, §5.2), line shifted with history like P2 anchors | where the prompt text ends; input area starts here |
| `cursor_clean` | live grid cursor == `prompt_end` (and `prompt_end` valid) | nothing typed/echoed after the prompt — safe to arm/submit with no clear |
| `episode_used` | set by ANY bytes this GUI sends to this PTY while `at_prompt` (raw keys, submit, clear chord); reset by the next `pre` | the user already directed input at this prompt; don't auto-arm over it |
| `settled` | ≥ 50ms since the `pre` latch | debounce rapid prompt cycling (`cls`, command bursts) |

Why the `pre` latch must be paired with `open_block` from the store: a `cat` of a log containing
old hook OSCs replays a stale `pre` into the GUI scanner (the GUI cannot check tokens — P2 §3.3),
but during any such command the daemon-verified store holds an OPEN block, so the arm check
fails. Spoof-proof without client-side tokens.

Why `at_prompt` needs a live hook and not just the store: the FIRST prompt of a session produces
no `D2C::Blocks` frame at all (a `pre` with no open block is a cwd-refresh only, daemon-side), so
the GUI's own scanner is the only signal that a fresh shell reached its prompt.

### 2.2 Attach cold-start heuristic (GUI restart onto an idle shell)

After attach, no live `pre` will arrive until the next prompt renders — but "20 idle shells,
reopen GUI" is the main use case. Arm tentatively at attach iff:

```
hooked && running && !alt && !mouse
  && recs.iter().all(|r| r.end_off.is_some())          // nothing running
  && recs.last().is_some_and(|r| r.epoch == list.epoch) // a command completed IN THIS SPAWN
```

The current-epoch requirement is load-bearing: a CLI-restore wrapper terminal (daemon `launch()`
bakes `claude --resume …` into `-Command`) is `hooked` with all OLD-epoch recs closed while
claude is still running — the trailing command never passes through `PSConsoleHostReadLine`, so
it opens no block. Requiring a closed rec from the current epoch proves this spawn reached an
interactive prompt after its trailing command finished. Terminals that fail the heuristic simply
stay Raw until their next real `pre` hook — degraded, never wrong.

`prompt_end` is unknown at attach (the replay is a serialized reconstruction, no OSCs), so
`cursor_clean` is false ⇒ cold-start arms are **manual-only** (the strip shows the Compose
button; click = clear-if-dirty + arm). One click after a GUI restart is the honest price; the
alternative (arming blind over possible stray text) corrupts commands.

### 2.3 Modes

```rust
/// Per-terminal composer mode. Raw is the default and the fallback.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ComposerMode {
    /// Editor shown; when it has egui focus, keys land in the draft only.
    Compose,
    /// All keys go to the PTY exactly as today. The reason drives the strip's label.
    Raw(RawReason),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RawReason {
    Busy,        // open block (command / TUI running) — strip shows cmd + elapsed
    AltScreen,   // full-screen app
    NoPrompt,    // hooked but no at_prompt latch yet (fresh spawn, cold attach)
    UserRaw,     // user typed raw / clicked grid at an armed prompt (episode_used)
    PostSubmit,  // between composer submit and the exec/pre that resolves it
    Dead,        // session exited
}
```

There is no separate "Transitioning" state: transitions are single-frame and synchronous
(D6/D7); the 50ms settle lives in the arm *condition*, not in a state.

### 2.4 Transition table (exhaustive)

| From | Event | To | Notes |
|---|---|---|---|
| any Raw | `pre` hook scanned (live) | Raw(NoPrompt→armable) | latch `at_prompt`, capture pending settle, reset `episode_used`; start 50ms timer |
| Raw, `at_prompt` | settle elapsed && gate passes && `cursor_clean` && `!episode_used` | **Compose** (auto-arm, takes focus from grid only) | the headline UX: pause at a fresh prompt ⇒ typing lands in the composer |
| Raw, `at_prompt` | user clicks strip's Compose button / editor body | **Compose** (manual) | if `!cursor_clean`: send win32 `Ctrl+C` first (D4); always allowed when gate core passes |
| Compose | user submits (Enter) | Raw(PostSubmit), **same frame** | bytes shipped; draft cleared; grid focus flag set; `episode_used = true` |
| Compose | user presses Esc / clicks into grid | Raw(UserRaw) | draft kept; grid focused; strip shows Compose affordance |
| Compose (unfocused) or armed | raw key sent to PTY at this prompt | Raw(UserRaw) | D7 — raw typing wins the episode |
| Compose (focused) | `exec` hook scanned (external client submitted / spoof) | stays **Compose**, submit disabled, strip shows "waiting for prompt" | never yank focus from a typing user (inv. 4); nothing they type reaches the PTY, so it is safe |
| Compose (unfocused, empty draft) | `exec` hook scanned | Raw(Busy) | quiet dismissal |
| any | `exec` hook scanned | clear `at_prompt`; `open_block` follows via Blocks frame | instant disarm signal, feed-time — beats the D2C::Blocks round-trip |
| any | `D2C::Reset` (restore) / `Exited` / reconnect | Raw(NoPrompt / Dead), draft kept | fresh session's first live `pre` re-arms |
| any | alt-screen entered | Raw(AltScreen), draft kept | belt over the open-block signal |

The claude timeline (the user's worry, step by step):
1. Frame N: user hits Enter on draft `claude`. Composer ships `"claude"` + `\r` via `C2D::Input`,
   sets `mode = Raw(PostSubmit)`, sets `episode_used`, clears the draft, flags the grid to take
   focus. **All in this frame — no waiting on any hook.**
2. Frame N+1: grid has egui focus. Every key from here goes through today's raw path.
3. ~ms later: PSReadLine echoes + accepts; the `exec` hook arrives in the output stream; the GUI
   scanner clears `at_prompt`; the Blocks frame marks the block open ⇒ `Busy`. Claude draws.
   Keys the user typed at frames N+1…N+k were already raw — nothing lost, nothing doubled.
4. Claude exits (hours later) → shell prompt → `pre` hook → `episode_used` reset (keys typed into
   claude were sent while `at_prompt == false`, so they never set it) → `cursor_clean` true at
   the fresh prompt → auto-arm. Composer returns by itself.

Type-ahead timeline (why D5 exists): user runs `sleep 5`, types `dir` while it runs (raw,
`at_prompt == false`). Prompt returns → `pre` latch → PSReadLine echoes the queued `dir` →
cursor sits PAST `prompt_end` → `cursor_clean` false → **no auto-arm**; the user's type-ahead
continues raw, exactly like a normal terminal. Manual click would `Ctrl+C` the stray text first.

### 2.5 The pure gate function (unit-testable)

```rust
/// src/gui/composer.rs
pub struct GateInputs {
    pub hooked: bool, pub running: bool, pub alt: bool, pub mouse: bool,
    pub open_block: bool, pub at_prompt: bool, pub settled: bool,
    pub cursor_clean: bool, pub episode_used: bool,
}

#[derive(PartialEq, Debug)]
pub enum GateVerdict {
    AutoArm,          // all conditions — take focus, show editor
    ManualOnly,       // core passes but cursor dirty / episode used — show Compose button
    Blocked(RawReason),
}

pub fn gate(i: &GateInputs) -> GateVerdict {
    if !i.hooked            { return GateVerdict::Blocked(RawReason::NoPrompt) } // strip absent anyway
    if !i.running           { return GateVerdict::Blocked(RawReason::Dead) }
    if i.alt                { return GateVerdict::Blocked(RawReason::AltScreen) }
    if i.open_block         { return GateVerdict::Blocked(RawReason::Busy) }
    if i.mouse              { return GateVerdict::Blocked(RawReason::Busy) }
    if !i.at_prompt || !i.settled { return GateVerdict::Blocked(RawReason::NoPrompt) }
    if i.cursor_clean && !i.episode_used { GateVerdict::AutoArm } else { GateVerdict::ManualOnly }
}
```

Evaluated once per frame per SELECTED terminal only (composer is invisible for others; their
state still updates from scanner events in `drain_ipc`, which is O(events), not per-frame).

---

## 3. Keyboard routing & focus model

Single source of truth, priority order: **modal > search > composer > grid.**

- `terminal_card` computes the grid's `focused` flag as
  `self.modal.is_none() && self.search.is_none() && !composer_has_focus` — the exact mechanism
  search already uses, extended by one term. The grid's `request_focus()` (term_view.rs:157)
  therefore never fights the composer.
- The composer editor is a real `egui::TextEdit::multiline` with `.lock_focus(true)` (Tab inserts
  instead of cycling widget focus — the P2-era focus-cycling bug class) and
  `.id(Id::new(("composer", terminal_id)))` (per-terminal state, no bleed between tabs).
- **Enter/Shift+Enter**: before showing the TextEdit each frame, call
  `ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))` — if it returns true and the
  editor had focus, submit. Shift+Enter is NOT consumed and reaches the TextEdit as a newline.
  Justification: consume-before-show is the standard egui pattern; the TextEdit never sees the
  plain Enter so no phantom newline needs trimming.
- **ArrowUp/ArrowDown**: consumed for history recall ONLY when the caret is on the first/last
  line of the draft (read `egui::text_edit::TextEditState::load(ctx, id)` →
  `state.cursor.char_range()` → count `\n` before the index). Otherwise they move the caret
  normally. Justification: standard readline recall UX without hijacking multi-line navigation.
- **Escape**: egui's TextEdit natively surrenders focus on Escape; we then set
  `mode = Raw(UserRaw)` and let the grid re-take focus. Draft kept.
- **Ctrl+C in the composer**: egui-native copy of the TextEdit selection; with no selection it
  does nothing. It NEVER reaches the PTY (nothing is running — the gate proved it; PTY-blind is
  the contract). Ctrl+Z/Y: egui TextEdit's built-in undoer. Ctrl+V: egui-native paste, newlines
  preserved, any size.
- **Clicking the grid** while composing: term_view's response click is detected in
  `terminal_card` (`resp.clicked()` on the grid response) → `composer.blur_to_grid()` →
  `Raw(UserRaw)`. Scrollback selection/copy by mouse **already works while the composer has
  focus**: `process_input` gates mouse events on `hovered`, not `has_focus` (term_view.rs:241),
  so drag-select/copy over the grid needs no changes and does not disturb composer focus (egui
  only moves focus on widget interaction, and the grid only requests focus when our flag says
  so).
- **Search overlay open**: search's TextEdit calls `request_focus()` every frame (mod.rs:1586) —
  it wins; the composer stays visible-unfocused; typing lands in search. Closing search returns
  focus by the normal per-frame flags.
- **Tab-switch** (`select_terminal`): if the new terminal's composer is armed (Compose mode), it
  takes focus; drafts are per-terminal and untouched. Justification: consistency with the
  headline UX — an armed prompt means typing composes.
- **IME**: egui TextEdit has full IME support (better than the grid path — this is a feature of
  composing, not a risk).
- Same-frame residue: events after a consumed Enter in the SAME frame batch (sub-16ms) land in
  the now-empty TextEdit as fresh draft — visible to the user, submittable, never sent to the
  PTY. Accepted (humanly unreachable, and fails visible-and-safe).

---

## 4. Submission path

### 4.1 Encoding (exact)

```rust
fn submission_bytes(backend: &TermBackend, draft: &str) -> Vec<u8> {
    let text = draft.trim_end();                 // trailing \n would double-submit on 5.1
    let sanitized = text.replace("\r\n", "\r").replace('\n', "\r");
    let mut out = Vec::with_capacity(sanitized.len() + 16);
    if backend.mode().contains(TermMode::BRACKETED_PASTE) {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(sanitized.as_bytes());
    }
    out.push(b'\r');                             // accept-line, OUTSIDE the brackets
    out
}
```

One-line justifications, per requirement:
- **UTF-8 text passthrough** is valid under win32-input-mode — it is WT's own paste path,
  already probe-verified end-to-end in this codebase (`keys` probe, rerun path in P2 §4.2).
- **Bracketed iff the mode is set**: `TermMode::BRACKETED_PASTE` in the GUI backend is ground
  truth — PSReadLine ≥ 2.2 (PS 7.2+) sets DECSET 2004 while ReadLine is active, PSReadLine 2.0
  (Windows PowerShell 5.1) never does; and the attach replay re-asserts the mode for late
  attachers (serialize.rs:306), so the flag is correct even right after a GUI restart. Sending
  `ESC[200~` to a shell that never requested it would leak literal garbage into PSReadLine 2.0 —
  the mode check makes that impossible by construction.
- **Multi-line semantics follow paste semantics** (that IS the mental model users have):
  - PSReadLine ≥ 2.2: the whole block is inserted as one multi-line buffer; the trailing `\r`
    accepts it as ONE submission → ONE exec hook whose `cmd` contains embedded `\n` (shell-side
    truncated at 2000 chars — `BlockRec.cmd` may be partial for huge scripts; execution is not).
  - PSReadLine 2.0 (PS 5.1): each `\r` accepts a line → N sequential submissions → N blocks, and
    a syntactically incomplete line falls into PSReadLine's native continuation. Identical to
    pasting into this terminal (or Windows Terminal) today. Documented, probe-asserted (§10.2).
- **Size**: one `C2D::Input` frame; `MAX_FRAME` is 32 MiB and the daemon `write_all`s to the
  ConPTY input pipe — no client-side chunking needed. No artificial cap (a 100 KiB paste-script
  is a legitimate submission).
- After shipping: `backend.scroll_to_bottom()` (same as `write_and_pin`), draft cleared, recall
  index reset, `mode = Raw(PostSubmit)` (D6).

### 4.2 The clear chord (manual activation over a dirty prompt only)

```rust
fn clear_chord(backend: &TermBackend) -> Vec<u8> {
    if backend.win32_input {
        crate::win32_input::encode_key(egui::Key::C, egui::Modifiers::CTRL).unwrap()
    } else {
        vec![0x03]
    }
}
```

- **Why Ctrl+C and not Escape/Ctrl+Home**: PSReadLine binds Ctrl+C to `CopyOrCancelLine` (no
  selection ⇒ `CancelLine`) in both Windows and Emacs edit modes, and we KNOW nothing is running
  (the gate proved no open block), so it can only cancel pending line input. Escape is
  `RevertLine` only in Windows mode — in Emacs mode it is a meta prefix and in Vi mode it enters
  command mode, where the following paste would be interpreted as editor commands (catastrophic).
  Ctrl+Home/End (`BackwardDeleteInput`/`ForwardDeleteInput`) are Windows-mode-only bindings.
- Cost: one cancelled line + one extra prompt render (which fires a fresh `pre` hook and cleanly
  re-latches — self-consistent). Visual churn accepted; it is honest and only on explicit click.
- Ordering: chord and any subsequent submission bytes may share a write — conhost's input queue
  is strictly ordered and PSReadLine processes sequentially; `CancelLine` completes, the host
  re-prompts, the next ReadLine consumes the queued text. Probe-verified (§10.1).
- Vi edit mode: Ctrl+C cancels in insert mode but is less uniformly bound — documented residual
  (open question 5). Never triggered without a click.
- Second-GUI residual: another client's typed text isn't observable via our `episode_used`, but
  IS observable via `cursor_clean` (the echo moves the grid cursor) — so even foreign stray text
  gates auto-arm correctly. The unobservable case is text typed on another client mid-flight in
  the same frame — accepted, same class as P2 §4.2's residual.

### 4.3 What the hooks record

`exec`'s `cmd` = exactly what `PSConsoleHostReadLine` returned: the full (multi-line on ≥2.2)
accepted text, truncated shell-side at 2000 chars. Because submissions flow through the real
ReadLine, **PSReadLine's own history gets the command too** — raw-mode ArrowUp keeps working for
users who mix modes. No special-casing needed anywhere.

---

## 5. Backend changes (src/gui/term_backend.rs + shared scanner)

### 5.1 Shared scanner: one new verb — `PromptEnd` (daemon-inert)

`daemon/blocks.rs`:

```rust
pub enum HookVerb {
    Init { pid: u32 },
    Exec { cmd: String },
    Pre  { exit: Option<i64>, n: u32, cwd: String },
    /// OSC 133;B — end of the rendered prompt string (emitted by the bootstrap
    /// between the prompt text and PSReadLine taking over). Carries no token;
    /// GUI-side prompt-end capture only. APPENDED as the last variant.
    PromptEnd,
}
```

In `parse_hook`, before the `7717;` check: `if body == b"133;B" { return Some(BlockEvent {
token: String::new(), verb: HookVerb::PromptEnd, offset_in_chunk: offset_after }); }`.
Daemon-side `on_block_event` adds `HookVerb::PromptEnd => None` under the token check's
*bypass* — concretely: early-return BEFORE the token comparison for `PromptEnd` (it has no
token, mutates nothing, notifies nothing). One-line justification: reusing the already-running
DFA costs zero extra scan passes; a second scanner would drift (P2 §2.1's argument). The
bootstrap already emits `133;B` (bootstrap.rs:48) — **no shell-side change**.

Spoof note: a printed `133;B` can only fake a *clean* cursor capture at a wrong position; the
worst case is arming over stray text = P2's accepted re-run residual, and only under active
same-user shenanigans. `133;A` remains ignored.

### 5.2 `BlockFeed` extensions (P2 §3.2 struct, three new fields + capture)

```rust
pub struct BlockFeed {
    // …P2 fields…
    /// Monotonic counters, bumped at feed-time. The composer diffs them per
    /// frame to see events without a callback plumb.
    pub pre_seen: u64,
    pub exec_seen: u64,
    /// Cursor cell captured at the last PromptEnd, in the same grid space as
    /// anchors (line shifted by track_scroll, invalidated with stale/alt/resize).
    pub prompt_end: Option<(i32 /*line*/, usize /*col*/)>,
}
```

In `advance_scanned` (P2 §3.3), inside the event loop:

- Bump `pre_seen` / `exec_seen` for `Pre` / `Exec` **before** the pending-sync-block `continue`
  — the events are real regardless of grid deferral (the latch is stream truth, not grid truth).
- `HookVerb::PromptEnd` (after the split-feed, so the grid is parsed up to the marker):
  `bf.prompt_end = Some((cursor.line.0, cursor.column.0))` — but ONLY if a sync block is not
  pending (a deferred grid would lie about the cursor; a prompt inside a sync block is not a
  real interactive prompt anyway, same reasoning as P2's anchor skip). Also gated on `enabled`.
- `track_scroll` shifts `prompt_end.0` together with anchors on history growth; history shrink
  with `line < 0`, ring saturation (`stale`), and alt-screen resize all set `prompt_end = None`
  (drop-don't-drift, P2 §3.4 doctrine). `resize_to`'s reflow remap: **do not remap** —
  `prompt_end = None` (a conhost resize repaint may re-wrap the prompt row; a wrong prompt-end
  is worse than a missing one; the next `pre` recaptures it).

New accessor:

```rust
impl TermBackend {
    /// True when the live cursor sits exactly at the captured prompt end —
    /// i.e. PSReadLine's input buffer is visibly empty.
    pub fn cursor_at_prompt_end(&self) -> bool {
        let Some(bf) = &self.block_feed else { return false };
        let Some((line, col)) = bf.prompt_end else { return false };
        if bf.stale { return false; }
        let cur = self.term.grid().cursor.point;
        cur.line.0 == line && cur.column.0 == col
    }
}
```

Cost: two integer compares per frame for the selected terminal; feed-time capture is one cursor
read per prompt render. Hookless sessions: `block_feed` is `None` — zero cost (inv. 2).

---

## 6. Composer module (new file: src/gui/composer.rs)

### 6.1 Types

```rust
use std::time::Instant;

pub const STRIP_H: f32 = 36.0;          // constant reservation (D1)
const EDITOR_MAX_ROWS: usize = 8;       // beyond this the editor scrolls internally
const SETTLE: std::time::Duration = std::time::Duration::from_millis(50);

pub struct ComposerState {
    pub mode: ComposerMode,
    pub draft: String,
    /// Prompt latch + settle timestamp (None = not at a prompt).
    at_prompt_since: Option<Instant>,
    /// Bytes were sent to the PTY during this prompt episode (D7).
    episode_used: bool,
    /// Last-seen BlockFeed counters, for edge detection.
    last_pre: u64,
    last_exec: u64,
    /// History recall: (index into recs walking backwards, draft saved before recall).
    recall: Option<(usize, String)>,
    /// One-frame flag: the editor should request egui focus this frame.
    pub want_focus: bool,
}
```

`App` gains `composers: HashMap<Uuid, ComposerState>` (entry created lazily on the first Blocks
frame with `epoch > 0` — hookless terminals never allocate one; inv. 2).

### 6.2 Core functions (signatures + contracts)

```rust
impl ComposerState {
    /// Per-frame signal pump for the SELECTED terminal (cheap: counter diffs).
    /// Applies transitions from §2.4. Returns the next wakeup needed for the
    /// settle window, if any (caller feeds request_repaint_after ONCE).
    pub fn tick(&mut self, feed: &BlockFeed, now: Instant) -> Option<Instant>;

    /// Called from drain_ipc for EVERY terminal (selected or not) on scanner
    /// events, Reset, Exited — keeps unselected composers truthful.
    pub fn on_stream_events(&mut self, feed: &BlockFeed);
    pub fn on_reset(&mut self);        // restore/reconnect: Raw(NoPrompt), keep draft
    pub fn on_exited(&mut self);       // Raw(Dead), keep draft

    /// Any raw bytes about to be sent to this terminal's PTY (from term_view's
    /// out.write). Sets episode_used when at a prompt; dismisses an armed
    /// composer (D7).
    pub fn on_raw_input(&mut self);

    /// Explicit user activation (strip click). Returns bytes to send FIRST
    /// (the clear chord) when the prompt is dirty, else empty.
    pub fn activate(&mut self, backend: &TermBackend) -> Vec<u8>;

    /// Submit: returns the submission bytes and flips to Raw(PostSubmit).
    pub fn submit(&mut self, backend: &TermBackend) -> Vec<u8>;

    /// ArrowUp/Down recall over `recs` (BlockList, sorted oldest→newest):
    /// walk backwards skipping consecutive duplicates and empty cmds; first
    /// ArrowUp saves the draft; edits (any text change) drop `recall`;
    /// ArrowDown past the newest restores the saved draft.
    pub fn recall_prev(&mut self, recs: &[BlockRec]);
    pub fn recall_next(&mut self, recs: &[BlockRec]);
}

/// Draw strip + editor. Returns bytes to ship to the daemon (submission /
/// clear chord) and whether the composer holds egui focus this frame.
pub struct ComposerOutput { pub write: Vec<u8>, pub has_focus: bool }

pub fn show(
    ui: &mut egui::Ui,
    strip_rect: egui::Rect,          // the reserved 36px band inside the card
    grid_rect: egui::Rect,           // for upward editor growth clamping
    state: &mut ComposerState,
    backend: &TermBackend,           // mode flags, cursor_clean, win32_input
    recs: &[BlockRec],               // history + open-block display
    running: bool,
) -> ComposerOutput;
```

`gate()` (§2.5) is a free function in this module; `tick` composes `GateInputs` from
`BlockList` + backend + its own latches.

### 6.3 Strip UI (mouse-first, every state clickable or labeled)

All painter-drawn in the P2 chrome style (SURFACE_2 fill, BORDER hairline top edge, 12px text):

| State | Left side | Right side |
|---|---|---|
| Compose (focused) | the editor itself (see below) | `Run ▸` button (accent; `TEXT_FAINT` when draft empty — empty Enter still submits a bare `\r` prompt-refresh) + hint "Shift+Enter — new line" in TEXT_FAINT 10px |
| Compose (unfocused) | editor body with draft (or hint "Type a command…"), dimmed border | `Run ▸` dimmed |
| Raw(Busy) | spinner-dot + open block's `cmd` (middle-ellipsized 48) + live elapsed (`fmt_duration`, P2 §11.6) | — (gate fails; no compose button — never a dead-end lie) |
| Raw(NoPrompt/UserRaw) with gate `ManualOnly`/latent | keyboard glyph + "Typing goes to the terminal" | `❯ Compose` ghost button (ACCENT text) — THE fallback affordance, always clickable when the gate core passes |
| Raw(AltScreen) | "Keys go to the app" TEXT_FAINT — once the alt screen has been held ≥ 400ms (`HIDE_AFTER`, C2) the strip COLLAPSES: the terminal reclaims the 36px (one debounced PTY resize; same `strip_hidden` predicate drives paint and `layout_for`). Pointer over the bottom band shows a translucent look-only peek OVERLAY over the grid. | — (right cluster rides the collapse; asleep/reconnecting/dead lanes never collapse) |
| Raw(Dead) | "Session ended" TEXT_FAINT | — (header owns Restore) |

- The strip itself (outside buttons) is click-to-activate when the gate core passes —
  the biggest possible target for the primary action (mouse-first doctrine).
- While Compose is active, a small `⌨` icon button on the strip's far right toggles back to raw
  (`Raw(UserRaw)`) — the visible, clickable escape hatch the roadmap requires.
- **Editor**: `TextEdit::multiline(&mut draft).font(FontId::monospace(prefs.font_size))
  .desired_rows(1).lock_focus(true).frame(false)` inside the strip when single-line; when the
  draft has n > 1 lines, the editor is hosted in an `egui::Area` (id `("composer_pop", tid)`,
  `Order::Foreground`) anchored to the strip's top edge growing upward:
  `height = min(n, EDITOR_MAX_ROWS) * row_h + 12`, width = strip width, SURFACE_2 fill, BORDER
  stroke, radius 6 (top corners), with an internal `ScrollArea::vertical()` past 8 rows. The
  grid underneath keeps its geometry — occlusion is transient and presentational (D1).
- Editor visuals: no chrome duplication of the shell prompt — just a single ACCENT `❯` glyph
  left of the text (D2: the real prompt lives in the grid, bare, directly above).
- Focus: when `want_focus` is set (auto-arm, manual activation, tab-switch to armed), call
  `response.request_focus()` once and clear the flag.

Repaint discipline: the strip's Busy elapsed-timer repaints ride the existing Working-pulse
`request_repaint_after(100ms)` (mod.rs:2520) — no new loop; the settle wakeup is one
`request_repaint_after` from `tick`.

---

## 7. Layout — the strip reservation and the shared-geometry landmine

`terminal_card` (mod.rs:1676) currently computes ONE `layout` from `ui.available_size()` and the
resize-commit loop applies it to **every** backend (mod.rs:1732-1739). A per-terminal strip
breaks that assumption — if hooked terminals get `avail - 36px` and hookless get `avail`,
switching tabs between a claude tab and a pwsh tab must NOT flip a shared grid size back and
forth (that would be a resize storm on every tab switch — the exact ConPTY-wipe incident class).

**Change (precise):**

1. `last_grid`/`pending_grid` keep storing the **base** card geometry (unchanged semantics —
   the debounce/throttle logic is untouched).
2. Add `fn hooked(&self, id: Uuid) -> bool { self.blocks.get(&id).is_some_and(|b| b.epoch > 0) }`
   and `fn layout_for(&self, id: Uuid, base: Vec2) -> Vec2 { if self.hooked(id) {
   Vec2::new(base.x, (base.y - composer::STRIP_H).max(0.0)) } else { base } }`.
3. The commit loop becomes: `for tid in ids { let l = self.layout_for(tid, layout); if let
   Some((c, r)) = b.resize_to(l, cell) { self.send(C2D::Resize { .. }) } }` — each terminal owns
   a stable geometry; tab switches change nothing.
4. `apply_snapshot`'s attach-time backend sizing uses `layout_for` too. Caveat: on the FIRST
   attach of a hooked terminal the Blocks frame (which carries `epoch`) hasn't arrived yet, so
   the Attach announces the un-shrunk grid; when the full Blocks sync lands (same drain batch as
   the Replay — enqueued together by the daemon), the next frame derives the strip layout and
   sends ONE corrective `C2D::Resize`. Accepted: one extra resize per hooked attach, no storm
   (and `epoch > 0` persists in `BlockList` for the life of the connection, so it never
   oscillates — including across death/restore, keeping geometry stable through Dead).
5. Inside `terminal_card`, after the resize logic: split the card rect —
   `grid_rect_area = avail minus bottom STRIP_H` for hooked terminals; run `term_view::show` in
   the top part (via `ui.allocate_ui_at_rect`/child Ui), then `composer::show` in the strip
   rect. Hookless terminals: today's single call, byte-for-byte.

The strip stays during Dead, busy, and UNSTABLE alt (inv. 5, amended by C2 below): a program
that blips through alt never triggers entry/exit resizes tied to app behavior.

C2 refinement (supersedes Bug C's render-only hide): after the alt screen has been held
continuously ≥ 400ms (`HIDE_AFTER`) the strip **collapses** — the band stops painting AND the
terminal reclaims the 36px: `layout_for` consults the SAME `strip_hidden` predicate the paint
does (single source; paint and PTY size cannot disagree), so the grid grows by the reserved
rows and the PTY resizes once through the ordinary debounced commit/heal machinery. The 400ms
hysteresis is the flap debounce: re-collapse always needs a fresh 400ms of stable alt, so alt
flapping (claude shelling out) can never storm resizes. ANY lane change (TUI exit, death,
sleep, reconnect) un-collapses the same frame: the strip returns and the rows go back with one
resize. Hover-peek while collapsed is a translucent OVERLAY floating over the grid's bottom
band (label + cluster under a TERM_BG wash) — look-only: no geometry, no interaction (the
band's pixels belong to the grid; clicks/wheel reach the app). Safety: the under-alt resize
drops block chrome RECOVERABLY (`pre_resize_ordinals` clears anchors/prompt_end/covers but no
longer stales the feed — the shell's next prompt re-primes block recording; pinned by
`alt_hide_show_resize_reprimes_block_feed_after_exit`), and mode flips alone still never
resize (`alt_screen_never_resizes_unchanged_layout`). Sleep while collapsed runs a
freeze-geometry pre-pass (`prepare_sleep_geometry`): un-collapse + resize back BEFORE the
sleep verb so the daemon's frozen frame matches the reserved-size asleep/wake presentation.

---

## 8. mod.rs wiring (exact touch points)

1. `mod composer;` + `use composer::{ComposerState, ComposerMode, RawReason};`
2. `App.composers: HashMap<Uuid, ComposerState>`; pruned in `apply_snapshot` alongside
   `self.blocks`; **NOT cleared** in `reconnect_if_needed` (drafts survive reconnect — D8), but
   each state gets `on_reset()` so latches re-arm from live hooks only.
3. `drain_ipc`:
   - `D2C::Blocks` arm (P2 version): after upsert, if `list.epoch > 0`, ensure a
     `ComposerState` entry exists.
   - `D2C::Output` arm: after `backend.advance_live(&bytes)` (P2), if a composer exists:
     `st.on_stream_events(backend.block_feed.as_ref().unwrap())` — counter diffs drive
     latch/dismiss for every terminal, selected or not.
   - `D2C::Reset` arm: `st.on_reset()`. `D2C::Exited`: `st.on_exited()`.
4. `terminal_card`:
   - compute `composer_focus` BEFORE building the grid `focused` flag:
     `let comp_active = self.composers.get(&id).is_some_and(|c| c.mode == ComposerMode::Compose);`
     then `focused = modal.is_none() && search.is_none() && !comp_active_and_focused` — track
     actual egui focus via the previous frame's `ComposerOutput.has_focus` stored in the state
     (one-frame lag is fine: egui focus itself is authoritative; the flag only stops the grid's
     `request_focus`).
   - run `st.tick(feed, now)` for the selected terminal; feed the returned wakeup to
     `request_repaint_after`.
   - after `term_view::show`: if `!out.write.is_empty()` → `st.on_raw_input()` before sending
     (raw typing dismissal, D7 — this catches keys, wheel-to-arrows in alt-screen, mouse
     reports, grid pastes: ALL raw bytes uniformly).
   - grid `resp.clicked()` while composer focused → `st.blur_to_grid()`.
   - call `composer::show(...)`; ship `ComposerOutput.write` via `C2D::Input` (same send site).
5. `select_terminal`: set `want_focus` on the target's composer if armed.
6. `Prefs`: nothing new in v1 (no per-user composer toggle yet — open question 7).

No changes to: bindings.rs, glyph_cache, theme, ipc.rs, daemon (except §5.1's inert verb),
protocol.rs, journal, serialize, bootstrap.

---

## 9. Degraded modes — the honest contract

| Situation | Strip | Composer | Keys |
|---|---|---|---|
| Hookless (claude tab, cmd, custom, epoch==0) | **absent** — zero change vs today | never | raw always |
| Hooked pwsh at an idle prompt, this attach saw the `pre` | shown | auto-arms after 50ms | composed |
| Hooked, command/TUI running (open block: claude, ssh, nested cmd/pwsh) | Busy: cmd + elapsed | blocked | raw |
| Hooked, alt-screen (vim) | "Keys go to the app" | blocked | raw |
| User typed raw at an armed prompt | Compose affordance | ManualOnly for the episode | raw until next prompt or click |
| Type-ahead queued into the next prompt | Compose affordance (cursor dirty) | ManualOnly (click = Ctrl+C clear) | raw |
| GUI restart onto idle shell (cold attach, current-epoch closed rec exists) | Compose affordance | ManualOnly (no prompt_end yet) | raw until click or next prompt |
| GUI restart onto restored-claude wrapper (hooked, only old-epoch recs) | "Typing goes to the terminal" | blocked (epoch heuristic) — arms at the first real prompt after claude exits | raw |
| Bootstrap write failed (hooked flag set, no hooks ever fire) | permanent "Typing goes to the terminal" | never arms (no `pre`) | raw — honest degraded |
| PSReadLine 2.0 / PS 5.1 multi-line submit | — | N sequential blocks (paste semantics) | — |
| PSReadLine Emacs mode | identical (Ctrl+C cancel verified binding) | full | — |
| PSReadLine Vi mode | works; manual clear chord is the documented residual | full | — |
| proto=1 daemon (P1, no P2 daemon bits) | works — composer needs only Blocks frames + local scanner | full | — |
| proto=0 daemon | no Blocks frames → epoch 0 → strip absent | never | raw always |
| Dead session | "Session ended" | blocked | grid keys go nowhere (today's behavior) |
| Mid-restore (Reset→Replay) | resets to NoPrompt, draft kept | re-arms on the new session's first live `pre` | raw during |
| Remote prompt inside `ssh` | Busy (ssh's block stays open) | blocked | raw — "hooks vanish" is detected as the never-closing block, no timer needed |

---

## 10. Probes (src/probe.rs — extend the P2 suite; all headless, no GUI attached)

### 10.1 `composer_submit` — clear chord + submission through a real PSReadLine

1. Hooked pwsh terminal; attach; await the first prompt (`await_output` for `PS `).
2. Send stray text WITHOUT enter: `Input "JUNKJUNK"`; brief settle (await echo of `JUNKJUNK`).
3. Send the manual-activation byte sequence exactly as the GUI would: win32-encoded Ctrl+C
   (reuse the `keys` probe's encoding), then `submission_bytes` equivalent for
   `"echo COMPOSED_OK"` — plain text (5.1 has no 2004) + `\r`, in ONE Input frame.
4. `await_blocks` for a closed rec with `cmd == "echo COMPOSED_OK"` and `exit == Some(0)`;
   assert NO rec whose cmd contains `JUNK` exists. This is the money assertion: the clear chord
   cancels stray input and the submission is recorded byte-exact — the whole §4 path against
   real PSReadLine, ordering included (chord and text in one write).
5. Empty-submit leg: send just `\r`; assert a fresh prompt renders (`await_output`) and NO new
   block rec appears (bootstrap skips blank lines) — proves prompt-refresh is block-silent.

### 10.2 `composer_multiline` — PS 5.1 paste semantics

1. Same terminal; send `"echo ML_A\recho ML_B\r"` as one Input frame (already `\r`-sanitized).
2. `await_blocks` until recs for BOTH `echo ML_A` and `echo ML_B` exist, both `exit Some(0)`,
   with `start_off(ML_A) < start_off(ML_B)` — asserts sequential accept-per-line ordering, the
   documented 5.1 behavior the composer inherits.

### 10.3 `composer_gate_replay` — the state machine against real session bytes

1. Hooked terminal; attach; capture ALL Output bytes into a buffer while driving:
   first prompt → `Input "ping -t 127.0.0.1\r"` → interrupt (win32 Ctrl+C) → prompt →
   `Input "echo GATE_END\r"` → prompt.
2. Feed the buffer (chunked at 7 bytes) through `BlockScanner` and replay events into a
   `ComposerState` + `gate()` (both are GUI-free pure logic), with a store mirror maintained
   from the received `D2C::Blocks` frames. Assert the verdict sequence:
   `Blocked(NoPrompt)` → (after first `Pre` + settle simulated) `AutoArm` → (after
   `Exec "ping…"`) `Blocked(Busy)` → (after the closing `Pre`) `AutoArm` → … — and critically:
   the `Exec` event's stream offset precedes the first byte of ping output (disarm-before-
   the-app-draws, the claude-safety property, asserted on real bytes).
3. `PromptEnd` leg: assert a `HookVerb::PromptEnd` event follows every `Pre` in the stream, and
   that the daemon logged no token warnings for them (inert verb).

Register all three in `CASES`; suite grows 22 → 25 (on top of P2's four).

---

## 11. Unit tests (cargo test)

composer.rs:
- `gate_truth_table`: every row of §2.5 including the `ManualOnly` split.
- `episode_rules`: raw input at prompt ⇒ `UserRaw` + no auto-arm until next `pre`; submit ⇒
  `PostSubmit` + `episode_used`; `pre` resets; keys sent while `at_prompt == false` (TUI case)
  do NOT set `episode_used`.
- `recall_walks_and_dedupes`: recs `[a, b, b, "", c]` → Up: c, b, a; Down restores saved draft;
  an edit mid-recall drops the recall state.
- `submission_bytes_matrix`: bracketed vs plain × trailing-newline trim × CRLF sanitize ×
  unicode (é, 漢, emoji) pass through untouched as UTF-8.

term_backend.rs (build streams with the blocks.rs `hook()` helper + a `b"\x1b]133;B\x07"`
marker):
- `prompt_end_captured_at_marker`: feed `pre-hook + "PS C:\\> "` + 133;B; assert
  `prompt_end == (cursor row, len("PS C:\\> "))` and `cursor_at_prompt_end()` true; feed typed
  echo `"dir"`; assert false; feed backspaces (`\x08 \x08`×3 shell-echo style); assert true
  again.
- `prompt_end_shifts_with_history_and_invalidates`: scroll k lines ⇒ line shifted −k; ED3 /
  saturation / `resize_to` ⇒ `None`.
- `pre_exec_counters_bump_even_inside_sync_block`: wrap hooks in `?2026h…l`; counters bump,
  `prompt_end` capture is skipped (deferred grid).

blocks.rs:
- `prompt_end_verb_parses_and_is_chunk_safe`: 133;B at chunk boundaries (1/7/64) yields
  identical events; `133;A` and foreign OSCs still yield nothing; daemon `on_block_event`
  ignores `PromptEnd` without a token warning.

---

## 12. Interactive checklist (screenshot-verified; never run a second GUI instance while the
user's is open; never inject input while the user is active)

1. Hooked pwsh tab: after the prompt renders and ~50ms passes, the composer arms — typing lands
   in the editor, the grid prompt row stays bare, the grid cursor renders hollow (unfocused).
2. Type `echo hi`, Enter: command echoes at the prompt, runs, block chrome appears (P2), and the
   composer re-arms at the next prompt automatically. Draft box is empty again.
3. Type `claude`, Enter, then IMMEDIATELY type text: the keys land in claude's UI raw — none in
   a composer, none lost, none doubled. On claude exit, the composer returns by itself.
4. Multi-line: Shift+Enter builds a 3-line draft — the editor grows UPWARD over the grid, the
   grid does not resize (no conhost reflow), Run submits; on PS 5.1 each line runs sequentially.
5. ArrowUp cycles previous commands (consecutive dupes skipped); typing mid-recall forks into a
   new draft; ArrowDown past newest restores what you had.
6. Click into the grid while composing: keys now go raw (typed chars echo at the shell prompt);
   the strip shows `❯ Compose`; clicking it cancels the stray text (one extra prompt renders)
   and re-opens the editor with the draft intact.
7. Type-ahead: run `ping -n 3 127.0.0.1`, type `dir` while it runs — at the next prompt the
   composer does NOT auto-arm (stray `dir` visible at the prompt); raw Enter runs it.
8. `vim` (alt-screen) from the hooked prompt: strip shows "Keys go to the app", no composer, no
   grid resize on entry/exit. `:q` → prompt → composer returns.
9. Claude tab (hookless): NO strip at all, terminal geometry and behavior byte-identical to
   today (compare row count before/after the build).
10. Scrollback drag-select + copy over the grid WHILE the composer is focused: selection works,
    composer keeps focus and draft.
11. Search overlay open while armed: typing goes to search; closing search returns typing to the
    composer; Esc chain never eats a keystroke.
12. Kill the shell (header ⏻): strip shows "Session ended", draft survives; Restore → new
    prompt → composer arms (after the first live prompt).
13. GUI restart onto 3 idle hooked shells: each shows `❯ Compose`; one click arms (no stray
    bytes fired at any shell that wasn't clicked).
14. Window resize while composing: grid reflows per the existing debounce, composer keeps focus
    and draft; auto-arm after resize still requires the next prompt or a click (no drift-arm).

---

## 13. Open questions — with the defaults the implementer should take

1. **Auto-arm focus steal when the window itself just gained focus** (alt-tab back): default —
   arm but do NOT set `want_focus` unless the grid held focus before; first click/keystroke
   decides. Rationale: alt-tab users may be aiming at scrollback.
2. **Should Busy-strip show a Cancel (Ctrl+C) button next to the running command?** Default NO
   for v1 — it is a one-line addition later, but interrupt semantics deserve their own thought
   (P2's Kill already exists in the header).
3. **Tab completion in the composer**: default none in v1 (Tab inserts spaces via lock_focus).
   PSReadLine completion is unreachable without PTY round-trips; a native completer is P4+.
4. **Incomplete multi-line submissions on ≥2.2** (unclosed brace): default submit as-is —
   PSReadLine's own continuation takes over in raw mode, strip shows ManualOnly (cursor past
   prompt end); no client-side PowerShell parser in v1.
5. **Vi edit mode users + manual clear**: default accept (Ctrl+C in vi-insert cancels; command
   mode is only reachable if the user was already driving vi-mode raw). Escape-hatch: make the
   chord a future pref.
6. **Empty-draft Enter sends bare `\r`**: default YES (prompt refresh is a habitual terminal
   gesture; probe 10.1 asserts it is block-silent).
7. **A pref to disable the composer entirely**: default not in v1; the per-episode `⌨` toggle
   covers the session-level need. Add `Prefs.composer: bool` only if the user asks.
8. **Draft persistence to disk across GUI restarts**: default NO (drafts are seconds-old
   ephemera; gui.json writes are fsync'd and shouldn't churn per keystroke).
9. **Strip height 36px vs 32px**: default 36 (matches header button rhythm; the editor line at
   13px mono + padding needs 30+).

---

## 14. Explicit DO-NOTs (each traces to an invariant or past incident)

- Do NOT send ANY byte to the PTY from composer code except: the submission, the bare `\r`
  refresh, and the click-gated clear chord (inv. 1; PSReadLine-blind contract).
- Do NOT bracket-paste unconditionally — check `TermMode::BRACKETED_PASTE` (PSReadLine 2.0
  would receive literal `ESC[200~` as input garbage).
- Do NOT use Escape (or Ctrl+Home) as the clear chord (edit-mode-dependent: meta prefix in
  Emacs, command mode in Vi — the P2 §11.2 concern, now resolved as Ctrl+C).
- Do NOT let the strip appear/disappear with prompts, alt-screen, or death — grid geometry must
  never depend on transient shell state (resize-storm incident class).
- Do NOT apply one shared grid layout to all terminals once the strip exists — per-terminal
  `layout_for` (§7), or tab switches become resize storms.
- Do NOT auto-arm without `cursor_clean` + `!episode_used` + settle (stray-text corruption;
  focus steal mid-word).
- Do NOT yank egui focus from a focused composer on external events; only user actions move
  focus (inv. 4; egui focus-cycling keystroke-loss bug class).
- Do NOT clear drafts on gate failures, resets, reconnects, or tab switches (D8).
- Do NOT feed the composer's prompt detection from `Replay` bytes (`advance`, not
  `advance_live` — reconstructions contain no hooks by design and raw-tail fallbacks contain
  STALE ones; P2 §3.3).
- Do NOT show the composer (or strip) for `epoch == 0` terminals (the load-bearing hookless
  gate — claude tabs must be untouched).
- Do NOT add protocol variants or bump `proto` — nothing on the wire changes.

---

## 15. Suggested implementation order (compile-green at each step)

1. blocks.rs `HookVerb::PromptEnd` + parse + daemon inert-arm + chunk tests (§5.1, §11).
   Pure addition; P2 probes stay green.
2. term_backend.rs `BlockFeed` counters + `prompt_end` capture/shift/invalidate +
   `cursor_at_prompt_end` + unit tests (§5.2, §11).
3. composer.rs types + `gate()` + `ComposerState` transitions + recall + `submission_bytes` +
   unit tests (§6, §2.5) — no UI yet, fully testable.
4. mod.rs wiring: `composers` map, drain_ipc arms, `on_raw_input` hook at the Input send site
   (§8). Still no visible change (no strip yet).
5. Layout split: `layout_for` + per-terminal resize loop + strip rect reservation (§7). Verify
   with a hookless tab that nothing moved.
6. composer::show strip + editor + focus routing + grid `focused` flag extension (§6.3, §3).
7. Probes 10.1–10.3; then the interactive checklist with screenshots.
