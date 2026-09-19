//! Local filesystem symbol extraction using tree-sitter-objectscript grammars.
//! No IRIS connection required.

use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Symbol {
    #[serde(rename = "Name")]
    pub name: String,
    pub kind: String,
    pub file: String,
    #[serde(rename = "FormalSpec", skip_serializing_if = "Option::is_none")]
    pub formal_spec: Option<String>,
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    /// #24/070: 1-BASED line of the declaration, so a caller can jump straight to it.
    ///
    /// tree-sitter rows are 0-based; every site adds 1 exactly once. `None` only where the grammar
    /// gave no node to take a position from — never 0, which would be a plausible-looking line
    /// number that no editor can use.
    #[serde(rename = "line", skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParseWarning {
    #[serde(rename = "type")]
    pub warning_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug)]
pub struct SymbolsLocalResult {
    pub symbols: Vec<Symbol>,
    pub parse_warnings: Vec<ParseWarning>,
}

// ── Glob matching ────────────────────────────────────────────────────────────

/// Returns true if `name` matches the glob `query`.
/// `*` is the only wildcard; matching is case-sensitive.
/// An empty query never matches.
pub fn glob_match(query: &str, name: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    // No wildcards → exact match.
    if !query.contains('*') {
        return query == name;
    }
    let parts: Vec<&str> = query.split('*').collect();
    let mut pos = 0usize;
    let name_bytes = name.as_bytes();
    let name_len = name_bytes.len();

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let part_bytes = part.as_bytes();
        if i == 0 {
            // First segment must be a prefix.
            if !name[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == parts.len() - 1 {
            // Last segment must be a suffix.
            if name_len < part.len() || !name[name_len - part.len()..].eq(*part) {
                return false;
            }
            // Ensure suffix doesn't overlap with current position.
            if name_len - part.len() < pos {
                return false;
            }
        } else {
            // Middle segment: find the next occurrence at or after pos.
            let found = name[pos..].find(part);
            match found {
                Some(offset) => pos += offset + part_bytes.len(),
                None => return false,
            }
        }
    }
    true
}

// ── UDL (.cls) extraction ────────────────────────────────────────────────────

pub fn extract_cls_symbols(
    source: &[u8],
    rel_path: &str,
    query: &str,
) -> (Vec<Symbol>, Vec<ParseWarning>) {
    let mut symbols = Vec::new();
    let mut warnings = Vec::new();

    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_objectscript::LANGUAGE_OBJECTSCRIPT_UDL.into())
        .is_err()
    {
        warnings.push(ParseWarning {
            warning_type: "PARSE_ERROR".into(),
            file: Some(rel_path.into()),
            class: None,
            files: None,
            message: Some("failed to set tree-sitter language".into()),
        });
        return (symbols, warnings);
    }

    let tree = parser.parse(source, None);
    let tree = match tree {
        Some(t) => t,
        None => {
            warnings.push(ParseWarning {
                warning_type: "PARSE_ERROR".into(),
                file: Some(rel_path.into()),
                class: None,
                files: None,
                message: Some("tree-sitter parse returned None".into()),
            });
            return (symbols, warnings);
        }
    };

    if tree.root_node().has_error() {
        warnings.push(ParseWarning {
            warning_type: "PARSE_ERROR".into(),
            file: Some(rel_path.into()),
            class: None,
            files: None,
            message: Some("syntax error in file".into()),
        });
        // Continue — extract what we can from the partial parse.
    }

    let (class_name, class_line) = match extract_class_name(&tree, source) {
        Some(n) => n,
        None => return (symbols, warnings),
    };

    if !glob_match(query, &class_name) {
        return (symbols, warnings);
    }

    // Emit class symbol.
    symbols.push(Symbol {
        name: class_name.clone(),
        kind: "class".into(),
        file: rel_path.into(),
        formal_spec: None,
        type_name: None,
        line: Some(class_line),
    });

    // Walk the tree for members.
    extract_cls_members(&tree, source, &class_name, rel_path, &mut symbols);

    (symbols, warnings)
}

/// The class name and the 1-based line it is declared on.
///
/// The line comes from the `class_name` node. MEASURED, because my first guess was wrong: I assumed
/// `class_definition` starts at the leading `///` block and would point above the `Class ...` line.
/// It does not — dumping the parse tree shows `documatic_line` is a SIBLING of `class_definition`,
/// not a child, so both nodes start on the same row and either would work today.
///
/// `class_name` is kept anyway because it is the node whose position IS the declaration by
/// definition, so it stays correct if the grammar ever attaches doc comments to the wrapper. But it
/// is a robustness choice, not a fix for a real off-by-N — stated that way so the next reader does
/// not believe a mechanism that is not there.
fn extract_class_name(tree: &tree_sitter::Tree, source: &[u8]) -> Option<(String, usize)> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    // Find class_definition
    for child in root.children(&mut cursor) {
        if child.kind() == "class_definition" {
            let mut c2 = child.walk();
            for sub in child.children(&mut c2) {
                if sub.kind() == "class_name" {
                    return Some((node_text(sub, source), sub.start_position().row + 1));
                }
            }
        }
    }
    None
}

