//! #263 proposal 2: answer a `PropCollision` on the payload instead of only advising about it.
//!
//! The hint added in #265 tells the caller which table holds the registration. This goes one step
//! further and does the lookup, so the delete argument arrives WITH the failure. The report measured
//! the difference: 29 tool calls when the model had to work it out (including three writes into
//! system globals), 2 when it knew the table. With `stale_registration.delete_id` populated it is
//! one failure plus one `%DeleteId`.
//!
//! The query is the one the MCP already runs at `interop.rs:1572`, widened to the columns the caller
//! needs. Verified live on IRIS for Health 2026.1: `Ens_Config.SearchTableProp` projects an `ID`
//! column whose value is already `<ClassExtent>||<Name>`, i.e. exactly what `%DeleteId` takes.
//!
//! WHAT THIS DELIBERATELY DOES NOT DO
//!
//! It never guesses. A failed query emits NOTHING — an absent field and "we looked and found no
//! row" must not be the same observation, because a broken query would otherwise read as a clean
//! "no stale registration here". Likewise `accused_class_exists` is omitted rather than defaulted
//! when the dictionary probe is undetermined.

use serde_json::Value;

/// What a `PropCollision` message names.
#[derive(Debug, PartialEq, Eq)]
pub struct Collision {
    /// The colliding property name — the only part needed to run the lookup.
    pub prop: String,
    /// The class being compiled: the one that "cannot override".
    pub compiling_class: Option<String>,
    /// The class the error blames for the existing definition. This is the one that is typically
    /// already deleted, which is what makes the raw error so misleading.
    pub accused_class: Option<String>,
}

