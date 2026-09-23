//! #352: read an IRIS stream by id, without consuming it and without loading it whole.
//!
//! ## What was measured before adopting this
//!
//! #352 makes adoption conditional on one thing: "confirmed non-consuming before adoption". Measured
//! on IRIS 2026.1 with a throwaway `%Stream.GlobalCharacter` (probe class deleted afterwards):
//!
//! | step | result |
//! |---|---|
//! | instance A: `Read(100)`, then `Read(100)` again | `"line-1"`, then `""` with `AtEnd=1` |
//! | instance B: a FRESH `%OpenId` on the same id, after A hit AtEnd | `Size=6`, `Read` → `"line-1"` |
//! | instance C: read `.Size` FIRST, then `Read(100)` | `6`, then `"line-1"` — reading size moves nothing |
//! | instance A again | still `""`, still `AtEnd=1` — B and C did not reset it |
//!
//! So read position is **per instance and never saved**. An inspector that opens the stream itself
//! cannot consume anything a later reader needs, and reading `Size` does not advance anything.
//!
//! That also corrects the issue's framing. #352 asks for an answer to "is the stream empty, or
//! already read?" — but since position is never persisted, *"already read" is not a state a later
//! reader can observe*. A stream a relay consumed to the end still reads from the start for the next
//! `%OpenId`. The question this answers is simply "what is in this stream".
//!
//! ## Why this is not upstream's implementation
//!
//! Upstream's `stream_inspect_impl` was read at `upstream/master` before writing this. Three things
//! are deliberately different:
//!
//! 1. **It calls `Do stream.Rewind()`.** Harmless, as the measurement above shows — but it is a
//!    mutation on an object the caller handed us to LOOK at, and its harmlessness is a property of
//!    IRIS rather than of the tool. Nothing here writes to the stream.
//! 2. **It reads the entire stream into one variable** (`While stream.AtEnd=0 { Set content=content_
//!    stream.Read(4096) }`) with no cap. Measured on this instance family, a string of 3,600,000
//!    characters survives a round trip and 3,700,000 fails `<MAXSTRING>` → SQLCODE -400 → -149. An
//!    interop message body can exceed that, so the tool would fail on exactly the payload a caller
//!    most wants to look at. This reads `max_chars` and reports the full `size` separately.
//! 3. **It writes the body on ONE line** (`Write "CONTENT|"_content`). An HL7 v2 message is
//!    CR-separated and any XML or JSON body contains newlines, so every line after the first matches
//!    no prefix and is dropped silently — the #347 defect, which this repo has already fixed twice
//!    and has a helper for. The body goes through [`crate::objectscript::write_marker_lines`] with a
//!    declared line count, so a short arrival is detectable instead of plausible.

use crate::objectscript::{os_str_expr, write_marker_lines, Declared};

/// #211: `stream_id` is the advertised name and the only one the schema publishes. `oid` is
/// upstream's spelling and `id` the obvious guess, both accepted as a rescue path so a caller
/// reasoning from either does not get rmcp's bare -32602, which names the field that is missing and
/// never the field that was sent.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StreamInspectParams {
    #[serde(alias = "oid", alias = "id", alias = "stream")]
    pub stream_id: String,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — a stream id is only meaningful in the namespace that holds it.
    #[serde(default)]
    pub namespace: Option<String>,
    /// How many characters of the body to return (default 4096). The full `size` is always
    /// reported, and `truncated` says when what came back is a prefix.
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
}

fn default_max_chars() -> usize {
    4096
}

/// The stream classes tried, in order. Character first: an interop message body is text far more
/// often than not, and a binary stream opened as character would report a misleading size.
pub const CHARACTER_CLASS: &str = "%Stream.GlobalCharacter";
pub const BINARY_CLASS: &str = "%Stream.GlobalBinary";

pub const M_FOUND: &str = "SI_FOUND:";
pub const M_CLASS: &str = "SI_CLASS:";
pub const M_SIZE: &str = "SI_SIZE:";
pub const M_WHY_LINES: &str = "SI_WHY_LINES:";
pub const M_WHY: &str = "SI_WHY:";
pub const M_BODY_LINES: &str = "SI_BODY_LINES:";
pub const M_BODY: &str = "SI_BODY:";

/// What inspecting one stream amounted to.
///
/// `NotFound` and `Unreadable` are separate from a stream that is genuinely empty, and neither is
/// reported as one: "this id opens nothing" and "this stream holds nothing" send a caller to
/// different places, and answering the first with the second is the negative fact CLAUDE.md is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamRead {
    /// Neither stream class opened this id. Carries what IRIS said, when it said anything.
    NotFound { why: String },
    /// The program produced no usable answer — no `SI_FOUND` marker at all.
    Unreadable,
    /// Opened and read.
    Read {
        class: String,
        size: u64,
        /// The characters that arrived. Empty for a binary stream, which is reported by size only.
        body: String,
        /// Declared line count of the body, and how many arrived. Unequal means the output was cut.
        lines_declared: usize,
        lines_received: usize,
    },
}

