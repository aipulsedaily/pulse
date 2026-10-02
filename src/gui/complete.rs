//! Tab completion for the composer draft (task #24): a NATIVE, family-aware
//! path completer — zero PTY round-trips, typing latency untouched.
//!
//! The composer's TextEdit runs `lock_focus(true)` (egui's event-filter tab
//! bit), so an unconsumed Tab reaches the widget's multiline arm and inserts
//! a literal `\t` — the user-reported "3 spacing" bug. composer::show now
//! consumes EVERY Tab press before the TextEdit shows (the P3/P4
//! consume-before-show pattern) and routes it here.
//!
//! Model (PSReadLine-style inline replacement, never a popup):
//!  • Tab completes the token under/before the caret against the LOCAL
//!    filesystem, resolved from the terminal's tracked cwd; repeat Tab
//!    cycles forward, Shift+Tab reverse, Esc restores the original token.
//!  • Candidates = one `read_dir` per cycle start (cached for the cycle;
//!    a draft edit invalidates lazily). Dirs first, then files, sorted
//!    case-insensitively; dirs complete with a trailing separator.
//!  • Families: Pwsh/Cmd/Other = Windows namespace, case-INSENSITIVE prefix
//!    match; WslShell = posix tokens mapped to `/mnt/<drive>` or
//!    `\\wsl.localhost\<distro>` for enumeration (rendered back as posix),
//!    case-SENSITIVE; Remote = a POSIX world no local call can see.
//!  • remote-completion: `Remote` is the lane that used to be a silent
//!    no-op. A directory on another machine cannot be enumerated by any
//!    local syscall, so its listing comes from the hooked shell STANDING in
//!    it, over the OSC 7717 hook channel the daemon already reads
//!    (`daemon::completion`). This module stays synchronous and pure: a
//!    `Dir::Remote` plan is answered from a cache the caller owns, and a
//!    cache miss returns `Start::Request` — "ask for this directory" — so
//!    the Tab no-ops exactly as it did before while the answer is fetched.
//!    The LOCAL lanes are untouched: a Windows or WSL plan is still one
//!    inline `read_dir`, at the same latency, through the same code.
//!  • Which world a terminal is IN is not the world it was SPAWNED in:
//!    typing `ssh host` at a pwsh prompt (or `sudo su` inside that) puts a
//!    POSIX shell in front of the user while `ShellFamily` still says Pwsh.
//!    `effective_family` reads that off the tracked cwd — the hooked remote
//!    shell reports its own `$PWD`, and a POSIX-absolute cwd under a Windows
//!    family can only mean a POSIX world is live inside it.
//!  • Quoting reuses drop.rs: PS single-quote, cmd conditional `"…"`, bash
//!    single-quote — applied to the WHOLE token only when it needs it; the
//!    tokenizer unquotes on the way in, so cycling a quoted token round-trips.
//!  • Hidden files: posix dotfiles are filtered unless the typed prefix
//!    starts with `.`; the Windows hidden ATTRIBUTE is deliberately ignored
//!    (names starting with `.` are ordinary on Windows — PSReadLine parity).
//!  • Budget: enumeration is synchronous only for local dirs; UNC targets
//!    (`\\wsl.localhost\…`) run on a spawned thread with a 150ms budget —
//!    timeout ⇒ this Tab silently no-ops (never block typing). Dirs beyond
//!    ENUM_CAP entries complete the common prefix only, no cycle (honest).
//!
//! v1 scope notes: command-NAME completion is out; `~` completes only in the
//! Windows namespace (Pwsh/Other render it back as typed, cmd — which has no
//! `~` — renders the expanded home); a WSL `~` token no-ops (the distro
//! user's home isn't knowable from here); bare `~` without a separator and
//! drive-relative `C:foo` tokens no-op (a bare `C:` completes the drive ROOT
//! — never drive-relative, the "C:" ≠ "C:\" trap).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::state::ShellFamily;

/// One `read_dir` may report at most this many entries before the completer
/// stops offering a cycle and degrades to common-prefix-only (honest: a 500+
/// item cycle is noise, and the cap bounds the per-Tab work).
pub const ENUM_CAP: usize = 500;
/// Hard bound on RETAINED matches while streaming an over-cap dir — beyond
/// this the haystack is unbounded and the Tab no-ops entirely (a common
/// prefix computed over a subset could over-complete).
const MATCH_BOUND: usize = 2048;
/// UNC (`\\…`) enumeration budget; timeout ⇒ silent no-op for this Tab.
const UNC_BUDGET: Duration = Duration::from_millis(150);

/// The completion family — path namespace + quoting rules. Derived from
/// `ShellFamily` (never persisted); owns the distro so `ComposerState` can
/// hold it without borrows.
#[derive(Debug, Clone, PartialEq)]
pub enum Family {
    Pwsh,
    Cmd,
    /// `distro`: value after -d; None = the default distro (no name to build
    /// a `\\wsl.localhost` UNC with — only `/mnt/<drive>` paths complete).
    Wsl { distro: Option<String> },
    /// A POSIX world no local call can see: a Pulse-spawned ssh terminal, a
    /// typed `ssh host`, a `sudo su` on the far side of either. Posix
    /// tokenizing and bash quoting; enumeration comes from the hooked remote
    /// shell (`Dir::Remote`), and with no listing in hand Tab no-ops exactly
    /// as it always did.
    Remote,
    /// Hookless/custom shells that somehow gained a composer: Windows
    /// namespace, WT-style bare-or-`"…"` quoting.
    Other,
}

pub fn family_for(f: &ShellFamily) -> Family {
    match f {
        ShellFamily::Pwsh => Family::Pwsh,
        ShellFamily::Cmd => Family::Cmd,
        ShellFamily::WslShell { distro } => Family::Wsl {
            distro: distro.clone(),
        },
        ShellFamily::Ssh { .. } => Family::Remote,
        ShellFamily::Other => Family::Other,
    }
}

// ───────────────────────────── tokenizer ─────────────────────────────

/// A draft token: byte range (INCLUDING any quotes) + the unquoted value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tok {
    pub start: usize,
    pub end: usize,
    pub value: String,
}

fn quote_chars(fam: &Family) -> &'static [char] {
    match fam {
        // pwsh: both quote forms; `''` doubles inside single quotes.
        Family::Pwsh => &['\'', '"'],
        // cmd (and WT-style Other) know only `"…"`.
        Family::Cmd | Family::Other => &['"'],
        // bash: both forms; backslash escapes outside single quotes.
        Family::Wsl { .. } | Family::Remote => &['\'', '"'],
    }
}

fn bs_escapes(fam: &Family) -> bool {
    matches!(fam, Family::Wsl { .. } | Family::Remote)
}

/// Split the WHOLE draft on unquoted whitespace (family quote/escape rules;
/// an unterminated quote runs to the end — the common mid-edit state).
/// Byte ranges include any quotes; `value` is the unquoted text. Shared by
/// the caret lookup below and the prompt highlighter (highlight.rs) — one
/// lexer, one set of quote rules.
pub(crate) fn tokens(fam: &Family, s: &str) -> Vec<Tok> {
    let mut toks: Vec<Tok> = Vec::new();
    let mut it = s.char_indices().peekable();
    let mut cur: Option<(usize, String)> = None;
    let mut quote: Option<char> = None;
    while let Some((i, c)) = it.next() {
        if let Some(q) = quote {
            if c == q {
                if q == '\''
                    && matches!(fam, Family::Pwsh)
                    && it.peek().is_some_and(|&(_, n)| n == '\'')
                {
                    it.next();
                    if let Some((_, v)) = &mut cur {
                        v.push('\'');
                    }
                } else {
                    quote = None;
                }
            } else if q == '"'
                && c == '\\'
                && bs_escapes(fam)
                && it
                    .peek()
                    .is_some_and(|&(_, n)| matches!(n, '"' | '\\' | '$' | '`'))
            {
                let (_, n) = it.next().unwrap();
                if let Some((_, v)) = &mut cur {
                    v.push(n);
                }
            } else if let Some((_, v)) = &mut cur {
                v.push(c);
            }
            continue;
        }
        if c.is_whitespace() {
            if let Some((st, v)) = cur.take() {
                toks.push(Tok {
                    start: st,
                    end: i,
                    value: v,
                });
            }
            continue;
        }
        let (_, v) = cur.get_or_insert_with(|| (i, String::new()));
        if quote_chars(fam).contains(&c) {
            quote = Some(c);
        } else if c == '\\' && bs_escapes(fam) {
            // Escaped char (e.g. `a\ b`) joins the token literally.
            match it.next() {
                Some((_, n)) => v.push(n),
                None => v.push('\\'),
            }
        } else {
            v.push(c);
        }
    }
    if let Some((st, v)) = cur.take() {
        toks.push(Tok {
            start: st,
            end: s.len(),
            value: v,
        });
    }
    toks
}

