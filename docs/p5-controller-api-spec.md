# P5 "Controller API" — Implementation Spec (final, implementation-ready)

Target: C:\Terminal Control (Rust daemon + egui 0.35 GUI, single crate). P5 adds a
programmatic control surface so external tools — shell scripts, automations, and above all
**Claude Code agents managing the user's ~20 sessions** — can drive the terminal manager:
list sessions/folders, create/kill/restart terminals, send input, read output/blocks,
wait for conditions, and subscribe to events.

Shape: **CLI-first**. A console-subsystem companion binary `tc.exe` speaks the existing
loopback bincode protocol (new appended variants) and prints stable JSON. No second wire
format, no client libraries, no new listener. warpctrl inspired the *concept* (loopback
control endpoint + per-action credentials + typed command catalog); everything below is
clean-room from this codebase's own shapes — no Warp code was read for this design.

**Zero GUI changes.** P5 is daemon + protocol + a new CLI module + probes. This also keeps
it merge-safe against p2-impl-2's in-flight GUI work; the only shared files are
protocol.rs and daemon/mod.rs, where P5 strictly APPENDS.

Ordered as the implementation plan: invariants → decisions → protocol → security → daemon
dispatch → verb semantics → wait engine → events → session additions → CLI → ergonomics →
perf → degraded modes → compat → probes → tests → docs snippet → checklist → open
questions → DO-NOTs → order. Each decision carries a one-line justification.

---

## 0. Non-negotiable invariants (violating any is a bug)

1. **Mirror purity / parser purity**: the controller never injects a byte into any VT
   parser stream or journal that conhost didn't emit. The ONLY bytes it ever writes are
   user-intended PTY input through the same path as `C2D::Input` (session writer), and
   daemon-authored text through the already-sanctioned `emit_output` (never used by P5 —
   listed to be explicit).
2. **Ingest atomicity untouched**: `Core::ingest` keeps parsing + journaling + fanout
   atomic under the journal lock. P5's per-chunk work (waiter feed) runs strictly AFTER
   the journal lock is released, exactly like `on_journal_compact`.
3. **bincode append-only**: new `C2D`/`D2C` variants go at the very END, after P2's
   `BlockText` (the current last variant in both enums — verify at merge time and append
   after whatever is last; source order == wire index). No fields added to existing
   variants. The new `CtlRequest`/`CtlBody`/`CtlEvent`/`WaitCond` enums are ALSO
   bincode-positional — the append-only discipline applies inside them forever.
   `DaemonInfo.proto` bumps 2 → 3. (P3 ships no protocol change; P5 owns proto=3.)
4. **Controller connections are never fanout targets**: a controller client never
   Attaches and never receives `D2C::Output`/`Replay`. Measured incident basis: each
   attached client costs the daemon ~+3.5–4s CPU per 50MB flood. Reads are pull
   (journal/grid snapshots); events are the rare block/exit/state notifications only.
5. **No daemon-side polling loops**: wait timeouts ride the existing 250ms journal-flush
   tick, gated by an atomic count so the tick pays zero when no waiters exist. Idle
   controller connections cost one parked reader thread and nothing else.
