//! Decoding a `%Status` chain that arrived as TEXT.
//!
//! #323: every time the model drives an IRIS API through `iris_execute` it hand-writes the decode
//! itself — 785 calls over 364 transcripts, and the single largest failure cluster in the workshop
//! cohort (56 of 559). What comes back is whatever the script printed, so the chain reaches the
//! caller as one opaque string.
//!
//! ## What was measured, and what the issue got wrong
//!
//! #323's stated root cause is that `$SYSTEM.Status.GetErrorText` "only ever reads the first error
//! of the chain", so errors 2..n are lost. Measured on IRIS 2026.1 against a two-element chain
//! built with `$system.Status.AppendStatus` (throwaway probe class, deleted afterwards), it
//! returns the WHOLE chain, CRLF-joined:
//!
//! ```text
//! ERROR #5002: ObjectScript error: first problem<CR><LF>ERROR #6301: SAX XML Parser Error: second problem
//! ```
//!
//! Controls: a single-element status decomposes to a count of 1; an OK status gives `IsError` 0 and
//! an empty text. So nothing is lost today, and this module is not recovering dropped errors.
//!
//! What IS missing is structure. The chain arrives as one string with **no per-error code**, so a
//! consumer that wants to key a remedy off error 6301 has to match a substring of a concatenation
//! (which is what `builtin_hint` does today), and a caller that wants error 2 has to split on
//! `$C(13,10)` and re-parse the prefix itself. This module does that once, here, correctly.
//!
//! ## Which forms are elements, measured
//!
//! `ERROR #<id>:` is the marker, and **the id is not always a number**. Measured on IRIS 2026.1:
//!
//! ```text
//! $System.Status.Error(5001,"numeric one")        -> ERROR #5001: numeric one
//! $System.Status.Error("MyErrName","named one")   -> ERROR #MyErrName: Unknown status code: <UserErrors>MyErrName (named one)
//! AppendStatus of the two                         -> both, CRLF-joined, DecomposeStatus counts 2
//! ```
//!
//! So one chain carries both shapes under one marker. An earlier version of this module required
//! digits after `ERROR #`, which made the named element parse to nothing, land in `undecoded`, and
//! turn that intact two-element chain into `Partial` — a truncation report about a chain that arrived
//! whole. [`StatusId`] carries either.
//!
//! What is still NOT a marker: `ERROR <EnsSearchTable>PropCollision: …`, the angle-bracket form that
//! appears in COMPILE CONSOLE output. `GetErrorText` does not render a status that way — the probe
//! above shows the domain landing inside the element's TEXT (`<UserErrors>MyErrName`) while the id
//! slot holds the bare name — so promoting a console line to a status element would be a claim
//! nothing here has measured.
//!
//! ## The three cases
//!
//! [`StatusChain`] is an enum and not an `Option<Vec<_>>` on purpose (CLAUDE.md: a failure must
//! never be answered with a negative fact). Recognising no marker is **not** a claim that the call
//! succeeded — [`StatusChain::NotRecognised`] says only that no status text was found, and the
//! payload for it is absent rather than `{"ok": true}`. An OK status is a separate variant that
//! only a caller which KNOWS the value's declared type may construct.

/// The marker `GetErrorText` puts in front of every element of a chain.
pub const MARKER: &str = "ERROR #";

/// The id an element carries. Two shapes, because IRIS produces two and only one of them is a key
/// a remedy can be looked up by.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum StatusId {
    /// `ERROR #5002:` — the form a hint arm can key on.
    Number(u32),
    /// `ERROR #MyErrName:` — a named id from a non-`%ObjectErrors` domain. Carried as it arrived;
    /// NOT an undecodable element.
    Name(String),
}

impl StatusId {
    /// The number, when there is one. `None` for a named id — which is the honest answer, and the
    /// reason a caller keying remedies on numbers cannot be silently handed a name.
    pub fn number(&self) -> Option<u32> {
        match self {
            StatusId::Number(n) => Some(*n),
            StatusId::Name(_) => None,
        }
    }
}

