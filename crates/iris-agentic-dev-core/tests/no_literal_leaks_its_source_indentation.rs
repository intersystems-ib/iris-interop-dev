//! #378: no product string literal carries a run of leaked source indentation into its VALUE.
//!
//! A Rust string continued with a trailing backslash collapses to single spaces: the `\`, the
//! newline and the next line's leading whitespace all disappear. When that same continuation is
//! COLLAPSED ONTO ONE SOURCE LINE, the indentation stops being layout and becomes content — and the
//! caller sees it:
//!
//! ```text
//! Source control answered with action 2, which means the action has to be                  completed
//! ```
//!
//! Seven such literals were repaired in #376/#377 and `advertised_text_is_clean.rs` shipped a gate
//! over tool DESCRIPTIONS. Neither stopped the next one: on 2026-10-06, one day after those merged,
//! `scm.rs::undriveable_refusal` arrived with three 18-space runs in two refusal messages that reach
//! clients through `err_json` and `Precheck::Refuse`. Descriptions are a population disjoint from
//! refusal details, and the per-producer runtime assertions added for #378 covered two `pub`
//! producers in `interop.rs` and were never extended to the third — a fix that did not propagate
//! across layers. This file is the layer that does not depend on remembering.
//!
//! ## Why this scan is sound, where the naive one was not
//!
//! #378's first pass reported 592 hits across 68 files, all false positives, and concluded that no
//! sound source gate existed because three legitimate classes cannot be told apart by a regex:
//!
//! 1. whitespace-only test fixtures such as `["", "   "]`;
//! 2. embedded ObjectScript whose indentation is deliberate, e.g. `"    Do ..RunUser()"`;
//! 3. aligned trailing comments and `json!` key padding.
//!
//! The answer is not a marker list. It is to stop scanning the SOURCE TEXT and scan the VALUE: this
//! file reconstructs each literal the way the compiler would, then flags it only when a line of the
//! reconstructed value still holds a run of two or more spaces AFTER that line is trimmed. Each
//! class then falls out of the population for a structural reason rather than by being named:
//!
//! * a whitespace-only fixture trims to empty;
//! * deliberate ObjectScript indentation is line-LEADING, so trimming removes it;
//! * aligned comments and `json!` key padding are not inside a literal at all, and the scanner
//!   below never emits a literal from inside a comment;
//! * verbatim IRIS and compiler output, which legitimately contains double spaces, lives inside
//!   `#[cfg(test)]` fixtures, which are stripped brace-matched before scanning.
//!
//! Raw strings are excluded by KIND: a raw string has no `\`-continuation, so it cannot exhibit this
//! defect, and every raw-string hit measured at HEAD was an embedded-ObjectScript statement
//! separator. That exclusion is what keeps the allowlist below at four entries rather than thirteen.
//!
//! ## Why the braces are matched over a MASK and not over the source
//!
//! The brace-matching `strip_test_mods` this file started from — borrowed from
//! `every_refusal_names_a_remedy.rs` — counts `{` and `}` in the raw text, so a brace inside a
//! string literal moves the closing brace. Measured on `gateway_manage.rs`, whose single
//! `#[cfg(test)] mod tests {` spans lines 1430-2899: the raw count closed it at line 1618, leaving
//! 1281 lines of test fixtures in the product population, and one of them —
//! `"<GATEWAY> org.postgresql.util.PSQLException  ERROR: …"`, verbatim JDBC output with a real
//! double space — was reported as a product leak. `assert!(x, "{sql}")` is enough to do it.
//!
//! So the scan runs once and produces two things at the same time: the literals, and a MASK of the
//! same length with every comment, char literal and string body blanked out. Braces are matched over
//! the mask, which is the only version in which they are code. `the_whole_test_mod_is_stripped_even_when_a_literal_holds_a_brace`
//! is the control for it, and `a_mask_is_the_same_length_as_its_source` is the control that the mask
//! is a mask rather than a rewrite.
//!
//! ## What this gate can and cannot claim
//!
//! It is a SHAPE claim: no product literal's reconstructed value carries an internal space run. It
//! says nothing about whether any particular message is reached at runtime. The behavioural half of
//! #378 lives next to each producer — `scm.rs`'s own
//! `no_undriveable_refusal_leaks_its_source_indentation` (`undriveable_refusal` and `CodeMeaning`
//! are `pub(crate)`, so only that module's `#[cfg(test)]` mod can call them), plus the two existing
//! assertions in `message_body_class_missing.rs` and `credential_create_names_the_real_failure.rs`.

