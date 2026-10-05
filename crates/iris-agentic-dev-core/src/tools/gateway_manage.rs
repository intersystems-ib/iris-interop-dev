//! `iris_gateway_manage` (#343) — answer "is the SQL Gateway usable, and if not, WHICH of the
//! four things is wrong" in one call instead of eight to fourteen `iris_execute` probes.
//!
//! #343's evidence: `e31-bo-jdbc` — stand up a JDBC Business Operation against PostgreSQL — is the
//! most expensive step in all twelve Sonnet runs of the benchmark, from plugin 1.41 to 1.120.1,
//! without one exception (14–37 min, hitting the 60-turn cap twice two plugin-years apart). A
//! `CLAUDE.md` arm pointing imperatively at `jdbc-sql.md` took 37 minutes anyway: it is the only one
//! of 44 steps where that arm does not win. The minutes go on probing the API by trial and error —
//! two different runs opened with two DIFFERENT classes for the same question, *is the gateway up?*,
//! because there is no canonical answer to ask for.
//!
//! So this tool is not "create a connection". It is the canonical answer, plus a `test` whose
//! verdict NAMES which failure mode it is. From outside, all four look identical.
//!
//! ## What was measured, and what it overturns
//!
//! Against IRIS for Health 2026.1 and PostgreSQL 17.11 through `postgresql-42.7.4.jar`
//! (`e2e/gateway/docker-compose.yaml`), on 2026-09-22. Six deliberately-broken connections plus a
//! good control, each built, tested and deleted on a throwaway instance:
//!
//! | connection | `TestConnection` | `error` byref |
//! |---|---|---|
//! | control (all correct) | 1 | *empty* |
//! | wrong password | 0 | `FATAL: password authentication failed for user "gateway_ro".` |
//! | user that does not exist | 0 | `FATAL: password authentication failed for user "no_such_role".` |
//! | database that does not exist | 0 | `FATAL: database "NoSuchDb" does not exist.` |
//! | port with nothing listening | 0 | `Connection to …:5999 refused. Check that the hostname and port…` |
//! | classpath naming a jar that is not there, WARM gateway | **1** | *empty* |
//! | classpath naming a jar that is not there, COLD gateway | 0 | *empty, length 0* |
//!
//! The last two rows are why this file exists, and both are the defect this repo is named after in
//! `CLAUDE.md` — a failure answered with something shaped like an answer:
//!
//!  * **Warm gateway, missing jar → `TestConnection` reports SUCCESS.** The driver class is already
//!    loaded in the running Java server from an earlier connection, so the classpath is never
//!    consulted. The connection works today and dies at the next gateway restart. A `test` built on
//!    `TestConnection` alone would call that healthy.
//!  * **Cold gateway, missing jar → failure with a ZERO-LENGTH reason.** Controlled: the same cold
//!    gateway with a correct classpath returns 1 and starts the server on demand, so the empty
//!    reason is attributable to the classpath and not to the cold start.
//!
//! Hence the rule this module follows: **classify by mechanism, not by the error string.** Whether
//! the jar exists is answered by `%File.Exists`, not by asking `TestConnection` how it feels.
//!
//! It also corrects #343's first failure mode. "The Java Gateway is not started" is NOT a failure
//! for a SQL Gateway connection: the `%JDBC Server` external language server starts on demand, and
//! the control above proves it (cold server, good connection, verdict 1). What genuinely stops it is
//! Java being absent or unsupported under the server's `JavaHome` — also measured, with a negative
//! control that names the directory it looked in. The `EnsLib.JavaGateway.Service` requirement is
//! real but belongs to the interop ADAPTER path, not to this one, and it is reported as such.

use crate::objectscript::os_str_expr;

/// The external language server a JDBC SQL Gateway connection runs through. `OpenGateway` is the
/// canonical reader for it: it returns the port, the bind address, its own classpath and its
/// `JavaHome`, and it fails with a named error for a server that is not defined. The alternative
/// the benchmark runs reached for — `Config.Gateways` over SQL — is a CPF-section projection that
/// reuses positional columns between sections, so `Address` came back holding a `.NET` version for
/// one row and a jar path for another. Measured; do not query it.
pub const JDBC_SERVER: &str = "%JDBC Server";

/// Actions this tool accepts. Also the advertised `enum` on the `action` field; the two are held
/// equal by `the_advertised_enum_is_the_action_list`.
pub const ACTIONS: &[&str] = &["probe", "list", "test", "create", "delete"];

/// What a call resolved to. `None` is what [`unknown_action_message`] exists for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Probe,
    List,
    Test,
    Create,
    Delete,
}

impl Action {
    /// Does this action CHANGE the instance? The write gate in `tools::mod` derives from this
    /// rather than keeping its own `matches!` over action strings.
    ///
    /// The match is exhaustive on purpose — no `_` arm. A variant added here is a compile error
    /// until someone classifies it, which is the only arrangement under which "dispatched but
    /// missing from the gate's list" cannot happen. The tree already names that hazard at the
    /// `iris_doc` arm of `mutating_call`: a mode added to the enum and dispatched but absent from a
    /// duplicated list would be an UNGATED WRITE.
    pub fn is_write(self) -> bool {
        match self {
            // Reads a definition, checks files, opens a connection read-only. No change.
            Self::Probe | Self::List | Self::Test => false,
            Self::Create | Self::Delete => true,
        }
    }
}

/// The ONE place an action string becomes a decision. The handler dispatches on this rather than on
/// its own `match` over literals, so "advertised" and "accepted" cannot be two lists that drift —
/// `the_advertised_enum_is_the_action_list` runs the mapping for every advertised value.
pub fn parse_action(raw: &str) -> Option<Action> {
    match raw.trim().to_lowercase().as_str() {
        "probe" => Some(Action::Probe),
        "list" => Some(Action::List),
        "test" => Some(Action::Test),
        "create" => Some(Action::Create),
        "delete" => Some(Action::Delete),
        _ => None,
    }
}

/// Rows `list` will return before it says it truncated. A namespace has a handful of gateway
/// connections, not thousands.
pub const LIST_LIMIT: usize = 200;

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Shared value readers
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Read a JSON value as a boolean whatever shape it arrived in.
///
/// `isJDBC` is a `%Library.Boolean`, and the two paths in this file read it through two different
/// transports: Atelier's `/action/query` hands it over as JSON `true`, while `%ToJSON()` of a
/// `%DynamicObject` set from a SQL logical hands over the number `1`. A reader that knows only one
/// shape is the sibling defect that once broke `iris_execute_method` outright — it parsed `1`/`"1"`
/// and fell over when `%Dictionary` gave it `true`.
///
/// `None` means the field was absent or of a shape that carries no boolean — NOT `false`.
pub fn json_bool(v: Option<&serde_json::Value>) -> Option<bool> {
    match v {
        Some(serde_json::Value::Bool(b)) => Some(*b),
        Some(serde_json::Value::Number(n)) => Some(n.as_f64().unwrap_or(0.0) != 0.0),
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            if t.eq_ignore_ascii_case("true") || t == "1" {
                Some(true)
            } else if t.eq_ignore_ascii_case("false") || t == "0" || t.is_empty() {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A string field, trimmed. Absent and empty are the same thing for every field this reads.
fn json_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn json_strings(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Facts: what IRIS said, before any judgement is applied
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Whether Java itself can run under the server's `JavaHome`. Three cases: it is there, it is not,
/// or we could not find out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavaFacts {
    /// `GetJavaVersion` answered. `supported` is `CheckJavaVersionSupported`'s verdict.
    Found { version: String, supported: bool },
    /// `GetJavaVersion` failed, carrying the directory it looked in.
    Absent { detail: String },
    /// The stage did not run — there is no server definition to take a `JavaHome` from.
    NotChecked,
}

/// What the `%JDBC Server` external language server looks like right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerFacts {
    Defined {
        name: String,
        port: String,
        address: String,
        /// The SERVER's own classpath, which is separate from any connection's.
        server_classpath: String,
        /// Listening at this instant. NOT a health verdict: measured, a cold server starts on
        /// demand and the connection still succeeds.
        listening: bool,
        /// Why `IsGatewayRunning` said no, when it said no.
        listen_detail: String,
        java: JavaFacts,
    },
    /// `OpenGateway` could not produce a definition, carrying its own error.
    NotDefined { detail: String },
}

/// What the named gateway connection looks like, and how far a connection attempt got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionFacts {
    pub name: String,
    /// Is there a row for this name? `None` means the program did not say — which must not be read
    /// as "there is no such connection", the exact substitution this repo is named after. Carried
    /// as its own field rather than inferred from four empty strings, because an inference is a
    /// guess dressed as a measurement and a connection CAN legitimately have every one of those
    /// fields blank.
    pub defined: Option<bool>,
    pub driver: String,
    pub url: String,
    pub dsn: String,
    pub user: String,
    pub is_jdbc: Option<bool>,
    pub classpath_entries: Vec<String>,
    /// Entries `%File.Exists` said are not there. Empty means every named entry exists.
    pub classpath_missing: Vec<String>,
    /// `$SYSTEM.SQLGateway.TestConnection`'s verdict.
    pub test_ok: bool,
    /// Its `error` byref, verbatim. Measured to be EMPTY for a missing jar on a cold server.
    pub test_error: String,
    /// `Some(true)` when the JDBC metadata handshake completed; `None` when the stage did not run.
    pub meta_ok: Option<bool>,
    pub meta_error: String,
    pub database: String,
    pub driver_name: String,
    pub driver_version: String,
    /// `None` when no probe statement was asked for.
    pub probe_ok: Option<bool>,
    pub probe_error: String,
    pub probe_rows: u64,
}

/// Everything one `test`/`probe` program reported. `Unavailable` is the third case the whole
/// codebase turns on: the program did not produce a verdict, so nothing below may be believed.
///
/// `Read` is boxed because it is an order of magnitude larger than `Unavailable`, and a value of
/// this type is produced once per tool call — the indirection costs nothing here and keeps the
/// failure arm from carrying the success arm's footprint everywhere it is moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayFacts {
    Read {
        server: Box<ServerFacts>,
        connection: Option<Box<ConnectionFacts>>,
    },
    Unavailable {
        detail: String,
    },
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The diagnosis
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// One named failure mode, or health. #343's whole ask: these four look identical from outside, and
/// collapsing them into "connection failed" is what costs 14–37 minutes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayDiagnosis {
    /// No verdict to read — the diagnosis itself could not be made.
    Inconclusive { stage: &'static str, detail: String },
    /// Mode 1, the half that is real: Java is not runnable, so the server cannot start on demand.
    JavaAbsent { detail: String },
    /// Mode 1: Java runs but this version is refused.
    JavaUnsupported { version: String, detail: String },
    /// Mode 1: there is no `%JDBC Server` definition at all.
    ServerNotDefined { detail: String },
    /// The connection name is not on this instance.
    ConnectionNotDefined { connection: String },
    /// An ODBC DSN connection. Every Java-side finding above is irrelevant to it, and saying so
    /// beats reporting a Java diagnosis that cannot apply.
    NotJdbc { connection: String },
    /// Mode 2: the classpath names files that are not on the IRIS host. Ranked ABOVE the connection
    /// verdict on purpose — measured, a warm gateway makes this connection test PASS.
    ClasspathMissing {
        connection: String,
        missing: Vec<String>,
        /// Whether `TestConnection` passed in spite of it. `true` is the dangerous case.
        test_passed_anyway: bool,
    },
    /// The connection failed and reported NOTHING. Measured signature of a cold server that could
    /// not load the driver.
    ConnectFailedWithoutReason { connection: String },
    /// Mode 3: the target refused the credential. The message is the target's own.
    CredentialRejected {
        connection: String,
        user: String,
        iris_error: String,
    },
    /// The connection failed for a reason the target named that is not credential-shaped.
    ConnectFailed {
        connection: String,
        iris_error: String,
    },
    /// The connection test passed but the JDBC handshake did not complete.
    HandshakeFailed {
        connection: String,
        iris_error: String,
    },
    /// Mode 4, the expensive one: the gateway WORKS and the target refused the statement.
    TargetRejectedStatement {
        connection: String,
        database: String,
        iris_error: String,
    },
    /// Everything asked for succeeded.
    Healthy {
        connection: String,
        database: String,
        driver: String,
    },
}

/// Does this message look like the target refusing a credential?
///
/// A HINT, not the verdict: the text is the remote engine's, and this module has measured exactly
/// one engine. It only ever chooses between two variants that BOTH carry the message verbatim, so
/// being wrong costs a heading and never hides the reason.
fn looks_like_credential_refusal(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["password authentication failed", "authentication failed"]
        .iter()
        .any(|needle| m.contains(needle))
}

/// Turn the facts into one named mode. The ORDER is the design: each stage is asked only once the
/// stage that would explain it away has been ruled out.
pub fn diagnose(facts: &GatewayFacts) -> GatewayDiagnosis {
    let (server, connection) = match facts {
        GatewayFacts::Unavailable { detail } => {
            return GatewayDiagnosis::Inconclusive {
                stage: "read",
                detail: detail.clone(),
            }
        }
        GatewayFacts::Read { server, connection } => (server, connection),
    };

    // The Java side first: nothing downstream can be believed if the runtime cannot start.
    match &**server {
        ServerFacts::NotDefined { detail } => {
            return GatewayDiagnosis::ServerNotDefined {
                detail: detail.clone(),
            }
        }
        ServerFacts::Defined { java, .. } => match java {
            JavaFacts::Absent { detail } => {
                return GatewayDiagnosis::JavaAbsent {
                    detail: detail.clone(),
                }
            }
            JavaFacts::Found { version, supported } if !supported => {
                return GatewayDiagnosis::JavaUnsupported {
                    version: version.clone(),
                    detail: String::new(),
                }
            }
            _ => {}
        },
    }

    let Some(c) = connection else {
        // `probe` alone: the Java side is sound and no connection was named.
        return GatewayDiagnosis::Healthy {
            connection: String::new(),
            database: String::new(),
            driver: String::new(),
        };
    };

    match c.defined {
        None => {
            return GatewayDiagnosis::Inconclusive {
                stage: "connection",
                detail: format!(
                    "the program did not say whether '{}' is defined on this instance, so whether \
                     it exists is unknown — that is not the same as its not existing",
                    c.name
                ),
            }
        }
        Some(false) => {
            return GatewayDiagnosis::ConnectionNotDefined {
                connection: c.name.clone(),
            }
        }
        Some(true) => {}
    }
    if c.is_jdbc == Some(false) {
        return GatewayDiagnosis::NotJdbc {
            connection: c.name.clone(),
        };
    }
    if !c.classpath_missing.is_empty() {
        return GatewayDiagnosis::ClasspathMissing {
            connection: c.name.clone(),
            missing: c.classpath_missing.clone(),
            test_passed_anyway: c.test_ok,
        };
    }
    if !c.test_ok {
        if c.test_error.is_empty() {
            return GatewayDiagnosis::ConnectFailedWithoutReason {
                connection: c.name.clone(),
            };
        }
        if looks_like_credential_refusal(&c.test_error) {
            return GatewayDiagnosis::CredentialRejected {
                connection: c.name.clone(),
                user: c.user.clone(),
                iris_error: c.test_error.clone(),
            };
        }
        return GatewayDiagnosis::ConnectFailed {
            connection: c.name.clone(),
            iris_error: c.test_error.clone(),
        };
    }
    if c.meta_ok == Some(false) {
        return GatewayDiagnosis::HandshakeFailed {
            connection: c.name.clone(),
            iris_error: c.meta_error.clone(),
        };
    }
    if c.probe_ok == Some(false) {
        return GatewayDiagnosis::TargetRejectedStatement {
            connection: c.name.clone(),
            database: c.database.clone(),
            iris_error: c.probe_error.clone(),
        };
    }
    GatewayDiagnosis::Healthy {
        connection: c.name.clone(),
        database: c.database.clone(),
        driver: if c.driver_version.is_empty() {
            c.driver_name.clone()
        } else {
            format!("{} {}", c.driver_name, c.driver_version)
        },
    }
}

impl GatewayDiagnosis {
    /// The short machine-readable mode name, for the envelope.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Inconclusive { .. } => "GATEWAY_DIAGNOSIS_INCONCLUSIVE",
            Self::JavaAbsent { .. } => "GATEWAY_JAVA_ABSENT",
            Self::JavaUnsupported { .. } => "GATEWAY_JAVA_UNSUPPORTED",
            Self::ServerNotDefined { .. } => "GATEWAY_SERVER_NOT_DEFINED",
            Self::ConnectionNotDefined { .. } => "GATEWAY_CONNECTION_NOT_DEFINED",
            Self::NotJdbc { .. } => "GATEWAY_CONNECTION_NOT_JDBC",
            Self::ClasspathMissing { .. } => "GATEWAY_CLASSPATH_MISSING",
            Self::ConnectFailedWithoutReason { .. } => "GATEWAY_CONNECT_FAILED_NO_REASON",
            Self::CredentialRejected { .. } => "GATEWAY_CREDENTIAL_REJECTED",
            Self::ConnectFailed { .. } => "GATEWAY_CONNECT_FAILED",
            Self::HandshakeFailed { .. } => "GATEWAY_HANDSHAKE_FAILED",
            Self::TargetRejectedStatement { .. } => "GATEWAY_TARGET_REJECTED_STATEMENT",
            Self::Healthy { .. } => "GATEWAY_OK",
        }
    }

    /// Is this a working gateway?
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy { .. })
    }

    /// What to do about it. One remedy per mode — never one sentence covering four.
    pub fn remedy(&self) -> String {
        match self {
            Self::Inconclusive { stage, detail } => format!(
                "the '{stage}' stage returned output that is not a verdict, so WHICH of the failure \
                 modes this is was not established — treat nothing below as measured. IRIS wrote: \
                 {detail}"
            ),
            Self::JavaAbsent { detail } => format!(
                "Java is not runnable for the {JDBC_SERVER} external language server, so it cannot \
                 start and no JDBC gateway connection can work. IRIS reported: {detail}. Install a \
                 supported JDK in the IRIS container, or point the server's Java Home at one \
                 (System Administration > Configuration > Connectivity > External Language \
                 Servers > {JDBC_SERVER})."
            ),
            Self::JavaUnsupported { version, detail } => format!(
                "Java {version} is present but this IRIS version does not support it for the \
                 {JDBC_SERVER} external language server. {detail} Install a supported JDK and set \
                 the server's Java Home to it."
            ),
            Self::ServerNotDefined { detail } => format!(
                "this instance has no '{JDBC_SERVER}' external language server defined, which is \
                 the server every JDBC SQL Gateway connection runs through. IRIS reported: \
                 {detail}. It is normally present out of the box — check System Administration > \
                 Configuration > Connectivity > External Language Servers."
            ),
            Self::ConnectionNotDefined { connection } => {
                crate::tools::gateway::connection_not_defined_message(connection)
            }
            Self::NotJdbc { connection } => format!(
                "SQL Gateway connection '{connection}' is an ODBC DSN connection, not a JDBC one, \
                 so the Java findings here do not apply to it and no Java diagnosis was made. \
                 iris_gateway_query reads through JDBC connections."
            ),
            Self::ClasspathMissing {
                connection,
                missing,
                test_passed_anyway,
            } => {
                let list = missing.join(", ");
                let warm = if *test_passed_anyway {
                    " The connection test PASSED anyway, and that is the trap: the driver class is \
                     already loaded into the running Java server from an earlier connection, so \
                     the classpath was never consulted. This connection works until the gateway \
                     restarts, and then stops — measured."
                } else {
                    " The connection test also failed, and on a cold Java server it fails with an \
                     EMPTY reason — measured — which is why the file check above, not the test, is \
                     what names this."
                };
                format!(
                    "SQL Gateway connection '{connection}' names classpath entries that do not \
                     exist on the IRIS host: {list}.{warm} Put the driver jar where IRIS can read \
                     it (inside the container, not on the client host) and set the connection's \
                     Class Path to that path."
                )
            }
            Self::ConnectFailedWithoutReason { connection } => format!(
                "SQL Gateway connection '{connection}' failed to connect and IRIS reported NO \
                 reason at all — a zero-length error, not a message this tool dropped. Every \
                 classpath entry it names does exist, so the usual cause is ruled out. What is \
                 left: the driver class name is wrong for the jar that is present, the Java server \
                 could not start (check its log under the instance's mgr directory), or its port \
                 is taken. Run action=probe to see the server's port and Java version."
            ),
            Self::CredentialRejected {
                connection,
                user,
                iris_error,
            } => {
                let who = if user.is_empty() {
                    "no username is stored on the connection".to_string()
                } else {
                    format!("the connection connects as '{user}'")
                };
                format!(
                    "the TARGET database refused the credential stored on SQL Gateway connection \
                     '{connection}' — {who}. This is the remote database's own message, passed \
                     through unchanged: {iris_error}. Fix the username or password in the gateway \
                     definition (System Administration > Configuration > Connectivity > SQL \
                     Gateway Connections); this tool never accepts a credential, so it cannot be \
                     passed in here. If a JDBC Business Operation is what is failing, note that \
                     its Credentials setting is a SEPARATE interop credential — list those with \
                     iris_credential_list."
                )
            }
            Self::ConnectFailed {
                connection,
                iris_error,
            } => format!(
                "SQL Gateway connection '{connection}' is defined, its classpath entries all \
                 exist, and the connection attempt still failed. This is the reason the target or \
                 the driver gave, passed through unchanged: {iris_error}. Read it as a statement \
                 about the TARGET — a host or port that refuses, a database name that does not \
                 exist — not about IRIS."
            ),
            Self::HandshakeFailed {
                connection,
                iris_error,
            } => format!(
                "the connection test for '{connection}' passed but reading the JDBC metadata \
                 failed, so the connection is NOT usable even though the test said otherwise. \
                 IRIS reported: {iris_error}"
            ),
            Self::TargetRejectedStatement {
                connection,
                database,
                iris_error,
            } => format!(
                "the gateway WORKS — '{connection}' connected to {database} and the driver \
                 handshake completed — and the target refused the statement. This is the most \
                 expensive failure to misread: nothing is wrong with the gateway, the classpath, \
                 the credential or the Java server, so do not go looking there. This is the \
                 target's own message: {iris_error}. A data-type complaint here (a date, a \
                 timestamp, a numeric precision) means a value is being sent unconverted — fix it \
                 in the transformation that builds it, not in the connection."
            ),
            Self::Healthy {
                connection,
                database,
                driver,
            } => {
                if connection.is_empty() {
                    format!(
                        "the {JDBC_SERVER} external language server is defined and its Java \
                         runtime is present and supported, so a JDBC SQL Gateway connection can \
                         run. Name a connection to test one end to end."
                    )
                } else {
                    format!(
                        "'{connection}' reached {database} through {driver} and every stage \
                         asked for succeeded. A JDBC Business Operation over this connection \
                         still needs its own JGService setting — see the note below."
                    )
                }
            }
        }
    }
}

