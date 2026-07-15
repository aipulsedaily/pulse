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
    /// stashed at launch — the breadcrumb itself retires on the first pre).
    steps: Vec<String>,
    /// Next step to type (steps[idx]).
    idx: usize,
    phase: Phase,
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Armed at spawn; waiting for the outer shell's first token-checked
    /// `pre` (hooked prompt settled) before typing step 0.
    AwaitPrompt,
    /// A step was typed; watching the journal for quiescence.
    Watch {
        sent: Instant,
        last_len: u64,
        last_change: Instant,
    },
}

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
/// opt-out (default ON) says yes.
pub(crate) fn reestablish_should_arm(chain_present: bool, hooked: bool, opt_in: bool) -> bool {
    chain_present && hooked && opt_in
}

impl Core {
    /// Arm at spawn success (launch() calls this after the session is in the
    /// map). Replaces any stale entry — the newest spawn owns the chain.
    pub(super) fn arm_reestablish(&self, id: Uuid, steps: Vec<String>) {
        if steps.is_empty() {
            return;
        }
        log::info!(
            "terminal {id}: nested chain re-establish armed ({} step(s): {}) — waiting for the hooked prompt",
            steps.len(),
            steps.join("; ")
        );
        self.reestablish.lock().insert(
            id,
            Reestablish {
                steps,
                idx: 0,
                phase: Phase::AwaitPrompt,
            },
        );
    }

    /// Drop any supervision. `why` is logged only when an entry existed
    /// (delete/exit teardown calls this unconditionally and silently).
    pub(super) fn cancel_reestablish(&self, id: Uuid, why: &str) {
        if self.reestablish.lock().remove(&id).is_some() {
            log::info!("terminal {id}: nested chain re-establish stopped — {why}");
        }
    }

    /// A token-checked `pre` landed for `id` (on_block_event). AwaitPrompt →
    /// the outer prompt has settled: type step 0. Watch → the OUTER prompt
    /// returned while a nested step was supposedly holding the shell — the
    /// nested world collapsed (e.g. `sudo` refused); abort the remainder
    /// instead of typing chain commands at the outer prompt.
    pub(super) fn reestablish_on_pre(&self, id: Uuid) {
        let action = {
            let mut map = self.reestablish.lock();
            match map.get_mut(&id) {
                Some(entry) => match entry.phase {
                    Phase::AwaitPrompt => {
                        // Phase moves to Watch inside send_reestablish_step
                        // (it needs the post-write journal length); mark the
                        // send outside the lock.
                        Some(entry.idx)
                    }
                    Phase::Watch { .. } => {
                        map.remove(&id);
                        log::info!(
                            "terminal {id}: nested chain re-establish stopped — outer prompt returned mid-chain (the nested step did not hold)"
                        );
                        None
                    }
                },
                None => None,
            }
        };
        if let Some(idx) = action {
            self.send_reestablish_step(id, idx);
        }
    }

    /// Type steps[idx] + Enter into the PTY (the C2D::Input write path —
    /// writer cloned out, write outside the sessions lock), then move the
    /// entry to Watch with the CURRENT journal length as the quiescence base.
    fn send_reestablish_step(&self, id: Uuid, idx: usize) {
        let cmd = match self.reestablish.lock().get(&id) {
            Some(e) => match e.steps.get(idx) {
                Some(c) => c.clone(),
                None => return,
            },
            None => return,
        };
        let writer = self.sessions.lock().get(&id).map(|s| s.writer.clone());
        let Some(w) = writer else {
            self.cancel_reestablish(id, "session gone before the step could be typed");
            return;
        };
        let total = self.reestablish.lock().get(&id).map(|e| e.steps.len()).unwrap_or(0);
        log::info!(
            "terminal {id}: nested chain re-establish — typing step {}/{}: {cmd}",
            idx + 1,
            total
        );
        {
            use std::io::Write;
            let mut w = w.lock();
            let _ = w.write_all(cmd.as_bytes());
            let _ = w.write_all(b"\r");
            let _ = w.flush();
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
        let watching: Vec<(Uuid, Instant, u64, Instant, bool)> = {
            let map = self.reestablish.lock();
            if map.is_empty() {
                return;
            }
            map.iter()
                .filter_map(|(id, e)| match e.phase {
                    Phase::Watch {
                        sent,
                        last_len,
                        last_change,
                    } => Some((*id, sent, last_len, last_change, e.idx + 1 < e.steps.len())),
                    Phase::AwaitPrompt => None,
                })
                .collect()
        };
        let now = Instant::now();
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
            let action = watch_action(
                since_sent,
                quiet_for,
                quiet_for >= STEP_QUIET && credential_prompt_line(&tail_line()),
                has_more,
            );
            match action {
                WatchAction::Wait => {}
                WatchAction::AbortCredential => {
                    self.cancel_reestablish(
                        id,
                        "a credential prompt appeared — finish it manually (credentials are never auto-typed)",
                    );
                }
                WatchAction::AbortTimeout => {
                    self.cancel_reestablish(id, "step never settled within 30s");
                }
                WatchAction::Next => {
                    let next = self.reestablish.lock().get(&id).map(|e| e.idx + 1);
                    if let Some(next) = next {
                        self.send_reestablish_step(id, next);
                    }
                }
                WatchAction::Done => {
                    let n = self
                        .reestablish
                        .lock()
                        .remove(&id)
                        .map(|e| e.steps.len())
                        .unwrap_or(0);
                    log::info!(
                        "terminal {id}: nested chain re-established ({n} step(s) typed)"
                    );
                }
            }
        }
    }

    /// Last non-blank line of the live mirror screen (the grid `read
    /// --screen` serializes) — the settled prompt line the credential check
    /// inspects.
    fn last_screen_line(&self, id: Uuid) -> Option<String> {
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
    #[test]
    fn reestablish_arm_gate() {
        assert!(reestablish_should_arm(true, true, true));
        assert!(!reestablish_should_arm(false, true, true), "no breadcrumb");
        assert!(!reestablish_should_arm(true, false, true), "hookless spawn");
        assert!(!reestablish_should_arm(true, true, false), "opted out");
    }
}
