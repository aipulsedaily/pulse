//! Remote CLI-resume probes (docs/remote-cli-resume-spec.md): a bare CLI
//! launched inside an ssh terminal (`ssh host`, `cd somewhere`, `claude`)
//! becomes resumable across kill / sleep / reboot / link-death by
//! correlating the remote session id through READ-ONLY sftp probes of the
//! remote CLI store (`pwd` + `-ls -lt` only — no writes, ever; inv. 1).
//!
//! Design pillars (spec §1):
//! - D1 skew-immunity: correlation = SNAPSHOT-DIFF (names + sizes + the
//!   server-attr `-t` sort), never clock math — ls date columns are never
//!   read (§12.3: the date FORM flips on sub-second clock skew).
//! - D2 two connections per CLI lifecycle max: one at bare-CLI block open
//!   (the snapshot leg, M0), one at the next restore-class launch (the
//!   correlate leg, M3/M4) — block close needs NO probe (inner_cli clears
//!   there by design) and sftp is a fresh connection that outlives the link.
//! - D3 the snapshot persists as a sidecar `probes\<id>.json` (atomic
//!   tmp+rename, blocks-sidecar discipline) so power loss mid-claude keeps
//!   the basis.
//! - Never guess: `|C| == 1` ⇒ Correlated; the ONE carve-out is R-NEWEST
//!   (§5.3, claude rotation) behind its full precondition set. Everything
//!   else stays Ambiguous → shell restored + preface candidates (§6.4).
//!
//! Event-driven only (M0/M3/M4/M5): no timers, no per-prompt work, no attach
//! work — an idle ssh terminal costs zero. All failures are silent-to-GUI
//! (`[probe]` daemon.log lines); the only user-visible surface is the
//! preface info line that already existed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::{claude_project_dir_name, CliConfidence, InnerCli, TermStatus, TerminalMeta};

// ─────────────────────────── §2.1 store descriptors ───────────────────────────

/// How an adapter's session store looks from the remote home over sftp.
pub struct RemoteStore {
    pub adapter: &'static str,
    /// D7 gate: remote correlation enabled. claude=true in v1; codex/copilot
    /// ship false (flip after ONE observed real-Linux-host store write each,
    /// §11 Q3); argv-Explicit resume works for every adapter regardless.
    pub remote: bool,
    /// The store is scoped to the block's cwd (claude's per-project dirs) —
    /// the §5.2 exactly-one no-snapshot fallback only exists for these ("the
    /// only session ever run there" is an identity; a lone GLOBAL entry
    /// proves nothing about THIS terminal). [One field beyond the spec's
    /// §2.1 struct — it encodes "claude only" without name-matching.]
    pub cwd_scoped: bool,
    /// Home-relative dirs to list, from the block's remote posix cwd.
    pub dirs: fn(cwd: &Path) -> Vec<String>,
    /// Store-shaped entry → resume token (also the entry filter).
    pub token_of: fn(name: &str, is_dir: bool) -> Option<String>,
    /// Rotation semantics (claude /clear) — enables R-NEWEST (§5.3).
    pub rotation: bool,
}

pub const STORES: &[RemoteStore] = &[
    RemoteStore {
        adapter: "claude",
        remote: true,
        cwd_scoped: true,
        dirs: claude_dirs,
        token_of: claude_token,
        rotation: true,
    },
    RemoteStore {
        adapter: "codex",
        remote: false,
        cwd_scoped: false,
        dirs: codex_dirs,
        token_of: codex_token,
        rotation: false,
    },
    RemoteStore {
        adapter: "copilot",
        remote: false,
        cwd_scoped: false,
        dirs: copilot_dirs,
        token_of: copilot_token,
        rotation: false,
    },
];

pub fn store_for(adapter: &str) -> Option<&'static RemoteStore> {
    STORES.iter().find(|s| s.adapter == adapter)
}

/// claude: one per-cwd dir, munged exactly like the local store (D6 — the
/// non-alnum→`-` rule matches Claude Code's own on both OSes).
fn claude_dirs(cwd: &Path) -> Vec<String> {
    vec![format!(
        ".claude/projects/{}",
        claude_project_dir_name(cwd)
    )]
}

fn claude_token(name: &str, is_dir: bool) -> Option<String> {
    if is_dir {
        return None;
    }
    let stem = name.strip_suffix(".jsonl")?;
    Uuid::parse_str(stem).ok().map(|u| u.to_string())
}

/// codex: GLOBAL date-sharded store — list local-date −1/0/+1 so the remote
/// clock's date shard is inside the window whatever the skew (§2 table).
fn codex_dirs(_cwd: &Path) -> Vec<String> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    codex_date_dirs(secs)
}

fn codex_date_dirs(now_unix_secs: i64) -> Vec<String> {
    let days = now_unix_secs.div_euclid(86_400);
    [-1i64, 0, 1]
        .iter()
        .map(|off| {
            let (y, m, d) = civil_from_days(days + off);
            format!(".codex/sessions/{y:04}/{m:02}/{d:02}")
        })
        .collect()
}

/// Days-since-unix-epoch → (year, month, day), Howard Hinnant's civil
/// algorithm (no chrono dependency for three directory names).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn codex_token(name: &str, is_dir: bool) -> Option<String> {
    if is_dir {
        return None;
    }
    let stem = name.strip_suffix(".jsonl")?;
    if !stem.starts_with("rollout-") {
        return None;
    }
    super::tracker::trailing_uuid(stem)
}

/// copilot: per-session DIRS named by id under a global root (§12.2: the
/// `d` mode char is the discriminator).
fn copilot_dirs(_cwd: &Path) -> Vec<String> {
    vec![".copilot/session-state".into()]
}

fn copilot_token(name: &str, is_dir: bool) -> Option<String> {
    if !is_dir {
        return None;
    }
    Uuid::parse_str(name).ok().map(|u| u.to_string())
}

// ─────────────────────────── listings ───────────────────────────

/// One store entry as listed (name already dir-prefix-stripped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Per-dir listings, `ls -lt` order preserved (newest-first, server-attr
/// sort — the ONLY time-shaped input this module consumes).
pub type Listings = Vec<(String, Vec<Entry>)>;

/// Section a probe connection's stdout into per-dir listings. Sectioning
/// rides the batch-mode command echoes (`sftp> -ls -lt <dir>` — sftp echoes
/// every batch command even under `-q`); entry names arrive in EITHER of
/// two real shapes, both captured live:
/// - full requested-path prefix (`.claude/projects/<munged>/<f>`): servers
///   with the users-groups-by-id extension (OpenSSH 8.7+ — the WSL staging
///   shape), where the CLIENT formats the line;
/// - BARE names: older sshds (the user's real host), where the client
///   prints the server's own longname verbatim.
///
/// The prefix is stripped when present; a residual `/` means a stray
/// full-path line from some other dir — dropped, never misfiled. In-dir
/// order (= `-t` order) is line order. A missing dir prints only stderr
/// (`Can't ls: … not found`), exit stays 0 — "store absent" is a NORMAL
/// empty listing.
pub fn split_listings(stdout: &str, dirs: &[String]) -> Listings {
    let mut out: Listings = dirs.iter().map(|d| (d.clone(), Vec::new())).collect();
    let mut cur: Option<usize> = None;
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("sftp>") {
            // A command echo: the section is the dir named by its last
            // token (store dirs are munged/dated names — never spaces).
            let tok = line.rsplit(' ').next().unwrap_or("");
            cur = dirs.iter().position(|d| d == tok);
            continue;
        }
        let Some(ci) = cur else { continue };
        for (name, size, is_dir) in crate::ssh_transport::parse_ls_l_full(line) {
            let pfx = format!("{}/", out[ci].0);
            let name = name.strip_prefix(&pfx).unwrap_or(&name);
            if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                continue;
            }
            out[ci].1.push(Entry {
                name: name.to_string(),
                size,
                is_dir,
            });
        }
    }
    out
}

