//! remote-completion: directory listings from the shell that is standing in
//! the directory.
//!
//! The gap this closes (field-reported): Tab completion in the composer is a
//! LOCAL `read_dir` resolved against the terminal's tracked cwd
//! (`gui::complete`). That is exactly right for pwsh, cmd and WSL — and
//! completely blind the moment the user types `ssh host` at a Windows prompt
//! and `cd`s around on the far side. The tracked cwd is then a POSIX path on
//! ANOTHER MACHINE: `plan_win` asks for a drive letter, gets `/root`, and
//! answers None, so Tab was a silent no-op in every remote world — a typed
//! `ssh`, a `sudo su` inside it, and an ssh-program terminal alike.
//!
//! No local call can answer that question. The only honest source is the
//! shell sitting on the remote host, and Pulse already talks to it: the hook
//! body it delivers (login rcfile for its own ssh/WSL spawns,
//! `nesthook`'s injection for a shell it witnessed being opened) emits
//! structured hex-JSON on OSC 7717. So completion rides the SAME channel, as
//! one more verb.
//!
//! Two lanes, deliberately asymmetric in cost and risk:
//!
//!  • PREFETCH (`bootstrap::COMP_PREFETCH`) — the prompt hook emits a `comp`
//!    for `$PWD` whenever `$PWD` changed. Nothing is typed into the user's
//!    shell, so there is no echo to erase, no history entry to scrub, no
//!    keystroke race and no gate to get wrong. It costs one `ls` per `cd`,
//!    and it is what makes the field-reported gesture — `cd` on the server,
//!    then Tab — land instantly.
//!
//!  • QUERY (`bootstrap::COMP_READER`) — for any OTHER directory (`cd ../`,
//!    `cd /etc/`, `cd ~/`, `cd src/sub/`) the daemon types eight bytes at a
//!    settled prompt and writes the request + a mirror-verified erase as
//!    invisible payload lines. This is `nesthook`'s proven shape and its
//!    proven gates, and it is bash-only: zsh cannot be typed into without
//!    changing how the USER's own history works (`comp_query_supported`).
//!
//! Everything about the lane is a SIDE channel: it never delays, alters or
//! queues a submission, a pending query is abandoned the moment the user
//! does anything, a late or missing answer degrades to exactly today's
//! behaviour (no candidates), and the GUI never blocks on it.

use super::*;

/// The shell must have been output-quiet this long before a query is typed.
/// Unlike `nesthook`, which WAITS for quiescence (it has one chance at a
/// witnessed episode), a Tab that arrives mid-output simply declines: the
/// next one retries, and declining costs the user nothing.
const REQ_MIN_QUIET: Duration = Duration::from_millis(300);

/// After the trigger line is typed, its echo must be back and quiet this
/// long before the payload lines are written — the echo returning is what
/// proves `stty -echo` has run.
const REQ_ECHO_QUIET: Duration = Duration::from_millis(150);

/// ...and the payload is written ANYWAY at this deadline. `__tc_cq` is
/// parked in two `read` builtins: abandoning after the trigger would wedge
/// the user's shell, so this must always fire.
const REQ_ECHO_DEADLINE: Duration = Duration::from_millis(1500);

/// How long the shell has to answer before the query is declared lost.
const REQ_REPLY_TIMEOUT: Duration = Duration::from_millis(2500);

/// A cached listing is served for this long. The cwd's entry is REPLACED by
/// the prefetch on every `cd`, so this bounds staleness only for other
/// directories and for files the last command created in place — the honest
/// trade against re-querying (or re-`ls`-ing the user's prompt) on every Tab.
const CACHE_TTL: Duration = Duration::from_secs(15);

/// Directories retained per terminal (LRU-free: oldest-inserted dropped).
/// A completion session walks a handful of directories; this is slack.
const CACHE_DIRS: usize = 32;

/// One directory as the remote shell sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct CompDir {
    pub entries: Vec<CompEntry>,
    /// The shell's listing blew `COMP_MAX_BYTES` and was dropped: the only
    /// honest answer is no candidates.
    pub trunc: bool,
}

/// One entry — the WIRE type (`protocol::CompEntry`), reused rather than
/// mirrored: it is what `D2C::Completion` carries, and a second shape here
/// would only ever drift. `dir` comes from `ls -p`'s trailing slash with
/// symlinks resolved (`-L`), so the GUI's dirs-first ordering and
/// trailing-separator behaviour hold exactly as they do locally.
pub use crate::protocol::CompEntry;

