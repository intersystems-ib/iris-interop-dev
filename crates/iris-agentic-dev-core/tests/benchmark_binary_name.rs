//! #340: no benchmark spawn may hardcode the binary it runs.
//!
//! The fork renamed the server binary `iris-dev` -> `iris-interop-dev`. Three spawn sites in
//! `benchmark/021/runner/` kept the old name, and that does not fail cleanly, because the failure
//! depends on what is installed:
//!
//! * nothing named `iris-dev` on PATH -> `FileNotFoundError`, which each call site turned into an
//!   empty result (`{}`, `[]`, or the string "unknown");
//! * a stale pre-rename copy on PATH -> the exec SUCCEEDS and the child is then killed. The machine
//!   this was found on has a root-owned `/usr/local/bin/iris-dev` dated five months before the
//!   rename; it returns rc=-9 (SIGKILL) with an empty stdout.
//!
//! The second is the dangerous one. `namespace.py::_mcp_call` returned `{}`, its caller read
//! `content = ""`, and `if "ERROR" in content` is then FALSE -- so `reset_benchmark_namespace()`
//! reported success though the server never ran, and a 15-task condition proceeded against a
//! namespace that was never dropped. That is the carry-over FR-001b exists to prevent, arriving as
//! clean-looking data.
//!
//! ## What this guard pins, and why that shape
//!
//! Not "the string `iris-dev` is absent" -- `binary.py` MUST name it, to say so in the error when it
//! finds one on PATH. The durable property is structural: **every spawn passes a resolved value as
//! argv[0], never a string literal.** A literal is how the stale name got three call sites and how
//! the next rename would get three more.
//!
//! The window is the argument list of each `subprocess.run(`/`subprocess.Popen(` call -- not the
//! file, not a fixed byte span. A guard whose haystack is wider than its claim passes on unrelated
//! text, which happened five times in this repo in one day: `report.py` puts "iris-dev Path
//! Benchmark" in an HTML <title> and `toolset_tracker.py` says "After spawning iris-dev" in a
//! docstring. Neither is a spawn.
//!
//! ## Why this is in the Rust suite
//!
//! There are 12 python test files under `benchmark/021/tests/` and NO workflow runs pytest, so a
//! python test beside them would never be enforced:
//!     grep -rl pytest .github/workflows/     # no matches
//! This target is in the required gate, so it is the only place a regression gets caught.

use std::path::{Path, PathBuf};

/// The file allowed to name the retired binary, because its job is to recognise it.
const RESOLVER: &str = "binary.py";

/// Below this many spawn sites, assume the scan broke rather than that every spawn vanished.
const MIN_SPAWNS: usize = 2;

fn runner_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // repo root
    p.push("benchmark");
    p.push("021");
    p.push("runner");
    p
}

fn python_files(dir: &Path, out: &mut Vec<PathBuf>) {
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
            python_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "py") {
            out.push(path);
        }
    }
}

/// Strip `#` comments and triple-quoted blocks, leaving executable code.
///
/// Both matter. `toolset_tracker.py`'s docstring names the retired binary, and `report.py` holds an
/// entire HTML template — including the word — inside a triple-quoted string. A guard that greps raw
/// source fires on both forever and teaches the next reader to delete it. This bit twice in one day
/// here: a comment reading "not `lines().next()`" tripped the guard for that very fix.
fn code_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with("\"\"\"") || rest.starts_with("'''") {
            let delim = &rest[..3];
            match rest[3..].find(delim) {
                Some(end) => {
                    i += 3 + end + 3;
                    continue;
                }
                None => break,
            }
        }
        let ch = rest.chars().next().unwrap();
        if ch == '#' {
            match rest.find('\n') {
                Some(nl) => {
                    i += nl;
                    continue;
                }
                None => break,
            }
        }
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The argument text of every `subprocess.run(` / `subprocess.Popen(` call, paren-balanced.
///
/// Balanced rather than a fixed window: these calls span several lines and carry nested `[...]` and
/// `{...}`, and a byte-count window would either truncate the argv or run past the call into the
/// next statement.
fn spawn_arg_lists(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    for pat in ["subprocess.run(", "subprocess.Popen("] {
        let mut from = 0usize;
        while let Some(rel) = code[from..].find(pat) {
            let open = from + rel + pat.len();
            let mut depth = 1i32;
            let mut j = open;
            for ch in code[open..].chars() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += ch.len_utf8();
            }
            out.push(code[open..j.min(code.len())].to_string());
            from = open;
        }
    }
    out
}