// ─────────────────────────── D3 sidecar ───────────────────────────

/// The M0 snapshot, persisted beside the blocks sidecars. Listing entries
/// are (name, size) in `ls -lt` order; is_dir is not stored (the diff only
/// ever consults S by name).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeSidecar {
    pub adapter: String,
    /// Remote posix cwd, verbatim.
    pub cwd: String,
    /// (epoch, start_off) of the block whose exec set inner_cli — the
    /// validity join back to the rehydrated BlockStore.
    pub block_key: (u32, u64),
    pub taken_ms: u64,
    pub listings: Vec<(String, Vec<(String, u64)>)>,
    /// A dir exceeded the 500-entry cap: the store is a haystack, the diff
    /// can never satisfy exactly-one — M3 goes straight to Ambiguous with no
    /// candidates (§4.5).
    #[serde(default)]
    pub overflow: bool,
}

fn sidecar_path_in(dir: &Path, id: Uuid) -> PathBuf {
    dir.join(format!("{id}.json"))
}

pub fn save_sidecar_in(dir: &Path, id: Uuid, s: &ProbeSidecar) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = sidecar_path_in(dir, id);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(s).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
}

pub fn load_sidecar_in(dir: &Path, id: Uuid) -> Option<ProbeSidecar> {
    let bytes = std::fs::read(sidecar_path_in(dir, id)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn save_sidecar(id: Uuid, s: &ProbeSidecar) -> std::io::Result<()> {
    save_sidecar_in(&crate::state::data_probes_dir(), id, s)
}

pub fn load_sidecar(id: Uuid) -> Option<ProbeSidecar> {
    load_sidecar_in(&crate::state::data_probes_dir(), id)
}

pub fn sidecar_exists(id: Uuid) -> bool {
    sidecar_path_in(&crate::state::data_probes_dir(), id).is_file()
}

/// M1 (block close) / DeleteTerminal: the basis is obsolete.
pub fn delete_sidecar(id: Uuid) {
    let _ = std::fs::remove_file(sidecar_path_in(&crate::state::data_probes_dir(), id));
}

/// Sidecar validity — the pure half (§5.1 gate): same adapter, same remote
/// cwd. The block_key half (`block_key_confirms`) needs the rehydrated
/// BlockStore and joins in `sidecar_valid`.
pub fn sidecar_matches(s: &ProbeSidecar, cli: &InnerCli) -> bool {
    s.adapter == cli.adapter && s.cwd == cli.cwd.to_string_lossy()
}

/// The sidecar's block_key must name a record whose command still parses to
/// the same adapter (rec.cmd re-run through analyze_cmdline) — a daemon
/// restart between M0 and M3 re-joins through the persisted blocks sidecar.
pub fn block_key_confirms(rec_cmd: Option<&str>, cli: &InnerCli) -> bool {
    rec_cmd.is_some_and(|cmd| {
        super::tracker::analyze_cmdline(cmd, &cli.cwd).is_some_and(|i| i.adapter == cli.adapter)
    })
}

fn sidecar_valid(core: &super::Core, id: Uuid, s: &ProbeSidecar, cli: &InnerCli) -> bool {
    if !sidecar_matches(s, cli) {
        return false;
    }
    let rec_cmd = {
        let mut map = core.blocks.lock();
        let store = map
            .entry(id)
            .or_insert_with(|| super::blocks::BlockStore::load(id));
        store
            .recs
            .iter()
            .find(|r| (r.epoch, r.start_off) == s.block_key)
            .map(|r| r.cmd.clone())
    };
    block_key_confirms(rec_cmd.as_deref(), cli)
}

// ─────────────────────────── §5 correlation rules ───────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Correlated(String),
    /// Definitive ambiguity; candidates newest-first (may be empty — the CLI
    /// persisted nothing visible / overflow / global-store fallback).
    Ambiguous(Vec<String>),
}

/// §5.1: C = store-shaped entries of L that are NEW names (born during the
/// block — set membership, not timestamps) or SIZE-CHANGED (written during
/// the block; ≠ not > — catches both the open-probe racing the store
/// creation and `--continue` appends). Tokens in `ls -lt` order, deduped.
pub fn diff_candidates(
    store: &RemoteStore,
    snapshot: &[(String, Vec<(String, u64)>)],
    current: &Listings,
) -> Vec<String> {
    let mut s: HashMap<(&str, &str), u64> = HashMap::new();
    for (dir, entries) in snapshot {
        for (n, sz) in entries {
            s.insert((dir.as_str(), n.as_str()), *sz);
        }
    }
    let mut out: Vec<String> = Vec::new();
    for (dir, entries) in current {
        for e in entries {
            let Some(tok) = (store.token_of)(&e.name, e.is_dir) else {
                continue;
            };
            let changed = match s.get(&(dir.as_str(), e.name.as_str())) {
                None => true,
                Some(sz) => *sz != e.size,
            };
            if changed && !out.contains(&tok) {
                out.push(tok);
            }
        }
    }
    out
}

/// Every store-shaped token in the listings, `ls -lt` order (the §5.2
/// fallback universe and the D10 candidates source).
pub fn all_candidates(store: &RemoteStore, current: &Listings) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_, entries) in current {
        for e in entries {
            if let Some(tok) = (store.token_of)(&e.name, e.is_dir) {
                if !out.contains(&tok) {
                    out.push(tok);
                }
            }
        }
    }
    out
}

