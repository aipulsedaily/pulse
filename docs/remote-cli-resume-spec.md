# Remote CLI Resume over SSH — Implementation Spec (final, implementation-ready)

Target: C:\Terminal Control (Rust daemon + egui 0.35 GUI + tc.exe, **proto 10** at
research time). Feature: a bare CLI launched INSIDE an ssh terminal (`ssh host`, `cd
somewhere`, `claude`, work for an hour) becomes RESUMABLE across kill / sleep / reboot /
link-death: the daemon correlates the remote session id by **sftp probes of the remote
CLI store**, and every existing restore spelling (boot restore, GUI Restore, wake,
auto-reconnect, tc restart) then re-runs `cd '<cwd>' && <cli> --resume <id>` through the
UNCHANGED §7.4 wrapper machinery (`ssh_restore_trailing` + one-shot remote rc).

Everything below was verified on this machine (2026-07-04, Windows 11 26200,
OpenSSH_for_Windows_9.5p2) against a REAL Linux sftp-server staged in WSL via
`sftp.exe -D` (the validated ssh-drop stand-in) with a fabricated remote `~/.claude` /
`~/.codex` / `~/.copilot` layout, plus source-level reads of tracker.rs, bootstrap.rs,
session.rs, daemon/mod.rs, gui/ssh_drop.rs. Captured outputs in §12.

Ordered: invariants → decisions → adapter/store table → probe moments → transport
sharing → correlation rules (incl. the skew-immune design) → restore wiring → files →
probes/tests → edges → open questions → DO-NOTs → evidence log.

---

## 0. Non-negotiable invariants

1. **Probes are READ-ONLY on the remote**: `pwd` + `ls` only. No mkdir, no put, no rm,
   no writes of any kind (ssh-drop's `~/.tc-drops` append-exception does NOT apply here
   — there is no exception).
2. **Never guess** (the confidence doctrine): a resume fires only for Explicit or
   Correlated identities. "Correlated" = the §5 rules exactly; everything else stays
   Ambiguous → shell restored + preface info line (now with candidates, §6.4). The ONE
   deliberate extension — R-NEWEST, §5.3 — is precedent-matched to the local
   mtime-newest rule `claude_extract` has always shipped (tracker.rs:549-557) and is
   strictly narrower (provable in-window birth).
3. **Zero wire changes**: proto stays 10. No protocol.rs variants, no state.rs
   persisted-field changes, no GUI changes. The correlated id flows through the
   EXISTING persisted `TerminalMeta.inner_cli` and the existing Snapshot broadcast.
4. **Event-driven only, bounded, silent**: probes fire on block-open / restore-class
   launch / auto-reconnect attempt — never on a timer, never per prompt. Every probe is
   deadline-bounded (§4.5). A failed probe degrades to today's behavior (Ambiguous)
   with a daemon.log line, never a toast, never a GUI state.
5. **Non-interactive always**: `-o BatchMode=yes` prepended (first-occurrence-wins). A
   password-auth host fails fast once, then probes are skipped for that terminal until
   a later spawn proves non-interactive auth (§4.6).
6. **Never touch ssh trust or config**: no known_hosts writes, no accept-new, no
   SendEnv/SetEnv. The user's flags (`-i`, `-F`, `-o`, port) carry through the existing
   `sftp_args` translation so identity/config resolution matches the session.
7. **Input never wakes / probes never spawn**: a probe is bookkeeping. Terminal
   lifecycle stays exactly where it is (launch(), sleep gates, reconnect supervision).
8. **Mirror purity / journals / blocks: untouched.** Probe results reach the user only
   via (a) the resume command in the fresh rc (a real journaled exec) and (b) preface
   info lines via the existing `push_info_line`.
9. **The daemon runs the probes** (it owns tracker/inner_cli/restore). Verified: auth
   on this box is a plain `~/.ssh/id_rsa` read directly by sftp — works from any
   process running as the user, boot-daemon included; the Windows ssh-agent service is
   a global named pipe (`\\.\pipe\openssh-ssh-agent`) when enabled, equally
   session-independent (on this machine the service is Stopped/Disabled — the key file
   IS the auth path). Residual: real key-auth from a boot daemon against a real host is
   expected-good but unverified until first field use (the author's test host is
   off-limits to staging by rule).

---

## 1. Headline decisions

| # | Decision | One-line justification |
|---|---|---|
| D1 | **Correlation = cross-terminal BIRTH-INTERVAL attribution, not clock math**: every ssh CLI block open probes the store and persists a snapshot; at restore ALL sibling snapshots of the same (destination, cwd) store + the restore listing form an event-ordered observation timeline; each file is bracketed to the interval between the last snapshot LACKING it and the first CONTAINING it (pure set membership), then attributed to the block whose open STARTS that interval (§5) | zero clock use anywhere — remote-vs-local skew and sftp's date-form instability (§12.3: a fresh file renders in YEAR form the moment remote mtime is ≥1s ahead of local now) are structurally irrelevant; birth-interval-per-block SEPARATES staggered starts where a whole-block-duration match would tie (the two-parallel-claudes acceptance test, §9.2a) |
| D1a | **Snapshots are cross-read**: the correlate leg reads EVERY terminal's `probes\<id>.json` for the same store, not just its own | a sibling's block-open snapshot is a timestamped witness of when THIS terminal's file already existed — the bracket that pins birth to the right block (§5.2) |
| D2 | **Two probe connections per CLI lifecycle max**: one at bare-CLI block open (snapshot), one at the next restore-class launch (correlate) | block close needs NO probe — inner_cli clears there by design (CLI exited ⇒ nothing to resume); the hour-long-claude case is exactly "open block at death" and sftp is a fresh connection that works after the ssh link is gone |
| D3 | Snapshot persisted as a sidecar `probes\<terminal-id>.json` | must survive daemon restarts (power loss mid-claude); small (one dir listing); same atomic tmp+rename discipline as blocks sidecars |
| D4 | Transport = the SHIPPED ssh-drop machinery, hoisted to a shared egui-free module `src/ssh_transport.rs` | `sftp_args` flag translation, `parse_pwd`/`parse_ls_l`, `classify_conn`, `resolve_sftp` are pure and already golden-tested; GUI and daemon are the SAME exe/crate — this is a file move, not a port |
| D5 | Store paths are HOME-RELATIVE in batches (`.claude/projects/<munged>`) | sftp sessions start in the remote home by protocol — no need to learn `$HOME` first; `pwd` stays in the batch only as a connection sanity anchor |
| D6 | The munge is `state::claude_project_dir_name` verbatim | its non-alnum→`-` rule matches Claude Code's own on both OSes (verified against the author's test `~/.claude/projects`: `C--Terminal-Control` etc.; Linux `/home/alice/proj` → `-home-alice-proj`) |
| D7 | v1 correlatable adapter = **claude**; codex + copilot ship as registry entries with `remote: false` (flip after one real-host verification each); qwen deferred; goose/opencode/crush are argv-only forever | claude's remote store is field-proven (the author's remote resumes worked in June when env was clean); codex/copilot paths are upstream-documented and staging-parse-verified but no real Linux CLI run was observed; qwen needs an unbounded per-project fan-out (§2) |
| D8 | Attribution is a FIXPOINT: a block whose opening-interval has exactly ONE unclaimed birth resolves (Correlated) and removes that file; repeat; a block left with ≥2 unclaimed births in its interval Ambiguous — EXCEPT claude R-NEWEST (§5.4): if it is the SOLE remaining unresolved claimant, its interval's rotation chain collapses to the `ls -lt`-newest (the /clear case) | staggered starts resolve 1:1 (§9.2a); /clear rotation under one uninterrupted block is the sole-claimant ≥2 case; two idle claudes whose first messages land in the same interval stay Ambiguous — never swap conversations |
| D9 | Restore-time probe runs INSIDE the launch path, before the §7.4 trailing is built; conn-thread entry points get a spawned `probe-launch-<id>` thread, boot lanes call it inline | a probe on a client-conn handler thread would stall that GUI's other traffic; boot lanes are already worker threads and the stagger absorbs 0.5-2s |
| D10 | Ambiguous-after-probe with candidates ⇒ preface info line lists up to 5 ids NEWEST-FIRST | the user can `claude --resume <id>` in one paste; newest-first is the best-guess ordering without claiming certainty |
| D11 | Sanitize remote verdicts: for Ssh (and WslShell) families, `analyze_cmdline` results must NEVER carry a token minted by a LOCAL-filesystem correlation branch | `claude_extract` consults the LOCAL `~/.claude/projects/<munge(posix-cwd)>` — a theoretical local dir named like a munged remote cwd would mint a WRONG id with Correlated confidence; cheap guard, closes it for WSL §7.3 too |
| D12 | Staging knob `TC_SSH_PROBE_TRANSPORT=<sftp -D command>` (data_dir_overridden-gated, TC_SSH_VIA_WSL class) | the whole probe pipeline runs against the WSL sftp-server stand-in with zero network; permanent env-gated infra STAYS per the knob rule |
| D13 | No new GUI toggle; probes are gated on `remote_hooks` (already a per-terminal opt-out) | no hooks ⇒ no exec events ⇒ no bare-CLI detection ⇒ nothing to probe; remote_hooks is already the "TC may act on this remote" consent bit (§11 Q2 records the call) |
| D14 | The claude wake-time re-pin belt gets a REMOTE analog on the same probe data (§6.5) | an Explicit `--resume <id>` whose run then `/clear`-rotated is stale at restore exactly like the local pinned case; the snapshot-diff already carries the evidence |

