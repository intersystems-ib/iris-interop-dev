//! #24 item 064: ObjectScript line coverage for a `%UnitTest` run, via
//! `%Monitor.System.LineByLine`.
//!
//! EVERYTHING BELOW WAS READ OUT OF THE CLASS SOURCE ON IRIS FOR HEALTH 2026.1, NOT ASSUMED.
//! The signatures came from `%Dictionary.CompiledMethod`, the metric names from a live
//! `GetMetrics(2)`, and the result semantics from the `ResultExecute`/`ResultFetch` bodies. Each of
//! the following would have been guessed wrong:
//!
//! 1. `Routine`, `Metric` and `Process` are **%List**, not strings — `Start` rejects a plain string
//!    with `InvalidParameter` (`'$lv(Routine)`). So every one is `$LISTBUILD`.
//! 2. Wildcards are **trailing `*` only**, and internally the name gets `.obj` appended and is
//!    resolved through `$$LIST^%R`. A class `Pkg.Cls` compiles to routines `Pkg.Cls.1` …, so
//!    covering a class means monitoring `Pkg.Cls*` — the bare class name matches nothing.
//! 3. `ResultExecute` finds the routine by **exact equality** against the monitored names
//!    (`i Routine=$zu(84,16,2,rtn)`). The wildcard you passed to `Start` will NOT match here, so the
//!    results must be fetched with the names `GetRoutineName()` gives back.
//! 4. A name that is not being monitored yields **zero rows AND `$$$OK`** — a silent empty that is
//!    indistinguishable from "monitored, nothing executed" unless you enumerate the routines first.
//!    This is why the generated program reports `routines_monitored` separately from the rows.
//! 5. Row N is line N: `f line=0:1:(l-1) s $$$ISCQUERYTEMP(Index,line+1)=list`.
//! 6. Counter order within a row is `GetMetrics(1)` — the *currently monitored* metrics, in the
//!    order the monitor holds them, which is not necessarily the order requested.
//! 7. `Time` and `TotalTime` are already divided by 1e6 into seconds by `ResultExecute`; the count
//!    metrics are raw. So a consumer must not scale them again.
//!
//! WHY IT IS SCOPED TO ONE PROCESS. The class doc carries an explicit warning: *"Starting the
//! line-by-line monitor will enable the collection of statistics for every line of code executed by
//! the selected routines and processes. This can have a major impact on the performance of a system,
//! and it is recommended that you do this only on a 'test' system."* The monitor is also
//! instance-wide and exclusive — a second `Start` returns `MonitorAlreadyRunning`.
//!
//! `%UnitTest.Manager.RunTest` runs in the CALLING process, so the whole cycle — start, run, collect,
//! stop — fits in one program and `Process` can be `$LISTBUILD($JOB)`. That confines the statistics
//! to this one job instead of every process on the instance. It is the difference between a tool that
//! is safe to run on a shared instance and one that is not.

use serde::{Deserialize, Serialize};

/// The coverage metric. `RtnLine` is "lines of ObjectScript" — the execution count per line.
pub const LINE_METRIC: &str = "RtnLine";

/// Metrics this tool will pass through. Anything else is refused rather than forwarded, because
/// `Start` validates against its own table and returns a bare `Unknown metric: X`, and because the
/// per-line semantics of the block-IO counters are not what a caller asking for "coverage" means.
pub const ALLOWED_METRICS: &[&str] = &[LINE_METRIC, "Time", "TotalTime"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageRequest {
    /// The `%UnitTest` spec, exactly as `iris_test` takes it.
    pub test_spec: String,
    /// Routine patterns to monitor. A class `Pkg.Cls` must be given as `Pkg.Cls*`; see `normalise`.
    pub routines: Vec<String>,
    /// Extra metrics beyond `RtnLine`.
    pub metrics: Vec<String>,
}

/// Why a request was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Invalid {
    NoRoutines,
    /// A pattern that cannot match a compiled routine, with the reason.
    BadRoutine(String, String),
    UnknownMetric(String),
    EmptyTestSpec,
}

