//! Peer identity. A trusted client certificate or a bound ServiceAccount token
//! is the only credential; impersonation is rejected rather than ignored.
use crate::{transport::Peer, Api, Failure, Result};
use axum::{body::Body, http::Request};
use h3s_auth::User;

pub(crate) fn forbid_impersonation(request: &Request<Body>) -> Result<()> {
    if request
        .headers()
        .keys()
        .any(|h| h.as_str().starts_with("impersonate-"))
    {
        return Err(Failure::new(
            403,
            "Forbidden",
            "impersonation is not enabled",
        ));
    }
    Ok(())
}

/// The bearer credential a request presents, if any. Extracted before any await
/// so the (not `Sync`) request body is never held across one.
pub(crate) fn credential(request: &Request<Body>) -> Result<Option<String>> {
    let mut headers = request.headers().get_all("authorization").iter();
    let Some(header) = headers.next() else {
        return Ok(None);
    };
    let value = header
        .to_str()
        .map_err(|_| Failure::new(401, "Unauthorized", "invalid bound token"))?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| Failure::new(401, "Unauthorized", "credentials must use Bearer"))?;
    if headers.next().is_some() {
        return Err(Failure::new(
            401,
            "Unauthorized",
            "exactly one credential is accepted",
        ));
    }
    if token.len() < 32 {
        return Err(Failure::new(401, "Unauthorized", "invalid bound token"));
    }
    Ok(Some(token.to_owned()))
}

/// A presented token is the identity; a certificate is not consulted then.
pub(crate) async fn authenticate(
    api: &Api,
    peer: Peer,
    credential: Option<String>,
) -> Result<User> {
    match credential {
        Some(token) => crate::token::authenticate(api, &token).await,
        None => peer.0.ok_or_else(|| {
            Failure::new(
                401,
                "Unauthorized",
                "a trusted client certificate or bound token is required",
            )
        }),
    }
}