use std::path::{Path, PathBuf};

/// Below this many files, assume the walk broke rather than that the crates emptied out.
const MIN_FILES_SCANNED: usize = 45;
/// Below this many reconstructed product literals, assume the scanner broke. This floor only has to
/// be high enough that "scanned nothing" cannot read as "nothing is dirty".
const MIN_PRODUCT_LITERALS: usize = 500;
/// Below this many literals DROPPED as test-mod fixtures, assume the stripping silently did nothing.
/// Without this, an inert `strip` and a tree with no test fixtures print the same clean result.
const MIN_TEST_LITERALS_DROPPED: usize = 500;

/// A literal whose internal run is deliberate, keyed on file name plus a distinctive substring of
/// the RECONSTRUCTED value, with the reason in place.
///
/// Every entry was measured, not assumed: an earlier draft of this list guessed `skills_tools.rs`'s
/// reason as a statement separator, and the scan showed it is a `$translate` replacement SET whose
/// character count is load-bearing. `every_allowlist_entry_still_matches_something` is what forces
/// an entry to be deleted rather than kept once the literal it excuses moves or is repaired.
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "interop.rs",
        "Quit:zk=\"\"",
        "generated ObjectScript: two spaces separating statements on one line of a program, where \
         the For loop must stay on ONE line because build_exec_class splits generated code on \\n",
    ),
    (
        "mod.rs",
        "set method=$order(^rIndex",
        "generated ObjectScript: two spaces separating statements on one line of the %Studio.Debugger \
         source-map program",
    ),
    (
        "skills_tools.rs",
        "^KBCHUNKS",
        "generated ObjectScript: the three spaces are the REPLACEMENT set of \
         $translate(key,q_$CHAR(13,10),\"   \") — three characters in, three out, so collapsing them \
         would change the mapping rather than tidy it",
    ),
    (
        "line_edit.rs",
        "line numbers are stale",
        "deliberate column alignment the caller is meant to read: `found:    ` is padded to line up \
         under `expected: ` in a two-line diff",
    ),
];

// ───────────────────────────── the detector ──────────────────────────────────

/// One non-raw string literal pulled out of a source file.
struct Literal {
    /// Index into the file's `char` sequence of the opening quote.
    offset: usize,
    /// 1-based line of the opening quote.
    line: usize,
    /// The literal's value, as the compiler would build it.
    value: String,
}

/// The result of one pass over a source file.
struct Scan {
    /// The same `char` sequence as the input, with every comment, char literal, raw string and
    /// string literal blanked to spaces and every newline kept where it was. Braces are code only
    /// here.
    mask: Vec<char>,
    literals: Vec<Literal>,
}

/// The longest run of 2+ spaces that survives trimming each line of `value`.
///
/// Trimming per line is the whole scoping decision: a leading run is either layout the compiler put
/// there or indentation the author meant, while an INTERNAL run is content that should have been one
/// space.
fn internal_run(value: &str) -> Option<usize> {
    let mut worst = 0usize;
    for line in value.split('\n') {
        let mut run = 0usize;
        for ch in line.trim().chars() {
            if ch == ' ' {
                run += 1;
                if run > worst {
                    worst = run;
                }
            } else {
                run = 0;
            }
        }
    }
    if worst >= 2 {
        Some(worst)
    } else {
        None
    }
}

