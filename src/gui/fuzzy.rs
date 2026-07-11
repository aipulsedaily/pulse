//! Ctrl-R history-search matcher (Warp-study Tier-2b #1): a tiny,
//! self-contained scorer over single-line command strings, written from
//! scratch for Pulse (idea parity only with "exact → prefix → fuzzy"
//! suggestion ordering; no external code, no dependency).
//!
//! Contract: `fuzzy_match(query, cand)` returns `Some((score, positions))`
//! when every query char appears in `cand` in order (ASCII case-insensitive
//! — command lines are overwhelmingly ASCII; non-ASCII compares exact), else
//! `None`. Higher score = better. Tier bases guarantee the ordering
//! whole-string exact > prefix > contiguous substring > scattered
//! subsequence; within the substring tier a word-boundary occurrence beats a
//! mid-word one, and within the subsequence tier contiguous runs and
//! word-boundary hits outrank scattered matches. `positions` are CHAR
//! indices into `cand` (the overlay's highlight spans). The empty query
//! matches everything at score 0 (callers keep their own recency order).

/// Tier bases — far above any per-char bonus sum, so tiers never overlap.
const TIER_EXACT: i32 = 3_000_000;
const TIER_PREFIX: i32 = 2_000_000;
const TIER_SUBSTR: i32 = 1_000_000;
/// Substring tier: a match starting at a word boundary beats any mid-word
/// occurrence regardless of position; earlier starts win among equals.
const SUBSTR_WORD_BONUS: i32 = 100_000;
/// Subsequence tier per-char shaping: contiguity beats scatter, word starts
/// beat mid-word hits, and every skipped char costs a little (capped per gap
/// so one long argument can't drown a good tail match).
const ADJ_BONUS: i32 = 16;
const WORD_BONUS: i32 = 8;
const GAP_CAP: i32 = 8;

/// `prev` is the char immediately before the match (None = string start).
fn word_start(prev: Option<char>) -> bool {
    prev.is_none_or(|c| {
        c.is_whitespace() || matches!(c, '-' | '_' | '.' | '/' | '\\' | ':')
    })
}

pub(crate) fn fuzzy_match(query: &str, cand: &str) -> Option<(i32, Vec<usize>)> {
    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    let c: Vec<char> = cand.chars().map(|c| c.to_ascii_lowercase()).collect();
    if q.is_empty() {
        return Some((0, Vec::new()));
    }
    if q == c {
        return Some((TIER_EXACT, (0..c.len()).collect()));
    }
    if c.len() > q.len() && c[..q.len()] == q[..] {
        return Some((TIER_PREFIX, (0..q.len()).collect()));
    }
    // Contiguous substring: scan every occurrence, keep the best-scored one.
    let mut best: Option<(i32, usize)> = None;
    if c.len() > q.len() {
        for start in 1..=c.len() - q.len() {
            if c[start..start + q.len()] == q[..] {
                let bonus = if word_start(Some(c[start - 1])) {
                    SUBSTR_WORD_BONUS
                } else {
                    0
                };
                let score = TIER_SUBSTR + bonus - (start as i32).min(1000);
                if best.is_none_or(|(b, _)| score > b) {
                    best = Some((score, start));
                }
            }
        }
    }
    if let Some((score, start)) = best {
        return Some((score, (start..start + q.len()).collect()));
    }
    // Scattered subsequence, greedy left-to-right (simple beats optimal —
    // the tiers above already caught every contiguous form).
    let mut pos = Vec::with_capacity(q.len());
    let mut score = 0i32;
    let mut qi = 0usize;
    let mut last: Option<usize> = None;
    for (i, &ch) in c.iter().enumerate() {
        if qi < q.len() && ch == q[qi] {
            if last.is_some_and(|l| l + 1 == i) {
                score += ADJ_BONUS;
            }
            if word_start(i.checked_sub(1).map(|p| c[p])) {
                score += WORD_BONUS;
            }
            let gap = match last {
                Some(l) => (i - l - 1) as i32,
                None => i as i32, // leading offset counts as a gap too
            };
            score -= gap.min(GAP_CAP);
            pos.push(i);
            last = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then_some((score, pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(q: &str, c: &str) -> i32 {
        fuzzy_match(q, c).expect("expected a match").0
    }

    #[test]
    fn tiers_exact_over_prefix_over_substring_over_subsequence() {
        let exact = score("git", "git");
        let prefix = score("git", "git status");
        let substr = score("git", "my git tool");
        let subseq = score("git", "grep -i toml");
        assert!(exact > prefix, "exact must beat prefix");
        assert!(prefix > substr, "prefix must beat substring");
        assert!(substr > subseq, "substring must beat subsequence");
    }

    #[test]
    fn word_boundary_beats_mid_word() {
        // Substring tier: "com" at a word start vs. buried mid-word.
        assert!(
            score("com", "git commit") > score("com", "incoming"),
            "boundary substring must outrank mid-word substring"
        );
        // Subsequence tier: word-start hits shape the score. "gco" over
        // "git checkout" (g / c at word starts) must beat the same letters
        // buried mid-word in "magic clover" (no boundary hits).
        assert!(
            score("gco", "git checkout") > score("gco", "magic clover"),
            "boundary subsequence must outrank buried subsequence"
        );
    }

    #[test]
    fn contiguity_bonus_within_subsequence() {
        // Both are subsequences of "abc"; the contiguous run scores higher.
        assert!(
            score("abc", "xxabcxx") > score("abc", "a-b-c"),
            "contiguous run must beat scattered chars"
        );
    }

    #[test]
    fn no_match_and_empty_query() {
        assert_eq!(fuzzy_match("xyz", "ls -la"), None);
        assert_eq!(fuzzy_match("gitx", "git"), None, "extra chars never match");
        assert_eq!(fuzzy_match("", "anything"), Some((0, Vec::new())));
        assert_eq!(fuzzy_match("a", ""), None, "empty candidate can't match");
    }

    #[test]
    fn case_insensitive_with_correct_positions() {
        let (s, pos) = fuzzy_match("GIT", "Git Status").expect("match");
        assert_eq!(s, TIER_PREFIX);
        assert_eq!(pos, vec![0, 1, 2]);
        // Substring positions land on the real occurrence.
        let (_, pos) = fuzzy_match("stat", "Git Status").expect("match");
        assert_eq!(pos, vec![4, 5, 6, 7]);
        // Subsequence positions are the matched char indices in order.
        let (_, pos) = fuzzy_match("gc", "git checkout").expect("match");
        assert_eq!(pos, vec![0, 4]);
    }

    #[test]
    fn exact_positions_cover_the_whole_candidate() {
        let (s, pos) = fuzzy_match("cargo test", "Cargo Test").expect("match");
        assert_eq!(s, TIER_EXACT);
        assert_eq!(pos.len(), "cargo test".chars().count());
    }
}
