// E2E regression harness for Docker discovery error messages.
// Community tests: run without --ignored (no license key needed).
// Enterprise tests: #[ignore] — requires IRIS_LICENSE_KEY_PATH env var.
//
// Run community: cargo test --test docker_discovery_e2e
// Run enterprise: IRIS_LICENSE_KEY_PATH=~/license/iris.key cargo test --test docker_discovery_e2e -- --ignored

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

// ── Infrastructure ────────────────────────────────────────────────────────────

fn iris_dev_bin() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("target/debug/iris-interop-dev");
    p
}

/// Spawn iris-dev mcp subprocess with IRIS_CONTAINER set, capture stderr output.
/// Sends the MCP initialize handshake, waits up to 5 seconds, then kills.
fn run_iris_dev_mcp_capture_stderr(container_name: &str, extra_env: &[(&str, &str)]) -> String {
    let bin = iris_dev_bin();
    let mut cmd = Command::new(&bin);
    cmd.args(["mcp"])
        .env("IRIS_CONTAINER", container_name)
        .env("IRIS_USERNAME", "test")
        .env("IRIS_PASSWORD", "test")
        .env("IRIS_TOOLSET", "baseline")
        .env("RUST_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    // Remove env vars that would cause discovery to succeed via other paths
    cmd.env_remove("IRIS_HOST").env_remove("IRIS_WEB_PORT");

    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().expect("failed to spawn iris-dev mcp");
    let mut stdin = child.stdin.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // Send MCP initialize to trigger discovery
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}"#;
    let _ = stdin.write_all((init.to_string() + "\n").as_bytes());
    let _ = stdin.flush();
    drop(stdin); // close stdin so child knows we're done writing

    // Read stderr in a thread. The thread sends only at EOF, and the child holds stderr open
    // until it exits, so the deadline below is really "how long may the child take to finish".
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        let mut output = String::new();
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    output.push_str(&l);
                    output.push('\n');
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(output);
    });

    // #225: this was `.recv_timeout(5s).unwrap_or_default()`, and BOTH halves were wrong.
    //
    // The deadline was measured on an idle machine. Under a full `cargo test --workspace` run this
    // target takes about twice as long as it does alone — 102.72s against 51.46s — so a child that
    // is merely slow trips a 5-second wait even though its output is perfectly good.
    //
    // `unwrap_or_default()` then turned that into an EMPTY STRING, which is the worse half:
    //
    //   * a `contains(...)` assertion failed reporting "expected X, got:" with an empty body,
    //     blaming the message under test for what was actually a deadline;
    //   * `test_auth_401_single_warn` asserts `count <= 1`, and an empty capture SATISFIES that —
    //     so the one test covering 401 de-duplication passed while proving nothing, every time the
    //     deadline fired. A silent timeout that reads as a pass is worse than a flake.
    //
    // Kill first, then take what the reader actually read: killing the child closes stderr, the
    // reader hits EOF and reports. A capture that still never arrives is a panic naming the
    // deadline, never an empty string.
    let output = match rx.recv_timeout(capture_deadline()) {
        Ok(o) => o,
        Err(_) => {
            let _ = child.kill();
            rx.recv_timeout(DRAIN_GRACE).unwrap_or_else(|e| {
                panic!(
                    "capturing stderr from `{} mcp` never completed: {e}. The reader thread did \
                     not report even after the child was killed, so this is not the slow-child \
                     case that {:?} covers.",
                    bin.display(),
                    capture_deadline(),
                )
            })
        }
    };

    let _ = child.kill();
    let _ = child.wait();
    output
}

