//! Env-gated cumulative stage timers for the daemon's hot paths.
//!
//! Set `TC_PERF_STAGES=1` in the daemon's environment to enable. When off
//! (the default — the installed daemon never sets it), every call site pays
//! exactly one branch on a cached bool; no `Instant::now()` is taken.
//!
//! When on, the 250ms flush tick logs a cumulative `[perf]` line to
//! daemon.log whenever ingest advanced since the last dump, attributing
//! CPU-ish wall time to: mirror parse, journal append, fanout frame build,
//! fanout enqueue, raw-byte scanners, query responses, client socket writes,
//! and tracker passes. Attach serialization is logged per-attach at its call
//! site (rare, so per-event lines beat counters there).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Mirror Term lock + vte parse (Core::ingest).
pub static PARSE_NS: AtomicU64 = AtomicU64::new(0);
/// Journal file append (Core::ingest).
pub static APPEND_NS: AtomicU64 = AtomicU64::new(0);
/// Output frame build: bincode serialize + length framing (Core::fanout).
pub static FRAME_NS: AtomicU64 = AtomicU64::new(0);
/// Fanout recipient checks + queue pushes (Core::fanout).
pub static ENQUEUE_NS: AtomicU64 = AtomicU64::new(0);
/// Raw-byte scanners: BlockScanner + OscScanner + ModeScanner (reader thread).
pub static SCAN_NS: AtomicU64 = AtomicU64::new(0);
/// VT query responses (session::respond_to_queries).
pub static RESPOND_NS: AtomicU64 = AtomicU64::new(0);
/// Client-writer socket write_all (wall, not CPU — includes backpressure).
pub static SOCK_NS: AtomicU64 = AtomicU64::new(0);
/// One tracker pass: process snapshot + per-session analyze (run() tick).
pub static TRACK_NS: AtomicU64 = AtomicU64::new(0);
/// Total bytes / chunks through Core::ingest.
pub static INGEST_BYTES: AtomicU64 = AtomicU64::new(0);
pub static INGEST_CHUNKS: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TC_PERF_STAGES").is_some_and(|v| v != "0"))
}

/// Daemon process start, set at the top of `daemon::run` (perf-wave-3 boot
/// timeline). Stage lines log ms since this origin because daemon.log
/// timestamps have 1-second resolution — useless for sub-second phases.
static T0: OnceLock<Instant> = OnceLock::new();

pub fn mark_t0() {
    let _ = T0.set(Instant::now());
}

/// Milliseconds since `mark_t0` (0 when never marked, e.g. probe processes).
pub fn boot_ms() -> u64 {
    T0.get().map(|t0| t0.elapsed().as_millis() as u64).unwrap_or(0)
}

/// Run `f`, attributing its wall time to `slot` when perf is on.
#[inline]
pub fn time<R>(slot: &AtomicU64, f: impl FnOnce() -> R) -> R {
    if !on() {
        return f();
    }
    let t0 = Instant::now();
    let r = f();
    slot.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
    r
}

/// Called from the daemon's 250ms flush tick: logs a cumulative snapshot
/// whenever ingest or the tracker advanced since the last dump.
pub fn dump_if_active() {
    if !on() {
        return;
    }
    static LAST: AtomicU64 = AtomicU64::new(u64::MAX);
    let bytes = INGEST_BYTES.load(Ordering::Relaxed);
    let track_ms = TRACK_NS.load(Ordering::Relaxed) / 1_000_000;
    // Fold tracker progress in so a 20-idle-session soak still dumps.
    let sig = bytes ^ (track_ms << 1);
    if sig == LAST.swap(sig, Ordering::Relaxed) {
        return;
    }
    let ms = |a: &AtomicU64| a.load(Ordering::Relaxed) / 1_000_000;
    log::info!(
        "[perf] bytes={bytes} chunks={} parse_ms={} append_ms={} frame_ms={} enqueue_ms={} scan_ms={} respond_ms={} sock_ms={} track_ms={}",
        INGEST_CHUNKS.load(Ordering::Relaxed),
        ms(&PARSE_NS),
        ms(&APPEND_NS),
        ms(&FRAME_NS),
        ms(&ENQUEUE_NS),
        ms(&SCAN_NS),
        ms(&RESPOND_NS),
        ms(&SOCK_NS),
        track_ms,
    );
}
