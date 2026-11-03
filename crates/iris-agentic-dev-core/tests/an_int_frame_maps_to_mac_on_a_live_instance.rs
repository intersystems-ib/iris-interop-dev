//! #534 e2e: a frame from a REAL error, mapped back to the `.MAC` line it came from, by a server
//! talking to a real instance.
//!
//! The unit tests pin the arithmetic against fixture text, and the wiremock tests pin the wiring
//! against a mocked Atelier. Neither can show that **IRIS agrees** — that the `.int` it generates
//! really drops the lines the rule says it drops, that `$ZERROR` really names the offset this code
//! counts, and that a routine put through `iris_doc` is really the routine the frame resolves
//! against. Only an instance can answer those, and they are exactly #534's e2e acceptance.
//!
//! ## Two spawns, not one, and the measured reason
//!
//! A routine compiled by `iris_doc(put, compile=true)` is **not callable from the process that
//! compiled it**: measured on 2026.1, `Do MAIN^IIDMacroE2E` in the same MCP session as the put
//! answered `<NOROUTINE>RunUser+1^IrisDevTmp.Run….1 *IIDMacroE2E`, and the identical call from a
//! fresh spawn raised the intended `<UNDEFINED>MAIN+3^IIDMacroE2E`. Same shape as the class case
//! already on record for `%Dictionary` visibility. So setup gets its own spawn, and a one-spawn
//! version of this test would fail for a reason that has nothing to do with the mapping.
//!
//! ## Fixtures
//!
//! `IIDFrameE2E` and `IIDMacroE2E`, both `IID`-prefixed and deleted at the end. This repo is
//! public and other sessions own other instances, so the names are this repo's and the documents
//! are removed rather than left behind.

#![allow(clippy::zombie_processes)]
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// 11 lines, with the blank at line 9 — **inside** `BOOM`'s block, between the label and the
/// line that fails.
///
/// The position is the whole point and the first draft of this file got it wrong: with the blank
/// ABOVE the label, the walk never reaches it and the mapping is right whether or not blank lines
/// are dropped. A control that broke the drop rule passed, which is how the mistake surfaced —
/// the same blind spot that let a mutation survive the unit fixtures. `BOOM` is MAC 7 and the
/// undefined reference is MAC 10, so the blank between them must be skipped to land on it.
const FRAME_MAC: &str = concat!(
    "ROUTINE IIDFrameE2E [Type=MAC]\n",
    " ; comment on MAC line 2\n",
    "MAIN ; label MAIN on MAC line 3\n",
    " new a\n",
    " set a=1\n",
    " quit\n",
    "BOOM ; label BOOM on MAC line 7\n",
    " new b\n",
    "\n",
    " set b=zzE2EUndefined\n",
    " quit\n",
);

/// 7 lines, with the `#define` at line 4 — **inside** `MAIN`'s block, below the label and above
/// the line that fails — and a `$$$` invocation at line 5 that expands in place.
///
/// Same reasoning as `FRAME_MAC`: a directive above the label would be crossed by nothing.
const MACRO_MAC: &str = concat!(
    "ROUTINE IIDMacroE2E [Type=MAC]\n",
    "MAIN ; label MAIN on MAC line 2\n",
    " new a\n",
    "#define Twice(%x) ((%x)*2)\n",
    " set a=$$$Twice(21)\n",
    " set b=zzMacroE2EUndefined\n",
    " quit\n",
);

fn iris_dev_bin() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("target/debug/iris-interop-dev");
    p
}

fn namespace() -> String {
    std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".to_string())
}

/// `IRIS_HOST` unset is a configuration error here, not a reason to pass.
///
/// This target is `#[ignore]`d, so the offline gate reports it IGNORED, which is honest. Anyone
/// passing `--include-ignored` is asking to run live tests, and an early `return` on an empty
/// `IRIS_HOST` would report `ok` for a test that asserted nothing.
fn require_iris() {
    let host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(
        !host.is_empty(),
        "IRIS_HOST is not set. This test is #[ignore]d precisely so it is skipped rather than \
         silently passing; running it with --include-ignored and no instance would report ok \
         having asserted nothing."
    );
}

fn init() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"int-frame-e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    ]
}

fn call(id: i64, tool: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":args}})
}

