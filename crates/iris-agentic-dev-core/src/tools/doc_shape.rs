//! #430 item 1: `iris_doc(get, shape='skeleton')` — the API surface of a class without its method
//! bodies, plus `strip_license` and a bounded default for built-in packages.
//!
//! WHY THIS IS A SEPARATE MODULE, and why every function here is pure: a skeleton is a LOSSY view.
//! The whole safety argument rests on two properties that are only testable if the transformation is
//! a function of a string:
//!
//!   1. it is **self-identifying** — the output carries [`SKELETON_MARKER`] at every point where
//!      something was removed, so a caller (or a later tool call) can tell a skeleton from source;
//!   2. it is **refused on write** — [`looks_like_a_skeleton`] is what `iris_doc(put)` checks, and
//!      writing a skeleton back would destroy every method body and the generated Storage block.
//!
//! An elided body is NEVER left as a bare `{}`. An empty method body is a legitimate thing for a
//! class to contain, so a bare `{}` would turn "I did not fetch this" into the false statement "this
//! method does nothing" — the repo's dominant bug shape (CLAUDE.md). Each elision says so in place.
//!
//! The node shape below is DUMPED from the parser, not read off `node-types.json` — and the
//! difference mattered. `node-types.json` lists `external_method_body_content` as a possible child
//! of `method_definition`, so the first version of this module keyed on it and elided **zero**
//! method bodies while correctly dropping Storage. The dump says why:
//!
//! ```text
//! class_statement
//!   method | classmethod               [keyword_method, then:]
//!     method_definition
//!       method_name, arguments, return_type
//!       {                              <- anonymous
//!       statement  "Set total = a + b"  }
//!       statement  "Quit total"         } THE BODY: ObjectScript parses into real statements
//!       }                              <- anonymous
//!   storage                            <- the whole member is dropped
//!   xdata | query | trigger
//!     external_method_body_content     <- KEPT, see below
//! ```
//!
//! `external_method_body_content` is the EXTERNAL-language body — a `[ Language = python ]` method,
//! and the XML inside `Storage` and `XData`. An ObjectScript body is not one node but a run of
//! `statement` siblings, so the body is identified by its DELIMITERS: everything between the
//! anonymous `{` and `}` children of `method_definition`. That span covers an ObjectScript body and
//! a Python one alike, which is the reason to cut on the braces rather than on any body node kind.
//!
//! `xdata` and `query` bodies are KEPT deliberately. An XData block is where a DTL and a BPL
//! actually live, and a Query's body is its SQL — for those members the "body" IS the API surface,
//! so eliding them would not be a leaner view of the class, it would be a different class. Only the
//! method family (`Method`, `ClassMethod`, `ClientMethod`) and `Storage` come out.

/// The text left behind wherever a method body was removed.
///
/// This is a CONTRACT, not a message: [`looks_like_a_skeleton`] greps for it to refuse a write, and
/// `a_skeleton_is_refused_on_write` fails if the two drift apart. Reword it and a skeleton stops
/// being detectable, so the write guard goes quiet while still passing a source-text assertion —
/// which is why the guard reads this constant instead of spelling the text again.
pub const SKELETON_MARKER: &str = "elided by iris_doc(shape='skeleton')";

/// The one-line banner prepended to a skeleton.
///
/// It repeats the marker so that a caller who keeps only the first line still holds a detectable
/// skeleton, and so the refusal can be explained by pointing at the content itself.
pub const SKELETON_BANNER: &str = "// *** PARTIAL VIEW: method bodies and Storage were \
                                   elided by iris_doc(shape='skeleton'). Writing this back would \
                                   DESTROY them. Re-read with shape='full' before editing. ***";

/// Built-in packages whose source is documentation, not the caller's code, and is the long tail
/// iris_doc was measured on (#430: p90 5.3k, max 12.6k chars across 129 calls).
const BUILTIN_PACKAGE_PREFIXES: [&str; 4] = ["%", "Ens", "EnsLib", "HS"];

