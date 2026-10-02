//! Journal Blocks: shell-announced command boundaries.
//!
//! The injected PowerShell bootstrap (see `bootstrap.rs`) emits one private
//! OSC per lifecycle point:
//!
//!   `ESC ] 7717 ; <token> ; <verb> ; <hex(utf8-json)>  BEL|ST`
//!
//! where `token` is 16 lowercase hex chars minted daemon-side per spawn,
//! `verb` is init|exec|pre, and the payload is hex-encoded compact JSON (hex
//! so a cwd or command containing BEL/ESC/';' can never terminate or split
//! the OSC). `BlockScanner` finds these in the raw output stream — it READS
//! only, never injects, so mirror purity is untouched — and `BlockStore`
//! keeps the resulting records, keyed to absolute journal offsets and
//! persisted in a per-terminal sidecar (`journals/<id>.blocks.json`).
//!
//! Trust model (honest): the token lives in a user-private bootstrap file and
//! is compared on every event, which defeats accidental/echoed forgery (a
//! `cat` of a log containing old hooks) and cross-session confusion — not a
//! determined same-user attacker, who could read the file. Same stance as
//! Warp's shell hooks. For SSH terminals the REMOTE end legitimately holds
//! the token too (it rides the base64 rc into the remote bash env, with a
//! brief mktemp residency on remote /tmp before self-delete) — so a hostile
//! remote host, or a co-tenant who wins the /tmp window, can forge blocks /
//! exit codes / cwd LABELS in the local GUI. Display/history integrity only:
//! never code execution, and never a real cwd redirect (commands still run
//! in the shell's actual cwd; the composer's cwd is a display string).

use std::path::PathBuf;
use uuid::Uuid;

use crate::state::{journals_dir, BlockRec};

/// A stuck/hostile OSC body can't grow the carry buffer unbounded.
const BODY_CAP: usize = 16 * 1024;
/// Records kept per terminal; oldest dropped first.
const MAX_RECS: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub enum HookVerb {
    /// `shell`/`home`/`user` are the P6a optional init fields (empty when the
    /// emitter predates them — the ps1 today): serde-lenient, no wire event.
    /// `home` powers the \\wsl$ claude correlation when §7.3 lands.
    Init {
        pid: u32,
        shell: String,
        home: String,
        user: String,
    },
    Exec { cmd: String },
    /// `native`: the raw `$LASTEXITCODE` (PowerShell only, else None) — see
    /// `bootstrap::PWSH_*` and `Core::reinstate_nested_chain`.
    Pre { exit: Option<i64>, n: u32, cwd: String, native: Option<i64> },
    /// OSC 133;B — end of the rendered prompt string (emitted by the
    /// bootstrap between the prompt text and PSReadLine taking over). Carries
    /// no token; GUI-side prompt-end capture only (P3 composer). The daemon
    /// ignores it BEFORE the token check — it mutates nothing, notifies
    /// nothing. APPENDED as the last variant.
    PromptEnd,
    /// `ESC ] 7717 ; tcbeacon ; [<adapter> ;] <event> ; <source> ; <session-id> BEL|ST`
    /// — a CLI-session beacon a hook script prints to /dev/tty from inside an
    /// ssh/WSL session (attribution Layer 3). The claude script
    /// (`~/.tc/claude-hook.sh`) omits the adapter (legacy 3-field form ⇒
    /// adapter defaults to "claude"); the codex script
    /// (`~/.tc/codex-hook.sh`) emits `codex` in the adapter slot. Carries NO
    /// rotating token (the remote script is persistent; tokens rotate per
    /// spawn) — advisory-trust: `Core::on_beacon` gates on hooks_live + an
    /// observed same-adapter exec block + uuid shape BEFORE believing it.
    /// APPENDED last.
    Beacon {
        adapter: String,
        event: String,
        source: String,
        sid: String,
        /// F1 beacon v2: the CLI's own cwd, hex-encoded by the hook script
        /// (field 4) and decoded here — accepted only when non-empty,
        /// POSIX-absolute, and ≤1024 bytes; legacy 3-field scripts (and any
        /// junk hex) yield None (preface variant B). APPENDED last.
        cwd: Option<String>,
    },
    /// OSC 133;A — start of the prompt render (perf-wave-3 D*). Unlike the
    /// `pre`/9;9 OSCs — immediate ConPTY passthroughs that can beat the
    /// command's last output rows through the pipe (conhost renders text on
    /// an async frame) — 133;A is emitted inside the rendered prompt string
    /// itself, so it rides the text frame AFTER the command output: a
    /// deterministic boundary between output tail and prompt text. The
    /// daemon anchors a pre-armed block close's `end_off` to it
    /// (`BlockStore::on_prompt_start`), which is what let the bootstrap drop
    /// its 15ms pre-hook drain sleep. Tokenless like PromptEnd — it can only
    /// ever RESOLVE a close a token-checked pre armed, never open or forge
    /// one. APPENDED last.
    PromptStart,
    /// remote-completion: one directory listing, as the hooked shell itself
    /// sees it (`bootstrap::COMP_FN`). `dir` is the ABSOLUTE path the shell
    /// resolved and listed (`~`/relative requests are expanded on its side,
    /// so this is the authoritative cache key); `list` is `ls -A -p -L`
    /// output — one name per line, a trailing `/` marking a directory;
    /// `trunc` means the listing blew `COMP_MAX_BYTES` and was DROPPED, so
    /// the only honest answer is "no candidates" (a common prefix over a
    /// subset would over-complete).
    ///
    /// Token-checked like `init`/`exec`/`pre` — a `comp` carrying an
    /// unrecognized token is a spoof and never reaches the cache. APPENDED
    /// last.
    Comp {
        dir: String,
        trunc: bool,
        list: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockEvent {
    pub token: String,
    pub verb: HookVerb,
    /// Index of the byte AFTER the OSC terminator, relative to the chunk fed
    /// to `feed`. The caller adds the chunk's absolute stream offset.
    pub offset_in_chunk: usize,
}

/// Chunk-boundary-safe scanner for the 7717 hook OSC, modeled on
/// `session::OscScanner` (same PendingEsc/InOsc structure, ESC-at-boundary
/// carry, ST-vs-BEL termination). Everything else — including OSC 133;A/B —
/// passes through unparsed.
#[derive(Default)]
pub struct BlockScanner {
    body: Vec<u8>,
    in_osc: bool,
    pending_esc: bool,
}

impl BlockScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<BlockEvent> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if !self.in_osc {
                if self.pending_esc {
                    self.pending_esc = false;
                    if b == b']' {
                        self.in_osc = true;
                        self.body.clear();
                        i += 1;
                        continue;
                    }
                } else if b != 0x1b {
                    // Ground state: bytes before the next ESC can't change
                    // anything — SIMD-skip them (per-chunk on every session's
                    // output; plain text must not pay a per-byte DFA step).
                    match memchr::memchr(0x1b, &data[i..]) {
                        Some(off) => {
                            i += off;
                            continue;
                        }
                        None => break,
                    }
                }
                if b == 0x1b {
                    // ESC ] opens an OSC; handle ESC at a chunk boundary too.
                    if i + 1 < data.len() {
                        if data[i + 1] == b']' {
                            self.in_osc = true;
                            self.body.clear();
                            i += 2;
                            continue;
                        }
                    } else {
                        self.pending_esc = true;
                    }
                }
                i += 1;
            } else {
                // A chunk boundary split the OSC right after an ESC: resolve
                // that pending ESC against this byte FIRST, exactly as
                // unsplit input would — '\' is ST (terminate + emit);
                // anything else ABORTS the sequence and the byte is
                // reprocessed in ground state (L-8: a BEL here used to hit
                // the terminator arm below and emit where unsplit input
                // aborts; an ESC here must stay reprocessable so it can open
                // a following sequence, as it does unsplit).
                if self.pending_esc {
                    self.pending_esc = false;
                    if b == b'\\' {
                        i += 1;
                        self.finish(i, &mut out);
                    } else {
                        self.in_osc = false;
                        self.body.clear();
                        // `b` NOT consumed: ground state reprocesses it.
                    }
                    continue;
                }
                match b {
                    0x07 => {
                        i += 1;
                        self.finish(i, &mut out);
                    }
                    0x1b => {
                        // ST is ESC '\'; anything else aborts the sequence.
                        if i + 1 < data.len() && data[i + 1] == b'\\' {
                            i += 2;
                            self.finish(i, &mut out);
                        } else if i + 1 == data.len() {
                            // ESC at the chunk edge: could be ST split across
                            // reads. Stay in-OSC and let the next chunk decide.
                            self.pending_esc = true;
                            i += 1;
                        } else {
                            self.in_osc = false;
                            self.body.clear();
                            i += 1;
                        }
                    }
                    _ => {
                        if self.body.len() < BODY_CAP {
                            self.body.push(b);
                        } else {
                            self.in_osc = false;
                            self.body.clear();
                        }
                        i += 1;
                    }
                }
            }
        }
        out
    }

    fn finish(&mut self, offset_after: usize, out: &mut Vec<BlockEvent>) {
        if let Some(ev) = parse_hook(&self.body, offset_after) {
            out.push(ev);
        }
        self.in_osc = false;
        self.body.clear();
        self.pending_esc = false;
    }
}

