//! #437: code that moved the current device was reported as a success with no output.
//!
//! `execute_via_generator` captures what the body writes by redirecting the current device to a
//! temp file. Nothing checked that the device was still that file when the body finished, so a
//! body — or any callee of it — that did `Use <something else>` and did not put it back sent the
//! rest of its output to a device the generator never reads.
//!
//! Measured on IRIS for Health 2026.1 through `iris_execute` BEFORE the fix. The body in each
//! case opens a second temp file and `Use`s it without restoring:
//!
//! | body | before | after |
//! |---|---|---|
//! | writes, moves, writes again | `success:true`, output = the first line only | `success:false`, `ERROR($DEVICE)` + the prefix |
//! | moves, then writes | `success:true`, `no_output:true`, and a hint saying the call "ran" | `success:false`, `ERROR($DEVICE)` |
//! | moves, then `<SYNTAX>` | `success:false` via the `$ZERROR` net | `success:false`, now with the frame and line number |
//! | writes, moves, then `<SYNTAX>` | **`success:true`, no error, no error_code** | `success:false`, the trap in `error` |
//!
//! The last row is the one that makes this a data-integrity bug rather than a cosmetic one: a
//! hard abort came back as a success because the `$ZERROR` safety net below only fires when the
//! captured output is EMPTY, and the move left a non-empty prefix in it. Its control is the same
//! abort without the device move, which was correctly `success:false` throughout.
//!
//! Two repairs, and they are deliberately not the same mechanism:
//!
//! * the generator detects it and reports `ERROR($DEVICE)` — covers every cause, including a
//!   callee the caller never wrote, and is what makes the failure impossible to read as an answer;
//! * the `Ens.Director` lifecycle calls restore `$IO` themselves — makes their output SURVIVE
//!   rather than merely be accounted for, since each writes its own verdict on the next statement.

use iris_agentic_dev_core::iris::connection::IrisConnection;
use iris_agentic_dev_core::tools::interop;
use iris_agentic_dev_core::tools::runtime_abort_line;

/// The generated wrapper, as one string.
fn generated() -> String {
    IrisConnection::build_exec_class_for_test("IrisDevTmp.Run437", " Write \"x\"").join("\n")
}

// ── the generator records where the device ended up ──────────────────────────────────────────

/// Both arms of the `Try`/`Catch`, because the Catch's own `Write` is also lost to a moved
/// device — and that `Write` is the only thing that carries the abort's frame and line number.
#[test]
fn the_device_is_snapshotted_on_both_arms_of_the_try() {
    let src = generated();
    let snapshots = src.matches("Set devio = $IO").count();
    assert_eq!(
        snapshots, 2,
        "expected a $IO snapshot on the Try arm and on the Catch arm, found {snapshots}:\n{src}"
    );
}

/// The snapshot has to happen before anything else writes, or the value read is the wrapper's
/// own device rather than the one the body left behind — the check would then never fire.
#[test]
fn the_snapshot_precedes_the_sentinel_write() {
    let src = generated();
    let last_snapshot = src
        .rfind("Set devio = $IO")
        .expect("the snapshot must be present");
    let sentinel = src
        .rfind("Write !")
        .expect("the IDEV-3 sentinel must be present");
    assert!(
        last_snapshot < sentinel,
        "the last $IO snapshot is at byte {last_snapshot} and the sentinel `Write !` at \
         {sentinel}. Snapshotting after a Write reads the wrapper's device, not the body's, so \
         the check silently never fires:\n{src}"
    );
}

