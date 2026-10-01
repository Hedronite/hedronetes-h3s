//! ServiceLB: assigns the local node's address to LoadBalancer Services, the
//! way k3s does it in-tree. No cloud provider, no external binary.
use crate::Error;
use k8s_openapi::api::core::v1::{Node, Service};
use kube::{
    api::{Api, ListParams, Patch, PatchParams},
    Client, Resource, ResourceExt,
};
use serde_json::json;
use std::time::Duration;

pub const SERVICELB_CONTROLLER_ID: &str = "system:h3s:servicelb";

/// True when the Service awaits an address: type LoadBalancer, no external
/// load-balancer class delegated elsewhere, and no published ingress yet.
pub fn needs_address(service: &Service) -> bool {
    let Some(spec) = service.spec.as_ref() else {
        return false;
    };
    spec.type_.as_deref() == Some("LoadBalancer")
        && spec.load_balancer_class.is_none()
        && service
            .status
            .as_ref()
            .and_then(|status| status.load_balancer.as_ref())
            .and_then(|load_balancer| load_balancer.ingress.as_ref())
            .is_none_or(|ingress| ingress.is_empty())
}

fn node_address(node: &Node) -> Option<String> {
    node.status
        .as_ref()?
        .addresses
        .as_ref()?
        .iter()
        .find_map(|address| (address.type_ == "InternalIP").then(|| address.address.clone()))
}

pub async fn servicelb_once(client: Client, node_name: Option<String>) -> Result<(), Error> {
    let nodes = Api::<Node>::all(client.clone());
    let mut address: Option<String> = None;
    for node in nodes.list(&ListParams::default()).await?.items {
        if node_name
            .as_deref()
            .is_some_and(|name| node.name_any() != name)
        {
            continue;
        }
        if let Some(ip) = node_address(&node) {
            address = Some(ip);
            break;
        }
    }
    let Some(address) = address else {
        // A cluster whose nodes carry no InternalIP has no address to hand
        // out yet; a later tick will.
        return Ok(());
    };
    let services = Api::<Service>::all(client);
    for service in services.list(&ListParams::default()).await?.items {
        if service.meta().deletion_timestamp.is_some() || !needs_address(&service) {
            continue;
        }
        let patch = Patch::Merge(json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"resourceVersion": service.resource_version()},
            "spec": {"type": "LoadBalancer"},
            "status": {"loadBalancer": {"ingress": [{"ip": address}]}}
        }));
        let namespace = service.namespace().unwrap_or_default();
        Api::<Service>::namespaced(services.clone().into_client(), &namespace)
            .patch_status(&service.name_any(), &PatchParams::default(), &patch)
            .await?;
    }
    Ok(())
}

pub async fn run_servicelb(client: Client, node_name: Option<String>) -> Result<(), Error> {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        match tokio::time::timeout(
            Duration::from_secs(60),
            servicelb_once(client.clone(), node_name.clone()),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("servicelb controller: {error}"),
            Err(_) => eprintln!("servicelb controller: reconciliation timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json};

    #[test]
    fn only_undelegated_load_balancers_wait_for_an_address() {
        let waiting: Service =
            from_value(json!({"metadata": {"name": "lb"}, "spec": {"type": "LoadBalancer"}}))
                .unwrap();
        assert!(needs_address(&waiting));
        let assigned: Service = from_value(json!({
            "metadata": {"name": "lb"},
            "spec": {"type": "LoadBalancer"},
            "status": {"loadBalancer": {"ingress": [{"ip": "192.0.2.10"}]}}
        }))
        .unwrap();
        assert!(!needs_address(&assigned));
        let delegated: Service = from_value(json!({
            "metadata": {"name": "lb"},
            "spec": {"type": "LoadBalancer", "loadBalancerClass": "example.com/custom"}
        }))
        .unwrap();
        assert!(!needs_address(&delegated));
        let cluster_ip: Service =
            from_value(json!({"metadata": {"name": "web"}, "spec": {"type": "ClusterIP"}}))
                .unwrap();
        assert!(!needs_address(&cluster_ip));
    }
}
