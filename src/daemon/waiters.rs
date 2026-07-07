//! P5 wait engine + event subscriptions.
//!
//! `tc wait` / `tc run --wait` register a Waiter here; the existing hook
//! sites resolve them: `on_block_event` (prompt renders, block closes),
//! `on_exit` (exits, dangling closes), post-ingest (output matching), and the
//! 250ms journal-flush tick (timeouts). No new threads, no polling: the hot
//! ingest path pays ONE relaxed atomic load while no waiter exists.
//!
//! Lock discipline: `Core.waiters` and `Core.subs` are LEAF locks, same
//! doctrine as `blocks` — they are only taken with journal/blocks/sessions
//! locks released, and nothing is ever locked while holding them. Resolved
//! waiters are drained OUT of the lock and replied to outside it (RunDone
//! assembly reads the journal, which must never nest inside a leaf).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use uuid::Uuid;

use crate::protocol::{CtlBody, CtlEvent, WaitHit, D2C};
use crate::state::BlockRec;

use super::{frame_bytes, now_ms, ClientConn, Core};

/// Agents never need more; bounds daemon memory against a runaway script.
pub const MAX_WAITERS_PER_CLIENT: usize = 16;
pub const MAX_WAITERS: usize = 256;
/// Stripped-output carry kept between chunks for an OutputMatch waiter.
/// Patterns spanning more than this much stripped text are out of contract.
const TRIM_KEEP: usize = 8 * 1024;
/// Journal bytes per read window when an OutputMatch scans history (the
/// `from_off` registration scan and the post-push recheck both walk the
/// full range in windows of this size).
pub(super) const FROM_OFF_SCAN_CAP: usize = 512 * 1024;

pub struct Waiter {
    /// Weak: a dead client must not pin its connection (entries are dropped
    /// at the next touch — resolution attempt, sweep, or disconnect purge).
    pub client: Weak<ClientConn>,
    pub req_id: u64,
    /// Terminal the wait targets.
    pub id: Uuid,
    /// Wall-clock ms (now_ms domain); swept by the 250ms flush tick.
    pub deadline_ms: u64,
    pub kind: WaiterKind,
}

pub enum WaiterKind {
    /// `run_tail` = Some(tail_bytes): this is a Run composite — reply RunDone
    /// (with the block's output tail) instead of Waited(BlockClosed).
    BlockClose { after_off: u64, run_tail: Option<u32> },
    Prompt,
    Exit,
    Output(OutputWaiter),
}

pub enum Matcher {
    Substring(String),
    /// `regex::bytes` — linear-time engine (a hostile pattern must not wedge
    /// the daemon) that matches raw bytes, so mid-UTF-8 chunk splits are
    /// harmless.
    Regex(regex::bytes::Regex),
}

impl Matcher {
    fn find(&self, hay: &[u8]) -> Option<(usize, usize)> {
        match self {
            Matcher::Substring(s) => {
                memchr::memmem::find(hay, s.as_bytes()).map(|i| (i, i + s.len()))
            }
            Matcher::Regex(r) => r.find(hay).map(|m| (m.start(), m.end())),
        }
    }
}

pub struct OutputWaiter {
    pub matcher: Matcher,
    pub stripper: crate::strip::AnsiStripper,
    /// Stripped bytes, trimmed to the last `TRIM_KEEP` at line boundaries
    /// after every unmatched feed (transiently larger during a feed; ingest
    /// chunks are ≤64KiB).
    buf: Vec<u8>,
    /// Absolute journal offset this waiter has been fed up to (r2-F5). The
    /// registration history scan and the post-push journal recheck advance
    /// it; `feed_output_waiters` clips live chunks against it — so a byte
    /// can be journal-fed and live-fed in any interleaving without ever
    /// being matched twice or skipped.
    pub(super) fed_to: u64,
}

impl OutputWaiter {
    pub fn new(matcher: Matcher) -> Self {
        Self {
            matcher,
            stripper: crate::strip::AnsiStripper::default(),
            buf: Vec::new(),
            fed_to: 0,
        }
    }