/// The Catch re-attaches before writing the trap. Without this the abort text goes to the moved
/// device and is lost, which is the row-four false success in the module note.
#[test]
fn the_catch_reattaches_the_capture_before_writing_the_trap() {
    let src = generated();
    let catch = src
        .find("} Catch ex {")
        .expect("the Catch arm must be present");
    let trap = src[catch..]
        .find(r#"Write "ERROR: ",ex.DisplayString()"#)
        .map(|i| catch + i)
        .expect("the Catch must write the trap");
    let reattach = src[catch..trap]
        .find("Use tmpfile")
        .map(|i| catch + i)
        .unwrap_or_else(|| {
            panic!(
                "the Catch arm writes the trap at byte {trap} with no `Use tmpfile` between \
                 `}} Catch ex {{` (byte {catch}) and it, so a body that moved the device loses \
                 its own abort:\n{src}"
            )
        });
    assert!(reattach < trap, "re-attach at {reattach}, trap at {trap}");
}

/// A native Windows IRIS lower-cases `$IO` (upstream issue 139), so an exact compare there would
/// fire on every call. Measured on 2026.1/Linux the two strings are byte-equal, which is why
/// this is a portability guard and not something the live test can demonstrate.
#[test]
fn the_device_comparison_is_case_insensitive() {
    let src = generated();
    let cmp = src
        .lines()
        .find(|l| l.contains("devio") && l.contains("ERROR($DEVICE)"))
        .unwrap_or_else(|| panic!("no device comparison found:\n{src}"));
    assert!(
        cmp.contains(r#"$ZConvert(devio,"L")"#) && cmp.contains(r#"$ZConvert(tmpfile,"L")"#),
        "both sides of the comparison must be case-folded, or a Windows instance reports every \
         call as a lost capture:\n{cmp}"
    );
}

/// The device line is PREPENDED. The wrapper's abort has to stay the last non-empty line for
/// `runtime_abort_line`'s anchor to find it, and a body that moved the device and then threw
/// produces both lines at once.
#[test]
fn the_device_line_is_prepended_so_the_trap_stays_last() {
    let src = generated();
    let line = src
        .lines()
        .find(|l| l.contains("ERROR($DEVICE)"))
        .unwrap_or_else(|| panic!("no device line found:\n{src}"));
    let marker = line
        .find("ERROR($DEVICE)")
        .expect("the literal is on this line");
    let concat = line[marker..]
        .find("_out }")
        .unwrap_or_else(|| panic!("the device text is not concatenated BEFORE `out`:\n{line}"));
    assert!(
        concat > 0,
        "expected `… _$Char(10)_out }}`, i.e. the device text in front of the captured output, \
         so that an abort written by the Catch remains the last line:\n{line}"
    );
}

// ── and the one function that decides `success` recognises it ────────────────────────────────

#[test]
fn a_lost_capture_is_a_failure() {
    let out = "ERROR($DEVICE): output capture lost - the code left the current device on \
               /tmp/filX.txt instead of the capture file, so anything written after the switch \
               was discarded";
    assert!(
        runtime_abort_line(out).is_some(),
        "a lost capture must not read as output"
    );
}

/// With both present the trap wins, because it names the line that failed and the device loss
/// only says the output is short. Both are still in `output` for the caller to read.
#[test]
fn the_trap_is_reported_in_preference_to_the_lost_capture() {
    let out = "ERROR($DEVICE): output capture lost - the code left the current device on \
               /tmp/filX.txt instead of the capture file, so anything written after the switch \
               was discarded\npartial output\nERROR: <SYNTAX> 3 RunUser+5^IrisDevTmp.Run1.1";
    let reported = runtime_abort_line(out).expect("this is a failure either way");
    assert!(
        reported.starts_with("ERROR: <SYNTAX>"),
        "expected the trap, got {reported:?}"
    );
}

/// The device line is found wherever it sits, not only first — the prepend is a convention the
/// generator keeps, and this function must not depend on it.
#[test]
fn the_lost_capture_is_found_even_when_it_is_not_the_first_line() {
    let out = "some output\nERROR($DEVICE): output capture lost - device /tmp/filX.txt";
    assert!(runtime_abort_line(out).is_some(), "{out}");
}

/// The control that keeps the three above honest: ordinary output, and output that merely
/// mentions the words, stay successes.
#[test]
fn ordinary_output_is_not_a_lost_capture() {
    for ok in [
        "plain output",
        "",
        "the word ERROR appears here",
        // Not the wrapper's line: no colon-space after the parenthesis.
        "ERROR($DEVICE)",
        "I wrote about ERROR($DEVICE) in the middle of a line",
    ] {
        assert!(
            runtime_abort_line(ok).is_none(),
            "reported as a failure: {ok:?}"
        );
    }
}

// ── the Ens.Director calls put the device back before they report ────────────────────────────

#[test]
fn the_bracket_restores_the_device_after_the_statement() {
    let got = interop::restoring_device("Do ##class(Ens.Director).RecoverProduction()");
    assert_eq!(
        got,
        "Set tIO=$IO Do ##class(Ens.Director).RecoverProduction() Use tIO"
    );
}

/// Every `UpdateProduction` the production_item builders emit is bracketed, and the restore
/// lands BEFORE the statement that reports on it. Presence alone would not say that: a
/// `Use tIO` after the `Write` restores a device whose output has already been thrown away.
#[test]
fn every_production_item_builder_restores_the_device_before_reporting() {
    let built: Vec<(&str, String)> = vec![
        (
            "set_enabled",
            interop::build_set_enabled_code("My.Prod", "MyItem", true),
        ),
        (
            "add_item",
            interop::build_add_item_code(
                "My.Prod",
                "MyItem",
                "My.BS.Thing",
                true,
                None,
                None,
                &std::collections::HashMap::new(),
            ),
        ),
        (
            "remove_item",
            interop::build_remove_item_code("My.Prod", "MyItem"),
        ),
        (
            "set_settings",
            interop::build_set_settings_code(
                "My.Prod",
                "MyItem",
                &std::collections::HashMap::new(),
                true,
            ),
        ),
    ];
    let mut checked = 0;
    for (name, code) in &built {
        let call = code
            .find("UpdateProduction(")
            .unwrap_or_else(|| panic!("{name} emits no UpdateProduction:\n{code}"));
        let snapshot = code[..call].rfind("Set tIO=$IO").unwrap_or_else(|| {
            panic!("{name}: UpdateProduction at {call} is not preceded by a snapshot:\n{code}")
        });
        let restore = code[call..]
            .find("Use tIO")
            .map(|i| call + i)
            .unwrap_or_else(|| panic!("{name}: no `Use tIO` after UpdateProduction:\n{code}"));
        let report = code[call..]
            .find("UPDATE_FAILED")
            .map(|i| call + i)
            .unwrap_or_else(|| {
                panic!("{name}: UpdateProduction's result is never reported:\n{code}")
            });
        assert!(
            snapshot < call && call < restore && restore < report,
            "{name}: expected snapshot({snapshot}) < call({call}) < restore({restore}) < \
             report({report}). A restore after the report cannot save the output:\n{code}"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        built.len(),
        "checked {checked} of {} builders",
        built.len()
    );
    println!("builders checked: {checked}");
}

/// The population guard. The four builders above are the ones with a pure entry point; the
/// `Ens.Director` lifecycle calls are built inside `async fn`s, so their text is only reachable
/// by reading the source. Derived rather than listed, so a NEW mutating call cannot be added
/// without either bracketing it or amending this test.
///
/// Comments are stripped first: a comment naming the construct is the commonest false witness
/// for a guard like this one, and `interop.rs` discusses `UpdateProduction` in prose repeatedly.
#[test]
fn every_mutating_ens_director_call_in_the_source_is_bracketed() {
    // These re-job the production's workers or edit the CPF, which is when $IO has been
    // measured to move. Reads (`GetProductionStatus`, `ProductionNeedsUpdate`) are excluded
    // because they do not, and `SetAutoStart` because it edits the class without re-jobbing —
    // all three are still covered by the generator's own check if that turns out to be wrong.
    const MUTATING: [&str; 5] = [
        "StartProduction(",
        "StopProduction(",
        "UpdateProduction(",
        "RecoverProduction(",
        "EnableConfigItem(",
    ];
    let mut total = 0;
    let mut unbracketed = Vec::new();
    let mut windows: Vec<usize> = Vec::new();
    for file in [
        "src/tools/interop.rs",
        "src/tools/admin.rs",
        "src/iris/connection.rs",
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{file}: {e}"));
        let all: Vec<&str> = src.lines().collect();
        for (n, raw) in all.iter().copied().enumerate() {
            let line = raw.trim_start();
            if line.starts_with("//") {
                continue;
            }
            // Only the generated ObjectScript matters, which always names the class inline.
            if !line.contains("Ens.Director)") {
                continue;
            }
            if !MUTATING.iter().any(|m| line.contains(m)) {
                continue;
            }
            total += 1;
            let (ok, window) = bracket_window(&all, n);
            windows.push(window);
            if !ok {
                unbracketed.push(format!("{file}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        total >= 6,
        "found only {total} mutating Ens.Director call sites — the pattern this test greps for \
         has drifted, so its zero means nothing. Fix the pattern, do not relax the bound."
    );
    assert!(
        unbracketed.is_empty(),
        "{} of {total} mutating Ens.Director calls do not restore the device:\n  {}",
        unbracketed.len(),
        unbracketed.join("\n  ")
    );
    println!(
        "mutating Ens.Director call sites bracketed: {total}/{total}; statement windows (lines): \
         {windows:?}"
    );
}

/// `Config.Namespaces` `Create`/`Delete` edit the CPF, which upstream fixed in the same change.
#[test]
fn the_namespace_writes_restore_the_device() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tools/admin.rs");
    let src = std::fs::read_to_string(path).expect("admin.rs");
    let all: Vec<&str> = src.lines().collect();
    let mut checked = 0;
    for call in ["Config.Namespaces).Create(", "Config.Namespaces).Delete("] {
        let sites: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.trim().starts_with("//") && l.contains(call))
            .map(|(i, _)| i)
            .collect();
        assert!(!sites.is_empty(), "no call site found for {call}");
        for at in sites {
            let (ok, window) = bracket_window(&all, at);
            assert!(
                ok,
                "{call} does not restore the device (statement window {window} lines):\n{}",
                all[at].trim()
            );
            checked += 1;
        }
    }
    assert_eq!(
        checked, 2,
        "expected one Create and one Delete, saw {checked}"
    );
}

/// Is the generated statement on `lines[at]` lexically inside a device bracket?
///
/// The window is a STATEMENT, not a fixed number of lines: `restoring_device(&format!(…))`
/// spans three or four source lines, so a one-line test reports a bracketed call as naked — and
/// a fixed-size window would be an arbitrary bound that passes on unrelated text as it grows.
/// Walking back until a line ends with `;`, `{` or `}` keeps the window exactly as wide as the
/// claim. The size is returned so a caller can print it rather than trust it.
fn bracket_window(lines: &[&str], at: usize) -> (bool, usize) {
    let mut i = at;
    loop {
        let line = lines[i].trim();
        if line.contains("restoring_device") || line.contains("Set tIO=$IO") {
            return (true, at - i + 1);
        }
        if i == 0 {
            return (false, at + 1);
        }
        // A previous line that closes a statement means we have left the construct.
        let prev = lines[i - 1].trim_end();
        if i < at && (prev.ends_with(';') || prev.ends_with('{') || prev.ends_with('}')) {
            return (false, at - i + 1);
        }
        i -= 1;
    }
}

// ── and IRIS agrees ──────────────────────────────────────────────────────────────────────────

mod live {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    /// `IRIS_HOST` unset is a configuration error here, not a reason to pass.
    fn require_iris() {
        assert!(
            !std::env::var("IRIS_HOST").unwrap_or_default().is_empty(),
            "IRIS_HOST is not set. This test is #[ignore]d precisely so it is skipped rather \
             than silently passing."
        );
    }

    fn iris_dev_bin() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p.push("target/debug/iris-interop-dev");
        p
    }

    /// Three lines that move the current device to a second file and do not put it back. The
    /// realistic cause is a CALLEE doing this, which the caller cannot see; written inline here
    /// because the fixture has to work on an instance with no production.
    const MOVE: &str = " Set other=##class(%Library.File).TempFilename(\"txt\")\n \
                        Open other:(\"WNS\"):5\n Use other";

    /// Run `code` through `iris_execute` and return the whole envelope.
    fn execute(code: &str) -> serde_json::Value {
        let ns = std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".into());
        let mut child = Command::new(iris_dev_bin())
            .args(["mcp"])
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
            .env("IRIS_NAMESPACE", &ns)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn iris-interop-dev mcp");
        let msgs = vec![
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"device-e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_execute","arguments":{"code":code,"namespace":ns}}}),
        ];
        let mut stdin = child.stdin.take().expect("stdin");
        let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
        for m in &msgs {
            stdin
                .write_all((serde_json::to_string(m).expect("serialise") + "\n").as_bytes())
                .expect("write");
        }
        stdin.flush().expect("flush");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let payload = loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) > 0 {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    if v.get("id").and_then(|i| i.as_i64()) == Some(2) {
                        break v;
                    }
                }
            }
            assert!(
                std::time::Instant::now() <= deadline,
                "iris_execute did not answer within 60s"
            );
        };
        let _ = child.kill();
        let _ = child.wait();
        let text = payload["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text block: {payload}"));
        serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
    }

    /// The four rows of the module note, plus the controls that make them mean something.
    ///
    /// The controls are not decoration. Without the third one — a body that moves the device and
    /// PUTS IT BACK — a check that simply always fired would pass every other row here.
    #[test]
    #[ignore = "requires live IRIS"]
    fn a_body_that_moves_the_device_is_reported_rather_than_truncated() {
        require_iris();
        let cases: [(&str, String, bool); 7] = [
            // ── controls: the check must stay quiet ──
            ("ordinary body", " Write \"plain\",!".into(), true),
            (
                "multi-line, non-ASCII (#464 still works)",
                " Write \"a\",!\n Write $CHAR(26085,26412),!".into(),
                true,
            ),
            (
                "moves the device and RESTORES it — the false-positive control",
                format!(" Set io=$IO\n{MOVE}\n Use io\n Write \"restored\",!"),
                true,
            ),
            // ── the defect ──
            (
                "writes, moves, writes again",
                format!(" Write \"before the move\",!\n{MOVE}\n Write \"AFTER\",!"),
                false,
            ),
            (
                "moves before writing anything",
                format!("{MOVE}\n Write \"all of this is lost\",!"),
                false,
            ),
            (
                "moves, then aborts",
                format!("{MOVE}\n Set x=$ZZNOSUCH"),
                false,
            ),
            (
                "writes, moves, THEN aborts — was success:true",
                format!(" Write \"partial output\",!\n{MOVE}\n Set x=$ZZNOSUCH"),
                false,
            ),
        ];
        let mut checked = 0;
        for (what, code, expect_success) in &cases {
            let v = execute(code);
            let success = v["success"]
                .as_bool()
                .unwrap_or_else(|| panic!("{what}: {v}"));
            assert_eq!(
                success, *expect_success,
                "{what}: expected success={expect_success}, got {v}"
            );
            let output = v["output"].as_str().unwrap_or("");
            if *expect_success {
                assert!(
                    !output.contains("ERROR($DEVICE)"),
                    "{what}: the check fired on a body that did not lose the device: {v}"
                );
            } else {
                assert!(
                    output.contains("ERROR($DEVICE): output capture lost"),
                    "{what}: reported a failure, but not as a lost capture: {v}"
                );
            }
            checked += 1;
            println!("  ok  {what} -> success={success}");
        }
        assert_eq!(checked, cases.len(), "{checked} of {} cases", cases.len());
        println!("cases checked: {checked}");
    }

    /// The row that makes this a correctness bug: the abort must survive the device move with
    /// its frame and the line number intact, because the Catch now re-attaches before writing.
    ///
    /// Paired with the same abort and no device move. The two differ in exactly one statement,
    /// so a run where both report the same thing says the fixture stopped moving the device.
    #[test]
    #[ignore = "requires live IRIS"]
    fn an_abort_after_a_device_move_is_still_reported_as_an_abort() {
        require_iris();
        let moved_body = format!(" Write \"partial output\",!\n{MOVE}\n Set x=$ZZNOSUCH");
        let control_body = " Write \"partial output\",!\n Set x=$ZZNOSUCH".to_string();
        let moved = execute(&moved_body);
        let control = execute(&control_body);

        for (what, v) in [("device moved", &moved), ("control", &control)] {
            assert_eq!(v["success"], false, "{what} must be a failure: {v}");
            assert_eq!(v["error_code"], "IRIS_RUNTIME_ERROR", "{what}: {v}");
            let err = v["error"].as_str().unwrap_or("");
            assert!(
                err.contains("<SYNTAX>"),
                "{what}: the abort is not in `error`: {v}"
            );
        }
        // The control did not lose its device, so it must NOT carry the device line. That is
        // what proves the fixture's extra statements are the thing being measured.
        assert!(
            !control["output"]
                .as_str()
                .unwrap_or("")
                .contains("ERROR($DEVICE)"),
            "the control lost its device too, so the pair measures nothing: {control}"
        );
        assert!(
            moved["output"]
                .as_str()
                .unwrap_or("")
                .contains("ERROR($DEVICE)"),
            "the moved case did not report a lost capture: {moved}"
        );
        // And the line number survives, which it only can because the Catch re-attached. The
        // two bodies are different LENGTHS — that is the whole fixture — so the numbers must
        // differ; what has to hold is that each names its own last line. Asserting they were
        // equal was the first version of this check, and it failed for exactly that reason.
        for (what, v, body) in [
            ("device moved", &moved, &moved_body),
            ("control", &control, &control_body),
        ] {
            let want = body.lines().count();
            assert_eq!(
                v["source_line_number"].as_u64().map(|n| n as usize),
                Some(want),
                "{what}: the abort is the last of {want} lines, so that is the line that must be \
                 reported: {v}"
            );
            assert_eq!(
                v["source_line"].as_str(),
                Some(" Set x=$ZZNOSUCH"),
                "{what}: wrong source line echoed back: {v}"
            );
        }
        assert_ne!(
            moved["source_line_number"], control["source_line_number"],
            "the two bodies differ in length, so identical line numbers mean the fixture is not \
             doing what it claims:\nmoved={moved}\ncontrol={control}"
        );
        println!(
            "abort reported with the device moved: {} (control: {})",
            moved["error"], control["error"]
        );
    }
}
