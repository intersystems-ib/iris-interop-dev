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

use crate::objectscript::os_str_expr;

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
/// Declared length in characters of what was read, before encoding — a short arrival is then
/// detectable against the decoded length.
pub const M_BODY_LEN: &str = "SI_BODY_LEN:";
/// The body, base64. One line by construction: the alphabet contains no newline.
pub const M_BODY_B64: &str = "SI_BODY_B64:";

/// What inspecting one stream amounted to.
///
/// `NotFound` is separate from a stream that is genuinely empty, and neither is reported as the
/// other: "this id holds nothing" and "there is no such id" send a caller to different places, and
/// the measured reason this needs saying is that `%OpenId` cannot tell them apart — it hands back a
/// usable empty object for an unknown id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamRead {
    /// `%ExistsId` said no.
    NotFound,
    /// The program produced no usable answer — no `SI_FOUND` marker at all.
    Unreadable,
    /// Opened and read.
    Read {
        /// The class the read went through. NOT a claim about the data's kind: character and binary
        /// streams share storage, so which class opens an id says nothing about what is in it.
        opened_as: String,
        size: u64,
        /// The exact bytes that arrived, base64-decoded.
        bytes: Vec<u8>,
        /// Characters IRIS said it read, against `bytes.len()`. Unequal means the reply was cut.
        declared_len: usize,
    },
}

impl StreamRead {
    /// Whether as much arrived as IRIS declared.
    pub fn body_complete(&self) -> bool {
        match self {
            StreamRead::Read {
                bytes,
                declared_len,
                ..
            } => bytes.len() == *declared_len,
            _ => false,
        }
    }
}

/// The program.
///
/// Every line of this is a MEASURED correction of a first version that ran only in unit tests
/// (IRIS 2026.1, throwaway streams on a scratch instance):
///
/// * **`%ExistsId`, not `'$IsObject`.** `%OpenId("999")` on an unknown id returns `$IsObject` 1
///   with an OK status — a usable EMPTY stream object. So the not-found arm never fired and a
///   missing id was reported as `size: 0, empty: true`, which is precisely the confusion this tool
///   exists to prevent. `%ExistsId` gives 1 / 0 / 0 for present / missing / blank.
/// * **Always read through `%Stream.GlobalBinary`.** Character and binary streams share storage:
///   `%Stream.GlobalCharacter.%OpenId` on a binary stream SUCCEEDS and `$classname` reports the
///   class you opened with, so the data's kind is not knowable from here. Measured, the binary read
///   returns a character stream's bytes intact (106 chars, both CRs) and a binary stream's first
///   `$CHAR(0)` without truncating.
/// * **Base64, not a line protocol.** `write_marker_lines` deletes CR by design — correct for a
///   CRLF `%Status` chain, destructive for a CR-separated HL7 v2 body, which collapsed to one line
///   with its segment boundaries gone while the declared count still said "complete".
///   `$System.Encryption.Base64Encode(raw, 1)` round-trips losslessly and needs no line protocol.
pub fn build_inspect_code(id: &str, max_chars: usize) -> String {
    let id_e = os_str_expr(id);
    format!(
        "set tId={id_e}\n\
         if '##class({bin}).%ExistsId(tId) {{ write {found}_\"0\"_$CHAR(10) Quit }}\n\
         set tS=##class({bin}).%OpenId(tId,,.tSC1)\n\
         if '$IsObject(tS) {{ write {found}_\"0\"_$CHAR(10) Quit }}\n\
         write {found}_\"1\"_$CHAR(10)\n\
         write {cls}_{bin_e}_$CHAR(10)\n\
         write {size}_tS.Size_$CHAR(10)\n\
         set tBody=tS.Read({max})\n\
         write {blen}_$LENGTH(tBody)_$CHAR(10)\n\
         write {b64}_$SYSTEM.Encryption.Base64Encode(tBody,1)_$CHAR(10)",
        bin = BINARY_CLASS,
        bin_e = os_str_expr(BINARY_CLASS),
        found = os_str_expr(M_FOUND),
        cls = os_str_expr(M_CLASS),
        size = os_str_expr(M_SIZE),
        blen = os_str_expr(M_BODY_LEN),
        b64 = os_str_expr(M_BODY_B64),
        max = max_chars,
    )
}

