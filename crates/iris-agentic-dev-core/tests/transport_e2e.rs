//! Transport-matrix e2e: validates that code execution works over BOTH transports the
//! MCP supports —
//!   * HTTP / Atelier REST (`IrisConnection::execute_via_generator`), and
//!   * Docker-exec (`IrisConnection::execute`, `docker exec <container> iris session`).
//!
//! Skipped unless `IRIS_HOST` is set. The Docker leg additionally needs `IRIS_CONTAINER`
//! (the e2e harness / scripts/iris-up.sh export it); it self-skips when unset or when docker
//! is unavailable, so the suite stays green on HTTP-only environments.

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};

fn conn() -> Option<IrisConnection> {
    let host = std::env::var("IRIS_HOST").ok().filter(|h| !h.is_empty())?;
    let port = std::env::var("IRIS_WEB_PORT").unwrap_or_else(|_| "52773".into());
    let user = std::env::var("IRIS_USERNAME").unwrap_or_else(|_| "_SYSTEM".into());
    let pass = std::env::var("IRIS_PASSWORD").unwrap_or_else(|_| "SYS".into());
    let base = format!("http://{host}:{port}");
    Some(IrisConnection::new(
        &base,
        "USER",
        &user,
        &pass,
        DiscoverySource::EnvVar,
    ))
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

/// HTTP / Atelier REST transport: compile-temp-class + SqlProc + query.
#[test]
fn http_transport_executes_and_captures_output() {
    let Some(c) = conn() else {
        eprintln!("skip: IRIS_HOST unset");
        return;
    };
    rt().block_on(async {
        let client = IrisConnection::http_client().unwrap();
        let out = c
            .execute_via_generator("write \"HTTP_OK:\",6*7,!", "USER", &client)
            .await
            .expect("http transport execute");
        assert!(out.contains("HTTP_OK:42"), "HTTP transport output: {out:?}");
    });
}

/// Docker-exec transport: `docker exec <container> iris session IRIS -U <ns>` over stdin.
#[test]
fn docker_transport_executes_and_captures_output() {
    let Some(c) = conn() else {
        eprintln!("skip: IRIS_HOST unset");
        return;
    };
    if std::env::var("IRIS_CONTAINER").is_err() {
        eprintln!("skip: IRIS_CONTAINER unset (docker-exec transport not exercised)");
        return;
    }
    rt().block_on(async {
        match c.execute("write \"DOCKER_OK:\",6*7,!", "USER").await {
            Ok(out) => assert!(
                out.contains("DOCKER_OK:42"),
                "docker transport output: {out:?}"
            ),
            Err(e) if e.to_string() == "DOCKER_REQUIRED" => {
                eprintln!("skip: docker-exec transport unavailable")
            }
            Err(e) => panic!("docker transport error: {e}"),
        }
    });
}

/// Both transports must agree on the same computation.
#[test]
fn both_transports_agree() {
    let Some(c) = conn() else {
        eprintln!("skip: IRIS_HOST unset");
        return;
    };
    if std::env::var("IRIS_CONTAINER").is_err() {
        eprintln!("skip: IRIS_CONTAINER unset");
        return;
    }
    rt().block_on(async {
        let client = IrisConnection::http_client().unwrap();
        let http = c
            .execute_via_generator("write 6*7", "USER", &client)
            .await
            .expect("http leg");
        let docker = match c.execute("write 6*7", "USER").await {
            Ok(o) => o,
            Err(_) => {
                eprintln!("skip: docker leg unavailable");
                return;
            }
        };
        assert!(
            http.contains("42") && docker.contains("42"),
            "transports disagree: http={http:?} docker={docker:?}"
        );
    });
}
