//! pulse-ctl — the Pulse controller CLI (P5; historically `tc`).
//!
//! Compiled into BOTH binaries: `src/bin/tc.rs` (the console-subsystem
//! `tc.exe`, the documented interface) and the main exe as
//! `pulse ctl …` (debug convenience). Speaks the internal bincode
//! protocol over the daemon's loopback port and prints stable JSON —
//! envelope `{"v":1,"ok":true,…}` / `{"v":1,"ok":false,"code":…,"msg":…}` —
//! one object per invocation (JSON-lines for `watch`).
//!
//! Exit codes: 0 ok · 1 transport/usage (no_daemon, proto_skew, usage,
//! internal) · 2 refused by policy/gate · 3 timeout · 4 target resolution
//! (not_found, ambiguous). Agents can branch without parsing.

use std::net::TcpStream;
use std::time::Duration;

use uuid::Uuid;

use crate::protocol::{
    read_frame, write_frame, C2D, CtlBody, CtlChord, CtlEvent, CtlRequest, CtlTerm, D2C,
    DaemonInfo, RunWait, WaitCond, WaitHit, EV_BLOCKS, EV_EXIT, EV_STATE, SCOPE_INPUT,
    SCOPE_MANAGE, SCOPE_READ,
};
use crate::state::{daemon_info_path, NewTerminal, TermKind};

const DEFAULT_RUN_TIMEOUT_SECS: u64 = 60;
const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 30;
const DEFAULT_RUN_TAIL: u32 = 8192;
const DEFAULT_READ_LINES: u32 = 100;
const DEFAULT_BLOCKS_LAST: u32 = 20;
/// Baseline socket read timeout for one-shot verbs.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(args: Vec<String>) -> i32 {
    // Measurement + latency consistency: a hidden background CLI gets parked
    // on E-cores under foreground load exactly like the daemon did (see
    // procinfo::set_high_qos — this is the same one-call opt-out).
    set_high_qos_self();
    // Attribution Layer 2: the claude hook shim bypasses the whole CLI
    // grammar/JSON envelope — it must NEVER print and NEVER exit non-zero
    // (claude surfaces failing hooks to the user).
    if args.first().map(String::as_str) == Some("__claude-hook") {
        cli_hook_report("claude", args.get(1).map(String::as_str));
        return 0;
    }
    // Attribution Layer 2 (codex, Windows-native lane): codex fires
    // ~/.codex/hooks.json commands with its full env, so the hook inherits
    // TC_SESSION_ID and reads session_id off stdin — same shim, adapter
    // "codex". Silent + exit 0 always (codex surfaces failing hooks).
    if args.first().map(String::as_str) == Some("__codex-hook") {
        cli_hook_report("codex", args.get(1).map(String::as_str));
        return 0;
    }
    let cmd = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    match execute(cmd) {
        Ok(()) => 0,
        Err(e) => fail(&e),
    }
}

/// `tc __claude-hook <event>` / `tc __codex-hook <event>` — the SessionStart
/// hook command TC installs for the given CLI (claude: launch_command's
/// `--settings`; codex: ~/.codex/hooks.json). Reads the hook's stdin JSON
/// (`{session_id, hook_event_name, source|reason, …}`; claude's sid also
/// rides env CLAUDE_CODE_SESSION_ID, codex's is stdin-only) and posts a
/// ReportCliSession for the terminal named by the inherited TC_SESSION_ID.
/// Contract: silent + exit 0 on EVERY path — both CLIs treat hook stdout as
/// context and a non-zero exit as a user-visible error; a dead daemon or a
/// pre-proto-11 daemon must cost the user nothing.
fn cli_hook_report(adapter: &str, argv_event: Option<&str>) {
    let adapter = adapter.to_string();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let Some(id) = std::env::var("TC_SESSION_ID")
            .ok()
            .and_then(|s| Uuid::parse_str(s.trim()).ok())
        else {
            return; // not inside a managed terminal
        };
        let mut buf = Vec::new();
        {
            use std::io::Read;
            let stdin = std::io::stdin();
            let _ = stdin.lock().take(64 * 1024).read_to_end(&mut buf);
        }
        let v: serde_json::Value =
            serde_json::from_slice(&buf).unwrap_or(serde_json::Value::Null);
        // CLAUDE_CODE_SESSION_ID is claude's export only — a codex launched
        // inside a claude context inherits it, so the fallback must never
        // apply to the codex adapter (it would report claude's uuid as the
        // codex session and restore would resume the wrong conversation).
        let Some(sid) = v["session_id"]
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                (adapter == "claude")
                    .then(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok())
                    .flatten()
            })
            .filter(|s| !s.trim().is_empty())
        else {
            return;
        };
        let event = v["hook_event_name"]
            .as_str()
            .or(argv_event)
            .unwrap_or("SessionStart")
            .to_string();
        let source = v["source"]
            .as_str()
            .or_else(|| v["reason"].as_str())
            .unwrap_or("")
            .to_string();
        let Ok(mut conn) = connect() else { return };
        if conn.info.proto < 11 {
            return; // pre-attribution daemon can't decode the verb
        }
        let _ = conn.call(CtlRequest::ReportCliSession {
            id,
            adapter,
            event,
            source,
            session_id: sid,
        });
    }));
}

// ────────────────────────────── errors/JSON ──────────────────────────────

#[derive(Debug)]
pub struct CliErr {
    pub code: String,
    pub msg: String,
}

impl CliErr {
    fn new(code: &str, msg: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            msg: msg.into(),
        }
    }
}

fn exit_for(code: &str) -> i32 {
    match code {
        "timeout" => 3,
        "not_found" | "ambiguous" => 4,
        "no_daemon" | "proto_skew" | "usage" | "internal" => 1,
        _ => 2, // busy, alt_screen, not_hooked, hooks_unverified, self_target,
                // forbidden, confirm, multiline, dead, running, exited,
                // wait_limit, bad_pattern, asleep, sleeping, not_asleep,
                // not_running, …
    }
}

fn fail(e: &CliErr) -> i32 {
    print_json(&serde_json::json!({
        "v": 1, "ok": false, "code": e.code, "msg": e.msg,
    }));
    if e.code == "usage" {
        eprintln!("{USAGE}");
    }
    exit_for(&e.code)
}

fn print_json(v: &serde_json::Value) {
    println!("{v}");
}

const USAGE: &str = r#"pulse-ctl — Pulse controller CLI (JSON out)

  tc list                                        [--folder <name>]
  tc create   --name <s> [--folder <name>] [--cwd <dir>]
              [--kind shell|claude|custom] [--program <exe>] [--arg <a>]...
              [--claude-session <uuid>]
  tc run      <term> <command...> [--force] [--force-self] [--multi]
              [--no-wait] [--timeout <secs=60>] [--tail <bytes=8192>]
  tc send     <term> (--text <s> [--enter] | --b64 <base64> | --key <chord>)
              [--force-self]
  tc read     <term> [--screen | --tail [--lines <n=100>]]
  tc blocks   <term> [--last <n=20>]
  tc block-text <term> <start_off>
  tc wait     <term> --for (block-close [--after <off>] | prompt | exit
              | output <pattern> [--regex] [--from <off>]) [--timeout <secs=30>]
  tc kill     <term> [--force-self]
  tc restart  <term> [--force-self]
  tc sleep    (<term> [--force] [--force-self] | --folder <name> [--force])
  tc wake     (<term> | --folder <name>)
  tc delete   <term> --yes [--force-self]
  tc watch    [--id <term>]... [--events blocks,exit,state]
  tc token    (create --name <s> --scope read|input|manage
              | revoke --name <s> | list)
  tc info

<term> = terminal UUID, exact name, or unambiguous case-insensitive prefix.
Chords: enter esc tab backspace up down left right home end pgup pgdn
        ctrl+c ctrl+d ctrl+z ctrl+l
