//! Geode custody for Secret payloads: sealed rows carry no plaintext, and
//! legacy plaintext rows stay readable when custody is configured.

use h3s_storage::*;
use std::path::PathBuf;

fn geode_binary() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("GEODE_BIN") {
        let path = PathBuf::from(explicit);
        if path.exists() {
            return Some(path);
        }
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join("geode"))
        .find(|path| path.is_file())
}

fn secret(name: &str, value: &str) -> StoredObject {
    StoredObject {
        key: StoreKey::new(format!("/registry/secrets/default/{name}")).unwrap(),
        value: value.as_bytes().to_vec(),
        revision: 0,
    }
}

fn configmap(name: &str, value: &str) -> StoredObject {
    StoredObject {
        key: StoreKey::new(format!("/registry/configmaps/default/{name}")).unwrap(),
        value: value.as_bytes().to_vec(),
        revision: 0,
    }
}

/// Committed bytes as `sqlite3` would read them: the database file plus any
/// WAL frames still waiting for a checkpoint. All store connections must be
/// closed first; SQLite folds the WAL on a clean close.
fn committed_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut bytes = std::fs::read(dir.join("h3s.db")).unwrap();
    if let Ok(extra) = std::fs::read(dir.join("h3s.db-wal")) {
        bytes.extend_from_slice(&extra);
    }
    bytes
}

#[tokio::test]
async fn sealed_secret_leaves_no_plaintext_in_the_database() {
    let Some(binary) = geode_binary() else {
        eprintln!("geode binary not found; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("h3s.db");
    let sealer = GeodeSealer::create(&binary, dir.path()).unwrap();
    let store = SqliteStore::open(&db)
        .await
        .unwrap()
        .with_secrets_sealer(sealer);
    let created = store.create(secret("db", "geode-canary")).await.unwrap();
    let read_back = store.get(&created.key).await.unwrap().expect("present");
    assert_eq!(read_back.value, b"geode-canary");
    let list = store
        .list(ListSelect::new("/registry/secrets/default/"))
        .await
        .unwrap();
    assert_eq!(list.items[0].value, b"geode-canary");
    let config = store.create(configmap("cm", "plain-value")).await.unwrap();
    assert_eq!(
        store.get(&config.key).await.unwrap().unwrap().value,
        b"plain-value"
    );
    drop(store);

    let bytes = committed_bytes(dir.path());
    assert!(
        !bytes.windows(11).any(|w| w == b"geode-canary"),
        "plaintext secret found in the database file"
    );
    assert!(
        bytes.windows(8).any(|w| w == b"H3SGEO1:"),
        "sealed rows must keep their envelope marker"
    );
    assert!(
        bytes.windows(11).any(|w| w == b"plain-value"),
        "non-secret payloads must not be sealed"
    );
}

#[tokio::test]
async fn sealed_rows_survive_reopen_and_legacy_plaintext_stays_readable() {
    let Some(binary) = geode_binary() else {
        eprintln!("geode binary not found; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("h3s.db");
    let sealer = GeodeSealer::create(&binary, dir.path()).unwrap();
    let store = SqliteStore::open(&db)
        .await
        .unwrap()
        .with_secrets_sealer(sealer);
    let sealed = store.create(secret("kept", "second-read")).await.unwrap();
    assert_eq!(
        store.get(&sealed.key).await.unwrap().unwrap().value,
        b"second-read"
    );
    drop(store);

    let legacy = SqliteStore::open(&db).await.unwrap();
    let plain = legacy
        .create(StoredObject {
            key: StoreKey::new("/registry/secrets/default/legacy").unwrap(),
            value: b"legacy-plaintext".to_vec(),
            revision: 0,
        })
        .await
        .unwrap();
    drop(legacy);

    let reopened = SqliteStore::open(&db)
        .await
        .unwrap()
        .with_secrets_sealer(GeodeSealer::create(&binary, dir.path()).unwrap());
    assert_eq!(
        reopened.get(&sealed.key).await.unwrap().unwrap().value,
        b"second-read"
    );
    assert_eq!(
        reopened.get(&plain.key).await.unwrap().unwrap().value,
        b"legacy-plaintext"
    );
    drop(reopened);

    let bytes = committed_bytes(dir.path());
    assert!(
        !bytes.windows(11).any(|w| w == b"second-read"),
        "sealed secret leaked plaintext to the database file"
    );
    assert!(
        bytes.windows(16).any(|w| w == b"legacy-plaintext"),
        "plaintext writes must stay unsealed so existing databases keep working"
    );
}