/// The token CONTAINING the caret (start < caret ≤ end), else an empty
/// token AT the caret (readline semantics: `cd |` completes the cwd with an
/// empty stem). Everything outside the returned range is preserved
/// byte-exact by the caller.
pub(crate) fn token_at(fam: &Family, s: &str, caret: usize) -> Tok {
    tokens(fam, s)
        .into_iter()
        .find(|t| t.start < caret && caret <= t.end)
        .unwrap_or(Tok {
            start: caret,
            end: caret,
            value: String::new(),
        })
}

// ───────────────────────── path resolution ─────────────────────────

/// Where one plan's candidates come from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Dir {
    /// A path this process can `read_dir` (may be UNC — then budget-threaded).
    Local(PathBuf),
    /// A POSIX directory only the shell standing on that host can list
    /// (remote-completion). The string is exactly what the shell is asked
    /// for: absolute, `$PWD`-relative, or `~`-prefixed — the SHELL resolves
    /// all three, and it is also the cache key both sides agree on.
    Remote(String),
    /// WSL: the `\\wsl.localhost` view FIRST (local, no round trip), with the
    /// hook channel as the fallback. The two differ in exactly the cases
    /// that matter: a nested `sudo su` world's `/root` is readable by the
    /// distro's root shell and EACCES over the UNC share, and a
    /// default-distro terminal may have no UNC name to build at all.
    LocalThenRemote(PathBuf, String),
}

/// One directory as the remote shell reported it (remote-completion). Owned
/// rather than borrowed so the cache can live behind any lock the caller
/// likes; the entry count is bounded by `bootstrap::COMP_MAX_BYTES`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RemoteListing {
    pub entries: Vec<Entry>,
    /// The directory blew the payload cap and the listing was dropped: the
    /// honest answer is no candidates (the same verdict `MATCH_BOUND` reaches
    /// locally — a cycle over a subset would over-complete).
    pub trunc: bool,
}

/// The caller's cache of remote listings. A trait so this module keeps no
/// dependency on the IPC layer and the whole remote lane is testable with a
/// hand-built map.
pub trait RemoteDirs {
    /// `Some` = known (possibly an empty/truncated listing, which is still an
    /// answer); `None` = not known.
    fn listing(&self, dir: &str) -> Option<RemoteListing>;
    /// A DEFINITIVE "nothing" has already come back for this directory — the
    /// gate declined, the shell did not answer, the path does not exist.
    /// Without it an unanswerable directory would be asked for forever; with
    /// it the Tab degrades to the old silent no-op and stays there.
    ///
    /// An ask still IN FLIGHT is deliberately NOT declined: the plan keeps
    /// returning `Start::Request`, which is how `tab_retry` knows to keep
    /// waiting, and the caller (which owns the send) dedupes.
    fn declined(&self, dir: &str) -> bool;
}

/// Everything needed to enumerate + render candidates for one token.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    /// Where the candidates come from.
    pub dir: Dir,
    /// Name prefix candidates must start with.
    pub prefix: String,
    /// What precedes the name in the rendered (pre-quoting) token — the
    /// token's own parent spelling, byte-preserved where possible.
    render_parent: String,
    /// Separator appended to directory candidates (token style wins).
    sep: char,
    /// Case-insensitive prefix match (Windows namespace).
    pub ci: bool,
    /// Posix dotfile rule: hide `.`-names unless the prefix starts with `.`.
    pub posix_hidden: bool,
}

fn win_shaped(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Posix path → the local view a Windows process can enumerate:
/// `/mnt/<drive>/…` → `<drive>:\…`; anything else absolute needs a NAMED
/// distro → `\\wsl.localhost\<distro>\…`; relative/default-distro ⇒ None.
/// Shared with mod.rs `local_cwd_for` (QOL §3.3 — one mapping, one test bed).
pub(crate) fn posix_to_local(p: &str, distro: Option<&str>) -> Option<PathBuf> {
    let b = p.as_bytes();
    if p.starts_with("/mnt/")
        && b.get(5).is_some_and(|c| c.is_ascii_alphabetic())
        && (b.len() == 6 || b[6] == b'/')
    {
        let drive = (b[5] as char).to_ascii_uppercase();
        let rest = p.get(7..).unwrap_or("").replace('/', "\\");
        return Some(PathBuf::from(format!("{drive}:\\{rest}")));
    }
    if let Some(rest) = p.strip_prefix('/') {
        let d = distro?;
        return Some(PathBuf::from(format!(
            "\\\\wsl.localhost\\{d}\\{}",
            rest.replace('/', "\\")
        )));
    }
    None
}

pub(crate) fn plan(
    fam: &Family,
    cwd: Option<&str>,
    home: Option<&str>,
    value: &str,
) -> Option<Plan> {
    match fam {
        Family::Remote => plan_remote(cwd, value),
        Family::Wsl { distro } => plan_wsl(cwd, distro.as_deref(), value),
        _ => plan_win(fam, cwd, home, value),
    }
}

/// Canonical spelling of a directory the SHELL will be asked for: no trailing
/// separator (so `src/` and `src` are one cache key on both sides), except
/// for the root itself, which IS its separator.
fn canon_req(d: &str) -> String {
    let t = d.trim_end_matches('/');
    if t.is_empty() {
        "/".to_string()
    } else {
        t.to_string()
    }
}

/// remote-completion: a POSIX plan whose listing can only come from the
/// shell on the other machine.
///
/// It never resolves anything it cannot know. `~` is passed through to the
/// shell VERBATIM — the remote home is the shell's business, and the v1
/// WSL `~` no-op existed precisely because guessing it would complete
/// garbage. A relative token is anchored at the tracked cwd, which for a
/// remote world is the hooked shell's own reported `$PWD`; `..` is left in
/// the request rather than folded, so the SHELL (and `ls`) resolve symlinked
/// parents the way the user's own `cd ..` would.
fn plan_remote(cwd: Option<&str>, value: &str) -> Option<Plan> {
    let (parent, prefix) = match value.rfind('/') {
        Some(i) => (&value[..i + 1], &value[i + 1..]),
        None => ("", value),
    };
    let req = if parent.starts_with('~') || parent.starts_with('/') {
        parent.to_string()
    } else {
        let c = cwd.filter(|c| c.starts_with('/'))?;
        format!("{}/{parent}", c.trim_end_matches('/'))
    };
    Some(Plan {
        dir: Dir::Remote(canon_req(&req)),
        prefix: prefix.to_string(),
        render_parent: parent.to_string(),
        sep: '/',
        ci: false,
        posix_hidden: true,
    })
}

/// The world the terminal is IN right now, which is not always the world it
/// was SPAWNED in (remote-completion).
///
/// A typed `ssh host` — the field-reported case — leaves `ShellFamily` at
/// Pwsh/Cmd forever: it is derived from the persisted program+args and
/// nothing about typing a command changes that. What DOES change is the
/// tracked cwd: the remote shell Pulse hooked (its own ssh rcfile, or
/// `nesthook`'s injection into a shell it witnessed being opened) reports its
/// `$PWD`, and the daemon folds that into `live_cwd`. A POSIX-ABSOLUTE cwd
/// under a Windows family is therefore not ambiguous — a Windows shell's cwd
/// is always drive-shaped — and it can only mean a POSIX world is live in
/// front of the user. That is the signal, and it is a witness, not a guess.
///
/// Scoped to completion on purpose: the composer's highlighter and quoting
/// still follow the SPAWN family. Widening it is a separate change with its
/// own blast radius (paste quoting, the history ghost's path heuristics),
/// and completing the wrong filesystem is the bug in front of us.
pub fn effective_family(fam: &Family, cwd: Option<&str>) -> Family {
    let posix_cwd = cwd.is_some_and(|c| c.starts_with('/'));
    match fam {
        // WSL keeps its local UNC lane (and its own remote fallback).
        Family::Wsl { .. } | Family::Remote => fam.clone(),
        _ if posix_cwd => Family::Remote,
        _ => fam.clone(),
    }
}

/// Whether this terminal's Tab can need the REMOTE lane at all: a POSIX
/// world is live in front of the user (`effective_family`), or it is WSL —
/// whose local UNC leg can still fall THROUGH to the hook channel (a nested
/// root shell's `/root`, a default-distro terminal with no UNC name).
///
/// The app reads it to make sure the listing cache EXISTS before the first
/// Tab, and that is not a nicety: `enumerate` cannot tell a cache that is
/// merely EMPTY from "there is no remote lane here" (`remote: None` ⇒
/// `Nothing`), and the cache used to be created only BY an ask — so the ask
/// that would have created it could never be parked. That circle is the
/// v0.1.20 field bug: `cd pr<Tab>` inside a real ssh session did nothing,
/// forever, and silently.
pub fn remote_lane_possible(fam: &Family, cwd: Option<&str>) -> bool {
    matches!(
        effective_family(fam, cwd),
        Family::Remote | Family::Wsl { .. }
    )
}

fn plan_win(fam: &Family, cwd: Option<&str>, home: Option<&str>, value: &str) -> Option<Plan> {
    // Token separator style wins for rendered dirs; default Windows `\`.
    let sep = if value.contains('/') && !value.contains('\\') {
        '/'
    } else {
        '\\'
    };
    // Bare drive: complete the drive ROOT — never drive-relative (the
    // "C:" ≠ "C:\" trap: bare drives resolve against a per-process cwd).
    if value.len() == 2 && win_shaped(value) {
        return Some(Plan {
            dir: Dir::Local(PathBuf::from(format!("{value}\\"))),
            prefix: String::new(),
            render_parent: format!("{value}{sep}"),
            sep,
            ci: true,
            posix_hidden: false,
        });
    }
    let (parent, prefix) = match value.rfind(['/', '\\']) {
        Some(i) => (&value[..i + 1], &value[i + 1..]),
        None => ("", value),
    };
    let (fs_dir, render_parent) = if parent.is_empty() {
        // Relative bare name: the tracked cwd is the parent. A drive-relative
        // "C:foo" stem never matches real names and honestly no-ops.
        let c = cwd.filter(|c| win_shaped(c))?;
        (PathBuf::from(c), String::new())
    } else if parent.starts_with('~') && parent[1..].starts_with(['/', '\\']) {
        // ~ = USERPROFILE. Pwsh/Other resolve `~` natively — render it back
        // as typed; cmd has NO `~` so the completed token renders the
        // expanded home (a working path beats byte preservation there).
        let h = home?;
        let fs = PathBuf::from(format!("{h}{}", &parent[1..]));
        let render = if matches!(fam, Family::Cmd) {
            format!("{h}{}", &parent[1..])
        } else {
            parent.to_string()
        };
        (fs, render)
    } else if win_shaped(parent) || parent.starts_with("\\\\") {
        (PathBuf::from(parent), parent.to_string())
    } else if parent.starts_with(['/', '\\']) {
        // Root-relative: anchor at the tracked cwd's drive.
        let c = cwd.filter(|c| win_shaped(c))?;
        (PathBuf::from(format!("{}{parent}", &c[..2])), parent.to_string())
    } else {
        let c = cwd.filter(|c| win_shaped(c))?;
        (Path::new(c).join(parent), parent.to_string())
    };
    Some(Plan {
        dir: Dir::Local(fs_dir),
        prefix: prefix.to_string(),
        render_parent,
        sep,
        ci: true,
        posix_hidden: false,
    })
}

fn plan_wsl(cwd: Option<&str>, distro: Option<&str>, value: &str) -> Option<Plan> {
    if value.starts_with('~') {
        // The distro user's home isn't knowable from the GUI — but it IS
        // knowable to the shell, which is hooked. remote-completion routes
        // `~` straight through to it rather than guessing (the v1 no-op) or
        // completing a Windows `%USERPROFILE%` that does not exist in there.
        return plan_remote(cwd, value);
    }
    let (parent, prefix) = match value.rfind('/') {
        Some(i) => (&value[..i + 1], &value[i + 1..]),
        None => ("", value),
    };
    let posix_parent = if parent.starts_with('/') {
        parent.to_string()
    } else {
        let c = cwd?;
        let base = if win_shaped(c) {
            // Pre-first-cd terminals still carry the Windows-shaped spawn
            // cwd; the shell actually sits at its /mnt translation.
            super::drop::translate_wsl(c, distro)?
        } else if c.starts_with('/') {
            c.to_string()
        } else {
            return None;
        };
        format!("{}/{parent}", base.trim_end_matches('/'))
    };
    // The UNC/drive view when one exists (local, no round trip), with the
    // hook channel behind it; a default-distro terminal inside the distro fs
    // has no UNC name to build and goes straight to the shell.
    let req = canon_req(&posix_parent);
    let dir = match posix_to_local(&posix_parent, distro) {
        Some(fs) => Dir::LocalThenRemote(fs, req),
        None => Dir::Remote(req),
    };
    Some(Plan {
        dir,
        prefix: prefix.to_string(),
        render_parent: parent.to_string(),
        sep: '/',
        ci: false,
        posix_hidden: true,
    })
}

// ───────────────────────── enumeration ─────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub name: String,
    pub dir: bool,
}