/// Parse `ls -A -p -L` output: one name per line, a trailing `/` marking a
/// directory. Blank lines are dropped (a trailing newline, and the two
/// halves a filename containing a literal newline would produce are at least
/// never empty-named). `.`/`..` can't appear — `-A` excludes them.
pub(crate) fn parse_listing(list: &str) -> Vec<CompEntry> {
    list.lines()
        .map(|l| l.trim_end_matches('\r'))
        .map(|l| match l.strip_suffix('/') {
            Some(n) => CompEntry {
                name: n.to_string(),
                dir: true,
            },
            None => CompEntry {
                name: l.to_string(),
                dir: false,
            },
        })
        // A name is never empty, and `/` alone (a bare separator, which no
        // real entry can produce) would complete to nothing at all.
        .filter(|e| !e.name.is_empty())
        .collect()
}

/// Per-terminal remote-completion state (LEAF lock).
#[derive(Default)]
pub(super) struct CompState {
    /// Spawn generation the cache belongs to; a bump wipes it.
    epoch: u32,
    /// Hook scope depth the cache belongs to — 0 = the shell Pulse spawned,
    /// N = the Nth nested shell. A listing taken inside `sudo su` on a
    /// remote host is meaningless once that world is gone, so a shallower
    /// shell speaking drops everything deeper.
    depth: usize,
    /// Cached listings, insertion-ordered for the cap.
    dirs: Vec<CacheEntry>,
    /// depth → the shell's own `init` report of what it is. The query lane
    /// arms only for a depth whose shell said "bash".
    shells: HashMap<usize, String>,
    /// A `comp` has actually arrived from this shell: proof the hook body in
    /// there knows the verb (an older build's body does not), so a query can
    /// expect an answer rather than silence.
    capable: bool,
    /// The query lane is retired for this spawn — a previous query's erase
    /// was declined, and one visible artifact is the most this feature may
    /// ever cost the user.
    stood_down: bool,
    /// Raw half-typed bytes are sitting in the shell's line buffer: our
    /// trigger would be appended to the user's command. Cleared by the next
    /// prompt.
    dirty: bool,
    /// The in-flight query, if any (one at a time, per terminal).
    req: Option<CompReq>,
}

/// One cached listing, stamped with the world it describes. `rebase` already
/// wipes on a generation/depth move; carrying the stamp per entry makes
/// `cache_valid` the single place the rule lives, so a future rebase gap can
/// never serve a listing from a world that is gone.
struct CacheEntry {
    dir: String,
    listing: CompDir,
    at: Instant,
    epoch: u32,
    depth: usize,
}

/// One in-flight query.
struct CompReq {
    /// The directory as REQUESTED (what the GUI asked for; the shell answers
    /// with the absolute path it resolved, which is the cache key).
    dir: String,
    phase: ReqPhase,
    /// Who asked — the reply goes to that client only, like `BlockText`.
    client: Weak<ClientConn>,
    armed: Instant,
}

enum ReqPhase {
    /// The trigger line was typed; waiting for its echo to settle before the
    /// invisible payload lines go out.
    AwaitEcho {
        sent: Instant,
        last_len: u64,
        last_change: Instant,
    },
    /// The payload was written; waiting for the `comp` answer.
    AwaitReply { sent: Instant },
}

/// Why a query was (not) armed — a pure verdict, table-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompGate {
    /// Type the trigger.
    Arm,
    /// The terminal is gone, asleep, or never ran.
    NotRunning,
    /// No hook has ever spoken here (a hookless spawn keeps its behaviour
    /// exactly), or the body in there has no `comp` verb.
    NoCapability,
    /// The foreground shell is not bash (`comp_query_supported`).
    WrongShell,
    /// A previous query's erase was declined — never a second artifact.
    StoodDown,
    /// A command is running: there is no prompt to type at, and our bytes
    /// would be that command's stdin.
    Busy,
    /// A full-screen program owns the terminal.
    AltScreen,
    /// A credential prompt is pending — never type into one.
    Credential,
    /// The cursor row does not read as a shell prompt.
    NotAtPrompt,
    /// Output is still streaming; the next Tab retries.
    NotQuiet,
    /// Half-typed raw bytes are in the shell's line buffer.
    DirtyInput,
    /// A query is already in flight (one at a time).
    InFlight,
}

