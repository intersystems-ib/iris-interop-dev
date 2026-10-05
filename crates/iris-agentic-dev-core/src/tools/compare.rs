//! #351: compare two namespaces, and one document across two namespaces.
//!
//! ## Two premise corrections, both read from upstream before writing this
//!
//! **1. Upstream's tools cannot be ported as they are.** `compare_namespace_impl` takes `server_a`,
//! `server_b` and ONE `namespace`: it compares the same namespace on two *registered servers*. This
//! fork has no server registry — it connects to one instance named by `IRIS_HOST`/`IRIS_NAMESPACE`,
//! which is why `iris_servers`, `iris_add_server`, `iris_remove_server` and `iris_import_servers` are
//! all recorded as out of remit. There is nothing for `server_a` to resolve against.
//!
//! So the axis is turned: **two namespaces on the connected instance**. That covers the case the
//! issue actually names — a deployment compared against its source of truth — for anyone whose dev
//! and test namespaces live on one instance, which is the normal shape for interop development.
//!
//! What it deliberately does NOT cover is disk-versus-namespace, the drift §1.14 is about. The MCP
//! reaches IRIS over HTTP and has no access to the caller's project directory; that is precisely why
//! the source-of-truth check is a PreToolUse hook and not a tool.
//!
//! **2. Upstream answers a read failure with a fact.** Its per-document arm is:
//!
//! ```text
//! match (a, b) {
//!     (Ok(sa), Ok(sb)) => { if sa == sb { same_count += 1 } else { different.push(doc) } }
//!     _ => { different.push(doc.clone()) }          // <-- a fetch that FAILED
//! }
//! ```
//!
//! A document neither side could read is reported as a document that DIFFERS. That is the shape
//! CLAUDE.md is about: a failure answered as a negative fact, and the most misleading direction here
//! — a caller reading "these 12 classes differ" has no way to learn that 12 fetches failed. Here an
//! unreadable document is its own bucket and is never counted as a difference.
//!
//! ## An empty comparison is not a clean comparison
//!
//! #351 measured both failure modes of the hand-rolled report it replaces: an unmounted source tree
//! reported `ONLY IN IRIS: <every class>`, and a typo'd package prefix reported **`in sync`** having
//! compared nothing. The second is the one worth designing against, so [`Comparison`] has a state for
//! it: an intersection of zero is `NothingCompared`, never `in_sync: true`.
//!
//! ## What "differ" is decided on
//!
//! `%Dictionary.CompiledClass.Hash`, one SQL read per namespace, rather than upstream's two document
//! fetches per class — 2 round trips instead of 400 for a 200-class comparison. Measured on IRIS
//! 2026.1: `Hash` is a readable per-class column (`w+OkZ1r5SVs` for `%Library.String`).
//!
//! The payload says `compared_on: "Hash"` out loud, because that equality is NOT the same claim as
//! identical source and this module does not make the stronger one. The control available on a
//! single instance — the same class read from two namespaces — is satisfied by class MAPPING rather
//! than by content hashing, so it cannot establish the stronger claim either way. A caller who needs
//! source-level certainty for a named class has `compare_document`, which reads both sources.

use std::collections::BTreeMap;

/// #351 turns upstream's axis: two NAMESPACES on the connected instance, because this fork has no
/// server registry for `server_a`/`server_b` to resolve against.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CompareNamespaceParams {
    #[serde(alias = "ns_a", alias = "a", alias = "source")]
    pub namespace_a: String,
    #[serde(alias = "ns_b", alias = "b", alias = "target")]
    pub namespace_b: String,
    /// Class-name PREFIX to narrow the comparison, e.g. "Hospital". Strongly recommended: a real
    /// namespace here holds 10,116 non-system classes.
    #[serde(default)]
    pub package: Option<String>,
    /// How many classes present in both sides to compare (default 200, as upstream). Anything past
    /// it is reported in `unchecked_count`, never dropped.
    #[serde(default = "default_cap")]
    pub max_compare: usize,
}

fn default_cap() -> usize {
    200
}