/// Read the framed output back.
pub fn parse_inspect_output(out: &str) -> StreamRead {
    let mut found: Option<bool> = None;
    let mut opened_as = String::new();
    let mut size: u64 = 0;
    let mut declared_len: Option<usize> = None;
    let mut b64: Option<String> = None;
    for line in out.lines() {
        let l = line.trim_end_matches('\r');
        if let Some(v) = l.strip_prefix(M_FOUND) {
            found = Some(v.trim() == "1");
        } else if let Some(v) = l.strip_prefix(M_CLASS) {
            opened_as = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix(M_SIZE) {
            size = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix(M_BODY_LEN) {
            declared_len = v.trim().parse().ok();
        } else if let Some(v) = l.strip_prefix(M_BODY_B64) {
            b64 = Some(v.trim().to_string());
        }
    }
    match found {
        None => StreamRead::Unreadable,
        Some(false) => StreamRead::NotFound,
        Some(true) => {
            // A body that does not decode is NOT an empty body: the transport mangled it, and
            // saying "this stream holds nothing" about that is the whole failure mode here.
            let Some(enc) = b64 else {
                return StreamRead::Unreadable;
            };
            let Ok(bytes) = base64_decode(&enc) else {
                return StreamRead::Unreadable;
            };
            StreamRead::Read {
                opened_as,
                size,
                bytes,
                declared_len: declared_len.unwrap_or(0),
            }
        }
    }
}

/// Standard base64 with padding, as `$SYSTEM.Encryption.Base64Encode(x, 1)` emits it (the `1`
/// suppresses the line breaks it otherwise inserts every 76 characters).
fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut val = [255u8; 256];
    for (i, c) in T.iter().enumerate() {
        val[*c as usize] = i as u8;
    }
    let clean: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    if clean.iter().any(|b| val[*b as usize] == 255) {
        return Err(());
    }
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut acc: u32 = 0;
        for (i, b) in chunk.iter().enumerate() {
            acc |= (val[*b as usize] as u32) << (18 - 6 * i);
        }
        let take = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return Err(()),
        };
        for i in 0..take {
            out.push((acc >> (16 - 8 * i)) as u8);
        }
    }
    Ok(out)
}

/// The payload.
pub fn payload(id: &str, max_chars: usize, r: &StreamRead) -> serde_json::Value {
    match r {
        StreamRead::Unreadable => serde_json::json!({
            "stream_id": id, "error_code": "STREAM_UNREADABLE",
            "error": "the inspect program produced no usable answer for this stream — not an empty \
                      stream, an unusable reply. Retry; if it persists the namespace may be wrong."
        }),
        StreamRead::NotFound => serde_json::json!({
            "stream_id": id, "error_code": "STREAM_NOT_FOUND",
            "error": format!(
                "%ExistsId says there is no stream with id '{id}' in this namespace. This is NOT an \
                 empty stream — an empty one reports success with size 0. Check the id (it is the \
                 %Id of the stream, which for a message body comes from the body's own property) \
                 and check the namespace: a stream id is only meaningful in the namespace holding it."
            ),
        }),
        StreamRead::Read {
            opened_as,
            size,
            bytes,
            declared_len,
        } => {
            let mut v = serde_json::json!({
                "success": true,
                "stream_id": id,
                // Which class the read went through — NOT what the data is. Character and binary
                // streams share storage, so no read here can tell you which it "really" is.
                "opened_as": opened_as,
                "size": size,
                "empty": *size == 0,
                "body_bytes": bytes.len(),
            });
            // Exact, always: base64 survives CR, NUL and anything else a body can hold.
            v["body_base64"] = serde_json::Value::String(base64_encode(bytes));
            // And a readable rendering when the bytes are text. `from_utf8_lossy` would hide binary
            // behind replacement characters, so a body that is not valid UTF-8 gets no `body` field
            // rather than a mangled one.
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    v["body"] = serde_json::Value::String(text.to_string());
                    v["body_chars"] = serde_json::json!(text.chars().count());
                }
                Err(_) => {
                    v["body_not_text"] = serde_json::Value::String(
                        "these bytes are not valid UTF-8, so no `body` is given — read \
                         `body_base64`, which is exact."
                            .into(),
                    );
                }
            }
            if *size > max_chars as u64 {
                v["truncated"] = serde_json::Value::Bool(true);
                v["max_chars"] = serde_json::json!(max_chars);
            }
            if bytes.len() != *declared_len {
                v["body_incomplete"] = serde_json::Value::Bool(true);
                v["declared_len"] = serde_json::json!(declared_len);
                v["received_len"] = serde_json::json!(bytes.len());
            }
            v
        }
    }
}