    /// Feed raw (unstripped) stream bytes; `Some(line)` when the pattern
    /// matched — the full stripped line containing the match.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        self.stripper.feed_bytes(bytes, &mut self.buf);
        if let Some((m0, m1)) = self.matcher.find(&self.buf) {
            let start = self.buf[..m0]
                .iter()
                .rposition(|&b| b == b'\n')
                .map(|p| p + 1)
                .unwrap_or(0);
            let end = self.buf[m1..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| m1 + p)
                .unwrap_or(self.buf.len());
            let line = String::from_utf8_lossy(&self.buf[start..end])
                .trim_end_matches('\r')
                .to_string();
            return Some(line);
        }
        if self.buf.len() > TRIM_KEEP {
            let floor = self.buf.len() - TRIM_KEEP;
            // Keep line boundaries: advance the cut to just past the next \n
            // (or take the hard floor if the tail is one giant line).
            let cut = memchr::memchr(b'\n', &self.buf[floor..])
                .map(|p| floor + p + 1)
                .unwrap_or(floor);
            self.buf.drain(..cut);
        }
        None
    }
}

pub struct Sub {
    pub client: Weak<ClientConn>,
    pub req_id: u64,
    /// None = all terminals.
    pub ids: Option<Vec<Uuid>>,
    /// EV_* bitflags.
    pub kinds: u32,
}

/// The earliest CLOSED record at or after `after_off` — the BlockClose
/// resolution rule, shared by registration-time immediate checks and the
/// live hook site.
pub fn first_close_at_or_after(recs: &[BlockRec], after_off: u64) -> Option<&BlockRec> {
    recs.iter()
        .filter(|r| r.start_off >= after_off && r.end_off.is_some())
        .min_by_key(|r| r.start_off)
}

