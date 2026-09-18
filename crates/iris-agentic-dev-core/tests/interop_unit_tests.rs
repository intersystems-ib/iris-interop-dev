use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection, SystemMode};
use iris_agentic_dev_core::tools::interop::*;
use iris_agentic_dev_core::tools::{ConnectionSource, ConnectionState, IrisTools, Toolset};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

mod interop_production_status {
    use super::*;

    #[test]
    fn iris_unreachable_when_no_connection() {
        let r = rt().block_on(interop_production_status_impl(
            None,
            ProductionStatusParams {
                namespace: "USER".into(),
                full_status: false,
            },
        ));
        let result = r.unwrap();
        let text = result.content[0].raw.as_text().unwrap().text.clone();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["success"], false);
        assert_eq!(v["error_code"], "IRIS_UNREACHABLE");
    }
}

mod production_item_codegen {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn add_item_code_has_canonical_api_and_settings() {
        let mut settings = HashMap::new();
        settings.insert("Adapter.FilePath".to_string(), "/data/in".to_string());
        settings.insert("TargetConfigNames".to_string(), "Router.Censo".to_string());
        let code = build_add_item_code(
            "Cocina.Production",
            "BS.Censo",
            "EnsLib.RecordMap.Service.FileService",
            true,
            Some(1),
            Some("Cocina"),
            &settings,
        );
        // Uses the supported Ens.Config API, not raw global pokes.
        assert!(code.contains("##class(Ens.Config.Production).%OpenId"));
        assert!(code.contains("##class(Ens.Config.Item).%New()"));
        assert!(code.contains("Set tItem.ClassName=\"EnsLib.RecordMap.Service.FileService\""));
        assert!(code.contains("Set tItem.Enabled=1"));
        assert!(code.contains("Set tItem.PoolSize=1"));
        assert!(code.contains("Do tProd.Items.Insert(tItem)"));
        // duplicate guard + live apply only when running.
        assert!(code.contains("ERROR:ITEM_EXISTS"));
        assert!(code.contains("If tRun=tProdName"));
        // adapter-targeted vs host-targeted settings.
        assert!(code.contains("Set tS.Name=\"FilePath\" Set tS.Target=\"Adapter\""));
        assert!(code.contains("Set tS.Name=\"TargetConfigNames\" Set tS.Target=\"Host\""));
    }

    #[test]
    fn add_item_disabled_and_default_production() {
        let code = build_add_item_code(
            "",
            "BO.SQL",
            "Cocina.BO.SQL",
            false,
            None,
            None,
            &HashMap::new(),
        );
        assert!(code.contains("Set tItem.Enabled=0"));
        // empty production -> resolve the running one at runtime.
        assert!(code.contains("GetProductionStatus(.tProdName"));
        assert!(!code.contains("Set tItem.PoolSize"));
    }

    #[test]
    fn remove_item_code_finds_and_removes_by_name() {
        let code = build_remove_item_code("Cocina.Production", "BS.Censo");
        assert!(code.contains("##class(Ens.Config.Production).%OpenId"));
        assert!(code.contains("tProd.Items.RemoveAt(tIdx)"));
        assert!(code.contains("ERROR:ITEM_NOT_FOUND"));
    }

