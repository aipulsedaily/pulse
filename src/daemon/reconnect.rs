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
    /// Attempts already fired (0 = none yet).
    attempt: u8,
    /// Waiting: when the next attempt fires. Watching: when the in-flight
    /// attempt was spawned (hook-arrival deadline base).
    at: Instant,
    /// An attempt is currently in flight (spawned, hooks not yet seen).
    watching: bool,
}

/// SSH reconnect backoff table (seconds after death/failure). Tests pin it.
const RECONNECT_BACKOFF: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
/// How long a spawned reconnect attempt may run without its hooks arming
/// before supervision stops (the attempt itself is LEFT RUNNING — it may be
/// sitting at an interactive auth prompt, which is a usable terminal).
const RECONNECT_HOOK_WINDOW: Duration = Duration::from_secs(30);

/// Delay before the next attempt after `attempts_done` failures; None =
/// exhausted (give up to Dead + the ordinary Restore affordances).
fn reconnect_backoff_after(attempts_done: u8) -> Option<Duration> {
    RECONNECT_BACKOFF.get(attempts_done as usize).copied()
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
                self.advance_reconnect(id, rc.attempt);
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
                at: Instant::now() + RECONNECT_BACKOFF[0],
                watching: false,
            },
        );
        self.set_reconnecting_flag(id, true);
    }

    /// After `attempts_done` failed attempts: schedule the next backoff step
    /// or give up (terminal stays Dead; the ordinary Restore affordances and
    /// boot-restore semantics apply from here).
    pub(super) fn advance_reconnect(&self, id: Uuid, attempts_done: u8) {
        let Some(delay) = reconnect_backoff_after(attempts_done) else {
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
            },
        );
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
                        },
                    );
                }
                Some((TermStatus::Dead, _)) => {
                    let attempt = rc.attempt + 1;
                    log::info!(
                        "terminal {id}: ssh reconnect attempt {attempt}/{}",
                        RECONNECT_BACKOFF.len()
                    );
                    self.reconnects.lock().insert(
                        id,
                        Reconnect {
                            attempt,
                            at: now,
                            watching: true,
                        },
                    );
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
    pub(super) fn requeue_reconnect(&self, id: Uuid, attempt: u8) {
        let mut map = self.reconnects.lock();
        if let Some(rc) = map.get_mut(&id) {
            if rc.watching && rc.attempt == attempt {
                let attempts_done = attempt.saturating_sub(1);
                rc.watching = false;
                rc.attempt = attempts_done;
                rc.at = Instant::now()
                    + reconnect_backoff_after(attempts_done).unwrap_or(RECONNECT_BACKOFF[0]);
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

    /// C2D::CancelReconnect / internal teardown: stop supervising. An
    /// in-flight attempt keeps running (it is a real ssh process).
    pub(super) fn cancel_reconnect(&self, id: Uuid) {
        if self.reconnects.lock().remove(&id).is_some() {
            log::info!("terminal {id}: ssh reconnect cancelled");
            self.set_reconnecting_flag(id, false);
        }
    }

    /// Flip the Snapshot-visible reconnecting flag, saving + broadcasting
    /// only on change.
    pub(super) fn set_reconnecting_flag(&self, id: Uuid, on: bool) {
        let changed = {
            let mut state = self.state.lock();
            match state.terminal_mut(id) {
                Some(t) if t.reconnecting != on => {
                    t.reconnecting = on;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SSH auto-reconnect: the backoff table (2s/10s/30s then give up) and
    /// the qualification truth table. The state machine's transitions ride
    /// these two pure functions; the live path is probe `ssh_reconnect`.
    #[test]
    fn reconnect_backoff_and_qualification() {
        assert_eq!(reconnect_backoff_after(0), Some(Duration::from_secs(2)));
        assert_eq!(reconnect_backoff_after(1), Some(Duration::from_secs(10)));
        assert_eq!(reconnect_backoff_after(2), Some(Duration::from_secs(30)));
        assert_eq!(reconnect_backoff_after(3), None, "3 attempts then give up");

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
}