impl Core {
    /// Push a waiter, enforcing both caps. The caller has already run the
    /// immediate-resolution checks.
    pub(super) fn push_waiter(&self, w: Waiter) -> Result<(), &'static str> {
        let mut ws = self.waiters.lock();
        if ws.len() >= MAX_WAITERS {
            return Err("wait_limit");
        }
        let mine = ws
            .iter()
            .filter(|x| x.client.ptr_eq(&w.client))
            .count();
        if mine >= MAX_WAITERS_PER_CLIENT {
            return Err("wait_limit");
        }
        ws.push(w);
        self.waiter_count.store(ws.len(), Ordering::Relaxed);
        Ok(())
    }

    /// Send one D2C::Ctl frame to a (possibly gone) waiter's client.
    fn waiter_reply(&self, client: &Weak<ClientConn>, req_id: u64, body: CtlBody) {
        if let Some(c) = client.upgrade() {
            if let Some(f) = frame_bytes(&D2C::Ctl { req_id, body }) {
                c.enqueue(&f);
            }
        }
    }

    /// L-10: close the check→push TOCTOU. The registration-time immediate
    /// check and `push_waiter` share no lock; a resolution firing between
    /// them ran its resolve pass BEFORE this waiter was in the list, parking
    /// it until timeout. After the push, re-check the condition — if it is
    /// now true, claim the waiter back by (client, req_id) and reply. A
    /// claim miss means the live resolve path already handled it (never
    /// double-reply).
    pub(super) fn recheck_block_close_after_push(
        &self,
        client: &Arc<ClientConn>,
        req_id: u64,
        id: Uuid,
        after_off: u64,
    ) {
        let hit = {
            let map = self.blocks.lock();
            map.get(&id)
                .and_then(|s| first_close_at_or_after(&s.recs, after_off).cloned())
        };
        let Some(rec) = hit else { return };
        let Some(w) = self.claim_waiter(client, req_id) else { return };
        let body = match w.kind {
            WaiterKind::BlockClose {
                run_tail: Some(tail),
                ..
            } => self.run_done_body(id, &rec, tail),
            _ => CtlBody::Waited {
                hit: WaitHit::BlockClosed { rec },
            },
        };
        self.waiter_reply(&w.client, w.req_id, body);
    }

    /// L-10, Prompt flavor: a `pre` landing between the open-block check and
    /// the push resolves nothing for this not-yet-listed waiter — and an idle
    /// shell emits no further `pre` until the user acts, so it would park to
    /// timeout. Re-check "no open block" after the push.
    pub(super) fn recheck_prompt_after_push(
        &self,
        client: &Arc<ClientConn>,
        req_id: u64,
        id: Uuid,
    ) {
        let at_prompt = {
            let map = self.blocks.lock();
            map.get(&id).is_some_and(|s| s.open.is_none())
        };
        if !at_prompt {
            return;
        }
        if let Some(w) = self.claim_waiter(client, req_id) {
            self.waiter_reply(&w.client, w.req_id, CtlBody::Waited { hit: WaitHit::Prompt });
        }
    }

    /// L-10, Exit flavor (r2-F4): kill-then-wait is the MOST-raced pattern —
    /// a terminal exiting between ctl_wait's `running` check and the push
    /// runs `resolve_exit_waiters` before this waiter is listed, and a dead
    /// terminal emits no further on_exit; the wait would park to timeout.
    /// Historical exits keep no code — the same honest None as the
    /// registration-time immediate check.
    pub(super) fn recheck_exit_after_push(
        &self,
        client: &Arc<ClientConn>,
        req_id: u64,
        id: Uuid,
    ) {
        let running = self.state.lock().terminal(id).map(|t| t.status)
            == Some(crate::state::TermStatus::Running);
        if running {
            return;
        }
        if let Some(w) = self.claim_waiter(client, req_id) {
            self.waiter_reply(
                &w.client,
                w.req_id,
                CtlBody::Waited {
                    hit: WaitHit::Exited { code: None },
                },
            );
        }
    }

    /// L-10, Output flavor (r2-F5b): chunks ingested between the
    /// registration snapshot (`fed_to`) and the push were fed to nobody.
    /// Claim the waiter back, feed it the journal gap, and either resolve or
    /// re-list. Loops because bytes can land while the waiter is claimed
    /// (they are fed to nobody too) — each pass consumes the gap and the
    /// loop ends the first time a claim window sees no growth, or the live
    /// path wins the claim race (both `fed_to`-clipped, so no byte is ever
    /// double-fed). Lock discipline: waiters and journal are never held
    /// together.
    pub(super) fn recheck_output_after_push(
        &self,
        client: &Arc<ClientConn>,
        req_id: u64,
        id: Uuid,
    ) {
        let Ok(journal) = self.journal(id) else { return };
        loop {
            let Some(mut w) = self.claim_waiter(client, req_id) else {
                return; // resolved (or purged) by the live path
            };
            let WaiterKind::Output(ow) = &mut w.kind else {
                let _ = self.push_waiter(w);
                return;
            };
            let mut hit = None;
            {
                if journal.lock().absolute_len() <= ow.fed_to {
                    if self.push_waiter(w).is_err() {
                        self.waiter_reply(
                            &Arc::downgrade(client),
                            req_id,
                            CtlBody::Err {
                                code: "wait_limit".into(),
                                msg: "too many pending waits".into(),
                            },
                        );
                    }
                    return;
                }
                // Windows are READ under the journal lock but fed (stripper +
                // regex) outside it, so a large gap never stalls this
                // terminal's ingest (r3-F7). The waiter is CLAIMED here — the
                // live path can't feed it concurrently — and the head is
                // re-read per window, so growth during the scan is consumed
                // before the push-back (which the outer claim/recheck loop
                // and the live feed then cover, `fed_to`-clipped as ever).
                let mut from = ow.fed_to;
                loop {
                    let chunk = {
                        let j = journal.lock();
                        let head = j.absolute_len();
                        if from >= head {
                            break;
                        }
                        let (chunk, _) = j.read_range(from, head, FROM_OFF_SCAN_CAP);
                        chunk
                    };
                    if chunk.is_empty() {
                        break;
                    }
                    from += chunk.len() as u64;
                    ow.fed_to = from;
                    if let Some(line) = ow.feed(&chunk) {
                        hit = Some((line, from));
                        break;
                    }
                }
            }
            if let Some((line, at_off)) = hit {
                self.waiter_reply(
                    &w.client,
                    w.req_id,
                    CtlBody::Waited {
                        hit: WaitHit::Output { line, at_off },
                    },
                );
                return;
            }
            if self.push_waiter(w).is_err() {
                self.waiter_reply(
                    &Arc::downgrade(client),
                    req_id,
                    CtlBody::Err {
                        code: "wait_limit".into(),
                        msg: "too many pending waits".into(),
                    },
                );
                return;
            }
        }
    }

    /// Remove and return this client's parked waiter with `req_id`, if any.
    fn claim_waiter(&self, client: &Arc<ClientConn>, req_id: u64) -> Option<Waiter> {
        let key = Arc::downgrade(client);
        let mut ws = self.waiters.lock();
        let pos = ws
            .iter()
            .position(|w| w.req_id == req_id && w.client.ptr_eq(&key))?;
        let w = ws.remove(pos);
        self.waiter_count.store(ws.len(), Ordering::Relaxed);
        Some(w)
    }

    /// Hook site: a token-checked `pre` rendered a prompt for `id` — resolve
    /// its Prompt waiters. Runs with no other lock held.
    pub(super) fn resolve_prompt_waiters(&self, id: Uuid) {
        let hits: Vec<Waiter> = {
            let mut ws = self.waiters.lock();
            let mut out = Vec::new();
            let mut i = 0;
            while i < ws.len() {
                if ws[i].id == id && matches!(ws[i].kind, WaiterKind::Prompt) {
                    out.push(ws.remove(i));
                } else {
                    i += 1;
                }
            }
            self.waiter_count.store(ws.len(), Ordering::Relaxed);
            out
        };
        for w in hits {
            self.waiter_reply(&w.client, w.req_id, CtlBody::Waited { hit: WaitHit::Prompt });
        }
    }

    /// Hook site: records for `id` just CLOSED (a `pre`, an exec-closes-
    /// dangling, or on_exit's close_dangling). Resolves BlockClose waiters —
    /// plain ones get Waited(BlockClosed), Run composites get RunDone built
    /// from the journal (outside the waiters lock; leaf discipline).
    pub(super) fn resolve_block_close(&self, id: Uuid, closed: &[BlockRec]) {
        if closed.is_empty() || self.waiter_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let hits: Vec<(Waiter, BlockRec)> = {
            let mut ws = self.waiters.lock();
            let mut out = Vec::new();
            let mut i = 0;
            while i < ws.len() {
                let hit = match &ws[i].kind {
                    WaiterKind::BlockClose { after_off, .. } if ws[i].id == id => {
                        first_close_at_or_after(closed, *after_off).cloned()
                    }
                    _ => None,
                };
                match hit {
                    Some(rec) => out.push((ws.remove(i), rec)),
                    None => i += 1,
                }
            }
            self.waiter_count.store(ws.len(), Ordering::Relaxed);
            out
        };
        for (w, rec) in hits {
            let body = match w.kind {
                WaiterKind::BlockClose {
                    run_tail: Some(tail),
                    ..
                } => self.run_done_body(id, &rec, tail),
                _ => CtlBody::Waited {
                    hit: WaitHit::BlockClosed { rec },
                },
            };
            self.waiter_reply(&w.client, w.req_id, body);
        }
    }

    /// SLEEP S11: a sleep is tearing this terminal down — every NON-Exit
    /// waiter for the id fails with the structured `code` ("asleep") BEFORE
    /// the kill, so the caller learns the CAUSE, not the mechanism ("exited"
    /// would be technically true once on_exit runs, but "your condition can
    /// only resolve after an explicit wake, unbounded" is the honest story).
    /// Exit waiters stay parked: the process genuinely exits moments later
    /// and on_exit resolves them truthfully. Factored from the failure half
    /// of `resolve_exit_waiters` — same drain-outside-the-lock discipline.
    pub(super) fn fail_waiters_for(&self, id: Uuid, code: &str, msg: &str) {
        if self.waiter_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let hits: Vec<Waiter> = {
            let mut ws = self.waiters.lock();
            let mut out = Vec::new();
            let mut i = 0;
            while i < ws.len() {
                if ws[i].id == id && sleep_fails_kind(&ws[i].kind) {
                    out.push(ws.remove(i));
                } else {
                    i += 1;
                }
            }
            self.waiter_count.store(ws.len(), Ordering::Relaxed);
            out
        };
        for w in hits {
            self.waiter_reply(
                &w.client,
                w.req_id,
                CtlBody::Err {
                    code: code.into(),
                    msg: msg.into(),
                },
            );
        }
    }

    /// Hook site: the session for `id` exited. Exit waiters resolve; every
    /// other waiter for the id fails with "exited" — its condition can no
    /// longer occur (Prompt/Output need a live shell; a dead session can't
    /// close the block a Run was waiting for — dangling closes were already
    /// resolved by the caller), and hanging to timeout wastes the agent's
    /// time.
    pub(super) fn resolve_exit_waiters(&self, id: Uuid, code: Option<u32>) {
        if self.waiter_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let hits: Vec<Waiter> = {
            let mut ws = self.waiters.lock();
            let mut out = Vec::new();
            let mut i = 0;
            while i < ws.len() {
                if ws[i].id == id {
                    out.push(ws.remove(i));
                } else {
                    i += 1;
                }
            }
            self.waiter_count.store(ws.len(), Ordering::Relaxed);
            out
        };
        for w in hits {
            let body = match w.kind {
                WaiterKind::Exit => CtlBody::Waited {
                    hit: WaitHit::Exited { code },
                },
                _ => CtlBody::Err {
                    code: "exited".into(),
                    msg: "the session exited before the condition could occur".into(),
                },
            };
            self.waiter_reply(&w.client, w.req_id, body);
        }
    }

    /// Hot path (post-ingest, journal lock released): feed one raw output
    /// chunk to this terminal's Output waiters. The caller has already gated
    /// on `waiter_count > 0`. NOTE `at_off` in the reply is the END of the
    /// chunk containing the match, not the match position itself — it is a
    /// resume offset for the next wait, nothing finer.
    pub(super) fn feed_output_waiters(&self, id: Uuid, bytes: &[u8], chunk_off: u64) {
        let at_off = chunk_off + bytes.len() as u64;
        let hits: Vec<(Waiter, String)> = {
            let mut ws = self.waiters.lock();
            let mut out = Vec::new();
            let mut i = 0;
            while i < ws.len() {
                let w = &mut ws[i];
                let line = match (&mut w.kind, w.id == id) {
                    (WaiterKind::Output(ow), true) if at_off > ow.fed_to => {
                        // Clip against what the journal-side scans already
                        // fed (r2-F5): never match the same byte twice.
                        let skip = ow.fed_to.saturating_sub(chunk_off) as usize;
                        ow.fed_to = at_off;
                        ow.feed(&bytes[skip..])
                    }
                    _ => None,
                };
                match line {
                    Some(line) => out.push((ws.remove(i), line)),
                    None => i += 1,
                }
            }
            self.waiter_count.store(ws.len(), Ordering::Relaxed);
            out
        };
        for (w, line) in hits {
            self.waiter_reply(
                &w.client,
                w.req_id,
                CtlBody::Waited {
                    hit: WaitHit::Output { line, at_off },
                },
            );
        }
    }

    /// Timeout sweep, riding the existing 250ms flush tick (gated by the
    /// caller on `waiter_count > 0`). Dead-client entries are dropped
    /// silently; expired ones get Err "timeout".
    pub(super) fn expire_waiters(&self, now: u64) {
        let expired: Vec<Waiter> = {
            let mut ws = self.waiters.lock();
            let mut out = Vec::new();
            let mut i = 0;
            while i < ws.len() {
                if ws[i].client.strong_count() == 0 {
                    ws.remove(i);
                } else if ws[i].deadline_ms <= now {
                    out.push(ws.remove(i));
                } else {
                    i += 1;
                }
            }
            self.waiter_count.store(ws.len(), Ordering::Relaxed);
            out
        };
        for w in expired {
            self.waiter_reply(
                &w.client,
                w.req_id,
                CtlBody::Err {
                    code: "timeout".into(),
                    msg: "condition did not occur within the timeout".into(),
                },
            );
        }
    }

    /// Disconnect purge: drop this client's waiters and subscriptions so a
    /// gone controller pins nothing.
    pub(super) fn purge_client(&self, client: &Arc<ClientConn>) {
        {
            let mut ws = self.waiters.lock();
            ws.retain(|w| !w.client.ptr_eq(&Arc::downgrade(client)) && w.client.strong_count() > 0);
            self.waiter_count.store(ws.len(), Ordering::Relaxed);
        }
        {
            let mut ss = self.subs.lock();
            ss.retain(|s| !s.client.ptr_eq(&Arc::downgrade(client)) && s.client.strong_count() > 0);
            self.sub_count.store(ss.len(), Ordering::Relaxed);
        }
    }

    /// Register a subscription (already scope-checked). Reply-first ordering
    /// is the caller's job.
    pub(super) fn push_sub(&self, s: Sub) {
        let mut ss = self.subs.lock();
        ss.push(s);
        self.sub_count.store(ss.len(), Ordering::Relaxed);
    }

    pub(super) fn remove_sub(&self, client: &Arc<ClientConn>, req_id: u64) {
        let mut ss = self.subs.lock();
        ss.retain(|s| !(s.req_id == req_id && s.client.ptr_eq(&Arc::downgrade(client))));
        self.sub_count.store(ss.len(), Ordering::Relaxed);
    }

    /// Emit one controller event to matching subscribers. `id` = the terminal
    /// it concerns (None = global, matches every subscription). Costs one
    /// relaxed load when nobody subscribed.
    pub(super) fn emit_event(&self, id: Option<Uuid>, bit: u32, ev: &CtlEvent) {
        if self.sub_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let targets: Vec<(Arc<ClientConn>, u64)> = {
            let mut ss = self.subs.lock();
            ss.retain(|s| s.client.strong_count() > 0);
            self.sub_count.store(ss.len(), Ordering::Relaxed);
            ss.iter()
                .filter(|s| s.kinds & bit != 0)
                .filter(|s| match (&s.ids, id) {
                    (Some(ids), Some(tid)) => ids.contains(&tid),
                    _ => true,
                })
                .filter_map(|s| s.client.upgrade().map(|c| (c, s.req_id)))
                .collect()
        };
        for (c, req_id) in targets {
            if let Some(f) = frame_bytes(&D2C::Ctl {
                req_id,
                body: CtlBody::Event { ev: ev.clone() },
            }) {
                c.enqueue(&f);
            }
        }
    }

    /// RunDone assembly: the closed record's journal range, stripped, cut to
    /// the LAST `tail_bytes` (the end of output is where errors live).
    pub(super) fn run_done_body(&self, id: Uuid, rec: &BlockRec, tail_bytes: u32) -> CtlBody {
        let (text, mut truncated) = self.block_text(id, rec);
        let output = if text.len() > tail_bytes as usize {
            truncated = true;
            let mut cut = text.len() - tail_bytes as usize;
            while !text.is_char_boundary(cut) {
                cut += 1;
            }
            text[cut..].to_string()
        } else {
            text
        };
        CtlBody::RunDone {
            exit: rec.exit,
            duration_ms: rec
                .ended_ms
                .unwrap_or(rec.started_ms)
                .saturating_sub(rec.started_ms),
            output,
            truncated,
            start_off: rec.start_off,
        }
    }
}