---

## 2. Adapter × store table (the complete map)

"Local correlation" = what the shipped extract fn reads (tracker.rs). "Remote
equivalent" = the same store on the ssh host, home-relative for sftp. "Remote
correlatable" = a per-run filesystem delta exists that snapshot-diff can see.

| Adapter | Explicit argv (already works over ssh via `analyze_cmdline` — DONE) | Local correlation store | Remote store (home-relative) | Session id ⇐ | Remote correlatable? | v1 |
|---|---|---|---|---|---|---|
| **claude** | `--resume <uuid>` / `--session-id <uuid>` (+ `=` forms) | `~/.claude/projects/<munged-cwd>/<uuid>.jsonl`; birth-within-30s of proc start, else mtime-newest w/ 5s gap | `.claude/projects/<munge(remote-posix-cwd)>/<uuid>.jsonl` — per-cwd scoped, one `ls -lt` | filename stem | **YES** — new/grown `.jsonl` names; `--continue` shows as a GROWN file (correct: resume that id) | **ON** |
| **codex** | `codex resume <uuid>`, `codex exec resume <uuid>`, `-c experimental_resume=<path>` | `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` (GLOBAL); unique-birth-within-30s only | `.codex/sessions/<Y>/<M>/<D>/` for local-date −1, 0, +1 (3 ignore-prefixed `ls -lt` per leg — remote-clock date-sharding absorbed by the ±1 window) | trailing 36 chars of stem (`trailing_uuid`) | YES — new rollout names (a rollout is born at launch); global store ⇒ cross-terminal collisions handled by exactly-one | entry ships `remote: false` — flip after one real-Linux-host observation |
| **copilot** | `--resume <id>` / `-r <id>` | `~/.copilot/session-state/<id>/` per-session DIRS (GLOBAL root); unique-birth only | `.copilot/session-state/` — one `ls -lt`, DIR entries (`d` mode char, §12.2) | dir name | PARTIAL — new dirs only (a bare `copilot` = new session = new dir; growth of an existing dir is not size-visible) | entry ships `remote: false` |
| **qwen** | `--resume <id>` / `--session-id <id>` | `~/.qwen/projects/<sanitized>/chats/<id>.jsonl` — sanitize scheme is qwen's, local code scans ALL projects | would need `ls` of `.qwen/projects` then per-project `chats/` — second sequential connection, unbounded fan-out | filename stem | Technically, at 2× the connections | **DEFERRED** (argv-only until asked for) |
| **goose** | `--session-id/-n/--name <id>` | global SQLite — fs correlation impossible even locally | same SQLite | — | NO | argv-only (done) |
| **opencode** | `-s/--session <id>` | global SQLite | same | — | NO | argv-only (done) |
| **crush** | `-s/--session <id>` (TRAP: `-C`/`-c` are not identities) | none (argv-only) | — | — | NO | argv-only (done) |
| devin/cursor/amp/cline/gemini/aider | (registry `enabled: false`) | — | — | — | — | unchanged, stay off |

Notes:
- The **Explicit column is already shipped**: `track_hook_exec` → `analyze_cmdline`
  parses the exec hook's command line for Ssh sessions today; an explicit-id launch
  restores over ssh right now. This spec adds the BARE-launch column only.
- The bash exec hook's `cmd` is `history 1` (bootstrap.rs:175), i.e. the FULL typed
  line — `cd x && claude` yields argv[0] = `cd` ⇒ no adapter ⇒ missed. Inherited P6a
  limitation for local WSL too; tabled in §10, not in scope.
- claude on macOS remotes uses the same `~/.claude/projects` layout; parser is
  server-independent (§3.4).

## 2.0a Store-file BIRTH timing (load-bearing for §5 attribution)

The correlation separates terminals by WHEN their store file is BORN relative to
sibling block-opens. So the birth trigger per adapter matters:

| Adapter | Store file born at | Consequence for the gray zone |
|---|---|---|
| **claude** | **FIRST MESSAGE, not launch** (a bare `claude` idling at its prompt has written NO `<uuid>.jsonl` yet). Version-caveat: some builds create the file at session init near launch — treat the trigger as UNVERIFIED and design for either (§5 is correct regardless; the ONLY requirement for clean separation is that the earlier terminal's file exists before the later terminal's block opens) | an idle-then-active claude widens the window: if terminal A never messages before B opens, A's file births in B's interval ⇒ Ambiguous (honest, §5.5) |
| codex | **LAUNCH** — the `rollout-<ts>-<uuid>.jsonl` is created when codex starts (before any turn) | tighter separation than claude (birth ≈ block-open); still remote:false in v1 |
| copilot | **LAUNCH** (assumed — session-state dir made at start; VERIFY before flip) | as codex |

