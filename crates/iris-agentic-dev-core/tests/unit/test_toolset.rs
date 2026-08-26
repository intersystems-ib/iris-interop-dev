// T015–T027: Toolset unit tests.
// Tests for Nostub and Merged toolset configurations.
// Written FIRST — must FAIL until T017–T033 are implemented.

use iris_agentic_dev_core::tools::{IrisTools, Toolset};

// ── Toolset::from_str ────────────────────────────────────────────────────────

#[test]
fn test_toolset_from_str_baseline() {
    assert_eq!(Toolset::from_str("baseline"), Toolset::Baseline);
    assert_eq!(Toolset::from_str(""), Toolset::Baseline);
    assert_eq!(Toolset::from_str("unknown"), Toolset::Baseline);
}

#[test]
fn test_toolset_from_str_nostub() {
    assert_eq!(Toolset::from_str("nostub"), Toolset::Nostub);
    assert_eq!(Toolset::from_str("NOSTUB"), Toolset::Nostub);
}

#[test]
fn test_toolset_from_str_merged() {
    assert_eq!(Toolset::from_str("merged"), Toolset::Merged);
    assert_eq!(Toolset::from_str("MERGED"), Toolset::Merged);
}

// ── T015: Nostub — stub tools absent ────────────────────────────────────────

/// iris_symbols_local is now a real tool (025-symbols-local-ts) — must be present in nostub.
#[test]
fn test_nostub_excludes_iris_symbols_local() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Nostub).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    assert!(
        names.contains("iris_symbols_local"),
        "iris_symbols_local must be registered in nostub toolset (no longer a stub). Found symbols tools: {:?}",
        names
            .iter()
            .filter(|n| n.contains("symbol"))
            .collect::<Vec<_>>()
    );
}

/// skill tool must not expose propose/optimize/share actions in nostub (FR-005).
#[test]
fn test_nostub_skill_excludes_stub_actions() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Nostub).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    for stub_action in &["skill_propose", "skill_optimize", "skill_share"] {
        assert!(
            !names.contains(*stub_action),
            "{} must not be registered in nostub toolset",
            stub_action
        );
    }
}

/// skill_community must not expose install action in nostub (FR-006).
#[test]
fn test_nostub_skill_community_excludes_install() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Nostub).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    assert!(
        !names.contains("skill_community_install"),
        "skill_community_install must not be registered in nostub toolset"
    );
}

/// Nostub must preserve all non-stub tools (not accidentally remove real ones).
#[test]
fn test_nostub_preserves_core_tools() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Nostub).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    for required in &[
        "iris_compile",
        "iris_execute",
        "iris_doc",
        "iris_query",
        "iris_symbols",
        "docs_introspect",
        "iris_search",
        "iris_info",
    ] {
        assert!(
            names.contains(*required),
            "Core tool {} must still be registered in nostub toolset",
            required
        );
    }
}

/// Nostub should have exactly 4 fewer tools than baseline
/// (skill_propose + skill_optimize + skill_share + skill_community_install = 4 stubs removed).
/// iris_symbols_local is no longer a stub (025-symbols-local-ts).
#[test]
fn test_nostub_tool_count() {
    let baseline = IrisTools::new_with_toolset(None, Toolset::Baseline)
        .expect("baseline IrisTools")
        .registered_tool_names()
        .len();
    let nostub = IrisTools::new_with_toolset(None, Toolset::Nostub)
        .expect("nostub IrisTools")
        .registered_tool_names()
        .len();
    assert_eq!(
        nostub,
        baseline - 4,
        "Nostub should have exactly 4 fewer tools than baseline (got baseline={}, nostub={})",
        baseline,
        nostub
    );
}

// ── T020–T027: Merged — parity stubs (full parity tests require live IRIS) ──

/// iris_debug must be registered in merged toolset (FR-007).
#[test]
fn test_merged_registers_iris_debug() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Merged).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    assert!(
        names.contains("iris_debug"),
        "iris_debug must be registered in merged toolset. Found tools: {:?}",
        names
            .iter()
            .filter(|n| n.contains("debug"))
            .collect::<Vec<_>>()
    );
}

/// iris_production must be registered in merged toolset (FR-008).
#[test]
fn test_merged_registers_iris_production() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Merged).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    assert!(
        names.contains("iris_production"),
        "iris_production must be registered in merged toolset"
    );
}

/// iris_interop_query must be registered in merged toolset (FR-009).
#[test]
fn test_merged_registers_iris_interop_query() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Merged).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    assert!(
        names.contains("iris_interop_query"),
        "iris_interop_query must be registered in merged toolset"
    );
}

/// iris_containers must be registered in merged toolset (FR-010).
#[test]
fn test_merged_registers_iris_containers() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Merged).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    assert!(
        names.contains("iris_containers"),
        "iris_containers must be registered in merged toolset"
    );
}

