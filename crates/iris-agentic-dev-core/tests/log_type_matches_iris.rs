//! `iris_interop_query(what=logs, log_type=…)` selects the severities IRIS actually numbers.
//!
//! The mapping this pins replaced one that was wrong on every value. IRIS's own definition, read from
//! `%Dictionary.CompiledProperty` for `Ens.Util.Log::Type` (an `Ens.DataType.LogType`):
//!
//! ```text
//! DISPLAYLIST = ,Assert,Error,Warning,Info,Trace,Alert
//!   => Assert 1, Error 2, Warning 3, Info 4, Trace 5, Alert 6
//! ```
//!
//! Confirmed against a live production's own Event Log — 6 rows of
//! `ERROR <Ens>ErrProductionAlreadyRunning` at Type 2, and 29 `$$$LOGINFO` rows at Type 4. Driven
//! through the tool before the fix:
//!
//! | asked for | returned | rows that existed |
//! |---|---|---|
//! | `error` | **0 rows** | 6 |
//! | `warning` | the 6 ERROR rows | 0 |
//! | `info` | **0 rows** | 29 |
//! | `alert` | the 29 INFO rows | 0 |
//! | `trace`, `assert` | all 35, silently unfiltered | — |
//!
//! `log_type=error` finding nothing while six errors sit in the log is the negative-fact shape, in the
//! tool a caller reaches for precisely when something is broken. The DEFAULT (`error,warning`) happened
//! to work — it asked for Types 3 and 2, and 2 is really Error — so the tool was right until a caller
//! named what they wanted.

use iris_agentic_dev_core::tools::interop::log_type_conditions;

/// The mapping, against IRIS's DISPLAYLIST order.
#[test]
fn each_word_selects_the_type_iris_gives_it() {
    for (word, ty) in [
        ("assert", 1),
        ("error", 2),
        ("warning", 3),
        ("info", 4),
        ("trace", 5),
        ("alert", 6),
    ] {
        let got = log_type_conditions(word).expect("a known word");
        assert_eq!(
            got,
            vec![format!("Type = {ty}")],
            "{word} must select Type {ty} — IRIS's DISPLAYLIST is \
             ,Assert,Error,Warning,Info,Trace,Alert"
        );
    }
}

/// The two that were measured against real rows, called out separately: these are the ones whose old
/// values returned the wrong set rather than merely a different number.
#[test]
fn error_and_info_select_the_types_the_live_log_actually_used() {
    // 6 rows of ERROR <Ens>ErrProductionAlreadyRunning sat at Type 2.
    assert_eq!(log_type_conditions("error").unwrap(), vec!["Type = 2"]);
    // 29 $$$LOGINFO rows sat at Type 4.
    assert_eq!(log_type_conditions("info").unwrap(), vec!["Type = 4"]);
    // And the old mapping's answers must no longer be produced for those words.
    assert_ne!(log_type_conditions("error").unwrap(), vec!["Type = 3"]);
    assert_ne!(log_type_conditions("info").unwrap(), vec!["Type = 1"]);
}

/// Comma-separated, in order, and the shipped default keeps working.
#[test]
fn several_words_select_several_types_and_the_default_still_selects_errors() {
    assert_eq!(
        log_type_conditions("error,warning").unwrap(),
        vec!["Type = 2", "Type = 3"],
        "the default must still include Error — it is what the tool returns when nobody asks"
    );
    assert_eq!(
        log_type_conditions(" ERROR , Info ").unwrap(),
        vec!["Type = 2", "Type = 4"],
        "case and surrounding space must not change which severity is meant"
    );
    // An empty spec selects nothing, which the caller's SQL renders as no filter.
    assert!(log_type_conditions("").unwrap().is_empty());
    assert!(log_type_conditions("  ,  ").unwrap().is_empty());
}

/// An unrecognised word must be REFUSED. It used to be dropped, which removed the WHERE clause and
/// returned every row of every type — a filter that looked applied and was not.
#[test]
fn an_unknown_word_is_refused_rather_than_dropped() {
    let err = log_type_conditions("nonsense").expect_err("must refuse");
    assert!(
        err.contains("nonsense"),
        "must name what it rejected: {err}"
    );
    for word in ["assert", "error", "warning", "info", "trace", "alert"] {
        assert!(err.contains(word), "must list {word} as accepted: {err}");
    }
    assert!(
        err.contains("dropped") || err.contains("every log row"),
        "must say what the old behaviour was, so the caller knows a silent pass is not what happened: \
         {err}"
    );
    // A known word alongside an unknown one still refuses: a partly-applied filter is the same trap.
    assert!(log_type_conditions("error,nonsense").is_err());
    // CONTROL: the known set really does pass, so the refusal is discriminating.
    assert!(log_type_conditions("error,warning,info").is_ok());
}
