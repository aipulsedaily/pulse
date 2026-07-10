# P6 "First-Class Shells" — WSL / CMD / SSH — Implementation Spec (final, implementation-ready)

Target: C:\Terminal Control (Rust daemon + egui 0.35 GUI + tc.exe, single crate, proto 4,
P0–P5 + review + 3 perf waves complete).

User requirement (verbatim): **"i want wsl support cmd support ssh support WITH HISTROY AND
STILL CLAUDE AND OTHER CLI MEMORY ETC."**

Interpretation (binding): `wsl.exe` shells, `cmd.exe` shells, and `ssh` sessions become
first-class terminals with the FULL experience wherever physically achievable — journal
blocks (cmd/exit/duration/cwd), the composer (auto-arm, prompt cover, submit gating,
reclaim), cross-session history, activity states, `tc run` gating — AND the existing CLI
session persistence (claude pinned/`--resume`, codex, …) keeps working when those CLIs run
INSIDE wsl/ssh: a claude session inside a WSL shell survives reboot exactly like one in
PowerShell does today. Where a shell physically cannot deliver a feature, we ship an
honest degraded mode (§13), never a guess.

Ordered as the implementation plan: invariants → decisions → shell-family model → hook
delivery per shell → path namespaces → blocks/submission ledger → composer matrix →
tracker/CLI persistence → restore synthesis → creation UX → protocol/state → file-by-file
→ probes → degraded table → perf → phasing → open questions → DO-NOTs. Every decision
carries a one-line justification.

---

## 0. Non-negotiable invariants (violating any is a bug)

1. **Mirror purity**: the daemon's per-session Term contains EXACTLY what conhost emitted
   this session. Hooks are injected via argv/env/files only — NEVER typed into the PTY,
   never fed to the mirror out-of-band. All three new shells satisfy this by
   construction: WSL = rcfile path in argv, cmd = `PROMPT` env var, ssh = remote command
   string in argv.
2. **Ingest atomicity untouched**: parse + journal + fanout stay atomic under the journal
   lock; all new per-chunk work (none is added — the existing BlockScanner already
   carries every hook) stays post-lock.