/// The arm gate (pure, so the whole matrix is table-testable without a PTY).
/// Order is deliberate: the cheap "this can never work" reasons come first,
/// then the "not right now" ones, so a log line names the real cause.
#[allow(clippy::too_many_arguments)]
pub(crate) fn comp_gate(
    running: bool,
    capable: bool,
    bash: bool,
    stood_down: bool,
    in_flight: bool,
    open_block: bool,
    alt: bool,
    credential: bool,
    at_prompt: bool,
    quiet_for: Duration,
    dirty: bool,
) -> CompGate {
    if !running {
        return CompGate::NotRunning;
    }
    if !capable {
        return CompGate::NoCapability;
    }
    if !bash {
        return CompGate::WrongShell;
    }
    if stood_down {
        return CompGate::StoodDown;
    }
    if in_flight {
        return CompGate::InFlight;
    }
    if open_block {
        return CompGate::Busy;
    }
    if alt {
        return CompGate::AltScreen;
    }
    if credential {
        return CompGate::Credential;
    }
    if dirty {
        return CompGate::DirtyInput;
    }
    if quiet_for < REQ_MIN_QUIET {
        return CompGate::NotQuiet;
    }
    if !at_prompt {
        return CompGate::NotAtPrompt;
    }
    CompGate::Arm
}

/// Cache validity (pure): a listing answers for a directory only while it
/// belongs to THIS spawn generation and THIS hook scope depth, and only
/// inside the TTL.
pub(crate) fn cache_valid(
    entry_epoch: u32,
    entry_depth: usize,
    now_epoch: u32,
    now_depth: usize,
    age: Duration,
) -> bool {
    entry_epoch == now_epoch && entry_depth == now_depth && age < CACHE_TTL
}

impl CompState {
    /// Re-base the cache onto (epoch, depth), wiping it when either moved.
    /// A shallower shell speaking means every deeper world is gone — the
    /// same truth `BlockStore::pop_nested_below` acts on for tokens.
    fn rebase(&mut self, epoch: u32, depth: usize) {
        if self.epoch != epoch {
            self.epoch = epoch;
            self.depth = depth;
            self.dirs.clear();
            self.shells.clear();
            self.capable = false;
            self.stood_down = false;
            self.dirty = false;
            self.req = None;
            return;
        }
        if self.depth != depth {
            self.depth = depth;
            self.dirs.clear();
            self.shells.retain(|d, _| *d <= depth);
            self.capable = false;
            self.req = None;
        }
    }

    fn insert(&mut self, dir: String, listing: CompDir, now: Instant) {
        self.dirs.retain(|e| e.dir != dir);
        self.dirs.push(CacheEntry {
            dir,
            listing,
            at: now,
            epoch: self.epoch,
            depth: self.depth,
        });
        while self.dirs.len() > CACHE_DIRS {
            self.dirs.remove(0);
        }
    }

    fn get(&self, dir: &str, now: Instant) -> Option<&CompDir> {
        self.dirs
            .iter()
            .find(|e| e.dir == dir)
            .filter(|e| {
                cache_valid(
                    e.epoch,
                    e.depth,
                    self.epoch,
                    self.depth,
                    now.duration_since(e.at),
                )
            })
            .map(|e| &e.listing)
    }

    fn bash_here(&self) -> bool {
        self.shells
            .get(&self.depth)
            .is_some_and(|s| bootstrap::comp_query_supported(s))
    }
}

impl Core {
    /// A token-checked `init` named the shell at `depth`. Recorded, never
    /// guessed: it is the only witness for whether the query lane may arm.
    pub(super) fn comp_on_init(&self, id: Uuid, epoch: u32, depth: usize, shell: &str) {
        let mut map = self.completion.lock();
        let st = map.entry(id).or_default();
        st.rebase(epoch, depth);
        if !shell.is_empty() {
            st.shells.insert(depth, shell.to_string());
        }
    }

