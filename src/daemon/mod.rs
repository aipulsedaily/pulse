//! The daemon: a PTY broker that owns every terminal session.
//!
//! The GUI is only a client. Closing it changes nothing for the terminals.
//! All state mutations happen here; clients receive Snapshot broadcasts.

// Public: the GUI reuses `blocks::BlockScanner` + `HookVerb` verbatim for
// offset→row anchoring (P2) — a second scanner implementation would drift.
pub mod anchors;
pub mod blocks;
pub mod claude_registry;
// pub(crate): gui/drop.rs reuses `wsl_mnt_path` (the golden-tested drive →
// /mnt translation) — a second implementation would drift (QOL §4.4).
pub(crate) mod bootstrap;
mod control;
mod ctl_tokens;
pub(crate) mod frame;
mod journal;
pub mod perf;
pub mod procinfo;
mod reconnect;
use reconnect::Reconnect;
mod remote_probe;
pub mod serialize;
mod session;
mod tracker;
mod waiters;

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Processor, Timeout};
use parking_lot::Mutex;
use uuid::Uuid;

use session::EventProxy;

use crate::protocol::{
    read_frame, C2D, CtlEvent, D2C, DaemonInfo, EV_BLOCKS, EV_EXIT, EV_STATE, MAX_FRAME,
    SCOPE_FULL,
};
use crate::state::{
    daemon_info_path, daemon_log_path, data_dir, CliConfidence, SharedState, TermKind, TermStatus,
    TerminalMeta,
};
use journal::Journal;
use session::Session;

/// Depth of each client's outbound queue. A client that stops draining its
/// socket fills this and is dropped; it reconnects and rebuilds losslessly
/// from journal replay. One dedicated thread owns the socket write half, so a
/// wedged client can never block a PTY reader or another client.
const CLIENT_QUEUE_DEPTH: usize = 1024;

/// D14 output-quiet threshold for Cmd-family at-prompt evidence (P6b §5.3):
/// conhost's async text frames arrive on a 16–33ms cadence and the pwsh
/// bootstrap's drain sleeps are 15ms, so 300ms of true silence proves nothing
/// is mid-render (the same constant the shutdown drain trusts).
const CMD_QUIET_MS: u64 = 300;

/// SLEEP S7: the controller busy gate's output-quiet threshold. An idle
/// claude REPL is alt-screen and quiet (the headline sleep target — alt
/// alone never gates, DO-NOT 9); a streaming claude is output-active. 3s
/// rides out prompt-render noise while catching anything mid-stream.
const SLEEP_QUIET_MS: u64 = 3000;

/// Restore pacing, shared by boot auto-restore and folder wake (S17/Q2):
/// N simultaneous `claude --resume` spawns are the login-storm the lanes
/// were built to pace — reusing the constants inherits their measured
/// tuning (perf-wave-3: 20 sessions ≈ 1.9s to all-spawned).
const RESTORE_LANES: usize = 4;
const RESTORE_STAGGER: Duration = Duration::from_millis(300);
/// Folder-wake pacing (r2 boot-perf 4b): wake is INTERACTIVE — the machine
/// is awake and the user is watching — so it paces tighter than login
/// boot-restore (burst wake ×10 measured fine with no pacing at all).
const WAKE_STAGGER: Duration = Duration::from_millis(100);

/// The boot auto-restore filter (S4), pure so the sleep skip is pinned by
/// unit test: `asleep` is the stronger "not until I say so" intent and wins
/// over auto_restore while set (inv. 6 — a terminal asleep at reboot stays
/// asleep after it).
pub(crate) fn should_boot_restore(t: &TerminalMeta) -> bool {
    t.auto_restore && t.launched_once && !t.asleep
}

/// P6b §5.2: a SubmitCommand payload must be ONE non-empty line — cmd
/// executes each line at its own prompt, so a multi-line record would lie
/// (Q2 defers the multi-line ledger), and an empty record has nothing to
/// key history on. Pure so U7 pins it.
pub fn validate_submit_command(cmd: &str) -> Result<(), &'static str> {
    if cmd.contains('\n') || cmd.contains('\r') {
        return Err("multi-line command; cmd runs one line at a time");
    }
    if cmd.trim().is_empty() {
        return Err("empty command");
    }
    Ok(())
}

/// Raw journal bytes read for one C2D::BlockText request. MAX_FRAME is 32 MiB
/// but a clipboard copy of more than ~1 MiB of text has no use and stalls the
/// client queue; the raw cap is higher because ANSI/OSC stripping shrinks it.
const BLOCK_TEXT_RAW_CAP: usize = 4 * 1024 * 1024;
/// Stripped-text cap for the D2C::BlockText reply.
const BLOCK_TEXT_CAP: usize = 1024 * 1024;

/// vte `Timeout` that never engages: with it, DECSET 2026 (synchronized
/// output) never enters vte's deferral path and every byte parses the moment
/// it arrives. The daemon wants exactly that: its mirror Term answers DSR/DA
/// queries (a query inside a sync block would otherwise sit buffered until
/// the block ends — deadlocking an app that awaits the reply before emitting
/// the terminating ESU) and is serialized on Attach (which must see current
/// state, or the attacher misses the in-flight block entirely). Presentation
/// atomicity is the CLIENT's concern: the raw BSU/ESU bytes still fan out
/// untouched, and the GUI's own default-`Processor` defers the present
/// (`TermBackend::pump_sync` enforces vte's 150ms cap).
#[derive(Default)]
pub struct NoSync;

impl Timeout for NoSync {
    fn set_timeout(&mut self, _: Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

/// A VT processor that applies synchronized-output blocks immediately.
pub type ImmediateProcessor = Processor<NoSync>;

pub struct ClientConn {
    tx: SyncSender<Arc<[u8]>>,
    attached: Mutex<HashSet<Uuid>>,
    alive: AtomicBool,
    /// SCOPE_* rights resolved at the handshake (Hello = FULL; HelloCtl =
    /// per its token) and immutable for the connection's lifetime.
    scope: u32,
    /// TC_SESSION_ID of the terminal this controller runs inside, if any —
    /// the recursion guard's target (see control.rs).
    self_session: Option<Uuid>,
}

impl ClientConn {
    /// Non-blocking. A full or disconnected queue marks the client dead; it is
    /// pruned on the next broadcast and rebuilds from replay on reconnect.
    fn enqueue(&self, frame: &Arc<[u8]>) {
        if !self.alive.load(Ordering::Relaxed) {
            return;
        }
        match self.tx.try_send(frame.clone()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.alive.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// Serialize a message into a length-prefixed frame once, so a broadcast pays
/// for encoding a single time and every client shares the same Arc.
/// Validate the PEB CurrentDirectory (0x38) offset on this build by reading our
/// own process's cwd and comparing to the real one. Used by `--probe peb`.
pub fn verify_peb_offset() -> bool {
    procinfo::verify_self_offset()
}

/// Total CPU ms (kernel + user) of `pid`. Used by `--probe flood`.
pub fn process_cpu_ms(pid: u32) -> Option<u64> {
    procinfo::process_cpu_ms(pid)
}

/// Build an `{id, bytes}` D2C frame from a BORROWED payload, skipping the
/// derive path's costs: `bincode::serialize` runs a size pre-pass and then
/// walks a `Vec<u8>` payload byte-by-byte (serde seq), and `frame_bytes`
/// copies the encoding again behind the length prefix — with a `.to_vec()`
/// the old fanout paid FOUR traversals of every output byte. Here the
/// payload is emitted with `serialize_bytes` (one bulk copy) straight into
/// the final frame buffer, and the length prefix is patched in place.
///
/// Wire-format identical to the derive for the same variant: bincode encodes
/// a byte seq and `serialize_bytes` the same way (u64 len + raw bytes), and
/// the caller passes the variant index/name matching D2C — pinned
/// bit-for-bit by `sync_tests::byte_frames_match_derived_encoding` for both
/// users (`output_frame`, `replay_frame`).
fn bytes_frame(
    variant_index: u32,
    variant_name: &'static str,
    id: Uuid,
    bytes: &[u8],
) -> Option<Arc<[u8]>> {
    struct RawBytes<'a>(&'a [u8]);
    impl serde::Serialize for RawBytes<'_> {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(self.0)
        }
    }
    struct FrameRef<'a>(u32, &'static str, Uuid, &'a [u8]);
    impl serde::Serialize for FrameRef<'_> {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeStructVariant;
            let mut sv = s.serialize_struct_variant("D2C", self.0, self.1, 2)?;
            sv.serialize_field("id", &self.2)?;
            sv.serialize_field("bytes", &RawBytes(self.3))?;
            sv.end()
        }
    }
    let mut buf: Vec<u8> = Vec::with_capacity(4 + 4 + 16 + 8 + bytes.len() + 16);
    buf.extend_from_slice(&[0u8; 4]);
    bincode::serialize_into(&mut buf, &FrameRef(variant_index, variant_name, id, bytes)).ok()?;
    let len = (buf.len() - 4) as u64;
    if len > MAX_FRAME as u64 {
        log::error!(
            "dropping oversized outbound frame: {len} bytes > MAX_FRAME {MAX_FRAME} (D2C::{variant_name})"
        );
        return None;
    }
    let len_le = (len as u32).to_le_bytes();
    buf[..4].copy_from_slice(&len_le);
    Some(buf.into())
}

fn output_frame(id: Uuid, bytes: &[u8]) -> Option<Arc<[u8]>> {
    bytes_frame(2, "Output", id, bytes)
}

/// The attach/resync Replay from a borrowed payload — lets the serialized
/// grid move on into the anchor-hint job instead of being cloned for it.
fn replay_frame(id: Uuid, bytes: &[u8]) -> Option<Arc<[u8]>> {
    bytes_frame(1, "Replay", id, bytes)
}

fn frame_bytes<T: serde::Serialize>(msg: &T) -> Option<Arc<[u8]>> {
    let data = bincode::serialize(msg).ok()?;
    if data.len() as u32 > MAX_FRAME {
        // L-6: never drop a frame silently — the realistic producer is a
        // pathological D2C::Replay (wide grid × 2000 history lines × per-cell
        // SGR churn), and the symptom without this line is a permanently
        // blank terminal with no diagnostic anywhere.
        log::error!(
            "dropping oversized outbound frame: {} bytes > MAX_FRAME {} ({})",
            data.len(),
            MAX_FRAME,
            std::any::type_name::<T>(),
        );
        return None;
    }
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(&data);
    Some(buf.into())
}

/// One deferred anchor-hint computation (r2 boot-perf 1). `compute_hints`
/// costs ~23-32ms per hooked 2MB-tail terminal and used to run SERIAL on the
/// single GUI-conn thread — ~500ms of a 20-terminal cold-boot attach cycle,
/// twice that in crash recovery (dead attach + resync). Inputs are owned
/// snapshots taken under the journal lock exactly as before; the GUI already
/// tolerates late-arriving `ReplayAnchors` (per-hint grid verification,
/// re-base by history growth, drop-whole-batch on resize/alt/shrink), so
/// only the frame's position in the client queue changes.
struct HintJob {
    /// Perf-log label ("attach" / "resync") — the boot-measurement greps
    /// key on it.
    label: &'static str,
    id: Uuid,
    tail: Vec<u8>,
    tail_base: u64,
    replay: Vec<u8>,
    recs: Vec<crate::state::BlockRec>,
    cols: u16,
    rows: u16,
    /// Weak: a client gone before the hints are ready just misses them.
    targets: Vec<Weak<ClientConn>>,
}

impl HintJob {
    /// Compute and enqueue (worker thread; a full queue DROPS the job —
    /// see `HintPool::submit`).
    fn run(self) {
        let t0 = perf::on().then(Instant::now);
        let items = anchors::compute_hints(
            &self.tail,
            self.tail_base,
            &self.replay,
            &self.recs,
            self.cols,
            self.rows,
        );
        if let Some(t0) = t0 {
            log::info!(
                "[perf] {} anchors id={} hints={} us={}",
                self.label,
                self.id,
                items.len(),
                t0.elapsed().as_micros()
            );
        }
        if items.is_empty() {
            return;
        }
        let Some(f) = frame_bytes(&D2C::ReplayAnchors { id: self.id, items }) else {
            return;
        };
        for t in &self.targets {
            if let Some(c) = t.upgrade() {
                c.enqueue(&f);
            }
        }
    }
}

/// A queued job holds ~2.5MB of owned inputs; beyond the cap the job is
/// DROPPED and logged (r3-latency 5) — never computed inline on the conn
/// thread. Clients tolerate absent hints (covers degrade to honest raw rows
/// until the next attach resubmits).
const HINT_WORKERS: usize = 3;
const HINT_QUEUE_CAP: usize = 24;

/// What `HintPool::submit` did with a job — returned so the overflow
/// contract is unit-pinned (T3); production callers ignore it.
#[derive(Debug, PartialEq, Eq)]
enum HintSubmit {
    Queued,
    DroppedFull,
    RanInline,
}

struct HintPool {
    tx: SyncSender<HintJob>,
    /// The pool holds the receiver too, so `submit` can never observe
    /// Disconnected while the pool is alive (and tests can build a
    /// worker-less pool to pin the overflow drop — T3).
    _rx: Arc<Mutex<std::sync::mpsc::Receiver<HintJob>>>,
}

impl HintPool {
    fn new() -> Self {
        Self::with_shape(HINT_QUEUE_CAP, HINT_WORKERS)
    }

    fn with_shape(cap: usize, workers: usize) -> Self {
        let (tx, rx) = sync_channel::<HintJob>(cap);
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..workers {
            let rx = rx.clone();
            let _ = std::thread::Builder::new()
                .name(format!("hints-{i}"))
                .spawn(move || loop {
                    // Hold the receiver lock only across the recv itself —
                    // the winner releases it before computing, so idle
                    // workers park on the recv, not behind a computation.
                    let job = rx.lock().recv();
                    match job {
                        Ok(job) => {
                            let _ = catch_unwind(AssertUnwindSafe(|| job.run()));
                        }
                        Err(_) => break, // pool dropped (daemon exit)
                    }
                });
        }
        Self { _rx: rx, tx }
    }

    fn submit(&self, job: HintJob) -> HintSubmit {
        match self.tx.try_send(job) {
            Ok(()) => HintSubmit::Queued,
            // Overflow DROPS the job instead of running it inline on the conn
            // thread (a ≥25-terminal recovery storm can exceed the cap; each
            // inline run put ~25ms between that connection's queued Input
            // frames). Clients fully tolerate absent hints — covers degrade
            // to honest raw rows until the next attach resubmits.
            Err(TrySendError::Full(job)) => {
                log::info!("hint job '{}' for {} dropped: queue full", job.label, job.id);
                HintSubmit::DroppedFull
            }
            // Unreachable while the pool is alive (we hold the receiver);
            // kept as a safety net for process teardown.
            Err(TrySendError::Disconnected(job)) => {
                job.run();
                HintSubmit::RanInline
            }
        }
    }
}

pub struct Core {
    state: Mutex<SharedState>,
    sessions: Mutex<HashMap<Uuid, Session>>,
    journals: Mutex<HashMap<Uuid, Arc<Mutex<Journal>>>>,
    clients: Mutex<Vec<Arc<ClientConn>>>,
    /// Per-terminal block records (Journal Blocks). LEAF lock: nothing else —
    /// journals map, a Journal, state, sessions, clients — is ever locked
    /// while this is held.
    blocks: Mutex<HashMap<Uuid, blocks::BlockStore>>,
    /// Consecutive fast (< 3s) exits per terminal, to warn in the restore marker.
    fast_exits: Mutex<HashMap<Uuid, u32>>,
    /// Wall-clock ms of the last error broadcast (throttle guard).
    last_error_ms: AtomicU64,
    /// P5 wait engine (LEAF lock, blocks doctrine). `waiter_count` mirrors
    /// its len so the hot ingest path pays one relaxed load when idle.
    waiters: Mutex<Vec<waiters::Waiter>>,
    waiter_count: AtomicUsize,
    /// P5 event subscriptions (LEAF lock) + its emission gate.
    subs: Mutex<Vec<waiters::Sub>>,
    sub_count: AtomicUsize,
    /// Scoped controller tokens (LEAF lock; loaded once in run()).
    ctl_tokens: Mutex<ctl_tokens::TokenFile>,
    /// Terminal ids with a `launch()` currently in flight (LEAF lock).
    /// launch() spans a slow spawn (bootstrap write + journal tail read +
    /// CreateProcess, easily 100-500ms) and only flips Dead→Running at the
    /// END — without this guard, boot auto-restore racing a GUI Restore
    /// click or `tc restart` both pass the Dead check, double-spawn, and
    /// the second `sessions.insert` drops the first Session. (The first
    /// session's exit-watcher can no longer tear down the second — on_exit
    /// is gen-stamped, r3-F9 — but the double-spawn itself still wastes a
    /// process and flickers state.) Concurrent launches for one id must
    /// COALESCE.
    launching: Mutex<HashSet<Uuid>>,
    /// P6a hook-based inner-CLI lifecycle (LEAF lock): per terminal, the
    /// (epoch, start_off) key of the block whose exec hook set `inner_cli` —
    /// when THAT record closes, the CLI exited back to a prompt and
    /// `inner_cli` is cleared (the same lifecycle the Win32 process tracker
    /// gives pwsh). Runtime-only: a daemon restart drops it, which is
    /// correct — a dangling open block at restore means the CLI was still
    /// running, so the resume wrapper fires and re-announces itself through
    /// a fresh exec hook.
    cli_blocks: Mutex<HashMap<Uuid, (u32, u64)>>,
    /// Per-terminal spawn/exit wall-clock window of the CURRENT/most-recent
    /// process (LEAF lock; runtime-only). Powers the claude session-id
    /// re-pin at wake: a transcript jsonl BORN inside this window belongs to
    /// this terminal's run (fork-on-resume / `/clear` rotate the id under
    /// the pin; resuming the frozen original replays a stale fork point).
    /// Runtime-only is correct: across a daemon restart the window is
    /// unknowable and the re-pin abstains (never guess).
    spawn_times: Mutex<HashMap<Uuid, (u64, Option<u64>)>>,
    /// Terminals whose next exit is DELIBERATE (kill/restart/delete/sleep):
    /// stamped immediately before `killer.kill()` at every such site and
    /// consumed by on_exit (LEAF lock). An UN-expected ssh death is the
    /// auto-reconnect trigger; without this marker a user-clicked Kill on an
    /// ssh tab would fight the user by reconnecting it.
    expected_exits: Mutex<HashSet<Uuid>>,
    /// SSH auto-reconnect supervisions (LEAF lock): terminal → attempt state.
    /// See `maybe_schedule_reconnect` for the qualification rules and
    /// `pump_reconnects` (250ms flush tick) for the backoff engine.
    reconnects: Mutex<HashMap<Uuid, Reconnect>>,
    /// Remote CLI-resume probe bookkeeping (LEAF locks inside): the §4.6
    /// auth-dead cache + the 30s listing cooldown. Arc so probe worker
    /// threads (M0 snapshot legs) borrow no Core.
    probe_rt: Arc<remote_probe::Runtime>,
    /// Terminal ids with a `probe-launch-<id>` worker in flight (LEAF lock):
    /// coalesces restore double-clicks during the probe window BEFORE
    /// launch()'s own LaunchGuard engages (§6.2). Claimed with an explicit
    /// if/else — NOT `insert(id).then_some(..)`, the LaunchGuard
    /// eager-construction trap.
    probing: Mutex<HashSet<Uuid>>,
    /// Attribution Layer 1 (WSL): the `home` each hook-fed shell reported in
    /// its init hook (LEAF lock; runtime-only — re-reported on every spawn's
    /// init). Keys the `\\wsl$\<distro>\<home>\.claude\sessions` registry
    /// scan; entries drop with the terminal.
    hook_homes: Mutex<HashMap<Uuid, String>>,
    /// Attribution Layer 1 (WSL) scan throttle: per terminal, the earliest
    /// Instant the next `\\wsl$` registry scan may run (LEAF lock). UNC
    /// reads cost ~ms — the activity-gated tracker tick would otherwise
    /// re-scan a streaming claude every 250ms.
    wsl_reg_next: Mutex<HashMap<Uuid, Instant>>,
    /// Attribution: the most recent ACCEPTED self-report per terminal
    /// (sid, wall-clock ms) — hook report / beacon (LEAF lock). The event
    /// hooks fire ~100ms BEFORE claude rewrites its registry file, so a
    /// tracker tick landing in that gap would read the STALE registry and
    /// flap the pin backwards (observed live: report → stale-registry
    /// revert → registry catch-up, three saves in one tick cycle — and a
    /// sleep inside the gap would persist the WRONG pin). A registry
    /// verdict that contradicts a report younger than
    /// `CLAUDE_REPORT_FRESH_MS` is ignored; agreement or age lets the
    /// registry rule again (hook-less claudes keep working).
    claude_reports: Mutex<HashMap<Uuid, (Uuid, u64)>>,
    /// r2-F10: serializes `broadcast_snapshot`'s clone→enqueue span. The
    /// state is cloned under the state lock but enqueued later under the
    /// clients lock; two racing broadcasts could otherwise interleave as
    /// clone(new)/clone(old)…enqueue(new)/enqueue(old) and a stale snapshot
    /// would transiently regress terminal metas GUI-side (apply_snapshot is
    /// wholesale). Taken FIRST, before state/clients — never while any other
    /// lock is held it could nest under.
    snapshot_order: Mutex<()>,
    /// r2 boot-perf 1: anchor-hint worker pool — see `HintJob`.
    hints: HintPool,
}

/// How long an accepted claude self-report outranks a contradicting
/// registry read (the registry-file write lag is ~100ms; generous).
const CLAUDE_REPORT_FRESH_MS: u64 = 5_000;

/// RAII in-flight-launch marker: `try_begin` claims the id or reports another
/// launch already owns it; the claim is released on drop, covering every
/// early-return path of `launch()`.
struct LaunchGuard<'a> {
    set: &'a Mutex<HashSet<Uuid>>,
    id: Uuid,
}

