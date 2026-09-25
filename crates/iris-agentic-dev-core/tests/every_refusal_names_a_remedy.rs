//! #329 item 5: every error code this server can emit has a remedy on record.
//!
//! The other four items in #329 fix individual refusals. This one is the property they share, and
//! the reason #310 exists: a refusal that says only what is absent leaves the caller — a model —
//! with a fact about the world and no next step. It then acts on that fact.
//!
//! ## Why this asserts over CODES, not over call sites
//!
//! Measured on master at the time of writing (the numbers are printed by the tests below, never
//! restated in prose — see CLAUDE.md on counts rotting):
//!
//! * `err_json` is called from ~290 places, but **only ~45% pass a literal code.** The rest pass a
//!   classifier call, so a table keyed on literals would cover under half the sites while looking
//!   complete. That is the exact failure shape this repo keeps hitting.
//! * every classifier returns `&'static str`, so the set of codes that can reach a caller IS
//!   enumerable even though the call sites are not usefully so.
//!
//! The producers, and why each is read the way it is:
//!
//! | producer | how a code gets out |
//! |---|---|
//! | `err_json("LITERAL", …)` | the literal itself |
//! | `classify_iris_error_or(msg, FALLBACK)` | the caller's `&'static str` fallback |
//! | `classify_iris_error(msg)` | delegates to the above with `"INTEROP_ERROR"` |
//! | `envelope::http_status_code(u16)` | its own table |
//! | `envelope::auth_error_code(msg)` | its own table |
//! | `envelope::transport_error_code(msg)` | its own table |
//! | `admin::admin_error_code(msg)` | its own table |
//!
//! ## What this test does NOT claim
//!
//! It asserts a remedy is **on record for every code**, reviewed once by a human. It does not judge
//! whether a sentence is *good*, and it does not assert the server puts that sentence in the
//! envelope at runtime — the table here is a review gate, so a NEW code cannot be added without
//! someone writing down what the caller should do. Wiring these into the emitted `hint` is the
//! follow-up, and is deliberately not smuggled in under this test's name.

use std::path::{Path, PathBuf};

