# SSH Drag-Drop Upload (#26) — Implementation Spec (final, implementation-ready)

Target: C:\Terminal Control (Rust daemon + egui 0.35 GUI + tc.exe, proto 7 at research
time). **This feature is GUI-only: ZERO wire changes, zero daemon changes, zero
protocol.rs / state.rs-persistence edits.** The upload rides child processes of the GUI;
the daemon never hears about it.

User requirement (verbatim): drop local file(s) on an ssh terminal → upload to the
remote via scp/sftp → paste the REMOTE path(s), "100% done". Plus a first-use consent
dialog ("This will copy the file to <host> via scp, to <remote location>, then paste the
path. Continue?" — [Continue] [Cancel] + "Never show this again" persisted) and a toast
on EVERY failure — distinct plain-language message + filename — built as the app's FIRST
toast surface, a small reusable component that #25 (attention toast) consumes later.

Binding interface inherited from docs/qol-spec.md §4.3/§4.6 (BINDING): `route_file_drop`
has exactly ONE `Ssh` match arm reserved for this feature. This spec replaces that arm's
BODY and inherits: the `paths: Vec<PathBuf>`, the terminal id, the composer-vs-raw mode
decision, and the hover tint/label surface (§4.7). Nothing else may branch on Ssh.

All research below was verified end-to-end on this machine (Windows 11 26200, OpenSSH_for_
Windows_9.5p2) against a REAL Linux sftp-server 9.6p1 staged in WSL /tmp via
`sftp.exe -D` (transport-bypass stand-in, same philosophy as TC_SSH_VIA_WSL), plus real
ssh-transport failure captures against github.com / non-routable IPs. Every stderr shape
in §7 is a real captured output, not documentation hearsay.

Ordered: invariants → decisions → transport → consent → toast component → upload
pipeline → failure/toast table → paste → edges → files → tests/staging → open questions
→ DO-NOTs.

---

## 0. Non-negotiable invariants

1. **GUI-only**: uploads are `std::process` children of the GUI (the drop is a GUI
   gesture; sftp must read the USER's `~/.ssh` — the GUI runs as the user; the daemon
   stays wire-frozen). GUI exit kills running uploads (§6.8) — documented, honest.
2. **Never touch the user's ssh trust or config**: no `StrictHostKeyChecking=accept-new`,
   no known_hosts writes, no config writes. The session already connected ⇒ the host key
   is already in `~/.ssh/known_hosts` (verified: the author's test host 192.0.2.14 has
   ed25519/rsa/ecdsa lines there, plain-text, shared by ssh and sftp by default). A host
   that is NOT yet trusted fails honestly (§7 row 6).
3. **Non-interactive always**: `-o BatchMode=yes` is PREPENDED (beats any user
   `-o BatchMode=no`; first-occurrence-wins, same rule P6c pinned for keepalives). A
   hidden child can never answer a prompt — a password host must fail instantly with the
   §7 row-4 toast, never hang.
4. **Pointer never disarms the composer** (P3 contract, qol inv. 3): consent dialog,
   toasts, and drops are pointer acts. Only the final PASTE routes as input, through the
   existing router semantics.
5. **Never guess**: paste fires ONLY for files POSITIVELY verified uploaded (name+size
   in the `ls -l` tail, §6.5) — exit codes and stderr silence are not proof. A wrong
   remote path pasted into a shell is worse than no paste.
6. **Never auto-delete user files on the remote**: `~/.tc-drops` is append-only from our
   side. The ONE exception: names THIS job created and did not verify complete
   (failed/cancelled partials) are best-effort `-rm`'d (§6.7) — they are our garbage,
   created seconds ago, never pasted.
7. **UX doctrine** (ux-doctrine.md): zero strokes, hover-reveal, auto-dismiss +
   hover-to-hold, toast never steals focus, no success confetti — a successful drop's
   feedback IS the pasted path appearing.
8. **Toast is a component, not a feature**: minimal API sized for #26 + the #25 seam
   (§5). No speculative options.
9. **mirror purity / journals / blocks: untouched** — upload bytes never enter the PTY
   stream; only the paste does, via existing input paths.

---

## 1. Headline decisions

| # | Decision | One-line justification |
|---|---|---|
| T1 | **Transport = `sftp.exe`, batch mode, both legs** — never scp | scp 9.5 is SFTP-backed anyway (same protocol), but sftp batch adds what scp lacks: `mkdir`, `ls` (collision probe + success verification), per-command `-` error control — scp has zero of those; speed identical |
| T2 | **Two connections per drop batch**: probe (`pwd` + `-mkdir .tc-drops` + `ls -1 .tc-drops`) then upload (`-put`×N + `ls -l .tc-drops` tail) | Windows sftp.exe stdout is FULLY BUFFERED over pipes (proven §9.2 — an interactive request/response driver is impossible); two parse-after-exit connections need no streaming and stay dumb-simple; ~0.3-0.6s extra on a LAN, once per drop |
| T3 | **Batch via temp FILE (`-b <file>`), not stdin** | eliminates the stdin pipe (no write-deadlock analysis, no encoding traps); every trial in §9 ran this way; file is UTF-8 no-BOM, LF, deleted after |
| T4 | sftp.exe resolved as **SIBLING of the session's resolved ssh** (fall back to PATH) | keeps client config/known_hosts semantics identical to the session's ssh; sftp.exe finds its ssh.exe by CreateProcess app-dir search (PROVEN: lone sftp.exe fails `posix_spawn: No such file or directory`; with sibling ssh.exe it connects) |
| T5 | Argv = `-o BatchMode=yes` + TRANSLATED user flags (§3.2) + our appended defaults (`ConnectTimeout=10`, ServerAlive 15/3) + destination VERBATIM | destination verbatim ⇒ `~/.ssh/config` Host aliases resolve identically (user has three alias entries today); first-occurrence-wins ⇒ prepended BatchMode is ours, appended timeouts are user-overridable — same rule as P6c keepalives |
| T6 | Landing dir = **`~/.tc-drops/`**, flat, created per-drop with ignore-error `-mkdir` | one predictable place, named in the consent dialog; flat because collision suffixing already disambiguates; `-mkdir` on existing dir is a no-op (proven: stderr noise, exit 0) |
| T7 | Collision naming: keep original filename; on collision append `-2`, `-3`… before the extension (`shot.png` → `shot-2.png`), against `ls -1` ∪ names already chosen in this batch | original names are what claude reads best (user's stated purpose); timestamp prefixes wreck readability; the ls is free — conn 1 already exists for `pwd`+mkdir |
| T8 | **Success = name+size match in conn 2's `ls -l` tail** — not exit codes | `-put` (ignore-prefix) keeps the batch going after a per-file failure but then exit is 0 (proven §9.6) — the ls tail is positive per-file ground truth; sizes proven parseable |
| T9 | Progress UX = **indeterminate spinner in the toast** + filename(s); no percent | sftp shows no progress meter without a tty (proven: nothing on pipes), and stdout buffering hides mid-run output anyway; a spinner + cancel is honest |
| T10 | Paste = remote ABSOLUTE paths (`<home>/.tc-drops/<name>`), POSIX single-quoted, space-separated, trailing space, routed by the qol router AT COMPLETION TIME | home captured from conn 1's `pwd` (`Remote working directory: /home/x`) — absolute + single-quoted works in every remote shell (bash/zsh/fish/sh), no `~`-expansion quoting trap; completion-time routing because the world changes during a multi-second upload |
| T11 | Partial batch: **paste all verified files, one Error toast itemizing the rest** | the successes are already on the host — withholding them wastes the upload; the toast names each failure with its reason; drop order preserved |
| T12 | Consent = modal on first-ever ssh drop (per user's verbatim copy), "Never show this again" = **GLOBAL** `Prefs.ssh_drop_skip_consent` | the dialog teaches a semantic ("drops upload then paste"), learned once — per-host would re-nag the same lesson; the host is restated in every progress toast anyway |
| T13 | Uploads queue **sequentially per terminal**, parallel across terminals | paste order must equal drop order per terminal; cross-terminal independence is free |
| T14 | No size-threshold confirm dialog | the consent dialog covers semantics; the progress toast + cancel IS the affordance for a slow accidental drop; a second modal is bloat (§11 Q3) |
| T15 | Cancel = toast ✕ → `TerminateProcess` the child → best-effort cleanup of this job's unverified names → "upload cancelled — nothing pasted" | clean semantics: cancelled ⇒ zero side effects the user can observe; names were ours this job (inv. 6) |
| T16 | New pure logic in **new file `src/gui/ssh_drop.rs`**, toast component in **new file `src/gui/toast.rs`** | sidebar-p2 is live in mod.rs/composer.rs — new files minimize churn contact (qol precedent); mod.rs gets only the localized wiring (§8) |

---

## 2. Environment facts (evidence, all verified on this machine)

- `C:\Windows\System32\OpenSSH\` ships `ssh.exe`, `scp.exe`, `sftp.exe` (+keygen/agent
  tools), version **OpenSSH_for_Windows_9.5p2, LibreSSL 3.8.2**. No sftp-server.exe (the
  Server capability is separate). The OpenSSH **Client** is an on-by-default optional
  capability — it CAN be absent (taxonomy row 3).
- scp 9.5 usage: `[-346ABCOpqRrsTv] … [-X sftp_option]` — `-O` = legacy SCP protocol
  opt-in ⇒ **default is the SFTP protocol**. scp offers no mkdir/ls/verification ⇒ T1.
- sftp usage: `[-46AaCfNpqrv] [-B buffer] [-b batchfile] … [-D sftp_server_command] …
  [-P port] … destination`. `-P` is PORT (vs ssh `-p`); `-l` is BANDWIDTH LIMIT (vs ssh
  login user) — the two renames the translator must handle.
- **Batch semantics** (all proven §9): `-b file` aborts at the first failing command,
  exit 1; a `-` prefix (`-mkdir`, `-put`) ignores that command's failure (stderr still
  printed, exit stays 0); command echoes (`sftp> …`) go to stdout, errors to stderr.
- **Windows path handling**: local paths in `put` may be written with backslashes inside
  double quotes — the port normalizes to `/` (echo shows the converted form). Spaces and
  unicode (é) round-trip correctly with a UTF-8 no-BOM batch file. Glob-hot bracket
  names are SAFE on this build: `put "…\shot [1].png"` uploaded the right file even with
  a sibling `shot 1.png` present (adversarial character-class test).
- **stdout is fully buffered over pipes** (§9.2) ⇒ no interactive driving; stdin CAN be
  streamed (commands execute as they arrive) but there is no reason to (T3).
- `pwd` prints `Remote working directory: /path` — the parse anchor for `<home>`.
- The user's `~/.ssh/config` has 3 Host aliases (one with a different HostName) — alias
  destinations must reach sftp VERBATIM so config resolution matches the session (T5).
- The author's test sessions are `ssh.exe 192.0.2.14` (bare host, no flags, key auth
  via default `~/.ssh/id_rsa`; host key present in known_hosts) — the zero-translation
  common case.
- known_hosts is SHARED: sftp/scp run ssh's own client code; default
  `%USERPROFILE%\.ssh\known_hosts` (+ `%ProgramData%\ssh\ssh_known_hosts` global) —
  no flag needed for trust to carry (inv. 2).

---

## 3. Transport

### 3.1 Connection legs

**Conn 1 — probe** (batch file):
```
pwd
-mkdir .tc-drops
ls -1 .tc-drops
```
Parse after exit: home dir from the `Remote working directory: ` line; existing names
from `ls -1` lines (strip the `.tc-drops/` prefix; skip `sftp> ` echo lines). Exit 255 ⇒
connection-class failure (classify stderr, §7 rows 1-6, one toast, STOP). Exit 1 ⇒ the
`ls` failed ⇒ `.tc-drops` could not be created (mkdir was ignore-prefixed) ⇒ §7 row 7
(`Can't ls: "<home>/.tc-drops" not found` captured). Exit 0 ⇒ proceed.

**Conn 2 — upload** (batch file, after name resolution §3.3):
```
-put "C:/…/local one.png" ".tc-drops/final one.png"
-put "C:/…/local two.txt" ".tc-drops/final two.txt"
ls -l .tc-drops
```
`-put` so one bad file doesn't abort the rest (T11). Verify each file by name+size in
the `ls -l` tail (§6.5). Exit 255 ⇒ connection died mid-upload (§7 row 11).

**Conn 3 — cleanup** (only after failures/cancel, best-effort, never toasted itself):
```
-rm ".tc-drops/final two.txt"        # every name this job chose but did not verify
```

### 3.2 Argv synthesis — `ssh_drop::sftp_args(meta_args, extra) -> Vec<String>`

Input = the session's PERSISTED `meta.args` (user flags + destination — the synthesized
`-t`/keepalive/remote-command tail is per-spawn and never persisted, so it never reaches
this function). Destination = `state::ssh_destination(meta_args)` (pub, shared).

```
[ "-q", "-o", "BatchMode=yes" ]                  # ours, PREPENDED (inv. 3)
+ translated user flags (order preserved):
    -p X | -pX      → -P X | -PX                 # port: renamed by scp/sftp
    -l X            → fold: destination becomes "X@dest" (skip if dest has '@')
    -b X            → -o BindAddress=X           # sftp has no -b (that's buffer-file)
    -B X            → -o BindInterface=X         # sftp -B is buffer size
    -m X            → -o MACs=X                  # sftp has no -m
    -i X, -F X, -J X, -o X, -c X, -4, -6, -C     → carried verbatim
    -t -T -N -f -G -K -k -M -a -A -x -X -Y -g -n -q -v -e X -E X -Q X -O X
    -L X -R X -D X -w X -W X -S X                → DROPPED (tty/forwarding/mux/query
                                                   flags are session-shaped; Windows
                                                   OpenSSH has no mux so -S never works
                                                   anyway)
+ [ "-o", "ConnectTimeout=10",
    "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3" ]   # appended AFTER
                                                   # user flags: first-wins ⇒ user
                                                   # overrides automatic (P6c rule)
+ [ destination ]                                 # verbatim (aliases/config resolve)
+ [ "-b", <batch file path> ]                     # order-free; put with flags for
                                                   # clarity anywhere before destination
```
Destination scheme rewrite: `ssh://…` → `sftp://…` (sftp rejects ssh:// — proven:
resolves literal host "ssh"); everything else untouched. Only flags reachable through
`shell_family == Ssh` need handling — exotic flags outside `state.rs::VALUE_FLAGS`
already classified the session `Other` (never hooked, never Ssh-dropped).

Golden (the author's test host): `["192.0.2.14"]` →
`-q -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15
-o ServerAliveCountMax=3 192.0.2.14 -b <file>`.

### 3.3 Name resolution (pure, golden-tested)

`resolve_names(dropped: &[PathBuf], existing: &HashSet<String>) -> Vec<(PathBuf, String)>`
- final name = original filename; if taken (existing ∪ chosen-so-far), try
  `stem-2.ext`, `-3`, … to `-99`, then `stem-<unix-secs>.ext` (never fails).
- Files whose names are not valid Unicode (unpaired surrogates) or empty → refused
  pre-flight with §7 row 9 (path-less oddity, effectively never happens).
- Directories dropped → refused pre-flight with their own toast line ("folders can't be
  uploaded (v1)") — sftp `put` of a dir needs `-R` + remote mkdir tree; out of v1 scope
  (§11 Q4). Local drops treat dirs as paths (qol) — the asymmetry is stated in the toast.

### 3.4 Spawn hygiene

- `Command::new(<sftp path>)` with `creation_flags(CREATE_NO_WINDOW /*0x0800_0000*/)`
  (std `CommandExt`; the GUI is windows-subsystem — a console child would flash a window
  otherwise; main.rs:107 precedent uses DETACHED_PROCESS for the daemon, but for a
  piped, waited child CREATE_NO_WINDOW is the correct flag; the ssh.exe that sftp spawns
  inherits no-console harmlessly — BatchMode needs no console).
- stdin null, stdout+stderr piped, `wait_with_output()` (std drains both concurrently —
  no pipe deadlock; `ls` of a 10k-file dir is fine).
- `child.id()` stashed pre-wait for cancel (§6.6).
- sftp path resolution: directory of the session's resolved ssh (re-run the
  `resolve_program` logic GUI-side on `meta.program`, or simply: if `meta.program` has a
  path component use its parent, else walk PATH like resolve_program does) → join
  `sftp.exe` → `is_file()` else PATH-search `sftp.exe` → else §7 row 3 toast. (T4
  guarantees the sibling pair actually cooperates.)

---

## 4. Consent dialog (first ssh drop)

- New `Modal::SshDropConsent` arm + `App.pending_ssh_drop: Option<PendingSshDrop>`
  (`{ terminal: Uuid, paths: Vec<PathBuf>, dont_ask_again: bool }`).
- The Ssh arm of `route_file_drop`: if `prefs.ssh_drop_skip_consent` ⇒ enqueue directly;
  else stash `PendingSshDrop` + open the modal. While ANY modal is open, new drops
  no-op (rare; the tint label still renders).
- Rendering: the existing `show_dialog` pattern (mod.rs:5672 — Modal + 0.12s fade,
  `primary_button` Continue, `ghost_button_auto` Cancel; ConfirmDelete twins at
  mod.rs:5394/5430 are the copy-paste template). Width 460.
- Copy (exact strings; `{host}` = destination verbatim, `{n}`/`{name}` per count):

  Title: `Upload to {host}?`

  Body (1 file): `This will copy "{name}" to {host} over SFTP, into ~/.tc-drops on that
  host, then paste the remote path into the terminal. Continue?`

  Body (N files): `This will copy {n} files to {host} over SFTP, into ~/.tc-drops on
  that host, then paste the remote paths into the terminal. Continue?`
  + up to 5 filenames listed below in TEXT_MUTED 12px (middle-ellipsized ~44 chars,
  existing `ellipsize` helpers), then `+ {n-5} more` if over.

  Checkbox: `Never show this again` (egui `ui.checkbox`, precedent mod.rs:3662) →
  `pending.dont_ask_again`.

  [Continue] ⇒ if `dont_ask_again` { `prefs.ssh_drop_skip_consent = true; save_prefs()` }
  then enqueue the pending job. [Cancel]/Esc ⇒ drop the pending job silently — no
  upload, nothing pasted, no toast (the user just said no; a toast would nag).

  (The user's verbatim said "via scp"; the dialog says "over SFTP" because that is what
  actually runs — §11 Q1 records the wording call. The consent text names `~/.tc-drops`
  per the user's `<remote location>` requirement; cleanup policy is documented there
  implicitly: files stay until the user deletes them.)

- `Prefs` gains `#[serde(default)] pub ssh_drop_skip_consent: bool` (gui.json via
  `prefs_path()` mod.rs:538 — GUI-local, zero wire impact). Consent scope GLOBAL (T12).
- Consent covers the WHOLE batch of the drop that raised it (edge: multi-file drop =
  one dialog). It is asked BEFORE conn 1 — nothing connects until Continue.

---

## 5. Toast component — new `src/gui/toast.rs` (the app's first toast surface)

Sized for #26 + the #25 reuse seam. Nothing else.

### 5.1 API

```rust
pub type ToastId = u64;

pub enum ToastKind { Progress, Error, Info }

pub enum ToastAction {                    // returned from show(); consumer dispatches
    CancelUpload(u64 /*job id*/),         // #26
    FocusTerminal(Uuid),                  // #25 seam (unused by #26)
}

pub struct Toast {
    pub kind: ToastKind,
    pub title: String,                    // 13px TEXT, single line, ellipsized
    pub detail: Vec<String>,              // 12px TEXT_SECONDARY lines (per-file rows)
    pub ttl: Option<Duration>,            // None = sticky (Progress); Some = auto-dismiss
    pub action: Option<ToastAction>,      // Progress: ✕ = action; Info: click = action
}

pub struct Toasts { /* items: Vec<(ToastId, Toast, Instant)>, next_id, hovered pause */ }
impl Toasts {
    pub fn push(&mut self, t: Toast) -> ToastId;          // cap 4 visible: evict oldest
                                                          // non-Progress first
    pub fn update(&mut self, id: ToastId, f: impl FnOnce(&mut Toast)); // morph in place
    pub fn dismiss(&mut self, id: ToastId);
    pub fn show(&mut self, ctx: &egui::Context, anchor: Rect) -> Option<ToastAction>;
}
```

### 5.2 Rendering (doctrine-compliant, mirrors the launcher's surface grammar)

- One `egui::Area` per toast, `Order::Foreground`, anchored BOTTOM-RIGHT inside
  `App.central_rect` (mod.rs:619, set at 4157): 12px right/bottom margin, lifted by
  `composer::STRIP_H` (composer.rs:31) when the shown terminal reserves the strip;
  stacked upward, 8px gaps, width min(320, central.width-24). Toasts render regardless
  of CentralView (uploads outlive view switches).
- Frame: `fill(SURFACE)`, `corner_radius(10)`, launcher shadow (offset [0,6], blur 28,
  `from_black_alpha(150)`), **zero stroke** (launcher idiom, mod.rs:5326-5334).
- Leading glyph, painter-drawn (no icon fonts, D35 rule): Progress = 12px arc spinner
  (rotate by `ui.input(|i| i.time)`; while any Progress toast lives, show() calls
  `ctx.request_repaint_after(Duration::from_millis(100))`); Error = 6px DANGER dot;
  Info = 6px ACCENT dot. Signal, not decoration.
- Fade in/out ≤120ms: `ctx.animate_bool_with_time(toast_id, …, 0.12)` +
  `multiply_opacity` (show_dialog idiom mod.rs:5680-5686).
- ttl countdown PAUSES while the pointer hovers the toast (hover-to-hold); a ghost ✕
  (existing `Icon::Close` painter) appears on hover top-right = dismiss (Error/Info) or
  `action` (Progress ⇒ CancelUpload).
- NEVER steals focus: no focusable widgets — raw painter + click hit-tests only
  (blocks-toolbar pattern); an egui Area takes no keyboard focus by itself.
- Defaults #26 uses: Progress `{ttl: None}`; Error `{ttl: Some(8s)}`; Info
  `{ttl: Some(5s)}`.

App wiring: `App.toasts: Toasts`; `self.toasts.show(ctx, central)` called once at the
end of `ui()` after the central panel (so it paints over everything except modals).

---

## 6. Upload pipeline — `src/gui/ssh_drop.rs`

### 6.1 Shape

```rust
pub struct Uploads {
    queues: HashMap<Uuid, VecDeque<Job>>,      // per-terminal FIFO (T13)
    running: HashMap<Uuid, RunningJob>,        // ≤1 per terminal
    events_rx: Receiver<Event>, events_tx: Sender<Event>,   // std::sync::mpsc
    next_job: u64,
}
struct RunningJob { job_id: u64, toast: ToastId, cancel: Arc<AtomicBool>,
                    child_pid: Arc<Mutex<Option<u32>>>, files: Vec<PathBuf> }
enum Event {                                   // worker → GUI
    Done { terminal: Uuid, job_id: u64, home: String,
           verdicts: Vec<(PathBuf, Result<String /*final name*/, FileErr>)> },
    ConnFailed { terminal: Uuid, job_id: u64, err: ConnErr },
    Cancelled { terminal: Uuid, job_id: u64 },
}
```

### 6.2 Enqueue (from the Ssh arm / consent Continue)

Push a Job; push a Progress toast: title `uploading {name} to {host}…` (1 file) /
`uploading {n} files to {host}…`, detail = filenames (≤4 + "+n more"), action
CancelUpload. If a job is already running for that terminal, title prefix `queued — `
until it starts. Start the worker if none running for the terminal.

### 6.3 Worker thread (std thread per running job; drop cadence — no pool needed)

The GUI has no async runtime; this mirrors the ipc reader pattern (ipc.rs:207 mpsc +
`ctx.request_repaint()` on send — pass a `Context` clone in). Steps:

1. Resolve sftp path (§3.4) — failure ⇒ `ConnFailed(SftpMissing)`.
2. Pre-flight each local file (`File::open` + metadata size) — unreadable ⇒ that file's
   verdict is `FileErr::LocalUnreadable` up front (still uploads the rest).
3. Write conn-1 batch temp file (`std::env::temp_dir()\tc-drop-<job>.1`, UTF-8, LF);
   spawn (§3.4); `wait_with_output()`. Classify: spawn error ⇒ SftpMissing; exit 255 ⇒
   stderr → ConnErr (§7 rows 1-6); exit 1 ⇒ MkdirDenied (row 7); else parse home +
   existing names.
4. `resolve_names` (§3.3); write conn-2 batch (`-put` per remaining file + `ls -l
   .tc-drops`); spawn; store pid; wait. Exit 255 ⇒ ConnFailed(Dropped) (row 11 — but
   files verified by a partial ls can't exist since ls is the tail; ALL files of this
   conn get "connection lost").
5. Verdicts: for each (local, final_name): success iff the ls -l tail has final_name
   with size == local size (regex
   `^\S+\s+\S+\s+\S+\s+\S+\s+(\d+)\s+\S+\s+\d+\s+\S+\s+(.+)$` on non-echo lines —
   date field 8 is `2026` or `04:08`, both `\S+`; name = trailing group, spaces legal;
   strip `.tc-drops/` prefix). Failure reason: match this file's name in conn-2 stderr
   (§7 rows 8-10 patterns), else generic WriteFailed.
6. Cancel flag checked between spawns; mid-transfer cancel = TerminateProcess (§6.6).
7. Failures/cancel with created-but-unverified names ⇒ conn-3 cleanup batch (`-rm` each,
   all ignore-prefixed, output discarded; inv. 6).
8. Send event (+`ctx.request_repaint()`); GUI thread does ALL toast/paste work.

### 6.4 Drain (in `logic()`, beside the existing channel drains)

`Done` ⇒ dismiss/morph the Progress toast: all-success ⇒ just dismiss (the paste is the
feedback, inv. 7) then paste (§6.9 → §7.1 paste rules); partial ⇒ Error toast
`{k} of {n} uploaded to {host}` with per-file failure lines + paste the successes;
all-failed ⇒ Error toast per §7. Then start the terminal's next queued job.
`ConnFailed` ⇒ Error toast per §7 (all files listed as not uploaded). `Cancelled` ⇒
Info toast `upload cancelled — nothing pasted` (5s).

### 6.5 Verification is the ONLY success authority (inv. 5, T8)

Proven necessity: `-put` of a missing local file left exit 0 (ignore prefix); disk-full
wrote a PARTIAL file at the final name with exit 1 but a name-present ls line — the SIZE
check catches it (§9.5: 300KB file, 64KB tmpfs, `write remote "...": Failure`).

### 6.6 Cancel

Toast ✕ ⇒ `cancel.store(true)` + if a pid is live: `OpenProcess(PROCESS_TERMINATE)` +
`TerminateProcess` (windows crate — `Win32_System_Threading` already in the crate's
feature set for procinfo; same crate, no Cargo change). Worker wakes from wait, sees the
flag, runs cleanup (§6.3.7), sends `Cancelled`.

### 6.7 Cleanup policy (document-only beyond conn 3)

`~/.tc-drops` is the user's directory; WE never expire/rotate/delete verified uploads.
The consent dialog names the location; a future "manage drops" surface is out of scope.

### 6.8 GUI lifetime

Running children die with the GUI only if we kill them: `Uploads::shutdown()` (called
from the existing app-exit path) terminates live pids — an orphaned hidden sftp.exe
uploading forever is worse than a truncated partial. No resume-on-relaunch (v1,
documented).

### 6.9 Terminal death/sleep/delete mid-upload

The sftp connection is transport-independent of the session (separate TCP) — the upload
COMPLETES. At paste time the router re-checks: terminal missing/not-presented-Running ⇒
`ctx.copy_text(paths)` + Info toast `uploaded — terminal closed, remote paths copied to
clipboard`. Never spawns, never wakes a sleeping terminal (sleep inv.: wake is a user
act).

---

## 7. Failure taxonomy → exact toast table (stderr shapes are REAL captured outputs)

Classifier: `classify_conn(stderr) -> ConnErr` — first match wins, case-sensitive
substring on the raw stderr; per-file errors matched by filename within the line.

| # | Class | Captured stderr (this machine, §9) | Exit | Toast title | Toast detail |
|---|---|---|---|---|---|
| 1 | Network timeout | `ssh: connect to host 10.255.255.1 port 22: Connection timed out` | 255 | `{host} didn't answer` | `network timeout — {n} file(s) not uploaded` |
| 2 | Connection refused | `banner exchange: Connection to UNKNOWN port -1: Connection refused` (the `UNKNOWN port -1` is a Win32-OpenSSH cosmetic bug — match `Connection refused`) | 255 | `{host} refused the connection` | `is sshd running? — nothing uploaded` |
| 3 | sftp.exe missing | (spawn `NotFound` — no child ran; lone-exe probe printed `CreateProcessW failed error:2` / `posix_spawn: No such file or directory`) | — | `can't upload — sftp.exe not found` | `install the Windows "OpenSSH Client" feature (looked beside {ssh path})` |
| 4 | Auth (BatchMode) | `git@github.com: Permission denied (publickey).` — password hosts read `…(password)` etc.; match `Permission denied (` | 255 | `{host}: key or agent auth required for drops` | `password prompts can't run in the background — set up a key to enable drops` |
| 5 | DNS | `ssh: Could not resolve hostname no-such-host-zzz.invalid: No such host is known.` | 255 | `can't find {host}` | `hostname didn't resolve — nothing uploaded` |
| 6 | Host key untrusted | `Host key verification failed.` (BatchMode can't prompt) | 255 | `{host} isn't trusted yet` | `open the terminal and accept the host key once, then retry` |
| 7 | mkdir denied | conn 1 exit 1; mkdir shape `remote mkdir "/home/x/.tc-drops": Permission denied`, then `Can't ls: "/home/x/.tc-drops" not found` | 1 | `couldn't create ~/.tc-drops on {host}` | `permission denied in the home directory` |
| 8 | Remote write failed (disk full / quota / generic) | `write remote "/home/x/.tc-drops/big.bin": Failure` — SFTP v3 has only 8 error codes; ENOSPC arrives as bare `Failure`, so the text must hedge | 0¹ | `upload failed: {file}` | `{host} couldn't write it — disk full or quota?` |
| 9 | Local file unreadable | `stat C:/gone-missing.bin: No such file or directory` (also caught pre-flight §6.3.2) | 0¹ | `can't read {file}` | `the local file is missing or locked` |
| 10 | Remote dir denied at put (race: dir vanished/perms flipped after conn 1) | `dest open "/home/x/.tc-drops/f.txt": Permission denied` / `…: No such file or directory` | 0¹ | `upload failed: {file}` | `{host} refused the write in ~/.tc-drops` |
| 11 | Connection lost mid-upload | conn 2 exit 255 after partial transfer (`Connection closed` trailer is universal — every 255 capture above ends with it; classify by the FIRST line) | 255 | `connection to {host} was lost` | `{files} did not finish — nothing pasted for them` |
| 12 | Cancelled | (we killed the pid) | — | `upload cancelled` | `nothing was pasted` |
| 13 | SFTP subsystem disabled on host (rare sshd config) | expected `subsystem request failed on channel 0` — NOT capturable without such a host; documented from OpenSSH source, marked unverified | 255 | `{host} doesn't support file transfer` | `the server disables SFTP — uploads can't work here` |

¹ per-file rows ride `-put` ignore-prefix: overall exit stays 0; the verdict comes from
the ls tail (§6.5), the REASON from the stderr line naming the file.

Every toast carries the filename(s) (title or detail) per the user's requirement.
`Connection closed` alone is never matched — it trails every failure.

---

## 7.1 Paste-after-success (qol router semantics, completion-time)

- Build `'{home}/.tc-drops/{name}'` per verified file — POSIX single-quote, `'` →
  `'\''` (reuse/share drop.rs's bash quoting golden from qol §4.4); join with single
  spaces + ONE trailing space (qol D7 parity).
- Route through the qol mode decision AT COMPLETION (T10): composer `mode == Compose` ⇒
  `ComposerState::insert_dropped_text` (draft append — pointer act, episode untouched);
  else ⇒ the existing `paste()` path (bracketed iff `TermMode::BRACKETED_PASTE` — remote
  readline usually sets it ⇒ atomic; raw/TUI claude gets it as typed input). Implement
  by factoring the tail of `route_file_drop` into `insert_text_routed(id, text)`
  (coordinate with the qol implementer — one router, one truth).
- Paste fires ONLY for verified files (inv. 5); partial batch pastes successes in drop
  order (T11 justification: bytes are already on the host; the Error toast itemizes the
  rest; re-dropping the failed file later collides cleanly into `-2` naming).
- Remote shell quoting truth: absolute single-quoted POSIX paths are inert in bash, zsh,
  fish, dash — the paste goes to "whatever shell" safely; no `~` inside quotes (never
  expands), which is WHY the absolute home from `pwd` is used (T10).

---

## 8. Files (implementation map)

> **As-of-writing note.** The pure fns first landed in `src/gui/ssh_drop.rs` per this table,
> but remote-cli-resume (D4) later **hoisted the reusable transport pieces** (`sftp_args`,
> the `parse_*`/`classify_*` parsers, `resolve_sftp`) into **`src/ssh_transport.rs`** so the
> remote CLI installer and ssh-drop share one implementation. `ssh_drop.rs`'s header documents
> the move; T16 below reads against that split.

| File | Change |
|---|---|
| **`src/gui/ssh_drop.rs` (NEW)** | pure: `sftp_args` translator (§3.2), `resolve_names` (§3.3), batch builders, `parse_pwd`/`parse_ls1`/`parse_ls_l`, `classify_conn`/`classify_file` (§7), posix quote (or reuse drop.rs's); impl: `Uploads`, worker (§6.3), `shutdown()`. ALL pure fns golden-tested here. **(Transport pure fns since hoisted to `src/ssh_transport.rs` — see the note above.)** |
| **`src/gui/toast.rs` (NEW)** | the §5 component + unit tests (eviction, ttl-pause bookkeeping as pure logic) |
| `src/gui/mod.rs` (sidebar-p2 territory — coordinate at merge; additions are localized) | `App { toasts, uploads, pending_ssh_drop }`; the ONE Ssh arm body in `route_file_drop` (consent gate → `uploads.enqueue`); `Modal::SshDropConsent` variant + dialog arm (§4); `Prefs.ssh_drop_skip_consent` (serde-default); drain call in `logic()` (§6.4); `toasts.show(ctx, central)` at the end of `ui()`; `Uploads::shutdown()` on exit; hover-label text for Ssh terminals (§4.7 of qol): `upload to {host} — {n} file(s)` replacing the v1 refusal line |
| `src/gui/drop.rs` (qol's new file) | share the bash single-quote helper + `insert_text_routed` seam (§7.1) |
| `src/gui/composer.rs` (sidebar-p2 territory) | nothing new (`insert_dropped_text` ships with qol §4.5) |
| `Cargo.toml` | none expected (std `CommandExt`; windows-crate Threading features already present for procinfo — verify at build) |
| protocol.rs / state.rs / daemon | **ZERO changes** (inv. 1; `ssh_destination`/`shell_family` are already pub) |

---

## 9. Evidence log + tests/staging

### 9.1 Staging transport (validated recipe — no sshd, no remote host, real protocol)

```
# one-time, zero system mutation (package extracted to WSL /tmp, no install):
wsl -d Ubuntu -- sh -c "cd /tmp && apt-get download openssh-sftp-server \
  && dpkg -x openssh-sftp-server_*.deb /tmp/tcsftp"
mkdir (WSL) /tmp/tcdrop-home            # the pretend remote $HOME
# then every sftp.exe invocation gets, instead of a destination:
sftp -q -b <batch> -D "C:/Windows/System32/wsl.exe -d Ubuntu -- \
  /tmp/tcsftp/usr/lib/openssh/sftp-server -d /tmp/tcdrop-home"
```
`-D sftp_server_command` accepts arguments but argv[0] must be a FULL path (bare
`wsl.exe` fails `posix_spawn: No such file or directory`). This runs the entire pipeline
(mkdir/ls/put/verify/unicode/spaces/partial-failures) against a REAL Linux fs.

**Permanent env-gated staging knob** (TC_SSH_VIA_WSL precedent, "env-gated infra
STAYS"): `TC_SSH_DROP_TRANSPORT=<-D command>` — when set (and only then), the worker
replaces `<translated flags> <destination>` with `-D <value>`. Lets the full GUI flow
(drop → consent → toast → paste) run against the WSL server with zero network. Failure
staging: point it at a `-d /tmp/tcdrop-ro` (chmod 555) home for row 7, kill the wsl
process mid-put for row 11, etc.

### 9.2 Key empirical facts (dated 2026-07-04, this machine)

1. `sftp -D` + batch file: full pipeline PASS (mkdir → put "file with space é.png" →
   ls verify → exit 0), unicode+spaces exact on the Linux side.
2. **stdout fully buffered over pipes**: a progressive stdin driver received the `ls`
   echo only on process EXIT (4s timeout expired with the connection open; commands DID
   execute as fed — mkdir landed). Interactive driving is dead ⇒ T2/T3.
3. `-mkdir` on existing dir: stderr `remote mkdir "...": Failure`, exit 0 (ignored).
   Unprefixed `mkdir` on existing dir: exit 1, batch aborted.
4. Overwrite: `put` onto an existing name silently truncates (7→11 bytes observed) —
   WHY collision naming is mandatory (T7).
5. Disk full (64KB tmpfs, 300KB put): `write remote "...": Failure`, exit 1 unprefixed;
   partial file LEFT at final size < local size — size verification is load-bearing.
6. Dress rehearsal of the exact §3.1 flow incl. one bad file: conn 1 exit 0 with
   parseable `pwd`/`ls -1`; conn 2 `-put`×2 (one missing local) + `ls -l` tail → exit 0,
   stderr `stat C:/gone-missing.bin: No such file or directory`, ls shows the good file
   at the collision-suffixed name `file with space é-2.png` size-correct, bad file
   absent ⇒ per-file verdicts derivable exactly as specced.
7. All §7 network/auth shapes captured live (10.255.255.1 timeout, 127.0.0.1:9 refused,
   github.com auth-denied with a throwaway key, empty-known_hosts verification failure,
   `.invalid` DNS). All exit 255.
8. Bracket-glob adversarial: local `shot [1].png` with sibling `shot 1.png` present
   uploads the RIGHT file (Windows port doesn't glob-match put's local arg).
9. Sibling resolution: lone sftp.exe (stripped PATH) fails to find ssh.exe; sftp.exe
   with sibling ssh.exe connects ⇒ T4 works.
10. User realities: sessions are bare `ssh.exe 192.0.2.14`; known_hosts already
    trusts the host (3 key types, unhashed); `~/.ssh/config` has 3 aliases incl. a
    HostName-rewriting one ⇒ destination-verbatim is mandatory.

### 9.3 Tests

- **Unit goldens (ssh_drop.rs)**: `sftp_args` (bare host; `-p 2222 -i k.pem -v u@h` →
  renames/drops; glued `-p2222`; `-l` fold incl. existing-`@` skip; `ssh://`→`sftp://`;
  BatchMode-first + appended-defaults ordering), `resolve_names` (clean, collision→-2,
  batch-internal collision, dotfile, no-ext, -99 rollover), `parse_pwd`, `parse_ls1`
  (echo-line skip, prefix strip), `parse_ls_l` (both date forms `Jul  4  2026` /
  `Jul  4 04:08`, spaces in names, size extract), `classify_conn` (every §7 captured
  string verbatim as a test fixture), posix quote goldens (`'`-bearing).
- **toast.rs units**: push/evict cap (Progress never evicted first), update/dismiss,
  ttl-pause math.
- **No probe changes** (daemon untouched). GUI staging = the §9.1 knob + qol's
  `dropfiles` raw_input_hook staging verb (qol §10) on an isolated TC_DATA_DIR daemon:
  create an Ssh terminal via TC_SSH_VIA_WSL, `dropfiles` a PNG, screenshot the consent
  dialog → Continue → spinner toast → pasted `'/tmp/tcdrop-home/.tc-drops/….png' `
  in the composer draft. Failure shots via the row-7/row-11 recipes.
- Acceptance bar (the user's workflow): drop a QuipShot PNG onto the ssh terminal →
  consent (first time only) → toast spinner → remote path appears in the composer
  quoted + trailing space → claude on the remote can read that path. A second identical
  drop lands as `-2.png`. Pulling the network mid-upload toasts row 11 and pastes
  nothing.

---

## 10. Edges (behavior table)

| Edge | Behavior |
|---|---|
| Multi-file drop | one job, one consent (if due), one progress toast, sequential puts in ONE conn-2, per-file verdicts (T11) |
| Drop while an upload runs on that terminal | queued job, `queued —` toast prefix (T13) |
| Drop on a DIFFERENT ssh terminal meanwhile | parallel job, own toast |
| Consent modal open, another drop arrives | ignored (no-op) while any modal is open (§4) |
| remote_hooks=false session | works identically — upload is transport-level, independent of hooks (family is still Ssh; composer is Raw ⇒ paste goes to PTY) |
| Terminal dies/sleeps/deleted mid-upload | upload completes; paste falls back to clipboard + Info toast (§6.9) |
| GUI closes mid-upload | children terminated via shutdown(); no resume (§6.8) |
| Non-bash remote login shell (fish/zsh/csh) | paste is absolute single-quoted POSIX — inert everywhere (§7.1); upload never touches the login shell at all (SFTP subsystem) |
| Windows filename charset | spaces/unicode proven; brackets proven safe; `'` handled by quoting; non-Unicode names refused pre-flight (§3.3) |
| Directory dropped | refused with toast line (v1; §11 Q4) |
| File > ~1GB | no threshold dialog (T14); spinner + cancel; ServerAlive bounds a dead link to ~45s |
| Remote path pasted while claude mid-response | same as qol local-drop edge: bracketed paste buffers as typed input |
| `.tc-drops` exists as a FILE on the remote | `-mkdir` fails ignored; `ls` fails ⇒ row 7 toast (honest) |
| Remote home unwritable but .tc-drops exists | conn 1 fine; puts fail row 10 per-file |
| known_hosts entry only under the session's custom `-o UserKnownHostsFile` | carried by §3.2 `-o` passthrough — trust follows the session |

---

## 11. Open questions (with defaults)

| # | Question | Default |
|---|---|---|
| Q1 | Consent wording "over SFTP" vs the user's verbatim "via scp" | ship "over SFTP" (truthful); trivially swappable string if the user prefers the verbatim |
| Q2 | Success feedback beyond the paste itself | none (inv. 7); revisit only if users report missing it |
| Q3 | Size-threshold confirm (>50MB) | NO dialog (T14); if field evidence shows accidental huge drops, add a `detail` line to the progress toast first, not a modal |
| Q4 | Directory upload (`put -R` + tree naming) | v1 refuses; revisit with a real use case |
| Q5 | scp `-O` fallback for SFTP-subsystem-less hosts (row 13) | not in v1 (needs remote scp binary + protocol quirks); the toast names the limitation |
| Q6 | Configurable remote dir | no — `~/.tc-drops` fixed, named in consent; a per-host pref is bloat until asked for |
| Q7 | Reuse for #25 attention toast | the §5 seam (`Info` + `FocusTerminal`) is deliberately sufficient; #25 builds nothing new in toast.rs |

---

## 12. DO-NOTs

1. **DO NOT use scp.exe** (no mkdir/ls/verify; `-t` collides with its internal sink
   mode) — sftp batch only (T1).
2. **DO NOT stream/parse sftp stdout mid-connection** — it is fully buffered over pipes
   (§9.2); parse after exit only.
3. **DO NOT trust exit codes or stderr-silence for success** — `-put` failures leave
   exit 0, disk-full leaves a partial file at the right name; only the ls name+size
   verify pastes (inv. 5).
4. **DO NOT write to known_hosts or pass accept-new** — untrusted hosts fail row 6;
   trust is the terminal's job (inv. 2).
5. **DO NOT let BatchMode be overridden off** — prepend it; a hidden child that could
   prompt is a hang (inv. 3).
6. **DO NOT carry session-only ssh flags** (`-t`, forwards, mux `-S`) into sftp argv —
   translation table §3.2 is exhaustive; unknown = drop, never guess.
7. **DO NOT paste `~`-relative paths or double-quoted paths** — absolute + POSIX single
   quotes only (§7.1).
8. **DO NOT auto-delete anything in ~/.tc-drops except THIS job's unverified names**
   (inv. 6).
9. **DO NOT run any of this in the daemon or add protocol variants** — GUI-only
   (inv. 1).
10. **DO NOT block the GUI thread** — all sftp waits happen on worker threads; toast
    morphs ride the mpsc drain in `logic()`.
11. **DO NOT build toast features #26 doesn't need** — the #25 seam is `Info` +
    `FocusTerminal`, nothing more (inv. 8).
12. **DO NOT ship staging knobs beyond the env-gated `TC_SSH_DROP_TRANSPORT`** (the
    qol `dropfiles` verb rule applies unchanged).
