//! Pod ServiceAccount admission: the account must exist and permit what the
//! Pod references. No token is projected: until TokenRequest and bearer
//! authentication exist the API would only refuse it, and the runtime profile
//! keeps `automountServiceAccountToken` false to match.
use crate::{key, object, resources::RESOURCES, Failure, Result};
use h3s_storage::Storage;
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc};

fn denied(message: &str) -> Failure {
    Failure::new(403, "Forbidden", message)
}
fn items(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

pub async fn admit(store: &Arc<dyn Storage>, namespace: &str, pod: &mut Value) -> Result<()> {
    if pod["metadata"]["annotations"]
        .get("kubernetes.io/config.mirror")
        .is_some()
    {
        return Err(denied("mirror Pod admission is not implemented"));
    }
    let spec = &mut pod["spec"];
    let name = spec["serviceAccountName"].as_str().unwrap_or("default");
    let resource = RESOURCES
        .iter()
        .find(|r| r.kind == "ServiceAccount")
        .expect("ServiceAccount resource");
    if !resource.valid_name(name) {
        return Err(Failure::new(422, "Invalid", "invalid serviceAccountName"));
    }
    let account = store
        .get(&key(format!(
            "/registry/serviceaccounts/{namespace}/{name}"
        ))?)
        .await?
        .ok_or_else(|| denied("Pod service account does not exist in its namespace"))?;
    let account = object(account)?;
    if !account["metadata"]["deletionTimestamp"].is_null() {
        return Err(denied("Pod service account is terminating"));
    }
    if items(&spec["imagePullSecrets"]).next().is_none() && !account["imagePullSecrets"].is_null() {
        spec["imagePullSecrets"] = account["imagePullSecrets"].clone();
    }
    if spec["automountServiceAccountToken"] == true {
        project_token(spec);
    }
    let enforce = account["metadata"]["annotations"]["kubernetes.io/enforce-mountable-secrets"]
        .as_str()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("t"));
    if enforce {
        validate_secret_references(spec, &account)?;
    }
    Ok(())
}
/// The upstream projection a Pod with `automountServiceAccountToken` carries:
/// the bound token, the cluster CA and the namespace, mounted read-only at the
/// documented path in every container. The kubelet materialises exactly this
/// shape, and admission names it in the stored Pod so what runs is visible.
fn project_token(spec: &mut Value) {
    const MOUNT_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
    let existing = spec["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| !v["projected"].is_null())
        .filter_map(|v| v["name"].as_str().map(str::to_owned))
        .next();
    let name = existing.unwrap_or_else(|| {
        format!(
            "kube-api-access-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..5]
        )
    });
    let Some(spec) = spec.as_object_mut() else {
        return;
    };
    if spec
        .get("volumes")
        .and_then(Value::as_array)
        .is_none_or(|v| !v.iter().any(|v| v["name"].as_str() == Some(name.as_str())))
    {
        spec.entry("volumes")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("volumes array")
            .push(json!({"name":name,"projected":{"defaultMode":420,"sources":[
                {"serviceAccountToken":{"expirationSeconds":3607,"path":"token"}},
                {"configMap":{"name":"kube-root-ca.crt","items":[{"key":"ca.crt","path":"ca.crt"}]}},
                {"downwardAPI":{"items":[{"path":"namespace","fieldRef":{"apiVersion":"v1","fieldPath":"metadata.namespace"}}]}}
            ]}}));
    }
    for field in ["containers", "initContainers"] {
        for container in spec
            .get_mut(field)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let mounts = container
                .as_object_mut()
                .expect("container object")
                .entry("volumeMounts")
                .or_insert_with(|| json!([]));
            let mounts = mounts.as_array_mut().expect("volumeMounts array");
            if !mounts.iter().any(|m| m["mountPath"] == MOUNT_PATH) {
                mounts.push(json!({"name":name,"readOnly":true,"mountPath":MOUNT_PATH}));
            }
        }
    }
}
fn validate_secret_references(spec: &Value, account: &Value) -> Result<()> {
    let allowed: BTreeSet<_> = items(&account["secrets"])
        .filter_map(|v| v["name"].as_str())
        .collect();
    let pulls: BTreeSet<_> = items(&account["imagePullSecrets"])
        .filter_map(|v| v["name"].as_str())
        .collect();
    let mut refs = Vec::new();
    for volume in items(&spec["volumes"]) {
        if let Some(name) = volume["secret"]["secretName"].as_str() {
            refs.push(name);
        }
        for source in items(&volume["projected"]["sources"]) {
            if let Some(name) = source["secret"]["name"].as_str() {
                refs.push(name);
            }
        }
    }
    for field in ["containers", "initContainers", "ephemeralContainers"] {
        for container in items(&spec[field]) {
            for env in items(&container["env"]) {
                if let Some(name) = env["valueFrom"]["secretKeyRef"]["name"].as_str() {
                    refs.push(name);
                }
            }
            for env in items(&container["envFrom"]) {
                if let Some(name) = env["secretRef"]["name"].as_str() {
                    refs.push(name);
                }
            }
        }
    }
    if refs.iter().any(|name| !allowed.contains(name)) {
        return Err(denied(
            "Pod references a secret not allowed by its service account",
        ));
    }
    if items(&spec["imagePullSecrets"])
        .any(|v| v["name"].as_str().is_none_or(|n| !pulls.contains(n)))
    {
        return Err(denied(
            "Pod references an image pull secret not allowed by its service account",
        ));
    }
    Ok(())
}