/// What to say for an action this tool does not have.
pub fn unknown_action_message(action: &str) -> String {
    format!(
        "'{action}' is not an action of iris_gateway_manage. Valid actions: {}.",
        ACTIONS.join(", ")
    )
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Keeping the password in
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// What replaces a secret anywhere it would otherwise be rendered.
pub const REDACTED: &str = "[redacted]";

/// The ObjectScript form of a secret, without the outer quotes `os_str_expr` adds.
///
/// This is the shape the password actually takes inside the generated program, and it is NOT the
/// raw password: `os_str_expr` doubles a `"` and splices non-ASCII as `$CHAR(n)`. For `a"b` the
/// program holds `a""b`; for `ñ` it holds `$CHAR(241)` with no quotes at all, which is why the
/// strip is conditional rather than assumed.
fn program_form(secret: &str) -> String {
    let rendered = os_str_expr(secret);
    match rendered.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        // A single-part rendering: `"abc"` → `abc`.
        Some(inner) if !inner.contains('"') || inner.contains("\"\"") => inner.to_string(),
        // A rendering with no outer quotes at all (`$CHAR(241)`), or one whose strip would be
        // ambiguous. Use it whole: over-matching is visible, under-matching is not.
        _ => rendered,
    }
}

/// Remove `secret` from text that is about to leave this process.
///
/// `create` has to put the password into generated ObjectScript — there is no other way to set
/// `%Library.SQLConnection.pwd` (see the note on the `password` parameter). So the password exists,
/// briefly, inside strings this process holds, and the question is not *whether* it is there but
/// whether any path can render it back out. Two defences, and this is the second:
///
///  1. No envelope on the `create` path carries the generated program, IRIS's echo of it, or the
///     call's parameters. That is the one that should hold.
///  2. Every string that reaches a `create` envelope goes through THIS function first. It exists
///     because defence 1 is a property of code that will be edited by people who do not know a
///     password is in scope, and a leak is not the kind of mistake that gets a second chance.
///
/// TWO passes, and each is necessary — this used to be three, and a mutation disabling any ONE of
/// them survived, because for every input tested the three covered each other. Each pass now has an
/// input that only it catches:
///
/// | pass | the copy it removes | witness |
/// |---|---|---|
/// | raw | the password quoted as a VALUE in a message | `Contraseña` — the program holds `$CHAR(241)`, so the program form does not match a message quoting the plain word |
/// | program form | the password as it appears in the generated source | `a"b` — the program holds `a""b`, which the raw form does not match |
///
/// An empty secret is a no-op. It MUST be: `str::replace` with an empty pattern inserts the marker
/// between every character, which would mangle every message on a call that supplied no password.
pub fn redact_secret(text: &str, secret: &str) -> String {
    let secret = secret.trim();
    if secret.is_empty() {
        return text.to_string();
    }
    // Program form first: it is at least as long as the raw form, so the raw pass cannot chop a
    // program-form occurrence in half and leave a fragment behind.
    let out = text.replace(&program_form(secret), REDACTED);
    out.replace(secret, REDACTED)
}

/// A password that can only leave this process redacted.
///
/// The point is the `Debug` and `Display` impls: the commonest way a secret escapes is not a missing
/// scrub but a `{:?}` of the struct that holds it, in a log line or an error someone adds later. A
/// newtype whose formatting is redacted makes that impossible rather than merely discouraged, and
/// the one method that yields the real bytes is named so that reading it is a decision.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// The real bytes. ONE caller: the generated-program builder. Named to be conspicuous.
    fn expose_for_program(&self) -> &str {
        &self.0
    }

    /// Remove this secret from text that is about to leave the process.
    pub fn scrub(&self, text: &str) -> String {
        redact_secret(text, &self.0)
    }
}

// Both impls, not just Debug: `{}` and `{:?}` are equally easy to reach for, and a type that is safe
// under one and not the other is a trap rather than a guarantee.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Generated ObjectScript
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// The Java-side stage: is there a `%JDBC Server`, is it listening, can Java run.
///
/// Every key that contains an underscore is written with `%Set`, never with dot syntax. `_` is
/// ObjectScript's CONCATENATION operator, so `set tSrv.java_found = 1` parses as
/// `tSrv.java` _ `found` and aborts with `<SYNTAX>` at run time — measured against the live
/// instance while writing this, on the first program that tried it. The tool's own envelope caught
/// it honestly (`ok:0`, so [`parse_facts`] returned `Unavailable`), which is exactly why a unit test
/// could not have found it: the output shape was correct and the program did nothing.
/// `no_dynamic_property_name_carries_an_underscore` is the guard.
fn server_stage_code() -> String {
    format!(
        r#"    set tSrv = ##class(%DynamicObject).%New()
    set gw = ""
    set gsc = ##class(%Net.Remote.Service).OpenGateway({server}, .gw)
    if '$ISOBJECT(gw) {{
        do tSrv.%Set("defined", 0)
        do tSrv.%Set("detail", $EXTRACT($SELECT($SYSTEM.Status.IsError(gsc):$SYSTEM.Status.GetErrorText(gsc), 1:"OpenGateway returned no definition and no error"), 1, 300))
    }} else {{
        do tSrv.%Set("defined", 1)
        do tSrv.%Set("name", gw.Name)
        do tSrv.%Set("port", gw.Port)
        do tSrv.%Set("address", gw.BindToIPAddress)
        do tSrv.%Set("server_classpath", gw.ClassPath)
        set psc = ""
        do tSrv.%Set("listening", $SELECT(##class(%Net.Remote.Service).IsGatewayRunning(gw.BindToIPAddress, gw.Port, 0, .psc, 1)=1:1, 1:0))
        do tSrv.%Set("listen_detail", $EXTRACT($SELECT($SYSTEM.Status.IsError(psc):$SYSTEM.Status.GetErrorText(psc), 1:""), 1, 300))
        set jver = "", jvs = ""
        set jsc = ##class(%Net.Remote.Service).GetJavaVersion(gw.JavaHome, .jver, .jvs)
        if $SYSTEM.Status.IsError(jsc) {{
            do tSrv.%Set("java_found", 0)
            do tSrv.%Set("java_detail", $EXTRACT($SYSTEM.Status.GetErrorText(jsc), 1, 300))
        }} else {{
            do tSrv.%Set("java_found", 1)
            do tSrv.%Set("java_version", jvs)
            set ssc = ##class(%Net.Remote.Service).CheckJavaVersionSupported(jver)
            do tSrv.%Set("java_supported", $SELECT($SYSTEM.Status.IsError(ssc):0, 1:1))
            do tSrv.%Set("java_detail", $EXTRACT($SELECT($SYSTEM.Status.IsError(ssc):$SYSTEM.Status.GetErrorText(ssc), 1:""), 1, 300))
        }}
    }}
    do tOut.%Set("server", tSrv)"#,
        server = os_str_expr(JDBC_SERVER)
    )
}

/// `action=probe`: the canonical answer to "is the Java gateway able to run", and nothing else.
pub fn build_probe_code() -> String {
    format!(
        r#"set tOut = ##class(%DynamicObject).%New()
do tOut.%Set("ok", 0)
try {{
{server}
    do tOut.%Set("ok", 1)
}} catch e {{
    do tOut.%Set("ok", 0)
    do tOut.%Set("error", $EXTRACT(e.DisplayString(), 1, 600))
}}
write tOut.%ToJSON()"#,
        server = server_stage_code()
    )
}

/// `action=test`: the Java stage, then the connection row, then the file check, then the timed
/// connection attempt, then the driver handshake, then the caller's probe statement.
///
/// One program, because the point of #343 is to replace eight to fourteen round trips with one.
pub fn build_test_code(connection: &str, probe_query: Option<&str>) -> String {
    let probe = match probe_query {
        None => String::new(),
        Some(sql) => format!(
            r#"
                    do tConn.%Set("probe_ran", 1)
                    try {{
                        set stmt = conn.CreateStatement()
                        set prs = stmt.ExecuteQuery({sql})
                        set pn = 0
                        while prs.Next() {{
                            set pn = pn + 1
                            if pn '< 1 {{ quit }}
                        }}
                        do tConn.%Set("probe_ok", 1)
                        do tConn.%Set("probe_rows", pn)
                    }} catch pe {{
                        do tConn.%Set("probe_ok", 0)
                        do tConn.%Set("probe_error", $EXTRACT(pe.DisplayString(), 1, 600))
                    }}"#,
            sql = os_str_expr(sql)
        ),
    };
    format!(
        r#"set tOut = ##class(%DynamicObject).%New()
do tOut.%Set("ok", 0)
try {{
{server}
    set tConn = ##class(%DynamicObject).%New()
    do tConn.%Set("name", {name})
    set tMiss = ##class(%DynamicArray).%New()
    set tCp = ##class(%DynamicArray).%New()
    set crs = ##class(%SQL.Statement).%ExecDirect(, "SELECT classpath, driver, URL, DSN, Usr, isJDBC FROM %Library.sys_SQLConnection WHERE Connection_Name = ?", {name})
    if crs.%Next() {{
        do tConn.%Set("defined", 1)
        set cp = crs.%GetData(1)
        do tConn.%Set("classpath", cp)
        do tConn.%Set("driver", crs.%GetData(2))
        do tConn.%Set("url", crs.%GetData(3))
        do tConn.%Set("dsn", crs.%GetData(4))
        do tConn.%Set("user", crs.%GetData(5))
        do tConn.%Set("is_jdbc", $SELECT(+crs.%GetData(6)=1:1, 1:0))
        set sep = $SELECT($SYSTEM.Version.GetOS()="Windows":";", 1:":")
        for i=1:1:$LENGTH(cp, sep) {{
            set one = $ZSTRIP($PIECE(cp, sep, i), "<>W")
            if one '= "" {{
                do tCp.%Push(one)
                if '##class(%File).Exists(one) {{ do tMiss.%Push(one) }}
            }}
        }}
    }} else {{
        do tConn.%Set("defined", 0)
    }}
    do tConn.%Set("classpath_entries", tCp)
    do tConn.%Set("classpath_missing", tMiss)
    if crs.%SQLCODE < 0 {{
        do tOut.%Set("ok", 0)
        do tOut.%Set("error", "reading the gateway connection failed: SQLCODE " _ crs.%SQLCODE _ " " _ crs.%Message)
        do tOut.%Set("connection", tConn)
        quit
    }}
    if tConn.%Get("defined") {{
        set terr = ""
        set tok = $SYSTEM.SQLGateway.TestConnection({name}, 10, 0, .terr)
        do tConn.%Set("test_ok", $SELECT(+tok=1:1, 1:0))
        do tConn.%Set("test_error", $EXTRACT($PIECE(terr, $CHAR(13), 1), 1, 400))
        if tConn.%Get("test_ok") {{
            try {{
                set conn = ##class(%XDBC.Gateway.JDBC.Connection).GetConnection({name})
                if '$ISOBJECT(conn) {{
                    do tConn.%Set("meta_ok", 0)
                    do tConn.%Set("meta_error", "GetConnection returned no object for this connection name")
                }} else {{
                    do conn.SetReadOnly(1)
                    set md = conn.GetMetaData()
                    do tConn.%Set("database", md.GetDatabaseProductNameAndVersion())
                    do tConn.%Set("driver_name", md.GetDriverName())
                    do tConn.%Set("driver_version", md.GetDriverVersion())
                    do tConn.%Set("meta_ok", 1){probe}
                    do conn.Close()
                }}
            }} catch me {{
                do tConn.%Set("meta_ok", 0)
                do tConn.%Set("meta_error", $EXTRACT(me.DisplayString(), 1, 600))
            }}
        }}
    }}
    do tOut.%Set("connection", tConn)
    do tOut.%Set("ok", 1)
}} catch e {{
    do tOut.%Set("ok", 0)
    do tOut.%Set("error", $EXTRACT(e.DisplayString(), 1, 600))
}}
write tOut.%ToJSON()"#,
        server = server_stage_code(),
        name = os_str_expr(connection),
        probe = probe
    )
}

