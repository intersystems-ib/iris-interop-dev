//! Safe embedding of user-supplied strings into generated ObjectScript source.
//!
//! ObjectScript string literals have exactly one escape: a quote is doubled
//! (`""`). Backslash is an ordinary character — C-style `\"` is a syntax error
//! (the `\` stays literal and the `"` terminates the string). A literal cannot
//! span source lines, and `build_exec_class` splits generated code on `\n`, so
//! control characters must never appear inside a literal; they are spliced in
//! via `$CHAR(n,...)` instead.
//!
//! #119 follow-up: the same is true of every NON-ASCII character. Generated
//! source reaches IRIS over two transports, and only one of them is charset
//! transparent: `execute_via_generator` PUTs the source as UTF-8 JSON, but
//! `IrisConnection::execute` pipes it into `docker exec -i <c> iris session`,
//! whose stdin is decoded 8-bit — the two UTF-8 bytes of `é` arrive as the two
//! characters `Ã©`, and a CJK character arrives as three. A search needle or a
//! skill name embedded as a raw literal therefore silently stops matching the
//! data actually stored in IRIS. So the output of `os_str_expr` is always pure
//! 7-bit ASCII: anything outside `0x20..=0x7E` is spliced via `$CHAR`, which
//! means the source survives ANY transport byte-for-byte.

/// Render `s` as a single-line ObjectScript *expression* that evaluates to
/// exactly `s`: printable-ASCII runs become quoted literals with `"` doubled,
/// everything else (control characters AND all non-ASCII) becomes a
/// `$CHAR(n,...)` splice, so the rendered expression is pure ASCII.
///
/// `os_str_expr(r#"say "hi""#)` → `"say ""hi"""`
/// `os_str_expr("a\r\nb")` → `"a"_$CHAR(13,10)_"b"`
/// `os_str_expr("café")` → `"caf"_$CHAR(233)`
///
/// A character outside the BMP is spliced as its two UTF-16 surrogate code
/// units — that is how IRIS stores it. `$CHAR` of a code point above 65535
/// does NOT round-trip: on IRIS 2026.2 `$char(128512)` returns the EMPTY
/// string (verified live), so splicing the raw code point would silently drop
/// the character.
pub fn os_str_expr(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".into();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut lit = String::new();
    let mut spliced: Vec<u32> = Vec::new();
    let flush_lit = |lit: &mut String, parts: &mut Vec<String>| {
        if !lit.is_empty() {
            parts.push(format!("\"{}\"", lit.replace('"', "\"\"")));
            lit.clear();
        }
    };
    let flush_spliced = |spliced: &mut Vec<u32>, parts: &mut Vec<String>| {
        if !spliced.is_empty() {
            let codes: Vec<String> = spliced.iter().map(|c| c.to_string()).collect();
            parts.push(format!("$CHAR({})", codes.join(",")));
            spliced.clear();
        }
    };
    let mut utf16 = [0u16; 2];
    for ch in s.chars() {
        let code = ch as u32;
        if (0x20..0x7f).contains(&code) {
            flush_spliced(&mut spliced, &mut parts);
            lit.push(ch);
        } else {
            flush_lit(&mut lit, &mut parts);
            for unit in ch.encode_utf16(&mut utf16) {
                spliced.push(*unit as u32);
            }
        }
    }
    flush_lit(&mut lit, &mut parts);
    flush_spliced(&mut spliced, &mut parts);
    parts.join("_")
}

