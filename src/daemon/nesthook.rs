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
//! (`bootstrap::NESTED_READER`) and waits for its echo to come back and
//! settle — which proves the remote shell has accepted and is EXECUTING it,
//! i.e. `stty -echo` has run — and phase 2 then writes the base64 payload
//! lines invisibly.
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

/// Output must be quiet this long after the witnessed opener before the
/// reader line is typed — the same settle rule `reestablish` types under
/// (long enough for `sudo su` to paint its prompt, short enough to feel
/// instant).
const OPEN_QUIET: Duration = Duration::from_millis(700);

/// An opener that never settles within this window abandons the injection —
/// never type into a world in an unknown state.
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// After the reader line is typed, its echo must have come back and been
/// quiet this long before the payload lines are written. The echo returning
/// proves the round trip completed and the shell accepted the line, so
/// `stty -echo` has run and the payload will not be echoed.
const ECHO_QUIET: Duration = Duration::from_millis(250);

/// ...and if the echo never settles, the payload is written ANYWAY at this
/// deadline. The reader line's two `read` builtins block the shell until
/// they get their two lines: abandoning after phase 1 would leave the user's
/// shell wedged, so this deadline must always fire. (Worst case the base64
/// is visible — one ugly screenful, never a broken shell.)
const ECHO_DEADLINE: Duration = Duration::from_secs(5);

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
    /// The three lines to type (reader + two base64 payload lines).
    inj: bootstrap::NestedInjection,
    phase: Phase,
    /// The opener that started this episode — logged, never re-typed.
    opener: String,
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Armed at the witnessed opener; watching for output quiescence before
    /// the reader line is typed.
    AwaitQuiet { armed: Instant, last_len: u64, last_change: Instant },
    /// The reader line was typed; watching its echo settle before the
    /// payload lines are written (see `ECHO_QUIET`/`ECHO_DEADLINE`).
    AwaitEcho { sent: Instant, last_len: u64, last_change: Instant },
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
pub(crate) fn open_action(
    since_armed: Duration,
    quiet_for: Duration,
    tail_is_credential: bool,
    tail_is_hostkey: bool,
    alt_screen: bool,
    at_prompt: bool,
) -> OpenAction {
    if since_armed >= OPEN_TIMEOUT {
        return OpenAction::AbortTimeout;
    }
    if quiet_for < OPEN_QUIET {
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
    open_action(since_armed, Duration::ZERO, false, false, false, false)
}

/// The phase-2 decision (pure): write the payload once the reader's echo has
/// settled, and ALWAYS by the deadline — a shell parked in `read` must never
/// be left waiting.
pub(crate) fn echo_ready(since_sent: Duration, quiet_for: Duration) -> bool {
    since_sent >= ECHO_DEADLINE || quiet_for >= ECHO_QUIET
}

/// What user input during phase 1 means (pure). The user owns their shell —
/// the question is only whether their bytes left a LINE BUFFER behind that
/// ours would concatenate onto.
///
/// - A SUBMITTED line (composer `SubmitCommand`, or raw bytes ending in
///   Enter) leaves no buffer: the shell is running it, its output re-bases
///   the quiescence clock, and the next settled prompt is still a safe place
///   to inject. Keep waiting — this is the common field path (`sudo su`
///   immediately followed by a command), and abandoning it would lose the
///   integration for the whole episode.
/// - PARTIAL typing (raw keystrokes with no Enter) leaves a half-typed line
///   in readline's buffer, and our line would be appended to THEIR command.
///   Abandon, always: a mangled command line is never an acceptable price.
pub(crate) fn input_keeps_waiting(submitted: bool) -> bool {
    submitted
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
        self.nesthooks.lock().insert(
            id,
            NestHook {
                depth,
                inj,
                phase: Phase::AwaitQuiet { armed: now, last_len: len, last_change: now },
                opener: opener.trim().to_string(),
            },
        );
        log::info!(
            "terminal {id}: nested hook injection armed (depth {depth}, opener {}) — waiting for the nested prompt to settle",
            opener.trim()
        );
    }

    /// Drop any injection state. Logged only when an entry existed.
    pub(super) fn cancel_nesthook(&self, id: Uuid, why: &str) {
        let entry = self.nesthooks.lock().remove(&id);
        if let Some(e) = entry {
            log::info!(
                "terminal {id}: nested hook injection stopped at depth {} — {why}",
                e.depth
            );
        }
    }

    /// User input arrived (the same `cancel_reestablish("user input")`
    /// sites). Phase 1 hands the shell back untouched; phase 2 must NOT
    /// abandon — the shell is parked in our `read` builtins — so it writes
    /// the payload immediately and lets the injection finish.
    pub(super) fn nesthook_on_input(&self, id: Uuid, submitted: bool) {
        let flush = {
            let mut map = self.nesthooks.lock();
            match map.get(&id).map(|e| e.phase) {
                Some(Phase::AwaitQuiet { .. }) if input_keeps_waiting(submitted) => {
                    // A submitted line: its output re-bases the settle clock
                    // on the next pump tick anyway. Nothing to do.
                    false
                }
                Some(Phase::AwaitQuiet { .. }) => {
                    map.remove(&id);
                    log::info!(
                        "terminal {id}: nested hook injection stopped — the user is typing at the nested prompt"
                    );
                    false
                }
                Some(Phase::AwaitEcho { .. }) => true,
                _ => false,
            }
        };
        if flush {
            self.send_nesthook_payload(id);
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
    }

    /// What `reestablish` needs before typing the inner-CLI resume.
    pub(super) fn nesthook_state(&self, id: Uuid) -> NestState {
        match self.nesthooks.lock().get(&id).map(|e| e.phase) {
            None => NestState::Absent,
            Some(Phase::Hooked) => NestState::Hooked,
            Some(_) => NestState::Pending,
        }
    }

    /// Write the three base64 payload lines (echo already suppressed by the
    /// reader line) and move to `AwaitInit`.
    ///
    /// The ERASE blob is composed HERE, not at arm time: it targets the row
    /// the echoed reader line starts on, and that row can only be read off the
    /// mirror once the echo has actually landed — after any wrapping and any
    /// scrolling the terminal itself decided on.
    fn send_nesthook_payload(&self, id: Uuid) {
        let reader = {
            let map = self.nesthooks.lock();
            let Some(e) = map.get(&id) else { return };
            if !matches!(e.phase, Phase::AwaitEcho { .. }) {
                return;
            }
            e.inj.reader.clone()
        };
        let erase_row = self.nesthook_erase_row(id, &reader);
        if erase_row.is_none() {
            log::info!(
                "terminal {id}: the injected line stays visible — the mirror does not show it \
                 exactly where it was typed (wrapped off-screen, or the shell redrew)"
            );
        }
        let lines = {
            let mut map = self.nesthooks.lock();
            let Some(e) = map.get_mut(&id) else { return };
            if !matches!(e.phase, Phase::AwaitEcho { .. }) {
                return;
            }
            e.inj.erase_b64 = bootstrap::nested_erase_payload(erase_row);
            e.inj.payload_lines().map(str::to_string)
        };
        for l in &lines {
            if !self.type_reestablish_line(id, l) {
                self.cancel_nesthook(id, "session gone before the payload could be written");
                return;
            }
        }
        if let Some(e) = self.nesthooks.lock().get_mut(&id) {
            e.phase = Phase::AwaitInit { sent: Instant::now() };
        }
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
                Phase::AwaitQuiet { armed, last_len, last_change } => {
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
                        OpenAction::Send => self.send_nesthook_reader(id, now),
                    }
                }
                Phase::AwaitEcho { sent, last_len, last_change } => {
                    let len = self.journal_len(id).unwrap_or(last_len);
                    if len != last_len {
                        // ABSOLUTE DEADLINE FIRST — same defect, same shape:
                        // `echo_ready`'s ECHO_DEADLINE arm is documented to
                        // "fire even while output is still streaming", and
                        // the `continue` was the reason it never did. A shell
                        // parked in our `read` must never be left waiting.
                        self.rebase_nesthook(id, len, now);
                        if echo_ready(now.duration_since(sent), Duration::ZERO) {
                            self.send_nesthook_payload(id);
                        }
                        continue;
                    }
                    if echo_ready(now.duration_since(sent), now.duration_since(last_change)) {
                        self.send_nesthook_payload(id);
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
    fn journal_len(&self, id: Uuid) -> Option<u64> {
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
    fn cursor_row_is_prompt(&self, id: Uuid) -> bool {
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
    fn nesthook_erase_row(&self, id: Uuid, reader: &str) -> Option<usize> {
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
    fn terminal_is_alt(&self, id: Uuid) -> bool {
        let term = self.sessions.lock().get(&id).map(|s| s.term.clone());
        term.is_some_and(|t| {
            t.lock()
                .mode()
                .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
        })
    }

    /// Output grew: re-base the quiescence clock for whichever phase is
    /// watching it.
    fn rebase_nesthook(&self, id: Uuid, len: u64, now: Instant) {
        let mut map = self.nesthooks.lock();
        let Some(e) = map.get_mut(&id) else { return };
        match &mut e.phase {
            Phase::AwaitQuiet { last_len, last_change, .. }
            | Phase::AwaitEcho { last_len, last_change, .. } => {
                *last_len = len;
                *last_change = now;
            }
            _ => {}
        }
    }

    /// Phase 1 → 2: type the reader line.
    fn send_nesthook_reader(&self, id: Uuid, now: Instant) {
        let reader = {
            let map = self.nesthooks.lock();
            let Some(e) = map.get(&id) else { return };
            e.inj.reader.clone()
        };
        if !self.type_reestablish_line(id, &reader) {
            self.cancel_nesthook(id, "session gone before the hooks could be typed");
            return;
        }
        let len = self.journal_len(id).unwrap_or(0);
        if let Some(e) = self.nesthooks.lock().get_mut(&id) {
            log::info!(
                "terminal {id}: injecting Pulse hooks into the nested shell (depth {})",
                e.depth
            );
            e.phase = Phase::AwaitEcho { sent: now, last_len: len, last_change: now };
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
        assert_eq!(open_action(ms(100), ms(100), false, false, false, true), OpenAction::Wait);
        assert_eq!(open_action(ms(600), ms(699), false, false, false, true), OpenAction::Wait);
        assert_eq!(open_action(ms(1000), ms(700), false, false, false, true), OpenAction::Send);
        // Credential abort — the reestablish predicate, reused verbatim.
        assert_eq!(
            open_action(ms(1000), ms(700), true, false, false, true),
            OpenAction::AbortCredential
        );
        assert!(reestablish::credential_prompt_line("[sudo] password for rig:"));
        // typed-ssh-nested: ssh's host-key question aborts too, and the
        // composer's auth classifier is the ONE definition of that row.
        assert_eq!(
            open_action(ms(1000), ms(700), false, true, false, true),
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
            open_action(ms(1000), ms(700), true, true, false, true),
            OpenAction::AbortCredential
        );
        // Alt-screen outranks both (no prompt exists at all).
        assert_eq!(open_action(ms(1000), ms(700), true, false, true, true), OpenAction::AbortAlt);
        assert_eq!(open_action(ms(1000), ms(700), false, true, true, true), OpenAction::AbortAlt);
        assert_eq!(open_action(ms(1000), ms(700), false, false, true, true), OpenAction::AbortAlt);
        // Settled but NOT at a shell prompt (a wizard/menu/banner): keep
        // watching — never type a line into something that wants keypresses.
        assert_eq!(
            open_action(ms(1000), ms(700), false, false, false, false),
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
            open_action(OPEN_TIMEOUT, ms(100), false, false, false, true),
            OpenAction::AbortTimeout
        );
        assert_eq!(
            open_action(OPEN_TIMEOUT + ms(1), ms(900), true, true, true, false),
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

        // ...and the payload the gate feeds: a row becomes an absolute CUP +
        // erase-to-end-of-display, no row becomes an EMPTY blob (eval "").
        assert!(!bootstrap::nested_erase_payload(Some(7)).is_empty());
        assert!(bootstrap::nested_erase_payload(None).is_empty());
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
        assert!(!echo_ready(ECHO_DEADLINE - ms(1), Duration::ZERO));
        assert!(echo_ready(ECHO_DEADLINE, Duration::ZERO));
    }

    #[test]
    fn input_gating() {
        assert!(input_keeps_waiting(true), "a submitted line must not abandon");
        assert!(
            !input_keeps_waiting(false),
            "half-typed input must abandon — never concatenate onto the user's line"
        );
    }

    /// Phase 2: the payload goes out on a settled echo, and UNCONDITIONALLY
    /// at the deadline — a shell parked in our `read` is never left waiting.
    #[test]
    fn echo_ready_matrix() {
        let ms = Duration::from_millis;
        assert!(!echo_ready(ms(100), ms(0)));
        assert!(!echo_ready(ms(100), ECHO_QUIET - ms(1)));
        assert!(echo_ready(ms(300), ECHO_QUIET));
        assert!(
            echo_ready(ECHO_DEADLINE, Duration::ZERO),
            "the deadline must fire even while output is still streaming"
        );
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