6. **Read paths never disturb live state**: no `do_resize` (Attach's resize-to-client is
   a GUI-attach behavior — a controller read must never reflow the user's grid), never
   seek the journal append handle (fresh `File`, the `read_range`/`tail` pattern), term
   locks held only for a bounded grid walk.
7. **Same-user trust model, stated honestly**: scoped tokens are guardrails for
   cooperating-but-fallible agents, NOT a security boundary — any same-user process can
   read daemon.json and obtain full rights. Cross-user and remote access remain blocked
   by the user-private data-dir ACL and the 127.0.0.1 bind.

---

## 1. Headline decisions

| # | Decision | One-line justification |
|---|---|---|
| D1 | **CLI-first surface**: `tc.exe <verb>` speaks internal bincode, prints JSON; no JSON-lines socket for third parties in v1 | Any tool (Claude Code included) can drive it with zero client code, and the JSON contract lives in one translator we own — a second wire dialect would double the compat surface for no consumer we actually have |
| D2 | JSON-lines-over-socket **evaluated and rejected for v1**: dual framing on one port needs sniffing or a second listener, and everything is loopback-only where the exe always exists (single-exe design) | The `Ctl` variants are designed so a socket JSON gateway could be added later as a pure translator, without daemon changes |
| D3 | **Separate console-subsystem binary `tc.exe`** (auto-discovered `src/bin/tc.rs`), sharing `ctl.rs`/`protocol.rs`/`state.rs`/`strip.rs` via `#[path]` | The main exe is `windows_subsystem="windows"` in release: PowerShell does not wait for it and its stdout is LOST (documented ops incident — probes need Start-Process -Wait); a controller CLI that loses its output is dead on arrival |
| D4 | Same daemon TCP port, new first-frame handshake `C2D::HelloCtl { token, self_session }`; scope stored per connection | One listener = one auth story, one accept loop, no port discovery changes; the scope on `ClientConn` makes enforcement a single guard in `handle_message` |
| D5 | ONE appended request/reply envelope pair — `C2D::Ctl { req_id, req: CtlRequest }` / `D2C::Ctl { req_id, body: CtlBody }` — instead of many top-level variants | Keeps the core protocol readable, gives every request a correlation id (needed for concurrent waits and event streams on one connection), and concentrates append-only growth in the namespaced sub-enums |
| D6 | Scoped tokens minted by the daemon on request (master-token-only), persisted in `ctl-tokens.json`; scopes = READ / INPUT / MANAGE bitflags; master token = FULL | Lets the user hand an agent a token that cannot `DeleteTerminal` or `Shutdown`; persistence across daemon restarts means scripts don't break every reboot (daemon.json's master token rotates per run) |
| D7 | `run` is **gated by the P3 gate core**, daemon-side: hooked + hooks-live + running + not alt-screen + no open block; `--force` overrides; `send`/chords are the explicitly-raw escape hatch | Automation must not type into a running TUI by accident; raw send exists precisely FOR driving TUIs, so gating it would only train everyone to pass --force |
| D8 | `run --wait` is a **daemon-side composite** (submit → wait for the block spawned after the submission offset → return exit/duration/output in one reply) | One command = one JSON = the flagship agent ergonomic, and doing it daemon-side closes the register-after-submit race a CLI-composed run+wait would have |
| D9 | Submission encoding = P3's `submission_bytes` exactly (trim, `\n`→`\r`, bracketed iff the mirror's `TermMode::BRACKETED_PASTE`, trailing `\r` outside brackets), computed daemon-side from the mirror | The mirror is the ground truth for BRACKETED_PASTE (PSReadLine 2.0 would eat literal `ESC[200~` as garbage) and this is the one input path already probe-verified end-to-end |
| D10 | Named-key chords (`ctrl+c`, `enter`, …) are encoded **daemon-side** via `win32_input::encode_key` when `Session.win32_input` is set, VT fallback otherwise | Keyboard fidelity must be decided where the mode state lives; the CLI cannot know whether conhost negotiated mode 9001 |
| D11 | **Recursion guard**: sessions spawn with `TC_SESSION_ID=<uuid>` in their env; the CLI forwards it in `HelloCtl.self_session`; the daemon refuses input/kill/restart/delete targeting that id unless the request sets `force_self` | An agent typing into its own terminal is a feedback loop and killing it is mid-task suicide; reads of self stay allowed (reading your own scrollback is useful) |
| D12 | `wait-for` is an event-driven **waiter registry** resolved at the existing hook sites (`on_block_event`, `on_exit`, post-ingest), timeouts swept by the 250ms flush tick | The client reader thread must never block inside `handle_message` (it would wedge that client's own subsequent frames), and no new thread/poll loop is allowed (inv. 5) |
| D13 | Events = `subscribe`/`tc watch` streaming `D2C::Ctl { req_id, body: Event }` frames — block open/close, exit, coarse state-changed; NEVER Output | Rare, tiny frames piggybacking existing notify sites; "state changed, re-list" beats diff machinery for v1 |
| D14 | No rate limits | Same-user trust model; a flooding controller is indistinguishable from a paste, and the real protections already exist (CLIENT_QUEUE_DEPTH, MAX_FRAME, journal caps, waiter caps) |
| D15 | JSON on stdout always — success AND errors (`{"ok":false,"code":…}`) — with distinguishing process exit codes | Agents parse exactly one stream and can branch on exit code without parsing when they don't care |

---

## 2. Protocol (src/protocol.rs)

### 2.1 New variants — appended at enum END (bincode is positional)

```rust
// C2D — append AFTER `BlockText` (current last; if another phase landed later
// variants first, append after THOSE — source order is wire order):

    /// Controller handshake — the alternative first frame to `Hello`.
    /// `token` is either the master daemon.json token (FULL rights) or a
    /// scoped controller token from ctl-tokens.json. `self_session` is the
    /// TC_SESSION_ID env of the terminal this controller runs inside, if any;
    /// the daemon refuses Run/SendRaw/SendChord/Kill/Restart/Delete against
    /// that id unless the request sets force_self (recursion guard, D11).
    HelloCtl { token: String, self_session: Option<Uuid> },
    /// Typed controller request. `req_id` is client-chosen and echoed on every
    /// reply/stream frame for this request; the client keeps it unique among
    /// its own in-flight requests (the daemon only echoes it).
    Ctl { req_id: u64, req: CtlRequest },
```

```rust
// D2C — append AFTER `BlockText`:

    /// Controller reply or event stream frame. One-shot verbs get exactly one
    /// frame; Subscribe and Run{wait}/Wait get their frames later, when the
    /// condition resolves (still tagged with the originating req_id).
    Ctl { req_id: u64, body: CtlBody },
```

### 2.2 The controller catalog (new types in protocol.rs — shared daemon/CLI)

All of these are bincode-positional: **append-only forever, same as C2D/D2C.**

```rust
/// Scope bitflags. FULL is reserved for the master token: it additionally
/// unlocks the legacy C2D verbs (the GUI protocol) and Token*/Shutdown.
pub const SCOPE_READ: u32 = 1;   // List, Read*, Wait, Subscribe/Unsubscribe
pub const SCOPE_INPUT: u32 = 2;  // Run, SendRaw, SendChord
pub const SCOPE_MANAGE: u32 = 4; // CreateTerminal/Folder, Kill, Restart, Delete
pub const SCOPE_FULL: u32 = u32::MAX;
// CLI presets: read = 1, input = 1|2 (input without list is unusable),
// manage = 1|2|4.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CtlRequest {
    List,
    CreateTerminal { spec: NewTerminal },
    CreateFolder { name: String },
    /// Submit a command line to a hooked shell at a (best-effort) idle prompt.
    /// Refused when a block is open / alt-screen / hooks unverified, unless
    /// `force`. `wait`: daemon-side composite — reply arrives when the block
    /// spawned by this submission closes (RunDone) or timeout.
    Run { id: Uuid, cmd: String, force: bool, force_self: bool, wait: Option<RunWait> },
    /// Raw bytes to the PTY, ungated by design (driving TUIs is its purpose).
    SendRaw { id: Uuid, bytes: Vec<u8>, force_self: bool },
    /// A named key chord, encoded daemon-side per the session's input mode.
    SendChord { id: Uuid, chord: CtlChord, force_self: bool },
    /// The visible grid as text (works for TUIs/claude; alt-screen reads the
    /// active — alt — grid, which is exactly what the caller wants).
    ReadScreen { id: Uuid },
    /// Last `lines` complete lines of the journal tail, ANSI/OSC-stripped.
    ReadTail { id: Uuid, lines: u32 },
    ReadBlocks { id: Uuid, last: u32 },
    /// Same semantics as C2D::BlockText, delivered as a Ctl reply.
    ReadBlockText { id: Uuid, start_off: u64 },
    Wait { id: Uuid, cond: WaitCond, timeout_ms: u64 },
    Kill { id: Uuid, force_self: bool },
    Restart { id: Uuid, force_self: bool },
    Delete { id: Uuid, force_self: bool },
    Subscribe { ids: Option<Vec<Uuid>>, kinds: u32 }, // EV_* bitflags below
    Unsubscribe { req_id: u64 },                      // the Subscribe's req_id
    TokenCreate { name: String, scope: u32 },         // master token only
    TokenRevoke { name: String },                     // master token only
    TokenList,                                        // master token only
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RunWait { pub timeout_ms: u64, pub tail_bytes: u32 }

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
    OutputMatch { pattern: String, regex: bool, from_off: Option<u64> },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CtlChord {
    Enter, Esc, Tab, Backspace, Up, Down, Left, Right,
    Home, End, PageUp, PageDown,
    CtrlC, CtrlD, CtrlZ, CtrlL,
}

pub const EV_BLOCKS: u32 = 1; // BlockOpened / BlockClosed
pub const EV_EXIT: u32 = 2;   // Exited
pub const EV_STATE: u32 = 4;  // StateChanged (coarse: re-List to see what)

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
    Err { code: String, msg: String },
    Listing { folders: Vec<Folder>, terminals: Vec<CtlTerm> },
    Created { id: Uuid },
    /// Ack for Kill/Restart/Delete/CreateFolder/Unsubscribe/TokenRevoke.
    Done,
    Screen { lines: Vec<String>, cursor_row: u16, cursor_col: u16, alt_screen: bool },
    Tail { lines: Vec<String>, truncated: bool },
    Blocks { recs: Vec<BlockRec> },
    BlockText { text: String, truncated: bool },
    /// Run without wait: the submission was written; at_off = absolute journal
    /// offset captured just before the write (the spawned block's start_off
    /// will be >= at_off — feed it to Wait{BlockClose{after_off}}).
    RunStarted { at_off: u64 },
    /// Run with wait: the block closed (or session died closing it dangling).
    RunDone { exit: Option<i64>, duration_ms: u64, output: String,
              truncated: bool, start_off: u64 },
    /// Wait resolved. `hit`: which condition fired, with its payload.
    Waited { hit: WaitHit },
    Subscribed,
    Event { ev: CtlEvent },
    Token { name: String, token: String, scope: u32 },
    Tokens { list: Vec<CtlTokenInfo> },
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
    pub kind: String,                    // "shell" | "claude" | "custom"
    pub claude_session: Option<Uuid>,    // TermKind::Claude pinned id
    pub inner_cli: Option<InnerCli>,     // hand-run CLI tracked in the shell
    pub program: String,
    pub cwd: String,                     // live_cwd if known, else meta.cwd
    pub status: String,                  // "running" | "dead"
    pub activity: String,                // "working" | "idle" | "dead"
    pub idle_ms: Option<u64>,            // ms since last PTY output (running only)
    pub cols: u16, pub rows: u16,
    pub hooked: bool,                    // block store epoch > 0
    pub open_block: Option<CtlOpenBlock>,
    pub last_block: Option<CtlLastBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtlOpenBlock { pub cmd: String, pub started_ms: u64 }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtlLastBlock { pub cmd: String, pub exit: Option<i64>, pub ended_ms: Option<u64> }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtlTokenInfo { pub name: String, pub token: String, pub scope: u32,
                          pub created_ms: u64 }
```

Why `activity` uses the GUI's 800ms Working threshold (`idle_ms < 800 → "working"`,
else `"idle"`; Dead → `"dead"`): one consistent definition across surfaces. Raw `idle_ms`
is exposed alongside so consumers with different thresholds compute their own. `NeedsYou`
is a GUI-side latch (bell + prompt-signature via the GUI parser) and is NOT exposed in
v1 — open question 2.

### 2.3 Version gate

- `daemon::run()` writes `proto: 3` (comment: `// 3 = Controller API (HelloCtl/Ctl), P5`).
- `gui/ipc.rs`: no behavior change; optionally extend the skew warning text. The GUI
  never sends controller frames.
- `tc` (the CLI) reads daemon.json first and refuses with a clear message when
  `proto < 3` ("daemon predates the controller API — restart it from this build"),
  because an old daemon would fail to decode `HelloCtl` and just drop the socket.

---

## 3. Security model

### 3.1 Threat model (honest, three rings)

| Adversary | Defense | Verdict |
|---|---|---|
| Remote hosts | daemon binds `127.0.0.1:0` only (existing) | blocked |
| Other local users | %LOCALAPPDATA% user-private ACL guards daemon.json + ctl-tokens.json + bootstrap; no token ⇒ Hello/HelloCtl rejected | blocked |
| Same-user processes | can read daemon.json ⇒ full rights are ALWAYS obtainable | **not a boundary** — scoped tokens are guardrails for fallible-but-cooperating agents, and that is their whole design goal |

Token comparison stays plain `==` like today's Hello: loopback-only + same-user trust
makes timing side-channels moot, and consistency beats theater.

### 3.2 Credential kinds