/// The `list` SELECT. Read-only SQL over the projection, so `list` needs no generated code and
/// writes nothing to the instance.
///
/// What `create` needs to build a JDBC SQL Gateway connection. Field names are
/// `%Library.SQLConnection`'s own, because that is the class being populated.
#[derive(Debug)]
pub struct NewConnection {
    pub name: String,
    pub url: String,
    pub driver: String,
    pub classpath: String,
    pub user: String,
    /// A [`Secret`], so a `{:?}` of this struct cannot print it.
    pub password: Secret,
    pub properties: String,
    pub on_connect_statement: String,
}

/// `action=create`. Refuses to overwrite: an existing name comes back as `exists`, never as a
/// silent replacement of somebody else's connection.
///
/// `%Library.SQLConnection` has no SQL projection to INSERT through — `%Library.sys_SQLConnection`
/// is a read-only projection, and the class itself has a numeric IDKEY — so this is object access
/// from generated ObjectScript, which is why the password has to be in the program text at all. See
/// [`redact_secret`].
pub fn build_create_code(c: &NewConnection) -> String {
    format!(
        r#"set tOut = ##class(%DynamicObject).%New()
do tOut.%Set("ok", 0)
try {{
    set crs = ##class(%SQL.Statement).%ExecDirect(, "SELECT ID FROM %Library.sys_SQLConnection WHERE Connection_Name = ?", {name})
    set tSeen = crs.%Next()
    if crs.%SQLCODE < 0 {{
        do tOut.%Set("error", "could not check whether the connection already exists: SQLCODE " _ crs.%SQLCODE _ " " _ crs.%Message)
        quit
    }}
    if tSeen {{
        do tOut.%Set("ok", 1)
        do tOut.%Set("exists", 1)
        quit
    }}
    set conn = ##class(%Library.SQLConnection).%New()
    set conn.Name = {name}
    set conn.isJDBC = 1
    set conn.driver = {driver}
    set conn.classpath = {classpath}
    set conn.URL = {url}
    set conn.Usr = {user}
    set conn.pwd = {pwd}
    set conn.properties = {properties}
    set conn.OnConnectStatement = {oncon}
    set sc = conn.%Save()
    if $SYSTEM.Status.IsError(sc) {{
        do tOut.%Set("error", $EXTRACT($SYSTEM.Status.GetErrorText(sc), 1, 600))
        quit
    }}
    kill conn
    set vrs = ##class(%SQL.Statement).%ExecDirect(, "SELECT ID FROM %Library.sys_SQLConnection WHERE Connection_Name = ?", {name})
    set tBack = vrs.%Next()
    if vrs.%SQLCODE < 0 {{
        do tOut.%Set("error", "the connection was saved but could not be read back: SQLCODE " _ vrs.%SQLCODE _ " " _ vrs.%Message)
        quit
    }}
    if 'tBack {{
        do tOut.%Set("error", "%Save() reported success but no connection with this name can be read back, so it was NOT created")
        quit
    }}
    do tOut.%Set("exists", 0)
    do tOut.%Set("ok", 1)
}} catch e {{
    do tOut.%Set("ok", 0)
    do tOut.%Set("error", $EXTRACT(e.DisplayString(), 1, 600))
}}
write tOut.%ToJSON()"#,
        name = os_str_expr(&c.name),
        driver = os_str_expr(&c.driver),
        classpath = os_str_expr(&c.classpath),
        url = os_str_expr(&c.url),
        user = os_str_expr(&c.user),
        pwd = os_str_expr(c.password.expose_for_program()),
        properties = os_str_expr(&c.properties),
        oncon = os_str_expr(&c.on_connect_statement),
    )
}

/// `action=delete`.
///
/// Three facts have to come back separately, and the reason is the house rule: a failed delete must
/// never read like a success, and an absence must never be reported unless it was established.
///
///  * `found` — whether a row was there BEFORE. Read first, and a failed read aborts rather than
///    answering "not found".
///  * `removed` — whether it is gone AFTER. Re-read rather than trusting the status: a `%DeleteId`
///    that reports success and leaves the row is exactly the shape this codebase keeps producing.
pub fn build_delete_code(connection: &str) -> String {
    format!(
        r#"set tOut = ##class(%DynamicObject).%New()
do tOut.%Set("ok", 0)
try {{
    set crs = ##class(%SQL.Statement).%ExecDirect(, "SELECT ID FROM %Library.sys_SQLConnection WHERE Connection_Name = ?", {name})
    set tSeen = crs.%Next()
    set tId = ""
    if tSeen {{ set tId = crs.%GetData(1) }}
    if crs.%SQLCODE < 0 {{
        do tOut.%Set("error", "could not read the connection, so whether it exists is unknown and nothing was deleted: SQLCODE " _ crs.%SQLCODE _ " " _ crs.%Message)
        quit
    }}
    if 'tSeen {{
        do tOut.%Set("found", 0)
        do tOut.%Set("removed", 0)
        do tOut.%Set("ok", 1)
        quit
    }}
    do tOut.%Set("found", 1)
    set sc = ##class(%Library.SQLConnection).%DeleteId(tId)
    do tOut.%Set("delete_status", $EXTRACT($SELECT($SYSTEM.Status.IsError(sc):$SYSTEM.Status.GetErrorText(sc), 1:""), 1, 600))
    set vrs = ##class(%SQL.Statement).%ExecDirect(, "SELECT ID FROM %Library.sys_SQLConnection WHERE Connection_Name = ?", {name})
    set tStill = vrs.%Next()
    if vrs.%SQLCODE < 0 {{
        do tOut.%Set("error", "the delete was attempted but the connection could not be read back, so whether it is gone is unknown: SQLCODE " _ vrs.%SQLCODE _ " " _ vrs.%Message)
        quit
    }}
    do tOut.%Set("removed", $SELECT(tStill:0, 1:1))
    do tOut.%Set("ok", 1)
}} catch e {{
    do tOut.%Set("ok", 0)
    do tOut.%Set("error", $EXTRACT(e.DisplayString(), 1, 600))
}}
write tOut.%ToJSON()"#,
        name = os_str_expr(connection)
    )
}

/// `pwd` is a `%CSP.Util.Passwd` and `Secret` names an entry in the secure store; NEITHER is
/// projected here. A gateway listing must not be a route to a credential.
pub fn list_sql() -> String {
    format!(
        "SELECT TOP {limit} Connection_Name, driver, classpath, URL, DSN, Usr, isJDBC, \
         OnConnectStatement FROM %Library.sys_SQLConnection ORDER BY Connection_Name",
        limit = LIST_LIMIT + 1
    )
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Parsing
// ─────────────────────────────────────────────────────────────────────────────────────────────

fn parse_server(v: &serde_json::Value) -> ServerFacts {
    let s = match v.get("server") {
        Some(s) => s,
        None => {
            return ServerFacts::NotDefined {
                detail: "the program reported no external language server stage at all".to_string(),
            }
        }
    };
    if json_bool(s.get("defined")) != Some(true) {
        let detail = json_str(s, "detail");
        return ServerFacts::NotDefined {
            detail: if detail.is_empty() {
                "no definition and no reason given".to_string()
            } else {
                detail
            },
        };
    }
    let java = match json_bool(s.get("java_found")) {
        Some(true) => JavaFacts::Found {
            version: json_str(s, "java_version"),
            supported: json_bool(s.get("java_supported")).unwrap_or(false),
        },
        Some(false) => JavaFacts::Absent {
            detail: {
                let d = json_str(s, "java_detail");
                if d.is_empty() {
                    "Java was not found and no reason was given".to_string()
                } else {
                    d
                }
            },
        },
        // Absent field: the stage did not run. NOT "Java is missing" — that would be a negative
        // fact standing in for a stage that never happened.
        None => JavaFacts::NotChecked,
    };
    ServerFacts::Defined {
        name: json_str(s, "name"),
        port: s
            .get("port")
            .map(|p| match p {
                serde_json::Value::String(t) => t.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default(),
        address: json_str(s, "address"),
        server_classpath: json_str(s, "server_classpath"),
        listening: json_bool(s.get("listening")).unwrap_or(false),
        listen_detail: json_str(s, "listen_detail"),
        java,
    }
}

fn parse_connection(v: &serde_json::Value) -> Option<ConnectionFacts> {
    let c = v.get("connection")?;
    let defined = json_bool(c.get("defined"));
    Some(ConnectionFacts {
        name: json_str(c, "name"),
        defined,
        driver: json_str(c, "driver"),
        url: json_str(c, "url"),
        dsn: json_str(c, "dsn"),
        user: json_str(c, "user"),
        // Only meaningful for a connection that exists. `None` where it does not, so nothing
        // downstream can read "not JDBC" off a row that was never there.
        is_jdbc: match defined {
            Some(true) => json_bool(c.get("is_jdbc")).or(Some(false)),
            _ => None,
        },
        classpath_entries: json_strings(c, "classpath_entries"),
        classpath_missing: json_strings(c, "classpath_missing"),
        test_ok: json_bool(c.get("test_ok")) == Some(true),
        test_error: json_str(c, "test_error"),
        meta_ok: json_bool(c.get("meta_ok")),
        meta_error: json_str(c, "meta_error"),
        database: json_str(c, "database"),
        driver_name: json_str(c, "driver_name"),
        driver_version: json_str(c, "driver_version"),
        probe_ok: json_bool(c.get("probe_ok")),
        probe_error: json_str(c, "probe_error"),
        probe_rows: c.get("probe_rows").and_then(|r| r.as_u64()).unwrap_or(0),
    })
}

/// Read the generated program's output.
///
/// Non-JSON, empty output and `ok:0` are all `Unavailable`, each carrying what was actually said.
/// This is the check `handle_gateway_query`'s connection test did NOT have until #309: the program
/// carries no `$ZTRAP`, so an IRIS-side abort escapes as raw text, and reading that as "no failure
/// reported" made the test silently pass.
pub fn parse_facts(out: &str) -> GatewayFacts {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return GatewayFacts::Unavailable {
            detail: "IRIS returned no output at all — the program did not run, which is not the \
                     same as a gateway that does not work"
                .to_string(),
        };
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return GatewayFacts::Unavailable {
            detail: trimmed.chars().take(400).collect(),
        };
    };
    if json_bool(v.get("ok")) != Some(true) {
        let err = json_str(&v, "error");
        return GatewayFacts::Unavailable {
            detail: if err.is_empty() {
                "the program reported failure and no message".to_string()
            } else {
                err
            },
        };
    }
    GatewayFacts::Read {
        server: Box::new(parse_server(&v)),
        connection: parse_connection(&v).map(Box::new),
    }
}

/// One row of `list`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GatewayConnectionRow {
    pub name: String,
    pub driver: String,
    pub classpath: String,
    pub url: String,
    pub dsn: String,
    pub user: String,
    pub is_jdbc: bool,
    pub on_connect_statement: String,
}

/// What a listing is: found, genuinely empty, or unavailable. Three cases, because an instance with
/// no gateway connections and an instance we could not read are different facts and the caller acts
/// on them differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayListing {
    Found {
        rows: Vec<GatewayConnectionRow>,
        truncated: bool,
    },
    Empty,
    Unavailable {
        detail: String,
    },
}

/// What `create` did. Four cases, not two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateOutcome {
    /// Saved AND read back under the name asked for.
    Created { name: String },
    /// A connection of that name was already there. Nothing was changed — this tool does not
    /// overwrite somebody else's definition.
    AlreadyExists { name: String },
    /// IRIS refused, and said why.
    Failed { name: String, reason: String },
    /// The program produced no verdict, so whether anything was created is UNKNOWN. Distinct from
    /// `Failed`: a caller must not conclude "not created" from it, because a save that landed and
    /// then failed to report is the same shape.
    Unavailable { detail: String },
}

/// What `delete` did. The three-way split is the point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// It was there, and a re-read confirms it is gone.
    Deleted { name: String },
    /// The read succeeded and found nothing. An ESTABLISHED absence, not a failure wearing one.
    NotFound { name: String },
    /// It was there and it is STILL there. `reason` carries whatever `%DeleteId` said, which may be
    /// nothing at all — a status that reports success and leaves the row is the shape this whole
    /// module is built against.
    NotRemoved { name: String, reason: String },
    /// Could not tell whether it existed, or whether the delete took. Never "not found".
    Unavailable { detail: String },
}

/// `ok:0`, non-JSON and empty output all mean the same thing: no verdict. Shared by both writers so
/// they cannot disagree about what silence means.
fn write_verdict(out: &str) -> Result<serde_json::Value, String> {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err(
            "IRIS returned no output at all, so whether anything changed is UNKNOWN — this is not \
             a report that nothing happened"
                .to_string(),
        );
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Err(trimmed.chars().take(400).collect());
    };
    if json_bool(v.get("ok")) != Some(true) {
        let err = json_str(&v, "error");
        return Err(if err.is_empty() {
            "the program reported failure and no message".to_string()
        } else {
            err
        });
    }
    Ok(v)
}

/// Read `build_create_code`'s output.
///
/// Takes the [`Secret`] and scrubs `out` BEFORE looking at it, rather than trusting the caller to
/// have scrubbed already. That is the difference between a guarantee and a convention: a mutation
/// that removed the handler's separate scrub call survived the whole suite, because the only test
/// that drove that path was write-gated before the handler ran. With the scrub in here there is no
/// site left to forget.
pub fn parse_create(out: &str, name: &str, secret: &Secret) -> CreateOutcome {
    let scrubbed = secret.scrub(out);
    let out: &str = &scrubbed;
    let v = match write_verdict(out) {
        Ok(v) => v,
        // `ok:0` on this path carries IRIS's own reason for refusing the save, which is a FAILURE
        // with a reason rather than an absent verdict. Non-JSON and silence are not.
        Err(detail) => {
            return if serde_json::from_str::<serde_json::Value>(out.trim()).is_ok() {
                CreateOutcome::Failed {
                    name: name.to_string(),
                    reason: detail,
                }
            } else {
                CreateOutcome::Unavailable { detail }
            }
        }
    };
    match json_bool(v.get("exists")) {
        Some(true) => CreateOutcome::AlreadyExists {
            name: name.to_string(),
        },
        Some(false) => CreateOutcome::Created {
            name: name.to_string(),
        },
        // `ok:1` with no `exists` field: the program did not say which of the two happened, and
        // guessing either way invents a fact about the instance.
        None => CreateOutcome::Unavailable {
            detail: "the program reported success without saying whether the connection was \
                     created or already existed"
                .to_string(),
        },
    }
}

/// Read `build_delete_code`'s output.
pub fn parse_delete(out: &str, name: &str) -> DeleteOutcome {
    let v = match write_verdict(out) {
        Ok(v) => v,
        Err(detail) => return DeleteOutcome::Unavailable { detail },
    };
    let found = json_bool(v.get("found"));
    let removed = json_bool(v.get("removed"));
    match (found, removed) {
        (Some(false), _) => DeleteOutcome::NotFound {
            name: name.to_string(),
        },
        (Some(true), Some(true)) => DeleteOutcome::Deleted {
            name: name.to_string(),
        },
        (Some(true), Some(false)) => DeleteOutcome::NotRemoved {
            name: name.to_string(),
            reason: {
                let r = json_str(&v, "delete_status");
                if r.is_empty() {
                    "%DeleteId reported no error, and the connection is still there — the delete \
                     did not take effect"
                        .to_string()
                } else {
                    r
                }
            },
        },
        // Either field missing is a missing answer, not a negative one.
        _ => DeleteOutcome::Unavailable {
            detail: "the program did not report both whether the connection existed and whether \
                     it is now gone, so the result is unknown"
                .to_string(),
        },
    }
}