/// The default byte cap applied to a built-in class when the caller named no `max_bytes`.
///
/// Chosen above the measured p90 (5.3k) so an ordinary built-in read still arrives whole, and below
/// the measured max (12.6k) so the tail is actually bounded. A caller who wants the whole thing says
/// `max_bytes: 0`, which still means unlimited — the default is a default, not a ceiling.
pub const BUILTIN_DEFAULT_MAX_BYTES: usize = 8192;

/// Which view of a document `get` should return.
///
/// `parse` is deliberately STRICT. #435 established the rule for this repo's filter enums the hard
/// way: a near-miss value must be REFUSED, never widened to the permissive case, because silently
/// treating `shape='skeletons'` as `full` returns 12k of source to a caller who asked to be lean and
/// nothing says why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocShape {
    /// The document exactly as IRIS stores it.
    Full,
    /// Class header, Parameters, Properties, XData and method SIGNATURES; no method bodies, no
    /// Storage block.
    Skeleton,
}

impl DocShape {
    /// `None` and `""` mean `Full`, so an omitted parameter keeps today's behaviour.
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim).unwrap_or("") {
            "" | "full" => Ok(Self::Full),
            "skeleton" => Ok(Self::Skeleton),
            other => Err(format!(
                "Unknown shape '{other}'. Use 'full' (the whole document, the default) or \
                 'skeleton' (signatures only, no method bodies). Refused rather than guessed, \
                 because returning full source to a caller who asked for a skeleton wastes the \
                 context they were trying to save."
            )),
        }
    }
}

/// Whether `name` lives in a package whose reads get [`BUILTIN_DEFAULT_MAX_BYTES`] by default.
///
/// Matched on the package SEGMENT, not as a bare string prefix: `Ensemble.Foo` is not `Ens`, and a
/// user class called `HSCustom.Thing` is not `HS`. Getting this wrong would silently truncate a
/// caller's own class, which is the opposite of the intent.
pub fn is_builtin_package(name: &str) -> bool {
    let name = name.trim();
    // A `%` class is spelled `%Library.File` — the sigil is the package, so a segment split would
    // yield "%Library" and never match "%". Handled before the split for that reason.
    if name.starts_with('%') {
        return true;
    }
    match name.split_once('.') {
        Some((pkg, _)) => BUILTIN_PACKAGE_PREFIXES.contains(&pkg),
        // No dot: a bare routine or a class with no package. Not a built-in.
        None => false,
    }
}

/// What [`skeleton`] did. Three cases, because "the output equals the input" has two very different
/// causes and a caller that cannot tell them apart will report the wrong thing.
#[derive(Debug, PartialEq, Eq)]
pub enum Skeletonized {
    /// Something came out. `bodies` counts elided method bodies; `storage` is whether a Storage
    /// block was dropped.
    Elided {
        content: String,
        bodies: usize,
        storage: bool,
    },
    /// The document parsed as a class and had no method bodies and no Storage to remove — a pure
    /// interface, a `.inc`, a message class of nothing but Properties. The full text IS the
    /// skeleton, and saying so is not the same as failing.
    NothingToElide,
    /// The UDL parse found no class at all: a `.mac`/`.int` routine, or source the grammar could not
    /// read. There is no skeleton to build, and returning the source labelled as one would be a lie.
    NotAClass,
}

