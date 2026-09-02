//! The process-wide HTTP client.
//!
//! **One client, shared by every backend.** Each `reqwest::blocking::Client`
//! owns a connection pool and spawns its own background runtime thread —
//! measured at exactly one thread per client. Building one per backend meant
//! separate pools that cannot share a keep-alive connection even when they talk
//! to the same host, and separate runtimes competing for the same work, none of
//! it visible at any call site.
//!
//! Nothing required them to be separate. Every construction site in this crate
//! built a byte-identical `Client` differing only in `.timeout(...)`, and
//! reqwest takes a per-request timeout that overrides the client default — so
//! the one thing that differed never needed its own client. Callers hold
//! `&'static Client` and pass their own timeout per request.
//!
//! Initialization is lazy: a process that makes no HTTP call never builds it,
//! and never pays for the thread.

use std::sync::OnceLock;

use reqwest::blocking::Client;

static CLIENT: OnceLock<Option<Client>> = OnceLock::new();

/// The shared client, or `None` if it could not be constructed.
///
/// Deliberately carries **no client-level timeout**: a default here would apply
/// to every caller, and each one has its own budget (the router's 3s hot-path
/// limit, the classifier's 300s, TEI's embed timeout, the 1.5s health probe).
/// Every caller must set `.timeout(..)` on the request.
pub(crate) fn shared() -> Option<&'static Client> {
    CLIENT
        .get_or_init(|| Client::builder().build().ok())
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the module: repeated calls must hand back the *same*
    /// client, not an equivalent one. A per-call client would pass any
    /// behavioural test while still spawning a thread and a pool each time, so
    /// pointer identity is the only assertion that pins it.
    #[test]
    fn shared_returns_one_client_for_the_process() {
        let a = shared().expect("client builds");
        let b = shared().expect("client builds");
        assert!(
            std::ptr::eq(a, b),
            "each call built a new client — the pool and its runtime thread are not shared"
        );
    }
}