struct EnumOut {
    matches: Vec<Entry>,
    capped: bool,
}

/// ONE streaming read_dir: retain prefix matches only, count the total.
/// `capped` = the dir exceeds `cap` entries (⇒ common-prefix-only mode).
fn enum_dir(dir: &Path, prefix: &str, ci: bool, posix_hidden: bool, cap: usize) -> Option<EnumOut> {
    let want_hidden = prefix.starts_with('.');
    let pfx_lc = if ci { Some(prefix.to_lowercase()) } else { None };
    let mut matches: Vec<Entry> = Vec::new();
    let mut total = 0usize;
    for ent in std::fs::read_dir(dir).ok()? {
        let Ok(ent) = ent else { continue };
        total += 1;
        // Non-Unicode names can't be rendered into the draft — skip.
        let Ok(name) = ent.file_name().into_string() else {
            continue;
        };
        if posix_hidden && name.starts_with('.') && !want_hidden {
            continue;
        }
        let hit = match &pfx_lc {
            Some(p) => name.to_lowercase().starts_with(p.as_str()),
            None => name.starts_with(prefix),
        };
        if !hit {
            continue;
        }
        let dir_flag = ent
            .file_type()
            .map(|t| {
                if t.is_symlink() {
                    // Follow the link for the dir/file split (rare — one
                    // extra stat per symlink only).
                    std::fs::metadata(ent.path()).map(|m| m.is_dir()).unwrap_or(false)
                } else {
                    t.is_dir()
                }
            })
            .unwrap_or(false);
        matches.push(Entry { name, dir: dir_flag });
        if matches.len() > MATCH_BOUND {
            return None; // unbounded haystack — silent no-op (honest)
        }
    }
    // Dirs first, then files; alphabetical, case-insensitive within groups.
    matches.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Some(EnumOut {
        matches,
        capped: total > cap,
    })
}

/// UNC targets (`\\wsl.localhost\…`, network shares) can stall for seconds —
/// enumerate them on a throwaway thread with a hard budget; local dirs run
/// inline (bounded by the cap machinery). Timeout ⇒ None (this Tab no-ops;
/// the orphan thread finishes into a dropped channel).
fn enum_budgeted(
    dir: PathBuf,
    prefix: String,
    ci: bool,
    posix_hidden: bool,
    cap: usize,
) -> Option<EnumOut> {
    if !dir.as_os_str().to_string_lossy().starts_with("\\\\") {
        return enum_dir(&dir, &prefix, ci, posix_hidden, cap);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(enum_dir(&dir, &prefix, ci, posix_hidden, cap));
    });
    rx.recv_timeout(UNC_BUDGET).ok().flatten()
}

/// remote-completion: the same filter/order over a listing the shell already
/// sent. No IO, no budget, no thread — the round trip happened before the
/// Tab, which is the whole point of the prefetch lane.
fn enum_remote(l: &RemoteListing, prefix: &str, posix_hidden: bool) -> EnumOut {
    if l.trunc {
        // The shell dropped the listing rather than send a subset.
        return EnumOut {
            matches: Vec::new(),
            capped: false,
        };
    }
    let want_hidden = prefix.starts_with('.');
    let mut matches: Vec<Entry> = l
        .entries
        .iter()
        .filter(|e| !(posix_hidden && e.name.starts_with('.') && !want_hidden))
        .filter(|e| e.name.starts_with(prefix))
        .cloned()
        .collect();
    matches.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    EnumOut {
        matches,
        capped: false,
    }
}

/// What enumerating a plan produced.
enum Enumerated {
    Found(EnumOut),
    /// remote-completion: only the shell can answer, and it has not been
    /// asked yet. The caller asks and this Tab no-ops — the honest degrade.
    Ask(String),
    /// Nothing, and nothing to ask: exactly today's silent no-op.
    Nothing,
}

/// Resolve a plan's target to candidates. The LOCAL legs are byte-for-byte
/// the pre-remote path (one `read_dir`, same budget, same caps); the remote
/// leg is a cache lookup; the WSL leg tries local first and only falls
/// through when the Windows-side view cannot answer at all (no such
/// directory, or EACCES — a nested root shell's `/root` over the UNC share).
fn enumerate(
    plan: &Plan,
    cap: usize,
    remote: Option<&dyn RemoteDirs>,
) -> Enumerated {
    let ask = |dir: &str| match remote {
        Some(r) => match r.listing(dir) {
            Some(l) => Enumerated::Found(enum_remote(&l, &plan.prefix, plan.posix_hidden)),
            None if r.declined(dir) => Enumerated::Nothing,
            None => Enumerated::Ask(dir.to_string()),
        },
        None => Enumerated::Nothing,
    };
    match &plan.dir {
        Dir::Local(fs) => match enum_budgeted(
            fs.clone(),
            plan.prefix.clone(),
            plan.ci,
            plan.posix_hidden,
            cap,
        ) {
            Some(out) => Enumerated::Found(out),
            None => Enumerated::Nothing,
        },
        Dir::Remote(dir) => ask(dir),
        Dir::LocalThenRemote(fs, dir) => match enum_budgeted(
            fs.clone(),
            plan.prefix.clone(),
            plan.ci,
            plan.posix_hidden,
            cap,
        ) {
            Some(out) => Enumerated::Found(out),
            None => ask(dir),
        },
    }
}

