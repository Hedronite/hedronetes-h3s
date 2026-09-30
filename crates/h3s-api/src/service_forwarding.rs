//! Typed Service forwarding policy shared by API admission and the native proxy.
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceForwarding {
    /// ClusterIP with a virtual IP; the native proxy programs nftables for this.
    ClusterIp,
    /// NodePort: the ClusterIP frontend plus a node frontend on the local node's
    /// InternalIP and the allocated nodePort.
    NodePort,
    /// ExternalName is admitted but has no dataplane rules.
    ExternalName,
    /// Headless services (`clusterIP: None`) are skipped by the proxy.
    Headless,
    /// LoadBalancer and other types the single-server API does not implement.
    UnsupportedType,
    /// Session, traffic, or external-address policy the native proxy does not implement.
    UnsupportedPolicy,
}

impl ServiceForwarding {
    /// Classify a Service spec after API defaults are applied.
    pub fn classify(spec: &Value) -> Self {
        let kind = spec["type"].as_str().unwrap_or("ClusterIP");
        if kind == "ExternalName" {
            return Self::ExternalName;
        }
        if spec["clusterIP"].as_str() == Some("None") {
            // A headless Service has no node frontend to program.
            return if kind == "ClusterIP" {
                Self::Headless
            } else {
                Self::UnsupportedType
            };
        }
        if !matches!(kind, "ClusterIP" | "NodePort") {
            return Self::UnsupportedType;
        }
        if spec["sessionAffinity"]
            .as_str()
            .is_some_and(|s| s != "None")
            || spec["externalIPs"]
                .as_array()
                .is_some_and(|values| !values.is_empty())
            || !spec["trafficDistribution"].is_null()
            || spec["externalTrafficPolicy"]
                .as_str()
                .is_some_and(|p| p != "Cluster")
            || spec["healthCheckNodePort"]
                .as_i64()
                .is_some_and(|p| p != 0)
        {
            return Self::UnsupportedPolicy;
        }
        if kind == "NodePort" {
            return Self::NodePort;
        }
        Self::ClusterIp
    }

    pub fn proxied(self) -> bool {
        matches!(self, Self::ClusterIp | Self::NodePort)
    }

    pub fn skip_in_plan(self) -> bool {
        !self.proxied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cluster_ip_and_node_port_are_proxied_and_unsupported_variants_skip() {
        assert_eq!(
            ServiceForwarding::classify(&json!({"ports":[{"port":80}]})),
            ServiceForwarding::ClusterIp
        );
        assert!(ServiceForwarding::ExternalName.skip_in_plan());
        assert!(ServiceForwarding::Headless.skip_in_plan());
        assert!(ServiceForwarding::UnsupportedType.skip_in_plan());
        assert!(ServiceForwarding::UnsupportedPolicy.skip_in_plan());
        assert_eq!(
            ServiceForwarding::classify(
                &json!({"type":"ExternalName","externalName":"example.invalid"})
            ),
            ServiceForwarding::ExternalName
        );
        assert_eq!(
            ServiceForwarding::classify(
                &json!({"type":"ClusterIP","clusterIP":"None","ports":[{"port":80}]})
            ),
            ServiceForwarding::Headless
        );
        let node_port = json!({"type":"NodePort","clusterIP":"10.43.0.10","externalTrafficPolicy":"Cluster","ports":[{"port":80,"nodePort":30080}]});
        assert_eq!(
            ServiceForwarding::classify(&node_port),
            ServiceForwarding::NodePort
        );
        assert!(ServiceForwarding::NodePort.proxied());
        for rejected in [
            json!({"type":"LoadBalancer","clusterIP":"10.43.0.10","ports":[{"port":80}]}),
            json!({"type":"NodePort","clusterIP":"None","ports":[{"port":80}]}),
        ] {
            assert_eq!(
                ServiceForwarding::classify(&rejected),
                ServiceForwarding::UnsupportedType,
                "{rejected}"
            );
        }
        assert_eq!(
            ServiceForwarding::classify(
                &json!({"sessionAffinity":"ClientIP","ports":[{"port":80}]})
            ),
            ServiceForwarding::UnsupportedPolicy
        );
        for rejected in [
            json!({"type":"NodePort","clusterIP":"10.43.0.10","externalTrafficPolicy":"Local","ports":[{"port":80,"nodePort":30080}]}),
            json!({"type":"NodePort","clusterIP":"10.43.0.10","externalTrafficPolicy":"Cluster","healthCheckNodePort":30081,"ports":[{"port":80,"nodePort":30080}]}),
        ] {
            assert_eq!(
                ServiceForwarding::classify(&rejected),
                ServiceForwarding::UnsupportedPolicy,
                "{rejected}"
            );
        }
        assert_eq!(
            ServiceForwarding::classify(
                &json!({"type":"NodePort","clusterIP":"10.43.0.10","externalTrafficPolicy":"Cluster","healthCheckNodePort":0,"ports":[{"port":80,"nodePort":30080}]})
            ),
            ServiceForwarding::NodePort
        );
    }
}