The acceptance scenario (§9.2a) passes because terminal A "works ~2 hours" before B
starts — A has certainly sent a message, so `file_A` exists in B's block-open snapshot,
which is exactly the bracket that pins `file_A` to A (proven on the stand-in, §12.5).

## 2.1 Remote store descriptors (new, daemon/remote_probe.rs)

```rust
pub struct RemoteStore {
    pub adapter: &'static str,
    pub remote: bool,                       // D7 gate, claude=true in v1
    /// Home-relative dirs to list, from the block's remote posix cwd.
    pub dirs: fn(cwd: &Path) -> Vec<String>,
    /// Store-shaped entry → resume token (also the entry filter).
    pub token_of: fn(name: &str, is_dir: bool) -> Option<String>,
    /// Rotation semantics (claude /clear) — enables R-NEWEST (§5.3).
    pub rotation: bool,
}
```
- claude: `dirs` = `[format!(".claude/projects/{}", claude_project_dir_name(cwd))]`;
  `token_of` = uuid stem of `*.jsonl` files; `rotation: true`.
- codex: `dirs` = the 3 date dirs (local today ±1, computed at probe time);
  `token_of` = `trailing_uuid` of `rollout-*.jsonl` stems; `rotation: false`.
- copilot: `dirs` = `[".copilot/session-state".into()]`; `token_of` = dir-entry name
  (uuid-shaped only); `rotation: false`.
- Munged/dated dir names are `[A-Za-z0-9./-]` only — batch-safe; quote anyway (the
  batch quoting rules are already proven for spaces).

---

## 3. Transport (shared module + probe legs)

### 3.1 `src/ssh_transport.rs` (NEW — a hoist, not a rewrite)

GUI and daemon are the SAME crate/exe; move these PURE items from `gui/ssh_drop.rs`
verbatim, keep their unit tests, and have `gui/ssh_drop.rs` `use crate::ssh_transport::*`
(zero behavior change, goldens prove it):

- `sftp_args(meta_args, batch)` + `sftp_args_transport(transport, batch)` — the full
  §3.2 flag-translation table (BatchMode prepended; `-p`→`-P` incl. glued; `-l` fold;
  `-i -F -J -o -c -4 -6 -C` carried; session-only flags dropped; ConnectTimeout=10 +
  ServerAlive appended after user flags; destination verbatim; `ssh://`→`sftp://`).
- `parse_pwd`, `parse_ls1`, `parse_ls_l` (name+size; the regex's date field is `\S+`
  either form — load-bearing, see D1), `classify_conn`/`ConnErr` (+`classify_file`/
  `FileErr` used only by the GUI — move together, they share fixtures).