/// Lenient payload schemas: missing fields default rather than fail, so a
/// malformed-but-token-bearing event still reaches the Core's token check
/// (which is where spoofs are logged and dropped).
#[derive(serde::Deserialize)]
struct InitPayload {
    #[serde(default)]
    #[allow(dead_code)]
    v: u32,
    #[serde(default)]
    pid: u32,
    // P6a optional fields (§3.1.4): the bash bootstrap reports them, the ps1
    // doesn't (yet) — defaults keep both parsing.
    #[serde(default)]
    shell: String,
    #[serde(default)]
    home: String,
    #[serde(default)]
    user: String,
}

#[derive(serde::Deserialize)]
struct ExecPayload {
    #[serde(default)]
    c: String,
}

#[derive(serde::Deserialize)]
struct PrePayload {
    #[serde(default)]
    e: Option<i64>,
    /// PowerShell's RAW `$LASTEXITCODE` (`e` is the folded status the block
    /// chrome uses). Absent from every other shell's hook.
    #[serde(default)]
    x: Option<i64>,
    #[serde(default)]
    n: u32,
    #[serde(default)]
    d: String,
}

/// remote-completion (`bootstrap::COMP_FN`): `d` = the absolute directory the
/// shell listed, `t` = over-cap flag, `l` = `ls -A -p -L` output.
#[derive(serde::Deserialize)]
struct CompPayload {
    #[serde(default)]
    d: String,
    #[serde(default)]
    t: u8,
    #[serde(default)]
    l: String,
}

/// Parse an OSC body (bytes between `ESC ]` and the terminator). Only bodies
/// starting `7717;` are ours — plus the tokenless `133;B` prompt-end and
/// `133;A` prompt-start markers (P3 / D*); anything malformed is dropped
/// with a debug log.
fn parse_hook(body: &[u8], offset_after: usize) -> Option<BlockEvent> {
    // Prompt-end marker: reuse the already-running DFA rather than a second
    // scanner (which would drift). Every other OSC stays ignored.
    if body == b"133;B" {
        return Some(BlockEvent {
            token: String::new(),
            verb: HookVerb::PromptEnd,
            offset_in_chunk: offset_after,
        });
    }
    // Prompt-start marker (D*): the deferred block close's end_off anchor —
    // same tokenless treatment as 133;B.
    if body == b"133;A" {
        return Some(BlockEvent {
            token: String::new(),
            verb: HookVerb::PromptStart,
            offset_in_chunk: offset_after,
        });
    }
    let s = std::str::from_utf8(body).ok()?;
    let rest = s.strip_prefix("7717;")?;
    // Attribution Layer 3: the tokenless remote beacon rides the same 7717
    // envelope with the literal `tcbeacon` in the token slot (a real token
    // is 16 hex chars — no collision). Payload is plain `;`-separated (the
    // emitter is a 10-line POSIX sh script; event/source are claude enum
    // words and the sid is a uuid — none can contain `;`). Extra fields are
    // ignored so the script can grow.
    if let Some(b) = rest.strip_prefix("tcbeacon;") {
        // The first field is an adapter keyword ("claude"/"codex") in the
        // adapter-carrying form, else it is the event (legacy claude 3-field
        // form ⇒ adapter defaults to "claude"). Events are enum words
        // (SessionStart/SessionEnd/…), never "claude"/"codex", so the
        // disambiguation is unambiguous. Extra trailing fields are ignored
        // (the emitter can grow).
        let mut fields: Vec<&str> = b.split(';').collect();
        let adapter = match fields.first() {
            Some(&a) if a == "claude" || a == "codex" => {
                fields.remove(0);
                a.to_string()
            }
            _ => "claude".to_string(),
        };
        let event = fields.first().copied().unwrap_or("").to_string();
        let source = fields.get(1).copied().unwrap_or("").to_string();
        let sid = fields.get(2).copied().unwrap_or("").trim().to_string();
        if sid.is_empty() {
            log::debug!("tcbeacon without a session id dropped");
            return None;
        }
        // Beacon v2 (F1): optional 4th field = hex(claude's cwd). Hex so a
        // cwd containing `;`/BEL/ESC can never split or terminate the OSC.
        // Advisory display data only — sanity-gated (utf8, non-empty,
        // POSIX-absolute, ≤1024 bytes) and None-on-anything-else, so a
        // legacy 3-field script and a junk field behave identically.
        let cwd = fields
            .get(3)
            .and_then(|h| hex_decode(h.trim()))
            .and_then(|b| String::from_utf8(b).ok())
            .filter(|c| !c.is_empty() && c.starts_with('/') && c.len() <= 1024);
        return Some(BlockEvent {
            token: String::new(),
            verb: HookVerb::Beacon {
                adapter,
                event,
                source,
                sid,
                cwd,
            },
            offset_in_chunk: offset_after,
        });
    }
    let mut parts = rest.splitn(3, ';');
    let token = parts.next()?.to_string();
    let verb = parts.next()?;
    let hex = parts.next().unwrap_or("");
    let json = match hex_decode(hex) {
        Some(j) => j,
        None => {
            log::debug!("block hook with undecodable hex payload dropped");
            return None;
        }
    };
    let verb = match verb {
        "init" => serde_json::from_slice::<InitPayload>(&json).ok().map(|p| HookVerb::Init {
            pid: p.pid,
            shell: p.shell,
            home: p.home,
            user: p.user,
        }),
        "exec" => serde_json::from_slice::<ExecPayload>(&json)
            .ok()
            .map(|p| HookVerb::Exec { cmd: p.c }),
        "pre" => serde_json::from_slice::<PrePayload>(&json).ok().map(|p| HookVerb::Pre {
            exit: p.e,
            n: p.n,
            cwd: p.d,
            native: p.x,
        }),
        // remote-completion: a listing is accepted only when the shell named
        // the ABSOLUTE directory it listed — that string is the cache key, and
        // a relative or empty one could only ever be filed against the wrong
        // directory. A truncated listing arrives with `l` already empty (the
        // shell drops it); dropping it again here costs nothing and makes the
        // invariant local.
        "comp" => serde_json::from_slice::<CompPayload>(&json)
            .ok()
            .filter(|p| p.d.starts_with('/'))
            .map(|p| HookVerb::Comp {
                dir: p.d,
                trunc: p.t != 0,
                list: if p.t != 0 { String::new() } else { p.l },
            }),
        _ => None,
    };
    match verb {
        Some(verb) => Some(BlockEvent {
            token,
            verb,
            offset_in_chunk: offset_after,
        }),
        None => {
            log::debug!("malformed block hook dropped");
            None
        }
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

// ───────────────────────────── store + sidecar ─────────────────────────────

/// Per-terminal block state, living in `Core.blocks` (a LEAF lock: nothing
/// else is ever locked while it is held). Clone is used to snapshot the
/// store out of the lock before sidecar file IO.
#[derive(Clone)]
pub struct BlockStore {
    /// Expected hook token for the current spawn; events carrying anything
    /// else are logged and dropped.
    pub token: String,
    /// Spawn generation; bumped by every launch().
    pub epoch: u32,
    /// Mirror of the journal's compaction base, persisted here (the sidecar
    /// is what survives restarts — the journal file doesn't know its base).
    pub base: u64,
    pub recs: Vec<BlockRec>,
    /// Index into `recs` of the currently open block, if any.
    pub open: Option<usize>,
    /// Freshest cwd reported by a prompt hook; stamps the next opened block.
    pub last_cwd: Option<PathBuf>,
    /// Any correct-token hook event arrived THIS spawn — the bootstrap is
    /// alive, so open-block tracking can be trusted as a busy signal (P5's
    /// run gate). `epoch > 0` only proves the INTENT to hook (bootstrap
    /// written); this proves the shell actually ran it. Runtime truth only:
    /// never persisted to the sidecar, reset on every launch() rotation.
    pub hooks_live: bool,
    /// D2: the currently open block was opened SYNTHETICALLY (SubmitCommand
    /// lane — no exec hook exists where it ran: cmd.exe, or a heuristic
    /// nested-shell episode). The closing `pre` then closes it with
    /// `exit = None` REGARDLESS of payload: in a nested episode the login
    /// shell's returning pre carries the OUTER command's exit (`sudo su`'s
    /// 0), which must never be misattributed to the last inner command. No
    /// behavior change for cmd (its static pre is already `e:null`). Runtime
    /// truth only — never persisted; cleared by hook-opened blocks.
    pub synthetic_open: bool,
    /// D* (perf-wave-3): a `pre` arrived for an open block — the close is
    /// ARMED but end_off waits for the next 133;A (see `PendingClose`).
    /// Runtime truth only — never persisted (the sidecar serializes
    /// epoch/base/recs); a launch rotation's `close_dangling` flushes it.
    pending_close: Option<PendingClose>,
    /// nested-shell-hooks: hook tokens for the NESTED shells Pulse injected
    /// into during this spawn, innermost LAST (index 0 = depth 1). Each is
    /// minted per injection and registered ALONGSIDE `token` — the outer
    /// shell's token never rotates for a nested episode, so a hook from
    /// either world is accepted, and `classify_token` still says WHICH world
    /// spoke. That distinction is load-bearing: the F1 episode-end rule
    /// ("the outer prompt returning means the nested world is gone") is only
    /// true for an OUTER-token `pre`, and the F2 re-establish trigger must
    /// never fire on a nested prompt.
    ///
    /// Runtime truth only — never persisted, dropped by `rotate` (a new
    /// spawn has no nested world yet) and popped back by `pop_nested_below`
    /// when a shallower shell speaks again.
    nested_tokens: Vec<String>,
}

/// nested-shell-hooks: cap on registered nested tokens for one spawn — the
/// same "a re-establish line is a hint, not a transcript" bound the
/// breadcrumb chain uses. Beyond it, injection stops (and says so) rather
/// than growing an unbounded accept-list.
pub const NESTED_TOKEN_MAX: usize = 8;

/// nested-shell-hooks: which shell a token-checked hook came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookScope {
    /// The login shell Pulse itself spawned (the rcfile/ps1/PROMPT lane).
    Outer,
    /// A nested shell Pulse witnessed being opened and injected hooks into.
    /// `depth` is 1 for the first `sudo su`, 2 for a `su - deploy` inside
    /// it, and so on.
    Nested { depth: usize },
}

impl HookScope {
    pub fn is_outer(self) -> bool {
        matches!(self, HookScope::Outer)
    }
    pub fn depth(self) -> usize {
        match self {
            HookScope::Outer => 0,
            HookScope::Nested { depth } => depth,
        }
    }
}

/// D* (perf-wave-3): the armed-but-deferred block close. The pre/9;9 OSCs
/// are immediate ConPTY passthroughs that can arrive BEFORE the command's
/// last output rows (conhost renders text on an async frame), so closing at
/// the pre's byte position could clip the block's tail — the race the
/// bootstrap's 15ms prompt() sleep used to absorb shell-side, at ~16ms of
/// visible prompt-return latency per command. The close now waits for the
/// next 133;A (PromptStart), which rides the text frame AFTER the output;
/// if the marker never comes (user clobbered `prompt`, hooks stripped), the
/// ingest loop's quiescence timeout flushes the close at the pre position —
/// byte-identical to the sleep-era end_off.
#[derive(Clone)]
struct PendingClose {
    /// (epoch, start_off) of the armed record — index-free, so journal
    /// compaction (`evict`, which drains recs) can never dangle it; an open
    /// record (end_off None) is never evicted, only flagged truncated.
    key: (u32, u64),
    /// Exit already D2-resolved at arm time (synthetic open ⇒ None).
    exit: Option<i64>,
    n: u32,
    /// Byte offset just past the pre OSC — the fallback end_off.
    pre_off: u64,
    /// When the pre armed this close; the fallback's honest ended_ms.
    armed_ms: u64,
}

/// Sidecar file body: everything a restart needs to rehydrate.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Sidecar {
    epoch: u32,
    base: u64,
    recs: Vec<BlockRec>,
}