/// One document, two namespaces.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CompareDocumentParams {
    /// Document name WITH its Atelier suffix, e.g. "Hospital.BO.Db.cls".
    #[serde(alias = "name", alias = "class_name")]
    pub document: String,
    #[serde(alias = "ns_a", alias = "a", alias = "source")]
    pub namespace_a: String,
    #[serde(alias = "ns_b", alias = "b", alias = "target")]
    pub namespace_b: String,
}

/// Read one namespace's class list from a `/action/query` reply.
///
/// A reply this code cannot read becomes [`SideRead::Failed`], never an empty listing: an
/// unreachable namespace and an empty one are different answers, and the whole point of the
/// `BothSidesEmpty` state is lost if a failure can arrive wearing its clothes.
pub fn side_from_query(result: anyhow::Result<serde_json::Value>) -> SideRead {
    match result {
        Err(e) => SideRead::Failed(e.to_string()),
        Ok(v) => {
            match v["result"]["content"].as_array() {
                None => SideRead::Failed(format!(
                "the query reply carried no result.content array, so no class list was read: {}",
                serde_json::to_string(&v).unwrap_or_default().chars().take(200).collect::<String>()
            )),
                Some(rows) => SideRead::Listed(
                    rows.iter()
                        .filter_map(|r| {
                            let n = r["Name"].as_str()?;
                            Some((n.to_string(), r["Hash"].as_str().unwrap_or("").to_string()))
                        })
                        .collect(),
                ),
            }
        }
    }
}

/// How many `unchecked` NAMES the payload carries. The count is always exact; this caps only the
/// list, because a real namespace holds five figures of classes.
pub const UNCHECKED_NAMES_SHOWN: usize = 50;

/// One namespace's class list: name -> `Hash`.
pub type Listing = BTreeMap<String, String>;

/// What reading one side produced. `Failed` carries why, and never degrades to an empty listing —
/// an unreachable namespace and an empty one are different answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideRead {
    Listed(Listing),
    Failed(String),
}

/// What comparing the two sides amounted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparison {
    /// One side could not be read. Names which, so the caller fixes the right thing.
    SideUnreadable { side: &'static str, why: String },
    /// Both sides read and BOTH listed nothing. A package filter that matches nothing, or two
    /// namespaces that hold no application classes, land here — and this is the state #351 measured
    /// the hand-rolled report getting wrong by reporting "in sync" having compared nothing.
    ///
    /// Note what is NOT here: one side empty and the other not. Measured against real namespaces
    /// (DEMO holds 4 `Comun.*` classes, USER holds 0), collapsing that into this state would hide
    /// the four classes missing from the other side — which is the single most useful answer a
    /// deployment check can get. That case is a `Compared` with `only_in_a` populated and `in_sync`
    /// false.
    BothSidesEmpty { count_a: usize, count_b: usize },
    /// A real comparison.
    Compared {
        only_in_a: Vec<String>,
        only_in_b: Vec<String>,
        differ: Vec<String>,
        same_count: usize,
        /// Classes present in both that were NOT compared because the cap was reached. Never
        /// silently dropped (#351 acceptance).
        unchecked: Vec<String>,
    },
}

/// The class list for one namespace.
///
/// `%` classes are excluded: they are system classes mapped from IRISLIB into every namespace, so
/// including them makes every comparison look identical for thousands of rows and buries the
/// application classes the caller asked about. `package` narrows further.
pub fn class_list_sql(package: Option<&str>) -> (String, Vec<String>) {
    // MEASURED on IRIS 2026.1 (DEMO, read-only), because the obvious spelling is wrong and a test
    // over the generated TEXT cannot see it:
    //
    //   no filter                              15875 rows
    //   Name NOT LIKE '%%%'                        0 rows   <-- three WILDCARDS: excludes everything
    //   Name NOT LIKE '\%%' ESCAPE '\'           10116 rows   <-- the 5759 %-prefixed ones excluded
    //   … AND Name LIKE 'Comun%'                    4 rows
    //
    // The first version of this function emitted the middle one and its unit test asserted the
    // clause was PRESENT, so it passed while the query it built would have listed nothing on both
    // sides — which `compare` would then have reported as NothingCompared for every call.
    let mut sql = String::from(
        r"SELECT Name, Hash FROM %Dictionary.CompiledClass WHERE Name NOT LIKE '\%%' ESCAPE '\'",
    );
    let mut params: Vec<String> = Vec::new();
    if let Some(pkg) = package.map(str::trim).filter(|p| !p.is_empty()) {
        sql.push_str(" AND Name LIKE ?");
        params.push(format!("{pkg}%"));
    }
    sql.push_str(" ORDER BY Name");
    (sql, params)
}