/// Remedy on record for every code the server can emit.
///
/// A remedy names an ACTION available to the caller, not a restatement of the failure. "The
/// namespace does not exist" is the failure; "list namespaces with `check_config`, then retry with
/// one of those" is the remedy.
///
/// Adding a code without adding a row here fails `every_emitted_code_has_a_remedy`. That is the
/// whole point: the gate is at authoring time, when the author still knows what the caller should do.
const REMEDIES: &[(&str, &str)] = &[
    ("BODY_READ_ERROR", "the response began but could not be read to the end; retry, and if it repeats capture the partial body — this is a transport fault, not a rejection"),
    ("CREDENTIAL_EXISTS", "a credential of that name is already defined; pick another name, or update the existing one instead of creating it"),
    ("CREDENTIAL_NOT_FOUND", "list the defined credentials with iris_credential_list and use one of those names, or create it first"),
    ("DELETE_FAILED", "the delete was attempted and refused; the message carries the server's reason — check for a lock or a dependent item before retrying"),
    ("DOCKER_REQUIRED", "this path needs a reachable Docker daemon and IRIS_CONTAINER set; start the daemon, or use the HTTP path by setting IRIS_HOST and IRIS_WEB_PORT"),
    ("EXECUTION_FAILED", "the code reached IRIS and IRIS refused it; the message carries the ObjectScript error — fix the code, do not retry unchanged"),
    ("INTERNAL_ERROR", "a bug in this server, not in the request; the message names the failing step — please report it with that text"),
    ("INTEROP_ERROR", "the interop call reached IRIS and failed; the message carries the Ens error — check the production is running and the item name is exact"),
    ("INVALID_PARAM", "one argument is malformed; the message names which — correct it and retry"),
    ("INVALID_PARAMS", "a required argument is missing or empty; the message names which one — supply it and retry"),
    ("INVALID_XML", "the document is not well-formed XML; the message carries the parser's position — fix it there"),
    ("IRIS_AUTH_FAILED", "IRIS rejected the credentials; check IRIS_USERNAME and IRIS_PASSWORD, and that the account is not expired or locked"),
    ("IRIS_BAD_REQUEST", "IRIS rejected the request as malformed; the message carries its reason — this will not succeed on retry unchanged"),
    ("IRIS_CONFLICT", "another writer holds the document or the compile overlapped; retry once after a short pause"),
    ("IRIS_EXECUTE_ERROR", "the execution reached IRIS and failed; the message carries the ObjectScript error — fix the code rather than retrying"),
    ("IRIS_FORBIDDEN", "the account authenticated but lacks the privilege; grant the role the operation needs, or use an account that has it"),
    ("IRIS_HTTP_ERROR", "IRIS answered with an unexpected HTTP status; the message carries it — treat as a server fault and check the instance log"),
    ("IRIS_LOCKED", "the document is checked out or locked by another process; release it, or wait and retry"),
    ("IRIS_REQUEST_FAILED", "the request reached IRIS and came back unusable; the message carries what arrived — check the instance log for the matching entry"),
    ("IRIS_SERVER_ERROR", "IRIS raised a 5xx; this is an instance fault, not a bad request — check the instance log, then retry"),
    ("IRIS_UNREACHABLE", "nothing answered at the configured address; check IRIS_HOST and IRIS_WEB_PORT, and that the instance is up — this is NOT evidence that what you asked for is absent"),
    ("ITEM_EXISTS", "a config item of that name is already in the production; update it, or choose another name"),
    ("ITEM_NOT_FOUND", "list the production's items with iris_production_item and use one of those names — the name must match exactly, including package"),
    ("KEY_NOT_FOUND", "the lookup table has no such key; list the table's keys first, or add the key before reading it"),
    ("MESSAGE_BODY_CLASS_MISSING", "the message header is present but its body class is not compiled in that namespace; compile the class there, or read the header fields with iris_interop_query(what=messages), which does not open the body"),
    ("NAMESPACE_EXISTS", "a namespace of that name is already defined; use it, or pick another name"),
    ("NAMESPACE_NOT_FOUND", "list the available namespaces with check_config and pass one of those — an omitted namespace defaults to USER, which is rarely the interop one"),
    ("NOT_FOUND", "the document or resource is not in that namespace; the message names what IS there when it can — check the namespace and the exact name, suffix included"),
    ("NO_PRODUCTION", "no production is running in that namespace; start one with iris_production, or pass the production name explicitly"),
    ("PARSE_ERROR", "the server's own output could not be parsed; this is a fault in this server or a version mismatch — report it with the message text"),
    ("QUERY_ERROR", "the SQL reached IRIS and IRIS refused it; the message carries the SQLCODE and text — fix the statement rather than retrying"),
    ("SCM_CHECKOUT_FAILED", "the source-control checkout was attempted and refused; the message carries the provider's reason — the document was NOT checked out, so do not write on the assumption that it was"),
    ("SCM_ERROR", "the source-control hook raised an error; the message carries it — resolve it in the provider before retrying the write"),
    ("SCM_REJECTED", "source control declined the operation by policy; the message carries the provider's reason — this needs a change in the provider, not a retry"),
    ("SCM_UNAVAILABLE", "no source-control provider answered; this means UNKNOWN, not 'not under source control' — check the provider is configured before treating the document as free"),
    ("STREAM_READ_ERROR", "the stream could not be read; retry, and if it repeats the document may be corrupt on the server"),
    ("TABLE_NOT_FOUND", "resolve the real table name with iris_table_info or docs_introspect — IRIS table names differ from class names and the separator is not a dot"),
    ("UPDATE_FAILED", "the update was attempted and refused; the message carries the server's reason — re-read the current value before retrying"),
    ("UPLOAD_FAILED", "the document was sent and not accepted; the message carries the server's reason — this is not a transport fault, so retrying unchanged will fail again"),
    ("USER_EXISTS", "a user of that name already exists; modify that account rather than creating it"),
    ("USER_NOT_FOUND", "list users with iris_credential_list or check_config and use an existing name, or create the account first"),
    ("WEBAPP_EXISTS", "a web application is already mapped at that path; edit it, or choose a different path"),
    // Emitted via `envelope::fail_with`, which this file did not scan until #329 — see the note in
    // `vocabulary()`. These four had no remedy on record while being fully reachable.
    ("COMPILE_ERROR", "the source was written but IRIS refused to compile it; the message and compile_console carry the ObjectScript errors — fix those rather than retrying the same source"),
    ("IRIS_RUNTIME_ERROR", "the code ran and IRIS raised an error mid-execution; the message carries the ObjectScript error and location — this will not succeed on retry unchanged"),
    ("SQL_ERROR", "IRIS rejected the SQL; the message carries the SQLCODE and its text — resolve real table and column names with iris_table_info or docs_introspect rather than guessing, then correct the statement"),
    ("WRITE_ABORTED", "the write was cancelled before anything changed — you declined the source-control checkout it needed; re-issue and approve it, or check the document out first"),
    ("WEBAPP_NOT_FOUND", "list the defined web applications and use one of those paths — the path must include its leading slash"),
];

