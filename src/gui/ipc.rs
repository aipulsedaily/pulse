//! GUI-side connection to the daemon, with auto-spawn.

use std::net::TcpStream;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::protocol::{write_frame, C2D, D2C, DaemonInfo, MAX_FRAME};
use crate::state::daemon_info_path;

pub struct IpcClient {
    writer: Mutex<TcpStream>,
    /// Frames paired with their socket-arrival Instant, so the app can
    /// measure scheduling latency (arrival → drain) when latency tracing is
    /// enabled (TC_TRACE_LATENCY=1).
    pub rx: Receiver<(Instant, D2C)>,
    pub connected: Arc<AtomicBool>,
    /// pid of the daemon we connected to (drives the restart notice, R8a).
    pub pid: u32,
    /// Protocol generation of the daemon we connected to. The GUI never
    /// sends C2D::BlockText to a proto < 2 daemon (an old daemon DROPS the
    /// client on an undecodable frame); everything else degrades silently.
    pub proto: u32,
    /// Millis-since-start of the last frame received; a stale value forces a
    /// reconnect even if the socket looks open (R7).
    last_rx_ms: Arc<AtomicU64>,
    epoch: Instant,
    /// The app's currently selected terminal, mirrored here each `logic()`
    /// tick (perf-wave-2). Output frames for OTHER terminals never paint the
    /// grid — only a sidebar dot/badge, which already animates on a 100ms
    /// cadence — so the reader coalesces their repaints to 100ms instead of
    /// letting 20 background streams pin the UI at 60fps.
    selected: Arc<Mutex<Option<Uuid>>>,
    /// r2-M3: bytes currently sitting in the (otherwise unbounded) `rx`
    /// channel. The drain is budgeted at ~2MiB/frame while N simultaneous
    /// floods against a minimized GUI can produce more than it consumes;
    /// past `QUEUE_BYTE_CAP` the reader DROPS the connection — mirroring
    /// the daemon's own wedged-client policy — and the existing reconnect
    /// path rebuilds losslessly from journal replay.
    queued: Arc<AtomicUsize>,
}

/// r2-M3: same order of magnitude as the daemon's per-client cap
/// (CLIENT_QUEUE_DEPTH 1024 × ≤64KiB = 64MB).
const QUEUE_BYTE_CAP: usize = 64 * 1024 * 1024;

/// Bytes one queued frame is accounted as (payload + a fixed overhead so
/// payload-less frames still count).
fn frame_cost(msg: &D2C) -> usize {
    64 + match msg {
        D2C::Output { bytes, .. } | D2C::Replay { bytes, .. } => bytes.len(),
        _ => 0,
    }
}