impl CreateOutcome {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Created { .. } => "GATEWAY_CREATED",
            Self::AlreadyExists { .. } => "GATEWAY_CONNECTION_EXISTS",
            Self::Failed { .. } => "GATEWAY_CREATE_FAILED",
            Self::Unavailable { .. } => "GATEWAY_CREATE_UNKNOWN",
        }
    }

    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Created { .. })
    }

    pub fn message(&self) -> String {
        match self {
            Self::Created { name } => format!(
                "SQL Gateway connection '{name}' was created and read back. The password is stored \
                 in the IRIS gateway definition and is not returned by any action of this tool. \
                 Run action=test next: creating a definition is not evidence that it connects."
            ),
            Self::AlreadyExists { name } => format!(
                "a SQL Gateway connection named '{name}' already exists and was left EXACTLY as it \
                 was — nothing was overwritten. Use action=test to see whether the existing one \
                 works, or delete it first if you meant to replace it."
            ),
            Self::Failed { name, reason } => format!(
                "IRIS refused to create SQL Gateway connection '{name}', so nothing was created. \
                 IRIS reported: {reason}"
            ),
            Self::Unavailable { detail } => format!(
                "the create program returned no verdict, so whether the connection was created is \
                 UNKNOWN — do NOT assume it was not. Run action=list to see what is actually there \
                 before retrying, because a retry against an existing name is refused rather than \
                 merged. IRIS wrote: {detail}"
            ),
        }
    }
}

impl DeleteOutcome {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Deleted { .. } => "GATEWAY_DELETED",
            Self::NotFound { .. } => "GATEWAY_CONNECTION_NOT_DEFINED",
            Self::NotRemoved { .. } => "GATEWAY_DELETE_FAILED",
            Self::Unavailable { .. } => "GATEWAY_DELETE_UNKNOWN",
        }
    }

    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Deleted { .. })
    }

    pub fn message(&self) -> String {
        match self {
            Self::Deleted { name } => format!(
                "SQL Gateway connection '{name}' existed and is gone — confirmed by reading it back \
                 after the delete, not by trusting the delete's own status."
            ),
            Self::NotFound { name } => format!(
                "there is no SQL Gateway connection named '{name}' on this instance, so nothing \
                 was deleted. This is an established absence: the lookup ran and returned no row. \
                 Use action=list to see the names that do exist."
            ),
            Self::NotRemoved { name, reason } => format!(
                "SQL Gateway connection '{name}' EXISTS and is still there — the delete did not \
                 take effect. It has not been removed, whatever the delete reported. IRIS said: \
                 {reason}"
            ),
            Self::Unavailable { detail } => format!(
                "the delete program returned no verdict, so whether that connection still exists \
                 is UNKNOWN — do NOT read this as 'it was not there'. Run action=list to find out. \
                 IRIS wrote: {detail}"
            ),
        }
    }
}

