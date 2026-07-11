//! Prompt syntax highlighting (Warp-study Tier-2a #1): a lightweight,
//! token-level colorizer for the composer's draft, fed to egui's TextEdit
//! through its `.layouter` closure. Rides the SAME whitespace tokenizer the
//! Tab completer uses (`complete::tokens` — family quote/escape rules
//! included), so a quoted token here IS a quoted token there. No syntax
//! tree, no new dependency, no state.
//!
//! Pure presentation over `state.draft`: the produced `LayoutJob` differs
//! from egui's default single-section job ONLY in per-token section colors —
//! same font, same wrap width, same `keep_trailing_whitespace`/`halign`, so
//! galley geometry (rows, wraps, caret rects) is byte-identical to an
//! uncolored draft. The zero-delay dispatch path never sees any of this.
//!
//! Doctrine: subtle, not christmas-tree. Only whole whitespace-separated
//! tokens are classified (an unspaced `a|b` stays plain — honest beats
//! clever), and every hue is an EXISTING palette constant from mod.rs.

use std::ops::Range;

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Color32, FontId};

use super::complete::{self, Family};

/// Command head (first word of each command segment): the app accent.
const HEAD: Color32 = super::ACCENT;
/// Flags (`-x`, `--long`, `-Verbose`): one step dimmer than arguments.
const FLAG: Color32 = super::TEXT_SECONDARY;
/// Quoted strings: the terminal palette's muted gold (TAG_COLORS "Gold").
const STR: Color32 = super::TAG_COLORS[2].0;
/// Operators (`| && > ;` …): punctuation, dim.
const OP: Color32 = super::TEXT_MUTED;
/// Variable refs (`$FOO`, `$env:FOO`, `%FOO%`): the palette teal.
const VAR: Color32 = super::TAG_COLORS[4].0;
/// Paths and plain arguments: exactly today's draft color.
const PLAIN: Color32 = super::TEXT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Head,
    Flag,
    Str,
    Op,
    Var,
    Plain,
}

pub(crate) fn color(c: Class) -> Color32 {
    match c {
        Class::Head => HEAD,
        Class::Flag => FLAG,
        Class::Str => STR,
        Class::Op => OP,
        Class::Var => VAR,
        Class::Plain => PLAIN,
    }
}

/// Whole-token operators (whitespace-separated only). Covers the shared
/// pipeline/redirect set plus pwsh/bash stream forms (`2>`, `2>&1`, `*>`).
fn is_op(raw: &str) -> bool {
    matches!(
        raw,
        "|" | "||"
            | "&&"
            | ";"
            | "&"
            | ">"
            | ">>"
            | "<"
            | "<<"
            | "1>"
            | "1>>"
            | "2>"
            | "2>>"
            | "2>&1"
            | "*>"
            | "*>>"
    )
}

/// Operators after which the NEXT token is a command head again
/// (`git log | grep x` colors both `git` and `grep`).
fn separates_commands(raw: &str) -> bool {
    matches!(raw, "|" | "||" | "&&" | ";" | "&")
}

/// The token begins with one of the family's quote characters (the same set
/// `complete::tokens` honors) — render the whole token as a string literal.
fn is_quoted(fam: &Family, raw: &str) -> bool {
    let quotes: &[char] = match fam {
        Family::Pwsh => &['\'', '"'],
        Family::Cmd | Family::Other => &['"'],
        Family::Wsl { .. } | Family::Ssh => &['\'', '"'],
    };
    raw.starts_with(quotes)
}

/// Environment/variable reference: `$FOO` / `${FOO}` / `$env:FOO` for
/// pwsh/bash-shaped families, `%FOO%` for cmd (and WT-style Other, which
/// expands both spellings).
fn is_var(fam: &Family, raw: &str) -> bool {
    let dollar = raw.len() >= 2 && raw.starts_with('$');
    let percent = raw.len() >= 3 && raw.starts_with('%') && raw[1..].contains('%');
    match fam {
        Family::Cmd => percent,
        Family::Other => dollar || percent,
        Family::Pwsh | Family::Wsl { .. } | Family::Ssh => dollar,
    }
}

