//! SSH auto-reconnect: the backoff state machine (task D13). A child
//! module of `daemon` (private-field access to `Core` is deliberate — this
//! is the reconnect half of Core, not a separate abstraction): the pure
//! qualification/backoff functions, the `Reconnect` supervision record, and
//! the Core methods that drive it. The pump rides the 250ms journal-flush
//! tick in mod.rs; launches route through `Core::probe_aware_launch` (also
//! mod.rs) so a probe-due reconnect never blocks that tick.

use super::*;

/// One ssh auto-reconnect supervision. Backoff: attempts fire 2s/10s/30s
/// after the (previous) death; success = the fresh session's first
/// token-checked `pre` hook (the link is interactive again — the same
/// witness the qualification used); give-up = attempts exhausted (terminal
/// stays Dead with the ordinary Restore affordances) or 30s running without
/// hooks (interactive auth wall — the attempt is left running, honest).
#[derive(Debug, Clone, Copy)]
pub(super) struct Reconnect {
    /// Attempts already fired (0 = none yet). u32: a MANUAL supervision is
    /// unlimited — a long outage can run past 255 attempts.
    attempt: u32,
    /// Waiting: when the next attempt fires. Watching: when the in-flight
    /// attempt was spawned (hook-arrival deadline base).
    at: Instant,
    /// An attempt is currently in flight (spawned, hooks not yet seen).
    watching: bool,
    /// USER-INITIATED supervision (`Retry ▸` / `tc retry`, ssh-reestablish
    /// F1): the ladder never exhausts — backoff grows to the 30s ceiling and
    /// attempts continue until success, Cancel, or the auth-wall stop. The
    /// automatic (`hooks_were_live`) path keeps its 3-attempt cap.
    manual: bool,
}

/// SSH reconnect backoff table (seconds after death/failure). Tests pin it.
const RECONNECT_BACKOFF: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
/// F1: the manual ladder's backoff ceiling — every rung past the table
/// repeats at this pace, forever, until success or Cancel.
const MANUAL_BACKOFF_CEILING: Duration = Duration::from_secs(30);
/// How long a spawned reconnect attempt may run without its hooks arming
/// before supervision stops (the attempt itself is LEFT RUNNING — it may be
/// sitting at an interactive auth prompt, which is a usable terminal).
const RECONNECT_HOOK_WINDOW: Duration = Duration::from_secs(30);

/// Delay before the next attempt after `attempts_done` failures; None =
/// exhausted (give up to Dead + the ordinary Restore affordances). A MANUAL
/// supervision (F1: the user said "keep trying until my server is back")
/// never exhausts: past the table it repeats at the 30s ceiling, unlimited —
/// success, Cancel, or the per-attempt auth-wall stop are the only exits.
/// Probe staging (`TC_RETRY_BACKOFF_MS`, TC_DATA_DIR-isolated builds only —
/// the TC_SSH_VIA_WSL guard class) flattens every rung to a constant so the
/// unlimited ladder is provable in seconds, never against installed daemons.
fn reconnect_backoff_after(attempts_done: u32, manual: bool) -> Option<Duration> {
    if let Ok(ms) = std::env::var("TC_RETRY_BACKOFF_MS") {
        if crate::state::data_dir_overridden() {
            if let Ok(ms) = ms.parse::<u64>() {
                let flat = Duration::from_millis(ms.clamp(50, 60_000));
                return (manual || (attempts_done as usize) < RECONNECT_BACKOFF.len())
                    .then_some(flat);
            }
        }
    }
    let table = RECONNECT_BACKOFF.get(attempts_done as usize).copied();
    if manual {
        Some(table.unwrap_or(MANUAL_BACKOFF_CEILING))
    } else {
        table
    }
}

/// The pure ssh auto-reconnect qualification (maybe_schedule_reconnect owns
/// the witnesses): NOT deliberate, NOT a clean remote exit (code 0 = the
/// user typed `exit`), the dying connection had HOOKED (bootstrap ran ⇒
/// auth completed without interaction — password hosts and never-connected
/// sessions never qualify), family Ssh, not asleep, opt-in (default on).
fn reconnect_qualifies(
    expected: bool,
    code: Option<u32>,
    hooks_were_live: bool,
    is_ssh: bool,
    asleep: bool,
    opt_in: bool,
) -> bool {
    !expected && code != Some(0) && hooks_were_live && is_ssh && !asleep && opt_in
}