Env: TC_CTL_TOKEN scopes this process (else the master token is used);
     TC_SESSION_ID (set automatically inside managed terminals) enables the
     refuse-to-target-self guard.
`read --tail` is the journal (works for dead terminals, ANSI-stripped);
`read --screen` is the live grid (TUIs). A stripped TUI journal is redraw
soup — use --screen for those.
`sleep` kills the process tree but keeps journal/blocks/resume identity
persisted (boot restore skips it); `wake` relaunches through the restore
path. Busy terminals (open block / output <3s) refuse without --force;
folder sleep without --force skips busy members. run/send on an asleep
terminal refuse code "asleep" — waking is always explicit."#;

// ─────────────────────────────── arg parsing ───────────────────────────────

#[derive(Debug, PartialEq)]
pub enum SendPayload {
    Text { text: String, enter: bool },
    B64(Vec<u8>),
    Key(CtlChord),
}

#[derive(Debug, PartialEq)]
pub enum WaitSpec {
    BlockClose { after: Option<u64> },
    Prompt,
    Exit,
    Output {
        pattern: String,
        regex: bool,
        from: Option<u64>,
    },
}

#[derive(Debug, PartialEq)]
pub enum Cmd {
    List {
        folder: Option<String>,
    },
    Create {
        name: String,
        folder: Option<String>,
        cwd: Option<String>,
        kind: String,
        program: Option<String>,
        args: Vec<String>,
        claude_session: Option<Uuid>,
    },
    Run {
        term: String,
        cmd: String,
        force: bool,
        force_self: bool,
        multi: bool,
        no_wait: bool,
        timeout_secs: u64,
        tail: u32,
    },
    Send {
        term: String,
        payload: SendPayload,
        force_self: bool,
    },
    Read {
        term: String,
        tail: bool,
        lines: u32,
    },
    Blocks {
        term: String,
        last: u32,
    },
    BlockText {
        term: String,
        start_off: u64,
    },
    Wait {
        term: String,
        spec: WaitSpec,
        timeout_secs: u64,
    },
    Kill {
        term: String,
        force_self: bool,
    },
    Restart {
        term: String,
        force_self: bool,
    },
    /// SLEEP: exactly one of `term`/`folder` is Some (parser-enforced).
    Sleep {
        term: Option<String>,
        folder: Option<String>,
        force: bool,
        force_self: bool,
    },
    Wake {
        term: Option<String>,
        folder: Option<String>,
    },
    Delete {
        term: String,
        yes: bool,
        force_self: bool,
    },
    Watch {
        ids: Vec<String>,
        kinds: u32,
    },
    TokenCreate {
        name: String,
        scope: u32,
    },
    TokenRevoke {
        name: String,
    },
    TokenList,
    Info,
}

fn usage(msg: impl Into<String>) -> CliErr {
    CliErr::new("usage", msg)
}