fn extract_cls_members(
    tree: &tree_sitter::Tree,
    source: &[u8],
    class_name: &str,
    rel_path: &str,
    symbols: &mut Vec<Symbol>,
) {
    let root = tree.root_node();
    let mut cursor = root.walk();
    for top in root.children(&mut cursor) {
        if top.kind() != "class_definition" {
            continue;
        }
        let mut c2 = top.walk();
        for body_node in top.children(&mut c2) {
            if body_node.kind() != "class_body" {
                continue;
            }
            let mut c3 = body_node.walk();
            for stmt in body_node.children(&mut c3) {
                // Members are wrapped in class_statement nodes
                let member = if stmt.kind() == "class_statement" {
                    // get the actual member node (first named child)
                    stmt.named_child(0)
                } else {
                    Some(stmt)
                };
                let member = match member {
                    Some(m) => m,
                    None => continue,
                };
                // #24/070: the line is stamped ONCE, below, from the member node every arm already
                // has — not inside the four extractors. Four places to remember is four places to
                // forget, and a new member kind would arrive with no line and no test failure.
                let extracted = match member.kind() {
                    "method" | "classmethod" => {
                        extract_method_symbol(member, source, class_name, rel_path)
                    }
                    "property" => extract_property_symbol(member, source, class_name, rel_path),
                    "parameter" => extract_parameter_symbol(member, source, class_name, rel_path),
                    // #24/070: BPL and DTL are stored AS XData, so a reader that drops xdata
                    // cannot see the two component types the iris-interop skills teach most.
                    // Measured before writing this: the UDL grammar does emit an `xdata` node,
                    // wrapped in `class_statement` exactly like the members above.
                    "xdata" => extract_xdata_symbol(member, source, class_name, rel_path),
                    _ => None,
                };
                if let Some(mut sym) = extracted {
                    // The MEMBER node rather than the `class_statement` wrapper. NOT because the
                    // wrapper includes the doc comment — measured, it does not: `documatic_line` is
                    // a sibling inside `class_body`, so `stmt` and `member` start on the same row
                    // and a mutation swapping them is EQUIVALENT, not a caught bug. The member node
                    // is used because its position is the declaration by definition.
                    sym.line = Some(member.start_position().row + 1);
                    symbols.push(sym);
                }
            }
        }
    }
}

