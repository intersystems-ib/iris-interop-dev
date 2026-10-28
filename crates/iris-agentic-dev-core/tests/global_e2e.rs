//! #532 e2e: `iris_global` against a live instance, with IRIS itself as the authority.
//!
//! Renamed from `global_read_e2e.rs` when the write half landed. The old name was about to
//! become the thing this repo keeps catching itself doing: a file whose guarantee is narrower
//! than its contents, so a reader checking "is the write path covered end to end?" sees a name
//! that says no and stops looking.
//!
//! The unit tests pin the wire format and the addressing rules against hand-written strings. What
//! they cannot show is that IRIS agrees — that a comma inside a subscript really addresses the
//! node we think it does, that `$DATA` really comes back as 10 for a node with children and no
//! value, and that a value holding a newline survives the round trip through the generator's
//! output capture. Only an instance can answer those.
//!
//! FIXTURES LIVE ON `^IIDTest` AND NOWHERE ELSE. This repo is public and other sessions own other
//! instances; a test that wrote to a name someone else uses would be a data-loss bug wearing the
//! shape of a test. Every node written here is killed again at the end of the exchange.
//!
//! Fixtures are written with `iris_execute`, not with `iris_global`: this half of #532 implements
//! READS only, so the tool under test has no way to create its own fixtures — which is also why
//! the setup is visible ObjectScript rather than a helper.

#![allow(clippy::zombie_processes)]
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

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

/// Drive one MCP session to a `routines`-profile server over stdio, in two phases, the second
/// computed from the first's replies.
///
/// `IRIS_TOOLSET=routines` is not incidental: `iris_global` is advertised by that profile only, so
/// started on the fork's default the server would answer "tool not found" and every assertion
/// below would be unreachable for a reason unrelated to the behaviour under test.
///
/// Two phases rather than two sessions because `global_kill` needs a token that only exists at run
/// time, and the grant store is per PROCESS — a second spawn gets an empty store, so the kill
/// could only ever be tested through a refusal, and "every refusal fires" is also what a
/// completely broken kill looks like. A phase keeps the spawn, the environment and the read loop
/// to one copy; a second entry point for the no-follow-up case was written first and deleted
/// unused, since the only test here needs the second phase.
fn mcp_routines_then(
    messages: &[serde_json::Value],
    follow_up: impl Fn(&[serde_json::Value]) -> Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    let bin = iris_dev_bin();
    let mut cmd = Command::new(&bin);
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
        .env("IRIS_NAMESPACE", namespace())
        .env("IRIS_TOOLSET", "routines");

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn iris-interop-dev mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    let mut results = Vec::new();

    for msg in messages {
        stdin
            .write_all((serde_json::to_string(msg).expect("serialise") + "\n").as_bytes())
            .expect("write");
        stdin.flush().expect("flush");
        if msg.get("id").is_some() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        results.push(v);
                        break;
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                if std::time::Instant::now() > deadline {
                    panic!("no response to id {:?} within 30s", msg.get("id"));
                }
            }
        }
    }
    for msg in follow_up(&results) {
        stdin
            .write_all((serde_json::to_string(&msg).expect("serialise") + "\n").as_bytes())
            .expect("write");
        stdin.flush().expect("flush");
        if msg.get("id").is_some() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        results.push(v);
                        break;
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                if std::time::Instant::now() > deadline {
                    panic!("no response to follow-up id {:?} within 30s", msg.get("id"));
                }
            }
        }
    }
    let _ = child.kill();
    results
}

fn find(responses: &[serde_json::Value], id: i64) -> &serde_json::Value {
    responses
        .iter()
        .find(|r| r["id"] == serde_json::json!(id))
        .unwrap_or_else(|| panic!("no response with id {id}"))
}

/// The tool's own JSON payload, out of the MCP envelope.
fn body(responses: &[serde_json::Value], id: i64) -> serde_json::Value {
    let r = find(responses, id);
    let text = r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("id {id} carried no text content: {r}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("id {id} text is not JSON ({e}): {text}"))
}

fn init() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"global-e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    ]
}

fn call(id: i64, tool: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":args}})
}

fn global_call(id: i64, args: serde_json::Value) -> serde_json::Value {
    call(id, "iris_global", args)
}

