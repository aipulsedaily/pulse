//! F2 (ssh-reestablish): nested-chain auto re-establish — the "auto sudo su"
//! half of the user's ask. A child module of `daemon` like `reconnect`
//! (private-field access to `Core` is deliberate): after a relaunch /
//! reconnect / wake of a terminal that carried a nested-shell breadcrumb
//! (`sudo su`…), once the OUTER shell's hooked prompt has settled (the first
//! token-checked `pre` — the same witness reconnect resolution uses), the
//! recorded chain commands are typed back one at a time, each step gated on
//! output quiescence, and the whole remainder is ABORTED the instant the
//! settled tail line looks like a credential prompt — credentials are never
//! typed, the prompt is left sitting visibly for the user (I2 stays intact:
//! this lane types ONLY the user's own recorded chain commands, verbatim,
//! never the inner CLI and never secrets; the claude resume hint keeps
//! riding the honest preface).
//!
//! Doctrine fit: the recorded chain is a LOWER BOUND of what the user built
//! (spec §2), so re-typing it is exactly "redo what I did" — and every exit
//! is honest: abort reasons are logged, the credential prompt itself is
//! visible in the terminal, and the preface already named the chain.

use super::*;

/// Output must be quiet this long after a step before the next one is typed
/// (or the chain is declared done). Long enough for `sudo su` to print the
/// nested shell's prompt; short enough to feel instant.
const STEP_QUIET: Duration = Duration::from_millis(700);
/// A step that produces output (or silence) but never settles within this
/// window aborts the chain — never type into a world in an unknown state.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// One armed/running chain re-establish.
#[derive(Debug, Clone)]
pub(super) struct Reestablish {
    /// The recorded chain commands, opener first (`NestedChain.cmds`,
    /// stashed at launch; the breadcrumb itself now SURVIVES the armed
    /// spawn's trigger pre — see `reestablish_on_pre` — and retires on the
    /// first pre this engine does not consume).
    steps: Vec<String>,
    /// Next step to type (steps[idx]).
    idx: usize,
    phase: Phase,
    /// Nested-cli-resume: the composed FINAL step (`cd '<cli_cwd>' &&
    /// <adapter> --resume <sid>`, `tracker::nested_resume_step`) — typed
    /// ONLY at the Done edge of the LAST chain step, i.e. strictly inside
    /// the fully re-established nested shell (spec-I1 resolved by that
    /// ordering). None = hint-only sequence (incomplete breadcrumb).
    resume: Option<String>,
    /// The manual-resume fallback line pushed to the preface when the
    /// sequence stops before `resume` ran (`nested_resume_abort_hint`).
    hint: Option<String>,
    /// The `Session::gen` this sequence was armed for — GENERATION BINDING.
    /// A flapping link relaunches the terminal under a new generation while
    /// the old engine's timers are still armed; without this an entry from
    /// the previous spawn could type its next step (or its resume) into the
    /// NEW session, stacking chain entries and restarting a CLI while the
    /// previous remote process may still be alive. Every typing path checks
    /// it and abandons the stale generation instead.
    spawn_gen: u64,
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Armed at spawn; waiting for the outer shell's first token-checked
    /// `pre` (hooked prompt settled) before typing step 0.
    AwaitPrompt,
    /// The prompt witness landed; the step types on the next pump tick
    /// after `at`. The `pre` hook fires from PROMPT_COMMAND BEFORE PS1
    /// paints — typing instantly makes the echo land on a bare line above
    /// the prompt (field-observed); one short beat lets the prompt render
    /// so the typed command reads exactly like the user typing it.
    PendingSend { step: usize, at: Instant },
    /// A step was typed; watching the journal for quiescence.
    Watch {
        sent: Instant,
        last_len: u64,
        last_change: Instant,
    },
    /// nested-shell-hooks: every chain step settled, and a hook injection
    /// into the freshly re-established nested shell is still in flight — the
    /// resume is PARKED here until that shell's own hooked prompt arrives.
    ///
    /// Ordering is the whole point: `sudo su` → hooks injected → hooked
    /// nested prompt → THEN the resume. Typing the resume at the raw nested
    /// prompt (v0.1.13's behavior) starts a claude no hook can witness, so
    /// the very identity the next reconnect needs is never recorded — the
    /// field bug this closes. `since` bounds the wait: an unhookable shell
    /// (dash/fish, no base64) still gets its resume, just unattributed.
    AwaitNestedPrompt { since: Instant },
    /// typed-ssh-nested: a chain step settled, and a hook injection into the
    /// shell THAT STEP just opened is still in flight — the NEXT step is held
    /// here until that shell reports its own hooked prompt.
    ///
    /// Same ordering rule as `AwaitNestedPrompt`, one link earlier. It only
    /// became load-bearing with typed remote shells, because they are the
    /// first common MULTI-step chain (`ssh <host>` then `sudo su`; v0.1.14's
    /// field chains were all one step). Typing step 2 into the shell step 1
    /// opened, while Pulse is mid-injection into it, lands the reader line in
    /// the WRONG shell: the injection then hooks the innermost world but
    /// labels it depth 1, and the shell in between is left unhooked.
    /// `since` bounds the hold (`NESTED_HOOK_WAIT`) so an unhookable step
    /// still gets the rest of its chain, just unhooked at that link.
    AwaitStepHooked { since: Instant, next: usize },
}