/// Decode a non-raw literal body (the text between the quotes) the way the compiler does.
///
/// The case this file exists for is the first arm: `\` + newline + leading whitespace collapses to
/// nothing, which is why a correct continuation yields single spaces and a collapsed one does not.
fn decode(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut it = body.chars().peekable();
    while let Some(ch) = it.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match it.next() {
            // `\` + newline: eat the newline and ALL following whitespace. The compiler's rule, and
            // the reason a correctly continued literal has no run to find.
            Some('\n') | Some('\r') => {
                while let Some(&c) = it.peek() {
                    if c.is_whitespace() {
                        it.next();
                    } else {
                        break;
                    }
                }
            }
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('\\') => out.push('\\'),
            // `\xNN` is decoded rather than placeheld, because `\x20` IS a space.
            Some('x') => {
                let mut hex = String::new();
                for _ in 0..2 {
                    match it.peek() {
                        Some(&c) if c.is_ascii_hexdigit() => {
                            hex.push(c);
                            it.next();
                        }
                        _ => break,
                    }
                }
                match u8::from_str_radix(&hex, 16) {
                    Ok(b) => out.push(b as char),
                    Err(_) => out.push('\u{fffd}'),
                }
            }
            Some('u') => {
                let mut hex = String::new();
                if it.peek() == Some(&'{') {
                    it.next();
                    while let Some(&c) = it.peek() {
                        it.next();
                        if c == '}' {
                            break;
                        }
                        hex.push(c);
                    }
                }
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => out.push('\u{fffd}'),
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn blank(mask: &mut [char], src: &[char], from: usize, to: usize) {
    for k in from..to.min(src.len()) {
        if src[k] != '\n' {
            mask[k] = ' ';
        }
    }
}

/// One pass over a source file, producing every non-raw string literal AND the code mask.
///
/// Comments and literals are recognised together because they are the same job: a `//` inside
/// `"https://host/x"` is not a comment, and a `"` inside `// see "x` does not open a literal. A
/// chain that stripped comments first would truncate every URL literal in the tree at its scheme,
/// which is a silent corruption of the value this gate measures.
fn scan(src: &str) -> Scan {
    let b: Vec<char> = src.chars().collect();
    let mut mask = b.clone();
    let mut literals = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;
    while i < b.len() {
        let c = b[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        // `//` line comment
        if c == '/' && b.get(i + 1) == Some(&'/') {
            let start = i;
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            blank(&mut mask, &b, start, i);
            continue;
        }
        // `/* */` block comment, nested as Rust allows
        if c == '/' && b.get(i + 1) == Some(&'*') {
            let start = i;
            let mut depth = 1usize;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == '\n' {
                    line += 1;
                } else if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 1;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 1;
                }
                i += 1;
            }
            blank(&mut mask, &b, start, i);
            continue;
        }
        // Raw string `r"…"`, `r#"…"#`, `br#"…"#`. Excluded by kind — a raw string cannot carry a
        // `\`-continuation — but it must still be SKIPPED so its contents are not read as code.
        // The preceding char must not be an identifier char, or `for`/`char` would open one.
        let prev_is_ident = i > 0 && (b[i - 1].is_alphanumeric() || b[i - 1] == '_');
        if (c == 'r' || c == 'b') && !prev_is_ident {
            let mut j = i;
            if b[j] == 'b' {
                j += 1;
            }
            if b.get(j) == Some(&'r') {
                j += 1;
                let mut hashes = 0usize;
                while b.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) == Some(&'"') {
                    let start = i;
                    j += 1;
                    while j < b.len() {
                        if b[j] == '\n' {
                            line += 1;
                        }
                        if b[j] == '"' {
                            let mut k = j + 1;
                            let mut seen = 0usize;
                            while seen < hashes && b.get(k) == Some(&'#') {
                                seen += 1;
                                k += 1;
                            }
                            if seen == hashes {
                                j = k;
                                break;
                            }
                        }
                        j += 1;
                    }
                    blank(&mut mask, &b, start, j);
                    i = j;
                    continue;
                }
            }
        }
        // A char literal, or a lifetime. `'\n'` and `'x'` are literals; `'a` and `'static` are not.
        if c == '\'' {
            let close = if b.get(i + 1) == Some(&'\\') {
                b.iter()
                    .skip(i + 2)
                    .position(|&x| x == '\'')
                    .map(|p| i + 2 + p)
            } else if b.get(i + 2) == Some(&'\'') {
                Some(i + 2)
            } else {
                None
            };
            match close {
                Some(end) => {
                    blank(&mut mask, &b, i, end + 1);
                    i = end + 1;
                }
                // a lifetime: the tick is code, nothing is blanked
                None => i += 1,
            }
            continue;
        }
        // A non-raw string literal.
        if c == '"' {
            let start = i;
            let start_line = line;
            let mut body = String::new();
            i += 1;
            while i < b.len() {
                if b[i] == '\\' && i + 1 < b.len() {
                    if b[i + 1] == '\n' {
                        line += 1;
                    }
                    body.push(b[i]);
                    body.push(b[i + 1]);
                    i += 2;
                    continue;
                }
                if b[i] == '"' {
                    i += 1;
                    break;
                }
                if b[i] == '\n' {
                    line += 1;
                }
                body.push(b[i]);
                i += 1;
            }
            blank(&mut mask, &b, start, i);
            literals.push(Literal {
                offset: start,
                line: start_line,
                value: decode(&body),
            });
            continue;
        }
        i += 1;
    }
    Scan { mask, literals }
}

// ─────────────────── `#[cfg(test)]` stripping, brace-matched ──────────────────

fn find_chars(hay: &[char], needle: &str, from: usize) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || hay.len() < n.len() {
        return None;
    }
    (from..=hay.len() - n.len()).find(|&s| hay[s..s + n.len()] == n[..])
}

