//! #329: a bare `NOT_FOUND` is a negative fact, and the namespace holds the answer.
//!
//! `iris_doc` answered a missing document with exactly this and nothing else:
//!
//! ```text
//! Document not found: Censo.Msg.PacienteMenu.cls
//! ```
//!
//! Measured over 2,386 real error envelopes, that shape accounted for 43 responses — the largest
//! single bucket of refusals carrying no next action. And in that corpus the misses were **near
//! misses of real classes**: `Censo.Msg.*` against a `Censo.MSG.*` package that exists. A caller
//! reading "not found" concludes the class is absent; the truth was a casing difference two
//! characters wide, and the namespace could have said so.
//!
//! There is a second, more useful fact hiding in the same lookup: whether the **package** exists at
//! all. "No document in this namespace begins with `Censo.Msg.`" is a different statement from "that
//! one document is missing", and it is the one that tells a caller they are in the wrong namespace
//! or have the package name wrong.
//!
//! ## Why this does not reuse `near_miss_classes`
//!
//! `tools::mod::near_miss_classes` answers the same question for `docs_introspect`, and this is a
//! deliberate second implementation rather than a duplicate. It reads `%Dictionary.CompiledClass`
//! over SQL, so it sees only COMPILED classes and inherits the dictionary's measured stale
//! direction — a class written and compiled earlier in this same MCP process stays invisible to it
//! until a later process, which this crate documents at three separate sites.
//!
//! That blind spot is harmless for `docs_introspect`, whose subject is a compiled class's members:
//! a class the dictionary cannot see is one introspection cannot read either. It is NOT harmless
//! here. `iris_doc`'"'"'s NOT_FOUND fires on documents that may never have been compiled, including one
//! the caller wrote moments ago — and answering "nothing similar exists" for a file they just
//! created is precisely the failure this module exists to prevent. So this reads the Atelier
//! listing, which sees documents regardless of compile state.
//!
//! ## Why this is three states and not an `Option<Vec<String>>`
//!
//! The suggestion lookup is an HTTP call and can fail — a 401, a closed port, a namespace the
//! listing endpoint rejects. If that failure collapsed into "no suggestions", the response would say
//! *"Document not found, and nothing similar exists"* when the truth is *"we could not look"*. That
//! is the exact defect (#310) this issue is a continuation of, reintroduced inside its own fix. So
//! the failure is its own variant and the message says which happened.

use super::wildcard::ListingUnavailable;
use crate::iris::connection::IrisConnection;

/// What the namespace had to say about a document that was not found.
/// Not `PartialEq`: `ListingUnavailable` is not, and comparing two of these is not a thing any
/// caller needs — each arm is rendered, not matched against another instance.
#[derive(Debug, Clone)]
pub enum NearMisses {
    /// Documents in the namespace whose names are close to the one requested.
    Found(Vec<String>),
    /// The listing succeeded and the package prefix matched NOTHING. A stronger fact than the
    /// document being absent: the package is not in this namespace.
    PackageAbsent { prefix: String },
    /// The listing succeeded and matched the package, but nothing resembled the requested name.
    /// Distinct from `PackageAbsent` — the package is there, the document genuinely is not.
    NoneSimilar { prefix: String, scanned: usize },
    /// The lookup could not be performed. NEVER rendered as "nothing similar exists".
    Unavailable(ListingUnavailable),
}

/// The package prefix of a document name: everything up to and including the last `.` before the
/// final segment. `Censo.Msg.PacienteMenu.cls` -> `Censo.Msg.`
///
/// Returns `None` for a name with no package at all, where a prefix filter would be the whole
/// namespace and the listing would be both huge and useless.
pub fn package_prefix(name: &str) -> Option<String> {
    let stem = name
        .rsplit_once('.')
        .filter(|(_, ext)| {
            // Only strip a known Atelier extension; a package segment can look like one.
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "cls" | "mac" | "int" | "inc" | "bas" | "mvb" | "mvi" | "dfi"
            )
        })
        .map(|(s, _)| s)
        .unwrap_or(name);
    let (pkg, _) = stem.rsplit_once('.')?;
    if pkg.is_empty() {
        return None;
    }
    Some(format!("{pkg}."))
}

