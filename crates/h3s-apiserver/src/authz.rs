//! Authorization: the RBAC snapshot, then node-scoped grants for resource
//! requests that RBAC alone does not allow.
use crate::{key, nodes, object, resources::Target, selectors::Selection, Api, Failure, Result};
use h3s_auth::{Rbac, Request as AuthRequest, ResourceRequest, User};
use h3s_storage::ListSelect;
use serde_json::Value;

impl Api {
    pub(crate) async fn rbac(&self) -> Result<Rbac> {
        let mut r = Rbac::default();
        let mut snapshot = None;
        for kind in [
            "roles",
            "rolebindings",
            "clusterroles",
            "clusterrolebindings",
        ] {
            let mut sel = ListSelect::new(format!("/registry/{kind}/"));
            sel.at_revision = snapshot;
            loop {
                let page = self.store.list(sel.clone()).await?;
                snapshot.get_or_insert(page.revision);
                for obj in page.items {
                    match kind {
                        "roles" => r.roles.push(serde_json::from_slice(&obj.value)?),
                        "rolebindings" => r.role_bindings.push(serde_json::from_slice(&obj.value)?),
                        "clusterroles" => r.cluster_roles.push(serde_json::from_slice(&obj.value)?),
                        _ => r
                            .cluster_role_bindings
                            .push(serde_json::from_slice(&obj.value)?),
                    }
                }
                let Some(next) = page.next_after else {
                    break;
                };
                sel.at_revision = Some(page.revision);
                sel.start_after = Some(next);
            }
        }
        Ok(r)
    }
}

/// Non-resource URLs are readable by whoever RBAC grants `get` on the path.
pub(crate) async fn non_resource(
    api: &Api,
    user: &User,
    path: &str,
    denial: &'static str,
) -> Result<()> {
    if !api
        .rbac()
        .await?
        .allows(user, &AuthRequest::NonResource { verb: "get", path })
    {
        return Err(Failure::new(403, "Forbidden", denial));
    }
    Ok(())
}

/// What an authorized resource request may see: its selection, narrowed to
/// the node's own Pods for node grants, and a relationship guard that keeps
/// node reads of Secrets and ConfigMaps honest for the life of a stream.
pub(crate) struct Grant {
    pub selection: Selection,
    pub read_guard: Option<nodes::ReadGuard>,
}

pub(crate) async fn resource(
    api: &Api,
    user: &User,
    target: &Target,
    verb: &'static str,
    mut selection: Selection,
) -> Result<Grant> {
    let selected_name = selection.exact_name().map(str::to_owned);
    let attrs = ResourceRequest {
        verb,
        group: target.resource.group,
        resource: target.resource.plural,
        subresource: target.subresource,
        namespace: target.namespace.as_deref(),
        name: target.name.as_deref().or_else(|| {
            matches!(verb, "list" | "watch")
                .then(|| selected_name.as_deref())
                .flatten()
        }),
    };
    let rbac_allowed = api
        .rbac()
        .await?
        .allows(user, &AuthRequest::Resource(attrs.clone()));
    let node_allowed = if let Some(node) = user.node_name() {
        let related = nodes::related(api, node, target, attrs.name).await?;
        h3s_auth::node_allows(
            user,
            &attrs,
            selection.exact_field("spec.nodeName"),
            related,
        )
    } else {
        false
    };
    let node_constrained = user.node_name().is_some()
        && matches!(target.resource.kind, "Pod" | "Secret" | "ConfigMap");
    if node_constrained {
        if !node_allowed {
            return Err(Failure::new(
                403,
                "Forbidden",
                format!(
                    "user {} cannot {verb} {}",
                    user.name, target.resource.plural
                ),
            ));
        }
    } else if !rbac_allowed && !node_allowed {
        return Err(Failure::new(
            403,
            "Forbidden",
            format!(
                "user {} cannot {verb} {}",
                user.name, target.resource.plural
            ),
        ));
    }
    let mut read_guard = None;
    if node_allowed {
        let node = user.node_name().expect("node grant requires node identity");
        if target.resource.kind == "Pod" && matches!(verb, "list" | "watch") {
            // Also constrain name-only watches: a recreated Pod assigned to
            // another worker must not enter this node's stream.
            selection = selection.with_field("spec.nodeName", node);
        }
        if matches!(target.resource.kind, "Secret" | "ConfigMap") {
            read_guard = Some(nodes::ReadGuard::new(
                node,
                target,
                attrs.name.expect("relationship name"),
            ));
        }
    }
    Ok(Grant {
        selection,
        read_guard,
    })
}

/// The RBAC API group these objects live in.
const RBAC_GROUP: &str = "rbac.authorization.k8s.io";

