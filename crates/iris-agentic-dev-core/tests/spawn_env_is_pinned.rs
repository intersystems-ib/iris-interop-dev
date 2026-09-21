//! #304: every spawn that uses the port-9 "nothing listens here" idiom must also pin `IRIS_HOST`.
//!
//! #298 measured why: `IRIS_WEB_PORT=9` alone never enters the env-var leg of the discovery
//! cascade — that branch is guarded by `IRIS_HOST` — so a spawn setting only the port adopted
//! whatever IRIS happened to be reachable on the machine, reporting
//! `connected:true, connection_source:"auto_discovered", port:8080`. Seven such spawns were fixed
//! in #299/#300 and two were left pending a decision; the decision landed with this guard.
//!
//! Why a guard and not just the fix: the failure is INVISIBLE. A test that silently acquires a
//! connection still passes, and the comment beside it goes on saying no IRIS is present — that is
//! exactly what happened to six spawns and to two doc comments in this file. Nothing about the
//! symptom points at the cause, so the only thing that keeps the invariant is something that reads
//! the source.
//!
//! #304 noted that "a guard built around an undecided policy is worse than none". The policy is
//! now decided and stated here: **pinned by default; an exception must be declared in-line.** A
//! test that genuinely wants auto-discovery writes `spawn-env-guard: intentional auto-discovery`
//! in a comment inside its body, which this guard accepts and reports. An undeclared exception
//! fails. That way the invariant has no silent holes, and a deliberate one is visible in review.

use std::path::{Path, PathBuf};

/// The literal idiom this guard governs. A port read from the environment is a LIVE-IRIS test and
/// none of this applies to it — only the hard-coded discard port means "unreachable on purpose".
const PORT_9: &str = r#"IRIS_WEB_PORT", "9""#;
const PIN: &str = r#""IRIS_HOST""#;
const OPT_OUT: &str = "spawn-env-guard: intentional auto-discovery";

/// Below this many port-9 spawns, assume the scan broke rather than that the idiom vanished.
///
/// There are 9 in `mcp_handshake.rs` on master and 2 more in `cli_compile_wildcard_guards.rs`.
/// The floor is deliberately well under both: it exists to catch a parse that matched almost
/// nothing — which is how a guard reports "all clear" while checking nothing — not to pin a count
/// that legitimately moves when tests are added or retired.
const MIN_SPAWNS: usize = 6;

fn tests_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p
}

/// Every `.rs` file under `tests/`, recursively. A guard that cannot read its inputs must FAIL,
/// not quietly check fewer files, so every IO error here panics.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e} — refusing to report a clean scan",
            dir.display()
        )
    });
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|e| panic!("cannot read an entry in {}: {e}", dir.display()));
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

/// Split a file into `fn` bodies. Crude on purpose: the spawn and its `.env(...)` calls are always
/// in the same function, so "text between one `fn` and the next" is the right unit, and a parser
/// would be a second thing to get wrong.
fn fn_bodies(src: &str) -> Vec<(String, String)> {
    let mut starts: Vec<usize> = src
        .match_indices("\nfn ")
        .chain(src.match_indices("\nasync fn "))
        .chain(src.match_indices("\npub fn "))
        .map(|(i, _)| i)
        .collect();
    starts.sort_unstable();
    let mut out = Vec::new();
    for (i, &s) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(src.len());
        let body = &src[s..end];
        let name = body
            .split(['(', '<'])
            .next()
            .unwrap_or("")
            .rsplit(' ')
            .next()
            .unwrap_or("?")
            .to_string();
        out.push((name, body.to_string()));
    }
    out
}

#[test]
fn every_port_9_spawn_also_pins_iris_host() {
    let mut files = Vec::new();
    rust_files(&tests_dir(), &mut files);
    assert!(
        files.len() > 10,
        "only {} .rs files found under tests/ — the scan is broken, not the tree",
        files.len()
    );

    let mut checked = 0usize;
    let mut unpinned: Vec<String> = Vec::new();
    let mut declared: Vec<String> = Vec::new();

    for file in &files {
        let src = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", file.display()));
        if !src.contains(PORT_9) {
            continue;
        }
        let rel = file
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(file)
            .display()
            .to_string();
        for (name, body) in fn_bodies(&src) {
            if !body.contains(PORT_9) {
                continue;
            }
            checked += 1;
            if body.contains(PIN) {
                continue;
            }
            if body.contains(OPT_OUT) {
                declared.push(format!("{rel}::{name}"));
            } else {
                unpinned.push(format!("{rel}::{name}"));
            }
        }
    }

    // The plausibility control. Without it, a change to the idiom's spelling makes every assertion
    // below vacuous and this test reports success while checking nothing.
    assert!(
        checked >= MIN_SPAWNS,
        "found only {checked} port-9 spawns (expected at least {MIN_SPAWNS}). Either the idiom was \
         respelled and this guard no longer matches it, or the scan broke. Refusing to report a \
         clean result from a search that found almost nothing."
    );

    assert!(
        unpinned.is_empty(),
        "these spawns set IRIS_WEB_PORT=9 without pinning IRIS_HOST, so discovery will adopt \
         whatever IRIS is reachable and the test silently runs against a live instance \
         (#298/#304): {unpinned:#?}\n\nAdd `.env(\"IRIS_HOST\", \"127.0.0.1\")`, or, if the test \
         genuinely wants auto-discovery, put `{OPT_OUT}` in a comment in its body so the exception \
         is declared rather than invisible."
    );

    // Not a failure — but it must be visible, or a declared exception becomes a permanent one
    // nobody revisits.
    if !declared.is_empty() {
        eprintln!("port-9 spawns with a DECLARED auto-discovery exception: {declared:#?}");
    }
    eprintln!("port-9 spawns checked: {checked}, all pinned or declared");
}

/// The guard's own parse, pinned. `fn_bodies` splitting wrongly is the way this whole file becomes
/// decorative: if bodies merge, an unpinned spawn inherits a neighbour's `IRIS_HOST` and passes.
#[test]
fn the_body_split_keeps_neighbouring_fns_apart() {
    let src = "\nfn alpha() {\n    x.env(\"IRIS_HOST\", \"127.0.0.1\").env(\"IRIS_WEB_PORT\", \"9\");\n}\n\
               \nfn beta() {\n    x.env(\"IRIS_WEB_PORT\", \"9\");\n}\n";
    let bodies = fn_bodies(src);
    assert_eq!(bodies.len(), 2, "expected two bodies: {bodies:#?}");
    assert_eq!(bodies[0].0, "alpha");
    assert_eq!(bodies[1].0, "beta");
    assert!(bodies[0].1.contains(PIN), "alpha keeps its pin");
    assert!(
        !bodies[1].1.contains(PIN),
        "beta must NOT inherit alpha's pin — if it does, this guard cannot see an unpinned spawn"
    );
}

/// And the opt-out has to actually be honoured, or the escape hatch is a lie that turns into a
/// failing build the first time someone needs it.
#[test]
fn a_declared_exception_is_recognised_in_a_body() {
    let src =
        format!("\nfn gamma() {{\n    // {OPT_OUT}\n    x.env(\"IRIS_WEB_PORT\", \"9\");\n}}\n");
    let bodies = fn_bodies(&src);
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].1.contains(PORT_9));
    assert!(!bodies[0].1.contains(PIN));
    assert!(
        bodies[0].1.contains(OPT_OUT),
        "the opt-out marker must be found inside the body"
    );
}