/// Index just past the `}` matching the `{` at `open`, counted over the MASK.
fn match_brace(mask: &[char], open: usize) -> usize {
    let mut depth = 0i32;
    for (off, ch) in mask[open..].iter().enumerate() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + off + 1;
                }
            }
            _ => {}
        }
    }
    mask.len()
}

/// `(start, end)` for every `#[cfg(test)] mod NAME { … }`, brace-matched from the module's OWN brace.
///
/// Requiring the `mod NAME {` shape is not decoration. `every_refusal_names_a_remedy.rs` records
/// what "the next `{` after the attribute" did: two `#[cfg(test)]` attributes in `mod.rs` decorate a
/// `pub(crate) const … = &[`, so the search jumped past the `[` to an unrelated brace far below and
/// swallowed about a thousand lines — reporting `mod.rs` as having zero production refusals, which
/// is impossible in a file that both defines and calls `err_json`.
fn test_mod_ranges(mask: &[char]) -> Vec<(usize, usize)> {
    const ATTR: &str = "#[cfg(test)]";
    let attr_len = ATTR.chars().count();
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(start) = find_chars(mask, ATTR, at) {
        let after = start + attr_len;
        let rest: String = mask[after..].iter().collect();
        let mut head = rest.trim_start();
        if let Some(h) = head.strip_prefix("pub") {
            head = h.trim_start();
            if head.starts_with('(') {
                if let Some(close) = head.find(')') {
                    head = head[close + 1..].trim_start();
                }
            }
        }
        if let Some(h) = head.strip_prefix("mod ") {
            if let Some(brace_rel) = h.find('{') {
                // No other statement may intervene, or this is `#[cfg(test)] mod x;`.
                if !h[..brace_rel].contains(';') {
                    let before = rest.chars().count() - h.chars().count();
                    let brace = after + before + h[..brace_rel].chars().count();
                    let end = match_brace(mask, brace);
                    out.push((start, end));
                    at = end;
                    continue;
                }
            }
        }
        at = after;
    }
    out
}

fn in_any(ranges: &[(usize, usize)], offset: usize) -> bool {
    ranges.iter().any(|&(s, e)| offset >= s && offset < e)
}

// ───────────────────────────── the walk ──────────────────────────────────────

fn crates_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // …/crates/iris-agentic-dev-core → …/crates
    p.pop();
    p
}

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

/// Every `crates/*/src/**/*.rs`. `tests/` is deliberately out of scope: a test's own fixtures may
/// legitimately hold double spaces, and that is the population that made the naive scan useless.
fn product_files() -> Vec<PathBuf> {
    let crates = crates_dir();
    let mut roots = Vec::new();
    let entries = std::fs::read_dir(&crates)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", crates.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("cannot read an entry: {e}"));
        let src = entry.path().join("src");
        if src.is_dir() {
            roots.push(src);
        }
    }
    assert!(
        roots.len() >= 2,
        "found {} crate src dirs under {} — the layout moved and this gate scanned almost nothing",
        roots.len(),
        crates.display()
    );
    let mut files = Vec::new();
    for r in &roots {
        rust_files(r, &mut files);
    }
    files.sort();
    files
}

/// What the gate measured, so the controls can assert the scan happened.
struct Survey {
    files: usize,
    product_literals: usize,
    dropped_test_literals: usize,
    /// `(file name, line, run, value)` for every flagged literal, allowlist not yet applied.
    flagged: Vec<(String, usize, usize, String)>,
}