impl<'a> LaunchGuard<'a> {
    fn try_begin(set: &'a Mutex<HashSet<Uuid>>, id: Uuid) -> Option<Self> {
        // NOT `insert(id).then_some(Self{..})`: then_some constructs its
        // value EAGERLY, so the losing path would build a LaunchGuard and
        // immediately drop it — re-locking the mutex inside the still-held
        // lock statement (self-deadlock) and erasing the WINNER's claim.
        if set.lock().insert(id) {
            Some(Self { set, id })
        } else {
            None
        }
    }
}

impl Drop for LaunchGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().remove(&self.id);
    }
}

impl Core {
    fn journal(&self, id: Uuid) -> anyhow::Result<Arc<Mutex<Journal>>> {
        if let Some(j) = self.journals.lock().get(&id) {
            return Ok(j.clone());
        }
        // Miss: do the file IO (open + sidecar rehydrate + the r2-F6
        // reconcile save below) OUTSIDE the map lock — every terminal's
        // ingest calls journal() per chunk, so one terminal's first-open
        // (ms) or reconcile fsync under the map lock would stall the whole
        // fleet's ingest. Double-checked insert at the end keeps one-Arc-
        // per-id semantics; two racing opens of an append-only file are
        // benign (the loser's handle just drops).
        //
        // Never lazily create a journal for a terminal that no longer
        // exists: a killed session's reader thread keeps delivering buffered
        // output after DeleteTerminal; re-opening here would resurrect the
        // deleted file forever. The deletion race is closed by RE-CHECKING
        // state under the map lock at insert time (delete removes state
        // first, then the map entry, then the file).
        anyhow::ensure!(
            self.state.lock().terminal(id).is_some(),
            "terminal {id} deleted"
        );
        // KNOWN RESIDUAL (wave1 F3, LOW — fix here if this code is touched):
        // a delete completing between the state check above and the
        // `block_store_base` call below re-inserts a blocks-map entry for the
        // deleted id (memory-only, until restart) and `Journal::open`'s
        // create(true) leaves an empty orphan `<id>.log` (reaped on the next
        // HEALTHY boot). The double-checked re-ensure below correctly refuses
        // the journals-map insert, so nothing else leaks. The complete fix is
        // on THIS side: re-check state immediately before `block_store_base`
        // (or fold the state check into block_store_base under the blocks
        // lock), mirroring the journals-map double-check.
        //
        // First open since daemon start: rehydrate the block sidecar here (the
        // single Journal construction site) so the journal starts life with
        // the persisted compaction base — block offsets stay aligned across
        // restarts.
        let base = self.block_store_base(id);
        let j = Arc::new(Mutex::new(Journal::open(id, base)?));
        // r2-F6: the compaction rename and the sidecar save are two separate
        // commits — a power cut between them restarts with `base` one
        // compaction behind the file. Records pointing beyond the file's
        // real head are the tell; the honest degrade drops the ledger
        // (journal bytes are intact, only block chrome is lost) instead of
        // minting colliding offsets and returning cross-wired Copy-output.
        let head = j.lock().absolute_len();
        let snap = {
            let mut map = self.blocks.lock();
            map.get_mut(&id)
                .and_then(|s| s.reconcile_with_journal_head(head).then(|| s.clone()))
        };
        if let Some(s) = snap {
            log::warn!(
                "terminal {id}: block sidecar inconsistent with journal head {head} \
                 (crash between compaction and sidecar save?) — block records dropped"
            );
            s.save(id);
        }
        let mut journals = self.journals.lock();
        if let Some(existing) = journals.get(&id) {
            return Ok(existing.clone());
        }
        anyhow::ensure!(
            self.state.lock().terminal(id).is_some(),
            "terminal {id} deleted"
        );
        journals.insert(id, j.clone());
        Ok(j)
    }

    /// The persisted compaction base for `id`, loading the block store from
    /// its sidecar on first touch.
    fn block_store_base(&self, id: Uuid) -> u64 {
        let mut map = self.blocks.lock();
        map.entry(id)
            .or_insert_with(|| blocks::BlockStore::load(id))
            .base
    }

    /// Reader-thread output path: parse into the session's headless Term,
    /// journal, and fan out — atomically under the journal lock. An Attach
    /// serialization takes the same lock, so it can never observe a chunk in
    /// the Term that hasn't been fanned out yet (the client would otherwise
    /// receive that chunk twice: once inside the serialized replay and once
    /// live).
    /// Returns the absolute stream offset (see `Journal::absolute_len`) at
    /// which `bytes` begins — read BEFORE the append — so the caller can
    /// anchor block events to journal coordinates.
    /// `mute` (SLEEP freeze-frame): journal + mirror as always, but skip the
    /// live fanout — set on the dying session between the pre-kill frame
    /// capture and the kill, so attached clients keep the frozen frame
    /// instead of parsing the TUI's graceful-exit wipe. The bytes are still
    /// in the journal; the Exited re-attach replay reflects them (under the
    /// frame overlay), so nothing is ever lost.
    pub fn ingest(
        &self,
        id: Uuid,
        term: &FairMutex<Term<EventProxy>>,
        parser: &mut ImmediateProcessor,
        bytes: &[u8],
        mute: bool,
    ) -> u64 {
        let Ok(journal) = self.journal(id) else { return 0 };
        let mut j = journal.lock();
        let chunk_off = j.absolute_len();
        perf::time(&perf::PARSE_NS, || {
            let mut t = term.lock();
            parser.advance(&mut *t, bytes);
        });
        let new_base = perf::time(&perf::APPEND_NS, || j.append(bytes));
        let new_error = j.take_new_error();
        if !mute {
            self.fanout(id, bytes);
        }
        drop(j);
        if perf::on() {
            perf::INGEST_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            perf::INGEST_CHUNKS.fetch_add(1, Ordering::Relaxed);
        }
        // Compaction eviction happens with the journal lock released — the
        // blocks lock is a leaf and never nests inside it.
        if let Some(base) = new_base {
            self.on_journal_compact(id, base);
        }
        // P5 OutputMatch waiters: fed strictly AFTER the journal lock is
        // released (waiters is a leaf). One relaxed load when nobody waits.
        if self.waiter_count.load(Ordering::Relaxed) > 0 {
            self.feed_output_waiters(id, bytes, chunk_off);
        }
        if new_error {
            self.report_error("terminal output could not be journaled (disk full?)");
        }
        chunk_off
    }

    /// Enqueue an Output frame to every attached client. Callers must hold the
    /// terminal's journal lock (the output serialization point).
    fn fanout(&self, id: Uuid, bytes: &[u8]) {
        let clients = self.clients.lock();
        // LOW-11: build the frame only when someone is attached — a headless
        // flood (GUI closed) otherwise pays two ~64KiB copies per chunk for
        // nobody (~200MB of memcpy per 50MB of output). Holding the clients
        // lock across the serialize is fine: broadcast() already does, and
        // the recipient set must not change between the check and the send.
        if !clients
            .iter()
            .any(|c| c.alive.load(Ordering::Relaxed) && c.attached.lock().contains(&id))
        {
            return;
        }
        let frame = perf::time(&perf::FRAME_NS, || output_frame(id, bytes));
        if let Some(frame) = frame {
            perf::time(&perf::ENQUEUE_NS, || {
                for client in clients.iter() {
                    if client.alive.load(Ordering::Relaxed) && client.attached.lock().contains(&id)
                    {
                        client.enqueue(&frame);
                    }
                }
            });
        }
    }

    /// Daemon-authored output for a terminal WITHOUT a live session (e.g. a
    /// spawn failure). Journal + fanout only; never touches a mirror Term —
    /// the mirror must contain exactly what conhost emitted, nothing else.
    pub fn emit_output(&self, id: Uuid, bytes: &[u8]) {
        let Ok(journal) = self.journal(id) else { return };
        let mut j = journal.lock();
        let new_base = j.append(bytes);
        let new_error = j.take_new_error();
        self.fanout(id, bytes);
        drop(j);
        if let Some(base) = new_base {
            self.on_journal_compact(id, base);
        }
        if new_error {
            self.report_error("terminal output could not be journaled (disk full?)");
        }
    }

    /// A compaction moved the journal's head: evict/flag block records that
    /// now point before it, then persist the sidecar (it is what carries the
    /// base across restarts). Called with NO other lock held.
    fn on_journal_compact(&self, id: Uuid, new_base: u64) {
        let sidecar = {
            let mut map = self.blocks.lock();
            let Some(store) = map.get_mut(&id) else { return };
            store.evict(new_base);
            // Persist whenever the store was ever hooked (epoch > 0), even if
            // eviction emptied recs: the sidecar is the sole carrier of `base`
            // across restarts, and an already-saved sidecar left holding the
            // OLD base + evicted recs would rehydrate ghost records mapping
            // onto the wrong bytes after the next restart. An empty-recs
            // sidecar with the right base is exactly what keeps restart
            // consistent. Never-hooked terminals (claude, cmd: epoch 0, no
            // recs, no sidecar ever written) stay file-free — base=0 on
            // restart is safe when there are no records to misalign.
            (store.epoch > 0 || !store.recs.is_empty()).then(|| store.clone())
        };
        if let Some(s) = sidecar {
            s.save(id);
            // L-11: tell attached clients about the eviction — a GUI holding
            // evicted records would offer Copy-output clicks the daemon then
            // silently drops. Full sync (attach-style state transfer, no
            // events); once per ~8MB of output, not a hot path.
            self.notify_blocks(id, s.epoch, true, s.recs.clone());
        }
    }

    /// A block hook scanned out of the session's output stream. `abs_off` is
    /// the absolute journal offset just after the hook's OSC terminator.
    /// Runs AFTER ingest returned (journal lock released); takes only the
    /// leaf blocks lock, then notifies attached clients lock-free of it.
    pub fn on_block_event(&self, id: Uuid, abs_off: u64, ev: blocks::BlockEvent) {
        // PromptEnd (OSC 133;B) is a GUI-side prompt-end marker: it carries
        // no token, mutates nothing, notifies nothing — return BEFORE the
        // token comparison so it can never log a spoof warning (P3 §5.1).
        if matches!(ev.verb, blocks::HookVerb::PromptEnd) {
            return;
        }
        // Attribution Layer 3: the remote claude-session beacon is tokenless
        // by construction (the consent-installed remote script is persistent;
        // hook tokens rotate per spawn) — handled BEFORE the token check
        // behind its own advisory-trust gates. It mutates only inner_cli /
        // the claude pin, never the block store.
        if let blocks::HookVerb::Beacon { adapter, event, source, sid } = &ev.verb {
            self.on_beacon(id, adapter, event, source, sid);
            return;
        }
        let now = now_ms();
        let is_pre = matches!(ev.verb, blocks::HookVerb::Pre { .. });
        // P6a §7.2: the exec hook's command line, captured for the hook-based
        // inner-CLI fold below (WSL/remote process trees are invisible to the
        // Win32 tracker — the hooks are the only truthful witness).
        let exec_cmd = match &ev.verb {
            blocks::HookVerb::Exec { cmd } => Some(cmd.clone()),
            _ => None,
        };
        // Attribution Layer 1 (WSL): the init hook's `home` keys the
        // \\wsl$ claude-registry scan. Captured here, stored AFTER the
        // token check below (blocks is a leaf lock — nothing else may be
        // taken while it's held).
        let init_home = match &ev.verb {
            blocks::HookVerb::Init { home, .. } if !home.is_empty() => Some(home.clone()),
            _ => None,
        };
        let mut hook_cwd: Option<std::path::PathBuf> = None;
        // P6b §3.3.1: cmd's static PROMPT pre carries no cwd payload ($P
        // cannot be hex-encoded by PROMPT macros), but the adjacent tokenless
        // OSC 9;9 — rendered by the same PROMPT string, microseconds earlier
        // in the stream — already updated Session.osc_cwd (the ingest thread
        // feeds the OSC scanner before the block scan). Substitute it so the
        // record cwd stays real. Read BEFORE the blocks lock (sessions is not
        // a leaf; blocks is).
        let pre_cwd_fill: Option<std::path::PathBuf> = match &ev.verb {
            blocks::HookVerb::Pre { cwd, .. } if cwd.is_empty() => self
                .sessions
                .lock()
                .get(&id)
                .and_then(|s| s.osc_cwd.lock().clone()),
            _ => None,
        };
        // Prompt-time live_cwd witness (lane-label freshness): the payload
        // cwd when the hook carries one (pwsh/bash `d` field, verbatim like
        // BlockRec.cwd), else the adjacent OSC 9;9 fill (cmd). Applied after
        // the blocks lock releases — but only for a TOKEN-CHECKED pre, so the
        // value is captured here and gated on `outcome`-side acceptance
        // below via `pre_accepted`.
        let pre_live_cwd: Option<std::path::PathBuf> = match &ev.verb {
            blocks::HookVerb::Pre { cwd, .. } if !cwd.is_empty() => {
                Some(std::path::PathBuf::from(cwd.clone()))
            }
            blocks::HookVerb::Pre { .. } => pre_cwd_fill.clone(),
            _ => None,
        };
        // (epoch, changed recs, store snapshot to persist) — built under the
        // leaf blocks lock, acted on after it is released.
        let outcome = {
            let mut map = self.blocks.lock();
            let Some(store) = map.get_mut(&id) else { return };
            // accept_token also marks the bootstrap live (P5 hooks_live).
            if !store.accept_token(&ev.token) {
                log::warn!("terminal {id}: block hook with wrong token rejected");
                return;
            }
            match ev.verb {
                blocks::HookVerb::Init { pid, shell, home, user } => {
                    // shell/home/user are advisory diagnostics today; the
                    // \\wsl$ correlation (P6a.2 §7.3) will consume home. The
                    // log line is what probe `wsl_hooks` pins.
                    if shell.is_empty() && home.is_empty() {
                        log::info!("terminal {id}: block hooks active (shell pid {pid})");
                    } else {
                        log::info!(
                            "terminal {id}: block hooks active (shell pid {pid}, shell={shell}, home={home}, user={user})"
                        );
                    }
                    None
                }
                blocks::HookVerb::Exec { cmd } => {
                    // Freshest hook-reported cwd (POSIX-verbatim for WSL),
                    // read before open_block stamps it into the record.
                    hook_cwd = store.last_cwd.clone();
                    let changed = store.open_block(cmd, abs_off, now);
                    let recs: Vec<_> = changed
                        .into_iter()
                        .filter_map(|i| store.recs.get(i).cloned())
                        .collect();
                    // Sidecar persists on close, not open; a dangling close
                    // folded into this exec rides along with the next save.
                    Some((store.epoch, recs, None))
                }
                blocks::HookVerb::Pre { exit, n, cwd } => {
                    if let Some(fill) = pre_cwd_fill {
                        store.last_cwd = Some(fill);
                    }
                    match store.on_pre(exit, n, cwd, abs_off, now) {
                        Some(i) => {
                            let rec = store.recs.get(i).cloned();
                            Some((store.epoch, rec.into_iter().collect(), Some(store.clone())))
                        }
                        None => None, // cwd refresh only
                    }
                }
                blocks::HookVerb::PromptEnd => None, // early-returned above
                blocks::HookVerb::Beacon { .. } => None, // early-returned above
            }
        };
        // ANY token-checked hook (init/exec/pre) proves the link is
        // interactive again — a CLI-resume trailing occupies the shell
        // BEFORE its first prompt, so waiting for a `pre` alone left a
        // resumed ssh reconnect "reconnecting…" into the 30s supervision
        // window (probe ssh_cli_resume pinned it). Same witness clears the
        // probe auth-dead cache (§4.6: a spawn that hooked without anyone
        // typing proves non-interactive auth). Reaching here implies
        // accept_token passed — spoofed hooks returned above.
        self.resolve_reconnect(id);
        self.probe_rt.clear_auth_dead(id);
        // Token-checked init: remember the shell's home for the WSL
        // claude-registry scan (runtime-only, re-reported every spawn).
        if let Some(home) = init_home {
            self.hook_homes.lock().insert(id, home);
        }
        // P5 Prompt waiters resolve on EVERY token-checked `pre` — including
        // a cwd-refresh pre with no open block (the first-prompt case), which
        // yields no record outcome at all. Runs with the blocks lock released.
        if is_pre && self.waiter_count.load(Ordering::Relaxed) > 0 {
            self.resolve_prompt_waiters(id);
        }
        // Lane-label freshness: fold the token-checked pre's cwd into
        // live_cwd and broadcast ONE Snapshot when it actually changed (a cd
        // changes it once — never a storm). Runs before the outcome
        // early-return: the common `cd` lands as a cwd-refresh pre with no
        // open block. (Reaching here implies accept_token passed — spoofed
        // hooks returned above.)
        if is_pre {
            if let Some(cwd) = pre_live_cwd {
                if self.apply_hook_cwd(id, cwd) {
                    self.broadcast_snapshot();
                }
            }
        }
        let Some((epoch, recs, snap)) = outcome else { return };
        // P6a §7.2 inner-CLI lifecycle, hook-fed (WslShell family only inside
        // the helpers; all leaf/state locks taken sequentially, never nested):
        // a closing record that matches the remembered CLI block clears
        // inner_cli (the CLI exited back to a prompt); an exec that parses as
        // an adapter launch sets it (and remembers this block's key).
        if let Some(key) = recs
            .iter()
            .find(|r| r.end_off.is_some())
            .map(|r| (r.epoch, r.start_off))
        {
            let was_cli_block = self.cli_blocks.lock().get(&id) == Some(&key);
            if was_cli_block {
                self.cli_blocks.lock().remove(&id);
                self.set_inner_cli(id, None);
                // M1 (remote-cli-resume-spec): the CLI exited back to a
                // prompt — nothing to resume, the M0 snapshot basis is
                // obsolete. NO connection here, just the sidecar delete.
                remote_probe::delete_sidecar(id);
            }
        }
        if let Some(cmd) = &exec_cmd {
            self.track_hook_exec(id, epoch, abs_off, cmd, hook_cwd);
        }
        if let Some(s) = snap {
            s.save(id);
        }
        if !recs.is_empty() {
            // P5 BlockClose/Run waiters: any record that just CLOSED (a pre,
            // or exec closing a dangling predecessor) can resolve them.
            let closed: Vec<crate::state::BlockRec> = recs
                .iter()
                .filter(|r| r.end_off.is_some())
                .cloned()
                .collect();
            self.notify_blocks(id, epoch, false, recs);
            self.resolve_block_close(id, &closed);
        }
    }

    /// Enqueue a Blocks frame to every client attached to `id` (same
    /// recipient filter as fanout(), but callable without the journal lock).
    fn notify_blocks(&self, id: Uuid, epoch: u32, full: bool, recs: Vec<crate::state::BlockRec>) {
        // P5 events ride the incremental notifications only: a full sync is
        // attach-time state transfer, not news.
        if !full {
            for rec in &recs {
                let ev = if rec.end_off.is_none() {
                    CtlEvent::BlockOpened {
                        id,
                        rec: rec.clone(),
                    }
                } else {
                    CtlEvent::BlockClosed {
                        id,
                        rec: rec.clone(),
                    }
                };
                self.emit_event(Some(id), EV_BLOCKS, &ev);
            }
        }
        if let Some(frame) = frame_bytes(&D2C::Blocks { id, epoch, full, recs }) {
            for client in self.clients.lock().iter() {
                if client.alive.load(Ordering::Relaxed) && client.attached.lock().contains(&id) {
                    client.enqueue(&frame);
                }
            }
        }
    }

    /// `gen` is the caller's spawn generation (`Session.gen`): a late exit or
    /// panic handler from a PREVIOUS process must not tear down a successor
    /// session that a fast relaunch already put in the map under the same id.
    pub fn on_exit(&self, id: Uuid, gen: u64, code: Option<u32>) {
        let removed;
        {
            let mut sessions = self.sessions.lock();
            if sessions.get(&id).is_some_and(|s| s.gen != gen) {
                log::info!("terminal {id}: stale exit (gen {gen}) ignored — a successor session is live");
                return;
            }
            removed = sessions.remove(&id);
        }
        // Session drop closes the ConPTY (cross-process) — do it OUTSIDE the
        // sessions mutex, like every other blockable per-session syscall.
        drop(removed);
        log::info!("terminal {id} exited with {code:?}");
        let expected = self.expected_exits.lock().remove(&id);
        // Hook liveness of the DYING epoch, read before anything rotates it:
        // the ssh reconnect qualification (a hooked session proved its auth
        // completes without interaction).
        let hooks_were_live = self
            .blocks
            .lock()
            .get(&id)
            .is_some_and(|s| s.hooks_live);
        self.spawn_times
            .lock()
            .entry(id)
            .and_modify(|w| w.1 = Some(now_ms()));
        {
            let mut state = self.state.lock();
            if let Some(t) = state.terminal_mut(id) {
                t.status = TermStatus::Dead;
            } else {
                return; // deleted terminal
            }
            if let Err(e) = state.save() {
                drop(state);
                self.report_error(&format!("failed to save state on exit: {e}"));
            }
        }
        self.maybe_schedule_reconnect(id, code, expected, hooks_were_live);
        // Flush this terminal's journal so the tail survives a crash. No
        // in-stream "process exited" marker: the sidebar status dot and the
        // Restore affordance already say it, and any seam text would survive
        // in scrollback forever — persistence should read as if the process
        // never went away.
        let mut abs = 0u64;
        if let Ok(journal) = self.journal(id) {
            let mut j = journal.lock();
            j.sync();
            abs = j.absolute_len();
        }
        // Close a block the dying session left open (exit=None) and persist
        // the sidecar. Journal lock released first; blocks is a leaf.
        {
            let outcome = {
                let mut map = self.blocks.lock();
                map.get_mut(&id).and_then(|store| {
                    let changed = store.close_dangling(abs, now_ms());
                    (!changed.is_empty()).then(|| {
                        let recs: Vec<_> = changed
                            .into_iter()
                            .filter_map(|i| store.recs.get(i).cloned())
                            .collect();
                        (store.epoch, recs, store.clone())
                    })
                })
            };
            if let Some((epoch, recs, snap)) = outcome {
                snap.save(id);
                // P5: dangling closes first (a Run composite gets its honest
                // RunDone with exit None), then the exit resolution below.
                let closed = recs.clone();
                self.notify_blocks(id, epoch, false, recs);
                self.resolve_block_close(id, &closed);
            }
        }
        // P5: Exit waiters resolve; every other waiter for this id fails
        // "exited" (its condition can no longer occur); subscribers get the
        // event alongside the legacy broadcast.
        self.resolve_exit_waiters(id, code);
        self.emit_event(Some(id), EV_EXIT, &CtlEvent::Exited { id, code });
        self.broadcast(&D2C::Exited { id, code });
        self.broadcast_snapshot();
    }

