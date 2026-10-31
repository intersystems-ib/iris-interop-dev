//! #533 — reading the routine-era application error log, with three outcomes.
//!
//! `iris_debug(action=error_logs)` used to return `{"success": true, "logs": [],
//! "note": "IRIS error log is not accessible via Atelier REST SQL. Set IRIS_CONTAINER to enable
//! docker exec access to the full error log."}` **without issuing a request**. Both halves of the
//! note were false, measured on a live 2026.2 stack: `^ERRORS` is reachable over plain HTTP (it is
//! a global, not a table), and the reporting instance already had `container` set, so the note
//! named a remedy that was already in force. What a model acts on is `success` and `logs`, and
//! those said "this namespace logged no errors" while `^ERRORS` held two entries.
//!
//! [`ErrorLogRead`] is the third case this repo's CLAUDE.md asks for. Two arms cannot express the
//! difference between "the log is empty" and "the log was not read", so the discrimination lives
//! in the type and the compiler enumerates the call sites.
//!
//! ## The layout, measured on 2026.2
//!
//! ```text
//! ^ERRORS(<$H day>, n)                                <- the top node
//! ^ERRORS(<$H day>, n, "*STACK", level, "V", "$ZE")   <- the error text
//! ^ERRORS(<$H day>, n, "*STACK", level, "L")          <- the frame lines
//! ```
//!
//! The subscript shape is the part most likely to drift between versions, so the walk is written
//! against it explicitly and pinned by the fixtures in
//! `tests/an_unread_error_log_is_not_an_empty_one.rs` rather than inferred at run time.

/// The marker the walk writes once it has run to completion, followed by the total it counted.
///
/// This is what makes `success: true` impossible on a path that read nothing.
/// `execute_via_generator` returns `Ok("")` for a SqlProc that died at run time (#362), and an
/// empty string is also what a completed walk over an empty log would produce if the only signal
/// were the entry count — so the terminator, not the count, is the evidence that the read happened.
/// The leading `*` is load-bearing in two ways. It matches the `*STACK` convention the global
/// itself uses, and it keeps the marker out of the population
/// `tests/every_refusal_names_a_remedy.rs` scans: that gate reads every `const … : &str =
/// "SCREAMING_SNAKE"` in `src/tools` as an error code that needs a remedy on record, and a bare
/// `EL_END` was flagged as one. `interop::LOOKUP_VALUE_MARKER` carries a `:` for the same reason.
/// Do not "tidy" the punctuation away.
pub const ERROR_LOG_END: &str = "*EL_END";

/// How many length-prefixed fields each entry occupies on the wire.
const FIELDS_PER_ENTRY: usize = 6;

/// One entry of the application error log, as the walk reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorLogEntry {
    /// The first subscript: a `$HOROLOG` day number.
    pub day: String,
    /// `$ZDate(day, 3)` — empty when the first subscript is not a date this instance can render,
    /// which is reported rather than guessed at.
    pub date: String,
    /// The second subscript: the entry's number within that day.
    pub index: String,
    /// The top node's own value, returned untouched. Its contents differ between versions, so it
    /// is passed through instead of being parsed.
    pub raw: String,
    /// The `$ZERROR` text recorded for the entry.
    pub error: String,
    /// The recorded frame lines, outermost first.
    pub stack: Vec<String>,
}

/// What a read of the error log learned. Three outcomes, never two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorLogRead {
    /// The log was read and holds these entries. `total` is counted independently of how many
    /// were written, so a bounded page says how much it withheld.
    Entries {
        entries: Vec<ErrorLogEntry>,
        total: usize,
    },
    /// The log was read, ran to completion, and holds nothing.
    ReadAndEmpty,
    /// The read did not happen, or did not finish. Carries why.
    Unreadable(String),
}