/// One decoded element of a `%Status` chain.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StatusError {
    /// The id between `ERROR #` and `:`. Usually a number; sometimes a NAME.
    ///
    /// MEASURED on IRIS 2026.1: `$System.Status.Error("MyErrName", "named one")` renders as
    /// `ERROR #MyErrName: Unknown status code: <UserErrors>MyErrName (named one)` — the same
    /// `ERROR #` marker with a non-numeric id, and `DecomposeStatus` counts it as a normal element
    /// of a chain alongside a numeric one. A first version of this module required digits here, so
    /// a named element parsed to nothing, landed in `undecoded`, and turned an intact two-element
    /// chain into `Partial` — a truncation report about a chain that arrived whole.
    pub code: StatusId,
    /// Everything after `ERROR #<id>: `, trailing whitespace trimmed. Newlines inside an element
    /// are kept: an ObjectScript error's text can run to several lines and cutting it at the first
    /// would be the very loss this module exists to prevent.
    pub text: String,
}

/// What reading a `%Status` out of text produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusChain {
    /// No `%Status` error text was recognised. **Not** a claim that anything succeeded — the text
    /// may be an error this module cannot parse, or output that never carried a status at all.
    NotRecognised,
    /// A `%Status` that is OK. Constructible only by a caller that knows the value's declared type
    /// was `%Status` and that its decoded text was empty; [`decode_chain`] never returns this,
    /// because empty text with an unknown type says nothing.
    Ok,
    /// Every element decoded.
    Decoded {
        errors: Vec<StatusError>,
        /// What the script printed before the first element, e.g. the `"Stop: "` of
        /// `Write "Stop: ", $SYSTEM.Status.GetErrorText(sc)`. Kept so the block cannot be read as
        /// "the whole output was this status".
        preamble: String,
    },
    /// At least one element carried the marker and could not be decoded — a chain cut mid-element
    /// by a truncating transport ends exactly like this. What decoded is still reported; what did
    /// not is reported verbatim, under its own name, so a prefix is never presented as the whole.
    Partial {
        errors: Vec<StatusError>,
        undecoded: Vec<String>,
        preamble: String,
        /// Whether the CALLER already knows this is a failing status.
        ///
        /// `true` only from [`decode_known_error`], where IRIS's own `$$$ISOK` supplied the verdict —
        /// there `ok: false` is measured even when no element parses. `decode_chain` sets it `false`,
        /// because it sees only what a script chose to print: measured by running `iris_execute` on
        /// `write "see ERROR #5002 in the docs for details"`, which has the marker, decodes nothing,
        /// and must not come back asserting a failing status about prose.
        known_error: bool,
    },
}

impl StatusChain {
    /// The `status` block for a tool payload, or `None` when there is nothing to claim.
    ///
    /// Additive by construction: it never carries a success verdict of its own for the *tool*, only
    /// for the status it decoded.
    pub fn payload(&self) -> Option<serde_json::Value> {
        match self {
            StatusChain::NotRecognised => None,
            StatusChain::Ok => Some(serde_json::json!({ "ok": true, "errors": [] })),
            StatusChain::Decoded { errors, preamble } => {
                let mut v = serde_json::json!({ "ok": false, "errors": errors });
                Self::add_preamble(&mut v, preamble);
                Some(v)
            }
            StatusChain::Partial {
                errors,
                undecoded,
                preamble,
                known_error,
            } => {
                // `ok` is a CLAIM, and with nothing decoded there is nothing to base it on. Measured
                // by running iris_execute on `write "see ERROR #5002 in the docs for details"`: the
                // marker is there, no element parses, and the block came back `ok: false` — asserting
                // a failing status about prose, on a call that succeeded. A truncated chain
                // (`ERROR #63`) is indistinguishable from prose at the text level, so the block still
                // appears and still says it could not read what it found; it just stops claiming a
                // verdict it cannot support.
                let mut v = if errors.is_empty() && !known_error {
                    serde_json::json!({
                        "errors": errors,
                        "complete": false,
                        "undecoded": undecoded,
                        "note": "something here begins with the ERROR # marker and no element of it \
                                 could be decoded, so this is NOT reported as a failing status. It is \
                                 either a %Status chain cut mid-element or text that merely mentions \
                                 an error code.",
                    })
                } else {
                    serde_json::json!({
                    "ok": false,
                    "errors": errors,
                    // Named so a reader that only looks at `errors` still cannot mistake a prefix
                    // for the chain: `complete` is false and the rest is right there.
                    "complete": false,
                    "undecoded": undecoded,
                    })
                };
                Self::add_preamble(&mut v, preamble);
                Some(v)
            }
        }
    }