/// How long after the prompt witness before the step is typed (one pump
/// tick's grace for the prompt paint).
const SEND_GRACE: Duration = Duration::from_millis(250);

/// nested-shell-hooks: how long the parked resume waits for the injected
/// nested shell's hooked prompt before typing anyway. Longer than the
/// injection's own init timeout, so the honest "it stays unhooked" log lands
/// first and this is only ever the belt.
const NESTED_HOOK_WAIT: Duration = Duration::from_secs(15);

/// What the watcher should do with a settled/unsettled step — pure, so the
/// step gating and password-abort are table-testable without a PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchAction {
    /// Not settled yet — keep watching.
    Wait,
    /// Settled and the tail line demands credentials — abort the remainder.
    AbortCredential,
    /// Ran STEP_TIMEOUT without settling — abort (unknown world state).
    AbortTimeout,
    /// Settled cleanly and more steps remain — type the next one.
    Next,
    /// Settled cleanly, chain complete.
    Done,
}

/// The step-gating decision (pure): `quiet_for` = time since the journal
/// last grew, `since_sent` = time since the step was typed. A step is
/// "settled" only after STEP_QUIET of silence; the credential check runs
/// exactly at that edge (the prompt is the LAST line once output stops).
pub(crate) fn watch_action(
    since_sent: Duration,
    quiet_for: Duration,
    tail_is_credential: bool,
    has_more_steps: bool,
) -> WatchAction {
    if since_sent >= STEP_TIMEOUT {
        return WatchAction::AbortTimeout;
    }
    if quiet_for < STEP_QUIET {
        return WatchAction::Wait;
    }
    if tail_is_credential {
        return WatchAction::AbortCredential;
    }
    if has_more_steps {
        WatchAction::Next
    } else {
        WatchAction::Done
    }
}

/// Does this settled screen line demand a secret? Case-insensitive
/// substring table over the classic prompt vocabulary (`[sudo] password for
/// x:`, `Password:`, ssh passphrases, verification/one-time codes). Checked
/// ONLY against the last non-blank screen line once output has settled — a
/// program merely *mentioning* passwords mid-stream scrolls past and never
/// ends as the waiting tail line.
pub(crate) fn credential_prompt_line(line: &str) -> bool {
    let l = line.trim().to_ascii_lowercase();
    if l.is_empty() {
        return false;
    }
    const NEEDLES: &[&str] = &[
        "password",
        "passphrase",
        "passcode",
        "verification code",
        "one-time",
        "otp",
        "authentication code",
        "pin:",
        "pin for",
        "security code",
    ];
    NEEDLES.iter().any(|n| l.contains(n))
}

/// The launch-time arm gate (pure): a breadcrumb exists, this spawn is
/// HOOKED (without hooks there is no prompt-settled witness — the honest
/// preface alone carries it, exactly as before F2), and the per-terminal
/// opt-out (default ON) says yes. One switch (`ShellCfg.auto_reestablish`)
/// gates the WHOLE sequence — chain re-type and, when the breadcrumb is
/// complete, the final inner-CLI resume step.
pub(crate) fn reestablish_should_arm(chain_present: bool, hooked: bool, opt_in: bool) -> bool {
    chain_present && hooked && opt_in
}

/// Pure consumption rule for a token-checked outer-prompt `pre`
/// (nested-cli-resume, hypothesis-c fix — unit-tested): the armed/pending
/// phases CONSUME it (it is the trigger / a re-base of the send grace), and
/// the caller must then SKIP the breadcrumb clear — the auto-typed chain is
/// about to recreate exactly the world the breadcrumb records; clearing on
/// this pre is what destroyed the resume identity (sid + cli_cwd) every
/// reconnect cycle. A Watch-phase pre is NOT consumed: the outer prompt
/// returning mid-chain means the nested step collapsed — the chain aborts
/// and the ordinary clear runs.
fn pre_consumes(phase: &Phase) -> bool {
    matches!(phase, Phase::AwaitPrompt | Phase::PendingSend { .. })
}

/// nested-shell-hooks — the sequencing gate (pure): may the inner-CLI
/// resume be typed NOW, given the state of the hook injection into the
/// nested shell it will run in? `Absent` (nothing to inject — hookless
/// family, opted out, credential abort) and `Hooked` (the shell announced
/// its hooks) both go; `Pending` parks, because a resume typed into a shell
/// that is one tick away from being hooked would run UNWITNESSED and leave
/// the breadcrumb incomplete for the next reconnect — the exact loop this
/// feature closes.
///
/// typed-ssh-nested: the SAME gate now governs one more edge — the NEXT
/// CHAIN STEP (`Phase::AwaitStepHooked`). The rule is identical ("do not type
/// into a shell that is one tick from being hooked"); only the line being
/// typed differs, so the two edges share this predicate rather than growing a
/// second, driftable copy.
pub(crate) fn resume_may_send(nest: NestState) -> bool {
    !matches!(nest, NestState::Pending)
}