/// The §5 verdict. `snapshot` = a VALIDITY-GATED sidecar (None ⇒ §5.2
/// fallback); `sibling_open` = another TC terminal holds a same-adapter CLI
/// at the same (destination, cwd) — flips R-NEWEST off (§5.3 precondition 3).
pub fn correlate(
    store: &RemoteStore,
    snapshot: Option<&ProbeSidecar>,
    current: &Listings,
    overflow_now: bool,
    sibling_open: bool,
) -> Verdict {
    // Overflow (either side): a haystack store can never satisfy exactly-one
    // and a truncated listing can't prove a diff — straight to Ambiguous
    // with no candidates (§4.5).
    if overflow_now || snapshot.is_some_and(|s| s.overflow) {
        return Verdict::Ambiguous(Vec::new());
    }
    match snapshot {
        Some(s) => {
            let c = diff_candidates(store, &s.listings, current);
            match c.len() {
                1 => Verdict::Correlated(c[0].clone()),
                0 => Verdict::Ambiguous(Vec::new()),
                // §5.3 R-NEWEST: rotation store + VALID snapshot diff (this
                // branch) + no sibling ⇒ the ls -lt-newest member (total
                // order, precondition 4) is the live session at death —
                // /clear chains abandon their pre-clear ids by claude's own
                // semantics. Precedent: the LOCAL mtime-newest rule.
                _ if store.rotation && !sibling_open => Verdict::Correlated(c[0].clone()),
                _ => Verdict::Ambiguous(c),
            }
        }
        None => {
            // §5.2 no-snapshot fallback: exactly ONE store-shaped entry in a
            // CWD-SCOPED store is an identity ("the only session ever run
            // there" — the local cands.len()==1 branch). Global stores get
            // NO fallback; anything else is Ambiguous with candidates (D10).
            let all = all_candidates(store, current);
            if store.cwd_scoped && all.len() == 1 {
                Verdict::Correlated(all[0].clone())
            } else {
                Verdict::Ambiguous(all)
            }
        }
    }
}

/// §6.5 remote re-pin belt (M5, claude only), mirroring the local
/// `claude_repin_candidate` decision shape with the evidence source swapped
/// from created()/modified() to the snapshot diff:
/// - pinned ∈ C (its file was written during the run) ⇒ the pin is alive,
///   keep (None);
/// - pinned ∉ C ∧ |C| == 1 ⇒ the run rotated onto C's member, re-pin;
/// - |C| ≥ 2 ⇒ abstain (never guess).
pub fn repin_candidate(pinned: &str, c: &[String]) -> Option<String> {
    if c.iter().any(|t| t == pinned) {
        return None;
    }
    match c {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// §6.4 preface text for a definitive-Ambiguous verdict: up to 5 candidates
/// NEWEST-FIRST as full paste-able resume commands. Zero candidates keeps a
/// single informational line (no candidates claim).
pub fn ambiguous_notice(adapter: &str, cwd: &str, cands: &[String]) -> String {
    if cands.is_empty() {
        return format!(
            "── a {adapter} session was running here but no sessions were found in {cwd} — resume it manually ──"
        );
    }
    let mut out = format!("── multiple {adapter} sessions found in {cwd} — resume one manually ──");
    for (i, tok) in cands.iter().take(5).enumerate() {
        let cmd = super::tracker::restore_trailing(adapter, Some(tok))
            .unwrap_or_else(|| format!("{adapter} --resume {tok}"));
        out.push('\n');
        if i == 0 {
            out.push_str(&format!("   {cmd}   (newest)"));
        } else {
            out.push_str(&format!("   {cmd}"));
        }
    }
    out
}

// ─────────────────────────── runtime bookkeeping ───────────────────────────

/// §4.5/§4.6 cost discipline: per-terminal 30s listing cooldown + the
/// password-auth dead cache. Runtime-only (a daemon restart retries once —
/// the desired "keys were fixed while we were down" behavior). Held by Core
/// behind an Arc so probe worker threads outlive no lock.
pub struct Runtime {
    auth_dead: Mutex<HashSet<Uuid>>,
    /// Mirrors auth_dead.len() so hook-path clears cost one relaxed load.
    auth_dead_count: AtomicUsize,
    cache: Mutex<HashMap<(Uuid, Leg), Cached>>,
}

/// Which probe moment produced a cached listing. LOAD-BEARING: the legs
/// must NEVER share cache entries — a correlate leg reusing the M0 snapshot
/// listing would diff the basis against itself (C = ∅ ⇒ wrong definitive
/// Ambiguous ⇒ inner_cli cleared) on any kill→reconnect inside the cooldown
/// window. Snapshot-leg reuse = rapid claude-exit-claude; correlate-leg
/// reuse = rapid reconnect attempts (the two §4 cooldown intents).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Leg {
    Snapshot,
    Correlate,
}

struct Cached {
    taken: Instant,
    adapter: String,
    cwd: PathBuf,
    listings: Listings,
    overflow: bool,
}

/// §4 M0 cooldown: a listing this fresh for the same (terminal, adapter,
/// cwd) is reused instead of reconnecting (rapid claude-exit-claude; rapid
/// reconnect attempts).
const LISTING_COOLDOWN: Duration = Duration::from_secs(30);

/// §3.2 probe deadline: ConnectTimeout=10 covers connect; the watchdog
/// covers a wedged established link. Probes must die fast — the session's
/// softened 30/4 keepalives are NOT applied here (sftp_args ships 15/3).
const PROBE_DEADLINE: Duration = Duration::from_secs(25);

/// §4.5 sidecar bound: beyond this the store is a haystack and exactly-one
/// can never fire.
const DIR_ENTRY_CAP: usize = 500;

impl Runtime {
    pub fn new() -> Self {
        Self {
            auth_dead: Mutex::new(HashSet::new()),
            auth_dead_count: AtomicUsize::new(0),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn auth_dead(&self, id: Uuid) -> bool {
        self.auth_dead_count.load(Ordering::Relaxed) > 0 && self.auth_dead.lock().contains(&id)
    }

    pub fn mark_auth_dead(&self, id: Uuid) {
        let mut set = self.auth_dead.lock();
        if set.insert(id) {
            self.auth_dead_count.store(set.len(), Ordering::Relaxed);
        }
    }

    /// §4.6 clear-point: a later spawn of the terminal reached `hooks_live`
    /// (token-checked hook) — a freshly-spawned link that hooked without
    /// anyone typing proves non-interactive auth. Called from the hook path,
    /// so it must stay one relaxed load when the cache is empty.
    pub fn clear_auth_dead(&self, id: Uuid) {
        if self.auth_dead_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut set = self.auth_dead.lock();
        if set.remove(&id) {
            self.auth_dead_count.store(set.len(), Ordering::Relaxed);
            log::info!("[probe] {id}: auth-dead cleared (hooks proved non-interactive auth)");
        }
    }

    fn cache_get(&self, id: Uuid, leg: Leg, adapter: &str, cwd: &Path) -> Option<(Listings, bool)> {
        let cache = self.cache.lock();
        let c = cache.get(&(id, leg))?;
        (c.taken.elapsed() < LISTING_COOLDOWN && c.adapter == adapter && c.cwd == cwd)
            .then(|| (c.listings.clone(), c.overflow))
    }

    fn cache_put(
        &self,
        id: Uuid,
        leg: Leg,
        adapter: &str,
        cwd: &Path,
        listings: Listings,
        overflow: bool,
    ) {
        self.cache.lock().insert(
            (id, leg),
            Cached {
                taken: Instant::now(),
                adapter: adapter.to_string(),
                cwd: cwd.to_path_buf(),
                listings,
                overflow,
            },
        );
    }
}

// ─────────────────────────── §3.2 probe leg (one connection) ───────────────────────────

#[derive(Debug)]
pub enum ProbeErr {
    /// BatchMode hit an interactive-auth wall (`Permission denied (…`) —
    /// feeds the §4.6 cache.
    AuthRequired,
    /// Everything else: transient transport failure (retry per moment).
    Transport(String),
}

static BATCH_NONCE: AtomicUsize = AtomicUsize::new(0);

/// ONE sftp connection: `pwd` (connection sanity anchor) + one
/// ignore-prefixed `-ls -lt` per store dir. READ-ONLY by construction.
/// Argv = the terminal's own persisted flags through the shared translation
/// (identity/config/aliases resolve exactly like the session), or the
/// `TC_SSH_PROBE_TRANSPORT` stand-in (D12, data_dir_overridden-gated).
fn run_probe_conn(
    program: &str,
    args: &[String],
    dirs: &[String],
) -> Result<(Listings, bool), ProbeErr> {
    let sftp = crate::ssh_transport::resolve_sftp(program)
        .map_err(|looked| ProbeErr::Transport(format!("sftp.exe not found (looked beside {looked})")))?;
    let mut batch = String::from("pwd\n");
    for d in dirs {
        // Unquoted: store dirs are munged/dated names, [A-Za-z0-9./-] only
        // (batch-safe by construction; ls-arg quoting is unproven, -put
        // quoting is the proven case).
        batch.push_str(&format!("-ls -lt {d}\n"));
    }
    let bpath = std::env::temp_dir().join(format!(
        "tc-probe-{}-{}.batch",
        std::process::id(),
        BATCH_NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    crate::ssh_transport::write_batch(&bpath, &batch)
        .map_err(|e| ProbeErr::Transport(format!("batch write: {e}")))?;
    let bstr = bpath.to_string_lossy().into_owned();
    let argv = crate::ssh_transport::install_argv(args, &bstr);
    let out = crate::ssh_transport::run_sftp(&sftp, &argv, Some(PROBE_DEADLINE), None);
    let _ = std::fs::remove_file(&bpath);
    let out = out.map_err(|e| ProbeErr::Transport(format!("spawn: {e}")))?;
    match out.status.code() {
        Some(0) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut listings = split_listings(&stdout, dirs);
            let mut overflow = false;
            for (dir, entries) in &mut listings {
                if entries.len() > DIR_ENTRY_CAP {
                    log::info!(
                        "[probe] listing overflow: {dir} has {} entries (cap {DIR_ENTRY_CAP})",
                        entries.len()
                    );
                    entries.truncate(DIR_ENTRY_CAP);
                    overflow = true;
                }
            }
            Ok((listings, overflow))
        }
        Some(255) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            match crate::ssh_transport::classify_conn(&stderr) {
                crate::ssh_transport::ConnErr::Auth => Err(ProbeErr::AuthRequired),
                e => Err(ProbeErr::Transport(format!("{e:?}"))),
            }
        }
        code => Err(ProbeErr::Transport(format!(
            "sftp exited with {code:?} (watchdog kill or batch abort)"
        ))),
    }
}

/// Cooldown-aware listing (the §4 dedupe: M0 after a fresh M3, rapid
/// reconnect attempts, claude-exit-claude). The adapter identity rides
/// `store.adapter` (every caller resolves the store from the adapter).
fn listing_for(
    rt: &Runtime,
    id: Uuid,
    leg: Leg,
    cwd: &Path,
    store: &RemoteStore,
    program: &str,
    args: &[String],
) -> Result<(Listings, bool), ProbeErr> {
    if let Some(hit) = rt.cache_get(id, leg, store.adapter, cwd) {
        log::info!("[probe] {id}: reusing {leg:?}-leg listing within cooldown");
        return Ok(hit);
    }
    let dirs = (store.dirs)(cwd);
    let t0 = Instant::now();
    let out = run_probe_conn(program, args, &dirs)?;
    log::info!(
        "[probe] {id}: listed {} dir(s) in {}ms ({} entries)",
        dirs.len(),
        t0.elapsed().as_millis(),
        out.0.iter().map(|(_, e)| e.len()).sum::<usize>()
    );
    rt.cache_put(id, leg, store.adapter, cwd, out.0.clone(), out.1);
    Ok(out)
}

// ─────────────────────────── M0 snapshot leg ───────────────────────────

/// M0 trigger (called from track_hook_exec on the ingest path): spawn the
/// snapshot worker when the exec is a bare (token-less) launch of a
/// remote-correlatable adapter over ssh. The thread owns everything it
/// needs — no Core reference, no locks held across the connection.
pub(super) fn spawn_snapshot_leg(
    core: &super::Core,
    id: Uuid,
    inner: &InnerCli,
    program: String,
    args: Vec<String>,
    block_key: (u32, u64),
) {
    if inner.resume_token.is_some() {
        return; // M0 is the BARE-launch snapshot only
    }
    let Some(store) = store_for(&inner.adapter) else {
        return;
    };
    if !store.remote {
        return;
    }
    if store.cwd_scoped && !inner.cwd.to_string_lossy().starts_with('/') {
        return; // a non-posix cwd can't address the remote store
    }
    if core.probe_rt.auth_dead(id) {
        log::info!("[probe] {id}: M0 skipped (auth-dead cache)");
        return;
    }
    let rt = core.probe_rt.clone();
    let adapter = inner.adapter.clone();
    let cwd = inner.cwd.clone();
    // C7: a failed spawn silently forfeits this run's correlation basis —
    // the module contract says every failure leaves a [probe] line.
    if let Err(e) = std::thread::Builder::new()
        .name(format!("probe-snap-{id}"))
        .spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                snapshot_leg(&rt, id, &adapter, &cwd, block_key, &program, &args);
            }));
        })
    {
        log::warn!(
            "[probe] {id}: M0 snapshot thread spawn failed ({e}) — restore will degrade to Ambiguous"
        );
    }
}