impl IpcClient {
    pub fn send(&self, msg: &C2D) {
        let mut w = self.writer.lock().unwrap();
        if write_frame(&mut *w, msg).is_err() {
            self.connected.store(false, Ordering::Relaxed);
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Seconds since the last frame arrived from the daemon.
    pub fn silent_secs(&self) -> u64 {
        let now = self.epoch.elapsed().as_millis() as u64;
        now.saturating_sub(self.last_rx_ms.load(Ordering::Relaxed)) / 1000
    }

    /// Mirror the app's selected terminal for the reader thread's repaint
    /// decisions. Called once per `logic()` tick — selection changes are
    /// input-driven, and input already repaints, so the mirror is never more
    /// than one frame stale.
    pub fn set_selected(&self, id: Option<Uuid>) {
        *self.selected.lock().unwrap() = id;
    }

    /// r2-M3: the drain calls this for every frame it pulls off `rx`, so the
    /// reader's byte accounting stays balanced.
    pub fn note_drained(&self, msg: &D2C) {
        self.queued.fetch_sub(frame_cost(msg), Ordering::Relaxed);
    }
}

/// What the IPC reader should do about a repaint for one incoming frame.
#[derive(Debug, PartialEq)]
enum RepaintDecision {
    /// Wake the UI now — interactive traffic (keystroke echo) rides this.
    Immediate,
    /// Coalesce into one repaint at the throttle window's edge.
    Defer(Duration),
}

/// Byte-gated trailing-edge repaint throttle (~60/s under load).
///
/// Under an output flood every 64KiB frame used to request its own repaint,
/// so egui painted back-to-back for the flood's whole duration; frames inside
/// a 16ms window are instead coalesced into one deferred repaint at the
/// window's edge — which always fires, so the queue drains even when a flood
/// stops mid-window.
///
/// The BYTE GATE is the typing-latency fix: keystroke echo arrives as SEVERAL
/// tiny chunks per key (PSReadLine and TUI renders write in pieces), and the
/// old time-only throttle deferred every chunk after the first — measured
/// +16–31ms on every echoed character, stacking with vsync into the
/// user-visible "echo keeps typing after I stop" lag. Throttling now only
/// engages once the current window has carried flood-sized volume; tiny
/// traffic always repaints immediately. Vsync still caps the real paint rate,
/// so typing burns no extra CPU.
struct RepaintGate {
    window_start: Option<Instant>,
    window_bytes: usize,
}

impl RepaintGate {
    const WINDOW: Duration = Duration::from_millis(16);
    const MIN_BYTES: usize = 32 * 1024;

    fn new() -> Self {
        Self {
            window_start: None,
            window_bytes: 0,
        }
    }

    fn on_frame(&mut self, now: Instant, frame_bytes: usize) -> RepaintDecision {
        match self.window_start {
            Some(t0) if now.duration_since(t0) < Self::WINDOW => {
                self.window_bytes = self.window_bytes.saturating_add(frame_bytes);
                if self.window_bytes < Self::MIN_BYTES {
                    RepaintDecision::Immediate
                } else {
                    RepaintDecision::Defer(Self::WINDOW - now.duration_since(t0))
                }
            }
            _ => {
                // Quiet spell over (or first frame ever): new window, leading
                // edge repaints immediately.
                self.window_start = Some(now);
                self.window_bytes = frame_bytes;
                RepaintDecision::Immediate
            }
        }
    }
}

/// Read one D2C frame, decoding the two payload-carrying hot variants by
/// hand. `bincode::deserialize` walks a `Vec<u8>` field BYTE-BY-BYTE through
/// serde's seq machinery — the mirror image of the daemon-side cost wave 1
/// killed with `output_frame` — and the generic `read_frame` also
/// zero-initializes a fresh buffer per frame. Here Output (variant 2) and
/// Replay (variant 1) payloads are `read_exact` straight into their final,
/// exactly-sized Vec (no scratch copy, no memset); every other variant is
/// rare and small and falls back to the derived Deserialize via `scratch`.
///
/// Wire layout (bincode 1.3 defaults: LE, fixed-width ints), pinned by the
/// daemon's `output_frame_matches_derived_encoding` and this file's
/// `hand_decode_matches_derived_encoding`:
///   [variant: u32][uuid len: u64 = 16][uuid: 16B][payload len: u64][payload]
fn read_d2c<R: std::io::Read>(r: &mut R, scratch: &mut Vec<u8>) -> anyhow::Result<D2C> {
    use std::io::Read as _;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    anyhow::ensure!(len as u32 <= MAX_FRAME, "frame too large: {len}");

    const HDR: usize = 4 + 8 + 16 + 8;
    let mut hdr = [0u8; HDR];
    let n0 = len.min(HDR);
    r.read_exact(&mut hdr[..n0])?;

    if len >= HDR {
        let variant = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        if variant == 1 || variant == 2 {
            let uuid_len = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
            let payload_len = u64::from_le_bytes(hdr[28..36].try_into().unwrap());
            if uuid_len == 16 && payload_len == (len - HDR) as u64 {
                let id = uuid::Uuid::from_slice(&hdr[12..28])?;
                let mut bytes = vec![0u8; len - HDR];
                r.read_exact(&mut bytes)?;
                return Ok(if variant == 1 {
                    D2C::Replay { id, bytes }
                } else {
                    D2C::Output { id, bytes }
                });
            }
            // Structurally not what the derive emits — fall through and let
            // the derived Deserialize be the authority.
        }
    }

    scratch.clear();
    scratch.extend_from_slice(&hdr[..n0]);
    if len > n0 {
        let read = r.take((len - n0) as u64).read_to_end(scratch)?;
        anyhow::ensure!(read == len - n0, "socket closed mid-frame");
    }
    Ok(bincode::deserialize(scratch)?)
}

fn try_connect(ctx: egui::Context) -> anyhow::Result<IpcClient> {
    let info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    if info.proto < 2 {
        // Version skew: this GUI understands the blocks UI but the running
        // daemon predates it. Everything else still works — bincode D2C
        // indices are append-only — so warn rather than refuse. Single-exe
        // ships both roles; skew is transient.
        log::warn!(
            "daemon protocol {} predates blocks UI (P2); restart the daemon from this build",
            info.proto
        );
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], info.port));
    let stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    stream.set_nodelay(true)?;
    // TCP keepalive so a dead daemon eventually errors our reads (R7).
    let sock = socket2::SockRef::from(&stream);
    let _ = sock.set_keepalive(true);
    let _ = sock.set_tcp_keepalive(&socket2::TcpKeepalive::new().with_time(Duration::from_secs(15)));

