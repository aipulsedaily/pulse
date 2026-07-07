# Sleep / Wake — per-terminal & per-folder process hibernation — Implementation Spec (final, implementation-ready)

Target: C:\Terminal Control (Rust daemon + egui 0.35 GUI + tc.exe, single crate, proto 7
at research time — see §5.0 for the coordinated bump).

User requirement (verbatim): **"~20 claude powershells ≈ 6GB RAM → option to put a whole
folder to sleep or something WHILE keeping the persistant were i can come back at any
given time."** Right-click a terminal or folder → Sleep; wake anytime; 100% persistence.

Interpretation (binding): **sleep = a controlled, per-terminal daemon-restart-equivalent.**
Drain the output tail, kill the ConPTY process tree (shell + conhost + inner CLI — where
all the RAM lives), keep journal/blocks-sidecar/state/pinned-CLI-identity EXACTLY as a
reboot does, and wake through the existing `launch()` restore path (respawn, cd, `claude
--resume`, hooks re-arm, ReplayAnchors restyle the history). Nothing new is invented for
persistence — sleep deliberately rides the restore machinery the app already trusts with
reboots, because that machinery is the most probe-pinned code in the tree.

Measured headline (methodology + full numbers in §9): one idle claude-in-powershell
session = **~447 MB working set / ~650 MB commit**; the user's 22 live claude sessions
sum to **~6.6 GB WS** (claim confirmed). Killing the root shell demonstrably kills the
whole tree in <2.5s; the daemon retains ~1–2 MB/session. Wake = **293 ms** to a verified
hooked prompt; a warm `claude` relaunch reaches its TUI in **~1.1 s**.

Ordered: invariants → decisions → state machine → sleep flow → wake flow →
protocol/state → tc → GUI → RAM ground truth → file-by-file → probes/tests →
degraded/edges → perf → open questions → DO-NOTs. Every decision carries a one-line
justification.

---

## 0. Non-negotiable invariants (violating any is a bug)

1. **Persistence identity**: an asleep terminal's journal, blocks sidecar, state.json
   meta (incl. `inner_cli`, pinned Claude session id, `live_cwd`, `last_cols/rows`) are
   byte-identical to what a daemon shutdown would have left. Sleep adds ONE bit of state
   (`asleep`) and touches nothing else at rest.