    fn add_preamble(v: &mut serde_json::Value, preamble: &str) {
        if !preamble.trim().is_empty() {
            v["preamble"] = serde_json::Value::String(preamble.to_string());
        }
    }

    /// Whether every element that carried the marker was decoded. `true` for a chain with no
    /// elements at all, which is why it is never the test for "is there a status here".
    pub fn is_complete(&self) -> bool {
        !matches!(self, StatusChain::Partial { .. })
    }
}

/// Split `text` into the elements of a `%Status` chain.
///
/// Finds the marker anywhere, not only at the start of a line: the shape this exists for is
/// `Write "Stop: ", $Select($$$ISERR(sc):$SYSTEM.Status.GetErrorText(sc), 1:"OK")`, where the first
/// element is preceded by the script's own label.
pub fn decode_chain(text: &str) -> StatusChain {
    let starts: Vec<usize> = text.match_indices(MARKER).map(|(i, _)| i).collect();
    if starts.is_empty() {
        return StatusChain::NotRecognised;
    }
    let preamble = text[..starts[0]].to_string();
    let mut errors = Vec::new();
    let mut undecoded = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(text.len());
        let element = text[start..end].trim_end_matches(['\r', '\n', ' ', '\t']);
        match decode_element(element) {
            Some(e) => errors.push(e),
            None => undecoded.push(element.to_string()),
        }
    }
    if undecoded.is_empty() {
        StatusChain::Decoded { errors, preamble }
    } else {
        StatusChain::Partial {
            errors,
            undecoded,
            preamble,
            known_error: false,
        }
    }
}

/// Decode a `%Status` whose verdict is ALREADY KNOWN to be an error, so that failing to recognise
/// the text cannot come back as [`StatusChain::NotRecognised`] — i.e. as "no status here".
///
/// For a caller that read IRIS's own `$$$ISOK` (which `iris_execute_method` does) the error is a
/// measured fact independent of whether this module can parse the text. Answering an unparseable
/// text with the absent case would be the #310 shape exactly: a failure reported as a negative fact.
pub fn decode_known_error(text: &str) -> StatusChain {
    match decode_chain(text) {
        StatusChain::NotRecognised => StatusChain::Partial {
            errors: Vec::new(),
            undecoded: if text.trim().is_empty() {
                Vec::new()
            } else {
                vec![text.trim().to_string()]
            },
            preamble: String::new(),
            known_error: true,
        },
        other => other,
    }
}