/// The MANUAL retry qualification (dead-relaunch fix b, proto 13): an
/// explicit `Retry ▸` click skips the auto path's witnesses — deliberate?
/// exit code? `hooks_were_live`? opt-in? — because the click IS the consent
/// (a user retrying a password host gets exactly one spawned attempt per
/// rung, and the RECONNECT_HOOK_WINDOW auth-wall stop still ends
/// supervision; the AUTO gates stay untouched for the automatic path). It
/// still requires: family Ssh (relaunch loops make no sense elsewhere —
/// plain Restore covers those), plainly Dead (never surprise-restart a
/// running terminal; asleep is the stronger, deliberate intent), and no
/// supervision already running (idempotent double-click).
fn manual_retry_allowed(is_ssh: bool, dead: bool, asleep: bool, supervised: bool) -> bool {
    is_ssh && dead && !asleep && !supervised
}

impl Core {
    /// SSH auto-reconnect qualification, run on every exit (D13 revisited
    /// with field evidence: two hooked ssh sessions died with exit 255 at
    /// the same second — a link-level event across PC sleep — and stayed
    /// Dead until manual restores). Qualifies when ALL hold:
    ///   - the exit was NOT deliberate (kill/delete/sleep stamped it),
    ///   - exit code != 0 (a clean remote `exit` is the user leaving),
    ///   - the dying connection had HOOKED (its bootstrap ran ⇒ auth
    ///     completed non-interactively — password hosts and never-connected
    ///     sessions never qualify),
    ///   - family is Ssh, terminal still exists, not asleep,
    ///   - ShellCfg.auto_reconnect (default ON) — the opt-out.
    ///
    /// A death of an in-flight ATTEMPT advances the backoff instead.
    pub(super) fn maybe_schedule_reconnect(
        &self,
        id: Uuid,
        code: Option<u32>,
        expected: bool,
        hooks_were_live: bool,
    ) {
        // An existing supervision: the death of a watched attempt advances
        // the backoff; a waiting entry is untouched (the pump owns it).
        let prior = self.reconnects.lock().get(&id).copied();
        if let Some(rc) = prior {
            if rc.watching {
                self.advance_reconnect(id, rc.attempt, rc.manual);
            }
            return;
        }
        let (is_ssh, asleep, opt_in) = {
            let state = self.state.lock();
            match state.terminal(id) {
                Some(t) => (
                    matches!(
                        crate::state::shell_family(&t.kind, &t.program, &t.args),
                        crate::state::ShellFamily::Ssh { .. }
                    ),
                    t.asleep,
                    t.shell_cfg.as_ref().is_none_or(|c| c.auto_reconnect),
                ),
                None => return,
            }
        };
        if !reconnect_qualifies(expected, code, hooks_were_live, is_ssh, asleep, opt_in) {
            return;
        }
        log::info!(
            "terminal {id}: unexpected ssh death (exit {code:?}) — auto-reconnect in {:?}",
            RECONNECT_BACKOFF[0]
        );
        self.reconnects.lock().insert(
            id,
            Reconnect {
                attempt: 0,
                at: Instant::now() + reconnect_backoff_after(0, false).unwrap_or(RECONNECT_BACKOFF[0]),
                watching: false,
                manual: false,
            },
        );
        self.set_reconnecting_flag(id, true);
    }