/// Drive one MCP session over stdio, optionally sending a second batch computed from the first's
/// replies — `map_int` needs a frame that only exists once the error has been raised.
fn mcp_then(
    messages: &[serde_json::Value],
    follow_up: impl Fn(&[serde_json::Value]) -> Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut cmd = Command::new(iris_dev_bin());
    cmd.args(["mcp"])
        .env("IRIS_HOST", std::env::var("IRIS_HOST").unwrap_or_default())
        .env(
            "IRIS_WEB_PORT",
            std::env::var("IRIS_WEB_PORT").unwrap_or_else(|_| "52773".into()),
        )
        .env(
            "IRIS_USERNAME",
            std::env::var("IRIS_USERNAME").unwrap_or_else(|_| "_SYSTEM".into()),
        )
        .env(
            "IRIS_PASSWORD",
            std::env::var("IRIS_PASSWORD").unwrap_or_else(|_| "SYS".into()),
        )
        .env("IRIS_NAMESPACE", namespace());
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn iris-interop-dev mcp");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    let mut out = Vec::new();

    let pump = |batch: &[serde_json::Value],
                sink: &mut Vec<serde_json::Value>,
                stdin: &mut std::process::ChildStdin,
                reader: &mut BufReader<std::process::ChildStdout>| {
        for msg in batch {
            stdin
                .write_all((serde_json::to_string(msg).expect("serialise") + "\n").as_bytes())
                .expect("write");
            stdin.flush().expect("flush");
            if msg.get("id").is_none() {
                continue;
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        sink.push(v);
                        break;
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "no response to id {:?} within 60s",
                    msg.get("id")
                );
            }
        }
    };

    pump(messages, &mut out, &mut stdin, &mut reader);
    let second = follow_up(&out);
    pump(&second, &mut out, &mut stdin, &mut reader);
    let _ = child.kill();
    out
}