    /// A token-checked `comp` landed: file it and answer whoever is waiting.
    pub(super) fn comp_on_listing(
        &self,
        id: Uuid,
        epoch: u32,
        depth: usize,
        dir: &str,
        trunc: bool,
        list: &str,
    ) {
        let now = Instant::now();
        let entries = parse_listing(list);
        let n = entries.len();
        let listing = CompDir { entries, trunc };
        let (reply, dir_owned) = {
            let mut map = self.completion.lock();
            let st = map.entry(id).or_default();
            st.rebase(epoch, depth);
            st.capable = true;
            st.insert(dir.to_string(), listing.clone(), now);
            // The in-flight query is resolved by the ANSWER, not by the path
            // it asked for: the shell reports what it actually listed, and a
            // `~`/relative request resolves to something the GUI never spelled.
            let reply = match &st.req {
                Some(r) if matches!(r.phase, ReqPhase::AwaitReply { .. }) => {
                    let c = r.client.clone();
                    let asked = r.dir.clone();
                    st.req = None;
                    Some((c, asked))
                }
                _ => None,
            };
            // File it under the SPELLING that was asked for as well, when the
            // shell resolved it to something else (`~`, a relative path). The
            // next identical ask is then a cache hit instead of a second line
            // typed into the user's shell for an answer already in hand.
            if let Some((_, asked)) = &reply {
                if asked != dir {
                    st.insert(asked.clone(), listing.clone(), now);
                }
            }
            (reply, dir.to_string())
        };
        log::debug!(
            "terminal {id}: remote completion listed {dir_owned} ({n} entr{}, trunc={trunc})",
            if n == 1 { "y" } else { "ies" }
        );
        if let Some((client, asked)) = reply {
            self.send_completion(&client, id, &asked, &dir_owned, Some(&listing));
        }
    }

    /// A prompt returned at `depth`: the shell's line buffer is empty again,
    /// and every world deeper than `depth` is gone.
    pub(super) fn comp_on_pre(&self, id: Uuid, epoch: u32, depth: usize) {
        let mut map = self.completion.lock();
        let st = map.entry(id).or_default();
        st.rebase(epoch, depth);
        st.dirty = false;
    }

    /// Input arrived from the user. A SUBMITTED line leaves no buffer behind
    /// (the shell is running it; the next prompt clears `dirty` anyway);
    /// half-typed bytes do, and our trigger must never be appended to them.
    /// Either way an in-flight query is superseded: the user's keystroke wins.
    pub(super) fn comp_on_input(&self, id: Uuid, submitted: bool) {
        let mut map = self.completion.lock();
        let Some(st) = map.get_mut(&id) else { return };
        if !submitted {
            st.dirty = true;
        }
        // Phase 2 must NOT be abandoned — the shell is parked in our `read`
        // builtins and the pump still owes it two lines. Only the waiting
        // half is dropped, so the reply (if it comes) lands in the cache and
        // nothing is sent to a client that has moved on.
        if let Some(r) = &mut st.req {
            if matches!(r.phase, ReqPhase::AwaitReply { .. }) {
                st.req = None;
            } else {
                r.client = Weak::new();
            }
        }
    }

    /// Terminal death / relaunch: drop everything. The next spawn's `init`
    /// rebuilds it from scratch.
    pub(super) fn comp_forget(&self, id: Uuid) {
        self.completion.lock().remove(&id);
        self.comp_quiet.lock().remove(&id);
    }

    /// `C2D::RequestCompletion`: answer from cache, or arm a query.
    pub(super) fn comp_request(
        &self,
        client: &Arc<ClientConn>,
        id: Uuid,
        dir: &str,
    ) {
        // A request is a path the GUI read off the draft — bound it before it
        // is ever typed anywhere.
        if dir.is_empty() || dir.len() > 1024 || dir.contains(['\n', '\r', '\0']) {
            self.send_completion(&Arc::downgrade(client), id, dir, dir, None);
            return;
        }
        let now = Instant::now();
        let (epoch, depth) = self.comp_scope(id);
        let hit = {
            let mut map = self.completion.lock();
            let st = map.entry(id).or_default();
            st.rebase(epoch, depth);
            st.get(dir, now).cloned()
        };
        if let Some(l) = hit {
            self.send_completion(&Arc::downgrade(client), id, dir, dir, Some(&l));
            return;
        }
        let verdict = self.comp_arm(client, id, dir, now);
        if verdict != CompGate::Arm {
            log::debug!("terminal {id}: remote completion of {dir} declined ({verdict:?})");
            // Honest degrade: the GUI learns there is nothing, and stops
            // asking for a short while (its own negative cache).
            self.send_completion(&Arc::downgrade(client), id, dir, dir, None);
        }
    }