/// Walk `^ERRORS` in the current namespace, writing at most `limit` entries plus a terminator.
///
/// Curly-brace `For` blocks, not dot-continuation: the generator wraps this body in a `Try`, and
/// dot-continuation inside one is unreliable (see `tools::global::build_list_code`).
///
/// `$ZDate` is guarded by a pattern match rather than a nested `Try`, so a first subscript that is
/// not a day number yields an empty `date` instead of aborting the whole walk — one unexpected
/// subscript must not cost the caller every entry.
pub fn build_error_log_code(limit: usize) -> String {
    // A limit of 0 would make "counted some, wrote none" reachable through a legitimate request,
    // and that shape is reported as a failed read by `parse_error_log_output`.
    let lim = limit.clamp(1, 1000);
    format!(
        r#" Set lim = {lim}
 Set cnt = 0
 Set emitted = 0
 Set day = ""
 For {{
   Set day = $Order(^ERRORS(day))
   Quit:day=""
   Set n = ""
   For {{
     Set n = $Order(^ERRORS(day,n))
     Quit:n=""
     Set cnt = cnt + 1
     If emitted < lim {{
       Set raw = $Get(^ERRORS(day,n))
       Set ze = ""
       Set stk = ""
       Set lv = ""
       For {{
         Set lv = $Order(^ERRORS(day,n,"*STACK",lv))
         Quit:lv=""
         If ze = "" {{ Set ze = $Get(^ERRORS(day,n,"*STACK",lv,"V","$ZE")) }}
         Set ln = $Get(^ERRORS(day,n,"*STACK",lv,"L"))
         If ln '= "" {{ Set stk = $Select(stk="":ln,1:stk_$Char(10)_ln) }}
       }}
       Set dt = $Select((day?1.6N)&&(day>0)&&(day<300000):$ZDate(day,3),1:"")
       Write $Length(day)_":"_day
       Write $Length(dt)_":"_dt
       Write $Length(n)_":"_n
       Write $Length(raw)_":"_raw
       Write $Length(ze)_":"_ze
       Write $Length(stk)_":"_stk
       Set emitted = emitted + 1
     }}
   }}
 }}
 Write $Length("{ERROR_LOG_END}")_":{ERROR_LOG_END}"
 Write $Length(cnt)_":"_cnt,$Char(10)"#
    )
}

/// Decode [`build_error_log_code`]'s output into one of three outcomes.
///
/// Every way the reply can fall short of a completed walk is [`ErrorLogRead::Unreadable`] carrying
/// the reason. Nothing defaults: a missing field would otherwise produce a complete-looking entry
/// with one empty member, and a missing terminator a complete-looking log that happens to be short.
pub fn parse_error_log_output(raw: &str) -> ErrorLogRead {
    if let Some(abort) = crate::tools::runtime_abort_line(raw) {
        return ErrorLogRead::Unreadable(format!(
            "the walk over ^ERRORS aborted: {abort}. Nothing was read, so nothing is known about \
             the log."
        ));
    }
    let fields = match crate::tools::interop::parse_length_prefixed_entries(raw) {
        Ok(v) => v,
        Err(why) => {
            return ErrorLogRead::Unreadable(format!(
                "the reply from the ^ERRORS walk could not be decoded: {why}"
            ))
        }
    };
    if fields.len() < 2 || fields[fields.len() - 2] != ERROR_LOG_END {
        return ErrorLogRead::Unreadable(format!(
            "the reply carried no {ERROR_LOG_END} terminator, so the walk over ^ERRORS did not \
             run to completion — {} field(s) arrived. An empty or truncated reply is a failed \
             read, not an empty log.",
            fields.len()
        ));
    }
    let total: usize = match fields[fields.len() - 1].parse() {
        Ok(n) => n,
        Err(_) => {
            return ErrorLogRead::Unreadable(format!(
                "the {ERROR_LOG_END} terminator carried {:?} instead of a count, so how much the \
                 walk found is unknown",
                fields[fields.len() - 1]
            ))
        }
    };
    let body = &fields[..fields.len() - 2];
    if !body.len().is_multiple_of(FIELDS_PER_ENTRY) {
        return ErrorLogRead::Unreadable(format!(
            "the walk wrote {} field(s), which is not a whole number of {FIELDS_PER_ENTRY}-field \
             entries, so at least one entry arrived in part",
            body.len()
        ));
    }
    let entries: Vec<ErrorLogEntry> = body
        .chunks(FIELDS_PER_ENTRY)
        .map(|c| ErrorLogEntry {
            day: c[0].clone(),
            date: c[1].clone(),
            index: c[2].clone(),
            raw: c[3].clone(),
            error: c[4].clone(),
            stack: if c[5].is_empty() {
                Vec::new()
            } else {
                c[5].split('\n').map(str::to_string).collect()
            },
        })
        .collect();
    if entries.len() > total {
        return ErrorLogRead::Unreadable(format!(
            "the walk returned {} entries but counted {total}; the reply is incoherent, not a \
             short log",
            entries.len()
        ));
    }
    if entries.is_empty() {
        if total == 0 {
            return ErrorLogRead::ReadAndEmpty;
        }
        return ErrorLogRead::Unreadable(format!(
            "the walk counted {total} entries in ^ERRORS and returned none, so the read did not \
             deliver what it found"
        ));
    }
    ErrorLogRead::Entries { entries, total }
}