    /// C2D::RetryReconnect / ctl `retry` (proto 13, dead-relaunch fix b +
    /// ssh-reestablish F1): user-initiated entry into the same supervision
    /// maybe_schedule_reconnect runs — the identical 2s/10s/30s ladder,
    /// resolved by the fresh session's first token-checked `pre`,
    /// cancellable via CancelReconnect, and still subject to the
    /// RECONNECT_HOOK_WINDOW interactive-auth stop (an attempt sitting at a
    /// password prompt ends the loop; the attempt is left running). Two
    /// differences from the automatic path: the entry gate is
    /// `manual_retry_allowed` (the click IS the consent) instead of the
    /// `reconnect_qualifies` witnesses — which stay untouched — and the
    /// ladder is UNLIMITED past the table (30s ceiling) instead of capped at
    /// 3: the user said "keep trying until my server is back". Returns the
    /// typed refusal for the ctl surface; the C2D path drops it.
    pub(super) fn manual_reconnect(&self, id: Uuid) -> Result<(), (&'static str, String)> {
        let (is_ssh, dead, asleep) = {
            let state = self.state.lock();
            match state.terminal(id) {
                Some(t) => (
                    matches!(
                        crate::state::shell_family(&t.kind, &t.program, &t.args),
                        crate::state::ShellFamily::Ssh { .. }
                    ),
                    t.status == TermStatus::Dead,
                    t.asleep,
                ),
                None => return Err(("not_found", format!("no terminal {id}"))),
            }
        };
        {
            // Check-and-insert under ONE lock: a concurrent auto-schedule
            // (or a double-click racing itself) must not stack entries.
            let mut map = self.reconnects.lock();
            if !manual_retry_allowed(is_ssh, dead, asleep, map.contains_key(&id)) {
                return Err(if !is_ssh {
                    ("not_ssh", "retry is for ssh terminals; use restart".into())
                } else if asleep {
                    ("asleep", "terminal is asleep; wake it instead".into())
                } else if !dead {
                    ("running", "terminal is not dead".into())
                } else {
                    ("supervised", "a reconnect supervision is already running".into())
                });
            }
            let first = reconnect_backoff_after(0, true).unwrap_or(RECONNECT_BACKOFF[0]);
            log::info!(
                "terminal {id}: manual ssh reconnect requested — first attempt in {first:?}, unlimited attempts until success or cancel"
            );
            map.insert(
                id,
                Reconnect {
                    attempt: 0,
                    at: Instant::now() + first,
                    watching: false,
                    manual: true,
                },
            );
        }
        self.set_reconnecting_flag(id, true);
        self.set_retry_progress(
            id,
            0,
            reconnect_backoff_after(0, true)
                .unwrap_or(RECONNECT_BACKOFF[0])
                .as_secs() as u32,
        );
        Ok(())
    }

    /// After `attempts_done` failed attempts: schedule the next backoff step
    /// or give up (terminal stays Dead; the ordinary Restore affordances and
    /// boot-restore semantics apply from here). A manual ladder never gives
    /// up — `reconnect_backoff_after` returns the 30s ceiling forever.
    pub(super) fn advance_reconnect(&self, id: Uuid, attempts_done: u32, manual: bool) {
        let Some(delay) = reconnect_backoff_after(attempts_done, manual) else {
            log::info!(
                "terminal {id}: ssh reconnect gave up after {attempts_done} attempts"
            );
            self.reconnects.lock().remove(&id);
            self.set_reconnecting_flag(id, false);
            return;
        };
        log::info!("terminal {id}: ssh reconnect attempt {attempts_done} failed — next in {delay:?}");
        self.reconnects.lock().insert(
            id,
            Reconnect {
                attempt: attempts_done,
                at: Instant::now() + delay,
                watching: false,
                manual,
            },
        );
        if manual {
            // Honest lane: `retrying — attempt N · next in Ss`.
            self.set_retry_progress(id, attempts_done, delay.as_secs() as u32);
        }
    }

    /// The backoff engine, riding the 250ms flush tick. Fires due attempts
    /// through launch() (reconnect IS restart — same preface/seam/bootstrap
    /// machinery), and expires watched attempts whose hooks never arrived
    /// (interactive auth wall: supervision stops, the attempt is LEFT
    /// running — a password prompt is a usable terminal).
    pub(super) fn pump_reconnects(self: &Arc<Self>) {
        let now = Instant::now();
        let due: Vec<(Uuid, Reconnect)> = {
            let map = self.reconnects.lock();
            if map.is_empty() {
                return;
            }
            map.iter()
                .filter(|(_, rc)| {
                    if rc.watching {
                        now.duration_since(rc.at) >= RECONNECT_HOOK_WINDOW
                    } else {
                        now >= rc.at
                    }
                })
                .map(|(id, rc)| (*id, *rc))
                .collect()
        };
        for (id, rc) in due {
            if rc.watching {
                log::info!(
                    "terminal {id}: ssh reconnect attempt {} running without hooks for {:?} — supervision stopped (interactive auth?)",
                    rc.attempt,
                    RECONNECT_HOOK_WINDOW
                );
                self.reconnects.lock().remove(&id);
                self.set_reconnecting_flag(id, false);
                continue;
            }
            let status = {
                let state = self.state.lock();
                state.terminal(id).map(|t| (t.status, t.asleep))
            };
            match status {
                None => {
                    // Deleted while waiting.
                    self.reconnects.lock().remove(&id);
                }
                Some((_, true)) => {
                    // Slept while waiting — sleep is the stronger intent.
                    self.reconnects.lock().remove(&id);
                    self.set_reconnecting_flag(id, false);
                }
                Some((TermStatus::Running, _)) => {
                    // Manually restored while waiting: adopt the running
                    // process as the attempt and watch for its hooks.
                    self.reconnects.lock().insert(
                        id,
                        Reconnect {
                            attempt: rc.attempt,
                            at: now,
                            watching: true,
                            manual: rc.manual,
                        },
                    );
                }
                Some((TermStatus::Dead, _)) => {
                    let attempt = rc.attempt + 1;
                    if rc.manual {
                        log::info!(
                            "terminal {id}: ssh reconnect attempt {attempt} (manual, unlimited)"
                        );
                    } else {
                        log::info!(
                            "terminal {id}: ssh reconnect attempt {attempt}/{}",
                            RECONNECT_BACKOFF.len()
                        );
                    }
                    self.reconnects.lock().insert(
                        id,
                        Reconnect {
                            attempt,
                            at: now,
                            watching: true,
                            manual: rc.manual,
                        },
                    );
                    if rc.manual {
                        // In flight: `retrying — attempt N…` (next_s = 0).
                        self.set_retry_progress(id, attempt, 0);
                    }
                    // This pump rides the 250ms journal-fsync tick, and the
                    // launch may run a remote CLI-resume probe (a blocking
                    // sftp leg, 10-25s against an unreachable host — exactly
                    // the situation a reconnect is in). Inline it would stall
                    // EVERY terminal's flush for the whole connect timeout,
                    // so a probe-due launch moves to the probe-launch worker;
                    // the still-dead accounting travels with it.
                    self.probe_aware_launch(id, Some(attempt));
                }
            }
        }
    }