fn survey() -> Survey {
    let mut s = Survey {
        files: 0,
        product_literals: 0,
        dropped_test_literals: 0,
        flagged: Vec::new(),
    };
    for path in product_files() {
        let name = path
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or_default()
            .to_string();
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let scanned = scan(&text);
        let ranges = test_mod_ranges(&scanned.mask);
        s.files += 1;
        for lit in &scanned.literals {
            if in_any(&ranges, lit.offset) {
                s.dropped_test_literals += 1;
                continue;
            }
            s.product_literals += 1;
            if let Some(run) = internal_run(&lit.value) {
                s.flagged
                    .push((name.clone(), lit.line, run, lit.value.clone()));
            }
        }
    }
    s
}

fn is_allowed(file: &str, value: &str) -> bool {
    ALLOWED
        .iter()
        .any(|(f, needle, _)| *f == file && value.contains(needle))
}

// ───────────────────────────── the gate ──────────────────────────────────────

#[test]
fn no_product_literal_leaks_its_source_indentation() {
    let s = survey();

    // CONTROLS, first: a clean result must not come from having scanned nothing. Each floor names a
    // different way this gate could become a permanently-green no-op — the walk finding no files,
    // the scanner finding no literals, or the test-mod stripping being inert — and each is the
    // failure mode that would make the assertion below worthless rather than satisfied.
    assert!(
        s.files >= MIN_FILES_SCANNED,
        "scanned only {} files (floor {MIN_FILES_SCANNED}) — the walk broke, so a clean result \
         here would mean nothing",
        s.files
    );
    assert!(
        s.product_literals >= MIN_PRODUCT_LITERALS,
        "reconstructed only {} product literals (floor {MIN_PRODUCT_LITERALS}) — the scanner \
         broke, so a clean result here would mean nothing",
        s.product_literals
    );
    assert!(
        s.dropped_test_literals >= MIN_TEST_LITERALS_DROPPED,
        "dropped only {} literals as test fixtures (floor {MIN_TEST_LITERALS_DROPPED}) — the \
         `#[cfg(test)]` stripping did nothing, so this scan is not the population it claims",
        s.dropped_test_literals
    );

    let leaks: Vec<String> = s
        .flagged
        .iter()
        .filter(|(file, _, _, value)| !is_allowed(file, value))
        .map(|(file, line, run, value)| {
            let shown: String = value.chars().take(160).collect();
            format!("{file}:{line} run={run} {shown:?}")
        })
        .collect();

    assert!(
        leaks.is_empty(),
        "{} product literal(s) carry a run of leaked source indentation into their VALUE. Re-split \
         each one so every `\\`-continuation resumes with the next WORD and has exactly one space \
         before the backslash.\n  {}\n(scanned {} files, {} product literals, {} test literals \
         dropped)",
        leaks.len(),
        leaks.join("\n  "),
        s.files,
        s.product_literals,
        s.dropped_test_literals
    );
}