    /// Stamp a terminal's NEXT exit as deliberate (kill/delete/sleep — every
    /// site that calls killer.kill() on purpose). on_exit consumes the stamp;
    /// an UN-stamped ssh death is the auto-reconnect trigger.
    fn mark_expected_exit(&self, id: Uuid) {
        self.expected_exits.lock().insert(id);
    }


    fn broadcast(&self, msg: &D2C) {
        let Some(frame) = frame_bytes(msg) else { return };
        let mut clients = self.clients.lock();
        clients.retain(|c| c.alive.load(Ordering::Relaxed));
        for client in clients.iter() {
            client.enqueue(&frame);
        }
    }

    fn broadcast_snapshot(&self) {
        // r2-F10: hold the order lock across clone→enqueue so two racing
        // broadcasts can never enqueue an older state after a newer one.
        let _order = self.snapshot_order.lock();
        let snapshot = self.state.lock().clone();
        self.broadcast(&D2C::Snapshot { state: snapshot });
        // P5: coarse "something changed, List if you care" event — the daemon
        // computes no diffs, and List is O(terminals) with no IO.
        self.emit_event(None, EV_STATE, &CtlEvent::StateChanged);
    }

    /// launch() for CONN-THREAD entry points (C2D::RestartTerminal, Ctl
    /// Wake/Restart): when a remote CLI-resume probe is DUE (spec §6.2 D9 —
    /// a probe on a client-conn handler thread would stall that GUI's other
    /// traffic for up to the 25s probe bound), the launch moves to a
    /// `probe-launch-<id>` worker; otherwise it runs inline exactly as
    /// before. The `probing` claim coalesces double-clicks across the probe
    /// window; launch()'s own LaunchGuard still protects launch() itself.
    fn launch_from_conn(self: &Arc<Self>, id: Uuid) {
        self.probe_aware_launch(id, None);
    }