    /// r2-F9: an attempt the pump marked `watching` never actually launched
    /// (its probe_aware_launch lost the `probing` claim to a concurrent
    /// manual restore). Undo the rung: back to waiting at the PREVIOUS
    /// attempt count, so the ladder retries the same step instead of the
    /// watching entry expiring into "supervision stopped". If the coalesced
    /// launch does revive the terminal, the pre hook resolves supervision
    /// before this retry fires; if it was manually restored but hookless,
    /// the pump's Running arm re-adopts it.
    pub(super) fn requeue_reconnect(&self, id: Uuid, attempt: u32) {
        let mut map = self.reconnects.lock();
        if let Some(rc) = map.get_mut(&id) {
            if rc.watching && rc.attempt == attempt {
                let attempts_done = attempt.saturating_sub(1);
                rc.watching = false;
                rc.attempt = attempts_done;
                rc.at = Instant::now()
                    + reconnect_backoff_after(attempts_done, rc.manual)
                        .unwrap_or(RECONNECT_BACKOFF[0]);
                log::info!(
                    "terminal {id}: reconnect attempt {attempt} coalesced away — re-queued"
                );
            }
        }
    }

    /// A token-checked `pre` hook arrived for `id`: if a reconnect
    /// supervision exists, the link is interactively alive again — success.
    /// (Also adopts manual restores: any hooked comeback resolves it.)
    pub(super) fn resolve_reconnect(&self, id: Uuid) {
        let rc = self.reconnects.lock().remove(&id);
        if let Some(rc) = rc {
            log::info!(
                "terminal {id}: ssh reconnected (attempt {})",
                rc.attempt.max(1)
            );
            self.set_reconnecting_flag(id, false);
        }
    }

    /// Is the current supervision (if any) the manual, unlimited ladder?
    /// (probe_aware_launch's synchronous-failure accounting needs it — the
    /// Reconnect fields are private to this module.)
    pub(super) fn reconnect_is_manual(&self, id: Uuid) -> bool {
        self.reconnects.lock().get(&id).map(|r| r.manual).unwrap_or(false)
    }

    /// C2D::CancelReconnect / internal teardown: stop supervising. An
    /// in-flight attempt keeps running (it is a real ssh process). Returns
    /// whether a supervision existed (the ctl Kill path replies Done for a
    /// dead-but-supervised target instead of the misleading `dead` refusal).
    pub(super) fn cancel_reconnect(&self, id: Uuid) -> bool {
        if self.reconnects.lock().remove(&id).is_some() {
            log::info!("terminal {id}: ssh reconnect cancelled");
            self.set_reconnecting_flag(id, false);
            true
        } else {
            false
        }
    }