1. **Master token** (daemon.json, rotates every daemon start, existing): `Hello` or
   `HelloCtl` with it ⇒ `SCOPE_FULL`. FULL is required for: all legacy C2D verbs,
   `Shutdown`, and the three `Token*` requests (scoped tokens can NEVER mint tokens —
   no privilege ladder).
2. **Scoped controller tokens**: minted by `TokenCreate { name, scope }`, 32 lowercase
   hex chars (`Uuid::new_v4().simple()` ×1 — 122 bits, same entropy family as the master's
   two), persisted to `%LOCALAPPDATA%\TerminalControl\ctl-tokens.json`:

```rust
// src/daemon/ctl_tokens.rs (new)
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct TokenFile { pub tokens: Vec<CtlTokenInfo> }

pub fn path() -> PathBuf;                      // data_dir()\ctl-tokens.json
pub fn load() -> TokenFile;                    // missing/corrupt ⇒ Default (log)
pub fn save(f: &TokenFile);                    // atomic tmp+rename (SharedState::save pattern)
```

   Held in `Core.ctl_tokens: Mutex<TokenFile>` (leaf lock; loaded once in `run()`).
   `TokenCreate` upserts by name (re-creating a name rotates its token — that IS the
   rotation story), `TokenRevoke` removes by name. Existing scoped connections keep
   their scope until they disconnect (scope is resolved at handshake) — documented;
   revocation is for credentials at rest, not live-session eviction (open question 5).
   `TokenList` returns tokens in full: the file is same-user-readable anyway, and
   pretending otherwise is theater.

