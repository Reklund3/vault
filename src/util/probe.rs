use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// A plain TCP connect within the probe budget: confirms something is listening
/// on the mlx port. API-level failures surface on the first real call; a full
/// HTTP round trip doesn't fit the 200ms budget.
///
/// Still TCP-only, unlike [`tei_reachable`], and that gap is review D4's MLX
/// half — tracked under P1, because this one runs at process startup in `auto`
/// mode where the 200ms budget is the point. It cannot distinguish "reachable"
/// from "reachable but too slow to meet the hook's timeout".
pub fn mlx_reachable(endpoint: &str) -> bool {
    port_reachable(endpoint)
}

/// How long the TEI health probe waits. Larger than [`PROBE_TIMEOUT`] because
/// this is a real HTTP round trip rather than a TCP handshake, and it runs in
/// interactive commands rather than on the hook's hot path — but still short
/// enough that an unreachable server fails fast.
#[cfg(feature = "cli")]
const HEALTH_TIMEOUT: Duration = Duration::from_millis(1500);

/// Whether the TEI embeddings server is **serving**, not merely bound.
///
/// This is an HTTP `GET /health`, deliberately not a TCP connect (review D4).
/// Docker publishes a container's port the moment the container starts, which
/// is well before TEI has loaded weights and bound its HTTP server — measured
/// at ~28s on a cold start. A TCP probe therefore reports "reachable" for a
/// server that cannot answer a single embed, and `vault tei start` says so out
/// loud while the following `index sync` hard-errors.
///
/// A non-2xx reply still counts as serving: the process is up and speaking
/// HTTP, which is what this predicate is for. Dim and model correctness are
/// confirmed separately by `TeiEmbedder::verify_against_server`.
#[cfg(feature = "cli")]
pub fn tei_reachable(endpoint: &str) -> bool {
    let url = format!("{}/health", endpoint.trim_end_matches('/'));
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
    else {
        return false;
    };
    client.get(&url).send().is_ok()
}

/// Shared TCP-connect core. The endpoint's authority must carry an explicit
/// port (both the mlx and TEI config endpoints do); a portless authority falls
/// back to `socket_authority`'s 8080 default, which is only meaningful for mlx.
fn port_reachable(endpoint: &str) -> bool {
    let Some(authority) = socket_authority(endpoint) else {
        return false;
    };
    let Ok(addrs) = authority.to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr| TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok())
}

/// Extract `host:port` from an `http://host:port/...` endpoint, defaulting the
/// port to 8080 (mlx_lm.server's default) when absent.
pub(crate) fn socket_authority(endpoint: &str) -> Option<String> {
    let rest = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(endpoint);
    let authority = rest.split('/').next()?.trim();
    if authority.is_empty() {
        None
    } else if authority.contains(':') {
        Some(authority.to_string())
    } else {
        Some(format!("{authority}:8080"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_authority_parses_forms() {
        assert_eq!(
            socket_authority("http://localhost:8080").as_deref(),
            Some("localhost:8080")
        );
        assert_eq!(
            socket_authority("http://localhost").as_deref(),
            Some("localhost:8080")
        );
        assert_eq!(
            socket_authority("http://127.0.0.1:9000/v1/models").as_deref(),
            Some("127.0.0.1:9000")
        );
        assert_eq!(socket_authority("").as_deref(), None);
    }

    /// D4: the TEI probe must be an HTTP health check, not a TCP connect.
    ///
    /// A listener that accepts the connection and then says nothing is exactly
    /// what Docker gives you between publishing the port and TEI binding its
    /// HTTP server — measured at ~28s on a cold start. The old TCP probe called
    /// that "reachable", `vault tei start` announced it, and the next
    /// `index sync` hard-errored against it.
    #[test]
    fn tei_probe_rejects_a_socket_that_accepts_but_never_serves() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        // Deliberately never accept: connections queue in the backlog, so a TCP
        // connect succeeds while no HTTP response can ever arrive.
        let endpoint = format!("http://127.0.0.1:{port}");

        assert!(
            port_reachable(&endpoint),
            "precondition: TCP must connect, or this proves nothing"
        );
        assert!(
            !tei_reachable(&endpoint),
            "a socket that never answers HTTP is not serving"
        );
        drop(listener);
    }

    #[test]
    fn tei_probe_returns_false_for_a_closed_port() {
        assert!(!tei_reachable("http://127.0.0.1:1"));
    }

    #[test]
    fn mlx_reachable_returns_false_for_unreachable_port() {
        // Port 1 is privileged and not served — the probe fails fast.
        assert!(!mlx_reachable("http://127.0.0.1:1"));
    }
}