fn snapshot_leg(
    rt: &Runtime,
    id: Uuid,
    adapter: &str,
    cwd: &Path,
    block_key: (u32, u64),
    program: &str,
    args: &[String],
) {
    let Some(store) = store_for(adapter) else {
        return;
    };
    match listing_for(rt, id, Leg::Snapshot, cwd, store, program, args) {
        Ok((listings, overflow)) => {
            let sidecar = ProbeSidecar {
                adapter: adapter.to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
                block_key,
                taken_ms: super::now_ms(),
                listings: listings
                    .into_iter()
                    .map(|(dir, entries)| {
                        (
                            dir,
                            entries.into_iter().map(|e| (e.name, e.size)).collect(),
                        )
                    })
                    .collect(),
                overflow,
            };
            match save_sidecar(id, &sidecar) {
                Ok(()) => log::info!("[probe] {id}: M0 snapshot saved ({adapter})"),
                Err(e) => log::warn!("[probe] {id}: M0 sidecar write failed: {e}"),
            }
        }
        Err(ProbeErr::AuthRequired) => {
            rt.mark_auth_dead(id);
            log::info!(
                "[probe] {id}: M0 auth requires interaction — probes off until a hooked spawn proves keys"
            );
        }
        Err(ProbeErr::Transport(e)) => {
            log::info!("[probe] {id}: M0 snapshot failed ({e}) — M3 will use the §5.2 fallback");
        }
    }
}