/// Read an Atelier query body into a listing.
pub fn parse_listing(body: &serde_json::Value) -> GatewayListing {
    let Some(content) = body["result"]["content"].as_array() else {
        return GatewayListing::Unavailable {
            detail:
                "the query answered without a result set, so whether this instance has gateway \
                     connections is unknown — this is not an empty list"
                    .to_string(),
        };
    };
    if content.is_empty() {
        return GatewayListing::Empty;
    }
    let truncated = content.len() > LIST_LIMIT;
    let rows = content
        .iter()
        .take(LIST_LIMIT)
        .map(|r| GatewayConnectionRow {
            name: json_str(r, "Connection_Name"),
            driver: json_str(r, "driver"),
            classpath: json_str(r, "classpath"),
            url: json_str(r, "URL"),
            dsn: json_str(r, "DSN"),
            user: json_str(r, "Usr"),
            // The stated trap: this arrives as JSON `true` over Atelier, not 1 or "1".
            is_jdbc: json_bool(r.get("isJDBC")).unwrap_or(false),
            on_connect_statement: json_str(r, "OnConnectStatement"),
        })
        .collect();
    GatewayListing::Found { rows, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the stated trap: a %Library.Boolean arrives in three shapes ────────────────────────
    #[test]
    fn a_boolean_is_read_in_every_shape_iris_sends_it() {
        // Atelier's /action/query hands a %Library.Boolean over as JSON `true`. That exact shape
        // broke iris_execute_method once, by copying a sibling that knew only 1 and "1".
        assert_eq!(json_bool(Some(&serde_json::json!(true))), Some(true));
        assert_eq!(json_bool(Some(&serde_json::json!(false))), Some(false));
        assert_eq!(json_bool(Some(&serde_json::json!(1))), Some(true));
        assert_eq!(json_bool(Some(&serde_json::json!(0))), Some(false));
        assert_eq!(json_bool(Some(&serde_json::json!("1"))), Some(true));
        assert_eq!(json_bool(Some(&serde_json::json!("0"))), Some(false));
        assert_eq!(json_bool(Some(&serde_json::json!("true"))), Some(true));
        assert_eq!(json_bool(Some(&serde_json::json!("TRUE"))), Some(true));
        // Absent is NOT false: it means the field was never written, and a caller that needs to
        // know the difference must be able to.
        assert_eq!(json_bool(None), None);
        assert_eq!(json_bool(Some(&serde_json::json!(null))), None);
        assert_eq!(json_bool(Some(&serde_json::json!("yes"))), None);
    }

    #[test]
    fn the_live_row_shape_is_read_as_jdbc() {
        // Verbatim from `SELECT * FROM %Library.sys_SQLConnection` on the dev instance, 2026-09-22.
        let body = serde_json::json!({"result": {"content": [{
            "ID": 1, "Connection_Name": "PG_COCINA_E2E", "DSN": "", "OnConnectStatement": "",
            "URL": "jdbc:postgresql://pg-gateway-e2e:5432/Cocina", "Usr": "gateway_ro",
            "classpath": "/usr/irissys/mgr/postgresql-42.7.4.jar", "driver": "org.postgresql.Driver",
            "isJDBC": true
        }]}});
        match parse_listing(&body) {
            GatewayListing::Found { rows, truncated } => {
                assert!(!truncated);
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].name, "PG_COCINA_E2E");
                assert_eq!(rows[0].driver, "org.postgresql.Driver");
                assert_eq!(rows[0].classpath, "/usr/irissys/mgr/postgresql-42.7.4.jar");
                assert_eq!(rows[0].user, "gateway_ro");
                assert!(rows[0].is_jdbc, "isJDBC arrives as JSON true, not 1");
                assert_eq!(rows[0].dsn, "");
            }
            other => panic!("expected a row, got {other:?}"),
        }
    }

    #[test]
    fn the_listing_never_projects_a_password() {
        let sql = list_sql();
        // `pwd` is a %CSP.Util.Passwd and `Secret` names a secure-store entry. A listing must not
        // be a route to either. Asserted on the SQL because that is what decides it.
        assert!(!sql.contains("pwd"), "{sql}");
        assert!(!sql.contains("Secret"), "{sql}");
        // …and the control: it does project the fields the caller needs.
        for col in ["Connection_Name", "classpath", "driver", "isJDBC", "Usr"] {
            assert!(sql.contains(col), "{col} missing from {sql}");
        }
    }

    #[test]
    fn an_empty_listing_and_an_unreadable_one_are_different_answers() {
        assert_eq!(
            parse_listing(&serde_json::json!({"result": {"content": []}})),
            GatewayListing::Empty
        );
        // No result set at all must NOT read as "this instance has no gateway connections".
        match parse_listing(&serde_json::json!({"status": {"errors": [{"error": "boom"}]}})) {
            GatewayListing::Unavailable { detail } => {
                assert!(detail.contains("not an empty list"), "{detail}")
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_listing_longer_than_the_cap_says_it_is_truncated() {
        let rows: Vec<serde_json::Value> = (0..LIST_LIMIT + 1)
            .map(|i| serde_json::json!({"Connection_Name": format!("C{i}"), "isJDBC": true}))
            .collect();
        match parse_listing(&serde_json::json!({"result": {"content": rows}})) {
            GatewayListing::Found { rows, truncated } => {
                assert!(truncated);
                assert_eq!(rows.len(), LIST_LIMIT);
            }
            other => panic!("{other:?}"),
        }
    }

    // ── the third case: no verdict is not a verdict ────────────────────────────────────────
    #[test]
    fn output_that_is_not_a_verdict_is_unavailable_not_a_diagnosis() {
        for raw in [
            "",
            "   ",
            "<CLASS DOES NOT EXIST> *%Net.Remote.Service",
            "ERROR #5002: ObjectScript error: <UNDEFINED>",
            r#"{"ok":0,"error":"OpenGateway blew up"}"#,
            r#"{"ok":0}"#,
        ] {
            let facts = parse_facts(raw);
            assert!(
                matches!(facts, GatewayFacts::Unavailable { .. }),
                "{raw:?} read as {facts:?} — the caller would act on that as a measurement"
            );
            let d = diagnose(&facts);
            assert_eq!(d.code(), "GATEWAY_DIAGNOSIS_INCONCLUSIVE", "{raw:?}");
            assert!(!d.is_healthy(), "{raw:?}");
            assert!(
                d.remedy().contains("not a verdict") || d.remedy().contains("was not established"),
                "{}",
                d.remedy()
            );
        }
    }

    /// The control for the test above: a real verdict must still be read, or a parser that called
    /// everything a non-verdict would pass it and break the tool outright.
    #[test]
    fn a_real_verdict_is_still_read() {
        let facts = parse_facts(&healthy_json(None));
        match &facts {
            GatewayFacts::Read { server, connection } => {
                assert!(matches!(**server, ServerFacts::Defined { .. }));
                assert!(connection.is_some());
            }
            other => panic!("{other:?}"),
        }
        assert!(diagnose(&facts).is_healthy());
    }

    /// Verbatim shape of the healthy path, from the measured run against PG_COCINA_E2E.
    fn healthy_json(probe: Option<(bool, &str)>) -> String {
        let (probe_ran, probe_ok, probe_error) = match probe {
            None => (0, "null", ""),
            Some((true, _)) => (1, "1", ""),
            Some((false, e)) => (1, "0", e),
        };
        format!(
            r#"{{"ok":1,
              "server":{{"defined":1,"name":"%JDBC Server","port":53772,"address":"127.0.0.1",
                         "server_classpath":"","listening":1,"listen_detail":"",
                         "java_found":1,"java_version":"11.0.31","java_supported":1,"java_detail":""}},
              "connection":{{"defined":1,"name":"PG_COCINA_E2E",
                             "classpath":"/usr/irissys/mgr/postgresql-42.7.4.jar",
                             "driver":"org.postgresql.Driver",
                             "url":"jdbc:postgresql://pg-gateway-e2e:5432/Cocina","dsn":"",
                             "user":"gateway_ro","is_jdbc":1,
                             "classpath_entries":["/usr/irissys/mgr/postgresql-42.7.4.jar"],
                             "classpath_missing":[],
                             "test_ok":1,"test_error":"",
                             "meta_ok":1,"database":"PostgreSQL 17.11",
                             "driver_name":"PostgreSQL JDBC Driver","driver_version":"42.7.4",
                             "probe_ran":{probe_ran},"probe_ok":{probe_ok},
                             "probe_error":"{probe_error}","probe_rows":1}}}}"#
        )
    }

    #[test]
    fn the_healthy_path_reports_the_target_it_actually_reached() {
        match diagnose(&parse_facts(&healthy_json(None))) {
            GatewayDiagnosis::Healthy {
                connection,
                database,
                driver,
            } => {
                assert_eq!(connection, "PG_COCINA_E2E");
                assert_eq!(database, "PostgreSQL 17.11");
                assert_eq!(driver, "PostgreSQL JDBC Driver 42.7.4");
            }
            other => panic!("{other:?}"),
        }
    }

    // ── one variant per failure mode, and each must be reachable ───────────────────────────
    /// Rewrite one field of the healthy fixture. Differential by construction: everything the
    /// mode does not change stays at its measured value, so a variant can only be reached by the
    /// thing it claims to be about.
    fn with(field: &str, value: &str) -> String {
        let h = healthy_json(None);
        let needle = format!(r#""{field}":"#);
        let at = h
            .find(&needle)
            .unwrap_or_else(|| panic!("fixture has no {field} to change"));
        let rest = &h[at + needle.len()..];
        let end = rest.find([',', '}']).expect("field value must terminate");
        format!("{}{}{}", &h[..at + needle.len()], value, &rest[end..])
    }

    #[test]
    fn java_missing_outranks_everything_downstream() {
        // The measured negative control's text: it names the directory it looked in.
        let raw = with("java_found", "0");
        let raw = raw.replace(
            r#""java_detail":""#,
            r#""java_detail":"ERROR #5001: Java executable not found in the given directory: /nope/not-a-jdk/bin/"#,
        );
        match diagnose(&parse_facts(&raw)) {
            GatewayDiagnosis::JavaAbsent { detail } => {
                assert!(detail.contains("/nope/not-a-jdk/bin/"), "{detail}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unsupported_java_is_its_own_mode() {
        match diagnose(&parse_facts(&with("java_supported", "0"))) {
            GatewayDiagnosis::JavaUnsupported { version, .. } => assert_eq!(version, "11.0.31"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_server_with_no_definition_is_named_as_such() {
        let raw = r#"{"ok":1,"server":{"defined":0,"detail":"ERROR #5001: Can't open Gateway definition for name 'NO_SUCH'"}}"#;
        match diagnose(&parse_facts(raw)) {
            GatewayDiagnosis::ServerNotDefined { detail } => {
                assert!(detail.contains("Can't open Gateway definition"), "{detail}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_connection_that_is_not_defined_is_not_a_connection_that_failed() {
        let raw = r#"{"ok":1,
          "server":{"defined":1,"name":"%JDBC Server","port":53772,"address":"127.0.0.1",
                    "listening":1,"java_found":1,"java_version":"11.0.31","java_supported":1},
          "connection":{"defined":0,"name":"NOPE","classpath_entries":[],"classpath_missing":[]}}"#;
        match diagnose(&parse_facts(raw)) {
            GatewayDiagnosis::ConnectionNotDefined { connection } => assert_eq!(connection, "NOPE"),
            other => panic!("{other:?}"),
        }
        // and the remedy is the shared one, which names the fix and never a credential
        let m = diagnose(&parse_facts(raw)).remedy();
        assert!(m.contains("SQL Gateway Connections"), "{m}");
        assert!(m.contains("never accepts a credential"), "{m}");

        // The envelope must not assert anything ABOUT a row that is not there. `is_jdbc: false`
        // for a connection that does not exist is a property of nothing — and the diagnosis
        // returns before that field is consulted, so only this assertion pins it. (Found by a
        // surviving mutant: dropping the `defined` guard on is_jdbc changed no verdict.)
        let GatewayFacts::Read {
            connection: Some(c),
            ..
        } = parse_facts(raw)
        else {
            panic!("fixture must parse")
        };
        assert_eq!(
            c.is_jdbc, None,
            "a connection that does not exist is neither JDBC nor not-JDBC"
        );
        assert_eq!(
            connection_json(&c)["is_jdbc"],
            serde_json::Value::Null,
            "and that must survive into the envelope as null, not false"
        );
        // The control: a connection that DOES exist reports the flag, so the assertion above is
        // not satisfied by a field that is always null.
        let GatewayFacts::Read {
            connection: Some(live),
            ..
        } = parse_facts(LIVE_HEALTHY)
        else {
            panic!("live fixture must parse")
        };
        assert_eq!(connection_json(&live)["is_jdbc"], serde_json::json!(true));
    }

    /// "We could not tell whether this connection exists" must not come back as "it does not."
    /// The two are one `if` apart and a caller acts on them completely differently: one is a typo
    /// to fix, the other is a broken tool.
    #[test]
    fn a_connection_stage_that_says_nothing_is_not_a_connection_that_is_absent() {
        let raw = r#"{"ok":1,
          "server":{"defined":1,"name":"%JDBC Server","port":53772,"address":"127.0.0.1",
                    "listening":1,"java_found":1,"java_version":"11.0.31","java_supported":1},
          "connection":{"name":"MYSTERY","classpath_entries":[],"classpath_missing":[]}}"#;
        match diagnose(&parse_facts(raw)) {
            GatewayDiagnosis::Inconclusive { stage, detail } => {
                assert_eq!(stage, "connection");
                assert!(detail.contains("MYSTERY"), "{detail}");
                assert!(
                    detail.contains("not the same as its not existing"),
                    "{detail}"
                );
            }
            other => panic!("a missing answer read as an answer: {other:?}"),
        }
        // The control: the SAME output with the field present must reach the definite verdict, or
        // the test above would pass on a parser that called every connection unknown.
        let answered = raw.replace(r#""name":"MYSTERY","#, r#""name":"MYSTERY","defined":0,"#);
        assert!(matches!(
            diagnose(&parse_facts(&answered)),
            GatewayDiagnosis::ConnectionNotDefined { .. }
        ));
    }

    #[test]
    fn an_odbc_connection_gets_an_odbc_answer_not_a_java_one() {
        match diagnose(&parse_facts(&with("is_jdbc", "0"))) {
            GatewayDiagnosis::NotJdbc { connection } => assert_eq!(connection, "PG_COCINA_E2E"),
            other => panic!("{other:?}"),
        }
    }

    /// The measured trap, and the reason the file check outranks the connection verdict: a WARM
    /// Java server already holds the driver class, so a connection whose classpath names a jar
    /// that is not there reports SUCCESS. `test_ok` stays 1 in this fixture on purpose.
    #[test]
    fn a_missing_jar_is_reported_even_when_the_connection_test_passed() {
        let raw = with(
            "classpath_missing",
            r#"["/usr/irissys/mgr/postgresql-42.7.4.jar"]"#,
        );
        match diagnose(&parse_facts(&raw)) {
            GatewayDiagnosis::ClasspathMissing {
                missing,
                test_passed_anyway,
                ..
            } => {
                assert_eq!(missing, vec!["/usr/irissys/mgr/postgresql-42.7.4.jar"]);
                assert!(
                    test_passed_anyway,
                    "the fixture's test_ok is 1 — that IS the measured warm-gateway case"
                );
            }
            other => panic!("a passing test hid the missing jar: {other:?}"),
        }
        let m = diagnose(&parse_facts(&raw)).remedy();
        assert!(m.contains("until the gateway restarts"), "{m}");
    }

    /// The COLD half of the same measurement, and the assertion a surviving mutant named.
    ///
    /// Missing jar AND `test_ok:0` AND a zero-length reason is one single measured state — the
    /// cold Java server that could not load the driver. Both `ClasspathMissing` and
    /// `ConnectFailedWithoutReason` match it, so which one wins is decided purely by the order of
    /// two `if`s, and the test above cannot see that order: its fixture has `test_ok:1`, which the
    /// empty-reason branch never fires on. Moving the empty-reason check above the file check
    /// SURVIVED the whole suite until this existed.
    ///
    /// The file check must win. It knows WHY; the connection test measurably knows nothing.
    #[test]
    fn a_cold_server_with_a_missing_jar_is_named_by_the_file_check_not_by_the_silence() {
        let raw = with(
            "classpath_missing",
            r#"["/usr/irissys/mgr/postgresql-42.7.4.jar"]"#,
        );
        let raw = raw.replace(r#""test_ok":1"#, r#""test_ok":0"#);
        // The other half of the measured signature: the reason is zero-length, not merely vague.
        assert!(raw.contains(r#""test_error":"""#), "fixture shape: {raw}");

        match diagnose(&parse_facts(&raw)) {
            GatewayDiagnosis::ClasspathMissing {
                missing,
                test_passed_anyway,
                ..
            } => {
                assert_eq!(missing, vec!["/usr/irissys/mgr/postgresql-42.7.4.jar"]);
                assert!(
                    !test_passed_anyway,
                    "this is the COLD case — the connection test failed too"
                );
            }
            other => panic!(
                "the file check knows the reason and the connection test does not; the verdict \
                 must not be the silent one: {other:?}"
            ),
        }
        // …and the remedy must be the cold-case wording, not the warm one.
        let m = diagnose(&parse_facts(&raw)).remedy();
        assert!(m.contains("EMPTY reason"), "{m}");
        assert!(
            !m.contains("until the gateway restarts"),
            "the warm-gateway wording is wrong here: {m}"
        );
    }

    /// Measured: a COLD Java server with a jar that is not there fails with a zero-length reason.
    /// Controlled — the same cold server with the right jar returns 1.
    #[test]
    fn a_failure_with_no_reason_is_its_own_mode_not_a_generic_one() {
        let raw = with("test_ok", "0");
        match diagnose(&parse_facts(&raw)) {
            GatewayDiagnosis::ConnectFailedWithoutReason { connection } => {
                assert_eq!(connection, "PG_COCINA_E2E")
            }
            other => panic!("{other:?}"),
        }
        let m = diagnose(&parse_facts(&raw)).remedy();
        assert!(m.contains("NO \reason") || m.contains("NO reason"), "{m}");
        assert!(m.contains("zero-length"), "{m}");
    }

    #[test]
    fn the_four_connection_failures_measured_against_postgres_get_four_headings() {
        // Verbatim `error` byref texts from the measured matrix. Each must land on a variant whose
        // remedy points somewhere different.
        let cases: [(&str, &str); 4] = [
            (
                "Remote JDBC error: org.postgresql.util.PSQLException: FATAL: password authentication failed for user \\\"gateway_ro\\\".",
                "GATEWAY_CREDENTIAL_REJECTED",
            ),
            (
                "Remote JDBC error: org.postgresql.util.PSQLException: FATAL: password authentication failed for user \\\"no_such_role\\\".",
                "GATEWAY_CREDENTIAL_REJECTED",
            ),
            (
                "Remote JDBC error: org.postgresql.util.PSQLException: FATAL: database \\\"NoSuchDb\\\" does not exist.",
                "GATEWAY_CONNECT_FAILED",
            ),
            (
                "Remote JDBC error: org.postgresql.util.PSQLException: Connection to pg-gateway-e2e:5999 refused. Check that the hostname and port are correct and that the postmaster is accepting TCP/IP connections..",
                "GATEWAY_CONNECT_FAILED",
            ),
        ];
        for (err, expected) in cases {
            let raw = with("test_ok", "0");
            let raw = raw.replace(r#""test_error":""#, &format!(r#""test_error":"{err}"#));
            let d = diagnose(&parse_facts(&raw));
            assert_eq!(d.code(), expected, "{err}");
            // The target's own words must survive into the remedy — they are the diagnosis.
            assert!(
                d.remedy().contains("PSQLException"),
                "the remote message was dropped: {}",
                d.remedy()
            );
        }
    }

    /// Mode 4, the one #343 calls the most expensive: the gateway was WORKING. Its remedy must send
    /// the caller to the transformation, and must NOT send them to the gateway.
    #[test]
    fn a_statement_the_target_refused_says_the_gateway_is_fine() {
        let raw = healthy_json(Some((
            false,
            "<GATEWAY> org.postgresql.util.PSQLException  ERROR: date/time field value out of range: \\\"01/01/1980\\\"",
        )));
        let d = diagnose(&parse_facts(&raw));
        assert_eq!(d.code(), "GATEWAY_TARGET_REJECTED_STATEMENT");
        let m = d.remedy();
        assert!(m.contains("gateway WORKS"), "{m}");
        assert!(m.contains("do not go looking there"), "{m}");
        assert!(m.contains("transformation"), "{m}");
        assert!(
            m.contains("out of range"),
            "the target's words must survive: {m}"
        );
        // It must not be confusable with a broken gateway.
        assert!(!m.contains("classpath entries do not exist"), "{m}");
    }

    #[test]
    fn a_probe_statement_that_succeeded_leaves_the_verdict_healthy() {
        assert!(diagnose(&parse_facts(&healthy_json(Some((true, ""))))).is_healthy());
    }

    #[test]
    fn a_handshake_failure_beats_a_passing_connection_test() {
        let raw = with("meta_ok", "0");
        let raw = raw.replace(
            r#""database":"PostgreSQL 17.11""#,
            r#""database":"","meta_error":"<GATEWAY> no such method""#,
        );
        match diagnose(&parse_facts(&raw)) {
            GatewayDiagnosis::HandshakeFailed { iris_error, .. } => {
                assert!(iris_error.contains("no such method"), "{iris_error}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn probe_alone_is_healthy_when_the_java_side_is_sound() {
        let raw = r#"{"ok":1,"server":{"defined":1,"name":"%JDBC Server","port":53772,
            "address":"127.0.0.1","server_classpath":"","listening":0,
            "listen_detail":"ERROR #5001: Server name cannot be determined",
            "java_found":1,"java_version":"11.0.31","java_supported":1,"java_detail":""}}"#;
        let d = diagnose(&parse_facts(raw));
        assert!(
            d.is_healthy(),
            "a server that is not listening is NOT a failure — measured: it starts on demand and \
             the connection still succeeds. Got {d:?}"
        );
        assert!(d.remedy().contains("Name a connection"), "{}", d.remedy());
    }

    /// Every variant must have a distinct code and a non-trivial remedy. A mode that shares another
    /// mode's code cannot be acted on differently, which is the whole point of #343.
    #[test]
    fn every_mode_has_its_own_code_and_its_own_remedy() {
        let all = vec![
            GatewayDiagnosis::Inconclusive {
                stage: "read",
                detail: "x".into(),
            },
            GatewayDiagnosis::JavaAbsent { detail: "x".into() },
            GatewayDiagnosis::JavaUnsupported {
                version: "8".into(),
                detail: "x".into(),
            },
            GatewayDiagnosis::ServerNotDefined { detail: "x".into() },
            GatewayDiagnosis::ConnectionNotDefined {
                connection: "c".into(),
            },
            GatewayDiagnosis::NotJdbc {
                connection: "c".into(),
            },
            GatewayDiagnosis::ClasspathMissing {
                connection: "c".into(),
                missing: vec!["j".into()],
                test_passed_anyway: false,
            },
            GatewayDiagnosis::ConnectFailedWithoutReason {
                connection: "c".into(),
            },
            GatewayDiagnosis::CredentialRejected {
                connection: "c".into(),
                user: "u".into(),
                iris_error: "e".into(),
            },
            GatewayDiagnosis::ConnectFailed {
                connection: "c".into(),
                iris_error: "e".into(),
            },
            GatewayDiagnosis::HandshakeFailed {
                connection: "c".into(),
                iris_error: "e".into(),
            },
            GatewayDiagnosis::TargetRejectedStatement {
                connection: "c".into(),
                database: "d".into(),
                iris_error: "e".into(),
            },
            GatewayDiagnosis::Healthy {
                connection: "c".into(),
                database: "d".into(),
                driver: "dr".into(),
            },
        ];
        let mut codes = std::collections::HashSet::new();
        let mut remedies = std::collections::HashSet::new();
        for d in &all {
            assert!(codes.insert(d.code()), "duplicate code {}", d.code());
            let r = d.remedy();
            assert!(r.len() > 80, "remedy for {} is a stub: {r}", d.code());
            assert!(
                remedies.insert(r.clone()),
                "two modes share a remedy: {}",
                d.code()
            );
        }
        assert_eq!(
            codes.len(),
            all.len(),
            "every mode must be separately addressable"
        );
        assert_eq!(
            all.iter().filter(|d| d.is_healthy()).count(),
            1,
            "exactly one mode is health"
        );
    }

    // ── generated code ────────────────────────────────────────────────────────────────────
    #[test]
    fn the_test_program_asks_the_apis_that_discriminate() {
        let code = build_test_code("PG_X", None);
        // Measured: OpenGateway names the port/address/JavaHome and errors for a server that is
        // not defined. Config.Gateways over SQL reuses positional columns between CPF sections.
        assert!(code.contains("%Net.Remote.Service).OpenGateway"), "{code}");
        assert!(
            !code.contains("Config.Gateways"),
            "that projection reuses columns between CPF sections: {code}"
        );
        assert!(code.contains("IsGatewayRunning"), "{code}");
        // The classpath verdict must come from the FILE, not from the connection test.
        assert!(code.contains("%File).Exists"), "{code}");
        assert!(code.contains("$SYSTEM.SQLGateway.TestConnection"), "{code}");
        // %Connect reports success for a dead port — measured on 53773 and 59999.
        assert!(
            !code.contains("%Net.Remote.Gateway"),
            "that API cannot tell a live port from a dead one: {code}"
        );
        assert!(code.contains("GetDatabaseProductNameAndVersion"), "{code}");
        assert!(code.contains("SetReadOnly(1)"), "{code}");
        // No delimiter-separated output — #246's bug. JSON only.
        assert!(code.contains("%ToJSON()"), "{code}");
        assert!(!code.contains("$CHAR(1)"), "{code}");
    }

    #[test]
    fn the_classpath_split_uses_the_separator_iris_itself_uses() {
        // Measured in %Net.Remote.Service::CmdLineForJava line 133:
        //   Set tCPSep = $Select($$$isWINDOWS:";", 1:":")
        // so the split must be OS-dependent and decided on the IRIS side, not guessed here.
        let code = build_test_code("PG_X", None);
        assert!(
            code.contains(r#"$SELECT($SYSTEM.Version.GetOS()="Windows":";", 1:":")"#),
            "{code}"
        );
        assert!(
            code.contains("$PIECE(cp, sep, i)"),
            "the separator must actually be used to split: {code}"
        );
    }

    #[test]
    fn the_connection_name_is_escaped_and_never_spliced() {
        let code = build_test_code("PG\"X", None);
        assert!(code.contains(r#""PG""X""#), "{code}");
        assert!(
            !code.contains('\\'),
            "backslash is not an escape here: {code}"
        );
        // Non-ASCII goes through $CHAR so the rendered expression stays ASCII.
        assert!(build_test_code("Pur\u{e9}", None).contains("$CHAR(233)"));
    }

    #[test]
    fn a_probe_statement_is_only_generated_when_one_was_asked_for() {
        let without = build_test_code("C", None);
        assert!(!without.contains("ExecuteQuery"), "{without}");
        assert!(!without.contains("probe_ran"), "{without}");
        let with_probe = build_test_code("C", Some("SELECT count(*) FROM public.menus"));
        assert!(with_probe.contains("ExecuteQuery"), "{with_probe}");
        assert!(
            with_probe.contains("SELECT count(*) FROM public.menus"),
            "{with_probe}"
        );
        // Its failure must be caught separately from the handshake's, or mode 4 collapses into
        // "the connection is broken".
        assert!(with_probe.contains("catch pe"), "{with_probe}");
        assert!(
            with_probe.contains(r#"%Set("probe_ok", 0)"#),
            "{with_probe}"
        );
    }

    /// `_` is ObjectScript's concatenation operator, so `set o.java_found = 1` parses as
    /// `o.java` _ `found` and aborts `<SYNTAX>` at RUN time. Measured against the live instance on
    /// the first program that tried it: the whole program did nothing and returned `ok:0`, an
    /// output shape no unit test on the Rust side would have questioned.
    ///
    /// Scoped to the ASSIGNMENT and READ forms only, and comments are stripped first — an
    /// explanatory comment naming the construct it forbids is the commonest false witness.
    #[test]
    fn no_dynamic_property_name_carries_an_underscore() {
        /// Every `<something>.<name>` where `<name>` has an underscore AND the expression is a
        /// dynamic-object member access this module writes. Restricted to the receivers this module
        /// creates (`tOut`, `tSrv`, `tConn`), so `crs.%SQLCODE` and `md.GetDriverName` cannot be
        /// swept in and the window never exceeds the claim.
        fn offenders(code: &str) -> Vec<String> {
            let mut found = Vec::new();
            for raw in code.lines() {
                // Strip an ObjectScript line comment before matching.
                let line = match raw.find("//") {
                    Some(i) => &raw[..i],
                    None => raw,
                };
                for recv in ["tOut.", "tSrv.", "tConn."] {
                    let mut from = 0;
                    while let Some(i) = line[from..].find(recv) {
                        let at = from + i + recv.len();
                        let name: String = line[at..]
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '%')
                            .collect();
                        if name.contains('_') && !name.starts_with('%') {
                            found.push(format!("{recv}{name}"));
                        }
                        from = at;
                    }
                }
            }
            found
        }

        for (label, code) in [
            ("probe", build_probe_code()),
            ("test", build_test_code("C", None)),
            ("test+probe", build_test_code("C", Some("SELECT 1 FROM t"))),
        ] {
            let bad = offenders(&code);
            assert!(
                bad.is_empty(),
                "{label}: dot access to an underscored dynamic key parses as concatenation and \
                 aborts <SYNTAX> at run time — use %Set/%Get: {bad:?}"
            );
        }

        // The control: this detector must be able to FIND one, or the clean sweeps above mean
        // nothing. This is the exact line that was measured failing.
        let regression = "        set tSrv.server_classpath = gw.ClassPath\n";
        assert_eq!(
            offenders(regression),
            vec!["tSrv.server_classpath"],
            "the detector cannot see the shape it exists to forbid"
        );
        // …and it must not fire on a comment that merely names it.
        assert!(
            offenders("    // never write set tSrv.java_found = 1 with dot syntax\n").is_empty(),
            "a comment naming the construct is not an occurrence of it"
        );
        // …nor on the %Set spelling that is correct.
        assert!(offenders("    do tSrv.%Set(\"java_found\", 1)\n").is_empty());
    }

    #[test]
    fn the_probe_program_reads_the_java_side_and_no_connection() {
        let code = build_probe_code();
        assert!(code.contains("OpenGateway"), "{code}");
        assert!(code.contains("GetJavaVersion"), "{code}");
        assert!(code.contains("CheckJavaVersionSupported"), "{code}");
        // It must not connect to anything: a probe is about the gateway, not about a target.
        assert!(!code.contains("TestConnection"), "{code}");
        assert!(!code.contains("GetConnection"), "{code}");
    }

    #[test]
    fn an_action_this_tool_does_not_have_says_what_it_does_have() {
        for a in ["status", "", "PROBE!", "drop", "update"] {
            let m = unknown_action_message(a);
            for valid in ACTIONS {
                assert!(m.contains(valid), "'{valid}' missing from: {m}");
            }
        }
        // create and delete are ACTIONS now, so they must not be described as absent. The charter
        // refusal that used to live here was removed with them; its reasoning is on the `password`
        // parameter, where someone about to "fix" the plaintext will read it.
        for a in ["create", "CREATE", " delete "] {
            assert!(
                parse_action(a).is_some(),
                "'{a}' must resolve now that the charter was relaxed"
            );
        }
        // Naming a remediation must still not hand out a bypass or a secret shape.
        let m = unknown_action_message("status");
        assert!(!m.to_lowercase().contains("pgpassword"), "{m}");
        assert!(!m.to_lowercase().contains("iris_allow_prod"), "{m}");
    }

    /// The advertised `action` enum and the list the handler dispatches on are two copies of the
    /// same fact. A schema that advertises an action the handler rejects — or omits one it
    /// accepts — is a guess the caller cannot recover from, so the two are asserted equal here
    /// rather than left to be noticed.
    #[test]
    fn the_advertised_enum_is_the_action_list() {
        let schema = serde_json::to_value(schemars::schema_for!(GatewayManageParams)).unwrap();
        let advertised: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("action has no enum in the advertised schema: {schema}"))
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(
            advertised, ACTIONS,
            "the advertised values and the dispatch list disagree"
        );
        // Every advertised value must RESOLVE, through the same function the handler dispatches on.
        // Advertising an action the handler then rejects is a guess a caller cannot recover from.
        let resolved: Vec<Action> = advertised
            .iter()
            .map(|a| {
                parse_action(a)
                    .unwrap_or_else(|| panic!("'{a}' is advertised but does not resolve"))
            })
            .collect();
        // …and each to a DIFFERENT one, so two advertised names cannot silently do one thing.
        let distinct: std::collections::HashSet<_> = resolved.iter().collect();
        assert_eq!(distinct.len(), advertised.len(), "{resolved:?}");
        for expected in [
            Action::Probe,
            Action::List,
            Action::Test,
            Action::Create,
            Action::Delete,
        ] {
            assert!(
                distinct.contains(&expected),
                "an Action variant exists that no advertised value reaches: {expected:?} not in \
                 {resolved:?}"
            );
        }
        // The control: something NOT advertised must not resolve, or the loop above would pass on a
        // parser that accepts everything.
        assert_eq!(parse_action("status"), None);
        assert_eq!(parse_action("drop"), None);
        assert_eq!(parse_action(""), None);
        // Case and surrounding space are the caller's, not the contract's.
        assert_eq!(parse_action("  PROBE "), Some(Action::Probe));
    }

    // ── the password must never come back out ─────────────────────────────────────────────
    //
    // A sentinel distinctive enough that a partial match is still a failure, and that no other
    // fixture in this file can produce by accident.
    const SENTINEL: &str = "Hunter2-SENTINEL-xyzzy";

    fn sentinel_spec() -> NewConnection {
        NewConnection {
            name: "PG_SENTINEL".into(),
            url: "jdbc:postgresql://db:5432/Cocina".into(),
            driver: "org.postgresql.Driver".into(),
            classpath: "/usr/irissys/mgr/postgresql-42.7.4.jar".into(),
            user: "gateway_ro".into(),
            password: Secret::new(SENTINEL),
            properties: String::new(),
            on_connect_statement: String::new(),
        }
    }

    /// The premise the whole mitigation rests on: the password IS in the generated program. If this
    /// ever stops being true the scrubbing tests below become vacuous and would still pass.
    #[test]
    fn the_generated_create_program_really_does_contain_the_password() {
        let code = build_create_code(&sentinel_spec());
        assert!(
            code.contains(SENTINEL),
            "the scrub tests are only meaningful because this is true"
        );
        assert!(code.contains("set conn.pwd ="), "{code}");
    }

    /// The worst case, and the one the scrub exists for: IRIS echoes the program back. Everything
    /// that reaches a `create` envelope goes through `redact_secret`, so even an echo of the whole
    /// source must come out clean.
    #[test]
    fn scrubbing_removes_the_password_from_an_echo_of_the_whole_program() {
        let code = build_create_code(&sentinel_spec());
        let scrubbed = redact_secret(&code, SENTINEL);
        assert!(
            !scrubbed.contains(SENTINEL),
            "the password survived a scrub of the program text"
        );
        assert!(
            scrubbed.contains(REDACTED),
            "nothing was replaced: {scrubbed}"
        );
        // …and the scrub must not have eaten the rest of the program.
        assert!(
            scrubbed.contains("set conn.Usr = \"gateway_ro\""),
            "{scrubbed}"
        );
        assert!(
            scrubbed.contains("jdbc:postgresql://db:5432/Cocina"),
            "{scrubbed}"
        );
    }

    /// A password containing characters `os_str_expr` transforms does NOT appear literally in the
    /// program — it is doubled or spliced as `$CHAR`. Scrubbing only the raw form would leave that
    /// copy behind, which is the whole reason `redact_secret` removes the rendering too.
    #[test]
    fn scrubbing_covers_the_form_the_password_actually_takes_in_the_program() {
        for pw in [
            "quote\"inside",
            "Contraseña",
            "tab\there",
            "plain",
            "a\"b\"c",
        ] {
            let mut spec = sentinel_spec();
            spec.password = Secret::new(pw);
            let code = build_create_code(&spec);
            let scrubbed = redact_secret(&code, pw);
            assert!(
                !scrubbed.contains(pw),
                "raw form of {pw:?} survived: {scrubbed}"
            );
            // The rendered form is what is actually in the program; it must be gone too.
            let rendered = crate::objectscript::os_str_expr(pw);
            assert!(
                !scrubbed.contains(&rendered),
                "rendered form {rendered:?} of {pw:?} survived: {scrubbed}"
            );
            // Control: the unscrubbed program DOES carry the rendering, so the assertion above is
            // not passing because there was nothing to find.
            assert!(
                code.contains(&rendered),
                "control failed — {rendered:?} is not in the program at all: {code}"
            );
        }
    }

    /// Each redaction pass needs an input that ONLY it catches, or a mutation disabling it survives.
    ///
    /// This test exists because that happened: the implementation had three overlapping passes, and
    /// disabling any one of them changed no test result — including the control, which is the worst
    /// possible mutation outcome. The witnesses below are what makes each pass load-bearing.
    #[test]
    fn each_redaction_pass_has_an_input_only_it_catches() {
        // WITNESS FOR THE PROGRAM-FORM PASS. A quote is doubled by os_str_expr, so the program
        // holds `a""b` while the raw password is `a"b`. The raw pass cannot see it.
        let pw = "a\"b";
        let in_program = "set conn.pwd = \"a\"\"b\"";
        assert!(
            !in_program.contains(pw),
            "premise: the raw form is NOT in the program text, which is why the raw pass alone \
             would miss it"
        );
        assert!(
            !redact_secret(in_program, pw).contains("a\"\"b"),
            "the program-form pass is what removes this: {}",
            redact_secret(in_program, pw)
        );

        // WITNESS FOR THE RAW PASS. A non-ASCII password is spliced as $CHAR in the program, so the
        // program form is `"Contrase"_$CHAR(241)_"a"` — which does not appear in a message that
        // quotes the plain word. Only the raw pass catches that.
        let pw2 = "Contraseña";
        let in_message = "IRIS refused: cannot store Contraseña for this user";
        let form = program_form(pw2);
        assert!(
            !in_message.contains(&form),
            "premise: the program form {form:?} is NOT in a message quoting the plain value, which \
             is why the program-form pass alone would miss it"
        );
        assert!(
            !redact_secret(in_message, pw2).contains(pw2),
            "the raw pass is what removes this: {}",
            redact_secret(in_message, pw2)
        );

        // A password that is ENTIRELY non-ASCII renders with no outer quotes at all, so the strip
        // must not silently produce an empty pattern.
        let pw3 = "ñé";
        let spliced = program_form(pw3);
        assert!(
            spliced.starts_with("$CHAR("),
            "premise for the no-quotes branch: {spliced:?}"
        );
        let prog3 = format!("set conn.pwd = {}", os_str_expr(pw3));
        assert!(
            !redact_secret(&prog3, pw3).contains(&spliced),
            "an all-non-ASCII password survived in the program: {}",
            redact_secret(&prog3, pw3)
        );
    }

    /// The commonest way a secret escapes is not a missing scrub — it is a `{:?}` someone adds
    /// later. `Secret`'s formatting is redacted so that cannot happen.
    #[test]
    fn a_secret_cannot_be_formatted_into_anything() {
        let s = Secret::new(SENTINEL);
        assert_eq!(format!("{s:?}"), REDACTED);
        assert_eq!(format!("{s}"), REDACTED);
        // …and the struct that holds one.
        let spec = sentinel_spec();
        let dumped = format!("{spec:?}");
        assert!(
            !dumped.contains(SENTINEL) && !dumped.contains("Hunter2"),
            "a debug dump of the create parameters leaked the password: {dumped}"
        );
        // Control: the dump is not empty, so the assertion above is about redaction rather than
        // about a Debug impl that prints nothing.
        assert!(dumped.contains("gateway_ro"), "{dumped}");
        assert!(dumped.contains(REDACTED), "{dumped}");
    }

    /// `parse_create` scrubs its own input, so there is no site for the handler to forget. A
    /// mutation removing the handler's separate scrub survived before this existed.
    #[test]
    fn parse_create_scrubs_whatever_iris_said() {
        let leaky = format!(r#"{{"ok":0,"error":"<SYNTAX> set conn.pwd = \"{SENTINEL}\""}}"#);
        // The premise: the input really does carry it.
        assert!(leaky.contains(SENTINEL));
        let o = parse_create(&leaky, "PG_X", &Secret::new(SENTINEL));
        let msg = o.message();
        assert!(!msg.contains(SENTINEL), "{msg}");
        assert!(
            msg.contains(REDACTED),
            "the reason must survive, redacted: {msg}"
        );
        // Control: passing the WRONG secret leaves it, which proves the scrub is driven by the
        // secret it was given rather than by some unrelated filtering.
        let other = parse_create(&leaky, "PG_X", &Secret::new("something-else"));
        assert!(other.message().contains(SENTINEL), "{}", other.message());
    }

    /// An empty secret must be a NO-OP. `str::replace` with an empty pattern inserts the marker
    /// between every character, so a call that supplied no password would come back mangled.
    #[test]
    fn an_empty_secret_scrubs_nothing() {
        let text = "SQL Gateway connection 'PG_X' was created.";
        assert_eq!(redact_secret(text, ""), text);
        assert_eq!(redact_secret(text, "   "), text);
        // The control: a real secret DOES change the text, so the equality above is not trivially
        // true of every input.
        assert_ne!(
            redact_secret("the pw is s3cret", "s3cret"),
            "the pw is s3cret"
        );
    }

    /// Every message this module can produce on the `create` path, scrubbed. A message that quotes
    /// IRIS's own error is the likeliest accidental carrier.
    #[test]
    fn no_create_outcome_message_can_carry_the_password() {
        let outcomes = [
            CreateOutcome::Created {
                name: "PG_SENTINEL".into(),
            },
            CreateOutcome::AlreadyExists {
                name: "PG_SENTINEL".into(),
            },
            // IRIS quoting the failing line back at us — the realistic leak.
            CreateOutcome::Failed {
                name: "PG_SENTINEL".into(),
                reason: format!("ERROR #5002: <SYNTAX> set conn.pwd = \"{SENTINEL}\""),
            },
            CreateOutcome::Unavailable {
                detail: format!("<SYNTAX> zSet+4 set conn.pwd = \"{SENTINEL}\""),
            },
        ];
        for o in &outcomes {
            let scrubbed = redact_secret(&o.message(), SENTINEL);
            assert!(
                !scrubbed.contains(SENTINEL),
                "{} leaked the password: {scrubbed}",
                o.code()
            );
        }
        // Controls. Two of these fixtures DO contain the sentinel before scrubbing, so the sweep
        // above is not passing over messages that never had it.
        let carriers = outcomes
            .iter()
            .filter(|o| o.message().contains(SENTINEL))
            .count();
        assert_eq!(
            carriers, 2,
            "the two IRIS-quoting fixtures must carry the sentinel before scrubbing, or this test \
             proves nothing"
        );
    }

    /// The parse must survive a scrubbed input — the handler scrubs BEFORE parsing, so a scrub that
    /// broke the JSON would turn every create into `Unavailable`.
    #[test]
    fn scrubbing_before_parsing_does_not_break_the_verdict() {
        let raw = r#"{"ok":1,"exists":0}"#;
        assert_eq!(
            parse_create(raw, "PG_X", &Secret::new(SENTINEL)),
            CreateOutcome::Created {
                name: "PG_X".into()
            }
        );
        // …including when the output genuinely quotes the password back.
        let leaky = format!(r#"{{"ok":0,"error":"cannot set pwd to {SENTINEL}"}}"#);
        match parse_create(&leaky, "PG_X", &Secret::new(SENTINEL)) {
            CreateOutcome::Failed { reason, .. } => {
                assert!(!reason.contains(SENTINEL), "{reason}");
                assert!(reason.contains(REDACTED), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    // ── create: four outcomes ─────────────────────────────────────────────────────────────
    #[test]
    fn create_reports_created_only_when_it_was_read_back() {
        assert_eq!(
            parse_create(r#"{"ok":1,"exists":0}"#, "PG_X", &Secret::new("")),
            CreateOutcome::Created {
                name: "PG_X".into()
            }
        );
        assert!(parse_create(r#"{"ok":1,"exists":0}"#, "PG_X", &Secret::new("")).succeeded());
    }

    #[test]
    fn create_never_overwrites_an_existing_name() {
        let o = parse_create(r#"{"ok":1,"exists":1}"#, "PG_X", &Secret::new(""));
        assert_eq!(
            o,
            CreateOutcome::AlreadyExists {
                name: "PG_X".into()
            }
        );
        assert!(!o.succeeded(), "an existing name is NOT a create");
        assert!(
            o.message().contains("left EXACTLY as it was"),
            "{}",
            o.message()
        );
    }

    #[test]
    fn a_create_that_could_not_be_told_about_is_not_a_create_that_failed() {
        // Non-JSON and silence carry no verdict: a save that landed and then failed to report has
        // exactly this shape, so "not created" would be an invented fact.
        for raw in ["", "   ", "<CLASS DOES NOT EXIST> *%Library.SQLConnection"] {
            let o = parse_create(raw, "PG_X", &Secret::new(""));
            assert_eq!(o.code(), "GATEWAY_CREATE_UNKNOWN", "{raw:?} -> {o:?}");
            assert!(!o.succeeded());
            assert!(
                o.message().contains("do NOT assume it was not"),
                "{}",
                o.message()
            );
        }
        // `ok:1` with no `exists` is also no answer — the program did not say which happened.
        assert_eq!(
            parse_create(r#"{"ok":1}"#, "PG_X", &Secret::new("")).code(),
            "GATEWAY_CREATE_UNKNOWN"
        );
        // The control: a JSON `ok:0` IS a reason, and must stay a FAILURE rather than an unknown.
        let failed = parse_create(
            r#"{"ok":0,"error":"duplicate name"}"#,
            "PG_X",
            &Secret::new(""),
        );
        assert_eq!(failed.code(), "GATEWAY_CREATE_FAILED");
        assert!(
            failed.message().contains("duplicate name"),
            "{}",
            failed.message()
        );
    }

    /// `%Save()` returning OK is not evidence the row is there. The program re-reads, and this is
    /// the verdict that must come back if the re-read finds nothing.
    #[test]
    fn a_save_that_reported_success_but_left_nothing_is_a_failure() {
        let raw = r#"{"ok":0,"error":"%Save() reported success but no connection with this name can be read back, so it was NOT created"}"#;
        let o = parse_create(raw, "PG_X", &Secret::new(""));
        assert_eq!(o.code(), "GATEWAY_CREATE_FAILED");
        assert!(!o.succeeded());
    }

    // ── delete: found / removed is a three-way answer ─────────────────────────────────────
    #[test]
    fn delete_tells_absent_apart_from_not_removed() {
        assert_eq!(
            parse_delete(r#"{"ok":1,"found":1,"removed":1}"#, "PG_X"),
            DeleteOutcome::Deleted {
                name: "PG_X".into()
            }
        );
        assert_eq!(
            parse_delete(r#"{"ok":1,"found":0,"removed":0}"#, "PG_X"),
            DeleteOutcome::NotFound {
                name: "PG_X".into()
            }
        );
        // Existed, still there. This must NEVER read as a success or as an absence.
        let stuck = parse_delete(
            r#"{"ok":1,"found":1,"removed":0,"delete_status":"ERROR #5803: lock failed"}"#,
            "PG_X",
        );
        assert_eq!(stuck.code(), "GATEWAY_DELETE_FAILED");
        assert!(!stuck.succeeded());
        assert!(
            stuck.message().contains("still there"),
            "{}",
            stuck.message()
        );
        assert!(
            stuck.message().contains("lock failed"),
            "{}",
            stuck.message()
        );
        // The three codes must differ, or the distinction is not addressable.
        let codes: std::collections::HashSet<&str> = [
            parse_delete(r#"{"ok":1,"found":1,"removed":1}"#, "X").code(),
            parse_delete(r#"{"ok":1,"found":0,"removed":0}"#, "X").code(),
            stuck.code(),
        ]
        .into_iter()
        .collect();
        assert_eq!(codes.len(), 3, "{codes:?}");
    }

    /// The measured hazard this re-read exists for: a status that says OK and leaves the row.
    /// `delete_status` is empty, and the verdict must still be "not removed".
    #[test]
    fn a_delete_that_reported_no_error_and_changed_nothing_is_still_a_failure() {
        let o = parse_delete(
            r#"{"ok":1,"found":1,"removed":0,"delete_status":""}"#,
            "PG_X",
        );
        assert_eq!(o.code(), "GATEWAY_DELETE_FAILED");
        assert!(
            o.message().contains("did not take effect"),
            "{}",
            o.message()
        );
    }

    #[test]
    fn a_delete_that_could_not_be_told_about_is_not_a_connection_that_was_absent() {
        for raw in [
            "",
            "   ",
            "<CLASS DOES NOT EXIST>",
            r#"{"ok":0,"error":"SQLCODE -30"}"#,
            // Either field missing is a missing answer.
            r#"{"ok":1,"found":1}"#,
            r#"{"ok":1,"removed":1}"#,
        ] {
            let o = parse_delete(raw, "PG_X");
            assert_eq!(o.code(), "GATEWAY_DELETE_UNKNOWN", "{raw:?} -> {o:?}");
            assert!(!o.succeeded());
            assert!(
                o.message()
                    .contains("do NOT read this as 'it was not there'"),
                "{}",
                o.message()
            );
        }
        // Control: a complete answer is still read, or the sweep above would pass on a parser that
        // called everything unknown.
        assert!(parse_delete(r#"{"ok":1,"found":1,"removed":1}"#, "PG_X").succeeded());
    }

    // ── the generated writers ─────────────────────────────────────────────────────────────
    #[test]
    fn the_create_program_checks_first_reads_back_and_never_overwrites() {
        let code = build_create_code(&sentinel_spec());
        // It looks before it leaps, and reports the existing name rather than replacing it.
        assert!(code.contains(r#"%Set("exists", 1)"#), "{code}");
        assert!(
            !code.contains("%DeleteId") && !code.contains("%KillExtent"),
            "create must not remove anything: {code}"
        );
        // A %Save() that reports success is not the verdict — it reads the row back.
        assert!(code.contains("could not be read back"), "{code}");
        assert!(code.contains("%Library.SQLConnection).%New()"), "{code}");
        assert!(code.contains("set conn.isJDBC = 1"), "{code}");
        // A failed existence check must abort rather than fall through into a create.
        assert!(
            code.contains("could not check whether the connection already exists"),
            "{code}"
        );
    }

    #[test]
    fn the_delete_program_reads_back_instead_of_trusting_the_status() {
        let code = build_delete_code("PG_X");
        assert!(code.contains("%DeleteId"), "{code}");
        // The re-read: two SELECTs, one before and one after.
        assert_eq!(
            code.matches("SELECT ID FROM %Library.sys_SQLConnection")
                .count(),
            2,
            "the delete must confirm by reading back, not by trusting %DeleteId: {code}"
        );
        assert!(code.contains(r#"%Set("removed""#), "{code}");
        assert!(code.contains(r#"%Set("found""#), "{code}");
        // A failed read must not answer "not found".
        assert!(
            code.contains("whether it exists is unknown and nothing was deleted"),
            "{code}"
        );
    }

    #[test]
    fn the_writers_escape_their_inputs_and_carry_no_underscored_dot_access() {
        let code = build_delete_code("PG\"X");
        assert!(code.contains(r#""PG""X""#), "{code}");
        assert!(
            !code.contains('\\'),
            "backslash is not an escape here: {code}"
        );
        let mut spec = sentinel_spec();
        spec.name = "Pur\u{e9}".into();
        assert!(build_create_code(&spec).contains("$CHAR(233)"));
    }

    /// `_` is ObjectScript's concatenation operator — the same defect the read programs hit. The
    /// writers use `%Set` for every underscored key too.
    #[test]
    fn the_writers_use_set_for_underscored_keys() {
        for code in [build_create_code(&sentinel_spec()), build_delete_code("C")] {
            let stripped: String = code
                .lines()
                .map(|l| match l.find("//") {
                    Some(i) => &l[..i],
                    None => l,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !stripped.contains("tOut.delete_status") && !stripped.contains("tOut.on_connect"),
                "dot access to an underscored dynamic key aborts <SYNTAX> at run time: {stripped}"
            );
            assert!(
                code.contains(r#"do tOut.%Set("ok""#),
                "the verdict key must be set through %Set: {code}"
            );
        }
        // Control: the detector can see the forbidden shape when it is there.
        let bad = "        set tOut.delete_status = \"x\"\n";
        assert!(bad.contains("tOut.delete_status"));
    }

    /// Every generated program must write its verdict EXACTLY once.
    ///
    /// Measured on the live instance while building `create`: `quit` inside a `try` block ends the
    /// BLOCK, not the routine, so an early exit spelled `write tOut.%ToJSON()` + `quit` fell
    /// through to the trailing write and emitted `{"ok":1,"exists":1}{"ok":1,"exists":1}`. Two
    /// concatenated objects are not JSON, so the create came back `GATEWAY_CREATE_UNKNOWN` —
    /// honest, and useless.
    ///
    /// The SAME shape was already in the merged `test` program's SQLCODE path, which no test had
    /// ever driven. Both are fixed here: an early exit sets its fields and `quit`s, and the one
    /// trailing write emits the object.
    #[test]
    fn every_generated_program_writes_its_verdict_exactly_once() {
        for (label, code) in [
            ("probe", build_probe_code()),
            ("test", build_test_code("C", None)),
            ("test+probe", build_test_code("C", Some("SELECT 1 FROM t"))),
            ("create", build_create_code(&sentinel_spec())),
            ("delete", build_delete_code("C")),
        ] {
            assert_eq!(
                code.matches("write tOut.%ToJSON()").count(),
                1,
                "{label} writes its verdict more than once; `quit` inside `try` does not end the \
                 routine, so the output is two concatenated objects and parses as nothing: {code}"
            );
            // …and the one it has must be the LAST statement, or an early exit would skip it.
            assert!(
                code.trim_end().ends_with("write tOut.%ToJSON()"),
                "{label}: the single write must be the final statement: {code}"
            );
        }
        // The control: the detector can see a doubled write when there is one.
        let doubled = "write tOut.%ToJSON()\nquit\nwrite tOut.%ToJSON()";
        assert_eq!(doubled.matches("write tOut.%ToJSON()").count(), 2);
    }

    /// The write classification the gate derives from. Getting this backwards on either side is a
    /// security-relevant bug: a read wrongly gated is friction, a write wrongly ungated is not.
    #[test]
    fn only_create_and_delete_are_writes() {
        assert!(!Action::Probe.is_write());
        assert!(!Action::List.is_write());
        assert!(!Action::Test.is_write());
        assert!(Action::Create.is_write());
        assert!(Action::Delete.is_write());
        // Derived from ACTIONS so a new action cannot be added without landing on one side.
        let writes: Vec<&str> = ACTIONS
            .iter()
            .copied()
            .filter(|a| parse_action(a).is_some_and(|x| x.is_write()))
            .collect();
        assert_eq!(writes, vec!["create", "delete"], "the write set moved");
    }

    #[test]
    fn the_credential_hint_only_ever_chooses_between_two_carrying_variants() {
        assert!(looks_like_credential_refusal(
            "FATAL: password authentication failed for user \"x\""
        ));
        assert!(!looks_like_credential_refusal(
            "FATAL: database \"NoSuchDb\" does not exist."
        ));
        assert!(!looks_like_credential_refusal("Connection refused"));
    }

    // ── the programs' REAL output, byte for byte ───────────────────────────────────────────
    //
    // Everything above this point is a fixture I wrote. These three are what `build_test_code`
    // and `build_probe_code` actually printed when run through the generator against IRIS for
    // Health 2026.1 and PostgreSQL 17.11 on 2026-09-22 — pasted unedited, including key order and
    // the `%JDBC Server` name's leading `%`. A hand-written fixture agreeing with a parser is not
    // evidence the parser reads what IRIS sends.

    /// `action=test` with `probe_query="SELECT count(*) FROM public.menus"`, verbatim.
    const LIVE_HEALTHY: &str = r#"{"ok":1,"server":{"defined":1,"name":"%JDBC Server","port":53772,"address":"127.0.0.1","server_classpath":"","listening":1,"listen_detail":"","java_found":1,"java_version":"11.0.31","java_supported":1,"java_detail":""},"connection":{"name":"PG_COCINA_E2E","defined":1,"classpath":"/usr/irissys/mgr/postgresql-42.7.4.jar","driver":"org.postgresql.Driver","url":"jdbc:postgresql://pg-gateway-e2e:5432/Cocina","dsn":"","user":"gateway_ro","is_jdbc":1,"classpath_entries":["/usr/irissys/mgr/postgresql-42.7.4.jar"],"classpath_missing":[],"test_ok":1,"test_error":"","database":"PostgreSQL 17.11","driver_name":"PostgreSQL JDBC Driver","driver_version":"42.7.4","meta_ok":1,"probe_ran":1,"probe_ok":1,"probe_rows":1}}"#;

    /// `action=test connection=NO_SUCH_CONN_XYZ`, verbatim.
    const LIVE_NOT_DEFINED: &str = r#"{"ok":1,"server":{"defined":1,"name":"%JDBC Server","port":53772,"address":"127.0.0.1","server_classpath":"","listening":1,"listen_detail":"","java_found":1,"java_version":"11.0.31","java_supported":1},"connection":{"name":"NO_SUCH_CONN_XYZ","defined":0,"classpath_entries":[],"classpath_missing":[]}}"#;

    /// `action=test` with `probe_query="SELECT no_such_col FROM public.menus"`, verbatim — mode 4 as
    /// PostgreSQL actually words it, with every earlier stage green.
    const LIVE_STATEMENT_REFUSED: &str = r#"{"ok":1,"server":{"defined":1,"name":"%JDBC Server","port":53772,"address":"127.0.0.1","server_classpath":"","listening":1,"listen_detail":"","java_found":1,"java_version":"11.0.31","java_supported":1,"java_detail":""},"connection":{"name":"PG_COCINA_E2E","defined":1,"classpath":"/usr/irissys/mgr/postgresql-42.7.4.jar","driver":"org.postgresql.Driver","url":"jdbc:postgresql://pg-gateway-e2e:5432/Cocina","dsn":"","user":"gateway_ro","is_jdbc":1,"classpath_entries":["/usr/irissys/mgr/postgresql-42.7.4.jar"],"classpath_missing":[],"test_ok":1,"test_error":"","database":"PostgreSQL 17.11","driver_name":"PostgreSQL JDBC Driver","driver_version":"42.7.4","meta_ok":1,"probe_ran":1,"probe_ok":0,"probe_error":"<GATEWAY> org.postgresql.util.PSQLException  ERROR: column \"no_such_col\" does not exist"}}"#;

    #[test]
    fn the_real_healthy_output_is_read_as_healthy() {
        match diagnose(&parse_facts(LIVE_HEALTHY)) {
            GatewayDiagnosis::Healthy {
                connection,
                database,
                driver,
            } => {
                assert_eq!(connection, "PG_COCINA_E2E");
                assert_eq!(database, "PostgreSQL 17.11");
                assert_eq!(driver, "PostgreSQL JDBC Driver 42.7.4");
            }
            other => panic!("the live healthy output read as {other:?}"),
        }
        // The Java-side facts must survive into the envelope too, since `probe` is all of them.
        let GatewayFacts::Read { server, .. } = parse_facts(LIVE_HEALTHY) else {
            panic!("live output is not readable")
        };
        match *server {
            ServerFacts::Defined {
                name, port, java, ..
            } => {
                assert_eq!(name, "%JDBC Server");
                // The port arrives as a JSON NUMBER, not a string — it must not be lost.
                assert_eq!(port, "53772");
                assert_eq!(
                    java,
                    JavaFacts::Found {
                        version: "11.0.31".to_string(),
                        supported: true
                    }
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_real_not_defined_output_names_the_connection() {
        match diagnose(&parse_facts(LIVE_NOT_DEFINED)) {
            GatewayDiagnosis::ConnectionNotDefined { connection } => {
                assert_eq!(connection, "NO_SUCH_CONN_XYZ")
            }
            other => panic!("a name that does not exist read as {other:?}"),
        }
    }

    /// The two live blobs differ in ONE field — `probe_ok` — and that field alone must move the
    /// verdict from health to mode 4. Anything else changing would mean the fixtures are not
    /// comparable and the conclusion would not be about the probe.
    #[test]
    fn the_real_statement_refusal_is_mode_four() {
        let d = diagnose(&parse_facts(LIVE_STATEMENT_REFUSED));
        assert_eq!(d.code(), "GATEWAY_TARGET_REJECTED_STATEMENT");
        let m = d.remedy();
        assert!(m.contains("PostgreSQL 17.11"), "{m}");
        assert!(m.contains("no_such_col"), "{m}");
        assert!(m.contains("gateway WORKS"), "{m}");

        // Every earlier stage in this same live output was green: that is what makes it mode 4
        // rather than "the gateway is broken".
        let GatewayFacts::Read {
            connection: Some(c),
            ..
        } = parse_facts(LIVE_STATEMENT_REFUSED)
        else {
            panic!("live output is not readable")
        };
        assert!(c.test_ok);
        assert_eq!(c.meta_ok, Some(true));
        assert!(c.classpath_missing.is_empty());
        assert_eq!(c.probe_ok, Some(false));

        // And the one-field difference from the healthy blob, asserted rather than assumed.
        let diff = LIVE_HEALTHY
            .split(',')
            .zip(LIVE_STATEMENT_REFUSED.split(','))
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            diff <= 2,
            "the two live blobs differ in {diff} comma-separated fields — they are not the same \
             run with one thing changed, so the comparison proves nothing"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The JGService requirement — the interop half of #343's first failure mode
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// The documented requirement a JDBC Business Operation cannot work without.
///
/// ONE copy, shared with the `<INVALID OREF> … initAdapterJG^EnsLib.JavaGateway.Common` abort hint
/// in `tools::mod` (#209), which is where this text was already written. It is restated nowhere:
/// `jgservice_requirement_is_not_restated` asserts both users carry this exact string.
///
/// It applies to the ADAPTER path only. Measured on 2026-09-22: `$SYSTEM.SQLGateway.TestConnection`
/// succeeded against PG_COCINA_E2E on an instance with NO production running at all, so a SQL
/// Gateway connection needs no `EnsLib.JavaGateway.Service`. A Business Operation over that same
/// connection still does — which is exactly why a healthy `test` must say so rather than let the
/// caller conclude the JDBC work is done.
pub const JGSERVICE_REQUIREMENT: &str = "Per the SQL Gateway documentation, JGService is REQUIRED \
     for all JDBC data sources, even with a working SQL gateway connection: a business service of \
     type EnsLib.JavaGateway.Service must be present, and the adapter needs that configuration \
     item's exact name. Check, in this order: (1) an EnsLib.JavaGateway.Service item exists in the \
     production, (2) the production is started, (3) the operation's JGService setting names that \
     item exactly — set it with iris_production_item(action=set_settings, item=<BO>, \
     settings={\"Adapter.JGService\": \"<that item name>\"}).";

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Tool surface
// ─────────────────────────────────────────────────────────────────────────────────────────────

use schemars::JsonSchema;
use serde::Deserialize;

// No struct-level `///` here: schemars promotes one into the advertised inputSchema's top-level
// description, and that is wire traffic on every tools/list. Rationale stays in `//` comments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GatewayManageParams {
    /// What to do. "probe": can a JDBC SQL Gateway run on this instance at all — is the Java
    /// external language server defined, and is its Java runtime present and supported. "list":
    /// the SQL Gateway connections defined here, with driver, class path and username (never a
    /// password). "test": take ONE connection all the way to the external database and name which
    /// failure mode it is if it does not get there. "create": define a new JDBC connection.
    /// "delete": remove one.
    // #112: the values belong in the SCHEMA, not only in the UNKNOWN_ACTION message. Naming the
    // field without them moves the guess one level down, which cost nine of the campaign's 31
    // parameter errors. `every_tool_advertises_the_parameters_it_reads` enforces it — and caught
    // this one. Kept in step with [`ACTIONS`] by `the_advertised_enum_is_the_action_list`.
    #[schemars(extend("enum" = ["probe", "list", "test", "create", "delete"]))]
    pub action: String,
    /// NAME of a SQL Gateway connection (for example "PG_COCINA"). Required for test, create and
    /// delete.
    #[serde(default)]
    pub connection: Option<String>,
    /// JDBC URL of the external database, for action=create — for example
    /// "jdbc:postgresql://dbhost:5432/Cocina". IRIS treats an entry containing a colon as a JDBC
    /// URL.
    #[serde(default)]
    pub url: Option<String>,
    /// JDBC driver class, for action=create — for example "org.postgresql.Driver".
    #[serde(default)]
    pub driver: Option<String>,
    /// Full path to the driver jar AS IRIS SEES IT, for action=create — inside the IRIS container,
    /// not on the client host. Separate several with the target OS's path separator. action=test
    /// checks each entry against the filesystem, because a jar that is not there makes a connection
    /// that works until the Java server next restarts.
    #[serde(default)]
    pub classpath: Option<String>,
    /// Username the connection logs in as, for action=create.
    #[serde(default)]
    pub user: Option<String>,
    /// Password for `user`, for action=create. It is stored in the IRIS gateway definition and is
    /// never returned by any action of this tool.
    //
    // WHY THIS IS PLAINTEXT, and what was considered instead. Read this before "fixing" it.
    //
    // #214 built this family of tools so that no credential crosses the MCP boundary: iris_gateway_
    // query takes a connection NAME precisely so the password stays where an administrator put it.
    // `create` cannot honour that, and the maintainer relaxed the charter deliberately for this
    // action. It is not an oversight and it is not fixable by being cleverer about the API:
    //
    //   * `%Library.SQLConnection` stores its own `Usr` and `pwd`. It does not read
    //     `Ens.Config.Credentials`, so an interop credential id cannot be pointed at — verified
    //     against the class, whose properties are `Usr`, `pwd` (`%CSP.Util.Passwd`) and `Secret`.
    //   * `Secret` names an entry in the secure store, but creating THAT entry needs the password
    //     too, so routing through it moves the plaintext one call earlier and removes nothing.
    //   * There is no SQL write path to substitute a bound parameter for:
    //     `%Library.sys_SQLConnection` is a read-only projection and the class has a numeric IDKEY,
    //     so `create` is object access from generated ObjectScript, and the password is therefore
    //     inside the program text.
    //
    // What is mitigated instead is the way OUT: see `redact_secret` and `Secret`. Do not add a
    // credential-lookup parameter believing it removes the plaintext — it does not, and it would
    // give a caller a reason to think the password is safe when it is in the transcript either way.
    //
    // KNOWN RESIDUE, stated rather than papered over. `execute_via_generator` PUTs the program as a
    // class document, compiles it, runs it and deletes it — and that delete is best effort, so a
    // process killed between compile and delete leaves an `IrisDevTmp.Run<id>` class on the instance
    // whose source contains this password in clear. It is the SAME instance that is about to store
    // the password in the gateway definition anyway, so this exposes it to no new party, but it does
    // sit in a class document rather than in `pwd`. If that matters for a deployment, the fix is at
    // the generator, not here: nothing this module can do about it, and pretending otherwise would
    // be worse than the note.
    //
    // The redaction covers the way back to the CALLER. It does not and cannot cover the instance.
    #[serde(default)]
    pub password: Option<String>,
    /// Extra JDBC driver properties, for action=create. Optional.
    #[serde(default)]
    pub properties: Option<String>,
    /// Statement to run on the remote system immediately after connecting, for action=create.
    /// Optional — an Oracle session NLS setting is the usual use.
    #[serde(default)]
    pub on_connect_statement: Option<String>,
    /// Optional read-only SELECT for action=test, in the EXTERNAL database's own dialect. Once the
    /// connection is proven, this statement is run through it — so "the gateway is broken" and
    /// "the gateway works and the target refused this statement" come back as different answers.
    /// Mutating statements are refused and never sent.
    #[serde(default)]
    pub probe_query: Option<String>,
    /// IRIS namespace whose gateway definitions to use. OMIT this field to use the connection's
    /// configured namespace (IRIS_NAMESPACE) — only pass a value to deliberately target a
    /// different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}

fn server_json(server: &ServerFacts) -> serde_json::Value {
    match server {
        ServerFacts::NotDefined { detail } => serde_json::json!({
            "name": JDBC_SERVER,
            "defined": false,
            "detail": detail,
        }),
        ServerFacts::Defined {
            name,
            port,
            address,
            server_classpath,
            listening,
            listen_detail,
            java,
        } => {
            let (java_found, java_version, java_supported, java_detail) = match java {
                JavaFacts::Found { version, supported } => {
                    (Some(true), version.clone(), Some(*supported), String::new())
                }
                JavaFacts::Absent { detail } => {
                    (Some(false), String::new(), Some(false), detail.clone())
                }
                JavaFacts::NotChecked => (None, String::new(), None, String::new()),
            };
            serde_json::json!({
                "name": name,
                "defined": true,
                "port": port,
                "address": address,
                "server_class_path": server_classpath,
                // Reported as a FACT, never as a verdict: measured, a server that is not listening
                // starts on demand and the connection still succeeds.
                "listening": listening,
                "listening_detail": listen_detail,
                "listening_note": "not listening is normal — this server starts on demand when a \
                                   JDBC gateway connection is used",
                "java_found": java_found,
                "java_version": java_version,
                "java_supported": java_supported,
                "java_detail": java_detail,
            })
        }
    }
}

fn connection_json(c: &ConnectionFacts) -> serde_json::Value {
    serde_json::json!({
        "name": c.name,
        "driver": c.driver,
        "url": c.url,
        "dsn": c.dsn,
        "user": c.user,
        "is_jdbc": c.is_jdbc,
        "class_path_entries": c.classpath_entries,
        "class_path_missing": c.classpath_missing,
        "connect_test_ok": c.test_ok,
        "connect_test_error": c.test_error,
        "database": c.database,
        "driver_name": c.driver_name,
        "driver_version": c.driver_version,
        "probe_ok": c.probe_ok,
        "probe_error": c.probe_error,
        "probe_rows": c.probe_rows,
    })
}

/// `probe` takes no connection. Is the caller passing one anyway, and what should they be told?
///
/// #385: the probe program deliberately reads the Java side and nothing else — that is asserted by
/// `the_probe_program_reads_the_java_side_and_no_connection`, and it is the right design: "a probe
/// is about the gateway, not about a target". So a `connection` argument could never affect the
/// answer. The problem was that the answer it gave was `GATEWAY_OK`, `success: true` — exactly what
/// a caller was hoping to hear about the connection they had just named.
///
/// Measured 2026-09-24 against an instance with no connections defined:
/// `action=probe, connection="ZzNoSuchGateway"` returned `GATEWAY_OK` with no `connection` key
/// anywhere in the reply, while `action=test` on the same name returned
/// GATEWAY_CONNECTION_NOT_DEFINED and `action=delete` likewise. The instance-level answer was true;
/// it answered a different question than the one asked.
///
/// This is the call this repo already made twice: #356 (`iris_doc(put)` refuses a `names` array
/// instead of discarding it) and #382 (`rule_name` was discarded when no action was given).
///
/// Returns `None` when nothing was passed, or when what was passed is blank — a blank string names
/// no connection, so there is nothing to refuse and nothing was discarded.
pub fn probe_ignores_connection(connection: Option<&str>) -> Option<String> {
    let named = connection.map(str::trim).filter(|s| !s.is_empty())?;
    Some(format!(
        "action=probe does not take a connection: it asks whether a JDBC SQL Gateway can run on \
         this instance at all — whether the Java external language server is defined and its Java \
         runtime is present and supported — and never looks at any one connection. '{named}' was \
         NOT checked, so this call was refused rather than answered about something else. Use \
         action=test with connection='{named}' to take that connection all the way to its \
         database, or action=list to see which names are defined here."
    ))
}

pub async fn handle_gateway_manage(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    p: GatewayManageParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let action_raw = p.action.trim().to_string();

    match parse_action(&action_raw) {
        Some(Action::Probe) => {
            // #385: accepting a connection here and answering GATEWAY_OK told the caller the
            // connection they named was fine, about a name this action never reads.
            if let Some(msg) = probe_ignores_connection(p.connection.as_deref()) {
                // The code literal stays ADJACENT to `fail_with(` deliberately. The #329 remedy
                // gate finds codes by scanning for `fail_with("`, so a rustfmt-wrapped call is
                // invisible to it and its REMEDIES row reads as stale (#361). Measured: 0 of the 11
                // `fail_with(` calls in this file are visible to that scan, which is why none of
                // the GATEWAY_* codes carry a remedy on record. If this line is ever re-wrapped the
                // gate fails loudly naming PARAM_NOT_FOR_ACTION, which is the intended tripwire.
                let extra = serde_json::json!({ "namespace": namespace, "action": "probe" });
                return crate::tools::envelope::fail_with("PARAM_NOT_FOR_ACTION", &msg, extra);
            }
            let out = match iris
                .execute_via_generator(&build_probe_code(), &namespace, client)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::envelope::transport_fail(
                        "handle_gateway_manage",
                        &e.to_string(),
                    )
                }
            };
            let facts = parse_facts(&out);
            let diagnosis = diagnose(&facts);
            emit(&facts, &diagnosis, &namespace, "probe")
        }
        Some(Action::List) => {
            let body = match iris.query(&list_sql(), vec![], &namespace, client).await {
                Ok(v) => v,
                Err(e) => {
                    // An unreadable listing must never arrive as "this instance has none".
                    return crate::tools::envelope::fail_with(
                        "GATEWAY_LIST_UNAVAILABLE",
                        &format!(
                            "the SQL Gateway connections on this instance could not be read, so \
                             whether any are defined is unknown — this is NOT an empty list. IRIS \
                             reported: {e}"
                        ),
                        serde_json::json!({ "namespace": namespace }),
                    );
                }
            };
            match parse_listing(&body) {
                GatewayListing::Unavailable { detail } => crate::tools::envelope::fail_with(
                    "GATEWAY_LIST_UNAVAILABLE",
                    &detail,
                    serde_json::json!({ "namespace": namespace }),
                ),
                GatewayListing::Empty => crate::tools::envelope::ok_json(serde_json::json!({
                    "success": true,
                    "namespace": namespace,
                    "action": "list",
                    "connections": [],
                    "count": 0,
                    "note": "this instance has NO SQL Gateway connections defined. The query ran \
                             and returned nothing, which is an answer rather than a failure. \
                             Create one in the Management Portal under System Administration > \
                             Configuration > Connectivity > SQL Gateway Connections.",
                })),
                GatewayListing::Found { rows, truncated } => {
                    let mut obj = serde_json::json!({
                        "success": true,
                        "namespace": namespace,
                        "action": "list",
                        "count": rows.len(),
                        "connections": rows,
                        "note": "the stored password is never returned by this tool. Use \
                                 action=test to find out whether a connection actually works.",
                    });
                    if truncated {
                        obj["truncated"] = true.into();
                        obj["note"] =
                            format!("listed the first {LIST_LIMIT} connections; there are more.")
                                .into();
                    }
                    crate::tools::envelope::ok_json(obj)
                }
            }
        }
        Some(Action::Test) => {
            let connection = p.connection.as_deref().unwrap_or("").trim().to_string();
            if connection.is_empty() {
                return crate::tools::envelope::fail_with(
                    "MISSING_PARAMS",
                    "action=test needs 'connection' — the NAME of a SQL Gateway connection on this \
                     instance. Nothing was run. Use action=list to see which names exist.",
                    serde_json::json!({ "namespace": namespace }),
                );
            }
            let probe_sql = p
                .probe_query
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(sql) = probe_sql {
                if let Err(keyword) = crate::tools::gateway::validate_gateway_sql(sql) {
                    return crate::tools::envelope::fail_with(
                        "SQL_NOT_READ_ONLY",
                        &crate::tools::gateway::rejected_sql_message(&keyword),
                        serde_json::json!({
                            "connection": connection,
                            "namespace": namespace,
                            "rejected": keyword,
                        }),
                    );
                }
            }
            let out = match iris
                .execute_via_generator(&build_test_code(&connection, probe_sql), &namespace, client)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::envelope::transport_fail(
                        "handle_gateway_manage",
                        &e.to_string(),
                    )
                }
            };
            let facts = parse_facts(&out);
            let diagnosis = diagnose(&facts);
            emit(&facts, &diagnosis, &namespace, "test")
        }
        Some(Action::Create) => {
            // The password is in scope from here to the end of this arm. NOTHING in it may put a
            // string into an envelope without `redact_secret` — including IRIS's own error text,
            // which can quote the line it failed on.
            let password = Secret::new(p.password.clone().unwrap_or_default());

            let connection = p.connection.as_deref().unwrap_or("").trim().to_string();
            let mut missing: Vec<&str> = Vec::new();
            if connection.is_empty() {
                missing.push("connection");
            }
            if p.url.as_deref().unwrap_or("").trim().is_empty() {
                missing.push("url");
            }
            if p.driver.as_deref().unwrap_or("").trim().is_empty() {
                missing.push("driver");
            }
            if p.user.as_deref().unwrap_or("").trim().is_empty() {
                missing.push("user");
            }
            if password.is_empty() {
                missing.push("password");
            }
            if !missing.is_empty() {
                return crate::tools::envelope::fail_with(
                    "MISSING_PARAMS",
                    &format!(
                        "action=create needs {} — nothing was created. 'connection' is the NAME to \
                         give the definition, 'url' the JDBC URL as IRIS will reach the database, \
                         'driver' the JDBC driver class, and 'user'/'password' the login it stores. \
                         'classpath' should name the driver jar as IRIS sees it.",
                        missing.join(", ")
                    ),
                    // Deliberately no echo of the arguments: one of them is the password.
                    serde_json::json!({ "namespace": namespace, "missing": missing }),
                );
            }

            let spec = NewConnection {
                name: connection.clone(),
                url: p.url.as_deref().unwrap_or("").trim().to_string(),
                driver: p.driver.as_deref().unwrap_or("").trim().to_string(),
                classpath: p.classpath.as_deref().unwrap_or("").trim().to_string(),
                user: p.user.as_deref().unwrap_or("").trim().to_string(),
                password,
                properties: p.properties.as_deref().unwrap_or("").trim().to_string(),
                on_connect_statement: p
                    .on_connect_statement
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            };
            let out = match iris
                .execute_via_generator(&build_create_code(&spec), &namespace, client)
                .await
            {
                Ok(v) => v,
                // The transport error can quote the request body, which held the program.
                Err(e) => {
                    return crate::tools::envelope::transport_fail(
                        "handle_gateway_manage",
                        &spec.password.scrub(&e.to_string()),
                    )
                }
            };
            let outcome = parse_create(&out, &connection, &spec.password);
            let detail = serde_json::json!({
                "namespace": namespace,
                "action": "create",
                "connection": connection,
                "outcome": outcome.code(),
                // Echoed back so a caller can see what was stored — MINUS the password, which is
                // simply not a field of this object.
                "url": spec.url,
                "driver": spec.driver,
                "class_path": spec.classpath,
                "user": spec.user,
            });
            if outcome.succeeded() {
                let mut obj = detail;
                obj["success"] = true.into();
                obj["message"] = spec.password.scrub(&outcome.message()).into();
                obj["next_step"] = format!(
                    "run action=test connection={connection} — a definition that saved is not a \
                     connection that works, and the class path is only checked there."
                )
                .into();
                return crate::tools::envelope::ok_json(obj);
            }
            crate::tools::envelope::fail_with(
                outcome.code(),
                &spec.password.scrub(&outcome.message()),
                detail,
            )
        }
        Some(Action::Delete) => {
            let connection = p.connection.as_deref().unwrap_or("").trim().to_string();
            if connection.is_empty() {
                return crate::tools::envelope::fail_with(
                    "MISSING_PARAMS",
                    "action=delete needs 'connection' — the NAME of the SQL Gateway connection to \
                     remove. Nothing was deleted. Use action=list to see which names exist.",
                    serde_json::json!({ "namespace": namespace }),
                );
            }
            let out = match iris
                .execute_via_generator(&build_delete_code(&connection), &namespace, client)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::envelope::transport_fail(
                        "handle_gateway_manage",
                        &e.to_string(),
                    )
                }
            };
            let outcome = parse_delete(&out, &connection);
            let detail = serde_json::json!({
                "namespace": namespace,
                "action": "delete",
                "connection": connection,
                "outcome": outcome.code(),
            });
            if outcome.succeeded() {
                let mut obj = detail;
                obj["success"] = true.into();
                obj["message"] = outcome.message().into();
                return crate::tools::envelope::ok_json(obj);
            }
            crate::tools::envelope::fail_with(outcome.code(), &outcome.message(), detail)
        }
        None => crate::tools::envelope::fail_with(
            "UNKNOWN_ACTION",
            &unknown_action_message(&action_raw),
            serde_json::json!({ "namespace": namespace, "valid_actions": ACTIONS }),
        ),
    }
}

/// Render a diagnosis. A failed mode goes out through the FAILURE envelope carrying its own code,
/// so a caller that only reads `success` still cannot mistake a broken gateway for a working one.
fn emit(
    facts: &GatewayFacts,
    diagnosis: &GatewayDiagnosis,
    namespace: &str,
    action: &str,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let mut detail = serde_json::json!({
        "namespace": namespace,
        "action": action,
        "diagnosis": diagnosis.code(),
        "remedy": diagnosis.remedy(),
    });
    if let GatewayFacts::Read { server, connection } = facts {
        detail["java_gateway"] = server_json(server);
        if let Some(c) = connection {
            detail["connection"] = connection_json(c);
        }
    }
    if diagnosis.is_healthy() {
        detail["success"] = true.into();
        // A working SQL Gateway connection is NOT a working JDBC Business Operation. Said on the
        // healthy path deliberately: this is where a caller stops looking.
        detail["business_operation_note"] = JGSERVICE_REQUIREMENT.into();
        return crate::tools::envelope::ok_json(detail);
    }
    crate::tools::envelope::fail_with(diagnosis.code(), &diagnosis.remedy(), detail)
}
