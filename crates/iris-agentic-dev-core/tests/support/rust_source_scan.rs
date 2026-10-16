//! One Rust-source scanner, shared by the two source-text gates in this directory.
//!
//! ## Why this is a module and not a second copy
//!
//! #378 fixed a brace matcher that counted `{` and `}` over the RAW text, so a brace inside a
//! string or char literal moved the closing brace. The fix landed in
//! `no_literal_leaks_its_source_indentation.rs` and left the original it was copied from —
//! `every_refusal_names_a_remedy.rs` — still counting braces over raw text. The review of #378
//! named that sibling, and it was right: on `gateway_manage.rs` the raw matcher closed the single
//! `#[cfg(test)] mod tests {` (line 1430) at line 1618 instead of 2899, because line 1616 holds
//! `rest.find([',', '}'])` — a `'}'` char literal, which the raw count reads as a closing brace.
//! 1281 lines of test code therefore sat inside what that gate calls the PRODUCTION population.
//!
//! Copying the repaired version a third time would have put the same defect in three places. This
//! module is the one copy, so the next fix to the scanner reaches every gate that matches braces
//! over Rust source.
//!
//! ## The invariant the whole thing rests on
//!
//! `scan` makes ONE pass and returns two things: the literals, and a MASK — the same `char`
//! sequence with every comment, char literal, raw string and string body blanked to spaces and
//! every newline left where it was. Braces are code only in the mask, and because the mask is
//! index-for-index the same length as the source, a range found in the mask slices the source.
//! `a_mask_is_the_same_length_as_its_source` in the leak gate is the control for that invariant.
//!
//! Comments and literals are recognised in the same pass because they are the same job: a `//`
//! inside `"https://host/x"` is not a comment, and a `"` inside `// see "x` does not open a
//! literal. A chain that stripped comments first would truncate every URL literal in the tree at
//! its scheme.
//!
//! ## Dead code
//!
//! Each consumer uses a subset: the leak gate wants `literals` plus `test_mod_ranges`/`in_any`, the
//! remedy gate wants `strip_test_mods` and `mask_of`. Neither uses all of it, so a per-item
//! `#[allow(dead_code)]` would have to be on nearly every item. The module-level allow is the
//! honest form, and nothing here is reachable from the product.

#![allow(dead_code)]

/// One non-raw string literal pulled out of a source file.
pub struct Literal {
    /// Index into the file's `char` sequence of the opening quote.
    pub offset: usize,
    /// 1-based line of the opening quote.
    pub line: usize,
    /// The literal's value, as the compiler would build it.
    pub value: String,
}

/// The result of one pass over a source file.
pub struct Scan {
    /// The same `char` sequence as the input, with every comment, char literal, raw string and
    /// string literal blanked to spaces and every newline kept where it was. Braces are code only
    /// here.
    pub mask: Vec<char>,
    pub literals: Vec<Literal>,
}

