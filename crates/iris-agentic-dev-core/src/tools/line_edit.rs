//! #24 (upstream `0947049` + `203465e`): positional line edits for `iris_doc`, so a one-line change to
//! a 500-line class does not cost a full re-upload.
//!
//! THE TWO WAYS THIS FEATURE CORRUPTS A FILE, both guarded here:
//!
//! 1. **A LOSSY ROUND TRIP.** Splitting and rejoining must reproduce the input byte for byte, or every
//!    edit silently rewrites the whole document — line endings, trailing newline and all. `str::lines()`
//!    is NOT usable: it strips a trailing `\r`, so a CRLF file would come back LF-converted. This splits
//!    on `\n` and leaves any `\r` inside the line, which round-trips both.
//! 2. **EDITING A TRUNCATED READ.** `iris_doc(get)` paginates. Applying an edit to a partial read and
//!    writing it back TRUNCATES THE DOCUMENT. The handler refuses unless the read was complete; that
//!    guard is the single most important line in the feature.
//!
//! Line numbers are 1-BASED, matching what an editor and `symbols_local` report.

use serde::Serialize;

/// One positional operation. Exactly one per call, deliberately: after any edit the line numbers below
/// it have shifted, so a batch would be applying the caller's second set of numbers to a file that no
/// longer matches them — a silent wrong-line edit. The response returns the new line count so the
/// caller can recompute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineOp {
    /// Insert BEFORE line `at`. `at == total + 1` appends.
    Insert { at: usize, lines: Vec<String> },
    /// Remove `count` lines starting at `at`.
    Delete { at: usize, count: usize },
}

impl LineOp {
    pub fn at(&self) -> usize {
        match self {
            Self::Insert { at, .. } | Self::Delete { at, .. } => *at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    AtIsZero,
    /// Insert position past the append slot, or delete position past the last line.
    AtBeyondEnd {
        at: usize,
        total: usize,
    },
    DeleteRunsPastEnd {
        at: usize,
        count: usize,
        total: usize,
    },
    CountIsZero,
    NoLines,
    ExpectMismatch {
        at: usize,
        expected: String,
        found: String,
    },
    ExpectRequiredForDelete,
}

impl EditError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ExpectMismatch { .. } => "LINE_EXPECT_MISMATCH",
            _ => "INVALID_PARAMS",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::AtIsZero => "`at` is 1-based; 0 is not a line. The first line is 1.".into(),
            Self::AtBeyondEnd { at, total } => format!(
                "`at`={at} is past the end: the document has {total} lines. To APPEND, pass \
                 at={} (one past the last line).",
                total + 1
            ),
            Self::DeleteRunsPastEnd { at, count, total } => format!(
                "Deleting {count} lines from {at} runs past the end: the document has {total} \
                 lines, so at most {} can be deleted from there.",
                total.saturating_sub(at - 1)
            ),
            Self::CountIsZero => "`count` must be at least 1 — deleting 0 lines is a no-op.".into(),
            Self::NoLines => "`lines` is empty — inserting nothing is a no-op.".into(),
            Self::ExpectMismatch {
                at,
                expected,
                found,
            } => format!(
                "REFUSED: line {at} is not what you expected, so the document changed since you \
                 read it and these line numbers are stale. Re-read it and retry.\n  expected: \
                 {expected:?}\n  found:    {found:?}"
            ),
            Self::ExpectRequiredForDelete => "mode=delete_lines requires `expect`: the text of the \
                 FIRST line being deleted, exactly as it reads now. Deleting by line number alone \
                 removes whatever happens to be there, and if the document changed since you read it \
                 that is not what you meant. Insertion does not require it — a misplaced insert loses \
                 nothing and shows up in the returned summary."
                .into(),
        }
    }
}

/// Split content into 1-based lines plus whether it ended with a newline.
///
/// Any `\r` stays INSIDE the line, so CRLF survives [`join_lines`] unchanged. `str::lines()` would
/// strip it and silently convert the file.
pub fn split_lines(content: &str) -> (Vec<String>, bool) {
    if content.is_empty() {
        return (Vec::new(), false);
    }
    let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
    // A trailing newline leaves a final empty element that is not a line. Record it and drop it, so
    // `total` matches what an editor shows.
    let trailing = lines.last().is_some_and(|l| l.is_empty());
    if trailing {
        lines.pop();
    }
    (lines, trailing)
}

/// Inverse of [`split_lines`].
pub fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    out
}