/// Base64 with padding, so the exact bytes can be handed back to a caller.
fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let acc = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((acc >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures are the bytes a real instance returned. `MULTILINE` is a CR-separated HL7 v2 body
    /// written to a `%Stream.GlobalCharacter` and read back through `%Stream.GlobalBinary`: 106
    /// characters, two CRs, which the previous line protocol silently deleted.
    const HL7: &str = "MSH|^~\\&|HIS|H|DIET|D|20260924||ADT^A01|1|P|2.5\rPID|1||123456^^^H||DOE^JOHN||19700101|M\rPV1|1|I|WARD^01^02";

    fn reply(found: bool, size: u64, bytes: &[u8]) -> String {
        if !found {
            return format!("{M_FOUND}0\n");
        }
        format!(
            "{M_FOUND}1\n{M_CLASS}{BINARY_CLASS}\n{M_SIZE}{size}\n{M_BODY_LEN}{}\n{M_BODY_B64}{}\n",
            bytes.len(),
            base64_encode(bytes)
        )
    }

    /// The encoder pinned against RFC 4648's own vectors, NOT against my decoder.
    ///
    /// A mutation dropping the `=` padding SURVIVED the round-trip test below, because
    /// `base64_decode` filters padding out — so encoder and decoder agreed with each other while
    /// the encoder's output would have been rejected by any standard consumer. `body_base64` goes to
    /// a caller that will use a real base64 library, so the padding is part of the contract.
    #[test]
    fn the_encoder_matches_the_standard_vectors_padding_included() {
        for (raw, expect) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg=="),
            (&b"fo"[..], "Zm8="),
            (&b"foo"[..], "Zm9v"),
            (&b"foob"[..], "Zm9vYg=="),
            (&b"fooba"[..], "Zm9vYmE="),
            (&b"foobar"[..], "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(raw), expect, "raw={raw:?}");
        }
        // Length is always a multiple of 4 — the property a consumer relies on.
        for n in 0..20 {
            let v: Vec<u8> = (0..n).map(|i| i as u8).collect();
            assert_eq!(base64_encode(&v).len() % 4, 0, "n={n}");
        }
    }

    #[test]
    fn base64_round_trips_every_byte_including_cr_and_nul() {
        for case in [
            HL7.as_bytes().to_vec(),
            vec![0u8, 1, 2, 255, 254],
            b"".to_vec(),
            b"a".to_vec(),
            b"ab".to_vec(),
            b"abc".to_vec(),
            (0u8..=255).collect::<Vec<u8>>(),
        ] {
            let enc = base64_encode(&case);
            assert!(
                !enc.contains('\n'),
                "the encoding must stay on one line: {enc}"
            );
            assert_eq!(base64_decode(&enc).expect("decodes"), case, "enc={enc}");
        }
        // CONTROL: a string that is not base64 must fail rather than decode to something.
        assert!(base64_decode("not base64!!").is_err());
    }

    /// The defect that shipped: a CR-separated body came back with its segment boundaries deleted.
    #[test]
    fn a_cr_separated_body_keeps_its_separators() {
        let r = parse_inspect_output(&reply(true, 106, HL7.as_bytes()));
        let v = payload("5", 4096, &r);
        assert_eq!(v["body"], HL7, "the CRs must survive the round trip");
        assert_eq!(v["body"].as_str().unwrap().matches('\r').count(), 2);
        assert_eq!(v["body_bytes"], 106);
        assert!(r.body_complete());
    }

    /// The other defect: an id that does not exist came back as an empty stream, because `%OpenId`
    /// hands out a usable empty object for an unknown id. `%ExistsId` is what distinguishes them.
    #[test]
    fn a_missing_id_is_not_an_empty_stream() {
        let r = parse_inspect_output(&reply(false, 0, b""));
        assert_eq!(r, StreamRead::NotFound);
        let v = payload("999", 4096, &r);
        assert_eq!(v["error_code"], "STREAM_NOT_FOUND");
        assert!(v.get("empty").is_none(), "must not report emptiness: {v}");
        assert!(
            v.get("size").is_none(),
            "must not report a size it never read: {v}"
        );
        assert!(
            v["error"].as_str().unwrap().contains("NOT an empty stream"),
            "{v}"
        );
        // CONTROL: a stream that EXISTS and holds nothing is the other answer.
        let e = payload("5", 4096, &parse_inspect_output(&reply(true, 0, b"")));
        assert_eq!(e["success"], true);
        assert_eq!(e["empty"], true);
        assert_eq!(e["size"], 0);
    }

    /// The program must use the primitive that can tell them apart, and read through the class that
    /// preserves bytes.
    #[test]
    fn the_program_uses_existsid_and_reads_through_the_binary_class() {
        let code = build_inspect_code("5", 4096);
        assert!(code.contains("%ExistsId(tId)"), "{code}");
        assert!(
            code.matches(BINARY_CLASS).count() >= 2,
            "the read must go through the binary class, which preserves CR and NUL: {code}"
        );
        assert!(
            !code.contains(CHARACTER_CLASS),
            "opening as character truncates a binary body at the first NUL, and says nothing about \
             what the data is: {code}"
        );
        assert!(code.contains("Base64Encode(tBody,1)"), "{code}");
        // The defect that shipped: `write_marker_lines` deletes CR, so a CR-separated HL7 body
        // collapsed to one line with its segment boundaries gone. A mutation re-adding that
        // translate SURVIVED every parser test, because those feed hand-written marker text and
        // cannot see the ObjectScript. This is the assertion that can.
        assert!(
            !code.contains("$TRANSLATE"),
            "nothing may rewrite the body before it is encoded — deleting CR destroys an HL7 v2 \
             message's segment boundaries silently: {code}"
        );
        assert!(
            !code.contains("$CHAR(13)"),
            "the body must not be split or filtered on CR: {code}"
        );
        // And the body must go out through the encoder rather than any line protocol.
        assert!(!code.contains("$PIECE(tBody"), "{code}");
        assert!(code.contains(".Read(4096)"), "{code}");
        for mutation in ["Rewind", "MoveTo", ".Write(", "%Save", "Clear", "%Delete"] {
            assert!(
                !code.contains(mutation),
                "the program mutates the stream: {mutation}\n{code}"
            );
        }
    }

    /// Binary bytes come back exactly, and are NOT offered as text.
    #[test]
    fn binary_bytes_are_returned_exactly_and_not_as_text() {
        let raw = vec![0u8, 1, 2, 255, 254];
        let v = payload("6", 4096, &parse_inspect_output(&reply(true, 5, &raw)));
        assert_eq!(v["body_bytes"], 5);
        assert!(
            v.get("body").is_none(),
            "invalid UTF-8 must not be offered as text: {v}"
        );
        assert!(
            v["body_not_text"].as_str().unwrap().contains("body_base64"),
            "{v}"
        );
        assert_eq!(
            base64_decode(v["body_base64"].as_str().unwrap()).unwrap(),
            raw
        );
        // CONTROL: text DOES get a body field.
        let t = payload("5", 4096, &parse_inspect_output(&reply(true, 3, b"abc")));
        assert_eq!(t["body"], "abc");
        assert!(t.get("body_not_text").is_none());
    }

    #[test]
    fn a_body_longer_than_max_chars_is_flagged_truncated() {
        let v = payload(
            "7",
            20,
            &parse_inspect_output(&reply(true, 500, b"01234567890123456789")),
        );
        assert_eq!(v["truncated"], true);
        assert_eq!(v["max_chars"], 20);
        assert_eq!(v["size"], 500);
        // CONTROL: one that fits is not flagged.
        let f = payload("5", 20, &parse_inspect_output(&reply(true, 3, b"abc")));
        assert!(f.get("truncated").is_none(), "{f}");
    }

    /// Fewer bytes than IRIS declared: a prefix, said out loud.
    #[test]
    fn a_short_arrival_is_flagged_not_silently_shortened() {
        let short = format!(
            "{M_FOUND}1\n{M_CLASS}{BINARY_CLASS}\n{M_SIZE}100\n{M_BODY_LEN}100\n{M_BODY_B64}{}\n",
            base64_encode(b"abc")
        );
        let r = parse_inspect_output(&short);
        assert!(!r.body_complete());
        let v = payload("5", 4096, &r);
        assert_eq!(v["body_incomplete"], true);
        assert_eq!(v["declared_len"], 100);
        assert_eq!(v["received_len"], 3);
    }

    /// A body that does not decode is NOT an empty body.
    #[test]
    fn an_undecodable_body_is_unreadable_not_empty() {
        let bad = format!("{M_FOUND}1\n{M_CLASS}{BINARY_CLASS}\n{M_SIZE}5\n{M_BODY_LEN}5\n{M_BODY_B64}!!!not!!!\n");
        assert_eq!(parse_inspect_output(&bad), StreamRead::Unreadable);
        let v = payload("5", 4096, &parse_inspect_output(&bad));
        assert_eq!(v["error_code"], "STREAM_UNREADABLE");
        assert!(v.get("empty").is_none(), "{v}");
        // And a reply with no marker at all is the same outcome.
        assert_eq!(parse_inspect_output("unrelated\n"), StreamRead::Unreadable);
    }

    /// `opened_as` is which class the read used, not a claim about the data.
    #[test]
    fn opened_as_does_not_claim_what_the_data_is() {
        let v = payload("6", 4096, &parse_inspect_output(&reply(true, 15, b"text")));
        assert_eq!(v["opened_as"], BINARY_CLASS);
        assert!(
            v.get("class").is_none(),
            "the old field claimed a kind it cannot know: {v}"
        );
    }
}