/// What a step-growth tick means (pure). Re-basing the quiescence clock on
/// journal growth used to `continue` PAST `watch_action`, so STEP_TIMEOUT —
/// the only thing bounding a step that never settles — was never evaluated
/// while output kept arriving. The absolute deadline is asked FIRST now, with
/// `quiet_for = 0`, which can only answer Wait or AbortTimeout.
pub(crate) fn growth_watch(since_sent: Duration, has_more_steps: bool) -> WatchAction {
    watch_action(since_sent, Duration::ZERO, false, has_more_steps)
}

/// What to do with a line that is HELD for a previous hop's hook injection
/// (pure). One rule, shared by both hold phases:
///
///   - the injection resolved (`Hooked`, or `Absent` = nothing to inject /
///     honestly gave up) ⇒ SEND: the hop is settled either way;
///   - still `Pending` inside the wait ⇒ WAIT;
///   - still `Pending` AT the deadline ⇒ ABORT.
///
/// That last arm is the containment. It used to type on ("the previous step
/// never reported hooks; typing on" / "resuming unattributed") — i.e. it
/// typed the next chain step, or a session-restoring resume, into a shell
/// whose identity was never positively confirmed. A chain that cannot prove
/// where it is stops and leaves the user the manual line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HoldAction {
    Wait,
    Send,
    Abort,
}

pub(crate) fn hold_action(expired: bool, nest: NestState) -> HoldAction {
    if resume_may_send(nest) {
        HoldAction::Send
    } else if expired {
        HoldAction::Abort
    } else {
        HoldAction::Wait
    }
}

/// Pure resume-step gate (nested-cli-resume): the ONLY watch action that
/// may type the final resume step is `Done` — a partial chain (`Next`),
/// any abort, or an unsettled step must never reach it. The Done arm in
/// `pump_reestablish` additionally asserts the last chain step settled.
pub(crate) fn resume_may_type(action: WatchAction) -> bool {
    matches!(action, WatchAction::Done)
}

impl Core {
    /// Arm at spawn success (launch() calls this after the session is in the
    /// map). Replaces any stale entry — the newest spawn owns the chain.
    /// `resume` is the composed final inner-CLI resume step (+ its manual
    /// fallback hint) when the breadcrumb is complete — see
    /// `tracker::nested_resume_step`.
    pub(super) fn arm_reestablish(
        &self,
        id: Uuid,
        steps: Vec<String>,
        resume: Option<(String, String)>,
    ) {
        if steps.is_empty() {
            return;
        }
        let (resume, hint) = match resume {
            Some((step, hint)) => (Some(step), Some(hint)),
            None => (None, None),
        };
        log::info!(
            "terminal {id}: nested chain re-establish armed ({} step(s): {}{}) — waiting for the hooked prompt",
            steps.len(),
            steps.join("; "),
            if resume.is_some() { "; then the inner-CLI resume" } else { "" }
        );
        // `SPAWN_GEN` starts at 1, so 0 is never a live generation: an arm
        // with no session in the map (impossible today — launch inserts
        // first) fails CLOSED, aborting rather than typing blind.
        let spawn_gen = self.sessions.lock().get(&id).map(|s| s.gen).unwrap_or(0);
        self.reestablish.lock().insert(
            id,
            Reestablish {
                steps,
                idx: 0,
                phase: Phase::AwaitPrompt,
                resume,
                hint,
                spawn_gen,
            },
        );
    }

    /// GENERATION BINDING: is the armed sequence still talking to the spawn
    /// it was armed for? A relaunch (a flapping ssh link is the field shape)
    /// bumps `Session::gen`; anything still armed for the previous one must
    /// never type into the successor.
    fn reestablish_gen_ok(&self, id: Uuid) -> bool {
        let armed = self.reestablish.lock().get(&id).map(|e| e.spawn_gen);
        let live = self.sessions.lock().get(&id).map(|s| s.gen);
        match (armed, live) {
            (Some(a), Some(l)) => a == l,
            // No entry left (already finished/cancelled) is not a stale
            // generation; no session at all is handled by the write path.
            (None, _) => true,
            (Some(_), None) => false,
        }
    }

    /// Drop any supervision. `why` is logged only when an entry existed
    /// (delete/exit teardown calls this unconditionally and silently). A
    /// sequence that still owed an inner-CLI resume leaves the manual hint
    /// in the preface (nested-cli-resume: skipped/aborted ⇒ the hint prints,
    /// exactly as the notice used to carry it).
    pub(super) fn cancel_reestablish(&self, id: Uuid, why: &str) {
        let entry = self.reestablish.lock().remove(&id);
        if let Some(e) = entry {
            log::info!("terminal {id}: nested chain re-establish stopped — {why}");
            self.push_resume_hint(id, &e);
        }
    }