    let mut writer = stream.try_clone()?;
    // Generation-carrying handshake (proto 12): the daemon needs to know we
    // re-attach ourselves on D2C::Reset so it suppresses its blind-size
    // Replay push in the restore resync (the width-mismatch garble fix). An
    // older daemon cannot decode the appended variant — it gets the legacy
    // Hello (and keeps its legacy push; the GUI gates the re-attach on
    // `proto >= 12` to match, so nothing doubles under skew).
    if info.proto >= 12 {
        write_frame(
            &mut writer,
            &C2D::Hello2 { token: info.token, proto: crate::protocol::PROTO },
        )?;
    } else {
        write_frame(&mut writer, &C2D::Hello { token: info.token })?;
    }

    let (tx, rx) = std::sync::mpsc::channel::<(Instant, D2C)>();
    let connected = Arc::new(AtomicBool::new(true));
    let connected_reader = connected.clone();
    let epoch = Instant::now();
    let last_rx_ms = Arc::new(AtomicU64::new(0));
    let last_rx_reader = last_rx_ms.clone();
    let selected: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let selected_reader = selected.clone();
    let queued = Arc::new(AtomicUsize::new(0));
    let queued_reader = queued.clone();
    let mut read_stream = stream;
    std::thread::Builder::new()
        .name("ipc-reader".into())
        .spawn(move || {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let mut gate = RepaintGate::new();
                let mut scratch: Vec<u8> = Vec::new();
                while let Ok(msg) = read_d2c(&mut read_stream, &mut scratch) {
                    last_rx_reader.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                    let sz = match &msg {
                        D2C::Output { bytes, .. } | D2C::Replay { bytes, .. } => bytes.len(),
                        _ => 0,
                    };
                    // Output for a non-selected terminal repaints
                    // nothing but a sidebar dot/badge (100ms-cadence
                    // chrome): coalesce to that cadence instead of
                    // painting the whole window per chunk. Selection
                    // is None only before the first Snapshot, when
                    // there is nothing to paint anyway. Every other
                    // frame kind keeps its immediate/gated path.
                    let background = match &msg {
                        D2C::Output { id, .. } => {
                            *selected_reader.lock().unwrap() != Some(*id)
                        }
                        _ => false,
                    };
                    // r2-M3: byte-cap the channel — a wedged/minimized drain
                    // must not accumulate unboundedly. Dropping the
                    // connection forces the lossless reconnect+replay path.
                    let cost = frame_cost(&msg);
                    if queued_reader.fetch_add(cost, Ordering::Relaxed) + cost > QUEUE_BYTE_CAP {
                        log::warn!(
                            "ipc receive queue exceeded {}MB — dropping the connection to rebuild from replay",
                            QUEUE_BYTE_CAP / (1024 * 1024)
                        );
                        break;
                    }
                    if tx.send((Instant::now(), msg)).is_err() {
                        break;
                    }
                    if background {
                        ctx.request_repaint_after(Duration::from_millis(100));
                    } else {
                        match gate.on_frame(Instant::now(), sz) {
                            RepaintDecision::Immediate => ctx.request_repaint(),
                            RepaintDecision::Defer(d) => ctx.request_repaint_after(d),
                        }
                    }
                }
            }));
            connected_reader.store(false, Ordering::Relaxed);
            ctx.request_repaint();
        })?;

    Ok(IpcClient {
        writer: Mutex::new(writer),
        rx,
        connected,
        pid: info.pid,
        proto: info.proto,
        last_rx_ms,
        epoch,
        selected,
        queued,
    })
}