/// Case-insensitive edit distance, capped: we only care whether two names are CLOSE, and a cap
/// keeps a pathological pair cheap.
fn close_enough(a: &str, b: &str, max: usize) -> bool {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    if a == b {
        return true;
    }
    let (la, lb) = (a.chars().count(), b.chars().count());
    if la.abs_diff(lb) > max {
        return false;
    }
    // Classic DP, bounded by the cap so a far pair exits early.
    let bv: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=bv.len()).collect();
    let mut cur = vec![0usize; bv.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        let mut row_min = cur[0];
        for (j, cb) in bv.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            row_min = row_min.min(cur[j + 1]);
        }
        if row_min > max {
            return false;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[bv.len()] <= max
}

/// How many near misses to name. Enough to be useful, few enough that the message stays readable.
const MAX_SUGGESTIONS: usize = 5;

/// Edit distance within which two document names count as near misses. 3 covers the observed
/// failure — a casing difference plus a character or two — without pairing unrelated classes.
const MAX_DISTANCE: usize = 3;

/// Ask the namespace what it has near `name`.
///
/// Only ever called on a path that has ALREADY established the document is missing, so the extra
/// HTTP request never touches a successful read.
pub async fn near_misses_for(
    iris: &IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
    name: &str,
) -> NearMisses {
    let Some(prefix) = package_prefix(name) else {
        // No package to filter on. Listing the whole namespace to guess would cost more than it is
        // worth, and an unfiltered listing on a real instance is thousands of rows.
        return NearMisses::NoneSimilar {
            prefix: String::new(),
            scanned: 0,
        };
    };

    let url = iris.versioned_ns_url(namespace, &format!("/docnames/CLS?filter={prefix}"));
    let resp = client
        .get(&url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            return NearMisses::Unavailable(ListingUnavailable {
                url,
                status: None,
                detail: e.to_string(),
                filter: Some(prefix),
            })
        }
    };
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        return NearMisses::Unavailable(ListingUnavailable {
            url,
            status: Some(status),
            // Atelier answers 401 and 404 with a ZERO-BYTE body, so there is nothing to quote.
            detail: format!("HTTP {status} listing the package"),
            filter: Some(prefix),
        });
    }

    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            return NearMisses::Unavailable(ListingUnavailable {
                url,
                status: Some(status),
                detail: format!("could not read the listing body: {e}"),
                filter: Some(prefix),
            })
        }
    };
    let body: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return NearMisses::Unavailable(ListingUnavailable {
                url,
                status: Some(status),
                detail: format!("listing body was not JSON: {e}"),
                filter: Some(prefix),
            })
        }
    };
    let Some(rows) = body["result"]["content"].as_array() else {
        // A 200 whose shape is not an Atelier listing is not an empty namespace.
        return NearMisses::Unavailable(ListingUnavailable {
            url,
            status: Some(status),
            detail: "200 response was not an Atelier listing (no result.content array)".to_string(),
            filter: Some(prefix),
        });
    };

    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| r["name"].as_str())
        .map(str::to_string)
        .collect();

    if names.is_empty() {
        return NearMisses::PackageAbsent { prefix };
    }

    let mut close: Vec<String> = names
        .iter()
        .filter(|cand| close_enough(cand, name, MAX_DISTANCE))
        .cloned()
        .collect();
    close.sort();
    close.truncate(MAX_SUGGESTIONS);

    if close.is_empty() {
        NearMisses::NoneSimilar {
            prefix,
            scanned: names.len(),
        }
    } else {
        NearMisses::Found(close)
    }
}