    /// The un-run resume step's manual fallback: push the hint as a preface
    /// info line (visible on the next attach/replay — live clients already
    /// have the same command inline in the seam notice) and log it. No-op
    /// for hint-less sequences and dead sessions.
    fn push_resume_hint(&self, id: Uuid, e: &Reestablish) {
        let Some(hint) = &e.hint else { return };
        if let Some(s) = self.sessions.lock().get(&id) {
            s.preface.lock().push_info_line(hint);
            log::info!("terminal {id}: inner-CLI resume skipped — {hint}");
        }
    }

    /// A token-checked `pre` landed for `id` (on_block_event). AwaitPrompt →
    /// the outer prompt has settled: type step 0. Watch → the OUTER prompt
    /// returned while a nested step was supposedly holding the shell — the
    /// nested world collapsed (e.g. `sudo` refused); abort the remainder
    /// instead of typing chain commands at the outer prompt.
    ///
    /// Returns whether this engine CONSUMED the pre as its trigger/re-base
    /// (AwaitPrompt/PendingSend). The caller keys the breadcrumb clear on
    /// it (nested-cli-resume, hypothesis c): the armed spawn's trigger pre
    /// must NOT retire the chain — the auto-typed commands are about to
    /// recreate exactly the world it records, and retiring it here
    /// destroyed the resume identity (sid + cli_cwd) every reconnect
    /// cycle. Retirement belongs to the abort paths and to the next pre
    /// this engine does not consume (the nested world really ended).
    pub(super) fn reestablish_on_pre(&self, id: Uuid) -> bool {
        let (consumed, aborted) = {
            let mut map = self.reestablish.lock();
            let Some(entry) = map.get_mut(&id) else {
                return false;
            };
            let consumed = pre_consumes(&entry.phase);
            let aborted = match entry.phase {
                Phase::AwaitPrompt => {
                    // Type on the next pump tick (SEND_GRACE) so the prompt
                    // finishes painting under the echo.
                    entry.phase = Phase::PendingSend {
                        step: entry.idx,
                        at: Instant::now() + SEND_GRACE,
                    };
                    None
                }
                // A second pre while the send is still pending (e.g. an
                // extra prompt refresh) just re-bases the grace beat.
                Phase::PendingSend { step, .. } => {
                    entry.phase = Phase::PendingSend {
                        step,
                        at: Instant::now() + SEND_GRACE,
                    };
                    None
                }
                Phase::Watch { .. } => map.remove(&id),
                // nested-shell-hooks: parked for the nested hooked prompt,
                // and the OUTER prompt spoke instead — the nested shell is
                // gone; the resume must not be typed at the login shell.
                Phase::AwaitNestedPrompt { .. } => map.remove(&id),
                // typed-ssh-nested: same rule one link earlier — the outer
                // prompt returning while a step's hooks were landing means
                // that step collapsed; the rest of the chain must not be
                // typed at the login shell.
                Phase::AwaitStepHooked { .. } => map.remove(&id),
            };
            (consumed, aborted)
        };
        if let Some(e) = aborted {
            log::info!(
                "terminal {id}: nested chain re-establish stopped — outer prompt returned mid-chain (the nested step did not hold)"
            );
            self.push_resume_hint(id, &e);
        }
        consumed
    }

    /// nested-shell-hooks: a NESTED-scope `pre` landed — the injected shell
    /// has painted its first hooked prompt. That is exactly the witness the
    /// parked resume was waiting for: the shell accepting the next line is
    /// the re-established nested shell AND its exec hook will attribute the
    /// CLI (spec-I1 ordering intact, now with attribution).
    pub(super) fn reestablish_on_nested_pre(&self, id: Uuid) {
        let entry = {
            let mut map = self.reestablish.lock();
            match map.get(&id).map(|e| e.phase) {
                Some(Phase::AwaitNestedPrompt { .. }) => map.remove(&id),
                _ => None,
            }
        };
        let Some(e) = entry else { return };
        self.finish_resume(id, &e, "the nested shell's hooked prompt settled");
    }

    /// Type the parked/settled inner-CLI resume, or leave the manual hint.
    fn finish_resume(&self, id: Uuid, e: &Reestablish, why: &str) {
        let live = self.sessions.lock().get(&id).map(|s| s.gen);
        if live != Some(e.spawn_gen) {
            log::info!(
                "terminal {id}: nested chain re-establish abandoned — the session was relaunched under a new generation; the inner-CLI resume is not typed"
            );
            self.push_resume_hint(id, e);
            return;
        }
        let n = e.steps.len();
        match &e.resume {
            Some(cmd) if self.type_reestablish_line(id, cmd) => {
                log::info!(
                    "terminal {id}: nested chain re-established ({n} step(s) typed, {why}); resuming the inner CLI: {cmd}"
                );
            }
            Some(_) => {
                log::info!(
                    "terminal {id}: nested chain re-established ({n} step(s) typed) but the session is gone — inner-CLI resume skipped"
                );
                self.push_resume_hint(id, e);
            }
            None => {
                log::info!("terminal {id}: nested chain re-established ({n} step(s) typed)");
            }
        }
    }