/// #24/070: an `XData` block, which is how BPL and DTL are actually stored.
///
/// `Type` carries the XMLNamespace when the block declares one, because that is what
/// distinguishes a DTL from a BPL from an arbitrary XData payload —
/// `http://www.intersystems.com/dtl` vs `.../bpl`. The NAME alone does not: a class is free to
/// call its DTL block anything, and plenty of non-interop XData is named `DTL` by coincidence.
///
/// Node shape, dumped from the grammar rather than guessed:
/// ```text
/// xdata
///   keyword_xdata   [XData]
///   xdata_name      [DTL] -> identifier
///   xdata_keyword   [XMLNamespace = "..."] -> string_literal
///   external_method_body_content
/// ```
/// A block with no `[ ... ]` keywords has no `xdata_keyword` child at all, so `Type` is None
/// rather than an empty string — absent and empty are different answers.
fn extract_xdata_symbol(
    node: tree_sitter::Node,
    source: &[u8],
    class_name: &str,
    rel_path: &str,
) -> Option<Symbol> {
    let mut cursor = node.walk();
    let mut name: Option<String> = None;
    let mut xml_namespace: Option<String> = None;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "xdata_name" => {
                name = child
                    .utf8_text(source)
                    .ok()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string);
            }
            "xdata_keyword" => {
                // The name check is load-bearing, and measured: `SchemaSpec` parses to the SAME
                // node kind `xdata_keyword` with the same `string_literal` child, so without it a
                // SchemaSpec would be reported as the XMLNamespace. (`MimeType` gets its own kind,
                // `xdata_keyword_mimetype` with a `typename` child, so it cannot reach here —
                // which is why a MimeType-only test does NOT exercise this branch. A mutation
                // that dropped this check survived such a test; `SchemaSpec` is what kills it.)
                //
                // Only XMLNamespace is reported: it is the one that identifies the payload, and a
                // field per keyword would grow this for no interop gain.
                let text = child.utf8_text(source).unwrap_or("");
                if text.to_ascii_lowercase().contains("xmlnamespace") {
                    let mut kc = child.walk();
                    for k in child.children(&mut kc) {
                        if k.kind() == "string_literal" {
                            xml_namespace = k
                                .utf8_text(source)
                                .ok()
                                .map(|t| t.trim().trim_matches('"').to_string())
                                .filter(|t| !t.is_empty());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let name = name?;
    Some(Symbol {
        name: format!("{class_name}.{name}"),
        kind: "xdata".to_string(),
        file: rel_path.to_string(),
        formal_spec: None,
        type_name: xml_namespace,
        // Filled in centrally by extract_cls_members from the member node — see
        // the comment there. Left None here so there is ONE source of truth.
        line: None,
    })
}

fn extract_method_symbol(
    node: tree_sitter::Node,
    source: &[u8],
    class_name: &str,
    rel_path: &str,
) -> Option<Symbol> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "method_definition" {
            let mut c2 = child.walk();
            let mut method_name = None;
            let mut formal_spec = None;
            for sub in child.children(&mut c2) {
                if sub.kind() == "method_name" {
                    let n = first_identifier_text(sub, source);
                    if !n.is_empty() {
                        method_name = Some(n);
                    }
                } else if sub.kind() == "arguments" {
                    // Slice the byte range and strip surrounding parens.
                    let raw = node_text(sub, source);
                    let trimmed = raw.trim();
                    let inner = if trimmed.starts_with('(') && trimmed.ends_with(')') {
                        trimmed[1..trimmed.len() - 1].trim().to_string()
                    } else {
                        trimmed.to_string()
                    };
                    formal_spec = Some(inner);
                }
            }
            if let Some(name) = method_name {
                return Some(Symbol {
                    name: format!("{}.{}", class_name, name),
                    kind: "method".into(),
                    file: rel_path.into(),
                    formal_spec,
                    type_name: None,
                    // Filled in centrally by extract_cls_members from the member node — see
                    // the comment there. Left None here so there is ONE source of truth.
                    line: None,
                });
            }
        }
    }
    None
}

fn extract_property_symbol(
    node: tree_sitter::Node,
    source: &[u8],
    class_name: &str,
    rel_path: &str,
) -> Option<Symbol> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "property_name" {
            // property_name contains an identifier child
            let name = first_identifier_text(child, source);
            if !name.is_empty() {
                return Some(Symbol {
                    name: format!("{}.{}", class_name, name),
                    kind: "property".into(),
                    file: rel_path.into(),
                    formal_spec: None,
                    type_name: None,
                    // Filled in centrally by extract_cls_members from the member node — see
                    // the comment there. Left None here so there is ONE source of truth.
                    line: None,
                });
            }
        }
    }
    None
}

fn extract_parameter_symbol(
    node: tree_sitter::Node,
    source: &[u8],
    class_name: &str,
    rel_path: &str,
) -> Option<Symbol> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "parameter_name" {
            let name = first_identifier_text(child, source);
            if !name.is_empty() {
                return Some(Symbol {
                    name: format!("{}.{}", class_name, name),
                    kind: "parameter".into(),
                    file: rel_path.into(),
                    formal_spec: None,
                    type_name: None,
                    // Filled in centrally by extract_cls_members from the member node — see
                    // the comment there. Left None here so there is ONE source of truth.
                    line: None,
                });
            }
        }
    }
    None
}

/// Returns the text of the first identifier-like leaf under a node.
fn first_identifier_text(node: tree_sitter::Node, source: &[u8]) -> String {
    if node.child_count() == 0 {
        return node_text(node, source);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "dotted_name" {
            return node_text(child, source);
        }
    }
    // fallback: return full node text
    node_text(node, source)
}

// ── Routine (.mac/.inc) extraction ──────────────────────────────────────────

