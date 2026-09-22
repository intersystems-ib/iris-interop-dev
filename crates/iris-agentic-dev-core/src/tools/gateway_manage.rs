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

/// Actions this tool accepts. `create` and `delete` are deliberately absent — see
/// [`why_no_create_message`].
pub const ACTIONS: &[&str] = &["probe", "list", "test"];

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

    if c.is_jdbc.is_none() && c.driver.is_empty() && c.url.is_empty() && c.dsn.is_empty() {
        return GatewayDiagnosis::ConnectionNotDefined {
            connection: c.name.clone(),
        };
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

/// Why `create` and `delete` are not actions here, said where a caller will look for them.
///
/// #214's charter for this module is that no credential crosses the MCP boundary in either
/// direction: the tool takes a connection NAME, and the username and password stay in the gateway
/// definition where an administrator put them. A `create` action cannot honour that — a JDBC SQL
/// Gateway connection stores its own username and password, so `create` would have to accept a
/// plaintext password as a tool argument, which puts it in the transcript. That is the exact harm
/// #214 was built to remove (it replaced 32 `PGPASSWORD=… psql` invocations).
pub fn why_no_create_message() -> String {
    format!(
        "'create' and 'delete' are not actions of this tool. A SQL Gateway connection stores a \
         username and password, so creating one here would mean sending a plaintext password as a \
         tool argument and into the transcript — the harm this family of tools exists to remove. \
         Create the connection once in the Management Portal under System Administration > \
         Configuration > Connectivity > SQL Gateway Connections, then use action=test to verify it \
         and iris_gateway_query to read through it. Valid actions: {}.",
        ACTIONS.join(", ")
    )
}

/// What to say for an action this tool does not have.
pub fn unknown_action_message(action: &str) -> String {
    if action.eq_ignore_ascii_case("create") || action.eq_ignore_ascii_case("delete") {
        return why_no_create_message();
    }
    format!(
        "'{action}' is not an action of iris_gateway_manage. Valid actions: {}.",
        ACTIONS.join(", ")
    )
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
        write tOut.%ToJSON()
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
    let defined = json_bool(c.get("defined")) == Some(true);
    Some(ConnectionFacts {
        name: json_str(c, "name"),
        driver: if defined {
            json_str(c, "driver")
        } else {
            String::new()
        },
        url: if defined {
            json_str(c, "url")
        } else {
            String::new()
        },
        dsn: if defined {
            json_str(c, "dsn")
        } else {
            String::new()
        },
        user: json_str(c, "user"),
        is_jdbc: if defined {
            json_bool(c.get("is_jdbc")).or(Some(false))
        } else {
            None
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
        for a in ["status", "", "PROBE!"] {
            let m = unknown_action_message(a);
            for valid in ACTIONS {
                assert!(m.contains(valid), "{m}");
            }
        }
        // create/delete get the reason, not just the list — a caller who asks for them is asking
        // the right question and deserves the answer.
        for a in ["create", "CREATE", "delete"] {
            let m = unknown_action_message(a);
            assert!(m.contains("plaintext password"), "{m}");
            assert!(m.contains("SQL Gateway Connections"), "{m}");
            // Naming the remediation must not hand out a bypass.
            assert!(!m.to_lowercase().contains("pgpassword"), "{m}");
        }
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
    /// failure mode it is if it does not get there.
    pub action: String,
    /// NAME of a SQL Gateway connection, for action=test (for example "PG_COCINA"). This tool
    /// never accepts a host, user, or password: the credential stays in the IRIS gateway
    /// definition where an administrator put it.
    #[serde(default)]
    pub connection: Option<String>,
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

pub async fn handle_gateway_manage(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    p: GatewayManageParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let action_raw = p.action.trim().to_string();
    let action = action_raw.to_lowercase();

    match action.as_str() {
        "probe" => {
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
        "list" => {
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
        "test" => {
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
        _ => crate::tools::envelope::fail_with(
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
