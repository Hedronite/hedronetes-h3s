//! API-owned relationship reads and node admission for M1. No node, controller,
//! or authorizer opens the registry directly. Decisions use persisted Pod state.
use crate::{key, object, resources::Target, Api, Failure, Result};
use h3s_auth::{node_label_allowed, User};
use h3s_storage::ListSelect;
use serde_json::Value;
use std::collections::BTreeSet;

fn deny(message: &str) -> Failure {
    Failure::new(403, "Forbidden", message)
}
fn items(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}
fn equivalent(a: &Value, b: &Value) -> bool {
    let empty = |v: &Value| {
        v.is_null() || v.as_array().is_some_and(Vec::is_empty) || v.as_str() == Some("")
    };
    a == b || (empty(a) && empty(b))
}
fn assigned(pod: &Value, node: &str) -> bool {
    pod["spec"]["nodeName"].as_str() == Some(node)
}

/// Resolve only typed references. Merely mentioning a name in arbitrary JSON
/// (annotations, commands, volume names, or literal env values) grants nothing.
fn references(pod: &Value, resource: &str, name: &str) -> bool {
    let spec = &pod["spec"];
    let secret = resource == "secrets";
    if secret && items(&spec["imagePullSecrets"]).any(|v| v["name"] == name) {
        return true;
    }
    for volume in items(&spec["volumes"]) {
        let source = if secret { "secret" } else { "configMap" };
        let field = if secret { "secretName" } else { "name" };
        if volume[source][field] == name {
            return true;
        }
        if items(&volume["projected"]["sources"]).any(|v| v[source]["name"] == name) {
            return true;
        }
        // CSI node-publish secrets are typed, namespaced Pod references.
        if secret && volume["csi"]["nodePublishSecretRef"]["name"] == name {
            return true;
        }
    }
    for field in ["containers", "initContainers", "ephemeralContainers"] {
        for container in items(&spec[field]) {
            let key_ref = if secret {
                "secretKeyRef"
            } else {
                "configMapKeyRef"
            };
            let source = if secret { "secretRef" } else { "configMapRef" };
            if items(&container["env"]).any(|v| v["valueFrom"][key_ref]["name"] == name)
                || items(&container["envFrom"]).any(|v| v[source]["name"] == name)
            {
                return true;
            }
        }
    }
    false
}

pub async fn related(api: &Api, node: &str, target: &Target, name: Option<&str>) -> Result<bool> {
    let (Some(namespace), Some(name)) = (target.namespace.as_deref(), name) else {
        return Ok(false);
    };
    if !target.resource.group.is_empty() {
        return Ok(false);
    }
    if target.resource.plural == "pods" {
        return api
            .store
            .get(&key(format!("/registry/pods/{namespace}/{name}"))?)
            .await?
            .map(|v| object(v).map(|pod| assigned(&pod, node)))
            .transpose()
            .map(|v| v.unwrap_or(false));
    }
    if !matches!(target.resource.plural, "secrets" | "configmaps") {
        return Ok(false);
    }
    let mut select = ListSelect::new(format!("/registry/pods/{namespace}/"));
    loop {
        let page = api.store.list(select.clone()).await?;
        select.at_revision = Some(page.revision);
        for stored in page.items {
            let pod = object(stored)?;
            if assigned(&pod, node) && references(&pod, target.resource.plural, name) {
                return Ok(true);
            }
        }
        let Some(cursor) = page.next_after else {
            return Ok(false);
        };
        select.start_after = Some(cursor);
    }
}

/// Relationship-based Secret/ConfigMap streams recheck before each emission.
/// A revoked relationship closes the stream; no subsequent body is delivered.
#[derive(Clone)]
pub struct ReadGuard {
    node: String,
    target: Target,
    name: String,
}
impl ReadGuard {
    pub fn new(node: &str, target: &Target, name: &str) -> Self {
        Self {
            node: node.into(),
            target: target.clone(),
            name: name.into(),
        }
    }
    pub async fn check(&self, api: &Api) -> Result<()> {
        if related(api, &self.node, &self.target, Some(&self.name)).await? {
            Ok(())
        } else {
            Err(deny("node no longer has a Pod relationship to this object"))
        }
    }
}

