//! Client-side concurrency control for mutating Atelier operations.
//!
//! Atelier raises transient conflicts when writes/compiles overlap (reproduced on IRIS for Health
//! 2026.1, Atelier API v8):
//!   * concurrent PUT of the *same* document  -> HTTP 423 (Locked)
//!   * concurrent `/action/compile` (any class) -> HTTP 400 with an empty body
//!
//! Sequential and different-document writes are fine. The previous code surfaced these as
//! `IRIS_UNREACHABLE` (see #16), so an agent concluded IRIS was down and abandoned the MCP.
//!
//! Two complementary mechanisms here:
//!  1. [`compile_gate`] — an in-process semaphore that serializes `/action/compile` calls. Compiles
//!     collide regardless of the classes involved, so running them concurrently can never succeed;
//!     gating them is strictly better (and not slower) than letting them fail and retry. This handles
//!     the dominant single-session case (one Claude session fans out many subagents through ONE MCP
//!     process).
//!  2. [`send_with_retry`] — bounded retry with exponential backoff on transient statuses. This is the
//!     ONLY thing that helps across *separate* MCP processes (multiple Claude sessions, or other Atelier
//!     clients, against the same server), which an in-process gate fundamentally cannot coordinate.

use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Serializes `/action/compile` within this process. Default 1 (full serialization); override with
/// `IRIS_MAX_CONCURRENT_COMPILES` (>=1).
pub(crate) fn compile_gate() -> &'static Semaphore {
    static GATE: OnceLock<Semaphore> = OnceLock::new();
    GATE.get_or_init(|| {
        let permits = std::env::var("IRIS_MAX_CONCURRENT_COMPILES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n >= 1)
            .unwrap_or(1);
        Semaphore::new(permits)
    })
}

/// Max total attempts (1 initial + retries) for a mutating request.
const MAX_ATTEMPTS: u32 = 4;

/// Send a mutating Atelier request, rebuilt fresh each attempt by `make_req`, retrying on transient
/// concurrency conflicts: 423 (Locked) and 409 (Conflict) always, plus 400 when `retry_on_400` is set
/// (an empty-body 400 is Atelier's signature for an overlapping compile). Transport errors that are
/// timeouts/connects are retried too. Backoff is exponential (100ms, 200ms, 400ms).
///
/// Returns the final [`reqwest::Response`] (which may still be non-2xx if retries were exhausted) so the
/// caller keeps its existing status/body handling.
pub(crate) async fn send_with_retry(
    make_req: impl Fn() -> reqwest::RequestBuilder,
    retry_on_400: bool,
) -> reqwest::Result<reqwest::Response> {
    let mut attempt: u32 = 1;
    loop {
        match make_req().send().await {
            Ok(resp) => {
                let s = resp.status().as_u16();
                let transient = matches!(s, 409 | 423) || (retry_on_400 && s == 400);
                if transient && attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) if attempt < MAX_ATTEMPTS && (e.is_timeout() || e.is_connect()) => {
                tokio::time::sleep(backoff(attempt)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    // attempt is 1-based: 100ms, 200ms, 400ms, ...
    Duration::from_millis(100u64 * (1u64 << (attempt - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_exponential() {
        assert_eq!(backoff(1), Duration::from_millis(100));
        assert_eq!(backoff(2), Duration::from_millis(200));
        assert_eq!(backoff(3), Duration::from_millis(400));
    }

    #[tokio::test]
    async fn compile_gate_serializes_to_one_by_default() {
        // Default is a single permit unless the env override is set.
        let gate = compile_gate();
        let _p = gate.acquire().await.unwrap();
        if std::env::var("IRIS_MAX_CONCURRENT_COMPILES").is_err() {
            assert_eq!(gate.available_permits(), 0);
        }
    }
}