    /// Type one command line + Enter into the PTY (the C2D::Input write
    /// path — writer cloned out, write outside the sessions lock). Returns
    /// false when the session is gone.
    pub(super) fn type_reestablish_line(&self, id: Uuid, cmd: &str) -> bool {
        let writer = self.sessions.lock().get(&id).map(|s| s.writer.clone());
        let Some(w) = writer else { return false };
        use std::io::Write;
        let mut w = w.lock();
        // EVERY leg is load-bearing. Discarding these errors and returning
        // `true` reported a send that never reached the PTY - the caller
        // then advanced the chain (or declared the resume typed) on a write
        // that failed, which is the "sends after uncertainty" class in its
        // purest form. A half-written line is worse than none, so the caller
        // abandons this generation.
        let ok = w.write_all(cmd.as_bytes()).is_ok()
            && w.write_all(b"\r").is_ok()
            && w.flush().is_ok();
        if !ok {
            log::warn!(
                "terminal {id}: the PTY write failed mid-line - this re-establish generation is abandoned"
            );
        }
        ok
    }

    /// Type steps[idx], then move the entry to Watch with the CURRENT
    /// journal length as the quiescence base.
    fn send_reestablish_step(&self, id: Uuid, idx: usize) {
        if !self.reestablish_gen_ok(id) {
            self.cancel_reestablish(
                id,
                "the session was relaunched under a new generation — the stale chain is abandoned rather than typed into the new one",
            );
            return;
        }
        let cmd = match self.reestablish.lock().get(&id) {
            Some(e) => match e.steps.get(idx) {
                Some(c) => c.clone(),
                None => return,
            },
            None => return,
        };
        let total = self.reestablish.lock().get(&id).map(|e| e.steps.len()).unwrap_or(0);
        log::info!(
            "terminal {id}: nested chain re-establish — typing step {}/{}: {cmd}",
            idx + 1,
            total
        );
        if !self.type_reestablish_line(id, &cmd) {
            self.cancel_reestablish(id, "session gone before the step could be typed");
            return;
        }
        let len = self
            .journal(id)
            .map(|j| j.lock().absolute_len())
            .unwrap_or(0);
        let now = Instant::now();
        if let Some(e) = self.reestablish.lock().get_mut(&id) {
            e.idx = idx;
            e.phase = Phase::Watch {
                sent: now,
                last_len: len,
                last_change: now,
            };
        }
    }