    /// The probe-due/worker routing shared by conn-thread launches and the
    /// reconnect pump. `reconnect_attempt` = Some(n) when this launch IS
    /// reconnect attempt n: after the launch, a SYNCHRONOUS spawn failure
    /// advances the backoff ladder — there is no process then, so no on_exit
    /// ever will (a spawned-but-dying attempt is advanced by
    /// maybe_schedule_reconnect when it exits). That accounting must run
    /// with the launch, wherever the launch runs.
    fn probe_aware_launch(self: &Arc<Self>, id: Uuid, reconnect_attempt: Option<u8>) {
        let run = move |core: &Arc<Self>| {
            core.launch(id);
            if let Some(attempt) = reconnect_attempt {
                let still_dead = {
                    let state = core.state.lock();
                    state
                        .terminal(id)
                        .is_none_or(|t| t.status == TermStatus::Dead)
                };
                if still_dead {
                    core.advance_reconnect(id, attempt);
                }
            }
        };
        // Reconnect attempts ALWAYS route through the worker: the caller is
        // the 250ms flush tick, and a plain launch() (bootstrap write +
        // journal tail read + CreateProcess, 100-500ms) would stall every
        // journal's fsync and all wait timeouts once per backoff rung. The
        // `probing` claim + requeue below already coalesce concurrent
        // attempts for one id.
        if reconnect_attempt.is_none() && !remote_probe::probe_due(self, id) {
            run(self);
            return;
        }
        // NOT `insert(id).then_some(..)`: then_some constructs eagerly (the
        // LaunchGuard trap) — claim with an explicit if/else.
        let claimed = self.probing.lock().insert(id);
        if !claimed {
            log::info!("launch({id}) coalesced: a probe-launch is in flight");
            // r2-F9: the pump already marked this attempt `watching` — but
            // nothing launched FOR THE LADDER, so if the coalesced-into
            // launch doesn't revive the terminal the entry would just expire
            // 30s later ("interactive auth?") and supervision stops. Put the
            // rung back so the backoff engine stays in charge.
            if let Some(attempt) = reconnect_attempt {
                self.requeue_reconnect(id, attempt);
            }
            return;
        }
        let core = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("probe-launch-{id}"))
            .spawn(move || {
                let _ = catch_unwind(AssertUnwindSafe(|| run(&core)));
                core.probing.lock().remove(&id);
            });
        if spawned.is_err() {
            self.probing.lock().remove(&id);
            run(self); // never strand a restore on thread exhaustion
        }
    }

    /// Spawn (or resume) a terminal's process. Appends a restore marker if the
    /// journal already has history.
    fn launch(self: &Arc<Self>, id: Uuid) {
        // Coalesce concurrent launches (see `Core::launching`): the loser
        // returns immediately; the winner's Snapshot broadcast updates
        // everyone once the spawn lands.
        let Some(_launch_guard) = LaunchGuard::try_begin(&self.launching, id) else {
            log::info!("launch({id}) coalesced: another launch is in flight");
            return;
        };
        let mut meta = {
            let state = self.state.lock();
            match state.terminal(id) {
                Some(t) if t.status == TermStatus::Dead => t.clone(),
                _ => return,
            }
        };
        // Claude session-id RE-PIN (wake/restore): a `--resume` FORKS in some
        // claude versions and `/clear` rotates the id — the pinned one then
        // replays a frozen fork point ("does not resume properly"). If the
        // previous run's window shows the pinned transcript went untouched
        // while exactly ONE new session jsonl was born, that one is this
        // terminal's live conversation: re-pin to it. Abstains (never
        // guesses) without a window, with an actively-written pinned file,
        // or with ≠1 candidates.
        if let TermKind::Claude {
            session_id,
            extra_args,
        } = meta.kind.clone()
        {
            let window = self.spawn_times.lock().get(&id).copied();
            if let Some((spawn_ms, exit_ms)) = window {
                let end_ms = exit_ms.unwrap_or_else(now_ms);
                if let Some(new_id) =
                    tracker::claude_repin_candidate(&meta.cwd, session_id, spawn_ms, end_ms)
                {
                    log::info!(
                        "terminal {id}: claude session id re-pinned {session_id} -> {new_id} (rotated during the previous run)"
                    );
                    meta.kind = TermKind::Claude {
                        session_id: new_id,
                        extra_args,
                    };
                    let mut state = self.state.lock();
                    if let Some(t) = state.terminal_mut(id) {
                        t.kind = meta.kind.clone();
                        state.save_logged("claude session re-pin");
                    }
                }
            }
        }
        // Clamped like C2D::Resize: a state.json written by an older build
        // could hold arbitrary u16s, and these go straight into a grid alloc.
        let (cols, rows) = if meta.last_cols >= 2 && meta.last_rows >= 2 {
            (meta.last_cols.clamp(2, 1000), meta.last_rows.clamp(2, 1000))
        } else {
            (session::DEFAULT_COLS, session::DEFAULT_ROWS)
        };

        // Build the spawn plan. Shell/Custom terminals resume where their tree
        // last was; a confidently-tracked inner CLI (claude) is re-launched via
        // a direct powershell -Command — or, for WSL shells, a restore
        // trailing baked into the fresh rcfile (never keystroke injection).
        // Claude-kind terminals keep their stronger pinned-id path untouched.
        //
        // `hooked`: this spawn injects the block-hook bootstrap — a plain
        // PowerShell shell or WSL-bash shell (spawn() injects it per family),
        // a cmd shell (PROMPT env, P6b — pre/9;9/133 hooks only, no exec),
        // an ssh shell with remote_hooks on (one-shot remote rc, P6c), or the
        // powershell restore wrapper below (baked into its -Command). All
        // other programs (opted-out ssh, claude, non-pwsh Custom,
        // prompt-clobbering shells) run in degraded mode: no hooks, zero
        // block records — they render exactly as today.
        let family = crate::state::shell_family(&meta.kind, &meta.program, &meta.args);
        let is_wsl = matches!(family, crate::state::ShellFamily::WslShell { .. });
        let is_cmd = matches!(family, crate::state::ShellFamily::Cmd);
        // P6c: remote hooks are a per-terminal opt-in defaulting ON
        // (ShellCfg.remote_hooks; None = the serde default = on).
        let is_ssh = matches!(family, crate::state::ShellFamily::Ssh { .. });
        let ssh_hooks = is_ssh
            && meta
                .shell_cfg
                .as_ref()
                .is_none_or(|c| c.remote_hooks);
        let mut hooked = ssh_hooks
            || matches!(
                family,
                crate::state::ShellFamily::Pwsh
                    | crate::state::ShellFamily::WslShell { .. }
                    | crate::state::ShellFamily::Cmd
            );
        // Remote CLI-resume seam (remote-cli-resume-spec §6.1): for an ssh
        // shell with a tracked inner CLI, run the correlate leg (M3/M4 — a
        // bounded read-only sftp listing diffed against the M0 sidecar) or
        // the M5 re-pin belt BEFORE the inner_cli match below. On Correlated
        // it mutates + persists meta.inner_cli and the ordinary Explicit/
        // Correlated restore path takes over byte-identically; on definitive
        // ambiguity it clears inner_cli and returns the §6.4 candidates text
        // (pushed into the preface after spawn). We run this before the
        // meta.kind gate check inside — it self-gates on family/inner_cli.
        let probe_notice: Option<String> = if matches!(meta.kind, TermKind::Shell | TermKind::Custom)
        {
            remote_probe::upgrade_before_launch(self, id, &mut meta)
        } else {
            None
        };
        let mut spawn_meta = meta.clone();
        let mut ambiguous_adapter: Option<String> = None;
        // WSL/ssh inner-CLI resume rides the rcfile tail (§7.4), built below
        // and handed to the rcfile writer at the rotation site.
        let mut bash_trailing: Option<String> = None;
        if matches!(meta.kind, TermKind::Shell | TermKind::Custom) {
            // live_cwd validity is namespace-aware (§4): a POSIX cwd is
            // trusted verbatim — `.is_dir()` on "/tmp" resolves against the
            // daemon's own drive on Windows, and a stale remote dir simply
            // makes the shell start in $HOME with cd's error visible.
            let base_cwd = match crate::state::path_namespace(&family) {
                crate::state::PathNamespace::Win => meta
                    .live_cwd
                    .clone()
                    .filter(|p| p.is_dir())
                    .unwrap_or_else(|| meta.cwd.clone()),
                crate::state::PathNamespace::Posix => meta
                    .live_cwd
                    .clone()
                    .unwrap_or_else(|| meta.cwd.clone()),
            };
            // Adapter-generic: any confidently-tracked CLI whose registry entry
            // can produce a resume command gets relaunched in place (claude,
            // codex, copilot, qwen, goose, …). Ambiguity is surfaced, never
            // guessed.
            match &meta.inner_cli {
                Some(cli)
                    if matches!(
                        cli.confidence,
                        CliConfidence::Explicit | CliConfidence::Correlated
                    ) =>
                {
                    match tracker::restore_trailing(&cli.adapter, cli.resume_token.as_deref()) {
                        Some(resume_cmd) if is_wsl => {
                            // §7.4 WslShell row: same wsl argv with the
                            // cd+resume appended to the freshly-rotated
                            // rcfile. The CLI runs, and on exit the user is
                            // at a hooked prompt (pwsh -NoExit parity). The
                            // cli cwd is POSIX-verbatim (hook-tracked).
                            let cli_cwd = cli.cwd.to_string_lossy();
                            bash_trailing = bootstrap::bash_restore_trailing(&cli_cwd, &resume_cmd)
                                .or_else(|| {
                                    // Unquotable resume (never true for
                                    // registry adapters): degrade to cd-only.
                                    log::warn!(
                                        "terminal {id}: resume command not embeddable; restoring shell only"
                                    );
                                    None
                                });
                            spawn_meta.cwd = base_cwd;
                        }
                        Some(resume_cmd) if is_ssh => {
                            // §7.4 Ssh row: cd (best-effort — the remote dir
                            // may be gone; the shell must still come up
                            // hooked) + resume baked into the one-shot rc,
                            // announced through a real exec hook so the
                            // resumed CLI gets a block and inner_cli clears
                            // when it closes. The cli cwd is remote-POSIX
                            // verbatim (hook-tracked).
                            let cli_cwd = cli.cwd.to_string_lossy();
                            bash_trailing =
                                bootstrap::ssh_restore_trailing(&cli_cwd, &resume_cmd).or_else(
                                    || {
                                        log::warn!(
                                            "terminal {id}: resume command not embeddable; restoring shell only"
                                        );
                                        Some(bootstrap::ssh_cd_trailing(&cli_cwd))
                                    },
                                );
                            spawn_meta.cwd = base_cwd;
                        }
                        Some(resume_cmd) if is_cmd => {
                            // §7.4 Cmd row: `cmd.exe /K <resume>` spawned in
                            // the CLI's cwd (CommandBuilder.cwd — no `cd /d`
                            // quoting gymnastics needed) with the PROMPT env
                            // re-injected per family by spawn(): the CLI
                            // runs, and on exit the user lands at a hooked
                            // cmd prompt (pwsh -NoExit parity). kind stays
                            // Shell and the program stays cmd.exe so the
                            // family classifier still says Cmd (args are
                            // ignored for cmd — only wsl argv shapes gate).
                            let cli_cwd = if cli.cwd.is_dir() {
                                cli.cwd.clone()
                            } else {
                                base_cwd.clone()
                            };
                            spawn_meta.args = vec!["/K".into(), resume_cmd];
                            spawn_meta.cwd = cli_cwd;
                        }
                        Some(resume_cmd) => {
                            let cli_cwd = if cli.cwd.is_dir() {
                                cli.cwd.clone()
                            } else {
                                base_cwd.clone()
                            };
                            spawn_meta.kind = TermKind::Custom; // wrapper baked into the script
                            spawn_meta.program = "powershell.exe".into();
                            spawn_meta.args = session::powershell_restore_command(
                                Some(&bootstrap::script_path(id)),
                                Some(&cli_cwd),
                                Some(&resume_cmd),
                            );
                            spawn_meta.cwd = cli_cwd;
                            hooked = true; // bootstrap baked into the -Command
                        }
                        None => spawn_meta.cwd = base_cwd,
                    }
                }
                Some(cli) if matches!(cli.confidence, CliConfidence::Ambiguous) => {
                    spawn_meta.cwd = base_cwd;
                    ambiguous_adapter = Some(cli.adapter.clone());
                }
                _ => spawn_meta.cwd = base_cwd,
            }
            // P6c §8: a plain ssh restore lands in the tracked remote cwd via
            // a best-effort cd baked into the one-shot rc (ssh has no --cd
            // transport). Only a posix-absolute cwd qualifies — the first
            // spawn's Windows-side meta.cwd means "remote default $HOME", so
            // no cd is emitted at all; a stale remote dir prints cd's own
            // error and the shell still comes up hooked (honest).
            if is_ssh && bash_trailing.is_none() {
                let cwd = spawn_meta.cwd.to_string_lossy();
                if cwd.starts_with('/') {
                    bash_trailing = Some(bootstrap::ssh_cd_trailing(&cwd));
                }
            }
        }

        // Journal Blocks spawn rotation: mint a fresh hook token, regenerate
        // the bootstrap file, bump the store's epoch, and close any block the
        // previous session left open (exit=None — it never reported one).
        // Only hooked spawns rotate; degraded-mode terminals keep no store
        // state and no sidecar.
        let bootstrap_path = if hooked {
            let token = bootstrap::mint_token();
            let write_result = if is_wsl {
                // v0.1.2 WSL welcome banner: fresh creates print the motd
                // when the creating surface opted in at create time
                // (ShellCfg.wsl_motd — the GUI stamps its Settings pref;
                // ctl/probe creates default off). Restores/relaunches
                // (launched_once) NEVER show it — and must never CONSUME the
                // distro's once-a-day stamp either, so they pre-set
                // MOTD_SHOWN instead of running the stock login chain bare.
                let motd = if meta.launched_once {
                    bootstrap::WslMotd::Restore
                } else if meta.shell_cfg.as_ref().is_some_and(|c| c.wsl_motd) {
                    bootstrap::WslMotd::Banner
                } else {
                    bootstrap::WslMotd::Stock
                };
                bootstrap::write_bashrc(id, &token, bash_trailing.as_deref(), motd)
            } else if is_ssh {
                bootstrap::write_bashrc_remote(id, &token, bash_trailing.as_deref())
            } else if is_cmd {
                bootstrap::write_cmd_prompt(id, &token)
            } else {
                // Banner-visibility fix: `-Command` suppresses PowerShell's
                // logo, so the bootstrap reproduces it (top of the script,
                // once per real spawn) unless the terminal's own args say
                // -NoLogo. Checked on the PERSISTED meta args (the user's),
                // not spawn_meta's synthesized tail.
                bootstrap::write_script(id, &token, bootstrap::pwsh_wants_banner(&meta.args))
            };
            let path = match write_result {
                Ok(p) => Some(p),
                Err(e) => {
                    // pwsh spawns fall back to the inline prompt wrapper
                    // (cwd tracking survives, blocks don't); WSL spawns
                    // degrade to plain `bash -i` (spawn()'s guard); cmd
                    // spawns degrade to no PROMPT env (hookless, working);
                    // ssh spawns degrade to plain `ssh <args> <host>`; the
                    // restore wrapper's dot-source will error once and
                    // continue.
                    log::error!("bootstrap script write failed for {id}: {e}");
                    None
                }
            };
            // Absolute offset for the dangling close, read before the seam
            // append below so it stays inside the dead session's bytes.
            let abs = self
                .journal(id)
                .map(|j| j.lock().absolute_len())
                .unwrap_or(0);
            let snap = {
                let mut map = self.blocks.lock();
                let store = map.entry(id).or_insert_with(|| blocks::BlockStore::load(id));
                // Epoch bump + fresh token + hooks_live reset (P5).
                store.rotate(token);
                store.close_dangling(abs, now_ms());
                (!store.recs.is_empty() || store.epoch > 1).then(|| store.clone())
            };
            if let Some(s) = snap {
                s.save(id);
            }
            path
        } else {
            None
        };

        // Suspend attached clients for this id during the world-rewrite; live
        // fanout would hit their stale pre-restore grids.
        let suspended: Vec<Arc<ClientConn>> = {
            let clients = self.clients.lock();
            clients
                .iter()
                .filter(|c| c.alive.load(Ordering::Relaxed) && c.attached.lock().remove(&id))
                .cloned()
                .collect()
        };
        let mut preface = serialize::Preface::default();
        if meta.launched_once {
            // Journal-only seam: a concealed sentinel + pad + home separates
            // this session's raw stream from the previous one so a future
            // scratch-parse (the preface build below) can't let absolute
            // cursor moves interleave sessions. NOTHING here touches the
            // mirror Term or clients — the mirror mirrors conhost exactly.
            let fast_exits = *self.fast_exits.lock().get(&id).unwrap_or(&0);
            let marker = format!("\r\n\x1b[8m{}\x1b[28m", serialize::SEAM_SENTINEL);
            let mut seam: Vec<u8> = marker.into_bytes();
            seam.extend(std::iter::repeat_n(b"\r\n".as_slice(), rows as usize).flatten());
            seam.extend_from_slice(b"\x1b[H");
            if let Ok(journal) = self.journal(id) {
                let mut j = journal.lock();
                let tail = j.tail();
                // Older sessions' content, pre-rendered (seams erased) for
                // attach-time prepending — built BEFORE the seam append so a
                // dead-in-alt tail's closure fix (`?1049l` + the killed
                // frame re-printed as scrollback) can be journaled AHEAD of
                // the seam: future re-parses then see a properly closed alt
                // region as real bytes (the "sleeping claude wipes the
                // history" fix — an unexited alt ENTER used to swallow every
                // later session into the frozen alt grid).
                let pre_t0 = perf::on().then(Instant::now);
                let (p, alt_fix) = serialize::preface_with_alt_fix(&tail, cols, rows);
                preface = p;
                if let Some(t0) = pre_t0 {
                    log::info!(
                        "[perf] preface_from_raw id={id} tail_bytes={} us={}",
                        tail.len(),
                        t0.elapsed().as_micros()
                    );
                }
                let new_base = if alt_fix.is_empty() {
                    j.append(&seam)
                } else {
                    log::info!(
                        "terminal {id}: closing dead alt-screen region in journal (+{} bytes frame preserve)",
                        alt_fix.len()
                    );
                    let mut fixed = alt_fix;
                    fixed.extend_from_slice(&seam);
                    j.append(&fixed)
                };
                drop(j);
                if let Some(base) = new_base {
                    self.on_journal_compact(id, base);
                }
            }
            if fast_exits >= 2 {
                preface.push_info_line(&format!(
                    "── exited immediately {fast_exits}× — check the command ──"
                ));
            }
        }
        match session::spawn(
            self.clone(),
            &spawn_meta,
            cols,
            rows,
            preface,
            bootstrap_path.as_deref(),
        ) {
            Ok(s) => {
                // The terminal may have been deleted while the process spawned;
                // if so, kill what we just started instead of resurrecting it.
                let mut state = self.state.lock();
                match state.terminal_mut(id) {
                    Some(t) => {
                        t.status = TermStatus::Running;
                        t.launched_once = true;
                        // SLEEP S3: waking IS launching — clear the flag in
                        // the SAME mutate that sets Running, so (Running,
                        // asleep) can never be produced by a wake and every
                        // wake spelling (Ctl Wake, GUI Restore/↻, tc
                        // restart, folder wake) shares the one clear-point.
                        t.asleep = false;
                        // Persist the hooked verdict so a client's very first
                        // attach already knows to reserve the composer strip
                        // (kills the 49↔52 boot resize flip — the epoch used
                        // to arrive only with the Blocks sync, one drain too
                        // late for the attach-at-size announcement).
                        t.hooked = hooked;
                        // The size this PTY actually spawned at is
                        // authoritative until a client resizes: write it back
                        // when meta was unknown, so a Snapshot can never
                        // advertise an unknown grid for a RUNNING session.
                        // (An unknown left the GUI's attach at the 160×42
                        // default, yanking a live PTY to it on every boot —
                        // the conhost repaint-storm / Bug-B raw material.)
                        if t.last_cols < 2 || t.last_rows < 2 {
                            t.last_cols = cols;
                            t.last_rows = rows;
                        }
                        // A Resize can land while the process is spawning: its
                        // session lookup misses (nothing in the map yet), so
                        // only state was updated. state is authoritative —
                        // apply it to the fresh PTY+Term before publishing the
                        // session. Holding the state lock through the insert
                        // makes this watertight: a concurrent Resize either
                        // already wrote last_* (visible here), or blocks on
                        // the state lock until the session is in the map and
                        // its own lookup succeeds.
                        let (lc, lr) = (t.last_cols, t.last_rows);
                        if lc >= 2 && lr >= 2 && (lc, lr) != (cols, rows) {
                            let _ = s.master.lock().resize(portable_pty::PtySize {
                                rows: lr,
                                cols: lc,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                            // Conhost-parity grow — see do_resize.
                            serialize::resize_conhost(
                                &mut s.term.lock(),
                                lc as usize,
                                lr as usize,
                            );
                            log::info!("[resize] {id} {lc}x{lr} (post-spawn catch-up)");
                        }
                        if let Err(e) = state.save() {
                            drop(state);
                            self.report_error(&format!("failed to save state: {e}"));
                        }
                        // Fresh spawn window (claude re-pin evidence base).
                        self.spawn_times.lock().insert(id, (now_ms(), None));
                        self.sessions.lock().insert(id, s);
                        // SLEEP freeze-frame: waking IS launching — the frame
                        // sidecar dies in the SAME success path that clears
                        // the asleep flag (a failed spawn keeps both, so the
                        // view stays honest). The successor TUI repaints from
                        // scratch; v1 accepts the brief blank.
                        frame::remove(id);
                    }
                    None => {
                        drop(state);
                        let mut s = s;
                        let _ = s.killer.kill();
                        return;
                    }
                }
                if let Some(adapter) = &ambiguous_adapter {
                    // A CLI session was running here but its identity was
                    // ambiguous; never guess — tell the user how to resume.
                    // Preface-space (not the mirror/PTY stream): informational
                    // lines must never shift the mirror's coordinates away
                    // from conhost's. The resync below carries it to clients.
                    if let Some(s) = self.sessions.lock().get(&id) {
                        s.preface.lock().push_info_line(&format!(
                            "── a {adapter} session was running here but its identity was ambiguous; resume it manually (e.g. {adapter} --resume <session-id>) ──"
                        ));
                    }
                }
                if let Some(text) = &probe_notice {
                    // Remote correlate leg ended definitively ambiguous: the
                    // §6.4 candidates block (up to 5 paste-able resume
                    // commands, newest-first). Preface-space like the line
                    // above; inner_cli was already cleared (DO-NOT 9), so
                    // the generic ambiguous arm can never double-report.
                    if let Some(s) = self.sessions.lock().get(&id) {
                        let mut preface = s.preface.lock();
                        for line in text.lines() {
                            preface.push_info_line(line);
                        }
                    }
                }
                // Re-sync the suspended clients onto the rewritten world:
                // discard-and-replace via Reset + a serialized Replay, ordered
                // against live output by the journal lock (same guarantee as
                // Attach).
                if !suspended.is_empty() {
                    // Hint inputs (proto 7), snapshotted under the journal
                    // lock exactly like Attach's; computed after it drops.
                    let mut hint_job: Option<(Vec<u8>, u64, Vec<u8>)> = None;
                    let arcs = self
                        .sessions
                        .lock()
                        .get(&id)
                        .map(|s| (s.term.clone(), s.preface.clone(), s.win32_input.clone()));
                    if let (Some((term, preface, win32)), Ok(journal)) = (arcs, self.journal(id)) {
                        let j = journal.lock();
                        let mut raw_tail_replay = false;
                        let mut bytes = {
                            let t = term.lock();
                            if serialize::is_alt_screen(&t) {
                                // Cut-safe: a tail cut inside the alt region
                                // would otherwise paint TUI frames onto the
                                // client's PRIMARY grid (claude-fragment
                                // fusion class — see alt_tail_for_live).
                                raw_tail_replay = true;
                                serialize::alt_tail_for_live(j.tail())
                            } else {
                                serialize::serialize_term(&t, Some(&preface.lock()))
                            }
                        };
                        // Private mode 9001 (win32-input-mode) isn't part of
                        // the mirror's serializable state; re-assert it so the
                        // client's key encoder sees it (term_backend scans its
                        // own byte stream for exactly this).
                        if win32.load(Ordering::Relaxed) {
                            bytes.extend_from_slice(b"\x1b[?9001h");
                        }
                        // Same guarantee as Attach: captured under the
                        // journal lock, so live Output resumes exactly here.
                        let stream_off = j.absolute_len();
                        // ONE shared Replay frame for every suspended client
                        // (borrowed payload — see replay_frame): encodes the
                        // serialized grid once and lets `bytes` MOVE into the
                        // hint job instead of being cloned per consumer.
                        let rframe = replay_frame(id, &bytes);
                        if !raw_tail_replay {
                            let tail = j.tail();
                            if !tail.is_empty() {
                                let tail_base = j.absolute_len() - tail.len() as u64;
                                hint_job = Some((tail, tail_base, bytes));
                            }
                        }
                        for c in &suspended {
                            c.attached.lock().insert(id);
                            if let Some(f) = frame_bytes(&D2C::Reset { id }) {
                                c.enqueue(&f);
                            }
                            if let Some(f) = &rframe {
                                c.enqueue(f);
                            }
                            if let Some(f) = frame_bytes(&D2C::StreamPos { id, off: stream_off }) {
                                c.enqueue(&f);
                            }
                        }
                        drop(j);
                    }
                    // Blocks full sync after the resync (journal lock
                    // dropped; blocks is a leaf). Fixes a real P1 gap:
                    // launch() close_dangling()s BEFORE clients are suspended
                    // and never notified them, so a reconnected GUI kept a
                    // stale open record forever — wrongly disabling Re-run.
                    let full = {
                        let map = self.blocks.lock();
                        map.get(&id).map(|s| (s.epoch, s.recs.clone()))
                    };
                    // r4 perf-daemon LOW-2 (same shape as the attach arm):
                    // serialize ONCE from the moved recs — shared across all
                    // suspended clients — then recover them for the hint job.
                    let full = match full {
                        Some((epoch, recs)) => {
                            let msg = D2C::Blocks { id, epoch, full: true, recs };
                            if let Some(f) = frame_bytes(&msg) {
                                for c in &suspended {
                                    c.enqueue(&f);
                                }
                            }
                            let D2C::Blocks { recs, .. } = msg else { unreachable!() };
                            Some((epoch, recs))
                        }
                        None => None,
                    };
                    // Restored-history anchors: computed on the hint worker
                    // pool (r2 boot-perf 1 — crash recovery resyncs every
                    // terminal, previously ~29ms each serial on this thread);
                    // the GUI re-bases rows by its history growth since the
                    // Replay and re-verifies each hint against its grid.
                    if let (Some((tail, tail_base, replay)), Some((epoch, recs))) =
                        (hint_job, full.filter(|(e, _)| *e > 0))
                    {
                        let _ = epoch;
                        self.hints.submit(HintJob {
                            label: "resync",
                            id,
                            tail,
                            tail_base,
                            replay,
                            recs,
                            cols,
                            rows,
                            targets: suspended.iter().map(Arc::downgrade).collect(),
                        });
                    }
                }
            }
            Err(e) => {
                log::error!("spawn failed for {id}: {e}");
                // Their old view still stands; resubscribe before the error line.
                for c in &suspended {
                    c.attached.lock().insert(id);
                }
                self.emit_output(id, format!("\r\n\x1b[31mspawn failed: {e}\x1b[0m\r\n").as_bytes());
            }
        }
        self.broadcast_snapshot();
    }

    /// Fold a tracker pass into a Shell/Custom terminal, saving only on change
    /// (capture-on-change is the power-loss guarantee for restore metadata).
    /// Returns whether GUI-visible metadata (live_cwd / inner_cli) changed —
    /// the caller coalesces one Snapshot broadcast per tracker tick so the
    /// lane/row labels update without waiting for an unrelated broadcast
    /// (the stale-`cd`-label bug), without storming snapshots per terminal.
    fn apply_track_sample(&self, id: Uuid, sample: tracker::TrackSample) -> bool {
        let mut state = self.state.lock();
        let Some(t) = state.terminal_mut(id) else {
            return false;
        };
        if let TermKind::Claude { session_id, extra_args } = &t.kind {
            // Attribution Layer 1, Claude-KIND: the pid registry says the
            // LIVE conversation rotated away from the pin (in-TUI /clear or
            // /resume switch — the wrong-resume-after-sleep bug). Re-target
            // the pin NOW, while the evidence is a gated self-report of the
            // running process, instead of guessing at wake time.
            if let Some(sid) = sample.claude_live {
                // Report-freshness guard: the SessionStart hook fires
                // ~100ms before claude rewrites the registry file — a tick
                // in that gap reads the STALE registry and would flap the
                // just-reported pin backwards (see `claude_reports`).
                let stale_vs_report = self.claude_reports.lock().get(&id).is_some_and(
                    |(rep_sid, at)| {
                        *rep_sid != sid && now_ms().saturating_sub(*at) < CLAUDE_REPORT_FRESH_MS
                    },
                );
                if stale_vs_report {
                    return false;
                }
                if sid != *session_id {
                    log::info!(
                        "terminal {id}: claude session pin follows the live registry \
                         {session_id} -> {sid}"
                    );
                    t.kind = TermKind::Claude {
                        session_id: sid,
                        extra_args: extra_args.clone(),
                    };
                    state.save_logged("claude pin follows live registry");
                    return true;
                }
            }
            return false;
        }
        let mut changed = false;
        if let Some(cwd) = sample.live_cwd {
            if t.live_cwd.as_ref() != Some(&cwd) {
                t.live_cwd = Some(cwd);
                changed = true;
            }
        }
        // HOOK-STICKY EXPLICIT (codex, and any adapter without a live pid
        // registry): a SessionStart hook/beacon upgrades inner_cli to
        // Explicit(<live sid>). The Win32 tracker's birth-correlation is only
        // a LAUNCH heuristic — once the process is >30s old codex_extract can
        // no longer birth-match and returns Ambiguous(token:None), which would
        // otherwise CLOBBER the hook's Explicit id every tick (and, worse,
        // discard the in-TUI /resume switch the hook just captured). So a
        // same-adapter incoming sample that is NOT Explicit must not overwrite
        // an existing Explicit token; a genuinely new codex session re-fires
        // its own SessionStart and refines again. claude is unaffected — its
        // tracker sample is registry-Explicit, so this guard never trips.
        let keep_explicit = matches!(
            (&t.inner_cli, &sample.inner_cli),
            (Some(cur), Some(new))
                if cur.adapter == new.adapter
                    && cur.confidence == CliConfidence::Explicit
                    && new.confidence != CliConfidence::Explicit
        );
        if !keep_explicit && t.inner_cli != sample.inner_cli {
            t.inner_cli = sample.inner_cli;
            changed = true;
        }
        if changed {
            state.save_logged("inner_cli track sample");
        }
        changed
    }

    /// P6: cwd-only tracker fold for hook-fed families (WslShell P6a, Ssh
    /// P6c). The Win32 tracker's inner_cli verdict is meaningless there
    /// (Linux/remote processes are invisible to Toolhelp), so this must NOT
    /// touch inner_cli — the hook lifecycle in on_block_event owns it.
    /// Returns whether live_cwd changed (same coalesced-broadcast contract
    /// as apply_track_sample).
    fn apply_posix_cwd(&self, id: Uuid, cwd: Option<std::path::PathBuf>) -> bool {
        let Some(cwd) = cwd else { return false };
        let mut state = self.state.lock();
        let Some(t) = state.terminal_mut(id) else {
            return false;
        };
        if t.live_cwd.as_ref() != Some(&cwd) {
            t.live_cwd = Some(cwd);
            state.save_logged("live_cwd (posix fold)");
            return true;
        }
        false
    }

    /// Prompt-time live_cwd fold from a token-checked `pre` hook (Bug: the
    /// input-lane label went stale — apply_track_sample persisted the cwd
    /// without broadcasting, so the GUI label waited for an UNRELATED
    /// Snapshot). The hook is the freshest possible witness (it renders with
    /// the prompt itself); the caller broadcasts one Snapshot when this
    /// returns true. A `cd` changes the cwd exactly once, so this can never
    /// storm — Enter-spam at the same cwd is a no-op.
    fn apply_hook_cwd(&self, id: Uuid, cwd: std::path::PathBuf) -> bool {
        let mut state = self.state.lock();
        let Some(t) = state.terminal_mut(id) else {
            return false;
        };
        if matches!(t.kind, TermKind::Claude { .. }) {
            return false; // pinned-id terminals never track cwd
        }
        if t.live_cwd.as_ref() != Some(&cwd) {
            t.live_cwd = Some(cwd);
            state.save_logged("live_cwd (pre hook)");
            return true;
        }
        false
    }

    /// Set/clear a terminal's tracked inner CLI, saving on change
    /// (capture-on-change is the power-loss guarantee for restore metadata).
    /// Broadcasts ONE Snapshot on change: hook-fed families (WSL/ssh) skip
    /// the tracker tick's coalesced broadcast, so without this the GUI —
    /// including the Layer-3 consent trigger, which watches for a claude
    /// inner_cli appearing on an ssh terminal — waited for an unrelated
    /// Snapshot (the stale-label class). A CLI opens/closes once per run;
    /// this can never storm.
    fn set_inner_cli(&self, id: Uuid, inner: Option<crate::state::InnerCli>) {
        let changed = {
            let mut state = self.state.lock();
            let Some(t) = state.terminal_mut(id) else { return };
            if matches!(t.kind, TermKind::Claude { .. }) {
                return;
            }
            if t.inner_cli != inner {
                t.inner_cli = inner;
                state.save_logged("inner_cli set");
                true
            } else {
                false
            }
        };
        if changed {
            self.broadcast_snapshot();
        }
    }

    /// Backwards-compatible claude entry point (Layer 1 WSL registry, the
    /// Claude-KIND registry re-pin). Thin wrapper over `apply_cli_session`.
    fn apply_claude_session(&self, id: Uuid, sid: Uuid, why: &str) -> bool {
        self.apply_cli_session(id, "claude", sid, why)
    }

    /// Attribution: fold a CLI SELF-REPORTED session id (claude Layer 1 WSL
    /// registry / Layer 2 hook report / Layer 3 tcbeacon; codex Layer 2 hook
    /// report / Layer 3 tcbeacon) into a terminal. For `adapter == "claude"`,
    /// Claude-KIND terminals re-target the pin. For any adapter, a Shell/Custom
    /// terminal with a tracked SAME-adapter inner_cli upgrades its resume token
    /// to Explicit (the CLI itself named the session). Anything else is a
    /// no-op — a report can never CREATE cli state, only refine it (the
    /// tracker establishes the inner_cli before the first prompt where the
    /// hook fires). Change-gated + persisted; returns whether anything changed
    /// (the caller broadcasts or coalesces).
    fn apply_cli_session(&self, id: Uuid, adapter: &str, sid: Uuid, why: &str) -> bool {
        // Freshness stamp FIRST (even for no-op applies): a registry read
        // that contradicts this report inside CLAUDE_REPORT_FRESH_MS is a
        // stale-file artifact and must not flap the pin (see
        // `claude_reports`). The WSL-registry caller stamps too — harmless:
        // no other witness ticks those terminals.
        self.claude_reports.lock().insert(id, (sid, now_ms()));
        let mut state = self.state.lock();
        let Some(t) = state.terminal_mut(id) else {
            return false;
        };
        match &t.kind {
            TermKind::Claude { session_id, extra_args } if adapter == "claude" => {
                if *session_id == sid {
                    return false;
                }
                log::info!(
                    "terminal {id}: claude session pin {session_id} -> {sid} ({why})"
                );
                t.kind = TermKind::Claude {
                    session_id: sid,
                    extra_args: extra_args.clone(),
                };
            }
            _ => {
                let Some(cli) = &t.inner_cli else { return false };
                if cli.adapter != adapter {
                    return false;
                }
                let sid_s = sid.to_string();
                if cli.resume_token.as_deref() == Some(sid_s.as_str())
                    && cli.confidence == CliConfidence::Explicit
                {
                    return false;
                }
                log::info!("terminal {id}: {adapter} inner-cli session -> {sid} ({why})");
                let mut cli = cli.clone();
                cli.resume_token = Some(sid_s);
                cli.confidence = CliConfidence::Explicit;
                t.inner_cli = Some(cli);
            }
        }
        state.save_logged("explicit inner_cli (report/beacon)");
        true
    }

    /// Attribution Layer 3: a `tcbeacon` OSC scanned out of this terminal's
    /// own output stream (the consent-installed remote hook script printing
    /// to /dev/tty). Advisory-trust — the beacon carries no rotating token,
    /// so it is believed only when (a) THIS spawn's bootstrap proved itself
    /// (hooks_live), (b) a same-adapter exec block was observed and is still
    /// open (cli_blocks + the adapter gate inside apply_cli_session), and (c)
    /// the payload is uuid-shaped. Spoofing past those gates requires
    /// same-user code already running inside the session — the Warp-hooks
    /// trust stance, same as the block hooks. `adapter` selects the target
    /// (claude vs codex); a beacon whose adapter doesn't match the open CLI
    /// block's inner_cli is a no-op.
    fn on_beacon(&self, id: Uuid, adapter: &str, event: &str, source: &str, sid: &str) {
        let Ok(sid) = Uuid::parse_str(sid) else {
            log::debug!("terminal {id}: tcbeacon with a non-uuid session dropped");
            return;
        };
        if !self.blocks.lock().get(&id).is_some_and(|s| s.hooks_live) {
            log::debug!("terminal {id}: tcbeacon before hooks_live dropped");
            return;
        }
        if !self.cli_blocks.lock().contains_key(&id) {
            log::debug!("terminal {id}: tcbeacon without an open CLI block dropped");
            return;
        }
        if event != "SessionStart" {
            // claude's SessionEnd(clear|resume) is the transient half of a
            // switch — the paired SessionStart lands ~200ms later; other
            // ends mean the CLI is exiting and the block-close lifecycle owns
            // clearing inner_cli. codex has no SessionEnd. Either way:
            // observe, never mutate.
            log::debug!("terminal {id}: tcbeacon {adapter} {event} (source={source}) ignored");
            return;
        }
        if self.apply_cli_session(id, adapter, sid, "tcbeacon") {
            log::info!("terminal {id}: tcbeacon {adapter} session {sid} (source={source})");
            self.broadcast_snapshot();
        }
    }

    /// Attribution Layer 1, WSL leg: refine an open claude inner_cli's
    /// token from the distro's own pid registry
    /// (`\\wsl$\<distro>\<home>\.claude\sessions`, liveness = /proc over
    /// the same mount, exactly-one-in-cwd or nothing). Runs on the tracker
    /// tick for hook-fed WslShell terminals, throttled to one UNC scan per
    /// 2s per terminal. 2.1.91-era registries are launch snapshots (they
    /// never follow /clear) — this leg fixes launch identity; the injected
    /// hooks are the live belt where they exist. Returns whether meta
    /// changed (the tick coalesces one Snapshot).
    fn refresh_wsl_claude(&self, id: Uuid) -> bool {
        // R3-3: a fresh hook self-report is strictly better evidence than a
        // registry scan — skip the UNC leg entirely while the belt is live.
        // A WORKING WSL claude (active tick every 300ms) otherwise pays
        // ~1800 redundant \\wsl$ scans/hour (read_dir + ~a dozen json/proc
        // probes over P9 each) with the sid not rotating. Hook-less
        // old-registry claudes never report, so they keep the full 2s
        // cadence; a /clear on a hooked claude is re-announced by the hook
        // itself in ~ms.
        {
            let reports = self.claude_reports.lock();
            if reports
                .get(&id)
                .is_some_and(|(_, ms)| now_ms().saturating_sub(*ms) < 60_000)
            {
                return false;
            }
        }
        {
            let mut next = self.wsl_reg_next.lock();
            let now = Instant::now();
            match next.get(&id) {
                Some(t) if *t > now => return false,
                _ => {
                    next.insert(id, now + Duration::from_secs(2));
                }
            }
        }
        let (distro, cwd) = {
            let state = self.state.lock();
            let Some(t) = state.terminal(id) else { return false };
            let crate::state::ShellFamily::WslShell { distro } =
                crate::state::shell_family(&t.kind, &t.program, &t.args)
            else {
                return false;
            };
            let Some(cli) = &t.inner_cli else { return false };
            if cli.adapter != "claude" {
                return false;
            }
            (distro, cli.cwd.to_string_lossy().into_owned())
        };
        let Some(home) = self.hook_homes.lock().get(&id).cloned() else {
            return false;
        };
        let Some(sid) = claude_registry::wsl_live_claude(distro.as_deref(), &home, &cwd)
        else {
            return false;
        };
        self.apply_claude_session(id, sid, "wsl registry")
    }

    /// Ids of terminals whose family is hook-fed (P6 §7.1 tracker routing:
    /// WslShell + Ssh): their cwd comes from the OSC 9;9 hook alone (the PEB
    /// of wsl.exe is meaningless; ssh's remote world has no PEB at all), the
    /// Toolhelp descendant walk is skipped (Linux/remote procs are
    /// invisible), and inner_cli is owned by the hook lifecycle.
    fn hook_fed_family_ids(&self) -> HashSet<Uuid> {
        self.state
            .lock()
            .terminals
            .iter()
            .filter(|t| {
                matches!(
                    crate::state::shell_family(&t.kind, &t.program, &t.args),
                    crate::state::ShellFamily::WslShell { .. }
                        | crate::state::ShellFamily::Ssh { .. }
                )
            })
            .map(|t| t.id)
            .collect()
    }

    /// P6 §7.2: an exec hook line from a hook-fed family (WslShell P6a, Ssh
    /// P6c), parsed against the same adapter registry the Win32 tracker uses.
    /// Explicit tokens (`claude --resume <uuid>`) come out exactly like the
    /// PEB path; bare launches degrade to Ambiguous (WSL upgrades via \\wsl$
    /// correlation in §7.3; ssh has no filesystem to correlate — Ambiguous is
    /// the honest ceiling, §3.4.3).
    fn track_hook_exec(
        &self,
        id: Uuid,
        epoch: u32,
        start_off: u64,
        cmd: &str,
        hook_cwd: Option<std::path::PathBuf>,
    ) {
        let (cwd, is_ssh, program, args) = {
            let state = self.state.lock();
            let Some(t) = state.terminal(id) else { return };
            let family = crate::state::shell_family(&t.kind, &t.program, &t.args);
            if !matches!(
                family,
                crate::state::ShellFamily::WslShell { .. }
                    | crate::state::ShellFamily::Ssh { .. }
            ) {
                return; // pwsh/cmd keep the Win32 tracker
            }
            (
                hook_cwd
                    .or_else(|| t.live_cwd.clone())
                    .unwrap_or_else(|| t.cwd.clone()),
                matches!(family, crate::state::ShellFamily::Ssh { .. }),
                t.program.clone(),
                t.args.clone(),
            )
        };
        let Some(inner) = tracker::analyze_cmdline(cmd, &cwd) else {
            return;
        };
        self.cli_blocks.lock().insert(id, (epoch, start_off));
        // Ssh only (remote-resume): persist the blocks sidecar NOW for this
        // OPEN record (opens ordinarily persist on close). The remote-resume
        // sidecar-validity gate re-joins probes\<id>.json to THIS record
        // across a daemon restart (spec §10 "daemon restart between M0 and
        // M3") — and a CLI block is open at death by definition (the
        // hour-long-claude case). Without this, a graceful shutdown/power
        // loss dropped the open rec, the gate failed, and restores degraded
        // to the §5.2 fallback (probe ssh_cli_resume's staggered acceptance
        // pinned it). WSL has no probe sidecar, so it keeps close-time saves.
        if is_ssh {
            let snap = self.blocks.lock().get(&id).cloned();
            if let Some(s) = snap {
                s.save(id);
            }
        }
        // M0 snapshot leg (remote-cli-resume-spec §4): a BARE launch of a
        // remote-correlatable adapter over ssh gets its store listed ONCE on
        // a worker thread and persisted as the probes\<id>.json sidecar —
        // the diff basis the restore-time correlate leg consumes. Explicit
        // launches carry their own token and are never probed here.
        if is_ssh {
            remote_probe::spawn_snapshot_leg(self, id, &inner, program, args, (epoch, start_off));
        }
        self.set_inner_cli(id, Some(inner));
    }

    /// Create + launch a terminal, returning its id. Shared by the legacy
    /// C2D::CreateTerminal arm and the controller's CreateTerminal (which
    /// must reply Created{id} — the legacy arm can't return it).
    fn create_terminal_inner(self: &Arc<Self>, spec: crate::state::NewTerminal) -> Uuid {
        let id = Uuid::new_v4();
        {
            let mut state = self.state.lock();
            let order = state.alloc_order();
            state.terminals.push(TerminalMeta {
                id,
                name: spec.name,
                folder: spec.folder,
                kind: spec.kind,
                program: spec.program,
                args: spec.args,
                cwd: spec.cwd,
                order,
                auto_restore: true,
                launched_once: spec.already_launched,
                status: TermStatus::Dead,
                last_cols: 0,
                last_rows: 0,
                live_cwd: None,
                inner_cli: None,
                hooked: false, // launch() sets the real verdict
                shell_cfg: spec.shell_cfg,
                color_tag: None,
                asleep: false,
                reconnecting: false,
            });
            if let Err(e) = state.save() {
                drop(state);
                self.report_error(&format!("failed to save state: {e}"));
            }
        }
        self.launch(id);
        id
    }

    /// The full deletion sequence, state-FIRST (journal-resurrection incident
    /// class: once the terminal is gone from state, journal() refuses to
    /// lazily re-create its file, so the killed process's reader thread —
    /// which keeps draining buffered ConPTY output for a while — can no
    /// longer resurrect the journal). Shared legacy/controller.
    fn delete_terminal_inner(&self, id: Uuid) {
        self.reconnects.lock().remove(&id);
        self.mutate(|s| s.terminals.retain(|t| t.id != id));
        // F1: every parked waiter for this id (ALL kinds — Exit included)
        // fails "deleted" NOW. on_exit early-returns for a deleted id, so
        // nothing else can ever resolve them; without this sweep a
        // `pulse-ctl wait`/`run --wait` parks to its full timeout and then
        // lies "timeout". AFTER the state mutate, so a wait registering
        // behind us hits not_found — and the post-push `claim_back_if_deleted`
        // re-check covers a push that lands after this sweep.
        self.fail_all_waiters_for(id, "deleted", "the terminal was deleted");
        // Bound the removed Session so the kill AND the Session drop (ConPTY
        // close) run outside the sessions mutex.
        let removed = self.sessions.lock().remove(&id);
        if let Some(mut s) = removed {
            self.mark_expected_exit(id);
            let _ = s.killer.kill();
        }
        self.journals.lock().remove(&id);
        self.fast_exits.lock().remove(&id);
        self.blocks.lock().remove(&id);
        self.hook_homes.lock().remove(&id);
        self.wsl_reg_next.lock().remove(&id);
        self.claude_reports.lock().remove(&id);
        // r2-M4: these two were missed — tiny entries, but forever.
        self.cli_blocks.lock().remove(&id);
        self.spawn_times.lock().remove(&id);
        blocks::BlockStore::delete_sidecar(id);
        remote_probe::delete_sidecar(id);
        bootstrap::delete_script(id);
        frame::remove(id);
        Journal::delete(id);
    }

    /// One block's output as clean text: journal range (fresh handle),
    /// ANSI/OSC-stripped, size-capped. Shared by the legacy C2D::BlockText
    /// arm, the controller's ReadBlockText, and RunDone assembly — one
    /// implementation, zero drift. Called with NO other lock held.
    fn block_text(&self, id: Uuid, rec: &crate::state::BlockRec) -> (String, bool) {
        let Ok(journal) = self.journal(id) else {
            return (String::new(), false);
        };
        let (raw, clipped) = {
            let j = journal.lock();
            // An open block extends to the stream head.
            let end = rec.end_off.unwrap_or_else(|| j.absolute_len());
            j.read_range(rec.start_off, end, BLOCK_TEXT_RAW_CAP)
        };
        // The range is pure command output: start_off is just after the exec
        // hook's OSC terminator (after the command echo + newline) and
        // end_off just after the closing pre hook, which the bootstrap emits
        // BEFORE any prompt text. The stripper removes SGR noise and the
        // hook OSCs themselves; byte-level stripping + one lossy decode keeps
        // multi-byte UTF-8 intact.
        let mut stripped = Vec::with_capacity(raw.len());
        let mut stripper = crate::strip::AnsiStripper::default();
        stripper.feed_bytes(&raw, &mut stripped);
        let mut text = String::from_utf8_lossy(&stripped).into_owned();
        let mut truncated = clipped || rec.truncated;
        if text.len() > BLOCK_TEXT_CAP {
            // Cut at a char boundary.
            let mut cut = BLOCK_TEXT_CAP;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            truncated = true;
        }
        (text, truncated)
    }

    fn handle_message(self: &Arc<Self>, client: &Arc<ClientConn>, msg: C2D) {
        // Scoped controllers speak Ctl (and Ping) ONLY — the legacy verbs are
        // the GUI protocol and stay master-token territory. One guard, so no
        // legacy arm can ever forget it.
        if client.scope != SCOPE_FULL {
            match &msg {
                C2D::Ping | C2D::Ctl { .. } | C2D::HelloCtl { .. } => {}
                _ => {
                    log::warn!("scoped controller sent a non-Ctl frame; dropped");
                    return;
                }
            }
        }
        match msg {
            C2D::Hello { .. } => {}
            C2D::Ping => {
                if let Some(f) = frame_bytes(&D2C::Pong) {
                    client.enqueue(&f);
                }
            }

            C2D::CreateFolder { name } => {
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
            }
            C2D::RenameFolder { id, name } => {
                self.mutate(|s| {
                    if let Some(f) = s.folders.iter_mut().find(|f| f.id == id) {
                        f.name = name;
                    }
                });
            }
            C2D::DeleteFolder { id } => {
                self.mutate(|s| {
                    s.folders.retain(|f| f.id != id);
                    for t in s.terminals.iter_mut().filter(|t| t.folder == Some(id)) {
                        t.folder = None;
                    }
                });
            }
            C2D::SetFolderCollapsed { id, collapsed } => {
                self.mutate(|s| {
                    if let Some(f) = s.folders.iter_mut().find(|f| f.id == id) {
                        f.collapsed = collapsed;
                    }
                });
            }
            C2D::MoveFolder { id, delta } => {
                self.mutate(|s| {
                    s.folders.sort_by_key(|f| f.order);
                    if let Some(pos) = s.folders.iter().position(|f| f.id == id) {
                        let new_pos = (pos as i64 + delta as i64)
                            .clamp(0, s.folders.len() as i64 - 1) as usize;
                        let f = s.folders.remove(pos);
                        s.folders.insert(new_pos, f);
                        for (i, f) in s.folders.iter_mut().enumerate() {
                            f.order = i as i64;
                        }
                    }
                });
            }

            C2D::CreateTerminal { spec } => {
                self.create_terminal_inner(spec);
            }
            C2D::RenameTerminal { id, name } => {
                self.mutate(|s| {
                    if let Some(t) = s.terminal_mut(id) {
                        t.name = name;
                    }
                });
            }
            C2D::MoveTerminal { id, folder } => {
                self.mutate(|s| {
                    if let Some(t) = s.terminal_mut(id) {
                        t.folder = folder;
                    }
                });
            }
            C2D::ReorderTerminal { id, delta } => {
                self.mutate(|s| {
                    let folder = s.terminal(id).and_then(|t| t.folder);
                    let mut group: Vec<Uuid> = {
                        let mut g: Vec<&TerminalMeta> =
                            s.terminals.iter().filter(|t| t.folder == folder).collect();
                        g.sort_by_key(|t| t.order);
                        g.iter().map(|t| t.id).collect()
                    };
                    if let Some(pos) = group.iter().position(|&t| t == id) {
                        let new_pos =
                            (pos as i64 + delta as i64).clamp(0, group.len() as i64 - 1) as usize;
                        let t = group.remove(pos);
                        group.insert(new_pos, t);
                        for (i, tid) in group.iter().enumerate() {
                            // Re-pack orders within the group, keeping global uniqueness loose.
                            if let Some(term) = s.terminal_mut(*tid) {
                                term.order = i as i64;
                            }
                        }
                    }
                });
            }
            C2D::SetAutoRestore { id, auto } => {
                self.mutate(|s| {
                    if let Some(t) = s.terminal_mut(id) {
                        t.auto_restore = auto;
                    }
                });
            }
            C2D::SetColorTag { id, tag } => {
                self.mutate(|s| {
                    if let Some(t) = s.terminal_mut(id) {
                        t.color_tag = tag;
                    }
                });
            }
            C2D::SetFolderColor { id, tag } => {
                self.mutate(|s| {
                    if let Some(f) = s.folders.iter_mut().find(|f| f.id == id) {
                        f.color_tag = tag;
                    }
                });
            }
            C2D::DeleteTerminal { id } => {
                self.delete_terminal_inner(id);
            }
            C2D::RestartTerminal { id } => self.launch_from_conn(id),
            C2D::KillTerminal { id } => {
                self.cancel_reconnect(id);
                // Killer cloned under the sessions lock, TerminateProcess
                // outside it (the C2D::Input if-let-temporary class).
                let killer = self.sessions.lock().get(&id).map(|s| s.killer.clone_killer());
                if let Some(mut k) = killer {
                    self.mark_expected_exit(id);
                    let _ = k.kill();
                }
            }

            C2D::Attach { id, cols, rows } => {
                // Bring the session to the attacher's grid size FIRST (0 =
                // unknown, skip): serialization then happens at the client's
                // own height, so the relative cursor placement is exact — a
                // shorter client would otherwise clamp cursor-up moves at its
                // viewport top (the "floating cursor" bug) — and the resize
                // itself makes the shell repaint into the new geometry.
                if cols >= 2 && rows >= 2 {
                    self.do_resize(id, cols, rows);
                }
                // Grid for a dead-terminal reconstruction: the attacher's
                // size, else the terminal's last known size (launch()'s
                // fallback rules). Read from state BEFORE the journal lock —
                // state must never be taken under a journal lock.
                let (eff_cols, eff_rows) = if cols >= 2 && rows >= 2 {
                    (cols.clamp(2, 1000), rows.clamp(2, 1000))
                } else {
                    let state = self.state.lock();
                    state
                        .terminal(id)
                        .filter(|t| t.last_cols >= 2 && t.last_rows >= 2)
                        .map(|t| (t.last_cols.clamp(2, 1000), t.last_rows.clamp(2, 1000)))
                        .unwrap_or((session::DEFAULT_COLS, session::DEFAULT_ROWS))
                };
                // Live primary-screen sessions get a serialized grid
                // reconstruction (preface + mirror, seam-free). Dead
                // terminals get the same reconstruction from a scratch-parse
                // of their journal tail (perf-wave-3 — see serialize_dead;
                // a tail that ends inside the alt screen reconstructs the
                // restored PRIMARY grid since the render-bugs pass). LIVE
                // alt-screen falls back to the raw journal tail — cut-safe
                // via alt_tail_for_live, or a tail cut inside the alt region
                // paints TUI frames onto the attacher's primary grid (the
                // "claude remnants fused with prompts" artifact).
                let arcs = self.sessions.lock().get(&id).map(|s| {
                    (
                        s.term.clone(),
                        s.preface.clone(),
                        s.win32_input.clone(),
                        s.prompt.clone(),
                        s.last_output.clone(),
                    )
                });
                // D14 family verdict, read BEFORE the journal lock (state
                // must never be taken under a journal lock).
                let cmd_family = arcs.is_some() && self.is_cmd_family(id);
                // SLEEP freeze-frame: the asleep flag, read before the
                // journal lock for the same reason. Only a DEAD session can
                // present Asleep, so the live arm never pays the read.
                let asleep = arcs.is_none()
                    && self.state.lock().terminal(id).is_some_and(|t| t.asleep);
                // Restored-history hints (proto 7): inputs snapshotted under
                // the journal lock, computed AFTER it drops (the mapping
                // parse is milliseconds — too long for the lock), enqueued at
                // the very end of the attach sequence. The GUI re-bases the
                // rows by its own history growth, so Output frames slipping
                // in between are harmless.
                // (journal tail, tail base offset, replay bytes, block recs).
                type HintInputs = (Vec<u8>, u64, Vec<u8>, Vec<crate::state::BlockRec>);
                let mut hint_job: Option<HintInputs> = None;
                if let Ok(journal) = self.journal(id) {
                    // The journal lock spans snapshot + subscribe + Replay
                    // enqueue; ingest holds it across parse+fanout, so the
                    // serialized state and the live stream can neither miss
                    // nor double-apply a chunk.
                    let j = journal.lock();
                    let ser_t0 = perf::on().then(Instant::now);
                    // Raw-tail replays (live alt-screen; the never-engaging
                    // dead-alt belt) get no hints: their bytes are not a
                    // grid serialization, so replay rows are not derivable
                    // (edge honesty — TUI frames carry no prompt rows).
                    let mut raw_tail_replay = false;
                    // Dead-arm tail, kept for the hint job below: the same
                    // lock hold means a re-read would return identical bytes
                    // — at 2MB apiece, a GUI boot attaching ~20 dead
                    // terminals paid the read twice each.
                    let mut dead_tail: Option<Vec<u8>> = None;
                    // SLEEP freeze-frame overlay engaged: the serialized
                    // scrollback underlay is topped with `?1049h` + the
                    // pre-kill frame — live-TUI semantics, so hints (whose
                    // rows point into the now-hidden primary grid) are
                    // skipped, exactly like the live alt-screen arm.
                    let mut frame_overlay = false;
                    let mut bytes = match &arcs {
                        Some((term, preface, _, _, _)) => {
                            let t = term.lock();
                            if serialize::is_alt_screen(&t) {
                                raw_tail_replay = true;
                                serialize::alt_tail_for_live(j.tail())
                            } else {
                                serialize::serialize_term(&t, Some(&preface.lock()))
                            }
                        }
                        None => {
                            let tail = j.tail();
                            match serialize::serialize_dead(&tail, eff_cols, eff_rows) {
                                Some(mut b) => {
                                    // Asleep + a valid frame sidecar: replay
                                    // the frozen alt frame over the underlay.
                                    // Missing/corrupt frame (frame::read
                                    // removes corrupt files) = exactly the
                                    // pre-freeze behavior.
                                    if asleep {
                                        if let Some(f) = frame::read(id).filter(|f| f.alt) {
                                            b.extend_from_slice(b"\x1b[?1049h");
                                            b.extend_from_slice(&f.bytes);
                                            frame_overlay = true;
                                        }
                                    }
                                    dead_tail = Some(tail);
                                    b
                                }
                                None => {
                                    raw_tail_replay = true;
                                    tail
                                }
                            }
                        }
                    };
                    if let Some(t0) = ser_t0 {
                        log::info!(
                            "[perf] attach serialize id={id} bytes={} us={}",
                            bytes.len(),
                            t0.elapsed().as_micros()
                        );
                    }
                    // Re-assert win32-input-mode for the attaching client:
                    // private mode 9001 isn't tracked by the mirror Term, and
                    // the client's key encoder keys off seeing it.
                    if arcs
                        .as_ref()
                        .is_some_and(|(_, _, w, _, _)| w.load(Ordering::Relaxed))
                    {
                        bytes.extend_from_slice(b"\x1b[?9001h");
                    }
                    // Absolute stream offset where live Output resumes for
                    // this client. Ingest holds this same journal lock across
                    // append+fanout, so absolute_len here is exactly where
                    // the first post-Replay Output frame begins.
                    let stream_off = j.absolute_len();
                    // Block-store snapshot (read under the journal lock like
                    // everything else in this sequence; blocks is a leaf
                    // lock, journal→blocks nesting is one-way).
                    let full = {
                        let map = self.blocks.lock();
                        map.get(&id).map(|s| (s.epoch, s.recs.clone()))
                    };
                    // r4 perf-daemon LOW-2: serialize the Blocks frame NOW
                    // from the MOVED recs, then recover them for the hint
                    // job below — one recs clone per attach (the map copy)
                    // instead of two. Enqueue position is unchanged (Replay
                    // → StreamPos → Blocks, all under this journal lock).
                    let (blocks_frame, mut full) = match full {
                        Some((epoch, recs)) => {
                            let msg = D2C::Blocks { id, epoch, full: true, recs };
                            let f = frame_bytes(&msg);
                            let D2C::Blocks { recs, .. } = msg else { unreachable!() };
                            (f, Some((epoch, recs)))
                        }
                        None => (None, None),
                    };
                    // Hint inputs (proto 7): hooked sessions with a
                    // grid-serialized replay. The tail + base are snapshotted
                    // under this same lock so they are exactly the bytes the
                    // replay reflects. The Replay frame is built from a
                    // BORROW of `bytes` (replay_frame), so the serialized
                    // grid MOVES into the hint job — no clone; and the dead
                    // arm's already-read tail is reused instead of re-read.
                    let rframe = replay_frame(id, &bytes);
                    if !raw_tail_replay && !frame_overlay && full.as_ref().is_some_and(|(e, _)| *e > 0) {
                        let tail = dead_tail.take().unwrap_or_else(|| j.tail());
                        if !tail.is_empty() {
                            let tail_base = j.absolute_len() - tail.len() as u64;
                            // The frame bytes are already built — the recs
                            // MOVE into the hint job (LOW-2).
                            let (_, recs) = full.take().expect("checked is_some above");
                            hint_job = Some((tail, tail_base, bytes, recs));
                        }
                    }
                    client.attached.lock().insert(id);
                    if let Some(f) = rframe {
                        client.enqueue(&f);
                    }
                    if let Some(f) = frame_bytes(&D2C::StreamPos { id, off: stream_off }) {
                        client.enqueue(&f);
                    }
                    // Full block sync, still under the journal lock so the
                    // queue order is Replay → StreamPos → Blocks → live
                    // Output: the epoch that enables the GUI's scanner must
                    // reach it before the first live hook byte.
                    if let Some(f) = blocks_frame {
                        client.enqueue(&f);
                    }
                    // Cold-attach prompt certification (task #15): enqueued
                    // AFTER Replay/StreamPos/Blocks so the GUI has the mirror
                    // reconstructed and the scanner enabled before it seeds.
                    // Computed in the POST-resize mirror, so line/col land in
                    // the just-serialized coordinate space (the client resized
                    // us first, then we serialized; the reconstructed cursor
                    // sits at exactly this cell). at_prompt = a 133;B fired
                    // with no command running; clean = the cursor hasn't moved
                    // off the prompt end since (empty input buffer). prompt_end
                    // carries the CLEAN column (not the typed cursor) so a
                    // dirty prompt's replayed cursor won't match it GUI-side
                    // and stays manual-arm. Skipped for alt-screen (raw tail
                    // replay, no meaningful prompt cell).
                    if let Some((term, _, _, prompt, last_output)) = &arcs {
                        let mark = *prompt.lock();
                        let open_none = self
                            .blocks
                            .lock()
                            .get(&id)
                            .map(|s| s.open.is_none())
                            .unwrap_or(false);
                        let t = term.lock();
                        let alt = serialize::is_alt_screen(&t);
                        let cur = t.grid().cursor.point;
                        let (cur_line, cur_col) = (cur.line.0, cur.column.0);
                        drop(t);
                        let at_prompt = !alt && mark.is_some() && open_none;
                        let (pe_col, mut clean) = match mark {
                            Some(col) => (col, at_prompt && col == cur_col),
                            None => (cur_col, false),
                        };
                        // D14 (P6b): cmd's prompt latch is weak — no exec
                        // hook ever clears it while a typed command runs, so
                        // a `clean` verdict additionally needs the session
                        // output-quiet ≥300ms (the cursor-col match above is
                        // already required). The GUI's own cursor_clean stays
                        // the cover gate either way; this keeps the certified
                        // bit honest for probes and cold arms.
                        if clean && cmd_family {
                            clean = now_ms()
                                .saturating_sub(last_output.load(Ordering::Relaxed))
                                >= CMD_QUIET_MS;
                        }
                        if let Some(f) = frame_bytes(&D2C::PromptState {
                            id,
                            at_prompt,
                            line: cur_line,
                            col: pe_col as u32,
                            clean,
                        }) {
                            client.enqueue(&f);
                        }
                    }
                }
                // Restored-history anchors (proto 7): computed on the hint
                // worker pool (r2 boot-perf 1 — ~25ms per hooked terminal,
                // previously serial on this one conn thread for the whole
                // boot attach cycle) and enqueued whenever ready. Output
                // frames may land first; the GUI re-bases hint rows by its
                // history growth since the Replay parse and re-verifies each
                // hint against its grid.
                if let Some((tail, tail_base, replay, recs)) = hint_job {
                    self.hints.submit(HintJob {
                        label: "attach",
                        id,
                        tail,
                        tail_base,
                        replay,
                        recs,
                        cols: eff_cols,
                        rows: eff_rows,
                        targets: vec![Arc::downgrade(client)],
                    });
                }
            }
            C2D::Detach { id } => {
                client.attached.lock().remove(&id);
            }
            C2D::Input { id, bytes } => {
                // Clone the writer Arc out and write OUTSIDE the sessions
                // mutex (SubmitCommand's pattern): a full ConPTY input pipe
                // (app stopped reading stdin) blocks write_all indefinitely,
                // and holding the global guard across it would wedge every
                // terminal's input plus every sessions-taking thread.
                let writer = self.sessions.lock().get(&id).map(|s| s.writer.clone());
                if let Some(w) = writer {
                    let mut w = w.lock();
                    let _ = w.write_all(&bytes);
                    let _ = w.flush();
                }
            }
            C2D::Resize { id, cols, rows } => self.do_resize(id, cols, rows),

            C2D::DebugDump => {
                // Probe support: write every live session's three size views
                // (headless Term, PTY, persisted state) to a file the probe
                // polls. File-based so the GUI's D2C match needs no new arm.
                let state_dims: HashMap<Uuid, (u16, u16)> = self
                    .state
                    .lock()
                    .terminals
                    .iter()
                    .map(|t| (t.id, (t.last_cols, t.last_rows)))
                    .collect();
                let mut entries = Vec::new();
                {
                    let sessions = self.sessions.lock();
                    for (sid, s) in sessions.iter() {
                        let (term_cols, term_rows) = {
                            use alacritty_terminal::grid::Dimensions;
                            let t = s.term.lock();
                            (t.columns() as u16, t.screen_lines() as u16)
                        };
                        let (pty_cols, pty_rows) = s
                            .master
                            .lock()
                            .get_size()
                            .map(|sz| (sz.cols, sz.rows))
                            .unwrap_or((0, 0));
                        let (state_cols, state_rows) =
                            state_dims.get(sid).copied().unwrap_or((0, 0));
                        entries.push(crate::protocol::DebugTermInfo {
                            id: *sid,
                            term_cols,
                            term_rows,
                            pty_cols,
                            pty_rows,
                            state_cols,
                            state_rows,
                        });
                    }
                }
                // Atomic swap so the probe never reads a half-written file.
                let path = data_dir().join("debug_dump.json");
                let tmp = data_dir().join("debug_dump.json.tmp");
                if let Ok(data) = serde_json::to_vec_pretty(&entries) {
                    if std::fs::write(&tmp, data).is_ok() {
                        let _ = std::fs::rename(&tmp, &path);
                    }
                }
            }

            C2D::BlockText { id, start_off } => {
                // Leaf blocks lock: find the record; clone; drop the lock.
                let rec = self.blocks.lock().get(&id).and_then(|s| {
                    s.recs.iter().find(|r| r.start_off == start_off).cloned()
                });
                let Some(rec) = rec else {
                    log::debug!("BlockText: unknown block {start_off} for {id}");
                    return;
                };
                let (text, truncated) = self.block_text(id, &rec);
                if let Some(f) = frame_bytes(&D2C::BlockText {
                    id,
                    start_off,
                    text,
                    truncated,
                }) {
                    client.enqueue(&f); // requester only — a reply, not a broadcast
                }
            }

            C2D::Shutdown => {
                log::info!("shutdown requested");
                let sd_t0 = perf::on().then(Instant::now);
                // 100%-persistence contract: conhost renders text on an
                // ASYNCHRONOUS frame, so output the shell already produced
                // (e.g. the tail rows of an `ls` run moments before an
                // --install) can still be in the ConPTY pipe when the frame
                // arrives. Exiting immediately dropped those bytes on the
                // floor — the journal truncated mid-table and the restore
                // replayed the loss forever (field-confirmed 3×). Drain
                // until every session has been output-quiet before flushing.
                self.drain_output_tail(Duration::from_millis(2000));
                let drain_done = sd_t0.map(|t| t.elapsed().as_millis());
                self.flush_all();
                if let (Some(t0), Some(d)) = (sd_t0, drain_done) {
                    log::info!(
                        "[perf] shutdown drain_ms={d} flush_ms={}",
                        t0.elapsed().as_millis() as u64 - d as u64
                    );
                }
                // The LAST write before the process dies: a failure here
                // (disk full, AV lock) is invisible until the user reboots
                // into wrong terminals — log it, always (C1).
                self.state.lock().save_logged("shutdown final save");
                std::process::exit(0);
            }

            // P5 controller channel. HelloCtl is only valid as the first
            // frame (consumed by handle_client); ignore it here like Hello.
            C2D::HelloCtl { .. } => {}
            C2D::Ctl { req_id, req } => self.handle_ctl(client, req_id, req),

            // P6b §5.2: the submission ledger for exec-less shells (cmd).
            C2D::SubmitCommand { id, cmd, write } => {
                self.submit_command(client, id, cmd, write)
            }

            // SLEEP (proto 9): the GUI's post-confirm verbs. No busy gate
            // here — the GUI gate/confirm modal already ran (§7.1); the
            // shared core re-filters to presented-Running anyway. Worker
            // thread per S19: handle_client reads frames sequentially, and
            // a 2s inline drain would freeze typing in every other terminal
            // on this connection.
            C2D::SleepTerminal { id } => {
                let core = self.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("sleep-{}", &id.to_string()[..8]))
                    .spawn(move || {
                        let _ = catch_unwind(AssertUnwindSafe(move || {
                            core.sleep_terminals(&[id]);
                        }));
                    });
            }
            C2D::SleepFolder { folder } => {
                let core = self.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("sleep-f-{}", &folder.to_string()[..8]))
                    .spawn(move || {
                        let _ = catch_unwind(AssertUnwindSafe(move || {
                            let members = core.folder_sleep_members(folder);
                            core.sleep_terminals(&members);
                        }));
                    });
            }
            C2D::WakeFolder { folder } => {
                let members = self.folder_wake_members(folder);
                self.wake_staggered(members);
            }
            C2D::CancelReconnect { id } => {
                self.cancel_reconnect(id);
            }
            C2D::SetAutoReconnect { id, on } => {
                self.mutate(|s| {
                    if let Some(t) = s.terminal_mut(id) {
                        t.shell_cfg.get_or_insert_with(Default::default).auto_reconnect = on;
                    }
                });
                if !on {
                    self.cancel_reconnect(id);
                }
            }
        }
    }

    /// P6b §5.2 `C2D::SubmitCommand`: record (and optionally write) a command
    /// for an exec-less shell. Single-line only — cmd executes each line at
    /// its own prompt, so one record for N lines would be a lie (Q2 keeps the
    /// multi-line ledger out of v1). The synthetic block opens at the
    /// PRE-WRITE journal head so everything the submission causes (echo +
    /// output) lands inside it; the next token-checked `pre` closes it via
    /// the existing on_pre path (exit None — honest, D7).
    fn submit_command(&self, client: &Arc<ClientConn>, id: Uuid, cmd: String, write: bool) {
        if let Err(msg) = validate_submit_command(&cmd) {
            log::warn!("SubmitCommand for {id} refused: {msg}");
            if let Some(f) = frame_bytes(&D2C::Error {
                message: format!("SubmitCommand refused: {msg}"),
            }) {
                client.enqueue(&f); // requester only — a reply, not a broadcast
            }
            return;
        }
        let cmd = cmd.trim().to_string();
        if write {
            // Submission bytes exactly like P3/P5: the mirror decides
            // bracketed paste (cmd.exe never sets DECSET 2004 — bare + \r).
            let bracketed = self
                .sessions
                .lock()
                .get(&id)
                .map(|s| s.term.clone())
                .is_some_and(|t| {
                    t.lock()
                        .mode()
                        .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
                });
            let bytes = control::submission_bytes(bracketed, &cmd);
            // at_off BEFORE the write (mirror purity: input is not output —
            // nothing is journaled here; the echo needs a conhost round trip
            // so everything this submission causes lands >= at_off).
            let at_off = match self.journal(id) {
                Ok(j) => j.lock().absolute_len(),
                Err(_) => {
                    log::debug!("SubmitCommand: unknown terminal {id}");
                    return;
                }
            };
            let writer = self.sessions.lock().get(&id).map(|s| s.writer.clone());
            let Some(w) = writer else {
                log::debug!("SubmitCommand: terminal {id} is not running");
                if let Some(f) = frame_bytes(&D2C::Error {
                    message: "SubmitCommand refused: terminal is not running".into(),
                }) {
                    client.enqueue(&f);
                }
                return;
            };
            {
                let mut w = w.lock();
                let _ = w.write_all(&bytes);
                let _ = w.flush();
            }
            self.open_synthetic(id, cmd, at_off);
        } else {
            // Record-only: the bytes already went via Input (a GUI-observed
            // raw Enter at a cmd prompt) — open at the current head.
            let at_off = match self.journal(id) {
                Ok(j) => j.lock().absolute_len(),
                Err(_) => return,
            };
            self.open_synthetic(id, cmd, at_off);
        }
    }

    /// Open a synthetic block record (P6b §5.2): the daemon-authored
    /// equivalent of an exec hook for shells that cannot emit one. Reuses
    /// `BlockStore::open_block` (a dangling predecessor closes at this
    /// offset, MAX_RECS eviction, last_cwd stamping) and the normal
    /// incremental Blocks notify + EV_BLOCKS events. The store must exist
    /// and have been hooked (launch() rotated an epoch) — an unhooked
    /// terminal has no prompt hooks to ever CLOSE the record, so opening one
    /// would lie forever.
    pub(crate) fn open_synthetic(&self, id: Uuid, cmd: String, at_off: u64) {
        let outcome = {
            let mut map = self.blocks.lock();
            let Some(store) = map.get_mut(&id) else {
                log::debug!("open_synthetic: no block store for {id}");
                return;
            };
            if store.epoch == 0 {
                log::debug!("open_synthetic: terminal {id} was never hooked; dropped");
                return;
            }
            // cmd's static pre carries no cwd payload; last_cwd is kept fresh
            // from the adjacent OSC 9;9 (on_block_event's fill). Fall back to
            // the session's OSC cwd if no pre has landed yet this spawn.
            let changed = store.open_block(cmd, at_off, now_ms());
            let recs: Vec<_> = changed
                .into_iter()
                .filter_map(|i| store.recs.get(i).cloned())
                .collect();
            (store.epoch, recs)
        };
        let (epoch, recs) = outcome;
        if !recs.is_empty() {
            let closed: Vec<crate::state::BlockRec> = recs
                .iter()
                .filter(|r| r.end_off.is_some())
                .cloned()
                .collect();
            self.notify_blocks(id, epoch, false, recs);
            self.resolve_block_close(id, &closed);
        }
    }

    /// D14 (P6b §5.3) — the exec-less at-prompt evidence for Cmd-family
    /// terminals: with no exec hook, the 133;B prompt latch is never cleared
    /// while a typed command runs (only the NEXT pre re-cycles it), so
    /// "prompt latched + no open block" is vacuous. The strongest honest
    /// daemon-side signal is: the mirror cursor sits exactly at the latched
    /// prompt-end column on its row, AND the session has been output-quiet
    /// ≥ 300ms. Residual risk (an interactive app idling with a prompt-shaped
    /// screen) is documented; `--force` exists.
    fn cmd_prompt_evidence(&self, id: Uuid) -> bool {
        let arcs = self
            .sessions
            .lock()
            .get(&id)
            .map(|s| (s.term.clone(), s.prompt.clone(), s.last_output.clone()));
        let Some((term, prompt, last_output)) = arcs else {
            return false;
        };
        let Some(col) = *prompt.lock() else {
            return false;
        };
        let cur_col = term.lock().grid().cursor.point.column.0;
        if cur_col != col {
            return false;
        }
        now_ms().saturating_sub(last_output.load(Ordering::Relaxed)) >= CMD_QUIET_MS
    }

    /// Whether `id`'s family is Cmd (P6b routing: the submission ledger and
    /// the D14 gate additions apply only there).
    fn is_cmd_family(&self, id: Uuid) -> bool {
        let state = self.state.lock();
        state.terminal(id).is_some_and(|t| {
            matches!(
                crate::state::shell_family(&t.kind, &t.program, &t.args),
                crate::state::ShellFamily::Cmd
            )
        })
    }

    /// Resize a terminal's PTY + headless Term + persisted last_* size.
    /// Clamped to sane bounds before anything touches the grid: alacritty's
    /// Grid neither validates nor caps (0 rows/cols underflows indices;
    /// 65535×65535 tries to allocate billions of cells inside the broker) and
    /// ConPTY coordinates are i16. No-op resizes are skipped (ConPTY treats
    /// every ResizePseudoConsole as real work). No snapshot broadcast —
    /// nothing user-visible changed. State is saved only on change so a
    /// repeated-size storm can't become an fsync storm.
    fn do_resize(&self, id: Uuid, cols: u16, rows: u16) {
        let cols = cols.clamp(2, 1000);
        let rows = rows.clamp(2, 1000);
        let perf_t0 = perf::on().then(Instant::now);
        let mut save_us = 0u64;
        {
            let mut state = self.state.lock();
            if let Some(t) = state.terminal_mut(id) {
                if (t.last_cols, t.last_rows) != (cols, rows) {
                    t.last_cols = cols;
                    t.last_rows = rows;
                    let t0 = perf_t0.map(|_| Instant::now());
                    state.save_logged("last_cols/rows");
                    save_us = t0.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                }
            }
        }
        // Clone the per-session handles out and resize OUTSIDE the global
        // sessions mutex: ResizePseudoConsole is a synchronous cross-process
        // call into conhost, and the mirror reflow walks up to 2000
        // scrollback lines — neither belongs inside a lock every C2D::Input
        // contends. The per-session master mutex (held across check+apply)
        // serializes concurrent resizes of the same terminal, so the
        // unchanged-check/apply pairing stays atomic per terminal.
        let handles = self
            .sessions
            .lock()
            .get(&id)
            .map(|s| (s.master.clone(), s.term.clone()));
        if let Some((master, term)) = handles {
            let master = master.lock();
            let unchanged = {
                use alacritty_terminal::grid::Dimensions;
                let t = term.lock();
                t.columns() == cols as usize && t.screen_lines() == rows as usize
            };
            if !unchanged {
                let t0 = perf_t0.map(|_| Instant::now());
                let _ = master.resize(portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                let pty_us = t0.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                // Conhost-parity grow (serialize::resize_conhost): a raw
                // alacritty rows-grow PULLS scrollback rows onto the screen —
                // conhost never does, and its post-resize repaint then erases
                // the pulled rows from the mirror screen while they are gone
                // from mirror history too (content silently destroyed from
                // every future attach serialization).
                let t0 = perf_t0.map(|_| Instant::now());
                serialize::resize_conhost(&mut term.lock(), cols as usize, rows as usize);
                // One line per applied PTY resize so a storm is countable.
                if let (Some(all), Some(t0)) = (perf_t0, t0) {
                    log::info!(
                        "[resize] {id} {cols}x{rows} save_us={save_us} pty_us={pty_us} \
                         mirror_us={} total_us={} boot_ms={}",
                        t0.elapsed().as_micros(),
                        all.elapsed().as_micros(),
                        perf::boot_ms()
                    );
                } else {
                    log::info!("[resize] {id} {cols}x{rows}");
                }
            }
        }
    }

    fn mutate(&self, f: impl FnOnce(&mut SharedState)) {
        {
            let mut state = self.state.lock();
            f(&mut state);
            if let Err(e) = state.save() {
                drop(state);
                self.report_error(&format!("failed to save state: {e}"));
            }
        }
        self.broadcast_snapshot();
    }

    /// Record a fast (< 3s) exit for a terminal; returns the running count.
    pub fn note_fast_exit(&self, id: Uuid, fast: bool) {
        let mut map = self.fast_exits.lock();
        if fast {
            *map.entry(id).or_insert(0) += 1;
        } else {
            map.remove(&id);
        }
    }

    /// Log + broadcast an error to clients, throttled so a persistent failure
    /// (e.g. a full disk) cannot spam the wire.
    pub fn report_error(&self, msg: &str) {
        log::error!("{msg}");
        let now = now_ms();
        let last = self.last_error_ms.swap(now, Ordering::Relaxed);
        if now.saturating_sub(last) < 30_000 && last != 0 {
            return;
        }
        self.broadcast(&D2C::Error {
            message: msg.to_string(),
        });
    }

    fn flush_all(&self) {
        for j in self.journals.lock().values() {
            j.lock().sync();
        }
    }

    /// Wait (bounded) for in-flight PTY output to reach the journals before a
    /// planned exit. `Session.last_output` is stamped by the reader thread on
    /// every read, so "every session quiet for ≥ IDLE_MS" means nothing is
    /// mid-burst: conhost's async text frames arrive on a 16–33ms cadence and
    /// the bootstrap hook sleeps are 15ms, so 300ms of true silence proves the
    /// pipe is empty. A flooding session hits `cap` — the process must still
    /// exit; that tail is beyond any shutdown's reach.
    fn drain_output_tail(&self, cap: Duration) {
        self.drain_targets(None, cap);
    }

    /// The shutdown drain predicate, scoped to a target set (SLEEP §3):
    /// poll every 25ms until each targeted session has in_flight == 0 AND
    /// has been output-quiet ≥300ms, bounded by `cap`. `only: None` = every
    /// session (the Shutdown/shutdown_flush call, byte-identical behavior).
    /// A folder sleep shares ONE window: sleeping 15 idle terminals costs
    /// one ~300ms-quiet check pass, not 15 (DO-NOT 5).
    fn drain_targets(&self, only: Option<&HashSet<Uuid>>, cap: Duration) {
        const IDLE_MS: u64 = 300;
        let start = Instant::now();
        loop {
            let now = now_ms();
            let busy = self.sessions.lock().iter().any(|(id, s)| {
                only.is_none_or(|set| set.contains(id))
                    // Read-but-not-journaled bytes make a session busy
                    // regardless of stamp age (L-7: the stamp lands AFTER
                    // the journal append, and `in_flight` covers the
                    // read→channel→append window, so quiet really means
                    // "the last read bytes are journaled").
                    && (s.in_flight.load(Ordering::Relaxed) > 0
                        || now.saturating_sub(s.last_output.load(Ordering::Relaxed)) < IDLE_MS)
            });
            if !busy {
                return;
            }
            if start.elapsed() >= cap {
                log::warn!("output drain hit its {cap:?} cap with output still flowing");
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// SLEEP §3 — the shared sleep core (single terminal AND folder bulk):
    /// flag → fail non-Exit waiters "asleep" → shared drain → kill. The
    /// exit-watcher → on_exit path then does ALL bookkeeping unchanged
    /// (Dead status, journal sync, dangling-block close exit=None, sidecar
    /// save, Exit-waiter resolution, EV_EXIT, D2C::Exited, Snapshot) —
    /// sleep's identity lives entirely in the pre-set flag (DO-NOT 3).
    ///
    /// Ordering is load-bearing: the flag mutate (ONE save + ONE Snapshot)
    /// lands BEFORE the kill, so a daemon death anywhere in between reloads
    /// as (Dead, asleep=true) = Asleep — the intended outcome (§2.1).
    /// Mirror purity: nothing here touches the mirror Term, the PTY, or the
    /// journal (DO-NOT 7); the wake-time seam is launch()'s existing code.
    ///
    /// Callers: C2D handlers run this on a `sleep-…` worker thread (S19 —
    /// a 2s inline drain would freeze every other terminal on the client's
    /// connection); the Ctl handler runs it inline on its own conn thread
    /// and replies Done after the kill is issued.
    pub(crate) fn sleep_terminals(&self, ids: &[Uuid]) {
        // 1. Filter to presented-Running (status Running, not already
        //    flagged) with a live session — a spawn in flight is refused by
        //    the callers; racing ones just miss the kill below, harmless.
        let targets: Vec<Uuid> = {
            let state = self.state.lock();
            ids.iter()
                .copied()
                .filter(|id| {
                    state
                        .terminal(*id)
                        .is_some_and(|t| t.status == TermStatus::Running && !t.asleep)
                })
                .collect()
        };
        // r2-F8: the same spawn-in-flight guard the single-terminal ctl path
        // has — a member whose session hasn't landed in the sessions map yet
        // would be flagged asleep but never killed: presented "sleeping"
        // forever with a live, un-killed shell (input refused, wake refused).
        // Skip + log, matching the busy-skip pattern.
        let targets: Vec<Uuid> = {
            let sessions = self.sessions.lock();
            targets
                .into_iter()
                .filter(|id| {
                    let live = sessions.contains_key(id);
                    if !live {
                        log::info!("sleep: skipping {id} — session not up yet (spawn in flight)");
                    }
                    live
                })
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        log::info!("sleep: {} terminal(s) {targets:?}", targets.len());
        // 2. Persist intent FIRST (one save, one Snapshot — capture-on-change).
        self.mutate(|s| {
            for id in &targets {
                if let Some(t) = s.terminal_mut(*id) {
                    t.asleep = true;
                }
            }
        });
        // 3. Waiters learn the cause before the mechanism (S11).
        for id in &targets {
            self.fail_waiters_for(
                *id,
                "asleep",
                "the terminal was put to sleep; the condition can only resolve after an explicit wake",
            );
        }
        // 4. One SHARED drain window for the whole set — the journal keeps
        //    every byte conhost already produced (restore_fidelity class,
        //    DO-NOT 1). Idle terminals return in ~one 25ms tick.
        let set: HashSet<Uuid> = targets.iter().copied().collect();
        self.drain_targets(Some(&set), Duration::from_millis(2000));
        // 4.5 FREEZE-FRAME capture + fanout mute (sleep-freeze). The kill is
        //     not a freeze: claude's graceful exit handler runs on the ConPTY
        //     console-close and WIPES the alt screen into the journal before
        //     EOF (`?1049l` + resume hint + full-screen erase — verified
        //     byte-for-byte), so the post-drain/pre-kill mirror is the ONLY
        //     witness of the frame the user was looking at. Alt-screen
        //     targets get their grid serialized to journals/<id>.frame
        //     (atomic, crc-guarded, 2MB-capped — pure decoration over the
        //     journal: any failure logs and the sleep proceeds unchanged).
        //     Then the session's live fanout is MUTED: the teardown bytes
        //     (the wipe + conhost's mode-reset trailer) still hit the journal
        //     (mirror purity, DO-NOT 1/7) but no longer repaint attached
        //     GUIs — their last live frame IS the freeze-frame, and the
        //     Exited re-attach serves the same frame from disk. All on this
        //     sleep worker thread: zero cost anywhere while awake; folder
        //     sleeps pay ms-class serial captures before the one kill pass.
        {
            use alacritty_terminal::grid::Dimensions;
            let arcs: Vec<_> = {
                let sessions = self.sessions.lock();
                targets
                    .iter()
                    .filter_map(|id| {
                        sessions
                            .get(id)
                            .map(|s| (*id, s.term.clone(), s.mute_fanout.clone()))
                    })
                    .collect()
            };
            for (id, term, mute) in arcs {
                let captured = {
                    let t = term.lock();
                    serialize::is_alt_screen(&t).then(|| {
                        (
                            t.columns() as u16,
                            t.screen_lines() as u16,
                            serialize::capture_alt_frame(&t),
                        )
                    })
                };
                mute.store(true, Ordering::Relaxed);
                match captured {
                    Some((cols, rows, bytes)) => match frame::write(id, cols, rows, true, &bytes) {
                        Ok(()) => log::info!(
                            "sleep: froze alt frame for {id} ({} bytes at {cols}x{rows})",
                            bytes.len()
                        ),
                        Err(e) => log::warn!(
                            "sleep: freeze-frame capture failed for {id}: {e} (journal restore unaffected)"
                        ),
                    },
                    // Primary-screen sessions: the journal reconstruction is
                    // already faithful (v1 scope) — just make sure no stale
                    // frame from an earlier alt-screen sleep can shadow it.
                    None => frame::remove(id),
                }
            }
        }
        // 5. Kill. Session drop closes the ConPTY → conhost dies →
        //    attached clients terminate (measured: whole tree gone <2.5s;
        //    no explicit tree sweep — DO-NOT 8). A session that died on its
        //    own in between simply isn't in the map.
        {
            let mut sessions = self.sessions.lock();
            for id in &targets {
                if let Some(s) = sessions.get_mut(id) {
                    self.mark_expected_exit(*id);
                    let _ = s.killer.kill();
                }
            }
        }
        // Sleep is the stronger intent: stop any reconnect supervision.
        for id in &targets {
            self.cancel_reconnect(*id);
        }
    }

    /// SLEEP S7: the busy evidence a controller sleep is gated on (and the
    /// modal copy names): an OPEN block (command/TUI running), else output
    /// within SLEEP_QUIET_MS. Quiet alt-screen does NOT gate — the idle
    /// claude REPL is the headline target (DO-NOT 9).
    ///
    /// Sleep-freeze refinement (the "`tc run claude` always gates busy"
    /// friction bug): an open block whose session sits QUIET on the ALT
    /// SCREEN is the spec's own pass row ("quiet alt-screen TUI: dies exactly
    /// as a reboot would") — the block is open precisely BECAUSE the TUI was
    /// launched as a command (typed at a hooked prompt or via `tc run`), and
    /// gating on the launch spelling contradicted S7's headline case. A
    /// streaming TUI still gates (output within the quiet window); a
    /// primary-screen command (build, `ping -t`) still gates on its block.
    pub(crate) fn sleep_busy_evidence(&self, id: Uuid) -> Option<String> {
        let open_cmd = {
            let map = self.blocks.lock();
            map.get(&id)
                .and_then(|s| s.open.and_then(|i| s.recs.get(i)))
                .map(|r| r.cmd.clone())
        };
        if let Some(cmd) = open_cmd {
            // Same nesting as DebugDump: term is a leaf under sessions.
            let alt_quiet = self.sessions.lock().get(&id).is_some_and(|s| {
                now_ms().saturating_sub(s.last_output.load(Ordering::Relaxed)) >= SLEEP_QUIET_MS
                    && serialize::is_alt_screen(&s.term.lock())
            });
            if !alt_quiet {
                return Some(format!("{cmd} is running"));
            }
        }
        let quiet_ms = self
            .sessions
            .lock()
            .get(&id)
            .map(|s| now_ms().saturating_sub(s.last_output.load(Ordering::Relaxed)));
        match quiet_ms {
            Some(q) if q < SLEEP_QUIET_MS => Some("output is flowing".into()),
            _ => None,
        }
    }

    /// SLEEP S17: wake a set of asleep terminals through launch(), on the
    /// boot-restore lane structure but with the tighter interactive
    /// WAKE_STAGGER (r2 boot-perf 4b). launch() itself clears the asleep
    /// flag and is concurrency-safe for distinct ids (per-id LaunchGuard).
    pub(crate) fn wake_staggered(self: &Arc<Self>, ids: Vec<Uuid>) {
        let n = ids.len();
        if n == 0 {
            return;
        }
        log::info!("wake: {n} terminal(s) {ids:?}");
        let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(ids)));
        for _ in 0..RESTORE_LANES.min(n) {
            let core = self.clone();
            let queue = queue.clone();
            std::thread::spawn(move || loop {
                let Some(id) = queue.lock().pop_front() else {
                    break;
                };
                let t0 = Instant::now();
                // r2 boot-perf 3: same lane-stall fix as boot-restore —
                // a probe-due ssh member must not freeze its wake lane
                // for a 10-25s sftp leg. Catch per MEMBER (and log) so a
                // panic costs one wake, not the lane's queue remainder.
                let r = catch_unwind(AssertUnwindSafe(|| {
                    core.probe_aware_launch(id, None);
                }));
                if let Err(e) = r {
                    log::error!("wake lane panicked on {id}: {}", session::panic_payload(&e));
                }
                // 4b: interactive pacing, launch time credited.
                std::thread::sleep(WAKE_STAGGER.saturating_sub(t0.elapsed()));
            });
        }
    }

    /// Presented-Running members of a folder — the folder-sleep target set.
    fn folder_sleep_members(&self, folder: Uuid) -> Vec<Uuid> {
        self.state
            .lock()
            .terminals
            .iter()
            .filter(|t| {
                t.folder == Some(folder) && t.status == TermStatus::Running && !t.asleep
            })
            .map(|t| t.id)
            .collect()
    }

    /// Presented-Asleep members of a folder — the folder-wake target set
    /// (S16: dead non-asleep members are untouched; wake resurrects exactly
    /// what sleep suspended).
    fn folder_wake_members(&self, folder: Uuid) -> Vec<Uuid> {
        self.state
            .lock()
            .terminals
            .iter()
            .filter(|t| t.folder == Some(folder) && t.status == TermStatus::Dead && t.asleep)
            .map(|t| t.id)
            .collect()
    }

    /// Clean-shutdown top-up: a final tracker pass, then durably flush journals
    /// and state. Called from the WM_ENDSESSION handler on logoff/shutdown.
    fn shutdown_flush(&self) {
        let snaps: Vec<(Uuid, u32, Option<std::path::PathBuf>)> = {
            let sessions = self.sessions.lock();
            sessions
                .iter()
                .filter_map(|(id, s)| s.root_pid.map(|p| (*id, p, s.osc_cwd.lock().clone())))
                .collect()
        };
        let hook_fed = self.hook_fed_family_ids();
        // Lazy for the same reason as the tracker tick: hook-fed sessions
        // never consume the table, and WM_ENDSESSION's budget is ~5s.
        let mut table: Option<Vec<(u32, u32, String)>> = None;
        for (id, root, osc) in snaps {
            if hook_fed.contains(&id) {
                self.apply_posix_cwd(id, osc);
            } else {
                let table = table.get_or_insert_with(procinfo::snapshot_processes);
                let sample = tracker::analyze(table, root, osc);
                self.apply_track_sample(id, sample);
            }
        }
        // Same in-flight-output drain as C2D::Shutdown, on a tighter cap —
        // WM_ENDSESSION's whole budget is ~5s and the tracker pass above
        // already spent some of it.
        self.drain_output_tail(Duration::from_millis(1000));
        self.flush_all();
        // Final write before Windows kills us — log a failure (C1).
        self.state.lock().save_logged("WM_ENDSESSION final save");
    }
}

use std::sync::atomic::AtomicU64;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn handle_client(core: Arc<Core>, stream: TcpStream, token: &str) {
    let mut reader = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    // First frame must be a valid Hello (GUI, FULL rights) or HelloCtl
    // (controller: master token ⇒ FULL, scoped token ⇒ its bits).
    let (scope, self_session) = match read_frame::<_, C2D>(&mut reader) {
        Ok(C2D::Hello { token: t }) if t == token => (SCOPE_FULL, None),
        Ok(C2D::HelloCtl {
            token: t,
            self_session,
        }) => {
            if t == token {
                (SCOPE_FULL, self_session)
            } else {
                let found = core
                    .ctl_tokens
                    .lock()
                    .tokens
                    .iter()
                    .find(|k| k.token == t)
                    .map(|k| k.scope);
                match found {
                    Some(s) => (s, self_session),
                    None => {
                        log::warn!("controller rejected: bad token");
                        return;
                    }
                }
            }
        }
        _ => {
            log::warn!("client rejected: bad hello");
            return;
        }
    };

    let (tx, rx) = sync_channel::<Arc<[u8]>>(CLIENT_QUEUE_DEPTH);
    let client = Arc::new(ClientConn {
        tx,
        attached: Mutex::new(HashSet::new()),
        alive: AtomicBool::new(true),
        scope,
        self_session,
    });

    // One dedicated writer owns the socket write half. Producers only enqueue,
    // so a client that stops reading blocks nothing but its own writer thread.
    //
    // Frames already sitting in the queue are coalesced into one write_all:
    // PTY reads arrive in line-sized pieces (~275B average under a flood —
    // measured), so per-frame syscalls were ~194k per 50MB and dominated the
    // daemon's flood CPU (~1.8s/50MB). try_recv never waits: a lone frame
    // (typing echo) still goes out immediately, zero added latency; only an
    // already-formed backlog batches. Byte stream is identical either way.
    let mut wstream = stream;
    let _ = std::thread::Builder::new()
        .name("client-writer".into())
        .spawn(move || {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                const BATCH_CAP: usize = 1024 * 1024;
                let mut batch: Vec<u8> = Vec::new();
                while let Ok(frame) = rx.recv() {
                    let buf: &[u8] = match rx.try_recv() {
                        // Common case (idle/typing): nothing queued behind it —
                        // write the frame directly, no copy.
                        Err(_) => &frame,
                        Ok(second) => {
                            batch.clear();
                            batch.extend_from_slice(&frame);
                            batch.extend_from_slice(&second);
                            while batch.len() < BATCH_CAP {
                                match rx.try_recv() {
                                    Ok(f) => batch.extend_from_slice(&f),
                                    Err(_) => break,
                                }
                            }
                            &batch
                        }
                    };
                    if perf::time(&perf::SOCK_NS, || wstream.write_all(buf)).is_err() {
                        break;
                    }
                }
            }));
            let _ = wstream.shutdown(Shutdown::Both);
        });

    core.clients.lock().push(client.clone());
    {
        // The connect-time snapshot rides the same ordering lock as
        // broadcast_snapshot (r2-F10): without it, a concurrent broadcast
        // could enqueue a NEWER state to this just-listed client before this
        // clone lands, and the wholesale apply would transiently regress
        // terminal metas GUI-side.
        let _order = core.snapshot_order.lock();
        if let Some(f) = frame_bytes(&D2C::Snapshot {
            state: core.state.lock().clone(),
        }) {
            client.enqueue(&f);
        }
    }
    log::info!("client connected");

    while let Ok(msg) = read_frame::<_, C2D>(&mut reader) {
        core.handle_message(&client, msg);
    }
    client.alive.store(false, Ordering::Relaxed);
    // P5: a gone controller must pin no waiters/subscriptions.
    core.purge_client(&client);
    // Prune so the last Arc<ClientConn> drops with this stack frame; that drops
    // the SyncSender and unblocks the writer thread's recv().
    core.clients.lock().retain(|c| c.alive.load(Ordering::Relaxed));
    log::info!("client disconnected");
}

