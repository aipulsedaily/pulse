//! IPC protocol between the GUI client and the daemon.
//!
//! Transport: TCP on 127.0.0.1 (loopback only). The daemon binds an ephemeral
//! port and writes `daemon.json` (port + auth token + pid) into the data dir
//! with user-private ACLs (default for %LOCALAPPDATA%). Clients must present
//! the token in `Hello` before anything else is accepted.
//!
//! Framing: u32 little-endian length prefix followed by a bincode-encoded
//! message. Output chunks are raw bytes inside the message, so the terminal
//! stream is never re-encoded as text.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use uuid::Uuid;

use crate::state::{BlockRec, Folder, InnerCli, NewTerminal, SharedState};

pub const MAX_FRAME: u32 = 32 * 1024 * 1024;

/// This build's protocol generation — the single source for `DaemonInfo::
/// proto` and `C2D::Hello2::proto`. History lives at the `proto:` field in
/// `daemon::run` (src\daemon\mod.rs).
pub const PROTO: u32 = 13;

/// Client -> Daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum C2D {
    Hello { token: String },

    // Folder management
    CreateFolder { name: String },
    RenameFolder { id: Uuid, name: String },
    DeleteFolder { id: Uuid },
    SetFolderCollapsed { id: Uuid, collapsed: bool },
    MoveFolder { id: Uuid, delta: i32 },

    // Terminal lifecycle
    CreateTerminal { spec: NewTerminal },
    RenameTerminal { id: Uuid, name: String },
    MoveTerminal { id: Uuid, folder: Option<Uuid> },
    ReorderTerminal { id: Uuid, delta: i32 },
    DeleteTerminal { id: Uuid },
    /// Relaunch a dead terminal through its resume adapter.
    RestartTerminal { id: Uuid },
    KillTerminal { id: Uuid },
    SetAutoRestore { id: Uuid, auto: bool },

    // I/O
    /// Subscribe to a terminal's output. Daemon replies with Replay followed
    /// by live Output frames. `cols`/`rows` are the attaching client's grid
    /// (0 = unknown): the daemon resizes the session to match BEFORE
    /// serializing, so the reconstruction — including the relative cursor
    /// placement — is exact at the client's height, and the resize itself
    /// makes the shell repaint into the new geometry.
    Attach { id: Uuid, cols: u16, rows: u16 },
    Detach { id: Uuid },
    Input { id: Uuid, bytes: Vec<u8> },
    Resize { id: Uuid, cols: u16, rows: u16 },

    /// Ask the daemon to shut down (terminals die; journals/state are flushed).
    Shutdown,

    /// Liveness probe; the daemon replies with Pong. Lets an idle client detect
    /// a half-open socket and force a reconnect.
    Ping,

    /// Debug/testing (used by `--probe`): dump every live session's grid
    /// dimensions — headless Term, PTY, persisted state — to
    /// `debug_dump.json` in the data dir. The reply is file-based so no new
    /// D2C variant (and thus no GUI change) is needed.
    DebugDump,

    /// Ask for one block's output text: journal bytes start_off..end_off (or
    /// ..head for an open block), ANSI/OSC-stripped, size-capped. Answered
    /// with D2C::BlockText to the requesting client only. Unknown start_off
    /// is logged and silently dropped.
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    BlockText { id: Uuid, start_off: u64 },

    /// Controller handshake (P5) — the alternative first frame to `Hello`.
    /// `token` is either the master daemon.json token (FULL rights) or a
    /// scoped controller token from ctl-tokens.json. `self_session` is the
    /// TC_SESSION_ID env of the terminal this controller runs inside, if any;
    /// the daemon refuses Run/SendRaw/SendChord/Kill/Restart/Delete against
    /// that id unless the request sets force_self (recursion guard).
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    HelloCtl {
        token: String,
        self_session: Option<Uuid>,
    },
    /// Typed controller request (P5). `req_id` is client-chosen and echoed on
    /// every reply/stream frame for this request; the client keeps it unique
    /// among its own in-flight requests (the daemon only echoes it).
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    Ctl { req_id: u64, req: CtlRequest },

    /// Submit a command line to a terminal the daemon should ALSO record as a
    /// block (P6b §5.2 — exec-less shells: cmd, whose PROMPT-macro hooks can
    /// carry a `pre` but no exec, so typed/submitted commands are otherwise
    /// invisible to the block store). Single-line only; multi-line is refused
    /// with a D2C::Error frame (cmd executes each line at its own prompt).
    /// `write: true` ⇒ the daemon computes submission_bytes from the mirror
    /// (P3/P5-identical), writes them to the PTY, and opens a SYNTHETIC block
    /// at the pre-write journal head. `write: false` ⇒ record-only: the bytes
    /// already went via Input (a GUI-observed raw Enter at a cmd prompt);
    /// open the synthetic block at the current head, write nothing. Either
    /// way the NEXT token-checked `pre` closes the block — exit stays None
    /// (cmd has no per-command exit codes, permanently — D7), duration and
    /// cwd (via the adjacent OSC 9;9) are real. FULL-scope legacy verb
    /// (scoped controllers use Ctl Run, which routes through the same
    /// synthetic-open helper for Cmd-family terminals).
    ///
    /// APPENDED at the enum's end (the C2D tail; proto 5 → 6): bincode
    /// encodes variants positionally.
    SubmitCommand { id: Uuid, cmd: String, write: bool },

    /// Set or clear a terminal's sidebar color tag (task #22). `tag` indexes
    /// the GUI's curated swatch table; None clears. Persisted in
    /// `TerminalMeta.color_tag`.
    ///
    /// APPENDED at the enum's end (the C2D tail; proto 7 → 8): bincode
    /// encodes variants positionally.
    SetColorTag { id: Uuid, tag: Option<u8> },
    /// Folder flavor of SetColorTag (persisted in `Folder.color_tag`).
    ///
    /// APPENDED at the enum's end (proto 8): bincode encodes variants
    /// positionally.
    SetFolderColor { id: Uuid, tag: Option<u8> },

    /// SLEEP (proto 9): put a running terminal to sleep — set the persisted
    /// `asleep` flag, fail its non-Exit waiters, drain its output tail
    /// (≤2s), kill its process tree; the exit-watcher → on_exit path does
    /// all bookkeeping. Journal/blocks/meta/pinned-CLI identity persist
    /// exactly like a daemon shutdown; boot restore skips the terminal
    /// until an explicit wake. FULL-scope legacy verb (scoped controllers
    /// use Ctl::Sleep, which carries refusal semantics; this one is the
    /// GUI's post-confirm fire-and-forget — no busy gate here, the GUI
    /// gated/confirmed already). Executed on a worker thread (a 2s inline
    /// drain would freeze every other terminal on the connection); results
    /// arrive via the normal Snapshot + Exited broadcasts.
    ///
    /// APPENDED at the enum's end (the C2D tail; proto 8 → 9): bincode
    /// encodes variants positionally.
    SleepTerminal { id: Uuid },
    /// Sleep every presented-Running terminal in a folder, sharing ONE
    /// drain window (never fan out N SleepTerminal sends — DO-NOT 5).
    ///
    /// APPENDED at the enum's end (proto 9): bincode encodes variants
    /// positionally.
    SleepFolder { folder: Uuid },
    /// Wake every presented-Asleep terminal in a folder, staggered
    /// daemon-side through the boot-restore lanes (S17). Single-terminal
    /// wake rides the existing RestartTerminal — launch() clears the
    /// asleep flag, so a duplicate verb would be wire noise.
    ///
    /// APPENDED at the enum's end (proto 9): bincode encodes variants
    /// positionally.
    WakeFolder { folder: Uuid },

    /// SSH AUTO-RECONNECT (proto 10): stop the reconnect supervision for
    /// `id` — remove the backoff entry and clear `TerminalMeta.reconnecting`.
    /// An attempt already in flight is LEFT RUNNING (it is a real ssh
    /// process the user can watch or kill); a terminal waiting for its next
    /// backoff stays Dead with the ordinary Restore affordances.
    ///
    /// APPENDED at the enum's end (the C2D tail; proto 9 → 10): bincode
    /// encodes variants positionally.
    CancelReconnect { id: Uuid },
    /// Persist the ssh auto-reconnect opt-in/out (ShellCfg.auto_reconnect;
    /// default true). Off also cancels any active supervision.
    ///
    /// APPENDED at the enum's end (proto 10): bincode encodes variants
    /// positionally.
    SetAutoReconnect { id: Uuid, on: bool },

    /// Generation-carrying GUI handshake (the alternative first frame to
    /// `Hello`; same master token ⇒ FULL rights). `proto` is the CLIENT's
    /// protocol generation, which the daemon needs for the width-mismatch
    /// garble fix: a proto ≥ 12 client re-attaches itself on `D2C::Reset`
    /// (announcing its real grid so ConPTY is resized BEFORE the replay is
    /// serialized), so the daemon must suppress its own blind-size Replay
    /// push in the restore resync for such clients — and must NOT for
    /// legacy clients, which would otherwise stare at a blank grid forever.
    /// Clients send this only to a proto ≥ 12 daemon (an older daemon fails
    /// to decode the unknown variant and drops the connection).
    ///
    /// APPENDED at the enum's end (the C2D tail; proto 11 → 12): bincode
    /// encodes variants positionally.
    Hello2 { token: String, proto: u32 },

    /// MANUAL SSH RETRY (proto 13, dead-relaunch fix b): user-initiated
    /// bounded reconnect for a Dead ssh terminal — the Dead lane's
    /// `Retry ▸`. Enters the EXISTING reconnect.rs supervision (2s/10s/30s
    /// backoff, capped at 3 attempts, cancellable via CancelReconnect)
    /// WITHOUT the `hooks_were_live` auto-qualification: the automatic
    /// path keeps that gate untouched (auth-wall hosts must never
    /// blind-retry AUTOMATICALLY), while an explicit user click is its own
    /// consent — and the 30s-no-hooks interactive-auth stop still applies
    /// to every attempt, so a manual loop can never hammer a password
    /// prompt either. No-op for non-ssh / asleep / non-Dead /
    /// already-supervised terminals. The GUI gates the send on the daemon
    /// generation (an older daemon drops the connection on an undecodable
    /// C2D variant — the color-tag/sleep skew pattern).
    ///
    /// APPENDED at the enum's end (the C2D tail; proto 12 → 13): bincode
    /// encodes variants positionally.
    RetryReconnect { id: Uuid },
}

