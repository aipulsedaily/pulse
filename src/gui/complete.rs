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
//!    case-SENSITIVE; Ssh = NO local view of the remote fs — Tab is a
//!    silent no-op (never spaces).
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
    /// No local view of the remote fs: Tab no-ops, silently.
    Ssh,
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
        ShellFamily::Ssh { .. } => Family::Ssh,
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
        Family::Wsl { .. } | Family::Ssh => &['\'', '"'],
    }
}

fn bs_escapes(fam: &Family) -> bool {
    matches!(fam, Family::Wsl { .. } | Family::Ssh)
}

/// Split the draft on unquoted whitespace (family quote/escape rules; an
/// unterminated quote runs to the end — the common mid-edit state) and
/// return the token CONTAINING the caret (start < caret ≤ end), else an
/// empty token AT the caret (readline semantics: `cd |` completes the cwd
/// with an empty stem). Everything outside the returned range is preserved
/// byte-exact by the caller.
pub(crate) fn token_at(fam: &Family, s: &str, caret: usize) -> Tok {
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
    toks.into_iter()
        .find(|t| t.start < caret && caret <= t.end)
        .unwrap_or(Tok {
            start: caret,
            end: caret,
            value: String::new(),
        })
}

// ───────────────────────── path resolution ─────────────────────────

/// Everything needed to enumerate + render candidates for one token.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    /// Local directory to enumerate (may be UNC — then budget-threaded).
    pub fs_dir: PathBuf,
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
        Family::Ssh => None,
        Family::Wsl { distro } => plan_wsl(cwd, distro.as_deref(), value),
        _ => plan_win(fam, cwd, home, value),
    }
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
            fs_dir: PathBuf::from(format!("{value}\\")),
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
        fs_dir,
        prefix: prefix.to_string(),
        render_parent,
        sep,
        ci: true,
        posix_hidden: false,
    })
}

