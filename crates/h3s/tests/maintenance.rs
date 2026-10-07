//! Startup maintenance contract: `run_server` awaits `maintain(1024)` after the
//! store opens and before the API serves, so a seeded SQLite registry comes up
//! with its MVCC floor already moved. The 60 second interval is not slept on.
use h3s_storage::{ListSelect, SqliteStore, Storage, StoreKey, StoredObject};
use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{sleep, timeout},
};

const SEEDED_HEAD: u64 = 1030;

struct Server {
    child: Child,
    log: std::path::PathBuf,
}
impl Server {
    fn start(dir: &Path, port: u16, admin: u16) -> Self {
        let log = dir.join("server.log");
        let file = fs::File::create(&log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_h3s"))
            .args([
                "server",
                "--data-dir",
                &dir.join("runtime").display().to_string(),
                "--write-kubeconfig",
                &dir.join("admin.kubeconfig").display().to_string(),
                "--bind-address",
                "127.0.0.1",
                "--https-listen-port",
                &port.to_string(),
                "--observability-port",
                &admin.to_string(),
                "--disable-agent",
            ])
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
}
impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Two adjacent free ports, neither of them 6443.
fn ports() -> (u16, u16) {
    loop {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        if port == 6443 {
            continue;
        }
        if let Some(admin) = port.checked_add(1) {
            if admin != 6443 && TcpListener::bind(("127.0.0.1", admin)).is_ok() {
                return (port, admin);
            }
        }
    }
}

/// Raise a fresh SQLite registry to `SEEDED_HEAD` through one key's history,
/// where the server expects it: `<data-dir>/server/db/h3s.db`, mode 0700.
async fn seed(runtime: &Path) {
    let db_dir = runtime.join("server/db");
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&db_dir).unwrap();
    let store = SqliteStore::open(db_dir.join("h3s.db")).await.unwrap();
    let key = StoreKey::new("/registry/configmaps/default/gc-seed").unwrap();
    let object = |n: u64| StoredObject {
        key: key.clone(),
        value: serde_json::to_vec(&serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "gc-seed", "namespace": "default"},
            "data": {"n": n.to_string()},
        }))
        .unwrap(),
        revision: 0,
    };
    let mut head = store.create(object(0)).await.unwrap().revision;
    for n in 1..SEEDED_HEAD {
        head = store.update(object(n), head).await.unwrap().revision;
    }
    assert!(head >= SEEDED_HEAD, "seed head {head}");
    // Seeded history is still whole: nothing has compacted it yet.
    let mut at_one = ListSelect::new("/registry/configmaps/default/");
    at_one.at_revision = Some(1);
    store.list(at_one).await.expect("revision 1 is readable");
}

async fn get(admin: u16, path: &str) -> Option<String> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", admin))
        .await
        .ok()?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await.ok()?;
    Some(response)
}

#[tokio::test]
async fn startup_maintain_compacts_seeded_sqlite_before_serving() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = dir.path().join("runtime");
    seed(&runtime).await;
    let (port, admin) = ports();
    let mut server = Server::start(dir.path(), port, admin);

    timeout(Duration::from_secs(30), async {
        loop {
            server.assert_running();
            if get(admin, "/readyz")
                .await
                .is_some_and(|r| r.starts_with("HTTP/1.1 200") && r.ends_with("ok\n"))
            {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("/readyz never became ok");

    let kubeconfig = fs::read_to_string(dir.path().join("admin.kubeconfig")).unwrap();
    let client = h3s_controllers::client_from_kubeconfig(&kubeconfig)
        .await
        .unwrap();
    let list = |rv: u64| {
        let request = http::Request::get(format!(
            "/api/v1/namespaces/default/configmaps?resourceVersion={rv}"
        ))
        .body(Vec::new())
        .unwrap();
        let client = client.clone();
        async move { client.request_text(request).await }
    };
    // The floor is head - 1024, well above 1, so revision 1 is gone.
    match list(1).await {
        Err(kube::Error::Api(status)) => assert_eq!(status.code, 410, "{status:?}"),
        other => panic!("resourceVersion=1 should be 410 Expired, got {other:?}"),
    }
    // The current snapshot is untouched: compaction never moves to the head.
    let current = list(0).await.expect("current list");
    assert!(current.contains("gc-seed"), "{current}");
    server.assert_running();
}
