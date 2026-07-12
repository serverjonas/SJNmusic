//! Authentication helpers for the optional bearer-token scheme.
//!
//! The daemon can be configured to require `Authorization: Bearer <token>`
//! for any request whose peer is not a loopback address. This module owns
//! the three primitives every other auth site relies on:
//!
//! * [`generate_token`] — 32 cryptographically random bytes encoded as
//!   URL-safe unpadded base64 (43 chars, header-safe).
//! * [`constant_time_eq`] — string compare that does not short-circuit on
//!   mismatched prefixes/lengths, so a guesser's per-token timing leak is
//!   bounded to the constant overhead of the loop.
//! * [`is_local_request`] — true iff the request's TCP peer is in the
//!   loopback space (including IPv4-mapped IPv6 like `::ffff:127.0.0.1`,
//!   which `IpAddr::is_loopback` alone does not classify as loopback).
//!
//! [`bearer_from_request`] is the only parse path the HTTP layer uses to
//! extract the credential string, and [`request_is_authorized`] packages
//! the full "is this caller allowed?" decision so the dispatcher can ask
//! one boolean question per route.
//!
//! These helpers never log the credential. The HTTP server's rejection
//! paths must call [`request_is_authorized`] (or `bearer_from_request`
//! + [`constant_time_eq`]) and never `debug!` the bearer header itself:
//! leaking the value to disk or stdout would defeat the entire scheme.

use std::net::{IpAddr, SocketAddr};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use tiny_http::Request;

/// Length of the URL-safe-base64 encoding of 32 bytes (256 / 6 = 43
/// chars, no padding). Mirrored as a constant in the unit tests below.
const TOKEN_LEN: usize = 43;

/// Generate a fresh 32-byte bearer token. Output is 43 chars of URL-safe
/// unpadded base64, drawn from `rand::thread_rng()` which is itself
/// backed by `getrandom` on Linux — adequate for a single shared secret
/// against non-state-level attackers.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Constant-time equality on string slices. We do NOT short-circuit when
/// lengths differ: the loop runs over `max(a.len(), b.len())` and folds
/// the length mismatch into the same XOR-accumulator. For tokens the
/// legitimate length is a 43-char public constant, so even the
/// short-circuit-on-length approach wouldn't give an attacker useful
/// information — but defending against the corner case costs almost
/// nothing here and keeps the timing profile uniform.
///
/// The `core::hint::black_box` calls and a final `Ordering::SeqCst`
/// compiler fence keep LLVM from recognising the "compute a single
/// boolean, all of whose inputs feed through `|=`" idiom and
/// optimising it down to a short-circuiting `==` in release builds.
/// Defense in depth against an optimisation that would re-introduce
/// the very timing channel this function exists to suppress.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    use std::sync::atomic::{compiler_fence, Ordering};
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let n = a_bytes.len().max(b_bytes.len());
    let mut diff: u8 = (a_bytes.len() != b_bytes.len()) as u8;
    for i in 0..n {
        let x = a_bytes.get(i).copied().unwrap_or(0);
        let y = b_bytes.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    let diff = black_box_u8(diff);
    compiler_fence(Ordering::SeqCst);
    diff == 0
}

#[inline]
fn black_box_u8(x: u8) -> u8 {
    // `black_box` is a hint to the optimiser: the value may be observed
    // by an external observer and so should not be elided or folded.
    std::hint::black_box(x)
}

/// True iff `req`'s peer address is the IPv4 loopback subnet (including
/// `127.0.0.0/8` in principle, though tiny_http's `remote_addr` always
/// reports `127.0.0.1` on Linux), `::1`, OR an IPv4-mapped IPv6 address
/// like `::ffff:127.0.0.1` that the OS produces when a daemon with a
/// dual-stack socket accepts a v4 client.
pub fn is_local_request(req: &Request) -> bool {
    match req.remote_addr() {
        Some(addr) => peer_is_loopback(*addr),
        None => false,
    }
}

fn peer_is_loopback(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.to_ipv4_mapped()
                    .map(|v4| v4.is_loopback())
                    .unwrap_or(false)
        }
    }
}

/// Extract the bearer token from an `Authorization` header. Returns
/// `None` if the header is absent, falls outside the Bearer scheme
/// (e.g. Basic / Digest), or carries an empty token. Both the header
/// name and the scheme word are matched case-insensitively per RFC 7235.
pub fn bearer_from_request(req: &Request) -> Option<String> {
    for header in req.headers() {
        if !header.field.equiv("Authorization") {
            continue;
        }
        let trimmed = header.value.as_str().trim_start();
        const PREFIX: &str = "Bearer ";
        if trimmed.len() >= PREFIX.len()
            && trimmed[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
        {
            let token = trimmed[PREFIX.len()..].trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
        // Authorization header was present but malformed (wrong scheme,
        // empty token, etc). Do NOT silently fall through to treat it
        // as "no credential"; the caller's auth check will return 401.
        return None;
    }
    None
}

/// One-line auth decision used by the HTTP dispatcher for every non-auth
/// route. True if the request is from a loopback peer (always trusted),
/// OR if the daemon has authentication disabled, OR if the request
/// carries a `Bearer` header whose value matches the configured token
/// under [`constant_time_eq`].
pub fn request_is_authorized(req: &Request, auth_token: Option<&str>) -> bool {
    if is_local_request(req) {
        return true;
    }
    let Some(expected) = auth_token else {
        // No token configured → authentication is off entirely.
        return true;
    };
    bearer_from_request(req)
        .map(|t| constant_time_eq(&t, expected))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_matches_equal_strings() {
        assert!(constant_time_eq("", ""));
        assert!(constant_time_eq("a", "a"));
        assert!(constant_time_eq("hello world", "hello world"));
    }

    #[test]
    fn constant_time_rejects_unequal_strings() {
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("ab", "abc"));
        assert!(!constant_time_eq("", "x"));
        assert!(!constant_time_eq("x", ""));
    }

    #[test]
    fn generate_produces_distinct_well_formed_tokens() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert_eq!(t1.len(), TOKEN_LEN);
        assert_eq!(t2.len(), TOKEN_LEN);
        assert_ne!(t1, t2, "32 random bytes should never collide");
        // URL-safe base64 alphabet: A-Z a-z 0-9 - _.
        for c in t1.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "token contained non-url-safe char: {c:?}"
            );
        }
    }

    #[test]
    fn peer_loopback_accepts_v4_and_v6_variants() {
        let v4_loopback: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let v4_other: SocketAddr = "10.0.0.1:1234".parse().unwrap();
        let v6_loopback: SocketAddr = "[::1]:1234".parse().unwrap();
        let v6_other: SocketAddr = "[2001:db8::1]:1234".parse().unwrap();
        let v4_mapped_loopback: SocketAddr =
            std::net::SocketAddrV6::new("::ffff:127.0.0.1".parse().unwrap(), 1234, 0, 0)
                .into();
        assert!(peer_is_loopback(v4_loopback));
        assert!(!peer_is_loopback(v4_other));
        assert!(peer_is_loopback(v6_loopback));
        assert!(!peer_is_loopback(v6_other));
        assert!(
            peer_is_loopback(v4_mapped_loopback),
            "IPv4-mapped IPv6 must classify as loopback"
        );
    }
}