    /// Flip the Snapshot-visible reconnecting flag, saving + broadcasting
    /// only on change. Dropping the flag also zeroes the manual-retry
    /// progress fields — the SINGLE clear point for every supervision exit
    /// (success, cancel, give-up, auth-wall stop, sleep, delete).
    pub(super) fn set_reconnecting_flag(&self, id: Uuid, on: bool) {
        let changed = {
            let mut state = self.state.lock();
            match state.terminal_mut(id) {
                Some(t)
                    if t.reconnecting != on
                        || (!on && (t.retry_attempt != 0 || t.retry_next_s != 0)) =>
                {
                    t.reconnecting = on;
                    if !on {
                        t.retry_attempt = 0;
                        t.retry_next_s = 0;
                    }
                    state.save_logged("reconnecting flag");
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.broadcast_snapshot();
        }
    }

    /// F1: stamp the manual ladder's Snapshot-visible progress (`retrying —
    /// attempt N · next in Ss`). Change-gated like the flag; only the manual
    /// path calls it, so the auto lane keeps its plain "reconnecting…".
    pub(super) fn set_retry_progress(&self, id: Uuid, attempt: u32, next_s: u32) {
        let changed = {
            let mut state = self.state.lock();
            match state.terminal_mut(id) {
                Some(t) if t.retry_attempt != attempt || t.retry_next_s != next_s => {
                    t.retry_attempt = attempt;
                    t.retry_next_s = next_s;
                    state.save_logged("retry progress");
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.broadcast_snapshot();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SSH auto-reconnect: the backoff table (2s/10s/30s then give up for
    /// the AUTO lane; 30s-ceiling UNLIMITED for the MANUAL lane — F1, the
    /// user's "keep retrying until my server is back") and the qualification
    /// truth table. The state machine's transitions ride these pure
    /// functions; the live paths are probes `ssh_reconnect` and
    /// `dead_retry_manual`.
    #[test]
    fn reconnect_backoff_and_qualification() {
        assert_eq!(reconnect_backoff_after(0, false), Some(Duration::from_secs(2)));
        assert_eq!(reconnect_backoff_after(1, false), Some(Duration::from_secs(10)));
        assert_eq!(reconnect_backoff_after(2, false), Some(Duration::from_secs(30)));
        assert_eq!(
            reconnect_backoff_after(3, false),
            None,
            "auto: 3 attempts then give up"
        );

        // F1 — the manual ladder shares the table's ramp, then NEVER gives
        // up: every rung past it is the 30s ceiling.
        assert_eq!(reconnect_backoff_after(0, true), Some(Duration::from_secs(2)));
        assert_eq!(reconnect_backoff_after(1, true), Some(Duration::from_secs(10)));
        assert_eq!(reconnect_backoff_after(2, true), Some(Duration::from_secs(30)));
        assert_eq!(
            reconnect_backoff_after(3, true),
            Some(Duration::from_secs(30)),
            "manual: ceiling, not give-up"
        );
        for n in [4u32, 7, 100, 10_000] {
            assert_eq!(
                reconnect_backoff_after(n, true),
                Some(Duration::from_secs(30)),
                "manual attempt {n} keeps the 30s ceiling — unlimited"
            );
        }

        let q = reconnect_qualifies;
        // The field case: unexpected 255 on a hooked ssh session.
        assert!(q(false, Some(255), true, true, false, true));
        // Signal-killed transports (WSL stand-in kill -9) qualify too.
        assert!(q(false, Some(137), true, true, false, true));
        assert!(q(false, None, true, true, false, true));
        // Every gate flips it off.
        assert!(!q(true, Some(255), true, true, false, true), "deliberate kill");
        assert!(!q(false, Some(0), true, true, false, true), "clean remote exit");
        assert!(!q(false, Some(255), false, true, false, true), "never hooked");
        assert!(!q(false, Some(255), true, false, false, true), "not ssh");
        assert!(!q(false, Some(255), true, true, true, true), "asleep");
        assert!(!q(false, Some(255), true, true, false, false), "opted out");
    }

    /// Fix b — the manual `Retry ▸` gate: the click replaces the auto
    /// witnesses (hooks_were_live / exit code / deliberate / opt-in are
    /// deliberately ABSENT from this signature — a never-hooked timed-out
    /// host is exactly the case it exists for), but ssh-only, plainly-Dead
    /// (asleep is the stronger intent), and idempotent under an existing
    /// supervision.
    #[test]
    fn manual_retry_gate() {
        let m = manual_retry_allowed;
        // The field case: a never-hooked ssh tab dead on connect timeout.
        assert!(m(true, true, false, false));
        assert!(!m(false, true, false, false), "not ssh — Restore covers it");
        assert!(!m(true, false, false, false), "running/never-launched");
        assert!(!m(true, true, true, false), "asleep stays asleep");
        assert!(!m(true, true, false, true), "already supervised (double-click)");
    }
}
