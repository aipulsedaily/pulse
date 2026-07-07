//! P4 clickable cross-session history: the pure aggregation/filter layer.
//!
//! The corpus is the `BlockList` stores the App already holds — every attach
//! ships a full `D2C::Blocks` sync for every terminal (dead ones included:
//! the daemon loads the sidecar on first journal touch), so the cross-session
//! command history is ALREADY client-side and zero wire changes are needed
//! (spec §3.1 / D5). This module is egui-free: unit-testable and
//! probe-drivable; the popup UI lives in gui/mod.rs where App state is.

use std::path::PathBuf;
use uuid::Uuid;

use crate::state::BlockRec;

/// Hard cap on index entries after dedupe (oldest dropped). 20 terminals ×
/// 500 recs = 10k worst-case input; 5k deduped entries ≈ a fraction of a MB.
pub const MAX_HISTORY: usize = 5000;

/// One deduped command across all terminals and epochs. The MOST RECENT
/// instance (max started_ms) is the representative; `count` accumulates.
pub struct HistEntry {
    /// Trimmed, as recorded (may contain \n on PSReadLine ≥2.2).
    pub cmd: String,
    /// Lowercase, for filtering.
    cmd_lc: String,
    /// Most-recent user's terminal.
    pub term: Uuid,
    /// Resolved at build time (dead terminals still have metas).
    pub term_name: String,
    pub term_dead: bool,
    pub cwd: Option<PathBuf>,
    /// Lowercase display string, for filtering.
    cwd_lc: String,
    /// started_ms of the most recent use.
    pub last_ms: u64,
    /// Exit of the most recent use (None = open / never closed cleanly).
    pub exit: Option<i64>,
    /// Most recent use still running.
    pub open: bool,
    /// Total occurrences across all terminals/epochs.
    pub count: u32,
}