    #[test]
    fn codegen_escapes_quotes_objectscript_style() {
        // ObjectScript literals escape `"` by doubling; `'` needs no escaping (#6).
        let code =
            build_add_item_code("P\"x", "It'\"m", "Cls\"", true, None, None, &HashMap::new());
        assert!(code.contains(r#""It'""m""#));
        assert!(code.contains(r#""Cls""""#));
        assert!(code.contains(r#""P""x""#));
        assert!(!code.contains("''"));
    }
}

mod interop_production_start {
    use super::*;

    #[test]
    fn iris_unreachable() {
        let r = rt().block_on(interop_production_start_impl(
            None,
            ProductionNameParams {
                production: Some("Test".into()),
                namespace: "USER".into(),
            },
        ));
        let result = r.unwrap();
        let text = result.content[0].raw.as_text().unwrap().text.clone();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error_code"], "IRIS_UNREACHABLE");
    }
}

mod production_start_missing_name {
    use super::*;

    /// #63: an absent or blank name never reaches Ens.Director. It used to be
    /// passed through as `StartProduction("")`, which answers
    /// `<Ens>ErrInvalidProduction` — the same error IRIS gives when the production
    /// class was never compiled in the namespace, so a parameter slip read as a
    /// deployment failure and the "fix" looked like recompiling.
    #[test]
    fn is_a_parameter_error_not_a_lifecycle_error() {
        for missing in [None, Some("   ".to_string())] {
            let r = rt().block_on(interop_production_start_impl(
                None,
                ProductionNameParams {
                    production: missing.clone(),
                    namespace: "APP".into(),
                },
            ));
            let result = r.unwrap();
            assert_eq!(result.is_error, Some(true), "for {missing:?}");
            let text = result.content[0].raw.as_text().unwrap().text.clone();
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["error_code"], "MISSING_PARAMETER", "for {missing:?}");
            let err = v["error"].as_str().unwrap();
            assert!(
                !err.contains("ErrInvalidProduction"),
                "must not look like a real IRIS lifecycle failure: {err}"
            );
            let accepted: Vec<&str> = v["accepted_parameters"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap())
                .collect();
            for key in ["production", "production_name", "name"] {
                assert!(accepted.contains(&key), "{key} must be named in the error");
            }
            assert_eq!(v["namespace"], "APP");
        }
    }
}

mod interop_production_stop {
    use super::*;

    #[test]
    fn iris_unreachable() {
        let r = rt().block_on(interop_production_stop_impl(
            None,
            ProductionStopParams {
                production: None,
                namespace: "USER".into(),
                timeout: 30,
                force: false,
            },
        ));
        let result = r.unwrap();
        let text = result.content[0].raw.as_text().unwrap().text.clone();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error_code"], "IRIS_UNREACHABLE");
    }
}

mod interop_production_update {
    use super::*;

    #[test]
    fn iris_unreachable() {
        let r = rt().block_on(interop_production_update_impl(
            None,
            ProductionUpdateParams {
                namespace: "USER".into(),
                timeout: 30,
                force: false,
            },
        ));
        let result = r.unwrap();
        let text = result.content[0].raw.as_text().unwrap().text.clone();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error_code"], "IRIS_UNREACHABLE");
    }
}

mod interop_production_needs_update {
    use super::*;

    #[test]
    fn iris_unreachable() {
        let r = rt().block_on(interop_production_needs_update_impl(
            None,
            ProductionNeedsUpdateParams {
                namespace: "USER".into(),
            },
        ));
        let result = r.unwrap();
        let text = result.content[0].raw.as_text().unwrap().text.clone();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error_code"], "IRIS_UNREACHABLE");
    }
}

mod interop_production_recover {
    use super::*;

    #[test]
    fn iris_unreachable() {
        let r = rt().block_on(interop_production_recover_impl(
            None,
            ProductionRecoverParams {
                namespace: "USER".into(),
            },
        ));
        let result = r.unwrap();
        let text = result.content[0].raw.as_text().unwrap().text.clone();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error_code"], "IRIS_UNREACHABLE");
    }
}

mod interop_logs {
    use super::*;

    #[test]
    fn iris_unreachable() {
        let r = rt().block_on(interop_logs_impl(
            None,
            LogsParams {
                namespace: None,
                item_name: None,
                session_id: None,
                since_id: None,
                limit: 10,
                log_type: "error".into(),
            },
        ));
        let result = r.unwrap();
        let text = result.content[0].raw.as_text().unwrap().text.clone();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error_code"], "IRIS_UNREACHABLE");
    }
}

mod interop_queues {
    use super::*;

    #[test]
    fn iris_unreachable() {
        let r = rt().block_on(interop_queues_impl(None, None));
        let result = r.unwrap();
        let text = result.content[0].raw.as_text().unwrap().text.clone();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error_code"], "IRIS_UNREACHABLE");
    }
}

mod interop_message_search {
    use super::*;

    #[test]
    fn iris_unreachable() {
        let r = rt().block_on(interop_message_search_impl(
            None,
            MessageSearchParams {
                namespace: None,
                source: None,
                target: None,
                class_name: None,
                session_id: None,
                since_id: None,
                limit: 20,
                body_class: None,
                body_where: None,
                body_select: vec![],
                search_table: None,
            },
        ));
        let result = r.unwrap();
        let text = result.content[0].raw.as_text().unwrap().text.clone();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error_code"], "IRIS_UNREACHABLE");
    }
}

mod parse_status {
    use iris_agentic_dev_core::tools::interop::parse_status_response;

    #[test]
    fn running() {
        let (name, code, state) = parse_status_response("Demo.Prod:1").unwrap();
        assert_eq!(name, "Demo.Prod");
        assert_eq!(code, 1);
        assert_eq!(state, "Running");
    }

    #[test]
    fn stopped() {
        let (_, code, state) = parse_status_response("Demo.Prod:2").unwrap();
        assert_eq!(code, 2);
        assert_eq!(state, "Stopped");
    }

    #[test]
    fn troubled() {
        let (_, _code, state) = parse_status_response("Demo.Prod:4").unwrap();
        assert_eq!(state, "Troubled");
    }

    #[test]
    fn no_production() {
        assert!(parse_status_response(":").is_err());
        assert!(parse_status_response("").is_err());
    }

    #[test]
    fn interop_error() {
        let err = parse_status_response("ERROR:Something went wrong").unwrap_err();
        assert!(err.starts_with("INTEROP_ERROR"));
    }
}

// T010 — env-guard: write tools absent when SystemMode=Live
mod env_guard {
    use super::*;

    fn conn_with_mode(mode: SystemMode) -> IrisConnection {
        let mut c = IrisConnection::new(
            "http://localhost:52773",
            "USER",
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        );
        c.system_mode = mode;
        c
    }

    /// #114: what "Live" means changed, deliberately.
    ///
    /// This test used to assert that `iris_credential_manage` and `iris_production_item`
    /// DISAPPEAR on a Live connection. That was wrong in both directions. It let five
    /// write-capable tools through — `iris_doc {mode:put}`, `iris_execute`, `iris_compile`,
    /// `iris_lookup_manage {action:set}` and `iris_test` all dispatched and reached IRIS —
    /// while blocking reads: removing `iris_production_item` wholesale took `get_settings`
    /// with it, so a Live instance could not even be inspected.
    ///
    /// The contract now: every tool stays listed and reachable on every connection, reads
    /// are never refused, and a MUTATING call is refused unless the connection is
    /// write-allowed. `mutating_call` is the decision and is tested exhaustively in
    /// `tools::write_gate_tests`; this pins the half that lives on `IrisTools`.
    #[test]
    fn live_mode_keeps_every_tool_reachable_and_gates_only_writes() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        let tools =
            IrisTools::new_with_toolset(Some(conn_with_mode(SystemMode::Live)), Toolset::Merged)
                .unwrap();
        let names = tools.registered_tool_names();

        // Nothing is hidden any more — hiding a tool hides its read actions too.
        for tool in [
            "iris_credential_manage",
            "iris_production_item",
            "iris_credential_list",
            "iris_lookup_manage",
        ] {
            assert!(
                names.contains(tool),
                "'{tool}' must stay LISTED on a Live connection — the gate refuses writes,                  it does not remove tools"
            );
            assert!(
                tools.is_tool_reachable(tool),
                "'{tool}' must stay REACHABLE on a Live connection"
            );
        }

        // The connection is the thing that is read-only, and the server knows it.
        assert!(
            !tools.write_tools_enabled(),
            "a Live connection must not be write-allowed without IRIS_ALLOW_PROD"
        );
    }

    #[test]
    fn write_tools_present_when_development() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        let tools = IrisTools::new_with_toolset(
            Some(conn_with_mode(SystemMode::Development)),
            Toolset::Merged,
        )
        .unwrap();
        let names = tools.registered_tool_names();
        assert!(names.contains("iris_credential_manage"));
        assert!(names.contains("iris_production_item"));
    }

    /// #169: a hot-reload must never REOPEN a write gate that has closed.
    ///
    /// The gate is inferred from `system_mode` and the namespace, and both arrive from a
    /// `.iris-agentic-dev.toml` that sits inside the caller's own workspace. Before the latch,
    /// a caller refused a write could rewrite `namespace` to something that does not look like
    /// production and retry; the identical call then reached IRIS. Reproduced on 0.13.0, where
    /// the refused `iris_doc put` went from `WRITE_GATED` to an IRIS `HTTP 404` — the request
    /// left the process, which is what proves the gate was gone rather than merely reported
    /// differently.
    #[test]
    fn a_reload_cannot_reopen_a_write_gate_that_has_closed() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        let tools =
            IrisTools::new_with_toolset(Some(conn_with_mode(SystemMode::Live)), Toolset::Merged)
                .unwrap();
        assert!(
            !tools.write_tools_enabled(),
            "precondition: a Live connection is not write-allowed"
        );

        // The escalation: swap in a connection the gate WOULD allow, exactly as a reload of a
        // rewritten config does.
        {
            let mut conn = tools.connection.lock().unwrap();
            *conn = ConnectionState::from_iris(
                conn_with_mode(SystemMode::Development),
                ConnectionSource::ConfigFile,
                None,
            );
        }

        assert!(
            !tools.write_tools_enabled(),
            "a gate that has closed once must stay closed until restart — a config the caller \
             can write must not be able to reopen it"
        );
    }

    /// The latch must not fire on its own: a gate that never closed stays open across reloads,
    /// or every long session would drift shut for no reason.
    #[test]
    fn a_gate_that_never_closed_stays_open_across_a_reload() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        let tools = IrisTools::new_with_toolset(
            Some(conn_with_mode(SystemMode::Development)),
            Toolset::Merged,
        )
        .unwrap();
        assert!(tools.write_tools_enabled());

        {
            let mut conn = tools.connection.lock().unwrap();
            *conn = ConnectionState::from_iris(
                conn_with_mode(SystemMode::Test),
                ConnectionSource::ConfigFile,
                None,
            );
        }

        assert!(
            tools.write_tools_enabled(),
            "the latch arms only on an observed CLOSED gate, never on a reload by itself"
        );
    }

    /// NARROWING is what #114 built the per-call gate for, and #169 must not cost it.
    #[test]
    fn a_reload_onto_a_live_instance_still_closes_the_gate() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        let tools = IrisTools::new_with_toolset(
            Some(conn_with_mode(SystemMode::Development)),
            Toolset::Merged,
        )
        .unwrap();
        assert!(tools.write_tools_enabled());

        {
            let mut conn = tools.connection.lock().unwrap();
            *conn = ConnectionState::from_iris(
                conn_with_mode(SystemMode::Live),
                ConnectionSource::ConfigFile,
                None,
            );
        }

        assert!(
            !tools.write_tools_enabled(),
            "a reload onto a Live instance must still close the gate"
        );
    }