pub fn extract_routine_symbols(
    source: &[u8],
    rel_path: &str,
    query: &str,
) -> (Vec<Symbol>, Vec<ParseWarning>) {
    let mut symbols = Vec::new();
    let mut warnings = Vec::new();

    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_objectscript_routine::LANGUAGE_OBJECTSCRIPT_ROUTINE.into())
        .is_err()
    {
        warnings.push(ParseWarning {
            warning_type: "PARSE_ERROR".into(),
            file: Some(rel_path.into()),
            class: None,
            files: None,
            message: Some("failed to set routine language".into()),
        });
        return (symbols, warnings);
    }

    let tree = parser.parse(source, None);
    let tree = match tree {
        Some(t) => t,
        None => {
            warnings.push(ParseWarning {
                warning_type: "PARSE_ERROR".into(),
                file: Some(rel_path.into()),
                class: None,
                files: None,
                message: Some("parse returned None".into()),
            });
            return (symbols, warnings);
        }
    };

    if tree.root_node().has_error() {
        warnings.push(ParseWarning {
            warning_type: "PARSE_ERROR".into(),
            file: Some(rel_path.into()),
            class: None,
            files: None,
            message: Some("syntax error in routine".into()),
        });
    }

    // Extract routine name from the file path (stem of filename).
    let routine_name = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    if !glob_match(query, &routine_name) {
        return (symbols, warnings);
    }

    let root = tree.root_node();
    extract_routine_nodes(root, source, &routine_name, rel_path, &mut symbols);

    (symbols, warnings)
}

/// Walk routine source_file recursively to find tag_statement and pound_define nodes.
fn extract_routine_nodes(
    node: tree_sitter::Node,
    source: &[u8],
    routine_name: &str,
    rel_path: &str,
    symbols: &mut Vec<Symbol>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "tag_statement" => {
                // tag_statement can directly contain a tag or tag_with_params
                let mut c2 = child.walk();
                for sub in child.children(&mut c2) {
                    if sub.kind() == "tag" || sub.kind() == "tag_with_params" {
                        let tag_name = extract_tag_name(sub, source);
                        if !tag_name.is_empty() {
                            symbols.push(Symbol {
                                name: format!("{}:{}", routine_name, tag_name),
                                kind: "label".into(),
                                file: rel_path.into(),
                                formal_spec: None,
                                type_name: None,
                                // #24/070: the tag / macro_def node is what a reader wants to
                                // land on, so take the position from `sub`, not the statement.
                                line: Some(sub.start_position().row + 1),
                            });
                        }
                        break;
                    }
                }
            }
            "pound_define" => {
                let mut c2 = child.walk();
                for sub in child.children(&mut c2) {
                    if sub.kind() == "macro_def" {
                        let macro_name = node_text(sub, source);
                        if !macro_name.is_empty() {
                            symbols.push(Symbol {
                                name: macro_name,
                                kind: "macro".into(),
                                file: rel_path.into(),
                                formal_spec: None,
                                type_name: None,
                                // #24/070: the tag / macro_def node is what a reader wants to
                                // land on, so take the position from `sub`, not the statement.
                                line: Some(sub.start_position().row + 1),
                            });
                        }
                        break;
                    }
                }
            }
            // tag_with_params can appear directly as a statement child
            "tag_with_params" => {
                let mut c2 = child.walk();
                for sub in child.children(&mut c2) {
                    if sub.kind() == "tag" {
                        let tag_name = extract_tag_name(sub, source);
                        if !tag_name.is_empty() {
                            symbols.push(Symbol {
                                name: format!("{}:{}", routine_name, tag_name),
                                kind: "label".into(),
                                file: rel_path.into(),
                                formal_spec: None,
                                type_name: None,
                                // #24/070: the tag / macro_def node is what a reader wants to
                                // land on, so take the position from `sub`, not the statement.
                                line: Some(sub.start_position().row + 1),
                            });
                        }
                        break;
                    }
                }
            }
            // Recurse into statement wrappers
            "statement" | "source_file" => {
                extract_routine_nodes(child, source, routine_name, rel_path, symbols);
            }
            _ => {}
        }
    }
}

fn extract_tag_name(node: tree_sitter::Node, source: &[u8]) -> String {
    // The tag node itself may be the identifier, or it may contain one.
    let text = node_text(node, source);
    // Strip trailing colon or params if present.
    let clean = text.split('(').next().unwrap_or(&text).trim();
    let clean = clean.trim_end_matches(':').trim();
    clean.to_string()
}

// ── Workspace scan ───────────────────────────────────────────────────────────