/// Pull the quoted value that follows `marker`.
fn quoted_after(msg: &str, marker: &str) -> Option<String> {
    let rest = &msg[msg.find(marker)? + marker.len()..];
    let rest = rest.strip_prefix('\'')?;
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// Parse the real message shape:
///
/// ```text
/// ERROR <EnsSearchTable>PropCollision: SearchTable property collision: Property 'PatientFirstName'
/// in class 'HOSPITAL.Search.HL7' cannot override the definition from class
/// 'Hospital.SearchTable.PatientFirstName'
/// ```
///
/// `prop` is required — without it there is nothing to look up. The two class names are optional so
/// a reworded build still yields a usable lookup rather than nothing at all.
pub fn parse(msg: &str) -> Option<Collision> {
    if !msg.contains("PropCollision") {
        return None;
    }
    Some(Collision {
        prop: quoted_after(msg, "Property ")?,
        compiling_class: quoted_after(msg, "in class "),
        accused_class: quoted_after(msg, "from class "),
    })
}

/// The lookup. Single-quote escaped the same way the existing call sites do it.
pub fn stale_registration_sql(prop: &str) -> String {
    format!(
        "SELECT ID, Name, PropId, ClassExtent, ClassDerivation FROM Ens_Config.SearchTableProp \
         WHERE Name = '{}'",
        prop.replace('\'', "''")
    )
}

/// Shape one Atelier row. `ID` is carried through as `delete_id` because that is what it IS.
fn row_to_json(r: &Value) -> Value {
    let mut o = serde_json::Map::new();
    if let Some(id) = r["ID"].as_str() {
        o.insert("delete_id".into(), Value::String(id.to_string()));
    }
    if let Some(v) = r["ClassExtent"].as_str() {
        o.insert("class_extent".into(), Value::String(v.to_string()));
    }
    if let Some(v) = r["ClassDerivation"].as_str() {
        o.insert("class_derivation".into(), Value::String(v.to_string()));
    }
    // PropId arrives as a number on one driver path and a numeric string on another — the same
    // split class_presence documents for IsCompiled. Keep whatever came, untranslated.
    if !r["PropId"].is_null() {
        o.insert("prop_id".into(), r["PropId"].clone());
    }
    Value::Object(o)
}

/// Build the `stale_registration` value from rows the query actually returned.
///
/// `accused_exists` is `None` when the dictionary probe could not decide; the field is then omitted
/// rather than guessed either way.
pub fn build(c: &Collision, rows: &[Value], accused_exists: Option<bool>) -> Value {
    let shaped: Vec<Value> = rows.iter().map(row_to_json).collect();
    let mut o = serde_json::Map::new();
    o.insert("prop".into(), Value::String(c.prop.clone()));
    if let Some(a) = &c.accused_class {
        o.insert("accused_class".into(), Value::String(a.clone()));
    }
    if let Some(e) = accused_exists {
        o.insert("accused_class_exists".into(), Value::Bool(e));
    }
    // Exactly one row is the case the report measured, and then the argument is unambiguous — so
    // lift it to the top level. With several rows the caller MUST choose, and a top-level
    // `delete_id` would be an invitation to delete the wrong extent's registration.
    if shaped.len() == 1 {
        if let Some(id) = shaped[0]["delete_id"].as_str() {
            o.insert("delete_id".into(), Value::String(id.to_string()));
        }
    }
    o.insert("rows".into(), Value::Array(shaped));
    if rows.is_empty() {
        o.insert(
            "note".into(),
            Value::String(
                "No row is registered under this property name, so this is NOT a stale \
                 registration — it is a live collision between two search tables that both declare \
                 it. Reconcile the two declarations instead of deleting anything."
                    .into(),
            ),
        );
    }
    Value::Object(o)
}

/// Run the lookup and attach `stale_registration`. Returns whether it attached anything.
///
/// Silent no-op unless the message really is a `PropCollision`, so the happy path and every other
/// compile error cost nothing — no extra request is made.
pub async fn enrich(
    payload: &mut Value,
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
    msg: &str,
) -> bool {
    let Some(c) = parse(msg) else {
        return false;
    };
    let rows = match iris
        .query(&stale_registration_sql(&c.prop), vec![], namespace, client)
        .await
    {
        // A failed lookup attaches NOTHING. Reporting "no stale row" because the query broke would
        // be worse than staying quiet: the hint alone is still correct and still actionable.
        Err(e) => {
            tracing::debug!("stale_registration lookup failed for {}: {e}", c.prop);
            return false;
        }
        Ok(v) => v["result"]["content"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
    };
    let accused_exists = match &c.accused_class {
        None => None,
        Some(a) => match super::class_presence(iris, client, namespace, a).await {
            super::ClassPresence::Absent => Some(false),
            super::ClassPresence::Compiled | super::ClassPresence::DefinedNotCompiled => Some(true),
            super::ClassPresence::Undetermined => None,
        },
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "stale_registration".into(),
            build(&c, &rows, accused_exists),
        );
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verbatim console line from #263.
    const REAL: &str = "ERROR <EnsSearchTable>PropCollision: SearchTable property collision: \
         Property 'PatientFirstName' in class 'HOSPITAL.Search.HL7' cannot override the definition \
         from class 'Hospital.SearchTable.PatientFirstName'";

    #[test]
    fn the_real_message_yields_all_three_names() {
        let c = parse(REAL).expect("must parse the message the report captured");
        assert_eq!(c.prop, "PatientFirstName");
        assert_eq!(c.compiling_class.as_deref(), Some("HOSPITAL.Search.HL7"));
        // The accused class is the one that no longer exists — NOT the class being compiled.
        // Confusing the two is what sends a reader to %Dictionary for the wrong name.
        assert_eq!(
            c.accused_class.as_deref(),
            Some("Hospital.SearchTable.PatientFirstName")
        );
    }

    #[test]
    fn an_unrelated_compile_error_is_not_parsed_at_all() {
        assert!(parse("ERROR #1026: Invalid command").is_none());
        // and the guard is on the marker, not on the quotes
        assert!(parse("Property 'X' in class 'Y' cannot override").is_none());
    }

    /// Robustness: a build that rewords the class clauses must still give us something to look up.
    #[test]
    fn a_reworded_message_still_yields_the_property() {
        let c = parse("ERROR <EnsSearchTable>PropCollision: Property 'Medication' clashes")
            .expect("the property alone is enough to run the lookup");
        assert_eq!(c.prop, "Medication");
        assert_eq!(c.compiling_class, None);
        assert_eq!(c.accused_class, None);
    }

    /// No property name means no lookup — better nothing than a query on a guess.
    #[test]
    fn a_propcollision_without_a_property_name_is_none() {
        assert!(parse("ERROR <EnsSearchTable>PropCollision: something else entirely").is_none());
    }

    #[test]
    fn the_sql_doubles_a_single_quote() {
        let sql = stale_registration_sql("O'Brien");
        assert!(sql.contains("Name = 'O''Brien'"), "{sql}");
        assert!(
            sql.contains("SELECT ID,"),
            "the ID is the delete argument: {sql}"
        );
    }

    fn row(id: &str, extent: &str) -> Value {
        serde_json::json!({
            "ID": id, "Name": "PatientFirstName", "PropId": 5,
            "ClassExtent": extent,
            "ClassDerivation": "Hospital.SearchTable.PatientFirstName~EnsLib.HL7.SearchTable",
        })
    }

    #[test]
    fn one_row_lifts_the_delete_id_to_the_top() {
        let c = parse(REAL).unwrap();
        let v = build(
            &c,
            &[row(
                "EnsLib.HL7.SearchTable||PatientFirstName",
                "EnsLib.HL7.SearchTable",
            )],
            Some(false),
        );
        assert_eq!(v["delete_id"], "EnsLib.HL7.SearchTable||PatientFirstName");
        assert_eq!(v["accused_class_exists"], false);
        assert_eq!(v["rows"][0]["class_extent"], "EnsLib.HL7.SearchTable");
        assert_eq!(v["rows"][0]["prop_id"], 5);
        assert!(v["note"].is_null(), "a found row is not a note case: {v}");
    }

    /// The hazard the top-level field would create: with two extents registering the same name,
    /// naming one of them invites deleting the wrong registration. The caller must choose.
    #[test]
    fn two_rows_do_not_get_a_top_level_delete_id() {
        let c = parse(REAL).unwrap();
        let v = build(
            &c,
            &[
                row(
                    "EnsLib.HL7.SearchTable||PatientFirstName",
                    "EnsLib.HL7.SearchTable",
                ),
                row(
                    "EnsLib.EDI.X12.SearchTable||PatientFirstName",
                    "EnsLib.EDI.X12.SearchTable",
                ),
            ],
            Some(false),
        );
        assert!(
            v["delete_id"].is_null(),
            "ambiguous: must not name one of two extents: {v}"
        );
        assert_eq!(v["rows"].as_array().unwrap().len(), 2, "{v}");
    }

    /// Zero rows is a REAL and different answer: a live collision between two search tables, which
    /// is the case the BestPractices text describes. It must not read as "stale row, go delete".
    #[test]
    fn zero_rows_says_it_is_not_a_stale_registration() {
        let c = parse(REAL).unwrap();
        let v = build(&c, &[], Some(true));
        assert!(v["delete_id"].is_null(), "{v}");
        assert_eq!(v["rows"].as_array().unwrap().len(), 0, "{v}");
        let note = v["note"].as_str().expect("must explain the empty result");
        assert!(note.contains("NOT a stale"), "{note}");
        assert!(
            note.contains("Reconcile"),
            "must give the different remedy: {note}"
        );
    }

    /// An undetermined dictionary probe is omitted, never defaulted. A false here would assert the
    /// class is gone on the strength of a failed query.
    #[test]
    fn an_undetermined_class_probe_omits_the_field_rather_than_guessing() {
        let c = parse(REAL).unwrap();
        let v = build(&c, &[row("X||PatientFirstName", "X")], None);
        assert!(
            v.get("accused_class_exists").is_none(),
            "must not claim either way: {v}"
        );
        // but the rest is still reported
        assert_eq!(v["delete_id"], "X||PatientFirstName");
    }
}