    /// #169: the denial must not hand the model the setting that lifts it.
    ///
    /// The remediation belongs on stderr, where the operator reads it. This guards the
    /// structured field specifically — the prose is checked by reading, the field by CI.
    #[test]
    fn the_write_gate_denial_does_not_name_its_own_bypass() {
        let src = include_str!("../src/tools/mod.rs");
        assert!(
            !src.contains("allow_with"),
            "the WRITE_GATED envelope must not carry an `allow_with` field — a denial that \
             names the setting that lifts it is an instruction to lift it, and the reader of \
             this envelope is the party the gate exists to constrain"
        );
    }
}

// ── 056 interop-depth: iris_message_body / iris_business_rule_info / iris_production_diff ──
// Ported from upstream's 056-interop-depth (f92da6d), adapted to this fork's conventions.

mod interop_depth_helpers {
    use super::*;

    #[test]
    fn content_type_detects_hl7_json_xml_and_text() {
        assert_eq!(detect_content_type("MSH|^~\\&|SENDER|..."), "HL7v2");
        assert_eq!(
            detect_content_type("  \n MSH|^~\\&|X"),
            "HL7v2",
            "leading whitespace is trimmed"
        );
        assert_eq!(detect_content_type("{\"a\":1}"), "JSON");
        assert_eq!(detect_content_type("[1,2]"), "JSON");
        assert_eq!(detect_content_type("<Root/>"), "XML");
        assert_eq!(detect_content_type("just words"), "text");
        assert_eq!(detect_content_type(""), "text");
    }

    #[test]
    fn truncate_body_is_a_noop_under_the_limit() {
        let (out, trunc, len) = truncate_body("hello", 100);
        assert_eq!(out, "hello");
        assert!(!trunc);
        assert_eq!(len, 5);
    }

    #[test]
    fn truncate_body_reports_the_original_length_not_the_kept_length() {
        let (out, trunc, len) = truncate_body("abcdefghij", 4);
        assert_eq!(out, "abcd");
        assert!(trunc);
        assert_eq!(
            len, 10,
            "actual_size must reflect the whole body, not the slice"
        );
    }