/// Build the skeleton of a UDL class.
///
/// Byte-exact everywhere it does not elide: the ranges the parser reports are spliced out of the
/// original string and nothing is re-serialised, so comments, keywords, `/// ` documentation and
/// line endings survive. `str::lines()` is NOT used anywhere in this module for the same reason it
/// is banned on the line-edit path — it strips a trailing `\r`, which would LF-convert a CRLF class.
pub fn skeleton(source: &str) -> Skeletonized {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_objectscript::LANGUAGE_OBJECTSCRIPT_UDL.into())
        .is_err()
    {
        return Skeletonized::NotAClass;
    }
    let Some(tree) = parser.parse(source, None) else {
        return Skeletonized::NotAClass;
    };
    let root = tree.root_node();

    // No class_definition means this is not a class. Checked BEFORE looking for bodies, so a
    // routine that happens to contain no `{}` is reported as NotAClass rather than NothingToElide.
    let mut has_class = false;
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "class_definition" {
            has_class = true;
            break;
        }
    }
    if !has_class {
        return Skeletonized::NotAClass;
    }

    let mut bodies: Vec<std::ops::Range<usize>> = Vec::new();
    let mut storages: Vec<std::ops::Range<usize>> = Vec::new();
    collect_elisions(root, source, &mut bodies, &mut storages);

    if bodies.is_empty() && storages.is_empty() {
        return Skeletonized::NothingToElide;
    }

    // One ordered edit list. A body range can never overlap a storage range (a Storage block holds
    // no method), so sorting by start is enough to splice in one left-to-right pass.
    enum Cut {
        Body(std::ops::Range<usize>),
        Storage(std::ops::Range<usize>),
    }
    let mut cuts: Vec<Cut> = bodies
        .iter()
        .cloned()
        .map(Cut::Body)
        .chain(storages.iter().cloned().map(Cut::Storage))
        .collect();
    cuts.sort_by_key(|c| match c {
        Cut::Body(r) | Cut::Storage(r) => r.start,
    });

    let mut out = String::with_capacity(source.len());
    out.push_str(SKELETON_BANNER);
    out.push('\n');
    let mut at = 0usize;
    for cut in &cuts {
        let (range, replacement) = match cut {
            Cut::Body(r) => (r.clone(), format!("\n\t// ... {SKELETON_MARKER}\n")),
            Cut::Storage(r) => (r.clone(), format!("// Storage {SKELETON_MARKER}\n")),
        };
        if range.start < at {
            // Overlapping cuts would corrupt the output. Not reachable with the node kinds above,
            // and skipping is the safe direction: a body left IN is merely verbose, whereas a bad
            // splice would hand back unparseable text.
            continue;
        }
        out.push_str(&source[at..range.start]);
        out.push_str(&replacement);
        at = range.end;
    }
    out.push_str(&source[at..]);

    Skeletonized::Elided {
        content: out,
        bodies: bodies.len(),
        storage: !storages.is_empty(),
    }
}

/// Walk the tree collecting the byte ranges to remove.
///
/// Descent STOPS at every node that produces a cut, so one member can never contribute two
/// overlapping ranges — a `storage` block holds an `external_method_body_content` of its own, and a
/// `method_definition` holds the statements that make up its body.
fn collect_elisions(
    node: tree_sitter::Node,
    source: &str,
    bodies: &mut Vec<std::ops::Range<usize>>,
    storages: &mut Vec<std::ops::Range<usize>>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "storage" => {
                storages.push(storage_cut(child));
                continue;
            }
            "method_definition" | "clientmethod" => {
                if let Some(span) = body_span(child, source) {
                    bodies.push(span);
                }
                continue;
            }
            _ => {}
        }
        collect_elisions(child, source, bodies, storages);
    }
}

