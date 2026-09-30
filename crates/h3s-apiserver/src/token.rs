//! Bound ServiceAccount tokens. This single-server API is the only issuer and
//! the only verifier, so a token is an opaque high-entropy secret whose claims
//! live in the registry: issuing records them, verifying looks them up, and
//! expiry, ServiceAccount deletion, or record deletion revokes them. Nothing
//! else verifies a token, so no signing key is published or needed.
use crate::{
    authz, http::Query, key, object, resources::Target, selectors::Selection, stored, Api, Failure,
    Result,
};
use axum::{
    body::Body,
    http::Request,
    response::{IntoResponse, Response},
    Json,
};
use h3s_auth::User;
use serde_json::{json, Value};

/// The audience this API accepts when a request names none.
pub const API_AUDIENCE: &str = "https://kubernetes.default.svc";
/// Bound-token lifetimes, in the Kubernetes bounded range.
pub const DEFAULT_SECONDS: i64 = 3607;
const MIN_SECONDS: i64 = 600;
const MAX_SECONDS: i64 = 86_400;
const MAX_AUDIENCES: usize = 10;
const MAX_TOKEN: usize = 512;
/// API-private records. No resource route can address this prefix.
const PREFIX: &str = "/registry/h3s-bound-tokens/";

fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(422, "Invalid", message)
}
fn unauthorized(message: &'static str) -> Failure {
    Failure::new(401, "Unauthorized", message)
}
fn denied(message: &'static str) -> Failure {
    Failure::new(403, "Forbidden", message)
}
fn bounded(value: &str) -> bool {
    (1..=256).contains(&value.len()) && !value.bytes().any(|b| b.is_ascii_control())
}
/// The record key for a presented token: its digest, never the token itself.
fn record_key(token: &str) -> Result<h3s_storage::StoreKey> {
    let digest = h3s_auth::bootstrap::digest(token);
    key(format!(
        "{PREFIX}{}",
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}
fn seconds() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
fn expiry(seconds: i64) -> Result<String> {
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .ok_or_else(|| invalid("invalid token lifetime"))
}

/// How a token is bound to a running Pod.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Bound {
    kind: String,
    name: String,
    uid: String,
}

/// `POST /api/v1/namespaces/{ns}/serviceaccounts/{name}/token`.
pub async fn create(
    api: &Api,
    user: &User,
    target: &Target,
    query: &Query,
    request: Request<Body>,
) -> Result<Response> {
    if request.method() != "POST" {
        return Err(Failure::new(
            405,
            "MethodNotAllowed",
            "a bound token requires POST",
        ));
    }
    if query.params.keys().any(|key| key != "timeout") {
        return Err(Failure::new(
            400,
            "BadRequest",
            "a bound token accepts only the client timeout parameter",
        ));
    }
    let namespace = target
        .namespace
        .as_deref()
        .ok_or_else(|| invalid("a bound token requires a namespace"))?;
    let account = target
        .name
        .as_deref()
        .ok_or_else(|| invalid("a bound token requires a service account"))?;
    // Authorization uses the ordinary RBAC and node paths for this subresource.
    authz::resource(
        api,
        user,
        target,
        "create",
        Selection::parse("", "", target.resource.kind)?,
    )
    .await?;
    let (content_type, bytes) = crate::http::body(request).await?;
    let value = crate::wire::decode(&bytes, &content_type)?;
    if value["kind"] != "TokenRequest" || value["apiVersion"] != "authentication.k8s.io/v1" {
        return Err(invalid(
            "a bound token requires an authentication.k8s.io/v1 TokenRequest",
        ));
    }
    let spec = &value["spec"];
    if spec.as_object().is_none_or(|m| {
        m.keys()
            .any(|k| !["audiences", "expirationSeconds", "boundObjectRef"].contains(&k.as_str()))
    }) {
        return Err(invalid("unsupported TokenRequest spec field"));
    }
    let mut audiences = Vec::new();
    for audience in spec["audiences"].as_array().into_iter().flatten() {
        let audience = audience.as_str().filter(|a| bounded(a)).ok_or_else(|| {
            invalid("token audiences must be non-empty printable strings up to 256 bytes")
        })?;
        if !audiences.iter().any(|a| a == audience) {
            audiences.push(audience.to_owned());
        }
    }
    if audiences.is_empty() {
        audiences.push(API_AUDIENCE.into());
    }
    if audiences.len() > MAX_AUDIENCES {
        return Err(invalid(format!(
            "a token may name at most {MAX_AUDIENCES} audiences"
        )));
    }
    let expiration = match spec["expirationSeconds"].as_i64() {
        None if spec["expirationSeconds"].is_null() => DEFAULT_SECONDS,
        Some(seconds) if (MIN_SECONDS..=MAX_SECONDS).contains(&seconds) => seconds,
        _ => {
            return Err(invalid(format!(
                "expirationSeconds must be between {MIN_SECONDS} and {MAX_SECONDS}"
            )))
        }
    };
    let bound = match &spec["boundObjectRef"] {
        Value::Null => None,
        reference => {
            if reference.as_object().is_none_or(|m| {
                m.keys()
                    .any(|k| !["apiVersion", "kind", "name", "uid"].contains(&k.as_str()))
            }) {
                return Err(invalid("unsupported boundObjectRef field"));
            }
            let bound = Bound {
                kind: reference["kind"].as_str().unwrap_or("").to_owned(),
                name: reference["name"].as_str().unwrap_or("").to_owned(),
                uid: reference["uid"].as_str().unwrap_or("").to_owned(),
            };
            if bound.kind != "Pod"
                || !h3s_api::valid_node_name(&bound.name)
                || bound.uid.is_empty()
                || bound.uid.len() > 128
                || bound.uid.contains('\0')
            {
                return Err(invalid(
                    "boundObjectRef must name a Pod with kind, name and uid",
                ));
            }
            Some(bound)
        }
    };
    if let Some(node) = user.node_name() {
        // A node may only mint a token for a Pod it is running, exactly as the
        // node authorizer narrows its other Pod relationships.
        let reference = bound
            .as_ref()
            .ok_or_else(|| denied("a node may only mint a token bound to a Pod it is running"))?;
        let pod = api
            .store
            .get(&key(format!(
                "/registry/pods/{namespace}/{}",
                reference.name
            ))?)
            .await?
            .map(object)
            .transpose()?
            .ok_or_else(|| denied("a node may only mint a token bound to a Pod it is running"))?;
        if pod["metadata"]["uid"].as_str() != Some(reference.uid.as_str())
            || pod["spec"]["nodeName"].as_str() != Some(node)
        {
            return Err(denied(
                "a node may only mint a token bound to a Pod it is running",
            ));
        }
    }
    let account_key = key(format!("/registry/serviceaccounts/{namespace}/{account}"))?;
    let account_object = api
        .store
        .get(&account_key)
        .await?
        .map(object)
        .transpose()?
        .ok_or_else(|| Failure::new(404, "NotFound", "service account not found"))?;
    if !account_object["metadata"]["deletionTimestamp"].is_null() {
        return Err(denied("service account is terminating"));
    }
    let issued = h3s_auth::bootstrap::random_secret()
        .map_err(|_| Failure::new(500, "InternalError", "token generation unavailable"))?;
    let expires = seconds()
        .checked_add(expiration)
        .ok_or_else(|| invalid("invalid token lifetime"))?;
    let record = json!({
        "version": 1,
        "namespace": namespace,
        "service_account": account,
        "uid": account_object["metadata"]["uid"],
        "audiences": audiences,
        "expiration": expires,
        "bound": bound.as_ref().map(|b| json!({"kind": b.kind, "name": b.name, "uid": b.uid})),
    });
    {
        let _guard = api.admission_writes.lock().await;
        api.store
            .create(stored(record_key(&issued)?, &record)?)
            .await?;
    }
    Ok((
        axum::http::StatusCode::CREATED,
        [("cache-control", "no-store")],
        Json(json!({
            "apiVersion": "authentication.k8s.io/v1",
            "kind": "TokenRequest",
            "metadata": value["metadata"],
            "spec": {
                "audiences": audiences,
                "expirationSeconds": expiration,
                "boundObjectRef": value["spec"]["boundObjectRef"],
            },
            "status": {
                "token": issued,
                "expirationTimestamp": expiry(expires)?,
            },
        })),
    )
        .into_response())
}

/// Verify a presented bearer token against its issued record.
pub async fn authenticate(api: &Api, token: &str) -> Result<User> {
    if !(32..=MAX_TOKEN).contains(&token.len()) || token.bytes().any(|b| !b.is_ascii_graphic()) {
        return Err(unauthorized("invalid bound token"));
    }
    // The record is API-private state, not a Kubernetes object.
    let stored = api
        .store
        .get(&record_key(token)?)
        .await?
        .ok_or_else(|| unauthorized("bound token is not valid"))?;
    let record: Value = serde_json::from_slice(&stored.value)
        .map_err(|_| unauthorized("bound token is not valid"))?;
    if record["version"] != 1 || record["expiration"].as_i64().is_none_or(|e| e < seconds()) {
        return Err(unauthorized("bound token is not valid"));
    }
    let namespace = record["namespace"]
        .as_str()
        .filter(|n| h3s_api::valid_node_name(n))
        .ok_or_else(|| unauthorized("bound token is not valid"))?;
    let account = record["service_account"]
        .as_str()
        .filter(|n| h3s_api::valid_node_name(n))
        .ok_or_else(|| unauthorized("bound token is not valid"))?;
    // A deleted ServiceAccount revokes its outstanding tokens.
    let current = api
        .store
        .get(&key(format!(
            "/registry/serviceaccounts/{namespace}/{account}"
        ))?)
        .await?
        .map(object)
        .transpose()?
        .ok_or_else(|| unauthorized("bound token is not valid"))?;
    if current["metadata"]["uid"] != record["uid"] {
        return Err(unauthorized("bound token is not valid"));
    }
    Ok(User {
        name: format!("system:serviceaccount:{namespace}:{account}"),
        groups: vec![
            "system:serviceaccounts".into(),
            format!("system:serviceaccounts:{namespace}"),
            "system:authenticated".into(),
        ],
    })
}