// ───────────────────────── rendering + quoting ─────────────────────────

/// Build the full replacement token for one candidate: parent spelling as
/// typed + name (+ trailing separator for dirs), quoted per family only
/// when the token needs it.
pub(crate) fn render_token(plan: &Plan, fam: &Family, name: &str, is_dir: bool) -> String {
    let mut t = String::with_capacity(plan.render_parent.len() + name.len() + 1);
    t.push_str(&plan.render_parent);
    t.push_str(name);
    if is_dir {
        t.push(plan.sep);
    }
    quote_token(fam, &t)
}

/// Family quoting (reuses drop.rs — the drag-drop goldens are the oracle):
/// PS single-quote, cmd conditional `"…"`, bash single-quote. Bare when the
/// token carries nothing special — completion should not uglify plain paths.
fn quote_token(fam: &Family, t: &str) -> String {
    match fam {
        Family::Pwsh => {
            const SPECIAL: &[char] = &[
                '\'', '"', '`', '$', '&', ';', ',', '(', ')', '{', '}', '[', ']', '|', '<',
                '>', '@', '#',
            ];
            if t.chars().any(char::is_whitespace) || t.contains(SPECIAL) {
                super::drop::pwsh_quote(t)
            } else {
                t.to_string()
            }
        }
        Family::Cmd => super::drop::cmd_quote(t),
        Family::Wsl { .. } | Family::Remote => {
            const SPECIAL: &[char] = &[
                '\'', '"', '`', '$', '&', '|', ';', '(', ')', '<', '>', '*', '?', '[', ']',
                '{', '}', '!', '#', '~', '\\',
            ];
            if t.chars().any(char::is_whitespace) || t.contains(SPECIAL) {
                super::drop::bash_single_quote(t)
            } else {
                t.to_string()
            }
        }
        Family::Other => super::drop::other_quote(t),
    }
}

/// Longest common prefix of the matched names (char-wise; case-insensitive
/// comparison on Windows takes the first match's casing).
fn common_prefix(matches: &[Entry], ci: bool) -> String {
    let first = &matches[0].name;
    let mut len = first.chars().count();
    for e in &matches[1..] {
        let mut l = 0usize;
        for (a, b) in first.chars().zip(e.name.chars()) {
            let same = if ci {
                a.to_lowercase().eq(b.to_lowercase())
            } else {
                a == b
            };
            if !same {
                break;
            }
            l += 1;
        }
        len = len.min(l);
        if len == 0 {
            break;
        }
    }
    first.chars().take(len).collect()
}

// ───────────────────────── cycle state machine ─────────────────────────

/// A live completion cycle: the draft is `head + candidate + tail`; the
/// caller validates `expect` against the real draft before every step —
/// ANY other edit commits the current candidate by invalidating the cycle.
#[derive(Debug, Clone)]
pub struct TabCycle {
    head: String,
    tail: String,
    /// The original token text, byte-exact from the draft (Esc restores it).
    original: String,
    cands: Vec<String>,
    /// -1 = cycle entered but nothing applied yet; else index into `cands`.
    pos: i64,
    /// The exact draft this cycle last produced.
    pub(crate) expect: String,
}

impl TabCycle {
    /// Advance by `delta` presses (+forward / −reverse, wrapping) and return
    /// (new draft, caret in CHARS at the completed token's end).
    pub(crate) fn step(&mut self, delta: i64) -> (String, usize) {
        let n = self.cands.len() as i64; // ≥ 2 by construction
        self.pos = if self.pos < 0 {
            // Entering from the original token: the first forward press
            // lands on candidate 0, the first reverse press on the LAST.
            if delta > 0 {
                (delta - 1).rem_euclid(n)
            } else {
                delta.rem_euclid(n)
            }
        } else {
            (self.pos + delta).rem_euclid(n)
        };
        let c = &self.cands[self.pos as usize];
        let draft = format!("{}{}{}", self.head, c, self.tail);
        self.expect = draft.clone();
        (draft, self.head.chars().count() + c.chars().count())
    }

    /// Esc: the draft with the ORIGINAL token back in place.
    pub(crate) fn restore(&self) -> (String, usize) {
        (
            format!("{}{}{}", self.head, self.original, self.tail),
            self.head.chars().count() + self.original.chars().count(),
        )
    }

    /// The cycle is still authoritative for this draft.
    pub(crate) fn matches(&self, draft: &str) -> bool {
        self.expect == draft
    }
}

/// What one cycle-start resolves to.
pub(crate) enum Start {
    /// ≥2 candidates: a cycle (the caller steps it immediately).
    Cycle(TabCycle),
    /// One-shot draft edit (single candidate, or over-cap common prefix) —
    /// no cycle; the NEXT Tab re-plans from the completed token, which is
    /// what makes a completed directory descend.
    Edit { draft: String, caret: usize },
    /// remote-completion: the directory is only knowable from the shell on
    /// the other machine and no listing is in hand. The caller asks for it
    /// (`C2D::RequestCompletion`) and this Tab does NOTHING — identical to
    /// the pre-remote behaviour, which is what makes the lane safe to add:
    /// the worst case is the old no-op.
    Request(String),
    /// Nothing to do — the Tab was consumed regardless (never spaces).
    None,
}