/// agent_info must NOT be registered in merged toolset (FR-011).
#[test]
fn test_merged_excludes_agent_info() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Merged).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    assert!(
        !names.contains("agent_info"),
        "agent_info must not be registered in merged toolset"
    );
}

/// Merged must exclude all original debug tools (replaced by iris_debug).
#[test]
fn test_merged_excludes_original_debug_tools() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Merged).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    for replaced in &[
        "debug_capture_packet",
        "debug_get_error_logs",
        "debug_map_int_to_cls",
        "debug_source_map",
    ] {
        assert!(
            !names.contains(*replaced),
            "{} must not be registered in merged toolset (replaced by iris_debug)",
            replaced
        );
    }
}

/// Merged must exclude all original interop production tools (replaced by iris_production).
#[test]
fn test_merged_excludes_original_interop_production_tools() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Merged).expect("IrisTools::new");
    let names = tools.registered_tool_names();
    for replaced in &[
        "interop_production_status",
        "interop_production_start",
        "interop_production_stop",
        "interop_production_update",
        "interop_production_needs_update",
        "interop_production_recover",
    ] {
        assert!(
            !names.contains(*replaced),
            "{} must not be registered in merged toolset (replaced by iris_production)",
            replaced
        );
    }
}

/// Merged must advertise exactly 46 tools (measured 2026-08-26; 50 - 8 + 4).
/// Renamed: the old `test_merged_tool_count_is_23` contradicted its own assertion (33),
/// and both numbers came from a hardcoded list that had drifted away from the router.
#[test]
fn test_merged_tool_count() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Merged).expect("IrisTools::new");
    let count = tools.registered_tool_names().len();
    assert_eq!(
        count, 46,
        "Merged toolset must advertise exactly 46 tools, got {}",
        count
    );
    // iris_get_log must be registered in Merged (027-progressive-disclosure)
    assert!(
        tools.registered_tool_names().contains("iris_get_log"),
        "iris_get_log must appear in Merged toolset"
    );
}

/// iris_get_log must NOT be registered in Baseline or Nostub (027-progressive-disclosure).
#[test]
fn test_iris_get_log_absent_from_baseline_and_nostub() {
    let baseline = IrisTools::new_with_toolset(None, Toolset::Baseline).expect("IrisTools::new");
    assert!(
        !baseline.registered_tool_names().contains("iris_get_log"),
        "iris_get_log must NOT appear in Baseline toolset"
    );
    let nostub = IrisTools::new_with_toolset(None, Toolset::Nostub).expect("IrisTools::new");
    assert!(
        !nostub.registered_tool_names().contains("iris_get_log"),
        "iris_get_log must NOT appear in Nostub toolset"
    );
}

// ── Interop profile (fork default) ───────────────────────────────────────────

#[test]
fn test_toolset_from_str_interop() {
    assert_eq!(Toolset::from_str("interop"), Toolset::Interop);
    assert_eq!(Toolset::from_str("INTEROP"), Toolset::Interop);
}

/// Interop toolset must expose EXACTLY the interop keep-list (INTEROP_TOOLS).
/// This is the registry self-check: derived from the live router, so a typo in the
/// keep-list or an upstream rename makes this fail instead of silently shipping wrong tools.
#[test]
fn test_interop_toolset_exact() {
    use iris_agentic_dev_core::tools::INTEROP_TOOLS;
    let tools = IrisTools::new_with_toolset(None, Toolset::Interop).expect("interop IrisTools");
    let names = tools.registered_tool_names();
    let expected: std::collections::HashSet<String> =
        INTEROP_TOOLS.iter().map(|s| s.to_string()).collect();
    let missing: Vec<_> = expected.difference(&names).collect();
    let unexpected: Vec<_> = names.difference(&expected).collect();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "Interop toolset must expose exactly INTEROP_TOOLS.\n  missing (in keep-list but not router — typo/rename?): {:?}\n  unexpected (in router but not keep-list): {:?}",
        missing,
        unexpected,
    );
    assert_eq!(
        names.len(),
        INTEROP_TOOLS.len(),
        "Interop profile must be {} tools, got {}",
        INTEROP_TOOLS.len(),
        names.len()
    );
}

/// Interop must keep the interop-critical tools the workshop actually needed.
#[test]
fn test_interop_preserves_critical_tools() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Interop).expect("interop IrisTools");
    let names = tools.registered_tool_names();
    for required in &[
        "iris_query",
        "iris_execute",
        "iris_compile",
        "iris_doc",
        "iris_test",
        "iris_production",
        "iris_interop_query",
        "iris_lookup_manage",
        "iris_credential_manage",
        "iris_table_info",
    ] {
        assert!(
            names.contains(*required),
            "interop-critical tool {} must be in the interop profile",
            required
        );
    }
}