impl StreamRead {
    /// Whether the body arrived whole. A binary stream declares no body, so it is complete by
    /// construction — `size` is the answer there, not the bytes.
    pub fn body_complete(&self) -> bool {
        match self {
            StreamRead::Read {
                lines_declared,
                lines_received,
                ..
            } => lines_declared == lines_received,
            _ => false,
        }
    }
}

/// The program. Reads `Size` BEFORE the body, because the measurement above shows reading size moves
/// nothing and the size is the half that is always reportable.
///
/// The not-found branch ends in `Quit`, which returns from the METHOD — measured on IRIS 2026.1, and
/// the behaviour wanted here: there is nothing further to report.
pub fn build_inspect_code(id: &str, max_chars: usize) -> String {
    let id_e = os_str_expr(id);
    format!(
        "set tId={id_e}\n\
         set tS=##class({ch}).%OpenId(tId,,.tSC1)\n\
         set tCls={ch_e}\n\
         if '$IsObject(tS) {{ set tS=##class({bin}).%OpenId(tId,,.tSC2) set tCls={bin_e} }}\n\
         if '$IsObject(tS) {{ write {found}_\"0\"_$CHAR(10) set tWhy=$SYSTEM.Status.GetErrorText(tSC1) {why} Quit }}\n\
         write {found}_\"1\"_$CHAR(10)\n\
         write {cls}_tCls_$CHAR(10)\n\
         write {size}_tS.Size_$CHAR(10)\n\
         set tBody=\"\"\n\
         if tCls={ch_e} {{ set tBody=tS.Read({max}) }}\n\
         {body}",
        ch = CHARACTER_CLASS,
        bin = BINARY_CLASS,
        ch_e = os_str_expr(CHARACTER_CLASS),
        bin_e = os_str_expr(BINARY_CLASS),
        found = os_str_expr(M_FOUND),
        cls = os_str_expr(M_CLASS),
        size = os_str_expr(M_SIZE),
        max = max_chars,
        why = write_marker_lines("tWhy", M_WHY_LINES, M_WHY, Declared::Lines).replace('\n', " "),
        body = write_marker_lines("tBody", M_BODY_LINES, M_BODY, Declared::Lines),
    )
}