/// Compare two sides, capping the number of common classes examined.
pub fn compare(a: &SideRead, b: &SideRead, cap: usize) -> Comparison {
    let (la, lb) = match (a, b) {
        (SideRead::Failed(why), _) => {
            return Comparison::SideUnreadable {
                side: "a",
                why: why.clone(),
            }
        }
        (_, SideRead::Failed(why)) => {
            return Comparison::SideUnreadable {
                side: "b",
                why: why.clone(),
            }
        }
        (SideRead::Listed(la), SideRead::Listed(lb)) => (la, lb),
    };
    let only_in_a: Vec<String> = la
        .keys()
        .filter(|k| !lb.contains_key(*k))
        .cloned()
        .collect();
    let only_in_b: Vec<String> = lb
        .keys()
        .filter(|k| !la.contains_key(*k))
        .cloned()
        .collect();
    if la.is_empty() && lb.is_empty() {
        return Comparison::BothSidesEmpty {
            count_a: 0,
            count_b: 0,
        };
    }
    let common: Vec<&String> = la.keys().filter(|k| lb.contains_key(*k)).collect();
    let take = common.len().min(cap);
    let mut differ = Vec::new();
    let mut same_count = 0usize;
    for name in &common[..take] {
        if la.get(*name) == lb.get(*name) {
            same_count += 1;
        } else {
            differ.push((*name).clone());
        }
    }
    let unchecked: Vec<String> = common[take..].iter().map(|s| (*s).clone()).collect();
    Comparison::Compared {
        only_in_a,
        only_in_b,
        differ,
        same_count,
        unchecked,
    }
}

/// The payload. `in_sync` is asserted only when a real comparison found no difference AND nothing
/// went unchecked — the two ways "no differences reported" can be untrue.
pub fn payload(ns_a: &str, ns_b: &str, c: &Comparison) -> serde_json::Value {
    match c {
        Comparison::SideUnreadable { side, why } => {
            let (which, name) = if *side == "a" {
                ("a", ns_a)
            } else {
                ("b", ns_b)
            };
            serde_json::json!({
                "error_code": "COMPARE_SIDE_UNREADABLE",
                "error": format!(
                    "namespace_{which} ('{name}') could not be listed, so nothing was compared — \
                     this is NOT a report that the two namespaces match. Check the namespace name \
                     and that this user can read it. IRIS said: {why}"
                ),
                "namespace_a": ns_a, "namespace_b": ns_b, "unreadable_side": which,
            })
        }
        Comparison::BothSidesEmpty { count_a, count_b } => serde_json::json!({
            "error_code": "COMPARE_BOTH_SIDES_EMPTY",
            "error": format!(
                "both {ns_a} and {ns_b} listed NO application classes, so nothing was compared — \
                 this is NOT 'in sync'. A `package` filter matching nothing lands here (check the \
                 spelling and that it is a prefix, not a pattern), as does a pair of namespaces \
                 that hold no application classes at all."
            ),
            "namespace_a": ns_a, "namespace_b": ns_b,
            "count_a": count_a, "count_b": count_b,
        }),
        Comparison::Compared {
            only_in_a,
            only_in_b,
            differ,
            same_count,
            unchecked,
        } => serde_json::json!({
            "success": true,
            "namespace_a": ns_a,
            "namespace_b": ns_b,
            "compared_on": "Hash",
            "only_in_a": only_in_a,
            "only_in_b": only_in_b,
            "differ": differ,
            "same_count": same_count,
            // How many classes existed on BOTH sides. Zero means no pairwise comparison happened at
            // all, even though names were listed — the distinction the hand-rolled report lacked.
            "common_count": same_count + differ.len() + unchecked.len(),
            // The COUNT is always exact — that is #351's acceptance item. The NAME list is capped,
            // because a namespace here really holds 10,116 non-system classes and shipping all of
            // them would make the payload the problem. Capping it is stated, not silent.
            "unchecked_count": unchecked.len(),
            "unchecked": unchecked.iter().take(UNCHECKED_NAMES_SHOWN).collect::<Vec<_>>(),
            "unchecked_names_truncated": unchecked.len() > UNCHECKED_NAMES_SHOWN,
            // Not merely "differ is empty": anything left unchecked, or present on one side only,
            // means the namespaces are not known to match.
            "in_sync": differ.is_empty()
                && unchecked.is_empty()
                && only_in_a.is_empty()
                && only_in_b.is_empty(),
        }),
    }
}

