//! Real binary composition and crash recovery. No CRI is supplied: these tests
//! require truthful NotReady and do not claim container/network acceptance.
use k8s_openapi::api::{coordination::v1::Lease, core::v1::Node};
use kube::{api::ListParams, Api, Client};
use serde_json::Value;
use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::time::{sleep, timeout};

struct Process {
    child: Child,
    log: std::path::PathBuf,
}
impl Process {
    fn start(dir: &Path, args: &[String]) -> Self {
        let log = dir.join(format!(
            "process-{}.log",
            h3s_auth::bootstrap::random_secret().unwrap()
        ));
        let file = fs::File::create(&log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_h3s"))
            .args(args)
            .env_remove("H3S_TOKEN")
            .stdin(Stdio::null())
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap();
        Self { child, log }
    }
    fn assert_running(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "process exited: {}",
            fs::read_to_string(&self.log).unwrap()
        );
    }
    #[cfg(unix)]
    fn signal(&self, signal: &str) {
        let status = Command::new("kill")
            .args([signal, &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }
    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}
fn port() -> u16 {
    loop {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        if let Some(admin) = port.checked_add(1) {
            if TcpListener::bind(("127.0.0.1", admin)).is_ok() {
                return port;
            }
        }
    }
}
fn server_args(dir: &Path, api_port: u16, kubelet_port: u16) -> Vec<String> {
    [
        "server".into(),
        "--data-dir".into(),
        dir.join("runtime").display().to_string(),
        "--write-kubeconfig".into(),
        dir.join("admin.kubeconfig").display().to_string(),
        "--bind-address".into(),
        "127.0.0.1".into(),
        "--https-listen-port".into(),
        api_port.to_string(),
        "--observability-port".into(),
        "0".into(),
        "--node-name".into(),
        "server-node".into(),
        "--node-ip".into(),
        "192.0.2.10".into(),
        "--kubelet-port".into(),
        kubelet_port.to_string(),
    ]
    .into()
}

async fn admin_get(port: u16, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

async fn client(dir: &Path, process: &mut Process) -> Client {
    timeout(Duration::from_secs(30), async {
        loop {
            process.assert_running();
            if let Ok(config) = fs::read_to_string(dir.join("admin.kubeconfig")) {
                let client = h3s_controllers::client_from_kubeconfig(&config)
                    .await
                    .unwrap();
                if Api::<Node>::all(client.clone())
                    .list(&ListParams::default())
                    .await
                    .is_ok()
                {
                    return client;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("API startup timed out")
}
async fn node(client: &Client, process: &mut Process, name: &str, ip: &str) -> Node {
    timeout(Duration::from_secs(60), async {
        loop {
            process.assert_running();
            if let Ok(node) = Api::<Node>::all(client.clone()).get(name).await {
                let lease = Api::<Lease>::namespaced(client.clone(), "kube-node-lease")
                    .get(name)
                    .await;
                let cidr = node.spec.as_ref().and_then(|s| s.pod_cidr.as_ref());
                let status = node.status.as_ref();
                let ready = status
                    .and_then(|s| s.conditions.as_ref())
                    .is_some_and(|cs| {
                        cs.iter().any(|c| {
                            c.type_ == "Ready"
                                && c.status == "False"
                                && c.reason.as_deref() == Some("RuntimeNotReady")
                        })
                    });
                let address = status
                    .and_then(|s| s.addresses.as_ref())
                    .is_some_and(|a| a.iter().any(|a| a.type_ == "InternalIP" && a.address == ip));
                let owner = lease
                    .ok()
                    .and_then(|l| l.metadata.owner_references)
                    .is_some_and(|os| {
                        os.iter()
                            .any(|o| Some(&o.uid) == node.metadata.uid.as_ref())
                    });
                if cidr.is_some() && ready && address && owner {
                    return node;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("node registration/CIDR/Lease timed out")
}
async fn health(client: &Client, process: &mut Process, name: &str) {
    timeout(Duration::from_secs(60), async {
        loop {
            process.assert_running();
            let req = http::Request::get(format!("/api/v1/nodes/{name}/proxy/healthz"))
                .body(Vec::new())
                .unwrap();
            if client
                .request_text(req)
                .await
                .is_ok_and(|body| body == "ok\n")
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("supervisor-backed kubelet health timed out");
    let req = http::Request::get(format!("/api/v1/nodes/{name}/proxy/readyz"))
        .body(Vec::new())
        .unwrap();
    assert!(matches!(client.request_text(req).await, Err(kube::Error::Api(e)) if e.code == 503));
}

#[tokio::test]
async fn server_and_separate_worker_enroll_and_recover_with_retained_identities_and_cidrs() {
    let dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();
    let args = server_args(dir.path(), port(), port());
    let mut server = Process::start(dir.path(), &args);
    let client = client(dir.path(), &mut server).await;
    let original = node(&client, &mut server, "server-node", "192.0.2.10").await;
    health(&client, &mut server, "server-node").await;
    let identity_path = dir.path().join("runtime/agent/identity.json");
    let identity = fs::read(&identity_path).unwrap();
    let serving = fs::read(dir.path().join("runtime/agent/serving.json")).unwrap();
    let enrollment: Value = serde_json::from_slice(&identity).unwrap();
    let mut worker = Process::start(
        worker_dir.path(),
        &[
            "agent".into(),
            "--server".into(),
            enrollment["server"].as_str().unwrap().into(),
            "--token-file".into(),
            dir.path()
                .join("runtime/server/node-token")
                .display()
                .to_string(),
            "--data-dir".into(),
            worker_dir.path().join("runtime").display().to_string(),
            "--node-name".into(),
            "worker-node".into(),
            "--node-ip".into(),
            "192.0.2.11".into(),
            "--kubelet-port".into(),
            port().to_string(),
        ],
    );
    let original_worker = node(&client, &mut worker, "worker-node", "192.0.2.11").await;
    health(&client, &mut worker, "worker-node").await;
    assert_ne!(
        original.spec.as_ref().unwrap().pod_cidr,
        original_worker.spec.as_ref().unwrap().pod_cidr
    );
    let worker_identity_path = worker_dir.path().join("runtime/agent/identity.json");
    let worker_identity = fs::read(&worker_identity_path).unwrap();
    assert_eq!(
        fs::read(worker_dir.path().join("runtime/agent/server-ca.crt")).unwrap(),
        fs::read(dir.path().join("runtime/server/ca.crt")).unwrap(),
    );
    assert_ne!(identity, worker_identity);
    let ledger_request = || {
        http::Request::get(h3s_api::network::NODE_CIDR_PATH)
            .body(Vec::new())
            .unwrap()
    };
    let ledger: Value = client.request(ledger_request()).await.unwrap();
    assert_eq!(ledger["reservations"].as_object().unwrap().len(), 2);

    server.stop(); // Crash, then reopen the exact registry, PKI and local identity.
    server = Process::start(dir.path(), &args);
    let restarted = node(&client, &mut server, "server-node", "192.0.2.10").await;
    health(&client, &mut server, "server-node").await;
    health(&client, &mut worker, "worker-node").await;
    assert_eq!(original.metadata.uid, restarted.metadata.uid);
    assert_eq!(
        original.spec.as_ref().unwrap().pod_cidr,
        restarted.spec.as_ref().unwrap().pod_cidr
    );
    assert_eq!(identity, fs::read(&identity_path).unwrap());
    assert_eq!(
        serving,
        fs::read(dir.path().join("runtime/agent/serving.json")).unwrap()
    );
    assert_eq!(worker_identity, fs::read(&worker_identity_path).unwrap());
    assert_eq!(
        ledger,
        client.request::<Value>(ledger_request()).await.unwrap()
    );

    // A deleted local Node must be recreated by the embedded heartbeat, with
    // the name's durable subnet and a Lease referring to its new UID.
    Api::<Node>::all(client.clone())
        .delete("server-node", &Default::default())
        .await
        .unwrap();
    let replacement = node(&client, &mut server, "server-node", "192.0.2.10").await;
    assert_ne!(original.metadata.uid, replacement.metadata.uid);
    assert_eq!(
        original.spec.unwrap().pod_cidr,
        replacement.spec.unwrap().pod_cidr
    );
    assert_eq!(identity, fs::read(identity_path).unwrap());
}

#[tokio::test]
async fn disable_agent_serves_api_without_node_identity_or_local_listener() {
    let dir = tempfile::tempdir().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut args = server_args(dir.path(), port(), occupied.local_addr().unwrap().port());
    args.push("--disable-agent".into());
    // Local identity inputs are irrelevant when the local agent is disabled.
    let pos = args.iter().position(|v| v == "--node-name").unwrap();
    args[pos + 1] = "INVALID NAME".into();
    let mut server = Process::start(dir.path(), &args);
    let client = client(dir.path(), &mut server).await;
    assert!(Api::<Node>::all(client)
        .list(&ListParams::default())
        .await
        .unwrap()
        .items
        .is_empty());
    assert!(!dir.path().join("runtime/agent").exists());
    server.assert_running();
}

#[tokio::test]
async fn local_agent_failure_is_restarted_with_backoff_and_never_drops_the_api() {
    let dir = tempfile::tempdir().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let args = server_args(dir.path(), port(), occupied.local_addr().unwrap().port());
    let mut server = Process::start(dir.path(), &args);
    // The API serves while the local agent cannot bind its kubelet listener.
    let client = client(dir.path(), &mut server).await;
    let restarts = timeout(Duration::from_secs(40), async {
        loop {
            server.assert_running();
            let log = fs::read_to_string(&server.log).unwrap();
            let restarts = log
                .lines()
                .filter(|l| l.contains("h3s local agent: agent I/O") && l.contains("restarting in"))
                .count();
            if restarts >= 2 {
                return log;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("local agent was not restarted");
    assert!(restarts.contains("restarting in 1s"), "{restarts}");
    assert!(restarts.contains("restarting in 2s"), "{restarts}");
    let namespaces = Api::<k8s_openapi::api::core::v1::Namespace>::all(client.clone())
        .list(&ListParams::default())
        .await
        .unwrap();
    assert!(namespaces
        .items
        .iter()
        .any(|n| n.metadata.name.as_deref() == Some("kube-system")));
    server.assert_running();
    // Freeing the port lets the next restart succeed without any intervention.
    drop(occupied);
    let _ = node(&client, &mut server, "server-node", "192.0.2.10").await;
    health(&client, &mut server, "server-node").await;
}

#[tokio::test]
async fn invalid_secure_join_token_is_rejected_after_ca_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();
    let api_port = port();
    let mut args = server_args(dir.path(), api_port, port());
    args.push("--disable-agent".into());
    let mut server = Process::start(dir.path(), &args);
    let _client = client(dir.path(), &mut server).await;

    let server_token = fs::read_to_string(dir.path().join("runtime/server/node-token")).unwrap();
    let mut invalid = server_token.trim().as_bytes().to_vec();
    let last = invalid.last_mut().unwrap();
    *last = if *last == b'a' { b'b' } else { b'a' };
    let token_file = worker_dir.path().join("invalid-token");
    fs::write(&token_file, invalid).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut worker = Process::start(
        worker_dir.path(),
        &[
            "agent".into(),
            "--server".into(),
            format!("https://127.0.0.1:{api_port}"),
            "--token-file".into(),
            token_file.display().to_string(),
            "--data-dir".into(),
            worker_dir.path().join("runtime").display().to_string(),
            "--node-name".into(),
            "invalid-token-worker".into(),
            "--node-ip".into(),
            "192.0.2.12".into(),
        ],
    );
    let exit = timeout(Duration::from_secs(30), async {
        loop {
            if let Some(status) = worker.child.try_wait().unwrap() {
                break status;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("agent with invalid token did not exit");
    assert!(!exit.success());
    let log = fs::read_to_string(&worker.log).unwrap();
    assert!(log.contains("401"), "{log}");
    assert!(worker_dir
        .path()
        .join("runtime/agent/server-ca.crt")
        .is_file());
    server.assert_running();
}

#[tokio::test]
async fn memory_store_serves_without_creating_a_registry_database() {
    let dir = tempfile::tempdir().unwrap();
    let mut args = server_args(dir.path(), port(), port());
    args.extend(["--disable-agent".into(), "--store".into(), "memory".into()]);
    let mut server = Process::start(dir.path(), &args);
    let client = client(dir.path(), &mut server).await;
    assert!(Api::<Node>::all(client)
        .list(&ListParams::default())
        .await
        .is_ok());
    assert!(!dir.path().join("runtime/server/db/h3s.db").exists());
    server.assert_running();
}

#[tokio::test]
async fn json_logs_and_metrics_are_live_process_surfaces() {
    let dir = tempfile::tempdir().unwrap();
    let api_port = port();
    let mut args = server_args(dir.path(), api_port, port());
    args.extend([
        "--disable-agent".into(),
        "--log-format".into(),
        "json".into(),
    ]);
    let mut server = Process::start(dir.path(), &args);
    let _client = client(dir.path(), &mut server).await;
    let log = fs::read_to_string(&server.log).unwrap();
    let metrics = log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|entry| entry["fields"]["metrics"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("metrics address missing: {log}"));
    let port = metrics
        .strip_prefix("http://127.0.0.1:")
        .and_then(|value| value.strip_suffix("/metrics"))
        .unwrap_or_else(|| panic!("unexpected metrics address: {metrics}"))
        .parse()
        .unwrap();
    let response = admin_get(port, "/metrics").await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    for metric in [
        "h3s_apiserver_requests_total",
        "h3s_store_revision",
        "h3s_watchers",
        "h3s_scheduler_binds_total",
        "h3s_proxy_apply_total",
        "h3s_supervisor_restarts_total",
    ] {
        assert!(response.contains(metric), "missing {metric}: {response}");
    }
    assert!(
        log.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|entry| entry["fields"]["message"] == "h3s server listening"),
        "{log}"
    );
    server.assert_running();
}

#[cfg(unix)]
#[tokio::test]
async fn scheduler_restart_does_not_drop_the_api() {
    let dir = tempfile::tempdir().unwrap();
    let mut args = server_args(dir.path(), port(), port());
    args.push("--disable-agent".into());
    let mut server = Process::start(dir.path(), &args);
    let client = client(dir.path(), &mut server).await;

    sleep(Duration::from_millis(200)).await;
    server.signal("-USR1");
    timeout(Duration::from_secs(10), async {
        loop {
            server.assert_running();
            let log = fs::read_to_string(&server.log).unwrap();
            if log.contains("h3s scheduler stopped; restarting in 1s") {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("scheduler stop was not supervised");

    sleep(Duration::from_secs(2)).await;
    server.signal("-USR1");
    timeout(Duration::from_secs(10), async {
        loop {
            server.assert_running();
            let log = fs::read_to_string(&server.log).unwrap();
            if log.matches("h3s scheduler received SIGUSR1").count() >= 2 {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("scheduler did not restart");

    assert!(Api::<Node>::all(client)
        .list(&ListParams::default())
        .await
        .is_ok());
    server.assert_running();
}

#[tokio::test]
async fn second_server_on_the_same_registry_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let args = server_args(dir.path(), port(), port());
    let mut server = Process::start(dir.path(), &args);
    let client = client(dir.path(), &mut server).await;
    let mut second_args = server_args(dir.path(), port(), port());
    second_args.push("--disable-agent".into());
    let mut second = Process::start(dir.path(), &second_args);
    let exit = timeout(Duration::from_secs(40), async {
        loop {
            if let Some(exit) = second.child.try_wait().unwrap() {
                return exit;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("second server against the same registry kept running");
    assert!(!exit.success());
    let log = fs::read_to_string(&second.log).unwrap();
    assert!(log.contains("is locked by another h3s server"), "{log}");
    assert!(dir.path().join("runtime/server/db/.registry.lock").exists());
    // The first server is untouched and still serving.
    server.assert_running();
    assert!(Api::<Node>::all(client)
        .list(&ListParams::default())
        .await
        .is_ok());
}
