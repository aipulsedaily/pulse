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

/// remote-completion OBSERVABILITY, and why it is shaped this way.
///
/// v0.1.20 shipped this feature with every line it writes at `debug!`. The
/// daemon logs at `info!`, and `TC_LOG_DEBUG` refuses to raise that outside a
/// `TC_DATA_DIR` sandbox — so on a real install the lane was completely
/// unobservable. When the field report came in ("`cd pr<Tab>` does nothing"),
/// the log could not say whether a query had been armed, declined, typed,
/// answered or never asked for at all. That is half the bug.
///
/// So the DECISIVE events are `info!` now: a query typed, a query declined
/// (with the gate's own verdict), a query answered or abandoned. Each is at
/// most one line per Tab that actually reached the shell, and the only
/// repeat-prone one — a decline while the user holds Tab — is deduped per
/// terminal to the FIRST of each (directory, verdict) pair.
///
/// The HOT events (a cache hit, every prefetched listing) stay `debug!`,
/// because they fire per Tab and per `cd` forever. `TC_TRACE_COMPLETION=1`
/// promotes them to `info!` — the house `TC_TRACE_*` shape, and unlike
/// `TC_LOG_DEBUG` it works on an ordinary install, which is where the next
/// field report will come from.
fn comp_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("TC_TRACE_COMPLETION").as_deref() == Ok("1"))
}

/// One hot-path completion event: `debug!` normally, `info!` under
/// `TC_TRACE_COMPLETION=1`. Per Tab and per `cd`, never per byte, so
/// building the message unconditionally costs nothing that matters.
fn comp_log(msg: &str) {
    if comp_trace() {
        log::info!("{msg}");
    } else {
        log::debug!("{msg}");
    }
}

/// The shell must have been output-quiet this long before a query is typed.
/// Unlike `nesthook`, which WAITS for quiescence (it has one chance at a
/// witnessed episode), a Tab that arrives mid-output simply declines: the
/// next one retries, and declining costs the user nothing.
const REQ_MIN_QUIET: Duration = Duration::from_millis(300);

/// After the trigger line is typed, its echo must have been SEEN and then
/// been quiet this long before the payload lines are written — the echo
/// returning is what proves `stty -echo` has run.
///
/// "Seen" is load-bearing and is checked separately (`ReqPhase::AwaitEcho`'s
/// `rtt`): a journal that has not grown is equally consistent with "the echo
/// came and settled" and "the trigger has not arrived yet", and on any link
/// slower than this the second one is the truth.
const REQ_ECHO_QUIET: Duration = Duration::from_millis(150);

/// ...and the payload is written ANYWAY at this deadline. `__tc_cq` is
/// parked in two `read` builtins: abandoning after the trigger would wedge
/// the user's shell, so this must always fire.
///
/// THE WAN HAZARD, and why this one is adaptive. Missing the echo is not a
/// missed completion — it is a PERMANENT one. `comp_send_payload` composes
/// the erase from the mirror, the mirror cannot show a line whose echo has
/// not arrived yet, a mirror that cannot vouch means nothing is erased, and
/// one visible ` __tc_cq` stands the query lane down for the whole spawn. So
/// on a link slower than this deadline, the FIRST Tab retires the lane and
/// every later one is declined `StoodDown` — "it still does nothing", for
/// the rest of the session, from a single press.
///
/// The floor is therefore well clear of any plausible WAN round trip on its
/// own (it was a flat 1500ms, which a 1s-RTT host would lose), and the budget
/// grows from there with the link actually measured on this channel. Nothing
/// slows down for a fast shell: the normal exit is `REQ_ECHO_QUIET`, ~200ms
/// in, and this deadline is only ever reached when the echo does NOT come.
const REQ_ECHO_DEADLINE_MIN: Duration = Duration::from_millis(2500);
const REQ_ECHO_DEADLINE_MAX: Duration = Duration::from_secs(12);

/// How long the shell has to answer before the query is declared lost.
///
/// The floor is the constant this used to be, deliberately: a shorter budget
/// on a fast shell would buy nothing and cost something real, because an
/// abandoned query tells the GUI "nothing" and that negative answer is cached
/// for `COMP_DECLINED` (3s) — so a query abandoned a moment too early turns
/// one slow answer into three seconds of silence. Only the ceiling moves.
const REQ_REPLY_MIN: Duration = Duration::from_millis(2500);
const REQ_REPLY_MAX: Duration = Duration::from_secs(20);