    /// The step-gating engine, riding the 250ms flush tick beside
    /// `pump_reconnects`. Watches quiescence, aborts on credential prompts /
    /// timeouts / dead sessions, and types the next step when settled.
    pub(super) fn pump_reestablish(self: &Arc<Self>) {
        /// A Watch-phase row: (id, sent, last_len, last_change, has_more).
        type WatchRow = (Uuid, Instant, u64, Instant, bool);
        let now = Instant::now();
        let parked: Vec<(Uuid, Instant)>;
        let step_holds: Vec<(Uuid, usize, Instant)>;
        let (due_sends, watching): (Vec<(Uuid, usize)>, Vec<WatchRow>) = {
            let map = self.reestablish.lock();
            if map.is_empty() {
                return;
            }
            // CONTAINMENT (was: "types the resume anyway"). A park that
            // outlived NESTED_HOOK_WAIT means the injection into the shell
            // the resume would run in NEVER resolved — neither hooked nor
            // honestly given up (a give-up flips the state to Absent, which
            // releases the park through the ordinary `resume_may_send`
            // path below and still types). An unresolved wait is exactly
            // "the previous hop is not positively confirmed": stop, and
            // leave the user the manual resume line instead of typing a
            // session-restoring command into a shell of unknown identity.
            parked = map
                .iter()
                .filter_map(|(id, e)| match e.phase {
                    Phase::AwaitNestedPrompt { since } => Some((*id, since)),
                    _ => None,
                })
                .collect();
            // typed-ssh-nested: steps held for the previous step's injection.
            step_holds = map
                .iter()
                .filter_map(|(id, e)| match e.phase {
                    Phase::AwaitStepHooked { since, next } => Some((*id, next, since)),
                    _ => None,
                })
                .collect();
            let sends = map
                .iter()
                .filter_map(|(id, e)| match e.phase {
                    Phase::PendingSend { step, at } if now >= at => Some((*id, step)),
                    _ => None,
                })
                .collect();
            let watches = map
                .iter()
                .filter_map(|(id, e)| match e.phase {
                    Phase::Watch {
                        sent,
                        last_len,
                        last_change,
                    } => Some((*id, sent, last_len, last_change, e.idx + 1 < e.steps.len())),
                    _ => None,
                })
                .collect();
            (sends, watches)
        };
        for (id, since) in parked {
            match hold_action(
                now.duration_since(since) >= NESTED_HOOK_WAIT,
                self.nesthook_state(id),
            ) {
                HoldAction::Wait => {}
                HoldAction::Send => {
                    let entry = self.reestablish.lock().remove(&id);
                    if let Some(e) = entry {
                        self.finish_resume(id, &e, "the nested shell's hook injection resolved");
                    }
                }
                HoldAction::Abort => self.cancel_reestablish(
                    id,
                    "the nested shell never resolved its hooks within the wait — the resume is not typed into an unconfirmed shell (the manual line is in the preface)",
                ),
            }
        }
        // typed-ssh-nested: release a held step the moment the previous
        // step's injection resolves — hooked, or honestly given up — and
        // release it anyway once the hold outlives that give-up, so an
        // unhookable link never costs the rest of the chain.
        for (id, next, since) in step_holds {
            match hold_action(
                now.duration_since(since) >= NESTED_HOOK_WAIT,
                self.nesthook_state(id),
            ) {
                HoldAction::Wait => {}
                HoldAction::Send => self.send_reestablish_step(id, next),
                // CONTAINMENT (was: "typing on"): the previous hop was never
                // positively confirmed, so the next step would be typed into
                // whatever shell actually holds the line. A partially-entered
                // chain is not a place to keep typing.
                HoldAction::Abort => self.cancel_reestablish(
                    id,
                    "the previous chain step never resolved its hooks — the rest of the chain is not typed into an unconfirmed shell",
                ),
            }
        }
        for (id, step) in due_sends {
            self.send_reestablish_step(id, step);
        }
        for (id, sent, last_len, last_change, has_more) in watching {
            // A dead/asleep terminal ends the chain (its own relaunch path
            // re-arms from the persisted breadcrumb if one survives).
            let running = {
                let state = self.state.lock();
                state
                    .terminal(id)
                    .is_some_and(|t| t.status == TermStatus::Running && !t.asleep)
            };
            if !running {
                self.cancel_reestablish(id, "terminal is no longer running");
                continue;
            }
            let len = self
                .journal(id)
                .map(|j| j.lock().absolute_len())
                .unwrap_or(last_len);
            if len != last_len {
                // ABSOLUTE DEADLINE FIRST: re-basing the quiescence clock
                // and `continue`ing skipped `watch_action` entirely, so a
                // step whose output never stops (a chatty login, a stuck
                // progress bar) could hold the chain open indefinitely -
                // STEP_TIMEOUT is evaluated inside the call the `continue`
                // jumped over. Ask with quiet_for = 0: the only reachable
                // answers are Wait and AbortTimeout.
                if growth_watch(now.duration_since(sent), has_more) == WatchAction::AbortTimeout
                {
                    self.cancel_reestablish(id, "step never settled within 30s");
                    continue;
                }
                if let Some(e) = self.reestablish.lock().get_mut(&id) {
                    if let Phase::Watch {
                        last_len: ll,
                        last_change: lc,
                        ..
                    } = &mut e.phase
                    {
                        *ll = len;
                        *lc = now;
                    }
                }
                continue;
            }
            let quiet_for = now.duration_since(last_change);
            let since_sent = now.duration_since(sent);
            // The credential check reads the settled screen's LAST non-blank
            // line (mirror truth — same grid ctl `read --screen` serializes).
            let tail_line = || self.last_screen_line(id).unwrap_or_default();
            // typed-ssh-nested: a replayed `ssh <host>` step can land on the
            // host-key question just as easily as on a password prompt — both
            // are answers only the user may give, so both abort the chain.
            let action = watch_action(
                since_sent,
                quiet_for,
                quiet_for >= STEP_QUIET && {
                    let tail = tail_line();
                    credential_prompt_line(&tail)
                        || matches!(
                            crate::gui::composer::detect_auth_prompt(&tail),
                            crate::gui::composer::AuthPrompt::HostKey
                        )
                },
                has_more,
            );
            match action {
                WatchAction::Wait => {}
                WatchAction::AbortCredential => {
                    self.cancel_reestablish(
                        id,
                        "a prompt only you can answer appeared (a credential, or a host key to confirm) — finish it manually; Pulse never types those",
                    );
                }
                WatchAction::AbortTimeout => {
                    self.cancel_reestablish(id, "step never settled within 30s");
                }
                WatchAction::Next => {
                    let next = self.reestablish.lock().get(&id).map(|e| e.idx + 1);
                    if let Some(next) = next {
                        // typed-ssh-nested: the step settled, but Pulse may
                        // be mid-injection into the shell it just opened.
                        // Typing the next step now would push the reader
                        // line into the wrong shell — park instead (the
                        // `AwaitNestedPrompt` rule, one link earlier).
                        if !resume_may_send(self.nesthook_state(id)) {
                            if let Some(e) = self.reestablish.lock().get_mut(&id) {
                                e.phase = Phase::AwaitStepHooked { since: now, next };
                            }
                            log::debug!(
                                "terminal {id}: holding chain step {} for the previous step's hooks",
                                next + 1
                            );
                            continue;
                        }
                        self.send_reestablish_step(id, next);
                    }
                }
                WatchAction::Done => {
                    // Nested-cli-resume: the ONLY place the resume step may
                    // type. Reaching Done means every recorded chain command
                    // was typed AND settled cleanly (no credential prompt,
                    // no timeout, no outer-prompt collapse, no user
                    // takeover) — so the shell accepting the next line IS
                    // the fully re-established nested shell. That ordering
                    // resolves the original spec-I1 concern (an auto-resume
                    // fired at the OUTER prompt would run as the ssh login
                    // user against the wrong session store): the command
                    // executes strictly inside the nested context or not at
                    // all.
                    // nested-shell-hooks: the chain is up, but the hooks
                    // may still be going into it. PARK the resume until the
                    // nested shell reports its own hooked prompt — a resume
                    // typed one tick early runs unwitnessed and leaves the
                    // breadcrumb incomplete for the next reconnect.
                    let nest = self.nesthook_state(id);
                    if !resume_may_send(nest) {
                        if let Some(e) = self.reestablish.lock().get_mut(&id) {
                            if e.resume.is_some() {
                                e.phase = Phase::AwaitNestedPrompt { since: now };
                                log::info!(
                                    "terminal {id}: nested chain re-established; holding the inner-CLI resume for the nested shell's hooked prompt"
                                );
                                continue;
                            }
                        }
                    }
                    let Some(e) = self.reestablish.lock().remove(&id) else {
                        continue;
                    };
                    debug_assert_eq!(
                        e.idx + 1,
                        e.steps.len(),
                        "Done edge requires the LAST chain step to have settled"
                    );
                    debug_assert!(resume_may_type(action), "Done is the only resume edge");
                    self.finish_resume(id, &e, "every chain step settled");
                }
            }
        }
    }