/// Read the framed output back.
pub fn parse_inspect_output(out: &str) -> StreamRead {
    let mut found: Option<bool> = None;
    let mut class = String::new();
    let mut size: u64 = 0;
    let mut why: Vec<String> = Vec::new();
    let mut body: Vec<String> = Vec::new();
    let mut declared: Option<usize> = None;
    for line in out.lines() {
        let l = line.trim_end_matches('\r');
        if let Some(v) = l.strip_prefix(M_FOUND) {
            found = Some(v.trim() == "1");
        } else if let Some(v) = l.strip_prefix(M_CLASS) {
            class = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix(M_SIZE) {
            size = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix(M_BODY_LINES) {
            declared = v.trim().parse().ok();
        } else if let Some(v) = l.strip_prefix(M_BODY) {
            body.push(v.to_string());
        } else if let Some(v) = l.strip_prefix(M_WHY) {
            why.push(v.to_string());
        }
    }
    match found {
        None => StreamRead::Unreadable,
        Some(false) => StreamRead::NotFound {
            why: why.join("\n"),
        },
        Some(true) => {
            // A body of "" is written by write_marker_lines as ONE empty line, so an empty stream
            // declares 1 and receives 1. That is why emptiness is read off `size`, never off the
            // line count.
            let lines_declared = declared.unwrap_or(0);
            StreamRead::Read {
                class,
                size,
                body: body.join("\n"),
                lines_declared,
                lines_received: body.len(),
            }
        }
    }
}

/// The payload. `truncated` is set from `size` versus what was asked for, so a prefix is never
/// presented as the whole stream.
pub fn payload(id: &str, max_chars: usize, r: &StreamRead) -> serde_json::Value {
    match r {
        StreamRead::Unreadable => serde_json::json!({
            "stream_id": id, "error_code": "STREAM_UNREADABLE",
            "error": "the inspect program produced no answer for this stream — not an empty \
                      stream, an unusable reply. Retry; if it persists the namespace may be wrong."
        }),
        StreamRead::NotFound { why } => {
            let mut v = serde_json::json!({
                "stream_id": id, "error_code": "STREAM_NOT_FOUND",
                "error": format!(
                    "no {CHARACTER_CLASS} or {BINARY_CLASS} with id '{id}' in this namespace. \
                     This is NOT an empty stream. Check the id (it is the %Id of the stream, which \
                     for a message body comes from the body's own property) and check the \
                     namespace — a stream id is only meaningful in the namespace that holds it."
                ),
            });
            if !why.trim().is_empty() {
                v["iris_status"] = serde_json::Value::String(why.clone());
            }
            v
        }
        StreamRead::Read {
            class,
            size,
            body,
            lines_declared,
            lines_received,
        } => {
            let binary = class == BINARY_CLASS;
            let mut v = serde_json::json!({
                "success": true,
                "stream_id": id,
                "class": class,
                "size": size,
                "empty": *size == 0,
            });
            if binary {
                // Bytes do not survive a line-oriented protocol, and a mangled body is worse than
                // none. The size still answers "did the relay write anything".
                v["body_omitted"] = serde_json::Value::String(
                    "binary stream: size is reported, content is not, because arbitrary bytes \
                     cannot be carried on a text protocol without corrupting them."
                        .into(),
                );
            } else {
                v["body"] = serde_json::Value::String(body.clone());
                v["body_chars"] = serde_json::json!(body.chars().count());
                let asked = max_chars as u64;
                if *size > asked {
                    v["truncated"] = serde_json::Value::Bool(true);
                    v["max_chars"] = serde_json::json!(max_chars);
                }
                if lines_declared != lines_received {
                    v["body_incomplete"] = serde_json::Value::Bool(true);
                    v["lines_declared"] = serde_json::json!(lines_declared);
                    v["lines_received"] = serde_json::json!(lines_received);
                }
            }
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_program_never_writes_to_the_stream() {
        let code = build_inspect_code("4", 4096);
        for mutation in ["Rewind", "MoveTo", "Write", "%Save", "Clear", "%Delete"] {
            assert!(
                !code.contains(mutation),
                "the program calls {mutation}, which mutates an object the caller asked us to \
                 LOOK at:\n{code}"
            );
        }
        // CONTROL: it does read, so the assertions above are not passing on an empty program.
        assert!(code.contains(".Read("), "{code}");
        assert!(code.contains(".Size"), "{code}");
    }

    #[test]
    fn size_is_read_before_the_body() {
        let code = build_inspect_code("4", 100);
        let size_at = code.find(".Size").expect("reads Size");
        let read_at = code.find(".Read(").expect("reads the body");
        assert!(
            size_at < read_at,
            "Size must be read first — it is the half that is always reportable:\n{code}"
        );
    }

    #[test]
    fn the_body_read_is_capped_at_what_was_asked_for() {
        let code = build_inspect_code("4", 512);
        assert!(code.contains(".Read(512)"), "{code}");
        // The whole-stream loop upstream uses must not appear: it is what hits <MAXSTRING>.
        assert!(!code.contains("AtEnd"), "no read-to-the-end loop:\n{code}");
        assert!(!code.contains("While"), "{code}");
    }

    #[test]
    fn a_body_with_newlines_is_carried_line_by_line_with_a_declared_count() {
        let code = build_inspect_code("4", 4096);
        assert!(
            code.contains(M_BODY_LINES),
            "the count must be declared:\n{code}"
        );
        assert!(code.contains(M_BODY), "{code}");
        // The single-line form is the #347 defect.
        assert!(
            !code.contains("write \"SI_BODY:\"_tBody"),
            "the body is written as one line, so an HL7 message loses every segment after the \
             first:\n{code}"
        );
    }

    /// An HL7 v2 body: CR-separated segments. `write_marker_lines` deletes CR and splits on LF, so
    /// the reader sees one line per segment.
    #[test]
    fn every_line_of_a_multi_line_body_is_reassembled() {
        let out = "SI_FOUND:1\nSI_CLASS:%Stream.GlobalCharacter\nSI_SIZE:42\n\
                   SI_BODY_LINES:3\nSI_BODY:MSH|^~\\&|HIS\nSI_BODY:PID|1||123\nSI_BODY:PV1|1|I\n";
        let r = parse_inspect_output(out);
        let StreamRead::Read { body, size, .. } = &r else {
            panic!("expected Read, got {r:?}")
        };
        assert_eq!(*size, 42);
        assert_eq!(body, "MSH|^~\\&|HIS\nPID|1||123\nPV1|1|I");
        assert!(r.body_complete());
    }

    /// Fewer lines than declared: the body is a PREFIX, and saying so is the point.
    #[test]
    fn a_body_that_arrives_short_is_flagged_not_silently_shortened() {
        let out = "SI_FOUND:1\nSI_CLASS:%Stream.GlobalCharacter\nSI_SIZE:42\n\
                   SI_BODY_LINES:3\nSI_BODY:MSH|^~\\&|HIS\n";
        let r = parse_inspect_output(out);
        assert!(!r.body_complete());
        let v = payload("4", 4096, &r);
        assert_eq!(v["body_incomplete"], true);
        assert_eq!(v["lines_declared"], 3);
        assert_eq!(v["lines_received"], 1);
    }

    /// An id that opens nothing is NOT an empty stream, and the message has to say so out loud —
    /// this is the confusion #352 was filed about.
    #[test]
    fn an_id_that_opens_nothing_is_not_an_empty_stream() {
        let r = parse_inspect_output(
            "SI_FOUND:0\nSI_WHY_LINES:1\nSI_WHY:ERROR #5809: Object not found\n",
        );
        assert_eq!(
            r,
            StreamRead::NotFound {
                why: "ERROR #5809: Object not found".into()
            }
        );
        let v = payload("999", 4096, &r);
        assert_eq!(v["error_code"], "STREAM_NOT_FOUND");
        assert!(v.get("empty").is_none(), "must not report emptiness: {v}");
        assert!(
            v.get("size").is_none(),
            "must not report a size it never read: {v}"
        );
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("NOT an empty stream"),
            "{v}"
        );
        assert!(
            v["iris_status"]
                .as_str()
                .unwrap_or_default()
                .contains("5809"),
            "{v}"
        );
    }

    /// A genuinely empty stream is a DIFFERENT answer: it opened, and it holds nothing.
    #[test]
    fn a_stream_that_opened_and_holds_nothing_says_empty() {
        let r = parse_inspect_output(
            "SI_FOUND:1\nSI_CLASS:%Stream.GlobalCharacter\nSI_SIZE:0\nSI_BODY_LINES:1\nSI_BODY:\n",
        );
        let v = payload("4", 4096, &r);
        assert_eq!(v["success"], true);
        assert_eq!(v["empty"], true);
        assert_eq!(v["size"], 0);
        assert_eq!(v["body"], "");
        assert_ne!(v["error_code"], "STREAM_NOT_FOUND");
    }

    /// No marker at all: the program said nothing usable. Not found, and not empty either.
    #[test]
    fn a_reply_with_no_marker_is_unreadable_not_empty() {
        let r = parse_inspect_output("some unrelated output\n");
        assert_eq!(r, StreamRead::Unreadable);
        let v = payload("4", 4096, &r);
        assert_eq!(v["error_code"], "STREAM_UNREADABLE");
        assert!(v.get("empty").is_none(), "{v}");
    }

    /// A stream longer than what was asked for is a PREFIX and says so.
    #[test]
    fn a_stream_longer_than_max_chars_is_reported_as_truncated() {
        let r = parse_inspect_output(
            "SI_FOUND:1\nSI_CLASS:%Stream.GlobalCharacter\nSI_SIZE:100000\nSI_BODY_LINES:1\nSI_BODY:abc\n",
        );
        let v = payload("4", 10, &r);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["max_chars"], 10);
        assert_eq!(v["size"], 100000);
        // CONTROL: a stream that FITS must not be flagged.
        let fits = parse_inspect_output(
            "SI_FOUND:1\nSI_CLASS:%Stream.GlobalCharacter\nSI_SIZE:3\nSI_BODY_LINES:1\nSI_BODY:abc\n",
        );
        assert!(payload("4", 10, &fits).get("truncated").is_none());
    }

    /// Binary: size only. A mangled body is worse than none, and the size still answers "did the
    /// relay write anything".
    #[test]
    fn a_binary_stream_reports_its_size_and_omits_its_bytes() {
        let r = parse_inspect_output(
            "SI_FOUND:1\nSI_CLASS:%Stream.GlobalBinary\nSI_SIZE:2048\nSI_BODY_LINES:1\nSI_BODY:\n",
        );
        let v = payload("4", 4096, &r);
        assert_eq!(v["size"], 2048);
        assert_eq!(v["empty"], false);
        assert!(
            v.get("body").is_none(),
            "bytes must not be reported as text: {v}"
        );
        assert!(
            v["body_omitted"]
                .as_str()
                .unwrap_or_default()
                .contains("binary"),
            "{v}"
        );
        // And the program only reads a body for the character class.
        let code = build_inspect_code("4", 99);
        assert!(
            code.contains(&format!("if tCls={}", os_str_expr(CHARACTER_CLASS))),
            "{code}"
        );
    }
}