- `resolve_sftp(program)` (sibling-of-resolved-ssh, PATH fallback).
- NEW tiny helpers both sides want: `write_batch(path, text)` (UTF-8 no-BOM, LF) and
  `run_sftp(argv, timeout) -> Output` (CREATE_NO_WINDOW, stdin null, stdout+stderr
  piped, watchdog thread that `TerminateProcess`es the child at the deadline — the
  GUI's `kill_pid` moves here).

`Uploads`/`Worker`/toast plumbing STAY in gui/ssh_drop.rs (egui-typed). tc.exe does not
include the new module (no #[path] additions).

### 3.2 Probe leg — ONE connection, ls-only

Batch (per D5, all store dirs of the adapter in one file):

```
pwd
-ls -lt <dir-1>
-ls -lt <dir-2 (codex date dirs)>
…
```

- `-ls` ignore-prefix: a missing dir prints `Can't ls: "<abs>" not found` on stderr,
  exit stays 0, listing section empty (§12.2) — "store absent" is a NORMAL empty
  snapshot, not an error.
- `ls -lt` = long + mtime-sorted NEWEST-FIRST, sorted server-attr-side by the client
  (§12.2 proves ordering) — one command serves both the size diff and the R-NEWEST
  order.
- Output lines carry the full requested-path prefix (`.claude/projects/<munged>/<f>`)
  — strip it like the GUI does (parse_ls_l precedent).
- Argv = `ssh_transport::sftp_args(&meta.args, batch)` — the terminal's own persisted
  flags, so `-i`/`-F`/`-o`/aliases resolve identically to the session — or
  `sftp_args_transport` when `TC_SSH_PROBE_TRANSPORT` is set (D12).
- Exit 255 ⇒ `classify_conn`: `AuthRequired` ⇒ §4.6 cache; everything else ⇒ transient
  transport failure (retry rules per moment, §4).
- Deadline: `wait` bounded at **25s** wall (ConnectTimeout=10 covers connect; the
  watchdog covers a wedged established link) — probe ServerAlive stays the sftp_args
  default 15/3 (the softened 30/4 is the SESSION's setting; probes should die fast).

### 3.3 Spawn hygiene (daemon-side)

`Command::new(resolve_sftp(&meta.program)?)` + `creation_flags(CREATE_NO_WINDOW)`,
stdin null, `wait_with_output` on the probe worker thread. The daemon's env is already
scrubbed of Claude-session markers at terminal spawns; sftp children need no scrub
(nothing remote executes user code) but inherit the daemon env harmlessly.

### 3.4 Server-independence

The `ls -l` line shape is formatted CLIENT-SIDE from SFTP v3 attrs (proof: link count
renders `?` and perms render masked `-rw-******` — no server produces那 shape; §12.2)
⇒ name/size/order parsing is stable across OpenSSH/BSD/macOS servers. Date columns are
NOT stable (§12.3) and are never read (DO-NOT #1).

---

## 4. Probe moments (when, exactly, and why)

| # | Moment | Probe? | What happens |
|---|---|---|---|
| M0 | Bare-CLI block OPEN (`track_hook_exec` set inner_cli with token=None, family Ssh, adapter `remote: true`) | **YES — snapshot leg** | worker thread: one §3.2 connection → write `probes\<id>.json` sidecar {adapter, cwd, block_key: (epoch,start_off), event_ms (the block-open wall clock — the §5.1 timeline axis), overflow, listings: dir→[(name,size)] in ls -lt order}. This snapshot is the time-witness that a SIBLING terminal's later restore reads (D1a) — so it must be taken PROMPTLY at open, even for a terminal that itself never restores. Cooldown: a listing <30s old for the same (terminal, adapter, cwd) is reused |
| M1 | Block CLOSE (the `cli_blocks` key closes) | **NO connection** | inner_cli clears (existing lifecycle — CLI exited, nothing to resume); DELETE the probe sidecar (this terminal has nothing to resume). SAFE for siblings: losing a closed block's witness can only COARSEN a sibling's §5 timeline (merge two intervals) ⇒ degrade a clean separation to Ambiguous, NEVER a wrong swap — the never-swap invariant holds |
| M2 | Daemon graceful shutdown / sleep with the block OPEN | **NO** | shutdown has a 2s budget and sleep needs none: the persisted M0 snapshot + the M3 probe at the next launch cover it — sftp is a FRESH connection, alive after the ssh link (and the remote CLI) died. THIS is the hour-long-claude case |
| M3 | Restore-class launch (boot restore, GUI Restore, ctl Wake/Restart, tc restart, wake-from-sleep) with `meta.inner_cli = Some(cli)`, family Ssh, ssh_hooks on, `cli.confidence == Ambiguous`, `cli.resume_token == None`, adapter `remote: true` | **YES — correlate leg** | one §3.2 connection for the fresh listing L, then §5 over {ALL sibling sidecars for this store (D1a)} ∪ {L} → on Correlated: upgrade `meta.inner_cli` (token + Correlated, persisted via the state lock + save) and fall through to the UNCHANGED §7.4 trailing; on definitive-Ambiguous: preface candidates line (§6.4) + CLEAR inner_cli; on transport failure: keep inner_cli (retry at the next restore-class event), restore shell-only. NOTE at a multi-terminal boot restore: sibling sidecars persist across the shutdown ⇒ each terminal's correlate leg sees the others' block-open witnesses even though the siblings aren't respawned yet (§9.2a both-open case) |
| M4 | Auto-reconnect attempt (`pump_reconnects` → launch) | **YES — same as M3** | rides each attempt inside the existing 2s/10s/30s backoff; the 30s cooldown reuses a fresh listing across rapid attempts; a Correlated verdict persists ⇒ later attempts skip the probe entirely |
| M5 | Explicit-token restore (claude only, sidecar present) | **piggyback on M3's connection** | the remote re-pin belt (§6.5) — no extra connection; skipped entirely when no sidecar exists |

Never probed: prompts/pre hooks, ticks, attach, GUI events, hookless or
remote_hooks-off terminals, TermKind::Claude pinned terminals (local machinery owns
those), non-Ssh families (WSL §7.3 stays a separate future item — via \\wsl$, not sftp).

### 4.5 Bounds recap
ConnectTimeout=10 (connect) · watchdog 25s (total) · cooldown 30s/terminal · ≤2
connections per CLI lifecycle + ≤1 per reconnect attempt (cooldown-deduped) · sidecar
≤64KiB (cap the stored listing at 500 entries/dir — beyond that the store is a haystack
and exactly-one can't fire anyway; log and store `overflow: true` ⇒ M3 goes straight to
Ambiguous-with-no-candidates).

### 4.6 Password-auth cache
`classify_conn == AuthRequired` (BatchMode `Permission denied (…`) ⇒ set a RUNTIME
`probe_auth_dead` flag for the terminal (a leaf `Mutex<HashSet<Uuid>>` beside
cli_blocks): all probes skip while set. Cleared when a LATER spawn of that terminal
reaches `hooks_live` (accept_token — the existing single token-check site): a
freshly-spawned link that hooked without anyone typing proves non-interactive auth
(same evidence class the reconnect qualifier uses). Not persisted — a daemon restart
retries once, which is the desired "keys were fixed while we were down" behavior.

---

## 5. Correlation rules (skew-immune, cross-terminal, birth-interval)

WHY not the naive single-sidecar diff: in the acceptance scenario (§9.2a) terminal A
opens at T0 and works 2h; terminal B opens at T0+2h in the SAME dir. A's own snapshot
(T0) predates BOTH files, and A's file GROWS in parallel while B runs — so A's naive
"new ∪ size-changed vs my snapshot" contains BOTH files, and so does B's. A per-block
sibling gate would then abstain BOTH — failing the test. Correlation must instead pin
each file's BIRTH to the block whose start is nearest, using the SIBLINGS' snapshots as
the time witnesses.

### 5.1 The observation timeline (per remote store, event-ordered — no remote clock)

For the (ssh destination, remote posix cwd) store being correlated, gather:
- every TC terminal's `probes\<id>.json` sidecar for THAT store (D1a — the daemon owns
  them all): each is `(event_ms, Some(block: (terminal,epoch,start_off)), names→sizes)`
  where `event_ms` = the LOCAL daemon wall-clock at that block's open;
- the restore-time listing L: `(now_ms, None, names→sizes)`.

Sort by `event_ms` ASCENDING → observations `O_0 … O_k` (O_k = L). The ordering axis is
the DAEMON's own clock across events IT observed (block opens + this restore) — NO
remote timestamp is ever compared to a local one, so it is skew-immune (invariant: only
set membership of remote-consistent listings + the daemon's own event order are read;
remote mtimes appear only in `ls -lt` SORT for R-NEWEST tie-break, never as values).

Sidecar validity gate (per sidecar, before it joins the timeline): adapter matches ∧
cwd matches ∧ `block_key` names a rehydrated BlockStore rec whose cmd re-parses
(analyze_cmdline) to the same adapter. Invalid/stale sidecars are dropped; the restored
terminal's own sidecar missing ⇒ §5.5 fallback.

### 5.2 Birth interval per file (pure set membership)

For each store-shaped file `f` (token_of matches) PRESENT at restore (in L):

```
birth(f) = ( O_i , O_{i+1} )   where O_i = last observation with f ∉ names,
                                       O_{i+1} = first observation with f ∈ names
```

`f` present in O_0 already (existed at the earliest block-open) ⇒ birth is UNBRACKETED
(predates all TC observation) — attributed to no block by birth (§5.5 handles it as a
possible `--continue`).

### 5.3 Attribution fixpoint (the birth-proximity rule)

Each interval `(O_i, O_{i+1})` is OWNED by the block that opened at `O_i` (if `O_i` is a
block-open, not the restore) — that block is the most-recently-started session as of the
interval, i.e. the nearest-preceding start to every birth in it. Iterate to fixpoint:

1. For each still-unresolved open block X (opened at `O_x`), let `births_X` = files whose
   birth interval is exactly `(O_x, O_{x+1})` and are not yet claimed.
2. If `|births_X| == 1` ⇒ **Correlated**(that token); claim the file; loop.
3. Repeat until no block resolves.

Then the remaining unresolved blocks:
- `|births_X| == 0` ⇒ §5.5 (X's file wasn't born in its own opening interval — X idled
  past a sibling's open, or never messaged).
- `|births_X| ≥ 2` ⇒ Ambiguous UNLESS §5.4 (rotation, sole claimant).

Acceptance walk (§9.2a, fixture §12.5): O_0=A-open{}, O_1=B-open{file_A}, O_2=L{file_A,
file_B}. birth(file_A)=(O_0,O_1)→A owns, `|births_A|=1`⇒A resumes file_A. birth(file_B)=
(O_1,O_2)→B owns, `|births_B|=1`⇒B resumes file_B. A→A, B→B. ✓

### 5.4 R-NEWEST (claude /clear rotation — sole-claimant carve-out)

For an unresolved block X with `|births_X| ≥ 2`, resume the `ls -lt`-NEWEST member of
`births_X` iff ALL:
1. adapter `rotation: true` (claude), AND
2. births came from a VALID timeline (§5.1, X's own sidecar present), AND
3. X is the SOLE remaining unresolved open block for this store after the §5.3 fixpoint
   (no OTHER open block could own any birth in X's interval), AND
4. newest is unambiguous in `ls -lt` order (order is total).

Rationale: /clear inside one uninterrupted block creates a rotation chain — all births
land in X's own interval with no sibling open between them; pre-clear ids are abandoned
by claude, the live one is newest-written. Precedent: the LOCAL claude rule already
ships mtime-newest-with-5s-gap as Correlated. Distinguisher from the idle gray zone
(§5.5): condition 3 — if a second terminal is also unresolved, a ≥2 interval could be a
SPLIT between two sessions, so R-NEWEST is forbidden and both stay Ambiguous.

Exposure: a NON-TC actor writing the same remote cwd's store during X's interval —
visible-and-recoverable (resumed TUI shows the conversation; `claude --resume` picker
fixes a wrong pick), strictly narrower than the local rule's. §11 Q1 = strict-mode
one-liner (delete this arm).

### 5.5 Fallbacks & the honest gray zone (never swap conversations)

- **No sidecar for the restored terminal** (M0 failed / predates feature): list the
  store; **exactly ONE store-shaped entry in the cwd-scoped store** ⇒ Correlated (claude
  only — cwd-scoped store ⇒ "the only session ever run here" is an identity, mirroring
  the local `cands.len()==1` branch). Global stores (codex/copilot) get NO lone-entry
  fallback. Anything else ⇒ Ambiguous with candidates (D10).
- **Unbracketed file that grew** (present in O_0, `--continue` case): attributable to X
  only if X is the sole unresolved claimant AND exactly one such grown file exists —
  else Ambiguous (a grown file in a multi-session dir proves nothing about which block).
- **THE GRAY ZONE (must stay Ambiguous): two bare claudes in the same remote dir whose
  first-message births fall in the SAME interval** — i.e. no block-open event separates
  them (both idled past each other's open, then both messaged). `births` of that interval
  ≥ 2 with ≥ 2 unresolved blocks ⇒ every involved block Ambiguous; candidates listed
  newest-first (§6.4). The resume machinery NEVER guesses an ordering fine enough to swap
  conversations. This is the exact case the acceptance scenario AVOIDS by having A work
  before B starts (§2.0a), and the exact case §9.2a's negative variant PINS.
- `|births| == 0` for all (nobody messaged) / store dir absent / >500 entries (overflow)
  ⇒ Ambiguous, no candidates.
- `|C| ≥ 2` for a rotation-LESS adapter (codex/copilot) ⇒ always Ambiguous (no /clear
  semantics to justify newest).

---

## 6. Restore wiring

### 6.1 The seam

`launch()` (daemon/mod.rs ~1149) already dispatches on `meta.inner_cli` with
`Explicit | Correlated` → `tracker::restore_trailing(adapter, token)` →
`bootstrap::ssh_restore_trailing(&cli_cwd, &resume_cmd)` baked into the one-shot rc
(with SSH_SELF_DELETE before it and the manual `__tc_emit exec` announcing the resumed
CLI so a new block opens and inner_cli's lifecycle re-arms). **This spec inserts ONE
step before that match** for `is_ssh && ssh_hooks`:

```
remote_probe::upgrade_before_launch(core, id, &mut meta)   // M3/M5, bounded
```

which (a) runs the correlate leg when due, (b) mutates + persists
`meta.inner_cli`/state on Correlated, (c) queues the preface candidates line on
definitive-Ambiguous and clears inner_cli, (d) no-ops on cooldown/auth-dead/not-due.
Everything downstream — trailing, hooked respawn, epoch rotation, GUI presentation —
is byte-identical to the shipped Explicit path.

### 6.2 Threading

- Conn-thread entry points (C2D::RestartTerminal, Ctl Wake/Restart/Run-gated wake):
  when a probe is DUE (M3 predicate true, cooldown open, not auth-dead), spawn
  `probe-launch-<id>` (a thread) that runs upgrade_before_launch → launch(). A
  `probing: Mutex<HashSet<Uuid>>` guard (LaunchGuard's sibling, same if/else
  construction — remember the `bool::then_some` eager-construction trap) coalesces
  double-clicks; the existing LaunchGuard still protects launch() itself.
- Boot restore lanes + pump_reconnects worker: call upgrade_before_launch inline (they
  are already off the conn threads; the stagger/backoff absorbs the bound).
- Sleep's wake path (Ctl Wake / GUI Wake ▸ / WakeFolder) rides the same two shapes —
  wake IS launch().

### 6.3 Wake-from-sleep specifics

Sleep with an open claude block already requires force/confirm (busy gate) — the kill
SIGHUPs the remote shell and the CLI dies with the link; the jsonl's final state is on
disk. Wake = launch() ⇒ M3 fires ⇒ correlate ⇒ resume. The `asleep` flag's clear-point
inside launch() is untouched.

### 6.4 Preface candidates (Ambiguous UX, daemon-side only)

Extend the existing ambiguous info line (push_info_line — preface-only, never mirror):

```
multiple claude sessions found in /home/alice/proj — resume one manually:
  claude --resume cccccccc-…   (newest)
  claude --resume bbbbbbbb-…
```

Cap 5, newest-first (ls -lt order), full commands so a paste/history-Run works. When
the probe TRANSPORT failed, keep today's shorter line (no candidates claim).

### 6.5 Remote re-pin belt (M5, claude only)

At restore of an Explicit-token claude inner_cli with a VALID sidecar: run the §5
timeline for this terminal's own block. If the pinned token's file was NOT born/grown in
this block's interval (untouched all run) ∧ the interval has exactly one member ∧ this
block is the sole claimant ⇒ re-pin inner_cli.resume_token to that member (log exactly
like the local `claude_repin_candidate` line). Pinned file IN this block's interval ⇒
keep (it's alive). Ambiguous ⇒ keep the pin (abstain — same rules as tracker.rs's local
belt, evidence source swapped from created()/modified() to the §5 birth-interval).

---

## 7. Frequency / cost discipline (recap, normative)

- Probes are event-driven ONLY (M0/M3/M4/M5). No polling, no per-prompt work, no
  attach work. An idle ssh terminal costs zero.
- Per-terminal 30s listing cooldown; per-terminal auth-dead cache (§4.6); Correlated
  persists ⇒ never re-probed for the same run.
- One connection carries ALL dirs for the adapter (batch of `-ls` lines).
- All failures silent-to-GUI: daemon.log `[probe]` lines (gate them on the existing
  log level, not a new env knob; they are rare by construction). The ONLY user-visible
  surface is the §6.4 preface line — which already existed.

---

## 8. Files (implementation map)

| File | Change |
|---|---|
| **`src/ssh_transport.rs` (NEW)** | pure hoist from gui/ssh_drop.rs (§3.1): sftp_args/sftp_args_transport, parse_pwd/parse_ls1/parse_ls_l, classify_conn/ConnErr (+classify_file/FileErr), resolve_sftp, write_batch, run_sftp(+watchdog kill_pid). Goldens move with it |
| `src/gui/ssh_drop.rs` | `use crate::ssh_transport::*`; deletes the moved fns; Uploads/Worker unchanged |
| **`src/daemon/remote_probe.rs` (NEW)** | `RemoteStore` registry (§2.1); sidecar type + atomic IO (`probes\<id>.json`, tmp+rename); snapshot leg (M0) + correlate leg (§5) as pure fns over parsed listings (unit-testable without sockets); `upgrade_before_launch`; cooldown/auth-dead bookkeeping; TC_SSH_PROBE_TRANSPORT handling |
| `src/daemon/mod.rs` | track_hook_exec: sanitize remote verdict (D11) + M0 snapshot trigger (worker thread) ; cli_blocks close path: delete sidecar (M1); launch(): the §6.1 seam + `probing` guard + §6.4 preface; DeleteTerminal: delete sidecar; Core: `probing` + `probe_auth_dead` leaf locks |
| `src/daemon/tracker.rs` | the D11 guard (either an extract-signature `local_store: bool` param or a post-hoc `sanitize_remote(inner) -> InnerCli` that strips fs-derived Correlated tokens — implementer's pick; requirement: a posix-cwd/Ssh analysis can only ever emit Explicit tokens or token-less Ambiguous) |
| `src/state.rs` | `pub fn data_probes_dir()` beside the journals helper. NOTHING persisted-schema changes |
| `src/daemon/session.rs` / bootstrap.rs / protocol.rs / GUI / tc.exe | **ZERO changes** (the trailing machinery is already adapter-generic) |

---

## 9. Probes & tests

### 9.1 Cargo units (remote_probe.rs + ssh_transport.rs)
- Diff rule table: new-name / size-changed / absent-dir-snapshot / |C|∈{0,1,≥2} /
  overflow-500 ⇒ straight-to-Ambiguous.
- R-NEWEST preconditions: rotation flag, valid-sidecar gate, sibling-block gate flips
  it off, newest-first pick from a captured `ls -lt` fixture.
- §5.2 fallback: claude exactly-one fires, codex/copilot never fall back.
- Re-pin belt truth table (§6.5) mirroring `claude_repin_evidence_rules`.
- parse_ls_l against BOTH captured date forms (§12.2 fixture, anonymized in lockstep
  with the code fixture — H:M and YEAR lines in one listing) + the `?`-link/
  masked-perms shape + prefix strip.
- Store descriptors: claude munged path golden (`/home/alice/proj` →
  `.claude/projects/-home-alice-proj`), codex ±1 date dirs, copilot dir filter.
- D11 sanitize: `analyze_cmdline("claude", posix_cwd)` can never emit a token even
  with a colliding LOCAL store dir staged in a temp HOME.
- Sidecar roundtrip + validity gate (wrong adapter/cwd/block_key ⇒ invalid).

### 9.2a `ssh_two_parallel_claudes` — THE USER ACCEPTANCE TEST (MUST PASS, named)

Verbatim scenario: ssh host, `cd /xyz/`; terminal **A** opens a bare `claude` at T0 and
works ~2h; a SECOND ssh terminal **B** opens a second bare `claude` in the SAME `/xyz/`
at T0+2h; both work in parallel for hours; the app closes with **both CLI blocks OPEN**;
restore/reboot ⇒ **A resumes A's conversation and B resumes B's**.

Staged on the WSL stand-in (fabricated stores, staggered births — §12.5 proves the
listing shapes; the probe drives it through the real daemon):
1. Isolated TC_DATA_DIR daemon; `TC_SSH_VIA_WSL`=session stand-in +
   `TC_SSH_PROBE_TRANSPORT`=`-D` sftp-server on `/tmp/tcprobe-home`.
2. Terminal **A**: create ssh terminal, `cd /tmp/tcprobe-home/xyz`, run bare `claude`
   (a fake `claude` script: on first "message" it writes `~/.claude/projects/-xyz/
   <uuidA>.jsonl` and appends every tick). Drive ONE message so `file_A` is born.
   Assert `probes\<A>.json` snapshot recorded with `file_A` ABSENT at A's open, then
   present after (the M0 witness).
3. **Wall-clock stagger** (the load-bearing part): terminal **B** opens its block AFTER
   `file_A` already exists ⇒ B's M0 snapshot CONTAINS `file_A` (the §5.1 bracket). Run
   bare `claude` in the same `/tmp/tcprobe-home/xyz`, drive a message ⇒ `file_B` born.
4. With BOTH cli_blocks OPEN, restart the daemon in-case (Shutdown+respawn, PowerShell
   context rule). Sibling sidecars persist.
5. Assert at boot restore: A's correlate leg (timeline O_0=A-open{}, O_1=B-open{file_A},
   O_2=L) attributes `file_A` to A ⇒ A's first CLI block cmd == `claude --resume <uuidA>`;
   B's leg attributes `file_B` to B ⇒ `claude --resume <uuidB>`. Assert via
   BlockText/journal, and inner_cli Explicit for both afterward.
6. **Negative variant (the gray zone, must ABSTAIN)** `ssh_two_parallel_claudes_idle`:
   A opens but sends NO message until AFTER B opens; then both message. Both births land
   in B's interval (no block-open separates them) ⇒ BOTH restore shell-only, preface
   lists both candidates newest-first, neither inner_cli guessed. Pins that we NEVER
   swap conversations when starts can't be ordered.

Both are counted must-pass in the suite; the negative variant is as important as the
positive (it proves the abstention, not just the resume).

### 9.2 Probe cases (suite, WSL stand-ins; SKIP without an Ubuntu distro)

`ssh_cli_resume` — the end-to-end:
1. Isolated TC_DATA_DIR daemon with `TC_SSH_VIA_WSL=<host>` (session transport
   stand-in) and `TC_SSH_PROBE_TRANSPORT=<-D …sftp-server -d /tmp/tcprobe-home>`
   (probe stand-in pointed at the SAME fabricated home the stand-in shell uses:
   run the wsl stand-in with HOME=/tmp/tcprobe-home so hooks' $PWD and the store
   agree).
2. Fabricate a fake `claude` executable in the stand-in home's PATH: a 5-line sh
   script that mkdir-p's `~/.claude/projects/$(munge $PWD)`, writes
   `$(uuidgen).jsonl`, appends to it every second, and sleeps until killed.
3. Create the ssh terminal, `tc run` a `cd /tmp/tcprobe-home/proj` then send the bare
   `claude` line; await the block-open + assert the M0 sidecar exists with the
   pre-launch listing.
4. `kill -9 $$` the stand-in shell (the ssh_reconnect probe pattern) ⇒ unexpected exit
   ⇒ auto-reconnect ⇒ assert the respawned session's first CLI block cmd ==
   `claude --resume <that-uuid>` (BlockText/journal assertion) and inner_cli is
   Explicit again afterward.
5. /clear analog: variant where the fake claude writes a SECOND jsonl mid-run ⇒
   R-NEWEST resumes the second uuid.
6. Ambiguous variant: TWO TC ssh terminals in the same fabricated cwd both running the
   fake claude ⇒ both restore shell-only, preface lists 2 candidates newest-first,
   inner_cli cleared (no retry storm on the next restore).
- `ssh_cli_resume_fallback`: delete the sidecar before restore ⇒ exactly-one rule
  correlates; add a second old jsonl ⇒ Ambiguous.
- Auth-dead: point TC_SSH_PROBE_TRANSPORT at a command printing
  `Permission denied (publickey).` to stderr + exit 255 (a sh stub) ⇒ probe skipped on
  the next attempt (log assertion), cleared after a hooks_live respawn.
- Existing `ssh_reconnect`/`ssh_bootstrap_local` must stay green untouched.

### 9.3 Staging recipe (validated 2026-07-04, this session)
WSL /tmp is volatile — re-stage per session: `apt-get download openssh-sftp-server &&
dpkg -x` to /tmp/tcsftp; fabricate the store under /tmp/tcprobe-home (touch -d for
mtimes); drive `sftp.exe -q -b <batch> -D "C:/Windows/System32/wsl.exe -d Ubuntu --
/tmp/tcsftp/usr/lib/openssh/sftp-server -d /tmp/tcprobe-home"`. wsl.exe mangles inline
multi-line quoting from git-bash — write staging scripts to a file and run
`wsl -- sh /mnt/c/...` (MSYS_NO_PATHCONV=1).

---

## 10. Edges (behavior table)

| Edge | Behavior |
|---|---|
| CLI launched with explicit `--resume <id>` over ssh | already works today (Explicit via analyze_cmdline) — no probe, no change |
| Bare claude, exits cleanly, terminal dies later | inner_cli cleared at block close (existing) ⇒ shell-only restore — correct, matches local |
| Hour-long claude, PC sleeps / daemon killed / reboot | M0 sidecar persisted ⇒ M3 at boot restore correlates over a fresh sftp connection ⇒ `cd '<cwd>' && claude --resume <id>` |
| /clear mid-run (id rotates under an open block) | diff yields 2 candidates; R-NEWEST resumes the post-clear id (§5.3) |
| `claude --continue` (no new file) | grown-file rule catches the appended jsonl ⇒ Correlated to the continued session |
| Two claudes SEQUENTIALLY in one ssh session | each exec overwrites inner_cli + sidecar (new block_key); only the last open block matters at death |
| **Two claudes, same remote cwd, STAGGERED starts, BOTH OPEN at shutdown (the §9.2a acceptance test)** | cross-terminal birth-interval (§5): each file attributed to the block whose open precedes its birth ⇒ A→A, B→B. MUST PASS |
| Two claudes same cwd, first messages NOT separated by a block-open (both idled past each other's open) | THE gray zone: births share one interval, ≥2 unresolved blocks ⇒ BOTH Ambiguous, candidates listed — never swap (§5.5, negative test §9.2a.6) |
| N claudes same cwd, all blocks open at restore | fixpoint resolves every block whose interval has exactly one unclaimed birth; unseparated ones stay Ambiguous (§5.3) |
| Non-TC actor writes the same store mid-window | R-NEWEST's stated exposure (§5.3); strict mode is the Q1 one-liner |
| `cd x && claude` (compound line) | never detected (history-1 argv[0]=cd) — inherited P6a limitation, unchanged |
| Store dir absent at correlate | C empty ⇒ Ambiguous, no candidates (different HOME / CLAUDE_CONFIG_DIR / never persisted) |
| Remote clock skew (any magnitude, either sign) | invisible: set-membership + size + server-side sort only; date columns never parsed |
| macOS / BSD remote | same claude store path; ls shape is client-formatted from attrs (§3.4) — works; codex/copilot flip per-OS only after field verification (D7) |
| Password-auth host | first probe fails fast (BatchMode) ⇒ auth-dead cache ⇒ zero further connections until a hooks_live spawn proves keys (§4.6); restores stay shell-only Ambiguous |
| SFTP subsystem disabled on host | probe exit 255 ⇒ transport-fail path ⇒ silent Ambiguous (keep inner_cli for retry) |
| remote_hooks=false terminal | no exec hooks ⇒ no bare-CLI tracking at all ⇒ no probes (D13) |
| Reconnect resumes while the remote CLI is still alive (server ClientAlive lag after a link-only drop) | inherited from the SHIPPED Explicit-token reconnect path — unchanged; claude tolerates or refuses visibly; not made worse by this feature |
| Daemon restart between M0 and M3 | sidecar + BlockStore sidecar both persist; validity gate re-joins them |
| Probe transport fails at M3/M4 | shell-only restore, inner_cli KEPT ⇒ next restore-class event retries; definitive Ambiguous (probe ran, ≥2/0) CLEARS it ⇒ no retry storm |
| tc restart / GUI Restore double-click during a probe | `probing` guard coalesces; LaunchGuard backstops launch() itself |
| Store >500 entries | overflow ⇒ Ambiguous immediately (no candidates line) — a haystack store can never satisfy exactly-one anyway |

---

## 11. Open questions (with defaults — implementation proceeds on the default)

| # | Question | Default |
|---|---|---|
| Q1 | R-NEWEST default ON? | ON for claude with the §5.3 precondition set (local mtime-newest precedent); strict mode = delete one match arm, documented |
| Q2 | A per-terminal probe opt-out beyond remote_hooks? | NO new toggle (D13); revisit only if a user objects to background sftp connections — the auth/keys story means the connection is to THEIR OWN host with THEIR OWN key |
| Q3 | Enable codex/copilot remote correlation in v1? | ship the descriptors `remote: false`; flip each after ONE observed real-Linux-host store write (the local adapters' own "exact-id verified" bar) |
| Q4 | Double-listing "mtime-active" detection (probe twice, 2s apart, the growing file = the live one) for reconnect-while-remote-alive | REJECTED for v1: only helps a narrow race (server hasn't reaped the old session), costs a second connection per attempt, and R-NEWEST already covers the rotation case that motivated it |
| Q5 | Surface "correlated, resuming <id>" in the GUI during restore | NO — the resumed TUI itself is the feedback (ux doctrine: no confetti); daemon.log carries the forensics |
| Q6 | qwen remote fan-out | deferred until someone actually runs bare qwen over ssh; argv-Explicit works today |
| Q7 | WSL §7.3 (\\wsl$ correlation) unification with this machinery | keep separate: WSL stores are locally mounted (no sftp needed); only the D11 sanitize and the verdict rules are shared vocabulary |

---

## 12. Evidence log (captured 2026-07-04, this machine)

### 12.1 Environment
- OpenSSH_for_Windows_9.5p2; sftp-server 9.6p1 (Ubuntu-24.04 WSL, dpkg-extracted, no
  install). ssh-agent service: Stopped/Disabled; `\\.\pipe\openssh-ssh-agent` absent;
  `~/.ssh` = config, id_rsa(+pub), known_hosts — plain key-file auth, process-context
  independent (inv. 9).
- Local `~/.claude/projects` confirms the munge (D6): `C--Terminal-Control`,
  `C--Users-alice-Downloads`, `C--Some-Side-Project--claude-worktrees-agent-…`.
- Env-lineage check (research Q2): remote envs are clean by construction —
  session::spawn env_removes `is_claude_session_var` on EVERY spawn (session.rs:412),
  ssh forwards no env (grep: zero SendEnv/SetEnv anywhere), the remote rc exports only
  `TC_RC` + `MOTD_SHOWN` (bootstrap.rs:334,344,400), WSLENV is WslShell-gated
  (session.rs:422). A remote claude WILL write its transcript.

### 12.2 sftp probe shapes (batch vs the fabricated store, exit 0)
```
sftp> pwd
Remote working directory: /tmp/tcprobe-home
sftp> -ls -1 .claude/projects/-home-alice-proj
.claude/projects/-home-alice-proj/aaaaaaaa-….jsonl            ← full-prefix names
sftp> -ls -l .claude/projects/-home-alice-proj
-rw-******    ? alice alice   1 Jul  4 13:56 …/aaaaaaaa-….jsonl   ← 2h old: H:M form
-rw-******    ? alice alice  10 Jul  4 15:46 …/bbbbbbbb-….jsonl   ← 10min: H:M form
-rw-******    ? alice alice   5 Jul  4  2026 …/cccccccc-….jsonl   ← touched NOW: YEAR form(!)
sftp> -ls -lt .claude/projects/-home-alice-proj
(cccccccc first, then bbbbbbbb, then aaaaaaaa — newest-first, sort is attr-truth)
sftp> -ls -l .claude/projects/MISSING-DIR
(empty; stderr: Can't ls: "/tmp/tcprobe-home/.claude/projects/MISSING-DIR" not found; exit stays 0)
sftp> -ls -l .copilot/session-state
drwx******    ? alice alice 4096 Jul  4  2026 .copilot/session-state/1b2f3c4d-…   ← 'd' mode char
```
`-rw-******` masked perms + `?` link count = the listing is CLIENT-formatted from
attrs, not a server longname ⇒ shape is ours/stable (§3.4). `parse_ls_l`'s regex
(`\S+` date fields) parses every line above unmodified.

### 12.3 The date-form trap (why D1 is mandatory)
The file touched "now" rendered in YEAR form (`Jul  4  2026`) while 10-minute-old and
2-hour-old files rendered `H:M` — OpenSSH's formatter uses the year branch whenever
the remote mtime is even fractionally AHEAD of the local clock (WSL vs Windows clocks
drift sub-second). Any design that parses listing dates inherits a skew-dependent
format flip on exactly the freshest — most correlation-relevant — files. Ergo:
set-membership + sizes + server-side `-t` sort; dates never read.

### 12.5 Acceptance-scenario store evolution (staged, staggered births)
Fabricated `/tmp/tcprobe-home/.claude/projects/-xyz/` driven through the three
observation moments, then `ls -lt` captured over `sftp -D`:
```
E0 (A opens, T0):       store = {}                              ← snapshot S_A empty
  (A messages: file_A born, then grows over 2h)
E1 (B opens, T0+2h):    store = { aaaa….jsonl }                 ← snapshot S_B has file_A
  (B messages: file_B born; A still active → file_A grows)
E2 (restore):  sftp -ls -lt .claude/projects/-xyz  →
  -rw-******  ? alice alice 16 Jul  4  2026 …/bbbbbbbb-….jsonl   ← file_B (newest)
  -rw-******  ? alice alice 16 Jul  4  2026 …/aaaaaaaa-….jsonl   ← file_A (GREW 4→16)
```
Birth-interval attribution (pure set membership, §5.2/5.3): file_A ∉ S_A, ∈ S_B ⇒
(E0,E1) ⇒ owner A. file_B ∉ S_B, ∈ L ⇒ (E1,E2) ⇒ owner B. **A→file_A, B→file_B — the
parallel growth of file_A is irrelevant because attribution keys on FIRST APPEARANCE,
not size.** A single-sidecar size-diff would have tied (both files "new/changed" vs A's
T0 snapshot); the sibling snapshot S_B is what separates them.

### 12.4 Code seams verified (source-level, today's tree)
- `track_hook_exec` (daemon/mod.rs:1699-1726): Ssh family routed, hook cwd verbatim,
  `cli_blocks` insert + `set_inner_cli` — the M0 trigger point.
- Block-close clear (mod.rs:657-668): the M1 sidecar-delete point.
- `launch()` inner_cli match (mod.rs:1149-1190): the §6.1 seam; ssh arm already builds
  `ssh_restore_trailing(cd + __tc_emit exec + resume)` — resume re-announces as a real
  block so the lifecycle re-arms.
- `analyze_cmdline` (tracker.rs:288) passes the posix cwd into extract fns whose
  fs-branches read the LOCAL home (claude_extract:498-504) — the D11 hazard, currently
  benign only because no local dir munges like a remote posix cwd.
- `sftp_args`/`parse_ls_l`/`classify_conn`/`resolve_sftp` (gui/ssh_drop.rs:44,316,368,
  946) are egui-free and golden-tested — the §3.1 hoist set.
- `claude_repin_candidate_in` (tracker.rs:440-486): the decision shape §6.5 mirrors.

---

## 13. DO-NOTs (hard rules for the implementer)

1. **DO NOT parse ls date/time columns — ever** (§12.3). Set-membership, sizes, and
   `-t` order are the only time-shaped inputs.
2. **DO NOT compare remote mtimes to local clocks or local block timestamps** — the
   snapshot diff replaces all window math.
3. **DO NOT write on the remote from probes** (no mkdir/put/rm — inv. 1; the drop
   feature's `.tc-drops` exception does not extend here).
4. **DO NOT probe on conn handler threads or inside the Shutdown path** (§6.2, M2).
5. **DO NOT resume on |C| ≥ 2** outside R-NEWEST's exact precondition set; strict
   exactly-one everywhere else. Ambiguous + candidates is a feature, not a failure.
6. **DO NOT let remote (posix-cwd) analyses mint tokens from LOCAL fs branches**
   (D11) — Explicit-or-nothing out of analyze_cmdline for Ssh/WslShell.
7. **DO NOT touch TermKind::Claude pinned terminals** — the local pin/repin machinery
   owns them; this feature is Shell-family inner_cli only.
8. **DO NOT add wire variants, persisted state fields, or GUI code** — proto stays 10;
   the sidecar file and runtime leaf locks are the entire new state surface.
9. **DO NOT retry a DEFINITIVE Ambiguous** (probe ran, 0 or ≥2) — clear inner_cli;
   only transport failures keep it for retry.
10. **DO NOT let BatchMode be overridden off, and never touch known_hosts/config**
    (inv. 5/6).
11. **DO NOT poll**: no timers, no per-prompt probes, no attach probes. M0/M3/M4/M5
    are the complete moment set.
12. **DO NOT ship staging knobs beyond the env-gated `TC_SSH_PROBE_TRANSPORT`**
    (data_dir_overridden-gated, TC_SSH_VIA_WSL class); delete any scratch demo rigs
    before install.
