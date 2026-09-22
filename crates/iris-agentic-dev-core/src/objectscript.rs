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

/// What the declaration line of a [`write_marker_lines`] block declares — and therefore what the
/// reader can check the arrival against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Declared {
    /// The exact character count of the whole field, newline separators included. Catches a missing
    /// line AND a last line that arrived half-written.
    Chars,
    /// The number of lines. Weaker than [`Declared::Chars`], and the right choice for a reader that
    /// TRIMS each line before matching it: a trimmed line no longer carries the character count it
    /// was measured with, so a character declaration would report a false short read.
    Lines,
}

/// Generated ObjectScript that carries `var`'s value through a LINE-ORIENTED marker protocol
/// intact, whatever newlines it contains: one `line_marker`-prefixed line per line of the value,
/// preceded by one `declaration_marker` line saying how much there is to reassemble.
///
/// This exists because of `$SYSTEM.Status.GetErrorText`. On a chained `%Status` it returns the WHOLE
/// chain CRLF-joined. Measured on IRIS 2026.1 and 2025c, a two-element chain built with
/// `$system.Status.AppendStatus` decodes to 51 characters in two CRLF-separated pieces:
///
/// ```text
/// ERROR #5001: first cause<CR><LF>ERROR #5001: second cause
/// ```
///
/// Written as a single marker line, element 1 is captured by the reader's `strip_prefix` arm and
/// elements 2..n land on lines that match no arm at all and are dropped silently — no error, no log
/// line, no shortened-output marker. Worse, a dropped line that happens to begin with ANOTHER
/// marker's literal is parsed as that field, so the loss becomes corruption.
///
/// Repeating the marker closes both: every line of the value is attributed to this field, and no
/// content of it can be mistaken for another one. The declaration is what makes a SHORT arrival
/// detectable, so the reader can report "I could not reassemble this" as its own case instead of a
/// shorter-but-plausible value — the mechanism `IEM_VALUE_LEN` already proves for
/// `iris_execute_method`'s return value.
///
/// CR is deleted first, so the declaration is measured on exactly what the reader reassembles: the
/// reader joins the lines with `\n`, and Rust's `str::lines()` has already dropped any `\r` for it.
/// Deleting rather than translating means a LONE CR (no LF) joins two elements instead of splitting
/// them — content is preserved either way, and `GetErrorText` uses CRLF.
///
/// One statement per line, because `IrisConnection::build_exec_class` splits generated code on
/// `\n`: the `for` loop and its body must stay on one line.
pub fn write_marker_lines(
    var: &str,
    declaration_marker: &str,
    line_marker: &str,
    declared: Declared,
) -> String {
    let n = match declared {
        Declared::Chars => format!("$LENGTH({var})"),
        Declared::Lines => format!("$LENGTH({var},$CHAR(10))"),
    };
    format!(
        "set {var}=$TRANSLATE({var},$CHAR(13))\n\
         write {decl}_{n}_$CHAR(10)\n\
         for tMLI=1:1:$LENGTH({var},$CHAR(10)) {{ write {line}_$PIECE({var},$CHAR(10),tMLI)_$CHAR(10) }}",
        decl = os_str_expr(declaration_marker),
        line = os_str_expr(line_marker),
    )
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

    /// The block must repeat the marker per line and declare a count — those two together are what
    /// make a chain carryable and a short arrival detectable.
    #[test]
    fn marker_lines_repeat_the_marker_and_declare_a_count() {
        let b = write_marker_lines("tX", "M_LEN\t", "M\t", Declared::Chars);
        // CR removed before measuring, or the declaration counts characters the reader never sees.
        assert!(b.contains("set tX=$TRANSLATE(tX,$CHAR(13))"), "{b}");
        // The declaration is the CHARACTER count, not the line count.
        assert!(
            b.contains("write \"M_LEN\"_$CHAR(9)_$LENGTH(tX)_$CHAR(10)"),
            "{b}"
        );
        // One write per line, driven by $PIECE — this is what carries element 2..n at all.
        assert!(
            b.contains("for tMLI=1:1:$LENGTH(tX,$CHAR(10)) {")
                && b.contains("_$PIECE(tX,$CHAR(10),tMLI)_"),
            "the value must be written one line per piece: {b}"
        );
        // Every generated statement on its own line: build_exec_class splits on '\n'.
        assert_eq!(b.lines().count(), 3, "{b}");
    }

    /// `Declared::Lines` is the variant for a reader that trims; it must declare a piece COUNT.
    #[test]
    fn a_line_declaration_counts_pieces_not_characters() {
        let b = write_marker_lines("tX", "M_LINES:", "M:", Declared::Lines);
        assert!(
            b.contains("write \"M_LINES:\"_$LENGTH(tX,$CHAR(10))_$CHAR(10)"),
            "{b}"
        );
        assert!(
            !b.contains("_$LENGTH(tX)_"),
            "a Lines declaration must not emit the character count: {b}"
        );
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
