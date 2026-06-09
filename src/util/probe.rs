use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// A plain TCP connect within the probe budget: confirms something is listening
/// on the mlx port. API-level failures surface on the first real call; a full
/// HTTP round trip doesn't fit the 200ms budget.
pub fn mlx_reachable(endpoint: &str) -> bool {
    port_reachable(endpoint)
}

/// TCP reachability for the TEI embeddings server, same 200ms budget as the mlx
/// probe. Used by `vault tei start|status` to decide whether to spawn and by
/// `vault index sync` preflight. A successful connect means something is
/// listening; dim/model correctness is confirmed separately by
/// `TeiEmbedder::verify_against_server`.
pub fn tei_reachable(endpoint: &str) -> bool {
    port_reachable(endpoint)
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
    addrs.into_iter().any(|addr| TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok())
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
        assert_eq!(socket_authority("http://localhost:8080").as_deref(), Some("localhost:8080"));
        assert_eq!(socket_authority("http://localhost").as_deref(), Some("localhost:8080"));
        assert_eq!(
            socket_authority("http://127.0.0.1:9000/v1/models").as_deref(),
            Some("127.0.0.1:9000")
        );
        assert_eq!(socket_authority("").as_deref(), None);
    }

    #[test]
    fn mlx_reachable_returns_false_for_unreachable_port() {
        // Port 1 is privileged and not served — the probe fails fast.
        assert!(!mlx_reachable("http://127.0.0.1:1"));
    }
}