/// Every fixture node, and the legacy shapes each one exists to prove.
///
/// `$C(10)` rather than a literal newline: the code reaches IRIS as one line of a generated
/// method, so an embedded line feed in the SOURCE would end the statement. The newline in the
/// VALUE is the point of that fixture, and this is how it gets there.
const FIXTURES: &str = concat!(
    " Kill ^IIDTest\n",
    // $D=1 with a non-empty value — the ordinary case, as a control for all the others.
    " Set ^IIDTest(\"plain\")=\"simple\"\n",
    // $D=1 whose value IS the empty string. Must not read as "no value".
    " Set ^IIDTest(\"empty\")=\"\"\n",
    // $D=10: children, no value of its own. $GET returns "" here too, which is the collapse.
    " Set ^IIDTest(\"parent\",\"child\")=\"leaf\"\n",
    // $D=11: a value AND children. $QUERY never returns this node, so a walk must emit it itself.
    " Set ^IIDTest(\"both\")=\"mine\"\n",
    " Set ^IIDTest(\"both\",\"kid\")=\"theirs\"\n",
    // Subscripts upstream's allowlist rejected outright.
    " Set ^IIDTest(\"ACME, INC.\")=\"comma key\"\n",
    " Set ^IIDTest(\"O'BRIEN\")=\"apostrophe key\"\n",
    // A caret in a VALUE — ordinary data in routine-era globals, structure to a naive parser.
    " Set ^IIDTest(\"caret\")=\"A^B(1)\"\n",
    // A pipe AND a newline in one value: both of upstream's delimiters at once.
    " Set ^IIDTest(\"pipes\")=\"a|b\"_$C(10)_\"c|d\"\n",
    // M code as a value, which must round-trip unchanged rather than be interpreted.
    " Set ^IIDTest(\"mcode\")=\"S Y=$J(V,10,2)\"\n",
    // The canonic-number rule: a numeric subscript and a NON-canonic numeric-looking string are
    // different nodes, while a canonic numeric string is the same node as the number.
    " Set ^IIDTest(1)=\"numeric one\"\n",
    " Set ^IIDTest(\"01\")=\"string zero one\"\n",
);

/// `IRIS_HOST` unset is a configuration error here, not a reason to pass.
///
/// This target is `#[ignore]`d, so the offline gate skips it and reports it as IGNORED — which is
/// honest. Anyone passing `--include-ignored` is asking to run live tests, and the existing e2e
/// files' habit of returning early on an empty `IRIS_HOST` reports `ok` for a test that asserted
/// nothing. `interop_e2e_tests.rs` documents the cost of that: a stale count shipped because eight
/// self-skipping tests were counted as passes. So this panics instead.
fn require_iris() -> String {
    let host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(
        !host.is_empty(),
        "IRIS_HOST is not set. This test is #[ignore]d precisely so it is skipped rather than \
         silently passing; running it with --include-ignored and no instance would otherwise \
         report ok having asserted nothing."
    );
    host
}