/// Statements that write `payload` to the open stream held in `stream_var`,
/// chunked so no generated source line approaches the routine line-length
/// limit (worst case a chunk of quotes doubles, plus `$CHAR` overhead).
pub fn os_stream_write_stmts(stream_var: &str, payload: &str, chunk_chars: usize) -> Vec<String> {
    let chars: Vec<char> = payload.chars().collect();
    chars
        .chunks(chunk_chars.max(1))
        .map(|c| {
            let piece: String = c.iter().collect();
            format!("Do {}.Write({})", stream_var, os_str_expr(&piece))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_string_is_quoted() {
        assert_eq!(os_str_expr("GeneroSOAP"), "\"GeneroSOAP\"");
    }

    #[test]
    fn empty_string() {
        assert_eq!(os_str_expr(""), "\"\"");
    }

    #[test]
    fn quotes_are_doubled_not_backslashed() {
        assert_eq!(os_str_expr(r#"key="M""#), r#""key=""M""""#);
        assert!(!os_str_expr(r#"a"b"#).contains('\\'));
    }

    #[test]
    fn backslash_is_literal() {
        assert_eq!(os_str_expr(r"C:\tmp"), "\"C:\\tmp\"");
    }

    #[test]
    fn apostrophe_untouched() {
        assert_eq!(os_str_expr("it's"), "\"it's\"");
    }

    #[test]
    fn newlines_become_char_splices() {
        assert_eq!(os_str_expr("a\nb"), "\"a\"_$CHAR(10)_\"b\"");
        assert_eq!(os_str_expr("a\r\nb"), "\"a\"_$CHAR(13,10)_\"b\"");
        assert_eq!(os_str_expr("\n"), "$CHAR(10)");
    }

    /// #119: a raw non-ASCII literal does NOT survive `docker exec … iris session`
    /// (8-bit stdin turns the two UTF-8 bytes of `ñ` into two characters), so it is
    /// spliced as `$CHAR` — the same treatment control characters already got.
    #[test]
    fn non_ascii_is_char_spliced_not_written_raw() {
        assert_eq!(os_str_expr("señal"), "\"se\"_$CHAR(241)_\"al\"");
        assert_eq!(os_str_expr("café"), "\"caf\"_$CHAR(233)");
        // A run of non-ASCII collapses into ONE $CHAR list.
        assert_eq!(os_str_expr("中文"), "$CHAR(20013,25991)");
        assert_eq!(
            os_str_expr("producción"),
            "\"producci\"_$CHAR(243)_\"n\"",
            "the shipped Spanish trigger vocabulary must round-trip"
        );
    }

    /// A code point above the BMP must be spliced as its two UTF-16 units: verified
    /// live on IRIS 2026.2, `$char(128512)` returns "" (length 0) while
    /// `$char(55357,56832)` returns the emoji.
    #[test]
    fn astral_chars_are_spliced_as_utf16_surrogate_pairs() {
        assert_eq!(os_str_expr("😀"), "$CHAR(55357,56832)");
        assert!(!os_str_expr("😀").contains("128512"));
    }

    /// The whole point: generated source is transport-independent because it is 7-bit.
    #[test]
    fn every_expr_is_pure_ascii() {
        for s in [
            "señal",
            "café ñ 中文 description",
            "unicode-café-中",
            "😀 mixed \u{7f} and \t",
            "producción/notificación",
        ] {
            let expr = os_str_expr(s);
            assert!(
                expr.is_ascii(),
                "generated source must be pure ASCII to survive an 8-bit transport: {expr}"
            );
        }
    }

    #[test]
    fn every_expr_line_has_balanced_quotes() {
        let samples = [
            "plain",
            r#"<entry table="G" key="M">1</entry>"#,
            "multi\nline\r\nwith\ttabs",
            "quote\"and'apostrophe",
            "acentuación \"citada\" 中",
        ];
        for s in samples {
            let expr = os_str_expr(s);
            assert!(!expr.contains('\n'), "expr must be single-line: {expr}");
            assert_eq!(
                expr.matches('"').count() % 2,
                0,
                "unbalanced quotes in {expr}"
            );
        }
    }

    #[test]
    fn write_stmts_chunk_and_cover_payload() {
        let payload = "x".repeat(1000);
        let stmts = os_stream_write_stmts("tStream", &payload, 400);
        assert_eq!(stmts.len(), 3);
        assert!(stmts.iter().all(|s| s.starts_with("Do tStream.Write(")));
        // chunking must never split on a non-char boundary
        let stmts = os_stream_write_stmts("tStream", &"ñ".repeat(401), 400);
        assert_eq!(stmts.len(), 2);
    }
}
