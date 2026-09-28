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
    ("ADMIN_WRITE_DISABLED", "admin write tools are off unless asked for; set IRIS_ADMIN_TOOLS=1 in the server's environment and restart it, or make the change in the Management Portal instead"),
    ("BASELINE_UNAVAILABLE", "the committed side of the comparison could not be read, so no verdict exists; check the production class is readable with iris_doc(mode=get) and retry — an unread baseline is not an empty production"),
    ("BODY_CLASS_NOT_FOUND", "the message's body class is not compiled in this namespace, so the body cannot be projected; compile it with iris_compile, or read the header alone"),
    ("BODY_READ_ERROR", "the response began but could not be read to the end; retry, and if it repeats capture the partial body — this is a transport fault, not a rejection"),
    ("CHECKIN_BLOCKED", "check-in is disabled unless asked for; set IRIS_SCM_ALLOW_CHECKIN=1 to enable it, or check the document in through your own source-control client"),
    ("CLASS_NOT_COMPILED", "the class exists but has no compiled members to read; compile it with iris_compile and retry — nothing was read, so do not conclude it has no methods"),
    ("COMPILE_ERROR", "the source was written but IRIS refused to compile it; the message and compile_console carry the ObjectScript errors — fix those rather than retrying the same source"),
    ("COMPILE_TIME_CODE_BLOCKED", "the write carries a body that runs at compile time; read the generator, then either drop CodeMode=objectgenerator or unset IRIS_BLOCK_CODEGEN for code you have audited"),
    ("CONFIG_ITEM_INVALID", "a config item names a business type IRIS cannot run; the payload names the item — fix its class or remove it from the production, then recompile"),
    ("CONTAINER_NOT_FOUND", "no running container matched that name; the payload lists the ones that are up — pass one of those, or start the container first"),
    ("CONTAINER_UNREACHABLE", "the container exists but its web server did not answer; wait for startup to finish and check the web port is published, then retry"),
    ("CREDENTIAL_EXISTS", "a credential of that name is already defined; pick another name, or update the existing one instead of creating it"),
    ("CREDENTIAL_NOT_FOUND", "list the defined credentials with iris_credential_list and use one of those names, or create it first"),
    ("DELETE_FAILED", "the delete was attempted and refused; the message carries the server's reason — check for a lock or a dependent item before retrying"),
    ("DOCKER_REQUIRED", "this path needs a reachable Docker daemon and IRIS_CONTAINER set; start the daemon, or use the HTTP path by setting IRIS_HOST and IRIS_WEB_PORT"),
    ("ELICITATION_EXPIRED", "pending dialogs are held for five minutes only; re-run the original write to get a fresh one and confirm it promptly"),
    ("ELICITATION_NOT_FOUND", "no pending dialog has that id; re-run the write to get a new one, and note a server restart discards the in-memory store"),
    ("EMPTY_QUERY", "the statement was empty once comments were stripped; send the SQL itself rather than a comment block"),
    ("EXECUTE_FAILED", "the method could not be run at all; the message carries IRIS's reason — check the class is compiled and the arguments match the signature before retrying"),
    ("EXECUTION_FAILED", "the code reached IRIS and IRIS refused it; the message carries the ObjectScript error — fix the code, do not retry unchanged"),
    ("GATEWAY_BAD_OUTPUT", "the connection test returned something that is not a verdict, so whether the gateway works is unknown and no query was sent; re-run the test and treat this as unknown, not as a failed connection"),
    ("GATEWAY_LIST_UNAVAILABLE", "the instance's gateway connections could not be read, so this is NOT an empty list; fix the read failure named in the message, or look at the connections in the Management Portal before concluding none exist"),
    ("GATEWAY_QUERY_FAILED", "the external database rejected the statement and its own error is passed through unchanged; correct the SQL for that database's dialect and retry"),
    ("HL7_EMPTY_SCHEMA_READ", "the read returned no fields, which is not the same as a segment having none; check the version and segment names and re-read before concluding the segment is empty"),
    ("HL7_NOT_AVAILABLE", "the HL7 schema classes are not present in that namespace; work in a namespace where EnsLib HL7 is installed, or install schema support there first"),
    ("HL7_SCHEMA_ERROR", "IRIS refused the schema request and its message is carried through; check the version identifier exists on this instance, then retry"),
    ("INSUFFICIENT_HISTORY", "a skill is mined from recent tool calls and there are too few so far; keep working and retry later, or author the skill by hand"),
    ("INTERNAL_ERROR", "a bug in this server, not in the request; the message names the failing step — please report it with that text"),
    ("INTEROP_ERROR", "the interop call reached IRIS and failed; the message carries the Ens error — check the production is running and the item name is exact"),
    ("INTEROP_NOT_AVAILABLE", "Interoperability is not enabled in that namespace; pick an interop namespace (check_config lists them) or enable interop there before retrying"),
    ("INVALID_ACTION", "the action argument is not one this tool accepts; the message lists the accepted values — resend with one of those"),
    ("INVALID_MESSAGE_ID", "message_id must be the numeric Ens.MessageHeader ID; get it from iris_interop_query what=messages and pass that number"),
    ("INVALID_OUTPUT", "the generated text is not a compilable class and nothing was written; the raw output is in the payload — regenerate it, or fix it and write it with iris_doc"),
    ("INVALID_PARAM", "one argument is malformed; the message names which — correct it and retry"),
    ("INVALID_PARAMS", "a required argument is missing or empty; the message names which one — supply it and retry"),
    ("INVALID_WHAT", "`what` selects which interop view to read; the message lists the valid values — pass one of them"),
    ("INVALID_XML", "the document is not well-formed XML; the message carries the parser's position — fix it there"),
    ("IRIS_AUTH_FAILED", "IRIS rejected the credentials; check IRIS_USERNAME and IRIS_PASSWORD, and that the account is not expired or locked"),
    ("IRIS_BAD_REQUEST", "IRIS rejected the request as malformed; the message carries its reason — this will not succeed on retry unchanged"),
    ("IRIS_CONFLICT", "another writer holds the document or the compile overlapped; retry once after a short pause"),
    ("IRIS_EXECUTE_ERROR", "the execution reached IRIS and failed; the message carries the ObjectScript error — fix the code rather than retrying"),
    ("IRIS_FORBIDDEN", "the account authenticated but lacks the privilege; grant the role the operation needs, or use an account that has it"),
    ("IRIS_HTTP_ERROR", "IRIS answered with an unexpected HTTP status; the message carries it — treat as a server fault and check the instance log"),
    ("IRIS_LOCKED", "the document is checked out or locked by another process; release it, or wait and retry"),
    ("IRIS_REQUEST_FAILED", "the request reached IRIS and came back unusable; the message carries what arrived — check the instance log for the matching entry"),
    ("IRIS_RUNTIME_ERROR", "the code ran and IRIS raised an error mid-execution; the message carries the ObjectScript error and location — this will not succeed on retry unchanged"),
    ("IRIS_SERVER_ERROR", "IRIS raised a 5xx; this is an instance fault, not a bad request — check the instance log, then retry"),
    ("IRIS_UNREACHABLE", "nothing answered at the configured address; check IRIS_HOST and IRIS_WEB_PORT, and that the instance is up — this is NOT evidence that what you asked for is absent"),
    ("ITEM_EXISTS", "a config item of that name is already in the production; update it, or choose another name"),
    ("ITEM_NOT_FOUND", "list the production's items with iris_production_item and use one of those names — the name must match exactly, including package"),
    ("KEY_NOT_FOUND", "the lookup table has no such key; list the table's keys first, or add the key before reading it"),
    ("LEARNING_DISABLED", "the skills tools are opt-in; set OBJECTSCRIPT_LEARNING=true in the server's environment and restart before using them"),
    ("LISTING_UNAVAILABLE", "the namespace's document list could not be read, so the wildcard was NOT expanded and nothing ran; fix the listing failure named in the message, or name the documents explicitly"),
    ("LOG_EXPIRED", "stored output is kept for a limited time and this entry is past it; re-run the tool that produced it — the entry is gone, not empty"),
    ("LOG_NOT_FOUND", "no stored output has that id; use the id from the response that offered it, and note a server restart clears the store"),
    ("MALFORMED_RESULT", "IRIS answered with something this server cannot parse, so no result is reported; the first bytes are in the message — retry, and if it repeats treat it as a bug here rather than as an empty answer"),
    ("MESSAGE_NOT_FOUND", "no body is stored for that message id; confirm the id with iris_interop_query what=messages, and note bodies can be purged while headers remain"),
    ("METHOD_THREW", "the method ran and raised; the exception text is in the payload — fix the cause rather than retrying unchanged"),
    ("MISSING_CLASS", "the production references a class that is not compiled in this namespace; the payload names it — compile that class with iris_compile, then retry"),
    ("MISSING_PARAMETER", "a required argument was absent and nothing was sent to IRIS; the payload lists the accepted parameter names — supply one and retry"),
    ("MISSING_PARAMS", "the mode you asked for needs an argument you did not send; the message names which one — add it and retry"),
    ("MISSING_SESSION_ID", "a trace is read one session at a time; pass the numeric session_id that iris_interop_query what=messages reports for each message"),
    ("MISSING_WHAT", "this tool dispatches on `what` and none was given; the message lists the values — pass one of them"),
    ("NAMESPACE_EXISTS", "a namespace of that name is already defined; use it, or pick another name"),
    ("NAMESPACE_NOT_FOUND", "list the available namespaces with check_config and pass one of those — an omitted namespace defaults to USER, which is rarely the interop one"),
    ("NAMESPACE_NOT_INTEROP", "that namespace has no Ens.* classes, so no interop tool can run in it; choose an interop namespace (check_config lists them) or enable Interoperability there"),
    ("NOT_A_CLASS_METHOD", "this tool calls class methods only; either make the method a ClassMethod, or instantiate the object and call it through iris_execute"),
    ("NOT_FOUND", "the document or resource is not in that namespace; the message names what IS there when it can — check the namespace and the exact name, suffix included"),
    ("NOT_IMPLEMENTED", "this entry point is a stub in this build and does nothing; perform the step by hand and do not wait on it to appear"),
    ("NOT_SQL", "iris_query runs SQL statements only; send ObjectScript through iris_execute instead"),
    ("NO_PRODUCTION", "no production is running in that namespace; start one with iris_production, or pass the production name explicitly"),
    ("PARSE_ERROR", "the server's own output could not be parsed; this is a fault in this server or a version mismatch — report it with the message text"),
    ("PHI_ACK_REQUIRED", "an unredacted body needs acknowledgePhi=true alongside dataPolicy=allow; set both deliberately, or use dataPolicy=redact"),
    ("PHI_POLICY_BLOCKED", "the policy in force blocks message bodies; pass dataPolicy=redact for a masked body, or dataPolicy=allow with acknowledgePhi=true if you are authorised to read PHI"),
    ("PRODUCTION_ALREADY_RUNNING", "another production is already running in that namespace and IRIS will not start a second; the payload names it — stop that one first, or work in the namespace where yours runs"),
    ("PRODUCTION_NOT_FOUND", "no production of that name exists in the namespace; list them with iris_query \"SELECT ID FROM Ens_Config.Production\", then use one of those names or compile the missing class"),
    ("QUERY_ERROR", "the SQL reached IRIS and IRIS refused it; the message carries the SQLCODE and text — fix the statement rather than retrying"),
    ("READ_ERROR", "the document could not be read, so nothing downstream ran; the message carries the reason — check the name and namespace, then retry"),
    ("READ_TRUNCATED", "the document came back cut short and editing it would write the truncation back; do not retry the edit — report this, and make any change through a full put"),
    ("READ_UNREADABLE", "the read returned no content, so the edit was refused rather than applied to nothing; confirm the document with iris_doc(mode=get) first"),
    ("ROUTINE_NOT_FOUND", "the frame names a class that is not compiled here, so it cannot be mapped; compile the class, or read the frame as raw .INT text"),
    ("RULE_NOT_FOUND", "no business rule of that name is in the namespace; call action=list to see which rules are there and use one of those names"),
    ("RULE_NOT_PROJECTED", "the rule class is compiled but neither its Ens_Rule.RuleSet row nor its XData could be read; recompile the rule class so IRIS reprojects it, then retry"),
    ("SCM_CHECKOUT_FAILED", "the source-control checkout was attempted and refused; the message carries the provider's reason — the document was NOT checked out, so do not write on the assumption that it was"),
    ("SCM_ERROR", "the source-control hook raised an error; the message carries it — resolve it in the provider before retrying the write"),
    ("SCM_REJECTED", "source control declined the operation by policy; the message carries the provider's reason — this needs a change in the provider, not a retry"),
    ("SCM_UNAVAILABLE", "no source-control provider answered; this means UNKNOWN, not 'not under source control' — check the provider is configured before treating the document as free"),
    ("SCOPE_REQUIRED", "the pattern would select on its tail alone, which is the whole namespace; qualify it with at least one package level and retry"),
    ("SCRATCH_WRITE_BLOCKED", "this tool reads, but answers by writing a temporary class, and strict read-only refuses that; set IRIS_SOFT_READ_ONLY instead if a scratch class is acceptable — it still refuses every declared mutation"),
    ("SEARCH_PROP_NOT_FOUND", "that property is not registered on the Search Table extent; the payload lists the ones that are — use one of those"),
    ("SEARCH_TABLE_NOT_FOUND", "the search-table class is not registered or not compiled here; compile it, or drop the search_table filter — do not read this as no matches"),
    ("SEARCH_TIMEOUT", "the asynchronous search did not finish in its window, so nothing can be concluded about matches; narrow the document scope or the pattern and run it again"),
    ("SKILLS_PARSE_FAILED", "the registry was read but could not be parsed; IRIS was reachable, so this is NOT an empty registry — inspect the stored content before writing over it"),
    ("SQL_ERROR", "IRIS rejected the SQL; the message carries the SQLCODE and its text — resolve real table and column names with iris_table_info or docs_introspect rather than guessing, then correct the statement"),
    ("SQL_NOT_READ_ONLY", "the statement is not read-only and this path runs SELECTs only; rewrite it as a SELECT, or make the change through a tool that is allowed to write"),
    ("SQL_WRITE_BLOCKED", "a destructive keyword was rejected; resend with force: true if the write is intended, otherwise rewrite the statement as a SELECT"),
    ("STREAM_READ_ERROR", "the stream could not be read; retry, and if it repeats the document may be corrupt on the server"),
    ("TABLE_NOT_FOUND", "resolve the real table name with iris_table_info or docs_introspect — IRIS table names differ from class names and the separator is not a dot"),
    ("TIMEOUT", "the operation did not finish inside its budget and may STILL be running on the server; check the instance's state, then raise timeout or narrow the work before retrying"),
    ("TOOL_NOT_IN_TOOLSET", "the tool exists in this build but the running toolset does not include it; restart the server with --toolset baseline (or IRIS_TOOLSET=baseline) if you need it"),
    ("TOO_BROAD", "the wildcard matched more documents than one request may queue and nothing ran; add the next package level to narrow it and proceed in parts"),
    ("UNKNOWN_ACTION", "the action is not one this tool accepts; the payload lists the valid actions — resend with one of them"),
    ("UNKNOWN_SEGMENT_OR_CATEGORY", "the segment or category is not defined in that schema version; list what the version defines and use one of those names"),
    ("UNKNOWN_TOOL", "no tool of that name exists in this build; list the advertised tools and use one of those names, and check the build with check_config if you expected it to be there"),
    ("UNSUPPORTED_BODY_CLASS", "this tool projects only the body families the message lists; read the body through iris_query against its own table, or convert it first"),
    ("UNSUPPORTED_IRIS_VERSION", "this IRIS build lacks the API the feature needs; the payload names the missing method — use a newer instance or take the manual route"),
    ("UPDATE_FAILED", "the update was attempted and refused; the message carries the server's reason — re-read the current value before retrying"),
    ("UPLOAD_FAILED", "the document was sent and not accepted; the message carries the server's reason — this is not a transport fault, so retrying unchanged will fail again"),
    ("USER_EXISTS", "a user of that name already exists; modify that account rather than creating it"),
    ("USER_NOT_FOUND", "list users with iris_credential_list or check_config and use an existing name, or create the account first"),
    ("VALUE_TRUNCATED", "the value arrived incomplete and is therefore not reported as the result; re-read it in pieces or through a stream rather than using the partial text"),
    ("WEBAPP_EXISTS", "a web application is already mapped at that path; edit it, or choose a different path"),
    ("WEBAPP_NOT_FOUND", "list the defined web applications and use one of those paths — the path must include its leading slash"),
    ("WORKSPACE_NOT_FOUND", "the path does not exist on the host running this server; pass a path that exists there — a path on your own machine is not visible to this process"),
    ("WRITE_ABORTED", "the write was cancelled before anything changed — you declined the source-control checkout it needed; re-issue and approve it, or check the document out first"),
    ("WRITE_GATED", "the write gate is shut, so the mutation was never attempted; the read actions of this tool still work, and the server log names what to set if the gate is shut by a heuristic and writing is intended — an explicitly requested read-only mode is deliberate and has no override"),
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

