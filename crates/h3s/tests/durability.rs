//! Tower-independent durability contract: rotate a live SQLite server token,
//! reject the previous token after restart, and restore an offline backup.
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    api::{Api, PostParams},
    Client,
};
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
        let log = dir.join(format!("{}.log", args[0]));
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
            "{}",
            fs::read_to_string(&self.log).unwrap()
        );
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
fn server_args(dir: &Path, port: u16) -> Vec<String> {
    vec![
        "server".into(),
        "--data-dir".into(),
        dir.join("runtime").display().to_string(),
        "--write-kubeconfig".into(),
        dir.join("admin.kubeconfig").display().to_string(),
        "--bind-address".into(),
        "127.0.0.1".into(),
        "--https-listen-port".into(),
        port.to_string(),
        "--observability-port".into(),
        "0".into(),
        "--disable-agent".into(),
    ]
}
async fn client(dir: &Path, process: &mut Process) -> Client {
    timeout(Duration::from_secs(30), async {
        loop {
            process.assert_running();
            if let Ok(config) = fs::read_to_string(dir.join("admin.kubeconfig")) {
                if let Ok(client) = h3s_controllers::client_from_kubeconfig(&config).await {
                    if Api::<ConfigMap>::namespaced(client.clone(), "default")
                        .list(&Default::default())
                        .await
                        .is_ok()
                    {
                        return client;
                    }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("server startup timeout")
}
fn command(args: &[String]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_h3s"))
        .args(args)
        .output()
        .unwrap()
}

#[tokio::test]
async fn rotate_rejects_old_token_and_restore_recovers_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let port = port();
    let mut server = Process::start(dir.path(), &server_args(dir.path(), port));
    let api_client = client(dir.path(), &mut server).await;
    let maps = Api::<ConfigMap>::namespaced(api_client, "default");
    maps.create(
        &PostParams::default(),
        &ConfigMap {
            metadata: kube::api::ObjectMeta {
                name: Some("durability".into()),
                ..Default::default()
            },
            data: Some([("state".into(), "kept".into())].into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let runtime = dir.path().join("runtime");
    let backup = dir.path().join("backup");
    let busy_backup = command(&vec![
        "backup".into(),
        "--data-dir".into(),
        runtime.display().to_string(),
        "--output".into(),
        backup.display().to_string(),
    ]);
    assert!(!busy_backup.status.success());
    server.stop();
    let backup_out = command(&vec![
        "backup".into(),
        "--data-dir".into(),
        runtime.display().to_string(),
        "--output".into(),
        backup.display().to_string(),
    ]);
    assert!(
        backup_out.status.success(),
        "{}",
        String::from_utf8_lossy(&backup_out.stderr)
    );
    let old = fs::read_to_string(runtime.join("server/node-token")).unwrap();
    let rotate = command(&vec![
        "token".into(),
        "rotate".into(),
        "--data-dir".into(),
        runtime.display().to_string(),
    ]);
    assert!(
        rotate.status.success(),
        "{}",
        String::from_utf8_lossy(&rotate.stderr)
    );
    let new = String::from_utf8(rotate.stdout).unwrap().trim().to_owned();
    assert_ne!(old.trim(), new);
    assert!(h3s_auth::bootstrap::valid_token(&new));
    for member in ["h3s.db", "h3s.db-wal", "h3s.db-shm"] {
        let _ = fs::remove_file(runtime.join("server/db").join(member));
    }
    let restore = command(&vec![
        "restore".into(),
        "--data-dir".into(),
        runtime.display().to_string(),
        "--from".into(),
        backup.display().to_string(),
    ]);
    assert!(
        restore.status.success(),
        "{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    let mut server = Process::start(dir.path(), &server_args(dir.path(), port));
    let api_client = client(dir.path(), &mut server).await;
    assert_eq!(
        Api::<ConfigMap>::namespaced(api_client, "default")
            .get("durability")
            .await
            .unwrap()
            .data
            .unwrap()["state"],
        "kept"
    );
    let old_agent = Command::new(env!("CARGO_BIN_EXE_h3s"))
        .args([
            "agent",
            "--server",
            &format!("https://127.0.0.1:{port}"),
            "--token",
            old.trim(),
            "--node-name",
            "old-token",
            "--node-ip",
            "192.0.2.77",
            "--data-dir",
            &dir.path().join("old-agent").display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(!old_agent.status.success());
}
