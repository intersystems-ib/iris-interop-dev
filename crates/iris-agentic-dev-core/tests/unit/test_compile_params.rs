//! T015: Unit tests for CompileParams deserialization.

#[cfg(test)]
mod tests {
    use iris_agentic_dev_core::tools::CompileParams;

    #[test]
    fn compile_params_basic() {
        let p: CompileParams = serde_json::from_str(r#"{"target":"MyApp.Patient.cls"}"#).unwrap();
        assert_eq!(p.target, "MyApp.Patient.cls");
        assert_eq!(p.flags, "cuk");
        assert_eq!(p.namespace, None); // omitted -> resolved to the connection namespace at call time
        assert!(!p.force_writable);
    }

    #[test]
    fn compile_params_wildcard() {
        let p: CompileParams =
            serde_json::from_str(r#"{"target":"MyApp.*.cls","flags":"ck"}"#).unwrap();
        assert_eq!(p.target, "MyApp.*.cls");
        assert!(p.target.contains('*'), "wildcard target must contain *");
        assert_eq!(p.flags, "ck");
    }

    #[test]
    fn compile_params_full() {
        let p: CompileParams = serde_json::from_str(
            r#"{"target":"HS.FHIR.*.cls","flags":"cuk","namespace":"HSLIB","force_writable":true}"#,
        )
        .unwrap();
        assert_eq!(p.namespace.as_deref(), Some("HSLIB"));
        assert!(p.force_writable);
    }
}

/// #211: the four tools use a different noun for the same idea — iris_query(query),
/// iris_compile(target), iris_test(pattern), docs_introspect(class_name). A caller
/// reasoning from the domain sent the natural synonym, serde failed BEFORE the handler
/// ran, and rmcp answered with a bare -32602 that names the MISSING field, never the one
/// that was sent, and carries no error_code. 5 of 14 students, 6 occurrences.
///
/// These exercise the real deserializer rather than restating a list of aliases: a list
/// kept beside the struct drifts, and a drifted list still passes.
#[cfg(test)]
mod alias_rescue_path {
    use iris_agentic_dev_core::tools::{CompileParams, IntrospectParams, QueryParams, TestParams};

    #[test]
    fn compile_accepts_every_class_spelling() {
        for k in ["target", "class", "classname", "className", "class_name"] {
            let p: CompileParams =
                serde_json::from_str(&format!(r#"{{"{k}":"MyApp.Patient.cls"}}"#))
                    .unwrap_or_else(|e| panic!("iris_compile rejected {k}: {e}"));
            assert_eq!(p.target, "MyApp.Patient.cls", "for {k}");
            assert_eq!(
                p.flags, "cuk",
                "defaults must survive the alias path, for {k}"
            );
        }
    }

    /// The exact call from the field report: {"namespace": .., "classname": ..} on iris_test,
    /// which produced `missing field \`pattern\`` and a correct retry 14 seconds later.
    #[test]
    fn test_accepts_the_spelling_the_cohort_actually_sent() {
        let p: TestParams = serde_json::from_str(
            r#"{"namespace":"MYAPP","classname":"MyApp.Tests.CensoRecordMapTest"}"#,
        )
        .expect("the measured call must no longer fail in serde");
        assert_eq!(p.pattern, "MyApp.Tests.CensoRecordMapTest");
        assert_eq!(p.namespace.as_deref(), Some("MYAPP"));

        for k in [
            "pattern",
            "class",
            "classname",
            "className",
            "class_name",
            "test_class",
        ] {
            let p: TestParams = serde_json::from_str(&format!(r#"{{"{k}":"A.B.Test"}}"#))
                .unwrap_or_else(|e| panic!("iris_test rejected {k}: {e}"));
            assert_eq!(p.pattern, "A.B.Test", "for {k}");
        }
    }

    #[test]
    fn query_accepts_sql_and_statement() {
        for k in ["query", "sql", "statement"] {
            let p: QueryParams = serde_json::from_str(&format!(r#"{{"{k}":"SELECT 1"}}"#))
                .unwrap_or_else(|e| panic!("iris_query rejected {k}: {e}"));
            assert_eq!(p.query, "SELECT 1", "for {k}");
        }
    }

    #[test]
    fn introspect_accepts_class_but_still_owns_class_name() {
        for k in ["class_name", "class", "classname", "className"] {
            let p: IntrospectParams = serde_json::from_str(&format!(r#"{{"{k}":"Ens.Director"}}"#))
                .unwrap_or_else(|e| panic!("docs_introspect rejected {k}: {e}"));
            assert_eq!(p.class_name, "Ens.Director", "for {k}");
        }
    }

    /// The collision the issue warns about, asserted as BEHAVIOUR rather than as a list:
    /// no struct may resolve two different spellings of the same field silently. serde
    /// rejects a duplicate rather than letting field order pick a winner.
    #[test]
    fn one_struct_never_silently_picks_between_two_spellings() {
        let dup: Result<IntrospectParams, _> =
            serde_json::from_str(r#"{"class_name":"A.One","class":"B.Two"}"#);
        assert!(
            dup.is_err(),
            "two spellings of class_name must be refused, not resolved by field order"
        );
        let dup: Result<TestParams, _> =
            serde_json::from_str(r#"{"pattern":"A.One","classname":"B.Two"}"#);
        assert!(dup.is_err(), "two spellings of pattern must be refused");
    }

    /// Aliases are a rescue path, not a second contract: the SCHEMA must still advertise
    /// exactly one name, or the alias becomes a competing public contract.
    #[test]
    fn the_schema_advertises_only_the_canonical_name() {
        let schema = serde_json::to_value(schemars::schema_for!(TestParams)).unwrap();
        let props = schema["properties"].as_object().expect("properties");
        assert!(props.contains_key("pattern"));
        for alias in [
            "class",
            "classname",
            "className",
            "class_name",
            "test_class",
        ] {
            assert!(
                !props.contains_key(alias),
                "alias {alias} leaked into the published schema"
            );
        }
    }
}