impl Invalid {
    pub fn message(&self) -> String {
        match self {
            Invalid::NoRoutines => "Pass at least one routine pattern. To cover a CLASS, pass \
                 'Pkg.Cls*' — a class compiles to routines named 'Pkg.Cls.1', 'Pkg.Cls.2', … so the \
                 bare class name matches no routine at all."
                .into(),
            Invalid::BadRoutine(r, why) => format!("Routine pattern '{r}' is not usable: {why}"),
            Invalid::UnknownMetric(m) => format!(
                "Unknown metric '{m}'. This tool accepts {}. IRIS supports many more (see \
                 GetMetrics(3)), but their per-line meaning is not line coverage.",
                ALLOWED_METRICS.join(", ")
            ),
            Invalid::EmptyTestSpec => "Pass the %UnitTest spec to run while monitoring — the same \
                 value iris_test takes."
                .into(),
        }
    }
}

/// Reject what IRIS would reject, before spending a round trip — and reject the one thing IRIS
/// would silently ACCEPT and return nothing for: a bare class name.
pub fn validate(req: &CoverageRequest) -> Result<(), Invalid> {
    if req.test_spec.trim().is_empty() {
        return Err(Invalid::EmptyTestSpec);
    }
    if req.routines.is_empty() {
        return Err(Invalid::NoRoutines);
    }
    for r in &req.routines {
        let t = r.trim();
        if t.is_empty() {
            return Err(Invalid::BadRoutine(r.clone(), "it is blank".into()));
        }
        // `Start` appends ".obj" and resolves through $$LIST^%R, which only understands a trailing
        // asterisk. An interior '*' silently matches nothing.
        if let Some(i) = t.find('*') {
            if i != t.len() - 1 {
                return Err(Invalid::BadRoutine(
                    r.clone(),
                    "an asterisk is only a wildcard as the LAST character; anywhere else it \
                     matches nothing and the monitor reports zero rows with no error"
                        .into(),
                ));
            }
        }
        if t.contains('"') || t.contains(',') {
            return Err(Invalid::BadRoutine(
                r.clone(),
                "a quote or comma cannot appear in a routine name".into(),
            ));
        }
    }
    for m in &req.metrics {
        if !ALLOWED_METRICS.iter().any(|a| a.eq_ignore_ascii_case(m)) {
            return Err(Invalid::UnknownMetric(m.clone()));
        }
    }
    Ok(())
}

/// The metric list actually sent: `RtnLine` first and always, then any extras, de-duplicated
/// case-insensitively. `RtnLine` is not optional — without it there is no coverage, only timings.
pub fn metric_list(req: &CoverageRequest) -> Vec<String> {
    let mut out = vec![LINE_METRIC.to_string()];
    for m in &req.metrics {
        let canon = ALLOWED_METRICS
            .iter()
            .find(|a| a.eq_ignore_ascii_case(m))
            .map(|a| a.to_string())
            .unwrap_or_else(|| m.clone());
        if !out.iter().any(|e| e.eq_ignore_ascii_case(&canon)) {
            out.push(canon);
        }
    }
    out
}

/// A `$LISTBUILD(...)` of quoted strings.
fn list_build(items: &[String]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|s| crate::objectscript::os_str_expr(s.trim()))
        .collect();
    format!("$LISTBUILD({})", parts.join(","))
}

