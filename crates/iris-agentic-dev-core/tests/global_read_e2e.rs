//! #532 e2e: `iris_global`'s reads against a live instance, with IRIS itself as the authority.
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

/// Speak MCP to a `routines`-profile server over stdio.
///
/// `IRIS_TOOLSET=routines` is not incidental: `iris_global` is advertised by that profile only, so
/// started on the fork's default the server would answer "tool not found" and every assertion
/// below would be unreachable for a reason unrelated to the behaviour under test.
fn mcp_routines(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
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
fn a_global_read_reports_what_iris_actually_holds() {
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
    msgs.push(call(
        90,
        "iris_execute",
        serde_json::json!({"code": " Kill ^IIDTest\n Write \"killed\"", "namespace": ns}),
    ));

    let r = mcp_routines(&msgs);

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

    // ── the fixtures are gone ────────────────────────────────────────────────────────────
    let teardown = body(&r, 90);
    assert_eq!(
        teardown["success"],
        serde_json::json!(true),
        "^IIDTest must be killed again, or this test leaves data behind: {teardown}"
    );
}