/// Below this many literal codes, assume the scan broke rather than that the server stopped
/// refusing things. A guard that parses nothing reports full coverage.
const MIN_LITERAL_CODES: usize = 25;
/// The classifier tables that must each be located. Zero found = the windows moved.
const MIN_PRODUCERS_FOUND: usize = 4;

fn tools_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push("tools");
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

/// Remove `#[cfg(test)] mod NAME { … }` blocks, brace-matched from the module's OWN brace.
///
/// The naive version of this — "find the next `{` after the attribute" — silently swallowed about a
/// thousand lines of `mod.rs`, because two `#[cfg(test)]` attributes there decorate
/// `pub(crate) const … = &[` rather than a module, so the search jumped past the `[` to an unrelated
/// brace far below. It reported mod.rs as having ZERO production refusals, which is impossible: the
/// file both defines `err_json` and calls it. Requiring the `mod NAME {` shape is what makes the
/// window match the claim.
fn strip_test_mods(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        match find_test_mod_brace(text, i) {
            Some((start, brace)) => {
                out.push_str(&text[i..start]);
                i = match_brace(text, brace);
            }
            None => {
                out.push_str(&text[i..]);
                break;
            }
        }
    }
    out
}

/// `(attribute_start, index_of_the_mod_body_open_brace)` for the next `#[cfg(test)] mod X {`.
fn find_test_mod_brace(text: &str, from: usize) -> Option<(usize, usize)> {
    const ATTR: &str = "#[cfg(test)]";
    let mut at = from;
    while let Some(rel) = text[at..].find(ATTR) {
        let start = at + rel;
        let rest = &text[start + ATTR.len()..];
        // Only whitespace, a visibility, and `mod NAME` may sit between the attribute and the brace.
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
            if let Some(b) = h.find('{') {
                // no other statement may intervene
                if !h[..b].contains(';') {
                    let brace = text.len() - h.len() + b;
                    return Some((start, brace));
                }
            }
        }
        at = start + ATTR.len();
    }
    None
}