fn sidecar_path(id: Uuid) -> PathBuf {
    journals_dir().join(format!("{id}.blocks.json"))
}

impl BlockStore {
    /// Rehydrate from the sidecar, or start empty. Prior-epoch records stay
    /// valid because offsets are absolute and a restore only APPENDS. The
    /// token starts unmatchable ("") until launch() rotates it.
    pub fn load(id: Uuid) -> Self {
        // C2 honesty: a corrupt sidecar must not SILENTLY become an empty
        // store (the terminal's whole command history vanishing without a
        // trace). Same doctrine as state.json: back the bytes up, log, start
        // fresh.
        let side: Sidecar = match std::fs::read(sidecar_path(id)) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(side) => side,
                Err(e) => {
                    let path = sidecar_path(id);
                    let backup = path.with_extension("json.corrupt");
                    log::error!(
                        "block sidecar {path:?} is corrupt ({e}); backing up to {backup:?} and starting empty"
                    );
                    let _ = std::fs::rename(&path, &backup);
                    Sidecar::default()
                }
            },
            Err(_) => Sidecar::default(), // fresh terminal: no sidecar yet
        };
        Self {
            token: String::new(),
            epoch: side.epoch,
            base: side.base,
            recs: side.recs,
            open: None,
            last_cwd: None,
            hooks_live: false,
            synthetic_open: false,
            pending_close: None,
            nested_tokens: Vec::new(),
        }
    }

    /// r2-F6 crash-consistency guard, run at the journal's single
    /// construction site with the file's real head (`base + file_len`).
    /// The compaction rename and the sidecar save are two non-atomic
    /// commits: a power cut between them reloads a `base` one compaction
    /// behind the file, and every persisted offset then maps to the wrong
    /// bytes (and new appends mint colliding offsets). Records beyond the
    /// head are the tell — drop the ledger (returns true when it did).
    /// Journal bytes are never touched; only block chrome is lost.
    pub fn reconcile_with_journal_head(&mut self, head: u64) -> bool {
        let beyond = self
            .recs
            .iter()
            .any(|r| r.start_off > head || r.end_off.is_some_and(|e| e > head));
        if beyond {
            self.recs.clear();
            self.open = None;
        }
        beyond
    }

    /// Token gate for an incoming hook event: `None` ⇒ the caller logs the
    /// spoof and drops the event; `Some(scope)` ⇒ marks the bootstrap live
    /// for this spawn. The single check site, so `hooks_live` can never
    /// disagree with what actually got accepted.
    ///
    /// nested-shell-hooks: the gate also NAMES the shell. `None`
    /// ⇒ the caller logs the spoof and drops the event (byte-identical to
    /// the pre-nested `accept_token(false)` path); `Some(scope)` marks the
    /// bootstrap live and tells the caller whether the outer login shell or
    /// an injected nested shell spoke.
    ///
    /// Deliberately checks the OUTER token first: it is the hot path (every
    /// prompt of every terminal), and a nested token can never collide with
    /// it (both are freshly minted 64-bit randoms).
    pub fn classify_token(&mut self, token: &str) -> Option<HookScope> {
        let scope = if token == self.token {
            HookScope::Outer
        } else {
            let depth = self.nested_tokens.iter().position(|t| t == token)? + 1;
            HookScope::Nested { depth }
        };
        self.hooks_live = true;
        Some(scope)
    }

    /// Register a freshly minted token for a nested shell about to be
    /// injected, returning its depth (1-based). Capped at `NESTED_TOKEN_MAX`:
    /// a deeper chain stops registering rather than growing an unbounded
    /// accept-list (`None` ⇒ the caller skips the injection and says so).
    pub fn push_nested_token(&mut self, token: String) -> Option<usize> {
        if self.nested_tokens.len() >= NESTED_TOKEN_MAX {
            return None;
        }
        self.nested_tokens.push(token);
        Some(self.nested_tokens.len())
    }

    /// A shell at `depth` spoke (0 = the outer login shell), so every world
    /// INSIDE it is gone: drop those tokens, returning how many. This is
    /// what stops a stale nested token from accepting hooks after its shell
    /// exited (`exit` out of `su - deploy` back into `sudo su`), and what
    /// makes the outer prompt returning a full reset.
    pub fn pop_nested_below(&mut self, depth: usize) -> usize {
        let before = self.nested_tokens.len();
        self.nested_tokens.truncate(depth);
        before - self.nested_tokens.len()
    }

    /// How many nested shells are currently hooked (0 = none).
    pub fn nested_depth(&self) -> usize {
        self.nested_tokens.len()
    }

    /// Spawn rotation (launch()): new epoch, fresh token, and the liveness
    /// proof resets — the NEW shell hasn't run its bootstrap yet. A D*
    /// pending close deliberately SURVIVES rotation: launch() calls
    /// `close_dangling` right after, which flushes it with its real exit
    /// (the key is (epoch, start_off), so it can never touch new-epoch recs).
    pub fn rotate(&mut self, token: String) {
        self.epoch += 1;
        self.token = token;
        self.hooks_live = false;
        self.synthetic_open = false;
        // nested-shell-hooks: a fresh spawn has no nested world yet — the
        // old nested shells died with the old process tree.
        self.nested_tokens.clear();
    }

    /// Atomic tmp+rename write (same pattern as SharedState::save), so a
    /// power cut can never leave a truncated sidecar.
    pub fn save(&self, id: Uuid) {
        // Serialize from BORROWED recs — this runs per block close on the
        // pty-ingest thread, and cloning up to 500 records just to feed
        // serde was pure waste. Field-for-field the same JSON as `Sidecar`.
        #[derive(serde::Serialize)]
        struct SidecarRef<'a> {
            epoch: u32,
            base: u64,
            recs: &'a [BlockRec],
        }
        let side = SidecarRef {
            epoch: self.epoch,
            base: self.base,
            recs: &self.recs,
        };
        let Ok(data) = serde_json::to_vec(&side) else { return };
        let path = sidecar_path(id);
        let tmp = journals_dir().join(format!("{id}.blocks.json.tmp"));
        let write_tmp = || -> std::io::Result<()> {
            use std::io::Write;
            std::fs::create_dir_all(journals_dir())?;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
            Ok(())
        };
        // C2 honesty: a failing save means block records quietly stop
        // persisting across restarts — log it (the append path owns the
        // user-facing disk-full banner).
        match write_tmp() {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp, &path) {
                    log::error!("block sidecar rename failed for {path:?}: {e}");
                }
            }
            Err(e) => log::error!("block sidecar write failed for {tmp:?}: {e}"),
        }
    }

    pub fn delete_sidecar(id: Uuid) {
        let _ = std::fs::remove_file(sidecar_path(id));
    }

    /// Open a new block. Returns the indices of changed records (a dangling
    /// open block is closed with exit=None at the new block's offset first —
    /// two exec without an intervening pre). Hook-opened by default: a real
    /// exec hook proves the shell can announce boundaries again, so the
    /// synthetic flag clears (D2).
    pub fn open_block(&mut self, cmd: String, start_off: u64, now_ms: u64) -> Vec<usize> {
        self.synthetic_open = false;
        let mut changed = Vec::new();
        // D*: an exec landing while a close is still armed (lost 133;A and
        // the stream never went quiet) flushes it at its pre position first —
        // the pre DID arrive, so its exit/end are real; only the anchor is.
        if let Some(i) = self.flush_pending_close() {
            changed.push(i);
        }
        if let Some(idx) = self.open.take() {
            if let Some(r) = self.recs.get_mut(idx) {
                r.end_off = Some(start_off);
                r.ended_ms = Some(now_ms);
                changed.push(idx);
            }
        }
        self.recs.push(BlockRec {
            epoch: self.epoch,
            n: 0,
            cmd,
            cwd: self.last_cwd.clone(),
            exit: None,
            started_ms: now_ms,
            ended_ms: None,
            start_off,
            end_off: None,
            truncated: false,
        });
        if self.recs.len() > MAX_RECS {
            let drop_n = self.recs.len() - MAX_RECS;
            self.recs.drain(..drop_n);
            // Shift surviving changed-indices; a changed record that was
            // itself dropped has nothing left to report.
            changed.retain(|c| *c >= drop_n);
            for c in &mut changed {
                *c -= drop_n;
            }
        }
        self.open = Some(self.recs.len() - 1);
        changed.push(self.recs.len() - 1);
        changed
    }

    /// D2: open a SYNTHETIC block (the SubmitCommand lane) — `open_block`
    /// plus the flag that makes the closing pre honest about exit codes
    /// (see `synthetic_open`).
    pub fn open_block_synthetic(&mut self, cmd: String, start_off: u64, now_ms: u64) -> Vec<usize> {
        let changed = self.open_block(cmd, start_off, now_ms);
        self.synthetic_open = true;
        changed
    }

    /// A prompt hook: refreshes cwd, and if a block is open, ARMS its close —
    /// end_off is deferred to the next 133;A (`on_prompt_start`), the marker
    /// that rides the text frame after the command output (D*; see
    /// `PendingClose`). A still-armed close from a lost 133;A flushes at its
    /// own pre position first; the returned index is that flushed record, if
    /// any. A synthetic-opened block arms with `exit = None` regardless of
    /// payload (D2 misattribution guard — the pre that closes the LAST inner
    /// command of a nested-shell episode is the login shell's, carrying the
    /// OUTER command's exit).
    pub fn on_pre(
        &mut self,
        exit: Option<i64>,
        n: u32,
        cwd: String,
        pre_off: u64,
        now_ms: u64,
    ) -> Option<usize> {
        let flushed = self.flush_pending_close();
        if !cwd.is_empty() {
            self.last_cwd = Some(PathBuf::from(cwd));
        }
        if let Some(idx) = self.open.take() {
            let synth = std::mem::take(&mut self.synthetic_open);
            if let Some(r) = self.recs.get(idx) {
                self.pending_close = Some(PendingClose {
                    key: (r.epoch, r.start_off),
                    exit: if synth { None } else { exit },
                    n,
                    pre_off,
                    armed_ms: now_ms,
                });
            }
        }
        flushed
    }

    /// The 133;A anchor arrived: close the armed record THERE — just past
    /// the marker, after the full output tail, before the prompt text. Only
    /// the FIRST 133;A while a close is armed is honored; with nothing armed
    /// (a program echoing the sequence mid-output, a nested shell's own
    /// shell-integration prompt) this is a no-op — the spurious-marker guard.
    pub fn on_prompt_start(&mut self, end_off: u64, now_ms: u64) -> Option<usize> {
        let p = self.pending_close.take()?;
        self.close_pending(p, end_off, now_ms)
    }

    /// Flush an armed close at its pre position with its arm-time stamp —
    /// byte- and time-identical to the sleep-era immediate close. Fired by
    /// the ingest loop's quiescence fallback (marker lost), a following pre
    /// or exec (marker lost, stream never quiet), and `close_dangling`.
    pub fn flush_pending_close(&mut self) -> Option<usize> {
        let p = self.pending_close.take()?;
        let (off, at) = (p.pre_off, p.armed_ms);
        self.close_pending(p, off, at)
    }

    /// True while a pre-armed close awaits its 133;A — the ingest loop's
    /// signal to park with a timeout instead of indefinitely.
    pub fn pre_close_armed(&self) -> bool {
        self.pending_close.is_some()
    }

    fn close_pending(&mut self, p: PendingClose, end_off: u64, ended_ms: u64) -> Option<usize> {
        let idx = self
            .recs
            .iter()
            .rposition(|r| (r.epoch, r.start_off) == p.key)?;
        let r = &mut self.recs[idx];
        if r.end_off.is_some() {
            return None; // already closed elsewhere (a dangling close raced)
        }
        r.exit = p.exit;
        r.n = p.n;
        r.end_off = Some(end_off);
        r.ended_ms = Some(ended_ms);
        Some(idx)
    }

    /// Close every still-open record (launch rotation / process exit / a
    /// crash that left sidecar records without an end). exit stays None —
    /// the truth is the session never reported one.
    pub fn close_dangling(&mut self, end_off: u64, now_ms: u64) -> Vec<usize> {
        self.open = None;
        self.synthetic_open = false;
        let mut changed = Vec::new();
        // D*: an armed close knows its real exit and pre position — flush it
        // honestly before the blanket exit=None sweep (its record then reads
        // end_off=Some and the loop below skips it).
        if let Some(i) = self.flush_pending_close() {
            changed.push(i);
        }
        for (i, r) in self.recs.iter_mut().enumerate() {
            if r.end_off.is_none() {
                r.end_off = Some(end_off);
                r.ended_ms = Some(now_ms);
                changed.push(i);
            }
        }
        changed
    }

    /// Compaction eviction: records fully before the new base are gone from
    /// the file — drop them; records straddling the cut keep their coords but
    /// are flagged truncated. An open record (end_off=None) extends to the
    /// stream head and can only straddle, never evict.
    pub fn evict(&mut self, new_base: u64) {
        self.base = new_base;
        let open_key = self.open.and_then(|i| self.recs.get(i)).map(|r| (r.epoch, r.start_off));
        self.recs.retain_mut(|r| {
            let end = r.end_off.unwrap_or(u64::MAX);
            if end <= new_base {
                return false;
            }
            if r.start_off < new_base {
                r.truncated = true;
            }
            true
        });
        self.open = open_key.and_then(|k| {
            self.recs.iter().position(|r| (r.epoch, r.start_off) == k)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PowerShell's pre carries the RAW `$LASTEXITCODE` beside the folded
    /// status; every other shell's pre carries none. The nested-death verdict
    /// reads the raw value — folded, a second identical link loss (-1 again)
    /// read as a deliberate exit 1.
    #[test]
    fn a_pwsh_pre_carries_its_raw_native_exit() {
        let hex = |s: &str| s.bytes().map(|b| format!("{b:02x}")).collect::<String>();
        let body = format!("7717;0123456789abcdef;pre;{}", hex(r#"{"e":1,"n":3,"d":"C:\\","x":-1}"#));
        let ev = parse_hook(body.as_bytes(), 0).expect("a pre parses");
        assert!(matches!(ev.verb, HookVerb::Pre { exit: Some(1), native: Some(-1), .. }));
        let body = format!("7717;0123456789abcdef;pre;{}", hex(r#"{"e":255,"n":3,"d":"/"}"#));
        let ev = parse_hook(body.as_bytes(), 0).expect("a pre parses");
        assert!(matches!(ev.verb, HookVerb::Pre { exit: Some(255), native: None, .. }));
    }

    fn hook(verb: &str, json: &str, token: &str) -> Vec<u8> {
        let hex = crate::strip::hex_lower(json.as_bytes());
        format!("\x1b]7717;{token};{verb};{hex}\x07").into_bytes()
    }

    const TOK: &str = "0123456789abcdef";

    fn stream() -> Vec<u8> {
        let mut s = Vec::new();
        s.extend_from_slice(b"PS C:\\> noise \x1b[31mred\x1b[0m ");
        s.extend(hook("init", r#"{"v":1,"pid":1234}"#, TOK));
        s.extend_from_slice(b"\x1b]9;9;C:\\\x07\x1b]133;A\x07PS C:\\>\x1b]133;B\x07");
        s.extend(hook("exec", r#"{"c":"echo hi"}"#, TOK));
        s.extend_from_slice(b"hi\r\n");
        s.extend(hook("pre", r#"{"e":0,"n":2,"d":"C:\\"}"#, TOK));
        // An ST-terminated one too.
        let mut st = hook("exec", r#"{"c":"dir"}"#, TOK);
        let l = st.len();
        st.splice(l - 1..l, *b"\x1b\\");
        s.extend(st);
        s.extend_from_slice(b"trailing");
        s
    }

    /// Feed the same stream in different chunk sizes; the events (and their
    /// ABSOLUTE offsets) must be identical — the ModeScanner ethos.
    #[test]
    fn chunk_splitting_is_invisible() {
        let data = stream();
        let collect = |chunk: usize| -> Vec<(String, HookVerb, u64)> {
            let mut sc = BlockScanner::new();
            let mut out = Vec::new();
            let mut abs = 0u64;
            for c in data.chunks(chunk) {
                for ev in sc.feed(c) {
                    out.push((ev.token.clone(), ev.verb.clone(), abs + ev.offset_in_chunk as u64));
                }
                abs += c.len() as u64;
            }
            out
        };
        let whole = collect(data.len());
        assert_eq!(
            whole.len(),
            6,
            "init, prompt-start, prompt-end, exec, pre, exec"
        );
        assert_eq!(collect(1), whole);
        assert_eq!(collect(7), whole);
        assert_eq!(collect(64), whole);
    }

    /// remote-completion: the `comp` payload parser — every shape the wire
    /// can carry, including the ones a hostile or broken emitter produces.
    #[test]
    fn comp_payload_parser_accepts_only_answerable_listings() {
        let one = |json: &str| {
            let mut sc = BlockScanner::new();
            sc.feed(&hook("comp", json, TOK)).into_iter().next().map(|e| e.verb)
        };
        // The ordinary answer.
        assert_eq!(
            one(r#"{"d":"/root","t":0,"l":"alpha/\nnotes.txt"}"#),
            Some(HookVerb::Comp {
                dir: "/root".into(),
                trunc: false,
                list: "alpha/\nnotes.txt".into(),
            })
        );
        // An EMPTY directory is an answer, not a failure — the GUI caches it
        // and stops asking.
        assert_eq!(
            one(r#"{"d":"/empty","t":0,"l":""}"#),
            Some(HookVerb::Comp { dir: "/empty".into(), trunc: false, list: String::new() })
        );
        // OVER-CAP: the listing is dropped on BOTH sides. A subset could only
        // ever over-complete, so `l` is forced empty here even if an emitter
        // sent one anyway.
        assert_eq!(
            one(r#"{"d":"/huge","t":1,"l":"aaa\nbbb"}"#),
            Some(HookVerb::Comp { dir: "/huge".into(), trunc: true, list: String::new() })
        );
        // The directory must be ABSOLUTE: it is the cache key both sides
        // agree on, and a relative or missing one could only be filed against
        // the wrong directory.
        assert_eq!(one(r#"{"d":"rel/x","t":0,"l":"a"}"#), None);
        assert_eq!(one(r#"{"d":"","t":0,"l":"a"}"#), None);
        assert_eq!(one(r#"{"t":0,"l":"a"}"#), None);
        // Garbage: not JSON, not an object, truncated mid-string, undecodable
        // hex, odd-length hex. Every one is dropped, none panics.
        assert_eq!(one("not json"), None);
        assert_eq!(one("[1,2,3]"), None);
        assert_eq!(one(r#"{"d":"/root","t":0,"l":"unterminated"#), None);
        let mut sc = BlockScanner::new();
        assert!(sc.feed(b"\x1b]7717;0123456789abcdef;comp;zzzz\x07").is_empty());
        let mut sc = BlockScanner::new();
        assert!(sc.feed(b"\x1b]7717;0123456789abcdef;comp;7b7\x07").is_empty());
        // Unknown extra fields are ignored, so the payload can grow.
        assert_eq!(
            one(r#"{"d":"/root","t":0,"l":"x","future":[1]}"#),
            Some(HookVerb::Comp { dir: "/root".into(), trunc: false, list: "x".into() })
        );
        // A WRONG-token comp is a spoof: the scanner still parses the
        // envelope (it is token-agnostic), and `classify_token` upstream is
        // what rejects it — pinned here so the token slot is never dropped
        // from the comp path.
        let evs = {
            let mut sc = BlockScanner::new();
            sc.feed(&hook("comp", r#"{"d":"/root","t":0,"l":"x"}"#, "deadbeefdeadbeef"))
        };
        assert_eq!(evs[0].token, "deadbeefdeadbeef");
    }

    /// remote-completion: the shell's byte cap must keep the whole OSC body
    /// under the scanner's `BODY_CAP`, or a maximal listing would be dropped
    /// at the carry buffer and the lane would fail exactly on the big
    /// directories it is bounded for.
    #[test]
    fn comp_payload_fits_the_osc_body_cap() {
        let dir = "/".to_string() + &"d".repeat(1024); // a maximal-ish path
        let list = "x".repeat(super::super::bootstrap::COMP_MAX_BYTES);
        let json = format!(r#"{{"d":"{dir}","t":0,"l":"{list}"}}"#);
        // The body is `7717;<token>;comp;<hex(json)>` — hex doubles it.
        let body = 5 + TOK.len() + 1 + "comp".len() + 1 + json.len() * 2;
        assert!(
            body < BODY_CAP,
            "a maximal comp body is {body} bytes, over the {BODY_CAP}-byte cap"
        );
        // ...and it really does round-trip through the scanner at that size.
        let mut sc = BlockScanner::new();
        let evs = sc.feed(&hook("comp", &json, TOK));
        assert_eq!(evs.len(), 1, "a maximal listing must survive the scanner");
    }

    #[test]
    fn events_parse_and_offsets_point_after_terminator() {
        let data = stream();
        let mut sc = BlockScanner::new();
        let evs = sc.feed(&data);
        assert_eq!(
            evs[0].verb,
            HookVerb::Init {
                pid: 1234,
                shell: String::new(),
                home: String::new(),
                user: String::new()
            }
        );
        assert_eq!(evs[1].verb, HookVerb::PromptStart);
        assert_eq!(evs[2].verb, HookVerb::PromptEnd);
        assert_eq!(evs[3].verb, HookVerb::Exec { cmd: "echo hi".into() });
        assert_eq!(
            evs[4].verb,
            HookVerb::Pre { exit: Some(0), n: 2, cwd: "C:\\".into(), native: None }
        );
        assert_eq!(evs[5].verb, HookVerb::Exec { cmd: "dir".into() });
        // exec #1's offset points at the 'h' of "hi\r\n".
        let off = evs[3].offset_in_chunk;
        assert_eq!(&data[off..off + 2], b"hi");
        assert!(evs.iter().all(|e| e.token == TOK
            || matches!(e.verb, HookVerb::PromptEnd | HookVerb::PromptStart)));
    }

    /// P3 §11 + D* — the tokenless `133;A`/`133;B` prompt markers parse to
    /// `PromptStart`/`PromptEnd` identically at every chunk size, foreign
    /// OSCs still yield nothing, and each offset points just past its
    /// terminator.
    #[test]
    fn prompt_end_verb_parses_and_is_chunk_safe() {
        let mut data = b"\x1b]133;A\x07".to_vec();
        let after_a = data.len();
        data.extend_from_slice(b"PS C:\\>");
        data.extend_from_slice(b"\x1b]133;B\x07");
        let after = data.len();
        data.extend_from_slice(b"tail");
        let collect = |chunk: usize| -> Vec<(HookVerb, usize)> {
            let mut sc = BlockScanner::new();
            let mut out = Vec::new();
            let mut base = 0usize;
            for c in data.chunks(chunk) {
                for ev in sc.feed(c) {
                    assert!(ev.token.is_empty(), "prompt markers carry no token");
                    out.push((ev.verb, base + ev.offset_in_chunk));
                }
                base += c.len();
            }
            out
        };
        let whole = collect(data.len());
        assert_eq!(
            whole,
            vec![(HookVerb::PromptStart, after_a), (HookVerb::PromptEnd, after)]
        );
        assert_eq!(collect(1), whole);
        assert_eq!(collect(7), whole);
        assert_eq!(collect(64), whole);
        // ST-terminated variants parse too.
        let mut sc = BlockScanner::new();
        let evs = sc.feed(b"\x1b]133;B\x1b\\");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].verb, HookVerb::PromptEnd);
        let evs = sc.feed(b"\x1b]133;A\x1b\\");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].verb, HookVerb::PromptStart);
        // Near-misses stay silent.
        assert!(sc
            .feed(b"\x1b]133;\x07\x1b]133;Bx\x07\x1b]133;Ax\x07")
            .is_empty());
    }

    /// L-8: a stray ESC inside an OSC body must behave identically at every
    /// chunk size. Unsplit, ESC+BEL ABORTS the sequence (ESC+BEL is not ST)
    /// and ESC+ESC leaves the second ESC free to open a following sequence —
    /// the old scanner emitted the aborted OSC when the stream split exactly
    /// at the ESC.
    #[test]
    fn esc_inside_osc_body_is_chunk_invariant() {
        let mut data = b"\x1b]7717;".to_vec();
        data.extend_from_slice(TOK.as_bytes());
        data.extend_from_slice(b";exec;6869\x1b\x07"); // ESC BEL: abort, not emit
        data.extend(hook("exec", r#"{"c":"ok"}"#, TOK)); // recovery proof
        // ESC ESC: abort, then the second ESC opens a real sequence.
        data.extend_from_slice(b"\x1b]7717;junk\x1b\x1b]133;B\x07");
        let collect = |chunk: usize| -> Vec<HookVerb> {
            let mut sc = BlockScanner::new();
            let mut out = Vec::new();
            for c in data.chunks(chunk) {
                out.extend(sc.feed(c).into_iter().map(|e| e.verb));
            }
            out
        };
        let whole = collect(data.len());
        assert_eq!(
            whole,
            vec![HookVerb::Exec { cmd: "ok".into() }, HookVerb::PromptEnd],
            "aborted OSCs must never emit; the post-abort ESC must still open 133;B"
        );
        for chunk in [1, 2, 3, 7, 64] {
            assert_eq!(collect(chunk), whole, "divergence at chunk size {chunk}");
        }
    }

    /// Attribution Layer 3: the tcbeacon verb parses at every chunk size
    /// (mid-TUI delivery splits arbitrarily), tolerates a future 4th field,
    /// keeps an empty token, and drops sid-less shapes. Ordinary hook
    /// tokens can never collide with the literal (16 hex chars ≠ "tcbeacon").
    #[test]
    fn tcbeacon_parses_and_is_chunk_safe() {
        let sid = "1b2f3c4d-0000-4000-8000-1b2f3c4d0000";
        let mut data = b"claude tui paint \x1b[38;5;10m*\x1b[0m ".to_vec();
        data.extend_from_slice(
            format!("\x1b]7717;tcbeacon;SessionStart;resume;{sid}\x07").as_bytes(),
        );
        data.extend_from_slice(b"more paint");
        let collect = |chunk: usize| -> Vec<HookVerb> {
            let mut sc = BlockScanner::new();
            let mut out = Vec::new();
            for c in data.chunks(chunk) {
                out.extend(sc.feed(c).into_iter().map(|e| {
                    assert!(e.token.is_empty(), "beacon carries no token");
                    e.verb
                }));
            }
            out
        };
        let whole = collect(data.len());
        assert_eq!(
            whole,
            vec![HookVerb::Beacon {
                adapter: "claude".into(),
                event: "SessionStart".into(),
                source: "resume".into(),
                sid: sid.into(),
                cwd: None,
            }],
            "legacy 3-field claude beacon defaults adapter to claude"
        );
        for chunk in [1, 3, 7, 64] {
            assert_eq!(collect(chunk), whole, "divergence at chunk size {chunk}");
        }
        // ST-terminated + a trailing non-hex future field: sid still lands
        // whole; the junk field 4 is NOT a cwd (F1 v2 gate ⇒ None).
        let mut sc = BlockScanner::new();
        let evs = sc.feed(
            format!("\x1b]7717;tcbeacon;SessionEnd;clear;{sid};v2\x1b\\").as_bytes(),
        );
        assert_eq!(
            evs[0].verb,
            HookVerb::Beacon {
                adapter: "claude".into(),
                event: "SessionEnd".into(),
                source: "clear".into(),
                sid: sid.into(),
                cwd: None,
            }
        );
        // The codex adapter-carrying form: `tcbeacon;codex;<event>;<source>;<sid>`.
        let mut sc = BlockScanner::new();
        let evs = sc.feed(format!("\x1b]7717;tcbeacon;codex;SessionStart;startup;{sid}\x07").as_bytes());
        assert_eq!(
            evs[0].verb,
            HookVerb::Beacon {
                adapter: "codex".into(),
                event: "SessionStart".into(),
                source: "startup".into(),
                sid: sid.into(),
                cwd: None,
            }
        );
        // F1 beacon v2: the 4-field form carries hex(cwd) — decoded, gated.
        let cwd_hex = crate::strip::hex_lower(b"/srv/app");
        let mut sc = BlockScanner::new();
        let evs = sc.feed(
            format!("\x1b]7717;tcbeacon;claude;SessionStart;startup;{sid};{cwd_hex}\x07")
                .as_bytes(),
        );
        assert_eq!(
            evs[0].verb,
            HookVerb::Beacon {
                adapter: "claude".into(),
                event: "SessionStart".into(),
                source: "startup".into(),
                sid: sid.into(),
                cwd: Some("/srv/app".into()),
            }
        );
        // v2 gates: non-absolute, empty, junk hex, oversized ⇒ cwd None.
        let cwd = |payload: &str| -> Option<String> {
            let mut sc = BlockScanner::new();
            let evs = sc.feed(
                format!("\x1b]7717;tcbeacon;claude;SessionStart;startup;{sid};{payload}\x07")
                    .as_bytes(),
            );
            match &evs[0].verb {
                HookVerb::Beacon { cwd, .. } => cwd.clone(),
                v => panic!("not a beacon: {v:?}"),
            }
        };
        assert_eq!(cwd(&crate::strip::hex_lower(b"relative/path")), None);
        assert_eq!(cwd(""), None);
        assert_eq!(cwd("zz"), None);
        assert_eq!(cwd("abc"), None, "odd-length hex");
        let huge = crate::strip::hex_lower(format!("/{}", "x".repeat(1100)).as_bytes());
        assert_eq!(cwd(&huge), None, "oversized cwd is dropped");
        // sid-less beacons drop (both forms); a REAL token named "tcbeacon" is
        // impossible (tokens are 16 hex) so the plain-verb path stays unshadowed.
        assert!(sc.feed(b"\x1b]7717;tcbeacon;SessionStart\x07").is_empty());
        assert!(sc.feed(b"\x1b]7717;tcbeacon;SessionStart;startup;\x07").is_empty());
        assert!(sc.feed(b"\x1b]7717;tcbeacon;codex;SessionStart;startup;\x07").is_empty());
    }

    #[test]
    fn malformed_and_foreign_bodies_are_dropped_or_lenient() {
        let mut sc = BlockScanner::new();
        // Foreign OSC codes produce nothing (133;A/B are ours since D*/P3).
        assert!(sc.feed(b"\x1b]9;9;C:\\\x07\x1b]0;title\x07").is_empty());
        // Odd-length / non-hex payloads drop.
        assert!(sc.feed(b"\x1b]7717;t;exec;abc\x07").is_empty());
        assert!(sc.feed(b"\x1b]7717;t;exec;zz\x07").is_empty());
        // Unknown verb drops.
        assert!(sc.feed(b"\x1b]7717;t;boom;7b7d\x07").is_empty());
        // Empty JSON object parses leniently (fields default) so the token
        // check upstream — where spoofs are logged — still sees the event.
        let evs = sc.feed(b"\x1b]7717;00000000deadbeef;pre;7b7d\x07");
        assert_eq!(
            evs[0].verb,
            HookVerb::Pre { exit: None, n: 0, cwd: String::new(), native: None }
        );
        // Oversized body is abandoned without producing an event.
        let mut big = b"\x1b]7717;t;exec;".to_vec();
        big.extend(std::iter::repeat_n(b'6', BODY_CAP + 10));
        big.push(0x07);
        assert!(sc.feed(&big).is_empty());
        // …and the scanner recovers for the next sequence.
        assert_eq!(sc.feed(&hook("exec", r#"{"c":"ok"}"#, TOK)).len(), 1);
    }

    fn rec(start: u64, end: Option<u64>) -> BlockRec {
        BlockRec {
            epoch: 1,
            n: 0,
            cmd: "x".into(),
            cwd: None,
            exit: end.map(|_| 0),
            started_ms: 0,
            ended_ms: None,
            start_off: start,
            end_off: end,
            truncated: false,
        }
    }

    #[test]
    fn eviction_drops_flags_and_retargets_open() {
        let mut st = BlockStore {
            token: TOK.into(),
            epoch: 1,
            base: 0,
            recs: vec![rec(0, Some(50)), rec(60, Some(150)), rec(160, None)],
            open: Some(2),
            last_cwd: None,
            hooks_live: false,
            synthetic_open: false,
            pending_close: None,
            nested_tokens: Vec::new(),
        };
        st.evict(100);
        assert_eq!(st.base, 100);
        assert_eq!(st.recs.len(), 2, "fully-pre-base record evicted");
        assert!(st.recs[0].truncated, "straddling record flagged");
        assert_eq!(st.recs[0].end_off, Some(150));
        assert!(!st.recs[1].truncated);
        assert_eq!(st.open, Some(1), "open index follows the survivor");
        // A cut past the open block's start truncates it too (end=None is
        // 'extends to head': never evicted).
        st.evict(200);
        assert_eq!(st.recs.len(), 1);
        assert!(st.recs[0].truncated);
        assert_eq!(st.open, Some(0));
    }

    #[test]
    fn exec_exec_closes_first_dangling() {
        let mut st = BlockStore {
            token: TOK.into(),
            epoch: 1,
            base: 0,
            recs: Vec::new(),
            open: None,
            last_cwd: None,
            hooks_live: false,
            synthetic_open: false,
            pending_close: None,
            nested_tokens: Vec::new(),
        };
        st.open_block("first".into(), 10, 1);
        let changed = st.open_block("second".into(), 90, 2);
        assert_eq!(st.recs[0].end_off, Some(90), "first closed at second's offset");
        assert_eq!(st.recs[0].exit, None);
        assert_eq!(changed, vec![0, 1]);
        // D*: the pre ARMS the close (end deferred); the 133;A anchor lands it.
        assert_eq!(st.on_pre(Some(3), 7, "C:\\w".into(), 200, 3), None);
        assert_eq!(st.recs[1].end_off, None, "close deferred to the anchor");
        assert!(st.pre_close_armed());
        assert_eq!(st.on_prompt_start(230, 3), Some(1));
        assert_eq!(st.recs[1].exit, Some(3));
        assert_eq!(st.recs[1].n, 7);
        assert_eq!(st.recs[1].end_off, Some(230));
        // A pre with no open block just refreshes cwd (and arms nothing).
        assert_eq!(st.on_pre(Some(0), 8, "C:\\x".into(), 300, 4), None);
        assert!(!st.pre_close_armed());
        assert_eq!(st.last_cwd.as_deref(), Some(std::path::Path::new("C:\\x")));
    }

    /// D2 misattribution guard: a SYNTHETIC-opened block (SubmitCommand lane
    /// — heuristic nested-shell episode, or cmd) closes with `exit = None`
    /// regardless of the closing pre's payload: the pre that ends a nested
    /// episode is the LOGIN shell's, carrying the OUTER command's exit
    /// (`sudo su`'s 0), which must never stamp the last inner command.
    /// Hook-opened blocks (a real exec) keep taking the real exit — including
    /// one that dangling-closes a synthetic predecessor.
    #[test]
    fn synthetic_close_exit_none() {
        let mut st = BlockStore::load(Uuid::new_v4()); // no sidecar
        st.rotate(TOK.into());
        // Inner command via the synthetic lane; the returning login-shell
        // pre carries exit 0 (sudo su's) — the rec must close exit None.
        // (D*: arm at the pre, land at the 133;A anchor.)
        st.open_block_synthetic("whoami".into(), 100, 1);
        assert_eq!(st.on_pre(Some(0), 7, "/root".into(), 200, 2), None);
        assert_eq!(st.on_prompt_start(210, 2), Some(0));
        assert_eq!(st.recs[0].exit, None, "outer exit must not misattribute");
        assert_eq!(st.recs[0].end_off, Some(210));
        // Integration is back: a hook-opened block takes its real exit.
        st.open_block("false".into(), 300, 3);
        assert_eq!(st.on_pre(Some(1), 8, "/root".into(), 400, 4), None);
        assert_eq!(st.on_prompt_start(410, 4), Some(1));
        assert_eq!(st.recs[1].exit, Some(1), "hook-opened keeps the real exit");
        // A hook exec dangling-closing a synthetic predecessor CLEARS the
        // flag: the successor's pre stamps its real exit again.
        st.open_block_synthetic("inner".into(), 500, 5);
        st.open_block("outer".into(), 600, 6);
        assert_eq!(st.recs[2].end_off, Some(600), "dangling close at successor");
        assert_eq!(st.recs[2].exit, None);
        assert_eq!(st.on_pre(Some(9), 9, String::new(), 700, 7), None);
        assert_eq!(st.on_prompt_start(710, 7), Some(3));
        assert_eq!(st.recs[3].exit, Some(9), "hook open cleared the flag");
        // close_dangling resets the flag too (launch rotation / exit).
        st.open_block_synthetic("dangler".into(), 800, 8);
        st.close_dangling(900, 9);
        assert!(!st.synthetic_open);
    }

    /// P5 §15 hooks_live_lifecycle: load ⇒ false; a token-checked event sets
    /// it; a launch rotation resets it; a wrong-token event never sets it.
    #[test]
    fn hooks_live_lifecycle() {
        let mut st = BlockStore::load(Uuid::new_v4()); // no sidecar on disk
        assert!(!st.hooks_live, "fresh load must start unverified");
        st.rotate(TOK.into());
        assert_eq!(st.epoch, 1);
        assert!(!st.hooks_live, "rotation itself proves nothing");
        assert!(st.classify_token("00000000deadbeef").is_none(), "wrong token rejected");
        assert!(!st.hooks_live, "a rejected event must not verify the hooks");
        assert!(st.classify_token(TOK).is_some());
        assert!(st.hooks_live, "a correct-token event proves the bootstrap ran");
        st.rotate("fedcba9876543210".into());
        assert_eq!(st.epoch, 2);
        assert!(!st.hooks_live, "a new spawn starts unverified again");
        assert!(st.classify_token(TOK).is_none(), "the old token no longer matches");
    }

    /// nested-shell-hooks — the token scope registry: an injected nested
    /// shell's token is accepted ALONGSIDE the outer one, `classify_token`
    /// says which world spoke, a shell speaking retires every world inside
    /// it, and a spawn rotation drops them all.
    #[test]
    fn nested_token_scopes() {
        let mut st = BlockStore::load(Uuid::new_v4());
        st.rotate(TOK.into());
        assert_eq!(st.nested_depth(), 0);
        assert_eq!(st.classify_token(TOK), Some(HookScope::Outer));
        // An unregistered token is still a spoof — the stale-token warning
        // path is unchanged for everything we did not mint.
        assert!(st.classify_token("deadbeefdeadbeef").is_none());

        assert_eq!(st.push_nested_token("aaaa1111aaaa1111".into()), Some(1));
        assert_eq!(st.push_nested_token("bbbb2222bbbb2222".into()), Some(2));
        assert_eq!(st.nested_depth(), 2);
        assert_eq!(
            st.classify_token("aaaa1111aaaa1111"),
            Some(HookScope::Nested { depth: 1 })
        );
        assert_eq!(
            st.classify_token("bbbb2222bbbb2222"),
            Some(HookScope::Nested { depth: 2 }),
        );
        assert_eq!(st.classify_token(TOK), Some(HookScope::Outer), "outer still speaks");
        assert_eq!(HookScope::Outer.depth(), 0);
        assert!(HookScope::Outer.is_outer());
        assert!(!HookScope::Nested { depth: 1 }.is_outer());

        // The depth-1 shell speaking means the depth-2 world is gone: its
        // token stops being accepted (a dead shell can never emit again, but
        // an unbounded accept-list is exactly how a stale token would come
        // back to bite).
        assert_eq!(st.pop_nested_below(1), 1);
        assert!(st.classify_token("bbbb2222bbbb2222").is_none());
        assert_eq!(
            st.classify_token("aaaa1111aaaa1111"),
            Some(HookScope::Nested { depth: 1 })
        );
        // The OUTER shell speaking is a full reset of the nested world.
        assert_eq!(st.pop_nested_below(0), 1);
        assert_eq!(st.nested_depth(), 0);
        assert!(st.classify_token("aaaa1111aaaa1111").is_none());

        // Cap: a chain deeper than NESTED_TOKEN_MAX stops registering rather
        // than growing the accept-list without bound.
        for i in 0..NESTED_TOKEN_MAX {
            assert_eq!(st.push_nested_token(format!("{i:016x}")), Some(i + 1));
        }
        assert_eq!(st.push_nested_token("ffffffffffffffff".into()), None);
        assert!(st.classify_token("ffffffffffffffff").is_none());

        // A spawn rotation drops every nested token: the new shell's world
        // has no nested shells yet (they died with the old process tree).
        st.rotate("1111222233334444".into());
        assert_eq!(st.nested_depth(), 0);
        assert!(st.classify_token(TOK).is_none());
        assert_eq!(st.classify_token("1111222233334444"), Some(HookScope::Outer));
    }

    /// r2-F6: records beyond the journal's real head mean the sidecar's base
    /// predates the file's last compaction (crash between the two commits) —
    /// the ledger is dropped; a consistent ledger is untouched.
    #[test]
    fn reconcile_drops_records_beyond_the_journal_head() {
        let mut st = BlockStore {
            token: TOK.into(),
            epoch: 3,
            base: 0,
            recs: Vec::new(),
            open: None,
            last_cwd: None,
            hooks_live: false,
            synthetic_open: false,
            pending_close: None,
            nested_tokens: Vec::new(),
        };
        st.open_block("ok".into(), 100, 1);
        st.on_pre(Some(0), 1, String::new(), 900, 2);
        st.open_block("open".into(), 950, 3);
        // Consistent: head at or beyond every offset — nothing changes.
        assert!(!st.reconcile_with_journal_head(1000));
        assert_eq!(st.recs.len(), 2);
        assert_eq!(st.open, Some(1));
        // Cross-wired: the open block's start is beyond the head.
        assert!(st.reconcile_with_journal_head(920));
        assert!(st.recs.is_empty(), "the whole ledger drops, honestly");
        assert_eq!(st.open, None);
        // Idempotent on the now-empty store.
        assert!(!st.reconcile_with_journal_head(0));
    }

    /// D* (a) — the ConPTY reorder the pre-sleep used to absorb: the pre OSC
    /// arrives BEFORE the command's last output rows; with the sleep gone,
    /// end_off must land at the 133;A that rides the text frame AFTER the
    /// full tail — the tail stays inside the block, the prompt text outside.
    /// Walked scanner→store exactly like the ingest loop, at several chunk
    /// sizes (the reorder can split anywhere).
    #[test]
    fn deferred_close_brackets_reordered_output_tail() {
        // Stream: [exec][early output][pre][9;9][REORDERED TAIL][133;A][prompt]
        let mut data = Vec::new();
        data.extend(hook("exec", r#"{"c":"ls"}"#, TOK));
        let exec_off = data.len() as u64;
        data.extend_from_slice(b"alpha\r\n");
        data.extend(hook("pre", r#"{"e":0,"n":2,"d":"C:\\"}"#, TOK));
        data.extend_from_slice(b"\x1b]9;9;C:\\\x07");
        data.extend_from_slice(b"TAIL_beta\r\n"); // the late text frame
        data.extend_from_slice(b"\x1b]133;A\x07");
        let anchor_off = data.len() as u64;
        data.extend_from_slice(b"PS C:\\> ");
        for chunk in [data.len(), 1, 7, 64] {
            let mut sc = BlockScanner::new();
            let mut st = BlockStore::load(Uuid::new_v4());
            st.rotate(TOK.into());
            let mut abs = 0u64;
            for c in data.chunks(chunk) {
                for ev in sc.feed(c) {
                    let off = abs + ev.offset_in_chunk as u64;
                    match ev.verb {
                        // Tokenless anchor — mirrors on_block_event's
                        // pre-token-check handling.
                        HookVerb::PromptStart => {
                            st.on_prompt_start(off, 3);
                        }
                        _ if st.classify_token(&ev.token).is_none() => {}
                        HookVerb::Exec { cmd } => {
                            st.open_block(cmd, off, 1);
                        }
                        HookVerb::Pre { exit, n, cwd, .. } => {
                            assert_eq!(st.on_pre(exit, n, cwd, off, 2), None);
                        }
                        _ => {}
                    }
                }
                abs += c.len() as u64;
            }
            let r = &st.recs[0];
            assert_eq!(r.start_off, exec_off, "chunk {chunk}");
            assert_eq!(
                r.end_off,
                Some(anchor_off),
                "end_off must be the 133;A anchor (chunk {chunk})"
            );
            assert_eq!(r.exit, Some(0));
            assert_eq!(r.n, 2);
            // The money property: the block's byte span contains the
            // reordered tail and none of the prompt text.
            let body = &data[r.start_off as usize..r.end_off.unwrap() as usize];
            assert!(
                body.windows(9).any(|w| w == b"TAIL_beta"),
                "reordered tail clipped from the block (chunk {chunk})"
            );
            assert!(
                !body.windows(3).any(|w| w == b"PS "),
                "prompt text folded into the block (chunk {chunk})"
            );
        }
    }

    /// D* (b) — MANDATORY fallback: the 133;A never arrives (user clobbered
    /// `prompt`, hooks stripped) — the flush closes at the pre position with
    /// the armed exit/n and the ARM-time stamp: byte- and time-identical to
    /// the sleep-era immediate close.
    #[test]
    fn fallback_closes_at_pre_position() {
        let mut st = BlockStore::load(Uuid::new_v4());
        st.rotate(TOK.into());
        st.open_block("make".into(), 100, 1);
        assert_eq!(st.on_pre(Some(2), 5, "C:\\w".into(), 400, 2), None);
        assert!(st.pre_close_armed());
        assert_eq!(st.recs[0].end_off, None, "close deferred while armed");
        assert_eq!(st.flush_pending_close(), Some(0));
        assert!(!st.pre_close_armed());
        let r = &st.recs[0];
        assert_eq!(r.end_off, Some(400), "fallback = the pre's own offset");
        assert_eq!(r.exit, Some(2));
        assert_eq!(r.n, 5);
        assert_eq!(r.ended_ms, Some(2), "arm-time stamp, not flush time");
        // A late 133;A after the flush is spurious — nothing re-closes.
        assert_eq!(st.on_prompt_start(500, 9), None);
        assert_eq!(st.recs[0].end_off, Some(400));
    }

    /// D* (c) — the spurious-marker guard: 133;A is honored ONLY while a
    /// pre-armed close waits, and only the FIRST one. A program echoing the
    /// sequence mid-output (no pre yet) and any second marker are no-ops.
    #[test]
    fn spurious_prompt_start_is_ignored() {
        let mut st = BlockStore::load(Uuid::new_v4());
        st.rotate(TOK.into());
        st.open_block("cat file".into(), 10, 1);
        // Mid-output 133;A: no close armed — nothing may change.
        assert_eq!(st.on_prompt_start(50, 2), None);
        assert_eq!(st.recs[0].end_off, None);
        assert_eq!(st.open, Some(0), "block stays open through a fake marker");
        // The real pre arms; the FIRST 133;A closes; the second is spurious.
        assert_eq!(st.on_pre(Some(0), 3, String::new(), 90, 3), None);
        assert_eq!(st.on_prompt_start(120, 4), Some(0));
        assert_eq!(st.recs[0].end_off, Some(120));
        assert_eq!(st.recs[0].ended_ms, Some(4), "anchor close stamps its own time");
        assert_eq!(st.on_prompt_start(150, 5), None);
        assert_eq!(st.recs[0].end_off, Some(120), "the first anchor wins");
    }

    /// D* — dangling closes flush an armed close with its REAL exit at the
    /// pre position (the pre DID arrive; only its anchor didn't), while
    /// genuinely unclosed records still sweep to exit=None at the head.
    #[test]
    fn close_dangling_flushes_armed_close_with_real_exit() {
        let mut st = BlockStore::load(Uuid::new_v4());
        st.rotate(TOK.into());
        st.open_block("false".into(), 100, 1);
        assert_eq!(st.on_pre(Some(7), 2, String::new(), 200, 2), None);
        let changed = st.close_dangling(900, 9);
        assert_eq!(changed, vec![0]);
        assert_eq!(st.recs[0].end_off, Some(200), "armed close keeps its pre offset");
        assert_eq!(st.recs[0].exit, Some(7), "armed close keeps its real exit");
        assert!(!st.pre_close_armed());
        // An un-armed dangler still closes honestly at the head, exit None.
        st.open_block("hang".into(), 1000, 10);
        let changed = st.close_dangling(1100, 11);
        assert_eq!(changed, vec![1]);
        assert_eq!(st.recs[1].end_off, Some(1100));
        assert_eq!(st.recs[1].exit, None);
    }

    /// D* — a lost 133;A never wedges the ledger: a following exec flushes
    /// the armed close at its pre position before opening, and a following
    /// pre (bash themes can strip the PS1 wrap per-render while PROMPT_COMMAND
    /// keeps firing) does the same.
    #[test]
    fn exec_or_pre_while_armed_flushes_previous() {
        let mut st = BlockStore::load(Uuid::new_v4());
        st.rotate(TOK.into());
        st.open_block("a".into(), 10, 1);
        assert_eq!(st.on_pre(Some(0), 1, String::new(), 80, 2), None);
        let changed = st.open_block("b".into(), 100, 3);
        assert_eq!(changed, vec![0, 1]);
        assert_eq!(st.recs[0].end_off, Some(80), "flushed at ITS pre, not the exec");
        assert_eq!(st.recs[0].exit, Some(0));
        // Consecutive pres: the second flushes the first's armed close.
        assert_eq!(st.on_pre(Some(4), 2, String::new(), 300, 4), None);
        assert_eq!(st.on_pre(Some(9), 3, String::new(), 500, 5), Some(1));
        assert_eq!(st.recs[1].end_off, Some(300));
        assert_eq!(st.recs[1].exit, Some(4), "first pre's exit, not the second's");
        assert!(!st.pre_close_armed(), "second pre had no open block to arm");
    }
}