/// How long the capture may take. Overridable because the right value depends on machine load, and
/// the 5 seconds this replaced had only ever been measured on an idle machine (#225).
fn capture_deadline() -> std::time::Duration {
    const DEFAULT_SECS: u64 = 30;
    std::time::Duration::from_secs(
        std::env::var("IRIS_E2E_CAPTURE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_SECS),
    )
}

/// Grace period for the reader to report AFTER the child has been killed. Short on purpose: stderr
/// is closed by the kill, so EOF is immediate unless something is genuinely wrong.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether a capture proves anything at all.
///
/// The guard exists because `test_auth_401_single_warn`'s assertion is `count <= 1`, which an empty
/// capture satisfies. Any test whose assertion can be satisfied by the ABSENCE of output has to
/// establish that output arrived before it counts anything.
fn captured_something(stderr: &str) -> bool {
    !stderr.trim().is_empty()
}

/// Lines mentioning a 401. Shared by the live test and its unit tests below, so the two cannot
/// disagree about what is being counted.
fn count_401_lines(stderr: &str) -> usize {
    stderr.lines().filter(|l| l.contains("401")).count()
}

/// Start a fresh Docker container and return its name.
/// The container is removed on drop via a cleanup handle.
struct ContainerHandle {
    name: String,
}

impl Drop for ContainerHandle {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

fn start_fresh_container(
    image: &str,
    name: &str,
    web_port: Option<u16>,
    license_key: Option<&str>,
) -> ContainerHandle {
    // Remove any existing container with this name
    let _ = Command::new("docker").args(["rm", "-f", name]).output();

    let mut cmd = Command::new("docker");
    cmd.arg("run").arg("-d").arg("--name").arg(name);

    if let Some(port) = web_port {
        cmd.args(["-p", &format!("{}:52773", port)]);
    }

    if let Some(key) = license_key {
        cmd.args(["-v", &format!("{}:/usr/irissys/mgr/iris.key:ro", key)]);
    }

    cmd.args(["-e", "IRIS_PASSWORD=SYS"]);
    cmd.arg(image);
    cmd.args(["--check-caps", "false"]);

    let output = cmd.output().expect("docker run failed");
    if !output.status.success() {
        panic!(
            "Failed to start container {}: {}",
            name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Wait for IRIS to start
    std::thread::sleep(std::time::Duration::from_secs(25));

    // Create a test user via docker exec (bypass OS-auth-only default)
    let _ = Command::new("docker")
        .args([
            "exec",
            name,
            "iris",
            "session",
            "iris",
            "-U",
            "%SYS",
            "##class(Security.Users).Create(\"test\",\"%ALL\",\"test\")",
        ])
        .output();

    ContainerHandle {
        name: name.to_string(),
    }
}

// ── Phase 3/US1: Container not found ─────────────────────────────────────────

/// T017: IRIS_CONTAINER pointing to a nonexistent container — "not found in Docker"
#[test]
fn test_container_not_found_message() {
    let stderr = run_iris_dev_mcp_capture_stderr("definitely-not-running-container-xyz", &[]);
    println!("stderr: {}", stderr);
    assert!(
        stderr.contains("not found in Docker"),
        "expected 'not found in Docker' in stderr, got:\n{}",
        stderr
    );
    assert!(
        !stderr.contains("not reachable via Docker"),
        "old generic message must not appear, got:\n{}",
        stderr
    );
}

// ── Phase 4/US3: Port not mapped ─────────────────────────────────────────────

/// T024: Container running but port 52773 NOT mapped — "port 52773 is not mapped"
#[test]
fn test_port_not_mapped_message() {
    let _container = start_fresh_container(
        "containers.intersystems.com/intersystems/iris-community:2026.1",
        "e2e-nomapped",
        None, // no port mapping
        None,
    );

    let stderr = run_iris_dev_mcp_capture_stderr("e2e-nomapped", &[]);
    println!("stderr: {}", stderr);

    assert!(
        stderr.contains("port 52773 is not mapped"),
        "expected 'port 52773 is not mapped' in stderr, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("iris_execute") || stderr.contains("docker exec"),
        "expected docker exec note in stderr, got:\n{}",
        stderr
    );
    assert!(
        !stderr.contains("not reachable via Docker"),
        "old generic message must not appear"
    );
}

// ── Phase 6/US4: 401 dedup ────────────────────────────────────────────────────

/// T040: Community container without IRIS_PASSWORD — exactly one 401 warn
#[test]
fn test_auth_401_single_warn() {
    // Start community container without IRIS_PASSWORD so _SYSTEM gets OS auth only
    let _ = Command::new("docker")
        .args(["rm", "-f", "e2e-nopassword"])
        .output();
    let mut cmd = Command::new("docker");
    cmd.args([
        "run",
        "-d",
        "--name",
        "e2e-nopassword",
        "-p",
        "52796:52773",
        "containers.intersystems.com/intersystems/iris-community:2026.1",
        "--check-caps",
        "false",
    ]);
    let _ = cmd.output();
    std::thread::sleep(std::time::Duration::from_secs(25));
    let _cleanup = ContainerHandle {
        name: "e2e-nopassword".to_string(),
    };

    let stderr = run_iris_dev_mcp_capture_stderr("e2e-nopassword", &[]);
    println!("stderr: {}", stderr);

    // The positive control, without which this whole test is vacuous: `count <= 1` below is
    // satisfied by an empty capture, so a count of 0 must mean "the real output had no 401 line",
    // never "there was no output" (#225).
    assert!(
        captured_something(&stderr),
        "captured no stderr at all, so the 401 assertions below would pass without testing \
         anything. Raise IRIS_E2E_CAPTURE_SECS if this machine is slow."
    );

    let warn_401_count = count_401_lines(&stderr);
    assert!(
        warn_401_count <= 1,
        "expected at most 1 line mentioning 401, got {}:\n{}",
        warn_401_count,
        stderr
    );
    if warn_401_count == 1 {
        assert!(
            !stderr.contains("not found or not reachable"),
            "old generic second warn must not appear after 401"
        );
    }
}

// ── Phase 5/US2: Web server absent (enterprise) ───────────────────────────────

/// T032: Enterprise iris:2026.1 — "Atelier REST API is not responding" + enterprise hint
#[test]
#[ignore = "requires live enterprise container (IRIS_LICENSE_KEY_PATH env var)"]
fn test_enterprise_web_server_absent_message() {
    let key = std::env::var("IRIS_LICENSE_KEY_PATH")
        .expect("IRIS_LICENSE_KEY_PATH must be set for enterprise tests");

    let _container = start_fresh_container(
        "containers.intersystems.com/intersystems/iris:2026.1",
        "e2e-enterprise",
        Some(52797),
        Some(&key),
    );

    let stderr = run_iris_dev_mcp_capture_stderr("e2e-enterprise", &[]);
    println!("stderr: {}", stderr);

    assert!(
        stderr.contains("Atelier REST API is not responding"),
        "expected 'Atelier REST API is not responding' in stderr, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("iris-community")
            || stderr.contains("Web Gateway")
            || stderr.contains("irishealth-community"),
        "expected enterprise hint text in stderr, got:\n{}",
        stderr
    );
    assert!(
        !stderr.contains("WebServer=1"),
        "must NOT suggest WebServer=1 CPF (crashes enterprise), got:\n{}",
        stderr
    );
}

// ── Phase 7/US5: Full regression harness ─────────────────────────────────────

/// T047: Community regression — iris-community:2026.1 and irishealth-community:2026.1
#[test]
fn test_all_community_images() {
    // iris-community: port not mapped → port-not-mapped message
    let _c1 = start_fresh_container(
        "containers.intersystems.com/intersystems/iris-community:2026.1",
        "e2e-reg-community",
        None,
        None,
    );
    let stderr1 = run_iris_dev_mcp_capture_stderr("e2e-reg-community", &[]);
    assert!(
        stderr1.contains("port 52773 is not mapped"),
        "iris-community without port mapping: expected port-not-mapped message, got:\n{}",
        stderr1
    );

    // irishealth-community: port not mapped → same message
    let _c2 = start_fresh_container(
        "containers.intersystems.com/intersystems/irishealth-community:2026.1",
        "e2e-reg-irishealth-community",
        None,
        None,
    );
    let stderr2 = run_iris_dev_mcp_capture_stderr("e2e-reg-irishealth-community", &[]);
    assert!(
        stderr2.contains("port 52773 is not mapped"),
        "irishealth-community without port mapping: expected port-not-mapped message, got:\n{}",
        stderr2
    );
}

/// T048: Enterprise regression — iris:2026.1 and irishealth:2026.1
#[test]
#[ignore = "requires live enterprise containers (IRIS_LICENSE_KEY_PATH env var)"]
fn test_all_enterprise_images() {
    let key = std::env::var("IRIS_LICENSE_KEY_PATH")
        .expect("IRIS_LICENSE_KEY_PATH must be set for enterprise tests");

    // iris enterprise: web server absent → Atelier not responding
    let _c1 = start_fresh_container(
        "containers.intersystems.com/intersystems/iris:2026.1",
        "e2e-reg-enterprise",
        Some(52798),
        Some(&key),
    );
    let stderr1 = run_iris_dev_mcp_capture_stderr("e2e-reg-enterprise", &[]);
    assert!(
        stderr1.contains("Atelier REST API is not responding"),
        "iris enterprise: expected Atelier-not-responding message, got:\n{}",
        stderr1
    );

    // irishealth enterprise: same
    let _c2 = start_fresh_container(
        "containers.intersystems.com/intersystems/irishealth:2026.1",
        "e2e-reg-irishealth-enterprise",
        Some(52799),
        Some(&key),
    );
    let stderr2 = run_iris_dev_mcp_capture_stderr("e2e-reg-irishealth-enterprise", &[]);
    assert!(
        stderr2.contains("Atelier REST API is not responding"),
        "irishealth enterprise: expected Atelier-not-responding message, got:\n{}",
        stderr2
    );
}

/// Unit tests for the capture guards. These need no Docker and no container, which is the point:
/// the defect in #225 was in how a capture was TURNED INTO a verdict, and that is testable without
/// reproducing the slow machine that exposed it.
#[cfg(test)]
mod capture_guard_tests {
    use super::*;

    /// Why the guard exists, as a test. `test_auth_401_single_warn` asserts `count <= 1`; an empty
    /// capture yields 0, which satisfies it. So emptiness has to be rejected BEFORE counting, or
    /// the test reports a pass having examined nothing.
    #[test]
    fn an_empty_capture_would_satisfy_the_401_assertion() {
        assert_eq!(
            count_401_lines(""),
            0,
            "nothing to count in an empty capture"
        );
        assert!(
            count_401_lines("") <= 1,
            "this is the real test's assertion, and emptiness meets it"
        );
        assert!(
            !captured_something(""),
            "so the guard must reject an empty capture"
        );
    }

    /// Whitespace is not output either — a child that emitted only a newline proves no more than
    /// one that emitted nothing.
    #[test]
    fn whitespace_only_is_not_a_capture() {
        assert!(!captured_something("   \n  \t \n"));
        assert!(captured_something("warn: got HTTP 401 from Atelier"));
    }

    /// The assertion must still be able to FAIL, or guarding it changes nothing: two 401 lines
    /// exceed the limit the live test allows.
    #[test]
    fn a_duplicated_401_warn_exceeds_the_limit() {
        let two = "warn: HTTP 401 unauthorized\nwarn: HTTP 401 unauthorized again\n";
        assert!(captured_something(two), "control: this capture is real");
        assert_eq!(count_401_lines(two), 2);
        assert!(
            count_401_lines(two) > 1,
            "the de-duplication failure the live test exists to catch"
        );
    }

    /// Only the mentioning lines count, not every line of a real capture.
    #[test]
    fn unrelated_lines_are_not_counted() {
        let mixed = "info: discovery starting\nwarn: HTTP 401 unauthorized\ninfo: done\n";
        assert_eq!(count_401_lines(mixed), 1);
        assert!(captured_something(mixed));
    }
}