/// The permissions a rule set grants, one entry per (verb, group, resource,
/// resource name). `*` stays literal: a principal holds only what its own
/// rules match, so a rule granting `*` is a grant the principal must already
/// have to pass it on.
fn granted(rules: &Value) -> Vec<(String, String, String, Option<String>)> {
    let list = |value: &Value| -> Option<Vec<String>> {
        value.as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_owned)
                .collect()
        })
    };
    let mut out = Vec::new();
    for rule in rules.as_array().into_iter().flatten() {
        let groups = list(&rule["apiGroups"]).unwrap_or_else(|| vec![String::new()]);
        let resources = list(&rule["resources"]).unwrap_or_default();
        let verbs = list(&rule["verbs"]).unwrap_or_default();
        let names: Vec<Option<String>> = match list(&rule["resourceNames"]) {
            Some(names) if !names.is_empty() => names.into_iter().map(Some).collect(),
            _ => vec![None],
        };
        for verb in &verbs {
            for group in &groups {
                for resource in &resources {
                    for name in &names {
                        out.push((verb.clone(), group.clone(), resource.clone(), name.clone()));
                    }
                }
            }
        }
    }
    out
}
fn request<'a>(
    verb: &'a str,
    group: &'a str,
    resource: &'a str,
    subresource: Option<&'a str>,
    namespace: Option<&'a str>,
    name: Option<&'a str>,
) -> ResourceRequest<'a> {
    ResourceRequest {
        verb,
        group,
        resource,
        subresource,
        namespace,
        name,
    }
}
fn escalation_denied(what: &str) -> Failure {
    Failure::new(
        403,
        "Forbidden",
        format!("user cannot grant permissions it does not hold: {what}"),
    )
}
fn split_resource(resource: &str) -> (&str, Option<&str>) {
    match resource.split_once('/') {
        Some((resource, subresource)) => (resource, Some(subresource)),
        None => (resource, None),
    }
}
/// Kubernetes escalation prevention: a principal may create or change an RBAC
/// object only within what it already holds, unless it may `escalate` that
/// object kind. A binding asks the same question about the role it references,
/// unless the principal may `bind` that role. A bootstrap administrator is
/// unaffected, so cluster bootstrap still works.
pub(crate) async fn escalation(
    api: &Api,
    user: &User,
    target: &Target,
    verb: &str,
    value: &Value,
) -> Result<()> {
    if user.is_superuser() || !matches!(verb, "create" | "update" | "patch") {
        return Ok(());
    }
    let rbac = api.rbac().await?;
    let namespace = target.namespace.as_deref().filter(|n| !n.is_empty());
    let plural = target.resource.plural;
    let within = |verb: &str,
                  group: &str,
                  resource: &str,
                  namespace: Option<&str>,
                  name: Option<&str>| {
        let (resource, subresource) = split_resource(resource);
        rbac.allows(
            user,
            &AuthRequest::Resource(request(verb, group, resource, subresource, namespace, name)),
        )
    };
    if matches!(plural, "roles" | "clusterroles") {
        if within("escalate", RBAC_GROUP, plural, namespace, None) {
            return Ok(());
        }
        for (verb, group, resource, name) in granted(&value["rules"]) {
            if !within(&verb, &group, &resource, namespace, name.as_deref()) {
                return Err(escalation_denied(&format!("{verb} on {group}/{resource}")));
            }
        }
        return Ok(());
    }
    if !matches!(plural, "rolebindings" | "clusterrolebindings") {
        return Ok(());
    }
    let role_ref = &value["roleRef"];
    let referenced = match role_ref["kind"].as_str() {
        Some("ClusterRole") => "clusterroles",
        Some("Role") => "roles",
        _ => return Err(Failure::new(403, "Forbidden", "invalid roleRef kind")),
    };
    let name = role_ref["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| Failure::new(403, "Forbidden", "roleRef name is required"))?;
    if within("bind", RBAC_GROUP, referenced, namespace, Some(name)) {
        return Ok(());
    }
    let path = match referenced {
        "roles" => format!("/registry/roles/{}/{name}", namespace.unwrap_or("")),
        _ => format!("/registry/clusterroles/{name}"),
    };
    let role = api
        .store
        .get(&key(path)?)
        .await?
        .map(object)
        .transpose()?
        .ok_or_else(|| Failure::new(403, "Forbidden", "roleRef names a missing role"))?;
    let scope = if referenced == "roles" {
        namespace
    } else {
        None
    };
    for (verb, group, resource, resource_name) in granted(&role["rules"]) {
        if !within(&verb, &group, &resource, scope, resource_name.as_deref()) {
            return Err(escalation_denied(&format!("{verb} on {group}/{resource}")));
        }
    }
    Ok(())
}
