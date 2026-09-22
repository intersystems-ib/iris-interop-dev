//! #347: a `%Status` chain of two or more errors must arrive COMPLETE at
//! `iris_execute_method`'s `status_text` and at `iris_coverage`'s `refused`.
//!
//! ## Why this target exists at all
//!
//! Both sites carry a value that is not line-oriented through a LINE-ORIENTED marker protocol.
//! `$SYSTEM.Status.GetErrorText(sc)` returns the whole chain CRLF-joined, so a reader whose
//! `strip_prefix` arm captures "the rest of the marker's line" captures element 1 and nothing
//! else; elements 2..n land on later iterations of the same loop, match no arm, and are dropped
//! with no error, no log line and no shortened-output marker.
//!
//! ## Why it is not a fixture test
//!
//! A hand-written `"ERROR #5001: a\r\nERROR #5001: b"` proves the parser handles that one string.
//! It does not prove the protocol carries what IRIS actually emits — which is the thing that
//! broke. So every test here BUILDS the chain on the instance with
//! `$system.Status.AppendStatus`, decodes it with the real `$SYSTEM.Status.GetErrorText`, ships it
//! over the real generator transport, and feeds the device output to the SHIPPED parser.
//!
//! Measured on both containers this was developed against (IRIS 2026.1 and 2025c), a chain of
//! `$system.Status.Error(5001,"first cause")` and `$system.Status.Error(5001,"second cause")`
//! decodes to 51 characters in 2 CRLF-separated pieces:
//!
//! ```text
//! ERROR #5001: first cause<CR><LF>ERROR #5001: second cause
//! ```
//!
//! `ex.DisplayString()` of a `%Exception.StatusException` carrying the same chain is multi-line
//! too (46 characters, 2 pieces) — which is why `IEM_ERROR` is treated here as well as
//! `IEM_STATUS_TEXT`. That is a measurement, not an inference.

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use iris_agentic_dev_core::tools::{coverage, execute_method};

/// The scratch class. Namespaced to this issue so a leftover from a killed run is identifiable.
const PROBE_CLASS: &str = "IrisDevChain347.Probe";

/// The two causes. Asserted individually, so "the whole chain arrived" is checked element by
/// element rather than by a length that a single long first error could also satisfy.
const FIRST: &str = "first cause 347";
const SECOND: &str = "second cause 347";