pub fn scan_workspace(workspace: &Path, query: &str, limit: usize) -> SymbolsLocalResult {
    let mut symbols = Vec::new();
    let mut warnings = Vec::new();
    // class_name → list of file paths that define it (for duplicate detection)
    let mut class_files: HashMap<String, Vec<String>> = HashMap::new();

    scan_dir(
        workspace,
        workspace,
        query,
        limit,
        &mut symbols,
        &mut warnings,
        &mut class_files,
    );

    // Emit DUPLICATE_CLASS warnings.
    for (class_name, paths) in &class_files {
        if paths.len() > 1 {
            warnings.push(ParseWarning {
                warning_type: "DUPLICATE_CLASS".into(),
                file: None,
                class: Some(class_name.clone()),
                files: Some(paths.clone()),
                message: None,
            });
        }
    }

    SymbolsLocalResult {
        symbols,
        parse_warnings: warnings,
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_dir(
    workspace: &Path,
    dir: &Path,
    query: &str,
    limit: usize,
    symbols: &mut Vec<Symbol>,
    warnings: &mut Vec<ParseWarning>,
    class_files: &mut HashMap<String, Vec<String>>,
) {
    if symbols.len() >= limit {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut paths: Vec<std::path::PathBuf> =
        entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort(); // alphabetical order for determinism

    for path in paths {
        if symbols.len() >= limit {
            return;
        }

        if path.is_symlink() {
            continue; // no symlink follow
        }

        if path.is_dir() {
            scan_dir(
                workspace,
                &path,
                query,
                limit,
                symbols,
                warnings,
                class_files,
            );
            continue;
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        if ext != "cls" && ext != "mac" && ext != "inc" {
            continue; // skip .int and everything else
        }

        let rel_path = path
            .strip_prefix(workspace)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let source = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                warnings.push(ParseWarning {
                    warning_type: "PARSE_ERROR".into(),
                    file: Some(rel_path),
                    class: None,
                    files: None,
                    message: Some("failed to read file".into()),
                });
                continue;
            }
        };

        // Check UTF-8 validity.
        if std::str::from_utf8(&source).is_err() {
            warnings.push(ParseWarning {
                warning_type: "ENCODING_ERROR".into(),
                file: Some(rel_path),
                class: None,
                files: None,
                message: Some("file is not valid UTF-8".into()),
            });
            continue;
        }

        if ext == "cls" {
            let (mut file_syms, mut file_warns) = extract_cls_symbols(&source, &rel_path, query);

            // Track class names for duplicate detection.
            for sym in &file_syms {
                if sym.kind == "class" {
                    class_files
                        .entry(sym.name.clone())
                        .or_default()
                        .push(rel_path.clone());
                }
            }

            // Respect limit.
            let remaining = limit.saturating_sub(symbols.len());
            file_syms.truncate(remaining);
            symbols.append(&mut file_syms);
            warnings.append(&mut file_warns);
        } else {
            // .mac or .inc
            let (mut file_syms, mut file_warns) =
                extract_routine_symbols(&source, &rel_path, query);
            let remaining = limit.saturating_sub(symbols.len());
            file_syms.truncate(remaining);
            symbols.append(&mut file_syms);
            warnings.append(&mut file_warns);
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn node_text(node: tree_sitter::Node, source: &[u8]) -> String {
    let start = node.start_byte();
    let end = node.end_byte();
    if end <= source.len() {
        String::from_utf8_lossy(&source[start..end]).to_string()
    } else {
        String::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("MyApp.Foo", "MyApp.Foo"));
    }

    #[test]
    fn glob_no_match_without_wildcard() {
        assert!(!glob_match("Foo", "MyApp.Foo"));
    }

    #[test]
    fn glob_package_wildcard() {
        assert!(glob_match("MyApp.*", "MyApp.Foo"));
        assert!(glob_match("MyApp.*", "MyApp.Bar"));
        assert!(!glob_match("MyApp.*", "OtherApp.Foo"));
    }

    #[test]
    fn glob_suffix_wildcard() {
        assert!(glob_match("*Service", "OrderService"));
        assert!(!glob_match("*Service", "OrderUtil"));
    }

    #[test]
    fn glob_mid_wildcard() {
        assert!(glob_match("MyApp.*.Base", "MyApp.Sub.Base"));
        assert!(!glob_match("MyApp.*.Base", "MyApp.Sub.Other"));
    }

    #[test]
    fn glob_empty_query_never_matches() {
        assert!(!glob_match("", "anything"));
        assert!(!glob_match("", ""));
    }

    // ── #24/070: XData, which is how BPL and DTL are stored ──────────────────────

    const DTL_AND_BPL: &str = r#"Class Demo.DT.Map Extends Ens.DataTransformDTL
{

Parameter IGNOREMISSINGSOURCE = 1;

XData DTL [ XMLNamespace = "http://www.intersystems.com/dtl" ]
{
<transform sourceClass='EnsLib.HL7.Message' targetClass='Demo.MSG.Out'>
<assign value='source.GetValueAt("PID:5.1")' property='target.Name' action='set'/>
</transform>
}

XData BPL [ XMLNamespace = "http://www.intersystems.com/bpl" ]
{
<process language='objectscript'>
<sequence><call name='Op' target='Demo.BO.Out' async='0'/></sequence>
</process>
}

Method Run() As %Status
{
    Quit $$$OK
}

}
"#;

    fn xdata_of(src: &str) -> Vec<Symbol> {
        let (syms, warns) = extract_cls_symbols(src.as_bytes(), "src/Demo/DT/Map.cls", "*");
        assert!(warns.is_empty(), "unexpected parse warnings: {warns:?}");
        syms.into_iter().filter(|s| s.kind == "xdata").collect()
    }

    /// Before this, the dispatch dropped `xdata` through its `_ => {}` arm, so the two component
    /// types the iris-interop skills teach most were invisible to symbols_local.
    #[test]
    fn xdata_blocks_are_reported_with_their_namespace() {
        let x = xdata_of(DTL_AND_BPL);
        assert_eq!(x.len(), 2, "expected both XData blocks: {x:?}");

        assert_eq!(x[0].name, "Demo.DT.Map.DTL");
        assert_eq!(x[0].kind, "xdata");
        assert_eq!(x[0].file, "src/Demo/DT/Map.cls");
        assert_eq!(
            x[0].type_name.as_deref(),
            Some("http://www.intersystems.com/dtl"),
            "the namespace is what distinguishes a DTL from a BPL, not the block name: {:?}",
            x[0]
        );

        assert_eq!(x[1].name, "Demo.DT.Map.BPL");
        assert_eq!(
            x[1].type_name.as_deref(),
            Some("http://www.intersystems.com/bpl")
        );

        // The quotes must be stripped, not carried into the value.
        assert!(
            !x[0].type_name.as_deref().unwrap_or("").contains('"'),
            "{:?}",
            x[0]
        );
    }

    /// POSITIVE CONTROL for the test above: the other members must still be found. A change that
    /// broke the dispatch while adding xdata would otherwise pass the xdata assertions alone.
    #[test]
    fn adding_xdata_did_not_displace_the_other_members() {
        let (syms, _) = extract_cls_symbols(DTL_AND_BPL.as_bytes(), "src/Demo/DT/Map.cls", "*");
        let kinds: std::collections::BTreeSet<&str> =
            syms.iter().map(|s| s.kind.as_str()).collect();
        assert!(kinds.contains("method"), "methods lost: {kinds:?}");
        assert!(kinds.contains("parameter"), "parameters lost: {kinds:?}");
        assert!(kinds.contains("xdata"), "xdata missing: {kinds:?}");
        assert!(
            syms.iter().any(|s| s.name == "Demo.DT.Map.Run"),
            "the method is gone: {syms:?}"
        );
    }

    /// An XData block with no `[ ... ]` keywords has no namespace. `Type` must be ABSENT, not an
    /// empty string — absent and empty are different answers, and an empty string would read as
    /// "declared, and blank".
    #[test]
    fn xdata_without_a_namespace_reports_no_type() {
        let src = r#"Class Demo.Plain Extends %RegisteredObject
{

XData Config
{
<settings><item name="x">1</item></settings>
}

}
"#;
        let x = xdata_of(src);
        assert_eq!(x.len(), 1, "{x:?}");
        assert_eq!(x[0].name, "Demo.Plain.Config");
        assert_eq!(
            x[0].type_name, None,
            "no namespace means absent: {:?}",
            x[0]
        );
    }

    /// A keyword that is NOT XMLNamespace must not be mistaken for one.
    #[test]
    fn a_non_namespace_keyword_is_not_reported_as_the_namespace() {
        let src = r#"Class Demo.Mime Extends %RegisteredObject
{

XData Payload [ MimeType = "application/json" ]
{
{"a":1}
}

}
"#;
        let x = xdata_of(src);
        assert_eq!(x.len(), 1, "{x:?}");
        assert_eq!(x[0].name, "Demo.Mime.Payload");
        assert_eq!(
            x[0].type_name, None,
            "MimeType is not an XMLNamespace: {:?}",
            x[0]
        );
    }

    /// The keyword that shares `xdata_keyword`'s node kind, and therefore the only one that can
    /// reach the name check. Measured: `SchemaSpec` parses to `xdata_keyword` with a
    /// `string_literal` child, exactly like `XMLNamespace`.
    ///
    /// This test exists because the first version of this suite used `MimeType` instead, and a
    /// mutation that removed the name check SURVIVED — `MimeType` gets its own node kind, so it
    /// never reaches the branch and proved nothing about it.
    #[test]
    fn schemaspec_is_not_mistaken_for_the_namespace() {
        let src = r#"Class Demo.Schema Extends %RegisteredObject
{

XData Spec [ SchemaSpec = "http://x/schema" ]
{
<xs:schema/>
}

}
"#;
        let x = xdata_of(src);
        assert_eq!(x.len(), 1, "{x:?}");
        assert_eq!(x[0].name, "Demo.Schema.Spec");
        assert_eq!(
            x[0].type_name, None,
            "SchemaSpec shares xdata_keyword's node kind and must NOT be read as the \
             XMLNamespace: {:?}",
            x[0]
        );
    }

    /// Both keywords present: the namespace is picked and the other is not, whichever order.
    #[test]
    fn the_namespace_is_picked_out_of_several_keywords() {
        for src in [
            r#"Class Demo.Both Extends %RegisteredObject
{

XData P [ XMLNamespace = "http://x/ns", MimeType = "application/json" ]
{
<x/>
}

}
"#,
            r#"Class Demo.Both Extends %RegisteredObject
{

XData P [ SchemaSpec = "http://x/schema", XMLNamespace = "http://x/ns" ]
{
<x/>
}

}
"#,
        ] {
            let x = xdata_of(src);
            assert_eq!(x.len(), 1, "{x:?}");
            assert_eq!(
                x[0].type_name.as_deref(),
                Some("http://x/ns"),
                "the XMLNamespace must win over the sibling keyword: {:?}",
                x[0]
            );
        }
    }

    /// The glob still applies: xdata is filtered by the CLASS name like every other member, so a
    /// query that excludes the class must not leak its XData.
    #[test]
    fn xdata_respects_the_class_glob() {
        let (syms, _) =
            extract_cls_symbols(DTL_AND_BPL.as_bytes(), "src/Demo/DT/Map.cls", "Other.*");
        assert!(
            syms.is_empty(),
            "a non-matching glob must return nothing, xdata included: {syms:?}"
        );
        // Control: the matching glob does return it, so the emptiness above means filtering
        // rather than a parser that found nothing.
        let (syms2, _) =
            extract_cls_symbols(DTL_AND_BPL.as_bytes(), "src/Demo/DT/Map.cls", "Demo.*");
        assert!(
            syms2.iter().any(|s| s.kind == "xdata"),
            "the control must find xdata: {syms2:?}"
        );
    }
}

#[cfg(test)]
mod line_number_tests {
    //! #24/070: every symbol carries the 1-based line of its declaration, so a caller can jump to
    //! it instead of grepping the file it was just told the name of.
    //!
    //! The whole risk here is an off-by-one, so these assert EXACT lines against a source whose
    //! layout is written out line by line in the comment beside it. A test that only checked
    //! `line.is_some()` would pass with every number wrong.
    use super::*;

    /// Line numbers are in the comment, counted from 1. The leading newline after `r#"` is
    /// deliberate: it makes line 1 blank, so an off-by-one cannot hide behind "the first line".
    ///
    /// ```text
    ///  1 (blank)
    ///  2 /// A class with doc comments, to prove the line is the DECLARATION.
    ///  3 Class Demo.Line.Probe Extends %RegisteredObject
    ///  4 {
    ///  5 (blank)
    ///  6 /// doc for the parameter
    ///  7 Parameter VERSION = 3;
    ///  8 (blank)
    ///  9 Property Name As %String;
    /// 10 (blank)
    /// 11 /// doc line one
    /// 12 /// doc line two
    /// 13 Method Run() As %Status
    /// 14 {
    /// 15     Quit $$$OK
    /// 16 }
    /// 17 (blank)
    /// 18 XData Conf [ XMLNamespace = "http://example.com/x" ]
    /// 19 {
    /// 20 <x/>
    /// 21 }
    /// 22 (blank)
    /// 23 }
    /// ```
    const PROBE: &str = r#"
/// A class with doc comments, to prove the line is the DECLARATION.
Class Demo.Line.Probe Extends %RegisteredObject
{

/// doc for the parameter
Parameter VERSION = 3;

Property Name As %String;

/// doc line one
/// doc line two
Method Run() As %Status
{
    Quit $$$OK
}

XData Conf [ XMLNamespace = "http://example.com/x" ]
{
<x/>
}

}
"#;

    fn probe() -> Vec<Symbol> {
        let (syms, warns) = extract_cls_symbols(PROBE.as_bytes(), "src/Demo/Line/Probe.cls", "*");
        assert!(warns.is_empty(), "unexpected parse warnings: {warns:?}");
        syms
    }

    fn line_of(syms: &[Symbol], name: &str) -> usize {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol named {name} in {syms:?}"))
            .line
            .unwrap_or_else(|| panic!("{name} carries no line: {syms:?}"))
    }

    /// The class line must be the `Class ...` line, NOT the `///` above it. This pins the OUTCOME,
    /// which is the contract a caller depends on. It does not pin the node choice: `documatic_line`
    /// is a sibling of `class_definition`, so taking the row from either node gives 3 today — a
    /// mutation swapping them is equivalent. What this test really guards is the 1-based offset and
    /// the fact that a doc comment above a declaration never shifts the reported line.
    #[test]
    fn the_class_line_is_the_declaration_not_its_doc_comment() {
        let s = probe();
        assert_eq!(
            line_of(&s, "Demo.Line.Probe"),
            3,
            "line 2 is the doc comment, line 3 is `Class ...`"
        );
    }

    /// Same property for members. The two-line doc comment above `Method Run()` must not shift the
    /// reported line — and note it does not shift it for EITHER node choice, since the comments are
    /// sibling `documatic_line` nodes rather than part of the wrapper. The value here is the offset
    /// and the outcome, not a claim about which node was necessary.
    #[test]
    fn a_member_line_is_the_declaration_not_its_doc_comment() {
        let s = probe();
        assert_eq!(
            line_of(&s, "Demo.Line.Probe.Run"),
            13,
            "lines 11-12 are doc comments; the Method is on 13"
        );
        assert_eq!(
            line_of(&s, "Demo.Line.Probe.VERSION"),
            7,
            "line 6 is the doc comment; the Parameter is on 7"
        );
    }

    #[test]
    fn every_member_kind_carries_its_own_line() {
        let s = probe();
        assert_eq!(line_of(&s, "Demo.Line.Probe.Name"), 9, "property");
        assert_eq!(line_of(&s, "Demo.Line.Probe.Conf"), 18, "xdata");
    }

    /// 1-BASED, and the guard against the classic off-by-one: nothing may report 0, and nothing may
    /// report a line past the end of the file.
    #[test]
    fn no_line_is_zero_or_past_the_end_of_the_file() {
        let s = probe();
        let last = PROBE.lines().count();
        assert!(
            last > 20,
            "precondition: the probe has enough lines: {last}"
        );
        for sym in &s {
            let l = sym.line.unwrap_or_else(|| panic!("{sym:?} has no line"));
            assert!(l >= 1, "0 is not a line an editor can use: {sym:?}");
            assert!(
                l <= last,
                "line {l} is past the file's {last} lines: {sym:?}"
            );
        }
    }

    /// EVERY symbol, not just the ones named above — a kind added later must not arrive without a
    /// line. This is what the central stamp in `extract_cls_members` buys.
    #[test]
    fn no_symbol_is_emitted_without_a_line() {
        let s = probe();
        assert!(s.len() >= 5, "precondition: class + 4 members, got {s:?}");
        let missing: Vec<_> = s.iter().filter(|x| x.line.is_none()).collect();
        assert!(missing.is_empty(), "symbols with no line: {missing:?}");
    }

    /// Routine files too — labels are how a .mac is navigated, so a label without a line is the
    /// least useful symbol of the set.
    #[test]
    fn routine_labels_and_macros_carry_their_lines() {
        // 1 ROUTINE Demo.Util
        // 2 #define MAXROWS 100
        // 3 (blank)
        // 4 Start
        // 5  quit
        let src = "ROUTINE Demo.Util\n#define MAXROWS 100\n\nStart\n quit\n";
        let (syms, _w) = extract_routine_symbols(src.as_bytes(), "src/Demo/Util.mac", "*");
        assert!(
            !syms.is_empty(),
            "precondition: the routine parser produced symbols"
        );
        // EXACT lines, not a range: `1 <= l <= 5` is satisfied by a hardcoded 1, which is exactly
        // the mutation this test exists to kill.
        let by_kind = |k: &str| -> Vec<(String, usize)> {
            syms.iter()
                .filter(|s| s.kind == k)
                .map(|s| (s.name.clone(), s.line.expect("a line")))
                .collect()
        };
        assert_eq!(
            by_kind("macro"),
            vec![("MAXROWS".to_string(), 2usize)],
            "the #define is on line 2: {syms:?}"
        );
        // `Util:Start`, not `Demo.Util:Start` — and that is PRE-EXISTING behaviour this change does
        // not touch: `routine_name` comes from `Path::file_stem()`, so the package is dropped even
        // though line 1 of the source says `ROUTINE Demo.Util`. Filed separately rather than
        // changed here, because renaming symbols is a contract change and this PR is about lines.
        // The exact assertion is what surfaced it; the `1 <= l <= 5` range it replaced hid it.
        assert_eq!(
            by_kind("label"),
            vec![("Util:Start".to_string(), 4usize)],
            "the Start label is on line 4: {syms:?}"
        );
    }
}