/// Interop must prune the meta/non-interop surface (skills/kb/agent/generate/search/info/debug-individual/scm/containers).
#[test]
fn test_interop_excludes_meta_tools() {
    let tools = IrisTools::new_with_toolset(None, Toolset::Interop).expect("interop IrisTools");
    let names = tools.registered_tool_names();
    for excluded in &[
        "skill",
        "skill_list",
        "kb",
        "agent_info",
        "iris_search",
        "iris_info",
        "iris_generate",
        "iris_macro",
        "iris_source_control",
        "iris_containers",
        "iris_admin",
        "debug_capture_packet",
    ] {
        assert!(
            !names.contains(*excluded),
            "meta/non-interop tool {} must NOT be in the interop profile",
            excluded
        );
    }
}

// ── Anti-drift: the counts the Toolset doc comments claim ────────────────────
//
// Until 2026-08-26 `registered_tool_names()` built the non-Interop tiers from a hardcoded
// list last audited against v0.4.x. It had drifted 17 tools short and carried one phantom
// (`iris_admin`, which the router removes for baseline/nostub), and NOTHING caught it:
// the tests that guarded it compared the hardcoded list against itself, so they were
// tautological. These tests pin the real, router-derived numbers.

/// Baseline advertises 54 — the 58 the `#[tool_router]` macro registers minus the 4
/// merged-only ones.
#[test]
fn test_baseline_tool_count() {
    let n = IrisTools::new_with_toolset(None, Toolset::Baseline)
        .expect("IrisTools::new")
        .registered_tool_names()
        .len();
    assert_eq!(
        n, 54,
        "Baseline must advertise exactly 54 tools (Toolset::Baseline doc comment says 54), got {}",
        n
    );
}

/// `test_nostub_tool_count` only asserts baseline-minus-4, so it stayed green through the
/// whole drift. Pin the absolute number too.
#[test]
fn test_nostub_tool_count_absolute() {
    let n = IrisTools::new_with_toolset(None, Toolset::Nostub)
        .expect("IrisTools::new")
        .registered_tool_names()
        .len();
    assert_eq!(n, 50, "Nostub must advertise exactly 50 tools, got {}", n);
}

/// The single regression gate for the four doc-comment numbers. A failure here means the
/// tool surface moved: update `Toolset`'s doc comments in src/tools/mod.rs, or explain
/// the change.
#[test]
fn test_toolset_counts_match_doc_comments() {
    for (ts, expected) in [
        (Toolset::Baseline, 54usize),
        (Toolset::Nostub, 50),
        (Toolset::Merged, 46),
        (Toolset::Interop, 23),
    ] {
        let n = IrisTools::new_with_toolset(None, ts)
            .expect("IrisTools::new")
            .registered_tool_names()
            .len();
        assert_eq!(
            n, expected,
            "Toolset::{:?} advertises {} tools but src/tools/mod.rs documents {} — update \
             the doc comment or explain the change",
            ts, n, expected
        );
    }
    // Two independent anchors for the interop number: the keep-list and the router.
    assert_eq!(
        iris_agentic_dev_core::tools::INTEROP_TOOLS.len(),
        23,
        "INTEROP_TOOLS is the interop profile — it must agree with the measured count"
    );
}

/// The merged-only tools must not leak into baseline/nostub. `iris_admin` is the one the
/// stale hardcoded list got wrong: it claimed baseline advertised a tool the router
/// explicitly removes. `test_iris_get_log_absent_from_baseline_and_nostub` covers one of
/// the four; this generalises it.
#[test]
fn test_baseline_excludes_merged_only_tools() {
    let baseline = IrisTools::new_with_toolset(None, Toolset::Baseline)
        .expect("IrisTools::new")
        .registered_tool_names();
    let nostub = IrisTools::new_with_toolset(None, Toolset::Nostub)
        .expect("IrisTools::new")
        .registered_tool_names();
    for t in [
        "iris_debug",
        "iris_containers",
        "iris_admin",
        "iris_get_log",
    ] {
        assert!(
            !baseline.contains(t),
            "{t} is merged-only and must NOT be in Baseline"
        );
        assert!(
            !nostub.contains(t),
            "{t} is merged-only and must NOT be in Nostub"
        );
    }
}

/// `IrisTools::new()` stamps `Toolset::Baseline`, so its router must be the pruned
/// baseline router — it used to assign the raw 58-tool one.
#[test]
fn test_new_uses_pruned_baseline_router() {
    let via_new = IrisTools::new(None)
        .expect("IrisTools::new")
        .registered_tool_names();
    let via_toolset = IrisTools::new_with_toolset(None, Toolset::Baseline)
        .expect("IrisTools::new")
        .registered_tool_names();
    assert_eq!(
        via_new.len(),
        54,
        "new() claims Toolset::Baseline, so it must advertise the baseline surface"
    );
    assert_eq!(
        via_new, via_toolset,
        "new() and new_with_toolset(_, Baseline) must register the same tools"
    );
}