/// The detector must FIRE. Without this, "nothing is dirty" is indistinguishable from a predicate
/// that can never be true — which is how #378's first attempt produced 592 false positives and,
/// tuned the other way, would have produced a silent zero.
#[test]
fn the_detector_flags_a_collapsed_literal() {
    // The measured `compare_namespace` text, before #376 repaired it.
    let src = "
fn f() {
    let _ = format!(\"namespace_a and namespace_b are both '{ns_a}'. Comparing a namespace with                      itself always reports in_sync.\");
}
";
    let runs: Vec<usize> = scan(src)
        .literals
        .iter()
        .filter_map(|l| internal_run(&l.value))
        .collect();
    assert_eq!(
        runs.len(),
        1,
        "the detector must flag the measured defect exactly once, got {runs:?}"
    );
    assert!(runs[0] >= 3, "run was {}", runs[0]);

    // And the `scm.rs` shape this gate was shipped for: a literal whose `\`-continuations were
    // collapsed onto one source line, 18 spaces each.
    let scm = "
fn f() {
    let _ = format!(
        \"Source control answered with action {code}, which means the action has to be                  completed in the provider's own web UI.\"
    );
}
";
    let scm_runs: Vec<usize> = scan(scm)
        .literals
        .iter()
        .filter_map(|l| internal_run(&l.value))
        .collect();
    assert_eq!(scm_runs, vec![18], "the scm.rs shape must flag at run=18");
}

/// The other load-bearing control, and the one a gate like this normally lacks: a CORRECT multi-line
/// `\`-continuation must NOT be flagged. A gate that cannot show both halves is not evidence, and a
/// gate that reddens on correct code gets deleted rather than obeyed.
#[test]
fn the_detector_does_not_flag_a_correct_continuation() {
    let src = "
fn f() {
    let _ = format!(
        \"Source control answered with action {code}, which means the action has to be \\
         completed in the provider's own web UI — this tool cannot open it, and nothing \\
         was changed.\"
    );
}
";
    let lits = scan(src).literals;
    assert_eq!(lits.len(), 1, "expected one literal, got {}", lits.len());
    assert!(
        lits[0]
            .value
            .contains("has to be completed in the provider's own web UI"),
        "the continuation was not reconstructed as the compiler would: {:?}",
        lits[0].value
    );
    assert!(
        lits[0].value.contains("and nothing was changed."),
        "the second continuation was not reconstructed: {:?}",
        lits[0].value
    );
    assert!(
        internal_run(&lits[0].value).is_none(),
        "a correct continuation was flagged: {:?}",
        lits[0].value
    );

    // Spaces AFTER the backslash are eaten too, so this correct form is clean as well.
    let trailing = "fn f() { let _ = \"one \\\n     two\"; }";
    assert_eq!(scan(trailing).literals[0].value, "one two");
    assert!(internal_run(&scan(trailing).literals[0].value).is_none());
}

/// The three legitimate classes #378 said could not be separated, each excluded by the structure of
/// the predicate rather than by being named in a marker list.
#[test]
fn the_three_legitimate_classes_are_not_flagged() {
    // 1. a whitespace-only fixture: every line trims to empty.
    for lit in scan("fn f() { let _ = [\"\", \"   \", \"\\t\\t\", \"  \\n  \"]; }").literals {
        assert!(
            internal_run(&lit.value).is_none(),
            "a whitespace-only fixture was flagged: {:?}",
            lit.value
        );
    }

    // 2. embedded ObjectScript: the indentation is line-LEADING, so trimming removes it.
    let one = scan("fn f() { let _ = \"    Do ..RunUser()\"; }").literals;
    assert!(
        internal_run(&one[0].value).is_none(),
        "line-leading ObjectScript indentation was flagged: {:?}",
        one[0].value
    );
    let multi =
        scan("fn f() { let _ = \"ClassMethod Run() {\\n    Do ..Go()\\n    Quit\\n}\"; }").literals;
    assert!(
        internal_run(&multi[0].value).is_none(),
        "line-leading indentation after a \\n was flagged: {:?}",
        multi[0].value
    );

    // 3. aligned trailing comments and `json!` key padding are not inside a literal at all.
    let aligned = "fn f() {\n    let _ = 1;        // aligned:      padded comment\n    let _ = json!({ \"a\": 1,  \"bb\": 2 });\n}";
    let flagged: Vec<String> = scan(aligned)
        .literals
        .iter()
        .filter(|l| internal_run(&l.value).is_some())
        .map(|l| l.value.clone())
        .collect();
    assert!(
        flagged.is_empty(),
        "comment alignment or json! padding leaked into a literal: {flagged:?}"
    );
}

/// CONTROL that the stripping actually happened. A comment naming the forbidden construct is the
/// commonest false witness for a source guard, so this feeds the detector the SAME dirty text three
/// times — in a `//` comment, in a `/* */` comment, and inside a `#[cfg(test)] mod` — and then once
/// in product code, which must be the only one seen.
#[test]
fn comments_and_test_mods_are_stripped_before_scanning() {
    let dirty = "leaked                  run";
    let src = format!(
        "// let _ = \"{dirty}\";\n\
         /* let _ = \"{dirty}\"; */\n\
         fn product() {{}}\n\
         #[cfg(test)]\n\
         mod tests {{\n    \
             fn t() {{ let _ = \"{dirty}\"; }}\n\
         }}\n"
    );
    let s = scan(&src);
    let ranges = test_mod_ranges(&s.mask);
    let survivors: Vec<usize> = s
        .literals
        .iter()
        .filter(|l| !in_any(&ranges, l.offset))
        .filter_map(|l| internal_run(&l.value))
        .collect();
    assert!(
        survivors.is_empty(),
        "the stripping did not happen: {} dirty literal(s) survived in comments or a test mod, \
         runs {survivors:?}",
        survivors.len()
    );

    // POSITIVE CONTROL: the very same text in product code IS seen. Without this, the assertion
    // above is equally satisfied by a scanner that finds no literals anywhere.
    let product = format!("fn product() {{ let _ = \"{dirty}\"; }}\n");
    let p = scan(&product);
    let seen: Vec<usize> = p
        .literals
        .iter()
        .filter(|l| !in_any(&test_mod_ranges(&p.mask), l.offset))
        .filter_map(|l| internal_run(&l.value))
        .collect();
    assert_eq!(
        seen,
        vec![18],
        "the detector cannot see the dirty literal even in product code, so the assertion above \
         proves nothing"
    );

    // And a `#[cfg(test)] mod x;` declaration, with no body, must not swallow the rest of the file.
    let decl = "#[cfg(test)]\nmod t;\nfn f() { let _ = \"a                  b\"; }\n";
    let d = scan(decl);
    let after: Vec<usize> = d
        .literals
        .iter()
        .filter(|l| !in_any(&test_mod_ranges(&d.mask), l.offset))
        .filter_map(|l| internal_run(&l.value))
        .collect();
    assert_eq!(
        after,
        vec![18],
        "a bodyless `#[cfg(test)] mod t;` swallowed the product code after it"
    );
}

/// The defect this file's own first draft had, kept as a control.
///
/// Matching braces over the SOURCE closes a test mod at the first `}` that happens to sit inside a
/// string literal. Measured on `gateway_manage.rs` (mod spans 1430-2899): the raw count closed it at
/// 1618 and reported verbatim JDBC output, `"…PSQLException  ERROR: …"`, as a product leak. Matching
/// over the mask is what fixes it, and this is the smallest input that tells the two apart.
#[test]
fn the_whole_test_mod_is_stripped_even_when_a_literal_holds_a_brace() {
    let src = "fn product() {}\n\
               #[cfg(test)]\n\
               mod tests {\n    \
                   fn a() { assert!(x, \"}}}}\"); }\n    \
                   fn b() { let _ = \"verbatim  output\"; }\n\
               }\n";
    let s = scan(src);
    let ranges = test_mod_ranges(&s.mask);
    assert_eq!(ranges.len(), 1, "the test mod was not located at all");

    let survivors: Vec<String> = s
        .literals
        .iter()
        .filter(|l| !in_any(&ranges, l.offset))
        .map(|l| l.value.clone())
        .collect();
    assert!(
        survivors.is_empty(),
        "a brace inside a string literal closed the test mod early, leaving {survivors:?} in the \
         product population"
    );

    // POSITIVE CONTROL for the same input shape: with the braces removed from the literal there is
    // still nothing left over, so the assertion above is about the brace and not about the walk.
    let plain = src.replace("\"}}}}\"", "\"ok\"");
    let p = scan(&plain);
    let left: Vec<String> = p
        .literals
        .iter()
        .filter(|l| !in_any(&test_mod_ranges(&p.mask), l.offset))
        .map(|l| l.value.clone())
        .collect();
    assert!(left.is_empty(), "unexpected survivors: {left:?}");
}

/// The mask must be a MASK: same length, same newlines, same code braces. If it were a rewrite, the
/// offsets the gate uses to place a literal inside or outside a test mod would be meaningless.
///
/// EVERY blanked region in this fixture spans a newline, and that is deliberate. The first version
/// used a one-line comment, a one-line string and a one-line raw string, so no blanked region
/// contained a newline at all — and a `blank` that overwrote newlines SURVIVED, with all nine tests
/// in this file green. The surviving mutant named the weak fixture rather than a missing test.
#[test]
fn a_mask_is_the_same_length_as_its_source() {
    let src = concat!(
        "fn f() {\n",
        "    /* a block comment with a { brace\n",
        "       and a second line */\n",
        "    let _ = \"a } brace spanning \\\n",
        "             two source lines\";\n",
        "    let _ = r#\"raw { }\n",
        "               still raw\"#;\n",
        "    let c = '}';\n",
        "}\n",
    );
    let s = scan(src);
    assert_eq!(
        s.mask.len(),
        src.chars().count(),
        "the mask is not the same length as its source"
    );
    assert_eq!(
        s.mask.iter().filter(|&&c| c == '\n').count(),
        src.chars().filter(|&c| c == '\n').count(),
        "the mask lost or gained a newline"
    );
    // Only the two braces of `fn f() { … }` are code here. The four in a comment, a string, a raw
    // string and a char literal are not.
    assert_eq!(
        s.mask.iter().filter(|&&c| c == '{').count(),
        1,
        "mask: {}",
        s.mask.iter().collect::<String>()
    );
    assert_eq!(
        s.mask.iter().filter(|&&c| c == '}').count(),
        1,
        "mask: {}",
        s.mask.iter().collect::<String>()
    );
}

/// A `//` inside a string, and a `"` inside a comment, are the two ways a strip-then-scan chain
/// corrupts the value this gate measures. Every URL literal in the tree is the first case.
#[test]
fn a_url_literal_is_not_truncated_and_a_quote_in_a_comment_opens_nothing() {
    let url = "fn f() { let _ = \"https://ccr.example/csp/ccr/login.csp?ns=APP\"; }";
    assert_eq!(
        scan(url).literals[0].value,
        "https://ccr.example/csp/ccr/login.csp?ns=APP",
        "the URL literal was truncated at its scheme"
    );

    // An unbalanced quote in a comment must not swallow the following code's literals.
    let quoted = "fn f() {\n    // see \"the docs\n    let _ = \"clean\";\n}";
    let vals: Vec<String> = scan(quoted)
        .literals
        .iter()
        .map(|l| l.value.clone())
        .collect();
    assert_eq!(vals, vec!["clean".to_string()]);

    // A `'"'` char literal does not open a string either.
    let ch = "fn f() { if c == '\"' { let _ = \"clean\"; } }";
    let vals: Vec<String> = scan(ch).literals.iter().map(|l| l.value.clone()).collect();
    assert_eq!(vals, vec!["clean".to_string()]);

    // A lifetime is not a char literal: `'a` must not swallow the code after it.
    let lt = "fn f<'a>(x: &'a str) { let _ = \"clean\"; }";
    let vals: Vec<String> = scan(lt).literals.iter().map(|l| l.value.clone()).collect();
    assert_eq!(vals, vec!["clean".to_string()]);

    // A raw string is excluded by kind and its contents are not read as code.
    let raw = "fn f() { let _ = r#\"Set a=1  Set b=2 \" not a quote\"#; let _ = \"clean\"; }";
    let vals: Vec<String> = scan(raw).literals.iter().map(|l| l.value.clone()).collect();
    assert_eq!(vals, vec!["clean".to_string()]);

    // `for` and `char` begin with the raw-string sigils and must not be mistaken for one.
    let ident = "fn f() { for c in s.chars() { let _ = \"clean\"; } }";
    let vals: Vec<String> = scan(ident)
        .literals
        .iter()
        .map(|l| l.value.clone())
        .collect();
    assert_eq!(vals, vec!["clean".to_string()]);
}

/// An allowlist entry that matches nothing is a permanently-green exemption, and the list only ever
/// grows unless something fails when the literal it excuses moves.
#[test]
fn every_allowlist_entry_still_matches_something() {
    let s = survey();
    assert!(
        !s.flagged.is_empty(),
        "nothing was flagged at all, so this test cannot tell a live entry from a stale one"
    );
    for (file, needle, reason) in ALLOWED {
        let matched = s
            .flagged
            .iter()
            .any(|(f, _, _, value)| f == file && value.contains(needle));
        assert!(
            matched,
            "allowlist entry ({file}, {needle:?}) matches no flagged literal any more — the \
             literal it excused ({reason}) was moved, reworded or repaired, so delete the entry \
             rather than leave a green exemption behind"
        );
    }
    // CONTROL: the allowlist is not swallowing the population. Every flagged literal is accounted
    // for by exactly one entry, so an entry cannot quietly widen to cover a future leak.
    assert_eq!(
        s.flagged.len(),
        ALLOWED.len(),
        "{} literals flagged against {} allowlist entries — if an entry now covers more than one \
         literal, a new leak can hide behind it:\n  {}",
        s.flagged.len(),
        ALLOWED.len(),
        s.flagged
            .iter()
            .map(|(f, l, r, v)| format!(
                "{f}:{l} run={r} {:?}",
                v.chars().take(80).collect::<String>()
            ))
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}