2. **Drain before kill**: the per-session output tail is drained (in_flight == 0 AND
   output-quiet ≥300 ms, capped) before the process dies — the journal-truncation class
   (`restore_fidelity`: conhost renders on async frames; a kill mid-`ls` loses the
   table's tail rows forever) applies to sleep exactly as it does to Shutdown.
3. **Mirror purity untouched**: sleep never writes to the mirror Term, the PTY, or the
   journal (the wake-time seam append is `launch()`'s existing code, not sleep's).
4. **bincode append-only**: new C2D variants at the C2D enum END, new CtlRequest
   variants at ITS end; `TerminalMeta.asleep` appended `#[serde(default)]` after
   `shell_cfg` (same same-exe rationale — GUI+daemon version-matched, tc.exe never
   decodes Snapshot). Proto bumps by one, COORDINATED with sidebar-p2 (§5.0).
5. **Never guess, never auto-wake**: selecting, clicking, scrolling, copying, or
   `tc run`-ing an asleep terminal NEVER spawns processes. Waking is always an explicit,
   visible, clickable act (or an explicit `tc wake`). A misclick that silently launches
   `claude --resume` (API session, 450 MB tree) is a correctness bug, not a convenience.
6. **Boot honors sleep**: a terminal asleep at reboot stays asleep after reboot, until
   woken — `auto_restore` is the "restore after reboot" intent; `asleep` is the stronger
   "not until I say so" intent and wins while set.
7. **Degraded honesty**: waiters/controllers blocked by sleep get structured refusals
   ("asleep", "busy"), never silent drops or fake successes; the GUI states what a
   sleeping terminal is with a distinct glyph, never a look-alike of Dead.
8. **No new polling loops**: the sleep drain is a bounded one-shot poll (25 ms tick,
   ≤2 s cap — `drain_output_tail`'s exact discipline, scoped to the target set); wake
   pacing reuses the restore-lane constants. Idle daemon cost of the feature is zero.
9. **UX doctrine** (ux-doctrine.md): mouse-first, zero dividers, hover-reveal, text/glyph
   affordances. The moon is a glyph in the existing dot slot, the Wake affordance is
   accent text in existing slots — no new chrome surfaces.
10. **Capture-on-change**: the `asleep` flag flows through the same
    mutate → state.save → broadcast_snapshot path as every other meta change (power-loss
    safe; multi-GUI coherent).

---

## 1. Headline decisions

| # | Decision | One-line justification |
|---|---|---|
| S1 | **Persist `asleep: bool` on TerminalMeta (serde-default, appended last); `TermStatus` stays two-variant.** Presented state is derived: (Running,false)=Running · (Running,true)=Sleeping (transient) · (Dead,true)=Asleep · (Dead,false)=Dead | `SharedState::load()` forces every status to Dead at boot (state.rs:412) — a persisted `TermStatus::Asleep` variant would be clobbered on load and needs a wire-enum change; the flag survives load untouched, keeps `on_exit` 100% unchanged, and gives the "Sleeping" transient for free |
| S2 | **Sleep = flag → fail waiters "asleep" → per-session drain (≤2 s) → `killer.kill()`; the existing exit-watcher → `on_exit` does ALL bookkeeping** (Dead status, journal sync, dangling-block close exit=None, sidecar save, Exited broadcast, Snapshot) | Measured: TerminateProcess on the shell kills claude.exe + conhost + powershell in <2.5 s (Session drop closes the ConPTY; conhost death terminates attached clients) — the Kill path already reclaims the entire tree, so sleep is Kill plus intent |
| S3 | **Wake = `launch(id)`, verbatim**; `launch()` clears `asleep` in the same mutate that sets Running | launch() IS the reboot-restore path (seam append, preface, family synthesis, claude `--resume`, hook token rotation, suspend+Reset+Replay+StreamPos+Blocks+PromptState+ReplayAnchors resync) — wake inheriting it wholesale is the whole architecture insight; one clear-point covers wake verb, GUI Restore, and tc restart alike |
| S4 | **Boot auto-restore skips asleep**: the run() filter becomes `t.auto_restore && t.launched_once && !t.asleep` | One-line skip at the single restore enqueue site; asleep-across-reboots falls out (inv. 6) |
| S5 | **New C2D verbs: `SleepTerminal{id}`, `SleepFolder{folder}`, `WakeFolder{folder}` — single wake rides the existing `RestartTerminal`** | RestartTerminal on a Dead+asleep terminal is EXACTLY wake once launch() clears the flag — a duplicate verb is wire noise; folder verbs are daemon-side so the drain window is SHARED (one ≤2 s pass for N terminals) and wake-all is staggered daemon-side |
| S6 | **New CtlRequest verbs: `Sleep{id,force,force_self}`, `Wake{id}`, `SleepFolder{folder,force}`, `WakeFolder{folder}` — all SCOPE_MANAGE** | Controllers need refusal semantics C2D doesn't carry (Wake refuses "not_asleep" so it can never surprise-restart a RUNNING terminal; Sleep refuses "busy" without --force); MANAGE because sleep kills processes and wake spawns them — Kill/Restart's exact class |
| S7 | **Busy gate (Ctl + GUI confirm): open block OR output within SLEEP_QUIET_MS=3000** — quiet alt-screen does NOT gate | An idle claude REPL is alt-screen and quiet (the headline sleep target — must be friction-free); a streaming claude is output-active (gated); reboot-parity: a quiet vim dies at reboot today, and sleep is a deliberate per-row click with the folder modal listing every member by name/title (§8.4) |
| S8 | **GUI confirms only when the gate trips**: single terminal → confirm modal naming the open block's cmd (or "output flowing"); folder sleep → ALWAYS a modal listing the N running terminals (name + open-block cmd/OSC title), because it's the blind bulk act. Idle sleeps are instant, zero friction | Modal-when-evidence matches ConfirmDeleteTerminal's existing pattern; a second-click-within-Ns timing window is hidden state the doctrine forbids (nothing visible explains why click #1 "didn't work") |
| S9 | **`tc run`/`SendRaw`/`SendChord` on an asleep terminal refuse code "asleep"** — no auto-wake | Auto-wake would let an INPUT-scoped token spawn processes (scope escalation) and turns a typo into a 450 MB claude resume; the refusal names the fix (`tc wake`) |
| S10 | **Recursion guard: `Sleep` joins the force_self set** (`tc sleep` inside the terminal being slept kills the caller mid-reply) | Same self-harm class as Kill/Restart/Delete; Wake needs no guard (an asleep terminal cannot host the calling process) |
| S11 | **Waiters: non-Exit waiters for the id fail code "asleep" BEFORE the kill; Exit waiters resolve naturally via on_exit** | "Your condition can only resolve after an explicit wake, unbounded" is the honest cause ("exited" would be technically true but names the mechanism, not the reason); the process genuinely exits, so Exit waiters resolving is truthful |
| S12 | **GUI keeps the attached TermBackend as-is on sleep (frozen last frame incl. a claude alt-screen TUI); no detach, no shrink in v1** | The grid IS what the user left ("come back at any given time" — the conversation stays browsable/copyable until wake, and wake's resync replaces it anyway); GUI cost is 9–45 MB/term vs the ~450–700 MB/term the kill reclaims (2–6% of the win) — the shrink lever is specced as a follow-up (§15 Q3) with measured bounds, not bundled risk |
| S13 | **Attention state clears on sleep**: NeedsYou latch, burst count, unread dot, taskbar-flash eligibility all reset; asleep terminals never enter `waiting_terminals()` | Sleeping is the user's explicit "not now" — a NeedsYou pill on an asleep row would nag about a world the user deliberately shelved |
| S14 | **Visual: crescent-moon glyph in the status-dot slot (painter-drawn, TEXT_MUTED) + dimmed name; Dead keeps its hollow ring** | Distinct-at-a-glance is the requirement; paint the crescent as circle + row-fill-colored offset circle (font-glyph ☾ risks atlas fallback holes); dimming says dormant, the ring keeps meaning "died on its own" |
| S15 | **Wake affordances: the top-bar action slot shows accent "Wake" (where Dead shows "Restore"); sidebar context menu "Wake"; hooked terminals' strip lane shows `☾ asleep` + accent `Wake ▸`; folder menu "Wake all". Click-select only VIEWS** | Every surface that today offers Restore-for-dead offers Wake-for-asleep — zero new chrome, and the strip (stationary input furniture) is exactly where an input-shaped affordance belongs; select-to-view is inv. 5 |
| S16 | **Folder semantics: Sleep all = every presented-Running member (skip dead/asleep/sleeping); Wake all = every presented-Asleep member (skip dead — dead means "died", not "shelved")** | Sleep/wake toggle INTENT, they don't repair crashes; a folder wake resurrects exactly what folder sleep suspended |
| S17 | **Folder wake staggers: daemon worker thread, RESTORE_LANES=4 / 300 ms per lane (boot's constants)** | 15 simultaneous `claude --resume` spawns are the login-storm the restore lanes were built to pace; reusing the constants inherits their measured tuning |
| S18 | **`CtlTerm.status` gains string values "asleep"/"sleeping" (and `activity` "asleep"); no CtlTerm shape change** | The status field is already an open string enum — new values are JSON-additive with zero wire risk; documented in controller-api.md so agent consumers add arms |
| S19 | **Sleep executes off the client-handler thread** (C2D → spawned `sleep-{id}` worker; Ctl → inline on the controller's own conn thread, reply after the kill is issued) | handle_client reads frames sequentially — a 2 s drain inline on the GUI's connection would freeze typing in every other terminal; a controller conn blocking only itself is the P5 composite-wait precedent |
| S20 | **journal_reap / compaction / delete: zero changes** | The reaper only removes journals whose id left state (asleep ids stay); compaction fires only inside `append()` (an asleep journal receives no appends until wake's seam); `delete_terminal_inner` already handles session-absent terminals |

---

## 2. State machine

### 2.1 Persisted + derived states

```rust
// state.rs — TerminalMeta, appended AFTER shell_cfg (bincode Snapshot wire order):
/// User put this terminal to sleep: process tree torn down, everything else
/// persisted. Survives reboots (boot restore skips it) until an explicit wake.
#[serde(default)]
pub asleep: bool,

/// Derived presentation — NEVER persisted, NEVER on the wire as an enum.
pub enum PresentedStatus { Running, Sleeping /*transient*/, Asleep, Dead }
pub fn presented_status(status: TermStatus, asleep: bool) -> PresentedStatus
```

(Running,true) = "Sleeping" exists only inside the drain window (flag saved → exit lands,
typically <1 s, ≤2 s cap) and after a power-loss inside that window the reload gives
(Dead,true) = Asleep — the intended outcome. Wake can never produce (Running,true):
launch() clears the flag in the SAME `mutate` that sets Running (S3).

### 2.2 Full lifecycle table

| Situation when Sleep is invoked | Gate verdict | What happens |
|---|---|---|
| Hooked shell at idle prompt (quiet ≥3 s, no open block) | pass | instant: flag → drain (returns in ~1 tick) → kill → Asleep |
| Claude-kind terminal, REPL idle (alt-screen, quiet) | pass | instant — THE headline case; alt-screen alone never gates (S7) |
| Open block running (`ping -t`, build, …) | gated | GUI: confirm modal naming the cmd; Ctl: refuse `busy` unless `--force`. On proceed: the block closes dangling exit=None at on_exit — the reboot-mid-command shape |
| Claude mid-response (output flowing) | gated | same as above; on proceed the stream cuts — `--resume` recovers the conversation to the last jsonl-persisted message; the in-flight turn may be partial (documented, §12) |
| Quiet alt-screen TUI (vim at rest) | pass | dies exactly as a reboot would kill it today; the folder modal lists the terminal by name/OSC title first (S7/S8) |
| ssh session | pass/gated by same rules | kill = deliberate link drop; wake = fresh `ssh` + one-shot rc (auth prompt renders honestly if keys/agent absent) — mechanically Dead+Restart, semantically "user shelved it" (§12) |
| Terminal launching (spawn in flight, not yet in sessions map) | refuse | Ctl `not_running` / GUI hides Sleep while presented ≠ Running; retry after the prompt lands |
| Already Dead | refuse | Ctl `dead`; GUI shows Restore, not Sleep |
| Already Asleep / Sleeping | no-op | Ctl `asleep`; GUI shows Wake |

| Wake path | Behavior |
|---|---|
| Click-select the row / click the grid / scroll / copy | VIEWS only — never wakes (inv. 5) |
| Top-bar "Wake" (Dead-Restore slot), sidebar menu "Wake", strip-lane `Wake ▸`, folder "Wake all" | `RestartTerminal` (single) / `WakeFolder` (bulk) |
| `tc wake <term>` | Ctl `Wake` — refuses `not_asleep` on running/dead-not-asleep terminals |
| `tc restart <term>` / GUI Restore on an asleep terminal | identical to wake (launch clears the flag) — allowed, documented |
| `tc run` / `send` on an asleep terminal | refuse code `asleep`, msg names `tc wake` (S9) |
| Boot auto-restore | SKIPS asleep terminals (S4) — they stay asleep across reboots |
| Delete | allowed as today (confirm modal); delete of an asleep terminal removes journal/sidecar/meta like any delete |

### 2.3 What the user sees during the transitions

Sleep: moon glyph appears on the flag Snapshot (sub-second), row dims, composer demotes
(draft KEPT — §7.3), grid keeps the frozen last frame (S12). No in-stream marker, no
seam text — identical philosophy to on_exit's marker-less death.

Wake: strip lane shows the existing REVEAL-gated "waking…" quiet state, the row dot goes
Working-pulse when output flows (existing activity machinery — no new spinner state);
attached GUIs get the launch() resync (Reset → Replay → … → ReplayAnchors) so the frozen
frame is replaced by the live restored world in one atomic swap. Shell prompt in ~300 ms;
a resumed claude TUI in ~1–2 s warm (§9.4).

---

## 3. Sleep flow (daemon)

```
Core::sleep_terminals(ids: Vec<Uuid>, source)      // shared: single + folder
  1. filter to presented-Running ids (state lock); refusals per §2.2
  2. mutate: for each id → t.asleep = true          // ONE save + ONE broadcast_snapshot
  3. for each id: fail_waiters_for(id, "asleep")    // non-Exit kinds only (S11)
  4. drain: poll every 25ms until EVERY target id has
       in_flight == 0 && now - last_output >= 300ms, cap 2s total
     (drain_output_tail's exact predicate, scoped to the target set — one SHARED
      window for a folder: sleeping 15 idle terminals costs one ~300ms pass, not 15)
  5. for each id: sessions.lock().get_mut(id) → killer.kill()
  6. exit-watcher per session → on_exit(id, code)   // UNCHANGED: sessions.remove
     (Session drop closes ConPTY → conhost dies → attached clients terminated —
      measured: claude.exe gone <2.5s), status=Dead, journal sync, close_dangling
      exit=None + sidecar save, resolve Exit waiters, EV_EXIT, D2C::Exited,
      broadcast_snapshot
```

- Step 2 before step 5 is load-bearing: if the daemon dies between them, boot sees
  asleep=true and skips the restore — the intent survives (§2.1).
- C2D handlers spawn a `sleep-{ids:x}` worker for steps 3–5 (S19); the Ctl handler runs
  them inline on its own conn thread and replies `Done` after step 5.
- `on_exit` needs ZERO changes: it sets Dead (correct — the process IS dead; Asleep is
  the derived presentation of Dead+flag), and its `return`-if-deleted guard already
  tolerates races.
- Kill-vs-natural-death race: if the process dies on its own between steps 1 and 5, the
  kill no-ops on a missing session — outcome is Asleep with a self-exited journal;
  indistinguishable from sleep and harmless.
- `fail_waiters_for(id, code)`: factored from the failure half of
  `resolve_exit_waiters` (waiters.rs:301–330 already fails "exited" wholesale) — same
  loop, parameterized code, Exit-kind excluded.

## 4. Wake flow (daemon) — and why it equals a boot restore

Wake is `launch(id)` with one added line (clear `asleep` inside the success `mutate`,
mod.rs ~975). Everything below is EXISTING behavior, enumerated to answer "what differs
from a boot restore" — the answer is *nothing structural*:

| launch() stage | Sleep-wake relevance |
|---|---|
| LaunchGuard coalesce | double-click Wake / wake-vs-restart race collapses to one spawn |
| meta.launch_command() | Claude-kind: pinned `--resume <uuid>` (jsonl exists ⇒ resume, else `--session-id`) — the pinned identity slept untouched |
| inner_cli wrappers (§7.4 of p6 spec) | hand-run claude/codex in pwsh/cmd/wsl/ssh resumes via the family wrapper; Ambiguous ⇒ shell + info line, never a guess |
| hook token rotation + epoch bump + close_dangling | a block left open by a forced sleep closes exit=None here if on_exit somehow didn't (belt) |
| seam append + preface_from_raw | the slept session's scrollback becomes preface, seam-concealed — "like it never went away" |
| spawn at meta.last_cols/rows | grid identical to what the user slept |
| suspend + Reset + Replay + StreamPos + Blocks + PromptState + ReplayAnchors | an attached GUI's frozen frame is atomically replaced; covers/anchors re-mint (history-parity machinery, pixel-parity probe-pinned) |

Differences from an actual boot restore, both benign and documented:
1. Wake usually happens UNDER an attached GUI ⇒ it rides the resync path instead of a
   fresh attach — the known "live resync shows the dead session's final bare prompt
   until next reopen" cosmetic residual applies (history-parity §honest-residuals).
2. No lane stagger for a single wake (direct call); folder wake re-adds it (S17).

Measured wake latency (§9.4): restart → verified hooked prompt **293 ms**; warm `claude`
launch → alt-screen TUI **~1062 ms**. Cold-machine first launches and `--resume` of a
long transcript are seconds-class (unmeasured here — flagged §15 Q6).

---

## 5. Protocol & state changes (append-only, complete list)

### 5.0 Proto coordination (binding)
Proto is 7 at research time. **sidebar-p2 is concurrently editing protocol/state and may
take 8.** This feature claims **the next number after whatever sidebar-p2 lands** (8 if
they didn't bump, 9 if they did); the constant + comment live at daemon/mod.rs run()
(`proto: 7` today, line ~2572). If both land in one install, one combined bump is fine —
what matters is: new-GUI-vs-old-daemon skew warns, and the append ORDER of enum variants
matches land order. Coordinate the C2D/D2C tail with sidebar-p2's appends explicitly at
merge time.

### 5.1 Wire (src/protocol.rs)
```rust
// C2D — APPENDED at enum END (after SubmitCommand, or after sidebar-p2's tail):
/// Put a running terminal to sleep: drain its output tail, kill its process
/// tree, keep journal/blocks/meta/pinned-CLI persisted; boot restore skips it
/// until an explicit wake. FULL-scope legacy verb (scoped controllers use
/// Ctl::Sleep). Executed on a worker thread; results arrive via the normal
/// Exited + Snapshot broadcasts.
SleepTerminal { id: Uuid },
/// Sleep every presented-Running terminal in a folder (one shared drain window).
SleepFolder { folder: Uuid },
/// Wake every presented-Asleep terminal in a folder, staggered like boot
/// restore lanes. Single-terminal wake rides RestartTerminal (launch() clears
/// the asleep flag).
WakeFolder { folder: Uuid },

// CtlRequest — APPENDED at enum END (after TokenList):
Sleep { id: Uuid, force: bool, force_self: bool },   // refuse: not_found|dead|asleep|not_running|busy(¬force)
Wake { id: Uuid },                                    // refuse: not_found|not_asleep
SleepFolder { folder: Uuid, force: bool },            // refuse: not_found; skips non-Running members
WakeFolder { folder: Uuid },                          // refuse: not_found; skips non-Asleep members
```
- `required_scope`: all four → `SCOPE_MANAGE` (S6); extend the scope-table test.
- Recursion guard (control.rs:75): add `CtlRequest::Sleep` to the force_self match arm
  (S10). `Wake`/folders excluded (cannot target the caller's live host).
- Replies: `Done` for all four (Ctl); folder verbs also `Done` when zero members matched
  (idempotent bulk semantics — the Listing says what changed).
- No new D2C variants: status flows via the existing `Snapshot` (meta.asleep rides
  TerminalMeta) and `Exited`; events via existing EV_EXIT/EV_STATE.
- `CtlTerm`: NO shape change; `status` gains "asleep"/"sleeping" values, `activity`
  gains "asleep" (S18).

### 5.2 State (src/state.rs)
- `TerminalMeta` += `#[serde(default)] pub asleep: bool` — appended AFTER `shell_cfg`
  (bincode Snapshot wire order; same-exe rationale as `hooked`/`shell_cfg`; the skew
  window is the install copy-race, already managed).
- `presented_status()` pure fn + unit table (S1).
- state.json: old→new loads (missing = false); new→old ignores unknown (no
  deny_unknown_fields anywhere).

### 5.3 Daemon (src/daemon/mod.rs)
- `sleep_terminals(ids, force_ctx)` (§3) + per-set drain helper
  `drain_targets(&HashSet<Uuid>, cap)` (extract the predicate from
  `drain_output_tail`, parameterize the session filter — Shutdown keeps its all-sessions
  call, byte-identical behavior).
- `launch()`: success-mutate adds `t.asleep = false` (S3).
- Boot restore filter += `&& !t.asleep` (S4, run() line ~2732).
- handle_message arms for the three C2D verbs (worker-thread spawn, S19).
- control.rs arms for the four Ctl verbs (busy gate per S7: any open rec OR
  `now - last_output < 3000`; `--force` bypasses; `cmd_prompt_evidence` NOT required —
  sleep is not an input verb).
- waiters.rs: `fail_waiters_for(id, code)` refactor (S11).

---

## 6. tc CLI (src/ctl.rs)

```
tc sleep <term> [--force] [--force-self]      # refuse busy without --force
tc sleep --folder <name> [--force]
tc wake  <term>
tc wake  --folder <name>
```
- Folder resolution reuses the existing `--folder <name>` machinery (list/create
  already resolve folder names — ctl.rs:864–912); exact-name match, `not_found` JSON on
  miss, same as today.
- Flags are LEADING-only per the run-grammar doctrine (these verbs take no command tail,
  so the trap class doesn't arise, but the parser style matches).
- Exit codes: standard 0/2 (ok/refused); refusal codes: `busy`, `asleep`, `not_asleep`,
  `dead`, `not_running`, `not_found`.
- `tc list`: status strings per S18 — agents filter `status == "asleep"` for a wake
  sweep. Document in docs/controller-api.md (§10 file table).

---

## 7. GUI (src/gui/)

### 7.1 Sidebar
- Terminal row: status-dot slot renders the crescent moon (painter: TEXT_MUTED filled
  circle + offset circle filled with the row's CURRENT computed fill — hover-lerped —
  never a font glyph; S14) when presented Asleep/Sleeping; name color drops to
  TEXT_MUTED; second line shows "asleep" (replacing idle_label). Dead keeps the hollow
  ring — the two must never be confusable.
- Context menu (terminal_row, mod.rs ~2883): the status match becomes presented-status:
  Running → "Sleep" (+ existing "Kill process"); Asleep → "Wake"; Dead → "Restore"
  (unchanged). "Sleep" click runs the S7 gate GUI-side: gate passes → send
  `SleepTerminal` immediately; gate trips → `Modal::ConfirmSleep(id)` naming the open
  block cmd / "output is flowing".
- Folder row context menu: append "Sleep all" (any presented-Running member) →
  `Modal::ConfirmSleepFolder(folder_id)` ALWAYS (S8) listing members (name + open-block
  cmd or OSC title, moon-marked members omitted); "Wake all" (any Asleep member) →
  `WakeFolder` directly (waking is additive — nothing can be lost — no modal).
- Folder badge: when n>0 members asleep, the folder count slot renders `☾ n` in
  TEXT_MUTED (partial-sleep at a glance; full-sleep = all members moon ⇒ same badge with
  n = member count).
- Sort: asleep rows keep their `order` (no NeedsYou bump possible — S13 clears it).

### 7.2 Top bar + central
- Bar action slot (mod.rs ~3005): presented Asleep → accent "Wake" ghost button (exact
  Restore pattern, sends RestartTerminal); Sleeping → disabled dim "sleeping…" text.
- Central grid: UNCHANGED (S12) — frozen last frame stays selectable/copyable/searchable;
  the wake resync replaces it. Claude-kind asleep terminals therefore keep showing the
  conversation TUI frame until wake (better than Dead's post-mortem primary-grid view —
  an accepted, deliberate asymmetry).
- Dashboard tiles: presented Asleep renders the moon in the tile's dot slot; preview =
  the frozen backend grid (free).

### 7.3 Composer / strip
- `RawReason::Asleep` (composer.rs Raw enum): set from drain_ipc when the presented
  status transitions to Sleeping/Asleep (the Exited arm already calls `on_exited()` —
  extend it to pick Asleep over Dead when meta.asleep). Draft is KEPT (the existing
  on_exited contract) — the user returns to their half-typed command after wake.
- `LaneContent`: Asleep arm → left lane `☾ asleep` (TEXT_MUTED) + right-cluster slot
  shows accent `Wake ▸` where Run ▸ lives (stable-chrome F3: same slot, dim-never-
  unmount). Click → RestartTerminal. During wake: existing "waking…"/quiet REVEAL rules
  (no new timer).
- Gate: `gate()` inputs already see hooked/alt/at_prompt = false on a dead world —
  Asleep behaves as Raw; on the wake resync the cold-attach PromptState path re-arms the
  composer exactly like an app open (proto-3 machinery, zero change).

### 7.4 Attention + activity (mod.rs update_activity / activity_of)
- `activity_of`: presented Asleep/Sleeping → new `Activity::Asleep` (falls between Idle
  and Dead in match order); moon rendering keys off it.
- On the transition to asleep: `st.needs_you = false; st.bursts = 0;`
  `unread.remove(id)`, `attention_flashed.remove(id)` (S13).
- `waiting_terminals()` excludes Asleep.

### 7.5 History / blocks / search — all journal-backed, verified zero-change
- History popup corpus = App.blocks, synced by the attach full-sync — asleep terminals
  are attach-able (dead-attach path: serialize_dead + Blocks full + ReplayAnchors,
  verified in the Attach handler) so their commands stay in cross-session history, dead
  or asleep, across GUI restarts.
- BlockText/ReadBlockText/ReadTail need only the journal (`block_text` takes no session
  — mod.rs:1334) — Copy output of an asleep terminal's blocks works.
- History-popup "Run" targeting an asleep terminal: gated by the existing
  `history_run_allowed` (needs Running) — the button renders dim; no new rule.
- GUI reconnect while terminals asleep: apply_snapshot attaches all (dead-attach
  machinery), ReplayAnchors re-mint covers — the reopened view of an asleep terminal is
  the standard reconstructed history (alt-screen tails reconstruct the primary grid via
  the alt-cut machinery; the frozen TUI frame is a live-GUI-session-only bonus).

---

## 8. Folder semantics (complete)

1. **Sleep all**: members with presented == Running only (S16). The confirm modal lists
   every target with name + open-block cmd (RED) or OSC title (muted); one "Sleep N
   terminals" primary action. `--force` semantics are implicit in confirming.
2. **Wake all**: members with presented == Asleep only; staggered 4 lanes × 300 ms
   daemon-side (S17). Dead members untouched (Restore is a different intent).
3. **Mixed display**: folder badge `☾ n` (§7.1); the folder dashboard shows per-tile
   moons.
4. **tc**: `tc sleep --folder <name>` / `tc wake --folder <name>` → the Ctl folder
   verbs; JSON reply `Done` (poll `tc list` for the settled state — StateChanged events
   fire per member).
5. Empty result sets are no-ops, not errors (bulk idempotence, S6 note).

---

## 9. RAM ground truth (measured 2026-07-04, this machine: 13900K/RTX 3070, Win11 26200)

### 9.1 Methodology
- **User's real processes, READ-ONLY** (`Get-CimInstance Win32_Process` for
  pid/ppid/cmdline + `Get-Process` for WorkingSet64/PrivateMemorySize64) — no process
  was touched.
- **Staged round-trip**: scratch copies of target\release exes (never lock the tree),
  isolated `TC_DATA_DIR` daemon (proto 7, pid-verified), `tc create` hooked pwsh →
  `tc run claude` (real claude.exe, zero messages sent = zero API cost, no jsonl
  written) → tree measured → `tc kill` (the sleep teardown) → survivors checked →
  `tc restart` timed → re-measured. Fully torn down afterwards; no leftovers.
- GUI numbers: live read-only observation of the user's GUI + perf-wave-3's staged
  measurements (re-staging a GUI was out of bounds: no focus-safe demo knobs exist in
  the installed build).

### 9.2 The 6 GB claim — confirmed
| Population (live) | Count | WS sum | Per-process |
|---|---|---|---|
| claude.exe | 22 | **5279 MB** | WS 184–677 MB; **private 208 MB–2114 MB** (long-lived sessions grow: typical mains 750–1400 MB priv) |
| powershell.exe hosting them | 22 | 1239 MB | WS 51–89 MB, priv 57–79 MB |
| conhost.exe (per session) | — | — | 7–17 MB each |
| **claude + shells + conhosts** | | **≈ 6.6 GB WS** | matches "~20 ≈ 6 GB" |

18 of the claude sessions live under WindowsTerminal-hosted powershells; 1 under the TC
daemon (414 MB WS / 676 MB priv) — the populations look identical per-tree.

### 9.3 Per-terminal reclaim (staged, reproduced twice within ±1 MB)
| Piece | WS | Private |
|---|---|---|
| claude.exe (fresh idle REPL) | 348 MB | 576–584 MB |
| powershell.exe (hooked) | 89 MB | 69 MB |
| conhost.exe | 8.5 MB | 1.5 MB |
| **Tree = sleep reclaim, fresh session** | **≈ 447 MB** | **≈ 650 MB** |
| Long-lived heavy session (user's live max) | up to ~700 MB | up to ~2.2 GB |
| Daemon retains per asleep terminal | ~1–2 MB (mirror+journal maps; 27.4→29 MB across the whole exercise; wave-3: 40 MB @ 20 sessions) | |

**Kill mechanism proof**: `tc kill` (TerminateProcess on the shell alone) →
claude.exe + conhost.exe + powershell.exe ALL gone <2.5 s. Session drop closes the
pseudoconsole; conhost death terminates its attached clients. **No explicit tree-kill
sweep is needed** — S2's whole-tree reclaim claim is empirical, not inferred.

**Projected: sleeping 20 claude terminals returns ≈ 9 GB WS / 13+ GB commit** (using the
user's live per-tree numbers), while the daemon keeps ~30–40 MB total.

### 9.4 Wake latency (staged)
| Step | Measured |
|---|---|
| `tc restart` → verified hooked prompt (wait-for-prompt with hooks_unverified retry) | **293 ms** |
| `claude` launch → alt-screen TUI, warm | **1062 ms** |
| cold first launch / `--resume` of a long transcript | seconds-class; unmeasured (§15 Q6) |

### 9.5 GUI-side per-terminal cost (the S12/Q3 evidence)
- User's live GUI now: 226 MB WS / 363 MB priv @ 7 attached terminals.
- perf-wave-3 (staged, 20 terminals, 61 MB journals): 352 MB WS / 520 MB priv ≈
  **9 MB/terminal** replay floor (~2050 lines × cols × 24 B cells), ceiling
  **~45 MB/terminal** when a live flood saturates the 10k ring.
- So sleep-with-kept-backend leaves 9–45 MB/terminal in the GUI — 2–6% of the ~450–700 MB
  the kill reclaims. That ratio is why S12 keeps the frame in v1 and Q3 defers the
  shrink lever (drop backend + immediate re-Attach → dead-replay floor ≈ 2–9 MB,
  machinery already exists and is pixel-parity-proven; cost = one dead-attach ~15–20 ms
  daemon-side per slept terminal).

---

## 10. File-by-file plan

| File | Changes |
|---|---|
| `src/state.rs` | `asleep` field (appended after shell_cfg) + `presented_status()` + unit table |
| `src/protocol.rs` | C2D `SleepTerminal`/`SleepFolder`/`WakeFolder` appended; CtlRequest `Sleep`/`Wake`/`SleepFolder`/`WakeFolder` appended; `required_scope` MANAGE arms + test rows; append-point comments; proto comment (coordinated bump, §5.0) |
| `src/daemon/mod.rs` | `sleep_terminals()` + `drain_targets()` (extracted predicate; Shutdown call-site unchanged); handle_message C2D arms (worker spawn); `launch()` clears `asleep`; boot filter `&& !t.asleep`; proto const bump |
| `src/daemon/control.rs` | Ctl arms + busy gate (open rec OR last_output <3000 ms; force bypass); Sleep joins the force_self guard set; refusal codes per §6 |
| `src/daemon/waiters.rs` | `fail_waiters_for(id, code)` refactor (failure half of resolve_exit_waiters, Exit-kind excluded) |
| `src/ctl.rs` | `sleep`/`wake` verbs (+`--folder`, `--force`, `--force-self`), USAGE text, refusal exit codes, parse tests |
| `src/gui/mod.rs` | presented-status plumbing; moon glyph (Icon::Moon painter arm); row/folder context-menu items; `Modal::ConfirmSleep`/`ConfirmSleepFolder`; bar Wake slot; folder `☾ n` badge; `Activity::Asleep` + S13 attention clearing; dashboards |
| `src/gui/composer.rs` | `RawReason::Asleep`; LaneContent Asleep arm (`☾ asleep` + `Wake ▸`); on_exited picks Asleep when flagged; draft-kept unchanged |
| `src/gui/term_backend.rs` | NOTHING (S12) — verify-only |
| `src/probe.rs` | 3 cases §11 + skip-free (no external deps) |
| `docs/controller-api.md` | status/activity new string values; sleep/wake verbs, refusal codes |
| `docs/sleep-spec.md` | this file |

Nothing touches: session.rs (spawn/reader/ingest/exit-watcher), journal.rs, blocks.rs,
serialize.rs, anchors.rs, tracker.rs, bootstrap.rs, term_view.rs — the restore machinery
is consumed, not modified (S3/S20).

---

## 11. Probes & tests

Probes (suite 44 → 47; all run against the isolated TC_DATA_DIR daemon, PowerShell
Start-Process context per the harness doctrine):

- **P-S1 `sleep_roundtrip`** (the acceptance case): create hooked pwsh → prompt → run a
  short command (block rec exists) → `Ctl Sleep` → assert: Listing status "asleep";
  root shell pid + its conhost GONE from a Toolhelp snapshot (the RAM-reclaim proxy —
  process absence is deterministic where RSS is noise); journal file + sidecar intact
  byte-for-byte (hash before/after); state.json `asleep: true`. Then daemon
  Shutdown + respawn → assert the asleep terminal did NOT restore (status stays
  asleep-dead) while a sibling auto_restore terminal DID (the S4 skip, both polarities).
  Then `Ctl Wake` → wait-for-prompt (hooks_unverified retry loop, ≤10 s) → ReadBlocks
  returns the pre-sleep rec (history survived) → fresh attach_view sees
  Replay→StreamPos→Blocks→PromptState→ReplayAnchors with a block hint for the pre-sleep
  command (covers re-mint). Assert wake-to-prompt ≤5 s.
- **P-S2 `sleep_busy_gate`**: raw-type `ping -t` (open block) → `Ctl Sleep` no-force ⇒
  refuse `busy`; `--force` ⇒ sleeps; wake ⇒ the dangling block closed exit=None
  (reboot-parity). Also `Ctl Wake` on the RUNNING sibling ⇒ `not_asleep`; `Ctl Run` on
  the asleep terminal ⇒ `asleep`.
- **P-S3 `sleep_waiters_folder`**: register `Wait{BlockClose}` + `Wait{Exit}` →
  `Ctl SleepFolder --force` on a 2-terminal folder ⇒ BlockClose waiter fails code
  "asleep", Exit waiter resolves Exited; both terminals asleep off ONE drain window
  (wall-clock assert < 2×2 s cap); `WakeFolder` ⇒ both prompts return; the folder's
  dead (non-asleep) third member untouched.

Cargo tests: `presented_status` table; boot-filter skip (state-level: asleep +
auto_restore + launched_once ⇒ not enqueued); `required_scope` rows; ctl parse rows
(`sleep --folder`, flag-looking term ref ⇒ usage); recursion-guard row for Sleep;
`fail_waiters_for` excludes Exit kind; GUI: presented-status → menu-item table,
Activity::Asleep clears needs_you/bursts (sim_frame-style walk), lane_content Asleep arm.

RAM assertion policy: probes assert PROCESS ABSENCE, not RSS bands (machine-dependent by
design — the flood-CPU precedent); the §9 numbers live in this spec, not in probe code.

---

## 12. Degraded modes & edge table

| Edge | Behavior (all honest, none silent) |
|---|---|
| Sleep mid-claude-response | gated (S7/S8); on proceed the in-flight turn is cut — `--resume` recovers to the last jsonl-persisted message; scrollback keeps what streamed. Stated in the confirm modal copy |
| ssh asleep vs ssh dead | mechanically identical (link killed, wake = fresh ssh + one-shot rc + keepalives); SEMANTIC difference is user intent: asleep ⇒ boot-skip + moon + Wake affordance; dead ⇒ auto_restore honors + ring + Restore. This intent bit is the entire reason Asleep exists as a state |
| ssh wake without keys/agent | auth prompt renders raw in the terminal (composer stays Raw(NoPrompt) — the §3.4.5 gating), exactly like boot restore today |
| Sleep during `tc run --wait` composite | the RunDone waiter fails "asleep" (S11) — the caller knows precisely why |
| `tc wait --for exit` then sleep | resolves `Exited` (truthful — the process exited) |
| Sleep while launching (LaunchGuard held / session not in map) | refuse `not_running`; GUI hides Sleep until Running |
| Wake-vs-wake / wake-vs-restart race | LaunchGuard coalesces (existing) |
| Sleep-vs-delete race | delete wins idempotently (delete_terminal_inner kills + removes; on_exit's deleted-guard returns) |
| Power loss between flag-save and kill | boot loads asleep=true ⇒ skipped ⇒ Asleep — intended outcome |
| Power loss during folder wake stagger | woken members restored Running; un-woken keep asleep=true ⇒ still asleep after reboot; re-run Wake all |
| Blind `SetAutoRestore` on an asleep terminal | allowed; persists; still skipped while asleep (documented: asleep wins, auto_restore applies after wake — inv. 6) |
| GUI reconnect / restart with asleep terminals | dead-attach machinery: serialize_dead replay (alt-cut safe) + Blocks full + ReplayAnchors covers — full history view, search, copy (§7.5) |
| History popup / BlockText / ReadTail on asleep | journal-backed, session-free — verified zero-change (§7.5) |
| Asleep terminal's frozen alt-frame vs reopen | live GUI keeps the TUI frame until wake; a reopened GUI shows the reconstructed primary grid (existing alt-cut contract) — asymmetry documented, not a bug |
| NeedsYou/burst at sleep instant | cleared (S13); never latches again while asleep (no output can arrive) |
| Multi-GUI | flag rides Snapshot — all clients converge; sleep from GUI A dims the row in GUI B |
| cmd/wsl family sleep | family-agnostic: drain+kill+launch are family-blind; wake re-injects PROMPT env / rcfile per family (launch() synthesis) |
| Sleeping terminal's journal compaction / reap | impossible while asleep (no appends; id in state) — S20 |
| `tc sleep` inside its own terminal | recursion guard: refuse unless `--force-self` (S10) |

---

## 13. Perf budget

- Sleep: flag mutate+save ~1 ms; drain typically one 25 ms tick for an idle terminal
  (its last output is minutes old), ≤2 s cap under flood; kill + exit-watcher + on_exit
  ≈ single-digit ms + one journal fsync. Folder-of-15 idle: one shared ~300 ms-quiet
  check window, wall <1 s.
- Wake: launch ~50 ms (spawn+preface) + shell init ~250 ms (§9.4); claude TUI +~1 s
  warm. Folder wake: lanes 4×300 ms — 15 terminals ≈ 1.4 s to all-spawned (boot-restore
  band).
- Idle cost of the feature: ZERO (no polling; the flag is a bool in structs already
  broadcast).
- GUI: one Snapshot + one Exited per slept terminal; the moon/badge is painter work in
  existing passes.

---

## 14. Creation-order / phasing

Single shippable pass (the machinery is all reuse): state+protocol → daemon
(sleep_terminals/drain/launch-clear/boot-skip/ctl) → tc verbs → GUI (menus, modal, moon,
lane, attention) → probes → docs. No sub-phase is user-visible alone; land whole behind
the normal suite + probe bar.

---

## 15. Open questions (defaults chosen — implementer may proceed on defaults)

| # | Question | Default (justified) |
|---|---|---|
| Q1 | Typing into a focused asleep terminal's composer: queue-and-wake? | NO — draft accumulates locally (kept on wake); input never spawns processes (inv. 5). Revisit only with explicit user ask |
| Q2 | Folder-wake stagger constants | RESTORE_LANES=4 / 300 ms (S17); make them shared consts with boot rather than new knobs |
| Q3 | GUI backend shrink-on-sleep (drop + re-Attach → 2–9 MB floor vs 9–45 MB kept) | OFF in v1 (S12); ship as a follow-up if 20-asleep GUI RSS annoys — the machinery (dead-attach + ReplayAnchors) needs zero new code, only the drop call |
| Q4 | Expose "sleeping" transient in CtlTerm.status | YES ("sleeping") — it is observable truth and costs a string |
| Q5 | Pre-fail waiters "asleep" vs letting on_exit fail "exited" | "asleep" (S11) — cause over mechanism; one small refactor |
| Q6 | Measure `claude --resume` latency on a long transcript + cold start | Unmeasured here (would need a message-bearing session = API cost); record on first real use — does not gate the design (wake is async and visibly progressive) |
| Q7 | Auto-sleep policy (sleep after N hours idle) | OUT of v1; the folder verb makes manual bulk cheap. Design later atop the same sleep_terminals() |

---

## 16. DO-NOTs (each protects a measured or probe-pinned behavior)

1. **DO NOT skip the drain before the kill** — the async-conhost journal-truncation
   class (`restore_fidelity`) will silently eat command tails from the persisted
   history.
2. **DO NOT persist a third TermStatus variant** — `SharedState::load()` force-resets
   status to Dead (state.rs:412) and would erase Asleep at every boot; the flag design
   (S1) exists because of this exact line.
3. **DO NOT modify on_exit for sleep** — its Dead-marking, dangling-close, waiter and
   broadcast sequence is shared with real deaths and probe-pinned; sleep's identity
   lives entirely in the pre-set flag.
4. **DO NOT auto-wake on select/click/run** (inv. 5) — misclicks must stay free;
   `tc run` refusing "asleep" also keeps INPUT-scoped tokens from spawning processes.
5. **DO NOT fan folder sleep out as N GUI-side SleepTerminal sends** — you lose the
   shared drain window and the single Snapshot; the daemon owns bulk.
6. **DO NOT run the drain on the client-handler thread** (S19) — a 2 s inline drain
   freezes every other terminal's Input on that connection.
7. **DO NOT touch the mirror/journal/PTY in the sleep path** (inv. 3) — the wake-time
   seam is launch()'s job; a sleep-time marker would violate marker-less-death parity
   and mirror purity.
8. **DO NOT add an explicit process-tree sweep to the kill** — measured: the ConPTY
   close already terminates the tree (<2.5 s incl. claude.exe); a Toolhelp
   TerminateProcess sweep re-introduces the kill-wrong-pid class for zero benefit.
9. **DO NOT gate sleep on alt-screen** — the idle claude REPL (the headline case) is
   alt+quiet; the S7 gate (open block / recent output) is the correct busy signal.
10. **DO NOT reuse the Dead ring for Asleep** — "died on its own" vs "I shelved it" is
    the semantic the whole feature adds; visual conflation erases it.
11. **DO NOT bump proto without coordinating with sidebar-p2** (§5.0) — two features
    appending enum variants in the same window must agree on land order; bincode is
    positional.
12. **DO NOT assert RSS bands in probes** — process absence is the deterministic
    reclaim proxy; RSS numbers live in this spec with their methodology (§9), machine-
    dependent by design.