/// Always run for node identities, including when RBAC granted the write.
/// Call under the single-server admission lock, after applying status strategy.
pub fn admit(
    user: &User,
    target: &Target,
    verb: &str,
    value: &Value,
    old: Option<&Value>,
) -> Result<()> {
    let Some(node) = user.node_name() else {
        return Ok(());
    };
    match (target.resource.group, target.resource.kind) {
        ("", "Node") => {
            if value["metadata"]["name"] != node || verb == "delete" {
                return Err(deny(
                    "a node may modify only its own Node and may not delete it",
                ));
            }
            let empty = Value::Null;
            let old = old.unwrap_or(&empty);
            for field in ["podCIDR", "podCIDRs", "configSource"] {
                if !equivalent(&value["spec"][field], &old["spec"][field]) {
                    return Err(deny(
                        "node network allocation and configSource are administrator-managed",
                    ));
                }
            }
            if verb != "create" && !equivalent(&value["spec"]["taints"], &old["spec"]["taints"]) {
                return Err(deny("nodes may not modify their taints"));
            }
            for field in [
                "ownerReferences",
                "finalizers",
                "deletionTimestamp",
                "deletionGracePeriodSeconds",
            ] {
                if !equivalent(&value["metadata"][field], &old["metadata"][field]) {
                    return Err(deny(
                        "nodes may not alter Node ownership or deletion metadata",
                    ));
                }
            }
            let labels: BTreeSet<_> = [value, old]
                .into_iter()
                .filter_map(|v| v["metadata"]["labels"].as_object())
                .flat_map(|m| m.keys())
                .collect();
            for label in labels {
                if value["metadata"]["labels"][label] != old["metadata"]["labels"][label]
                    && !node_label_allowed(label)
                {
                    return Err(deny(
                        "nodes may not modify administrative Kubernetes labels",
                    ));
                }
            }
        }
        ("", "Pod") => {
            let Some(old) = old else {
                return Err(deny("node mirror Pod creation is not implemented"));
            };
            if !assigned(old, node) {
                return Err(deny("node may modify only Pods assigned to itself"));
            }
            if verb == "delete" {
                return Ok(());
            }
            if target.subresource != Some("status") {
                return Err(deny("node Pod updates must use the status subresource"));
            }
            if value["status"]["resourceClaimStatuses"] != old["status"]["resourceClaimStatuses"] {
                return Err(deny("node may not alter Pod resource claim allocation"));
            }
        }
        ("coordination.k8s.io", "Lease")
            if target.namespace.as_deref() != Some("kube-node-lease")
                || value["metadata"]["name"] != node =>
        {
            return Err(deny(
                "node may modify only its named lease in kube-node-lease",
            ));
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::RESOURCES;
    use h3s_auth::{node_allows, ResourceRequest, User};
    use serde_json::json;

    fn node_user(name: &str) -> User {
        User {
            name: format!("system:node:{name}"),
            groups: vec!["system:nodes".into(), "system:authenticated".into()],
        }
    }
    fn target(
        kind: &str,
        namespace: Option<&str>,
        name: &str,
        subresource: Option<&'static str>,
    ) -> Target {
        Target {
            resource: *RESOURCES
                .iter()
                .find(|r| r.kind == kind)
                .expect("known kind"),
            namespace: namespace.map(str::to_owned),
            name: Some(name.into()),
            subresource,
        }
    }
    fn pod(name: &str, assigned: &str) -> Value {
        json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name,"namespace":"team-a","uid":"3e17c2c0-49e2-4f2b-a917-89da3f986647"},"spec":{"nodeName":assigned}})
    }
    fn node_object(name: &str) -> Value {
        json!({"apiVersion":"v1","kind":"Node","metadata":{"name":name},"spec":{}})
    }
    fn request(
        resource: &'static str,
        name: &'static str,
        verb: &'static str,
    ) -> ResourceRequest<'static> {
        ResourceRequest {
            verb,
            group: "",
            resource,
            subresource: None,
            namespace: Some("team-a"),
            name: Some(name),
        }
    }

    /// KP-44: the node authorizer and node admission decide the same request.
    /// Where the authorizer grants a Pod delete, admission still holds the
    /// persisted assignment; where the authorizer widens a Node update to any
    /// name, admission is what keeps a node on its own object.
    #[test]
    fn node_grants_and_node_admission_cannot_diverge() {
        let user = node_user("a");
        // A Pod delete is a node grant only through a verified relationship.
        assert!(node_allows(
            &user,
            &request("pods", "owned", "delete"),
            None,
            true
        ));
        assert!(!node_allows(
            &user,
            &request("pods", "owned", "delete"),
            None,
            false
        ));
        let pods = target("Pod", Some("team-a"), "owned", None);
        let owned = pod("owned", "a");
        let foreign = pod("owned", "b");
        assert!(admit(&user, &pods, "delete", &owned, Some(&owned)).is_ok());
        assert!(admit(&user, &pods, "delete", &foreign, Some(&foreign)).is_err());
        // Deleting its own Node object is never granted, and admission refuses
        // it even when RBAC does.
        assert!(!node_allows(
            &user,
            &request("nodes", "a", "delete"),
            None,
            true
        ));
        let nodes = target("Node", None, "a", None);
        let own = node_object("a");
        assert!(admit(&user, &nodes, "delete", &own, Some(&own)).is_err());
        // A Node update is granted without naming the object, so admission is
        // the constraint that has to hold: a node may not rename or retarget
        // any Node object, including by creating a second one.
        assert!(node_allows(
            &user,
            &request("nodes", "b", "update"),
            None,
            false
        ));
        let other = target("Node", None, "b", None);
        let renamed = node_object("b");
        assert!(admit(&user, &other, "update", &renamed, Some(&own)).is_err());
        assert!(admit(&user, &other, "create", &renamed, None).is_err());
        // The node's own object stays writable for the fields a node owns.
        assert!(admit(&user, &nodes, "update", &own, Some(&own)).is_ok());
    }
}