/// Index just past the `}` matching the `{` at `open`.
fn match_brace(text: &str, open: usize) -> usize {
    let mut depth = 0i32;
    for (off, ch) in text[open..].char_indices() {
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
    text.len()
}

/// Drop `//` line comments. A code named in a comment is not a code the server emits, and a comment
/// explaining a refusal is exactly where the code name appears in prose.
fn strip_line_comments(text: &str) -> String {
    text.lines()
        .map(|l| match l.find("//") {
            Some(at) => &l[..at],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The upper-case token in `pat"CODE"`, for every occurrence.
fn codes_after(code: &str, pat: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = code[from..].find(pat) {
        let at = from + rel + pat.len();
        let tok: String = code[at..]
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        let after = code[at + tok.len()..].chars().next();
        if !tok.is_empty() && after == Some('"') {
            out.push(tok);
        }
        from = at;
    }
    out
}

/// The `&'static str` fallback passed as the 2nd argument of `classify_iris_error_or(msg, "CODE")`.
fn fallback_codes(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let pat = "classify_iris_error_or(";
    let mut from = 0usize;
    while let Some(rel) = code[from..].find(pat) {
        let open = from + rel + pat.len();
        let end = match_paren(code, open);
        let args = &code[open..end];
        if let Some(comma) = args.rfind(',') {
            let second = args[comma + 1..].trim();
            if let Some(lit) = second.strip_prefix('"').and_then(|s| s.split('"').next()) {
                if !lit.is_empty() && lit.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                    out.push(lit.to_string());
                }
            }
        }
        from = open;
    }
    out
}

fn match_paren(text: &str, after_open: usize) -> usize {
    let mut depth = 1i32;
    for (off, ch) in text[after_open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return after_open + off;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// The body of `fn NAME(` up to its matching brace.
fn fn_body(text: &str, name: &str) -> Option<String> {
    let pat = format!("fn {name}(");
    let at = text.find(&pat)?;
    let brace = text[at..].find('{')? + at;
    Some(text[brace..match_brace(text, brace)].to_string())
}

/// Every code the server can emit, with the producer that can emit it.
fn vocabulary() -> Vec<(String, Vec<String>)> {
    let mut files = Vec::new();
    rust_files(&tools_dir(), &mut files);
    assert!(
        !files.is_empty(),
        "no .rs files under {} — scanning nothing",
        tools_dir().display()
    );

    let mut map: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        Default::default();
    let mut add = |code: String, producer: &str| {
        map.entry(code).or_default().insert(producer.to_string());
    };

    let mut producers_found = 0usize;
    for f in &files {
        let raw = std::fs::read_to_string(f)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", f.display()));
        let prod = strip_line_comments(&strip_test_mods(&raw));
        for c in codes_after(&prod, "err_json(\"") {
            add(c, "literal");
        }
        for c in codes_after(&prod, "err_json_with_url(\"") {
            add(c, "literal");
        }
        // #329: `envelope::fail_with` / `fail` are a THIRD emission route, and this scan missed them
        // entirely. The omission was invisible until item 2 replaced four `err_json("ITEM_NOT_FOUND",
        // …)` calls with one `fail_with("ITEM_NOT_FOUND", …)` helper — at which point
        // `no_remedy_entry_is_stale` declared the code unreachable while the server still emitted it.
        // Measuring the route turned up FOUR live codes with no remedy on record, so the guarantee
        // this file advertises was narrower than its name for as long as it has existed.
        for c in codes_after(&prod, "fail_with(\"") {
            add(c, "fail_with");
        }
        for c in codes_after(&prod, "fail(\"") {
            add(c, "fail");
        }
        for c in fallback_codes(&prod) {
            add(c, "fallback");
        }
        let name = f
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let tables: &[&str] = match name.as_str() {
            "envelope.rs" => &[
                "http_status_code",
                "auth_error_code",
                "transport_error_code",
            ],
            "admin.rs" => &["admin_error_code"],
            _ => &[],
        };
        for t in tables {
            match fn_body(&prod, t) {
                Some(body) => {
                    producers_found += 1;
                    for c in string_literals_upper(&body) {
                        add(c, t);
                    }
                }
                None => panic!(
                    "cannot locate `fn {t}` in {name} — the window moved, so this scan would \
                     silently under-report the vocabulary"
                ),
            }
        }
    }
    assert!(
        producers_found >= MIN_PRODUCERS_FOUND,
        "located only {producers_found} classifier tables (expected >= {MIN_PRODUCERS_FOUND})"
    );
    map.into_iter()
        .map(|(k, v)| (k, v.into_iter().collect()))
        .collect()
}

/// Upper-case string literals in a snippet.
fn string_literals_upper(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'"' {
            if let Some(end) = text[i + 1..].find('"') {
                let lit = &text[i + 1..i + 1 + end];
                if lit.len() >= 3
                    && lit
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                {
                    out.push(lit.to_string());
                }
                i += 1 + end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[test]
fn every_emitted_code_has_a_remedy() {
    let vocab = vocabulary();
    let literal_count = vocab
        .iter()
        .filter(|(_, p)| p.iter().any(|x| x == "literal"))
        .count();
    // CONTROL: the scan found real codes. A parse that matched nothing satisfies the claim below
    // vacuously, and reads exactly like a clean tree.
    assert!(
        literal_count >= MIN_LITERAL_CODES,
        "found only {literal_count} codes emitted as literals (expected >= {MIN_LITERAL_CODES}); \
         the scan broke rather than the refusals disappearing"
    );

    let missing: Vec<String> = vocab
        .iter()
        .filter(|(c, _)| !REMEDIES.iter().any(|(k, _)| k == c))
        .map(|(c, p)| format!("{c} (emitted via {})", p.join(", ")))
        .collect();
    assert!(
        missing.is_empty(),
        "these codes can reach a caller with no remedy on record. Add a row to REMEDIES naming what \
         the caller should DO — not a restatement of the failure:\n  {}",
        missing.join("\n  ")
    );
    eprintln!(
        "codes with a remedy on record: {} ({literal_count} emitted as literals)",
        vocab.len()
    );
}

#[test]
fn no_remedy_entry_is_stale() {
    let vocab = vocabulary();
    let stale: Vec<&str> = REMEDIES
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| !vocab.iter().any(|(c, _)| c == k))
        .collect();
    assert!(
        stale.is_empty(),
        "these REMEDIES rows name codes the server can no longer emit — delete them, or the table \
         rots into fiction and stops being reviewable: {stale:?}"
    );
}

#[test]
fn a_remedy_names_an_action_not_just_the_failure() {
    // Deliberately weak: it pins LENGTH and the absence of the empty string, not quality. Judging
    // "actionable" mechanically is not something this test can honestly claim to do, and a strict
    // keyword rule would be satisfied by pasting the keyword in.
    let thin: Vec<&str> = REMEDIES
        .iter()
        .filter(|(_, r)| r.trim().len() < 40)
        .map(|(k, _)| *k)
        .collect();
    assert!(
        thin.is_empty(),
        "these remedies are too short to name an action; say what the caller should do: {thin:?}"
    );
    let dupes: Vec<&str> = REMEDIES
        .iter()
        .enumerate()
        .filter(|(i, (k, _))| REMEDIES[..*i].iter().any(|(j, _)| j == k))
        .map(|(_, (k, _))| *k)
        .collect();
    assert!(dupes.is_empty(), "duplicate REMEDIES rows: {dupes:?}");
}

#[test]
fn the_scanner_ignores_test_modules_and_comments() {
    // Positive control for all three windows, on a sample whose right answer is known by reading.
    let sample = r#"
fn real(&self) -> X {
    return err_json("REAL_CODE", "m");
}
// err_json("COMMENTED_CODE", "m") — named in prose, not emitted
#[cfg(test)]
pub(crate) const SOME_TABLE: &[&str] = &["NOT_A_CODE"];
fn also_real() -> X {
    return err_json("SECOND_REAL", "m");
}
#[cfg(test)]
mod tests {
    #[test]
    fn t() {
        let _ = err_json("TEST_ONLY_CODE", "m");
    }
}
"#;
    let prod = strip_line_comments(&strip_test_mods(sample));
    let found = codes_after(&prod, "err_json(\"");
    assert!(
        found.iter().any(|c| c == "REAL_CODE"),
        "a production literal must be seen: {found:?}"
    );
    assert!(
        found.iter().any(|c| c == "SECOND_REAL"),
        "a `#[cfg(test)] const` must NOT swallow the code after it — this is the mod.rs bug: {found:?}"
    );
    assert!(
        !found.iter().any(|c| c == "TEST_ONLY_CODE"),
        "a code inside `#[cfg(test)] mod` must be excluded: {found:?}"
    );
    assert!(
        !found.iter().any(|c| c == "COMMENTED_CODE"),
        "a code named in a comment must not count: {found:?}"
    );
    assert_eq!(
        found.len(),
        2,
        "exactly the two production codes, nothing else: {found:?}"
    );
}