/// Reject what would be wrong BEFORE touching the document.
///
/// `expect` is the text the caller believes is at `at`. It is REQUIRED for a delete and optional for an
/// insert — an asymmetry with a reason: a delete removes content, so landing on the wrong line destroys
/// something, while a misplaced insert loses nothing and is visible in the returned summary.
pub fn validate(op: &LineOp, lines: &[String], expect: Option<&str>) -> Result<(), EditError> {
    let total = lines.len();
    if op.at() == 0 {
        return Err(EditError::AtIsZero);
    }
    match op {
        LineOp::Insert { at, lines: ins } => {
            if ins.is_empty() {
                return Err(EditError::NoLines);
            }
            // `total + 1` is the append slot and is legal.
            if *at > total + 1 {
                return Err(EditError::AtBeyondEnd { at: *at, total });
            }
        }
        LineOp::Delete { at, count } => {
            if *count == 0 {
                return Err(EditError::CountIsZero);
            }
            if *at > total {
                return Err(EditError::AtBeyondEnd { at: *at, total });
            }
            if at + count - 1 > total {
                return Err(EditError::DeleteRunsPastEnd {
                    at: *at,
                    count: *count,
                    total,
                });
            }
            if expect.is_none() {
                return Err(EditError::ExpectRequiredForDelete);
            }
        }
    }
    // Compared verbatim except for trailing whitespace, which an editor may have normalised and which
    // is never the thing the caller means to identify a line by.
    if let Some(want) = expect {
        // Nothing to compare against at the append slot.
        if let Some(found) = lines.get(op.at() - 1) {
            if found.trim_end() != want.trim_end() {
                return Err(EditError::ExpectMismatch {
                    at: op.at(),
                    expected: want.to_string(),
                    found: found.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Apply the operation. Call [`validate`] first; this assumes the bounds hold and clamps rather than
/// panicking if they do not.
pub fn apply(lines: &[String], op: &LineOp) -> Vec<String> {
    let mut out = lines.to_vec();
    match op {
        LineOp::Insert { at, lines: ins } => {
            let idx = (at.saturating_sub(1)).min(out.len());
            out.splice(idx..idx, ins.iter().cloned());
        }
        LineOp::Delete { at, count } => {
            let start = (at.saturating_sub(1)).min(out.len());
            let end = (start + count).min(out.len());
            out.drain(start..end);
        }
    }
    out
}

/// What changed, exactly — the diff for a single positional op.
///
/// Reported rather than a unified diff with context: for one contiguous operation the removed and
/// inserted lines ARE the change, with no ambiguity about what context to show. `lines_after` is what a
/// caller needs to recompute positions for its next edit.
#[derive(Debug, Serialize)]
pub struct EditSummary {
    pub at: usize,
    pub lines_before: usize,
    pub lines_after: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inserted: Vec<String>,
}

pub fn summarise(before: &[String], after: &[String], op: &LineOp) -> EditSummary {
    let (removed, inserted) = match op {
        LineOp::Insert { lines: ins, .. } => (Vec::new(), ins.clone()),
        LineOp::Delete { at, count } => {
            let start = (at.saturating_sub(1)).min(before.len());
            let end = (start + count).min(before.len());
            (before[start..end].to_vec(), Vec::new())
        }
    };
    EditSummary {
        at: op.at(),
        lines_before: before.len(),
        lines_after: after.len(),
        removed,
        inserted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // ── the round trip: the invariant every edit depends on ──────────────────────────────────

    /// If split→join is not the identity, EVERY edit silently rewrites the whole document even when the
    /// edit itself is correct. This is the first thing to get right.
    #[test]
    fn split_then_join_is_the_identity() {
        for content in [
            "",
            "one line",
            "one line\n",
            "a\nb\nc",
            "a\nb\nc\n",
            "\n",
            "\n\n\n",
            "trailing spaces   \n",
            "a\n\nb\n",
        ] {
            let (lines, trailing) = split_lines(content);
            assert_eq!(
                join_lines(&lines, trailing),
                content,
                "round trip lost data for {content:?} -> {lines:?} trailing={trailing}"
            );
        }
    }

    /// CRLF must survive. `str::lines()` strips the `\r`, which would convert the file on every edit —
    /// a whole-file diff from a one-line change.
    #[test]
    fn crlf_survives_the_round_trip() {
        let content = "Class X\r\n{\r\n}\r\n";
        let (lines, trailing) = split_lines(content);
        assert_eq!(join_lines(&lines, trailing), content);
        assert!(
            lines[0].ends_with('\r'),
            "the CR stays in the line: {lines:?}"
        );
        // and the line COUNT is what an editor shows, not one more
        assert_eq!(lines.len(), 3);
    }

    /// A trailing newline is not a line. `"a\nb\n"` is two lines, as an editor shows.
    #[test]
    fn a_trailing_newline_is_not_counted_as_a_line() {
        let (lines, trailing) = split_lines("a\nb\n");
        assert_eq!(lines, s(&["a", "b"]));
        assert!(trailing);
        let (lines2, trailing2) = split_lines("a\nb");
        assert_eq!(lines2, s(&["a", "b"]));
        assert!(!trailing2, "no trailing newline here");
    }

    /// Empty content is zero lines, and must not become one empty line — otherwise inserting at 1 into
    /// an empty document would leave a stray blank.
    #[test]
    fn empty_content_is_zero_lines() {
        let (lines, trailing) = split_lines("");
        assert!(lines.is_empty());
        assert!(!trailing);
        assert_eq!(join_lines(&lines, trailing), "");
    }

    // ── apply ───────────────────────────────────────────────────────────────────────────────

    #[test]
    fn insert_puts_the_lines_before_the_given_line() {
        let before = s(&["a", "b", "c"]);
        let after = apply(
            &before,
            &LineOp::Insert {
                at: 2,
                lines: s(&["X", "Y"]),
            },
        );
        assert_eq!(after, s(&["a", "X", "Y", "b", "c"]));
    }

    #[test]
    fn inserting_at_one_prepends() {
        let after = apply(
            &s(&["a"]),
            &LineOp::Insert {
                at: 1,
                lines: s(&["X"]),
            },
        );
        assert_eq!(after, s(&["X", "a"]));
    }

    /// `total + 1` is the append slot — the only way to add at the end, and legal.
    #[test]
    fn inserting_one_past_the_end_appends() {
        let before = s(&["a", "b"]);
        let op = LineOp::Insert {
            at: 3,
            lines: s(&["c"]),
        };
        assert_eq!(validate(&op, &before, None), Ok(()));
        assert_eq!(apply(&before, &op), s(&["a", "b", "c"]));
    }

    #[test]
    fn delete_removes_exactly_the_requested_run() {
        let before = s(&["a", "b", "c", "d"]);
        let after = apply(&before, &LineOp::Delete { at: 2, count: 2 });
        assert_eq!(after, s(&["a", "d"]));
    }

    // ── validate: the refusals ──────────────────────────────────────────────────────────────

    #[test]
    fn at_zero_is_refused_because_lines_are_one_based() {
        let e = validate(
            &LineOp::Insert {
                at: 0,
                lines: s(&["x"]),
            },
            &s(&["a"]),
            None,
        )
        .unwrap_err();
        assert_eq!(e, EditError::AtIsZero);
        assert!(e.message().contains("1-based"), "{}", e.message());
    }

    /// Inserting two past the end would leave a gap. The message must name the append slot, since that
    /// is what the caller almost certainly wanted.
    #[test]
    fn inserting_two_past_the_end_is_refused_and_names_the_append_slot() {
        let e = validate(
            &LineOp::Insert {
                at: 4,
                lines: s(&["x"]),
            },
            &s(&["a", "b"]),
            None,
        )
        .unwrap_err();
        assert_eq!(e, EditError::AtBeyondEnd { at: 4, total: 2 });
        assert!(e.message().contains("at=3"), "{}", e.message());
    }

    #[test]
    fn a_delete_running_past_the_end_is_refused_with_the_max() {
        let e = validate(
            &LineOp::Delete { at: 2, count: 5 },
            &s(&["a", "b", "c"]),
            Some("b"),
        )
        .unwrap_err();
        assert_eq!(
            e,
            EditError::DeleteRunsPastEnd {
                at: 2,
                count: 5,
                total: 3
            }
        );
        assert!(e.message().contains("at most 2"), "{}", e.message());
    }

    #[test]
    fn no_op_edits_are_refused() {
        assert_eq!(
            validate(
                &LineOp::Insert {
                    at: 1,
                    lines: vec![]
                },
                &s(&["a"]),
                None
            ),
            Err(EditError::NoLines)
        );
        assert_eq!(
            validate(&LineOp::Delete { at: 1, count: 0 }, &s(&["a"]), Some("a")),
            Err(EditError::CountIsZero)
        );
    }

    // ── the stale-line-numbers guard ─────────────────────────────────────────────────────────

    /// The whole point of `expect`: line numbers come from an earlier read, and if the document changed
    /// the edit lands somewhere else ENTIRELY SILENTLY. This turns that into a refusal.
    #[test]
    fn a_wrong_expect_refuses_and_shows_both_texts() {
        let e = validate(
            &LineOp::Delete { at: 2, count: 1 },
            &s(&["a", "b", "c"]),
            Some("not b"),
        )
        .unwrap_err();
        assert_eq!(e.code(), "LINE_EXPECT_MISMATCH");
        let m = e.message();
        assert!(m.contains("stale"), "{m}");
        assert!(m.contains("\"not b\""), "must show what was expected: {m}");
        assert!(m.contains("\"b\""), "and what is actually there: {m}");
    }

    /// REQUIRED for delete, because a delete on the wrong line destroys content.
    #[test]
    fn delete_without_expect_is_refused() {
        let e = validate(&LineOp::Delete { at: 1, count: 1 }, &s(&["a"]), None).unwrap_err();
        assert_eq!(e, EditError::ExpectRequiredForDelete);
        assert!(e.message().contains("requires `expect`"), "{}", e.message());
    }

    /// OPTIONAL for insert, and the asymmetry is deliberate: a misplaced insert loses nothing and shows
    /// in the summary. Requiring it would also make appending impossible — there is no line to expect
    /// at the append slot.
    #[test]
    fn insert_without_expect_is_allowed() {
        assert_eq!(
            validate(
                &LineOp::Insert {
                    at: 1,
                    lines: s(&["x"])
                },
                &s(&["a"]),
                None
            ),
            Ok(())
        );
    }

    /// Trailing whitespace is not what identifies a line, and an editor may have normalised it.
    #[test]
    fn expect_ignores_trailing_whitespace_only() {
        assert_eq!(
            validate(
                &LineOp::Delete { at: 1, count: 1 },
                &s(&["  Set x = 1   "]),
                Some("  Set x = 1")
            ),
            Ok(())
        );
        // LEADING whitespace is indentation and IS significant
        assert!(validate(
            &LineOp::Delete { at: 1, count: 1 },
            &s(&["  Set x = 1"]),
            Some("Set x = 1")
        )
        .is_err());
    }

    // ── summary ─────────────────────────────────────────────────────────────────────────────

    #[test]
    fn a_delete_summary_reports_exactly_what_went() {
        let before = s(&["a", "b", "c"]);
        let op = LineOp::Delete { at: 2, count: 2 };
        let after = apply(&before, &op);
        let sum = summarise(&before, &after, &op);
        assert_eq!(sum.at, 2);
        assert_eq!(sum.lines_before, 3);
        assert_eq!(sum.lines_after, 1);
        assert_eq!(sum.removed, s(&["b", "c"]));
        assert!(sum.inserted.is_empty());
    }

    #[test]
    fn an_insert_summary_reports_what_arrived_and_the_new_count() {
        let before = s(&["a"]);
        let op = LineOp::Insert {
            at: 2,
            lines: s(&["b", "c"]),
        };
        let after = apply(&before, &op);
        let sum = summarise(&before, &after, &op);
        assert_eq!(sum.inserted, s(&["b", "c"]));
        assert!(sum.removed.is_empty());
        // the new count is what the caller needs to place its NEXT edit
        assert_eq!(sum.lines_after, 3);
    }

    /// End to end on the bytes: a one-line edit must change exactly one line and nothing else — the
    /// property the round-trip test exists to support.
    #[test]
    fn a_one_line_edit_changes_only_that_line_in_the_output_bytes() {
        let content = "Class X\r\n{\r\nMethod M()\r\n{\r\n}\r\n}\r\n";
        let (lines, trailing) = split_lines(content);
        let op = LineOp::Delete { at: 3, count: 1 };
        assert_eq!(validate(&op, &lines, Some("Method M()")), Ok(()));
        let out = join_lines(&apply(&lines, &op), trailing);
        assert_eq!(out, "Class X\r\n{\r\n{\r\n}\r\n}\r\n");
        // CRLF intact everywhere, and the trailing newline preserved
        assert_eq!(out.matches("\r\n").count(), 5, "{out:?}");
        assert!(out.ends_with("\r\n"));
    }
}
