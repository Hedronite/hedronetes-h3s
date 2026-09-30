mod common;
use common::Server;
use h3s_controllers::{endpoint_gc_once, endpoints_once, ENDPOINT_CONTROLLER_ID};
use kube::Client;
use serde_json::{json, Value};
const SLICES: &str = "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices";
const SERVICES: &str = "/api/v1/namespaces/default/services";
const PODS: &str = "/api/v1/namespaces/default/pods";
async fn client(s: &Server) -> Client {
    let identity = s.pki.issue_client(ENDPOINT_CONTROLLER_ID, None).unwrap();
    h3s_controllers::client_from_kubeconfig(&s.pki.kubeconfig(&s.endpoint(), &identity).unwrap())
        .await
        .unwrap()
}
async fn request(s: &Server, method: &str, path: &str, value: Value) -> Value {
    let (code, value) = s.json(s.admin(), method, path, value).await;
    assert!(matches!(code, 200 | 201), "{method} {path}: {code} {value}");
    value
}
async fn service(s: &Server) -> Value {
    request(s,"POST",SERVICES,json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"web"},"spec":{"selector":{"app":"web"},"ports":[{"name":"http","port":80,"targetPort":"http"}]}})).await
}
/// A Ready Pod with caller-chosen labels and a named container port.
async fn labelled_pod(s: &Server, name: &str, labels: Value, port: (&str, u16), ip: &str) -> Value {
    let (port_name, port_number) = port;
    let mut p = request(
        s,
        "POST",
        PODS,
        json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name,"labels":labels},"spec":{"nodeName":"worker","automountServiceAccountToken":false,"enableServiceLinks":false,"dnsPolicy":"Default","securityContext":{"runAsNonRoot":true,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"web","image":"example.invalid/web:v1","ports":[{"name":port_name,"containerPort":port_number}],"securityContext":{"runAsUser":65534,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]}}),
    )
    .await;
    p["status"] = json!({"phase":"Running","podIP":ip,"podIPs":[{"ip":ip}],"conditions":[{"type":"Ready","status":"True"}]});
    request(s, "PUT", &format!("{PODS}/{name}/status"), p).await
}
async fn pod(s: &Server, name: &str, ip: &str, port: u16, ready: bool) -> Value {
    let mut p=request(s,"POST",PODS,json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name,"labels":{"app":"web"}},"spec":{"nodeName":"worker","automountServiceAccountToken":false,"enableServiceLinks":false,"dnsPolicy":"Default","securityContext":{"runAsNonRoot":true,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"web","image":"example.invalid/web:v1","ports":[{"name":"http","containerPort":port}],"securityContext":{"runAsUser":65534,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]}})).await;
    p["status"] = json!({"phase":"Running","podIP":ip,"podIPs":[{"ip":ip}],"conditions":[{"type":"Ready","status":if ready {"True"} else {"False"}}]});
    request(s, "PUT", &format!("{PODS}/{name}/status"), p).await
}
async fn slices(s: &Server) -> Vec<Value> {
    request(s, "GET", SLICES, json!({})).await["items"]
        .as_array()
        .unwrap()
        .clone()
}
async fn managed(s: &Server) -> Vec<Value> {
    slices(s)
        .await
        .into_iter()
        .filter(|s| {
            s["metadata"]["labels"]["endpointslice.kubernetes.io/managed-by"]
                == "hedronetes.io/endpointslice-controller"
        })
        .collect()
}
async fn tick(client: &Client) {
    endpoints_once(client.clone(), "default", "web")
        .await
        .unwrap();
    endpoint_gc_once(client.clone()).await.unwrap();
}
async fn remove(s: &Server, base: &str, object: &Value) {
    request(
        s,
        "DELETE",
        &format!("{base}/{}", object["metadata"]["name"].as_str().unwrap()),
        json!({"preconditions":{"uid":object["metadata"]["uid"]},"propagationPolicy":"Background"}),
    )
    .await;
}
#[tokio::test]
async fn endpoint_controller_tracks_real_api_selection_readiness_ports_restart_and_service_uid() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let mut svc = service(&s).await;
    let p1 = pod(&s, "one", "10.42.2.11", 8080, true).await;
    let mut p2 = pod(&s, "two", "10.42.2.12", 9090, false).await;
    // Another manager may share the Service label; it must remain untouched.
    let manual=request(&s,"POST",SLICES,json!({"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":"external-manager","labels":{"kubernetes.io/service-name":"web","endpointslice.kubernetes.io/managed-by":"example.invalid/other"}},"addressType":"IPv4","ports":[{"name":"http","port":7070}],"endpoints":[{"addresses":["10.42.2.99"]}]})).await;
    let c = client(&s).await;
    tick(&c).await;
    let first = managed(&s).await;
    assert_eq!(first.len(), 2);
    assert!(first
        .iter()
        .all(|e| e["metadata"]["ownerReferences"][0]["uid"] == svc["metadata"]["uid"]));
    let one = first
        .iter()
        .find(|e| e["ports"][0]["port"] == 8080)
        .unwrap();
    let two = first
        .iter()
        .find(|e| e["ports"][0]["port"] == 9090)
        .unwrap();
    assert_eq!(
        one["endpoints"][0]["targetRef"]["uid"],
        p1["metadata"]["uid"]
    );
    assert_eq!(one["endpoints"][0]["addresses"], json!(["10.42.2.11"]));
    assert_eq!(one["endpoints"][0]["conditions"]["ready"], true);
    assert_eq!(two["endpoints"][0]["conditions"]["ready"], false);
    tick(&c).await;
    assert_eq!(
        managed(&s).await,
        first,
        "unchanged pass must not churn resource versions"
    );
    p2["status"]["conditions"][0]["status"] = json!("True");
    p2 = request(&s, "PUT", &format!("{PODS}/two/status"), p2).await;
    tick(&c).await;
    let second = managed(&s).await;
    assert!(second
        .iter()
        .all(|e| e["endpoints"][0]["conditions"]["ready"] == true));
    assert_eq!(
        second
            .iter()
            .find(|e| e["ports"][0]["port"] == 8080)
            .unwrap()["metadata"]["resourceVersion"],
        one["metadata"]["resourceVersion"]
    );
    p2["metadata"]["labels"]["app"] = json!("other");
    request(&s, "PUT", &format!("{PODS}/two"), p2).await;
    tick(&c).await;
    let selected = managed(&s).await;
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0]["ports"][0]["port"], 8080);
    svc["spec"]["ports"][0]["targetPort"] = json!(8181);
    svc = request(&s, "PUT", &format!("{SERVICES}/web"), svc).await;
    tick(&c).await;
    let changed = managed(&s).await;
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["ports"][0]["port"], 8181);
    let old_uid = changed[0]["metadata"]["uid"].clone();
    let s = s.restart(dir.path()).await;
    let c = client(&s).await;
    tick(&c).await;
    assert_eq!(managed(&s).await, changed);
    remove(&s, PODS, &p1).await;
    tick(&c).await;
    assert_eq!(managed(&s).await[0]["endpoints"], json!([]));
    remove(&s, SERVICES, &svc).await;
    let replacement = service(&s).await;
    assert_ne!(replacement["metadata"]["uid"], svc["metadata"]["uid"]);
    tick(&c).await;
    let replaced = managed(&s).await;
    assert_eq!(replaced.len(), 1);
    assert_ne!(replaced[0]["metadata"]["uid"], old_uid);
    assert_eq!(
        replaced[0]["metadata"]["ownerReferences"][0]["uid"],
        replacement["metadata"]["uid"]
    );
    remove(&s, SERVICES, &replacement).await;
    assert_eq!(endpoint_gc_once(c).await.unwrap(), 1);
    assert!(managed(&s).await.is_empty());
    assert_eq!(slices(&s).await, vec![manual]);
}
#[tokio::test]
async fn endpoint_controller_identity_cannot_mutate_inputs_or_read_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let svc = service(&s).await;
    let p = pod(&s, "one", "10.42.2.11", 8080, true).await;
    let identity = s.pki.issue_client(ENDPOINT_CONTROLLER_ID, None).unwrap();
    let http = s.pki.client_config(Some(&identity)).unwrap();
    for (method, path, body) in [
        ("GET", "/api/v1/namespaces/default/secrets", json!({})),
        (
            "PUT",
            "/api/v1/namespaces/default/services/web",
            svc.clone(),
        ),
        (
            "DELETE",
            "/api/v1/namespaces/default/services/web",
            json!({}),
        ),
        ("PUT", "/api/v1/namespaces/default/pods/one/status", p),
        (
            "POST",
            "/api/v1/nodes",
            json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"forbidden"}}),
        ),
        (
            "POST",
            "/apis/rbac.authorization.k8s.io/v1/clusterroles",
            json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":"forbidden"}}),
        ),
    ] {
        let (code, _) = s.json(http.clone(), method, path, body).await;
        assert_eq!(code, 403, "{method} {path}");
    }
    let c = client(&s).await;
    tick(&c).await;
    assert_eq!(managed(&s).await.len(), 1);
    // Removing the selector collects only our previously owned automatic slice.
    let mut svc = svc;
    svc["spec"]["selector"] = json!({});
    request(&s, "PUT", &format!("{SERVICES}/web"), svc).await;
    tick(&c).await;
    assert!(managed(&s).await.is_empty());
}
/// KP-21 live reproduction: the NodePort Service QA applied against the Ready
/// CoreDNS Pod produced no EndpointSlice at all.
/// The live path: the running controller watches Services, rather than being
/// called once by hand.
#[tokio::test]
async fn running_controller_publishes_a_slice_for_a_new_node_port_service() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let client = client(&s).await;
    let running = tokio::spawn(h3s_controllers::run_endpoint_controller(client));
    request(
        &s,
        "POST",
        SERVICES,
        json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"kp21-live"},"spec":{
            "type":"NodePort",
            "selector":{"hedronetes.io/component":"coredns-m1"},
            "ports":[{"name":"ready","port":8181,"targetPort":"ready","nodePort":30081}]}}),
    )
    .await;
    labelled_pod(
        &s,
        "coredns-m1",
        json!({"hedronetes.io/component":"coredns-m1"}),
        ("ready", 8181),
        "10.42.0.9",
    )
    .await;
    let published = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let found = managed(&s).await;
            if !found.is_empty() {
                return found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;
    running.abort();
    assert!(
        published.is_ok(),
        "the running endpoint controller never published a slice for the NodePort Service"
    );
}

