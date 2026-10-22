//! #430 item 4: bounded rows, and the totals that keep a bounded answer honest.
//!
//! The pattern is not "return less". It is "return less AND say what you left out", because a
//! shortened list that reports no total is indistinguishable from a complete one — and a model
//! reads a complete-looking list as the set of things that exist.
//!
//! Two properties carry the weight here:
//!
//!   - `total_rows` is what IRIS returned, not what we chose to show;
//!   - a shortened CELL carries its marker inside its own value, because a caller that reads one
//!     field never sees a payload-level counter.

use iris_agentic_dev_core::tools::sql_mode::{
    bound_rows, DEFAULT_FIELD_CHARS, DEFAULT_ROW_LIMIT, FIELD_CUT_MARKER,
};

fn row(name: &str) -> serde_json::Value {
    serde_json::json!({"Name": name})
}

fn rows(n: usize) -> Vec<serde_json::Value> {
    (0..n).map(|i| row(&format!("Class{i}"))).collect()
}

#[test]
fn a_result_set_under_the_cap_is_not_truncated() {
    let b = bound_rows(rows(5), 10, 0);
    assert_eq!(b.rows.len(), 5);
    assert_eq!(b.total, 5);
    assert!(!b.truncated);
    assert_eq!(b.fields_truncated, 0);
}

#[test]
fn a_result_set_over_the_cap_reports_the_real_total() {
    let b = bound_rows(rows(500), 200, 0);
    assert_eq!(b.rows.len(), 200, "only the cap crosses into context");
    assert_eq!(b.total, 500, "but the REAL size is reported");
    assert!(b.truncated);
}

/// Exactly at the cap is not truncated. The off-by-one here decides whether a 200-row answer
/// claims to be partial, which would send a caller paging through nothing.
#[test]
fn a_result_set_exactly_at_the_cap_is_complete() {
    let b = bound_rows(rows(200), 200, 0);
    assert_eq!(b.rows.len(), 200);
    assert_eq!(b.total, 200);
    assert!(!b.truncated, "200 of 200 is the whole set");
}

/// `limit == 0` means no cap, matching `iris_doc`'s `max_bytes`. The caller who asks for everything
/// gets everything.
#[test]
fn a_zero_limit_means_every_row() {
    let b = bound_rows(rows(1000), 0, 0);
    assert_eq!(b.rows.len(), 1000);
    assert_eq!(b.total, 1000);
    assert!(!b.truncated);
}

/// The invariant that makes `total` trustworthy: what was shown plus what was withheld is what
/// there was. A `total` that did not satisfy this would be decoration.
#[test]
fn nothing_is_lost_between_shown_and_total() {
    for (n, limit) in [
        (0, 200),
        (1, 200),
        (199, 200),
        (200, 200),
        (201, 200),
        (5000, 200),
    ] {
        let b = bound_rows(rows(n), limit, 0);
        assert_eq!(b.total, n, "total is the input size (n={n})");
        assert!(b.rows.len() <= n, "cannot show more than there was (n={n})");
        assert_eq!(
            b.truncated,
            b.rows.len() < b.total,
            "truncated must mean exactly 'shorter than total' (n={n})"
        );
    }
}

// ─── cell truncation ─────────────────────────────────────────────────────────────────────────

#[test]
fn a_cut_cell_can_never_be_mistaken_for_complete() {
    let long = "x".repeat(DEFAULT_FIELD_CHARS + 120);
    let b = bound_rows(vec![row(&long)], 0, DEFAULT_FIELD_CHARS);
    let cell = b.rows[0]["Name"].as_str().expect("still a string");
    assert!(
        cell.contains(FIELD_CUT_MARKER),
        "the marker rides in the VALUE, not just in a counter: {cell:.80}"
    );
    assert!(
        cell.contains("120 chars]"),
        "and it says how much is missing: {}",
        &cell[cell.len().saturating_sub(40)..]
    );
    assert_eq!(b.fields_truncated, 1);
}

#[test]
fn a_cell_at_or_under_the_cap_is_untouched() {
    let exact = "y".repeat(DEFAULT_FIELD_CHARS);
    let b = bound_rows(vec![row(&exact)], 0, DEFAULT_FIELD_CHARS);
    assert_eq!(b.rows[0]["Name"].as_str(), Some(exact.as_str()));
    assert_eq!(b.fields_truncated, 0, "no cut, so nothing to report");
}

#[test]
fn a_zero_field_cap_leaves_every_cell_whole() {
    let long = "z".repeat(10_000);
    let b = bound_rows(vec![row(&long)], 0, 0);
    assert_eq!(b.rows[0]["Name"].as_str().map(str::len), Some(10_000));
    assert_eq!(b.fields_truncated, 0);
}