/// Hand-rolled verb + flags loop (the --probe house style; the grammar is a
/// dozen flags — clap would be the crate's first arg dependency for zero
/// expressive gain).
pub fn parse_args(args: &[String]) -> Result<Cmd, CliErr> {
    let mut it = args.iter().map(String::as_str);
    let verb = it.next().ok_or_else(|| usage("no verb"))?;
    let rest: Vec<&str> = it.collect();

    // Small helpers over `rest`.
    let flag_val = |name: &str| -> Result<Option<String>, CliErr> {
        for (i, a) in rest.iter().enumerate() {
            if *a == name {
                return match rest.get(i + 1) {
                    Some(v) => Ok(Some((*v).to_string())),
                    None => Err(usage(format!("{name} needs a value"))),
                };
            }
        }
        Ok(None)
    };
    let has_flag = |name: &str| rest.contains(&name);

    match verb {
        "list" => {
            for (i, a) in rest.iter().enumerate() {
                match *a {
                    "--folder" => {
                        if rest.get(i + 1).is_none() {
                            return Err(usage("--folder needs a value"));
                        }
                    }
                    "--all-fields" => {} // accepted; output always carries all fields
                    v if v.starts_with("--") && rest.get(i.wrapping_sub(1)) != Some(&"--folder") => {
                        return Err(usage(format!("unknown flag {v}")))
                    }
                    _ => {}
                }
            }
            Ok(Cmd::List {
                folder: flag_val("--folder")?,
            })
        }
        "create" => {
            let name = flag_val("--name")?.ok_or_else(|| usage("create needs --name"))?;
            let kind = flag_val("--kind")?.unwrap_or_else(|| "shell".into());
            if !matches!(kind.as_str(), "shell" | "claude" | "custom") {
                return Err(usage("--kind must be shell|claude|custom"));
            }
            let mut extra = Vec::new();
            let mut i = 0;
            while i < rest.len() {
                if rest[i] == "--arg" {
                    match rest.get(i + 1) {
                        Some(v) => extra.push((*v).to_string()),
                        None => return Err(usage("--arg needs a value")),
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            let claude_session = match flag_val("--claude-session")? {
                Some(s) => Some(
                    Uuid::parse_str(&s)
                        .map_err(|_| usage("--claude-session must be a UUID"))?,
                ),
                None => None,
            };
            Ok(Cmd::Create {
                name,
                folder: flag_val("--folder")?,
                cwd: flag_val("--cwd")?,
                kind,
                program: flag_val("--program")?,
                args: extra,
                claude_session,
            })
        }
        "run" => {
            // Grammar: `run [tc-flags] <term> [tc-flags] [--] <command…>`.
            // tc's own flags are recognized only while LEADING — before the
            // command's first word. The first token after the terminal ref
            // that isn't a tc flag starts the command, and from there
            // everything is the command VERBATIM: `tc run t git clean
            // --force` submits `git clean --force`, it does not arm tc's
            // busy-gate bypass. `--` ends tc's flags explicitly (required
            // for a command whose first word itself starts with `--`).
            // Consumers are agents composing commands programmatically —
            // silently-rewritten submissions are a correctness defect.
            let mut term: Option<String> = None;
            let mut words: Vec<String> = Vec::new();
            let mut force = false;
            let mut force_self = false;
            let mut multi = false;
            let mut no_wait = false;
            let mut timeout_secs = DEFAULT_RUN_TIMEOUT_SECS;
            let mut tail = DEFAULT_RUN_TAIL;
            let mut i = 0;
            let mut cmd_started = false;
            while i < rest.len() {
                let w = rest[i];
                if !cmd_started {
                    let mut consumed = true;
                    match w {
                        "--" => cmd_started = true,
                        "--force" => force = true,
                        "--force-self" => force_self = true,
                        "--multi" => multi = true,
                        "--no-wait" => no_wait = true,
                        "--timeout" => {
                            timeout_secs = parse_num(&rest, &mut i, "--timeout")?;
                        }
                        "--tail" => {
                            tail = parse_num(&rest, &mut i, "--tail")? as u32;
                        }
                        _ => consumed = false,
                    }
                    if consumed {
                        i += 1;
                        continue;
                    }
                    if term.is_none() {
                        // Reject a flag-looking token as the terminal ref —
                        // it is almost certainly a typo'd tc flag, and
                        // treating it as a name would resolve nothing.
                        if w.starts_with("--") {
                            return Err(usage(format!("unknown flag {w}")));
                        }
                        term = Some(w.to_string());
                        i += 1;
                        continue;
                    }
                    // First non-flag token after the terminal ref: the
                    // command starts here.
                    cmd_started = true;
                }
                words.push(w.to_string());
                i += 1;
            }
            let term = term.ok_or_else(|| usage("run needs <term> <command>"))?;
            if words.is_empty() {
                return Err(usage("run needs a command"));
            }
            Ok(Cmd::Run {
                term,
                cmd: words.join(" "),
                force,
                force_self,
                multi,
                no_wait,
                timeout_secs,
                tail,
            })
        }
        "send" => {
            let term = rest
                .first()
                .filter(|a| !a.starts_with("--"))
                .ok_or_else(|| usage("send needs <term>"))?
                .to_string();
            let payload = if let Some(text) = flag_val("--text")? {
                SendPayload::Text {
                    text,
                    enter: has_flag("--enter"),
                }
            } else if let Some(b64) = flag_val("--b64")? {
                SendPayload::B64(
                    b64_decode(&b64).ok_or_else(|| usage("--b64 payload is not valid base64"))?,
                )
            } else if let Some(k) = flag_val("--key")? {
                SendPayload::Key(
                    parse_chord(&k).ok_or_else(|| usage(format!("unknown chord {k}")))?,
                )
            } else {
                return Err(usage("send needs --text, --b64, or --key"));
            };
            Ok(Cmd::Send {
                term,
                payload,
                force_self: has_flag("--force-self"),
            })
        }
        "read" => {
            let term = rest
                .first()
                .filter(|a| !a.starts_with("--"))
                .ok_or_else(|| usage("read needs <term>"))?
                .to_string();
            let tail = has_flag("--tail");
            if tail && has_flag("--screen") {
                return Err(usage("--screen and --tail are exclusive"));
            }
            let lines = match flag_val("--lines")? {
                Some(v) => v
                    .parse()
                    .map_err(|_| usage("--lines must be a number"))?,
                None => DEFAULT_READ_LINES,
            };
            Ok(Cmd::Read { term, tail, lines })
        }
        "blocks" => {
            let term = rest
                .first()
                .filter(|a| !a.starts_with("--"))
                .ok_or_else(|| usage("blocks needs <term>"))?
                .to_string();
            let last = match flag_val("--last")? {
                Some(v) => v.parse().map_err(|_| usage("--last must be a number"))?,
                None => DEFAULT_BLOCKS_LAST,
            };
            Ok(Cmd::Blocks { term, last })
        }
        "block-text" => {
            let term = rest
                .first()
                .ok_or_else(|| usage("block-text needs <term> <start_off>"))?
                .to_string();
            let start_off = rest
                .get(1)
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| usage("block-text needs a numeric <start_off>"))?;
            Ok(Cmd::BlockText { term, start_off })
        }
        "wait" => {
            let term = rest
                .first()
                .filter(|a| !a.starts_with("--"))
                .ok_or_else(|| usage("wait needs <term>"))?
                .to_string();
            let for_pos = rest
                .iter()
                .position(|a| *a == "--for")
                .ok_or_else(|| usage("wait needs --for <condition>"))?;
            let cond_name = rest
                .get(for_pos + 1)
                .ok_or_else(|| usage("--for needs a condition"))?;
            let spec = match *cond_name {
                "block-close" => WaitSpec::BlockClose {
                    after: match flag_val("--after")? {
                        Some(v) => Some(
                            v.parse().map_err(|_| usage("--after must be a number"))?,
                        ),
                        None => None,
                    },
                },
                "prompt" => WaitSpec::Prompt,
                "exit" => WaitSpec::Exit,
                "output" => {
                    let pattern = rest
                        .get(for_pos + 2)
                        .filter(|a| !a.starts_with("--"))
                        .ok_or_else(|| usage("--for output needs a pattern"))?
                        .to_string();
                    WaitSpec::Output {
                        pattern,
                        regex: has_flag("--regex"),
                        from: match flag_val("--from")? {
                            Some(v) => Some(
                                v.parse().map_err(|_| usage("--from must be a number"))?,
                            ),
                            None => None,
                        },
                    }
                }
                other => return Err(usage(format!("unknown wait condition {other}"))),
            };
            let timeout_secs = match flag_val("--timeout")? {
                Some(v) => v
                    .parse()
                    .map_err(|_| usage("--timeout must be seconds"))?,
                None => DEFAULT_WAIT_TIMEOUT_SECS,
            };
            Ok(Cmd::Wait {
                term,
                spec,
                timeout_secs,
            })
        }
        "kill" | "restart" => {
            let term = rest
                .first()
                .filter(|a| !a.starts_with("--"))
                .ok_or_else(|| usage(format!("{verb} needs <term>")))?
                .to_string();
            let force_self = has_flag("--force-self");
            Ok(if verb == "kill" {
                Cmd::Kill { term, force_self }
            } else {
                Cmd::Restart { term, force_self }
            })
        }
        "sleep" | "wake" => {
            // Flags are LEADING-only by grammar doctrine; these verbs take
            // no command tail, so the whole arg list is flags + one target.
            let folder = flag_val("--folder")?;
            let mut term: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    "--folder" => i += 1, // value consumed below
                    "--force" if verb == "sleep" => {}
                    "--force-self" if verb == "sleep" => {}
                    w if w.starts_with("--") => {
                        return Err(usage(format!("unknown flag {w}")))
                    }
                    w => {
                        if term.is_some() {
                            return Err(usage(format!("{verb} takes one terminal")));
                        }
                        term = Some(w.to_string());
                    }
                }
                i += 1;
            }
            if term.is_some() == folder.is_some() {
                return Err(usage(format!("{verb} needs <term> or --folder <name>")));
            }
            Ok(if verb == "sleep" {
                Cmd::Sleep {
                    term,
                    folder,
                    force: has_flag("--force"),
                    force_self: has_flag("--force-self"),
                }
            } else {
                Cmd::Wake { term, folder }
            })
        }
        "delete" => {
            let term = rest
                .first()
                .filter(|a| !a.starts_with("--"))
                .ok_or_else(|| usage("delete needs <term>"))?
                .to_string();
            Ok(Cmd::Delete {
                term,
                yes: has_flag("--yes"),
                force_self: has_flag("--force-self"),
            })
        }
        "watch" => {
            let mut ids = Vec::new();
            let mut kinds = EV_BLOCKS | EV_EXIT | EV_STATE;
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    "--id" => match rest.get(i + 1) {
                        Some(v) => {
                            ids.push((*v).to_string());
                            i += 1;
                        }
                        None => return Err(usage("--id needs a value")),
                    },
                    "--events" => match rest.get(i + 1) {
                        Some(v) => {
                            kinds = 0;
                            for part in v.split(',') {
                                kinds |= match part.trim() {
                                    "blocks" => EV_BLOCKS,
                                    "exit" => EV_EXIT,
                                    "state" => EV_STATE,
                                    other => {
                                        return Err(usage(format!("unknown event kind {other}")))
                                    }
                                };
                            }
                            i += 1;
                        }
                        None => return Err(usage("--events needs a value")),
                    },
                    other => return Err(usage(format!("unknown flag {other}"))),
                }
                i += 1;
            }
            Ok(Cmd::Watch { ids, kinds })
        }
        "token" => match rest.first().copied() {
            Some("create") => {
                let name = flag_val("--name")?.ok_or_else(|| usage("token create needs --name"))?;
                let scope = match flag_val("--scope")?.as_deref() {
                    Some("read") => SCOPE_READ,
                    Some("input") => SCOPE_READ | SCOPE_INPUT,
                    Some("manage") => SCOPE_READ | SCOPE_INPUT | SCOPE_MANAGE,
                    _ => return Err(usage("token create needs --scope read|input|manage")),
                };
                Ok(Cmd::TokenCreate { name, scope })
            }
            Some("revoke") => Ok(Cmd::TokenRevoke {
                name: flag_val("--name")?.ok_or_else(|| usage("token revoke needs --name"))?,
            }),
            Some("list") => Ok(Cmd::TokenList),
            _ => Err(usage("token needs create|revoke|list")),
        },
        "info" => Ok(Cmd::Info),
        "help" | "--help" | "-h" => Err(usage("help")),
        other => Err(usage(format!("unknown verb {other}"))),
    }
}

fn parse_num(rest: &[&str], i: &mut usize, name: &str) -> Result<u64, CliErr> {
    *i += 1;
    rest.get(*i)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| usage(format!("{name} must be a number")))
}