    /// The spawn generation + hook scope depth the cache must be keyed to.
    fn comp_scope(&self, id: Uuid) -> (u32, usize) {
        let map = self.blocks.lock();
        match map.get(&id) {
            Some(s) => (s.epoch, s.nested_depth()),
            None => (0, 0),
        }
    }

    /// Run the gate and, on Arm, type the trigger line.
    fn comp_arm(
        &self,
        client: &Arc<ClientConn>,
        id: Uuid,
        dir: &str,
        now: Instant,
    ) -> CompGate {
        let running = {
            let state = self.state.lock();
            state
                .terminal(id)
                .is_some_and(|t| t.status == TermStatus::Running && !t.asleep)
        };
        let (open_block, hooks_live) = {
            let map = self.blocks.lock();
            match map.get(&id) {
                Some(s) => (s.open.is_some(), s.hooks_live),
                None => (false, false),
            }
        };
        let alt = self.terminal_is_alt(id);
        let tail = if alt {
            String::new()
        } else {
            self.last_screen_line(id).unwrap_or_default()
        };
        let credential = reestablish::credential_prompt_line(&tail)
            || matches!(
                crate::gui::composer::detect_auth_prompt(&tail),
                crate::gui::composer::AuthPrompt::HostKey
            );
        let at_prompt = !alt && self.cursor_row_is_prompt(id);
        let quiet_for = self.comp_quiet_for(id, now);
        let verdict = {
            let mut map = self.completion.lock();
            let st = map.entry(id).or_default();
            let v = comp_gate(
                running,
                st.capable && hooks_live,
                st.bash_here(),
                st.stood_down,
                st.req.is_some(),
                open_block,
                alt,
                credential,
                at_prompt,
                quiet_for,
                st.dirty,
            );
            if v == CompGate::Arm {
                st.req = Some(CompReq {
                    dir: dir.to_string(),
                    phase: ReqPhase::AwaitEcho {
                        sent: now,
                        last_len: self.journal_len(id).unwrap_or(0),
                        last_change: now,
                    },
                    client: Arc::downgrade(client),
                    armed: now,
                });
            }
            v
        };
        if verdict != CompGate::Arm {
            return verdict;
        }
        if !self.type_reestablish_line(id, bootstrap::COMP_READER) {
            self.completion.lock().get_mut(&id).map(|s| s.req.take());
            return CompGate::NotRunning;
        }
        log::debug!("terminal {id}: remote completion query typed for {dir}");
        CompGate::Arm
    }

    /// Sample the output-quiescence clock for one terminal, and return how
    /// long it has been quiet.
    ///
    /// `pump_completion` calls this on every tick for every
    /// completion-capable terminal, which is what makes the answer MEAN
    /// something when a request arrives: sampled only on demand, the first
    /// sample would always read "it just changed" and every first query would
    /// be declined as `NotQuiet` (observed on the live probe). A terminal the
    /// clock has not been following yet starts at zero and simply has to wait
    /// one `REQ_MIN_QUIET` — a Tab that lands in that window declines and the
    /// next one works.
    fn comp_quiet_for(&self, id: Uuid, now: Instant) -> Duration {
        let len = self.journal_len(id).unwrap_or(0);
        let mut map = self.comp_quiet.lock();
        let e = map.entry(id).or_insert((len, now));
        if e.0 != len {
            *e = (len, now);
        }
        now.duration_since(e.1)
    }