/// `-x` / `--long` / `-Verbose` — but NOT bare negative numbers (`head -1`
/// keeps its argument plain).
fn is_flag(raw: &str) -> bool {
    raw.len() >= 2 && raw.starts_with('-') && !raw.as_bytes()[1].is_ascii_digit()
}

/// Classify every token of the draft. Ranges are byte ranges into `text`,
/// ordered and non-overlapping (the gaps between them are whitespace, which
/// the job builder fills with the plain color). Multiline drafts restart the
/// command-head rule per line (`\n` between tokens = new command).
pub(crate) fn classify(fam: &Family, text: &str) -> Vec<(Range<usize>, Class)> {
    let toks = complete::tokens(fam, text);
    let mut out = Vec::with_capacity(toks.len());
    let mut head_next = true;
    let mut prev_end = 0usize;
    for t in toks {
        let raw = &text[t.start..t.end];
        if text[prev_end..t.start].contains('\n') {
            head_next = true;
        }
        let class = if is_op(raw) {
            Class::Op
        } else if is_quoted(fam, raw) {
            Class::Str
        } else if is_var(fam, raw) {
            Class::Var
        } else if is_flag(raw) {
            Class::Flag
        } else if head_next {
            Class::Head
        } else {
            Class::Plain
        };
        // Only a command separator re-opens the head slot; any other token
        // (including `>` redirect targets) closes it for this segment.
        head_next = class == Class::Op && separates_commands(raw);
        prev_end = t.end;
        out.push((t.start..t.end, class));
    }
    out
}

