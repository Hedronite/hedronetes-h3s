//! Local-path claim binding. The single-server API is also the provisioner: a
//! claim naming a StorageClass whose provisioner is `h3s.io/local-path` gets a
//! node-local PersistentVolume, exactly as the Service write owns its ClusterIP.
//! The volume carries no node affinity, so the claim can run on any node and the
//! kubelet creates the directory on whichever node mounts it.
use crate::{key, object, stored, Failure, Result};
use h3s_storage::{Error, Storage};
use serde_json::{json, Value};
use std::sync::Arc;

/// The one provisioner this API implements.
pub const PROVISIONER: &str = "h3s.io/local-path";
/// The directory the provisioner hands out; the kubelet creates the leaf.
pub const BASE: &str = "/var/lib/hedronetes/local-path";

fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(422, "Invalid", message)
}
/// The directory a claim owns, on whichever node mounts it.
pub fn path(namespace: &str, claim: &str) -> String {
    format!("{BASE}/{namespace}_{claim}")
}
fn bytes(value: &Value) -> Option<i64> {
    value
        .as_str()
        .and_then(h3s_api::quantity::Quantity::parse)
        .and_then(|q| q.as_bytes())
}
async fn get(store: &Arc<dyn Storage>, path: String) -> Result<Option<Value>> {
    store.get(&key(path)?).await?.map(object).transpose()
}
fn bound(value: &mut Value, volume: &Value, name: &str) {
    let requested = value["spec"]["resources"]["requests"]["storage"].clone();
    value["spec"]["volumeName"] = json!(name);
    value["status"] = json!({
        "phase": "Bound",
        "accessModes": volume["spec"]["accessModes"],
        "capacity": {"storage": volume["spec"]["capacity"]["storage"].as_str().map(str::to_owned).unwrap_or_else(|| requested.as_str().unwrap_or("").to_owned())},
    });
}

/// Bind the claim this write is creating, or provision one for it. A claim
/// whose class has another provisioner stays Pending, as in Kubernetes.
pub(crate) async fn bind(store: &Arc<dyn Storage>, value: &mut Value) -> Result<()> {
    let namespace = value["metadata"]["namespace"]
        .as_str()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| invalid("a claim requires a namespace"))?
        .to_owned();
    let name = value["metadata"]["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| invalid("a claim requires a name"))?
        .to_owned();
    let uid = value["metadata"]["uid"].as_str().unwrap_or("").to_owned();
    let requested = value["spec"]["volumeName"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if !requested.is_empty() {
        // A pre-bound claim only accepts the volume that names it back.
        let volume = get(store, format!("/registry/persistentvolumes/{requested}"))
            .await?
            .ok_or_else(|| invalid("the requested PersistentVolume does not exist"))?;
        if !volume["spec"]["claimRef"].is_null()
            && (volume["spec"]["claimRef"]["uid"].as_str() != Some(uid.as_str())
                || volume["spec"]["claimRef"]["namespace"].as_str() != Some(namespace.as_str()))
        {
            return Err(invalid("the PersistentVolume is claimed by another object"));
        }
        bound(value, &volume, &requested);
        return Ok(());
    }
    let Some(class) = value["spec"]["storageClassName"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
    else {
        return Ok(());
    };
    let class = get(store, format!("/registry/storageclasses/{class}"))
        .await?
        .ok_or_else(|| invalid("the named StorageClass does not exist"))?;
    if class["provisioner"].as_str() != Some(PROVISIONER) {
        return Ok(());
    }
    let storage = bytes(&value["spec"]["resources"]["requests"]["storage"])
        .ok_or_else(|| invalid("resources.requests.storage must be a resource quantity"))?;
    let volume_name = format!("pvc-{uid}");
    let local = path(&namespace, &name);
    let volume = json!({
        "apiVersion": "v1",
        "kind": "PersistentVolume",
        "metadata": {
            "name": volume_name,
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": crate::now(),
            "labels": {"app.kubernetes.io/managed-by": "h3s-local-path"},
        },
        "spec": {
            "capacity": {"storage": format!("{storage}")},
            "accessModes": value["spec"]["accessModes"],
            "persistentVolumeReclaimPolicy": "Delete",
            "storageClassName": class["metadata"]["name"],
            "volumeMode": value["spec"]["volumeMode"],
            "local": {"path": local},
            "claimRef": {
                "apiVersion": "v1",
                "kind": "PersistentVolumeClaim",
                "name": name,
                "namespace": namespace,
                "uid": uid,
            },
        },
    });
    match store
        .create(stored(
            key(format!("/registry/persistentvolumes/{volume_name}"))?,
            &volume,
        )?)
        .await
    {
        Ok(_) | Err(Error::AlreadyExists(_)) => {}
        Err(error) => return Err(error.into()),
    }
    bound(value, &volume, &volume_name);
    Ok(())
}