/// Best effort: start the daemon at login so terminals restore after reboot
/// without waiting for the GUI. Prefer the installed copy if one exists.
fn install_autostart() {
    // An isolated daemon (TC_DATA_DIR set) must never touch the machine's
    // real autostart: installed_exe_path() resolves inside the override dir,
    // won't exist, and the current_exe fallback would repoint the Run key at
    // a build-tree binary.
    if crate::state::data_dir_overridden() {
        return;
    }
    let exe = crate::installed_exe_path()
        .filter(|p| p.exists())
        .or_else(|| std::env::current_exe().ok());
    let Some(exe) = exe else { return };
    // C3 honesty: if this write fails, the daemon never starts at boot and
    // terminals never auto-restore — at least say why in the log.
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    match hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run") {
        Ok((key, _)) => {
            if let Err(e) = key.set_value(
                "Pulse",
                &format!("\"{}\" --daemon", exe.display()),
            ) {
                log::warn!("autostart Run-key write failed (no boot auto-restore): {e}");
            }
        }
        Err(e) => log::warn!("autostart Run-key open failed (no boot auto-restore): {e}"),
    }
}

/// The release daemon is a `windows` subsystem process with no console, so
/// SetConsoleCtrlHandler never fires. A hidden message-only window is the only
/// way to get a clean-shutdown notification (WM_ENDSESSION) to top up state.
static SHUTDOWN_CORE: std::sync::OnceLock<Arc<Core>> = std::sync::OnceLock::new();