3. **bincode append-only**: the one new C2D variant (`SubmitCommand`, §10) goes at the
   enum END after `Ctl`; `DaemonInfo.proto` bumps 4 → 5. `TerminalMeta` gains ONE
   appended `#[serde(default)]` field (§10.2) — legal because GUI+daemon are the same
   exe (always version-matched) and `CtlTerm` (tc.exe's wire shape) is deliberately
   decoupled from `TerminalMeta`; tc.exe never decodes `Snapshot` payloads it doesn't
   subscribe to. state.json is serde-JSON with unknown-field tolerance both directions.
4. **Hook grammar frozen**: `ESC]7717;<token>;<verb>;<hex(utf8 json)>BEL|ST` with verbs
   init|exec|pre + tokenless `133;B` — unchanged on the wire. New OPTIONAL init payload
   fields (§3.1.4) ride the existing serde-lenient schemas; no scanner change.
5. **Degraded honesty**: a wrong cover/block/resume is worse than none. Every gate that
   cannot be proven for a shell family is withheld for that family and the limitation is
   stated in the UI (strip label) and in §13 — never silently faked.
6. **Never guess a resume identity** (existing doctrine): Ambiguous inner CLIs restore
   the shell + an info line, exactly as today, in every new shell family.
7. **No new polling loops**: WSL distro / ssh host enumeration happens on dialog-open
   only (registry + file read); no periodic `wsl.exe` spawns (a `wsl.exe` launch costs
   ~100ms+ and would violate the idle-CPU budget the tracker gate just bought).
8. **Same-user trust model, stated honestly**: hook tokens in the bash rcfile / `PROMPT`
   env / remote /tmp rcfile are exactly as readable to a determined same-user process as
   today's .ps1 — guardrails against echo/replay confusion, not a security boundary.
   Remote (ssh) tokens additionally live on the remote host's /tmp for the session's
   lifetime; stated in §3.4.6.
9. **UX doctrine** (ux-doctrine.md): mouse-first, zero dividers, one-prompt-ever, cover
   drops on any uncertainty. The selector VISUALS belong to docs/selector-ui-spec.md
   (concurrent plan); this spec only defines the data it must expose (§9).
10. **Capture-on-change persistence**: all new restore metadata (remote cwd, detected
    inner CLI from hooks) flows through the same state.save-on-change path that
    guarantees power-loss safety today.

---

## 1. Headline decisions

| # | Decision | One-line justification |
|---|---|---|
| D1 | **ShellFamily is derived, not stored**: a pure `shell_family(program, args) -> ShellFamily` classifier (Pwsh / WslShell / Cmd / Ssh / Other) from the meta the user already provides | Zero wire/state migration for the common path; the program+args ARE the identity, and deriving can never disagree with what actually spawns |
| D2 | WSL terminals spawn an **explicit shell we choose (bash default)** inside the distro, never the distro's default shell | Hook injection is shell-specific (`--rcfile` is a bash mechanism); "default shell, maybe hooked" is a guess — an explicit shell is provable, and zsh/fish become explicit options later (§3.2) |
| D3 | WSL bootstrap = per-spawn **rcfile on the Windows side** (`data_dir\bootstrap\<id>.bashrc`), reached via the `/mnt/<drive>/…` automount translation, launched `wsl.exe -d <distro> --cd <cwd> --exec /bin/sh -c '<rc-guard>'` with a self-healing fallback to plain `bash -i` | Reuses the existing rotate-per-spawn/delete-with-terminal lifecycle and user-private ACL verbatim; the sh guard means a missing automount degrades to a working unhooked shell instead of a broken one |
| D4 | Same OSC 7717 hex-JSON hook grammar in every shell; bash implementation = `PROMPT_COMMAND` pre-hook (with the same 15ms drain sleep) + minimal clean-room **DEBUG-trap preexec** for the exec hook + `133;A`/`133;B` wrapped into PS1 idempotently every prompt | One scanner, one BlockStore, one composer — the daemon cannot tell shells apart and must not; the DEBUG-trap technique (bash-preexec's) is the only pre-exec hook bash has |
| D5 | **cwd canonical form = the path exactly as the shell sees it** (POSIX for wsl/ssh, Windows for pwsh/cmd), tagged per-session by a `PathNamespace`; no `\\wsl$` translation in any hot path | Restore feeds the cwd back to the SAME shell (`wsl --cd` accepts leading-`/` Linux paths; ssh `cd`), so the shell's own namespace is the only lossless one; `\\wsl$` is a slow 9p bridge used only for opt-in claude-jsonl correlation (§7.3) |
| D6 | **cmd.exe hooks via the `PROMPT` environment variable** (`$E]…$E\` escapes): OSC 9;9 cwd + a STATIC token-bearing pre hook + 133;A/B — set with `CommandBuilder.env`, no AutoRun registry, no /k wrapper | `PROMPT` is per-process and macro-expanded at every render ($P = live path) — the only per-prompt code execution cmd has; AutoRun is machine-global (invasive) and doskey cannot wrap arbitrary commands |
| D7 | cmd has **no exec hook and no per-command exit codes — permanently** (PROMPT cannot express `%ERRORLEVEL%` at render time; there is no readline wrapper); block records for cmd come from the **submission ledger** (D8) and exit stays `None` | Stating the hard limit honestly beats a heuristic that misattributes exit codes; WT's own shell-integration docs hit the identical wall |
| D8 | New appended `C2D::SubmitCommand { id, cmd, write }` (proto 5): daemon computes `submission_bytes` from the mirror, writes them (when `write`), and opens a **synthetic block** at the pre-write journal head; the next token-checked `pre` closes it. GUI composer routes submits through it for exec-less families; `write:false` records a block for a GUI-observed raw Enter without writing bytes | Gives cmd (and any future exec-less shell) real history/blocks for every composed AND observed command — the daemon stays block-authoritative and the composer keeps zero-bytes-per-keystroke |
| D9 | **ssh hooks = one-shot remote bootstrap in the ssh command line** (mktemp + base64 rc + `exec bash --rcfile`), per-host `remote_hooks` opt-in defaulting ON, POSIX-quoted so any remote login shell (fish included) survives it, self-healing to plain `sh -i` when bash is absent | No persistent remote mutation (never touches ~/.bashrc), works on any bash-bearing host, and failure degrades to a working plain ssh instead of a broken login |
| D10 | **Remote/WSL inner-CLI tracking is hook-based**: `exec` hook command lines are parsed against the existing adapter registry (argv-Explicit tokens only in v1); the Win32 PEB/Toolhelp tracker is skipped for Ssh and used only for cwd-fallback for WslShell | Linux/remote processes are invisible to Toolhelp — the hooks are the only truthful witness, and they already carry exactly the argv the adapters parse |
| D11 | Claude-in-WSL **jsonl correlation via `\\wsl$\<distro>\<home>`** ships as phase 2 of P6a, keyed off the init hook's new optional `home` field | Gets bare-`claude`-in-WSL to Correlated confidence with zero remote cost, but 9p metadata quirks (birth times) need probe time — don't block the Explicit path on it |
| D12 | Restore synthesis per family (§8): `wsl.exe -d <distro> --cd <cwd> --exec …rc-guard…` (+ resume command appended to the rcfile tail); `cmd.exe` respawned with PROMPT env + `CommandBuilder.cwd` (`/K cd /d … && <resume>` only when an inner CLI must be resumed); `ssh <host>` + remote `cd` + resume baked into the one-shot rc | Mirrors the proven pwsh pattern exactly: direct-spawn command lines, never keystroke injection, shell stays alive after the CLI exits |
| D13 | ~~**No ssh auto-reconnect in v1**~~ **SUPERSEDED (proto 10)** — Q1 was revisited with field evidence (two hooked ssh sessions died exit 255 at once across a PC sleep and stayed Dead); bounded auto-reconnect shipped in `daemon/reconnect.rs` (2s/10s/30s backoff, opt-in default-on, only for links that had HOOKED so auth completed non-interactively). We still add `-o ServerAliveInterval=15 -o ServerAliveCountMax=3` to spawned ssh argv (user args win on conflict). | Original rationale (auth-prompt/flapping-link/wrong-world hazards) is answered by the qualification gates in reconnect.rs — see its module doc. |
| D14 | **Family-aware run/arm gating**: for exec-less shells (cmd) `at_prompt` cannot be cleared by an exec hook, so the P5 run gate additionally requires mirror-cursor-at-prompt-col + output-quiet ≥300ms, and the GUI keeps `cursor_clean` as the cover gate (it already is) | cmd's `pre` latch alone would call a running command "idle"; the mirror cursor + quiet window is the strongest daemon-side evidence available without an exec hook |
| D15 | **Clear chord per family**: Ctrl+C for pwsh/bash/zsh/fish/ssh (edit-mode-independent everywhere), **ESC for cmd** (cmd's line editor clears the input in place, no `^C` splatter, no new prompt) | One chord per family chosen for provable whole-line kill; ESC is wrong for readline (vi-mode catastrophe) and Ctrl+C is noisy for cmd — per-family is the only correct table |
| D16 | Selector/creation UX: GUI-side enumerators — WSL distros from the **Lxss registry** (never parse `wsl -l` output), ssh hosts from `~/.ssh/config` (+ one Include level) — handed to the selector as a data contract (§9); visuals belong to docs/selector-ui-spec.md | `wsl.exe -l` emits UTF-16LE, localized, and costs a process spawn; the registry is structured, instant, and works while WSL is stopped |
| D17 | Phasing: **P6a WSL-bash → P6b cmd → P6c ssh**, each independently shippable with the full verification bar; zsh/fish are a P6a fast-follow (mechanisms specified now, §3.2) | WSL-bash is highest value with full hooks achievable; cmd is small and mostly-degraded; ssh has the largest unknown surface (remote envs) and benefits from the bash template being battle-tested by P6a first |
| D18 | win32-input-mode, DSR answering, journals, serialization/preface/seams, resize machinery: **zero changes** | These layers are byte-stream/window-geometry mechanics that don't know what shell is attached — touching them for P6 would be gratuitous risk |

---

## 2. Shell family model (src/state.rs)

```rust
/// Derived, never persisted (D1). Classification of what actually spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellFamily {
    /// powershell.exe / pwsh.exe, TermKind::Shell — today's fully-hooked path.
    Pwsh,
    /// wsl.exe with an explicit inner shell we control (bash v1; zsh/fish later).
    WslShell { distro: Option<String> },
    /// cmd.exe as the terminal's shell.
    Cmd,
    /// ssh.exe (Windows OpenSSH client or compatible argv shape).
    Ssh { host: String },
    /// Everything else (claude-kind, Custom commands, unknown shells) — exactly
    /// today's unhooked behavior.
    Other,
}

pub fn shell_family(kind: &TermKind, program: &str, args: &[String]) -> ShellFamily
```

Rules (unit-tested, §12 U1):
- Only `TermKind::Shell` can be Pwsh/WslShell/Cmd/Ssh (Custom keeps full degraded
  freedom; Claude-kind is untouched). Stem-matched case-insensitively like today's
  pwsh check.
- `wsl` / `wsl.exe`: distro = value after `-d`/`--distribution` (None = default distro).
  If the user hand-built exotic wsl args (`--system`, `--user`, `-e`), classify
  WslShell only when OUR canonical arg shape is present (a `--exec /bin/sh -c` tail we
  generated); else Other — we never hook argv shapes we didn't synthesize.
- `cmd` / `cmd.exe`: Cmd.
- `ssh` / `ssh.exe`: host = first non-flag arg (skipping known value-taking flags
  `-p -i -l -o -F -J -L -R -D -W -b -c -m -e`); no host found ⇒ Other.
- Everything else ⇒ Other.

**`TerminalMeta` appended field** (§10.2): `#[serde(default)] pub shell_cfg: Option<ShellCfg>`
```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ShellCfg {
    /// WSL inner shell: "bash" (default) | "zsh" | "fish" (P6a.2).
    #[serde(default)] pub shell: Option<String>,
    /// Ssh: inject the one-shot remote bootstrap (default true).
    #[serde(default = "default_true")] pub remote_hooks: bool,
}
```
Only the fields the classifier can't derive live here. Justification: distro/host stay in
args (single source of truth); the opt-ins are user choices that must persist.

**Path namespace** (derived): `fn path_namespace(family) -> PathNamespace { Win | Posix }`
— WslShell/Ssh ⇒ Posix, others ⇒ Win. Used by the per-session OSC cwd scanner (§4) and
the restore validity checks (§8).

---

## 3. Hook delivery per shell

The daemon side is COMMON to all: the reader-thread BlockScanner, token check,
BlockStore, prompt latch, PromptState, P5 waiters, and the GUI BlockFeed are untouched.
This section is only about getting the shells to emit the same bytes pwsh already does.

### 3.1 WSL bash (P6a — the headline)

**3.1.1 Launch synthesis** (session::spawn, new branch for `ShellFamily::WslShell`):

```
program: wsl.exe
args:    [-d <distro>]  --cd <cwd>  --exec /bin/sh -c '<GUARD>'
GUARD:   if [ -r "<RC_MNT>" ]; then exec bash --rcfile "<RC_MNT>" -i; else exec bash -i; fi
```

- `--cd <cwd>`: accepts both Windows paths and leading-`/` Linux paths natively — this is
  what makes the verbatim-namespace cwd doctrine (D5) free. First spawn (user-picked
  Windows dir) passes the Windows path; restores pass the tracked POSIX `live_cwd`.
- `--exec` (execvp, no default-shell indirection) + `/bin/sh -c` guard: deterministic
  regardless of the user's default shell; missing rcfile (automount disabled, moved
  data dir) self-heals to plain `bash -i` — degraded-hookless, never broken (D3). If
  even bash is absent (rare minimal distro) the exec fails, sh exits, and the
  fast-exit warning machinery reports it exactly like any crashing command.
- `RC_MNT` = drive-letter translation of `bootstrap::script_path_bash(id)`:
  `C:\Users\…\bootstrap\<id>.bashrc` → `/mnt/c/Users/…/bootstrap/<id>.bashrc`
  (lowercase drive, backslashes→slashes; pure function, unit-tested §12 U2). A
  non-drive (UNC) data dir is untranslatable ⇒ spawn hookless with one log line
  (TC_DATA_DIR on a share is a probe-only scenario).
- Env: `TC_SESSION_ID` is already set for the whole tree; ADD
  `WSLENV = <inherited WSLENV>:TC_SESSION_ID` (no `/u` flag — flows Win→WSL AND back
  out through interop, so a `tc.exe` invoked from inside the WSL shell still
  self-identifies for the recursion guard; interop-launched Windows exes run on the
  Windows side, so tc.exe's loopback connect works from WSL unchanged).
- Quoting: `<RC_MNT>` contains no spaces only if the data dir has none —
  %LOCALAPPDATA% can. It is double-quoted inside the single-quoted sh -c body; single
  quotes cannot appear in a drive-letter translation of a Windows path that portable-pty
  accepted, and we assert-and-degrade if one ever does (log + hookless).

**3.1.2 The bash rcfile template** (bootstrap.rs, new `BASH_TEMPLATE`, token substituted
at generation exactly like the ps1):

Behavioral spec (implementation may differ in spelling; probes pin the observable bytes):

```bash
# generated by Pulse; token rotates every spawn
[ -f ~/.bashrc ] && . ~/.bashrc          # user's config FIRST (mirrors pwsh wrapping)
__TC_TOK='{TOKEN}'; __TC_N=0
__tc_emit() {  # $1 verb, $2 compact json
  local hex; hex=$(printf %s "$2" | od -v -An -tx1 | tr -d ' \n')
  printf '\033]7717;%s;%s;%s\007' "$__TC_TOK" "$1" "$hex"
}
__tc_json_str() { local s=${1//\\/\\\\}; s=${s//\"/\\\"}; printf %s "$s"; }
__tc_pre() {
  local e=$?                              # FIRST: the real exit code
  sleep 0.015                             # same ConPTY frame-drain as the ps1 pre hook
  __TC_N=$((__TC_N+1))
  __tc_emit pre "{\"e\":$e,\"n\":$__TC_N,\"d\":\"$(__tc_json_str "$PWD")\"}"
  printf '\033]9;9;%s\007' "$PWD"         # cwd for the tracker (posix namespace)
  __tc_wrap_ps1                           # idempotent 133;A/B wrap (below)
  __tc_at_prompt=1                        # arm the DEBUG-trap preexec latch
}
__tc_wrap_ps1() {                         # re-applied EVERY prompt: prompt themes
  case $PS1 in "\[\033]133;A\007\]"*) ;;  # rewrite PS1 per-render and would clobber
  *) PS1="\[\033]133;A\007\]${PS1}\[\033]133;B\007\]" ;; esac
}
__tc_debug() {                            # clean-room minimal bash-preexec
  [ -n "$COMP_LINE" ] && return           # completion, not execution
  [ "$__tc_at_prompt" = 1 ] || return     # one exec per prompt cycle; PROMPT_COMMAND
  __tc_at_prompt=0                        #   itself runs with the latch already 0
  local c; c=$(HISTTIMEFORMAT= builtin history 1 2>/dev/null); c=${c#*[0-9]  }
  [ -n "$c" ] || c=$BASH_COMMAND          # history off ⇒ first simple command
  c=${c:0:2000}
  __tc_emit exec "{\"c\":\"$(__tc_json_str "$c")\"}"
}
PROMPT_COMMAND="__tc_pre${PROMPT_COMMAND:+;$PROMPT_COMMAND}"   # OURS FIRST: exit code
trap '__tc_debug' DEBUG
__tc_emit init "{\"v\":1,\"pid\":$$,\"shell\":\"bash\",\"home\":\"$(__tc_json_str "$HOME")\",\"user\":\"$(__tc_json_str "$USER")\"}"
{RESTORE_TRAILING}                        # empty, or "cd '<cwd>' ; claude --resume <id>"
```

Load-bearing details, each one-line justified:
- **`e=$?` captured before anything** — any command in the hook would clobber it.
- **`__tc_pre` FIRST in PROMPT_COMMAND** — a user hook running first would clobber `$?`.
- **15ms sleep in pre** — identical ConPTY reorder as pwsh: conhost passes OSCs through
  the pipe immediately but renders text async, so the pre hook can beat the command's
  last output rows into the stream and clip `end_off` (probe-proven on pwsh); wsl.exe's
  output crosses the same conhost. exec hook stays sleep-free (same doctrine: it would
  delay every command start).
- **133;B at PS1 end, no drain possible** — bash has no between-prompt-paint-and-readline
  hook, so 133;B rides PS1 and the ~15ms wrong-cell race is accepted: `cursor_clean` is
  the load-bearing guard (it already is on pwsh) and the P4 reclaim window recovers the
  slip-through keystroke. Documented residual, same class as pwsh's.
- **Idempotent per-prompt PS1 re-wrap** — git-prompt themes rebuild PS1 inside their own
  PROMPT_COMMAND every render; a once-at-rc wrap would be clobbered after one prompt.
- **DEBUG-trap one-shot latch** — DEBUG fires per simple command (pipelines fire
  several); the latch set only by the pre hook makes exactly the FIRST command after a
  prompt emit exec, which is the block-open semantic. `history 1` carries the full
  typed line (multi-line included, joined by `;`? no — history stores the literal line
  with embedded newlines; the JSON escaper handles `"`/`\` and the daemon's serde is
  lenient about the rest); truncated to 2000 like the ps1.
- **init carries `shell`/`home`/`user` (NEW optional fields)** — serde-lenient
  `InitPayload` gains `#[serde(default)] home: String, user: String, shell: String`;
  zero wire change, powers §7.3 correlation and diagnostics.
- **Existing trap/PROMPT_COMMAND coexistence**: we append, never replace, the user's
  PROMPT_COMMAND; a pre-existing user DEBUG trap IS replaced (bash allows one) — logged
  limitation in §13 (same class as pwsh users who redefine `prompt` after our wrap).

**3.1.3 Ordering on the wire**: pre → 9;9 → (user prompt paint) with 133;A prefix →
prompt text → 133;B, then exec after accept — byte-order-compatible with what the
BlockScanner, prompt latch, and composer already consume from pwsh. No daemon change.

**3.1.4 What the daemon does differently for WslShell**: nothing in the reader/scanner.
Only: `hooked = true` in launch() (§11 mod.rs), Posix namespace for the OSC cwd scanner
(§4), tracker adjustments (§7), restore synthesis (§8).

### 3.2 zsh / fish inside WSL (P6a.2 fast-follow — mechanisms fixed now)

- **zsh**: generate `data_dir\bootstrap\<id>.zshdir\.zshrc` (sources `$HOME/.zshrc`
  first, then the same hooks via native `precmd`/`preexec` — no DEBUG-trap needed;
  `preexec` receives the command as `$1`: strictly better than bash). Launch:
  `--exec /bin/sh -c 'export ZDOTDIR=<dir_mnt>; exec zsh -i'` with the same guard shape.
  Justification: ZDOTDIR is zsh's sanctioned rcfile redirection; no user-file mutation.
- **fish**: `--exec /bin/sh -c 'exec fish -i -C "source <rc_mnt>"'`; hooks via
  `fish_preexec`/`fish_postexec` event functions; fish always sources user config first
  by design (`-C` runs AFTER config.fish). Hex via `string` builtins or `od` (fish has
  no parameter expansion; template differs, emitted bytes identical).
- `ShellCfg.shell` selects; selector exposes it as a per-distro dropdown (§9).

### 3.3 cmd.exe (P6b)

**3.3.1 Injection** — `CommandBuilder.env("PROMPT", <value>)` on the cmd spawn:

```
$E]9;9;$P$E\$E]7717;{TOKEN};pre;{HEX_STATIC}$E\$E]133;A$E\{BASE}$E]133;B$E\
```

- `{BASE}` = the user's existing `PROMPT` env value if set (wrapped, not replaced), else
  `$P$G`. Justification: preserve customized prompts, same courtesy as the pwsh wrapper.
- `{HEX_STATIC}` = hex of `{"e":null,"n":0}` — a CONSTANT string: PROMPT macros cannot
  hex-encode `$P` and cannot expand `%ERRORLEVEL%` at render time (env vars in the
  PROMPT string expand once, when set). So the token-bearing pre proves liveness and
  closes blocks; **cwd rides the adjacent tokenless OSC 9;9** (`$P` is a render-time
  macro) and **exit codes are permanently unavailable** (D7).
- On a `pre` whose payload cwd is empty AND the session family is Cmd, `on_block_event`
  stamps `BlockStore.last_cwd` from `Session.osc_cwd` instead (the 9;9 lands in the
  same prompt render, microseconds earlier in the stream). One 5-line branch.
- Verification basis: this is Windows Terminal's documented cmd shell-integration
  mechanism ($E escapes emitting OSC 133 from PROMPT), probe-pinned in §12 P3.
- The verbs stream: pre → 9;9 → 133;A → text → 133;B. **No exec hook exists and none is
  synthesizable shell-side** (no readline wrapper, doskey can't wrap arbitrary input,
  AutoRun is machine-global) — blocks come from §5.
- Token rotation: same launch() rotation; the env var is baked at spawn (rotates every
  spawn like the ps1). `hooks_live` proves the prompt actually rendered our OSC.
- cmd is a Win32 process: the EXISTING PEB/Toolhelp tracker works unchanged (cwd
  fallback, inner-CLI detection for claude-under-cmd) — cmd actually ships with more of
  the P0–P5 feature set intact than WSL does (§13).

**3.3.2 at_prompt weakness**: with no exec hook the session.prompt latch set at 133;B is
never cleared when a command starts (only the NEXT pre re-cycles it). Mitigations:
- GUI: `cursor_clean` (cursor must sit exactly at the captured prompt-end cell) already
  withholds the cover/auto-arm whenever output moved the cursor — no change, stated.
- Daemon (P5 run gate + PromptState): for Cmd family, `at_prompt` additionally requires
  mirror cursor column == latched col on the cursor's row AND `last_output` quiet
  ≥300ms (D14). Residual risk (an interactive app idling with a prompt-shaped screen)
  is documented; `--force` exists.

### 3.4 ssh (P6c)

**3.4.1 Launch synthesis** (`remote_hooks: true`, the default):

```
program: ssh.exe
args: -t  -o ServerAliveInterval=15 -o ServerAliveCountMax=3  <user args…>  <host>
      "sh -c 'f=$(mktemp); echo {B64_RC} | base64 -d > \"$f\"; [ -x \"$(command -v bash)\" ] && exec bash --rcfile \"$f\" -i; rm -f \"$f\"; exec sh -i'"
```

- `-t`: a remote command suppresses tty allocation; we need the interactive tty.
- The remote command is ONE argv element; outer parsing by the remote **login shell**
  (could be fish/zsh/csh) sees a single simple command `sh -c '<single-quoted>'` —
  valid in every shell family; everything interesting is POSIX inside sh (D9).
- `{B64_RC}` = base64 of the SAME bash rc template as §3.1.2 (one template, two
  deliveries) with two deltas: sources `~/.bashrc` on the REMOTE, and
  `{RESTORE_TRAILING}` = `cd '<remote cwd>' 2>/dev/null; <resume command>` on restores.
  base64 beats heredoc/quoting gymnastics and survives any argv mangling; coreutils
  base64 is effectively universal, and its absence self-heals to `sh -i` (unhooked but
  working).
- The rc `rm -f "$f"`s itself at its END (rc lives on the remote /tmp only for the
  session bootstrap; token exposure window stated in inv. 8).
- ServerAlive flags: prepended so user-provided duplicates in `<user args…>` win
  (ssh takes the LAST -o occurrence? — it takes the FIRST for most options: therefore
  we APPEND ours AFTER user args instead; ssh first-wins semantics make user overrides
  automatic). One-line rule: user args come first, our keepalive defaults after.
- `remote_hooks: false` (per-host opt-out, selector toggle): plain `ssh <args> <host>`,
  fully degraded row in §13.

**3.4.2 What the hooks replace**: PEB cwd, Toolhelp trees, and jsonl correlation are all
impossible across the wire. The hook stream carries everything we keep: cwd from `pre`
(remote POSIX path), busy/idle from open blocks, command lines from `exec` — the
tracker for Ssh family is PURELY hook-fed (§7.2).

**3.4.3 CLI persistence inside ssh** (the user's headline "claude inside ssh"):
- `exec` hook cmd parsed against the adapter registry (§7.2). `claude --resume <id>` /
  `--session-id <id>` typed remotely ⇒ Explicit ⇒ restore re-runs
  `ssh <host>` + one-shot rc whose trailing = `cd '<cwd>'; claude --resume <id>` —
  reboot-surviving remote claude, the pwsh-parity story.
- Bare `claude` remotely ⇒ no filesystem to correlate against ⇒ Ambiguous ⇒ restore
  brings the hooked remote shell up in the right cwd + the existing info line
  ("resume it manually…"). Honest limit, stated in §13.
- The inner CLI is cleared when the block that opened it closes (§7.2) — same
  lifecycle the process tracker gives pwsh today.

**3.4.4 Reconnect semantics** (D13 — **superseded at proto 10**): link death ⇒ ssh exits
⇒ on_exit ⇒ Dead (amber → dead dot, activity truthful); ServerAlive bounds detection to
~45s. Restart re-runs the full synthesis (fresh token, fresh remote rc). Boot auto-restore
works when auth is non-interactive (keys/agent); password prompts simply sit waiting in the
terminal — visible, honest. **Bounded auto-reconnect now runs** (daemon/reconnect.rs) for
links that had hooked — see its module doc; the "no loops in v1" line is historical.

**3.4.5 Auth surfaces**: we never parse or answer auth prompts; they render in the
terminal like any output (hookless until the shell starts — at_prompt stays false, so
the composer correctly stays Raw(NoPrompt) during "Password:"). Justification: a cover
over a password prompt would be a correctness and safety bug.

**3.4.6 Trust statement**: the hook token transits the ssh channel and rests briefly in
remote /tmp (mode 600 via mktemp). A hostile REMOTE root can read it and spoof blocks
for that terminal — in-scope acceptance for v1, documented; block records are advisory
UI, never an execution path.

---

## 4. Path namespaces & cwd canonicalization

- `parse_osc_cwd` gains a namespace parameter (per-session, from the family):
  - `Win` (today): `normalize_win_path` unchanged (drive-root trap guarded).
  - `Posix` (new): trim trailing `/` EXCEPT preserve bare `/`; reject empty; keep
    verbatim otherwise. NEVER routed through `normalize_win_path` (it would mangle
    `/` → None and could never hit the drive-relative trap anyway). Unit-tested §12 U3.
- `BlockRec.cwd`, `meta.live_cwd`, `InnerCli.cwd` for wsl/ssh sessions hold the POSIX
  string verbatim inside `PathBuf` (an opaque byte container on Windows — display,
  compare, and restore round-trip losslessly; nothing calls `.is_dir()` on them — see
  next bullet).
- **`is_dir()` guards become namespace-aware**: `launch()`'s
  `live_cwd.filter(|p| p.is_dir())` and the tracker's equivalents apply ONLY in the Win
  namespace; Posix cwds are trusted verbatim (validation would require `\\wsl$` (slow,
  distro must run) or a remote round-trip (absurd)). A stale remote dir simply makes
  the shell start in `$HOME` with cd's error visible — shell-native failure, honest.
- Restore transport: `wsl --cd` accepts leading-`/` paths natively; ssh restores embed
  `cd '<cwd>'` in the rc (single-quote-escaped by doubling? POSIX uses `'\''` — the
  generator escapes `'` as `'"'"'`… simpler: the rc is base64'd so the cd line is
  written literally with printf-safe quoting handled at template-fill time,
  unit-tested §12 U4). cmd restores use `CommandBuilder.cwd` (Win namespace).
- GUI: history popup / block chrome / composer cover display the cwd string raw —
  `/home/alice/proj` reads perfectly; zero rendering changes.
- `claude_project_dir_name` already munges any string (`/`→`-`): `\\wsl$` correlation
  (§7.3) computes the Linux-side name from the POSIX cwd verbatim.

---

## 5. Blocks & the submission ledger (proto 5)

### 5.1 What already works with zero changes
WSL-bash and hooked-ssh emit the full verb stream ⇒ BlockStore/sidecars/eviction/
compaction/StreamPos/GUI anchoring/P4 history/P5 waiters all function identically —
records carry real exits (`$?`), durations, and POSIX cwds. **This is the "WITH
HISTORY" requirement satisfied for wsl+ssh by §3 alone.**

### 5.2 `C2D::SubmitCommand` — blocks for exec-less shells (cmd)

```rust
// C2D — APPENDED at enum END (after Ctl); proto 4 → 5:
/// Submit a command line to a terminal the daemon should ALSO record as a
/// block (exec-less shells: cmd). `write:true` ⇒ daemon computes
/// submission_bytes from the mirror (P3/P5-identical), writes them to the PTY,
/// and opens a synthetic block at the pre-write journal head. `write:false` ⇒
/// record-only: the bytes already went via Input (GUI-observed raw Enter);
/// open the synthetic block at the current head, write nothing.
SubmitCommand { id: Uuid, cmd: String, write: bool },
```

- Handler (mod.rs): FULL-scope legacy verb (GUI/master only — scoped controllers use
  `Ctl Run`, which for Cmd-family terminals routes through the same synthetic-open
  helper so `tc run` on cmd yields a real RunStarted/RunDone). `open_synthetic(id, cmd)`
  = `BlockStore.open_block(cmd, journal.absolute_len(), now)` + the normal incremental
  `D2C::Blocks` notify + EV_BLOCKS event. The NEXT token-checked `pre` closes it via
  the existing `on_pre` path (exit=None for cmd, honest).
- Single-line only in v1: `cmd` containing `\n`/`\r` is refused with an Error frame
  (cmd executes each line at its own prompt; a multi-line ledger queue is §16 Q2).
- GUI composer routing (composer.rs / mod.rs): for Cmd-family terminals, `submit()`
  sends `SubmitCommand{write:true}` INSTEAD of `Input{submission_bytes}` — one branch
  at the existing submit site; SubmitHold/cover mechanics unchanged (the echo still
  lands in the grid the same way).
- GUI observed-raw capture (the "typed directly into cmd" path): in the raw-input
  handler, when family==Cmd, a latched prompt_end exists, and the outgoing byte is
  `\r` (or the win32 Enter record), read `backend.reclaim_text()` — the FINAL rendered
  input-row text, exact regardless of editing keys — and if it yields
  `Reclaim::Text(cmd)`, send `SubmitCommand{cmd, write:false}` alongside the Enter.
  Commands typed while no GUI is attached are NOT recorded (stated in §13; the daemon
  cannot reconstruct edited input from raw bytes without an exec hook).
- Records for cmd therefore cover: everything composed, everything `tc run`, everything
  typed under an attached GUI. `exit` always None; duration real (pre closes it); cwd
  real (9;9). History popup/status glyphs already render `exit:None` open-dot/bare
  states — no GUI schema change.

### 5.3 Run-gate for exec-less shells (D14)
`ctl_run`'s gate for Cmd family: `hooks_live && no_open_block && !alt` PLUS
`session.prompt.lock().is_some_and(|col| mirror cursor col == col)` PLUS
`now - last_output >= 300ms`. Justification: without exec hooks, "no open block" is
vacuous for typed commands — the cursor+quiet check is the strongest honest signal.

---

## 6. Composer gate per shell (the correctness matrix)

| Signal / behavior | pwsh 5.1 (today) | WSL bash | cmd | ssh (hooked) |
|---|---|---|---|---|
| `hooked` (epoch>0) | yes | yes | yes | yes |
| at_prompt latch (133;B) | yes | yes | yes (weak — no exec clear; D14) | yes |
| prompt_end capture | 133;B after 15ms drain | 133;B at PS1 end (~15ms race, cursor_clean guards) | 133;B after $P render (same race) | same as bash + link latency |
| cursor_clean / cover | yes | yes | yes | yes (echo latency may hold SubmitHold to its 250ms cap — honest raw after) |
| BRACKETED_PASTE | never (PSReadLine 2.0) ⇒ bare + `\r` | readline ≥8.1 sets it ⇒ bracketed | never ⇒ bare | remote readline decides ⇒ usually bracketed |
| Multi-line submit | `\n`→`\r`, N sequential blocks | bracketed: literal newlines, ONE accept, ONE exec/block (history 1 carries all) | REFUSED in v1 (strip hint "cmd runs one line at a time") | as bash |
| Clear chord (activation over dirty prompt) | win32 Ctrl+C (CancelLine) | Ctrl+C (readline abort, emacs+vi safe; bash re-runs PROMPT_COMMAND after ^C ⇒ re-latch — probe-pinned §12 P2) | **ESC** (cmd clears line in place; no ^C splatter, no re-prompt needed — latch was never cleared) | Ctrl+C |
| Reclaim (P4 extract_input) | yes | yes — ghost-text heuristic (DIM\|ITALIC) is PSReadLine-specific and simply never fires on bash (no ghost text) ⇒ no false CursorMidLine | yes (same) | yes |
| Empty-Enter spacer covers | yes | yes (pre with no exec ⇒ spacer, mechanism identical) | PARTIAL: pre fires per prompt, spacers work; no exec means no "superseded" signal — existing paint-time self-heal covers it | yes |
| Race-window auto-reclaim | yes | yes | yes | window widened by RTT — keep RECLAIM_WINDOW=800ms, accept |

GUI changes required (composer.rs): `clear_chord(backend, family)` (ESC for Cmd — win32
`encode_key(Escape)` else `0x1b`), multi-line refusal for Cmd on submit + strip hint,
submit routing per §5.2. Everything else is signal-identical by construction.

Multi-row prompts (bash themes with `\n` in PS1): the cover paints ONLY the 133;B row
(the input row) — upper prompt rows stay visible raw. Correct by the one-prompt
doctrine (the input line is the prompt the cover replaces); note in the visual QA list.

---

## 7. Tracker & CLI persistence per shell

### 7.1 Family routing (daemon tick, mod.rs + tracker.rs)
- **Pwsh / Cmd / Other**: EXACTLY today's analyze() (OSC cwd → PEB descent → Toolhelp
  adapters). cmd inherits full inner-CLI tracking for free (claude under cmd is a
  Win32 child — visible, correlatable, restorable today-style).
- **WslShell**: cwd from OSC 9;9 only (PEB of wsl.exe is meaningless); descendants
  scan SKIPPED (Linux procs invisible); inner CLI from hooks (§7.2). The tracker-tick
  activity gate already skips output-quiet sessions — unchanged.
- **Ssh**: cwd from hooks only; process tree = ssh.exe only ⇒ everything from hooks.

### 7.2 Hook-based inner-CLI detection (new, daemon-side, shared wsl/ssh)
In `on_block_event` (post-token-check, blocks lock released — same slot where
`reader_prompt` is maintained):
- On `Exec{cmd}` for WslShell/Ssh sessions: `tracker::analyze_cmdline(&cmd)` — new pure
  fn: shell-words split (naive whitespace + quote-aware; unit-tested), argv[0] stem
  against the SAME adapter registry (`Match::matches` with the split argv), then the
  adapter's existing `extract` on the argv (fs-correlation branches return Ambiguous
  when `start_ft`/home are unavailable — they already do on `None`). Explicit tokens
  (`claude --resume <uuid>` etc.) come out exactly like the PEB path.
  For WslShell + claude, phase 2 (§7.3) upgrades bare launches to Correlated.
- Result folded through the EXISTING `apply_track_sample` (capture-on-change save):
  `inner_cli = Some(InnerCli{adapter, token, confidence, cwd: last hook cwd})`.
- On the `pre` that CLOSES the block whose exec set the inner CLI: clear it (the CLI
  exited back to the prompt) — same lifecycle the process tracker provides for pwsh.
  Implementation: remember `(epoch, start_off)` of the block that set it; clear when
  that rec closes. Daemon-restart-safe: inner_cli is persisted; a dangling open block
  at restore means the CLI was still running ⇒ resume wrapper fires — correct.
- Justification: the exec hook line IS the argv the adapters were built to parse; no
  new adapter code, no polling, no remote cost.

### 7.3 Claude-in-WSL correlation (P6a phase 2)
- init hook now reports `home` (e.g. `/home/alice`) + distro known from meta ⇒ the
  Windows-visible project dir is
  `\\wsl$\<distro>\<home>\.claude\projects\<munged-posix-cwd>\*.jsonl`.
- Reuse `claude_extract`'s candidate logic with a parameterized projects root (refactor
  its `dirs::home_dir()` into an argument): birth-within-30s may be unreliable over 9p
  (`created()` often errs) — the code already falls through to the mtime-newest +
  5s-ambiguity-gap branch, which 9p serves fine. Process start time: unavailable for
  Linux procs ⇒ pass None ⇒ mtime path only. Confidence caps at Correlated; ties ⇒
  Ambiguous, exactly today's doctrine.
- Gated behind one probe (`wsl_correlate`) before enabling; ships OFF if 9p mtimes
  prove untrustworthy (§16 Q3).

### 7.4 Restore wrappers per family (inner CLI present, confidence ≥ Correlated)
| Family | Wrapper |
|---|---|
| Pwsh | (today) `powershell -NoExit -Command . '<ps1>'; Set-Location …; <resume>` |
| WslShell | same wsl argv as §3.1.1 with `{RESTORE_TRAILING}` = `cd '<cli.cwd>' && <resume>` appended to the freshly-rotated rcfile — CLI runs, on exit the user is at a hooked prompt (pwsh -NoExit parity) |
| Cmd | `cmd.exe /K "cd /d <cli.cwd> && <resume>"` + PROMPT env (hooked prompt after the CLI exits) |
| Ssh | §3.4.1 with rc trailing = `cd '<cli.cwd>' 2>/dev/null; <resume>` |

`restore_trailing` (adapter registry) is reused verbatim — the resume COMMAND is
shell-agnostic text; only the wrapper differs. Ambiguous ⇒ shell restored + preface
info line (existing `push_info_line`), all families.

### 7.5 F1 — Nested-shell breadcrumb lifecycle (nested-resume spec)
A hook-fed exec that classifies `nested_shell_cmd` (`sudo su`/`su`/plain `bash`…)
opens `TerminalMeta.nested_chain` (persisted) + the runtime `nested_open` marker.
While the episode is live: D2 synthetic submissions that are THEMSELVES nested-shell
spawns append to the chain (cap 8, consecutive-dedupe); a tcbeacon SessionStart mints
`InnerCli{nested: true}` (attribution/preface ONLY — `cli_wants_resume` excludes it
from every auto-resume lane; the probe/registry/refine legs all skip it, spec I3).
Restore of a terminal holding a chain is SHELL-ONLY (cd-only trailing) plus the
honest re-establish preface (`tracker::nested_restore_notice`, variants A/B/C) —
never a resume across a privilege boundary (I1), never keystrokes (I2), never
silent loss (I4).

| Event | runtime marker | nested_chain | nested inner_cli |
|---|---|---|---|
| nested exec hook (`sudo su`) | SET | REPLACED (fresh opener) | kept |
| D2 synthetic nested spawn | — | append (cap/dedupe) | — |
| beacon SessionStart (marker set) | — | `cli_cwd` witnessed | minted/refined (nested:true) |
| beacon SessionEnd (source ∉ {clear,resume}) | — | kept | cleared |
| token-checked `pre` | CLEARED | CLEARED | CLEARED |
| sleep / death / power loss | dropped (runtime) | persists | persists |
| spawn success (restore) | CLEARED | kept until first pre (feeds preface first) | kept until first pre |
| delete | removed | dies with meta | dies with meta |

---

## 8. Restore synthesis (no inner CLI — the common case)

| Family | Respawn command | cwd mechanism |
|---|---|---|
| Pwsh | (today) bootstrap dot-source | live_cwd via Set-Location / CommandBuilder.cwd |
| WslShell | `wsl.exe [-d <distro>] --cd <live_cwd> --exec /bin/sh -c '<guard>'` (fresh rcfile+token) | `--cd` takes the POSIX live_cwd verbatim (D5); distro auto-boots (first prompt slower — the restore-lane stagger absorbs it) |
| Cmd | `cmd.exe` + PROMPT env | `CommandBuilder.cwd(live_cwd)` (Win namespace, is_dir-guarded as today) |
| Ssh | `ssh <user args> <keepalive> <host> "<one-shot rc>"` with rc `cd '<live_cwd>'` | remote cd inside the rc; failure prints the shell's own error (honest) |

Journal/preface/seam machinery: UNCHANGED for all (byte streams are byte streams). The
`hooked` flag in launch() extends from "pwsh stem" to "family ∈ {Pwsh, WslShell, Cmd} ∪
{Ssh if remote_hooks}" — epoch rotation, token mint, dangling-close, sidecar behavior
identical.

---

## 9. Terminal creation UX — data contract for the selector

(Visual/interaction design belongs to docs/selector-ui-spec.md — the concurrent
planner-selector deliverable. This section defines exactly what P6 exposes to it.)

```rust
/// GUI-side (gui/shells.rs — new): enumerated on dialog open, cached for the
/// dialog's lifetime, never polled (inv. 7).
pub struct ShellChoice {
    pub family: ShellFamilyTag,       // Pwsh | Wsl{distro} | Cmd | Ssh{host} | Claude | Custom
    pub label: String,                // "PowerShell", "Ubuntu (WSL)", "Command Prompt", "alice@devbox"
    pub detail: Option<String>,       // "default distro", "~/.ssh/config", version string
    pub is_default: bool,             // pwsh true; WSL default-distro flagged
    pub degraded_note: Option<String>,// §13 one-liner for the tooltip ("no exit codes" for cmd, …)
    pub fields: Vec<ChoiceField>,     // extra inputs the dialog must render for this choice
}
pub enum ChoiceField {
    Cwd { namespace: PathNamespace, hint: String },  // wsl accepts BOTH path styles (hint says so)
    WslShell { options: &'static [&'static str] },   // bash (default) | zsh | fish  (P6a.2)
    SshHostFreeform,                                  // prefilled from config; editable
    RemoteHooks { default_on: bool },                 // ssh opt-in toggle + one-line explanation
}
```

Enumerators (gui/shells.rs, unit-tested with fixture data §12 U5):
- `wsl_distros()`: read `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss\*`
  (`DistributionName` values; `DefaultDistribution` GUID marks the default; skip
  `State != 1` (installing/uninstalling)). No process spawn, works while WSL is stopped,
  immune to `wsl -l`'s UTF-16LE/localization traps (D16). Registry absent ⇒ no WSL rows.
- `ssh_hosts()`: parse `%USERPROFILE%\.ssh\config` `Host` tokens — skip patterns
  containing `*?!`, expand ONE level of `Include` (relative to ~/.ssh). Missing file ⇒
  just the freeform row.
- Creation submit → `NewTerminal` exactly as today: Wsl ⇒ `TermKind::Shell`,
  program `wsl.exe`, args `["-d", distro]` (daemon's launch/spawn adds --cd/--exec — the
  synthesized tail is NOT persisted in meta.args; it's per-spawn like the pwsh
  dot-source, so token rotation works); Cmd ⇒ program `cmd.exe`; Ssh ⇒ program
  `ssh.exe`, args `[user_args…, host]` + `shell_cfg.remote_hooks`. NewTerminal is wire-
  untouched (shell_cfg is applied by a follow-up SetShellCfg? NO — simpler: shell_cfg
  defaults are derivable at create time; the GUI create path uses the legacy
  CreateTerminal then the new appended `C2D::SetShellCfg`? REJECTED, two-step create is
  racy). Resolution: `NewTerminal` ALSO gains the appended `#[serde(default)]
  shell_cfg: Option<ShellCfg>` field — same append-only-struct rationale as
  TerminalMeta (§10.2); old tc.exe sending the shorter struct fails bincode decode on a
  NEW daemon, so the daemon's CreateTerminal decode path must tolerate it: bincode
  positional decode of a missing TRAILING Option field is an EOF error, therefore the
  proto bump to 5 exists precisely so a stale tc.exe (proto≤4) is warned by its
  existing version-skew check; `--install` copies both exes atomically (documented ops
  reality). One-line justification: a single spec-complete create beats a two-frame
  race, and the skew window is the install copy-race we already manage.

Sidebar/dashboard: family glyph next to the existing status dot (text glyph, no new
chrome — doctrine): `wsl`/`cmd`/`ssh` dimmed suffix in the two-line row's second line,
where the cwd already renders. Zero new borders.

---

## 10. Protocol & state changes (append-only, complete list)

### 10.1 Wire (src/protocol.rs) — proto 4 → 5
- `C2D::SubmitCommand { id: Uuid, cmd: String, write: bool }` — appended after `Ctl`
  (§5.2). FULL-scope (legacy-verb guard already drops it for scoped conns).
- NO new D2C variants (synthetic blocks ride existing `Blocks` incrementals).
- `CtlRequest`/`CtlBody`: UNCHANGED in shape; `Run` on a Cmd-family terminal internally
  routes through the synthetic-open helper (RunStarted/RunDone semantics preserved;
  RunDone.exit = None for cmd — already an Option).
- `CtlTerm`: gains NOTHING in v1 (family is derivable client-side from program; tc's
  JSON contract stays frozen). §16 Q5 tracks adding `family` explicitly.

### 10.2 State (src/state.rs)
- `TerminalMeta` += `#[serde(default)] pub shell_cfg: Option<ShellCfg>` (LAST field).
  state.json: old→new loads (missing = None), new→old loads (serde ignores unknown —
  no deny_unknown_fields anywhere). bincode Snapshot: GUI+daemon same-exe (inv. 3).
- `NewTerminal` += same appended field (§9 justification).
- `ShellFamily` + `shell_family()` + `path_namespace()` + `ShellCfg` (§2).
- `BlockRec`: UNCHANGED (no provenance field in v1 — `exit:None` already renders
  honestly; §16 Q4).

### 10.3 Hook payloads (daemon-internal, serde-lenient — no version event)
- `InitPayload` += `#[serde(default)] home: String, user: String, shell: String`.
- `PrePayload` unchanged (cmd's static pre rides the lenient defaults: e=None, n=0,
  d="" → cwd substituted from osc_cwd §3.3.1).

### 10.4 daemon.json
- `DaemonInfo.proto = 5`. Version-skew warning machinery already exists.

---

## 11. File-by-file plan

| File | Changes |
|---|---|
| `src/state.rs` | ShellFamily enum + `shell_family()` + `PathNamespace` + `ShellCfg`; appended `shell_cfg` on TerminalMeta + NewTerminal; unit tests (classifier table, posix normalize helpers if placed here) |
| `src/protocol.rs` | `C2D::SubmitCommand` appended; proto const/comment 5; append-point comments updated (SubmitCommand is now the C2D tail) |
| `src/daemon/bootstrap.rs` | Refactor to per-family generators: `write_ps1` (today's, renamed), `write_bashrc(id, token, restore_trailing) -> PathBuf` (§3.1.2 template), `cmd_prompt_value(token, user_prompt) -> String` (§3.3.1), `ssh_remote_command(rc_bytes) -> String` (base64 + guard §3.4.1), `wsl_mnt_path(&Path) -> Option<String>`; `mint_token` shared; per-family script extensions (`<id>.ps1` / `<id>.bashrc`); zsh/fish generators stubbed behind ShellCfg.shell (P6a.2); quoting/golden unit tests |
| `src/daemon/session.rs` | `spawn()` family branch: WslShell argv synthesis (+`WSLENV` env append), Cmd `PROMPT` env, Ssh argv synthesis; per-session `PathNamespace` threaded into `OscScanner`/`parse_osc_cwd` (posix branch preserving `/`); no reader/ingest changes |
| `src/daemon/mod.rs` | `launch()`: `hooked` per family (D1 classifier); restore synthesis table §8 (family match replacing the pwsh-only wrapper block); posix-aware live_cwd validity (§4); `on_block_event`: cmd cwd substitution (§3.3.1), hook-based inner-CLI set/clear (§7.2); `SubmitCommand` handler + `open_synthetic` helper (§5.2); Cmd run-gate additions (§5.3); tracker tick family routing (§7.1) |
| `src/daemon/tracker.rs` | `analyze_cmdline(&str) -> Option<InnerCli>` (shell-words split + registry reuse); `claude_extract` projects-root parameterization (§7.3); unit tests (quoted args, remote paths, `--resume=<id>` forms) |
| `src/daemon/blocks.rs` | Nothing structural; `InitPayload` optional fields; (synthetic opens reuse `open_block`) |
| `src/gui/shells.rs` (new) | `ShellChoice` catalog + `wsl_distros()` (Lxss registry) + `ssh_hosts()` (config parse); fixture-driven unit tests |
| `src/gui/mod.rs` | Creation dialog consumes ShellChoice rows (visuals per selector-ui-spec.md); family glyph in rows; SubmitCommand routing for Cmd; observed-Enter capture (§5.2); strip hint for cmd multi-line refusal |
| `src/gui/composer.rs` | `clear_chord(backend, family)` (ESC for Cmd); submit route branch; multi-line refusal for Cmd; no gate() change (signals identical) |
| `src/gui/term_backend.rs` | Nothing (BlockFeed/anchors/prompt_end shell-agnostic) — verify-only |
| `src/ctl.rs` / `src/bin/tc.rs` | `run` on Cmd family: no CLI change (daemon routes); docs snippet for family behavior |
| `src/probe.rs` | §12 probes + skip-with-notice helpers (`wsl_available()` via Lxss registry, `sshd_local()` via port 22 connect) |
| `docs/p6-shells-spec.md` | this file |

---

## 12. Probes & tests

Unit (cargo, deterministic, no environment deps):
- **U1** `shell_family` classification table (incl. exotic-wsl-args ⇒ Other; ssh flag
  skipping; case-insensitivity).
- **U2** `wsl_mnt_path`: `C:\A B\c.bashrc` → `/mnt/c/A B/c.bashrc`; UNC ⇒ None.
- **U3** posix normalize: `/` stays `/`; `/home/z/` → `/home/z`; empty ⇒ None; win
  namespace regression suite untouched.
- **U4** template/quoting goldens: bashrc fill (token + restore trailing with `'` in
  cwd), cmd PROMPT value (user PROMPT wrap), ssh remote command (base64 round-trip,
  single-argv shape, fish-safe outer).
- **U5** enumerator parsers on fixtures: Lxss registry shape (mock via injected reader),
  ssh config (Host lists, wildcard skip, Include).
- **U6** `analyze_cmdline`: `claude --resume <uuid>` Explicit; `codex resume <uuid>`;
  quoted paths; bare `claude` ⇒ candidate-with-Ambiguous; non-adapter ⇒ None.
- **U7** SubmitCommand gating: multi-line refused; synthetic open closes on next pre;
  write:false writes zero PTY bytes (mock writer).

Probes (suite currently 36; all new cases skip-with-notice when the environment lacks
the prerequisite — printed as `SKIP(<case>): <reason>` and counted separately so a
skip never masquerades as green):
- **P1 `wsl_hooks`** (needs ≥1 Lxss distro): create WslShell terminal → await
  token-checked init (assert shell/home fields) + pre + 133;B → `tc run 'echo TC_WSL_OK'`
  → assert block rec: exit 0, posix cwd, output contains TC_WSL_OK; assert composer
  gate inputs via PromptState (at_prompt, clean).
- **P2 `wsl_composer_semantics`**: bracketed-paste mode observed from remote readline;
  Ctrl+C at prompt ⇒ NEW pre+133;B re-latch (pins the D15/bash-PROMPT_COMMAND-after-^C
  assumption); multi-line submit ⇒ ONE block whose cmd contains both lines.
- **P3 `cmd_hooks`**: spawn Cmd → assert pre(token, e=None)+9;9+133;B and NO exec ever;
  SubmitCommand{write:true} `echo CMD_OK` → synthetic block closes on next pre, output
  has CMD_OK, exit None; SubmitCommand{write:false} records without PTY write (journal
  head unmoved); run-gate refusal while a `ping -t` runs (cursor/quiet gate, D14).
- **P4 `wsl_restore`** (WSL present): cd inside WSL (`cd /tmp`), graceful daemon restart
  → assert respawn argv contains `--cd /tmp` (state) + first pre reports `/tmp` + seam
  rules hold (existing remnant helpers).
- **P5 `wsl_inner_cli`**: type `claude --resume <fake-uuid>` as a NON-EXISTENT command
  is unsafe — instead run a stub script named `claude` on PATH inside the distro? NO
  (inv.: no installs). The probe fabricates the exec hook by running
  `printf '\033]7717;<token>;exec;<hex(claude --resume <uuid>)>\007'` — REJECTED
  (spoofing our own scanner = not end-to-end). RESOLUTION: assert `analyze_cmdline` at
  unit level (U6) + probe asserts the STATE fold path with a real hook by running the
  harmless real command `echo` wrapped as adapter-shaped? Also fake. Ship: unit U6 +
  P1's real-hook plumbing + a `wsl_inner_cli` probe that runs ONLY when `claude` exists
  in the distro (`command -v claude` via one wsl.exe call), else SKIP-notice. Honesty
  over coverage theater.
- **P6 `ssh_bootstrap_local`**: if WSL present, execute the generated REMOTE command
  string (minus the ssh transport) via `wsl.exe --exec sh -c '<remote-cmd-body>'` in a
  terminal — proves mktemp/base64/exec-bash/rc/self-delete end-to-end through a real
  ConPTY; assert hooked prompt + one block round-trip. If localhost sshd answers
  (port 22), ALSO run the full `ssh 127.0.0.1` variant (key-auth only; skip on auth
  failure with notice). No WSL and no sshd ⇒ SKIP-notice; the command-string goldens
  (U4) still pin the synthesis.
- **P7 `shell_catalog`** (report-only): print enumerated distros/hosts; never asserts
  (environment-shaped).
- **P8 `cmd_restore`**: kill + restore a Cmd terminal with live_cwd changed via `cd`
  (9;9-tracked); assert respawn cwd + PROMPT env re-injected (hooks live again) +
  blocks sidecar continuity (epoch bumped, old recs intact).

Interactive checklist additions (screenshot QA per ops notes): WSL terminal cover shows
`❯ /home/…` posix cwd; cmd strip shows the no-exit-codes hint on hover; multi-row bash
prompt covers only the input row; ssh password prompt renders raw (no cover).

---

## 13. Degraded-mode honesty table (feature × shell)

Legend: ● full · ◐ partial (note) · ○ unavailable (honest label) — every ◐/○ appears in
the selector tooltip (§9 degraded_note) and nowhere pretends otherwise.

| Feature | pwsh (today) | WSL bash | WSL zsh/fish (P6a.2) | WSL degraded (no automount/bash) | cmd | ssh hooked | ssh plain (opt-out / bash absent) | Custom/other |
|---|---|---|---|---|---|---|---|---|
| Block records (cmd, cwd, duration) | ● | ● | ● | ○ | ◐ composed/`tc run`/GUI-observed only; typed-while-GUI-closed ○ | ● | ○ | ○ |
| Exit codes | ◐ (cmdlet folding) | ● (`$?`) | ● | ○ | ○ (permanent — PROMPT can't render ERRORLEVEL) | ● | ○ | ○ |
| Composer auto-arm + prompt cover | ● | ● | ● | ○ (Raw NoPrompt) | ● (weak at_prompt guarded by cursor_clean) | ● (link-latency holds honest) | ○ | ○ |
| Reclaim / race-window recovery | ● | ● | ● | ○ | ● | ● (wider race, same guards) | ○ | ○ |
| Cross-session history (P4 popup) | ● | ● | ● | ○ | ◐ (recorded subset above) | ● | ○ | ○ |
| CLI persistence (claude et al.) | ● Explicit+Correlated | ● Explicit; Correlated via \\wsl$ P6a.2 (§7.3) | same | ○ (no hooks) | ● (Win32 tracker — full parity with pwsh) | ◐ Explicit only; bare launch ⇒ Ambiguous info line | ○ | ◐ (Win32 tracker still sees children) |
| Live cwd tracking / restore-in-cwd | ● | ● (hooks) | ● | ◐ (spawn cwd only) | ● (9;9 + PEB) | ● (hooks) | ◐ (respawn at meta.cwd, remote cwd lost) | ◐ (PEB) |
| Activity states (Working/Idle/NeedsYou) | ● | ● | ● | ● (output-based signals shell-agnostic) | ● | ● | ● | ● |
| `tc run` gating / RunDone | ● | ● | ● | ○ (refused: hooks not live) | ◐ (synthetic block; exit None; D14 gate) | ● | ○ (refused) | ○ (refused) |
| Scrollback persistence / restore seams / win32-input | ● | ● | ● | ● | ● | ● | ● | ● |

(The bottom row is the point: the P0-era guarantees — journals, seamless restore,
typing fidelity — are shell-independent and every family gets them unconditionally.)

Also stated plainly: a pre-existing user DEBUG trap in bash is replaced by ours
(logged); commands typed in cmd with no GUI attached are not recorded; ssh hook tokens
transit the channel and rest in remote /tmp for the bootstrap window (inv. 8).

---

## 14. Performance constraints

- Hook scanning: zero new cost (same BlockScanner; memchr ESC gate already skips plain
  text). The bash hooks add ~2 OSC writes + one 15ms prompt-render sleep per prompt —
  the pwsh-measured envelope.
- No periodic `wsl.exe` spawns anywhere (inv. 7): enumeration = registry read on dialog
  open; `command -v claude` checks happen only inside probes.
- Tracker tick: WslShell/Ssh sessions SKIP the Toolhelp descendant walk (cheaper than
  today, not dearer); the activity gate semantics unchanged.
- `\\wsl$` correlation (P6a.2) runs only in the restore/track slow paths, never per
  tick, and is bounded by the same read_dir the pwsh claude adapter does locally.
- Restore lanes: a cold distro boot on the first WSL restore can take seconds — it
  occupies one of the 4 restore lanes without blocking the others; no tuning change.
- QoS: `set_high_qos` applies to wsl.exe/ssh.exe/cmd.exe pids exactly as to
  powershell.exe (session::spawn is family-agnostic there) — conhost sweep unchanged.

---

## 15. Phasing & verification bar (each phase independently shippable)

**P6a — WSL bash (highest value, full hooks)**
1. state.rs classifier + namespaces; bootstrap bash template; session/mod spawn+restore;
   tracker routing + analyze_cmdline; GUI selector data (WSL rows) + composer signals
   (no behavior change needed for bash beyond family plumbing).
2. Bar: cargo tests (U1–U6 subset) green; probes P1/P2/P4 green on this machine (WSL
   present) or SKIP-notice honestly recorded; existing 36-probe suite untouched-green
   against an isolated proto-5 daemon (TC_DATA_DIR); zero warnings both bins; install
   per the documented copy-race dance; live screenshot QA (cover with posix cwd).
   P6a.2 fast-follow: zsh/fish templates + `wsl_correlate` probe gate (§7.3).

**P6b — cmd (small, mostly-degraded, high daily utility)**
1. PROMPT env injection; SubmitCommand (proto 5 lands HERE if P6a shipped without it —
   otherwise both ship together; either way the bump is a single release); composer
   routing + ESC chord + multi-line refusal; observed-Enter capture; D14 gate.
2. Bar: U4/U7 + P3/P8 green; `tc run` on cmd returns RunDone{exit:None} correctly;
   degraded notes visible in selector tooltip.

**P6c — ssh (opt-in remote hooks)**
1. ssh synthesis + one-shot rc + keepalive args; remote_hooks toggle end-to-end;
   hook-only tracker path; restore-with-remote-resume.
2. Bar: U4 goldens + P6 (WSL-transport variant at minimum) green; auth-prompt QA
   (no cover over "Password:"); Dead-on-link-drop within the ServerAlive envelope
   verified against a real host once, manually, recorded in the memory file.

---

## 16. Open questions (with defaults — implementation proceeds on the default)

| # | Question | Default |
|---|---|---|
| Q1 | ssh auto-reconnect (bounded backoff, key-auth-only, opt-in per host)? | OFF in v1 (D13); collect Dead-frequency evidence first |
| Q2 | cmd multi-line submission ledger (queue N synthetic blocks closed by successive pres, flushed on uncertainty)? | NOT in v1 — refuse multi-line for cmd; ledger is designed (§5.2 note) if demand appears |
| Q3 | Enable \\wsl$ claude correlation by default once probed? | ON if `wsl_correlate` proves 9p mtimes stable; else ship OFF behind ShellCfg |
| Q4 | `BlockRec.source` provenance field (hook vs synthetic vs observed)? | NO in v1 — exit:None already communicates the trust level; add only if UI needs to distinguish |
| Q5 | `CtlTerm.family` field for tc JSON consumers? | NO in v1 — derivable from `program`; append (JSON-additive, safe) when a consumer asks |
| Q6 | WSL default-shell autodetect (`getent passwd`) instead of fixed bash? | NO — explicit shell is the provable contract (D2); zsh/fish arrive as explicit choices |
| Q7 | Keepalive flags also for user-supplied `-o ServerAlive*`? | User args win (first-occurrence-wins ssh semantics; ours appended after) |
| Q8 | Hook cmd.exe via clink when present (would give real pre/exec hooks)? | NO in v1 — third-party dependency detection is a support surface; revisit on user demand |

---

## 17. DO-NOTs (hard rules for the implementer)

1. Do NOT write to any user/remote config: no remote `~/.bashrc`/`~/.profile`, no
   AutoRun registry key, no wsl.conf, no doskey macrofiles. All injection is per-spawn
   argv/env/generated-file (inv. 1).
2. Do NOT type bootstrap text into any PTY, ever (pwsh drops startup PTY input; every
   shell has an argv/env path — use it).
3. Do NOT translate POSIX cwds to `\\wsl$`/Windows form in records, state, or restore
   commands — verbatim namespace only (D5). `\\wsl$` exists solely inside §7.3.
4. Do NOT run `normalize_win_path` on a Posix-namespace path (it destroys `/`), and do
   NOT call `.is_dir()`/`Set-Location` on one.
5. Do NOT parse `wsl -l` / `wsl -l -v` output (UTF-16LE, localized) — Lxss registry only.
6. Do NOT trust cmd's `at_prompt` latch for anything input-writing without the
   cursor-col + output-quiet check (D14) — no exec hook means the latch lies mid-command.
7. Do NOT emit synthetic blocks for commands the daemon didn't see submitted or a GUI
   didn't observe rendered — no journal-text-guessing of typed cmd commands (echo bytes
   are editing-key soup; the grid reclaim is the only honest witness).
8. Do NOT reorder/insert-mid-enum any protocol type; `SubmitCommand` goes at the C2D
   END; ShellCfg fields are append-only-with-serde-default forever.
9. Do NOT let the tracker spawn `wsl.exe` (or ssh anything) on its tick — hooks are the
   only remote/WSL witness in steady state (inv. 7).
10. ~~Do NOT auto-reconnect ssh~~ **(SUPERSEDED proto 10 — bounded auto-reconnect ships in
    daemon/reconnect.rs, gated on links that hooked; see D13.)** Still: never auto-answer any
    auth prompt; never paint a cover while at_prompt is false (password prompts must render raw).
11. Do NOT replace the user's PROMPT_COMMAND/precmd/PROMPT — wrap/append idempotently,
    ours-first only where `$?` capture demands it (§3.1.2).
12. Do NOT touch win32_input.rs, serialize.rs, journal.rs, waiters.rs, or the resize
    pipeline for this phase (D18) — any perceived need means the design above is being
    violated somewhere else.
13. Do NOT probe against the user's live daemon; TC_DATA_DIR isolation + no `--install`
    combination, per standing ops doctrine. Probes that need WSL/sshd must SKIP with a
    printed notice, never silently pass.
14. Do NOT show a second input box or any new chrome stroke for the new families — the
    strip/cover/one-prompt doctrine applies unchanged (ux-doctrine.md).