/// Characters, not bytes. Slicing UTF-8 at a byte offset panics mid-codepoint, and these cells
/// carry HL7 and patient text where multi-byte characters are the norm. A panic here would take
/// the whole tool call down on data that is merely ordinary.
#[test]
fn a_multibyte_cell_is_cut_on_a_character_boundary() {
    let accented = "é".repeat(DEFAULT_FIELD_CHARS + 50);
    let b = bound_rows(vec![row(&accented)], 0, DEFAULT_FIELD_CHARS);
    let cell = b.rows[0]["Name"].as_str().expect("valid UTF-8 survived");
    let kept: String = cell.chars().take_while(|c| *c == 'é').collect();
    assert_eq!(
        kept.chars().count(),
        DEFAULT_FIELD_CHARS,
        "cut at 500 CHARACTERS, not 500 bytes"
    );
    assert!(cell.contains("50 chars]"), "and 50 were dropped");
}

#[test]
fn fields_truncated_counts_cells_not_rows() {
    let long = "q".repeat(DEFAULT_FIELD_CHARS + 1);
    let two_in_one_row = serde_json::json!({"A": long, "B": long, "C": "short"});
    let b = bound_rows(vec![two_in_one_row], 0, DEFAULT_FIELD_CHARS);
    assert_eq!(b.fields_truncated, 2, "two cells cut in a single row");
}

/// Only strings are capped. A number silently rewritten as a marker string would be far worse than
/// a long value — it would change the TYPE a caller deserialises.
#[test]
fn non_string_cells_are_left_alone() {
    let r = serde_json::json!({"n": 1234567890, "f": 1.5, "b": true, "z": serde_json::Value::Null});
    let b = bound_rows(vec![r.clone()], 0, 1);
    assert_eq!(b.rows[0], r, "untouched: {}", b.rows[0]);
    assert_eq!(b.fields_truncated, 0);
}

#[test]
fn the_default_row_cap_is_a_cap_and_not_a_handful() {
    // Asserts the derivation rather than restating the number: a cap small enough to bite ordinary
    // queries would push callers to limit=0 and defeat the whole item.
    let cap = std::hint::black_box(DEFAULT_ROW_LIMIT);
    assert!(cap >= 100, "cap {cap} must not bite an ordinary query");
    assert!(cap <= 1000, "cap {cap} must actually bound a runaway one");
}

// ─── iris_coverage: a capped list must not move the percentage ───────────────────────────────

use iris_agentic_dev_core::tools::coverage::{report_json, CoverageReport, RoutineCoverage};

fn routine(name: &str, total: usize, hit: usize) -> RoutineCoverage {
    RoutineCoverage {
        name: name.into(),
        routine_lines_total: total,
        routine_lines_hit: hit,
        hits: Default::default(),
    }
}

/// The hazard this item introduces, pinned.
///
/// `report_json` sums `routine_lines_total`/`_hit` over every routine BEFORE the per-routine array
/// is capped. Move the sum after the cap and the percentage becomes a confident wrong number — and
/// a plausible one, which is the dangerous kind. The fixture is built so the two answers cannot be
/// confused: the routines that survive the cap are fully covered, the ones dropped are not covered
/// at all, so a subset-derived percentage would read 100% against a true 66.67%.
#[test]
fn the_percentage_covers_every_routine_not_just_the_returned_ones() {
    let mut routines: Vec<RoutineCoverage> = (0..DEFAULT_ROW_LIMIT)
        .map(|i| routine(&format!("Shown{i}.1"), 10, 10))
        .collect();
    routines.extend((0..100).map(|i| routine(&format!("Dropped{i}.1"), 10, 0)));
    let rep = CoverageReport {
        metrics: vec!["RtnLine".into()],
        routines_monitored: routines.len(),
        routines,
        run_error: None,
        stopped: true,
        refused: None,
    };

    let out = report_json(&rep, "APP");

    // The array is capped...
    assert_eq!(
        out["routines"].as_array().map(Vec::len),
        Some(DEFAULT_ROW_LIMIT),
        "the array is bounded: {}",
        out["routines_total"]
    );
    assert_eq!(out["routines_truncated"], true, "and says so");
    assert_eq!(out["routines_total"], 300, "and reports the real count");

    // ...while the numbers still describe all 300.
    assert_eq!(out["routine_lines_total"], 3000, "every routine counted");
    assert_eq!(out["routine_lines_hit"], 2000, "every hit counted");
    let pct = out["coverage_pct"].as_f64().expect("a percentage");
    assert!(
        (pct - 66.67).abs() < 0.01,
        "the percentage must cover all 300 routines, not the 200 returned (got {pct}) — \
         100.0 here would mean the sum moved after the cap"
    );
}

/// Under the cap, nothing is claimed about truncation.
#[test]
fn a_small_coverage_report_is_not_marked_truncated() {
    let rep = CoverageReport {
        metrics: vec!["RtnLine".into()],
        routines_monitored: 2,
        routines: vec![routine("A.1", 10, 5), routine("B.1", 10, 5)],
        run_error: None,
        stopped: true,
        refused: None,
    };
    let out = report_json(&rep, "APP");
    assert_eq!(out["routines"].as_array().map(Vec::len), Some(2));
    assert_eq!(out["routines_total"], 2);
    assert!(
        out.get("routines_truncated").is_none(),
        "nothing was withheld, so nothing is declared: {out}"
    );
    assert_eq!(out["coverage_pct"], 50.0);
}
