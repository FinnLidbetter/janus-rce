use rocket::http::Status;
use rocket::request::{FromRequest, Outcome, Request};
use subtle::ConstantTimeEq;

use crate::config::LoadedConfig;

/// Zero-sized marker type. Its presence as a route parameter means the request
/// carried a valid bearer token.
pub struct AuthToken;

#[derive(Debug)]
pub enum AuthError {
    Missing,
    Malformed,
    Invalid,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AuthToken {
    type Error = AuthError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let Some(header) = req.headers().get_one("Authorization") else {
            return Outcome::Error((Status::Unauthorized, AuthError::Missing));
        };

        let Some(token) = header.strip_prefix("Bearer ") else {
            return Outcome::Error((Status::Unauthorized, AuthError::Malformed));
        };

        let config = req
            .rocket()
            .state::<LoadedConfig>()
            .expect("LoadedConfig must be in managed state");

        if constant_time_eq(config.token.as_bytes(), token.as_bytes()) {
            Outcome::Success(AuthToken)
        } else {
            Outcome::Error((Status::Unauthorized, AuthError::Invalid))
        }
    }
}

/// Constant-time byte-slice equality.
///
/// `subtle::ConstantTimeEq` for slices only guarantees constant time when both
/// slices have the same length. When lengths differ the comparison definitively
/// returns `false`, but the length check itself is a potential timing side
/// channel. We mitigate this by always running a constant-time comparison of
/// `provided` against itself (a no-op) when lengths differ, keeping the branch
/// behaviour as uniform as possible for an attacker measuring response time on
/// a loopback interface.
fn constant_time_eq(expected: &[u8], provided: &[u8]) -> bool {
    let lengths_match = expected.len() == provided.len();

    // Always perform a constant-time comparison to avoid a pure length-based
    // timing signal. When lengths differ we compare `provided` to itself
    // (always equal) and then mask the result with `lengths_match` (false).
    let bytes_equal: bool = if lengths_match {
        expected.ct_eq(provided).into()
    } else {
        // Dummy comparison of equal-length slices so the cost is similar.
        provided.ct_eq(provided).into()
    };

    lengths_match && bytes_equal
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn equal_bytes() {
        assert!(constant_time_eq(b"secret", b"secret"));
    }

    #[test]
    fn different_content_same_len() {
        assert!(!constant_time_eq(b"secret", b"Secret"));
    }

    #[test]
    fn different_lengths() {
        assert!(!constant_time_eq(b"secret", b"sec"));
    }

    #[test]
    fn both_empty() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn one_empty() {
        assert!(!constant_time_eq(b"secret", b""));
    }
}