/// The program. One process, one job, and `Stop()` on every path.
///
/// Structured so the monitor is stopped even when the test throws: the run is wrapped in
/// `try/catch`, and `Stop()` sits after the catch rather than inside the happy path. Leaving a
/// monitor running is not a cosmetic failure — it degrades the whole instance and blocks the next
/// `Start` with `MonitorAlreadyRunning`.
pub fn build_program(req: &CoverageRequest) -> String {
    let routines = list_build(&req.routines);
    let metrics = list_build(&metric_list(req));
    let spec = crate::objectscript::os_str_expr(req.test_spec.trim());
    format!(
        r#"set $ZTRAP=""
set tStart=##class(%Monitor.System.LineByLine).GetRoutineCount()
if tStart>0 {{
  write "COVERAGE_REFUSED:monitor already running, "_tStart_" routines",!
  quit
}}
set tSC=##class(%Monitor.System.LineByLine).Start({routines},{metrics},$LISTBUILD($JOB))
if '$SYSTEM.Status.IsOK(tSC) {{
  write "COVERAGE_START_FAILED:"_$SYSTEM.Status.GetErrorText(tSC),!
  quit
}}
set tRunErr=""
try {{
  do ##class(%UnitTest.Manager).RunTest({spec},"/noload/nodelete")
}} catch ex {{
  set tRunErr=ex.DisplayString()
}}
do ##class(%Monitor.System.LineByLine).Pause()
set tMetrics=##class(%Monitor.System.LineByLine).GetMetrics(1)
set tCount=##class(%Monitor.System.LineByLine).GetRoutineCount()
write "COVERAGE_METRICS:"_tMetrics,!
write "COVERAGE_ROUTINES:"_tCount,!
if tRunErr'="" write "COVERAGE_RUN_ERROR:"_tRunErr,!
for i=1:1:tCount {{
  set tRtn=##class(%Monitor.System.LineByLine).GetRoutineName(i)
  set tRS=##class(%ResultSet).%New("%Monitor.System.LineByLine:Result")
  do tRS.Execute(tRtn)
  set tLine=0,tHit=0,tHits=""
  while tRS.Next() {{
    set tLine=tLine+1
    set tRow=tRS.GetData(1)
    set tN=$LIST(tRow,1)
    if tN>0 {{
      set tHit=tHit+1
      set tHits=tHits_$SELECT(tHits="":"",1:" ")_tLine_":"_tN
    }}
  }}
  kill tRS
  write "COVERAGE_RTN:"_tRtn_"|"_tLine_"|"_tHit,!
  write "COVERAGE_HITS:"_tHits,!
}}
do ##class(%Monitor.System.LineByLine).Stop()
set tLeft=##class(%Monitor.System.LineByLine).GetRoutineCount()
write "COVERAGE_STOPPED:"_tLeft,!
"#
    )
}

/// One routine's coverage.
#[derive(Debug, PartialEq, Serialize)]
pub struct RoutineCoverage {
    pub name: String,
    /// Lines the MONITOR tracks for this compiled routine. **Not** the number of executable source
    /// lines in the `.cls` — a class becomes one or more generated routines, so this denominator is
    /// compiled-routine lines. Named explicitly so a caller cannot mistake it for source coverage.
    pub routine_lines_total: usize,
    pub routine_lines_hit: usize,
    /// Hit counts, line number → times executed. Zero-count lines are omitted; the total is
    /// `routine_lines_total`, so the misses are derivable without carrying them.
    pub hits: std::collections::BTreeMap<usize, u64>,
}

impl RoutineCoverage {
    /// Percentage over `routine_lines_total`. `None` when the monitor tracked no lines at all —
    /// 0/0 is not 0%, and reporting it as 0% would read as "nothing was covered".
    pub fn pct(&self) -> Option<f64> {
        if self.routine_lines_total == 0 {
            return None;
        }
        Some((self.routine_lines_hit as f64) * 100.0 / (self.routine_lines_total as f64))
    }
}

/// What the program reported.
#[derive(Debug, PartialEq, Serialize)]
pub struct CoverageReport {
    pub metrics: Vec<String>,
    pub routines_monitored: usize,
    pub routines: Vec<RoutineCoverage>,
    /// Set when the test itself threw. The coverage collected up to that point is still returned —
    /// a partial measurement is useful, but it must be labelled.
    pub run_error: Option<String>,
    /// `Stop()` ran and the monitor is idle. False is a REAL problem worth surfacing: a monitor left
    /// running degrades the instance and blocks the next Start.
    pub stopped: bool,
    /// The refusal, when the program declined to start.
    pub refused: Option<String>,
}