fn spawn_daemon() -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS only — NEVER CREATE_NEW_PROCESS_GROUP: that flag
    // starts the new group with Ctrl+C DISABLED, a default that ConPTY
    // children inherit, so native commands in every terminal the daemon owns
    // (ping, ssh, …) silently ignore Ctrl+C. Found by the keys probe: it
    // passed against a directly-started daemon and failed against an
    // installed/GUI-spawned one. The flag bought nothing here anyway — a
    // detached process has no console to share signals with.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    // Prefer the stable installed copy so a moved build dir can't orphan things.
    let exe = crate::installed_exe_path()
        .filter(|p| p.exists())
        .map(Ok)
        .unwrap_or_else(std::env::current_exe)?;
    std::process::Command::new(exe)
        .arg("--daemon")
        // v0.1.2 field bug A (hygiene): a shortcut/Update.exe-launched GUI
        // runs with CWD=<velopack-root>\current\, and everything it spawns
        // inherits that — an open CWD handle inside `current\` blocks the
        // update swap and the uninstall rmdir (Update.exe's process sweep
        // only kills exes under the root; the daemon runs from data bin\ and
        // survives it). Pin the long-lived daemon to the data dir, which
        // exists before this call and outlives every install.
        .current_dir(crate::state::data_dir())
        .creation_flags(DETACHED_PROCESS)
        .spawn()?;
    Ok(())
}

/// Connect to a running daemon (if any) and ask it to shut down. Best effort:
/// used by `--install` before replacing the binary.
pub fn request_shutdown() -> anyhow::Result<()> {
    request_shutdown_at(&daemon_info_path())
}

/// `request_shutdown` against an explicit daemon.json. The rebrand data-dir
/// migration hand-shakes the OLD daemon, whose socket info lives in the old
/// dir while `daemon_info_path()` already resolves to the new one.
pub fn request_shutdown_at(info_path: &std::path::Path) -> anyhow::Result<()> {
    let info: DaemonInfo = serde_json::from_slice(&std::fs::read(info_path)?)?;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], info.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    write_frame(&mut stream, &C2D::Hello { token: info.token })?;
    write_frame(&mut stream, &C2D::Shutdown)?;
    // Don't drop the socket the instant the bytes are queued: the daemon
    // answers Hello with a Snapshot, and writing that to an already-closed
    // socket RSTs the connection — which discards its receive buffer, i.e.
    // the still-unread Shutdown frame. (Observed live: --install's shutdown
    // was a no-op and the binary copy failed with "file in use".) Read until
    // the daemon closes its end on exit, bounded so a wedged daemon can't
    // hang the installer.
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut buf = [0u8; 4096];
    use std::io::Read;
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break, // daemon closed, or bounded timeout
            Ok(_) => {}
        }
    }
    Ok(())
}

