mod common;
use common::Server;
use serde_json::{json, Value};

#[tokio::test]
async fn node_discovery_is_read_only_and_survives_api_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::start_with_node_cidrs(dir.path(), "10.42.0.0/16", 24).await;
    let identity = s
        .pki
        .issue_client("system:node:worker", Some("system:nodes"))
        .unwrap();
    let service = json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"web"},"spec":{"ports":[{"port":80,"protocol":"TCP"},{"name":"dns","port":53,"protocol":"UDP"}]}});
    let mut service = service;
    service["spec"]["ports"][0]["name"] = json!("http");
    let (code, service) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/default/services",
            service,
        )
        .await;
    assert_eq!(code, 201, "{service}");
    let slice = json!({"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":"web","labels":{"kubernetes.io/service-name":"web"}},"addressType":"IPv4","ports":[{"name":"http","port":8080}],"endpoints":[{"addresses":["10.42.2.2"]}]});
    let (code, slice) = s
        .json(
            s.admin(),
            "POST",
            "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices",
            slice,
        )
        .await;
    assert_eq!(code, 201, "{slice}");
    for pass in 0..2 {
        for (collection, named, obj) in [
            (
                "/api/v1/services",
                "/api/v1/namespaces/default/services/web",
                &service,
            ),
            (
                "/apis/discovery.k8s.io/v1/endpointslices",
                "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices/web",
                &slice,
            ),
        ] {
            let tls = || s.pki.client_config(Some(&identity)).unwrap();
            let (code, list) = s.json(tls(), "GET", collection, json!({})).await;
            assert_eq!(code, 200);
            assert_eq!(list["items"].as_array().unwrap().len(), 1);
            assert_eq!(s.json(tls(), "GET", named, json!({})).await.0, 200);
            let watch = s
                .raw(
                    tls(),
                    "GET",
                    &format!("{collection}?watch=true&timeoutSeconds=1"),
                    json!({}),
                    &[],
                )
                .await;
            assert_eq!(watch.status(), 200);
            drop(watch);
            for verb in ["PUT", "DELETE"] {
                assert_eq!(s.json(tls(), verb, named, obj.clone()).await.0, 403);
            }
            let create_path = named.rsplit_once('/').unwrap().0;
            assert_eq!(s.json(tls(), "POST", create_path, obj.clone()).await.0, 403);
            assert_eq!(
                s.patch(
                    tls(),
                    named,
                    "application/merge-patch+json",
                    json!({"metadata":{"labels":{"attack":"true"}}})
                )
                .await
                .0,
                403
            );
        }
        if pass == 0 {
            assert_eq!(
                s.json(
                    s.pki.client_config(Some(&identity)).unwrap(),
                    "GET",
                    "/api/v1/secrets",
                    json!({})
                )
                .await
                .0,
                403
            );
            s = s.restart(dir.path()).await;
        }
    }
}
#[tokio::test]
async fn node_port_services_are_admitted_allocated_and_immutable() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let base = "/api/v1/namespaces/default/services";
    let create = |name: &str, spec: Value| json!({"apiVersion":"v1","kind":"Service","metadata":{"name":name},"spec":spec});
    // An explicit node port inside the published range is kept.
    let (code, stated) = s
        .json(
            s.admin(),
            "POST",
            base,
            create(
                "stated",
                json!({"type":"NodePort","ports":[{"name":"http","port":80,"nodePort":30080}]}),
            ),
        )
        .await;
    assert_eq!(code, 201, "{stated}");
    assert_eq!(stated["spec"]["externalTrafficPolicy"], "Cluster");
    assert_eq!(stated["spec"]["ports"][0]["nodePort"], 30080);
    assert_ne!(stated["spec"]["clusterIP"], json!("None"));
    // An unset node port is allocated in range and never repeats.
    let (code, allocated) = s
        .json(
            s.admin(),
            "POST",
            base,
            create(
                "allocated",
                json!({"type":"NodePort","ports":[{"name":"http","port":80}]}),
            ),
        )
        .await;
    assert_eq!(code, 201, "{allocated}");
    let node_port = allocated["spec"]["ports"][0]["nodePort"].as_i64().unwrap();
    assert!((30000..=32767).contains(&node_port), "{node_port}");
    assert_ne!(node_port, 30080);
    // An applied Service round-trips its allocation, and an update that omits
    // the node port keeps the stored one instead of reallocating.
    let mut applied = allocated.clone();
    applied["metadata"]["resourceVersion"] = allocated["metadata"]["resourceVersion"].clone();
    let (code, kept) = s
        .json(s.admin(), "PUT", &format!("{base}/allocated"), applied)
        .await;
    assert_eq!(code, 200, "{kept}");
    assert_eq!(kept["spec"]["ports"][0]["nodePort"], node_port);
    let mut omitted = kept.clone();
    omitted["spec"]["ports"] = json!([{"name":"http","port":80}]);
    let (code, kept) = s
        .json(s.admin(), "PUT", &format!("{base}/allocated"), omitted)
        .await;
    assert_eq!(code, 200, "{kept}");
    assert_eq!(kept["spec"]["ports"][0]["nodePort"], node_port);
    // A different node port on an existing Service is immutable.
    let mut changed = kept.clone();
    changed["spec"]["ports"] = json!([{"name":"http","port":80,"nodePort":30099}]);
    assert_eq!(
        s.json(s.admin(), "PUT", &format!("{base}/allocated"), changed)
            .await
            .0,
        422
    );
    let (code, load_balancer) = s
        .json(
            s.admin(),
            "POST",
            base,
            create(
                "loadbalancer",
                json!({"type":"LoadBalancer","ports":[{"name":"http","port":80}]}),
            ),
        )
        .await;
    assert_eq!(code, 201, "{load_balancer}");
    assert_eq!(load_balancer["spec"]["type"], "LoadBalancer");
    assert_ne!(load_balancer["spec"]["clusterIP"], json!("None"));
    for (name, spec) in [
        (
            "range",
            json!({"type":"NodePort","ports":[{"name":"http","port":80,"nodePort":29999}]}),
        ),
        (
            "taken",
            json!({"type":"NodePort","ports":[{"name":"http","port":80,"nodePort":30080}]}),
        ),
        (
            "local",
            json!({"type":"NodePort","externalTrafficPolicy":"Local","ports":[{"name":"http","port":80}]}),
        ),
        (
            "health",
            json!({"type":"NodePort","healthCheckNodePort":30090,"ports":[{"name":"http","port":80}]}),
        ),
        (
            "headless",
            json!({"type":"NodePort","clusterIP":"None","ports":[{"name":"http","port":80}]}),
        ),
        (
            "cluster",
            json!({"ports":[{"name":"http","port":80,"nodePort":30080}]}),
        ),
    ] {
        let (code, refusal) = s.json(s.admin(), "POST", base, create(name, spec)).await;
        assert_eq!(code, 422, "{name}: {refusal}");
        assert_eq!(
            s.json(s.admin(), "GET", &format!("{base}/{name}"), json!({}))
                .await
                .0,
            404,
            "{name} must not be persisted"
        );
    }
}
#[tokio::test]
async fn unsupported_forwarding_policies_are_rejected_instead_of_silently_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    for (name, extra) in [
        ("affinity", json!({"sessionAffinity":"ClientIP"})),
        ("external", json!({"externalIPs":["192.0.2.4"]})),
        ("distribution", json!({"trafficDistribution":"PreferClose"})),
        ("sctp", json!({"ports":[{"port":80,"protocol":"SCTP"}]})),
    ] {
        let mut service = json!({"apiVersion":"v1","kind":"Service","metadata":{"name":name},"spec":{"ports":[{"port":80}]}});
        service["spec"]
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let (code, result) = s
            .json(
                s.admin(),
                "POST",
                "/api/v1/namespaces/default/services",
                service,
            )
            .await;
        assert_eq!(code, 422, "{result}");
    }
}