/// `ERROR #5002: ObjectScript error: boom` -> code 5002, text `ObjectScript error: boom`.
///
/// Everything else is `None`, including a number with no colon after it and a colon with no text
/// after it — both are what a chain cut mid-element looks like, and neither describes an error.
fn decode_element(element: &str) -> Option<StatusError> {
    let rest = element.strip_prefix(MARKER)?;
    // The id runs to the first colon. It is NOT required to be numeric — see `StatusId`.
    let (id, after) = rest.split_once(':')?;
    let id = id.trim();
    if id.is_empty() || id.contains(char::is_whitespace) {
        // No id at all, or something that is not an id — `ERROR #: x`, or a colon so far away that
        // what precedes it is prose. Both are what a chain cut mid-element looks like.
        return None;
    }
    let text = after.trim();
    if text.is_empty() {
        return None;
    }
    let code = match id.parse::<u32>() {
        Ok(n) => StatusId::Number(n),
        Err(_) => StatusId::Name(id.to_string()),
    };
    Some(StatusError {
        code,
        text: text.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measured on IRIS 2026.1: a two-element chain built with `$system.Status.AppendStatus`,
    /// decoded by the real `$SYSTEM.Status.GetErrorText`. CRLF-joined, both elements present.
    const MEASURED_TWO: &str = "ERROR #5002: ObjectScript error: first problem\r\n\
                                ERROR #6301: SAX XML Parser Error: second problem";

    fn decoded(t: &str) -> (Vec<StatusError>, String) {
        match decode_chain(t) {
            StatusChain::Decoded { errors, preamble } => (errors, preamble),
            other => panic!("expected Decoded, got {other:?}"),
        }
    }

    #[test]
    fn both_elements_of_the_measured_chain_decode_with_their_own_codes() {
        let (errors, preamble) = decoded(MEASURED_TWO);
        assert_eq!(
            errors,
            vec![
                StatusError {
                    code: StatusId::Number(5002),
                    text: "ObjectScript error: first problem".into()
                },
                StatusError {
                    code: StatusId::Number(6301),
                    text: "SAX XML Parser Error: second problem".into()
                },
            ],
            "the per-error CODE is the whole point: a remedy keyed on 6301 cannot be found by \
             matching a substring of the concatenation"
        );
        assert_eq!(
            preamble, "",
            "the measured text is the status and nothing else"
        );
    }

    /// The 785-call shape from the report: the script labels its own output, so the first element
    /// is not at the start. The label must survive — it is the caller's own text.
    #[test]
    fn a_label_before_the_first_element_is_kept_not_dropped() {
        let (errors, preamble) = decoded("Stop: ERROR #5002: ObjectScript error: boom");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, StatusId::Number(5002));
        assert_eq!(preamble, "Stop: ");
        let p = decode_chain("Stop: ERROR #5002: ObjectScript error: boom")
            .payload()
            .expect("a payload");
        assert_eq!(p["preamble"], "Stop: ");
    }

    /// A one-element chain is the common case and must not need the multi-element path.
    #[test]
    fn a_single_element_chain_decodes_to_one_error() {
        let (errors, _) = decoded("ERROR #6062: The Monitor is already running");
        assert_eq!(
            errors,
            vec![StatusError {
                code: StatusId::Number(6062),
                text: "The Monitor is already running".into()
            }]
        );
    }

    /// LF-only, because a transport that translates CRLF (the marker-line protocol in
    /// `objectscript::write_marker_lines` deletes CR by design) must decode identically.
    #[test]
    fn lf_joined_decodes_the_same_as_crlf_joined() {
        let crlf = decode_chain(MEASURED_TWO);
        let lf = decode_chain(&MEASURED_TWO.replace("\r\n", "\n"));
        assert_eq!(crlf, lf);
    }

    /// The case this module exists to make impossible: output with no status text must not be
    /// answered with `ok: true`. Absence of a marker is not evidence of success.
    #[test]
    fn output_with_no_status_is_not_reported_as_a_successful_status() {
        for t in ["", "OK", "42", "Stop: OK", "   \n  "] {
            assert_eq!(decode_chain(t), StatusChain::NotRecognised, "{t:?}");
            assert!(
                decode_chain(t).payload().is_none(),
                "{t:?} must carry NO status block at all — an `ok` of either value would be a \
                 claim about a status that was never seen"
            );
        }
    }

    /// An OK status is only ever asserted by a caller that knows the declared type was `%Status`.
    #[test]
    fn an_ok_status_is_its_own_variant_and_says_so() {
        let p = StatusChain::Ok.payload().expect("a payload");
        assert_eq!(p["ok"], true);
        assert_eq!(p["errors"].as_array().map(|a| a.len()), Some(0));
    }

    /// A chain cut mid-element: what decoded is kept, what did not is reported verbatim, and the
    /// block says out loud that it is not the whole chain.
    #[test]
    fn a_chain_cut_mid_element_is_partial_not_a_shorter_chain() {
        let cut = "ERROR #5002: ObjectScript error: first problem\r\nERROR #63";
        let chain = decode_chain(cut);
        assert_eq!(
            chain,
            StatusChain::Partial {
                errors: vec![StatusError {
                    code: StatusId::Number(5002),
                    text: "ObjectScript error: first problem".into()
                }],
                undecoded: vec!["ERROR #63".into()],
                preamble: String::new(),
                known_error: false,
            },
            "{chain:?}"
        );
        assert!(!chain.is_complete());
        let p = chain.payload().expect("a payload");
        assert_eq!(p["complete"], false);
        assert_eq!(p["undecoded"][0], "ERROR #63");
        assert_eq!(
            p["errors"].as_array().map(|a| a.len()),
            Some(1),
            "the element that DID decode is still reported"
        );
    }

    /// The other two truncation points, both of which describe no error: a number with no colon,
    /// and a colon with nothing after it.
    #[test]
    fn a_number_without_text_is_undecoded_not_an_error_with_no_explanation() {
        for t in [
            "ERROR #5002",
            "ERROR #5002:",
            "ERROR #5002:   ",
            "ERROR #: x",
        ] {
            match decode_chain(t) {
                StatusChain::Partial {
                    errors, undecoded, ..
                } => {
                    assert!(errors.is_empty(), "{t:?} decoded something: {errors:?}");
                    assert_eq!(undecoded.len(), 1, "{t:?}");
                }
                other => panic!("{t:?} must be Partial, got {other:?}"),
            }
        }
    }

    /// `Decoded` with no errors would be a status block claiming a chain with nothing in it. The
    /// marker-present/marker-absent split makes it unreachable; this pins that.
    #[test]
    fn a_decoded_chain_is_never_empty() {
        for t in [
            MEASURED_TWO,
            "Stop: ERROR #1: x",
            "ERROR #6062: The Monitor is already running",
        ] {
            if let StatusChain::Decoded { errors, .. } = decode_chain(t) {
                assert!(!errors.is_empty(), "{t:?}");
            }
        }
    }

    /// A multi-line element keeps its own newlines — cutting an ObjectScript error's text at the
    /// first line is the loss this module exists to prevent, one level down.
    #[test]
    fn newlines_inside_one_element_are_kept() {
        let (errors, _) = decoded("ERROR #5002: ObjectScript error: <UNDEFINED>\n  at zFoo+1^Bar");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].text.contains("zFoo+1^Bar"),
            "the continuation line is part of the error: {:?}",
            errors[0].text
        );
    }

    /// The `ERROR <Tag>Name:` form carries no number and is not known to be a `%Status` element,
    /// so it must not be promoted to one — and must not be dropped either.
    #[test]
    fn the_unnumbered_tag_form_is_not_claimed_as_a_status_element() {
        let tag = "ERROR <EnsSearchTable>PropCollision: SearchTable property collision: \
                   Property 'PatientFirstName' in class 'HOSPITAL.Search.HL7'";
        assert_eq!(decode_chain(tag), StatusChain::NotRecognised);
        // But when it follows a real element it stays inside that element's text.
        let (errors, _) = decoded(&format!("ERROR #5001: outer\r\n{tag}"));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].text.contains("PropCollision"), "{:?}", errors[0]);
    }

    /// A status IRIS itself called an error, whose text this module cannot parse, must not come
    /// back as "no status here" — the verdict is known even when the text is not.
    #[test]
    fn a_known_error_with_unparseable_text_is_never_the_absent_case() {
        let chain = decode_known_error("something went wrong, no marker in sight");
        assert_ne!(chain, StatusChain::NotRecognised);
        let p = chain.payload().expect("a known error must carry a block");
        assert_eq!(p["ok"], false);
        assert_eq!(p["complete"], false);
        assert_eq!(
            p["undecoded"][0],
            "something went wrong, no marker in sight"
        );
    }

    /// And when it CAN be parsed, the known-error path must not invent a second reading of it.
    #[test]
    fn a_known_error_that_parses_decodes_exactly_as_the_plain_path_does() {
        assert_eq!(decode_known_error(MEASURED_TWO), decode_chain(MEASURED_TWO));
    }

    /// An error whose text carried nothing at all is still an error. `ok: false` with an empty
    /// `errors` is the honest reading; a missing block would say no status was involved.
    #[test]
    fn a_known_error_with_no_text_still_says_it_is_an_error() {
        let p = decode_known_error("").payload().expect("a block");
        assert_eq!(p["ok"], false);
        assert_eq!(p["errors"].as_array().map(|a| a.len()), Some(0));
        assert_eq!(p["complete"], false);
    }

    /// MEASURED on IRIS 2026.1, and the defect this fixes: a named id is a normal element of a
    /// chain, not a truncation. The earlier version reported this chain as `Partial`.
    #[test]
    fn a_named_error_id_is_an_element_not_an_undecoded_fragment() {
        let measured = "ERROR #5001: numeric one\r\n\
                        ERROR #MyErrName: Unknown status code: <UserErrors>MyErrName (named one)";
        let chain = decode_chain(measured);
        let StatusChain::Decoded { errors, .. } = &chain else {
            panic!("an intact two-element chain must not be Partial: {chain:?}")
        };
        assert!(chain.is_complete());
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].code, StatusId::Number(5001));
        assert_eq!(errors[1].code, StatusId::Name("MyErrName".into()));
        assert!(
            errors[1].text.contains("<UserErrors>MyErrName"),
            "{:?}",
            errors[1].text
        );
        // A remedy keyed on numbers gets a number for one and honest nothing for the other.
        assert_eq!(errors[0].code.number(), Some(5001));
        assert_eq!(errors[1].code.number(), None);
    }

    /// The payload mirrors IRIS: one `code` slot carrying whichever shape arrived. Pinned in both
    /// directions so a caller can rely on the type telling it which.
    #[test]
    fn the_payload_code_is_a_number_for_a_numeric_id_and_a_string_for_a_named_one() {
        let p = decode_chain("ERROR #5001: a\nERROR #MyErrName: b")
            .payload()
            .expect("a payload");
        assert_eq!(p["errors"][0]["code"], 5001);
        assert!(p["errors"][0]["code"].is_number());
        assert_eq!(p["errors"][1]["code"], "MyErrName");
        assert!(p["errors"][1]["code"].is_string());
    }

    /// And the truncation cases still ARE truncation — the widening must not swallow them.
    #[test]
    fn widening_the_id_did_not_turn_truncation_into_an_element() {
        for cut in [
            "ERROR #63",
            "ERROR #:",
            "ERROR #: x",
            "ERROR #5002:",
            "ERROR #5002",
        ] {
            match decode_chain(cut) {
                StatusChain::Partial { errors, .. } => {
                    assert!(errors.is_empty(), "{cut:?} decoded something: {errors:?}")
                }
                other => panic!("{cut:?} must stay Partial, got {other:?}"),
            }
        }
        // And prose that merely contains the marker plus a distant colon is not an element either.
        match decode_chain("ERROR #the compile failed: see the console") {
            StatusChain::Partial { errors, .. } => assert!(errors.is_empty()),
            other => panic!("an id with spaces in it is not an id, got {other:?}"),
        }
    }

    /// MEASURED by running `iris_execute` on `write "see ERROR #5002 in the docs for details"`: the
    /// marker is there, nothing decodes, and the block used to come back `ok: false` — asserting a
    /// failing status about prose, on a call that succeeded. `ok` is a claim; with nothing decoded and
    /// no verdict from IRIS there is nothing to base it on.
    #[test]
    fn prose_that_merely_mentions_an_error_code_is_not_reported_as_a_failing_status() {
        let v = decode_chain("see ERROR #5002 in the docs for details")
            .payload()
            .expect("the marker is there, so something is reported");
        assert!(
            v.get("ok").is_none(),
            "no verdict may be claimed when nothing decoded: {v}"
        );
        assert_eq!(v["complete"], false);
        assert_eq!(v["errors"].as_array().map(|a| a.len()), Some(0));
        assert!(
            v["note"]
                .as_str()
                .unwrap_or_default()
                .contains("NOT reported as a failing status"),
            "the payload must say why there is no verdict: {v}"
        );
        // CONTROL: a chain that DOES decode still carries the verdict.
        let real = decode_chain("ERROR #5002: boom")
            .payload()
            .expect("a payload");
        assert_eq!(real["ok"], false);
    }

    /// And the measured-verdict path is unaffected: `iris_execute_method` reads IRIS's own `$$$ISOK`,
    /// so there `ok: false` holds even when the text cannot be parsed.
    #[test]
    fn a_known_error_still_claims_the_verdict_it_measured() {
        let v = decode_known_error("no marker anywhere in here")
            .payload()
            .expect("a payload");
        assert_eq!(
            v["ok"], false,
            "the caller read $$$ISOK, so the verdict is measured rather than guessed: {v}"
        );
        assert_eq!(v["complete"], false);
        // The two paths must DIFFER on this, or the distinction is not being made.
        let guessed = decode_chain("see ERROR #5002 in the docs")
            .payload()
            .expect("a payload");
        assert!(guessed.get("ok").is_none());
    }
}