/// One session's dimensions as written by `C2D::DebugDump`. The three pairs
/// must always agree; a probe asserting that catches any resize-pipeline
/// divergence (e.g. a resize lost while a spawn was in flight).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugTermInfo {
    pub id: Uuid,
    pub term_cols: u16,
    pub term_rows: u16,
    pub pty_cols: u16,
    pub pty_rows: u16,
    pub state_cols: u16,
    pub state_rows: u16,
}

/// Daemon -> Client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum D2C {
    /// Sent on successful Hello and after every state mutation — EXCEPT
    /// inner_cli tracking changes, which are captured-on-change SAVED but
    /// deliberately NOT broadcast (hook-fed families skip the tracker tick
    /// that coalesces broadcasts). A client that needs live inner_cli must
    /// not poll Snapshot for it.
    Snapshot { state: SharedState },
    /// Screen reconstruction sent right after Attach, before any live output.
    /// For live primary-screen sessions this is a serialization of the
    /// daemon's grid (scrollback + screen + cursor + modes) — height
    /// independent and seam-free — not raw journal bytes. Alt-screen and dead
    /// terminals fall back to the raw journal tail.
    Replay { id: Uuid, bytes: Vec<u8> },
    Output { id: Uuid, bytes: Vec<u8> },
    /// The client must discard its terminal state for `id`; a fresh Replay
    /// follows. Sent when the daemon rewrites a terminal's world (restore).
    Reset { id: Uuid },
    /// Terminal process exited.
    Exited { id: Uuid, code: Option<u32> },
    Error { message: String },
    /// Reply to Ping.
    Pong,
    /// Journal Blocks for a terminal. Sent full (whole capped list, replaces
    /// the client's copy) right after the Replay on Attach, and incremental
    /// (just the changed records) on every block open/close. Clients upsert
    /// keyed by (epoch, start_off).
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally, so
    /// appending preserves every existing wire index.
    Blocks {
        id: Uuid,
        epoch: u32,
        full: bool,
        recs: Vec<BlockRec>,
    },
    /// Sent immediately after EVERY Replay (attach and restore-resync): the
    /// absolute journal stream offset at which live Output frames resume. The
    /// GUI anchors block records to grid rows by counting Output bytes from
    /// this base. Captured under the same journal lock as the Replay
    /// snapshot, so it is exact (ingest holds that lock across append+fanout,
    /// making Output frames a gapless suffix of the journal stream).
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    StreamPos { id: Uuid, off: u64 },
    /// Reply to C2D::BlockText (requester only, never broadcast).
    BlockText {
        id: Uuid,
        start_off: u64,
        text: String,
        truncated: bool,
    },
    /// Cold-attach prompt certification (task #15, proto 3): sent once at the
    /// END of the attach/resync sequence (after Replay/StreamPos/Blocks). Tells
    /// the GUI whether the session is sitting at an interactive prompt and
    /// where its prompt end is, so the composer can arm with the cover on the
    /// instant the app opens instead of waiting for the next live prompt.
    /// `line`/`col` are in the just-serialized replay's coordinate space
    /// (0-based screen row, column); `clean` is true only when the input
    /// buffer is empty (cursor still at the prompt end) — a dirty prompt is
    /// `at_prompt: true, clean: false` and the GUI falls back to manual arm.
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    /// (The P5 controller channel appended `Ctl` after this and bumped
    /// proto to 4, as this comment once demanded.)
    PromptState {
        id: Uuid,
        at_prompt: bool,
        line: i32,
        col: u32,
        clean: bool,
    },
    /// Controller reply or event stream frame (P5). One-shot verbs get exactly
    /// one frame; Subscribe and Run{wait}/Wait get their frames later, when
    /// the condition resolves (still tagged with the originating req_id).
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    Ctl { req_id: u64, body: CtlBody },
    /// Restored-history anchors (proto 7): where each persisted block's
    /// prompt/command row and each superseded bare-prompt (spacer) row landed
    /// in the just-sent Replay, in replay coordinate space (the same space
    /// PromptState uses: 0-based screen row, negative = scrollback). Computed
    /// daemon-side by re-parsing the journal tail with per-hook checkpoints
    /// and locating each row in a parse of the actual Replay bytes — rows
    /// that can't be located exactly are simply absent (drop, never guess).
    /// Sent at the very END of the attach/resync sequence (after
    /// Replay/StreamPos/Blocks[/PromptState]), OUTSIDE the journal lock, so
    /// live Output frames may precede it: the GUI re-bases rows by the
    /// history growth since its Replay parse. The GUI joins block hints to
    /// BlockRecs by `start_off`, re-verifies each row against its own grid,
    /// and mints history covers + block anchors for pre-attach rows.
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    ReplayAnchors { id: Uuid, items: Vec<AnchorHint> },
}

