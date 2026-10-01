//! Server-Side Apply against the shipped API, with a temporary data directory.
mod common;
use common::Server;
use http_body_util::BodyExt;
use serde_json::{json, Value};

const DEPLOYMENT: &str = "/apis/apps/v1/namespaces/default/deployments/web";
const APPLY: &str = "application/apply-patch+json";

/// `kubectl apply --server-side` with the manager owning what it writes.
async fn apply(s: &Server, path: &str, manager: &str, force: bool, value: Value) -> (u16, Value) {
    let query = if force {
        format!("?fieldManager={manager}&force=true")
    } else {
        format!("?fieldManager={manager}")
    };
    let response = s
        .raw(
            s.admin(),
            "PATCH",
            &format!("{path}{query}"),
            value,
            &[("Content-Type", APPLY)],
        )
        .await;
    let code = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap())
}
fn deployment(replicas: i64, image: &str) -> Value {
    json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web","labels":{"app":"web"}},
        "spec":{"replicas":replicas,"selector":{"matchLabels":{"app":"web"}},
        "template":{"metadata":{"labels":{"app":"web"}},"spec":{
            "securityContext":{"runAsNonRoot":true,"runAsUser":65534,"seccompProfile":{"type":"RuntimeDefault"}},
            "containers":[{"name":"web","image":image,"securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]}}}})
}
fn manager_entry<'a>(object: &'a Value, manager: &str) -> &'a Value {
    object["metadata"]["managedFields"]
        .as_array()
        .unwrap_or_else(|| panic!("managedFields missing: {object}"))
        .iter()
        .find(|entry| entry["manager"] == manager)
        .unwrap_or_else(|| panic!("entry for {manager} missing: {object}"))
}

#[tokio::test]
async fn applies_from_two_managers_own_and_update_their_own_fields() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    // Manager a creates the Deployment and owns what it declared.
    let (code, created) = apply(&s, DEPLOYMENT, "a", false, deployment(2, "web:v1")).await;
    assert_eq!(code, 201, "{created}");
    assert_eq!(
        created["spec"]["selector"]["matchLabels"]["app"], "web",
        "{created}"
    );
    assert_eq!(
        created["spec"]["template"]["spec"]["containers"][0]["name"], "web",
        "{created}"
    );
    let entry = manager_entry(&created, "a");
    assert_eq!(entry["operation"], "Apply");
    assert_eq!(entry["fieldsType"], "FieldsV1");
    assert_eq!(
        entry["fieldsV1"]["f:spec"]["f:replicas"],
        json!({}),
        "{entry}"
    );
    assert_eq!(
        entry["fieldsV1"]["f:spec"]["f:template"]["f:spec"]["f:containers"]["k:{\"name\":\"web\"}"]
            ["f:image"],
        json!({}),
        "{entry}"
    );
    // Manager b applies a field of its own that a never declared. Manager a's
    // entry, and every value a owns, are untouched.
    let (code, second) = apply(
        &s,
        DEPLOYMENT,
        "b",
        false,
        json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web","annotations":{"team":"b"}},"spec":{"template":{"metadata":{"annotations":{"team":"b"}}}}}),
    )
    .await;
    assert_eq!(code, 200, "{second}");
    assert_eq!(second["metadata"]["annotations"]["team"], "b");
    assert_eq!(second["spec"]["replicas"], 2);
    assert_eq!(
        second["spec"]["template"]["spec"]["containers"][0]["image"],
        "web:v1"
    );
    assert_eq!(manager_entry(&second, "a"), entry);
    assert_eq!(
        manager_entry(&second, "b")["fieldsV1"]["f:metadata"]["f:annotations"]["f:team"],
        json!({})
    );
    // Manager a reapplies its own image: only that changes, and b's field and
    // value survive untouched.
    let (code, third) = apply(&s, DEPLOYMENT, "a", false, deployment(2, "web:v2")).await;
    assert_eq!(code, 200, "{third}");
    assert_eq!(
        third["spec"]["template"]["spec"]["containers"][0]["image"],
        "web:v2"
    );
    assert_eq!(third["spec"]["replicas"], 2);
    assert_eq!(third["metadata"]["annotations"]["team"], "b");
    assert_eq!(
        manager_entry(&third, "b")["fieldsV1"]["f:metadata"]["f:annotations"]["f:team"],
        json!({})
    );
    // A field two managers set to different values is a conflict until the
    // request forces it, and then ownership moves.
    let (code, conflict) = apply(
        &s,
        DEPLOYMENT,
        "b",
        false,
        json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web"},"spec":{"replicas":4}}),
    )
    .await;
    assert_eq!(code, 409, "{conflict}");
    assert!(
        conflict["message"].as_str().unwrap().contains("managed by"),
        "{conflict}"
    );
    let (code, forced) = apply(
        &s,
        DEPLOYMENT,
        "b",
        true,
        json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web"},"spec":{"replicas":4}}),
    )
    .await;
    assert_eq!(code, 200, "{forced}");
    assert_eq!(forced["spec"]["replicas"], 4);
}
