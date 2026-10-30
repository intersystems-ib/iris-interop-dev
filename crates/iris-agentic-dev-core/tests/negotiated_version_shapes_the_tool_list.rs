//! What the rmcp 3.x bump does, and does not, change about the bytes `tools/list`
//! puts on the wire.
//!
//! 3.x adds three `2026-07-28` fields to every paginated result — `resultType`, and
//! the SEP-2549 caching hints `ttlMs` and `cacheScope` — and sends them only to a peer
//! that negotiated that version. That reads like a wire change arriving inside a
//! dependency bump, which is why this file exists. Measured, it is not one, and the
//! reason is worth pinning rather than re-deriving:
//!
//! `2026-07-28` is `ProtocolVersion::NO_INITIALIZE`. SEP-2567 removed the `initialize`
//! handshake there and moved the lifecycle into per-request `_meta`, so rmcp refuses to
//! negotiate it over `initialize` at all — `negotiate_protocol_version` falls back to
//! the newest version that still HAS an initialize handshake, which is `2025-11-25`.
//! Since the caching hints are filled only for a peer on `2026-07-28` or later, an
//! `initialize` client can never be sent them. Every client this server has today, and
//! every other test in this suite, speaks `initialize`.
//!
//! So both tests below assert the SAME legacy shape, and the pair is the point: one
//! client asks for the oldest protocol and one asks for a version newer than any that
//! can be reached this way. If a later bump changes the ceiling, or starts emitting the
//! 2026-07-28 fields to an initialize client, the two fail for different reasons — the
//! first on its version, the second on fields appearing where they cannot today.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// The newest protocol version reachable over an `initialize` handshake. Not a free
/// choice: `2026-07-28` dropped `initialize`, so rmcp falls back to this one.
const NEWEST_OVER_INITIALIZE: &str = "2025-11-25";

/// The three fields rmcp 3.x adds to a paginated result for a `2026-07-28` peer.
const V2026_ONLY_FIELDS: [&str; 3] = ["resultType", "ttlMs", "cacheScope"];

fn iris_dev_bin() -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/iris-agentic-dev-core → crates
    path.pop(); // crates → workspace root
    path.push("target/debug/iris-interop-dev");
    path
}

/// Spawn the server, initialize as a client claiming `client_version`, then ask for the
/// tool list. Returns the version the server negotiated and the `tools/list` result.
///
/// The binary is asserted rather than skipped over. A missing binary means the caller
/// ran `cargo test --test …` without building the bin target first; reporting that as a
/// pass would make this file read as coverage it is not providing.
fn handshake_then_list(client_version: &str) -> (String, serde_json::Value) {
    let bin = iris_dev_bin();
    assert!(
        bin.exists(),
        "no binary at {} — build the workspace before running this test, because an \
         absent binary would otherwise look like a passing protocol assertion",
        bin.display()
    );

    let mut child = Command::new(&bin)
        .arg("mcp")
        // #298: the PAIR disables discovery. IRIS_WEB_PORT alone never enters the env-var
        // leg of the cascade, so the server would fall through to auto-discovery and adopt
        // whatever IRIS is reachable. Port 9 refuses instantly; nothing here needs IRIS.
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", "9")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn iris-interop-dev mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));

    let send = |stdin: &mut std::process::ChildStdin, line: String| {
        stdin.write_all(line.as_bytes()).expect("write");
        stdin.flush().expect("flush");
    };
    let read = |reader: &mut BufReader<std::process::ChildStdout>| {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        serde_json::from_str::<serde_json::Value>(&line)
            .unwrap_or_else(|e| panic!("not JSON-RPC: {e}: {line}"))
    };

    send(
        &mut stdin,
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{client_version}","capabilities":{{}},"clientInfo":{{"name":"wire-shape","version":"0.1"}}}}}}"#
        ) + "\n",
    );
    let init = read(&mut reader);
    let negotiated = init["result"]["protocolVersion"]
        .as_str()
        .unwrap_or_else(|| panic!("initialize carried no protocolVersion: {init}"))
        .to_string();

    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#.to_string() + "\n",
    );
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#.to_string() + "\n",
    );
    let listed = read(&mut reader);

    child.kill().ok();
    child.wait().ok();

    let result = listed["result"].clone();
    // Without this, every "field is absent" assertion below would also pass against an
    // error envelope, which has none of them either.
    assert!(
        result.get("tools").is_some(),
        "tools/list returned no tool array, so any assertion about the result's other \
         fields would be about an error envelope rather than a tool list: {listed}"
    );
    (negotiated, result)
}

/// A client pinned to the oldest protocol keeps it, and sees none of the new fields.
#[test]
fn an_initialize_client_on_the_oldest_protocol_sees_the_legacy_shape() {
    let (negotiated, result) = handshake_then_list("2024-11-05");
    assert_eq!(
        negotiated, "2024-11-05",
        "a pinned client must be answered on its own protocol, not the server's newest"
    );
    for field in V2026_ONLY_FIELDS {
        assert!(
            result.get(field).is_none(),
            "{field} belongs to 2026-07-28 and must not reach a 2024-11-05 client, \
             whose library has no reason to expect it: {result}"
        );
    }
}

/// Asking for the version that DROPPED `initialize` does not get it. The server falls
/// back to the newest version the handshake still exists in, and the result keeps the
/// legacy shape — so no client reachable over `initialize` is sent the new fields.
#[test]
fn asking_over_initialize_for_a_no_initialize_version_falls_back() {
    let (negotiated, result) = handshake_then_list("2026-07-28");
    assert_eq!(
        negotiated, NEWEST_OVER_INITIALIZE,
        "2026-07-28 removed the initialize handshake, so it cannot be negotiated here; \
         the answer is the newest version that still has one"
    );
    for field in V2026_ONLY_FIELDS {
        assert!(
            result.get(field).is_none(),
            "{field} is filled only for a peer on 2026-07-28 or later, which this \
             handshake cannot reach — its presence would mean the caching hints are \
             being sent to clients that never negotiated them: {result}"
        );
    }
}