/// Render an [`ErrorLogRead::Entries`] / [`ErrorLogRead::ReadAndEmpty`] payload. The caller owns
/// the refusal, because a refusal must not carry a `logs` key at all.
pub fn read_json(read: &ErrorLogRead, namespace: &str) -> Option<serde_json::Value> {
    let (entries, total) = match read {
        ErrorLogRead::Entries { entries, total } => (entries.as_slice(), *total),
        ErrorLogRead::ReadAndEmpty => (&[][..], 0usize),
        ErrorLogRead::Unreadable(_) => return None,
    };
    let logs: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "day": e.day,
                "date": e.date,
                "index": e.index,
                "raw": e.raw,
                "error": e.error,
                "stack": e.stack,
            })
        })
        .collect();
    let mut v = serde_json::json!({
        "success": true,
        // The discriminator a caller branches on. `logs: []` alone cannot say whether the log was
        // read, which is the whole of #533.
        "log_read": true,
        "global": "^ERRORS",
        "namespace": namespace,
        "logs": logs,
        "count": logs.len(),
        "total_count": total,
        "truncated": total > logs.len(),
    });
    if logs.is_empty() {
        v["note"] = serde_json::json!(format!(
            "^ERRORS was read in namespace '{namespace}' and holds no entries. This is a read \
             that found nothing, not a read that failed — `log_read` is true."
        ));
    } else if total > logs.len() {
        v["note"] = serde_json::json!(format!(
            "{} of {total} entries returned; raise `limit` for the rest.",
            logs.len()
        ));
    }
    Some(v)
}

/// The error code every failed read of an error log answers with.
///
/// Deliberately NOT used as the first argument of `fail_with` at the call sites: that argument is
/// scanned for a string LITERAL by `tests/every_refusal_names_a_remedy.rs`, and a const path
/// returns `None` there — the code would slip past the gate that requires a remedy on record for
/// it. The literal is spelled out at each site; this const exists so tests can name it.
pub const ERROR_LOG_UNAVAILABLE: &str = "ERROR_LOG_UNAVAILABLE";

/// The same defect over SQL — #533's siblings.
///
/// `debug_capture_packet` and `debug_get_error_logs` both answered `{"success": true,
/// "errors"/"logs": [], "note": "%SYSTEM.Error not available on this IRIS edition"}` when the
/// `SELECT` failed. "The table is not there" is a fact about the SERVER; "there are no errors" is
/// a fact about the WORLD, and only the first one had been established. Both are a failed read.
pub fn sql_error_log_outcome(err: &str) -> ErrorLogRead {
    let absent = err.contains("SQLCODE: -30")
        || (err.contains("Table") && err.contains("not found"))
        || err.contains("does not exist");
    if absent {
        return ErrorLogRead::Unreadable(format!(
            "%SYSTEM.Error could not be read on this IRIS: {err}. The table being absent says \
             nothing about whether errors were logged — read the routine-era log with \
             iris_debug(action=error_logs), which walks ^ERRORS."
        ));
    }
    ErrorLogRead::Unreadable(format!("%SYSTEM.Error could not be read: {err}"))
}

/// The reason inside an [`ErrorLogRead::Unreadable`], for a caller rendering the refusal.
pub fn unreadable_reason(read: &ErrorLogRead) -> Option<&str> {
    match read {
        ErrorLogRead::Unreadable(why) => Some(why.as_str()),
        _ => None,
    }
}
