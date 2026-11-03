//! #464: output written by generated code lost every character above U+00FF.
//!
//! `execute_via_generator` captures what the body `Write`s by redirecting the current device to a
//! temp file and reading it back with `%Stream.FileCharacter`. Neither end declared a translation
//! table, so IRIS used the process default — an 8-bit table on a Unicode instance — and every
//! code point outside it became `?`.
//!
//! Measured on IRIS for Health 2026.1 BEFORE the fix, through `iris_execute`:
//!
//! | written | came back |
//! |---|---|
//! | `Martínez café` (Latin-1) | `Martínez café` |
//! | こんにちは | `?????` |
//! | 日本語 | `???` |
//! | € | `?` |
//! | 😊 (surrogate pair) | `??` |
//!
//! Latin-1 surviving is why this went unnoticed: Spanish and French text looks correct and only
//! CJK, emoji and the euro sign are destroyed. An HL7 payload or a patient name in a non-Latin
//! script read through `iris_execute` was silently wrong rather than visibly broken.
//!
//! BOTH ends need the table. `/IOT="UTF8"` alone writes UTF-8 that the read would then decode
//! through the 8-bit default; `TranslateTable` alone decodes as UTF-8 bytes that were never
//! written as UTF-8. The two assertions below are separate for that reason.

use iris_agentic_dev_core::iris::connection::IrisConnection;

/// The generated ObjectScript, as one string.
fn generated() -> String {
    IrisConnection::build_exec_class_for_test("IrisDevTmp.Run464", " Write \"x\"").join("\n")
}

// ── the generated code declares a table on both ends ─────────────────────────────────────────

/// The WRITE end: the device the body's output goes to.
#[test]
fn the_capture_device_is_opened_as_utf8() {
    let src = generated();
    assert!(
        src.contains(r#"Open tmpfile:("WNS":/IOT="UTF8"):5"#),
        "the capture Open declares no UTF8 translation, so output above U+00FF becomes '?':\n{src}"
    );
}

/// The READ end: the stream that reads the file back.
#[test]
fn the_capture_stream_reads_as_utf8() {
    let src = generated();
    assert!(
        src.contains(r#"Set stream.TranslateTable = "UTF8""#),
        "the capture stream declares no translation table:\n{src}"
    );
}

/// Upstream sets the table before linking, and this pins that order — but it is a CONVENTION
/// guard, not a behavioural one, and the distinction is measured rather than assumed.
///
/// The first version of this comment claimed `%Stream.FileCharacter` fixes its translation when
/// it is linked, so a late `TranslateTable` would be ignored. A mutation that moved the line
/// AFTER `LinkToFile` killed only this test: the live round trip still returned every code point
/// correctly on 2026.1. So the claim was wrong, and saying it here would have been a false
/// explanation sitting next to a passing test — the shape this repo keeps finding.
///
/// What the order is still worth: the two trees agree, and the code does not rely on
/// `%Stream` accepting a change after the link, which is unspecified rather than guaranteed.
#[test]
fn the_stream_table_is_set_before_the_file_is_linked() {
    let src = generated();
    let table = src
        .find(r#"Set stream.TranslateTable = "UTF8""#)
        .expect("the TranslateTable line must be present");
    let link = src
        .find("stream.LinkToFile(tmpfile)")
        .expect("the LinkToFile line must be present");
    assert!(
        table < link,
        "TranslateTable is set at byte {table} and LinkToFile at {link}. Measured on 2026.1 the \
         late order still worked, so this is a convention guard matching upstream — if you moved \
         it deliberately, move this assertion too rather than deleting it:\n{src}"
    );
}

/// The two ends are independent settings and a half-fix reads as a fix on casual inspection, so
/// count them rather than trusting the two assertions above to both be kept.
#[test]
fn both_ends_of_the_pipe_are_declared() {
    let src = generated();
    let ends = [
        (r#"/IOT="UTF8""#, "the write end (device)"),
        (r#"stream.TranslateTable = "UTF8""#, "the read end (stream)"),
    ];
    let present = ends.iter().filter(|(m, _)| src.contains(m)).count();
    assert_eq!(
        present,
        ends.len(),
        "only {present} of {} ends declare UTF8; missing: {:?}",
        ends.len(),
        ends.iter()
            .filter(|(m, _)| !src.contains(m))
            .map(|(_, w)| *w)
            .collect::<Vec<_>>()
    );
    println!("UTF8 declarations found: {present}");
}

// ── and that IRIS agrees ─────────────────────────────────────────────────────────────────────

mod live {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    fn iris_dev_bin() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p.push("target/debug/iris-interop-dev");
        p
    }

    /// `IRIS_HOST` unset is a configuration error here, not a reason to pass: an early return
    /// would report `ok` for a test that asserted nothing.
    fn require_iris() {
        assert!(
            !std::env::var("IRIS_HOST").unwrap_or_default().is_empty(),
            "IRIS_HOST is not set. This test is #[ignore]d precisely so it is skipped rather \
             than silently passing."
        );
    }

    /// Run `code` through `iris_execute` on a live instance and return what it wrote.
    fn execute(code: &str) -> String {
        let ns = std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".into());
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
            .env("IRIS_NAMESPACE", &ns);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn iris-interop-dev mcp");
        let msgs = vec![
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"utf8-e2e","version":"0.1"}}}),
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
        let text = payload["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text block: {payload}"));
        let v: serde_json::Value =
            serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"));
        v["output"]
            .as_str()
            .unwrap_or_else(|| panic!("no output field: {v}"))
            .trim_end_matches('\n')
            .to_string()
    }

    /// Every row of the table in the module note, driven through a real instance.
    ///
    /// `$CHAR` rather than literal source text: the code reaches IRIS as one line of a generated
    /// method, so what is being tested is the OUTPUT path. Putting the characters in the source
    /// would also exercise the input splicing, which is a different fix (#119) and already works.
    #[test]
    #[ignore = "requires live IRIS"]
    fn every_code_point_written_by_the_body_survives_the_round_trip() {
        require_iris();
        let cases: [(&str, &str, &str); 6] = [
            (
                "ASCII",
                " Write \"plain ascii\"",
                "plain ascii",
            ),
            (
                "Latin-1 accents (survived even before the fix — the control that keeps this honest)",
                " Write \"Mart\",$CHAR(237),\"nez caf\",$CHAR(233)",
                "Martínez café",
            ),
            (
                "hiragana",
                " Write $CHAR(12371,12435,12395,12385,12399)",
                "こんにちは",
            ),
            ("kanji", " Write $CHAR(26085,26412,35486)", "日本語"),
            ("euro sign", " Write $CHAR(8364)", "€"),
            (
                "emoji, a surrogate pair",
                " Write $CHAR(55357,56842)",
                "😊",
            ),
        ];
        let mut checked = 0;
        for (label, code, want) in cases {
            let got = execute(code);
            assert_eq!(
                got, want,
                "{label}: wrote {want:?} and read back {got:?} — a '?' here means a translation \
                 table is missing on one end of the capture pipe"
            );
            checked += 1;
        }
        assert_eq!(checked, 6, "every row must be exercised");
        println!("code-point rows asserted: {checked}");
    }
}