/// Parse the program's stdout. Tolerant of interleaved `%UnitTest` output, which writes freely to the
/// same device — every line this cares about is prefixed, and anything else is ignored.
pub fn parse_output(out: &str) -> CoverageReport {
    let mut rep = CoverageReport {
        metrics: vec![],
        routines_monitored: 0,
        routines: vec![],
        run_error: None,
        stopped: false,
        refused: None,
    };
    let mut pending: Option<(String, usize, usize)> = None;
    for line in out.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("COVERAGE_REFUSED:") {
            rep.refused = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("COVERAGE_START_FAILED:") {
            rep.refused = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("COVERAGE_METRICS:") {
            rep.metrics = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        } else if let Some(v) = l.strip_prefix("COVERAGE_ROUTINES:") {
            rep.routines_monitored = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("COVERAGE_RUN_ERROR:") {
            rep.run_error = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("COVERAGE_RTN:") {
            let mut it = v.split('|');
            let name = it.next().unwrap_or("").to_string();
            let total = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
            let hit = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
            pending = Some((name, total, hit));
        } else if let Some(v) = l.strip_prefix("COVERAGE_HITS:") {
            if let Some((name, total, hit)) = pending.take() {
                let mut hits = std::collections::BTreeMap::new();
                for pair in v.split_whitespace() {
                    if let Some((ln, n)) = pair.split_once(':') {
                        if let (Ok(ln), Ok(n)) = (ln.parse::<usize>(), n.parse::<u64>()) {
                            hits.insert(ln, n);
                        }
                    }
                }
                rep.routines.push(RoutineCoverage {
                    name,
                    routine_lines_total: total,
                    routine_lines_hit: hit,
                    hits,
                });
            }
        } else if let Some(v) = l.strip_prefix("COVERAGE_STOPPED:") {
            rep.stopped = v.trim() == "0";
        }
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(spec: &str, routines: &[&str], metrics: &[&str]) -> CoverageRequest {
        CoverageRequest {
            test_spec: spec.into(),
            routines: routines.iter().map(|s| s.to_string()).collect(),
            metrics: metrics.iter().map(|s| s.to_string()).collect(),
        }
    }

    // ── validation: reject what IRIS rejects, and the one thing it silently accepts ──────────

    #[test]
    fn a_usable_request_passes() {
        assert_eq!(
            validate(&req("MyApp.Tests", &["MyApp.BS.Foo*"], &[])),
            Ok(())
        );
    }

    /// The trap from the class source: `$$LIST^%R` only understands a TRAILING asterisk. An interior
    /// one matches nothing and the monitor then reports zero rows **with no error**, so the caller
    /// sees an empty coverage report and no reason for it.
    #[test]
    fn an_interior_asterisk_is_refused_because_it_would_silently_match_nothing() {
        let e = validate(&req("T", &["MyApp.*.Foo"], &[])).expect_err("must refuse");
        match &e {
            Invalid::BadRoutine(r, _) => assert_eq!(r, "MyApp.*.Foo"),
            other => panic!("wrong variant: {other:?}"),
        }
        let m = e.message();
        assert!(m.contains("LAST character"), "{m}");
        assert!(
            m.contains("zero rows with no error"),
            "the message must say WHY silence is the symptom: {m}"
        );
    }

    /// A trailing asterisk is the supported form and must NOT be refused.
    #[test]
    fn a_trailing_asterisk_is_accepted() {
        assert_eq!(validate(&req("T", &["MyApp.BS.Foo*"], &[])), Ok(()));
    }

    #[test]
    fn no_routines_is_refused_and_the_message_explains_the_class_to_routine_mapping() {
        let e = validate(&req("T", &[], &[])).expect_err("must refuse");
        assert_eq!(e, Invalid::NoRoutines);
        let m = e.message();
        // The single most likely mistake: passing the class name. Say so up front.
        assert!(m.contains("'Pkg.Cls*'"), "{m}");
        assert!(
            m.contains("Pkg.Cls.1"),
            "must show what a class compiles to: {m}"
        );
    }

    #[test]
    fn an_empty_test_spec_is_refused() {
        assert_eq!(
            validate(&req("   ", &["A*"], &[])),
            Err(Invalid::EmptyTestSpec)
        );
    }

    #[test]
    fn an_unsupported_metric_is_refused_with_the_allowed_set() {
        let e = validate(&req("T", &["A*"], &["GloRef"])).expect_err("must refuse");
        assert_eq!(e, Invalid::UnknownMetric("GloRef".into()));
        let m = e.message();
        assert!(m.contains("RtnLine"), "{m}");
        // GloRef is a REAL IRIS metric — the refusal must not imply it does not exist.
        assert!(
            m.contains("IRIS supports many more"),
            "must not deny that the metric exists: {m}"
        );
    }

    #[test]
    fn the_supported_extras_are_accepted_case_insensitively() {
        assert_eq!(validate(&req("T", &["A*"], &["time", "TOTALTIME"])), Ok(()));
    }

    // ── the metric list ─────────────────────────────────────────────────────────────────────

    /// Without `RtnLine` there is no coverage, only timings — so it is always first and always
    /// present, whatever the caller asked for.
    #[test]
    fn rtnline_is_always_present_and_first() {
        assert_eq!(metric_list(&req("T", &["A*"], &[])), vec!["RtnLine"]);
        assert_eq!(
            metric_list(&req("T", &["A*"], &["Time"])),
            vec!["RtnLine", "Time"]
        );
    }

    #[test]
    fn a_caller_who_asks_for_rtnline_does_not_get_it_twice() {
        assert_eq!(
            metric_list(&req("T", &["A*"], &["rtnline", "Time"])),
            vec!["RtnLine", "Time"],
            "de-duplicated case-insensitively, and canonicalised to the spelling IRIS expects"
        );
    }

    // ── the generated program ───────────────────────────────────────────────────────────────

    #[test]
    fn the_program_passes_lists_not_strings() {
        let p = build_program(&req("MyApp.Tests", &["MyApp.BS.Foo*"], &["Time"]));
        // `Start` rejects a plain string with InvalidParameter ('$lv(Routine)), so every argument
        // must be a $LISTBUILD — including the single-element cases.
        assert!(
            p.contains(r#"$LISTBUILD("MyApp.BS.Foo*")"#),
            "routines must be a list: {p}"
        );
        assert!(
            p.contains(r#"$LISTBUILD("RtnLine","Time")"#),
            "metrics must be a list, RtnLine first: {p}"
        );
    }

    /// The safety property that makes this runnable on a shared instance: statistics are collected
    /// for THIS job only, not every process. `%UnitTest.Manager.RunTest` runs in the calling process,
    /// which is what makes that possible.
    #[test]
    fn collection_is_scoped_to_this_process() {
        let p = build_program(&req("T", &["A*"], &[]));
        assert!(
            p.contains("$LISTBUILD($JOB)"),
            "must scope to the current job: {p}"
        );
    }

    /// Leaving the monitor running degrades the whole instance and blocks the next Start with
    /// MonitorAlreadyRunning. So the run is wrapped and Stop() is NOT inside the happy path.
    #[test]
    fn the_monitor_is_stopped_even_when_the_test_throws() {
        let p = build_program(&req("T", &["A*"], &[]));
        let stop = p.find(").Stop()").expect("must stop");
        let catch = p.find("catch ex").expect("must catch");
        assert!(
            catch < stop,
            "Stop() must come after the catch, not inside the success path: {p}"
        );
        assert!(p.contains("set tRunErr=ex.DisplayString()"), "{p}");
    }

    /// It must not clobber a monitor someone else started — the monitor is instance-wide and
    /// exclusive, so a blind Start would either fail or interfere.
    #[test]
    fn an_already_running_monitor_is_refused_before_starting() {
        let p = build_program(&req("T", &["A*"], &[]));
        let guard = p.find("COVERAGE_REFUSED").expect("must guard");
        let start = p.find(").Start(").expect("must start");
        assert!(guard < start, "the guard must precede Start: {p}");
    }

    /// Results must be fetched with the names the monitor reports, because `ResultExecute` matches
    /// by EXACT equality — the wildcard passed to Start would match nothing here.
    #[test]
    fn results_are_fetched_by_the_resolved_routine_name_not_the_wildcard() {
        let p = build_program(&req("T", &["MyApp.BS.Foo*"], &[]));
        assert!(p.contains("GetRoutineName(i)"), "{p}");
        assert!(
            p.contains("do tRS.Execute(tRtn)"),
            "must execute with the resolved name: {p}"
        );
        assert!(
            !p.contains(r#"Execute("MyApp.BS.Foo*")"#),
            "must NOT pass the wildcard to the result query: {p}"
        );
    }

    #[test]
    fn a_quote_in_the_test_spec_is_escaped_not_interpolated() {
        let p = build_program(&req("A\"B", &["A*"], &[]));
        // os_str_expr doubles DOUBLE quotes.
        assert!(p.contains(r#""A""B""#), "{p}");
    }

    // ── parsing ─────────────────────────────────────────────────────────────────────────────

    const GOOD: &str = r#"
COVERAGE_METRICS:RtnLine,Time
COVERAGE_ROUTINES:2
some %UnitTest chatter that must be ignored
COVERAGE_RTN:MyApp.BS.Foo.1|10|3
COVERAGE_HITS:2:1 5:4 9:1
COVERAGE_RTN:MyApp.BS.Foo.2|4|0
COVERAGE_HITS:
COVERAGE_STOPPED:0
"#;

    #[test]
    fn a_good_run_parses_both_routines_and_the_hit_map() {
        let r = parse_output(GOOD);
        assert_eq!(r.metrics, vec!["RtnLine", "Time"]);
        assert_eq!(r.routines_monitored, 2);
        assert_eq!(r.routines.len(), 2);
        assert!(r.stopped, "STOPPED:0 means the monitor is idle");
        assert!(r.run_error.is_none());
        assert!(r.refused.is_none());

        let a = &r.routines[0];
        assert_eq!(a.name, "MyApp.BS.Foo.1");
        assert_eq!(a.routine_lines_total, 10);
        assert_eq!(a.routine_lines_hit, 3);
        assert_eq!(a.hits.get(&5), Some(&4), "line 5 ran 4 times");
        assert_eq!(a.hits.len(), 3, "only non-zero lines are carried");
        assert_eq!(a.pct(), Some(30.0));
    }

    /// A routine with lines but no hits is 0%, which is a real answer and must not be confused with
    /// the no-lines case below.
    #[test]
    fn a_routine_with_no_hits_is_zero_percent_not_unknown() {
        let r = parse_output(GOOD);
        let b = &r.routines[1];
        assert_eq!(b.routine_lines_hit, 0);
        assert!(b.hits.is_empty());
        assert_eq!(b.pct(), Some(0.0));
    }

    /// 0 of 0 is NOT 0%. Reporting it as 0% would read as "nothing was covered" when the truth is
    /// "the monitor tracked no lines here" — a different problem, usually a bad pattern.
    #[test]
    fn no_tracked_lines_is_none_rather_than_zero_percent() {
        let r = parse_output("COVERAGE_RTN:X.1|0|0\nCOVERAGE_HITS:\nCOVERAGE_STOPPED:0\n");
        assert_eq!(r.routines[0].pct(), None);
    }

    /// The monitor NOT stopping is a real failure worth surfacing, so it must not default to true.
    #[test]
    fn a_monitor_left_running_is_reported_as_not_stopped() {
        let r = parse_output("COVERAGE_RTN:X.1|2|1\nCOVERAGE_HITS:1:1\nCOVERAGE_STOPPED:3\n");
        assert!(!r.stopped, "3 routines still monitored is not stopped");
        // and the absence of the line entirely is also not-stopped
        let r2 = parse_output("COVERAGE_RTN:X.1|2|1\nCOVERAGE_HITS:1:1\n");
        assert!(!r2.stopped, "no STOPPED line must not read as stopped");
    }

    /// A thrown test still yields the coverage gathered so far — labelled, not silently partial.
    #[test]
    fn a_failed_test_run_keeps_the_partial_coverage_and_names_the_error() {
        let r = parse_output(
            "COVERAGE_ROUTINES:1\nCOVERAGE_RUN_ERROR:<UNDEFINED> zzz\nCOVERAGE_RTN:X.1|5|2\n\
             COVERAGE_HITS:1:1 2:1\nCOVERAGE_STOPPED:0\n",
        );
        assert_eq!(r.run_error.as_deref(), Some("<UNDEFINED> zzz"));
        assert_eq!(r.routines[0].routine_lines_hit, 2, "partial data is kept");
        assert!(r.stopped);
    }

    #[test]
    fn a_refusal_is_surfaced_and_carries_no_invented_coverage() {
        let r = parse_output("COVERAGE_REFUSED:monitor already running, 4 routines\n");
        assert_eq!(
            r.refused.as_deref(),
            Some("monitor already running, 4 routines")
        );
        assert!(r.routines.is_empty(), "must not invent rows");
        assert!(
            !r.stopped,
            "it never started, so it is not 'stopped' either"
        );
    }

    #[test]
    fn a_start_failure_is_surfaced_as_a_refusal() {
        let r = parse_output("COVERAGE_START_FAILED:ERROR #6060: Unknown metric: Nope\n");
        assert!(
            r.refused.as_deref().unwrap_or_default().contains("#6060"),
            "{r:?}"
        );
    }

    /// %UnitTest writes freely to the same device. Nothing it prints may be mistaken for a record —
    /// including a line that merely mentions one of the prefixes.
    #[test]
    fn interleaved_unittest_output_is_ignored() {
        let r = parse_output(
            "ok\nCOVERAGE_METRICS:RtnLine\n  see COVERAGE_RTN: in the docs\n\
             COVERAGE_RTN:X.1|3|1\nCOVERAGE_HITS:2:1\nCOVERAGE_STOPPED:0\n",
        );
        assert_eq!(
            r.routines.len(),
            1,
            "a prose mention must not become a routine: {r:?}"
        );
        assert_eq!(r.routines[0].name, "X.1");
    }
}