#[test]
#[ignore = "requires live IRIS"]
fn iris_global_agrees_with_what_iris_actually_holds() {
    require_iris();
    let ns = namespace();

    let mut msgs = init();
    msgs.push(call(
        2,
        "iris_execute",
        serde_json::json!({"code": FIXTURES, "namespace": ns}),
    ));
    // $DATA's four states, read one node at a time.
    msgs.push(global_call(
        10,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["plain"],"namespace":ns}),
    ));
    msgs.push(global_call(
        11,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["empty"],"namespace":ns}),
    ));
    msgs.push(global_call(
        12,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["parent"],"namespace":ns}),
    ));
    msgs.push(global_call(
        13,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["both"],"namespace":ns}),
    ));
    msgs.push(global_call(
        14,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["nothing here"],"namespace":ns}),
    ));
    // Subscripts and values legacy data actually contains.
    msgs.push(global_call(
        20,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["ACME, INC."],"namespace":ns}),
    ));
    msgs.push(global_call(
        21,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["O'BRIEN"],"namespace":ns}),
    ));
    msgs.push(global_call(
        22,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["caret"],"namespace":ns}),
    ));
    msgs.push(global_call(
        23,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["pipes"],"namespace":ns}),
    ));
    msgs.push(global_call(
        24,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["mcode"],"namespace":ns}),
    ));
    // The canonic-number rule, from all three directions.
    msgs.push(global_call(
        30,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":[1],"namespace":ns}),
    ));
    msgs.push(global_call(
        31,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["1"],"namespace":ns}),
    ));
    msgs.push(global_call(
        32,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["01"],"namespace":ns}),
    ));
    // Listing and walking.
    msgs.push(global_call(
        40,
        serde_json::json!({"action":"list","global_name":"IIDTest","namespace":ns}),
    ));
    msgs.push(global_call(
        41,
        serde_json::json!({"action":"list","global_name":"IIDTest","max_subscripts":2,"namespace":ns}),
    ));
    msgs.push(global_call(
        50,
        serde_json::json!({"action":"walk","global_name":"IIDTest","subscripts":["both"],"namespace":ns}),
    ));
    msgs.push(global_call(
        51,
        serde_json::json!({"action":"walk","global_name":"IIDTest","subscripts":["parent"],"namespace":ns}),
    ));
    // Tear the fixtures down whatever the assertions below decide.
    // ── the write half (#532 part two) ──────────────────────────────────────────────────
    // A value and a subscript that would each break naive quoting, written through the tool and
    // read back through it. The read half proved IRIS agrees about ADDRESSING; this proves it
    // agrees about what was stored.
    msgs.push(global_call(
        60,
        serde_json::json!({"action":"set","global_name":"IIDTest",
            "subscripts":["w, \"x\"", 2],
            "value":"line1\nline2 ^NOTAREF |pipe| \"quoted\" \u{00e9}",
            "namespace":ns}),
    ));
    msgs.push(global_call(
        61,
        serde_json::json!({"action":"get","global_name":"IIDTest",
            "subscripts":["w, \"x\"", 2],"namespace":ns}),
    ));
    // The same node again: the second write must report created=false. "I made a node" and "I
    // overwrote somebody's value" are different events, and only the prior $DATA distinguishes them.
    msgs.push(global_call(
        62,
        serde_json::json!({"action":"set","global_name":"IIDTest",
            "subscripts":["w, \"x\"", 2],"value":"second","namespace":ns}),
    ));
    // An empty value is a value: $DATA must come back 1, not 0.
    msgs.push(global_call(
        63,
        serde_json::json!({"action":"set","global_name":"IIDTest","subscripts":["blank"],
            "value":"","namespace":ns}),
    ));
    msgs.push(global_call(
        64,
        serde_json::json!({"action":"get","global_name":"IIDTest","subscripts":["blank"],"namespace":ns}),
    ));
    // The blocklist, through the SERVER. The unit tests show `write_verdict` refuses these; only
    // a live call shows the refusal is REACHED by the dispatch rather than merely present in the
    // module — a guard that exists and is never consulted passes every source-level assertion.
    msgs.push(global_call(
        70,
        serde_json::json!({"action":"set","global_name":"oddDEF","subscripts":["x"],
            "value":"never","namespace":ns}),
    ));
    msgs.push(global_call(
        71,
        serde_json::json!({"action":"set","global_name":"Ens.MessageHeaderD","subscripts":[1],
            "value":"never","namespace":ns}),
    ));
    msgs.push(global_call(
        72,
        serde_json::json!({"action":"set","global_name":"%Dictionary","subscripts":["x"],
            "value":"never","namespace":ns}),
    ));
    msgs.push(global_call(
        73,
        serde_json::json!({"action":"set","global_name":"ENS.Foo","subscripts":[1],
            "value":"never","namespace":ns}),
    ));
    // The positive control for all four: the same shape of call, on an ordinary global, succeeds.
    // Without it a blanket refusal of every write would pass 70-73 while breaking the tool.
    msgs.push(global_call(
        74,
        serde_json::json!({"action":"set","global_name":"IIDTest","subscripts":["allowed"],
            "value":"yes","namespace":ns}),
    ));
    // ── the deletion flow (#532 part three) ────────────────────────────────────────────
    // A subtree of its own, so the preview's node list is exactly known and the kill cannot be
    // confused with the fixtures the earlier assertions still need.
    msgs.push(call(
        79,
        "iris_execute",
        serde_json::json!({"code":
            " Set ^IIDTest(\"doomed\",1)=\"a\"\n Set ^IIDTest(\"doomed\",2)=\"b\"\n              Set ^IIDTest(\"doomed\",2,\"deep\")=\"c\"", "namespace": ns}),
    ));
    // A preview whose cap is smaller than the subtree must mint NOTHING.
    msgs.push(call(
        80,
        "global_preview",
        serde_json::json!({"global_name":"IIDTest","subscripts":["doomed"],
            "max_nodes":2,"namespace":ns}),
    ));
    // The real preview.
    msgs.push(call(
        81,
        "global_preview",
        serde_json::json!({"global_name":"IIDTest","subscripts":["doomed"],"namespace":ns}),
    ));
    // A kill with no token at all.
    msgs.push(call(
        82,
        "global_kill",
        serde_json::json!({"global_name":"IIDTest","subscripts":["doomed"],"namespace":ns}),
    ));
    // A kill with a token that was never issued.
    msgs.push(call(
        83,
        "global_kill",
        serde_json::json!({"global_name":"IIDTest","subscripts":["doomed"],
            "confirm_token":"00000000-0000-0000-0000-000000000000","namespace":ns}),
    ));
    // A preview of a reserved global must refuse rather than hand back an unusable token.
    msgs.push(call(
        84,
        "global_preview",
        serde_json::json!({"global_name":"Ens.MessageHeaderD","namespace":ns}),
    ));
    // Teardown (id 90) is emitted in PHASE TWO below, after the kill, so the kill runs against
    // a subtree that still exists.

    let r = mcp_routines_then(&msgs, |first| {
        // The token exists only now. Everything after it is the part no refusal can stand in for:
        // a deletion that actually happens, and a second attempt with the spent token.
        let token = body(first, 81)["confirm_token"]
            .as_str()
            .unwrap_or_else(|| panic!("phase one must have minted a token: {}", body(first, 81)))
            .to_string();
        let ns = namespace();
        vec![
            call(
                91,
                "global_kill",
                serde_json::json!({"global_name":"IIDTest","subscripts":["doomed"],
                    "confirm_token": token, "namespace": ns}),
            ),
            // The same token again. A kill is not idempotent, so this must be CONSUMED and must
            // not read as a second deletion or as a missing token.
            call(
                92,
                "global_kill",
                serde_json::json!({"global_name":"IIDTest","subscripts":["doomed"],
                    "confirm_token": token, "namespace": ns}),
            ),
            // IRIS is the authority on whether it is gone, not the kill's own report.
            global_call(
                93,
                serde_json::json!({"action":"get","global_name":"IIDTest",
                    "subscripts":["doomed"],"namespace": ns}),
            ),
            // And the siblings the kill must NOT have touched.
            global_call(
                94,
                serde_json::json!({"action":"get","global_name":"IIDTest",
                    "subscripts":["plain"],"namespace": ns}),
            ),
            call(
                90,
                "iris_execute",
                serde_json::json!({"code": " Kill ^IIDTest\n Write \"killed\"",
                    "namespace": ns}),
            ),
        ]
    });

    // The fixtures were written. Without this, every read below could fail for the same one
    // reason and each assertion would report it as its own finding.
    let setup = body(&r, 2);
    assert_eq!(
        setup["success"],
        serde_json::json!(true),
        "the fixtures did not get written, so nothing below is a result about iris_global: {setup}"
    );

    // ── $DATA ─────────────────────────────────────────────────────────────────────────────
    let plain = body(&r, 10);
    assert_eq!(plain["node"]["data"], serde_json::json!(1), "{plain}");
    assert_eq!(
        plain["node"]["value"],
        serde_json::json!("simple"),
        "{plain}"
    );

    let empty = body(&r, 11);
    assert_eq!(empty["node"]["data"], serde_json::json!(1), "{empty}");
    assert_eq!(
        empty["node"]["value"],
        serde_json::json!(""),
        "a node whose value IS the empty string must report it as a value: {empty}"
    );
    assert_eq!(empty["node"]["has_value"], serde_json::json!(true));

    let parent = body(&r, 12);
    assert_eq!(
        parent["node"]["data"],
        serde_json::json!(10),
        "a node with children and no value of its own is $DATA=10: {parent}"
    );
    assert_eq!(
        parent["node"]["value"],
        serde_json::Value::Null,
        "$GET returns \"\" here, and reporting that as a value is the defect: {parent}"
    );
    assert_eq!(parent["node"]["has_value"], serde_json::json!(false));
    assert_eq!(parent["node"]["has_children"], serde_json::json!(true));

    let both = body(&r, 13);
    assert_eq!(both["node"]["data"], serde_json::json!(11), "{both}");
    assert_eq!(both["node"]["value"], serde_json::json!("mine"), "{both}");

    let absent = body(&r, 14);
    assert_eq!(absent["node"]["data"], serde_json::json!(0), "{absent}");
    assert_eq!(absent["node"]["value"], serde_json::Value::Null);

    // ── subscripts and values legacy data contains ───────────────────────────────────────
    assert_eq!(
        body(&r, 20)["node"]["value"],
        serde_json::json!("comma key"),
        "a comma inside a subscript must address one node, not two"
    );
    assert_eq!(
        body(&r, 21)["node"]["value"],
        serde_json::json!("apostrophe key")
    );
    assert_eq!(
        body(&r, 22)["node"]["value"],
        serde_json::json!("A^B(1)"),
        "a caret in a value is data"
    );
    assert_eq!(
        body(&r, 23)["node"]["value"],
        serde_json::json!("a|b\nc|d"),
        "a pipe AND a newline in one value must survive the round trip intact"
    );
    assert_eq!(
        body(&r, 24)["node"]["value"],
        serde_json::json!("S Y=$J(V,10,2)"),
        "M code is a value, not something to interpret"
    );

    // ── the canonic-number rule, measured rather than assumed ────────────────────────────
    let num = body(&r, 30);
    let canonic_str = body(&r, 31);
    let non_canonic = body(&r, 32);
    assert_eq!(num["node"]["value"], serde_json::json!("numeric one"));
    assert_eq!(
        canonic_str["node"]["value"],
        serde_json::json!("numeric one"),
        "IRIS canonicalises \"1\" to 1, so a canonic numeric STRING reads the numeric node"
    );
    assert_eq!(
        num["node"]["name"], canonic_str["node"]["name"],
        "and both report the same canonical reference"
    );
    assert_eq!(
        non_canonic["node"]["value"],
        serde_json::json!("string zero one"),
        "\"01\" is NOT canonic, so it is its own string-subscripted node"
    );
    assert_ne!(
        num["node"]["name"], non_canonic["node"]["name"],
        "1 and \"01\" must not resolve to the same node"
    );

    // ── listing, and the cap that reports itself ─────────────────────────────────────────
    let list = body(&r, 40);
    assert_eq!(
        list["complete"],
        serde_json::json!(true),
        "the whole level fits well under the default page: {list}"
    );
    let subs: Vec<&str> = list["subscripts"]
        .as_array()
        .expect("subscripts")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for expected in ["ACME, INC.", "O'BRIEN", "both", "caret", "empty", "mcode"] {
        assert!(
            subs.contains(&expected),
            "{expected:?} is missing from the level: {subs:?}"
        );
    }

    let capped = body(&r, 41);
    assert_eq!(
        capped["complete"],
        serde_json::json!(false),
        "a level larger than the page must say so: {capped}"
    );
    assert_eq!(capped["stopped_because"], serde_json::json!("cap"));
    assert_eq!(capped["count"], serde_json::json!(2));

    // ── the walk emits the base node's own value ─────────────────────────────────────────
    let walk = body(&r, 50);
    let nodes = walk["nodes"].as_array().expect("nodes");
    assert_eq!(
        walk["complete"],
        serde_json::json!(true),
        "two nodes is a complete subtree: {walk}"
    );
    let datas: Vec<i64> = nodes.iter().filter_map(|n| n["data"].as_i64()).collect();
    assert!(
        datas.contains(&11),
        "the base of ^IIDTest(\"both\") holds a value AND children; $QUERY never returns it, so \
         the walk must emit it itself: {walk}"
    );
    let values: Vec<&str> = nodes.iter().filter_map(|n| n["value"].as_str()).collect();
    assert!(
        values.contains(&"mine") && values.contains(&"theirs"),
        "both the base value and the child must appear: {walk}"
    );

    // ── a $DATA=10 base is emitted too ──────────────────────────────────────────────────
    // Upstream guarded this with `$Data(@baseRef)#2`, which is true only for 1 and 11 — so a base
    // that has children and no value of its own was skipped entirely and the subtree came back
    // starting one level down. Until this case ran live, the only thing holding that behaviour was
    // a source-text assertion that `bst#2` does not appear, which pins a spelling rather than an
    // outcome.
    let walk10 = body(&r, 51);
    let nodes10 = walk10["nodes"].as_array().expect("nodes");
    let base = nodes10
        .iter()
        .find(|n| n["name"].as_str() == Some("^IIDTest(\"parent\")"))
        .unwrap_or_else(|| panic!("the $DATA=10 base is missing from its own walk: {walk10}"));
    assert_eq!(
        base["data"],
        serde_json::json!(10),
        "the base has children and no value of its own: {walk10}"
    );
    assert_eq!(
        base["value"],
        serde_json::Value::Null,
        "and it must not acquire $GET's empty string as a value: {walk10}"
    );
    assert!(
        nodes10.iter().any(|n| n["value"].as_str() == Some("leaf")),
        "the child must be walked as well: {walk10}"
    );

    // ── the write half ──────────────────────────────────────────────────────────────────
    let w = body(&r, 60);
    assert_eq!(
        w["success"],
        serde_json::json!(true),
        "the write itself must land before anything below means anything: {w}"
    );
    assert_eq!(
        w["created"],
        serde_json::json!(true),
        "this node did not exist, so the write created it: {w}"
    );
    assert_eq!(
        w["previous_data"],
        serde_json::json!(0),
        "$DATA before the write was read from IRIS and must be 0: {w}"
    );
    assert_eq!(
        w["data"],
        serde_json::json!(1),
        "and after the write the node has a value and no children: {w}"
    );

    let back = body(&r, 61);
    assert_eq!(
        back["node"]["value"],
        serde_json::json!("line1\nline2 ^NOTAREF |pipe| \"quoted\" \u{00e9}"),
        "every byte written must come back identical — the line feed, the caret, the pipes, the \
         doubled quotes and the accent are DATA, and this is the assertion that proves the \
         escaping round-trips through IRIS rather than merely through our own parser: {back}"
    );
    assert_eq!(
        back["node"]["name"],
        serde_json::json!("^IIDTest(\"w, \"\"x\"\"\",2)"),
        "IRIS's own $NAME must show the comma and the quotes inside ONE subscript, and the \
         numeric subscript bare: {back}"
    );

    let again = body(&r, 62);
    assert_eq!(
        again["created"],
        serde_json::json!(false),
        "the node already had a value, so this write overwrote rather than created: {again}"
    );
    assert_eq!(
        again["previous_data"],
        serde_json::json!(1),
        "and the prior $DATA proves it: {again}"
    );

    let blank_set = body(&r, 63);
    let blank_get = body(&r, 64);
    assert_eq!(
        blank_set["data"],
        serde_json::json!(1),
        "setting the empty string gives the node a value; $DATA is 1, not 0: {blank_set}"
    );
    assert_eq!(
        blank_get["node"]["has_value"],
        serde_json::json!(true),
        "and reading it back agrees: an empty value is not an absent one: {blank_get}"
    );
    assert_eq!(
        blank_get["node"]["value"],
        serde_json::json!(""),
        "the value is the empty string, reported as such rather than as null: {blank_get}"
    );

    // ── the blocklist is reached, and it is not a blanket refusal ───────────────────────
    for (id, global) in [
        (70, "oddDEF"),
        (71, "Ens.MessageHeaderD"),
        (72, "%Dictionary"),
    ] {
        let v = body(&r, id);
        assert_eq!(
            v["error_code"],
            serde_json::json!("SYSTEM_GLOBAL_WRITE_REFUSED"),
            "^{global} is reserved and the live dispatch must refuse it, not merely the unit \
             test: {v}"
        );
        assert_ne!(
            v["success"],
            serde_json::json!(true),
            "^{global} must not report a successful write: {v}"
        );
    }
    let cased = body(&r, 73);
    assert_eq!(
        cased["error_code"],
        serde_json::json!("GLOBAL_FAMILY_UNDETERMINED"),
        "^ENS.Foo differs from ^Ens.Foo only in case, and IRIS treats those as different \
         globals, so the answer is 'I cannot tell' under its own code — not the reserved \
         refusal and not an allow: {cased}"
    );

    let control = body(&r, 74);
    assert_eq!(
        control["success"],
        serde_json::json!(true),
        "the positive control: an ordinary global still accepts the same shape of write, so the \
         four refusals above are the blocklist and not a broken write path: {control}"
    );

    // ── the deletion flow ───────────────────────────────────────────────────────────────
    assert_eq!(
        body(&r, 79)["success"],
        serde_json::json!(true),
        "the doomed fixtures must exist before any of this means anything"
    );

    let capped = body(&r, 80);
    assert_eq!(
        capped["error_code"],
        serde_json::json!("GLOBAL_PREVIEW_NODE_CAP"),
        "a preview that could not enumerate the whole subtree must refuse: {capped}"
    );
    assert!(
        capped.get("confirm_token").is_none(),
        "and it must not hand back a token for a subtree it never counted: {capped}"
    );

    let prev = body(&r, 81);
    assert_eq!(prev["success"], serde_json::json!(true), "{prev}");
    assert_eq!(
        prev["count"],
        serde_json::json!(4),
        "the walk descends AND reports the base: `^IIDTest(\"doomed\")` itself holds no value but \
         has children, so $DATA is 10 there and the Kill takes it too, plus two children and one \
         grandchild: {prev}"
    );
    let token = prev["confirm_token"]
        .as_str()
        .unwrap_or_else(|| panic!("a complete preview issues a token: {prev}"))
        .to_string();
    assert!(!token.is_empty(), "{prev}");

    let no_token = body(&r, 82);
    assert_eq!(
        no_token["error_code"],
        serde_json::json!("GLOBAL_CONFIRM_REQUIRED"),
        "a kill without a token must refuse and name the call to make: {no_token}"
    );
    let bad_token = body(&r, 83);
    assert_eq!(
        bad_token["error_code"],
        serde_json::json!("GLOBAL_CONFIRM_NOT_FOUND"),
        "a token this server never issued is not found, which is a different answer from \
         expired or consumed: {bad_token}"
    );
    let reserved = body(&r, 84);
    assert_eq!(
        reserved["error_code"],
        serde_json::json!("SYSTEM_GLOBAL_WRITE_REFUSED"),
        "previewing a reserved global must refuse at step one rather than issue a token that \
         could never be redeemed: {reserved}"
    );

    // ── the kill itself ─────────────────────────────────────────────────────────────────
    let killed = body(&r, 91);
    assert_eq!(
        killed["success"],
        serde_json::json!(true),
        "the token from the preview must redeem: {killed}"
    );
    assert_eq!(
        killed["removed_nodes"], prev["count"],
        "and it removes exactly the nodes the preview listed — compared against the preview's own \
         count rather than a second literal, so the two cannot drift apart: {killed}"
    );
    assert_eq!(
        killed["remaining_data"],
        serde_json::json!(0),
        "proved by reading $DATA back after the Kill, not by an echoed literal: {killed}"
    );

    let twice = body(&r, 92);
    assert_eq!(
        twice["error_code"],
        serde_json::json!("GLOBAL_CONFIRM_CONSUMED"),
        "the same token a second time is CONSUMED — not NOT_FOUND, which would tell the caller \
         to check for a typo when the fact is that the deletion already ran: {twice}"
    );

    let gone = body(&r, 93);
    assert_eq!(
        gone["node"]["data"],
        serde_json::json!(0),
        "IRIS agrees the subtree is gone: {gone}"
    );
    assert_eq!(
        gone["node"]["has_value"],
        serde_json::json!(false),
        "{gone}"
    );

    let survivor = body(&r, 94);
    assert_eq!(
        survivor["node"]["value"],
        serde_json::json!("simple"),
        "the kill removed the subtree it was authorised for and NOTHING else — a sibling under \
         the same global is still there: {survivor}"
    );

    // ── the fixtures are gone ────────────────────────────────────────────────────────────
    let teardown = body(&r, 90);
    assert_eq!(
        teardown["success"],
        serde_json::json!(true),
        "^IIDTest must be killed again, or this test leaves data behind: {teardown}"
    );
}