fn spawn_shutdown_window(core: Arc<Core>) {
    let _ = SHUTDOWN_CORE.set(core);
    std::thread::spawn(|| {
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe { run_shutdown_window() }));
    });
}

unsafe fn run_shutdown_window() {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage,
        HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
    };

    let hinstance: HINSTANCE = match GetModuleHandleW(None) {
        Ok(m) => HINSTANCE(m.0),
        Err(_) => return,
    };
    let class_name: PCWSTR = w!("TerminalControlShutdown");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(shutdown_wndproc),
        hInstance: hinstance,
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassW(&wc);

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class_name,
        w!("tc-shutdown"),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(hinstance),
        None,
    );
    if hwnd.is_err() {
        return;
    }

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}


unsafe extern "system" fn shutdown_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::core::w;
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
    use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_ENDSESSION, WM_QUERYENDSESSION};

    match msg {
        WM_QUERYENDSESSION => {
            let _ = ShutdownBlockReasonCreate(hwnd, w!("Flushing terminal journals"));
            if let Some(core) = SHUTDOWN_CORE.get() {
                core.shutdown_flush();
            }
            LRESULT(1) // TRUE: allow shutdown
        }
        WM_ENDSESSION => {
            if let Some(core) = SHUTDOWN_CORE.get() {
                core.shutdown_flush();
            }
            let _ = ShutdownBlockReasonDestroy(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Boot orphan-reap predicate (R3-2, extracted for unit pinning — R4-T2): a
/// per-terminal artifact file is an orphan when its name's LEADING
/// dot-component parses as a uuid that is NOT in the live terminal set.
/// `split('.').next()`, not `file_stem()`: `<uuid>.blocks.json` needs the
/// FIRST component. Names whose first component is not a uuid (daemon.log,
/// readme.txt) are never orphans — unknown formats are left alone.
fn is_orphan_artifact(name: &str, live: &HashSet<Uuid>) -> bool {
    name.split('.')
        .next()
        .and_then(|stem| Uuid::parse_str(stem).ok())
        .is_some_and(|id| !live.contains(&id))
}

pub fn run() -> anyhow::Result<()> {
    perf::mark_t0();
    std::fs::create_dir_all(data_dir())?;

    // Immunize against launchers that seeded the ignore-Ctrl+C disposition
    // (CREATE_NEW_PROCESS_GROUP / SetConsoleCtrlHandler(NULL, TRUE) — MSYS
    // bash and some schedulers do this): ConPTY children inherit our
    // ConsoleFlags, and with bit 0 set every native command in every terminal
    // silently ignores Ctrl+C. Same bug class as the spawn_daemon lore in
    // gui/ipc.rs, entered via the LAUNCHER's inherited flag instead of our
    // own creation flags. The daemon is DETACHED with no console, so it never
    // relies on being Ctrl+C-ignorant for its own lifetime.
    unsafe {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        let _ = SetConsoleCtrlHandler(None, false);
    }

    // Single instance FIRST: hold an exclusive lock file for the daemon's
    // lifetime. The lock must be claimed before log rotation — a LOSING
    // start that rotated first would rename the LIVE daemon's log out from
    // under its open handle (Windows renames of open-shared files succeed),
    // sending the live daemon's lines to .log.old and defeating the cap.
    let lock_path = data_dir().join("daemon.lock");
    let _lock = {
        use std::os::windows::fs::OpenOptionsExt;
        match OpenOptions::new()
            .write(true)
            .create(true)
            // Never truncate: the file's CONTENT is irrelevant (the exclusive
            // share_mode is the lock); truncating on open would be a write to
            // a file another daemon may hold.
            .truncate(false)
            .share_mode(0) // exclusive: second daemon fails to open
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(_) => {
                // Loser: append-only logger (NO rotation) so this one line
                // still lands without touching the winner's log file.
                if let Ok(f) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(daemon_log_path())
                {
                    let _ = simplelog::WriteLogger::init(
                        simplelog::LevelFilter::Info,
                        simplelog::Config::default(),
                        f,
                    );
                }
                log::info!("daemon already running; exiting");
                return Ok(());
            }
        }
    };

    // Startup log rotation (R3-1): the log otherwise grows forever (a 24/7
    // year of steady-state lines reaches tens of MB; a flapping reconnect can
    // do worse). Rename-replace keeps exactly one prior generation. Winner
    // only (see the lock claim above), and still before WriteLogger::init,
    // so probes' byte-offset reads within a run are unaffected.
    crate::state::rotate_log_at_startup(&daemon_log_path());
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(daemon_log_path())?;
    let _ = simplelog::WriteLogger::init(
        simplelog::LevelFilter::Info,
        simplelog::Config::default(),
        log_file,
    );

    log::info!("daemon starting (pid {})", std::process::id());
    // The daemon is the typing/ingest path for every terminal; never let
    // Windows demote it to background QoS (see procinfo::set_high_qos).
    if !procinfo::set_high_qos(std::process::id()) {
        log::warn!("power-throttling opt-out failed; daemon may be E-core scheduled under load");
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let info = DaemonInfo {
        port,
        token: token.clone(),
        pid: std::process::id(),
        // 11 = CLAUDE SESSION ATTRIBUTION: CtlRequest::ReportCliSession
        //      appended (the `tc __claude-hook` self-report, Layer 2);
        // 10 = SSH AUTO-RECONNECT: C2D::CancelReconnect appended +
        //      TerminalMeta.reconnecting appended after asleep (Snapshot
        //      wire layout changed) + ShellCfg.auto_reconnect (serde-default);
        // 9 = SLEEP: C2D::SleepTerminal/SleepFolder/WakeFolder + CtlRequest
        //     Sleep/Wake/SleepFolder/WakeFolder appended; TerminalMeta.asleep
        //     appended after color_tag (Snapshot wire layout changed);
        // 8 = C2D::SetColorTag/SetFolderColor + color_tag appended to
        //     TerminalMeta and Folder (Snapshot wire layout changed), task #22;
        // 7 = D2C::ReplayAnchors (restored-history hints);
        // 6 = C2D::SubmitCommand (cmd submission ledger, P6b);
        // 5 = TerminalMeta.hooked appended (Snapshot wire layout changed);
        // 4 = Controller API (HelloCtl/Ctl), P5; 3 = PromptState.
        proto: 11,
    };
    std::fs::write(daemon_info_path(), serde_json::to_vec(&info)?)?;
    install_autostart();
    if perf::on() {
        // Clients can connect from here (daemon.json is on disk and the
        // listener is bound); restores proceed in the background.
        log::info!("[perf] boot serving boot_ms={}", perf::boot_ms());
    }

    let (loaded_state, state_healthy) = SharedState::load();
    let core = Arc::new(Core {
        state: Mutex::new(loaded_state),
        sessions: Mutex::new(HashMap::new()),
        journals: Mutex::new(HashMap::new()),
        clients: Mutex::new(Vec::new()),
        blocks: Mutex::new(HashMap::new()),
        fast_exits: Mutex::new(HashMap::new()),
        launching: Mutex::new(HashSet::new()),
        cli_blocks: Mutex::new(HashMap::new()),
        last_error_ms: AtomicU64::new(0),
        waiters: Mutex::new(Vec::new()),
        waiter_count: AtomicUsize::new(0),
        subs: Mutex::new(Vec::new()),
        sub_count: AtomicUsize::new(0),
        ctl_tokens: Mutex::new(ctl_tokens::load()),
        spawn_times: Mutex::new(HashMap::new()),
        expected_exits: Mutex::new(HashSet::new()),
        reconnects: Mutex::new(HashMap::new()),
        probe_rt: Arc::new(remote_probe::Runtime::new()),
        probing: Mutex::new(HashSet::new()),
        hook_homes: Mutex::new(HashMap::new()),
        wsl_reg_next: Mutex::new(HashMap::new()),
        claude_reports: Mutex::new(HashMap::new()),
        snapshot_order: Mutex::new(()),
        hints: HintPool::new(),
    });

    // Reap per-terminal artifacts whose terminal no longer exists. Older
    // builds' delete path raced the dying session's reader thread, which
    // re-created the journal after deletion; and a crash inside
    // delete_terminal_inner (correct but not atomic) can strand any of the
    // five artifacts. R3-2 widened this from `*.log` only to every
    // uuid-named file in the three per-terminal dirs — field evidence:
    // leaked `<uuid>.blocks.json` sidecars, and a crash mid-compaction can
    // leave a 4MB `.log.tmp` corpse. The predicate lives in
    // `is_orphan_artifact` (unit-pinned). GATED on a HEALTHY state load
    // (R4-F1): against a defaulted/empty state — corrupt state.json,
    // transient read error, fresh install — every artifact would classify as
    // orphaned and the reap would irreversibly delete all journals, block
    // sidecars, and probe stores. Skipping a boot is free; the reap runs
    // again on the next healthy boot.
    if state_healthy {
        let ids: HashSet<Uuid> = core.state.lock().terminals.iter().map(|t| t.id).collect();
        let mut reaped = 0u32;
        for dir in [
            crate::state::journals_dir(),
            crate::state::data_probes_dir(),
            bootstrap::bootstrap_dir(),
        ] {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let orphan = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|name| is_orphan_artifact(name, &ids));
                if orphan && std::fs::remove_file(&path).is_ok() {
                    reaped += 1;
                }
            }
        }
        if reaped > 0 {
            log::info!("reaped {reaped} orphan per-terminal file(s)");
        }
    } else {
        log::warn!("state.json did not load healthy; skipping orphan artifact reap this boot");
    }

    // Clean-shutdown top-up on logoff/reboot (no console → message window).
    spawn_shutdown_window(core.clone());

    // Power-loss-grade journal flush: a fast tick fsyncs each dirty journal as
    // soon as its output burst ends (idle ≥500ms), capped at 2s for sustained
    // output. Idle terminals therefore lose ~0 on a power cut.
    {
        let flush_core = core.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(250));
            // Catch per ITERATION and log: a whole-loop catch_unwind would
            // let one panic silently end journal fsyncs (voiding the ≤600ms
            // power-loss window), wait-timeout expiry, and the reconnect
            // pump for the daemon's lifetime, with zero diagnostic.
            let tick = catch_unwind(AssertUnwindSafe(|| {
                let journals: Vec<_> = flush_core.journals.lock().values().cloned().collect();
                for j in journals {
                    // The fsync itself runs OUTSIDE the journal lock (dup'd
                    // handle): an echo chunk arriving during a slow fsync
                    // (saturated disk queue) must not wait it out. Appends
                    // racing the sync re-mark dirty; the journal only
                    // appends, so a handle taken now covers every byte
                    // appended up to this point — exposure unchanged.
                    let pending = j.lock().begin_tick_sync();
                    if let Some(f) = pending {
                        let res = f.sync_data();
                        j.lock().finish_tick_sync(&res);
                    }
                }
                // P5 wait timeouts ride this existing tick (inv. 5: no new
                // polling loop); zero cost while no waiter exists. 250ms
                // granularity on timeouts is documented.
                if flush_core.waiter_count.load(Ordering::Relaxed) > 0 {
                    flush_core.expire_waiters(now_ms());
                }
                // SSH auto-reconnect backoff engine (no-op while the map is
                // empty — one lock probe per tick).
                flush_core.pump_reconnects();
                perf::dump_if_active();
            }));
            if let Err(e) = tick {
                log::error!("flush tick panicked: {}", session::panic_payload(&e));
            }
        });
    }

    // Session tracker: every 300ms, read each running Shell/Custom terminal's
    // live cwd (OSC report or PEB) and any hand-run CLI, persisting on change so
    // a restart resumes both. catch_unwind-wrapped like the other workers.
    //
    // Activity-gated: a pass costs ~15ms (Toolhelp process-table walk — this
    // dominated a 20-idle-session daemon at ~47ms/s CPU), and a session that
    // produced no output can't have moved its cwd interactively (cd/CLI launch
    // always repaints the prompt). Ticks where every session has been quiet
    // since the previous pass are skipped, with a forced full pass every 3s so
    // even a pathological SILENT process-tree change (a child spawned by a
    // long-running command that hasn't printed yet) is captured within 3s —
    // the power-loss capture window for output-driven changes stays ≤600ms.
    //
    // DIAL (R3-4, deliberate default): this 3s force period is essentially
    // 100% of the daemon's measured idle CPU (~6ms/s ≈ 0.6% of one core with
    // 20 idle terminals). Widening it to 10s cuts idle burn ~3× at the cost
    // of silent-change capture latency 3s→10s; 0.6% is already fine for a
    // 24/7 daemon, so the capture latency wins.
    {
        let track_core = core.clone();
        std::thread::spawn(move || {
            let mut last_full = Instant::now();
            let mut prev_pass_ms = now_ms();
            loop {
                std::thread::sleep(Duration::from_millis(300));
                // Per-iteration catch (see the flush tick): a tracker panic
                // must not silently freeze cwd/inner-CLI attribution — and
                // therefore restores — for the daemon's lifetime.
                let tick = catch_unwind(AssertUnwindSafe(|| {
                    let force = last_full.elapsed() >= Duration::from_secs(3);
                    // Slack covers the stamp/tick race at the window edge.
                    let cutoff = prev_pass_ms.saturating_sub(50);
                    let snaps: Vec<(Uuid, u32, Option<std::path::PathBuf>)> = {
                        let sessions = track_core.sessions.lock();
                        sessions
                            .iter()
                            .filter(|(_, s)| {
                                force || s.last_output.load(Ordering::Relaxed) >= cutoff
                            })
                            .filter_map(|(id, s)| {
                                s.root_pid.map(|p| (*id, p, s.osc_cwd.lock().clone()))
                            })
                            .collect()
                    };
                    prev_pass_ms = now_ms();
                    if force {
                        last_full = Instant::now();
                    }
                    if snaps.is_empty() {
                        return;
                    }
                    let mut meta_changed = false;
                    perf::time(&perf::TRACK_NS, || {
                        // P6 §7.1: WslShell/Ssh sessions are hook-fed — cwd
                        // from the scanned OSC 9;9 only, no Toolhelp/PEB
                        // walk, and inner_cli untouched (the hook lifecycle
                        // owns it — the Win32 verdict would clear it every
                        // tick).
                        let hook_fed = track_core.hook_fed_family_ids();
                        // One process-table snapshot for the whole tick keeps
                        // idle CPU low — built LAZILY: hook-fed (WSL/ssh)
                        // sessions never consume it, so an all-hook-fed tick
                        // skips the ~15ms Toolhelp walk entirely.
                        let mut table: Option<Vec<(u32, u32, String)>> = None;
                        for (id, root, osc) in snaps {
                            if hook_fed.contains(&id) {
                                meta_changed |= track_core.apply_posix_cwd(id, osc);
                                // Attribution Layer 1, WSL: refine an open
                                // claude block's token from the distro's own
                                // pid registry (self-gates on family +
                                // inner_cli; throttled inside).
                                meta_changed |= track_core.refresh_wsl_claude(id);
                            } else {
                                let table =
                                    table.get_or_insert_with(procinfo::snapshot_processes);
                                let sample = tracker::analyze(table, root, osc);
                                meta_changed |= track_core.apply_track_sample(id, sample);
                            }
                        }
                    });
                    if meta_changed {
                        // Coalesced: ONE Snapshot per tick when any label-
                        // visible metadata moved (the stale-lane-cwd bug:
                        // apply_track_sample saved without broadcasting, so
                        // the GUI label waited for an unrelated Snapshot).
                        track_core.broadcast_snapshot();
                    }
                }));
                if let Err(e) = tick {
                    log::error!("tracker tick panicked: {}", session::panic_payload(&e));
                }
            }
        });
    }

    // Auto-restore: relaunch terminals marked auto_restore, in a few parallel
    // lanes with a per-lane stagger. Serial + 300ms measured 7.1s for 20
    // sessions (perf-wave-3) with the stagger 85% of it — the launch work
    // itself is ~50ms/session. Lanes keep boot fast while still pacing
    // process creation so 20 shell/Claude inits don't hit a login-time
    // machine as one spike (launch() is already concurrency-safe for
    // distinct ids: the GUI and controller clients launch in parallel today;
    // per-id coalescing is `Core::launching`).
    {
        let ids: Vec<Uuid> = {
            let state = core.state.lock();
            state
                .terminals
                .iter()
                // SLEEP S4: asleep terminals are skipped — asleep-across-
                // reboots falls out of this one line (should_boot_restore
                // is the pure, unit-pinned form).
                .filter(|t| should_boot_restore(t))
                .map(|t| t.id)
                .collect()
        };
        let n = ids.len();
        if perf::on() && n > 0 {
            log::info!("[perf] boot restore begin n={n} boot_ms={}", perf::boot_ms());
        }
        let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(ids)));
        let remaining = Arc::new(AtomicUsize::new(n));
        for _ in 0..RESTORE_LANES.min(n.max(1)) {
            let restore_core = core.clone();
            let queue = queue.clone();
            let remaining = remaining.clone();
            std::thread::spawn(move || loop {
                let Some(id) = queue.lock().pop_front() else {
                    break;
                };
                let t0 = Instant::now();
                // r2 boot-perf 3: a probe-due ssh terminal runs its
                // remote CLI-resume probe INLINE in launch() — an sftp
                // with ConnectTimeout=10 + a 25s watchdog, against a
                // network that at PC boot typically isn't up yet. One
                // such terminal used to stall its whole restore lane
                // (four of them = ALL lanes); probe_aware_launch moves
                // the probe leg to its worker and the lane proceeds.
                // The probed terminal loses stagger pacing — it is
                // waiting on the network anyway. Catch per MEMBER (and
                // log) so a panic costs one restore, not the lane's
                // queue remainder.
                let r = catch_unwind(AssertUnwindSafe(|| {
                    restore_core.probe_aware_launch(id, None);
                }));
                if let Err(e) = r {
                    log::error!(
                        "boot-restore lane panicked on {id}: {}",
                        session::panic_payload(&e)
                    );
                }
                if perf::on() {
                    log::info!(
                        "[perf] boot launch id={id} launch_ms={} boot_ms={}",
                        t0.elapsed().as_millis(),
                        perf::boot_ms()
                    );
                }
                if remaining.fetch_sub(1, Ordering::Relaxed) == 1 && perf::on() {
                    log::info!("[perf] boot restore done n={n} boot_ms={}", perf::boot_ms());
                }
                // r2 boot-perf 4a: credit the launch's own duration
                // (~100ms over 2MB tails) against the stagger — the wave
                // period becomes a flat RESTORE_STAGGER instead of
                // launch+RESTORE_STAGGER, ~25% off boot-restore.
                // RESTORE_LANES stays the login-storm dial.
                std::thread::sleep(RESTORE_STAGGER.saturating_sub(t0.elapsed()));
            });
        }
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                // TCP keepalive so a hard-killed client eventually errors the
                // writer thread instead of leaking it forever.
                let sock = socket2::SockRef::from(&stream);
                let _ = sock.set_keepalive(true);
                let _ = sock.set_tcp_keepalive(
                    &socket2::TcpKeepalive::new().with_time(Duration::from_secs(15)),
                );
                let core = core.clone();
                let token = token.clone();
                std::thread::spawn(move || {
                    let _ = catch_unwind(AssertUnwindSafe(move || {
                        handle_client(core, stream, &token)
                    }));
                });
            }
            Err(e) => log::error!("accept failed: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::{self, test::TermSize};

    struct NullListener;
    impl alacritty_terminal::event::EventListener for NullListener {
        fn send_event(&self, _: alacritty_terminal::event::Event) {}
    }

    fn new_term() -> Term<NullListener> {
        Term::new(term::Config::default(), &TermSize::new(20, 4), NullListener)
    }

    fn row_text(term: &Term<NullListener>, line: i32) -> String {
        let row = &term.grid()[Line(line)];
        (0..20).map(|c| row[Column(c)].c).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn daemon_parser_applies_sync_blocks_immediately() {
        // BSU + text with NO terminating ESU: the mirror must apply it right
        // away — it answers DSR queries and is serialized on Attach, so it
        // can never sit on deferred bytes.
        let mut term = new_term();
        let mut parser = ImmediateProcessor::new();
        parser.advance(&mut term, b"\x1b[?2026hHELLO");
        assert_eq!(row_text(&term, 0), "HELLO");
    }

    /// SLEEP S4: the boot auto-restore filter skips asleep terminals — the
    /// whole asleep-across-reboots story is this one predicate. Both
    /// polarities pinned (probe sleep_roundtrip pins them live).
    #[test]
    fn boot_restore_skips_asleep() {
        let mut t = TerminalMeta {
            id: Uuid::new_v4(),
            name: "t".into(),
            folder: None,
            kind: TermKind::Shell,
            program: "powershell.exe".into(),
            args: vec![],
            cwd: "C:\\".into(),
            order: 0,
            auto_restore: true,
            launched_once: true,
            status: TermStatus::Dead,
            last_cols: 0,
            last_rows: 0,
            live_cwd: None,
            inner_cli: None,
            hooked: false,
            shell_cfg: None,
            color_tag: None,
            asleep: false,
            reconnecting: false,
        };
        assert!(should_boot_restore(&t), "awake auto_restore terminal restores");
        t.asleep = true;
        assert!(!should_boot_restore(&t), "asleep wins over auto_restore (inv. 6)");
        t.asleep = false;
        t.launched_once = false;
        assert!(!should_boot_restore(&t), "never-launched terminals never restore");
        t.launched_once = true;
        t.auto_restore = false;
        assert!(!should_boot_restore(&t));
    }

    /// U7 (P6b §12): the SubmitCommand validation table — multi-line (both
    /// separators) and empty/whitespace are refused; a plain line passes.
    /// The synthetic open/close lifecycle itself is BlockStore territory
    /// (open_block + on_pre, pinned in blocks.rs); the no-PTY-write side of
    /// write:false is pinned end-to-end by probe cmd_hooks (journal head
    /// unmoved).
    #[test]
    fn submit_command_validation_table() {
        assert!(validate_submit_command("echo hi").is_ok());
        assert!(validate_submit_command("  dir /b  ").is_ok());
        assert!(validate_submit_command("echo a\necho b").is_err());
        assert!(validate_submit_command("echo a\recho b").is_err());
        assert!(validate_submit_command("").is_err());
        assert!(validate_submit_command("   ").is_err());
        // The refusal names the family rule (the GUI hint shows the same).
        assert!(validate_submit_command("a\nb")
            .unwrap_err()
            .contains("one line at a time"));
    }

    /// U7 companion: a synthetic block (opened by SubmitCommand — no exec
    /// hook involved) closes on the next pre EXACTLY like a hook-opened one,
    /// carrying the pre's honest exit — which for cmd's static PROMPT pre is
    /// None, with the cwd substituted from the adjacent 9;9 (the empty-cwd
    /// fill path stamps last_cwd BEFORE on_pre runs).
    #[test]
    fn synthetic_block_closes_on_pre_with_exit_none() {
        let mut st = blocks::BlockStore::load(Uuid::new_v4()); // no sidecar
        st.rotate("0123456789abcdef".into());
        // The cwd fill (on_block_event's Pre arm) runs before on_pre.
        st.last_cwd = Some(std::path::PathBuf::from("C:\\Users"));
        let changed = st.open_block("echo CMD_OK".into(), 100, 1_000);
        assert_eq!(changed, vec![0]);
        assert_eq!(st.recs[0].cwd.as_deref(), Some(std::path::Path::new("C:\\Users")));
        assert!(st.recs[0].end_off.is_none());
        // cmd's static pre: e=null, n=0, empty cwd payload.
        let closed = st.on_pre(None, 0, String::new(), 250, 1_400);
        assert_eq!(closed, Some(0));
        let r = &st.recs[0];
        assert_eq!(r.exit, None, "cmd never reports exit codes (D7)");
        assert_eq!(r.end_off, Some(250));
        assert_eq!(r.ended_ms, Some(1_400));
        assert_eq!(
            st.last_cwd.as_deref(),
            Some(std::path::Path::new("C:\\Users")),
            "an empty pre cwd payload must not clobber the 9;9-filled cwd"
        );
    }

    /// MEDIUM-2 launch guard: a second launch for the SAME id while one is in
    /// flight must coalesce (try_begin fails); an unrelated id is unaffected;
    /// the claim releases on drop (incl. early returns) so the id is
    /// launchable again afterwards.
    #[test]
    fn launch_guard_coalesces_same_id_and_releases_on_drop() {
        let set: Mutex<HashSet<Uuid>> = Mutex::new(HashSet::new());
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let g1 = LaunchGuard::try_begin(&set, a).expect("first claim wins");
        assert!(
            LaunchGuard::try_begin(&set, a).is_none(),
            "concurrent launch of the same id must coalesce"
        );
        let g2 = LaunchGuard::try_begin(&set, b).expect("other ids unaffected");
        drop(g2);
        drop(g1);
        assert!(
            LaunchGuard::try_begin(&set, a).is_some(),
            "claim must release on drop"
        );
    }

    /// The hand-rolled frames must be BIT-FOR-BIT what the derived serde
    /// path produced forever — clients (including old GUIs) decode them
    /// with the derived Deserialize. Covers empty, tiny, binary-with-escapes,
    /// and a >64KiB payload, for BOTH borrowed-payload variants.
    #[test]
    fn byte_frames_match_derived_encoding() {
        let id = Uuid::new_v4();
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        for payload in [&b""[..], b"x", b"hello \x1b[31m wor\xffld\x00", &big] {
            let derived = frame_bytes(&D2C::Output {
                id,
                bytes: payload.to_vec(),
            })
            .expect("derived frame");
            let manual = output_frame(id, payload).expect("manual frame");
            assert_eq!(&*derived, &*manual, "Output payload len {}", payload.len());
            let derived = frame_bytes(&D2C::Replay {
                id,
                bytes: payload.to_vec(),
            })
            .expect("derived frame");
            let manual = replay_frame(id, payload).expect("manual frame");
            assert_eq!(&*derived, &*manual, "Replay payload len {}", payload.len());
        }
    }

    /// R4-T3: the HintPool overflow contract (r3-latency 5) — a full queue
    /// DROPS the job, it neither runs inline on the submitting (conn) thread
    /// nor panics. Worker-less pool so nothing drains the queue.
    #[test]
    fn hint_pool_overflow_drops_never_inline() {
        let pool = HintPool::with_shape(2, 0);
        let job = |label| HintJob {
            label,
            id: Uuid::new_v4(),
            tail: Vec::new(),
            tail_base: 0,
            replay: Vec::new(),
            recs: Vec::new(),
            cols: 80,
            rows: 24,
            targets: Vec::new(),
        };
        assert_eq!(pool.submit(job("a")), HintSubmit::Queued);
        assert_eq!(pool.submit(job("b")), HintSubmit::Queued);
        assert_eq!(
            pool.submit(job("overflow")),
            HintSubmit::DroppedFull,
            "beyond the cap the job must be dropped, never computed inline"
        );
        // The pool holds the receiver, so Disconnected (the inline fallback)
        // is unreachable while it lives.
        assert_eq!(pool.submit(job("still-full")), HintSubmit::DroppedFull);
    }

    /// R4-T2: the boot reap's deletion predicate. The failure mode of a
    /// regression here is "deletes live journals/sidecars", so every shape is
    /// pinned: orphan uuid files (all three artifact extensions) ⇒ true; the
    /// same names with a LIVE id ⇒ false; non-uuid stems ⇒ false (unknown
    /// formats untouched); and the file_stem() trap (`x.blocks.json` must
    /// classify by its FIRST dot-component).
    #[test]
    fn orphan_artifact_predicate() {
        let live_id = Uuid::new_v4();
        let dead_id = Uuid::new_v4();
        let live: HashSet<Uuid> = [live_id].into_iter().collect();
        for ext in ["log", "blocks.json", "log.tmp", "probe.json", "frame", "frame.tmp"] {
            assert!(
                is_orphan_artifact(&format!("{dead_id}.{ext}"), &live),
                "unknown-uuid .{ext} is an orphan"
            );
            assert!(
                !is_orphan_artifact(&format!("{live_id}.{ext}"), &live),
                "live-uuid .{ext} must NEVER be reaped"
            );
        }
        // Bare uuid (no extension): still classified by the leading component.
        assert!(is_orphan_artifact(&dead_id.to_string(), &live));
        assert!(!is_orphan_artifact(&live_id.to_string(), &live));
        // Non-uuid stems are never orphans — unknown formats are left alone.
        for name in ["daemon.log", "daemon.log.old", "readme.txt", "state.json", "", ".hidden"] {
            assert!(!is_orphan_artifact(name, &live), "{name:?} must not be reaped");
        }
        // Empty live set (the F1 disaster shape — now also gated at the call
        // site on a healthy state load): predicate itself still classifies
        // uuid files as orphans, which is why the health gate must exist.
        let none: HashSet<Uuid> = HashSet::new();
        assert!(is_orphan_artifact(&format!("{live_id}.log"), &none));
    }

    #[test]
    fn default_parser_defers_sync_blocks_until_esu() {
        // The GUI-side (default) processor holds the same bytes until ESU —
        // that is the flicker-free present; TermBackend::pump_sync enforces
        // the 150ms cap for blocks that never see their ESU.
        let mut term = new_term();
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"\x1b[?2026hHELLO");
        assert_eq!(row_text(&term, 0), "");
        parser.advance(&mut term, b"\x1b[?2026l");
        assert_eq!(row_text(&term, 0), "HELLO");
    }
}