/// Deadline helper shared by Wait and Run{wait} registration.
pub fn deadline_from(timeout_ms: u64) -> u64 {
    now_ms().saturating_add(timeout_ms)
}

/// SLEEP S11's kind rule, pure so the unit test pins it: everything EXCEPT
/// Exit fails "asleep" at sleep time (Prompt/Output need a live shell; a
/// BlockClose can't happen until an explicit wake — restart-spanning waits
/// are out of contract); Exit waiters resolve naturally via on_exit because
/// the process truthfully exits.
pub(super) fn sleep_fails_kind(kind: &WaiterKind) -> bool {
    !matches!(kind, WaiterKind::Exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(start: u64, end: Option<u64>, exit: Option<i64>) -> BlockRec {
        BlockRec {
            epoch: 1,
            n: 0,
            cmd: "x".into(),
            cwd: None,
            exit,
            started_ms: 100,
            ended_ms: end.map(|_| 350),
            start_off: start,
            end_off: end,
            truncated: false,
        }
    }

    /// §15 waiters_block_close_after_off: recs at 10/90 — after_off 50
    /// resolves on the 90-block only; a dangling close (end set, exit None)
    /// resolves and carries exit None.
    #[test]
    fn waiters_block_close_after_off() {
        let recs = vec![rec(10, Some(50), Some(0)), rec(90, Some(150), Some(3))];
        let hit = first_close_at_or_after(&recs, 50).expect("the 90-block closes");
        assert_eq!(hit.start_off, 90);
        assert_eq!(hit.exit, Some(3));
        assert!(first_close_at_or_after(&recs, 200).is_none());
        // Open records never match (end_off None = still running).
        let with_open = vec![rec(10, Some(50), Some(0)), rec(90, None, None)];
        assert!(first_close_at_or_after(&with_open, 50).is_none());
        // Dangling-closed (close_dangling: end set, exit stays None).
        let dangling = vec![rec(90, Some(120), None)];
        let hit = first_close_at_or_after(&dangling, 50).unwrap();
        assert_eq!(hit.exit, None, "dangling close resolves with exit None");
        // Ties: earliest matching start_off wins.
        let two = vec![rec(90, Some(120), Some(1)), rec(60, Some(80), Some(0))];
        assert_eq!(first_close_at_or_after(&two, 50).unwrap().start_off, 60);
    }

    /// §15 waiters_output_chunk_invariance: identical hit/no-hit at chunk
    /// sizes 1/7/64 (the ModeScanner ethos), a pattern split across a chunk
    /// boundary still matches, and the trim keeps line boundaries.
    #[test]
    fn waiters_output_chunk_invariance() {
        // SGR + an OSC hook interleave the pattern; the stripper must erase
        // them identically at every chunk size.
        let stream: Vec<u8> = b"noise line\r\n\x1b[31mCompi\x1b]7717;t;exec;7b7d\x07ling\x1b[0m serde\r\ntail".to_vec();
        let run = |chunk: usize| -> Option<String> {
            let mut w = OutputWaiter::new(Matcher::Substring("Compiling".into()));
            for c in stream.chunks(chunk) {
                if let Some(line) = w.feed(c) {
                    return Some(line);
                }
            }
            None
        };
        // Hit/no-hit is chunk-invariant; the reported LINE is best-effort up
        // to the match point (tiny chunks can fire before the rest of the
        // line arrives — resolving immediately beats hanging on a line that
        // never gets its newline). Every flavor must contain the pattern.
        assert_eq!(
            run(stream.len()).as_deref(),
            Some("Compiling serde"),
            "whole-stream feed sees the full line"
        );
        for chunk in [1usize, 7, 64] {
            let line = run(chunk).unwrap_or_else(|| panic!("no hit at chunk {chunk}"));
            assert!(
                line.starts_with("Compiling"),
                "chunk {chunk} reported {line:?}"
            );
        }

        // No-hit stays no-hit at every size.
        let miss = |chunk: usize| -> bool {
            let mut w = OutputWaiter::new(Matcher::Substring("NEVER_MATCHES".into()));
            stream.chunks(chunk).any(|c| w.feed(c).is_some())
        };
        assert!(!miss(1) && !miss(7) && !miss(64));

        // Regex flavor, split across a boundary.
        let re = Matcher::Regex(regex::bytes::Regex::new(r"Compiling \w+").unwrap());
        let mut w = OutputWaiter::new(re);
        let (a, b) = stream.split_at(20); // splits inside the pattern region
        let hit = w.feed(a).or_else(|| w.feed(b));
        assert_eq!(hit.as_deref(), Some("Compiling serde"));
    }

    /// SLEEP S11: `fail_waiters_for`'s kind rule — every kind but Exit
    /// fails at sleep time; Exit stays parked for on_exit's truthful
    /// resolution.
    #[test]
    fn sleep_fails_every_kind_but_exit() {
        assert!(sleep_fails_kind(&WaiterKind::Prompt));
        assert!(sleep_fails_kind(&WaiterKind::BlockClose {
            after_off: 0,
            run_tail: None,
        }));
        assert!(sleep_fails_kind(&WaiterKind::BlockClose {
            after_off: 7,
            run_tail: Some(4096),
        }));
        assert!(sleep_fails_kind(&WaiterKind::Output(OutputWaiter::new(
            Matcher::Substring("x".into())
        ))));
        assert!(
            !sleep_fails_kind(&WaiterKind::Exit),
            "Exit waiters resolve via on_exit, never fail 'asleep'"
        );
    }

    /// The 8KiB carry trim cuts at line boundaries, so a match spanning the
    /// trim point (within contract: pattern inside the kept tail) survives.
    #[test]
    fn output_trim_keeps_line_boundaries() {
        let mut w = OutputWaiter::new(Matcher::Substring("THE_END_MARKER".into()));
        // ~40KiB of 100-byte lines, no match anywhere.
        let line = format!("{}\r\n", "x".repeat(98));
        for _ in 0..400 {
            assert!(w.feed(line.as_bytes()).is_none());
        }
        assert!(w.buf.len() <= TRIM_KEEP, "carry exceeded the trim cap");
        assert!(
            w.buf.is_empty() || w.buf[0] == b'x',
            "trim did not land on a line boundary"
        );
        // The pattern arriving split across two feeds still matches against
        // the kept tail.
        assert!(w.feed(b"THE_END_").is_none());
        let hit = w.feed(b"MARKER rest of line\r\n");
        assert!(hit.is_some_and(|l| l.ends_with("THE_END_MARKER rest of line")));
    }
}