pub fn parse_chord(s: &str) -> Option<CtlChord> {
    Some(match s.to_ascii_lowercase().as_str() {
        "enter" => CtlChord::Enter,
        "esc" | "escape" => CtlChord::Esc,
        "tab" => CtlChord::Tab,
        "backspace" => CtlChord::Backspace,
        "up" => CtlChord::Up,
        "down" => CtlChord::Down,
        "left" => CtlChord::Left,
        "right" => CtlChord::Right,
        "home" => CtlChord::Home,
        "end" => CtlChord::End,
        "pgup" | "pageup" => CtlChord::PageUp,
        "pgdn" | "pagedown" => CtlChord::PageDown,
        "ctrl+c" => CtlChord::CtrlC,
        "ctrl+d" => CtlChord::CtrlD,
        "ctrl+z" => CtlChord::CtrlZ,
        "ctrl+l" => CtlChord::CtrlL,
        _ => return None,
    })
}

/// Minimal standard-alphabet base64 decoder (raw input bytes for `send
/// --b64`; a whole crate for 25 lines would be dependency theater).
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for chunk in s.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        if chunk.len() != 4 && pad == 0 && chunk.len() < 2 {
            return None;
        }
        let mut acc = 0u32;
        let mut bits = 0u32;
        for &c in chunk.iter().filter(|&&c| c != b'=') {
            acc = (acc << 6) | val(c)?;
            bits += 6;
        }
        acc <<= (24u32).saturating_sub(bits).min(24);
        let bytes = (bits / 8) as usize;
        for k in 0..bytes {
            out.push(((acc >> (16 - 8 * k)) & 0xff) as u8);
        }
    }
    Some(out)
}

// ────────────────────────────── connection ──────────────────────────────

struct CtlConn {
    read: TcpStream,
    write: TcpStream,
    next_req: u64,
    self_session: Option<Uuid>,
    info: DaemonInfo,
}

fn connect() -> Result<CtlConn, CliErr> {
    let info: DaemonInfo = std::fs::read(daemon_info_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| {
            CliErr::new(
                "no_daemon",
                "no daemon.json — is the Pulse daemon running?",
            )
        })?;
    if info.proto < 4 {
        return Err(CliErr::new(
            "proto_skew",
            format!(
                "daemon protocol {} predates the controller API — restart it from this build",
                info.proto
            ),
        ));
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], info.port));
    let read = TcpStream::connect_timeout(&addr, Duration::from_secs(1))
        .map_err(|e| CliErr::new("no_daemon", format!("daemon not reachable: {e}")))?;
    read.set_nodelay(true).ok();
    read.set_read_timeout(Some(IO_TIMEOUT)).ok();
    let mut write = read
        .try_clone()
        .map_err(|e| CliErr::new("internal", format!("{e}")))?;
    // TC_CTL_TOKEN sandboxes this process to a scoped token; the master
    // token is the same-user default.
    let token = std::env::var("TC_CTL_TOKEN").unwrap_or_else(|_| info.token.clone());
    let self_session = std::env::var("TC_SESSION_ID")
        .ok()
        .and_then(|s| Uuid::parse_str(&s).ok());
    write_frame(
        &mut write,
        &C2D::HelloCtl {
            token,
            self_session,
        },
    )
    .map_err(|e| CliErr::new("no_daemon", format!("handshake failed: {e}")))?;
    Ok(CtlConn {
        read,
        write,
        next_req: 1,
        self_session,
        info,
    })
}

impl CtlConn {
    /// One request → its reply body (frames for other req ids and the
    /// snapshot the daemon sends on connect are skipped).
    fn call(&mut self, req: CtlRequest) -> Result<CtlBody, CliErr> {
        let req_id = self.next_req;
        self.next_req += 1;
        write_frame(&mut self.write, &C2D::Ctl { req_id, req })
            .map_err(|e| CliErr::new("no_daemon", format!("send failed: {e}")))?;
        loop {
            match read_frame::<_, D2C>(&mut self.read) {
                Ok(D2C::Ctl {
                    req_id: rid,
                    body,
                }) if rid == req_id => return Ok(body),
                Ok(_) => {}
                Err(e) => {
                    return Err(CliErr::new(
                        "no_daemon",
                        format!("connection lost waiting for the reply: {e}"),
                    ))
                }
            }
        }
    }

    /// For waits: socket timeout = the request's own timeout + slack, so a
    /// dead daemon errors out instead of hanging forever.
    fn call_deadline(&mut self, req: CtlRequest, total: Duration) -> Result<CtlBody, CliErr> {
        self.read
            .set_read_timeout(Some(total + Duration::from_secs(5)))
            .ok();
        let r = self.call(req);
        self.read.set_read_timeout(Some(IO_TIMEOUT)).ok();
        r
    }

    /// Legacy liveness check (Ping is allowed for every scope).
    fn ping(&mut self) -> Result<(), CliErr> {
        write_frame(&mut self.write, &C2D::Ping)
            .map_err(|e| CliErr::new("no_daemon", format!("{e}")))?;
        loop {
            match read_frame::<_, D2C>(&mut self.read) {
                Ok(D2C::Pong) => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(CliErr::new("no_daemon", format!("{e}"))),
            }
        }
    }

    fn listing(&mut self) -> Result<(Vec<crate::state::Folder>, Vec<CtlTerm>), CliErr> {
        match self.call(CtlRequest::List)? {
            CtlBody::Listing { folders, terminals } => Ok((folders, terminals)),
            body => Err(body_err(body)),
        }
    }
}

/// A CtlBody that should have been a specific success shape: map Err bodies
/// to CliErr, anything else to internal.
fn body_err(body: CtlBody) -> CliErr {
    match body {
        CtlBody::Err { code, msg } => CliErr { code, msg },
        _ => CliErr::new("internal", "unexpected reply shape from the daemon"),
    }
}

// ─────────────────────────── target resolution ───────────────────────────

/// §9.2: UUID > exact name > unique case-insensitive prefix. Resolution is
/// CLI-side (the daemon stays id-only — one authority for identity).
pub fn resolve_in(cands: &[(Uuid, String)], arg: &str) -> Result<Uuid, CliErr> {
    if let Ok(id) = Uuid::parse_str(arg) {
        return Ok(id);
    }
    let exact: Vec<&(Uuid, String)> = cands.iter().filter(|(_, n)| n == arg).collect();
    if exact.len() == 1 {
        return Ok(exact[0].0);
    }
    if exact.len() > 1 {
        return Err(ambiguous(arg, &exact));
    }
    let low = arg.to_lowercase();
    let ci: Vec<&(Uuid, String)> = cands
        .iter()
        .filter(|(_, n)| n.to_lowercase() == low)
        .collect();
    if ci.len() == 1 {
        return Ok(ci[0].0);
    }
    if ci.len() > 1 {
        return Err(ambiguous(arg, &ci));
    }
    let pre: Vec<&(Uuid, String)> = cands
        .iter()
        .filter(|(_, n)| n.to_lowercase().starts_with(&low))
        .collect();
    match pre.len() {
        1 => Ok(pre[0].0),
        0 => Err(CliErr::new(
            "not_found",
            format!("no terminal matches {arg:?}"),
        )),
        _ => Err(ambiguous(arg, &pre)),
    }
}

fn ambiguous(arg: &str, cands: &[&(Uuid, String)]) -> CliErr {
    let names: Vec<String> = cands
        .iter()
        .map(|(id, n)| format!("{n} ({id})"))
        .collect();
    CliErr::new(
        "ambiguous",
        format!("{arg:?} matches several terminals: {}", names.join(", ")),
    )
}

fn resolve_term(conn: &mut CtlConn, arg: &str) -> Result<Uuid, CliErr> {
    if let Ok(id) = Uuid::parse_str(arg) {
        return Ok(id);
    }
    let (_, terms) = conn.listing()?;
    let cands: Vec<(Uuid, String)> = terms.into_iter().map(|t| (t.id, t.name)).collect();
    resolve_in(&cands, arg)
}