fn plan_wsl(cwd: Option<&str>, distro: Option<&str>, value: &str) -> Option<Plan> {
    if value.starts_with('~') {
        // The distro user's home isn't knowable from the GUI — no-op
        // (never-guess; a wrong home would complete garbage).
        return None;
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
    let fs_dir = posix_to_local(&posix_parent, distro)?;
    Some(Plan {
        fs_dir,
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
        Family::Wsl { .. } | Family::Ssh => {
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
) -> Start {
    let tok = token_at(fam, draft, caret.min(draft.len()));
    let Some(plan) = plan(fam, cwd, home, &tok.value) else {
        return Start::None;
    };
    let Some(out) = enum_budgeted(
        plan.fs_dir.clone(),
        plan.prefix.clone(),
        plan.ci,
        plan.posix_hidden,
        cap,
    ) else {
        return Start::None;
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
        assert_eq!(p.fs_dir, PathBuf::from(r"C:\proj").join("../"));
        assert_eq!((p.render_parent.as_str(), p.sep, p.ci), ("../", '/', true));
        // Relative subdir, backslash style.
        let p = plan(&pwsh(), cwd, None, r"src\ma").unwrap();
        assert_eq!(p.fs_dir, PathBuf::from(r"C:\proj").join(r"src\"));
        assert_eq!((p.prefix.as_str(), p.sep), ("ma", '\\'));
        // Absolute.
        let p = plan(&pwsh(), None, None, r"C:\Users\za").unwrap();
        assert_eq!(p.fs_dir, PathBuf::from(r"C:\Users\"));
        assert_eq!(p.prefix, "za");
        // Bare drive completes the ROOT (never drive-relative).
        let p = plan(&pwsh(), None, None, "C:").unwrap();
        assert_eq!(p.fs_dir, PathBuf::from(r"C:\"));
        assert_eq!((p.prefix.as_str(), p.render_parent.as_str()), ("", r"C:\"));
        // Root-relative anchors at the cwd's drive.
        let p = plan(&pwsh(), cwd, None, r"\tools\x").unwrap();
        assert_eq!(p.fs_dir, PathBuf::from(r"C:\tools\"));
        // ~: pwsh renders it back as typed; cmd expands (no ~ in cmd).
        let home = Some(r"C:\Users\z");
        let p = plan(&pwsh(), None, home, "~/Doc").unwrap();
        assert_eq!(p.fs_dir, PathBuf::from(r"C:\Users\z/"));
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
            p.fs_dir,
            PathBuf::from(r"\\wsl.localhost\Ubuntu-24.04\home\z\..\")
        );
        assert_eq!((p.render_parent.as_str(), p.sep), ("../", '/'));
        assert!(!p.ci);
        assert!(p.posix_hidden);
        // /mnt token → drive-letter enumeration (local, no UNC budget).
        let p = plan(&wsl(d), Some("/home/z"), None, "/mnt/c/Us").unwrap();
        assert_eq!(p.fs_dir, PathBuf::from(r"C:\"));
        assert_eq!(p.prefix, "Us");
        // Windows-shaped pre-first-cd cwd translates through /mnt.
        let p = plan(&wsl(d), Some(r"C:\proj"), None, "src/").unwrap();
        assert_eq!(p.fs_dir, PathBuf::from(r"C:\proj\src\"));
        // Default distro: /mnt maps, distro-fs paths honestly no-op.
        let p = plan(&wsl(None), Some("/mnt/c/x"), None, "y/").unwrap();
        assert_eq!(p.fs_dir, PathBuf::from(r"C:\x\y\"));
        assert_eq!(plan(&wsl(None), Some("/home/z"), None, "y/"), None);
        // ~ no-ops on WSL (home unknowable — never guess).
        assert_eq!(plan(&wsl(d), Some("/home/z"), None, "~/x"), None);
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
        match start(&pwsh(), Some(cwd), None, "cd a", 4, 3) {
            Start::Edit { draft, caret } => {
                assert_eq!(draft, "cd aaa");
                assert_eq!(caret, 6);
            }
            _ => panic!("expected common-prefix Edit"),
        }
        // No progress beyond the typed prefix ⇒ honest no-op.
        assert!(matches!(
            start(&pwsh(), Some(cwd), None, "cd aaa", 6, 3),
            Start::None
        ));
        // Under the cap the same dir cycles normally.
        assert!(matches!(
            start(&pwsh(), Some(cwd), None, "cd a", 4, ENUM_CAP),
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
        let Start::Cycle(mut c) = start(&pwsh(), Some(cwd), None, "cd ", 3, ENUM_CAP) else {
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
        let Start::Cycle(mut c) = start(&pwsh(), Some(cwd), None, "cd ", 3, ENUM_CAP) else {
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
        let Start::Edit { draft, caret } = start(&pwsh(), Some(cwd), None, "cd sr", 5, ENUM_CAP)
        else {
            panic!("expected single-candidate Edit");
        };
        assert_eq!(draft, r"cd src\");
        assert_eq!(caret, 7);
        // The NEXT Tab re-plans from the completed token — descends.
        let Start::Edit { draft, .. } = start(&pwsh(), Some(cwd), None, &draft, 7, ENUM_CAP)
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
        let Start::Edit { draft, caret } = start(&pwsh(), Some(cwd), None, "cd a", 4, ENUM_CAP)
        else {
            panic!();
        };
        assert_eq!(draft, r"cd 'a dir\'");
        // …and the quoted token re-tokenizes for the next Tab: descend into
        // it, meeting a name with a quote (pwsh doubles it).
        let Start::Edit { draft, .. } =
            start(&pwsh(), Some(cwd), None, &draft, caret, ENUM_CAP)
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
        let Start::Cycle(mut c) = start(&Family::Cmd, Some(cwd), None, "cd p", 4, ENUM_CAP)
        else {
            panic!();
        };
        assert_eq!(c.step(1).0, "cd plain\\");
        assert_eq!(c.step(1).0, "cd \"Program Files\\\"");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_and_missing_context_no_op() {
        assert!(matches!(
            start(&Family::Ssh, Some("/home/z"), None, "cd x", 4, ENUM_CAP),
            Start::None
        ));
        // Nonexistent dir / no cwd for a relative token: silent no-op.
        assert!(matches!(
            start(&pwsh(), Some(r"C:\definitely\not\a\dir\xyz"), None, "cd x", 4, ENUM_CAP),
            Start::None
        ));
        assert!(matches!(
            start(&pwsh(), None, None, "cd x", 4, ENUM_CAP),
            Start::None
        ));
    }
}