/// Aggregate + dedupe + order (spec §3.2). Pure. `lists` = one tuple per
/// terminal: (id, display name, dead flag, recs sorted by start_off).
/// Dedupe key = exact trimmed cmd string. Sort: last_ms desc, then count
/// desc, then cmd asc — a total order, so the UI is stable across rebuilds.
/// Everything is CLONED out of the recs: no borrows into the App's block
/// stores may outlive the build (the popup outlives any single borrow).
pub fn build_index(lists: &[(Uuid, String, bool, &[BlockRec])]) -> Vec<HistEntry> {
    let mut by_cmd: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut out: Vec<HistEntry> = Vec::new();
    for (id, name, dead, recs) in lists {
        for r in recs.iter() {
            let cmd = r.cmd.trim();
            if cmd.is_empty() {
                continue; // the bootstrap already skips blank lines; belt only
            }
            match by_cmd.get(cmd) {
                Some(&i) => {
                    let e = &mut out[i];
                    e.count += 1;
                    // The newest instance represents the entry.
                    if r.started_ms >= e.last_ms {
                        e.term = *id;
                        e.term_name = name.clone();
                        e.term_dead = *dead;
                        e.cwd = r.cwd.clone();
                        e.cwd_lc = r
                            .cwd
                            .as_ref()
                            .map(|p| p.to_string_lossy().to_lowercase())
                            .unwrap_or_default();
                        e.last_ms = r.started_ms;
                        e.exit = r.exit;
                        e.open = r.end_off.is_none();
                    }
                }
                None => {
                    by_cmd.insert(cmd, out.len());
                    out.push(HistEntry {
                        cmd: cmd.to_string(),
                        cmd_lc: cmd.to_lowercase(),
                        term: *id,
                        term_name: name.clone(),
                        term_dead: *dead,
                        cwd: r.cwd.clone(),
                        cwd_lc: r
                            .cwd
                            .as_ref()
                            .map(|p| p.to_string_lossy().to_lowercase())
                            .unwrap_or_default(),
                        last_ms: r.started_ms,
                        exit: r.exit,
                        open: r.end_off.is_none(),
                        count: 1,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| {
        b.last_ms
            .cmp(&a.last_ms)
            .then(b.count.cmp(&a.count))
            .then(a.cmd.cmp(&b.cmd))
    });
    out.truncate(MAX_HISTORY); // drops the oldest tail
    out
}

/// Tokenized AND-substring filter (D8): the query is split on whitespace and
/// every token must appear in cmd OR cwd, case-insensitive. No regex, no
/// fuzzy scoring — command recall must be predictable. Empty query =
/// identity. Returns indices into `entries` (order preserved = recency).
pub fn filter(entries: &[HistEntry], query: &str) -> Vec<u32> {
    let toks: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .collect();
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            toks.iter()
                .all(|t| e.cmd_lc.contains(t.as_str()) || e.cwd_lc.contains(t.as_str()))
        })
        .map(|(i, _)| i as u32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(cmd: &str, started_ms: u64, exit: Option<i64>, open: bool) -> BlockRec {
        BlockRec {
            epoch: 1,
            n: 0,
            cmd: cmd.into(),
            cwd: Some(PathBuf::from("C:\\Proj")),
            exit,
            started_ms,
            ended_ms: (!open).then_some(started_ms + 1),
            start_off: started_ms, // unique per terminal for these tests
            end_off: (!open).then_some(started_ms + 100),
            truncated: false,
        }
    }

    #[test]
    fn index_dedupes_and_orders() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // "git push" ran in A (old) and in B (newest use, epoch-crossing is
        // irrelevant to the key); "ls" ran once in between; blanks skipped.
        let recs_a = vec![rec("git push", 100, Some(0), false), rec("  ", 150, None, false)];
        let recs_b = vec![rec("ls", 200, Some(0), false), rec("git push", 300, Some(1), false)];
        let lists: Vec<(Uuid, String, bool, &[BlockRec])> = vec![
            (a, "A".into(), false, recs_a.as_slice()),
            (b, "B".into(), true, recs_b.as_slice()),
        ];
        let idx = build_index(&lists);
        assert_eq!(idx.len(), 2, "duplicates dedupe, blanks skipped");
        assert_eq!(idx[0].cmd, "git push", "recency order (newest first)");
        assert_eq!(idx[0].count, 2);
        assert_eq!(idx[0].last_ms, 300, "newest instance represents");
        assert_eq!(idx[0].term, b);
        assert!(idx[0].term_dead);
        assert_eq!(idx[0].exit, Some(1), "exit carried from the newest use");
        assert_eq!(idx[1].cmd, "ls");
        // Tiebreaks: equal last_ms ⇒ count desc, then cmd asc.
        let recs_c = vec![
            rec("bbb", 500, Some(0), false),
            rec("aaa", 500, Some(0), false),
            BlockRec { start_off: 900, ..rec("bbb", 500, Some(0), false) },
        ];
        let lists: Vec<(Uuid, String, bool, &[BlockRec])> =
            vec![(a, "A".into(), false, recs_c.as_slice())];
        let idx = build_index(&lists);
        assert_eq!(idx[0].cmd, "bbb", "count desc breaks the last_ms tie");
        assert_eq!(idx[0].count, 2);
        assert_eq!(idx[1].cmd, "aaa");
    }

    #[test]
    fn filter_tokens_and_case() {
        let a = Uuid::new_v4();
        let recs = vec![
            rec("git commit && git push", 300, Some(0), false),
            rec("Get-ChildItem", 200, Some(0), false),
            rec("cargo build", 100, Some(0), false),
        ];
        let lists: Vec<(Uuid, String, bool, &[BlockRec])> =
            vec![(a, "A".into(), false, recs.as_slice())];
        let idx = build_index(&lists);
        // Multi-token AND: both tokens must appear (in any position).
        let hits = filter(&idx, "git push");
        assert_eq!(hits.len(), 1);
        assert_eq!(idx[hits[0] as usize].cmd, "git commit && git push");
        // Case-insensitive, matches cwd too ("proj" is in C:\Proj).
        assert_eq!(filter(&idx, "GET-CHILD").len(), 1);
        assert_eq!(filter(&idx, "proj").len(), 3, "cwd matches count");
        // Empty query = identity, order preserved.
        assert_eq!(filter(&idx, ""), vec![0, 1, 2]);
        // No hits.
        assert!(filter(&idx, "git zzz").is_empty());
    }

    #[test]
    fn index_caps_at_max() {
        let a = Uuid::new_v4();
        let recs: Vec<BlockRec> = (0..(MAX_HISTORY + 7) as u64)
            .map(|i| rec(&format!("cmd {i}"), i, Some(0), false))
            .collect();
        let lists: Vec<(Uuid, String, bool, &[BlockRec])> =
            vec![(a, "A".into(), false, recs.as_slice())];
        let idx = build_index(&lists);
        assert_eq!(idx.len(), MAX_HISTORY);
        // The oldest 7 dropped: the tail entry is cmd 7 (sorted newest-first).
        assert_eq!(idx.last().unwrap().cmd, "cmd 7");
        assert_eq!(idx[0].cmd, format!("cmd {}", MAX_HISTORY + 6));
    }

    #[test]
    fn open_and_failed_flags() {
        let a = Uuid::new_v4();
        let recs = vec![rec("sleep 99", 100, None, true), rec("bad", 50, Some(3), false)];
        let lists: Vec<(Uuid, String, bool, &[BlockRec])> =
            vec![(a, "A".into(), false, recs.as_slice())];
        let idx = build_index(&lists);
        assert!(idx[0].open, "open rec carries the open flag");
        assert_eq!(idx[0].exit, None);
        assert!(!idx[1].open);
        assert_eq!(idx[1].exit, Some(3), "failed rec carries its exit");
    }
}