/// Quoted string tokens, either quote style.
fn quoted_tokens(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = code.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let q = bytes[i];
        if q == b'"' || q == b'\'' {
            if let Some(end) = code[i + 1..].find(q as char) {
                out.push(code[i + 1..i + 1 + end].to_string());
                i += 1 + end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// A spawn's argv[0] must not be a string literal. `["a", "b"]` is hardcoded; `[path, "mcp"]` is not.
fn argv0_literal(args: &str) -> Option<String> {
    let open = args.find('[')?;
    let close = args[open..].find(']')? + open;
    let first = args[open + 1..close].split(',').next()?.trim().to_string();
    let b = first.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        return Some(first[1..first.len() - 1].to_string());
    }
    None
}

#[test]
fn no_benchmark_spawn_hardcodes_its_binary() {
    let mut files = Vec::new();
    python_files(&runner_dir(), &mut files);
    assert!(
        !files.is_empty(),
        "no python files under {} — the guard is scanning nothing",
        runner_dir().display()
    );

    let mut spawns = 0usize;
    let mut offenders = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", f.display()));
        for args in spawn_arg_lists(&code_only(&text)) {
            spawns += 1;
            if let Some(lit) = argv0_literal(&args) {
                offenders.push(format!("{}: argv[0] is the literal {lit:?}", f.display()));
            }
        }
    }

    // CONTROL: the parser found real spawn calls. A parse that matched nothing reports "all clear"
    // exactly like a clean tree — the failure mode this repo hits most.
    assert!(
        spawns >= MIN_SPAWNS,
        "found only {spawns} spawn call(s) across {} files (expected >= {MIN_SPAWNS}). The parser \
         broke rather than the tree being clean.",
        files.len()
    );

    assert!(
        offenders.is_empty(),
        "these spawns hardcode a binary name; resolve it through runner/binary.py instead:\n  {}",
        offenders.join("\n  ")
    );
    eprintln!(
        "spawn call sites checked: {spawns} across {} files",
        files.len()
    );
}

#[test]
fn only_the_resolver_names_the_retired_binary() {
    let mut files = Vec::new();
    python_files(&runner_dir(), &mut files);
    let mut named_in = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", f.display()));
        if quoted_tokens(&code_only(&text))
            .iter()
            .any(|t| t == "iris-dev")
        {
            named_in.push(
                f.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }
    // CONTROL, not decoration: the resolver MUST still name it. If this list is empty the scan is
    // broken, and an empty list would otherwise satisfy the "only the resolver" claim vacuously.
    assert!(
        named_in.contains(&RESOLVER.to_string()),
        "{RESOLVER} no longer names the retired binary — either the scan broke, or the resolver can \
         no longer tell the user a stale iris-dev is what it found. Saw: {named_in:?}"
    );
    let strays: Vec<_> = named_in.iter().filter(|f| *f != RESOLVER).collect();
    assert!(
        strays.is_empty(),
        "only {RESOLVER} may name the retired binary (it recognises it to report it); these also \
         do: {strays:?}"
    );
}

#[test]
fn the_parser_sees_code_and_ignores_prose() {
    // Positive control for both halves of the window: an argv literal MUST be caught, and prose
    // naming the same binary must NOT be.
    let sample = "\"\"\"After spawning iris-dev, the client calls tools/list.\"\"\"\n\
                  # iris-dev is the old name\n\
                  TITLE = \"iris-dev Path Benchmark\"\n\
                  a = subprocess.Popen([\"iris-dev\", \"mcp\"], stdin=PIPE)\n\
                  b = subprocess.run([path, \"--version\"], capture_output=True)\n";
    let code = code_only(sample);
    assert!(
        !code.contains("After spawning"),
        "the docstring survived stripping: {code:?}"
    );
    assert!(
        !code.contains("is the old name"),
        "the # comment survived stripping: {code:?}"
    );
    let args = spawn_arg_lists(&code);
    assert_eq!(args.len(), 2, "both spawns must be found: {args:?}");
    let lits: Vec<_> = args.iter().filter_map(|a| argv0_literal(a)).collect();
    assert_eq!(
        lits,
        vec!["iris-dev".to_string()],
        "exactly the hardcoded spawn must be flagged, and the resolved one must not: {lits:?}"
    );
}