/// Decode a non-raw literal body (the text between the quotes) the way the compiler does.
///
/// The case this file exists for is the first arm: `\` + newline + leading whitespace collapses to
/// nothing, which is why a correct continuation yields single spaces and a collapsed one does not.
pub fn decode(body: &str) -> String {
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

pub fn blank(mask: &mut [char], src: &[char], from: usize, to: usize) {
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
pub fn scan(src: &str) -> Scan {
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

pub fn find_chars(hay: &[char], needle: &str, from: usize) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || hay.len() < n.len() {
        return None;
    }
    (from..=hay.len() - n.len()).find(|&s| hay[s..s + n.len()] == n[..])
}

/// Index just past the `}` matching the `{` at `open`, counted over the MASK, or `None` when the
/// depth never returns to zero.
///
/// #378 review, nit 5: the earlier version returned `mask.len()` in that case — a FAILURE rendered
/// as a clean fact. A scanner desync would silently extend a `#[cfg(test)] mod` window to
/// end-of-file, every literal after it would be counted as a stripped test fixture, and both of the
/// leak gate's floors would move the wrong way at once: `dropped_test_literals` up, satisfying its
/// floor more easily, and `product_literals` down. Most of the population could vanish and the gate
/// would still print a clean pass. `None` makes the caller decide, and every caller here refuses.
///
/// This is not hypothetical for the matcher this replaced: measured over `crates/*/src`, the raw
/// version ran off the end in FOUR files (`coverage.rs`, `interop.rs`, `scm.rs`, `objectscript.rs`)
/// and escaped notice only because the test mod was the last item in each.
pub fn match_brace(mask: &[char], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (off, ch) in mask[open..].iter().enumerate() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + off + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// 1-based line of a mask index, for a message that names where the scan came apart.
pub fn line_of(mask: &[char], idx: usize) -> usize {
    mask[..idx.min(mask.len())]
        .iter()
        .filter(|&&c| c == '\n')
        .count()
        + 1
}

/// `(net, lowest)` brace depth over the whole mask. Well-formed Rust gives `(0, 0)`.
///
/// Anything else means the single-pass scanner mis-lexed something — a literal form it does not
/// know — and every window matched over that mask is suspect. A caller that walks files can assert
/// this per file, which catches a desync even in a file with no `#[cfg(test)] mod` to go wrong.
pub fn brace_depth(mask: &[char]) -> (i32, i32) {
    let mut depth = 0i32;
    let mut lowest = 0i32;
    for &c in mask {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < lowest {
                    lowest = depth;
                }
            }
            _ => {}
        }
    }
    (depth, lowest)
}

/// `(start, end)` for every `#[cfg(test)] mod NAME { … }`, brace-matched from the module's OWN brace.
///
/// Requiring the `mod NAME {` shape is not decoration. `every_refusal_names_a_remedy.rs` records
/// what "the next `{` after the attribute" did: two `#[cfg(test)]` attributes in `mod.rs` decorate a
/// `pub(crate) const … = &[`, so the search jumped past the `[` to an unrelated brace far below and
/// swallowed about a thousand lines — reporting `mod.rs` as having zero production refusals, which
/// is impossible in a file that both defines and calls `err_json`.
pub fn test_mod_ranges(mask: &[char]) -> Vec<(usize, usize)> {
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
                    let end = match_brace(mask, brace).unwrap_or_else(|| {
                        panic!(
                            "unmatched `{{` at line {} of a `#[cfg(test)] mod`: the scanner's brace \
                             depth never returned to zero, so this window would run to end-of-file \
                             and every literal after it would be counted as a stripped test \
                             fixture. That is a scan failure that reads as a clean population — \
                             refusing it instead of reporting it.",
                            line_of(mask, brace)
                        )
                    });
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

pub fn in_any(ranges: &[(usize, usize)], offset: usize) -> bool {
    ranges.iter().any(|&(s, e)| offset >= s && offset < e)
}

/// Just the mask, for a caller that matches windows but does not want the literals.
pub fn mask_of(text: &str) -> Vec<char> {
    scan(text).mask
}

/// Index just past the `)` matching the `(` whose index is `after_open - 1`, counted over the MASK.
///
/// Same reason as `match_brace`: `rest.find([',', ')'])` is not a closing paren, and neither is the
/// one in `"… (expected …"`. Measured at the time of writing, no `classify_iris_error_or(` call in
/// the tree carries a paren inside a literal, so this is identical in behaviour to the raw count it
/// replaces — it is here so the file does not keep one hardened matcher next to one that is not.
pub fn match_paren(mask: &[char], after_open: usize) -> Option<usize> {
    let mut depth = 1i32;
    for (off, ch) in mask[after_open..].iter().enumerate() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(after_open + off);
                }
            }
            _ => {}
        }
    }
    None
}

/// `text` with every `#[cfg(test)] mod NAME { … }` block removed, braces matched over the mask.
///
/// The literals are kept: a caller that scans for `err_json("CODE", …)` needs the string bodies
/// intact, so the mask decides only WHERE the module ends, and the cut is made in the source.
/// Because the mask is index-for-index the same length as the source's `char` sequence, the ranges
/// transfer directly.
pub fn strip_test_mods(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let ranges = test_mod_ranges(&mask_of(text));
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    // `test_mod_ranges` walks forward and resumes at each range's end, so the ranges it returns are
    // sorted and disjoint. Asserting it here rather than trusting it: a range that went backwards
    // would panic on the slice below instead of silently dropping the wrong window.
    for &(start, end) in &ranges {
        assert!(
            i <= start && start <= end && end <= chars.len(),
            "test-mod ranges are not sorted and disjoint: {ranges:?} (at {i})"
        );
        out.extend(&chars[i..start]);
        i = end;
    }
    out.extend(&chars[i..]);
    out
}

/// The body of `fn NAME(`, from its opening brace to the matching one, braces matched over the mask.
///
/// Returns the SOURCE window, comments and literals intact — the caller decides what to strip from
/// it, because a caller reading string literals out of the window and one reading code need
/// different things. The `fn NAME(` itself is located in the mask, so an occurrence in a comment or
/// a string cannot be mistaken for the definition.
pub fn fn_body(text: &str, name: &str) -> Option<String> {
    let mask = mask_of(text);
    let chars: Vec<char> = text.chars().collect();
    let pat = format!("fn {name}(");
    let at = find_chars(&mask, &pat, 0)?;
    let brace = mask[at..].iter().position(|&c| c == '{')? + at;
    // `None` here is NOT "no such function" — it is "found it and could not delimit it", which must
    // not be returned as the same answer. A caller reads `None` as "the window moved" and panics
    // with a message about the wrong thing.
    let end = match_brace(&mask, brace)
        .unwrap_or_else(|| {
            panic!(
                "found `{pat}` at line {} but its body has no matching `}}`: the scanner desynced, \
                 so any window taken from this file would be wrong",
                line_of(&mask, brace)
            )
        })
        .min(chars.len());
    Some(chars[brace..end].iter().collect())
}