    /// Cutting mid-character would panic on a str slice — the boundary walk is the point.
    #[test]
    fn truncate_body_breaks_on_a_utf8_boundary() {
        let s = "aé€"; // 1 + 2 + 3 bytes
        let (out, trunc, len) = truncate_body(s, 2);
        assert_eq!(
            out, "a",
            "must back off to the boundary rather than split é"
        );
        assert!(trunc);
        assert_eq!(len, 6);
        let (out2, _, _) = truncate_body(s, 3);
        assert_eq!(out2, "aé");
    }

    #[test]
    fn redact_hl7v2_blanks_the_phi_fields_and_keeps_the_rest() {
        let msg = "MSH|^~\\&|SENDAPP|SENDFAC|RECVAPP|RECVFAC|20260824||ADT^A01|MSG1|P|2.5\rPID|1||123456^^^MRN||DOE^JOHN||19700101|M|||742 Evergreen Tce^^Springfield^IL^62704";
        let out = redact_hl7v2(msg);
        assert!(
            !out.contains("DOE^JOHN"),
            "PID-5 patient name must go: {out}"
        );
        assert!(
            !out.contains("123456^^^MRN"),
            "PID-3 identifier must go: {out}"
        );
        assert!(!out.contains("19700101"), "PID-7 DOB must go: {out}");
        assert!(!out.contains("Evergreen"), "PID-11 address must go: {out}");
        assert!(!out.contains("SENDAPP"), "MSH-3 sending app must go: {out}");
        assert!(
            out.contains("ADT^A01"),
            "message type is not PHI and must survive: {out}"
        );
        assert!(out.contains("RECVAPP"), "MSH-5 is not redacted: {out}");
        assert!(
            out.starts_with("MSH|"),
            "segment structure must survive: {out}"
        );
    }

    #[test]
    fn redact_hl7v2_leaves_non_hl7_untouched() {
        let json = "{\"patient\":\"DOE^JOHN\"}";
        assert_eq!(
            redact_hl7v2(json),
            json,
            "only HL7 v2 has known PHI positions"
        );
    }

    #[test]
    fn redact_hl7v2_handles_crlf_and_lf_segments() {
        for sep in ["\r", "\n", "\r\n"] {
            let msg =
                format!("MSH|^~\\&|APP|F|R|F|20260824||ADT^A01|1|P|2.5{sep}PID|1||MRN1||DOE^JANE");
            let out = redact_hl7v2(&msg);
            assert!(!out.contains("DOE^JANE"), "sep {sep:?} not handled: {out}");
            assert!(
                out.contains(sep),
                "the original separator must be preserved: {out:?}"
            );
        }
    }

    #[test]
    fn production_items_parse_out_of_class_source() {
        let src = r#"
Class MyApp.Prod Extends Ens.Production
{
XData ProductionDefinition
{
<Production Name="MyApp.Prod">
  <Item Name="FileIn" Category="" ClassName="MyApp.BS.FileService" PoolSize="1" Enabled="true"/>
  <Item Name="SqlOut" Category="" ClassName="MyApp.BO.SqlOperation" PoolSize="1" Enabled="false"/>
</Production>
}
}
"#;
        let items = parse_production_items_from_source(src);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert_eq!(
            items[0],
            ("FileIn".into(), "MyApp.BS.FileService".into(), true)
        );
        assert_eq!(
            items[1],
            ("SqlOut".into(), "MyApp.BO.SqlOperation".into(), false)
        );
    }

    /// `Name="` also appears inside `ClassName="` — the parser must not confuse them.
    #[test]
    fn item_name_is_not_matched_inside_class_name() {
        let src = r#"<Item ClassName="Pkg.BS.Thing" Name="Real" Enabled="true"/>"#;
        let items = parse_production_items_from_source(src);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].0, "Real",
            "Name must not bind to ClassName's suffix"
        );
        assert_eq!(items[0].1, "Pkg.BS.Thing");
    }

    #[test]
    fn item_without_explicit_enabled_defaults_to_enabled() {
        let items = parse_production_items_from_source(r#"<Item Name="A" ClassName="P.BS.A"/>"#);
        assert_eq!(items.len(), 1);
        assert!(items[0].2, "IRIS treats a missing Enabled as enabled");
    }

    #[test]
    fn non_item_lines_are_ignored() {
        let items = parse_production_items_from_source(
            "Class X {\n<Production Name=\"P\">\n</Production>\n}",
        );
        assert!(items.is_empty(), "got {items:?}");
    }
}

mod interop_depth_guards {
    use super::*;

    /// PHI gating is the reason this tool defaults to refusing: a message body can
    /// carry patient data, so `block` must stop it before any IRIS call happens.
    #[test]
    fn message_body_is_blocked_under_the_default_policy() {
        let r = rt().block_on(handle_iris_message_body(
            None,
            &MessageBodyParams {
                message_id: "1".into(),
                namespace: "USER".into(),
                max_bytes: 65536,
                acknowledge_phi: false,
                data_policy: "block".into(),
            },
            "block",
        ));
        let v = tool_payload(&r);
        assert_eq!(v["error_code"], "PHI_POLICY_BLOCKED", "{v}");
    }

    #[test]
    fn allow_policy_still_requires_an_explicit_acknowledgement() {
        let r = rt().block_on(handle_iris_message_body(
            None,
            &MessageBodyParams {
                message_id: "1".into(),
                namespace: "USER".into(),
                max_bytes: 65536,
                acknowledge_phi: false,
                data_policy: "allow".into(),
            },
            "allow",
        ));
        let v = tool_payload(&r);
        assert_eq!(v["error_code"], "PHI_ACK_REQUIRED", "{v}");
    }

    #[test]
    fn a_non_numeric_message_id_is_rejected_before_reaching_iris() {
        let r = rt().block_on(handle_iris_message_body(
            None,
            &MessageBodyParams {
                message_id: "not-a-number".into(),
                namespace: "USER".into(),
                max_bytes: 65536,
                acknowledge_phi: true,
                data_policy: "allow".into(),
            },
            "allow",
        ));
        let v = tool_payload(&r);
        assert_eq!(v["error_code"], "INVALID_MESSAGE_ID", "{v}");
    }

