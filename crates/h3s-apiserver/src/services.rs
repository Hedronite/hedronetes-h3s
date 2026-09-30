//! Single-server ClusterIP and NodePort allocation. The Service write is the
//! allocation commit. Callers hold Api::service_writes through allocation and
//! the registry mutation.
use super::{object, Failure, Result};
use h3s_storage::{ListSelect, Storage};
use serde_json::{json, Value};
use std::{collections::BTreeSet, net::Ipv4Addr, ops::RangeInclusive, sync::Arc};

/// The published node port range, as in Kubernetes.
const NODE_PORTS: RangeInclusive<u16> = 30000..=32767;

fn invalid(message: &str) -> Failure {
    Failure::new(422, "Invalid", message)
}
/// A Service port's node port; an unset or zero value is not an allocation.
fn node_port(port: &Value) -> Option<u16> {
    port["nodePort"]
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .filter(|n| *n != 0)
}
/// The declared name of a Service port, empty when the single-port form omits it.
fn port_name(port: &Value) -> &str {
    port["name"].as_str().unwrap_or("")
}
/// A Service port is the same across an update when its name matches, or, for
/// the unnamed single-port form, when port and protocol match.
fn same_port(before: &Value, after: &Value) -> bool {
    if !port_name(before).is_empty() || !port_name(after).is_empty() {
        return port_name(before) == port_name(after);
    }
    before["port"] == after["port"] && before["protocol"] == after["protocol"]
}
/// Service IPs and node ports already published by the other Services.
async fn published(
    store: &Arc<dyn Storage>,
    uid: &str,
) -> Result<(BTreeSet<Ipv4Addr>, BTreeSet<u16>)> {
    let mut ips = BTreeSet::new();
    let mut ports = BTreeSet::new();
    let mut selection = ListSelect::new("/registry/services/");
    loop {
        let page = store.list(selection.clone()).await?;
        selection.at_revision = Some(page.revision);
        for stored in page.items {
            let current = object(stored)?;
            if current["metadata"]["uid"].as_str() == Some(uid) {
                continue;
            }
            if let Some(ip) = current["spec"]["clusterIP"]
                .as_str()
                .and_then(|s| s.parse::<Ipv4Addr>().ok())
            {
                ips.insert(ip);
            }
            for port in current["spec"]["ports"].as_array().into_iter().flatten() {
                if let Some(node_port) = node_port(port) {
                    ports.insert(node_port);
                }
            }
        }
        let Some(cursor) = page.next_after else {
            return Ok((ips, ports));
        };
        selection.start_after = Some(cursor);
    }
}
/// Keep the node ports this Service already published and allocate every one
/// this request leaves unset. `previous` is absent only on create.
fn node_ports(spec: &mut Value, previous: Option<&Value>, used: &mut BTreeSet<u16>) -> Result<()> {
    let ports = spec
        .get_mut("ports")
        .and_then(Value::as_array_mut)
        .expect("validated Service ports");
    let before = |port: &Value| -> Option<u16> {
        previous
            .and_then(|spec| spec["ports"].as_array())
            .and_then(|ports| ports.iter().find(|p| same_port(p, port)))
            .and_then(node_port)
    };
    for port in ports.iter_mut() {
        let selected = match node_port(port) {
            Some(stated) => {
                if before(port).is_some_and(|published| published != stated) {
                    return Err(invalid("nodePort is immutable"));
                }
                stated
            }
            None => match before(port) {
                Some(published) => published,
                None => (*NODE_PORTS.start()..=*NODE_PORTS.end())
                    .find(|candidate| !used.contains(candidate))
                    .ok_or_else(|| {
                        Failure::new(
                            503,
                            "ServiceUnavailable",
                            "Service node port range exhausted",
                        )
                    })?,
            },
        };
        if !used.insert(selected) {
            return Err(invalid("nodePort already allocated"));
        }
        port["nodePort"] = json!(selected);
    }
    Ok(())
}

pub(crate) async fn assign(
    store: &Arc<dyn Storage>,
    value: &mut Value,
    old: Option<&Value>,
) -> Result<()> {
    let uid = value["metadata"]["uid"].as_str().unwrap_or("").to_owned();
    let spec = &mut value["spec"];
    if let Some(old) = old {
        if old["spec"]["type"] != spec["type"] {
            return Err(invalid("Service type transitions are not yet supported"));
        }
        for field in ["clusterIP", "clusterIPs", "ipFamilies", "ipFamilyPolicy"] {
            if spec.get(field).is_none_or(Value::is_null) {
                spec[field] = old["spec"][field].clone();
            }
            if spec[field] != old["spec"][field] {
                return Err(invalid("Service IP allocation is immutable"));
            }
        }
        if spec["type"] == "NodePort" {
            let (_, mut used) = published(store, &uid).await?;
            node_ports(spec, Some(&old["spec"]), &mut used)?;
        }
        return Ok(());
    }
    if spec["type"] == "ExternalName" {
        if spec["clusterIP"].as_str().is_some_and(|s| !s.is_empty())
            || spec["clusterIPs"].as_array().is_some_and(|v| !v.is_empty())
        {
            return Err(invalid("ExternalName cannot allocate a ClusterIP"));
        }
        return Ok(());
    }
    if spec["ipFamilyPolicy"]
        .as_str()
        .is_some_and(|v| v != "SingleStack")
        || spec["ipFamilies"]
            .as_array()
            .is_some_and(|v| v != &vec![json!("IPv4")])
    {
        return Err(invalid(
            "the M1 Service allocator supports IPv4 SingleStack",
        ));
    }
    let (used, mut node_ports_used) = published(store, &uid).await?;
    let requested = spec["clusterIP"].as_str().unwrap_or("");
    let selected = if requested == "None" {
        "None".to_owned()
    } else {
        // Initial M1 single-stack range; a configurable CIDR remains follow-up work. .1 is reserved for the API
        // Service; .0 and .255.255 are the /16 network and broadcast addresses.
        let base = u32::from(Ipv4Addr::new(10, 43, 0, 0));
        if requested.is_empty() {
            (2..65535)
                .map(|offset| Ipv4Addr::from(base + offset))
                .find(|ip| !used.contains(ip))
                .ok_or_else(|| Failure::new(503, "ServiceUnavailable", "Service CIDR exhausted"))?
                .to_string()
        } else {
            let ip = requested
                .parse::<Ipv4Addr>()
                .map_err(|_| invalid("invalid ClusterIP"))?;
            let number = u32::from(ip);
            if !(base + 2..base + 65535).contains(&number) {
                return Err(invalid("ClusterIP outside allocatable 10.43.0.0/16 range"));
            }
            if used.contains(&ip) {
                return Err(invalid("ClusterIP already allocated"));
            }
            requested.to_owned()
        }
    };
    if spec["clusterIPs"]
        .as_array()
        .is_some_and(|v| v != &vec![json!(selected)])
    {
        return Err(invalid("clusterIPs must match clusterIP"));
    }
    spec["clusterIP"] = json!(selected);
    spec["clusterIPs"] = json!([selected]);
    spec["ipFamilies"] = json!(["IPv4"]);
    spec["ipFamilyPolicy"] = json!("SingleStack");
    if spec["type"] == "NodePort" {
        node_ports(spec, None, &mut node_ports_used)?;
    }
    Ok(())
}