    /// The query engine, riding the 250ms flush tick beside `pump_nesthook`.
    pub(super) fn pump_completion(self: &Arc<Self>) {
        let now = Instant::now();
        let (tracked, ids): (Vec<Uuid>, Vec<Uuid>) = {
            let map = self.completion.lock();
            if map.is_empty() {
                return;
            }
            (
                map.keys().copied().collect(),
                map.iter()
                    .filter(|(_, s)| s.req.is_some())
                    .map(|(id, _)| *id)
                    .collect(),
            )
        };
        // Keep the quiescence clock running for every completion-capable
        // terminal, so the gate's answer is a measurement rather than a
        // first-sample guess (see `comp_quiet_for`). One in-memory journal
        // length read each, on the 250ms tick `pump_nesthook` already rides.
        for id in &tracked {
            let _ = self.comp_quiet_for(*id, now);
        }
        self.comp_quiet.lock().retain(|id, _| tracked.contains(id));
        for id in ids {
            let running = {
                let state = self.state.lock();
                state
                    .terminal(id)
                    .is_some_and(|t| t.status == TermStatus::Running && !t.asleep)
            };
            if !running {
                self.comp_cancel(id, "terminal is no longer running");
                continue;
            }
            let phase = {
                let map = self.completion.lock();
                match map.get(&id).and_then(|s| s.req.as_ref()) {
                    Some(r) => match r.phase {
                        ReqPhase::AwaitEcho {
                            sent,
                            last_len,
                            last_change,
                        } => Some(Ok((sent, last_len, last_change))),
                        ReqPhase::AwaitReply { sent } => Some(Err(sent)),
                    },
                    None => None,
                }
            };
            match phase {
                Some(Ok((sent, last_len, last_change))) => {
                    let len = self.journal_len(id).unwrap_or(last_len);
                    let (quiet_for, last_change) = if len != last_len {
                        self.comp_rebase_echo(id, len, now);
                        (Duration::ZERO, now)
                    } else {
                        (now.duration_since(last_change), last_change)
                    };
                    let _ = last_change;
                    // The deadline arm is absolute and must fire even while
                    // output is still streaming: `__tc_cq` is blocked in two
                    // `read`s and a shell left waiting is the one outcome
                    // worse than a missing completion.
                    if now.duration_since(sent) >= REQ_ECHO_DEADLINE
                        || quiet_for >= REQ_ECHO_QUIET
                    {
                        self.comp_send_payload(id);
                    }
                }
                Some(Err(sent)) if now.duration_since(sent) >= REQ_REPLY_TIMEOUT => {
                    self.comp_cancel(id, "the shell did not answer in time");
                }
                Some(Err(_)) => {}
                None => {}
            }
        }
    }

    fn comp_rebase_echo(&self, id: Uuid, len: u64, now: Instant) {
        let mut map = self.completion.lock();
        let Some(r) = map.get_mut(&id).and_then(|s| s.req.as_mut()) else {
            return;
        };
        if let ReqPhase::AwaitEcho {
            last_len,
            last_change,
            ..
        } = &mut r.phase
        {
            *last_len = len;
            *last_change = now;
        }
    }

    /// Write the two INVISIBLE payload lines (echo already suppressed by the
    /// trigger) and move to `AwaitReply`.
    ///
    /// The erase blob is composed HERE, never at arm time: it targets the row
    /// the echoed trigger starts on, and that row can only be read off the
    /// mirror once the echo has actually landed. The mirror returning None
    /// means it does not vouch for that region being exactly our line, so
    /// nothing is erased — and the lane then STANDS DOWN for this spawn: one
    /// visible ` __tc_cq` is the most this feature may ever cost.
    fn comp_send_payload(&self, id: Uuid) {
        let armed = {
            let map = self.completion.lock();
            match map.get(&id).and_then(|s| s.req.as_ref()) {
                Some(r) if matches!(r.phase, ReqPhase::AwaitEcho { .. }) => Some(r.dir.clone()),
                _ => None,
            }
        };
        let Some(dir) = armed else { return };
        let erase_row = self.nesthook_erase_row(id, bootstrap::COMP_READER);
        let lines = bootstrap::comp_query_payload(&dir, erase_row);
        {
            let mut map = self.completion.lock();
            let Some(st) = map.get_mut(&id) else { return };
            let Some(r) = st.req.as_mut() else { return };
            if !matches!(r.phase, ReqPhase::AwaitEcho { .. }) {
                return;
            }
            r.phase = ReqPhase::AwaitReply {
                sent: Instant::now(),
            };
            if erase_row.is_none() {
                st.stood_down = true;
            }
        }
        if erase_row.is_none() {
            log::info!(
                "terminal {id}: the completion query line stays visible — the mirror does not \
                 show it exactly where it was typed; no further queries will be typed into this \
                 shell"
            );
        }
        for l in &lines {
            if !self.type_reestablish_line(id, l) {
                self.comp_cancel(id, "session gone before the request could be written");
                return;
            }
        }
    }