#[tokio::test]
async fn node_port_service_publishes_the_slice_it_dnat_needs() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let client = client(&s).await;
    let service = request(
        &s,
        "POST",
        SERVICES,
        json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"kp21-coredns-nodeport"},"spec":{
            "type":"NodePort",
            "selector":{"hedronetes.io/component":"coredns-m1"},
            "ports":[{"name":"ready","port":8181,"targetPort":"ready","nodePort":30081}]}}),
    )
    .await;
    assert_eq!(service["spec"]["ports"][0]["nodePort"], 30081);
    labelled_pod(
        &s,
        "coredns-m1",
        json!({"hedronetes.io/component":"coredns-m1"}),
        ("ready", 8181),
        "10.42.0.9",
    )
    .await;
    endpoints_once(client.clone(), "default", "kp21-coredns-nodeport")
        .await
        .unwrap();
    let published = managed(&s).await;
    assert_eq!(
        published
            .iter()
            .map(|sl| sl["metadata"]["labels"]["kubernetes.io/service-name"]
                .as_str()
                .unwrap()
                .to_owned())
            .collect::<Vec<_>>(),
        vec!["kp21-coredns-nodeport".to_owned()],
        "{published:?}"
    );
    // The slice must carry the resolved target port the node port DNATs to,
    // not the published node port.
    let slice = &published[0];
    assert_eq!(slice["ports"][0]["name"], "ready");
    assert_eq!(slice["ports"][0]["port"], json!(8181));
    let endpoints = slice["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 1, "{slice}");
    assert_eq!(endpoints[0]["addresses"], json!(["10.42.0.9"]));
    assert_eq!(endpoints[0]["conditions"]["ready"], json!(true));
}
