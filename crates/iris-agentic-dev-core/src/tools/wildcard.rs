//! #313: the guarded wildcard expansion for a compile target, shared by `iris_compile` and the
//! CLI's `compile` command.
//!
//! It lived inside the `iris_compile` handler, so only that entry point carried the guards. The
//! CLI reached `compile_document` with the pattern untouched, which — measured against a live
//! instance, since the repo had only ever inferred it — Atelier expands SERVER-SIDE. So the CLI
//! did compile the package; it just did it with no scope rule, no cap and no count, and a pattern
//! matching NOTHING came back `{"status":{"errors":[]}}` with "Compilation finished successfully",
//! i.e. a typo reported as a successful compile.
//!
//! What is deliberately NOT here: the `not_expanded` cross-check. It answers a different question
//! (which Hidden/generated classes a listing-based expansion cannot see) and belongs to the
//! caller that wants to report it. Also measured on the same instance: Atelier's own server-side
//! expansion skips Hidden classes too — `WcProbe.*` left a `[ Hidden ]` class uncompiled, while
//! compiling it by exact name worked — so expanding against the listing here loses no coverage
//! that passing the raw pattern had.

use crate::iris::connection::IrisConnection;

/// The cap a single wildcard compile may queue, re-exported so a caller can name it in its own
/// refusal without hard-coding a second copy of the number.
pub use super::WILDCARD_EXPANSION_CAP;

/// What the class listing said about a wildcard compile target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandedTargets {
    /// Nothing literal precedes the first `*`. Refused before the listing is fetched — there is
    /// no expansion to inspect, and no reason to pull 10k names to say so (#88).
    Unqualified,
    /// More than [`super::WILDCARD_EXPANSION_CAP`] documents matched. Carries the real count so
    /// the caller's error can state it.
    TooBroad { matched: usize },
    /// The documents to compile, suffix included. An EMPTY `targets` is a genuine miss and must
    /// stay a not-found — a typo must never become a compile.
    Expanded {
        /// Matching document names, suffix included.
        targets: Vec<String>,
        /// How many names the listing returned. NOT "documents in the namespace" once
        /// `narrowed` is true: then it counts candidates (#94).
        scanned: usize,
        /// Whether the listing was narrowed server-side via `?filter=`.
        narrowed: bool,
        /// The filter actually used, when one was.
        filter: Option<String>,
    },
}

/// The listing could not be read, so there was nothing to expand against and neither the cap nor
/// the scope rule could be applied.
///
/// Falling back to the raw pattern here used to hand `Pkg.*` straight to `/action/compile` with no
/// expansion, no count and no cap — the guard silently off exactly when the instance is unhealthy.
/// A wildcard therefore fails instead of guessing, and the caller renders the refusal.
#[derive(Debug, Clone)]
pub struct ListingUnavailable {
    /// The URL actually requested, so the caller can reproduce it (#94).
    pub url: String,
    /// The HTTP status, when the request completed. `Some(404)` is the caller's cue to ask
    /// whether the namespace exists at all — that body is zero bytes, so only a second
    /// question can tell a missing namespace from a missing endpoint (#93).
    pub status: Option<u16>,
    /// Human-readable cause: `HTTP <code>`, or the transport error.
    pub detail: String,
    /// The filter attempted, if any.
    pub filter: Option<String>,
}

