//! Escalation prevention replaces the bootstrap-administrator-only 403 on RBAC
//! writes: a principal may change RBAC objects within what it already holds, an
//! administrator still bootstraps, and an ordinary granted write succeeds.
mod common;
use common::Server;
use serde_json::{json, Value};

const RBAC: &str = "/apis/rbac.authorization.k8s.io/v1";

async fn create(s: &Server, path: &str, value: Value) -> (u16, Value) {
    s.json(s.admin(), "POST", path, value).await
}
fn cluster_role(name: &str, rules: Value) -> Value {
    json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":name},"rules":rules})
}

#[tokio::test]
async fn escalation_prevents_grants_a_principal_does_not_hold() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    // Bootstrap: an administrator still writes RBAC objects. This role is the
    // one that lets an ordinary principal write RBAC in a namespace.
    for (path, value) in [
        (
            format!("{RBAC}/clusterroles"),
            cluster_role(
                "developer",
                json!([
                    {"apiGroups":["rbac.authorization.k8s.io"],"resources":["roles","rolebindings"],"verbs":["get","list","watch","create","update","patch","delete"]},
                    {"apiGroups":[""],"resources":["configmaps"],"verbs":["get","list","watch"]}
                ]),
            ),
        ),
        (
            format!("{RBAC}/clusterrolebindings"),
            json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBinding","metadata":{"name":"developer"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"developer"},"subjects":[{"kind":"Group","apiGroup":"rbac.authorization.k8s.io","name":"developers"}]}),
        ),
        (
            format!("{RBAC}/clusterroles"),
            cluster_role(
                "super",
                json!([{"apiGroups":["*"],"resources":["*"],"verbs":["*"]}]),
            ),
        ),
    ] {
        let (code, created) = create(&s, &path, value).await;
        assert_eq!(code, 201, "{path}: {created}");
    }
    let developer = || {
        s.pki
            .client_config(Some(
                &s.pki.issue_client("dev", Some("developers")).unwrap(),
            ))
            .unwrap()
    };
    let roles = format!("{RBAC}/namespaces/team-a/roles");
    // An ordinary write the principal is granted succeeds.
    let (code, reader) = create(
        &s,
        &roles,
        json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":"reader"},"rules":[{"apiGroups":[""],"resources":["configmaps"],"verbs":["get","list","watch"]}]}),
    )
    .await;
    assert_eq!(code, 201, "{reader}");
    // ... and binding the role it just made is equally within its rights.
    let (code, binding) = create(
        &s,
        &format!("{RBAC}/namespaces/team-a/rolebindings"),
        json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":"reader"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":"reader"},"subjects":[{"kind":"Group","apiGroup":"rbac.authorization.k8s.io","name":"developers"}]}),
    )
    .await;
    assert_eq!(code, 201, "{binding}");
    // Creating a Role the principal could not itself use is escalation.
    for (name, rules) in [
        (
            "wildcard",
            json!([{"apiGroups":["rbac.authorization.k8s.io"],"resources":["clusterroles"],"verbs":["*"]}]),
        ),
        (
            "secrets",
            json!([{"apiGroups":[""],"resources":["secrets"],"verbs":["get"]}]),
        ),
    ] {
        let (code, refused) = s
            .json(
                developer(),
                "POST",
                &roles,
                json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":name},"rules":rules}),
            )
            .await;
        assert_eq!(code, 403, "{name}: {refused}");
        let message = refused["message"].as_str().unwrap();
        assert!(
            message.contains("cannot grant permissions it does not hold"),
            "{name}: {message}"
        );
        assert!(
            !message.contains("bootstrap administrator"),
            "{name}: {message}"
        );
    }
    // Binding a role that grants more than the principal holds is escalation.
    let (code, refused) = s
        .json(
            developer(),
            "POST",
            &format!("{RBAC}/namespaces/team-a/rolebindings"),
            json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":"super"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"super"},"subjects":[{"kind":"Group","apiGroup":"rbac.authorization.k8s.io","name":"developers"}]}),
        )
        .await;
    assert_eq!(code, 403, "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("cannot grant permissions it does not hold"),
        "{refused}"
    );
    // The administrator's own write is unaffected, and the stored Role is gone
    // only for the refused ones.
    assert_eq!(
        s.json(s.admin(), "GET", &format!("{roles}/reader"), json!({}))
            .await
            .0,
        200
    );
    for name in ["wildcard", "secrets"] {
        assert_eq!(
            s.json(s.admin(), "GET", &format!("{roles}/{name}"), json!({}))
                .await
                .0,
            404,
            "{name}"
        );
    }
}