/// The byte range between a method's braces, or `None` when there is nothing there to remove.
///
/// `None` for an empty body is load-bearing in two directions. It keeps `bodies` an honest count of
/// what was actually elided, and it is what lets a class of empty methods report
/// [`Skeletonized::NothingToElide`] — which is returned WITHOUT the banner, so `put` does not
/// refuse a document that would lose nothing by being written back.
///
/// On `}` the later value overwrites the earlier, which reads as "take the last one". MEASURED: it
/// makes no difference today, because `method_definition` has exactly ONE brace pair among its
/// direct children — a nested `If x { ... }` sits inside a `statement` node, so its braces are
/// grandchildren and never arrive here. A mutation to take the FIRST `}` survived, and that is what
/// established this; an earlier version of this comment claimed the last-brace choice was what kept
/// the cut off an inner brace, which was a rationale for a mechanism that does not exist.
/// `a_method_definition_has_exactly_one_brace_pair_as_direct_children` pins the measurement, so if
/// the grammar ever flattens nested blocks the assumption fails loudly instead of silently cutting
/// a body short.
fn body_span(method: tree_sitter::Node, source: &str) -> Option<std::ops::Range<usize>> {
    let mut cursor = method.walk();
    let mut open: Option<usize> = None;
    let mut close: Option<usize> = None;
    for child in method.children(&mut cursor) {
        match child.kind() {
            "{" if open.is_none() => open = Some(child.end_byte()),
            "}" => close = Some(child.start_byte()),
            _ => {}
        }
    }
    let (start, end) = (open?, close?);
    // A malformed parse can put `}` before `{`; an empty body makes them equal. Neither is a cut.
    if end <= start {
        return None;
    }
    // WHITESPACE-ONLY is also not a body, and this is the case that actually occurs: `Method A()`
    // over `{\n}` leaves a span holding one newline, so a byte-length test calls it a body. An
    // abstract or stub method is spelled exactly that way, so without this a class of stubs came
    // back banner-marked — and `put` then REFUSES a document that would lose nothing by being
    // written back. Found by a mutation that made an empty body count as an elision and survived,
    // because the fixture it ran against had no method at all.
    if source.get(start..end).is_none_or(|b| b.trim().is_empty()) {
        return None;
    }
    Some(start..end)
}

/// The range to delete for a Storage member: the `storage` node itself, widened to swallow the
/// `class_statement` wrapper when there is one, so the member leaves no empty shell behind.
fn storage_cut(storage: tree_sitter::Node) -> std::ops::Range<usize> {
    match storage.parent() {
        Some(p) if p.kind() == "class_statement" => p.byte_range(),
        _ => storage.byte_range(),
    }
}

/// Drop a leading `/* ... */` license block, returning the remainder and whether one was removed.
///
/// ONLY a `/* */` block, and only before anything else. The InterSystems header this targets is a
/// block comment:
///
/// ```text
/// /* Copyright (c) 2026 by InterSystems Corporation. ... */
/// Class %Foo.Bar Extends %RegisteredObject
/// ```
///
/// A `///` line is NOT touched even though it also sits above `Class`, because that is the class's
/// documatic comment — the one piece of prose a model reading an unfamiliar class most needs. The
/// #430 text groups "license header" and "verbose /// comments" in one phrase; they are different
/// things and only the first is noise.
pub fn strip_license_header(source: &str) -> (String, bool) {
    let lead = source.len() - source.trim_start().len();
    let rest = &source[lead..];
    if !rest.starts_with("/*") {
        return (source.to_string(), false);
    }
    match rest.find("*/") {
        Some(close) => {
            let after = &rest[close + 2..];
            // Keep the newline structure of what follows: only the comment and the whitespace
            // directly after it go, so the class declaration still starts a line.
            (
                after
                    .trim_start_matches([' ', '\t', '\r', '\n'])
                    .to_string(),
                true,
            )
        }
        // An unterminated `/*` means the whole document is one comment, or the source is truncated.
        // Removing to end-of-file would return nothing at all, so leave it alone and say so.
        None => (source.to_string(), false),
    }
}

/// Whether `content` is (or contains) a skeleton, and must therefore never be written to IRIS.
///
/// Reads [`SKELETON_MARKER`] rather than naming the text again, so the guard cannot drift from what
/// [`skeleton`] produces. It deliberately fails CLOSED: a class whose own comments quote the marker
/// is refused too. That trade is right for a write — the cost of a false refusal is one re-read with
/// an explicit shape, and the cost of a false accept is every method body in the class.
pub fn looks_like_a_skeleton(content: &str) -> bool {
    content.contains(SKELETON_MARKER)
}