/// `AnchorHint.kind`: the row is a recorded block's prompt+command row; join
/// to the BlockRec whose `start_off` matches.
pub const ANCHOR_BLOCK: u8 = 0;
/// `AnchorHint.kind`: the row is a superseded bare prompt (pre-without-exec —
/// an empty Enter / Ctrl+C at a prompt). `start_off` is the prompt-end (OSC
/// 133;B) stream offset — an identity, not a join key.
pub const ANCHOR_SPACER: u8 = 1;

/// One restored-history row hint (see D2C::ReplayAnchors). Wire-positional,
/// append-only, like every protocol struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorHint {
    pub start_off: u64,
    /// Replay-space grid row (0-based screen, negative = scrollback).
    pub row: i32,
    /// Grid column where the shell's input area begins on that row (the
    /// prompt end — where the command text starts).
    pub col: u32,
    /// ANCHOR_BLOCK or ANCHOR_SPACER.
    pub kind: u8,
}

// ───────────────────────── controller catalog (P5) ─────────────────────────
//
// All of these are bincode-positional: append-only forever, same as C2D/D2C.
// No fields may be added to existing variants; new variants go at the END.

/// Scope bitflags. FULL is reserved for the master token: it additionally
/// unlocks the legacy C2D verbs (the GUI protocol) and Token*/Shutdown.
pub const SCOPE_READ: u32 = 1; // List, Read*, Wait, Subscribe/Unsubscribe
pub const SCOPE_INPUT: u32 = 2; // Run, SendRaw, SendChord
pub const SCOPE_MANAGE: u32 = 4; // CreateTerminal/Folder, Kill, Restart, Delete
pub const SCOPE_FULL: u32 = u32::MAX;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CtlRequest {
    List,
    CreateTerminal {
        spec: NewTerminal,
    },
    CreateFolder {
        name: String,
    },
    /// Submit a command line to a hooked shell at a (best-effort) idle prompt.
    /// Refused when a block is open / alt-screen / hooks unverified, unless
    /// `force`. `wait`: daemon-side composite — the reply arrives when the
    /// block spawned by this submission closes (RunDone) or timeout.
    Run {
        id: Uuid,
        cmd: String,
        force: bool,
        force_self: bool,
        wait: Option<RunWait>,
    },
    /// Raw bytes to the PTY, ungated by design (driving TUIs is its purpose).
    SendRaw {
        id: Uuid,
        bytes: Vec<u8>,
        force_self: bool,
    },
    /// A named key chord, encoded daemon-side per the session's input mode.
    SendChord {
        id: Uuid,
        chord: CtlChord,
        force_self: bool,
    },
    /// The visible grid as text (works for TUIs/claude; alt-screen reads the
    /// active — alt — grid, which is exactly what the caller wants).
    ReadScreen {
        id: Uuid,
    },
    /// Last `lines` complete lines of the journal tail, ANSI/OSC-stripped.
    ReadTail {
        id: Uuid,
        lines: u32,
    },
    ReadBlocks {
        id: Uuid,
        last: u32,
    },
    /// Same semantics as C2D::BlockText, delivered as a Ctl reply.
    ReadBlockText {
        id: Uuid,
        start_off: u64,
    },
    Wait {
        id: Uuid,
        cond: WaitCond,
        timeout_ms: u64,
    },
    Kill {
        id: Uuid,
        force_self: bool,
    },
    Restart {
        id: Uuid,
        force_self: bool,
    },
    Delete {
        id: Uuid,
        force_self: bool,
    },
    Subscribe {
        ids: Option<Vec<Uuid>>,
        kinds: u32, // EV_* bitflags
    },
    Unsubscribe {
        req_id: u64, // the Subscribe's req_id
    },
    TokenCreate {
        name: String,
        scope: u32,
    }, // master token only
    TokenRevoke {
        name: String,
    }, // master token only
    TokenList, // master token only
    /// SLEEP (proto 9, S6): controller sleep with refusal semantics.
    /// Refusals: not_found | asleep (already) | sleeping (drain in flight)
    /// | dead | not_running (spawn in flight) | busy (open block or output
    /// within 3s — `force` bypasses). Inline on the controller's own conn
    /// thread; the reply is Done after the kill is issued.
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    Sleep {
        id: Uuid,
        force: bool,
        force_self: bool,
    },
    /// Wake an asleep terminal (launch() — the boot-restore path — clears
    /// the flag). Refuses `not_asleep` on running/dead-not-asleep targets
    /// so it can never surprise-restart a live terminal, and `sleeping`
    /// during the sub-second drain transient.
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    Wake {
        id: Uuid,
    },
    /// Folder sleep: every presented-Running member, ONE shared drain
    /// window. Without `force`, members with busy evidence are skipped
    /// (logged); refusal only for an unknown folder. Empty result sets are
    /// no-ops (bulk idempotence).
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    SleepFolder {
        folder: Uuid,
        force: bool,
    },
    /// Folder wake: every presented-Asleep member, staggered daemon-side
    /// (boot-restore lanes). Dead members untouched — dead means "died",
    /// not "shelved". Replies Done immediately; poll List for settling.
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    WakeFolder {
        folder: Uuid,
    },
    /// CLAUDE SESSION ATTRIBUTION Layer 2 (proto 11): a CLI's own
    /// SessionStart/SessionEnd hook reporting its live session id for the
    /// terminal it runs inside (`tc __claude-hook`, injected via
    /// launch_command's `--settings` argv; `id` comes from the inherited
    /// TC_SESSION_ID, the payload from the hook's stdin JSON). The daemon
    /// validates the uuid shape and applies SessionStart to the terminal's
    /// pinned claude id / claude inner_cli (`Core::apply_claude_session`);
    /// SessionEnd is observational (clear/resume ends are the transient
    /// half of a switch; other ends are owned by the exit/block lifecycle).
    /// Reply: Done (or Err not_found / bad_session).
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    ReportCliSession {
        id: Uuid,
        /// Adapter key ("claude" | "codex" today; future CLIs ride the same
        /// verb — codex's Windows-native lane posts adapter:"codex").
        adapter: String,
        /// "SessionStart" | "SessionEnd".
        event: String,
        /// SessionStart source / SessionEnd reason ("startup"|"clear"|
        /// "resume"|…) — forensics + transient-end classification.
        source: String,
        /// The session id as reported; uuid shape validated daemon-side.
        session_id: String,
    },
    /// F1 (ssh-reestablish): `tc retry <term>` — user-initiated ssh
    /// reconnect supervision on a dead ssh terminal (Core::manual_reconnect,
    /// the exact C2D::RetryReconnect path): 2s/10s/30s backoff, then the 30s
    /// ceiling FOREVER — unlimited attempts until the host answers, the user
    /// cancels, or an attempt sits 30s at an interactive auth wall.
    /// Refusals: not_found | not_ssh | running | asleep | supervised.
    ///
    /// APPENDED at the enum's end: bincode encodes variants positionally.
    Retry {
        id: Uuid,
        force_self: bool,
    },
}