    /// #151: the three checks were `== "block"`, `== "allow"` and a final `== "redact"`,
    /// so a value matching none of them — `Allow`, `none`, a typo — passed every gate and
    /// reached the final branch, which returned the body UNREDACTED and unacknowledged.
    /// Validation has to happen before the gates, not between them.
    #[test]
    fn an_unrecognised_policy_is_refused_rather_than_read_unredacted() {
        for bogus in ["Allow", "ALLOW", "none", "redact ", ""] {
            let r = rt().block_on(handle_iris_message_body(
                None,
                &MessageBodyParams {
                    message_id: "1".into(),
                    namespace: "USER".into(),
                    max_bytes: 65536,
                    acknowledge_phi: true,
                    data_policy: bogus.into(),
                },
                bogus,
            ));
            let v = tool_payload(&r);
            assert_eq!(v["error_code"], "INVALID_PARAM", "policy {bogus:?}: {v}");
        }
    }

    #[test]
    fn business_rule_info_rejects_an_unknown_action() {
        let r = rt().block_on(handle_iris_business_rule_info(
            None,
            &BusinessRuleInfoParams {
                action: "delete".into(),
                rule_name: None,
                namespace: "USER".into(),
            },
        ));
        let v = tool_payload(&r);
        assert_eq!(v["error_code"], "INVALID_ACTION", "{v}");
    }

    #[test]
    fn business_rule_get_requires_a_rule_name() {
        let r = rt().block_on(handle_iris_business_rule_info(
            None,
            &BusinessRuleInfoParams {
                action: "get".into(),
                rule_name: None,
                namespace: "USER".into(),
            },
        ));
        let v = tool_payload(&r);
        assert_eq!(v["error_code"], "INVALID_PARAMS", "{v}");
    }

    #[test]
    fn each_depth_tool_reports_no_connection_rather_than_panicking() {
        let r = rt().block_on(handle_iris_business_rule_info(
            None,
            &BusinessRuleInfoParams {
                action: "list".into(),
                rule_name: None,
                namespace: "USER".into(),
            },
        ));
        assert_eq!(tool_payload(&r)["error_code"], "IRIS_UNREACHABLE");

        let r = rt().block_on(handle_iris_production_diff(
            None,
            &ProductionDiffParams {
                production: None,
                namespace: "USER".into(),
            },
        ));
        assert_eq!(tool_payload(&r)["error_code"], "IRIS_UNREACHABLE");
    }
}

mod interop_depth_redaction_detail {
    use super::*;

    /// Truncation happens before redaction, so a redacted body can be longer than
    /// max_bytes ([REDACTED] is wider than what it replaces). What must stay true is
    /// that no PHI survives and the caller can still tell it got a partial body.
    #[test]
    fn redaction_after_truncation_still_removes_phi() {
        let msg =
            "MSH|^~\\&|SENDAPP|F|R|F|20260825||ADT^A01|1|P|2.5\rPID|1||MRN9||DOE^JOHN||19700101";
        let (cut, truncated, full) = truncate_body(msg, 60);
        assert!(truncated);
        assert_eq!(full, msg.len(), "the reported size is the whole body");
        let out = redact_hl7v2(&cut);
        assert!(
            !out.contains("SENDAPP"),
            "MSH-3 must go even in a partial body: {out}"
        );
    }

    /// A body cut mid-segment must not lose its HL7 identity — content_type drives
    /// whether redaction runs at all.
    #[test]
    fn a_partial_hl7_body_is_still_detected_as_hl7() {
        let (cut, _, _) = truncate_body("MSH|^~\\&|APP|FAC|R|F|20260825||ADT^A01", 12);
        assert_eq!(detect_content_type(&cut), "HL7v2", "got {cut:?}");
    }
}

/// Pull the JSON payload out of a tool result for assertions.
fn tool_payload(r: &Result<rmcp::model::CallToolResult, rmcp::ErrorData>) -> serde_json::Value {
    let r = r.as_ref().expect("tool returned a transport error");
    match &r.content[0].raw {
        rmcp::model::RawContent::Text(t) => serde_json::from_str(&t.text).expect("payload is JSON"),
        _ => panic!("expected text content"),
    }
}

/// #118 / #119 — the production every `iris_production_item` action opens.
mod production_target_resolution {
    use super::*;

