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
//! i.e. `stty -echo` has run — and phase 2 then writes the two base64 payload
//! lines invisibly. One echoed line is the whole visible artifact, and the
//! payload scrubs even that out of bash's history.
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
    /// pwsh/cmd: the hook body is bash/zsh. Nested shells there are not a
    /// thing Pulse witnesses through a POSIX exec hook.
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
/// hook verdict, `hook_fed` that the family is one whose execs we witness
/// (WSL/ssh), `opt_in` the per-terminal switch, `claude_kind` the pinned
/// terminal class, `depth` the nested tokens already registered.
pub(crate) fn arm_verdict(
    hooked: bool,
    hook_fed: bool,
    opt_in: bool,
    claude_kind: bool,
    depth: usize,
) -> ArmVerdict {
    if claude_kind {
        return ArmVerdict::PinnedKind;
    }
    if !hook_fed {
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
    /// Never settled: the world's state is unknown.
    AbortTimeout,
    /// Settled, quiet, at an ordinary prompt: type the reader line.
    Send,
}

/// The phase-1 decision (pure). `quiet_for` = time since the journal last
/// grew, `since_armed` = time since the opener was witnessed. The credential
/// and alt-screen checks run at the SETTLED edge, where the tail line is the
/// waiting prompt.
pub(crate) fn open_action(
    since_armed: Duration,
    quiet_for: Duration,
    tail_is_credential: bool,
    alt_screen: bool,
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
    OpenAction::Send
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
        let (hooked, opt_in, claude_kind, hook_fed) = {
            let state = self.state.lock();
            let Some(t) = state.terminal(id) else { return };
            let fam = crate::state::shell_family(&t.kind, &t.program, &t.args);
            (
                t.hooked,
                t.shell_cfg.clone().unwrap_or_default().auto_reestablish,
                matches!(t.kind, TermKind::Claude { .. }),
                matches!(
                    fam,
                    crate::state::ShellFamily::WslShell { .. }
                        | crate::state::ShellFamily::Ssh { .. }
                ),
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
            match arm_verdict(hooked, hook_fed, opt_in, claude_kind, already) {
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

    /// Write the two base64 payload lines (echo already suppressed by the
    /// reader line) and move to `AwaitInit`.
    fn send_nesthook_payload(&self, id: Uuid) {
        let lines = {
            let map = self.nesthooks.lock();
            let Some(e) = map.get(&id) else { return };
            if !matches!(e.phase, Phase::AwaitEcho { .. }) {
                return;
            }
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
                        self.rebase_nesthook(id, len, now);
                        continue;
                    }
                    let quiet_for = now.duration_since(last_change);
                    let settled = quiet_for >= OPEN_QUIET;
                    let alt = settled && self.terminal_is_alt(id);
                    let cred = settled
                        && !alt
                        && reestablish::credential_prompt_line(
                            &self.last_screen_line(id).unwrap_or_default(),
                        );
                    match open_action(now.duration_since(armed), quiet_for, cred, alt) {
                        OpenAction::Wait => {}
                        OpenAction::AbortCredential => self.cancel_nesthook(
                            id,
                            "a credential prompt is pending (hooks are never typed into one)",
                        ),
                        OpenAction::AbortAlt => self.cancel_nesthook(
                            id,
                            "a full-screen program owns the terminal (no prompt to inject at)",
                        ),
                        OpenAction::AbortTimeout => {
                            self.cancel_nesthook(id, "the nested shell never settled")
                        }
                        OpenAction::Send => self.send_nesthook_reader(id, now),
                    }
                }
                Phase::AwaitEcho { sent, last_len, last_change } => {
                    let len = self.journal_len(id).unwrap_or(last_len);
                    if len != last_len {
                        self.rebase_nesthook(id, len, now);
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

    /// Phase 1 gating: quiescence gates every transition; the credential and
    /// alt-screen checks fire only at the settled edge; the timeout wins.
    #[test]
    fn open_action_matrix() {
        let ms = Duration::from_millis;
        assert_eq!(open_action(ms(100), ms(100), false, false), OpenAction::Wait);
        assert_eq!(open_action(ms(600), ms(699), false, false), OpenAction::Wait);
        assert_eq!(open_action(ms(1000), ms(700), false, false), OpenAction::Send);
        // Credential abort — the reestablish predicate, reused verbatim.
        assert_eq!(
            open_action(ms(1000), ms(700), true, false),
            OpenAction::AbortCredential
        );
        assert!(reestablish::credential_prompt_line("[sudo] password for rig:"));
        // Alt-screen outranks the credential line (no prompt exists at all).
        assert_eq!(open_action(ms(1000), ms(700), true, true), OpenAction::AbortAlt);
        assert_eq!(open_action(ms(1000), ms(700), false, true), OpenAction::AbortAlt);
        // Timeout outranks everything, settled or not.
        assert_eq!(
            open_action(OPEN_TIMEOUT, ms(100), false, false),
            OpenAction::AbortTimeout
        );
        assert_eq!(
            open_action(OPEN_TIMEOUT + ms(1), ms(900), true, true),
            OpenAction::AbortTimeout
        );
    }

    /// User input during phase 1: a SUBMITTED line keeps the injection
    /// waiting (its output re-bases the settle clock and the next prompt is
    /// still safe); PARTIAL typing abandons it (our line would be appended
    /// to the user's half-typed command).
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
    }
}