/// The sentence appended to `Document not found: <name>`.
///
/// Each arm says something DIFFERENT, which is the entire point — a caller can act on all four, and
/// none of them is "nothing exists" unless that was actually established.
pub fn describe(namespace: &str, misses: &NearMisses) -> String {
    match misses {
        NearMisses::Found(names) => format!(
            " Closest in namespace '{namespace}': {}. Names are case-sensitive here — a package \
             spelled differently is a different package.",
            names.join(", ")
        ),
        NearMisses::PackageAbsent { prefix } => format!(
            " No document in namespace '{namespace}' begins with '{prefix}' at all, so the PACKAGE \
             is not here — not just this document. Check the namespace, or the package spelling."
        ),
        NearMisses::NoneSimilar { prefix, scanned } => {
            if prefix.is_empty() {
                String::new()
            } else {
                format!(
                    " The package '{prefix}' exists in namespace '{namespace}' ({scanned} \
                     document(s)) but nothing in it resembles this name."
                )
            }
        }
        NearMisses::Unavailable(u) => format!(
            " Could not list namespace '{namespace}' to suggest alternatives, so this is NOT \
             evidence that nothing similar exists: {}.",
            u.detail
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_prefix_strips_only_a_known_extension() {
        assert_eq!(
            package_prefix("Censo.Msg.PacienteMenu.cls").as_deref(),
            Some("Censo.Msg.")
        );
        assert_eq!(package_prefix("Pkg.Routine.mac").as_deref(), Some("Pkg."));
        // `.Census` is a package segment, not an extension, so it must NOT be stripped.
        assert_eq!(
            package_prefix("Pkg.MSG.Census").as_deref(),
            Some("Pkg.MSG.")
        );
        // Nothing to filter on.
        assert_eq!(package_prefix("Flat.cls"), None);
        assert_eq!(package_prefix("NoDots"), None);
    }

    /// THE observed failure: a casing difference in a package segment.
    #[test]
    fn a_casing_difference_counts_as_a_near_miss() {
        assert!(close_enough(
            "Censo.MSG.PacienteMenu.cls",
            "Censo.Msg.PacienteMenu.cls",
            MAX_DISTANCE
        ));
    }

    /// The CONTROL for the test above. Without it, `close_enough` returning true for everything
    /// would pass — and then every refusal would name five unrelated classes.
    #[test]
    fn unrelated_names_are_not_near_misses() {
        // BOTH of these differ in LENGTH by more than the cap, so they exit at the length shortcut
        // and never reach the edit-distance code. That was the whole test once, and a mutation
        // making `close_enough` return true unconditionally SURVIVED it: the assertion held via a
        // branch it was not written to exercise.
        assert!(!close_enough(
            "Censo.MSG.PacienteMenu.cls",
            "Totally.Other.Thing.cls",
            MAX_DISTANCE
        ));
        assert!(!close_enough(
            "A.B.Short.cls",
            "A.B.MuchLongerName.cls",
            MAX_DISTANCE
        ));

        // SAME LENGTH, unrelated — the pair that forces the distance computation to actually run.
        let a = "Alpha.Beta.Gamma.cls";
        let b = "Zulu.Xray.Delta.clsx";
        assert!(
            a.len().abs_diff(b.len()) <= MAX_DISTANCE,
            "this pair must NOT be separable by the length shortcut, or it exercises the same \
             branch as the two above: {} vs {}",
            a.len(),
            b.len()
        );
        assert!(
            !close_enough(a, b, MAX_DISTANCE),
            "{a} and {b} are unrelated and must not be offered as a correction"
        );
    }

    #[test]
    fn each_arm_says_something_different() {
        let found = describe("APP", &NearMisses::Found(vec!["A.B.C.cls".into()]));
        let absent = describe(
            "APP",
            &NearMisses::PackageAbsent {
                prefix: "A.B.".into(),
            },
        );
        let none = describe(
            "APP",
            &NearMisses::NoneSimilar {
                prefix: "A.B.".into(),
                scanned: 7,
            },
        );
        let unavail = describe(
            "APP",
            &NearMisses::Unavailable(ListingUnavailable {
                url: "u".into(),
                status: Some(401),
                detail: "HTTP 401 listing the package".into(),
                filter: Some("A.B.".into()),
            }),
        );

        assert!(found.contains("A.B.C.cls"), "{found}");
        assert!(absent.contains("PACKAGE"), "{absent}");
        assert!(none.contains("7 document(s)"), "{none}");

        // THE assertion this module exists for: a failed lookup must not read as absence.
        assert!(
            unavail.contains("NOT \n             evidence") || unavail.contains("NOT evidence"),
            "the unavailable arm must say plainly that it is not evidence of absence: {unavail}"
        );
        assert!(
            unavail.contains("401"),
            "the reason must survive: {unavail}"
        );
        for other in [&found, &absent, &none] {
            assert!(
                !other.contains("Could not list"),
                "only the Unavailable arm may claim the lookup failed: {other}"
            );
        }
    }

    /// A name with no package produces no sentence at all, rather than an empty-looking claim.
    #[test]
    fn a_packageless_name_adds_nothing_rather_than_claiming_nothing_exists() {
        let s = describe(
            "APP",
            &NearMisses::NoneSimilar {
                prefix: String::new(),
                scanned: 0,
            },
        );
        assert!(
            s.is_empty(),
            "with no package to filter on there is no finding to report, and an empty finding must \
             not be dressed up as one: {s:?}"
        );
    }
}