/// Build the composer TextEdit's layout job. Mirrors what egui's DEFAULT
/// layouter produces (`LayoutJob::simple(text, font, color, wrap_width)` +
/// `halign = LEFT` + `keep_trailing_whitespace = true`) except that token
/// spans carry their class colors — geometry-identical, colors only.
///
/// Runs inside the layouter closure, so it must tokenize the text it is
/// HANDED (mid-frame edits re-layout before the draft settles); cost is one
/// O(len) pass + section pushes per call, and egui's galley cache absorbs
/// repeated identical jobs across frames.
pub(crate) fn layout_job(fam: &Family, text: &str, font_id: &FontId, wrap_width: f32) -> LayoutJob {
    if text.is_empty() {
        // Match the default layouter's empty job exactly (one empty section
        // keeps the caret/row-height machinery identical).
        let mut job = LayoutJob::simple(String::new(), font_id.clone(), PLAIN, wrap_width);
        job.keep_trailing_whitespace = true;
        return job;
    }
    let mut job = LayoutJob {
        halign: Align::LEFT,
        keep_trailing_whitespace: true,
        ..Default::default()
    };
    job.wrap.max_width = wrap_width;
    let plain = TextFormat::simple(font_id.clone(), PLAIN);
    let mut cursor = 0usize;
    for (r, class) in classify(fam, text) {
        if r.start > cursor {
            job.append(&text[cursor..r.start], 0.0, plain.clone());
        }
        job.append(
            &text[r.clone()],
            0.0,
            TextFormat::simple(font_id.clone(), color(class)),
        );
        cursor = r.end;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, plain);
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::text::ByteIndex;

    fn pwsh() -> Family {
        Family::Pwsh
    }

    /// classify() output as (token text, class) pairs for terse assertions.
    fn classes<'a>(fam: &Family, s: &'a str) -> Vec<(&'a str, Class)> {
        classify(fam, s)
            .into_iter()
            .map(|(r, c)| (&s[r], c))
            .collect()
    }

    #[test]
    fn head_flags_args_and_operators() {
        assert_eq!(
            classes(&pwsh(), "git log --oneline -5 | grep fix > out.txt"),
            vec![
                ("git", Class::Head),
                ("log", Class::Plain),
                ("--oneline", Class::Flag),
                ("-5", Class::Plain), // negative-number rule
                ("|", Class::Op),
                ("grep", Class::Head), // head re-opens after a pipe
                ("fix", Class::Plain),
                (">", Class::Op),
                ("out.txt", Class::Plain), // redirect target is NOT a head
            ]
        );
        // && and ; also separate commands; 2>&1 is one operator token.
        assert_eq!(
            classes(&pwsh(), "cargo build && cargo test 2>&1 ; echo done"),
            vec![
                ("cargo", Class::Head),
                ("build", Class::Plain),
                ("&&", Class::Op),
                ("cargo", Class::Head),
                ("test", Class::Plain),
                ("2>&1", Class::Op),
                (";", Class::Op),
                ("echo", Class::Head),
                ("done", Class::Plain),
            ]
        );
        // Unspaced pseudo-operators stay plain (subtle beats clever).
        assert_eq!(
            classes(&pwsh(), "ls a|b")[1],
            ("a|b", Class::Plain),
        );
    }

    #[test]
    fn quotes_both_kinds_and_unterminated() {
        assert_eq!(
            classes(&pwsh(), r#"git commit -m 'fix: a b' "and this""#),
            vec![
                ("git", Class::Head),
                ("commit", Class::Plain),
                ("-m", Class::Flag),
                ("'fix: a b'", Class::Str),
                (r#""and this""#, Class::Str),
            ]
        );
        // Unterminated quote runs to the end (mid-edit state) — still a Str.
        assert_eq!(
            classes(&pwsh(), "echo 'half writ").last().copied(),
            Some(("'half writ", Class::Str))
        );
        // Cmd family: only double quotes are quotes.
        assert_eq!(
            classes(&Family::Cmd, r#"type "a b.txt" 'c"#),
            vec![
                ("type", Class::Head),
                (r#""a b.txt""#, Class::Str),
                ("'c", Class::Plain),
            ]
        );
    }

    #[test]
    fn variables_per_family() {
        assert_eq!(
            classes(&pwsh(), "echo $env:USERPROFILE $x"),
            vec![
                ("echo", Class::Head),
                ("$env:USERPROFILE", Class::Var),
                ("$x", Class::Var),
            ]
        );
        assert_eq!(
            classes(&Family::Cmd, "echo %USERPROFILE% $HOME %x"),
            vec![
                ("echo", Class::Head),
                ("%USERPROFILE%", Class::Var),
                ("$HOME", Class::Plain), // cmd never expands $
                ("%x", Class::Plain),    // no closing % — not a var ref
            ]
        );
        let wsl = Family::Wsl { distro: None };
        assert_eq!(
            classes(&wsl, "echo $HOME ${FOO}"),
            vec![
                ("echo", Class::Head),
                ("$HOME", Class::Var),
                ("${FOO}", Class::Var),
            ]
        );
    }

    #[test]
    fn multiline_restarts_the_head_rule_per_line() {
        assert_eq!(
            classes(&pwsh(), "ls -la\ncargo build --release"),
            vec![
                ("ls", Class::Head),
                ("-la", Class::Flag),
                ("cargo", Class::Head),
                ("build", Class::Plain),
                ("--release", Class::Flag),
            ]
        );
    }

    #[test]
    fn layout_job_covers_every_byte_with_the_right_colors() {
        let font = FontId::monospace(14.0);
        let s = "git st | more";
        let job = layout_job(&pwsh(), s, &font, 320.0);
        assert_eq!(job.text, s);
        // Sections are contiguous 0..len (the LayoutJob invariant) — this is
        // exactly what format_at_byte's internal sanity check asserts.
        assert_eq!(job.format_at_byte(ByteIndex(0)).color, color(Class::Head)); // g
        assert_eq!(job.format_at_byte(ByteIndex(3)).color, PLAIN); // space gap
        assert_eq!(job.format_at_byte(ByteIndex(4)).color, PLAIN); // st
        assert_eq!(job.format_at_byte(ByteIndex(7)).color, color(Class::Op)); // |
        assert_eq!(job.format_at_byte(ByteIndex(9)).color, color(Class::Head)); // more
        // Geometry knobs match egui's default layouter exactly.
        assert_eq!(job.wrap.max_width, 320.0);
        assert!(job.break_on_newline);
        assert!(job.keep_trailing_whitespace);
        assert_eq!(job.halign, Align::LEFT);
        // Empty text still yields the one-empty-section job.
        let empty = layout_job(&pwsh(), "", &font, 320.0);
        assert_eq!(empty.sections.len(), 1);
        assert_eq!(empty.wrap.max_width, 320.0);
    }
}