// ─────────────────────────── M3/M4/M5 restore legs ───────────────────────────

/// Cheap conn-thread predicate (§6.2 D9): is a probe DUE for this launch?
/// True ⇒ the caller spawns `probe-launch-<id>` instead of probing on the
/// conn handler thread; the probe itself runs inside launch() via
/// `upgrade_before_launch` either way.
pub(super) fn probe_due(core: &super::Core, id: Uuid) -> bool {
    let Some(t) = core.state.lock().terminal(id).cloned() else {
        return false;
    };
    if t.status != TermStatus::Dead {
        return false;
    }
    if !matches!(
        crate::state::shell_family(&t.kind, &t.program, &t.args),
        crate::state::ShellFamily::Ssh { .. }
    ) {
        return false;
    }
    if !t.shell_cfg.as_ref().is_none_or(|c| c.remote_hooks) {
        return false;
    }
    let Some(cli) = &t.inner_cli else {
        return false;
    };
    let Some(store) = store_for(&cli.adapter) else {
        return false;
    };
    if !store.remote || core.probe_rt.auth_dead(id) {
        return false;
    }
    if store.cwd_scoped && !cli.cwd.to_string_lossy().starts_with('/') {
        return false;
    }
    let correlate_due =
        matches!(cli.confidence, CliConfidence::Ambiguous) && cli.resume_token.is_none();
    let repin_due = matches!(cli.confidence, CliConfidence::Explicit)
        && cli.resume_token.is_some()
        && store.rotation
        && sidecar_exists(id);
    correlate_due || repin_due
}

/// §5.3 precondition 3: a SIBLING terminal contends the same (ssh
/// destination, remote adapter store, cwd). Evidence is EITHER a present
/// `inner_cli` (set at exec, persisted, cleared when the block closes) OR a
/// present probe SIDECAR for the same adapter+cwd.
///
/// The sidecar is LOAD-BEARING here, not just belt-and-braces: boot-restore
/// lanes launch siblings CONCURRENTLY, and the first sibling to reach a
/// definitive-Ambiguous verdict CLEARS its own inner_cli — so a second
/// sibling checking only `inner_cli` would no longer see it, R-NEWEST would
/// fire, and the two contending terminals would resolve differently by pure
/// restore-order luck (probe ssh_cli_resume's simultaneous variant pinned
/// this). The sidecar survives the concurrent clear (it is deleted only at
/// M1 block-close / DeleteTerminal, never on the Ambiguous path), so it is a
/// stable record that a terminal HELD a claude cli_block on this store.
fn sibling_cli_open(core: &super::Core, id: Uuid, args: &[String], cli: &InnerCli) -> bool {
    let Some(dest) = crate::state::ssh_destination(args) else {
        return true; // unknown destination: fail safe (blocks R-NEWEST)
    };
    let dest = dest.to_string();
    let cli_cwd = cli.cwd.to_string_lossy();
    let siblings: Vec<(Uuid, Option<InnerCli>)> = {
        let state = core.state.lock();
        state
            .terminals
            .iter()
            .filter(|t| {
                t.id != id
                    && matches!(
                        crate::state::shell_family(&t.kind, &t.program, &t.args),
                        crate::state::ShellFamily::Ssh { .. }
                    )
                    && crate::state::ssh_destination(&t.args) == Some(dest.as_str())
            })
            .map(|t| (t.id, t.inner_cli.clone()))
            .collect()
    };
    siblings.iter().any(|(sid, inner)| {
        let live = inner
            .as_ref()
            .is_some_and(|c| c.adapter == cli.adapter && c.cwd == cli.cwd);
        let via_sidecar = load_sidecar(*sid)
            .is_some_and(|s| s.adapter == cli.adapter && s.cwd == cli_cwd);
        live || via_sidecar
    })
}

/// The §6.1 seam, called by launch() (under its LaunchGuard) before the
/// inner_cli restore match — for every entry lane: boot restore, GUI
/// Restore, ctl Wake/Restart, tc restart, wake-from-sleep, auto-reconnect.
/// (a) runs the correlate leg when due (M3/M4), (b) mutates + persists
/// `meta.inner_cli` on Correlated, (c) returns the preface candidates text
/// on definitive-Ambiguous and CLEARS inner_cli (DO-NOT 9 — no retry
/// storms), (d) no-ops on cooldown/auth-dead/not-due; transport failure
/// keeps inner_cli for the next restore-class event. Also the M5 re-pin
/// belt for Explicit-token rotation adapters with a valid sidecar.
pub(super) fn upgrade_before_launch(
    core: &super::Core,
    id: Uuid,
    meta: &mut TerminalMeta,
) -> Option<String> {
    if !matches!(
        crate::state::shell_family(&meta.kind, &meta.program, &meta.args),
        crate::state::ShellFamily::Ssh { .. }
    ) {
        return None;
    }
    if !meta.shell_cfg.as_ref().is_none_or(|c| c.remote_hooks) {
        return None; // D13: remote_hooks is the consent bit — no hooks, no probes
    }
    let cli = meta.inner_cli.clone()?;
    let store = store_for(&cli.adapter)?;
    if !store.remote {
        return None;
    }
    // M5: Explicit-token restore of a rotation adapter (claude) — the re-pin
    // belt piggybacks on the same listing; skipped entirely without a valid
    // sidecar. M3/M4: token-less Ambiguous — the correlate leg.
    let repin_leg = matches!(cli.confidence, CliConfidence::Explicit)
        && cli.resume_token.is_some()
        && store.rotation;
    let correlate_leg =
        matches!(cli.confidence, CliConfidence::Ambiguous) && cli.resume_token.is_none();
    if !repin_leg && !correlate_leg {
        return None;
    }
    if core.probe_rt.auth_dead(id) {
        log::info!("[probe] {id}: skipped (auth-dead cache)");
        return None;
    }
    if store.cwd_scoped && !cli.cwd.to_string_lossy().starts_with('/') {
        return None; // a non-posix cwd can't address the remote store
    }
    let sidecar = load_sidecar(id).filter(|s| sidecar_valid(core, id, s, &cli));
    if repin_leg && sidecar.is_none() {
        return None;
    }
    let (listings, overflow) = match listing_for(
        &core.probe_rt,
        id,
        Leg::Correlate,
        &cli.cwd,
        store,
        &meta.program,
        &meta.args,
    ) {
        Ok(v) => v,
        Err(ProbeErr::AuthRequired) => {
            core.probe_rt.mark_auth_dead(id);
            log::info!(
                "[probe] {id}: auth requires interaction — probes off until a hooked spawn proves keys"
            );
            return None;
        }
        Err(ProbeErr::Transport(e)) => {
            // Keep inner_cli: retry at the next restore-class event; the
            // shell restores with today's shorter ambiguous line (§6.4).
            log::info!("[probe] {id}: correlate transport failed ({e}) — shell-only this time");
            return None;
        }
    };
    if repin_leg {
        let s = sidecar.as_ref().expect("gated above");
        if overflow || s.overflow {
            return None; // no provable diff — keep the pin
        }
        let c = diff_candidates(store, &s.listings, &listings);
        let pinned = cli.resume_token.clone().expect("gated above");
        if let Some(new_tok) = repin_candidate(&pinned, &c) {
            log::info!(
                "terminal {id}: remote {} session re-pinned {pinned} -> {new_tok} (rotated during the previous run)",
                cli.adapter
            );
            let mut inner = cli.clone();
            inner.resume_token = Some(new_tok);
            inner.confidence = CliConfidence::Correlated;
            meta.inner_cli = Some(inner.clone());
            core.set_inner_cli(id, Some(inner));
        }
        return None;
    }
    // Correlate leg (M3/M4).
    let sibling = sibling_cli_open(core, id, &meta.args, &cli);
    match correlate(store, sidecar.as_ref(), &listings, overflow, sibling) {
        Verdict::Correlated(tok) => {
            log::info!(
                "terminal {id}: remote {} session correlated -> {tok}",
                cli.adapter
            );
            let mut inner = cli.clone();
            inner.resume_token = Some(tok);
            inner.confidence = CliConfidence::Correlated;
            meta.inner_cli = Some(inner.clone());
            core.set_inner_cli(id, Some(inner));
            None
        }
        Verdict::Ambiguous(cands) => {
            log::info!(
                "terminal {id}: remote {} correlation definitively ambiguous ({} candidate(s)) — inner_cli cleared",
                cli.adapter,
                cands.len()
            );
            meta.inner_cli = None;
            core.set_inner_cli(id, None);
            Some(ambiguous_notice(
                &cli.adapter,
                &cli.cwd.to_string_lossy(),
                &cands,
            ))
        }
    }
}