// ── compare_document ──────────────────────────────────────────────────────────────────────

/// One document read from both namespaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocVerdict {
    /// Byte-identical source.
    Same,
    /// Different source, with the line counts of each side.
    Differ { lines_a: usize, lines_b: usize },
    /// At least one side could not be read. NEVER reported as a difference — that is the upstream
    /// defect this module exists partly to avoid.
    Unreadable { side: &'static str, why: String },
}

/// Compare one document's source from both sides.
pub fn document_verdict(a: Result<&str, String>, b: Result<&str, String>) -> DocVerdict {
    match (a, b) {
        (Err(why), _) => DocVerdict::Unreadable { side: "a", why },
        (_, Err(why)) => DocVerdict::Unreadable { side: "b", why },
        (Ok(sa), Ok(sb)) => {
            if sa == sb {
                DocVerdict::Same
            } else {
                DocVerdict::Differ {
                    lines_a: sa.lines().count(),
                    lines_b: sb.lines().count(),
                }
            }
        }
    }
}

/// The payload for one document.
pub fn document_payload(name: &str, ns_a: &str, ns_b: &str, v: &DocVerdict) -> serde_json::Value {
    match v {
        DocVerdict::Unreadable { side, why } => {
            let (which, ns) = if *side == "a" {
                ("a", ns_a)
            } else {
                ("b", ns_b)
            };
            serde_json::json!({
                "error_code": "COMPARE_DOC_UNREADABLE",
                "error": format!(
                    "'{name}' could not be read from namespace_{which} ('{ns}'), so the two copies \
                     were NOT compared — this is not a report that they differ, and not a report \
                     that they match. IRIS said: {why}"
                ),
                "document": name, "namespace_a": ns_a, "namespace_b": ns_b,
                "unreadable_side": which,
            })
        }
        DocVerdict::Same => serde_json::json!({
            "success": true, "document": name,
            "namespace_a": ns_a, "namespace_b": ns_b,
            "same": true, "compared_on": "source",
        }),
        DocVerdict::Differ { lines_a, lines_b } => serde_json::json!({
            "success": true, "document": name,
            "namespace_a": ns_a, "namespace_b": ns_b,
            "same": false, "compared_on": "source",
            "lines_a": lines_a, "lines_b": lines_b,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(pairs: &[(&str, &str)]) -> SideRead {
        SideRead::Listed(
            pairs
                .iter()
                .map(|(n, h)| (n.to_string(), h.to_string()))
                .collect(),
        )
    }

    #[test]
    fn classes_only_on_one_side_are_named_on_that_side() {
        let a = listing(&[("App.BS.Feed", "h1"), ("App.BO.Db", "h2")]);
        let b = listing(&[("App.BS.Feed", "h1"), ("App.BP.Route", "h3")]);
        let Comparison::Compared {
            only_in_a,
            only_in_b,
            differ,
            same_count,
            ..
        } = compare(&a, &b, 200)
        else {
            panic!("expected Compared")
        };
        assert_eq!(only_in_a, vec!["App.BO.Db"]);
        assert_eq!(only_in_b, vec!["App.BP.Route"]);
        assert!(differ.is_empty());
        assert_eq!(same_count, 1);
    }

    #[test]
    fn a_different_hash_is_a_difference_and_an_equal_one_is_not() {
        let a = listing(&[("App.X", "h1"), ("App.Y", "same")]);
        let b = listing(&[("App.X", "h2"), ("App.Y", "same")]);
        let Comparison::Compared {
            differ, same_count, ..
        } = compare(&a, &b, 200)
        else {
            panic!("expected Compared")
        };
        assert_eq!(differ, vec!["App.X"]);
        assert_eq!(same_count, 1, "App.Y must not be counted as differing");
    }

    /// The issue's measured failure mode: a package filter matching nothing reported "in sync"
    /// having compared nothing. That must be impossible to express.
    /// The issue's measured failure mode: a filter matching nothing reported "in sync" having
    /// compared nothing. Both sides empty must be its own state.
    #[test]
    fn both_sides_empty_is_not_in_sync() {
        let c = compare(&listing(&[]), &listing(&[]), 200);
        assert_eq!(
            c,
            Comparison::BothSidesEmpty {
                count_a: 0,
                count_b: 0
            }
        );
        let v = payload("DEV", "TEST", &c);
        assert_eq!(v["error_code"], "COMPARE_BOTH_SIDES_EMPTY");
        assert!(v.get("in_sync").is_none(), "must not claim sync: {v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("NOT 'in sync'"),
            "{v}"
        );
    }

    /// The shape measured against real namespaces: DEMO holds 4 `Comun.*` classes and USER holds 0,
    /// so the intersection is empty. Collapsing that into "nothing compared" would HIDE the four
    /// classes missing from the other side, which is the answer a deployment check exists to get.
    #[test]
    fn one_side_empty_still_names_what_the_other_side_holds() {
        let c = compare(
            &listing(&[
                ("Comun.BO.Aviso", "pav1l9xjkNc"),
                ("Comun.MSG.AvisoReq", "+UAnNeUpqqk"),
                ("Comun.Produccion", "VrnI8pgCEzY"),
                ("Comun.RUL.Alertas", "ehIjfwj6aBg"),
            ]),
            &listing(&[]),
            200,
        );
        let Comparison::Compared {
            only_in_a,
            same_count,
            ..
        } = &c
        else {
            panic!("expected Compared, got {c:?} — the four missing classes must be reported")
        };
        assert_eq!(only_in_a.len(), 4, "{only_in_a:?}");
        assert_eq!(*same_count, 0);
        let v = payload("DEMO", "USER", &c);
        assert_eq!(v["in_sync"], false);
        assert_eq!(
            v["common_count"], 0,
            "no pair was compared, and the payload says so"
        );
        assert_eq!(v["only_in_a"].as_array().map(|a| a.len()), Some(4));
    }

    /// Two namespaces with disjoint application classes: informative, not "nothing compared".
    #[test]
    fn disjoint_namespaces_report_both_sides_rather_than_collapsing() {
        let c = compare(
            &listing(&[("A.One", "h")]),
            &listing(&[("B.Two", "h")]),
            200,
        );
        let v = payload("DEV", "TEST", &c);
        assert_eq!(v["only_in_a"], serde_json::json!(["A.One"]));
        assert_eq!(v["only_in_b"], serde_json::json!(["B.Two"]));
        assert_eq!(v["common_count"], 0);
        assert_eq!(v["in_sync"], false);
    }

    /// A side that could not be read names WHICH side, and is never a match report.
    #[test]
    fn an_unreadable_side_is_named_and_is_not_a_match_report() {
        let c = compare(
            &SideRead::Failed("namespace 'NOPE' does not exist".into()),
            &listing(&[("A.One", "h")]),
            200,
        );
        assert_eq!(
            c,
            Comparison::SideUnreadable {
                side: "a",
                why: "namespace 'NOPE' does not exist".into()
            }
        );
        let v = payload("NOPE", "TEST", &c);
        assert_eq!(v["error_code"], "COMPARE_SIDE_UNREADABLE");
        assert_eq!(v["unreadable_side"], "a");
        assert!(v.get("in_sync").is_none(), "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("NOT a report"),
            "{v}"
        );
        // And side b is reported when it is b that failed.
        let c2 = compare(&listing(&[]), &SideRead::Failed("boom".into()), 200);
        assert_eq!(payload("DEV", "TEST", &c2)["unreadable_side"], "b");
    }

    /// Past the cap is REPORTED, never dropped — #351's own acceptance item.
    #[test]
    fn classes_past_the_cap_are_reported_as_unchecked() {
        let pairs: Vec<(String, String)> = (0..5)
            .map(|i| (format!("App.C{i}"), "h".to_string()))
            .collect();
        let refs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let c = compare(&listing(&refs), &listing(&refs), 2);
        let Comparison::Compared {
            same_count,
            unchecked,
            ..
        } = &c
        else {
            panic!("expected Compared")
        };
        assert_eq!(*same_count, 2);
        assert_eq!(unchecked.len(), 3, "{unchecked:?}");
        let v = payload("DEV", "TEST", &c);
        assert_eq!(v["unchecked_count"], 3);
        assert_eq!(
            v["in_sync"], false,
            "nothing differed among those compared, but 3 were never looked at: {v}"
        );
    }

    /// `in_sync` is asserted only when there is nothing left to doubt — and it IS asserted then,
    /// or the assertion above would be satisfied by a constant false.
    /// #351's acceptance item, with an oracle that can actually see it. A mutation capping
    /// `unchecked_count` the way the NAME list is capped SURVIVED the cap test above, because that
    /// test leaves 3 unchecked and the name cap is 50 — `take(50).count()` and `len()` agree at 3.
    /// The count has to be exact past the cap, and the name list's truncation has to be stated.
    #[test]
    fn the_unchecked_count_is_exact_even_past_the_name_cap() {
        let n = UNCHECKED_NAMES_SHOWN + 17;
        let pairs: Vec<(String, String)> = (0..n)
            .map(|i| (format!("App.C{i:04}"), "h".to_string()))
            .collect();
        let refs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let v = payload("DEV", "TEST", &compare(&listing(&refs), &listing(&refs), 0));
        assert_eq!(
            v["unchecked_count"], n,
            "the COUNT must be exact — capping it is the silent truncation #351 forbids: {v}"
        );
        assert_eq!(
            v["unchecked"].as_array().map(|a| a.len()),
            Some(UNCHECKED_NAMES_SHOWN),
            "the NAME list is capped"
        );
        assert_eq!(
            v["unchecked_names_truncated"], true,
            "and the caller is told the list is a prefix"
        );
        assert_eq!(v["in_sync"], false);
        // CONTROL: below the cap the list is whole and the flag is false.
        let sv = payload(
            "DEV",
            "TEST",
            &compare(&listing(&refs[..3]), &listing(&refs[..3]), 0),
        );
        assert_eq!(sv["unchecked_count"], 3);
        assert_eq!(sv["unchecked"].as_array().map(|a| a.len()), Some(3));
        assert_eq!(sv["unchecked_names_truncated"], false);
    }

    #[test]
    fn in_sync_is_claimed_when_everything_matched_and_nothing_was_skipped() {
        let a = listing(&[("App.X", "h"), ("App.Y", "h2")]);
        let v = payload("DEV", "TEST", &compare(&a, &a, 200));
        assert_eq!(v["in_sync"], true);
        assert_eq!(v["unchecked_count"], 0);
        assert_eq!(v["same_count"], 2);
    }

    /// A class present on one side only also blocks `in_sync`.
    #[test]
    fn a_one_sided_class_blocks_in_sync() {
        let v = payload(
            "DEV",
            "TEST",
            &compare(
                &listing(&[("App.X", "h"), ("App.Extra", "h")]),
                &listing(&[("App.X", "h")]),
                200,
            ),
        );
        assert_eq!(v["in_sync"], false);
        assert_eq!(v["differ"].as_array().map(|a| a.len()), Some(0));
        assert_eq!(v["only_in_a"], serde_json::json!(["App.Extra"]));
    }

    #[test]
    fn the_class_list_query_excludes_system_classes_and_can_narrow_to_a_package() {
        let (sql, params) = class_list_sql(None);
        assert!(sql.contains("%Dictionary.CompiledClass"), "{sql}");
        // The ESCAPE form, not the bare one. Measured: `NOT LIKE '%%%'` is three wildcards and
        // returns 0 of 15875 rows, so it excludes every class rather than the %-prefixed ones — and
        // a `contains` assertion on the clause text passed while the query listed nothing.
        assert!(
            sql.contains(r"NOT LIKE '\%%' ESCAPE '\'"),
            "the %-exclusion must be escaped, or it matches everything: {sql}"
        );
        assert!(
            !sql.contains("NOT LIKE '%%%'"),
            "this is the unescaped form that excludes all 15875 classes: {sql}"
        );
        assert!(params.is_empty());
        let (sql2, params2) = class_list_sql(Some("App"));
        assert!(sql2.contains("AND Name LIKE ?"), "{sql2}");
        assert_eq!(params2, vec!["App%"]);
        // A blank package is not a filter.
        assert!(class_list_sql(Some("   ")).1.is_empty());
    }

    // ── compare_document ──────────────────────────────────────────────────────────────

    #[test]
    fn identical_source_is_the_same_document() {
        assert_eq!(
            document_verdict(Ok("Class A\n{\n}"), Ok("Class A\n{\n}")),
            DocVerdict::Same
        );
        let v = document_payload("A.cls", "DEV", "TEST", &DocVerdict::Same);
        assert_eq!(v["same"], true);
        assert_eq!(v["compared_on"], "source");
    }

    #[test]
    fn different_source_reports_both_line_counts() {
        let v = document_verdict(Ok("Class A\n{\n}"), Ok("Class A\n{\nMethod M() {}\n}"));
        assert_eq!(
            v,
            DocVerdict::Differ {
                lines_a: 3,
                lines_b: 4
            }
        );
        let p = document_payload("A.cls", "DEV", "TEST", &v);
        assert_eq!(p["same"], false);
        assert_eq!(p["lines_a"], 3);
        assert_eq!(p["lines_b"], 4);
    }

    /// The upstream defect, made impossible: a document that could not be read is NOT a document
    /// that differs.
    #[test]
    fn a_document_that_could_not_be_read_is_not_a_document_that_differs() {
        let v = document_verdict(Ok("Class A"), Err("HTTP 404".into()));
        assert_eq!(
            v,
            DocVerdict::Unreadable {
                side: "b",
                why: "HTTP 404".into()
            }
        );
        let p = document_payload("A.cls", "DEV", "TEST", &v);
        assert_eq!(p["error_code"], "COMPARE_DOC_UNREADABLE");
        assert!(p.get("same").is_none(), "must claim neither: {p}");
        let msg = p["error"].as_str().unwrap_or_default();
        assert!(msg.contains("not a report that they differ"), "{msg}");
        assert!(msg.contains("not a report that they match"), "{msg}");
        // Side a too.
        assert_eq!(
            document_verdict(Err("boom".into()), Ok("x")),
            DocVerdict::Unreadable {
                side: "a",
                why: "boom".into()
            }
        );
    }

    /// A query reply that cannot be read is a FAILED side, never an empty listing — otherwise a
    /// broken reply on both sides would arrive as `BothSidesEmpty`, which reads as "these namespaces
    /// hold nothing" rather than "the read did not work".
    #[test]
    fn an_unreadable_query_reply_is_a_failed_side_not_an_empty_one() {
        let r = side_from_query(Ok(serde_json::json!({"status": {"errors": ["boom"]}})));
        match r {
            SideRead::Failed(why) => assert!(!why.is_empty(), "must say why"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(matches!(
            side_from_query(Err(anyhow::anyhow!("transport down"))),
            SideRead::Failed(_)
        ));
        // CONTROL: a well-formed reply with zero rows IS an empty listing, which is a different
        // thing and must still be readable.
        assert_eq!(
            side_from_query(Ok(serde_json::json!({"result": {"content": []}}))),
            SideRead::Listed(Listing::new())
        );
    }

    /// Real rows, as `/action/query` returns them — the shape measured on DEMO.
    #[test]
    fn real_query_rows_become_a_name_to_hash_listing() {
        let r = side_from_query(Ok(serde_json::json!({"result": {"content": [
            {"Name": "Comun.BO.Aviso", "Hash": "pav1l9xjkNc"},
            {"Name": "Comun.Produccion", "Hash": "VrnI8pgCEzY"}
        ]}})));
        let SideRead::Listed(l) = r else {
            panic!("expected Listed")
        };
        assert_eq!(l.len(), 2);
        assert_eq!(
            l.get("Comun.Produccion").map(String::as_str),
            Some("VrnI8pgCEzY")
        );
    }
}