    /// Drop an in-flight query, telling the requester there is nothing (so a
    /// Tab never waits on an answer that is not coming).
    fn comp_cancel(&self, id: Uuid, why: &str) {
        let taken = {
            let mut map = self.completion.lock();
            map.get_mut(&id).and_then(|s| s.req.take())
        };
        if let Some(r) = taken {
            log::debug!(
                "terminal {id}: remote completion query for {} abandoned after {}ms — {why}",
                r.dir,
                r.armed.elapsed().as_millis()
            );
            self.send_completion(&r.client, id, &r.dir, &r.dir, None);
        }
    }

    /// Reply to ONE client (never a broadcast — a completion is an answer to
    /// a question that client asked, like `D2C::BlockText`).
    fn send_completion(
        &self,
        client: &Weak<ClientConn>,
        id: Uuid,
        asked: &str,
        dir: &str,
        listing: Option<&CompDir>,
    ) {
        let Some(c) = client.upgrade() else { return };
        if !c.alive.load(Ordering::Relaxed) {
            return;
        }
        let msg = D2C::Completion {
            id,
            asked: asked.to_string(),
            dir: dir.to_string(),
            found: listing.is_some(),
            trunc: listing.is_some_and(|l| l.trunc),
            entries: listing.map(|l| l.entries.clone()).unwrap_or_default(),
        };
        if let Some(f) = frame_bytes(&msg) {
            c.enqueue(&f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> (bool, bool, bool, bool, bool, bool, bool, bool, bool, Duration, bool) {
        // running, capable, bash, stood_down, in_flight, open_block, alt,
        // credential, at_prompt, quiet_for, dirty
        (
            true,
            true,
            true,
            false,
            false,
            false,
            false,
            false,
            true,
            Duration::from_secs(1),
            false,
        )
    }

    fn gate(t: (bool, bool, bool, bool, bool, bool, bool, bool, bool, Duration, bool)) -> CompGate {
        comp_gate(t.0, t.1, t.2, t.3, t.4, t.5, t.6, t.7, t.8, t.9, t.10)
    }

    /// The gating table: every reason a query is NOT typed, and the ONE
    /// shape that is.
    #[test]
    fn comp_gate_table() {
        assert_eq!(gate(ok()), CompGate::Arm);

        let mut t = ok();
        t.0 = false;
        assert_eq!(gate(t), CompGate::NotRunning);
        // A dead terminal is reported as dead even when everything else is
        // also wrong — the cheapest truth wins the log line.
        let mut t = ok();
        t.0 = false;
        t.1 = false;
        t.5 = true;
        assert_eq!(gate(t), CompGate::NotRunning);

        let mut t = ok();
        t.1 = false;
        assert_eq!(gate(t), CompGate::NoCapability);
        let mut t = ok();
        t.2 = false;
        assert_eq!(gate(t), CompGate::WrongShell);
        let mut t = ok();
        t.3 = true;
        assert_eq!(gate(t), CompGate::StoodDown);
        let mut t = ok();
        t.4 = true;
        assert_eq!(gate(t), CompGate::InFlight);
        let mut t = ok();
        t.5 = true;
        assert_eq!(gate(t), CompGate::Busy);
        let mut t = ok();
        t.6 = true;
        assert_eq!(gate(t), CompGate::AltScreen);
        let mut t = ok();
        t.7 = true;
        assert_eq!(gate(t), CompGate::Credential);
        let mut t = ok();
        t.10 = true;
        assert_eq!(gate(t), CompGate::DirtyInput);
        let mut t = ok();
        t.9 = Duration::from_millis(10);
        assert_eq!(gate(t), CompGate::NotQuiet);
        let mut t = ok();
        t.8 = false;
        assert_eq!(gate(t), CompGate::NotAtPrompt);

        // Exactly at the quiet threshold arms; one tick under does not.
        let mut t = ok();
        t.9 = REQ_MIN_QUIET;
        assert_eq!(gate(t), CompGate::Arm);
        t.9 = REQ_MIN_QUIET - Duration::from_millis(1);
        assert_eq!(gate(t), CompGate::NotQuiet);
    }

    /// `ls -A -p -L` → entries, including every shape that can appear.
    #[test]
    fn parse_listing_marks_dirs_and_drops_noise() {
        let got = parse_listing("alpha/\nbravo\n.git/\n.bashrc\na b/\n");
        assert_eq!(
            got,
            vec![
                CompEntry { name: "alpha".into(), dir: true },
                CompEntry { name: "bravo".into(), dir: false },
                CompEntry { name: ".git".into(), dir: true },
                CompEntry { name: ".bashrc".into(), dir: false },
                CompEntry { name: "a b".into(), dir: true },
            ]
        );
        // Empty / whitespace-only payloads yield nothing, never a bogus entry.
        assert!(parse_listing("").is_empty());
        assert!(parse_listing("\n\n").is_empty());
        // A lone "/" cannot name anything — dropped rather than completed to
        // the empty string.
        assert!(parse_listing("/\n").is_empty());
        // CRLF would mean the shell's `ls` emitted \r (it does not), but a
        // stray \r must never become part of a name.
        assert_eq!(parse_listing("x\r\n"), vec![CompEntry { name: "x".into(), dir: false }]);
    }

    /// The cache validity table: generation, scope depth and age.
    #[test]
    fn cache_validity_table() {
        let fresh = Duration::from_secs(1);
        assert!(cache_valid(3, 1, 3, 1, fresh));
        // A relaunch (new epoch) and a collapsed nested world (shallower
        // depth) both invalidate — the listing describes a world that is gone.
        assert!(!cache_valid(3, 1, 4, 1, fresh));
        assert!(!cache_valid(3, 1, 3, 0, fresh));
        assert!(!cache_valid(3, 0, 3, 1, fresh));
        // TTL boundary.
        assert!(cache_valid(3, 1, 3, 1, CACHE_TTL - Duration::from_millis(1)));
        assert!(!cache_valid(3, 1, 3, 1, CACHE_TTL));
    }

    /// Insertion, replacement, the per-terminal cap and the rebase wipes.
    #[test]
    fn cache_insert_replace_cap_and_rebase() {
        let now = Instant::now();
        let mut st = CompState::default();
        st.rebase(1, 0);
        let one = |n: &str| CompDir {
            entries: vec![CompEntry { name: n.into(), dir: false }],
            trunc: false,
        };
        st.insert("/a".into(), one("x"), now);
        assert_eq!(st.get("/a", now).unwrap().entries[0].name, "x");
        // Same directory again REPLACES (a `cd` back must not serve a stale
        // listing from before the last one).
        st.insert("/a".into(), one("y"), now);
        assert_eq!(st.dirs.len(), 1);
        assert_eq!(st.get("/a", now).unwrap().entries[0].name, "y");
        // Expired entries are not served.
        assert!(st.get("/a", now + CACHE_TTL).is_none());
        // The cap drops the oldest insertion, never the newest.
        for i in 0..CACHE_DIRS + 5 {
            st.insert(format!("/d{i}"), one("z"), now);
        }
        assert_eq!(st.dirs.len(), CACHE_DIRS);
        assert!(st.get("/a", now).is_none());
        assert!(st.get(&format!("/d{}", CACHE_DIRS + 4), now).is_some());

        // A deeper shell's listings vanish when a shallower one speaks, and
        // its `init`-reported shell goes with them.
        st.shells.insert(0, "bash".into());
        st.shells.insert(1, "zsh".into());
        st.rebase(1, 1);
        assert!(!st.bash_here(), "depth 1 reported zsh — no query lane there");
        st.insert("/deep".into(), one("q"), now);
        st.capable = true;
        st.rebase(1, 0);
        assert!(st.get("/deep", now).is_none(), "the nested world is gone");
        assert!(!st.capable, "capability is per shell, not per terminal");
        assert!(st.bash_here(), "depth 0 still reports bash");
        // A relaunch wipes the shell map too (a new spawn re-announces).
        st.stood_down = true;
        st.rebase(2, 0);
        assert!(st.shells.is_empty() && !st.stood_down && !st.bash_here());
    }

    /// zsh prefetches but is never typed into; bash gets both lanes.
    #[test]
    fn query_lane_is_bash_only() {
        assert!(bootstrap::comp_query_supported("bash"));
        for s in ["zsh", "sh", "dash", "fish", "", "pwsh"] {
            assert!(!bootstrap::comp_query_supported(s), "{s} must not be typed into");
        }
    }
}
