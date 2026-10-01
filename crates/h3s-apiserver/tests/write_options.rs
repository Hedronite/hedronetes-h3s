//! Write-only request options and codecs, driven through the shipped API.
mod common;
use common::Server;
use http_body_util::BodyExt;
use serde_json::{json, Value};

/// A PATCH with an explicit content type, so the apply verb is exercised the
/// way a client that speaks it would.
async fn patch_as(s: &Server, path: &str, content_type: &str, value: Value) -> (u16, Value) {
    let response = s
        .raw(
            s.admin(),
            "PATCH",
            path,
            value,
            &[("Content-Type", content_type)],
        )
        .await;
    let code = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap())
}

fn nulls(value: &Value, path: &str, found: &mut Vec<String>) {
    match value {
        Value::Null => found.push(path.to_owned()),
        Value::Object(map) => {
            for (k, v) in map {
                nulls(v, &format!("{path}/{k}"), found);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                nulls(v, &format!("{path}/{i}"), found);
            }
        }
        _ => {}
    }
}
fn assert_canonical(value: &Value) {
    let mut found = Vec::new();
    nulls(value, "", &mut found);
    assert!(found.is_empty(), "null members {found:?} in {value}");
}

#[tokio::test]
async fn dry_run_is_rejected_on_writes_only() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let original = s.configmap("settings", "one").await;
    let list = "/api/v1/namespaces/team-a/configmaps";
    let path = "/api/v1/namespaces/team-a/configmaps/settings";
    // Reads ignore dryRun, as upstream does.
    let (code, page) = s
        .json(s.admin(), "GET", &format!("{list}?dryRun=All"), json!({}))
        .await;
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let (code, got) = s
        .json(s.admin(), "GET", &format!("{path}?dryRun=All"), json!({}))
        .await;
    assert_eq!(code, 200, "{got}");
    assert_eq!(got, original);
    let watch = s
        .raw(
            s.admin(),
            "GET",
            &format!("{list}?watch=true&dryRun=All&timeoutSeconds=1"),
            json!({}),
            &[],
        )
        .await;
    assert_eq!(watch.status(), 200);
    drop(watch);
    // Every mutation is refused before it can touch the registry.
    let create = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"other"},"data":{"value":"x"}});
    let update = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"settings","resourceVersion":original["metadata"]["resourceVersion"]},"data":{"value":"two"}});
    for (method, target, body) in [
        ("POST", list, create),
        ("PUT", path, update),
        ("DELETE", path, json!({})),
    ] {
        let (code, failure) = s
            .json(s.admin(), method, &format!("{target}?dryRun=All"), body)
            .await;
        assert_eq!(code, 400, "{method}: {failure}");
        assert_eq!(failure["reason"], "BadRequest", "{failure}");
    }
    let (code, failure) = s
        .patch(
            s.admin(),
            &format!("{path}?dryRun=All"),
            "application/merge-patch+json",
            json!({"data":{"value":"two"}}),
        )
        .await;
    assert_eq!(code, 400, "{failure}");
    assert_eq!(s.json(s.admin(), "GET", path, json!({})).await.1, original);
    assert_eq!(
        s.json(s.admin(), "GET", &format!("{list}/other"), json!({}))
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn patch_media_types_list_codecs_and_apply_is_a_separate_unimplemented_verb() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let original = s.configmap("settings", "one").await;
    let path = "/api/v1/namespaces/team-a/configmaps/settings";
    // The apply exercise uses its own object, so the rest of this test still
    // describes the object it created above.
    let s2 = s.json(s.admin(), "POST", "/api/v1/namespaces/team-a/configmaps", json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"applied"},"data":{"value":"one"}})).await;
    assert_eq!(s2.0, 201, "{s2:?}");
    let apply_path = "/api/v1/namespaces/team-a/configmaps/applied";
    let apply = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"applied"},"data":{"value":"two"}});
    // Server-side apply is a real verb now: it refuses a body it cannot decode
    // rather than guessing at it, it needs the manager that owns what it
    // writes, and with one it merges and records ownership.
    let (code, failure) = patch_as(
        &s,
        apply_path,
        "application/apply-patch+yaml",
        apply.clone(),
    )
    .await;
    assert_eq!(code, 415, "{failure}");
    let (code, failure) = patch_as(
        &s,
        apply_path,
        "application/apply-patch+json",
        apply.clone(),
    )
    .await;
    assert_eq!(code, 400, "{failure}");
    assert_eq!(failure["reason"], "BadRequest", "{failure}");
    let (code, owned) = patch_as(
        &s,
        &format!("{apply_path}?fieldManager=tests"),
        "application/apply-patch+json",
        apply.clone(),
    )
    .await;
    assert_eq!(code, 200, "{owned}");
    assert_eq!(owned["data"]["value"], "two");
    assert_eq!(owned["metadata"]["managedFields"][0]["manager"], "tests");
    assert_eq!(owned["metadata"]["managedFields"][0]["operation"], "Apply");
    let (code, failure) = s
        .patch(
            s.admin(),
            path,
            "application/example+json",
            json!({"data":{"value":"two"}}),
        )
        .await;
    assert_eq!(code, 415, "{failure}");
    let message = failure["message"].as_str().unwrap();
    for codec in [
        "application/json-patch+json",
        "application/merge-patch+json",
        "application/strategic-merge-patch+json",
    ] {
        assert!(message.contains(codec), "{message}");
    }
    assert!(!message.contains("apply"), "{message}");
    assert_eq!(s.json(s.admin(), "GET", path, json!({})).await.1, original);
}