/// Folder-name resolution, the exact list/create `--folder` machinery
/// (exact case-insensitive name; a raw UUID passes through).
fn resolve_folder(conn: &mut CtlConn, arg: &str) -> Result<Uuid, CliErr> {
    if let Ok(id) = Uuid::parse_str(arg) {
        return Ok(id);
    }
    let (folders, _) = conn.listing()?;
    let low = arg.to_lowercase();
    folders
        .iter()
        .find(|f| f.name.to_lowercase() == low)
        .map(|f| f.id)
        .ok_or_else(|| CliErr::new("not_found", format!("no folder named {arg:?}")))
}

/// SLEEP: the daemon must speak proto 9 or the Sleep/Wake CtlRequest
/// variants fail its bincode decode and it drops the connection — a
/// confusing "connection lost" instead of this named error (the install
/// copy-race skew window).
fn require_sleep_proto(conn: &CtlConn) -> Result<(), CliErr> {
    if conn.info.proto < 9 {
        return Err(CliErr::new(
            "proto_skew",
            format!(
                "daemon protocol {} predates sleep/wake — restart it from this build",
                conn.info.proto
            ),
        ));
    }
    Ok(())
}

/// CLI-side recursion pre-check (the daemon enforces it authoritatively; this
/// just saves the round trip and prints the same error).
fn self_check(conn: &CtlConn, id: Uuid, force_self: bool) -> Result<(), CliErr> {
    if conn.self_session == Some(id) && !force_self {
        return Err(CliErr::new(
            "self_target",
            "refusing to target the terminal this controller runs inside (pass --force-self to override)",
        ));
    }
    Ok(())
}

// ─────────────────────────────── execution ───────────────────────────────