/// Expand `pattern` against the namespace's CLS listing, applying #88's scope rule and cap.
///
/// The pattern is assumed to contain `*`; a caller with a literal target has nothing to expand.
pub async fn expand_compile_wildcard(
    iris: &IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
    pattern: &str,
) -> Result<ExpandedTargets, ListingUnavailable> {
    // #88: an unqualified pattern is refused BEFORE the listing is fetched.
    if super::wildcard_target_is_unqualified(pattern) {
        return Ok(ExpandedTargets::Unqualified);
    }
    let list_url = iris.versioned_ns_url(namespace, "/docnames/CLS");
    // #94: narrow the listing SERVER-SIDE. `?filter=X` becomes `Name Like '%X%'` inside the query
    // GetDocNames already runs, so the response is a SUPERSET of what the client regex selects.
    // 1,696,950 bytes -> ~2,066; a wildcard compile 331 ms -> ~45 ms.
    //
    // DELIBERATELY NO CACHE, and do not add one. What is left after narrowing is ~38 ms of
    // server-side index walk that no filter can avoid; a perfect cache would buy back ~33 ms.
    // In-process invalidation cannot see another MCP process, a human saving a class in VS Code /
    // Studio / the Portal, an ImportDir or IPM install, a mapping change, or generated dependents
    // — and any of those inside the TTL makes `Pkg.*` skip a class while still reporting
    // success:true. That is the exact failure mode this issue series exists to eliminate; 33 ms
    // does not buy it. `e2e_compile_wildcard_package` is the regression test.
    let listing_filter = super::wildcard_listing_filter(pattern);
    let mut fetch_url = match listing_filter {
        Some(f) => format!("{list_url}?filter={}", urlencoding::encode(f)),
        None => list_url.clone(),
    };
    let mut narrowed = listing_filter.is_some();
    let mut filter_used: Option<String> = listing_filter.map(str::to_string);
    let mut listing = client
        .get(&fetch_url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await;
    // An Atelier build that rejects the parameter must degrade to exactly the unfiltered
    // behaviour, not to a new failure: retry once, unfiltered.
    if narrowed && !matches!(&listing, Ok(r) if r.status().is_success()) {
        fetch_url = list_url.clone();
        narrowed = false;
        filter_used = None;
        listing = client
            .get(&fetch_url)
            .basic_auth(&iris.username, Some(&iris.password))
            .send()
            .await;
    }
    match listing {
        Ok(resp) if resp.status().is_success() => {
            let status = resp.status().as_u16();
            // #310's shape, inherited with this code: `resp.json().unwrap_or_default()` turns an
            // unreadable 200 into `Null`, `docnames_in_body` turns that into `[]`, and the caller
            // then reports "no CLS document matches 'X' (0 name(s) scanned)" — a claim about the
            // namespace, made from a body nobody could read. A web gateway serving an HTML error
            // page with a 200 is enough to produce it. The three ways to fail are now three
            // failures; only `content: []` is an empty namespace.
            let fail = |detail: String| {
                Err(ListingUnavailable {
                    url: fetch_url.clone(),
                    status: Some(status),
                    detail,
                    filter: filter_used.clone(),
                })
            };
            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    return fail(format!(
                        "HTTP {status}, but the body is not an Atelier listing: it could not be \
                         read ({e})"
                    ))
                }
            };
            let body: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    return fail(format!(
                        "HTTP {status}, but the body is not an Atelier listing: not JSON ({e}). \
                         First {} byte(s): {:?}",
                        text.len().min(120),
                        &text[..text.len().min(120)]
                    ))
                }
            };
            if !body["result"]["content"].is_array() {
                return fail(format!(
                    "HTTP {status}, but the body is not an Atelier listing: `result.content` is \
                     {}, not an array. An empty listing is `content: []`; this is a different \
                     answer and must not be read as one.",
                    match &body["result"]["content"] {
                        serde_json::Value::Null => "absent".to_string(),
                        other => format!("{other:.60}"),
                    }
                ));
            }
            // One pass over the listing, not two: `scanned` and the expansion read the same Vec.
            let names = super::docnames_in_body(&body);
            let scanned = names.len();
            Ok(match super::expand_wildcard_target(&names, pattern) {
                // Unreachable — the guard above already returned — but the outcome is the pure
                // function's to own, not this call site's.
                super::WildcardExpansion::Unqualified => ExpandedTargets::Unqualified,
                super::WildcardExpansion::TooBroad { matched } => {
                    ExpandedTargets::TooBroad { matched }
                }
                super::WildcardExpansion::Matched(targets) => ExpandedTargets::Expanded {
                    targets,
                    scanned,
                    narrowed,
                    filter: filter_used,
                },
            })
        }
        other => {
            let (status, detail) = match other {
                Ok(resp) => {
                    let s = resp.status().as_u16();
                    (Some(s), format!("HTTP {s}"))
                }
                Err(e) => (None, e.to_string()),
            };
            Err(ListingUnavailable {
                url: fetch_url,
                status,
                detail,
                filter: filter_used,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    //! #313: the shared decision, tested where it lives. `iris_compile` had these outcomes pinned
    //! only through its own envelope; the CLI now depends on the same four, so they are asserted
    //! against the function rather than against one caller's rendering of it.
    use super::*;
    use crate::iris::connection::DiscoverySource;
    use wiremock::matchers::{method, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    fn listing(names: &[&str]) -> serde_json::Value {
        serde_json::json!({"result": {"content": names.iter()
            .map(|n| serde_json::json!({"cat":"CLS","db":"APP-CODE","gen":false,"name":n}))
            .collect::<Vec<_>>()}})
    }

    async fn run(
        server: &MockServer,
        pattern: &str,
    ) -> Result<ExpandedTargets, ListingUnavailable> {
        let conn = IrisConnection::new(
            server.uri(),
            "APP",
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        );
        let client = IrisConnection::http_client().unwrap();
        expand_compile_wildcard(&conn, &client, "APP", pattern).await
    }

    /// Mount the listing for `pattern`'s OWN filter, read from the implementation rather than
    /// retyped. Hardcoding it put the wrong string ("APPPKG" for "APPPKG.*", which narrows on
    /// "APPPKG.") into five mocks at once: every request 404'd, the retry cleared the filter, and
    /// each test failed with a ListingUnavailable that looked like a bug in the code under test.
    async fn mount_for(server: &MockServer, pattern: &str, names: &[&str]) {
        let filter = super::super::wildcard_listing_filter(pattern)
            .expect("these tests only use patterns that narrow");
        Mock::given(method("GET"))
            .and(path_regex(r".*/docnames/CLS$"))
            .and(query_param("filter", filter))
            .respond_with(ResponseTemplate::new(200).set_body_json(listing(names)))
            .mount(server)
            .await;
    }

    /// The control. Without it every refusal below is satisfied by a function that always refuses.
    #[test]
    fn a_qualified_pattern_expands_to_what_matched() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_for(
                &server,
                "APPPKG.*",
                &["APPPKG.One.cls", "APPPKG.Two.cls", "Other.Three.cls"],
            )
            .await;
            match run(&server, "APPPKG.*").await {
                Ok(ExpandedTargets::Expanded {
                    targets, scanned, ..
                }) => {
                    assert_eq!(targets, vec!["APPPKG.One.cls", "APPPKG.Two.cls"]);
                    assert_eq!(scanned, 3, "scanned counts the listing, not the matches");
                }
                other => panic!("expected Expanded, got {other:?}"),
            }
        });
    }

    /// #88: refused with NO request made. `expect(0)` on the listing is the assertion — a guard
    /// that fires only after pulling 10k names is not the guard that was specified.
    #[test]
    fn an_unqualified_pattern_is_refused_before_any_listing_request() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(listing(&[])))
                .expect(0)
                .mount(&server)
                .await;
            assert_eq!(
                run(&server, "*").await.unwrap(),
                ExpandedTargets::Unqualified
            );
            assert_eq!(
                run(&server, "*.cls").await.unwrap(),
                ExpandedTargets::Unqualified
            );
            assert_eq!(
                run(&server, "*Foo").await.unwrap(),
                ExpandedTargets::Unqualified
            );
        });
    }

    /// #88: over the cap, and the COUNT comes back so the caller can state it. A refusal that
    /// cannot say how many matched sends the reader back to guess at a narrower package.
    #[test]
    fn over_the_cap_is_too_broad_and_carries_the_real_count() {
        rt().block_on(async {
            let server = MockServer::start().await;
            let names: Vec<String> = (0..WILDCARD_EXPANSION_CAP + 3)
                .map(|i| format!("APPPKG.C{i}.cls"))
                .collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            mount_for(&server, "APPPKG.*", &refs).await;
            assert_eq!(
                run(&server, "APPPKG.*").await.unwrap(),
                ExpandedTargets::TooBroad {
                    matched: WILDCARD_EXPANSION_CAP + 3
                }
            );
        });
    }

    /// Exactly at the cap must still compile — an off-by-one here turns a legal package into a
    /// refusal, which is the failure mode a cap acquires when nobody tests its boundary.
    #[test]
    fn exactly_at_the_cap_still_expands() {
        rt().block_on(async {
            let server = MockServer::start().await;
            let names: Vec<String> = (0..WILDCARD_EXPANSION_CAP)
                .map(|i| format!("APPPKG.C{i}.cls"))
                .collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            mount_for(&server, "APPPKG.*", &refs).await;
            match run(&server, "APPPKG.*").await.unwrap() {
                ExpandedTargets::Expanded { targets, .. } => {
                    assert_eq!(targets.len(), WILDCARD_EXPANSION_CAP)
                }
                other => panic!("expected Expanded at the cap, got {other:?}"),
            }
        });
    }

    /// An EMPTY expansion is a distinct outcome, NOT an error and NOT a compile. This is the one
    /// the CLI had no way to report: Atelier answers a no-match wildcard with `errors: []` and
    /// "Compilation finished successfully", so passing the pattern through printed success for a
    /// typo. Measured on a live instance, which is why this case exists.
    #[test]
    fn a_pattern_matching_nothing_expands_to_an_empty_set() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_for(&server, "NOSUCH.*", &["Other.Thing.cls"]).await;
            match run(&server, "NOSUCH.*").await.unwrap() {
                ExpandedTargets::Expanded {
                    targets, scanned, ..
                } => {
                    assert!(targets.is_empty(), "got {targets:?}");
                    assert_eq!(scanned, 1, "the listing was read; it just matched nothing");
                }
                other => panic!("expected an empty Expanded, got {other:?}"),
            }
        });
    }

    /// A listing that cannot be read must NOT degrade to "pass the pattern through": that is the
    /// guard silently off exactly when the instance is unhealthy. The 404 case additionally has to
    /// carry its status, because the caller's next question — does the namespace exist? — cannot
    /// be answered from a zero-byte body (#93).
    #[test]
    fn a_listing_failure_is_an_error_carrying_status_and_url() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            let err = run(&server, "APPPKG.*").await.unwrap_err();
            assert_eq!(err.status, Some(404), "{err:?}");
            assert!(err.url.contains("/docnames/CLS"), "{err:?}");
            assert!(err.detail.contains("404"), "{err:?}");
        });
    }

    /// #310's shape, in the line this module inherited: a 200 whose body is NOT an Atelier listing
    /// must not read as "nothing matched". `resp.json().unwrap_or_default()` yields `Null`,
    /// `docnames_in_body` yields `[]`, and the caller then states a NOT_FOUND — "no CLS document
    /// matches 'X' (0 name(s) scanned)" — which is a claim about the namespace derived from a body
    /// that could not be read. A failure must not be answered with a negative fact.
    #[test]
    fn a_200_whose_body_is_not_a_listing_is_a_failure_not_an_empty_namespace() {
        rt().block_on(async {
            for body in [
                "<html>the web gateway ate it</html>",
                "",
                "{\"result\":{}}",
                "{\"result\":{\"content\":\"not-an-array\"}}",
            ] {
                let server = MockServer::start().await;
                Mock::given(method("GET"))
                    .and(path_regex(r".*/docnames/CLS$"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(body))
                    .mount(&server)
                    .await;
                match run(&server, "APPPKG.*").await {
                    Err(e) => assert!(
                        e.detail.contains("not an Atelier listing"),
                        "body {body:?} -> {e:?}"
                    ),
                    Ok(other) => panic!(
                        "body {body:?} was read as {other:?} — an unreadable listing must not \
                         become 'nothing matched'"
                    ),
                }
            }
        });
    }

    /// The control for the case above: a 200 that IS a listing and is genuinely empty stays an
    /// empty expansion, because a namespace really can hold nothing. Without this, the fix above
    /// would be satisfied by treating every empty result as a failure — which would turn a real
    /// empty namespace into a listing error.
    #[test]
    fn a_genuinely_empty_listing_is_still_an_empty_expansion() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_for(&server, "APPPKG.*", &[]).await;
            match run(&server, "APPPKG.*").await.unwrap() {
                ExpandedTargets::Expanded {
                    targets, scanned, ..
                } => {
                    assert!(targets.is_empty());
                    assert_eq!(scanned, 0, "an empty listing is zero names, legitimately");
                }
                other => panic!("expected an empty Expanded, got {other:?}"),
            }
        });
    }

    /// #94: a build that rejects `?filter=` must degrade to the unfiltered request, not to a new
    /// failure — and then `narrowed` must say so, because `scanned` means something different in
    /// each case.
    #[test]
    fn a_rejected_filter_retries_unfiltered_and_reports_it() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .and(query_param(
                    "filter",
                    super::super::wildcard_listing_filter("APPPKG.*").unwrap(),
                ))
                .respond_with(ResponseTemplate::new(400))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(listing(&["APPPKG.One.cls"])),
                )
                .mount(&server)
                .await;
            match run(&server, "APPPKG.*").await.unwrap() {
                ExpandedTargets::Expanded {
                    targets,
                    narrowed,
                    filter,
                    ..
                } => {
                    assert_eq!(targets, vec!["APPPKG.One.cls"]);
                    assert!(!narrowed, "the retry was unfiltered");
                    assert!(filter.is_none(), "no filter survived the retry: {filter:?}");
                }
                other => panic!("expected Expanded after the retry, got {other:?}"),
            }
        });
    }
}