// ─────────────────────────── tests (spec §9.1) ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> &'static RemoteStore {
        store_for("claude").unwrap()
    }

    fn entries(v: &[(&str, u64)]) -> Vec<Entry> {
        v.iter()
            .map(|(n, s)| Entry {
                name: n.to_string(),
                size: *s,
                is_dir: false,
            })
            .collect()
    }

    fn listing(dir: &str, v: &[(&str, u64)]) -> Listings {
        vec![(dir.to_string(), entries(v))]
    }

    fn snap(dir: &str, v: &[(&str, u64)]) -> Vec<(String, Vec<(String, u64)>)> {
        vec![(
            dir.to_string(),
            v.iter().map(|(n, s)| (n.to_string(), *s)).collect(),
        )]
    }

    const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    const C: &str = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    const DIR: &str = ".claude/projects/-home-alice-proj";

    fn jl(u: &str) -> String {
        format!("{u}.jsonl")
    }

    // ── store descriptors ──

    #[test]
    fn claude_store_descriptor_golden() {
        let dirs = (claude().dirs)(Path::new("/home/alice/proj"));
        assert_eq!(dirs, vec![".claude/projects/-home-alice-proj".to_string()]);
        // token_of: uuid-stem .jsonl files only.
        assert_eq!((claude().token_of)(&jl(A), false), Some(A.to_string()));
        assert_eq!((claude().token_of)(&jl(A), true), None); // dirs never
        assert_eq!((claude().token_of)("notes.jsonl", false), None);
        assert_eq!((claude().token_of)(&format!("{A}.txt"), false), None);
        assert!(claude().remote && claude().rotation && claude().cwd_scoped);
    }

    #[test]
    fn codex_store_descriptor_golden() {
        let codex = store_for("codex").unwrap();
        assert!(!codex.remote && !codex.rotation && !codex.cwd_scoped);
        // 2026-07-04 12:00:00 UTC = 1783166400.
        assert_eq!(
            codex_date_dirs(1_783_166_400),
            vec![
                ".codex/sessions/2026/07/03".to_string(),
                ".codex/sessions/2026/07/04".to_string(),
                ".codex/sessions/2026/07/05".to_string(),
            ]
        );
        // Month boundary: 2026-07-01 = 1782907200.
        assert_eq!(
            codex_date_dirs(1_782_907_200),
            vec![
                ".codex/sessions/2026/06/30".to_string(),
                ".codex/sessions/2026/07/01".to_string(),
                ".codex/sessions/2026/07/02".to_string(),
            ]
        );
        let stem = format!("rollout-2026-07-04T10-00-00-{A}");
        assert_eq!(
            (codex.token_of)(&format!("{stem}.jsonl"), false),
            Some(A.to_string())
        );
        assert_eq!((codex.token_of)(&format!("{A}.jsonl"), false), None); // not rollout-
        assert_eq!((codex.token_of)(&format!("{stem}.jsonl"), true), None);
    }

    #[test]
    fn copilot_store_descriptor_golden() {
        let cp = store_for("copilot").unwrap();
        assert!(!cp.remote && !cp.rotation && !cp.cwd_scoped);
        assert_eq!(
            (cp.dirs)(Path::new("/anything")),
            vec![".copilot/session-state".to_string()]
        );
        // DIR entries only, uuid-shaped only.
        assert_eq!((cp.token_of)(A, true), Some(A.to_string()));
        assert_eq!((cp.token_of)(A, false), None);
        assert_eq!((cp.token_of)("not-a-uuid", true), None);
    }

    // ── split_listings on the §12.2 shape ──

    #[test]
    fn split_listings_sections_by_prefix_and_keeps_order() {
        let stdout = format!(
            "sftp> pwd\r\nRemote working directory: /tmp/tcprobe-home\r\n\
             sftp> -ls -lt {DIR}\r\n\
             -rw-******    ? alice  alice    5 Jul  4  2026 {DIR}/{}\r\n\
             -rw-******    ? alice  alice   10 Jul  4 15:46 {DIR}/{}\r\n\
             -rw-******    ? alice  alice    1 Jul  4 13:56 {DIR}/{}\r\n\
             sftp> -ls -lt .copilot/session-state\r\n\
             drwx******    ? alice  alice 4096 Jul  4  2026 .copilot/session-state/{C}\r\n",
            jl(C),
            jl(B),
            jl(A),
        );
        let dirs = vec![DIR.to_string(), ".copilot/session-state".to_string()];
        let l = split_listings(&stdout, &dirs);
        assert_eq!(l.len(), 2);
        // ls -lt order preserved: newest (C) first.
        assert_eq!(
            l[0].1,
            entries(&[(&jl(C), 5), (&jl(B), 10), (&jl(A), 1)])
        );
        assert_eq!(l[1].1.len(), 1);
        assert!(l[1].1[0].is_dir);
        assert_eq!(l[1].1[0].name, C);
        // A missing dir yields an empty section, not an error.
        let l = split_listings("sftp> pwd\nRemote working directory: /h\n", &dirs);
        assert!(l[0].1.is_empty() && l[1].1.is_empty());
    }

    /// REAL-HOST capture (2026-07-04, the author's test host), anonymized:
    /// host/username replaced, structure and sizes verbatim.
    /// an sshd without the users-groups-by-id extension makes the client
    /// print SERVER longnames — BARE filenames, real link counts, unmasked
    /// perms. The old prefix-required parse dropped every entry here, so a
    /// probe against this host could never produce a snapshot basis or a
    /// diff candidate (the "ONE recent session yet ambiguous" field bug).
    #[test]
    fn split_listings_real_host_bare_names() {
        let stdout = "sftp> pwd\r\n\
            Remote working directory: /home/alice\r\n\
            sftp> -ls -lt .claude/projects/-home\r\n\
            -rw-------    1 alice     alice        38956 Jul  4 21:37 9049fdee-bc43-4854-bad7-ec7126f345d4.jsonl\r\n\
            -rw-------    1 alice     alice        13093 Jul  4 19:48 10871cff-198d-44b0-a84b-c3cc54983768.jsonl\r\n\
            drwxrwxr-x    2 alice     alice         4096 Jul  4 19:48 memory\r\n\
            sftp> \r\n";
        let dirs = vec![".claude/projects/-home".to_string()];
        let l = split_listings(stdout, &dirs);
        assert_eq!(l.len(), 1);
        assert_eq!(
            l[0].1,
            vec![
                Entry {
                    name: "9049fdee-bc43-4854-bad7-ec7126f345d4.jsonl".into(),
                    size: 38956,
                    is_dir: false,
                },
                Entry {
                    name: "10871cff-198d-44b0-a84b-c3cc54983768.jsonl".into(),
                    size: 13093,
                    is_dir: false,
                },
                Entry {
                    name: "memory".into(),
                    size: 4096,
                    is_dir: true,
                },
            ]
        );
        // …and downstream: the `memory` dir is not store-shaped, the two
        // transcripts are, newest first.
        assert_eq!(
            all_candidates(claude(), &l),
            vec![
                "9049fdee-bc43-4854-bad7-ec7126f345d4".to_string(),
                "10871cff-198d-44b0-a84b-c3cc54983768".to_string(),
            ]
        );
    }

    // ── §5.1 diff rule table ──

    #[test]
    fn diff_new_name_and_size_change() {
        // new name ⇒ candidate (set membership, no timestamps).
        let c = diff_candidates(
            claude(),
            &snap(DIR, &[(&jl(A), 100)]),
            &listing(DIR, &[(&jl(B), 5), (&jl(A), 100)]),
        );
        assert_eq!(c, vec![B.to_string()]);
        // size ≠ (grown OR shrunk) ⇒ candidate (`--continue` appends; the
        // M0-races-store-creation case).
        let c = diff_candidates(
            claude(),
            &snap(DIR, &[(&jl(A), 100)]),
            &listing(DIR, &[(&jl(A), 160)]),
        );
        assert_eq!(c, vec![A.to_string()]);
        // untouched ⇒ not a candidate.
        let c = diff_candidates(
            claude(),
            &snap(DIR, &[(&jl(A), 100)]),
            &listing(DIR, &[(&jl(A), 100)]),
        );
        assert!(c.is_empty());
        // non-store-shaped entries never become candidates.
        let c = diff_candidates(
            claude(),
            &snap(DIR, &[]),
            &listing(DIR, &[("junk.txt", 9)]),
        );
        assert!(c.is_empty());
    }

    #[test]
    fn diff_absent_dir_snapshot_means_all_new() {
        // Snapshot-of-absent-dir = empty S ⇒ every store-shaped entry is new
        // (the dir was born with the CLI's first run).
        let c = diff_candidates(
            claude(),
            &snap(DIR, &[]),
            &listing(DIR, &[(&jl(B), 5), (&jl(A), 1)]),
        );
        assert_eq!(c, vec![B.to_string(), A.to_string()]);
    }

    #[test]
    fn correlate_verdicts_zero_one_many() {
        let sc = |v: &[(&str, u64)]| ProbeSidecar {
            adapter: "claude".into(),
            cwd: "/home/alice/proj".into(),
            block_key: (1, 0),
            taken_ms: 0,
            listings: snap(DIR, v),
            overflow: false,
        };
        // |C| == 1 ⇒ Correlated.
        let s = sc(&[]);
        assert_eq!(
            correlate(claude(), Some(&s), &listing(DIR, &[(&jl(A), 1)]), false, false),
            Verdict::Correlated(A.to_string())
        );
        // |C| == 0 ⇒ Ambiguous, no candidates.
        let s = sc(&[(&jl(A), 1)]);
        assert_eq!(
            correlate(claude(), Some(&s), &listing(DIR, &[(&jl(A), 1)]), false, false),
            Verdict::Ambiguous(vec![])
        );
        // |C| ≥ 2 with the sibling gate up ⇒ Ambiguous WITH candidates,
        // newest-first (ls -lt order).
        let s = sc(&[]);
        assert_eq!(
            correlate(
                claude(),
                Some(&s),
                &listing(DIR, &[(&jl(C), 5), (&jl(B), 10)]),
                false,
                true,
            ),
            Verdict::Ambiguous(vec![C.to_string(), B.to_string()])
        );
    }

    #[test]
    fn correlate_overflow_goes_straight_to_ambiguous_no_candidates() {
        let mut s = ProbeSidecar {
            adapter: "claude".into(),
            cwd: "/p".into(),
            block_key: (1, 0),
            taken_ms: 0,
            listings: snap(DIR, &[]),
            overflow: true,
        };
        let cur = listing(DIR, &[(&jl(A), 1)]);
        assert_eq!(
            correlate(claude(), Some(&s), &cur, false, false),
            Verdict::Ambiguous(vec![])
        );
        s.overflow = false;
        // current-side overflow too, and it beats even an exact diff.
        assert_eq!(
            correlate(claude(), Some(&s), &cur, true, false),
            Verdict::Ambiguous(vec![])
        );
        // no-snapshot + overflow: fallback never fires either.
        assert_eq!(
            correlate(claude(), None, &cur, true, false),
            Verdict::Ambiguous(vec![])
        );
    }

    // ── §5.3 R-NEWEST preconditions ──

    /// Newest-first pick from a captured `ls -lt`-shaped fixture, parsed
    /// through the real splitter (order is the server-attr sort).
    #[test]
    fn r_newest_picks_ls_lt_first_with_all_preconditions() {
        let stdout = format!(
            "sftp> -ls -lt {DIR}\r\n\
             -rw-******    ? alice  alice    5 Jul  4  2026 {DIR}/{}\r\n\
             -rw-******    ? alice  alice   10 Jul  4 15:46 {DIR}/{}\r\n\
             -rw-******    ? alice  alice    1 Jul  4 13:56 {DIR}/{}\r\n",
            jl(C),
            jl(B),
            jl(A),
        );
        let cur = split_listings(&stdout, &[DIR.to_string()]);
        // Valid snapshot (empty store at open), rotation, no sibling ⇒ the
        // /clear chain resolves to the newest member.
        let s = ProbeSidecar {
            adapter: "claude".into(),
            cwd: "/home/alice/proj".into(),
            block_key: (1, 0),
            taken_ms: 0,
            listings: snap(DIR, &[]),
            overflow: false,
        };
        assert_eq!(
            correlate(claude(), Some(&s), &cur, false, false),
            Verdict::Correlated(C.to_string())
        );
    }

    #[test]
    fn r_newest_sibling_gate_flips_it_off() {
        let s = ProbeSidecar {
            adapter: "claude".into(),
            cwd: "/p".into(),
            block_key: (1, 0),
            taken_ms: 0,
            listings: snap(DIR, &[]),
            overflow: false,
        };
        let cur = listing(DIR, &[(&jl(C), 5), (&jl(B), 10)]);
        assert_eq!(
            correlate(claude(), Some(&s), &cur, false, true),
            Verdict::Ambiguous(vec![C.to_string(), B.to_string()])
        );
    }

    #[test]
    fn r_newest_requires_rotation_flag() {
        // codex (rotation: false): ≥2 diff candidates NEVER auto-resolve.
        let codex = store_for("codex").unwrap();
        let dir = ".codex/sessions/2026/07/04";
        let r1 = format!("rollout-2026-07-04T10-00-00-{A}.jsonl");
        let r2 = format!("rollout-2026-07-04T11-00-00-{B}.jsonl");
        let s = ProbeSidecar {
            adapter: "codex".into(),
            cwd: "/p".into(),
            block_key: (1, 0),
            taken_ms: 0,
            listings: snap(dir, &[]),
            overflow: false,
        };
        let cur = listing(dir, &[(&r2, 5), (&r1, 5)]);
        assert_eq!(
            correlate(codex, Some(&s), &cur, false, false),
            Verdict::Ambiguous(vec![B.to_string(), A.to_string()])
        );
    }

    #[test]
    fn r_newest_requires_valid_snapshot_diff_not_fallback() {
        // No snapshot ⇒ the ≥2 case can never R-NEWEST (precondition 2):
        // two entries in the fallback universe stay Ambiguous even for
        // claude with no sibling.
        let cur = listing(DIR, &[(&jl(C), 5), (&jl(B), 10)]);
        assert_eq!(
            correlate(claude(), None, &cur, false, false),
            Verdict::Ambiguous(vec![C.to_string(), B.to_string()])
        );
    }

    // ── §5.2 fallback rules ──

    #[test]
    fn fallback_claude_exactly_one_fires() {
        let cur = listing(DIR, &[(&jl(A), 42)]);
        assert_eq!(
            correlate(claude(), None, &cur, false, false),
            Verdict::Correlated(A.to_string())
        );
        // Store absent (empty listing): Ambiguous, no candidates.
        assert_eq!(
            correlate(claude(), None, &listing(DIR, &[]), false, false),
            Verdict::Ambiguous(vec![])
        );
    }

    #[test]
    fn fallback_global_stores_never_fire() {
        // codex/copilot: a lone GLOBAL entry proves nothing about THIS
        // terminal — candidates listed, never correlated.
        let codex = store_for("codex").unwrap();
        let dir = ".codex/sessions/2026/07/04";
        let r1 = format!("rollout-2026-07-04T10-00-00-{A}.jsonl");
        assert_eq!(
            correlate(codex, None, &listing(dir, &[(&r1, 5)]), false, false),
            Verdict::Ambiguous(vec![A.to_string()])
        );
        let cp = store_for("copilot").unwrap();
        let cur = vec![(
            ".copilot/session-state".to_string(),
            vec![Entry {
                name: A.to_string(),
                size: 4096,
                is_dir: true,
            }],
        )];
        assert_eq!(
            correlate(cp, None, &cur, false, false),
            Verdict::Ambiguous(vec![A.to_string()])
        );
    }

    // ── §6.5 re-pin belt truth table (mirrors claude_repin_evidence_rules) ──

    #[test]
    fn repin_belt_truth_table() {
        // Pinned ∈ C (written during the run) ⇒ alive, keep.
        assert_eq!(repin_candidate(A, &[A.to_string()]), None);
        assert_eq!(repin_candidate(A, &[B.to_string(), A.to_string()]), None);
        // Pinned ∉ C ∧ |C| == 1 ⇒ rotated onto the member, re-pin.
        assert_eq!(repin_candidate(A, &[B.to_string()]), Some(B.to_string()));
        // |C| ≥ 2 ⇒ abstain.
        assert_eq!(repin_candidate(A, &[B.to_string(), C.to_string()]), None);
        // Empty diff ⇒ nothing rotated, keep.
        assert_eq!(repin_candidate(A, &[]), None);
    }

    // ── sidecar roundtrip + validity ──

    #[test]
    fn sidecar_roundtrip_and_validity_gate() {
        let dir = std::env::temp_dir().join(format!("tc_probe_sc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = Uuid::new_v4();
        let s = ProbeSidecar {
            adapter: "claude".into(),
            cwd: "/home/alice/proj".into(),
            block_key: (3, 1234),
            taken_ms: 42,
            listings: snap(DIR, &[(&jl(A), 100)]),
            overflow: false,
        };
        assert!(load_sidecar_in(&dir, id).is_none());
        save_sidecar_in(&dir, id, &s).unwrap();
        assert_eq!(load_sidecar_in(&dir, id), Some(s.clone()));
        // No stray tmp left behind (atomic tmp+rename).
        assert!(!sidecar_path_in(&dir, id).with_extension("json.tmp").exists());

        // Validity: adapter + cwd must match the persisted inner_cli…
        let cli = InnerCli {
            adapter: "claude".into(),
            resume_token: None,
            confidence: CliConfidence::Ambiguous,
            cwd: PathBuf::from("/home/alice/proj"),
        };
        assert!(sidecar_matches(&s, &cli));
        let mut wrong = cli.clone();
        wrong.adapter = "codex".into();
        assert!(!sidecar_matches(&s, &wrong));
        let mut wrong = cli.clone();
        wrong.cwd = PathBuf::from("/home/alice/other");
        assert!(!sidecar_matches(&s, &wrong));
        // …and the block_key's record must re-parse to the same adapter.
        assert!(block_key_confirms(Some("claude"), &cli));
        assert!(block_key_confirms(Some("claude --verbose"), &cli));
        assert!(!block_key_confirms(Some("git status"), &cli));
        assert!(!block_key_confirms(Some("codex"), &cli));
        assert!(!block_key_confirms(None, &cli)); // key names no rec ⇒ invalid
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── §6.4 preface notice ──

    #[test]
    fn ambiguous_notice_caps_five_newest_first_full_commands() {
        let cands: Vec<String> = (0..7)
            .map(|i| format!("00000000-0000-0000-0000-00000000000{i}"))
            .collect();
        let text = ambiguous_notice("claude", "/home/alice/proj", &cands);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 6); // header + cap 5
        assert!(lines[0].contains("multiple claude sessions found in /home/alice/proj"));
        assert!(lines[1].contains(&format!("claude --resume {}", cands[0])));
        assert!(lines[1].contains("(newest)"));
        assert!(lines[5].contains(&format!("claude --resume {}", cands[4])));
        assert!(!text.contains(&cands[5]));
        // Zero candidates: single line, no candidates claim.
        let text = ambiguous_notice("claude", "/p", &[]);
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("no sessions were found in /p"));
    }
}