/// The JSON payload of the reply to `id`, or a panic naming what came back instead.
fn payload(replies: &[serde_json::Value], id: i64) -> serde_json::Value {
    let reply = replies
        .iter()
        .find(|r| r.get("id").and_then(|v| v.as_i64()) == Some(id))
        .unwrap_or_else(|| panic!("no reply with id {id} in {replies:#?}"));
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("reply {id} carries no text block: {reply}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("reply {id} is not JSON ({e}): {text}"))
}

/// Put both fixtures and compile them, in a spawn of their own — see the module note.
fn create_fixtures() {
    let ns = namespace();
    let mut msgs = init();
    msgs.push(call(
        2,
        "iris_doc",
        serde_json::json!({"mode":"put","name":"IIDFrameE2E.mac","content":FRAME_MAC,"namespace":ns,"compile":true}),
    ));
    msgs.push(call(
        3,
        "iris_doc",
        serde_json::json!({"mode":"put","name":"IIDMacroE2E.mac","content":MACRO_MAC,"namespace":ns,"compile":true}),
    ));
    let replies = mcp_then(&msgs, |_| Vec::new());
    for id in [2, 3] {
        let p = payload(&replies, id);
        assert_eq!(
            p["success"],
            serde_json::Value::Bool(true),
            "fixture {id} was not stored, so nothing below is about the mapping: {p}"
        );
        assert_eq!(
            p["compiled"],
            serde_json::Value::Bool(true),
            "fixture {id} did not compile, so it has no .int to map against: {p}"
        );
    }
}

fn delete_fixtures() {
    let ns = namespace();
    let mut msgs = init();
    for (i, doc) in ["IIDFrameE2E.mac", "IIDMacroE2E.mac"].iter().enumerate() {
        msgs.push(call(
            2 + i as i64,
            "iris_doc",
            serde_json::json!({"mode":"delete","name":doc,"namespace":ns}),
        ));
    }
    // Best effort: a fixture left behind is untidy, not a failure of what is under test.
    let _ = std::panic::catch_unwind(|| mcp_then(&msgs, |_| Vec::new()));
}

/// Raise `code` and return whatever it wrote — the real `$ZERROR` frame.
///
/// `Try`/`Catch` rather than `$ZTRAP` with a label: measured on 2026.1, the label form inside the
/// generated wrapper answered `<NOLINE> 8 ^IrisDevTmp.Run….1`, a frame about the wrapper rather
/// than about the routine, so the test would have been mapping the wrong thing.
fn raise(code: &str) -> String {
    let ns = namespace();
    let mut msgs = init();
    msgs.push(call(
        2,
        "iris_execute",
        serde_json::json!({"code": code, "namespace": ns}),
    ));
    let replies = mcp_then(&msgs, |_| Vec::new());
    let p = payload(&replies, 2);
    p["output"]
        .as_str()
        .unwrap_or_else(|| panic!("iris_execute returned no output: {p}"))
        .trim()
        .to_string()
}

#[test]
#[ignore = "requires live IRIS"]
fn an_error_raised_in_a_mac_maps_to_the_mac_line_it_came_from() {
    require_iris();
    create_fixtures();
    let ns = namespace();

    // A REAL error, not a constructed string: the offsets below are IRIS's own.
    let frame = raise(" Try { Do BOOM^IIDFrameE2E } Catch e { Write $ZERROR }\n");
    assert!(
        frame.starts_with("<UNDEFINED>BOOM+2^IIDFrameE2E"),
        "IRIS raised {frame:?}, not the frame this test is about — the fixture or the call is \
         wrong, and mapping it would assert nothing"
    );

    let macro_frame = raise(" Try { Do MAIN^IIDMacroE2E } Catch e { Write $ZERROR }\n");
    assert!(
        macro_frame.starts_with("<UNDEFINED>MAIN+3^IIDMacroE2E"),
        "IRIS raised {macro_frame:?} for the macro fixture"
    );

    let mut msgs = init();
    msgs.push(call(
        2,
        "iris_debug",
        serde_json::json!({"action":"map_int","error_string":frame,"namespace":ns}),
    ));
    msgs.push(call(
        3,
        "iris_debug",
        serde_json::json!({"action":"map_int","error_string":macro_frame,"namespace":ns}),
    ));
    // The line the macro expanded ON, which is the case #534 expected to be only approximate.
    msgs.push(call(
        4,
        "iris_debug",
        serde_json::json!({"action":"map_int","error_string":"<UNDEFINED>MAIN+2^IIDMacroE2E","namespace":ns}),
    ));
    let replies = mcp_then(&msgs, |_| Vec::new());

    // (a) the plain fixture. The blank sits BETWEEN `BOOM` and the failing line, so the walk
    // has to skip it to land on MAC 10 — break the drop rule and this lands on the blank itself.
    let p = payload(&replies, 2);
    assert_eq!(p["int_line_number"], 9, "{p}");
    assert_eq!(p["int_line"], " set b=zzE2EUndefined", "{p}");
    assert_eq!(p["mac_line_number"], 10, "{p}");
    assert_eq!(p["mac_line"], " set b=zzE2EUndefined", "{p}");
    assert_eq!(p["mac_mapping"], "exact", "{p}");
    assert_eq!(p["mapped_to_source_line"], true, "{p}");
    assert_eq!(p["label_line_number"], 7, "BOOM's line in the .int: {p}");

    // (b) across a #define sitting INSIDE the label's block: the directive line is gone from the
    // .int, so the walk must skip it for a different reason than the blank above.
    let q = payload(&replies, 3);
    assert_eq!(q["int_line_number"], 5, "{q}");
    assert_eq!(q["mac_line_number"], 6, "{q}");
    assert_eq!(q["mac_line"], " set b=zzMacroE2EUndefined", "{q}");
    assert_eq!(q["mac_mapping"], "exact", "{q}");

    // (c) the expanded line. The LINE is exact; the texts differ, and both are reported.
    let r = payload(&replies, 4);
    assert_eq!(r["int_line_number"], 4, "{r}");
    assert_eq!(r["mac_line_number"], 5, "{r}");
    assert_eq!(r["mac_mapping"], "exact_line_macro_expanded", "{r}");
    assert_eq!(r["mac_line"], " set a=$$$Twice(21)", "what you edit: {r}");
    assert_eq!(
        r["int_line_as_generated"], " set a=((21)*2)",
        "what actually ran: {r}"
    );

    delete_fixtures();
}

/// The acceptance bullet the old code could never satisfy: it answered about a CLASS.
#[test]
#[ignore = "requires live IRIS"]
fn a_frame_for_a_routine_that_is_not_there_is_refused_as_a_routine() {
    require_iris();
    let ns = namespace();
    let mut msgs = init();
    msgs.push(call(
        2,
        "iris_debug",
        serde_json::json!({"action":"map_int","error_string":"<UNDEFINED>BOOM+2^IIDNoSuchE2E","namespace":ns}),
    ));
    let replies = mcp_then(&msgs, |_| Vec::new());
    let p = payload(&replies, 2);
    assert_eq!(p["error_code"], "ROUTINE_NOT_FOUND", "{p}");
    let whole = p.to_string();
    assert!(
        !whole.contains("is not a compiled class"),
        "the class wording this issue is about must not come back: {p}"
    );
    assert!(
        p["error"].as_str().unwrap_or_default().contains("routine"),
        "the refusal must be worded for routines: {p}"
    );
    // And it must not be the "bring me the text" answer, which is what it used to be.
    assert_ne!(p["outcome"], "INT_SOURCE_REQUIRED", "{p}");
}