/// The `"SCREAMING_SNAKE"` literal passed as the FIRST ARGUMENT of `pat`, for every occurrence.
///
/// #361: this used to be handed the pattern with the opening quote attached — `err_json("` — so it
/// saw only calls whose code sits on the same line as the paren. `rustfmt` moves the first argument
/// onto its own line as soon as the message is long, so whether a code was ever reviewed depended
/// on how long its message happened to be. It read 36 of the tree's 90 literal codes while all four
/// tests here were green, and the ones it skipped were the long-message refusals most worth
/// reviewing. Skipping the whitespace between the paren and the literal is the whole fix.
fn codes_after(code: &str, pat: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = code[from..].find(pat) {
        let at = from + rel + pat.len();
        if let Some(tok) = first_arg_code(&code[at..]) {
            out.push(tok);
        }
        from = at;
    }
    out
}

/// The code literal at the head of an argument list. `None` when the first argument is a classifier
/// call, a variable, or a wrapper's context string — those carry a vocabulary somewhere else, which
/// is what `EMITTER_ROUTES` below exists to keep honest.
fn first_arg_code(args: &str) -> Option<String> {
    let rest = args.trim_start().strip_prefix('"')?;
    let tok: String = rest
        .chars()
        .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
        .collect();
    if tok.is_empty() || !rest[tok.len()..].starts_with('"') {
        return None;
    }
    Some(tok)
}