/// Resolve a fresh Tab press: tokenize at the caret (bytes), resolve the
/// token's parent against the tracked cwd, enumerate, and build rendered
/// candidates. `cap` is `ENUM_CAP` in production (parameterized for tests).
pub(crate) fn start(
    fam: &Family,
    cwd: Option<&str>,
    home: Option<&str>,
    draft: &str,
    caret: usize,
    cap: usize,
    remote: Option<&dyn RemoteDirs>,
) -> Start {
    let tok = token_at(fam, draft, caret.min(draft.len()));
    let Some(plan) = plan(fam, cwd, home, &tok.value) else {
        return Start::None;
    };
    let out = match enumerate(&plan, cap, remote) {
        Enumerated::Found(out) => out,
        Enumerated::Ask(dir) => return Start::Request(dir),
        Enumerated::Nothing => return Start::None,
    };
    if out.matches.is_empty() {
        return Start::None;
    }
    let head = draft[..tok.start].to_string();
    let tail = draft[tok.end..].to_string();
    if out.capped {
        // Over-cap dir: extend to the common prefix only, never cycle.
        let lcp = common_prefix(&out.matches, plan.ci);
        if lcp.chars().count() <= plan.prefix.chars().count() {
            return Start::None;
        }
        let rendered = render_token(&plan, fam, &lcp, false);
        let new_draft = format!("{head}{rendered}{tail}");
        if new_draft == draft {
            return Start::None;
        }
        let caret = head.chars().count() + rendered.chars().count();
        return Start::Edit {
            draft: new_draft,
            caret,
        };
    }
    let cands: Vec<String> = out
        .matches
        .iter()
        .map(|e| render_token(&plan, fam, &e.name, e.dir))
        .collect();
    if cands.len() == 1 {
        let new_draft = format!("{head}{}{tail}", cands[0]);
        if new_draft == draft {
            return Start::None;
        }
        let caret = head.chars().count() + cands[0].chars().count();
        return Start::Edit {
            draft: new_draft,
            caret,
        };
    }
    Start::Cycle(TabCycle {
        head,
        tail,
        original: draft[tok.start..tok.end].to_string(),
        cands,
        pos: -1,
        expect: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn pwsh() -> Family {
        Family::Pwsh
    }
    fn wsl(d: Option<&str>) -> Family {
        Family::Wsl {
            distro: d.map(str::to_string),
        }
    }

    /// The LOCAL target of a plan — `Dir::Local`, or the local half of
    /// `Dir::LocalThenRemote` (the WSL UNC view, tried first). Panics on a
    /// purely remote plan, which is what every assertion here wants: these
    /// tests are about the local lanes staying byte-identical.
    fn local_of(p: &Plan) -> PathBuf {
        match &p.dir {
            Dir::Local(d) | Dir::LocalThenRemote(d, _) => d.clone(),
            Dir::Remote(d) => panic!("expected a local plan, got remote {d}"),
        }
    }

    /// The REMOTE target of a plan: the string the shell will be asked for.
    fn remote_of(p: &Plan) -> &str {
        match &p.dir {
            Dir::Remote(d) | Dir::LocalThenRemote(_, d) => d,
            Dir::Local(d) => panic!("expected a remote plan, got local {d:?}"),
        }
    }

    /// A hand-built listing cache — the whole remote lane is testable
    /// without a daemon, a PTY or a network.
    #[derive(Default)]
    struct FakeRemote {
        dirs: std::collections::HashMap<String, RemoteListing>,
        declined: std::collections::HashSet<String>,
    }

    impl FakeRemote {
        fn with(mut self, dir: &str, names: &[(&str, bool)]) -> Self {
            self.dirs.insert(
                dir.to_string(),
                RemoteListing {
                    entries: names
                        .iter()
                        .map(|(n, d)| Entry { name: n.to_string(), dir: *d })
                        .collect(),
                    trunc: false,
                },
            );
            self
        }
        fn truncated(mut self, dir: &str) -> Self {
            self.dirs.insert(
                dir.to_string(),
                RemoteListing { entries: Vec::new(), trunc: true },
            );
            self
        }
        fn declined(mut self, dir: &str) -> Self {
            self.declined.insert(dir.to_string());
            self
        }
    }

    impl RemoteDirs for FakeRemote {
        fn listing(&self, dir: &str) -> Option<RemoteListing> {
            self.dirs.get(dir).cloned()
        }
        fn declined(&self, dir: &str) -> bool {
            self.declined.contains(dir)
        }
    }

    /// Fresh temp dir per test (removed best-effort at the end).
    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "tc_tab_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    // ── tokenizer: quotes, escapes, caret positions ──────────────────────

    #[test]
    fn tokenizer_splits_on_unquoted_whitespace_at_caret() {
        let t = token_at(&pwsh(), "cd ../", 6);
        assert_eq!((t.start, t.end, t.value.as_str()), (3, 6, "../"));
        // Caret inside the first token.
        let t = token_at(&pwsh(), "cd ../", 2);
        assert_eq!((t.start, t.end, t.value.as_str()), (0, 2, "cd"));
        // Caret in whitespace (or at a token's FIRST byte): empty token AT
        // the caret — readline's empty-stem completion point.
        let t = token_at(&pwsh(), "cd ../", 3);
        assert_eq!((t.start, t.end, t.value.as_str()), (3, 3, ""));
        let t = token_at(&pwsh(), "cd ", 3);
        assert_eq!((t.start, t.end, t.value.as_str()), (3, 3, ""));
        // Multi-line drafts: \n is whitespace, tokens never span lines.
        let t = token_at(&pwsh(), "ls\ncd src", 9);
        assert_eq!((t.start, t.end, t.value.as_str()), (6, 9, "src"));
    }

    #[test]
    fn tokenizer_pwsh_quotes_doubling_and_unterminated() {
        let s = r"cd 'C:\a b\c'";
        let t = token_at(&pwsh(), s, s.len());
        assert_eq!((t.start, t.end), (3, s.len()));
        assert_eq!(t.value, r"C:\a b\c");
        // Doubled inner quote.
        let s = "type 'it''s.txt'";
        let t = token_at(&pwsh(), s, s.len());
        assert_eq!(t.value, "it's.txt");
        // Unterminated quote runs to the end (mid-edit state).
        let s = r"cd 'C:\a b";
        let t = token_at(&pwsh(), s, s.len());
        assert_eq!((t.start, t.end), (3, s.len()));
        assert_eq!(t.value, r"C:\a b");
    }

    #[test]
    fn tokenizer_cmd_double_quotes() {
        let s = r#"type "C:\a b\f.txt" x"#;
        let t = token_at(&Family::Cmd, s, 19);
        assert_eq!((t.start, t.end), (5, 19));
        assert_eq!(t.value, r"C:\a b\f.txt");
    }

    #[test]
    fn tokenizer_bash_escapes_and_mixed_quotes() {
        let s = r"ls a\ b/c";
        let t = token_at(&wsl(Some("U")), s, s.len());
        assert_eq!((t.start, t.end), (3, s.len()));
        assert_eq!(t.value, "a b/c");
        // Quote closing mid-token continues the same token.
        let s = "ls 'x y'/z";
        let t = token_at(&wsl(Some("U")), s, s.len());
        assert_eq!(t.value, "x y/z");
    }

    // ── plan: Windows namespace ──────────────────────────────────────────

    #[test]
    fn plan_win_relative_absolute_root_home_and_drive() {
        let cwd = Some(r"C:\proj");
        // Relative with ../ keeps the typed spelling; sep style follows.
        let p = plan(&pwsh(), cwd, None, "../").unwrap();
        assert_eq!(local_of(&p), PathBuf::from(r"C:\proj").join("../"));
        assert_eq!((p.render_parent.as_str(), p.sep, p.ci), ("../", '/', true));
        // Relative subdir, backslash style.
        let p = plan(&pwsh(), cwd, None, r"src\ma").unwrap();
        assert_eq!(local_of(&p), PathBuf::from(r"C:\proj").join(r"src\"));
        assert_eq!((p.prefix.as_str(), p.sep), ("ma", '\\'));
        // Absolute.
        let p = plan(&pwsh(), None, None, r"C:\Users\za").unwrap();
        assert_eq!(local_of(&p), PathBuf::from(r"C:\Users\"));
        assert_eq!(p.prefix, "za");
        // Bare drive completes the ROOT (never drive-relative).
        let p = plan(&pwsh(), None, None, "C:").unwrap();
        assert_eq!(local_of(&p), PathBuf::from(r"C:\"));
        assert_eq!((p.prefix.as_str(), p.render_parent.as_str()), ("", r"C:\"));
        // Root-relative anchors at the cwd's drive.
        let p = plan(&pwsh(), cwd, None, r"\tools\x").unwrap();
        assert_eq!(local_of(&p), PathBuf::from(r"C:\tools\"));
        // ~: pwsh renders it back as typed; cmd expands (no ~ in cmd).
        let home = Some(r"C:\Users\z");
        let p = plan(&pwsh(), None, home, "~/Doc").unwrap();
        assert_eq!(local_of(&p), PathBuf::from(r"C:\Users\z/"));
        assert_eq!(p.render_parent, "~/");
        let p = plan(&Family::Cmd, None, home, r"~\Doc").unwrap();
        assert_eq!(p.render_parent, r"C:\Users\z\");
        // No cwd ⇒ relative tokens can't resolve.
        assert_eq!(plan(&pwsh(), None, None, "src/"), None);
        // Bare ~ without a separator no-ops (documented v1).
        assert_eq!(plan(&pwsh(), None, home, "~"), None);
    }

    // ── plan: WSL posix ↔ local mapping ──────────────────────────────────

    #[test]
    fn posix_to_local_maps_mnt_and_unc() {
        assert_eq!(
            posix_to_local("/mnt/c/Users/z", Some("U")).unwrap(),
            PathBuf::from(r"C:\Users\z")
        );
        assert_eq!(posix_to_local("/mnt/d", None).unwrap(), PathBuf::from(r"D:\"));
        assert_eq!(
            posix_to_local("/home/z", Some("Ubuntu-24.04")).unwrap(),
            PathBuf::from(r"\\wsl.localhost\Ubuntu-24.04\home\z")
        );
        // /mnt itself lists through the distro fs (mount points visible).
        assert_eq!(
            posix_to_local("/mnt/", Some("U")).unwrap(),
            PathBuf::from(r"\\wsl.localhost\U\mnt\")
        );
        // Default distro has no UNC name; relative paths never map.
        assert_eq!(posix_to_local("/home/z", None), None);
        assert_eq!(posix_to_local("rel/x", Some("U")), None);
    }

    #[test]
    fn plan_wsl_tokens_and_cwds() {
        let d = Some("Ubuntu-24.04");
        // Relative against a posix cwd → UNC enumeration, posix render.
        let p = plan(&wsl(d), Some("/home/z"), None, "../").unwrap();
        assert_eq!(
            local_of(&p),
            PathBuf::from(r"\\wsl.localhost\Ubuntu-24.04\home\z\..\")
        );
        assert_eq!((p.render_parent.as_str(), p.sep), ("../", '/'));
        assert!(!p.ci);
        assert!(p.posix_hidden);
        // /mnt token → drive-letter enumeration (local, no UNC budget).
        let p = plan(&wsl(d), Some("/home/z"), None, "/mnt/c/Us").unwrap();
        assert_eq!(local_of(&p), PathBuf::from(r"C:\"));
        assert_eq!(p.prefix, "Us");
        // Windows-shaped pre-first-cd cwd translates through /mnt.
        let p = plan(&wsl(d), Some(r"C:\proj"), None, "src/").unwrap();
        assert_eq!(local_of(&p), PathBuf::from(r"C:\proj\src\"));
        // Default distro: /mnt still maps to the drive letter; a distro-fs
        // path has no UNC name to build, so remote-completion routes it to
        // the shell instead of the old silent no-op.
        let p = plan(&wsl(None), Some("/mnt/c/x"), None, "y/").unwrap();
        assert_eq!(local_of(&p), PathBuf::from(r"C:\x\y\"));
        let p = plan(&wsl(None), Some("/home/z"), None, "y/").unwrap();
        assert_eq!(p.dir, Dir::Remote("/home/z/y".into()));
        // `~` likewise: the SHELL knows the distro user's home, so the token
        // goes to it verbatim (never a %USERPROFILE% guess).
        let p = plan(&wsl(d), Some("/home/z"), None, "~/x").unwrap();
        assert_eq!(p.dir, Dir::Remote("~".into()));
    }

    // ── enumeration: ordering, filters, cap ──────────────────────────────

    #[test]
    fn enumerate_orders_dirs_first_alpha_and_matches_prefix() {
        let dir = scratch("order");
        std::fs::create_dir(dir.join("bravo")).unwrap();
        std::fs::create_dir(dir.join("alpha")).unwrap();
        touch(&dir, "apple.txt");
        touch(&dir, "Notes.txt");
        let out = enum_dir(&dir, "", true, false, ENUM_CAP).unwrap();
        let names: Vec<&str> = out.matches.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "bravo", "apple.txt", "Notes.txt"]);
        assert!(!out.capped);
        // Case-insensitive prefix (Windows namespace).
        let out = enum_dir(&dir, "A", true, false, ENUM_CAP).unwrap();
        let names: Vec<&str> = out.matches.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "apple.txt"]);
        // Case-sensitive (posix): "N" hits Notes.txt only, "n" nothing.
        let out = enum_dir(&dir, "N", false, false, ENUM_CAP).unwrap();
        assert_eq!(out.matches.len(), 1);
        assert_eq!(out.matches[0].name, "Notes.txt");
        assert!(enum_dir(&dir, "n", false, false, ENUM_CAP).unwrap().matches.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enumerate_posix_dotfile_rule() {
        let dir = scratch("hidden");
        std::fs::create_dir(dir.join(".git")).unwrap();
        touch(&dir, "main.rs");
        // Posix rule: dotfiles hidden for a bare prefix…
        let out = enum_dir(&dir, "", false, true, ENUM_CAP).unwrap();
        assert_eq!(out.matches.len(), 1);
        assert_eq!(out.matches[0].name, "main.rs");
        // …revealed when the prefix asks for them…
        let out = enum_dir(&dir, ".", false, true, ENUM_CAP).unwrap();
        assert_eq!(out.matches[0].name, ".git");
        // …and Windows namespace ignores the convention entirely.
        let out = enum_dir(&dir, "", true, false, ENUM_CAP).unwrap();
        assert_eq!(out.matches.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn over_cap_dir_completes_common_prefix_only_no_cycle() {
        let dir = scratch("cap");
        for i in 0..4 {
            touch(&dir, &format!("aaa{i}.txt"));
        }
        let cwd = dir.to_str().unwrap();
        // cap 3 < 4 entries ⇒ capped ⇒ Edit to the common prefix "aaa".
        match start(&pwsh(), Some(cwd), None, "cd a", 4, 3, None) {
            Start::Edit { draft, caret } => {
                assert_eq!(draft, "cd aaa");
                assert_eq!(caret, 6);
            }
            _ => panic!("expected common-prefix Edit"),
        }
        // No progress beyond the typed prefix ⇒ honest no-op.
        assert!(matches!(
            start(&pwsh(), Some(cwd), None, "cd aaa", 6, 3, None),
            Start::None
        ));
        // Under the cap the same dir cycles normally.
        assert!(matches!(
            start(&pwsh(), Some(cwd), None, "cd a", 4, ENUM_CAP, None),
            Start::Cycle(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cycle machine + rendering round-trips ────────────────────────────

    #[test]
    fn cycle_forward_reverse_wrap_and_restore() {
        let dir = scratch("cycle");
        std::fs::create_dir(dir.join("alpha")).unwrap();
        std::fs::create_dir(dir.join("bravo")).unwrap();
        touch(&dir, "a file.txt");
        let cwd = dir.to_str().unwrap();
        let Start::Cycle(mut c) = start(&pwsh(), Some(cwd), None, "cd ", 3, ENUM_CAP, None) else {
            panic!("expected cycle");
        };
        // Forward: dirs first alphabetically, then the quoted spacey file.
        assert_eq!(c.step(1).0, r"cd alpha\");
        assert_eq!(c.step(1).0, r"cd bravo\");
        let (d, caret) = c.step(1);
        assert_eq!(d, "cd 'a file.txt'");
        assert_eq!(caret, d.chars().count());
        // Wraps forward, then reverse steps walk back.
        assert_eq!(c.step(1).0, r"cd alpha\");
        assert_eq!(c.step(-1).0, "cd 'a file.txt'");
        // Esc restores the original (empty) token byte-exact.
        assert_eq!(c.restore().0, "cd ");
        // Validity tracking: the last applied draft matches, others don't.
        assert!(c.matches("cd 'a file.txt'"));
        assert!(!c.matches("cd 'a file.txt' x"));
        // A fresh REVERSE entry lands on the LAST candidate.
        let Start::Cycle(mut c) = start(&pwsh(), Some(cwd), None, "cd ", 3, ENUM_CAP, None) else {
            panic!("expected cycle");
        };
        assert_eq!(c.step(-1).0, "cd 'a file.txt'");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_candidate_completes_then_descends() {
        let dir = scratch("single");
        std::fs::create_dir(dir.join("src")).unwrap();
        touch(&dir.join("src"), "main.rs");
        let cwd = dir.to_str().unwrap();
        let Start::Edit { draft, caret } = start(&pwsh(), Some(cwd), None, "cd sr", 5, ENUM_CAP, None)
        else {
            panic!("expected single-candidate Edit");
        };
        assert_eq!(draft, r"cd src\");
        assert_eq!(caret, 7);
        // The NEXT Tab re-plans from the completed token — descends.
        let Start::Edit { draft, .. } = start(&pwsh(), Some(cwd), None, &draft, 7, ENUM_CAP, None)
        else {
            panic!("expected descent");
        };
        assert_eq!(draft, r"cd src\main.rs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quoted_token_roundtrips_through_completion() {
        let dir = scratch("roundtrip");
        std::fs::create_dir(dir.join("a dir")).unwrap();
        touch(&dir.join("a dir"), "it's.txt");
        let cwd = dir.to_str().unwrap();
        // Completing into a spacey dir quotes the WHOLE token…
        let Start::Edit { draft, caret } = start(&pwsh(), Some(cwd), None, "cd a", 4, ENUM_CAP, None)
        else {
            panic!();
        };
        assert_eq!(draft, r"cd 'a dir\'");
        // …and the quoted token re-tokenizes for the next Tab: descend into
        // it, meeting a name with a quote (pwsh doubles it).
        let Start::Edit { draft, .. } =
            start(&pwsh(), Some(cwd), None, &draft, caret, ENUM_CAP, None)
        else {
            panic!();
        };
        assert_eq!(draft, r"cd 'a dir\it''s.txt'");
        // The doubled form still parses back to the real name.
        let t = token_at(&pwsh(), &draft, draft.len());
        assert_eq!(t.value, r"a dir\it's.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wsl_renders_posix_case_sensitive_and_quoted() {
        // Pure render check (no WSL required): the plan carries posix
        // spelling; names with spaces get bash single quotes.
        let p = plan(&wsl(Some("U")), Some("/home/z"), None, "pro").unwrap();
        assert_eq!(render_token(&p, &wsl(Some("U")), "projects", true), "projects/");
        assert_eq!(
            render_token(&p, &wsl(Some("U")), "my stuff", true),
            "'my stuff/'"
        );
        let p = plan(&wsl(Some("U")), Some("/home/z"), None, "../pro").unwrap();
        assert_eq!(
            render_token(&p, &wsl(Some("U")), "it's", false),
            r"'../it'\''s'"
        );
    }

    #[test]
    fn cmd_and_other_quoting_via_drop_helpers() {
        let dir = scratch("cmdq");
        std::fs::create_dir(dir.join("Program Files")).unwrap();
        std::fs::create_dir(dir.join("plain")).unwrap();
        let cwd = dir.to_str().unwrap();
        let Start::Cycle(mut c) = start(&Family::Cmd, Some(cwd), None, "cd p", 4, ENUM_CAP, None)
        else {
            panic!();
        };
        assert_eq!(c.step(1).0, "cd plain\\");
        assert_eq!(c.step(1).0, "cd \"Program Files\\\"");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── remote-completion: the source matrix ─────────────────────────────

    /// WHERE each (family, cwd) pair takes its candidates from — the whole
    /// point of the change, as one table.
    ///
    /// The rows that were SILENT no-ops before this exists are the
    /// field-reported ones: a Windows family whose tracked cwd is a POSIX
    /// path (a typed `ssh host`, or a `sudo su` inside it), an ssh-program
    /// terminal, and a default-distro WSL shell outside `/mnt`.
    #[test]
    fn completion_source_matrix() {
        // Local pwsh / cmd / other: a drive-lettered cwd, one local
        // read_dir. The remote cache is never consulted.
        for fam in [pwsh(), Family::Cmd, Family::Other] {
            let p = plan(&fam, Some(r"C:\proj"), None, "s").unwrap();
            assert!(matches!(p.dir, Dir::Local(_)), "{fam:?} must stay local");
        }
        // WSL with a NAMED distro: the UNC view first, the hook channel
        // behind it (a nested root shell's /root is EACCES over the share).
        let p = plan(&wsl(Some("U")), Some("/home/z"), None, "d").unwrap();
        assert_eq!(
            p.dir,
            Dir::LocalThenRemote(
                PathBuf::from(r"\\wsl.localhost\U\home\z\"),
                "/home/z".into()
            )
        );
        // WSL DEFAULT distro inside the distro fs: no UNC name to build —
        // it used to return None (silent no-op); now the shell answers.
        let p = plan(&wsl(None), Some("/home/z"), None, "d").unwrap();
        assert_eq!(p.dir, Dir::Remote("/home/z".into()));
        // ...and /mnt still resolves to a DRIVE LETTER, which is an ordinary
        // local directory — the hook channel sits behind it as a fallback
        // that only matters if the Windows-side read fails.
        let p = plan(&wsl(None), Some("/mnt/c/x"), None, "d").unwrap();
        assert_eq!(
            p.dir,
            Dir::LocalThenRemote(PathBuf::from(r"C:\x\"), "/mnt/c/x".into())
        );

        // An ssh-PROGRAM terminal: remote, always.
        let p = plan(&Family::Remote, Some("/var/log"), None, "s").unwrap();
        assert_eq!(p.dir, Dir::Remote("/var/log".into()));

        // THE FIELD REPORT: pwsh/cmd that typed `ssh host` and cd'd on the
        // far side. `ShellFamily` still says Pwsh/Cmd; the tracked cwd is
        // the remote shell's own reported $PWD, and that is the witness.
        for fam in [pwsh(), Family::Cmd] {
            let eff = effective_family(&fam, Some("/root"));
            assert_eq!(eff, Family::Remote, "{fam:?} inside a remote shell");
            let p = plan(&eff, Some("/root"), None, "pro").unwrap();
            assert_eq!(p.dir, Dir::Remote("/root".into()));
            assert_eq!(p.prefix, "pro");
            assert!(!p.ci, "posix is case-SENSITIVE");
            assert!(p.posix_hidden, "posix hides dotfiles");
            // With no cwd witness at all, a Windows family stays Windows —
            // never promoted on a guess.
            assert_eq!(effective_family(&fam, None), fam);
            assert_eq!(effective_family(&fam, Some(r"C:\proj")), fam);
        }
        // WSL and Remote are never re-derived: WSL keeps its local lane.
        assert_eq!(
            effective_family(&wsl(Some("U")), Some("/home/z")),
            wsl(Some("U"))
        );
        assert_eq!(effective_family(&Family::Remote, None), Family::Remote);
    }

    /// `plan_remote` resolves only what it can know and hands the rest to
    /// the shell, which is the only thing that knows a remote `$HOME`.
    #[test]
    fn plan_remote_requests_and_render_spelling() {
        let r = Family::Remote;
        // Empty token: the cwd itself — and spelled EXACTLY as the prompt
        // hook reports `$PWD`, so the prefetch is a cache HIT (the whole
        // reason `cd <Tab>` is instant).
        let p = plan(&r, Some("/root"), None, "").unwrap();
        assert_eq!(remote_of(&p), "/root");
        assert_eq!((p.prefix.as_str(), p.render_parent.as_str()), ("", ""));
        // Relative: anchored at the cwd, trailing separator canonicalized
        // away so `src` and `src/` are ONE key on both sides.
        let p = plan(&r, Some("/root"), None, "src/ma").unwrap();
        assert_eq!(remote_of(&p), "/root/src");
        assert_eq!((p.prefix.as_str(), p.render_parent.as_str()), ("ma", "src/"));
        // `..` is left for the shell to resolve — folding it here would walk
        // through a symlinked parent the user's own `cd ..` would not.
        let p = plan(&r, Some("/home/z"), None, "../").unwrap();
        assert_eq!(remote_of(&p), "/home/z/..");
        // Absolute.
        let p = plan(&r, Some("/root"), None, "/etc/ss").unwrap();
        assert_eq!((remote_of(&p), p.prefix.as_str()), ("/etc", "ss"));
        // Root is its own separator.
        let p = plan(&r, Some("/root"), None, "/e").unwrap();
        assert_eq!((remote_of(&p), p.prefix.as_str()), ("/", "e"));
        // A cwd that IS the root still anchors.
        let p = plan(&r, Some("/"), None, "e").unwrap();
        assert_eq!(remote_of(&p), "/");
        // `~` goes to the shell VERBATIM — the remote home is unknowable
        // here, and guessing %USERPROFILE% would complete a Windows path
        // that does not exist on that host.
        let p = plan(&r, Some("/root"), None, "~/Doc").unwrap();
        assert_eq!((remote_of(&p), p.render_parent.as_str()), ("~", "~/"));
        let p = plan(&r, Some("/root"), None, "~/").unwrap();
        assert_eq!(remote_of(&p), "~");
        // A WSL `~` takes the same lane (it used to be a flat no-op).
        let p = plan(&wsl(Some("U")), Some("/home/z"), None, "~/x").unwrap();
        assert_eq!(remote_of(&p), "~");
        // A relative token with no cwd, or a Windows-shaped one, cannot be
        // anchored: honest None, exactly like the local lanes.
        assert_eq!(plan(&r, None, None, "src/"), None);
        assert_eq!(plan(&r, Some(r"C:\proj"), None, "src/"), None);
    }

    /// The remote lane cycles, orders and quotes IDENTICALLY to the local
    /// one — same dirs-first ordering, same trailing separator, same posix
    /// dotfile rule, same bash quoting.
    #[test]
    fn remote_listing_cycles_like_a_local_dir() {
        let r = Family::Remote;
        let rem = FakeRemote::default().with(
            "/root",
            &[
                ("notes.txt", false),
                ("bravo", true),
                ("alpha", true),
                ("a file.txt", false),
                (".hidden", false),
                (".config", true),
            ],
        );
        let Start::Cycle(mut c) =
            start(&r, Some("/root"), None, "cd ", 3, ENUM_CAP, Some(&rem))
        else {
            panic!("expected a cycle from the cached listing");
        };
        // Dirs first, alphabetical within groups; dotfiles hidden for a bare
        // prefix; a spacey name bash-quoted as a WHOLE token.
        assert_eq!(c.step(1).0, "cd alpha/");
        assert_eq!(c.step(1).0, "cd bravo/");
        assert_eq!(c.step(1).0, "cd 'a file.txt'");
        assert_eq!(c.step(1).0, "cd notes.txt");
        assert_eq!(c.step(1).0, "cd alpha/", "wraps");
        assert_eq!(c.restore().0, "cd ", "Esc restores byte-exact");
        // A `.` prefix reveals them, dirs still first.
        let Start::Cycle(mut c) =
            start(&r, Some("/root"), None, "cd .", 4, ENUM_CAP, Some(&rem))
        else {
            panic!();
        };
        assert_eq!(c.step(1).0, "cd .config/");
        assert_eq!(c.step(1).0, "cd .hidden");
        // Case SENSITIVITY: posix never folds.
        let rem2 = FakeRemote::default().with("/root", &[("Notes", false)]);
        assert!(matches!(
            start(&r, Some("/root"), None, "cd n", 4, ENUM_CAP, Some(&rem2)),
            Start::None
        ));
        // A single candidate completes without a cycle and the NEXT Tab
        // descends — which is what asks for the subdirectory's listing.
        let rem3 = FakeRemote::default().with("/root", &[("src", true)]);
        let Start::Edit { draft, caret } =
            start(&r, Some("/root"), None, "cd sr", 5, ENUM_CAP, Some(&rem3))
        else {
            panic!("expected a single-candidate Edit");
        };
        assert_eq!((draft.as_str(), caret), ("cd src/", 7));
        assert!(matches!(
            start(&r, Some("/root"), None, &draft, caret, ENUM_CAP, Some(&rem3)),
            Start::Request(ref d) if d == "/root/src"
        ));
    }

    /// Everything the remote lane does when it has no answer — and every one
    /// of them is the pre-remote silent no-op or an ask, never a guess.
    #[test]
    fn remote_misses_ask_once_then_degrade() {
        let r = Family::Remote;
        // Unknown directory with a cache present: ASK, and the Tab does
        // nothing this frame.
        let rem = FakeRemote::default();
        assert!(matches!(
            start(&r, Some("/root"), None, "cd x", 4, ENUM_CAP, Some(&rem)),
            Start::Request(ref d) if d == "/root"
        ));
        // A DEFINITIVE nothing came back for it: no candidates, and no
        // further asking — holding Tab costs one request, not hundreds.
        let rem = FakeRemote::default().declined("/root");
        assert!(matches!(
            start(&r, Some("/root"), None, "cd x", 4, ENUM_CAP, Some(&rem)),
            Start::None
        ));
        // NO cache at all: byte-identical to the behaviour this change
        // replaced — nothing to ask THROUGH, so nothing is asked.
        //
        // v0.1.20's comment here read "a pre-proto-14 daemon, a terminal with
        // no remote world", and that second clause was the field bug: a
        // terminal that very much HAS a remote world also arrives here,
        // because the app's cache was created only BY an ask. See
        // `remote_lane_possible` and `an_empty_cache_asks_a_missing_one_cannot`.
        assert!(matches!(
            start(&r, Some("/root"), None, "cd x", 4, ENUM_CAP, None),
            Start::None
        ));
        // Over-cap directory: the shell dropped the listing rather than send
        // a subset, so there is nothing to complete and nothing to prefix —
        // a common prefix over a subset would OVER-complete.
        let rem = FakeRemote::default().truncated("/root");
        assert!(matches!(
            start(&r, Some("/root"), None, "cd a", 4, ENUM_CAP, Some(&rem)),
            Start::None
        ));
        // An empty directory is an ANSWER, not a miss: no candidates, no ask.
        let rem = FakeRemote::default().with("/root", &[]);
        assert!(matches!(
            start(&r, Some("/root"), None, "cd a", 4, ENUM_CAP, Some(&rem)),
            Start::None
        ));
    }

    /// THE v0.1.20 FIELD BUG, as a pure assertion.
    ///
    /// `cd pr<Tab>` inside a real ssh session did nothing at all, forever,
    /// because the two halves below are NOT the same thing and the app could
    /// only ever supply the second one on a first Tab: a listing cache that
    /// exists and is empty parks an ask, and a cache that does not exist
    /// cannot. The app's cache was created only as a CONSEQUENCE of an ask,
    /// so the ask that would have created it was never produced — a closed
    /// circle, in every remote shape, with nothing in any log.
    ///
    /// `remote_lane_possible` is what breaks it: the app now creates the
    /// cache from the tracked cwd, before any Tab.
    #[test]
    fn an_empty_cache_asks_a_missing_one_cannot() {
        let r = Family::Remote;
        let empty = FakeRemote::default();
        // His exact gesture: a bare `pr` stem anchored at the remote $PWD.
        assert!(
            matches!(
                start(&r, Some("/home/dev"), None, "cd pr", 5, ENUM_CAP, Some(&empty)),
                Start::Request(ref d) if d == "/home/dev"
            ),
            "an empty cache must park the ask for the remote cwd"
        );
        assert!(
            matches!(
                start(&r, Some("/home/dev"), None, "cd pr", 5, ENUM_CAP, None),
                Start::None
            ),
            "no cache at all cannot ask — which is why one must EXIST first"
        );
        // And with the answer in hand the same press completes, including the
        // prefix-of-another-directory shape he hit.
        let rem = FakeRemote::default().with(
            "/home/dev",
            &[("pre-migration-backup", true), ("pre", true), ("prod.log", false)],
        );
        let Start::Cycle(mut c) =
            start(&r, Some("/home/dev"), None, "cd pr", 5, ENUM_CAP, Some(&rem))
        else {
            panic!("the answered cache must cycle");
        };
        assert_eq!(c.step(1).0, "cd pre/");
        assert_eq!(c.step(1).0, "cd pre-migration-backup/");
        assert_eq!(c.step(1).0, "cd prod.log");
    }

    /// Which terminals must be handed a listing cache before their first Tab
    /// — the predicate the app keys the fix on. It must say YES for every
    /// shape that can reach `Dir::Remote`, and NO for every purely local one
    /// (which must keep carrying no cache at all).
    #[test]
    fn remote_lane_possible_follows_the_tracked_cwd() {
        // The field shape: a pwsh/cmd terminal that typed `ssh host`. The
        // SPAWN family never changes; the POSIX cwd is the whole witness.
        for fam in [Family::Pwsh, Family::Cmd, Family::Other] {
            assert!(
                remote_lane_possible(&fam, Some("/home/dev")),
                "{fam:?} with a posix cwd is a remote world"
            );
            assert!(
                remote_lane_possible(&fam, Some("/root")),
                "{fam:?} inside a nested sudo su is still remote"
            );
            // Purely local: no cache, exactly as before.
            assert!(!remote_lane_possible(&fam, Some(r"C:\Users\dev")));
            assert!(!remote_lane_possible(&fam, None));
        }
        // An ssh-PROGRAM terminal (`family_for(ShellFamily::Ssh)`) is remote
        // before it has reported any cwd at all — `cd /et<Tab>` there needs
        // the lane with cwd None.
        assert!(remote_lane_possible(&Family::Remote, None));
        // WSL keeps its local UNC leg but can still fall through to the hook
        // channel (`/root` under a nested root shell, a default distro with
        // no UNC name), so it needs the cache too — including inside `/mnt`,
        // where a `~` token in the same draft can still ask.
        assert!(remote_lane_possible(
            &Family::Wsl { distro: Some("Ubuntu-24.04".into()) },
            Some("/home/dev")
        ));
        assert!(remote_lane_possible(&Family::Wsl { distro: None }, Some("/mnt/c")));
        // A WSL terminal needs it even before it reports a cwd.
        assert!(remote_lane_possible(&Family::Wsl { distro: None }, None));

        // The two WSL shapes the lane actually answers, now that the cache
        // exists for them. `~` goes straight to the shell (the GUI cannot
        // know a distro home), and a DEFAULT distro has no UNC name to build,
        // so anything outside `/mnt` is the shell's to answer too.
        let named = Family::Wsl { distro: Some("Ubuntu-24.04".into()) };
        let dflt = Family::Wsl { distro: None };
        let empty = FakeRemote::default();
        assert!(
            matches!(
                start(&named, Some("/home/z"), None, "cd ~/pr", 7, ENUM_CAP, Some(&empty)),
                Start::Request(ref d) if d == "~"
            ),
            "`~` in WSL must ask the shell, never guess a Windows home"
        );
        assert!(
            matches!(
                start(&dflt, Some("/home/z"), None, "cd pr", 5, ENUM_CAP, Some(&empty)),
                Start::Request(ref d) if d == "/home/z"
            ),
            "a default-distro WSL home has no UNC view — it must ask"
        );
        // ...while a `/mnt` cwd the Windows side CAN enumerate never leaves
        // the local lane: `C:\` is read, nothing matches, and no ask goes out.
        assert!(matches!(
            start(&dflt, Some("/mnt/c"), None, "cd __tc_absent__", 16, ENUM_CAP, Some(&empty)),
            Start::None
        ));
    }

    /// The WSL fallback: local UNC first (byte-identical latency and
    /// results), the hook channel only when the Windows-side view cannot
    /// answer at all — the nested `sudo su` `/root` case.
    #[test]
    fn wsl_prefers_the_local_view_and_falls_back() {
        let dir = scratch("wslfb");
        std::fs::create_dir(dir.join("local_only")).unwrap();
        // A plan whose LOCAL half exists: the remote cache is ignored even
        // when it holds a different (stale) answer.
        let p = Plan {
            dir: Dir::LocalThenRemote(dir.clone(), "/home/z".into()),
            prefix: String::new(),
            render_parent: String::new(),
            sep: '/',
            ci: false,
            posix_hidden: true,
        };
        let rem = FakeRemote::default().with("/home/z", &[("remote_only", true)]);
        let Enumerated::Found(out) = enumerate(&p, ENUM_CAP, Some(&rem)) else {
            panic!("the local view must win");
        };
        assert_eq!(out.matches.len(), 1);
        assert_eq!(out.matches[0].name, "local_only");
        // An UNREADABLE local half (EACCES / missing — a root-owned /root
        // over the UNC share) falls through to the shell.
        let p = Plan {
            dir: Dir::LocalThenRemote(dir.join("nope"), "/home/z".into()),
            ..p
        };
        let Enumerated::Found(out) = enumerate(&p, ENUM_CAP, Some(&rem)) else {
            panic!("expected the remote fallback");
        };
        assert_eq!(out.matches[0].name, "remote_only");
        // ...and with no cache, the old honest no-op.
        assert!(matches!(enumerate(&p, ENUM_CAP, None), Enumerated::Nothing));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_and_missing_context_no_op() {
        assert!(matches!(
            start(&Family::Remote, Some("/home/z"), None, "cd x", 4, ENUM_CAP, None),
            Start::None
        ));
        // Nonexistent dir / no cwd for a relative token: silent no-op.
        assert!(matches!(
            start(&pwsh(), Some(r"C:\definitely\not\a\dir\xyz"), None, "cd x", 4, ENUM_CAP, None),
            Start::None
        ));
        assert!(matches!(
            start(&pwsh(), None, None, "cd x", 4, ENUM_CAP, None),
            Start::None
        ));
    }
}