    /// ObjectScript has no operator precedence. `If $$$ISERR(sc)||n=""` parses as
    /// `((ISERR)||n)=""`, and since `||` yields 0 or 1 while neither `0=""` nor `1=""` holds,
    /// the branch is unreachable. Three sites shipped it; this is the guard that keeps a
    /// fourth from being written.
    #[test]
    fn the_prologue_tests_for_no_production_in_a_statement_that_can_fire() {
        let code = resolve_production_prologue("");
        assert!(
            !code.contains("||"),
            "compound condition reintroduced in generated ObjectScript:\n{code}"
        );
        assert_eq!(
            code.matches(r#"If tProdName="""#).count(),
            2,
            "expected the fallback and the still-empty test as two statements:\n{code}"
        );
        assert!(code.contains("ERROR:NO_PRODUCTION:"), "{code}");
    }

    #[test]
    fn an_explicit_production_is_embedded_and_the_running_one_is_only_a_fallback() {
        let code = resolve_production_prologue("My.Production");
        assert!(code.contains(r#"Set tProdName="My.Production""#), "{code}");
        // GetProductionStatus stays, but behind the `tProdName=""` test — so a named,
        // STOPPED production is opened from disk instead of being unreachable.
        assert!(code.contains("GetProductionStatus"), "{code}");
        assert!(code.contains(r#"%OpenId(tProdName"#), "{code}");
        assert!(
            !code.contains("%OpenId(n,"),
            "must not open the running production by the `n` the status call filled: {code}"
        );
    }

    /// #119: `get_settings`/`set_settings`/`enable`/`disable` each inlined their own copy that
    /// never read `production=`. `add`/`remove` did. One helper now, so they cannot disagree.
    #[test]
    fn add_and_remove_are_built_from_the_same_prologue() {
        let settings = std::collections::HashMap::new();
        let add = build_add_item_code(
            "My.Production",
            "BS.In",
            "My.BS.In",
            true,
            None,
            None,
            &settings,
        );
        let remove = build_remove_item_code("My.Production", "BS.In");
        let prologue = resolve_production_prologue("My.Production");
        for (what, code) in [("add", &add), ("remove", &remove)] {
            assert!(
                code.starts_with(&prologue),
                "{what} does not open the production the shared way:\n{code}"
            );
        }
    }
}

/// #118, crate-wide: the same precedence trap must not reappear anywhere in generated
/// ObjectScript. Scans the real source rather than one function's output.
#[test]
fn no_generated_objectscript_compares_after_an_unparenthesised_or() {
    fn scan(dir: &std::path::Path, hits: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("readable src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                scan(&path, hits);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("readable source");
                for (n, line) in text.lines().enumerate() {
                    // Prose that QUOTES the trap (this file, and the helper's own doc comment)
                    // is not the trap. Generated ObjectScript comments start `//` as well.
                    let t = line.trim_start();
                    if t.starts_with("//") || t.starts_with('*') {
                        continue;
                    }
                    // Only ObjectScript statements — Rust's own `||` obeys precedence.
                    let is_os = line.contains("$$$ISERR(")
                        || line.trim_start().starts_with("If ")
                        || line.trim_start().starts_with("While ");
                    if !is_os {
                        continue;
                    }
                    for (idx, _) in line.match_indices("||") {
                        let rest = line[idx + 2..].trim_start();
                        // Parenthesising the right operand is the fix, so `|| (` is fine.
                        if rest.starts_with('(') {
                            continue;
                        }
                        let operand: String = rest
                            .chars()
                            .take_while(|c| !"()= <>'".contains(*c))
                            .collect();
                        let after = rest[operand.len()..].trim_start();
                        if after.starts_with('=') || after.starts_with("'=") {
                            hits.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                        }
                    }
                }
            }
        }
    }
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    scan(&src, &mut hits);
    assert!(
        hits.is_empty(),
        "ObjectScript has no operator precedence — parenthesise the right operand (#118):\n{}",
        hits.join("\n")
    );
}

/// #218: an empty config-item name reached `FindItemByConfigName`, which subscripts
/// `^Ens.Runtime("DispatchName","")`, and IRIS answered with a raw `<SUBSCRIPT>` that
/// the server classified as INTEROP_ERROR with no hint. The caller then diagnosed the
/// production — which was fine. The parameter was empty, and `item_name` (the spelling
/// this same tool accepts for the PRODUCTION name) was how callers got there.
mod production_item_name_arg {
    use super::*;
    use std::collections::HashMap;

    fn params(action: &str, item: &str) -> ProductionItemParams {
        ProductionItemParams {
            action: action.into(),
            item: item.into(),
            namespace: "APP".into(),
            settings: HashMap::new(),
            apply: true,
            class_name: None,
            enabled: None,
            production: None,
            pool_size: None,
            category: None,
        }
    }

    /// Every action addresses one item, `add` included — so every action refuses.
    /// `iris: None` is the point: a MISSING_PARAMETER here proves the check runs
    /// BEFORE the connection check, so a parameter slip is never reported as an
    /// unreachable server (and the refusal is knowable without an IRIS at all).
    #[test]
    fn empty_item_is_a_parameter_error_not_a_subscript() {
        for action in [
            "add",
            "remove",
            "enable",
            "disable",
            "get_settings",
            "set_settings",
        ] {
            for blank in ["", "   "] {
                let r = rt().block_on(interop_production_item_impl(None, params(action, blank)));
                let result = r.unwrap();
                assert_eq!(result.is_error, Some(true), "for {action}/{blank:?}");
                let text = result.content[0].raw.as_text().unwrap().text.clone();
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(
                    v["error_code"], "MISSING_PARAMETER",
                    "for {action}/{blank:?}"
                );
                let err = v["error"].as_str().unwrap();
                assert!(
                    !err.contains("SUBSCRIPT") && !err.contains("DispatchName"),
                    "must not look like a broken production: {err}"
                );
                let accepted: Vec<&str> = v["accepted_parameters"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_str().unwrap())
                    .collect();
                for key in ["item", "item_name", "config_name", "name"] {
                    assert!(accepted.contains(&key), "{key} missing from {accepted:?}");
                }
            }
        }
    }

    #[test]
    fn item_name_arg_reads_every_spelling_and_treats_blank_as_absent() {
        for key in ["item", "item_name", "config_name", "name"] {
            let v = serde_json::json!({ key: "BO.WriteToSQL" });
            assert_eq!(
                item_name_arg(&v).as_deref(),
                Some("BO.WriteToSQL"),
                "spelling {key} not read"
            );
        }
        // Blank is absent — it must not reach ObjectScript as "".
        assert_eq!(item_name_arg(&serde_json::json!({"item": "   "})), None);
        assert_eq!(item_name_arg(&serde_json::json!({})), None);
        // Whitespace around a real value is trimmed, not rejected.
        assert_eq!(
            item_name_arg(&serde_json::json!({"item_name": "  BS.In  "})).as_deref(),
            Some("BS.In")
        );
        // The explicit item spellings outrank the shared `name`.
        assert_eq!(
            item_name_arg(&serde_json::json!({"name": "Prod", "item": "BO.Out"})).as_deref(),
            Some("BO.Out")
        );
    }

    /// The trap named in #218: `name` is in PRODUCTION_NAME_KEYS too. On this tool the
    /// item owns it, so the production reader must NOT reach for it — otherwise a bare
    /// `name=` addresses two different things at once.
    #[test]
    fn production_only_arg_does_not_claim_name() {
        assert_eq!(
            production_only_arg(&serde_json::json!({"name": "P.Prod"})),
            None
        );
        assert_eq!(
            production_only_arg(&serde_json::json!({"production_name": "P.Prod"})).as_deref(),
            Some("P.Prod")
        );
        assert_eq!(
            production_only_arg(&serde_json::json!({"production": "P.Prod"})).as_deref(),
            Some("P.Prod")
        );
        // ...while the full reader still does, for every other interop tool.
        assert_eq!(
            production_name_arg(&serde_json::json!({"name": "P.Prod"})).as_deref(),
            Some("P.Prod")
        );
    }
}

/// #185: the ORDER of the two writes into `iris_execute`'s single `hint` slot. The unit
/// tests next to `apply_redirect_hint` prove it YIELDS to an occupied slot; they cannot
/// prove it is CALLED after the thing that occupies it, because that call sits in an async
/// path needing a live IRIS. This guards the call-site ordering in the source instead.
///
/// WHAT THIS DOES NOT COVER, so a clean run is not read as more than it is: it checks the
/// two call sites' relative position in the file, not that they run in that order at
/// runtime, and not that some third writer cannot reach `resp["hint"]` first. A new
/// producer of `hint` on this path would pass this test and still reintroduce #185.
#[test]
fn the_redirect_hint_is_written_after_enrich_abort_not_before() {
    let mod_rs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("tools")
        .join("mod.rs");
    let text = std::fs::read_to_string(&mod_rs).expect("readable mod.rs");

    // Scope the window to the http path's abort branch. An earlier version of this guard
    // searched the whole file after enrich_abort and so matched the DOCKER path's call —
    // it passed with the http call moved back before enrich_abort, i.e. with #185 fully
    // reintroduced. Mutation is what exposed it; the window is the fix.
    let decl = text
        .find("let redirect_hint = sql_lint::execute_redirect_hint(&p.code);")
        .expect("redirect_hint declaration moved — re-point this guard (#185)");
    let enrich = text
        .find("enrich_abort(&iris, client, &namespace, abort, src, &mut resp).await;")
        .expect("the enrich_abort call site moved — re-point this guard (#185)");
    let ret = text[enrich..]
        .find(r#"return envelope::fail_with("IRIS_RUNTIME_ERROR", abort, resp);"#)
        .map(|i| i + enrich)
        .expect("the abort return moved — re-point this guard (#185)");

    // AFTER: the redirect is applied between enrich_abort and the return it feeds.
    assert!(
        text[enrich..ret].contains("apply_redirect_hint(&mut resp, redirect_hint);"),
        "the abort path no longer applies the redirect after enrich_abort (#185)"
    );
    // NOT BEFORE: nothing applies it between the declaration and enrich_abort. This is the
    // assertion that fails when the call is moved back to where the bug was.
    assert!(
        !text[decl..enrich].contains("apply_redirect_hint(&mut resp, redirect_hint);"),
        "the static redirect is claiming the hint slot before the abort explanation (#185)"
    );

    // The `hint` slot has several legitimate writers (abort_hint on both exec paths,
    // enrich_abort). What must stay singular is the REDIRECT's writer — that is the one
    // that was jumping the queue. Both exec paths route through the helper.
    let redirect_writers = text.match_indices("if let Some(h) = redirect_hint").count();
    assert_eq!(
        redirect_writers, 1,
        "redirect_hint must reach the envelope through apply_redirect_hint only (#185)"
    );
}

/// #212: one tool, two contracts. `add` stripped `Adapter.` and set Target="Adapter";
/// `set_settings` hard-coded Target="Host" for every key, so `Adapter.DSN` created a HOST
/// setting literally named "Adapter.DSN" that the adapter never reads. The tool returned OK,
/// the odd name appeared in the Portal with its value — so the caller verified it visually
/// and concluded the write worked — and the BO terminated at startup. The <Ens>ErrGeneral it
/// produced appeared 21 times across 8 students in one cohort.
mod set_settings_target_resolution {
    use super::*;
    use std::collections::HashMap;

    /// The twin of `add_item_code_has_canonical_api_and_settings`'s target assertions,
    /// which #212 notes could not be written while this codegen lived inline in the arm.
    #[test]
    fn an_adapter_prefixed_key_targets_the_adapter() {
        let mut settings = HashMap::new();
        settings.insert(
            "Adapter.DSN".to_string(),
            "jdbc:postgresql://h:5432/d".into(),
        );
        settings.insert("TargetConfigNames".to_string(), "Router.In".into());
        let code = build_set_settings_code("My.Production", "BO.WriteToSQL", &settings, true);

        // Looked up AND created against the resolved target, with the prefix stripped.
        assert!(
            code.contains(r#"Set tS=tItem.FindSettingByName("DSN","Adapter")"#),
            "{code}"
        );
        assert!(
            code.contains(r#"Set tS.Name="DSN" Set tS.Target="Adapter""#),
            "{code}"
        );
        // A bare key is still the business host.
        assert!(
            code.contains(r#"Set tS=tItem.FindSettingByName("TargetConfigNames","Host")"#),
            "{code}"
        );
        // The defect itself: no setting NAMED with its prefix, on any target.
        assert!(
            !code.contains(r#""Adapter.DSN""#),
            "the prefix is still being written into the setting NAME: {code}"
        );
    }

    /// The JDBC quintet from the field report — the combination that dies at startup.
    #[test]
    fn the_jdbc_settings_all_reach_the_adapter() {
        let mut settings = HashMap::new();
        for (k, v) in [
            ("Adapter.DSN", "PGCONN"),
            ("Adapter.JGService", "Util.JDBCGateway"),
            ("Adapter.Credentials", "PGCRED"),
        ] {
            settings.insert(k.to_string(), v.to_string());
        }
        let code = build_set_settings_code("", "BO.WriteToSQL", &settings, true);
        for name in ["DSN", "JGService", "Credentials"] {
            assert!(
                code.contains(&format!(r#"Set tS.Name="{name}" Set tS.Target="Adapter""#)),
                "{name} did not reach the adapter:\n{code}"
            );
        }
    }

    /// `add` and `set_settings` must not drift apart again: one resolver, both callers.
    #[test]
    fn add_and_set_settings_resolve_a_key_identically() {
        for (key, want_target, want_name) in [
            ("Adapter.FilePath", "Adapter", "FilePath"),
            ("Host.PoolSize", "Host", "PoolSize"),
            ("TargetConfigNames", "Host", "TargetConfigNames"),
        ] {
            assert_eq!(resolve_setting_target(key), (want_target, want_name));

            let mut one = HashMap::new();
            one.insert(key.to_string(), "v".to_string());
            let add = build_add_item_code("P", "I", "C", true, None, None, &one);
            let set = build_set_settings_code("P", "I", &one, false);
            let fragment = format!(r#"Set tS.Name="{want_name}" Set tS.Target="{want_target}""#);
            assert!(add.contains(&fragment), "add disagreed for {key}:\n{add}");
            assert!(
                set.contains(&fragment),
                "set_settings disagreed for {key}:\n{set}"
            );
        }
    }

    /// A dotted prefix this tool does not know resolved to Host, which is almost certainly
    /// not what the caller meant. Say so rather than writing it silently.
    #[test]
    fn an_unknown_dotted_prefix_is_warned_about_not_swallowed() {
        let mut settings = HashMap::new();
        settings.insert("Adaptor.DSN".to_string(), "typo-for-Adapter".into());
        let w = unknown_prefix_warnings(&settings);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("Adaptor.DSN"), "{w:?}");
        assert!(
            w[0].contains("Adapter.DSN"),
            "must name the spelling meant: {w:?}"
        );

        // The three legitimate shapes stay silent.
        let mut quiet = HashMap::new();
        quiet.insert("Adapter.DSN".to_string(), "x".into());
        quiet.insert("Host.PoolSize".to_string(), "1".into());
        quiet.insert("TargetConfigNames".to_string(), "R".into());
        assert!(unknown_prefix_warnings(&quiet).is_empty());
    }
}

/// #215: an INVALID_ACTION answers what THIS tool accepts and stops. The model asked for the
/// same thing every time — enumerate — and `iris_production_item` has no listing action at
/// all, so its enum is a list of things that do not help. 3 of 14 students, 7 occurrences,
/// none carrying a hint.
mod enumeration_redirect_tests {
    use super::*;
    use std::collections::HashMap;

    fn envelope(r: Result<rmcp::model::CallToolResult, rmcp::ErrorData>) -> serde_json::Value {
        let result = r.unwrap();
        let text = result.content[0].raw.as_text().unwrap().text.clone();
        serde_json::from_str(&text).unwrap()
    }

    /// The literal call from the field report: action=list on iris_production_item.
    #[test]
    fn an_enumeration_verb_gets_told_where_the_answer_is() {
        let v = envelope(rt().block_on(interop_production_item_impl(
            None,
            ProductionItemParams {
                action: "list".into(),
                // #218's refusal must not pre-empt this: the ACTION is the error here.
                item: "Any.Item".into(),
                namespace: "APP".into(),
                settings: HashMap::new(),
                apply: true,
                class_name: None,
                enabled: None,
                production: None,
                pool_size: None,
                category: None,
            },
        )));
        assert_eq!(v["error_code"], "INVALID_ACTION");
        let hint = v["hint"].as_str().unwrap_or("");
        assert!(!hint.is_empty(), "7 of 7 envelopes carried no hint: {v}");
        // It must say where the answer IS, and be honest that no listing action exists.
        assert!(hint.contains("iris_doc"), "{hint}");
        assert!(
            hint.contains("no listing action"),
            "must admit the capability does not exist rather than invent one: {hint}"
        );
        // And must NOT forward-reference the capabilities that do not work yet (#204).
        assert!(
            !hint.contains("full=true") && !hint.contains("config_items"),
            "points at a capability that does not exist: {hint}"
        );
    }

    /// The helper covers the verbs the cohort actually guessed, and nothing else — a
    /// non-enumeration typo must keep the plain enum message with no misleading redirect.
    #[test]
    fn only_enumeration_verbs_trigger_the_redirect() {
        for verb in [
            "list",
            "list_all",
            "items",
            "show",
            "all",
            "enumerate",
            "LIST",
        ] {
            assert!(
                enumeration_redirect("iris_production_item", verb).is_some(),
                "{verb} should redirect"
            );
        }
        // `update` is genuinely not an action of this tool, but it is not an enumeration —
        // the field report's own sequence shows it, and it must not borrow this hint.
        for verb in ["update", "restart", "get", ""] {
            assert!(
                enumeration_redirect("iris_production_item", verb).is_none(),
                "{verb} must not redirect"
            );
        }
        // A tool with no entry gets nothing rather than another tool's advice.
        assert!(enumeration_redirect("iris_compile", "list").is_none());
    }

    /// iris_lookup_manage DOES have the capability, so its redirect names the actions rather
    /// than admitting absence. This half has no dependency on #204.
    #[test]
    fn lookup_manage_names_its_own_listing_actions() {
        let h = enumeration_redirect("iris_lookup_manage", "list").expect("must redirect");
        assert!(h.contains("list_keys"), "{h}");
        assert!(h.contains("list_tables"), "{h}");
    }
}