3. **CLI credential resolution** (`tc`): `TC_CTL_TOKEN` env var if set (this is how a
   user sandboxes an agent: set it to a read/input-scoped token in the agent's env),
   else the master token from daemon.json (full rights — the same-user default).

### 3.3 Recursion guard (controller inside a managed terminal)

- `session::spawn` adds `cmd.env("TC_SESSION_ID", id.to_string())` right after
  `CommandBuilder::new` (§8.2). Inherited by the whole tree — the shell, claude, and any
  `tc` invocation an agent makes from inside it.
- `tc` reads its own `TC_SESSION_ID` and forwards it as `HelloCtl.self_session`.
- Daemon-side (authoritative): `Run`/`SendRaw`/`SendChord`/`Kill`/`Restart`/`Delete`
  targeting `self_session` without `force_self: true` ⇒ `Err { code: "self_target" }`.
- CLI-side (ergonomic): `tc` also pre-checks and prints the same error without a round
  trip; `--force-self` sets the flag. Reads and waits on self stay allowed.
- `tc list` marks the matching terminal `"self": true` (CLI-added field) so an agent can
  identify its own host terminal in one call.

Why refuse-by-default rather than allow: input-to-self is a feedback loop (the submitted
command re-enters the agent's own transcript and can re-instruct it) and kill-self orphans
the task; both are essentially never intended.

### 3.4 Enforcement point

`ClientConn` gains an immutable `scope: u32` (set before the conn is published to
`Core.clients`) and `self_session: Option<Uuid>`. One guard at the top of
`handle_message`:

```rust
if client.scope != SCOPE_FULL {
    match &msg {
        C2D::Ping | C2D::Ctl { .. } => {}
        _ => { log::warn!("scoped controller sent a non-Ctl frame; dropped"); return; }
    }
}
```

and a pure per-verb table used by the Ctl dispatcher (unit-tested, §15):

```rust
/// The scope bits a request needs. Token* and legacy verbs need FULL and are
/// checked separately.
pub fn required_scope(req: &CtlRequest) -> u32
```

Insufficient scope ⇒ `Err { code: "forbidden", msg: "requires <scope> scope" }`.

---

## 4. Daemon: handshake + dispatch (src/daemon/mod.rs)

### 4.1 handle_client

The first-frame match becomes:

```rust
let (scope, self_session) = match read_frame::<_, C2D>(&mut reader) {
    Ok(C2D::Hello { token: t }) if t == token => (SCOPE_FULL, None),
    Ok(C2D::HelloCtl { token: t, self_session }) => {
        if t == token { (SCOPE_FULL, self_session) }
        else {
            match core.ctl_tokens.lock().tokens.iter().find(|k| k.token == t) {
                Some(k) => (k.scope, self_session),
                None => { log::warn!("controller rejected: bad token"); return; }
            }
        }
    }
    _ => { log::warn!("client rejected: bad hello"); return; }
};
```

Everything else (queue, writer thread, snapshot-on-connect) is unchanged — scoped
controllers also receive the initial `Snapshot` frame (harmless: it is state they can
List anyway, and skipping it would special-case the writer path for nothing).

Disconnect cleanup: after the read loop, additionally purge this client's waiters and
subscriptions (`core.waiters`/`core.subs` retain on `Weak::upgrade` — §6.4).

### 4.2 Ctl dispatch

```rust
// in handle_message:
C2D::HelloCtl { .. } => {} // only valid as the first frame; ignore like Hello
C2D::Ctl { req_id, req } => self.handle_ctl(client, req_id, req),
```

```rust
impl Core {
    fn handle_ctl(self: &Arc<Self>, client: &Arc<ClientConn>, req_id: u64, req: CtlRequest);
    /// Reply helper: enqueue one D2C::Ctl frame to this client only.
    fn ctl_reply(&self, client: &ClientConn, req_id: u64, body: CtlBody);
    fn ctl_err(&self, client: &ClientConn, req_id: u64, code: &str, msg: String);
}
```

`handle_ctl` first checks `required_scope`, then the recursion guard for the six
targeting verbs, then dispatches. Every arm that can fail replies `Err` — a controller
request NEVER goes unanswered (unlike legacy fire-and-forget verbs; agents need closure).
Unknown terminal id ⇒ `Err { code: "not_found" }`.

---

## 5. Verb semantics (exact)

### 5.1 List

Assemble under short, sequenced locks — state → sessions (for `idle_ms`) → blocks (leaf):

```rust
fn ctl_list(&self) -> CtlBody // Listing
```

- `folders`: `state.folders` clone (already Serialize).
- Per terminal: `kind`/`claude_session` from `TermKind` (claude ⇒ pinned id);
  `cwd = live_cwd.unwrap_or(cwd)` display string; `status` from `TermStatus`;
  `idle_ms = now_ms - session.last_output` (running only, §8.1); `activity` per §2.2;
  `cols/rows` from `last_cols/rows`; `hooked = blocks[id].epoch > 0`;
  `open_block` from the store's `open` index; `last_block` = last rec with
  `end_off.is_some()`.
- Cost: O(terminals), no journal IO, no term locks. Safe to call at any frequency.

### 5.2 CreateTerminal / CreateFolder / Kill / Restart / Delete

Thin wrappers over the existing arms — `CreateTerminal` duplicates the existing
`C2D::CreateTerminal` body but replies `Created { id }` (the legacy arm can't return the
id; the controller must have it). `Kill`/`Restart` reuse the existing session-kill /
`launch(id)` calls; `Delete` reuses the full existing deletion sequence (state-first
ordering preserved — journal-resurrection incident class). All reply `Done` (or `Err`).
No daemon-side confirmation for any of them: the CLI owns destructive-action UX
(`tc delete` requires `--yes`, §9.2) — a non-interactive daemon prompt is a
contradiction, and Kill/Restart are recoverable by design (journals persist).

### 5.3 Run — the gated submission

```rust
fn ctl_run(self: &Arc<Self>, client: &Arc<ClientConn>, req_id: u64,
           id: Uuid, cmd: String, force: bool, wait: Option<RunWait>)
```

Order of checks (each refusal is a distinct `code` so agents can branch):

1. `status == Running` else `Err "dead"`.
2. Multi-line: `cmd` containing `\n`/`\r` ⇒ `Err "multiline"` unless the CLI sanitized
   it via `--multi` (which converts to `\r`-separated, P3 paste semantics, and documents
   that `wait` resolves on the FIRST block close). Why refuse by default: on PSReadLine
   each `\r` is a separate submission — an agent that didn't know that gets one exit code
   for N commands, which is a silent lie.
3. Gate (skipped entirely when `force`):
   - `hooked` (`blocks[id].epoch > 0`) else `Err "not_hooked"` (claude/cmd/custom tabs —
     hint in msg: "use send/read --screen for TUIs").
   - `hooks_live` (§5.7) else `Err "hooks_unverified"` (bootstrap failed to run this
     spawn — the busy gate would be blind, so refuse rather than guess).
   - No open block (`store.open.is_none()`) else
     `Err { code: "busy", msg: "<cmd> running for <dur>; pass --force to type into it" }`.
   - Not alt-screen: clone the term Arc out of the sessions lock (Attach's exact
     pattern), then `serialize::is_alt_screen(&term.lock())` — true ⇒ `Err "alt_screen"`.
4. Build bytes daemon-side (unit-tested pure fn, mirrors P3 §4.1 exactly):

```rust
/// P3 submission_bytes, daemon flavor: mirror Term supplies BRACKETED_PASTE.
pub fn submission_bytes(bracketed: bool, cmd: &str) -> Vec<u8>
// trim_end; "\r\n"→"\r"; '\n'→'\r'; wrap ESC[200~…ESC[201~ iff bracketed;
// push b'\r' OUTSIDE the brackets.
```

5. Capture `at_off`: take the journal lock, read `absolute_len()`, release. Taken BEFORE
   the PTY write so the echo/exec-hook bytes (which arrive only after conhost round-trip)
   are guaranteed `>= at_off`. No journal write happens here (mirror purity: input is not
   output).
6. Write via the session writer (identical to `C2D::Input`).
7. `wait: None` ⇒ reply `RunStarted { at_off }`.
   `wait: Some(w)` ⇒ register waiter `BlockClose { after_off: at_off }` with
   `run_tail: Some(w.tail_bytes)` and deadline `now + w.timeout_ms` — the reply is
   deferred to the waiter's resolution (§6), which builds `RunDone` by reading the
   closed rec's journal range through the shared block-text helper (§5.6):
   `output` = LAST `tail_bytes` of the stripped text (tail, not head — the end of output
   is where errors live), `truncated` accordingly, `exit`/`duration_ms` from the rec.

Accepted residuals (documented, same class as P2 §4.2 / P3): text already typed-but-
unsubmitted at the prompt gets the submission appended (the controller has no
cursor-clean signal — that is GUI-feed-time state); and only hook-announced commands can
make the gate refuse. No blind clear chord is ever sent (P3's Ctrl+C-on-click is a
user-intent action; a daemon-initiated clear would be an uninvited PTY write).

### 5.4 SendRaw / SendChord

- `SendRaw`: bytes straight to the session writer. Ungated by D7's rationale. Requires
  INPUT scope + recursion guard. Reply `Done`.
- `SendChord`: encode then same path:

```rust
fn chord_bytes(chord: CtlChord, win32: bool) -> Vec<u8>
// win32 (Session.win32_input): crate::win32_input::encode_key(key, mods)
//   with the egui::Key/Modifiers mapping below;
// VT fallback: the lean table (Enter=b"\r", Esc=b"\x1b", Tab=b"\t",
//   Backspace=b"\x7f", CtrlC=b"\x03", CtrlD=b"\x04", CtrlZ=b"\x1a",
//   CtrlL=b"\x0c", arrows/nav = CSI sequences per bindings::vt_fallback's rules).
```

  Mapping: `CtrlC → (Key::C, Modifiers::CTRL)`, `Up → (Key::ArrowUp, NONE)`, etc.
  Why both paths: the keys probe proved win32-encoded Ctrl+C is what reliably interrupts
  under mode 9001; raw `0x03` is only trustworthy when 9001 is off.

### 5.5 ReadScreen / ReadTail

```rust
fn ctl_read_screen(&self, id: Uuid) -> CtlBody
```

- Sessions lock → clone term Arc → drop sessions lock → `term.lock()` → walk
  `grid()[Line(0..screen_lines)]`, collecting `cell.c` per column, skipping
  `WIDE_CHAR_SPACER` flagged cells, `trim_end()` per row (the `sync_tests::row_text`
  recipe, spacer-aware). Also capture `cursor.point` and `is_alt_screen`. Bounded: rows
  ≤ 1000 by the resize clamp. Dead session ⇒ `Err "dead"` (hint: use `tail`).
- Never resizes, never serializes VT (text out, not bytes out — no mirror interaction
  beyond a read lock).

```rust
fn ctl_read_tail(&self, id: Uuid, lines: u32) -> CtlBody
```

- `lines` clamped to 5000. Journal: fresh-handle `tail()` (2MiB cap already built in).
- Strip with `crate::strip::AnsiStripper` (one instance, streaming — it removes SGR, OSC
  bodies including the 7717 hooks, and control noise).
- Drop seam lines: any line whose trimmed text equals `serialize::SEAM_SENTINEL` or the
  legacy visible markers ("── restored ──" / "── process exited ──") — the stripper
  removes the SGR-8 conceal but not the sentinel TEXT, and leaking it would confuse
  consumers. Reuse the serializer's existing seam-text predicate if it is `pub`; else a
  3-line local check (do not re-derive the sentinel string — import the const).
- Return the last `lines` complete lines; `truncated` = tail cap hit or head cut mid-way.
- Works for dead terminals (journal persists) — this is the post-mortem read.

Why journal-tail text and not a serialized reconstruction: reads must not take term locks
for MBs of work, dead terminals have no Term, and stripped-linear-history is what
grep-minded agents actually want. TUI screens are what `ReadScreen` is for (a stripped
journal of a TUI is redraw soup — documented in the CLI help).

### 5.6 ReadBlocks / ReadBlockText

- `ReadBlocks { last }`: blocks leaf lock → clone the last `min(last, len)` recs → reply.
- `ReadBlockText`: factor the existing `C2D::BlockText` handler body into

```rust
/// Shared by C2D::BlockText, CtlRequest::ReadBlockText, and RunDone assembly.
fn block_text(&self, id: Uuid, rec: &BlockRec) -> (String, bool) // (text, truncated)
```

  (same caps: `BLOCK_TEXT_RAW_CAP`/`BLOCK_TEXT_CAP`; same fresh-handle `read_range`;
  same open-block = read-to-head rule). The legacy arm keeps replying `D2C::BlockText`;
  the Ctl arm replies `CtlBody::BlockText`. One implementation, zero drift.

### 5.7 `hooks_live` (one new BlockStore field)

```rust
// blocks.rs BlockStore: NOT persisted to the sidecar (runtime truth only).
/// Any correct-token hook event arrived this spawn — the bootstrap is alive,
/// so open-block tracking can be trusted as a busy signal.
pub hooks_live: bool,
```

- Reset to `false` in `launch()`'s rotation block (where epoch bumps).
- Set to `true` in `on_block_event` right after the token check passes (any verb —
  `Init` is the usual first).
- `Sidecar` unchanged; `BlockStore::load` starts it `false`.

Why: `hooked` proves the *intent* to hook (bootstrap written); `hooks_live` proves the
shell actually ran it — the difference is exactly the "bootstrap write succeeded but the
wrapper never executed" degraded row of P3 §9, where a busy-gate would silently pass.

---

## 6. The wait engine (new file: src/daemon/waiters.rs)

### 6.1 Types

```rust
pub const MAX_WAITERS_PER_CLIENT: usize = 16; // agents never need more; bounds memory
pub const MAX_WAITERS: usize = 256;
const OUTPUT_BUF_CAP: usize = 64 * 1024;      // per OutputMatch waiter

pub struct Waiter {
    pub client: Weak<ClientConn>,   // Weak: a dead client must not pin the conn
    pub req_id: u64,
    pub id: Uuid,                   // terminal
    pub deadline_ms: u64,           // wall clock (now_ms domain)
    pub kind: WaiterKind,
}

pub enum WaiterKind {
    BlockClose { after_off: u64, run_tail: Option<u32> }, // run_tail ⇒ reply RunDone
    Prompt,
    Exit,
    Output(OutputWaiter),
}

pub struct OutputWaiter {
    pub matcher: Matcher,               // Substring(String) | Regex(regex::Regex)
    pub stripper: crate::strip::AnsiStripper,
    pub buf: String,                    // stripped, capped, cut at line boundaries
}
```

`Core` gains:

```rust
waiters: Mutex<Vec<Waiter>>,            // LEAF lock, same doctrine as blocks
waiter_count: AtomicUsize,              // hot-path gate: ingest checks this first
subs: Mutex<Vec<Sub>>,                  // §7
sub_count: AtomicUsize,
ctl_tokens: Mutex<ctl_tokens::TokenFile>,
```

Lock discipline: `waiters`/`subs` are leaves — never taken while holding them; always
taken AFTER journal/blocks/sessions locks are released. `waiter_count` is maintained
inside the `waiters` lock (`store(len)` on every mutation) and read `Relaxed` outside.

### 6.2 Registration (in handle_ctl for `Wait` and `Run{wait}`)

- Enforce the two caps (`Err "wait_limit"`).
- **Immediate-resolution check first** (no waiter if already true):
  - `Prompt`: hooked && hooks_live && running && `open.is_none()` ⇒ reply `Waited(Prompt)`
    now. Not hooked / not live ⇒ `Err "not_hooked"` / `"hooks_unverified"` (a Prompt wait
    that can never fire must refuse, not hang to timeout).
  - `Exit`: status Dead ⇒ reply immediately (`code: None` — historical exits don't keep
    codes; documented).
  - `BlockClose { after_off }`: a rec with `start_off >= after_off && end_off.is_some()`
    already in the store ⇒ resolve now (covers replies that raced the close).
  - `OutputMatch { from_off: Some(o) }`: `read_range(o, head, 512KiB)` → strip → match
    → resolve now on hit; else fall through to live registration (the stripper + buffer
    keep their state so a pattern split across the boundary still matches).
- Push the waiter; `regex: true` compiles here — invalid pattern ⇒ `Err "bad_pattern"`.
  Why the `regex` crate (new dep): linear-time engine (no catastrophic backtracking
  inside the daemon — a hostile pattern must not wedge anything), and it is the
  ecosystem-standard, zero-unsafe default. Substring mode stays for the 90% case.

### 6.3 Resolution hook sites (all run with NO other lock held)

1. **`on_block_event`** (after the blocks lock is released, next to `notify_blocks`):
   - `Exec` outcome (a block opened): nothing resolves (BlockClose waits for close).
   - `Pre` arm — **regardless of whether a block closed** (a pre with no open block is a
     prompt render too, the first-prompt case): resolve `Prompt` waiters for this id.
   - A closed rec (from `Pre` or the exec-closes-dangling path): resolve
     `BlockClose` waiters with `rec.start_off >= after_off` → `Waited(BlockClosed{rec})`
     or, for `run_tail`, assemble `RunDone` via `block_text` (§5.6; tail-cut to
     `tail_bytes` at a char boundary) and reply.
2. **`on_exit`**: resolve `Exit` waiters (`Waited(Exited{code})`). Then resolve this id's
   `BlockClose`/`run_tail` waiters whose rec got dangling-closed by `close_dangling`
   (exit: None — the honest answer). Remaining `Prompt`/`Output` waiters for the id:
   fail with `Err "exited"` — the condition can no longer occur; hanging to timeout
   would waste the agent's time.
3. **`ingest` — the hot path** (in the reader thread, AFTER the journal lock is dropped,
   same slot as `on_journal_compact`):

```rust
if core.waiter_count.load(Ordering::Relaxed) > 0 {
    core.feed_output_waiters(id, bytes, chunk_off);
}
```

   `feed_output_waiters`: under the waiters lock, for each `Output` waiter on this id:
   `stripper.feed(bytes, &mut buf)`; test the matcher against `buf` (substring: `contains`
   on the whole buf — cheap at 64KiB; regex: `find`); on hit, record the matching line +
   `at_off ≈ chunk_off + bytes.len()` (line-resolution offsets are not exactly
   recoverable post-strip; documented as "offset of the chunk that completed the match");
   else trim `buf` to its last 8KiB at a `\n` boundary (patterns spanning >8KiB of
   stripped text are out of contract — documented). Resolved waiters are drained out of
   the lock and replied outside it.
   Cost when idle: ONE relaxed atomic load per ingest chunk. Cost when armed: stripping
   only for terminals that have an Output waiter.
4. **Timeout sweep** — extend the existing 250ms flush thread (inv. 5: no new loop):

```rust
if flush_core.waiter_count.load(Ordering::Relaxed) > 0 {
    flush_core.expire_waiters(now_ms()); // deadline hit ⇒ Err "timeout";
                                         // dead Weak ⇒ silently dropped
}
```

   250ms granularity on timeouts is documented (a `--timeout 30` resolves within 30.25s).

### 6.4 Client death

Waiter/sub replies go through `Weak::upgrade` — a gone client's entries are dropped at
the next touch (resolution attempt, sweep, or the disconnect purge in `handle_client`).
Nothing ever blocks on a dead controller.

---

## 7. Events (Subscribe / tc watch)

```rust
pub struct Sub {
    pub client: Weak<ClientConn>,
    pub req_id: u64,
    pub ids: Option<Vec<Uuid>>, // None = all terminals
    pub kinds: u32,             // EV_* bitflags
}
```

- `Subscribe` replies `Subscribed` immediately, then streams `Event` bodies with the same
  `req_id`. `Unsubscribe { req_id }` removes it (`Done`).
- Emission sites (each: `sub_count` gate → collect matching subs under the lock → enqueue
  outside it):
  - `notify_blocks`: for each rec in the frame, `end_off.is_none()` ⇒ `BlockOpened`,
    else `BlockClosed` (EV_BLOCKS). Full-sync frames (`full: true`) are NOT translated
    into events (they are attach-time state transfer, not news).
  - `on_exit`: `Exited { id, code }` (EV_EXIT), emitted next to the existing broadcast.
  - `broadcast_snapshot`: one coarse `StateChanged` (EV_STATE). Why coarse: the daemon
    already computes no diffs; "something changed, List if you care" is cheap and honest,
    and `List` is O(terminals) with no IO.
- Delivery uses the existing bounded `enqueue` — a wedged watcher goes `alive=false` and
  is pruned exactly like a wedged GUI. Events are tiny and rare (human-scale command
  cadence), so the 1024-deep queue is generous.

---

## 8. Session additions (src/daemon/session.rs)

### 8.1 `last_output`

```rust
pub struct Session {
    // …existing…
    /// Wall-clock ms of the last PTY output chunk (now_ms domain). Written by
    /// the reader thread (one Relaxed store per chunk), read by ctl_list.
    pub last_output: Arc<AtomicU64>,
}
```

Initialized to `now_ms()` at spawn; the reader loop stores before calling
`core.ingest(..)`. One relaxed atomic store per ≤64KiB chunk — unmeasurable.

### 8.2 `TC_SESSION_ID`

In `spawn`, immediately after `CommandBuilder::new(&resolved)`:

```rust
cmd.env("TC_SESSION_ID", id.to_string());
```

Inherited by the whole ConPTY tree. Also usable later by the tracker as a cheap
own-terminal marker; v1 uses it only for the recursion guard.

---

## 9. The CLI (new: src/ctl.rs + src/bin/tc.rs; edits: main.rs, Cargo.toml, install)

### 9.1 Binary layout

- **`src/bin/tc.rs`** — auto-discovered by cargo as the `tc` bin, console subsystem
  (no `windows_subsystem` attr):

```rust
//! tc — Terminal Control controller CLI (console subsystem).
//! The main exe is windows-subsystem in release: PowerShell doesn't wait for
//! it and its stdout is lost (documented ops incident) — a controller CLI
//! must be a real console program.
#[path = "../state.rs"]    mod state;
#[path = "../protocol.rs"] mod protocol;
#[path = "../strip.rs"]    mod strip;
#[path = "../ctl.rs"]      mod ctl;
fn main() { std::process::exit(ctl::run(std::env::args().skip(1).collect())); }
```

  This compiles because the dependency closure is exactly `protocol → state` (+ std/serde/
  bincode/uuid/dirs/log) and `strip` is std-only — no daemon/gui/egui code is pulled in.
  `crate::state` inside protocol.rs resolves to this bin crate's own `mod state`.
- **`src/ctl.rs`** — all logic, compiled into BOTH binaries. `main.rs` adds `mod ctl;`
  and routes `Some("ctl") => std::process::exit(ctl::run(args[2..].to_vec()))` so
  `terminal-control ctl …` also works (fine in debug builds and when output is
  redirected; `tc` is the documented interface).
- **Arg parsing: hand-rolled** (verb + flags loop), matching the `--probe` house style.
  One-line justification: the grammar is a dozen flags; clap would be the crate's first
  arg-parsing dependency for zero expressive gain.
- **`--install`**: after copying the main exe, also copy `tc.exe` (sibling of
  `current_exe()`) to `bin\tc.exe`; missing sibling ⇒ warn and continue (a
  single-bin `cargo run` build). No PATH edits in v1 (open question 3) — agents use the
  full path, which `tc`'s own docs snippet prints.

### 9.2 Verbs (exact grammar)

```
tc list                                        [--folder <name>] [--all-fields]
tc create   --name <s> [--folder <name>] [--cwd <dir>]
            [--kind shell|claude|custom] [--program <exe>] [--arg <a>]...
            [--claude-session <uuid>]          → {"id":…}
tc run      <term> <command…>  [--force] [--force-self] [--multi]
            [--no-wait] [--timeout <secs=60>] [--tail <bytes=8192>]
tc send     <term> (--text <s> [--enter] | --b64 <base64> | --key <chord>)
            [--force-self]
tc read     <term> [--screen (default) | --tail [--lines <n=100>]]
tc blocks   <term> [--last <n=20>]
tc block-text <term> <start_off>
tc wait     <term> --for (block-close [--after <off>] | prompt | exit
            | output <pattern> [--regex] [--from <off>]) [--timeout <secs=30>]
tc kill     <term> [--force-self]
tc restart  <term> [--force-self]
tc delete   <term> --yes [--force-self]
tc watch    [--id <term>]... [--events blocks,exit,state]
tc token    (create --name <s> --scope read|input|manage | revoke --name <s> | list)
tc info                                        → daemon pid/port/proto + self id
```

- `<term>` = UUID, or exact name, or unambiguous case-insensitive name prefix. Resolution
  is CLI-side via `List` (daemon stays id-only — one authority for identity). Ambiguity ⇒
  `Err "ambiguous"` listing candidates; miss ⇒ `"not_found"`. Exit code 4 for both.
- `run` defaults to `--wait`-style composite (timeout 60s) because that is the agent's
  90% case; `--no-wait` returns `RunStarted` for fire-and-forget.
- `send --key` chords: `enter esc tab backspace up down left right home end pgup pgdn
  ctrl+c ctrl+d ctrl+z ctrl+l` → `CtlChord`. `--text --enter` appends an `Enter` chord
  after the text (two writes, ordered on one connection).
- `delete` refuses without `--yes` (`code: "confirm"`, exit 2) — the one irrecoverable
  verb (journal + sidecar removed).
- `watch` prints one JSON object per line until killed; it is the only long-lived mode.

### 9.3 JSON contract (stdout, always; one object per invocation, JSON-lines for watch)

Envelope: `{"v":1,"ok":true,…}` / `{"v":1,"ok":false,"code":"…","msg":"…"}`. The `v`
field is the CLI contract version — bumped only on breaking shape changes, independent of
`DaemonInfo.proto`. The CLI maps `CtlBody` to these shapes (dedicated structs, not
blind serde of internals — §2.2's decoupling rationale):

```json
tc list →
{"v":1,"ok":true,
 "folders":[{"id":"7f…","name":"Work"}],
 "terminals":[{"id":"3fa8…","name":"api server","folder":"7f…","kind":"shell",
   "claude_session":null,"inner_cli":null,"program":"powershell.exe",
   "cwd":"C:\\repo","status":"running","activity":"idle","idle_ms":184223,
   "cols":142,"rows":38,"hooked":true,"self":false,
   "open_block":null,
   "last_block":{"cmd":"cargo test","exit":0,"ended_ms":1751443200123}}]}

tc run 3fa8 "git status" →
{"v":1,"ok":true,"exit":0,"duration_ms":312,"start_off":48211,
 "output":"On branch main\nnothing to commit, working tree clean","truncated":false}

tc run 3fa8 "cargo bench" (while ping -t runs) →
{"v":1,"ok":false,"code":"busy",
 "msg":"ping -t 127.0.0.1 running for 42s; pass --force to type into it",
 "open_cmd":"ping -t 127.0.0.1"}            (exit code 2)

tc read 3fa8 --screen →
{"v":1,"ok":true,"alt_screen":false,"cursor":{"row":37,"col":13},
 "lines":["PS C:\\repo> ", "…"]}

tc wait 3fa8 --for output "Compiling" →
{"v":1,"ok":true,"hit":"output","line":"   Compiling serde v1.0.219","at_off":52100}

tc watch (stream) →
{"v":1,"event":"block_closed","id":"3fa8…","cmd":"cargo test","exit":0,
 "duration_ms":8123,"start_off":48211}
{"v":1,"event":"exited","id":"9b2c…","code":1}
{"v":1,"event":"state_changed"}
```

### 9.4 Exit codes + error code table

| exit | meaning | `code` values |
|---|---|---|
| 0 | success | — |
| 1 | transport/internal (daemon unreachable, proto<3, bad args) | `no_daemon`, `proto_skew`, `usage`, `internal` |
| 2 | refused by policy/gate | `busy`, `alt_screen`, `not_hooked`, `hooks_unverified`, `self_target`, `forbidden`, `confirm`, `multiline`, `dead`, `wait_limit`, `bad_pattern` |
| 3 | timeout | `timeout` |
| 4 | target resolution | `not_found`, `ambiguous` |

Distinct 2/3/4 let an agent branch (`retry-after-wait` vs `wrong target` vs `give up`)
without JSON parsing.

### 9.5 CLI internals (signatures)

```rust
// src/ctl.rs
pub fn run(args: Vec<String>) -> i32;
fn connect() -> Result<CtlConn, CliErr>;        // daemon.json → proto check →
                                                // TcpStream → HelloCtl{token, env self}
struct CtlConn { stream: TcpStream, write: TcpStream, next_req: u64 }
impl CtlConn {
    fn call(&mut self, req: CtlRequest) -> Result<CtlBody, CliErr>;  // send, read
        // frames until D2C::Ctl with our req_id (ignore Snapshot/others)
    fn call_deadline(&mut self, req: CtlRequest, extra: Duration) -> …; // for waits:
        // socket read timeout = request timeout + 5s slack
}
fn resolve_term(conn: &mut CtlConn, arg: &str) -> Result<Uuid, CliErr>; // §9.2 rules
fn parse_chord(s: &str) -> Option<CtlChord>;
fn print_json(…); // envelope assembly; serde_json to stdout, \n-terminated
```

The connection sets the same nodelay/keepalive as the probe's `Conn`. `watch` clears the
read timeout and loops printing events until EOF/Ctrl+C.

---

## 10. Claude Code ergonomics (the acceptance scenarios)

The four canonical agent flows — these must work verbatim on a real setup:

```bash
# 1. Orient: what sessions exist, what's busy, what needs attention?
tc list
# → agent picks id by "name" / "activity" / "last_block.exit"

# 2. Run a command in the build shell and get its result in ONE call:
tc run "build shell" "cargo test -q" --timeout 300
# → {"ok":true,"exit":0,"duration_ms":81234,"output":"…test result: ok…"}
# busy? → exit code 2, {"code":"busy","open_cmd":"…"} → agent waits:
tc wait "build shell" --for prompt --timeout 600   # then retries the run

# 3. Drive a claude session (TUI — run is refused by design; go raw + screen):
tc read "research claude" --screen          # what is it showing?
tc send "research claude" --text "summarize the findings" --enter
tc wait "research claude" --for output "●" --timeout 120   # activity marker
tc read "research claude" --screen

# 4. Babysit long jobs from a watcher loop:
tc watch --events blocks,exit
# stream: {"event":"block_closed","cmd":"cargo bench","exit":1,…} → investigate:
tc blocks "build shell" --last 3
tc block-text "build shell" 48211
```

And the guardrail story: the user mints `tc token create --name agents --scope input`
once, sets `TC_CTL_TOKEN` in the agents' env, and no agent can ever delete a terminal or
shut the daemon down — while `run`'s busy gate keeps them from typing into a running TUI
even with input scope.

---

## 11. Performance budget (explicit)

| Cost | When | Bound |
|---|---|---|
| Idle controller connection | always | 1 parked reader thread + 1 parked writer thread; NOT in any fanout filter (never attached) |
| `ingest` overhead, no waiters | every output chunk | 1 relaxed atomic load |
| `ingest` overhead, waiters armed | chunks of terminals WITH an Output waiter | strip of that chunk + capped-buffer match |
| block/exit event emission | per block open/close/exit | 1 relaxed load; sub walk only when sub_count > 0 |
| Timeout sweep | existing 250ms flush tick | skipped at count==0; O(waiters) otherwise |
| List | per request | O(terminals), no IO, no term locks |
| ReadScreen | per request | one term lock, ≤1000-row walk |
| ReadTail / block-text | per request | fresh-handle file read ≤2MiB/4MiB, stripped |
| Waiter memory | per waiter | ≤64KiB buf, ≤16/client, ≤256 total |

No daemon-side polling, no new threads, no repaint-class loops. The GUI is untouched.

---

## 12. Degraded modes — the honest contract

| Situation | list | read screen/tail | run | send/chord | wait prompt/block | wait output/exit | events |
|---|---|---|---|---|---|---|---|
| Hooked pwsh, idle prompt | full | yes | yes | yes | yes | yes | yes |
| Hooked, command/TUI running (open block) | full (`open_block` set) | yes | refused `busy` (`--force` overrides) | yes | waits | yes | yes |
| Hooked, alt-screen (vim) | full | screen = alt grid | refused `alt_screen` | yes | waits | yes | yes |
| Hookless (claude tab, cmd, custom) | full (`hooked:false`) | yes | refused `not_hooked` | yes | refused `not_hooked` | yes | exit/state only |
| Bootstrap written but never ran | full | yes | refused `hooks_unverified` | yes | refused `hooks_unverified` | yes | exit/state only |
| Dead terminal | full (`status:"dead"`) | tail yes, screen refused `dead` | refused `dead` | refused `dead` | refused/immediate (`exit` resolves now) | exit immediate | state |
| Controller's own terminal (`self_session`) | full (`self:true`) | yes | refused `self_target` unless `--force-self` | same | yes | yes | yes |
| Scoped read token | full | yes | `forbidden` | `forbidden` | yes | yes | yes |
| Scoped input token | full | yes | yes | yes | yes | yes | yes |
| proto<3 daemon | `tc` refuses upfront (`proto_skew`) — nothing half-works | — | — | — | — | — | — |
| Old GUI ↔ new daemon | unaffected (append-only) | — | — | — | — | — | — |

---

## 13. Wire-compat matrix

| Client | Daemon | Result |
|---|---|---|
| GUI (proto≤2 build) | P5 daemon (3) | works — appended variants are never sent to it, `DaemonInfo.proto` is serde-defaulted |
| P5 GUI | old daemon (2) | works — GUI sends nothing new; existing skew warning covers BlockText |
| `tc` | old daemon (≤2) | clean refusal BEFORE connecting (daemon.json proto check); never sends an undecodable frame |
| `tc` | P5 daemon | full |
| probe (this build) | P5 daemon | full (probe gains ctl cases) |

---

## 14. Probes (src/probe.rs — extend the suite; all headless, no GUI attached)

Probe `Conn` gains `open_ctl(token: &str, self_session: Option<Uuid>) -> Conn` (HelloCtl
handshake) and

```rust
fn ctl(&mut self, req_id: u64, req: CtlRequest) -> anyhow::Result<CtlBody>
// send C2D::Ctl; read frames until D2C::Ctl{req_id} (skipping Snapshot etc.)
```

### 14.1 `ctl_scope` — token minting + enforcement + recursion guard

1. Master conn: `TokenCreate { name:"probe_ro", scope: SCOPE_READ }` → `Token`; same for
   `probe_in` (READ|INPUT). Assert `ctl-tokens.json` exists and contains both names.
2. Read-scoped conn: `List` OK; `Run`/`SendRaw`/`Kill` each → `Err "forbidden"`;
   `TokenCreate` → `Err "forbidden"` (no privilege ladder).
3. Read-scoped conn sends a LEGACY frame `C2D::Input { … b"echo LEAK\r" }`, then via the
   master conn `ReadTail` after a 2s bound: assert no `LEAK` line (scoped legacy frames
   are dropped) and the daemon still answers Ping (guard didn't break the loop).
4. Recursion guard: `open_ctl(master_token, self_session=Some(id))` for a probe terminal
   `id`; `Run { id, .. }` → `Err "self_target"`; with `force_self: true` → succeeds
   (assert via a marker block). `Kill` → `"self_target"`; a DIFFERENT id kills fine.
5. `TokenRevoke` both; `TokenList` empty of probe names.

### 14.2 `ctl_run_wait` — the composite round trip (the money case)

1. Hooked probe terminal. `Run { cmd: "echo CTL_RUN_OK_77", force:false, wait: Some(
   {timeout_ms: 20_000, tail_bytes: 8192}) }`.
2. Assert ONE `RunDone`: `exit == Some(0)`, `output` contains `CTL_RUN_OK_77`, contains
   NO `0x1b`/`0x07` and no `7717` (stripped), no `PS ` prompt after the marker line
   (block-range exactness), `duration_ms < 20_000`, `start_off >= at_off` implied by
   construction.
3. `Wait { cond: Prompt, timeout_ms: 5000 }` → resolves (immediately or on next pre).
4. `Wait { cond: OutputMatch { pattern:"CTL_WM_9", regex:false, from_off:None }, … }`
   registered, THEN `Run { "echo CTL_WM_9", wait:None }` → `Waited(Output)` with the line.
5. `from_off` leg: capture `at_off` from a `RunStarted`, run the echo, sleep 500ms, then
   `Wait{OutputMatch{from_off: Some(at_off)}}` → resolves from journal history (race
   closed).
6. Timeout leg: `Wait { OutputMatch "NEVER_MATCHES_42", timeout_ms: 1200 }` → `Err
   "timeout"` within 1.2–2.0s (250ms sweep granularity).
7. `Run` with embedded `\n` → `Err "multiline"`.

### 14.3 `ctl_busy_gate` — refuse-when-busy through a real shell

1. Hooked terminal; `Run { "ping -t 127.0.0.1", wait: None }` → `RunStarted`; await the
   OPEN block (`await_blocks`).
2. `Run { "echo NOPE" }` → `Err "busy"` and the msg contains `ping`.
3. `SendChord { CtrlC }` (daemon picks win32 vs VT itself — this ALSO regression-tests
   chord encoding against mode 9001) → block closes (`await_blocks` end_off set).
4. Same `Run` again → succeeds with `RunDone exit Some(0)`.
5. Unhooked leg: `Custom cmd.exe` terminal → `Run` → `Err "not_hooked"`;
   `SendRaw b"echo RAW_OK_5\r"` → `ReadTail` shows `RAW_OK_5` (raw path ungated).

### 14.4 `ctl_read` — read surfaces

1. Hooked terminal, run `echo CTL_READ_A` + `cmd /c exit 3` (via Run).
2. `ReadTail { lines: 100 }`: contains `CTL_READ_A`; no ESC/BEL/`7717`; no
   `SEAM_SENTINEL` text.
3. `ReadScreen`: some line starts with `PS ` (prompt visible), `alt_screen == false`,
   cursor row within `rows`.
4. `ReadBlocks { last: 10 }`: both recs present, exit `Some(0)` / `Some(3)`.
5. `ReadBlockText` for the echo block == the `C2D::BlockText` reply for the same rec
   (shared-helper equivalence).
6. `Subscribe { kinds: EV_BLOCKS|EV_EXIT }` on a second conn; run one command; assert
   `BlockOpened` then `BlockClosed` events arrive with the right cmd; `KillTerminal` →
   `Exited` event.

Register in `CASES` after the blocks cases; suite grows by 4 (current 19 + P2's 4 → 27
with these; exact count depends on merge order — the names are the contract, not the
number).

---

## 15. Unit tests (cargo test)

- `required_scope_table` (protocol.rs or ctl_tokens.rs): every CtlRequest variant maps to
  its scope; Token* + legacy need FULL.
- `submission_bytes_matrix` (daemon): bracketed × plain × CRLF/`\n` sanitize × trailing
  newline trim × unicode passthrough (shared vectors with P3's test if it landed).
- `chord_bytes_both_modes`: CtrlC/Enter/Up under win32=true (matches
  `win32_input::encode_key`) and false (VT bytes exact).
- `waiters_output_chunk_invariance`: feed a stripped-match stream at chunk sizes 1/7/64
  through `OutputWaiter` — identical hit/no-hit and buffer state (ModeScanner ethos);
  pattern split across a chunk boundary still matches; 8KiB trim keeps line boundaries.
- `waiters_block_close_after_off`: recs at offsets 10/90 — `after_off: 50` resolves on
  the 90-block only; dangling-close (end via `close_dangling`) resolves with exit None.
- `hooks_live_lifecycle` (blocks.rs): load ⇒ false; token-checked event ⇒ true; launch
  rotation ⇒ false again; wrong-token event does NOT set it.
- `ctl_tokens_roundtrip`: save/load atomic; upsert-by-name rotates the token; revoke
  removes.
- `ctl_arg_parser`: verb table — `run` flags, `--key` chords, `--for` grammar, `--yes`
  requirement, unknown flag ⇒ usage error (exit 1).
- `resolve_term_rules`: uuid > exact name > unique prefix; ambiguity error carries
  candidates.
- `json_shapes`: serialize one `CtlTerm`/`RunDone`/event line and assert the exact key
  set (the frozen `v:1` contract).

---

## 16. Docs snippet (paste as docs/controller-api.md — the user-facing card)

```markdown
# Driving Terminal Control from scripts and agents (tc.exe)

`%LOCALAPPDATA%\TerminalControl\bin\tc.exe` controls the daemon. Output is JSON.

    tc list                                  # sessions, folders, activity
    tc run <name|id> "cargo test"            # runs at the prompt, returns
                                             # {"exit":0,"output":…} when done
    tc send <name|id> --text "hi" --enter    # raw input (TUIs: claude, ssh…)
    tc send <name|id> --key ctrl+c           # interrupt
    tc read <name|id> --screen               # what the terminal shows now
    tc read <name|id> --tail --lines 200     # recent output, ANSI-stripped
    tc blocks <name|id>                      # recent commands + exit codes
    tc wait <name|id> --for prompt           # block until the shell is idle
    tc watch                                 # live JSON events (blocks, exits)

`run` refuses while something is running (exit code 2, `"code":"busy"`) —
pass `--force` only if you really mean to type into a running program. From
inside a managed terminal, `tc` refuses to send input to itself
(`"self_target"`) unless you pass `--force-self`.

Scoped credentials for agents (optional): `tc token create --name agents
--scope input`, then set `TC_CTL_TOKEN` in the agent's environment — it can
then read and type, but never kill/delete terminals or stop the daemon.
Anything on this machine running as you can read the master token; scopes are
a seatbelt, not a lock.
```

---

## 17. Interactive checklist (P5 has no GUI surface — verify non-interference + live UX)

1. `cargo build --release` produces BOTH `terminal-control.exe` and `tc.exe`;
   `--install` copies both; `tc info` prints pid/port/proto=3 from PowerShell with
   output visible and the prompt WAITING (the whole D3 point).
2. With the user's real GUI open and ~20 sessions live: `tc list` completes instantly
   and matches the sidebar (names, running/dead, working dots ↔ `activity`).
3. `tc run` on a scratch hooked terminal: the command visibly types + runs in the GUI,
   a P2 block appears, and `RunDone.exit` matches the chip. GUI typing latency
   unaffected during a `tc watch` session (fanout untouched — no controller attach).
4. `tc read --screen` on a live claude tab returns the visible TUI text.
5. `tc send --key ctrl+c` interrupts a `ping -t` started from the GUI keyboard.
6. Kill the daemon mid-`tc wait` — the CLI errors out (exit 1) rather than hanging.
7. Probe suite green END-TO-END against an **installed** daemon (Start-Process -Wait
   pattern; the CREATE_NEW_PROCESS_GROUP incident says installed-vs-direct daemons can
   behave differently).
8. No GUI screenshots required (no GUI changes) — but confirm the GUI still connects,
   attaches, and shows blocks after the proto bump (skew path exercised both directions).

---

## 18. Open questions — each with the default the implementer should take

1. **`regex` crate dependency**: default YES (linear-time engine, daemon-safe). If deps
   are vetoed, ship substring-only and reject `regex: true` with `bad_pattern`.
2. **NeedsYou / bell as a controller event**: default DEFER — the GUI's latch is
   parser-event + prompt-signature based (GUI-side); daemon-side would need mirror bell
   plumbing. `idle_ms` + `open_block` cover the "stuck?" question for v1.
3. **Add `bin\` to user PATH at --install**: default NO (silently editing PATH is
   invasive); print the full path in `--install` output instead. Revisit on user ask.
4. **Live-session eviction on TokenRevoke**: default NO (scope resolved at handshake;
   revocation guards credentials at rest). One-line future fix: stamp conns with the
   token name and sweep on revoke.
5. **`Wait { Exit }` on an already-dead terminal returns `code: None`**: default accept —
   historical exit codes aren't persisted; persisting them is a state.json field for
   another phase.
6. **`run --multi` waiting for the LAST block instead of the first**: default FIRST +
   document (blank-line skipping makes "how many blocks will N lines make" unknowable);
   agents should issue one command per run.
7. **Structured `List` filtering daemon-side (`--folder`)**: default CLI-side filter
   (List is already O(terminals) and tiny); daemon filtering is an optimization nobody
   needs at 20 sessions.
8. **Should `Subscribe` replay current open blocks on start**: default NO — `watch`
   consumers who need current state call `List`/`ReadBlocks` first (documented in help).
9. **`tc` pretty/table output for humans**: default JSON-only in v1 (`| ConvertFrom-Json`
   exists); a `--pretty` formatter is cosmetic and can land any time.

---

## 19. Explicit DO-NOTs (each traces to an invariant or past incident)

- Do NOT let a controller connection Attach or receive Output/Replay fanout — the
  per-client flood cost is measured at ~+3.5–4s CPU per 50MB (fanout incident class);
  reads are pull, events are rare frames only (inv. 4).
- Do NOT write ANY controller byte into a mirror Term, a journal, or `emit_output` —
  controller input goes through the session writer exactly like `C2D::Input`, nothing
  else (mirror purity; the coordinate-divergence bug class).
- Do NOT touch `Core::ingest`'s lock span — waiter feeding runs after the journal lock is
  released, `waiters`/`subs`/`ctl_tokens` are LEAF locks like `blocks` (ingest atomicity;
  deadlock discipline).
- Do NOT add fields to existing protocol variants or reorder ANY enum — including the new
  `CtlRequest`/`CtlBody`/`CtlEvent`/`WaitCond`/`CtlChord`, which are wire-positional
  forever (bincode append-only).
- Do NOT resize a session from any read path — `Attach`'s resize-to-client is a GUI
  behavior; a controller read reflowing the user's grid is a resize-storm-class incident.
- Do NOT seek or reuse the journal append handle for reads — fresh `File` per read, the
  `read_range`/`tail` pattern (append-handle corruption under concurrent writes).
- Do NOT block the client reader thread in `handle_message` on any wait — waiter
  registry only (a blocked reader wedges that client's own Shutdown/Detach frames).
- Do NOT add polling threads — timeout sweep rides the existing 250ms flush tick, gated
  to zero cost at waiter_count==0 (inv. 5).
- Do NOT send a clear chord (or any un-asked-for byte) before a Run — P3's Ctrl+C is
  click-gated user intent; the daemon has no cursor-clean signal and must not guess.
- Do NOT bracket-paste unconditionally — `TermMode::BRACKETED_PASTE` from the mirror
  decides (PSReadLine 2.0 renders literal `ESC[200~` as input garbage).
- Do NOT let scoped tokens mint tokens or send legacy C2D frames (no privilege ladder;
  the one-guard enforcement point stays airtight).
- Do NOT log token values (daemon.log is user-readable but logs get pasted into issues).
- Do NOT give `tc.exe` a `windows_subsystem` attribute or route agents through the main
  exe for ctl — lost-stdout/no-wait incident (D3).
- Do NOT ship `tc run` waiting on anything other than block records — screen-scrape
  completion detection is the drift-class hack the whole blocks architecture exists to
  avoid.

---

## 20. Suggested implementation order (compile-green at each step)

1. **protocol.rs**: `HelloCtl`/`Ctl` variants + all `Ctl*` types + scope consts +
   `required_scope` + unit tests. `proto` stays 2 until step 4 (nothing dispatches yet).
2. **ctl_tokens.rs** (load/save/atomic) + `hooks_live` in blocks.rs + `Session.last_output`
   + `TC_SESSION_ID` env + unit tests. All inert.
3. **daemon dispatch**: `ClientConn.scope`, handshake match, the scope guard,
   `handle_ctl` with the SYNCHRONOUS verbs (List/Create/Kill/Restart/Delete/Reads/
   SendRaw/SendChord/Token*), `block_text` helper factored. Probe `ctl_scope` +
   `ctl_read` written and green here.
4. **waiters.rs** + hook sites in `on_block_event`/`on_exit`/ingest/flush-tick +
   `Run` (gate + submission + composite wait) + `Subscribe`/events. Bump `proto: 3`.
   Probes `ctl_run_wait` + `ctl_busy_gate` green.
5. **ctl.rs + src/bin/tc.rs** + main.rs route + `--install` copies tc.exe + arg-parser
   and JSON unit tests.
6. Docs snippet, interactive checklist against the live installed daemon last.