fn execute(cmd: Cmd) -> Result<(), CliErr> {
    match cmd {
        Cmd::Info => {
            let mut conn = connect()?;
            conn.ping()?;
            print_json(&serde_json::json!({
                "v": 1, "ok": true,
                "pid": conn.info.pid,
                "port": conn.info.port,
                "proto": conn.info.proto,
                "self": conn.self_session.map(|u| u.to_string()),
            }));
            Ok(())
        }
        Cmd::List { folder } => {
            let mut conn = connect()?;
            let (folders, terms) = conn.listing()?;
            let folder_id = match &folder {
                Some(name) => {
                    let low = name.to_lowercase();
                    Some(
                        folders
                            .iter()
                            .find(|f| f.name.to_lowercase() == low)
                            .map(|f| f.id)
                            .ok_or_else(|| {
                                CliErr::new("not_found", format!("no folder named {name:?}"))
                            })?,
                    )
                }
                None => None,
            };
            let self_id = conn.self_session;
            let terminals: Vec<serde_json::Value> = terms
                .iter()
                .filter(|t| folder_id.is_none() || t.folder == folder_id)
                .map(|t| term_json(t, self_id == Some(t.id)))
                .collect();
            let folders: Vec<serde_json::Value> = folders
                .iter()
                .map(|f| serde_json::json!({"id": f.id.to_string(), "name": f.name}))
                .collect();
            print_json(&serde_json::json!({
                "v": 1, "ok": true, "folders": folders, "terminals": terminals,
            }));
            Ok(())
        }
        Cmd::Create {
            name,
            folder,
            cwd,
            kind,
            program,
            args,
            claude_session,
        } => {
            let mut conn = connect()?;
            let folder_id = match &folder {
                Some(fname) => {
                    let (folders, _) = conn.listing()?;
                    let low = fname.to_lowercase();
                    Some(
                        folders
                            .iter()
                            .find(|f| f.name.to_lowercase() == low)
                            .map(|f| f.id)
                            .ok_or_else(|| {
                                CliErr::new("not_found", format!("no folder named {fname:?}"))
                            })?,
                    )
                }
                None => None,
            };
            let cwd = cwd
                .map(std::path::PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| std::path::PathBuf::from("C:\\"));
            let (kind, program, args) = match kind.as_str() {
                "claude" => (
                    TermKind::Claude {
                        session_id: claude_session.unwrap_or_else(Uuid::new_v4),
                        extra_args: args,
                    },
                    program.unwrap_or_else(|| "claude".into()),
                    Vec::new(),
                ),
                "custom" => {
                    let p = program
                        .ok_or_else(|| usage("--kind custom needs --program"))?;
                    (TermKind::Custom, p, args)
                }
                _ => (
                    TermKind::Shell,
                    program.unwrap_or_else(|| "powershell.exe".into()),
                    if args.is_empty() {
                        vec!["-NoLogo".into()]
                    } else {
                        args
                    },
                ),
            };
            let spec = NewTerminal {
                name,
                folder: folder_id,
                kind,
                program,
                args,
                cwd,
                already_launched: claude_session.is_some(),
                shell_cfg: None,
            };
            match conn.call(CtlRequest::CreateTerminal { spec })? {
                CtlBody::Created { id } => {
                    print_json(&serde_json::json!({"v":1,"ok":true,"id":id.to_string()}));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::Run {
            term,
            cmd,
            force,
            force_self,
            multi,
            no_wait,
            timeout_secs,
            tail,
        } => {
            let mut conn = connect()?;
            let id = resolve_term(&mut conn, &term)?;
            self_check(&conn, id, force_self)?;
            let cmd = if multi {
                cmd.replace("\r\n", "\r").replace('\n', "\r")
            } else {
                cmd
            };
            let wait = (!no_wait).then_some(RunWait {
                timeout_ms: timeout_secs * 1000,
                tail_bytes: tail,
            });
            let req = CtlRequest::Run {
                id,
                cmd,
                force,
                force_self,
                wait,
            };
            let body = if no_wait {
                conn.call(req)?
            } else {
                conn.call_deadline(req, Duration::from_secs(timeout_secs))?
            };
            match body {
                CtlBody::RunStarted { at_off } => {
                    print_json(&serde_json::json!({
                        "v":1,"ok":true,"started":true,"at_off":at_off,
                    }));
                    Ok(())
                }
                CtlBody::RunDone {
                    exit,
                    duration_ms,
                    output,
                    truncated,
                    start_off,
                } => {
                    print_json(&serde_json::json!({
                        "v":1,"ok":true,"exit":exit,"duration_ms":duration_ms,
                        "start_off":start_off,"output":output,"truncated":truncated,
                    }));
                    Ok(())
                }
                CtlBody::Err { code, msg } if code == "busy" => {
                    // Ergonomic enrichment: name the open command as a field
                    // (agents branch on it without parsing the message).
                    let open_cmd = conn.listing().ok().and_then(|(_, ts)| {
                        ts.into_iter()
                            .find(|t| t.id == id)
                            .and_then(|t| t.open_block.map(|o| o.cmd))
                    });
                    print_json(&serde_json::json!({
                        "v":1,"ok":false,"code":"busy","msg":msg,"open_cmd":open_cmd,
                    }));
                    std::process::exit(2);
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::Send {
            term,
            payload,
            force_self,
        } => {
            let mut conn = connect()?;
            let id = resolve_term(&mut conn, &term)?;
            self_check(&conn, id, force_self)?;
            let reqs: Vec<CtlRequest> = match payload {
                SendPayload::Text { text, enter } => {
                    let mut v = vec![CtlRequest::SendRaw {
                        id,
                        bytes: text.into_bytes(),
                        force_self,
                    }];
                    if enter {
                        v.push(CtlRequest::SendChord {
                            id,
                            chord: CtlChord::Enter,
                            force_self,
                        });
                    }
                    v
                }
                SendPayload::B64(bytes) => vec![CtlRequest::SendRaw {
                    id,
                    bytes,
                    force_self,
                }],
                SendPayload::Key(chord) => vec![CtlRequest::SendChord {
                    id,
                    chord,
                    force_self,
                }],
            };
            for req in reqs {
                match conn.call(req)? {
                    CtlBody::Done => {}
                    body => return Err(body_err(body)),
                }
            }
            print_json(&serde_json::json!({"v":1,"ok":true}));
            Ok(())
        }
        Cmd::Read { term, tail, lines } => {
            let mut conn = connect()?;
            let id = resolve_term(&mut conn, &term)?;
            if tail {
                match conn.call(CtlRequest::ReadTail { id, lines })? {
                    CtlBody::Tail { lines, truncated } => {
                        print_json(&serde_json::json!({
                            "v":1,"ok":true,"lines":lines,"truncated":truncated,
                        }));
                        Ok(())
                    }
                    body => Err(body_err(body)),
                }
            } else {
                match conn.call(CtlRequest::ReadScreen { id })? {
                    CtlBody::Screen {
                        lines,
                        cursor_row,
                        cursor_col,
                        alt_screen,
                    } => {
                        print_json(&serde_json::json!({
                            "v":1,"ok":true,"alt_screen":alt_screen,
                            "cursor":{"row":cursor_row,"col":cursor_col},
                            "lines":lines,
                        }));
                        Ok(())
                    }
                    body => Err(body_err(body)),
                }
            }
        }
        Cmd::Blocks { term, last } => {
            let mut conn = connect()?;
            let id = resolve_term(&mut conn, &term)?;
            match conn.call(CtlRequest::ReadBlocks { id, last })? {
                CtlBody::Blocks { recs } => {
                    let blocks: Vec<serde_json::Value> = recs.iter().map(block_json).collect();
                    print_json(&serde_json::json!({"v":1,"ok":true,"blocks":blocks}));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::BlockText { term, start_off } => {
            let mut conn = connect()?;
            let id = resolve_term(&mut conn, &term)?;
            match conn.call(CtlRequest::ReadBlockText { id, start_off })? {
                CtlBody::BlockText { text, truncated } => {
                    print_json(&serde_json::json!({
                        "v":1,"ok":true,"text":text,"truncated":truncated,
                    }));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::Wait {
            term,
            spec,
            timeout_secs,
        } => {
            let mut conn = connect()?;
            let id = resolve_term(&mut conn, &term)?;
            let cond = match spec {
                WaitSpec::BlockClose { after } => WaitCond::BlockClose {
                    after_off: after.unwrap_or(0),
                },
                WaitSpec::Prompt => WaitCond::Prompt,
                WaitSpec::Exit => WaitCond::Exit,
                WaitSpec::Output {
                    pattern,
                    regex,
                    from,
                } => WaitCond::OutputMatch {
                    pattern,
                    regex,
                    from_off: from,
                },
            };
            let body = conn.call_deadline(
                CtlRequest::Wait {
                    id,
                    cond,
                    timeout_ms: timeout_secs * 1000,
                },
                Duration::from_secs(timeout_secs),
            )?;
            match body {
                CtlBody::Waited { hit } => {
                    let v = match hit {
                        WaitHit::BlockClosed { rec } => serde_json::json!({
                            "v":1,"ok":true,"hit":"block_close",
                            "cmd":rec.cmd,"exit":rec.exit,
                            "duration_ms":rec.ended_ms.unwrap_or(rec.started_ms)
                                .saturating_sub(rec.started_ms),
                            "start_off":rec.start_off,
                        }),
                        WaitHit::Prompt => serde_json::json!({"v":1,"ok":true,"hit":"prompt"}),
                        WaitHit::Exited { code } => serde_json::json!({
                            "v":1,"ok":true,"hit":"exit","code":code,
                        }),
                        WaitHit::Output { line, at_off } => serde_json::json!({
                            "v":1,"ok":true,"hit":"output","line":line,"at_off":at_off,
                        }),
                    };
                    print_json(&v);
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::Kill { term, force_self } => {
            let mut conn = connect()?;
            let id = resolve_term(&mut conn, &term)?;
            self_check(&conn, id, force_self)?;
            match conn.call(CtlRequest::Kill { id, force_self })? {
                CtlBody::Done => {
                    print_json(&serde_json::json!({"v":1,"ok":true}));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::Restart { term, force_self } => {
            let mut conn = connect()?;
            let id = resolve_term(&mut conn, &term)?;
            self_check(&conn, id, force_self)?;
            match conn.call(CtlRequest::Restart { id, force_self })? {
                CtlBody::Done => {
                    print_json(&serde_json::json!({"v":1,"ok":true}));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::Sleep {
            term,
            folder,
            force,
            force_self,
        } => {
            let mut conn = connect()?;
            require_sleep_proto(&conn)?;
            let req = match (&term, &folder) {
                (Some(t), None) => {
                    let id = resolve_term(&mut conn, t)?;
                    self_check(&conn, id, force_self)?;
                    CtlRequest::Sleep {
                        id,
                        force,
                        force_self,
                    }
                }
                (None, Some(f)) => CtlRequest::SleepFolder {
                    folder: resolve_folder(&mut conn, f)?,
                    force,
                },
                _ => unreachable!("parser enforces exactly one target"),
            };
            match conn.call(req)? {
                CtlBody::Done => {
                    print_json(&serde_json::json!({"v":1,"ok":true}));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::Wake { term, folder } => {
            let mut conn = connect()?;
            require_sleep_proto(&conn)?;
            let req = match (&term, &folder) {
                (Some(t), None) => CtlRequest::Wake {
                    id: resolve_term(&mut conn, t)?,
                },
                (None, Some(f)) => CtlRequest::WakeFolder {
                    folder: resolve_folder(&mut conn, f)?,
                },
                _ => unreachable!("parser enforces exactly one target"),
            };
            match conn.call(req)? {
                CtlBody::Done => {
                    print_json(&serde_json::json!({"v":1,"ok":true}));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::Delete {
            term,
            yes,
            force_self,
        } => {
            if !yes {
                return Err(CliErr::new(
                    "confirm",
                    "delete removes the terminal AND its journal irrecoverably; pass --yes",
                ));
            }
            let mut conn = connect()?;
            let id = resolve_term(&mut conn, &term)?;
            self_check(&conn, id, force_self)?;
            match conn.call(CtlRequest::Delete { id, force_self })? {
                CtlBody::Done => {
                    print_json(&serde_json::json!({"v":1,"ok":true}));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::Watch { ids, kinds } => {
            let mut conn = connect()?;
            let resolved = if ids.is_empty() {
                None
            } else {
                let mut v = Vec::with_capacity(ids.len());
                for arg in &ids {
                    v.push(resolve_term(&mut conn, arg)?);
                }
                Some(v)
            };
            match conn.call(CtlRequest::Subscribe {
                ids: resolved,
                kinds,
            })? {
                CtlBody::Subscribed => {}
                body => return Err(body_err(body)),
            }
            // The one long-lived mode: stream events until killed (or the
            // daemon goes away, which is an exit-1 transport error).
            conn.read.set_read_timeout(None).ok();
            loop {
                match read_frame::<_, D2C>(&mut conn.read) {
                    Ok(D2C::Ctl {
                        body: CtlBody::Event { ev },
                        ..
                    }) => {
                        print_json(&event_json(&ev));
                    }
                    Ok(_) => {}
                    Err(_) => {
                        return Err(CliErr::new("no_daemon", "event stream closed"));
                    }
                }
            }
        }
        Cmd::TokenCreate { name, scope } => {
            let mut conn = connect()?;
            match conn.call(CtlRequest::TokenCreate { name, scope })? {
                CtlBody::Token { name, token, scope } => {
                    print_json(&serde_json::json!({
                        "v":1,"ok":true,"name":name,"token":token,"scope":scope,
                    }));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::TokenRevoke { name } => {
            let mut conn = connect()?;
            match conn.call(CtlRequest::TokenRevoke { name })? {
                CtlBody::Done => {
                    print_json(&serde_json::json!({"v":1,"ok":true}));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
        Cmd::TokenList => {
            let mut conn = connect()?;
            match conn.call(CtlRequest::TokenList)? {
                CtlBody::Tokens { list } => {
                    let tokens: Vec<serde_json::Value> = list
                        .iter()
                        .map(|t| {
                            serde_json::json!({
                                "name": t.name, "token": t.token,
                                "scope": t.scope, "created_ms": t.created_ms,
                            })
                        })
                        .collect();
                    print_json(&serde_json::json!({"v":1,"ok":true,"tokens":tokens}));
                    Ok(())
                }
                body => Err(body_err(body)),
            }
        }
    }
}

// ────────────────────────────── JSON shapes ──────────────────────────────
//
// Dedicated builders, not blind serde of internals: the v:1 contract must
// not silently change when internal types grow.

pub fn term_json(t: &CtlTerm, is_self: bool) -> serde_json::Value {
    serde_json::json!({
        "id": t.id.to_string(),
        "name": t.name,
        "folder": t.folder.map(|f| f.to_string()),
        "kind": t.kind,
        "claude_session": t.claude_session.map(|u| u.to_string()),
        "inner_cli": t.inner_cli.as_ref().map(|c| serde_json::json!({
            "adapter": c.adapter,
            "resume_token": c.resume_token,
            "cwd": c.cwd.display().to_string(),
        })),
        "program": t.program,
        "cwd": t.cwd,
        "status": t.status,
        "activity": t.activity,
        "idle_ms": t.idle_ms,
        "cols": t.cols,
        "rows": t.rows,
        "hooked": t.hooked,
        "self": is_self,
        "open_block": t.open_block.as_ref().map(|o| serde_json::json!({
            "cmd": o.cmd, "started_ms": o.started_ms,
        })),
        "last_block": t.last_block.as_ref().map(|l| serde_json::json!({
            "cmd": l.cmd, "exit": l.exit, "ended_ms": l.ended_ms,
        })),
    })
}

pub fn block_json(r: &crate::state::BlockRec) -> serde_json::Value {
    serde_json::json!({
        "cmd": r.cmd,
        "exit": r.exit,
        "cwd": r.cwd.as_ref().map(|p| p.display().to_string()),
        "started_ms": r.started_ms,
        "ended_ms": r.ended_ms,
        "start_off": r.start_off,
        "end_off": r.end_off,
        "epoch": r.epoch,
        "truncated": r.truncated,
        "open": r.end_off.is_none(),
    })
}

pub fn event_json(ev: &CtlEvent) -> serde_json::Value {
    match ev {
        CtlEvent::BlockOpened { id, rec } => serde_json::json!({
            "v":1,"event":"block_opened","id":id.to_string(),
            "cmd":rec.cmd,"start_off":rec.start_off,
        }),
        CtlEvent::BlockClosed { id, rec } => serde_json::json!({
            "v":1,"event":"block_closed","id":id.to_string(),
            "cmd":rec.cmd,"exit":rec.exit,
            "duration_ms": rec.ended_ms.unwrap_or(rec.started_ms)
                .saturating_sub(rec.started_ms),
            "start_off":rec.start_off,
        }),
        CtlEvent::Exited { id, code } => serde_json::json!({
            "v":1,"event":"exited","id":id.to_string(),"code":code,
        }),
        CtlEvent::StateChanged => serde_json::json!({"v":1,"event":"state_changed"}),
    }
}

/// High-QoS opt-in (the procinfo::set_high_qos pattern, duplicated because
/// tc.exe's dependency closure deliberately excludes the daemon modules).
fn set_high_qos_self() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_STATE,
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    unsafe {
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const PROCESS_POWER_THROTTLING_STATE as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// §15 ctl_arg_parser: the verb table — run flags, --key chords, --for
    /// grammar, --yes requirement, unknown flag ⇒ usage.
    #[test]
    fn ctl_arg_parser() {
        assert_eq!(
            parse_args(&s(&["list"])).unwrap(),
            Cmd::List { folder: None }
        );
        assert_eq!(
            parse_args(&s(&["list", "--folder", "Work"])).unwrap(),
            Cmd::List {
                folder: Some("Work".into())
            }
        );
        // run: tc flags are LEADING-only — once the command starts, every
        // token (flag-looking or not) is the command, verbatim. Anything
        // else silently rewrites agent-composed commands.
        match parse_args(&s(&["run", "build", "cargo", "test", "-q", "--timeout", "300"])).unwrap()
        {
            Cmd::Run {
                term,
                cmd,
                force,
                no_wait,
                timeout_secs,
                tail,
                ..
            } => {
                assert_eq!(term, "build");
                assert_eq!(
                    cmd, "cargo test -q --timeout 300",
                    "--timeout after the command start belongs to the command"
                );
                assert!(!force && !no_wait);
                assert_eq!(timeout_secs, DEFAULT_RUN_TIMEOUT_SECS);
                assert_eq!(tail, DEFAULT_RUN_TAIL);
            }
            other => panic!("{other:?}"),
        }
        // Leading tc flags: before the term ref, or between it and the
        // command's first word.
        match parse_args(&s(&["run", "--no-wait", "b", "--timeout", "300", "dir"])).unwrap() {
            Cmd::Run {
                term,
                cmd,
                no_wait,
                timeout_secs,
                ..
            } => {
                assert_eq!(term, "b");
                assert_eq!(cmd, "dir");
                assert!(no_wait);
                assert_eq!(timeout_secs, 300);
            }
            other => panic!("{other:?}"),
        }
        // THE MEDIUM-5 scenario: `--force` inside the command must never arm
        // tc's own busy-gate bypass, and the command must not be rewritten.
        match parse_args(&s(&["run", "t", "git", "clean", "--force"])).unwrap() {
            Cmd::Run { cmd, force, .. } => {
                assert_eq!(cmd, "git clean --force");
                assert!(!force, "--force belongs to git, not tc");
            }
            other => panic!("{other:?}"),
        }
        // `--` ends tc's flags: required for a command whose first word
        // starts with `--`.
        match parse_args(&s(&["run", "t", "--force", "--", "--version"])).unwrap() {
            Cmd::Run { cmd, force, .. } => {
                assert_eq!(cmd, "--version");
                assert!(force, "leading --force before -- is tc's");
            }
            other => panic!("{other:?}"),
        }
        // A flag-looking token where the term ref belongs is a usage error
        // (almost certainly a typo'd tc flag).
        assert!(parse_args(&s(&["run", "--bogus", "t", "dir"])).is_err());
        assert!(parse_args(&s(&["run", "b"])).is_err(), "command required");
        assert!(
            parse_args(&s(&["run", "b", "--"])).is_err(),
            "-- with nothing after it is still a missing command"
        );
        // send: chords + exclusivity + b64.
        assert_eq!(
            parse_args(&s(&["send", "t", "--key", "ctrl+c"])).unwrap(),
            Cmd::Send {
                term: "t".into(),
                payload: SendPayload::Key(CtlChord::CtrlC),
                force_self: false,
            }
        );
        assert!(parse_args(&s(&["send", "t", "--key", "ctrl+q"])).is_err());
        assert_eq!(
            parse_args(&s(&["send", "t", "--text", "hi", "--enter"])).unwrap(),
            Cmd::Send {
                term: "t".into(),
                payload: SendPayload::Text {
                    text: "hi".into(),
                    enter: true
                },
                force_self: false,
            }
        );
        assert_eq!(
            parse_args(&s(&["send", "t", "--b64", "aGk="])).unwrap(),
            Cmd::Send {
                term: "t".into(),
                payload: SendPayload::B64(b"hi".to_vec()),
                force_self: false,
            }
        );
        // wait --for grammar.
        assert_eq!(
            parse_args(&s(&["wait", "t", "--for", "prompt"])).unwrap(),
            Cmd::Wait {
                term: "t".into(),
                spec: WaitSpec::Prompt,
                timeout_secs: DEFAULT_WAIT_TIMEOUT_SECS,
            }
        );
        assert_eq!(
            parse_args(&s(&[
                "wait", "t", "--for", "output", "Compiling", "--regex", "--from", "42",
                "--timeout", "9",
            ]))
            .unwrap(),
            Cmd::Wait {
                term: "t".into(),
                spec: WaitSpec::Output {
                    pattern: "Compiling".into(),
                    regex: true,
                    from: Some(42),
                },
                timeout_secs: 9,
            }
        );
        assert_eq!(
            parse_args(&s(&["wait", "t", "--for", "block-close", "--after", "7"])).unwrap(),
            Cmd::Wait {
                term: "t".into(),
                spec: WaitSpec::BlockClose { after: Some(7) },
                timeout_secs: DEFAULT_WAIT_TIMEOUT_SECS,
            }
        );
        assert!(parse_args(&s(&["wait", "t", "--for", "nonsense"])).is_err());
        // delete without --yes parses (the confirm refusal happens at
        // execute time so it reaches the JSON envelope).
        assert_eq!(
            parse_args(&s(&["delete", "t"])).unwrap(),
            Cmd::Delete {
                term: "t".into(),
                yes: false,
                force_self: false,
            }
        );
        // SLEEP §11: sleep/wake grammar — term xor --folder, leading-only
        // flags, flag-looking term ref ⇒ usage.
        assert_eq!(
            parse_args(&s(&["sleep", "api"])).unwrap(),
            Cmd::Sleep {
                term: Some("api".into()),
                folder: None,
                force: false,
                force_self: false,
            }
        );
        assert_eq!(
            parse_args(&s(&["sleep", "api", "--force", "--force-self"])).unwrap(),
            Cmd::Sleep {
                term: Some("api".into()),
                folder: None,
                force: true,
                force_self: true,
            }
        );
        assert_eq!(
            parse_args(&s(&["sleep", "--folder", "Work", "--force"])).unwrap(),
            Cmd::Sleep {
                term: None,
                folder: Some("Work".into()),
                force: true,
                force_self: false,
            }
        );
        assert_eq!(
            parse_args(&s(&["wake", "api"])).unwrap(),
            Cmd::Wake {
                term: Some("api".into()),
                folder: None,
            }
        );
        assert_eq!(
            parse_args(&s(&["wake", "--folder", "Work"])).unwrap(),
            Cmd::Wake {
                term: None,
                folder: Some("Work".into()),
            }
        );
        // Exactly one target; a flag-looking term ref is a typo'd flag.
        assert!(parse_args(&s(&["sleep"])).is_err());
        assert!(parse_args(&s(&["sleep", "api", "--folder", "Work"])).is_err());
        assert!(parse_args(&s(&["sleep", "--bogus", "api"])).is_err());
        assert!(parse_args(&s(&["wake"])).is_err());
        assert!(parse_args(&s(&["wake", "api", "--force"])).is_err(), "--force is sleep-only");
        assert!(parse_args(&s(&["wake", "a", "b"])).is_err());
        // watch events filter.
        assert_eq!(
            parse_args(&s(&["watch", "--events", "blocks,exit"])).unwrap(),
            Cmd::Watch {
                ids: vec![],
                kinds: EV_BLOCKS | EV_EXIT,
            }
        );
        assert!(parse_args(&s(&["watch", "--events", "bogus"])).is_err());
        // token grammar.
        assert_eq!(
            parse_args(&s(&["token", "create", "--name", "agents", "--scope", "input"])).unwrap(),
            Cmd::TokenCreate {
                name: "agents".into(),
                scope: SCOPE_READ | SCOPE_INPUT,
            }
        );
        assert!(parse_args(&s(&["token", "create", "--name", "x"])).is_err());
        // unknown verb/flag ⇒ usage.
        assert!(parse_args(&s(&["frobnicate"])).is_err());
        assert!(parse_args(&s(&["watch", "--bogus"])).is_err());
        assert!(parse_args(&[]).is_err());
    }

    /// §15 resolve_term_rules: uuid > exact > unique ci prefix; ambiguity
    /// carries candidates.
    #[test]
    fn resolve_term_rules() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let cands = vec![
            (a, "build shell".to_string()),
            (b, "Build Server".to_string()),
            (c, "api".to_string()),
        ];
        // Raw uuid wins without a lookup.
        let z = Uuid::new_v4();
        assert_eq!(resolve_in(&cands, &z.to_string()).unwrap(), z);
        // Exact (case-sensitive) name.
        assert_eq!(resolve_in(&cands, "build shell").unwrap(), a);
        // Case-insensitive exact.
        assert_eq!(resolve_in(&cands, "BUILD SERVER").unwrap(), b);
        // Unique case-insensitive prefix.
        assert_eq!(resolve_in(&cands, "ap").unwrap(), c);
        // Ambiguous prefix names candidates.
        let e = resolve_in(&cands, "build").unwrap_err();
        assert_eq!(e.code, "ambiguous");
        assert!(e.msg.contains("build shell") && e.msg.contains("Build Server"));
        // Miss.
        assert_eq!(resolve_in(&cands, "zzz").unwrap_err().code, "not_found");
        assert_eq!(exit_for("ambiguous"), 4);
        assert_eq!(exit_for("not_found"), 4);
        assert_eq!(exit_for("busy"), 2);
        assert_eq!(exit_for("timeout"), 3);
        assert_eq!(exit_for("no_daemon"), 1);
    }

    /// §15 json_shapes: the frozen v:1 key sets.
    #[test]
    fn json_shapes() {
        let t = CtlTerm {
            id: Uuid::nil(),
            name: "api server".into(),
            folder: None,
            kind: "shell".into(),
            claude_session: None,
            inner_cli: None,
            program: "powershell.exe".into(),
            cwd: "C:\\repo".into(),
            status: "running".into(),
            activity: "idle".into(),
            idle_ms: Some(184223),
            cols: 142,
            rows: 38,
            hooked: true,
            open_block: None,
            last_block: Some(crate::protocol::CtlLastBlock {
                cmd: "cargo test".into(),
                exit: Some(0),
                ended_ms: Some(1),
            }),
        };
        let v = term_json(&t, false);
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "activity",
                "claude_session",
                "cols",
                "cwd",
                "folder",
                "hooked",
                "id",
                "idle_ms",
                "inner_cli",
                "kind",
                "last_block",
                "name",
                "open_block",
                "program",
                "rows",
                "self",
                "status",
            ]
        );
        let rec = crate::state::BlockRec {
            epoch: 1,
            n: 2,
            cmd: "cargo test".into(),
            cwd: None,
            exit: Some(0),
            started_ms: 100,
            ended_ms: Some(8223),
            start_off: 48211,
            end_off: Some(50000),
            truncated: false,
        };
        let ev = event_json(&CtlEvent::BlockClosed {
            id: Uuid::nil(),
            rec: rec.clone(),
        });
        let keys: Vec<&str> = ev.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["cmd", "duration_ms", "event", "exit", "id", "start_off", "v"]
        );
        assert_eq!(ev["duration_ms"], 8123);
        let b = block_json(&rec);
        assert_eq!(b["open"], false);
        assert_eq!(b["exit"], 0);
        assert_eq!(b["start_off"], 48211);
    }

    #[test]
    fn b64_decoder_basics() {
        assert_eq!(b64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(b64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64_decode("YQ==").unwrap(), b"a");
        assert_eq!(b64_decode("AAECAw==").unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(b64_decode("").unwrap(), Vec::<u8>::new());
        assert!(b64_decode("!!bad!!").is_none());
    }

    #[test]
    fn chord_names_cover_the_documented_set() {
        for name in [
            "enter",
            "esc",
            "tab",
            "backspace",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pgup",
            "pgdn",
            "ctrl+c",
            "ctrl+d",
            "ctrl+z",
            "ctrl+l",
        ] {
            assert!(parse_chord(name).is_some(), "{name} must parse");
        }
        assert!(parse_chord("ctrl+alt+del").is_none());
        assert_eq!(parse_chord("CTRL+C"), Some(CtlChord::CtrlC));
    }
}