    /// Last non-blank line of the live mirror screen (the grid `read
    /// --screen` serializes) — the settled prompt line the credential check
    /// inspects.
    pub(super) fn last_screen_line(&self, id: Uuid) -> Option<String> {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::cell::Flags;
        let term = self.sessions.lock().get(&id).map(|s| s.term.clone())?;
        let t = term.lock();
        let (cols, rows) = (t.columns(), t.screen_lines());
        for r in (0..rows).rev() {
            let row = &t.grid()[Line(r as i32)];
            let mut s = String::with_capacity(cols);
            for c in 0..cols {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                s.push(cell.c);
            }
            let s = s.trim_end().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F2 — the credential-prompt table: every classic secret prompt aborts;
    /// ordinary prompts and output lines never do.
    #[test]
    fn credential_prompt_table() {
        let yes = [
            "[sudo] password for tester:",
            "Password:",
            "password:",
            "tester@host's password:",
            "Enter passphrase for key '/home/t/.ssh/id_ed25519':",
            "Verification code:",
            "One-time password (OATH) for `tester':",
            "Enter OTP:",
            "PIN for token:",
            "Enter your authentication code:",
            "  Passcode or option (1-3):",
        ];
        for l in yes {
            assert!(credential_prompt_line(l), "must abort on: {l:?}");
        }
        let no = [
            "",
            "   ",
            "root@host:/var/log#",
            "tester@host:~$",
            "$",
            "#",
            "Welcome to Ubuntu 24.04.3 LTS",
            "Last login: Wed Jul 15 15:59:11 2026 from ::1",
            // Mid-stream mentions never end as the settled tail line, but
            // the classifier still must not fire on prompt-shaped rows.
            "PS C:\\Users\\z>",
        ];
        for l in no {
            assert!(!credential_prompt_line(l), "must NOT abort on: {l:?}");
        }
    }

    /// F2 — the step-gating truth table: quiescence gates every transition,
    /// the credential check only fires at the settled edge, timeout wins.
    #[test]
    fn watch_action_table() {
        let ms = Duration::from_millis;
        // Not yet quiet: wait, even with a scary tail (the check is only
        // meaningful once output stops — callers pass false pre-settle, but
        // the timeout precedence is pinned here too).
        assert_eq!(watch_action(ms(100), ms(100), false, true), WatchAction::Wait);
        assert_eq!(watch_action(ms(600), ms(699), false, false), WatchAction::Wait);
        // Settled + credential ⇒ abort, regardless of remaining steps.
        assert_eq!(
            watch_action(ms(1000), ms(700), true, true),
            WatchAction::AbortCredential
        );
        assert_eq!(
            watch_action(ms(1000), ms(700), true, false),
            WatchAction::AbortCredential
        );
        // Settled + clean ⇒ next step / done.
        assert_eq!(watch_action(ms(1000), ms(700), false, true), WatchAction::Next);
        assert_eq!(watch_action(ms(1000), ms(700), false, false), WatchAction::Done);
        // 30s without settling ⇒ abort; timeout outranks everything.
        assert_eq!(
            watch_action(Duration::from_secs(30), ms(100), false, true),
            WatchAction::AbortTimeout
        );
        assert_eq!(
            watch_action(Duration::from_secs(31), Duration::from_secs(31), true, true),
            WatchAction::AbortTimeout
        );
    }

    /// F2 — the launch-time arm gate: breadcrumb + hooked + opt-in, all
    /// three or nothing (a hookless spawn has no prompt witness; the honest
    /// preface alone covers it — pre-F2 behavior).
    /// astra fix 4a (reconnect half): a step whose output never stops used
    /// to skip `watch_action` entirely on every growth tick, so STEP_TIMEOUT
    /// - the only bound on a step that never settles - was never evaluated.
    #[test]
    fn growth_ticks_still_enforce_the_step_deadline() {
        let ms = Duration::from_millis;
        assert_eq!(growth_watch(ms(0), true), WatchAction::Wait);
        assert_eq!(growth_watch(STEP_TIMEOUT - ms(1), true), WatchAction::Wait);
        assert_eq!(growth_watch(STEP_TIMEOUT, true), WatchAction::AbortTimeout);
        assert_eq!(
            growth_watch(STEP_TIMEOUT, false),
            WatchAction::AbortTimeout,
            "the last step is bounded too"
        );
        // A growth tick can never be mistaken for a settled step.
        assert_ne!(growth_watch(ms(10), true), WatchAction::Next);
        assert_ne!(growth_watch(ms(10), false), WatchAction::Done);
    }

    /// astra fix 5 - RECONNECT CONTAINMENT: the engine must refuse to type
    /// the next line while the previous hop is not positively confirmed.
    ///
    /// A hop resolves either way - `Hooked` (it announced itself) or
    /// `Absent` (nothing to inject / the injection honestly gave up) - and
    /// then the chain continues. Still `Pending` at the deadline is the
    /// unconfirmed case, and it used to "type on" / "resume unattributed":
    /// a chain step, or a session-restoring resume, typed into a shell whose
    /// identity was never established. Now it aborts and leaves the manual
    /// line.
    #[test]
    fn hold_action_refuses_to_type_after_an_unconfirmed_hop() {
        // Resolved: send, deadline or not.
        for expired in [false, true] {
            assert_eq!(hold_action(expired, NestState::Hooked), HoldAction::Send);
            assert_eq!(hold_action(expired, NestState::Absent), HoldAction::Send);
        }
        // Unresolved inside the wait: hold the line.
        assert_eq!(hold_action(false, NestState::Pending), HoldAction::Wait);
        // Unresolved AT the deadline: abort - never type on.
        assert_eq!(hold_action(true, NestState::Pending), HoldAction::Abort);
        // The predicate agrees with the resume gate it shares (one rule).
        for nest in [NestState::Hooked, NestState::Absent, NestState::Pending] {
            assert_eq!(
                hold_action(false, nest) == HoldAction::Send,
                resume_may_send(nest)
            );
        }
    }

    #[test]
    fn reestablish_arm_gate() {
        assert!(reestablish_should_arm(true, true, true));
        assert!(!reestablish_should_arm(false, true, true), "no breadcrumb");
        assert!(!reestablish_should_arm(true, false, true), "hookless spawn");
        assert!(!reestablish_should_arm(true, true, false), "opted out");
    }

    /// Nested-cli-resume (hypothesis-c fix) — breadcrumb persistence across
    /// a re-establish cycle: the armed spawn's trigger pre (and any re-base
    /// pre while the send is pending) is CONSUMED, so on_block_event skips
    /// the breadcrumb clear and the resume identity (sid + cli_cwd)
    /// survives the cycle. A Watch-phase pre (chain collapsed) is NOT
    /// consumed — the ordinary retirement runs, exactly as before.
    #[test]
    fn pre_consumption_table() {
        let now = Instant::now();
        assert!(pre_consumes(&Phase::AwaitPrompt), "trigger pre must not clear the chain");
        assert!(
            pre_consumes(&Phase::PendingSend { step: 0, at: now }),
            "re-base pre must not clear the chain"
        );
        assert!(
            !pre_consumes(&Phase::Watch {
                sent: now,
                last_len: 0,
                last_change: now,
            }),
            "an outer prompt mid-chain aborts AND retires the chain"
        );
    }

    /// Nested-cli-resume — the resume step types on the Done edge ONLY:
    /// a partial chain (Next), an unsettled step (Wait), a credential
    /// abort, or a timeout must never reach it. (The keystroke-cancel leg
    /// removes the supervision entry entirely — C2D::Input/SubmitCommand →
    /// `cancel_reestablish("user input")` — so no action is ever computed
    /// for it; the abort paths above pin the remaining matrix.)
    #[test]
    fn resume_step_gating_matrix() {
        assert!(resume_may_type(WatchAction::Done));
        assert!(!resume_may_type(WatchAction::Next), "partial chain must never resume");
        assert!(!resume_may_type(WatchAction::Wait), "unsettled step must never resume");
        assert!(
            !resume_may_type(WatchAction::AbortCredential),
            "credential abort applies to the resume step too"
        );
        assert!(!resume_may_type(WatchAction::AbortTimeout));
        // And the composed matrix stays coherent with the watcher: a
        // settled credential tail on the LAST step aborts instead of
        // resuming (the resume would otherwise type into a password
        // prompt).
        let ms = Duration::from_millis;
        let last_step_settled_credential = watch_action(ms(1000), ms(700), true, false);
        assert_eq!(last_step_settled_credential, WatchAction::AbortCredential);
        assert!(!resume_may_type(last_step_settled_credential));
    }
}