fn conn() -> Option<(IrisConnection, String)> {
    let host = std::env::var("IRIS_HOST").ok().filter(|h| !h.is_empty())?;
    let port = std::env::var("IRIS_WEB_PORT").unwrap_or_else(|_| "52773".into());
    let user = std::env::var("IRIS_USERNAME").unwrap_or_else(|_| "_SYSTEM".into());
    let pass = std::env::var("IRIS_PASSWORD").unwrap_or_else(|_| "SYS".into());
    let ns = std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".into());
    let base = format!("http://{host}:{port}");
    Some((
        IrisConnection::new(&base, &ns, &user, &pass, DiscoverySource::EnvVar),
        ns,
    ))
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

/// ObjectScript that compiles `PROBE_CLASS` from a stream — no filesystem, so it works on any
/// instance the Atelier transport can reach.
///
/// `TwoErrorChain` returns a REAL two-element `%Status`: two `$system.Status.Error` values joined
/// by `$system.Status.AppendStatus`. Nothing here hard-codes the CRLF that the defect turns on.
fn build_probe_class_code() -> String {
    // Braces are spliced as $CHAR(123)/$CHAR(125) rather than written as literals: this code is
    // itself compiled into a method body by `build_exec_class`, and a `{` inside a string literal
    // there is one brace-counting bug away from truncating the method.
    format!(
        r#"set src=##class(%Stream.TmpCharacter).%New()
set ob=$CHAR(123),cb=$CHAR(125)
do src.WriteLine("Class {cls} Extends %RegisteredObject")
do src.WriteLine(ob)
do src.WriteLine("ClassMethod TwoErrorChain() As %Status")
do src.WriteLine(ob)
do src.WriteLine("  set sc1=$system.Status.Error(5001,""{first}"")")
do src.WriteLine("  set sc2=$system.Status.Error(5001,""{second}"")")
do src.WriteLine("  quit $system.Status.AppendStatus(sc1,sc2)")
do src.WriteLine(cb)
do src.WriteLine(cb)
do src.Rewind()
set sc=$system.OBJ.LoadStream(src,"ck-d")
write $select($system.Status.IsOK(sc):"PROBE_READY",1:"PROBE_FAILED:"_$system.Status.GetErrorText(sc)),!
"#,
        cls = PROBE_CLASS,
        first = FIRST,
        second = SECOND,
    )
}

fn drop_probe_class_code() -> String {
    format!("do $system.OBJ.Delete(\"{PROBE_CLASS}\",\"-d\")\nwrite \"PROBE_DROPPED\",!\n")
}

/// The whole tool path for a method that returns a chained `%Status`: dictionary lookup →
/// `build_invoke_code` → generator → `parse_invoke_output`.
#[test]
#[ignore = "requires live IRIS"]
fn a_two_error_chain_arrives_complete_at_status_text() {
    let Some((c, ns)) = conn() else {
        eprintln!("skip: IRIS_HOST unset");
        return;
    };
    rt().block_on(async {
        let client = IrisConnection::http_client().unwrap();

        let made = c
            .execute_via_generator(&build_probe_class_code(), &ns, &client)
            .await
            .expect("compile the probe class");
        assert!(
            made.contains("PROBE_READY"),
            "the probe class did not compile, so nothing below is a measurement: {made}"
        );

        // The dictionary read the tool itself performs. Asserting `found` is the control: if it
        // were false the invocation would still run (by design, #242) and the meta would be a
        // DEFAULT, which does not request the status decode at all — the test would then pass
        // vacuously with an empty status_text.
        let meta = execute_method::parse_method_meta(
            &c.query(
                execute_method::build_method_meta_query(),
                vec![
                    serde_json::Value::String(PROBE_CLASS.to_string()),
                    serde_json::Value::String("TwoErrorChain".to_string()),
                ],
                &ns,
                &client,
            )
            .await
            .expect("dictionary lookup"),
        );
        assert!(meta.found, "control: the dictionary must see the probe");
        assert!(
            execute_method::returns_status(&meta.return_type),
            "control: the probe must be decoded as a %Status, got {:?}",
            meta.return_type
        );

        let code = execute_method::build_invoke_code(PROBE_CLASS, "TwoErrorChain", &[], &meta);
        let out = c
            .execute_via_generator(&code, &ns, &client)
            .await
            .expect("invoke the probe");
        let r = execute_method::parse_invoke_output(&out);

        let _ = c
            .execute_via_generator(&drop_probe_class_code(), &ns, &client)
            .await;

        assert_eq!(
            r.status_ok,
            Some(false),
            "control: a chained error status must decode as NOT ok, or status_text is empty for \
             an unrelated reason. raw output: {out:?}"
        );
        assert!(
            r.status_text.contains(FIRST),
            "the FIRST chain element is missing, so this is not the truncation under test — \
             something else broke. status_text={:?} raw={out:?}",
            r.status_text
        );
        assert!(
            r.status_text.contains(SECOND),
            "#347: element 2 of the %Status chain was dropped. IRIS decoded the chain \
             CRLF-joined and the marker protocol carried only the first line. \
             status_text={:?} raw={out:?}",
            r.status_text
        );
        assert!(
            !r.status_text_truncated,
            "the whole chain arrived, so nothing may be reported as a short read: {r:?}"
        );
        // The declaration must match what arrived. Without it a chain cut in transit would look
        // like a shorter error message.
        assert_eq!(
            r.status_text_len,
            Some(r.status_text.chars().count()),
            "IRIS's declared length must equal what was reassembled: {r:?}"
        );
    });
}

/// The coverage site, driven with the SHIPPED emitter.
///
/// Making `%Monitor.System.LineByLine.Start` itself fail with a two-element chain is not something a
/// test can arrange, so this builds the chain into `tSC` and then runs
/// `coverage::build_start_failure_report("tSC")` — the exact ObjectScript `build_program` embeds —
/// through the real transport, and parses the device output with the shipped `coverage::parse_output`.
/// A re-implementation of the emitter here would pass while the product stayed broken.
#[test]
#[ignore = "requires live IRIS"]
fn a_two_error_chain_arrives_complete_at_coverage_refused() {
    let Some((c, ns)) = conn() else {
        eprintln!("skip: IRIS_HOST unset");
        return;
    };
    rt().block_on(async {
        let client = IrisConnection::http_client().unwrap();
        let code = format!(
            "set sc1=$system.Status.Error(5001,\"{FIRST}\")\n\
             set sc2=$system.Status.Error(5001,\"{SECOND}\")\n\
             set tSC=$system.Status.AppendStatus(sc1,sc2)\n{}",
            coverage::build_start_failure_report("tSC")
        );
        let out = c
            .execute_via_generator(&code, &ns, &client)
            .await
            .expect("run the start-failure report against a real chain");

        // CONTROL: the chain really was built and decoded, so an empty `refused` below could only be
        // the parser's doing. IRIS joins the two elements with CRLF, which is what the line-oriented
        // reader used to split on.
        assert!(
            out.contains(FIRST) && out.contains(SECOND),
            "the program did not emit both elements, so nothing below is a measurement: {out:?}"
        );

        let rep = coverage::parse_output(&out);
        let refused = rep
            .refused
            .as_ref()
            .unwrap_or_else(|| panic!("a start failure must be reported as a refusal: {out:?}"));
        assert!(
            refused.is_whole(),
            "#347: the refusal arrived incomplete — the declared line count and the lines that \
             reached the parser disagree. refused={refused:?} raw={out:?}"
        );
        assert!(
            refused.text().contains(FIRST),
            "the FIRST element is missing, so this is not the truncation under test: {refused:?}"
        );
        assert!(
            refused.text().contains(SECOND),
            "#347: element 2 of the %Status chain was dropped. GetErrorText returned the whole \
             chain CRLF-joined and only the first line carried the marker. refused={refused:?} \
             raw={out:?}"
        );
    });
}