/// Codes written straight into a payload as `"error_code": "CODE"`.
///
/// `McpError::invalid_params(msg, Some(json!({"error_code": "X", ...})))` is a complete emission
/// route that goes nowhere near `err_json`, so none of the other scans can see it.
fn error_code_fields(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0usize;
    let pat = "\"error_code\"";
    while let Some(rel) = code[from..].find(pat) {
        let at = from + rel + pat.len();
        if let Some(rest) = code[at..].trim_start().strip_prefix(':') {
            if let Some(tok) = first_arg_code(rest) {
                out.push(tok);
            }
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
        for c in codes_after(&prod, "err_json(") {
            add(c, "literal");
        }
        for c in codes_after(&prod, "err_json_with_url(") {
            add(c, "literal");
        }
        // #329: `envelope::fail_with` / `fail` are a THIRD emission route, and this scan missed them
        // entirely. The omission was invisible until item 2 replaced four `err_json("ITEM_NOT_FOUND",
        // …)` calls with one `fail_with("ITEM_NOT_FOUND", …)` helper — at which point
        // `no_remedy_entry_is_stale` declared the code unreachable while the server still emitted it.
        // Measuring the route turned up FOUR live codes with no remedy on record, so the guarantee
        // this file advertises was narrower than its name for as long as it has existed.
        for c in codes_after(&prod, "fail_with(") {
            add(c, "fail_with");
        }
        for c in codes_after(&prod, "fail(") {
            add(c, "fail");
        }
        for c in fallback_codes(&prod) {
            add(c, "fallback");
        }
        // A THIRD emission route, found when SCRATCH_WRITE_BLOCKED was added for #303 and this file
        // called the brand-new row stale. `McpError::invalid_params` carries its own
        // `json!({"error_code": ...})` payload and never touches `err_json`, so the route was
        // invisible to every scan above — including `WRITE_GATED`, the write gate's own refusal,
        // which has reached callers with no reviewed remedy for as long as the gate has existed.
        for c in error_code_fields(&prod) {
            add(c, "error_code field");
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
            // #361: `interop_fail` routes 14 sites through `classify_interop_failure`, whose five
            // codes reached callers while no window here read them — the same population gap as
            // the wrapped literals, arriving through a wrapper instead of through rustfmt.
            "interop.rs" => &["classify_interop_failure"],
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
fn wrapped_by_rustfmt() -> X {
    return err_json(
        "WRAPPED_REAL",
        &format!("a message long enough that rustfmt moved the code onto its own line"),
    );
}
fn a_wrapper_passing_context_not_a_code() -> X {
    return transport_fail("handle_something", &e);
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
    let found = codes_after(&prod, "err_json(");
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
    assert!(
        found.iter().any(|c| c == "WRAPPED_REAL"),
        "#361: a call rustfmt wrapped is the same call — whether a code is scanned must not depend \
         on where the literal sits relative to the paren: {found:?}"
    );
    assert_eq!(
        found.len(),
        3,
        "exactly the three production codes, nothing else — a wrapper's context string is not a \
         code: {found:?}"
    );
}

/// The routes read as code emitters, kept beside the scan that uses them.
const EMITTERS: &[&str] = &["err_json(", "err_json_with_url(", "fail_with(", "fail("];

/// Every callee those patterns match, and where its codes come from. A wrapper that passes a
/// context string rather than a code still emits one — from a classifier — and this table is where
/// that is written down.
const EMITTER_ROUTES: &[(&str, &str)] = &[
    (
        "err_json",
        "first argument: a literal, or a classifier whose fallback is read",
    ),
    ("err_json_with_url", "first argument"),
    ("fail", "first argument"),
    ("fail_with", "first argument"),
    (
        "transport_fail",
        "envelope::transport_error_code, read as a producer table",
    ),
    (
        "http_status_fail",
        "envelope::http_status_code, read as a producer table",
    ),
    (
        "interop_fail",
        "interop::classify_interop_failure, read as a producer table",
    ),
    (
        "skills_read_fail",
        "its own match arms, each a literal this scan reads directly",
    ),
    (
        "dict_exec_fail",
        "classify_iris_error_or, whose fallback this scan reads",
    ),
];

/// #361: the control that fails on a PARTIAL scan, which `MIN_LITERAL_CODES` cannot.
///
/// A floor of N codes is cleared as easily by a scan reading 40% of the tree as by one reading all
/// of it — and that floor was itself measured through the broken parse, so it moved with the
/// defect. The population, not the count, is what has to be pinned: every call site these patterns
/// match belongs to a callee named here, with its code source written beside it. A new wrapper —
/// the shape that hid `classify_interop_failure`'s five codes — is then a red rather than a
/// silently smaller number.
#[test]
fn every_emitter_names_where_its_codes_come_from() {
    let mut files = Vec::new();
    rust_files(&tools_dir(), &mut files);
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    let mut unknown: Vec<String> = Vec::new();

    for f in &files {
        let raw = std::fs::read_to_string(f)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", f.display()));
        let prod = strip_line_comments(&strip_test_mods(&raw));
        for pat in EMITTERS {
            let mut from = 0usize;
            while let Some(rel) = prod[from..].find(pat) {
                let at = from + rel;
                // Walk back over the full path so `crate::tools::envelope::fail` and a bare `fail`
                // are the same callee, and `interop_fail` is not mistaken for one.
                let mut start = at;
                while start > 0 {
                    let c = prod[..start].chars().next_back().unwrap_or(' ');
                    if c.is_alphanumeric() || c == '_' || c == ':' {
                        start -= c.len_utf8();
                    } else {
                        break;
                    }
                }
                let callee = prod[start..at + pat.len() - 1]
                    .rsplit("::")
                    .next()
                    .unwrap_or_default()
                    .to_string();
                if EMITTER_ROUTES.iter().any(|(n, _)| *n == callee) {
                    seen.insert(callee);
                } else {
                    let line = prod[..at].matches('\n').count() + 1;
                    unknown.push(format!(
                        "{}:{line} {callee}(",
                        f.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                from = at + pat.len();
            }
        }
    }

    assert!(
        unknown.is_empty(),
        "these callees emit an error envelope and are not in EMITTER_ROUTES, so nothing here knows \
         where their codes come from — add a row naming the source, and a producer table in \
         `vocabulary()` if the codes are not literals:\n  {}",
        unknown.join("\n  ")
    );
    // CONTROL: both kinds were actually walked. A broken walk finds neither, and a walk that finds
    // only bare emitters is the #361 state — the wrappers unexamined.
    assert!(
        seen.contains("err_json") && seen.iter().any(|c| c != "err_json" && c.ends_with("fail")),
        "the walk saw {seen:?} — it must meet both a bare emitter and a wrapper, or it is not \
         reading the tree this test claims to cover"
    );
    eprintln!("emitter callees in the tree: {seen:?}");
}