/// Connect to the daemon, launching it if needed.
pub fn connect_or_spawn(ctx: egui::Context) -> anyhow::Result<IpcClient> {
    if let Ok(client) = try_connect(ctx.clone()) {
        return Ok(client);
    }
    spawn_daemon()?;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(client) = try_connect(ctx.clone()) {
            return Ok(client);
        }
    }
    anyhow::bail!("could not reach the daemon after launching it")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-rolled reader must decode EXACTLY what the derived
    /// Deserialize would, for the hot variants (any payload shape, empty
    /// included), for small non-hot variants shorter than the fixed header
    /// (Pong), and for structured ones (Blocks/PromptState). Equality is
    /// checked by re-encoding the decoded value: bincode encoding is
    /// deterministic, so byte-identical re-encode ⇔ identical value. Frames
    /// are streamed through one Cursor to prove framing stays aligned.
    #[test]
    fn hand_decode_matches_derived_encoding() {
        let id = uuid::Uuid::new_v4();
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let msgs = vec![
            D2C::Output { id, bytes: b"x".to_vec() },
            D2C::Output { id, bytes: Vec::new() },
            D2C::Replay { id, bytes: big },
            D2C::Pong,
            D2C::Exited { id, code: Some(3) },
            D2C::StreamPos { id, off: 987654321 },
            D2C::PromptState { id, at_prompt: true, line: -4, col: 17, clean: false },
            D2C::Error { message: "boom".into() },
            D2C::Output { id, bytes: b"hello \x1b[31m wor\xffld\x00".to_vec() },
        ];
        let mut wire = Vec::new();
        let mut encodings = Vec::new();
        for m in &msgs {
            let enc = bincode::serialize(m).unwrap();
            wire.extend_from_slice(&(enc.len() as u32).to_le_bytes());
            wire.extend_from_slice(&enc);
            encodings.push(enc);
        }
        let mut cursor = std::io::Cursor::new(wire);
        let mut scratch = Vec::new();
        for (i, enc) in encodings.iter().enumerate() {
            let decoded = read_d2c(&mut cursor, &mut scratch).expect("decode");
            assert_eq!(
                &bincode::serialize(&decoded).unwrap(),
                enc,
                "frame {i} decoded to a different value"
            );
        }
    }

    /// Typing echo — a few tiny chunks per keystroke, keystrokes far apart —
    /// must NEVER be deferred. This is the regression test for the reported
    /// "I stop typing and it keeps typing" lag: the old time-only throttle
    /// deferred every chunk that landed within 16ms of the previous one.
    #[test]
    fn typing_echo_is_never_deferred() {
        let mut gate = RepaintGate::new();
        let t0 = Instant::now();
        // 20 keystrokes at ~80ms cadence, each echoing as 3 chunks 2ms apart.
        for key in 0..20u64 {
            let key_t = t0 + Duration::from_millis(key * 80);
            for chunk in 0..3u64 {
                let now = key_t + Duration::from_millis(chunk * 2);
                assert_eq!(
                    gate.on_frame(now, 48),
                    RepaintDecision::Immediate,
                    "echo chunk {chunk} of key {key} was throttled"
                );
            }
        }
    }

    /// A flood (64KiB frames back-to-back) must coalesce: leading edge
    /// repaints, followers inside the window defer to the window's edge.
    #[test]
    fn flood_coalesces_to_window_edge() {
        let mut gate = RepaintGate::new();
        let t0 = Instant::now();
        assert_eq!(gate.on_frame(t0, 64 * 1024), RepaintDecision::Immediate);
        for i in 1..8u64 {
            let now = t0 + Duration::from_millis(i * 2);
            match gate.on_frame(now, 64 * 1024) {
                RepaintDecision::Defer(d) => {
                    assert!(d <= RepaintGate::WINDOW, "deferral exceeds the window");
                }
                RepaintDecision::Immediate => panic!("flood frame {i} not throttled"),
            }
        }
        // Next window: leading edge again (fixed cadence, not a resettable
        // debounce — a debounce here would postpone paints for a flood's
        // whole duration).
        let next = t0 + Duration::from_millis(17);
        assert_eq!(gate.on_frame(next, 64 * 1024), RepaintDecision::Immediate);
    }

    /// Small frames right after a flood window expires are immediate again —
    /// the gate never leaks throttling into a following quiet spell.
    #[test]
    fn quiet_after_flood_is_immediate() {
        let mut gate = RepaintGate::new();
        let t0 = Instant::now();
        let _ = gate.on_frame(t0, 64 * 1024);
        let _ = gate.on_frame(t0 + Duration::from_millis(1), 64 * 1024);
        let later = t0 + Duration::from_millis(40);
        assert_eq!(gate.on_frame(later, 32), RepaintDecision::Immediate);
    }

    /// Accumulated small frames that add up to flood volume within one window
    /// DO trip the gate — the throttle is about volume, not frame count.
    #[test]
    fn accumulated_volume_trips_the_gate() {
        let mut gate = RepaintGate::new();
        let t0 = Instant::now();
        assert_eq!(gate.on_frame(t0, 20 * 1024), RepaintDecision::Immediate);
        assert_eq!(
            gate.on_frame(t0 + Duration::from_millis(2), 8 * 1024),
            RepaintDecision::Immediate
        );
        match gate.on_frame(t0 + Duration::from_millis(4), 8 * 1024) {
            RepaintDecision::Defer(_) => {}
            RepaintDecision::Immediate => panic!("36KiB within one window not throttled"),
        }
    }
}