/// The scope bits a request needs. Token* need FULL (scoped tokens can never
/// mint tokens — no privilege ladder); legacy C2D verbs are FULL-gated
/// separately at the top of handle_message.
pub fn required_scope(req: &CtlRequest) -> u32 {
    use CtlRequest::*;
    match req {
        List | ReadScreen { .. } | ReadTail { .. } | ReadBlocks { .. }
        | ReadBlockText { .. } | Wait { .. } | Subscribe { .. } | Unsubscribe { .. } => SCOPE_READ,
        Run { .. } | SendRaw { .. } | SendChord { .. } => SCOPE_INPUT,
        // Sleep kills processes and Wake spawns them — Kill/Restart's exact
        // class (S6). ReportCliSession mutates restore identity (the claude
        // pin / inner_cli), the same metadata class MANAGE already guards.
        CreateTerminal { .. } | CreateFolder { .. } | Kill { .. } | Restart { .. }
        | Delete { .. } | Sleep { .. } | Wake { .. } | SleepFolder { .. }
        | WakeFolder { .. } | ReportCliSession { .. } | Retry { .. } => SCOPE_MANAGE,
        TokenCreate { .. } | TokenRevoke { .. } | TokenList => SCOPE_FULL,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RunWait {
    pub timeout_ms: u64,
    pub tail_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WaitCond {
    /// First block with start_off >= after_off that CLOSES (end_off set).
    BlockClose { after_off: u64 },
    /// The shell renders a prompt with no open block (hooked shells only).
    /// Resolves immediately if already true at registration.
    Prompt,
    /// Session process exits.
    Exit,
    /// Stripped output matches. `from_off`: also scan journal bytes from this
    /// absolute offset at registration (closes the register-after-output
    /// race for clients composing run→wait themselves); None = live-only.
    OutputMatch {
        pattern: String,
        regex: bool,
        from_off: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CtlChord {
    Enter,
    Esc,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    CtrlC,
    CtrlD,
    CtrlZ,
    CtrlL,
}

pub const EV_BLOCKS: u32 = 1; // BlockOpened / BlockClosed
pub const EV_EXIT: u32 = 2; // Exited
pub const EV_STATE: u32 = 4; // StateChanged (coarse: re-List to see what)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CtlEvent {
    BlockOpened { id: Uuid, rec: BlockRec },
    BlockClosed { id: Uuid, rec: BlockRec },
    Exited { id: Uuid, code: Option<u32> },
    /// Folders/terminals/status changed (fired from broadcast_snapshot).
    StateChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CtlBody {
    /// Structured refusal/failure. `code` is the machine key (§9.4 table).
    Err {
        code: String,
        msg: String,
    },
    Listing {
        folders: Vec<Folder>,
        terminals: Vec<CtlTerm>,
    },
    Created {
        id: Uuid,
    },
    /// Ack for Kill/Restart/Delete/CreateFolder/Unsubscribe/TokenRevoke/SendRaw/SendChord.
    Done,
    Screen {
        lines: Vec<String>,
        cursor_row: u16,
        cursor_col: u16,
        alt_screen: bool,
    },
    Tail {
        lines: Vec<String>,
        truncated: bool,
    },
    Blocks {
        recs: Vec<BlockRec>,
    },
    BlockText {
        text: String,
        truncated: bool,
    },
    /// Run without wait: the submission was written; at_off = absolute journal
    /// offset captured just before the write (the spawned block's start_off
    /// will be >= at_off — feed it to Wait{BlockClose{after_off}}).
    RunStarted {
        at_off: u64,
    },
    /// Run with wait: the block closed (or the session died closing it dangling).
    RunDone {
        exit: Option<i64>,
        duration_ms: u64,
        output: String,
        truncated: bool,
        start_off: u64,
    },
    /// Wait resolved. `hit`: which condition fired, with its payload.
    Waited {
        hit: WaitHit,
    },
    Subscribed,
    Event {
        ev: CtlEvent,
    },
    Token {
        name: String,
        token: String,
        scope: u32,
    },
    Tokens {
        list: Vec<CtlTokenInfo>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WaitHit {
    BlockClosed { rec: BlockRec },
    Prompt,
    Exited { code: Option<u32> },
    Output { line: String, at_off: u64 },
}

/// One terminal in a Listing. A DEDICATED shape (not TerminalMeta): the
/// controller JSON contract must not silently change when SharedState grows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtlTerm {
    pub id: Uuid,
    pub name: String,
    pub folder: Option<Uuid>,
    pub kind: String,                 // "shell" | "claude" | "custom"
    pub claude_session: Option<Uuid>, // TermKind::Claude pinned id
    pub inner_cli: Option<InnerCli>,  // hand-run CLI tracked in the shell
    pub program: String,
    pub cwd: String, // live_cwd if known, else meta.cwd
    pub status: String, // "running" | "sleeping" | "asleep" | "dead" (open string enum — S18)
    pub activity: String, // "working" | "idle" | "asleep" | "dead"
    pub idle_ms: Option<u64>, // ms since last PTY output (running only)
    pub cols: u16,
    pub rows: u16,
    pub hooked: bool, // block store epoch > 0
    pub open_block: Option<CtlOpenBlock>,
    pub last_block: Option<CtlLastBlock>,
    /// F1 nested-shell breadcrumb (attribution surface for automation; the
    /// GUI reads the Snapshot instead). APPENDED last — bincode field order
    /// is wire order; pulse.exe/pulse-ctl.exe ship together (the skew
    /// window is the install copy-race that already exists).
    #[serde(default)]
    pub nested_chain: Option<crate::state::NestedChain>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtlOpenBlock {
    pub cmd: String,
    pub started_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtlLastBlock {
    pub cmd: String,
    pub exit: Option<i64>,
    pub ended_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtlTokenInfo {
    pub name: String,
    pub token: String,
    pub scope: u32,
    pub created_ms: u64,
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> anyhow::Result<()> {
    let data = bincode::serialize(msg)?;
    let len = data.len() as u32;
    anyhow::ensure!(len <= MAX_FRAME, "frame too large: {len}");
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&data)?;
    w.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    anyhow::ensure!(len <= MAX_FRAME, "frame too large: {len}");
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(bincode::deserialize(&buf)?)
}

/// Contents of daemon.json — how clients find and authenticate to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// Protocol generation, for version-skew warnings (a GUI newer than the
    /// daemon it found). 0 = a daemon.json written before the field existed.
    #[serde(default)]
    pub proto: u32,
}

#[cfg(test)]
mod ctl_tests {
    use super::*;
    use uuid::Uuid;

    /// §15 required_scope_table: every CtlRequest variant maps to its scope;
    /// Token* need FULL. Exhaustive by construction — adding a variant without
    /// extending this table fails to compile the constructor list below only
    /// if someone updates it, so the match in required_scope (which IS
    /// exhaustive) stays the authority; this pins the assignments.
    #[test]
    fn required_scope_table() {
        let id = Uuid::nil();
        let spec = NewTerminal {
            name: String::new(),
            folder: None,
            kind: crate::state::TermKind::Shell,
            program: String::new(),
            args: vec![],
            cwd: std::path::PathBuf::new(),
            already_launched: false,
            shell_cfg: None,
        };
        let read = [
            CtlRequest::List,
            CtlRequest::ReadScreen { id },
            CtlRequest::ReadTail { id, lines: 10 },
            CtlRequest::ReadBlocks { id, last: 5 },
            CtlRequest::ReadBlockText { id, start_off: 0 },
            CtlRequest::Wait {
                id,
                cond: WaitCond::Prompt,
                timeout_ms: 1,
            },
            CtlRequest::Subscribe {
                ids: None,
                kinds: EV_BLOCKS,
            },
            CtlRequest::Unsubscribe { req_id: 1 },
        ];
        for r in &read {
            assert_eq!(required_scope(r), SCOPE_READ, "{r:?}");
        }
        let input = [
            CtlRequest::Run {
                id,
                cmd: "x".into(),
                force: false,
                force_self: false,
                wait: None,
            },
            CtlRequest::SendRaw {
                id,
                bytes: vec![],
                force_self: false,
            },
            CtlRequest::SendChord {
                id,
                chord: CtlChord::CtrlC,
                force_self: false,
            },
        ];
        for r in &input {
            assert_eq!(required_scope(r), SCOPE_INPUT, "{r:?}");
        }
        let manage = [
            CtlRequest::CreateTerminal { spec },
            CtlRequest::CreateFolder { name: "f".into() },
            CtlRequest::Kill {
                id,
                force_self: false,
            },
            CtlRequest::Restart {
                id,
                force_self: false,
            },
            CtlRequest::Delete {
                id,
                force_self: false,
            },
            // SLEEP S6: process-lifecycle class, MANAGE like Kill/Restart.
            CtlRequest::Sleep {
                id,
                force: false,
                force_self: false,
            },
            CtlRequest::Wake { id },
            CtlRequest::SleepFolder {
                folder: id,
                force: false,
            },
            CtlRequest::WakeFolder { folder: id },
            // Attribution Layer 2: identity mutation = MANAGE class.
            CtlRequest::ReportCliSession {
                id,
                adapter: "claude".into(),
                event: "SessionStart".into(),
                source: "clear".into(),
                session_id: Uuid::nil().to_string(),
            },
            // F1: retry spawns processes — Kill/Restart's exact class.
            CtlRequest::Retry {
                id,
                force_self: false,
            },
        ];
        for r in &manage {
            assert_eq!(required_scope(r), SCOPE_MANAGE, "{r:?}");
        }
        let full = [
            CtlRequest::TokenCreate {
                name: "t".into(),
                scope: SCOPE_READ,
            },
            CtlRequest::TokenRevoke { name: "t".into() },
            CtlRequest::TokenList,
        ];
        for r in &full {
            assert_eq!(required_scope(r), SCOPE_FULL, "{r:?}");
        }
        // The CLI presets compose as documented: input implies read.
        assert_eq!(SCOPE_READ | SCOPE_INPUT, 3);
        assert_eq!(SCOPE_READ | SCOPE_INPUT | SCOPE_MANAGE, 7);
        // FULL passes every check a preset passes.
        for bits in [SCOPE_READ, SCOPE_INPUT, SCOPE_MANAGE] {
            assert_eq!(SCOPE_FULL & bits, bits);
        }
    }
}