#[tokio::test]
async fn objects_are_normalized_once_and_stored_without_nulls() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    // No initContainers, ports or volumes: the strategy must not leave them
    // behind as null now that nothing re-normalizes after it.
    let pod = json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"web"},"spec":{"automountServiceAccountToken":false,"securityContext":{"runAsNonRoot":true,"runAsUser":65534,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"web","image":"example.invalid/web:v1","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]}});
    let (code, created) = s
        .json(s.admin(), "POST", "/api/v1/namespaces/default/pods", pod)
        .await;
    assert_eq!(code, 201, "{created}");
    assert_canonical(&created);
    assert!(created["spec"].get("initContainers").is_none());
    assert!(created["spec"]["containers"][0].get("ports").is_none());
    // Image updates compare the new spec with the stored one; a canonical
    // store is what keeps that comparison honest.
    let mut updated = created.clone();
    updated["spec"]["containers"][0]["image"] = "example.invalid/web:v2".into();
    let (code, updated) = s
        .json(
            s.admin(),
            "PUT",
            "/api/v1/namespaces/default/pods/web",
            updated,
        )
        .await;
    assert_eq!(code, 200, "{updated}");
    assert_canonical(&updated);
    assert_eq!(
        updated["spec"]["containers"][0]["image"],
        "example.invalid/web:v2"
    );
    // ServiceAccount admission runs after the strategy and must leave the
    // spec canonical too; it never projects a token.
    let projected = json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"projected"},"spec":{"securityContext":{"runAsNonRoot":true,"runAsUser":65534,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"web","image":"example.invalid/web:v1","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]}});
    let (code, projected) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/default/pods",
            projected,
        )
        .await;
    assert_eq!(code, 201, "{projected}");
    assert_canonical(&projected);
    assert!(projected["spec"].get("volumes").is_none(), "{projected}");
    let (code, patched) = s
        .patch(
            s.admin(),
            "/api/v1/namespaces/default/pods/projected",
            "application/json-patch+json",
            json!([{"op":"replace","path":"/spec/containers/0/image","value":"example.invalid/web:v2"}]),
        )
        .await;
    assert_eq!(code, 200, "{patched}");
    assert_canonical(&patched);
    // A typed-invalid body is still refused at decode.
    let (code, failure) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/default/configmaps",
            json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bad"},"data":{"value":42}}),
        )
        .await;
    assert_eq!(code, 422, "{failure}");
    // Namespace status updates copy an absent spec as absent, not null.
    s.namespace("team-a").await;
    let (_, mut ns) = s
        .json(s.admin(), "GET", "/api/v1/namespaces/team-a", json!({}))
        .await;
    assert!(ns.get("spec").is_none(), "{ns}");
    ns["status"]["phase"] = "Active".into();
    let (code, ns) = s
        .json(s.admin(), "PUT", "/api/v1/namespaces/team-a/status", ns)
        .await;
    assert_eq!(code, 200, "{ns}");
    assert_canonical(&ns);
    assert!(ns.get("spec").is_none(), "{ns}");
}