/// How many measured round trips each budget allows for. The echo costs one
/// round trip and the reply costs one plus the remote `ls`; eight leaves room
/// for jitter, a loaded remote host and a big directory, and both budgets are
/// clamped anyway.
const RTT_BUDGET_FACTOR: u32 = 8;

/// Turn a measured link round trip into a deadline. `None` (nothing measured
/// on this terminal yet) means the floor — i.e. exactly the behaviour of the
/// constant it replaces.
///
/// Pure, so the whole policy is table-testable without a PTY or a slow link.
pub(crate) fn comp_budget(rtt: Option<Duration>, min: Duration, max: Duration) -> Duration {
    match rtt {
        Some(r) => r.saturating_mul(RTT_BUDGET_FACTOR).clamp(min, max),
        None => min,
    }
}

/// Fold a fresh round-trip sample into the running estimate: rise at once,
/// decay slowly. A timeout budget must react immediately when a link gets
/// worse and must not snap back on one lucky sample, so this is a peak hold
/// with a quarter-weight decay rather than a symmetric average.
pub(crate) fn comp_rtt_fold(prev: Option<Duration>, sample: Duration) -> Duration {
    match prev {
        Some(p) if p > sample => (p.saturating_mul(3) + sample) / 4,
        _ => sample,
    }
}

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
    /// Depths whose shell has actually SENT a `comp`: proof the hook body in
    /// there knows the verb (an older build's body does not), so a query can
    /// expect an answer rather than silence.
    ///
    /// Per DEPTH, and retained when a deeper world collapses, for the same
    /// reason `shells` is: a shell that proved itself once does not become
    /// incapable because the user ran `sudo su` and came back out. A single
    /// terminal-wide bool did exactly that — `rebase` cleared it on the way
    /// back down, and the prefetch that would re-prove it only fires on a cwd
    /// CHANGE, which returning to an unchanged `$PWD` is not. The query lane
    /// then stayed `NoCapability` for the rest of the session (rig-observed,
    /// depth 2 → depth 1, pinned by `capability_survives_a_nested_round_trip`).
    capable: HashSet<usize>,
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
    /// The last (directory, verdict) a query was declined for. Holding Tab in
    /// a directory the lane cannot answer would otherwise write the same
    /// `info!` line every few hundred milliseconds; the first one is the one
    /// that explains the failure.
    last_decline: Option<(String, CompGate)>,
    /// The link, as MEASURED on this very channel: how long the hooked shell
    /// took to echo the trigger line back (trigger written → the first byte
    /// of its echo lands in the journal). That is one round trip through the
    /// pty, the transport and the remote tty — the same path the answer has
    /// to come back along — so it is the honest baseline for both deadlines.
    ///
    /// Observed per query and folded by `comp_rtt_fold`. Kept across depth
    /// changes (a `sudo su` does not move the host) and cleared on a new
    /// spawn (which may be a different host entirely).
    echo_rtt: Option<Duration>,
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
        /// How long the FIRST byte of the echo took to come back — one
        /// measured round trip of this link, taken in band on the very query
        /// that then has to budget for it.
        rtt: Option<Duration>,
    },
    /// The payload was written; waiting for the `comp` answer, with the
    /// budget this link earned (`comp_budget`) carried along so the pump does
    /// not have to re-derive it on every tick.
    AwaitReply { sent: Instant, budget: Duration },
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
            self.capable.clear();
            self.stood_down = false;
            self.dirty = false;
            self.req = None;
            self.last_decline = None;
            // A new spawn may be a different host entirely, so the link has
            // to be measured again. A DEPTH change does not touch it: a
            // `sudo su` is the same machine over the same transport.
            self.echo_rtt = None;
            return;
        }
        if self.depth != depth {
            self.depth = depth;
            // Listings do NOT survive: the world they describe was being
            // changed by another shell. Capability and the shell's identity
            // DO, for every scope at or outside the new one — those are facts
            // about hook bodies that are still running.
            self.dirs.clear();
            self.shells.retain(|d, _| *d <= depth);
            self.capable.retain(|d| *d <= depth);
            self.req = None;
            self.last_decline = None;
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

    /// The shell at the CURRENT depth has proved it speaks `comp`.
    fn capable_here(&self) -> bool {
        self.capable.contains(&self.depth)
    }

    /// Who, if anyone, is owed this listing as the answer to their query —
    /// and how long they waited for it.
    ///
    /// This is a SAFETY boundary, not a convenience. A listing answers a
    /// query only while that query is still in flight AND its payload has
    /// actually gone out. Once `comp_cancel` has taken the request (the
    /// budget ran out, the terminal went away) or `comp_on_input` has dropped
    /// the waiting half (the user typed, submitted, or opened Ctrl-R), there
    /// is no target: a late listing is filed in the cache and sent to NOBODY,
    /// so it can never be applied to a draft that has moved on. A request
    /// still in `AwaitEcho` is not a target either — its payload is not out,
    /// so this listing is a prefetch, not its answer.
    ///
    /// The other two halves of the same property live elsewhere and are
    /// already pinned: the cache is keyed by (spawn generation, hook depth)
    /// so an answer can never be served into a later world (`cache_valid`),
    /// and a shell whose token was rotated away cannot be heard at all
    /// (`BlockStore::classify_token`).
    fn take_reply_target(
        &mut self,
        now: Instant,
    ) -> Option<(Weak<ClientConn>, String, Duration)> {
        let (client, asked, waited) = match self.req.as_ref()?.phase {
            ReqPhase::AwaitReply { sent, .. } => {
                let r = self.req.as_ref()?;
                (r.client.clone(), r.dir.clone(), now.duration_since(sent))
            }
            ReqPhase::AwaitEcho { .. } => return None,
        };
        self.req = None;
        Some((client, asked, waited))
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
            st.capable.insert(depth);
            st.insert(dir.to_string(), listing.clone(), now);
            // The in-flight query is resolved by the ANSWER, not by the path
            // it asked for: the shell reports what it actually listed, and a
            // `~`/relative request resolves to something the GUI never spelled.
            let reply = st.take_reply_target(now);
            // File it under the SPELLING that was asked for as well, when the
            // shell resolved it to something else (`~`, a relative path). The
            // next identical ask is then a cache hit instead of a second line
            // typed into the user's shell for an answer already in hand.
            if let Some((_, asked, _)) = &reply {
                if asked != dir {
                    st.insert(asked.clone(), listing.clone(), now);
                }
            }
            (reply, dir.to_string())
        };
        // A listing that ANSWERS a query is decisive (it closes the loop a
        // field report asks about) and rare — one per Tab that reached the
        // shell. An unsolicited one is the prefetch, which fires on every
        // `cd` forever, so it stays hot.
        let msg = format!(
            "terminal {id}: remote completion listed {dir_owned} ({n} entr{}, trunc={trunc})",
            if n == 1 { "y" } else { "ies" }
        );
        if let Some((client, asked, waited)) = reply {
            log::info!(
                "{msg} — answering the query for {asked} after {}ms",
                waited.as_millis()
            );
            self.send_completion(&client, id, &asked, &dir_owned, Some(&listing));
        } else {
            comp_log(&msg);
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
            log::info!(
                "terminal {id}: remote completion request rejected — the directory is empty, \
                 over 1024 bytes, or carries a newline"
            );
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
            comp_log(&format!(
                "terminal {id}: remote completion of {dir} served from the cache \
                 ({} entries, epoch {epoch}, depth {depth})",
                l.entries.len()
            ));
            self.send_completion(&Arc::downgrade(client), id, dir, dir, Some(&l));
            return;
        }
        comp_log(&format!(
            "terminal {id}: remote completion cache miss for {dir} (epoch {epoch}, depth {depth})"
        ));
        let verdict = self.comp_arm(client, id, dir, now);
        if verdict != CompGate::Arm {
            // Decisive AND repeat-prone: a user holding Tab in a directory the
            // lane cannot answer would write this every few hundred
            // milliseconds, so only the FIRST of each (directory, verdict) is
            // recorded. `rebase` forgets it when the world changes, so a
            // verdict that becomes true again is said again.
            let fresh = {
                let mut map = self.completion.lock();
                let st = map.entry(id).or_default();
                let now_pair = (dir.to_string(), verdict);
                let fresh = st.last_decline.as_ref() != Some(&now_pair);
                st.last_decline = Some(now_pair);
                fresh
            };
            if fresh {
                log::info!("terminal {id}: remote completion of {dir} declined ({verdict:?})");
            }
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
                st.capable_here() && hooks_live,
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
                        rtt: None,
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
        // Decisive: the one event that proves the lane reached the user's own
        // shell. One line per Tab that missed the cache and passed the gate.
        log::info!("terminal {id}: remote completion query typed for {dir}");
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
                match map.get(&id) {
                    Some(s) => match s.req.as_ref().map(|r| &r.phase) {
                        Some(&ReqPhase::AwaitEcho {
                            sent,
                            last_len,
                            last_change,
                            rtt,
                        }) => Some(Ok((sent, last_len, last_change, s.echo_rtt, rtt.is_some()))),
                        Some(&ReqPhase::AwaitReply { sent, budget }) => Some(Err((sent, budget))),
                        None => None,
                    },
                    None => None,
                }
            };
            match phase {
                Some(Ok((sent, last_len, last_change, rtt, echoed))) => {
                    let len = self.journal_len(id).unwrap_or(last_len);
                    let (quiet_for, echoed) = if len != last_len {
                        self.comp_rebase_echo(id, len, now);
                        (Duration::ZERO, true)
                    } else {
                        (now.duration_since(last_change), echoed)
                    };
                    // The quiet arm means "the echo came back AND has settled"
                    // — and the first half is not implied by the second. The
                    // journal is also quiet while the trigger is still in
                    // flight, so on any link slower than REQ_ECHO_QUIET the
                    // old unconditional test fired before the echo existed:
                    // the payload went out blind, the mirror had no line to
                    // vouch for, nothing was erased and the lane stood itself
                    // down for the whole spawn. Rig-reproduced at 300ms of
                    // one-way delay — the FIRST Tab retired the lane and every
                    // later one was declined `StoodDown`.
                    //
                    // The deadline arm stays absolute: `__tc_cq` is blocked in
                    // two `read`s, and a shell left waiting is the one outcome
                    // worse than a missing completion. It scales with the link
                    // this terminal has shown us, because reaching it before
                    // the echo lands is what costs the lane.
                    let deadline =
                        comp_budget(rtt, REQ_ECHO_DEADLINE_MIN, REQ_ECHO_DEADLINE_MAX);
                    if now.duration_since(sent) >= deadline
                        || (echoed && quiet_for >= REQ_ECHO_QUIET)
                    {
                        self.comp_send_payload(id);
                    }
                }
                Some(Err((sent, budget))) if now.duration_since(sent) >= budget => {
                    self.comp_cancel(id, "the shell did not answer in time");
                }
                Some(Err(_)) => {}
                None => {}
            }
        }
    }

    /// Output arrived while the trigger's echo was awaited. Besides re-basing
    /// the quiet clock, the FIRST such byte is the link measurement this whole
    /// lane budgets from: the trigger went out at `sent` and has just come
    /// back, which is one round trip through the pty, the transport and the
    /// remote tty.
    fn comp_rebase_echo(&self, id: Uuid, len: u64, now: Instant) {
        let mut map = self.completion.lock();
        let Some(st) = map.get_mut(&id) else { return };
        let Some(r) = st.req.as_mut() else { return };
        let mut sample = None;
        if let ReqPhase::AwaitEcho {
            sent,
            last_len,
            last_change,
            rtt,
        } = &mut r.phase
        {
            *last_len = len;
            *last_change = now;
            if rtt.is_none() {
                let m = now.duration_since(*sent);
                *rtt = Some(m);
                sample = Some(m);
            }
        }
        if let Some(m) = sample {
            st.echo_rtt = Some(comp_rtt_fold(st.echo_rtt, m));
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
            // The answer has to come back along the same path the echo just
            // did, plus the remote `ls`, so the budget is derived from what
            // this link measured moments ago (`comp_budget`) and carried on
            // the phase — the pump then just compares against it.
            let budget = comp_budget(st.echo_rtt, REQ_REPLY_MIN, REQ_REPLY_MAX);
            r.phase = ReqPhase::AwaitReply {
                sent: Instant::now(),
                budget,
            };
            comp_log(&format!(
                "terminal {id}: remote completion payload written for {dir} \
                 (link {}, budget {}ms)",
                match st.echo_rtt {
                    Some(r) => format!("{}ms measured", r.as_millis()),
                    None => "unmeasured".to_string(),
                },
                budget.as_millis()
            ));
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
            // The budget it was given goes in the line: a lane that keeps
            // abandoning on a slow host is only diagnosable if the log says
            // what it was waiting for and for how long it was willing to.
            let budget = match r.phase {
                ReqPhase::AwaitReply { budget, .. } => format!("{}ms budget", budget.as_millis()),
                ReqPhase::AwaitEcho { .. } => "before the payload went out".to_string(),
            };
            log::info!(
                "terminal {id}: remote completion query for {} abandoned after {}ms ({budget}) \
                 — {why}",
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
        st.capable.insert(1);
        st.rebase(1, 0);
        assert!(st.get("/deep", now).is_none(), "the nested world is gone");
        assert!(!st.capable_here(), "depth 0 never proved itself");
        assert!(
            !st.capable.contains(&1),
            "the deeper shell's capability died with it"
        );
        assert!(st.bash_here(), "depth 0 still reports bash");
        // A relaunch wipes the shell map too (a new spawn re-announces).
        st.stood_down = true;
        st.rebase(2, 0);
        assert!(st.shells.is_empty() && !st.stood_down && !st.bash_here());
    }

    /// RIG-FOUND (v0.1.20): `sudo su` on the remote host, then `exit`, and the
    /// query lane was dead for the rest of the session.
    ///
    /// `capable` was one bool for the whole terminal, cleared on every depth
    /// change. Coming back OUT of a nested shell is a depth change, and the
    /// only thing that re-proves capability is a prefetch — which fires on a
    /// cwd CHANGE, and returning to a `$PWD` you never left is not one. So
    /// every later Tab was declined `NoCapability`, silently, forever.
    ///
    /// Capability is a fact about a hook body that is still running, so it
    /// belongs per depth and survives the collapse of anything deeper —
    /// exactly like the `shells` map beside it.
    #[test]
    fn capability_survives_a_nested_round_trip() {
        let mut st = CompState::default();
        // Depth 1: the hooked remote bash speaks, and proves the verb.
        st.rebase(1, 1);
        st.shells.insert(1, "bash".into());
        st.capable.insert(1);
        assert!(st.capable_here() && st.bash_here());

        // `sudo su` → depth 2. Nothing there has spoken yet, so no query is
        // armed into it on a guess.
        st.rebase(1, 2);
        assert!(!st.capable_here(), "an unproven shell must not be typed into");
        st.shells.insert(2, "bash".into());
        st.capable.insert(2);
        assert!(st.capable_here());

        // `exit` → back to depth 1, whose `$PWD` never changed, so no fresh
        // prefetch will arrive. THE REGRESSION: it must still be capable.
        st.rebase(1, 1);
        assert!(
            st.capable_here(),
            "the depth-1 shell proved itself once and is still running"
        );
        assert!(st.bash_here(), "...and its shell identity survives too");
        assert!(
            !st.capable.contains(&2),
            "the root shell is gone; a NEW one must prove itself again"
        );

        // A relaunch is the one thing that wipes it: that shell is dead.
        st.rebase(2, 0);
        assert!(st.capable.is_empty() && st.shells.is_empty());
    }

    /// The latency policy: what the lane tolerates, end of table.
    ///
    /// The shape that matters is "never shorter than the constants it
    /// replaces, longer exactly when the link is measurably slow". A fast
    /// shell keeps today's numbers — it has nothing to gain from a tighter
    /// budget and something to lose, because an abandoned query is cached as
    /// a definitive nothing by the GUI for `COMP_DECLINED`.
    #[test]
    fn budgets_follow_the_measured_link() {
        let ms = Duration::from_millis;
        let reply = |rtt| comp_budget(rtt, REQ_REPLY_MIN, REQ_REPLY_MAX);
        let echo = |rtt| comp_budget(rtt, REQ_ECHO_DEADLINE_MIN, REQ_ECHO_DEADLINE_MAX);

        // Nothing measured yet (the first query of a spawn): the floors, i.e.
        // exactly the constants this replaced.
        assert_eq!(reply(None), REQ_REPLY_MIN);
        assert_eq!(echo(None), REQ_ECHO_DEADLINE_MIN);
        // A local shell / loopback: measured in single-digit ms, so the
        // floors still win and a fast shell abandons on today's schedule.
        assert_eq!(reply(Some(ms(3))), REQ_REPLY_MIN);
        assert_eq!(echo(Some(ms(3))), REQ_ECHO_DEADLINE_MIN);
        // A LAN host, and a transatlantic one: still inside the floors, which
        // is the honest reading — 2.5s was never thin for these.
        assert_eq!(reply(Some(ms(20))), REQ_REPLY_MIN);
        assert_eq!(reply(Some(ms(120))), REQ_REPLY_MIN);
        // Where it starts to matter: a link whose own round trip is a
        // meaningful fraction of the old constant.
        assert_eq!(reply(Some(ms(400))), ms(3200));
        assert_eq!(echo(Some(ms(400))), ms(3200));
        // A bad satellite / congested path: the old flat 1500ms echo deadline
        // would have fired BEFORE the echo arrived and retired the lane for
        // the whole spawn. Now there is room for eight of these round trips.
        assert_eq!(echo(Some(ms(1000))), Duration::from_secs(8));
        assert_eq!(reply(Some(ms(1000))), Duration::from_secs(8));
        // ...and a pathological measurement cannot park the lane forever.
        assert_eq!(echo(Some(Duration::from_secs(60))), REQ_ECHO_DEADLINE_MAX);
        assert_eq!(reply(Some(Duration::from_secs(60))), REQ_REPLY_MAX);

        // The estimator rises at once and decays slowly: a budget must react
        // the moment a link degrades and must not snap back on one good
        // sample.
        assert_eq!(comp_rtt_fold(None, ms(200)), ms(200));
        assert_eq!(comp_rtt_fold(Some(ms(20)), ms(400)), ms(400), "rise is instant");
        assert_eq!(comp_rtt_fold(Some(ms(400)), ms(0)), ms(300), "decay is a quarter");
        // Four good samples later it has mostly forgotten the spike, so a
        // one-off hiccup does not inflate the budget forever.
        let mut r = Some(ms(400));
        for _ in 0..8 {
            r = Some(comp_rtt_fold(r, ms(10)));
        }
        assert!(r.unwrap() < ms(60), "decayed to {:?}", r.unwrap());
    }

    /// A late answer — one that arrives after the lane gave up on it — is
    /// FILED and sent to NOBODY. The three ways it could go wrong are all
    /// shut: no client is told, no draft is touched, and the (epoch, depth)
    /// key keeps it out of a later world.
    #[test]
    fn an_abandoned_query_has_no_one_left_to_answer() {
        let now = Instant::now();
        let mut st = CompState::default();
        st.rebase(1, 0);
        // Nothing in flight at all.
        assert!(st.take_reply_target(now).is_none());

        // In flight, but the payload has not gone out: this listing is a
        // prefetch, not the answer, and must not resolve the query.
        st.req = Some(CompReq {
            dir: "/srv".into(),
            phase: ReqPhase::AwaitEcho {
                sent: now,
                last_len: 0,
                last_change: now,
                rtt: None,
            },
            client: Weak::new(),
            armed: now,
        });
        assert!(st.take_reply_target(now).is_none());
        assert!(st.req.is_some(), "...and the query is still live");

        // Payload out: this one IS the answer, exactly once, with the wait
        // measured.
        let sent = now - Duration::from_millis(900);
        st.req = Some(CompReq {
            dir: "/srv".into(),
            phase: ReqPhase::AwaitReply { sent, budget: REQ_REPLY_MIN },
            client: Weak::new(),
            armed: sent,
        });
        let (_, asked, waited) = st.take_reply_target(now).expect("the query is owed an answer");
        assert_eq!(asked, "/srv");
        assert_eq!(waited, Duration::from_millis(900));
        assert!(
            st.take_reply_target(now).is_none(),
            "a second listing must not be sent to a client that was already answered"
        );

        // THE REGRESSION this pins: the budget ran out, `comp_cancel` took the
        // request and told the asker "nothing" — and then the shell answers.
        // There is no target, so nothing is sent; the listing is only filed.
        st.req = Some(CompReq {
            dir: "/srv".into(),
            phase: ReqPhase::AwaitReply { sent, budget: REQ_REPLY_MIN },
            client: Weak::new(),
            armed: sent,
        });
        st.req = None; // what comp_cancel / comp_on_input leave behind
        assert!(st.take_reply_target(now).is_none());
        // And the listing it carried cannot be served into a later spawn or a
        // collapsed nested world even from the cache.
        st.insert("/srv".into(), CompDir { entries: vec![], trunc: false }, now);
        assert!(st.get("/srv", now).is_some());
        st.rebase(2, 0);
        assert!(st.get("/srv", now).is_none(), "a new spawn never sees it");
        assert_eq!(st.echo_rtt, None, "...and re-measures the link");
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
